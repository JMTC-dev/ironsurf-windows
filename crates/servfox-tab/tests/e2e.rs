//! End-to-end test for the tab runtime (Blitz IPC runtime, task 7c).
//!
//! Headless (no `$DISPLAY`): spawns the built `servfox-tab` binary against a
//! local `file://` fixture and drives the full `ToTab`/`FromTab` protocol —
//! `Ready` → `UrlChanged` → `Frame` (non-uniform at the requested size) →
//! `TitleChanged` → `Destroy` → exit 0, all bounded by 60 s.
//!
//! Run with: `cargo test -p servfox-tab --test e2e`
//!
//! These are the Servo-era (task 5) shell-contract expectations, restored
//! unchanged on the Blitz runtime (task 7c): every assertion is
//! engine-agnostic IPC protocol behaviour.

use std::io::BufReader;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use servfox_common::{read_frame, write_frame, FromTab, ToTab};

/// Minimal page: white background, a solid red box, and a distinctive title —
/// guarantees a non-uniform frame and an exact `TitleChanged` match.
const FIXTURE: &str = r#"<!doctype html>
<html>
  <head><title>ServFox E2E</title></head>
  <body style="margin: 0; background: #ffffff">
    <div style="width: 120px; height: 120px; background: #ff3200"></div>
    <h1 style="color: #000064; font-family: monospace">ServFox end to end</h1>
  </body>
</html>
"#;

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

fn spawn_tab(fixture: &std::path::Path, profile_dir: &std::path::Path) -> TabChild {
    let exe = env!("CARGO_BIN_EXE_servfox-tab");
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
        .expect("spawn servfox-tab");

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
    !rgba
        .chunks_exact(4)
        .step_by(97)
        .any(|px| px != first)
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
        |m| matches!(m, FromTab::Ready { tab_id } if tab_id == &servfox_common::TabId(7)),
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
        "TitleChanged(\"ServFox E2E\")",
        |m| matches!(m, FromTab::TitleChanged { title: Some(t) } if t == "ServFox E2E"),
        deadline,
        &mut seen,
    );
    assert_eq!(
        title,
        FromTab::TitleChanged {
            title: Some("ServFox E2E".into())
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

    let mut child = Command::new(env!("CARGO_BIN_EXE_servfox-tab"))
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
        .expect("spawn servfox-tab");
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
