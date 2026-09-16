//! Per-container User-Agent, applied in the tab process (task 3/5).
//!
//! Headless (no `$DISPLAY`, localhost only): spawns the built `ironsurf-tab`
//! against a tiny `std::net::TcpListener` fixture on 127.0.0.1 whose pages
//! **echo the request's `User-Agent` header** into the served body, and whose
//! own `<script>` ships both that echo and `navigator.userAgent` to the shell
//! as `console.log` lines. Asserts:
//!
//! * default: with no `--user-agent` the wire carries `netmod::USER_AGENT`
//!   (duplicated below — integration tests cannot link the bin's modules),
//! * launched with `--user-agent UA-TEST/1.0`: every request carries it, and
//!   `navigator.userAgent` agrees,
//! * mid-session `ToTab::SetUserAgent { Some("Second/2.0") }`: the NEXT
//!   navigation echoes the new value (no forced reload: the live document is
//!   untouched until we navigate ourselves),
//! * `SetUserAgent { None }`: the built-in default again,
//! * the tab stays alive and keeps painting throughout.
//!
//! Run with: `cargo test -p ironsurf-tab --test user_agent`

use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use ironsurf_common::{read_frame, write_frame, FromTab, ToTab};

const DEADLINE: Duration = Duration::from_secs(60);

/// `netmod::USER_AGENT` (src/netmod.rs) — duplicated here because integration
/// tests link the library, not the bin's private modules. If the constant
/// changes, this test changes with it (deliberately redundant, not derived).
const DEFAULT_UA: &str = "Mozilla/5.0 (X11; Linux x86_64; IronSurf/0.1)";

/// The page the fixture serves for every request: the server-side echo of the
/// request's `User-Agent` header sits in the body, and the page's own script
/// ships both the echo (`wire:`) and `navigator.userAgent` (`nav:`) to the
/// shell as `"js"` console lines.
fn ua_page(echoed_user_agent: &str) -> String {
    let escaped = echoed_user_agent
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    format!(
        r#"<!doctype html>
<html>
  <head><title>UA echo</title></head>
  <body style="margin:0;background:#ffffff">
    <p id="wire">UA=[{escaped}]</p>
    <script>
      console.log("wire:" + document.getElementById("wire").textContent);
      console.log("nav:" + navigator.userAgent);
    </script>
  </body>
</html>"#
    )
}

/// Minimal HTTP/1.0 fixture on 127.0.0.1 (one request per connection): every
/// response echoes the request's `User-Agent` header into the body. Returns
/// the bound port. Kept alive for the whole test process.
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
            // The header the whole test is about (HTTP headers are
            // case-insensitive; reqwest sends `User-Agent`).
            let mut echoed = String::from("<missing>");
            for line in head.lines().skip(1) {
                if let Some((name, value)) = line.split_once(':') {
                    if name.trim().eq_ignore_ascii_case("user-agent") {
                        echoed = value.trim().to_owned();
                    }
                }
            }
            let body = ua_page(&echoed);
            let response = format!(
                "HTTP/1.0 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            ) + &body;
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });
    port
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

    fn js_lines(&self) -> Vec<String> {
        self.seen
            .iter()
            .filter_map(|msg| match msg {
                FromTab::ConsoleLog { source, message, .. } if source == "js" => {
                    Some(message.clone())
                }
                _ => None,
            })
            .collect()
    }

    /// How many `"js"` console lines equal `expected` so far.
    fn js_count(&self, expected: &str) -> usize {
        self.js_lines()
            .iter()
            .filter(|line| line.as_str() == expected)
            .count()
    }

    /// Pull messages until `pred` matches one (already-seen messages count).
    fn wait_for(&mut self, what: &str, pred: impl Fn(&FromTab) -> bool) {
        let deadline = Instant::now() + DEADLINE;
        loop {
            if self.seen.iter().any(|msg| pred(msg)) {
                return;
            }
            if Instant::now() >= deadline {
                let status = self.child.try_wait().ok().flatten();
                panic!(
                    "timed out waiting for {what} (child status: {status:?})\n\
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
                        "stdout closed while waiting for {what} (child status: {status:?})\n\
                         messages seen ({}): {:#?}",
                        self.seen.len(),
                        self.seen
                    );
                }
            }
        }
    }

    fn frames(&self) -> usize {
        self.seen
            .iter()
            .filter(|msg| matches!(msg, FromTab::Frame { .. }))
            .count()
    }
}

