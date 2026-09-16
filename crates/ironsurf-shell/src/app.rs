//! Runtime glue: Slint `AppWindow` ⇄ tab processes ⇄ container registry.
//!
//! All state lives inside `Rc<Core>` so the Slint callbacks, the message
//! drain timer and the spinner timer can share it mutably (RefCells +
//! methods on `Rc<Core>`). `Frame` messages are coalesced to the newest per
//! drain tick; the model is pushed once per tick when anything changed.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};
use std::sync::mpsc::{channel, Receiver, Sender};

use ironsurf_common::{debug, error, info, trace, warn};
use ironsurf_common::{
    default_url, ContextAction, ContextTarget, ContainerId, DialogKind, FromTab, TabId, ToTab,
    Userscript,
};
use ironsurf_storage::default_data_root;
use slint::{
    Color, ComponentHandle, Image, Model, ModelRc, Rgba8Pixel, SharedPixelBuffer, SharedString,
    Timer, TimerMode, VecModel, Weak,
};
// The language's `MouseCursor` enum (the type of the page TouchArea's
// `mouse-cursor` / the generated `set_page_cursor`): a builtin enum, re-
// exported for Rust code next to the generated accessors themselves.
use slint::private_unstable_api::re_exports::MouseCursor;

use crate::classify::classify_input;
use crate::keymap::{self, Shortcut};
use crate::state::TabState;
use crate::tabs::{PumpedMessage, TabProc};
use crate::uisettings::UiSettings;
use crate::{AppWindow, ContainerInfo, MenuItem, TabInfo};
// The Slint-side row structs the DevTools panel renders (`state`'s own
// view-models of the same name are converted into these).
use crate::{ConsoleRow, NetRow};
// The pure userscript-menu helpers (src/userscripts.rs).
use crate::userscripts::{build_new_userscript, enabled_userscripts, parse_patterns, userscript_rows};
// The pure User-Agent-switcher helpers (src/user_agents.rs).
use crate::user_agents::{normalize_custom_ua, ua_display, ua_menu_rows};

/// The tab's built-in User-Agent — what a container **without** an override
/// sends, and what the UA menu's Copy button copies in that case. Duplicate
/// of `ironsurf-tab`'s `netmod::USER_AGENT` (the tab binary is not a library
/// dependency); keep the two strings in sync.
pub const DEFAULT_USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64; IronSurf/0.1)";

/// The stored UA override of `container` (trimmed; blank stored values are
/// treated as "no override", matching what `Store::set_container_user_agent`
/// writes). `None` = the container uses the tab's built-in default.
fn container_user_agent(store: &ironsurf_storage::Store, container: ContainerId) -> Option<String> {
    store
        .container(container)
        .and_then(|c| c.user_agent.clone())
        .filter(|ua| !ua.trim().is_empty())
}

/// Fixed palette — **keep in sync** with `palette-color()` in ui/ironsurf.slint.
pub const PALETTE: [&str; 7] = [
    "#4a90d9", "#e06666", "#6aa84f", "#f6b26b", "#8e7cc3", "#6d4c41", "#0097a7",
];

const SPINNER_FRAMES: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

struct Proc {
    tab: TabProc,
    state: TabState,
    /// Page zoom factor (Ctrl-`+`/`-`/`0`, shell-owned policy; see
    /// `keymap::zoom_in`/`zoom_out`). Sent to the tab as `ToTab::SetZoom`.
    zoom: f32,
    /// The child process died without saying `Closed` (crash) or a send hit
    /// a broken pipe. The tab stays in the strip showing its last frame;
    /// `state.error` carries the “has crashed” status-line message.
    crashed: bool,
    /// DevTools panel open for *this* tab (F12 / Ctrl-`I`). Per tab, like the
    /// zoom factor: switching tabs switches the panel to the new tab's feed,
    /// and a freshly opened tab starts closed.
    devtools_open: bool,
    /// Which pane the panel shows for this tab (Console / Network).
    devtools_view: DevToolsView,
}

/// The shell-side half of a JS dialog being shown (task: window.alert /
/// confirm / prompt): which tab is blocked on the pipe, under which request
/// id, waiting for the user's button. Exactly one can exist at a time — the
/// requesting tab does not answer its stdin while it waits.
#[derive(Clone, Debug, PartialEq, Eq)]
struct PendingDialog {
    tab: TabId,
    dialog_id: u32,
    kind: DialogKind,
    message: String,
    default: Option<String>,
}

impl PendingDialog {
    /// Only a prompt carries text back (the contract: `text` is `Some`
    /// exactly when a prompt was accepted).
    fn prompt(&self) -> bool {
        self.kind == DialogKind::Prompt
    }
}

/// Which DevTools pane the panel shows. Kept as an index (`view_index`) for
/// the Slint property, so the UI stays a number and Rust keeps the names.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DevToolsView {
    #[default]
    Console,
    Network,
}

impl DevToolsView {
    pub fn index(self) -> i32 {
        match self {
            DevToolsView::Console => 0,
            DevToolsView::Network => 1,
        }
    }

    pub fn from_index(idx: i32) -> Self {
        match idx {
            1 => DevToolsView::Network,
            _ => DevToolsView::Console,
        }
    }
}

/// A tab process that is gone: log it loudly (with whatever stderr — hence
/// panic message + backtrace — the shell captured) and put the crash on the
/// tab's status line. `detail` says how the death was noticed.
fn note_tab_crash(p: &mut Proc, detail: std::fmt::Arguments<'_>) {
    let id = p.tab.id;
    p.crashed = true;
    p.state.loading = false;
    p.state.error = Some(format!("Tab {} has crashed \u{2014} {detail}", id.0));
    error!("tab {id} has crashed: {detail}");
    let tail = p.tab.stderr_tail();
    if tail.trim().is_empty() {
        error!("tab {id} left no stderr behind (it inherited the terminal's, or crashed before logging)");
    } else {
        error!("tab {id} stderr (backtrace if it panicked):\n{tail}");
    }
}

/// Shared runtime core (one per window; kept in an `Rc`).
pub struct Core {
    store: RefCell<ironsurf_storage::Store>,
    ui_path: PathBuf,
    settings: RefCell<UiSettings>,
    procs: RefCell<Vec<Proc>>,
    active: Cell<usize>,
    next_tab_id: Cell<u32>,
    sink: Sender<PumpedMessage>,
    rx: RefCell<Receiver<PumpedMessage>>,
    shell: RefCell<Option<Weak<AppWindow>>>,
    tab_bin: PathBuf,
    extra_tab_args: Vec<String>,
    spawn_error: RefCell<Option<String>>,
    spinner: Cell<usize>,
    last_page: Cell<(f32, f32)>,
    /// Pointer-move rate limiting (see [`move_due`]): when the last move was
    /// forwarded, and the newest move that was dropped because it was sent too
    /// soon after it (flushed by the drain tick so the final position is lost
    /// for at most one tick).
    last_move_at: Cell<Option<Instant>>,
    pending_move: Cell<Option<(f32, f32)>>,
    /// The session clipboard (task: context menu). Tabs cannot reach a
    /// system clipboard from inside the sandbox, so every copy — menu
    /// entries, page Ctrl+C/X — funnels here, and every paste source (menu
    /// Paste, Ctrl+V) is pushed back out through `ToTab::Clipboard`.
    clipboard: RefCell<String>,
    /// The actions behind the currently-open context menu, index-aligned
    /// with the Slint `menu-items` model.
    menu_items: RefCell<Vec<MenuAction>>,
    /// The JS dialogs awaiting an answer (task: window.alert/confirm/prompt),
    /// FIFO. Every requesting tab is **blocked** on its pipe, so several
    /// tabs can queue at once; the overlay shows the front one (its tab
    /// activated), and the `dialog_shown` marker says whether that front is
    /// already on screen. Ids tie each answer to its request
    /// (`ToTab::DialogResponse`).
    pending_dialogs: RefCell<VecDeque<PendingDialog>>,
    /// `(tab, dialog id)` of the dialog currently on screen (`None` = the
    /// overlay is closed, and the front of the queue is waiting for it).
    dialog_shown: RefCell<Option<(TabId, u32)>>,
    /// The DevTools console table. Shared with the window (`console-rows`), so
    /// it is patched in place instead of rebuilt per drain tick.
    dt_console: Rc<VecModel<ConsoleRow>>,
    /// The DevTools network table (same reason).
    dt_net: Rc<VecModel<NetRow>>,
    /// Script id of each userscript-menu row, in `us-scripts` model order:
    /// the overlay mutates by row index, the Store (and the wire) speak ids.
    /// Refreshed from the Store whenever the menu list is re-pushed.
    us_ids: RefCell<Vec<u64>>,
    /// The script the inline editor is **editing** (row clicked): Some(id)
    /// upserts in place on Save (same id, same list position, `enabled`
    /// carried through); `None` is a fresh "+ New script" draft that takes
    /// the next id. Cleared on Save, Cancel, and every menu (re)open.
    us_edit_id: RefCell<Option<u64>>,
}

/// The assembled application: core + the timers that drive it (owned so
/// they are never dropped while the window runs).
pub struct App {
    core: Rc<Core>,
    _timers: Vec<Timer>,
}

impl App {
    /// Open the container store, create the core, attach every Slint
    /// callback and start the pump/spinner timers.
    pub fn new(
        ui: &AppWindow,
        tab_bin: PathBuf,
        extra_tab_args: Vec<String>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let store = ironsurf_storage::Store::open(default_data_root())?;
        let ui_path = store.root().join("ui.json");
        let settings = UiSettings::load(&ui_path);
        let (sink, rx) = channel();
        let core = Rc::new(Core {
            store: RefCell::new(store),
            ui_path,
            settings: RefCell::new(settings),
            procs: RefCell::new(Vec::new()),
            active: Cell::new(0),
            next_tab_id: Cell::new(1),
            sink,
            rx: RefCell::new(rx),
            shell: RefCell::new(Some(ui.as_weak())),
            tab_bin,
            extra_tab_args,
            spawn_error: RefCell::new(None),
            spinner: Cell::new(0),
            last_page: Cell::new((0.0, 0.0)),
            last_move_at: Cell::new(None),
            pending_move: Cell::new(None),
            clipboard: RefCell::new(String::new()),
            menu_items: RefCell::new(Vec::new()),
            pending_dialogs: RefCell::new(VecDeque::new()),
            dialog_shown: RefCell::new(None),
            dt_console: Rc::new(VecModel::default()),
            dt_net: Rc::new(VecModel::default()),
            us_ids: RefCell::new(Vec::new()),
            us_edit_id: RefCell::new(None),
        });

        core.attach_callbacks(ui);

        // Message pump: drain every tab's FromTab stream at ~30 Hz.
        let drain_timer = Timer::default();
        let c = core.clone();
        drain_timer.start(TimerMode::Repeated, std::time::Duration::from_millis(33), move || {
            c.drain();
        });

        // Status-bar spinner (only repaints while something is loading).
        let spinner_timer = Timer::default();
        let c = core.clone();
        spinner_timer.start(TimerMode::Repeated, std::time::Duration::from_millis(120), move || {
            c.tick_spinner();
        });

        Ok(App { core, _timers: vec![drain_timer, spinner_timer] })
    }

    /// Open the initial tab (`Some(url)` overrides the default start page).
    pub fn start(&self, url: Option<&str>) {
        self.core.open_first_tab(url);
    }

    /// Persist settings and shut every tab process down (idempotent).
    pub fn shutdown(&self) {
        let procs = std::mem::take(&mut *self.core.procs.borrow_mut());
        info!("shutting down: saving settings, closing {} tab(s)", procs.len());
        self.core.save_settings();
        for mut p in procs {
            p.tab.destroy();
        }
    }

    /// Access to the runtime core (used by the headless integration tests).
    pub fn core(&self) -> Rc<Core> {
        self.core.clone()
    }
}

impl Core {
    // ---- accessors -----------------------------------------------------------

