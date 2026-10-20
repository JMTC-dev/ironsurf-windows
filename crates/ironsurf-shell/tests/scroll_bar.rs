//! The page's overlay scroll bar, end to end (task: scroll bar).
//!
//! Nothing about this bar is guessed from the shell's side: only the tab has
//! laid the page out, so the numbers come up the pipe as `FromTab::ScrollState`
//! and the shell decides two things — that the bar is up while the reader is
//! scrolling, and that it goes away a few seconds after the last movement. This
//! test drives the **real** `ironsurf-tab` (Blitz laying the page out, the pump
//! carrying the geometry) on a fixture taller than its window and reads the
//! window properties ironsurf.slint draws the bar from; the thumb's arithmetic
//! itself is unit-tested in `state::tests`, the hide is `Core::tick_spinner`
//! (the spinner timer's body) noticing the hold has run out.
//!
//! Skips when `ironsurf-tab` is not built next to the test binary (plain
//! `cargo test -p ironsurf-shell`).

use std::path::PathBuf;
use std::time::{Duration, Instant};

use ironsurf_shell::app::App;
use ironsurf_shell::state::SCROLLBAR_HOLD;
use ironsurf_shell::AppWindow;
use slint::ComponentHandle;

/// The workspace-built `ironsurf` binary (the tab host lives inside it): test binaries live in
/// `target/<profile>/deps/`, the tab sits one level up. `None` when running
/// `cargo test -p ironsurf-shell` alone (then this test skips).
fn real_tab_bin() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let profile_dir = exe.parent()?.parent()?; // target/<profile>
    // The single `ironsurf` binary hosts the tab half; the pre-merge
    // `ironsurf-tab` name is still accepted for a stale target directory.
    ["ironsurf", "ironsurf-tab"]
        .iter()
        .map(|n| profile_dir.join(format!("{n}{}", std::env::consts::EXE_SUFFIX)))
        .find(|c| c.is_file())
}

/// Drain until `cond` holds, or until `budget` runs out and say so — a timeout
/// here is a fact the test wants to print the state with, not a panic in the
/// middle of a leg.
fn wait_until(budget: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + budget;
    loop {
        if cond() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
    }
}

fn wait_for(what: &str, app: &App, cond: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        app.core().drain();
        if cond() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what}; tabs: {:?}",
            app.core().tab_snapshot()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The wheel over the page as the window hands it to the shell: `dy` follows
/// winit's convention (negative = wheel down = the document moves down the
/// page), which the tab forwards to its engine untouched.
fn wheel(ui: &AppWindow, dy: f32) {
    // The pointer has to be *on* the page for the wheel to belong to it, so
    // move it in first, exactly as a reader's hand would.
    ui.invoke_page_pointer(200.0, 150.0, 0, 0, false, false, false, false);
    ui.invoke_page_wheel(200.0, 150.0, 0.0, dy);
}

