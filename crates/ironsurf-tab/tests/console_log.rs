//! DevTools console entries ride the IPC (task: JS/CSS/DOM console sources).
//!
//! Headless (no `$DISPLAY`, localhost only): spawns the built `ironsurf-tab`
//! against a tiny `std::net::TcpListener` HTTP fixture and asserts the
//! `FromTab::ConsoleLog` stream, classified by `source`:
//!
//! * every `console.*` level → `{ source: "js", level: log|info|warn|error }`,
//! * a broken `<script>` → `{ source: "js", level: "error" }` **and** a tab
//!   that stays alive (next script runs, next navigation works),
//! * a native DOM throw (`document.querySelector("[")`) → `{ source: "dom",
//!   level: "error" }` with the DOM-prefixed message, reported **once** (no
//!   duplicate `"js"` line for the same throw),
//! * invalid CSS (a `<style>` body and a `style="…"` attribute) →
//!   `{ source: "css", level: "error" }` via the tab's Stylo re-parse seam,
//! * `.css` served with the wrong `Content-Type` → `{ source: "css" }` from
//!   the fetch-side seam in the `BlitzNetProvider`.
//!
//! Run with: `cargo test -p ironsurf-tab --test console_log`

use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use ironsurf_common::{read_frame, write_frame, FromTab, ToTab};

const DEADLINE: Duration = Duration::from_secs(60);

/// Console page: all four levels on the wire, in order.
const PAGE_CONSOLE: &str = r#"<!doctype html>
<html>
  <head><title>Console</title></head>
  <body style="margin:0;background:#ffffff">
    <script>
      console.log("wire-log");
      console.info("wire-info");
      console.warn("wire-warn");
      console.error("wire-error");
    </script>
  </body>
</html>
"#;

/// A `<script>` that cannot even compile (syntax error mid-function), then a
/// second script proving the tab went on to run the rest of the page.
const PAGE_BROKEN_SCRIPT: &str = r#"<!doctype html>
<html>
  <head><title>Broken</title></head>
  <body style="margin:0;background:#ffffff">
    <script>function ( {</script>
    <script>console.log("alive-after-error");</script>
  </body>
</html>
"#;

/// An uncaught throw out of a native DOM binding (`__ironsurfDomQuery` with
/// an invalid selector): reported once, with `source: "dom"`.
const PAGE_BAD_DOM_CALL: &str = r#"<!doctype html>
<html>
  <head><title>Bad DOM call</title></head>
  <body style="margin:0;background:#ffffff">
    <script>document.querySelector("[");</script>
    <script>console.log("alive-after-dom-error");</script>
  </body>
</html>
"#;

/// Invalid CSS at both re-parse seams: a `<style>` body with an unknown
/// property and an inline `style` attribute with an invalid value.
const PAGE_BAD_CSS: &str = r#"<!doctype html>
<html>
  <head><title>Bad CSS</title></head>
  <body style="margin:0;background:#ffffff">
    <style>body { invalid-property: invalid-value }</style>
    <p style="color: not-a-real-color">styled wrong</p>
  </body>
</html>
"#;

/// A `<link rel=stylesheet>` whose response is not `text/css` (the fetch-side
/// CSS seam). The document itself is fine.
const PAGE_BAD_CSS_LINK: &str = r#"<!doctype html>
<html>
  <head><title>Bad stylesheet link</title>
    <link rel="stylesheet" href="/bad.css">
  </head>
  <body style="margin:0;background:#ffffff"><p>linked</p></body>
</html>
"#;

/// Control for the content-type seam: the same link, served `text/css`.
const PAGE_GOOD_CSS_LINK: &str = r#"<!doctype html>
<html>
  <head><title>Good stylesheet link</title>
    <link rel="stylesheet" href="/good.css">
  </head>
  <body style="margin:0;background:#ffffff"><p>linked well</p></body>
</html>
"#;

const PLAIN_PAGE: &str = r#"<!doctype html>
<html><head><title>Plain</title></head><body style="margin:0;background:#ffffff"><p>plain</p></body></html>
"#;

/// Minimal HTTP/1.0 fixture on 127.0.0.1 (one request per connection);
/// returns the bound port. Kept alive for the whole test process.
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
                "/console" => http_response(200, "text/html", PAGE_CONSOLE.as_bytes()),
                "/broken" => http_response(200, "text/html", PAGE_BROKEN_SCRIPT.as_bytes()),
                "/bad-dom" => http_response(200, "text/html", PAGE_BAD_DOM_CALL.as_bytes()),
                "/bad-css" => http_response(200, "text/html", PAGE_BAD_CSS.as_bytes()),
                "/bad-css-link" => http_response(200, "text/html", PAGE_BAD_CSS_LINK.as_bytes()),
                // The wrong-content-type fixture: a `.css` URL served as HTML.
                "/bad.css" => http_response(200, "text/html", b"body { color: red }"),
                // …and a well-behaved control: `.css` served as `text/css`.
                "/good.css" => http_response(200, "text/css", b"body { color: red }"),
                "/good-css-link.html" => {
                    http_response(200, "text/html", PAGE_GOOD_CSS_LINK.as_bytes())
                }
                _ => http_response(200, "text/html", PLAIN_PAGE.as_bytes()),
            };
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });
    port
}

