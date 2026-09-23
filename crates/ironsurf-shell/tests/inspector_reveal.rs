//! A selection made **on the page** has to end up on screen in the DevTools pane
//! (`ARCHITECTURE.md`, *Reveal a page-side selection*).
//!
//! The ⌖ crosshair, the context menu's *Inspect Element*, and a snapshot that
//! names a `selected` node all put the reader on a row they did not choose — and a
//! row below the fold of a short Inspector pane is a row nobody sees. The shell
//! knows *which* row that is (`InspectorUi::reveal_request`); only the pane knows
//! its geometry. This drives the scripted tab's own answer to *Inspect Element* —
//! node `b+6`, `<p class="note">`, the seventh row of an eight-row tree — through
//! the shell's real IPC path, then asks what the pane did about it.
//!
//! Two readings of the pane, because each lies on its own:
//!
//! * `inspector-shown-from` is the offset the tree **reported** for itself, and
//!   with the band's measured height it says where the visible slice sits — but a
//!   reported offset is not yet a drawn row.
//! * The rows the `ListView` has **realized** are drawn ones: a row exists in the
//!   layout only while it is in the viewport (plus Slint's small cache), and its
//!   markup line (`dr-toks`) sits at that row's indent with one `dr-tok` per
//!   markup token — so the pair (indent, token count) names the model row it
//!   belongs to. Realized rows are the proof the tree actually moved somewhere it
//!   had never been.
//!
//! Legs, in order: nothing owed ⇒ first row visible, picked row not, tree at its
//! top ⇒ *Inspect Element* ⇒ the picked row is selected, the tree reports an
//! offset where it had none, and the picked row is drawn ⇒ pump after pump,
//! nothing moves ⇒ clicking *another row* moves the highlight and does not drag
//! the view ⇒ the same node picked again is a request once more, and a pane
//! already showing it stays put instead of jumping.
//!
//! The pane asks for the offset that would put the row wholly inside the band, and
//! the tree keeps the last word: a `ListView` refuses to be scrolled past what it
//! has realized, so a long pan comes back short — the pane re-asks from where it
//! landed, up to a bounded number of turns (see `InspectorReveal`), and what this
//! test can insist on is that it got to the row (it is drawn), never past it, and
//! then held. That is the whole of what a reader without the feature never got:
//! the row was not even drawn.
//!
//! Headless (`i-slint-backend-testing`): no X, no engine. With
//! `init_no_event_loop()` nothing pumps Slint's queued work, so `draw` below is
//! what lets a pane act on a request at all; `Core::drain` is the pump for tab
//! messages, which in the real app are delivered by its event loop.

use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

use ironsurf_shell::app::{App, Core};
use ironsurf_shell::AppWindow;
use slint::{ComponentHandle, Model};

/// The row the scripted tab answers *Inspect Element* with: `document > html >
/// (head > title > text) > body > p` — the seventh of eight rows.
const PICKED_ROW: usize = 6;
/// The row the pane starts on: the snapshot names no selection, so the document
/// node takes it — the first row of the tree.
const FIRST_ROW: usize = 0;
/// The height one row is painted at, the indent per depth, and where a row's
/// markup starts inside it — the pane's own numbers, restated here so a silent
/// change in any of them fails in this test.
const ROW_HEIGHT: f32 = 22.0;
const INDENT: f32 = 12.0;
const MARKUP_X: f32 = 22.0;

/// A window with the scripted fake tab on the testing backend. The temp dir is
/// this process's `IRONSURF_DATA_DIR`, so it is held for as long as the app.
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

/// Let the frame run: queued property changes and `changed` handlers, so the pane
/// can act on a reveal request. The real app gets this from its event loop.
fn draw() {
    slint::platform::update_timers_and_animations();
}

