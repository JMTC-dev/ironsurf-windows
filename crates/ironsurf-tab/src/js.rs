//! The Boa JavaScript layer (task 8): `<script>` extraction plus a
//! deliberately tiny binding surface, per `ARCHITECTURE.md` §JavaScript (Boa).
//!
//! Per navigation the runtime rebuilds **one** `boa_engine::Context`
//! (fresh realm per document, no cross-document leakage), binds:
//!
//! - `window` — eventTarget-ish no-ops (`addEventListener`/`removeEventListener`
//!   /`dispatchEvent`), `window.window === window`, `location` reflecting the
//!   current document URL; `href=`/`assign`/`replace`/`reload` queue a real
//!   navigation ([`HostState::pending_nav`]) that the tab's poll loop follows
//!   (search engines redirect this way),
//! - `document` — `title` get/set (a set stores the title, marks the host
//!   `title_assigned` so the tab emits `FromTab::TitleChanged` and repaints),
//!   `body.textContent` as a read-only snapshot of the parsed body,
//!   no-op listeners,
//! - `console.log/info/warn/error` — tab stderr **and** the per-navigation
//!   capture the tab turns into `FromTab::StatusText`,
//! - `setTimeout`/`setInterval`/`clearTimeout`/`clearInterval`/
//!   `requestAnimationFrame`/`cancelAnimationFrame` — **real timers**: the
//!   natives append entries to a [`TimerQueue`] owned by the [`JsRuntime`],
//!   and the tab's poll loop executes due callbacks (`run_due_timers`)
//!   between polls. rAF is a 16 ms one-shot. A throwing callback is caught,
//!   reported like any script error, and never stops later timers.
//!
//! There is no DOM mutation and no engine-internal DOM binding: the glue below
//! is the whole surface. Every script (and every timer callback) is evaluated
//! inside a catch — a throwing script is reported (`ScriptError`, turned into
//! `FromTab::StatusText` by the runtime) and the next one still runs.
//!
//! The context is **not** owned by the initial script run: [`JsRuntime`]
//! (Context + host + timer queue) lives for the lifetime of the document, so
//! callbacks scheduled by page scripts survive the first run and fire from
//! the poll loop until the next navigation replaces the runtime.
//!
//! The bindings reach the tab through a thread-local host pointer: blitz's
//! `HtmlDocument` (and therefore script execution) lives on exactly one thread
//! (`docs/BLITZ_API_NOTES.md` §8), so a `thread_local!` stack is enough and
//! keeps every native closure `Copy` (a boa requirement for
//! [`NativeFunction::from_copy_closure`]). Host borrows are never held across
//! code that can re-enter JavaScript.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use boa_engine::property::Attribute;
use boa_engine::{Context, JsNativeError, JsString, JsValue, NativeFunction, Source};
use blitz_dom::{local_name, BaseDocument, LocalName, Node, NodeData, NodeId, Prefix, QualName};
use blitz_html::HtmlDocument;
use url::Url;

// Page-side diagnostics: `console.*` and script errors go to stderr at the
// level the page itself asked for (see [`log_console`]).
use ironsurf_common::{error, info, trace, warn};

/// Emit one `console.*` line to stderr at the level the page chose.
/// The same line also reaches the shell (status text) via the host capture.
fn log_console(level: ConsoleLevel, url: &str, text: &str) {
    match level {
        ConsoleLevel::Error => error!("page console [{level}] {url}: {text}"),
        ConsoleLevel::Warn => warn!("page console [{level}] {url}: {text}"),
        ConsoleLevel::Info | ConsoleLevel::Log => {
            info!("page console [{level}] {url}: {text}")
        }
    }
}

// ---------------------------------------------------------------------------
// Script extraction

/// A script collected from the document, ready to evaluate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScriptSource {
    /// The JavaScript text.
    pub code: String,
    /// Where it came from: `"inline"` for `<script>` bodies, otherwise the
    /// resolved `src` URL (used in diagnostics and status texts).
    pub origin: String,
}

/// What [`extract_scripts`] found: executable scripts in document order, plus
/// human-readable notes about everything it deliberately skipped.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Extraction {
    pub scripts: Vec<ScriptSource>,
    pub notes: Vec<String>,
}

fn is_script(node: &Node) -> bool {
    node.element_data()
        .is_some_and(|el| el.name.local == local_name!("script"))
}

/// Manual pre-order walk (document order) used when the selector engine
/// rejects `"script"` — extraction must never silently produce nothing.
fn walk_preorder(doc: &BaseDocument, node_id: NodeId, out: &mut Vec<NodeId>) {
    out.push(node_id);
    if let Some(node) = doc.get_node(node_id) {
        for child in node.children.iter().copied() {
            walk_preorder(doc, child, out);
        }
    }
}

/// Classic-script `type` values we execute. `None`/empty means "no type
/// attribute" (a classic script); ES modules are *not* included — they need a
/// module resolver we do not have yet (TODO).
fn is_classic_script_type(type_attr: Option<&str>) -> bool {
    let Some(raw) = type_attr.map(str::trim).filter(|t| !t.is_empty()) else {
        return true;
    };
    let lowered = raw.to_ascii_lowercase();
    matches!(
        lowered.as_str(),
        "text/javascript"
            | "application/javascript"
            | "application/x-javascript"
            | "text/ecmascript"
            | "application/ecmascript"
            | "text-javascript"
            | "javascript"
            | "ecmascript"
            | "module-1.0"
    ) || lowered.starts_with("text/javascript;")
}

/// ES-module `type` values (skipped with a note, not executed).
fn is_module_type(type_attr: Option<&str>) -> bool {
    type_attr
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .is_some_and(|t| t.eq_ignore_ascii_case("module"))
}

/// Collect the `<script>` elements of `doc` **in document order**.
///
/// Inline scripts contribute their text content; `src=` scripts are resolved
/// against `base` and fetched through `fetch` (the runtime passes its own
/// document fetcher, so the tab's net policy applies to scripts too). A script
/// that cannot be fetched, or whose `type` we do not execute, is recorded in
/// [`Extraction::notes`] and skipped — it must never abort the page.
pub fn extract_scripts(
    doc: &BaseDocument,
    base: &Url,
    fetch: &mut dyn FnMut(&Url) -> Result<String, String>,
) -> Extraction {
    let mut extraction = Extraction::default();

    let ids = match doc.query_selector_all("script") {
        Ok(ids) => ids.to_vec(),
        Err(_) => {
            let mut all = Vec::new();
            walk_preorder(doc, doc.root_node().id, &mut all);
            all.into_iter().filter(|id| is_script(doc.get_node(*id).expect("visited node"))).collect()
        }
    };

    for id in ids {
        let Some(node) = doc.get_node(id) else { continue };
        let Some(element) = node.element_data() else { continue };
        let type_attr = element.attr(local_name!("type"));

        if is_module_type(type_attr) {
            extraction
                .notes
                .push("skipped type=\"module\" script (module loading TODO)".to_string());
            continue;
        }
        if !is_classic_script_type(type_attr) {
            // JSON blobs, `<template>`-style data, unknown languages.
            extraction.notes.push(format!(
                "skipped script with unsupported type {:?}",
                type_attr.unwrap_or_default().trim()
            ));
            continue;
        }

        let src = element
            .attr(local_name!("src"))
            .map(str::trim)
            .filter(|src| !src.is_empty())
            .map(str::to_owned);

        let (code, origin) = match src {
            Some(raw) => match base.join(&raw) {
                Ok(url) => match fetch(&url) {
                    Ok(body) => (body, url.as_str().to_string()),
                    Err(err) => {
                        extraction.notes.push(format!("script {raw:?} failed: {err}"));
                        continue;
                    }
                },
                Err(err) => {
                    extraction
                        .notes
                        .push(format!("script src {raw:?} could not be resolved: {err}"));
                    continue;
                }
            },
            // Inline: the parser keeps `<script>` bodies as raw text nodes.
            None => (node.text_content(), "inline".to_string()),
        };

        if code.trim().is_empty() {
            continue;
        }
        extraction.scripts.push(ScriptSource { code, origin });
    }

    extraction
}

// ---------------------------------------------------------------------------
// Host state (what the bindings read/write on the Rust side)

/// `console` level, restricted to the four names the contract binds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConsoleLevel {
    Log,
    Info,
    Warn,
    Error,
}

impl ConsoleLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            ConsoleLevel::Log => "log",
            ConsoleLevel::Info => "info",
            ConsoleLevel::Warn => "warn",
            ConsoleLevel::Error => "error",
        }
    }

    fn parse(raw: &str) -> Self {
        match raw {
            "info" => ConsoleLevel::Info,
            "warn" => ConsoleLevel::Warn,
            "error" => ConsoleLevel::Error,
            _ => ConsoleLevel::Log,
        }
    }
}

impl std::fmt::Display for ConsoleLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Per-navigation host state shared between the tab runtime and the JS
/// bindings. Everything the engine gives back is data (title text, console
/// lines, notes) — never DOM mutations.
#[derive(Clone, Debug, Default)]
pub struct HostState {
    /// `document.title` as JavaScript reads it: seeded with the parsed title,
    /// then whatever a script assigned.
    pub title: String,
    /// `Some` once a script assigned `document.title`. The runtime turns this
    /// into `FromTab::TitleChanged` and a repaint request.
    pub title_assigned: Option<String>,
    /// Captured `console.*` output (level + already stringified message).
    pub console: Vec<(ConsoleLevel, String)>,
    /// Human-readable notes raised by the bindings (timer calls) — the runtime
    /// shows the newest as `FromTab::StatusText`.
    pub notes: Vec<String>,
    /// A page-driven navigation (`location.href=`/`assign`/`replace`/`reload`)
    /// waiting for the tab: `href=`/`assign` push history, `replace`/`reload`
    /// replace the current entry. Taken by the tab after every JS turn
    /// ([`JsRuntime::take_pending_navigation`]); the last write of a turn wins.
    pub pending_nav: Option<PendingNav>,
    /// A script changed something that needs a fresh frame.
    pub repaint: bool,
    /// Number of timer/rAF scheduling calls (stats + debug).
    pub timer_calls: u32,
    /// Number of timer callbacks the poll loop has executed.
    pub timers_fired: u32,
    /// Read-only snapshot of `document.body.textContent`
    /// (`None` = the document has no `<body>`).
    pub body_text: Option<String>,
    /// URL the bindings report through `location.href`.
    pub url: String,
    /// Video (task: playback): the `<video>` half of the host. Page reads
    /// (`v.currentTime`, `v.duration`, …) see the snapshot the tab's last
    /// media pump wrote; page writes (`v.play()`, `v.currentTime = 5`, …)
    /// queue commands the poll loop applies to the real players. The type
    /// lives in `runtime` — the tab owns both ends of this cell.
    pub media: crate::runtime::MediaHost,
}

impl HostState {
    /// `url` — the document URL; `title` — the title blitz parsed (`None` when
    /// the page has no `<title>`, which JS reads as `""`); `body_text` — the
    /// body's `textContent` snapshot.
    pub fn new(url: impl Into<String>, title: Option<&str>, body_text: Option<String>) -> Self {
        HostState {
            title: title.unwrap_or_default().to_string(),
            body_text,
            url: url.into(),
            ..HostState::default()
        }
    }

    /// Newest note worth showing the user, if any.
    pub fn latest_note(&self) -> Option<&str> {
        self.notes.last().map(String::as_str)
    }
}

/// One page-driven navigation waiting for the tab's poll loop.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PendingNav {
    /// Raw target as the page wrote it (absolute or relative; may itself be a
    /// `javascript:` program). The tab resolves it against the document URL.
    pub target: String,
    /// `location.replace`/`reload` semantics: replace the current history
    /// entry instead of pushing a new one.
    pub replace: bool,
}

/// Shared handle (the tab keeps ownership, the bindings borrow it while the
/// scripts run on the same thread).
pub type HostRef = Rc<RefCell<HostState>>;

/// One scheduled timer. Callbacks are `JsValue`s from the owning [`JsRuntime`]'s
/// realm (boa 0.22 keeps GC objects alive while a `JsValue` handle exists, so
/// storing them alongside the context is safe).
#[derive(Clone, Debug)]
pub struct TimerEntry {
    /// The id handed back to the page.
    pub id: u32,
    /// The callback to evaluate when due.
    pub callback: JsValue,
    /// `setInterval` entry (re-armed after each fire).
    pub interval: bool,
    /// `requestAnimationFrame` entry (fires with a timestamp argument).
    pub raf: bool,
    /// When the tab's poll loop should run it.
    pub due: Instant,
    /// Re-arm period for intervals.
    pub period: Duration,
}

/// Timer store shared between the JS natives and the tab's poll loop (the
/// [`JsRuntime`] owns it; natives reach it through the active-scope stack).
#[derive(Debug)]
pub struct TimerQueue {
    /// Ids start at 1: `0` is the "not scheduled" return browsers use for
    /// non-callable callbacks, so a live timer must never report it.
    next_id: u32,
    entries: Vec<TimerEntry>,
}

impl Default for TimerQueue {
    fn default() -> Self {
        TimerQueue {
            next_id: 1,
            entries: Vec::new(),
        }
    }
}

impl TimerQueue {
    /// Schedule `callback`; returns the page-visible timer id.
    fn schedule(
        &mut self,
        callback: JsValue,
        delay: Duration,
        interval: bool,
        raf: bool,
        now: Instant,
    ) -> u32 {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        self.entries.push(TimerEntry {
            id,
            callback,
            interval,
            raf,
            due: now + delay,
            period: delay,
        });
        id
    }

    /// Remove the entry with this id (`clearTimeout`/`clearInterval`/
    /// `cancelAnimationFrame`). True when something was cancelled.
    fn cancel(&mut self, id: u32) -> bool {
        let before = self.entries.len();
        self.entries.retain(|entry| entry.id != id);
        self.entries.len() != before
    }

    /// Earliest deadline the poll loop should wake for.
    fn earliest(&self) -> Option<Instant> {
        self.entries.iter().map(|entry| entry.due).min()
    }

    fn has_entries(&self) -> bool {
        !self.entries.is_empty()
    }

    /// Take every entry due at `now`, in due order. Intervals are re-armed
    /// **before** their callbacks run, so a `clearInterval` from inside a
    /// callback (or from a callback scheduled in the same batch) sticks.
    fn take_due(&mut self, now: Instant) -> Vec<TimerEntry> {
        let mut due: Vec<TimerEntry> = Vec::new();
        let mut keep: Vec<TimerEntry> = Vec::new();
        for entry in std::mem::take(&mut self.entries).drain(..) {
            if entry.due <= now {
                due.push(entry);
            } else {
                keep.push(entry);
            }
        }
        self.entries = keep;
        due.sort_by_key(|entry| entry.due);
        for entry in &due {
            if entry.interval {
                self.entries.push(TimerEntry {
                    due: now + entry.period,
                    ..entry.clone()
                });
            }
        }
        due
    }
}

/// The pair of cells natives resolve while a context owned by a [`JsRuntime`]
/// is evaluating (host state + the live timer queue).
#[derive(Clone)]
struct ActiveScope {
    host: HostRef,
    timers: Rc<RefCell<TimerQueue>>,
}

thread_local! {
    /// Stack of scopes whose contexts are currently evaluating. Natives
    /// resolve the top entry; [`JsRuntime`] pushes for the duration of a
    /// script run or a timer batch.
    static ACTIVE_HOSTS: RefCell<Vec<ActiveScope>> = const { RefCell::new(Vec::new()) };
}

// ---------------------------------------------------------------------------
// DOM bridge (task: give Boa a real DOM via blitz-dom)
//
// Design (see docs/ACID3_STATUS.md):
//
// - While JavaScript runs (a script batch or a timer batch), the live
//   `HtmlDocument` is registered in a thread-local Vec of raw pointers
//   ([`LIVE_DOCS`]); index = docId (0 = the main document). The registration
//   is popped the moment execution returns, so a pointer is never
//   dereferenced outside JavaScript. blitz lives on exactly this thread,
//   and no host borrow outlives the closure that takes it.
// - Node wrappers are plain objects `{__d: docId, __n: nodeId}` sharing one
//   prototype (`__ironsurfNodeProto`, defined in the PRELUDE). Identity is
//   enforced by the wrap native through a realm-side cache object
//   (`__ironsurfNodeCache`, key `"docId:nodeId"`), so two `getElementById`
//   calls return the `===`same wrapper.
// - All behaviour is a native global (`__ironsurfDom*`) taking docId+nodeId
//   as integers; the JS glue on `NodeProto`/`Document` delegates with those
//   ints. NodeList/HTMLCollection/NamedNodeMap are live `Proxy` views that
//   re-read the document through the registry at ACCESS time (Acid3 tests
//   removal-during-iteration).
//
// `NodeId` is the raw `u64` of blitz's versioned id (slotmap: index in the
// low bits, version on top). Slotmap never hands out the null key, so `0`
// is the bridge's "no such node" sentinel, and ids stay exact JS numbers
// for any realistic page (version < 2^21).

/// Sentinel for "no such node" (slotmap null keys are never live nodes).
const NO_NODE: u64 = 0;

thread_local! {
    /// Documents whose JS is currently executing (design: Vec-like, index =
    /// docId; the tab registers its main document around every evaluation).
    static LIVE_DOCS: RefCell<Vec<*mut HtmlDocument>> = const { RefCell::new(Vec::new()) };
}

/// Borrows `doc` mutably for the duration of one evaluation (popped — and
/// thus unreachable from JS — the instant the guard drops).
struct LiveDocGuard<'a> {
    index: usize,
    /// Keeps the registered document's unique borrow alive for the guard's
    /// lifetime: the tab cannot touch (or drop) the document while JS holds
    /// it registered.
    _doc: std::marker::PhantomData<&'a mut HtmlDocument>,
}

fn register_live_doc(doc: &mut HtmlDocument) -> LiveDocGuard<'_> {
    let index = LIVE_DOCS.with(|docs| {
        docs.borrow_mut().push(doc);
        docs.borrow().len() - 1
    });
    LiveDocGuard {
        index,
        _doc: std::marker::PhantomData,
    }
}

impl Drop for LiveDocGuard<'_> {
    fn drop(&mut self) {
        LIVE_DOCS.with(|docs| {
            let mut docs = docs.borrow_mut();
            // LIFO by construction (guards nest); pop the matching frame.
            if docs.len() > self.index {
                docs.truncate(self.index);
            }
        });
    }
}

fn with_live_doc<R>(doc_id: u32, f: impl FnOnce(&mut HtmlDocument) -> R) -> Option<R> {
    LIVE_DOCS.with(|docs| {
        let docs = docs.borrow();
        let ptr = *docs.get(usize::try_from(doc_id).ok()?)?;
        // SAFETY: registered by [`register_live_doc`] for the duration of
        // this very JS execution; nothing else holds the document.
        Some(unsafe { &mut *ptr }).map(f)
    })
}

/// Read-modify-write access: hands over the document's
/// [`blitz_dom::DocumentMutator`] (reads go through `mutator.doc`), flushes
/// the mutator (inline `<style>` processing, title propagation, eager ops),
/// and hands the result back. The runtime's per-batch `settle()` then
/// restyles/relays-out so JS mutations paint.
fn with_mut<R>(
    doc_id: u32,
    f: impl FnOnce(&mut blitz_dom::DocumentMutator<'_>) -> R,
) -> Option<R> {
    with_live_doc(doc_id, |doc| {
        let mut mutator = doc.mutate();
        let outcome = f(&mut mutator);
        mutator.flush();
        outcome
    })
}

/// Parse a document index argument (natives take `(docId, nodeId, …)`).
fn arg_doc(args: &[JsValue], index: usize, context: &mut Context) -> Result<u32, boa_engine::JsError> {
    Ok(args
        .get(index)
        .map(|value| value.to_u32(context))
        .transpose()?
        .unwrap_or(0))
}

/// Parse a raw [`NodeId`] argument (0 = "no such node").
fn arg_node(args: &[JsValue], index: usize, context: &mut Context) -> Result<u64, boa_engine::JsError> {
    Ok(args
        .get(index)
        .map(|value| value.to_number(context))
        .transpose()?
        .map(|n| {
            if n.is_finite() && n >= 0.0 && n < (u64::MAX as f64) {
                // Node ids are exact JS integers well below 2^53.
                n as u64
            } else {
                NO_NODE
            }
        })
        .unwrap_or(NO_NODE))
}

fn node_id(raw: u64) -> NodeId {
    NodeId::from_u64(raw)
}

fn node_raw(id: NodeId) -> u64 {
    id.as_u64()
}

/// Best-effort DOM API name validation (HTML-ish name production; empty and
/// characters outside the allow-list are rejected the way `createElement`
/// and friends reject them).
fn is_valid_dom_name(name: &str) -> bool {
    !name.is_empty()
        && name.chars().all(|c| {
            c.is_ascii_alphanumeric()
                || matches!(c, '-' | '_' | ':' | '.')
        })
}

/// `createElement` rules (task spec): the tag name is lowercased for HTML;
/// prefixed names get an `xlink:` treatment like the parser's, everything
/// else lands in the HTML namespace.
fn qual_name_for_tag(tag: &str) -> Result<QualName, &'static str> {
    let lowered = tag.to_ascii_lowercase();
    if lowered.is_empty() {
        return Err("empty tag name");
    }
    if !is_valid_dom_name(&lowered) {
        return Err("invalid tag name");
    }
    Ok(QualName::new(None, blitz_dom::ns!(html), LocalName::from(lowered.as_str())))
}

