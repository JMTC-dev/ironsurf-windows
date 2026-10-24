//! Archive mode, end to end on the testing backend (task: archive mode).
//!
//! The 💾 toolbar button toggles a right-docked overlay that never stacks
//! with the other three menus; the CheckBox flips the mode (off by default),
//! persists it to `ui.json` at once, and rebroadcasts `ToTab::SetArchiveMode`
//! — the scripted fake tab (`--expect-archive-mode`) echoes its current
//! value as `archive:on|off`, the launch state and every later toggle, so
//! the wire is assertable end to end. The folder Apply validates (relative
//! paths rejected with the draft kept, `~` expanded, the tree created).
//! A tab spawned **while the mode is on** launches with `--archive-mode`,
//! echoes `archive:on` from its very first line, and ships its archived
//! fetches — which the shell lays out as
//! `<root>/<ISO8601>/<host>/index.html`, `<root>/<ISO8601>/<host>/js/app.js?v=2`
//! and `<root>/<ISO8601>/<host>/html/?q=asdf` (the URL query rides on the
//! file name; the `file:` asset dropped entirely). Headless
//! (`i-slint-backend-testing`): no X, no engine.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ironsurf_shell::AppWindow;
use ironsurf_shell::app::App;
use ironsurf_shell::archive::fs_safe_name;
use ironsurf_shell::uisettings::UiSettings;

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

fn status(ui: &AppWindow) -> String {
    ui.get_status_text().to_string()
}

/// List the entries of a directory (sorted for stable asserts).
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

