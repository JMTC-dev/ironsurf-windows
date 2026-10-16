//! Popup controls on the shell side (task: block popups), end to end on the
//! testing backend with the scripted fake tab. The rules themselves live in the
//! real tab (`ironsurf-tab/src/popup.rs`, proven by `ironsurf-tab/tests/popups.rs`);
//! what is proven here is the shell half of the wire:
//!
//!   * a tab nobody touches is **never sent a policy** — the tab blocks on its
//!     own, so safety needs no UI (the fake echoes a policy only once one
//!     arrives);
//!   * ⛔ (`popup-block-toggled`) flips the master switch for that tab and the
//!     button state follows the switch;
//!   * Ctrl-Alt-`P` (`popup-allow-site`) sends the page's **own origin** —
//!     normalized, its port kept — with the switch still up; a page with no site
//!     origin sends nothing at all rather than a rule that would match every
//!     site;
//!   * a popup the tab refused shows in the status line ("blocked 1 popup —
//!     Ctrl-Alt-P allows this site") and the notice dies with the page that made
//!     it, while allowing the site puts it away at once.
//!
//! `IRONSURF_DATA_DIR` is process-global and each test wants its own directory,
//! so the three tests are serialized on one lock and each keeps its data dir
//! alive until it is finished.

use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use ironsurf_shell::AppWindow;
use ironsurf_shell::app::App;

/// One test at a time: the data dir is chosen by a process-wide variable.
fn serialized() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

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

/// The status line: where the fake tab's `popups:…` policy echo lands, and the
/// same line the popup notice appears on.
fn status(ui: &AppWindow) -> String {
    ui.get_status_text().to_string()
}

/// The shell's own record of the popup switch and the allow-list, as the pump
/// keeps it — what the next policy message will be built from.
fn policy(app: &App) -> (bool, Vec<String>) {
    let core = app.core();
    let tab = core.tab_snapshot().first().cloned().unwrap_or_default();
    (
        !tab.popup_block_off,
        tab.popup_allowed.clone(),
    )
}

/// `(data dir, window, app)` — the directory must outlive the app.
fn shell(extra: &[&str]) -> (tempfile::TempDir, AppWindow, App) {
    i_slint_backend_testing::init_no_event_loop();
    let dir = tempfile::tempdir().expect("data dir");
    std::env::set_var("IRONSURF_DATA_DIR", dir.path());
    let ui = AppWindow::new().expect("window on testing backend");
    let fake = PathBuf::from(env!("CARGO_BIN_EXE_fake_tab"));
    let app = App::new(&ui, fake, extra.iter().map(|s| s.to_string()).collect())
        .expect("app core");
    app.start(None);
    (dir, ui, app)
}

/// Wait for the fake tab's own page (the address bar follows the typed
/// navigation before the tab's record does, so this waits on the record).
fn wait_for_page(app: &App, host: &str) {
    let core = app.core();
    wait_for("the page's own address", || {
        core.drain();
        core.tab_snapshot()
            .first()
            .and_then(|t| t.url.clone())
            .is_some_and(|u| u.contains(host))
    });
}

#[test]
fn the_popup_switch_and_the_allow_list_reach_the_tab() {
    let _serialized = serialized();
    let (_dir, ui, app) = shell(&["--expect-popup-policy"]);
    let core = app.core();
    wait_for("tab ready", || {
        core.drain();
        core.tab_snapshot().first().is_some_and(|s| s.ready)
    });

    // A fresh tab: blocking stands and nothing has been said about it — the tab
    // blocks on its own, so a shell that never sends a policy is still safe.
    assert!(ui.get_popup_blocked(), "the ⛔ button starts pressed");
    assert_eq!(policy(&app), (true, vec![]));
    assert!(
        !status(&ui).starts_with("popups:"),
        "an untouched tab is never sent a policy: {}",
        status(&ui)
    );

    // Ctrl-Alt-P on a real page: its own origin, port and all, switch still up.
    ui.invoke_navigate("https://news.test:8443/top?a=1".into());
    wait_for_page(&app, "news.test");
    ui.invoke_popup_allow_site();
    wait_for("the origin reaches the tab", || {
        core.drain();
        status(&ui) == "popups:block:https://news.test:8443"
    });
    assert_eq!(
        policy(&app).1,
        vec!["https://news.test:8443".to_string()],
        "the shell remembers what it allowed, so the next policy keeps it"
    );

    // ⛔ releases this tab entirely; the button says which way it is, and the
    // allow-list travels with the switch instead of being dropped by it.
    ui.invoke_popup_block_toggled();
    wait_for("the switch off", || {
        core.drain();
        status(&ui) == "popups:allow:https://news.test:8443"
    });
    assert!(!ui.get_popup_blocked());
    ui.invoke_popup_block_toggled();
    wait_for("the switch back on", || {
        core.drain();
        status(&ui) == "popups:block:https://news.test:8443"
    });
    assert!(ui.get_popup_blocked());
}

#[test]
fn a_page_with_no_site_origin_has_nothing_to_allow() {
    let _serialized = serialized();
    let (_dir, ui, app) = shell(&["--expect-popup-policy"]);
    let core = app.core();
    wait_for("tab ready", || {
        core.drain();
        core.tab_snapshot().first().is_some_and(|s| s.ready)
    });
    // The fake's start page names no host: allowing must send NOTHING rather
    // than an empty rule, which the tab would read as allowing everything.
    ui.invoke_popup_allow_site();
    std::thread::sleep(Duration::from_millis(200));
    core.drain();
    assert!(
        !status(&ui).starts_with("popups:"),
        "no origin, no policy: {}",
        status(&ui)
    );
    assert!(policy(&app).1.is_empty());
}

#[test]
fn a_blocked_popup_shows_in_the_status_line_until_the_next_page() {
    let _serialized = serialized();
    let (_dir, ui, app) = shell(&["--popup-blocked"]);
    let core = app.core();
    wait_for("tab ready", || {
        core.drain();
        core.tab_snapshot().first().is_some_and(|s| s.ready)
    });
    assert!(
        !status(&ui).contains("popup"),
        "an unvisited page blocked nothing: {}",
        status(&ui)
    );

    // The scripted popup page: the tab refused the window and said so.
    ui.invoke_navigate("https://ads.test/popup-interstitial".into());
    wait_for("the notice", || {
        core.drain();
        status(&ui).contains("blocked 1 popup")
    });
    assert!(
        status(&ui).contains("Ctrl-Alt-P"),
        "the notice names the key that lets the site through: {}",
        status(&ui)
    );

    // Allowing the site puts the notice away — the user has acted on it.
    ui.invoke_popup_allow_site();
    wait_for("the notice gone after allowing", || {
        core.drain();
        !status(&ui).contains("blocked 1 popup")
    });

    // And a page that asks for nothing says nothing.
    ui.invoke_navigate("https://news.test/".into());
    wait_for("the next page", || {
        core.drain();
        status(&ui).contains("news.test") || !status(&ui).contains("popup")
    });
    assert!(
        !status(&ui).contains("popup"),
        "the notice belongs to the page that made it: {}",
        status(&ui)
    );
}
