# IronSurf — Architecture & Crate Contracts

IronSurf is a Rust web browser using the **Blitz** stack — `blitz-dom`/`blitz-html`
(DOM + Stylo CSS) with `blitz-paint` + `anyrender_vello_cpu` (pure-CPU 2D renderer,
zero GPU) and **Boa** (`boa_engine`, pure-Rust JS) as its engine and **Slint** for the
GUI, with **multi-account containers** (Firefox-MultiAccountContainers-style isolated
sessions) and **per-tab OS sandboxing**.

> **Engine pivot (2026-08):** Servo/mozjs was removed as an engine dependency — mozjs
> embeds C++ SpiderMonkey (needs clang/libclang, llvm-objdump, 45-min builds), which does
> not belong in a fully-Rust codebase. IronSurf-tab now embeds Blitz (Stylo CSS + DOM,
> `html5ever` parsing) rendered through `vello_cpu`, with `boa_engine` for JavaScript.
> The `ironsurf-common` IPC contract (`ToTab`/`FromTab`) is **unchanged**; only the tab
> crate's engine internals changed.

This document is the single source of truth for crate APIs. Tasks implement against these
contracts; extend, don't contradict. Keep all crates compiling and `cargo test --workspace` green.

## Design overview

```
┌────────────────────────── ironsurf-shell (Slint GUI, main process) ─────────────────────────┐
│  tab bar (container-colored) · URL bar · container manager · frame compositor · input       │
│      │ spawn child process per tab (stdio pipes)                                            │
└──────┼───────────────────────────────────────────────────────────────────────────────────── ┘
       │ ToTab / FromTab (length-prefixed bincode over stdin/stdout)
┌──────▼──────────────────────────────┐        ┌──────────────────────────────┐
│ ironsurf-tab (child process #1, …N)  │   …    │ ironsurf-tab (child process N)│
│  - applies OS sandbox at startup    │        │  (different container ⇒       │
│  - embeds Blitz + Stylo + Boa       │        │   different OS process ⇒      │
│  - renders via vello_cpu (software) │        │   memory can't be shared)     │
│  - per-container network layer:     │        │                               │
│    cookies/cache/HTTPS policy       │        │                               │
└──────────────┬──────────────────────┘        └───────────────────────────────┘
               │ profile dir (~/.local/share/ironsurf/profiles/<container>/) used as tab data path
     ironsurf-storage: container registry + per-container cookie jar / cache / local storage
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
  ironsurf-common/     # types + IPC codec. NO heavy deps (serde, bincode, thiserror only)
  ironsurf-storage/    # container registry, profiles, cookie jar, cache, local storage
  ironsurf-sandbox/    # Landlock + rlimits + prctl sandbox application
  ironsurf-tab/        # BIN: sandboxed per-tab Blitz+Boa host speaking ironsurf-common IPC
  ironsurf-shell/      # BIN: Slint GUI, tab/container management, frame compositor
```

MSRV: latest stable. Edition 2021. `resolver = "2"`.

---

## ironsurf-common

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
    #[serde(default)]
    pub user_agent: Option<String>,  // per-container UA override; None = engine default
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
    OpenLinkInNewTab { url: String },      // middle-click a link → shell opens url in a new tab (same container)
    // source: "js" | "css" | "dom" | "page" | "userscript" — which DevTools panel the entry
    // belongs to. A "userscript" entry always names its author first: message is
    // `"{script name}: {text}"` (console line) or `"{script name}: {Error: …}"` (throw).
    ConsoleLog       { source: String, level: String, message: String },
    // one finished (or failed) page request → DevTools network panel
    NetworkEvent     { event: NetEvent },
    // window.alert/confirm/prompt: the tab emits this and **blocks its JS
    // thread** (pumping stdin, deferring every other ToTab message) until
    // the shell answers with the matching `ToTab::DialogResponse`.
    DialogRequest    { dialog_id: u32, kind: DialogKind, message: String, default: Option<String> },
    // the pointer shape over the page viewport: CSS `cursor:` on the hovered
    // element (blitz-resolved) first, then the engine's clickability
    // heuristics (link/button/clickable control → Pointer; a video's seek bar
    // → EwResize); `Default` is the plain arrow. Emitted **only on change**,
    // like StatusText; `Pointer` is the pointing hand over an `<a>`.
    CursorChanged    { cursor: CursorIcon },
    // archive mode (the shell's 💾 toggle): one finished http(s) fetch while
    // the mode was on — the tab ships the body verbatim, the **shell** writes
    // it under the configured root. `url` = final (post-redirect) URL;
    // `is_document` = a main-document navigation (saved as the host's
    // `index.html` when its URL carries **no query** — with a query the full
    // URL names the file, see the shell's archive mapping), every other entry
    // a subresource/media file (saved under its URL path + query).
    // Failures are never archived; local schemes never ship.
    ArchivedFetch    { url: String, is_document: bool, bytes: Vec<u8> },
    // find in page: the tab counted `matches` hits for `query`; `active` is the
    // **1-based** index of the hit painted light blue (0 = none, which only
    // happens when `matches == 0`). Sent after every find command that changed
    // something and after a load re-ran a live query; `query` names the text the
    // numbers describe, so a stale reply the bar has typed past is recognisable.
    FindResult       { query: String, matches: usize, active: usize },
    Closed,
    Error            { message: String },
}

