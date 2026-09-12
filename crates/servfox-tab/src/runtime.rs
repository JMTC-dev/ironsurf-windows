//! The full tab runtime (task 5): sandbox + one Servo/`WebView` per process,
//! speaking the `servfox-common` IPC protocol over stdin/stdout.
//!
//! Lifecycle per `ARCHITECTURE.md` §servfox-tab:
//! 1. apply the OS sandbox (`writable_rw = [profile-dir]`),
//! 2. create the software `RenderingContext` + `Servo` (engine data path =
//!    profile dir via `Opts.config_dir`),
//! 3. build one `WebView` whose delegate streams `FromTab` messages,
//! 4. poll `[stdin, wake-pipe]`, spinning the Servo event loop (≤30 fps while
//!    the page animates),
//! 5. on `Destroy`/EOF: persist the cookie jar, flush, exit 0.

use std::cell::{Cell, RefCell};
use std::error::Error;
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use dpi::PhysicalSize;
use servo::{
    ConsoleLogLevel, DeviceIntRect, DeviceIntSize, DevicePoint, InputEvent, KeyState,
    LoadStatus, MouseButton as ServoMouseButton, MouseButtonAction, MouseButtonEvent,
    MouseLeftViewportEvent, MouseMoveEvent, NamedKey, Opts, Preferences, RenderingContext,
    RgbaImage, Scroll as ServoScroll, ServoBuilder, SoftwareRenderingContext, WebView,
    WebViewBuilder, WebViewDelegate, WebViewPoint, WebViewVector,
};
use servfox_common::{write_frame, FromTab, PointerKind, TabId, ToTab};
use servfox_sandbox::SandboxPolicy;
use servfox_storage::ContainerStore;
use url::Url;

use crate::netmod::{self, blocked_error_page, ManagedNet, NetPolicy};
use crate::wake::{make_self_pipe, WakeReader};

/// Default page size until the shell sends `ToTab::Resize`.
const DEFAULT_SIZE: (u32, u32) = (1024, 768);
/// Spin cadence while a page is animating: at most 30 fps of loop iterations.
const ANIM_TICK: Duration = Duration::from_millis(33);
/// Idle poll cadence (stdin/wake events still arrive immediately).
const IDLE_TICK: Duration = Duration::from_millis(250);

// ---------------------------------------------------------------------------
// CLI

#[derive(Clone, Debug)]
pub struct TabArgs {
    pub tab_id: TabId,
    pub container_id: servfox_common::ContainerId,
    pub profile_dir: PathBuf,
    pub url: String,
    pub no_sandbox: bool,
    pub net_policy: NetPolicy,
}

impl TabArgs {
    pub fn parse(args: &[String]) -> Result<Self, Box<dyn Error>> {
        let mut tab_id = TabId(0);
        let mut container_id = servfox_common::ContainerId(1);
        let mut profile_dir: Option<PathBuf> = None;
        let mut url = None;
        let mut no_sandbox = false;
        let mut managed = false;

        let mut it = args.iter().skip(1);
        while let Some(arg) = it.next() {
            let mut value = |what: &str| -> Result<String, Box<dyn Error>> {
                it.next()
                    .cloned()
                    .ok_or_else(|| format!("--{what} requires a value").into())
            };
            match arg.as_str() {
                "--tab-id" => tab_id = TabId(value("tab-id")?.parse()?),
                "--container-id" => {
                    container_id = servfox_common::ContainerId(value("container-id")?.parse()?)
                }
                "--profile-dir" => profile_dir = Some(PathBuf::from(value("profile-dir")?)),
                "--url" => url = Some(value("url")?),
                "--no-sandbox" => no_sandbox = true,
                "--managed-net" => managed = true,
                other => return Err(format!("unexpected argument {other:?}").into()),
            }
        }
        let profile_dir = match profile_dir {
            Some(dir) => dir,
            // Fall back to the storage layer's own layout when the shell
            // didn't pass an explicit profile dir.
            None => {
                let store = servfox_storage::Store::open(servfox_storage::default_data_root())?;
                store.profile_dir(container_id)
            }
        };
        std::fs::create_dir_all(&profile_dir)?;

        Ok(TabArgs {
            tab_id,
            container_id,
            profile_dir,
            url: url.unwrap_or_else(servfox_common::default_url),
            no_sandbox,
            net_policy: if managed {
                NetPolicy::Managed
            } else {
                NetPolicy::Passthrough
            },
        })
    }
}

