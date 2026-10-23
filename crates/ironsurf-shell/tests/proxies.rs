//! Per-container proxies, end to end on the testing backend (task: proxies).
//!
//! Modeled exactly on `tests/user_agent.rs`: the scripted fake tab
//! (`--expect-proxy`) echoes its *current* route on the status line — the
//! launch `--proxy` at startup and every inbound `ToTab::SetProxy` afterwards
//! (`proxy:<url>`, `proxy:direct` for `None`) — so the whole flow is
//! assertable over the real pipe: the proxy section lives in the settings
//! drawer (☰ — no toolbar button of its own), the drawer never stacks with
//! the other full-window overlays, and opening it re-pushes the proxy
//! state; a definition pick
//! persists the **name** to the Store and broadcasts the **resolved URL** to
//! the container's own tabs only (two containers, isolation proven per tab);
//! deleting a definition strands its containers (name kept, traffic direct)
//! and re-adding it under the same name reconnects them; a bad Add shows the
//! one-line error with both drafts kept and changes nothing; and a **newly
//! spawned** tab of a proxied container launches with `--proxy` and reports
//! the URL from its very first line. Headless (`i-slint-backend-testing`).

use std::path::PathBuf;
use std::time::{Duration, Instant};

use ironsurf_shell::AppWindow;
use ironsurf_shell::app::App;
use ironsurf_shell::uisettings::UiSettings;
use ironsurf_common::ProxyDef;
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

/// The proxy menu exactly as the overlay renders it: (name, url, active).
fn proxy_rows(ui: &AppWindow) -> Vec<(String, String, bool)> {
    let rows = ui.get_proxy_rows();
    (0..rows.row_count())
        .map(|i| {
            let r = rows.row_data(i).unwrap();
            (r.name.to_string(), r.url.to_string(), r.active)
        })
        .collect()
}

/// The status line — the fake tab's `proxy:<value>` echo lands here.
fn status(ui: &AppWindow) -> String {
    ui.get_status_text().to_string()
}