fn http_response(status: u16, content_type: &str, body: &[u8]) -> String {
    format!(
        "HTTP/1.0 {status} OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    ) + &String::from_utf8_lossy(body)
}

struct TabChild {
    child: Child,
    stdin: ChildStdin,
    events: Receiver<FromTab>,
    seen: Vec<FromTab>,
}

impl Drop for TabChild {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl TabChild {
    fn send(&mut self, msg: ToTab) {
        write_frame(&mut self.stdin, &msg).expect("write ToTab frame to tab stdin");
    }

    /// Block until a `FromTab::ConsoleLog` satisfies `pred` (already-seen
    /// messages count). Panics (dumping everything seen) at the deadline.
    fn wait_console(
        &mut self,
        what: &str,
        pred: impl Fn(&str, &str, &str) -> bool,
    ) -> (String, String, String) {
        let deadline = Instant::now() + DEADLINE;
        loop {
            if let Some(entry) = self.seen.iter().find_map(|msg| match msg {
                FromTab::ConsoleLog {
                    source,
                    level,
                    message,
                } if pred(source, level, message) =>
                {
                    Some((source.clone(), level.clone(), message.clone()))
                }
                _ => None,
            }) {
                return entry;
            }
            if Instant::now() >= deadline {
                let status = self.child.try_wait().ok().flatten();
                panic!(
                    "timed out waiting for console entry {what} (child status: {status:?})\n\
                     messages seen ({}): {:#?}",
                    self.seen.len(),
                    self.seen
                );
            }
            match self.events.recv_timeout(Duration::from_millis(500)) {
                Ok(msg) => self.seen.push(msg),
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    let status = self.child.wait().ok();
                    panic!(
                        "stdout closed while waiting for console entry {what} \
                         (child status: {status:?})\nmessages seen ({}): {:#?}",
                        self.seen.len(),
                        self.seen
                    );
                }
            }
        }
    }

    /// Block until any `FromTab` satisfies `pred`.
    fn wait_for(&mut self, what: &str, pred: impl Fn(&FromTab) -> bool) {
        let deadline = Instant::now() + DEADLINE;
        loop {
            if self.seen.iter().any(|msg| pred(msg)) {
                return;
            }
            if Instant::now() >= deadline {
                panic!("timed out waiting for {what}; seen {:#?}", self.seen);
            }
            match self.events.recv_timeout(Duration::from_millis(500)) {
                Ok(msg) => self.seen.push(msg),
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    panic!("stdout closed while waiting for {what}")
                }
            }
        }
    }
}

