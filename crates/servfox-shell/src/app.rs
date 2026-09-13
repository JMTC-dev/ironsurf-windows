//! Runtime glue: Slint `AppWindow` ⇄ tab processes ⇄ container registry.
//!
//! All state lives inside `Rc<Core>` so the Slint callbacks, the message
//! drain timer and the spinner timer can share it mutably (RefCells +
//! methods on `Rc<Core>`). `Frame` messages are coalesced to the newest per
//! drain tick; the model is pushed once per tick when anything changed.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::mpsc::{channel, Receiver, Sender};

use servfox_common::{default_url, ContainerId, FromTab, TabId, ToTab};
use servfox_storage::default_data_root;
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

/// Fixed palette — **keep in sync** with `palette-color()` in ui/servfox.slint.
pub const PALETTE: [&str; 7] = [
    "#4a90d9", "#e06666", "#6aa84f", "#f6b26b", "#8e7cc3", "#6d4c41", "#0097a7",
];

const SPINNER_FRAMES: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

struct Proc {
    tab: TabProc,
    state: TabState,
}

/// Shared runtime core (one per window; kept in an `Rc`).
pub struct Core {
    store: RefCell<servfox_storage::Store>,
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
        let store = servfox_storage::Store::open(default_data_root())?;
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
        self.core.save_settings();
        let procs = std::mem::take(&mut *self.core.procs.borrow_mut());
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
        self.shell.borrow().as_ref().and_then(|w| w.upgrade().ok())
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
        let Some(id) = self.active_tab_id() else { return };
        let procs = self.procs.borrow();
        if let Some(p) = procs.iter().find(|p| p.tab.id == id) {
            let _ = p.tab.send(msg);
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
        match TabProc::spawn(&tab_bin, id, container, &profile_dir, url, &extra_refs, &self.sink) {
            Ok(tab) => {
                *self.spawn_error.borrow_mut() = None;
                let (w, h) = self.page_size();
                {
                    let mut procs = self.procs.borrow_mut();
                    procs.push(Proc { tab, state: TabState::default() });
                    self.active.set(procs.len() - 1);
                }
                if w > 0.0 && h > 0.0 {
                    self.send_active(ToTab::Resize { width: w as u32, height: h as u32, scale: 1.0 });
                }
            }
            Err(err) => {
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
                return;
            }
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
        if let Some(url) = classify_input(text) {
            self.send_active(ToTab::Navigate { url: url.clone() });
            if let Some(ui) = self.ui() {
                ui.set_url_text(url.into());
                // After navigating from the URL bar, focus returns to the page.
                ui.set_focus_page_token(ui.get_focus_page_token() + 1);
            }
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

    fn page_pointer(&self, x: f32, y: f32, kind: i32, button: i32) {
        use servfox_common::{MouseButton, PointerKind};
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
        self.send_active(ToTab::Pointer(servfox_common::PointerEvent { x, y, kind, button }));
    }

    fn page_wheel(&self, x: f32, y: f32, delta_x: f32, delta_y: f32) {
        self.send_active(ToTab::Scroll(servfox_common::ScrollEvent { x, y, delta_x, delta_y }));
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
                std::mem::replace(&mut procs[at], Proc { tab, state: TabState::default() })
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
        if self.store.borrow_mut().delete_container(cid).is_err() {
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
        let msgs: Vec<PumpedMessage> = self.rx.borrow_mut().try_iter().collect();
        if msgs.is_empty() {
            self.check_resize();
            return;
        }
        let active = self.active_tab_id();
        let mut dirty = false;
        let mut newest_frame: Option<Image> = None;

        for (tid, msg) in msgs {
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
                    let cs = servfox_storage::ContainerStore::open(&store, container);
                    let _ = cs.append_history(&url, Some(&title));
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
        ui.set_tabs(&ModelRc::from(Rc::new(VecModel::from(tabs))));

        let conts: Vec<ContainerInfo> = store
            .containers()
            .iter()
            .map(|c| ContainerInfo {
                name: c.name.clone().into(),
                color: color_hex(&c.color),
                isDefault: c.is_default,
            })
            .collect();
        ui.set_containers(&ModelRc::from(Rc::new(VecModel::from(conts))));

        if let Some(p) = procs.get(active) {
            ui.set_can_back(p.state.can_back);
            ui.set_can_fwd(p.state.can_forward);
            ui.set_url_text(p.state.url.clone().unwrap_or_default().into());
            let status = p
                .state
                .error
                .clone()
                .or_else(|| p.state.status.clone())
                .or_else(|| self.spawn_error.borrow().clone())
                .unwrap_or_default();
            ui.set_status_text(status.into());
            let (glyph, color) = p.state.security().render();
            ui.set_sec_glyph(glyph.into());
            ui.set_sec_color(color.into());
            if let Some(c) = store.container(p.tab.container) {
                ui.set_container_label(c.name.clone().into());
            }
        } else {
            let status = self.spawn_error.borrow().clone().unwrap_or_default();
            ui.set_status_text(status.into());
        }
        ui.set_active_index(active as i32);
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
        bind0!(on_go_forward, |c| c.send_active(ToTab::Forward));
        bind0!(on_reload, |c| c.send_active(ToTab::Reload));
        bind0!(on_new_tab, |c| {
            let cid = c.new_tab_container();
            c.open_tab(cid, Some(&default_url()));
        });
        bind0!(on_open_manager, |c| c.manager_open(true));
        bind0!(on_close_manager, |c| c.manager_open(false));
        bind0!(on_mgr_add, |c| c.mgr_add());

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
