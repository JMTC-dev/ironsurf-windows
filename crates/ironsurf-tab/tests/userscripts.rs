//! Userscripts ride into the tab and run on matching pages (task: userscript
//! execution with per-script error attribution).
//!
//! Headless (no `$DISPLAY`, localhost only): spawns the built `ironsurf-tab`
//! against a tiny `std::net::TcpListener` HTTP fixture, ships three userscripts
//! over `ToTab::SetUserscripts`, then navigates. Asserts:
//!
//! * a script whose pattern matches the document URL **runs** (its
//!   `document.title` assignment reaches `FromTab::TitleChanged`), after the
//!   page's own scripts,
//! * a script that throws is reported as `FromTab::ConsoleLog { source:
//!   "userscript", level: "error" }` whose message **leads with the script
//!   name** (`Boom script: Error: kaput`), and the tab survives,
//! * `console.log` made by a userscript arrives as `source: "userscript"` with
//!   the `"{name}: "` prefix, while the page's own `console.log` keeps
//!   `source: "js"` with verbatim text,
//! * a script whose pattern does **not** match never runs (no side effect, no
//!   console line),
//! * an empty `ToTab::SetUserscripts` clears the set (the same page then loads
//!   with no userscript effect at all),
//! * the tab stays alive and keeps painting frames.
//!
//! Run with: `cargo test -p ironsurf-tab --test userscripts`

use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use ironsurf_common::{read_frame, write_frame, FromTab, ToTab, Userscript};

const DEADLINE: Duration = Duration::from_secs(60);

/// The page every script in this test targets. Its own `<script>` proves page
/// scripts still run first and keep their `"js"` console source.
const PAGE: &str = r#"<!doctype html>
<html>
  <head><title>Userscript target</title></head>
  <body style="margin:0;background:#ffffff">
    <p>target</p>
    <script>console.log("page-line");</script>
  </body>
</html>
"#;

const PLAIN_PAGE: &str = r#"<!doctype html>
<html><head><title>Plain</title></head><body style="margin:0;background:#ffffff"><p>plain</p></body></html>
"#;

/// A page whose own script bounces to `/plain.html` (a JS-driven redirect):
/// userscripts must run once per **document**, so on this page and again on
/// the one it redirects to.
const PAGE_REDIRECT: &str = r#"<!doctype html>
<html>
  <head><title>Redirecting</title></head>
  <body style="margin:0;background:#ffffff"><p>bye</p>
    <script>location.href = '/plain.html';</script>
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
                "/userscript-target" => http_response(200, "text/html", PAGE.as_bytes()),
                "/redirect" => http_response(200, "text/html", PAGE_REDIRECT.as_bytes()),
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

    /// Like [`TestTab::wait_for`], but only messages from `from` onwards count.
    ///
    /// `wait_for` answers from the whole log, which is wrong for a **reload of
    /// the same address**: that address is already in the log from the first
    /// visit, so it returns before the reload happened. Sleeping a fixed while
    /// instead — what step 7 below used to do — loses the reloaded document's
    /// title on a loaded machine, which is how that assert went red.
    fn wait_for_after(
        &mut self,
        what: &str,
        from: usize,
        pred: impl Fn(&FromTab) -> bool,
    ) {
        let deadline = Instant::now() + DEADLINE;
        loop {
            if self.seen[from..].iter().any(|msg| pred(msg)) {
                return;
            }
            if Instant::now() >= deadline {
                let status = self.child.try_wait().ok().flatten();
                panic!(
                    "timed out waiting for {what} after message #{from} (child status: \
                     {status:?})\nmessages since then: {:#?}",
                    &self.seen[from..]
                );
            }
            match self.events.recv_timeout(Duration::from_millis(500)) {
                Ok(msg) => self.seen.push(msg),
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    let status = self.child.wait().ok();
                    panic!(
                        "stdout closed while waiting for {what} (child status: {status:?})"
                    );
                }
            }
        }
    }

    fn console(&self, source: &str) -> Vec<(String, String)> {
        self.seen
            .iter()
            .filter_map(|msg| match msg {
                FromTab::ConsoleLog {
                    source: s,
                    level,
                    message,
                } if s == source => Some((level.clone(), message.clone())),
                _ => None,
            })
            .collect()
    }

    /// Index of the first console entry whose message contains `needle`.
    fn console_index(&self, needle: &str) -> Option<usize> {
        self.seen.iter().position(|msg| {
            matches!(msg, FromTab::ConsoleLog { message, .. } if message.contains(needle))
        })
    }

    fn frames(&self) -> usize {
        self.seen
            .iter()
            .filter(|msg| matches!(msg, FromTab::Frame { .. }))
            .count()
    }

    /// Block until strictly more frames have arrived than `before`.
    fn wait_more_frames(&mut self, before: usize) {
        let deadline = Instant::now() + DEADLINE;
        while self.frames() <= before {
            if Instant::now() >= deadline {
                panic!(
                    "no frame after the userscript run (had {}, seen {} messages)",
                    before,
                    self.seen.len()
                );
            }
            if let Ok(msg) = self.events.recv_timeout(Duration::from_millis(250)) {
                self.seen.push(msg);
            }
        }
    }
}