/// Pump until `f` says so — drain, let the pane run, look again.
fn wait_for(what: &str, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if f() {
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// `(x, y, width, height)` of every realized element with `id`.
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

fn box_of(ui: &AppWindow, id: &str) -> Option<(f32, f32, f32, f32)> {
    boxes(ui, id).into_iter().find(|b| b.2 > 0.0 && b.3 > 0.0)
}

/// The tree's viewport — the band of rows the reader can see.
fn band(ui: &AppWindow) -> (f32, f32, f32, f32) {
    box_of(ui, "insp-tree").expect("the Inspector tree is laid out")
}

/// How far the tree says it is scrolled: a positive number of pixels from the top
/// of the content to the top of the band.
fn shown(ui: &AppWindow) -> f32 {
    ui.get_inspector_shown_from()
}

/// Where row `n` sits in the content, and what `shown` would have to be for it to
/// fit inside a band of `h`.
fn row_top(n: usize) -> f32 {
    n as f32 * ROW_HEIGHT
}

/// Is row `n` inside the band, given what the tree reported about itself?
fn row_in_view(ui: &AppWindow, n: usize) -> bool {
    let h = band(ui).3;
    let top = row_top(n);
    shown(ui) <= top + 0.01 && shown(ui) + h + 0.01 >= top + ROW_HEIGHT
}

/// `(depth, token count)` of every **model** row: what the pane has been told to
/// paint, in tree order.
fn row_sigs(ui: &AppWindow) -> Vec<(usize, usize)> {
    let model = ui.get_inspector_rows();
    (0..model.row_count())
        .map(|i| {
            let r = model.row_data(i).expect("in range");
            (r.depth as usize, r.toks.row_count())
        })
        .collect()
}

/// The same pair for every row the tree has actually **laid out**: its markup
/// line sits at its indent, and its tokens are its children.
fn laid_out_sigs(ui: &AppWindow) -> Vec<(usize, usize)> {
    let x0 = band(ui).0 + MARKUP_X;
    let mut lines: Vec<(f32, f32, usize)> = Vec::new();
    for (x, y, _, _) in boxes(ui, "dr-tok") {
        match lines.iter_mut().find(|(ly, _, _)| (ly - y).abs() < 1.0) {
            Some((_, _, n)) => *n += 1,
            None => lines.push((y, x, 1)),
        }
    }
    lines.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
    lines
        .into_iter()
        .map(|(_, x, n)| (((x - x0) / INDENT + 0.5) as usize, n))
        .collect()
}

/// Has the tree laid the row of `sig` out at all? Realizing it means the tree
/// came to rest close enough to it to draw it.
fn laid_out(ui: &AppWindow, sig: (usize, usize)) -> bool {
    laid_out_sigs(ui).contains(&sig)
}

/// Is row `n` *painted* inside the band? Where the tree says it should be —
/// `band.top + (row.top - offset)` — there has to be a highlighted row of the
/// right height, and it has to be inside the band rather than clipped by it. That
/// is a stronger claim than the reported offset alone, because a painted box is
/// something the tree laid out and drew at that position.
fn row_painted_in_view(ui: &AppWindow, n: usize) -> bool {
    let b = band(ui);
    let y = b.1 + (row_top(n) - shown(ui));
    let inside = y >= b.1 - 0.01 && y + ROW_HEIGHT <= b.1 + b.3 + 0.01;
    inside
        && boxes(ui, "dr-bg")
            .into_iter()
            .any(|r| (r.1 - y).abs() < 1.0 && (r.3 - ROW_HEIGHT).abs() < 0.01)
}

/// Which model row the pane highlights.
fn selected_row(ui: &AppWindow) -> Option<usize> {
    let model = ui.get_inspector_rows();
    (0..model.row_count()).find(|i| model.row_data(*i).expect("in range").selected)
}

fn row_node(ui: &AppWindow, n: usize) -> i32 {
    ui.get_inspector_rows().row_data(n).expect("in range").node
}

/// Right-click in the page, then choose *⌖ Inspect Element* from the menu that
/// opens: the page-side pick, asking about the point the menu opened at
/// (`ToTab::InspectPickAt`). ⌖ armed with Ctrl-Shift-C reaches the same answer by
/// the other route, which `inspector.rs` drives.
fn inspect_element_at(ui: &AppWindow, core: &Rc<Core>, x: f32, y: f32) {
    ui.invoke_page_pointer(x, y, 1, 1);
    ui.invoke_page_pointer(x, y, 2, 1);
    wait_for("the context menu to open", || {
        core.drain();
        ui.get_menu_open()
    });
    assert_eq!((ui.get_menu_x(), ui.get_menu_y()), (x, y), "the menu opens where the click was");
    let items: Vec<String> = (0..ui.get_menu_items().row_count())
        .map(|i| ui.get_menu_items().row_data(i).expect("in range").label.to_string())
        .collect();
    let at = items
        .iter()
        .position(|l| l == "\u{2316} Inspect Element")
        .expect("the context menu offers the tree of the page the click landed in");
    ui.invoke_menu_pick(at as i32);
}

/// The state of the tree as one comparable value: how far it is scrolled and
/// every laid-out row's position.
fn view(ui: &AppWindow) -> (f32, Vec<(f32, f32, f32, f32)>) {
    (shown(ui), boxes(ui, "dr-toks"))
}

#[test]
fn a_selection_made_on_the_page_is_scrolled_into_view() {
    let (_dir, ui, app) = harness();
    // A ListView hands out geometry only once its window has been drawn, and this
    // test is entirely about where the rows end up.
    ui.show().expect("show on the testing backend");
    // A short window makes the pane a band of a couple of rows, which is the
    // situation the feature exists for: most of the tree is below the fold.
    ui.window().set_size(slint::LogicalSize::new(640.0, 300.0));
    let core = app.core();
    let pump = || core.drain();

    wait_for("the tab to be ready", || {
        pump();
        let s = core.tab_snapshot();
        !s.is_empty() && s[0].ready
    });

    // Panel up, Inspector pane up. The snapshot that arrives names no selection,
    // so the document row takes it and nothing is owed.
    ui.invoke_page_key(true, "\u{F70F}".into(), false, false, false, false, false); // F12
    ui.invoke_devtools_select_view(2);
    wait_for("an Inspector tree the pane has laid out", || {
        pump();
        draw();
        ui.get_inspector_rows().row_count() > PICKED_ROW && box_of(&ui, "insp-tree").is_some()
    });

    let b = band(&ui);
    let sigs = row_sigs(&ui);
    assert!(b.3 > 0.0, "the tree has a viewport: {b:?}");
    assert!(row_top(FIRST_ROW) + ROW_HEIGHT <= b.3, "the first row fits in it");
    assert!(
        row_top(PICKED_ROW) + ROW_HEIGHT > b.3 + 8.0,
        "the picked row has to hang past the bottom edge, with room to spare, or this \
         test proves nothing: band {} rows, picked row {PICKED_ROW} starts at {}",
        b.3 / ROW_HEIGHT,
        row_top(PICKED_ROW),
    );
    assert_ne!(sigs[FIRST_ROW], sigs[PICKED_ROW], "the two rows must be tellable apart");
    assert_eq!(selected_row(&ui), Some(FIRST_ROW), "the pane starts on the document row");
    assert_eq!(shown(&ui), 0.0, "at the top of the content");
    assert!(laid_out(&ui, sigs[FIRST_ROW]), "the first row is drawn");
    assert!(!laid_out(&ui, sigs[PICKED_ROW]), "the picked row is below the fold, undrawn");

    // The page asks for the node under a click. Selecting a row the reader cannot
    // see is a request — the pane has to go looking for it.
    inspect_element_at(&ui, &core, 233.0, 147.0);
    wait_for("the page's pick to select the picked row", || {
        pump();
        draw();
        selected_row(&ui) == Some(PICKED_ROW)
    });
    wait_for("the pane to paint the picked row inside the band", || {
        pump();
        draw();
        row_painted_in_view(&ui, PICKED_ROW)
    });
    let off = shown(&ui);
    assert!(off > 0.0, "the tree moved: it reports an offset where it had none");
    assert!(
        laid_out(&ui, sigs[PICKED_ROW]),
        "and it moved far enough to draw the row the page selected — realized rows: {:?}",
        laid_out_sigs(&ui)
    );
    assert!(off <= row_top(PICKED_ROW) + 0.01, "and it never went past the row it was sent to");
    assert!(
        off + b.3 + 0.01 >= row_top(PICKED_ROW) + ROW_HEIGHT,
        "the ask was for the row to fit inside the band: asked for {}, band {}, row ends at {}",
        off, b.3, row_top(PICKED_ROW) + ROW_HEIGHT,
    );

    // The tree does not fight the reader once the row is home: let it settle, then
    // hold — asked for nothing new, it keeps the offset, pump after pump.
    for _ in 0..6 {
        pump();
        draw();
    }
    let settled = view(&ui);
    assert!(settled.0 > 0.0, "and the offset it settled at is the one it reported");
    for _ in 0..12 {
        pump();
        draw();
    }
    assert_eq!(view(&ui), settled, "the pane does not fight its own scroll");
    assert_eq!(selected_row(&ui), Some(PICKED_ROW), "still the row the page selected");
    assert!(row_in_view(&ui, PICKED_ROW), "and still on screen");

    // Clicking *another row* is the reader arriving somewhere themselves: the
    // highlight moves, the view does not — `InspectorUi::select` owes no scroll,
    // because the row they clicked is the row they are looking at.
    ui.invoke_devtools_select_node(row_node(&ui, FIRST_ROW));
    wait_for("the row click to select the document row", || {
        pump();
        draw();
        selected_row(&ui) == Some(FIRST_ROW)
    });
    assert_eq!(view(&ui), settled, "a row click must not drag the view to the top of the content");

    // And a page pick is a request every time, even for a row that is already
    // showing: the selection lands on it again, and a pane that already has it on
    // screen keeps it there rather than jumping.
    inspect_element_at(&ui, &core, 96.0, 120.0);
    wait_for("the second pick to select the row again", || {
        pump();
        draw();
        selected_row(&ui) == Some(PICKED_ROW)
    });
    wait_for("the pane to hold the row on screen", || {
        pump();
        draw();
        row_in_view(&ui, PICKED_ROW)
    });
    assert!(
        shown(&ui) <= row_top(PICKED_ROW) + 0.01,
        "the reveal does not shove a row that was already visible above the top edge"
    );
}
