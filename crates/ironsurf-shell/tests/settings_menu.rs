//! Settings menu (task): the ☰ (three-lines) panel that used to open only
//! the browsing history is the browser's one settings area, and its bottom
//! item is the scrolling-speed slider. Whatever the slider does is applied
//! browser-wide — `ToTab::SetScrollSpeed` to every running tab, the same
//! seed to every fresh one — persisted to `ui.json` at once, clamped to the
//! slider's own range (0.2× … 3×), and NaN-ed back to the default. The
//! scripted fake tab tolerates the new message (it has no wheel to feel it);
//! the tab-side scaling itself is pinned in the runtime's own suite.
//!
//! Own test binary: `IRONSURF_DATA_DIR` is process-global, one `App` per
//! binary (see `dialog_crash.rs`). Headless (`i-slint-backend-testing`).

use std::path::PathBuf;
use std::time::{Duration, Instant};

use ironsurf_shell::app::App;
use ironsurf_shell::uisettings::UiSettings;
use ironsurf_shell::AppWindow;
use slint::{ComponentHandle, Model};

fn wait_for(what: &str, cond: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if cond() {
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn scroll_speed_slider_applies_persists_and_clamps() {
    i_slint_backend_testing::init_no_event_loop();

    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("IRONSURF_DATA_DIR", dir.path());
    let ui_json = dir.path().join("ui.json");
    let disk = || UiSettings::load(&ui_json);

    let ui = AppWindow::new().expect("window on testing backend");
    let fake = PathBuf::from(env!("CARGO_BIN_EXE_fake_tab"));
    let app = App::new(&ui, fake, Vec::new()).expect("app core");
    let core = app.core();
    let pump = || core.drain();

    app.start(None);
    wait_for("first tab", || {
        pump();
        ui.get_tabs().row_count() == 1
    });

    // ── untouched: the slider sits on the default damping ───────────────────
    assert!(
        disk().scroll_speed.is_none(),
        "a fresh profile has no stored speed"
    );
    assert!(
        (ui.get_scroll_speed() - 1.0).abs() < 1e-6,
        "the panel opens on 100%"
    );

    // ── the slider at 2×: reflected, persisted, eaten by the live tab ──────
    ui.invoke_scroll_speed_set(2.0);
    assert!(
        (ui.get_scroll_speed() - 2.0).abs() < 1e-6,
        "the slider shows what it set"
    );
    assert_eq!(disk().scroll_speed, Some(2.0), "the choice hits the disk");
    pump();
    assert_eq!(ui.get_tabs().row_count(), 1, "the live tab ate the new speed");

    // A tab spawned afterwards is seeded the same value and survives it.
    assert!(ui.invoke_url_chord("t".into(), true, false, false, false));
    wait_for("second tab", || {
        pump();
        ui.get_tabs().row_count() == 2
    });

    // ── clamped to the slider's range, nonsense lands on the default ────────
    ui.invoke_scroll_speed_set(99.0);
    assert!(
        (ui.get_scroll_speed() - 3.0).abs() < 1e-6,
        "3× is as fast as the slider goes"
    );
    ui.invoke_scroll_speed_set(0.01);
    assert!(
        (ui.get_scroll_speed() - 0.2).abs() < 1e-6,
        "0.2× is as slow as it gets"
    );
    ui.invoke_scroll_speed_set(f32::NAN);
    assert!(
        (ui.get_scroll_speed() - 1.0).abs() < 1e-6,
        "NaN is not a speed: default damping"
    );
    assert_eq!(disk().scroll_speed, Some(1.0), "and that persists too");

    // Both tabs are still alive through it all.
    pump();
    assert_eq!(ui.get_tabs().row_count(), 2);
}