#[test]
fn archive_mode_toggles_persists_and_writes_the_tree() {
    i_slint_backend_testing::init_no_event_loop();

    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("IRONSURF_DATA_DIR", dir.path());
    let ui_json = dir.path().join("ui.json");
    let disk = || UiSettings::load(&ui_json);

    let ui = AppWindow::new().expect("window on testing backend");
    let fake = PathBuf::from(env!("CARGO_BIN_EXE_fake_tab"));
    let app =
        App::new(&ui, fake.clone(), vec!["--expect-archive-mode".to_string()]).expect("app core");
    app.start(None);
    let core = app.core();
    let pump = || core.drain();

    // ── off by default: tab 1 launches WITHOUT --archive-mode ⇒ archive:off ──
    wait_for("tab ready with the mode off ⇒ archive:off", || {
        pump();
        core.tab_snapshot().first().is_some_and(|s| s.ready) && status(&ui) == "archive:off"
    });
    assert!(!disk().archive_enabled, "off by default, persisted");

    // ── the 💾 button toggles the overlay ────────────────────────────────────
    assert!(!ui.get_archive_menu_open());
    ui.invoke_toggle_archive_menu();
    assert!(ui.get_archive_menu_open());
    assert!(
        ui.get_archive_status().starts_with("Off"),
        "the status line says the mode is off: {:?}",
        ui.get_archive_status()
    );
    assert!(
        ui.get_archive_dir_text().ends_with("ironsurf-tabs"),
        "the draft is prefilled with the built-in default root: {:?}",
        ui.get_archive_dir_text()
    );
    ui.invoke_toggle_archive_menu();
    assert!(!ui.get_archive_menu_open());

    // ── the four overlays never stack ────────────────────────────────────────
    ui.invoke_open_manager();
    ui.invoke_toggle_archive_menu();
    assert!(ui.get_archive_menu_open() && !ui.get_manager_open(), "archive closes the manager");
    ui.invoke_open_manager();
    assert!(!ui.get_archive_menu_open(), "the manager closes the archive menu");
    ui.invoke_toggle_archive_menu();

    // ── point the root at a temp folder (validation legs first) ──────────────
    let root = dir.path().join("arch");
    ui.set_archive_dir_text("relative/nope".into());
    ui.invoke_archive_apply_dir("relative/nope".into());
    assert!(
        !ui.get_archive_error().is_empty(),
        "a relative folder is rejected on the one line"
    );
    assert_eq!(
        ui.get_archive_dir_text().as_str(),
        "relative/nope",
        "the rejected draft survives untouched"
    );
    assert_eq!(disk().archive_dir, None, "a rejected Apply changes nothing");
    ui.set_archive_dir_text(root.to_string_lossy().to_string().as_str().into());
    ui.invoke_archive_apply_dir(root.to_string_lossy().to_string().as_str().into());
    assert!(ui.get_archive_error().is_empty(), "the temp folder is accepted");
    assert!(root.exists(), "Apply creates the tree");
    assert_eq!(disk().archive_dir.as_deref(), Some(root.to_str().unwrap()));

    // ── flip the mode on: persisted + broadcast to the live tab ──────────────
    ui.invoke_archive_set_enabled(true);
    assert!(disk().archive_enabled, "the CheckBox persists at once");
    wait_for("the live tab echoes the toggle", || {
        pump();
        status(&ui) == "archive:on"
    });
    assert!(ui.get_archive_enabled());
    assert!(ui.get_archive_status().starts_with("On"), "status: {:?}", ui.get_archive_status());

    // ── a tab spawned while the mode is on: --archive-mode at birth, its own
    // timestamp folder, and its files on disk ────────────────────────────────
    ui.invoke_new_tab();
    wait_for("the second tab (born archiving) is ready", || {
        pump();
        core.tab_snapshot().len() == 2 && core.tab_snapshot()[1].ready
    });
    // its scripted fetches have shipped with the Ready batch; drain them.
    let read = |p: &Path| std::fs::read(p).unwrap_or_default();
    wait_for("the document lands under <root>/<ISO8601>/example.com/index.html", || {
        pump();
        entries(&root).first().is_some_and(|visit| {
            read(&root.join(visit).join("example.com/index.html")) == b"ARCHIVE-PAGE"
        })
    });
    let landed = root.join(entries(&root).first().cloned().unwrap());
    // the timestamp folder is ISO8601-with-offset and exactly one exists.
    let name = landed.file_name().unwrap().to_string_lossy().to_string();
    assert!(name.contains('T') && name.len() >= 20, "visit folder {name}");
    assert_eq!(entries(&root), vec![name.clone()]);
    // the subresource keeps its path, the query riding on the file name…
    // (as the file system takes it: `app.js%3Fv=2` on Windows)
    assert_eq!(
        read(&landed.join("example.com/js").join(fs_safe_name("app.js?v=2"))),
        b"ARCHIVE-APP"
    );
    // …and so does a query document: `/html/?q=asdf` is the file `?q=asdf`
    // inside `html/`, next to the front page rather than on top of it.
    assert_eq!(
        read(&landed.join("example.com/html").join(fs_safe_name("?q=asdf"))),
        b"ARCHIVE-QUERY-PAGE"
    );
    // …and the local file never lands anywhere under the archive root.
    let walk = |root: &Path| -> Vec<PathBuf> {
        let mut out = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else {
                    out.push(p.strip_prefix(root).unwrap().to_path_buf());
                }
            }
        }
        out.sort();
        out
    };
    // Compared as paths, names as the file system takes them (Windows joins
    // with `\` and escapes the `?`).
    let site = Path::new(&name).join("example.com");
    assert_eq!(
        walk(&root),
        vec![
            site.join("html").join(fs_safe_name("?q=asdf")),
            site.join("index.html"),
            site.join("js").join(fs_safe_name("app.js?v=2")),
        ],
        "exactly the three web files, the file: asset dropped"
    );

    // the status line counts the saves.
    wait_for("the overlay counts the archived files", || {
        pump();
        ui.get_archive_status().contains("3 file(s) saved")
    });

    // ── flip it off: broadcast, and nothing further is written ───────────────
    ui.invoke_archive_set_enabled(false);
    wait_for("the live tabs echo the switch off", || {
        pump();
        status(&ui) == "archive:off"
    });
    assert!(!disk().archive_enabled);
    assert_eq!(walk(&root).len(), 3, "off changes nothing on disk");
    ui.invoke_close_archive_menu();
    assert!(!ui.get_archive_menu_open());
}