/// `setAttribute`/`removeAttribute` name rules: like the tag rules but also
/// rejecting `xmlns` redefinitions and prefix shenanigans (simplified).
fn qual_name_for_attr(name: &str) -> Result<QualName, &'static str> {
    if name.is_empty() || name.eq_ignore_ascii_case("xmlns") || !is_valid_dom_name(name) {
        return Err("invalid attribute name");
    }
    let (prefix, local) = match name.split_once(':') {
        Some((prefix, local)) if !prefix.is_empty() && !local.is_empty() => {
            (Some(prefix.to_ascii_lowercase()), local)
        }
        None => (None, name),
        Some(_) => return Err("invalid attribute name"),
    };
    let local = local.to_ascii_lowercase();
    let ns = match prefix.as_deref() {
        None => blitz_dom::ns!(html),
        Some("xml") => blitz_dom::ns!(xml),
        Some("xlink") => blitz_dom::ns!(xlink),
        Some(_) => blitz_dom::ns!(html),
    };
    Ok(QualName::new(
        prefix.map(Prefix::from),
        ns,
        LocalName::from(local.as_str()),
    ))
}


/// The `<html>` element id (the document element): the first element child
/// of the document root node.
fn html_element_id(doc: &BaseDocument) -> Option<NodeId> {
    let root_id = doc.root_node().id;
    doc.get_node(root_id)?.children.iter().copied().find(|child| {
        doc.get_node(*child)
            .is_some_and(|node| matches!(node.data, NodeData::Element { .. }))
    })
}

// ---------------------------------------------------------------------------
// HTML serialization (shared by innerHTML and the headless `--body-out`)

/// HTML void elements (serialized as `<tag …>` with no end tag).
fn is_void_element(local: &LocalName) -> bool {
    let local: &str = local;
    [
        "area", "base", "basefont", "bgsound", "br", "col", "embed", "frame", "hr", "img",
        "input", "keygen", "link", "meta", "param", "source", "track", "wbr",
    ]
    .contains(&local)
}

/// Serialize the DOM children of `id` (its `innerHTML`).
pub fn serialize_children(doc: &BaseDocument, id: NodeId) -> String {
    let mut out = String::new();
    if let Some(node) = doc.get_node(id) {
        for child in node.children.iter().copied() {
            serialize_node(doc, child, false, &mut out);
        }
    }
    out
}

/// Serialize `id` itself (its `outerHTML`).
pub fn serialize_node_html(doc: &BaseDocument, id: NodeId) -> String {
    let mut out = String::new();
    serialize_node(doc, id, false, &mut out);
    out
}

/// Pre-order serialization of one node and its DOM children.
///
/// `raw` marks being inside a raw-text element (`<script>`/`<style>`/
/// `<textarea>`-alike), whose text children are emitted verbatim — exactly
/// what `innerHTML` returns in a browser. Text gets `&`/`<`/`>` escaped and
/// attribute values get `&`/`"` escaped with proper quoting; void elements
/// get no end tag and nothing ever self-closes. Anonymous blocks
/// (layout-only nodes) are transparent: their children serialize as if direct.
fn serialize_node(doc: &BaseDocument, id: NodeId, raw: bool, out: &mut String) {
    let Some(node) = doc.get_node(id) else { return };
    match &node.data {
        NodeData::Text(text) => {
            if raw {
                out.push_str(&text.content);
            } else {
                escape_text(&text.content, out);
            }
        }
        NodeData::Comment { contents } => {
            out.push_str("<!--");
            out.push_str(contents);
            out.push_str("-->");
        }
        NodeData::Element(element) => {
            let local: &str = &element.name.local;
            // `<script>`/`<style>` bodies are raw text, never escaped markup.
            let child_raw = matches!(local, "script" | "style" | "textarea" | "title");
            out.push('<');
            out.push_str(local);
            if let Some(attrs) = node.data.attrs() {
                for attr in attrs {
                    out.push(' ');
                    let name = attr.name.prefix.as_ref().map_or_else(
                        || attr.name.local.to_string(),
                        |prefix| format!("{prefix}:{}", attr.name.local),
                    );
                    out.push_str(&name);
                    out.push_str("=\"");
                    escape_attr(&attr.value, out);
                    out.push('"');
                }
            }
            out.push('>');
            if !is_void_element(&element.name.local) {
                for child in node.children.iter().copied() {
                    serialize_node(doc, child, child_raw || raw, out);
                }
                out.push_str("</");
                out.push_str(local);
                out.push('>');
            }
        }
        NodeData::AnonymousBlock(_) | NodeData::Document(_) => {
            for child in node.children.iter().copied() {
                serialize_node(doc, child, raw, out);
            }
        }
    }
}

fn escape_text(s: &str, out: &mut String) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(c),
        }
    }
}

fn escape_attr(s: &str, out: &mut String) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
}

// ---------------------------------------------------------------------------
// The DOM natives (`__ironsurfDom*`)


/// Names of the DOM natives (module-private).
mod dom_native {
    pub(super) const HAS_DOC: &str = "__ironsurfDomHasDoc";
    pub(super) const ROOT: &str = "__ironsurfDomRoot";
    pub(super) const HTML: &str = "__ironsurfDomHtml";
    pub(super) const HEAD: &str = "__ironsurfDomHead";
    pub(super) const BODY: &str = "__ironsurfDomBody";
    pub(super) const NODE_TYPE: &str = "__ironsurfDomNodeType";
    pub(super) const NODE_NAME: &str = "__ironsurfDomNodeName";
    pub(super) const DATA: &str = "__ironsurfDomData";
    pub(super) const SET_DATA: &str = "__ironsurfDomSetData";
    pub(super) const PARENT: &str = "__ironsurfDomParent";
    pub(super) const PREVIOUS_SIBLING: &str = "__ironsurfDomPrevSibling";
    pub(super) const NEXT_SIBLING: &str = "__ironsurfDomNextSibling";
    pub(super) const PREVIOUS_ELEMENT_SIBLING: &str = "__ironsurfDomPrevElementSibling";
    pub(super) const NEXT_ELEMENT_SIBLING: &str = "__ironsurfDomNextElementSibling";
    pub(super) const CHILD_COUNT: &str = "__ironsurfDomChildCount";
    pub(super) const CHILD_AT: &str = "__ironsurfDomChildAt";
    pub(super) const FIRST_CHILD: &str = "__ironsurfDomFirstChild";
    pub(super) const LAST_CHILD: &str = "__ironsurfDomLastChild";
    pub(super) const FIRST_ELEMENT_CHILD: &str = "__ironsurfDomFirstElementChild";
    pub(super) const LAST_ELEMENT_CHILD: &str = "__ironsurfDomLastElementChild";
    pub(super) const CREATE_ELEMENT: &str = "__ironsurfDomCreateElement";
    pub(super) const CREATE_TEXT: &str = "__ironsurfDomCreateText";
    pub(super) const CREATE_COMMENT: &str = "__ironsurfDomCreateComment";
    pub(super) const GET_BY_ID: &str = "__ironsurfDomGetById";
    pub(super) const LIVE_ITEM: &str = "__ironsurfDomLiveItem";
    pub(super) const LIVE_COUNT: &str = "__ironsurfDomLiveCount";
    pub(super) const QUERY: &str = "__ironsurfDomQuery";
    pub(super) const APPEND_CHILD: &str = "__ironsurfDomAppendChild";
    pub(super) const INSERT_BEFORE: &str = "__ironsurfDomInsertBefore";
    pub(super) const REMOVE_CHILD: &str = "__ironsurfDomRemoveChild";
    pub(super) const REPLACE_CHILD: &str = "__ironsurfDomReplaceChild";
    pub(super) const DETACH: &str = "__ironsurfDomDetach";
    pub(super) const CLONE: &str = "__ironsurfDomClone";
    pub(super) const TEXT: &str = "__ironsurfDomText";
    pub(super) const TEXT_SET: &str = "__ironsurfDomTextSet";
    pub(super) const SERIALIZE: &str = "__ironsurfDomSerialize";
    pub(super) const SET_INNER: &str = "__ironsurfDomSetInner";
    pub(super) const ATTR: &str = "__ironsurfDomAttr";
    pub(super) const ATTR_SET: &str = "__ironsurfDomAttrSet";
    pub(super) const ATTR_REMOVE: &str = "__ironsurfDomAttrRemove";
    pub(super) const ATTR_COUNT: &str = "__ironsurfDomAttrCount";
    pub(super) const ATTR_AT: &str = "__ironsurfDomAttrAt";
    // Form controls: value / checked (blitz editor + input special data).
    pub(super) const VALUE: &str = "__ironsurfDomValue";
    pub(super) const VALUE_SET: &str = "__ironsurfDomValueSet";
    pub(super) const CHECKED: &str = "__ironsurfDomChecked";
    pub(super) const CHECKED_SET: &str = "__ironsurfDomCheckedSet";
}

/// Live-collection kinds (the `type` argument of `LIVE_ITEM`/`LIVE_COUNT`).
mod live_kind {
    pub(super) const CHILDREN: u32 = 0;
    pub(super) const TAGS: u32 = 1;
    pub(super) const CLASSES: u32 = 2;
    pub(super) const SELECTOR: u32 = 3;
    /// `getElementsByTagName("*")`: every element in tree order.
    pub(super) const ALL_ELEMENTS: u32 = 4;
}

/// Whether `node` is an element (live collections skip anonymous blocks).
fn is_element(node: &Node) -> bool {
    matches!(node.data, NodeData::Element(_))
}

fn child_ids(doc: &BaseDocument, id: NodeId) -> Vec<NodeId> {
    doc.get_node(id)
        .map(|node| node.children.iter().copied().collect())
        .unwrap_or_default()
}

/// Pre-order walk collecting node ids (bounded; used by the `*` collection).
fn elements_in_tree_order(doc: &BaseDocument) -> Vec<NodeId> {
    let mut out = Vec::new();
    fn walk(doc: &BaseDocument, id: NodeId, out: &mut Vec<NodeId>) {
        for child in child_ids(doc, id) {
            if is_element(doc.get_node(child).expect("child is live")) {
                out.push(child);
            }
            walk(doc, child, out);
        }
    }
    walk(doc, doc.root_node().id, &mut out);
    out
}

/// The live membership of a collection (`param` is the lowercased tag name,
/// class name, or selector — read fresh from the document every time).
fn live_ids(doc: &BaseDocument, kind: u32, param: &str) -> Vec<NodeId> {
    match kind {
        live_kind::TAGS | live_kind::CLASSES => {
            let selector = if kind == live_kind::TAGS {
                if param.is_empty() {
                    return Vec::new();
                }
                param.to_string()
            } else {
                if param.split_whitespace().any(str::is_empty) || param.trim().is_empty() {
                    return Vec::new();
                }
                let joined = param
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(".");
                format!(".{joined}")
            };
            doc.query_selector_all(&selector)
                .map(|ids| ids.to_vec())
                .unwrap_or_default()
        }
        live_kind::SELECTOR => doc
            .query_selector_all(param)
            .map(|ids| ids.to_vec())
            .unwrap_or_default(),
        live_kind::ALL_ELEMENTS => elements_in_tree_order(doc),
        _ => Vec::new(),
    }
}

/// Sibling of `node` at `offset` (±1) within its parent's child list.
fn sibling_at(doc: &BaseDocument, id: NodeId, offset: isize) -> Option<NodeId> {
    let node = doc.get_node(id)?;
    let parent = doc.get_node(node.parent?)?;
    let position = parent.children.iter().position(|child| *child == id)?;
    let next = position as isize + offset;
    if next < 0 {
        return None;
    }
    parent.children.get(next as usize).copied()
}

/// Element sibling of `node` at `offset` (skipping non-element siblings).
fn element_sibling_at(doc: &BaseDocument, id: NodeId, offset: isize) -> Option<NodeId> {
    let mut current = sibling_at(doc, id, offset)?;
    loop {
        if doc.get_node(current).is_some_and(is_element) {
            return Some(current);
        }
        current = sibling_at(doc, current, offset)?;
    }
}

fn throw_no_doc() -> boa_engine::JsError {
    JsNativeError::typ()
        .with_message("no live DOM document registered for this script")
        .into()
}

fn arg_string(args: &[JsValue], index: usize, context: &mut Context) -> Result<String, boa_engine::JsError> {
    Ok(args
        .get(index)
        .cloned()
        .unwrap_or(JsValue::undefined())
        .to_string(context)?
        .to_std_string_escaped())
}

fn arg_u32(args: &[JsValue], index: usize, context: &mut Context) -> Result<u32, boa_engine::JsError> {
    Ok(args
        .get(index)
        .map(|value| value.to_u32(context))
        .transpose()?
        .unwrap_or(u32::MAX))
}

fn arg_i64(args: &[JsValue], index: usize, context: &mut Context) -> Result<i64, boa_engine::JsError> {
    Ok(args
        .get(index)
        .map(|value| value.to_number(context))
        .transpose()?
        .map(|n| {
            if n.is_finite() && n >= (i64::MIN as f64) && n < (i64::MAX as f64) {
                n as i64
            } else {
                -1
            }
        })
        .unwrap_or(-1))
}

