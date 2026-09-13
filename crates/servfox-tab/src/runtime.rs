//! The Blitz tab runtime (task 7c): sandbox + one `HtmlDocument` per
//! navigation, speaking the `servfox-common` IPC protocol over stdin/stdout.
//!
//! Lifecycle per `ARCHITECTURE.md` §servfox-tab (Blitz edition):
//! 1. apply the OS sandbox (`writable_rw = [profile-dir]`),
//! 2. own the event loop on ONE thread — `poll()` over `[non-blocking stdin,
//!    self-pipe]` (no winit, no engine thread; Blitz is a library we drive —
//!    `HtmlDocument` is neither `Send` nor `Sync`, see `docs/BLITZ_API_NOTES.md` §8),
//! 3. one `HtmlDocument` per navigation: main-document fetch always goes
//!    through `netmod::fetch_document` (managed policy applies), subresources
//!    flow through our `BlitzNetProvider`,
//! 4. `resolve` → `render_to_buffer::<VelloCpuImageRenderer>` →
//!    `FromTab::Frame` (≤30 fps coalesced), `TitleChanged`/`UrlChanged`/
//!    `HistoryChanged` emitted on change,
//! 5. on `Destroy`/EOF: persist the cookie jar, flush stdout, exit 0.
//!
//! INPUT/LINK-CLICKS (task 7d): `ToTab::Pointer/Key/Scroll` route through the
//! single [`handle_input`] extension point; link clicks round-trip via the
//! `NavigationProvider` hook ([`NavHook`]) already wired into every document.

use std::error::Error;
use std::io::{self, BufWriter, Read, StdoutLock, Write};
use std::os::fd::AsRawFd;
use std::os::fd::RawFd;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyrender::{render_to_buffer, PaintScene};
use anyrender_vello_cpu::VelloCpuImageRenderer;
use blitz_dom::DocumentConfig;
use blitz_html::HtmlDocument;
use blitz_paint::paint_scene;
use blitz_traits::navigation::{NavigationOptions, NavigationProvider};
use blitz_traits::shell::{ColorScheme, ShellProvider, Viewport};
use kurbo::{Affine, Rect, Size};
use peniko::{Color, Fill};
use servfox_common::{read_frame, write_frame, ContainerId, FromTab, TabId, ToTab};
use servfox_sandbox::SandboxPolicy;
use servfox_storage::{ContainerStore, CookieJar, NetCache};
use url::Url;

use crate::netmod::{self, BlitzNetProvider, NetPolicy};

/// Default page size until the shell sends `ToTab::Resize` (physical px).
const DEFAULT_SIZE: (u32, u32) = (1024, 768);
/// Frame throttle: never emit more than ~30 fps (coalesce within one window).
const FRAME_INTERVAL: Duration = Duration::from_millis(33);
/// Idle poll cadence — stdin/wake events still arrive immediately via poll().
const IDLE_TICK: Duration = Duration::from_millis(250);

// ---------------------------------------------------------------------------
// CLI

#[derive(Clone, Debug)]
pub struct TabArgs {
    pub tab_id: TabId,
    pub container_id: ContainerId,
    pub profile_dir: PathBuf,
    pub url: String,
    /// Directory consulted for `file://` navigations whose path is missing:
    /// `DIR/<basename>` is tried before the fetch fails (fixture layout).
    pub offline_fixtures: Option<PathBuf>,
    pub net_policy: NetPolicy,
    pub no_sandbox: bool,
}

impl TabArgs {
    pub fn parse(args: &[String]) -> Result<Self, Box<dyn Error>> {
        let mut tab_id = TabId(0);
        let mut container_id = ContainerId(1);
        let mut profile_dir: Option<PathBuf> = None;
        let mut url = None;
        let mut offline_fixtures = None;
        let mut managed = false;
        let mut no_sandbox = false;

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
                    container_id = ContainerId(value("container-id")?.parse()?)
                }
                "--profile-dir" => profile_dir = Some(PathBuf::from(value("profile-dir")?)),
                "--url" => url = Some(value("url")?),
                "--offline-fixtures" => offline_fixtures = Some(PathBuf::from(value("offline-fixtures")?)),
                "--managed-net" => managed = true,
                "--no-sandbox" => no_sandbox = true,
                other => return Err(format!("unexpected argument {other:?}").into()),
            }
        }
        let profile_dir = match profile_dir {
            Some(dir) => dir,
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
            offline_fixtures,
            net_policy: if managed {
                NetPolicy::Managed
            } else {
                NetPolicy::Passthrough
            },
            no_sandbox,
        })
    }
}

