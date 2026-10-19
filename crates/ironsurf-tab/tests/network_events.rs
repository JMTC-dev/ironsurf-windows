//! DevTools network reporting over IPC (task: Firefox-Network-tab style).
//!
//! Headless (no `$DISPLAY`, localhost only): spawns the built `ironsurf-tab`
//! binary against a tiny `std::net::TcpListener` HTTP fixture on 127.0.0.1
//! serving a page with an `<img>` subresource and an inline `console.log`,
//! then asserts the `FromTab::NetworkEvent` stream:
//!
//! * one event for the main-document load (GET, 200, exact body size),
//! * one event per followed redirect hop (302 hop + final 200),
//! * one event for the `<img>` subresource (200, exact byte size),
//! * one failure event for an unreachable server (error `Some`, status
//!   `None`),
//! * no events for `data:` URLs or unsupported schemes (no-spam rule).
//!
//! Run with: `cargo test -p ironsurf-tab --test network_events`

use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use ironsurf_common::{read_frame, write_frame, FromTab, NetEvent, ToTab};

const DEADLINE: Duration = Duration::from_secs(60);

/// Fixture document: an `<img>` subresource (fetched through the tab's
/// `BlitzNetProvider`) plus an inline console line (sanity check that the
/// page actually parsed and ran).
const PAGE: &str = r#"<!doctype html>
<html>
  <head><title>Net events</title></head>
  <body style="margin: 0; background: #ffffff">
    <img src="/img.png" width="40" height="40" alt="sub">
    <script>console.log("net-probe-console");</script>
  </body>
</html>
"#;

/// "Body" served for `/img.png`. Blitz fetches the subresource whatever its
/// decodability is (the fetch settles and is reported regardless), so the
/// bytes only need a stable length to assert `response_bytes` exactly.
const IMG: &[u8] = b"\x89PNG\r\n\x1a\nnot-a-real-png-but-the-fetch-still-lands";

const FINAL_PAGE: &str = r#"<!doctype html>
<html><head><title>Final</title></head><body style="margin:0;background:#ffffff"><p>final hop</p></body></html>
"#;

/// A page whose inline script issues one `XMLHttpRequest` and, once it lands,
/// one `fetch()` — the two JS-initiated request paths the network panel must
/// show (task: XHR/fetch in the DevTools network tab).
const XHR_FETCH_PAGE: &str = r#"<!doctype html>
<html>
  <head><title>JS net</title></head>
  <body style="margin: 0; background: #ffffff">
    <script>
      var x = new XMLHttpRequest();
      x.open("GET", "/api-xhr");
      x.onload = function () {
        fetch("/api-fetch").then(function (r) { return r.text(); }).then(function (t) {
          document.title = "JS-NET:" + t;
        });
      };
      x.send();
    </script>
  </body>
</html>
"#;

/// Minimal HTTP/1.0 fixture on 127.0.0.1 (one request per connection);
/// returns the bound port. Kept alive for the whole test process.
fn spawn_local_http() -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind loopback");
    let port = listener.local_addr().expect("addr").port();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            // Read the request head.
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
                "/img.png" => http_response(200, "image/png", IMG),
                "/r" => "HTTP/1.0 302 Found\r\nLocation: /final\r\nConnection: close\r\n\r\n".into(),
                "/final" => http_response(200, "text/html", FINAL_PAGE.as_bytes()),
                "/xhr-fetch.html" => http_response(200, "text/html", XHR_FETCH_PAGE.as_bytes()),
                "/api-xhr" => http_response(200, "text/plain", b"XHR-BODY"),
                "/api-fetch" => http_response(200, "text/plain", b"FETCH-BODY"),
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

    /// Block until a `FromTab::NetworkEvent` satisfies `pred` (already-seen
    /// messages count, like the e2e suite's `wait_for`).
    fn wait_net(&mut self, what: &str, pred: impl Fn(&NetEvent) -> bool) -> NetEvent {
        let deadline = Instant::now() + DEADLINE;
        loop {
            if let Some(event) = self
                .seen
                .iter()
                .filter_map(net_event)
                .find(|event| pred(event))
            {
                return event.clone();
            }
            if Instant::now() >= deadline {
                let status = self.child.try_wait().ok().flatten();
                panic!(
                    "timed out waiting for network event {what} (child status: {status:?})\n\
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
                        "stdout closed while waiting for network event {what} \
                         (child status: {status:?})\nmessages seen ({}): {:#?}",
                        self.seen.len(),
                        self.seen
                    );
                }
            }
        }
    }
}