/// Install every DOM native on `context` (design: all behaviour is native;
/// the JS glue only delegates — see the PRELUDE).
fn register_dom_natives(context: &mut Context) {
    macro_rules! native {
        ($name:expr, $len:expr, $body:expr) => {
            if let Err(err) = context.register_global_builtin_callable(
                JsString::from($name),
                $len,
                NativeFunction::from_copy_closure($body),
            ) {
                error!("js dom binding `{}` failed to install: {err}", $name);
            }
        };
    }

    native!(dom_native::HAS_DOC, 0, |_this, _args, _context| {
        let any = LIVE_DOCS.with(|docs| !docs.borrow().is_empty());
        Ok(JsValue::from(any))
    });
    native!(dom_native::ROOT, 1, |_this, args, context| {
        let doc = arg_doc(args, 0, context)?;
        let raw = with_live_doc(doc, |document| node_raw(document.root_node().id))
            .unwrap_or(NO_NODE);
        Ok(JsValue::new(raw as f64))
    });
    native!(dom_native::HTML, 1, |_this, args, context| {
        let doc = arg_doc(args, 0, context)?;
        let raw = with_live_doc(doc, |document| html_element_id(document).map(node_raw))
            .flatten()
            .unwrap_or(NO_NODE);
        Ok(JsValue::new(raw as f64))
    });
    native!(dom_native::HEAD, 1, |_this, args, context| {
        let doc = arg_doc(args, 0, context)?;
        let raw = with_live_doc(doc, |document| {
            document.find_head_node().map(|node| node_raw(node.id))
        })
        .flatten()
        .unwrap_or(NO_NODE);
        Ok(JsValue::new(raw as f64))
    });
    native!(dom_native::BODY, 1, |_this, args, context| {
        let doc = arg_doc(args, 0, context)?;
        let raw = with_live_doc(doc, |document| {
            document.find_body_node().map(|node| node_raw(node.id))
        })
        .flatten()
        .unwrap_or(NO_NODE);
        Ok(JsValue::new(raw as f64))
    });
    native!(dom_native::NODE_TYPE, 2, |_this, args, context| {
        let doc = arg_doc(args, 0, context)?;
        let node = arg_node(args, 1, context)?;
        let kind = with_live_doc(doc, |document| {
            document
                .get_node(node_id(node))
                .map(|node| match node.data {
                    NodeData::Element(_) | NodeData::AnonymousBlock(_) => 1u8,
                    NodeData::Text(_) => 3,
                    NodeData::Comment { .. } => 8,
                    NodeData::Document(_) => 9,
                })
        })
        .flatten()
        .unwrap_or(0);
        Ok(JsValue::new(i32::from(kind)))
    });
    native!(dom_native::NODE_NAME, 2, |_this, args, context| {
        let doc = arg_doc(args, 0, context)?;
        let node = arg_node(args, 1, context)?;
        let name = with_live_doc(doc, |document| {
            document.get_node(node_id(node)).map(|node| match &node.data {
                NodeData::Element(element) => element.name.local.to_uppercase(),
                NodeData::AnonymousBlock(_) => String::new(),
                NodeData::Text(_) => "#text".to_string(),
                NodeData::Comment { .. } => "#comment".to_string(),
                NodeData::Document(_) => "#document".to_string(),
            })
        })
        .flatten()
        .unwrap_or_default();
        Ok(JsValue::from(JsString::from(name)))
    });
    native!(dom_native::DATA, 2, |_this, args, context| {
        let doc = arg_doc(args, 0, context)?;
        let node = arg_node(args, 1, context)?;
        let data = with_live_doc(doc, |document| {
            document.get_node(node_id(node)).and_then(|node| match &node.data {
                NodeData::Text(text) => Some(text.content.to_string()),
                NodeData::Comment { contents } => Some(contents.clone()),
                _ => None,
            })
        })
        .flatten();
        Ok(match data {
            Some(value) => JsValue::from(JsString::from(value)),
            None => JsValue::null(),
        })
    });
    native!(dom_native::SET_DATA, 3, |_this, args, context| {
        let doc = arg_doc(args, 0, context)?;
        let node = arg_node(args, 1, context)?;
        let value = arg_string(args, 2, context)?;
        let Some(outcome) = with_mut(doc, move |mutator| -> Result<(), boa_engine::JsNativeError> {
            let target = mutator
                .doc
                .get_node(node_id(node))
                .ok_or_else(|| JsNativeError::typ().with_message("NotFoundError: node is gone"))?;
            match target.data {
                NodeData::Text(_) => mutator.set_node_text(node_id(node), &value),
                NodeData::Comment { .. } => {
                    if let Some(target) = mutator.doc.get_node_mut(node_id(node)) {
                        if let NodeData::Comment { contents } = &mut target.data {
                            *contents = value;
                        }
                    }
                }
                _ => {}
            }
            Ok(())
        }) else {
            return Err(throw_no_doc());
        };
        outcome.map_err(boa_engine::JsError::from_native)?;
        Ok(JsValue::undefined())
    });
    native!(dom_native::PARENT, 2, |_this, args, context| {
        let doc = arg_doc(args, 0, context)?;
        let node = arg_node(args, 1, context)?;
        let raw = with_live_doc(doc, |document| {
            document
                .get_node(node_id(node))
                .and_then(|node| node.parent)
                .map_or(NO_NODE, node_raw)
        })
        .unwrap_or(NO_NODE);
        Ok(JsValue::new(raw as f64))
    });
    native!(dom_native::PREVIOUS_SIBLING, 2, |_this, args, context| {
        let doc = arg_doc(args, 0, context)?;
        let node = arg_node(args, 1, context)?;
        let raw = with_live_doc(doc, |document| {
            sibling_at(document, node_id(node), -1).map_or(NO_NODE, node_raw)
        })
        .unwrap_or(NO_NODE);
        Ok(JsValue::new(raw as f64))
    });
    native!(dom_native::NEXT_SIBLING, 2, |_this, args, context| {
        let doc = arg_doc(args, 0, context)?;
        let node = arg_node(args, 1, context)?;
        let raw = with_live_doc(doc, |document| {
            sibling_at(document, node_id(node), 1).map_or(NO_NODE, node_raw)
        })
        .unwrap_or(NO_NODE);
        Ok(JsValue::new(raw as f64))
    });
    native!(dom_native::PREVIOUS_ELEMENT_SIBLING, 2, |_this, args, context| {
        let doc = arg_doc(args, 0, context)?;
        let node = arg_node(args, 1, context)?;
        let raw = with_live_doc(doc, |document| {
            element_sibling_at(document, node_id(node), -1).map_or(NO_NODE, node_raw)
        })
        .unwrap_or(NO_NODE);
        Ok(JsValue::new(raw as f64))
    });
    native!(dom_native::NEXT_ELEMENT_SIBLING, 2, |_this, args, context| {
        let doc = arg_doc(args, 0, context)?;
        let node = arg_node(args, 1, context)?;
        let raw = with_live_doc(doc, |document| {
            element_sibling_at(document, node_id(node), 1).map_or(NO_NODE, node_raw)
        })
        .unwrap_or(NO_NODE);
        Ok(JsValue::new(raw as f64))
    });
    native!(dom_native::CHILD_COUNT, 2, |_this, args, context| {
        let doc = arg_doc(args, 0, context)?;
        let node = arg_node(args, 1, context)?;
        let count = with_live_doc(doc, |document| {
            document
                .get_node(node_id(node))
                .map_or(0, |node| node.children.len())
        })
        .unwrap_or(0);
        Ok(JsValue::new(count as f64))
    });
    native!(dom_native::CHILD_AT, 3, |_this, args, context| {
        let doc = arg_doc(args, 0, context)?;
        let node = arg_node(args, 1, context)?;
        let index = arg_i64(args, 2, context)?;
        let raw = with_live_doc(doc, |document| {
            let target = document.get_node(node_id(node))?;
            usize::try_from(index)
                .ok()
                .and_then(|index| target.children.get(index).copied())
                .map(node_raw)
        })
        .flatten()
        .unwrap_or(NO_NODE);
        Ok(JsValue::new(raw as f64))
    });
    native!(dom_native::FIRST_CHILD, 2, |_this, args, context| {
        let doc = arg_doc(args, 0, context)?;
        let node = arg_node(args, 1, context)?;
        let raw = with_live_doc(doc, |document| {
            document
                .get_node(node_id(node))
                .and_then(|node| node.children.first().copied())
                .map(node_raw)
        })
        .flatten()
        .unwrap_or(NO_NODE);
        Ok(JsValue::new(raw as f64))
    });
    native!(dom_native::LAST_CHILD, 2, |_this, args, context| {
        let doc = arg_doc(args, 0, context)?;
        let node = arg_node(args, 1, context)?;
        let raw = with_live_doc(doc, |document| {
            document
                .get_node(node_id(node))
                .and_then(|node| node.children.last().copied())
                .map(node_raw)
        })
        .flatten()
        .unwrap_or(NO_NODE);
        Ok(JsValue::new(raw as f64))
    });
    native!(dom_native::FIRST_ELEMENT_CHILD, 2, |_this, args, context| {
        let doc = arg_doc(args, 0, context)?;
        let node = arg_node(args, 1, context)?;
        let raw = with_live_doc(doc, |document| {
            child_ids(document, node_id(node))
                .into_iter()
                .find(|child| document.get_node(*child).is_some_and(is_element))
                .map(node_raw)
        })
        .flatten()
        .unwrap_or(NO_NODE);
        Ok(JsValue::new(raw as f64))
    });
    native!(dom_native::LAST_ELEMENT_CHILD, 2, |_this, args, context| {
        let doc = arg_doc(args, 0, context)?;
        let node = arg_node(args, 1, context)?;
        let raw = with_live_doc(doc, |document| {
            child_ids(document, node_id(node))
                .into_iter()
                .rev()
                .find(|child| document.get_node(*child).is_some_and(is_element))
                .map(node_raw)
        })
        .flatten()
        .unwrap_or(NO_NODE);
        Ok(JsValue::new(raw as f64))
    });
    native!(dom_native::CREATE_ELEMENT, 2, |_this, args, context| {
        let doc = arg_doc(args, 0, context)?;
        let tag = arg_string(args, 1, context)?;
        let name = qual_name_for_tag(&tag)
            .map_err(|reason| {
                JsNativeError::range().with_message(format!("InvalidCharacterError: {reason}"))
            })
            .map_err(boa_engine::JsError::from_native)?;
        let raw = with_mut(doc, move |mutator| node_raw(mutator.create_element(name, Vec::new())))
            .unwrap_or(NO_NODE);
        Ok(JsValue::new(raw as f64))
    });
    native!(dom_native::CREATE_TEXT, 2, |_this, args, context| {
        let doc = arg_doc(args, 0, context)?;
        let text = arg_string(args, 1, context)?;
        let raw = with_mut(doc, |mutator| node_raw(mutator.create_text_node(&text)))
            .unwrap_or(NO_NODE);
        Ok(JsValue::new(raw as f64))
    });
    native!(dom_native::CREATE_COMMENT, 2, |_this, args, context| {
        let doc = arg_doc(args, 0, context)?;
        let text = arg_string(args, 1, context)?;
        let raw = with_mut(doc, |mutator| node_raw(mutator.create_comment_node(&text)))
            .unwrap_or(NO_NODE);
        Ok(JsValue::new(raw as f64))
    });
    native!(dom_native::GET_BY_ID, 2, |_this, args, context| {
        let doc = arg_doc(args, 0, context)?;
        let id = arg_string(args, 1, context)?;
        let raw = with_live_doc(doc, |document| {
            document.get_element_by_id(&id).map_or(NO_NODE, node_raw)
        })
        .unwrap_or(NO_NODE);
        Ok(JsValue::new(raw as f64))
    });
    native!(dom_native::LIVE_COUNT, 4, |_this, args, context| {
        let doc = arg_doc(args, 0, context)?;
        let kind = arg_u32(args, 1, context)?;
        let raw = arg_node(args, 2, context)?;
        let param = arg_string(args, 3, context)?;
        let count = if kind == live_kind::CHILDREN {
            with_live_doc(doc, |document| {
                document
                    .get_node(node_id(raw))
                    .map_or(0, |node| node.children.len())
            })
            .unwrap_or(0)
        } else if kind == live_kind::SELECTOR {
            // querySelectorAll with an unparsable selector must raise
            // SyntaxError (task contract), so propagate the parse error.
            let count = with_live_doc(doc, |document| {
                document.query_selector_all(&param).map(|ids| ids.len()).map_err(|_| {
                    JsNativeError::syntax().with_message(format!("SyntaxError: invalid selector {param:?}"))
                })
            });
            match count {
                None => return Err(throw_no_doc()),
                Some(Err(err)) => return Err(boa_engine::JsError::from_native(err)),
                Some(Ok(count)) => count,
            }
        } else {
            with_live_doc(doc, |document| live_ids(document, kind, &param).len()).unwrap_or(0)
        };
        Ok(JsValue::new(count as f64))
    });
    native!(dom_native::LIVE_ITEM, 5, |_this, args, context| {
        let doc = arg_doc(args, 0, context)?;
        let kind = arg_u32(args, 1, context)?;
        let raw = arg_node(args, 2, context)?;
        let index = arg_i64(args, 3, context)?;
        let param = arg_string(args, 4, context)?;
        let raw_item = if kind == live_kind::CHILDREN {
            with_live_doc(doc, |document| {
                let target = document.get_node(node_id(raw))?;
                usize::try_from(index)
                    .ok()
                    .and_then(|index| target.children.get(index).copied())
            })
            .flatten()
        } else {
            with_live_doc(doc, |document| {
                let ids = live_ids(document, kind, &param);
                usize::try_from(index)
                    .ok()
                    .and_then(|index| ids.get(index).copied())
            })
            .flatten()
        };
        Ok(JsValue::new(raw_item.map_or(NO_NODE, node_raw) as f64))
    });
    native!(dom_native::QUERY, 2, |_this, args, context| {
        let doc = arg_doc(args, 0, context)?;
        let selector = arg_string(args, 1, context)?;
        // An unparsable selector raises SyntaxError (task contract); a
        // valid-but-empty match is just null at the JS layer.
        let outcome = with_live_doc(doc, |document| {
            document
                .query_selector(&selector)
                .map(|found| found.map_or(NO_NODE, node_raw))
                .map_err(|_| {
                    JsNativeError::syntax()
                        .with_message(format!("SyntaxError: invalid selector {selector:?}"))
                })
        });
        match outcome {
            None => Err(throw_no_doc()),
            Some(Err(err)) => Err(boa_engine::JsError::from_native(err)),
            Some(Ok(raw)) => Ok(JsValue::new(raw as f64)),
        }
    });
    native!(dom_native::APPEND_CHILD, 3, |_this, args, context| {
        let doc = arg_doc(args, 0, context)?;
        let parent = arg_node(args, 1, context)?;
        let child = arg_node(args, 2, context)?;
        let outcome = with_mut(doc, |mutator| -> Result<(), boa_engine::JsNativeError> {
            if mutator.doc.get_node(node_id(parent)).is_none() {
                return Err(JsNativeError::typ().with_message("NotFoundError: parent is gone"));
            }
            if mutator.doc.get_node(node_id(child)).is_none() {
                return Err(JsNativeError::typ().with_message("NotFoundError: child is gone"));
            }
            mutator.append_children(node_id(parent), &[node_id(child)]);
            Ok(())
        });
        match outcome {
            None => Err(throw_no_doc()),
            Some(Err(err)) => Err(boa_engine::JsError::from_native(err)),
            Some(Ok(())) => Ok(JsValue::undefined()),
        }
    });
    native!(dom_native::INSERT_BEFORE, 4, |_this, args, context| {
        let doc = arg_doc(args, 0, context)?;
        let parent = arg_node(args, 1, context)?;
        let child = arg_node(args, 2, context)?;
        let reference = arg_node(args, 3, context)?;
        let outcome = with_mut(doc, |mutator| -> Result<(), boa_engine::JsNativeError> {
            if mutator.doc.get_node(node_id(parent)).is_none() {
                return Err(JsNativeError::typ().with_message("NotFoundError: parent is gone"));
            }
            if mutator.doc.get_node(node_id(child)).is_none() {
                return Err(JsNativeError::typ().with_message("NotFoundError: child is gone"));
            }
            if reference == NO_NODE {
                mutator.append_children(node_id(parent), &[node_id(child)]);
                return Ok(());
            }
            let reference_node = mutator
                .doc
                .get_node(node_id(reference))
                .ok_or_else(|| JsNativeError::typ().with_message("NotFoundError: reference is gone"))?;
            if reference_node.parent != Some(node_id(parent)) {
                return Err(JsNativeError::typ().with_message(
                    "NotFoundError: the reference node is not a child of this node",
                ));
            }
            mutator.insert_nodes_before(node_id(reference), &[node_id(child)]);
            Ok(())
        });
        match outcome {
            None => Err(throw_no_doc()),
            Some(Err(err)) => Err(boa_engine::JsError::from_native(err)),
            Some(Ok(())) => Ok(JsValue::undefined()),
        }
    });
    native!(dom_native::REMOVE_CHILD, 3, |_this, args, context| {
        let doc = arg_doc(args, 0, context)?;
        let parent = arg_node(args, 1, context)?;
        let child = arg_node(args, 2, context)?;
        let outcome = with_mut(doc, |mutator| -> Result<(), boa_engine::JsNativeError> {
            if mutator.doc.get_node(node_id(parent)).is_none() {
                return Err(JsNativeError::typ().with_message("NotFoundError: parent is gone"));
            }
            let child_node = mutator
                .doc
                .get_node(node_id(child))
                .ok_or_else(|| JsNativeError::typ().with_message("NotFoundError: child is gone"))?;
            if child_node.parent != Some(node_id(parent)) {
                return Err(JsNativeError::typ().with_message(
                    "NotFoundError: the child to remove is not a child of this node",
                ));
            }
            mutator.remove_node(node_id(child));
            Ok(())
        });
        match outcome {
            None => Err(throw_no_doc()),
            Some(Err(err)) => Err(boa_engine::JsError::from_native(err)),
            Some(Ok(())) => Ok(JsValue::undefined()),
        }
    });
    native!(dom_native::REPLACE_CHILD, 4, |_this, args, context| {
        let doc = arg_doc(args, 0, context)?;
        let parent = arg_node(args, 1, context)?;
        let new_child = arg_node(args, 2, context)?;
        let old_child = arg_node(args, 3, context)?;
        let outcome = with_mut(doc, |mutator| -> Result<(), boa_engine::JsNativeError> {
            if mutator.doc.get_node(node_id(parent)).is_none() {
                return Err(JsNativeError::typ().with_message("NotFoundError: parent is gone"));
            }
            if mutator.doc.get_node(node_id(new_child)).is_none() {
                return Err(JsNativeError::typ().with_message("NotFoundError: new child is gone"));
            }
            let old = mutator
                .doc
                .get_node(node_id(old_child))
                .ok_or_else(|| JsNativeError::typ().with_message("NotFoundError: old child is gone"))?;
            if old.parent != Some(node_id(parent)) {
                return Err(JsNativeError::typ().with_message(
                    "NotFoundError: the child to replace is not a child of this node",
                ));
            }
            mutator.replace_node_with(node_id(old_child), &[node_id(new_child)]);
            Ok(())
        });
        match outcome {
            None => Err(throw_no_doc()),
            Some(Err(err)) => Err(boa_engine::JsError::from_native(err)),
            Some(Ok(())) => Ok(JsValue::undefined()),
        }
    });
    native!(dom_native::DETACH, 2, |_this, args, context| {
        let doc = arg_doc(args, 0, context)?;
        let node = arg_node(args, 1, context)?;
        let outcome = with_mut(doc, |mutator| -> Result<(), boa_engine::JsNativeError> {
            if mutator.doc.get_node(node_id(node)).is_none() {
                return Err(JsNativeError::typ().with_message("NotFoundError: node is gone"));
            }
            if mutator
                .doc
                .get_node(node_id(node))
                .and_then(|target| target.parent)
                .is_some()
            {
                mutator.remove_node(node_id(node));
            }
            Ok(())
        });
        match outcome {
            None => Err(throw_no_doc()),
            Some(Err(err)) => Err(boa_engine::JsError::from_native(err)),
            Some(Ok(())) => Ok(JsValue::undefined()),
        }
    });
    native!(dom_native::CLONE, 3, |_this, args, context| {
        let doc = arg_doc(args, 0, context)?;
        let node = arg_node(args, 1, context)?;
        let deep = args
            .get(2)
            .cloned()
            .unwrap_or(JsValue::new(false))
            .to_boolean();
        let raw = with_mut(doc, move |mutator| -> Option<u64> {
            if mutator.doc.get_node(node_id(node)).is_none() {
                return None;
            }
            if deep {
                return Some(node_raw(mutator.deep_clone_node(node_id(node))));
            }
            // Shallow clone: same kind/name/attributes, no children. Gather
            // the owned recipe first so the read borrow ends before creating.
            enum ShallowPlan {
                Element(QualName, Vec<blitz_dom::Attribute>),
                Text(String),
                Comment(String),
                Unsupported,
            }
            let plan = {
                let source = mutator.doc.get_node(node_id(node))?;
                match &source.data {
                    NodeData::Element(element) => ShallowPlan::Element(
                        element.name.clone(),
                        element.attrs.iter().cloned().collect(),
                    ),
                    NodeData::Text(text) => ShallowPlan::Text(text.content.to_string()),
                    NodeData::Comment { contents } => ShallowPlan::Comment(contents.clone()),
                    NodeData::AnonymousBlock(_) | NodeData::Document(_) => {
                        ShallowPlan::Unsupported
                    }
                }
            };
            match plan {
                ShallowPlan::Element(name, attrs) => {
                    Some(node_raw(mutator.create_element(name, attrs)))
                }
                ShallowPlan::Text(text) => Some(node_raw(mutator.create_text_node(&text))),
                ShallowPlan::Comment(contents) => {
                    Some(node_raw(mutator.create_comment_node(&contents)))
                }
                ShallowPlan::Unsupported => None,
            }
        })
        .flatten()
        .unwrap_or(NO_NODE);
        Ok(JsValue::new(raw as f64))
    });
    native!(dom_native::TEXT, 2, |_this, args, context| {
        let doc = arg_doc(args, 0, context)?;
        let node = arg_node(args, 1, context)?;
        let text = with_live_doc(doc, |document| {
            document.get_node(node_id(node)).map(|node| node.text_content())
        })
        .flatten();
        Ok(match text {
            Some(value) => JsValue::from(JsString::from(value)),
            None => JsValue::null(),
        })
    });
    native!(dom_native::TEXT_SET, 3, |_this, args, context| {
        let doc = arg_doc(args, 0, context)?;
        let node = arg_node(args, 1, context)?;
        let value = arg_string(args, 2, context)?;
        let outcome = with_mut(doc, move |mutator| -> Result<(), boa_engine::JsNativeError> {
            let kind = mutator
                .doc
                .get_node(node_id(node))
                .map(|target| match target.data {
                    NodeData::Text(_) => 0u8,
                    NodeData::Comment { .. } => 1,
                    _ => 2,
                })
                .ok_or_else(|| JsNativeError::typ().with_message("NotFoundError: node is gone"))?;
            match kind {
                0 => mutator.set_node_text(node_id(node), &value),
                1 => {
                    if let Some(target) = mutator.doc.get_node_mut(node_id(node)) {
                        if let NodeData::Comment { contents } = &mut target.data {
                            *contents = value;
                        }
                    }
                }
                _ => {
                    let text = mutator.create_text_node(&value);
                    mutator.replace_children(node_id(node), &[text]);
                }
            }
            Ok(())
        });
        match outcome {
            None => Err(throw_no_doc()),
            Some(Err(err)) => Err(boa_engine::JsError::from_native(err)),
            Some(Ok(())) => Ok(JsValue::undefined()),
        }
    });
    native!(dom_native::SERIALIZE, 3, |_this, args, context| {
        let doc = arg_doc(args, 0, context)?;
        let node = arg_node(args, 1, context)?;
        let outer = args
            .get(2)
            .cloned()
            .unwrap_or(JsValue::new(false))
            .to_boolean();
        let html = with_live_doc(doc, |document| {
            if outer {
                serialize_node_html(document, node_id(node))
            } else {
                serialize_children(document, node_id(node))
            }
        })
        .unwrap_or_default();
        Ok(JsValue::from(JsString::from(html)))
    });
    native!(dom_native::SET_INNER, 3, |_this, args, context| {
        let doc = arg_doc(args, 0, context)?;
        let node = arg_node(args, 1, context)?;
        let html = arg_string(args, 2, context)?;
        let outcome = with_mut(doc, |mutator| -> Result<(), boa_engine::JsNativeError> {
            if mutator.doc.get_node(node_id(node)).is_none() {
                return Err(JsNativeError::typ().with_message("NotFoundError: node is gone"));
            }
            // Real parse: the HTML parser fragments the markup and moves the
            // resulting children under `node` (innerHTML-set semantics).
            mutator.remove_and_drop_all_children(node_id(node));
            blitz_html::DocumentHtmlParser::parse_inner_html_into_mutator(
                mutator,
                node_id(node),
                &html,
            );
            Ok(())
        });
        match outcome {
            None => Err(throw_no_doc()),
            Some(Err(err)) => Err(boa_engine::JsError::from_native(err)),
            Some(Ok(())) => Ok(JsValue::undefined()),
        }
    });
    native!(dom_native::ATTR, 2, |_this, args, context| {
        let doc = arg_doc(args, 0, context)?;
        let node = arg_node(args, 1, context)?;
        let name = arg_string(args, 2, context)?;
        let value = with_live_doc(doc, |document| {
            let (prefix, local) = split_attr_name(&name);
            let element = document.get_node(node_id(node))?.element_data()?;
            element
                .attrs
                .iter()
                .find(|attr| {
                    attr.name.prefix.as_ref().map(|p| p.to_string()) == prefix
                        && str::eq_ignore_ascii_case(&attr.name.local, local.as_str())
                })
                .map(|attr| attr.value.to_string())
        })
        .flatten();
        Ok(match value {
            Some(value) => JsValue::from(JsString::from(value)),
            None => JsValue::null(),
        })
    });
    native!(dom_native::ATTR_SET, 3, |_this, args, context| {
        let doc = arg_doc(args, 0, context)?;
        let node = arg_node(args, 1, context)?;
        let name = arg_string(args, 2, context)?;
        let value = arg_string(args, 3, context)?;
        let name = qual_name_for_attr(&name)
            .map_err(|reason| {
                JsNativeError::range().with_message(format!("InvalidCharacterError: {reason}"))
            })
            .map_err(boa_engine::JsError::from_native)?;
        let outcome = with_mut(doc, move |mutator| -> Result<(), boa_engine::JsNativeError> {
            if mutator.doc.get_node(node_id(node)).is_none() {
                return Err(JsNativeError::typ().with_message("NotFoundError: node is gone"));
            }
            mutator.set_attribute(node_id(node), name, &value);
            Ok(())
        });
        match outcome {
            None => Err(throw_no_doc()),
            Some(Err(err)) => Err(boa_engine::JsError::from_native(err)),
            Some(Ok(())) => Ok(JsValue::undefined()),
        }
    });
    native!(dom_native::ATTR_REMOVE, 2, |_this, args, context| {
        let doc = arg_doc(args, 0, context)?;
        let node = arg_node(args, 1, context)?;
        let name = arg_string(args, 2, context)?;
        let outcome = with_mut(doc, |mutator| -> Result<(), boa_engine::JsNativeError> {
            if mutator.doc.get_node(node_id(node)).is_none() {
                return Err(JsNativeError::typ().with_message("NotFoundError: node is gone"));
            }
            // Remove the stored attribute whose local name matches (the
            // parser stores full QualNames pages cannot spell).
            let existing = mutator
                .doc
                .get_node(node_id(node))
                .and_then(|node| node.element_data())
                .and_then(|element| {
                    element
                        .attrs
                        .iter()
                        .find(|attr| str::eq_ignore_ascii_case(&attr.name.local, name.as_str()))
                })
                .map(|attr| attr.name.clone());
            if let Some(qual_name) = existing {
                mutator.clear_attribute(node_id(node), qual_name);
            }
            Ok(())
        });
        match outcome {
            None => Err(throw_no_doc()),
            Some(Err(err)) => Err(boa_engine::JsError::from_native(err)),
            Some(Ok(())) => Ok(JsValue::undefined()),
        }
    });
    native!(dom_native::ATTR_COUNT, 2, |_this, args, context| {
        let doc = arg_doc(args, 0, context)?;
        let node = arg_node(args, 1, context)?;
        let count = with_live_doc(doc, |document| {
            document
                .get_node(node_id(node))
                .and_then(|node| node.element_data())
                .map_or(0, |element| element.attrs.len())
        })
        .unwrap_or(0);
        Ok(JsValue::new(count as f64))
    });
    native!(dom_native::ATTR_AT, 3, |_this, args, context| {
        let doc = arg_doc(args, 0, context)?;
        let node = arg_node(args, 1, context)?;
        let index = arg_i64(args, 2, context)?;
        let pair = with_live_doc(doc, |document| {
            document
                .get_node(node_id(node))
                .and_then(|node| node.element_data())
                .and_then(|element| {
                    usize::try_from(index)
                        .ok()
                        .and_then(|index| element.attrs.iter().nth(index))
                })
                .map(|attr| {
                    let name = attr.name.prefix.as_ref().map_or_else(
                        || attr.name.local.to_string(),
                        |prefix| format!("{prefix}:{}", attr.name.local),
                    );
                    (name, attr.value.to_string())
                })
        })
        .flatten();
        Ok(match pair {
            Some((name, value)) => {
                let array = boa_engine::object::builtins::JsArray::from_iter(
                    [
                        JsValue::from(JsString::from(name)),
                        JsValue::from(JsString::from(value)),
                    ],
                    context,
                );
                JsValue::from(array)
            }
            None => JsValue::null(),
        })
    });

    // -- form controls ("<input>"/"<button>" work) ----------------------------

    native!(dom_native::VALUE, 2, |_this, args, context| {
        let doc = arg_doc(args, 0, context)?;
        let node = arg_node(args, 1, context)?;
        let value = with_live_doc(doc, move |document| form_value(document, node_id(node)));
        Ok(match value.flatten() {
            Some(value) => JsValue::from(JsString::from(value)),
            None => JsValue::null(),
        })
    });
    native!(dom_native::VALUE_SET, 3, |_this, args, context| {
        let doc = arg_doc(args, 0, context)?;
        let node = arg_node(args, 1, context)?;
        let value = arg_string(args, 2, context)?;
        // Text inputs/textareas own a live parley editor: replace its text
        // (blitz's source of truth once it has run).
        let editor_text = value.clone();
        let edited = with_live_doc(doc, move |document| {
            let node = document.get_node_mut(node_id(node))?;
            let element = node.element_data_mut()?;
            let input = element.text_input_data_mut()?;
            input.editor.set_text(&editor_text);
            node.mark_ancestors_dirty();
            Some(())
        })
        .flatten();
        if edited.is_some() {
            return Ok(JsValue::undefined());
        }
        // Everything else (and inputs layout has not upgraded yet): the
        // `value` content attribute, through the mutator (restyle hints).
        let outcome = with_mut(doc, move |mutator| -> Result<(), boa_engine::JsNativeError> {
            if mutator.doc.get_node(node_id(node)).is_none() {
                return Err(JsNativeError::typ().with_message("NotFoundError: node is gone"));
            }
            let name = qual_name_for_attr("value").expect("valid attr name");
            mutator.set_attribute(node_id(node), name, &value);
            Ok(())
        });
        match outcome {
            None => Err(throw_no_doc()),
            Some(Err(err)) => Err(boa_engine::JsError::from_native(err)),
            Some(Ok(())) => Ok(JsValue::undefined()),
        }
    });
    native!(dom_native::CHECKED, 2, |_this, args, context| {
        let doc = arg_doc(args, 0, context)?;
        let node = arg_node(args, 1, context)?;
        let checked = with_live_doc(doc, move |document| {
            checkbox_state(document.get_node(node_id(node))?.element_data()?)
        })
        .flatten();
        Ok(match checked {
            Some(checked) => JsValue::new(checked),
            None => JsValue::null(),
        })
    });
    native!(dom_native::CHECKED_SET, 3, |_this, args, context| {
        let doc = arg_doc(args, 0, context)?;
        let node = arg_node(args, 1, context)?;
        let checked = args.get(2).map(|v| v.to_boolean()).unwrap_or(false);
        let outcome = with_mut(doc, move |mutator| -> Result<(), boa_engine::JsNativeError> {
            let target = mutator
                .doc
                .get_node_mut(node_id(node))
                .ok_or_else(|| JsNativeError::typ().with_message("NotFoundError: node is gone"))?;
            let element = target
                .element_data_mut()
                .ok_or_else(|| JsNativeError::typ().with_message("not an element"))?;
            if checkbox_state(element).is_none() {
                return Err(JsNativeError::typ()
                    .with_message("checked is only valid on checkbox/radio inputs"));
            }
            // Keeps the special data AND the ElementState::CHECKED bit (so
            // `:checked` style matching follows) in sync (element.rs).
            element.set_checkbox_input_checked(checked);
            let name = qual_name_for_attr("checked").expect("valid attr name");
            if checked {
                mutator.set_attribute(node_id(node), name, "");
            } else {
                mutator.clear_attribute(node_id(node), name);
            }
            Ok(())
        });
        match outcome {
            None => Err(throw_no_doc()),
            Some(Err(err)) => Err(boa_engine::JsError::from_native(err)),
            Some(Ok(())) => Ok(JsValue::undefined()),
        }
    });
}