/// Which modal the shell shows: `Alert` (OK), `Confirm` (OK/Cancel → bool),
/// `Prompt` (LineEdit + OK/Cancel → string|null). `message` is capped at
/// `MAX_DIALOG_MESSAGE` (64 KiB) chars; `default` is only meaningful for
/// `Prompt`. A cancelled answer (Cancel/Esc, shell quit, broken pipe) reads
/// `confirm` → `false` and `prompt` → `null`; `alert` always returns undefined.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DialogKind { Alert, Confirm, Prompt }

/// One shell-to-tab message.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ToTab {
    Navigate     { url: String },
    Reload,
    Stop,                                  // v1: no-op placeholder
    Back,
    Forward,
    Resize       { width: u32, height: u32, scale: f32 },
    SetZoom      { zoom: f32 },           // page zoom, 1.0 = 100% (Ctrl-+/Ctrl--/Ctrl-0)
    SetVisible   { visible: bool },        // throttle background rendering
    Pointer      (PointerEvent),
    Key          (KeyEvent),
    Scroll       (ScrollEvent),
    // answer to `FromTab::DialogRequest`: `confirmed` = OK (vs Cancel/Esc);
    // `text` is `Some` exactly when a Prompt was accepted. Responses for
    // unknown dialog ids are ignored (the request may have been cleared).
    DialogResponse { dialog_id: u32, confirmed: bool, text: Option<String> },
    // userscripts: the shell sends **all enabled** scripts (patterns included);
    // the tab filters them per document URL on every navigation (scripts still
    // apply after JS-driven redirects). Disabled scripts are never sent; an
    // empty vec clears the tab-side set.
    SetUserscripts { scripts: Vec<Userscript> },
    // per-container User-Agent override: Some(v) = send exactly v as the HTTP
    // User-Agent on every subsequent request; None = the tab's built-in
    // default. The shell re-sends it on every tab spawn and on every switch.
    SetUserAgent { user_agent: Option<String> },
    // archive mode master switch (the 💾 toolbar menu; **global**, not
    // per-container): while enabled the tab emits `FromTab::ArchivedFetch`
    // for every successful http(s) response (documents, subresources,
    // media) from that moment on. The shell owns the destination (root +
    // per-tab timestamp dir) and does all writing — tabs stay sandboxed.
    // Fresh tabs inherit the setting via the `--archive-mode` spawn flag.
    SetArchiveMode { enabled: bool },
    // find in page (`Ctrl-F`): install/replace the live query and re-run it from
    // the top. The shell owns the bar; the **tab** matches over the text it laid
    // out (case-insensitive, document order, visible text only) and paints it —
    // **every** hit yellow, the **active** hit light blue. First hit becomes
    // active and scrolls into view; the empty query clears query, highlights and
    // count. Answers `FromTab::FindResult`. The query survives navigations.
    SetFind          { query: String },
    FindNext,                             // next hit (wrapping), scrolls into view
    FindPrevious,                          // previous hit (wrapping)
    Destroy,
}

/// A user-defined JS program run on matching pages (Tampermonkey-style).
/// Empty pattern list matches nothing; patterns see `userscript_matches`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Userscript {
    pub id: u64,
    pub name: String,
    pub patterns: Vec<String>,
    pub code: String,
    pub enabled: bool,           // disabled scripts are never sent to the tab
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

/// One page request, reported once when it finishes (or fails).
/// Completed ⇒ `status` set, `error` None; failed ⇒ `status` None, `error` Some.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NetEvent {
    pub method: String,
    pub url: String,
    pub status: Option<u16>,
    pub response_bytes: Option<u64>,
    pub duration_ms: Option<u64>,
    pub error: Option<String>,
}

/// One userscript URL pattern vs one document URL (both lowercased — paths
/// compare case-insensitively too). Blank patterns match nothing; bare `"*"`
/// matches every URL; non-`*` patterns match the full URL exactly, with
/// `example.com/docs` sugar = `*://example.com/docs*` (exact host, path
/// prefix); `*`-leading patterns apply their domain (leading `*.`/`.`
/// stripped) as a host **boundary** — host == domain or ends with
/// `.{domain}`, never a lookalike like `notexample.com` — plus verbatim
/// `ends_with` on the URL for an optional path part. Non-http(s) schemes
/// (data:, ironsurf:, about:) never match wildcard domain patterns; exact
/// patterns can still match them literally.
pub fn userscript_matches(pattern: &str, url: &str) -> bool;
/// Enabled scripts with at least one matching pattern, in input order.
pub fn userscripts_for_url<'a>(scripts: &'a [Userscript], url: &str) -> Vec<&'a Userscript>;

