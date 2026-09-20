//! DevTools console diagnostics (task: JS/CSS/DOM errors on the wire).
//!
//! JS `console.*` and script exceptions ride [`crate::js`]'s host cell into
//! `FromTab::ConsoleLog { source: "js" }`; native DOM throws are classified
//! there as `source: "dom"`. This module is the **CSS** half: the tab's
//! DevTools console entries (`"css"`) come from two best-effort seams.
//!
//! ## Why a re-parse seam (the upstream gap, documented per the task)
//!
//! Blitz parses CSS through Stylo and passes **`None`** as Stylo's
//! `ParseErrorReporter` (`BaseDocument::make_stylesheet` in blitz-dom
//! 0.3.0-beta.2), so every stylesheet parse error Stylo detects is silently
//! dropped before it could reach any sink. Blitz's own diagnostics go through
//! `tracing`, and no `tracing` subscriber is installed in the tab process —
//! installing one would only capture a handful of `tracing::warn!` sites
//! (unsupported *inline style properties*, resource-load failures), not the
//! invalid-declaration reporting we need. Hooking upstream is not cheaply
//! capturable, so this module **re-parses the document's CSS with Stylo
//! itself** (same engine, same version, zero new compilation cost) behind a
//! real [`style::error_reporting::ParseErrorReporter`]. What that captures:
//!
//! * every `<style>` element's text, re-parsed as an author stylesheet —
//!   invalid declarations (`p { invalid: invalid }`), bad selectors, bad
//!   at-rules, unterminated blocks — reported per line/column exactly as a
//!   browser console shows them;
//! * every inline `style="…"` attribute, re-parsed as a synthetic
//!   `ironsurf-inline-scan { … }` rule (same error reporting path);
//! * external CSS with the wrong `Content-Type` (see
//!   [`css_content_type_warning`], called by the tab's `BlitzNetProvider` on
//!   every settled `.css` response).
//!
//! Vendor-prefixed **unknown property names** are deliberately NOT reported
//! (see [`silently_dropped_unknown_property`]): every engine drops
//! `-webkit-…`/`-ms-…`-style names with no console output, and page code
//! writes them intentionally (Mastodon's progress bar stamps five transition
//! prefixes per style-attribute update — reporting them floods the console
//! with errors that mean nothing). Value errors on REAL properties
//! (`color: bogus`) still report.
//!
//! Known honest gaps: `<link rel=stylesheet>` *content* is fetched by blitz
//! and never appears as DOM text, so its parse errors are not visible to the
//! seam (its failures and wrong content types are); `@import` bodies are not
//! followed (the seam parses with no stylesheet loader so a re-parse can
//! never touch the network).
//!
//! The `log` crate is deliberately **not** bridged: IronSurf's own logging
//! ([`ironsurf_common::logging`]) is dependency-free stderr, Stylo logs
//! nothing through `log` without a reporter, and a global `log` sink could
//! block the tab's 64 KiB stderr pipe — the volume contract in
//! `ironsurf_common::logging` applies. Everything here flows over the IPC
//! channel instead.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use blitz_dom::{local_name, BaseDocument, NodeData, NodeId};
use url::Url;

use selectors::matching::QuirksMode;
use style::error_reporting::{ContextualParseError, ParseErrorReporter};
use style::media_queries::MediaList;
use style::servo_arc::Arc as ServoArc;
use style::shared_lock::SharedRwLock;
use style::stylesheets::{AllowImportRules, Origin, Stylesheet, UrlExtraData};

/// One queued DevTools console entry: the three string fields of
/// `FromTab::ConsoleLog` (source `"js" | "css" | "dom" | "page"`). Queues of
/// these are the cross-thread hand-off (fetch workers → tab loop); the tab
/// loop drains them and `emit`s the real message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConsoleEntry {
    pub source: String,
    pub level: String,
    pub message: String,
}

/// Shared console queue (`Arc<Mutex<..>>`, same shape as the DevTools network
/// queue in [`crate::netmod`]).
pub type ConsoleQueue = Arc<Mutex<Vec<ConsoleEntry>>>;

