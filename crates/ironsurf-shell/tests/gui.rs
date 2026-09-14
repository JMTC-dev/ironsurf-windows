//! Headless GUI integration test: constructs the real Slint window on the
//! testing backend (no display), drives it with synthetic callbacks and a
//! scripted fake tab process, and checks the state → model pipeline.
//!
//! Runs with `cargo test -p ironsurf-shell` — no X, no Servo.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use ironsurf_shell::app::App;
use ironsurf_shell::AppWindow;
use slint::Model;

/// Drive the pump manually (the drain timer needs a running event loop,
/// which the testing backend does not spin) until `cond` holds.
fn wait_for(what: &str, cond: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        // Drain happens through the app core; caller closes over it.
        if cond() {
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn window_model_and_fake_tab_pump() {
    i_slint_backend_testing::init_no_event_loop();

    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("IRONSURF_DATA_DIR", dir.path());

    let ui = AppWindow::new().expect("window on testing backend");
    let fake = PathBuf::from(env!("CARGO_BIN_EXE_fake_tab"));
    let app = App::new(&ui, fake, Vec::new()).expect("app core");
    app.start(None); // ironsurf://welcome (the built-in start page) in the default container

    let pump = || app.core().drain();
    let snap = || app.core().tab_snapshot();

    // Fake-tab preamble must reduce into state + model.
    wait_for("state appeared", || {
        pump();
        !snap().is_empty() && snap()[0].ready && !snap()[0].title.is_empty()
    });
    let state = snap().into_iter().next().unwrap();
    assert_eq!(state.url.as_deref(), Some("ironsurf://welcome"));
    assert_eq!(state.title, "Fake — ironsurf://welcome");
    assert!(!state.loading);

    let tabs = ui.get_tabs();
    assert_eq!(tabs.row_count(), 1);
    let row = tabs.row_data(0).unwrap();
    assert_eq!(row.title.as_str(), "Fake — ironsurf://welcome");
    assert!(row.active);
    assert_eq!(ui.get_url_text().as_str(), "ironsurf://welcome");
    assert_eq!(ui.get_container_label().as_str(), "personal");

    // Bare host typed in the URL bar → classified to https:// and echoed
    // back by the fake tab through the pump.
    ui.invoke_navigate("example.org/page".into());
    wait_for("navigate round-trip", || {
        pump();
        snap()[0].url.as_deref() == Some("https://example.org/page")
    });

    // A phrase with spaces → DuckDuckGo search.
    ui.invoke_navigate("rustlang news".into());
    wait_for("search round-trip", || {
        pump();
        snap()[0].url.as_deref() == Some("https://html.duckduckgo.com/html/?q=rustlang+news")
    });

    // Container manager: recolor the default container, add a new one.
    ui.invoke_mgr_select(0);
    ui.invoke_mgr_recolor(0, 2); // palette #6aa84f
    let conts = ui.get_containers();
    assert_eq!(conts.row_count(), 1);
    let c0 = conts.row_data(0).unwrap();
    // slint 1.x Color channels are u8: palette #6aa84f → g=168 > r=106.
    assert!(c0.color.green() > 150 && c0.color.green() > c0.color.red());

    ui.set_container_edit_text("work".into());
    ui.invoke_mgr_add();
    assert_eq!(ui.get_containers().row_count(), 2);
    assert_eq!(ui.get_containers().row_data(1).unwrap().name.as_str(), "work");

    // Move the active tab into "work" (re-spawn keeps the URL).
    ui.invoke_mgr_move_tab_to(1);
    wait_for("moved tab ready", || {
        pump();
        snap()[0].ready
            && snap()[0].url.as_deref() == Some("https://html.duckduckgo.com/html/?q=rustlang+news")
    });
    assert_eq!(ui.get_container_label().as_str(), "work");

    // Deleting the (now unused) default container is refused; deleting the
    // non-default one while a tab uses it drops that tab ⇒ loop quit path.
    ui.invoke_close_tab(0);
    assert!(snap().is_empty());

    app.shutdown();
}
