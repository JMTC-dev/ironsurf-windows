//! Per-tab state reduced from the `FromTab` message stream.
//!
//! Pure (no Slint, no processes): the shell's message pump applies every
//! decoded `FromTab` through [`TabState::reduce`] and then syncs the UI.
//! `Frame` messages never touch the state — they are handed to the frame
//! compositor directly (coalesced to the newest per tab).

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use ironsurf_common::{
    CursorIcon, DownloadHint, DownloadTrigger, FromTab, NetEvent, ScrollState, TabId,
};

/// Cap on retained DevTools console entries per tab: the oldest are dropped
/// once the ring is full, so a chatty page cannot grow shell memory forever.
pub const MAX_CONSOLE_ENTRIES: usize = 500;
/// Cap on retained DevTools network entries per tab (oldest dropped first).
/// The running log is never cleared on navigation — like Firefox's unticked
/// "Persist logs", it accumulates for the tab's lifetime.
pub const MAX_NETWORK_ENTRIES: usize = 500;

/// One DevTools console entry, reduced from [`FromTab::ConsoleLog`]. Stored
/// on [`TabState::console`] in arrival order; the (future) panel reads it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ConsoleEntry {
    /// Origin subsystem: `"js"` | `"css"` | `"dom"` | `"page"` | `"userscript"`
    /// (a userscript entry leads with its script name — the panel shows both).
    pub source: String,
    /// Severity label as the engine emitted it (`"log"`, `"warn"`, …).
    pub level: String,
    /// Rendered message text.
    pub message: String,
}

/// Find-in-page results as the tab last reported them (task: the `Ctrl-F`
/// find bar). The query travels *with* the counts: the bar shows its counter
/// against exactly the text the search ran on, so a number belonging to a query
/// the reader has already edited past never appears.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FindState {
    /// The query these counts belong to (`""` = not searching).
    pub query: String,
    /// Total hits on the page.
    pub matches: usize,
    /// 1-based position of the **active** (light-blue) hit; 0 = no active hit.
    pub active: usize,
}

impl FindState {
    /// The counter the bar shows: `3 / 12` while searching, an honest “No
    /// results” when the query matches nothing, and blank when there is no
    /// query (the bar is closed, or has been emptied).
    pub fn label(&self) -> String {
        if self.query.is_empty() {
            String::new()
        } else if self.matches == 0 {
            "No results".to_owned()
        } else if self.active == 0 {
            // Search running / nothing jumped to yet: the total alone.
            format!("{} matched", self.matches)
        } else {
            format!("{} / {}", self.active, self.matches)
        }
    }
}

/// Everything the shell renders about one tab, minus the frame itself.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TabState {
    pub title: String,
    pub url: Option<String>,
    pub loading: bool,
    pub can_back: bool,
    pub can_forward: bool,
    pub status: Option<String>,
    pub ready: bool,
    pub closed: bool,
    pub error: Option<String>,
    /// Pointer shape the tab last asked for over its page (task: hover
    /// cursor). Kept per tab so switching tabs re-arms the right shape;
    /// the change itself is applied to the window by the pump at once.
    pub cursor: CursorIcon,
    /// DevTools console feed (bounded by [`MAX_CONSOLE_ENTRIES`]).
    pub console: Vec<ConsoleEntry>,
    /// DevTools network feed (bounded by [`MAX_NETWORK_ENTRIES`]).
    pub network: Vec<NetEvent>,
    /// The find bar's numbers, straight from the tab (see [`FindState`]).
    pub find: FindState,
    /// **Reader view** (task: reader mode): the tab reports the page on
    /// screen is its cleaned-up text-and-images rendering. The 📄 toolbar
    /// button holds while this is set; every real navigation clears it.
    pub reader: bool,
    /// The tab's document viewport as the tab itself last described it (task:
    /// scroll bar). Only the tab has laid the page out, so only it knows how
    /// far the document goes and where inside it the reader is standing now;
    /// the overlay bar the shell draws over the page is placed from these
    /// numbers (see [`scrollbar_thumb`]), kept per tab so switching tabs
    /// switches the bar along with the page.
    pub scroll: ScrollState,
}

impl TabState {
    /// Apply one tab message. Returns `true` when anything observable
    /// changed (so the pump can skip redundant UI syncs).
    pub fn reduce(&mut self, msg: &FromTab) -> bool {
        match msg {
            FromTab::Ready { .. } => {
                let (was, was_err) = (self.ready, self.error.take());
                if was_err.is_some() {
                    return true;
                }
                self.ready = true;
                !was
            }
            FromTab::TitleChanged { title } => {
                let next = title.clone().unwrap_or_default();
                if next == self.title {
                    false
                } else {
                    self.title = next;
                    true
                }
            }
            FromTab::UrlChanged { url } => {
                if url == &self.url {
                    false
                } else {
                    self.url = url.clone();
                    true
                }
            }
            FromTab::LoadingChanged { loading } => {
                // A load that starts replaces the last thing the bar had to
                // say: an error from the previous attempt must never shadow the
                // new one (the UI shows `error` ahead of `status`).
                let started = *loading && !self.loading;
                let was_error = started.then(|| self.error.take()).flatten();
                let changed = *loading != self.loading;
                self.loading = *loading;
                changed || was_error.is_some()
            }
            FromTab::HistoryChanged { can_go_back, can_go_forward } => {
                let changed = *can_go_back != self.can_back
                    || *can_go_forward != self.can_forward;
                self.can_back = *can_go_back;
                self.can_forward = *can_go_forward;
                changed
            }
            FromTab::StatusText { text } => {
                if text == &self.status {
                    false
                } else {
                    self.status = text.clone();
                    true
                }
            }
            // DevTools feed (console + network): appended to bounded per-tab
            // logs the (future) panel reads. They never dirty the frame, so
            // reduce always reports `false` here regardless of acceptance.
            FromTab::ConsoleLog { source, level, message } => {
                push_bounded(
                    &mut self.console,
                    ConsoleEntry {
                        source: source.clone(),
                        level: level.clone(),
                        message: message.clone(),
                    },
                    MAX_CONSOLE_ENTRIES,
                );
                false
            }
            FromTab::NetworkEvent { event } => {
                push_bounded(&mut self.network, event.clone(), MAX_NETWORK_ENTRIES);
                false
            }
            // Find bar: the counts are shown, so a change dirties the UI.
            FromTab::FindResult { query, matches, active } => {
                let next = FindState {
                    query: query.clone(),
                    matches: *matches,
                    active: *active,
                };
                if next == self.find {
                    false
                } else {
                    self.find = next;
                    true
                }
            }
            // Reader view (task: reader mode): mirrors the tab's authoritative
            // answer — the button state follows the tab, never the other way
            // around (a refusal to read an empty page moves nothing).
            FromTab::ReaderState { enabled } => {
                if *enabled == self.reader {
                    false
                } else {
                    self.reader = *enabled;
                    true
                }
            }
            // Actions for the pump (spawn a new tab, open the context menu,
            // file the copy into the session clipboard, show the modal
            // dialog overlay), not per-tab state.
            FromTab::OpenLinkInNewTab { .. } => false,
            FromTab::ContextMenu { .. } => false,
            FromTab::ClipboardText { .. } => false,
            FromTab::DialogRequest { .. } => false,
            // Hover cursor: no frame to repaint (the pump pushes it onto the
            // window immediately); the record here only serves tab switches.
            FromTab::CursorChanged { cursor } => {
                self.cursor = *cursor;
                false
            }
            // Scroll geometry: nothing to repaint (the pump draws the overlay
            // bar the moment the report lands, so the reader never waits on a UI
            // sync), and the record here only serves tab switches and a thumb
            // that has to be re-placed after a resize.
            FromTab::ScrollState { state } => {
                if state.same_within(&self.scroll, 0.5) {
                    false
                } else {
                    self.scroll = *state;
                    true
                }
            }
            FromTab::Frame { .. } => false,
            // Archive mode: the pump writes the body to disk; there is no
            // per-tab state to show and never a frame to repaint.
            FromTab::ArchivedFetch { .. } => false,
            // The archive server is shell machinery, not page state: the pump
            // acts on it (bind the server / mirror the settings), the tab's
            // own line never changes.
            FromTab::ArchiveServerStart { .. } | FromTab::ArchiveServerSettingsChanged { .. } => false,
            // File downloads: rows live on the owning `Proc` (they die with
            // its process), not on the tab's line, so nothing here changes.
            // The pump writes bytes and files the row; `sync_ui` shows it.
            FromTab::DownloadStart { .. }
            | FromTab::DownloadChunk { .. }
            | FromTab::DownloadEnd { .. } => false,
            // DevTools Inspector: the contract landed with the snapshot stream and
            // the pane is task #82 (shell side) — reducing it to nothing until then
            // keeps the pump honest (there is no inspector state to show yet).
            FromTab::InspectSnapshot { .. } | FromTab::InspectHover { .. } | FromTab::InspectPicked { .. } => false,
            FromTab::Closed => {
                if self.closed {
                    false
                } else {
                    self.closed = true;
                    true
                }
            }
            FromTab::Error { message } => {
                self.error = Some(message.clone());
                self.loading = false;
                true
            }
        }
    }

    /// Whether a scroll-geometry report from the tab **moves the offset** this
    /// record holds, as opposed to only restating the range — a window resize,
    /// a zoom, a fresh document opening at the top. Only an offset that moves
    /// is the page scrolling, and that alone is the reader's cue to put the
    /// overlay scroll bar up (task: scroll bar). Half a CSS px of slack, the
    /// same tolerance [`ScrollState::same_within`] reports with, so a tab that
    /// settles a fraction of a pixel short of where it said it was never looks
    /// like a scroll.
    pub fn scroll_moves(&self, next: &ScrollState) -> bool {
        (next.y - self.scroll.y).abs() > 0.5
    }

    /// The status-bar security chip for the current URL: a lock for https,
    /// a warning for plain http, nothing for `about:`/`file:`/etc.
    pub fn security(&self) -> Security {
        Security::of(self.url.as_deref().unwrap_or(""))
    }
}

// ---- overlay scroll bar ---------------------------------------------------------
//
// The bar itself is one Rectangle over the page's right edge
// (ui/ironsurf.slint) and every *decision* about it is here: whether the
// numbers describe a page that scrolls at all, where its thumb sits, and how
// long it stays up after the reader last moved the document. Pure and
// headless, so a thumb's math is testable without a window; the pump in `app`
// only hands over the live geometry and the clock.

/// How long the page's overlay scroll bar stays up after the document last
/// moved under the reader's wheel (task: scroll bar), before it hides itself
/// again. Long enough to read where one stands in a long page after stopping,
/// short enough that the page gets its right edge back.
pub const SCROLLBAR_HOLD: Duration = Duration::from_millis(2000);

/// The shortest thumb a reader still reads as a thumb, px. A document many
/// screens long would otherwise draw a hairline the width of one pixel row.
pub const SCROLLBAR_MIN_THUMB: f32 = 28.;

/// Where the page's overlay scroll bar thumb sits in a `track`-px column, from
/// the geometry the tab reported: `(top, height)` in px, or `None` when there
/// is nothing to point at — a page that fits its window (`range == 0`), or a
/// track too short to hold a thumb in (a window that is not laid out yet).
///
/// The content spans `viewport_h + range` CSS px, so the thumb takes
/// `viewport_h / (viewport_h + range)` of the track and travels whatever is
/// left of it: at `y == 0` it sits at the track's top and at `y == range` its
/// bottom edge lands exactly on the track's end — the same rule at every zoom
/// and window size, because both numbers come from the page's own space.
pub fn scrollbar_thumb(state: &ScrollState, track: f32) -> Option<(f32, f32)> {
    if !state.is_scrollable() || !track.is_finite() || track <= SCROLLBAR_MIN_THUMB {
        return None;
    }
    let window = state.viewport_h.max(0.0);
    let height = (track * (window / (window + state.range))).clamp(SCROLLBAR_MIN_THUMB, track);
    let top = (state.y / state.range).clamp(0.0, 1.0) * (track - height);
    Some((top, height))
}