/// New empty console queue.
pub fn console_queue() -> ConsoleQueue {
    Arc::new(Mutex::new(Vec::new()))
}

/// Cap on entries one scan pass returns (a stylesheet with a thousand broken
/// declarations yields one page-load worth of console noise, not a flood).
const MAX_ENTRIES_PER_SCAN: usize = 16;
/// Cap on remembered (already-reported) CSS bodies per document.
const MAX_SCANNED_MEMORY: usize = 512;

/// Per-document CSS scan bookkeeping: content hashes of CSS already parsed
/// (and reported) this document, so a `<style>` re-read every poll tick (and
/// every timer batch) never reports the same sheet twice.
#[derive(Default)]
pub struct CssScanState {
    scanned: HashSet<u64>,
}

impl CssScanState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Forget everything (fresh document: `load_html` → `run_scripts`).
    pub fn reset(&mut self) {
        self.scanned.clear();
    }

    /// `true` when `css` has not been scanned yet (records it; the record set
    /// is bounded — past the cap everything new is still "unscanned" but no
    /// longer deduplicated, which degrades to a re-scan, never a miss).
    fn note(&mut self, css: &str) -> bool {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        css.hash(&mut hasher);
        let hash = hasher.finish();
        if self.scanned.contains(&hash) {
            return false;
        }
        if self.scanned.len() < MAX_SCANNED_MEMORY {
            self.scanned.insert(hash);
        }
        true
    }
}

/// Collects Stylo parse errors behind the `ParseErrorReporter` seam.
struct CollectingReporter {
    entries: Arc<Mutex<Vec<String>>>,
}

impl ParseErrorReporter for CollectingReporter {
    fn report_error(
        &self,
        _url: &UrlExtraData,
        location: cssparser::SourceLocation,
        error: ContextualParseError,
    ) {
        // An unrecognised property name every engine drops silently is not a
        // console error (see [`silently_dropped_unknown_property`]).
        if let ContextualParseError::UnsupportedPropertyDeclaration(decl, _, _) = &error {
            if let Some(name) = declaration_name(decl) {
                if silently_dropped_unknown_property(&name) {
                    return;
                }
            }
        }
        let mut entries = self.entries.lock().expect("css reporter mutex");
        // Bounded: the parse can surface hundreds of errors on a hostile
        // sheet; the first ones describe the problem well enough.
        if entries.len() < 64 {
            entries.push(format!("{}:{} {}", location.line, location.column, error));
        }
    }
}

/// The property name of a declaration slice (`"color: red !important"` →
/// `"color"`), lowercased; `None` when the slice isn't `name: value` shaped.
fn declaration_name(decl: &str) -> Option<String> {
    let (name, _) = decl.split_once(':')?;
    let name = name.trim().to_ascii_lowercase();
    (!name.is_empty()).then_some(name)
}

/// `true` when no browser logs this unknown property name: dashed idents
/// (`-webkit-transition`, `-ms-filter`, `_zoom`, … — the leading dash IS the
/// "everywhere-invalid, ignore me" convention), legacy unprefixed spellings
/// of `-ms-*` names (`ms-transition`, from old IE-era code that survived into
/// production inline styles), and their `-ms-` forms once Internet
/// Explorer's required hyphen was standardised away. Stylo itself already
/// silences the first family (`is_non_mozilla_vendor_identifier`); the other
/// two reach the reporter, and a real console stays quiet about all three —
/// only the VALUE of a genuine property (`color: bogus`) is worth a page
/// error.
fn silently_dropped_unknown_property(name: &str) -> bool {
    name.starts_with('-')
        || name.starts_with('_')
        || (name.starts_with("ms-") && KNOWN_MS_ALIASES.contains(&name))
}

