//! Subresource fan-out through the tab's CAPPED fetch pool.
//!
//! A page declares its `<img>`/`<link>` loads in one parse pass, and every
//! one lands in `NetProvider::fetch`. The tab used to answer each of those
//! with its own OS thread — so thread count was a function of page size, and
//! an image-heavy page under a thread limit died on `EAGAIN` in
//! `thread::spawn` (the real-world crash: a "scan" page with hundreds of
//! thumbnails took a tab process down mid-parse). Subresources now queue on a
//! bounded pool, which this suite checks end-to-end over the real IPC with
//! far more images than the pool has threads:
//!
//! * every image is fetched and reported (`FromTab::NetworkEvent`) exactly
//!   once — a queued job is never dropped, and none of them is fetched twice;
//! * the tab paints the page and stays alive for a second image-heavy
//!   navigation (a queue that stalled or a spawn that panicked would end the
//!   process or leave the frame uniform).
//!
//! Headless: no `$DISPLAY`, localhost only.
//! Run with: `cargo test -p ironsurf-tab --test subresource_pool`

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use ironsurf_common::{read_frame, write_frame, FromTab, NetEvent, ToTab};

const DEADLINE: Duration = Duration::from_secs(45);

/// Images on the fixture page — comfortably past `MAX_SUBRESOURCE_FETCHERS`.
const IMAGES: usize = 120;

/// Body of every `/img?i=N` response: a REAL 4×4 red PNG, so a fully drained
/// queue is visible as red tiles in the frame (and `response_bytes` is a
/// fixed, assertable number).
const IMG: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
    0x52, 0x00, 0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0x04, 0x08, 0x02, 0x00, 0x00, 0x00, 0x26,
    0x93, 0x09, 0x29, 0x00, 0x00, 0x00, 0x10, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0xf8,
    0xcf, 0xc0, 0x00, 0x47, 0x0c, 0xc4, 0x71, 0x00, 0xae, 0x93, 0x0f, 0xf1, 0xd0, 0x5f, 0x23,
    0x9e, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
];

fn page(tag: &str) -> String {
    let imgs: String = (0..IMAGES)
        .map(|i| format!("<img src=\"/img?i={i}\" width=\"24\" height=\"24\">"))
        .collect();
    format!(
        r#"<!doctype html>
<html>
  <head><title>{tag}</title></head>
  <body style="margin: 0; background: #ffffff">{imgs}</body>
</html>
"#
    )
}

/// HTTP/1.0 fixture on 127.0.0.1 (one request per connection, like a real
/// server that closes): answers the page for `/{tag}` and one image per
/// `/img?i=N`. Returns the bound port.
fn spawn_image_server() -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind loopback");
    let port = listener.local_addr().expect("addr").port();
    thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
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
            let target = head
                .split_whitespace()
                .nth(1)
                .unwrap_or("/")
                .to_string();
            let response = if target.starts_with("/img") {
                http_response(200, "image/png", IMG)
            } else {
                let tag = target.trim_matches(|c| c == '/').to_owned();
                let tag = if tag.is_empty() { "a" } else { &tag };
                http_response(200, "text/html", page(tag).as_bytes())
            };
            let _ = stream.write_all(&response);
            let _ = stream.flush();
        }
    });
    port
}

/// Build the whole response as bytes — a body that is not text (a PNG,
/// say) must reach the tab intact, so nothing here may round-trip it
/// through a `String`.
fn http_response(status: u16, content_type: &str, body: &[u8]) -> Vec<u8> {
    let mut response = format!(
        "HTTP/1.0 {status} OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    response.extend_from_slice(body);
    response
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

    /// `(frames after the first `seen_before`, most red pixels in any of
    /// them)` — the newest frames are the ones a forced repaint produces.
    fn paint_stats(&self, seen_before: usize) -> (usize, usize) {
        let frames: Vec<&Vec<u8>> = self
            .seen
            .iter()
            .filter_map(|m| match m {
                FromTab::Frame { rgba, .. } => Some(rgba),
                _ => None,
            })
            .collect();
        let new_frames = frames.len().saturating_sub(seen_before);
        let best_red = frames
            .iter()
            .skip(seen_before)
            .map(|rgba| red_pixels(rgba))
            .max()
            .unwrap_or(0);
        (new_frames, best_red)
    }

    fn alive(&mut self) {
        if let Some(status) = self.child.try_wait().expect("try_wait") {
            panic!(
                "tab process exited early ({status}); messages seen ({}): {:#?}",
                self.seen.len(),
                self.seen
            );
        }
    }

    /// Block until `pred` has matched `want` network events, returning them.
    /// The tab crashing (the historical failure mode) fails here with the
    /// whole message trail attached.
    fn wait_for_events(&mut self, want: usize, pred: impl Fn(&NetEvent) -> bool) -> Vec<NetEvent> {
        let deadline = Instant::now() + DEADLINE;
        loop {
            let matched: Vec<NetEvent> = self
                .seen
                .iter()
                .filter_map(|m| match m {
                    FromTab::NetworkEvent { event } if pred(event) => Some(event.clone()),
                    _ => None,
                })
                .collect();
            if matched.len() >= want {
                return matched;
            }
            if Instant::now() >= deadline {
                self.alive();
                panic!(
                    "only {} of {want} matching subresource events were reported\n\
                     messages seen ({}): {:#?}",
                    matched.len(),
                    self.seen.len(),
                    self.seen
                );
            }
            match self.events.recv_timeout(Duration::from_millis(200)) {
                Ok(msg) => self.seen.push(msg),
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    let status = self.child.wait().expect("wait");
                    panic!(
                        "stdout closed after {} messages (child status: {status:?}) — the \
                         tab died while its subresources were still queued",
                        self.seen.len()
                    );
                }
            }
        }
    }
}

