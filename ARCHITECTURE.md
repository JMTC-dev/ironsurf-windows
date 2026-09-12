# ServFox — Architecture & Crate Contracts

ServFox is a Rust web browser using the **Blitz** stack — `blitz-dom`/`blitz-html`
(DOM + Stylo CSS) with `blitz-paint` + `anyrender_vello_cpu` (pure-CPU 2D renderer,
zero GPU) and **Boa** (`boa_engine`, pure-Rust JS) as its engine and **Slint** for the
GUI, with **multi-account containers** (Firefox-MultiAccountContainers-style isolated
sessions) and **per-tab OS sandboxing**.

> **Engine pivot (2026-08):** Servo/mozjs was removed as an engine dependency — mozjs
> embeds C++ SpiderMonkey (needs clang/libclang, llvm-objdump, 45-min builds), which does
> not belong in a fully-Rust codebase. Servfox-tab now embeds Blitz (Stylo CSS + DOM,
> `html5ever` parsing) rendered through `vello_cpu`, with `boa_engine` for JavaScript.
> The `servfox-common` IPC contract (`ToTab`/`FromTab`) is **unchanged**; only the tab
> crate's engine internals changed.

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
│  - embeds Blitz + Stylo + Boa       │        │   different OS process ⇒      │
│  - renders via vello_cpu (software) │        │   memory can't be shared)     │
│  - per-container network layer:     │        │                               │
│    cookies/cache/HTTPS policy       │        │                               │
└──────────────┬──────────────────────┘        └───────────────────────────────┘
               │ profile dir (~/.local/share/servfox/profiles/<container>/) used as tab data path
     servfox-storage: container registry + per-container cookie jar / cache / local storage
```

**Why this shape**

- *Tabs cannot access each other's memory*: every tab is its own OS process with its own
  address space, additionally restricted by Landlock (filesystem visible only to its own
  container profile dir), `RLIMIT_*`, `PR_SET_NO_NEW_PRIVS`. Two tabs in *different
  containers* always run in distinct processes; the sandbox also makes reading another
  container's profile dir (cookies/cache/localStorage on disk) fail with `EACCES`.
- *Containers* = named, colored, isolated sessions. Each container has a profile directory
  on disk that holds all engine-adjacent state (cookies, cache, DOM storage, history), so
  it is scoped to the container. Cross-restart cookie persistence and HTTPS policy live in
  the tab's network layer (see §Network layer).
- *Blitz embedding*: the tab process owns one Blitz `Document` (built with
  `HtmlDocument::from_html` + `DocumentConfig`), drives it on its own single-threaded
  event loop (`resolve(scale)` after every input/net/JS tick), paints it with
  `blitz_paint::paint_scene` into `anyrender_vello_cpu::VelloCpuImageRenderer`, and ships
  RGBA frames to the shell over the unchanged IPC. No GPU, no winit, no wgpu — `vello_cpu`
  is a pure-Rust software rasterizer, which is exactly what a no-GPU fleet requires.

## Crate layout (cargo workspace at /workspace)

```
Cargo.toml            # [workspace] members = crates/*, shared deps, profile.release
crates/
  servfox-common/     # types + IPC codec. NO heavy deps (serde, bincode, thiserror only)
  servfox-storage/    # container registry, profiles, cookie jar, cache, local storage
  servfox-sandbox/    # Landlock + rlimits + prctl sandbox application
  servfox-tab/        # BIN: sandboxed per-tab Blitz+Boa host speaking servfox-common IPC
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
profiles/<container-name-slug>-<id>/  # tab data path; owned by container
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

## servfox-tab (binary) — the Blitz + Boa host

CLI: `servfox-tab --tab-id N --container-id N --profile-dir PATH [--url URL] [--offline-fixtures DIR] [--managed-net] [--no-sandbox]`

Engine stack (pinned; all pure Rust — **no C/C++ toolchain, no mozjs, no wgpu/GPU**):

