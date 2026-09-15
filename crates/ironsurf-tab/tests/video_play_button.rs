//! Regression: clicking the video player's DRAWN play button must actually
//! toggle playback (it used to be swallowed by the scrub-bar hit-region and
//! only seek ~4% — nothing visible).
//!
//! Spawns the real tab binary on a `file://` page embedding `/tmp/vid/flower.mp4`
//! (the repo's H.264 fixture), drives the click over IPC, and judges purely
//! from the pixel stream: pause freezes the video rect, resume moves it again.
//! Self-skips when the fixture is missing.
use std::io::BufReader;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use ironsurf_common::{read_frame, write_frame, FromTab, ToTab};

struct TabChild {
    child: Child,
    stdin: ChildStdin,
    events: Receiver<FromTab>,
}

impl TabChild {
    fn send(&mut self, msg: ToTab) {
        write_frame(&mut self.stdin, &msg).expect("write ToTab");
    }
    fn recv(&mut self, timeout: Duration) -> Option<FromTab> {
        self.events.recv_timeout(timeout).ok()
    }
}

impl Drop for TabChild {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

fn hash(rgba: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce34ea27b17;
    for b in rgba {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn crop_hash(rgba: &[u8], w: u32, x0: u32, y0: u32, cw: u32, ch: u32) -> u64 {
    let mut h: u64 = 0xcbf29ce34ea27b17;
    for y in y0..y0 + ch {
        for x in x0..x0 + cw {
            let i = ((y * w + x) * 4) as usize;
            if i + 4 <= rgba.len() {
                for b in &rgba[i..i + 4] {
                    h ^= *b as u64;
                    h = h.wrapping_mul(0x100000001b3);
                }
            }
        }
    }
    h
}

// The page lives at a fixed absolute path (the `src` must be absolute for a
// `file://` document); the media fixture is the repo convention under /tmp.
const PAGE: &str = "/tmp/ironsurf-vidplay-page.html";

#[test]
fn video_autoplay_and_click() {
    if !std::path::Path::new("/tmp/vid/flower.mp4").exists() {
        eprintln!("SKIP: /tmp/vid/flower.mp4 fixture not present");
        return;
    }
    std::fs::write(
        PAGE,
        "<!doctype html><html><head><title>Vid</title></head>\
         <body style=\"margin: 0; background: #ffffff\">\
         <video src=\"file:///tmp/vid/flower.mp4\" width=\"320\" height=\"240\"></video>\
         </body></html>",
    )
    .expect("write page");
    let dir = std::env::temp_dir().join(format!("vidplay-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let exe = env!("CARGO_BIN_EXE_ironsurf-tab");
    let mut child = Command::new(exe)
        .env_remove("DISPLAY")
        .arg("--tab-id").arg("7")
        .arg("--container-id").arg("1")
        .arg("--profile-dir").arg(&dir)
        // The fixture lives outside the tab's sandboxed read view; this test
        // is about the widget's input mapping, not the sandbox.
        .arg("--no-sandbox").arg("--url").arg(format!("file://{PAGE}"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(std::fs::File::create(dir.join("tab.log")).unwrap())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let stdin = child.stdin.take().unwrap();
    let (tx, events) = mpsc::channel();
    thread::spawn(move || {
        let mut r = BufReader::new(stdout);
        while let Ok(m) = read_frame::<_, FromTab>(&mut r) {
            if tx.send(m).is_err() { break; }
        }
    });
    let mut tab = TabChild { child, stdin, events };

    let deadline = Instant::now() + Duration::from_secs(30);
    // Wait for first frame
    let mut first = None;
    while Instant::now() < deadline {
        if let Some(m) = tab.recv(Duration::from_millis(500)) {
            if let FromTab::Frame { rgba, .. } = m {
                first = Some(rgba);
                break;
            }
        }
    }
    let _first = first.expect("a frame");

    // Collect frames for 2.5s without touching anything (autoplay check).
    let mut auto = Vec::new();
    let end = Instant::now() + Duration::from_millis(2500);
    while Instant::now() < end {
        if let Some(m) = tab.recv(Duration::from_millis(200)) {
            if let FromTab::Frame { width, height, rgba, .. } = m {
                auto.push((width, height, hash(&rgba), crop_hash(&rgba, width, 0, 0, 320, 240)));
            }
        }
    }
    let auto_unique = {
        let mut u: Vec<u64> = auto.iter().map(|f| f.3).collect();
        u.sort();
        u.dedup();
        u.len()
    };
    println!("AUTOPLAY frames={} unique-rect={}", auto.len(), auto_unique);

    // Click the DRAWN play button: the glyph in the control strip, centred
    // at x=12, y=height-11 = 229 on the 320x240 element at the page origin.
    let click = |kind, x, y| ToTab::Pointer(ironsurf_common::PointerEvent {
        x, y, kind, button: ironsurf_common::MouseButton::Left,
    });
    tab.send(click(ironsurf_common::PointerKind::Move, 12.0, 229.0));
    tab.send(click(ironsurf_common::PointerKind::Down, 12.0, 229.0));
    tab.send(click(ironsurf_common::PointerKind::Up, 12.0, 229.0));
    // Drain 800ms of frames after the (pause) click: the picture must freeze
    // (≤2 distinct rects — the paused repaint, then stillness).
    let mut after1 = Vec::new();
    let end = Instant::now() + Duration::from_millis(800);
    while Instant::now() < end {
        if let Some(m) = tab.recv(Duration::from_millis(200)) {
            if let FromTab::Frame { width, height, rgba, .. } = m {
                after1.push(crop_hash(&rgba, width, 0, 0, 320, 240));
            }
        }
    }
    let mut u1 = after1.clone(); u1.sort(); u1.dedup();
    println!("AFTER STRIP-PLAY CLICK (pause): frames={} unique={}", after1.len(), u1.len());
    // Slack of 3 for in-flight frames at the moment of the click; before the
    // fix a strip click left the clip playing and a dozen distinct rects
    // streamed through this window.
    assert!(u1.len() <= 3, "clicking the strip play button must PAUSE the picture; {} distinct rects streamed on", u1.len());

    // Click the same button again (resume): frames must start changing again.
    tab.send(click(ironsurf_common::PointerKind::Move, 12.0, 229.0));
    tab.send(click(ironsurf_common::PointerKind::Down, 12.0, 229.0));
    tab.send(click(ironsurf_common::PointerKind::Up, 12.0, 229.0));
    let mut after2 = Vec::new();
    let end = Instant::now() + Duration::from_millis(1200);
    while Instant::now() < end {
        if let Some(m) = tab.recv(Duration::from_millis(200)) {
            if let FromTab::Frame { width, height, rgba, .. } = m {
                after2.push(crop_hash(&rgba, width, 0, 0, 320, 240));
            }
        }
    }
    let mut u2 = after2.clone(); u2.sort(); u2.dedup();
    println!("AFTER STRIP-PLAY CLICK (resume): frames={} unique={}", after2.len(), u2.len());
    assert!(u2.len() >= 2, "the play button must RESUME: no frame change after the second click");

    tab.send(ToTab::Destroy);
    let _ = tab.child.wait();
    println!("TAB LOG:\n{}", std::fs::read_to_string(dir.join("tab.log")).unwrap_or_default());
}