// ---------------------------------------------------------------------------
// Outbox — every FromTab message goes through here (main thread only)

#[derive(Clone)]
struct Outbox(Rc<Cell<bool>>);

impl Outbox {
    fn new() -> Self {
        Outbox(Rc::new(Cell::new(false)))
    }

    fn send(&self, msg: FromTab) {
        let stdout = io::stdout();
        let mut lock = stdout.lock();
        if write_frame(&mut lock, &msg).is_err() {
            // Shell vanished: flag for the runtime loop to shut down.
            self.0.set(true);
        }
    }

    fn broken(&self) -> bool {
        self.0.get()
    }
}

// ---------------------------------------------------------------------------
// Delegate

struct AppDelegate {
    out: Outbox,
    rendering_context: Rc<dyn RenderingContext>,
    /// Set once the `WebView` exists (chicken-and-egg with `Rc<delegate>`).
    webview: RefCell<Option<WebView>>,
    net: Option<Rc<ManagedNet>>,
    policy: NetPolicy,
    visible: Cell<bool>,
    frame_pending: Cell<bool>,
    frames_sent: Cell<u64>,
    loading: Cell<bool>,
    animating: Cell<bool>,
    closed: Cell<bool>,
    crashed: Cell<bool>,
    title: RefCell<Option<String>>,
}

impl AppDelegate {
    fn new(
        out: Outbox,
        rendering_context: Rc<dyn RenderingContext>,
        policy: NetPolicy,
        net: Option<ManagedNet>,
    ) -> Rc<Self> {
        Rc::new(AppDelegate {
            out,
            rendering_context,
            webview: RefCell::new(None),
            net: net.map(Rc::new),
            policy,
            visible: Cell::new(true),
            frame_pending: Cell::new(false),
            frames_sent: Cell::new(0),
            loading: Cell::new(false),
            animating: Cell::new(false),
            closed: Cell::new(false),
            crashed: Cell::new(false),
            title: RefCell::new(None),
        })
    }

    fn emit(&self, msg: FromTab) {
        self.out.send(msg);
    }

    /// Composite the latest WebRender output into the rendering context and
    /// read the framebuffer back as a `FromTab::Frame`.
    fn capture_frame(&self, webview: &WebView) {
        // Documented Servo 0.5 paint contract (docs/SERVO_INTEGRATION.md):
        // notify_new_frame_ready must be followed by WebView::paint().
        webview.paint();
        let ctx = self.rendering_context.as_ref();
        if ctx.make_current().is_err() {
            eprintln!("servfox-tab: capture: make_current failed");
            return;
        }
        let size = ctx.size();
        let rect = DeviceIntRect::from_size(DeviceIntSize::new(
            size.width as i32,
            size.height as i32,
        ));
        match ctx.read_to_image(rect) {
            Some(image) => self.send_image(image, webview),
            None => eprintln!("servfox-tab: capture: read_to_image({size:?}) returned None"),
        }
    }

    fn send_image(&self, image: RgbaImage, webview: &WebView) {
        let (width, height) = (image.width(), image.height());
        if width == 0 || height == 0 {
            return;
        }
        let scale = webview.hidpi_scale_factor().get();
        self.emit(FromTab::Frame {
            width,
            height,
            scale,
            rgba: image.into_raw(),
        });
        self.frames_sent.set(self.frames_sent.get() + 1);
    }

    /// Deliver a frame that arrived while the tab was hidden, or one that the
    /// engine produced right at load-complete before the loop observed it.
    fn flush_pending_frame(&self, webview: &WebView) {
        if self.frame_pending.replace(false) && self.visible.get() {
            self.capture_frame(webview);
        }
    }
}