    /// Snapshot of the per-tab states (integration tests / debugging).
    pub fn tab_snapshot(&self) -> Vec<TabState> {
        self.procs.borrow().iter().map(|p| p.state.clone()).collect()
    }

    /// The JS dialogs awaiting the user (task: window.alert/confirm/prompt):
    /// `(tab, dialog id)` in FIFO order (integration tests peek here).
    pub fn pending_dialogs(&self) -> Vec<(TabId, u32)> {
        self.pending_dialogs
            .borrow()
            .iter()
            .map(|p| (p.tab, p.dialog_id))
            .collect()
    }

    /// The DevTools panel as the tabs have it: `(open, pane)` per tab, in strip
    /// order. The flag is per tab, so this is what survives switching tabs.
    pub fn devtools_state(&self) -> Vec<(bool, DevToolsView)> {
        self.procs
            .borrow()
            .iter()
            .map(|p| (p.devtools_open, p.devtools_view))
            .collect()
    }

    fn ui(&self) -> Option<AppWindow> {
        self.shell.borrow().as_ref().and_then(|w| w.upgrade())
    }

    fn active_proc_index(&self) -> Option<usize> {
        let procs = self.procs.borrow();
        if procs.is_empty() {
            None
        } else {
            Some(self.active.get().min(procs.len() - 1))
        }
    }

    fn active_tab_id(&self) -> Option<TabId> {
        let i = self.active_proc_index()?;
        Some(self.procs.borrow()[i].tab.id)
    }

    fn send_active(&self, msg: ToTab) {
        let Some(id) = self.active_tab_id() else {
            debug!("dropped {msg:?}: no active tab");
            return;
        };
        let mut crashed = false;
        {
            let mut procs = self.procs.borrow_mut();
            if let Some(p) = procs.iter_mut().find(|p| p.tab.id == id) {
                if p.crashed {
                    // Already reported dead: the status line says so; do not
                    // re-log the same crash for every keystroke.
                    debug!("dropped {msg:?}: tab {id} has crashed");
                    return;
                }
                if let Err(err) = p.tab.send(msg) {
                    // `msg` is consumed by send(); the command itself is
                    // already on the record via `tabs::TabProc::send`'s trace
                    // line. A write error means the child is gone (broken
                    // pipe): say so on the tab's status line — never silently.
                    let status = p.tab.try_wait_status();
                    match status {
                        Some(status) => note_tab_crash(
                            p,
                            format_args!("the process exited ({status}) after a command failed to reach it: {err}"),
                        ),
                        None => note_tab_crash(
                            p,
                            format_args!("its stdin is dead ({err}) — the process is no longer reachable"),
                        ),
                    }
                    crashed = true;
                }
            }
        }
        if crashed {
            self.sync_ui();
        }
    }

    /// Container for new tabs: the remembered one if it still exists, else
    /// the registry default ("remember last chosen" per ui.json).
    fn new_tab_container(&self) -> ContainerId {
        let store = self.store.borrow();
        let remembered = self.settings.borrow().new_tab_container.map(ContainerId);
        match remembered.filter(|id| store.container(*id).is_some()) {
            Some(id) => id,
            None => store.default_container_id(),
        }
    }

    fn page_size(&self) -> (f32, f32) {
        match self.ui() {
            Some(ui) => (ui.get_page_width(), ui.get_page_height()),
            None => self.last_page.get(),
        }
    }

    // ---- tabs ------------------------------------------------------------------

    fn open_first_tab(&self, url: Option<&str>) {
        let container = self.new_tab_container();
        let url = url.map(str::to_string).unwrap_or_else(default_url);
        self.open_tab(container, Some(&url));
        if let Some(ui) = self.ui() {
            // Bumping past 0 arms the focus guards in the .slint file.
            ui.set_focus_page_token(ui.get_focus_page_token() + 1);
        }
        self.sync_ui();
    }

    fn open_tab(&self, container: ContainerId, url: Option<&str>) {
        // Per-spawn argument list: the shared `extra_tab_args` plus this
        // container's UA override (task: per-container UA), so the very first
        // page load of a fresh tab already sends the stored string. Built
        // per spawn — `self.extra_tab_args` is never mutated.
        let (profile_dir, tab_bin, extra) = {
            let store = self.store.borrow();
            let mut extra = self.extra_tab_args.clone();
            if let Some(ua) = container_user_agent(&store, container) {
                extra.push("--user-agent".to_string());
                extra.push(ua);
            }
            (store.profile_dir(container), self.tab_bin.clone(), extra)
        };
        let id = {
            let id = TabId(self.next_tab_id.get());
            self.next_tab_id.set(self.next_tab_id.get() + 1);
            id
        };
        let extra_refs: Vec<&str> = extra.iter().map(String::as_str).collect();
        debug!("open tab {id} in container {}: url {:?}", container.0, url);
        match TabProc::spawn(&tab_bin, id, container, &profile_dir, url, &extra_refs, &self.sink) {
            Ok(tab) => {
                *self.spawn_error.borrow_mut() = None;
                let (w, h) = self.page_size();
                {
                    let mut procs = self.procs.borrow_mut();
                    procs.push(Proc {
                        tab,
                        state: TabState::default(),
                        zoom: 1.0,
                        crashed: false,
                        devtools_open: false,
                        devtools_view: DevToolsView::default(),
                    });
                    self.active.set(procs.len() - 1);
                }
                if w > 0.0 && h > 0.0 {
                    self.send_active(ToTab::Resize { width: w as u32, height: h as u32, scale: 1.0 });
                }
                // A tab process starts with an empty script set: seed it with
                // the enabled userscripts at once (task: userscript menu).
                self.seed_userscripts(id);
            }
            Err(err) => {
                error!("spawn {} failed: {err}", tab_bin.display());
                *self.spawn_error.borrow_mut() =
                    Some(format!("spawn {} failed: {err}", tab_bin.display()));
            }
        }
        self.set_visible_except();
        self.sync_ui();
        // A new tab activates: an open UA menu re-points to its container.
        self.ua_refresh_if_open();
    }

    fn close_tab(&self, idx: usize) {
        // A tab whose modal is still open must not be left waiting on an
        // answer the overlay will never produce again (task: JS dialogs):
        // close it cancelled first (which also unblocks the tab process).
        if let Some(tid) = self.procs.borrow().get(idx).map(|p| p.tab.id) {
            self.clear_dialog_for(tid, true);
        }
        let mut procs = self.procs.borrow_mut();
        if idx >= procs.len() {
            return;
        }
        info!("closing tab {} ({} still open)", procs[idx].tab.id.0, procs.len() - 1);
        let mut proc = procs.remove(idx);
        proc.tab.destroy();
        let empty = procs.is_empty();
        if !empty {
            self.active.set(self.active.get().min(procs.len() - 1));
        }
        drop(procs);
        if empty {
            // Last tab closed ⇒ the window (and every helper thread) is done.
            let _ = slint::quit_event_loop();
        } else {
            self.set_visible_except();
        }
        self.sync_ui();
    }

    // ---- JS dialogs (window.alert/confirm/prompt) --------------------------

    /// A tab reported `FromTab::DialogRequest`: that tab is now blocked on
    /// its pipe (task: window.alert/confirm/prompt). The request joins the
    /// FIFO queue; a request from a tab that is gone or crashed is answered
    /// cancelled right away, so a dying tab never waits on a dialog nobody
    /// can show.
    fn open_dialog(&self, tid: TabId, dialog_id: u32, kind: DialogKind, message: &str, default: Option<&str>) {
        let alive = self
            .procs
            .borrow()
            .iter()
            .find(|p| p.tab.id == tid)
            .is_some_and(|p| !p.crashed && !p.state.closed);
        if !alive {
            warn!("tab {tid} asked for a dialog but is gone or crashed — answering cancelled");
            let _ = self.send_to(tid, ToTab::DialogResponse { dialog_id, confirmed: false, text: None });
            return;
        }
        self.pending_dialogs.borrow_mut().push_back(PendingDialog {
            tab: tid,
            dialog_id,
            kind,
            message: message.to_string(),
            default: default.map(str::to_string),
        });
        self.show_next_dialog();
    }

    /// Put the front of the queue on screen — unless a dialog is already
    /// showing. The requesting tab is activated: a background tab's modal
    /// must not sit unseen while its process waits. A queued tab that
    /// vanished since is dropped (its pump answers cancelled at EOF).
    fn show_next_dialog(&self) {
        if self.dialog_shown.borrow().is_some() {
            return; // one modal at a time; the next waits its turn
        }
        let Some(next) = self.pending_dialogs.borrow().front().cloned() else { return };
        let at = self.procs.borrow().iter().position(|p| p.tab.id == next.tab);
        let Some(at) = at else {
            self.pending_dialogs.borrow_mut().retain(|p| p != &next);
            return self.show_next_dialog();
        };
        let title = format!("Tab {} — {}", next.tab.0, self.procs.borrow()[at].state.title);
        let Some(ui) = self.ui() else { return };
        if self.active.get() != at {
            info!("tab {} has a dialog — switching to its tab", next.tab.0);
            self.activate_tab(at);
        }
        *self.dialog_shown.borrow_mut() = Some((next.tab, next.dialog_id));
        ui.set_dialog_kind(match next.kind {
            DialogKind::Alert => 0,
            DialogKind::Confirm => 1,
            DialogKind::Prompt => 2,
        });
        ui.set_dialog_message(next.message.as_str().into());
        ui.set_dialog_title(title.into());
        // The prompt box starts at the page's default; the other kinds
        // clear it so a stale answer can never leak into the next dialog.
        ui.set_dialog_input(next.default.as_deref().unwrap_or_default().into());
        ui.set_dialog_open(true);
    }

    /// The user dismissed the open dialog: OK (accepted) or Cancel/Esc
    /// (cancelled). The answer rides back as `ToTab::DialogResponse`; the
    /// blocked tab's JS call returns from there, and the next queued dialog
    /// (from any tab) takes the overlay.
    fn answer_dialog(&self, confirmed: bool) {
        let Some(shown) = self.dialog_shown.borrow_mut().take() else {
            debug!("dialog answer with no dialog on screen — ignored");
            return;
        };
        let pending = {
            let mut queue = self.pending_dialogs.borrow_mut();
            let at = queue.iter().position(|p| (p.tab, p.dialog_id) == shown);
            match at.map(|i| queue.remove(i).expect("position came from this queue")) {
                Some(p) => p,
                None => {
                    // The request was already cleared (its tab went away);
                    // the tab we answered is the same request — drop it.
                    debug!("dialog answer for an already-cleared request — ignored");
                    if let Some(ui) = self.ui() {
                        ui.set_dialog_open(false);
                    }
                    return;
                }
            }
        };
        let ui = self.ui();
        let text = ui
            .as_ref()
            .filter(|_| confirmed && pending.prompt())
            .map(|ui| ui.get_dialog_input().to_string());
        if let Some(ui) = ui {
            ui.set_dialog_open(false);
        }
        info!(
            "dialog #{} ({:?}) on tab {} answered {}",
            pending.dialog_id,
            pending.kind,
            pending.tab.0,
            if confirmed { "OK" } else { "cancel" }
        );
        self.send_to(pending.tab, ToTab::DialogResponse { dialog_id: pending.dialog_id, confirmed, text });
        self.show_next_dialog();
    }

    /// Drop every request queued or shown for *that* tab — it is going away
    /// (closed, reaped, or crashed) and its blocked calls can no longer be
    /// answered from the overlay. The tab is woken with cancelled answers
    /// while it can still receive them (`notify_tab`); the overlay closes
    /// (or rolls to the next queued dialog).
    fn clear_dialog_for(&self, tid: TabId, notify_tab: bool) {
        let dropped: Vec<PendingDialog> = {
            let queue = self.pending_dialogs.borrow();
            let dropped = queue.iter().filter(|p| p.tab == tid).cloned().collect::<Vec<_>>();
            if dropped.is_empty() {
                return;
            }
            dropped
        };
        self.pending_dialogs.borrow_mut().retain(|p| p.tab != tid);
        let shown_was_ours =
            self.dialog_shown.borrow().as_ref().is_some_and(|(t, _)| *t == tid);
        if shown_was_ours {
            *self.dialog_shown.borrow_mut() = None;
            if let Some(ui) = self.ui() {
                ui.set_dialog_open(false);
            }
        }
        if notify_tab {
            for p in &dropped {
                let _ = self.send_to(
                    tid,
                    ToTab::DialogResponse { dialog_id: p.dialog_id, confirmed: false, text: None },
                );
            }
        }
        if shown_was_ours {
            self.show_next_dialog();
        }
    }

