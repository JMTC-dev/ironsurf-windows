# ServFox — Architecture & Crate Contracts

ServFox is a Rust web browser using **Servo** (the `servo` crate from crates.io, v0.1+)
as its engine and **Slint** for the GUI, with **multi-account containers**
(Firefox-MultiAccountContainers-style isolated sessions) and **per-tab OS sandboxing**.

This document is the single source of truth for crate APIs. Tasks implement against these
contracts; extend, don't contradict. Keep all crates compiling and `cargo test --workspace` green.

## Design overview

```
┌────────────────────────── servfox-shell (Slint GUI, main process) ─────────────────────────┐
│  tab bar (container-colored) · URL bar · container manager · frame compositor · input       │
│      │ spawn child process per tab (stdio pipes)                                            │
└──────┼───────────────────────────────────────────────────────────────────────────────────── ┘
       │ ToTab / FromTab (length-prefixed bincode over stdin/stdout)
┌──────▼──────────────────────────────┐        ┌──────────────────────────────┐
│ servfox-tab (child process #1, …N)  │   …    │ servfox-tab (child process N)│
│  - applies OS sandbox at startup    │        │  (different container ⇒       │
│  - embeds Servo (Software rendering)│        │   different OS process ⇒      │
│  - per-container network layer:     │        │   memory can't be shared)     │
│    cookies/cache/HTTPS policy       │        │                               │
└──────────────┬──────────────────────┘        └───────────────────────────────┘
               │ profile dir (~/.local/share/servfox/profiles/<container>/) used as Servo data path
     servfox-storage: container registry + per-container cookie jar / cache / local storage
```

**Why this shape**

- *Tabs cannot access each other's memory*: every tab is its own OS process with its own
  address space, additionally restricted by Landlock (filesystem visible only to its own
  container profile dir), `RLIMIT_*`, `PR_SET_NO_NEW_PRIVS`. Two tabs in *different
  containers* always run in distinct processes; the sandbox also makes reading another
  container's profile dir (cookies/cache/localStorage on disk) fail with `EACCES`.
- *Containers* = named, colored, isolated sessions. Each container has a profile directory
  on disk that is handed to Servo as its data path, so all engine state (cookies, cache,
  DOM storage, history) is scoped to the container. Cross-restart cookie persistence and
  HTTPS policy live in the tab's network layer (see §Network layer).
- *Servo embedding*: `servo` crate, `WebView` + `RenderingContext`. Tab processes render
  **offscreen/software** and ship RGBA frames to the shell — this keeps the GUI (Slint
  software renderer) decoupled from the engine and is the only sane multi-process split.

## Crate layout (cargo workspace at /workspace)

```
Cargo.toml            # [workspace] members = crates/*, shared deps, profile.release
crates/
  servfox-common/     # types + IPC codec. NO heavy deps (serde, bincode, thiserror only)
  servfox-storage/    # container registry, profiles, cookie jar, cache, local storage
  servfox-sandbox/    # Landlock + rlimits + prctl sandbox application
  servfox-tab/        # BIN: sandboxed per-tab Servo host speaking servfox-common IPC
  servfox-shell/      # BIN: Slint GUI, tab/container management, frame compositor
```

MSRV: latest stable. Edition 2021. `resolver = "2"`.

---

## servfox-common

Types must be `serde::{Serialize, Deserialize}` (+ `Clone, Debug, PartialEq` where sensible).

