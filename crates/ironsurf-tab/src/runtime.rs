//! The Blitz tab runtime (task 7c): sandbox + one `HtmlDocument` per
//! navigation, speaking the `ironsurf-common` IPC protocol over stdin/stdout.
//!
//! Lifecycle per `ARCHITECTURE.md` §ironsurf-tab (Blitz edition):
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
//!
//! RESPONSIVENESS (task 11): a real mouse emits far more hover moves than this
//! single-threaded renderer can afford, so a stdin batch is collapsed to the
//! newest move ([`coalesce_moves`]) and a hover move that lands on the same
//! element skips `resolve` + repaint entirely; hovering an `<a href>` reports
//! its target as `FromTab::StatusText` so links are visibly clickable. See
//! `docs/ENGINE_INTEGRATION.md` §E for the measured before/after numbers.
//!
//! JAVASCRIPT (task 8): after parse + resolve, every navigation runs the
//! document's `<script>`s in one fresh Boa context (`crate::js` — tiny binding
//! surface, `src=` through our own fetcher, exceptions caught). A script's
//! `document.title = …` becomes `FromTab::TitleChanged` + a repaint; script
//! errors, refused `location` writes and `console.*` output become
//! `FromTab::StatusText`. `javascript:` URLs are refused at [`Tab::navigate`],
//! the single navigation entry point.
//!
//! `data:` URLS: a fetched `data:` document goes through [`render_document`]
//! before it reaches the HTML parser, so `data:text/javascript;base64,…` (and
//! other non-markup media types) is displayed as inert source instead of being
//! parsed as HTML — while a page's own `<script src="data:text/javascript;base64,…">`
//! still fetches and runs (`docs/ENGINE_INTEGRATION.md` §G).

use std::cell::RefCell;
use std::error::Error;
use std::io::{self, BufWriter, Read, StdoutLock, Write};
use std::os::fd::AsRawFd;
use std::os::fd::RawFd;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyrender::{render_to_buffer, PaintScene};
use anyrender_vello_cpu::VelloCpuImageRenderer;
use blitz_dom::{Document as _, DocumentConfig};
use blitz_html::HtmlDocument;
use blitz_paint::paint_scene;
use blitz_traits::events::{
    BlitzKeyEvent, BlitzPointerEvent, BlitzPointerId, BlitzWheelDelta, BlitzWheelEvent, KeyState,
    MouseEventButton, MouseEventButtons, Point, PointerCoords, PointerDetails, UiEvent,
};
use blitz_traits::navigation::{NavigationOptions, NavigationProvider};
use blitz_traits::shell::{ColorScheme, ShellProvider, Viewport};
use ironsurf_common::{
    read_frame, write_frame, ContainerId, FromTab, KeyEvent, Modifiers, PointerEvent, PointerKind,
    ScrollEvent, TabId, ToTab,
};
use keyboard_types::{Code, Key, Location, Modifiers as KMods};
use kurbo::{Affine, Rect, Size};
use peniko::{Color, Fill};
// stderr diagnostics (see `ironsurf_common::logging`): `info` = the user-visible
// timeline (navigation, load start/finish, title/url), `debug` = per-step detail,
// `trace` = per-frame/per-input noise. stdout is the IPC channel — never a log.
use ironsurf_common::logging;
use ironsurf_common::{debug, error, info, trace, warn};
use ironsurf_sandbox::SandboxPolicy;
use ironsurf_storage::{ContainerStore, CookieJar, NetCache};
use url::Url;

use crate::js::{self, HostRef};
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
                "--container-id" => container_id = ContainerId(value("container-id")?.parse()?),
                "--profile-dir" => profile_dir = Some(PathBuf::from(value("profile-dir")?)),
                "--url" => url = Some(value("url")?),
                "--offline-fixtures" => {
                    offline_fixtures = Some(PathBuf::from(value("offline-fixtures")?))
                }
                "--managed-net" => managed = true,
                "--no-sandbox" => no_sandbox = true,
                // Verbosity flags: the level itself is read from argv by
                // `logging::level_from_args` in `run`; they are accepted here so
                // passing them is not a fatal "unexpected argument".
                "--log" | "--log-level" => {
                    value("log")?;
                }
                "-v" | "-vv" | "--verbose" | "--debug" => {}
                other if other.starts_with("--log=") => {}
                other => return Err(format!("unexpected argument {other:?}").into()),
            }
        }
        let profile_dir = match profile_dir {
            Some(dir) => dir,
            None => {
                let store = ironsurf_storage::Store::open(ironsurf_storage::default_data_root())?;
                store.profile_dir(container_id)
            }
        };
        std::fs::create_dir_all(&profile_dir)?;

        Ok(TabArgs {
            tab_id,
            container_id,
            profile_dir,
            url: url.unwrap_or_else(ironsurf_common::default_url),
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
        let rc = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_NONBLOCK | libc::O_CLOEXEC) };
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
            libc::write(self.write_fd, byte.as_ptr() as *const libc::c_void, 1);
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
        debug!("document title flushed by blitz: {title:?}");
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
        debug!("page asked to navigate → {}", options.url);
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
    let mut readable: Vec<PathBuf> = ironsurf_sandbox::default_readable_ro()
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
    debug!(
        "applying sandbox: {} readable, writable {}",
        policy.readable_ro.len(),
        policy.writable_rw.len()
    );
    match ironsurf_sandbox::apply(&policy) {
        Ok(status) => info!(
            "sandbox applied: landlock={} rlimits={} notes={:?}",
            status.landlock_active, status.rlimits_applied, status.notes
        ),
        // Sandboxing must never abort the tab (contract): degrade loudly.
        Err(err) => error!("sandbox failed (degraded, continuing): {err}"),
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
            Url::parse(&format!("https://{input}"))
                .map_err(|_| format!("not a URL: {input:?} ({original})").into())
        }
    }
}

