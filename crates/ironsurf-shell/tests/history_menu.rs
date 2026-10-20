//! Browsing history end to end (task: browsing history) — headless.
//!
//! One shared `history.jsonl` at the data-root, fed by every tab's
//! `UrlChanged`: visits from container 1 and container 2 land in the **same**
//! log (the seeded second container proves it is not per-profile). The log is
//! read on a **page** — `ironsurf://history` — which the ☰ settings drawer's
//! History button opens; the drawer itself only counts the log in one line and
//! offers the buttons. The page asks the shell for the list (the file is out of
//! reach of the tab's sandbox) and is told again whenever the log moves; "Clear
//! history" deletes the log whole, and the Ctrl-Alt-T chord reopens the last
//! closed tab — its address restored and recorded as a fresh visit.
//! `about:blank` and `ironsurf://…` are plumbing, never a visit. The log format,
//! dedup, trimming, late titles and clearing round trips are covered by
//! `ironsurf-storage/tests/history.rs`; the page's own markup by
//! `ironsurf-tab`'s `netmod` tests; this is the shell's flow.

use std::path::PathBuf;
use std::time::{Duration, Instant};

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

    // ── The ☰ drawer: settings and a one-line count of the shared whole. The
    //    visits themselves are no longer rows in here — they are a page ───────
    assert!(!ui.get_history_menu_open());
    ui.invoke_toggle_history_menu();
    assert!(ui.get_history_menu_open());
    assert_eq!(
        ui.get_history_status().as_str(),
        "2 visits shared by every container",
        "the drawer counts the log it no longer lists"
    );

    // ── Overlays never stack: each neighbour's open closes this one, and
    //    opening History closes all of them ──────────────────────────────────
    ui.invoke_open_manager();
    assert!(!ui.get_history_menu_open(), "the manager closes the history menu");
    ui.invoke_toggle_history_menu();
    assert!(ui.get_history_menu_open() && !ui.get_manager_open(), "and vice versa");

    // ── The drawer's History button opens the visit log as a page, and puts
    //    the drawer away ─────────────────────────────────────────────────────
    ui.invoke_history_page();
    assert!(!ui.get_history_menu_open(), "the drawer steps aside for its page");
    wait_for("a tab opens on the history page", || {
        pump();
        tabs().last().is_some_and(|t| t.url.as_deref() == Some(ironsurf_common::HISTORY_URL))
    });
    assert_eq!(
        log_urls().len(),
        2,
        "opening the page is not a visit — the internal scheme is plumbing"
    );

    // The page cannot read the log itself (it lives at the data root, outside
    // the tab's sandbox), so it asks on the way in and the shell answers; the
    // fake tab prints the answer it was given as its status line:
    // "history:<rows in the log>:<rows shown>".
    wait_for("the shell answers the page's request for the log", || {
        pump();
        tabs().last().is_some_and(|t| t.status.as_deref() == Some("history:2:2"))
    });

    // ── Clear history: the whole shared log goes, the drawer re-reads as empty,
    //    and the open page is told the log moved under it ────────────────────
    ui.invoke_history_clear();
    assert!(!log.path().exists(), "the log file is removed");
    assert!(log_urls().is_empty());
    assert_eq!(ui.get_history_status().as_str(), "0 visits shared by every container");
    wait_for("the open history page sees the emptied log", || {
        pump();
        tabs().last().is_some_and(|t| t.status.as_deref() == Some("history:0:0"))
    });
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
