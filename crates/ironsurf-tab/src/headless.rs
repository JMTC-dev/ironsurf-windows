//! Headless ACID-test harness mode (ACID3 work, step 1).
//!
//! `ironsurf-tab --headless --url URL [--wait MS] [--body-out FILE]
//! [--shot FILE.ppm] [--size WxH] [--no-sandbox implied]`
//!
//! One-shot, terminal-driven invocation of the *same* machinery
//! [`runtime::run`] uses — same fetch/`netmod`/policy objects (via
//! [`runtime::TabArgs::parse`] on the filtered argv), same single
//! `HtmlDocument` + settle + [`crate::js`] script flow through
//! [`runtime::Tab::navigate`] — but with:
//!
//! - the OS sandbox skipped (`--no-sandbox` implied: a scoring harness reads
//!   arbitrary `file://` fixtures and writes arbitrary output paths),
//! - **stdout unused**: the tab's IPC sink points at `/dev/null`
//!   ([`runtime::Tab::new_headless`]), so the `ToTab`/`FromTab` contract is
//!   untouched and nothing is emitted on the (unconnected) stdout pipe,
//! - a fixed-size viewport (`--size`, default 1024×768) applied before the
//!   single navigation, and
//! - a bounded `--wait` window that drains self-pipe wake-ups with a short
//!   poll sleep and runs the JS timers + settle periodically (this window is
//!   the poll loop for `setTimeout`/`setInterval`/rAF — see `runtime::Tab::
//!   headless_pump`), so anything that lands late is handled before scoring
//!   output is captured.
//!
//! Outputs: the `<body>` element's **innerHTML** (a small DOM serializer over
//! `blitz-dom` nodes — blitz has no `innerHTML`; `textContent` is the fallback
//! if there is no body) to `--body-out`, and a full-viewport `vello_cpu`
//! RGBA→RGB P6 PPM to `--shot`. A one-line summary goes to **stderr**:
//! `HEADLESS title=<title> body=<len> shot=<path>`. Exit code is 0 on the
//! happy path; `main` `_exit`s without ever speaking IPC.

use std::error::Error;
use std::fs;
use std::io::Write;
use std::os::fd::RawFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyrender::{render_to_buffer, PaintScene};
use anyrender_vello_cpu::VelloCpuImageRenderer;
use blitz_html::HtmlDocument;
use blitz_paint::paint_scene;
use kurbo::{Affine, Rect, Size};
use peniko::{Color, Fill};
use ironsurf_common::ToTab;

use crate::runtime::{NavHistory, SelfPipe, Tab, TabArgs};

/// Poll sleep inside the `--wait` window: wake-ups are delivered
/// immediately, this only bounds how long a quiet page is waited out.
const POLL_SLICE: Duration = Duration::from_millis(50);

// ---------------------------------------------------------------------------
// CLI

pub struct HeadlessArgs {
    /// Filtered argv for the *normal* tab parser (net policy, profile dir,
    /// `--offline-fixtures`, log level … all keep working in headless mode).
    pub tab_args: TabArgs,
    pub wait: Duration,
    pub body_out: Option<PathBuf>,
    pub shot: Option<PathBuf>,
    /// Find-in-page query to run before the shot (`--find QUERY`): lets the
    /// highlights a live search paints be inspected as pixels.
    pub find: Option<String>,
    /// Extra `FindNext` commands after the query (`--find-jumps N`): the shot
    /// then shows the light-blue marker *N* results further along, which is how
    /// the jump/scroll/wrap behaviour gets inspected.
    pub find_jumps: usize,
    pub width: u32,
    pub height: u32,
    /// Synthetic left-clicks (`--click X,Y`, repeatable): Move/Down/Up sent
    /// through the same `ToTab::Pointer` path the shell drives, executed
    /// after the `--wait` window; `--dump-inputs` then serialises every
    /// `<input>`'s id/type/checked state to stderr for harness diagnostics.
    pub clicks: Vec<(f32, f32)>,
    pub dump_inputs: bool,
    /// `--dump-layout`: print the client rect of the first `<article>` and
    /// every ancestor element (tag/class + rect) to stderr, so a blank column
    /// can be told apart from a zero-height scroll container.
    pub dump_layout: bool,
    /// `--dump-paint`: paint into a recording `Scene` and print a command
    /// histogram, so a laid-out-but-invisible subtree can be told apart from a
    /// paint-culling bug.
    pub dump_paint: bool,
    /// `--dump-paint-detail`: for every large fill record its untransformed
    /// path, its transform and the enclosing clip stack — the fingerprint that
    /// tells a mis-scaled vector (SVG/path) fill from an oversized CSS box.
    pub dump_paint_detail: bool,
    /// `--hit X,Y` (repeatable): print the element chain under each point.
    pub hits: Vec<(f32, f32)>,
}