    /// Send one command to a tab by id (the pending dialog's tab is not
    /// necessarily the active one at the moments this is called).
    fn send_to(&self, tid: TabId, msg: ToTab) -> std::io::Result<()> {
        let mut procs = self.procs.borrow_mut();
        match procs.iter_mut().find(|p| p.tab.id == tid) {
            Some(p) => p.tab.send(msg),
            None => Err(std::io::Error::other("tab is gone")),
        }
    }

    fn activate_tab(&self, idx: usize) {
        {
            let procs = self.procs.borrow();
            if idx >= procs.len() {
                debug!("activate ignored: no tab at index {idx}");
                return;
            }
            debug!("activate tab {} (index {idx})", procs[idx].tab.id.0);
        }
        self.active.set(idx);
        self.set_visible_except();
        if let Some(id) = self.active_tab_id() {
            let (w, h) = self.page_size();
            if w > 0.0 && h > 0.0 {
                let procs = self.procs.borrow();
                if let Some(p) = procs.iter().find(|p| p.tab.id == id) {
                    let _ = p.tab.send(ToTab::Resize { width: w as u32, height: h as u32, scale: 1.0 });
                }
            }
        }
        self.sync_ui();
        // The UA menu (if open) follows the activation: the new tab's
        // container's value, rows, and error line replace the previous one's.
        self.ua_refresh_if_open();
    }

    fn cycle_tab(&self) {
        let len = self.procs.borrow().len();
        if len > 1 {
            let next = (self.active.get() as usize + 1) % len;
            self.activate_tab(next);
        }
    }

    /// Only the active tab renders; background tabs throttle themselves.
    fn set_visible_except(&self) {
        let active = self.active_tab_id();
        let procs = self.procs.borrow();
        for p in procs.iter() {
            let _ = p.tab.send(ToTab::SetVisible { visible: Some(p.tab.id) == active });
        }
    }

    fn navigate(&self, text: &str) {
        let Some(url) = classify_input(text) else {
            debug!("address bar: {text:?} is not a URL/search term, ignored");
            return;
        };
        info!("address bar: {text:?} → navigating to {url}");
        self.send_active(ToTab::Navigate { url: url.clone() });
        if let Some(ui) = self.ui() {
            ui.set_url_text(url.into());
            // After navigating from the URL bar, focus returns to the page.
            ui.set_focus_page_token(ui.get_focus_page_token() + 1);
        }
    }

    /// Resize the active tab when the page area changed size.
    fn check_resize(&self) {
        let size = self.page_size();
        if size == self.last_page.get() || size.0 <= 0.0 || size.1 <= 0.0 {
            return;
        }
        self.last_page.set(size);
        self.send_active(ToTab::Resize { width: size.0 as u32, height: size.1 as u32, scale: 1.0 });
    }

    // ---- input routing ---------------------------------------------------------

    fn page_key(&self, pressed: bool, text: &str, ctrl: bool, shift: bool, alt: bool, meta: bool, repeat: bool) {
        if !pressed {
            return; // releases are not forwarded: the tab protocol models presses only
        }
        match keymap::shortcut_for(text, ctrl, alt, meta) {
            Some(sc) => debug!("shortcut {sc:?} (key {text:?} ctrl={ctrl} alt={alt} meta={meta})"),
            None => trace!("key {text:?} ctrl={ctrl} alt={alt} meta={meta} → page"),
        }
        match keymap::shortcut_for(text, ctrl, alt, meta) {
            Some(Shortcut::FocusUrl) => {
                if let Some(ui) = self.ui() {
                    let t = ui.get_focus_url_token() + 1;
                    ui.set_focus_url_token(t);
                }
                return;
            }
            Some(Shortcut::CloseTab) => {
                let idx = self.active_proc_index().unwrap_or(0);
                self.close_tab(idx);
                return;
            }
            Some(Shortcut::Reload) => {
                self.send_active(ToTab::Reload);
                return;
            }
            Some(Shortcut::NewTab) => {
                let cid = self.new_tab_container();
                self.open_tab(cid, Some(&default_url()));
                return;
            }
            Some(Shortcut::CycleTab) => {
                self.cycle_tab();
                return;
            }
            Some(Shortcut::ZoomIn) => {
                self.step_zoom(keymap::zoom_in);
                return;
            }
            Some(Shortcut::ZoomOut) => {
                self.step_zoom(keymap::zoom_out);
                return;
            }
            Some(Shortcut::ZoomReset) => {
                self.apply_zoom(1.0);
                return;
            }
            Some(Shortcut::ToggleDevTools) => {
                // Page focus only: the chord arrives from the page's `FocusScope`.
                // Catching it while the URL bar is focused would need a second
                // `FocusScope` wrapped around the `LineEdit`, which risks
                // swallowing ordinary typing — and the panel is a page tool.
                self.toggle_devtools();
                return;
            }
            Some(Shortcut::PageFocus) => {
                // Escape also dismisses an open context menu.
                self.close_context_menu();
                if let Some(ui) = self.ui() {
                    let t = ui.get_focus_page_token() + 1;
                    ui.set_focus_page_token(t);
                }
                return;
            }
            None => {}
        }
        // Ctrl+V: the sandboxed tab cannot read the session clipboard, so
        // push it in before the keystroke is relayed (blitz's editors pull
        // the paste from `ShellProvider::get_clipboard_text`, which answers
        // from this cache).
        if is_paste_chord(text, ctrl, alt, meta) {
            let clip = self.clipboard.borrow().clone();
            debug!("clipboard: pushed {} char(s) ahead of Ctrl+V", clip.chars().count());
            self.send_active(ToTab::Clipboard { text: clip });
        }
        let ev = keymap::to_ipc_key(text, ctrl, shift, alt, meta, repeat);
        self.send_active(ToTab::Key(ev));
    }

    /// One zoom step (in/out) from the active tab's current factor.
    fn step_zoom(&self, step: impl Fn(f32) -> f32) {
        let zoom = self
            .active_proc_index()
            .map(|i| step(self.procs.borrow()[i].zoom))
            .unwrap_or(1.0);
        self.apply_zoom(zoom);
    }

    /// Push a new zoom factor to the active tab (Ctrl-`+`/`-`/`0` chords);
    /// the tab rescales its document viewport and reports back with fresh
    /// frames. The status bar says "Zoom N%" until the page next speaks.
    fn apply_zoom(&self, zoom: f32) {
        let Some(i) = self.active_proc_index() else {
            debug!("zoom {zoom:.0}% dropped: no active tab");
            return;
        };
        let label = format!("Zoom {:.0}%", zoom * 100.0);
        info!("{label} (active tab)");
        {
            let mut procs = self.procs.borrow_mut();
            let p = &mut procs[i];
            p.zoom = zoom;
            p.state.status = Some(label);
            if let Err(err) = p.tab.send(ToTab::SetZoom { zoom }) {
                warn!("tab {} could not set zoom: {err}", p.tab.id);
            }
        }
        self.sync_ui();
    }

    // ---- DevTools panel --------------------------------------------------------

    /// F12 / Ctrl-`I`: flip the DevTools panel of the active tab. The flag sits
    /// on the tab record (like the zoom factor), so switching tabs switches the
    /// panel to the new tab's feed and a freshly spawned tab starts closed.
    fn toggle_devtools(&self) {
        let Some(i) = self.active_proc_index() else {
            debug!("DevTools toggle dropped: no active tab");
            return;
        };
        let mut procs = self.procs.borrow_mut();
        let p = &mut procs[i];
        p.devtools_open = !p.devtools_open;
        let (id, open) = (p.tab.id, p.devtools_open);
        drop(procs);
        info!("DevTools {} on tab {id}", if open { "opened" } else { "closed" });
        self.sync_ui();
    }

    /// The panel's “✕” row (and nothing else) closes it for the active tab.
    fn close_devtools(&self) {
        let Some(i) = self.active_proc_index() else { return };
        {
            let mut procs = self.procs.borrow_mut();
            if !procs[i].devtools_open {
                return;
            }
            procs[i].devtools_open = false;
        }
        self.sync_ui();
    }

    /// A header row picked a pane (Console / Network).
    fn set_devtools_view(&self, view: DevToolsView) {
        let Some(i) = self.active_proc_index() else { return };
        {
            let mut procs = self.procs.borrow_mut();
            if procs[i].devtools_view == view {
                return;
            }
            procs[i].devtools_view = view;
        }
        self.sync_ui();
    }

    /// Write the panel's whole view-model (pane, counts, rows) for the active
    /// tab. Called from `sync_ui`, and again from the drain tick whenever a
    /// console/network message landed: those never report `changed` (they must
    /// not dirty the frame), so nothing else would push them while open.
    fn write_devtools(&self, ui: &AppWindow, procs: &[Proc]) {
        let active = self.active.get().min(procs.len().saturating_sub(1));
        let Some(p) = procs.get(active) else {
            ui.set_devtools_open(false);
            ui.set_console_count(0);
            ui.set_network_count(0);
            return;
        };
        ui.set_devtools_open(p.devtools_open);
        ui.set_devtools_view(p.devtools_view.index());

        // The models are shared (`Rc<VecModel>` installed once): appending the
        // few rows that arrived keeps the list's scroll position, which a fresh
        // model per tick would throw away.
        sync_model(&self.dt_console, &p.state.console_rows().into_iter().map(|r| ConsoleRow {
            level: r.level.into(),
            text: r.text.into(),
        }).collect::<Vec<_>>());
        sync_model(&self.dt_net, &p.state.net_rows().into_iter().map(|r| NetRow {
            method: r.method.into(),
            url: r.url.into(),
            status: r.status.into(),
            size: r.size.into(),
            time: r.time.into(),
            failed: r.failed,
        }).collect::<Vec<_>>());
        ui.set_console_count(self.dt_console.row_count() as i32);
        ui.set_network_count(self.dt_net.row_count() as i32);
    }

    fn page_pointer(&self, x: f32, y: f32, kind: i32, button: i32) {
        use ironsurf_common::{MouseButton, PointerKind};
        let kind = match kind {
            1 => PointerKind::Down,
            2 => PointerKind::Up,
            4 => PointerKind::Out,
            _ => PointerKind::Move,
        };
        let button = match button {
            1 => MouseButton::Right,
            2 => MouseButton::Middle,
            _ => MouseButton::Left,
        };
        // Hover moves are rate limited: a moving mouse produces far more of
        // them than the tab can hit-test, and every stale one delays the click
        // that follows. Presses, releases and buttons never wait.
        if matches!(kind, PointerKind::Move) {
            let now = Instant::now();
            if !move_due(now, self.last_move_at.get()) {
                self.pending_move.set(Some((x, y)));
                return;
            }
            self.last_move_at.set(Some(now));
            self.pending_move.set(None);
        } else {
            // A press makes the current position authoritative.
            self.pending_move.set(None);
            self.last_move_at.set(Some(Instant::now()));
        }
        self.send_active(ToTab::Pointer(ironsurf_common::PointerEvent { x, y, kind, button }));
    }

    /// Forward the newest hover move that was dropped as too soon after the
    /// previous one (called from the 33 Hz drain tick, so the cursor position
    /// the tab is showing lags by at most one tick).
    fn flush_pending_move(&self) {
        let Some((x, y)) = self.pending_move.take() else { return };
        let now = Instant::now();
        if !move_due(now, self.last_move_at.get()) {
            self.pending_move.set(Some((x, y)));
            return;
        }
        self.last_move_at.set(Some(now));
        self.send_active(ToTab::Pointer(ironsurf_common::PointerEvent {
            x,
            y,
            kind: ironsurf_common::PointerKind::Move,
            button: ironsurf_common::MouseButton::Left,
        }));
    }

