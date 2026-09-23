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
    // right-click on the page: the tab has hit-tested what the cursor was over
    // (`ContextTarget`); the shell opens the context menu at (`x`, `y`) — logical
    // px in the page viewport — and each choice comes back as `ToTab::ContextAction`
    // (engine-side) or is serviced by the shell alone (clipboard, new tab,
    // history). The right-button pair never reaches blitz, so it never disturbs
    // the text selection the menu's Copy row is about.
    ContextMenu      { x: f32, y: f32, target: ContextTarget },
    // the page copied text (Ctrl+C / Ctrl+X / editor cut, surfaced through
    // blitz's `ShellProvider`): the shell files it on the session clipboard.
    ClipboardText    { text: String },
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
    // **DevTools Inspector**: the document tree as one flat pre-order list, sent when the pane
    // opens ([`ToTab::SetInspect`]), after every navigation while it stays open, whenever the tab
    // itself changed what is selected, and whenever the markup changed under a live inspector
    // (the tab re-walks the tree on its poll tick and stays **silent** while it is unchanged, so
    // an open pane never floods the pipe). Node ids are valid inside `generation` only.
    InspectSnapshot  { snapshot: InspectSnapshot },
    // **inspect element**: the deepest element under the cursor changed while the picker was
    // armed (`None` = pointer off page, or nothing hit-answers).
    InspectHover     { node: Option<u64> },
    // **inspect element**: a click landed with the picker armed — the tab selected `node`, sent a
    // fresh `InspectSnapshot`, and disarmed the picker.
    InspectPicked    { node: u64 },
    // **archive browsing server** (`ironsurf://archive`): the tab asks the shell to bring
    // up the localhost server over the archive folder — tabs are sandboxed and the archive
    // tree belongs to the shell, so only the shell binds a socket. Every override is
    // optional (blank = the shell's configured value: folder `$HOME/Downloads/ironsurf-tabs`,
    // host `127.0.0.1`, port random); `open_in_new_tab` (a link written
    // `ironsurf://archive?open=1`) asks for the listing in a *new tab* instead of moving
    // this tab to the server's control page. Answered with `ToTab::ArchiveServerInfo`.
    ArchiveServerStart { root: Option<String>, host: Option<String>, port: Option<u16>, open_in_new_tab: bool },
    // the server's own control page applied a new folder/host/port: the shell mirrors and
    // persists it (`port` 0 = stopped).
    ArchiveServerSettingsChanged { root: String, host: String, port: u16 },
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
    // replace the clipboard cache the editors' Ctrl+V reads (blitz asks its
    // `ShellProvider`, which answers from this cache). The tab cannot reach
    // the session clipboard itself; the shell pushes the current contents
    // just before relaying a Ctrl+V keystroke.
    Clipboard    { text: String },
    // the user picked a context-menu entry the **engine** services (paste,
    // video play/pause); shell-side entries never reach the tab.
    ContextAction(ContextAction),
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
    // **archive browsing server** reply: `ok` = listening (or already was) with `address`
    // = its `http://host:port` root; on failure `address` is empty and `error` says why
    // (port busy, folder refused). The tab renders it on `ironsurf://archive`, and steps
    // itself to `address + "/ironsurf/serve"` unless the request asked for a new tab.
    ArchiveServerInfo { tab_id: TabId, ok: bool, address: String, error: Option<String> },
    // find in page (`Ctrl-F`): install/replace the live query and re-run it from
    // the top. The shell owns the bar; the **tab** matches over the text it laid
    // out (case-insensitive, document order, visible text only) and paints it —
    // **every** hit yellow, the **active** hit light blue. First hit becomes
    // active and scrolls into view; the empty query clears query, highlights and
    // count. Answers `FromTab::FindResult`. The query survives navigations.
    SetFind          { query: String },
    FindNext,                             // next hit (wrapping), scrolls into view
    FindPrevious,                          // previous hit (wrapping)
    // DevTools **Inspector** pane open/closed for this tab: `true` snapshots the document now and
    // keeps the shell's tree current (one tree walk per change, never one per frame); `false`
    // stops snapshotting and drops the on-page highlight.
    SetInspect     { open: bool },
    // **inspect element** picker (Firefox's crosshair, and the context menu's "Inspect Element"):
    // while armed the tab **swallows** left-pointer events instead of handing them to blitz (no
    // hover DOM events, no click actuation, no selection drag), emits `FromTab::InspectHover` for
    // what is under the cursor, and answers the click with `FromTab::InspectPicked` — then disarms
    // itself and lets the page have pointers again.
    SetInspectPick { enabled: bool },
    // the shell selected a tree row / breadcrumb step: highlight that element's box over the page
    // and scroll it into view when it is not fully visible; `None` clears the highlight. An id
    // from an older `generation` is ignored, never an error (the pane can lag a navigation).
    InspectSelect  { node: Option<u64> },
    // right-click → **Inspect Element**: hit-test the page point (logical px, the space of
    // `ToTab::Pointer`) and select the deepest element there, as if that point had been clicked
    // with the picker armed, **minus** the pick event. The tab answers with a fresh
    // `InspectSnapshot` whose `selected` is the hit node.
    InspectPickAt  { x: f32, y: f32 },
    Destroy,
}

/// One node of the DevTools Inspector tree. The tree is **flat**: children follow their parent in
/// pre-order and carry `parent` + `depth`, so the shell indents, folds and re-selects without
/// parsing markup. `id` is a handle valid only inside one [`InspectSnapshot::generation`] (blitz
/// recycles its own node ids across navigations, so the tab mints these from 0 in document order).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InspectNode {
    pub id: u64,
    pub parent: Option<u64>,
    pub kind: InspectKind,     // Element | Text | Comment | Doctype | Document
    /// Tag name for `Element` (lower-cased in HTML, the parser's spelling in foreign content,
    /// `xlink:`-style prefix kept); `""` for every other kind.
    pub tag: String,
    /// Content attributes in document order (JS-set properties that never touched an attribute are
    /// absent, exactly like Firefox's markup view). Capped by `MAX_INSPECT_ATTRS`, each value cut
    /// to `MAX_INSPECT_ATTR_VALUE` chars with a trailing `…`.
    pub attrs: Vec<(String, String)>,
    /// Text node text / comment body / doctype subset (`html` for `<!DOCTYPE html>`); `""` for
    /// elements — their text lives in their `Text` children. Whitespace-only runs are **dropped**
    /// by the tab, other text is whitespace-collapsed and cut to `MAX_INSPECT_TEXT` chars with a
    /// trailing `…`.
    pub text: String,
    pub depth: usize,          // 0 = the `Document` row; the shell's indent unit
    /// `Some` only for a laid-out element with a non-empty box; `None` for `display: none` /
    /// `display: contents` / never-laid-out subtrees (`<head>`) / every non-element kind.
    pub bounds: Option<InspectBox>,
}

/// Border box in **viewport CSS px** — the space of `Element.getBoundingClientRect()`: the
/// document's scroll offset is already applied, page zoom is **not**. Negative coordinates are
/// honest (the element is scrolled out of view).
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct InspectBox { pub x: f32, pub y: f32, pub width: f32, pub height: f32 }

/// A whole document tree for the DevTools Inspector pane, size-bounded (`MAX_INSPECT_NODES` nodes;
/// the walk stops rather than cutting a subtree in half) so a real page never bloats an IPC frame.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct InspectSnapshot {
    pub generation: u64,        // bumped per new document
    pub url: String,            // post-redirect document URL
    pub nodes: Vec<InspectNode>,
    pub selected: Option<u64>,  // the box the tab paints on the page
    pub truncated: bool,        // a cap gave up; the pane says so at the bottom
}

pub const MAX_INSPECT_NODES: usize = 4096;        // nodes per snapshot
pub const MAX_INSPECT_ATTRS: usize = 24;          // attributes per element row
pub const MAX_INSPECT_ATTR_VALUE: usize = 256;    // chars per attribute value
pub const MAX_INSPECT_TEXT: usize = 200;          // chars per text row

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

