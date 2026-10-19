//! The status bar's loading bar (task: website loading progress, bottom bar,
//! left-hand side).
//!
//! The bar is the active tab's `loading` state in another shape, so this test
//! checks the *whole* route it travels: the real `ironsurf-tab` navigating to
//! a page that keeps the tab busy, the shell seeing the load in flight, and
//! `Core::tick_spinner` (the spinner timer's body) pushing that onto the
//! window's `loading` / `load-progress` properties that ironsurf.slint renders
//! as the sweeping segment.
//!
//! Skips when `ironsurf-tab` is not built next to the test binary (plain
//! `cargo test -p ironsurf-shell`).

use std::path::PathBuf;
use std::time::{Duration, Instant};

use ironsurf_shell::app::{App, LOAD_SWEEP_STEPS};
use ironsurf_shell::AppWindow;

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

#[test]
fn the_window_bar_shows_the_active_tabs_load_as_it_happens() {
    let Some(tab_bin) = real_tab_bin() else {
        eprintln!("skip: ironsurf not built (cargo build --workspace first)");
        return;
    };
    i_slint_backend_testing::init_no_event_loop();

    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("IRONSURF_DATA_DIR", dir.path());

    // The fixture has to stay loading for a while *after* the shell has been
    // told the load started: a script that blocks the tab keeps
    // `LoadingChanged { loading: true }` in flight (the `false` only comes
    // when the document settles), which is exactly the window the bar is up.
    let store = ironsurf_storage::Store::open(ironsurf_storage::default_data_root()).unwrap();
    let profile = store.profile_dir(store.default_container_id());
    std::fs::create_dir_all(&profile).unwrap();
    std::fs::write(
        profile.join("fast.html"),
        "<html><head><title>Fast page</title></head><body><p>quick</p></body></html>",
    )
    .unwrap();
    std::fs::write(
        profile.join("slow.html"),
        "<html><head><title>Slow page</title></head>\
         <body><h1>Still working</h1>\
         <script>\
           var end = Date.now() + 3000; while (Date.now() < end) { }\n\
           document.title = 'Slow page done';\n\
         </script></body></html>",
    )
    .unwrap();
    let fast = url::Url::from_file_path(profile.join("fast.html"))
        .unwrap()
        .to_string();
    let slow = url::Url::from_file_path(profile.join("slow.html"))
        .unwrap()
        .to_string();

    let ui = AppWindow::new().expect("window on testing backend");
    let app = App::new(&ui, tab_bin, Vec::new()).expect("app core");
    let snap = || app.core().tab_snapshot();
    app.start(Some(&fast));

    wait_for("fast fixture loaded", &app, || {
        !snap().is_empty()
            && snap()[0].ready
            && snap()[0].title == "Fast page"
            && !snap()[0].loading
    });

    // Idle: the bar is down and parked at the track's left end.
    assert!(!snap()[0].loading, "fixture page must not be loading yet");
    app.core().tick_spinner();
    assert!(!ui.get_loading(), "an idle tab must not raise the bar");
    assert_eq!(ui.get_load_progress(), 0);

    // Navigate: poll the window property (draining between samples, and
    // ticking as the real spinner timer would) until the load is in flight.
    ui.invoke_navigate(slow.clone().into());
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut saw_load = false;
    let mut saw_progress = false;
    while Instant::now() < deadline && !saw_load {
        app.core().drain();
        app.core().tick_spinner();
        saw_load = ui.get_loading();
    }
    assert!(
        saw_load,
        "the window never showed the tab's load in flight"
    );

    // The load is in flight: tick the way the spinner timer does and watch
    // the segment travel along the track.
    for _ in 0..5 {
        std::thread::sleep(Duration::from_millis(130));
        app.core().drain();
        app.core().tick_spinner();
        if !ui.get_loading() {
            break;
        }
        assert!(
            (ui.get_load_progress() as usize) < LOAD_SWEEP_STEPS,
            "the sweep left the track mid-load: {}",
            ui.get_load_progress()
        );
        if ui.get_load_progress() > 0 {
            saw_progress = true;
        }
    }

    // Done: the same messages that stop the spinner drop the bar again, and
    // the tab really did finish the blocking script.
    wait_for("load finished", &app, || {
        app.core().tick_spinner();
        !snap()[0].loading && !ui.get_loading()
    });
    assert!(
        saw_progress,
        "the segment never swept while the page was loading"
    );
    assert!(
        snap()[0].title.contains("done"),
        "fixture script did not run to the end: {:?}",
        snap()[0].title
    );

    app.shutdown();
}
