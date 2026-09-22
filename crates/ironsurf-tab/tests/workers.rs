//! Web Workers on real OS threads, end-to-end over the tab's own IPC.
//!
//! The unit tests in `worker.rs` drive [`WorkerHost`] directly; this suite
//! drives the **built tab binary** against a loopback HTTP fixture, so what is
//! proven is the wiring a page actually depends on: `new Worker(url)` in the
//! document realm fetches its script through the tab's script backend, runs on
//! a second OS thread with its own Boa realm, and its `postMessage` replies
//! reach the document realm through the tab's poll loop (the page's
//! `onmessage` runs, `document.title` changes, the frame repaints).
//!
//! * a classic worker's script and its `importScripts` body are fetched like a
//!   `<script src>` (same-origin, same policy) and run before its first event;
//! * the worker scope is not the window: `typeof window === "undefined"`,
//!   `location.href` is the worker script URL, `name` is the option, and the
//!   realm's `crypto`/digest natives answer identically to the tab's;
//! * replies arrive as objects (`e.data` is the parsed message), arrive
//!   promptly, and are ordered with the worker's own posts;
//! * a worker built from a `blob:` URL of a `Blob` made in the document runs
//!   those bytes, and `revokeObjectURL` before construction is an error;
//! * an uncaught throw in a worker surfaces once as an `error` event on the
//!   `Worker` object, and the tab stays alive;
//! * `terminate()` stops the messages (a worker that keeps posting is not
//!   observable afterwards), `{type:"module"}` is refused by name, and
//!   navigation tears the document's workers down (the next page's
//!   `location.href`/script proves the old worker thread is gone).
//!
//! Headless: no `$DISPLAY`, localhost only.
//! Run with: `cargo test -p ironsurf-tab --test workers`

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use ironsurf_common::{read_frame, write_frame, FromTab, ToTab};

const DEADLINE: Duration = Duration::from_secs(60);

/// The worker script the classic page starts: it imports a second script, then
/// answers with everything the page cannot observe from outside the worker's
/// own realm.
const ECHO_WORKER: &str = r#"
importScripts("/worker/child.js");
function hexOf(text) {
  var out = "";
  for (var i = 0; i < text.length; i++) {
    var v = text.charCodeAt(i) & 0xFF;
    out += (v < 16 ? "0" : "") + v.toString(16);
  }
  return out;
}
onmessage = function (event) {
  postMessage({
    echo: event.data,
    child: CHILD_LOADED === true,
    windowType: typeof window,
    documentType: typeof document,
    selfIsScope: self === globalThis,
    href: location.href,
    origin: location.origin,
    name: name,
    sha: hexOf(__ironsurfSha256("abc", 0)),
    random: crypto.getRandomValues(new Uint8Array(4)).length,
    uuid: /^[0-9a-f-]{36}$/.test(crypto.randomUUID())
  });
};
postMessage("ready");
"#;

const CHILD_SCRIPT: &str = "var CHILD_LOADED = true;\n";

/// Classic worker page: starts the worker, logs its reply, names the document
/// when the reply lands (so the paint/title change proves the delivery loop).
const PAGE_CLASSIC: &str = r#"<!doctype html>
<html>
  <head><title>Classic</title></head>
  <body style="margin:0;background:#ffffff">
    <script>
      var worker = new Worker("/worker/echo.js", { name: "sidekick" });
      worker.onmessage = function (event) {
        console.log("classic:" + JSON.stringify(event.data));
        if (event.data && event.data.echo === "ping") document.title = "answered";
      };
      worker.onerror = function (event) {
        console.log("classic-error:" + event.message);
      };
      worker.postMessage("ping");
      // A second post before the reply proves the queue is FIFO, not racy.
      worker.postMessage("second");
    </script>
    <p>classic worker</p>
  </body>
</html>
"#;

/// A worker whose whole script is the bytes of a `Blob` created in the
/// document, addressed by its `blob:` object URL.
const PAGE_BLOB: &str = r#"<!doctype html>
<html>
  <head><title>Blob</title></head>
  <body style="margin:0;background:#ffffff">
    <script>
      var source = "var answer = 6 * 7; postMessage('blob:' + answer + ':' + typeof window);";
      var blob = new Blob([source], { type: "text/javascript" });
      var url = URL.createObjectURL(blob);
      console.log("blob-url:" + (url.indexOf("blob:") === 0 ? "shaped" : url));
      var worker;
      try {
        worker = new Worker(url);
      } catch (e) {
        console.log("blob-construct-threw:" + String(e && e.message || e));
        throw e;
      }
      worker.onmessage = function (event) {
        console.log("blob-" + event.data);
        URL.revokeObjectURL(url);
        var second = new Worker(url);
        second.onerror = function (event) { console.log("revoked-error:" + event.message); };
        second.onmessage = function () { console.log("revoked-should-not-run"); };
      };
      worker.onerror = function (event) { console.log("blob-error:" + event.message); };
    </script>
    <p>blob worker</p>
  </body>
