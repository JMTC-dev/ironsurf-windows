//! Root-cause fixture: the `TypeError: cannot convert 'null' or 'undefined'
//! to object` that https://infosec.exchange/explore (Mastodon/Glitch) throws
//! from `packs/chevron_right-D7sFWRVh.js:1:11396` inside react-spring's rAF
//! frame loop (task #69).
//!
//! MINIMAL, SELF-CONTAINED localhost reproduction: a hand-condensed port of
//! @react-spring/core's FluidValue observer machinery (`addObserver`/
//! `flushObservers`/the `InterpolateFn` class) shipped as an **ES module** —
//! exactly the compilation mode the real `chevron_right` chunk uses. The
//! layout-effect path
//!
//! ```js
//! deps.forEach(dep => addObserver(dep, onChange))     // dep IS the interpolation
//!   -> observerAdded(n){ n==1 && each(this.inputs, ins => each(ins, v => isFluid(v) && addObserver(v, this))) }
//! ```
//!
//! runs the pack's `rt`/`observerAdded` chain verbatim. boa 0.22 evaluates
//! `this` inside the DOUBLY-NESTED arrow as `undefined` in module code (one
//! arrow level is fine; the identical classic script is fine — see the pure-
//! boa unit `boa_0_22_bug_this_is_undefined_through_nested_arrows_in_modules`
//! in `src/js.rs`), so `undefined` lands in the spring's
//! `Symbol(FluidValue.observers)` Set and the next change flush —
//! `flushObservers(spring)` ≙ the pack's `et` inside the frame loop — throws
//! the production TypeError out of `Set::forEach`.
//!
//! Headless (no `$DISPLAY`, localhost only). Run the reproduction (currently
//! pinned by `#[ignore]` — it must flip to the healthy assertion once the
//! engine bug is fixed):
//!
//! ```text
//! cargo test -p ironsurf-tab --test react_spring_observer_this -- --ignored --nocapture
//! ```

use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use ironsurf_common::{read_frame, write_frame, FromTab, ToTab};

const DEADLINE: Duration = Duration::from_secs(60);

/// The fixture: react-spring's observer wiring ported to ES-module code, with
/// the frame-loop flush driven directly. Title reports the outcome.
const FIXTURE: &str = r#"<!doctype html>
<html>
  <head><title>spring-this</title></head>
  <body style="margin:0;background:#ffffff">
    <script type="module">
// --- @react-spring/core FluidValue machinery, condensed from
// --- packs/chevron_right-D7sFWRVh.js (offsets 11340-11760, 44096) ---
const GET = Symbol.for('FluidValue.get');
const OBS = Symbol.for('FluidValue.observers');
const def = (o, k, v) => Object.defineProperty(o, k, { value: v, writable: true, configurable: true });
const each = (xs, f) => xs.forEach(f);                       // pack: N
const isFluid = x => !!(x && x[GET]);                        // pack: z
function notifyObserver(o, e) { o.eventObserved ? o.eventObserved(e) : o(e) }              // pack: $e  (throws at 1:11396)
function flushObservers(target, e) { const s = target[OBS]; s && s.forEach(o => notifyObserver(o, e)) } // pack: et
function addObserver(t, o) {                                  // pack: rt
  if (t[GET]) {
    let s = t[OBS];
    s || def(t, OBS, s = new Set);
    if (!s.has(o)) { s.add(o); t.observerAdded && t.observerAdded(s.size, o) }
  }
  return o;
}
class FluidValue { constructor(get) { if (!get && !(get = this.get)) throw Error('Unknown getter'); def(this, GET, get) } }
class InterpolateFn extends FluidValue {                      // pack: kr (chevron 43827)
  constructor(inputs) { super(() => 0); this.inputs = inputs }
  observerAdded(n) { n == 1 && each(this.inputs, ins => each(ins, v => isFluid(v) && addObserver(v, this))) }
  eventObserved(e) { flushObservers(this, e) }
}
// --- the explore-page interaction, in miniature ---
const spring = {}; def(spring, GET, () => 1);                 // a SpringImpl (fluid value)
spring._onChange = () => flushObservers(spring, { type: 'change', value: 0 });
const onChange = () => {};                                    // the useSpring subscriber f
const interp = new InterpolateFn([[spring]]);
each(new Set([interp]), dep => addObserver(dep, onChange));   // layout-effect: N(c,e=>rt(e,f))
// the rAF frame loop advancing the spring fires _onChange:
try {
  spring._onChange();
  document.title = 'SPRING-OK';
} catch (err) {
  console.error(String(err));
  document.title = 'THROW:' + err.constructor.name;
}
    </script>
  </body>