// ---------------------------------------------------------------------------
// Form-control value semantics (input / textarea / select / button)
// ---------------------------------------------------------------------------

/// The `type` of an `<input>` element as the DOM sees it (lower-case;
/// unknown/missing falls back to `"text""). `None` for non-input elements.
pub(crate) fn input_type(element: &blitz_dom::node::ElementData) -> Option<String> {
    if element.name.local != local_name!("input") {
        return None;
    }
    let raw = element.attr(local_name!("type")).unwrap_or("text").to_ascii_lowercase();
    const KNOWN: [&str; 21] = [
        "text", "password", "search", "email", "url", "tel", "number", "date", "time", "week",
        "month", "datetime-local", "range", "color", "button", "submit", "reset", "checkbox",
        "radio", "file", "hidden",
    ];
    Some(if KNOWN.contains(&raw.as_str()) {
        raw
    } else {
        "text".to_string()
    })
}

/// `checked` for checkbox/radio inputs (blitz's special data, seeded from
/// the `checked` attribute at parse time); `None` for anything else.
fn checkbox_state(element: &blitz_dom::node::ElementData) -> Option<bool> {
    match input_type(element).as_deref() {
        Some("checkbox") | Some("radio") => Some(element.checkbox_input_checked().unwrap_or(false)),
        _ => None,
    }
}

/// DOM `value` for form controls: the live editor text for text inputs
/// (blitz's source of truth once layout has run), the `value` attribute for
/// buttons and not-yet-upgraded inputs, the text content for textareas,
/// the selected option's value for selects. `None` = no such property
/// (maps to `undefined` in the bridge).
pub(crate) fn form_value(doc: &BaseDocument, id: NodeId) -> Option<String> {
    let node = doc.get_node(id)?;
    let element = node.element_data()?;
    let tag = element.name.local.clone();
    if tag == local_name!("input") {
        // Once the editor exists it holds the live text — even for password
        // inputs (the DOM spec leaks plaintext; parity).
        if let Some(input) = element.text_input_data() {
            return Some(input.editor.raw_text().to_string());
        }
        return Some(match input_type(element).as_deref() {
            Some("checkbox") | Some("radio") => {
                element.attr(local_name!("value")).unwrap_or("on").to_string()
            }
            _ => element.attr(local_name!("value")).unwrap_or_default().to_string(),
        });
    }
    if tag == local_name!("textarea") {
        if let Some(input) = element.text_input_data() {
            return Some(input.editor.raw_text().to_string());
        }
        return Some(node.text_content());
    }
    if tag == local_name!("select") {
        // First `selected` option wins (the dropdown UI stays a no-op, but
        // option selection drives this read side).
        for child in node.children.iter().copied() {
            let Some(child_node) = doc.get_node(child) else {
                continue;
            };
            let Some(option) = child_node.element_data() else {
                continue;
            };
            if option.name.local == local_name!("option")
                && option.has_attr(local_name!("selected"))
            {
                return Some(
                    option
                        .attr(local_name!("value"))
                        .map(str::to_owned)
                        .unwrap_or_else(|| child_node.text_content()),
                );
            }
        }
        return Some(String::new());
    }
    if tag == local_name!("button") {
        return Some(element.attr(local_name!("value")).unwrap_or_default().to_string());
    }
    None
}

/// `"xlink:href"` → `(Some("xlink"), "href")`; plain names have no prefix.
fn split_attr_name(name: &str) -> (Option<String>, String) {
    match name.split_once(':') {
        Some((prefix, local)) if !prefix.is_empty() && !local.is_empty() => {
            (Some(prefix.to_ascii_lowercase()), local.to_ascii_lowercase())
        }
        _ => (None, name.to_ascii_lowercase()),
    }
}

/// Borrow the active host mutably. Never called across a re-entry point (all
/// JS→Rust string conversions happen before the borrow).
fn with_host<R>(f: impl FnOnce(&mut HostState) -> R) -> Option<R> {
    let scope = ACTIVE_HOSTS.with(|stack| stack.borrow().last().cloned())?;
    let outcome = f(&mut scope.host.borrow_mut());
    Some(outcome)
}

/// Mutably access the active timer queue (natives schedule/cancel here).
fn with_timers<R>(f: impl FnOnce(&mut TimerQueue) -> R) -> Option<R> {
    let scope = ACTIVE_HOSTS.with(|stack| stack.borrow().last().cloned())?;
    let outcome = f(&mut scope.timers.borrow_mut());
    Some(outcome)
}

// ---------------------------------------------------------------------------
// Bindings

/// Native/glue names (private to the binding surface; underscore-prefixed so
/// they are obvious in page stack traces).
mod native {
    pub(super) const URL: &str = "__ironsurfUrl";
    pub(super) const BODY_TEXT: &str = "__ironsurfBodyText";
    pub(super) const HAS_BODY: &str = "__ironsurfHasBody";
    pub(super) const TITLE_GET: &str = "__ironsurfTitleGet";
    pub(super) const TITLE_SET: &str = "__ironsurfTitleSet";
    pub(super) const CONSOLE: &str = "__ironsurfConsole";
    pub(super) const NAVIGATE: &str = "__ironsurfNavigate";
    pub(super) const SET_TIMEOUT: &str = "__ironsurfSetTimeout";
    pub(super) const SET_INTERVAL: &str = "__ironsurfSetInterval";
    pub(super) const RAF: &str = "__ironsurfRaf";
    pub(super) const TIMER_CLEAR: &str = "__ironsurfClearTimer";
    pub(super) const MEDIA_GET: &str = "__ironsurfMediaGet";
    pub(super) const MEDIA_CMD: &str = "__ironsurfMediaCmd";
}

/// One-shot delay used for `requestAnimationFrame` (~60 Hz; the tab has no
/// vsync source, a fixed 16 ms frame is good enough).
const RAF_INTERVAL: Duration = Duration::from_millis(16);

/// Coerce a `setTimeout`/`setInterval` delay argument: non-numbers and
/// non-finite values read as 0, and everything is clamped so
/// `setTimeout(fn, 1e300)` cannot poison the poll deadline arithmetic.
fn delay_from(
    args: &[JsValue],
    index: usize,
    context: &mut Context,
) -> Result<Duration, boa_engine::JsError> {
    let raw = args
        .get(index)
        .map(|value| value.to_number(context))
        .transpose()?
        .unwrap_or(0.0);
    if !raw.is_finite() || raw <= 0.0 {
        return Ok(Duration::ZERO);
    }
    Ok(Duration::from_millis(raw.min(86_400_000.0 * 3650.0) as u64))
}

