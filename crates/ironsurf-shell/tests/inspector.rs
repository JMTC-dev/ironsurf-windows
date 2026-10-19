//! DevTools **Inspector** pane, shell side, end to end on the testing backend.
//!
//! The scripted fake tab (`src/bin/fake_tab.rs`) answers `ToTab::SetInspect`
//! with a fixed eight-node document whose node ids are offset by tab id, and
//! echoes every `ToTab::SetInspect` / `SetInspectPick` / `InspectSelect` /
//! `InspectPickAt` on its status line — which is what makes the shell's half of
//! the contract observable here:
//!
//! * the document is asked for **once**, when this tab's Inspector pane opens,
//!   and never again on a redraw (25 pumps and a re-pick of the same pane change
//!   nothing) — that is what stops a panel from re-walking the DOM forever;
//! * folding and selecting are the shell's own business: the tree is a view
//!   over the snapshot the tab already sent, so neither row intent sends the
//!   tab back to its document;
//! * Ctrl-Shift-C arms ⌖ for the active tab (opening the panel and its Inspector
//!   pane), keeps the panel open when it was already open, is a no-op with no
//!   tabs, and is dropped by Escape, by `devtools-close`, and by leaving the
//!   Inspector pane — so no page is left swallowing pointers for a picker
//!   nobody can see;
//! * the context menu offers “⌖ Inspect Element” on any right-click and asks
//!   `ToTab::InspectPickAt` with the point the menu opened at;
//! * the tree belongs to its tab: switching tabs switches documents (the window
//!   shows nothing rather than the last tab's tree repainted), a background tab
//!   keeps its own tree with its own folds, and closing a tab drops its tree
//!   with its process.
//!
//! Headless (`i-slint-backend-testing`): no X, no engine. The real-engine leg
//! is `tests/inspector_e2e.rs`.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use ironsurf_common::TabId;
use ironsurf_shell::app::{App, DevToolsView};
use ironsurf_shell::AppWindow;
use slint::Model;