/// Drive the read loop honestly: keep pulling events until the js-line count
/// reaches `count` (or the deadline).
fn wait_js_count(tab: &mut TabChild, expected: &str, count: usize) {
    let deadline = Instant::now() + DEADLINE;
    while tab.js_count(expected) < count {
        if Instant::now() >= deadline {
            panic!(
                "no js line {expected:?} x{count} (have {}, seen {} messages)\njs lines: {:?}",
                tab.js_count(expected),
                tab.seen.len(),
                tab.js_lines()
            );
        }
        if let Ok(msg) = tab.events.recv_timeout(Duration::from_millis(250)) {
            tab.seen.push(msg);
        }
    }
}

fn spawn_tab(url: &str, profile_dir: &std::path::Path, extra_args: &[&str]) -> TabChild {
    let exe = env!("CARGO_BIN_EXE_ironsurf-tab");
    let mut command = Command::new(exe);
    command
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .arg("--tab-id")
        .arg("44")
        .arg("--container-id")
        .arg("1")
        .arg("--profile-dir")
        .arg(profile_dir)
        .arg("--url")
        .arg(url);
    for arg in extra_args {
        command.arg(arg);
    }
    let mut child = command
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

fn wire_line(user_agent: &str) -> String {
    format!("wire:UA=[{user_agent}]")
}

fn nav_line(user_agent: &str) -> String {
    format!("nav:{user_agent}")
}

/// (a) default, (c) mid-session `SetUserAgent { Some(..) }`, (d) `None` again
/// — one wire session, three documents.
#[test]
fn user_agent_defaults_then_follows_set_user_agent() {
    let port = spawn_local_http();
    let profile = tempfile::tempdir().expect("profile dir");
    let url = format!("http://127.0.0.1:{port}/first.html");
    let mut tab = spawn_tab(&url, profile.path(), &[]);

    // (a) No flag: the FIRST request already carries the built-in default,
    // and `navigator.userAgent` agrees with the wire.
    wait_js_count(&mut tab, &wire_line(DEFAULT_UA), 1);
    wait_js_count(&mut tab, &nav_line(DEFAULT_UA), 1);

    // (c) Mid-session change: `Some(v)` becomes the effective UA. Prove it is
    // the NEXT document's request that carries it — and that nothing
    // reloaded by itself: the tab may only paint on OUR navigation.
    tab.send(ToTab::SetUserAgent {
        user_agent: Some("  Second/2.0  ".to_string()), // trimmed on the way in
    });
    tab.send(ToTab::Navigate { url: url.clone() });
    wait_js_count(&mut tab, &wire_line("Second/2.0"), 1);
    wait_js_count(&mut tab, &nav_line("Second/2.0"), 1);

    // (d) `None`: back to the built-in default — a FRESH echo, not the stale
    // (a) line (count goes to 2).
    let defaults_before = tab.js_count(&wire_line(DEFAULT_UA));
    tab.send(ToTab::SetUserAgent { user_agent: None });
    tab.send(ToTab::Navigate { url: url.clone() });
    wait_js_count(&mut tab, &wire_line(DEFAULT_UA), defaults_before + 1);
    wait_js_count(&mut tab, &nav_line(DEFAULT_UA), 2);

    assert!(
        tab.child.try_wait().ok().flatten().is_none(),
        "the tab survives its User-Agent changes"
    );
    assert!(tab.frames() > 0, "the tab keeps painting frames");
}

/// (b) `--user-agent 'UA-TEST/1.0'` at launch: the very FIRST document's
/// request carries it (no IPC round-trip needed), and `navigator.userAgent`
/// reports the same value.
#[test]
fn user_agent_flag_applies_to_the_first_request() {
    let port = spawn_local_http();
    let profile = tempfile::tempdir().expect("profile dir");
    let url = format!("http://127.0.0.1:{port}/flagged.html");
    let mut tab = spawn_tab(
        &url,
        profile.path(),
        &["--user-agent", "UA-TEST/1.0"],
    );
    wait_js_count(&mut tab, &wire_line("UA-TEST/1.0"), 1);
    wait_js_count(&mut tab, &nav_line("UA-TEST/1.0"), 1);
    // The default must NOT be on the wire at all.
    assert_eq!(
        tab.js_count(&wire_line(DEFAULT_UA)),
        0,
        "the flag replaces the default from the very first request"
    );
    assert!(
        tab.child.try_wait().ok().flatten().is_none(),
        "the flagged tab is alive"
    );
}
