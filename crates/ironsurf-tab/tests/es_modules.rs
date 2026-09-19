//! `<script type="module">` on the REAL tab (task: ES-module loading).
//! Headless (no `$DISPLAY`, localhost only): a tiny HTTP fixture serves a
//! page whose entry module is external (`src=/main.js`) and which
//!
//! * statically imports a **relative** dependency (`./dep.js`),
//! * imports a **bare specifier** (`"pretty"`) answered by the page's
//!   `<script type="importmap">`,
//! * runs a **dynamic** `import()` (top-level await) of a fourth file, and
//! * checks its own **`import.meta.url`**.
//!
//! All four must hold for the title to read `MODULE-OK`; anything else ships
//! the failing readings in the title so a regression names itself. This is
//! the full pipeline over the real network layer: extraction collects the
//! module, Boa parses it, the import map resolves the specifier, the tab's
//! own fetcher (cookie jar, cache, DevTools reporting) pulls every body, and
//! the graph evaluates before the microtask drain returns.
//!
//! Run with: `cargo test -p ironsurf-tab --test es_modules`

use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use ironsurf_common::{read_frame, write_frame, FromTab, ToTab};

const DEADLINE: Duration = Duration::from_secs(60);

/// The entry module: every capability the loader owes the page, checked
/// before the title is allowed to read `MODULE-OK`.
const MAIN_JS: &str = r#"import { x } from "./dep.js";
import { y } from "pretty";
const m = await import("./dyn.js");
const metaOk = import.meta.url === new URL("main.js", location.href).href;
if (x === 6 && y === 7 && m.v === 8 && metaOk) {
  document.title = "MODULE-OK";
} else {
  document.title = `MODULE-BAD:${x}/${y}/${m.v}/${metaOk}`;
}
"#;

const PAGE: &str = r#"<!doctype html>
<html>
  <head>
    <title>Modules page</title>
    <script type="importmap">{"imports": { "pretty": "/pretty.js" }}</script>
  </head>
  <body style="margin:0;background:#ffffff">
    <script type="module" src="/main.js"></script>
  </body>
</html>
"#;

/// Serves every path of the fixture graph; 404s otherwise (which must never
/// be hit — a missing module body would reject the import and prove up in
/// the title instead of silently working).
fn serve(path: &str) -> Option<(&'static str, &'static str)> {
    match path {
        "/" | "/index.html" => Some(("text/html; charset=utf-8", PAGE)),
        "/main.js" => Some(("text/javascript; charset=utf-8", MAIN_JS)),
        "/dep.js" => Some(("text/javascript; charset=utf-8", "export const x = 6;\n")),
        "/pretty.js" => Some(("text/javascript; charset=utf-8", "export const y = 7;\n")),
        "/dyn.js" => Some(("text/javascript; charset=utf-8", "export const v = 8;\n")),
        _ => None,
    }
}

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
            match serve(&path) {
                Some((content_type, body)) => {
                    let response = format!(
                        "HTTP/1.0 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.write_all(response.as_bytes());
                }
                None => {
                    let _ = stream.write_all(
                        b"HTTP/1.0 404 Not Found\r\nContent-Length: 0\r\n\r\n",
                    );
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

    /// Every `TitleChanged` observed so far, newest last.
    fn titles(&self) -> Vec<String> {
        self.seen
            .iter()
            .filter_map(|msg| match msg {
                FromTab::TitleChanged { title: Some(t) } => Some(t.clone()),
                _ => None,
            })
            .collect()
    }

    /// Pump until a title starts with `needle` — or panic, naming every
    /// title seen so the failure explains itself (`MODULE-BAD:…` does).
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
                     (child status: {status:?})\nseen titles: {:?}",
                    self.titles()
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
        .arg("96")
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
fn script_type_module_loads_resolves_and_evaluates_on_a_real_tab() {
    let port = spawn_local_http();
    let dir = tempfile::tempdir().expect("tempdir");
    let profile = dir.path();

    let mut tab = spawn_tab(
        &format!("http://127.0.0.1:{port}/index.html"),
        profile,
    );
    // The full pipeline: extraction → module parse → import map → network →
    // graph evaluation → the title only reads MODULE-OK when EVERY import
    // (relative, bare-mapped, dynamic) and `import.meta.url` came out right.
    tab.wait_title("MODULE-OK");

    // The tab stays alive and finishes its load: frames keep flowing after
    // the module turn (a wedged job queue would stop the page mid-load).
    let deadline = Instant::now() + DEADLINE;
    let mut saw_frame_after = false;
    let mut loading_done = false;
    loop {
        if saw_frame_after && loading_done {
            break;
        }
        if Instant::now() >= deadline {
            panic!("no frame/loading-idle after the module ran: {:?}", tab.seen.len());
        }
        if tab.pump() {
            // Only events appended AFTER the title count; simple and enough:
            // the frame must arrive somewhere in the tail.
            let tail: Vec<&FromTab> = tab.seen.iter().rev().take(64).collect();
            saw_frame_after = tail
                .iter()
                .any(|msg| matches!(msg, FromTab::Frame { .. }));
            loading_done = tail
                .iter()
                .any(|msg| matches!(msg, FromTab::LoadingChanged { loading: false }));
        }
    }

    tab.send(ToTab::Destroy);
    let _ = tab.child.wait();
}
