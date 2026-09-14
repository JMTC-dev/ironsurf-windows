//! fake_tab — a scripted stand-in for `ironsurf-tab` used by the shell's
//! headless pump tests (and as an escape hatch when the engine build is not
//! available). Speaks the same framing: `FromTab` on stdout, `ToTab` on
//! stdin; exits 0 on `Destroy` or EOF.

use std::io::BufReader;

use ironsurf_common::{
    read_frame, write_frame, ContainerId, FromTab, TabId, ToTab,
};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut tab_id = TabId(0);
    let mut _container = ContainerId(1);
    let mut url = "about:blank".to_string();
    let mut size = (400u32, 300u32);

    let mut it = args.iter().skip(1);
    while let Some(arg) = it.next() {
        let mut value = |what: &str| -> String {
            it.next()
                .unwrap_or_else(|| panic!("--{what} requires a value"))
                .clone()
        };
        match arg.as_str() {
            "--tab-id" => tab_id = TabId(value("tab-id").parse().unwrap()),
            "--container-id" => _container = ContainerId(value("container-id").parse().unwrap()),
            "--profile-dir" => {
                value("profile-dir");
            }
            "--url" => url = value("url"),
            "--size" => {
                let v = value("size");
                let (w, h) = v.split_once('x').unwrap_or(("400", "300"));
                size = (w.parse().unwrap_or(400), h.parse().unwrap_or(300));
            }
            _ => {} // tolerate the real tab's flags
        }
    }

    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let mut emit = |msg: FromTab| {
        let _ = write_frame(&mut out, &msg);
    };

    emit(FromTab::Ready { tab_id });
    announce(&mut emit, &url, false);
    emit(frame(size)); // like the real tab: a first frame without any Resize

    let stdin = std::io::stdin();
    let mut reader = BufReader::new(stdin.lock());
    loop {
        let msg = match read_frame::<_, ToTab>(&mut reader) {
            Ok(msg) => msg,
            Err(_) => break, // EOF or corrupt: clean death
        };
        match msg {
            ToTab::Navigate { url: new } => {
                url = new;
                announce(&mut emit, &url, true);
            }
            ToTab::Reload => announce(&mut emit, &url, true),
            ToTab::Resize { width, height, .. } => {
                size = (width.clamp(16, 4096), height.clamp(16, 4096));
                emit(frame(size));
            }
            ToTab::Back | ToTab::Forward | ToTab::Stop => {
                emit(FromTab::HistoryChanged { can_go_back: false, can_go_forward: false });
            }
            ToTab::Pointer(_) | ToTab::Key(_) | ToTab::Scroll(_) | ToTab::SetVisible { .. } => {}
            ToTab::SetZoom { .. } => {} // no real layout to rescale
            ToTab::Destroy => break,
        }
    }
    std::process::exit(0);
}

fn announce(emit: &mut impl FnMut(FromTab), url: &str, with_nav_history: bool) {
    emit(FromTab::LoadingChanged { loading: true });
    emit(FromTab::UrlChanged { url: Some(url.to_string()) });
    emit(FromTab::TitleChanged { title: Some(format!("Fake — {url}")) });
    emit(FromTab::HistoryChanged {
        can_go_back: with_nav_history,
        can_go_forward: false,
    });
    emit(FromTab::LoadingChanged { loading: false });
}

fn frame(size: (u32, u32)) -> FromTab {
    let (w, h) = (size.0.min(1024) as usize, size.1.min(1024) as usize);
    let mut rgba = Vec::with_capacity(w * h * 4);
    for y in 0..h {
        for x in 0..w {
            rgba.push((x % 251) as u8);
            rgba.push((y % 253) as u8);
            rgba.push(128);
            rgba.push(255);
        }
    }
    FromTab::Frame { width: w as u32, height: h as u32, scale: 1.0, rgba }
}