    fn page_wheel(&self, x: f32, y: f32, delta_x: f32, delta_y: f32) {
        self.send_active(ToTab::Scroll(ironsurf_common::ScrollEvent { x, y, delta_x, delta_y }));
    }

    /// Land a tab's hover-cursor report on the UI (task: cursor icon): the
    /// page's `TouchArea` draws the shape, and — because Slint re-reads an
    /// item's `mouse-cursor` only *while processing a pointer event*, and the
    /// mouse may have stopped the very instant the tab answered — pokes a
    /// synthetic pointer move at the last position over the page so the new
    /// shape appears without the user moving. The poke is a plain move (a
    /// press is never synthesised), the rate limiter absorbs it, and the tab
    /// — same point, same cursor — stays silent, so it cannot loop.
    fn apply_page_cursor(&self, cursor: ironsurf_common::CursorIcon) {
        let Some(ui) = self.ui() else { return };
        ui.set_page_cursor(slint_cursor(cursor));
        if ui.get_page_pointer_inside() {
            ui.window().dispatch_event(slint::platform::WindowEvent::PointerMoved {
                position: slint::LogicalPosition::new(
                    ui.get_pointer_x_in_window(),
                    ui.get_pointer_y_in_window(),
                ),
            });
        }
    }

    // ---- context menu --------------------------------------------------------

    /// A tab reported a right-click: build the menu for what the cursor
    /// found (plus the tab's history state and the session clipboard) and
    /// open it at the click point, in page-area coordinates.
    fn open_context_menu(&self, x: f32, y: f32, target: &ContextTarget) {
        let Some(ui) = self.ui() else { return };
        let (url, can_back, can_forward) = match self.active_proc_index() {
            Some(i) => {
                let procs = self.procs.borrow();
                let p = &procs[i];
                (p.state.url.clone(), p.state.can_back, p.state.can_forward)
            }
            None => (None, false, false),
        };
        let clipboard_empty = self.clipboard.borrow().is_empty();
        let items = context_menu_items(target, url.as_deref(), can_back, can_forward, clipboard_empty);
        debug!("context menu: {} entries", items.len());
        self.menu_items
            .replace(items.iter().map(|(_, a)| a.clone()).collect());
        ui.set_menu_items(ModelRc::from(Rc::new(VecModel::from(
            items
                .into_iter()
                .map(|(label, _)| MenuItem { label: label.into() })
                .collect::<Vec<_>>(),
        ))));
        ui.set_menu_x(x);
        ui.set_menu_y(y);
        ui.set_menu_open(true);
    }

    fn close_context_menu(&self) {
        if let Some(ui) = self.ui() {
            ui.set_menu_open(false);
        }
    }

    /// A context-menu row was picked (or the menu was closed): run the
    /// stored action. Shell-side entries never reach the tab; engine-side
    /// ones go over as `ToTab::ContextAction`.
    fn menu_pick(&self, idx: usize) {
        let action = self.menu_items.borrow().get(idx).cloned();
        self.close_context_menu();
        let Some(action) = action else {
            debug!("context menu pick {idx} has no action (stale menu?)");
            return;
        };
        debug!("context menu: {action:?}");
        match action {
            MenuAction::Back => self.send_active(ToTab::Back),
            MenuAction::Forward => self.send_active(ToTab::Forward),
            MenuAction::Reload => self.send_active(ToTab::Reload),
            MenuAction::OpenNewTab(url) => {
                // Same container as the requesting tab, per the middle-click
                // rule the tab already follows for links.
                let container = self
                    .active_proc_index()
                    .map(|i| self.procs.borrow()[i].tab.container);
                if let Some(container) = container {
                    self.open_tab(container, Some(&url));
                }
            }
            MenuAction::CopyText(text) => self.clipboard_set(&text),
            MenuAction::Paste => {
                let text = self.clipboard.borrow().clone();
                if !text.is_empty() {
                    // One stdin batch, two messages: the tab loads the
                    // clipboard into its engine, then synthesizes the Ctrl+V
                    // that consumes it.
                    self.send_active(ToTab::Clipboard { text: text.clone() });
                    self.send_active(ToTab::ContextAction(ContextAction::Paste { text }));
                }
            }
            MenuAction::ToggleVideo => {
                self.send_active(ToTab::ContextAction(ContextAction::ToggleVideo))
            }
        }
    }

    /// Put text on the session clipboard. An empty copy is dropped — the
    /// clipboard keeps whatever it held, as every browser does.
    fn clipboard_set(&self, text: &str) {
        if text.is_empty() {
            return;
        }
        info!("clipboard: copied {} char(s)", text.chars().count());
        *self.clipboard.borrow_mut() = text.to_owned();
    }

    // ---- container manager -----------------------------------------------------

    fn containers_snapshot(&self) -> Vec<ContainerId> {
        self.store.borrow().containers().iter().map(|c| c.id).collect()
    }

    fn container_index(&self, id: ContainerId) -> usize {
        self.containers_snapshot().iter().position(|c| *c == id).unwrap_or(0)
    }

    fn manager_open(&self, open: bool) {
        let Some(ui) = self.ui() else { return };
        if open {
            // Two full-window overlays never stack: opening the manager shuts
            // the userscript menu and the UA switcher (and vice versa — see
            // `userscript_menu` and `ua_menu`).
            ui.set_us_menu_open(false);
            ui.set_ua_menu_open(false);
            let active_container = self
                .active_proc_index()
                .map(|i| self.procs.borrow()[i].tab.container)
                .unwrap_or_else(|| self.store.borrow().default_container_id());
            let idx = self.container_index(active_container);
            ui.set_selected_container(idx as i32);
            ui.set_editing_index(-1);
        }
        ui.set_manager_open(open);
    }

    fn mgr_new_tab_in(&self, idx: usize) {
        let Some(cid) = self.containers_snapshot().get(idx).copied() else { return };
        self.settings.borrow_mut().new_tab_container = Some(cid.0);
        self.save_settings();
        self.open_tab(cid, Some(&default_url()));
        self.manager_open(false);
    }

    /// Move = re-spawn the active tab in another container keeping its URL
    /// (a tab process is bound to exactly one container/profile/sandbox).
    fn mgr_move_tab_to(&self, idx: usize) {
        let Some(cid) = self.containers_snapshot().get(idx).copied() else { return };
        let Some(at) = self.active_proc_index() else { return };
        let (url, id, same) = {
            let procs = self.procs.borrow();
            let p = &procs[at];
            (p.state.url.clone(), p.tab.id, p.tab.container == cid)
        };
        if same {
            self.manager_open(false);
            return;
        }
        let profile_dir = self.store.borrow().profile_dir(cid);
        let (w, h) = self.page_size();
        info!("moving tab {id} to container {}: respawning the process", cid.0);
        // The respawn rides the destination container's UA exactly like
        // `open_tab` does — a per-spawn list, `extra_tab_args` untouched.
        let extra = {
            let mut extra = self.extra_tab_args.clone();
            if let Some(ua) = container_user_agent(&self.store.borrow(), cid) {
                extra.push("--user-agent".to_string());
                extra.push(ua);
            }
            extra
        };
        let replacement = TabProc::spawn(
            &self.tab_bin,
            id,
            cid,
            &profile_dir,
            url.as_deref(),
            &extra.iter().map(String::as_str).collect::<Vec<&str>>(),
            &self.sink,
        );
        if let Ok(tab) = replacement {
            let old = {
                let mut procs = self.procs.borrow_mut();
                let zoom = procs[at].zoom;
                let (devtools_open, devtools_view) =
                    (procs[at].devtools_open, procs[at].devtools_view);
                std::mem::replace(
                    &mut procs[at],
                    Proc {
                        tab,
                        state: TabState::default(),
                        zoom,
                        crashed: false,
                        devtools_open,
                        devtools_view,
                    },
                )
            };
            if w > 0.0 && h > 0.0 {
                let _ = old.tab.send(ToTab::Resize { width: w as u32, height: h as u32, scale: 1.0 });
            }
            let mut old = old;
            old.tab.destroy();
            // The replacement process is new: seed its userscript set too.
            self.seed_userscripts(id);
        }
        self.manager_open(false);
        self.sync_ui();
    }

    fn mgr_delete(&self, idx: usize) {
        let Some(cid) = self.containers_snapshot().get(idx).copied() else { return };
        // Store::delete_container refuses the default and removes the
        // profile dir — that is the data wipe.
        info!("deleting container {} (profile wipe)", cid.0);
        if self.store.borrow_mut().delete_container(cid).is_err() {
            warn!("container {} could not be deleted (default container?)", cid.0);
            self.manager_open(false);
            self.sync_ui();
            return;
        }
        {
            let mut procs = self.procs.borrow_mut();
            procs.retain(|p| p.tab.container != cid);
            if !procs.is_empty() {
                self.active.set(self.active.get().min(procs.len() - 1));
            }
            let empty = procs.is_empty();
            drop(procs);
            if empty {
                let _ = slint::quit_event_loop();
            }
        }
        if self.settings.borrow().new_tab_container == Some(cid.0) {
            self.settings.borrow_mut().new_tab_container = None;
        }
        self.save_settings();
        self.manager_open(false);
    }

    fn mgr_start_rename(&self, idx: usize) {
        let Some(ui) = self.ui() else { return };
        let Some(cid) = self.containers_snapshot().get(idx).copied() else { return };
        let name = self.store.borrow().container(cid).map(|c| c.name.clone()).unwrap_or_default();
        ui.set_container_edit_text(name.into());
        ui.set_editing_index(idx as i32);
    }

    fn mgr_rename(&self, idx: usize) {
        let Some(cid) = self.containers_snapshot().get(idx).copied() else { return };
        let name = self.ui().map(|ui| ui.get_container_edit_text().to_string()).unwrap_or_default();
        if !name.trim().is_empty() {
            let _ = self.store.borrow_mut().rename_container(cid, name.trim());
        }
        if let Some(ui) = self.ui() {
            ui.set_editing_index(-1);
            ui.set_container_edit_text(String::new().into());
        }
        self.sync_ui();
    }

    fn mgr_add(&self) {
        let name = self.ui().map(|ui| ui.get_container_edit_text().trim().to_string()).unwrap_or_default();
        if !name.is_empty() {
            let _ = self.store.borrow_mut().create_container(&name, PALETTE[0]);
        }
        if let Some(ui) = self.ui() {
            ui.set_container_edit_text(String::new().into());
        }
        self.sync_ui();
    }

    fn mgr_recolor(&self, idx: usize, palette: usize) {
        let Some(cid) = self.containers_snapshot().get(idx).copied() else { return };
        let color = PALETTE.get(palette).copied().unwrap_or(PALETTE[0]);
        let _ = self.store.borrow_mut().set_container_color(cid, color);
        self.sync_ui();
    }

    fn mgr_select(&self, idx: i32) {
        if let Some(ui) = self.ui() {
            ui.set_selected_container(idx);
        }
    }

    // ---- userscript menu (task: userscript menu) -----------------------------

    /// Every stored userscript, in storage order (integration tests).
    pub fn userscripts(&self) -> Vec<Userscript> {
        self.store.borrow().userscripts()
    }

    /// The id of each userscript-menu row, in `us-scripts` model order
    /// (integration tests).
    pub fn userscript_row_ids(&self) -> Vec<u64> {
        self.us_ids.borrow().clone()
    }

    /// Re-read the list from the Store: refresh the row-id mirror and push the
    /// rows (`name`, patterns joined by `", "`, enabled) onto the menu model.
    fn us_refresh(&self) {
        if let Some(ui) = self.ui() {
            let scripts = self.store.borrow().userscripts();
            *self.us_ids.borrow_mut() = scripts.iter().map(|s| s.id).collect();
            ui.set_us_scripts(ModelRc::from(Rc::new(VecModel::from(userscript_rows(&scripts)))));
        }
    }

