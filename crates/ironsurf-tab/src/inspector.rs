//! DevTools Inspector — the tab half of F12's HTML tree and "inspect element".
//!
//! The **shell** owns the pane (the tree view, the breadcrumbs, the F12 chord,
//! the context-menu item); the **tab** owns the document, so the tree, the ids,
//! the highlight and every hit-test happen here (ARCHITECTURE §"Developer tools
//! (F12 / Ctrl-Shift-I)" and
//! [`ironsurf_common::ToTab::SetInspect`] / [`ironsurf_common::FromTab::InspectSnapshot`]).
//!
//! **Ids and generations.** The shell addresses nodes by [`InspectNode::id`]:
//! an index into the flat pre-order list of the snapshot it arrived with. Ids
//! are minted `0,1,2…` in document order and die with that snapshot — the
//! runtime keeps the blitz node behind each id ([`Walk::nodes`]) for as long as
//! the generation lives, which is what makes selecting, hit-testing and painting
//! the highlight cheap: **no tree walk per frame, ever**.
//!
//! **What gets a row.** Exactly what the `--body-out`/`innerHTML` serializer
//! shows ([`crate::js::serialize_node`]): elements, text and comments, with
//! layout-only anonymous blocks *transparent* (their children promote to their
//! parent row), plus the two nodes a serializer has no syntax for — the document
//! itself (the single `depth 0` root row) and the doctype. Whitespace-only text
//! is dropped, because a pretty-printed page would otherwise be mostly blank
//! rows.
//!
//! **Geometry.** `bounds` come from [`BaseDocument::get_client_bounding_rect`] —
//! viewport-relative **CSS px** with no zoom applied, and the union of
//! per-line-box fragments for a non-atomic inline element, so `<span>`/`<a>`/`<b>`
//! get a box the way `getClientRects()` gives them one. They are `None` for
//! anything without a box (`display: none`, `display: contents`, a never-laid-out
//! `<head>` subtree, an element collapsed to nothing) and for every non-element
//! kind. The *highlight* is re-measured from the stored node every
//! frame ([`marker`]), so scrolling and scripted restyles move the box without a
//! new snapshot.
//!
//! ## The doctype gap
//!
//! blitz's parser does not keep the `<!DOCTYPE …>` node — `NodeData` has no
//! `Doctype` variant (it is commented out upstream, in
//! `blitz-dom/src/node/node.rs`). The contract has one, so the runtime sniffs it
//! out of the source it hands the parser ([`doctype_from_source`]) and passes it
//! to the walk, which puts the row where the parser had it: the first child of
//! the document node.

use anyrender::PaintScene;
use blitz_dom::node::NodeData;
use blitz_dom::{BaseDocument, Node, NodeId};
use ironsurf_common::{
    InspectBox, InspectKind, InspectNode, InspectSnapshot, MAX_INSPECT_ATTRS,
    MAX_INSPECT_ATTR_VALUE, MAX_INSPECT_NODES, MAX_INSPECT_TEXT,
};
use kurbo::{Affine, Rect};
use peniko::{Color, Fill};

/// The inspected element's content box, filled with this over the page.
pub const COLOR_FILL: Color = Color::from_rgba8(255, 147, 38, 110);
/// …and ringed by a [`OUTLINE_PX`] band of this, so a box with an amber
/// background of its own stays distinguishable from the selection.
pub const COLOR_OUTLINE: Color = Color::from_rgba8(255, 213, 0, 220);
/// Highlight outline thickness, in **device** px (thin enough to stay a hairline
/// at 1.0, thick enough to see against any page colour).
const OUTLINE_PX: f64 = 2.0;

// ---------------------------------------------------------------------------
// the walk

/// One row, as the traversal decided it: identity only. Field values (tag,
/// attributes, text, bounds) are filled in by the caller that wants a snapshot;
/// the callers that only count rows (see [`id_of_node`]) never pay for them.
struct Entry {
    /// The blitz node behind the row — `None` only for the doctype, which has
    /// no node (see the module's "doctype gap").
    node: Option<NodeId>,
    kind: InspectKind,
    id: u64,
    parent: Option<u64>,
    depth: usize,
}