```rust
pub struct ContainerId(pub u32);
pub struct TabId(pub u32);

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Container {
    pub id: ContainerId,
    pub name: String,
    pub color: String,        // "#rrggbb" hex, used for the tab stripe
    pub is_default: bool,
}

/// One tab-to-shell message.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum FromTab {
    Ready            { tab_id: TabId },
    Frame            { width: u32, height: u32, scale: f32, rgba: Vec<u8> }, // RGBA8, straight alpha, top-down; rgba.len() == w*h*4
    TitleChanged     { title: Option<String> },
    UrlChanged       { url: Option<String> },
    LoadingChanged   { loading: bool },
    HistoryChanged   { can_go_back: bool, can_go_forward: bool },
    StatusText       { text: Option<String> },
    ConsoleLog       { level: String, message: String },
    Closed,
    Error            { message: String },
}

/// One shell-to-tab message.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ToTab {
    Navigate     { url: String },
    Reload,
    Stop,                                  // v1: no-op placeholder
    Back,
    Forward,
    Resize       { width: u32, height: u32, scale: f32 },
    SetVisible   { visible: bool },        // throttle background rendering
    Pointer      (PointerEvent),
    Key          (KeyEvent),
    Scroll       (ScrollEvent),
    Destroy,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct PointerEvent { pub x: f32, pub y: f32, pub kind: PointerKind, pub button: MouseButton }
pub enum PointerKind { Move, Down, Up, Over, Out }
pub enum MouseButton  { Left, Right, Middle }

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct KeyEvent {
    pub key: String,    // DOM `key`
    pub code: String,   // DOM `code`
    pub text: Option<String>,
    pub repeat: bool,
    pub mods: Modifiers, // bitflags-style struct or u8 newtype — implement helpers
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct ScrollEvent { pub x: f32, pub y: f32, pub delta_x: f32, pub delta_y: f32 }

/// Framing: u32 LE length prefix + bincode payload, over stdin/stdout pipes.
pub fn write_frame<W: std::io::Write, T: serde::Serialize>(w: &mut W, msg: &T) -> std::io::Result<()>;
pub fn read_frame<R: std::io::Read, T: serde::de::DeserializeOwned>(r: &mut R) -> std::io::Result<T>;
pub const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024; // guard against corrupt streams
```

Also export `fn default_url() -> String` (`https://example.com`), `KEY_*` constants are
unnecessary; shell maps Slint key events → `KeyEvent`.

---

## servfox-storage

Pure disk layer; no engine dependency. `thiserror` error type `StoreError`.

Layout under a data root (`$SERVFOX_DATA_DIR` or `~/.local/share/servfox`):

```
containers.json                       # registry (Vec<Container>), written atomically
profiles/<container-name-slug>-<id>/  # handed to Servo as data path; owned by container
  cookies.json                        # our persistent jar (see below)
  net-cache/                          # bounded response cache (our layer)
  local-storage/                      # reserved for engine/localStorage persistence
  history.jsonl
```

```rust
pub struct Store { root: PathBuf }
impl Store {
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, StoreError>; // creates dirs, loads/creates registry + default container ("personal")
    pub fn root(&self) -> &Path;
    pub fn containers(&self) -> &[Container];
    pub fn create_container(&mut self, name: &str, color: &str) -> Result<ContainerId, StoreError>;
    pub fn rename_container(&mut self, id: ContainerId, name: &str) -> Result<(), StoreError>;
    pub fn set_container_color(&mut self, id: ContainerId, color: &str) -> Result<(), StoreError>;
    pub fn delete_container(&mut self, id: ContainerId) -> Result<(), StoreError>; // refuses deleting default
    pub fn profile_dir(&self, id: ContainerId) -> PathBuf;              // ensures dir exists
    pub fn default_container_id(&self) -> ContainerId;
    pub fn container(&self, id: ContainerId) -> Option<&Container>;
}

pub struct ContainerStore { container: ContainerId, profile_dir: PathBuf }
impl ContainerStore {
    pub fn open(store: &Store, id: ContainerId) -> Self;
    pub fn cookie_jar(&self) -> CookieJar;         // loads cookies.json
    pub fn cache(&self) -> NetCache;
    pub fn append_history(&self, url: &str, title: Option<&str>) -> Result<(), StoreError>;
}

pub struct Cookie { pub name: String, pub value: String, pub domain: String,
                    pub path: String, pub secure: bool, pub http_only: bool,
                    pub expires_unix: Option<i64> }
impl CookieJar {
    pub fn set_from_set_cookie(&mut self, request_url: &url::Url, header_value: &str, now_unix: i64);
    pub fn cookie_header(&self, request_url: &url::Url, now_unix: i64) -> Option<String>; // "a=1; b=2"
    pub fn save(&self) -> Result<(), StoreError>;
    pub fn clear(&mut self);
    pub fn cookies(&self) -> &[Cookie];
}
// Cookie rules (must be unit-tested):
//  - Secure cookies are ONLY sent over https: URLs and ONLY set over https:
//  - domain suffix match (request host == cookie domain or ends with ".{cookie domain}")
//  - path prefix match per RFC6265
//  - expired cookies are dropped on load/access
//  - host-only cookies (no leading dot) match exact host

pub struct NetCache; // bounded by total bytes (~256 MB) & entries (~1000), LRU eviction
impl NetCache {
    pub fn lookup(&self, url: &str) -> Option<CachedEntry>;   // CachedEntry { status, headers: Vec<(String,String)>, body: Vec<u8>, etag, last_modified, cached_at }
    pub fn store(&self, url: &str, status: u16, headers: &[(String, String)], body: Vec<u8>, now_unix: i64);
    pub fn clear(&self);
}
```

