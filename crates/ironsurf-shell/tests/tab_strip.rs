//! The tab strip keeps itself inside the bar, and says more when asked
//! (task: tab strip fits the bar):
//!
//!   * a tab takes the width its title wants **while every tab can have that**; once
//!     they cannot, every tab takes an even share of the bar — so a long row of long
//!     titles stays inside the strip instead of running off its right end, and each
//!     title is elided to whatever its tab got. Only once a share would be too narrow
//!     to read (stripe + badge + ✕ + a syllable) do the tabs stop shrinking and the
//!     row overflow for the Flickable to pan;
//!   * a title is never drawn outside its own tab — the elision is real geometry, not
//!     a wish: a bare `max-width` left the `Text` its full preferred width and the
//!     words ran over the neighbour;
//!   * the ellipsis hides nothing: hovering a tab puts its **full** title in the
//!     window's one hover tooltip, below the tab and inside the window, and the
//!     tooltip goes away when the pointer does (the ✕ asks for nothing).
//!
//! Own test binary: `IRONSURF_DATA_DIR` is process-global, so every test
//! serializes on one lock and each keeps its data dir alive until finished.
//! Headless (`i-slint-backend-testing`): no X, no engine — the tab is the
//! scripted `fake_tab`, whose title is `Fake — <url>`.

use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use ironsurf_shell::AppWindow;
use ironsurf_shell::app::App;
use slint::{ComponentHandle, Model};

/// One test at a time: the data dir is chosen by a process-wide variable.
fn serialized() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
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

/// `(data dir, window, app)` — the directory must outlive the app.
fn shell() -> (tempfile::TempDir, AppWindow, App) {
    i_slint_backend_testing::init_no_event_loop();
    let dir = tempfile::tempdir().expect("data dir");
    std::env::set_var("IRONSURF_DATA_DIR", dir.path());
    let ui = AppWindow::new().expect("window on testing backend");
    let fake = PathBuf::from(env!("CARGO_BIN_EXE_fake_tab"));
    let app = App::new(&ui, fake, vec![]).expect("app core");
    app.start(None);
    (dir, ui, app)
}

/// The tab strip as the window sees it: one title per tab, in order.
fn titles(ui: &AppWindow) -> Vec<String> {
    ui.get_tabs()
        .iter()
        .map(|t| t.title.to_string())
        .collect()
}

/// `(x, y, width, height)` of every element with this id that the window shows —
/// an element clipped out of its parent (a tab panned past the end of the bar)
/// is not in the answer at all.
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

/// Move the virtual pointer — hover is derived from where the pointer *is*.
fn pointer(ui: &AppWindow, at: (f32, f32)) {
    ui.window()
        .dispatch_event(slint::platform::WindowEvent::PointerMoved {
            position: slint::LogicalPosition::new(at.0, at.1),
        });
}

/// The boxes of `id`, waited for. A `for` over a model realizes its items during
/// the update pass and the testing backend runs no event loop, so a tab list that
/// has just grown is not in the tree yet — keep pointing at the page (which has
/// nothing to say, unlike the strip) until the row is laid out.
fn wait_for_boxes(ui: &AppWindow, id: &str, n: usize) -> Vec<(f32, f32, f32, f32)> {
    let what = format!("{id} × {n}");
    wait_for(&what, || {
        pointer(ui, (20.0, 500.0));
        boxes(ui, id).len() == n
    });
    boxes(ui, id)
}

/// Titles so the strip has something to run out of room with: the fake tab
/// titles itself `Fake — <url>`, so a long address is a long title.
fn long_title(seed: usize) -> String {
    format!(
        "https://news.test/{seed}/the-quick-brown-fox-jumps-over-the-lazy-dog-again-and-again?page={seed}&ref=strip"
    )
}

/// Wait until the tab list holds `n` tabs, each with a title of its own.
fn wait_for_tabs(ui: &AppWindow, app: &App, n: usize) {
    let core = app.core();
    wait_for("tabs with titles", || {
        core.drain();
        let tabs = titles(ui);
        tabs.len() == n && tabs.iter().all(|t| !t.is_empty())
    });
}