impl WebViewDelegate for AppDelegate {
    fn notify_url_changed(&self, _webview: WebView, url: Url) {
        self.emit(FromTab::UrlChanged {
            url: Some(url.as_str().to_owned()),
        });
    }

    fn notify_page_title_changed(&self, _webview: WebView, title: Option<String>) {
        *self.title.borrow_mut() = title.clone();
        self.emit(FromTab::TitleChanged { title });
    }

    fn notify_status_text_changed(&self, _webview: WebView, status: Option<String>) {
        self.emit(FromTab::StatusText { text: status });
    }

    fn notify_load_status_changed(&self, _webview: WebView, status: LoadStatus) {
        let loading = !matches!(status, LoadStatus::Complete);
        self.loading.set(loading);
        self.emit(FromTab::LoadingChanged { loading });
    }

    fn notify_history_changed(&self, _webview: WebView, entries: Vec<Url>, current: usize) {
        self.emit(FromTab::HistoryChanged {
            can_go_back: current > 0,
            can_go_forward: current + 1 < entries.len(),
        });
    }

    fn notify_animating_changed(&self, _webview: WebView, animating: bool) {
        self.animating.set(animating);
    }

    fn notify_new_frame_ready(&self, webview: WebView) {
        if !self.visible.get() {
            self.frame_pending.set(true);
            return;
        }
        self.capture_frame(&webview);
    }

    fn show_console_message(&self, _webview: WebView, level: ConsoleLogLevel, message: String) {
        self.emit(FromTab::ConsoleLog {
            level: format!("{level:?}").to_ascii_lowercase(),
            message,
        });
    }

    fn notify_closed(&self, _webview: WebView) {
        self.closed.set(true);
    }

    fn notify_crashed(&self, _webview: WebView, reason: String, _backtrace: Option<String>) {
        self.crashed.set(true);
        self.emit(FromTab::Error {
            message: format!("webview crashed: {reason}"),
        });
    }

    /// Container network chokepoint (`--managed-net`); Passthrough returns.
    fn load_web_resource(&self, _webview: WebView, load: servo::WebResourceLoad) {
        if self.policy != NetPolicy::Managed {
            return;
        }
        let Some(net) = self.net.as_ref() else { return };
        let request = load.request().clone();
        let url = request.url.clone();
        match netmod::plan(self.policy, &url, request.is_for_main_frame) {
            netmod::Plan::Pass => {}
            netmod::Plan::Fetch(target) => match net.fetch(&target) {
                Ok((status, headers, body)) => serve_intercepted(load, &url, status, headers, body),
                Err(error) => {
                    eprintln!("servfox-tab: managed fetch failed for {target}: {error}");
                    let body = blocked_error_page(&url, &target, &error).into_bytes();
                    serve_intercepted(
                        load,
                        &url,
                        403,
                        vec![(
                            "content-type".to_owned(),
                            "text/html; charset=utf-8".to_owned(),
                        )],
                        body,
                    );
                }
            },
            netmod::Plan::Upgrade(https) => match net.fetch(&https) {
                Ok((status, headers, body)) if (200..400).contains(&status) => {
                    serve_intercepted(load, &url, status, headers, body)
                }
                other => {
                    // https upgrade failed → block with an error page.
                    let why = match &other {
                        Ok((status, _, _)) => format!("https upgrade returned {status}"),
                        Err(e) => format!("https upgrade failed: {e}"),
                    };
                    eprintln!("servfox-tab: blocking insecure navigation {url}: {why}");
                    let body = blocked_error_page(&url, &https, &why).into_bytes();
                    serve_intercepted(
                        load,
                        &url,
                        403,
                        vec![(
                            "content-type".to_owned(),
                            "text/html; charset=utf-8".to_owned(),
                        )],
                        body,
                    );
                }
            },
        }
    }
}

fn serve_intercepted(
    load: servo::WebResourceLoad,
    url: &Url,
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
) {
    use http::{header::HeaderName, HeaderValue, StatusCode};
    let mut map = http::HeaderMap::new();
    for (name, value) in netmod::forwarded_response_headers(&headers) {
        if let (Ok(n), Ok(v)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(&value),
        ) {
            map.insert(n, v);
        }
    }
    let response = servo::WebResourceResponse::new(url.clone())
        .status_code(StatusCode::from_u16(status).unwrap_or_default())
        .headers(map);
    let mut intercepted = load.intercept(response);
    intercepted.send_body_data(body);
    intercepted.finish();
}