/// Standard properties `ms-x` stands in for (the `-ms-` vendor form of every
/// one of these is or was a real engine property).
const KNOWN_MS_ALIASES: &[&str] = &[
    "ms-animation",
    "ms-animation-delay",
    "ms-animation-direction",
    "ms-animation-duration",
    "ms-animation-fill-mode",
    "ms-animation-iteration-count",
    "ms-animation-name",
    "ms-animation-play-state",
    "ms-animation-timing-function",
    "ms-appearance",
    "ms-backface-visibility",
    "ms-background-size",
    "ms-border-bottom-left-radius",
    "ms-border-bottom-right-radius",
    "ms-border-top-left-radius",
    "ms-border-top-right-radius",
    "ms-box-shadow",
    "ms-box-sizing",
    "ms-column-count",
    "ms-filter",
    "ms-flex",
    "ms-flex-align",
    "ms-flex-direction",
    "ms-flex-item-align",
    "ms-flex-order",
    "ms-flex-pack",
    "ms-flex-wrap",
    "ms-hyphens",
    "ms-interpolation-mode",
    "ms-linear-gradient",
    "ms-order",
    "ms-perspective",
    "ms-perspective-origin",
    "ms-radial-gradient",
    "ms-transform",
    "ms-transform-origin",
    "ms-transform-style",
    "ms-transition",
    "ms-transition-delay",
    "ms-transition-duration",
    "ms-transition-property",
    "ms-transition-timing-function",
    "ms-user-select",
];

/// Re-parse `css` as an author stylesheet through Stylo's real error-reporting
/// path (the one blitz switches off) and return the console messages for the
/// problems found (empty when the CSS is valid). Never touches the network
/// (`@import` has no loader here) and never fails: a parse that panics… does
/// not happen (Stylo is infallible-by-dropping), but anything unexpected
/// degrades to "no entries".
fn parse_css_errors(css: &str, base: &Url) -> Vec<String> {
    let entries = Arc::new(Mutex::new(Vec::<String>::new()));
    let reporter = CollectingReporter {
        entries: Arc::clone(&entries),
    };
    let lock = SharedRwLock::new();
    let media = ServoArc::new(lock.wrap(MediaList::empty()));
    let _sheet = Stylesheet::from_str(
        css,
        UrlExtraData::from(base.clone()),
        Origin::Author,
        media,
        lock,
        /* stylesheet_loader = */ None,
        Some(&reporter),
        QuirksMode::NoQuirks,
        // Same as blitz; with no loader a parsed `@import` simply never
        // resolves — the re-parse can never reach the network.
        AllowImportRules::Yes,
    );
    // Drain before the reporter drops (it holds a second handle to the queue).
    let collected = std::mem::take(&mut *entries.lock().expect("css reporter mutex"));
    collected
}

fn walk(doc: &BaseDocument, id: NodeId, out: &mut Vec<NodeId>) {
    out.push(id);
    if let Some(node) = doc.get_node(id) {
        for child in node.children.iter().copied() {
            walk(doc, child, out);
        }
    }
}

/// Scan `doc`'s CSS seams and return console entries (source `"css"`,
/// level `"error"`) for everything invalid that [`state`] has not reported
/// yet: every `<style>` element body and every `style="…"` attribute
/// (re-parsed through Stylo; see the module docs for the upstream gap).
pub fn scan_css(doc: &BaseDocument, base: &Url, state: &mut CssScanState) -> Vec<ConsoleEntry> {
    let mut ids = Vec::new();
    walk(doc, doc.root_node().id, &mut ids);
    let mut entries = Vec::new();
    for id in ids {
        if entries.len() >= MAX_ENTRIES_PER_SCAN {
            break;
        }
        let Some(node) = doc.get_node(id) else {
            continue;
        };
        if !matches!(node.data, NodeData::Element(_)) {
            continue;
        }
        let is_style = node
            .element_data()
            .is_some_and(|el| el.name.local == local_name!("style"));
        if is_style {
            let text = node.text_content();
            if text.trim().is_empty() || !state.note(&text) {
                continue;
            }
            for message in parse_css_errors(&text, base) {
                if entries.len() >= MAX_ENTRIES_PER_SCAN {
                    break;
                }
                entries.push(ConsoleEntry {
                    source: "css".to_string(),
                    level: "error".to_string(),
                    message: format!("<style>: {message}"),
                });
            }
        } else if let Some(attr) = node.attr(local_name!("style")) {
            if attr.trim().is_empty() || !state.note(&format!("attr\0{attr}")) {
                continue;
            }
            // Inline style attributes parse through the same seam by
            // wrapping the declarations in a synthetic rule block.
            let synthetic = format!("ironsurf-inline-scan {{ {attr} }}");
            for message in parse_css_errors(&synthetic, base) {
                if entries.len() >= MAX_ENTRIES_PER_SCAN {
                    break;
                }
                entries.push(ConsoleEntry {
                    source: "css".to_string(),
                    level: "error".to_string(),
                    message: format!("style=\"{attr}\": {message}"),
                });
            }
        }
    }
    entries
}