impl HeadlessArgs {
    pub fn parse(args: &[String]) -> Result<Self, Box<dyn Error>> {
        let mut wait_ms: u64 = 5_000;
        let mut body_out = None;
        let mut shot = None;
        let mut size = (1024u32, 768u32);
        let mut find = None;
        let mut find_jumps: usize = 0;
        let mut clicks = Vec::new();
        let mut dump_inputs = false;
        let mut dump_layout = false;
        let mut dump_paint = false;
        let mut dump_paint_detail = false;
        let mut hits = Vec::new();
        let mut saw_url = false;

        // Every flag the *normal* parser must not see (headless-only surface).
        let mut tab_argv: Vec<String> = Vec::with_capacity(args.len());
        tab_argv.push(args.first().cloned().unwrap_or_default());

        let mut it = args.iter().skip(1);
        while let Some(arg) = it.next() {
            let mut value = |what: &str| -> Result<String, Box<dyn Error>> {
                it.next()
                    .cloned()
                    .ok_or_else(|| format!("--{what} requires a value").into())
            };
            match arg.as_str() {
                "--headless" => {} // selected by main()
                "--wait" => {
                    wait_ms = value("wait")?.parse().map_err(|_| "--wait needs milliseconds")?
                }
                "--body-out" => body_out = Some(PathBuf::from(value("body-out")?)),
                "--shot" => shot = Some(PathBuf::from(value("shot")?)),
                "--find" => find = Some(value("find")?),
                "--find-jumps" => {
                    find_jumps = value("find-jumps")?
                        .parse()
                        .map_err(|_| "--find-jumps needs a count")?
                },
                "--click" => {
                    let v = value("click")?;
                    let (x, y) = v
                        .split_once(',')
                        .ok_or_else(|| format!("--click must be X,Y, got {v:?}"))?;
                    clicks.push((
                        x.parse().map_err(|_| format!("bad x in --click {v:?}"))?,
                        y.parse().map_err(|_| format!("bad y in --click {v:?}"))?,
                    ));
                }
                "--dump-inputs" => dump_inputs = true,
                "--dump-layout" => dump_layout = true,
                "--dump-paint" => dump_paint = true,
                "--dump-paint-detail" => dump_paint_detail = true,
                "--hit" => {
                    let v = value("hit")?;
                    let (x, y) = v
                        .split_once(',')
                        .ok_or_else(|| format!("--hit must be X,Y, got {v:?}"))?;
                    hits.push((
                        x.parse().map_err(|_| format!("bad x in --hit {v:?}"))?,
                        y.parse().map_err(|_| format!("bad y in --hit {v:?}"))?,
                    ));
                }
                "--size" => {
                    let v = value("size")?;
                    let (w, h) = v
                        .split_once('x')
                        .ok_or_else(|| format!("--size must be WxH, got {v}"))?;
                    size = (
                        w.parse()
                            .map_err(|_| format!("bad width in --size {v:?}"))?,
                        h.parse()
                            .map_err(|_| format!("bad height in --size {v:?}"))?,
                    );
                }
                "--url" => {
                    saw_url = true;
                    let v = value("url")?;
                    tab_argv.push("--url".to_string());
                    tab_argv.push(v);
                }
                // Everything else (`--managed-net`, `--offline-fixtures DIR`,
                // `--log …`, profile overrides) passes through untouched; a
                // value-taking flag keeps its value with it.
                other @ ("--tab-id" | "--container-id" | "--profile-dir"
                | "--offline-fixtures" | "--log" | "--log-level") => {
                    tab_argv.push(other.to_string());
                    tab_argv.push(value(other.trim_start_matches('-'))?);
                }
                _ => tab_argv.push(arg.clone()),
            }
        }
        if !saw_url {
            return Err("--headless requires --url URL".into());
        }
        let mut tab_args = TabArgs::parse(&tab_argv)?;
        // Sandbox implied off; the harness writes outside the profile dir and
        // a scoring run is single-purpose by construction.
        tab_args.no_sandbox = true;

        Ok(HeadlessArgs {
            tab_args,
            wait: Duration::from_millis(wait_ms),
            body_out,
            shot,
            find,
            find_jumps,
            clicks,
            dump_inputs,
            dump_layout,
            dump_paint,
            dump_paint_detail,
            hits,
            width: size.0.max(1),
            height: size.1.max(1),
        })
    }
}

