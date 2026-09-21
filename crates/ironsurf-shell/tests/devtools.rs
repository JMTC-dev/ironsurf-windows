//! DevTools panel, end to end on the testing backend: the scripted fake tab
//! (`--devtools-feed`) streams `FromTab::ConsoleLog` / `FromTab::NetworkEvent`
//! messages, the shell reduces them into the per-tab logs and renders them as
//! the panel's rows, F12 / Ctrl-`I` flip the panel for the active tab only, and
//! switching tabs switches the pane to the other tab's feed.
//!
//! Headless (`i-slint-backend-testing`): no X, no engine. The GUI smoke run
//! stays gated behind `$DISPLAY` + `IRONSURF_SMOKE=1` (see `tests/real_tab.rs`).

use std::path::PathBuf;
use std::time::{Duration, Instant};

use ironsurf_shell::app::{App, DevToolsView};
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

/// The console table exactly as the panel renders it.
fn console_rows(ui: &AppWindow) -> Vec<(String, String)> {
    let rows = ui.get_console_rows();
    (0..rows.row_count())
        .map(|i| {
            let r = rows.row_data(i).unwrap();
            (r.level.to_string(), r.text.to_string())
        })
        .collect()
}

/// The network table exactly as the panel renders it (all columns pre-formatted).
#[allow(clippy::type_complexity)]
fn net_rows(ui: &AppWindow) -> Vec<(String, String, String, String, String, String, bool)> {
    let rows = ui.get_net_rows();
    (0..rows.row_count())
        .map(|i| {
            let r = rows.row_data(i).unwrap();
            (
                r.method.to_string(),
                r.url.to_string(),
                r.initiator.to_string(),
                r.status.to_string(),
                r.size.to_string(),
                r.time.to_string(),
                r.failed,
            )
        })
        .collect()
}

/// F12, exactly as Slint hands it to the page's `FocusScope`.
fn press_f12(ui: &AppWindow) {
    ui.invoke_page_key(true, "\u{F70F}".into(), false, false, false, false, false);
}

/// Ctrl-`I`.
fn press_ctrl_i(ui: &AppWindow) {
    ui.invoke_page_key(true, "i".into(), true, false, false, false, false);
}