</html>
"#;

/// An uncaught throw at the top level of a worker script: reported once on the
/// `Worker` object, with the tab alive afterwards (the next line logs).
const PAGE_THROW: &str = r#"<!doctype html>
<html>
  <head><title>Throw</title></head>
  <body style="margin:0;background:#ffffff">
    <script>
      var worker = new Worker("/worker/boom.js");
      worker.onerror = function (event) { console.log("throw-error:" + event.message); };
      worker.onmessage = function (event) { console.log("throw-message:" + event.data); };
      setTimeout(function () { console.log("alive-after-throw"); }, 300);
    </script>
    <p>throwing worker</p>
  </body>
</html>
"#;

const BOOM_WORKER: &str = "postMessage('before');\nnull.boom;\npostMessage('after');\n";

/// `terminate()`: a worker that posts forever goes quiet, and its `close`
/// leaves no message behind.
const PAGE_TERMINATE: &str = r#"<!doctype html>
<html>
  <head><title>Terminate</title></head>
  <body style="margin:0;background:#ffffff">
    <script>
      var worker = new Worker("/worker/ticker.js");
      var seen = 0;
      worker.onmessage = function () { seen += 1; };
      setTimeout(function () {
        worker.terminate();
        var atTerminate = seen;
        setTimeout(function () { console.log("after-terminate:" + (seen === atTerminate)); }, 400);
      }, 200);
    </script>
    <p>terminate</p>
  </body>
</html>
"#;

const TICKER_WORKER: &str = "setInterval(function () { postMessage('beat'); }, 20);\n";

/// `{ type: "module" }` is refused rather than silently run as a classic
/// script, and the page keeps working afterwards.
const PAGE_MODULE: &str = r#"<!doctype html>
<html>
  <head><title>Module</title></head>
  <body style="margin:0;background:#ffffff">
    <script>
      // Refusing a module worker is a load failure, not a constructor throw:
      // the named error arrives async on the Worker's `error` event, exactly
      // like a script that 404s, so a page's `onerror` set after `new Worker`
      // still hears it.
      var refusedWorker = new Worker("/worker/echo.js", { type: "module" });
      refusedWorker.onerror = function (event) {
        console.log("module-refused:" + String(event.message || event));
      };
      var worker = new Worker("/worker/echo.js");
      worker.onmessage = function (event) {
        if (event.data && event.data.echo === "still-works") console.log("classic-after-module");
      };
      worker.postMessage("still-works");
    </script>
    <p>module worker request</p>
  </body>
</html>
"#;

/// Control page for the navigation assertion: no worker, and a title the test
/// can wait for after leaving the terminating page.
const PLAIN_PAGE: &str = r#"<!doctype html>
<html><head><title>Plain</title></head><body style="margin:0;background:#ffffff"><p>plain</p></body></html>
"#;

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
            let response = match path.as_str() {
                "/worker/classic" => html(PAGE_CLASSIC.as_bytes()),
                "/worker/blob" => html(PAGE_BLOB.as_bytes()),
                "/worker/throw" => html(PAGE_THROW.as_bytes()),
                "/worker/terminate" => html(PAGE_TERMINATE.as_bytes()),
                "/worker/module" => html(PAGE_MODULE.as_bytes()),
                "/worker/echo.js" => script(ECHO_WORKER.as_bytes()),
                "/worker/child.js" => script(CHILD_SCRIPT.as_bytes()),
                "/worker/boom.js" => script(BOOM_WORKER.as_bytes()),
                "/worker/ticker.js" => script(TICKER_WORKER.as_bytes()),
                _ => html(PLAIN_PAGE.as_bytes()),
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

fn html(body: &[u8]) -> String {
    http_response(200, "text/html", body)
}

fn script(body: &[u8]) -> String {
    http_response(200, "text/javascript", body)
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

    fn navigate(&mut self, url: &str) {
        self.send(ToTab::Navigate { url: url.into() });
    }

    /// Block until a `console.log` message satisfies `pred` (already-seen
    /// messages count). Panics (dumping everything seen) at the deadline.
    fn wait_console(&mut self, what: &str, pred: impl Fn(&str) -> bool) -> String {
        let deadline = Instant::now() + DEADLINE;
        loop {
            if let Some(message) = self.seen.iter().find_map(|msg| match msg {
                FromTab::ConsoleLog { message, .. } if pred(message) => Some(message.clone()),
                _ => None,
            }) {
                return message;
            }
            if Instant::now() >= deadline {
                let status = self.child.try_wait().ok().flatten();
                    panic!(
                        "timed out waiting for console message {what} (child status: {status:?})\n\
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
                        "stdout closed while waiting for console message {what} \
                         (child status: {status:?})\nmessages seen ({}): {:#?}",
                        self.seen.len(),
                        self.seen
                    );
                }
            }
        }
    }

    /// Block until the document title is exactly `want`.
    fn wait_title(&mut self, want: &str) {
        let deadline = Instant::now() + DEADLINE;
        loop {
            if self
                .seen
                .iter()
                .any(|msg| matches!(msg, FromTab::TitleChanged { title } if title.as_deref() == Some(want)))
            {
                return;
            }
            if Instant::now() >= deadline {
                panic!("timed out waiting for title {want}; seen {:#?}", self.seen);
            }
            match self.events.recv_timeout(Duration::from_millis(500)) {
                Ok(msg) => self.seen.push(msg),
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    panic!("stdout closed while waiting for title {want}")
                }
            }
        }
    }
}