// ---------------------------------------------------------------------------
// Runner

pub fn run(args: &[String]) -> Result<(), Box<dyn Error>> {
    let args = HeadlessArgs::parse(args)?;
    ironsurf_common::warn!(
        "headless harness: url={} wait={}ms size={}x{} sandbox=off (implied --no-sandbox)",
        args.tab_args.url,
        args.wait.as_millis(),
        args.width,
        args.height
    );

    let pipe = Arc::new(SelfPipe::new()?);
    let mut tab = Tab::new_headless(&args.tab_args, Arc::clone(&pipe));

    // Fixed viewport BEFORE the navigation so the document is built at the
    // requested size (same `set_viewport` semantics the shell drives with).
    tab.handle_to_tab(ToTab::Resize {
        width: args.width,
        height: args.height,
        scale: 1.0,
    });

    // The one navigation: fetch (managed/passthrough policy per flags) →
    // parse → settle → `<script>` run (`crate::js`), exactly as live tabs.
    tab.navigate(&args.tab_args.url.clone(), NavHistory::Fresh);

    // --wait window: this IS the poll loop for JS timers — each pump runs
    // every due setTimeout/setInterval/rAF callback (real timers task) and
    // settles late subresource messages, and the sleep never overshoots the
    // earliest timer deadline, so a `setTimeout(fn, 10)` fires ~10 ms in.
    let deadline = Instant::now() + args.wait;
    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let slice = remaining
            .min(POLL_SLICE)
            .min(tab.timer_sleep_hint().unwrap_or(remaining));
        poll_wake(pipe.read_fd(), slice);
        pipe.drain();
        tab.headless_pump();
    }

    // Synthetic clicks (`--click X,Y`): the full Move/Down/Up triple through
    // the shell-facing pointer path, then a settle pump so any restyle the
    // click's default action produced is reflected before capture.
    for (x, y) in args.clicks.clone() {
        for kind in [
            ironsurf_common::PointerKind::Move,
            ironsurf_common::PointerKind::Down,
            ironsurf_common::PointerKind::Up,
        ] {
            tab.handle_to_tab(ToTab::Pointer(ironsurf_common::PointerEvent {
                x,
                y,
                kind,
                button: ironsurf_common::MouseButton::Left,
            }));
        }
        tab.headless_pump();
    }
    if args.dump_inputs {
        if let Some(doc) = tab.document_mut() {
            dump_input_state(doc);
        }
    }
    if args.dump_layout {
        if let Some(doc) = tab.document_mut() {
            dump_article_layout(doc);
        }
    }
    if args.dump_paint {
        if let Some(doc) = tab.document_mut() {
            dump_paint_scene(doc, args.width, args.height);
        }
    }
    if args.dump_paint_detail {
        if let Some(doc) = tab.document_mut() {
            dump_paint_detail(doc, args.width, args.height);
        }
    }
    for (x, y) in args.hits.clone() {
        if let Some(doc) = tab.document_mut() {
            dump_hit_chain(doc, x, y);
        }
    }
    if args.dump_layout {
        if let Some(doc) = tab.document_mut() {
            dump_big_boxes(doc);
        }
    }

    // A scripted search (`--find`): exactly the command the shell sends when the
    // reader types in the find bar, so the shot shows the highlights a live
    // search paints — every match yellow, the current one light blue.
    if let Some(query) = args.find.clone() {
        tab.handle_to_tab(ToTab::SetFind { query });
        // …and N times the shell's “next” button (Enter): the active marker
        // walks the results, scrolling each one into view.
        for _ in 0..args.find_jumps {
            tab.handle_to_tab(ToTab::FindNext);
        }
    }

    // Capture from the settled document.
    let title = tab.title_now().unwrap_or_default();
    let body = tab
        .document_mut()
        .map(|doc| body_html(&*doc))
        .unwrap_or_default();

    if let Some(path) = &args.body_out {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, &body)?;
    }
    // The highlights are measured from the live layout at the shot's own scale
    // (before the document is borrowed for painting), then drawn over the
    // finished page scene.
    let highlights = if args.shot.is_some() {
        tab.find_highlights(1.0)
    } else {
        Vec::new()
    };
    let shot_len = match (&args.shot, tab.document_mut()) {
        (Some(path), Some(doc)) => {
            if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
                fs::create_dir_all(parent)?;
            }
            let len = shot_ppm(doc, args.width, args.height, &highlights, path)?;
            Some(len)
        }
        (Some(_), None) => {
            ironsurf_common::warn!("headless: no document to screenshot");
            None
        }
        (None, _) => None,
    };

    tab.persist();

    let shot_desc = match (&args.shot, shot_len) {
        (Some(path), Some(_)) => path.display().to_string(),
        _ => "-".to_string(),
    };
    // stderr only — stdout stays unused in headless mode (IPC contract).
    eprintln!(
        "HEADLESS title={title} body={} shot={shot_desc}",
        body.len()
    );
    Ok(())
}

