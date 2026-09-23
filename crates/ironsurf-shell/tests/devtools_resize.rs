//! The F12 panel is resizable by dragging its top edge, and its height belongs
//! to the tab that was dragged.
//!
//! The panel is docked to the bottom of the page area, so its top edge is the
//! thing to grab: pull it up and the panel grows, push it down and it shrinks.
//! The strip that takes the drag **straddles** that edge — half over the page
//! above, half over the panel's own separator line — because the panel has
//! nothing but a 1 px line at its top and the header buttons start 4 px below
//! it. All of that is headless-visible through `ElementHandle`, and so is the
//! rule the drag is built on: the pointer's travel is measured from where the
//! press landed, so a panel held at its floor or ceiling cannot drift.
//!
//! Headless (`i-slint-backend-testing`): no X, no engine.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use ironsurf_shell::app::{App, DEVTOOLS_MIN_HEIGHT};
use ironsurf_shell::AppWindow;
use slint::ComponentHandle;

/// Every test here wants its own `IRONSURF_DATA_DIR`, which is process-global.
static RESIZE_ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

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

fn press_f12(ui: &AppWindow) {
    ui.invoke_page_key(true, "\u{F70F}".into(), false, false, false, false, false);
}

/// Alt-`1`: back to the first tab.
fn press_alt_1(ui: &AppWindow) {
    ui.invoke_page_key(true, "1".into(), false, false, true, false, false);
}

/// One moment of a drag on the panel's top edge: 0 press, 1 move, 2 release,
/// `travel` positive **up** (see the `devtools-edge` callback).
fn edge(ui: &AppWindow, kind: i32, travel: f32) {
    ui.invoke_devtools_edge(kind, travel);
}

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

/// `(x, y, width, height)` of every realized element with `id`.
fn boxes(ui: &AppWindow, id: &str) -> Vec<(f32, f32, f32, f32)> {
    i_slint_backend_testing::ElementQuery::from_root(ui)
        .match_id(format!("AppWindow::{id}"))
        .find_all()
        .iter()
        .map(|e| {
            let p = e.absolute_position();
            let s = e.size();
            (p.x, p.y, s.width, s.height)
        })
        .collect()
}

/// The height a panel nobody dragged paints at, for the window as it stands —
/// the same rule Rust applies (`default_devtools_height`).
fn default_height(ui: &AppWindow) -> f32 {
    240f32.min(ui.get_page_height() * 0.65)
}

fn near(a: f32, b: f32) -> bool {
    (a - b).abs() < 0.6
}

/// Open the panel on the first tab and wait for it to be on screen.
fn open(ui: &AppWindow, pump: impl Fn()) {
    press_f12(ui);
    wait_for("panel open", || {
        pump();
        ui.get_devtools_open()
    });
}

#[test]
fn the_strip_that_takes_the_drag_straddles_the_panels_top_edge() {
    let _guard = RESIZE_ENV.lock().unwrap_or_else(|p| p.into_inner());
    let (_dir, ui, app) = harness();
    ui.show().expect("show on the testing backend");
    let core = app.core();
    let pump = || core.drain();
    wait_for("tab ready", || {
        pump();
        !core.tab_snapshot().is_empty() && core.tab_snapshot()[0].ready
    });

    // Before F12 there is nothing to grab, and the height Rust holds is the
    // panel's own default: 240 px, or the page's 65 % where that is less.
    assert!(
        (ui.get_devtools_height() - default_height(&ui)).abs() < 0.6,
        "an untouched panel starts at its default: got {} want {}",
        ui.get_devtools_height(),
        default_height(&ui)
    );
    open(&ui, pump);
    ui.window().request_redraw();

    let panel = boxes(&ui, "dt-panel");
    let strip = boxes(&ui, "dt-edge");
    assert!(panel.len() == 1 && strip.len() == 1, "panel and its edge must be laid out once");
    let (_, py, pw, ph) = panel[0];
    let (_, sy, sw, sh) = strip[0];
    assert!(near(ph, ui.get_devtools_height()), "the panel paints the height it is given: {ph}");
    // Straddle: the strip is centred on the panel's top edge…
    assert!(near(sy, py - 4.0), "strip y={sy} should sit 4px above the panel top {py}");
    assert!(near(sh, 8.0), "strip is 8px tall, got {sh}");
    assert!(near(sw, pw), "the whole width is grabbable: {sw} vs {pw}");
    // …so it covers the separator line and stops where the header buttons begin
    // (they are 24px tall, centred in the 32px header row, i.e. from y+4).
    assert!(sy < py && sy + sh > py, "the strip must lie over the edge itself");
    assert!(
        sy + sh <= py + 4.0 + 0.01,
        "the strip may not reach the header buttons: strip ends {}, buttons start {}",
        sy + sh,
        py + 4.0
    );

}