// ---------------------------------------------------------------------------
// Self-pipe (loop wake-up from ShellProvider / NetWaker worker threads)

/// Non-blocking pipe; every wake-up is one byte. The read end joins the
/// `poll()` set, the write end is shared with the Blitz hooks (all of which
/// are `Send + Sync` and may fire on net-worker threads).
pub struct SelfPipe {
    read_fd: RawFd,
    write_fd: RawFd,
}
// Raw file descriptors we own; writes are single non-blocking `write()` calls.
unsafe impl Send for SelfPipe {}
unsafe impl Sync for SelfPipe {}

impl SelfPipe {
    pub fn new() -> io::Result<Self> {
        let mut fds = [0 as RawFd; 2];
        // SAFETY: writing two fds into a 2-element array.
        let rc = unsafe {
            libc::pipe2(fds.as_mut_ptr(), libc::O_NONBLOCK | libc::O_CLOEXEC)
        };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(SelfPipe {
            read_fd: fds[0],
            write_fd: fds[1],
        })
    }

    pub fn read_fd(&self) -> RawFd {
        self.read_fd
    }

    /// Poke the loop. Ignores `EAGAIN`/full-pipe: a saturated pipe already
    /// means wake-ups are pending.
    pub fn wake(&self) {
        // SAFETY: single-byte write to our own pipe end.
        unsafe {
            let byte: [u8; 1] = [1];
            libc::write(
                self.write_fd,
                byte.as_ptr() as *const libc::c_void,
                1,
            );
        }
    }

    /// Drain all pending wake-up bytes; returns how many were pending (the
    /// loop uses "something woke us" as the resolve/paint trigger).
    pub fn drain(&self) -> usize {
        let mut buf = [0u8; 256];
        let mut total = 0usize;
        loop {
            // SAFETY: read into a valid buffer from our own pipe end.
            let n = unsafe {
                libc::read(
                    self.read_fd,
                    buf.as_mut_ptr() as *mut libc::c_void,
                    buf.len(),
                )
            };
            if n <= 0 {
                return total; // EAGAIN or error: drained.
            }
            total += n as usize;
        }
    }
}

// ---------------------------------------------------------------------------
// Blitz shell / navigation hooks

/// `ShellProvider` bridging document-side signals (redraw requests, title
/// flushes) into the IPC loop via the self-pipe. Shared across navigations;
/// the title slot is reset at the start of every navigation.
struct ShellHook {
    pipe: Arc<SelfPipe>,
    title: Mutex<Option<String>>,
}

impl ShellProvider for ShellHook {
    fn request_redraw(&self) {
        self.pipe.wake();
    }
    fn set_window_title(&self, title: String) {
        *self.title.lock().expect("title mutex") = Some(title);
        self.pipe.wake();
    }
}

/// `NavigationProvider`: link clicks / form submits are recorded here (called
/// inline on the doc thread from `handle_ui_event`) and picked up by the loop
/// right after the event — the top-level fetch always goes through
/// `fetch_document`, never through Blitz (ARCHITECTURE §Network layer).
struct NavHook {
    pipe: Arc<SelfPipe>,
    pending: Mutex<Option<NavigationOptions>>,
}

impl NavigationProvider for NavHook {
    fn navigate_to(&self, options: NavigationOptions) {
        *self.pending.lock().expect("nav mutex") = Some(options);
        self.pipe.wake();
    }
}

// ---------------------------------------------------------------------------
// Sandbox (same read-only set the Servo-era runtime needed, see a3cb047)

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
    // Extras the old runtime listed: the dynamic-loader cache, fontconfig
    // (first text layout) and the CA symlink targets under /etc/ssl/certs.
    for extra in [
        "/etc/ld.so.cache",
        "/etc/glvnd",
        "/usr/share/glvnd",
        "/etc/fonts",
        "/usr/share/fontconfig",
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
        // Sandboxing must never abort the tab (contract): degrade loudly.
        Err(err) => eprintln!("servfox-tab: sandbox failed (degraded, continuing): {err}"),
    }
}