/// What the cursor was over when the user right-clicked — every field is
/// "under the cursor at right-click time", and the shell decides from these
/// which menu entries exist (Firefox-style: an entry exists exactly when its
/// thing is there). `selection` is selected page text (whitespace-only and
/// over-cap count as no selection; cap `MAX_CONTEXT_TEXT` = 256 KiB);
/// `editable` = a text control (under the cursor **or** the focused one)
/// that can take a paste.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ContextTarget {
    pub link: Option<String>,          // nearest <a href>, resolved
    pub image: Option<String>,         // <img src>, resolved
    pub video: Option<String>,         // <video src>, resolved (empty = src-less)
    pub video_playing: bool,
    pub selection: Option<String>,
    pub editable: bool,
}

/// Context-menu entries the tab process services: `Paste { text }` inserts at
/// the focused editor (the tab loads the cache and synthesizes the Ctrl+V),
/// `ToggleVideo` play/pauses the `<video>` that was under the cursor.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ContextAction { Paste { text: String }, ToggleVideo }

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
    // What initiated the request: "document" | "script" | "resource" | "xhr" | "fetch";
    // None when the engine did not say.
    pub initiator: Option<String>,
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
  local-storage/                      # DOM localStorage: one JSON area file per origin
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
    pub fn local_storage(&self) -> LocalStorage;   // areas under local-storage/ (task: per-origin DOM storage)
    pub fn append_history(&self, url: &str, title: Option<&str>) -> Result<(), StoreError>;
}

pub struct Cookie { pub name: String, pub value: String, pub domain: String,
                    pub path: String, pub secure: bool, pub http_only: bool,
                    pub expires_unix: Option<i64> }
