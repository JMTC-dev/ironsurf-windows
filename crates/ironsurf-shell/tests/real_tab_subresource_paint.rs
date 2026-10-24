//! The real tab — sandboxed, as the shell always starts it — paints a page
//! whose look arrives over HTTP *after* the document: an external stylesheet
//! (an orange body) served late from a loopback server.
//!
//! Guards two things end to end, by the pixels the compositor ends up with:
//! the tab sandbox leaves the paint path intact, and a subresource landing
//! after the first frame still repaints the page. (Blitz's `request_redraw`
//! used to only wake the tab loop: the loop applied the stylesheet but never
//! painted it, so the page stayed unstyled until the next mouse move.)

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use ironsurf_shell::app::App;
use ironsurf_shell::AppWindow;

/// The workspace-built `ironsurf` binary (see `real_tab.rs`).
fn real_tab_bin() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let profile_dir = exe.parent()?.parent()?;
    ["ironsurf", "ironsurf-tab"]
        .iter()
        .map(|n| profile_dir.join(format!("{n}{}", std::env::consts::EXE_SUFFIX)))
        .find(|c| c.is_file())
}

/// Serve `/` (a page linking `/style.css`) and the stylesheet — held back
/// long enough that it surely lands after the first frame — until the test
/// process ends.
fn serve() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            std::thread::spawn(move || {
                let mut buf = [0u8; 4096];
                let n = stream.read(&mut buf).unwrap_or(0);
                let request = String::from_utf8_lossy(&buf[..n]);
                let (content_type, body) = if request.starts_with("GET /style.css") {
                    std::thread::sleep(Duration::from_millis(400));
                    ("text/css", "html, body { margin: 0; height: 100%; background: #ff8000; }")
                } else {
                    (
                        "text/html",
                        "<!doctype html><html><head><title>Orange</title>\
                         <link rel=stylesheet href=/style.css></head><body></body></html>",
                    )
                };
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\
                     Connection: close\r\n\r\n{body}",
                    body.len()
                );
            });
        }
    });
    format!("http://{addr}/")
}

/// The compositor's current page pixel at the frame's centre, if any frame.
fn centre_pixel(ui: &AppWindow) -> Option<(u8, u8, u8)> {
    let buffer = ui.get_page_image().to_rgba8()?;
    let (w, h) = (buffer.width() as usize, buffer.height() as usize);
    if w < 16 || h < 16 {
        return None;
    }
    let px = buffer.as_slice()[(h / 2) * w + w / 2];
    Some((px.r, px.g, px.b))
}

#[test]
fn a_late_stylesheet_repaints_the_sandboxed_tab() {
    let Some(tab_bin) = real_tab_bin() else {
        eprintln!("skip: ironsurf not built (cargo build --workspace first)");
        return;
    };
    i_slint_backend_testing::init_no_event_loop();
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("IRONSURF_DATA_DIR", dir.path());

    let url = serve();
    let ui = AppWindow::new().expect("window on testing backend");
    let app = App::new(&ui, tab_bin, Vec::new()).expect("app core");
    app.start(Some(&url));

    let deadline = Instant::now() + Duration::from_secs(30);
    let mut last = None;
    loop {
        app.core().drain();
        last = centre_pixel(&ui).or(last);
        if let Some((r, g, b)) = last {
            if r > 230 && (100..=160).contains(&g) && b < 40 {
                break; // #ff8000 — the stylesheet was applied and painted
            }
        }
        assert!(
            Instant::now() < deadline,
            "the page never painted orange; last centre pixel {last:?}; tabs: {:?}",
            app.core().tab_snapshot()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    app.shutdown();
}