/// Serialise every `<input>` element's `id`/`type`/checkedness to stderr
/// (`INPUT id=… type=… checked=true|false`) — the diagnostics twin of
/// `--click`, so a harness can watch a checkbox toggle from the DOM side.
/// Serialise every `<input>` element's `id`/`type`/checkedness to stderr
/// (`INPUT id=… type=… checked=true|false`) — the diagnostics twin of
/// `--click`, so a harness can watch a checkbox toggle from the DOM side.
/// Serialise every `<input>` element's `id`/`type`/checkedness to stderr
/// (`INPUT id=… type=… checked=true|false`) — the diagnostics twin of
/// `--click`, so a harness can watch a checkbox toggle from the DOM side.
/// Serialise every `<input>` element's `id`/`type`/checkedness to stderr
/// (`INPUT id=… type=… checked=true|false`) — the diagnostics twin of
/// `--click`, so a harness can watch a checkbox toggle from the DOM side.
/// Serialise every `<input>` element's `id`/`type`/checkedness to stderr
/// (`INPUT id=… type=… checked=true|false`) — the diagnostics twin of
/// `--click`, so a harness can watch a checkbox toggle from the DOM side.
/// Serialise every `<input>` element's `id`/`type`/checkedness to stderr
/// (`INPUT id=… type=… checked=true|false`) — the diagnostics twin of
/// `--click`, so a harness can watch a checkbox toggle from the DOM side.
/// Serialise every `<input>` element's `id`/`type`/checkedness to stderr
/// (`INPUT id=… type=… checked=true|false`) — the diagnostics twin of
/// `--click`, so a harness can watch a checkbox toggle from the DOM side.
/// Serialise every `<input>` element's `id`/`type`/checkedness to stderr
/// (`INPUT id=… type=… checked=true|false`) — the diagnostics twin of
/// `--click`, so a harness can watch a checkbox toggle from the DOM side.
/// Print the element chain under a viewport point, hit-test style.
fn dump_hit_chain(doc: &blitz_html::HtmlDocument, x: f32, y: f32) {
    let Some(start) = doc.element_from_point(x, y) else {
        eprintln!("HIT {x},{y}: <nothing>");
        return;
    };
    let mut names = Vec::new();
    let mut cur = Some(start);
    while let Some(id) = cur {
        let Some(node) = doc.get_node(id) else { break };
        if let Some(el) = node.element_data() {
            names.push(format!(
                "{}{}",
                el.name.local,
                match el.id.as_ref().map(|a| &**a) {
                    Some(i) => format!("#{i}"),
                    None => match el.attr(blitz_dom::local_name!("class")) {
                        Some(c) if !c.is_empty() => {
                            format!(".{}", c.split_whitespace().next().unwrap_or(""))
                        }
                        _ => String::new(),
                    },
                }
            ));
        }
        cur = node.parent;
    }
    eprintln!("HIT {x},{y}: {}", names.join(" < "));
}