// ---------------------------------------------------------------------------
// Sandbox

fn apply_sandbox(args: &TabArgs) {
    let exe = std::env::current_exe().ok();
    let exe_dir = exe
        .as_ref()
        .and_then(|exe| exe.parent().map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("/"));
    let mut readable: Vec<PathBuf> = servfox_sandbox::default_readable_ro()
        .into_iter()
        .map(PathBuf::from)
        .collect();
    // Extra read-only paths the Servo/GLES stack needs once Landlock is on:
    // the dynamic-loader cache and EGL-Vendor (glvnd) directories are read at
    // e.g. `dlopen("libEGL.so.1")` time, fontconfig at first text layout.
    for extra in [
        "/etc/ld.so.cache",
        "/etc/glvnd",
        "/usr/share/glvnd",
        "/etc/fonts",
        "/usr/share/fontconfig",
        // /etc/ssl/certs entries are symlinks into the ca-certificates tree;
        // Landlock resolves them, so TLS roots need the target readable too.
        "/usr/share/ca-certificates",
    ] {
        if std::path::Path::new(extra).exists() {
            readable.push(PathBuf::from(extra));
        }
    }
    readable.push(exe_dir.clone());
    let policy = SandboxPolicy {
        readable_ro: readable,
        writable_rw: vec![args.profile_dir.clone()],
        exec_ro: exe.map(|exe| vec![exe]).unwrap_or_default(),
        ..SandboxPolicy::default()
    };
    match servfox_sandbox::apply(&policy) {
        Ok(status) => eprintln!(
            "servfox-tab: sandbox landlock={} rlimits={} notes={:?}",
            status.landlock_active, status.rlimits_applied, status.notes
        ),
        Err(err) => eprintln!("servfox-tab: sandbox failed (degraded, continuing): {err}"),
    }
}

// ---------------------------------------------------------------------------
// Runtime