impl CookieJar {
    pub fn set_from_set_cookie(&mut self, request_url: &url::Url, header_value: &str, now_unix: i64);
    pub fn cookie_header(&self, request_url: &url::Url, now_unix: i64) -> Option<String>; // "a=1; b=2" (wire view, incl. HttpOnly)
    pub fn document_cookie(&self, request_url: &url::Url, now_unix: i64) -> String; // script view: same matching, HttpOnly hidden, "" when nothing visible
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
//  - HttpOnly is captured/persisted and still sent normally over HTTP, but
//    it is NEVER in the `document_cookie` (script) view — that attribute
//    exists precisely to hide a cookie from scripts while it keeps riding
//  - expired cookies are dropped on load/access; an already-expired
//    Set-Cookie (or Max-Age<=0, or a malformed Max-Age) deletes the cookie
//    it names instead of storing it

pub struct NetCache; // bounded by total bytes (~256 MB) & entries (~1000), LRU eviction
impl NetCache {
    pub fn lookup(&self, url: &str) -> Option<CachedEntry>;   // CachedEntry { status, headers: Vec<(String,String)>, body: Vec<u8>, etag, last_modified, cached_at }
    pub fn store(&self, url: &str, status: u16, headers: &[(String, String)], body: Vec<u8>, now_unix: i64);
    pub fn clear(&self);
}

pub struct LocalStorage; // DOM localStorage: one lazily-loaded JSON area per origin (task: per-origin storage)
impl LocalStorage {
    pub fn open(dir: impl AsRef<Path>) -> Self;
    pub fn get(&mut self, origin: &str, key: &str) -> Option<String>;
    pub fn set(&mut self, origin: &str, key: &str, value: &str) -> Result<(), StoreError>; // StoreError::QuotaExceeded over the per-origin cap
    pub fn remove(&mut self, origin: &str, key: &str) -> Result<bool, StoreError>;
    pub fn clear(&mut self, origin: &str) -> Result<(), StoreError>;
    pub fn len(&mut self, origin: &str) -> usize;
    pub fn key(&mut self, origin: &str, index: usize) -> Option<String>; // insertion order
}
pub fn storage_origin_key(url: &str) -> Option<String>; // durable storage origin, None = opaque

// localStorage rules (must be unit-tested):
//  - the area key is the serialized origin `scheme://host:port` with default ports folded in
//    (`http://example.com` == `http://example.com:80`; `https://example.com` is a DIFFERENT
//    origin), so path/query/fragment/user never split or merge an area;
//  - only `http`/`https` have a durable area — `about:`, `data:`, `file:`, `javascript:`,
//    `ws:`, `blob:`… resolve to no origin, and the caller (the tab) throws SecurityError for them;
//  - one JSON file per area under `local-storage/`, named by the lowercase hex of the origin key
//    (no host name in file names), written atomically (tmp+rename); corrupt files read as empty;
//  - quota is `MAX_LOCAL_STORAGE_BYTES` (5 MiB of UTF-16 code units over names + values, like
//    browsers) **per origin**, checked before anything changes — a refused write stores nothing;
//  - areas are container-scoped by construction: the store's directory is the container profile.
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
boa_engine = "0.22"   # JavaScript (pure Rust) — ALL language features:
                      # annex-b, intl_bundled, temporal, float16, xsum, …
boa_runtime = "0.22"  # the WinterTC Web-API layer: atob/btoa, queueMicrotask,
                      # structuredClone, TextEncoder/Decoder, AbortController,
                      # URL, console, process, fetch (features all+process)
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
   6a. **Text selection & copy/paste** is blitz-native and chord-transparent: dragging
   with the left button selects page text (blitz's `DragMode::Selecting`, painted by
   `blitz-paint`'s selection highlight) and Ctrl+C copies the live selection — the page's
   when none of it sits in an editor, the focused editor's otherwise — through
   `ShellProvider::set_clipboard_text`, which parks it for the poll loop to ship as
   `FromTab::ClipboardText`. Ctrl+V is the mirror: the editors read
   `ShellProvider::get_clipboard_text`, answered from the cache `ToTab::Clipboard` pushed.
   The **right-button pair never reaches blitz** (and never enters the held-button set):
   blitz collapses the selection on any button press, which would eat the context menu's
   own Copy target between press and menu; nothing else is lost, as the page never sees
   right-button DOM events in this engine.
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
  Companion rule: entries the server declared **explicitly fresh** (`Cache-Control`
  `max-age`/`s-maxage`/`Expires`, explicit freshness only — no heuristics, and none of
  `no-store`/`no-cache`/`must-revalidate`/`proxy-revalidate`; analysed by the pure
  `ironsurf_storage::fresh_until` from the cached headers at lookup time) are served
  straight from the cache without touching the network — and without re-capturing the
  stored `Set-Cookie`s, which were already applied when the response first arrived;
  everything else keeps the revalidation path.
- Main-document navigations always go through our own reqwest fetcher in the tab process
  (not via NetProvider), so the managed policy always applies to top-level requests
  regardless of provider mode. Concretely: **the container cookie jar is always
  attached to navigations under both policies** — cookies come back from the
  server and ride back on every later navigation and every redirect hop.
  Redirects are followed hop-by-hop (a manual-redirect client): each hop gets
  its own jar-computed `Cookie` header for its own origin, and each hop's
  `Set-Cookie`s bind to that hop; POST→GET conversion follows the browser
  rules (303 always, 301/302 convert; 307/308 keep method+body).
- **`Refresh` navigations are honoured** (task: Anubis `metarefresh` gates): the
  non-standard HTTP `Refresh:` response header (`2; url=/pass-challenge?…`) and the
  semantically identical `<meta http-equiv="refresh" content="2; url=…">` both arm a
  real delayed navigation. The header is parsed by `netmod::parse_refresh`/
  `refresh_from_headers` while the response is in hand (`DocFetch::refresh`, resolved
  against the final URL; `None` for local schemes); the meta tag is read from the live
  document after its scripts run and goes through the same parser. `Tab::schedule_refresh`
  sets `(deadline, target)`, the poll loop caps its sleep by that deadline (like a JS
  timer) and `Tab::follow_due_refresh` (step 5 of `drain_nav_hook`, so the headless harness
  gets it too) navigates when it comes due — never inline with the load that armed it, so a
  self-refreshing page spins the loop like any browser's refresh loop instead of the stack.
  The header wins when both are present; a directive with no target reloads the same URL;
  the separator may be `;` or the legacy `,`, `url=` is case-insensitive, and quotes around
  the target are stripped. This is exactly what lets a JS-less Anubis `metarefresh` challenge
  (which randomly emits one form or the other) clear itself: the challenge cookie captured
  from the gate response rides back on the refresh request, whose 302 lands on the real page.
  `Passthrough` therefore skips only the https-upgrade and the response cache
  on navigations. One jar instance (`Arc<Mutex<CookieJar>>`) is shared by the
  tab's main-document fetches and the provider's subresource fetches — cookies
  set by either are instantly visible to the other, and shutdown persistence
  flushes the one jar (never two copies overwriting each other). The response
  cache is shared the same way (`BlitzNetProvider::cache_handle()` gives the
  tab the very `Arc<Mutex<NetCache>>` the provider serves from), so documents,
  `<script src>` bodies and images/css/fonts all read and write one cache.
  Shared jar/cache state is reached through the `JarAccess`/`CacheAccess`
  traits, which take the mutex for the instant of one map operation only — no
  fetch ever holds a jar or cache lock across `send()`/`bytes()`, so every
  subresource of a page really does download in parallel (and a navigation
  never serialises behind the subresources it just spawned).
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
  `welcome` renders the start page (`WELCOME_URL` = `default_url()`, which documents and links
  `ironsurf://archive`), `archive` renders the summary of the shell's
  [archive browsing server](#archive-browsing-server-srcarchive_serversrc) — an address-less
  page by itself: it asks the shell (see `FromTab::ArchiveServerStart`) and shows what comes
  back, any other name renders an
  honest internal "no such page" that never left the process. All output escapes the requested
  URL — page names are user text, never markup.
- Container isolation holds even in Passthrough: one process = one container; its on-disk
  state lives under the sandbox-visible profile dir only.
- **DevTools network reporting**: one `FromTab::NetworkEvent` per settled or failed page
  request — main documents (every followed redirect hop, via the reporting fetch variants),
  provider subresources (`<img>`, `<script src>`, CSS) **and JS-initiated requests**
  (`XMLHttpRequest.send` and `fetch()`, which both run on the tab's own network stack)
  alike. A completed request carries method/status/bytes/duration; a failed one carries
  `error: Some(…)`, `status: None` — never both. Every event names its **initiator** in
  `initiator: Some(…)`: `"document"` (navigation / form POST), `"script"` (`<script src>`,
  ES modules), `"resource"` (images/CSS/fonts), `"xhr"`, or `"fetch"`. `data:` and
  unsupported schemes report nothing (no-spam); revalidated cache hits report the cached
  entry's status/size, and a direct fresh serve (no network at all) is one event for the
  original URL with that same status/size and duration 0. The shell appends these into the
  per-tab network log and renders the initiator column.

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
are a small, DOM-shaped surface.
**`<script type="importmap">` is supported** (task: importmap support): the extraction walk
recognises the type, never fetches (`src=` maps are invalid — import maps are inline-only)
and never executes it; the JSON body is parsed with the spec's forgiving rules (unknown
members ignored, malformed entries dropped, but invalid JSON or non-object top-level
`imports`/`scopes` discards the whole map with a note). The first valid map wins — a second
is ignored — and lands on `Extraction::import_map` and then `JsRuntime` for the document's
lifetime; `ImportMap::resolve` implements the spec's resolution (exact keys, longest
`/`-terminated prefix keys with remainder replacement, `null` unmappings, `scopes` shadowing
`imports` for requests from inside a URL prefix, bare-vs-relative specifier split).
**`<script type="module">` loading is implemented on top of it** (task: module loading): the
extraction walk treats `type="module"` as a first-class script — inline bodies collected,
`src=` bodies fetched through the same capped concurrent pool as classic scripts — and each
becomes a `ScriptSource` with `kind: Module` and its **authoritative URL** (its resolved
`src`, or the document URL when inline) as its origin. Every realm is built with a
`UrlModuleLoader` (boa 0.22's `ModuleLoader` host hook, replacing the default
*filesystem* loader) that resolves specifiers through the document's import map —
`ImportMap::resolve_for`, keyed by the importing module's URL so `scopes` apply — and
fetches bodies through the tab's own network layer, so lazily-imported dependencies ride
the same cookie jar, cache and DevTools `NetworkEvent` reporting (and archive sink) as
`<script src>` does. Modules are cached by absolute URL: two importers share one instance
(fetched and evaluated once), and cycles work — the entry module is registered before its
imports load. Top-level `await`, static and dynamic `import()`, and `import.meta.url` are
real; a module graph failure (unresolvable bare specifier, failed dep fetch, link clash,
thrown body) reports a `ScriptError` like any classic-script throw and never kills the tab.
Honest gaps: no MIME check on module responses (the classic-script fetcher answers any
2xx body as script text), and import attributes (`with { type: "json" }`) are ignored.
**Full Boa support is switched on** (task: as much JavaScript as possible): every
`boa_engine` language feature is compiled in — `annex-b` (legacy web compat: `substr`/`trimLeft`,
legacy escapes/regex quirks), `intl_bundled` (`Intl` with ICU data shipped *in the binary* —
locale formatting works with no system data), `temporal`, `float16`, `xsum` (+ `deser`,
`jsvalue-enum`, `native-backtrace`; only wasm/fuzz/debug-only knobs skipped) — and the whole
**`boa_runtime`** layer registers on every fresh realm before the IronSurf glue:
`atob`/`btoa`, `queueMicrotask`, `structuredClone`, `TextEncoder`/`TextDecoder`,
`AbortController`/`AbortSignal`, `URL`, a richer `console`, `process` (its `env` map is
emptied before page code runs — the tab leaks no environment) and `fetch`: the fetch
classes stay boa_runtime's, but the backend is the tab's own `TabFetcher` (the container
cookie jar, User-Agent cell, redirect policy and DevTools `NetworkEvent` queue — not
boa_runtime's private reqwest client), one `initiator: "fetch"` event per request. The
IronSurf glue registers over the names the tab must own — `console` (stderr + capture) and
the timers (the poll loop, not the runtime's internal timer channel) — via its plain
sloppy-mode assignments. Boa 0.22 does **not** drain its job queue inside `eval`, so the
runtime drains it once per script turn (`run_jobs` after every script batch, timer batch,
event dispatch and userscript): `Promise.then`/`queueMicrotask` callbacks now really run,
and `fetch` promises settle; a throwing job is reported as a script error (origin
`"promise"`) like any other.
**The DOM/Web surface grew a full Mastodon boot chain** (task: make https://infosec.exchange
render — it showed only the `<noscript>` shell): PRELUDE gains `URLSearchParams` (full WHATWG
form), `Event`/`CustomEvent`/`EventTarget` (dispatched events are real `instanceof Event`),
`DOMException` (spec-shaped, deliberately **no own `stack`** so core-js's probe installs its
wrapper over ours), an **in-memory IndexedDB** (stores, key ranges, multiEntry/compound
indexes, cursors, versionchange upgrades — Mastodon's `initializeEmoji()` gates `createRoot()
.render()` behind the `idb` wrapper's instanceof + request/tx event contract), `MessageChannel`/
`MessagePort`, microtask-delivered `MutationObserver`, `DOMParser.parseFromString` (real HTML5
fragment parsing through `parse_inner_html_into_mutator` + scoped `querySelector`/`matches`/
`closest` so the parsed tree is queryable), `window.history` (`pushState`/`replaceState` are
URL-only: they update the address bar via `pending_url` → `url_override` → `FromTab::UrlChanged`
— the engine's session history is untouched, same-origin is enforced tab-side), `getComputedStyle`
(a Proxy reading stylo `ComputedValues::clone_<property>()` accessors), `reportError`,
`document.hasFocus`, `crypto`-adjacent odds, and `document.createElementNS` (new native
`__ironsurfDomCreateElementNs` builds the `QualName` with a real svg/mathml `Namespace` —
react-dom creates **every `<svg>`** through it and mastodon's landing leads with SVG icons).
All HTML*/SVG*/`Node`/`Element`/`Document` interface classes exist with `Symbol.hasInstance`
predicates keyed on node type + local tag name (react-dom's `x instanceof HTMLIFrameElement`
checks decide event paths, and must not lie), while `Element.prototype` **is** the shared
wrapper prototype — polyfills that read `Element.prototype.matches` or install accessors on
`HTMLElement.prototype.inert` land on real wrappers. `window.fetch` unwraps same-realm
URL-likes to `href` before handing to boa_runtime's fetcher (it rejects unknown URL objects).
The `checked` setter is a silent no-op on non-checkbox inputs (react hands `checked={false}`
to text inputs; a hard TypeError previously killed mastodon's search column behind an error
boundary), bare `xmlns` is settable (react-dom stamps it on every SVG root), `classList` is a
snapshot DOMTokenList over the `class` attribute, and both `console.error(err)` and module
rejections print error **stacks** — the single most valuable debug affordance added this round.
**Foreign content keeps name case** (task: black blobs over the first Mastodon posts). HTML
lower-cases names; SVG/MathML must not. `usvg`, stylo and the SVG DOM look up `viewBox`,
`gradientUnits`, `preserveAspectRatio`, `linearGradient`/`clipPath`/`feGaussianBlur`
case-sensitively, and an `<svg>` root carrying a lower-cased `viewbox` silently loses its
coordinate system: `usvg` falls back to a 24-unit viewport, so a glyph authored on a 960-unit
grid (Mastodon's whole icon set) paints ~40× oversized — a 24 px icon became a 720x560 black
fill over the timeline. So `qual_name_for_attr(name, element_ns)`/`qual_name_for_ns` lowercase
**only inside the HTML namespace** and otherwise apply html5ever's "adjust MathML/SVG
attributes/tags" renames (`viewbox`→`viewBox`, `lineargradient`→`linearGradient`) — the same
table the markup parser already runs, since the DOM spec's name-adjustment step owes
`setAttribute` the fixup too. Reads stay case-insensitive the way browsers made them
(`getAttribute("viewBox")` finds a stored `viewbox` and vice versa). Pinned by the
`js::foreign_name_case` units and
`tests/headless.rs::svg_viewbox_case_survives_setattribute_and_icons_paint_their_size`, which
asserts on the *recorded paint*: every black fill ≤40 px, not a column-wide blob. Honest gaps:
timers still drive nothing off `document.visibilityState`, and unhead/helmet's deferred-init
timer still throws once (`createContext` undefined there — a module-cycle quirk, page survives).
**Lazy route chunks actually mount** (task: infosec.exchange/explore posts render): three
pieces close the Vite/React-lazy chain. (1) `rel` is a **reflected** string attribute on
`<link>`/`<a>`/`<area>`/`<form>` (with `as`/`crossOrigin` on `<link>`) — blitz's
`load_linked_stylesheet` reads the `rel` *content attribute*, so `link.rel = "stylesheet"`
must write it (a stored own property left the dynamically inserted sheet inert and never
fetched); tags without the member keep own-property semantics. (2) Blitz applies a fetched
sheet to the stylist but never fires the link's DOM `load` event, and `__vitePreload`
resolves its CSS-dependency promise on exactly that event before calling the route chunk's
dynamic `import()`; `Tab::dispatch_stylesheet_loads` (called from every `settle`) fires it
once per node, keyed by `SpecialElementData::Stylesheet`. (3) `<a>`/`<area>` carry the
HTMLAnchorElement **URL decomposition members** (`protocol`/`host`/`hostname`/`port`/
`pathname`/`search`/`hash`/`username`/`password`/`origin`) over the resolved `href` —
Mastodon's link-card builds an anchor and reads `.hostname`, and an absent member is
`undefined`, which its punycode decoder then `split`s into a `TypeError` that aborts the
whole status render. Finally, `Tab.anim_clock` feeds `BaseDocument::resolve`'s
`current_time_for_animations` (was a constant `0.`): Mastodon's `.status{animation:…fade}`
(`0%{opacity:0}`) was frozen at its start keyframe, so a column full of fetched statuses
painted blank. The oversized SVG fill that used to paint over the first posts (~720x560 bboxes
clipped to the column) was **our** bridge corrupting `viewBox`, not a blitz paint bug — see the
name-case invariant above; the `--dump-paint-detail` headless flag (bbox/brush/fill-rule/clips of
every recorded fill, `IRONSURF_FILL_MIN_AREA` sets the floor, default 1500 px²) is what located
it and is what the regression test asserts through.
**`boa_ast` is forked** (task: the explore-page react-spring `TypeError: cannot convert
'null' or 'undefined' to object`): upstream `Scope::escape_this_in_enclosing_function_scope`
stops escaping `this` at the first enclosing function scope — even an arrow's, which cannot
bind `this` — so any `this` reached through **2+ nested arrows** lost its binding: the real
`this`-binding function never got `requires_function_scope`, its function environment was
never pushed, and the `This` opcode fell through to `undefined` (modules) / `globalThis`
(scripts). Vendored as `thirdparty/boa/boa_ast` (stock 0.22.0 + a two-hunk fix: function
scopes record arrow-ness; the escape walk keeps going through arrow scopes) and wired in
via `[patch.crates-io]` in the root `Cargo.toml` — the fix is tracked in git, not applied
to registry checkouts; see `thirdparty/boa/PATCH.md` (upstream `main` is still unfixed).
Live effect: chevron_right's rAF frame flush no longer throws on infosec.exchange/explore.
The binding surface stays:
- `window` (eventTarget-ish no-ops), `document.title` get/set (set → `FromTab::TitleChanged`
  and repaint), `document.body.textContent` read-only snapshot,
- **`window.matchMedia(query)`** returns a MediaQueryList without evaluating media features:
  `matches` is true only for the whole-media queries `""`/`all`/`screen`/`only screen` and false
  otherwise (`prefers-color-scheme: dark` included); listeners register as no-ops and `change`
  never fires. A *callable* `matchMedia` is what such reveal scripts need — its absence killed
  startpage.com's first inline script ("TypeError: not a callable function") before it could lift
  the page-shipped `#root { opacity: 0 }`, leaving the whole page (search bar included) invisible.
- **`element.dataset`** is a live `DOMStringMap` Proxy over the `data-*` attributes with the
  spec's camel-case mapping (`siteTheme` ⇄ `data-site-theme`, `col2` ⇄ `data-col2`, leading
  capitals take no dash); reads re-read the document, writes/deletes go through the same
  `__ironsurfDomAttr*` natives as `setAttribute`, so they restyle and repaint like any attribute
  mutation, and the keys enumerate (`for (k in el.dataset)`).
- **`window.Image` / `HTMLImageElement` are implemented** (task: `ReferenceError: Image is not defined`):
  `new Image([width[, height]])` really creates an `<img>` **in the live document** and leaves it detached
  (`isConnected === false`, like a real one); the size arguments go through WebIDL `unsigned long` coercion
  (garbage/negatives read `0`, fractions truncate) and an *absent* argument writes no attribute at all.
  `Image.prototype === HTMLImageElement.prototype` (the node-wrapper prototype), so `instanceof` holds for parsed
  and script-built images alike. The interface members ride that prototype: `complete`/`naturalWidth`/
  `naturalHeight`/`currentSrc` read what blitz attached to the element (one new native,
  `__ironsurfDomImageState` → `[loaded, naturalWidth, naturalHeight]` from `SpecialElementData::Image`),
  `width`/`height` read the content attributes as HTML's reflected non-negative integers falling back to the
  intrinsic size, and `decode()` resolves at once. With them, **`src` and `href` became reflected URL
  attributes** on the tags that carry them (img/input/script/iframe/audio/video/source/track/embed; a/area/link/
  base): the getter answers the url *resolved against the document url* (`__ironsurfDomResolveUrl` — the same
  `Url::join` the media fetcher uses), the setter writes the content attribute through the `setAttribute` native,
  so `img.src = "x.png"` really fetches once the element is in the tree, and `a.href` reads absolute without
  navigating. A tag with no such IDL member keeps the browser's own-property semantics (`div.href = "x"` stores a
  string) — the accessors never swallow a page's data. Still TODO: a *detached*-image preload — blitz requests
  `<img src>` when the element is inserted, so a bare `new Image(); i.src = …` beacon starts no fetch and never
  fires `load` (append the image and it loads for real, verified by `tests/e2e.rs`:
  `script_built_image_reads_engine_state`).
- **`location.origin` serialises the document's origin** (task: infosec.exchange's emoji-data loader did
  `new URL(path, location.origin)`; with `origin` absent the base was undefined and WHATWG parsing threw
  "relative URL without a base"). It is the *serialized tuple* origin `scheme://host:port` — explicit ports
  are kept (origins do not fold default ports, unlike the localStorage area key above) — and documents with
  no tuple origin (`about:`, `data:`, `file:`…) answer the spec's opaque string `"null"`, which makes such a
  base fail loudly instead of silently resolving against nothing.
- **Canvas is a paint surface, honestly empty** (task: Mastodon paints every custom-emoji blur placeholder
  through `canvas.getContext("2d").putImageData(createImageData(w,h),0,0)` — no getContext meant the *call*
  itself threw into the app's catch and logged `Blurhash decoding failure`; the decode is pure JS). `getContext`
  lives on the element prototype: `"2d"` answers a context, `"webgl"`/`"webgl2"`/`"bitmaprenderer"`/unknown
  answer `null` like a browser with them disabled, and non-canvas elements answer `undefined`. The 2D context
  keeps state as plain own properties, every drawing call is a silent no-op (there is no rasteriser and no GPU
  here), `createImageData`/`getImageData` return real `Uint8ClampedArray`-backed ImageData, and reads answer
  the fresh all-zero buffer — **nothing painted ⇒ nothing readable back**, never fake pixels.
  `width`/`height` are canvas-reflected content attributes with the HTML 300×150 defaults (extending the img
  accessors above, so `canvas.width = canvas.width` round-trips). `toDataURL`/`toBlob`/`OffscreenCanvas` are
  deliberately *absent*: encoding bytes we never rendered would lie, and their callers (upload paths) feature-detect.
- **`window.localStorage` is a real `Storage`** (task: per-origin DOM storage): `getItem`/
  `setItem`/`removeItem`/`clear`/`key(i)`/`length`, plus the spec's Proxy forms — `localStorage.foo = 1`
  stores, `localStorage.foo` reads, `delete localStorage.foo` removes, `Object.keys` enumerates the
  stored names. Every operation funnels through ONE native answering a `[status, payload]` envelope,
  and **the per-origin security rule is enforced on the Rust side, never in JS**: the area key is the
  document's origin re-resolved from the host URL on *every* call (never cached across documents or
  calls), only `http`/`https` documents have an area, and opaque documents (`about:`, `data:`, `file:`,
  `javascript:`…) get a `SecurityError` (a JS `Error` with `name = "SecurityError"`) out of every
  access; over the per-origin 5 MiB quota `setItem` throws `QuotaExceededError`. Areas persist under
  the container profile's `local-storage/` (ironsurf-storage) as one JSON file per origin — so
  storage is per container *and* per origin (two tabs of one container on different origins never
  read each other's data; a tab restart keeps its areas; the sandbox hides them from other
  containers).
- **`document.cookie` reads and writes the container's live cookie jar** (task: the axios XSRF
  read — Mastodon's request layer did `document.cookie.match(…)`, read `undefined`, and died
  with boa's `TypeError: cannot convert 'null' or 'undefined' to object`). Both directions funnel
  through ONE native (`__ironsurfCookies(op, value)`), and all rules live on the Rust side: the
  document URL is re-parsed from the host state on *every* call (never cached), only `http`/`https`
  documents have cookies at all, and reads answer `CookieJar::document_cookie` — the jar's
  **script-visible view**: the identical domain/path/Secure/expiry matching the wire `Cookie:`
  header uses (longer paths first), with `HttpOnly` entries hidden. Writes ride the jar's RFC6265
  `Set-Cookie` parser (so `Path`/`Domain`/`Expires`/`Max-Age` all behave like the network path,
  including delete-by-expiry), after the native strips any `HttpOnly` attribute — scripts can
  never mint one. Cookieless documents (opaque origin, bare tests, the headless harness) read
  `""` and silently drop writes — **never `undefined`**, which is what made axios die. The tab
  hands the JS host the very `Arc<Mutex<CookieJar>>` the network provider captures `Set-Cookie`
  into, so a server cookie is script-readable the moment the response lands and a script-set
  cookie rides out on the very next request (`tests/cookies.rs` pins both halves: the exact page
  title never contains the HttpOnly value, the server-side `Cookie:` header always does).
- **`fetch`/`Request` accept what real pages hand them** (task: infosec.exchange's API layer —
  Mastodon's axios builds `Object.assign(Object.create(null), N, {signal: O, …, body: data})`,
  a null-prototype init whose nullable members are present-but-null). Two glue steps, in PRELUDE:
  a present-null `signal`/`body` normalises to `undefined` through a shallow copy (boa's `TryFromJs`
  demuxes only `undefined`; a present `null` died as "cannot convert value to a Object" for the
  signal and "body is not a supported type" for the body — every Mastodon API request carried
  `signal: null`); and a string resource is resolved against the DOCUMENT base like browsers —
  Mastodon's same-origin `/api/v2/…` style survived as an authority-less URI into reqwest, whose
  build failure surfaced as the http crate's Display, literally `builder error`, an opaque rejection
  no one could debug. `null`/`undefined` resources are NOT absolutized (they must reject TypeError,
  not silently GET the base URL — pinned), the caller's init is never mutated, and the `Request`
  wrapper returns the raw instance keeping `instanceof` semantics (`tests/fetch_init.rs` pins the
  full axios-shaped init end-to-end against a live local server, incl. the relative-path case).
- **Real React runs end-to-end** (task: ECMAScript/React support; `tests/react_render.rs`).
  The committed React 19 + react-dom IIFE bundle (`tests/fixtures/react19/`, MIT, see its
  NOTICE) mounts through the DOM bridge on a headless localhost tab: `createRoot(...).render()`
  paints a real tree, `useState`/`useEffect`/`useLayoutEffect` run, synthetic events fire from
  `el.click()`, the keyed list reconciles, and the page's own title reports the measured box,
  click count, viewport width and the `Symbol`/`Object.assign`/`Promise` feature checks. The
  ES6/DOM/timer floor the task named is pinned in the same file (`es6_dom_timer_surface_is_complete`)
  and by the extensive `js.rs` unit surface.
- **`dispatchEvent` keeps event identity** (task: React synthetic events). The dispatch paths
  (`Node.prototype`, `document`, `window`) cloned every event that lacked the internal
  `__ironsurfEvent` marker to support plain-object `{type: "click"}` events — but the marker was
  never set, so they cloned *real* events too: a listener's `preventDefault()` set
  `defaultPrevented` on the copy, invisible to the caller, and `ev.target === el` failed. `Event`
  (and every `makeEvent`/UI-event instance, which copies its keys) now carries the marker, so a
  genuine event flows through **unchanged** — `defaultPrevented`, the `false` return and `target`
  all reflect the dispatch — while plain objects are still wrapped. `eventPath(host)` also
  special-cases a dispatch **on `window`** (path `[host]`): the old shape appended `document` as
  the target slot, so window listeners only ever ran in the capture phase.
- **CSSOM View geometry is bridged and forces layout** (task: React measuring hooks). ONE native,
  `__ironsurfDomRect(docId, nodeId)`, answers the whole box model as an array; the PRELUDE accessors
  compose it: `getBoundingClientRect()`/`getClientRects()` (viewport/client coords, `[]` for a
  boxless node), `offsetWidth`/`offsetHeight`/`offsetLeft`/`offsetTop` (blitz's spec `offset_top_left`
  against the real `offsetParent`), `clientWidth`/`clientHeight`/`clientTop`/`clientLeft`,
  `scrollWidth`/`scrollHeight`, and `scrollLeft`/`scrollTop` (reads plus a real writer that scrolls
  the root's viewport or any element's overflow box, clamped at 0, and asks for a repaint). The
  native calls `document.resolve(0.)` **first**, like a browser's forced synchronous layout: a React
  `useLayoutEffect` that measures a tree committed earlier in the same script turn reads real
  numbers (a `width:200px; padding:5px` box answers 210×60, not a stale zero). `display:none`,
  `display:contents` and detached elements answer the all-zero rect and an empty `getClientRects()`.
- **The viewport is live** (task: React responsive code). `window.innerWidth`/`innerHeight`/
  `outerWidth`/`outerHeight` are getters over the tab's CSS-px size (`HostState::viewport_width/height`,
  written at realm creation and on every `ToTab::Resize`), not the old 1024×768 constants;
  `window.scrollX`/`scrollY`/`pageXOffset`/`pageYOffset` read the document's viewport scroll, and
  `window.scrollTo`/`scrollBy` (and the legacy `window.scroll` alias) move it. A real `ToTab::Resize`
  also dispatches a `resize` event on `window` **after** layout settles, so the page's handler sees
  the new `innerWidth` and React re-renders (`tests/react_render.rs` asserts `RESIZED:900` after a
  1024→900 resize).
- **Framework plumbing that a React ecosystem feature-detects** (task: React ecosystem):
  `performance` (`now()` from a monotonic native clock started at realm build, plus `timeOrigin`
  and no-op User Timing methods, empty `getEntries*`), `requestIdleCallback`/`cancelIdleCallback`
  (a one-tick task on the timer queue with the real `{didTimeout, timeRemaining}` deadline),
  `ResizeObserver` (optimistic like `IntersectionObserver`: one delivery per `observe()` carrying
  the element's real settled `contentRect` plus the modern `*BoxSize` arrays; no re-delivery, so
  nothing loops), `CSS.escape` (the CSSOM algorithm) and a `CSS.supports` that round-trips through
  the real style parser, and `isSecureContext` (https or loopback). Plus the Node/Element
  conveniences libraries call constantly — `append`/`prepend`/`before`/`after`/`replaceWith`/
  `replaceChildren` (strings become text nodes), `insertAdjacentHTML`/`Element`/`Text` (parse via
  the existing `innerHTML` path), `toggleAttribute`, `isSameNode`/`isEqualNode` and `normalize` —
  all thin compositions of the primitives above.
- `console.log/info/warn/error` → tab stderr + `FromTab::StatusText` **and** a
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

**Reddit's JS proof-of-work gate is passed** (task: reddit.com renders the subreddit feed). The
first response is a shell page whose inline script — on `DOMContentLoaded`, with `{once:true}`
— reads `document.forms[0]`, stamps a computed token through `form.elements.namedItem(...)` and
`form.requestSubmit()`s the GET form; only the answered request gets the real page. So the
realm grew the document lifecycle and three form members: (1) `document.readyState` is a real
lifecycle (`loading` while the document's scripts run, `interactive` when the runtime's
`__ironsurfContentLoaded` hook fires, `complete` with `window` `load` — the hook is idempotent
and fires both events through the realm's event machinery, after the page's scripts, the point
a browser fires `DOMContentLoaded`); (2) `customElements` is an honest **registry** only
(define/get/getName/whenDefined/upgrade, spec name validation, `SyntaxError`/`NotSupportedError`
on bad or duplicate names, `whenDefined` resolving through the job drain) over a real
`HTMLElement` base — there are **no upgrades** (no shadow roots, no lifecycle callbacks; every
element stays `:defined`, the invariant the vendored `thirdparty/blitz-dom` patch's stylo
`:defined` handling encodes), which is correct for reddit's SSR feed and lets
`class X extends HTMLElement` bundles parse instead of dying on the name; (3) `PerformanceObserver`
is the record-only surface (`observe`/`disconnect`/`takeRecords`, `supportedEntryTypes: []`,
`TypeError` on a non-function callback) so telemetry bundles keep feature-detecting; (4)
`document.forms` is a live list, `form.elements` a live `HTMLFormControlsCollection`(Proxy) with
`namedItem`/`item`/numeric+name indexing, and `requestSubmit`/`submit` fire the cancelable
bubbling `submit` event then resolve the submission through `__ironsurfFormSubmit` — the tab's
own live entry list — queueing a GET navigation on the poll loop's JS-navigation queue (a
script-set `value` and script-appended hidden inputs submit exactly like a click; POST from
script reports, since the JS hook carries no body). `DOMException.prototype.toString` answers
`"Name: message"` like a browser so console errors stay one line. Verified live (23
`shreddit-post` nodes, real post titles, zero load-blocking js errors) and pinned by
`js::reddit_pow_tests` (lifecycle/registry/observer/realm surface) plus
`tests/pow_shell.rs`, whose localhost fixture only reads `POW-OK` when the queued navigation
carried the doubled token.
**One known page-noise error** on reddit: a deferred script copy inside the suspense flow
re-runs in our engine (no upgrades to gate it), so the second, unguarded
`customElements.define("faceplate-perfmark", …)` throws `NotSupportedError` — the same throw a
browser without upgrades would make; the feed renders regardless.