// ---------------------------------------------------------------------------
// What the HTML parser actually receives

/// Escape text for use in HTML PCDATA (the source view must show `<script>`
/// tags as text, never as live markup).
fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 16);
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// A read-only viewer for a non-markup document, in the spirit of what a
/// browser shows when you navigate to a script/stylesheet/plain-text URL: a
/// header line plus the source in a `<pre>`.
fn source_view_document(title: &str, meta: &str, source: &str) -> String {
    format!(
        r#"<!doctype html>
<html><head><meta charset="utf-8"><title>{title}</title></head>
<body style="margin: 0; background: #ffffff; color: #101010; font-family: monospace">
  <div style="padding: 8px 12px; background: #f0f0f0; border-bottom: 1px solid #d0d0d0; font-size: 12px">
    {meta}
  </div>
  <pre style="margin: 0; padding: 12px; font-size: 13px; white-space: pre-wrap; word-break: break-word">{source}</pre>
</body></html>
"#,
        title = escape_html(title),
        meta = escape_html(meta),
        source = escape_html(source),
    )
}

/// Decide what bytes the HTML parser should receive for a fetched main
/// document.
///
/// `data:` URLs carry their own media type, and most of them are **not**
/// markup. `data:text/javascript;base64,…` (and every other script/stylesheet/
/// JSON/`text/*` payload, base64 or percent-encoded) is displayed as inert
/// source: parsing it as HTML would paint the program text as a page of
/// garbage, and evaluating it would be a `javascript:` URL by another name —
/// those the tab refuses by contract. Markup types (`text/html`, xhtml, svg,
/// xml) and every other scheme (http(s), file, about) are handed to the parser
/// exactly as before, and subresources are unaffected: a page may still load
/// `data:text/javascript;base64,…` through `<script src>`, which is where
/// running that program is the page's own decision.
pub fn render_document(fetch: &netmod::DocFetch) -> String {
    if let Some(data) = netmod::parse_data_url(&fetch.url) {
        if data.renders_as_source() {
            let source = data.text();
            debug!(
                "data: URL is {} ({} B of source) — rendering the source view, not parsing it",
                data.mime,
                data.bytes.len()
            );
            return source_view_document(
                &data.mime,
                &format!(
                    "{} · {} · {} bytes · {}",
                    data.mime,
                    if data.base64 {
                        "base64"
                    } else {
                        "percent-encoded"
                    },
                    data.bytes.len(),
                    fetch.url.as_str(),
                ),
                &source,
            );
        }
        debug!("data: URL is {} — parsing as markup", data.mime);
    }
    fetch.body_text()
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
    /// `document.title` as last assigned by page JavaScript (task 8). Wins over
    /// the `ShellProvider`/`<title>` node sources while the document is live.
    js_title: Option<String>,
    /// Status text raised by the script run (errors, refused `location` writes,
    /// `console.*`), emitted once per navigation after the load-finished clear.
    js_status: Option<String>,
    /// Paint state: set when a resolve produced (or may have produced) new
    /// pixels; coalesced into at most ~30 fps of `FromTab::Frame`.
    needs_paint: bool,
    last_paint: Option<Instant>,
    offline_fixtures: Option<PathBuf>,
    /// Shared wake pipe (owned alongside the hooks; poked by ShellProvider /
    /// NetWaker worker threads).
    #[allow(dead_code)]
    pipe: Arc<SelfPipe>,
    /// Which mouse buttons are currently held (ironsurf `PointerKind::Down/Up`
    /// track the transitions); carried in every `BlitzPointerEvent.buttons`.
    mouse_buttons: MouseEventButtons,
    /// Last hover-reported link target (`StatusText`), for change-detection so
    /// a moving mouse only emits when the target actually changes.
    hover_status: Option<String>,
}

/// stdout side of the IPC: one persistent BufWriter, flushed per frame.
/// In headless harness mode the sink is `/dev/null` instead (see
/// [`Tab::new_headless`]) so the process never speaks the protocol.
struct TabOut {
    writer: Box<dyn Write>,
    broken: bool,
}

impl TabOut {
    fn new() -> Self {
        let stdout: StdoutLock<'static> = io::stdout().lock();
        TabOut {
            writer: Box::new(BufWriter::with_capacity(1 << 20, stdout)),
            broken: false,
        }
    }

    /// Discarding sink (headless mode): `send` keeps working, frames go nowhere.
    fn null() -> Self {
        TabOut {
            writer: Box::new(BufWriter::new(io::sink())),
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
    /// A tab wired exactly like a live one (same net provider, hooks, jar,
    /// cache, script pipeline) but with the shell sink discarded: used by the
    /// `--headless` ACID-test harness, which navigates once, waits, and dumps
    /// DOM text + a screenshot instead of streaming frames (see
    /// [`crate::headless`]).
    pub(crate) fn new_headless(args: &TabArgs, pipe: Arc<SelfPipe>) -> Self {
        let mut tab = Tab::new(args, pipe);
        tab.out = TabOut::null();
        tab
    }

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
            js_title: None,
            js_status: None,
            needs_paint: false,
            last_paint: None,
            offline_fixtures: args.offline_fixtures.clone(),
            pipe,
            mouse_buttons: MouseEventButtons::None,
            hover_status: None,
        }
    }

