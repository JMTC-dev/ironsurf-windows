//! Reader view (task: reader mode, Ctrl-Alt-`R` / the 📄 toolbar button).
//!
//! Turns the page currently on screen into a clean document of **text and
//! images only**: no scripts, no styles, no chrome — just the article, laid
//! out in a centered reading column.
//!
//! The source is the **live DOM** at the moment of the request — everything
//! JavaScript and XHR put into the document before the button was pressed is
//! reader material (JS-heavy single-page apps render as nothing from the
//! fetched bytes alone). But scripts can also *destroy* content on the way:
//! a client-side router that fails mid-boot clears a server-rendered body
//! back to an empty shell (`making-tailscale-faster`-class failures). So the
//! entry point [`extract_best`] runs the extractor twice — over the live DOM
//! and over the fetched source — and keeps whichever candidate carries
//! **more text**, with the live DOM winning ties.
//!
//! The extraction itself is a compact readability-style pipeline:
//!
//! 1. **Collect** — walk the blitz tree into an owned arena, pruning
//!    scripts/styles/controls/hidden subtrees and normalizing whitespace.
//! 2. **Score** — every paragraph with real prose votes for its own node,
//!    its parent, and (at a third weight) its grandparent; class/id names
//!    add ±25 (`article`/`content`/`main` … up, `sidebar`/`comment`/`nav`
//!    … down); link density penalises navigation blocks.
//! 3. **Choose** — the highest-scoring structural container wins; a strong
//!    sibling carrying its own paragraphs is merged in (split article
//!    containers). Nothing above threshold → fall back to *every* substantial
//!    paragraph in document order.
//! 4. **Render** — an allowlist of text and image tags; every other element
//!    is unwrapped to its children, navigation-shaped and negative-named
//!    blocks are dropped, anchors keep `href`, images keep `src` + `alt`
//!    (lazy-load `data-src` spellings first when the live `src` is a
//!    placeholder). Output is one small standalone HTML document whose base
//!    URL is the page's own, so relative images still resolve.

use blitz_dom::{BaseDocument, NodeData, NodeId};

/// The whole reader document, ready to hand to `load_html`.
pub struct ReaderDoc {
    /// Standalone HTML (head + reader CSS + cleaned-up body).
    pub html: String,
    /// Article title: first `<h1>`, else the document `<title>`.
    pub title: Option<String>,
    /// Characters of prose text in the rendered output — what [`extract_best`]
    /// compares between the live-DOM and fetched-source candidates.
    pub text_len: usize,
    /// `<img>`s kept in the output.
    pub images: usize,
}

/// The shortest rendered text that still counts as an article. Below this a
/// candidate is noise (a cookie wall, a JS shell) and extraction fails.
const MIN_ARTICLE_CHARS: usize = 140;

/// Elements whose subtree never carries reader text — collected away whole.
fn prune_subtree(tag: &str) -> bool {
    matches!(
        tag,
        "script"
            | "style"
            | "noscript"
            | "template"
            | "head"
            | "svg"
            | "math"
            | "canvas"
            | "video"
            | "audio"
            | "iframe"
            | "object"
            | "embed"
            | "track"
            | "map"
            | "area"
            | "button"
            | "select"
            | "option"
            | "optgroup"
            | "textarea"
            | "datalist"
            | "label"
    )
}

// ---------------------------------------------------------------------------
// collection: blitz tree → owned arena

/// One arena node: the tree extraction sees, with the few attributes output
/// needs and the scoring aggregates filled in by [`score`].
struct RNode {
    tag: String,
    /// Text-node content (normalized); `None` for elements.
    text: Option<String>,
    class: String,
    id: String,
    href: Option<String>,
    src: Option<String>,
    alt: String,
    children: Vec<usize>,
    // scoring aggregates (subtree)
    text_len: usize,
    link_len: usize,
    para_count: usize,
    /// paragraph votes accumulated by `score`
    score: f64,
    /// class/id name weight
    bonus: f64,
}

impl RNode {
    fn new(tag: String) -> Self {
        Self {
            tag,
            text: None,
            class: String::new(),
            id: String::new(),
            href: None,
            src: None,
            alt: String::new(),
            children: Vec::new(),
            text_len: 0,
            link_len: 0,
            para_count: 0,
            score: 0.0,
            bonus: 0.0,
        }
    }
}

