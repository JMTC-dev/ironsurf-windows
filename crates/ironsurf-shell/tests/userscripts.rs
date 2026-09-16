//! Userscript menu, end to end on the testing backend (task: userscript menu).
//!
//! The scripted fake tab (`--expect-userscripts`) echoes every inbound
//! `ToTab::SetUserscripts` on its status line with the ids it carries, so the
//! whole menu flow is assertable over the real pipe: the toolbar "</>" button
//! toggles the overlay, Save goes through the Store (a rejected script is not
//! added and shows a one-line error), every mutation — toggle / save / delete
//! — rebroadcasts the **enabled-only** set to every live tab, a freshly
//! opened tab is seeded with the current set, the container manager and the
//! menu never stack, and the toggled flag lands on disk (the storage round
//! trip itself is covered by `src/userscripts.rs` and ironsurf-storage).
//!
//! Headless (`i-slint-backend-testing`): no X, no engine.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use ironsurf_shell::app::App;
use ironsurf_shell::AppWindow;
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

/// The menu list exactly as the overlay renders it: (id, name, patterns, enabled).
fn rows(ui: &AppWindow) -> Vec<(i32, String, String, bool)> {
    let rows = ui.get_us_scripts();
    (0..rows.row_count())
        .map(|i| {
            let r = rows.row_data(i).unwrap();
            (r.id, r.name.to_string(), r.patterns.to_string(), r.enabled)
        })
        .collect()
}

/// The status line — the fake tab's `userscripts:[ids]` echo lands here.
fn status(ui: &AppWindow) -> String {
    ui.get_status_text().to_string()
}