#[test]
fn devtools_panel_fills_reduces_and_toggles_per_tab() {
    i_slint_backend_testing::init_no_event_loop();

    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("IRONSURF_DATA_DIR", dir.path());

    let ui = AppWindow::new().expect("window on testing backend");
    let fake = PathBuf::from(env!("CARGO_BIN_EXE_fake_tab"));
    let app = App::new(&ui, fake, vec!["--devtools-feed".to_string()]).expect("app core");
    app.start(None);
    let core = app.core();
    let pump = || core.drain();
    let snap = || core.tab_snapshot();

    // The fake tab's first feed batch must reduce into the tab's logs — and
    // the panel's tables are filled along the way, so opening the panel is
    // never a blank page for a tab that has been talking all along.
    wait_for("console + network feed reduced", || {
        pump();
        let s = snap();
        !s.is_empty() && s[0].ready && s[0].console.len() >= 2 && s[0].network.len() >= 2
    });
    let state = snap().remove(0);
    assert_eq!(state.console[0].source, "js");
    assert_eq!(state.network[1].error.as_deref(), Some("dns failure"));

    // The panel is closed until asked for, and belongs to no view yet.
    pump();
    assert!(!ui.get_devtools_open());
    assert_eq!(core.devtools_state(), vec![(false, DevToolsView::Console)]);

    // ---- F12 opens it for the active tab, with the reduced rows on screen --
    press_f12(&ui);
    pump();
    assert!(ui.get_devtools_open(), "F12 must open the panel");
    assert_eq!(ui.get_devtools_view(), 0, "Console is the default pane");
    assert_eq!(
        console_rows(&ui),
        vec![
            ("log".into(), "[js] fake tab awake".into()),
            ("warn".into(), "[css] unsupported property zoom".into()),
        ],
        "the console table is `[source] message`, level carried through"
    );
    assert_eq!(
        net_rows(&ui),
        vec![
            (
                "GET".into(),
                "https://example.com/".into(),
                "document".into(),
                "200".into(),
                "1.2 kB".into(),
                "42 ms".into(),
                false
            ),
            (
                "POST".into(),
                "https://api.invalid/upload".into(),
                "xhr".into(),
                // a failure names itself in the status column, in red
                "dns failure".into(),
                "\u{2014}".into(),
                "7 ms".into(),
                true
            ),
        ],
        "missing values render as an em dash"
    );
    assert_eq!(ui.get_console_count(), 2);
    assert_eq!(ui.get_network_count(), 2);

    // ---- the drain tick keeps the open panel current ----------------------
    // Reload makes the fake tab emit its second batch (one console line, one
    // redirect); nothing else needs to happen for the tables to grow.
    ui.invoke_reload();
    wait_for("second batch in the panel", || {
        pump();
        ui.get_console_count() == 3 && ui.get_network_count() == 3
    });
    let rows = console_rows(&ui);
    assert_eq!(rows.last().unwrap(), &("error".into(), "[js] reloaded and upset".into()));
    let net = net_rows(&ui);
    assert_eq!(
        net.last().unwrap(),
        &(
            "GET".into(),
            "https://example.com/next".into(),
            "document".into(),
            "301".into(),
            // 0 bytes renders as a count, not as "no value"
            "0 B".into(),
            "1.50 s".into(),
            false
        )
    );

    // ---- the pane choice is the tab's too ---------------------------------
    ui.invoke_devtools_select_view(1);
    pump();
    assert_eq!(ui.get_devtools_view(), 1);
    assert_eq!(core.devtools_state(), vec![(true, DevToolsView::Network)]);

    // ---- Ctrl-I closes it (the other half of the same chord) --------------
    press_ctrl_i(&ui);
    pump();
    assert!(!ui.get_devtools_open());
    // …and F12 brings the same tab's panel back, in the pane it remembered.
    press_f12(&ui);
    pump();
    assert!(ui.get_devtools_open());
    assert_eq!(ui.get_devtools_view(), 1, "the tab keeps its pane choice");

    // ---- per-tab flag: a new tab starts closed, showing *its* feed ---------
    // (the fake tab streams the same scripted batch in every spawn, which makes
    // the counts a clean marker of *which* tab the panel is rendering)
    ui.invoke_new_tab();
    wait_for("second tab ready with its own feed", || {
        pump();
        snap().len() == 2 && snap()[1].ready && snap()[1].console.len() == 2
    });
    pump();
    assert_eq!(snap().len(), 2);
    assert!(
        !ui.get_devtools_open(),
        "DevTools is per tab: the freshly opened tab has its own closed panel"
    );
    assert_eq!(
        (ui.get_console_count(), ui.get_network_count()),
        (2, 2),
        "the tables follow the active tab, not the one that was open before"
    );
    assert_eq!(core.devtools_state(), vec![(true, DevToolsView::Network), (false, DevToolsView::Console)]);

    // …and the new tab opens its own panel, in its own pane.
    press_f12(&ui);
    pump();
    assert!(ui.get_devtools_open());
    assert_eq!(core.devtools_state()[1], (true, DevToolsView::Console));

    // ---- switching tabs switches the view to that tab's feed --------------
    ui.invoke_activate_tab(0);
    wait_for("first tab's feed back on screen", || {
        pump();
        ui.get_devtools_open() && ui.get_console_count() == 3 && ui.get_network_count() == 3
    });
    assert_eq!(ui.get_devtools_view(), 1, "tab 0 had picked the network pane");
    assert_eq!(console_rows(&ui).len(), 3);

    // The "✕" row closes it for the active tab alone.
    ui.invoke_devtools_close();
    pump();
    assert!(!ui.get_devtools_open());
    assert_eq!(
        core.devtools_state(),
        vec![(false, DevToolsView::Network), (true, DevToolsView::Console)],
        "closing one tab's panel leaves the other's open"
    );

    app.shutdown();
}