### Web Workers (`src/worker.rs`)

Classic dedicated workers on **real OS threads**: `new Worker(url)` (same-origin script or a
`blob:` URL the document made with `URL.createObjectURL`) starts a thread that owns its own Boa
realm, and the tab keeps one message bus per navigation. A worker realm is built from
`WORKER_PRELUDE` (scope as event target, `self`, `postMessage`, `importScripts`, the realm's own
timers, `console`, `crypto`, `location`/`name`, `fetch` on the tab's network stack); the document
realm gets `WORKER_PRELUDE_MAIN` (`Blob`, `URL.createObjectURL`/`revokeObjectURL`, `Worker`,
`__ironsurfDeliverWorkerMessages`). Both preludes are `pub const` strings in `worker.rs`, appended
to the realm they belong to — the page-facing bindings stay in `js.rs`.

The contract is deliberately narrower than the platform, and the narrowness is the API: payloads
travel as **JSON text** (`JSON.stringify` on the way in, `JSON.parse` on the way out; a circular
or `undefined` payload is refused rather than silently emptied), no `MessagePort`/
`MessageChannel`/`BroadcastChannel`, no transferables, no module workers (`{type: "module"}`
answers with a named error), `Worker.terminate()` is synchronous, and a worker dies with its
document (navigation clears the registry, no persistent origin). Deliberately absent: `WebSocket`
and `EventSource` (no socket or event-stream transport in this engine — a `ReferenceError` beats a
stub that drops data).

Three boundaries are load-bearing and tested:

- **Worker → tab**: posted messages land on a queue in the `WorkerHost` (`WorkerMessage` = worker
  id, kind, JSON payload) and the tab's wake pipe is poked, so a page with idle workers costs no
  wakeups. The tab loop calls `JsRuntime::deliver_worker_messages()` per turn; that hands the whole
  batch to the realm glue as **one JSON string literal** (`json_string`-escaped) — the glue parses
  it once and returns the number of events it dispatched. Passing the batch as an array/object
  expression instead makes the glue's `JSON.parse` fail and silently drop every message.
- **Tab → worker**: `worker.postMessage(data)` resolves against the *document's* URL for `blob:`/
  relative script URLs, and every request a worker makes (script body, `importScripts`, `fetch`) is
  reported to the same DevTools network queue as a document request, with the shared jar and live
  user-agent cell — a worker is a network client of its document, not a separate principal.
- **Delivery is a JS turn**: `JsRuntime::deliver_worker_messages()` runs inside `Tab::run_due_timers`,
  not at the top of the loop, because the page's `onmessage` is a script turn and owes exactly what a
  `setTimeout` callback gets — `settle()`, a repaint request, `take_js_title()`, and
  `consume_js_status()`. Skipping that last step strands the `console.*` lines a page logs from its
  own `onmessage` in the realm's host capture (an idle page never wakes again to ship them), which is
  why a drain outside the timer step silently loses the reply's DevTools output.

Tests: unit tests in `worker.rs` (blob store, registry lifecycle, `terminate`, wake counts, and
`a_page_realm_talks_to_a_worker_thread`, which runs a page realm, a worker thread and the delivery
step together) plus `tests/workers.rs`, which drives the **real** tab binary against a localhost
fixture and asserts a page receives an object its worker posted (and that a worker's `fetch` shares
the document's cookie jar).

### SHA-256 (`src/sha256.rs`)

The engine answers exactly one digest natively: `crypto.subtle.digest("SHA-256", …)` in **worker**
realms, backed by the `__ironsurfSha256` native (dependency-free FIPS 180-4 — the 64 round
constants, the standard message schedule, the compression function). It exists for one class of
pages: Anubis proof-of-work gates (startpage.com's search and the rest of that family, including
the ones that hand the work to a `Worker`), whose challenge asks for `floor(difficulty / 2)`
leading zero **bytes** of `SHA-256(randomData ‖ nonce)` — tens of thousands of digests that hold a
page at “Calculating…” for minutes interpreted and finish in well under a second natively. Nothing
else is implemented and nothing is pretended: `digest` rejects every other algorithm with
`NotSupportedError` instead of answering with a different digest, `subtle.sign`/`encrypt`/
`deriveBits` are absent, and the **document** realm keeps `crypto.subtle` missing entirely (task:
XMLHttpRequest + Web Crypto), so feature detection stays honest in both realms.

**A whole Anubis gate now passes end to end** (startpage.com's search: challenge page → worker
PoW → clearance → real page), and the four members it needs beyond the worker/digest machinery
are pinned by unit tests: `navigator.cookieEnabled` (must exist and be `true` — a missing/falsy
member sends Anubis down its “browser blocks cookies” error path, which on its themed page
dereferences `#title`/`#image` it does not have and dies as an unhandled rejection);
`HTMLElement.lang`/`dir` (reflected string attributes on every HTML element, so
`document.documentElement.lang` answers the locale instead of `undefined` — the
`locales/undefined.json` 404 the shell surfaced); `URL.prototype.searchParams` (boa_runtime
answers it with a hard “not implemented” *error*, and Anubis's pass-challenge step is exactly
`new URL(…).searchParams.set(…)`; the returned `URLSearchParams` writes the query back through
every mutator); and the cookie jar's `Expires` parser (`cookies.rs` had a 0-indexed month table
feeding a 1-indexed `days_from_civil`, so a cookie expiring 30 minutes out was stored as
already-expired and never rode the pass-challenge request — Anubis answered “cookies appear to
be disabled”).

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
- Status bar: **loading progress bar** at the left-hand end (visible only while the active
  tab loads): a 96 px recessed track with a 24 px lit segment sweeping along it
  (`load-progress` 0…11, 4 px a step, ping-ponging; the spinner timer's 120 ms tick
  advances it and every load start zeroes it — `load_sweep_position`), and the text
  column indents past it while it is up. The tab protocol reports a load as one bool
  (no byte counts), so the progress is honestly indeterminate and the sweep is the
  progress. Then status text + loading spinner (timer animation) + https/lock
  indicator (⛬ / http warning).
- DevTools panel (F12 / Ctrl-Shift-I toggle, per tab): bottom-docked overlay Rectangle over the
  page area (never a `PopupWindow` — LinuxKMS has no popups), header [Console ‹n› |
  Network ‹n› | Inspector | ⌖ | Clear | ✕] plus a Console view (level icon + colour,
  `[source] message`), a Network table (Method | URL | Initiator | Status | Size | Time;
  unknown values `—`; a failed request shows its error text in red instead of the status) and
  an **Inspector** (HTML tree, see §Developer tools). `Clear` empties the visible pane's feed
  (Console / Network only); ⌖ toggles *inspect element* (`Ctrl-Shift-C`) for the whole tab.
  Fed by the per-tab bounded logs (`TabState::console` / `network`) and the tab's
  `InspectSnapshot`, re-pushed on every drain tick while open. Its **top edge is draggable**
  (`MouseCursor.ns-resize`): the height is a shell-owned number per tab (`devtools-height`,
  240 px default, 80 px floor, ≤90 % of the page area once dragged), reported as
  `devtools-edge(kind, travel)` — press/move/release with travel positive up, measured off the
  press so a clamped drag cannot drift) and *stored* as that number, not a share: a window
  resize re-fits what paints, the reader's own number survives a shrink-then-grow.
