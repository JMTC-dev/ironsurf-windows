//! Runtime glue: Slint `AppWindow` ⇄ tab processes ⇄ container registry.
//!
//! All state lives inside `Rc<Core>` so the Slint callbacks, the message
//! drain timer and the spinner timer can share it mutably (RefCells +
//! methods on `Rc<Core>`). `Frame` messages are coalesced to the newest per
//! drain tick; the model is pushed once per tick when anything changed.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};
use std::sync::mpsc::{channel, Receiver, Sender};

use ironsurf_common::{debug, error, info, trace, warn};
use ironsurf_common::{default_url, ContainerId, FromTab, TabId, ToTab};
use ironsurf_storage::default_data_root;
use slint::{
    Color, ComponentHandle, Image, ModelRc, Rgba8Pixel, SharedPixelBuffer, SharedString, Timer,
    TimerMode, VecModel, Weak,
};

use crate::classify::classify_input;
use crate::keymap::{self, Shortcut};
use crate::state::TabState;
use crate::tabs::{PumpedMessage, TabProc};
use crate::uisettings::UiSettings;
use crate::{AppWindow, ContainerInfo, TabInfo};

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
        let (profile_dir, tab_bin, extra) = {
            let store = self.store.borrow();
            (store.profile_dir(container), self.tab_bin.clone(), self.extra_tab_args.clone())
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
                    procs.push(Proc { tab, state: TabState::default(), zoom: 1.0, crashed: false });
                    self.active.set(procs.len() - 1);
                }
                if w > 0.0 && h > 0.0 {
                    self.send_active(ToTab::Resize { width: w as u32, height: h as u32, scale: 1.0 });
                }
            }
            Err(err) => {
                error!("spawn {} failed: {err}", tab_bin.display());
                *self.spawn_error.borrow_mut() =
                    Some(format!("spawn {} failed: {err}", tab_bin.display()));
            }
        }
        self.set_visible_except();
        self.sync_ui();
    }

    fn close_tab(&self, idx: usize) {
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
            Some(Shortcut::PageFocus) => {
                if let Some(ui) = self.ui() {
                    let t = ui.get_focus_page_token() + 1;
                    ui.set_focus_page_token(t);
                }
                return;
            }
            None => {}
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
        let replacement = TabProc::spawn(
            &self.tab_bin,
            id,
            cid,
            &profile_dir,
            url.as_deref(),
            &self.extra_tab_args.iter().map(String::as_str).collect::<Vec<&str>>(),
            &self.sink,
        );
        if let Ok(tab) = replacement {
            let old = {
                let mut procs = self.procs.borrow_mut();
                let zoom = procs[at].zoom;
                std::mem::replace(
                    &mut procs[at],
                    Proc { tab, state: TabState::default(), zoom, crashed: false },
                )
            };
            if w > 0.0 && h > 0.0 {
                let _ = old.tab.send(ToTab::Resize { width: w as u32, height: h as u32, scale: 1.0 });
            }
            let mut old = old;
            old.tab.destroy();
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
                FromTab::ConsoleLog { level, message } => {
                    info!("tab {tid} console[{level}]: {message}")
                }
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
            info!("reaped {} closed tab(s); {} remain", before - after, after);
            dirty = true;
        }

        if dirty {
            self.sync_ui();
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
        {
            let mut procs = self.procs.borrow_mut();
            for p in procs.iter_mut() {
                if p.crashed || p.state.closed {
                    continue;
                }
                if let Some(status) = p.tab.try_wait_status() {
                    note_tab_crash(p, format_args!("the process exited ({status}) without closing cleanly"));
                    crashed = true;
                }
            }
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
    }
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
}