// ---------------------------------------------------------------------------
// URL resolution

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

// ---------------------------------------------------------------------------
// Runtime

pub struct Tab {
    out: TabOut,
    /// Latest size/scale from the shell (physical px + device scale).
    width: u32,
    height: u32,
    scale: f32,
    visible: bool,
    doc: Option<HtmlDocument>,
    jar: CookieJar,
    cache: NetCache,
    provider: Arc<BlitzNetProvider>,
    shell: Arc<ShellHook>,
    nav: Arc<NavHook>,
    /// URL-only session history (per the common IPC contract).
    history: Vec<Url>,
    history_index: usize,
    last_url: Option<String>,
    last_title: Option<Option<String>>,
    /// Paint state: set when a resolve produced (or may have produced) new
    /// pixels; coalesced into at most ~30 fps of `FromTab::Frame`.
    needs_paint: bool,
    last_paint: Option<Instant>,
    offline_fixtures: Option<PathBuf>,
    /// Shared wake pipe (owned alongside the hooks; kept for 7d input work).
    #[allow(dead_code)]
    pipe: Arc<SelfPipe>,
}

/// stdout side of the IPC: one persistent BufWriter, flushed per frame.
struct TabOut {
    writer: BufWriter<StdoutLock<'static>>,
    broken: bool,
}

impl TabOut {
    fn new() -> Self {
        TabOut {
            writer: BufWriter::with_capacity(1 << 20, io::stdout().lock()),
            broken: false,
        }
    }

    fn send(&mut self, msg: FromTab) {
        if write_frame(&mut self.writer, &msg).is_err() {
            // Shell vanished: flag for the loop to shut down.
            self.broken = true;
        }
        if self.writer.flush().is_err() {
            self.broken = true;
        }
    }
}

impl Tab {
    fn new(args: &TabArgs, pipe: Arc<SelfPipe>) -> Self {
        let store = ContainerStore::from_profile_dir(args.container_id, &args.profile_dir);
        // The tab drives main-document fetches with its own jar/cache; the
        // provider owns a second handle to the same on-disk jar for
        // subresources (both are flushed on shutdown).
        let provider = Arc::new(
            BlitzNetProvider::new(args.net_policy, store.cookie_jar(), store.cache())
                .with_waker(Arc::new(PipeWaker(Arc::clone(&pipe)))),
        );
        Tab {
            out: TabOut::new(),
            width: DEFAULT_SIZE.0,
            height: DEFAULT_SIZE.1,
            scale: 1.0,
            visible: true,
            doc: None,
            jar: store.cookie_jar(),
            cache: store.cache(),
            provider,
            shell: Arc::new(ShellHook {
                pipe: Arc::clone(&pipe),
                title: Mutex::new(None),
            }),
            nav: Arc::new(NavHook {
                pipe: Arc::clone(&pipe),
                pending: Mutex::new(None),
            }),
            history: Vec::new(),
            history_index: 0,
            last_title: None,
            last_url: None,
            needs_paint: false,
            last_paint: None,
            offline_fixtures: args.offline_fixtures.clone(),
            pipe,
        }
    }

    fn emit(&mut self, msg: FromTab) {
        self.out.send(msg);
    }

    fn viewport(&self) -> Viewport {
        Viewport::new(
            (self.width as f32 * self.scale) as u32,
            (self.height as f32 * self.scale) as u32,
            self.scale,
            ColorScheme::Light,
        )
    }

    fn emit_history_changed(&mut self) {
        let can_back = self.history_index > 0;
        let can_fwd = self.history_index + 1 < self.history.len();
        self.emit(FromTab::HistoryChanged {
            can_go_back: can_back,
            can_go_forward: can_fwd,
        });
    }

    // -- navigation -------------------------------------------------------