/// Pre-order traversal of the DOM tree, one [`Entry`] per row the Inspector
/// shows, ids minted from 0 in document order.
///
/// `on_row` returns `false` to stop the walk (that is how the
/// [`MAX_INSPECT_NODES`] cap bites); the return value says whether the walk ran
/// to the end, i.e. `truncated = !walked_to_the_end`.
///
/// Row rules, shared by every caller so a pick can never name a row the
/// snapshot does not carry:
///
/// * the document node → [`InspectKind::Document`], the one root (`depth 0`);
/// * `doctype` (when the runtime recovered one) → [`InspectKind::Doctype`], the
///   document's first child;
/// * elements → [`InspectKind::Element`]; comments → [`InspectKind::Comment`];
/// * text → [`InspectKind::Text`], unless it is whitespace-only (dropped);
/// * anonymous blocks (synthesised by layout, and by `<video>`-alike widgets)
///   are **transparent**: their children promote to their parent row, at their
///   parent's depth, exactly like the `innerHTML` serializer treats them;
/// * a nested document (an iframe's) is transparent too — the pane shows the
///   outer tree, and the iframe element itself.
fn traverse(
    doc: &BaseDocument,
    doctype: Option<&str>,
    on_row: &mut dyn FnMut(&Entry) -> bool,
) -> bool {
    let root = doc.root_node();
    let mut emitted = 0u64;
    let mut emit = |entry: Entry| -> bool {
        if emitted as usize >= MAX_INSPECT_NODES {
            return false;
        }
        emitted += 1;
        on_row(&entry)
    };

    // The root row: always the first, and never capped away (the cap is far
    // larger than one row).
    if !emit(Entry {
        node: Some(root.id),
        kind: InspectKind::Document,
        id: 0,
        parent: None,
        depth: 0,
    }) {
        return false;
    }
    let root_id = 0u64;
    let mut next = 1u64;
    if doctype.is_some() {
        let entry = Entry {
            node: Some(root.id),
            kind: InspectKind::Doctype,
            id: next,
            parent: Some(root_id),
            depth: 1,
        };
        next += 1;
        if !emit(entry) {
            return false;
        }
    }

    // (node, depth it would report, row id of its nearest element-or-root
    // ancestor). Seeded with the root's children, since the root was emitted
    // above; pushed in reverse so the stack yields document order.
    let mut stack: Vec<(NodeId, usize, Option<u64>)> = push_children(root, 1, Some(root_id), Vec::new());
    while let Some((id, depth, parent)) = stack.pop() {
        let Some(node) = doc.get_node(id) else {
            continue;
        };
        let kind = match &node.data {
            NodeData::Element(_) => Some(InspectKind::Element),
            NodeData::Comment { .. } => Some(InspectKind::Comment),
            NodeData::Text(text) => {
                // Dropped whole, not just trimmed: `<p> </p>` is a row for the
                // `<p>` and nothing else.
                if text.content.trim().is_empty() {
                    None
                } else {
                    Some(InspectKind::Text)
                }
            }
            NodeData::AnonymousBlock(_) => None,
            // A second document node is an iframe's: transparent.
            NodeData::Document(_) if id != root.id => None,
            NodeData::Document(_) => Some(InspectKind::Document),
        };
        match kind {
            Some(kind) => {
                let entry = Entry {
                    node: Some(id),
                    kind,
                    id: next,
                    parent,
                    depth,
                };
                next += 1;
                let row_id = entry.id;
                if !emit(entry) {
                    return false;
                }
                stack = push_children(node, depth + 1, Some(row_id), stack);
            }
            None => {
                stack = push_children(node, depth, parent, stack);
            }
        }
    }
    true
}

/// Push `node`'s children onto `stack` (reverse order, so the next `pop()` is
/// the first child) tagged with the depth/parent the traversal decided for them.
fn push_children(
    node: &Node,
    depth: usize,
    parent: Option<u64>,
    mut stack: Vec<(NodeId, usize, Option<u64>)>,
) -> Vec<(NodeId, usize, Option<u64>)> {
    stack.extend(node.children.iter().rev().copied().map(|id| (id, depth, parent)));
    stack
}

/// A tree walk: the snapshot to ship, plus the blitz node behind every row —
/// `nodes[i]` belongs to `snapshot.nodes[i]`. The runtime keeps `nodes` as the
/// node handles of the generation it just sent (see [`node_of`]).
pub struct Walk {
    pub snapshot: InspectSnapshot,
    pub nodes: Vec<NodeId>,
}

/// Walk `doc` into a full [`InspectSnapshot`].
///
/// `generation` is the runtime's navigation counter: the shell is told (via the
/// ids it holds) that they belong to it alone. `selected` is the id of the
/// node to single out, if any — this function does not decide that, it stamps
/// it. `doctype` is the recovered doctype text (see the module's "doctype gap").
pub fn walk(
    doc: &BaseDocument,
    url: &str,
    generation: u64,
    selected: Option<u64>,
    doctype: Option<&str>,
) -> Walk {
    let mut nodes = Vec::new();
    let mut rows: Vec<InspectNode> = Vec::new();
    let walked_to_the_end = traverse(doc, doctype, &mut |entry| {
        let node = entry.node.and_then(|id| doc.get_node(id));
        rows.push(row(doc, node, entry, doctype));
        // Every row names a node; the doctype row borrows the document's, so
        // the handle list stays indexable by id.
        nodes.push(entry.node.unwrap_or(doc.root_node().id));
        true
    });
    let truncated = !walked_to_the_end;
    // A `selected` id the cap removed is not carried over: the shell's
    // selection would point at nothing.
    let selected = selected.filter(|id| (*id as usize) < rows.len());
    Walk {
        snapshot: InspectSnapshot {
            generation,
            url: url.to_string(),
            selected,
            truncated,
            nodes: rows,
        },
        nodes,
    }
}

/// [`walk`] with the full row set: the whole tree as an [`InspectSnapshot`].
///
/// A runtime that ships the tree every tick uses `walk` instead, because it
/// also wants the id table it built; this is the one-call entry point a tool has
/// when it only wants the rows. `doctype` is the recovered doctype text (see
/// [`doctype_from_source`]) — `None` and the doctype row is absent.
#[allow(dead_code)] // the runtime calls `walk`; this is the tool-side entry point
pub fn snapshot_with_doctype(
    doc: &BaseDocument,
    url: &str,
    generation: u64,
    selected: Option<u64>,
    doctype: Option<&str>,
) -> InspectSnapshot {
    walk(doc, url, generation, selected, doctype).snapshot
}

/// Render one row from the traversal's decision plus the node behind it.
/// `doctype` is the recovered doctype text, which is the content of the one row
/// that has no node.
fn row(doc: &BaseDocument, node: Option<&Node>, entry: &Entry, doctype: Option<&str>) -> InspectNode {
    let mut attrs = Vec::new();
    let mut tag = String::new();
    let mut text = String::new();
    let mut bounds = None;
    if let Some(node) = node {
        match &node.data {
            NodeData::Element(element) => {
                tag = qualified(element.name.prefix.as_ref(), &element.name.local);
                if let Some(list) = node.data.attrs() {
                    for attr in list.iter().take(MAX_INSPECT_ATTRS) {
                        attrs.push((
                            qualified(attr.name.prefix.as_ref(), &attr.name.local),
                            cap_chars(&attr.value, MAX_INSPECT_ATTR_VALUE),
                        ));
                    }
                }
                bounds = bounds_of(doc, node);
            }
            NodeData::Text(content) => text = display_text(&content.content),
            NodeData::Comment { contents } => text = display_text(contents),
            _ => {}
        }
    }
    if entry.kind == InspectKind::Doctype {
        text = display_text(doctype.unwrap_or_default());
    }
    InspectNode {
        id: entry.id,
        parent: entry.parent,
        depth: entry.depth,
        kind: entry.kind,
        tag,
        attrs,
        text,
        bounds,
    }
}

