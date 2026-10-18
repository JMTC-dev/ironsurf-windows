//! File downloads, end to end on the testing backend (task: file downloads).
//!
//! The ⬇ toolbar button (and Ctrl-Shift-D) opens the fifth right-docked
//! overlay, which never stacks with the other four; its folder LineEdit
//! follows the archive's rules (`~` expanded, relative refused with the draft
//! kept, blank reverts to the default) and persists to `ui.json` as
//! `download_dir`. The **tab** decides what is a file and fetches it — the
//! scripted fake tab (`src/bin/fake_tab.rs`) answers
//! `ToTab::DownloadUrl` ("⬇ Download linked file" in the context menu) with
//! `DownloadStart` → `DownloadChunk`×2 → `DownloadEnd`, and the shell lays the
//! bytes out in the folder: the name comes from the URL (`menu-link`), a
//! second same-named save becomes `menu-link (2)`, and a URL carrying "fail"
//! settles `Failed` with **no** file left in the folder. The status line names
//! the newest row of the tab on screen, so a save is visible with the panel
//! shut. Headless (`i-slint-backend-testing`): no X, no engine, no network.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ironsurf_shell::app::App;
use ironsurf_shell::uisettings::UiSettings;
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

/// Sorted entries of a folder (empty when it does not exist yet).
fn entries(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

/// The ⬇ panel's rows, `name`/`status`/`sub` joined for stable asserts.
fn rows(ui: &AppWindow) -> Vec<(String, String, String)> {
    let model = ui.get_downloads();
    (0..model.row_count())
        .map(|i| {
            let row = model.row_data(i).unwrap();
            (
                row.name.to_string(),
                row.status.to_string(),
                row.sub.to_string(),
            )
        })
        .collect()
}

/// The context menu's labels.
fn menu(ui: &AppWindow) -> Vec<String> {
    (0..ui.get_menu_items().row_count())
        .map(|i| ui.get_menu_items().row_data(i).unwrap().label.to_string())
        .collect()
}

/// Right-button press+release at (120, 60) — what Slint's TouchArea produces
/// for a right-click; the scripted tab answers with a link target.
fn right_click(ui: &AppWindow) {
    ui.invoke_page_pointer(120.0, 60.0, 1, 1, false, false, false, false); // down, right
    ui.invoke_page_pointer(120.0, 60.0, 2, 1, false, false, false, false); // up, right
}

/// Wait for the context menu, then pick a row by its label.
fn pick(ui: &AppWindow, pump: impl Fn(), label: &str) {
    wait_for(&format!("menu with {label:?}"), || {
        pump();
        ui.get_menu_open() && menu(ui).iter().any(|l| l == label)
    });
    let at = menu(ui).iter().position(|l| l == label).unwrap();
    ui.invoke_menu_pick(at as i32);
}

#[test]
fn downloads_land_name_themselves_and_survive_a_panel_that_shuts() {
    i_slint_backend_testing::init_no_event_loop();

    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("IRONSURF_DATA_DIR", dir.path());
    // The folder an env var names — absolute, so it is taken (a relative one
    // says nothing about where the disk is).
    let env_root = dir.path().join("env-downloads");
    std::env::set_var(
        ironsurf_shell::downloads::DOWNLOAD_DIR_ENV,
        env_root.to_str().unwrap(),
    );
    let ui_json = dir.path().join("ui.json");
    let disk = || UiSettings::load(&ui_json);
    let saved = |root: &Path, name: &str| std::fs::read(root.join(name)).unwrap_or_default();

    let ui = AppWindow::new().expect("window on testing backend");
    let fake = PathBuf::from(env!("CARGO_BIN_EXE_fake_tab"));
    let app = App::new(&ui, fake, Vec::new()).expect("app core");
    app.start(None);
    let core = app.core();
    let pump = || core.drain();
    wait_for("tab ready", || {
        pump();
        !core.tab_snapshot().is_empty() && core.tab_snapshot()[0].ready
    });

    // ── the ⬇ button opens the panel, prefilled with the live default ───────
    assert!(!ui.get_download_menu_open());
    ui.invoke_toggle_download_menu();
    assert!(ui.get_download_menu_open());
    assert_eq!(
        ui.get_download_dir_text().as_str(),
        env_root.to_str().unwrap(),
        "the draft holds the effective root ($IRONSURF_DOWNLOAD_DIR)"
    );
    assert!(
        ui.get_download_status().starts_with("0 download"),
        "an empty panel says so and names the folder: {:?}",
        ui.get_download_status()
    );
    // The panel's rows are the active tab's, of which there are none yet.
    assert!(rows(&ui).is_empty());

    // ── the five overlays never stack: ⬇ is open right now, so opening the
    //    container manager shuts it, and the ⬇ button shuts the manager back ──
    ui.invoke_open_manager();
    assert!(
        ui.get_manager_open() && !ui.get_download_menu_open(),
        "the manager takes the room the ⬇ panel had"
    );
    ui.invoke_toggle_download_menu();
    assert!(
        ui.get_download_menu_open() && !ui.get_manager_open(),
        "…and the ⬇ panel takes it back"
    );
    ui.invoke_toggle_archive_menu();
    assert!(ui.get_archive_menu_open() && !ui.get_download_menu_open(), "archive closes downloads");
    ui.invoke_toggle_download_menu();
    assert!(ui.get_download_menu_open() && !ui.get_archive_menu_open(), "downloads closes archive");

    // ── Ctrl-Shift-D opens and shuts the same panel; plain Ctrl-D is the
    //    page's own chord and does nothing here ──────────────────────────────
    ui.invoke_close_download_menu();
    ui.invoke_page_key(true, "D".into(), true, false, false, false, false);
    assert!(ui.get_download_menu_open(), "Ctrl-Shift-D opens");
    ui.invoke_page_key(true, "D".into(), true, false, false, false, false);
    assert!(!ui.get_download_menu_open(), "…and shuts");
    ui.invoke_page_key(true, "d".into(), true, false, false, false, false);
    ui.invoke_page_key(true, "\u{04}".into(), true, false, false, false, false);
    assert!(
        !ui.get_download_menu_open(),
        "a plain Ctrl-D stays the page's (no panel, nothing saved)"
    );

    // ── the folder's rules: relative refused (draft kept, nothing saved),
    //    absolute accepted (created + persisted), blank reverts ──────────────
    let root = dir.path().join("saves");
    ui.invoke_toggle_download_menu();
    ui.set_download_dir_text("relative/nope".into());
    ui.invoke_download_apply_dir("relative/nope".into());
    assert!(
        !ui.get_download_error().is_empty(),
        "a relative folder is refused on the one line: {:?}",
        ui.get_download_error()
    );
    assert_eq!(
        ui.get_download_dir_text().as_str(),
        "relative/nope",
        "the rejected draft survives untouched"
    );
    assert_eq!(disk().download_dir, None, "a refused Apply saves nothing");
    assert!(!root.exists());
    ui.set_download_dir_text(root.to_string_lossy().to_string().as_str().into());
    ui.invoke_download_apply_dir(root.to_string_lossy().to_string().as_str().into());
    assert!(ui.get_download_error().is_empty(), "the temp folder is accepted");
    assert!(root.exists(), "Apply creates the folder");
    assert_eq!(disk().download_dir.as_deref(), Some(root.to_str().unwrap()));

    // ── "⬇ Download linked file" asks the tab; the bytes land in the folder ──
    // The scripted tab's link target is `<url>/menu-link`, so the name comes
    // from the URL and the body is two chunks: `FAKE-DOWNLOAD-` + `BODY`.
    let bytes = || b"FAKE-DOWNLOAD-BODY".to_vec();
    right_click(&ui);
    pick(&ui, pump, "\u{2B07} Download linked file");
    wait_for("the file lands in the folder", || {
        pump();
        entries(&root) == vec!["menu-link".to_string()]
    });
    assert_eq!(saved(&root, "menu-link"), bytes());
    // …and the row is visible without the panel: the status line names the
    // newest row of the tab on screen.
    wait_for("the status line names the save", || {
        pump();
        ui.get_status_text().as_str() == "Saved menu-link \u{2014} 18 B"
    });
    // The panel, still open, counts it and quotes the full path.
    assert_eq!(
        rows(&ui),
        vec![(
            "menu-link".to_string(),
            "Done 18 B".to_string(),
            root.join("menu-link").to_string_lossy().into_owned(),
        )],
        "name, size, and the path the file is at"
    );
    assert_eq!(
        ui.get_download_status(),
        format!("1 download · {}", root.display()).as_str()
    );

    // ── a second save of the same name collides: the counter rides before the
    //    extension (here: there is none), never overwriting the first ────────
    right_click(&ui);
    pick(&ui, pump, "\u{2B07} Download linked file");
    wait_for("the second file lands as `menu-link (2)`", || {
        pump();
        entries(&root) == vec!["menu-link".to_string(), "menu-link (2)".to_string()]
    });
    assert_eq!(saved(&root, "menu-link (2)"), bytes());
    // Newest first, so the second download leads the list — and it keeps the
    // name it was *asked* to be: the `(2)` lives in the path the row shows,
    // not in the name the reader wrote.
    assert_eq!(
        rows(&ui)[0],
        (
            "menu-link".to_string(),
            "Done 18 B".to_string(),
            root.join("menu-link (2)").to_string_lossy().into_owned(),
        )
    );

    // ── a fetch that fails leaves nothing behind: the row says `Failed`, the
    //    file it opened is deleted, and the earlier saves are untouched ──────
    // The scripted tab fails any URL carrying "fail", so point the page at
    // such an address first (its link target inherits it).
    ui.invoke_navigate("example.com/fail/thing".into());
    wait_for("the page moved to a failing address", || {
        pump();
        core.tab_snapshot()[0]
            .url
            .as_deref()
            .is_some_and(|u| u.contains("/fail/thing"))
    });
    right_click(&ui);
    pick(&ui, pump, "\u{2B07} Download linked file");
    wait_for("the row settles Failed", || {
        pump();
        rows(&ui).first().is_some_and(|r| r.1 == "Failed")
    });
    assert_eq!(
        entries(&root),
        vec!["menu-link".to_string(), "menu-link (2)".to_string()],
        "a download that never wrote a byte leaves no file in the folder"
    );
    wait_for("the status line says it failed", || {
        pump();
        ui.get_status_text().as_str() == "Failed menu-link"
    });

    // ── `Clear finished` drops the settled rows and keeps the files: clearing
    //    a list is not deleting what the list named ──────────────────────────
    assert_eq!(rows(&ui).len(), 3);
    ui.invoke_download_clear_finished();
    pump();
    assert!(rows(&ui).is_empty(), "Done and Failed rows leave the panel");
    assert_eq!(
        entries(&root),
        vec!["menu-link".to_string(), "menu-link (2)".to_string()],
        "the files stay exactly where they are"
    );

    // ── a blank draft reverts to the default folder (the archive's rule,
    //    inverted: a blank says "give it back to the browser") ───────────────
    ui.set_download_dir_text("".into());
    ui.invoke_download_apply_dir("".into());
    assert!(ui.get_download_error().is_empty());
    assert_eq!(
        ui.get_download_dir_text().as_str(),
        env_root.to_str().unwrap(),
        "back to $IRONSURF_DOWNLOAD_DIR, the built-in default"
    );
    assert_eq!(disk().download_dir, None, "the revert is persisted too");

    // ── a download is not a navigation: the page stayed put throughout ──────
    let page = core.tab_snapshot().remove(0);
    assert_eq!(
        page.title.as_str(),
        "Fake \u{2014} https://example.com/fail/thing",
        "no title change ever happened for a save"
    );
    assert!(!page.loading, "no spinner either");
    ui.invoke_close_download_menu();
    assert!(!ui.get_download_menu_open());
}
