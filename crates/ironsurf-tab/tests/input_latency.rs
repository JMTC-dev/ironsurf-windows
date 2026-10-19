//! Input responsiveness: a click must be acted on immediately, even while the
//! mouse is travelling across the page.
//!
//! A real pointer emits a `ToTab::Pointer` move per device packet. Blitz
//! hit-tests (`resolve`) and vello_cpu repaints are expensive (hundreds of ms
//! on this software-only box), so processing every superseded move — and
//! repainting for hovers that changed nothing — used to bury the click that
//! follows under ~20 s of stale frames. The tab now (a) collapses runs of
//! moves to the newest position per stdin batch (`runtime::coalesce_moves`)
//! and (b) skips the repaint when a bare hover move lands on the same element.
//!
//! Headless: spawns the real `ironsurf-tab` and speaks the IPC contract.

use std::io::BufReader;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use ironsurf_common::{
    read_frame, write_frame, FromTab, MouseButton, PointerEvent, PointerKind, ToTab,
};

/// Page one: three chunky block links separated by gaps over a tall body, so a
/// mouse crossing the page transitions hover between link/gap/link repeatedly.
const PAGE1: &str = r#"<!doctype html>
<html><head><title>One</title></head>
<body style="margin:0;background:#ffffff">
<a href="page2.html" style="display:block;height:100px">one</a>
<div style="height:60px">gap</div>
<a href="page3.html" style="display:block;height:100px">two</a>
<div style="height:60px">gap</div>
<a href="page4.html" style="display:block;height:100px">three</a>
</body></html>"#;

const PAGE2: &str = r#"<!doctype html><html><head><title>Two</title></head><body style="margin:0;background:#00ff00"></body></html>"#;

/// A click must be acknowledged this quickly (locally ~1 s; before the fix the
/// same gesture took ~20 s). Generous for a loaded CI box, but far inside the
/// old worst case, so a regression of that shape still fails.
const CLICK_ACK_BUDGET: Duration = Duration::from_millis(2500);

struct TabChild {
    child: Child,
    stdin: ChildStdin,
    events: Receiver<FromTab>,
    frames: usize,
}

impl Drop for TabChild {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl TabChild {
    fn send(&mut self, msg: ToTab) {
        write_frame(&mut self.stdin, &msg).expect("write ToTab frame");
    }

    /// Next message, or `None` on timeout. Frame arrivals are counted.
    fn recv(&mut self, within: Duration) -> Option<FromTab> {
        match self.events.recv_timeout(within) {
            Ok(msg) => {
                if matches!(msg, FromTab::Frame { .. }) {
                    self.frames += 1;
                }
                Some(msg)
            }
            Err(_) => None,
        }
    }

    /// Read until the tab goes `quiet` with nothing to say (hard-capped, so a
    /// chatty tab costs time but never hangs the suite).
    fn drain_quiet(&mut self, quiet: Duration) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            if self.recv(quiet).is_none() {
                return;
            }
        }
    }

    /// Wait for the next `StatusText`, ignoring everything else.
    fn next_status(&mut self, within: Duration) -> Option<Option<String>> {
        let deadline = Instant::now() + within;
        while Instant::now() < deadline {
            if let Some(FromTab::StatusText { text }) = self.recv(Duration::from_millis(200)) {
                return Some(text);
            }
        }
        None
    }
}

fn spawn_tab(fixture: &std::path::Path, profile: &std::path::Path) -> TabChild {
    let exe = env!("CARGO_BIN_EXE_ironsurf");
    let mut child = Command::new(exe)
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .arg("--tab-id")
        .arg("11")
        .arg("--container-id")
        .arg("1")
        .arg("--profile-dir")
        .arg(profile)
        .arg("--url")
        .arg(url::Url::from_file_path(fixture).expect("file url").as_str())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn ironsurf-tab");
    let stdout = child.stdout.take().expect("stdout");
    let stdin = child.stdin.take().expect("stdin");
    let (tx, events) = mpsc::channel();
    thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        while let Ok(msg) = read_frame::<_, FromTab>(&mut reader) {
            if tx.send(msg).is_err() {
                break;
            }
        }
    });
    TabChild { child, stdin, events, frames: 0 }
}

/// Write the fixtures, spawn the tab, and wait until page one is on screen
/// (`UrlChanged` + a `Frame`).
fn start_page1(dir: &std::path::Path) -> TabChild {
    let profile = dir.join("profile");
    std::fs::create_dir_all(&profile).expect("profile dir");
    std::fs::write(profile.join("page1.html"), PAGE1).expect("write page1");
    for name in ["page2.html", "page3.html", "page4.html"] {
        std::fs::write(profile.join(name), PAGE2).expect("write link target");
    }
    let page1 = profile.join("page1.html");
    let mut tab = spawn_tab(&page1, &profile);

    let deadline = Instant::now() + Duration::from_secs(60);
    let (mut got_url, mut got_frame) = (false, false);
    while !(got_url && got_frame) {
        if let Some(msg) = tab.recv(Duration::from_millis(500)) {
            if matches!(&msg, FromTab::UrlChanged { url: Some(u) } if u.ends_with("page1.html")) {
                got_url = true;
            }
            if matches!(msg, FromTab::Frame { .. }) {
                got_frame = true;
            }
        }
        assert!(Instant::now() < deadline, "page one never rendered");
    }
    // Settle: drop the load-time traffic so per-test counts start clean.
    tab.drain_quiet(Duration::from_millis(400));
    tab
}