#[test]
fn the_bar_comes_up_with_the_scroll_and_hides_itself_a_few_seconds_later() {
    let Some(tab_bin) = real_tab_bin() else {
        eprintln!("skip: ironsurf not built (cargo build --workspace first)");
        return;
    };
    i_slint_backend_testing::init_no_event_loop();

    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("IRONSURF_DATA_DIR", dir.path());

    // Fixtures inside the default container's profile dir (the sandbox's only
    // readable place): one page several windows tall, one that fits.
    let store = ironsurf_storage::Store::open(ironsurf_storage::default_data_root()).unwrap();
    let profile = store.profile_dir(store.default_container_id());
    std::fs::create_dir_all(&profile).unwrap();
    std::fs::write(
        profile.join("tall.html"),
        "<!doctype html><html><head><title>Tall page</title></head>\
         <body style=\"margin:0\">\
         <div style=\"height:1200px;background:#ddeeff\"></div>\n\
         <div style=\"height:1200px;background:#ffeedd\"></div>\n\
         <div style=\"height:1200px;background:#ddeeff\"></div>\n\
         </body></html>",
    )
    .unwrap();
    std::fs::write(
        profile.join("short.html"),
        "<html><head><title>Short page</title></head>\
         <body><h1>Nothing to scroll</h1></body></html>",
    )
    .unwrap();
    let tall = url::Url::from_file_path(profile.join("tall.html")).unwrap().to_string();
    let short = url::Url::from_file_path(profile.join("short.html")).unwrap().to_string();

    let ui = AppWindow::new().expect("window on testing backend");
    let app = App::new(&ui, tab_bin, Vec::new()).expect("app core");
    let snap = || app.core().tab_snapshot();
    app.start(Some(&tall));

    // A page taller than its window reports a range without being touched: the
    // tab measures its own document, and the bar's numbers arrive on their own.
    wait_for("the tab reports a scrollable document", &app, || {
        snap().first().is_some_and(|t| t.scroll.is_scrollable())
    });
    let geometry = snap()[0].scroll;
    assert_eq!(geometry.y, 0.0, "a document opens at the top");
    assert!(geometry.viewport_h > 0.0, "the window it scrolls in: {geometry:?}");

    // …and the bar is *not* up for it. A page that merely could be scrolled is
    // not a page the reader is scrolling; nothing has moved yet.
    app.core().tick_spinner();
    assert!(
        !ui.get_scrollbar_visible(),
        "a scrollable page nobody has scrolled keeps the bar away: {geometry:?}"
    );

    // Now the reader scrolls. The bar answers the wheel, so it shows, and once
    // the tab has reported the new offset the thumb has travelled from the top
    // of the track because the document has.
    wheel(&ui, -400.0);
    wait_for("the tab reports the page scrolled", &app, || snap()[0].scroll.y > 0.0);
    let page_h = ui.get_page_height();
    assert!(page_h > 0.0, "the page area has a height to be a track");
    let (thumb_y, thumb_h) = (ui.get_scrollbar_thumb_y(), ui.get_scrollbar_thumb_height());
    assert!(thumb_h > 0.0 && thumb_h < page_h, "thumb {thumb_h} in a {page_h} track");
    assert!(thumb_y > 0.0, "the thumb moved down with the page: {thumb_y}");
    assert!(
        thumb_y + thumb_h <= page_h + 0.01,
        "the thumb stays inside its track: {thumb_y} + {thumb_h} > {page_h}"
    );
    assert!(
        ui.get_scrollbar_visible(),
        "the bar is up while the page is being scrolled: {:?}",
        snap()[0].scroll
    );

    // Keep scrolling and the thumb keeps travelling: the bottom of the track is
    // the bottom of the document, and nothing about the bar resets.
    let before = thumb_y;
    wheel(&ui, -400.0);
    wait_for("the thumb travels with the page", &app, || {
        ui.get_scrollbar_thumb_y() > before
    });

    // Then the reader stops. After the hold, the bar is gone on its own — the
    // spinner heartbeat notices, no message from the tab is owed — while the
    // page itself stays exactly where it was scrolled to. Let the wheel's
    // glide land first: `scrolled` pins the offset the page came to rest at,
    // not a step of the glide (task: slower and smoother wheel).
    std::thread::sleep(Duration::from_millis(400));
    app.core().drain();
    let scrolled = snap()[0].scroll.y;
    let started = Instant::now();
    while Instant::now() - started < SCROLLBAR_HOLD + Duration::from_millis(500) {
        app.core().drain();
        app.core().tick_spinner();
        std::thread::sleep(Duration::from_millis(50));
        if !ui.get_scrollbar_visible() {
            assert!(
                started.elapsed() >= SCROLLBAR_HOLD,
                "the bar hid before its hold ran out: {:?}",
                started.elapsed()
            );
            break;
        }
    }
    assert!(
        !ui.get_scrollbar_visible(),
        "the bar never hid itself after {:?} of holding",
        SCROLLBAR_HOLD
    );
    assert_eq!(
        snap()[0].scroll.y,
        scrolled,
        "hiding the bar must not move the page"
    );

    // The bar is not the wheel's servant: a tab that reports a *different
    // offset* puts it up, and nothing else is touching the page here. The
    // fixture scrolls itself with `window.scrollTo`, so the only thing on the
    // wire between the hidden bar and the visible one is `FromTab::ScrollState`.
    //
    // Start from a known quiet: back to the top (which shows the bar again —
    // the offset moved) and then wait that show-up out, so the bar is provably
    // down at the moment of the navigation.
    wheel(&ui, 800.0);
    wait_for("the page is back at the top", &app, || snap()[0].scroll.y == 0.0);
    wait_until(SCROLLBAR_HOLD + Duration::from_millis(500), || {
        app.core().drain();
        app.core().tick_spinner();
        std::thread::sleep(Duration::from_millis(50));
        !ui.get_scrollbar_visible()
    });
    assert!(!ui.get_scrollbar_visible(), "the bar went back down with the hold");

    std::fs::write(
        profile.join("autoscroll.html"),
        "<!doctype html><html><head><title>Self scroll</title></head>\
         <body style=\"margin:0\"><div style=\"height:3000px\"></div>\
         <script>setTimeout(function () { window.scrollTo(0, 400); }, 50);</script>\
         </body></html>",
    )
    .unwrap();
    let autoscroll = url::Url::from_file_path(profile.join("autoscroll.html"))
        .unwrap()
        .to_string();
    ui.invoke_navigate(autoscroll.clone().into());
    let showed = wait_until(Duration::from_secs(10), || {
        app.core().drain();
        app.core().tick_spinner();
        std::thread::sleep(Duration::from_millis(50));
        // Only the new document's own reports count: the old page's numbers are
        // about a page that is no longer on screen.
        snap()[0].url.as_deref() == Some(autoscroll.as_str()) && ui.get_scrollbar_visible()
    });
    assert!(
        showed,
        "a document that moves under nobody still earns the bar: {:?}",
        snap()[0].scroll
    );

    // A page that fits its window has no bar at all, however hard the reader
    // wheels at it: there is nothing off-screen to point at.
    ui.invoke_navigate(short.clone().into());
    wait_for("the short page loads", &app, || {
        snap()[0].url.as_deref() == Some(short.as_str()) && !snap()[0].loading
    });
    assert!(
        !snap()[0].scroll.is_scrollable(),
        "a page that fits reports no range: {:?}",
        snap()[0].scroll
    );
    for _ in 0..4 {
        wheel(&ui, -400.0);
        app.core().drain();
    }
    assert!(
        !ui.get_scrollbar_visible(),
        "no scroll, no bar: {:?}",
        snap()[0].scroll
    );
}