    /// Open/close the menu overlay. Opening re-reads the Store, resets the
    /// editor, and closes the container manager: two full-window overlays must
    /// never stack — and the manager closes this one the same way (see
    /// [`Core::manager_open`]).
    fn userscript_menu(&self, open: bool) {
        let Some(ui) = self.ui() else { return };
        debug!("userscript menu {}", if open { "opened" } else { "closed" });
        if open {
            // Never stack: the manager and the UA switcher both step aside —
            // and close this menu the same way from their own open legs.
            ui.set_manager_open(false);
            ui.set_ua_menu_open(false);
            ui.set_us_editing(false);
            ui.set_us_editing_existing(false);
            ui.set_us_error(SharedString::default());
            *self.us_edit_id.borrow_mut() = None;
            self.us_refresh();
        }
        ui.set_us_menu_open(open);
    }

    /// The toolbar's "</>" button: flip the menu overlay.
    fn toggle_userscript_menu(&self) {
        let open = self.ui().is_some_and(|ui| ui.get_us_menu_open());
        self.userscript_menu(!open);
    }

    /// A row's CheckBox was flipped: persist through the Store **at once** and
    /// rebroadcast. `toggled` fires after the checkbox flipped its own
    /// `checked`, so the wanted state is the negation of the stored one.
    fn us_toggle(&self, idx: usize) {
        let Some(id) = self.us_ids.borrow().get(idx).copied() else { return };
        let now = !self
            .store
            .borrow()
            .userscripts()
            .iter()
            .any(|s| s.id == id && s.enabled);
        if let Err(err) = self.store.borrow_mut().set_userscript_enabled(id, now) {
            warn!("userscript {id} could not be toggled: {err}");
        }
        self.us_broadcast();
        self.us_refresh();
    }

    /// A row's ✕: remove the script (its data is the script itself — nothing
    /// else to wipe) and rebroadcast the rest.
    fn us_delete(&self, idx: usize) {
        let Some(id) = self.us_ids.borrow().get(idx).copied() else { return };
        if let Err(err) = self.store.borrow_mut().delete_userscript(id) {
            warn!("userscript {id} could not be deleted: {err}");
        }
        self.us_broadcast();
        self.us_refresh();
    }

    /// "+ New script": reveal the inline editor with empty fields.
    fn us_new(&self) {
        let Some(ui) = self.ui() else { return };
        *self.us_edit_id.borrow_mut() = None;
        ui.set_us_name(SharedString::default());
        ui.set_us_patterns(SharedString::default());
        ui.set_us_code(SharedString::default());
        ui.set_us_error(SharedString::default());
        ui.set_us_editing_existing(false);
        ui.set_us_editing(true);
    }

    /// A row was clicked (anywhere but its CheckBox / ✕): prefill the editor
    /// with that script and mark the session as an **edit** — Save upserts
    /// the same id instead of minting a new one. Clicking another row while
    /// the editor is up simply retargets it (the unsaved draft is dropped).
    fn us_edit(&self, idx: usize) {
        let Some(ui) = self.ui() else { return };
        // Row indices resolve through the row-id mirror, exactly like toggle
        // and delete; the script itself comes from the Store (the source of
        // truth, and the only place holding the full code).
        let Some(id) = self.us_ids.borrow().get(idx).copied() else { return };
        let Some(script) = self.store.borrow().userscripts().into_iter().find(|s| s.id == id)
        else { return }; // vanished between refresh and click — nothing to edit
        *self.us_edit_id.borrow_mut() = Some(id);
        ui.set_us_name(script.name.as_str().into());
        ui.set_us_patterns(script.patterns.join(crate::userscripts::PATTERN_SEPARATOR).into());
        ui.set_us_code(script.code.as_str().into());
        ui.set_us_error(SharedString::default());
        ui.set_us_editing_existing(true);
        ui.set_us_editing(true);
    }

    /// Save the editor — as a **new** script (next id) or **in place** (the
    /// id of the row that opened the editor; `enabled` and list position are
    /// the stored script's, everything else the editor's). Validation is the
    /// Store's (`upsert_userscript`): a rejected script is **not** added,
    /// the editor stays up, and the `StoreError` text becomes the menu's
    /// one-line error. An accepted Save rebroadcasts the new enabled set.
    fn us_save(&self) {
        let Some(ui) = self.ui() else { return };
        let (name, patterns, code) = (
            ui.get_us_name().to_string(),
            ui.get_us_patterns().to_string(),
            ui.get_us_code().to_string(),
        );
        let editing = *self.us_edit_id.borrow();
        let script = match editing {
            // Edit: keep the id and the enabled flag (the CheckBox owns that);
            // if the row was deleted out from under the editor, the upsert
            // simply re-inserts under the still-unused id.
            Some(id) => Userscript {
                id,
                name: name.trim().to_string(),
                patterns: parse_patterns(&patterns),
                code,
                enabled: self
                    .store
                    .borrow()
                    .userscripts()
                    .iter()
                    .find(|s| s.id == id)
                    .is_some_and(|s| s.enabled),
            },
            // New: one id per Save, read from the Store at this instant — the
            // shell is the only writer and Save is sequential, so
            // `next_userscript_id` is fresh here; two Saves always produce two
            // distinct scripts. (An allocator that reads `next` up-front and
            // upserts later hands the same id out twice — the silent overwrite
            // the userscripts e2e hit.)
            None => build_new_userscript(self.store.borrow().next_userscript_id(), &name, &patterns, &code),
        };
        if let Err(err) = self.store.borrow_mut().upsert_userscript(script) {
            warn!("userscript {name:?} rejected: {err}");
            ui.set_us_error(err.to_string().into());
            return; // script not added/changed; the editor keeps the draft
        }
        *self.us_edit_id.borrow_mut() = None;
        ui.set_us_error(SharedString::default());
        ui.set_us_editing(false);
        ui.set_us_editing_existing(false);
        ui.set_us_name(SharedString::default());
        ui.set_us_patterns(SharedString::default());
        ui.set_us_code(SharedString::default());
        self.us_broadcast();
        self.us_refresh();
    }

    /// "Cancel": drop the draft (or the edit), keep the list.
    fn us_cancel(&self) {
        let Some(ui) = self.ui() else { return };
        *self.us_edit_id.borrow_mut() = None;
        ui.set_us_editing(false);
        ui.set_us_editing_existing(false);
        ui.set_us_error(SharedString::default());
    }

    /// The script set as it rides the wire: **all enabled** scripts (contract:
    /// disabled ones are never sent; an empty set clears a tab).
    fn us_payload(&self) -> Vec<Userscript> {
        enabled_userscripts(&self.store.borrow().userscripts())
    }

    /// After any mutation (toggle / save / delete): push the new enabled set to
    /// every **live** tab, best effort. Crashed and already-closed tabs are
    /// skipped, and a send that fails on a tab the drain tick has not yet
    /// noticed dying is ignored — `check_crashes` makes that noise, matching
    /// the existing crash handling (see `set_visible_except`).
    fn us_broadcast(&self) {
        let scripts = self.us_payload();
        let mut procs = self.procs.borrow_mut();
        for p in procs.iter_mut() {
            if p.crashed || p.state.closed {
                continue;
            }
            if let Err(err) = p.tab.send(ToTab::SetUserscripts { scripts: scripts.clone() }) {
                debug!("userscripts not delivered to tab {}: {err}", p.tab.id);
            }
        }
    }

    /// Hand the current enabled set to one freshly spawned tab process: a new
    /// process starts empty, so the shell seeds it (the empty set is sent too
    /// — it provably clears a respawned tab).
    fn seed_userscripts(&self, tid: TabId) {
        let scripts = self.us_payload();
        let _ = self.send_to(tid, ToTab::SetUserscripts { scripts });
    }

    // ---- User-Agent switcher (task: per-container UA) -------------------------

    /// The container of the active tab — the one the UA menu reads and
    /// mutates (and the container manager selects on open).
    fn active_container(&self) -> ContainerId {
        self.active_proc_index()
            .map(|i| self.procs.borrow()[i].tab.container)
            .unwrap_or_else(|| self.store.borrow().default_container_id())
    }

    /// Push the menu's live state for the active tab's container: the
    /// `ua-current` display label and the `ua-rows` list (default row first,
    /// then the five presets, the container's value highlighted). Called on
    /// every (re)open, after every mutation, and on every tab switch.
    fn ua_state(&self, ui: &AppWindow) {
        let current = container_user_agent(&self.store.borrow(), self.active_container());
        ui.set_ua_current(ua_display(current.as_deref()).as_str().into());
        ui.set_ua_rows(ModelRc::from(Rc::new(VecModel::from(ua_menu_rows(
            current.as_deref(),
        )))));
    }

    /// A tab switch moved the menu to another container: re-point rows and
    /// current label (and drop the previous container's stale error line).
    fn ua_refresh_if_open(&self) {
        if let Some(ui) = self.ui() {
            if ui.get_ua_menu_open() {
                self.ua_state(&ui);
                ui.set_ua_error(SharedString::default());
            }
        }
    }

    /// Open/close the UA overlay. Opening re-reads the Store for the active
    /// tab's container, prefills the custom draft with the container's raw
    /// value, and closes the userscript menu **and** the container manager —
    /// two full-window overlays never stack, and both close this one the same
    /// way (see [`Core::manager_open`] / [`Core::userscript_menu`]).
    fn ua_menu(&self, open: bool) {
        let Some(ui) = self.ui() else { return };
        debug!("user-agent menu {}", if open { "opened" } else { "closed" });
        if open {
            ui.set_manager_open(false);
            ui.set_us_menu_open(false);
            ui.set_ua_error(SharedString::default());
            let current = container_user_agent(&self.store.borrow(), self.active_container());
            // Prefill the draft with exactly what the container holds (a
            // custom string shows itself; a preset or the default starts
            // blank — like the userscript editor prefilling its row).
            let draft = current
                .filter(|ua| ironsurf_common::builtin_user_agent_label(ua).is_none())
                .unwrap_or_default();
            ui.set_ua_custom(draft.as_str().into());
            self.ua_state(&ui);
        }
        ui.set_ua_menu_open(open);
    }

    /// The toolbar's "UA" button: flip the overlay.
    fn toggle_ua_menu(&self) {
        let open = self.ui().is_some_and(|ui| ui.get_ua_menu_open());
        self.ua_menu(!open);
    }

    /// Persist `ua` for the container the menu currently targets and push it
    /// onto that container's live tabs. A Store rejection (bad characters,
    /// too long) rides the one-line `ua-error` and changes nothing at all —
    /// rows, stored value, and the wire stay as they were, exactly like the
    /// userscripts Save leg.
    fn apply_ua(&self, ui: &AppWindow, ua: Option<&str>) {
        let cid = self.active_container();
        if let Err(err) = self.store.borrow_mut().set_container_user_agent(cid, ua) {
            warn!("container {} rejected a user agent: {err}", cid.0);
            ui.set_ua_error(err.to_string().into());
            return;
        }
        ui.set_ua_error(SharedString::default());
        self.ua_broadcast(cid, ua);
        self.ua_state(ui);
    }

    /// A row was clicked: select that UA for the container. Row indices
    /// resolve through the pushed `ua-rows` model — row 0 (empty `ua`) is
    /// "Default (IronSurf)", the others carry their preset string verbatim.
    fn ua_select(&self, idx: usize) {
        let Some(ui) = self.ui() else { return };
        let Some(row) = ui.get_ua_rows().row_data(idx) else { return };
        let ua = if row.ua.is_empty() { None } else { Some(row.ua.as_str()) };
        self.apply_ua(&ui, ua);
    }

    /// "Restore default": the container drops its override.
    fn ua_restore_default(&self) {
        if let Some(ui) = self.ui() {
            self.apply_ua(&ui, None);
        }
    }

