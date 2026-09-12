//! Servo integration spike (task 4).
//!
//! `servfox-tab --spike --url file:///… --out /tmp/spike.ppm --size 1024x768`
//!
//! Renders a page fully offscreen with `servo::SoftwareRenderingContext`
//! (surfman software adapter — smitha software GL, no X / no GPU) and writes the
//! captured framebuffer to a PPM file. Proves: Servo 0.5 embeds, spins its event
//! loop via an `EventLoopWaker` self-pipe, loads `file://` URLs, and yields
//! readable pixels headless.

use std::cell::{Cell, RefCell};
use std::error::Error;
use std::fmt;
use std::fs::File;
use std::io::Write;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use dpi::PhysicalSize;
use servo::{
    EventLoopWaker, LoadStatus, Opts, RenderingContext, RgbaImage, ScreenshotCaptureError, ServoBuilder,
    SoftwareRenderingContext, WebView, WebViewBuilder, WebViewDelegate,
};

/// Hard bound on the whole render+capture sequence.
const SPIKE_TIMEOUT: Duration = Duration::from_secs(60);

// ---------------------------------------------------------------------------
// CLI

pub struct SpikeArgs {
    pub url: String,
    pub out: String,
    pub width: u32,
    pub height: u32,
}

impl SpikeArgs {
    fn parse(args: &[String]) -> Result<Self, Box<dyn Error>> {
        let mut url = None;
        let mut out = None;
        let mut size = (1024u32, 768u32);
        let mut it = args.iter().skip(1);
        while let Some(arg) = it.next() {
            let mut value = |what: &str| -> Result<String, Box<dyn Error>> {
                it.next()
                    .cloned()
                    .ok_or_else(|| format!("--{what} requires a value").into())
            };
            match arg.as_str() {
                "--spike" => {} // selected by main()
                "--url" => url = Some(value("url")?),
                "--out" => out = Some(value("out")?),
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
                other => return Err(format!("unexpected argument {other:?} in spike mode").into()),
            }
        }
        Ok(SpikeArgs {
            url: url.ok_or("--url is required")?,
            out: out.ok_or("--out is required")?,
            width: size.0,
            height: size.1,
        })
    }
}

// ---------------------------------------------------------------------------
// EventLoopWaker over a self-pipe (Servo wakes us from engine threads)

/// An [`EventLoopWaker`] that writes one byte into a non-blocking pipe; the main
/// thread `poll()`s the read end.
struct PipeWaker {
    tx: Arc<OwnedFd>,
}

/// A readable wake-end that also drains the pipe.
struct WakeReader {
    fd: OwnedFd,
}

fn make_self_pipe() -> std::io::Result<(PipeWaker, WakeReader)> {
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: writing two ints into an array of two ints.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: pipe() returned two fresh descriptors.
    let read_fd = unsafe { OwnedFd::from_raw_fd(fds[0]) };
    // SAFETY: pipe() returned two fresh descriptors.
    let write_fd = unsafe { OwnedFd::from_raw_fd(fds[1]) };
    for fd in [read_fd.as_raw_fd(), write_fd.as_raw_fd()] {
        // SAFETY: F_GETFL/F_SETFL on valid descriptors.
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFL);
            libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
    }
    Ok((
        PipeWaker {
            tx: Arc::new(write_fd),
        },
        WakeReader { fd: read_fd },
    ))
}

impl EventLoopWaker for PipeWaker {
    fn clone_box(&self) -> Box<dyn EventLoopWaker> {
        Box::new(PipeWaker {
            tx: self.tx.clone(),
        })
    }

    fn wake(&self) {
        let byte = [1u8];
        // SAFETY: writing one byte to a valid, non-blocking pipe write-end.
        unsafe {
            libc::write(
                self.tx.as_raw_fd(),
                byte.as_ptr() as *const libc::c_void,
                1,
            );
        }
    }
}

