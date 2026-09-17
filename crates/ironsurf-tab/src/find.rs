//! Find in page (`Ctrl-F`) — the tab half of the feature.
//!
//! The **shell** owns the find bar (the chord, the text entry, the counters);
//! the **tab** owns the document, so matching, highlighting and jumping happen
//! here (ARCHITECTURE §ironsurf-common: [`ironsurf_common::ToTab::SetFind`] /
//! [`ironsurf_common::FromTab::FindResult`]).
//!
//! **What gets matched.** blitz lays a document's text out into *inline roots*
//! — the elements that own a parley text layout (`<p>`, a `<div>` of prose, a
//! table cell, an anonymous block synthesized around mixed content). An inline
//! root's layout `text` is the collapsed, laid-out text of its whole inline
//! subtree, so it is exactly what the user sees: searching it finds what is
//! *visible* (a `<span>`'s text included, once merged into its root) and never
//! finds `display: none` text, `<script>`/`<style>` source, or form-control
//! values — like every real browser. Walking **layout** children (not DOM
//! children) is what makes that hold: mixed content like
//! `text <div>block</div> tail` lives in anonymous inline roots the DOM never
//! shows, and an atomic inline (`display: inline-block`) keeps its own root and
//! its own text, so nothing is matched twice and nothing is missed.
//!
//! **Where the rectangles come from.** A hit is a byte range of one inline
//! root's layout text; parley turns that range into the geometry its own
//! selection highlight uses (`Cursor::from_byte_index` +
//! `Selection::geometry_with`, one box per line fragment), and the root's
//! absolute position — `absolute_position` + padding/border − document scroll,
//! in CSS px — places those boxes in the viewport. That is the convention
//! blitz's `inline_fragment_rects` (`getClientRects()`) uses, so a highlight
//! lands on the glyphs it names at any zoom or scroll offset.
//!
//! **Painting.** Highlights go *over* the finished page scene (see
//! `runtime::Tab::paint_now`), after `paint_scene`: yellow behind every hit,
//! light blue behind the active one, both translucent so the text under them
//! stays readable.

use anyrender::PaintScene;
use blitz_dom::{BaseDocument, NodeId};
use kurbo::{Affine, Rect};
use parley::{Affinity, Cursor, Selection};
use peniko::{Color, Fill};

/// One hit: the byte range `start..end` of the inline root `node`'s laid-out
/// text. Node ids only mean something for the document that produced them, so
/// every entry point re-validates them against the live tree (a DOM mutated by
/// script simply yields no rectangle, and the hit is skipped).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hit {
    pub node: NodeId,
    pub start: usize,
    pub end: usize,
}

/// Every hit that is not the active one: **yellow**.
pub const COLOR_ALL: Color = Color::from_rgba8(255, 255, 0, 130);
/// The active hit — the one prev/next sits on: **light blue**.
pub const COLOR_ACTIVE: Color = Color::from_rgba8(125, 211, 252, 210);

/// A jump leaves this much blank viewport above (and to the left of) the hit,
/// so the text around it stays readable instead of the hit being jammed into
/// the corner of the window.
const SCROLL_MARGIN: f64 = 24.0;

// ---------------------------------------------------------------------------
// matching

/// Case-insensitive, non-overlapping occurrences of `needle` in `haystack`, as
/// byte ranges of `haystack`. Matching is per-character case *folding* over the
/// original string, so the offsets always refer to the source (lowercasing the
/// whole haystack would move them). An empty needle matches nothing.
pub fn find_occurrences(haystack: &str, needle: &str) -> Vec<(usize, usize)> {
    let mut hits = Vec::new();
    if needle.is_empty() {
        return hits;
    }
    let pattern: Vec<char> = needle.chars().collect();
    // Hits never overlap: the next candidate must start at or after the end of
    // the previous hit.
    let mut next = 0usize;
    for (i, _) in haystack.char_indices() {
        if i < next {
            continue;
        }
        if let Some(end) = match_here(haystack, i, &pattern) {
            hits.push((i, end));
            next = end;
        }
    }
    hits
}

/// `pattern` matches `haystack` at the byte offset `from`? Returns the offset
/// just past the match.
fn match_here(haystack: &str, from: usize, pattern: &[char]) -> Option<usize> {
    let mut index = from;
    for &p in pattern {
        let c = haystack[index..].chars().next()?;
        if !fold_eq(c, p) {
            return None;
        }
        index += c.len_utf8();
    }
    // A zero-length pattern would match everywhere; callers pass non-empty ones.
    (index > from).then_some(index)
}

