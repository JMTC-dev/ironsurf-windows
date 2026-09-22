//! The DevTools Inspector tree must lay a node's markup **end to end**.
//!
//! A `Text` in a layout has no upper width bound, so the leftover width of a
//! wide row used to be shared *equally between every markup token*: a `<html>`
//! row painted as `<    html    >` and the tree read as columns of `<` and `>`.
//! The row now hands its spare width to a stretchy tail instead, which is
//! observable headlessly: the token boxes stay as wide as their glyphs and the
//! whole markup stays on the left of the row.
//!
//! Geometry comes from the real layout, read back through
//! `i-slint-backend-testing`'s `ElementHandle` API (element ids are kept in the
//! generated code by `build.rs`), against the scripted fake tab's fixed
//! document — no X, no engine.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use ironsurf_shell::app::App;
use ironsurf_shell::AppWindow;
use slint::{ComponentHandle, Model};

/// Every test here wants its own `IRONSURF_DATA_DIR`, which is process-global.
static INSPECTOR_LAYOUT_ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn harness() -> (tempfile::TempDir, AppWindow, App) {
    i_slint_backend_testing::init_no_event_loop();
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("IRONSURF_DATA_DIR", dir.path());
    let ui = AppWindow::new().expect("window on testing backend");
    let fake = PathBuf::from(env!("CARGO_BIN_EXE_fake_tab"));
    let app = App::new(&ui, fake, Vec::new()).expect("app core");
    app.start(None);
    (dir, ui, app)
}

fn press_f12(ui: &AppWindow) {
    ui.invoke_page_key(true, "\u{F70F}".into(), false, false, false, false, false);
}