pub fn run(args: TabArgs) -> i32 {
    // Rustls crypto provider, required by Servo's network stack (and ours).
    if rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .is_err()
    {
        eprintln!("servfox-tab: a rustls crypto provider was already installed");
    }

    if args.no_sandbox {
        eprintln!("servfox-tab: sandbox disabled (--no-sandbox)");
    } else {
        apply_sandbox(&args);
    }

    let out = Outbox::new();

    // Container cookie jar + net-cache live in the profile dir.
    let container_store =
        ContainerStore::from_profile_dir(args.container_id, &args.profile_dir);
    let net = (args.net_policy == NetPolicy::Managed)
        .then(|| ManagedNet::new(container_store.cookie_jar(), container_store.cache()));

    let (waker, wake_reader) = match make_self_pipe() {
        Ok(pair) => pair,
        Err(err) => {
            out.send(FromTab::Error {
                message: format!("self-pipe: {err}"),
            });
            return 1;
        }
    };

    let size = PhysicalSize::new(DEFAULT_SIZE.0, DEFAULT_SIZE.1);
    let rendering_context = match SoftwareRenderingContext::new(size) {
        Ok(ctx) => Rc::new(ctx),
        Err(err) => {
            out.send(FromTab::Error {
                message: format!("SoftwareRenderingContext::new failed: {err:?}"),
            });
            return 1;
        }
    };
    if rendering_context.make_current().is_err() {
        out.send(FromTab::Error {
            message: "RenderingContext::make_current failed".into(),
        });
        return 1;
    }

    // Container data path: bind the engine's on-disk state (cookie sqlite,
    // localStorage, cache storage) to this container's profile directory.
    let mut opts = Opts::default();
    opts.config_dir = Some(args.profile_dir.clone());
    opts.temporary_storage = false;
    opts.sandbox = false;

    let servo = ServoBuilder::default()
        .opts(opts)
        .preferences(Preferences::default())
        .event_loop_waker(Box::new(waker))
        .build();
    servo.setup_logging();

    let delegate = AppDelegate::new(
        out.clone(),
        rendering_context.clone(),
        args.net_policy,
        net,
    );

    let url = match parse_input_url(&args.url) {
        Ok(url) => url,
        Err(err) => {
            out.send(FromTab::Error {
                message: format!("bad start url: {err}"),
            });
            return 2;
        }
    };
    let webview = WebViewBuilder::new(&servo, rendering_context.clone())
        .url(url)
        .delegate(delegate.clone())
        .build();
    delegate.webview.replace(Some(webview.clone()));
    webview.show();
    webview.focus();

    out.send(FromTab::Ready { tab_id: args.tab_id });

    let stdin_fd = io::stdin().as_raw_fd();
    if let Err(err) = set_stdin_nonblocking() {
        out.send(FromTab::Error {
            message: format!("stdin non-blocking: {err}"),
        });
        return 1;
    }
    let mut pending: Vec<u8> = Vec::new();

    let exit_code: i32 = loop {
        // 1. Drain wake-ups, then wait for engine wake or shell input.
        wake_reader.drain();
        let tick = if delegate.animating.get() {
            ANIM_TICK
        } else {
            IDLE_TICK
        };
        let stdin_ready = poll_stdin_or_wake(stdin_fd, &wake_reader, tick);

        // 2. Drain every complete ToTab frame currently available on stdin.
        if stdin_ready {
            match drain_stdin(&mut pending, &webview, &delegate) {
                Ok(Some(code)) => break code,
                Ok(None) => {}
                Err(err) => {
                    eprintln!("servfox-tab: stdin protocol error: {err}");
                    out.send(FromTab::Error {
                        message: format!("protocol error: {err}"),
                    });
                    break 2;
                }
            }
        }

        // 3. Turn the Servo event loop; delegate callbacks fire here.
        servo.spin_event_loop();

        // 4. Lifecycle endings.
        if delegate.crashed.get() {
            break 1;
        }
        if delegate.closed.get() {
            out.send(FromTab::Closed);
            break 0;
        }
        if out.broken() {
            // Shell vanished: persist what we can, then die quietly.
            break 0;
        }
        // Deliver frames produced while hidden / right at load-complete.
        delegate.flush_pending_frame(&webview);
    };

    persist_cookies(&delegate);
    let _ = io::stdout().flush();
    eprintln!(
        "servfox-tab: exiting {exit_code} (frames_sent={}, title={:?})",
        delegate.frames_sent.get(),
        delegate.title.borrow()
    );
    exit_code
}

fn persist_cookies(delegate: &AppDelegate) {
    // Best-effort: the jar's path is inside the sandbox-writable profile dir.
    if let Some(net) = delegate.net.as_ref() {
        let jar = net.jar.borrow().clone();
        let _ = jar.save();
    }
}

// ---------------------------------------------------------------------------
// stdin pump

/// Wait until stdin or the wake pipe is readable (or `tick` elapses).
/// Returns true when stdin has data (or hit EOF — `POLLHUP`).
fn poll_stdin_or_wake(stdin_fd: libc::c_int, wake: &WakeReader, tick: Duration) -> bool {
    let mut pfd = [
        libc::pollfd {
            fd: stdin_fd,
            events: libc::POLLIN,
            revents: 0,
        },
        libc::pollfd {
            fd: wake.fd(),
            events: libc::POLLIN,
            revents: 0,
        },
    ];
    // SAFETY: two valid pollfd entries.
    let ready = unsafe { libc::poll(pfd.as_mut_ptr(), 2, tick.as_millis() as i32) };
    if ready <= 0 {
        return false;
    }
    wake.drain();
    let revents = pfd[0].revents;
    revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0
}

/// Mark stdin non-blocking so a full-frame read can never park the event
/// loop (frames arrive as single writes from the shell, well under one pipe
/// buffer).
fn set_stdin_nonblocking() -> io::Result<()> {
    let fd = io::stdin().as_raw_fd();
    // SAFETY: F_GETFL/F_SETFL on a valid descriptor.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags == -1 {
            return Err(io::Error::last_os_error());
        }
        if libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) == -1 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Read everything currently available on stdin, append to `pending`, and