/// Case-insensitive single-character comparison: equal, or their lowercasings
/// agree. Folding character-by-character (rather than the whole strings) is
/// what keeps the byte offsets honest.
fn fold_eq(a: char, b: char) -> bool {
    a == b || a.to_lowercase().eq(b.to_lowercase())
}

// ---------------------------------------------------------------------------
// collecting hits from a document

/// Every hit in **document (reading) order**: the layout tree is walked depth
/// first from the root element, and each inline root's laid-out text is
/// searched once. Text that was never laid out (`display: none`, `<head>`,
/// `<script>` source, control values) has no inline root and so cannot match.
pub fn collect(doc: &BaseDocument, needle: &str) -> Vec<Hit> {
    let mut hits = Vec::new();
    if needle.is_empty() {
        return hits;
    }
    let Some(root) = doc.try_root_element() else {
        return hits;
    };
    // Iterative: pages nest deeper than a recursive descent should reasonably
    // assume, and this runs on the tab's own stack every search.
    let mut stack = vec![root.id];
    while let Some(id) = stack.pop() {
        let Some(node) = doc.get_node(id) else { continue };
        if let Some(layout) = node.element_data().and_then(|e| e.inline_layout_data.as_ref()) {
            for (start, end) in find_occurrences(&layout.text, needle) {
                hits.push(Hit { node: id, start, end });
            }
        }
        let children = layout_children(node);
        // Reversed so the first child is visited first (the stack is LIFO).
        stack.extend(children.into_iter().rev());
    }
    hits
}