// ---- DevTools panel view-models ---------------------------------------------
//
// The panel (ui/ironsurf.slint) paints plain `Text` cells, so everything the
// renderer needs is pre-formatted here: this is the reduction the panel shows,
// kept pure (no Slint) so it is unit-testable headless and drives the UI through
// `app::Core::sync_ui`.

/// Placeholder for "the engine never said" cells in the panel tables (an em
/// dash, like Firefox's network table).
pub const MISSING: &str = "\u{2014}";

/// One rendered DevTools console row.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ConsoleRow {
    /// Severity label (`"log"` | `"info"` | `"warn"` | `"error"`): drives the
    /// panel's level icon and colour.
    pub level: String,
    /// `"[source] message"`, exactly as the panel paints it.
    pub text: String,
}

/// One rendered DevTools network row: every column pre-formatted, missing
/// values [`MISSING`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NetRow {
    pub method: String,
    pub url: String,
    /// What initiated the request (`"document"`, `"script"`, `"resource"`,
    /// `"xhr"`, `"fetch"`), or [`MISSING`] when the engine did not say.
    pub initiator: String,
    /// Status code, or the failure text when the request never answered.
    pub status: String,
    pub size: String,
    pub time: String,
    /// `true` ⇒ the request failed ([`NetEvent::error`] is `Some`) and the
    /// panel paints `status` in red.
    pub failed: bool,
}

/// What copying a Network row yields (task: copy in the DevTools tabs): its
/// columns in display order, joined with single spaces. An unknown value
/// ([`MISSING`]) is skipped rather than pasted as a dash.
pub fn net_row_clipboard(r: &NetRow) -> String {
    [
        r.method.as_str(),
        r.url.as_str(),
        r.initiator.as_str(),
        r.status.as_str(),
        r.size.as_str(),
        r.time.as_str(),
    ]
    .into_iter()
    .filter(|s| !s.is_empty() && *s != MISSING)
    .collect::<Vec<&str>>()
    .join(" ")
}

impl TabState {
    /// Console rows for the panel, in arrival order.
    pub fn console_rows(&self) -> Vec<ConsoleRow> {
        self.console
            .iter()
            .map(|e| ConsoleRow {
                level: e.level.clone(),
                text: format!("[{}] {}", e.source, e.message),
            })
            .collect()
    }

    /// Network rows for the panel, in arrival order.
    pub fn net_rows(&self) -> Vec<NetRow> {
        self.network
            .iter()
            .map(|e| NetRow {
                method: e.method.clone(),
                url: e.url.clone(),
                initiator: e.initiator.clone().unwrap_or_else(|| MISSING.to_string()),
                // A failed request has no status: say *why* instead (the panel
                // paints the whole cell red when `failed`).
                status: match (&e.status, &e.error) {
                    (Some(status), _) => status.to_string(),
                    (None, Some(err)) => err.clone(),
                    (None, None) => MISSING.to_string(),
                },
                size: format_size(e.response_bytes),
                time: format_duration(e.duration_ms),
                failed: e.error.is_some(),
            })
            .collect()
    }
}

/// Response size for the panel: bytes below 1 KiB, then decimal units
/// (`1.4 kB`, `2.0 MB`). Unknown (`None`) is [`MISSING`].
pub fn format_size(bytes: Option<u64>) -> String {
    const UNITS: [&str; 5] = ["B", "kB", "MB", "GB", "TB"];
    match bytes {
        None => MISSING.to_string(),
        Some(b) => {
            let mut value = b as f64;
            let mut unit = 0;
            while value >= 1024.0 && unit + 1 < UNITS.len() {
                value /= 1024.0;
                unit += 1;
            }
            if unit == 0 {
                format!("{b} B")
            } else {
                format!("{value:.1} {}", UNITS[unit])
            }
        }
    }
}

/// Request duration for the panel: whole ms below a second, seconds above.
/// Never measured (`None`) is [`MISSING`].
pub fn format_duration(ms: Option<u64>) -> String {
    match ms {
        None => MISSING.to_string(),
        Some(ms) if ms < 1000 => format!("{ms} ms"),
        Some(ms) => format!("{:.2} s", ms as f64 / 1000.0),
    }
}

/// Append `item`, dropping the oldest front entries until the vec is at most
/// `cap` long. Shared by the console and network logs.
fn push_bounded<T>(log: &mut Vec<T>, item: T, cap: usize) {
    log.push(item);
    if let Some(excess) = log.len().checked_sub(cap) {
        log.drain(..excess);
    }
}

/// Lock/http indicator rendered next to the status text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Security {
    Https,
    InsecureHttp,
    None,
}

impl Security {
    pub fn of(url: &str) -> Self {
        if url.starts_with("https:") {
            Security::Https
        } else if url.starts_with("http:") {
            Security::InsecureHttp
        } else {
            Security::None
        }
    }

    /// `(glyph, css color)` for the status bar.
    pub fn render(self) -> (&'static str, &'static str) {
        match self {
            // U+26EC, per ARCHITECTURE.md ("⛬ / http warning")
            Security::Https => ("⛬", "#8fce8f"),
            Security::InsecureHttp => ("⚠ http", "#e57373"),
            Security::None => ("", "#888888"),
        }
    }
}

// ---- downloads (task: file downloads) ---------------------------------------
//
// A tab decides a top-level fetch is a file and streams it to the shell; the
// **shell** owns the row, the name, and the file (see the tab's `downloads.rs`
// for the sending half and [`crate::downloads`] for the writing half). Rows
// live on the owning `Proc` — a tab's downloads die with its process.

/// Where a download stands. `Saving` is a live file on disk; `Done` means
/// every byte landed and the path is real; `Failed` means the fetch or the
/// write did not, and a file that never got a byte is deleted rather than
/// left as a zero-byte tease.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DownloadState {
    Saving,
    Done,
    Failed,
}

/// One download row: what the ⬇ panel lists and the status line quotes.
/// `bytes` counts what landed **so far** (a chunked feed makes it progress);
/// `path` is `Some` only once the file is complete — an in-flight row's file
/// is known to the store ([`DownloadStore::target`]), not to the view.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DownloadRow {
    pub tab_id: TabId,
    /// The tab's own id for this download (a per-tab counter: two tabs can
    /// both say `1`, so a row is keyed `(tab_id, id)`).
    pub id: u64,
    /// The name shown — sanitized, and the name the file was *asked* to be.
    /// The file on disk may carry a `(2)` counter next to it; the row keeps
    /// the clean name and [`DownloadRow::path`] carries the truth.
    pub name: String,
    pub url: String,
    pub trigger: DownloadTrigger,
    pub bytes: u64,
    pub state: DownloadState,
    /// Set when the file is complete; `None` while `Saving` and when a
    /// download failed with no bytes written.
    pub path: Option<PathBuf>,
    /// Monotonic counter handed out by the shell's pump, so the ⬇ panel can
    /// order rows across tabs (rows arrive per-proc; the panel lists them
    /// newest first).
    pub stamp: u64,
}

/// Rows a tab's store keeps at most (oldest **finished** row dropped first,
/// so a live `Saving` row is never lost to a cap). A panel shows
/// [`crate::downloads::MENU_ROWS`] of these, newest first.
pub const MAX_DOWNLOAD_ROWS: usize = 200;

/// The downloads one tab process has filed. Keys are `(tab_id, id)` pairs
/// the tab itself minted; a download whose process died is reaped by
/// [`DownloadStore::reap`] so no row streams into a dead file.
#[derive(Clone, Debug, Default)]
pub struct DownloadStore {
    /// Arrival order; the panel re-sorts by [`DownloadRow::stamp`] desc.
    rows: Vec<DownloadRow>,
    /// The file an in-flight row streams into, per `(tab, id)`.
    targets: HashMap<(TabId, u64), PathBuf>,
}

/// A download settled ([`DownloadStore::finish`]): enough for the pump to
/// delete what should not be in the folder.
pub struct Finished {
    pub row: DownloadRow,
    /// The partial file to delete: `Some` when nothing was ever written.
    pub delete: Option<PathBuf>,
}

impl DownloadStore {
    /// Rows in arrival order.
    pub fn rows(&self) -> &[DownloadRow] {
        &self.rows
    }

    /// The file the row streams into, while it is `Saving` (`None` once it
    /// settles: the row's own [`DownloadRow::path`] carries a finished file).
    pub fn target(&self, tab: TabId, id: u64) -> Option<PathBuf> {
        self.targets.get(&(tab, id)).cloned()
    }

    /// File a [`FromTab::DownloadStart`]: a `Saving` row, named, plus the
    /// path its bytes are streaming into. `path` is the file the pump has
    /// just created (see [`crate::downloads::create`]); `None` when the disk
    /// refused (a read-only folder), which leaves the row `Saving` with no
    /// file to show — the pump marks it `Failed` right away.
    pub fn start(&mut self, tab: TabId, hint: &DownloadHint, name: String, path: Option<PathBuf>, stamp: u64) -> DownloadRow {
        let row = DownloadRow {
            tab_id: tab,
            id: hint.id,
            name,
            url: hint.url.clone(),
            trigger: hint.trigger,
            bytes: 0,
            state: DownloadState::Saving,
            path: None,
            stamp,
        };
        self.rows.push(row.clone());
        self.enforce_cap();
        if let Some(path) = path {
            self.targets.insert((row.tab_id, row.id), path);
        }
        row
    }

    /// Add arrived bytes to a row. Returns `None` when no row is streaming
    /// (a chunk after its `End`, or a row dropped by [`Self::enforce_cap`])
    /// — the pump then writes nothing, because there is no file to trust.
    pub fn add_bytes(&mut self, tab: TabId, id: u64, bytes: usize) -> Option<PathBuf> {
        let idx = self.find_streaming(tab, id)?;
        self.rows[idx].bytes += bytes as u64;
        self.targets.get(&(tab, id)).cloned()
    }

    /// A download settled. `error: Some` (or a write that failed) marks the
    /// row `Failed` — and a file that never got a byte is handed back for
    /// deletion, because an empty `report.zip` is not a download. `error:
    /// None` marks it `Done` with the path it grew at.
    pub fn finish(&mut self, tab: TabId, id: u64, error: Option<&str>) -> Option<Finished> {
        let idx = self.find_streaming(tab, id)?;
        let target = self.targets.remove(&(tab, id));
        let (failed, bytes) = {
            let row = &mut self.rows[idx];
            row.state = if error.is_some() { DownloadState::Failed } else { DownloadState::Done };
            // A failed row keeps its path only when bytes landed: the reader
            // should see the name of a file that still exists.
            row.path = match (error, target.as_ref()) {
                (None, path) => path.cloned(),
                (Some(_), path) if row.bytes > 0 => path.cloned(),
                (_, _) => None,
            };
            (error.is_some(), row.bytes)
        };
        let delete = match (failed, bytes) {
            (true, 0) => target.clone(),
            (_, _) => None,
        };
        Some(Finished { row: self.rows[idx].clone(), delete })
    }

    /// The newest row — `Saving` rows outrank finished ones so the status
    /// line says what is happening, not what happened. A tab with no
    /// download says `None`; the UI's last-known-rule keeps the old text.
    pub fn newest(&self) -> Option<&DownloadRow> {
        self.rows
            .iter()
            .max_by_key(|row| (row.state == DownloadState::Saving, row.stamp))
    }

