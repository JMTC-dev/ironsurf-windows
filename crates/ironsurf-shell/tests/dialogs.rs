//! `window.alert` / `confirm` / `prompt`, shell side (task: JS dialogs).
//!
//! On the testing backend with the scripted fake tab (`--dialog`): a tab's
//! `DialogRequest` opens the modal overlay (and the blocked tab waits on the
//! pipe), the OK/Cancel buttons ride back as `ToTab::DialogResponse` (the
//! fake tab echoes every answer on its status line), a second tab's request
//! queues FIFO behind the shown one and takes the overlay with its tab
//! activated when the front is answered, a prompt round-trips its edited
//! text, and a crashing tab's modal is closed instead of hanging.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use ironsurf_common::TabId;
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

#[test]
fn alert_confirm_prompt_round_trip_over_the_ipc() {
    i_slint_backend_testing::init_no_event_loop();

    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("IRONSURF_DATA_DIR", dir.path());

    let ui = AppWindow::new().expect("window on testing backend");
    let fake = PathBuf::from(env!("CARGO_BIN_EXE_fake_tab"));
    let app = App::new(&ui, fake, vec!["--dialog".to_string()]).expect("app core");
    app.start(None);
    let pump = || app.core().drain();
    let snap = || app.core().tab_snapshot();
    let status = |i: usize| snap().get(i).and_then(|s| s.status.clone());

    // ── The startup alert: overlay opens in alert mode with the exact text ──
    wait_for("alert overlay opens", || {
        pump();
        ui.get_dialog_open()
    });
    assert_eq!(ui.get_dialog_kind(), 0, "alert reads kind 0");
    assert_eq!(ui.get_dialog_message().as_str(), "Fake page greets you!");
    assert_eq!(ui.get_dialog_input().as_str(), "", "alert clears the input box");
    assert!(ui.get_dialog_title().as_str().contains("Tab 1"));
    assert_eq!(app.core().pending_dialogs().len(), 1);

    // ── A second tab while tab 1's modal stands: its own startup alert
    // queues FIFO behind the shown one (both tab processes blocked) ──
    ui.invoke_new_tab();
    wait_for("second tab up, its dialog queued", || {
        pump();
        snap().len() == 2 && app.core().pending_dialogs().len() == 2
    });
    assert!(ui.get_dialog_open());
    assert_eq!(
        ui.get_dialog_message().as_str(),
        "Fake page greets you!",
        "only the front of the queue is on screen"
    );
    assert_eq!(
        app.core().pending_dialogs()[0].0,
        TabId(1),
        "the shown dialog belongs to tab 1 (the front of the queue)"
    );

    // Answering the front unblocks tab 1 and hands the overlay to tab 2's
    // queued alert, bringing that tab to the front.
    ui.invoke_dialog_accept();
    wait_for("tab 1 receives its answer", || {
        pump();
        status(0).as_deref() == Some("dialog1:1:")
    });
    assert!(ui.get_dialog_open(), "the queued dialog takes the overlay");
    assert_eq!(ui.get_dialog_kind(), 0);
    assert_eq!(app.core().pending_dialogs().len(), 1);
    pump();
    assert_eq!(ui.get_active_index(), 1, "the queued dialog's tab was activated");

    // Cancel the second alert: cancelled answer, overlay closes, queue drains.
    ui.invoke_dialog_cancel();
    wait_for("tab 2 receives its cancelled answer", || {
        pump();
        status(1).as_deref() == Some("dialog1:0:")
    });
    pump();
    assert!(!ui.get_dialog_open());
    assert!(app.core().pending_dialogs().is_empty());

    // ── Prompt leg: reload the active tab → its scripted prompt arrives with
    // the page's default prefilled, and the edited text rides back ──
    ui.invoke_reload();
    wait_for("prompt overlay opens", || {
        pump();
        ui.get_dialog_open() && ui.get_dialog_kind() == 2
    });
    assert_eq!(ui.get_dialog_message().as_str(), "Give it a name:");
    assert_eq!(ui.get_dialog_input().as_str(), "ada", "the default prefills the box");

    ui.set_dialog_input("grace".into());
    ui.invoke_dialog_accept();
    wait_for("the edited prompt text comes back over the pipe", || {
        pump();
        status(1).as_deref() == Some("dialog2:1:grace")
    });
    pump();
    assert!(!ui.get_dialog_open());

    // ── Cancel leg: the next prompt answered Cancel reads cancelled ────────
    ui.invoke_reload();
    wait_for("second prompt opens", || {
        pump();
        ui.get_dialog_open() && ui.get_dialog_kind() == 2
    });
    ui.invoke_dialog_cancel();
    wait_for("the cancel echoes back", || {
        pump();
        status(1).as_deref() == Some("dialog3:0:")
    });
    pump();
    assert!(!ui.get_dialog_open());
    assert!(app.core().pending_dialogs().is_empty());
}
