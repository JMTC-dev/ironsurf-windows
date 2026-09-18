//! `Alt-1` … `Alt-9` switch to the N-th tab, and the mapping is **dynamic**:
//! which tab a digit lands on is resolved against the live tab list on every
//! press (see `keymap::tab_index_for_digit`), so closing a tab re-slots every
//! remaining shortcut and `Alt-9` follows the end of the strip as it moves.
//! The tab strip paints the digit that reaches each tab today, which is the
//! same mapping seen from the other side.
//!
//! Own test binary: `IRONSURF_DATA_DIR` is process-global, one `App` per
//! binary (see `dialog_crash.rs`). Headless (`i-slint-backend-testing`): no X,
//! no engine — the tabs are scripted `fake_tab` processes.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use ironsurf_shell::app::App;
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

/// One key press delivered as the page's `FocusScope` would deliver it.
fn press(ui: &AppWindow, text: &str, ctrl: bool, alt: bool) {
    ui.invoke_page_key(true, text.into(), ctrl, false, alt, false, false);
}

/// Ids of the tabs in the strip, in strip order.
fn tab_ids(ui: &AppWindow) -> Vec<i32> {
    let tabs = ui.get_tabs();
    (0..tabs.row_count())
        .filter_map(|i| tabs.row_data(i))
        .map(|t| t.id)
        .collect()
}

/// Alt-digits the strip shows, in strip order (empty = no chord reaches it).
fn badges(ui: &AppWindow) -> Vec<String> {
    let tabs = ui.get_tabs();
    (0..tabs.row_count())
        .filter_map(|i| tabs.row_data(i))
        .map(|t| t.shortcut.to_string())
        .collect()
}

#[test]
fn alt_digits_follow_the_tabs_as_they_open_and_close() {
    i_slint_backend_testing::init_no_event_loop();

    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("IRONSURF_DATA_DIR", dir.path());

    let ui = AppWindow::new().expect("window on testing backend");
    let fake = PathBuf::from(env!("CARGO_BIN_EXE_fake_tab"));
    let app = App::new(&ui, fake, Vec::new()).expect("app core");
    let pump = || app.core().drain();

    app.start(None);
    wait_for("first tab", || {
        pump();
        tab_ids(&ui).len() == 1
    });

    // Three tabs: 1 (first), then two more — the newest is the active one.
    for _ in 0..2 {
        ui.invoke_new_tab();
    }
    wait_for("three tabs in the strip", || {
        pump();
        tab_ids(&ui).len() == 3
    });
    assert_eq!(ui.get_active_index(), 2, "a new tab activates itself");
    assert_eq!(badges(&ui), vec!["1", "2", "3"], "one digit per tab");

    // Alt-N goes to the N-th tab.
    press(&ui, "1", false, true);
    assert_eq!(ui.get_active_index(), 0, "Alt-1 → the first tab");
    press(&ui, "3", false, true);
    assert_eq!(ui.get_active_index(), 2, "Alt-3 → the third tab");
    press(&ui, "2", false, true);
    assert_eq!(ui.get_active_index(), 1, "Alt-2 → the second tab");
    // Alt-9 reaches nothing while there are only three tabs: the active tab
    // stays put rather than guessing.
    press(&ui, "9", false, true);
    assert_eq!(ui.get_active_index(), 1, "Alt-9 with 3 tabs does nothing");
    // Unmodified digits and Ctrl-digits are nobody's tab chord.
    press(&ui, "1", false, false);
    press(&ui, "1", true, false);
    assert_eq!(ui.get_active_index(), 1, "only the Alt form switches tabs");

    // --- closing re-slots everything ---------------------------------------
    let before = tab_ids(&ui);
    let survivor_of_alt_3 = before[2];
    ui.invoke_close_tab(0);
    wait_for("two tabs left", || {
        pump();
        tab_ids(&ui).len() == 2
    });
    assert_eq!(badges(&ui), vec!["1", "2"], "the digits slide down");
    press(&ui, "2", false, true);
    let tabs = ui.get_tabs();
    assert_eq!(ui.get_active_index(), 1);
    assert_eq!(
        tabs.row_data(1).expect("second tab").id,
        survivor_of_alt_3,
        "what was Alt-3 answers Alt-2 once a tab in front of it closed"
    );

    // --- Alt-9 is "the last tab" and keeps chasing the end ------------------
    for _ in 0..6 {
        ui.invoke_new_tab();
    }
    wait_for("eight tabs", || {
        pump();
        tab_ids(&ui).len() == 8
    });
    press(&ui, "1", false, true);
    assert_eq!(ui.get_active_index(), 0, "back to the first tab");
    press(&ui, "9", false, true);
    assert_eq!(
        ui.get_active_index(),
        0,
        "Alt-9 still reaches nothing at eight tabs"
    );

    for _ in 0..3 {
        ui.invoke_new_tab();
    }
    wait_for("eleven tabs", || {
        pump();
        tab_ids(&ui).len() == 11
    });
    press(&ui, "2", false, true);
    assert_eq!(ui.get_active_index(), 1, "positioned away from the end");
    press(&ui, "9", false, true);
    assert_eq!(ui.get_active_index(), 10, "Alt-9 → the last tab from nine on");
    press(&ui, "8", false, true);
    assert_eq!(ui.get_active_index(), 7, "Alt-8 still means position 8");
    let strip = badges(&ui);
    assert_eq!(strip.len(), 11);
    assert_eq!(strip[7], "8");
    assert_eq!(strip[8], "", "position 9 has no chord of its own once Alt-9 means last");
    assert_eq!(strip[9], "");
    assert_eq!(strip[10], "9", "the last tab carries Alt-9");

    // Closing the last tab moves Alt-9 onto the new last tab.
    let ids = tab_ids(&ui);
    ui.invoke_close_tab(10);
    wait_for("ten tabs left", || {
        pump();
        tab_ids(&ui).len() == 10
    });
    assert_eq!(badges(&ui)[9], "9", "Alt-9 followed the end of the strip");
    press(&ui, "1", false, true);
    assert_eq!(ui.get_active_index(), 0);
    press(&ui, "9", false, true);
    assert_eq!(ui.get_active_index(), 9);
    assert_eq!(tab_ids(&ui)[9], ids[9], "the new last tab is the old second-to-last");

    ui.window().hide().unwrap();
}
