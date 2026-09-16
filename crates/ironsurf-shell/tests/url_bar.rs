//! Ctrl-L / F6 focus the URL bar and highlight the whole address, end to end
//! on the testing backend: the chord arrives through the page's `FocusScope`
//! (`page-key`), Rust bumps `focus-url-token`, the `UrlBar` grabs focus and
//! selects all text — proven by typing one character afterwards, which must
//! REPLACE the address instead of extending or leaving it untouched.
//!
//! The realistic user sequence is reproduced: window active, a real click on
//! the page (which arms the Slint focus machinery, exactly like browsing
//! before hitting Ctrl-L), then the chord, then typing.
//! Headless (`i-slint-backend-testing`): no X, no engine.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use ironsurf_shell::app::App;
use ironsurf_shell::AppWindow;
use slint::ComponentHandle;

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

/// A real left click in the page area: the canonical way to give the page
/// keyboard focus before reaching for Ctrl-L.
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

/// One printable key press delivered to whatever the window has focused —
/// the URL bar `LineEdit` after a focus chord, with the whole address
/// selected, so the character replaces the selection.
fn type_char(ui: &AppWindow, c: char) {
    ui.window().dispatch_event(slint::platform::WindowEvent::KeyPressed {
        text: c.to_string().into(),
    });
}

#[test]
fn ctrl_l_and_f6_focus_the_url_bar_and_select_all_text() {
    i_slint_backend_testing::init_no_event_loop();

    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("IRONSURF_DATA_DIR", dir.path());

    let ui = AppWindow::new().expect("window on testing backend");
    let fake = PathBuf::from(env!("CARGO_BIN_EXE_fake_tab"));
    let app = App::new(&ui, fake, Vec::new()).expect("app core");
    app.start(None); // ironsurf://welcome

    let pump = || app.core().drain();
    wait_for("start URL in the bar", || {
        pump();
        ui.get_url_text().as_str() == "ironsurf://welcome"
    });
    activate(&ui);
    click_page(&ui);

    // --- Ctrl-L (control-char form, as a backend delivers the chord) ------
    ui.invoke_page_key(true, "\u{0c}".into(), true, false, false, false, false);
    assert!(ui.get_focus_url_token() > 0, "Ctrl-L must bump the URL focus token");
    // The whole address was highlighted: typing replaces it outright.
    type_char(&ui, 'x');
    assert_eq!(
        ui.get_url_text().as_str(),
        "x",
        "after Ctrl-L the selection covers the address, so typing replaces it"
    );

    // --- F6 (unmodified) ----------------------------------------------------
    ui.set_url_text("https://example.com/".into());
    let token_before = ui.get_focus_url_token();
    // F6 = U+F704 + 5 = U+F709 in Slint's private-use key table.
    ui.invoke_page_key(true, "\u{F709}".into(), false, false, false, false, false);
    assert!(
        ui.get_focus_url_token() > token_before,
        "F6 must bump the URL focus token like Ctrl-L"
    );
    type_char(&ui, 'y');
    assert_eq!(
        ui.get_url_text().as_str(),
        "y",
        "after F6 the selection covers the address, so typing replaces it"
    );

    // --- neighbouring keys must NOT steal focus or select -------------------
    // F5 (U+F708) is an ordinary key: the page gets it, the token does not move.
    let t = ui.get_focus_url_token();
    ui.invoke_page_key(true, "\u{F708}".into(), false, false, false, false, false);
    assert_eq!(ui.get_focus_url_token(), t, "F5 is not a URL-focus chord");
}
