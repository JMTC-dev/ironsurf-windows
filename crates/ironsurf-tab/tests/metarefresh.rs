//! Anubis `metarefresh` gates on the REAL tab (task: Anubis metarefresh).
//! Headless (no `$DISPLAY`, localhost only): a tiny HTTP fixture serves the
//! exact shape of a `metarefresh` challenge — a page with NO JavaScript that
//! asks the client to navigate to `pass-challenge` either through the HTTP
//! `Refresh:` response header or through `<meta http-equiv="refresh">`
//! (upstream Anubis randomly picks between the two, one per request), after
//! `difficulty + 1` seconds. Passing requires the whole chain:
//!
//! 1. the first response's `Set-Cookie` lands in the container jar,
//! 2. the delayed navigation the response/markup asked for actually fires
//!    after its delay (the tab's poll loop, not a human click),
//! 3. the challenge request carries the cookie back,
//! 4. the 302 to the real page is followed, whose own title proves success.
//!
//! Anything missing leaves the "Making sure you're not a bot!" title in place
//! and the test's wait times out naming it.
//!
//! Run with: `cargo test -p ironsurf-tab --test metarefresh`

use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use ironsurf_common::{read_frame, write_frame, FromTab, ToTab};

const DEADLINE: Duration = Duration::from_secs(60);
/// The opaque challenge the gate hands out; only a client that sends the
/// matching cookie back gets through.
const TOKEN: &str = "e37a08a73fe9f7db2862db2bc2755174c85a5ed5bca2bd808b0f83cb8eaa5228";

/// The gate page, in both Anubis flavours. The header flavour carries the
/// directive out-of-band; the meta flavour has no JS at all, only the tag.
fn gate_page(flavour: &str) -> String {
    let target = format!("/pass-challenge?challenge={TOKEN}&id=01a0c7a6-84a7-71f0-a121-af61522db6b7&redir=%2F");
    match flavour {
        "header" => format!(
            r#"<!doctype html><html lang="en"><head><title>Making sure you&#39;re not a bot!</title></head>
<body><h1 id="title">Making sure you&#39;re not a bot!</h1><p id="status">Loading...</p></body></html>"#
        ),
        _ => format!(
            r#"<!doctype html><html lang="en"><head><title>Making sure you&#39;re not a bot!</title>
<meta http-equiv="refresh" content="1; url={target}"></head>
<body><h1 id="title">Making sure you&#39;re not a bot!</h1><p id="status">Loading...</p></body></html>"#
        ),
    }
}

/// The gatekeeper. Returns the full raw response for the request head.
fn serve(head: &str) -> String {
    let target = head.split_whitespace().nth(1).unwrap_or("/");
    let (path, query) = match target.split_once('?') {
        Some((path, query)) => (path, query),
        None => (target, ""),
    };
    let lower = head.to_ascii_lowercase();
    let has_cookie = lower.contains(&format!("cookie: gate={TOKEN}"));
    match path {
        "/header" | "/meta" => {
            let body = gate_page(path.trim_start_matches('/'));
            let refresh = if path == "/header" {
                // Anubis: `<difficulty+1>; url=<pass-challenge>`.
                format!(
                    "Refresh: 1; url=/pass-challenge?challenge={TOKEN}&id=01a0c7a6-84a7-71f0-a121-af61522db6b7&redir=%2F\r\n"
                )
            } else {
                String::new()
            };
            format!(
                "HTTP/1.0 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n{refresh}Set-Cookie: gate={TOKEN}; Path=/; HttpOnly\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            )
        }
        "/pass-challenge" => {
            let solved = query
                .split('&')
                .any(|pair| pair == format!("challenge={TOKEN}"))
                && has_cookie;
            if solved {
                "HTTP/1.0 302 Found\r\nLocation: /final\r\nContent-Length: 0\r\n\r\n".to_string()
            } else {
                let body = format!(
                    r#"<!doctype html><html><head><title>GATE-BAD:{query}</title></head><body>gate</body></html>"#
                );
                format!(
                    "HTTP/1.0 403 Forbidden\r\nContent-Type: text/html\r\nContent-Length: {}\r\n\r\n{body}",
                    body.len()
                )
            }
        }
        "/final" => {
            let body = r#"<!doctype html><html><head><title>GATE-OK</title></head>
<body><h1>protected</h1></body></html>"#;
            format!(
                "HTTP/1.0 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            )
        }
        _ => "HTTP/1.0 404 Not Found\r\nContent-Length: 0\r\n\r\n".to_string(),
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
            let response = serve(&String::from_utf8_lossy(&buf));
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
            if self.titles().iter().any(|t| t == needle) {
                return;
            }
            if Instant::now() >= deadline {
                let status = self.child.try_wait().ok().flatten();
                panic!(
                    "timed out needing a title {needle:?} \
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
    let exe = env!("CARGO_BIN_EXE_ironsurf-tab");
    let mut child = Command::new(exe)
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .arg("--tab-id")
        .arg("98")
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

fn gate_is_passed(start: &str) {
    let port = spawn_local_http();
    let dir = tempfile::tempdir().expect("tempdir");
    let profile = dir.path();

    let mut tab = spawn_tab(&format!("http://127.0.0.1:{port}{start}"), profile);
    // The whole gate: Set-Cookie → delayed navigation (poll loop) → cookie
    // returned on the challenge request → 302 → the real page's title.
    tab.wait_title("GATE-OK");
    // The challenge hop (and its 302) happens inside one navigation, so the
    // address bar only ever reports the post-redirect real page.
    assert!(
        tab.urls().iter().any(|u| u.ends_with("/final")),
        "the gate must have navigated on to the real page, urls: {:?}",
        tab.urls()
    );
    tab.send(ToTab::Destroy);
    let _ = tab.child.wait();
}

#[test]
fn the_refresh_response_header_passes_the_gate() {
    gate_is_passed("/header");
}

#[test]
fn the_meta_refresh_tag_passes_the_gate() {
    gate_is_passed("/meta");
}