    /// The custom-UA LineEdit → Apply: [`normalize_custom_ua`] first (trim;
    /// all-blank = restore the default), then the same Store leg as a row
    /// pick. A rejected string shows on the one line **with the draft kept**;
    /// an accepted one clears the draft and updates rows and label.
    fn ua_apply_custom(&self, input: &str) {
        let Some(ui) = self.ui() else { return };
        let normalized = normalize_custom_ua(input);
        self.apply_ua(&ui, normalized.as_deref());
        if container_user_agent(&self.store.borrow(), self.active_container()) == normalized {
            // only after success — on rejection the draft must survive as typed
            ui.set_ua_custom(SharedString::default());
        }
    }

    /// "Copy": the *effective* UA of the menu's container on the session
    /// clipboard — the override if there is one, the tab default otherwise.
    fn ua_copy(&self) {
        let effective = container_user_agent(&self.store.borrow(), self.active_container())
            .unwrap_or_else(|| DEFAULT_USER_AGENT.to_string());
        self.clipboard_set(&effective);
    }

    /// After any accepted mutation: push the new value to every **live** tab
    /// of that container only — tabs in other containers must not receive it
    /// (each tab applies its own container's value). Crashed and closed tabs
    /// are skipped and send failures ignored, exactly like
    /// [`Core::us_broadcast`] — the crash reporter owns those tabs.
    fn ua_broadcast(&self, container: ContainerId, ua: Option<&str>) {
        let mut procs = self.procs.borrow_mut();
        for p in procs.iter_mut() {
            if p.tab.container != container || p.crashed || p.state.closed {
                continue;
            }
            let msg = ToTab::SetUserAgent { user_agent: ua.map(str::to_string) };
            if let Err(err) = p.tab.send(msg) {
                debug!("user agent not delivered to tab {}: {err}", p.tab.id);
            }
        }
    }

    // ---- persistence -------------------------------------------------------------

    fn save_settings(&self) {
        let settings = self.settings.borrow().clone();
        let _ = settings.save(&self.ui_path); // best-effort
    }

    // ---- message pump --------------------------------------------------------------

    /// Drain every queued `FromTab` message: reduce per-tab state, keep only
    /// the newest frame for the active tab, reap closed tabs, sync the UI.
    pub fn drain(&self) {
        // Superseded hover moves go out here, one per tick at the latest.
        self.flush_pending_move();
        // A child can die between messages (panic, kill): without a poll the
        // tab would go silent and the shell would keep painting its stale
        // frame as if nothing happened.
        self.check_crashes();
        let msgs: Vec<PumpedMessage> = self.rx.borrow_mut().try_iter().collect();
        if msgs.is_empty() {
            self.check_resize();
            return;
        }
        let active = self.active_tab_id();
        let mut dirty = false;
        // Console/network messages arrived this tick: `reduce` reports `false`
        // for them on purpose (they must never dirty the frame), so the open
        // panel needs its own nudge below.
        let mut devtools_feed = false;
        let mut newest_frame: Option<Image> = None;

        for (tid, msg) in msgs {
            // Diagnostics first: the message we drop below is the one thing
            // that must never be formatted (up to ~4 MB per frame).
            match &msg {
                FromTab::Frame { width, height, scale, rgba } => trace!(
                    "tab {tid} frame {}x{}@{scale:.2} ({} B)",
                    width,
                    height,
                    rgba.len()
                ),
                FromTab::Error { message } => error!("tab {tid} error: {message}"),
                FromTab::ConsoleLog { source, level, message } => {
                    devtools_feed = true;
                    info!("tab {tid} console[{source}:{level}]: {message}")
                }
                // DevTools network panel: the row data is the reduced per-tab
                // log (`TabState::network`); nothing to log per message.
                FromTab::NetworkEvent { .. } => devtools_feed = true,
                FromTab::UrlChanged { url } => {
                    info!("tab {tid} url → {}", url.as_deref().unwrap_or("<none>"))
                }
                FromTab::TitleChanged { title } => {
                    info!("tab {tid} title → {}", title.as_deref().unwrap_or("<none>"))
                }
                FromTab::LoadingChanged { loading } => info!(
                    "tab {tid} loading {}",
                    if *loading { "started" } else { "finished" }
                ),
                FromTab::Closed => info!("tab {tid} says it is closed"),
                FromTab::OpenLinkInNewTab { url } => {
                    info!("tab {tid} middle-click: opening {url} in a new tab")
                }
                FromTab::ContextMenu { x, y, target } => {
                    info!("tab {tid} right-click at ({x:.0},{y:.0}): {target:?}")
                }
                FromTab::DialogRequest { dialog_id, kind, message, .. } => {
                    info!("tab {tid} opened a {kind:?} dialog #{dialog_id}: {message:?}")
                }
                FromTab::ClipboardText { text } => {
                    // Page copy (Ctrl+C/X, editor cut): the shell owns the
                    // session clipboard (task: context menu).
                    self.clipboard_set(text);
                }
                other => debug!("tab {tid} → {other:?}"),
            }
            if let FromTab::Frame { width, height, ref rgba, .. } = msg {
                if Some(tid) == active {
                    if let Some(img) = image_from_rgba(width, height, rgba) {
                        newest_frame = Some(img);
                    }
                }
                continue;
            }
            let changed = {
                let mut procs = self.procs.borrow_mut();
                procs
                    .iter_mut()
                    .find(|p| p.tab.id == tid)
                    .map(|p| p.state.reduce(&msg))
                    .unwrap_or(false)
            };
            dirty |= changed;
            // Container history: one line per navigation (best effort).
            if let FromTab::UrlChanged { url: Some(url) } = &msg {
                let entry = {
                    let procs = self.procs.borrow();
                    procs
                        .iter()
                        .find(|p| p.tab.id == tid)
                        .map(|p| (p.tab.container, p.state.title.clone()))
                };
                if let Some((container, title)) = entry {
                    let store = self.store.borrow();
                    let cs = ironsurf_storage::ContainerStore::open(&store, container);
                    if let Err(err) = cs.append_history(&url, Some(&title)) {
                        warn!("history entry for {url} not written: {err}");
                    } else {
                        debug!("history: recorded {url} in container {}", container.0);
                    }
                }
            }
            // Middle-click "open link in new tab": spawn a sibling tab in the
            // requesting tab's container; the requester itself never navigates
            // (its runtime swallows the middle-click release).
            if let FromTab::OpenLinkInNewTab { url } = &msg {
                let container = {
                    let procs = self.procs.borrow();
                    procs
                        .iter()
                        .find(|p| p.tab.id == tid)
                        .map(|p| p.tab.container)
                };
                if let Some(container) = container {
                    self.open_tab(container, Some(url));
                }
            }
            // Right-click → the context menu opens over the page at the
            // click point (task: context menu).
            if let FromTab::ContextMenu { x, y, target } = &msg {
                self.open_context_menu(*x, *y, target);
            }
            // `alert`/`confirm`/`prompt` → the tab is now blocked on the
            // pipe; the overlay opens (on the requesting tab, brought to
            // the front) and the answer goes back via OK/Cancel.
            if let FromTab::DialogRequest { dialog_id, kind, message, default } = &msg {
                self.open_dialog(tid, *dialog_id, *kind, message, default.as_deref());
            }
            // Hover cursor (task: cursor icon): the page says what shape the
            // pointer is over it. Only the active tab ever sees pointer
            // events, but a background message must not repaint the wrong
            // page — the per-tab record (state.reduce) still keeps it.
            if let FromTab::CursorChanged { cursor } = &msg {
                if Some(tid) == active {
                    self.apply_page_cursor(*cursor);
                }
            }
        }

        // Reap closed tabs (dropping a TabProc closes stdin and reaps it).
        let (before, after) = {
            let mut procs = self.procs.borrow_mut();
            let before = procs.len();
            procs.retain(|p| !p.state.closed);
            let after = procs.len();
            if after == 0 {
                drop(procs);
                let _ = slint::quit_event_loop();
            } else {
                self.active.set(self.active.get().min(after - 1));
            }
            (before, after)
        };
        if before != after {
            // A tab that said `Closed` while its modal was queued or shown
            // takes the overlay with it (the request ids die with the
            // process); the queue rolls to the next live dialog.
            let queued = self
                .pending_dialogs
                .borrow()
                .iter()
                .map(|p| p.tab)
                .collect::<Vec<_>>();
            for tab in queued {
                if !self.procs.borrow().iter().any(|p| p.tab.id == tab) {
                    self.clear_dialog_for(tab, false);
                }
            }
        }
        if before != after {
            info!("reaped {} closed tab(s); {} remain", before - after, after);
            dirty = true;
        }

        if dirty {
            self.sync_ui();
        } else if devtools_feed {
            // Panel open or not, keep the table current: pushing into a panel
            // nobody looks at is cheap, and it means the pane is complete the
            // instant F12 opens it.
            let procs = self.procs.borrow();
            if let Some(ui) = self.ui() {
                self.write_devtools(&ui, &procs);
            }
        }
        if let (Some(img), Some(ui)) = (newest_frame, self.ui()) {
            ui.set_page_image(img);
            ui.window().request_redraw();
        }
    }

    /// Notice tab processes that died without saying `Closed` (crash, kill):
    /// log the exit + captured backtrace and pin “Tab N has crashed” onto each
    /// victim's status line. Crashed tabs are kept in the strip (their frame
    /// is the last thing the user asked for); ✕ still closes them.
    fn check_crashes(&self) {
        let mut crashed = false;
        let mut victims = Vec::new();
        {
            let mut procs = self.procs.borrow_mut();
            for p in procs.iter_mut() {
                if p.crashed || p.state.closed {
                    continue;
                }
                if let Some(status) = p.tab.try_wait_status() {
                    note_tab_crash(p, format_args!("the process exited ({status}) without closing cleanly"));
                    victims.push(p.tab.id);
                    crashed = true;
                }
            }
        }
        // A crashed tab that was blocked on a modal dialog takes the overlay
        // down with it — the answer cannot reach a dead process (task: JS
        // dialogs).
        for tid in victims {
            self.clear_dialog_for(tid, false);
        }
        if crashed {
            self.sync_ui();
        }
    }

    fn tick_spinner(&self) {
        let Some(ui) = self.ui() else { return };
        let loading = self
            .active_proc_index()
            .map(|i| self.procs.borrow()[i].state.loading)
            .unwrap_or(false);
        let want: SharedString = if loading {
            let i = (self.spinner.get() + 1) % SPINNER_FRAMES.len();
            self.spinner.set(i);
            SPINNER_FRAMES[i].to_string().into()
        } else {
            " ".into()
        };
        if ui.get_spinner() != want {
            ui.set_spinner(want);
        }
    }

    /// Push a full snapshot of the model into the window properties.
    pub fn sync_ui(&self) {
        let Some(ui) = self.ui() else { return };
        let procs = self.procs.borrow();
        let store = self.store.borrow();
        let active = self.active.get().min(procs.len().saturating_sub(1));

        let tabs: Vec<TabInfo> = procs
            .iter()
            .enumerate()
            .map(|(i, p)| TabInfo {
                id: p.tab.id.0 as i32,
                title: p.state.title.clone().into(),
                color: color_hex(
                    store
                        .container(p.tab.container)
                        .map(|c| c.color.as_str())
                        .unwrap_or("#9e9e9e"),
                ),
                active: i == active,
            })
            .collect();
        ui.set_tabs(ModelRc::from(Rc::new(VecModel::from(tabs))));

        let conts: Vec<ContainerInfo> = store
            .containers()
            .iter()
            .map(|c| ContainerInfo {
                name: c.name.clone().into(),
                color: color_hex(&c.color),
                isDefault: c.is_default,
            })
            .collect();
        ui.set_containers(ModelRc::from(Rc::new(VecModel::from(conts))));

        if let Some(p) = procs.get(active) {
            ui.set_can_back(p.state.can_back);
            ui.set_can_fwd(p.state.can_forward);
            ui.set_url_text(p.state.url.clone().unwrap_or_default().into());
            // The tab's hover cursor comes back with the tab (task: cursor
            // icon) — switching tabs must not leave the previous page's hand
            // (or I-beam) drawn over this one.
            ui.set_page_cursor(slint_cursor(p.state.cursor));
            // The status line is *one* message, always the newest: an error is
            // dropped when the next load starts (TabState::reduce), a load
            // with nothing reported yet still says so instead of going blank.
            let status = p
                .state
                .error
                .clone()
                .or_else(|| p.state.status.clone())
                .or_else(|| self.spawn_error.borrow().clone())
                .or_else(|| {
                    p.state.loading.then(|| "Loading \u{2026}".to_string())
                })
                .unwrap_or_default();
            ui.set_status_text(status.into());
            let (glyph, color) = p.state.security().render();
            ui.set_sec_glyph(glyph.into());
            ui.set_sec_color(color_hex(color));
            if let Some(c) = store.container(p.tab.container) {
                ui.set_container_label(c.name.clone().into());
            }
        } else {
            let status = self.spawn_error.borrow().clone().unwrap_or_default();
            ui.set_status_text(status.into());
        }
        ui.set_active_index(active as i32);
        // The DevTools panel renders the active tab too (pane, counts, rows):
        // toggling, switching tabs and closing it all land here.
        self.write_devtools(&ui, &procs);
        // The spinner must react to LoadingChanged immediately, not on the next
        // tick of the spinner timer — a click on a link should start turning
        // the moment the tab says it is loading.
        self.tick_spinner();
    }

