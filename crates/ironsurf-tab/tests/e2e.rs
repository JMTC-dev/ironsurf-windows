//! End-to-end test for the tab runtime (Blitz IPC runtime, task 7c).
//!
//! Headless (no `$DISPLAY`): spawns the built `ironsurf-tab` binary against a
//! local `file://` fixture and drives the full `ToTab`/`FromTab` protocol —
//! `Ready` → `UrlChanged` → `Frame` (non-uniform at the requested size) →
//! `TitleChanged` → `Destroy` → exit 0, all bounded by 60 s.
//!
//! Run with: `cargo test -p ironsurf-tab --test e2e`
//!
//! These are the Servo-era (task 5) shell-contract expectations, restored
//! unchanged on the Blitz runtime (task 7c): every assertion is
//! engine-agnostic IPC protocol behaviour.

use std::io::BufReader;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use ironsurf_common::{read_frame, write_frame, FromTab, ToTab};

/// Minimal page: white background, a solid red box, and a distinctive title —
/// guarantees a non-uniform frame and an exact `TitleChanged` match.
const FIXTURE: &str = r#"<!doctype html>
<html>
  <head><title>IronSurf E2E</title></head>
  <body style="margin: 0; background: #ffffff">
    <div style="width: 120px; height: 120px; background: #ff3200"></div>
    <h1 style="color: #000064; font-family: monospace">IronSurf end to end</h1>
  </body>
</html>
"#;

/// Link-navigation fixture (task 7d): page one holds a single fixed-layout
/// anchor — `margin:0`, 200×40 CSS px at the top-left, so its centre is at
/// (100, 20) in shell coordinates. Clicking it must round-trip through the
/// `NavigationProvider` hook into a real top-level navigation.
const LINK_PAGE1: &str = r#"<!doctype html>
<html>
  <head><title>Page One</title></head>
  <body style="margin: 0; background: #ffffff">
    <a href="page2.html" style="display: block; width: 200px; height: 40px; font-family: monospace; color: #000064">next</a>
  </body>
</html>
"#;

const LINK_PAGE2: &str = r#"<!doctype html>
<html>
  <head><title>Page Two</title></head>
  <body style="margin: 0; background: #00ff00"></body>
</html>
"#;

/// Task 8: a page whose inline `<script>` rewrites the title and logs to the
/// console. `document.title = …` must arrive over IPC as `TitleChanged`,
/// overriding the parsed `<title>`.
const JS_TITLE_PAGE: &str = r#"<!doctype html>
<html>
  <head><title>Base Title</title></head>
  <body style="margin: 0; background: #ffffff">
    <div style="width: 120px; height: 120px; background: #0032ff"></div>
    <script>console.log("hello-from-js"); document.title = "js-set-title";</script>
  </body>
</html>
"#;

/// Task 8: an external script body, fetched through the tab's own fetcher.
const JS_EXTERNAL_PAGE: &str = r#"<!doctype html>
<html>
  <head><title>Base Title</title><script src="title-setter.js"></script></head>
  <body style="margin: 0; background: #ffffff">
    <div style="width: 120px; height: 120px; background: #3200ff"></div>
  </body>
</html>
"#;
const JS_EXTERNAL_BODY: &str = "document.title = 'external-title';\n";