    /// Navigate to `raw` (shell text). `history` says what to do with the
    /// URL-only session history (fresh loads truncate forward entries).
    pub fn navigate(&mut self, raw: &str, history: NavHistory) -> bool {
        let url = match parse_input_url(raw) {
            Ok(url) => url,
            Err(err) => {
                self.emit(FromTab::Error {
                    message: format!("bad url {raw:?}: {err}"),
                });
                return false;
            }
        };
        if url.scheme() == "javascript" {
            // Contract: javascript: URLs are refused with a status text.
            self.emit(FromTab::StatusText {
                text: Some(format!("refused javascript: URL {raw}")),
            });
            return false;
        }
        if history == NavHistory::Fresh {
            self.history.truncate(self.history_index + 1);
        }

        self.emit(FromTab::LoadingChanged { loading: true });
        self.emit(FromTab::StatusText {
            text: Some(format!("Waiting for {}", url.host_str().unwrap_or(url.as_str()))),
        });

        let url = self.apply_offline_fixtures(url);
        match netmod::fetch_document(
            &url,
            self_provider_policy(&self.provider),
            &mut self.jar,
            &self.cache,
            netmod::now_unix(),
        ) {
            Ok(doc_fetch) => {
                self.load_html(&doc_fetch.url, &doc_fetch.body_text(), history);
                self.emit(FromTab::LoadingChanged { loading: false });
                self.emit(FromTab::StatusText { text: None });
                true
            }
            Err(err) => {
                // Fetch failure: report, keep the previous document on screen.
                eprintln!("servfox-tab: navigation to {url} failed: {err}");
                if let Some(page) = err.error_page() {
                    // Blocked https-upgrade: render the block page at the
                    // requested URL (it stays in history as the user typed it).
                    self.load_html(&url, &page, history);
                    self.emit(FromTab::LoadingChanged { loading: false });
                    false
                } else {
                    self.emit(FromTab::Error {
                        message: err.to_string(),
                    });
                    self.emit(FromTab::LoadingChanged { loading: false });
                    self.emit(FromTab::StatusText { text: None });
                    false
                }
            }
        }
    }

    /// `--offline-fixtures DIR` semantics: a `file://` navigation whose path
    /// does not exist resolves against the fixture dir by basename (e.g. the
    /// shell gives `file:///index.html` for fixtures served under `DIR`).
    fn apply_offline_fixtures(&self, url: Url) -> Url {
        let Some(dir) = self.offline_fixtures.as_ref() else {
            return url;
        };
        if url.scheme() != "file" || url.to_file_path().map(|p| p.exists()).unwrap_or(false) {
            return url;
        }
        let name = std::path::Path::new(url.path())
            .file_name()
            .map(|n| n.to_owned())
            .filter(|n| n != "/" && !n.as_os_str().is_empty());
        match name {
            Some(name) => {
                let candidate = dir.join(name);
                if candidate.exists() {
                    Url::from_file_path(candidate).unwrap_or(url)
                } else {
                    url
                }
            }
            None => url,
        }
    }

    /// Build a fresh `HtmlDocument` for an already-fetched document and drive
    /// it to a first paint; then emit the metadata deltas (url/title/history).
    fn load_html(&mut self, url: &Url, html: &str, history: NavHistory) {
        // A fresh doc must not inherit the previous doc's flushed title.
        *self.shell.title.lock().expect("title mutex") = None;
        let config = DocumentConfig {
            base_url: Some(url.as_str().to_owned()),
            viewport: Some(self.viewport()),
            net_provider: Some(Arc::clone(&self.provider) as Arc<dyn blitz_traits::net::NetProvider>),
            navigation_provider: Some(Arc::clone(&self.nav)
                as Arc<dyn blitz_traits::navigation::NavigationProvider>),
            shell_provider: Some(Arc::clone(&self.shell)
                as Arc<dyn blitz_traits::shell::ShellProvider>),
            ..Default::default()
        };
        let document = HtmlDocument::from_html(html, config);
        self.doc = Some(document);
        self.settle();

        match history {
            NavHistory::Fresh => {
                let target = self
                    .history
                    .iter()
                    .position(|h| h == url)
                    .unwrap_or(self.history.len().saturating_sub(1));
                if self.history.get(target) != Some(url) {
                    self.history.push(url.clone());
                }
                self.history_index = target;
            }
            NavHistory::Session(session) => {
                self.history_index = session;
                self.history[session] = url.clone(); // redirects replace entries
            }
        }
        self.emit_history_changed();

        // Force a first frame for the new document (bypasses the throttle).
        self.needs_paint = true;
        self.last_paint = None;
        self.maybe_repaint();
        self.check_meta();
    }