/// The name of an element or attribute: `local`, with its `xlink:`-style prefix
/// in foreign content, where the prefix is part of the name. HTML element and
/// attribute names reach blitz already lower-cased (the tokenizer lower-cases
/// them), while foreign content keeps its real spelling — which is exactly the
/// rule the contract asks for, so the name is passed through as blitz has it.
fn qualified(prefix: Option<&impl std::fmt::Display>, local: &impl std::fmt::Display) -> String {
    match prefix.map(|prefix| prefix.to_string()) {
        Some(prefix) if !prefix.is_empty() => format!("{prefix}:{local}"),
        _ => local.to_string(),
    }
}

// ---------------------------------------------------------------------------
// text and attribute values

/// Whitespace runs collapse to one space and the ends are trimmed (what a tree
/// pane shows is the laid-out text, and the pane renders rows in a column), then
/// the result is cut to [`MAX_INSPECT_TEXT`] chars with a trailing `…`.
fn display_text(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut pending_space = false;
    for c in raw.chars() {
        if c.is_whitespace() {
            pending_space = true;
            continue;
        }
        if pending_space && !out.is_empty() {
            out.push(' ');
        }
        pending_space = false;
        out.push(c);
    }
    cap_chars(&out, MAX_INSPECT_TEXT)
}

/// Cut to `limit` **chars** (values are UTF-8, and a cut must land on a char
/// boundary), marking the cut with the contract's `…`.
fn cap_chars(s: &str, limit: usize) -> String {
    match s.char_indices().nth(limit) {
        Some((at, _)) => format!("{}…", &s[..at]),
        None => s.to_string(),
    }
}

/// The doctype **name** recovered from the source a document was parsed from —
/// `html` for `<!DOCTYPE html>`, which is what [`InspectNode::text`] carries on
/// the doctype row.
///
/// blitz's parser throws the node away (module docs), so this is the only way the
/// pane can show a doctype at all. `None` for a source with none (quirks mode) is
/// what makes the row absent — the contract's "when the parser kept one". The
/// tokenizer recognises a DOCTYPE only at the very start of the source (a
/// `<!doctype>` after any other content is a bogus comment), so sniff it there:
/// past a byte-order mark and whitespace, read the directive up to the `>` that is
/// not inside a quoted public/system identifier, then take the name — its first
/// token, lower-cased. A legacy
/// `<!DOCTYPE html PUBLIC "-//W3C//DTD HTML 4.01//EN" …>` has no field in the
/// contract for the identifiers to live in, so it reads as `html` too; a directive
/// with no name at all (`<!DOCTYPE>`) has nothing to show and is `None`.
///
/// The scan reads the whole directive before the name is taken, because a quoted
/// identifier may contain `>`: stopping at the first `>` would mis-read
/// `<!DOCTYPE html PUBLIC "…/y">` — taking a name out of what is still the
/// doctype's own text.
pub fn doctype_from_source(html: &str) -> Option<String> {
    let rest = html.trim_start_matches(['\u{feff}']).trim_start();
    let body = rest
        .strip_prefix("<!")
        .and_then(|rest| rest.strip_prefix("DOCTYPE").or_else(|| rest.strip_prefix("doctype")))?;
    let mut quote: Option<char> = None;
    let mut end = None;
    for (index, c) in body.char_indices() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None if c == '"' || c == '\'' => quote = Some(c),
            None if c == '>' => {
                end = Some(index);
                break;
            }
            None => {}
        }
    }
    let end = end?;
    // The name is the directive's first token; whatever legacy identifiers
    // follow it (`PUBLIC …`, `SYSTEM …`) has no field to live in.
    let name = body[..end]
        .trim()
        .split([' ', '\t', '\n', '\r', '\x0c'])
        .next()
        .unwrap_or_default()
        .trim_end_matches('[')
        .to_ascii_lowercase();
    (!name.is_empty()).then_some(name)
}

// ---------------------------------------------------------------------------
// bounds

/// The inspected box of an element: viewport-relative **CSS px**, page zoom not
/// applied — the space of `Element.getBoundingClientRect()`, and derived exactly
/// as [`crate::find`] derives its rects (`absolute_position` − `viewport_scroll`).
///
/// `None` for a node with no box: `display: none`, `display: contents`, a
/// subtree the layout never reached (`<head>`), a detached node, and every
/// non-element kind.
fn bounds_of(doc: &BaseDocument, node: &Node) -> Option<InspectBox> {
    let rect = box_rect(doc, node)?;
    Some(InspectBox {
        x: rect.x0 as f32,
        y: rect.y0 as f32,
        width: rect.width() as f32,
        height: rect.height() as f32,
    })
}

