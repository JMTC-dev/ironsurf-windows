//! A crashed tab must never die silently: the drain tick notices the dead
//! child (no `Closed`, `try_wait` says exited) and pins a “Tab N has crashed”
//! message onto the tab strip's status line. Uses the scripted `fake_tab`
//! in `--crash` mode (panics on stderr, exits 101) — no engine, no X.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use ironsurf_shell::app::App;
use ironsurf_shell::AppWindow;

#[test]
fn crashed_tab_shows_error_in_status_line() {
    i_slint_backend_testing::init_no_event_loop();

    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("IRONSURF_DATA_DIR", dir.path());

    let ui = AppWindow::new().expect("window on testing backend");
    let fake = PathBuf::from(env!("CARGO_BIN_EXE_fake_tab"));
    let app = App::new(&ui, fake, vec!["--crash".to_string()]).expect("app core");
    app.start(None);
    let core = app.core();

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        core.drain();
        let snap = core.tab_snapshot();
        if let Some(state) = snap.first() {
            if state
                .error
                .as_deref()
                .is_some_and(|e| e.contains("has crashed"))
            {
                // The message survives into the UI, and the tab is still in
                // the strip (last frame + crash message), not silently gone.
                assert_eq!(snap.len(), 1);
                let status = ui.get_status_text().to_string();
                assert!(status.contains("has crashed"), "status line says: {status:?}");
                app.shutdown();
                return;
            }
        }
        assert!(
            Instant::now() < deadline,
            "crash never surfaced in the status line: {snap:?}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}