#[test]
fn proxy_menu_switches_proxies_per_container() {
    i_slint_backend_testing::init_no_event_loop();

    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("IRONSURF_DATA_DIR", dir.path());

    // Seed a second container ("work", id 2) and the global definition pool
    // BEFORE the shell opens its Store and loads ui.json, so the menu has
    // two containers to isolate between and two definitions to pick.
    const SOCKS_URL: &str = "socks5://127.0.0.1:1080";
    const WEB_URL: &str = "http://user:pass@proxy.internal:3128";
    {
        let mut seed = ironsurf_storage::Store::open(dir.path()).unwrap();
        let work = seed.create_container("work", "#4a90d9").unwrap();
        assert_eq!(work, ironsurf_common::ContainerId(2));
        let settings = UiSettings {
            proxies: vec![
                ProxyDef { name: "socks".into(), url: SOCKS_URL.into() },
                ProxyDef { name: "web".into(), url: WEB_URL.into() },
            ],
            ..Default::default()
        };
        settings.save(&dir.path().join("ui.json")).unwrap();
    }
    let disk = || ironsurf_storage::Store::open(dir.path()).unwrap();
    let disk_proxy = |id: u32| {
        disk()
            .containers()
            .into_iter()
            .find(|c| c.id.0 == id)
            .and_then(|c| c.proxy.clone())
    };
    let disk_defs = || {
        UiSettings::load(&dir.path().join("ui.json"))
            .proxies
            .iter()
            .map(|d| (d.name.clone(), d.url.clone()))
            .collect::<Vec<_>>()
    };

    let ui = AppWindow::new().expect("window on testing backend");
    let fake = PathBuf::from(env!("CARGO_BIN_EXE_fake_tab"));
    let app =
        App::new(&ui, fake.clone(), vec!["--expect-proxy".to_string()]).expect("app core");
    app.start(None);
    let core = app.core();
    let pump = || core.drain();

    // ── Tab 1, default container, no proxy ⇒ proxy:direct from launch ────────
    wait_for("tab ready, launched without a proxy ⇒ proxy:direct", || {
        pump();
        core.tab_snapshot().first().is_some_and(|s| s.ready) && status(&ui) == "proxy:direct"
    });

    // ── The settings drawer (☰) hosts the proxy section ──────────────────────────────
    assert!(!ui.get_history_menu_open());
    ui.invoke_toggle_history_menu();
    assert!(ui.get_history_menu_open());
    let rows = proxy_rows(&ui);
    assert_eq!(rows.len(), 3, "Direct + the two seeded definitions");
    assert_eq!(rows[0], ("Direct".to_string(), String::new(), true));
    assert_eq!(rows[1], ("socks".to_string(), SOCKS_URL.to_string(), false));
    assert_eq!(rows[2], ("web".to_string(), WEB_URL.to_string(), false));
    assert_eq!(ui.get_proxy_current().as_str(), "Direct");
    ui.invoke_toggle_history_menu();
    assert!(!ui.get_history_menu_open());
    ui.invoke_toggle_history_menu();

    // ── The full-window overlays never stack: each open closes the others ────
    ui.invoke_open_manager();
    assert!(ui.get_manager_open());
    assert!(!ui.get_history_menu_open(), "the manager closes the settings drawer");
    ui.invoke_toggle_ua_menu();
    assert!(ui.get_ua_menu_open());
    assert!(!ui.get_manager_open(), "the UA menu closes the manager");
    ui.invoke_toggle_history_menu();
    assert!(ui.get_history_menu_open());
    assert!(
        !ui.get_manager_open() && !ui.get_ua_menu_open(),
        "opening the settings drawer closes both the UA menu and the manager"
    );

    // ── Open a tab in container 2; the (still open) menu follows the tab ─────
    ui.invoke_mgr_new_tab_in(1);
    wait_for("the second tab (container 2) is ready", || {
        pump();
        core.tab_snapshot().len() == 2 && core.tab_snapshot()[1].ready
    });
    assert!(ui.get_history_menu_open(), "the drawer survives the tab switch");
    assert!(proxy_rows(&ui)[0].2, "container 2 has no proxy yet");
    assert_eq!(ui.get_proxy_current().as_str(), "Direct");

    // ── Pick "socks" (row 1): the NAME persists, the URL rides the wire ─────
    ui.invoke_proxy_select(1);
    assert_eq!(disk_proxy(2).as_deref(), Some("socks"), "the definition name is persisted");
    assert_eq!(disk_proxy(1), None, "the other container is untouched");
    wait_for("the container-2 tab echoes the resolved URL", || {
        pump();
        status(&ui) == format!("proxy:{SOCKS_URL}")
    });
    assert_eq!(ui.get_proxy_current().as_str(), "socks");
    let rows = proxy_rows(&ui);
    assert!(rows[1].2 && !rows[0].2, "the chosen row is highlighted, not Direct");

    // Let any (hypothetical) cross-container leak arrive, then prove
    // container 1's tab never got it: isolation.
    std::thread::sleep(Duration::from_millis(150));
    pump();
    ui.invoke_activate_tab(0);
    pump();
    assert_eq!(status(&ui), "proxy:direct", "container 1 never received the pick");
    assert_eq!(ui.get_proxy_current().as_str(), "Direct", "the menu re-pointed to container 1");
    assert!(proxy_rows(&ui)[0].2);

    // ── A NEW tab of the proxied container spawns with --proxy ───────────────
    ui.invoke_mgr_new_tab_in(1);
    wait_for("the spawned tab reports the launch proxy", || {
        pump();
        core.tab_snapshot().len() == 3 && status(&ui) == format!("proxy:{SOCKS_URL}")
    });

    // ── Direct clears the choice on the container and its live tabs ──────────
    ui.invoke_proxy_select(0);
    assert_eq!(disk_proxy(2), None, "Direct stores no name");
    wait_for("Direct rides the wire as None", || {
        pump();
        status(&ui) == "proxy:direct"
    });
    // …but only that container's tabs: put container 2 back on socks for the
    // delete/re-add leg, while container 1 stays direct.
    ui.invoke_proxy_select(1);
    wait_for("container 2 back on socks", || {
        pump();
        status(&ui) == format!("proxy:{SOCKS_URL}")
    });

    // ── Delete "socks": settings shrink, its containers go direct, the
    // stored name SURVIVES (re-adding it later reconnects them) ───────────────
    ui.invoke_proxy_delete(1);
    assert_eq!(
        disk_defs(),
        vec![("web".to_string(), WEB_URL.to_string())],
        "the definition is gone from ui.json"
    );
    assert_eq!(disk_proxy(2).as_deref(), Some("socks"), "the container keeps its name");
    wait_for("the dangling name re-resolves to direct", || {
        pump();
        status(&ui) == "proxy:direct"
    });
    assert!(
        proxy_rows(&ui).iter().all(|r| !r.2),
        "a dangling name highlights no row"
    );
    assert_eq!(ui.get_proxy_current().as_str(), "socks", "the label still names it");

    // ── Re-Add under the same name: every container that kept it reconnects ──
    let new_url = "http://[::1]:8888";
    ui.set_proxy_add_name("socks".into());
    ui.set_proxy_add_url(format!("  {new_url}  ").as_str().into()); // trimmed on success
    ui.invoke_proxy_add();
    assert!(ui.get_proxy_error().is_empty(), "a valid Add is accepted");
    assert_eq!(
        disk_defs(),
        vec![
            ("web".to_string(), WEB_URL.to_string()),
            ("socks".to_string(), new_url.to_string()),
        ]
    );
    wait_for("the re-added definition reconnects its containers", || {
        pump();
        status(&ui) == format!("proxy:{new_url}")
    });
    assert!(proxy_rows(&ui)[2].2, "the re-pushed rows mark it active again");
    assert!(ui.get_proxy_add_name().is_empty() && ui.get_proxy_add_url().is_empty(), "drafts cleared");

    // ── Rejections: duplicate name and a bad URL — one line, drafts kept,
    // nothing written, nothing broadcast ──────────────────────────────────────
    ui.set_proxy_add_name("socks".into());
    ui.set_proxy_add_url("http://elsewhere:1".into());
    ui.invoke_proxy_add();
    assert!(!ui.get_proxy_error().is_empty(), "the duplicate is rejected on the one line");
    assert_eq!(ui.get_proxy_add_name().to_string(), "socks", "the name draft survives");

    ui.set_proxy_add_name("ftp".into());
    ui.set_proxy_add_url("ftp://proxy:21".into());
    ui.invoke_proxy_add();
    assert!(ui.get_proxy_error().to_string().contains("socks5://"), "the scheme rule is spelled out");
    assert_eq!(ui.get_proxy_add_url().to_string(), "ftp://proxy:21", "the URL draft survives");
    assert_eq!(
        disk_defs().len(),
        2,
        "rejected Adds wrote nothing"
    );
    pump();
    std::thread::sleep(Duration::from_millis(150));
    pump();
    assert_eq!(status(&ui), format!("proxy:{new_url}"), "nothing was rebroadcast");

    ui.invoke_close_history_menu();
    assert!(!ui.get_history_menu_open(), "the scrim/✕ closes the drawer too");

    app.shutdown();
}
