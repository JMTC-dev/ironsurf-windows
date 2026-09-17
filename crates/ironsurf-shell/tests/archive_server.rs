//! Archive browsing server (`ironsurf://archive`), end to end through the
//! shell: a tab asks for the server, the shell binds it on a random localhost
//! port, answers the tab with the address, and the folder really is served —
//! including the control page the summary links to.
//!
//! The tab here is the scripted `fake_tab`, which emits the same
//! [`FromTab::ArchiveServerStart`] the real runtime emits when it renders
//! `ironsurf://archive`. No display, no network beyond localhost.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::Path;
use std::time::{Duration, Instant};

use ironsurf_shell::app::App;
use ironsurf_shell::archive_server::percent_encode_note;
use ironsurf_shell::AppWindow;
use slint::ComponentHandle;

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

/// An archived visit: the folder layout archive mode writes (timestamp dir /
/// host dir / index.html) plus a plain file at the root.
fn fixture(root: &Path) {
    let visit = root.join("2026-01-01T00:00:00+00:00").join("example.com");
    std::fs::create_dir_all(&visit).unwrap();
    std::fs::write(visit.join("index.html"), b"<html><title>Archived visit</title></html>").unwrap();
    std::fs::write(root.join("README.txt"), b"the archive\n").unwrap();
}

/// One HTTP exchange over a fresh connection (the server closes after each).
fn get(addr: SocketAddr, path: &str) -> String {
    let mut stream = TcpStream::connect(addr).expect("connect to the archive server");
    stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    stream
        .write_all(format!("GET {path} HTTP/1.1\r\n\r\n").as_bytes())
        .unwrap();
    let mut body = String::new();
    stream.read_to_string(&mut body).expect("read the reply");
    body
}

/// `http://host:port` → `SocketAddr` (only localhost is ever advertised).
fn socket_of(address: &str) -> SocketAddr {
    let authority = address
        .strip_prefix("http://")
        .unwrap_or(address)
        .trim_end_matches('/');
    let (host, port) = authority.rsplit_once(':').expect("host:port");
    assert_eq!(host, "127.0.0.1", "only localhost is ever served: {address}");
    SocketAddr::new([127, 0, 0, 1].into(), port.parse().expect("port"))
}

/// The port out of the status line the fake tab paints when the shell answers
/// (`archive-server:http://127.0.0.1:41234:` — `archive-server:off:<why>` when
/// the shell could not start the server).
fn announced(status: Option<&str>) -> Option<u16> {
    let status = status?.strip_prefix("archive-server:")?;
    let rest = status.strip_prefix("http://127.0.0.1:")?;
    rest.split(':').next().and_then(|p| p.parse().ok())
}

/// The two tests share the process-global `IRONSURF_DATA_DIR` (the shell's
/// store path has no per-test knob), so they take turns.
fn lock_the_shell() -> std::sync::MutexGuard<'static, ()> {
    static SHELL: std::sync::Mutex<()> = std::sync::Mutex::new(());
    match SHELL.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