/// Print the client rect of the first `<article>` and every ancestor element
/// (tag + first class + x/y/w/h) to stderr. `--dump-layout`'s twin: a column
/// whose markup is present but blank is either a zero-height scroll container
/// or a paint problem, and the rect chain says which.
fn dump_article_layout(doc: &blitz_html::HtmlDocument) {
    let Some(article) = doc
        .tree()
        .iter()
        .find(|(_, node)| {
            node.element_data()
                .is_some_and(|el| el.name.local.as_ref() == "article")
        })
        .map(|(id, _)| id)
    else {
        eprintln!("LAYOUT: no <article> in the document");
        return;
    };
    let viewport = doc.viewport_scroll();
    eprintln!("LAYOUT viewport_scroll=({:.1},{:.1})", viewport.x, viewport.y);
    let mut cur = Some(article);
    while let Some(id) = cur {
        let Some(node) = doc.get_node(id) else { break };
        let desc = match node.element_data() {
            Some(el) => format!(
                "{}{}",
                el.name.local,
                el.attr(blitz_dom::local_name!("class"))
                    .map(|c| format!(".{}", c.split_whitespace().next().unwrap_or("")))
                    .unwrap_or_default()
            ),
            None => "#non-element".to_string(),
        };
        let rect = doc
            .get_client_bounding_rect(id)
            .map(|r| format!("x={:.1} y={:.1} w={:.1} h={:.1}", r.x, r.y, r.width, r.height))
            .unwrap_or_else(|| "<no rect>".to_string());
        let scroll = node.scroll_offset();
        let overflow = node.scrollable_overflow();
        let paint_children = node.paint_children.borrow();
        let paint_info = match paint_children.as_ref() {
            Some(children) => format!(
                " paint_children={} has_article={}",
                children.len(),
                children.contains(&article)
            ),
            None => " paint_children=None".to_string(),
        };
        let style = node
            .primary_styles()
            .map(|s| {
                let prop = |p: &str| crate::js::computed_style_value(&*s, p);
                format!(
                    " opacity={} vis={} display={} overflow=({},{})",
                    prop("opacity"),
                    prop("visibility"),
                    prop("display"),
                    prop("overflow-x"),
                    prop("overflow-y"),
                )
            })
            .unwrap_or_default();
        eprintln!(
            "LAYOUT {desc}: {rect} scroll=({:.1},{:.1}) overflow=({:.1},{:.1},{:.1},{:.1}){style}{paint_info}",
            scroll.x,
            scroll.y,
            overflow.x0,
            overflow.y0,
            overflow.width(),
            overflow.height()
        );
        cur = node.parent;
    }
    // Descend from the article: a stale/empty `paint_children` on any level
    // stops the paint traversal even though layout is correct.
    dump_paint_chain_rec(doc, article, 0);
    // …and the text-bearing subtree specifically.
    if let Some(content) = doc.tree().iter().find_map(|(id, node)| {
        let el = node.element_data()?;
        let class = el.attr(blitz_dom::local_name!("class"))?;
        class.contains("status__content").then_some(id)
    }) {
        eprintln!("PAINTTEXT from {content:?}");
        dump_paint_chain_rec(doc, content, 0);
    }
}

fn dump_paint_chain_rec(doc: &blitz_html::HtmlDocument, id: blitz_dom::NodeId, depth: usize) {
    if depth > 4 {
        return;
    }
    let Some(node) = doc.get_node(id) else { return };
    let children: Vec<blitz_dom::NodeId> = node.children.iter().copied().collect();
    let paint: Vec<blitz_dom::NodeId> = node
        .paint_children
        .borrow()
        .as_ref()
        .map(|v| v.iter().copied().collect())
        .unwrap_or_default();
    for (index, child_id) in children.iter().enumerate() {
        let Some(child) = doc.get_node(*child_id) else { continue };
        let desc = child
            .element_data()
            .map(|el| {
                format!(
                    "{}{}",
                    el.name.local,
                    el.attr(blitz_dom::local_name!("class"))
                        .map(|c| format!(".{}", c.split_whitespace().next().unwrap_or("")))
                        .unwrap_or_default()
                )
            })
            .unwrap_or_else(|| "#text".to_string());
        let painted = paint.contains(child_id);
        let child_paint = child
            .paint_children
            .borrow()
            .as_ref()
            .map(|v| v.len())
            .unwrap_or(0);
        let child_style = child
            .primary_styles()
            .map(|s| {
                let prop = |p: &str| crate::js::computed_style_value(&*s, p);
                format!(
                    " opacity={} vis={} display={}",
                    prop("opacity"),
                    prop("visibility"),
                    prop("display")
                )
            })
            .unwrap_or_default();
        let bb = if child.element_data().is_some() {
            *child.scrollable_overflow()
        } else {
            kurbo::Rect::ZERO
        };
        eprintln!(
            "PAINCHAIN {}{} idx={index} painted={painted} paint_children={child_paint} inline_root={} ovf=({:.0},{:.0},{:.0},{:.0}){child_style}",
            "  ".repeat(depth),
            desc,
            child.flags.is_inline_root(),
            bb.x0,
            bb.y0,
            bb.x1,
            bb.y1
        );
        if index == 0 {
            dump_paint_chain_rec(doc, *child_id, depth + 1);
        }
    }
}