```toml
blitz-dom = "0.3.0-beta.2"        # Document/BaseDocument, UiEvent, resolve(), query_selector
blitz-html = "0.3.0-beta.2"       # HtmlDocument::from_html(html, DocumentConfig)
blitz-net = "0.3.0-beta.2"        # default Provider (images/fonts); we wrap/replace ours
blitz-paint = "0.3.0-beta.2"      # paint_scene(scene, doc, scale, w, h, x, y)
blitz-traits = "0.3.0-beta.2"     # net::NetProvider/NetHandler, shell::{Viewport, ColorScheme}
anyrender = "0.13"                # PaintScene + render_to_buffer
anyrender_vello_cpu = "0.17"      # VelloCpuImageRenderer — CPU rasterizer, RGBA8 output
boa_engine = "0.22"               # JavaScript (pure Rust)
```

(`stylo 0.20` + `html5ever` + `parley` come in transitively. Reference: `DioxusLabs/blitz`
`examples/screenshot.rs` on GitHub main — the canonical headless parse→resolve→paint flow;
vendored sources live under `~/.cargo/registry/src/*/blitz-*-0.3.0-beta.2/`.)

Lifecycle:
1. Apply `servfox_sandbox::apply` with `writable_rw = [profile_dir]`,
   `readable_ro = [/usr/share/fonts, /usr/lib, /etc/ssl/certs, /etc/resolv.conf, <exe dir>]`.
   Report `FromTab::Ready` only after startup. (Sandbox failures degrade, never abort;
   `--no-sandbox` for debugging.)
2. Own the event loop — **no winit/waker plumbing anymore**: a single thread blocks in
   `poll()` over [non-blocking stdin, self-pipe for wakeups] with a timeout for animation /
   pending-net ticks. There is no external engine thread; Blitz is a library you drive.
3. One `Document` per navigation: fetch HTML (see Network layer), then
   `HtmlDocument::from_html(&html, DocumentConfig { base_url, net_provider: Arc<ours>,
   viewport: Some(Viewport::new(w*scale, h*scale, scale, ColorScheme::Light)), ..Default })`.
   After every input event / net completion / JS tick call `document.resolve(scale)`; if the
   doc reports changes (or a scripted/animation tick is pending), repaint and emit
   `FromTab::Frame` (throttle ≤ 30 fps). Emit `TitleChanged`/`UrlChanged` on change (compare
   against cached last values).
4. Frames: `render_to_buffer::<VelloCpuImageRenderer, _>(|scene| { white bg fill;
   paint_scene(scene, doc.as_mut(), scale, w, h, 0, 0); }, w, h)` → `FromTab::Frame`
   (RGBA8 straight alpha; verify buffer layout against the anyrender_vello_cpu source — if it
   ever emits premultiplied/BGR, convert; the spike test proves pixel sanity).
5. Resize: `ToTab::Resize` → store new `Viewport`, rebuild/resize document viewport, resolve,
   repaint.
6. Input: `ToTab::Pointer/Key/Scroll` → `blitz_dom::events` `UiEvent` variants (pointer
   press/release/move with `hit test` done internally, mouse wheel, keyboard key-press /
   text-input — see `blitz-dom/src/events/{pointer,keyboard}.rs`) → `document.handle_ui_event(ev)`,
   then resolve+repaint. Link clicks that request top-level navigation must round-trip: detect
   the navigation request via the anchor handling in `blitz-dom/src/events/driver.rs`
   (`EventDriver`); if Blitz only surfaces `DocumentEvent::NavigateIframe`, resolve the
   clicked node's anchor href from the hover/target node id ourselves.
7. On `Destroy`/EOF: persist cookie jar, flush, `_exit(0)` style shutdown is fine (no GPU
   teardown hazard anymore, but staying consistent with the shell is harmless).