#[test]
fn dragging_the_strip_up_makes_the_panel_taller_and_down_shorter() {
    let _guard = RESIZE_ENV.lock().unwrap_or_else(|p| p.into_inner());
    let (_dir, ui, app) = harness();
    ui.show().expect("show on the testing backend");
    let core = app.core();
    let pump = || core.drain();
    wait_for("tab ready", || {
        pump();
        !core.tab_snapshot().is_empty() && core.tab_snapshot()[0].ready
    });
    open(&ui, pump);
    let start = ui.get_devtools_height();
    assert!(core.devtools_heights()[0].is_none(), "nobody has dragged yet");

    // Up 60 px: taller by exactly that, and the panel's own geometry follows.
    edge(&ui, 0, 0.0);
    edge(&ui, 1, 60.0);
    let taller = ui.get_devtools_height();
    assert!(near(taller, start + 60.0), "up 60px: got {taller}, started {start}");
    ui.window().request_redraw();
    let painted = boxes(&ui, "dt-panel")
        .first()
        .unwrap_or_else(|| panic!("the panel must be laid out while it is dragged"))
        .3;
    assert!(near(painted, taller), "the panel paints the dragged height: {painted} vs {taller}");

    // Down 30 px from the **press**, which is back below where the panel began:
    // the travel is measured off the grab, never off the last height.
    edge(&ui, 1, -30.0);
    assert!(near(ui.get_devtools_height(), start - 30.0), "the drag follows the pointer back down");

    // Releasing changes nothing and leaves the height on the tab record.
    edge(&ui, 2, 0.0);
    let kept = core.devtools_heights()[0].expect("the dragged height is remembered");
    assert!(near(kept, start - 30.0), "released at {kept}");

    // Overshoot both ways: the panel stops at its bounds instead of eating the
    // page, and coming back off a bound lands where the pointer is — the press,
    // not the clamped last height, is what travel is measured from.
    let page = ui.get_page_height();
    edge(&ui, 0, 0.0);
    edge(&ui, 1, 100_000.0);
    assert!(
        ui.get_devtools_height() <= page * 0.9 + 0.01,
        "a panel may not take more than a tenth of the page under it: {} of {page}",
        ui.get_devtools_height()
    );
    edge(&ui, 1, -100.0);
    assert!(
        near(ui.get_devtools_height(), kept - 100.0),
        "a panel pinned at the ceiling must not drift: got {}, want {}",
        ui.get_devtools_height(),
        kept - 100.0
    );
    edge(&ui, 1, -100_000.0);
    assert!(
        ui.get_devtools_height() >= DEVTOOLS_MIN_HEIGHT - 0.01,
        "the floor keeps the panel readable: {}",
        ui.get_devtools_height()
    );
    edge(&ui, 2, 0.0);
    assert!(near(core.devtools_heights()[0].unwrap(), DEVTOOLS_MIN_HEIGHT));
}

#[test]
fn a_tabs_height_stays_with_that_tab_and_not_with_the_panel() {
    let _guard = RESIZE_ENV.lock().unwrap_or_else(|p| p.into_inner());
    let (_dir, ui, app) = harness();
    let core = app.core();
    let pump = || core.drain();
    wait_for("tab ready", || {
        pump();
        !core.tab_snapshot().is_empty() && core.tab_snapshot()[0].ready
    });
    open(&ui, pump);
    let first = ui.get_devtools_height();
    edge(&ui, 0, 0.0);
    edge(&ui, 1, 120.0);
    edge(&ui, 2, 0.0);
    let dragged = ui.get_devtools_height();
    assert!(near(dragged, first + 120.0), "dragged the first tab's panel to {dragged}");

    // A second tab is a different reader's window height: its own panel is the
    // default, and so is the panel on screen the moment it becomes the tab.
    ui.invoke_new_tab();
    wait_for("second tab ready", || {
        pump();
        core.tab_snapshot().len() == 2 && core.tab_snapshot()[1].ready
    });
    assert_eq!(core.devtools_heights().len(), 2);
    assert!(core.devtools_heights()[1].is_none(), "the new tab was never dragged");
    wait_for("the panel switched off with the tab", || {
        pump();
        !ui.get_devtools_open()
    });
    wait_for("panel height switched to the new tab's", || {
        pump();
        near(ui.get_devtools_height(), default_height(&ui))
    });
    press_f12(&ui);
    wait_for("second tab's panel open", || {
        pump();
        ui.get_devtools_open()
    });
    assert!(
        near(ui.get_devtools_height(), default_height(&ui)),
        "the other tab's drag must not have moved this one: {}",
        ui.get_devtools_height()
    );

    // And back to the first tab: its own height comes back, still its own.
    press_alt_1(&ui);
    wait_for("the first tab's height back", || {
        pump();
        near(ui.get_devtools_height(), dragged)
    });
    let heights = core.devtools_heights();
    assert_eq!(heights.len(), 2);
    assert!(heights[0].is_some() && heights[1].is_none(), "one drag, one tab: {heights:?}");
}