    /// resolve → (drain pending criticals via messages) → resolve, as the
    /// canonical headless flow prescribes (`docs/BLITZ_API_NOTES.md` §1/§3).
    fn settle(&mut self) {
        let Some(doc) = self.doc.as_mut() else { return };
        doc.resolve(0.);
        for _ in 0..8 {
            if !doc.has_pending_critical_resources() {
                break;
            }
            doc.handle_messages();
            doc.resolve(0.);
        }
        doc.handle_messages();
        doc.resolve(0.);
    }

    fn resolve_and_maybe_paint(&mut self) {
        if self.doc.is_none() {
            return;
        }
        self.settle();
        self.maybe_repaint();
        self.check_meta();
    }

    // -- painting -----------------------------------------------------------

    /// Paint the document into an RGBA8 buffer and emit `FromTab::Frame`,
    /// coalesced to at most ~30 fps. Invisible tabs drop paint requests
    /// (they stay pending via `needs_paint` until the next visible repaint).
    fn maybe_repaint(&mut self) {
        if !self.needs_paint {
            return;
        }
        if !self.visible {
            return; // pending: flushed on SetVisible(true)
        }
        if let Some(last) = self.last_paint {
            if last.elapsed() < FRAME_INTERVAL {
                return; // coalesce; the tick loop retries once due
            }
        }
        self.paint_now();
    }

    fn paint_now(&mut self) {
        let Some(_doc) = self.doc.as_mut() else { return };
        let (width, height) = ((self.width as f32 * self.scale) as u32, (self.height as f32 * self.scale) as u32);
        if width == 0 || height == 0 {
            return;
        }
        let scale = self.scale;
        let buffer = render_to_buffer::<VelloCpuImageRenderer, _>(
            |scene| {
                scene.fill(
                    Fill::NonZero,
                    Affine::IDENTITY,
                    Color::from_rgb8(255, 255, 255),
                    None,
                    &Rect::from_origin_size((0.0, 0.0), Size::new(f64::from(width), f64::from(height))),
                );
                // `&mut HtmlDocument` coerces to `&mut BaseDocument` via DerefMut.
                paint_scene(
                    scene,
                    self.doc.as_mut().expect("doc"),
                    scale as f64,
                    width,
                    height,
                    0,
                    0,
                );
            },
            width,
            height,
        );
        self.last_paint = Some(Instant::now());
        self.needs_paint = false;
        self.emit(FromTab::Frame {
            width,
            height,
            scale,
            rgba: buffer,
        });
    }

    // -- metadata deltas ------------------------------------------------------

    /// Compare title/url against the cached last values and emit
    /// `TitleChanged`/`UrlChanged` on change. The title prefers the value
    /// flushed through the `ShellProvider` hook (which JS/DOM mutations
    /// update), falling back to polling the `<title>` node.
    fn check_meta(&mut self) {
        let mut current_url = self.doc.as_ref().map(|doc| doc.url().as_str().to_owned());
        if current_url.is_none() {
            // Before the first successful load the tab still reports the
            // URL it is trying to show (stable contract for the shell).
            current_url = self.history.get(self.history_index).map(|u| u.as_str().to_owned());
        }
        if current_url != self.last_url {
            self.last_url = current_url;
            let url = self.last_url.clone();
            self.emit(FromTab::UrlChanged { url });
        }

        let hooked = self.shell.title.lock().expect("title mutex").clone();
        let polled = self
            .doc
            .as_ref()
            .and_then(|doc| doc.find_title_node())
            .map(|node| node.text_content());
        let title = hooked.or(polled);
        if Some(&title) != self.last_title.as_ref() {
            self.last_title = Some(title.clone());
            self.emit(FromTab::TitleChanged { title });
        }
    }

    // -- shell event handling -------------------------------------------------