### Network layer (container sessions, HTTPS)
Implement `blitz_traits::net::NetProvider` (`fn fetch(&self, doc_id, Request, Box<dyn NetHandler>)`)
in `servfox-tab/src/netmod.rs` and pass `Arc<OurProvider>` as `DocumentConfig.net_provider`
(keeps images/css/fonts flowing). Two policies, unchanged from before:
- `Passthrough` (default): plain `reqwest` (rustls) fetch on a small worker thread, respond
  via the `NetHandler` callbacks; cookies NOT injected for subresources.
- `Managed` (`--managed-net`): same worker, but inject `Cookie` header from the container's
  `CookieJar`, capture `Set-Cookie` into the jar, serve from `NetCache` with ETag/
  If-None-Match revalidation, and enforce https-upgrade policy (http: → try https, block
  page on failure; `about:`/`file://`/local fixtures exempt).
- Main-document navigations always go through our own reqwest fetcher in the tab process
  (not via NetProvider), so the managed policy always applies to top-level requests
  regardless of provider mode.
- Container isolation holds even in Passthrough: one process = one container; its on-disk
  state lives under the sandbox-visible profile dir only.

### JavaScript (Boa)
After parse & resolve, extract `<script>` elements from the DOM (`document.root_element()`
walk or `query_selector("script")`), fetch `src=` through our network layer, and execute in
order in **one** `boa_engine::Context` per tab (per navigation: rebuild context). Bindings
are a deliberately tiny surface (no engine-internal DOM mutation yet):
- `window` (eventTarget-ish no-ops), `document.title` get/set (set → `FromTab::TitleChanged`
  and repaint), `document.body.textContent` read-only snapshot,
  `console.log/info/warn/error` → tab stderr + `FromTab::StatusText`.
- Script exceptions must never kill the tab: catch, report via StatusText, continue.
- `javascript:` URLs are refused (status text), matching the shell contract.

### Headless tests (must run with no X display)
- `tests/spike.rs`: render a `file://` fixture (tempdir HTML with colored blocks) through
  the full parse→resolve→paint path; assert buffer size `w*h*4` and non-uniform pixels.
- `tests/e2e.rs`: launch the built binary with a `file://` page, read
  `FromTab::{Ready, UrlChanged, Frame}` (non-uniform), then `Destroy` → exit 0.
- Boa tests: inline `<script>document.title="js-set-title"</script>` → `TitleChanged` with
  the JS-set title arrives over IPC; broken script → tab stays alive, frame still arrives.

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
logic runs without the engine (Blitz) or X. GUI smoke test gated behind `$DISPLAY` + `SERVFOX_SMOKE=1`.

---

## Root-level deliverables

- `README.md`: features, screenshots note, build (`cargo build --release`), run
  (`target/release/servfox-shell`), sandbox & container explanation, limitations.
- `docs/ENGINE_INTEGRATION.md`: the Blitz/Boa embedding specifics discovered while
  implementing the tab crate (document lifecycle, paint/readback path, NetProvider quirks,
  Boa binding surface). Renamed from `SERVO_INTEGRATION.md` at the engine pivot.
- `scripts/demo.sh`: Xvfb/display screenshot of the running browser (if display available).
- `.gitignore` for `target/`.

## Verification gates per task

Each task MUST end with its own check passing:
1. common: `cargo test -p servfox-common`
2. storage: `cargo test -p servfox-storage` (registry lifecycle, cookie RFC rules incl.
   Secure-over-HTTPS-only, cache LRU eviction, **container isolation of profiles**)
3. sandbox: `cargo test -p servfox-sandbox` (probe child proves forbidden dir unreadable when Landlock is available; degrades gracefully otherwise)
4. tab/blitz: `cargo build -p servfox-tab && cargo test -p servfox-tab` (headless frame test;
   `cargo tree -p servfox-tab` must contain no `mozjs`/`wgpu`/`servo v` entries)
5. shell: `cargo test -p servfox-shell` (+ smoke with fake tab proc; GUI check if display)
6. end-to-end: shell driving two containers side-by-side (scripted), README + docs complete.
