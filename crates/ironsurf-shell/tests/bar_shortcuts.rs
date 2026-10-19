//! Task: keep keyboard shortcuts usable while the URL bar is focused.
//!
//! With the URL bar focused its LineEdit sees keys first, so browser chords
//! used to die as editing noise. The bar (and the find bar) now offer every
//! modifier chord — and the modifierless function-key chords — to Rust before
//! their own editing swallows them (`url-chord` / `find-chord` in
//! `ironsurf.slint`, answered by `keymap::bar_shortcut`): Ctrl-T still makes
//! a tab, the digits still switch tabs, Ctrl-W still closes one, and F12
//! still opens DevTools. What the entry keeps: plain typing, caret motion,
//! its editing chords (Ctrl-A/C/V/X/Y/Z — none are in the table) and Esc,
//! which belongs to the bar (it owns the completion dropdown).
//!
//! Own test binary: `IRONSURF_DATA_DIR` is process-global, one `App` per
//! binary (see `dialog_crash.rs`). Headless (`i-slint-backend-testing`): no
//! X, no engine — the tabs are scripted `fake_tab` processes.

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

fn tab_count(ui: &AppWindow) -> usize {
    ui.get_tabs().row_count()
}

#[test]
fn browser_chords_survive_a_focused_url_bar() {
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
        tab_count(&ui) == 1
    });

    // Ctrl-T handed over by the URL bar: still a new tab, and it activates.
    assert!(
        ui.invoke_url_chord("t".into(), true, false, false, false),
        "the bar's Ctrl-T is the browser's"
    );
    wait_for("second tab", || {
        pump();
        tab_count(&ui) == 2
    });
    assert_eq!(ui.get_active_index(), 1, "a new tab activates itself");

    // The number chords work from the bar too: Alt-1 back to the first tab.
    assert!(ui.invoke_url_chord("1".into(), false, false, true, false));
    assert_eq!(ui.get_active_index(), 0, "Alt-1 from the bar → the first tab");
    assert!(ui.invoke_url_chord("2".into(), false, false, true, false));
    assert_eq!(ui.get_active_index(), 1, "Alt-2 from the bar → the second tab");

    // Ctrl-Tab cycles, again from the bar.
    assert!(ui.invoke_url_chord("\t".into(), true, false, false, false));
    assert_eq!(ui.get_active_index(), 0, "Ctrl-Tab from the bar cycles");

    // F12 needs no modifier at all and is still the browser's (DevTools
    // toggles; the panel itself is covered by devtools.rs — here: consumed).
    assert!(ui.invoke_url_chord("\u{F70F}".into(), false, false, false, false));

    // What the entry keeps: its editing chords and Esc answer false and touch
    // nothing — select-all, copy, paste stay mid-URL, and Esc never closes a
    // tab or drops page focus while the bar owns it.
    for (key, what) in
        [("a", "Ctrl-A select-all"), ("c", "Ctrl-C copy"), ("v", "Ctrl-V paste")]
    {
        assert!(
            !ui.invoke_url_chord(key.into(), true, false, false, false),
            "{what} stays with the entry"
        );
    }
    assert!(
        !ui.invoke_url_chord("\u{1b}".into(), false, false, false, false),
        "Esc stays the bar's own (completion dropdown)"
    );
    assert_eq!(tab_count(&ui), 2, "none of the kept keys opened or closed a tab");

    // Ctrl-W from the bar closes the active tab.
    assert!(ui.invoke_url_chord("w".into(), true, false, false, false));
    wait_for("one tab left", || {
        pump();
        tab_count(&ui) == 1
    });

    // The find bar mirrors all of it: Ctrl-T from there opens a tab, and Esc
    // stays local (it closes the find bar, which the bar itself handles).
    assert!(ui.invoke_find_chord("t".into(), true, false, false, false));
    wait_for("a tab opened from the find bar", || {
        pump();
        tab_count(&ui) == 2
    });
    assert!(!ui.invoke_find_chord("\u{1b}".into(), false, false, false, false));
    assert!(ui.invoke_find_chord("1".into(), false, false, true, false));
    assert_eq!(ui.get_active_index(), 0, "Alt-1 from the find bar switches tabs");
}