fn spawn_tab(url: &str, profile_dir: &std::path::Path) -> TabChild {
    let exe = env!("CARGO_BIN_EXE_ironsurf-tab");
    let mut child = Command::new(exe)
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .arg("--tab-id")
        .arg("42")
        .arg("--container-id")
        .arg("1")
        .arg("--profile-dir")
        .arg(profile_dir)
        .arg("--url")
        .arg(url)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(std::fs::File::create(profile_dir.join("tab-stderr.log")).expect("stderr log"))
        .spawn()
        .expect("spawn ironsurf-tab");

    let stdout = child.stdout.take().expect("child stdout");
    let stdin = child.stdin.take().expect("child stdin");
    let (tx, events) = mpsc::channel();
    thread::spawn(move || {
        let mut reader = std::io::BufReader::new(stdout);
        while let Ok(msg) = read_frame::<_, FromTab>(&mut reader) {
            if tx.send(msg).is_err() {
                break;
            }
        }
    });

    TabChild {
        child,
        stdin,
        events,
        seen: Vec::new(),
    }
}

/// Every `console.*` level arrives as its own `ConsoleLog { source: "js" }`,
/// with the exact message text, in page order.
#[test]
fn console_levels_ride_the_wire() {
    let port = spawn_local_http();
    let profile = tempfile::tempdir().expect("profile dir");
    let base = format!("http://127.0.0.1:{port}");
    let mut tab = spawn_tab(&format!("{base}/console"), profile.path());

    for (level, text) in [
        ("log", "wire-log"),
        ("info", "wire-info"),
        ("warn", "wire-warn"),
        ("error", "wire-error"),
    ] {
        let (source, got_level, message) = tab.wait_console(level, |source, got, message| {
            source == "js" && got == level && message == text
        });
        assert_eq!((source.as_str(), got_level.as_str(), message.as_str()), ("js", level, text));
    }
    // Page order is preserved on the wire (the shell's ring keeps first-seen order).
    let texts: Vec<&str> = tab
        .seen
        .iter()
        .filter_map(|msg| match msg {
            FromTab::ConsoleLog { source, message, .. } if source == "js" => Some(message.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        texts,
        vec!["wire-log", "wire-info", "wire-warn", "wire-error"],
        "console entries must arrive in page order"
    );
}

/// A script that fails to compile is a `ConsoleLog { error, js }` — and the
/// tab survives: the next script runs, and a later navigation still works.
#[test]
fn broken_script_reports_error_js_and_tab_stays_alive() {
    let port = spawn_local_http();
    let profile = tempfile::tempdir().expect("profile dir");
    let base = format!("http://127.0.0.1:{port}");
    let mut tab = spawn_tab(&format!("{base}/broken"), profile.path());

    // 1. The compile error itself: one `js`/`error` entry.
    let (source, level, message) =
        tab.wait_console("syntax error", |source, level, _| source == "js" && level == "error");
    assert_eq!((source.as_str(), level.as_str()), ("js", "error"));
    assert!(
        !message.is_empty(),
        "the script error must carry a readable message, got {message:?}"
    );

    // 2. Still alive on the page side: the NEXT script's console line lands.
    tab.wait_console("surviving script", |_, _, message| {
        message == "alive-after-error"
    });

    // 3. Still alive on the IPC side: a navigation to another page completes.
    tab.send(ToTab::Navigate {
        url: format!("{base}/plain.html"),
    });
    tab.wait_for("UrlChanged after the error", |msg| {
        matches!(msg, FromTab::UrlChanged { url: Some(u) } if u == &format!("{base}/plain.html"))
    });
    // …and the child process itself is alive (no panic, no exit).
    assert!(
        tab.child.try_wait().ok().flatten().is_none(),
        "the tab must survive a broken script"
    );
}

/// An uncaught throw out of a native DOM binding is `ConsoleLog { error, dom }`
/// with the DOM-prefixed message — reported once (no duplicate `"js"` entry
/// carrying the same throw).
#[test]
fn bad_dom_call_reports_dom_source_once() {
    let port = spawn_local_http();
    let profile = tempfile::tempdir().expect("profile dir");
    let base = format!("http://127.0.0.1:{port}");
    let mut tab = spawn_tab(&format!("{base}/bad-dom"), profile.path());

    // 1. The DOM entry itself: SyntaxError (from the selector parser), once,
    // with `source: "dom"`.
    let (source, level, message) = tab.wait_console("dom throw", |source, level, message| {
        source == "dom" && level == "error" && message.contains("SyntaxError")
    });
    assert_eq!((source.as_str(), level.as_str()), ("dom", "error"));
    // The message is the native's readable DOM-prefixed text.
    assert!(
        message.contains("SyntaxError"),
        "expected a DOM-prefixed message, got {message:?}"
    );

    // 2. Let the page settle (the marker script after the throw), then check
    // the throw was reported EXACTLY once — the generic script-error path
    // skipped it (no duplicate `"js"` line with the same message).
    tab.wait_console("marker after dom throw", |_, _, message| {
        message == "alive-after-dom-error"
    });
    let dom_reports = tab
        .seen
        .iter()
        .filter(|msg| {
            matches!(msg, FromTab::ConsoleLog { source, level, .. } if source == "dom" && level == "error")
        })
        .count();
    assert_eq!(dom_reports, 1, "the DOM throw must be reported exactly once");
    let js_dupes = tab
        .seen
        .iter()
        .filter(|msg| {
            matches!(msg, FromTab::ConsoleLog { source, level, message } if source == "js"
                && level == "error"
                && message.contains("SyntaxError"))
        })
        .count();
    assert_eq!(js_dupes, 0, "the generic js path must not double-report the native throw");
}

/// Invalid CSS — a `<style>` body and an inline `style="…"` attribute — is
/// reported by the tab's Stylo re-parse seam as `ConsoleLog { source: "css" }`.
#[test]
fn invalid_css_reports_css_source() {
    let port = spawn_local_http();
    let profile = tempfile::tempdir().expect("profile dir");
    let base = format!("http://127.0.0.1:{port}");
    let mut tab = spawn_tab(&format!("{base}/bad-css"), profile.path());

    // 1. The `<style>` body's unknown property.
    tab.wait_console("style element error", |source, level, message| {
        source == "css" && level == "error" && message.contains("<style>")
    });
    // 2. The inline `style="color: not-a-real-color"` attribute.
    tab.wait_console("inline style error", |source, level, message| {
        source == "css" && level == "error" && message.contains("color")
    });
}

/// The fetch-side CSS seam: a `.css` response served as `text/html` raises a
/// `ConsoleLog { source: "css" }` naming both types; a control page linking a
/// properly-served `text/css` stylesheet raises none.
#[test]
fn wrong_content_type_css_warns_and_good_css_does_not() {
    let port = spawn_local_http();
    let profile = tempfile::tempdir().expect("profile dir");
    let base = format!("http://127.0.0.1:{port}");
    let mut tab = spawn_tab(&format!("{base}/bad-css-link"), profile.path());

    let (source, level, message) = tab.wait_console("wrong css type", |source, level, message| {
        source == "css" && level == "error" && message.contains("bad.css")
    });
    assert_eq!((source.as_str(), level.as_str()), ("css", "error"));
    assert!(
        message.contains("text/html") && message.contains("text/css"),
        "the entry must name what was served and what CSS needs, got {message:?}"
    );

    // Control: the same page linking a real text/css stylesheet → no such warn.
    let control = tempfile::tempdir().expect("control profile dir");
    let mut control_tab = spawn_tab(
        &format!("{base}/good-css-link.html"),
        control.path(),
    );
    // Give the control load a moment to settle (its document event proves life).
    control_tab.wait_for("control load finished", |msg| {
        matches!(msg, FromTab::LoadingChanged { loading: false })
    });
    // …plus a grace window: a stray css warning (subresource settle) could
    // only arrive after the load finished, never before.
    thread::sleep(Duration::from_millis(1000));
    while let Ok(msg) = control_tab.events.try_recv() {
        control_tab.seen.push(msg);
    }
    let warns = control_tab
        .seen
        .iter()
        .filter(|msg| {
            matches!(msg, FromTab::ConsoleLog { source, .. } if source == "css")
        })
        .count();
    assert_eq!(warns, 0, "text/css must not warn (control page): {:#?}", control_tab.seen);
}