/// Paint the settled document into `anyrender`'s recording `Scene` and print a
/// command histogram (`--dump-paint`). Glyph runs are bucketed by their
/// transform's translation so a blank article column shows up as "zero glyph
/// runs below y=100" even though layout says the text is there.
fn dump_paint_scene(doc: &mut blitz_html::HtmlDocument, width: u32, height: u32) {
    use anyrender::recording::RenderCommand;
    use kurbo::Shape as _;
    let mut scene = anyrender::recording::Scene::new();
    blitz_paint::paint_scene(&mut scene, doc, 1.0, width, height, 0, 0);
    let mut glyphs = 0usize;
    let mut glyphs_in_articles = 0usize;
    let mut fills = 0usize;
    let mut strokes = 0usize;
    let mut clips = 0usize;
    let mut clip_shapes: Vec<String> = Vec::new();
    let mut glyph_positions: Vec<String> = Vec::new();
    let mut big_fills: Vec<String> = Vec::new();
    for cmd in &scene.commands {
        match cmd {
            RenderCommand::PushClipLayer(clip) => {
                let bb = clip.transform.transform_rect_bbox(clip.clip.bounding_box());
                clip_shapes.push(format!(
                    "({:.0},{:.0})-({:.0},{:.0})",
                    bb.x0, bb.y0, bb.x1, bb.y1
                ));
            }
            RenderCommand::PushLayer(layer) => {
                let bb = layer.transform.transform_rect_bbox(layer.clip.bounding_box());
                clip_shapes.push(format!(
                    "L({:.0},{:.0})-({:.0},{:.0})",
                    bb.x0, bb.y0, bb.x1, bb.y1
                ));
            }
            _ => {}
        }
    }
    eprintln!("PAINTCLIPS {}", clip_shapes.join(" "));
    for cmd in &scene.commands {
        match cmd {
            RenderCommand::GlyphRun(run) => {
                glyphs += 1;
                let [_, _, _, _, tx, ty] = run.transform.as_coeffs();
                if tx >= 340.0 && tx <= 940.0 && ty >= 100.0 && ty <= 700.0 {
                    glyphs_in_articles += 1;
                }
                if glyph_positions.len() < 40 {
                    glyph_positions.push(format!(
                        "({tx:.0},{ty:.0})fs{:.0}x{}",
                        run.font_size,
                        run.glyphs.len()
                    ));
                }
            }
            RenderCommand::Fill(fill) => {
                fills += 1;
                let bb = fill.transform.transform_rect_bbox(fill.shape.bounding_box());
                let area = (bb.x1 - bb.x0) * (bb.y1 - bb.y0);
                if area > 5000.0 {
                    let brush = match &fill.brush {
                        anyrender::Paint::Solid(c) => {
                            format!("{},{},{},{}", c.components[0], c.components[1], c.components[2], c.components[3])
                        }
                        _ => "non-solid".to_string(),
                    };
                    big_fills.push(format!(
                        "({:.0},{:.0})-({:.0},{:.0}) rgba({brush})",
                        bb.x0, bb.y0, bb.x1, bb.y1
                    ));
                }
            }
            RenderCommand::Stroke(_) => strokes += 1,
            RenderCommand::PushClipLayer(_) | RenderCommand::PushLayer(_) => clips += 1,
            _ => {}
        }
    }
    eprintln!("PAINTGLYPHS {}", glyph_positions.join(" "));
    eprintln!("PAINTFILLS {}", big_fills.join(" "));
    eprintln!(
        "PAINT commands={} glyph_runs={glyphs} glyph_runs_in_column_top={glyphs_in_articles} fills={fills} strokes={strokes} layers={clips}",
        scene.commands.len()
    );
}

