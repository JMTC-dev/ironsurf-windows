//! Reddit's JS proof-of-work gate on the REAL tab (task: reddit.com shell).
//! Headless (no `$DISPLAY`, localhost only): a tiny HTTP fixture serves the
//! shape of reddit's first response — a shell page whose ONLY job is an
//! inline script that, on `DOMContentLoaded`, stamps a computed token into a
//! hidden input of a GET form and `requestSubmit()`s it. Reaching the real
//! page therefore requires the whole chain:
//!
//! 1. `DOMContentLoaded` fires after the document's scripts (the shell
//!    registers its listener with `{ once: true }` at parse time),
//! 2. `document.forms[0]` / `form.elements.namedItem('solution')` resolve
//!    against the LIVE document,
//! 3. a script-set `input.value` is what submits (`requestSubmit` must read
//!    the live entry list, not the parsed markup),
//! 4. the GET submission resolves to a URL carrying `solution=<token>`, and
//! 5. the QUEUED JS navigation actually runs (no click, no user gesture) —
//!    the server answers that URL with the real page, whose own script only
//!    reads `POW-OK` when `location.search` carries the token.
//!
//! Anything missing ships the failing reading in the title (`POW-BAD:…`).
//!
//! Run with: `cargo test -p ironsurf-tab --test pow_shell`

use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use ironsurf_common::{read_frame, write_frame, FromTab, ToTab};

const DEADLINE: Duration = Duration::from_secs(60);
/// What the shell's script computes: the same "double the token" shape as
/// reddit's PoW (the answer is derived from the shell's own state, so a
/// hard-coded form value could never satisfy it).
const TOKEN: &str = "1ef2afb3b1e30e02";

/// The shell: one inline script, one GET form, no other way forward.
const SHELL: &str = r#"<!doctype html>
<html>
  <head><title>Just a moment...</title></head>
  <body style="margin:0;background:#ffffff">
    <form method="GET" action="/r/LocalLLaMA/">
      <input type="hidden" name="js_challenge" value="1">
      <input type="hidden" name="solution" value="">
    </form>
    <script>
      addEventListener("DOMContentLoaded", function () {
        var form = document.forms[0];
        var input = form.elements.namedItem("solution");
        // Promise turn, exactly like the real shell's `delay()`ed compute.
        Promise.resolve().then(function () {
          input.value = "1ef2afb3b1e30e02" + "1ef2afb3b1e30e02";
          form.requestSubmit();
        });
      }, { once: true });
    </script>
  </body>
</html>
"#;