#[test]
fn a_crowded_strip_stays_inside_the_bar_and_a_hovered_tab_says_its_title() {
    let _serialized = serialized();
    let (_dir, ui, app) = shell();
    ui.show().expect("show on the testing backend");
    ui.window()
        .set_size(slint::LogicalSize::new(800.0, 600.0));
    let core = app.core();
    wait_for("the first tab", || {
        core.drain();
        titles(&ui).len() == 1
    });

    // One tab, long title: there is room for what it wants, so it takes it —
    // and the bar is nowhere near full.
    ui.invoke_navigate(long_title(0).into());
    wait_for("the long title", || {
        core.drain();
        titles(&ui)
            .first()
            .is_some_and(|t| t.chars().count() > 60)
    });
    let roomy = wait_for_boxes(&ui, "tabrect", 1);
    assert_eq!(roomy.len(), 1);
    let bar = boxes(&ui, "stripr")[0];
    assert!(
        roomy[0].2 > 120.0 && roomy[0].2 <= 220.5,
        "a lone tab keeps the width its title wants (up to the 220 cap): {roomy:?}"
    );
    assert!(roomy[0].0 + roomy[0].2 <= bar.0 + bar.2 + 0.5);

    // A hover says the whole title, even though the tab showed at most 220 px
    // of it.
    let hover = (roomy[0].0 + 40.0, roomy[0].1 + roomy[0].3 / 2.0);
    pointer(&ui, hover);
    assert!(ui.get_tip_on(), "the pointer rests on a tab");
    let full = titles(&ui).first().cloned().unwrap_or_default();
    assert_eq!(ui.get_tip_text().as_str(), full, "the tip is the title in full");
    let tip = boxes(&ui, "tip-layer");
    assert!(tip[0].3 > 10.0, "a box with words in it: {tip:?}");
    assert!(
        tip[0].1 >= roomy[0].1 + roomy[0].3,
        "the tip hangs below its tab: {tip:?} vs {roomy:?}"
    );
    let window = ui.window().size();
    assert!(
        tip[0].0 >= 0.0 && tip[0].0 + tip[0].2 <= window.width as f32 + 0.5,
        "the tip stays on screen: {tip:?} in {window:?}"
    );
    // The tip carries more title than the tab did: it is wider than the strip
    // gave the title, which is the whole point of the tooltip.
    let shown = boxes(&ui, "tt");
    assert!(
        shown[0].0 + shown[0].2 <= roomy[0].0 + roomy[0].2 - 20.0,
        "the title never runs past its own tab: label {:?} in tab {:?}",
        shown[0],
        roomy[0]
    );
    assert!(
        tip[0].2 > shown[0].2,
        "the tip outshows the elided label: tip {:?} vs label {:?}",
        tip[0],
        shown[0]
    );

    // The pointer leaving puts it away again.
    pointer(&ui, (hover.0, bar.1 + 300.0));
    assert!(!ui.get_tip_on(), "the pointer is off the strip");
    assert_eq!(ui.get_tip_text().as_str(), "");

    // Now crowd it: eight long-titled tabs in an 800 px bar. Every one of them
    // would take its 220 (1760 px together); they take an even share instead
    // and the whole row stays inside the bar.
    for _ in 1..8 {
        ui.invoke_new_tab();
    }
    wait_for_tabs(&ui, &app, 8);
    let bar = boxes(&ui, "stripr")[0];
    let tabs = wait_for_boxes(&ui, "tabrect", 8);
    assert_eq!(tabs.len(), 8);
    let widest = tabs.iter().map(|b| b.2).fold(0.0, f32::max);
    let narrowest = tabs.iter().map(|b| b.2).fold(1e9, f32::min);
    assert!(
        widest - narrowest < 1.0,
        "eight tabs share the bar evenly: {tabs:?}"
    );
    assert!(
        widest < 120.0,
        "the share squeezed the 220 px tabs: widest {widest}"
    );
    let right = tabs.iter().map(|b| b.0 + b.2).fold(0.0, f32::max);
    assert!(
        right <= bar.0 + bar.2 + 0.5,
        "the last tab ends inside the bar: row ends {right}, bar ends {}",
        bar.0 + bar.2
    );
    assert!(
        tabs.iter().all(|b| b.0 >= bar.0 - 0.5),
        "no tab starts before the bar: {tabs:?} vs {bar:?}"
    );

    // Nothing is drawn outside the bar: the row itself fits, so there is
    // nothing left to pan.
    let row = boxes(&ui, "tab-row")[0];
    assert!(
        row.2 <= bar.2 + 0.5,
        "the row is no wider than the bar: {row:?} vs {bar:?}"
    );

    // And each squeezed title is cut to what its tab has left (elided), while
    // the hover still says everything.
    let labels = boxes(&ui, "tt");
    assert_eq!(labels.len(), 8);
    for (label, tab) in labels.iter().zip(tabs.iter()) {
        assert!(
            label.0 + label.2 <= tab.0 + tab.2 - 20.0,
            "the title stops before the ✕: label {label:?} in tab {tab:?}"
        );
    }
    let third = tabs[2];
    pointer(&ui, (third.0 + 40.0, third.1 + third.3 / 2.0));
    assert!(ui.get_tip_on());
    let want = titles(&ui).into_iter().nth(2).expect("third tab");
    assert_eq!(
        ui.get_tip_text().as_str(),
        want,
        "a squeezed tab still hands over its full title"
    );
    let tip = boxes(&ui, "tip-layer");
    assert!(
        tip[0].0 + tip[0].2 <= window.width as f32 + 0.5,
        "a tip over a tab near the right edge stays on screen: {tip:?}"
    );

    // The ✕ asks for nothing: pointing at the close button is not pointing at
    // the title.
    let close = (tabs[2].0 + tabs[2].2 - 10.0, tabs[2].1 + tabs[2].3 / 2.0);
    pointer(&ui, close);
    assert!(!ui.get_tip_on(), "the ✕ keeps the tooltip to itself");
}