/// The wrong-`Content-Type` seam: a response the page almost certainly asked
/// for as a stylesheet (`.css` path) that no browser would apply as one.
/// `ok` is the settled response's status, `headers` its header list (original
/// casing). Returns the console entry (source `"css"`, level `"error"` —
/// Firefox blocks non-`text/css` stylesheets, and so do we for CSS parsing
/// purposes) or `None` when the response looks like CSS.
pub fn css_content_type_warning(
    url: &Url,
    status: u16,
    headers: &[(String, String)],
) -> Option<ConsoleEntry> {
    if !(status == 200 || status == 204 || status == 206) {
        return None; // failures are the network panel's story, not the console's
    }
    let is_css_path = url
        .path_segments()
        .and_then(|mut s| s.next_back())
        .is_some_and(|last| last.rsplit('.').next() == Some("css") && last.len() > 4);
    if !is_css_path {
        return None;
    }
    let content_type = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-type"))
        .map(|(_, value)| value.clone());
    let css = content_type
        .as_deref()
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .eq_ignore_ascii_case("text/css");
    if css {
        return None;
    }
    let seen = content_type
        .as_deref()
        .map(|value| {
            value
                .split(';')
                .next()
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase()
        })
        .unwrap_or_else(|| "<none>".to_string());
    Some(ConsoleEntry {
        source: "css".to_string(),
        level: "error".to_string(),
        message: format!(
            "refused to apply stylesheet {url} (served as \"{seen}\", expected \"text/css\")"
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn css_path_content_type_matrix() {
        let css = Url::parse("https://example.com/a.css").unwrap();
        let hdr = |v: &str| vec![("Content-Type".to_string(), v.to_string())];
        // text/css (with or without parameters) is fine.
        assert!(css_content_type_warning(&css, 200, &hdr("text/css")).is_none());
        assert!(css_content_type_warning(&css, 200, &hdr("Text/CSS; charset=utf-8")).is_none());
        // Wrong type (the Firefox-blocked case) and a missing header both warn.
        let wrong = css_content_type_warning(&css, 200, &hdr("text/html")).expect("warn");
        assert_eq!(wrong.source, "css");
        assert_eq!(wrong.level, "error");
        assert!(wrong.message.contains("text/html"));
        assert!(css_content_type_warning(&css, 200, &[]).is_some());
        // Non-CSS paths and failures never warn (failures are network events).
        assert!(css_content_type_warning(
            &Url::parse("https://example.com/a.png").unwrap(),
            200,
            &hdr("text/html")
        )
        .is_none());
        assert!(css_content_type_warning(&css, 404, &hdr("text/html")).is_none());
        // A query string on the .css URL still reads as a stylesheet request.
        assert!(css_content_type_warning(
            &Url::parse("https://example.com/a.css?v=2").unwrap(),
            200,
            &hdr("text/plain")
        )
        .is_some());
    }

    /// The re-parse seam itself (the wire test `tests/console_log.rs` covers
    /// it end to end): invalid declarations in a `<style>` body and in an
    /// inline `style="…"` attribute are reported as `css`/`error`, valid CSS
    /// is silent, and repeat scans of the same content are deduped.
    #[test]
    fn scan_reports_invalid_style_and_dedupes() {
        let doc = blitz_html::HtmlDocument::from_html(
            "<!doctype html><html><head>\
             <style>p { invalid-property: nope }</style>\
             </head><body><p style=\"color: bogus\">x</p></body></html>",
            blitz_dom::DocumentConfig::default(),
        );
        let base = Url::parse("about:blank").expect("about:blank");
        let mut state = CssScanState::new();
        let first = scan_css(&doc, &base, &mut state);
        assert!(
            first
                .iter()
                .any(|e| e.source == "css" && e.level == "error" && e.message.contains("<style>")),
            "the <style> body must be reported: {first:?}"
        );
        assert!(
            first
                .iter()
                .any(|e| e.source == "css" && e.message.contains("color")),
            "the inline style attribute must be reported: {first:?}"
        );
        // Same document, second scan (the poll loop re-enters every JS turn):
        // content-hash dedup means nothing new.
        let second = scan_css(&doc, &base, &mut state);
        assert!(second.is_empty(), "repeat scans dedupe: {second:?}");

        // Valid CSS is silent.
        let good = blitz_html::HtmlDocument::from_html(
            "<!doctype html><html><head><style>p { color: red; font-size: 12px }</style>\
             </head><body><p style=\"margin: 0\">x</p></body></html>",
            blitz_dom::DocumentConfig::default(),
        );
        let mut fresh = CssScanState::new();
        let entries = scan_css(&good, &base, &mut fresh);
        assert!(entries.is_empty(), "valid CSS must not warn: {entries:?}");
    }

    /// The Mastodon progress-bar case (live infosec.exchange console flood):
    /// legacy/vendor unknown NAMES are dropped silently by every engine and
    /// must produce no console entries — while genuine property VALUE errors
    /// still report.
    #[test]
    fn vendor_prefixed_unknown_properties_are_silent() {
        let base = Url::parse("about:blank").expect("about:blank");
        let doc = blitz_html::HtmlDocument::from_html(
            "<!doctype html><html><head>\
             <style>a { -ms-transition: width 200ms; -webkit-box-shadow: 0 0 1px red; \
                        _zoom: 1; ms-filter: progid:dx; ms-transition: width 200ms linear 0s }</style>\
             </head><body><div style=\"will-change: width, opacity; \
             transition: width 200ms linear 0s; ms-transition: width 200ms linear 0s; \
             -webkit-transition: width 200ms linear 0s; -moz-transition: width 200ms linear 0s; \
             -o-transition: width 200ms linear 0s; width: 100%; opacity: 1\">x</div>\
             <p style=\"color: bogus\">y</p></body></html>",
            blitz_dom::DocumentConfig::default(),
        );
        let mut state = CssScanState::new();
        let entries = scan_css(&doc, &base, &mut state);
        assert!(
            entries
                .iter()
                .all(|e| !e.message.contains("transition") && !e.message.contains("filter")),
            "vendor/legacy names must be silent: {entries:?}"
        );
        assert!(
            entries.iter().any(|e| e.message.contains("color")),
            "a real property's bad value still reports: {entries:?}"
        );
        // Pure vendor noise ⇒ zero entries.
        let quiet = blitz_html::HtmlDocument::from_html(
            "<!doctype html><html><body><div style=\"-ms-transition: all 1s; ms-transition: all 1s; -webkit-user-select: none\">x</div></body></html>",
            blitz_dom::DocumentConfig::default(),
        );
        let mut fresh = CssScanState::new();
        assert!(scan_css(&quiet, &base, &mut fresh).is_empty());
    }

    #[test]
    fn scan_state_dedupes_by_content() {
        let mut state = CssScanState::new();
        assert!(state.note("p { color: red }"));
        assert!(!state.note("p { color: red }"));
        assert!(state.note("p { color: blue }"));
        state.reset();
        assert!(state.note("p { color: red }"));
    }
}