    pub fn handle_to_tab(&mut self, msg: ToTab) -> Option<i32> {
        match msg {
            ToTab::Navigate { url } => {
                self.navigate(&url, NavHistory::Fresh);
            }
            ToTab::Reload => {
                let Some(url) = self.history.get(self.history_index).cloned() else {
                    return None;
                };
                let session = self.history_index;
                self.navigate(url.as_str(), NavHistory::Session(session));
            }
            ToTab::Stop => {} // v1 no-op placeholder, per contract
            ToTab::Back => {
                if self.history_index > 0 {
                    self.history_index -= 1;
                    self.emit_history_changed();
                    let url = self.history[self.history_index].clone();
                    let session = self.history_index;
                    self.navigate(url.as_str(), NavHistory::Session(session));
                }
            }
            ToTab::Forward => {
                if self.history_index + 1 < self.history.len() {
                    self.history_index += 1;
                    self.emit_history_changed();
                    let url = self.history[self.history_index].clone();
                    let session = self.history_index;
                    self.navigate(url.as_str(), NavHistory::Session(session));
                }
            }
            ToTab::Resize {
                width,
                height,
                scale,
            } => {
                self.width = width.max(1);
                self.height = height.max(1);
                self.scale = if scale.is_finite() && scale > 0.0 {
                    scale
                } else {
                    self.scale
                };
                let viewport = self.viewport();
                if let Some(doc) = self.doc.as_mut() {
                    // `set_viewport` is THE resize entry point (no resize()
                    // in beta.2) — rebuilds the Stylo device and clamps scroll.
                    doc.set_viewport(viewport);
                }
                self.needs_paint = true;
                self.last_paint = None; // force a frame at the new size
                self.resolve_and_maybe_paint();
            }
            ToTab::SetVisible { visible } => {
                self.visible = visible;
                if visible {
                    self.needs_paint = true;
                    self.last_paint = None;
                    self.resolve_and_maybe_paint();
                }
            }
            ev @ (ToTab::Pointer(_) | ToTab::Key(_) | ToTab::Scroll(_)) => {
                self.handle_input(&ev);
            }
            ToTab::Destroy => {
                self.persist();
                return Some(0);
            }
        }
        None
    }

    /// Input extension point (task 7d): map `ToTab::Pointer/Key/Scroll` onto
    /// `blitz_traits::events::UiEvent` (`PointerMove/Down/Up`, `KeyDown`,
    /// `Wheel` — see `docs/BLITZ_API_NOTES.md` §4), feed
    /// `doc.handle_ui_event(ev)`, then `resolve_and_maybe_paint()`. Link
    /// clicks already round-trip via the wired `NavHook` (§5): once real
    /// pointer events arrive, the loop drains pending navigations for free.
    fn handle_input(&mut self, _ev: &ToTab) {
        // TODO(task 7d): translate and route the UiEvent here.
    }

    /// Drain a pending `NavigationProvider` request (link click / form POST).
    fn drain_nav_hook(&mut self) -> bool {
        let pending = self.nav.pending.lock().expect("nav mutex").take();
        if let Some(options) = pending {
            // 7d will honour POST bodies; top-level GETs work today.
            self.navigate(options.url.as_str(), NavHistory::Fresh);
            return true;
        }
        false
    }

    fn persist(&mut self) {
        // Best-effort: both jar handles (main-doc + subresource) live inside
        // the sandbox-writable profile dir.
        let _ = self.provider.save_cookies();
        let _ = self.jar.save();
    }

    fn flush_stdout(&mut self) {
        let _ = self.out.writer.flush();
    }
}

/// Net policy is fixed at startup; read it back from the provider so the
/// runtime never carries a second copy.
fn self_provider_policy(provider: &Arc<BlitzNetProvider>) -> NetPolicy {
    provider.policy()
}

/// `blitz_traits::net::NetWaker`: Blitz's own providers wake the doc client
/// on completion (even on error, which has no delivery path); ours pokes the
/// same self-pipe the poll-loop watches.
struct PipeWaker(Arc<SelfPipe>);
impl blitz_traits::net::NetWaker for PipeWaker {
    fn wake(&self, _client_id: usize) {
        self.0.wake();
    }
}

/// What a navigation should do to the URL-only session history.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NavHistory {
    /// Fresh load: truncate forward entries (redirect replaces the entry).
    Fresh,
    /// In-place load of history slot `usize` (Back/Forward/Reload).
    Session(usize),
}

// ---------------------------------------------------------------------------
// stdin pump + main loop

/// Mark stdin non-blocking: the loop only reads when `poll()` says readable,
/// partial frames accumulate in `pending` and can never park the loop.
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