/// Trusted glue installed on top of the natives. Written in JS on purpose:
/// argument coercion (`console.log` of objects, `title` stringification) then
/// runs through JavaScript's own semantics, and the Rust side stays four
/// tiny natives.
const PRELUDE: &str = r##"
(function () {
  var host = globalThis;

  function navTo(target, replace) {
    // Queue a real navigation: the tab's poll loop follows it after the
    // current JS turn (search engines redirect exactly this way).
    host.__ironsurfNavigate(String(target), !!replace);
  }
  function noop() {}
  function returnTrue() { return true; }

  // ---------------------------------------------------------------------
  // DOM events ("<input>, <button> does not work"). blitz's EventDriver
  // performs the default actions (focus, active, caret edits, click
  // landing on the right node) and hands the DOM events it dispatches to
  // the tab process, which replays them through this real EventTarget
  // implementation: capture (window → ancestors), target, bubbling, the
  // inline on<type>="" attribute handlers (compiled per node+type, re-
  // compiled when the attribute text changes), and the `on<type>`
  // property handlers — so `addEventListener`, `onclick = fn` and
  // `<button onclick="...">` all work. Errors inside handlers are
  // reported and never stop the propagation to the next listener.
  // ---------------------------------------------------------------------
  var listenerStore = Object.create(null); // targetId → type → [{fn,capture}]
  var nextTargetId = 1;
  function targetId(obj) {
    if (obj.__ironsurfEtid === undefined) {
      Object.defineProperty(obj, "__ironsurfEtid", {
        value: nextTargetId++,
        enumerable: false,
        configurable: true,
      });
    }
    return obj.__ironsurfEtid;
  }
  function listenerList(obj, type, create) {
    var id = targetId(obj);
    var byType = listenerStore[id];
    if (!byType) {
      if (!create) return null;
      byType = listenerStore[id] = Object.create(null);
    }
    var list = byType[type];
    if (!list) {
      if (!create) return null;
      list = byType[type] = [];
    }
    return list;
  }
  function isListener(fn) {
    return (
      typeof fn === "function" ||
      (fn !== null && typeof fn === "object" && typeof fn.handleEvent === "function")
    );
  }
  function callListener(entry, ev, current) {
    try {
      if (typeof entry.fn === "function") entry.fn.call(current, ev);
      else entry.fn.handleEvent(ev);
    } catch (err) {
      reportError("listener for " + ev.type + ": " + err);
    }
  }
  function addListener(obj, type, fn, options) {
    if (!obj || !isListener(fn)) return;
    var capture = options === true || !!(options && typeof options === "object" && options.capture);
    var list = listenerList(obj, type, true);
    for (var i = 0; i < list.length; i++) {
      if (list[i].fn === fn && list[i].capture === capture) return; // already registered
    }
    list.push({ fn: fn, capture: capture });
  }
  function removeListener(obj, type, fn, options) {
    if (!obj) return;
    var capture = options === true || !!(options && typeof options === "object" && options.capture);
    var list = listenerList(obj, type, false);
    if (!list) return;
    for (var i = 0; i < list.length; i++) {
      if (list[i].fn === fn && list[i].capture === capture) {
        list.splice(i, 1);
        return;
      }
    }
  }
  // Non-bubbling event types (focus/blur fire on the target only).
  var NO_BUBBLE = { focus: 1, blur: 1, focusin_nonbubbling: 1, scroll: 1, load: 1 };
  // Inline `on<type>="..."` attribute handlers, compiled per (node,type);
  // recompiled only when the attribute text changes.
  var inlineCache = Object.create(null); // targetId → type → {src, fn}
  function inlineAttrHandler(node, type) {
    if (node === host || node === document || !node.getAttribute) return null;
    var src = node.getAttribute("on" + type);
    if (src === null || src === "") return null;
    var byType = inlineCache[targetId(node)];
    if (!byType) byType = inlineCache[targetId(node)] = Object.create(null);
    var entry = byType[type];
    if (!entry || entry.src !== src) {
      var compiled = null;
      try {
        compiled = new Function("event", src);
      } catch (err) {
        // A page with an uncompilable handler stays alive; the handler is dead.
        compiled = null;
      }
      entry = byType[type] = { src: src, fn: compiled };
    }
    return entry.fn;
  }
  function makeEvent(type, target, init) {
    init = init || {};
    var ev = {
      type: type,
      target: target,
      currentTarget: target,
      bubbles: init.bubbles !== false && !(init.bubbles === undefined && NO_BUBBLE[type]),
      cancelable: init.cancelable !== false,
      defaultPrevented: false,
      isTrusted: false,
      detail: typeof init.detail === "number" ? init.detail : 0,
      // mouse axes (0 until a real MouseEvent fills them in)
      screenX: 0, screenY: 0, clientX: 0, clientY: 0,
      pageX: 0, pageY: 0, offsetX: 0, offsetY: 0,
      button: 0, buttons: 0,
      relatedTarget: null,
      // key axes
      key: "", code: "", location: 0, repeat: false, isComposing: false,
      ctrlKey: false, shiftKey: false, altKey: false, metaKey: false,
      // input events
      inputType: "input",
      data: null,
      // internal propagation flags
      _stop: false, _stopImmediate: false,
      preventDefault: function () {
        ev.defaultPrevented = true;
      },
      stopPropagation: function () {
        ev._stop = true;
      },
      stopImmediatePropagation: function () {
        ev._stop = true;
        ev._stopImmediate = true;
      },
    };
    return ev;
  }
  // The propagation path: target → … → <html> → document → window.
  function eventPath(target) {
    var path = [];
    if (target && target.__n) {
      var d = target.__d;
      var seen = 0;
      var node = target;
      while (node && node.__n && seen++ < 4096) {
        path.push(node);
        node = host.__ironsurfDomParent(d, node.__n) ? wrapNode(d, host.__ironsurfDomParent(d, node.__n)) : null;
      }
    } else if (target && target !== document && target !== host) {
      path.push(target);
    }
    path.push(document);
    path.push(host);
    return path;
  }
  // One path node: its listeners (filtered by phase), its inline attribute
  // handler and its on<type> property handler.
  function visitNode(node, ev, phase) {
    var list = listenerList(node, ev.type, false);
    if (list) {
      // Snapshot: a listener may add/remove listeners during dispatch.
      var pending = list.slice();
      for (var i = 0; i < pending.length; i++) {
        if (ev._stopImmediate) return;
        var entry = pending[i];
        if (phase === "target" || entry.capture === (phase === "capture")) {
          ev.currentTarget = node;
          callListener(entry, ev, node);
        }
      }
    }
    if (ev._stopImmediate) return;
    // Inline handlers: the attribute form and the property form share the
    // slot semantics of real `on*` DOM attributes (one handler, bubble
    // phase, runs even when no listener was registered).
    var handler = null;
    if (node === host || node === document) {
      var prop = node["on" + ev.type];
      if (typeof prop === "function") handler = prop;
    } else {
      handler = inlineAttrHandler(node, ev.type);
      if (!handler) {
        var own = node["on" + ev.type];
        if (typeof own === "function") handler = own;
      }
    }
    if (handler && (phase === "target" || phase === "bubble")) {
      ev.currentTarget = node;
      try {
        handler.call(node, ev);
      } catch (err) {
        reportError("on" + ev.type + " handler: " + err);
      }
    }
  }
  // Fire one event: capture down, target, bubble up (unless it does not
  // bubble). Returns nothing; `ev.defaultPrevented` tells the caller.
  function firePath(path, ev) {
    for (var i = path.length - 1; i >= 1; i--) {
      if (ev._stop) return;
      visitNode(path[i], ev, "capture");
    }
    if (!ev._stop) visitNode(path[0], ev, "target");
    if (ev.bubbles) {
      for (var j = 1; j < path.length; j++) {
        if (ev._stop) return;
        visitNode(path[j], ev, "bubble");
      }
    }
  }
  // The entry point the tab process calls after blitz ran the default
  // actions for a real user interaction (and what dispatchEvent uses).
  // `init` carries the event axes (mouse coords, key labels, modifiers).
  function fireDomEvent(type, target, init) {
    var ev = makeEvent(type, target, init || {});
    if (init) {
      if (typeof init.clientX === "number") {
        ev.clientX = ev.pageX = ev.offsetX = init.clientX;
        ev.clientY = ev.pageY = ev.offsetY = init.clientY;
        ev.screenX = init.screenX !== undefined ? init.screenX : init.clientX;
        ev.screenY = init.screenY !== undefined ? init.screenY : init.clientY;
      }
      if (typeof init.button === "number") ev.button = init.button;
      if (init.key !== undefined) ev.key = String(init.key);
      if (init.code !== undefined) ev.code = String(init.code);
      if (init.repeat) ev.repeat = true;
      if (init.ctrlKey) ev.ctrlKey = true;
      if (init.shiftKey) ev.shiftKey = true;
      if (init.altKey) ev.altKey = true;
      if (init.metaKey) ev.metaKey = true;
      if (init.inputType !== undefined) ev.inputType = String(init.inputType);
      if (init.data !== undefined) ev.data = init.data;
    }
    firePath(eventPath(target), ev);
    return ev;
  }
  host.__ironsurfFire = function (type, d, n, init) {
    var target = n ? wrapNode(d, n) : host;
    return fireDomEvent(String(type), target, init);
  };
  function dispatchUserEvent(type, targetObj, init) {
    var target = targetObj || host;
    var ev = target && target.__n
      ? host.__ironsurfFire(String(type), target.__d, target.__n, init || {})
      : fireDomEvent(String(type), target, init || {});
    return !ev.defaultPrevented;
  }

  // URL-component decomposition of an href string (no URL parser in the
  // realm; this is the whole algorithm — the reflection is always the tab's
  // own canonical URL string, so a light split is honest here).
  function urlParts(u) {
    u = String(u == null ? "" : u);
    var hash = "";
    var cut = u.indexOf("#");
    if (cut >= 0) { hash = u.slice(cut); u = u.slice(0, cut); }
    var search = "";
    cut = u.indexOf("?");
    if (cut >= 0) { search = u.slice(cut); u = u.slice(0, cut); }
    var protoCut = u.indexOf(":");
    var rest = u;
    var protocol = "";
    if (protoCut > 0 && /^[a-zA-Z][a-zA-Z0-9+.\-]*$/.test(u.slice(0, protoCut))) {
      protocol = u.slice(0, protoCut + 1).toLowerCase();
      rest = u.slice(protoCut + 1);
    }
    var auth = "";
    var path = rest;
    if (rest.charAt(0) === "/" && rest.charAt(1) === "/") {
      var end = rest.length;
      for (var i = 2; i < rest.length; i++) {
        var c = rest.charAt(i);
        if (c === "/" || c === "?" || c === "#") { end = i; break; }
      }
      auth = rest.slice(2, end);
      path = rest.slice(end) || "/";
    }
    var at = auth.indexOf("@");
    var hostport = at >= 0 ? auth.slice(at + 1) : auth;
    return {
      protocol: protocol,
      host: hostport,
      hostname: hostport.replace(/:\d+$/, ""),
      pathname: auth === "" && path === "" ? "" : path,
      search: search,
      hash: hash,
    };
  }

  var location = {
    // The document URL — and a real navigation choke point: `href=`,
    // `assign()` and `replace()` queue the load for the tab's poll loop
    // (`replace` overwrites the current history entry), exactly how
    // search-engine redirect pages send us on.
    get href() {
      return host.__ironsurfUrl;
    },
    set href(target) {
      navTo(target, false);
    },
    assign: function (target) { navTo(target, false); },
    replace: function (target) { navTo(target, true); },
    reload: function () { navTo(host.__ironsurfUrl, true); },
    toString: function () {
      return host.__ironsurfUrl;
    },
    get protocol() { return urlParts(host.__ironsurfUrl).protocol; },
    get host() { return urlParts(host.__ironsurfUrl).host; },
    get hostname() { return urlParts(host.__ironsurfUrl).hostname; },
    get pathname() { return urlParts(host.__ironsurfUrl).pathname; },
    get search() { return urlParts(host.__ironsurfUrl).search; },
    get hash() { return urlParts(host.__ironsurfUrl).hash; },
  };


  // Like browsers, the global object *is* `window`: window.window === window,
  // and page-level `var`s land on the same object. So `window` is not built —
  // it gets decorated, eventTarget no-ops included.
  host.self = host;
  host.window = host;
  host.top = host;
  host.parent = host;
  host.name = "";
  host.closed = false;
  host.devicePixelRatio = 1;
  host.innerWidth = 0;
  host.innerHeight = 0;
  host.outerWidth = 0;
  host.outerHeight = 0;
  host.location = location;
  host.addEventListener = function (type, fn, options) {
    addListener(host, String(type), fn, options);
  };
  host.removeEventListener = function (type, fn, options) {
    removeListener(host, String(type), fn, options);
  };
  host.dispatchEvent = function (ev) {
    // A plain object or the output of makeEvent-style code is accepted;
    // it dispatches on `window` with real capture/bubble propagation.
    if (!ev || typeof ev.type !== "string") return returnTrue();
    var event = ev;
    if (!event.__ironsurfEvent) {
      event = makeEvent(String(ev.type), host, ev);
      Object.keys(ev).forEach(function (k) {
        if (event[k] === undefined && k !== "type") event[k] = ev[k];
      });
    }
    event.target = host;
    firePath(eventPath(host), event);
    return !event.defaultPrevented;
  };
  // Real timers: the natives append to the tab's timer queue and the poll
  // loop runs the callbacks (rAF is a 16 ms one-shot).
  host.setTimeout = host.__ironsurfSetTimeout;
  host.setInterval = host.__ironsurfSetInterval;
  host.clearTimeout = host.__ironsurfClearTimer;
  host.clearInterval = host.__ironsurfClearTimer;
  host.requestAnimationFrame = host.__ironsurfRaf;
  host.cancelAnimationFrame = host.__ironsurfClearTimer;
  host.scrollTo = noop;
  host.scroll = noop;

  // ---------------------------------------------------------------------
  // The DOM bridge (design: wrappers are plain {__d, __n} objects sharing
  // one prototype; every behaviour is a native taking docId+nodeId ints;
  // live lists are Proxies re-reading the document at access time).
  // ---------------------------------------------------------------------

  // Identity: one wrapper per (docId, nodeId) via this realm-side cache.
  var nodeCache = Object.create(null);
  Object.defineProperty(host, "__ironsurfNodeCache", { value: nodeCache });

  function wrapNode(d, id) {
    if (typeof id !== "number" || !id) return null;
    var key = d + ":" + id;
    var found = nodeCache[key];
    if (found !== undefined) return found;
    var made = Object.create(NodeProto);
    Object.defineProperty(made, "__d", { value: d });
    Object.defineProperty(made, "__n", { value: id });
    nodeCache[key] = made;
    return made;
  }
  Object.defineProperty(host, "__ironsurfWrap", { value: wrapNode });

  // Non-index property keys ("length", "item", …) parse to -1.
  function toIndex(key) {
    var s = String(key);
    if (s.length === 0 || s.length > 15) return -1;
    var n = 0;
    for (var i = 0; i < s.length; i++) {
      var c = s.charCodeAt(i);
      if (c < 48 || c > 57) return -1;
      n = n * 10 + (c - 48);
    }
    return n;
  }

  // Live NodeList/HTMLCollection view: the Proxy reads the document fresh
  // on EVERY property access (Acid3: mutations during iteration are seen).
  // kind: 0 children, 1 tags, 2 classes, 3 selector, 4 all elements.
  function makeList(d, kind, node, param) {
    var target = { __d: d, __kind: kind, __node: node, __param: param };
    function count() {
      return host.__ironsurfDomLiveCount(d, kind, node, param);
    }
    function item(i) {
      return wrapNode(d, host.__ironsurfDomLiveItem(d, kind, node, i, param));
    }
    var handlers = {
      get: function (t, key) {
        if (key === "length") return count();
        if (key === "item")
          return function (i) {
            var n = toIndex(i);
            return n < 0 || n >= count() ? null : item(n);
          };
        if (key === "entries")
          return function () {
            var pairs = [];
            for (var i = 0, n = count(); i < n; i++) pairs.push([i, item(i)]);
            return pairs[Symbol.iterator]();
          };
        if (key === "forEach")
          return function (fn) {
            for (var i = 0, n = count(); i < n; i++) fn(item(i), i, view);
          };
        if (typeof key === "symbol") {
          // Iterating the live list yields snapshots; mutations in the
          // middle of iteration are the *page's* problem, not ours.
          if (key === Symbol.iterator)
            return function () {
              var arr = [];
              for (var i = 0, n = count(); i < n; i++) arr.push(item(i));
              return arr[Symbol.iterator]();
            };
          if (key === Symbol.toStringTag) return "NodeList";
          return undefined;
        }
        var i = toIndex(key);
        if (i >= 0) return i < count() ? item(i) : undefined;
        if (key === "toString") return function () { return "[object NodeList]"; };
        return undefined;
      },
      has: function (t, key) {
        if (key === "length" || key === "item" || key === "forEach" || key === "entries")
          return true;
        var i = toIndex(key);
        return i >= 0 && i < count();
      },
      ownKeys: function () {
        var keys = ["length", "item"];
        for (var i = 0, n = count(); i < n; i++) keys.push(String(i));
        return keys;
      },
      getOwnPropertyDescriptor: function (t, key) {
        if (key === "length")
          return { value: count(), writable: false, enumerable: true, configurable: true };
        if (key === "item")
          return { value: this.get(t, "item"), writable: false, enumerable: false, configurable: true };
        var i = toIndex(key);
        if (i >= 0 && i < count())
          return { value: item(i), writable: false, enumerable: true, configurable: true };
        return undefined;
      },
    };
    var view = new Proxy(target, handlers);
    return view;
  }

  // NamedNodeMap: length/item(i)/getNamedItem(name) plus [name] lookup.
  // Attr items are {name, value} snapshots (writing attr.value is a no-op;
  // use setAttribute).
  function makeAttrMap(d, n) {
    var target = { __d: d, __n: n };
    function attrAt(i) {
      var pair = host.__ironsurfDomAttrAt(d, n, i);
      if (pair === null) return null;
      return { name: pair[0], value: pair[1], nodeType: 2 };
    }
    function named(name) {
      var v = host.__ironsurfDomAttr(d, n, String(name));
      return v === null ? null : { name: String(name), value: v, nodeType: 2 };
    }
    var handlers = {
      get: function (t, key) {
        if (key === "length") return host.__ironsurfDomAttrCount(d, n);
        if (key === "item")
          return function (i) {
            var idx = toIndex(i);
            return idx < 0 || idx >= host.__ironsurfDomAttrCount(d, n) ? null : attrAt(idx);
          };
        if (key === "getNamedItem") return function (name) { return named(name); };
        if (typeof key === "symbol") return undefined;
        var v = host.__ironsurfDomAttr(d, n, String(key));
        if (v !== null) return v; // attrs.class === "…" (string, like browsers)
        var i = toIndex(key);
        if (i >= 0) {
          var a = attrAt(i);
          return a === null ? undefined : a;
        }
        return undefined;
      },
      has: function (t, key) {
        if (key === "length" || key === "item" || key === "getNamedItem") return true;
        if (typeof key === "string" && host.__ironsurfDomAttr(d, n, key) !== null) return true;
        var i = toIndex(key);
        return i >= 0 && i < host.__ironsurfDomAttrCount(d, n);
      },
      ownKeys: function () {
        var keys = ["length"];
        var count = host.__ironsurfDomAttrCount(d, n);
        for (var i = 0; i < count; i++) keys.push(String(i));
        return keys;
      },
      getOwnPropertyDescriptor: function (t, key) {
        if (key === "length")
          return { value: host.__ironsurfDomAttrCount(d, n), writable: false, enumerable: true, configurable: true };
        var i = toIndex(key);
        if (i >= 0 && i < host.__ironsurfDomAttrCount(d, n))
          return { value: attrAt(i), writable: false, enumerable: true, configurable: true };
        return undefined;
      },
    };
    return new Proxy(target, handlers);
  }

  // The one Node prototype every wrapper shares (mutating accessors).
  var NodeProto = {};
  Object.defineProperty(host, "__ironsurfNodeProto", { value: NodeProto });

  function protoGet(name, fn) {
    Object.defineProperty(NodeProto, name, {
      get: fn,
      enumerable: false,
      configurable: true,
    });
  }
  function protoAccessor(name, get, set) {
    Object.defineProperty(NodeProto, name, {
      get: get,
      set: set,
      enumerable: false,
      configurable: true,
    });
  }
  function protoMethod(name, fn) {
    Object.defineProperty(NodeProto, name, {
      value: fn,
      writable: true,
      enumerable: false,
      configurable: true,
    });
  }

  protoGet("nodeType", function () { return host.__ironsurfDomNodeType(this.__d, this.__n); });
  protoGet("nodeName", function () { return host.__ironsurfDomNodeName(this.__d, this.__n); });
  protoAccessor(
    "nodeValue",
    function () { return host.__ironsurfDomData(this.__d, this.__n); },
    function (v) { host.__ironsurfDomSetData(this.__d, this.__n, String(v)); }
  );
  protoGet("parentNode", function () {
    return wrapNode(this.__d, host.__ironsurfDomParent(this.__d, this.__n));
  });
  protoGet("parentElement", function () {
    var p = wrapNode(this.__d, host.__ironsurfDomParent(this.__d, this.__n));
    return p !== null && p.nodeType === 1 ? p : null;
  });
  protoGet("ownerDocument", function () { return document; });
  protoGet("isConnected", function () {
    // Connected == still parented somewhere under the document root.
    var n = this.__n;
    while (n) {
      if (n === host.__ironsurfDomRoot(this.__d)) return true;
      n = host.__ironsurfDomParent(this.__d, n);
    }
    return false;
  });
  protoGet("childNodes", function () { return makeList(this.__d, 0, this.__n, ""); });
  protoGet("children", function () {
    // HTMLCollection of element children; recomputed per access (live).
    var kids = this.childNodes;
    var out = [];
    for (var i = 0, n = kids.length; i < n; i++) {
      var kid = kids[i];
      if (kid !== null && kid.nodeType === 1) out.push(kid);
    }
    return out;
  });
  protoMethod("hasChildNodes", function () {
    return host.__ironsurfDomChildCount(this.__d, this.__n) > 0;
  });
  protoGet("firstChild", function () { return wrapNode(this.__d, host.__ironsurfDomFirstChild(this.__d, this.__n)); });
  protoGet("lastChild", function () { return wrapNode(this.__d, host.__ironsurfDomLastChild(this.__d, this.__n)); });
  protoGet("previousSibling", function () { return wrapNode(this.__d, host.__ironsurfDomPrevSibling(this.__d, this.__n)); });
  protoGet("nextSibling", function () { return wrapNode(this.__d, host.__ironsurfDomNextSibling(this.__d, this.__n)); });
  protoGet("firstElementChild", function () { return wrapNode(this.__d, host.__ironsurfDomFirstElementChild(this.__d, this.__n)); });
  protoGet("lastElementChild", function () { return wrapNode(this.__d, host.__ironsurfDomLastElementChild(this.__d, this.__n)); });
  protoGet("previousElementSibling", function () { return wrapNode(this.__d, host.__ironsurfDomPrevElementSibling(this.__d, this.__n)); });
  protoGet("nextElementSibling", function () { return wrapNode(this.__d, host.__ironsurfDomNextElementSibling(this.__d, this.__n)); });
  protoGet("attributes", function () { return makeAttrMap(this.__d, this.__n); });
  protoAccessor(
    "textContent",
    function () {
      var v = host.__ironsurfDomText(this.__d, this.__n);
      return v === null ? "" : v;
    },
    function (v) { host.__ironsurfDomTextSet(this.__d, this.__n, String(v)); }
  );
  protoAccessor(
    "innerHTML",
    function () { return host.__ironsurfDomSerialize(this.__d, this.__n, false); },
    function (v) { host.__ironsurfDomSetInner(this.__d, this.__n, String(v)); }
  );
  protoGet("outerHTML", function () { return host.__ironsurfDomSerialize(this.__d, this.__n, true); });

  // Form control value/checked (task: `<form>`): the natives read the live
  // parley editor text blitz keeps for text inputs and textareas (falling
  // back to the `value` content attribute), and checkbox/radio checkedness
  // (blitz's input special data, mirrored to the `checked` attribute). On a
  // non-control `value`/`checked` stay `undefined`, like the DOM.
  protoAccessor(
    "value",
    function () {
      var v = host.__ironsurfDomValue(this.__d, this.__n);
      return v === null ? undefined : v;
    },
    function (v) { host.__ironsurfDomValueSet(this.__d, this.__n, String(v)); }
  );
  protoAccessor(
    "checked",
    function () {
      var c = host.__ironsurfDomChecked(this.__d, this.__n);
      return c === null ? undefined : !!c;
    },
    function (v) { host.__ironsurfDomCheckedSet(this.__d, this.__n, !!v); }
  );

  // ChildNode mutations: the natives validate (throwing NotFoundError
  // TypeErrors, like DOMExceptions do here) and apply via blitz.
  function unwrapChild(child) {
    if (child === null || child === undefined || typeof child !== "object") {
      throw new TypeError("parameter 1 is not of type 'Node'");
    }
    return child;
  }
  protoMethod("appendChild", function (child) {
    unwrapChild(child);
    if (child.__frag) {
      // DocumentFragment: move all its children across (and empty it).
      var kids = child.__frag.slice();
      child.__frag.length = 0;
      for (var i = 0; i < kids.length; i++) this.appendChild(kids[i]);
      return child;
    }
    host.__ironsurfDomAppendChild(this.__d, this.__n, child.__n);
    return child;
  });
  protoMethod("insertBefore", function (child, reference) {
    unwrapChild(child);
    if (child.__frag) {
      var kids = child.__frag.slice();
      child.__frag.length = 0;
      for (var i = 0; i < kids.length; i++) this.insertBefore(kids[i], reference);
      return child;
    }
    var refId = reference === null || reference === undefined ? 0 : reference.__n;
    host.__ironsurfDomInsertBefore(this.__d, this.__n, child.__n, refId);
    return child;
  });
  protoMethod("removeChild", function (child) {
    unwrapChild(child);
    host.__ironsurfDomRemoveChild(this.__d, this.__n, child.__n);
    return child;
  });
  protoMethod("replaceChild", function (newChild, oldChild) {
    unwrapChild(newChild);
    unwrapChild(oldChild);
    host.__ironsurfDomReplaceChild(this.__d, this.__n, newChild.__n, oldChild.__n);
    return oldChild;
  });
  // ChildNode.remove(): detach from the current parent.
  protoMethod("remove", function () {
    host.__ironsurfDomDetach(this.__d, this.__n);
  });
  protoMethod("cloneNode", function (deep) {
    return wrapNode(this.__d, host.__ironsurfDomClone(this.__d, this.__n, !!deep));
  });

  // Element IDL attribute mirrors (get/set via the id/class attributes).
  protoAccessor(
    "id",
    function () { return host.__ironsurfDomAttr(this.__d, this.__n, "id"); },
    function (v) { host.__ironsurfDomAttrSet(this.__d, this.__n, "id", String(v)); }
  );
  protoAccessor(
    "className",
    function () { return host.__ironsurfDomAttr(this.__d, this.__n, "class"); },
    function (v) { host.__ironsurfDomAttrSet(this.__d, this.__n, "class", String(v)); }
  );

  protoMethod("getAttribute", function (name) {
    return host.__ironsurfDomAttr(this.__d, this.__n, String(name));
  });
  protoMethod("setAttribute", function (name, value) {
    host.__ironsurfDomAttrSet(this.__d, this.__n, String(name), String(value));
  });
  protoMethod("removeAttribute", function (name) {
    host.__ironsurfDomAttrRemove(this.__d, this.__n, String(name));
  });
  protoMethod("hasAttribute", function (name) {
    return host.__ironsurfDomAttr(this.__d, this.__n, String(name)) !== null;
  });
  // Element-level queries: document-wide match (scoping is a known gap).
  protoMethod("querySelector", function (selector) {
    return wrapNode(this.__d, host.__ironsurfDomQuery(this.__d, String(selector)));
  });
  protoMethod("querySelectorAll", function (selector) {
    host.__ironsurfDomQuery(this.__d, String(selector));
    return makeList(this.__d, 3, 0, String(selector));
  });
  protoMethod("addEventListener", function (type, fn, options) {
    addListener(this, String(type), fn, options);
  });
  protoMethod("removeEventListener", function (type, fn, options) {
    removeListener(this, String(type), fn, options);
  });
  protoMethod("dispatchEvent", function (ev) {
    if (!ev || typeof ev.type !== "string") return true;
    var event = ev;
    if (!event.__ironsurfEvent) {
      event = makeEvent(String(ev.type), this, ev);
      Object.keys(ev).forEach(function (k) {
        if (event[k] === undefined && k !== "type") event[k] = ev[k];
      });
    }
    event.target = this;
    firePath(eventPath(this), event);
    return !event.defaultPrevented;
  });
  // getElementsByClassName on any element (live, like the document one).
  protoMethod("getElementsByClassName", function (name) {
    // Not scoped to descendants in this bridge — document-wide live list.
    return makeList(this.__d, 2, 0, String(name));
  });
  protoMethod("getElementsByTagName", function (name) {
    var tag = String(name).toLowerCase();
    return makeList(this.__d, tag === "*" ? 4 : 1, 0, tag);
  });

  // DocumentFragment: no blitz node (blitz has no fragment kind), so the
  // fragment is a JS list of real nodes (detached on insert).
  function makeFragment() {
    var frag = Object.create(NodeProto);
    Object.defineProperty(frag, "__d", { value: 0 });
    Object.defineProperty(frag, "__n", { value: 0 });
    Object.defineProperty(frag, "__frag", { value: [] });
    return frag;
  }
  // Fragment overrides (duck-typed by the __frag marker).
  var fragOverrides = {
    nodeType: 11,
    nodeName: "#document-fragment",
    childNodes: function () {
      var kids = this.__frag;
      var view = { length: kids.length, item: function (i) { return kids[i] === undefined ? null : kids[i]; } };
      for (var i = 0; i < kids.length; i++) view[String(i)] = kids[i];
      return view;
    },
    hasChildNodes: function () { return this.__frag.length > 0; },
    appendChild: function (child) {
      unwrapChild(child);
      if (child.__frag) {
        var kids = child.__frag.slice();
        child.__frag.length = 0;
        for (var i = 0; i < kids.length; i++) this.appendChild(kids[i]);
        return this;
      }
      host.__ironsurfDomDetach(this.__d, child.__n);
      this.__frag.push(child);
      return child;
    },
    insertBefore: function (child, reference) {
      unwrapChild(child);
      var at = reference === null || reference === undefined ? this.__frag.length : this.__frag.indexOf(reference);
      if (at < 0) throw new TypeError("NotFoundError: reference is not a child of this fragment");
      if (child.__frag) {
        var kids = child.__frag.slice();
        child.__frag.length = 0;
        var self = this;
        kids.forEach(function (k) { self.insertBefore(k, reference); });
        return this;
      }
      host.__ironsurfDomDetach(this.__d, child.__n);
      this.__frag.splice(at, 0, child);
      return child;
    },
    removeChild: function (child) {
      var at = this.__frag.indexOf(child);
      if (at < 0) throw new TypeError("NotFoundError: child is not in this fragment");
      this.__frag.splice(at, 1);
      return child;
    },
    textContent: function () {
      var out = "";
      for (var i = 0; i < this.__frag.length; i++) out += this.__frag[i].textContent;
      return out;
    },
  };
  // Fragments get their own property bindings (see fragPatch below).
  function fragPatch(frag) {
    Object.defineProperty(frag, "nodeType", { value: 11 });
    Object.defineProperty(frag, "nodeName", { value: "#document-fragment" });
    Object.defineProperty(frag, "childNodes", { get: fragOverrides.childNodes });
    Object.defineProperty(frag, "hasChildNodes", { value: fragOverrides.hasChildNodes });
    Object.defineProperty(frag, "appendChild", { value: fragOverrides.appendChild, writable: true });
    Object.defineProperty(frag, "insertBefore", { value: fragOverrides.insertBefore, writable: true });
    Object.defineProperty(frag, "removeChild", { value: fragOverrides.removeChild, writable: true });
    Object.defineProperty(frag, "textContent", {
      get: fragOverrides.textContent,
      set: function (v) {
        this.__frag.length = 0;
        if (String(v).length > 0) this.__frag.push(wrapNode(0, host.__ironsurfDomCreateText(0, String(v))));
      },
    });
    Object.defineProperty(frag, "innerHTML", {
      get: function () {
        var out = "";
        for (var i = 0; i < this.__frag.length; i++) out += this.__frag[i].outerHTML;
        return out;
      },
      set: function () { throw new Error("innerHTML is not supported on DocumentFragment"); },
    });
    return frag;
  }

  // The document object. When the tab registers a live blitz document the
  // accessors below read through it; with no live document (the plain host
  // snapshot world the bindings tests use) they fall back to host state.
  function bodyFallback() {
    return host.__ironsurfHasBody
      ? { nodeType: 1, textContent: host.__ironsurfBodyText }
      : null;
  }

  var document = {
    nodeType: 9,
    __d: 0,
    readyState: "complete",
    visibilityState: "visible",
    hidden: false,
    get documentElement() {
      if (!host.__ironsurfDomHasDoc()) return null;
      return wrapNode(0, host.__ironsurfDomHtml(0));
    },
    get head() {
      if (!host.__ironsurfDomHasDoc()) return null;
      return wrapNode(0, host.__ironsurfDomHead(0));
    },
    get body() {
      if (!host.__ironsurfDomHasDoc()) return bodyFallback();
      return wrapNode(0, host.__ironsurfDomBody(0));
    },
    doctype: null,
    getElementsByTagName: function (name) {
      var tag = String(name).toLowerCase();
      return makeList(0, tag === "*" ? 4 : 1, 0, tag);
    },
    getElementsByClassName: function (name) {
      return makeList(0, 2, 0, String(name));
    },
    getElementById: function (id) {
      return wrapNode(0, host.__ironsurfDomGetById(0, String(id)));
    },
    querySelector: function (selector) {
      // Invalid selectors raise SyntaxError from the native.
      return wrapNode(0, host.__ironsurfDomQuery(0, String(selector)));
    },
    querySelectorAll: function (selector) {
      // Browsers validate at call time: run the single-shot query once so an
      // unparsable selector raises SyntaxError here, then hand back the live
      // list (which stays in step with the document from then on).
      host.__ironsurfDomQuery(0, String(selector));
      return makeList(0, 3, 0, String(selector));
    },
    createElement: function (tag) {
      // Invalid names raise InvalidCharacterError from the native.
      return wrapNode(0, host.__ironsurfDomCreateElement(0, String(tag)));
    },
    createTextNode: function (text) {
      return wrapNode(0, host.__ironsurfDomCreateText(0, String(text)));
    },
    createComment: function (data) {
      return wrapNode(0, host.__ironsurfDomCreateComment(0, String(data)));
    },
    createDocumentFragment: function () {
      return fragPatch(makeFragment());
    },
    createEvent: function () { return {}; },
    addEventListener: function (type, fn, options) {
      addListener(document, String(type), fn, options);
    },
    removeEventListener: function (type, fn, options) {
      removeListener(document, String(type), fn, options);
    },
    dispatchEvent: function (ev) {
      if (!ev || typeof ev.type !== "string") return true;
      var event = ev;
      if (!event.__ironsurfEvent) {
        event = makeEvent(String(ev.type), document, ev);
        Object.keys(ev).forEach(function (k) {
          if (event[k] === undefined && k !== "type") event[k] = ev[k];
        });
      }
      event.target = document;
      firePath(eventPath(document), event);
      return !event.defaultPrevented;
    },
    write: function () {},
    writeln: function () {},
    get title() {
      return host.__ironsurfTitleGet();
    },
    set title(value) {
      host.__ironsurfTitleSet(String(value));
    },
    get URL() {
      return host.__ironsurfUrl;
    },
    get documentURI() {
      return host.__ironsurfUrl;
    },
  };

  // ---------------------------------------------------------------------
  // HTMLMediaElement glue for <video> (video task)
  //
  // The engine side lives in runtime.rs: the tab attaches a player to every
  // <video>, pumps it, and refreshes a snapshot per element. These are the
  // page-visible members: reads go through __ironsurfMediaGet (the last
  // snapshot the tab pumped), writes through __ironsurfMediaCmd (a command
  // the poll loop applies between pumps) — the location.href= pattern. They
  // sit on the wrapper prototype like every other member, so
  // document.querySelector("video").currentTime just works; on a non-media
  // element the host is never consulted and the members read as if there
  // were no media (paused true, NaN duration), like a real element with no
  // source.
  // ---------------------------------------------------------------------
  function mediaTag(self) {
    var name = self.nodeName;
    name = typeof name === "string" ? name.toLowerCase() : "";
    return name === "video" || name === "audio" ? name : "";
  }
  function mediaGet(self, prop) {
    if (!mediaTag(self)) return undefined;
    return host.__ironsurfMediaGet(self.__n, prop);
  }
  function mediaCmd(self, op, arg) {
    if (!mediaTag(self)) return undefined;
    host.__ironsurfMediaCmd(self.__n, op, arg === undefined ? 0 : arg);
  }
  function mediaNum(value, fallback) {
    return typeof value === "number" && value === value ? value : fallback;
  }
  function mediaAccessor(name, get, set) {
    Object.defineProperty(NodeProto, name, {
      get: get, set: set, enumerable: false, configurable: true
    });
  }
  // The interface the media members belong to: the wrapper prototype carries
  // them (this bridge has no deeper prototype chain), and it is exported
  // (globalThis.HTMLMediaElement, visible as host.HTMLMediaElement too) as a
  // callable sentinel whose prototype is the wrapper prototype, so the
  // real-DOM check `v instanceof HTMLMediaElement` holds for element wrappers.
  function HTMLMediaElement() {}
  HTMLMediaElement.prototype = NodeProto;
  mediaAccessor("currentTime",
    function () { return mediaNum(mediaGet(this, "currentTime"), NaN); },
    function (value) {
      var seconds = Number(value);
      if (isFinite(seconds) && seconds >= 0) mediaCmd(this, "seek", seconds);
    });
  mediaAccessor("duration",
    function () { return mediaNum(mediaGet(this, "duration"), NaN); });
  // An element with no attached player reads as paused; with one, the
  // tab's snapshot answers.
  mediaAccessor("paused",
    function () { return mediaGet(this, "paused") !== false; });
  mediaAccessor("ended",
    function () { return mediaGet(this, "ended") === true; });
  mediaAccessor("playbackRate",
    function () { return mediaNum(mediaGet(this, "playbackRate"), 1); },
    function (value) {
      var rate = Number(value);
      if (isFinite(rate) && rate > 0) mediaCmd(this, "rate", rate);
    });
  mediaAccessor("muted",
    function () { return mediaGet(this, "muted") === true; },
    function (value) { mediaCmd(this, "muted", value ? 1 : 0); });
  mediaAccessor("videoWidth",
    function () { return mediaNum(mediaGet(this, "videoWidth"), 0); });
  mediaAccessor("videoHeight",
    function () { return mediaNum(mediaGet(this, "videoHeight"), 0); });
  // IronSurf addition (the plan's `mediaError`): the user-facing failure
  // reason the engine produced (codec / too-large / unreachable …) or null.
  mediaAccessor("mediaError",
    function () {
      var value = mediaGet(this, "mediaError");
      return value == null ? null : String(value);
    });
  function mediaMethod(name, fn) {
    Object.defineProperty(NodeProto, name, {
      value: fn, writable: true, enumerable: false, configurable: true
    });
  }
  mediaMethod("play", function () { mediaCmd(this, "play"); });
  mediaMethod("pause", function () { mediaCmd(this, "pause"); });
  // load() needs no plumbing: a src change on the element attribute is
  // noticed by the tab's per-tick attach pass.
  mediaMethod("load", function () {});
  mediaMethod("canPlayType", function (type) {
    type = String(type).toLowerCase();
    return /mp4|webm|avi|matroska|h264|avc1|vp8|vp9/.test(type) ? "probably" : "";
  });
  // Export the interface name (checks like `v instanceof HTMLMediaElement`).
  globalThis.HTMLMediaElement = HTMLMediaElement;

  function makeConsole(level) {
    return function () {
      // Array.prototype.join applies JavaScript's ToString to every argument.
      host.__ironsurfConsole(level, Array.prototype.join.call(arguments, " "));
    };
  }

  var console = {
    log: makeConsole("log"),
    info: makeConsole("info"),
    warn: makeConsole("warn"),
    error: makeConsole("error"),
    debug: makeConsole("log"),
    trace: makeConsole("log"),
    dir: makeConsole("log"),
    table: makeConsole("log"),
    assert: function () {},
  };

  // `document`/`console` as globals (and thus as `window.document`/etc, since
  // window IS the global object).
  host.document = document;
  host.console = console;
  document.defaultView = host;
})();
"##;

