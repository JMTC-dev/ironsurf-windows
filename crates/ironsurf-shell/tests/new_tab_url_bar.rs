//! Opening a new tab lands the user in the address bar (task: new tabs focus
//! the URL bar): Ctrl-T and the ＋ button both bump `focus-url-token` — the
//! very bump Ctrl-L / F6 produce — so the `UrlBar` grabs focus and highlights
//! the whole address, and the first keystroke replaces it.
//!
//! Own test binary: `IRONSURF_DATA_DIR` is process-global, one `App` per
//! binary (see `dialog_crash.rs`). Headless (`i-slint-backend-testing`): no X,
//! no engine — the tab is the scripted `fake_tab`.
//!
//! Also covered, because the feature depends on it: the freshly spawned tab
//! announces its own URL a beat later, and that announcement must **not** land
//! on top of the address being typed; Escape abandons the draft and shows the
//! tab's real URL again.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use ironsurf_shell::app::App;
use ironsurf_shell::AppWindow;
use slint::{ComponentHandle, Model};

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

/// Mark the window active, as the window manager would after mapping it.
fn activate(ui: &AppWindow) {
    ui.window().dispatch_event(slint::platform::WindowEvent::WindowActiveChanged(true));
}

/// A real left click in the page area: gives the page keyboard focus, exactly
/// like browsing before a new tab opens.
fn click_page(ui: &AppWindow) {
    let pos = slint::LogicalPosition::new(400.0, 300.0);
    let win = ui.window();
    win.dispatch_event(slint::platform::WindowEvent::PointerMoved { position: pos });
    win.dispatch_event(slint::platform::WindowEvent::PointerPressed {
        position: pos,
        button: slint::platform::PointerEventButton::Left,
    });
    win.dispatch_event(slint::platform::WindowEvent::PointerReleased {
        position: pos,
        button: slint::platform::PointerEventButton::Left,
    });
}

/// One printable key press delivered to whatever the window has focused — the
/// URL bar `LineEdit` after a new tab opened, with the address selected, so
/// the character replaces it.
fn type_char(ui: &AppWindow, c: char) {
    ui.window().dispatch_event(slint::platform::WindowEvent::KeyPressed {
        text: c.to_string().into(),
    });
}

/// Titles the shell pushed onto the tab strip, in order.
fn tab_titles(ui: &AppWindow) -> Vec<String> {
    let tabs = ui.get_tabs();
    (0..tabs.row_count())
        .filter_map(|i| tabs.row_data(i))
        .map(|t| t.title.to_string())
        .collect()
}

#[test]
fn a_new_tab_opens_with_the_url_bar_focused_and_selected() {
    i_slint_backend_testing::init_no_event_loop();

    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("IRONSURF_DATA_DIR", dir.path());

    let ui = AppWindow::new().expect("window on testing backend");
    let fake = PathBuf::from(env!("CARGO_BIN_EXE_fake_tab"));
    let app = App::new(&ui, fake, Vec::new()).expect("app core");
    let pump = || app.core().drain();

    app.start(None);
    wait_for("first tab in the bar", || {
        pump();
        ui.get_url_text().as_str() == "ironsurf://welcome"
    });
    activate(&ui);
    click_page(&ui);

    // --- Ctrl-T --------------------------------------------------------------
    let token = ui.get_focus_url_token();
    ui.invoke_page_key(true, "\u{14}".into(), true, false, false, false, false);
    pump();
    assert_eq!(tab_titles(&ui).len(), 2, "Ctrl-T opened a second tab");
    assert!(
        ui.get_focus_url_token() > token,
        "a new tab bumps the URL focus token exactly like F6 does"
    );
    // The bar is focused (proven by the keystroke below landing in it) and it
    // already carries the address the tab is loading.
    assert_eq!(ui.get_url_text().as_str(), "ironsurf://welcome", "the new tab filled the bar");

    // That address is highlighted: the keystroke replaces it outright.
    type_char(&ui, 'e');
    type_char(&ui, 'x');
    assert_eq!(
        ui.get_url_text().as_str(),
        "ex",
        "typing replaces the address a new tab highlighted"
    );

    // The spawned tab reports its URL and title a moment later; that feed must
    // never clobber the address being typed.
    wait_for("the new tab announced itself", || {
        pump();
        tab_titles(&ui).last().is_some_and(|t| t == "Fake \u{2014} ironsurf://welcome")
    });
    for _ in 0..5 {
        pump();
    }
    assert_eq!(ui.get_url_text().as_str(), "ex", "a loading tab cannot overwrite the draft");

    // Escape abandons it: the bar shows the active tab's own address again.
    ui.invoke_page_key(true, "\u{1b}".into(), false, false, false, false, false);
    assert_eq!(
        ui.get_url_text().as_str(),
        "ironsurf://welcome",
        "Escape shows the active tab's URL again"
    );

    // --- the ＋ button ---------------------------------------------------------
    click_page(&ui);
    let token = ui.get_focus_url_token();
    ui.invoke_new_tab();
    pump();
    assert_eq!(tab_titles(&ui).len(), 3, "the ＋ button opened a third tab");
    assert!(ui.get_focus_url_token() > token, "＋ bumps the URL focus token");
    assert_eq!(ui.get_url_text().as_str(), "ironsurf://welcome", "the new tab filled the bar");
    type_char(&ui, 'q');
    assert_eq!(ui.get_url_text().as_str(), "q", "＋ opens with the address highlighted");

    // A click in the page hands focus (and the bar) back to the tab: the
    // abandoned draft stops shielding the address bar from the tab's URL.
    // Waited for, not pumped a fixed number of times: the URL is the one the
    // freshly spawned tab announces, and a process start is not instant
    // everywhere (Windows' is slower) — a draft that is never dropped still
    // fails, on the deadline.
    click_page(&ui);
    wait_for("clicking the page drops the draft", || {
        pump();
        ui.get_url_text().as_str() == "ironsurf://welcome"
    });
}