---

## servfox-sandbox

Linux-only core, compiles everywhere with a no-op fallback (`available() == false`).

```rust
pub struct SandboxPolicy {
    pub readable_ro: Vec<PathBuf>, // e.g. /usr/share/fonts, /etc/ssl/certs, /nix?, binary dir
    pub writable_rw: Vec<PathBuf>, // exactly ONE: the container profile dir (+ /tmp? NO)
    pub exec_ro:     Vec<PathBuf>, // the servfox-tab executable path
    pub max_open_files: u64,       // default 1024
    pub max_processes:  u64,       // default 128
    pub address_space_mb: Option<u64>,
}
pub struct SandboxStatus { pub landlock_active: bool, pub rlimits_applied: bool, pub notes: Vec<String> }
pub fn apply(policy: &SandboxPolicy) -> Result<SandboxStatus, SandboxError>;
// Steps: 1) prctl(PR_SET_NO_NEW_PRIVS)  2) rlimits (RLIMIT_NOFILE, NPROC, optional AS)
//        3) Landlock ABI v1+: allow FS read-only on readable_ro+exec_ro, read-write-create on
//           writable_rw (use landlock crate: Ruleset → BestEffort so old kernels degrade),
//           then ruleset restricts everything else.
pub fn is_available() -> bool; // Landlock present on this kernel
```

Must never panic if Landlock is unavailable (older kernels): degrade to rlimits+no_new_privs,
record it in `SandboxStatus.notes`.