/// Build a fresh boa context for one navigation, with the tiny binding surface
/// bound to `host`. The context is intentionally thrown away at the next
/// navigation (per `ARCHITECTURE.md` §JavaScript (Boa)).
pub fn build_context(host: &HostRef, _timers: &Rc<RefCell<TimerQueue>>) -> Context {
    // `_timers` documents the pairing (natives reach the queue through the
    // active-scope stack a [`JsRuntime`] pushes around every evaluation).
    let (url, body_text, has_body) = {
        let host = host.borrow();
        (
            host.url.clone(),
            host.body_text.clone().unwrap_or_default(),
            host.body_text.is_some(),
        )
    };

    let mut context = Context::default();

    for (name, value) in [
        (native::URL, JsValue::from(JsString::from(url))),
        (native::BODY_TEXT, JsValue::from(JsString::from(body_text))),
        (native::HAS_BODY, JsValue::from(has_body)),
    ] {
        if let Err(err) = context.register_global_property(
            JsString::from(name),
            value,
            Attribute::all(),
        ) {
            error!("js binding `{name}` failed to install: {err}");
        }
    }

    // document.title (get reflects prior sets, set asks for a TitleChanged + repaint).
    if let Err(err) = context.register_global_builtin_callable(
        JsString::from(native::TITLE_GET),
        0,
        NativeFunction::from_copy_closure(|_this, _args, _context| {
            let title = with_host(|host| host.title.clone())
                .ok_or_else(|| JsNativeError::typ().with_message("no js host"))?;
            Ok(JsValue::from(JsString::from(title)))
        }),
    ) {
        error!("js binding `title` getter failed to install: {err}");
    }
    if let Err(err) = context.register_global_builtin_callable(
        JsString::from(native::TITLE_SET),
        1,
        NativeFunction::from_copy_closure(|_this, args, _context| {
            // Coerce first (a page's own value-of could re-enter JS), then borrow.
            let value = args
                .first()
                .cloned()
                .unwrap_or(JsValue::undefined())
                .to_string(_context)?
                .to_std_string_escaped();
            with_host(|host| {
                host.title = value.clone();
                host.title_assigned = Some(value);
                host.repaint = true;
            });
            Ok(JsValue::undefined())
        }),
    ) {
        error!("js binding `title` setter failed to install: {err}");
    }
    // console.log/info/warn/error → tab stderr + the per-navigation capture.
    if let Err(err) = context.register_global_builtin_callable(
        JsString::from(native::CONSOLE),
        2,
        NativeFunction::from_copy_closure(|_this, args, _context| {
            let level = args
                .first()
                .and_then(|value| value.as_string())
                .map(|s| s.to_std_string_escaped())
                .unwrap_or_default();
            let text = args
                .get(1)
                .cloned()
                .unwrap_or(JsValue::undefined())
                .to_string(_context)?
                .to_std_string_escaped();
            let level = ConsoleLevel::parse(&level);
            let url = with_host(|host| {
                host.console.push((level, text.clone()));
                host.url.clone()
            })
            .unwrap_or_default();
            log_console(level, &url, &text);
            Ok(JsValue::undefined())
        }),
    ) {
        error!("js binding `console` failed to install: {err}");
    }
    // location writes: queued as the host's pending navigation — the tab's
    // poll loop follows it after the current JS turn (replacing the current
    // history entry when `replace` is set).
    if let Err(err) = context.register_global_builtin_callable(
        JsString::from(native::NAVIGATE),
        2,
        NativeFunction::from_copy_closure(|_this, args, _context| {
            let target = args
                .first()
                .cloned()
                .unwrap_or(JsValue::undefined())
                .to_string(_context)?
                .to_std_string_escaped();
            let replace = args.get(1).is_some_and(|v| v.to_boolean());
            if target.is_empty() {
                return Ok(JsValue::undefined());
            }
            info!("js navigation requested → {target} (replace={replace})");
            with_host(|host| {
                host.pending_nav = Some(PendingNav { target, replace });
            });
            Ok(JsValue::undefined())
        }),
    ) {
        error!("js binding `location` failed to install: {err}");
    }
    // Timers: real queue entries the tab's poll loop executes. A non-callable
    // callback schedules nothing and reads as id 0 (what browsers do).
    let schedule = |interval: bool, raf: bool, delay_arg: Option<usize>| {
        move |_this: &JsValue, args: &[JsValue], context: &mut Context| {
            let callback = args.first().cloned().unwrap_or(JsValue::undefined());
            if !callback.is_callable() {
                return Ok(JsValue::new(0u32));
            }
            let delay = match delay_arg {
                Some(index) => delay_from(args, index, context)?,
                None => Duration::ZERO,
            };
            let delay = if raf { RAF_INTERVAL } else { delay };
            let id = with_timers(|queue| {
                queue.schedule(callback, delay, interval, raf, Instant::now())
            })
            .unwrap_or(0);
            with_host(|host| host.timer_calls += 1);
            Ok(JsValue::new(id))
        }
    };
    for (name, interval, raf, delay_arg) in [
        (native::SET_TIMEOUT, false, false, Some(1usize)),
        (native::SET_INTERVAL, true, false, Some(1usize)),
        (native::RAF, false, true, None),
    ] {
        let native_fn = NativeFunction::from_copy_closure(schedule(interval, raf, delay_arg));
        if let Err(err) =
            context.register_global_builtin_callable(JsString::from(name), 2, native_fn)
        {
            error!("js binding `{name}` failed to install: {err}");
        }
    }
    // Video (task: playback) reads: `__ironsurfMediaGet(nodeId, prop)` reads
    // the snapshot the tab's last media pump wrote (`null` = the tab has no
    // player for that node — the prelude's `HTMLMediaElement` glue maps the
    // props to their no-media readings from there).
    if let Err(err) = context.register_global_builtin_callable(
        JsString::from(native::MEDIA_GET),
        2,
        NativeFunction::from_copy_closure(|_this, args, context| {
            let node = args
                .first()
                .map(|value| value.to_number(context).map(|n| n as u64))
                .transpose()?
                .unwrap_or(0);
            let prop = args
                .get(1)
                .cloned()
                .unwrap_or(JsValue::undefined())
                .to_string(context)?
                .to_std_string_escaped();
            let value = with_host(|host| {
                host.media.snapshot(node).map(|snap| {
                    use crate::runtime::MediaSnapshot;
                    let MediaSnapshot {
                        duration,
                        current_time,
                        paused,
                        ended,
                        rate,
                        muted,
                        width,
                        height,
                        ref error,
                    } = *snap;
                    match prop.as_str() {
                        "duration" => JsValue::from(duration),
                        "currentTime" => JsValue::from(current_time),
                        "paused" => JsValue::from(paused),
                        "ended" => JsValue::from(ended),
                        "playbackRate" => JsValue::from(rate),
                        "muted" => JsValue::from(muted),
                        "videoWidth" => JsValue::from(f64::from(width)),
                        "videoHeight" => JsValue::from(f64::from(height)),
                        "mediaError" => match error {
                            Some(text) => JsValue::from(JsString::from(text.as_str())),
                            None => JsValue::null(),
                        },
                        _ => JsValue::undefined(),
                    }
                })
            });
            Ok(value.flatten().unwrap_or(JsValue::null()))
        }),
    ) {
        error!("js binding `mediaGet` failed to install: {err}");
    }
    // Video writes: `__ironsurfMediaCmd(nodeId, op, arg)` queues one
    // `MediaCommand`; the poll loop applies it to the player between pumps
    // (the `location.href=` pattern — JS never touches the player directly).
    if let Err(err) = context.register_global_builtin_callable(
        JsString::from(native::MEDIA_CMD),
        3,
        NativeFunction::from_copy_closure(|_this, args, context| {
            let node = args
                .first()
                .map(|value| value.to_number(context).map(|n| n as u64))
                .transpose()?
                .unwrap_or(0);
            let op = args
                .get(1)
                .cloned()
                .unwrap_or(JsValue::undefined())
                .to_string(context)?
                .to_std_string_escaped();
            let arg = args
                .get(2)
                .map(|value| value.to_number(context))
                .transpose()?
                .unwrap_or(0.0);
            if let Some(command) = crate::runtime::media_command_from_js(node, &op, arg) {
                with_host(|host| host.media.enqueue(command));
            }
            Ok(JsValue::undefined())
        }),
    ) {
        error!("js binding `mediaCmd` failed to install: {err}");
    }
    // clearTimeout/clearInterval/cancelAnimationFrame: cancel by id.
    if let Err(err) = context.register_global_builtin_callable(
        JsString::from(native::TIMER_CLEAR),
        1,
        NativeFunction::from_copy_closure(|_this, args, context| {
            let id = args
                .first()
                .map(|value| value.to_u32(context))
                .transpose()?
                .unwrap_or(0);
            with_timers(|queue| queue.cancel(id));
            Ok(JsValue::undefined())
        }),
    ) {
        error!("js binding `clearTimer` failed to install: {err}");
    }

    // DOM bridge natives (`__ironsurfDom*`): all node behaviour lives here;
    // the prelude's NodeProto/document glue only delegates (design B).
    register_dom_natives(&mut context);

    if let Err(err) = context.eval(Source::from_bytes(PRELUDE)) {
        // Our own glue failing is a bug, not page input: report loudly, keep
        // the globals that did install, and let the tab run.
        error!("js prelude failed to evaluate: {err}");
    }

    context
}

/// A script that threw (parse errors included). Reported to the shell as
/// `FromTab::StatusText`; never fatal to the tab.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScriptError {
    pub origin: String,
    pub message: String,
}

/// The JavaScript side of one document: **one `boa_engine::Context` for the
/// lifetime of the document**, its host cell, and the live [`TimerQueue`].
/// The tab owns this (rebuilt per navigation); because it outlives the
/// initial script run, callbacks scheduled by page scripts fire later from
/// the poll loop ([`JsRuntime::run_due_timers`]).
pub struct JsRuntime {
    context: Context,
    host: HostRef,
    timers: Rc<RefCell<TimerQueue>>,
    /// Monotonic origin for rAF timestamps.
    started: Instant,
}

impl JsRuntime {
    /// Fresh realm for one navigation, seeded from `host`.
    pub fn new(host: HostRef) -> Self {
        let timers = Rc::new(RefCell::new(TimerQueue::default()));
        let context = build_context(&host, &timers);
        JsRuntime {
            context,
            host,
            timers,
            started: Instant::now(),
        }
    }

    /// The host cell the bindings write through (title, console, notes).
    pub fn host(&self) -> &HostRef {
        &self.host
    }

    /// Take the navigation this document's scripts queued since the last turn
    /// (`location.href=`/`assign`/`replace`/`reload`). The tab calls this
    /// after every JS turn and follows it through its own navigation entry
    /// point — never re-entering the stack that queued it.
    pub fn take_pending_navigation(&self) -> Option<PendingNav> {
        self.host.borrow_mut().pending_nav.take()
    }