/// Collapse a DOM text run to single spaces, keeping one leading and one
/// trailing space when the original had them (inline `a <b>b</b> c` spacing
/// survives; markup-indentation noise does not).
fn normalize_ws(s: &str) -> String {
    if s.is_empty() {
        return String::new();
    }
    let starts_ws = s.chars().next().is_some_and(char::is_whitespace);
    let ends_ws = s.chars().last().is_some_and(char::is_whitespace);
    let body = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if body.is_empty() {
        // whitespace-only run: one space, never two
        return " ".to_string();
    }
    let mut out = String::with_capacity(body.len() + 2);
    if starts_ws {
        out.push(' ');
    }
    out.push_str(&body);
    if ends_ws {
        out.push(' ');
    }
    out
}

/// An inline `style` attribute declaring the subtree away.
fn declared_hidden(style: &str) -> bool {
    let squashed: String = style
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect::<String>()
        .to_ascii_lowercase();
    squashed.contains("display:none") || squashed.contains("visibility:hidden")
}

/// Collect `node`'s subtree into the arena under `parent`. Returns the arena
/// index of the collected node, or `parent` itself when the node is pruned
/// or produced nothing worth keeping.
fn collect(doc: &BaseDocument, node_id: NodeId, arena: &mut Vec<RNode>, parent: usize, depth: u32) {
    if depth > 200 {
        return; // pathological nesting: stop, never recurse forever
    }
    let Some(node) = doc.get_node(node_id) else { return };
    let mut keep = None; // arena index this node writes into (children's parent)
    match &node.data {
        NodeData::Element(element) => {
            let tag: &str = &element.name.local;
            if prune_subtree(tag) {
                return;
            }
            let mut style = String::new();
            let mut hidden_attr = false;
            let mut lazy_src: Option<String> = None;
            let mut me = RNode::new(tag.to_ascii_lowercase());
            if let Some(list) = node.data.attrs() {
                for attr in list.iter() {
                    let name: &str = &attr.name.local;
                    let value = attr.value.to_string();
                    match name {
                        "class" => me.class = value,
                        "id" => me.id = value,
                        "href" => me.href = Some(value),
                        "src" => me.src = Some(value),
                        "alt" => me.alt = value,
                        "style" => style = value,
                        "hidden" => hidden_attr = value != "until-found",
                        // lazy-loaders park the real address behind `src`:
                        // collect the usual spellings as a fallback.
                        "data-src" | "data-lazy-src" | "data-original" | "data-echo" => {
                            if lazy_src.is_none() {
                                lazy_src = Some(value);
                            }
                        }
                        _ => {}
                    }
                }
            }
            if hidden_attr || declared_hidden(&style) {
                return;
            }
            // a lazy placeholder never beats the real address
            if let Some(lazy) = lazy_src {
                match me.src.as_deref() {
                    Some(s) if is_placeholder(s) => me.src = Some(lazy),
                    None => me.src = Some(lazy),
                    _ => {}
                }
            }
            let idx = arena.len();
            arena.push(me);
            arena[parent].children.push(idx);
            keep = Some(idx);
        }
        NodeData::Text(content) => {
            let text = normalize_ws(&content.content);
            if text.is_empty() {
                return;
            }
            let mut me = RNode::new("text".to_string());
            me.text = Some(text);
            let idx = arena.len();
            arena.push(me);
            arena[parent].children.push(idx);
            keep = Some(idx);
        }
        _ => {} // comments, document/anonymous boxes: transparent
    }
    let parent_idx = match keep {
        Some(idx) => idx,
        None => parent,
    };
    for child in node.children.iter().copied() {
        collect(doc, child, arena, parent_idx, depth + 1);
    }
}

/// A `src` that is almost certainly a lazy-load placeholder, not the image.
fn is_placeholder(src: &str) -> bool {
    let s = src.trim();
    s.is_empty()
        || s.starts_with("data:image/gif")
        || (s.starts_with("data:") && s.len() < 120)
}