fn wait_for(what: &str, cond: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if cond() {
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Every test in this binary wants its own `IRONSURF_DATA_DIR`, which is a
/// process-global, so the tests here run one at a time (the same switch is
/// shared in `tests/clipboard_shortcuts.rs`).
static INSPECTOR_ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn lock_env() -> std::sync::MutexGuard<'static, ()> {
    INSPECTOR_ENV
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// A window with the scripted fake tab on the testing backend. Held for as long
/// as the app lives, since the fake tab's data dir is this process's env.
fn harness() -> (tempfile::TempDir, AppWindow, App) {
    i_slint_backend_testing::init_no_event_loop();
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("IRONSURF_DATA_DIR", dir.path());
    let ui = AppWindow::new().expect("window on testing backend");
    let fake = PathBuf::from(env!("CARGO_BIN_EXE_fake_tab"));
    let app = App::new(&ui, fake, Vec::new()).expect("app core");
    app.start(None);
    (dir, ui, app)
}

/// Which rows the pane highlights.
fn selected_rows(ui: &AppWindow) -> Vec<i32> {
    let model = ui.get_inspector_rows();
    (0..model.row_count())
        .filter(|i| model.row_data(*i).unwrap().selected)
        .map(|i| model.row_data(i).unwrap().node)
        .collect()
}

/// The tree as the pane renders it, minus the highlight: `(node, depth, twisty)`.
fn rows(ui: &AppWindow) -> Vec<(i32, i32, String)> {
    let model = ui.get_inspector_rows();
    (0..model.row_count())
        .map(|i| {
            let r = model.row_data(i).unwrap();
            (r.node, r.depth, r.twisty.to_string())
        })
        .collect()
}

/// The breadcrumb exactly as the pane renders it.
fn crumb(ui: &AppWindow) -> Vec<String> {
    let model = ui.get_inspector_crumb();
    (0..model.row_count())
        .map(|i| {
            let c = model.row_data(i).unwrap();
            format!("{}{}", c.tag, c.ident)
        })
        .collect()
}

/// The markup text of row `n`, tokens joined — the pane's own colouring.
fn row_text(ui: &AppWindow, n: usize) -> String {
    let model = ui.get_inspector_rows();
    let r = model.row_data(n).unwrap();
    (0..r.toks.row_count())
        .map(|i| r.toks.row_data(i).unwrap().text.to_string())
        .collect::<String>()
}

/// F12 / Ctrl-Shift-C / Escape, exactly as Slint hands them to the page.
fn press_f12(ui: &AppWindow) {
    ui.invoke_page_key(true, "\u{F70F}".into(), false, false, false, false, false);
}
fn press_ctrl_shift_c(ui: &AppWindow) {
    ui.invoke_page_key(true, "C".into(), true, true, false, false, false);
}
fn press_escape(ui: &AppWindow) {
    ui.invoke_page_key(true, "Escape".into(), false, false, false, false, false);
}

/// Right-button press+release at (x, y) — a right-click into the page.
fn right_click(ui: &AppWindow, x: f32, y: f32) {
    ui.invoke_page_pointer(x, y, 1, 1, false, false, false, false);
    ui.invoke_page_pointer(x, y, 2, 1, false, false, false, false);
}

/// The Inspector pane of the active tab: F12, then the third pane.
fn open_inspector(ui: &AppWindow) {
    if !ui.get_devtools_open() {
        press_f12(ui);
    }
    ui.invoke_devtools_select_view(2);
}

#[test]
fn the_pane_asks_its_tab_once_and_keeps_its_own_view_state() {
    let _guard = lock_env();
    let (_dir, ui, app) = harness();
    let core = app.core();
    let pump = || core.drain();
    let snap = || core.tab_snapshot();
    wait_for("tab ready", || {
        pump();
        !snap().is_empty() && snap()[0].ready
    });

    // No tree, and — the pane has never been opened — no request either.
    assert_eq!(core.inspector_probe().nodes, 0);
    assert!(!core.inspector_probe().stream_wanted);
    assert_eq!(rows(&ui).len(), 0);
    assert!(ui.get_inspector_hint().contains("no document tree"));

    // F12 alone opens the panel on the Console pane, which never asks for a
    // document: only the Inspector pane does.
    press_f12(&ui);
    pump();
    assert!(ui.get_devtools_open());
    assert_eq!(ui.get_devtools_view(), DevToolsView::Console.index());
    assert!(!core.inspector_probe().pane_open);
    assert!(!core.inspector_probe().stream_wanted);

    // The Inspector pane: the tab is asked, answers, and the window paints it.
    ui.invoke_devtools_select_view(2);
    wait_for("the tab's document arrives", || {
        pump();
        core.inspector_probe().nodes == 8
    });
    let probe = core.inspector_probe();
    assert!(probe.pane_open && probe.tree_for_active);
    // The instruction stands even though the tab answered: an open Inspector
    // pane keeps its tab walking the document, and the next generation (a
    // reload) is meant to arrive on its own. What ends is the *waiting*.
    assert!(probe.stream_wanted);
    assert!(!ui.get_inspector_hint().contains("asking"));
    assert_eq!((probe.generation, probe.elements, probe.collapsed), (Some(1), 5, 0));
    assert_eq!(ui.get_inspector_count(), 5);
    assert!(!ui.get_inspector_empty());
    assert!(!ui.get_inspector_truncated());
    assert!(
        ui.get_inspector_summary().contains("5 element(s)"),
        "the header names the document: {:?}",
        ui.get_inspector_summary()
    );

    // The tree is indented, twistied on its branches, and coloured per token.
    let base = rows(&ui)[0].0;
    assert_eq!(
        rows(&ui),
        vec![
            (base, 0, "\u{2212}".into()), // #document
            (base + 1, 1, "\u{2212}".into()), // html
            (base + 2, 2, "\u{2212}".into()), // head[lang]
            (base + 3, 3, "\u{2212}".into()), // title
            (base + 4, 4, String::new()), // "Fake — …"
            (base + 5, 2, "\u{2212}".into()), // body#main
            (base + 6, 3, "\u{2212}".into()), // p.note
            (base + 7, 4, String::new()), // "fake body text"
        ],
        "pre-order, one row per node, leaves not clickable"
    );
    // A fresh tree selects its root, so the pane is never all grey.
    assert_eq!(core.inspector_probe().selected, Some(base as u64));
    assert_eq!(selected_rows(&ui), vec![base]);
    let body_row = row_text(&ui, 5);
    assert!(
        body_row.contains("body") && body_row.contains("id") && body_row.contains("main"),
        "the row re-renders the markup the tab's `tag`/`attrs` describe: {body_row:?}"
    );

    // …and one request per pane-open, not one per redraw: 25 pumps, a re-pick
    // of the same pane, and a Clear that this pane refuses change nothing.
    for _ in 0..25 {
        pump();
        ui.invoke_devtools_select_view(2);
        ui.invoke_devtools_clear();
    }
    pump();
    assert_eq!(
        snap()[0].status.as_deref(),
        Some("inspect-stream:on:1"),
        "the tab was asked for its document exactly once"
    );
    assert_eq!(core.inspector_probe().nodes, 8, "Clear must not eat the tree");

    // Fold: the shell's own view over the same snapshot — the tab is not asked.
    let body = base + 5;
    ui.invoke_devtools_toggle_fold(body);
    pump();
    let folded = rows(&ui);
    assert_eq!(folded.len(), 6, "body's two-row subtree is folded away");
    assert!(!folded.iter().any(|(n, ..)| *n == base + 6 || *n == base + 7));
    assert_eq!(
        folded.iter().find(|(n, ..)| *n == body).map(|(_, _, t)| t.as_str()),
        Some("+"),
        "a folded branch shows +"
    );
    assert_eq!(core.inspector_probe().collapsed, 1);
    pump();
    assert_eq!(
        snap()[0].status.as_deref(),
        Some("inspect-stream:on:1"),
        "folding is a view over the snapshot the tab already sent"
    );
    ui.invoke_devtools_toggle_fold(body);
    pump();
    assert_eq!(rows(&ui).len(), 8);
    assert_eq!(core.inspector_probe().collapsed, 0);

    // Select: the pane highlights, the breadcrumb follows, and the tab is told
    // to paint its box — exactly once, and never for a node it does not have.
    let p = base + 6;
    ui.invoke_devtools_select_node(p);
    wait_for("the tab is told what is selected", || {
        pump();
        snap()[0].status.as_deref() == Some(format!("select:{p}").as_str())
    });
    assert_eq!(core.inspector_probe().selected, Some(p as u64));
    assert_eq!(selected_rows(&ui), vec![p], "one row highlighted");
    assert_eq!(crumb(&ui), ["#document", "html", "body#main", "p.note"]);

    // An id from a snapshot the tab has replaced: the pane keeps the selection
    // it can justify and stays silent — it must not ask the tab to highlight a
    // node the current document may not contain.
    let stale = base + 99;
    ui.invoke_devtools_select_node(stale);
    pump();
    assert_eq!(core.inspector_probe().selected, Some(p as u64));
    assert_eq!(
        snap()[0].status.as_deref(),
        Some(format!("select:{p}").as_str()),
        "no `InspectSelect` for an unknown node id"
    );

    // Selecting a node inside a folded branch opens the branch: the pane has to
    // show what it selected.
    let head = base + 2;
    ui.invoke_devtools_toggle_fold(head);
    pump();
    assert_eq!(rows(&ui).len(), 6, "head's subtree (title + its text) folds away");
    ui.invoke_devtools_select_node(base + 4);
    wait_for("the tab is told about the node inside the fold", || {
        pump();
        snap()[0].status.as_deref() == Some(format!("select:{}", base + 4).as_str())
    });
    assert_eq!(core.inspector_probe().collapsed, 0, "the fold above the selection opens");
    assert_eq!(rows(&ui).len(), 8);
    assert_eq!(crumb(&ui), ["#document", "html", "head", "title", "#text"]);

    // …but the tree, folds included, belongs to the Inspector pane rather than
    // to the window: moving to Console stops the walk and drops the snapshot
    // (a tab holding a tree it cannot show is only memory), so coming back asks
    // for the document again — and starts from unfolded.
    ui.invoke_devtools_toggle_fold(head);
    pump();
    assert_eq!(core.inspector_probe().collapsed, 1);
    ui.invoke_devtools_select_view(0);
    wait_for("Console stops the walk", || {
        pump();
        snap()[0].status.as_deref() == Some("inspect-stream:off:2")
    });
    assert_eq!(core.inspector_probe().nodes, 0);
    assert_eq!(core.inspector_probe().collapsed, 0, "the folds left with the tree");
    assert_eq!(rows(&ui).len(), 0);
    ui.invoke_devtools_select_view(2);
    wait_for("Inspector asks its tab again", || {
        pump();
        rows(&ui).len() == 8
    });
    assert_eq!(core.inspector_probe().collapsed, 0);
    assert_eq!(
        snap()[0].status.as_deref(),
        Some("inspect-stream:on:3"),
        "one request per pane-open, never one per redraw"
    );

    // Leaving the Inspector pane stops the walk: the tree is this document's,
    // and a pane that is not showing it keeps neither.
    // …and leaving the Inspector pane for good (here: Network) ends the walk,
    // drops the tree and forgets the folds — off for the fourth time, one per
    // pane-open in this test, which is exactly the budget.
    ui.invoke_devtools_select_view(1);
    wait_for("the tab stops streaming", || {
        pump();
        snap()[0].status.as_deref() == Some("inspect-stream:off:4")
    });
    let probe = core.inspector_probe();
    assert!(!probe.pane_open && !probe.stream_wanted);
    assert_eq!((probe.nodes, probe.collapsed, probe.selected), (0, 0, None));
    assert_eq!(rows(&ui).len(), 0, "no tree left on screen for a pane that is gone");
}

#[test]
fn the_picker_arms_for_its_tab_and_is_put_away_every_way_back() {
    let _guard = lock_env();
    let (_dir, ui, app) = harness();
    let core = app.core();
    let pump = || core.drain();
    let snap = || core.tab_snapshot();
    wait_for("tab ready", || {
        pump();
        !snap().is_empty() && snap()[0].ready
    });

    // Ctrl-Shift-C with the panel shut: panel + Inspector pane + the mode, in
    // one chord, and the tab told to swallow pointers.
    press_ctrl_shift_c(&ui);
    wait_for("the tab arms its picker", || {
        pump();
        snap()[0].status.as_deref() == Some("pick:on")
    });
    let probe = core.inspector_probe();
    assert!(probe.pane_open, "the mode opens the pane it is meant to land in");
    assert!(probe.pick);
    assert!(ui.get_devtools_open() && ui.get_devtools_view() == DevToolsView::Inspector.index());
    assert!(ui.get_inspector_pick(), "\u{2316} shows as armed in the header");

    // F12 while armed closes the panel — and the mode leaves with the pane it
    // was armed in. A page that went on eating clicks for an invisible picker
    // would be a bug, not a mode.
    press_f12(&ui);
    wait_for("the panel closed and stopped the walk with it", || {
        pump();
        !ui.get_devtools_open() && snap()[0].status.as_deref() == Some("inspect-stream:off:2")
    });
    let probe = core.inspector_probe();
    assert!(!probe.pick && !probe.pane_open && !probe.stream_wanted);
    assert!(!ui.get_inspector_pick(), "the header goes dark too");

    // The header ✕ is the same exit.
    press_ctrl_shift_c(&ui);
    wait_for("armed again", || {
        pump();
        core.inspector_probe().pick
    });
    ui.invoke_devtools_close();
    wait_for("✕ disarms with the panel", || {
        pump();
        !ui.get_devtools_open() && !core.inspector_probe().pick
    });

    // Escape puts the mode away before focus goes back to the page — and, unlike
    // the two exits above, leaves the panel and the pane where they were.
    press_ctrl_shift_c(&ui);
    wait_for("armed for the Escape leg", || {
        pump();
        core.inspector_probe().pick && snap()[0].status.as_deref() == Some("pick:on")
    });
    // The shell disarms the moment it sees the key, so the pane goes dark at
    // once; the tab catches up on the next echo. Waiting for both is what makes
    // the line below readable rather than a race.
    press_escape(&ui);
    wait_for("Escape disarms the picker, here and at the tab", || {
        pump();
        !core.inspector_probe().pick && snap()[0].status.as_deref() == Some("pick:off")
    });
    assert!(
        ui.get_devtools_open() && ui.get_devtools_view() == DevToolsView::Inspector.index(),
        "Escape leaves the panel up"
    );

    // The same chord again disarms it too (a mode you can only enter is a trap).
    press_ctrl_shift_c(&ui);
    wait_for("armed a third time", || {
        pump();
        core.inspector_probe().pick && snap()[0].status.as_deref() == Some("pick:on")
    });
    press_ctrl_shift_c(&ui);
    wait_for("the chord disarms its own mode", || {
        pump();
        !core.inspector_probe().pick && snap()[0].status.as_deref() == Some("pick:off")
    });
    assert!(ui.get_devtools_open(), "disarming does not hide the panel");

    // Leaving the Inspector pane ends the mode with the pane, without closing
    // the panel or touching the other two panes.
    press_ctrl_shift_c(&ui);
    wait_for("armed for the pane-switch leg", || {
        pump();
        core.inspector_probe().pick
    });
    ui.invoke_devtools_select_view(0);
    wait_for("leaving the pane ends the mode with it", || {
        pump();
        // The tab hears about both at once (pick off, then stream off), so the
        // line ends on the walk stopping; the mode itself is shell state.
        !core.inspector_probe().pick
            && snap()[0]
                .status
                .as_deref()
                .is_some_and(|s| s.starts_with("inspect-stream:off"))
    });
    assert!(ui.get_devtools_open() && ui.get_devtools_view() == DevToolsView::Console.index());

    // The context menu offers Inspect Element on any right-click and asks about
    // the point the menu opened at — here a point that is not the origin, so a
    // hard-wired (0,0) would fail.
    right_click(&ui, 233.0, 147.0);
    wait_for("the menu opens", || {
        pump();
        ui.get_menu_open()
    });
    let items: Vec<String> = (0..ui.get_menu_items().row_count())
        .map(|i| ui.get_menu_items().row_data(i).unwrap().label.to_string())
        .collect();
    let at = items
        .iter()
        .position(|l| l == "\u{2316} Inspect Element")
        .expect("the context menu offers the tree of the page the click landed in");
    assert_eq!((ui.get_menu_x(), ui.get_menu_y()), (233.0, 147.0));
    ui.invoke_menu_pick(at as i32);
    wait_for("the tab is asked what is under the click point", || {
        pump();
        snap()[0].status.as_deref() == Some("pick-at:233,147")
    });
    // The tab answered with the node under the point: the pane selects it (and
    // opens the Inspector pane, which this tab left on Console), and the mode
    // that could have produced this answer is over — one pick per arming.
    let hit = core
        .inspector_probe()
        .target
        .expect("the pane took the tab's answer")
        .0 as u64
        * 1000
        + 6;
    wait_for("the picked node is selected", || {
        pump();
        core.inspector_probe().selected == Some(hit)
    });
    wait_for("one pick per arming", || {
        pump();
        !core.inspector_probe().pick
    });
    assert!(core.inspector_probe().pane_open);
    assert_eq!(selected_rows(&ui), vec![hit as i32]);
}

#[test]
fn each_tab_holds_its_own_document_and_losing_a_tab_loses_its_tree() {
    let _guard = lock_env();
    let (_dir, ui, app) = harness();
    let core = app.core();
    let pump = || core.drain();
    let snap = || core.tab_snapshot();
    wait_for("first tab ready", || {
        pump();
        !snap().is_empty() && snap()[0].ready
    });

    open_inspector(&ui);
    wait_for("tab 1 has a tree", || {
        pump();
        core.inspector_probe().nodes == 8
    });
    let first = core.inspector_probe().target.expect("tab 1 owns its tree");
    let base = rows(&ui)[0].0;
    ui.invoke_devtools_toggle_fold(base + 5);
    pump();
    assert_eq!(rows(&ui).len(), 6);

    // A second tab: the pane is per tab, so the new tab shows no panel, no tree,
    // and above all not the first tab's tree under a new title.
    ui.invoke_new_tab();
    wait_for("second tab ready", || {
        pump();
        snap().len() == 2 && snap()[1].ready
    });
    let second = core.inspector_probe().target;
    assert_ne!(second, Some(first), "the window is not still bound to tab 1");
    assert!(!ui.get_devtools_open(), "DevTools is per tab");
    assert_eq!(rows(&ui).len(), 0, "no tree of another tab is repainted here");
    assert_eq!(core.inspector_probe().nodes, 0);
    assert!(ui.get_inspector_hint().contains("no document tree"));

    // Tab 1 keeps what the shell owned about its own document — the tree and the
    // fold — while it is in the background.
    ui.invoke_activate_tab(0);
    wait_for("tab 1's tree and fold are back", || {
        pump();
        core.inspector_probe().nodes == 8 && core.inspector_probe().collapsed == 1
    });
    assert_eq!(core.inspector_probe().target, Some(first));
    assert_eq!(rows(&ui).len(), 6, "the fold came back with the tab");
    assert_eq!(
        snap()[0].status.as_deref(),
        Some("inspect-stream:on:1"),
        "and it did not have to walk its document again"
    );

    // Tab 2 opens its own Inspector: its own document, its own ids, its own
    // stream request.
    ui.invoke_activate_tab(1);
    open_inspector(&ui);
    wait_for("tab 2 streams its own document", || {
        pump();
        core.inspector_probe().nodes == 8
    });
    assert_eq!(core.inspector_probe().target, Some(TabId(first.0 + 1)));
    assert_eq!(rows(&ui)[0].0, base + 1000, "a different tab's tree, not a repaint");
    assert_eq!(core.inspector_probe().collapsed, 0, "…with its own folds (none)");
    assert_eq!(snap()[1].status.as_deref(), Some("inspect-stream:on:1"));

    // Closing the tab drops the tree with the process — nothing is left pointing
    // at a document whose owner is gone.
    ui.invoke_close_tab(1);
    wait_for("tab 2 is gone", || {
        pump();
        snap().len() == 1
    });
    ui.invoke_activate_tab(0);
    pump();
    assert_eq!(core.inspector_probe().target, Some(first));
    assert_eq!(core.inspector_probe().nodes, 8);
}

#[test]
fn every_inspector_intent_survives_having_no_tab_at_all() {
    let _guard = lock_env();
    // The panel is a page tool: with no tabs there is no page to ask. None of
    // these may panic, and none may invent a document.
    let (_dir, ui, app) = harness();
    let core = app.core();
    let pump = || core.drain();
    wait_for("start-up tab exists", || {
        pump();
        !core.tab_snapshot().is_empty()
    });
    ui.invoke_close_tab(0);
    wait_for("no tabs", || {
        pump();
        core.tab_snapshot().is_empty()
    });
    let default = core.inspector_probe();
    assert_eq!(default, Default::default(), "no tab, no tree, no mode");
    press_f12(&ui);
    press_ctrl_shift_c(&ui);
    press_escape(&ui);
    ui.invoke_devtools_select_view(2);
    ui.invoke_devtools_toggle_pick();
    ui.invoke_devtools_select_node(1);
    ui.invoke_devtools_toggle_fold(1);
    ui.invoke_devtools_clear();
    right_click(&ui, 10.0, 10.0);
    pump();
    assert_eq!(core.inspector_probe(), Default::default());
    assert!(!ui.get_devtools_open());
}
