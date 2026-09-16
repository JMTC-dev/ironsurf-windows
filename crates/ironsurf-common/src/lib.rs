//! ironsurf-common — shared types and the IPC codec for IronSurf.
//!
//! Framing: `u32` LE length prefix + bincode payload, over stdin/stdout pipes.
//! See `/workspace/ARCHITECTURE.md` §ironsurf-common (single source of truth).

pub mod logging;

use serde::{Deserialize, Serialize};

/// Identifier of a container (isolated session).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ContainerId(pub u32);

/// Identifier of a tab (one OS process per tab).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct TabId(pub u32);

/// Both ids print as bare numbers, which is what log lines want (`tab 3`).
impl std::fmt::Display for TabId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::fmt::Display for ContainerId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A named, colored, isolated browsing session.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Container {
    pub id: ContainerId,
    pub name: String,
    /// `"#rrggbb"` hex, used for the tab stripe.
    pub color: String,
    pub is_default: bool,
}

/// One tab-to-shell message.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum FromTab {
    Ready {
        tab_id: TabId,
    },
    /// RGBA8, straight alpha, top-down; `rgba.len() == width * height * 4`.
    Frame {
        width: u32,
        height: u32,
        scale: f32,
        rgba: Vec<u8>,
    },
    TitleChanged {
        title: Option<String>,
    },
    UrlChanged {
        url: Option<String>,
    },
    LoadingChanged {
        loading: bool,
    },
    HistoryChanged {
        can_go_back: bool,
        can_go_forward: bool,
    },
    StatusText {
        text: Option<String>,
    },
    /// Middle-click on a link: the shell must open `url` in a **new tab**
    /// (same container as the requesting tab) without navigating this tab.
    OpenLinkInNewTab {
        url: String,
    },
    /// Right-click on the page: the tab has hit-tested the element under the
    /// cursor ([`ContextTarget`]); the shell opens the context menu at
    /// (`x`, `y`) — logical px in the page viewport, the same space as
    /// `ToTab::Resize`. Menu choices come back as `ToTab::ContextAction`
    /// (engine-side actions) or are handled by the shell itself (open in
    /// new tab, clipboard, navigation).
    ContextMenu {
        x: f32,
        y: f32,
        target: ContextTarget,
    },
    /// The page copied text (Ctrl+C / Ctrl+X / editor cut — surfaced through
    /// blitz's `ShellProvider`): the shell stores it in the session clipboard.
    ClipboardText {
        text: String,
    },
    /// DevTools console entry. `source` names the subsystem the entry came
    /// from: `"js"` (script `console.*` / engine JS diagnostics), `"css"`
    /// (stylesheet parse/style problems), `"dom"` (markup/DOM diagnostics)
    /// or `"page"` (anything else about the document itself).
    ConsoleLog {
        /// `"js"` | `"css"` | `"dom"` | `"page"`.
        source: String,
        level: String,
        message: String,
    },
    /// DevTools network entry: one request the tab made for the page
    /// (document, subresource, XHR, media…). Emitted once per request when
    /// it finishes (or fails); `NetEvent::error` carries the failure.
    NetworkEvent {
        event: NetEvent,
    },
    /// Page JavaScript opened a modal dialog (`alert` / `confirm` / `prompt`).
    /// The tab **blocks** on the poll loop until the shell answers with
    /// [`ToTab::DialogResponse`] carrying the same `dialog_id` (sync JS
    /// dialogs, like every real browser); every other `ToTab` message that
    /// arrives meanwhile is deferred, in order, until the dialog settles.
    /// `default` is the `prompt` prefill (always `None` for alert/confirm).
    DialogRequest {
        dialog_id: u32,
        kind: DialogKind,
        /// Dialog body text (bounded by [`MAX_DIALOG_MESSAGE`]).
        message: String,
        /// `prompt` default value; `None` for alert/confirm.
        default: Option<String>,
    },
    /// The pointer shape for what the mouse is over **in the page viewport**
    /// (task: hover cursor). Emitted whenever the shape *changes* (the tab
    /// change-detects, like `StatusText`): CSS `cursor` on the hovered
    /// element first (blitz resolves it), then the engine's clickability
    /// heuristics — links, buttons, clickable form controls, a video's
    /// play/pause area. `CursorIcon::Default` means the plain arrow.
    CursorChanged {
        cursor: CursorIcon,
    },
    Closed,
    Error {
        message: String,
    },
}