/// F12, then the third pane.
fn open_inspector(ui: &AppWindow) {
    if !ui.get_devtools_open() {
        press_f12(ui);
    }
    ui.invoke_devtools_select_view(2);
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

/// `(x, y, width, height)` of every element with `id` that the tree realized.
fn boxes(ui: &AppWindow, id: &str) -> Vec<(f32, f32, f32, f32)> {
    i_slint_backend_testing::ElementQuery::from_root(ui)
        .match_id(format!("AppWindow::{id}"))
        .find_all()
        .iter()
        .map(|e| {
            let p = e.absolute_position();
            let s = e.size();
            (p.x, p.y, s.width, s.height)
        })
        .collect()
}

/// The markup tokens of each realized tree row, left to right, grouped by the row
/// they sit in: `((row_y, row_width), tokens)`.
fn rows_of_tokens(ui: &AppWindow) -> Vec<((f32, f32), Vec<(f32, f32)>)> {
    let laid_out: Vec<(f32, f32)> = boxes(ui, "dr-toks")
        .into_iter()
        .map(|(_, y, w, _)| (y, w))
        .collect();
    let mut rows: Vec<((f32, f32), Vec<(f32, f32)>)> = Vec::new();
    for (x, y, w, _) in boxes(ui, "dr-tok") {
        match rows.iter_mut().find(|((ry, _), _)| (ry - y).abs() < 1.0) {
            Some((_, toks)) => toks.push((x, w)),
            None => {
                let width = laid_out
                    .iter()
                    .find(|(ry, _)| (ry - y).abs() < 1.0)
                    .map(|(_, w)| *w)
                    .unwrap_or(0.0);
                rows.push(((y, width), vec![(x, w)]));
            }
        }
    }
    for ((_, _), toks) in rows.iter_mut() {
        toks.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
    }
    rows.sort_by(|a, b| a.0 .0.partial_cmp(&b.0 .0).unwrap());
    rows
}

#[test]
fn a_nodes_markup_is_laid_end_to_end_across_its_row() {
    let _guard = INSPECTOR_LAYOUT_ENV
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (_dir, ui, app) = harness();
    // Rows of a ListView only get geometry once the window has been drawn.
    ui.show().expect("show on the testing backend");
    let core = app.core();
    let pump = || core.drain();
    wait_for("tab ready", || {
        pump();
        !core.tab_snapshot().is_empty() && core.tab_snapshot()[0].ready
    });
    open_inspector(&ui);
    wait_for("inspector tree", || {
        pump();
        ui.get_inspector_rows().row_count() > 1
    });

    let rows = rows_of_tokens(&ui);
    assert!(
        rows.len() >= 2,
        "inspector tree rows missing from the layout ({} rows realized)",
        rows.len()
    );
    if std::env::var("IRONSURF_TRACE_LAYOUT").is_ok() {
        for ((y, width), toks) in &rows {
            eprintln!("row y={y:.0} width={width:.0} tokens={toks:?}");
        }
    }

    for ((y, row_width), toks) in &rows {
        assert!(!toks.is_empty(), "row at y={y:.0} lost its markup tokens");
        // End to end: no token starts before the previous one ends, and none of
        // the row's width lands between two tokens.
        let markup: f32 = toks.iter().map(|(_, w)| w).sum();
        let span = toks.last().unwrap().0 + toks.last().unwrap().1 - toks[0].0;
        assert!(
            (span - markup).abs() < 1.0,
            "row at y={y:.0}: tokens are spread apart — span {span:.1}px of \
             {markup:.1}px of token, row {row_width:.1}px",
        );
        for pair in toks.windows(2) {
            let (x0, w0) = pair[0];
            let (x1, _) = pair[1];
            assert!(
                (x0 + w0 - x1).abs() < 1.0,
                "row at y={y:.0}: gap of {:.1}px between two markup tokens",
                x1 - (x0 + w0)
            );
        }
        // …and the markup as a whole stays on the left of its row: the spare
        // width belongs to the tail, not to the gaps between tokens.
        assert!(
            markup < row_width / 2.0,
            "row at y={y:.0}: markup of {markup:.1}px fills the whole \
             {row_width:.1}px row instead of its left half",
        );
    }
}

#[test]
fn every_row_keeps_a_tail_to_absorb_its_spare_width() {
    let _guard = INSPECTOR_LAYOUT_ENV
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (_dir, ui, app) = harness();
    // Rows of a ListView only get geometry once the window has been drawn.
    ui.show().expect("show on the testing backend");
    let core = app.core();
    let pump = || core.drain();
    wait_for("tab ready", || {
        pump();
        !core.tab_snapshot().is_empty() && core.tab_snapshot()[0].ready
    });
    open_inspector(&ui);
    wait_for("inspector tree", || {
        pump();
        ui.get_inspector_rows().row_count() > 1
    });

    let rows = boxes(&ui, "dr-toks");
    let tails = boxes(&ui, "dr-tail");
    assert!(rows.len() >= 2, "no tree rows realized in the layout");
    assert_eq!(
        rows.len(),
        tails.len(),
        "every tree row needs a stretchy tail to claim the width its tokens do not use"
    );
    for (row, (x, y, w, _)) in rows.iter().zip(tails.iter()) {
        let row_width = row.2;
        assert!(
            *w > row_width / 2.0,
            "tail at y={y:.0} claimed {w:.1}px of a {row_width:.1}px row — the              width the tokens do not use went somewhere else (x={x:.1})",
        );
    }
}
#[test]
fn a_tokens_width_is_its_glyphs_never_the_rows_leftover() {
    let _guard = INSPECTOR_LAYOUT_ENV
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (_dir, ui, app) = harness();
    let core = app.core();
    let pump = || core.drain();
    wait_for("tab ready", || {
        pump();
        !core.tab_snapshot().is_empty() && core.tab_snapshot()[0].ready
    });
    open_inspector(&ui);
    wait_for("inspector tree", || {
        pump();
        ui.get_inspector_rows().row_count() > 1
    });
    ui.show().expect("show on the testing backend");

    let wide = rows_of_tokens(&ui);
    // Narrow the panel: a row that still fits its markup has to paint that
    // markup exactly as wide as before. Width that comes from splitting the
    // row between the tokens follows the row width instead — that is the bug.
    ui.window().set_size(slint::LogicalSize::new(300.0, 700.0));
    ui.window().request_redraw();
    let narrow = rows_of_tokens(&ui);

    let mut checked = 0;
    for (wide_row, narrow_row) in wide.iter().zip(narrow.iter()) {
        let ((_, wide_row_width), wide_toks) = wide_row;
        let ((_, narrow_row_width), narrow_toks) = narrow_row;
        let markup: f32 = wide_toks.iter().map(|(_, w)| w).sum();
        // Rows that no longer fit their markup elide, which is allowed to
        // shorten the tail token; only compare the rows that still fit.
        if wide_toks.len() != narrow_toks.len() || markup > *narrow_row_width {
            continue;
        }
        for (i, ((_, wide_w), (_, narrow_w))) in
            wide_toks.iter().zip(narrow_toks.iter()).enumerate()
        {
            assert!(
                (wide_w - narrow_w).abs() < 0.5,
                "token {i} of a {markup:.1}px markup changed from {wide_w:.1}px to                  {narrow_w:.1}px when the row went from {wide_row_width:.1}px to                  {narrow_row_width:.1}px — its width comes from the row, not from its glyphs",
            );
            checked += 1;
        }
    }
    assert!(checked >= 8, "only {checked} tokens compared; the tree is not laid out");
}
