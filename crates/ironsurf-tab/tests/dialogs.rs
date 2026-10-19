//! `window.alert` / `confirm` / `prompt` on the REAL engine over real IPC
//! (task: JS dialogs).
//!
//! Spawns the built `ironsurf-tab` against a localhost fixture and plays the
//! shell side of the wire by hand:
//!
//! * every dialog is a `FromTab::DialogRequest` (kind, message, default) and
//!   the tab's script **stalls** until the matching `ToTab::DialogResponse`
//!   arrives — proven by the page's own arithmetic (`a=true c=true p=grace`)
//!   landing in `document.title` only after the last answer;
//! * `alert` returns `undefined`, `confirm` the button, `prompt` the text;
//! * a cancelled answer reads `confirm` `false` and `prompt` `null` — even
//!   when the response carries junk text;
//! * messages sent while a modal is open are **deferred, not lost**: a
//!   `Navigate` fired mid-dialog runs (in order) after the script finishes;
//! * stdin EOF mid-dialog answers cancelled (a dead shell must never hang a
//!   tab) and the tab then exits 0 on its own.
//!
//! Headless (no `$DISPLAY`, localhost only). Run with:
//! `cargo test -p ironsurf-tab --test dialogs`

use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use ironsurf_common::{read_frame, write_frame, DialogKind, FromTab, ToTab};

const DEADLINE: Duration = Duration::from_secs(60);

/// The full trio: each call must block the script until answered, and the
/// three return shapes must be exactly the web's.
const PAGE_TRIO: &str = r#"<!doctype html>
<html>
  <head><title>Trio</title></head>
  <body style="margin:0;background:#ffffff">
    <script>
      var a = alert("hey");
      var c = confirm("sure?");
      var p = prompt("name", "ada");
      document.title = "a=" + (a === undefined) + " c=" + c + " p=" + p;
    </script>
  </body>
</html>
"#;

/// The cancelled readings: `confirm` false, `prompt` null — and a second
/// script proving the page kept parsing after each dismissal.
const PAGE_CANCEL: &str = r#"<!doctype html>
<html>
  <head><title>Cancel</title></head>
  <body style="margin:0;background:#ffffff">
    <script>
      var c = confirm("cancel me");
      var p = prompt("cancel me", "dflt");
      document.title = "c=" + c + " p=" + p;
    </script>
  </body>
</html>
"#;

/// The EOF case: the shell dies mid-`prompt`.
const PAGE_EOF: &str = r#"<!doctype html>
<html>
  <head><title>EOF</title></head>
  <body style="margin:0;background:#ffffff">
    <script>
      var p = prompt("who quit", "d");
      document.title = "p=" + p;
    </script>
  </body>
</html>
"#;

/// The page a mid-dialog `Navigate` lands on (deferred-message check).
const PAGE_AFTER: &str = r#"<!doctype html>
<html><head><title>after-nav</title></head><body>later</body></html>
"#;

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
                "/" | "/index.html" => http_response(200, "text/html", PAGE_TRIO.as_bytes()),
                "/cancel" => http_response(200, "text/html", PAGE_CANCEL.as_bytes()),
                "/eof" => http_response(200, "text/html", PAGE_EOF.as_bytes()),
                "/next" => http_response(200, "text/html", PAGE_AFTER.as_bytes()),
                _ => http_response(404, "text/plain", b"no such route"),
            };
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });
    port
}

