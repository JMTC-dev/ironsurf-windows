//! The Boa JavaScript layer (task 8): `<script>` extraction plus a
//! deliberately tiny binding surface, per `ARCHITECTURE.md` §JavaScript (Boa).
//!
//! Per navigation the runtime rebuilds **one** `boa_engine::Context`
//! (fresh realm per document, no cross-document leakage), binds:
//!
//! - `window` — eventTarget-ish no-ops (`addEventListener`/`removeEventListener`
//!   /`dispatchEvent`), `window.window === window`, `location.href` reflecting
//!   the current document URL (writes are refused with a status note; JS-driven
//!   navigation is TODO once the shell contract grows a reason to route it),
//! - `document` — `title` get/set (a set stores the title, marks the host
//!   `title_assigned` so the tab emits `FromTab::TitleChanged` and repaints),
//!   `body.textContent` as a read-only snapshot of the parsed body,
//!   no-op listeners,
//! - `console.log/info/warn/error` — tab stderr **and** the per-navigation
//!   capture the tab turns into `FromTab::StatusText`,
//! - `setTimeout`/`setInterval`/`clearTimeout`/`clearInterval`/
//!   `requestAnimationFrame`/`cancelAnimationFrame` — recognised but **not
//!   implemented** (no timer thread): calls are counted, reported once via
//!   StatusText and do nothing (TODO: plumb a timer queue into the poll loop).
//!
//! There is no DOM mutation and no engine-internal DOM binding: the glue below
//! is the whole surface. Every script is evaluated inside a catch — a throwing
//! script is reported (`ScriptError`, turned into `FromTab::StatusText` by the
//! runtime) and the next script still runs.
//!
//! The bindings reach the tab through a thread-local host pointer: blitz's
//! `HtmlDocument` (and therefore script execution) lives on exactly one thread
//! (`docs/BLITZ_API_NOTES.md` §8), so a `thread_local!` stack is enough and
//! keeps every native closure `Copy` (a boa requirement for
//! [`NativeFunction::from_copy_closure`]). Host borrows are never held across
//! code that can re-enter JavaScript.

use std::cell::RefCell;
use std::rc::Rc;

use boa_engine::property::Attribute;
use boa_engine::{Context, JsNativeError, JsString, JsValue, NativeFunction, Source};
use blitz_dom::{local_name, BaseDocument, Node, NodeId};
use url::Url;

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
    /// Human-readable notes raised by the bindings (refused `location` writes,
    /// timer calls) — the runtime shows the newest as `FromTab::StatusText`.
    pub notes: Vec<String>,
    /// A script changed something that needs a fresh frame.
    pub repaint: bool,
    /// Number of no-op timer/rAF calls (reported once, see module docs).
    pub timer_calls: u32,
    /// Read-only snapshot of `document.body.textContent`
    /// (`None` = the document has no `<body>`).
    pub body_text: Option<String>,
    /// URL the bindings report through `location.href`.
    pub url: String,
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

/// Shared handle (the tab keeps ownership, the bindings borrow it while the
/// scripts run on the same thread).
pub type HostRef = Rc<RefCell<HostState>>;

thread_local! {
    /// Stack of hosts whose contexts are currently evaluating. Natives resolve
    /// the top entry; [`run_scripts`] pushes for the duration of the run.
    static ACTIVE_HOSTS: RefCell<Vec<HostRef>> = const { RefCell::new(Vec::new()) };
}

/// Borrow the active host mutably. Never called across a re-entry point (all
/// JS→Rust string conversions happen before the borrow).
fn with_host<R>(f: impl FnOnce(&mut HostState) -> R) -> Option<R> {
    let host = ACTIVE_HOSTS.with(|stack| stack.borrow().last().cloned())?;
    let outcome = f(&mut host.borrow_mut());
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
    pub(super) const REFUSE: &str = "__ironsurfRefuse";
    pub(super) const TIMER: &str = "__ironsurfTimer";
}