/// Task 8: a page with a syntax error in the first script and a live script
/// after it — the tab must still paint and report the title.
const JS_BROKEN_PAGE: &str = r#"<!doctype html>
<html>
  <head><title>Broken JS</title></head>
  <body style="margin: 0; background: #ffffff">
    <div style="width: 120px; height: 120px; background: #00ff32"></div>
    <script>function broken( {</script>
    <script>console.warn("second-script-ran");</script>
  </body>
</html>
"#;

/// A page whose classic script comes from a base64 `data:` URL — the payload is
/// `console.log("data-script-ran"); document.title = "data-title";`. The `src`
/// is quoted: unquoted attribute values may not contain `=`, and `,`/`=` are
/// exactly what a base64 `data:` URL is made of.
const JS_DATA_SCRIPT_PAGE: &str = r#"<!doctype html>
<html>
  <head><title>Base Title</title></head>
  <body style="margin: 0; background: #ffffff">
    <div style="width: 120px; height: 120px; background: #009632"></div>
    <script src="data:text/javascript;base64,Y29uc29sZS5sb2coImRhdGEtc2NyaXB0LXJhbiIpOyBkb2N1bWVudC50aXRsZSA9ICJkYXRhLXRpdGxlIjs="></script>
  </body>
</html>
"#;

/// A top-level navigation target: base64 of
/// `console.log("data-url-executed"); document.title = "should-not-run";`.
/// Displaying this as source is the point — executing it is the bug.
const JS_DATA_URL_TOP_LEVEL: &str =
    "data:text/javascript;base64,Y29uc29sZS5sb2coImRhdGEtdXJsLWV4ZWN1dGVkIik7IGRvY3VtZW50LnRpdGxlID0gInNob3VsZC1ub3QtcnVuIjs=";

const DEADLINE: Duration = Duration::from_secs(60);

struct TabChild {
    child: Child,
    stdin: ChildStdin,
    events: Receiver<FromTab>,
}

impl Drop for TabChild {
    fn drop(&mut self) {
        // Never leak a child on a panic/failure path.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl TabChild {
    fn send(&mut self, msg: ToTab) {
        write_frame(&mut self.stdin, &msg).expect("write ToTab frame to tab stdin");
    }

    /// Drain everything the tab emits during `window` into a fresh vec
    /// (messages already consumed by `wait_for` are not re-delivered).
    fn collect(&mut self, window: Duration) -> Vec<FromTab> {
        let mut msgs = Vec::new();
        let until = Instant::now() + window;
        loop {
            let step = until.saturating_duration_since(Instant::now());
            if step.is_zero() || Instant::now() >= until {
                return msgs;
            }
            match self
                .events
                .recv_timeout(step.min(Duration::from_millis(200)))
            {
                Ok(msg) => msgs.push(msg),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => return msgs,
            }
        }
    }

    /// Wait until *any* message seen so far (including ones consumed by
    /// earlier waits) matches `pred`; returns the first matching message.
    fn wait_for(
        &mut self,
        what: &str,
        pred: impl Fn(&FromTab) -> bool,
        deadline: Instant,
        seen: &mut Vec<FromTab>,
    ) -> FromTab {
        loop {
            if Instant::now() >= deadline {
                let status = self.child.try_wait().ok().flatten();
                panic!(
                    "timed out waiting for {what} (child status: {status:?})\n\
                     messages seen ({}): {seen:#?}",
                    seen.len()
                );
            }
            if let Some(msg) = seen.iter().find(|m| pred(m)) {
                return msg.clone();
            }
            match self.events.recv_timeout(Duration::from_millis(500)) {
                Ok(msg) => {
                    seen.push(msg.clone());
                    if pred(&msg) {
                        return msg;
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    // The tab died; give it a moment to reap, then dump.
                    let status = self.child.wait().ok();
                    panic!(
                        "stdout closed while waiting for {what} (child status: {status:?})\n\
                         messages seen ({}): {seen:#?}",
                        seen.len()
                    );
                }
            }
        }
    }
}

/// Like `wait_for` but only messages that arrive *from now on* can satisfy
/// the predicate — old traffic cannot accidentally re-match (the plain
/// `wait_for` re-scans every message ever seen).
impl TabChild {
    fn wait_for_new(
        &mut self,
        what: &str,
        pred: impl Fn(&FromTab) -> bool,
        deadline: Instant,
        seen: &mut Vec<FromTab>,
    ) -> FromTab {
        loop {
            if Instant::now() >= deadline {
                let status = self.child.try_wait().ok().flatten();
                panic!(
                    "timed out waiting for new {what} (child status: {status:?})\n\
                     messages seen ({}): {seen:#?}",
                    seen.len()
                );
            }
            match self.events.recv_timeout(Duration::from_millis(500)) {
                Ok(msg) => {
                    let matched = pred(&msg);
                    seen.push(msg.clone());
                    if matched {
                        return msg;
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    let status = self.child.wait().ok();
                    panic!(
                        "stdout closed while waiting for new {what} (child status: {status:?})\n\
                         messages seen ({}): {seen:#?}",
                        seen.len()
                    );
                }
            }
        }
    }
}

fn spawn_tab(fixture: &std::path::Path, profile_dir: &std::path::Path) -> TabChild {
    let exe = env!("CARGO_BIN_EXE_ironsurf-tab");
    let mut child = Command::new(exe)
        // Prove the fully software path: no display of any kind.
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .arg("--tab-id")
        .arg("7")
        .arg("--container-id")
        .arg("1")
        .arg("--profile-dir")
        .arg(profile_dir)
        .arg("--url")
        .arg(
            url::Url::from_file_path(fixture)
                .expect("file url")
                .as_str(),
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        // Engine logs stay out of the IPC stream (and out of test output noise).
        .stderr(std::fs::File::create(profile_dir.join("tab-stderr.log")).expect("stderr log"))
        .spawn()
        .expect("spawn ironsurf-tab");

    let stdout = child.stdout.take().expect("child stdout");
    let stdin = child.stdin.take().expect("child stdin");
    let (tx, events) = mpsc::channel();
    thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
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
    }
}

/// True if the RGBA buffer is not one flat color (sampled, cheap).
fn rgba_is_uniform(rgba: &[u8]) -> bool {
    if rgba.len() < 4 {
        return true;
    }
    let first = &rgba[..4];
    !rgba.chunks_exact(4).step_by(97).any(|px| px != first)
}

/// A frame is "rendered" if it is the right size and not a single flat color.
fn assert_rendered(msg: &FromTab, want_w: u32, want_h: u32) {
    match msg {
        FromTab::Frame {
            width,
            height,
            rgba,
            ..
        } => {
            let (width, height) = (*width, *height);
            assert_eq!((width, height), (want_w, want_h), "frame size");
            assert_eq!(
                rgba.len(),
                (width * height * 4) as usize,
                "frame byte length"
            );
            let first = &rgba[..4];
            let differs = rgba
                .chunks_exact(4)
                .step_by((width.max(1) as usize).min(97))
                .any(|px| px != first);
            assert!(
                differs,
                "frame is uniform — nothing was rendered ({width}x{height})"
            );
        }
        other => panic!("expected Frame, got {other:?}"),
    }
}

#[test]
fn tab_lifecycle_file_fixture() {
    let dir = tempfile::tempdir().expect("tempdir");
    let profile = dir.path().join("profile");
    std::fs::create_dir_all(&profile).expect("profile dir");
    // The sandbox makes the *profile dir* the only place the tab may read
    // outside its declared read-only set — so the fixture lives inside it
    // (exactly what an `--offline-fixtures` layout under the profile would do).
    let fixture = profile.join("index.html");
    std::fs::write(&fixture, FIXTURE).expect("write fixture");

    let deadline = Instant::now() + DEADLINE;
    let mut tab = spawn_tab(&fixture, &profile);
    let mut seen = Vec::new();

    // 1. Ready carries our tab id.
    let ready = tab.wait_for(
        "Ready",
        |m| matches!(m, FromTab::Ready { tab_id } if tab_id == &ironsurf_common::TabId(7)),
        deadline,
        &mut seen,
    );
    assert!(matches!(ready, FromTab::Ready { .. }));

    // 2. The fixture URL is reported.
    tab.wait_for(
        "UrlChanged(file:///…/index.html)",
        |m| match m {
            FromTab::UrlChanged { url: Some(u) } => u.ends_with("/index.html"),
            _ => false,
        },
        deadline,
        &mut seen,
    );

    // 3. A frame at the default size eventually carries real pixels (early
    // composites can be flat clears, so keep consuming frames until rendered).
    let first = tab.wait_for(
        "rendered Frame(1024x768)",
        |m| {
            matches!(m, FromTab::Frame { width, height, rgba, .. }
                if *width == 1024 && *height == 768 && !rgba_is_uniform(rgba))
        },
        deadline,
        &mut seen,
    );
    assert_rendered(&first, 1024, 768);

    // 4. The document title arrives.
    let title = tab.wait_for(
        "TitleChanged(\"IronSurf E2E\")",
        |m| matches!(m, FromTab::TitleChanged { title: Some(t) } if t == "IronSurf E2E"),
        deadline,
        &mut seen,
    );
    assert_eq!(
        title,
        FromTab::TitleChanged {
            title: Some("IronSurf E2E".into())
        }
    );

    // 5. Resize lands a frame of the new size (proves the context resize +
    // reflow → read-back path).
    tab.send(ToTab::Resize {
        width: 800,
        height: 600,
        scale: 1.0,
    });
    let resized = tab.wait_for(
        "rendered Frame(800x600) after Resize",
        |m| {
            matches!(m, FromTab::Frame { width, height, rgba, .. }
                if *width == 800 && *height == 600 && !rgba_is_uniform(rgba))
        },
        deadline,
        &mut seen,
    );
    assert_rendered(&resized, 800, 600);

    // 6. Destroy → exit 0 (within the remaining deadline).
    tab.send(ToTab::Destroy);
    let child = &mut tab.child;
    let exit_loop = Instant::now();
    loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            assert!(
                status.success(),
                "tab exited with {status:?} after Destroy (stderr log under profile)"
            );
            break;
        }
        assert!(
            exit_loop.elapsed() < DEADLINE,
            "tab did not exit within {DEADLINE:?} of Destroy"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn tab_exits_cleanly_on_stdin_eof() {
    let dir = tempfile::tempdir().expect("tempdir");
    let profile = dir.path().join("profile");
    std::fs::create_dir_all(&profile).expect("profile dir");

    let fixture = profile.join("index.html");
    std::fs::write(&fixture, FIXTURE).expect("write fixture");

    let mut child = Command::new(env!("CARGO_BIN_EXE_ironsurf-tab"))
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .arg("--tab-id")
        .arg("8")
        .arg("--container-id")
        .arg("1")
        .arg("--profile-dir")
        .arg(&profile)
        .arg("--url")
        .arg(url::Url::from_file_path(&fixture).unwrap().as_str())
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(profile.join("tab-stderr.log")).unwrap())
        .spawn()
        .expect("spawn ironsurf-tab");
    // Closing stdin (the shell hanging up) must shut the tab down like Destroy.
    drop(child.stdin.take());

    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            assert!(status.success(), "tab exited with {status:?} on EOF");
            assert!(started.elapsed() < DEADLINE);
            return;
        }
        assert!(
            started.elapsed() < DEADLINE,
            "tab did not exit within {DEADLINE:?} of stdin EOF"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Task 7d: a left-button click on an `<a>` must round-trip through the
/// `NavigationProvider` hook into a real top-level navigation — the tab
/// fetches page two and reports `UrlChanged` (plus its title) without the
/// shell ever sending a `Navigate`.
#[test]
fn tab_link_click_navigates() {
    let dir = tempfile::tempdir().expect("tempdir");
    let profile = dir.path().join("profile");
    std::fs::create_dir_all(&profile).expect("profile dir");
    let page1 = profile.join("page1.html");
    std::fs::write(&page1, LINK_PAGE1).expect("write page1");
    std::fs::write(&profile.join("page2.html"), LINK_PAGE2).expect("write page2");

    let deadline = Instant::now() + DEADLINE;
    let mut tab = spawn_tab(&page1, &profile);
    let mut seen = Vec::new();

    // Land on page one first (rendered, so layout — and hit-testing — is real).
    tab.wait_for(
        "UrlChanged(page1.html)",
        |m| matches!(m, FromTab::UrlChanged { url: Some(u) } if u.ends_with("/page1.html")),
        deadline,
        &mut seen,
    );
    tab.wait_for(
        "rendered Frame(1024x768) on page1",
        |m| {
            matches!(m, FromTab::Frame { width, height, rgba, .. }
                if *width == 1024 && *height == 768 && !rgba_is_uniform(rgba))
        },
        deadline,
        &mut seen,
    );

    // Click the centre of the 200x40 anchor at the top-left (margin:0 body,
    // default 1024x768 scale-1 viewport): CSS px == shell px (100, 20).
    let click = |kind| {
        ToTab::Pointer(ironsurf_common::PointerEvent {
            x: 100.0,
            y: 20.0,
            kind,
            button: ironsurf_common::MouseButton::Left,
        })
    };
    tab.send(click(ironsurf_common::PointerKind::Move));
    tab.send(click(ironsurf_common::PointerKind::Down));
    tab.send(click(ironsurf_common::PointerKind::Up));

    // The href ("page2.html", resolved against page1's base URL) arrives as a
    // fresh top-level navigation reported by the tab itself.
    tab.wait_for(
        "UrlChanged(page2.html) from the link click",
        |m| matches!(m, FromTab::UrlChanged { url: Some(u) } if u.ends_with("/page2.html")),
        deadline,
        &mut seen,
    );
    tab.wait_for(
        "TitleChanged(\"Page Two\")",
        |m| matches!(m, FromTab::TitleChanged { title: Some(t) } if t == "Page Two"),
        deadline,
        &mut seen,
    );

    // And back returns to page one — the message must arrive *after* Back
    // (the plain wait_for would re-match the page-1 UrlChanged from spawn).
    tab.send(ToTab::Back);
    tab.wait_for_new(
        "UrlChanged(page1.html) after Back",
        |m| matches!(m, FromTab::UrlChanged { url: Some(u) } if u.ends_with("/page1.html")),
        deadline,
        &mut seen,
    );
    // Forward reaches page two again, and history flags say so.
    tab.send(ToTab::Forward);
    tab.wait_for_new(
        "UrlChanged(page2.html) after Forward",
        |m| matches!(m, FromTab::UrlChanged { url: Some(u) } if u.ends_with("/page2.html")),
        deadline,
        &mut seen,
    );
}

/// Task 7d: pointer / keyboard / scroll events flow into blitz without
/// harming the tab — frames keep arriving afterwards, and the process stays
/// alive (headless; no display, localhost/file fixtures only).
#[test]
fn tab_survives_input_events() {
    let dir = tempfile::tempdir().expect("tempdir");
    let profile = dir.path().join("profile");
    std::fs::create_dir_all(&profile).expect("profile dir");
    let fixture = profile.join("index.html");
    std::fs::write(&fixture, FIXTURE).expect("write fixture");

    let deadline = Instant::now() + DEADLINE;
    let mut tab = spawn_tab(&fixture, &profile);
    let mut seen = Vec::new();

    tab.wait_for(
        "rendered Frame(1024x768)",
        |m| {
            matches!(m, FromTab::Frame { width, height, rgba, .. }
                if *width == 1024 && *height == 768 && !rgba_is_uniform(rgba))
        },
        deadline,
        &mut seen,
    );

    // Pointer motion + press/release over the red box, a Tab key press
    // (blitz's focus default action) with a Shift-Tab back, and wheel scrolls
    // in both delta flavours (lines and pixels).
    let pointer = |kind, x, y| {
        ToTab::Pointer(ironsurf_common::PointerEvent {
            x,
            y,
            kind,
            button: ironsurf_common::MouseButton::Left,
        })
    };
    use ironsurf_common::PointerKind;
    tab.send(pointer(PointerKind::Move, 60.0, 60.0));
    tab.send(pointer(PointerKind::Down, 60.0, 60.0));
    tab.send(pointer(PointerKind::Move, 70.0, 62.0));
    tab.send(pointer(PointerKind::Up, 70.0, 62.0));
    tab.send(ToTab::Key(ironsurf_common::KeyEvent {
        key: "Tab".into(),
        code: "Tab".into(),
        text: None,
        repeat: false,
        mods: ironsurf_common::Modifiers::NONE,
    }));
    tab.send(ToTab::Key(ironsurf_common::KeyEvent {
        key: "Tab".into(),
        code: "Tab".into(),
        text: None,
        repeat: false,
        mods: ironsurf_common::Modifiers::NONE.with_shift(),
    }));
    // Printable text input (rides on KeyDown.text in blitz beta.2).
    tab.send(ToTab::Key(ironsurf_common::KeyEvent {
        key: "h".into(),
        code: "KeyH".into(),
        text: Some("h".into()),
        repeat: false,
        mods: ironsurf_common::Modifiers::NONE,
    }));
    tab.send(ToTab::Scroll(ironsurf_common::ScrollEvent {
        x: 100.0,
        y: 100.0,
        delta_x: 0.0,
        delta_y: 1.0,
    }));
    tab.send(ToTab::Scroll(ironsurf_common::ScrollEvent {
        x: 100.0,
        y: 100.0,
        delta_x: 0.0,
        delta_y: -60.0,
    }));

    // Everything above must be processed without killing the tab, and paint
    // requests they raise keep producing frames.
    let after = tab.collect(Duration::from_millis(1500));
    let frames = after
        .iter()
        .filter(|m| matches!(m, FromTab::Frame { width, height, .. } if *width == 1024 && *height == 768))
        .count();
    assert!(
        frames >= 1,
        "no frames arrived after input events ({frames}); messages: {after:?}"
    );
    assert!(
        tab.child.try_wait().expect("try_wait").is_none(),
        "tab process died handling input events"
    );

    // It still responds to the rest of the protocol.
    tab.send(ToTab::Destroy);
    let started = Instant::now();
    loop {
        if let Some(status) = tab.child.try_wait().expect("try_wait") {
            assert!(status.success(), "tab exited with {status:?} after Destroy");
            assert!(started.elapsed() < DEADLINE);
            return;
        }
        assert!(
            started.elapsed() < DEADLINE,
            "tab did not exit within {DEADLINE:?} of Destroy"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Send `Destroy` and require a clean exit 0 (used by the task 8 JS tests).
fn exit_cleanly(tab: &mut TabChild) {
    tab.send(ToTab::Destroy);
    let started = Instant::now();
    loop {
        if let Some(status) = tab.child.try_wait().expect("try_wait") {
            assert!(status.success(), "tab exited with {status:?} after Destroy");
            assert!(
                started.elapsed() < DEADLINE,
                "tab did not exit within {DEADLINE:?} of Destroy"
            );
            return;
        }
        assert!(
            started.elapsed() < DEADLINE,
            "tab did not exit within {DEADLINE:?} of Destroy"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Task 8: `console.log` + `document.title = "js-set-title"` in an inline
/// `<script>` must surface over the IPC channel as
/// `TitleChanged("js-set-title")` (beating the parsed `<title>`) and a
/// `StatusText` carrying the console line — with the page still painting.
#[test]
fn tab_script_sets_title_over_ipc() {
    let dir = tempfile::tempdir().expect("tempdir");
    let profile = dir.path().join("profile");
    std::fs::create_dir_all(&profile).expect("profile dir");
    let fixture = profile.join("index.html");
    std::fs::write(&fixture, JS_TITLE_PAGE).expect("write fixture");

    let deadline = Instant::now() + DEADLINE;
    let mut tab = spawn_tab(&fixture, &profile);
    let mut seen = Vec::new();

    let title = tab.wait_for(
        "TitleChanged(\"js-set-title\") from the inline script",
        |m| matches!(m, FromTab::TitleChanged { title: Some(t) } if t == "js-set-title"),
        deadline,
        &mut seen,
    );
    assert_eq!(
        title,
        FromTab::TitleChanged {
            title: Some("js-set-title".into())
        }
    );
    assert!(
        !seen
            .iter()
            .any(|m| matches!(m, FromTab::TitleChanged { title: Some(t) } if t == "Base Title")),
        "a script-assigned title must win over the parsed <title>: {seen:?}"
    );

    tab.wait_for(
        "StatusText carrying the console.log output",
        |m| matches!(m, FromTab::StatusText { text: Some(t) } if t.contains("hello-from-js")),
        deadline,
        &mut seen,
    );

    tab.wait_for(
        "rendered Frame(1024x768) for the scripted page",
        |m| {
            matches!(m, FromTab::Frame { width, height, rgba, .. }
                if *width == 1024 && *height == 768 && !rgba_is_uniform(rgba))
        },
        deadline,
        &mut seen,
    );

    exit_cleanly(&mut tab);
}

/// Task 8: `<script src="…">` bodies are fetched through the tab's own fetcher
/// (same policy as the document) and executed — here, from a sibling file.
#[test]
fn tab_runs_external_script() {
    let dir = tempfile::tempdir().expect("tempdir");
    let profile = dir.path().join("profile");
    std::fs::create_dir_all(&profile).expect("profile dir");
    let fixture = profile.join("index.html");
    std::fs::write(&fixture, JS_EXTERNAL_PAGE).expect("write fixture");
    std::fs::write(profile.join("title-setter.js"), JS_EXTERNAL_BODY).expect("write script");

    let deadline = Instant::now() + DEADLINE;
    let mut tab = spawn_tab(&fixture, &profile);
    let mut seen = Vec::new();

    tab.wait_for(
        "TitleChanged(\"external-title\") from title-setter.js",
        |m| matches!(m, FromTab::TitleChanged { title: Some(t) } if t == "external-title"),
        deadline,
        &mut seen,
    );
    tab.wait_for(
        "rendered Frame(1024x768) for the external-script page",
        |m| {
            matches!(m, FromTab::Frame { width, height, rgba, .. }
                if *width == 1024 && *height == 768 && !rgba_is_uniform(rgba))
        },
        deadline,
        &mut seen,
    );

    exit_cleanly(&mut tab);
}

/// The startpage.com crash: `fetch_many` used to spawn ONE OS thread per
/// `<script src>`, and a script-heavy page under a thread limit died on
/// `EAGAIN` in `thread::Builder::spawn`. The fetcher pool is now capped
/// (`MAX_SCRIPT_FETCHERS`), so a page with far more scripts than workers must
/// still fetch and execute EVERY one — in document order. Each `sN.js` appends
/// `N,` to a shared `chain` and republishes it as the title; only document
/// order produces the exact final title, and only a surviving tab reports it.
#[test]
fn tab_fetches_many_external_scripts_in_document_order() {
    const SCRIPT_COUNT: usize = 24; // well past the 8-worker cap
    let expected: String = (1..=SCRIPT_COUNT).map(|n| format!("{n},")).collect();

    let dir = tempfile::tempdir().expect("tempdir");
    let profile = dir.path().join("profile");
    std::fs::create_dir_all(&profile).expect("profile dir");
    let tags: String = (1..=SCRIPT_COUNT)
        .map(|n| format!("<script src=\"s{n}.js\"></script>\n    "))
        .collect();
    let page = format!(
        r#"<!doctype html>
<html>
  <head><title>Base Title</title>
    <script>var chain = '';</script>
    {tags}</head>
  <body style="margin: 0; background: #ffffff">
    <div style="width: 120px; height: 120px; background: #3200ff"></div>
  </body>
</html>
"#
    );
    let fixture = profile.join("index.html");
    std::fs::write(&fixture, page).expect("write fixture");
    for n in 1..=SCRIPT_COUNT {
        std::fs::write(
            profile.join(format!("s{n}.js")),
            format!("chain += '{n},'; document.title = chain;\n"),
        )
        .expect("write script");
    }

    let deadline = Instant::now() + DEADLINE;
    let mut tab = spawn_tab(&fixture, &profile);
    let mut seen = Vec::new();

    tab.wait_for(
        "TitleChanged with all 24 script bodies executed in document order",
        |m| matches!(m, FromTab::TitleChanged { title: Some(t) } if t == &expected),
        deadline,
        &mut seen,
    );
    // Nothing beyond a clean protocol may have happened in between: an
    // out-of-order or dropped fetch would surface a wrong intermediate title,
    // and a thread-exhaustion crash would end the stream before the final one.
    tab.wait_for(
        "rendered Frame(1024x768) for the many-script page",
        |m| {
            matches!(m, FromTab::Frame { width, height, rgba, .. }
                if *width == 1024 && *height == 768 && !rgba_is_uniform(rgba))
        },
        deadline,
        &mut seen,
    );

    exit_cleanly(&mut tab);
}

/// Task 8: a `<script>` that does not even parse must not take the tab down —
/// the frame still arrives, the title still arrives, the error is reported as a
/// status text, and the protocol keeps working.
#[test]
fn tab_survives_script_syntax_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    let profile = dir.path().join("profile");
    std::fs::create_dir_all(&profile).expect("profile dir");
    let fixture = profile.join("index.html");
    std::fs::write(&fixture, JS_BROKEN_PAGE).expect("write fixture");

    let deadline = Instant::now() + DEADLINE;
    let mut tab = spawn_tab(&fixture, &profile);
    let mut seen = Vec::new();

    tab.wait_for(
        "a Frame despite the broken script",
        |m| {
            matches!(m, FromTab::Frame { width, height, rgba, .. }
                if *width == 1024 && *height == 768 && !rgba_is_uniform(rgba))
        },
        deadline,
        &mut seen,
    );
    tab.wait_for(
        "TitleChanged(\"Broken JS\") despite the broken script",
        |m| matches!(m, FromTab::TitleChanged { title: Some(t) } if t == "Broken JS"),
        deadline,
        &mut seen,
    );
    tab.wait_for(
        "StatusText reporting the JavaScript error",
        |m| matches!(m, FromTab::StatusText { text: Some(t) } if t.contains("JavaScript error")),
        deadline,
        &mut seen,
    );

    assert!(
        tab.child.try_wait().expect("try_wait").is_none(),
        "a syntax error in a page script killed the tab"
    );
    exit_cleanly(&mut tab);
}

/// Shell contract (JS-driven navigation): a `javascript:` URL typed into the
/// address bar runs as a program in the current page's JS world — here it
/// rewrites the title in place; the document URL never becomes `javascript:`.
#[test]
fn tab_executes_javascript_url_in_the_page() {
    let dir = tempfile::tempdir().expect("tempdir");
    let profile = dir.path().join("profile");
    std::fs::create_dir_all(&profile).expect("profile dir");
    let fixture = profile.join("index.html");
    std::fs::write(&fixture, FIXTURE).expect("write fixture");

    let deadline = Instant::now() + DEADLINE;
    let mut tab = spawn_tab(&fixture, &profile);
    let mut seen = Vec::new();

    tab.wait_for(
        "UrlChanged(index.html) before the program runs",
        |m| matches!(m, FromTab::UrlChanged { url: Some(u) } if u.ends_with("/index.html")),
        deadline,
        &mut seen,
    );

    tab.send(ToTab::Navigate {
        url: "javascript:document.title='pwned'; void(0)".to_string(),
    });
    let after = tab.collect(Duration::from_millis(1500));
    assert!(
        after
            .iter()
            .any(|m| matches!(m, FromTab::TitleChanged { title: Some(t) } if t == "pwned")),
        "the javascript: URL program did not run: {after:?}"
    );
    assert!(
        !after.iter().any(
            |m| matches!(m, FromTab::UrlChanged { url: Some(u) } if u.starts_with("javascript:"))
        ),
        "the tab navigated to a javascript: URL: {after:?}"
    );
    assert!(
        tab.child.try_wait().expect("try_wait").is_none(),
        "executing a javascript: URL killed the tab"
    );

    exit_cleanly(&mut tab);
}

/// …and a string completion value becomes a brand-new document at
/// `about:blank` (browser/spec behaviour), not an in-place no-op.
#[test]
fn tab_renders_javascript_url_string_result_as_a_document() {
    let dir = tempfile::tempdir().expect("tempdir");
    let profile = dir.path().join("profile");
    std::fs::create_dir_all(&profile).expect("profile dir");
    let fixture = profile.join("index.html");
    std::fs::write(&fixture, FIXTURE).expect("write fixture");

    let deadline = Instant::now() + DEADLINE;
    let mut tab = spawn_tab(&fixture, &profile);
    let mut seen = Vec::new();

    tab.wait_for(
        "UrlChanged(index.html) before the program runs",
        |m| matches!(m, FromTab::UrlChanged { url: Some(u) } if u.ends_with("/index.html")),
        deadline,
        &mut seen,
    );

    tab.send(ToTab::Navigate {
        url: "javascript:'<title>from-js</title><h1>hi</h1>'".to_string(),
    });
    tab.wait_for(
        "UrlChanged(about:blank) for the generated document",
        |m| matches!(m, FromTab::UrlChanged { url: Some(u) } if u == "about:blank"),
        deadline,
        &mut seen,
    );
    tab.wait_for(
        "TitleChanged(from-js) from the generated document",
        |m| matches!(m, FromTab::TitleChanged { title: Some(t) } if t == "from-js"),
        deadline,
        &mut seen,
    );

    exit_cleanly(&mut tab);
}

/// Shell contract (JS-driven navigation): a search-engine-style redirect
/// page — `location.href = "second.html"` from a page script — is followed
/// by the tab, pushes a history entry, and Back still returns to the
/// referring page.
const JS_REDIRECT_PUSH_PAGE: &str = r#"<!doctype html>
<html>
  <head><title>Redirecting</title></head>
  <body style="margin: 0; background: #ffffff">
    <script>location.href = "second.html";</script>
  </body>
</html>
"#;

/// `location.replace` variant: the redirecting page never stays in history.
const JS_REDIRECT_REPLACE_PAGE: &str = r#"<!doctype html>
<html>
  <head><title>Redirecting</title></head>
  <body style="margin: 0; background: #ffffff">
    <script>location.replace("final.html");</script>
  </body>
</html>
"#;

const JS_REDIRECT_TARGET: &str = r#"<!doctype html>
<html>
  <head><title>Arrived</title></head>
  <body style="margin: 0; background: #00ff00"></body>
</html>
"#;

#[test]
fn tab_follows_js_location_href_push() {
    let dir = tempfile::tempdir().expect("tempdir");
    let profile = dir.path().join("profile");
    std::fs::create_dir_all(&profile).expect("profile dir");
    std::fs::write(profile.join("second.html"), JS_REDIRECT_TARGET).expect("target");
    let fixture = profile.join("redirect-push.html");
    std::fs::write(&fixture, JS_REDIRECT_PUSH_PAGE).expect("fixture");

    let deadline = Instant::now() + DEADLINE;
    let mut tab = spawn_tab(&fixture, &profile);
    let mut seen = Vec::new();

    tab.wait_for(
        "UrlChanged(second.html) from the script's location.href",
        |m| matches!(m, FromTab::UrlChanged { url: Some(u) } if u.ends_with("/second.html")),
        deadline,
        &mut seen,
    );
    tab.wait_for(
        "TitleChanged(\"Arrived\")",
        |m| matches!(m, FromTab::TitleChanged { title: Some(t) } if t == "Arrived"),
        deadline,
        &mut seen,
    );
    // `href=` pushed an entry: Back returns to the redirecting page.
    tab.send(ToTab::Back);
    tab.wait_for_new(
        "UrlChanged back to the referring page",
        |m| matches!(m, FromTab::UrlChanged { url: Some(u) } if u.ends_with("/redirect-push.html")),
        deadline,
        &mut seen,
    );
    assert!(
        tab.child.try_wait().expect("try_wait").is_none(),
        "a js-driven redirect killed the tab"
    );
    exit_cleanly(&mut tab);
}

#[test]
fn tab_follows_js_location_replace() {
    let dir = tempfile::tempdir().expect("tempdir");
    let profile = dir.path().join("profile");
    std::fs::create_dir_all(&profile).expect("profile dir");
    std::fs::write(profile.join("final.html"), JS_REDIRECT_TARGET).expect("target");
    let fixture = profile.join("redirect-replace.html");
    std::fs::write(&fixture, JS_REDIRECT_REPLACE_PAGE).expect("fixture");

    let deadline = Instant::now() + DEADLINE;
    let mut tab = spawn_tab(&fixture, &profile);
    let mut seen = Vec::new();

    tab.wait_for(
        "UrlChanged(final.html) from location.replace",
        |m| matches!(m, FromTab::UrlChanged { url: Some(u) } if u.ends_with("/final.html")),
        deadline,
        &mut seen,
    );
    tab.wait_for(
        "TitleChanged(\"Arrived\")",
        |m| matches!(m, FromTab::TitleChanged { title: Some(t) } if t == "Arrived"),
        deadline,
        &mut seen,
    );
    // `replace` overwrote the current entry: Back cannot return to the
    // redirecting page (the entry under index 0 is the target itself).
    tab.send(ToTab::Back);
    tab.send(ToTab::Forward);
    let after = tab.collect(Duration::from_millis(800));
    assert!(
        !after.iter().any(
            |m| matches!(m, FromTab::UrlChanged { url: Some(u) } if u.ends_with("/redirect-replace.html"))
        ),
        "location.replace left the redirecting page in history: {after:?}"
    );
    exit_cleanly(&mut tab);
}

/// `data:text/javascript;base64,…` loaded by the page itself
/// (`<script src="data:…">`): the payload is RFC 2397 base64, decoded by our
/// own fetcher, and runs like any other classic script — the page opted in.
#[test]
fn tab_runs_data_url_script() {
    let dir = tempfile::tempdir().expect("tempdir");
    let profile = dir.path().join("profile");
    std::fs::create_dir_all(&profile).expect("profile dir");
    let fixture = profile.join("index.html");
    std::fs::write(&fixture, JS_DATA_SCRIPT_PAGE).expect("write fixture");

    let deadline = Instant::now() + DEADLINE;
    let mut tab = spawn_tab(&fixture, &profile);
    let mut seen = Vec::new();

    tab.wait_for(
        "TitleChanged(\"data-title\") from the data: script",
        |m| matches!(m, FromTab::TitleChanged { title: Some(t) } if t == "data-title"),
        deadline,
        &mut seen,
    );
    tab.wait_for(
        "the data: script's console.log reaches the shell",
        |m| matches!(m, FromTab::StatusText { text: Some(t) } if t.contains("data-script-ran")),
        deadline,
        &mut seen,
    );
    tab.wait_for(
        "rendered Frame(1024x768) for the data:-script page",
        |m| {
            matches!(m, FromTab::Frame { width, height, rgba, .. }
                if *width == 1024 && *height == 768 && !rgba_is_uniform(rgba))
        },
        deadline,
        &mut seen,
    );

    exit_cleanly(&mut tab);
}

/// The *same* `data:text/javascript;base64,…` typed into the URL bar is a
/// program, not a document. The tab loads it as a page (200, `UrlChanged`) and
/// displays it as inert source titled by its media type — it must never be
/// parsed as HTML or executed, which would make it a `javascript:` URL under a
/// different name (and those the tab refuses by contract).
#[test]
fn tab_shows_top_level_data_url_script_as_source() {
    let dir = tempfile::tempdir().expect("tempdir");
    let profile = dir.path().join("profile");
    std::fs::create_dir_all(&profile).expect("profile dir");
    let fixture = profile.join("index.html");
    std::fs::write(&fixture, FIXTURE).expect("write fixture");

    let deadline = Instant::now() + DEADLINE;
    let mut tab = spawn_tab(&fixture, &profile);
    let mut seen = Vec::new();

    tab.wait_for(
        "UrlChanged(index.html) before the data: navigation",
        |m| matches!(m, FromTab::UrlChanged { url: Some(u) } if u.ends_with("/index.html")),
        deadline,
        &mut seen,
    );

    // Everything the tab says about this navigation, in order: the forced first
    // frame of the new document, then its URL/title deltas, then load-finished
    // (see `runtime::load_html`), so the whole batch is collected and asserted
    // at once rather than waited for item by item.
    tab.send(ToTab::Navigate {
        url: JS_DATA_URL_TOP_LEVEL.to_string(),
    });
    let after = tab.collect(Duration::from_millis(4000));
    let any = |pred: fn(&FromTab) -> bool| after.iter().any(pred);

    assert!(
        any(|m| matches!(m, FromTab::UrlChanged { url: Some(u) } if u == JS_DATA_URL_TOP_LEVEL)),
        "the tab did not navigate to the data: URL: {after:?}"
    );
    // The source viewer titles the document by its media type …
    assert!(
        any(|m| matches!(m, FromTab::TitleChanged { title: Some(t) } if t == "text/javascript")),
        "no TitleChanged(\"text/javascript\") from the source viewer: {after:?}"
    );
    // … and it paints (a real document, not a blank or a crash).
    assert!(
        any(|m| matches!(m, FromTab::Frame { width, height, rgba, .. }
            if *width == 1024 && *height == 768 && !rgba_is_uniform(rgba))),
        "no rendered Frame for the source viewer: {after:?}"
    );
    // The program was displayed, never run — executing it would have set this
    // title and logged to the page console.
    assert!(
        !any(|m| matches!(m, FromTab::TitleChanged { title: Some(t) } if t == "should-not-run")),
        "a top-level data:text/javascript URL executed: {after:?}"
    );
    assert!(
        !any(
            |m| matches!(m, FromTab::StatusText { text: Some(t) } if t.contains("data-url-executed"))
        ),
        "a top-level data:text/javascript URL executed: {after:?}"
    );
    assert!(
        tab.child.try_wait().expect("try_wait").is_none(),
        "a data:text/javascript navigation killed the tab"
    );

    exit_cleanly(&mut tab);
}

/// Real-timers task: a callback armed with `setTimeout` at load must fire
/// from the *live* poll loop (not just headless) — the retitled page arrives
/// as `TitleChanged` seconds after the initial load settled.
#[test]
fn tab_timer_callback_fires_from_the_poll_loop() {
    let dir = tempfile::tempdir().expect("tempdir");
    let profile = dir.path().join("profile");
    std::fs::create_dir_all(&profile).expect("profile dir");
    let fixture = profile.join("index.html");
    std::fs::write(
        &fixture,
        r#"<!doctype html>
<html>
  <head><title>Base Title</title></head>
  <body>
    <script>
      setTimeout(function () {
        document.title = "timer-title";
      }, 20);
      setInterval(function () { window.__n = (window.__n || 0) + 1; }, 25);
    </script>
  </body>
</html>
"#,
    )
    .expect("write fixture");

    let deadline = Instant::now() + DEADLINE;
    let mut tab = spawn_tab(&fixture, &profile);
    let mut seen = Vec::new();

    // The parsed title must NOT be overridden instantly (that would mean the
    // callback ran synchronously); the timer title arrives via the poll loop.
    let title = tab.wait_for(
        "TitleChanged(\"timer-title\") from the setTimeout callback",
        |m| matches!(m, FromTab::TitleChanged { title: Some(t) } if t == "timer-title"),
        deadline,
        &mut seen,
    );
    assert_eq!(title, FromTab::TitleChanged { title: Some("timer-title".into()) });

    exit_cleanly(&mut tab);
}

/// The startpage.com search bar: the page's `<form>` ships ten
/// `<input type=hidden>` siblings beside the query field inside a flexed
/// layout. blitz's default UA sheet lacks `input[type=hidden] { display: none }`,
/// so the hidden controls got layout boxes, ate every flex pixel, and the
/// query input collapsed to width **0** — the bar painted (via its container)
/// but clicking it focused nothing and typing went nowhere. The tab now feeds
/// blitz's DEFAULT_CSS back plus a one-rule patch
/// (`forms::UA_PATCH_CSS`, task: startpage.com search bar); this fixture
/// mirrors startpage's real search-form markup and CSS, and the test walks the
/// user's exact moves: right-click the bar (the engine's hit test must report
/// an editable target), left-click it (focus), type, and see the glyphs paint.
const HIDDEN_INPUT_SEARCH_FORM: &str = r#"<!doctype html>
<html>
<head><title>Search bar fixture</title>
<style>
  body { margin: 0; background: #ffffff; }
  /* startpage's emotion rules, transcribed */
  .search-form-form { position: relative; display: flex; align-items: center;
    justify-content: space-between; height: 56px; background: #ffffff; }
  .search-input-container { flex: 1; padding: 10px 85px 10px 0; margin-left: 16px; width: 100%; }
  .search-form-input { font-size: 16px; width: 100%; color: #000000; border: none;
    background: transparent; outline: none; }
</style></head>
<body>
<form action="/sp/search" method="post" class="search-form-form" id="search">
  <div class="search-input-container"><input aria-label="search" id="q" maxlength="2048"
    name="query" placeholder="Search privately" spellcheck="false" title="Search"
    type="text" class="search-form-input" value=""></div>
  <input type="hidden" name="sc" value="tok">
  <input type="hidden" name="t" value="device">
  <input type="hidden" name="segment" value="startpage.udog">
  <input type="hidden" name="lui" value="english">
  <input type="hidden" name="language" value="english">
  <input type="hidden" name="cat" value="web">
  <input type="hidden" name="abd" value="0">
  <input type="hidden" name="abe" value="0">
  <input type="hidden" name="page" value="1">
  <input type="hidden" name="with_date" value="">
  <div class="btns"></div>
</form>
<div style="margin-top: 100px; height: 4px; background: #123456"></div>
</body>
</html>
"#;

/// Black glyph pixels in the query field's band (y 16..44, x 16..400) of a
/// frame — zero before anything is typed there, hundreds once "search" is.
fn query_band_ink(rgba: &[u8], width: u32) -> usize {
    let (w, h) = (width as usize, (rgba.len() / 4 / width as usize));
    let mut n = 0;
    for y in 16usize..44.min(h) {
        for x in 16usize..400.min(w) {
            let o = (y * w + x) * 4;
            if rgba[o] < 60 && rgba[o + 1] < 60 && rgba[o + 2] < 60 {
                n += 1;
            }
        }
    }
    n
}

#[test]
fn search_bar_with_hidden_inputs_is_clickable_and_typeable() {
    let dir = tempfile::tempdir().expect("tempdir");
    let profile = dir.path().join("profile");
    std::fs::create_dir_all(&profile).expect("profile dir");
    let fixture = profile.join("search.html");
    std::fs::write(&fixture, HIDDEN_INPUT_SEARCH_FORM).expect("write fixture");

    let deadline = Instant::now() + DEADLINE;
    let mut tab = spawn_tab(&fixture, &profile);
    let mut seen = Vec::new();

    tab.wait_for(
        "rendered Frame(1024x768)",
        |m| {
            matches!(m, FromTab::Frame { width, height, rgba, .. }
                if *width == 1024 && *height == 768 && !rgba_is_uniform(rgba))
        },
        deadline,
        &mut seen,
    );

    let pointer = |kind, button, x, y| {
        ToTab::Pointer(ironsurf_common::PointerEvent {
            x,
            y,
            kind,
            button,
        })
    };
    use ironsurf_common::{MouseButton, PointerKind};
    // The query line sits at y ≈ 18..38 inside the 56 px-tall flexed form.
    let (cx, cy) = (200.0f32, 28.0f32);

    // 1. Hit test: right-clicking the painted bar must report an EDITABLE
    //    target — pre-patch the query field had width 0 and the container
    //    div answered instead.
    tab.send(pointer(PointerKind::Move, MouseButton::Right, cx, cy));
    tab.send(pointer(PointerKind::Down, MouseButton::Right, cx, cy));
    tab.send(pointer(PointerKind::Up, MouseButton::Right, cx, cy));
    tab.wait_for_new(
        "ContextMenu over the query field (editable)",
        |m| matches!(m, FromTab::ContextMenu { target, .. } if target.editable),
        deadline,
        &mut seen,
    );

    // 2. Focus: a left click lands the caret in `#q` (the shell keys only
    //    reach the page while the page holds focus; blitz's insert needs the
    //    focused editor, which only a hit on the input itself produces).
    tab.send(pointer(PointerKind::Move, MouseButton::Left, cx, cy));
    tab.send(pointer(PointerKind::Down, MouseButton::Left, cx, cy));
    tab.send(pointer(PointerKind::Up, MouseButton::Left, cx, cy));

    // 3. Type a query and watch it paint: the field rendered empty before, so
    //    black ink in its band can only be the typed text.
    for ch in "search".chars() {
        let k = ch.to_string();
        tab.send(ToTab::Key(ironsurf_common::KeyEvent {
            key: k.clone(),
            code: format!("Key{}", k.to_uppercase()),
            text: Some(k),
            repeat: false,
            mods: ironsurf_common::Modifiers::NONE,
        }));
    }
    let painted = loop {
        let frames = tab.collect(Duration::from_millis(1200));
        let best = frames
            .iter()
            .filter_map(|m| match m {
                FromTab::Frame { width, rgba, .. } => Some(query_band_ink(rgba, *width)),
                _ => None,
            })
            .max()
            .unwrap_or(0);
        if best > 0 || Instant::now() >= deadline {
            break best;
        }
    };
    assert!(
        painted > 100,
        "typed query must paint in the search field (band ink = {painted}); \
         a width-0 input paints nothing"
    );

    exit_cleanly(&mut tab);
}