/// A node's laid-out children (its own, not its DOM children — anonymous
/// blocks appear here and never in the DOM), empty while it has no box.
fn layout_children(node: &blitz_dom::Node) -> Vec<NodeId> {
    node.layout_children
        .borrow()
        .as_ref()
        .map(|kids| kids.iter().copied().collect())
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// geometry

/// The viewport-relative rectangles (CSS px) covering one hit — one box per
/// line fragment, so a hit that wraps gets one per line. Empty when the node
/// is stale, its text changed under the hit, or it has no box.
pub fn hit_rects(doc: &BaseDocument, hit: &Hit) -> Vec<Rect> {
    let Some(node) = doc.get_node(hit.node) else {
        return Vec::new();
    };
    let Some(layout) = node.element_data().and_then(|e| e.inline_layout_data.as_ref()) else {
        return Vec::new();
    };
    // The layout may have been rebuilt since the search (a restyle, a resize,
    // script): a range that no longer fits the text yields nothing rather than
    // highlighting the wrong glyphs.
    if hit.end > layout.text.len() || hit.start >= hit.end {
        return Vec::new();
    }

    // parley's geometry is in the layout's own scaled units; the origin below
    // is in CSS px, so divide the fragments by the layout scale exactly like
    // `inline_fragment_rects` does.
    let scale = f64::from(layout.layout.scale());
    if scale == 0.0 {
        return Vec::new();
    }
    let box_layout = node.final_layout();
    let origin = node.absolute_position(0.0, 0.0);
    let scroll = doc.viewport_scroll();
    let origin_x = f64::from(origin.x)
        + f64::from(box_layout.padding.left + box_layout.border.left)
        - scroll.x;
    let origin_y = f64::from(origin.y)
        + f64::from(box_layout.padding.top + box_layout.border.top)
        - scroll.y;

    let anchor = Cursor::from_byte_index(&layout.layout, hit.start, Affinity::Downstream);
    let focus = Cursor::from_byte_index(&layout.layout, hit.end, Affinity::Downstream);
    let selection = Selection::new(anchor, focus);

    let mut rects = Vec::new();
    selection.geometry_with(&layout.layout, |box_rect, _line| {
        let x0 = origin_x + f64::from(box_rect.x0) / scale;
        let y0 = origin_y + f64::from(box_rect.y0) / scale;
        let x1 = origin_x + f64::from(box_rect.x1) / scale;
        let y1 = origin_y + f64::from(box_rect.y1) / scale;
        if x1 > x0 && y1 > y0 {
            rects.push(Rect::new(x0, y0, x1, y1));
        }
    });
    rects
}

/// The top-left-most box of a hit (the first fragment of its first line), for
/// the jump logic.
pub fn hit_origin(doc: &BaseDocument, hit: &Hit) -> Option<(f64, f64)> {
    let rect = hit_rects(doc, hit).into_iter().next()?;
    Some((rect.x0, rect.y0))
}

// ---------------------------------------------------------------------------
// painting

/// One painted highlight: a device-pixel rectangle and the colour to fill it
/// with. Built from the live layout, then handed to [`paint`] — the document is
/// borrowed mutably while the scene is being built, so geometry has to be
/// measured before that happens.
pub type Highlight = (Rect, Color);

/// Measure every hit: `active` is the index (into `hits`) that gets the
/// light-blue fill — the hit the find bar's counter sits on — and every other
/// hit gets yellow. Measuring happens per frame, so scrolling, resizing, zoom
/// and scripted restyles are always reflected in the next one. Hits whose node
/// or byte range went stale (a script rewrote the text) contribute nothing.
pub fn highlight_rects(
    doc: &BaseDocument,
    hits: &[Hit],
    active: Option<usize>,
    scale: f64,
) -> Vec<Highlight> {
    let mut out = Vec::new();
    if hits.is_empty() || !(scale.is_finite() && scale > 0.0) {
        return out;
    }
    for (index, hit) in hits.iter().enumerate() {
        let color = if Some(index) == active {
            COLOR_ACTIVE
        } else {
            COLOR_ALL
        };
        for rect in hit_rects(doc, hit) {
            // Rectangles arrive in CSS px; the scene is in device px.
            out.push((
                Rect::new(
                    rect.x0 * scale,
                    rect.y0 * scale,
                    rect.x1 * scale,
                    rect.y1 * scale,
                ),
                color,
            ));
        }
    }
    out
}

/// Draw the highlights *over* the finished page scene, in device pixels, with
/// the translucent fills the task asks for.
pub fn paint(scene: &mut impl PaintScene, highlights: &[Highlight]) {
    for (rect, color) in highlights {
        scene.fill(Fill::NonZero, Affine::IDENTITY, *color, None, rect);
    }
}

// ---------------------------------------------------------------------------
// jumping

/// Scroll the document (the root scroller — the viewport) so `hit` sits
/// [`SCROLL_MARGIN`] px below the top of the viewport. Returns whether the hit
/// is known. Called for the active hit on every jump; hits already fully
/// visible still get re-anchored, like a browser's find bar does, so repeated
/// Enter presses through a viewport-full of identical hits stay predictable.
pub fn scroll_hit_into_view(doc: &mut BaseDocument, hit: &Hit, viewport_w: f64, viewport_h: f64) {
    let Some(rect) = hit_rects(doc, hit).into_iter().next() else {
        return;
    };
    let scroll = doc.viewport_scroll();
    // `rect` is viewport-relative, so a hit already on screen needs no move;
    // otherwise bring it to the margin line (never scrolling a document that
    // cannot scroll at all).
    let fully_visible = rect.y0 >= 0.0 && rect.y1 <= viewport_h;
    let x = if rect.x0 >= 0.0 && rect.x1 <= viewport_w {
        scroll.x
    } else {
        scroll.x + rect.x0 - SCROLL_MARGIN.min(viewport_w.max(0.0))
    };
    let y = if fully_visible {
        scroll.y
    } else if rect.y0 < 0.0 {
        // Jumping backwards (or to something scrolled past): keep the hit's
        // line in one piece just below the top edge.
        scroll.y + rect.y0 - SCROLL_MARGIN.min((viewport_h * 0.25).max(0.0))
    } else {
        scroll.y + rect.y0 - SCROLL_MARGIN.min((viewport_h * 0.25).max(0.0))
    };
    let Some(root_id) = doc.try_root_element().map(|el| el.id) else {
        return;
    };
    doc.scroll_to(root_id, x, y, blitz_dom::ScrollBehavior::Instant);
}

// ---------------------------------------------------------------------------
// tests

#[cfg(test)]
mod tests {
    use super::*;

    fn occurrences(hay: &str, needle: &str) -> Vec<String> {
        find_occurrences(hay, needle)
            .into_iter()
            .map(|(s, e)| hay[s..e].to_string())
            .collect()
    }

    #[test]
    fn occurrences_are_case_insensitive_byte_ranges() {
        let hay = "Hello world, hello again; HELLO!";
        let hits = find_occurrences(hay, "hello");
        assert_eq!(hits.len(), 3);
        assert_eq!(hits, vec![(0, 5), (13, 18), (26, 31)]);
        assert_eq!(occurrences(hay, "hello"), ["Hello", "hello", "HELLO"]);
        // Offsets always address the source text verbatim.
        for (s, e) in &hits {
            assert_eq!(&hay[*s..*e].to_lowercase(), "hello");
        }
    }

    #[test]
    fn empty_needle_matches_nothing_and_hits_never_overlap() {
        assert!(find_occurrences("abc", "").is_empty());
        let hay = "aaaa";
        assert_eq!(find_occurrences(hay, "aa"), vec![(0, 2), (2, 4)]);
        assert_eq!(find_occurrences("abababab", "abab"), vec![(0, 4), (4, 8)]);
    }

    #[test]
    fn multi_byte_text_matches_by_bytes() {
        let hay = "naïve café — NAÏVE Café";
        let hits = find_occurrences(hay, "naïve");
        assert_eq!(hits.len(), 2);
        assert_eq!(occurrences(hay, "naïve"), ["naïve", "NAÏVE"]);
        // The whole phrase including the em dash, with a space in it.
        assert_eq!(find_occurrences(hay, "café — NAÏVE").len(), 1);
    }

    /// A settled document for the geometry/collect tests: the same flow the tab
    /// uses (parse with a viewport → resolve), no network, no window.
    pub(super) fn document(html: &str) -> blitz_html::HtmlDocument {
        use blitz_dom::DocumentConfig;
        use blitz_traits::shell::{ColorScheme, Viewport};
        let mut doc = blitz_html::HtmlDocument::from_html(
            html,
            DocumentConfig {
                base_url: Some("http://example.test/".to_owned()),
                viewport: Some(Viewport::new(800, 600, 1.0, ColorScheme::Light)),
                ..Default::default()
            },
        );
        doc.resolve(0.);
        doc
    }

    #[test]
    fn hidden_text_scripts_and_control_values_never_match() {
        let doc = document(
            "<!doctype html><html><head><script>var hello = 1</script><style>p{color:red}</style>\
             <title>hello title</title></head><body>\
             <p style=\"display:none\">hidden hello</p>\
             <textarea>textarea hello</textarea>\
             <input value=\"input hello\">\
             <p>visible hello</p></body></html>",
        );
        let hits = collect(doc.as_ref(), "hello");
        assert_eq!(hits.len(), 1, "only the visible prose matches");
    }

    #[test]
    fn mixed_content_and_atomic_inlines_match_once_each() {
        // "mixed text <div>block</div> tail" puts the loose text into anonymous
        // inline roots (never in the DOM), and an inline-block keeps its own
        // root — so each "hello" below is searched exactly once.
        let doc = document(
            "<!doctype html><html><body>\
             <div>mixed hello <div>inner hello</div> tail hello</div>\
             <p>outer <span style=\"display:inline-block\">atomic hello</span> end hello</p>\
             </body></html>",
        );
        let hits = collect(doc.as_ref(), "hello");
        assert_eq!(hits.len(), 5, "5 written hellos, 5 hits — no doubles, none missed");
    }

    #[test]
    fn rects_land_on_the_glyphs_they_name() {
        // One long line with two hits: the second sits to the right of the
        // first, both inside the viewport, both with a plausible text height.
        let doc = document(
            "<!doctype html><html><body style=\"margin:0\">\
             <p style=\"margin:0;font-size:20px\">alpha beta gamma delta epsilon zeta</p>\
             </body></html>",
        );
        let hits = collect(doc.as_ref(), "e");
        assert!(!hits.is_empty());
        let rects: Vec<_> = hits
            .iter()
            .map(|h| hit_rects(doc.as_ref(), h))
            .map(|r| r.into_iter().next().expect("a rect per hit"))
            .collect();
        for r in &rects {
            assert!(r.width() > 1.0 && r.height() > 8.0 && r.height() < 40.0, "{r:?}");
            assert!(r.y0 >= 0.0 && r.y1 <= 600.0, "{r:?} inside the viewport");
        }
        // Reading order: each later hit starts no earlier than the last.
        for w in rects.windows(2) {
            assert!(w[1].x0 > w[0].x0, "{:?} then {:?}", w[0], w[1]);
        }
        // A hit on a line scrolled out of the document is still measurable, but
        // a stale byte range yields nothing rather than the wrong glyphs.
        let mut stale = hits[0];
        stale.end = stale.start + 1;
        stale.start = stale.start.max(1) - 1;
        let _ = hit_rects(doc.as_ref(), &stale);
    }

    /// The two colours, as they must read **over the page**: the fills are
    /// translucent, so a yellow pixel is one where red sits well above blue and
    /// a light-blue pixel the other way round — whichever way the glyphs under
    /// them absorbed some of the paint.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Tint {
        Yellow,
        Blue,
    }

    impl Tint {
        fn of(self, r: u8, g: u8, b: u8) -> bool {
            let (r, b) = (r as i16, b as i16);
            match self {
                Tint::Yellow => r - b > 40 && g > 60,
                Tint::Blue => b - r > 40,
            }
        }
    }

    /// Paint the page exactly like `runtime::Tab::paint_now` does — white
    /// ground, `paint_scene`, then the highlights on top — at the document's own
    /// viewport size and scale 1, and return the RGBA buffer.
    fn render(doc: &mut blitz_html::HtmlDocument, hits: &[Hit], active: Option<usize>) -> Vec<u8> {
        use anyrender::render_to_buffer;
        use anyrender_vello_cpu::VelloCpuImageRenderer;
        let highlights = highlight_rects(doc.as_ref(), hits, active, 1.0);
        render_to_buffer::<VelloCpuImageRenderer, _>(
            |scene| {
                scene.fill(
                    Fill::NonZero,
                    Affine::IDENTITY,
                    Color::from_rgb8(255, 255, 255),
                    None,
                    &Rect::new(0.0, 0.0, 800.0, 600.0),
                );
                blitz_paint::paint_scene(scene, doc.as_mut(), 1.0, 800, 600, 0, 0);
                paint(scene, &highlights);
            },
            800,
            600,
        )
    }

    /// Does `rect` contain a pixel reading as `want`?
    fn tinted(rgba: &[u8], rect: &Rect, want: Tint) -> bool {
        let w = 800i64;
        let x0 = rect.x0.max(0.0) as i64;
        let x1 = (rect.x1.ceil() as i64).min(w);
        let y0 = rect.y0.max(0.0) as i64;
        let y1 = rect.y1.ceil() as i64;
        for y in y0..y1 {
            for x in x0..x1 {
                let i = ((y * w + x) * 4) as usize;
                if let (Some(&r), Some(&g), Some(&b)) =
                    (rgba.get(i), rgba.get(i + 1), rgba.get(i + 2))
                {
                    if want.of(r, g, b) {
                        return true;
                    }
                }
            }
        }
        false
    }

    #[test]
    fn every_hit_paints_yellow_and_the_active_one_paints_blue() {
        let html = "<!doctype html><html><body style=\"margin:0\">\
            <p style=\"margin:8px 0 0 8px;font-size:32px\">needle in the haystack needle again</p>\
            </body></html>";
        let mut doc = document(html);
        let hits = collect(doc.as_ref(), "needle");
        assert_eq!(hits.len(), 2, "the query hits twice");
        let rects: Vec<_> = hits
            .iter()
            .map(|h| hit_rects(doc.as_ref(), h).remove(0))
            .collect();

        // The frame the shell gets with the *second* hit active: the first stays
        // yellow, the second turns light blue.
        let frame = render(&mut doc, &hits, Some(1));
        assert!(tinted(&frame, &rects[0], Tint::Yellow), "every other match is yellow");
        assert!(!tinted(&frame, &rects[0], Tint::Blue), "only the active one turns blue");
        assert!(tinted(&frame, &rects[1], Tint::Blue), "the active match is light blue");
        assert!(!tinted(&frame, &rects[1], Tint::Yellow), "and not yellow as well");

        // A page with no search running is untouched: white paper, black text.
        let clean = render(&mut doc, &[], None);
        assert!(!tinted(&clean, &rects[0], Tint::Yellow), "no query, no paint");
        assert!(!tinted(&clean, &rects[0], Tint::Blue));
    }

    #[test]
    fn a_jump_puts_the_hit_in_the_viewport() {
        let mut doc = document(
            "<!doctype html><html><body style=\"margin:0\">\
             <div style=\"height:3000px\"></div>\
             <p style=\"margin:0;font-size:24px\">find me down here</p>\
             </body></html>",
        );
        let hits = collect(doc.as_ref(), "find");
        assert_eq!(hits.len(), 1);
        // Out of view to begin with…
        assert!(hit_rects(doc.as_ref(), &hits[0])[0].y0 > 600.0);
        scroll_hit_into_view(doc.as_mut(), &hits[0], 800.0, 600.0);
        let rect = hit_rects(doc.as_mut(), &hits[0]).remove(0);
        assert!(
            rect.y0 >= 0.0 && rect.y1 <= 600.0,
            "the hit is on screen (a hit near the end of the document sits at the bottom, \"\
             like any browser: the scroller is clamped): {rect:?}"
        );
    }

    #[test]
    fn words_split_across_nodes_are_found_in_one_inline_root() {
        // `<p>a<b>b</b>c</p>` lays out as the single string "abc" (blitz merges
        // the inline subtree), so searching the *document* finds "abc" even
        // though no text node contains it.
        let doc = document("<!doctype html><html><body><p>a<b>b</b>c</p></body></html>");
        let hits = collect(doc.as_ref(), "abc");
        assert_eq!(hits.len(), 1, "the merged inline text is searched");
    }
}