fn http_response(status: u16, content_type: &str, body: &[u8]) -> String {
    let reason = match status {
        200 => "OK",
        404 => "Not Found",
        _ => "Status",
    };
    format!(
        "HTTP/1.0 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    ) + &String::from_utf8_lossy(body)
}

struct TabChild {
    child: Child,
    stdin: Option<ChildStdin>,
    events: Receiver<FromTab>,
    seen: Vec<FromTab>,
    /// How many `DialogRequest`s `next_dialog` has already handed out.
    dialog_cursor: usize,
}

impl Drop for TabChild {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl TabChild {
    fn send(&mut self, msg: ToTab) {
        write_frame(self.stdin.as_mut().expect("stdin open"), &msg)
            .expect("write ToTab frame to tab stdin");
    }

    /// Close the pipe mid-dialog: the tab must treat it as a cancelled
    /// answer, not hang (a shell quitting mid-modal).
    fn close_stdin(&mut self) {
        drop(self.stdin.take());
    }

    /// Block until any already-queued or future `FromTab` satisfies `pred`.
    fn wait_for(&mut self, what: &str, pred: impl Fn(&FromTab) -> bool) {
        let deadline = Instant::now() + DEADLINE;
        loop {
            if self.seen.iter().any(|msg| pred(msg)) {
                return;
            }
            if Instant::now() >= deadline {
                panic!("timed out waiting for {what}; seen {:#?}", self.seen);
            }
            match self.events.recv_timeout(Duration::from_millis(500)) {
                Ok(msg) => self.seen.push(msg),
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    // The tab exited: settle with what it managed to say.
                    std::thread::sleep(Duration::from_millis(50));
                    while let Ok(msg) = self.events.try_recv() {
                        self.seen.push(msg);
                    }
                    if self.seen.iter().any(|msg| pred(msg)) {
                        return;
                    }
                    panic!("stdout closed while waiting for {what}; seen {:#?}", self.seen);
                }
            }
        }
    }

    /// The next `DialogRequest` in wire order (a cursor walks `seen`, which
    /// retains everything), waiting for it to arrive.
    fn next_dialog(&mut self, what: &str) -> (u32, DialogKind, String, Option<String>) {
        let deadline = Instant::now() + DEADLINE;
        loop {
            let requests = self
                .seen
                .iter()
                .filter_map(|msg| match msg {
                    FromTab::DialogRequest { dialog_id, kind, message, default } => {
                        Some((*dialog_id, *kind, message.clone(), default.clone()))
                    }
                    _ => None,
                })
                .collect::<Vec<_>>();
            if requests.len() > self.dialog_cursor {
                self.dialog_cursor += 1;
                let r = &requests[self.dialog_cursor - 1];
                return (r.0, r.1, r.2.clone(), r.3.clone());
            }
            assert!(Instant::now() < deadline, "timed out waiting for a dialog request ({what}); seen {:#?}", self.seen);
            match self.events.recv_timeout(Duration::from_millis(500)) {
                Ok(msg) => self.seen.push(msg),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    panic!("stdout closed waiting for a dialog request ({what})")
                }
            }
        }
    }

    /// Block until a `TitleChanged` carries exactly `title`.
    fn wait_title(&mut self, title: &str) {
        let want = title.to_string();
        self.wait_for(&format!("title {want:?}"), move |msg| {
            matches!(msg, FromTab::TitleChanged { title } if title.as_deref() == Some(want.as_str()))
        });
    }

    fn index_where(&mut self, what: &str, pred: impl Fn(&FromTab) -> bool) -> usize {
        self.wait_for(what, &pred);
        self.seen.iter().position(|msg| pred(msg)).expect("matched")
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
        stdin: Some(stdin),
        events,
        seen: Vec::new(),
        dialog_cursor: 0,
    }
}

