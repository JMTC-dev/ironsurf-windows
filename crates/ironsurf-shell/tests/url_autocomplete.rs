//! URL bar autocomplete end to end (task: URL bar autocomplete suggestions)
//! — headless on the testing backend.
//!
//! The shared visit log feeds a dropdown under the URL bar: typing a
//! fragment of a previously visited address opens it with matching rows
//! (newest visit first), ↑/↓ move a highlight WITHOUT touching the typed
//! text, Enter commits the highlighted row like a normal navigation, Esc
//! dismisses, a draft matching nothing closes the panel, and typing an
//! address out exactly closes it too — there is nothing left to complete.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use ironsurf_shell::app::App;
use ironsurf_shell::AppWindow;
use slint::ComponentHandle;
use slint::Model;

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

fn activate(ui: &AppWindow) {
    ui.window().dispatch_event(slint::platform::WindowEvent::WindowActiveChanged(true));
}

/// One key press delivered to whatever the window has focused — the URL bar
/// `LineEdit` after a focus chord. Special keys travel as Slint's private-use
/// characters (`slint::platform::Key`).
fn press_key(ui: &AppWindow, c: char) {
    ui.window().dispatch_event(slint::platform::WindowEvent::KeyPressed {
        text: c.to_string().into(),
    });
}

fn type_text(ui: &AppWindow, s: &str) {
    for c in s.chars() {
        press_key(ui, c);
    }
}

fn rows(ui: &AppWindow) -> Vec<(String, String)> {
    let items = ui.get_suggest_items();
    (0..items.row_count())
        .filter_map(|i| items.row_data(i).map(|r| (r.title.to_string(), r.url.to_string())))
        .collect()
}

#[test]
fn url_bar_autocomplete_lists_visits_and_commits_the_picked_row() {
    i_slint_backend_testing::init_no_event_loop();

    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("IRONSURF_DATA_DIR", dir.path());
    let log = ironsurf_storage::History::open(dir.path());

    let ui = AppWindow::new().expect("window on testing backend");
    let fake = PathBuf::from(env!("CARGO_BIN_EXE_fake_tab"));
    let app = App::new(&ui, fake, Vec::new()).expect("app core");
    app.start(None);
    let core = app.core();
    let pump = || core.drain();

    // ── Two real visits through the engine, then one appended straight to
    //    the log: the dropdown must read the SAME shared log the History menu
    //    shows, newest visit first ───────────────────────────────────────────
    wait_for("the first tab is ready", || {
        pump();
        core.tab_snapshot().first().is_some_and(|t| t.ready)
    });
    ui.invoke_navigate("example.org/golden-index".into());
    wait_for("first visit in the log", || {
        pump();
        log.entries().iter().any(|e| e.url == "https://example.org/golden-index")
    });
    ui.invoke_navigate("example.org/goldrate".into());
    wait_for("second visit in the log", || {
        pump();
        log.entries().iter().any(|e| e.url == "https://example.org/goldrate")
    });
    log.record("https://goldking.dev", Some("The Gold King")).unwrap();

    // ── Ctrl-L, type a fragment: the dropdown opens, matches ranked newest
    //    first (the just-typed draft itself never appears as a row) ──────────
    activate(&ui);
    ui.invoke_page_key(true, "\u{0c}".into(), true, false, false, false, false);
    assert!(ui.get_focus_url_token() > 0, "Ctrl-L focuses the bar");
    assert!(!ui.get_suggest_open(), "a fresh focus of the bar shows no dropdown");
    type_text(&ui, "gold");
    assert!(ui.get_suggest_open(), "typing a matching fragment opens the dropdown");
    assert_eq!(ui.get_suggest_selected(), 0, "the best match is highlighted at once");
    let urls: Vec<String> = rows(&ui).into_iter().map(|(_, u)| u).collect();
    assert_eq!(
        urls,
        vec![
            "https://goldking.dev".to_string(),
            "https://example.org/goldrate".to_string(),
            "https://example.org/golden-index".to_string(),
        ],
        "visits containing the draft, newest first"
    );

    // ── ↓ moves the highlight and NOTHING else: the draft is never rewritten
    //    behind the user and the arrow key never reaches the text ────────────
    press_key(&ui, slint::platform::Key::DownArrow.into());
    assert_eq!(ui.get_suggest_selected(), 1, "↓ steps down the list");
    assert_eq!(ui.get_url_text().as_str(), "gold", "↓ must not type into the bar");
    press_key(&ui, slint::platform::Key::UpArrow.into());
    press_key(&ui, slint::platform::Key::UpArrow.into());
    assert_eq!(
        ui.get_suggest_selected(),
        2,
        "↑ from the best match wraps to the last row"
    );

    // ── Enter commits the HIGHLIGHTED row — a real navigation, not the
    //    fragment the bar still shows ────────────────────────────────────────
    press_key(&ui, slint::platform::Key::Return.into());
    wait_for("the picked row navigates", || {
        pump();
        ui.get_url_text().as_str() == "https://example.org/golden-index"
    });
    assert!(!ui.get_suggest_open(), "navigation dismisses the dropdown");

    // ── Esc dismisses and leaves the draft alone ────────────────────────────
    ui.invoke_page_key(true, "\u{0c}".into(), true, false, false, false, false);
    type_text(&ui, "gold");
    assert!(ui.get_suggest_open(), "the dropdown is back for a new draft");
    press_key(&ui, slint::platform::Key::Escape.into());
    assert!(!ui.get_suggest_open(), "Esc closes the dropdown");
    assert_eq!(ui.get_url_text().as_str(), "gold", "Esc leaves the typed draft as is");

    // ── Nothing matches → no panel; the full address typed out → no panel ──
    type_text(&ui, "zzz");
    assert!(!ui.get_suggest_open(), "a draft matching nothing shows no dropdown");
    ui.invoke_page_key(true, "\u{0c}".into(), true, false, false, false, false);
    type_text(&ui, "https://goldking.dev");
    assert!(
        !ui.get_suggest_open(),
        "typing an address exactly leaves nothing to complete — the panel stays closed"
    );
    assert_eq!(ui.get_url_text().as_str(), "https://goldking.dev");
}