/// `read_frame`-compatible reader over a fixed in-memory frame: the length
/// prefix has already been validated, `len` payload bytes are available.
struct CursorReader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl Read for CursorReader<'_> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        let remaining = &self.buf[self.pos..];
        let n = remaining.len().min(out.len());
        out[..n].copy_from_slice(&remaining[..n]);
        self.pos += n;
        Ok(n)
    }
}

/// Pull everything currently available on stdin into `pending` and handle
/// every complete `ToTab` frame. `Ok(Some(code))` requests shutdown.
fn drain_stdin(tab: &mut Tab, pending: &mut Vec<u8>) -> io::Result<Option<i32>> {
    let fd = io::stdin().as_raw_fd();
    let mut chunk = [0u8; 64 * 1024];
    let mut eof = false;
    loop {
        // SAFETY: read into a valid buffer from stdin.
        let n = unsafe {
            libc::read(
                fd,
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
            eof = true; // Shell closed the pipe: same as Destroy.
            break;
        }
        let err = io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::EAGAIN) || err.raw_os_error() == Some(libc::EWOULDBLOCK)
        {
            break;
        }
        return Err(err);
    }

    loop {
        if pending.len() < 4 {
            break;
        }
        let len = u32::from_le_bytes([pending[0], pending[1], pending[2], pending[3]]) as usize;
        if len > servfox_common::MAX_FRAME_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("frame too large: {len} bytes"),
            ));
        }
        if pending.len() < 4 + len {
            break; // wait for the rest
        }
        // Deserialize exactly one frame (length-prefixed bincode via common).
        let mut reader = CursorReader {
            buf: &pending[..4 + len],
            pos: 0,
        };
        let msg: ToTab = read_frame(&mut reader)?;
        pending.drain(..4 + len);
        if let Some(code) = tab.handle_to_tab(msg) {
            return Ok(Some(code));
        }
    }

    if eof {
        tab.persist();
        return Ok(Some(0));
    }
    Ok(None)
}

/// Poll `[stdin, self-pipe]` until readable or `tick` elapses.
fn poll_inputs(stdin_fd: RawFd, wake_fd: RawFd, tick: Duration) -> bool {
    let mut pfd = [
        libc::pollfd {
            fd: stdin_fd,
            events: libc::POLLIN,
            revents: 0,
        },
        libc::pollfd {
            fd: wake_fd,
            events: libc::POLLIN,
            revents: 0,
        },
    ];
    // SAFETY: two valid pollfd entries.
    let ready = unsafe { libc::poll(pfd.as_mut_ptr(), 2, tick.as_millis() as i32) };
    ready > 0
}