/// The gatekeeper: only `solution=<token doubled>` gets the real page.
fn serve(path: &str) -> Option<(&'static str, String)> {
    let (path, query) = match path.split_once('?') {
        Some((path, query)) => (path, query),
        None => (path, ""),
    };
    match path {
        "/" | "/shell" => Some(("text/html; charset=utf-8", SHELL.to_string())),
        "/r/LocalLLaMA/" => {
            let solved = query
                .split('&')
                .any(|pair| pair == format!("solution={TOKEN}{TOKEN}"));
            let body = if solved {
                format!(
                    r#"<!doctype html><html><head><title>POW-OK</title></head>
                    <body style="margin:0;background:#ffffff"><h1>feed</h1>
                    <script>
                      document.title = location.search.indexOf("solution={TOKEN}{TOKEN}") >= 0
                        ? "POW-OK" : ("POW-BAD:" + location.search);
                    </script></body></html>"#
                )
            } else {
                // No token: the gate answers the shell again (a browser would
                // loop); the title stays `Just a moment...` and the test's
                // wait times out naming it.
                format!(
                    r#"<!doctype html><html><head><title>POW-BAD:{query}</title></head>
                    <body style="margin:0;background:#ffffff"><p>gate</p></body></html>"#
                )
            };
            Some(("text/html; charset=utf-8", body))
        }
        _ => None,
    }
}

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
            let target = head
                .split_whitespace()
                .nth(1)
                .unwrap_or("/")
                .to_string();
            match serve(&target) {
                Some((content_type, body)) => {
                    let response = format!(
                        "HTTP/1.0 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.write_all(response.as_bytes());
                }
                None => {
                    let _ =
                        stream.write_all(b"HTTP/1.0 404 Not Found\r\nContent-Length: 0\r\n\r\n");
                }
            }
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
            Err(mpsc::RecvTimeoutError::Timeout) => false,
            Err(mpsc::RecvTimeoutError::Disconnected) => false,
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

    /// Every `UrlChanged`/`Navigated`-style URL the tab reported, so the
    /// failure message can show whether the queued navigation ever ran.
    fn urls(&self) -> Vec<String> {
        self.seen
            .iter()
            .filter_map(|msg| match msg {
                FromTab::UrlChanged { url: Some(url) } => Some(url.clone()),
                _ => None,
            })
            .collect()
    }

    fn wait_title(&mut self, needle: &str) {
        let deadline = Instant::now() + DEADLINE;
        loop {
            if self.titles().iter().any(|t| t.starts_with(needle)) {
                return;
            }
            if Instant::now() >= deadline {
                let status = self.child.try_wait().ok().flatten();
                panic!(
                    "timed out needing a title starting with {needle:?} \
                     (child status: {status:?})\nseen titles: {:?}\nseen urls: {:?}",
                    self.titles(),
                    self.urls()
                );
            }
            self.pump();
        }
    }
}

fn spawn_tab(url: &str, profile_dir: &std::path::Path) -> TabChild {
    let exe = env!("CARGO_BIN_EXE_ironsurf");
    let mut child = Command::new(exe)
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .arg("--tab-id")
        .arg("97")
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

#[test]
fn the_proof_of_work_shell_submits_itself_and_reaches_the_real_page() {
    let port = spawn_local_http();
    let dir = tempfile::tempdir().expect("tempdir");
    let profile = dir.path();

    let mut tab = spawn_tab(&format!("http://127.0.0.1:{port}/shell"), profile);
    // The whole gate: DOMContentLoaded listener → live `document.forms` →
    // namedItem on `form.elements` → script-set value → requestSubmit → the
    // queued GET navigation → the server's token check → the real page's
    // own title script. Only a fully wired chain can print POW-OK.
    tab.wait_title("POW-OK");

    // The solved URL must be the one the form pointed at, with the token
    // appended by the form-entry-list algorithm.
    let solved = tab
        .urls()
        .into_iter()
        .find(|url| url.contains("solution="))
        .expect("a URL carrying the solved token");
    assert!(
        solved.contains(&format!("solution={TOKEN}{TOKEN}")),
        "solved URL should carry the doubled token, got {solved}"
    );
    assert!(
        solved.ends_with("/r/LocalLLaMA/?js_challenge=1&solution=1ef2afb3b1e30e021ef2afb3b1e30e02")
            || solved.contains("/r/LocalLLaMA/?"),
        "solved URL should target the form action, got {solved}"
    );

    // The tab stays alive after the gated navigation (the JS navigation queue
    // keeps being serviced: frames and a loading-idle keep flowing).
    let deadline = Instant::now() + DEADLINE;
    let mut saw_frame = false;
    let mut loading_done = false;
    loop {
        if saw_frame && loading_done {
            break;
        }
        if Instant::now() >= deadline {
            panic!(
                "no frame/loading-idle after the gated navigation: {:?}",
                tab.seen.len()
            );
        }
        if tab.pump() {
            let tail: Vec<&FromTab> = tab.seen.iter().rev().take(64).collect();
            saw_frame |= tail.iter().any(|msg| matches!(msg, FromTab::Frame { .. }));
            loading_done |= tail
                .iter()
                .any(|msg| matches!(msg, FromTab::LoadingChanged { loading: false }));
        }
    }

    tab.send(ToTab::Destroy);
    let _ = tab.child.wait();
}