// ---- the same drag, driven by real pointer events ---------------------------

/// Hand the window a pointer move the way a backend does.
fn pointer_move(ui: &AppWindow, x: f32, y: f32) {
    ui.window().dispatch_event(slint::platform::WindowEvent::PointerMoved {
        position: slint::LogicalPosition::new(x, y),
    });
}

fn pointer_press(ui: &AppWindow, x: f32, y: f32) {
    ui.window().dispatch_event(slint::platform::WindowEvent::PointerPressed {
        position: slint::LogicalPosition::new(x, y),
        button: slint::platform::PointerEventButton::Left,
    });
}

fn pointer_release(ui: &AppWindow, x: f32, y: f32) {
    ui.window().dispatch_event(slint::platform::WindowEvent::PointerReleased {
        position: slint::LogicalPosition::new(x, y),
        button: slint::platform::PointerEventButton::Left,
    });
}

/// The distance a drag moves the panel has to be measured in a frame that does
/// not move — and the strip rides the panel up as the panel grows, so a travel
/// read off the strip's *own* frame loses the ground the drag has already won
/// and the panel crawls at half the pointer's speed. That is invisible to a test
/// that calls the callback with a number, so this one drives the window with
/// real pointer events and holds the panel to the pointer, one for one.
#[test]
fn a_real_drag_moves_the_panel_the_distance_the_pointer_moved() {
    let _guard = RESIZE_ENV.lock().unwrap_or_else(|p| p.into_inner());
    let (_dir, ui, app) = harness();
    ui.show().expect("show on the testing backend");
    let core = app.core();
    let pump = || core.drain();
    wait_for("tab ready", || {
        pump();
        !core.tab_snapshot().is_empty() && core.tab_snapshot()[0].ready
    });
    open(&ui, pump);

    // Grab the strip 2 px above the panel's own top edge: over the page above,
    // not over the panel, nowhere near the header buttons.
    let (px, py, pw, _) = *boxes(&ui, "dt-panel").first().expect("the panel is laid out");
    let (_, sy, _, sh) = *boxes(&ui, "dt-edge").first().expect("the strip is laid out");
    let x = px + pw * 0.5;
    let from = py - 2.0;
    assert!(
        from > sy && from < sy + sh,
        "the grab lands inside the strip: {from} vs {sy}..{}",
        sy + sh
    );

    // How far up this window will let a panel of this height be dragged at all:
    // the measurements below must be drags, not the ceiling being felt.
    let start = ui.get_devtools_height();
    let room = ui.get_page_height() * 0.9 - start - 10.0;
    assert!(room > 40.0, "not enough page above the panel to drag in: {room}");

    pointer_move(&ui, x, from);
    pointer_press(&ui, x, from);
    for dy in [8.0f32, room * 0.4, room] {
        pointer_move(&ui, x, from - dy);
        pump();
        let got = ui.get_devtools_height();
        assert!(
            near(got, start + dy),
            "the pointer came up {dy}px and the panel came up {}px (from {start} to {got})",
            got - start
        );
        // The strip comes up with the panel, so it stays under the pointer —
        // which is what makes a stale frame of reference possible at all.
        let now = boxes(&ui, "dt-edge").first().expect("the strip is laid out").1;
        assert!(near(now, sy - dy), "the strip follows the panel up: {now} vs {}", sy - dy);
    }

    // Back to where the press happened and the panel is back where it began:
    // travel is measured off the grab, in a frame that does not move.
    pointer_move(&ui, x, from);
    assert!(
        near(ui.get_devtools_height(), start),
        "a panel dragged up {room}px and back down {room}px is where it started: {}",
        ui.get_devtools_height()
    );
    pointer_release(&ui, x, from);
    pump();
    assert!(
        near(core.devtools_heights()[0].expect("the height the drag ended at"), start),
        "and that is the height the tab keeps"
    );
}