    /// The single tab→shell choke point. Every message is logged except frames,
    /// which are `trace`-only because they fire up to 30× a second.
    fn emit(&mut self, msg: FromTab) {
        match &msg {
            FromTab::Frame {
                width,
                height,
                scale,
                rgba,
            } => trace!(
                "frame {}x{}@{scale:.2} ({} B rgba)",
                width,
                height,
                rgba.len()
            ),
            FromTab::UrlChanged { url } => {
                info!("url → {}", url.as_deref().unwrap_or("<none>"))
            }
            FromTab::TitleChanged { title } => {
                info!("title → {}", title.as_deref().unwrap_or("<none>"))
            }
            FromTab::LoadingChanged { loading } => {
                info!("loading {}", if *loading { "started" } else { "finished" })
            }
            FromTab::Error { message } => error!("to shell: {message}"),
            FromTab::ConsoleLog { level, message } => info!("to shell: console[{level}] {message}"),
            other => debug!("to shell: {other:?}"),
        }
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
        let started = Instant::now();
        info!("navigate \"{raw}\" ({history:?})");
        let url = match parse_input_url(raw) {
            Ok(url) => url,
            Err(err) => {
                warn!("bad url {raw:?}: {err}");
                self.emit(FromTab::Error {
                    message: format!("bad url {raw:?}: {err}"),
                });
                return false;
            }
        };
        if url.scheme() == "javascript" {
            // Contract: javascript: URLs are refused with a status text.
            warn!("refused javascript: URL {raw}");
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
            // Host when we have one; otherwise the URL itself, with `data:`
            // payloads reduced to a length so a multi-MB inline asset cannot
            // spray the status bar.
            text: Some(format!(
                "Waiting for {}",
                if url.scheme() == netmod::INTERNAL_SCHEME {
                    // `ironsurf://welcome` has an opaque host ("welcome"); the
                    // full URL is the meaningful thing to show.
                    netmod::url_for_log(&url)
                } else {
                    url.host_str()
                        .map(str::to_owned)
                        .unwrap_or_else(|| netmod::url_for_log(&url))
                }
            )),
        });

