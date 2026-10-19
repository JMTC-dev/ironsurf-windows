//! DevTools end to end on the REAL engine: spawns the actual `ironsurf-tab`
//! binary (the same one `tests/real_tab.rs` resolves, next to the test
//! executable) against a tiny `std::net::TcpListener` fixture on 127.0.0.1,
//! and drives it through the shell's own [`TabProc`]/[`Pump`] plumbing — the
//! exact IPC path the DevTools panel consumes in production.
//!
//! The fixture page exercises every console source the panel shows plus both
//! Network-panel legs:
//!
//! * `console.log` / `console.error` → `ConsoleLog { source: "js" }`,
//! * a `<script>` that cannot compile → another `js`/`error`, and the next
//!   script still runs (the tab survived),
//! * `document.querySelector("[")` → `ConsoleLog { source: "dom" }`,
//! * invalid CSS in a `<style>` block → `ConsoleLog { source: "css" }` (the
//!   tab's Stylo re-parse seam),
//! * the main document → one `NetworkEvent` (GET, 200, exact body size),
//! * an `<img>` subresource (a real, valid 1×1 PNG) → a second
//!   `NetworkEvent` through the tab's `BlitzNetProvider`.
//!
//! Headless: no `$DISPLAY` required, localhost only. Skips (like
//! `real_tab.rs`) when the tab binary was not built with the workspace.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use ironsurf_common::{ContainerId, FromTab, NetEvent, TabId};
use ironsurf_shell::tabs::{Pump, TabProc};

/// First paint of the real engine is slow on cold caches; the tab-side suites
/// allow 60 s per leg and this test waits for several legs at once.
const DEADLINE: Duration = Duration::from_secs(90);

/// A page that dirties every DevTools source in one document.
const PAGE: &str = r#"<!doctype html>
<html>
  <head>
    <title>DevTools e2e</title>
    <style>body { invalid-property: invalid-value }</style>
  </head>
  <body style="margin:0;background:#ffffff">
    <p style="color: not-a-real-color">styled wrong</p>
    <img src="/img.png" width="40" height="40" alt="sub">
    <script>console.log("e2e-log"); console.error("e2e-error");</script>
    <script>function ( {</script>
    <script>document.querySelector("[");</script>
    <script>console.log("alive-after-errors");</script>
  </body>
</html>
"#;

/// A real, valid 1×1 RGBA PNG (70 bytes) — the `<img>` subresource the
/// Network panel must report with the exact served byte count.
const PNG: &[u8] = &[
    137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 6, 0,
    0, 0, 31, 21, 196, 137, 0, 0, 0, 13, 73, 68, 65, 84, 120, 218, 99, 252, 207, 192, 80, 15, 0, 4,
    133, 1, 128, 132, 169, 140, 33, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
];

/// Minimal HTTP/1.0 fixture on 127.0.0.1 (one request per connection, kept
/// alive for the whole test process); returns the bound port.
fn spawn_local_http() -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind loopback");
    let port = listener.local_addr().expect("addr").port();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut buf: Vec<u8> = Vec::new();
            let mut byte = [0u8; 1];
            while !buf.ends_with(b"\r\n\r\n") {
                match stream.read(&mut byte) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => buf.push(byte[0]),
                }
                if buf.len() > 64 * 1024 {
                    break;
                }
            }
            let head = String::from_utf8_lossy(&buf).to_string();
            let path = head
                .split_whitespace()
                .nth(1)
                .unwrap_or("/")
                .split('?')
                .next()
                .unwrap_or("/")
                .to_string();
            let response = match path.as_str() {
                "/" | "/index.html" => http_response(200, "text/html", PAGE.as_bytes()),
                "/img.png" => http_response(200, "image/png", PNG),
                _ => http_response(404, "text/plain", b"no such route"),
            };
            let _ = stream.write_all(&response);
            let _ = stream.flush();
        }
    });
    port
}

/// Build the whole response as bytes — a body that is not text (a PNG,
/// say) must reach the tab intact, so nothing here may round-trip it
/// through a `String`.
fn http_response(status: u16, content_type: &str, body: &[u8]) -> Vec<u8> {
    let reason = match status {
        200 => "OK",
        404 => "Not Found",
        _ => "Status",
    };
    let mut response = format!(
        "HTTP/1.0 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    response.extend_from_slice(body);
    response
}

/// The workspace-built `ironsurf` binary — the tab host lives inside it (same resolution as `tests/real_tab.rs`).
fn real_tab_bin() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let profile_dir = exe.parent()?.parent()?; // target/<profile>
    // The single `ironsurf` binary hosts the tab half; the pre-merge
    // `ironsurf-tab` name is still accepted for a stale target directory.
    ["ironsurf", "ironsurf-tab"]
        .iter()
        .map(|n| profile_dir.join(format!("{n}{}", std::env::consts::EXE_SUFFIX)))
        .find(|c| c.is_file())
}

/// The DevTools feed accumulated off the wire, exactly as the shell's state
/// reduction consumes it (console entries in arrival order, one entry per
/// settled network request).
#[derive(Default)]
struct Feed {
    ready: bool,
    url: Option<String>,
    title: Option<String>,
    console: Vec<(String, String, String)>, // (source, level, message)
    net: Vec<NetEvent>,
}

impl Feed {
    fn absorb(&mut self, msg: FromTab) {
        match msg {
            FromTab::Ready { .. } => self.ready = true,
            FromTab::UrlChanged { url } => self.url = url,
            FromTab::TitleChanged { title } => self.title = title,
            FromTab::ConsoleLog {
                source,
                level,
                message,
            } => self.console.push((source, level, message)),
            FromTab::NetworkEvent { event } => self.net.push(event),
            _ => {}
        }
    }