#[test]
fn a_short_title_still_takes_only_what_it_needs() {
    let _serialized = serialized();
    let (_dir, ui, app) = shell();
    ui.show().expect("show on the testing backend");
    ui.window()
        .set_size(slint::LogicalSize::new(1000.0, 600.0));
    let core = app.core();
    wait_for("the first tab", || {
        core.drain();
        titles(&ui).len() == 1
    });

    // Two tabs with room to spare: each keeps the old per-title width, so a
    // short address is a short tab and a long address is a wide one, and the
    // strip never stretches them over an empty bar.
    ui.invoke_navigate("https://a.test/".into());
    wait_for("a short title", || {
        core.drain();
        titles(&ui)
            .first()
            .is_some_and(|t| t == "Fake — https://a.test/")
    });
    ui.invoke_new_tab();
    wait_for_tabs(&ui, &app, 2);
    ui.invoke_navigate(long_title(7).into());
    wait_for("the second tab's long title", || {
        core.drain();
        titles(&ui)
            .get(1)
            .is_some_and(|t| t.chars().count() > 60)
    });
    let tabs = wait_for_boxes(&ui, "tabrect", 2);
    assert!(
        tabs[0].2 < tabs[1].2,
        "a short title keeps a short tab: {tabs:?}"
    );
    let bar = boxes(&ui, "stripr")[0];
    let right = tabs.iter().map(|b| b.0 + b.2).fold(0.0, f32::max);
    assert!(right <= bar.0 + bar.2 + 0.5, "{tabs:?} vs {bar:?}");
    assert!(
        right < bar.0 + bar.2 - 100.0,
        "an uncrowded strip does not stretch its tabs across the bar: {tabs:?}"
    );
}

#[test]
fn past_the_point_of_squeezing_the_row_pans_instead() {
    let _serialized = serialized();
    let (_dir, ui, app) = shell();
    ui.show().expect("show on the testing backend");
    ui.window()
        .set_size(slint::LogicalSize::new(800.0, 600.0));
    let core = app.core();
    wait_for("the first tab", || {
        core.drain();
        titles(&ui).len() == 1
    });
    for _ in 1..12 {
        ui.invoke_new_tab();
    }
    wait_for_tabs(&ui, &app, 12);

    // Twelve long-titled tabs in an 800 px bar: a share would be 62 px, which
    // is a stripe and an ✕ with nothing to read, so the tabs stop at the floor
    // and the row goes past the bar for the Flickable to pan. What must not
    // happen either way is a title drawn outside the tab it belongs to.
    let bar = boxes(&ui, "stripr")[0];
    wait_for("the row to count all twelve tabs", || {
        pointer(&ui, (20.0, 500.0));
        boxes(&ui, "tab-row")[0].2 > bar.2 + 100.0
    });
    let row = boxes(&ui, "tab-row")[0];
    assert!(
        row.2 > bar.2,
        "twelve floored tabs overflow the bar, so there is something to pan: row {row:?} vs bar {bar:?}"
    );
    // Only the tabs the bar shows are in the tree — the rest are clipped away
    // until the row is panned to them.
    let tabs = boxes(&ui, "tabrect");
    assert!(
        tabs.len() >= 6 && tabs.len() < 12,
        "the bar shows what fits and the rest waits to be panned: {tabs:?}"
    );
    let narrowest = tabs.iter().map(|b| b.2).fold(1e9, f32::min);
    assert!(
        narrowest > 70.0,
        "a tab never squeezes below what it needs to be clicked and closed: {tabs:?}"
    );
    for (label, tab) in boxes(&ui, "tt").iter().zip(tabs.iter()) {
        assert!(
            label.0 + label.2 <= tab.0 + tab.2 - 20.0,
            "an elided title stops inside its own tab: label {label:?} in tab {tab:?}"
        );
    }

    // And every one of them can still say its whole title on hover — the ones
    // the bar is showing, at any rate; the rest have to be panned into view
    // before there is anything to point at.
    let shown = tabs[5];
    pointer(&ui, (shown.0 + 30.0, shown.1 + shown.3 / 2.0));
    assert!(
        shown.0 + shown.2 <= bar.0 + bar.2,
        "the tab being hovered is inside the bar: {shown:?} vs {bar:?}"
    );
    assert!(ui.get_tip_on(), "the pointer is on that tab");
    let want = titles(&ui).into_iter().nth(5).expect("sixth tab");
    assert_eq!(ui.get_tip_text().as_str(), want);
}