/// The **border box** of a laid-out node in CSS px (see [`bounds_of`]).
///
/// This is [`BaseDocument::get_client_bounding_rect`] — the engine's own answer,
/// which is what makes inline content work: a non-atomic inline element (`<span>`,
/// `<a>`, `<b>`, `<em>`) has no layout box of its own, being flattened into its
/// inline root's text layout, so `final_layout().size` is `0×0` and a hand-rolled
/// `absolute_position` + size reports nothing for the very elements "inspect
/// element" is most used on. The engine unions the node's per-line-box fragments
/// (`getClientRects()` semantics) instead, so link text and emphasised words get
/// a real box to highlight — one box per line when they wrap.
pub fn box_rect(doc: &BaseDocument, node: &Node) -> Option<Rect> {
    // `element_data()` is blitz's "this is a real element with a box model"
    // test: it is `None` for text/comments. `has_boxes()` then drops
    // `display: none`, `display: contents` and a never-laid-out `<head>`
    // subtree.
    node.element_data()?;
    if !node.has_boxes() {
        return None;
    }
    // The engine's own answer for the node's extents — the union of its
    // per-line-box fragments for a non-atomic inline element, which is the only
    // way a `<span>` has extents at all.
    let extents = doc.get_client_bounding_rect(node.id)?;
    // An element the page has collapsed to nothing has no visible box to mark,
    // and the contract's bounds are for a *non-empty* box.
    if !(extents.width > 0.0 && extents.height > 0.0) {
        return None;
    }

    // The engine derives that rect from layout alone, so it does not know about
    // the document scroll this app applies through the root scroller — which is
    // why `crate::find` takes origins from `absolute_position` instead. So take
    // the *place* from the nearest box in the flow that has real extents (for a
    // block element that is the element itself, for inline content the block
    // holding its text), and add where the node sits inside it as the engine
    // sees it. Both views subtract `viewport_scroll`, so the difference is the
    // offset within the flow, and the place below is the one that moves.
    let mut anchor: &Node = node;
    loop {
        let layout = anchor.final_layout();
        if layout.size.width > 0.0 && layout.size.height > 0.0 {
            break;
        }
        // The *flow* parent, which is what a box sits in (an inline span sits in
        // the block that lays out its text, whatever its DOM parent is).
        anchor = doc.get_node(anchor.layout_parent.get()?)?;
    }
    let within = doc
        .get_client_bounding_rect(anchor.id)
        .map(|anchor| (extents.x - anchor.x, extents.y - anchor.y))
        .unwrap_or((0.0, 0.0));
    let origin = anchor.absolute_position(0.0, 0.0);
    let scroll = doc.viewport_scroll();
    let x0 = f64::from(origin.x) - scroll.x + within.0;
    let y0 = f64::from(origin.y) - scroll.y + within.1;
    Some(Rect::new(
        x0,
        y0,
        x0 + f64::from(extents.width),
        y0 + f64::from(extents.height),
    ))
}

/// The **content box** inside a border box (inset by the element's border and
/// padding) — the area the fill goes into, so the highlight covers what the
/// element holds rather than the space its margins claim.
fn content_rect(node: &Node, border: Rect) -> Rect {
    let layout = node.final_layout();
    let left = f64::from(layout.border.left + layout.padding.left);
    let top = f64::from(layout.border.top + layout.padding.top);
    let right = f64::from(layout.border.right + layout.padding.right);
    let bottom = f64::from(layout.border.bottom + layout.padding.bottom);
    Rect::new(
        border.x0 + left,
        border.y0 + top,
        (border.x1 - right).max(border.x0 + left),
        (border.y1 - bottom).max(border.y0 + top),
    )
}

// ---------------------------------------------------------------------------
// ids and hit-testing

/// The node behind a row id of the current generation (`nodes` is the handle
/// list a [`Walk`] returned). Out of range is simply `None` — a stale id is not
/// an error anywhere in this feature.
pub fn node_of(nodes: &[NodeId], id: u64) -> Option<NodeId> {
    nodes.get(usize::try_from(id).ok()?).copied()
}

/// The row id of `target` — the same count [`walk`] mints, so a pick can never
/// name a row the snapshot does not carry (a `target` the [`MAX_INSPECT_NODES`]
/// cap walked past has no id, exactly as it has no row).
pub fn id_of_node(doc: &BaseDocument, target: NodeId, doctype: Option<&str>) -> Option<u64> {
    let mut found = None;
    traverse(doc, doctype, &mut |entry| {
        if entry.node == Some(target) {
            found = Some(entry.id);
            false
        } else {
            true
        }
    });
    found
}

/// "Inspect element" for a point: the row id of the **deepest element** at
/// viewport CSS px `(x, y)` — the same space as [`ironsurf_common::ToTab::Pointer`]
/// once the page zoom has been divided out (see `runtime::Tab::inspect_hit`),
/// i.e. exactly `document.elementFromPoint()`.
///
/// This *is* blitz's hit-test (the one `js.rs`'s `elementFromPoint`, the status
/// bar and the cursor code use): anonymous boxes and text resolve to their
/// nearest element, a point over the page background answers the root element,
/// and a point outside the viewport answers `None`.
///
/// `doctype` must be the same text the snapshot was walked with — the doctype row
/// shifts every id by one, which is precisely why the hit and the tree cannot be
/// computed by two different rules.
pub fn hit_element_at_with(
    doc: &BaseDocument,
    x: f32,
    y: f32,
    doctype: Option<&str>,
) -> Option<u64> {
    let hit = doc.element_from_point(x, y)?;
    id_of_node(doc, hit, doctype)
}

// ---------------------------------------------------------------------------
// painting the highlight

/// One painted marker: a device-pixel rectangle and the colour to fill it with.
/// Same idiom as [`crate::find::Highlight`] — geometry is measured before the
/// scene is built, because the document is borrowed mutably while painting.
pub type Highlight = (Rect, Color);