    /// Evaluate a `javascript:` URL's program in this document's world (the
    /// same realm page scripts live in, with `doc` as the live document).
    /// Ok(`Some(text)`) when the completion value is a string to render as a
    /// new document (spec), Ok(`None`) when it is `undefined`/empty (nothing
    /// loads), Err on a thrown exception.
    pub(crate) fn eval_javascript_url(
        &mut self,
        source: &str,
        doc: Option<&mut HtmlDocument>,
    ) -> Result<Option<String>, ScriptError> {
        let _active = self.activate();
        let _live_doc = doc.map(register_live_doc);
        match self.context.eval(Source::from_bytes(source)) {
            Ok(value) => {
                if value.is_undefined() || value.is_null() {
                    return Ok(None);
                }
                // Numbers/objects stringify through JS semantics; a throwing
                // `toString` reads as "no document" rather than an error.
                let text = value
                    .to_string(&mut self.context)
                    .map(|s| s.to_std_string_escaped())
                    .unwrap_or_default();
                if text.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(text))
                }
            }
            Err(err) => {
                let message = first_line(&err);
                error!("javascript error in javascript: URL: {message}");
                Err(ScriptError {
                    origin: "javascript:".to_string(),
                    message,
                })
            }
        }
    }

    /// Whether the poll loop has anything left to wait for.
    pub fn has_timers(&self) -> bool {
        self.timers.borrow().has_entries()
    }

    /// Earliest timer deadline (`None` = nothing scheduled).
    pub fn next_timer_deadline(&self) -> Option<Instant> {
        self.timers.borrow().earliest()
    }

    /// Push this runtime's scope (host + timers) onto the thread-local stack;
    /// the guard pops it on drop, so natives resolve this runtime's cells
    /// while any of its evaluations runs.
    fn activate(&self) -> ActiveGuard {
        let scope = ActiveScope {
            host: Rc::clone(&self.host),
            timers: Rc::clone(&self.timers),
        };
        ACTIVE_HOSTS.with(|stack| stack.borrow_mut().push(scope));
        ActiveGuard
    }

    /// Evaluate `scripts` in order. Every evaluation is wrapped: an exception
    /// is collected and the next script still runs (contract: script errors
    /// must never kill the tab).
    ///
    /// `doc` is the tab's live document: while the scripts run it is
    /// registered as `docId` 0 in [`LIVE_DOCS`], so the DOM natives can read
    /// and mutate it (design A). The registration — and with it the raw
    /// pointer — is gone the moment this call returns.
    pub fn run_scripts(
        &mut self,
        scripts: &[ScriptSource],
        doc: Option<&mut HtmlDocument>,
    ) -> Vec<ScriptError> {
        let started = Instant::now();
        let _active = self.activate();
        let _live_doc = doc.map(register_live_doc);
        let mut errors = Vec::new();
        for script in scripts {
            trace!(
                "eval [{}] ({} bytes, started {}ms ago)",
                script.origin,
                script.code.len(),
                started.elapsed().as_millis()
            );
            match self.context.eval(Source::from_bytes(script.code.as_str())) {
                Ok(_) => {}
                Err(err) => {
                    let message = first_line(&err);
                    error!("javascript error in {origin}: {message}", origin = script.origin);
                    errors.push(ScriptError {
                        origin: script.origin.clone(),
                        message,
                    });
                }
            }
        }
        errors
    }

    /// Evaluate one snippet with this runtime's scope active (the same world
    /// page scripts see; used by the binding tests).
    #[cfg(test)]
    pub(crate) fn eval(&mut self, source: &str) -> Result<JsValue, boa_engine::JsError> {
        let _active = self.activate();
        self.context.eval(Source::from_bytes(source))
    }

    /// Evaluate one snippet with `doc` registered as the live document
    /// (docId 0) — the DOM bridge tests run in exactly this world.
    #[cfg(test)]
    pub(crate) fn eval_with_doc(
        &mut self,
        source: &str,
        doc: &mut HtmlDocument,
    ) -> Result<JsValue, boa_engine::JsError> {
        let _active = self.activate();
        let _live_doc = register_live_doc(doc);
        self.context.eval(Source::from_bytes(source))
    }

    /// Replay a real user interaction through the page's own event system
    /// (`__ironsurfFire`, the glue's documented entry point): blitz has just
    /// run the default actions for a pointer/key event, and the tab hands the
    /// event to capture/bubble dispatch so `addEventListener`, inline
    /// `onclick=""` and property handlers all run. Returns whether the page
    /// called `preventDefault()` (task: `<form>` — which also gates the
    /// default action the tab owns, form submission).
    pub(crate) fn fire_event(
        &mut self,
        doc: &mut HtmlDocument,
        node: Option<NodeId>,
        kind: &str,
        init: &str,
    ) -> bool {
        let n = match node {
            Some(node) => node.as_u64().to_string(),
            None => "null".to_owned(),
        };
        let source = format!(
            "(function () {{ var e = __ironsurfFire({kind:?}, 0, {n}, {init}); return !!e.defaultPrevented; }})()"
        );
        // The same world page scripts live in, with `doc` as the live
        // document (docId 0) — the glue's wrappers resolve against it.
        let _active = self.activate();
        let _live_doc = register_live_doc(doc);
        match self.context.eval(Source::from_bytes(&source)) {
            Ok(value) => value.as_boolean().unwrap_or(false),
            Err(err) => {
                warn!("js {kind} dispatch failed: {err}");
                false
            }
        }
    }

    /// Execute every timer callback due at `now`, in due order. Exceptions
    /// are caught **per callback** (reported like any script error: origin
    /// `"timer"`) and never stop the remaining callbacks or later firings.
    /// Side effects stay in the host cell — the tab turns them into IPC
    /// exactly like [`JsRuntime::run_scripts`] does.
    pub fn run_due_timers(
        &mut self,
        now: Instant,
        doc: Option<&mut HtmlDocument>,
    ) -> Vec<ScriptError> {
        let due = self.timers.borrow_mut().take_due(now);
        if due.is_empty() {
            return Vec::new();
        }
        let _active = self.activate();
        let _live_doc = doc.map(register_live_doc);
        let mut errors = Vec::new();
        for entry in due {
            self.host.borrow_mut().timers_fired += 1;
            // rAF callbacks receive a DOMHighResTimeStamp-ish argument.
            let args = if entry.raf {
                vec![JsValue::new(self.started.elapsed().as_secs_f64() * 1000.0)]
            } else {
                Vec::new()
            };
            // Non-callables cannot be scheduled by the natives, so `None`
            // here is unreachable; treat it as a skipped callback anyway.
            if let Some(callback) = entry.callback.as_object() {
                if let Err(err) = callback.call(&JsValue::undefined(), &args, &mut self.context) {
                    let message = first_line(&err);
                    error!("javascript error in timer callback: {message}");
                    errors.push(ScriptError {
                        origin: "timer".to_string(),
                        message,
                    });
                }
            }
        }
        errors
    }
}

/// Pops the thread-local scope stack when dropped.
struct ActiveGuard;

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        ACTIVE_HOSTS.with(|stack| {
            stack.borrow_mut().pop();
        });
    }
}

/// First line of a boa error (its Display adds a shadow-stack backtrace).
fn first_line(err: &boa_engine::JsError) -> String {
    err.to_string().lines().next().unwrap_or("script error").to_string()
}

/// Text to surface as `FromTab::StatusText` for these script errors
/// (`None` when there were none). Newest error wins — it is the one the user
/// is most likely to notice.
pub fn status_text_for_errors(errors: &[ScriptError]) -> Option<String> {
    errors.last().map(|err| format!("JavaScript error: {}: {}", err.origin, err.message))
}

/// Body `textContent` snapshot of `doc` for the bindings: the `<body>`
/// element's text, or `None` when the document has no body element.
pub fn body_text_snapshot(doc: &BaseDocument) -> Option<String> {
    doc.find_body_node().map(|node| node.text_content())
}

// ---------------------------------------------------------------------------
// Unit tests (headless; no blitz document needed for the binding surface)

#[cfg(test)]
mod tests {
    use super::*;

    /// Evaluate a snippet in a fresh per-document runtime (what the binding
    /// tests exercise: prelude + natives, no page scripts).
    fn eval_snippet(source: &str, host: &HostRef) -> JsValue {
        let mut runtime = JsRuntime::new(Rc::clone(host));
        runtime.eval(source).expect("snippet should evaluate")
    }

    fn sample_host() -> HostRef {
        Rc::new(RefCell::new(HostState::new(
            "https://example.test/page.html",
            Some("Parsed Title"),
            Some(" body text ".to_string()),
        )))
    }

    #[test]
    fn title_get_reflects_the_parsed_title() {
        let host = sample_host();
        assert_eq!(
            eval_snippet("document.title", &host)
                .as_string()
                .map(|s| s.to_std_string_escaped()),
            Some("Parsed Title".to_string())
        );
        // `window.document` is the same object as the `document` global.
        assert_eq!(
            eval_snippet("window.document.title", &host)
                .as_string()
                .map(|s| s.to_std_string_escaped()),
            Some("Parsed Title".to_string())
        );
        assert!(host.borrow().title_assigned.is_none());
    }

    #[test]
    fn title_set_is_recorded_and_asks_for_a_repaint() {
        let host = sample_host();
        eval_snippet("document.title = 'js-set-title'", &host);
        let host = host.borrow();
        assert_eq!(host.title_assigned.as_deref(), Some("js-set-title"));
        assert_eq!(host.title, "js-set-title");
        assert!(host.repaint, "a title write must request a repaint");
    }

    #[test]
    fn title_set_coerces_to_string() {
        let host = sample_host();
        eval_snippet("document.title = 42 + 1", &host);
        assert_eq!(host.borrow().title_assigned.as_deref(), Some("43"));
    }

    #[test]
    fn window_is_self_referential_and_listeners_are_noops() {
        let host = sample_host();
        assert_eq!(
            eval_snippet("window.window === window", &host).as_boolean(),
            Some(true),
            "window.window must point back at window"
        );
        assert_eq!(
            eval_snippet("window.document === document", &host).as_boolean(),
            Some(true),
            "window.document must be the document global"
        );
        // No-ops: called, no exception, nothing recorded.
        assert!(
            eval_snippet("window.addEventListener('load', function () {}); 'ok'", &host)
                .as_string()
                .map(|s| s.to_std_string_escaped())
                == Some("ok".to_string())
        );
        assert!(host.borrow().console.is_empty());
    }

    #[test]
    fn location_href_reflects_the_document_url() {
        let host = sample_host();
        assert_eq!(
            eval_snippet("window.location.href", &host)
                .as_string()
                .map(|s| s.to_std_string_escaped()),
            Some("https://example.test/page.html".to_string())
        );
        // Writes queue a real navigation for the tab; the reflection keeps
        // reporting the current document URL until it actually loads.
        assert_eq!(
            eval_snippet(
                "window.location.assign('https://evil.test/x'); 'ok'",
                &host
            )
            .as_string()
            .map(|s| s.to_std_string_escaped()),
            Some("ok".to_string())
        );
        assert_eq!(
            host.borrow().pending_nav,
            Some(PendingNav {
                target: "https://evil.test/x".to_string(),
                replace: false,
            })
        );
        eval_snippet("window.location.replace('/next?a=1#b')", &host);
        assert_eq!(
            host.borrow().pending_nav,
            Some(PendingNav {
                target: "/next?a=1#b".to_string(),
                replace: true,
            })
        );
        eval_snippet("window.location.href = 'https://evil.test/'", &host);
        let borrowed = host.borrow();
        assert_eq!(borrowed.url, "https://example.test/page.html");
        assert_eq!(
            borrowed.pending_nav,
            Some(PendingNav {
                target: "https://evil.test/".to_string(),
                replace: false,
            })
        );
        // `reload()` re-reads the current URL with replace semantics.
        drop(borrowed);
        eval_snippet("window.location.reload()", &host);
        assert_eq!(
            host.borrow().pending_nav,
            Some(PendingNav {
                target: "https://example.test/page.html".to_string(),
                replace: true,
            })
        );
    }

    #[test]
    fn location_components_reflect_the_document_url() {
        let host = Rc::new(RefCell::new(HostState::new(
            "https://ex.test:8443/a/b?q=1#frag",
            None,
            None,
        )));
        assert_eq!(
            eval_snippet("location.protocol", &host)
                .as_string()
                .map(|s| s.to_std_string_escaped()),
            Some("https:".to_string())
        );
        assert_eq!(
            eval_snippet("location.host", &host)
                .as_string()
                .map(|s| s.to_std_string_escaped()),
            Some("ex.test:8443".to_string())
        );
        assert_eq!(
            eval_snippet("location.hostname", &host)
                .as_string()
                .map(|s| s.to_std_string_escaped()),
            Some("ex.test".to_string())
        );
        assert_eq!(
            eval_snippet("location.pathname", &host)
                .as_string()
                .map(|s| s.to_std_string_escaped()),
            Some("/a/b".to_string())
        );
        assert_eq!(
            eval_snippet("location.search", &host)
                .as_string()
                .map(|s| s.to_std_string_escaped()),
            Some("?q=1".to_string())
        );
        assert_eq!(
            eval_snippet("location.hash", &host)
                .as_string()
                .map(|s| s.to_std_string_escaped()),
            Some("#frag".to_string())
        );
    }

    #[test]
    fn body_text_content_is_a_readable_snapshot() {
        let host = sample_host();
        assert_eq!(
            eval_snippet("document.body.textContent", &host)
                .as_string()
                .map(|s| s.to_std_string_escaped()),
            Some(" body text ".to_string())
        );
    }

    #[test]
    fn document_body_is_null_without_a_body_element() {
        let host = Rc::new(RefCell::new(HostState::new("about:blank", None, None)));
        assert_eq!(
            eval_snippet("document.body === null", &host).as_boolean(),
            Some(true),
            "no <body> must read as null"
        );
        assert_eq!(
            eval_snippet("document.title", &host)
                .as_string()
                .map(|s| s.to_std_string_escaped()),
            Some(String::new())
        );
    }

    #[test]
    fn console_output_is_captured_per_level() {
        let host = sample_host();
        eval_snippet(
            "console.log('a', 1, {x: 2}); console.warn('careful'); console.error('boom'); console.info('fyi');",
            &host,
        );
        let captured: Vec<(String, String)> = host
            .borrow()
            .console
            .iter()
            .map(|(level, text)| (level.as_str().to_string(), text.clone()))
            .collect();
        assert_eq!(
            captured,
            vec![
                ("log".to_string(), "a 1 [object Object]".to_string()),
                ("warn".to_string(), "careful".to_string()),
                ("error".to_string(), "boom".to_string()),
                ("info".to_string(), "fyi".to_string()),
            ]
        );
    }

    #[test]
    fn timer_calls_create_queue_entries_and_are_counted() {
        let host = sample_host();
        let mut runtime = JsRuntime::new(Rc::clone(&host));
        let timeout_id = runtime
            .eval("setTimeout(function () {}, 1)")
            .expect("setTimeout")
            .as_i32()
            .expect("numeric id");
        let interval_id = runtime
            .eval("setInterval(function () {}, 1)")
            .expect("setInterval")
            .as_i32()
            .expect("numeric id");
        assert!(timeout_id > 0 && interval_id > 0 && timeout_id != interval_id);
        assert_eq!(host.borrow().timer_calls, 2, "scheduling is counted for stats");
        assert!(
            host.borrow()
                .notes
                .iter()
                .all(|note| !note.contains("timers are not implemented")),
            "the old not-implemented note path is gone"
        );
        // Non-callable callbacks schedule nothing and read as id 0.
        assert_eq!(
            runtime.eval("setTimeout('not a function', 1)").expect("eval").as_i32(),
            Some(0)
        );
    }

    #[test]
    fn timeout_fires_from_the_poll_loop_and_never_arms_again() {
        let host = sample_host();
        let mut runtime = JsRuntime::new(Rc::clone(&host));
        runtime
            .eval("setTimeout(function () { document.title = 'timer-fired'; }, 10)")
            .expect("setTimeout");
        assert!(host.borrow().title_assigned.is_none(), "not run yet");

        let now = Instant::now();
        // The loop polls before the deadline: nothing fires early.
        assert!(runtime.run_due_timers(now, None).is_empty());
        assert_eq!(host.borrow().title_assigned, None);

        // 20 ms later the poll loop catches the due callback …
        let later = now + Duration::from_millis(20);
        let errors = runtime.run_due_timers(later, None);
        assert!(errors.is_empty(), "{errors:?}");
        let borrowed = host.borrow();
        assert_eq!(borrowed.title_assigned.as_deref(), Some("timer-fired"));
        assert_eq!(borrowed.timers_fired, 1);
        drop(borrowed);

        // … and a one-shot never re-arms.
        assert!(!runtime.has_timers());
        assert!(runtime.run_due_timers(later + Duration::from_secs(5), None).is_empty());
        assert_eq!(host.borrow().timers_fired, 1);
    }

    #[test]
    fn interval_rearms_until_clearinterval_cancels_it() {
        let host = sample_host();
        let mut runtime = JsRuntime::new(Rc::clone(&host));
        runtime
            .eval(
                "var ticks = 0; intervalId = setInterval(function () { ticks += 1; document.title = 'tick-' + ticks; }, 5);",
            )
            .expect("setInterval");

        let now = Instant::now();
        // First fire, and the interval re-arms itself for the next poll.
        assert!(runtime.run_due_timers(now + Duration::from_millis(10), None).is_empty());
        assert_eq!(host.borrow().title_assigned.as_deref(), Some("tick-1"));
        assert!(runtime.has_timers(), "intervals stay scheduled until cleared");

        // clearInterval cancels: no further callback, even much later.
        runtime.eval("clearInterval(intervalId)").expect("clearInterval");
        assert!(!runtime.has_timers());
        assert!(runtime.run_due_timers(now + Duration::from_secs(60), None).is_empty());
        assert_eq!(
            runtime.eval("ticks").expect("read ticks").as_i32(),
            Some(1),
            "exactly one tick ran"
        );
        assert_eq!(host.borrow().timers_fired, 1);
    }

    #[test]
    fn throwing_callback_is_reported_and_never_stops_the_rest() {
        let host = sample_host();
        let mut runtime = JsRuntime::new(Rc::clone(&host));
        runtime
            .eval(
                "setTimeout(function () { throw new Error('callback boom'); }, 0);
                 setTimeout(function () { document.title = 'after-boom'; }, 1);",
            )
            .expect("scheduling");

        let errors = runtime.run_due_timers(Instant::now() + Duration::from_millis(50), None);
        assert_eq!(errors.len(), 1, "the throwing callback is caught …");
        assert_eq!(errors[0].origin, "timer");
        assert!(errors[0].message.contains("callback boom"), "{errors:?}");
        // … and its batch sibling still ran (the tab is alive and scripting).
        assert_eq!(host.borrow().title_assigned.as_deref(), Some("after-boom"));
        assert_eq!(runtime.eval("1 + 1").expect("eval").as_i32(), Some(2));
    }

    #[test]
    fn request_animation_frame_is_a_sixteen_ms_one_shot() {
        let host = sample_host();
        let mut runtime = JsRuntime::new(Rc::clone(&host));
        runtime
            .eval("requestAnimationFrame(function (t) { document.title = 'raf-' + (typeof t); })")
            .expect("requestAnimationFrame");
        // Not due immediately: rAF waits a frame (16 ms).
        assert!(runtime.run_due_timers(Instant::now(), None).is_empty());
        assert!(host.borrow().title_assigned.is_none());
        let errors = runtime.run_due_timers(Instant::now() + Duration::from_millis(20), None);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(
            host.borrow().title_assigned.as_deref(),
            Some("raf-number"),
            "rAF callbacks receive a timestamp"
        );
        assert!(!runtime.has_timers(), "rAF is one-shot");
    }

    #[test]
    fn throwing_script_is_reported_and_the_next_script_still_runs() {
        let host = sample_host();
        let scripts = [
            ScriptSource {
                code: "throw new Error('nope');".to_string(),
                origin: "inline".to_string(),
            },
            ScriptSource {
                code: "document.title = 'still-running';".to_string(),
                origin: "second.js".to_string(),
            },
        ];
        let mut runtime = JsRuntime::new(Rc::clone(&host));
        let errors = runtime.run_scripts(&scripts, None);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].origin, "inline");
        assert!(errors[0].message.contains("nope"), "{:?}", errors[0]);
        // The second script executed despite the first throwing.
        assert_eq!(host.borrow().title_assigned.as_deref(), Some("still-running"));
        // Status text mentions both the origin and the message.
        // boa appends its own position hint; the status text carries origin + message.
        let status = status_text_for_errors(&errors).expect("a status text");
        assert!(
            status.starts_with("JavaScript error: inline: Error: nope"),
            "{status}"
        );
        assert!(status_text_for_errors(&[]).is_none());
    }

    #[test]
    fn syntax_error_is_a_reported_script_error() {
        let host = sample_host();
        let scripts = [ScriptSource {
            code: "function broken( {".to_string(),
            origin: "inline".to_string(),
        }];
        let mut runtime = JsRuntime::new(Rc::clone(&host));
        let errors = runtime.run_scripts(&scripts, None);
        assert_eq!(errors.len(), 1, "syntax errors never abort the tab");
        assert!(!errors[0].message.is_empty());
    }

    #[test]
    fn contexts_are_isolated_between_navigations() {
        let first = sample_host();
        let mut first_runtime = JsRuntime::new(Rc::clone(&first));
        first_runtime.run_scripts(
            &[ScriptSource {
                code: "var leaked = 1; document.title = 'one';".to_string(),
                origin: "inline".to_string(),
            }],
            None,
        );
        let second = Rc::new(RefCell::new(HostState::new("https://example.test/2", None, None)));
        // The second navigation gets a fresh realm: `leaked` is gone.
        assert_eq!(
            eval_snippet("typeof leaked", &second).as_string().map(|s| s.to_std_string_escaped()),
            Some("undefined".to_string()),
            "per-navigation context must not leak page globals"
        );
        assert!(second.borrow().title_assigned.is_none());
    }

    /// Parse `html` into a real blitz document (headless, no net provider).
    fn dom(html: &str) -> blitz_html::HtmlDocument {
        blitz_html::HtmlDocument::from_html(html, blitz_dom::DocumentConfig::default())
    }

    /// Collect the scripts of `html`, answering every `src=` with `JS-<name>`
    /// so tests can see the resolution order and the URLs handed to the fetcher.
    fn extract_probe(html: &str, base: &str) -> (Vec<(String, String)>, Vec<String>) {
        let doc = dom(html);
        let base = Url::parse(base).expect("base url");
        let mut asked = Vec::new();
        let extraction = js_extract(&doc, &base, &mut |url| {
            asked.push(url.as_str().to_owned());
            Ok(format!("JS {}", url.path().rsplit('/').next().unwrap_or("?")))
        });
        let scripts = extraction
            .scripts
            .iter()
            .map(|script| (script.origin.clone(), script.code.clone()))
            .collect();
        (scripts, extraction.notes)
    }

    const SCRIPT_PAGE: &str = r#"<!doctype html>