    fn console_has(&self, source: &str, level: &str, msg_pred: impl Fn(&str) -> bool) -> bool {
        self.console
            .iter()
            .any(|(s, l, m)| s == source && l == level && msg_pred(m))
    }

    fn net_has(&self, pred: impl Fn(&NetEvent) -> bool) -> bool {
        self.net.iter().any(pred)
    }

    fn dump(&self) -> String {
        format!(
            "ready={} url={:?} title={:?}\nconsole: {:?}\nnetwork: {:#?}",
            self.ready, self.url, self.title, self.console, self.net
        )
    }
}

/// Drain the pump until `pred` holds over the accumulated feed.
fn wait_for(what: &str, pump: &mut Pump, feed: &mut Feed, pred: impl Fn(&Feed) -> bool) {
    let deadline = Instant::now() + DEADLINE;
    loop {
        for (_, msg) in pump.drain() {
            feed.absorb(msg);
        }
        if pred(feed) {
            return;
        }
        if Instant::now() >= deadline {
            panic!("timed out waiting for {what}\n{}", feed.dump());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn real_tab_devtools_feed_arrives_through_the_shell_pump() {
    let Some(tab_bin) = real_tab_bin() else {
        eprintln!("skip: ironsurf not built (cargo build --workspace first)");
        return;
    };
    let port = spawn_local_http();
    let profile = tempfile::tempdir().expect("profile dir");
    let url = format!("http://127.0.0.1:{port}/");

    let mut pump = Pump::new();
    let mut proc = TabProc::spawn(
        &tab_bin,
        TabId(11),
        ContainerId(1),
        profile.path(),
        Some(&url),
        &[],
        &pump.sender(),
    )
    .expect("spawn real ironsurf-tab");
    let mut feed = Feed::default();

    // The engine booted, parsed the fixture, and reported over real IPC.
    wait_for(
        "Ready/UrlChanged/title preamble",
        &mut pump,
        &mut feed,
        |f| {
            f.ready
                && f.url.as_deref() == Some(url.as_str())
                && f.title.as_deref() == Some("DevTools e2e")
        },
    );

    // ── Console panel: every source arrives with its level ──────────────
    wait_for("js console.log entry", &mut pump, &mut feed, |f| {
        f.console_has("js", "log", |m| m == "e2e-log")
    });
    wait_for("js console.error entry", &mut pump, &mut feed, |f| {
        f.console_has("js", "error", |m| m == "e2e-error")
    });
    // The broken <script> is its own error entry (readable message, not one
    // of the console.error() lines above), and the NEXT script still runs —
    // the tab survived the compile error.
    wait_for("broken-script error entry", &mut pump, &mut feed, |f| {
        f.console_has("js", "error", |m| !m.is_empty() && m != "e2e-error")
    });
    wait_for(
        "script after the errors still ran",
        &mut pump,
        &mut feed,
        |f| f.console_has("js", "log", |m| m == "alive-after-errors"),
    );
    // A native DOM binding throw is classified with source "dom".
    wait_for("dom error entry", &mut pump, &mut feed, |f| {
        f.console_has("dom", "error", |m| m.contains("SyntaxError"))
    });
    // Invalid CSS reaches the console through the tab's Stylo re-parse seam.
    wait_for("css error entry", &mut pump, &mut feed, |f| {
        f.console_has("css", "error", |m| !m.is_empty())
    });

    // ── Network panel: the document and the image, sane columns ─────────
    wait_for("document network event", &mut pump, &mut feed, |f| {
        f.net_has(|e| e.url == url)
    });
    let doc = feed
        .net
        .iter()
        .find(|e| e.url == url)
        .cloned()
        .expect("document event");
    assert_eq!(doc.method, "GET", "document event: {doc:?}");
    assert_eq!(doc.status, Some(200), "document event: {doc:?}");
    assert_eq!(
        doc.response_bytes,
        Some(PAGE.len() as u64),
        "document byte count: {doc:?}"
    );
    assert!(doc.error.is_none(), "document event: {doc:?}");
    assert!(doc.duration_ms.is_some(), "document duration: {doc:?}");

    wait_for("image network event", &mut pump, &mut feed, |f| {
        f.net_has(|e| e.url.ends_with("/img.png"))
    });
    let img = feed
        .net
        .iter()
        .find(|e| e.url.ends_with("/img.png"))
        .cloned()
        .expect("image event");
    assert_eq!(img.method, "GET", "image event: {img:?}");
    assert_eq!(img.status, Some(200), "image event: {img:?}");
    assert_eq!(
        img.response_bytes,
        Some(PNG.len() as u64),
        "image byte count: {img:?}"
    );
    assert!(img.error.is_none(), "image event: {img:?}");

    // ≥2 events (document + image) and the contract holds for every one:
    // method is never blank, and status/error are mutually exclusive.
    assert!(feed.net.len() >= 2, "network feed: {:?}", feed.net);
    for event in &feed.net {
        assert!(!event.method.is_empty(), "event method: {event:?}");
        assert!(
            event.error.is_none() || event.status.is_none(),
            "status/error must be exclusive: {event:?}"
        );
    }

    // The tab is still healthy after a page full of errors, and shuts down
    // cleanly on `Destroy` (exit 0, same path the shell uses on ✕).
    assert!(
        proc.try_wait_status().is_none(),
        "tab must survive the error page: {}",
        proc.stderr_tail()
    );
    proc.destroy();
    let status = proc.child_mut().wait().expect("wait for tab exit");
    assert_eq!(status.code(), Some(0), "stderr: {}", proc.stderr_tail());
}
