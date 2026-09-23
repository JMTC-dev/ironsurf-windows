//! Browsing history end to end (task: browsing history) — headless.
//!
//! One shared `history.jsonl` at the data-root, fed by every tab's
//! `UrlChanged`: visits from container 1 and container 2 land in the **same**
//! log (the seeded second container proves it is not per-profile), the ☰
//! toolbar button opens the overlay listing them newest-first, a row click
//! reopens that address in a new tab, "Clear history" deletes the log whole,
//! and the Ctrl-Alt-T chord reopens the last closed tab — its address
//! restored and recorded as a fresh visit. `about:blank` is plumbing, never
//! a visit. The log format, dedup, trimming, and clearing round trips are
//! covered by `ironsurf-storage/tests/history.rs`; this is the shell's flow.

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

/// A key chord from the page area, exactly as Slint delivers it.
fn press(ui: &AppWindow, text: &str, ctrl: bool, alt: bool) {
    ui.invoke_page_key(true, text.into(), ctrl, false, alt, false, false);
}

#[test]
fn history_is_shared_cleared_and_ctrl_alt_t_reopens_closed_tabs() {
    i_slint_backend_testing::init_no_event_loop();

    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("IRONSURF_DATA_DIR", dir.path());
    let log = ironsurf_storage::History::open(dir.path());
    let log_urls = || log.entries().into_iter().map(|e| e.url).collect::<Vec<_>>();

    // Seed a second container ("work", id 2) before the shell opens its own
    // Store, so the test can show visits from two containers in one log.
    {
        let mut seed = ironsurf_storage::Store::open(dir.path()).unwrap();
        let work = seed.create_container("work", "#4a90d9").unwrap();
        assert_eq!(work, ironsurf_common::ContainerId(2));
    }

    let ui = AppWindow::new().expect("window on testing backend");
    let fake = PathBuf::from(env!("CARGO_BIN_EXE_fake_tab"));
    let app = App::new(&ui, fake.clone(), vec![]).expect("app core");
    app.start(None);
    let core = app.core();
    let pump = || core.drain();
    let tabs = || core.tab_snapshot();

    // ── The internal start page is chrome, not a visit; the first real
    //    navigation opens the log ─────────────────────────────────────────────
    wait_for("the first tab is ready", || {
        pump();
        tabs().first().is_some_and(|t| t.ready)
    });
    assert!(log_urls().is_empty(), "ironsurf://welcome is not a visit");
    ui.invoke_navigate("example.org/one".into());
    wait_for("the visit is appended to the shared log", || {
        pump();
        log_urls() == vec!["https://example.org/one"]
    });

    // ── A tab in the OTHER container visits too: same file, no slice of its
    //    own (a freshly spawned blank tab must not count as a visit) ─────────
    ui.invoke_mgr_new_tab_in(1);
    wait_for("the container-2 tab is ready", || {
        pump();
        tabs().len() == 2 && tabs()[1].ready
    });
    assert_eq!(
        log_urls().first().map(|u| u.as_str()),
        Some("https://example.org/one"),
        "the new tab's start page is not a visit either"
    );
    ui.invoke_navigate("rustlang news".into());
    wait_for("the other container's visit lands in the same log", || {
        pump();
        log_urls()
            == vec![
                "https://html.duckduckgo.com/html/?q=rustlang+news",
                "https://example.org/one",
            ]
    });

    // ── The ☰ overlay: newest first, status counts the shared whole ──────────
    assert!(!ui.get_history_menu_open());
    ui.invoke_toggle_history_menu();
    assert!(ui.get_history_menu_open());
    let rows = |ui: &AppWindow| {
        let items = ui.get_history_items();
        (0..items.row_count())
            .map(|i| items.row_data(i).unwrap().url.to_string())
            .collect::<Vec<_>>()
    };
    assert_eq!(rows(&ui).len(), 2);
    assert_eq!(rows(&ui)[0], "https://html.duckduckgo.com/html/?q=rustlang+news");
    assert_eq!(ui.get_history_status().as_str(), "2 visits shared by every container");

    // ── Overlays never stack: each neighbour's open closes this one, and
    //    opening History closes all of them ──────────────────────────────────
    ui.invoke_open_manager();
    assert!(!ui.get_history_menu_open(), "the manager closes the history menu");
    ui.invoke_toggle_history_menu();
    assert!(ui.get_history_menu_open() && !ui.get_manager_open(), "and vice versa");

    // ── A row click opens that address in a new tab (menu closes) ────────────
    ui.invoke_history_pick(1); // "one", visited from container 1
    assert!(!ui.get_history_menu_open());
    wait_for("the picked address reopens in a new tab", || {
        pump();
        tabs().len() == 3 && tabs()[2].url.as_deref() == Some("https://example.org/one")
    });

    // ── Clear history: the whole shared log goes, menu re-reads as empty ─────
    ui.invoke_history_clear();
    assert!(!log.path().exists(), "the log file is removed");
    assert!(log_urls().is_empty());
    assert!(rows(&ui).is_empty(), "the open menu shows itself empty");
    assert_eq!(ui.get_history_status().as_str(), "0 visits shared by every container");
    ui.invoke_history_clear(); // clearing twice is fine
    assert!(log_urls().is_empty());

    // ── Ctrl-Alt-T: the last closed tab returns, container and all ───────────
    ui.invoke_close_tab(1); // the container-2 tab, last at https://…?q=rustlang+news
    wait_for("the tab is gone", || {
        pump();
        tabs().len() == 2
    });
    press(&ui, "t", true, true); // Ctrl-Alt-T
    wait_for("the closed tab is reopened with its address", || {
        pump();
        tabs().len() == 3
            && tabs()[2].url.as_deref()
                == Some("https://html.duckduckgo.com/html/?q=rustlang+news")
    });
    // Reopening is a real navigation: it lands in the (now cleared) log again.
    wait_for("the reopened visit is recorded", || {
        pump();
        log_urls() == vec!["https://html.duckduckgo.com/html/?q=rustlang+news"]
    });

    app.shutdown();
}