fn rgba_is_uniform(rgba: &[u8]) -> bool {
    rgba.chunks(4).all(|px| px == &rgba[0..4])
}

/// Reddish pixels — the colour every fixture image is served in, so a frame
/// that contains any has painted at least one of them.
fn red_pixels(rgba: &[u8]) -> usize {
    rgba.chunks(4)
        .filter(|px| px[0] > 200 && px[1] < 80 && px[2] < 80)
        .count()
}

fn spawn_tab(url: &str, profile_dir: &Path) -> TabChild {
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

/// Every image of an image-heavy page is fetched exactly once through the
/// capped pool — the queue drains, nothing is dropped, and the tab that owns
/// it is still alive to say so.
#[test]
fn every_image_of_an_image_heavy_page_loads_through_the_pool() {
    let port = spawn_image_server();
    let profile = tempfile::tempdir().expect("profile dir");
    let base = format!("http://127.0.0.1:{port}");
    let mut tab = spawn_tab(&format!("{base}/a"), profile.path());

    // Each image carries its own `?i=N`, so 120 distinct URLs must appear —
    // exactly once each (the pool never double-submits a queued job).
    let events = tab.wait_for_events(IMAGES, |e| {
        e.url.contains("/img?i=") && e.status == Some(200)
    });
    let distinct: std::collections::HashSet<&str> =
        events.iter().map(|e| e.url.as_str()).collect();
    assert_eq!(
        distinct.len(),
        IMAGES,
        "120 distinct images must each be reported"
    );

    // No duplicates, and every delivery carries the served body size.
    let mut counts = std::collections::BTreeMap::new();
    for msg in &tab.seen {
        if let FromTab::NetworkEvent { event } = msg {
            if event.url.contains("/img?i=") {
                *counts.entry(event.url.clone()).or_insert(0) += 1;
            }
        }
    }
    for (url, n) in &counts {
        assert_eq!(*n, 1, "{url} was fetched {n} times");
    }
    assert!(
        events.iter().all(|e| e.response_bytes == Some(IMG.len() as u64)),
        "every image event reports the served byte count"
    );

    // The delivered bytes really became image content: force a repaint (a
    // `Resize` at the current size) and the next frame must carry the red the
    // images are served in. Note the nudge is OURS, not blitz's —
    // `ShellProvider::request_redraw` wakes the tab loop but does not set its
    // paint flag, so an image landing after the first frame paints only when
    // the shell asks for one (a separate, pre-existing gap: the same page in
    // `--headless` mode paints the tiles by itself because that harness
    // repaints on every tick).
    let frames_before = tab
        .seen
        .iter()
        .filter(|m| matches!(m, FromTab::Frame { .. }))
        .count();
    tab.send(ToTab::Resize {
        width: 1024,
        height: 768,
        scale: 1.0,
    });
    let deadline = Instant::now() + DEADLINE;
    loop {
        let (new_frames, best_red) = tab.paint_stats(frames_before);
        if best_red > 0 {
            break;
        }
        tab.alive();
        if Instant::now() >= deadline {
            panic!(
                "no frame after the forced repaint painted an image: {new_frames} new \
                 frame(s), best red pixel count {best_red} ({frames_before} frame(s) had \
                 been sent before the repaint)\nmessages seen ({}): {:#?}",
                tab.seen.len(),
                tab.seen
            );
        }
        if let Ok(msg) = tab.events.recv_timeout(Duration::from_millis(200)) {
            tab.seen.push(msg);
        }
    }

    // A second image-heavy navigation on the same pool: the workers survived
    // the first page's fan-out, the new document loads, and the process is
    // still speaking the protocol.
    tab.send(ToTab::Navigate {
        url: format!("{base}/b"),
    });
    let deadline = Instant::now() + DEADLINE;
    loop {
        if tab
            .seen
            .iter()
            .any(|m| matches!(m, FromTab::TitleChanged { title: Some(t) } if t == "b"))
        {
            break;
        }
        tab.alive();
        assert!(Instant::now() < deadline, "second navigation title never arrived");
        if let Ok(msg) = tab.events.recv_timeout(Duration::from_millis(200)) {
            tab.seen.push(msg);
        }
    }
    tab.alive();
}
