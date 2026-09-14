//! Headless ACID-test harness mode (ACID3 work, step 1).
//!
//! `ironsurf-tab --headless --url URL [--wait MS] [--body-out FILE]
//! [--shot FILE.ppm] [--size WxH] [--no-sandbox implied]`
//!
//! One-shot, terminal-driven invocation of the *same* machinery
//! [`runtime::run`] uses — same fetch/`netmod`/policy objects (via
//! [`runtime::TabArgs::parse`] on the filtered argv), same single
//! `HtmlDocument` + settle + [`crate::js`] script flow through
//! [`runtime::Tab::navigate`] — but with:
//!
//! - the OS sandbox skipped (`--no-sandbox` implied: a scoring harness reads
//!   arbitrary `file://` fixtures and writes arbitrary output paths),
//! - **stdout unused**: the tab's IPC sink points at `/dev/null`
//!   ([`runtime::Tab::new_headless`]), so the `ToTab`/`FromTab` contract is
//!   untouched and nothing is emitted on the (unconnected) stdout pipe,
//! - a fixed-size viewport (`--size`, default 1024×768) applied before the
//!   single navigation, and
//! - a bounded `--wait` window that drains self-pipe wake-ups with a short
//!   poll sleep and runs the JS timers + settle periodically (this window is
//!   the poll loop for `setTimeout`/`setInterval`/rAF — see `runtime::Tab::
//!   headless_pump`), so anything that lands late is handled before scoring
//!   output is captured.
//!
//! Outputs: the `<body>` element's **innerHTML** (a small DOM serializer over
//! `blitz-dom` nodes — blitz has no `innerHTML`; `textContent` is the fallback
//! if there is no body) to `--body-out`, and a full-viewport `vello_cpu`
//! RGBA→RGB P6 PPM to `--shot`. A one-line summary goes to **stderr**:
//! `HEADLESS title=<title> body=<len> shot=<path>`. Exit code is 0 on the
//! happy path; `main` `_exit`s without ever speaking IPC.

use std::error::Error;
use std::fs;
use std::io::Write;
use std::os::fd::RawFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyrender::{render_to_buffer, PaintScene};
use anyrender_vello_cpu::VelloCpuImageRenderer;
use blitz_html::HtmlDocument;
use blitz_paint::paint_scene;
use kurbo::{Affine, Rect, Size};
use peniko::{Color, Fill};
use ironsurf_common::ToTab;

use crate::runtime::{NavHistory, SelfPipe, Tab, TabArgs};

/// Poll sleep inside the `--wait` window: wake-ups are delivered
/// immediately, this only bounds how long a quiet page is waited out.
const POLL_SLICE: Duration = Duration::from_millis(50);

// ---------------------------------------------------------------------------
// CLI

pub struct HeadlessArgs {
    /// Filtered argv for the *normal* tab parser (net policy, profile dir,
    /// `--offline-fixtures`, log level … all keep working in headless mode).
    pub tab_args: TabArgs,
    pub wait: Duration,
    pub body_out: Option<PathBuf>,
    pub shot: Option<PathBuf>,
    pub width: u32,
    pub height: u32,
}

impl HeadlessArgs {
    pub fn parse(args: &[String]) -> Result<Self, Box<dyn Error>> {
        let mut wait_ms: u64 = 5_000;
        let mut body_out = None;
        let mut shot = None;
        let mut size = (1024u32, 768u32);
        let mut saw_url = false;

        // Every flag the *normal* parser must not see (headless-only surface).
        let mut tab_argv: Vec<String> = Vec::with_capacity(args.len());
        tab_argv.push(args.first().cloned().unwrap_or_default());

        let mut it = args.iter().skip(1);
        while let Some(arg) = it.next() {
            let mut value = |what: &str| -> Result<String, Box<dyn Error>> {
                it.next()
                    .cloned()
                    .ok_or_else(|| format!("--{what} requires a value").into())
            };
            match arg.as_str() {
                "--headless" => {} // selected by main()
                "--wait" => {
                    wait_ms = value("wait")?.parse().map_err(|_| "--wait needs milliseconds")?
                }
                "--body-out" => body_out = Some(PathBuf::from(value("body-out")?)),
                "--shot" => shot = Some(PathBuf::from(value("shot")?)),
                "--size" => {
                    let v = value("size")?;
                    let (w, h) = v
                        .split_once('x')
                        .ok_or_else(|| format!("--size must be WxH, got {v}"))?;
                    size = (
                        w.parse()
                            .map_err(|_| format!("bad width in --size {v:?}"))?,
                        h.parse()
                            .map_err(|_| format!("bad height in --size {v:?}"))?,
                    );
                }
                "--url" => {
                    saw_url = true;
                    let v = value("url")?;
                    tab_argv.push("--url".to_string());
                    tab_argv.push(v);
                }
                // Everything else (`--managed-net`, `--offline-fixtures DIR`,
                // `--log …`, profile overrides) passes through untouched; a
                // value-taking flag keeps its value with it.
                other @ ("--tab-id" | "--container-id" | "--profile-dir"
                | "--offline-fixtures" | "--log" | "--log-level") => {
                    tab_argv.push(other.to_string());
                    tab_argv.push(value(other.trim_start_matches('-'))?);
                }
                _ => tab_argv.push(arg.clone()),
            }
        }
        if !saw_url {
            return Err("--headless requires --url URL".into());
        }
        let mut tab_args = TabArgs::parse(&tab_argv)?;
        // Sandbox implied off; the harness writes outside the profile dir and
        // a scoring run is single-purpose by construction.
        tab_args.no_sandbox = true;

        Ok(HeadlessArgs {
            tab_args,
            wait: Duration::from_millis(wait_ms),
            body_out,
            shot,
            width: size.0.max(1),
            height: size.1.max(1),
        })
    }
}