/// Measure one node's marker: its content box filled with [`COLOR_FILL`], then
/// a [`OUTLINE_PX`] band of [`COLOR_OUTLINE`] around its border box (four thin
/// rects, because the scene has no stroke to ask for). `scale` is the device
/// scale the scene is being painted at.
///
/// A node with no box (a text/comment row, something `display: none`, something
/// the script removed since the snapshot) contributes nothing, so a stale
/// selection is invisible rather than wrong.
pub fn marker(doc: &BaseDocument, node: NodeId, scale: f64) -> Vec<Highlight> {
    let mut out = Vec::new();
    if !(scale.is_finite() && scale > 0.0) {
        return out;
    }
    let Some(node) = doc.get_node(node) else {
        return out;
    };
    let Some(border) = box_rect(doc, node) else {
        return out;
    };
    let content = content_rect(node, border);
    let scale_rect = |r: Rect| {
        Rect::new(
            r.x0 * scale,
            r.y0 * scale,
            r.x1 * scale,
            r.y1 * scale,
        )
    };
    out.push((scale_rect(content), COLOR_FILL));
    let band = OUTLINE_PX.min(border.width().max(border.height()) / 2.0);
    let edges = [
        Rect::new(border.x0, border.y0, border.x1, border.y0 + band),
        Rect::new(border.x0, border.y1 - band, border.x1, border.y1),
        Rect::new(border.x0, border.y0, border.x0 + band, border.y1),
        Rect::new(border.x1 - band, border.y0, border.x1, border.y1),
    ];
    out.extend(edges.map(|edge| (scale_rect(edge), COLOR_OUTLINE)));
    out
}

/// Draw markers *over* the finished page scene, in device pixels.
pub fn paint(scene: &mut impl PaintScene, highlights: &[Highlight]) {
    for (rect, color) in highlights {
        scene.fill(Fill::NonZero, Affine::IDENTITY, *color, None, rect);
    }
}

// ---------------------------------------------------------------------------
// scrolling a selection into view

/// A selection jump leaves this much blank viewport above (and left of) the box,
/// the same margin find's find-bar jump uses, so the rows around it stay visible.
const SCROLL_MARGIN: f64 = 24.0;