    // ---- wiring --------------------------------------------------------------------

    fn attach_callbacks(self: &Rc<Self>, ui: &AppWindow) {
        macro_rules! bind0 {
            ($setter:ident, $body:expr) => {{
                let c = self.clone();
                ui.$setter(move || $body(&c));
            }};
        }
        bind0!(on_go_back, |c: &Rc<Core>| c.send_active(ToTab::Back));
        bind0!(on_go_forward, |c: &Rc<Core>| c.send_active(ToTab::Forward));
        bind0!(on_reload, |c: &Rc<Core>| c.send_active(ToTab::Reload));
        bind0!(on_new_tab, |c: &Rc<Core>| {
            let cid = c.new_tab_container();
            c.open_tab(cid, Some(&default_url()));
        });
        bind0!(on_open_manager, |c: &Rc<Core>| c.manager_open(true));
        bind0!(on_close_manager, |c: &Rc<Core>| c.manager_open(false));
        bind0!(on_mgr_add, |c: &Rc<Core>| c.mgr_add());

        {
            let c = self.clone();
            ui.on_navigate(move |url| c.navigate(url.as_str()));
        }
        {
            let c = self.clone();
            ui.on_activate_tab(move |idx| {
                if idx >= 0 {
                    c.activate_tab(idx as usize);
                }
            });
        }
        {
            let c = self.clone();
            ui.on_close_tab(move |idx| {
                if idx >= 0 {
                    c.close_tab(idx as usize);
                }
            });
        }
        {
            let c = self.clone();
            ui.on_mgr_new_tab_in(move |idx| {
                if idx >= 0 {
                    c.mgr_new_tab_in(idx as usize);
                }
            });
        }
        {
            let c = self.clone();
            ui.on_mgr_move_tab_to(move |idx| {
                if idx >= 0 {
                    c.mgr_move_tab_to(idx as usize);
                }
            });
        }
        {
            let c = self.clone();
            ui.on_mgr_delete(move |idx| {
                if idx >= 0 {
                    c.mgr_delete(idx as usize);
                }
            });
        }
        {
            let c = self.clone();
            ui.on_mgr_start_rename(move |idx| {
                if idx >= 0 {
                    c.mgr_start_rename(idx as usize);
                }
            });
        }
        {
            let c = self.clone();
            ui.on_mgr_rename(move |idx| {
                if idx >= 0 {
                    c.mgr_rename(idx as usize);
                }
            });
        }
        {
            let c = self.clone();
            ui.on_mgr_recolor(move |idx, pal| {
                if idx >= 0 && pal >= 0 {
                    c.mgr_recolor(idx as usize, pal as usize);
                }
            });
        }
        {
            let c = self.clone();
            ui.on_mgr_select(move |idx| c.mgr_select(idx));
        }
        {
            let c = self.clone();
            ui.on_page_key(move |pressed, text, ctrl, shift, alt, meta, repeat| {
                c.page_key(pressed, text.as_str(), ctrl, shift, alt, meta, repeat)
            });
        }
        {
            let c = self.clone();
            ui.on_page_pointer(move |x, y, kind, button| c.page_pointer(x, y, kind, button));
        }
        {
            let c = self.clone();
            ui.on_page_wheel(move |x, y, dx, dy| c.page_wheel(x, y, dx, dy));
        }
        {
            let c = self.clone();
            ui.on_menu_pick(move |idx| {
                if idx >= 0 {
                    c.menu_pick(idx as usize);
                }
            });
        }
        bind0!(on_menu_close, |c: &Rc<Core>| c.close_context_menu());

        // JS dialogs (task: window.alert/confirm/prompt): the two buttons of
        // the overlay — and the prompt box's Enter — answer the blocked tab.
        bind0!(on_dialog_accept, |c: &Rc<Core>| c.answer_dialog(true));
        bind0!(on_dialog_cancel, |c: &Rc<Core>| c.answer_dialog(false));

        // DevTools panel: the models are installed once and patched in place
        // (see `write_devtools`), the two intents come back to the core.
        ui.set_console_rows(ModelRc::from(self.dt_console.clone()));
        ui.set_net_rows(ModelRc::from(self.dt_net.clone()));
        {
            let c = self.clone();
            ui.on_devtools_select_view(move |view| c.set_devtools_view(DevToolsView::from_index(view)));
        }
        bind0!(on_devtools_close, |c: &Rc<Core>| c.close_devtools());

        // Userscript menu (task: userscript menu): the toolbar toggle, the
        // scrim/✕ close, and the row/footer intents — every mutation lands in
        // the Store at once and rebroadcasts the enabled set to all live tabs.
        bind0!(on_toggle_us_menu, |c: &Rc<Core>| c.toggle_userscript_menu());
        bind0!(on_close_us_menu, |c: &Rc<Core>| c.userscript_menu(false));
        bind0!(on_us_new, |c: &Rc<Core>| c.us_new());
        bind0!(on_us_save, |c: &Rc<Core>| c.us_save());
        bind0!(on_us_cancel, |c: &Rc<Core>| c.us_cancel());
        {
            let c = self.clone();
            ui.on_us_toggle(move |idx| {
                if idx >= 0 {
                    c.us_toggle(idx as usize);
                }
            });
        }
        {
            let c = self.clone();
            ui.on_us_delete(move |idx| {
                if idx >= 0 {
                    c.us_delete(idx as usize);
                }
            });
        }
        {
            let c = self.clone();
            ui.on_us_edit(move |idx| {
                if idx >= 0 {
                    c.us_edit(idx as usize);
                }
            });
        }

        // User-Agent switcher (task: per-container UA): the toolbar toggle,
        // the scrim/✕ close, and the row/footer intents — every accepted
        // mutation lands in the Store at once and rebroadcasts
        // ToTab::SetUserAgent to the live tabs of that container only.
        bind0!(on_toggle_ua_menu, |c: &Rc<Core>| c.toggle_ua_menu());
        bind0!(on_close_ua_menu, |c: &Rc<Core>| c.ua_menu(false));
        bind0!(on_ua_copy, |c: &Rc<Core>| c.ua_copy());
        bind0!(on_ua_restore_default, |c: &Rc<Core>| c.ua_restore_default());
        {
            let c = self.clone();
            ui.on_ua_select(move |idx| {
                if idx >= 0 {
                    c.ua_select(idx as usize);
                }
            });
        }
        {
            let c = self.clone();
            ui.on_ua_apply_custom(move |text| c.ua_apply_custom(text.as_str()));
        }
    }
}

/// Grow, shrink and patch a shared Slint model in place. Appending only the
/// rows that arrived keeps the list's scroll position (a fresh `VecModel` per
/// tick would throw it away, so a page's log would jump to the bottom every
/// 33 ms while the user read the middle of it).
fn sync_model<T: Clone + PartialEq + 'static>(model: &VecModel<T>, want: &[T]) {
    let have = model.row_count();
    let shared = have.min(want.len());
    for i in 0..shared {
        if model.row_data(i).map_or(true, |row| row != want[i]) {
            model.set_row_data(i, want[i].clone());
        }
    }
    for i in shared..want.len() {
        model.push(want[i].clone());
    }
    while model.row_count() > want.len() {
        model.remove(model.row_count() - 1);
    }
}

// ---------------------------------------------------------------------------
// context menu (task: context menu)

/// What a context-menu row does when picked. Clipboard, new-tab and history
/// entries are serviced by the shell alone; the engine-side ones (paste,
/// video play/pause) travel to the tab as `ToTab::ContextAction`.
#[derive(Clone, Debug, PartialEq)]
pub enum MenuAction {
    Back,
    Forward,
    Reload,
    /// Same container as the requesting tab (the middle-click rule).
    OpenNewTab(String),
    CopyText(String),
    Paste,
    ToggleVideo,
}

/// The menu for one right-click, in display order. Entries exist exactly
/// when their thing is there: link rows only over a link, video rows only
/// over a video, Paste only next to a caret *and* a non-empty clipboard.
/// Pure (labels + actions), so the entry matrix is unit-testable headless.
pub fn context_menu_items(
    target: &ContextTarget,
    url: Option<&str>,
    can_back: bool,
    can_forward: bool,
    clipboard_empty: bool,
) -> Vec<(String, MenuAction)> {
    let mut items: Vec<(String, MenuAction)> = Vec::new();
    if can_back {
        items.push(("← Back".into(), MenuAction::Back));
    }
    if can_forward {
        items.push(("→ Forward".into(), MenuAction::Forward));
    }
    items.push(("⟳ Reload".into(), MenuAction::Reload));
    if let Some(link) = &target.link {
        items.push((
            "Open link in new tab".into(),
            MenuAction::OpenNewTab(link.clone()),
        ));
    }
    if let Some(image) = &target.image {
        items.push((
            "Open image in new tab".into(),
            MenuAction::OpenNewTab(image.clone()),
        ));
    }
    if let Some(selection) = &target.selection {
        items.push(("Copy".into(), MenuAction::CopyText(selection.clone())));
    }
    if let Some(link) = &target.link {
        items.push((
            "Copy link address".into(),
            MenuAction::CopyText(link.clone()),
        ));
    }
    if let Some(image) = &target.image {
        items.push((
            "Copy image address".into(),
            MenuAction::CopyText(image.clone()),
        ));
    }
    if target.video.is_some() {
        items.push((
            if target.video_playing {
                "❚❚ Pause video"
            } else {
                "▶ Play video"
            }
            .into(),
            MenuAction::ToggleVideo,
        ));
        // A `src`-less `<video>` has no address to copy.
        if target.video.as_deref().is_some_and(|v| !v.is_empty()) {
            items.push((
                "Copy video address".into(),
                MenuAction::CopyText(target.video.clone().unwrap_or_default()),
            ));
        }
    }
    if target.editable && !clipboard_empty {
        items.push(("Paste".into(), MenuAction::Paste));
    }
    if let Some(page) = url {
        if !page.is_empty() {
            items.push((
                "Copy page address".into(),
                MenuAction::CopyText(page.to_owned()),
            ));
        }
    }
    items
}

/// A Ctrl+V key press (Slint delivers control characters for chords, so
/// both spellings count).
pub fn is_paste_chord(text: &str, ctrl: bool, alt: bool, meta: bool) -> bool {
    (ctrl || meta) && !alt && matches!(text, "v" | "V" | "\u{16}")
}

// ---------------------------------------------------------------------------
// helpers

/// Coerce raw RGBA bytes (straight alpha, top-down) into a Slint image.
pub fn image_from_rgba(width: u32, height: u32, rgba: &[u8]) -> Option<Image> {
    if width == 0 || height == 0 {
        return None;
    }
    if rgba.len() != width as usize * height as usize * 4 {
        return None; // corrupt frame: drop it
    }
    let mut buffer = SharedPixelBuffer::<Rgba8Pixel>::new(width, height);
    buffer.make_mut_bytes().copy_from_slice(rgba);
    Some(Image::from_rgba8(buffer))
}