/// Built-in User-Agent presets `(label, UA)` in switcher menu order; a user
/// may still type any other string (these are conveniences, not a whitelist).
pub const BUILTIN_USER_AGENTS: &[(&str, &str)];
/// Label of `ua` iff it is one of `BUILTIN_USER_AGENTS` (exact match);
/// `None` = a custom UA string.
pub fn builtin_user_agent_label(ua: &str) -> Option<&'static str>;
pub fn is_builtin_user_agent(ua: &str) -> bool;
/// Cap on a user-edited User-Agent string, in chars.
pub const MAX_USER_AGENT_CHARS: usize = 512;

/// Framing: u32 LE length prefix + bincode payload, over stdin/stdout pipes.
pub fn write_frame<W: std::io::Write, T: serde::Serialize>(w: &mut W, msg: &T) -> std::io::Result<()>;
pub fn read_frame<R: std::io::Read, T: serde::de::DeserializeOwned>(r: &mut R) -> std::io::Result<T>;
pub const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024; // guard against corrupt streams
```

`ScrollEvent` deltas speak the winit/AccessKit convention both ends of the pipe
already use: **positive `delta_y` scrolls UP** (`scrollTop` shrinks), a negative
one scrolls DOWN; likewise `delta_x` and LEFT. The shell forwards the toolkit's
deltas untouched and the tab hands them to blitz untouched — neither side negates
(a negation on either side inverts the wheel).

Also export `const WELCOME_URL: &str` (`"ironsurf://welcome"`) and
`fn default_url() -> String` (= `WELCOME_URL` — the built-in start page, answered locally by
every tab; see §ironsurf-tab internal scheme), `KEY_*` constants are
unnecessary; shell maps Slint key events → `KeyEvent`.

---

## ironsurf-storage

Pure disk layer; no engine dependency. `thiserror` error type `StoreError`.

Layout under a data root (`$IRONSURF_DATA_DIR` or `~/.local/share/ironsurf`):

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
//      — a Domain= cookie is a wildcard for the domain and every subdomain;
//  - host-only cookies (no leading dot) match the exact host only; a cookie
//    set by www.example.com is NEVER sent to example.com or a subdomain, and
//    .example.com never matches a lookalike like notexample.com
//  - a Domain= attribute must be a suffix of the request host; against an IP
//    request host or with a single-label value it rejects the cookie
//  - path prefix match per RFC6265 §5.1.4 verbatim (/foo matches /foo and
//    /foo/bar, never /foobar; /foo/ never matches bare /foo)
//  - HttpOnly is captured/persisted (scripts never see cookies at all) and
//    is still sent normally over HTTP
//  - expired cookies are dropped on load/access; an already-expired
//    Set-Cookie (or Max-Age<=0, or a malformed Max-Age) deletes the cookie
//    it names instead of storing it

pub struct NetCache; // bounded by total bytes (~256 MB) & entries (~1000), LRU eviction
impl NetCache {
    pub fn lookup(&self, url: &str) -> Option<CachedEntry>;   // CachedEntry { status, headers: Vec<(String,String)>, body: Vec<u8>, etag, last_modified, cached_at }
    pub fn store(&self, url: &str, status: u16, headers: &[(String, String)], body: Vec<u8>, now_unix: i64);
    pub fn clear(&self);
}
```

---

## ironsurf-sandbox

Linux-only core, compiles everywhere with a no-op fallback (`available() == false`).

```rust
pub struct SandboxPolicy {
    pub readable_ro: Vec<PathBuf>, // e.g. /usr/share/fonts, /etc/ssl/certs, /nix?, binary dir
    pub writable_rw: Vec<PathBuf>, // exactly ONE: the container profile dir (+ /tmp? NO)
    pub exec_ro:     Vec<PathBuf>, // the ironsurf-tab executable path
    pub max_open_files: u64,       // default 1024
    pub max_processes:  u64,       // default 128 — per-UID headroom on Linux (RLIMIT_NPROC is uid-wide;
                                   // applied as current-task-count + N, never below the uid's live tasks)
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

## ironsurf-tab (binary) — the Blitz + Boa host

CLI: `ironsurf-tab --tab-id N --container-id N --profile-dir PATH [--url URL] [--offline-fixtures DIR] [--managed-net] [--no-sandbox]`

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
1. Apply `ironsurf_sandbox::apply` with `writable_rw = [profile_dir]`,
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
   repaint. Zoom: `ToTab::SetZoom { zoom }` → store the factor, `document.set_viewport` with
   `Viewport::zoom = zoom` (blitz multiplies hidpi × zoom into its total scale), resolve,
   repaint; the tab divides incoming pointer coordinates and pixel scroll deltas by the zoom
   (logical px → CSS px), keeps the factor across navigations (fresh documents inherit it),
   and clamps nonsense values. The shell owns the chord policy — Ctrl-`+`/`=`, Ctrl-`-`,
   Ctrl-`0` step the per-tab factor (×÷1.1, clamped 30–500%, whole-percent levels) and show
   “Zoom N%” in the status bar.
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
in `ironsurf-tab/src/netmod.rs` and pass `Arc<OurProvider>` as `DocumentConfig.net_provider`
(keeps images/css/fonts flowing). Two policies, unchanged from before:
- `Passthrough` (default): plain `reqwest` (rustls) fetch on a small worker thread, respond
  via the `NetHandler` callbacks; cookies NOT injected for subresources.
- `Managed` (`--managed-net`): same worker, but inject `Cookie` header from the container's
  `CookieJar`, capture `Set-Cookie` into the jar, serve from `NetCache` with ETag/
  If-None-Match revalidation, and enforce https-upgrade policy (http: → try https, block
  page on failure; `about:`/`file://`/local fixtures exempt).