    /// A tab process died: its unfinished rows are `Failed` and their
    /// partial files come back for deletion — no orphan `.zip` is left
    /// growing in the folder ([`DownloadStore::reap`] hands the paths to the
    /// pump, which calls [`crate::downloads::discard`]).
    pub fn reap(&mut self, tab: TabId) -> Vec<PathBuf> {
        let mut orphans = Vec::new();
        let mut keys = Vec::new();
        for idx in 0..self.rows.len() {
            if self.rows[idx].tab_id == tab && self.rows[idx].state == DownloadState::Saving {
                self.rows[idx].state = DownloadState::Failed;
                self.rows[idx].path = None;
                keys.push((tab, self.rows[idx].id));
            }
        }
        for key in keys {
            if let Some(path) = self.targets.remove(&key) {
                orphans.push(path);
            }
        }
        orphans
    }

    /// `Clear finished`: drop the settled rows, keep the ones still running.
    /// Returns the dropped rows so the pump can leave their files alone —
    /// clearing a list is not deleting what it listed.
    pub fn clear_finished(&mut self) -> Vec<DownloadRow> {
        let mut dropped = Vec::new();
        self.rows.retain(|row| {
            let keep = row.state == DownloadState::Saving;
            if !keep {
                dropped.push(row.clone());
            }
            keep
        });
        dropped
    }

    /// Every file a finished row points at — what `Clear finished` leaves
    /// behind on disk, and what the panel shows once done.
    pub fn files(&self) -> Vec<PathBuf> {
        self.rows.iter().filter_map(|r| r.path.clone()).collect()
    }

    fn find_index(&self, tab: TabId, id: u64) -> Option<usize> {
        self.rows.iter().position(|r| r.tab_id == tab && r.id == id)
    }

    /// A row that is still writing. The last message for an id settles it,
    /// and nothing after that has anywhere to land.
    fn find_streaming(&self, tab: TabId, id: u64) -> Option<usize> {
        self.rows
            .iter()
            .position(|r| r.tab_id == tab && r.id == id && r.state == DownloadState::Saving)
    }

    /// Drop the oldest **finished** row until the vec fits
    /// [`MAX_DOWNLOAD_ROWS`]. A `Saving` row is never dropped: its file is
    /// still growing, and losing it would lose the only handle to it.
    fn enforce_cap(&mut self) {
        let excess = self.rows.len().saturating_sub(MAX_DOWNLOAD_ROWS);
        if excess == 0 {
            return;
        }
        // Oldest first: finished rows go, a live one stays (its file is
        // still growing). A store of only live rows bends the cap.
        let mut doomed: Vec<usize> = self
            .rows
            .iter()
            .enumerate()
            .filter(|(_, r)| r.state != DownloadState::Saving)
            .map(|(i, _)| i)
            .take(excess)
            .collect();
        doomed.reverse(); // remove from the back so indexes stay valid
        for i in doomed {
            let row = self.rows.remove(i);
            self.targets.remove(&(row.tab_id, row.id));
        }
    }
}

// ---- DevTools Inspector pane ----------------------------------------------
//
// The tab owns the document, the shell owns the presentation (ARCH §Developer
// tools). What arrives on the wire is an [`ironsurf_common::InspectSnapshot`]:
// a **flat** pre-order list — every node's children follow it in document
// order, one deeper per level, so `depth` alone describes the tree and the ids
// are positional handles (blitz recycles its own node ids, which is why the
// `generation` counter says when to throw them away). Folding, selection, the
// breadcrumb and the syntax colouring are all computed from that flat list
// here, in pure Rust, and pushed to the UI as row/token *models* rather than
// pre-formatted strings so the palette stays in the .slint file.

use ironsurf_common::{InspectKind, InspectNode, InspectSnapshot};

/// Token kinds: a row's markup is a `Vec<Tok>` of these, and `inspect-kind-color`
/// in `ui/ironsurf.slint` maps each kind to one entry of the fixed colour table.
pub const TOK_TEXT: u8 = 0; // near-white
pub const TOK_TAG: u8 = 1; // orange
pub const TOK_ATTR: u8 = 2; // blue
pub const TOK_STR: u8 = 3; // green
pub const TOK_PUNCT: u8 = 4; // grey — `<` `>` `/` `=` quotes
pub const TOK_DANGER: u8 = 5; // red — `<script>`/`<style>`/`<head>` names, `data:` values
/// Comment markup (drawn grey-italic).
pub const TOK_COMMENT: u8 = 6;
/// Doctype markup (drawn grey-italic).
pub const TOK_DOCTYPE: u8 = 7;
/// The value slot of an `id`/`class` attribute, which gets its own accent.
pub const TOK_IDENT: u8 = 8;

/// One syntax-highlighted token: `(text, kind)` — the contract's `Tok`.
pub type Tok = (String, u8);

#[inline]
fn tok(text: impl Into<String>, kind: u8) -> Tok {
    (text.into(), kind)
}

/// The quotes around an attribute value: `"` normally, `'` when the value
/// already contains a `"` (a row is markup a human reads, never re-parsed).
fn quote_attr(value: &str) -> (&'static str, &'static str) {
    if value.contains('"') && !value.contains('\'') {
        ("'", "'")
    } else {
        ("\"", "\"")
    }
}

/// Tag names whose *contents* a reader should not skim past: the row is painted
/// in the danger colour, the way a `data:` URL is.
pub fn tag_is_dangerous(tag: &str) -> bool {
    matches!(tag, "script" | "style" | "head")
}

/// Whether an attribute value is a `data:` URL — said in the danger colour
/// rather than printed as ordinary green, because these are the values that
/// hide a whole document (or a payload) in one attribute.
pub fn value_is_dangerous(value: &str) -> bool {
    value.trim_start().to_ascii_lowercase().starts_with("data:")
}

/// Element nodes are the only ones with a tag name, attributes and children.
#[inline]
pub fn dom_is_element(n: &InspectNode) -> bool {
    n.kind == InspectKind::Element
}

/// Text nodes carry their (already collapsed and bounded) text in `text`.
#[inline]
pub fn dom_is_text(n: &InspectNode) -> bool {
    n.kind == InspectKind::Text
}

/// Everything else: `#document`, `<!doctype html>`, `#comment`.
#[inline]
pub fn dom_is_other(n: &InspectNode) -> bool {
    !dom_is_element(n) && !dom_is_text(n)
}

/// How many of the snapshot's nodes are elements — the number the Inspector
/// button shows (text, comment and document nodes do not earn a count).
pub fn dom_element_count(nodes: &[InspectNode]) -> usize {
    nodes.iter().filter(|n| dom_is_element(n)).count()
}

fn shorten(s: &str, max: usize) -> &str {
    let mut end = s.len().min(max);
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// A node's short name for the breadcrumb: `div#main`, `body`, `#text`, …
/// (`id` first, else the first `class`, the way a reader finds a node they
/// wrote CSS for).
pub fn node_label(n: &InspectNode) -> String {
    match n.kind {
        InspectKind::Element => {
            let mut s = n.tag.clone();
            if let Some(id) = n.attrs.iter().find(|(k, _)| k == "id").map(|(_, v)| v.as_str()) {
                if !id.is_empty() {
                    s.push('#');
                    s.push_str(shorten(id, 24));
                }
            } else if let Some(class) = n.attrs.iter().find(|(k, _)| k == "class").map(|(_, v)| v.as_str()) {
                if let Some(first) = class.split_whitespace().next() {
                    s.push('.');
                    s.push_str(shorten(first, 24));
                }
            }
            s
        }
        InspectKind::Text => "#text".into(),
        InspectKind::Comment => "#comment".into(),
        InspectKind::Document => "#document".into(),
        InspectKind::Doctype => "<!doctype html>".into(),
    }
}

/// The markup tokens for one row: `<div class="x">` broken into punctuation,
/// tag, attribute and value tokens; text in the text colour; `<!-- … -->` and
/// `<!doctype html>` in their own greys; dangerous tags and `data:` values in
/// red. An element with nothing inside it also gets its `</tag>`.
pub fn node_tokens(n: &InspectNode, has_children: bool) -> Vec<Tok> {
    let mut out: Vec<Tok> = Vec::new();
    let danger = dom_is_element(n) && tag_is_dangerous(&n.tag);
    let name = if danger { TOK_DANGER } else { TOK_TAG };
    match n.kind {
        InspectKind::Element => {
            out.push(tok("<", TOK_PUNCT));
            out.push(tok(n.tag.clone(), name));
            for (attr, value) in &n.attrs {
                out.push(tok(format!(" {attr}"), TOK_ATTR));
                out.push(tok("=", TOK_PUNCT));
                let kind = if value_is_dangerous(value) {
                    TOK_DANGER
                } else if attr == "id" || attr == "class" {
                    TOK_IDENT
                } else {
                    TOK_STR
                };
                let (open, close) = quote_attr(value);
                out.push(tok(open, TOK_PUNCT));
                out.push(tok(value.clone(), kind));
                out.push(tok(close, TOK_PUNCT));
            }
            out.push(tok(">", TOK_PUNCT));
            if !has_children {
                out.push(tok("</", TOK_PUNCT));
                out.push(tok(n.tag.clone(), name));
                out.push(tok(">", TOK_PUNCT));
            }
        }
        InspectKind::Text => out.push(tok(n.text.clone(), TOK_TEXT)),
        InspectKind::Comment => {
            out.push(tok("<!--", TOK_PUNCT));
            out.push(tok(n.text.clone(), TOK_COMMENT));
            out.push(tok("-->", TOK_PUNCT));
        }
        InspectKind::Document => out.push(tok("#document", TOK_PUNCT)),
        InspectKind::Doctype => {
            out.push(tok("<!", TOK_PUNCT));
            out.push(tok(
                if n.text.is_empty() { "doctype".into() } else { n.text.clone() },
                TOK_DOCTYPE,
            ));
            out.push(tok(">", TOK_PUNCT));
        }
    }
    out
}

/// The end of the subtree rooted at `start` in the flat pre-order snapshot:
/// everything after it deeper than the node itself, up to the first node that
/// is not. (`Document` has no children of its own in the walk, so this is the
/// whole rest of a truncated snapshot — which is exactly what a copy of a
/// truncated document should be.)
fn copy_subtree_end(nodes: &[InspectNode], start: usize) -> usize {
    let base = nodes[start].depth;
    let mut end = start + 1;
    while end < nodes.len() && nodes[end].depth > base {
        end += 1;
    }
    end
}

/// Renders one subtree as indented markup for the copy buffer (see
/// [`InspectorUi::copy_text`]): each node on its own indented line, a
/// document element's children between its open and close tag, and the
/// `#document` row itself contributing nothing — a copy of the document is
/// its children. `root_depth` is the depth painted flush-left: an element
/// copy passes its own depth, so the node the reader picked starts the text
/// and every level under it gains one indent.
fn render_copy_markup(nodes: &[InspectNode], i: usize, root_depth: usize, out: &mut String) {
    let n = &nodes[i];
    let end = copy_subtree_end(nodes, i);
    let indent = "  ".repeat(n.depth.saturating_sub(root_depth));
    match n.kind {
        InspectKind::Document => {
            copy_children(nodes, i + 1, end, root_depth, out);
        }
        InspectKind::Element => {
            if end == i + 1 {
                // a leaf renders exactly as its row shows it: <tag></tag>
                out.push_str(&indent);
                out.push_str(&node_tokens(n, false).into_iter().map(|t| t.0).collect::<String>());
                out.push('\n');
                return;
            }
            out.push_str(&indent);
            out.push_str(&node_tokens(n, true).into_iter().map(|t| t.0).collect::<String>());
            out.push('\n');
            copy_children(nodes, i + 1, end, root_depth, out);
            out.push_str(&indent);
            out.push_str("</");
            out.push_str(&n.tag);
            out.push_str(">\n");
        }
        InspectKind::Text => {
            out.push_str(&indent);
            out.push_str(n.text.trim());
            out.push('\n');
        }
        InspectKind::Comment => {
            out.push_str(&indent);
            out.push_str("<!--");
            out.push_str(&n.text);
            out.push_str("-->\n");
        }
        InspectKind::Doctype => {
            out.push_str(&indent);
            out.push_str("<!DOCTYPE");
            if !n.text.is_empty() {
                out.push(' ');
                out.push_str(&n.text);
            }
            out.push_str(">\n");
        }
    }
}

/// Renders each direct child of the node spanning `(i, end)` once. The flat
/// list puts every descendant in the range, so after a child is rendered the
/// walk jumps past its whole subtree to the next sibling.
fn copy_children(nodes: &[InspectNode], mut c: usize, end: usize, root_depth: usize, out: &mut String) {
    while c < end {
        render_copy_markup(nodes, c, root_depth, out);
        c = copy_subtree_end(nodes, c);
    }
}

/// One visible row of the Inspector tree (a folded-away node simply has no row).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct InspectorRow {
    /// The snapshot node this row stands for (a positional handle).
    pub node: u64,
    /// Nesting level, for the `depth × 12px` indent.
    pub depth: usize,
    /// The row's markup as syntax-highlighted tokens.
    pub toks: Vec<Tok>,
    /// Something inside this node: the row gets a `+/−` twisty.
    pub has_children: bool,
    pub collapsed: bool,
    pub selected: bool,
    /// Comments and the doctype are drawn italic.
    pub italic: bool,
}