// ---------------------------------------------------------------------------
// Runner

pub fn run(args: &[String]) -> Result<(), Box<dyn Error>> {
    let args = HeadlessArgs::parse(args)?;
    ironsurf_common::warn!(
        "headless harness: url={} wait={}ms size={}x{} sandbox=off (implied --no-sandbox)",
        args.tab_args.url,
        args.wait.as_millis(),
        args.width,
        args.height
    );

    let pipe = Arc::new(SelfPipe::new()?);
    let mut tab = Tab::new_headless(&args.tab_args, Arc::clone(&pipe));

    // Fixed viewport BEFORE the navigation so the document is built at the
    // requested size (same `set_viewport` semantics the shell drives with).
    tab.handle_to_tab(ToTab::Resize {
        width: args.width,
        height: args.height,
        scale: 1.0,
    });

    // The one navigation: fetch (managed/passthrough policy per flags) →
    // parse → settle → `<script>` run (`crate::js`), exactly as live tabs.
    tab.navigate(&args.tab_args.url.clone(), NavHistory::Fresh);

    // --wait window: this IS the poll loop for JS timers — each pump runs
    // every due setTimeout/setInterval/rAF callback (real timers task) and
    // settles late subresource messages, and the sleep never overshoots the
    // earliest timer deadline, so a `setTimeout(fn, 10)` fires ~10 ms in.
    let deadline = Instant::now() + args.wait;
    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let slice = remaining
            .min(POLL_SLICE)
            .min(tab.timer_sleep_hint().unwrap_or(remaining));
        poll_wake(pipe.read_fd(), slice);
        pipe.drain();
        tab.headless_pump();
    }

    // Capture from the settled document.
    let title = tab.title_now().unwrap_or_default();
    let body = tab
        .document_mut()
        .map(|doc| body_html(&*doc))
        .unwrap_or_default();

    if let Some(path) = &args.body_out {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, &body)?;
    }
    let shot_len = match (&args.shot, tab.document_mut()) {
        (Some(path), Some(doc)) => {
            if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
                fs::create_dir_all(parent)?;
            }
            let len = shot_ppm(doc, args.width, args.height, path)?;
            Some(len)
        }
        (Some(_), None) => {
            ironsurf_common::warn!("headless: no document to screenshot");
            None
        }
        (None, _) => None,
    };

    tab.persist();

    let shot_desc = match (&args.shot, shot_len) {
        (Some(path), Some(_)) => path.display().to_string(),
        _ => "-".to_string(),
    };
    // stderr only — stdout stays unused in headless mode (IPC contract).
    eprintln!(
        "HEADLESS title={title} body={} shot={shot_desc}",
        body.len()
    );
    Ok(())
}

/// Poll the wake pipe for up to `tick`; true when a wake-up was pending.
fn poll_wake(wake_fd: RawFd, tick: Duration) -> bool {
    let mut pfd = libc::pollfd {
        fd: wake_fd,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one valid pollfd entry.
    let ready = unsafe { libc::poll(&mut pfd, 1, tick.as_millis() as i32) };
    ready > 0
}

/// Render the document into a fresh `vello_cpu` RGBA8 buffer at the full
/// viewport and write it as a P6 PPM; returns the on-disk file size.
fn shot_ppm(doc: &mut HtmlDocument, width: u32, height: u32, path: &Path) -> Result<u64, Box<dyn Error>> {
    let buffer = render_to_buffer::<VelloCpuImageRenderer, _>(
        |scene| {
            scene.fill(
                Fill::NonZero,
                Affine::IDENTITY,
                Color::from_rgb8(255, 255, 255),
                None,
                &Rect::from_origin_size(
                    (0.0, 0.0),
                    Size::new(f64::from(width), f64::from(height)),
                ),
            );
            paint_scene(scene, doc, 1.0, width, height, 0, 0);
        },
        width,
        height,
    );
    let mut file = fs::File::create(path)?;
    write!(file, "P6\n{width} {height}\n255\n")?;
    let mut rgb = Vec::with_capacity((width * height * 3) as usize);
    for chunk in buffer.chunks_exact(4) {
        rgb.extend_from_slice(&chunk[..3]);
    }
    file.write_all(&rgb)?;
    file.flush()?;
    let len = file.metadata()?.len();
    Ok(len)
}

// ---------------------------------------------------------------------------
// body.innerHTML

/// The `<body>` element's inner HTML: a small serializer over the `blitz-dom`
/// node tree (blitz has no `innerHTML`), falling back to `textContent` when
/// the document has no body element.
pub fn body_html(doc: &HtmlDocument) -> String {
    inner_html(doc).unwrap_or_else(|| crate::js::body_text_snapshot(doc).unwrap_or_default())
}

/// Serialize the children of the `<body>` element; `None` without a body.
/// Delegates to the shared serializer in [`crate::js`] (same code the DOM
/// bridge's `innerHTML` uses).
pub fn inner_html(doc: &HtmlDocument) -> Option<String> {
    let body = doc.find_body_node()?;
    Some(crate::js::serialize_children(doc, body.id))
}