/// handle every complete `ToTab` frame in it. `Ok(Some(code))` requests
/// shutdown with that exit code.
fn drain_stdin(
    pending: &mut Vec<u8>,
    webview: &WebView,
    delegate: &AppDelegate,
) -> io::Result<Option<i32>> {
    // 1. Pull all bytes the shell has sent so far (non-blocking).
    let mut chunk = [0u8; 64 * 1024];
    loop {
        // SAFETY: read into a valid buffer from stdin.
        let n = unsafe {
            libc::read(
                io::stdin().as_raw_fd(),
                chunk.as_mut_ptr() as *mut libc::c_void,
                chunk.len(),
            )
        };
        if n > 0 {
            pending.extend_from_slice(&chunk[..n as usize]);
            if (n as usize) < chunk.len() {
                break;
            }
            continue;
        }
        if n == 0 {
            // Shell closed the pipe: same as Destroy (after handling the
            // complete frames already in `pending`).
            drain_frames(pending, webview, delegate)?;
            return Ok(Some(0));
        }
        let err = io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::EAGAIN) || err.raw_os_error() == Some(libc::EWOULDBLOCK)
        {
            break;
        }
        return Err(err);
    }
    drain_frames(pending, webview, delegate)
}

/// Deserialize and handle every complete frame in `pending`.
fn drain_frames(
    pending: &mut Vec<u8>,
    webview: &WebView,
    delegate: &AppDelegate,
) -> io::Result<Option<i32>> {
    loop {
        if pending.len() < 4 {
            return Ok(None);
        }
        let len = u32::from_le_bytes([pending[0], pending[1], pending[2], pending[3]]) as usize;
        if len > servfox_common::MAX_FRAME_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("frame too large: {len} bytes"),
            ));
        }
        if pending.len() < 4 + len {
            return Ok(None); // wait for the rest
        }
        // read_frame expects the full length-prefixed frame; it can't borrow
        // `pending` while we later drain it, so copy the frame out first.
        let framed = pending[..4 + len].to_vec();
        let msg: ToTab = servfox_common::read_frame(&mut io::Cursor::new(&framed))?;
        pending.drain(..4 + len);
        if let Some(code) = handle(msg, webview, delegate) {
            return Ok(Some(code));
        }
    }
}

