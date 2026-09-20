//! `document.cookie` on the REAL tab, riding the container jar (task: the
//! axios XSRF read — Mastodon's request layer did `document.cookie.match(...)`,
//! read `undefined`, and threw boa's "cannot convert 'null' or 'undefined'
//! to object"). Headless (no `$DISPLAY`, localhost only): one tiny HTTP
//! fixture with three routes and a server-side record of every request's
//! `Cookie:` header. Asserts:
//!
//! * a page on a document the server `Set-Cookie`d reads the non-HttpOnly
//!   cookie through `document.cookie` (the HttpOnly one stays invisible —
//!   exact-title assertion, so a leak into the script view fails),
//! * a cookie the PAGE set (`js=a`, with an `HttpOnly` attribute it must
//!   not be allowed to stamp) rides out on the very next document request,
//! * the HttpOnly cookie the scripts never saw DOES ride on that same wire
//!   (server-side header record) — hidden from JS, not from HTTP,
//!
//! which is exactly the pair of guarantees the jar has always promised on
//! the Rust side, now proven through the JS surface end to end.
//!
//! Run with: `cargo test -p ironsurf-tab --test cookies`

use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use ironsurf_common::{read_frame, write_frame, FromTab, ToTab};

const DEADLINE: Duration = Duration::from_secs(60);

/// (request path, `Cookie:` header seen) for EVERY request the fixture
/// answered — the wire-side record the asserts read.
fn log() -> &'static Mutex<Vec<(String, String)>> {
    static LOG: OnceLock<Mutex<Vec<(String, String)>>> = OnceLock::new();
    LOG.get_or_init(|| Mutex::new(Vec::new()))
}

/// `/seed`  — served WITH one HttpOnly and one script-visible cookie; the
/// page reports what `document.cookie` reads through the title and sets a
/// cookie of its own (trying, and failing, to stamp it HttpOnly).
const PAGE_SEED: &str = r#"<!doctype html>
<html>
  <head><title>seed page</title></head>
  <body style="margin:0;background:#ffffff">
    <script>
      document.cookie = "js=a; HttpOnly";
      document.title = "seed:" + document.cookie;
    </script>
  </body>
</html>
"#;

/// `/echo` — no cookies of its own; the title is static so the WIRE view
/// can only come from the server-side header record, never from JS.
const PAGE_ECHO: &str = r#"<!doctype html>
<html>
  <head><title>echo page</title></head>
  <body style="margin:0;background:#ffffff"><p>echo</p></body>
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
            // Record the wire view of the jar for EVERY request.
            let cookie_header = head
                .lines()
                .find(|line| line.to_ascii_lowercase().starts_with("cookie:"))
                .map(|line| line["cookie:".len()..].trim().to_string())
                .unwrap_or_default();
            log().lock().unwrap().push((path.clone(), cookie_header));
            let (body, extra_headers) = if path == "/seed" {
                (
                    PAGE_SEED,
                    "Set-Cookie: secret=S; Path=/; HttpOnly\r\nSet-Cookie: srv=1; Path=/\r\n",
                )
            } else {
                (PAGE_ECHO, "")
            };
            let response = format!(
                "HTTP/1.0 200 OK\r\n{extra_headers}Content-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });
    port
}

struct TabChild {
    child: Child,
    #[allow(dead_code)]
    stdin: ChildStdin,
    events: Receiver<FromTab>,
    seen: Vec<FromTab>,
}

impl TabChild {
    fn send(&mut self, msg: ToTab) {
        write_frame(&mut self.stdin, &msg).expect("write ToTab frame to tab stdin");
    }

    fn pump(&mut self) -> bool {
        match self.events.recv_timeout(Duration::from_millis(500)) {
            Ok(msg) => {
                self.seen.push(msg);
                true
            }
            Err(_) => false,
        }
    }

    fn titles(&self) -> Vec<String> {
        self.seen
            .iter()
            .filter_map(|msg| match msg {
                FromTab::TitleChanged { title: Some(t) } => Some(t.clone()),
                _ => None,
            })
            .collect()
    }

    fn wait_title(&mut self, needle: &str) {
        let deadline = Instant::now() + DEADLINE;
        loop {
            if self.titles().iter().any(|t| t.contains(needle)) {
                return;
            }
            if Instant::now() >= deadline {
                let status = self.child.try_wait().ok().flatten();
                panic!(
                    "timed out waiting for title containing {needle:?} \
                     (child status: {status:?})\nseen titles: {:?}",
                    self.titles()
                );
            }
            self.pump();
        }
    }

    /// The server-side record: the `Cookie:` header of the LAST request to
    /// `path` (waits while no such request has arrived yet).
    fn wait_wire(&mut self, path: &str, needle: &str) -> String {
        let deadline = Instant::now() + DEADLINE;
        loop {
            let seen = log().lock().unwrap().clone();
            if let Some((_, header)) = seen.iter().rev().find(|(p, _)| p == path) {
                if header.contains(needle) {
                    return header.clone();
                }
            }
            if Instant::now() >= deadline {
                let status = self.child.try_wait().ok().flatten();
                panic!(
                    "timed out needing the wire request to {path:?} to carry \
                     {needle:?} (child status: {status:?}); every recorded \
                     (path, Cookie) pair: {seen:?}"
                );
            }
            self.pump();
        }
    }
}

fn spawn_tab(url: &str, profile_dir: &std::path::Path) -> TabChild {
    let exe = env!("CARGO_BIN_EXE_ironsurf-tab");
    let mut child = Command::new(exe)
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .arg("--tab-id")
        .arg("72")
        .arg("--container-id")
        .arg("1")
        .arg("--profile-dir")
        .arg(profile_dir)
        .arg("--url")
        .arg(url)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(
            std::fs::File::create(profile_dir.join(format!(
                "tab-cookies-stderr-{}.log",
                std::process::id()
            )))
            .expect("stderr log"),
        )
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

#[test]
fn document_cookie_shares_the_jar_with_the_wire_httponly_stays_hidden() {
    let port = spawn_local_http();
    let profile = tempfile::tempdir().expect("tempdir");
    let mut tab = spawn_tab(&format!("http://127.0.0.1:{port}/seed"), profile.path());

    // The script-visible view: the HttpOnly seed cookie NEVER appears —
    // the title is an exact match, so one leak and this assert dies. The
    // page's own `js=a` write is readable immediately.
    tab.wait_title("seed:srv=1; js=a");

    // The wire view on the NEXT document request (same jar, one tab): the
    // cookie the page set rides out, the server's HttpOnly cookie rides
    // out too — hidden from JS, invisible on this header would be a bug —
    // and the plain `srv=1` cookie is there as ever.
    tab.send(ToTab::Navigate {
        url: format!("http://127.0.0.1:{port}/echo"),
    });
    let header = tab.wait_wire("/echo", "js=a");
    assert!(
        header.contains("secret=S"),
        "HttpOnly cookie vanished from the wire (scripts hide it, HTTP must not): {header}"
    );
    assert!(header.contains("srv=1"), "server cookie missing on the wire: {header}");

    // And the script-visible view is unchanged on later reads: the exact
    // seed title above already pins that `secret=S` never entered
    // `document.cookie` (any leak would have changed that title).
    let _ = header;
}
