//! Real React on the real tab (task: ECMAScript/React support).
//!
//! Two headless, localhost-only end-to-end checks:
//!
//! 1. `real_react_19_mounts_measures_and_reconciles_on_a_live_tab` serves the
//!    **actual React 19 + react-dom IIFE bundle** committed under
//!    `tests/fixtures/react19/` (MIT; see its NOTICE) and asserts the full
//!    pipeline a browser owes React:
//!      * `createRoot(...).render()` mounts a real tree through the DOM bridge;
//!      * `useState`/`useEffect`/`useLayoutEffect` run, and the layout effect's
//!        **same-turn** `getBoundingClientRect`/`offsetWidth` read real layout
//!        (123×20/123 for a `width:123px;height:20px` box) — no post-turn wait;
//!      * React's **synthetic events** fire from `button.click()` (two clicks ⇒
//!        `n=2`), and the reconciler updates/extends a keyed list (`items=3`);
//!      * `window.innerWidth` reports the live viewport (1024), and a real
//!        `ToTab::Resize` fires the page's `resize` handler so it re-renders
//!        with the new width (`RESIZED:900`);
//!      * `Symbol`, `Object.assign` and `Promise` are present (the exact
//!        features React's runtime feature-detects).
//!
//! 2. `es6_dom_timer_surface_is_complete` pins the named requirements on a
//!    plain page: ES6 syntax (arrow/class/let-const/destructuring/spread),
//!    `Promise`/`Symbol`/`Object.assign`, the DOM mutation API, and the timers
//!    (`setTimeout`/`setInterval`/`requestAnimationFrame`).
//!
//! Run with: `cargo test -p ironsurf-tab --test react_render`

use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use ironsurf_common::{read_frame, write_frame, FromTab, ToTab};

const DEADLINE: Duration = Duration::from_secs(90);

/// The committed React 19 IIFE bundle (see `tests/fixtures/react19/NOTICE.md`).
const REACT_BUNDLE: &[u8] = include_bytes!("fixtures/react19/reactapp.js");

const PAGE: &str = r#"<!doctype html>
<html>
  <head><title>react</title></head>
  <body style="margin:0;background:#ffffff">
    <div id="root"></div>
    <script src="/reactapp.js"></script>
  </body>
</html>
"#;

/// Plain page for requirement 2; the title is the verdict.
const FEATURES_PAGE: &str = r####"<!doctype html>
<html>
  <head><title>features</title></head>
  <body style="margin:0;background:#ffffff"><div id="root"></div>
  <script>
  var missing = [];
  function need(name, ok) { if (!ok) missing.push(name); }
  // ES6 syntax
  var arrow = (x) => x * 2;
  class Base { constructor() { this.v = 1; } m() { return this.v; } }
  let block = 2; const fixed = 3;
  var { a, b } = { a: 1, b: 2 }, [c, d] = [3, 4];
  var spread = [...[1, 2], 5].length;
  need("es6", arrow(2) === 4 && new Base().m() === 1 && block + fixed === 5 &&
             a + b + c + d === 10 && spread === 3);
  need("Promise", typeof Promise === "function");
  need("Symbol", typeof Symbol === "function" && typeof Symbol.iterator === "symbol");
  need("Object.assign", Object.assign({}, { a: 1 }, { b: 2 }).b === 2);
  // DOM API
  var host = document.getElementById("root");
  var made = document.createElement("div");
  var text = document.createTextNode("x");
  made.appendChild(text);
  host.appendChild(made);
  var created = made.parentNode === host && text.nodeType === 3;
  var frag = document.createDocumentFragment();
  var li = document.createElement("span");
  frag.appendChild(li);
  host.appendChild(frag);
  var moved = li.parentNode === host && host.querySelectorAll("span").length === 1;
  host.removeChild(made);
  var removed = made.parentNode === null;
  need("dom", created && moved && removed && typeof document.body !== "undefined" &&
              typeof window.innerWidth === "number");
  need("geometry", typeof made.getBoundingClientRect === "function" &&
                   typeof made.offsetWidth === "number" &&
                   typeof made.getClientRects === "function");
  need("timers", typeof setTimeout === "function" && typeof setInterval === "function" &&
                 typeof requestAnimationFrame === "function");
  // The async half really runs.
  var raf = false, interval = false;
  requestAnimationFrame(function () { raf = true; });
  setInterval(function () { interval = true; }, 5);
  setTimeout(function () {
    need("raf", raf);
    need("interval", interval);
    document.title = missing.length ? "MISSING:" + missing.join(",") : "FEATURES-OK";
  }, 60);
  </script>
  </body>