fn ptr(x: f32, y: f32, kind: PointerKind) -> ToTab {
    ToTab::Pointer(PointerEvent { x, y, kind, button: MouseButton::Left, mods: ironsurf_common::Modifiers::NONE })
}

/// The user gesture the bug was reported against: move across the page (hover
/// transitions in and out of links the whole way), then press and release on a
/// link. The click must be acknowledged within `CLICK_ACK_BUDGET`.
#[test]
fn link_click_after_hover_flood_is_fast() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut tab = start_page1(dir.path());

    // 60 moves down the page, ~15 ms apart — a hand rolling a mouse.
    for i in 0..60 {
        let y = 20.0 + i as f32 * 8.0;
        tab.send(ptr(200.0, y, PointerKind::Move));
        thread::sleep(Duration::from_millis(15));
    }

    let t_click = Instant::now();
    tab.send(ptr(200.0, 50.0, PointerKind::Move));
    tab.send(ptr(200.0, 50.0, PointerKind::Down));
    thread::sleep(Duration::from_millis(80)); // human press-and-release
    tab.send(ptr(200.0, 50.0, PointerKind::Up));

    // The first thing a click on a link owes the user: a load has started.
    loop {
        assert!(
            t_click.elapsed() < CLICK_ACK_BUDGET,
            "click after hover flood unacknowledged after {:?} (frames seen: {})",
            t_click.elapsed(),
            tab.frames
        );
        if let Some(msg) = tab.recv(Duration::from_millis(100)) {
            match msg {
                FromTab::LoadingChanged { loading: true } => break,
                FromTab::UrlChanged { url: Some(u) } if u.ends_with("page2.html") => break,
                _ => {}
            }
        }
    }
    let ack = t_click.elapsed();

    // …and the navigation itself lands.
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(FromTab::UrlChanged { url: Some(u) }) = tab.recv(Duration::from_millis(200)) {
            assert!(u.ends_with("page2.html"), "unexpected navigation to {u}");
            break;
        }
        assert!(Instant::now() < deadline, "link click never navigated");
    }
    eprintln!("click after 60-move hover flood acknowledged in {ack:?}");
}

/// A hover move that changes nothing must not burn a frame: that is the
/// difference between "responsive" and "the click is queued behind 60 repaints".
#[test]
fn idle_hover_moves_do_not_repaint() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut tab = start_page1(dir.path());

    // Park in the gap between the second and third links (y 265), let the
    // hover transition that establishes the new hover node settle…
    tab.send(ptr(600.0, 265.0, PointerKind::Move));
    // The hover transition itself legitimately repaints once, and a vello_cpu
    // frame takes a few hundred ms — drain longer than one repaint costs so
    // the baseline is not raced.
    tab.drain_quiet(Duration::from_millis(1500));

    // …then burst 30 more moves inside that same element.
    let before = tab.frames;
    for i in 0..30 {
        tab.send(ptr(600.0 + (i % 3) as f32, 265.0, PointerKind::Move));
    }
    tab.drain_quiet(Duration::from_millis(400));
    assert_eq!(
        tab.frames - before,
        0,
        "30 hover moves inside one element repainted {} times",
        tab.frames - before
    );
}

/// Hovering a link reports where the click would go (status bar), and leaving
/// it clears that again — the visible cue that the page is interactive and not
/// a picture of one.
#[test]
fn hovering_a_link_reports_its_target() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut tab = start_page1(dir.path());

    // Park off the links first, so the link hover is a real transition.
    // Park below every link (the links end at y=420 of this 1024x768 page).
    tab.send(ptr(600.0, 600.0, PointerKind::Move));
    tab.drain_quiet(Duration::from_millis(1500));

    tab.send(ptr(200.0, 50.0, PointerKind::Move)); // onto <a href="page2.html">
    let reported = tab
        .next_status(Duration::from_secs(15))
        .flatten()
        .expect("hovering a <a> never produced a StatusText");
    assert!(
        reported.ends_with("page2.html"),
        "hover status should name the link target, got {reported:?}"
    );

    // Leaving the link clears it again.
    tab.send(ptr(600.0, 600.0, PointerKind::Move));
    let cleared = tab.next_status(Duration::from_secs(15));
    assert_eq!(
        cleared,
        Some(None),
        "leaving the link must clear the hover status text"
    );
}