<html>
  <head>
    <title>Scripts</title>
    <script>document.title = "head";</script>
    <script src="a.js"></script>
  </head>
  <body>
    <script type="application/json">{"data": 1}</script>
    <script src="nested/b.js"></script>
    <script type="module">import { x } from "./c.js";</script>
    <script src="gone.js" onerror="boom()"></script>
  </body>
</html>"#;

    #[test]
    fn scripts_are_collected_in_document_order() {
        let (scripts, notes) = extract_probe(SCRIPT_PAGE, "file:///srv/site/index.html");
        assert_eq!(
            scripts
                .iter()
                .map(|(origin, _)| origin.as_str())
                .collect::<Vec<_>>(),
            vec![
                "inline",
                "file:///srv/site/a.js",
                "file:///srv/site/nested/b.js",
                "file:///srv/site/gone.js",
            ],
            "head first, then body, in tree order (only the JSON/module scripts are skipped)"
        );
        assert_eq!(scripts[0].1, "document.title = \"head\";");
        assert_eq!(scripts[1].1, "JS a.js", "src= bodies come from the fetcher");
        // The two non-executed scripts are reported, not silently dropped.
        assert_eq!(notes.len(), 2, "{notes:?}");
        assert!(notes.iter().any(|note| note.contains("module")));
        assert!(notes
            .iter()
            .any(|note| note.contains("unsupported type")));
    }

    #[test]
    fn src_scripts_resolve_against_the_document_base_url() {
        let doc = dom("<!doctype html><html><body><script src=\"/abs.js\"></script><script src=\"rel.js\"></script></body></html>");
        let base = Url::parse("https://example.test/deep/page.html").expect("base");
        let mut asked = Vec::new();
        let extraction = js_extract(&doc, &base, &mut |url| {
            asked.push(url.as_str().to_owned());
            Ok("// ok".to_string())
        });
        assert_eq!(
            asked,
            vec!["https://example.test/abs.js", "https://example.test/deep/rel.js"]
        );
        assert_eq!(extraction.scripts.len(), 2);
    }

    #[test]
    fn unfetchable_and_unsupported_scripts_are_skipped_with_notes() {
        let doc = dom(SCRIPT_PAGE);
        let base = Url::parse("file:///srv/site/index.html").expect("base");
        let extraction = js_extract(&doc, &base, &mut |url| {
            if url.path().ends_with("gone.js") {
                Err("nope: not found".to_string())
            } else {
                Ok("// ok".to_string())
            }
        });
        // The broken src is skipped but its siblings still execute.
        assert_eq!(extraction.scripts.len(), 3);
        assert!(extraction.notes.iter().any(|note| note.contains("module")));
        assert!(extraction
            .notes
            .iter()
            .any(|note| note.contains("unsupported type")));
        assert!(extraction
            .notes
            .iter()
            .any(|note| note.contains("gone.js") && note.contains("nope: not found")));
    }

    #[test]
    fn body_snapshot_reads_the_body_element_text() {
        let doc = dom("<!doctype html><html><head><title>T</title></head><body>hello <b>js</b></body></html>");
        assert_eq!(body_text_snapshot(&doc).as_deref(), Some("hello js"));
        let head_only = dom("<!doctype html><html><head><title>T</title></head></html>");
        // blitz materialises a <body> for every HTML document, so the snapshot
        // is Some(…) with the body's (empty) text.
        assert_eq!(
            body_text_snapshot(&head_only).as_deref(),
            Some(""),
            "an empty body snapshots as empty text"
        );
    }

    /// Alias so the helper above reads well next to the module-level import.
    fn js_extract(
        doc: &blitz_dom::BaseDocument,
        base: &Url,
        fetch: &mut dyn FnMut(&Url) -> Result<String, String>,
    ) -> Extraction {
        extract_scripts(doc, base, fetch)
    }

    #[test]
    fn classic_type_filter_accepts_plain_scripts_only() {
        assert!(is_classic_script_type(None));
        assert!(is_classic_script_type(Some("   ")));
        assert!(is_classic_script_type(Some("text/JavaScript")));
        assert!(is_classic_script_type(Some("application/javascript")));
        assert!(!is_classic_script_type(Some("application/json")));
        assert!(!is_classic_script_type(Some("text/template")));
        assert!(is_module_type(Some("module")));
        assert!(!is_module_type(Some("text/javascript")));
    }

    // -----------------------------------------------------------------
    // The DOM bridge: real blitz document behind the JS DOM (design B)

    const DOM_PAGE: &str = r####"<!doctype html>
<html><head><title>Bridge</title></head>
<body>
  <div id="wrap" class="outer">
    <p id="p1" class="inner first">one</p>
    <p id="p2">two</p>
    <span data-x="v">three</span>
  </div>
</body></html>"####;

    /// Run `source` against `doc` (registered as the live document) and
    /// return the completion value; panics with the snippet on JS errors.
    fn dom_eval(
        runtime: &mut JsRuntime,
        doc: &mut blitz_html::HtmlDocument,
        source: &str,
    ) -> JsValue {
        match runtime.eval_with_doc(source, doc) {
            Ok(value) => value,
            Err(err) => {
                let message = first_line(&err);
                panic!("dom snippet failed: {message}\n--- snippet ---\n{source}")
            }
        }
    }

    /// Fresh runtime + live document for `DOM_PAGE`.
    fn dom_world() -> (JsRuntime, blitz_html::HtmlDocument) {
        let runtime = JsRuntime::new(sample_host());
        let doc = dom(DOM_PAGE);
        (runtime, doc)
    }

    /// Evaluate `source` against a fresh DOM world, expecting `true`.
    fn dom_true(source: &str) {
        let (mut runtime, mut doc) = dom_world();
        let value = dom_eval(&mut runtime, &mut doc, source);
        assert!(value.to_boolean(), "expected truthy for: {source}");
    }

    #[test]
    fn dom_document_anchors_point_at_the_real_tree() {
        let (mut runtime, mut doc) = dom_world();
        let value = dom_eval(
            &mut runtime,
            &mut doc,
            r#"
            [
              document.documentElement.nodeName,
              document.documentElement.nodeType,
              document.head !== null,
              document.body.nodeName,
              document.body.textContent.indexOf("three") >= 0,
              document.getElementById("p1").textContent,
              document.querySelector("span[data-x]").getAttribute("data-x")
            ].join("|");
            "#,
        );
        let text = value.to_string(&mut runtime.context).expect("string").to_std_string_escaped();
        assert_eq!(
            text,
            "HTML|1|true|BODY|true|one|v",
            "anchors must resolve through the live document"
        );
    }

    #[test]
    fn dom_node_identity_is_stable() {
        let (mut runtime, mut doc) = dom_world();
        let value = dom_eval(
            &mut runtime,
            &mut doc,
            r##"
            var body = document.body;
            var p1 = document.getElementById("p1");
            [
              document.body === body,
              document.getElementById("p1") === p1,
              document.querySelector("#p1") === p1,
              body.ownerDocument.body === body
            ].join(",");
            "##,
        );
        assert_eq!(
            value.to_string(&mut runtime.context).expect("s").to_std_string_escaped(),
            "true,true,true,true"
        );
    }

    #[test]
    fn dom_child_nodes_list_is_live() {
        dom_true(
            r#"
            var kids = document.body.childNodes;
            var before = kids.length;
            document.body.appendChild(document.createElement("i"));
            var grew = kids.length === before + 1;
            var lastIsNew = kids[kids.length - 1].nodeName === "I";
            document.body.removeChild(kids[kids.length - 1]);
            grew && kids.length === before && document.body.childNodes.length === before;
            "#,
        );
    }

    #[test]
    fn dom_query_selector_all_is_live_and_rejects_bad_selectors() {
        dom_true(
            r#"
            var list = document.querySelectorAll("p");
            var before = list.length;
            var p = document.createElement("p");
            document.body.appendChild(p);
            var grew = list.length === before + 1 && list[before] === p;
            document.body.removeChild(p);
            var threw = false;
            try { document.querySelectorAll("div["); } catch (e) { threw = e instanceof SyntaxError; }
            var emptyOk = document.querySelectorAll("table").length === 0;
            grew && list.length === before && threw && emptyOk;
            "#,
        );
    }

    #[test]
    fn dom_attributes_round_trip() {
        dom_true(
            r#"
            var el = document.getElementById("p1");
            var a1 = el.getAttribute("class") === "inner first";
            var a2 = el.getAttribute("nope") === null;
            el.setAttribute("data-k", "a&b");
            var a3 = el.getAttribute("data-k") === "a&b";
            var map = el.attributes;
            var has = map.getNamedItem("data-k").value === "a&b" && map.length >= 3;
            var indexed = map.item(0).name.length > 0;
            el.removeAttribute("data-k");
            var a4 = el.getAttribute("data-k") === null && !el.hasAttribute("data-k");
            var threw = false;
            try { el.setAttribute("<bad>", "x"); } catch (e) { threw = e instanceof RangeError; }
            a1 && a2 && a3 && has && indexed && a4 && threw;
            "#,
        );
    }

    #[test]
    fn dom_innerhtml_set_and_serialize() {
        dom_true(
            r#"
            var wrap = document.getElementById("wrap");
            wrap.innerHTML = '<p class="x">hi &amp; <b>bold</b></p><br>';
            var out = wrap.innerHTML;
            var b = wrap.querySelector("b");
            var expected = out === '<p class="x">hi &amp; <b>bold</b></p><br>';
            expected && b.textContent === "bold" && wrap.children.length === 2;
            "#,
        );
    }

    #[test]
    fn dom_textcontent_set_replaces_children() {
        dom_true(
            r#"
            var p = document.getElementById("p2");
            p.appendChild(document.createElement("b"));
            p.firstChild.textContent = "nested";
            var had = p.childNodes.length > 1;
            p.textContent = "plain & done";
            var only = p.childNodes.length === 1 && p.firstChild.nodeType === 3;
            only && p.textContent === "plain & done" && had && p.innerHTML === "plain &amp; done";
            "#,
        );
    }

    #[test]
    fn dom_insert_remove_replace_children() {
        dom_true(
            r#"
            var wrap = document.getElementById("wrap");
            var p1 = document.getElementById("p1");
            var span = document.querySelector("span");
            var marker = document.createElement("em");
            wrap.insertBefore(marker, p1);
            var first = wrap.firstElementChild === marker;
            var tail = document.createElement("u");
            wrap.insertBefore(tail, null);
            var last = wrap.lastElementChild === tail;
            wrap.removeChild(tail);
            var gone = wrap.lastElementChild !== tail;
            var b2 = document.createElement("b");
            var old = wrap.replaceChild(b2, span);
            var swapped = old === span && wrap.querySelector("b") !== null && span.parentNode !== wrap;
            first && last && gone && swapped;
            "#,
        );
    }

    #[test]
    fn dom_mutations_validate_their_arguments() {
        let (mut runtime, mut doc) = dom_world();
        // Removing a node that is not a child must raise (NotFoundError —
        // the DOMException family maps onto TypeError in this bridge).
        let err = runtime
            .eval_with_doc(
                r#"
                var other = document.createElement("div");
                document.body.removeChild(other);
                "#,
                &mut doc,
            )
            .expect_err("removing a non-child must throw");
        assert!(first_line(&err).contains("NotFoundError"), "{err}");

        let threw = dom_eval(
            &mut runtime,
            &mut doc,
            r#"
            var ok = false;
            try { document.body.appendChild(42); } catch (e) { ok = e instanceof TypeError; }
            ok;
            "#,
        );
        assert!(threw.to_boolean(), "non-node child must be a TypeError");
    }

    #[test]
    fn dom_clone_node_shallow_vs_deep() {
        dom_true(
            r#"
            var p1 = document.getElementById("p1");
            p1.appendChild(document.createElement("i"));
            var deep = p1.cloneNode(true);
            var shallow = p1.cloneNode(false);
            var idCopies = deep.getAttribute("id") === "p1";
            deep.setAttribute("id", "copy");
            var kidsOk = deep.children.length === 1 && shallow.children.length === 0;
            var textOk = deep.textContent === p1.textContent;
            // Clones are detached: parentNode is null until inserted.
            var detached = deep.parentNode === null && shallow.parentNode === null;
            var live = p1.parentNode !== null;
            idCopies && kidsOk && textOk && detached && live;
            "#,
        );
    }

    #[test]
    fn dom_document_fragment_moves_all_children() {
        dom_true(
            r#"
            var frag = document.createDocumentFragment();
            var a = document.createElement("a");
            var b = document.createElement("b");
            frag.appendChild(a);
            frag.appendChild(b);
            var inFrag = frag.childNodes.length === 2 && a.parentNode === null;
            var list = document.body.childNodes;
            var before = list.length;
            document.body.appendChild(frag);
            var moved = frag.childNodes.length === 0 &&
                        document.body.querySelectorAll("a").length === 1 &&
                        a.parentNode === document.body &&
                        list.length === before + 2;
            document.body.removeChild(a);
            document.body.removeChild(b);
            inFrag && moved;
            "#,
        );
    }

    #[test]
    fn dom_getelementsbytagname_reads_the_tree() {
        dom_true(
            r#"
            var ps = document.getElementsByTagName("p");
            var before = ps.length;
            var p = document.createElement("p");
            p.setAttribute("class", "inner");
            document.body.appendChild(p);
            var after = ps.length;
            var cls = document.getElementsByClassName("inner");
            var liveCls = cls.length === 2;
            document.body.removeChild(p);
            var star = document.getElementsByTagName("*").length > ps.length;
            (after === before + 1 || after === before) && liveCls && star;
            "#,
        );
    }

    #[test]
    fn dom_missing_nodes_read_as_null() {
        dom_true(
            r##"
            document.getElementById("nope") === null &&
            document.querySelector("#nope") === null &&
            document.body.getAttribute("nope") === null;
            "##,
        );
    }

    #[test]
    fn dom_invalid_names_throw_range_error() {
        let (mut runtime, mut doc) = dom_world();
        let err = runtime
            .eval_with_doc(r#"document.createElement("<div>")"#, &mut doc)
            .expect_err("invalid tag name must throw");
        assert!(first_line(&err).contains("InvalidCharacterError"), "{err}");
        let ok = dom_eval(
            &mut runtime,
            &mut doc,
            r#"
            var r1 = false;
            try { document.createElement("<div>"); } catch (e) { r1 = e instanceof RangeError; }
            var r2 = false;
            try { document.body.setAttribute("", "x"); } catch (e) { r2 = e instanceof RangeError; }
            r1 && r2;
            "#,
        );
        assert!(ok.to_boolean());
    }

    #[test]
    fn dom_siblings_and_comments() {
        dom_true(
            r##"
            var p2 = document.getElementById("p2");
            var prev = p2.previousElementSibling;
            var next = p2.nextElementSibling;
            var sibs = prev.id === "p1" && next.nodeName === "SPAN" &&
                       next.previousElementSibling === p2 &&
                       next.nextElementSibling === null;
            var c = document.createComment(" hi ");
            var cOk = c.nodeType === 8 && c.nodeName === "#comment" && c.nodeValue === " hi ";
            c.nodeValue = "bye";
            var mutated = c.nodeValue === "bye";
            document.body.appendChild(c);
            var appended = document.body.lastChild === c && c.parentNode !== null;
            document.body.removeChild(c);
            sibs && cOk && mutated && appended;
            "##,
        );
    }

    #[test]
    fn dom_bridge_survives_the_script_batch() {
        // The tab's real entry point: page scripts run through run_scripts
        // with the document registered, and DOM mutations are visible to
        // later scripts in the same batch.
        let host = sample_host();
        let mut runtime = JsRuntime::new(Rc::clone(&host));
        let mut doc = dom(DOM_PAGE);
        let scripts = [
            ScriptSource {
                code: r#"var d = document.createElement("div");
                         d.id = "built";
                         d.textContent = "by script one";
                         document.body.appendChild(d);"#
                    .to_string(),
                origin: "inline".to_string(),
            },
            ScriptSource {
                code: r#"document.title = document.getElementById("built").textContent.toUpperCase();
                         window.sawDom = true;"#
                    .to_string(),
                origin: "inline".to_string(),
            },
        ];
        let errors = runtime.run_scripts(&scripts, Some(&mut doc));
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(
            host.borrow().title_assigned.as_deref(),
            Some("BY SCRIPT ONE")
        );
        // The mutation landed in the real document, not a JS-only shadow.
        assert!(doc.query_selector("#built").expect("valid selector").is_some());
        let text = dom_eval(
            &mut runtime,
            &mut doc,
            "document.getElementById('built').textContent",
        );
        assert_eq!(
            text.to_string(&mut runtime.context).expect("s").to_std_string_escaped(),
            "by script one"
        );
    }

    // -------------------------------------------------------------------
    // HTMLMediaElement glue (task: playback) — the player is stubbed by a
    // seeded snapshot, exactly what the tab's pump writes each tick.

    const MEDIA_PAGE: &str = r####"<!doctype html>
<html><head><title>Tube</title></head>
<body><video id="v" src="flower.mp4" width="320" height="240"></video></body></html>"####;

    /// Runtime + live `MEDIA_PAGE` + host, with a stubbed player: the
    /// snapshot a real pump would have written for the `<video>` node.
    fn media_world() -> (JsRuntime, blitz_html::HtmlDocument, HostRef, u64) {
        let host = sample_host();
        let mut doc = dom(MEDIA_PAGE);
        let raw = {
            let base: &blitz_dom::BaseDocument = &doc;
            base.tree()
                .iter()
                .find(|(_id, node)| {
                    node.element_data()
                        .is_some_and(|el| el.name.local == blitz_dom::local_name!("video"))
                })
                .map(|(id, _node)| id.as_u64())
                .expect("the page has a <video>")
        };
        host.borrow_mut().media.snapshots.insert(
            raw,
            crate::runtime::MediaSnapshot {
                duration: 10.5,
                current_time: 2.25,
                paused: false,
                ended: false,
                rate: 1.5,
                muted: true,
                width: 320,
                height: 240,
                error: Some("unsupported codec".to_string()),
            },
        );
        let runtime = JsRuntime::new(Rc::clone(&host));
        // Registering the document happens inside dom_eval; hand back the
        // pieces so tests can both eval and inspect the host.
        (runtime, doc, host, raw)
    }

    fn js_text(runtime: &mut JsRuntime, value: JsValue) -> String {
        value
            .to_string(&mut runtime.context)
            .expect("string")
            .to_std_string_escaped()
    }

    #[test]
    fn video_properties_read_the_pumped_snapshot_with_the_right_types() {
        let (mut runtime, mut doc, _host, _raw) = media_world();
        let value = dom_eval(
            &mut runtime,
            &mut doc,
            r#"
            var v = document.querySelector("video");
            [
              typeof v.currentTime, typeof v.duration, typeof v.paused,
              typeof v.ended, typeof v.playbackRate, typeof v.muted,
              typeof v.videoWidth, typeof v.videoHeight,
              v.currentTime, v.duration, v.paused, v.playbackRate,
              v.muted, v.videoWidth, v.videoHeight, v.mediaError,
              v instanceof HTMLMediaElement, v.canPlayType("video/mp4")
            ].join("|");
            "#,
        );
        assert_eq!(
            js_text(&mut runtime, value),
            "number|number|boolean|boolean|number|boolean|number|number\
             |2.25|10.5|false|1.5|true|320|240|unsupported codec|true|probably"
        );
    }

    #[test]
    fn video_writes_become_media_commands_for_the_poll_loop() {
        let (mut runtime, mut doc, host, raw) = media_world();
        dom_eval(
            &mut runtime,
            &mut doc,
            r#"
            var v = document.querySelector("video");
            v.play();
            v.pause();
            v.currentTime = 7.25;
            v.muted = false;
            v.playbackRate = 2;
            "#,
        );
        use crate::runtime::MediaCommand;
        let commands = host.borrow().media.commands.clone();
        assert_eq!(
            commands,
            vec![
                MediaCommand::Play { node: raw },
                MediaCommand::Pause { node: raw },
                MediaCommand::Seek {
                    node: raw,
                    seconds: 7.25
                },
                MediaCommand::SetMuted {
                    node: raw,
                    muted: false
                },
                MediaCommand::SetRate { node: raw, rate: 2.0 },
            ],
            "writes queue commands in order; nothing happens until the loop applies them"
        );
        // The reads still report the last pumped snapshot, not the writes.
        let value = dom_eval(
            &mut runtime,
            &mut doc,
            "document.querySelector('video').currentTime",
        );
        assert_eq!(value.as_number().expect("number"), 2.25);
    }

    #[test]
    fn non_media_elements_never_consult_the_host() {
        let host = sample_host();
        let mut runtime = JsRuntime::new(Rc::clone(&host));
        let mut doc = dom(DOM_PAGE);
        let value = dom_eval(
            &mut runtime,
            &mut doc,
            r#"
            var d = document.getElementById("p1");
            d.play(); // a media method on a <p>: a no-op, not an exception
            d.currentTime = 5;
            d.muted = true;
            [
              typeof d.paused, d.paused, typeof d.duration, isNaN(d.duration),
              d.videoWidth, d.mediaError === null
            ].join("|");
            "#,
        );
        assert_eq!(
            js_text(&mut runtime, value),
            "boolean|true|number|true|0|true",
            "non-media elements read the no-media defaults"
        );
        let host = host.borrow();
        assert!(
            host.media.commands.is_empty(),
            "media writes on non-media elements must not reach the host"
        );
    }
}
