//! Copy in the DevTools tabs, shell side, headless (`i-slint-backend-testing`).
//!
//! The gestures the panel offers and what each one owes the clipboard:
//!
//! * one click on a Console (pane 0) or Network (pane 1) row **highlights**
//!   that row — the highlight rides the row model, the panel's `Copy` button
//!   is enabled only while the pane has one, and copying takes exactly the
//!   highlighted row (never a neighbour a moved feed left under the old
//!   index);
//! * a double-click highlights *and* copies in one gesture;
//! * on the Inspector (pane 2, `index` = node id) a double-click selects the
//!   node and copies its subtree's HTML, and the header `Copy` copies the
//!   selected node whenever a tree is up;
//! * `Clear` empties a feed and its highlight with it.
//!
//! Every copy lands in the session clipboard, read here through
//! `Core::session_clipboard`; the OS clipboard is opted out of with
//! `IRONSURF_NO_OS_CLIPBOARD` like the other clipboard tests.

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

/// The data dir and the clipboard opt-out are process-global env, so the
/// tests here run one at a time (as in `tests/clipboard_shortcuts.rs`).
static COPY_ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn harness(args: Vec<String>) -> (tempfile::TempDir, AppWindow, App) {
    i_slint_backend_testing::init_no_event_loop();
    std::env::set_var("IRONSURF_NO_OS_CLIPBOARD", "1");
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("IRONSURF_DATA_DIR", dir.path());
    let ui = AppWindow::new().expect("window on testing backend");
    let fake = PathBuf::from(env!("CARGO_BIN_EXE_fake_tab"));
    let app = App::new(&ui, fake, args).expect("app core");
    (dir, ui, app)
}

/// F12, exactly as Slint hands it to the page's `FocusScope`.
fn press_f12(ui: &AppWindow) {
    ui.invoke_page_key(true, "\u{F70F}".into(), false, false, false, false, false);
}

/// Which console rows the panel paints highlighted.
fn console_selected(ui: &AppWindow) -> Vec<usize> {
    let rows = ui.get_console_rows();
    (0..rows.row_count())
        .filter(|i| rows.row_data(*i).unwrap().selected)
        .collect()
}

/// Which network rows the panel paints highlighted.
fn net_selected(ui: &AppWindow) -> Vec<usize> {
    let rows = ui.get_net_rows();
    (0..rows.row_count())
        .filter(|i| rows.row_data(*i).unwrap().selected)
        .collect()
}

/// Which Inspector node the panel paints selected.
fn inspector_selected(ui: &AppWindow) -> Option<i32> {
    let rows = ui.get_inspector_rows();
    (0..rows.row_count())
        .find(|i| rows.row_data(*i).unwrap().selected)
        .map(|i| rows.row_data(i).unwrap().node)
}

#[test]
fn rows_highlight_on_click_and_copy_the_highlighted_row() {
    let _guard = COPY_ENV.lock().unwrap_or_else(|p| p.into_inner());
    let (_dir, ui, app) = harness(vec!["--devtools-feed".to_string()]);
    app.start(None);
    let core = app.core();
    let pump = || core.drain();

    // The fake tab's scripted feed; the panel opens onto it with F12.
    wait_for("feed reduced", || {
        pump();
        let s = core.tab_snapshot();
        !s.is_empty() && s[0].ready && s[0].console.len() >= 2 && s[0].network.len() >= 2
    });
    press_f12(&ui);
    wait_for("panel rows", || {
        pump();
        ui.get_console_rows().row_count() >= 2
    });

    // Nothing is highlighted before the first click, so `Copy` sits dark.
    assert!(console_selected(&ui).is_empty());
    assert!(!ui.get_devtools_copy_available(), "no highlight — no copy");

    // One click marks the row; the mark paints and enables the button.
    ui.invoke_devtools_select_row(0, 1);
    pump();
    assert_eq!(console_selected(&ui), vec![1], "the clicked row paints highlighted");
    assert!(ui.get_devtools_copy_available());

    // `Copy` takes exactly the highlighted line, whole.
    ui.invoke_devtools_copy();
    assert_eq!(core.session_clipboard(), "[css] unsupported property zoom");

    // A double-click highlights and copies in the same gesture.
    ui.invoke_devtools_copy_row(0, 0);
    pump();
    assert_eq!(console_selected(&ui), vec![0]);
    assert_eq!(core.session_clipboard(), "[js] fake tab awake");

    // An index the feed does not have (a stale or bogus click) is dropped,
    // never guessed onto a row that happens to sit there now.
    ui.invoke_devtools_select_row(0, 99);
    pump();
    assert_eq!(console_selected(&ui), vec![0], "a bad index must not move the highlight");

    // Each pane keeps its own highlight: switching to Network starts dark.
    ui.invoke_devtools_select_view(1);
    pump();
    assert!(!ui.get_devtools_copy_available(), "the Network pane has nothing marked yet");
    ui.invoke_devtools_select_row(1, 0);
    pump();
    assert_eq!(net_selected(&ui), vec![0]);
    ui.invoke_devtools_copy();
    assert_eq!(
        core.session_clipboard(),
        "GET https://example.com/ document 200 1.2 kB 42 ms",
        "a Network copy is its columns joined, in display order"
    );

    // `Clear` empties the feed and its highlight with it.
    ui.invoke_devtools_clear();
    pump();
    assert_eq!(ui.get_net_rows().row_count(), 0);
    assert!(net_selected(&ui).is_empty());
    assert!(!ui.get_devtools_copy_available(), "an emptied feed has nothing to copy");
}

#[test]
fn inspector_rows_copy_their_elements_html() {
    let _guard = COPY_ENV.lock().unwrap_or_else(|p| p.into_inner());
    let (_dir, ui, app) = harness(Vec::new());
    app.start(None);
    let core = app.core();
    let pump = || core.drain();

    wait_for("tab ready", || {
        pump();
        core.tab_snapshot().first().is_some_and(|s| s.ready)
    });
    press_f12(&ui);
    pump();
    ui.invoke_devtools_select_view(2);
    wait_for("inspector tree", || {
        pump();
        ui.get_inspector_rows().row_count() > 0
    });
    let base = ui.get_inspector_rows().row_data(0).unwrap().node;

    // Selecting a node is all the Inspector needs: its `Copy` is the selected
    // node's subtree, indented markup, closing tag and all.
    ui.invoke_devtools_select_node(base + 5); // <body id="main">
    pump();
    assert!(ui.get_devtools_copy_available(), "a tree with a selection always has a copy");
    ui.invoke_devtools_copy();
    assert_eq!(
        core.session_clipboard(),
        "<body id=\"main\">\n  <p class=\"note\">\n    fake body text\n  </p>\n</body>\n"
    );

    // A double-click on the `p` row copies that element alone — and leaves
    // that row selected, so the highlight matches what was copied.
    ui.invoke_devtools_copy_row(2, base + 6);
    pump();
    assert_eq!(core.session_clipboard(), "<p class=\"note\">\n  fake body text\n</p>\n");
    assert_eq!(inspector_selected(&ui), Some(base + 6));
}