/// Run the tab loop; returns the process exit code.
pub fn run(args: TabArgs) -> i32 {
    if args.no_sandbox {
        eprintln!("servfox-tab: sandbox disabled (--no-sandbox)");
    } else {
        // fds (stdin/stdout/profile) survive Landlock: apply touches the
        // filesystem view only, already-open descriptors keep working.
        apply_sandbox(&args);
    }

    let pipe = match SelfPipe::new() {
        Ok(pipe) => Arc::new(pipe),
        Err(err) => {
            let mut out = TabOut::new();
            out.send(FromTab::Error {
                message: format!("self-pipe: {err}"),
            });
            return 1;
        }
    };
    let mut tab = Tab::new(&args, Arc::clone(&pipe));

    if set_stdin_nonblocking().is_err() {
        // A blocking stdin would deadlock the poll loop; without a usable
        // descriptor we cannot speak IPC anyway — degrade by exiting.
        eprintln!("servfox-tab: stdin non-blocking failed");
        return 1;
    }

    tab.emit(FromTab::Ready { tab_id: args.tab_id });

    // Initial navigation (the shell may still send Navigate later; the
    // start URL mirrors the old runtime's behaviour of loading eagerly).
    let start = args.url.clone();
    tab.navigate(&start, NavHistory::Fresh);

    let stdin_fd = io::stdin().as_raw_fd();
    let wake_fd = pipe.read_fd();
    let mut pending: Vec<u8> = Vec::new();
    let mut exit_code = 0;

    loop {
        let tick = if tab.needs_paint {
            FRAME_INTERVAL
        } else {
            IDLE_TICK
        };
        poll_inputs(stdin_fd, wake_fd, tick);
        // Wake-ups from ShellProvider::request_redraw / the NetWaker.
        let woken = pipe.drain() > 0;
        let mut stdin_frames = false;

        // 1. Shell messages.
        match drain_stdin(&mut tab, &mut pending) {
            Ok(Some(code)) => {
                exit_code = code;
                break;
            }
            Ok(None) => {}
            Err(err) => {
                eprintln!("servfox-tab: stdin protocol error: {err}");
                tab.emit(FromTab::Error {
                    message: format!("protocol error: {err}"),
                });
                exit_code = 2;
                break;
            }
        }
        if !pending.is_empty() {
            stdin_frames = true;
        }

        // 2. Link clicks / form submits (NavigationProvider hook).
        if tab.drain_nav_hook() {
            continue;
        }

        // 3. Settle net completions / redraw requests and coalesce frames.
        //    Invisible tabs stay idle — repaints resume on SetVisible(true).
        let paint_due = tab.needs_paint
            && tab
                .last_paint
                .map(|last| last.elapsed() >= FRAME_INTERVAL)
                .unwrap_or(true);
        let activity = woken || stdin_frames;
        if tab.visible && (activity || paint_due) {
            tab.resolve_and_maybe_paint();
        }

        if tab.out.broken {
            // Shell vanished: persist what we can, then die quietly.
            break;
        }
    }

    tab.persist();
    tab.flush_stdout();
    exit_code
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_input_url_accepts_absolute_urls() {
        assert_eq!(
            parse_input_url("file:///tmp/x/index.html")
                .unwrap()
                .as_str(),
            "file:///tmp/x/index.html"
        );
        assert_eq!(
            parse_input_url("https://example.com/").unwrap().as_str(),
            "https://example.com/"
        );
        assert!(parse_input_url("about:blank").is_ok());
    }

    #[test]
    fn parse_input_url_upgrades_bare_hosts() {
        assert_eq!(
            parse_input_url("example.com").unwrap().as_str(),
            "https://example.com/"
        );
        // Bare hosts (Url::parse fails on them) get the https:// upgrade;
        // strings url can read as opaque schemes are kept as-is.
        assert_eq!(
            parse_input_url("example.com/path").unwrap().as_str(),
            "https://example.com/path"
        );
    }

    #[test]
    fn parse_input_url_rejects_free_text() {
        assert!(parse_input_url("not a url").is_err());
        assert!(parse_input_url("").is_err());
    }

    #[test]
    fn tab_args_defaults() {
        let args = TabArgs::parse(&[
            "servfox-tab".to_string(),
            "--tab-id".into(),
            "3".into(),
            "--container-id".into(),
            "2".into(),
            "--profile-dir".into(),
            std::env::temp_dir().join("servfox-tab-args-test").display().to_string(),
        ])
        .expect("parse");
        assert_eq!(args.tab_id, TabId(3));
        assert_eq!(args.container_id, ContainerId(2));
        assert_eq!(args.url, servfox_common::default_url());
        assert_eq!(args.net_policy, NetPolicy::Passthrough);
        assert!(!args.no_sandbox);
        assert!(args.offline_fixtures.is_none());
    }

    #[test]
    fn tab_args_all_flags() {
        let args = TabArgs::parse(
            &[
                "servfox-tab".to_string(),
                "--tab-id".into(),
                "7".into(),
                "--container-id".into(),
                "1".into(),
                "--profile-dir".into(),
                std::env::temp_dir().join("servfox-tab-args-test2").display().to_string(),
                "--url".into(),
                "https://example.org".into(),
                "--offline-fixtures".into(),
                "/tmp/fixtures".into(),
                "--managed-net".into(),
                "--no-sandbox".into(),
            ],
        )
        .expect("parse");
        assert_eq!(args.url, "https://example.org");
        assert_eq!(args.net_policy, NetPolicy::Managed);
        assert!(args.no_sandbox);
        assert_eq!(args.offline_fixtures, Some(PathBuf::from("/tmp/fixtures")));
    }

    #[test]
    fn self_pipe_wakes_and_drains() {
        let pipe = SelfPipe::new().expect("pipe");
        pipe.wake();
        pipe.wake();
        // Data is pending: poll(0) must report readable immediately.
        assert!(poll_inputs(-1, pipe.read_fd(), Duration::from_millis(0)));
        pipe.drain();
        assert!(!poll_inputs(-1, pipe.read_fd(), Duration::from_millis(0)));
    }
}