/// One shell-to-tab message.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ToTab {
    Navigate {
        url: String,
    },
    Reload,
    /// v1: no-op placeholder.
    Stop,
    Back,
    Forward,
    Resize {
        width: u32,
        height: u32,
        scale: f32,
    },
    /// Page zoom factor (`1.0` = 100%). The shell owns the chord steps and
    /// clamping (Ctrl-`+` / Ctrl-`-` / Ctrl-`0`); the tab applies the factor
    /// to the document viewport and maps pointer/scroll coordinates through
    /// it. The factor persists across navigations for the tab's lifetime.
    SetZoom {
        zoom: f32,
    },
    /// Throttle background rendering.
    SetVisible {
        visible: bool,
    },
    Pointer(PointerEvent),
    Key(KeyEvent),
    Scroll(ScrollEvent),
    /// Replace the tab-side clipboard cache the editors' Ctrl+V reads (blitz
    /// asks its `ShellProvider`, which answers from this cache). The tab
    /// process cannot reach the session clipboard itself; the shell pushes the
    /// current contents just before relaying a Ctrl+V keystroke.
    Clipboard {
        text: String,
    },
    /// The user picked an entry in a context menu that the *engine* services
    /// (paste, video play/pause); shell-side entries never reach the tab.
    ContextAction(ContextAction),
    /// Replace the tab-side userscript set. The shell sends **all enabled**
    /// scripts (patterns included); the **tab** filters them against the
    /// document URL on every navigation (see [`userscripts_for_url`]), so a
    /// script still applies after JS-driven redirects. Disabled scripts are
    /// never sent; an empty vec clears the tab-side set entirely.
    SetUserscripts {
        scripts: Vec<Userscript>,
    },
    /// Answer a [`FromTab::DialogRequest`] (matched by `dialog_id`):
    /// `confirmed` is the button the user pressed (alert: always true once
    /// dismissed; confirm: the answer JS returns; prompt: OK vs Cancel).
    /// `text` is the edited prompt value — `Some` exactly when a **prompt**
    /// was accepted (an empty box is `Some("")`).
    DialogResponse {
        dialog_id: u32,
        confirmed: bool,
        text: Option<String>,
    },
    Destroy,
}

/// Which of the three synchronous window dialogs the page asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DialogKind {
    /// `window.alert(msg)` — one OK, returns `undefined`.
    Alert,
    /// `window.confirm(msg)` — OK/Cancel, returns the boolean.
    Confirm,
    /// `window.prompt(msg, def)` — text box + OK/Cancel, returns the text or
    /// `null`.
    Prompt,
}

/// Cap for the text carried in a dialog message (the page hands over
/// whatever string it likes; the dialog is UI, not an exfil channel).
pub const MAX_DIALOG_MESSAGE: usize = 64 * 1024;

/// What the cursor was over when the user right-clicked. Every field is
/// "under the cursor at right-click time"; the shell decides from these
/// which menu entries exist (Firefox-style: link/image/video/selection
/// entries only when the thing is there).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ContextTarget {
    /// Nearest `<a href>`, resolved against the document URL.
    pub link: Option<String>,
    /// `<img src>` (resolved), if over an image.
    pub image: Option<String>,
    /// `<video src>` (resolved, may be empty if the element has no `src`),
    /// if over a video.
    pub video: Option<String>,
    /// Whether that `<video>` is currently playing (`Pause video` vs `Play video`).
    pub video_playing: bool,
    /// Selected page text (bounded by [`MAX_CONTEXT_TEXT`]), if any.
    pub selection: Option<String>,
    /// Whether a text control (the one under the cursor or the focused one)
    /// can take a paste.
    pub editable: bool,
}

/// Context-menu entries the tab process services (engine state lives there:
/// the editor caret, the media players).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ContextAction {
    /// Insert `text` at the focused text control's caret (the tab feeds the
    /// clipboard cache to blitz and synthesizes Ctrl+V).
    Paste {
        text: String,
    },
    /// Play/pause the `<video>` that was under the cursor at right-click
    /// (the tab keeps that element's player around until the next click).
    ToggleVideo,
}

/// Cap for text carried in context-menu messages (`ContextTarget::selection`)
/// — the selection is a convenience copy path, not an exfil channel.
pub const MAX_CONTEXT_TEXT: usize = 256 * 1024;

/// A user-defined JavaScript program that runs on matching pages
/// (Tampermonkey/Greasemonkey-style). The shell owns the script set and
/// ships it wholesale via [`ToTab::SetUserscripts`]; the tab decides, per
/// navigation, which scripts apply to the current document URL
/// (see [`userscripts_for_url`]).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Userscript {
    /// Stable id chosen by the shell (manager-UI bookkeeping / dedup).
    pub id: u64,
    /// Human-readable name shown in the manager UI (may be unicode).
    pub name: String,
    /// URL patterns this script runs on — exact or wildcard, matched by
    /// [`userscript_matches`]. An empty pattern list matches **nothing**.
    pub patterns: Vec<String>,
    /// The JavaScript source, verbatim.
    pub code: String,
    /// Disabled scripts are never sent to the tab
    /// (see [`ToTab::SetUserscripts`]).
    pub enabled: bool,
}