- Main-document navigations always go through our own reqwest fetcher in the tab process
  (not via NetProvider), so the managed policy always applies to top-level requests
  regardless of provider mode. Concretely: **the container cookie jar is always
  attached to navigations under both policies** — cookies come back from the
  server and ride back on every later navigation and every redirect hop.
  Redirects are followed hop-by-hop (a manual-redirect client): each hop gets
  its own jar-computed `Cookie` header for its own origin, and each hop's
  `Set-Cookie`s bind to that hop; POST→GET conversion follows the browser
  rules (303 always, 301/302 convert; 307/308 keep method+body).
  `Passthrough` therefore skips only the https-upgrade and the response cache
  on navigations. One jar instance (`Arc<Mutex<CookieJar>>`) is shared by the
  tab's main-document fetches and the provider's subresource fetches — cookies
  set by either are instantly visible to the other, and shutdown persistence
  flushes the one jar (never two copies overwriting each other).
- `data:` URLs (RFC 2397) are decoded by us, never fetched: `parse_data_url` returns the media
  type, optional `charset`, whether the payload was `;base64`, and the bytes; both the percent-
  encoded and base64 forms work for top-level documents and for subresources (`<script src=…>`,
  `<img>`, css). The media type decides what the tab *does* with the bytes: markup is parsed,
  non-markup (`data:text/javascript;base64,…`, css, JSON, `text/*`) is shown as inert escaped
  source titled by its media type — running a URL-bar program would be a `javascript:` URL by
  another name. A page may still load and run `data:text/javascript;base64,…` via `<script src>`;
  that is the page's own decision. See `docs/ENGINE_INTEGRATION.md` §G.
- `ironsurf:` is the browser's **internal scheme** (like `chrome:`): every `ironsurf://…` URL is
  answered locally by the tab and never touches the network, under either policy. The page is
  chosen by `internal_page_name` (host, with a `ironsurf` host or empty host falling through to
  the path, so `ironsurf://welcome` = `ironsurf:///welcome` = `ironsurf://ironsurf/welcome`);
  `welcome` renders the start page (`WELCOME_URL` = `default_url()`), any other name renders an
  honest internal "no such page" that never left the process. All output escapes the requested
  URL — page names are user text, never markup.
- Container isolation holds even in Passthrough: one process = one container; its on-disk
  state lives under the sandbox-visible profile dir only.
- **DevTools network reporting**: one `FromTab::NetworkEvent` per settled or failed page
  request — main documents (every followed redirect hop, via the reporting fetch variants)
  and provider subresources (`<img>`, `<script src>`, CSS) alike. A completed request carries
  method/status/bytes/duration; a failed one carries `error: Some(…)`, `status: None` — never
  both. `data:` and unsupported schemes report nothing (no-spam); revalidated cache hits
  report the cached entry's status/size. The shell appends these into the per-tab network log.

### DevTools CSS seam (`src/logging.rs`)

Blitz parses CSS through Stylo with `None` as Stylo's `ParseErrorReporter`, so upstream every
stylesheet parse error is silently dropped. The tab therefore **re-parses** the document's CSS
with Stylo itself behind a real reporter: every `<style>` element body and every inline
`style="…"` attribute (as a synthetic rule) become `ConsoleLog { source: "css", level:
"error" }` with line/column, plus a `source: "css"` warning for `.css` responses served with
the wrong `Content-Type` (raised by the `BlitzNetProvider`). Honest gaps: `<link
rel=stylesheet>` *content* is fetched by blitz and never exists as DOM text, so its parse
errors are invisible to the seam (its load failures and wrong content types are reported),
and `@import` bodies are not followed.

### JavaScript (Boa)
After parse & resolve, extract `<script>` elements from the DOM (`document.root_element()`
walk or `query_selector("script")`), fetch `src=` through our network layer, and execute in
order in **one** `boa_engine::Context` per tab (per navigation: rebuild context). Bindings
are a small, DOM-shaped surface:
- `window` (eventTarget-ish no-ops), `document.title` get/set (set → `FromTab::TitleChanged`
  and repaint), `document.body.textContent` read-only snapshot,
  `console.log/info/warn/error` → tab stderr + `FromTab::StatusText` **and** a
  `FromTab::ConsoleLog { source: "js", level, message }` per line (all four levels, page order).
