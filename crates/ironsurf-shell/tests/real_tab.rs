//! Real-tab integration test: drives the actual Slint `AppWindow` + [`App`]
//! against the **real** `ironsurf-tab` binary (Blitz + vello_cpu, same
//! workspace, resolved next to the test executable) — the full
//! shell → child-process → engine → frame pipeline, headless.
//!
//! The page fixtures live *inside the container's profile dir* because the
//! tab sandbox makes that the only place it may read outside its system
//! read-only set (same trick as `ironsurf-tab/tests/e2e.rs`).
//!
//! The live-web leg (https://example.com) only runs with `IRONSURF_ONLINE=1`.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use ironsurf_shell::app::App;
use ironsurf_shell::AppWindow;
use slint::Model;

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

/// The navigation chord in the spelling this platform reads — `Alt-\u{2190}` / `Alt-\u{2192}`
/// off a Mac, `\u{2318}-\u{2190}` / `\u{2318}-\u{2192}` on one (the same fold as the tab-number chords,
/// `keymap::Chords::digits_on_alt`) — delivered as the page's `FocusScope` would.
fn nav_chord(ui: &AppWindow, text: &str) {
    let on_alt = ironsurf_shell::keymap::Chords::CURRENT.digits_on_alt();
    ui.invoke_page_key(true, text.into(), false, false, on_alt, !on_alt, false);
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
fn app_browses_with_the_real_tab_binary() {
    let Some(tab_bin) = real_tab_bin() else {
        eprintln!("skip: ironsurf not built (cargo build --workspace first)");
        return;
    };
    i_slint_backend_testing::init_no_event_loop();

    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("IRONSURF_DATA_DIR", dir.path());

    // Fixtures inside the default container's profile dir (sandbox-readable).
    let store = ironsurf_storage::Store::open(ironsurf_storage::default_data_root()).unwrap();
    let profile = store.profile_dir(store.default_container_id());
    std::fs::create_dir_all(&profile).unwrap();
    std::fs::write(
        profile.join("page1.html"),
        "<html><head><title>IronSurf page one</title></head>\
         <body><h1>One</h1></body></html>",
    )
    .unwrap();
    std::fs::write(
        profile.join("page2.html"),
        "<html><head><title>IronSurf page two</title></head>\
         <body><h1>Two</h1></body></html>",
    )
    .unwrap();
    let page1 = url::Url::from_file_path(profile.join("page1.html"))
        .unwrap()
        .to_string();
    let page2 = url::Url::from_file_path(profile.join("page2.html"))
        .unwrap()
        .to_string();

    let ui = AppWindow::new().expect("window on testing backend");
    let app = App::new(&ui, tab_bin, Vec::new()).expect("app core");
    let snap = || app.core().tab_snapshot();

    app.start(Some(&page1));

    // The real engine must parse the fixture and report over real IPC.
    wait_for("real-tab preamble", &app, || {
        !snap().is_empty()
            && snap()[0].ready
            && snap()[0].title == "IronSurf page one"
            && snap()[0].url.as_deref() == Some(page1.as_str())
    });

    // vello_cpu frames must reach the compositor (Blitz paint → FromTab::Frame).
    wait_for("first rendered frame", &app, || {
        app.core().drain();
        let s = ui.get_page_image().size();
        s.width >= 16 && s.height >= 16
    });
    let size = ui.get_page_image().size();
    assert!(size.width >= 16 && size.height >= 16, "frame size {size:?}");
    let row = ui.get_tabs().row_data(0).unwrap();
    assert_eq!(row.title.as_str(), "IronSurf page one");
    assert_eq!(ui.get_url_text().as_str(), page1.as_str());

    // In-tab navigation through the URL bar (classify → ToTab::Navigate).
    ui.invoke_navigate(page2.clone().into());
    wait_for("navigation to page two", &app, || {
        snap()[0].url.as_deref() == Some(page2.as_str())
            && snap()[0].title == "IronSurf page two"
            && snap()[0].can_back
    });
    assert!(ui.get_can_back());

    // Real history: toolbar Back then Forward.
    ui.invoke_go_back();
    wait_for("history back", &app, || {
        snap()[0].url.as_deref() == Some(page1.as_str()) && snap()[0].can_forward
    });
    assert!(ui.get_can_fwd());
    ui.invoke_go_forward();
    wait_for("history forward", &app, || {
        snap()[0].url.as_deref() == Some(page2.as_str())
    });

    // The arrow chord, end to end through a real tab. Arrows reach the shell as
    // Slint's private-use keysyms and the tab answers the chord out of its own
    // session history, exactly as it answered the toolbar buttons; `nav_chord`
    // presses it in this platform's own spelling (`Alt-←` here, `⌘-←` on a Mac).
    nav_chord(&ui, "\u{F702}");
    wait_for("the back arrow chord", &app, || {
        snap()[0].url.as_deref() == Some(page1.as_str())
    });
    nav_chord(&ui, "\u{F703}");
    wait_for("the forward arrow chord", &app, || {
        snap()[0].url.as_deref() == Some(page2.as_str())
    });
    // Ctrl-\u{2190} is not the chord off a Mac (it moves a text caret a word at a
    // time), so the page keeps it and the history does not move.
    ui.invoke_page_key(true, "\u{F702}".into(), true, false, false, false, false);
    std::thread::sleep(Duration::from_millis(300));
    app.core().drain();
    assert_eq!(
        snap()[0].url.as_deref(),
        Some(page2.as_str()),
        "Ctrl-\u{2190} belongs to the page, not to history"
    );
    // PageUp/PageDown scroll (see `ironsurf-tab`'s page-scroll tests); as shell
    // keys they may not disturb anything else, and PageDown bound to a tab switch
    // would have been the bug.
    ui.invoke_page_key(true, "\u{F72D}".into(), false, false, false, false, false);
    std::thread::sleep(Duration::from_millis(300));
    app.core().drain();
    assert_eq!(snap().len(), 1, "PageDown switched away from the only tab");
    assert_eq!(
        snap()[0].url.as_deref(),
        Some(page2.as_str()),
        "PageDown navigated instead of scrolling"
    );

    // Keyboard/mouse events traverse the whole path without killing the tab
    // (the tab-side semantics are covered by ironsurf-tab's own e2e).
    ui.invoke_page_key(true, "x".into(), false, false, false, false, false);
    ui.invoke_page_pointer(120.0, 90.0, 0, 0, false, false, false, false);
    ui.invoke_page_wheel(120.0, 90.0, 0.0, 40.0);
    std::thread::sleep(Duration::from_millis(200));
    app.core().drain();
    assert_eq!(snap().len(), 1);
    assert!(!snap()[0].closed && snap()[0].error.is_none());

    // Live web through the shell (localhost+example.com is the only network
    // the CI contract allows) — opt-in so default runs stay hermetic.
    if std::env::var("IRONSURF_ONLINE").is_ok_and(|v| v == "1") {
        ui.invoke_navigate("https://example.com".into());
        wait_for("live https://example.com title", &app, || {
            snap()[0]
                .title
                .to_ascii_lowercase()
                .contains("example domain")
        });
    }

    // Closing the only tab drops the child process (EOF + reap).
    ui.invoke_close_tab(0);
    wait_for("tab gone", &app, || snap().is_empty());

    app.shutdown();
}