/// Rate limit for forwarded hover moves: one per UI frame at most. A 1000 Hz
/// gaming mouse or a fast hand across the page otherwise floods the tab with
/// positions it cannot hit-test in time, and the click at the end of the swipe
/// waits behind all of them.
pub const MOVE_INTERVAL: Duration = Duration::from_millis(33);

/// May a hover move be forwarded now? `last` is when the previous one went out
/// (`None` = none sent yet, so the first always passes).
pub fn move_due(now: Instant, last: Option<Instant>) -> bool {
    match last {
        Some(t) => now.duration_since(t) >= MOVE_INTERVAL,
        None => true,
    }
}

/// IPC cursor shape → the Slint shape the window draws (task: cursor icon).
/// Every IPC shape but `VerticalText` has a Slint twin (the I-beam is the
/// nearest stand-in); the mapping is exhaustive so a new engine shape can
/// never silently render as an arrow… it renders as exactly what was agreed.
pub fn slint_cursor(cursor: ironsurf_common::CursorIcon) -> MouseCursor {
    use ironsurf_common::CursorIcon;
    match cursor {
        CursorIcon::Default => MouseCursor::Default,
        CursorIcon::Hidden => MouseCursor::None,
        CursorIcon::Help => MouseCursor::Help,
        CursorIcon::Pointer => MouseCursor::Pointer,
        CursorIcon::Progress => MouseCursor::Progress,
        CursorIcon::Wait => MouseCursor::Wait,
        CursorIcon::Crosshair => MouseCursor::Crosshair,
        CursorIcon::Text => MouseCursor::Text,
        CursorIcon::VerticalText => MouseCursor::Text,
        CursorIcon::Alias => MouseCursor::Alias,
        CursorIcon::Copy => MouseCursor::Copy,
        CursorIcon::Move => MouseCursor::Move,
        CursorIcon::NoDrop => MouseCursor::NoDrop,
        CursorIcon::NotAllowed => MouseCursor::NotAllowed,
        CursorIcon::Grab => MouseCursor::Grab,
        CursorIcon::Grabbing => MouseCursor::Grabbing,
        CursorIcon::ColResize => MouseCursor::ColResize,
        CursorIcon::RowResize => MouseCursor::RowResize,
        CursorIcon::NResize => MouseCursor::NResize,
        CursorIcon::EResize => MouseCursor::EResize,
        CursorIcon::SResize => MouseCursor::SResize,
        CursorIcon::WResize => MouseCursor::WResize,
        CursorIcon::NeResize => MouseCursor::NeResize,
        CursorIcon::NwResize => MouseCursor::NwResize,
        CursorIcon::SeResize => MouseCursor::SeResize,
        CursorIcon::SwResize => MouseCursor::SwResize,
        CursorIcon::EwResize => MouseCursor::EwResize,
        CursorIcon::NsResize => MouseCursor::NsResize,
        CursorIcon::NeswResize => MouseCursor::NeswResize,
        CursorIcon::NwseResize => MouseCursor::NwseResize,
    }
}

/// `#rrggbb` → Color, tolerant of bad input.
pub fn color_hex(hex: &str) -> Color {
    let h = hex.strip_prefix('#').unwrap_or(hex);
    if h.len() == 6 {
        if let (Ok(r), Ok(g), Ok(b)) = (
            u8::from_str_radix(&h[0..2], 16),
            u8::from_str_radix(&h[2..4], 16),
            u8::from_str_radix(&h[4..6], 16),
        ) {
            return Color::from_rgb_u8(r, g, b);
        }
    }
    Color::from_rgb_u8(158, 158, 158)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_move_always_passes() {
        let now = Instant::now();
        assert!(move_due(now, None));
    }

    #[test]
    fn moves_inside_one_frame_are_dropped_until_the_interval_passes() {
        let t0 = Instant::now();
        assert!(move_due(t0, None));
        // A burst inside the same UI frame: nothing more goes out…
        for i in 1..10 {
            assert!(
                !move_due(t0 + Duration::from_millis(i * 3), Some(t0)),
                "move {i} must be dropped (superseded positions cost the tab a repaint each)"
            );
        }
        // …until the next frame's worth of time has elapsed.
        assert!(move_due(t0 + MOVE_INTERVAL, Some(t0)));
        assert!(move_due(t0 + MOVE_INTERVAL * 4, Some(t0)));
    }

    #[test]
    fn a_press_never_waits() {
        // `page_pointer` only consults the gate for Move; every other kind is
        // sent straight away. Assert the gate itself is the only thing that can
        // hold a pointer event back, i.e. presses do not call it with a recent
        // timestamp and get denied.
        let t0 = Instant::now();
        assert!(move_due(t0, None), "gate opens for the first move");
        assert!(!move_due(t0, Some(t0)), "gate closes again inside a frame");
    }

    // ---- context menu (task: right-click menu) ------------------------------

    fn target(
        link: Option<&str>,
        image: Option<&str>,
        video: Option<&str>,
        selection: Option<&str>,
        editable: bool,
    ) -> ContextTarget {
        ContextTarget {
            link: link.map(String::from),
            image: image.map(String::from),
            video: video.map(String::from),
            video_playing: false,
            selection: selection.map(String::from),
            editable,
        }
    }

    #[test]
    fn the_plain_page_menu_is_history_reload_and_address() {
        let items = context_menu_items(&target(None, None, None, None, false), Some("https://example.com/"), true, false, true);
        let labels: Vec<&str> = items.iter().map(|(l, _)| l.as_str()).collect();
        assert_eq!(labels, ["← Back", "⟳ Reload", "Copy page address"]);
        assert_eq!(items[2].1, MenuAction::CopyText("https://example.com/".into()));
    }

    #[test]
    fn a_link_earns_its_own_row_and_its_address_row() {
        let items = context_menu_items(
            &target(Some("https://example.com/a"), None, None, None, false),
            Some("https://example.com/"),
            false,
            false,
            true,
        );
        let labels: Vec<&str> = items.iter().map(|(l, _)| l.as_str()).collect();
        assert_eq!(
            labels,
            ["⟳ Reload", "Open link in new tab", "Copy link address", "Copy page address"]
        );
        assert_eq!(
            items[1].1,
            MenuAction::OpenNewTab("https://example.com/a".into())
        );
    }

    #[test]
    fn selection_image_and_video_entries_only_appear_over_their_own_thing() {
        let items = context_menu_items(
            &target(
                None,
                Some("https://example.com/i.png"),
                Some("https://example.com/v.mp4"),
                Some("selected words"),
                true,
            ),
            Some("https://example.com/"),
            false,
            true,
            false,
        );
        let labels: Vec<&str> = items.iter().map(|(l, _)| l.as_str()).collect();
        assert_eq!(
            labels,
            [
                "→ Forward",
                "⟳ Reload",
                "Open image in new tab",
                "Copy",
                "Copy image address",
                "▶ Play video",
                "Copy video address",
                "Paste",
                "Copy page address",
            ]
        );
        assert_eq!(items[3].1, MenuAction::CopyText("selected words".into()));
        assert_eq!(items[7].1, MenuAction::Paste);

        // …and a plain right-click has none of them.
        let bare = context_menu_items(
            &target(None, None, None, None, false),
            None,
            false,
            false,
            true,
        );
        assert_eq!(bare.len(), 1, "just Reload when there is nothing else");
    }

    #[test]
    fn a_playing_video_offers_pause_and_a_src_less_one_no_address() {
        let mut t = target(None, None, Some("https://example.com/v.mp4"), None, false);
        t.video_playing = true;
        let items = context_menu_items(&t, None, false, false, true);
        assert!(items.iter().any(|(l, _)| l == "❚❚ Pause video"));

        t.video = Some(String::new());
        t.video_playing = false;
        let items = context_menu_items(&t, None, false, false, true);
        assert!(items.iter().any(|(l, _)| l == "▶ Play video"));
        assert!(
            !items.iter().any(|(l, _)| l == "Copy video address"),
            "a src-less <video> has no address to copy"
        );
    }

    #[test]
    fn paste_waits_for_a_caret_and_a_non_empty_clipboard() {
        let editable_empty_clip = context_menu_items(
            &target(None, None, None, None, true),
            None,
            false,
            false,
            true,
        );
        assert!(
            !editable_empty_clip.iter().any(|(l, _)| l == "Paste"),
            "nothing copied yet — nothing to paste"
        );
        let editable_full_clip = context_menu_items(
            &target(None, None, None, None, true),
            None,
            false,
            false,
            false,
        );
        assert!(editable_full_clip.iter().any(|(l, _)| l == "Paste"));
        let clip_without_caret = context_menu_items(
            &target(None, None, None, None, false),
            None,
            false,
            false,
            false,
        );
        assert!(
            !clip_without_caret.iter().any(|(l, _)| l == "Paste"),
            "no caret — the menu must not promise a paste"
        );
    }

    #[test]
    fn the_devtools_table_grows_in_place_and_shrinks_on_a_tab_switch() {
        let model: Rc<VecModel<ConsoleRow>> = Rc::new(VecModel::default());
        let row = |t: &str| ConsoleRow {
            level: "log".into(),
            text: t.into(),
        };
        sync_model(&model, &[row("a"), row("b")]);
        assert_eq!(model.row_count(), 2);
        // The usual tick: the log got longer, the old rows stay where they are
        // (patching, not resetting, is what keeps the list's scroll position).
        sync_model(&model, &[row("a"), row("b"), row("c")]);
        assert_eq!(model.row_count(), 3);
        assert_eq!(model.row_data(0).unwrap().text.as_str(), "a");
        assert_eq!(model.row_data(2).unwrap().text.as_str(), "c");
        // Another tab's log: patched in place, surplus dropped.
        sync_model(&model, &[row("z")]);
        assert_eq!(model.row_count(), 1);
        assert_eq!(model.row_data(0).unwrap().text.as_str(), "z");
        sync_model(&model, &[]);
        assert_eq!(model.row_count(), 0);
    }

    #[test]
    fn devtools_pane_indices_round_trip() {
        assert_eq!(DevToolsView::default(), DevToolsView::Console);
        assert_eq!(DevToolsView::Console.index(), 0);
        assert_eq!(DevToolsView::Network.index(), 1);
        assert_eq!(DevToolsView::from_index(1), DevToolsView::Network);
        assert_eq!(DevToolsView::from_index(0), DevToolsView::Console);
        assert_eq!(
            DevToolsView::from_index(7),
            DevToolsView::Console,
            "an unknown pane falls back to Console"
        );
    }

    #[test]
    fn the_paste_chord_is_detected_whichever_way_the_backend_spells_it() {
        assert!(is_paste_chord("v", true, false, false));
        assert!(is_paste_chord("V", true, false, false));
        assert!(is_paste_chord("\u{16}", true, false, false));
        assert!(!is_paste_chord("v", false, false, false), "a bare v types");
        assert!(!is_paste_chord("v", true, true, false), "Alt+V is not paste");
        assert!(!is_paste_chord("x", true, false, false));
    }

    #[test]
    fn cursor_icons_map_onto_slint_shapes() {
        // Task: hover cursor. The mapping is exhaustive at compile time; these
        // are the spots where it is a deliberate stand-in rather than a twin.
        assert_eq!(slint_cursor(ironsurf_common::CursorIcon::Default), MouseCursor::Default);
        // The pointing hand — the feature's headline shape.
        assert_eq!(slint_cursor(ironsurf_common::CursorIcon::Pointer), MouseCursor::Pointer);
        // `cursor: none` is Slint's `None`, not the default arrow.
        assert_eq!(slint_cursor(ironsurf_common::CursorIcon::Hidden), MouseCursor::None);
        // Slint cannot draw a vertical I-beam; the horizontal one stands in.
        assert_eq!(slint_cursor(ironsurf_common::CursorIcon::VerticalText), MouseCursor::Text);
        assert_eq!(slint_cursor(ironsurf_common::CursorIcon::NwseResize), MouseCursor::NwseResize);
    }
}