</html>
"####;

/// Serves the fixture graph; 404s otherwise.
fn serve(path: &str) -> Option<(&'static str, &'static [u8])> {
    match path {
        "/" | "/index.html" => Some(("text/html; charset=utf-8", PAGE.as_bytes())),
        "/features.html" => Some(("text/html; charset=utf-8", FEATURES_PAGE.as_bytes())),
        "/reactapp.js" => Some(("text/javascript; charset=utf-8", REACT_BUNDLE)),
        _ => None,
    }
}

/// Minimal HTTP/1.0 fixture on 127.0.0.1 (one request per connection).
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
                    let header = format!(
                        "HTTP/1.0 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\r\n",
                        body.len()
                    );
                    let _ = stream.write_all(header.as_bytes());
                    let _ = stream.write_all(body);
                }
                None => {
                    let _ = stream.write_all(b"HTTP/1.0 404 Not Found\r\nContent-Length: 0\r\n\r\n");
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

    /// Pump until a title starts with `needle`; panic naming every title seen.
    fn wait_title(&mut self, needle: &str) -> String {
        let deadline = Instant::now() + DEADLINE;
        loop {
            if let Some(title) = self.titles().into_iter().find(|t| t.starts_with(needle)) {
                return title;
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
fn real_react_19_mounts_measures_and_reconciles_on_a_live_tab() {
    let port = spawn_local_http();
    let dir = tempfile::tempdir().expect("tempdir");
    let profile = dir.path();

    let mut tab = spawn_tab(&format!("http://127.0.0.1:{port}/index.html"), profile);
    // The page's own title only lands once React has mounted, the layout
    // effect measured the committed box, two synthetic clicks ran through
    // `onClick`, the keyed list grew, and the viewport read settled.
    let title = tab.wait_title("REACT-OK:");
    for needle in [
        "geo=123x20/123", // same-turn layout effect measurement
        "n=2",            // React synthetic onClick via el.click()
        "iw=1024",        // live window.innerWidth (default viewport)
        "items=3",        // reconciler list update
        "sym=true",       // Symbol
        "assign=true",    // Object.assign
        "promise=true",   // Promise
    ] {
        assert!(title.contains(needle), "missing {needle:?} in title {title:?}");
    }

    // A real resize must reach the page: the tab updates innerWidth, runs the
    // resize event (after layout settles) and React re-renders with the new
    // width, which the app reports as `RESIZED:<innerWidth>`.
    tab.send(ToTab::Resize {
        width: 900,
        height: 640,
        scale: 1.0,
    });
    tab.wait_title("RESIZED:900");

    tab.send(ToTab::Destroy);
    let _ = tab.child.wait();
}

#[test]
fn es6_dom_timer_surface_is_complete() {
    let port = spawn_local_http();
    let dir = tempfile::tempdir().expect("tempdir");
    let profile = dir.path();

    let mut tab = spawn_tab(&format!("http://127.0.0.1:{port}/features.html"), profile);
    // The async verdict (after a rAF + interval fired) is the pass/fail.
    tab.wait_title("FEATURES-OK");

    tab.send(ToTab::Destroy);
    let _ = tab.child.wait();
}