/// Paint the settled document into a recording `Scene` and describe every
/// *large* fill command (`--dump-paint-detail`): the untransformed path, the
/// transform applied to it and the enclosing clip stack. A huge black blob is
/// thereby fingerprinted — a mis-scaled vector fill says “24-unit path times a
/// 30× transform”, an oversized CSS box says “a 720×560 rect at an offset”.
fn dump_paint_detail(doc: &mut blitz_html::HtmlDocument, width: u32, height: u32) {
    use anyrender::recording::RenderCommand;
    use kurbo::Shape as _;

    let mut scene = anyrender::recording::Scene::new();
    blitz_paint::paint_scene(&mut scene, doc, 1.0, width, height, 0, 0);

    let brush_text = |brush: &anyrender::Paint| -> String {
        match brush {
            anyrender::Paint::Solid(c) => format!(
                "rgba({},{},{},{})",
                c.components[0], c.components[1], c.components[2], c.components[3]
            ),
            anyrender::Paint::Gradient(_) => "gradient".to_string(),
            anyrender::Paint::Image(_) => "image".to_string(),
            other => format!("{other:?}"),
        }
    };

    let mut stack: Vec<(char, kurbo::Rect)> = Vec::new();
    let mut shown = 0usize;
    let mut centers: Vec<(f64, f64)> = Vec::new();
    for (index, cmd) in scene.commands.iter().enumerate() {
        match cmd {
            RenderCommand::PushClipLayer(clip) => {
                stack.push((
                    'C',
                    clip.transform
                        .transform_rect_bbox(clip.clip.bounding_box()),
                ));
            }
            RenderCommand::PushLayer(layer) => {
                stack.push((
                    'L',
                    layer
                        .transform
                        .transform_rect_bbox(layer.clip.bounding_box()),
                ));
            }
            RenderCommand::PopLayer => {
                stack.pop();
            }
            RenderCommand::Fill(fill) => {
                let bb = fill.transform.transform_rect_bbox(fill.shape.bounding_box());
                // Threshold is overridable so the same dump can be used on a
                // small fixture (`IRONSURF_FILL_MIN_AREA=0`).
                let min_area: f64 = std::env::var("IRONSURF_FILL_MIN_AREA")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(5000.0);
                if (bb.x1 - bb.x0) * (bb.y1 - bb.y0) < min_area {
                    continue;
                }
                if shown >= 64 {
                    continue;
                }
                shown += 1;
                centers.push(((bb.x0 + bb.x1) / 2.0, (bb.y0 + bb.y1) / 2.0));
                let raw = fill.shape.bounding_box();
                let segs: Vec<String> = fill
                    .shape
                    .elements()
                    .iter()
                    .map(|el| format!("{el:?}"))
                    .collect();
                let [a, b, c, d, e, f] = fill.transform.as_coeffs();
                let clips: Vec<String> = stack
                    .iter()
                    .rev()
                    .take(3)
                    .map(|(kind, r)| {
                        format!("{kind}({:.0},{:.0})-({:.0},{:.0})", r.x0, r.y0, r.x1, r.y1)
                    })
                    .collect();
                eprintln!(
                    "FILLDETAIL idx={index} box=({:.0},{:.0})-({:.0},{:.0}) raw=({:.1},{:.1})-({:.1},{:.1}) segs={} brush={} xf=[{a:.2} {b:.2} {c:.2} {d:.2} {e:.1} {f:.1}] fill={:?} clips=[{}] path=[{}]",
                    bb.x0,
                    bb.y0,
                    bb.x1,
                    bb.y1,
                    raw.x0,
                    raw.y0,
                    raw.x1,
                    raw.y1,
                    fill.shape.elements().len(),
                    brush_text(&fill.brush),
                    fill.fill,
                    clips.join(" "),
                    segs.join(" ")
                );
            }
            _ => {}
        }
    }
    eprintln!("FILLDETAIL shown={shown}/big_fills commands={}", scene.commands.len());
    // The other half of the fingerprint: what DOM element actually sits under
    // the middle of each large fill (points outside the viewport are clamped).
    for (x, y) in centers {
        let x = x.clamp(1.0, f64::from(width) - 2.0) as f32;
        let y = y.clamp(1.0, f64::from(height) - 2.0) as f32;
        let Some(start) = doc.element_from_point(x, y) else {
            eprintln!("FILLDETAIPHIT {x},{y}: <nothing>");
            continue;
        };
        let mut names = Vec::new();
        let mut cur = Some(start);
        while let Some(id) = cur {
            let Some(node) = doc.get_node(id) else { break };
            if let Some(el) = node.element_data() {
                names.push(format!(
                    "{}{}",
                    el.name.local,
                    match el.id.as_ref().map(|a| &**a) {
                        Some(i) => format!("#{i}"),
                        None => match el.attr(blitz_dom::local_name!("class")) {
                            Some(c) if !c.is_empty() => format!(
                                ".{}",
                                c.split_whitespace().next().unwrap_or("")
                            ),
                            _ => String::new(),
                        },
                    }
                ));
            }
            cur = node.parent;
        }
        eprintln!("FILLDETAIPHIT {x},{y}: {}", names.join(" < "));
    }
}

