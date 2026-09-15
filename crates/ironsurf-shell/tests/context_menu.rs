//! Right-click context menu, end to end on the testing backend: a scripted
//! right-click into the page makes the (fake) tab report a context target,
//! the shell opens the menu with the entries that target deserves, and each
//! picked row does its job — new tab, session clipboard, and a paste that
//! round-trips back to the tab as `ToTab::Clipboard` + `ContextAction::Paste`.

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

fn labels(ui: &AppWindow) -> Vec<String> {
    (0..ui.get_menu_items().row_count())
        .map(|i| ui.get_menu_items().row_data(i).unwrap().label.to_string())
        .collect()
}

/// Right-button press+release at (x, y): exactly what Slint's TouchArea
/// produces for a right-click, forwarded through the same callback.
fn right_click(ui: &AppWindow) {
    ui.invoke_page_pointer(120.0, 60.0, 1, 1); // down, right
    ui.invoke_page_pointer(120.0, 60.0, 2, 1); // up, right
}

#[test]
fn right_click_opens_the_menu_the_target_deserves_and_the_rows_act() {
    i_slint_backend_testing::init_no_event_loop();

    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("IRONSURF_DATA_DIR", dir.path());

    let ui = AppWindow::new().expect("window on testing backend");
    let fake = PathBuf::from(env!("CARGO_BIN_EXE_fake_tab"));
    let app = App::new(&ui, fake, Vec::new()).expect("app core");
    app.start(None);
    let pump = || app.core().drain();
    let snap = || app.core().tab_snapshot();
    wait_for("tab ready", || {
        pump();
        !snap().is_empty() && snap()[0].ready
    });

    // No menu before the click.
    assert!(!ui.get_menu_open());

    // The fake tab answers a right-click with: link + selection + editable.
    right_click(&ui);
    wait_for("menu open", || {
        pump();
        ui.get_menu_open()
    });
    assert_eq!(
        labels(&ui),
        [
            "⟳ Reload",
            "Open link in new tab",
            "Copy",
            "Copy link address",
            "Copy page address",
        ],
        "empty clipboard ⇒ no Paste row yet"
    );
    // The menu sits at the click point.
    assert_eq!(ui.get_menu_x(), 120.0);
    assert_eq!(ui.get_menu_y(), 60.0);

    // "Copy" (row 2) files the selection into the session clipboard.
    ui.invoke_menu_pick(2);
    pump();
    assert!(!ui.get_menu_open(), "picking a row closes the menu");

    // Next menu: the same rows *plus* Paste (a caret and a full clipboard).
    right_click(&ui);
    wait_for("second menu open", || {
        pump();
        ui.get_menu_open()
    });
    let second = labels(&ui);
    assert_eq!(
        second,
        [
            "⟳ Reload",
            "Open link in new tab",
            "Copy",
            "Copy link address",
            "Paste",
            "Copy page address",
        ]
    );

    // "Paste" goes to the tab as Clipboard + ContextAction::Paste; the fake
    // tab echoes it back on the status line.
    let paste_at = second.iter().position(|l| l == "Paste").unwrap();
    ui.invoke_menu_pick(paste_at as i32);
    wait_for("paste echoed by the tab", || {
        pump();
        snap()[0].status.as_deref() == Some("pasted:fake selection")
    });

    // "Open link in new tab" spawns a sibling tab on the link URL.
    right_click(&ui);
    wait_for("menu open again", || {
        pump();
        ui.get_menu_open()
    });
    ui.invoke_menu_pick(1);
    wait_for("second tab opened the link URL", || {
        pump();
        snap().iter().any(|s| {
            s.url.as_deref() == Some("ironsurf://welcome/menu-link") && s.ready
        })
    });
    assert_eq!(snap().len(), 2);

    // The scrim: closing without picking runs no action (menu just hides).
    right_click(&ui);
    wait_for("menu open once more", || {
        pump();
        ui.get_menu_open()
    });
    ui.invoke_menu_close();
    pump();
    assert!(!ui.get_menu_open());
    assert_eq!(snap().len(), 2, "dismissed menu acts on nothing");

    app.shutdown();
}
