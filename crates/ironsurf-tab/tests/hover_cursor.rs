//! Hover cursor on the wire, REAL engine (task: cursor icon): the tab walks
//! the hovered element deepest-first — a computed CSS `cursor:` wins, then
//! this engine's clickability affordances, then blitz's general resolution —
//! and reports every *change* as `FromTab::CursorChanged`.
//!
//! Headless (no `$DISPLAY`, localhost only): spawns the built `ironsurf-tab`
//! against a tiny `std::net::TcpListener` HTTP fixture with a link, an
//! explicit `cursor: crosshair` zone and a plain-text paragraph, moves the
//! pointer over each, and asserts the feed:
//!
//! * the pointing hand appears over the `<a>` (the feature's headline),
//! * `cursor: crosshair` overrides everything under it,
//! * leaving the pointer out of the page reports the plain arrow again.
//!
//! Run with: `cargo test -p ironsurf-tab --test hover_cursor`

use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use ironsurf_common::{
    read_frame, write_frame, CursorIcon, FromTab, MouseButton, PointerEvent, PointerKind, ToTab,
};

const DEADLINE: Duration = Duration::from_secs(60);

/// One hover target per band of 60px: link (0–40), crosshair div (100–140),
/// plain text (180–220). Absolute positions make the geometry exact.
const PAGE: &str = r#"<!doctype html>
<html>
  <head><title>Cursors</title></head>
  <body style="margin:0;background:#ffffff">
    <a href="/target" style="position:absolute;left:10px;top:10px;width:180px;height:40px;display:block">the link</a>
    <div style="position:absolute;left:10px;top:100px;width:180px;height:40px;cursor:crosshair">crosshair zone</div>
    <p style="position:absolute;left:10px;top:180px;width:180px">some plain text</p>
  </body>
</html>"#;

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
            let response = http_response(200, "text/html", PAGE.as_bytes());
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

    fn pump(&mut self, budget: Duration) {
        let deadline = Instant::now() + budget;
        loop {
            match self.events.recv_timeout(Duration::from_millis(50)) {
                Ok(msg) => self.seen.push(msg),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if Instant::now() >= deadline {
                        return;
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            }
            if Instant::now() >= deadline {
                return;
            }
        }
    }

    fn wait_for(&mut self, what: &str, pred: impl Fn(&FromTab) -> bool) {
        let deadline = Instant::now() + DEADLINE;
        loop {
            if self.seen.iter().any(|m| pred(m)) {
                return;
            }
            if Instant::now() >= deadline {
                let status = self.child.try_wait().ok().flatten();
                panic!(
                    "timed out waiting for {what} (child status: {status:?})\n\
                     messages seen ({}): {:?}",
                    self.seen.len(),
                    self.seen
                );
            }
            self.pump(Duration::from_millis(100));
        }
    }

    /// The `CursorChanged` shapes seen so far, oldest first.
    fn cursors(&self) -> Vec<CursorIcon> {
        self.seen
            .iter()
            .filter_map(|m| match m {
                FromTab::CursorChanged { cursor } => Some(*cursor),
                _ => None,
            })
            .collect()
    }

    fn move_to(&mut self, x: f32, y: f32) {
        self.send(ToTab::Pointer(PointerEvent {
            x,
            y,
            kind: PointerKind::Move,
            button: MouseButton::Left,
            mods: ironsurf_common::Modifiers::NONE,
        }));
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
        stdin,
        events,
        seen: Vec::new(),
    }
}

#[test]
fn the_hand_appears_over_a_link_and_css_outranks_the_heuristics() {
    let dir = tempfile::tempdir().unwrap();
    let port = spawn_local_http();
    let mut tab = spawn_tab(
        &format!("http://127.0.0.1:{port}/cursors"),
        dir.path(),
    );

    tab.wait_for("Ready", |m| matches!(m, FromTab::Ready { .. }));
    tab.send(ToTab::Resize {
        width: 400,
        height: 300,
        scale: 1.0,
    });
    // The document must be loaded and painted before hit-testing means anything.
    tab.wait_for("the first frame", |m| matches!(m, FromTab::Frame { .. }));
    tab.pump(Duration::from_millis(300));

    // Over the <a>: the pointing hand — engine heuristics, no CSS said so.
    tab.move_to(60.0, 30.0);
    tab.pump(Duration::from_millis(300));
    let feed = tab.cursors();
    assert!(
        feed.contains(&CursorIcon::Pointer),
        "hovering the link must ask for the hand, feed was {feed:?}"
    );

    // Over the crosshair div: the page's own CSS beats every heuristic
    // (and it is a change away from the hand, so it arrives on the wire).
    tab.move_to(60.0, 120.0);
    tab.pump(Duration::from_millis(300));
    let feed = tab.cursors();
    assert!(
        feed.contains(&CursorIcon::Crosshair),
        "cursor: crosshair must override, feed was {feed:?}"
    );

    // Leaving the page: the arrow again (the shell's chrome owns the cursor).
    tab.send(ToTab::Pointer(PointerEvent {
        x: 60.0,
        y: 120.0,
        kind: PointerKind::Out,
        button: MouseButton::Left,
        mods: ironsurf_common::Modifiers::NONE,
    }));
    tab.pump(Duration::from_millis(300));
    let feed = tab.cursors();
    assert_eq!(
        feed.last().copied(),
        Some(CursorIcon::Default),
        "leaving the viewport must fall back to the arrow, feed was {feed:?}"
    );

    // Change detection: parking back on the link reports the hand, then a
    // second move to the very same spot adds nothing.
    let before = tab.cursors().len();
    tab.move_to(60.0, 30.0);
    tab.pump(Duration::from_millis(200));
    tab.move_to(60.0, 30.0);
    tab.pump(Duration::from_millis(300));
    let after = tab.cursors();
    assert_eq!(
        after.last().copied(),
        Some(CursorIcon::Pointer),
        "the hand again over the link, feed was {after:?}"
    );
    assert_eq!(
        after.len(),
        before + 1,
        "the identical second move must stay silent, feed was {after:?}"
    );
}
