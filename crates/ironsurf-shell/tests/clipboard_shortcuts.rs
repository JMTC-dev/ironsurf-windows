//! Keyboard copy & paste over the real pipe: Ctrl+C is never consumed by the
//! shell — it reaches the tab, whose engine (here: `--expect-copy`) answers a
//! copy with the selection as `FromTab::ClipboardText`, which the pump files
//! on the session clipboard. Ctrl+V is the mirror protocol: the shell pushes
//! `ToTab::Clipboard` **before** the keystroke, and the right-click menu picks
//! up exactly the same clipboard — its Paste row exists once the keyboard
//! copy has landed and pastes precisely what was copied.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use ironsurf_shell::app::App;
use ironsurf_shell::AppWindow;
use slint::Model;

/// The two tests in this binary share the process-global
/// `IRONSURF_NO_OS_CLIPBOARD` switch, so they run one at a time.
static CLIPBOARD_ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

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

fn labels(ui: &AppWindow) -> Vec<String> {
    (0..ui.get_menu_items().row_count())
        .map(|i| ui.get_menu_items().row_data(i).unwrap().label.to_string())
        .collect()
}

/// Right-button press+release at (x, y) through the page's pointer callback.
fn right_click(ui: &AppWindow) {
    ui.invoke_page_pointer(120.0, 60.0, 1, 1, false, false, false, false); // down, right
    ui.invoke_page_pointer(120.0, 60.0, 2, 1, false, false, false, false); // up, right
}

/// A page key press through the FocusScope callback.
fn page_key(ui: &AppWindow, text: &str, ctrl: bool) {
    ui.invoke_page_key(true, text.into(), ctrl, false, false, false, false);
}

#[test]
fn ctrl_c_then_the_menu_then_ctrl_v_round_trip_the_session_clipboard() {
    i_slint_backend_testing::init_no_event_loop();
    // Session-clipboard determinism (see the bridge test below): the desktop
    // clipboard must not feed into, or be fed by, these assertions.
    let _env = CLIPBOARD_ENV.lock().unwrap_or_else(|e| e.into_inner());
    std::env::set_var("IRONSURF_NO_OS_CLIPBOARD", "1");

    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("IRONSURF_DATA_DIR", dir.path());

    let ui = AppWindow::new().expect("window on testing backend");
    let fake = PathBuf::from(env!("CARGO_BIN_EXE_fake_tab"));
    let app =
        App::new(&ui, fake, vec!["--expect-copy".into()]).expect("app core with expect-copy tab");
    app.start(None);
    let pump = || app.core().drain();
    let snap = || app.core().tab_snapshot();
    wait_for("tab ready", || {
        pump();
        !snap().is_empty() && snap()[0].ready
    });

    // Before anything was copied this clipboard is empty ⇒ a right-click
    // offers no Paste row even over an editable target.
    right_click(&ui);
    wait_for("first menu open", || {
        pump();
        ui.get_menu_open()
    });
    let first = labels(&ui);
    assert!(
        !first.iter().any(|l| l == "Paste"),
        "empty clipboard ⇒ no Paste row: {first:?}"
    );
    ui.invoke_menu_close();
    pump();

    // Ctrl+C: forwarded untouched (no browser chord claims it) and answered
    // by the tab with its selection → the pump files the session clipboard.
    page_key(&ui, "c", true);

    // The very next menu proves the copy landed: the Paste row is there
    // without any Copy row having been picked.
    right_click(&ui);
    wait_for("menu with a Paste row after the keyboard copy", || {
        pump();
        ui.get_menu_open() && labels(&ui).iter().any(|l| l == "Paste")
    });

    // Picking Paste ships `ToTab::Clipboard` + `ContextAction::Paste`; the
    // fake echoes the paste itself last, with exactly the copied text.
    let paste_at = labels(&ui).iter().position(|l| l == "Paste").unwrap();
    ui.invoke_menu_pick(paste_at as i32);
    wait_for("the paste lands in the tab", || {
        pump();
        snap()[0].status.as_deref() == Some("clipboard:fake selection")
            || snap()[0].status.as_deref() == Some("pasted:fake selection")
    });
    wait_for("the paste action follows the cache push", || {
        pump();
        snap()[0].status.as_deref() == Some("pasted:fake selection")
    });

    // Ctrl+V: the shell pushes the session clipboard *ahead* of the chord
    // (the fake echoes the push), then forwards the keystroke itself — the
    // tab's editor path consumes it there.
    page_key(&ui, "v", true);
    wait_for("Ctrl+V pushes the clipboard before the keystroke", || {
        pump();
        snap()[0].status.as_deref() == Some("clipboard:fake selection")
    });
}

/// The OS bridge itself: the opt-out flag is honored, a missing display is
/// survivable, and — on a machine that *has* a desktop clipboard — a copy
/// really leaves through `set` and comes back through `get` (the manual
/// report this task answers: page copies must reach the system clipboard).
#[test]
fn the_os_bridge_honours_the_opt_out_and_round_trips_when_a_display_exists() {
    let _env = CLIPBOARD_ENV.lock().unwrap_or_else(|e| e.into_inner());
    use ironsurf_shell::clipboard::OsClipboard;

    // Disabled: no system access, every operation a safe no-op.
    std::env::set_var("IRONSURF_NO_OS_CLIPBOARD", "1");
    let mut off = OsClipboard::new();
    assert!(!off.active(), "opted out ⇒ no system clipboard");
    off.set("invisible");
    assert!(off.get().is_none());

    // Enabled: connection may fail (a headless CI has no display server) but
    // must never panic — and when it succeeds the text must really travel.
    std::env::set_var("IRONSURF_NO_OS_CLIPBOARD", "0");
    let mut on = OsClipboard::new();
    if on.active() {
        on.set("ironsurf os round trip");
        assert_eq!(on.get().as_deref(), Some("ironsurf os round trip"));
    } else {
        on.set("still fine");
        assert!(on.get().is_none());
    }
    std::env::set_var("IRONSURF_NO_OS_CLIPBOARD", "1");
}