#[test]
fn ironsurf_archive_starts_the_server_and_serves_the_folder() {
    let _shell = lock_the_shell();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    fixture(&root);
    let fake_tab = env!("CARGO_BIN_EXE_fake_tab");

    i_slint_backend_testing::init_no_event_loop();
    let data = tempfile::tempdir().unwrap();
    std::env::set_var("IRONSURF_DATA_DIR", data.path());
    // The container registry has to exist before the shell opens it.
    ironsurf_storage::Store::open(data.path()).expect("container store");
    let ui = AppWindow::new().expect("window on testing backend");
    let app = App::new(&ui, fake_tab.into(), Vec::new()).expect("app core");
    let snap = || app.core().tab_snapshot();

    // The archive page asks for the server, and names the folder to serve
    // (what the page's form submits when the user edits it).
    app.start(Some(&format!(
        "ironsurf://archive?root={}",
        percent_encode_note(&root.display().to_string())
    )));
    wait_for("the tab to reach the archive page", &app, || {
        !snap().is_empty() && snap()[0].url.as_deref() == Some("ironsurf://archive")
            || !snap().is_empty() && snap()[0].url.as_deref().unwrap_or("").starts_with("ironsurf://archive")
    });

    // The shell answered the tab with the address it bound, on a random port.
    wait_for("the archive server address", &app, || {
        announced(snap()[0].status.as_deref()).is_some()
    });
    let port = announced(snap()[0].status.as_deref()).expect("an address");
    assert_ne!(port, 0, "the random port is resolved, not left at zero");
    assert_ne!(port, 8080, "the default is random, not a fixed well-known port");
    let addr = socket_of(&format!("http://127.0.0.1:{port}"));

    // …and that address really serves the archive folder.
    let reply = get(addr, "/README.txt");
    assert!(reply.starts_with("HTTP/1.1 200 OK"), "{reply}");
    assert!(reply.ends_with("the archive\n"), "{reply}");
    let reply = get(addr, "/2026-01-01T00:00:00%2B00:00/example.com/");
    if reply.starts_with("HTTP/1.1 404") {
        // The `+` of an ISO8601 offset is a literal, not a space — but be
        // tolerant about how the visit folder was spelled on the way in.
        let alt = get(addr, "/2026-01-01T00:00:00+00:00/example.com/");
        assert!(alt.contains("Archived visit"), "{alt}");
    } else {
        assert!(reply.contains("Archived visit"), "{reply}");
    }

    // The front page pre-traverses the timeline: the site saved under a visit
    // folder is linked directly, no click per timestamp needed to reach it.
    let reply = get(addr, "/");
    assert!(reply.starts_with("HTTP/1.1 200 OK"), "{reply}");
    assert!(
        reply.contains("2026-01-01T00%3A00%3A00%2B00%3A00/example.com/"),
        "the saved site is linked straight off the front page: {reply}"
    );

    // The control page is part of the served address: folder, host and port
    // are editable there, and it names the address it lives at.
    let reply = get(addr, "/ironsurf/serve");
    assert!(reply.starts_with("HTTP/1.1 200 OK"), "{reply}");
    assert!(reply.contains("Archive server"), "{reply}");
    assert!(reply.contains("name=\"root\""), "the folder field: {reply}");
    assert!(reply.contains("name=\"host\""), "the host field: {reply}");
    assert!(reply.contains("name=\"port\""), "the port field: {reply}");
    assert!(reply.contains(&format!(":{port}")), "the page names its own address");

    // Nothing outside the folder can be read through it.
    let reply = get(addr, "/%2e%2e%2e%2e/etc/passwd");
    assert!(
        reply.starts_with("HTTP/1.1 403") || reply.starts_with("HTTP/1.1 404"),
        "{reply}"
    );
    assert!(!reply.contains("root:"));

    app.shutdown();
}

#[test]
fn open_in_a_new_tab_browses_the_archive_beside_the_summary() {
    let _shell = lock_the_shell();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    fixture(&root);
    let fake_tab = env!("CARGO_BIN_EXE_fake_tab");

    i_slint_backend_testing::init_no_event_loop();
    let data = tempfile::tempdir().unwrap();
    std::env::set_var("IRONSURF_DATA_DIR", data.path());
    // The container registry has to exist before the shell opens it.
    ironsurf_storage::Store::open(data.path()).expect("container store");
    let ui = AppWindow::new().expect("window on testing backend");
    let app = App::new(&ui, fake_tab.into(), Vec::new()).expect("app core");
    let snap = || app.core().tab_snapshot();

    // `?open=1` (the link the welcome page writes): the summary page stays put
    // and the served folder itself opens in a new tab.
    app.start(Some(&format!(
        "ironsurf://archive?open=1&root={}",
        percent_encode_note(&root.display().to_string())
    )));
    wait_for("a second tab on the served address", &app, || {
        snap().iter().any(|t| {
            t.url
                .as_deref()
                .unwrap_or_default()
                .starts_with("http://127.0.0.1:")
        })
    });
    let urls: Vec<String> = snap()
        .iter()
        .map(|t| t.url.clone().unwrap_or_default())
        .collect();
    assert!(
        urls.iter().any(|u| u.starts_with("ironsurf://archive")),
        "the summary page is still there: {urls:?}"
    );
    let served = urls
        .iter()
        .find(|u| u.starts_with("http://127.0.0.1:"))
        .unwrap_or_else(|| panic!("a new tab opened on the served address: {urls:?}"));
    let addr = socket_of(served);
    // The new tab's address is browsable straight away.
    let reply = get(addr, "/README.txt");
    assert!(reply.starts_with("HTTP/1.1 200 OK"), "{reply}");

    app.shutdown();
}