fn spawn_tab(url: &str, profile_dir: &Path) -> TabChild {
    let exe = env!("CARGO_BIN_EXE_ironsurf-tab");
    let mut argv: Vec<String> = vec![
        "--tab-id".into(),
        "42".into(),
        "--container-id".into(),
        "1".into(),
        "--profile-dir".into(),
        profile_dir.display().to_string(),
        "--url".into(),
        url.to_owned(),
    ];
    if std::env::var("WORKER_TEST_DEBUG").is_ok() {
        argv.push("--log".into());
        argv.push("debug".into());
    }
    let mut child = Command::new(exe)
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .args(&argv)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(
            std::fs::File::create(
                std::env::var("WORKER_TEST_LOG")
                    .map(std::path::PathBuf::from)
                    .unwrap_or_else(|_| profile_dir.join("tab-stderr.log")),
            )
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
            if tx.send(msg).is_ok() {
                continue;
            }
            break;
        }
    });

    TabChild {
        child,
        stdin,
        events,
        seen: Vec::new(),
    }
}

/// The worker's script and its `importScripts` body come off the network, run
/// on the worker's own thread, and its reply arrives in the document realm as
/// an object — with a worker scope that is not the window.
#[test]
fn a_page_worker_answers_from_its_own_thread() {
    let port = spawn_local_http();
    let profile = tempfile::tempdir().expect("profile dir");
    let base = format!("http://127.0.0.1:{port}");
    let mut tab = spawn_tab(&format!("{base}/worker/classic"), profile.path());

    // Top-level `postMessage` from the worker script, before any event.
    let ready = tab.wait_console("ready reply", |message| message.contains("\"ready\""));
    assert!(
        ready.contains("classic:"),
        "the top-level post should arrive through the same path: {ready}"
    );

    let answered = tab.wait_console("the answered reply", |message| {
        message.contains("\"echo\":\"ping\"")
    });
    // `JSON.stringify` of the object the worker posted: the reply travelled as
    // JSON and was parsed back into a real object in the document realm.
    for expected in [
        "\"child\":true",          // importScripts ran first, on that thread
        "\"windowType\":\"undefined\"", // no window in a worker scope
        "\"documentType\":\"undefined\"", // no document either
        "\"selfIsScope\":true",    // `self === globalThis` there
        &format!("\"href\":\"{base}/worker/echo.js\""), // location = the script
        &format!("\"origin\":\"http://127.0.0.1:{port}\""),
        "\"name\":\"sidekick\"", // the `name` option reached the scope
        "\"sha\":\"ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad\"",
        "\"random\":4",
        "\"uuid\":true",
    ] {
        assert!(
            answered.contains(expected),
            "worker reply should contain {expected}: {answered}"
        );
    }

    // The reply landed on the main thread: its handler repainted the document.
    tab.wait_title("answered");

    // Both posts were queued to the same worker and answered in order.
    let second = tab.wait_console("the second reply", |message| {
        message.contains("\"echo\":\"second\"")
    });
    assert!(
        tab.seen
            .iter()
            .filter_map(|msg| match msg {
                FromTab::ConsoleLog { message, .. } => Some(message.clone()),
                _ => None,
            })
            .any(|message| message.contains("\"echo\":\"ping\"")),
        "ping should have been answered before second: {second}"
    );
}