        let url = self.apply_offline_fixtures(url);
        debug!(
            "fetching main document {url} (net policy {:?})",
            self_provider_policy(&self.provider)
        );
        match netmod::fetch_document(
            &url,
            self_provider_policy(&self.provider),
            &mut self.jar,
            &self.cache,
            netmod::now_unix(),
        ) {
            Ok(doc_fetch) => {
                debug!(
                    "document fetched: {} status {} ({} B) in {} ms",
                    doc_fetch.url,
                    doc_fetch.status,
                    doc_fetch.body.len(),
                    started.elapsed().as_millis()
                );
                let html = render_document(&doc_fetch);
                self.load_html(&doc_fetch.url, &html, history);
                info!(
                    "loaded {} in {} ms",
                    doc_fetch.url,
                    started.elapsed().as_millis()
                );
                self.emit(FromTab::LoadingChanged { loading: false });
                self.emit(FromTab::StatusText { text: None });
                // Script output outranks the "load finished" clear (task 8).
                self.emit_js_status();
                true
            }
            Err(err) => {
                // Fetch failure: report, keep the previous document on screen.
                match err.error_page() {
                    Some(page) => {
                        // Blocked https-upgrade: render the block page at the
                        // requested URL (it stays in history as the user typed it).
                        warn!("navigation blocked by policy: {err}");
                        self.load_html(&url, &page, history);
                        self.emit(FromTab::LoadingChanged { loading: false });
                        self.emit_js_status();
                        false
                    }
                    None => {
                        error!(
                            "navigation to {url} failed after {} ms: {err}",
                            started.elapsed().as_millis()
                        );
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

    /// Build a fresh `HtmlDocument` for an already-parsed document, run its
    /// scripts, drive it to a first paint; then emit the metadata deltas
    /// (url/title/history).
    fn load_html(&mut self, url: &Url, html: &str, history: NavHistory) {
        // A fresh doc must not inherit the previous doc's flushed title.
        *self.shell.title.lock().expect("title mutex") = None;
        self.js_title = None;
        self.js_status = None;
        // The new document has nothing under the cursor yet; the next hover
        // transition must be allowed to report again.
        self.hover_status = None;
        let config = DocumentConfig {
            base_url: Some(url.as_str().to_owned()),
            viewport: Some(self.viewport()),
            net_provider: Some(
                Arc::clone(&self.provider) as Arc<dyn blitz_traits::net::NetProvider>
            ),
            navigation_provider: Some(
                Arc::clone(&self.nav) as Arc<dyn blitz_traits::navigation::NavigationProvider>
            ),
            shell_provider: Some(
                Arc::clone(&self.shell) as Arc<dyn blitz_traits::shell::ShellProvider>
            ),
            ..Default::default()
        };
        let parsed = Instant::now();
        let document = HtmlDocument::from_html(html, config);
        self.doc = Some(document);
        self.settle();
        debug!(
            "document parsed+settled for {url} ({} B html) in {} ms",
            html.len(),
            parsed.elapsed().as_millis()
        );
        // JavaScript runs on the settled DOM (ARCHITECTURE §JavaScript (Boa)).
        self.run_scripts(url);

        match history {
            NavHistory::Fresh => {
                // Re-visiting an entry jumps to it; a new URL is pushed and
                // becomes current (previously the index stayed behind, which
                // left Back dead after shell-driven navigations).
                match self.history.iter().position(|h| h == url) {
                    Some(pos) => self.history_index = pos,
                    None => {
                        self.history.push(url.clone());
                        self.history_index = self.history.len() - 1;
                    }
                }
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

    // -- javascript ---------------------------------------------------------

    /// Run the freshly parsed document's `<script>`s (task 8).
    ///
    /// Scripts are collected in document order; `src=` bodies go through the
    /// tab's own document fetcher, so the container net policy (cookies, cache,
    /// https-upgrade) applies to them too. One `boa` context is built per
    /// navigation and thrown away with the host afterwards. Nothing here can
    /// kill the tab: extraction failures degrade to "no scripts", and every
    /// `eval` is caught inside [`js::run_scripts`].
    fn run_scripts(&mut self, url: &Url) {
        let policy = self_provider_policy(&self.provider);
        let mut extraction = js::Extraction::default();
        let mut body_text = None;
        if let Some(doc) = self.doc.as_ref() {
            // Disjoint field borrows: `doc` stays immutable while the fetcher
            // mutably borrows the jar (subresources may also flow through the
            // Blitz provider, which owns its own handle to the same state).
            let mut fetch = |script_url: &Url| -> Result<String, String> {
                netmod::fetch_document(
                    script_url,
                    policy,
                    &mut self.jar,
                    &self.cache,
                    netmod::now_unix(),
                )
                .map(|script| script.body_text())
                .map_err(|err| err.to_string())
            };
            extraction = js::extract_scripts(doc, url, &mut fetch);
            body_text = js::body_text_snapshot(doc);
        }

        // The title JS starts from is the one blitz parsed for this document.
        let parsed_title = self
            .shell
            .title
            .lock()
            .expect("title mutex")
            .clone()
            .or_else(|| {
                self.doc
                    .as_ref()
                    .and_then(|doc| doc.find_title_node())
                    .map(|node| node.text_content())
            });
        let host: HostRef = HostRef::new(RefCell::new(js::HostState::new(
            url.as_str(),
            parsed_title.as_deref(),
            body_text,
        )));
        debug!(
            "running {} script(s) for {url}; skipped: {:?}",
            extraction.scripts.len(),
            extraction.notes
        );
        let js_started = Instant::now();
        let errors = js::run_scripts(&extraction.scripts, &host);
        debug!(
            "script run complete: {} error(s) in {} ms",
            errors.len(),
            js_started.elapsed().as_millis()
        );

        // Script side effects -> IPC. The title wins over `<title>` polling
        // (`check_meta` prefers `js_title`), and needs a fresh frame.
        if let Some(title) = host.borrow().title_assigned.clone() {
            self.js_title = Some(title);
            self.needs_paint = true;
        }
        // One status text per navigation: script errors outrank `console.*`
        // output, which outranks the "skipped script / not implemented" notes.
        let console_last = host
            .borrow()
            .console
            .last()
            .map(|(level, text)| format!("js console {level}: {text}"));
        let note = host
            .borrow()
            .latest_note()
            .map(str::to_owned)
            .or_else(|| extraction.notes.first().map(String::to_owned));
        self.js_status = js::status_text_for_errors(&errors)
            .or(console_last)
            .or(note);
    }

    /// Flush the status text the last script run raised. Called after the
    /// load-finished `StatusText(None)` so the shell actually gets to show it.
    fn emit_js_status(&mut self) {
        if let Some(text) = self.js_status.take() {
            self.emit(FromTab::StatusText { text: Some(text) });
        }
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
        let Some(_doc) = self.doc.as_mut() else {
            return;
        };
        let (width, height) = (
            (self.width as f32 * self.scale) as u32,
            (self.height as f32 * self.scale) as u32,
        );
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
                    &Rect::from_origin_size(
                        (0.0, 0.0),
                        Size::new(f64::from(width), f64::from(height)),
                    ),
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
            current_url = self
                .history
                .get(self.history_index)
                .map(|u| u.as_str().to_owned());
        }
        if current_url != self.last_url {
            self.last_url = current_url;
            let url = self.last_url.clone();
            self.emit(FromTab::UrlChanged { url });
        }

        let js = self.js_title.clone();
        let hooked = self.shell.title.lock().expect("title mutex").clone();
        let polled = self
            .doc
            .as_ref()
            .and_then(|doc| doc.find_title_node())
            .map(|node| node.text_content());
        let title = js.or(hooked).or(polled);
        if Some(&title) != self.last_title.as_ref() {
            self.last_title = Some(title.clone());
            self.emit(FromTab::TitleChanged { title });
        }
    }

    // -- shell event handling -------------------------------------------------

    pub fn handle_to_tab(&mut self, msg: ToTab) -> Option<i32> {
        // Raw shell traffic: commands are `debug`, the input flood (every
        // mouse packet) is `trace` so `--log debug` stays readable.
        match &msg {
            ToTab::Pointer(_) | ToTab::Key(_) | ToTab::Scroll(_) => trace!("from shell: {msg:?}"),
            other => debug!("from shell: {other:?}"),
        }
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

    /// Input entry point (task 7d): map `ToTab::Pointer/Key/Scroll` onto
    /// `blitz_traits::events::UiEvent` and feed `document.handle_ui_event`,
    /// then `resolve` + coalesced repaint (`docs/BLITZ_API_NOTES.md` §4).
    ///
    /// Coordinate space: ironsurf shell coordinates are logical px (the same
    /// space as `ToTab::Resize`'s `width`/`height`); blitz hit-tests and
    /// default actions run in **CSS px**, which for `Viewport::new(w*scale,
    /// h*scale, scale, …)` equals those logical px. `screen`/`client` are the
    /// unscrolled viewport position, `page` adds the document scroll offset
    /// (what blitz's hover/click hit-testing consumes, events/driver.rs).
    fn handle_input(&mut self, ev: &ToTab) {
        // Must this event produce pixels? A plain hover move only does when it
        // changes what is under the cursor: a mouse travelling across the page
        // fires one event per device packet, and each `resolve` + vello_cpu
        // repaint costs hundreds of milliseconds — unconditionally repainting
        // buried the click that follows under seconds of stale frames.
        let mut paint = true;
        match ev {
            ToTab::Pointer(pointer) => paint = self.handle_pointer(pointer),
            ToTab::Key(key) => self.handle_key(key),
            ToTab::Scroll(scroll) => self.handle_scroll(scroll),
            _ => {}
        }
        // Links announce themselves: hovering an `<a href>` puts its target in
        // the status bar, leaving it clears that again.
        self.report_hover();
        if !paint {
            return;
        }
        // Every handled input wants fresh pixels (contract §ironsurf-tab.3);
        // still coalesced by the ≤30 fps throttle in `maybe_repaint`.
        self.needs_paint = true;
        // A link click filled the NavigationProvider hook inline; skip painting
        // the outgoing page — the loop's `drain_nav_hook` replaces the
        // document (and paints the incoming one) right after this returns.
        if self.nav.pending.lock().expect("nav mutex").is_none() {
            self.resolve_and_maybe_paint();
        }
    }

    /// Nearest `<a href>` target under the cursor, resolved against the
    /// document URL, as `(hovered_node, target)`. `None` target = not over a
    /// link; a `#fragment` target is kept (blitz scrolls in place for those).
    fn hover_link(&self) -> Option<String> {
        let doc = self.doc.as_ref()?;
        let mut node_id = doc.get_hover_node_id()?;
        while let Some(node) = doc.get_node(node_id) {
            if let Some(el) = node.element_data() {
                if el.name.local == blitz_dom::local_name!("a") {
                    if let Some(href) = el.attr(blitz_dom::local_name!("href")) {
                        let href = href.trim();
                        if !href.is_empty() {
                            let resolved = doc
                                .url()
                                .join(href)
                                .map(|u| u.to_string())
                                .unwrap_or_else(|_| href.to_owned());
                            return Some(resolved);
                        }
                    }
                }
            }
            node_id = node.parent?;
        }
        None
    }

    /// Emit `FromTab::StatusText` when the hovered link target changes (and
    /// only then) — this is the user-visible signal that the page is not a
    /// picture: hovering a link shows where the click would take them.
    fn report_hover(&mut self) {
        let target = self.hover_link();
        if target == self.hover_status {
            return;
        }
        self.hover_status = target.clone();
        self.emit(FromTab::StatusText { text: target });
    }

    /// Pointer coordinates (logical px) → blitz `PointerCoords` (CSS px,
    /// with the document scroll added into the page axes).
    fn pointer_coords(&mut self, x: f32, y: f32) -> PointerCoords {
        // `viewport_scroll` is a `blitz_dom::util::Point<f64>` (no Default impl).
        let (scroll_x, scroll_y) = self
            .doc
            .as_ref()
            .map(|doc| {
                let s = doc.viewport_scroll();
                (s.x as f32, s.y as f32)
            })
            .unwrap_or((0.0, 0.0));
        PointerCoords {
            page_x: x + scroll_x,
            page_y: y + scroll_y,
            screen_x: x,
            screen_y: y,
            client_x: x,
            client_y: y,
        }
    }

    /// Feed one pointer event to blitz. Returns whether fresh pixels are
    /// wanted: presses/releases always are (active/focus/caret state), a bare
    /// hover move only when it moved onto a different element.
    fn handle_pointer(&mut self, ev: &PointerEvent) -> bool {
        use ironsurf_common::MouseButton;

        let button = match ev.button {
            MouseButton::Left => MouseEventButton::Main,
            MouseButton::Right => MouseEventButton::Secondary,
            MouseButton::Middle => MouseEventButton::Auxiliary,
        };
        // Track the held-button set the way the DOM does: Down adds this
        // button, Up clears it (blitz reads `buttons` for drag/caret state,
        // events/mod.rs `may_move_text_input_caret`).
        match ev.kind {
            PointerKind::Down => self.mouse_buttons |= MouseEventButtons::from(button),
            PointerKind::Up => self.mouse_buttons = MouseEventButtons::None,
            _ => {}
        }

        // Middle-click release over a link → "open in new tab": the Up event
        // is deliberately *not* fed to blitz, so no DOM `click` fires, the
        // NavigationProvider stays empty, and this tab keeps its document.
        // The shell spawns the sibling tab (same container) on this message.
        if matches!(ev.kind, PointerKind::Up) && ev.button == MouseButton::Middle {
            if let Some(url) = self.hover_link() {
                self.emit(FromTab::OpenLinkInNewTab { url });
                return true;
            }
        }

        // blitz synthesises enter/over/out/leave *DOM events* from
        // `PointerMove` (events/driver.rs::handle_pointer_move); there is no
        // Over/Out UiEvent, so ironsurf hover transitions arrive as moves.
        let hovering = matches!(
            ev.kind,
            PointerKind::Move | PointerKind::Over | PointerKind::Out
        ) && self.mouse_buttons.is_empty();
        let hover_before = self.doc.as_ref().and_then(|doc| doc.get_hover_node_id());

        let ui = match ev.kind {
            PointerKind::Move | PointerKind::Over | PointerKind::Out => {
                // Hover transitions carry the currently-held buttons (none for
                // a plain move), not the `button` field of the frame.
                UiEvent::PointerMove(self.pointer_event(ev, self.mouse_buttons))
            }
            PointerKind::Down => UiEvent::PointerDown(self.pointer_event(ev, self.mouse_buttons)),
            // On release `buttons` must already have the button cleared.
            PointerKind::Up => UiEvent::PointerUp(self.pointer_event(ev, MouseEventButtons::None)),
        };
        self.feed_ui_event(ui);

        if hovering {
            let hover_after = self.doc.as_ref().and_then(|doc| doc.get_hover_node_id());
            hover_before != hover_after
        } else {
            true
        }
    }

    fn pointer_event(
        &mut self,
        ev: &PointerEvent,
        buttons: MouseEventButtons,
    ) -> BlitzPointerEvent {
        BlitzPointerEvent {
            id: BlitzPointerId::Mouse,
            is_primary: true,
            coords: self.pointer_coords(ev.x, ev.y),
            button: match ev.button {
                ironsurf_common::MouseButton::Left => MouseEventButton::Main,
                ironsurf_common::MouseButton::Right => MouseEventButton::Secondary,
                ironsurf_common::MouseButton::Middle => MouseEventButton::Auxiliary,
            },
            buttons,
            mods: KMods::empty(), // ironsurf PointerEvent carries no modifiers
            details: PointerDetails::default(),
            // The event driver overwrites `element` from the target's
            // bounding rect (driver.rs::adjust_element_coords).
            element: Point::default(),
            // Mouse input: the shared active-pointer (multi-touch) list stays empty.
            active_pointers: Default::default(),
        }
    }

    /// Keyboard input: `ToTab::Key` is a key *press* (the contract has no
    /// release direction). blitz beta.2 has no separate `UnicodeInput`
    /// variant — printable text rides on `BlitzKeyEvent.text` and the whole
    /// default action (Tab focus moves, Ctrl+C, text-input edits) consumes
    /// `UiEvent::KeyDown` (events/mod.rs:211). We emit the matching
    /// `KeyUp` afterwards purely for DOM-fidelity (its default action is a
    /// no-op, events/mod.rs:222); there is no release in the contract to
    /// model beyond that.
    fn handle_key(&mut self, ev: &KeyEvent) {
        let key = ev.key.parse::<Key>().unwrap_or(Key::Unidentified);
        let code = ev.code.parse::<Code>().unwrap_or(Code::Unidentified);
        let text = ev
            .text
            .as_deref()
            .filter(|t| !t.is_empty())
            .map(blitz_traits::SmolStr::from);
        let base = BlitzKeyEvent {
            key,
            code,
            modifiers: to_blitz_mods(ev.mods),
            location: Location::Standard,
            is_auto_repeating: ev.repeat,
            is_composing: false,
            state: KeyState::Pressed,
            text,
        };
        self.feed_ui_event(UiEvent::KeyDown(base.clone()));
        let release = BlitzKeyEvent {
            state: KeyState::Released,
            is_auto_repeating: false,
            // The release carries no committed text (DOM semantics).
            text: None,
            ..base
        };
        self.feed_ui_event(UiEvent::KeyUp(release));
    }

    /// Wheel/scroll: blitz's default action scrolls by `Lines(x, y) * 20`
    /// CSS px or `Pixels(x, y)` verbatim (events/pointer.rs::handle_wheel;
    /// positive delta = down/right). The contract doesn't pin units, so: deltas
    /// with magnitude ≤ 3 are treated as wheel notches (lines), everything
    /// else as CSS-px pixel deltas.
    fn handle_scroll(&mut self, ev: &ScrollEvent) {
        let delta = if ev.delta_x.abs().max(ev.delta_y.abs()) <= 3.0 {
            BlitzWheelDelta::Lines(ev.delta_x as f64, ev.delta_y as f64)
        } else {
            BlitzWheelDelta::Pixels(ev.delta_x as f64, ev.delta_y as f64)
        };
        let ui = UiEvent::Wheel(BlitzWheelEvent {
            delta,
            coords: self.pointer_coords(ev.x, ev.y),
            buttons: self.mouse_buttons,
            mods: KMods::empty(),
            element: Point::default(),
        });
        self.feed_ui_event(ui);
    }

    /// The single document-input funnel: everything below (7e Boa scripting,
    /// synthetic events) should reach blitz through here.
    fn feed_ui_event(&mut self, ui: UiEvent) {
        if let Some(doc) = self.doc.as_mut() {
            doc.handle_ui_event(ui);
        }
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

    pub(crate) fn persist(&mut self) {
        // Best-effort: both jar handles (main-doc + subresource) live inside
        // the sandbox-writable profile dir.
        debug!("persisting cookie jars to profile dir");
        if let Err(err) = self.provider.save_cookies() {
            warn!("subresource cookie jar not saved: {err}");
        }
        if let Err(err) = self.jar.save() {
            warn!("main-document cookie jar not saved: {err}");
        }
    }

    fn flush_stdout(&mut self) {
        let _ = self.out.writer.flush();
    }

    /// One harness pump: drain a pending navigation request, else settle
    /// (resolve + document messages) and refresh the title/url caches. Timers
    /// and subresource completions land through here while `--wait` runs.
    pub(crate) fn headless_pump(&mut self) {
        if self.drain_nav_hook() {
            return;
        }
        self.settle();
        self.check_meta();
    }

    /// The live document, for harness-side DOM/serializer access.
    pub(crate) fn document_mut(&mut self) -> Option<&mut blitz_html::HtmlDocument> {
        self.doc.as_mut()
    }

    /// Title with the `check_meta` precedence (JS assignment > blitz flush >
    /// `<title>` node poll), resolved on demand for the harness summary.
    pub(crate) fn title_now(&mut self) -> Option<String> {
        let js = self.js_title.clone();
        let hooked = self.shell.title.lock().expect("title mutex").clone();
        let polled = self
            .doc
            .as_ref()
            .and_then(|doc| doc.find_title_node())
            .map(|node| node.text_content());
        js.or(hooked).or(polled)
    }
}

/// Translate the ironsurf modifier bits (`ironsurf_common::Modifiers`, its own
/// `u8` layout) into `keyboard_types::Modifiers` — the two bit layouts differ,
/// so this must never be a cast (`docs/BLITZ_API_NOTES.md` §4).
fn to_blitz_mods(m: Modifiers) -> KMods {
    let mut k = KMods::empty();
    if m.ctrl() {
        k |= KMods::CONTROL;
    }
    if m.shift() {
        k |= KMods::SHIFT;
    }
    if m.alt() {
        k |= KMods::ALT;
    }
    if m.meta() {
        k |= KMods::META;
    }
    k
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
        let n = unsafe { libc::read(fd, chunk.as_mut_ptr() as *mut libc::c_void, chunk.len()) };
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

    // Decode every complete frame that is available right now, then dispatch
    // the batch with superseded hover moves dropped (see [`coalesce_moves`]).
    let mut batch: Vec<ToTab> = Vec::new();
    loop {
        if pending.len() < 4 {
            break;
        }
        let len = u32::from_le_bytes([pending[0], pending[1], pending[2], pending[3]]) as usize;
        if len > ironsurf_common::MAX_FRAME_BYTES {
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
        batch.push(msg);
    }

    for msg in coalesce_moves(batch) {
        if let Some(code) = tab.handle_to_tab(msg) {
            return Ok(Some(code));
        }
    }

    if eof {
        debug!("stdin EOF: shell closed the pipe, persisting and exiting");
        tab.persist();
        return Ok(Some(0));
    }
    Ok(None)
}

/// Is this a bare pointer move (no buttons involved)?
fn is_move(msg: &ToTab) -> bool {
    matches!(
        msg,
        ToTab::Pointer(PointerEvent { kind, .. })
            if matches!(kind, PointerKind::Move | PointerKind::Over | PointerKind::Out)
    )
}

/// Collapse runs of consecutive hover moves down to the newest one.
///
/// A mouse travelling across the page emits far more move events per second
/// than the tab can hit-test and repaint, and every stale position it does
/// process delays the click that follows it. Only the final coordinate of an
/// uninterrupted run of moves is worth resolving; presses, releases, keys,
/// scrolls and every shell command keep their order and are never dropped.
fn coalesce_moves(msgs: Vec<ToTab>) -> Vec<ToTab> {
    let mut out: Vec<ToTab> = Vec::with_capacity(msgs.len());
    for msg in msgs {
        if is_move(&msg) && out.last().is_some_and(is_move) {
            // Replace a pending move; a move following any other event is kept.
            let last = out.last_mut().expect("last is a move");
            *last = msg;
            continue;
        }
        out.push(msg);
    }
    out
}

#[cfg(test)]
mod input_shaping_tests {
    use super::*;

    fn move_at(x: f32, y: f32) -> ToTab {
        ToTab::Pointer(PointerEvent {
            x,
            y,
            kind: PointerKind::Move,
            button: ironsurf_common::MouseButton::Left,
        })
    }

    fn press_at(x: f32, y: f32) -> ToTab {
        ToTab::Pointer(PointerEvent {
            x,
            y,
            kind: PointerKind::Down,
            button: ironsurf_common::MouseButton::Left,
        })
    }

    #[test]
    fn a_run_of_moves_collapses_to_the_newest() {
        let batch = vec![move_at(1.0, 1.0), move_at(2.0, 2.0), move_at(3.0, 3.0)];
        let out = coalesce_moves(batch);
        assert_eq!(out.len(), 1);
        assert_eq!(
            out[0],
            move_at(3.0, 3.0),
            "only the newest position of a move run survives"
        );
    }

    #[test]
    fn presses_and_commands_are_never_dropped_or_reordered() {
        let batch = vec![
            move_at(1.0, 1.0),
            move_at(2.0, 2.0),
            press_at(2.0, 2.0),
            move_at(4.0, 4.0),
            ToTab::Reload,
            move_at(5.0, 5.0),
            move_at(6.0, 6.0),
        ];
        let out = coalesce_moves(batch);
        assert_eq!(
            out,
            vec![
                move_at(2.0, 2.0),
                press_at(2.0, 2.0),
                move_at(4.0, 4.0),
                ToTab::Reload,
                move_at(6.0, 6.0)
            ],
            "the click and the shell command survive in order"
        );
    }
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
    // Label every line with this tab id; level comes from `--log`/`-v`, then
    // `IRONSURF_LOG`/`RUST_LOG` (inherited from the shell), then `info`.
    let argv: Vec<String> = std::env::args().collect();
    let level = logging::init(
        &format!("tab {}", args.tab_id.0),
        logging::level_from_args(&argv),
    );
    info!(
        "starting: container {} profile {} start url {:?} net policy {:?} sandbox {} log {:?}",
        args.container_id.0,
        args.profile_dir.display(),
        args.url,
        args.net_policy,
        !args.no_sandbox,
        level,
    );
    if args.no_sandbox {
        warn!("sandbox disabled (--no-sandbox): this tab is not confined");
    } else {
        // fds (stdin/stdout/profile) survive Landlock: apply touches the
        // filesystem view only, already-open descriptors keep working.
        apply_sandbox(&args);
    }

    let pipe = match SelfPipe::new() {
        Ok(pipe) => Arc::new(pipe),
        Err(err) => {
            error!("cannot create the wake self-pipe: {err}");
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
        error!("stdin non-blocking failed: cannot run the poll loop");
        return 1;
    }

    tab.emit(FromTab::Ready {
        tab_id: args.tab_id,
    });

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
                error!("stdin protocol error: {err}");
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
            debug!("stdout write failed: shell is gone, shutting down");
            break;
        }
    }

    info!("exiting with code {exit_code}");
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

    fn data_fetch(raw: &str, body: &str) -> netmod::DocFetch {
        netmod::DocFetch {
            url: Url::parse(raw).expect("data url"),
            status: 200,
            body: body.as_bytes().to_vec(),
        }
    }

    #[test]
    fn javascript_data_url_renders_as_inert_source() {
        // base64 of `console.log('<b>hi</b>'); document.title='pwned';` — the
        // `<b>hi</b>` inside the program text proves the viewer escapes: tags
        // must be shown as text, never handed back as live markup.
        let source = "console.log('<b>hi</b>'); document.title='pwned';";
        let raw = format!(
            "data:text/javascript;base64,{}",
            crate::netmod::encode_base64(source.as_bytes())
        );
        let html = render_document(&data_fetch(&raw, source));
        assert!(
            html.contains("&lt;b&gt;hi&lt;/b&gt;"),
            "source not escaped: {html}"
        );
        assert!(
            !html.contains("<b>hi</b>"),
            "raw markup leaked into the viewer: {html}"
        );
        // A text document titled by its media type, with the program visible.
        assert!(html.contains("<title>text/javascript</title>"), "{html}");
        assert!(html.contains("<pre"), "{html}");
        assert!(html.contains("base64"), "{html}");
        // Nothing the parser could execute.
        assert!(!html.contains("<script"), "{html}");
    }

    #[test]
    fn javascript_data_url_percent_encoded_is_source_too() {
        let html = render_document(&data_fetch(
            "data:application/javascript,document.title%3D1",
            "document.title=1",
        ));
        assert!(html.contains("document.title=1"), "{html}");
        assert!(html.contains("percent-encoded"), "{html}");
        assert!(!html.contains("<script"), "{html}");
    }

    #[test]
    fn markup_and_other_schemes_reach_the_parser_unchanged() {
        // `text/html` data URLs: parsed exactly as before.
        let html = "<!doctype html><title>ok</title><p>hi</p>";
        assert_eq!(
            render_document(&data_fetch("data:text/html,<p>hi</p>", html)),
            html
        );
        // http(s) and file: bodies never go through the source view, even when
        // the body happens to look like a script.
        let script = "document.title = 'from http'";
        let fetched = netmod::DocFetch {
            url: Url::parse("https://example.com/a.js").unwrap(),
            status: 200,
            body: script.as_bytes().to_vec(),
        };
        assert_eq!(render_document(&fetched), script);
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
            "ironsurf-tab".to_string(),
            "--tab-id".into(),
            "3".into(),
            "--container-id".into(),
            "2".into(),
            "--profile-dir".into(),
            std::env::temp_dir()
                .join("ironsurf-tab-args-test")
                .display()
                .to_string(),
        ])
        .expect("parse");
        assert_eq!(args.tab_id, TabId(3));
        assert_eq!(args.container_id, ContainerId(2));
        assert_eq!(args.url, ironsurf_common::default_url());
        assert_eq!(args.net_policy, NetPolicy::Passthrough);
        assert!(!args.no_sandbox);
        assert!(args.offline_fixtures.is_none());
    }

    #[test]
    fn tab_args_all_flags() {
        let args = TabArgs::parse(&[
            "ironsurf-tab".to_string(),
            "--tab-id".into(),
            "7".into(),
            "--container-id".into(),
            "1".into(),
            "--profile-dir".into(),
            std::env::temp_dir()
                .join("ironsurf-tab-args-test2")
                .display()
                .to_string(),
            "--url".into(),
            "https://example.org".into(),
            "--offline-fixtures".into(),
            "/tmp/fixtures".into(),
            "--managed-net".into(),
            "--no-sandbox".into(),
        ])
        .expect("parse");
        assert_eq!(args.url, "https://example.org");
        assert_eq!(args.net_policy, NetPolicy::Managed);
        assert!(args.no_sandbox);
        assert_eq!(args.offline_fixtures, Some(PathBuf::from("/tmp/fixtures")));
    }

    #[test]
    fn verbosity_flags_are_accepted_not_fatal() {
        // `--log`/`-v` reach the tab from the shell (`--log debug`) or a human;
        // they must parse cleanly (the level itself is read from argv by logging).
        let base = || {
            vec![
                "ironsurf-tab".to_string(),
                "--profile-dir".into(),
                std::env::temp_dir()
                    .join("ironsurf-tab-args-log")
                    .display()
                    .to_string(),
            ]
        };
        for flag in ["--log", "--log=trace", "-v", "-vv", "--verbose", "--debug"] {
            let mut args = base();
            args.push(flag.to_string());
            if flag == "--log" {
                args.push("debug".to_string());
            }
            let parsed = TabArgs::parse(&args);
            assert!(parsed.is_ok(), "{flag} must be accepted: {parsed:?}");
        }
        // A bad level is not an argument error either (logging falls back to the env/default).
        let mut args = base();
        args.extend(["--log".to_string(), "nope".to_string()]);
        assert!(TabArgs::parse(&args).is_ok());
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