- **Inspector pane** (third DevTools tab): breadcrumb row (the selected node's ancestor chain,
  `html › body › div#main`, rightmost first, each clickable) over a ListView of the tree. Each
  row is `depth × 12px` indented, shows a `+/−` twisty on elements with children, then syntax
  **highlighted** markup built from nested token models — a row is `Vec<Token>` where a token is
  `{ text, kind }` and kind maps to a fixed colour table: punctuation `<`/`>`/`/`/`=`/quotes grey,
  tag name orange, attribute name blue, attribute value green, text near-white, comment
  grey-italic, doctype grey-italic, `id`/`class` values get their own accent. Whitespace-only text
  nodes are never shown; a `truncated` snapshot says so as a dim last row. Clicking a row selects
  that node (`ToTab::InspectSelect` → the tab paints its box and scrolls it into view) and
  highlights the row (selected row background + the breadcrumb rewritten); `+/−` folds a subtree,
  `Enter`/click on the twisty folds it too, and selection from the page (⌖ pick, or the context
  menu's **Inspect Element** → `ToTab::InspectPickAt`) moves the tree: the selected row and every
  collapsed ancestor are expanded and made visible — and *made visible* means scrolled: only the
  pane knows its own height, so the view-model hands it the row of the page-side pick with a
  counter that only rises (the same node picked twice must move again), the tree pans by the
  smallest amount that shows that row, and because a ListView scrolls only as far as it has
  realized its rows the pane re-asks in strides until the offset the tree reports agrees. A row
  click, a fold and the wheel are the reader's own doing and never scroll anything. Right-clicking the page keeps its
  **Inspect Element** row (already there, now wired to select the hit node and open the pane).
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
- **Alt-`1` … Alt-`9` jump to the N-th tab** (Chrome/Firefox convention): `keymap::shortcut_for`
  reports the digit only (`Shortcut::SelectTab(n)`) and `tab_index_for_digit(n, tab_count)` resolves
  it **at press time** against the live tab list — `Alt-1`…`Alt-8` address those positions while they
  exist, `Alt-9` selects the **last** tab from nine tabs on, a digit with no tab behind it does
  nothing. The mapping is therefore dynamic by construction (no stored per-tab key): closing a tab
  re-slots every remaining shortcut. The inverse, `tab_shortcut_digit(index, tab_count)`, feeds
  `TabInfo.shortcut` so each tab paints the Alt-key that reaches it today (tabs 9+ behind the last
  show none).

Process/tab management (`src/tabs.rs`):
- `struct TabProc { id, container, child: Child, stdin, stdout, title, url, loading, can_back, can_fwd, frame: slint::Image, last_frame_age }`
- Spawn `ironsurf-tab` (resolve sibling of current exe; `--tab-id`…) with piped stdio;
  reader thread per tab → `mpsc::UnboundedSender<FromTab>` drained by Slint timer (UI updates
  via `slint::Weak<AppWindow>` invoke; throttle Frame messages: coalesce to newest).
- `Frame` → `slint::Image` from raw RGBA buffer (`slint::Image::from_rgba8_slab`/PixelBuffer API — verify name).
- Keyboard focus routing: Slint focus on page area → forward keys; Ctrl-L focuses URL bar;
  Esc unfocuses; F12 / Ctrl-Shift-I toggle the DevTools panel (consumed by the shell, never sent
  to the page; they act on the focused page, i.e. not while the URL bar holds focus);
  Ctrl-Shift-K / Ctrl-Shift-Y open the Console / Network panes and **Ctrl-Shift-C** arms
  *inspect element* (opening the panel + Inspector pane if needed, toggling off when armed); the
  same chord with the picker armed disarms it. `Esc` also disarms the picker before it
  unfocuses.
- **Text selection & copy/paste — the shell owns the session clipboard**: copy chords are
  never consumed (Ctrl-C/V/X/A always reach the tab — `keymap::clipboard_chords_always_reach_the_page`),
  every `FromTab::ClipboardText` (page Ctrl+C, editor copy/cut) is filed on the shell's
  session clipboard, and a detected Ctrl+V pushes `ToTab::Clipboard` **before** relaying
  the keystroke (the sandboxed tab cannot read any clipboard itself). The right-click opens
  a pure-Slint menu built from the tab's `ContextTarget` — rows exist exactly when their
  thing is there: Back/Forward/Reload, Open link/image in new tab, **Copy** (the selection),
  Copy link/image/video/page address, Play/Pause video, **Paste** (only with a control that
  can take a caret *and* a non-empty clipboard) — shell-side rows are serviced here, engine-side
  rows return as `ToTab::ContextAction` (Paste = cache push + synthesized Ctrl+V in the tab).
  The clipboard is session-private by design: nothing crosses to the OS clipboard.
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

### Archive browsing server (`src/archive_server.rs`)

The other half of archive mode: the saved tree becomes a **browsable site** on localhost,
served by the shell (never a tab — tabs are sandboxed and the archive folder is the shell's).
`ironsurf://archive` in any tab asks for it; no Slint UI exists for it on purpose — folder,
host and port are edited on the server's own HTML page.

- **Request path**: tab renders `ironsurf://archive` → `FromTab::ArchiveServerStart`
  (`?root=`, `?host=`, `?port=` override for this one request, percent-decoded by the tab,
  `?open=1` = "also open the listing in a new tab") → the shell's pump starts the server
  (lazily, once, reusing the running one) → `ToTab::ArchiveServerInfo`. Without `?open=1` the
  summary tab then sends **itself** to `<address>/ironsurf/serve`; with it the shell opens
  `address` (the archive listing) in a new tab of the same container and the summary stays put.
- **Address**: `Spec { root, host, port }`, defaults `default_archive_root()`
  (= `$HOME/Downloads/ironsurf-tabs`, the archive root's default), `127.0.0.1`, `0` = a random
  free port. What gets bound is what gets reported and persisted — a request for port 0 is
  answered with the port the OS picked. A server that cannot bind is refused **with the
  reason** (port busy, folder refused); no half-dead handle is handed out, and a failed start
  leaves any running server untouched.
- **Threads**: one long-lived **control thread** (stable control page, `/apply`, `/stop`, the
  internal command pipe, `ping`) plus a replaceable **serve thread** per *generation* — only
  a **different address** retires the old listener; an apply that keeps the address (the usual
  change of folder) re-points the live one, since the serving thread reads the folder per
  request. That is also the only way an apply could work: rebinding a port the retired
  listener still holds is `EADDRINUSE`. Retirement is generation-based (a stale thread can
  never kill its successor), and the blocked `accept()` is woken through a loopback self-pipe
  (`libc::poll`, no GL, no extra crate).
- **Serving**: `GET`/`HEAD` only, one connection at a time, HTTP/1.1 with `Connection: close`.
  Directory listings (directories first, then files, `/`-suffixed directory links), `index.html`
  when a directory has one, percent-decoded path segments (**never** `+` → space: archive
  folder names carry ISO8601 offsets like `2026-09-17T07:20:48+10:00`), and MIME from the
  extension — falling back to a sniff of the first bytes (html/xml prologs) when the name says
  nothing, which is the shape an archived query-less document arrives in
  (`…/2026-…+10-00/example.com`). Files above 512 MiB are named in a `413`, never read whole.
- **Traversal is refused, however written** — `..` segments, `%2e%2e`, `%2e`, over-encoded
  forms and symlink escapes all resolve inside the root, and a hostile `..` can only ever mean
  "the root itself" (`resolve_in` is pure and unit-tested; the serve root is canonicalized once).
- **Pages**: `/` is the archive listing (with a link to the control page), **pre-traversed one
  level**: every top-level visit (ISO8601 timestamp) row is followed by the sites saved under
  it, each linked straight as `<visit>/<host[:port]>/`, so the front page *is* the timeline of
  tab visits and no timestamp ever has to be clicked open just to find a saved page. The reach
  is capped (250 visits traversed, 100 entries per visit, with a dim "… more" row),
  symlinked folders are listed but never traversed, an unreadable visit shows a note instead of
  taking the page down, and deeper listings stay flat; `/ironsurf/serve`
  is the control page — the live address, the folder/host/port `GET` form (apply re-points the
  running server, and the form also answers a `POST` with a capped declared body), a stop
  button, and the same folder tree browsable in place. A **blank or absent port field keeps the
  port in use** (the field shows the bound number, so Apply never rolls a new address behind the
  user; typing `0` asks for a fresh random one), and the serve socket's deferral of
  `/apply`&nbsp;/&nbsp;`/stop` **carries the query** to the control socket — dropping it there
  would silently re-apply what was already serving.
  file names, errors, requested paths) is escaped; a browser that sends markup proves it.
- **Lifecycle**: `ServeEvent::{Spec, Address, Failed, Gone, Closed}` on a `std::sync::mpsc`
  channel the shell drains every tick — `Spec` persists the applied folder/host/port into
  `ui.json` (`settings.archive_server`), `Address` broadcasts `ToTab::ArchiveServerInfo` to
  every tab on a change (and announces a stop from the control page), `Closed` lets the shell
  drop its handle. Quitting the shell shuts the server down, releasing the port.

### Developer tools (F12 / Ctrl-Shift-I)

Per tab, a bottom-docked overlay (Console + Network + **Inspector** panes, see the UI bullet
above) fed by the tab's `ConsoleLog`/`NetworkEvent`/`InspectSnapshot` stream: Console shows JS/CSS/DOM entries (`[source]
message`, level icon + colour); Network shows Method | URL | Initiator | Status | Size |
Time per request (the initiator names what triggered it: document / script / resource /
xhr / fetch), failures in red with their error text. Screenshots: `docs/screenshots/devtools.png`
(Console) and `devtools-network.png`, reproducible with `scripts/demo.sh` (launches the real
browser on `$DISPLAY` against a localhost fixture and drives it through synthetic xdotool
input). Known limitation inherited from the tab: parse errors inside `<link>`-fetched
stylesheets never reach the console (see §ironsurf-tab DevTools CSS seam).

**Inspector — how the two sides split it.** The **tab** owns the document: it walks the DOM
(`DocumentEnd`/root down, same node-visit rules as the `--body-out` serializer, skipping
anonymous blocks) into the flat [`InspectSnapshot`], stamps each element's
[`InspectBox`] from the *live* layout (the same resolve + `absolute_position` − `viewport_scroll`
path the find highlights use, in CSS px, so a scroll/resize moves the highlight without a new
snapshot), hit-tests ⌖ picks with the engine's own hit-testing (`BaseDocument::element_from_point`,
the same point query `document.elementFromPoint()` answers with — anonymous boxes and text resolve
to their nearest element, a point over the background resolves to the root), paints the
selected/hovered box **over the page** in the same pass as the find highlights (content-box fill
`rgba(255,147,38,110)`, 2px outline `rgba(255,213,0,220)`, never a repaint loop — the box is
re-measured every frame it is shown), and swallows pick-mode pointers. The **shell** owns the
presentation: tree folding, selection, breadcrumb, the `+/−` twisties and the syntax colouring
(it re-renders `<tag attr="value">` from `tag`/`attrs` — the tab never sends markup text).
Because blitz node ids are recycled, the tab's snapshot ids are positional handles; the shell
treats an unknown id as "tree is stale" and waits for the next snapshot rather than guessing.
Headless CI reaches the same data without a shell: `ironsurf-tab --headless --inspect-out FILE`
writes the document's snapshot as one indented [`InspectSnapshot`] (`null` in the file is the tab
saying it had no document to walk, never a plausible-looking empty one), and
`--inspect-pick X,Y [--inspect-select ID]` runs the same commands a pane drives before the dump.

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
