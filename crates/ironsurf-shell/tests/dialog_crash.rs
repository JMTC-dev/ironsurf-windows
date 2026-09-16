//! A crashing tab must take its open modal dialog with it (task: JS
//! dialogs): the fake tab (`--dialog --crash`) raises an alert and then
//! panics; the shell's crash check has to close the overlay rather than
//! hang waiting for a user answer no one can deliver any more.
//! (Own test binary: `IRONSURF_DATA_DIR` is process-global, one `App` per
//! process — same convention as `tab_crash.rs`.)

use std::path::PathBuf;
use std::time::{Duration, Instant};

use ironsurf_shell::app::App;
use ironsurf_shell::AppWindow;

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

/// A tab that dies while its modal is up must not leave the overlay (and a
/// blocked answer) on screen forever: the crash check closes the dialog.
#[test]
fn a_crashing_tabs_modal_closes_with_it() {
    i_slint_backend_testing::init_no_event_loop();

    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("IRONSURF_DATA_DIR", dir.path());

    let ui = AppWindow::new().expect("window on testing backend");
    let fake = PathBuf::from(env!("CARGO_BIN_EXE_fake_tab"));
    let app = App::new(
        &ui,
        fake,
        vec!["--dialog".to_string(), "--crash".to_string()],
    )
    .expect("app core");
    app.start(None);
    let pump = || app.core().drain();

    // The modal opens from the fake tab's last act before its panic.
    wait_for("the doomed tab's dialog opens", || {
        pump();
        ui.get_dialog_open()
    });
    // The fake exits 101 after the dialog; the shell's crash check closes the
    // overlay with it (and there is nobody left to answer).
    wait_for("the crash closes the overlay", || {
        pump();
        !ui.get_dialog_open() && app.core().pending_dialogs().is_empty()
    });
}