/// The Inspector pane's whole view-model for **one tab**: the tree the tab last
/// sent, plus everything the shell owns on top of it — which nodes are folded,
/// which one is selected (and therefore what the breadcrumb says), whether
/// *inspect element* is armed, and whether a tree has been asked for but not
/// answered yet.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct InspectorUi {
    snapshot: Option<InspectSnapshot>,
    selected: Option<u64>,
    /// Nodes the reader folded shut.
    collapsed: Vec<u64>,
    /// ⌖ *inspect element* armed for this tab.
    pick: bool,
    /// A tree was asked for (`ToTab::SetInspect { open: true }` or
    /// `ToTab::InspectPickAt`) and no snapshot has landed since — the pane
    /// shows its honest empty state instead of a blank rectangle.
    pending: bool,
    /// The node the **page** selected and the tree still owes a scroll to (⌖
    /// pick, context-menu *Inspect Element*). Held as a node id and never as a
    /// row number: the rows a request lands on are the ones painted next, and
    /// they move when a subtree folds or the document is re-walked.
    reveal: Option<u64>,
    /// Counts reveal requests. The pane watches the count rather than the row,
    /// so asking for the row it already shows (pick, scroll away, pick again)
    /// still moves the tree.
    reveal_token: u64,
}

impl InspectorUi {
    /// The tree, if the tab has sent one.
    pub fn snapshot(&self) -> Option<&InspectSnapshot> {
        self.snapshot.as_ref()
    }

    /// File a snapshot. Ids are positional, so a page that changed under the
    /// pane can make any of them mean something else: folds that no longer name
    /// a node are dropped, and a new `generation` re-takes the selection from
    /// the tab (`InspectSnapshot::selected`) instead of guessing.
    pub fn set_snapshot(&mut self, snapshot: InspectSnapshot) {
        let new_generation = self
            .snapshot
            .as_ref()
            .is_none_or(|s| s.generation != snapshot.generation);
        let keep = self.selected;
        self.pending = false;
        self.snapshot = Some(snapshot);
        let snap = self.snapshot.as_ref().expect("just set");
        self.collapsed.retain(|id| snap.nodes.iter().any(|n| n.id == *id));
        let carried = keep.filter(|id| !new_generation && snap.nodes.iter().any(|n| n.id == *id));
        self.selected = carried
            .or(snap.selected)
            .or_else(|| snap.nodes.first().map(|n| n.id));
        // A selection the *tab* moved is news from the page — the answer to
        // `InspectPickAt` ships the hit node in `selected` — so the tree follows
        // it. The pane's own pick (`carried`) is not: the reader is on that row.
        if carried.is_none() && snap.selected.is_some() && self.selected == snap.selected {
            self.reveal = self.selected;
            self.reveal_token = self.reveal_token.wrapping_add(1);
        }
    }

    /// Forget the tree — the pane closed, or the tab it described is gone. The
    /// fold set goes with it; the pick state does not (that is the tab's ⌖).
    pub fn clear_tree(&mut self) {
        self.snapshot = None;
        self.selected = None;
        self.collapsed.clear();
        self.pending = true;
        // Nothing to scroll to. `reveal_token` keeps counting, so a request on
        // the next document is always a change the pane has not seen before.
        self.reveal = None;
    }

    /// ⌖ armed for this tab?
    pub fn pick(&self) -> bool {
        self.pick
    }

    pub fn set_pick(&mut self, pick: bool) {
        self.pick = pick;
    }

    /// A tree was asked for and has not arrived.
    pub fn pending(&self) -> bool {
        self.pending
    }

    /// Say a tree is about to be asked for (the pane opened, a pick was made).
    pub fn expect_tree(&mut self) {
        self.pending = true;
    }

    /// The node the breadcrumb describes.
    pub fn selected(&self) -> Option<u64> {
        self.selected
    }

    /// Select a node by id. An id the tree does not contain is "the snapshot is
    /// stale", never a guess, so it is ignored; a real selection unfolds every
    /// collapsed ancestor, or the row the reader clicked would stay hidden.
    pub fn select(&mut self, node: u64) {
        if !self.select_known(node) {
            return;
        }
        // Clicking a row *is* arriving at it: no scroll is owed.
        self.reveal = None;
    }

    /// The page selected `node` (⌖ pick, the context menu's *Inspect Element*,
    /// or a snapshot that came back naming one): select it **and** ask the pane
    /// to bring its row into view, so a selection made down the page is not lost
    /// above or below the fold.
    pub fn select_from_page(&mut self, node: u64) {
        if !self.select_known(node) {
            return;
        }
        self.reveal = Some(node);
        self.reveal_token = self.reveal_token.wrapping_add(1);
    }

    /// Highlight `node` and unfold every collapsed ancestor. `false` when the
    /// tree has no such id — a stale snapshot, which is never guessed at.
    fn select_known(&mut self, node: u64) -> bool {
        let Some(index) = self.index_of(node) else { return false };
        self.selected = Some(node);
        for a in self.ancestor_ids(index) {
            self.collapsed.retain(|c| *c != a);
        }
        true
    }

    /// The scroll the pane owes the page: `(row, token)`, where `row` is the row
    /// the pending page-side selection occupies in [`Self::rows`] and `token`
    /// counts requests. `None` when nothing is owed — no request, the reader
    /// selected elsewhere since, or the node is folded out of the tree.
    pub fn reveal_request(&self) -> Option<(usize, u64)> {
        let node = self.reveal.filter(|n| Some(*n) == self.selected)?;
        Some((self.row_index_of(node)?, self.reveal_token))
    }

    /// Which row of the painted tree `node` sits in, `None` while it is folded
    /// away or no tree has arrived. Asked of [`Self::rows`] itself, so the index
    /// and the pane's row count cannot disagree.
    pub fn row_index_of(&self, node: u64) -> Option<usize> {
        self.rows().iter().position(|r| r.node == node)
    }

    /// Offer an owed scroll again. The pane moves on a *change* in the request,
    /// and a tree that has only just come on screen — the Inspector pane opened,
    /// the panel reopened — never saw the request the page made, so it has to be
    /// asked for a second time.
    pub fn rearm_reveal(&mut self) {
        if self.reveal.is_some() {
            self.reveal_token = self.reveal_token.wrapping_add(1);
        }
    }

    /// Fold or unfold one node's subtree. Returns the node's new folded state —
    /// `false` for a node with nothing inside it (a leaf has no subtree to
    /// hide, so its row shows no twisty and its twisty is not clickable).
    pub fn toggle_fold(&mut self, node: u64) -> bool {
        let Some(i) = self.index_of(node) else { return false };
        let has_children = self
            .snapshot
            .as_ref()
            .and_then(|s| s.nodes.get(i + 1).map(|nx| nx.depth > s.nodes[i].depth))
            .unwrap_or(false);
        if !has_children {
            return false;
        }
        // Folding is the reader arranging the tree themselves; a scroll the page
        // asked for before would only drag them back to where they left it.
        self.reveal = None;
        match self.collapsed.iter().position(|c| *c == node) {
            Some(i) => {
                self.collapsed.remove(i);
                false
            }
            None => {
                self.collapsed.push(node);
                true
            }
        }
    }

    /// Nodes in the tree (text and comments included).
    pub fn node_count(&self) -> usize {
        self.snapshot.as_ref().map(|s| s.nodes.len()).unwrap_or(0)
    }

    /// Elements in the tree — what the Inspector button counts.
    pub fn element_count(&self) -> usize {
        self.snapshot
            .as_ref()
            .map(|s| dom_element_count(&s.nodes))
            .unwrap_or(0)
    }

    /// The tree hit `MAX_INSPECT_NODES`: the pane says so as a dim last row.
    pub fn truncated(&self) -> bool {
        self.snapshot.as_ref().is_some_and(|s| s.truncated)
    }

    pub fn generation(&self) -> Option<u64> {
        self.snapshot.as_ref().map(|s| s.generation)
    }

    /// The document address the snapshot was taken at (the tab stamps it).
    pub fn address(&self) -> Option<&str> {
        self.snapshot.as_ref().map(|s| s.url.as_str())
    }

    fn index_of(&self, node: u64) -> Option<usize> {
        self.snapshot.as_ref()?.nodes.iter().position(|n| n.id == node)
    }

    fn node_at(&self, index: usize) -> Option<&InspectNode> {
        self.snapshot.as_ref()?.nodes.get(index)
    }

    /// `(root, node)` ancestor ids of the node at `index`, root first.
    fn ancestor_ids(&self, index: usize) -> Vec<u64> {
        let snap = match self.snapshot.as_ref() {
            Some(s) => s,
            None => return Vec::new(),
        };
        let mut out: Vec<u64> = Vec::new();
        let mut parent = snap.nodes[index].parent;
        while let Some(p) = parent {
            if out.len() > snap.nodes.len() {
                break; // a cycle would be a walker bug, not a tree
            }
            match snap.nodes.iter().position(|n| n.id == p) {
                Some(i) => {
                    out.push(snap.nodes[i].id);
                    parent = snap.nodes[i].parent;
                }
                None => break, // a parent we cannot find is a stale snapshot
            }
        }
        out.reverse();
        out
    }

    /// The selected node's ancestor chain ending with the node itself, root
    /// first — the breadcrumb `html › body › div#main`.
    pub fn crumb(&self) -> Vec<(u64, String)> {
        let index = match self.selected.and_then(|id| self.index_of(id)) {
            Some(i) => i,
            None => return Vec::new(),
        };
        let mut ids = self.ancestor_ids(index);
        let last = match self.node_at(index) {
            Some(n) => n.id,
            None => return Vec::new(),
        };
        ids.push(last);
        ids.into_iter().filter_map(|id| self.id_label(id)).collect()
    }

    fn id_label(&self, id: u64) -> Option<(u64, String)> {
        let index = self.index_of(id)?;
        let n = self.node_at(index)?;
        Some((id, node_label(n)))
    }

    /// The breadcrumb as the pane wants it: one clickable entry per crumb with
    /// its name already split the way the tree splits it, so `div#main` shows a
    /// tag colour and an id/class colour instead of one flat label.
    pub fn crumb_items(&self) -> Vec<(u64, String, String)> {
        self.crumb()
            .into_iter()
            .filter_map(|(id, label)| {
                let index = self.index_of(id)?;
                let n = self.node_at(index)?;
                if n.kind == InspectKind::Element {
                    let ident = label.strip_prefix(n.tag.as_str()).unwrap_or("").to_string();
                    Some((id, n.tag.clone(), ident))
                } else {
                    Some((id, label, String::new()))
                }
            })
            .collect()
    }