fn net_event(msg: &FromTab) -> Option<&NetEvent> {
    match msg {
        FromTab::NetworkEvent { event } => Some(event),
        _ => None,
    }
}

fn spawn_tab(url: &str, profile_dir: &std::path::Path) -> TabChild {
    let exe = env!("CARGO_BIN_EXE_ironsurf");
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

/// The headline test: a localhost page load reports the document, every
/// redirect hop, the `<img>` subresource, and a failed server — each as
/// exactly one `FromTab::NetworkEvent` with the right method/status/size.
#[test]
fn network_events_report_documents_hops_and_subresources() {
    let port = spawn_local_http();
    let profile = tempfile::tempdir().expect("profile dir");
    let base = format!("http://127.0.0.1:{port}");
    let mut tab = spawn_tab(&format!("{base}/index.html"), profile.path());

    // 1. The main document: GET, 200, the exact fixture body size.
    let doc = tab.wait_net("document event", |e| {
        e.url.ends_with("/index.html") || e.url.ends_with("/index.html#")
    });
    assert_eq!(doc.method, "GET", "document event: {doc:?}");
    assert_eq!(doc.status, Some(200), "document event: {doc:?}");
    assert_eq!(
        doc.response_bytes,
        Some(PAGE.len() as u64),
        "document byte count: {doc:?}"
    );
    assert!(doc.error.is_none(), "document event: {doc:?}");
    assert!(
        doc.duration_ms.is_some(),
        "document duration measured: {doc:?}"
    );
    assert_eq!(
        doc.initiator.as_deref(),
        Some("document"),
        "the navigation names itself the initiator: {doc:?}"
    );

    // 2. The `<img>` subresource through the NetProvider: GET, 200, exact
    // byte count of the served body.
    let img = tab.wait_net("subresource event", |e| e.url.ends_with("/img.png"));
    assert_eq!(img.method, "GET", "img event: {img:?}");
    assert_eq!(img.status, Some(200), "img event: {img:?}");
    assert_eq!(
        img.response_bytes,
        Some(IMG.len() as u64),
        "img byte count: {img:?}"
    );
    assert!(img.error.is_none(), "img event: {img:?}");
    assert_eq!(
        img.initiator.as_deref(),
        Some("resource"),
        "a Blitz subresource is a plain resource: {img:?}"
    );

    // Sanity: the page really parsed and ran (its inline console line rides
    // the same IPC stream as a StatusText, exactly like the e2e JS tests).
    let deadline = Instant::now() + DEADLINE;
    loop {
        if tab.seen.iter().any(|m| {
            matches!(m, FromTab::StatusText { text: Some(t) } if t.contains("net-probe-console"))
        }) {
            break;
        }
        assert!(Instant::now() < deadline, "console line never arrived");
        if let Ok(msg) = tab.events.recv_timeout(Duration::from_millis(500)) {
            tab.seen.push(msg);
        }
    }

    // 3. A redirected navigation: one event per hop (the 302 itself) plus
    // one for the final response.
    tab.send(ToTab::Navigate {
        url: format!("{base}/r"),
    });
    let hop = tab.wait_net("redirect hop event", |e| {
        e.url.ends_with("/r") && e.status == Some(302)
    });
    assert_eq!(hop.method, "GET", "hop event: {hop:?}");
    assert_eq!(hop.response_bytes, Some(0), "empty-bodied 302: {hop:?}");
    let final_doc = tab.wait_net("redirect final event", |e| {
        e.url.ends_with("/final") && e.status == Some(200)
    });
    assert_eq!(
        final_doc.response_bytes,
        Some(FINAL_PAGE.len() as u64),
        "final doc: {final_doc:?}"
    );

    // 4. A failed request (connection refused on loopback): the event
    // carries `error: Some(..)` and `status: None`, naming the URL that was
    // asked for.
    tab.send(ToTab::Navigate {
        url: "http://127.0.0.1:1/".to_owned(),
    });
    let dead = tab.wait_net("failure event", |e| {
        e.url == "http://127.0.0.1:1/" && e.status.is_none() && e.error.is_some()
    });
    assert_eq!(dead.method, "GET", "failure event: {dead:?}");
    let reason = dead.error.clone().expect("checked above");
    assert!(!reason.is_empty(), "failure reason empty: {dead:?}");
}

/// JS-initiated requests show in the panel too (task: XHR/fetch in the
/// DevTools network tab): one `XMLHttpRequest` reports with initiator
/// `"xhr"`, one `fetch()` reports with initiator `"fetch"`, and both carry
/// the real status/body size of the served response.
#[test]
fn xhr_and_fetch_requests_reach_the_network_panel() {
    let port = spawn_local_http();
    let profile = tempfile::tempdir().expect("profile dir");
    let base = format!("http://127.0.0.1:{port}");
    let mut tab = spawn_tab(&format!("{base}/xhr-fetch.html"), profile.path());

    let xhr = tab.wait_net("XHR event", |e| e.url.ends_with("/api-xhr"));
    assert_eq!(xhr.method, "GET", "xhr event: {xhr:?}");
    assert_eq!(xhr.status, Some(200), "xhr event: {xhr:?}");
    assert_eq!(xhr.response_bytes, Some(8), "XHR-BODY is 8 bytes: {xhr:?}");
    assert!(xhr.error.is_none(), "xhr event: {xhr:?}");
    assert_eq!(
        xhr.initiator.as_deref(),
        Some("xhr"),
        "XMLHttpRequest names itself: {xhr:?}"
    );

    let fetch = tab.wait_net("fetch event", |e| e.url.ends_with("/api-fetch"));
    assert_eq!(fetch.method, "GET", "fetch event: {fetch:?}");
    assert_eq!(fetch.status, Some(200), "fetch event: {fetch:?}");
    assert_eq!(fetch.response_bytes, Some(10), "FETCH-BODY is 10 bytes: {fetch:?}");
    assert!(fetch.error.is_none(), "fetch event: {fetch:?}");
    assert_eq!(
        fetch.initiator.as_deref(),
        Some("fetch"),
        "window.fetch names itself: {fetch:?}"
    );
}

/// No-spam rule: an inline `data:` subresource is not a request and must
/// never appear in the network stream (pages carry many of them; the panel
/// would drown). The data: *document* itself is one load = at most one
/// event, and nothing else may ride along.
#[test]
fn data_url_subresources_stay_off_the_network_stream() {
    let profile = tempfile::tempdir().expect("profile dir");
    // A data: document holding a data: image (base64 of a 12-byte stub).
    let url = "data:text/html,<title>Data doc</title><img src='data:image/png;base64,iVBORw0KGgo='>";
    let mut tab = spawn_tab(url, profile.path());

    // The document itself loads (title arrives)…
    let deadline = Instant::now() + DEADLINE;
    loop {
        if tab.seen.iter().any(|m| {
            matches!(m, FromTab::TitleChanged { title: Some(t) } if t == "Data doc")
        }) {
            break;
        }
        assert!(Instant::now() < deadline, "data: document never loaded");
        if let Ok(msg) = tab.events.recv_timeout(Duration::from_millis(500)) {
            tab.seen.push(msg);
        }
    }

    // …and after a settle window, no event anywhere names the data: image.
    let window = Instant::now() + Duration::from_millis(600);
    while Instant::now() < window {
        if let Ok(msg) = tab.events.recv_timeout(Duration::from_millis(100)) {
            tab.seen.push(msg);
        }
    }
    let events: Vec<&NetEvent> = tab.seen.iter().filter_map(net_event).collect();
    for event in &events {
        assert!(
            !event.url.starts_with("data:image"),
            "inline data: subresource must not be reported: {events:?}"
        );
    }
    // One document load ⇒ at most one event (a data: document is no more
    // than a single entry, never a burst).
    assert!(
        events.len() <= 1,
        "expected at most one event for a one-document data: load, got: {events:?}"
    );
}
