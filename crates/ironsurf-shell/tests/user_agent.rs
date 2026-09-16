//! User-Agent switcher, end to end on the testing backend (task: per-container UA).
//!
//! Modeled exactly on `tests/userscripts.rs`: the scripted fake tab
//! (`--expect-user-agent`) echoes its *current* UA on the status line — the
//! launch `--user-agent` at startup and every inbound `ToTab::SetUserAgent`
//! afterwards (`ua:<value>`, `ua:default` for `None`) — so the whole menu
//! flow is assertable over the real pipe: the toolbar "UA" button toggles the
//! overlay and none of the three full-window overlays ever stacks; a preset
//! pick persists to the Store and broadcasts **to the container's own tabs
//! only** (two containers, isolation proven per tab); restore-default sends
//! `ua:default`; a custom Apply round-trips (trimmed) while an oversized one
//! (> `MAX_USER_AGENT_CHARS`) shows the StoreError on the one line and
//! changes nothing at all; and a **newly spawned** tab of a UA container
//! launches with `--user-agent` and reports the value from its very first
//! line. Headless (`i-slint-backend-testing`): no X, no engine.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use ironsurf_common::{BUILTIN_USER_AGENTS, MAX_USER_AGENT_CHARS};
use ironsurf_shell::AppWindow;
use ironsurf_shell::app::App;
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

/// The UA menu exactly as the overlay renders it: (label, ua, active).
fn ua_rows(ui: &AppWindow) -> Vec<(String, String, bool)> {
    let rows = ui.get_ua_rows();
    (0..rows.row_count())
        .map(|i| {
            let r = rows.row_data(i).unwrap();
            (r.label.to_string(), r.ua.to_string(), r.active)
        })
        .collect()
}

/// The status line — the fake tab's `ua:<value>` echo lands here.
fn status(ui: &AppWindow) -> String {
    ui.get_status_text().to_string()
}