    /// How many subtrees the reader folded shut. Together with the generation,
    /// node count and selection this says whether the rendered tree is still
    /// current (see `app::Core::write_devtools`).
    pub fn collapsed_len(&self) -> usize {
        self.collapsed.len()
    }

    /// What copying this node's row yields (task: copy in the DevTools tabs):
    /// an element copies its **subtree's markup** — opening tag with its
    /// attributes, every descendant on its own indented line, closing tag —;
    /// a text or comment node copies its content; `#document` copies the
    /// whole document. Captured text arrives already truncated with a
    /// trailing `…` — that is the snapshot's bound, and the copy does not
    /// pretend otherwise. Folding is irrelevant: the copy takes the subtree
    /// from the snapshot, not from what the pane happens to show.
    pub fn copy_text(&self, node: u64) -> Option<String> {
        let snap = self.snapshot.as_ref()?;
        let start = snap.nodes.iter().position(|n| n.id == node)?;
        // The picked node paints flush-left: an element copy starts with its
        // own `<tag>` at column 0. A `Document` has no tag of its own, so its
        // children start the text instead.
        let root_depth = snap.nodes[start].depth
            + if snap.nodes[start].kind == InspectKind::Document { 1 } else { 0 };
        let mut out = String::new();
        render_copy_markup(&snap.nodes, start, root_depth, &mut out);
        Some(out)
    }

    /// The breadcrumb with its own syntax colours: `div#main` is a tag token
    /// plus an id/class token, not one flat string.
    pub fn crumb_path(&self) -> Vec<Tok> {
        let mut out: Vec<Tok> = Vec::new();
        for (id, label) in self.crumb() {
            let n = match self.index_of(id).and_then(|i| self.node_at(i)) {
                Some(n) => n,
                None => continue,
            };
            if n.kind == InspectKind::Element {
                out.push(tok(n.tag.clone(), if tag_is_dangerous(&n.tag) { TOK_DANGER } else { TOK_TAG }));
                if let Some(rest) = label.strip_prefix(n.tag.as_str()) {
                    if !rest.is_empty() {
                        out.push(tok(rest.to_string(), TOK_IDENT));
                    }
                }
            } else {
                out.push(tok(label, TOK_PUNCT));
            }
        }
        out
    }

    /// The tree as visible rows, in document order: a node appears when none of
    /// its ancestors is folded. Whitespace-only text nodes never get a row (the
    /// tab does not send them; a stray one would be an invisible row a reader
    /// could click).
    pub fn rows(&self) -> Vec<InspectorRow> {
        let snap = match self.snapshot.as_ref() {
            Some(s) => s,
            None => return Vec::new(),
        };
        let mut rows: Vec<InspectorRow> = Vec::new();
        // While set, every node at least this deep belongs to a folded subtree.
        let mut hide_below: Option<usize> = None;
        for (i, n) in snap.nodes.iter().enumerate() {
            if let Some(depth) = hide_below {
                if n.depth >= depth {
                    continue;
                }
                hide_below = None;
            }
            let has_children = snap.nodes.get(i + 1).is_some_and(|nx| nx.depth > n.depth);
            if dom_is_text(n) && n.text.trim().is_empty() {
                continue;
            }
            let collapsed = has_children && self.collapsed.contains(&n.id);
            if collapsed {
                hide_below = Some(n.depth + 1);
            }
            rows.push(InspectorRow {
                node: n.id,
                depth: n.depth,
                toks: node_tokens(n, has_children),
                has_children,
                collapsed,
                selected: self.selected == Some(n.id),
                italic: matches!(n.kind, InspectKind::Comment | InspectKind::Doctype),
            });
        }
        rows
    }

    /// What the pane says when there is nothing to show — never a blank
    /// rectangle, always the actual reason.
    pub fn empty_hint(&self) -> String {
        if self.pending {
            "asking the tab for its document tree \u{2026}".into()
        } else if self.snapshot.is_none() {
            "no document tree yet \u{2014} the tab snapshots when this pane opens".into()
        } else {
            "the tab sent a tree with no nodes".into()
        }
    }