#[test]
fn userscript_menu_persists_and_broadcasts_the_enabled_set() {
    i_slint_backend_testing::init_no_event_loop();

    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("IRONSURF_DATA_DIR", dir.path());

    let ui = AppWindow::new().expect("window on testing backend");
    let fake = PathBuf::from(env!("CARGO_BIN_EXE_fake_tab"));
    let app =
        App::new(&ui, fake.clone(), vec!["--expect-userscripts".to_string()]).expect("app core");
    app.start(None);
    let core = app.core();
    let pump = || core.drain();
    let scripts = || core.userscripts();

    // ── Spawn seed: the very first tab receives the (empty) enabled set ─────
    wait_for("tab ready and seeded with the empty set", || {
        pump();
        core.tab_snapshot().first().is_some_and(|s| s.ready) && status(&ui) == "userscripts:[]"
    });

    // ── The toolbar "</>" button toggles the overlay ─────────────────────────
    assert!(!ui.get_us_menu_open());
    ui.invoke_toggle_us_menu();
    assert!(ui.get_us_menu_open());
    assert!(rows(&ui).is_empty(), "a fresh profile has no scripts");
    ui.invoke_toggle_us_menu();
    assert!(!ui.get_us_menu_open());
    ui.invoke_toggle_us_menu();

    // ── Save leg: the inline editor → Store → wire ───────────────────────────
    ui.invoke_us_new();
    assert!(ui.get_us_editing(), "\"+ New script\" reveals the editor");
    ui.set_us_name("Darkify".into());
    ui.set_us_patterns("  *example.com, example.org/docs ,,  ".into());
    ui.set_us_code("document.title = 'dark';".into());
    ui.invoke_us_save();
    assert!(!ui.get_us_editing(), "an accepted Save closes the editor");
    let s = scripts();
    assert_eq!(s.len(), 1);
    assert_eq!(s[0].name, "Darkify");
    assert_eq!(s[0].patterns, vec!["*example.com", "example.org/docs"]);
    assert!(s[0].enabled, "a new script starts enabled");
    assert_eq!(core.userscript_row_ids(), vec![s[0].id]);
    wait_for("the save rebroadcasts to every live tab", || {
        pump();
        status(&ui) == format!("userscripts:[{}]", s[0].id)
    });
    assert_eq!(rows(&ui), vec![(s[0].id as i32, "Darkify".into(), "*example.com, example.org/docs".into(), true)]);

    // ── A rejected script is NOT added; the error rides on one line ──────────
    ui.invoke_us_new();
    ui.set_us_name("   ".into()); // the Store rejects an empty name
    ui.set_us_patterns("*".into());
    ui.set_us_code("void 0;".into());
    ui.invoke_us_save();
    assert!(ui.get_us_editing(), "a rejected Save keeps the editor and the draft");
    assert!(!ui.get_us_error().is_empty(), "the StoreError text shows in the menu");
    assert_eq!(scripts().len(), 1, "nothing was added");
    ui.invoke_us_cancel();
    assert!(!ui.get_us_editing());

    // ── A second script: the wire carries both, in storage order ─────────────
    ui.invoke_us_new();
    ui.set_us_name("Quiet".into());
    ui.set_us_patterns("*".into());
    ui.set_us_code("console.log(2);".into());
    ui.invoke_us_save();
    let s = scripts();
    assert_eq!(s.len(), 2);
    assert_eq!(s[1].id, s[0].id + 1, "ids come from next_userscript_id");
    wait_for("both scripts on the wire", || {
        pump();
        status(&ui) == format!("userscripts:[{},{}]", s[0].id, s[1].id)
    });

    // ── The CheckBox: persist at once, wire drops the disabled script ────────
    ui.invoke_us_toggle(0);
    assert!(!scripts()[0].enabled, "the toggle is stored immediately");
    assert!(scripts()[1].enabled);
    wait_for("the disabled script leaves the wire", || {
        pump();
        status(&ui) == format!("userscripts:[{}]", s[1].id)
    });
    assert!(!rows(&ui)[0].3, "the row mirrors the stored flag");

    // …and flipping the same box back puts it on the wire again.
    ui.invoke_us_toggle(0);
    assert!(scripts()[0].enabled);
    wait_for("re-enabling puts the script back on the wire", || {
        pump();
        status(&ui) == format!("userscripts:[{},{}]", s[0].id, s[1].id)
    });

    // ── Delete: row removed, wire follows ────────────────────────────────────
    ui.invoke_us_delete(1);
    let s = scripts();
    assert_eq!(s.len(), 1);
    assert_eq!(s[0].name, "Darkify");
    assert_eq!(rows(&ui).len(), 1);
    wait_for("the delete rebroadcasts the rest", || {
        pump();
        status(&ui) == format!("userscripts:[{}]", s[0].id)
    });

    // ── A freshly opened tab is seeded with the current enabled set ──────────
    ui.invoke_new_tab();
    wait_for("the new tab receives the seed", || {
        pump();
        core.tab_snapshot().len() == 2 && status(&ui) == format!("userscripts:[{}]", s[0].id)
    });

    // ── Menu and container manager never stack: each closes the other ────────
    assert!(ui.get_us_menu_open());
    ui.invoke_open_manager();
    assert!(ui.get_manager_open());
    assert!(!ui.get_us_menu_open(), "the manager closes the userscript menu");
    ui.invoke_toggle_us_menu();
    assert!(ui.get_us_menu_open());
    assert!(!ui.get_manager_open(), "and vice versa");
    ui.invoke_close_us_menu();
    assert!(!ui.get_us_menu_open(), "the scrim/✕ closes it too");

    // ── Toggling persists: the flag on disk matches (reopen is covered by the
    // `userscripts.rs` unit tests against a tempdir Store) ───────────────────
    ui.invoke_toggle_us_menu();
    ui.invoke_us_toggle(0);
    assert!(!scripts()[0].enabled);
    wait_for("the all-disabled store sends the empty set", || {
        pump();
        status(&ui) == "userscripts:[]"
    });
    let reopened = ironsurf_storage::Store::open(dir.path()).expect("reopen the store");
    assert_eq!(reopened.userscripts(), scripts(), "disk and memory agree");

    app.shutdown();
}