Tests: spawn `self` re-exec (a hidden `--sbox-probe <writable> <forbidden>` arg on the tab
binary or a test-only bin here) that applies the sandbox and exits 42 iff opening
`<forbidden>` (another container's profile) **fails** and opening `<writable>` succeeds.

---

## servfox-tab (binary) — the Servo host

CLI: `servfox-tab --tab-id N --container-id N --profile-dir PATH [--url URL] [--offline-fixtures DIR]`

Lifecycle:
1. Apply `servfox_sandbox::apply` with `writable_rw = [profile_dir]`,
   `readable_ro = [/usr/share/fonts, /etc/ssl/certs, <exe dir>]`. Report `FromTab::Ready`
   only after startup. (Sandbox failures degrade, never abort; `--no-sandbox` for debugging.)
2. Install rustls crypto provider (`aws_lc_rs` — required by Servo, see winit_minimal example).
3. Create **one** `Servo` per process with a **software/offscreen** `RenderingContext`
   (`servo::SoftwareRenderingContext` or `OffscreenRenderingContext` — verify exact ctor in
   `servo` source `paint_api/rendering_context.rs`), with engine data path =
   `--profile-dir` (investigate `servo::Opts` / `servo_config::opts` builder + `Preferences`;
   servoshell uses something like `--user-data-path`; if unavailable keep per-process default
   dirs — process separation already isolates).
4. One `WebView` (created via `WebViewBuilder`) whose `WebViewDelegate` forwards to IPC:
   `notify_new_frame_ready` → capture frame (use `WebView::capture_screenshot`-style readback
   — exact API name TBD from `components/servo/webview.rs`; fallback: read the software
   context buffer) → send `FromTab::Frame`; `notify_page_title_changed`, `notify_url_changed`,
   `notify_load_status_changed`, `notify_history_changed`, `notify_status_text_changed`,
   `show_console_message`, `notify_closed`, `notify_crashed` → matching `FromTab::*`.
5. Event loop: single-threaded. Sources: stdin pipe (deserialize `ToTab`), Servo's
   `EventLoopWaker` (implemented to write 1 byte into a self-pipe; select()/poll over
   [stdin, selfpipe]). Every wake → `Servo::spin_event_loop()`. Animate throttle: when
   `webview.animating()`, poll-spin at ≤ 30 fps. Resize via `ToTab::Resize` →
   `rendering_context.resize()`/`webview.resize()`.
6. Input: `ToTab::Pointer/Key/Scroll` → map to `servo::InputEvent` variants
   (`MouseEvent`, `KeyboardEvent` from `servo::keyboard_types`, `WheelEvent`) →
   `webview.notify_input_event(..)`.
7. On `Destroy`/EOF: persist cookie jar, flush, exit 0.

### Network layer (container sessions, HTTPS)
Chokepoint: `WebViewDelegate::load_web_resource(load: WebResourceLoad)`.
- Policy `NetPolicy::Passthrough` (default): just return (Servo's own rustls HTTPS applies).
- Policy `NetPolicy::Managed` (enabled via `--managed-net`): intercept every http(s) load and
  fetch via our own `reqwest` (rustls) client so that we control **Cookie header injection
  from the container's `CookieJar`**, capture `Set-Cookie` responses into the jar, answer from
  `NetCache` (with ETag/If-None-Match revalidation), and **enforce https-only upgrades**
  (http: navigations → upgrade attempt to https, block with error page on failure) with an
  "allow http" escape only for `about:`/local fixtures. Wire this in a submodule
  `servfox-tab/src/netmod.rs`; verify `WebResourceResponse` supports passing status/headers
  through (`status_code`, headers builder — check servo source `components/shared/embedder`
  /`embedder_traits`).
- Even in Passthrough mode: cookies are still fully container-isolated because the engine
  process only ever holds one container's state and its disk state lives under that
  container's sandbox-visible profile dir.

### Headless tests (must run with no X display)
- `tests/e2e.rs`: launch the built tab binary with `--offline-fixtures` + a local file://
  page (tempdir, `file:///…/index.html`), read `FromTab::{Ready, UrlChanged, Frame}`, assert
  a frame of requested size arrives containing non-uniform pixels, then `Destroy`.
- If a display exists (`$DISPLAY`), also test navigating to a `python3 -m http.server` URL.

Servo dependency notes:
- `servo = { version = "0.1", default-features = false, features = ["bundled", "clipboard", "js_jit"] }`
  (verify published feature names via `cargo add servo --dry-run` / the crates.io manifest;
  `multiprocess` stays OFF for v1). If a feature listed here isn't published, adapt.
- Heavy build (~30+ min first time). Pin the resolved version in Cargo.lock and don't bump.
- Key source references (fetch raw from GitHub main if the crate API drifted):
  `components/servo/lib.rs`, `webview.rs`, `webview_delegate.rs`,
  `examples/winit_minimal.rs`, `ports/servoshell/`.

---

## servfox-shell (binary) — Slint GUI

Slint `1.x` (default features OK; force the **software renderer** so it composites on
headless X11 without GL: build with `slint` default + set software-softbuffer path;
runtime env `SLINTSoftwareRenderer`… verify flag in Slint docs, `SLINT_SCALE_FACTOR` respected).

UI (`crates/servfox-shell/ui/servfox.slint`):
- `AppWindow` with: toolbar row [back, forward, reload, URL `LineEdit`, "New tab",
  container dropdown], tab bar (horizontal scroll; each tab = container-colored stripe +
  title + ✕), page area (`Image` fed by active tab frame, `TouchArea`/`PointerAreaHandler`
  forwarding pointer+wheel events; keyboard `key-pressed`/`key-released` → `KeyEvent`).
- Container manager popup: list containers (color swatch), add/rename/choose color (fixed
  palette), "delete container (+ data)", per-tab "Move tab to container ▸" submenu (move =
  respawn tab process keeping URL), "New tab in container".
- Status bar: status text + loading spinner (timer animation) + https/lock indicator
  (⛬ / http warning).
- Menu (native or in-toolbar): New Window(v1: new tab), Close Tab (Ctrl-W), Tab Next/Prev
  (Ctrl-Tab), Open Container Manager, Clear Container Data.

Process/tab management (`src/tabs.rs`):
- `struct TabProc { id, container, child: Child, stdin, stdout, title, url, loading, can_back, can_fwd, frame: slint::Image, last_frame_age }`
- Spawn `servfox-tab` (resolve sibling of current exe; `--tab-id`…) with piped stdio;
  reader thread per tab → `mpsc::UnboundedSender<FromTab>` drained by Slint timer (UI updates
  via `slint::Weak<AppWindow>` invoke; throttle Frame messages: coalesce to newest).
- `Frame` → `slint::Image` from raw RGBA buffer (`slint::Image::from_rgba8_slab`/PixelBuffer API — verify name).
- Keyboard focus routing: Slint focus on page area → forward keys; Ctrl-L focuses URL bar;
  Esc unfocuses.
- Default start page: `about:blank` then URL bar typing does search-or-navigate: contains dot
  → `https://host` (upgrading typed `host/path` to HTTPS), else → `https://duckduckgo.com/?q=`.
- Container default from registry; per-container "remember last chosen container for new tabs"
  in `~/.local/share/servfox/ui.json` (best-effort).

Tests (headless CI-safe): tab-proc message pump logic + `FromTab` state reduction tested
against a **fake tab process** (a small test bin that scripts `FromTab` sequences) so shell
logic runs without Servo or X. GUI smoke test gated behind `$DISPLAY` + `SERVFOX_SMOKE=1`.

---

## Root-level deliverables

- `README.md`: features, screenshots note, build (`cargo build --release`), run
  (`target/release/servfox-shell`), sandbox & container explanation, limitations.
- `docs/SERVO_INTEGRATION.md`: the embedding specifics discovered while implementing the tab
  crate (exact rendering context used, readback path, prefs that mattered).
- `scripts/demo.sh`: Xvfb/display screenshot of the running browser (if display available).
- `.gitignore` for `target/`.

## Verification gates per task

Each task MUST end with its own check passing:
1. common: `cargo test -p servfox-common`
2. storage: `cargo test -p servfox-storage` (registry lifecycle, cookie RFC rules incl.
   Secure-over-HTTPS-only, cache LRU eviction, **container isolation of profiles**)
3. sandbox: `cargo test -p servfox-sandbox` (probe child proves forbidden dir unreadable when Landlock is available; degrades gracefully otherwise)
4. tab/servo: `cargo build -p servfox-tab && cargo test -p servfox-tab` (headless frame test)
5. shell: `cargo test -p servfox-shell` (+ smoke with fake tab proc; GUI check if display)
6. end-to-end: shell driving two containers side-by-side (scripted), README + docs complete.