#[test]
fn ua_menu_switches_user_agents_per_container() {
    i_slint_backend_testing::init_no_event_loop();

    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("IRONSURF_DATA_DIR", dir.path());

    // Seed a second container ("work", id 2) before the shell opens its own
    // Store, so the test has two containers to prove isolation between.
    {
        let mut seed = ironsurf_storage::Store::open(dir.path()).unwrap();
        let work = seed.create_container("work", "#4a90d9").unwrap();
        assert_eq!(work, ironsurf_common::ContainerId(2));
    }
    let disk = || ironsurf_storage::Store::open(dir.path()).unwrap();
    let disk_ua = |id: u32| {
        disk()
            .containers()
            .into_iter()
            .find(|c| c.id.0 == id)
            .and_then(|c| c.user_agent.clone())
    };

    let ui = AppWindow::new().expect("window on testing backend");
    let fake = PathBuf::from(env!("CARGO_BIN_EXE_fake_tab"));
    let app =
        App::new(&ui, fake.clone(), vec!["--expect-user-agent".to_string()]).expect("app core");
    app.start(None);
    let core = app.core();
    let pump = || core.drain();

    // ── Tab 1 spawns in the default container (1): no override ⇒ ua:default ──
    wait_for("tab ready, launched without a UA ⇒ ua:default", || {
        pump();
        core.tab_snapshot().first().is_some_and(|s| s.ready) && status(&ui) == "ua:default"
    });

    // ── The toolbar "UA" button toggles the overlay ──────────────────────────
    assert!(!ui.get_ua_menu_open());
    ui.invoke_toggle_ua_menu();
    assert!(ui.get_ua_menu_open());
    let rows = ua_rows(&ui);
    assert_eq!(rows.len(), 1 + BUILTIN_USER_AGENTS.len());
    assert_eq!(rows[0].0, "Default (IronSurf)");
    assert_eq!(rows[0].1, "", "the default row selects no string");
    assert!(rows[0].2, "no override ⇒ the default row is highlighted");
    assert_eq!(ui.get_ua_current().as_str(), "Default (IronSurf)");
    ui.invoke_toggle_ua_menu();
    assert!(!ui.get_ua_menu_open());
    ui.invoke_toggle_ua_menu();

    // ── The three overlays never stack: each open closes the others ──────────
    ui.invoke_open_manager();
    assert!(ui.get_manager_open());
    assert!(!ui.get_ua_menu_open(), "the manager closes the UA menu");
    ui.invoke_toggle_us_menu();
    assert!(ui.get_us_menu_open());
    assert!(!ui.get_manager_open(), "the userscript menu closes the manager");
    ui.invoke_toggle_ua_menu();
    assert!(ui.get_ua_menu_open());
    assert!(
        !ui.get_manager_open() && !ui.get_us_menu_open(),
        "opening the UA menu closes both the userscript menu and the manager"
    );

    // ── Open a tab in container 2; the (still open) menu follows the tab ─────
    ui.invoke_mgr_new_tab_in(1);
    wait_for("the second tab (container 2) is ready", || {
        pump();
        core.tab_snapshot().len() == 2 && core.tab_snapshot()[1].ready
    });
    assert!(ui.get_ua_menu_open(), "the menu survives the tab switch");
    assert!(ua_rows(&ui)[0].2, "container 2 has no override yet");
    assert_eq!(ui.get_ua_current().as_str(), "Default (IronSurf)");

    // ── Pick a preset (row 2 = Chrome): Store + broadcast, same container only ─
    let chrome = BUILTIN_USER_AGENTS[1].1;
    assert_eq!(ua_rows(&ui)[2].0, "Chrome");
    ui.invoke_ua_select(2);
    assert_eq!(disk_ua(2), Some(chrome.to_string()), "the pick is persisted at once");
    assert_eq!(disk_ua(1), None, "the other container is untouched");
    wait_for("the container-2 tab echoes the pick", || {
        pump();
        status(&ui) == format!("ua:{chrome}")
    });
    assert_eq!(ui.get_ua_current().as_str(), "Chrome");
    let rows = ua_rows(&ui);
    assert!(rows[2].2 && !rows[0].2, "the chosen row is highlighted, not the default");

    // Let any (hypothetical) cross-container leak arrive and be echoed first,
    // then show that container 1's tab still reports its own value: isolation.
    std::thread::sleep(Duration::from_millis(150));
    pump();
    ui.invoke_activate_tab(0);
    pump();
    assert_eq!(status(&ui), "ua:default", "container 1 never received the pick");
    assert_eq!(ui.get_ua_current().as_str(), "Default (IronSurf)", "the menu re-pointed to container 1");
    assert!(ua_rows(&ui)[0].2);

    // ── Back to container 2: "Restore default" ⇒ ua:default on the wire ──────
    ui.invoke_activate_tab(1);
    pump();
    assert_eq!(ui.get_ua_current().as_str(), "Chrome", "per-container menu state");
    ui.invoke_ua_restore_default();
    assert_eq!(disk_ua(2), None);
    wait_for("the restore broadcasts ua:default", || {
        pump();
        status(&ui) == "ua:default"
    });
    assert!(ua_rows(&ui)[0].2);

    // ── Custom Apply round-trips (trimmed); label says "Custom" ──────────────
    ui.set_ua_custom("  My Custom UA/9.9  ".into());
    let draft = ui.get_ua_custom().to_string();
    ui.invoke_ua_apply_custom(draft.as_str().into());
    assert_eq!(disk_ua(2).as_deref(), Some("My Custom UA/9.9"));
    wait_for("the custom string rides the wire", || {
        pump();
        status(&ui) == "ua:My Custom UA/9.9"
    });
    assert_eq!(ui.get_ua_current().as_str(), "Custom");
    assert!(ua_rows(&ui).iter().all(|r| !r.2), "a custom value highlights no row");

    // ── Oversized custom: one-line StoreError, nothing changed, draft kept ───
    let too_long = "u".repeat(MAX_USER_AGENT_CHARS + 88);
    ui.set_ua_custom(too_long.as_str().into());
    let typed = ui.get_ua_custom().to_string();
    ui.invoke_ua_apply_custom(typed.as_str().into());
    assert!(!ui.get_ua_error().is_empty(), "the StoreError shows on the one line");
    assert_eq!(disk_ua(2).as_deref(), Some("My Custom UA/9.9"), "the Store is untouched");
    assert_eq!(ui.get_ua_custom().to_string(), typed, "the rejected draft survives");
    pump();
    std::thread::sleep(Duration::from_millis(150));
    pump();
    assert_eq!(status(&ui), "ua:My Custom UA/9.9", "nothing was broadcast");

    // ── A NEW tab of a UA container spawns with --user-agent: its very first
    // echo (from the launch argument, before any wire message) is the UA ──────
    assert_eq!(ui.get_status_text().as_str(), "ua:My Custom UA/9.9");
    ui.invoke_new_tab(); // new_tab_container == 2 (set by mgr_new_tab_in)
    wait_for("the spawned tab reports the launch UA", || {
        pump();
        core.tab_snapshot().len() == 3 && status(&ui) == "ua:My Custom UA/9.9"
    });
    // The new tab is active, in container 2, and its startup echo arrived —
    // no SetUserAgent was ever sent to it (the menu broadcast predates it).

    ui.invoke_close_ua_menu();
    assert!(!ui.get_ua_menu_open(), "the scrim/✕ closes it too");

    app.shutdown();
}