- **`window.alert`/`confirm`/`prompt` are implemented as real synchronous modals**: the native
  emits `FromTab::DialogRequest` and **blocks the calling script in place** — pumping stdin
  (poll + read + frame-decode) without repainting — until the shell's `ToTab::DialogResponse`
  with the request's id arrives. Every *other* `ToTab` message received mid-dialog is deferred
  (FIFO) and replayed through the normal stdin pipeline right after the dialog resolves. The
  shell queues requests FIFO (several tabs may each be blocked on their own dialog), shows the
  front one in a centred modal overlay (scrim swallows page clicks; prompt prefills `default`),
  activating the requesting tab, and answers OK/Cancel/prompt-Enter over the wire. Every
  degenerate path answers **cancelled** rather than hanging a tab: Cancel/Esc, a tab that
  crashes or closes mid-dialog (the shell drops its overlay), a broken stdout, and stdin EOF
  (shell quit). With no dialog host at all (headless harness, bare tests) the calls answer
  cancelled immediately. Returns: `alert` → `undefined`, `confirm` → `true`/`false`,
  `prompt` → the text or `null` (never the default — the default only prefills the box).
- Script exceptions must never kill the tab: catch, report via StatusText, continue. Every
  uncaught exception is also a `ConsoleLog { source: "js", level: "error" }` — or `source:
  "dom"` when a native `__ironsurfDom*` binding threw (the registration macro records native
  `Err`s; reported **exactly once**, never duplicated on the generic script-error path).
- **The DOM bridge writes through to the live Blitz tree, and every JS change is reflected.**
  Reads and mutations go through the native `__ironsurfDom*` bindings, and the tab re-runs its
  settle (restyle + relayout + coalesced repaint) after every JS turn that can change the
  document: the initial script batch, **after every userscript run**, every timer batch and
  every `javascript:` program. The reflection rules that used to be silent gaps:
  * `document.write(...)`/`writeln(...)` really parse — scripts run after the parse, so the
    insertion cursor is the end of the tree: the written markup is parsed as a fragment and
    appended under `<body>` (never replacing the document that is running the script).
  * `element.style` is a CSSStyleDeclaration view over the `style` content attribute: member
    writes (`el.style.color = "red"`), `cssText`, `setProperty`/`getPropertyValue`/
    `removeProperty` and the non-standard whole-string assignment
    (`document.body.style = "…"`) ALL rewrite the attribute through the same native path as
    `setAttribute`, so the change restyles on the next resolve (verified against stylo's
    computed values).
  * `document.dir`/`document.lang` mirror the `<html>` element's attributes.
  * JS attribute writes match html5ever's storage — HTML attributes live in the **empty**
    namespace and blitz's `Attributes::set` compares the full `QualName` — so `setAttribute`
    on an attribute the parser already put there *replaces it in place* instead of pushing a
    duplicate that reads then shadow (the bug `document.body.style = "…"` exposed).
- **Userscripts (`ToTab::SetUserscripts`) run in the page's own realm.** The tab keeps the
  enabled set the shell installed (replace-wholesale; empty clears) and, **once per document**
  (including documents reached by a JS-driven redirect, since the filter uses the current
  document URL at run time), runs every script `userscripts_for_url` selects — in shell store
  order, after that document's own `<script>`s, through the same `boa_engine::Context` via
  `JsRuntime::run_userscript(name, code, doc)`: same live DOM (`document.title = …`, DOM writes,
  `location.href = …` push history, dialogs block exactly like page dialogs) and the same
  per-script `catch`.
  **Identity is the contract**: `HostState::userscript` carries the running script's name for the
  duration of that one evaluation (a field on the host cell, set/cleared around the run —
  single-threaded, never a global race, never inherited by later timer callbacks), so
  * `console.*` made by a userscript → `ConsoleLog { source: "userscript", level, message:
    "{name}: {text}" }` (page scripts keep `source: "js"` with verbatim text),
    * a script that throws → `ConsoleLog { source: "userscript", level: "error", message:
      "{name}: {err}" }` plus a one-line `StatusText` of `Userscript error in {name}`; the throw
      kills only its own script — the next script runs, the tab stays alive and painting.
  With no scripts installed the whole path returns before touching the runtime (zero behaviour
  change).
- **JS-driven navigation is implemented**: `location.href=`/`assign`/`replace`/`reload` queue a
  real navigation (`HostState::pending_nav`) that the tab's poll loop follows through its single
  navigation entry point after each JS turn — `assign`/`href=` push history, `replace`/`reload`
  replace the current entry, relative targets resolve against the document URL (this is how
  search-engine JS redirect pages get followed). `javascript:` URLs are *executed* in the current
  document's realm (address bar, `<a href>`, or a `location` write), and a string completion
  value becomes a new `about:blank` document at the current history entry (browser/spec
  behaviour); `undefined` changes nothing; a throw is reported like any script error. Matching
  the shell contract, which passes `javascript:` text through to the tab.