/// A `Blob` created in the document, addressed by its `blob:` URL, is a worker
/// script; revoking the URL stops it resolving, which the page sees as an error.
#[test]
fn a_blob_worker_runs_the_documents_bytes() {
    let port = spawn_local_http();
    let profile = tempfile::tempdir().expect("profile dir");
    let base = format!("http://127.0.0.1:{port}");
    let mut tab = spawn_tab(&format!("{base}/worker/blob"), profile.path());

    tab.wait_console("the object URL", |message| {
        message == "blob-url:shaped"
    });
    let ran = tab.wait_console("the blob worker's reply", |message| {
        message.contains("blob-blob:42:undefined")
    });
    assert!(ran.contains("blob:"), "{ran}");
}

/// An uncaught throw in a worker script is reported once on the `Worker`
/// object and does not take the tab down.
#[test]
fn a_throwing_worker_is_reported_and_the_tab_lives() {
    let port = spawn_local_http();
    let profile = tempfile::tempdir().expect("profile dir");
    let base = format!("http://127.0.0.1:{port}");
    let mut tab = spawn_tab(&format!("{base}/worker/throw"), profile.path());

    let reported = tab.wait_console("the worker error event", |message| {
        message.starts_with("throw-error:")
    });
    assert!(
        reported.contains("null"),
        "the throw should name what broke: {reported}"
    );
    // The script stopped at the throw: the post after it never arrives.
    tab.wait_console("a live tab afterwards", |message| {
        message == "alive-after-throw"
    });
    let stray: Vec<String> = tab
        .seen
        .iter()
        .filter_map(|msg| match msg {
            FromTab::ConsoleLog { message, .. } => Some(message.clone()),
            _ => None,
        })
        .filter(|message| message.contains("throw-message:after"))
        .collect();
    assert!(stray.is_empty(), "the worker kept running: {stray:?}");
}

/// `terminate()` silences a worker that is posting forever.
#[test]
fn terminating_a_worker_stops_its_messages() {
    let port = spawn_local_http();
    let profile = tempfile::tempdir().expect("profile dir");
    let base = format!("http://127.0.0.1:{port}");
    let mut tab = spawn_tab(&format!("{base}/worker/terminate"), profile.path());

    let quiet = tab.wait_console("no messages after terminate", |message| {
        message.starts_with("after-terminate:")
    });
    assert!(
        quiet.contains("true"),
        "the terminated worker kept posting: {quiet}"
    );
}

/// A `module` worker is refused by name (it would need a module graph in its
/// own realm), and the very next classic worker still works.
#[test]
fn a_module_worker_is_refused_not_misrun() {
    let port = spawn_local_http();
    let profile = tempfile::tempdir().expect("profile dir");
    let base = format!("http://127.0.0.1:{port}");
    let mut tab = spawn_tab(&format!("{base}/worker/module"), profile.path());

    let refused = tab.wait_console("the refusal", |message| {
        message.starts_with("module-refused:")
    });
    assert!(
        refused.contains("module"),
        "the refusal should say why: {refused}"
    );
    tab.wait_console("a classic worker after it", |message| {
        message == "classic-after-module"
    });
}

/// Navigation tears the previous document's workers down: a page with no
/// worker paints and answers, and nothing from the old page arrives afterwards.
#[test]
fn navigating_away_ends_the_documents_workers() {
    let port = spawn_local_http();
    let profile = tempfile::tempdir().expect("profile dir");
    let base = format!("http://127.0.0.1:{port}");
    let mut tab = spawn_tab(&format!("{base}/worker/terminate"), profile.path());

    // Let the ticker run, then leave the document entirely.
    thread::sleep(Duration::from_millis(400));
    tab.navigate(&format!("{base}/plain"));
    tab.wait_title("Plain");
    let before = tab.seen.len();
    thread::sleep(Duration::from_millis(300));
    let console_after: Vec<String> = tab.seen[before..]
        .iter()
        .filter_map(|msg| match msg {
            FromTab::ConsoleLog { message, .. } => Some(message.clone()),
            _ => None,
        })
        .filter(|message| message.contains("beat"))
        .collect();
    assert!(
        console_after.is_empty(),
        "a worker outlived its document: {console_after:?}"
    );
}
