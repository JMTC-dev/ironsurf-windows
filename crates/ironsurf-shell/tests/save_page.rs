//! The "Save page" dialog (task: save-page selector), end to end on the
//! testing backend.
//!
//! Ctrl-`S` pops a dialog over the page: the folder to save into (prefilled
//! with the ⬇ root, which defaults to `$HOME/Downloads`) and the name to save
//! under (prefilled from the URL, else the title). Choosing a folder follows
//! the ⬇ panel's rules exactly, and a confirmed save asks the tab — through
//! the very `ToTab::DownloadUrl` channel the context menu uses — for its own
//! page under that name; the scripted fake tab answers `DownloadStart` →
//! `DownloadChunk`×2 → `DownloadEnd`, and the bytes land **in the folder the
//! dialog pointed at**, not the root, while the row shows up in the ⬇ panel
//! like any other download. On `about:blank` there is nothing to save and the
//! dialog says so instead of opening. Headless: no X, no engine, no network.

use std::path::{Path, PathBuf};
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

/// The ⬇ panel's rows, `name`/`status` joined for stable asserts.
fn rows(ui: &AppWindow) -> Vec<(String, String)> {
    let model = ui.get_downloads();
    (0..model.row_count())
        .map(|i| {
            let row = model.row_data(i).unwrap();
            (row.name.to_string(), row.status.to_string())
        })
        .collect()
}

#[test]
fn ctrl_s_pops_a_save_dialog_that_saves_where_it_points() {
    i_slint_backend_testing::init_no_event_loop();

    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("IRONSURF_DATA_DIR", dir.path());
    // The ⬇ root an env var names — the default the dialog prefills with.
    let env_root = dir.path().join("env-downloads");
    std::env::set_var(
        ironsurf_shell::downloads::DOWNLOAD_DIR_ENV,
        env_root.to_str().unwrap(),
    );

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

    let ctrl_s = || ui.invoke_page_key(true, "s".into(), true, false, false, false, false);

    // ── no page, no dialog: on `about:blank` Ctrl-S answers on the one
    //    error line instead of opening an empty dialog ─────────────────────────
    ctrl_s();
    pump();
    assert!(!ui.get_save_menu_open(), "about:blank has nothing to save");
    assert!(
        ui.get_save_error().as_str().contains("no page"),
        "…and it says so: {:?}",
        ui.get_save_error()
    );

    // ── a page appears and Ctrl-S opens the dialog, both fields prefilled:
    //    the ⬇ root as the folder, the URL's own file as the name ──────────────
    ui.invoke_navigate("http://example.org/report.html".into());
    wait_for("report page", || {
        pump();
        core.tab_snapshot()[0]
            .url
            .as_deref()
            .is_some_and(|u| u.ends_with("report.html"))
    });
    let page = core.tab_snapshot()[0].url.clone().unwrap();
    ctrl_s();
    assert!(ui.get_save_menu_open(), "Ctrl-S opens the dialog on a real page");
    assert_eq!(ui.get_save_page().as_str(), page.as_str());
    assert_eq!(ui.get_save_name_text(), "report.html");
    assert_eq!(ui.get_save_dir_text().as_str(), env_root.to_str().unwrap());

    // ── it is one more overlay and never stacks: the ⬇ panel shuts it ─────────
    ui.invoke_toggle_download_menu();
    assert!(
        ui.get_download_menu_open() && !ui.get_save_menu_open(),
        "the ⬇ panel takes the room the dialog had"
    );
    ctrl_s();
    assert!(ui.get_save_menu_open() && !ui.get_download_menu_open());

    // ── a relative folder is refused the ⬇ panel's way: reason on the line,
    //    draft kept, dialog still open ──────────────────────────────────────────
    ui.invoke_save_pick_folder("notes".into());
    assert!(ui.get_save_menu_open());
    assert!(ui.get_save_error().as_str().contains("is not absolute"));
    assert_eq!(ui.get_save_dir_text().as_str(), env_root.to_str().unwrap());

    // ── an accepted folder is created, becomes the save's home, and clears
    //    the line ───────────────────────────────────────────────────────────────
    let chosen = dir.path().join("somewhere else");
    ui.invoke_save_pick_folder(chosen.to_str().unwrap().into());
    assert!(chosen.is_dir(), "an accepted folder is created");
    assert_eq!(ui.get_save_dir_text().as_str(), chosen.to_str().unwrap());
    assert!(ui.get_save_error().as_str().is_empty());

    // ── Esc closes the dialog without saving, and the folder just pointed at
    //    is what the next Ctrl-S reopens onto ───────────────────────────────────
    ui.invoke_page_key(true, "Escape".into(), false, false, false, false, false);
    assert!(!ui.get_save_menu_open(), "Esc shuts the dialog");
    ctrl_s();
    assert!(ui.get_save_menu_open());
    assert_eq!(
        ui.get_save_dir_text().as_str(),
        chosen.to_str().unwrap(),
        "the pointed folder outlives a plain close"
    );

    // ── "Save": the dialog shuts, the tab fetches its page through the ⬇
    //    machinery, and the bytes land in the pointed folder — the root
    //    itself stays untouched ─────────────────────────────────────────────────
    ui.invoke_save_page_confirm("my page.html".into());
    assert!(!ui.get_save_menu_open(), "a confirmed save shuts the dialog");
    wait_for("the save row", || {
        pump();
        rows(&ui)
            .iter()
            .any(|(name, status)| name == "my page.html" && status.starts_with("Done"))
    });
    assert_eq!(entries(&chosen), vec!["my page.html".to_string()]);
    assert_eq!(
        std::fs::read(chosen.join("my page.html")).unwrap(),
        b"FAKE-DOWNLOAD-BODY".as_slice(),
        "the page's bytes land in the folder the dialog pointed at"
    );
    assert!(
        entries(&env_root).is_empty(),
        "the ⬇ root keeps receiving everything else — nothing went there"
    );

    // ── the pointed folder was one-shot with the save, so the next Ctrl-S
    //    defaults again from the page: the ⬇ root and the URL's own name ───────
    ctrl_s();
    assert!(ui.get_save_menu_open());
    assert_eq!(ui.get_save_name_text(), "report.html");
    assert_eq!(ui.get_save_dir_text().as_str(), env_root.to_str().unwrap());
    ui.invoke_close_save_menu();
    assert!(!ui.get_save_menu_open());
}