/// Text of a blitz document's `<title>` (searched live: `head` is pruned
/// from the reader arena). The first `<title>` element found depth-first.
fn doc_title(doc: &BaseDocument) -> Option<String> {
    let mut stack: Vec<NodeId> = doc.root_node().children.iter().copied().collect();
    while let Some(id) = stack.pop() {
        let Some(node) = doc.get_node(id) else { continue };
        if let NodeData::Element(element) = &node.data {
            let tag: &str = &element.name.local;
            if tag == "title" {
                let text: String = node
                    .children
                    .iter()
                    .filter_map(|c| doc.get_node(*c))
                    .filter_map(|c| match &c.data {
                        NodeData::Text(t) => Some(t.content.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("");
                let text = text.trim().to_string();
                if !text.is_empty() {
                    return Some(text);
                }
            }
            if tag == "script" || tag == "style" {
                continue;
            }
        }
        stack.extend(node.children.iter().copied());
    }
    None
}

// ---------------------------------------------------------------------------
// scoring

/// Positive/negative container-name hints (readability's, trimmed).
fn class_bonus(text: &str) -> f64 {
    const POSITIVE: &[&str] = &[
        "article", "body", "blog", "content", "entry", "main", "page", "post", "story", "text",
        "wrapper", "column", "inner",
    ];
    const NEGATIVE: &[&str] = &[
        "banner",
        "combx",
        "comment",
        "comments",
        "disqus",
        "foot",
        "footer",
        "menu",
        "nav",
        "navbar",
        "rail",
        "related",
        "share",
        "shoutbox",
        "sidebar",
        "sponsor",
        "advert",
        "ads",
        "promo",
        "toolbar",
        "breadcrumb",
        "pagination",
        "cookie",
        "popup",
        "modal",
        "drawer",
        "carousel",
        "social",
        "meta",
        "hidden",
        "widget",
        "breadcrumb",
    ];
    let lower = text.to_ascii_lowercase();
    let mut bonus = 0.0;
    // whole tokens (`class="post meta"` scores as post+, meta−), not
    // substrings — `newsletter` must not read as `news`… nor `commentary` as
    // a comment section: hyphens/underscores split like spaces.
    for token in lower.split(|c: char| !c.is_alphanumeric()) {
        if token.len() < 3 {
            continue;
        }
        if POSITIVE.contains(&token) {
            bonus += 25.0;
        }
        if NEGATIVE.contains(&token) {
            bonus -= 25.0;
        }
    }
    bonus
}

/// Fill the subtree aggregates (`text_len`, `link_len`, `para_count`),
/// bottom-up through the arena (indices are children-after-parents by
/// construction, so the pass walks them in reverse).
fn tally(arena: &mut Vec<RNode>) {
    let n = arena.len();
    for i in (0..n).rev() {
        let (t, l, p) = {
            let me = &arena[i];
            let own_text = me.text.as_deref().map(str::chars).map_or(0, |c| c.count());
            let (mut t, mut l, mut p) = (own_text, 0usize, 0usize);
            for &c in &me.children {
                t += arena[c].text_len;
                l += arena[c].link_len;
                p += arena[c].para_count;
            }
            if me.tag == "a" {
                l = t; // everything under a link is link text
            }
            if me.tag == "p" && t >= 25 {
                p += 1;
            }
            (t, l, p)
        };
        arena[i].text_len = t;
        arena[i].link_len = l;
        arena[i].para_count = p;
    }
    // second pass: paragraph votes (`score`) need the aggregates above them
    // settled first, so a separate walk: children → parent → grandparent.
    let votes: Vec<(usize, f64)> = {
        let mut v = Vec::new();
        for i in 0..n {
            if arena[i].tag == "p" && arena[i].text_len >= 25 {
                let commas = arena[i]
                    .text
                    .as_deref()
                    .map(|t| t.matches(',').count().min(10))
                    .unwrap_or(0) as f64;
                // prose-length vote, capped so one giant block quote cannot
                // outrank the container that actually holds the article.
                let length = (arena[i].text_len as f64 / 100.0).min(3.0);
                v.push((i, 1.0 + length + commas));
            }
        }
        v
    };
    for (i, s) in votes {
        let parent = parent_of(arena, i);
        if let Some(p) = parent {
            arena[p].score += s;
            if let Some(g) = parent_of(arena, p) {
                arena[g].score += s / 3.0;
            }
        }
    }
    for i in 0..n {
        let mut b = 0.0;
        if !arena[i].class.is_empty() {
            b += class_bonus(&arena[i].class);
        }
        if !arena[i].id.is_empty() {
            b += class_bonus(&arena[i].id);
        }
        arena[i].bonus = b;
    }
}

/// Parent lookup: the arena is built children-after-parents in a preorder
/// walk, so the parent of `i` is the one earlier node whose children contain
/// it — a linear scan over a few thousand indices, run a few hundred times
/// per document.
fn parent_of(arena: &[RNode], i: usize) -> Option<usize> {
    // The arena is built children-after-parents, so a linear child scan over
    // the earlier nodes is the whole search; fine at these sizes.
    (0..i).rev().find(|&p| arena[p].children.contains(&i))
}

/// The best structural container, or `None` when nothing scores.
fn best_container(arena: &[RNode]) -> Option<usize> {
    const CANDIDATES: &[&str] = &["div", "article", "section", "main", "td", "figure"];
    // (`body` is deliberately not a candidate: when the prose sits directly on
    // the body, scoring has nothing to choose — the paragraph fallback below
    // is what should fire, not "render the entire body including its chrome".)
    let mut best: Option<(usize, f64, usize)> = None;
    for (i, me) in arena.iter().enumerate() {
        if !CANDIDATES.contains(&me.tag.as_str()) || me.text_len < 25 {
            continue;
        }
        let link_density = me.link_len as f64 / (me.text_len.max(1) as f64);
        let s = (me.score + me.bonus + me.para_count as f64 * 2.0) * (1.0 - link_density);
        // higher score wins; ties go to the node carrying more text
        let better = match best {
            None => true,
            Some((_, bs, bt)) => s > bs || (s == bs && me.text_len > bt),
        };
        if better {
            best = Some((i, s, me.text_len));
        }
    }
    // A candidate that never saw a single paragraph vote and carries no
    // structural name is not an article.
    best.filter(|(i, _, _)| arena[*i].score + arena[*i].bonus > 0.0).map(|(i, _, _)| i)
}

// ---------------------------------------------------------------------------
// output

/// HTML-escape text going into the reader document.
fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// The reader document's whole style sheet: a centered column, serif prose,
/// full-width images, nothing else on the page.
const READER_CSS: &str = "\
body{margin:0;background:#fbfaf7;color:#1c1b1a;font-family:Georgia,'DejaVu Serif','Noto Serif',serif}\
.rd-col{max-width:44rem;margin:0 auto;padding:2rem 1.25rem 5rem}\
.rd-title{font-size:1.8em;line-height:1.25;margin:0 0 1.5rem;font-weight:600}\
.rd-content p{margin:0 0 1.1em;line-height:1.65}\
.rd-content h1,.rd-content h2,.rd-content h3,.rd-content h4,.rd-content h5,.rd-content h6{\
margin:1.6em 0 .6em;line-height:1.3}\
img{max-width:100%;height:auto;display:block;margin:1.25rem 0}\
a{color:#1d4ed8;text-decoration:none}\
pre{white-space:pre-wrap;overflow-wrap:anywhere;background:#f1efe9;padding:.6rem;border-radius:4px}\
blockquote{margin:1.2em 0;padding-left:1rem;border-left:3px solid #cfcabf;color:#44423f}\
li{margin:.3em 0}\
figcaption{font-size:.85em;color:#6b6864;margin-top:.4em}\
hr{border:0;border-top:1px solid #ddd8cf;margin:2em 0}";

/// Tags rendered as themselves; anything else that survives selection is
/// unwrapped to its children.
fn kept_tag(tag: &str) -> bool {
    matches!(
        tag,
        "p" | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
            | "ul"
            | "ol"
            | "li"
            | "blockquote"
            | "pre"
            | "figure"
            | "figcaption"
            | "div"
            | "section"
            | "article"
            | "main"
            | "aside"
            | "details"
            | "summary"
            | "a"
            | "strong"
            | "b"
            | "em"
            | "i"
            | "u"
            | "s"
            | "strike"
            | "code"
            | "kbd"
            | "samp"
            | "var"
            | "abbr"
            | "q"
            | "cite"
            | "small"
            | "mark"
            | "sub"
            | "sup"
            | "time"
            | "span"
    )
}

/// Blocks whose name says "chrome, not prose" (dropped outright).
fn chrome_tag(tag: &str) -> bool {
    matches!(
        tag,
        "nav" | "header" | "footer" | "form" | "table" | "address" | "fieldset" | "dialog" | "menu"
    )
}

struct Sink<'a> {
    arena: &'a [RNode],
    out: String,
    text_len: usize,
    images: usize,
}

impl<'a> Sink<'a> {
    /// Render one arena node: navigation-shaped and negative-named blocks
    /// are pruned, allowlisted tags are kept, everything else is unwrapped
    /// to its children.
    fn node(&mut self, i: usize) {
        let me = &self.arena[i];
        if me.tag == "text" {
            let t = me.text.as_deref().unwrap_or("");
            self.text_len += t.chars().filter(|c| !c.is_whitespace()).count();
            self.out.push_str(&esc(t));
            return;
        }
        if chrome_tag(&me.tag) {
            return;
        }
        let link_density = me.link_len as f64 / (me.text_len.max(1) as f64);
        // navigation-shaped blocks: mostly links, little prose
        if me.tag != "a" && link_density > 0.5 && me.text_len < 400 && me.para_count == 0 {
            return;
        }
        // a negatively-named block with little prose is chrome
        if me.bonus < 0.0 && me.text_len < 120 && link_density > 0.2 {
            return;
        }
        match me.tag.as_str() {
            "img" => {
                let Some(src) = me.src.as_deref().map(str::trim) else { return };
                if src.is_empty() {
                    return;
                }
                self.images += 1;
                self.out.push_str("<img src=\"");
                self.out.push_str(&esc(src));
                if !me.alt.trim().is_empty() {
                    self.out.push_str("\" alt=\"");
                    self.out.push_str(&esc(me.alt.trim()));
                }
                self.out.push_str("\"/>");
            }
            "br" => self.out.push_str("<br/>"),
            "hr" => self.out.push_str("<hr/>"),
            "a" => {
                let href = me.href.as_deref().unwrap_or("").trim();
                // an anchor with no destination (or no label) is not a link:
                // keep whatever it wraps
                if href.is_empty() || me.text_len == 0 {
                    for &c in &me.children {
                        self.node(c);
                    }
                    return;
                }
                self.out.push_str("<a href=\"");
                self.out.push_str(&esc(href));
                self.out.push_str("\">");
                for &c in &me.children {
                    self.node(c);
                }
                self.out.push_str("</a>");
            }
            _ if kept_tag(&me.tag) => {
                let start = self.out.len();
                let content_start = start + me.tag.len() + 2;
                self.out.push('<');
                self.out.push_str(&me.tag);
                self.out.push('>');
                for &c in &me.children {
                    self.node(c);
                }
                let content_end = self.out.len();
                if self.out[content_start..content_end].trim().is_empty() {
                    // nothing kept below: drop the empty wrapper too
                    self.out.truncate(start);
                } else {
                    self.out.push_str("</");
                    self.out.push_str(&me.tag);
                    self.out.push('>');
                }
            }
            _ => {
                // transparent: unwrap unknown-but-harmless containers
                for &c in &me.children {
                    self.node(c);
                }
            }
        }
    }
}

/// Render a set of arena roots (the winner, plus merged siblings) in
/// document order into the reader body.
fn render_roots(arena: &[RNode], roots: &[usize]) -> (String, usize, usize) {
    let mut sink = Sink { arena, out: String::new(), text_len: 0, images: 0 };
    // roots are arena indices in collection order — sort to keep document order
    let mut roots = roots.to_vec();
    roots.sort_unstable();
    for r in roots {
        sink.node(r);
    }
    (sink.out, sink.text_len, sink.images)
}

/// One text-node-normalizing extraction pass over `doc`. `None` when nothing
/// readable came out (below [`MIN_ARTICLE_CHARS`]).
fn extract(doc: &BaseDocument) -> Option<ReaderDoc> {
    let mut arena = vec![RNode::new("root".to_string())]; // arena[0] = synthetic root
    for child in doc.root_node().children.iter().copied() {
        collect(doc, child, &mut arena, 0, 0);
    }
    tally(&mut arena);

    // choose: winner (+ merged strong siblings), else all-paragraphs fallback
    let mut roots: Vec<usize> = Vec::new();
    if let Some(best) = best_container(&arena) {
        let parent = parent_of(&arena, best);
        roots.push(best);
        if let Some(p) = parent {
            // a sibling carrying substantial prose of its own is part of the
            // same article (split containers are extremely common)
            let (bt, bp) = (arena[best].text_len, arena[best].para_count);
            for &sib in &arena[p].children {
                if sib != best
                    && arena[sib].text_len >= bt / 2
                    && arena[sib].para_count >= bp.div_ceil(2)
                    && arena[sib].text_len >= 75
                {
                    roots.push(sib);
                }
            }
        }
    }
    if roots.is_empty() {
        // fallback: every substantial paragraph anywhere, document order
        for i in 1..arena.len() {
            if arena[i].tag == "p" && arena[i].text_len >= 25 {
                roots.push(i);
            }
        }
        // …and every image anywhere: content the fallback can still show
        for i in 1..arena.len() {
            if arena[i].tag == "img" && arena[i].src.as_deref().is_some_and(|s| !is_placeholder(s)) {
                roots.push(i);
            }
        }
    }

    let (body, text_len, images) = render_roots(&arena, &roots);
    if text_len < MIN_ARTICLE_CHARS {
        return None;
    }

    // title: the first substantial h1 (document order), else <title>
    let first_h1 = (1..arena.len())
        .find(|&i| arena[i].tag == "h1" && arena[i].text_len >= 3)
        .and_then(|i| {
            let mut sink = Sink { arena: &arena, out: String::new(), text_len: 0, images: 0 };
            sink.node(i);
            let t = strip_tags(&sink.out).trim().to_string();
            if t.is_empty() || t.chars().count() > 200 {
                None
            } else {
                Some(t)
            }
        });
    let title = first_h1.or_else(|| doc_title(doc));

    // a duplicate of the title inside the content goes (readability does the
    // same; the header carries it once)
    let body = match &title {
        Some(t) => dedupe_leading_h1(body, t),
        None => body,
    };

    let mut html = String::with_capacity(body.len() + 512);
    html.push_str("<!DOCTYPE html>\n<html><head><meta charset=\"utf-8\"/>");
    if let Some(t) = &title {
        html.push_str("<title>");
        html.push_str(&esc(t));
        html.push_str("</title>");
    }
    html.push_str("<style>");
    html.push_str(READER_CSS);
    html.push_str("</style></head><body><div class=\"rd-col\">");
    if let Some(t) = &title {
        html.push_str("<h1 class=\"rd-title\">");
        html.push_str(&esc(t));
        html.push_str("</h1>");
    }
    html.push_str("<div class=\"rd-content\">");
    html.push_str(&body);
    html.push_str("</div></div></body></html>\n");
    Some(ReaderDoc { html, title, text_len, images })
}

/// Strip the (already-rendered) tags of an HTML fragment back to text.
fn strip_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

/// Remove a leading `<h1>…</h1>`/`<h1 class=…>…</h1>` from `body` when its
/// text equals the article title.
fn dedupe_leading_h1(body: String, title: &str) -> String {
    let lower = body.to_ascii_lowercase();
    if !lower.starts_with("<h1>") && !lower.starts_with("<h1 ") {
        return body;
    }
    let Some(end) = lower.find("</h1>") else { return body };
    let inner = &body[body.find('>').expect("starts with tag") + 1..end];
    if strip_tags(inner).trim() == title.trim() {
        return body[end + 5..].to_string();
    }
    body
}

/// Extract from the **fetched source** bytes: parse a scratch document
/// (structure only) and run the same pipeline. This is the candidate
/// scripts may have needed: content a failing client-side app cleared out
/// of the live DOM still stands in the HTML it loaded with.
///
/// `base` is the URL those bytes were fetched from: a real base is what
/// makes relative sub-resource references legal in the parsed scratch tree
/// (blitz asserts they resolve); the scratch document is never rendered and
/// is dropped right after extraction, so nothing is ever fetched against it.
/// Without a known URL a neutral local base stands in.
pub fn extract_source(html: &str, base: Option<&str>) -> Option<ReaderDoc> {
    let config = blitz_dom::DocumentConfig {
        base_url: Some(base.unwrap_or("http://localhost/").to_owned()),
        ..Default::default()
    };
    let doc = blitz_html::HtmlDocument::from_html(html, config);
    extract(&doc)
}

/// The entry point: extract over the **live DOM** (source of truth — it holds
/// everything JS had loaded in when the reader was asked for) and over the
/// **fetched source** — `(url, raw html)` of the current document, when
/// known — and keep the richer candidate; live DOM wins ties.
pub fn extract_best(
    live: Option<&BaseDocument>,
    source: Option<(&str, &str)>,
) -> Option<ReaderDoc> {
    let live_doc = live.and_then(extract);
    let source_doc = source.and_then(|(url, html)| extract_source(html, Some(url)));
    match (live_doc, source_doc) {
        (Some(l), Some(s)) => {
            if s.text_len > l.text_len {
                Some(s)
            } else {
                Some(l)
            }
        }
        (l, s) => l.or(s),
    }
}

// ---------------------------------------------------------------------------
// tests

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse `html` into a real blitz document (headless, no providers).
    /// A real base URL, like every loaded document has: blitz asserts
    /// relative sub-resource references resolve against the base.
    fn document(html: &str) -> blitz_html::HtmlDocument {
        blitz_html::HtmlDocument::from_html(
            html,
            blitz_dom::DocumentConfig {
                base_url: Some("https://example.com/blog/post/".into()),
                ..Default::default()
            },
        )
    }

    fn extract_html(html: &str) -> Option<ReaderDoc> {
        extract(&document(html))
    }

    const ARTICLE: &str = r#"<!doctype html><html><head><title>Slow Proxies — Tailscale Blog</title></head>
<body>
  <div class="cookie-banner">We use cookies to improve your experience. Accept all</div>
  <header class="site-header"><a href="/">Home</a> <a href="/blog">Blog</a> <a href="/docs">Docs</a></header>
  <nav class="navbar"><a href="/a">Alpha</a> <a href="/b">Beta</a> <a href="/c">Gamma</a></nav>
  <div class="layout">
    <div id="content" role="main">
      <h1>Why the slow proxy is faster than the fast proxy</h1>
      <p>When we first started running proxies at Tailscale, we made a number of assumptions about
         how they would behave under load, and most of those assumptions turned out to be wrong.
         This post is the story of what we learned fixing the tail latency of our coordination plane.</p>
      <p>The proxy network handles the traffic between your devices when a direct connection is not
         possible, punching through NATs, and relaying packets the way TURN does, but at a much
         larger scale than any single protocol was designed for.</p>
      <figure><img src="/images/latency-chart.png" alt="p99 latency chart"/><figcaption>p99 before and after</figcaption></figure>
      <p>Measuring the tail is the hard part: a mean of 20&nbsp;ms hides a p99 of two seconds,
         and users feel the p99, not the mean. We instrumented every hop, and the numbers were
         ugly in a way that pointed straight at the allocator.</p>
    </div>
    <aside class="sidebar related">
      <h3>Related posts</h3>
      <a href="/blog/one">Post one</a> <a href="/blog/two">Post two</a> <a href="/blog/three">Post three</a>
    </aside>
  </div>
  <footer class="site-footer"><a href="/privacy">Privacy</a> <a href="/tos">Terms</a></footer>
</body></html>"#;

    #[test]
    fn article_survives_and_chrome_falls() {
        let doc = extract_html(ARTICLE).expect("the article page yields a reader doc");
        assert!(doc.text_len > MIN_ARTICLE_CHARS);
        // title: the h1 wins over <title>
        assert_eq!(doc.title.as_deref(), Some("Why the slow proxy is faster than the fast proxy"));
        // prose survives (and the header keeps it once, deduped from the body)
        assert!(doc.html.contains("assumptions turned out to be wrong"));
        assert!(doc.html.contains("users feel the p99"));
        assert_eq!(doc.html.matches("class=\"rd-title\"").count(), 1);
        assert_eq!(doc.html.matches("tail latency").count(), 1, "h1 kept once, as the header");
        // images survive
        assert_eq!(doc.images, 1);
        assert!(doc.html.contains("/images/latency-chart.png"));
        // chrome does not
        assert!(!doc.html.contains("We use cookies"));
        assert!(!doc.html.contains("Post two"));
        assert!(!doc.html.contains("Privacy"));
        // no scripts or iframes in the output, ever
        assert!(!doc.html.contains("<script"));
        assert!(!doc.html.contains("iframe"));
    }

    #[test]
    fn hidden_subtrees_are_never_reader_material() {
        // the JS-cleared-content shape: everything readable sits behind
        // display:none / hidden containers plus an empty visible shell
        let html = "<!doctype html><html><body><div id=\"app\"></div>\
            <div style=\"display:none\">[redacted] This entire argument was removed by the \
             author and must not appear in the reader view of this page, ever, at any length.</div>\
            <p>Real visible prose that carries the page and stands on its own two feet just fine.</p>\
            <p>Another substantial paragraph of genuinely visible body text so the article clears \
             the minimum length, with commas, clauses, and everything else real prose has.</p>\
            </body></html>";
        let doc = extract_html(html).expect("extracts");
        assert!(!doc.html.contains("redacted"));
        assert!(doc.html.contains("stands on its own two feet"));
    }

    #[test]
    fn javascript_added_content_is_reader_material() {
        // the live DOM holds what the source never had — this is the whole
        // point of sourcing the extraction from the *document*, not the bytes
        let shell = document("<!doctype html><html><body><div id=\"app\"></div></body></html>");
        assert!(extract(&shell).is_none(), "the bare shell has nothing to read");
        // (a live SPA body, as if XHR had filled it):
        let filled = document("<!doctype html><html><body><div id=\"app\">\
            <h1>Fetched at runtime</h1>\
            <p>This paragraph only ever existed in the live DOM: an XHR response rendered by a \
             client-side framework long after the fetched bytes were parsed, and it must show up.</p>\
            <p>Plus a second paragraph of late-arriving text, with enough prose and commas, clauses \
             and dependents, to carry the reader document on its own.</p></div></body></html>");
        let doc = extract(&filled).expect("the live body extracts");
        assert!(doc.html.contains("only ever existed in the live DOM"));
        assert_eq!(doc.title.as_deref(), Some("Fetched at runtime"));
    }

    #[test]
    fn js_that_cleared_the_page_falls_back_to_the_source() {
        // the acceptance shape: the source carried the article; the failing
        // client-side app emptied the live DOM back to a shell.
        let cleared = document("<!doctype html><html><body><div id=\"root\"></div></body></html>");
        let best = extract_best(Some(&cleared), Some(("https://example.com/blog/slow/", ARTICLE)))
            .expect("source carries the article");
        assert!(best.html.contains("assumptions turned out to be wrong"));

        // and the reverse: a live DOM richer than the fetched bytes (a site
        // that renders its body client-side) keeps the live content
        let live = document("<!doctype html><html><body><article>\
            <p>Solo live paragraph one, the fetched source beside it is nearly empty, so the \
             comparison has a clear winner with plenty of prose characters to count.</p>\
            <p>Solo live paragraph two, still only in the document: the bytes that loaded first \
             had nothing like this, and reader mode shows the live content anyway.</p>\
            </article></body></html>");
        let best = extract_best(
            Some(&live),
            Some(("https://example.com/", "<html><body><div id=app></div></body></html>")),
        )
        .expect("live wins");
        assert!(best.html.contains("Solo live paragraph one"));
    }

    #[test]
    fn links_keep_their_anchors_and_prose_keeps_its_spacing() {
        let html = "<!doctype html><html><body><article>\
            <p>The <strong>proxy network</strong> connects <a href=\"https://example.com/docs/funnel\">the funnel</a> and relay, and inline <em>emphasis</em> keeps its spaces.</p>\
            <p>Nothing else here matters at all, though it does run comfortably past the threshold \
             with filler prose, commas, and a closing clause or two for good measure.</p>\
            </article></body></html>";
        let doc = extract_html(html).expect("extracts");
        assert!(doc.html.contains("<strong>proxy network</strong>"));
        assert!(doc.html.contains("<a href=\"https://example.com/docs/funnel\">the funnel</a>"));
        assert!(doc.html.contains("connects <a"), "space before the anchor survives");
        assert!(doc.html.contains("</a> and relay"));
    }

    #[test]
    fn text_only_page_with_no_structure_still_extracts() {
        // fallback path: paragraphs with no scored container at all
        let html = "<!doctype html><html><body>\
            <span>header junk</span>\
            <p>A paragraph that stands alone with no container around it, and enough prose in it \
             to be worth reading on its own.</p>\
            <p>A second free-standing paragraph, similarly long, carrying the page together with \
             the first so that together they clear the extraction threshold.</p>\
            </body></html>";
        let doc = extract_html(html).expect("fallback fires");
        assert!(doc.html.contains("stands alone with no container"));
        assert!(doc.html.contains("A second free-standing paragraph"));
        assert!(!doc.html.contains("header junk"));
    }

    #[test]
    fn placeholders_lose_to_lazy_attributes() {
        let html = "<!doctype html><html><body><article>\
            <p>Prose before the image, long enough to matter, with commas, clauses and the rest.</p>\
            <img src=\"data:image/gif;base64,R0lGOD\" data-src=\"https://cdn.example/photo.jpg\"/>\
            <p>Prose after the image, also long enough that the pair of them clears the extraction threshold handily.</p>\
            </article></body></html>";
        let doc = extract_html(html).expect("extracts");
        assert!(doc.html.contains("https://cdn.example/photo.jpg"));
        assert!(!doc.html.contains("R0lGOD"));
    }

    #[test]
    fn extraction_needs_real_prose() {
        assert!(extract_html("<html><body><p>too short</p></body></html>").is_none());
        assert!(extract_html("<html><body><script>document.title='x'</script></body></html>").is_none());
    }

    #[test]
    fn whitespace_normalization() {
        assert_eq!(normalize_ws("  hello \n world  "), " hello world ");
        assert_eq!(normalize_ws("\n\n"), " ");
        assert_eq!(normalize_ws("x"), "x");
    }

    #[test]
    fn escaped_output_cannot_break_the_document() {
        let html = "<!doctype html><html><body><article>\
            <p>Literal &lt;b&gt;tags&lt;/b&gt; &amp; quotes \"quoted\" in prose that runs long \
             enough, with commas, to be an article about markup escaping itself.</p>\
            <p>A second paragraph keeps the length over the threshold, closing the article neatly.</p>\
            </article></body></html>";
        let doc = extract_html(html).expect("extracts");
        assert!(doc.html.contains("&lt;b&gt;tags&lt;/b&gt;"));
        assert!(doc.html.contains("&quot;quoted&quot;"));
    }

    #[test]
    fn parent_lookup_round_trips() {
        let doc = document("<div><p>text</p></div>");
        let mut arena = vec![RNode::new("root".into())];
        for c in doc.root_node().children.iter().copied() {
            collect(&doc, c, &mut arena, 0, 0);
        }
        let p = (0..arena.len()).find(|i| arena[*i].tag == "p").expect("p collected");
        let div = parent_of(&arena, p).expect("p has a parent");
        assert_eq!(arena[div].tag, "div");
    }
}