/// Scroll the root scroller so `rect` (viewport CSS px, as [`box_rect`] reports
/// it) is **fully** visible — and do nothing when it already is, which is the
/// rule the contract gives an inspector click: a row you can see must not jump.
///
/// Same recipe as [`crate::find::scroll_hit_into_view`] (`scroll_to` on the root
/// element, `ScrollBehavior::Instant`); when the box is taller than the
/// viewport its top edge goes to the margin line, like every DevTools.
pub fn scroll_rect_into_view(
    doc: &mut BaseDocument,
    rect: Rect,
    viewport_w: f64,
    viewport_h: f64,
) {
    let fully_visible =
        rect.x0 >= 0.0 && rect.y0 >= 0.0 && rect.x1 <= viewport_w && rect.y1 <= viewport_h;
    if fully_visible {
        return;
    }
    let scroll = doc.viewport_scroll();
    let x = if rect.x0 >= 0.0 && rect.x1 <= viewport_w {
        scroll.x
    } else {
        scroll.x + rect.x0 - SCROLL_MARGIN.min(viewport_w.max(0.0))
    };
    let y = if rect.y0 >= 0.0 && rect.y1 <= viewport_h {
        scroll.y
    } else if rect.y0 < 0.0 {
        // Scrolled past above: bring the top edge back under the margin.
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
// the structural fingerprint

/// A cheap structural stamp of a snapshot — kinds, tags, attribute names *and*
/// values, text, parents and depths, plus the URL, generation and the truncation
/// flag; deliberately **not** `bounds`.
///
/// This is what lets an open pane stay silent about a document that has not
/// changed: the runtime re-walks on its poll tick and ships only when the stamp
/// moves, so an idle page costs a walk and no IPC, while a mutation (a node
/// appended, a tag renamed, an attribute set) ships immediately. Attribute
/// values are part of the stamp because a page that only flips `class` or `href`
/// — the most common SPA update of all — has changed the tree the pane is
/// showing, and pretending otherwise would leave it displaying a stale row.
/// Selection changes are not: those are emitted on their own, with a fresh
/// snapshot.
///
/// FNV-1a over the fields, in row order (so a node moving between parents is a
/// different stamp even though its own row is unchanged).
pub fn fingerprint(snapshot: &InspectSnapshot) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    fn feed(hash: &mut u64, bytes: &[u8]) {
        for &b in bytes {
            *hash ^= u64::from(b);
            *hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    feed(&mut hash, snapshot.url.as_bytes());
    feed(&mut hash, &snapshot.generation.to_le_bytes());
    feed(&mut hash, [u8::from(snapshot.truncated)].as_slice());
    for node in &snapshot.nodes {
        let kind = match node.kind {
            InspectKind::Element => 1u8,
            InspectKind::Text => 2,
            InspectKind::Comment => 3,
            InspectKind::Doctype => 4,
            InspectKind::Document => 5,
        };
        feed(&mut hash, [kind].as_slice());
        feed(&mut hash, node.tag.as_bytes());
        feed(&mut hash, node.text.as_bytes());
        feed(&mut hash, &node.depth.to_le_bytes());
        feed(
            &mut hash,
            &node.parent.unwrap_or(u64::MAX).to_le_bytes(),
        );
        for (name, value) in &node.attrs {
            feed(&mut hash, name.as_bytes());
            feed(&mut hash, [0].as_slice());
            feed(&mut hash, value.as_bytes());
            feed(&mut hash, [0].as_slice());
        }
        feed(&mut hash, [0xff].as_slice());
    }
    hash
}

// ---------------------------------------------------------------------------
// tests

#[cfg(test)]
mod tests {
    use super::*;
    use crate::find::tests::document;

    /// Rows as `(kind, tag, id, parent, depth)` — the shape under test, in one
    /// readable line per row.
    fn shape(snapshot: &InspectSnapshot) -> Vec<(InspectKind, String, u64, Option<u64>, usize)> {
        snapshot
            .nodes
            .iter()
            .map(|node| {
                (
                    node.kind,
                    node.tag.clone(),
                    node.id,
                    node.parent,
                    node.depth,
                )
            })
            .collect()
    }

    /// The same for the elements only, keyed by tag: `<div>` rows and so on.
    fn tags(snapshot: &InspectSnapshot) -> Vec<String> {
        snapshot
            .nodes
            .iter()
            .filter(|node| node.kind == InspectKind::Element)
            .map(|node| node.tag.clone())
            .collect()
    }

    /// Snapshot the way the runtime does: walk the settled document with the
    /// doctype recovered from the same source it was parsed from.
    fn snap(html: &str) -> InspectSnapshot {
        let doc = document(html);
        snapshot_with_doctype(
            &doc,
            "http://example.test/",
            7,
            None,
            doctype_from_source(html).as_deref(),
        )
    }

    #[test]
    fn preorder_ids_parents_and_depths() {
        let snapshot = snap(
            "<!doctype html><html><head><title>t</title></head>\
             <body><div id=a><p>hi</p></div></body></html>",
        );
        // Pre-order, with ids minted from 0 in document order…
        assert_eq!(
            shape(&snapshot),
            vec![
                (InspectKind::Document, String::new(), 0, None, 0),
                (InspectKind::Doctype, String::new(), 1, Some(0), 1),
                (InspectKind::Element, "html".into(), 2, Some(0), 1),
                (InspectKind::Element, "head".into(), 3, Some(2), 2),
                (InspectKind::Element, "title".into(), 4, Some(3), 3),
                (InspectKind::Text, String::new(), 5, Some(4), 4),
                (InspectKind::Element, "body".into(), 6, Some(2), 2),
                (InspectKind::Element, "div".into(), 7, Some(6), 3),
                (InspectKind::Element, "p".into(), 8, Some(7), 4),
                (InspectKind::Text, String::new(), 9, Some(8), 5),
            ]
        );
        assert_eq!(tags(&snapshot), ["html", "head", "title", "body", "div", "p"]);
        assert_eq!(snapshot.generation, 7);
        assert_eq!(snapshot.url, "http://example.test/");
        assert!(!snapshot.truncated);
        assert_eq!(snapshot.selected, None);
        // …and the text is the laid-out text, not the raw source.
        assert_eq!(snapshot.nodes[5].text, "t");
        assert_eq!(snapshot.nodes[9].text, "hi");
        // The div's `id` is on its row (the pane's row label comes from it).
        let div = &snapshot.nodes[7];
        assert_eq!(div.attrs.len(), 1);
        assert_eq!(div.attrs[0], ("id".to_string(), "a".to_string()));
    }

    #[test]
    fn whitespace_only_text_is_dropped_and_runs_collapse() {
        let snapshot = snap(
            "<!doctype html><html><body>  <div>  hi  there \n\t </div><p> </p>\
             <span>a   b</span></body></html>",
        );
        let text: Vec<&str> = snapshot
            .nodes
            .iter()
            .filter(|node| node.kind == InspectKind::Text)
            .map(|node| node.text.as_str())
            .collect();
        assert_eq!(
            text, ["hi there", "a b"],
            "whitespace-only text never gets a row, and runs collapse to one space"
        );
        // `<p>` survives as an empty element row — it is the text that went, not it.
        assert!(tags(&snapshot).contains(&"p".to_string()));
    }

    #[test]
    fn comment_and_doctype_rows_have_their_kinds() {
        let snapshot = snap(
            "<!doctype html><!-- keep me --><html><body><!--x--></body></html>",
        );
        let doctype = snapshot
            .nodes
            .iter()
            .find(|node| node.kind == InspectKind::Doctype)
            .expect("a doctype row");
        assert_eq!(doctype.depth, 1);
        assert_eq!(doctype.parent, Some(0));
        assert_eq!(doctype.id, 1, "the doctype is the document's first child");
        assert_eq!(doctype.text, "html", "the row carries the doctype name");
        let comments: Vec<&InspectNode> = snapshot
            .nodes
            .iter()
            .filter(|node| node.kind == InspectKind::Comment)
            .collect();
        assert_eq!(comments.len(), 2);
        assert_eq!(comments[0].text, "keep me");
        assert_eq!(comments[0].parent, Some(0), "before <html>: a child of the document");
        assert_eq!(comments[1].text, "x");
        // No doctype in the source → no doctype row (quirks mode).
        let quirks = snap("<html><body>x</body></html>");
        assert!(quirks
            .nodes
            .iter()
            .all(|node| node.kind != InspectKind::Doctype));
        assert_eq!(quirks.nodes[0].kind, InspectKind::Document);
    }

    #[test]
    fn doctype_from_source_recovers_what_the_parser_drops() {
        assert_eq!(
            doctype_from_source("<!DOCTYPE html>\n<html></html>").as_deref(),
            Some("html")
        );
        assert_eq!(
            // A legacy doctype: its public/system identifiers are quoted and one
            // of them contains a `>`, so the scan cannot stop at the first `>`;
            // the name is still `html`, and the spelling is lower-cased.
            doctype_from_source("<!doctype HTML PUBLIC \"-//W3C//DTD HTML 4.01//EN\" \"http://x/>y\">").as_deref(),
            Some("html")
        );
        assert_eq!(
            doctype_from_source("<!DOCTYPE html SYSTEM \"about:legacy-compat\">").as_deref(),
            Some("html")
        );
        assert_eq!(doctype_from_source("<!--c--><!doctype html>"), None, "it has to come first");
        assert_eq!(doctype_from_source("\u{feff}  <!DOCTYPE html>").as_deref(), Some("html"));
        assert_eq!(doctype_from_source("<html></html>"), None);
        assert_eq!(doctype_from_source(""), None);
        assert_eq!(doctype_from_source("<!doctype"), None, "never closed: no doctype");
        assert_eq!(doctype_from_source("<!DOCTYPE >"), None, "no name: nothing to show");
    }

    #[test]
    fn caps_bound_rows_attrs_and_values() {
        // An attribute value longer than MAX_INSPECT_ATTR_VALUE is cut, and says
        // so with `…`.
        let long = "v".repeat(MAX_INSPECT_ATTR_VALUE + 144);
        let snapshot = snap(&format!(
            "<!doctype html><html><body><div title=\"{long}\">x</div></body></html>"
        ));
        let div = snapshot
            .nodes
            .iter()
            .find(|node| node.tag == "div")
            .expect("the div");
        let value = div
            .attrs
            .iter()
            .find(|(name, _)| name == "title")
            .map(|(_, value)| value.clone())
            .expect("title");
        assert!(value.ends_with('…'), "the cut must be marked: {value}");
        assert_eq!(
            value.chars().count(),
            MAX_INSPECT_ATTR_VALUE + 1,
            "the cap counts chars, and `…` marks the cut"
        );

        // An element with more attributes than MAX_INSPECT_ATTRS keeps that many.
        let attrs: String = (0..40)
            .map(|i| format!(" data-a{i}={i}"))
            .collect::<Vec<_>>()
            .join("");
        let snapshot = snap(&format!(
            "<!doctype html><html><body><div{attrs}>x</div></body></html>"
        ));
        let div = snapshot
            .nodes
            .iter()
            .find(|node| node.tag == "div")
            .expect("the div");
        assert_eq!(div.attrs.len(), MAX_INSPECT_ATTRS);
        assert_eq!(div.attrs[0].0, "data-a0");

        // A document with more rows than MAX_INSPECT_NODES stops at the cap and
        // reports it — the whole snapshot, not the last row, is truncated.
        let body = (0..3000)
            .map(|i| format!("<div id=d{i}>t{i}</div>"))
            .collect::<String>();
        let snapshot = snap(&format!(
            "<!doctype html><html><body>{body}</body></html>"
        ));
        assert_eq!(snapshot.nodes.len(), MAX_INSPECT_NODES);
        assert!(snapshot.truncated);
        // Ids stay dense and pre-order right up to the cap.
        for (index, node) in snapshot.nodes.iter().enumerate() {
            assert_eq!(node.id, index as u64);
        }

        // Long text is capped the same way (200 chars + `…`).
        let snapshot = snap(&format!(
            "<!doctype html><html><body><p>{}</p></body></html>",
            "word ".repeat(200)
        ));
        let text = snapshot
            .nodes
            .iter()
            .find(|node| node.kind == InspectKind::Text)
            .expect("the text row");
        assert!(text.text.ends_with('…'));
        assert_eq!(text.text.chars().count(), MAX_INSPECT_TEXT + 1);
    }

    #[test]
    fn bounds_are_some_only_for_laid_out_elements() {
        let snapshot = snap(
            "<!doctype html><html><head><title>t</title><style>p{color:red}</style></head>\
             <body><div id=box style=\"width:100px;height:50px\">hi</div>\
             <p style=\"display:none\">gone</p></body></html>",
        );
        // `<head>` and everything in it: never laid out.
        let head = snapshot
            .nodes
            .iter()
            .find(|node| node.tag == "head")
            .expect("head");
        assert_eq!(head.bounds, None);
        let title = snapshot
            .nodes
            .iter()
            .find(|node| node.tag == "title")
            .expect("title");
        assert_eq!(title.bounds, None);
        assert!(snapshot
            .nodes
            .iter()
            .filter(|node| node.kind != InspectKind::Element)
            .all(|node| node.bounds.is_none()));

        // A laid-out `<div>`: its border box in CSS px, viewport-relative.
        let div = snapshot
            .nodes
            .iter()
            .find(|node| node.tag == "div")
            .expect("the div");
        let box_ = div.bounds.expect("a laid-out div has a box");
        assert!(box_.width > 99.0 && box_.width < 101.0, "{box_:?}");
        assert!(box_.height > 49.0 && box_.height < 51.0, "{box_:?}");
        // The default 8px body margin offsets it; `<html>` starts at the origin.
        assert!(box_.x > 7.0 && box_.x < 9.0, "{box_:?}");

        // `display: none` never has a box — and neither does its subtree.
        let p = snapshot
            .nodes
            .iter()
            .find(|node| node.tag == "p")
            .expect("the hidden p");
        assert_eq!(p.bounds, None);
    }

    #[test]
    fn markers_are_empty_for_a_boxless_node_and_two_toned_for_a_box() {
        let doc = document(
            "<!doctype html><html><body><div style=\"width:120px;height:60px\">x</div></body></html>",
        );
        let snapshot = walk(&doc, "http://example.test/", 1, None, None);
        let div = snapshot
            .snapshot
            .nodes
            .iter()
            .find(|node| node.tag == "div")
            .expect("the div");
        let highlights = marker(&doc, snapshot.nodes[div.id as usize], 1.0);
        // One fill + four outline bars.
        assert_eq!(highlights.len(), 5);
        assert_eq!(highlights[0].1, COLOR_FILL);
        assert!(highlights[1..].iter().all(|(_, color)| *color == COLOR_OUTLINE));
        // The content box sits inside the border box (the fill never covers the
        // outline bars).
        let fill = highlights[0].0;
        let outlined: Rect = highlights[1..]
            .iter()
            .map(|(rect, _)| *rect)
            .fold(Rect::ZERO, |acc, r| acc.union(r));
        assert!(fill.x0 >= outlined.x0 && fill.x1 <= outlined.x1);
        // A text row has no box: no marker, so a stale selection is invisible.
        let text = snapshot
            .snapshot
            .nodes
            .iter()
            .find(|node| node.kind == InspectKind::Text)
            .expect("the text row");
        assert!(marker(&doc, snapshot.nodes[text.id as usize], 1.0).is_empty());
        // No scale, no marker (never divide by a zero scale).
        assert!(marker(&doc, snapshot.nodes[div.id as usize], 0.0).is_empty());
    }

    #[test]
    fn hit_test_names_a_row_of_the_same_snapshot() {
        let doc = document(
            "<!doctype html><html><head><title>t</title></head><body>\
             <div id=outer><p id=inner style=\"width:80px;height:40px\">hi</p></div>\
             </body></html>",
        );
        let snapshot = snapshot_with_doctype(&doc, "http://example.test/", 3, None, Some("<!DOCTYPE html>"));
        // The centre of `<p id=inner>` hits the `p`, not the div around it — and
        // the id it reports is that `p`'s row in this very snapshot.
        let p = snapshot
            .nodes
            .iter()
            .find(|node| node.tag == "p")
            .expect("the p");
        let b = p.bounds.expect("a laid-out p");
        let hit = hit_element_at_with(
            &doc,
            b.x + b.width / 2.0,
            b.y + b.height / 2.0,
            Some("<!DOCTYPE html>"),
        )
        .expect("the p's centre answers");
        assert_eq!(hit, p.id);
        assert_eq!(snapshot.nodes[hit as usize].tag, "p");

        // A point outside the viewport answers nothing.
        assert_eq!(hit_element_at_with(&doc, 4000.0, 4000.0, Some("<!DOCTYPE html>")), None);
        // The doctype row shifts every id by one: without it the same point
        // resolves to the row *after* the p. That is the reason the runtime
        // threads the recovered doctype through both walks.
        assert_ne!(
            hit_element_at_with(&doc, b.x + b.width / 2.0, b.y + b.height / 2.0, None),
            Some(p.id)
        );

        // `node_of` is the handle lookup the runtime paints from; out of range
        // answers None rather than panicking on a stale id.
        let walk = walk(&doc, "http://example.test/", 3, None, Some("<!DOCTYPE html>"));
        assert!(node_of(&walk.nodes, u64::MAX).is_none());
        assert!(node_of(&walk.nodes, walk.snapshot.nodes.len() as u64).is_none());
        assert!(node_of(&walk.nodes, p.id).is_some());
        // The walk's node handles line up with its rows one for one.
        assert_eq!(walk.nodes.len(), walk.snapshot.nodes.len());
    }

    #[test]
    fn fingerprint_ignores_bounds_and_moves_when_the_tree_moves() {
        let html = "<!doctype html><html><body><div id=a><p>hi</p></div></body></html>";
        let first = snap(html);
        let again = snap(html);
        assert_eq!(
            fingerprint(&first),
            fingerprint(&again),
            "an unchanged re-snapshot must not look like a change"
        );
        // Bounds are not part of the stamp: the same tree at a different scroll
        // offset is the same tree.
        let mut scrolled = again.clone();
        for node in &mut scrolled.nodes {
            if let Some(b) = node.bounds.as_mut() {
                b.y += 40.0;
            }
        }
        assert_eq!(fingerprint(&first), fingerprint(&scrolled));

        // …but a node appearing (or vanishing, or moving) is a change.
        let (mut runtime, mut doc) = crate::js::tests::dom_world();
        let before = snapshot_with_doctype(&doc, "http://example.test/", 1, None, None);
        crate::js::tests::dom_eval(
            &mut runtime,
            &mut doc,
            r#"document.body.appendChild(document.createElement("section"))"#,
        );
        let after = snapshot_with_doctype(&doc, "http://example.test/", 1, None, None);
        assert!(after
            .nodes
            .iter()
            .any(|node| node.tag == "section"));
        assert_ne!(
            fingerprint(&before),
            fingerprint(&after),
            "createElement+append changed the tree the pane shows"
        );
        // URL and generation are in the stamp too (a new navigation with
        // identical markup is still a new tree the shell must re-render).
        let mut renamed = after.clone();
        renamed.url = "http://example.test/next".into();
        assert_ne!(fingerprint(&after), fingerprint(&renamed));
        let mut bumped = after.clone();
        bumped.generation += 1;
        assert_ne!(fingerprint(&after), fingerprint(&bumped));
        // Selecting a row is not a tree change.
        let mut selected = after.clone();
        selected.selected = Some(3);
        assert_eq!(fingerprint(&after), fingerprint(&selected));
    }
    #[test]
    fn inline_content_gets_its_own_box_not_its_lines() {
        // A `<span>` is a non-atomic inline: it has no layout box of its own, so
        // a box taken from `final_layout()` would be empty, and one taken from
        // the nearest block would be the whole line. The engine's client rect is
        // the union of its fragments, which is the word — and the place comes
        // from the block it sits in.
        let snapshot = snap(
            "<!doctype html><html><body><p id=para>before <span id=note>word</span> after</p></body></html>",
        );
        let para = snapshot
            .nodes
            .iter()
            .find(|node| node.tag == "p")
            .expect("the paragraph");
        let note = snapshot
            .nodes
            .iter()
            .find(|node| node.tag == "span")
            .expect("the span");
        let note = note.bounds.expect("inline content has a box");
        let para = para.bounds.expect("the block has a box");
        assert!(note.height > 0.0 && note.height <= para.height, "{note:?}");
        assert!(
            note.x > para.x,
            "it starts after the word that precedes it, not at the line's start: {note:?} vs {para:?}"
        );
        assert!(
            note.width < para.width,
            "it covers the word, not the line: {note:?} vs {para:?}"
        );
        assert!(note.x + note.width <= para.x + para.width + 0.01, "{note:?}");
    }
}
