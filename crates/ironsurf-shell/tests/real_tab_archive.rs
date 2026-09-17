//! Archive mode against the **real** `ironsurf-tab` binary (task: archive
//! mode), served by a tiny localhost HTTP server built on `std::net` alone —
//! no engine fixtures, no external network (localhost is inside the CI
//! contract). The shell switches archive mode on, spawns a tab (born with
//! `--archive-mode`), navigates to `http://127.0.0.1:<port>/`, and the
//! archive tree must appear under the configured root:
//!
//! ```text
//! <root>/<ISO8601>/127.0.0.1:<port>/index.html      (the document)
//! <root>/<ISO8601>/127.0.0.1:<port>/style.css       (a <link> subresource)
//! <root>/<ISO8601>/127.0.0.1:<port>/html/?q=asdf    (a query URL: the query
//!                                                   names the file)
//! ```
//!
//! Skipped (like `tests/real_tab.rs`) when the tab binary is not built.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use ironsurf_shell::app::App;
use ironsurf_shell::AppWindow;

fn real_tab_bin() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let profile_dir = exe.parent()?.parent()?; // target/<profile>
    let candidate = profile_dir.join(format!("ironsurf-tab{}", std::env::consts::EXE_SUFFIX));
    candidate.is_file().then_some(candidate)
}

const PAGE: &str = "<html><head><title>Archive fixture</title>\
         <link rel=\"stylesheet\" href=\"/style.css\"></head>\
         <body><h1>archive me</h1></body></html>";
const CSS: &str = "body { color: #123456; }";

fn serve(mut stream: TcpStream) {
    let mut buf = [0u8; 1024];
    let n = stream.read(&mut buf).unwrap_or(0);
    let head = String::from_utf8_lossy(&buf[..n]).into_owned();
    let path = head
        .split_whitespace()
        .nth(1)
        .unwrap_or("/")
        .to_string();
    let (ctype, body): (&str, &[u8]) = match path.as_str() {
        "/style.css" => ("text/css", CSS.as_bytes()),
        _ => ("text/html; charset=utf-8", PAGE.as_bytes()),
    };
    let resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(resp.as_bytes());
    let _ = stream.write_all(body);
    let _ = stream.flush();
}

#[test]
fn real_tab_archives_a_site_into_the_tree() {
    let Some(tab_bin) = real_tab_bin() else {
        eprintln!("skip: ironsurf-tab not built (cargo build --workspace first)");
        return;
    };
    i_slint_backend_testing::init_no_event_loop();

    // localhost server on an ephemeral port.
    let listener = TcpListener::bind("127.0.0.1:0").expect("loopback server");
    let port = listener.local_addr().unwrap().port();
    thread::spawn(move || {
        for stream in listener.incoming() {
            match stream {
                Ok(s) => serve(s),
                Err(_) => break,
            }
        }
    });
    let site = format!("http://127.0.0.1:{port}/");

    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("IRONSURF_DATA_DIR", dir.path());
    let root = dir.path().join("arch-root");

    let ui = AppWindow::new().expect("window on testing backend");
    let app = App::new(&ui, tab_bin, Vec::new()).expect("app core");
    let core = app.core();
    let snap = || core.tab_snapshot();

    // Folder + switch through the real menu plumbing, THEN start the shell:
    // the first tab is born while the mode is on and archives from its very
    // first fetch.
    ui.invoke_toggle_archive_menu();
    ui.set_archive_dir_text(root.to_string_lossy().as_ref().into());
    ui.invoke_archive_apply_dir(root.to_string_lossy().to_string().as_str().into());
    assert!(ui.get_archive_error().is_empty(), "{:?}", ui.get_archive_error());
    ui.invoke_archive_set_enabled(true);

    app.start(Some(&site));
    wait_for("real tab loaded the fixture", &app, || {
        !snap().is_empty()
            && snap()[0].ready
            && snap()[0].title == "Archive fixture"
            && snap()[0].url.as_deref() == Some(site.as_str())
    });

    // The document must be on disk as the host's `index.html` (the host
    // segment carries the explicit port), under exactly one timestamp dir.
    let body = {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            core.drain();
            let visits = std::fs::read_dir(&root)
                .map(|rd| rd.filter_map(|e| e.ok()).map(|e| e.path()).collect::<Vec<_>>())
                .unwrap_or_default();
            if visits.len() == 1 && visits[0].is_dir() {
                let index = visits[0].join(format!("127.0.0.1:{port}/index.html"));
                if let Ok(bytes) = std::fs::read(&index) {
                    break (visits[0].clone(), bytes);
                }
            }
            assert!(Instant::now() < deadline, "timed out waiting for the archived document");
            std::thread::sleep(Duration::from_millis(50));
        }
    };
    let (visit, archived) = body;
    assert_eq!(archived, PAGE.as_bytes(), "the archived document is the served bytes");
    let name = visit.file_name().unwrap().to_string_lossy().to_string();
    assert!(name.contains('T'), "timestamp folder {name}");

    // The `<link>` stylesheet travels the subresource path (spawn_http) and
    // keeps its URL path under the same host segment.
    let css_deadline = Instant::now() + Duration::from_secs(15);
    let css = visit.join(format!("127.0.0.1:{port}/style.css"));
    loop {
        core.drain();
        if std::fs::read(&css).is_ok() {
            break;
        }
        assert!(Instant::now() < css_deadline, "timed out waiting for the archived stylesheet");
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(std::fs::read(&css).unwrap(), CSS.as_bytes());

    // A query URL is stored in full: `/html/?q=asdf` lands as the file
    // `?q=asdf` inside the `html/` folder of the same host segment (the task:
    // not just `index.html` — the query belongs to the path too).
    ui.invoke_navigate(format!("{site}html/?q=asdf").into());
    let query_file = visit.join(format!("127.0.0.1:{port}/html/?q=asdf"));
    let query_deadline = Instant::now() + Duration::from_secs(15);
    loop {
        core.drain();
        if std::fs::read(&query_file).is_ok() {
            break;
        }
        assert!(
            Instant::now() < query_deadline,
            "timed out waiting for the archived query URL at {}",
            query_file.display()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(std::fs::read(&query_file).unwrap(), PAGE.as_bytes());

    // Switching off mid-session: the live tab acknowledges it; the archive
    // tree stops growing.
    ui.invoke_archive_set_enabled(false);
    std::thread::sleep(Duration::from_millis(100));
    core.drain();
    ui.invoke_navigate(format!("{site}/?again=1").into());
    std::thread::sleep(Duration::from_millis(500));
    core.drain();
    let files: usize = std::fs::read_dir(&root)
        .map(|rd| rd.filter_map(|e| e.ok()).filter(|e| e.path().is_dir()).count())
        .unwrap_or(0);
    assert_eq!(files, 1, "one timestamp folder only — the off-switch stopped the run");

    app.shutdown();
}

fn wait_for(what: &str, app: &App, cond: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        app.core().drain();
        if cond() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what}; tabs: {:?}",
            app.core().tab_snapshot()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}