### HTML forms (`src/forms.rs`)
Blitz parses forms but its submission algorithm is incomplete (`<select>` values are a TODO in
`construct_entry_list`; actions resolving outside `http(s):`/`data:`/`mailto:` are dropped), so
the tab owns submission outright — blitz's own submission is never used for its triggers:
- Triggers: a left-click press on a submit control (*arms*, finalised on release — a page JS
  `click` handler that `preventDefault()`s cancels it; the tab's entry list always replaces
  blitz's incomplete one in the navigation hook) and Enter in a single-line text field
  (implicit submission; the Enter is then not fed to blitz at all). Pointer/key events are
  replayed through the page's own JS event system (`__ironsurfFire`) around blitz's defaults.
- Entries follow the spec's "constructing the form data set" subset: tree order,
  `form=`/ancestor ownership, disabled/nameless/`datalist` exclusions, checkbox/radio
  checkedness, live parley editor text for inputs/textareas, select selectedness.
  GET becomes the query string; POST the request body per `enctype` (urlencoded,
  `text/plain`, multipart for string entries) — carried as `netmod::MainDocPost` into the
  main-document fetch, which POSTs for real (never cached) with the enctype `Content-Type`.
- Schemes: GET navigations work for every scheme we fetch (`http(s)`, `file`, `data`,
  `ironsurf:` — which blitz itself drops); POST only for `http(s)`, anything else is an
  honest status text, never a silent drop.
- `<select>` interaction lives here too (blitz renders options as plain text): clicking an
  option toggles its `selected` attribute + highlight and syncs the `<select>`'s `value`
  attribute. `prepare_document` seeds textarea editors from their child text and every
  select's initial selectedness once per parsed document, before the first layout.
- The welcome page (`ironsurf://welcome`) is a working GET-form demo: it echoes the query
  string its own form submits against it.

### Headless tests (must run with no X display)
- `tests/spike.rs`: render a `file://` fixture (tempdir HTML with colored blocks) through
  the full parse→resolve→paint path; assert buffer size `w*h*4` and non-uniform pixels.
- `tests/e2e.rs`: launch the built binary with a `file://` page, read
  `FromTab::{Ready, UrlChanged, Frame}` (non-uniform), then `Destroy` → exit 0.
- Boa tests: inline `<script>document.title="js-set-title"</script>` → `TitleChanged` with
  the JS-set title arrives over IPC; broken script → tab stays alive, frame still arrives.

---

## ironsurf-shell (binary) — Slint GUI

Slint `1.x` with **GPU acceleration, software fallback** (task: GPU): build with default
features **plus `backend-linuxkms-noseat`**, which compiles the LinuxKMS backend (DRM/KMS +
EGL/GBM). Default (no flags): Slint's backend chain runs winit → FemtoVG/GL on a desktop
session (auto-falls back to the software renderer where no GL exists), and LinuxKMS →
EGL/GBM FemtoVG on a bare VT (same software fallback). `--software` pins the old
software-only path; `--linuxkms` pins the KMS backend; explicit `SLINT_BACKEND`
(e.g. `linuxkms/femtovg`) always wins; `SLINT_SCALE_FACTOR` respected. The KMS backend
**links** (not dlopen) `libudev`, `libinput`, `libgbm`, `libxkbcommon` — build needs
`libudev-dev libinput-dev libgbm-dev libxkbcommon-dev` (see BUILD_DEBIAN.md).

UI (`crates/ironsurf-shell/ui/ironsurf.slint`):
- `AppWindow` with: toolbar row [back, forward, reload, URL `LineEdit`, "New tab",
  container dropdown], tab bar (horizontal scroll; each tab = container-colored stripe +
  title + ✕), page area (`Image` fed by active tab frame, `TouchArea`/`PointerAreaHandler`
  forwarding pointer+wheel events; keyboard `key-pressed`/`key-released` → `KeyEvent`).
- Container manager popup: list containers (color swatch), add/rename/choose color (fixed
  palette), "delete container (+ data)", per-tab "Move tab to container ▸" submenu (move =
  respawn tab process keeping URL), "New tab in container".
- Status bar: status text + loading spinner (timer animation) + https/lock indicator
  (⛬ / http warning).
- DevTools panel (F12 / Ctrl-I toggle, per tab): bottom-docked overlay Rectangle over the
  page area (never a `PopupWindow` — LinuxKMS has no popups), header [Console ‹n› |
  Network ‹n› | ✕] plus a Console view (level icon + colour, `[source] message`) and a
  Network table (Method | URL | Status | Size | Time; unknown values `—`; a failed request
  shows its error text in red instead of the status). Fed by the per-tab bounded logs
  (`TabState::console` / `network`), re-pushed on every drain tick while open.
- JS dialog overlay (`window.alert`/`confirm`/`prompt`): a centred modal Rectangle (title =
  the requesting tab, message, prompt LineEdit prefilled with the page's default, Cancel —
  hidden for alerts — + OK), declared after the page so it outranks every page overlay; the
  scrim is a dead TouchArea (a modal is dismissed by an answer, never by clicking past it).
  Fed by the shell's FIFO dialog queue (`FromTab::DialogRequest`, the requesting tab
  activated); OK/Cancel/prompt-Enter send `ToTab::DialogResponse` and unblock that tab.
  Closing or crashing a tab drops its queued/shown dialogs (cancelled).