/// One network request observed by the tab, reported to the shell as
/// [`FromTab::NetworkEvent`] (DevTools network panel).
///
/// A request that *completed* carries `status` (and `response_bytes` /
/// `duration_ms` when the engine measured them) and `error == None`. A
/// request that *failed* (DNS, TLS, connection refused, blocked by policy,
/// …) has `status == None` and `error == Some(reason)` — never both.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NetEvent {
    /// HTTP method (`"GET"`, `"POST"`, …), upper-cased by the engine.
    pub method: String,
    /// Request URL after the engine resolved it (redirects are separate
    /// events, one per hop).
    pub url: String,
    /// Response status code; `None` when no response arrived (see `error`).
    pub status: Option<u16>,
    /// Response body size in bytes, when known.
    pub response_bytes: Option<u64>,
    /// Wall-clock duration of the request in milliseconds, when measured.
    pub duration_ms: Option<u64>,
    /// Failure reason; `Some` exactly when the request failed (`status` is
    /// then `None`).
    pub error: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct PointerEvent {
    pub x: f32,
    pub y: f32,
    pub kind: PointerKind,
    pub button: MouseButton,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PointerKind {
    Move,
    Down,
    Up,
    Over,
    Out,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
}

/// A cursor (pointer) shape, as reported by the tab for what is under the
/// mouse ([`FromTab::CursorChanged`]). Mirrors the subset every layer of the
/// stack agrees on: the CSS `cursor` keywords blitz resolves, the
/// `cursor-icon` crate's vocabulary it reports them in, and the shapes
/// Slint's `MouseCursor` can actually draw (the names follow CSS).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CursorIcon {
    /// The platform's plain arrow.
    #[default]
    Default,
    /// `cursor: none` — the pointer is hidden.
    Hidden,
    Help,
    /// The pointing hand: the thing under the cursor is clickable.
    Pointer,
    Progress,
    Wait,
    Crosshair,
    /// The I-beam: text (or a text control) is under the cursor.
    Text,
    VerticalText,
    Alias,
    Copy,
    Move,
    NoDrop,
    NotAllowed,
    Grab,
    Grabbing,
    ColResize,
    RowResize,
    NResize,
    EResize,
    SResize,
    WResize,
    NeResize,
    NwResize,
    SeResize,
    SwResize,
    EwResize,
    NsResize,
    NeswResize,
    NwseResize,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct KeyEvent {
    /// DOM `key`.
    pub key: String,
    /// DOM `code`.
    pub code: String,
    pub text: Option<String>,
    pub repeat: bool,
    pub mods: Modifiers,
}

/// Wheel/trackpad scroll over the page viewport.
///
/// Sign convention (winit/AccessKit, which both ends of the pipe speak): a
/// POSITIVE `delta_y` scrolls UP (reveals content above; `scrollTop` shrinks),
/// a negative one scrolls DOWN. Same for `delta_x` and LEFT. The shell passes
/// the toolkit's deltas through untouched — do not negate them on either side.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct ScrollEvent {
    /// Pointer position (logical px, same space as `ToTab::Resize`).
    pub x: f32,
    pub y: f32,
    pub delta_x: f32,
    pub delta_y: f32,
}

/// Keyboard modifier state as a bitflags-style `u8` newtype.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Modifiers(pub u8);

impl Modifiers {
    pub const NONE: Modifiers = Modifiers(0);
    pub const CTRL_BIT: u8 = 1 << 0;
    pub const SHIFT_BIT: u8 = 1 << 1;
    pub const ALT_BIT: u8 = 1 << 2;
    pub const META_BIT: u8 = 1 << 3;

    #[inline]
    pub const fn new(bits: u8) -> Self {
        Modifiers(bits)
    }

    #[inline]
    pub const fn bits(self) -> u8 {
        self.0
    }

    #[inline]
    pub const fn empty(self) -> bool {
        self.0 == 0
    }

    #[inline]
    pub const fn ctrl(self) -> bool {
        self.0 & Self::CTRL_BIT != 0
    }

    #[inline]
    pub const fn shift(self) -> bool {
        self.0 & Self::SHIFT_BIT != 0
    }

    #[inline]
    pub const fn alt(self) -> bool {
        self.0 & Self::ALT_BIT != 0
    }

    #[inline]
    pub const fn meta(self) -> bool {
        self.0 & Self::META_BIT != 0
    }

    #[inline]
    pub const fn with_ctrl(mut self) -> Self {
        self.0 |= Self::CTRL_BIT;
        self
    }

    #[inline]
    pub const fn with_shift(mut self) -> Self {
        self.0 |= Self::SHIFT_BIT;
        self
    }

    #[inline]
    pub const fn with_alt(mut self) -> Self {
        self.0 |= Self::ALT_BIT;
        self
    }

    #[inline]
    pub const fn with_meta(mut self) -> Self {
        self.0 |= Self::META_BIT;
        self
    }

    #[inline]
    pub const fn union(self, other: Self) -> Self {
        Modifiers(self.0 | other.0)
    }
}

impl std::ops::BitOr for Modifiers {
    type Output = Modifiers;
    #[inline]
    fn bitor(self, rhs: Self) -> Self::Output {
        self.union(rhs)
    }
}

/// Guard against corrupt streams (frames larger than this are rejected).
pub const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;

/// The browser's built-in start page. `ironsurf:` is an internal scheme:
/// every `ironsurf://…` URL is answered locally by the tab process (see
/// `ironsurf_tab::netmod::internal_page_bytes`) and never touches the network.
pub const WELCOME_URL: &str = "ironsurf://welcome";

/// Default start page URL (the built-in `ironsurf://welcome` page).
pub fn default_url() -> String {
    WELCOME_URL.to_string()
}

/// Split a lowercased URL into `(scheme, host)`. The host is `None` when the
/// URL carries no `://` authority (`data:…`, `about:…`, plain `ironsurf://`
/// with an empty authority, or anything without a scheme). Credentials and
/// `:port` are stripped; IPv6 literal hosts are not special-cased (contract-
/// level matching only).
fn scheme_and_host(url: &str) -> (Option<&str>, Option<&str>) {
    let idx = match url.find("://") {
        Some(i) if i > 0 => i,
        _ => return (None, None),
    };
    let scheme = &url[..idx];
    let rest = &url[idx + 3..];
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = authority.rsplit_once('@').map(|(_, h)| h).unwrap_or(authority);
    let host = host.split(':').next().unwrap_or("");
    (Some(scheme), if host.is_empty() { None } else { Some(host) })
}