    /// The pane's header line: `Inspector — <title> · 7 element(s)`, with a
    /// truncation note when the tree was cut short. Empty with no tree, so the
    /// .slint shows only the dim hint then.
    pub fn summary(&self, heading: Option<&str>) -> String {
        if self.snapshot.is_none() {
            return String::new();
        }
        let address = self.address().filter(|u| !u.trim().is_empty());
        let mut s = match heading.filter(|t| !t.trim().is_empty()).or(address) {
            Some(t) => format!("Inspector \u{2014} {t}"),
            None => "Inspector".into(),
        };
        s.push_str(&format!(" \u{b7} {} element(s)", self.element_count()));
        if self.truncated() {
            s.push_str(" \u{b7} truncated");
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironsurf_common::TabId;

    #[test]
    fn reduce_full_message_sequence() {
        let mut s = TabState::default();
        assert!(s.reduce(&FromTab::Ready { tab_id: TabId(1) }));
        assert!(s.ready);

        assert!(s.reduce(&FromTab::UrlChanged { url: Some("https://example.com".into()) }));
        assert!(s.reduce(&FromTab::LoadingChanged { loading: true }));
        assert!(!s.reduce(&FromTab::LoadingChanged { loading: true }), "no-op must report false");
        assert!(s.reduce(&FromTab::TitleChanged { title: Some("Example".into()) }));
        assert!(s.reduce(&FromTab::HistoryChanged { can_go_back: true, can_go_forward: false }));
        assert!(s.reduce(&FromTab::LoadingChanged { loading: false }));
        assert!(s.reduce(&FromTab::StatusText { text: Some("Waiting…".into()) }));
        // frames carry no state
        assert!(!s.reduce(&FromTab::Frame {
            width: 2,
            height: 2,
            scale: 1.0,
            rgba: vec![0u8; 16],
        }));

        assert_eq!(s.title, "Example");
        assert_eq!(s.url.as_deref(), Some("https://example.com"));
        assert!(!s.loading);
        assert!(s.can_back);
        assert!(!s.can_forward);
        assert_eq!(s.status.as_deref(), Some("Waiting…"));

        assert!(s.reduce(&FromTab::Closed));
        assert!(s.closed);
        assert!(!s.reduce(&FromTab::Closed));
    }

    #[test]
    fn starting_a_load_drops_the_stale_error_so_status_text_shows() {
        let mut s = TabState::default();
        s.reduce(&FromTab::Error { message: "dns failed".into() });
        assert_eq!(s.error.as_deref(), Some("dns failed"));
        // The user clicks a link / retries: the new load must clear the bar of
        // the old failure, or "Waiting for …" is invisible behind it forever.
        assert!(s.reduce(&FromTab::LoadingChanged { loading: true }));
        assert_eq!(s.error, None, "a superseded error must not shadow the new status");
        assert!(s.loading);
        assert!(s.reduce(&FromTab::StatusText {
            text: Some("Waiting for example.com".into())
        }));
        assert_eq!(s.status.as_deref(), Some("Waiting for example.com"));
        // …and the status is replaced, never appended to.
        assert!(s.reduce(&FromTab::StatusText { text: None }));
        assert_eq!(s.status, None);
        assert!(!s.reduce(&FromTab::StatusText { text: None }));
    }

    #[test]
    fn error_clears_loading_and_records_message() {
        let mut s = TabState::default();
        s.reduce(&FromTab::LoadingChanged { loading: true });
        assert!(s.reduce(&FromTab::Error { message: "boom".into() }));
        assert_eq!(s.error.as_deref(), Some("boom"));
        assert!(!s.loading);
    }

    #[test]
    fn console_logs_append_in_order_and_never_dirty_the_frame() {
        let mut s = TabState::default();
        assert!(s.console.is_empty());
        for (i, src) in ["js", "css", "dom", "page"].iter().enumerate() {
            assert!(
                !s.reduce(&FromTab::ConsoleLog {
                    source: (*src).into(),
                    level: "warn".into(),
                    message: format!("m{i}"),
                }),
                "console logs must not dirty the frame"
            );
        }
        // Appended in arrival order, all fields carried through verbatim.
        assert_eq!(s.console.len(), 4);
        assert_eq!(
            s.console,
            vec![
                ConsoleEntry {
                    source: "js".into(),
                    level: "warn".into(),
                    message: "m0".into()
                },
                ConsoleEntry {
                    source: "css".into(),
                    level: "warn".into(),
                    message: "m1".into()
                },
                ConsoleEntry {
                    source: "dom".into(),
                    level: "warn".into(),
                    message: "m2".into()
                },
                ConsoleEntry {
                    source: "page".into(),
                    level: "warn".into(),
                    message: "m3".into()
                },
            ]
        );
    }

    #[test]
    fn console_log_ring_drops_oldest_at_the_cap() {
        let mut s = TabState::default();
        for i in 0..(MAX_CONSOLE_ENTRIES + 25) {
            s.reduce(&FromTab::ConsoleLog {
                source: "js".into(),
                level: "log".into(),
                message: format!("{i}"),
            });
        }
        assert_eq!(s.console.len(), MAX_CONSOLE_ENTRIES, "cap must be enforced");
        // The 25 oldest were dropped; the tail is the freshest entries.
        assert_eq!(s.console.first().unwrap().message, "25");
        assert_eq!(
            s.console.last().unwrap().message,
            (MAX_CONSOLE_ENTRIES + 24).to_string()
        );
    }

    #[test]
    fn network_events_append_and_never_dirty_the_frame() {
        let mut s = TabState::default();
        assert!(s.network.is_empty());
        // A success event, then a failure event — both must land on the log.
        assert!(
            !s.reduce(&FromTab::NetworkEvent {
                event: net_event("GET", "https://example.com", Some(200), None)
            }),
            "network events must not dirty the frame"
        );
        assert!(!s.reduce(&FromTab::NetworkEvent {
            event: net_event("GET", "https://evil.test", None, Some("dns failed".into()))
        }));
        assert_eq!(s.network.len(), 2);
        assert_eq!(s.network[0].url, "https://example.com");
        assert_eq!(s.network[0].status, Some(200));
        assert_eq!(s.network[0].error, None);
        assert_eq!(s.network[1].status, None);
        assert_eq!(s.network[1].error.as_deref(), Some("dns failed"));
    }

    #[test]
    fn network_log_ring_drops_oldest_at_the_cap() {
        let mut s = TabState::default();
        for i in 0..(MAX_NETWORK_ENTRIES + 10) {
            s.reduce(&FromTab::NetworkEvent {
                event: net_event("GET", &format!("https://e.test/{i}"), Some(200), None),
            });
        }
        assert_eq!(s.network.len(), MAX_NETWORK_ENTRIES, "cap must be enforced");
        assert_eq!(s.network.first().unwrap().url, "https://e.test/10");
        assert_eq!(
            s.network.last().unwrap().url,
            format!("https://e.test/{}", MAX_NETWORK_ENTRIES + 9)
        );
    }

    #[test]
    fn devtools_feed_runs_alongside_normal_state() {
        // DevTools messages must leave the rest of the reduce behaviour intact
        // (title/loading/status unchanged) while still accumulating.
        let mut s = TabState::default();
        s.reduce(&FromTab::Ready { tab_id: TabId(1) });
        s.reduce(&FromTab::UrlChanged { url: Some("https://example.com".into()) });
        s.reduce(&FromTab::ConsoleLog {
            source: "js".into(),
            level: "error".into(),
            message: "boom".into(),
        });
        s.reduce(&FromTab::NetworkEvent {
            event: net_event("POST", "https://example.com/api", Some(204), None),
        });
        assert!(s.ready);
        assert_eq!(s.url.as_deref(), Some("https://example.com"));
        assert_eq!(s.console.len(), 1);
        assert_eq!(s.network.len(), 1);
        assert_eq!(s.network[0].method, "POST");
    }

    fn net_event(
        method: &str,
        url: &str,
        status: Option<u16>,
        error: Option<String>,
    ) -> ironsurf_common::NetEvent {
        ironsurf_common::NetEvent {
            method: method.into(),
            url: url.into(),
            status,
            response_bytes: status.map(|_| 1234),
            duration_ms: status.map(|_| 42),
            error,
            initiator: Some("document".into()),
        }
    }

    #[test]
    fn console_rows_carry_the_level_and_a_bracketed_source() {
        let mut s = TabState::default();
        s.reduce(&FromTab::ConsoleLog {
            source: "js".into(),
            level: "error".into(),
            message: "boom".into(),
        });
        s.reduce(&FromTab::ConsoleLog {
            source: "css".into(),
            level: "warn".into(),
            message: "bad selector".into(),
        });
        // Userscript entries (task: userscript menu): the tab attributes them
        // with the script's name in the message — the panel renders both.
        s.reduce(&FromTab::ConsoleLog {
            source: "userscript".into(),
            level: "log".into(),
            message: "Darkify: hello from the script".into(),
        });
        assert_eq!(
            s.console_rows(),
            vec![
                ConsoleRow { level: "error".into(), text: "[js] boom".into() },
                ConsoleRow { level: "warn".into(), text: "[css] bad selector".into() },
                ConsoleRow {
                    level: "log".into(),
                    text: "[userscript] Darkify: hello from the script".into(),
                },
            ]
        );
    }

    #[test]
    fn net_rows_pre_format_every_column() {
        let mut s = TabState::default();
        s.reduce(&FromTab::NetworkEvent {
            event: net_event("GET", "https://example.com", Some(200), None),
        });
        s.reduce(&FromTab::NetworkEvent {
            event: ironsurf_common::NetEvent {
                method: "POST".into(),
                url: "https://api.example/upload".into(),
                status: None,
                response_bytes: None,
                duration_ms: Some(1500),
                error: Some("dns failed".into()),
                initiator: Some("xhr".into()),
            },
        });
        let rows = s.net_rows();
        assert_eq!(
            rows[0],
            NetRow {
                method: "GET".into(),
                url: "https://example.com".into(),
                initiator: "document".into(),
                status: "200".into(),
                // the fixture event is 1234 B / 42 ms
                size: "1.2 kB".into(),
                time: "42 ms".into(),
                failed: false,
            }
        );
        assert_eq!(rows[1].initiator, "xhr");
        // A failure speaks for itself in the status column, in red.
        assert_eq!(rows[1].status, "dns failed");
        assert!(rows[1].failed);
        assert_eq!(rows[1].size, MISSING);
        assert_eq!(rows[1].time, "1.50 s");
    }

    #[test]
    fn panel_formatters_cover_missing_values_and_units() {
        assert_eq!(format_size(None), MISSING);
        assert_eq!(format_size(Some(0)), "0 B");
        assert_eq!(format_size(Some(999)), "999 B");
        assert_eq!(format_size(Some(1024)), "1.0 kB");
        assert_eq!(format_size(Some(5 * 1024 * 1024)), "5.0 MB");
        assert_eq!(format_duration(None), MISSING);
        assert_eq!(format_duration(Some(0)), "0 ms");
        assert_eq!(format_duration(Some(999)), "999 ms");
        assert_eq!(format_duration(Some(1000)), "1.00 s");
    }

    #[test]
    fn cursor_changes_are_carried_without_a_repaint() {
        // Task: hover cursor. The shape lives on the record (tab switches
        // re-arm it from here) but never dirties a frame — the pump applies
        // it to the window directly.
        let mut s = TabState::default();
        assert_eq!(s.cursor, CursorIcon::Default, "a fresh tab starts with the arrow");
        assert!(!s.reduce(&FromTab::CursorChanged { cursor: CursorIcon::Pointer }));
        assert_eq!(s.cursor, CursorIcon::Pointer);
        assert!(!s.reduce(&FromTab::CursorChanged { cursor: CursorIcon::Pointer }));
        assert_eq!(s.cursor, CursorIcon::Pointer, "the same shape again changes nothing");
        assert!(!s.reduce(&FromTab::CursorChanged { cursor: CursorIcon::Text }));
        assert_eq!(s.cursor, CursorIcon::Text);
    }

    #[test]
    fn find_results_drive_the_bar_counter() {
        let mut s = TabState::default();
        assert_eq!(s.find.label(), "", "no query, no counter");
        assert!(s.reduce(&FromTab::FindResult {
            query: "the".into(),
            matches: 12,
            active: 1
        }));
        assert_eq!(s.find.label(), "1 / 12");
        assert!(
            !s.reduce(&FromTab::FindResult {
                query: "the".into(),
                matches: 12,
                active: 1
            }),
            "the same numbers are not a change"
        );
        assert!(s.reduce(&FromTab::FindResult {
            query: "the".into(),
            matches: 12,
            active: 7
        }));
        assert_eq!(s.find.label(), "7 / 12");
        // A query that matches nothing says so plainly, and an emptied bar
        // leaves no counter behind.
        assert!(s.reduce(&FromTab::FindResult {
            query: "zzz".into(),
            matches: 0,
            active: 0
        }));
        assert_eq!(s.find.label(), "No results");
        assert!(s.reduce(&FromTab::FindResult {
            query: String::new(),
            matches: 0,
            active: 0
        }));
        assert_eq!(s.find.label(), "");
    }

    #[test]
    fn security_chips() {
        assert_eq!(Security::of("https://example.com"), Security::Https);
        assert_eq!(Security::of("http://example.com"), Security::InsecureHttp);
        assert_eq!(Security::of("about:blank"), Security::None);
        assert_eq!(Security::of(""), Security::None);
        assert_eq!(Security::Https.render().0, "⛬");
        assert!(Security::InsecureHttp.render().0.contains("http"));
    }

    // ---- Inspector pane view-model ----------------------------------------

    fn ins(
        id: u64,
        parent: Option<u64>,
        depth: usize,
        kind: InspectKind,
        tag: &str,
        text: &str,
        attrs: &[(&str, &str)],
    ) -> InspectNode {
        InspectNode {
            id,
            parent,
            kind,
            tag: tag.into(),
            attrs: attrs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            text: text.into(),
            depth,
            bounds: None,
        }
    }

    /// The tree every Inspector test reads: a document, its doctype, a
    /// `<head>` with a title, a `<body id=main class="page home">` holding a
    /// `<div data-src="data:…">` with text, a comment, and a `<script>`.
    fn fixture_tree() -> InspectSnapshot {
        InspectSnapshot {
            generation: 1,
            url: "https://example.com/".into(),
            selected: Some(6),
            truncated: false,
            nodes: vec![
                ins(0, None, 0, InspectKind::Document, "", "", &[]),
                ins(1, Some(0), 1, InspectKind::Doctype, "", "html", &[]),
                ins(2, Some(0), 1, InspectKind::Element, "html", "", &[]),
                ins(3, Some(2), 2, InspectKind::Element, "head", "", &[]),
                ins(4, Some(3), 3, InspectKind::Element, "title", "", &[]),
                ins(5, Some(4), 4, InspectKind::Text, "", "Inspector fixture", &[]),
                ins(6, Some(2), 2, InspectKind::Element, "body", "", &[("id", "main"), ("class", "page home")]),
                ins(7, Some(6), 3, InspectKind::Element, "div", "", &[("data-src", "data:image/png;base64,AAAA")]),
                ins(8, Some(7), 4, InspectKind::Text, "", "hello", &[]),
                ins(9, Some(6), 3, InspectKind::Comment, "", " keep the footer ", &[]),
                ins(10, Some(2), 2, InspectKind::Element, "script", "", &[("src", "app.js")]),
            ],
        }
    }

    fn ui_with_tree() -> InspectorUi {
        let mut ui = InspectorUi::default();
        ui.expect_tree();
        ui.set_snapshot(fixture_tree());
        ui
    }

    // ---- copy (task: copy in the DevTools tabs) ---------------------------

    #[test]
    fn copying_a_node_yields_its_markup_flush_left() {
        let ui = ui_with_tree();
        // a text node is just its (trimmed) text
        assert_eq!(ui.copy_text(8).unwrap(), "hello\n");
        // an element with children: open tag, indented contents, close tag
        assert_eq!(
            ui.copy_text(7).unwrap(),
            "<div data-src=\"data:image/png;base64,AAAA\">\n  hello\n</div>\n"
        );
        // a leaf shows as the row shows it — `<tag></tag>` on one line
        assert_eq!(ui.copy_text(10).unwrap(), "<script src=\"app.js\"></script>\n");
    }

    #[test]
    fn copying_a_subtree_indents_one_level_per_depth_under_its_root() {
        let ui = ui_with_tree();
        assert_eq!(
            ui.copy_text(6).unwrap(),
            "\
<body id=\"main\" class=\"page home\">
  <div data-src=\"data:image/png;base64,AAAA\">
    hello
  </div>
  <!-- keep the footer -->
</body>
"
        );
        // the whole document starts flush with the doctype, its children too
        let all = ui.copy_text(0).unwrap();
        assert!(all.starts_with("<!DOCTYPE html>\n<html>\n"), "{all}");
    }

    #[test]
    fn copy_ignores_folds_a_folded_subtree_still_copies_whole() {
        let mut ui = ui_with_tree();
        let before = ui.copy_text(6).unwrap();
        assert!(ui.toggle_fold(6), "body is foldable");
        assert!(
            ui.rows().iter().all(|r| r.node != 7),
            "the fold hid the div from the tree"
        );
        assert_eq!(ui.copy_text(6).unwrap(), before, "the buffer copies the document, not the view");
    }

    #[test]
    fn copying_a_network_row_joins_its_columns_and_skips_dashes() {
        let mut s = TabState::default();
        s.reduce(&FromTab::NetworkEvent {
            event: net_event("GET", "https://example.com", Some(200), None),
        });
        s.reduce(&FromTab::NetworkEvent {
            event: ironsurf_common::NetEvent {
                method: "POST".into(),
                url: "https://api.example/upload".into(),
                status: None,
                response_bytes: None,
                duration_ms: Some(1500),
                error: Some("dns failed".into()),
                initiator: Some("xhr".into()),
            },
        });
        let rows = s.net_rows();
        assert_eq!(
            net_row_clipboard(&rows[0]),
            "GET https://example.com document 200 1.2 kB 42 ms"
        );
        // the failed request has no size — the dash is not pasted
        assert_eq!(
            net_row_clipboard(&rows[1]),
            "POST https://api.example/upload xhr dns failed 1.50 s"
        );
    }

    #[test]
    fn dom_helpers_split_the_kinds_and_count_elements() {
        let snap = fixture_tree();
        let elements = snap.nodes.iter().filter(|n| dom_is_element(n)).count();
        assert_eq!(elements, 6, "html head title body div script");
        assert_eq!(dom_element_count(&snap.nodes), elements);
        assert!(dom_is_text(&snap.nodes[5]));
        assert!(dom_is_other(&snap.nodes[0]) && dom_is_other(&snap.nodes[1]) && dom_is_other(&snap.nodes[9]));
        assert!(!dom_is_other(&snap.nodes[6]));
    }

    #[test]
    fn a_row_is_markup_broken_into_colour_kinds() {
        let body = &fixture_tree().nodes[6];
        let toks = node_tokens(body, true);
        let kinds: Vec<( &str, u8)> = toks.iter().map(|(t, k)| (t.as_str(), *k)).collect();
        assert_eq!(
            kinds,
            vec![
                ("<", TOK_PUNCT),
                ("body", TOK_TAG),
                (" id", TOK_ATTR),
                ("=", TOK_PUNCT),
                ("\"", TOK_PUNCT),
                ("main", TOK_IDENT), // `id`/`class` values get their own accent
                ("\"", TOK_PUNCT),
                (" class", TOK_ATTR),
                ("=", TOK_PUNCT),
                ("\"", TOK_PUNCT),
                ("page home", TOK_IDENT),
                ("\"", TOK_PUNCT),
                (">", TOK_PUNCT),
            ],
            "an element with children stops at the closing angle bracket"
        );

        // An empty element shows its closing tag; a `<script>` is a danger colour.
        let script = node_tokens(&fixture_tree().nodes[10], false);
        let rendered: String = script.iter().map(|(t, _)| t.as_str()).collect();
        assert_eq!(rendered, "<script src=\"app.js\"></script>");
        assert_eq!(script[1].1, TOK_DANGER, "the tag name of <script> is red");
        assert_eq!(script[2].1, TOK_ATTR);
        assert_eq!(script[5].1, TOK_STR, "an ordinary value stays green");

        // …and so is a `data:` URL, wherever it hides.
        let div = node_tokens(&fixture_tree().nodes[7], true);
        assert!(
            div.iter().any(|(t, k)| t == "data:image/png;base64,AAAA" && *k == TOK_DANGER),
            "data: URLs are painted in the danger colour: {div:?}"
        );
    }

    #[test]
    fn comments_doctypes_and_text_get_their_own_colours() {
        let snap = fixture_tree();
        let comment = node_tokens(&snap.nodes[9], false);
        assert_eq!(
            comment.iter().map(|(t, k)| (t.as_str(), *k)).collect::<Vec<_>>(),
            vec![("<!--", TOK_PUNCT), (" keep the footer ", TOK_COMMENT), ("-->", TOK_PUNCT)]
        );
        let doctype = node_tokens(&snap.nodes[1], false);
        assert_eq!(
            doctype.iter().map(|(t, k)| (t.as_str(), *k)).collect::<Vec<_>>(),
            vec![("<!", TOK_PUNCT), ("html", TOK_DOCTYPE), (">", TOK_PUNCT)]
        );
        let text = node_tokens(&snap.nodes[8], false);
        assert_eq!(text, vec![("hello".to_string(), TOK_TEXT)]);
    }

    #[test]
    fn folding_is_a_shell_side_view_over_the_flat_list() {
        let mut ui = ui_with_tree();
        assert_eq!(ui.rows().len(), 11, "nothing folded: every node is a row");
        let twisties: Vec<u64> = ui.rows().iter().filter(|r| r.has_children).map(|r| r.node).collect();
        assert_eq!(twisties, vec![0, 2, 3, 4, 6, 7], "leaves have no +/−");

        assert!(ui.toggle_fold(6), "body folds");
        let rows = ui.rows();
        assert_eq!(rows.len(), 8, "body's three descendants disappear");
        assert_eq!(rows.iter().find(|r| r.node == 6).map(|r| (r.collapsed, r.has_children)), Some((true, true)));
        assert!(!rows.iter().any(|r| r.node == 7 || r.node == 8 || r.node == 9));

        assert!(!ui.toggle_fold(6), "the same twisty unfolds");
        assert_eq!(ui.rows().len(), 11);
        // Folding a leaf is not a thing, and a stale id folds nothing.
        assert!(!ui.toggle_fold(5));
        assert!(!ui.toggle_fold(999));
    }

    #[test]
    fn selecting_a_row_rewrites_the_breadcrumb_and_unfolds_it() {
        let mut ui = ui_with_tree();
        // The tab's own selection arrives with the snapshot.
        assert_eq!(ui.selected(), Some(6));
        let crumb = ui.crumb();
        let labels: Vec<&str> = crumb.iter().map(|(_, l)| l.as_str()).collect();
        assert_eq!(labels, ["#document", "html", "body#main"], "root first");

        ui.select(8);
        let labels: Vec<String> = ui.crumb().into_iter().map(|(_, l)| l).collect();
        assert_eq!(labels, ["#document", "html", "body#main", "div", "#text"]);

        // A folded ancestor opens up, or the reader would have selected a row
        // they cannot see.
        ui.toggle_fold(2);
        assert!(ui.rows().iter().all(|r| r.node <= 2));
        ui.select(7);
        assert!(ui.rows().iter().any(|r| r.node == 7), "html unfolded for the selection");
        assert!(ui.rows().iter().find(|r| r.node == 7).map(|r| r.selected).unwrap_or(false));

        // An id the snapshot does not contain is a stale tree, never a guess.
        ui.select(4242);
        assert_eq!(ui.selected(), Some(7));
        assert_eq!(ui.crumb().len(), 4);
    }

    #[test]
    fn the_breadcrumb_keeps_the_syntax_colours() {
        let ui = ui_with_tree();
        let path = ui.crumb_path();
        let rendered: String = path.iter().map(|(t, _)| t.as_str()).collect();
        assert_eq!(rendered, "#documenthtmlbody#main");
        assert_eq!(path[1], ("html".to_string(), TOK_TAG));
        assert_eq!(path[2], ("body".to_string(), TOK_TAG));
        assert_eq!(path[3], ("#main".to_string(), TOK_IDENT), "the id is its own token");
    }

    #[test]
    fn a_node_without_an_id_is_labelled_by_its_first_class() {
        let mut ui = ui_with_tree();
        ui.select(7);
        assert_eq!(ui.crumb().last().map(|(_, l)| l.as_str()), Some("div"));
        let mut div_class = fixture_tree();
        div_class.nodes[7].attrs.push(("class".into(), "card wide".into()));
        ui.set_snapshot(div_class);
        ui.select(7);
        assert_eq!(ui.crumb().last().map(|(_, l)| l.as_str()), Some("div.card"));
    }

    #[test]
    fn a_new_generation_replaces_the_selection_the_tab_chose() {
        let mut ui = ui_with_tree();
        ui.select(8);
        let mut next = fixture_tree();
        next.generation = 2;
        next.selected = Some(10);
        next.nodes.truncate(4); // a shorter document: old ids are now nothing
        ui.set_snapshot(next);
        assert_eq!(ui.selected(), Some(10), "the tab's selection, not a stale id");
        assert_eq!(ui.generation(), Some(2));
        assert_eq!(ui.element_count(), 2, "html head");
        assert_eq!(ui.rows().len(), 4);
    }

    #[test]
    fn counts_and_the_truncation_notice_come_from_the_snapshot() {
        let mut ui = ui_with_tree();
        assert_eq!(ui.node_count(), 11);
        assert_eq!(ui.element_count(), 6, "the pane counts elements, not text nodes");
        assert!(!ui.truncated());
        assert!(ui.summary(Some("Fixture")).contains("6 element(s)"));

        let mut cut = fixture_tree();
        cut.truncated = true;
        ui.set_snapshot(cut);
        assert!(ui.truncated());
        assert!(ui.summary(None).ends_with("truncated"), "{}", ui.summary(None));
    }

    #[test]
    fn the_pane_never_shows_a_blank_rectangle() {
        let mut ui = InspectorUi::default();
        assert!(ui.rows().is_empty());
        assert!(ui.summary(None).is_empty());
        let hint = ui.empty_hint();
        assert!(hint.contains("no document tree yet"), "{hint}");

        ui.expect_tree();
        assert!(ui.pending());
        assert!(ui.empty_hint().contains("asking the tab"), "{}", ui.empty_hint());

        ui.set_snapshot(InspectSnapshot { generation: 3, url: "about:blank".into(), nodes: vec![], selected: None, truncated: false });
        assert!(!ui.pending(), "the snapshot answered");
        assert!(ui.empty_hint().contains("no nodes"), "{}", ui.empty_hint());

        // Closing the pane keeps ⌖ but drops the tree.
        ui.set_snapshot(fixture_tree());
        ui.set_pick(true);
        ui.clear_tree();
        assert!(ui.pending() && ui.snapshot().is_none() && ui.pick());
    }

    #[test]
    fn a_page_side_selection_is_a_row_the_pane_owes_a_scroll_to() {
        // The view-model has no geometry, so what it owes a scroll is a *row*, and
        // a count that says which request it is (see `reveal_request`).
        let mut ui = ui_with_tree();
        let body = ui.row_index_of(6).expect("node 6 has a row");
        let owed = ui.reveal_request().expect("the snapshot named a selection");
        assert_eq!(owed.0, body, "the row of the node the page named");

        // Picking the same node again is a request all over again: same row, later
        // count, or a pane already showing it would sit still.
        ui.select_from_page(6);
        let again = ui.reveal_request().expect("still owed");
        assert_eq!((again.0, owed.1 < again.1), (body, true), "same row, count gone up");

        // A row click is the reader arriving where they wanted, and a fold is the
        // reader arranging the tree: in both cases the page's request is dropped,
        // so the pane never drags anyone back to where the page once pointed.
        ui.select(7);
        assert_eq!(ui.reveal_request(), None, "a row click owes no scroll");
        ui.select_from_page(6);
        assert!(ui.toggle_fold(6), "body folds");
        assert_eq!(ui.reveal_request(), None, "and the request goes with the fold");
        assert_eq!(ui.row_index_of(7), None, "folding takes the subtree's rows away");
        ui.toggle_fold(6);

        // Rearming the pane — the tree has just come on screen — asks for nothing
        // while nothing is owed, and another turn of the same row when it is.
        let quiet = ui.reveal_request();
        ui.rearm_reveal();
        assert_eq!(quiet, None, "a fold, then nothing owed");
        assert_eq!(ui.reveal_request(), quiet, "so rearming asks for nothing");
        ui.select_from_page(6);
        let t = ui.reveal_request().expect("owed").1;
        ui.rearm_reveal();
        assert_eq!(ui.reveal_request(), Some((body, t + 1)), "a second ask for the same row");
    }

    #[test]
    fn whitespace_only_text_never_earns_a_row() {
        let mut snap = fixture_tree();
        // The tab drops these; a stray one still must not be an invisible row.
        snap.nodes.push(ins(11, Some(6), 3, InspectKind::Text, "", "  \n ", &[]));
        let mut ui = InspectorUi::default();
        ui.set_snapshot(snap);
        assert_eq!(ui.rows().len(), 11);
        assert!(!ui.rows().iter().any(|r| r.node == 11));
    }
    // ---- downloads (task: file downloads) ---------------------------------

    fn hint(id: u64, name: Option<&str>) -> DownloadHint {
        DownloadHint {
            id,
            url: "http://example.com/files/report.zip".to_owned(),
            name: name.map(|n| n.to_owned()),
            media_type: Some("application/zip".to_owned()),
            trigger: DownloadTrigger::Disposition,
        }
    }

    fn path(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn a_download_is_filed_named_and_settled() {
        let tab = TabId(1);
        let mut store = DownloadStore::default();
        let row = store.start(tab, &hint(7, Some("report.zip")), "report.zip".to_owned(), Some(path("/d/report.zip")), 1);
        assert_eq!((row.state, row.bytes, row.path.as_ref()), (DownloadState::Saving, 0, None));
        // the file is known while it streams, and progress counts real bytes
        assert_eq!(store.target(tab, 7), Some(path("/d/report.zip")));
        assert_eq!(store.add_bytes(tab, 7, 4096), Some(path("/d/report.zip")));
        let done = store.finish(tab, 7, None).expect("settles");
        assert_eq!(done.row.state, DownloadState::Done);
        assert_eq!((done.row.bytes, done.delete), (4096, None));
        assert_eq!(store.target(tab, 7), None, "the file rides on the row now");
        assert_eq!(store.files(), vec![path("/d/report.zip")]);
        // a late chunk has no row and no file to trust
        assert_eq!(store.add_bytes(tab, 7, 4), None);
        assert_eq!(store.finish(tab, 7, None).map(|_| ()), None);
    }

    #[test]
    fn a_failed_download_only_keeps_a_file_that_has_bytes() {
        let tab = TabId(2);
        let mut store = DownloadStore::default();
        // nothing landed: the empty file comes back for deletion
        store.start(tab, &hint(1, Some("big.iso")), "big.iso".to_owned(), Some(path("/d/big.iso")), 1);
        let failed = store.finish(tab, 1, Some("connection refused")).expect("settles");
        assert_eq!((failed.row.state, failed.row.path.as_ref(), failed.delete), (DownloadState::Failed, None, Some(path("/d/big.iso"))));
        assert!(failed.row.url.len() > 0, "a Failed row still says where it came from");
        assert_eq!(store.files(), Vec::<PathBuf>::new(), "nothing to show for a failed fetch");
        // half a file landed: the row keeps the name of a file that exists
        store.start(tab, &hint(2, Some("half.zip")), "half.zip".to_owned(), Some(path("/d/half.zip")), 2);
        store.add_bytes(tab, 2, 512);
        let partial = store.finish(tab, 2, Some("too large")).expect("settles");
        assert_eq!(
            (partial.row.state, partial.row.path, partial.delete),
            (DownloadState::Failed, Some(path("/d/half.zip")), None)
        );
    }

    #[test]
    fn rows_are_keyed_by_tab_and_process_death_reaps_them() {
        // two tabs may both mint id 1 — a row is keyed `(tab, id)`
        let (a, b) = (TabId(1), TabId(2));
        let mut store = DownloadStore::default();
        store.start(a, &hint(1, Some("a.txt")), "a.txt".to_owned(), Some(path("/d/a.txt")), 1);
        store.start(b, &hint(1, Some("b.txt")), "b.txt".to_owned(), Some(path("/d/b.txt")), 2);
        assert_eq!(store.add_bytes(a, 1, 8), Some(path("/d/a.txt")));
        assert_eq!(store.add_bytes(b, 1, 16), Some(path("/d/b.txt")));
        assert_eq!(store.rows().len(), 2);

        // the process dies mid-fetch: the row Failed, the partial file goes
        let orphans = store.reap(a);
        assert_eq!(orphans, vec![path("/d/a.txt")]);
        let row = store.rows().iter().find(|r| r.tab_id == a).expect("row stays");
        assert_eq!((row.state, row.bytes, row.path.as_ref()), (DownloadState::Failed, 8, None));
        assert_eq!(store.target(a, 1), None, "nothing streams into a dead file");
        assert_eq!(store.target(b, 1), Some(path("/d/b.txt")), "the other tab is untouched");
        assert_eq!(store.reap(a), Vec::<PathBuf>::new(), "reaping twice invents no file");
        store.finish(b, 1, None).expect("settles");
    }

    #[test]
    fn the_newest_row_leads_the_status_line() {
        let tab = TabId(1);
        let mut store = DownloadStore::default();
        assert_eq!(store.newest(), None, "a tab with no download says nothing");
        store.start(tab, &hint(1, Some("old.zip")), "old.zip".to_owned(), Some(path("/d/old.zip")), 1);
        store.finish(tab, 1, None).expect("settles");
        store.start(tab, &hint(2, Some("new.zip")), "new.zip".to_owned(), Some(path("/d/new.zip")), 2);
        // a live row outranks an older finished one no matter the stamp
        assert_eq!(store.newest().map(|r| (r.id, r.state)), Some((2, DownloadState::Saving)));
        store.finish(tab, 2, None).expect("settles");
        // two finished rows: the newer stamp wins
        assert_eq!(store.newest().map(|r| (r.id, r.state)), Some((2, DownloadState::Done)));
        store.start(tab, &hint(3, Some("third.zip")), "third.zip".to_owned(), None, 3);
        // a row whose file the disk refused is still the newest thing
        assert_eq!(store.newest().map(|r| (r.id, r.state)), Some((3, DownloadState::Saving)));
    }

    #[test]
    fn clear_finished_keeps_files_and_the_running_rows() {
        let tab = TabId(1);
        let mut store = DownloadStore::default();
        store.start(tab, &hint(1, Some("a.zip")), "a.zip".to_owned(), Some(path("/d/a.zip")), 1);
        store.finish(tab, 1, None).expect("settles");
        store.start(tab, &hint(2, Some("b.zip")), "b.zip".to_owned(), Some(path("/d/b.zip")), 2);
        store.start(tab, &hint(3, Some("c.zip")), "c.zip".to_owned(), Some(path("/d/c.zip")), 3);
        store.finish(tab, 3, Some("boom")).expect("settles");
        let dropped = store.clear_finished();
        assert_eq!(dropped.iter().map(|r| r.id).collect::<Vec<_>>(), vec![1, 3]);
        assert_eq!(store.rows().iter().map(|r| r.id).collect::<Vec<_>>(), vec![2], "only what is still running");
        // clearing the list is not deleting what it listed: both files stay,
        // one of them a Failed row's because half a file is still a file
        assert_eq!(
            dropped.iter().filter_map(|r| r.path.clone()).count(),
            1,
            "the Failed row lost its file with the error and says so with no path"
        );
        assert_eq!(store.target(tab, 2), Some(path("/d/b.zip")));
        assert_eq!(store.clear_finished().len(), 0, "nothing finished left to drop");
    }

    #[test]
    fn the_cap_drops_finished_rows_never_a_live_one() {
        let tab = TabId(1);
        let mut store = DownloadStore::default();
        for i in 0..(MAX_DOWNLOAD_ROWS + 3) as u64 {
            store.start(tab, &hint(i, Some("x.bin")), "x.bin".to_owned(), Some(path("/d/x.bin")), i);
            store.finish(tab, i, None).expect("settles");
        }
        // a live row past the cap survives the next arrival
        store.start(tab, &hint(999, Some("live.zip")), "live.zip".to_owned(), Some(path("/d/live.zip")), 900);
        assert_eq!(store.rows().len(), MAX_DOWNLOAD_ROWS);
        assert!(store.rows().iter().any(|r| r.id == 999), "the newest row is never dropped");
        assert_eq!(store.target(tab, 999), Some(path("/d/live.zip")), "its file stays known");
        // three rows went out with the last finished arrival, one more with
        // this one — the oldest finished rows, in arrival order
        assert_eq!(store.rows().first().map(|r| r.id), Some(4));
        assert_eq!(store.target(tab, 0), None, "a dropped row's file handle goes with it");
    }

    // ---- overlay scroll bar (task: scroll bar) --------------------------------

    fn geometry(y: f32, range: f32) -> ScrollState {
        ScrollState { y, range, viewport_h: 600. }
    }

    #[test]
    fn a_page_that_fits_its_window_gets_no_bar() {
        // Nothing to point at: the document is its viewport, so a bar would
        // claim the page has places the reader has not seen.
        assert_eq!(scrollbar_thumb(&geometry(0., 0.), 600.), None);
        // …and a track with no room in it draws nothing either (the window is
        // not laid out yet, or the page area is a sliver).
        assert_eq!(scrollbar_thumb(&geometry(0., 1200.), 0.), None);
        assert_eq!(scrollbar_thumb(&geometry(0., 1200.), f32::NAN), None);
    }

    #[test]
    fn the_thumb_is_the_window_of_content_it_covers_and_travels_the_rest() {
        // A 600 px window on a page 1800 px tall (range = 1200): the thumb
        // covers a third of the track, and the track is 600 px tall.
        let (top, height) = scrollbar_thumb(&geometry(0., 1200.), 600.).expect("a bar");
        assert!((height - 200.).abs() < 0.01, "one window in three: {height}");
        assert_eq!(top, 0., "at the top of the page, at the top of the track");

        // Halfway down, halfway along the track.
        let (middle, _) = scrollbar_thumb(&geometry(600., 1200.), 600.).expect("a bar");
        assert!((middle - 200.).abs() < 0.01, "half way: {middle}");

        // At the scroll limit the thumb's bottom edge is the track's end — a
        // reader who reaches the bottom of a page sees the bottom of the bar.
        let (bottom, height) = scrollbar_thumb(&geometry(1200., 1200.), 600.).expect("a bar");
        assert!((bottom + height - 600.).abs() < 0.01, "bottom edge: {bottom} + {height}");

        // A taller window means a taller thumb on the same page.
        let (_, taller) = scrollbar_thumb(
            &ScrollState { y: 0., range: 1200., viewport_h: 1200. },
            600.,
        )
        .expect("a bar");
        assert!((taller - 300.).abs() < 0.01, "half the content: {taller}");
    }

    #[test]
    fn a_thumb_stays_inside_its_track_however_long_the_page_is() {
        // A document a hundred screens long: the raw share of the track would
        // be a pixel row, so the thumb bottoms out at the readable minimum…
        let (top, height) = thumb(&geometry(0., 60_000.), 600.);
        assert!((height - SCROLLBAR_MIN_THUMB).abs() < 0.01, "{height}");
        // …and even then it never hangs off either end of the track.
        assert!(top >= 0. && top + height <= 600. + 0.001, "{top} + {height}");
        let (bottom, height) = thumb(&geometry(60_000., 60_000.), 600.);
        assert!(
            (bottom + height - 600.).abs() < 0.01,
            "a huge page still ends its bar at the track's end: {bottom} + {height}"
        );
        // A report that overshoots its own range (a tab mid-animation) is
        // clamped, not drawn past the edge.
        let (past, _) = thumb(&geometry(70_000., 60_000.), 600.);
        assert!((past - bottom).abs() < 0.01, "{past}");
    }

    /// A thumb the tests can assert on without an `Option` in the way.
    fn thumb(state: &ScrollState, track: f32) -> (f32, f32) {
        scrollbar_thumb(state, track).expect("a bar")
    }

    #[test]
    fn the_bar_hides_itself_a_few_seconds_after_the_last_movement() {
        // The contract the reader sees: the bar is a response to scrolling, not
        // a fixture — so it holds for a beat and then goes.
        let hold = SCROLLBAR_HOLD;
        assert!(
            hold > Duration::from_millis(500) && hold < Duration::from_millis(5000),
            "a few seconds, not a blink and not a fixture: {hold:?}"
        );
    }

    #[test]
    fn only_a_moving_offset_is_the_page_scrolling() {
        // The bar answers a scroll, not a layout change: a resize or a fresh
        // document at the top restates the range and must not light the bar up
        // (task: scroll bar).
        let mut s = TabState::default();
        assert!(
            !s.scroll_moves(&geometry(0., 1200.)),
            "a new range with the offset where it was is not a scroll"
        );
        assert!(
            s.scroll_moves(&geometry(120., 1200.)),
            "an offset that moves is a scroll"
        );
        s.scroll = geometry(120., 1200.);
        assert!(
            !s.scroll_moves(&geometry(120.4, 1200.)),
            "sub-pixel settling is the same place"
        );
        // Back to the top is a movement — the reader should see the thumb go.
        assert!(s.scroll_moves(&geometry(0., 1200.)));
    }

    #[test]
    fn scroll_geometry_lands_on_the_tab_record_without_touching_the_frame() {
        let mut s = TabState::default();
        assert!(!s.scroll.is_scrollable(), "a tab with no page cannot scroll");
        assert!(s.reduce(&FromTab::ScrollState { state: geometry(0., 1200.) }));
        assert!(s.scroll.is_scrollable());
        assert!(
            !s.reduce(&FromTab::ScrollState { state: geometry(0.2, 1200.) }),
            "sub-pixel jitter is the same geometry"
        );
        assert!(s.reduce(&FromTab::ScrollState { state: geometry(600., 1200.) }));
        assert_eq!(s.scroll.y, 600.);
        // A new document that fits again says so, and the bar goes away with it.
        assert!(s.reduce(&FromTab::ScrollState { state: geometry(0., 0.) }));
        assert!(!s.scroll.is_scrollable());
    }
}