impl WakeReader {
    /// Wait up to `timeout` for a wake byte, then drain the pipe.
    fn wait_and_drain(&self, timeout: Duration) {
        let mut pfd = [libc::pollfd {
            fd: self.fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        }];
        // SAFETY: single valid pollfd entry.
        let ready = unsafe { libc::poll(pfd.as_mut_ptr(), 1, timeout.as_millis() as i32) };
        if ready <= 0 {
            return;
        }
        let mut buf = [0u8; 256];
        loop {
            // SAFETY: reading into a valid buffer from a valid descriptor.
            let n = unsafe {
                libc::read(
                    self.fd.as_raw_fd(),
                    buf.as_mut_ptr() as *mut libc::c_void,
                    buf.len(),
                )
            };
            if n <= 0 {
                break;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// WebViewDelegate — records what the engine tells us

#[derive(Default)]
struct SpikeDelegate {
    load_complete: Cell<bool>,
    frames_ready: Cell<u32>,
    crash: RefCell<Option<String>>,
    closed: Cell<bool>,
    last_url: RefCell<Option<String>>,
    title: RefCell<Option<String>>,
}

impl fmt::Debug for SpikeDelegate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SpikeDelegate")
            .field("load_complete", &self.load_complete.get())
            .field("frames_ready", &self.frames_ready.get())
            .field("crash", &self.crash.borrow())
            .field("closed", &self.closed.get())
            .finish()
    }
}

impl WebViewDelegate for SpikeDelegate {
    fn notify_load_status_changed(&self, _webview: WebView, status: LoadStatus) {
        if matches!(status, LoadStatus::Complete) {
            self.load_complete.set(true);
        }
    }

    fn notify_new_frame_ready(&self, webview: WebView) {
        // Documented Servo 0.5 contract: composite the new WebRender output into
        // the RenderingContext. `take_screenshot`'s read-back runs at the end of
        // this same composite step, so skipping paint() deadlocks screenshots.
        self.frames_ready.set(self.frames_ready.get() + 1);
        webview.paint();
    }

    fn notify_url_changed(&self, _webview: WebView, url: url::Url) {
        *self.last_url.borrow_mut() = Some(url.as_str().to_owned());
    }

    fn notify_page_title_changed(&self, _webview: WebView, title: Option<String>) {
        *self.title.borrow_mut() = title;
    }

    fn notify_crashed(&self, _webview: WebView, reason: String, _backtrace: Option<String>) {
        *self.crash.borrow_mut() = Some(reason);
    }

    fn notify_closed(&self, _webview: WebView) {
        self.closed.set(true);
    }
}

// ---------------------------------------------------------------------------
// PPM output

fn write_ppm(path: &str, image: &RgbaImage) -> Result<(), Box<dyn Error>> {
    let (width, height) = image.dimensions();
    let mut file = File::create(path)?;
    write!(file, "P6\n{width} {height}\n255\n")?;
    // RGBA -> RGB
    let mut rgb = Vec::with_capacity((width * height * 3) as usize);
    for chunk in image.as_raw().chunks_exact(4) {
        rgb.extend_from_slice(&chunk[..3]);
    }
    file.write_all(&rgb)?;
    file.flush()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Main spike

pub fn run(args: &[String]) -> Result<(), Box<dyn Error>> {
    let args = SpikeArgs::parse(args)?;
    eprintln!(
        "servfox-tab spike: url={} out={} size={}",
        args.url, args.out,
        format_args!("{}x{}", args.width, args.height)
    );

    // Servo's network stack requires a process-wide rustls crypto provider.
    if rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .is_err()
    {
        eprintln!("servfox-tab spike: a rustls crypto provider was already installed");
    }

    let url = url::Url::parse(&args.url)?;

    // Fully software, offscreen rendering context (surfman software adapter).
    let size = PhysicalSize::new(args.width, args.height);
    let rendering_context = Rc::new(
        SoftwareRenderingContext::new(size)
            .map_err(|e| format!("SoftwareRenderingContext::new failed: {e:?}"))?,
    );
    rendering_context
        .make_current()
        .map_err(|e| format!("RenderingContext::make_current failed: {e:?}"))?;

    let (waker, wake_reader) = make_self_pipe()?;

    let mut opts = Opts::default();
    // The spike is transient; don't persist engine data.
    opts.temporary_storage = true;
    opts.sandbox = false;

    let servo = ServoBuilder::default()
        .opts(opts)
        .event_loop_waker(Box::new(waker))
        .build();
    // Respect RUST_LOG for diagnosing the spike.
    servo.setup_logging();

    let delegate = Rc::new(SpikeDelegate::default());
    let shot: Rc<RefCell<Option<Result<RgbaImage, ScreenshotCaptureError>>>> =
        Rc::new(RefCell::new(None));

    let webview = WebViewBuilder::new(&servo, rendering_context.clone())
        .url(url)
        .delegate(delegate.clone())
        .build();
    webview.show();

    let deadline = Instant::now() + SPIKE_TIMEOUT;
    let mut screenshot_requested = false;
    let result: Result<RgbaImage, Box<dyn Error>> = loop {
        servo.spin_event_loop();

        if let Some(crash) = delegate.crash.borrow().as_ref() {
            break Err(format!("webview crashed: {crash}").into());
        }
        if delegate.closed.get() {
            break Err("webview closed before screenshot".into());
        }

        if !screenshot_requested && delegate.load_complete.get() {
            screenshot_requested = true;
            let slot = shot.clone();
            // `take_screenshot` waits for document-complete + render up-to-date,
            // then calls us back from the event loop.
            webview.take_screenshot(None, move |r| *slot.borrow_mut() = Some(r));
        }

        if let Some(result) = shot.borrow_mut().as_mut() {
            match result {
                Ok(image) => {
                    break Ok(std::mem::replace(image, RgbaImage::new(0, 0)));
                }
                Err(e) => break Err(format!("screenshot capture failed: {e:?}").into()),
            }
        }

        if Instant::now() >= deadline {
            break Err(
                format!("timed out after {SPIKE_TIMEOUT:?}; delegate state: {delegate:?}").into(),
            );
        }

        // Keep the loop turning even without engine wakeups (timer-paced render
        // progress + screenshot completion).
        wake_reader.wait_and_drain(Duration::from_millis(10));
    };

    let image = result?;
    if image.width() != args.width || image.height() != args.height {
        eprintln!(
            "servfox-tab spike: note: captured {}x{} (requested {}x{})",
            image.width(),
            image.height(),
            args.width,
            args.height
        );
    }
    write_ppm(&args.out, &image)?;
    eprintln!(
        "servfox-tab spike: wrote {} ({}x{}), frames_seen={}, title={:?}",
        args.out,
        image.width(),
        image.height(),
        delegate.frames_ready.get(),
        delegate.title.borrow()
    );
    Ok(())
}