/// Whether the URL speaks `http:` or `https:` (the only schemes a wildcard
/// domain pattern may ever match).
fn is_http_like(scheme: Option<&str>) -> bool {
    matches!(scheme, Some("http") | Some("https"))
}

/// Match one userscript URL pattern against a document URL.
///
/// Comparison is fully case-insensitive: **both** strings are lowercased
/// first, so paths compare case-insensitively too (documented simplification).
///
/// * A blank (empty/whitespace) pattern matches nothing; skip blanks.
/// * The bare pattern `"*"` matches every URL (any scheme).
/// * A pattern **not** starting with `*` is compared to the **full URL
///   string** exactly (after lowercasing) — this is how non-http(s) URLs
///   like `ironsurf://settings/plugins` can still be targeted literally.
///   Additionally, a pattern with no `*` and no `://` (e.g.
///   `example.com/docs`) is `*://example.com/docs*` **sugar**: it matches
///   http(s) URLs whose host *equals* the pattern's host part and whose
///   path starts with the pattern's path (a pattern with no path — just a
///   bare host — matches any path on that exact host).
/// * A pattern starting with `*` (but not the bare `"*"`) strips the leading
///   `*` into remainder `R`, split at the first `/` into a **domain part**
///   and a **path part**. The URL must be http(s). Host boundary rule: the
///   URL's host matches iff it equals the domain (after stripping an
///   optional leading `*.` / `.`) or *ends with* `.{domain}` — a lookalike
///   like `notexample.com` NEVER matches `example.com`. An empty domain part
///   (`*/api`) means any host. If R has a path part, the lowercased URL must
///   `ends_with` that path string verbatim (no percent-decoding).
///
/// So `*example.com` matches `https://example.com/` and
/// `http://x.example.com/page` but NOT `https://notexample.com/`, and
/// `*example.com/api` matches URLs on that host boundary whose URL ends in
/// `/api`. Non-http(s) schemes (`data:`, `ironsurf:`, `about:`) never match
/// wildcard domain patterns; exact patterns can still match them literally.
pub fn userscript_matches(pattern: &str, url: &str) -> bool {
    let pattern = pattern.trim().to_ascii_lowercase();
    let url = url.trim().to_ascii_lowercase();
    if pattern.is_empty() {
        return false;
    }
    if pattern == "*" {
        return true;
    }
    if let Some(domain) = pattern.strip_prefix('*') {
        let (domain, path) = match domain.split_once('/') {
            Some((d, p)) => (d, Some(p)),
            None => (domain, None),
        };
        let domain = domain.strip_prefix("*.").or_else(|| domain.strip_prefix('.')).unwrap_or(domain);
        let (scheme, host) = scheme_and_host(&url);
        if !is_http_like(scheme) {
            return false;
        }
        let host = match host {
            Some(h) => h,
            None => return false,
        };
        let host_ok =
            domain.is_empty() || host == domain || host.ends_with(&format!(".{domain}"));
        host_ok && path.map(|p| url.ends_with(p)).unwrap_or(true)
    } else {
        if url == pattern {
            return true;
        }
        // `example.com/docs` sugar = `*://example.com/docs*` (host+prefix).
        if !pattern.contains("://") {
            let (pat_host, pat_path) = match pattern.split_once('/') {
                Some((h, p)) => (h, p),
                None => (pattern.as_str(), ""),
            };
            let (scheme, host) = scheme_and_host(&url);
            if is_http_like(scheme) && host == Some(pat_host) {
                let url_path = url
                    .split_once("://")
                    .map(|(_, r)| {
                        let after_auth =
                            r.split_once('/').map(|(_, p)| p).unwrap_or("");
                        after_auth.split(['?', '#']).next().unwrap_or("")
                    })
                    .unwrap_or("");
                return url_path.starts_with(pat_path);
            }
        }
        false
    }
}

/// The scripts that apply to `url`: enabled scripts whose **any** pattern
/// matches (see [`userscript_matches`]). Scripts with an empty/whitespace
/// pattern list match nothing and are never returned.
pub fn userscripts_for_url<'a>(scripts: &'a [Userscript], url: &str) -> Vec<&'a Userscript> {
    scripts
        .iter()
        .filter(|s| s.enabled && s.patterns.iter().any(|p| userscript_matches(p, url)))
        .collect()
}

fn invalid_data<E: std::fmt::Display>(e: E) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string())
}

/// Serialize `msg` with bincode and write it as
/// `u32 LE length prefix + payload`. Flushes the writer.
pub fn write_frame<W: std::io::Write, T: serde::Serialize>(
    w: &mut W,
    msg: &T,
) -> std::io::Result<()> {
    let payload = bincode::serialize(msg).map_err(invalid_data)?;
    if payload.len() > MAX_FRAME_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "frame too large: {} bytes (max {})",
                payload.len(),
                MAX_FRAME_BYTES
            ),
        ));
    }
    w.write_all(&(payload.len() as u32).to_le_bytes())?;
    w.write_all(&payload)?;
    w.flush()
}