</html>
"#;

/// Serves the fixture; 404s everything else.
fn serve(path: &str) -> Option<(&'static str, &'static str)> {
    match path {
        "/" | "/index.html" => Some(("text/html; charset=utf-8", FIXTURE)),
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

    /// Wait until ANY title arrives (the fixture always sets one).
    fn wait_any_title(&mut self) -> Vec<String> {
        let deadline = Instant::now() + DEADLINE;
        loop {
            let t = self.titles();
            if t.len() >= 2 || (t.len() == 1 && t[0] != "spring-this") {
                return t;
            }
            if Instant::now() >= deadline {
                panic!("no title from the fixture; seen: {:?}", self.titles());
            }
            self.pump();
        }
    }
}

fn spawn_tab(url: &str, profile_dir: &std::path::Path) -> (TabChild, std::path::PathBuf) {
    let exe = env!("CARGO_BIN_EXE_ironsurf-tab");
    let stderr_path = profile_dir.join("tab-stderr.log");
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
        .stderr(Stdio::from(
            std::fs::File::create(&stderr_path).expect("stderr log"),
        ))
        .spawn()
        .expect("spawn ironsurf-tab");
    let stdout = child.stdout.take().expect("child stdout");
    let stdin = child.stdin.take().expect("child stdin");
    let (tx, events) = mpsc::channel();
    thread::spawn(move || {
        let mut reader = std::io::BufReader::new(stdout);
        while let Ok(msg) = read_frame::<_, FromTab>(&mut reader) {
            if tx.send(msg).is_ok() {
                continue;
            }
            break;
        }
    });
    (
        TabChild {
            child,
            stdin,
            events,
            seen: Vec::new(),
        },
        stderr_path,
    )
}

/// CURRENT-BOA BUG PIN (passes today): the ES-module react-spring port
/// registers an `undefined` observer and the flush throws the production
/// message. When the engine bug is fixed THIS test flips — invert it
/// alongside `boa_0_22_bug_this_is_undefined_through_nested_arrows_in_modules`
/// (src/js.rs) and un-ignore the health test below.
#[test]
fn boa_bug_repro_undefined_observer_throws_the_production_typeerror() {
    let port = spawn_local_http();
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut tab, stderr_path) = spawn_tab(
        &format!("http://127.0.0.1:{port}/index.html"),
        dir.path(),
    );
    let titles = tab.wait_any_title();
    let stderr_text = {
        // Let the console line reach the stderr log, then read it.
        let deadline = Instant::now() + DEADLINE;
        let mut text = String::new();
        loop {
            text = std::fs::read_to_string(&stderr_path).unwrap_or_default();
            if text.contains("cannot convert") || Instant::now() >= deadline {
                break;
            }
            tab.pump();
            thread::sleep(Duration::from_millis(100));
        }
        text
    };
    let final_title = titles.last().cloned().unwrap_or_default();
    assert_eq!(
        final_title, "THROW:TypeError",
        "fixture title must report the throw (stderr: {stderr_text})"
    );
    assert!(
        stderr_text.contains("TypeError: cannot convert 'null' or 'undefined' to object"),
        "stderr must carry the exact production TypeError (titles: {titles:?})"
    );
    let _ = tab.child.kill();
}

/// THE HEALTHY ASSERTION (currently fails on boa 0.22 — task #69's fix flips
/// the pair). Run with `-- --ignored`.
#[test]
#[ignore = "boa 0.22 bug: `this` is undefined through >=2 nested arrows in module code, so react-spring registers an undefined observer (see module docs)"]
fn spring_observer_registration_survives_the_frame_flush() {
    let port = spawn_local_http();
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut tab, _stderr) = spawn_tab(
        &format!("http://127.0.0.1:{port}/index.html"),
        dir.path(),
    );
    let deadline = Instant::now() + DEADLINE;
    loop {
        let titles = tab.titles();
        assert!(
            !titles
                .iter()
                .any(|t| t.starts_with("THROW")),
            "observers must all be callable — a THROW title means the boa bug is still present: {titles:?}"
        );
        if titles.iter().any(|t| t == "SPRING-OK") {
            break;
        }
        assert!(Instant::now() < deadline, "timed out: {:?}", tab.titles());
        tab.pump();
    }
    let _ = tab.child.kill();
}