/// Trusted glue installed on top of the natives. Written in JS on purpose:
/// argument coercion (`console.log` of objects, `title` stringification) then
/// runs through JavaScript's own semantics, and the Rust side stays four
/// tiny natives.
const PRELUDE: &str = r#"
(function () {
  var host = globalThis;

  function refused(target) {
    host.__ironsurfRefuse(String(target));
  }
  function noop() {}
  function returnTrue() { return true; }

  var location = {
    // Reflector: the document URL. JS-driven navigation is not implemented
    // (TODO: route it through the tab's navigation entry point).
    get href() {
      return host.__ironsurfUrl;
    },
    set href(target) {
      refused(target);
    },
    assign: refused,
    replace: refused,
    reload: function () {
      refused(host.__ironsurfUrl);
    },
    toString: function () {
      return host.__ironsurfUrl;
    },
    get protocol() { return ""; },
    get host() { return ""; },
    get hostname() { return ""; },
    get pathname() { return ""; },
    get search() { return ""; },
    get hash() { return ""; },
  };

  // No timers: recognised, counted, and deliberately inert.
  function timer() {
    host.__ironsurfTimer();
    return 0;
  }

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
  host.addEventListener = noop;
  host.removeEventListener = noop;
  host.dispatchEvent = returnTrue;
  host.setTimeout = timer;
  host.setInterval = timer;
  host.clearTimeout = noop;
  host.clearInterval = noop;
  host.requestAnimationFrame = timer;
  host.cancelAnimationFrame = noop;
  host.scrollTo = noop;
  host.scroll = noop;

  var document = {
    nodeType: 9,
    readyState: "complete",
    visibilityState: "visible",
    hidden: false,
    documentElement: null,
    body: host.__ironsurfHasBody
      ? { nodeType: 1, textContent: host.__ironsurfBodyText }
      : null,
    getElementsByTagName: function () { return []; },
    getElementById: function () { return null; },
    querySelector: function () { return null; },
    querySelectorAll: function () { return [];; },
    addEventListener: function () {},
    removeEventListener: function () {},
    dispatchEvent: function () { return true; },
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
"#;

/// Build a fresh boa context for one navigation, with the tiny binding surface
/// bound to `host`. The context is intentionally thrown away at the next
/// navigation (per `ARCHITECTURE.md` §JavaScript (Boa)).
pub fn build_context(host: &HostRef) -> Context {
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
            eprintln!("ironsurf-tab: js binding {name} failed: {err}");
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
        eprintln!("ironsurf-tab: js binding title getter failed: {err}");
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
        eprintln!("ironsurf-tab: js binding title setter failed: {err}");
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
            eprintln!("ironsurf-tab: js console [{level:?}] {url}: {text}");
            Ok(JsValue::undefined())
        }),
    ) {
        eprintln!("ironsurf-tab: js binding console failed: {err}");
    }
    // location writes: refused, with a status note.
    if let Err(err) = context.register_global_builtin_callable(
        JsString::from(native::REFUSE),
        1,
        NativeFunction::from_copy_closure(|_this, args, _context| {
            let target = args
                .first()
                .cloned()
                .unwrap_or(JsValue::undefined())
                .to_string(_context)?
                .to_std_string_escaped();
            with_host(|host| {
                host.notes
                    .push(format!("refused javascript navigation to {target}"));
            });
            Ok(JsValue::undefined())
        }),
    ) {
        eprintln!("ironsurf-tab: js binding location failed: {err}");
    }
    // Timers: counted and reported once, never scheduled (TODO).
    if let Err(err) = context.register_global_builtin_callable(
        JsString::from(native::TIMER),
        0,
        NativeFunction::from_copy_closure(|_this, _args, _context| {
            with_host(|host| {
                host.timer_calls += 1;
                if host.timer_calls == 1 {
                    host.notes
                        .push("javascript timers are not implemented (TODO)".to_string());
                }
            });
            Ok(JsValue::undefined())
        }),
    ) {
        eprintln!("ironsurf-tab: js binding timers failed: {err}");
    }

    if let Err(err) = context.eval(Source::from_bytes(PRELUDE)) {
        // Our own glue failing is a bug, not page input: report loudly, keep
        // the globals that did install, and let the tab run.
        eprintln!("ironsurf-tab: js prelude failed: {err}");
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

/// Evaluate `scripts` in order in one fresh context owned by `host`.
///
/// Every evaluation is wrapped: an exception is collected and the next script
/// still runs (contract: script errors must never kill the tab).
pub fn run_scripts(scripts: &[ScriptSource], host: &HostRef) -> Vec<ScriptError> {
    ACTIVE_HOSTS.with(|stack| stack.borrow_mut().push(Rc::clone(host)));
    let mut context = build_context(host);
    let mut errors = Vec::new();
    for script in scripts {
        match context.eval(Source::from_bytes(script.code.as_str())) {
            Ok(_) => {}
            Err(err) => {
                // boa's Display adds a shadow-stack backtrace; keep one line.
                let message = err.to_string().lines().next().unwrap_or("script error").to_string();
                eprintln!(
                    "ironsurf-tab: js error in {}: {message}",
                    script.origin
                );
                errors.push(ScriptError {
                    origin: script.origin.clone(),
                    message,
                });
            }
        }
    }
    // Drop the realm before the host so nothing can observe a dead host.
    drop(context);
    ACTIVE_HOSTS.with(|stack| {
        stack.borrow_mut().pop();
    });
    errors
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

    fn eval_snippet(source: &str, host: &HostRef) -> JsValue {
        ACTIVE_HOSTS.with(|stack| stack.borrow_mut().push(Rc::clone(host)));
        let mut context = build_context(host);
        let result = context.eval(Source::from_bytes(source));
        drop(context);
        ACTIVE_HOSTS.with(|stack| {
            stack.borrow_mut().pop();
        });
        result.expect("snippet should evaluate")
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
        // Writes are refused with a note, and do not change the reflection.
        eval_snippet("window.location.assign('https://evil.test/')", &host);
        eval_snippet("window.location.href = 'https://evil.test/'", &host);
        let borrowed = host.borrow();
        assert_eq!(borrowed.url, "https://example.test/page.html");
        assert!(borrowed
            .notes
            .iter()
            .any(|note| note.contains("refused javascript navigation to https://evil.test/")));
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
    fn timers_are_counted_not_scheduled() {
        let host = sample_host();
        eval_snippet("setTimeout(function () {}, 1); setInterval(function () {}, 1);", &host);
        let borrowed = host.borrow();
        assert_eq!(borrowed.timer_calls, 2);
        assert_eq!(
            borrowed
                .notes
                .iter()
                .filter(|note| note.contains("timers are not implemented"))
                .count(),
            1,
            "the note is reported once"
        );
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
        let errors = run_scripts(&scripts, &host);
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
        let errors = run_scripts(&scripts, &host);
        assert_eq!(errors.len(), 1, "syntax errors never abort the tab");
        assert!(!errors[0].message.is_empty());
    }

    #[test]
    fn contexts_are_isolated_between_navigations() {
        let first = sample_host();
        run_scripts(
            &[ScriptSource {
                code: "var leaked = 1; document.title = 'one';".to_string(),
                origin: "inline".to_string(),
            }],
            &first,
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
}