- Userscript menu (top-bar "</>" button, right of ＋, toggles it): a right-docked overlay
  Rectangle over the page — same pattern as the container manager, never a `PopupWindow`
  (LinuxKMS has no popups) — and the two never stack: opening one closes the other. List
  rows from the Store: enable CheckBox (persisted through `Store::set_userscript_enabled`
  the instant it flips), bold name, URL patterns joined by `", "` in dim, ✕ deletes
  (`Store::delete_userscript`). Clicking a row **anywhere but its CheckBox/✕** (a
  bottom-z `TouchArea` — the layouts above it are click-transparent) edits that script:
  the same inline editor, prefilled (patterns rejoined as displayed), headed "Edit
  userscript". Footer "+ New script" reveals it empty, headed "New userscript". Either
  way Save upserts via `Store::upsert_userscript` — a **new** draft takes the id from
  `next_userscript_id` (read per Save; two Saves never collide), an **edit** keeps the
  row's id, its list position, and its stored `enabled` flag (the CheckBox owns that,
  the editor never flips it) — and a rejected script is **not** written: the `StoreError`
  text shows as a one-line error while the draft stays; Cancel drops the draft/edit. Broadcast policy: after every mutation
  (and at tab spawn/respawn) the shell sends `ToTab::SetUserscripts` with **all enabled**
  scripts to every live tab, best effort — crashed/closed tabs are skipped, send failures
  ignored (the crash reporting owns those); disabled scripts are never sent.
- User-Agent switcher (top-bar "UA" button, right of "</>", toggles it): the same
  right-docked overlay Rectangle (never a `PopupWindow`), and no two of the three
  full-window overlays (manager / userscripts / UA) ever stack — each open closes the
  others. The menu targets the **active tab's container**: rows are "Default
  (IronSurf)" first, then the five `BUILTIN_USER_AGENTS` presets, the row matching the
  stored value highlighted (pure `user_agents::ua_menu_rows`/`ua_display`); Copy puts the
  *effective* UA on the session clipboard (the tab default when there is no override),
  "Restore default" clears it, and a custom LineEdit + Apply normalises (trim; blank =
  default) before `Store::set_container_user_agent` — a rejected string shows its
  `StoreError` on the one line with the draft kept and nothing written. Every accepted
  change broadcasts `ToTab::SetUserAgent` to that container's **live tabs only** (tabs
  of other containers never receive it), and spawn/respawn of a UA container's tab
  appends `--user-agent <ua>` to that spawn alone (per-spawn args; the first page load
  already sends it).
- Archive mode (top-bar 💾 button, right of "UA", toggles it): the same right-docked
  overlay Rectangle, and no two of the four full-window overlays (manager /
  userscripts / UA / archive) ever stack — each open closes the others. The CheckBox
  flips the mode (**off by default**, persisted in `ui.json` alongside the root folder),
  broadcasts `ToTab::SetArchiveMode` to every live tab at once (**global** — containers
  are irrelevant), and a tab spawned while the mode is on launches with
  `--archive-mode` and archives from its first fetch. The destination is
  `<root>/<ISO8601 tab-process spawn timestamp>/<host[:port]>/…` (default root
  `$HOME/Downloads/ironsurf-tabs`, editable in the overlay: `~` expanded, relative
  paths rejected on the one line with the draft kept, the tree created on Apply).
  The **spawn instant** names the folder (chrono `%Y-%m-%dT%H:%M:%S%:z` — e.g.
  `2026-09-17T07:20:48+10:00/example.com/index.html`,
  `…/127.0.0.1:8000/image.png`), re-taken from that moment for tabs that were already
  live when the mode switched on — so a later run never overwrites an earlier one's
  folder. Tabs never write: each successful fetch arrives as `FromTab::ArchivedFetch`
  and the pump maps it to disk (a query-less document → `<host>/index.html`; anything
  else → the sanitized URL path with the **query kept**, appended to the last segment
  as part of the file name — `/html/?q=asdf` is the file `?q=asdf` *inside* `html/`,
  `/search?q=a` is `search?q=a`, so two queries of one URL never overwrite each other;
  the fragment is stripped, `%XX` decoded, dot-segments neutralized, each component
  clamped to 255 bytes — pure `shell::archive`, traversal refused at write time too);
  the status line counts the saves, failures are logged, never fatal.
- Menu (native or in-toolbar): New Window(v1: new tab), Close Tab (Ctrl-W), Tab Next/Prev
  (Ctrl-Tab), Open Container Manager, Clear Container Data.

Process/tab management (`src/tabs.rs`):
- `struct TabProc { id, container, child: Child, stdin, stdout, title, url, loading, can_back, can_fwd, frame: slint::Image, last_frame_age }`
- Spawn `ironsurf-tab` (resolve sibling of current exe; `--tab-id`…) with piped stdio;
  reader thread per tab → `mpsc::UnboundedSender<FromTab>` drained by Slint timer (UI updates
  via `slint::Weak<AppWindow>` invoke; throttle Frame messages: coalesce to newest).
- `Frame` → `slint::Image` from raw RGBA buffer (`slint::Image::from_rgba8_slab`/PixelBuffer API — verify name).
- Keyboard focus routing: Slint focus on page area → forward keys; Ctrl-L focuses URL bar;
  Esc unfocuses; F12 / Ctrl-I toggle the DevTools panel (consumed by the shell, never sent
  to the page; they act on the focused page, i.e. not while the URL bar holds focus).