/// Read one length-prefixed bincode frame.
pub fn read_frame<R: std::io::Read, T: serde::de::DeserializeOwned>(
    r: &mut R,
) -> std::io::Result<T> {
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf)?;
    let len = u32::from_le_bytes(len_buf) as usize;
    if len > MAX_FRAME_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("frame too large: {len} bytes (max {MAX_FRAME_BYTES})"),
        ));
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    bincode::deserialize(&buf).map_err(invalid_data)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key_event() -> KeyEvent {
        KeyEvent {
            key: "a".into(),
            code: "KeyA".into(),
            text: Some("a".into()),
            repeat: false,
            mods: Modifiers::NONE.with_ctrl(),
        }
    }

    fn pointer_event() -> PointerEvent {
        PointerEvent {
            x: 1.5,
            y: -2.25,
            kind: PointerKind::Down,
            button: MouseButton::Left,
        }
    }

    fn scroll_event() -> ScrollEvent {
        ScrollEvent {
            x: 10.0,
            y: 20.0,
            delta_x: 0.0,
            delta_y: -3.0,
        }
    }

    fn frame_round_trip<T>(msg: T) -> T
    where
        T: serde::Serialize + serde::de::DeserializeOwned + std::fmt::Debug + PartialEq,
    {
        let mut buf = Vec::new();
        write_frame(&mut buf, &msg).expect("write_frame");
        let got: T = read_frame(&mut buf.as_slice()).expect("read_frame");
        assert_eq!(got, msg);
        got
    }

    #[test]
    fn ids_and_container_round_trip() {
        frame_round_trip(ContainerId(7));
        frame_round_trip(TabId(42));
        frame_round_trip(Container {
            id: ContainerId(1),
            name: "personal".into(),
            color: "#ff5500".into(),
            is_default: true,
        });
        assert_eq!(ContainerId(1), ContainerId(1));
        assert_ne!(ContainerId(1), ContainerId(2));
    }

    #[test]
    fn from_tab_all_variants_round_trip() {
        frame_round_trip(FromTab::Ready { tab_id: TabId(3) });
        frame_round_trip(FromTab::Frame {
            width: 2,
            height: 2,
            scale: 1.0,
            rgba: vec![0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15],
        });
        frame_round_trip(FromTab::TitleChanged { title: Some("t".into()) });
        frame_round_trip(FromTab::TitleChanged { title: None });
        frame_round_trip(FromTab::UrlChanged { url: Some("https://example.com".into()) });
        frame_round_trip(FromTab::UrlChanged { url: None });
        frame_round_trip(FromTab::LoadingChanged { loading: true });
        frame_round_trip(FromTab::LoadingChanged { loading: false });
        frame_round_trip(FromTab::HistoryChanged { can_go_back: true, can_go_forward: false });
        frame_round_trip(FromTab::StatusText { text: Some("s".into()) });
        frame_round_trip(FromTab::StatusText { text: None });
        frame_round_trip(FromTab::OpenLinkInNewTab {
            url: "https://example.com/link".into()
        });
        frame_round_trip(FromTab::ConsoleLog {
            source: "js".into(),
            level: "warn".into(),
            message: "m".into(),
        });
        frame_round_trip(FromTab::NetworkEvent {
            event: NetEvent {
                method: "GET".into(),
                url: "https://example.com/".into(),
                status: Some(301),
                response_bytes: Some(0),
                duration_ms: Some(7),
                error: None,
            },
        });
        frame_round_trip(FromTab::CursorChanged { cursor: CursorIcon::Default });
        frame_round_trip(FromTab::CursorChanged { cursor: CursorIcon::Pointer });
        frame_round_trip(FromTab::Closed);
        frame_round_trip(FromTab::Error { message: "boom".into() });
        frame_round_trip(FromTab::ContextMenu {
            x: 120.5,
            y: 64.25,
            target: ContextTarget {
                link: Some("https://example.com/a".into()),
                image: Some("https://example.com/i.png".into()),
                video: Some("https://example.com/v.mp4".into()),
                video_playing: true,
                selection: Some("selected words".into()),
                editable: true,
            },
        });
        frame_round_trip(FromTab::ContextMenu {
            x: 0.0,
            y: 0.0,
            target: ContextTarget::default(),
        });
        frame_round_trip(FromTab::ClipboardText { text: "copied".into() });
        frame_round_trip(FromTab::DialogRequest {
            dialog_id: 1,
            kind: DialogKind::Alert,
            message: "Page says hi".into(),
            default: None,
        });
    }

    /// Every dialog kind survives the codec with its id/message/default
    /// intact (the prompt default travels as `Some`, alert/confirm as `None`).
    #[test]
    fn dialog_requests_round_trip() {
        frame_round_trip(FromTab::DialogRequest {
            dialog_id: 0,
            kind: DialogKind::Alert,
            message: "".into(),
            default: None,
        });
        frame_round_trip(FromTab::DialogRequest {
            dialog_id: 7,
            kind: DialogKind::Confirm,
            message: "Leave?".into(),
            default: None,
        });
        let msg = frame_round_trip(FromTab::DialogRequest {
            dialog_id: 42,
            kind: DialogKind::Prompt,
            message: "Your name:".into(),
            default: Some("Ada".into()),
        });
        match msg {
            FromTab::DialogRequest { dialog_id, kind, message, default } => {
                assert_eq!(dialog_id, 42);
                assert_eq!(kind, DialogKind::Prompt);
                assert_eq!(message, "Your name:");
                assert_eq!(default.as_deref(), Some("Ada"));
            }
            other => panic!("unexpected message: {other:?}"),
        }
    }

    /// `source` is one of the four documented DevTools origins; every one of
    /// them must survive the frame codec with `level`/`message` intact.
    #[test]
    fn console_log_source_round_trips() {
        for source in ["js", "css", "dom", "page"] {
            let msg = frame_round_trip(FromTab::ConsoleLog {
                source: source.into(),
                level: "error".into(),
                message: "unclosed string".into(),
            });
            match msg {
                FromTab::ConsoleLog { source: s, level, message } => {
                    assert_eq!(s, source);
                    assert_eq!(level, "error");
                    assert_eq!(message, "unclosed string");
                }
                other => panic!("unexpected message: {other:?}"),
            }
        }
    }

    #[test]
    fn network_event_round_trips() {
        // A completed request: status + sizes, no error.
        frame_round_trip(FromTab::NetworkEvent {
            event: NetEvent {
                method: "GET".into(),
                url: "https://example.com/style.css".into(),
                status: Some(200),
                response_bytes: Some(4096),
                duration_ms: Some(42),
                error: None,
            },
        });
        // A failed request: no status, an error instead.
        frame_round_trip(FromTab::NetworkEvent {
            event: NetEvent {
                method: "POST".into(),
                url: "https://example.com/api".into(),
                status: None,
                response_bytes: None,
                duration_ms: None,
                error: Some("connection refused".into()),
            },
        });
        // A bare minimum event (only method + url known).
        frame_round_trip(FromTab::NetworkEvent {
            event: NetEvent {
                method: "HEAD".into(),
                url: "ironsurf://welcome".into(),
                status: None,
                response_bytes: None,
                duration_ms: None,
                error: None,
            },
        });
    }

    #[test]
    fn to_tab_all_variants_round_trip() {
        frame_round_trip(ToTab::Navigate { url: default_url() });
        frame_round_trip(ToTab::Reload);
        frame_round_trip(ToTab::Stop);
        frame_round_trip(ToTab::Back);
        frame_round_trip(ToTab::Forward);
        frame_round_trip(ToTab::Resize { width: 800, height: 600, scale: 2.0 });
        frame_round_trip(ToTab::SetZoom { zoom: 1.0 });
        frame_round_trip(ToTab::SetZoom { zoom: 1.33 });
        frame_round_trip(ToTab::SetZoom { zoom: 0.5 });
        frame_round_trip(ToTab::SetVisible { visible: false });
        for kind in [
            PointerKind::Move,
            PointerKind::Down,
            PointerKind::Up,
            PointerKind::Over,
            PointerKind::Out,
        ] {
            for button in [MouseButton::Left, MouseButton::Right, MouseButton::Middle] {
                frame_round_trip(ToTab::Pointer(PointerEvent {
                    kind,
                    button,
                    ..pointer_event()
                }));
            }
        }
        frame_round_trip(ToTab::Key(key_event()));
        frame_round_trip(ToTab::Scroll(scroll_event()));
        frame_round_trip(ToTab::Clipboard { text: "clip".into() });
        frame_round_trip(ToTab::ContextAction(ContextAction::Paste { text: "hi".into() }));
        frame_round_trip(ToTab::ContextAction(ContextAction::ToggleVideo));
        frame_round_trip(ToTab::DialogResponse { dialog_id: 1, confirmed: true, text: None });
        frame_round_trip(ToTab::DialogResponse {
            dialog_id: 2,
            confirmed: false,
            text: None,
        });
        frame_round_trip(ToTab::DialogResponse {
            dialog_id: 3,
            confirmed: true,
            text: Some("Ada Lovelace".into()),
        });
        // An accepted empty prompt box is Some(""), not None.
        frame_round_trip(ToTab::DialogResponse {
            dialog_id: 4,
            confirmed: true,
            text: Some(String::new()),
        });
        frame_round_trip(ToTab::Destroy);
    }

    /// The userscript contract rides the wire intact: empty set (clear),
    /// one script with several patterns, and a unicode name.
    #[test]
    fn set_userscripts_round_trips() {
        frame_round_trip(ToTab::SetUserscripts { scripts: vec![] });
        frame_round_trip(ToTab::SetUserscripts {
            scripts: vec![Userscript {
                id: 1,
                name: "Darkify".into(),
                patterns: vec!["*example.com".into(), "example.org/*".into()],
                code: "console.log('hi')".into(),
                enabled: true,
            }],
        });
        let msg = frame_round_trip(ToTab::SetUserscripts {
            scripts: vec![
                Userscript {
                    id: 7,
                    name: "🔧 Т工具 — ünïcode Ñame 日".into(),
                    patterns: vec![],
                    code: "".into(),
                    enabled: false,
                },
                Userscript {
                    id: 8,
                    name: "second".into(),
                    patterns: vec!["*".into()],
                    code: "var x = 1;".into(),
                    enabled: true,
                },
            ],
        });
        match msg {
            ToTab::SetUserscripts { scripts } => {
                assert_eq!(scripts.len(), 2);
                assert_eq!(scripts[0].name, "🔧 Т工具 — ünïcode Ñame 日");
                assert!(!scripts[0].enabled);
                assert_eq!(scripts[1].patterns, vec!["*".to_string()]);
            }
            other => panic!("unexpected message: {other:?}"),
        }
    }

    /// Table-driven coverage of the documented URL-pattern semantics: exact
    /// match ±case, wildcard host boundary (never a lookalike), wildcard +
    /// path, bare `*`, no-scheme sugar, blank patterns, internal schemes.
    #[test]
    fn userscript_url_matching() {
        type Case = (&'static str, &'static str, bool);
        let cases: &[Case] = &[
            // Exact patterns: full-URL string equality, case-insensitive.
            ("https://example.com/x", "https://example.com/x", true),
            ("https://example.com/x", "https://example.com/y", false),
            ("HTTPS://Example.COM/X", "https://example.com/x", true),
            ("https://example.com/x", "https://example.com/x?q=1", false),
            // Bare `*` matches every URL (any scheme).
            ("*", "https://example.com/", true),
            ("*", "data:text/html,hi", true),
            ("*", "ironsurf://welcome", true),
            // Wildcard domain boundary: exact host and subdomains match.
            ("*example.com", "https://example.com/", true),
            ("*example.com", "http://x.example.com/page", true),
            ("*example.com", "https://a.b.example.com/deep/path?q=1", true),
            ("*example.com", "https://EXAMPLE.COM/Upper/Path", true),
            // …a lookalike suffix NEVER matches (host boundary rule).
            ("*example.com", "https://notexample.com/", false),
            ("*example.com", "https://evil.com/?u=example.com", false),
            ("*example.com", "https://xexample.com/", false),
            // Leading `*.` / `.` on the domain behave like `*example.com`.
            ("*.example.com", "https://example.com/", true),
            ("*.example.com", "https://sub.example.com/a", true),
            ("*.example.com", "https://notexample.com/", false),
            ("*sub.example.com", "https://example.com/", false),
            // Wildcard + path: the lowercased URL must end with the path part.
            ("*example.com/api", "https://example.com/api", true),
            ("*example.com/api", "http://sub.example.com/v1/api", true),
            ("*example.com/api", "https://example.com/api/extra", false),
            ("*example.com/api", "https://notexample.com/api", false),
            ("*example.com/", "https://example.com/anything", true),
            // No-scheme sugar: `host` = that exact host, any path; `host/path`
            // = path prefix.
            ("example.com", "https://example.com/", true),
            ("example.com", "https://example.com/deep/path?x=1", true),
            ("example.com", "https://x.example.com/", false),
            ("example.com", "https://notexample.com/", false),
            ("example.com/docs", "https://example.com/docs", true),
            ("example.com/docs", "https://example.com/docs/intro", true),
            ("example.com/docs", "https://example.com/other", false),
            ("example.com/docs", "http://example.com/docs", true),
            ("example.com/docs", "https://notexample.com/docs", false),
            // Non-http(s) schemes never match sugar or wildcard domains…
            ("example.com", "ironsurf://example.com/docs", false),
            ("*example.com", "ironsurf://example.com/", false),
            ("*example.com", "data:text/html,example.com", false),
            // …but exact patterns match internal schemes literally.
            ("ironsurf://welcome", "ironsurf://welcome", true),
            ("ironsurf://welcome", "ironsurf://settings", false),
            ("ironsurf://settings/plugins", "ironsurf://settings/plugins", true),
            // Blank patterns match nothing.
            ("", "https://example.com/", false),
            ("   ", "https://example.com/", false),
        ];
        for (pattern, url, want) in cases {
            assert_eq!(
                userscript_matches(pattern, url),
                *want,
                "pattern {pattern:?} vs url {url:?} (expected {want})"
            );
        }
    }

    /// `userscripts_for_url` returns enabled scripts with a matching pattern
    /// only — disabled scripts, non-matching patterns, and empty/whitespace
    /// pattern lists are all excluded.
    #[test]
    fn userscripts_for_url_filters_enabled_and_patterns() {
        let scripts = vec![
            Userscript {
                id: 1,
                name: "on-example".into(),
                patterns: vec!["*example.com".into()],
                code: "1".into(),
                enabled: true,
            },
            Userscript {
                id: 2,
                name: "disabled".into(),
                patterns: vec!["*".into()],
                code: "2".into(),
                enabled: false,
            },
            Userscript {
                id: 3,
                name: "no-patterns".into(),
                patterns: vec![],
                code: "3".into(),
                enabled: true,
            },
            Userscript {
                id: 4,
                name: "blank-patterns".into(),
                patterns: vec!["".into(), "  ".into()],
                code: "4".into(),
                enabled: true,
            },
            Userscript {
                id: 5,
                name: "other-host".into(),
                patterns: vec!["example.org".into()],
                code: "5".into(),
                enabled: true,
            },
            Userscript {
                id: 6,
                name: "multi-pattern".into(),
                patterns: vec!["example.org".into(), "  ".into(), "*docs.example.net/guide".into()],
                code: "6".into(),
                enabled: true,
            },
        ];

        let on_example = userscripts_for_url(&scripts, "https://example.com/page");
        let ids: Vec<u64> = on_example.iter().map(|s| s.id).collect();
        assert_eq!(ids, vec![1], "only the enabled matching script");

        let on_docs = userscripts_for_url(&scripts, "https://docs.example.net/guide");
        assert_eq!(
            on_docs.iter().map(|s| s.id).collect::<Vec<_>>(),
            vec![6],
            "a later pattern in the list matches"
        );

        assert!(userscripts_for_url(&scripts, "https://ironsurf.internal/x").is_empty());
        assert!(userscripts_for_url(&scripts, "data:text/html,x").is_empty());
        assert!(userscripts_for_url(&[], "https://example.com/").is_empty());

        // The returned references point at the original scripts.
        assert_eq!(on_example[0].code, "1");
    }

    #[test]
    fn key_event_all_modifier_combinations_round_trip() {
        let base = key_event();
        for bits in 0u8..=0b1111 {
            let mut ev = base.clone();
            ev.mods = Modifiers::new(bits);
            frame_round_trip(ev);
        }
    }

    #[test]
    fn modifiers_helpers() {
        assert!(Modifiers::NONE.empty());
        assert!(!Modifiers::NONE.with_ctrl().empty());
        let m = Modifiers::NONE.with_ctrl().with_shift().with_alt().with_meta();
        assert!(m.ctrl() && m.shift() && m.alt() && m.meta());
        assert_eq!(m.bits(), 0b1111);
        assert_eq!(
            Modifiers::NONE.with_ctrl() | Modifiers::NONE.with_alt(),
            Modifiers::NONE.with_ctrl().with_alt()
        );
        assert!(Modifiers::default().empty());
    }

    #[test]
    fn every_cursor_icon_round_trips() {
        // The full vocabulary the tab may report — the shell must be able to
        // decode any of it (task: hover cursor).
        let all = [
            CursorIcon::Default,
            CursorIcon::Hidden,
            CursorIcon::Help,
            CursorIcon::Pointer,
            CursorIcon::Progress,
            CursorIcon::Wait,
            CursorIcon::Crosshair,
            CursorIcon::Text,
            CursorIcon::VerticalText,
            CursorIcon::Alias,
            CursorIcon::Copy,
            CursorIcon::Move,
            CursorIcon::NoDrop,
            CursorIcon::NotAllowed,
            CursorIcon::Grab,
            CursorIcon::Grabbing,
            CursorIcon::ColResize,
            CursorIcon::RowResize,
            CursorIcon::NResize,
            CursorIcon::EResize,
            CursorIcon::SResize,
            CursorIcon::WResize,
            CursorIcon::NeResize,
            CursorIcon::NwResize,
            CursorIcon::SeResize,
            CursorIcon::SwResize,
            CursorIcon::EwResize,
            CursorIcon::NsResize,
            CursorIcon::NeswResize,
            CursorIcon::NwseResize,
        ];
        assert_eq!(all.len(), 30, "every CursorIcon variant is covered");
        for cursor in all {
            frame_round_trip(FromTab::CursorChanged { cursor });
        }
        assert_eq!(CursorIcon::default(), CursorIcon::Default);
    }

    #[test]
    fn multiple_frames_stream_in_order() {
        let mut buf = Vec::new();
        write_frame(&mut buf, &ToTab::Reload).unwrap();
        write_frame(&mut buf, &ToTab::Navigate { url: "https://a.test".into() }).unwrap();
        write_frame(&mut buf, &FromTab::Closed).unwrap();

        let mut cur = std::io::Cursor::new(&buf);
        assert_eq!(read_frame::<_, ToTab>(&mut cur).unwrap(), ToTab::Reload);
        assert_eq!(
            read_frame::<_, ToTab>(&mut cur).unwrap(),
            ToTab::Navigate { url: "https://a.test".into() }
        );
        assert_eq!(read_frame::<_, FromTab>(&mut cur).unwrap(), FromTab::Closed);
        // Stream exhausted -> error, not a hang.
        assert!(read_frame::<_, FromTab>(&mut cur).is_err());
    }

    #[test]
    fn truncated_frame_is_error() {
        let mut buf = Vec::new();
        write_frame(&mut buf, &ToTab::Navigate { url: "https://example.com/x".into() }).unwrap();
        // Cut the payload in half (length prefix intact).
        let cut = buf.len() - 5;
        let mut truncated = &buf[..cut];
        let res = read_frame::<_, ToTab>(&mut truncated);
        assert!(res.is_err(), "truncated frame must error, got {res:?}");

        // Truncated length prefix (fewer than 4 bytes) is also an error.
        let res2 = read_frame::<_, ToTab>(&mut &[0u8, 1, 2][..]);
        assert!(res2.is_err());
    }

    #[test]
    fn empty_reader_is_error() {
        let res = read_frame::<_, FromTab>(&mut std::io::empty());
        assert!(res.is_err());
    }

    #[test]
    fn oversize_frame_is_rejected() {
        // Craft a header claiming more than MAX_FRAME_BYTES.
        let header = (MAX_FRAME_BYTES as u32 + 1).to_le_bytes();
        let res = read_frame::<_, ToTab>(&mut &header[..]);
        assert!(res.is_err(), "oversize length prefix must error, got {res:?}");

        let u32_max = u32::MAX.to_le_bytes();
        let res2 = read_frame::<_, ToTab>(&mut &u32_max[..]);
        assert!(res2.is_err());
        assert_eq!(res2.unwrap_err().kind(), std::io::ErrorKind::InvalidData);

        // A frame exactly at MAX_FRAME_BYTES is not rejected by the guard
        // (it then fails on truncated payload, which is a different path).
        let exact = (MAX_FRAME_BYTES as u32).to_le_bytes();
        let res3 = read_frame::<_, ToTab>(&mut &exact[..]);
        assert!(res3.is_err());
        assert_ne!(res3.unwrap_err().kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn garbage_payload_is_invalid_data() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&[9u8; 4]); // plausible tiny length
        buf.extend_from_slice(b"not bincode at all, definitely garbage bytes!!");
        let res = read_frame::<_, ToTab>(&mut &buf[..]);
        assert!(res.is_err());
        assert_eq!(res.unwrap_err().kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn default_url_is_the_internal_welcome_page() {
        assert_eq!(default_url(), "ironsurf://welcome");
    }
}