fn spawn_tab(url: &str, profile_dir: &std::path::Path) -> TabChild {
    let exe = env!("CARGO_BIN_EXE_ironsurf");
    let mut child = Command::new(exe)
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .arg("--tab-id")
        .arg("43")
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

/// Three scripts, one install: one matches the target page exactly and patches
/// the title, one matches everything and throws, one matches a host that is
/// never loaded.
fn three_scripts(base: &str) -> Vec<Userscript> {
    vec![
        Userscript {
            id: 1,
            name: "Title patcher".to_string(),
            // Exact full-URL pattern (the tab lowercases both sides).
            patterns: vec![format!("{base}/userscript-target")],
            code: "document.title = 'patched-by-userscript'; console.log('patcher ran');".to_string(),
            enabled: true,
        },
        Userscript {
            id: 2,
            name: "Boom script".to_string(),
            // The bare wildcard: matches every URL, throws on every page.
            patterns: vec!["*".to_string()],
            code: "console.warn('about to blow up'); throw new Error('kaput');".to_string(),
            enabled: true,
        },
        Userscript {
            id: 3,
            name: "Never runs".to_string(),
            // A host this test never resolves: this script must stay inert.
            patterns: vec!["never.example.invalid/absent".to_string()],
            code: "document.title = 'SIDE-EFFECT-NEVER'; console.log('never-ran-marker');"
                .to_string(),
            enabled: true,
        },
    ]
}

/// The whole userscript contract over one wire session: install → navigate →
/// matched script ran (title), throwing script named, unmatched script silent,
/// page scripts untouched, tab alive and painting. Then: an empty install
/// clears the set.
#[test]
fn userscripts_run_per_document_and_are_attributed_by_name() {
    let port = spawn_local_http();
    let profile = tempfile::tempdir().expect("profile dir");
    let base = format!("http://127.0.0.1:{port}");
    // The tab starts on the plain page: scripts arrive BEFORE the navigation
    // that they must apply to.
    let mut tab = spawn_tab(&format!("{base}/plain.html"), profile.path());
    tab.wait_for("first page loaded", |msg| {
        matches!(msg, FromTab::LoadingChanged { loading: false })
    });

    tab.send(ToTab::SetUserscripts {
        scripts: three_scripts(&base),
    });
    let target = format!("{base}/userscript-target");
    tab.send(ToTab::Navigate { url: target.clone() });

    // 1. The throwing script is on the wire as a userscript error that LEADS
    // with the script name (DevTools shows `[userscript] Boom script: Error: kaput`).
    tab.wait_for("userscript error", |msg| {
        matches!(msg, FromTab::ConsoleLog { source, level, message } if
            source == "userscript" && level == "error" && message.contains("Boom script"))
    });
    let (level, message) = tab
        .console("userscript")
        .into_iter()
        .find(|(level, message)| level == "error" && message.contains("Boom script"))
        .expect("the wait above proved one exists");
    assert_eq!(level, "error");
    assert!(
        message.starts_with("Boom script: "),
        "the script name must lead the message, got {message:?}"
    );
    assert!(
        message.contains("kaput"),
        "the thrown error must be readable, got {message:?}"
    );

    // 2. The matching script actually ran: its `document.title` assignment
    // reached the shell (the page's own title was "Userscript target").
    tab.wait_for("TitleChanged from the userscript", |msg| {
        matches!(msg, FromTab::TitleChanged { title: Some(t) } if t == "patched-by-userscript")
    });

    // 3. Its `console.log` arrived attributed to the script by name, and the
    // page's own `console.log` stayed a plain `"js"` entry with verbatim text.
    let user_lines = tab.console("userscript");
    assert!(
        user_lines.iter().any(|(_, m)| m == "Title patcher: patcher ran"),
        "the userscript's console line must carry its name prefix: {user_lines:?}"
    );
    assert!(
        user_lines
            .iter()
            .any(|(level, m)| level == "warn" && m == "Boom script: about to blow up"),
        "every level the userscript logs is attributed: {user_lines:?}"
    );
    let js_lines = tab.console("js");
    assert!(
        js_lines.iter().any(|(_, m)| m == "page-line"),
        "page scripts keep source \"js\" with verbatim messages: {js_lines:?}"
    );

    // 4. Page scripts run FIRST, then the userscripts (the document the scripts
    // see is the one the page built).
    let page_at = tab.console_index("page-line").expect("page line");
    let patcher_at = tab.console_index("patcher ran").expect("patcher line");
    assert!(
        page_at < patcher_at,
        "userscripts must run after the page's own scripts (page {page_at}, patcher {patcher_at})"
    );

    // 5. The non-matching script never ran: no console line, no title hijack.
    assert!(
        tab.console_index("never-ran-marker").is_none(),
        "a script whose pattern does not match must never run: {:#?}",
        tab.seen
    );
    assert!(
        !tab.seen.iter().any(|msg| matches!(
            msg,
            FromTab::TitleChanged { title: Some(t) } if t == "SIDE-EFFECT-NEVER"
        )),
        "the non-matching script must not touch the page: {:#?}",
        tab.seen
    );

    // 6. Alive and painting: the errors killed nothing, and a fresh repaint
    // still produces a frame.
    assert!(
        tab.child.try_wait().ok().flatten().is_none(),
        "a throwing userscript must never kill the tab"
    );
    let frames_before = tab.frames();
    tab.send(ToTab::Resize {
        width: 812,
        height: 612,
        scale: 1.0,
    });
    tab.wait_more_frames(frames_before);

    // 7. Empty set = clear: with the set emptied, the very same target page
    // reloads with no userscript effect at all — the patcher's title never
    // comes back and the thrower adds no new error entry.
    let user_errors_before = tab
        .console("userscript")
        .iter()
        .filter(|(level, _)| level == "error")
        .count();
    let user_lines_before = tab.console("userscript").len();
    let reloaded_from = tab.seen.len();
    tab.send(ToTab::SetUserscripts { scripts: vec![] });
    tab.send(ToTab::Navigate { url: target.clone() });
    // A positioned wait, not a sleep, and not on `UrlChanged`: this navigation
    // goes to the address the tab is already on, and an address that did not
    // change is not announced again — the reload's own evidence is its document,
    // whose title is the page's own now that the patcher is gone. Waiting for it
    // also means the console counts below are read after this document had its
    // script turn (scripts run before the title settles), not after a guess.
    tab.wait_for_after(
        "the reloaded document keeps its own title",
        reloaded_from,
        |msg| matches!(msg, FromTab::TitleChanged { title: Some(t) } if t == "Userscript target"),
    );
    thread::sleep(Duration::from_millis(250));
    while let Ok(msg) = tab.events.try_recv() {
        tab.seen.push(msg);
    }
    assert_eq!(
        tab.console("userscript").len(),
        user_lines_before,
        "an empty SetUserscripts must clear the tab-side set (no script may run)"
    );
    assert_eq!(
        tab.console("userscript")
            .iter()
            .filter(|(level, _)| level == "error")
            .count(),
        user_errors_before,
        "the cleared set must not re-report the thrower"
    );
    // (The title itself was waited for above, positioned on this reload.)
    assert!(
        tab.child.try_wait().ok().flatten().is_none(),
        "the tab is still alive after the cleared navigation"
    );
}

/// A userscript dialog is the SAME plumbing as a page dialog: the script blocks
/// where it stands until the shell answers, and the line it logs afterwards is
/// still attributed to the script by name.
#[test]
fn userscript_dialogs_block_answer_and_stay_attributed() {
    let port = spawn_local_http();
    let profile = tempfile::tempdir().expect("profile dir");
    let base = format!("http://127.0.0.1:{port}");
    let mut tab = spawn_tab(&format!("{base}/plain.html"), profile.path());
    tab.wait_for("first page loaded", |msg| {
        matches!(msg, FromTab::LoadingChanged { loading: false })
    });

    tab.send(ToTab::SetUserscripts {
        scripts: vec![Userscript {
            id: 9,
            name: "Dialog user".to_string(),
            patterns: vec!["*".to_string()],
            code: "var ok = confirm('patch the page?'); console.log('answer=' + ok);".to_string(),
            enabled: true,
        }],
    });
    tab.send(ToTab::Navigate {
        url: format!("{base}/userscript-target"),
    });

    // The userscript is now blocked inside confirm() — the shell has the modal.
    tab.wait_for("dialog request from the userscript", |msg| {
        matches!(msg, FromTab::DialogRequest { message, .. } if message == "patch the page?")
    });
    let dialog_id = tab
        .seen
        .iter()
        .find_map(|msg| match msg {
            FromTab::DialogRequest { dialog_id, .. } => Some(*dialog_id),
            _ => None,
        })
        .expect("the request id");
    // Answering must unblock the script in place, and its console line must
    // carry the script name.
    tab.send(ToTab::DialogResponse {
        dialog_id,
        confirmed: true,
        text: None,
    });
    tab.wait_for("attributed line after the dialog", |msg| {
        matches!(msg, FromTab::ConsoleLog { source, message, .. } if
            source == "userscript" && message == "Dialog user: answer=true")
    });
    assert!(
        tab.child.try_wait().ok().flatten().is_none(),
        "the tab survives its own dialog"
    );
}

/// A userscript installed for `*` applies to every document, including the one
/// reached through a JS-driven redirect: the filter runs per document URL at
/// run time, so the redirect target still gets its script.
#[test]
fn userscripts_apply_again_after_a_js_redirect() {
    let port = spawn_local_http();
    let profile = tempfile::tempdir().expect("profile dir");
    let base = format!("http://127.0.0.1:{port}");
    let mut tab = spawn_tab(&format!("{base}/plain.html"), profile.path());
    tab.wait_for("first page loaded", |msg| {
        matches!(msg, FromTab::LoadingChanged { loading: false })
    });

    tab.send(ToTab::SetUserscripts {
        scripts: vec![Userscript {
            id: 7,
            name: "Redirect counter".to_string(),
            patterns: vec!["*".to_string()],
            code: "console.log('ran on ' + location.pathname);".to_string(),
            enabled: true,
        }],
    });
    let redirect = format!("{base}/redirect");
    tab.send(ToTab::Navigate { url: redirect.clone() });
    tab.wait_for("userscript on the redirecting document", |msg| {
        matches!(msg, FromTab::ConsoleLog { source, message, .. } if
            source == "userscript" && message == "Redirect counter: ran on /redirect")
    });

    // The page's own `location.href` write is a NEW document: the script must
    // run there too, filtered against the new document URL (the JS-redirect
    // case from the contract).
    tab.wait_for("the redirect landed", |msg| {
        matches!(msg, FromTab::UrlChanged { url: Some(u) } if u == &format!("{base}/plain.html"))
    });
    tab.wait_for("userscript on the redirected document", |msg| {
        matches!(msg, FromTab::ConsoleLog { source, message, .. } if
            source == "userscript" && message == "Redirect counter: ran on /plain.html")
    });
    assert!(
        tab.child.try_wait().ok().flatten().is_none(),
        "the tab survives the redirect"
    );
}

/// Every DOM change a userscript makes must land in the REAL document and be
/// readable through the engine's own APIs afterwards: `document.write` parses
/// into the live tree, whole-string `element.style = "…"` becomes the `style`
/// content attribute, and `document.dir` mirrors the `<html>` attribute. The
/// script verifies its own mutations through the DOM (the same natives the
/// engine exposes), and the verdicts ride the wire as console lines.
#[test]
fn userscript_dom_mutations_reach_the_live_document() {
    let port = spawn_local_http();
    let profile = tempfile::tempdir().expect("profile dir");
    let base = format!("http://127.0.0.1:{port}");
    let mut tab = spawn_tab(&format!("{base}/plain.html"), profile.path());
    tab.wait_for("first page loaded", |msg| {
        matches!(msg, FromTab::LoadingChanged { loading: false })
    });

    tab.send(ToTab::SetUserscripts {
        scripts: vec![Userscript {
            id: 8,
            name: "DOM reflector".to_string(),
            patterns: vec![format!("{base}/reflect")],
            code: r##"
                document.write("<p id=\"us-written\">written-by-userscript</p>");
                var w = document.querySelector("#us-written");
                console.log("written:" + (w === null ? "MISSING" : w.textContent));
                document.body.style = "color: rgb(1, 2, 3)";
                console.log("style:" + (document.body.getAttribute("style") === "color: rgb(1, 2, 3)" ? "applied" : "MISSING"));
                document.dir = "rtl";
                console.log("dir:" + document.documentElement.getAttribute("dir"));
            "##
            .to_string(),
            enabled: true,
        }],
    });
    tab.send(ToTab::Navigate {
        url: format!("{base}/reflect"),
    });
    for expected in [
        "DOM reflector: written:written-by-userscript",
        "DOM reflector: style:applied",
        "DOM reflector: dir:rtl",
    ] {
        tab.wait_for("reflected DOM mutation line", |msg| {
            matches!(msg, FromTab::ConsoleLog { source, message, .. } if
                source == "userscript" && message == expected)
        });
    }
    let frames = tab.frames();
    assert!(
        frames > 0,
        "the tab paints after the userscript mutated the DOM: {frames} frames"
    );
    assert!(
        tab.child.try_wait().ok().flatten().is_none(),
        "the tab survives its own DOM writes"
    );
}