- **A new tab opens in the address bar**: every tab open (`Ctrl-T`, the ＋ button, the
  container manager's New tab, a middle-click / `FromTab::OpenLinkInNewTab`, a context-menu
  Open link in new tab) fills the bar with the tab's start URL, bumps the same
  `focus-url-token` Ctrl-L / F6 use — so the bar takes keyboard focus with the **whole
  address highlighted** and the first keystroke replaces it — and activates the tab. (The
  first tab of a session is not a new tab: startup still hands focus to the page.)
  While that address is being drafted the shell stops pushing the tab's own URL into the
  bar, so the page that is loading in the background — or a redirect it follows — cannot
  overwrite the half-typed text; the draft ends with Submit, Esc, a click in the page,
  and a switch to (or the death of) its tab (`Core::url_draft`).
- Default start page: `ironsurf://welcome` (the tab-rendered built-in welcome page —
  `default_url()`), then URL bar typing does search-or-navigate: contains dot
  → `https://host` (upgrading typed `host/path` to HTTPS), else → `https://html.duckduckgo.com/html/?q=`.
  Explicit-scheme pass-through is case-insensitive and covers `http(s)://`, `about:`, `file://`,
  `data:` (whose base64 payloads never look like a host, e.g. `data:text/javascript;base64,…`),
  `ironsurf://` (the internal pages — the host has no dot, so without the scheme match
  `ironsurf://welcome` would silently become a search) and
  `javascript:` — the last is handed to the tab, which *runs it* as a program in the current
  page (never a fetch, never a history entry), instead of silently becoming a search.
- Container default from registry; per-container "remember last chosen container for new tabs"
  in `~/.local/share/ironsurf/ui.json` (best-effort).
- **Crashed tabs are loud, never silent**: child stderr is captured (bounded tail; `RUST_BACKTRACE=full`
  is set for children and the tab binary force-prints a backtrace on panic), the drain tick polls every
  child (`try_wait`), and a process that dies without `Closed` — or a send that fails (broken pipe, os
  error 32) — pins “Tab N has crashed” on that tab's status line, keeps the tab in the strip on its last
  frame, and logs the exit status plus the captured stderr/panic backtrace; further commands to it are
  dropped with the crash message standing (✕ still closes it).

### Developer tools (F12 / Ctrl-I)

Per tab, a bottom-docked overlay (Console + Network panes, see the UI bullet above) fed by
the tab's `ConsoleLog`/`NetworkEvent` stream: Console shows JS/CSS/DOM entries (`[source]
message`, level icon + colour); Network shows Method | URL | Status | Size | Time per
request, failures in red with their error text. Screenshots: `docs/screenshots/devtools.png`
(Console) and `devtools-network.png`, reproducible with `scripts/demo.sh` (launches the real
browser on `$DISPLAY` against a localhost fixture and drives it through synthetic xdotool
input). Known limitation inherited from the tab: parse errors inside `<link>`-fetched
stylesheets never reach the console (see §ironsurf-tab DevTools CSS seam).

Tests (headless CI-safe): tab-proc message pump logic + `FromTab` state reduction tested
against a **fake tab process** (a small test bin that scripts `FromTab` sequences) so shell
logic runs without the engine (Blitz) or X. `tests/devtools_e2e.rs` drives the **real**
ironsurf-tab through the shell's `TabProc` pump against a localhost fixture and asserts the
full DevTools feed (console sources js/css/dom + document/image `NetworkEvent`s with sane
method/status/size). GUI smoke test gated behind `$DISPLAY` + `IRONSURF_SMOKE=1`.

---

## Root-level deliverables

- `README.md`: features, screenshots note, build (`cargo build --release`), run
  (`target/release/ironsurf-shell`), sandbox & container explanation, limitations.
- `docs/ENGINE_INTEGRATION.md`: the Blitz/Boa embedding specifics discovered while
  implementing the tab crate (document lifecycle, paint/readback path, NetProvider quirks,
  Boa binding surface). Renamed from `SERVO_INTEGRATION.md` at the engine pivot.
- `scripts/demo.sh`: Xvfb/display screenshot of the running browser (if display available).
- `.gitignore` for `target/`.

## Verification gates per task

Each task MUST end with its own check passing:
1. common: `cargo test -p ironsurf-common`
2. storage: `cargo test -p ironsurf-storage` (registry lifecycle, cookie RFC rules incl.
   Secure-over-HTTPS-only, cache LRU eviction, **container isolation of profiles**)
3. sandbox: `cargo test -p ironsurf-sandbox` (probe child proves forbidden dir unreadable when Landlock is available; degrades gracefully otherwise)
4. tab/blitz: `cargo build -p ironsurf-tab && cargo test -p ironsurf-tab` (headless frame test;
   `cargo tree -p ironsurf-tab` must contain no `mozjs`/`wgpu`/`servo v` entries)
5. shell: `cargo test -p ironsurf-shell` (+ smoke with fake tab proc; GUI check if display)
6. end-to-end: shell driving two containers side-by-side (scripted), README + docs complete.