/// Print every element whose layout box is bigger than 500x500 (or is an
/// `svg`/`canvas`/`video`/`img` bigger than 200x200) — the twin of a paint
/// histogram full of huge unexpected fills.
fn dump_big_boxes(doc: &blitz_html::HtmlDocument) {
    for (id, node) in doc.tree().iter() {
        let Some(el) = node.element_data() else { continue };
        let Some(rect) = doc.get_client_bounding_rect(id) else { continue };
        let tag = el.name.local.as_ref();
        let special = matches!(tag, "svg" | "canvas" | "video" | "img");
        let threshold = if special { 200.0 } else { 500.0 };
        if rect.width > threshold || rect.height > threshold {
            eprintln!(
                "BIGBOX {tag}.{}: x={:.0} y={:.0} w={:.0} h={:.0}",
                el.attr(blitz_dom::local_name!("class"))
                    .map(|c| c.split_whitespace().next().unwrap_or("").to_string())
                    .unwrap_or_default(),
                rect.x,
                rect.y,
                rect.width,
                rect.height
            );
        }
    }
}

fn dump_input_state(doc: &blitz_html::HtmlDocument) {
    fn walk(doc: &blitz_html::HtmlDocument, id: blitz_dom::NodeId) {
        let Some(node) = doc.get_node(id) else { return };
        if let Some(el) = node.element_data() {
            if el.name.local == blitz_dom::local_name!("input") {
                let checked = el.checkbox_input_checked().unwrap_or(false);
                eprintln!(
                    "INPUT id={} type={:?} checked={checked} attr_checked={}",
                    el.id.as_ref().map(|a| &**a).unwrap_or("-"),
                    el.attr(blitz_dom::local_name!("type")),
                    el.has_attr(blitz_dom::local_name!("checked"))
                );
            }
        }
        let children: Vec<blitz_dom::NodeId> = node.children.iter().copied().collect();
        for child in children {
            walk(doc, child);
        }
    }
    if let Some(body) = doc.find_body_node() {
        walk(doc, body.id);
    }
}

/// Poll the wake pipe for up to `tick`; true when a wake-up was pending.
fn poll_wake(wake_fd: RawFd, tick: Duration) -> bool {
    let mut pfd = libc::pollfd {
        fd: wake_fd,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one valid pollfd entry.
    let ready = unsafe { libc::poll(&mut pfd, 1, tick.as_millis() as i32) };
    ready > 0
}

/// Render the document into a fresh `vello_cpu` RGBA8 buffer at the full
/// viewport and write it as a P6 PPM; returns the on-disk file size.
fn shot_ppm(
    doc: &mut HtmlDocument,
    width: u32,
    height: u32,
    highlights: &[crate::find::Highlight],
    path: &Path,
) -> Result<u64, Box<dyn Error>> {
    let buffer = render_to_buffer::<VelloCpuImageRenderer, _>(
        |scene| {
            scene.fill(
                Fill::NonZero,
                Affine::IDENTITY,
                Color::from_rgb8(255, 255, 255),
                None,
                &Rect::from_origin_size(
                    (0.0, 0.0),
                    Size::new(f64::from(width), f64::from(height)),
                ),
            );
            paint_scene(scene, doc, 1.0, width, height, 0, 0);
            crate::find::paint(scene, highlights);
        },
        width,
        height,
    );
    let mut file = fs::File::create(path)?;
    write!(file, "P6\n{width} {height}\n255\n")?;
    let mut rgb = Vec::with_capacity((width * height * 3) as usize);
    for chunk in buffer.chunks_exact(4) {
        rgb.extend_from_slice(&chunk[..3]);
    }
    file.write_all(&rgb)?;
    file.flush()?;
    let len = file.metadata()?.len();
    Ok(len)
}

// ---------------------------------------------------------------------------
// body.innerHTML

/// The `<body>` element's inner HTML: a small serializer over the `blitz-dom`
/// node tree (blitz has no `innerHTML`), falling back to `textContent` when
/// the document has no body element.
pub fn body_html(doc: &HtmlDocument) -> String {
    inner_html(doc).unwrap_or_else(|| crate::js::body_text_snapshot(doc).unwrap_or_default())
}

/// Serialize the children of the `<body>` element; `None` without a body.
/// Delegates to the shared serializer in [`crate::js`] (same code the DOM
/// bridge's `innerHTML` uses).
pub fn inner_html(doc: &HtmlDocument) -> Option<String> {
    let body = doc.find_body_node()?;
    Some(crate::js::serialize_children(doc, body.id))
}