fn handle(msg: ToTab, webview: &WebView, delegate: &AppDelegate) -> Option<i32> {
    match msg {
        ToTab::Navigate { url } => match parse_input_url(&url) {
            Ok(parsed) => webview.load(parsed),
            Err(err) => delegate.emit(FromTab::Error {
                message: format!("bad url {url:?}: {err}"),
            }),
        },
        ToTab::Reload => webview.reload(),
        ToTab::Stop => {} // v1 placeholder, per contract
        ToTab::Back => {
            if webview.can_go_back() {
                webview.go_back(1);
            }
        }
        ToTab::Forward => {
            if webview.can_go_forward() {
                webview.go_forward(1);
            }
        }
        ToTab::Resize {
            width,
            height,
            scale,
        } => {
            // WebView::resize routes through Paint::resize_rendering_context,
            // which early-returns when the context already reports the new
            // size — resizing the context ourselves would SILENTLY swallow
            // the viewport change (no reflow, no new frames). Let Servo own it.
            let size = PhysicalSize::new(width.max(1), height.max(1));
            webview.resize(size);
            let _ = scale; // echoed on frames; pane size is already physical
            delegate.frame_pending.set(true);
        }
        ToTab::SetVisible { visible } => {
            delegate.visible.set(visible);
            webview.set_throttled(!visible);
            if visible {
                delegate.frame_pending.set(true);
            }
        }
        ToTab::Pointer(event) => {
            let point = WebViewPoint::Device(DevicePoint::new(event.x, event.y));
            let input = match event.kind {
                PointerKind::Move | PointerKind::Over => {
                    InputEvent::MouseMove(MouseMoveEvent::new(point))
                }
                PointerKind::Out => InputEvent::MouseLeftViewport(MouseLeftViewportEvent {
                    focus_moving_to_another_iframe: false,
                }),
                PointerKind::Down => InputEvent::MouseButton(MouseButtonEvent::new(
                    MouseButtonAction::Down,
                    servo_button(event.button),
                    point,
                )),
                PointerKind::Up => InputEvent::MouseButton(MouseButtonEvent::new(
                    MouseButtonAction::Up,
                    servo_button(event.button),
                    point,
                )),
            };
            webview.notify_input_event(input);
        }
        ToTab::Key(event) => {
            use servo::{Code, Key, Modifiers as KtModifiers};
            // Prefer the typed text (so `<input>` sees characters), unless a
            // Ctrl chord says this is a shortcut, not text input.
            let key = event
                .text
                .as_deref()
                .filter(|t| !t.is_empty() && !event.mods.ctrl())
                .and_then(|t| t.parse::<Key>().ok())
                .or_else(|| event.key.parse::<Key>().ok())
                .unwrap_or(Key::Named(NamedKey::Unidentified));
            let code = event.code.parse::<Code>().unwrap_or(Code::Unidentified);
            let mut mods = KtModifiers::empty();
            if event.mods.ctrl() {
                mods |= KtModifiers::CONTROL;
            }
            if event.mods.shift() {
                mods |= KtModifiers::SHIFT;
            }
            if event.mods.alt() {
                mods |= KtModifiers::ALT;
            }
            if event.mods.meta() {
                mods |= KtModifiers::META;
            }
            webview.notify_input_event(InputEvent::Keyboard(servo::KeyboardEvent::new_without_event(
                KeyState::Down,
                key,
                code,
                servo::Location::Standard,
                mods,
                event.repeat,
                false,
            )));
        }
        ToTab::Scroll(event) => {
            // WebRender semantics: a positive delta reveals more content
            // (bottom/right respectively).
            let point = WebViewPoint::Page(euclid::Point2D::new(event.x, event.y));
            webview.notify_scroll_event(
                ServoScroll::Delta(WebViewVector::Page(euclid::Vector2D::new(
                    event.delta_x,
                    event.delta_y,
                ))),
                point,
            );
        }
        ToTab::Destroy => return Some(0),
    }
    None
}

fn servo_button(button: servfox_common::MouseButton) -> ServoMouseButton {
    match button {
        servfox_common::MouseButton::Left => ServoMouseButton::Left,
        servfox_common::MouseButton::Right => ServoMouseButton::Right,
        servfox_common::MouseButton::Middle => ServoMouseButton::Middle,
    }
}

/// Resolve shell-provided URL text: parse as-is; if that fails and the text
/// looks like a bare host (no whitespace), assume `https://…` — the same
/// search-or-navigate leniency the shell applies.
pub fn parse_input_url(input: &str) -> Result<Url, Box<dyn Error>> {
    match Url::parse(input) {
        Ok(url) => Ok(url),
        Err(original) => {
            if input.contains(char::is_whitespace) {
                return Err(format!("not a URL: {input:?} ({original})").into());
            }
            Url::parse(&format!("https://{input}")).map_err(|_| {
                format!("not a URL: {input:?} ({original})").into()
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_input_url_accepts_absolute_urls() {
        assert_eq!(
            parse_input_url("file:///tmp/x/index.html").unwrap().as_str(),
            "file:///tmp/x/index.html"
        );
        assert_eq!(
            parse_input_url("https://example.com/").unwrap().as_str(),
            "https://example.com/"
        );
        // Schemes without a path are fine too.
        assert!(parse_input_url("about:blank").is_ok());
    }

    #[test]
    fn parse_input_url_upgrades_bare_hosts() {
        assert_eq!(
            parse_input_url("example.com").unwrap().as_str(),
            "https://example.com/"
        );
    }

    #[test]
    fn parse_input_url_rejects_free_text() {
        assert!(parse_input_url("not a url").is_err());
        assert!(parse_input_url("").is_err());
    }
}