/// The trio blocks the script in order and answers with the web's exact
/// return shapes; a `Navigate` sent mid-dialog is deferred and replayed after
/// the script finished (in order, without losing the title update).
#[test]
fn alert_confirm_prompt_block_the_script_and_answer_in_order() {
    let port = spawn_local_http();
    let profile = tempfile::tempdir().expect("profile dir");
    let url = format!("http://127.0.0.1:{port}/");
    let mut tab = spawn_tab(&url, profile.path());

    // 1. alert("hey") blocks first: a kind-alert request, no default.
    let (id, kind, message, default) = tab.next_dialog("the alert request");
    assert_eq!(kind, DialogKind::Alert, "first dialog must be the alert");
    assert_eq!(message, "hey");
    assert_eq!(default, None);
    // The tab is blocked inside `alert()`: the script's title assignment
    // (the last statement) cannot have run yet.
    assert!(
        !tab.seen.iter().any(|msg| matches!(
            msg,
            FromTab::TitleChanged { title }
                if title.as_deref().is_some_and(|t| t.starts_with("a="))
        )),
        "alert() must block the script: the title write after the trio already arrived"
    );

    // 2. confirm("sure?") — answering alert unblocks it into the next call.
    tab.send(ToTab::DialogResponse { dialog_id: id, confirmed: true, text: None });
    let (id2, kind2, message2, _) = tab.next_dialog("the confirm request");
    assert_eq!(kind2, DialogKind::Confirm);
    assert_eq!(message2, "sure?");
    assert_ne!(id2, id, "each dialog gets its own id");

    // A message fired while the modal is open must be deferred and replayed,
    // not eaten by the modal pump — and the navigation may only happen after
    // the script finished (the trio title is set first).
    tab.send(ToTab::Navigate { url: format!("http://127.0.0.1:{port}/next") });

    // 3. prompt("name", "ada") — the default rides over the wire.
    tab.send(ToTab::DialogResponse { dialog_id: id2, confirmed: true, text: None });
    let (id3, kind3, message3, default3) = tab.next_dialog("the prompt request");
    assert_eq!(kind3, DialogKind::Prompt);
    assert_eq!(message3, "name");
    assert_eq!(default3.as_deref(), Some("ada"));

    // Answer it, and the script's own arithmetic proves all three returns:
    // `alert` undefined, `confirm` true, `prompt` the answer text.
    tab.send(ToTab::DialogResponse {
        dialog_id: id3,
        confirmed: true,
        text: Some("grace".into()),
    });
    tab.wait_title("a=true c=true p=grace");

    // The deferred navigation ran afterwards: UrlChanged to /next follows the
    // trio title in wire order, and the new document takes over.
    let title_at = tab.index_where("trio title present", |msg| {
        matches!(msg, FromTab::TitleChanged { title } if title.as_deref() == Some("a=true c=true p=grace"))
    });
    let nav_at = tab.index_where("the deferred navigation", |msg| {
        matches!(msg, FromTab::UrlChanged { url } if url.as_deref().is_some_and(|u| u.ends_with("/next")))
    });
    assert!(
        nav_at > title_at,
        "the mid-dialog Navigate must be deferred: {nav_at} vs {title_at} in {:#?}",
        tab.seen
    );
    tab.wait_for("the new page's title", |msg| {
        matches!(msg, FromTab::TitleChanged { title } if title.as_deref() == Some("after-nav"))
    });
}

/// Cancelled answers read `confirm` false and `prompt` null; junk `text` on
/// a cancelled prompt is ignored, and the page keeps running after both.
#[test]
fn cancelled_dialogs_read_false_and_null() {
    let port = spawn_local_http();
    let profile = tempfile::tempdir().expect("profile dir");
    let url = format!("http://127.0.0.1:{port}/cancel");
    let mut tab = spawn_tab(&url, profile.path());

    let (id, kind, message, _) = tab.next_dialog("the confirm request");
    assert_eq!((kind, message.as_str()), (DialogKind::Confirm, "cancel me"));
    tab.send(ToTab::DialogResponse { dialog_id: id, confirmed: false, text: None });

    let (id2, kind2, _, default2) = tab.next_dialog("the prompt request");
    assert_eq!(kind2, DialogKind::Prompt);
    assert_eq!(default2.as_deref(), Some("dflt"));
    // Cancel — even with text attached, which the tab must ignore.
    tab.send(ToTab::DialogResponse {
        dialog_id: id2,
        confirmed: false,
        text: Some("junk".into()),
    });

    tab.wait_title("c=false p=null");
}

/// With the pipe closed mid-dialog, the tab answers cancelled on its own and
/// exits cleanly — a quitting shell must never hang a tab process.
#[test]
fn eof_mid_dialog_answers_cancelled_and_the_tab_exits() {
    let port = spawn_local_http();
    let profile = tempfile::tempdir().expect("profile dir");
    let url = format!("http://127.0.0.1:{port}/eof");
    let mut tab = spawn_tab(&url, profile.path());

    let (_, kind, message, default) = tab.next_dialog("the prompt request");
    assert_eq!(kind, DialogKind::Prompt);
    assert_eq!(message, "who quit");
    assert_eq!(default.as_deref(), Some("d"));

    // The shell is gone: close stdin without ever answering.
    tab.close_stdin();

    // The prompt reads cancelled and the script finished…
    tab.wait_title("p=null");
    // …and the tab exited on its own (stdin EOF ends its run loop).
    let deadline = Instant::now() + DEADLINE;
    loop {
        if let Some(status) = tab
            .child
            .try_wait()
            .expect("try_wait")
        {
            assert!(status.success(), "the tab must exit 0 after EOF: {status}");
            return;
        }
        assert!(Instant::now() < deadline, "the tab did not exit after stdin EOF");
        std::thread::sleep(Duration::from_millis(50));
    }
}
