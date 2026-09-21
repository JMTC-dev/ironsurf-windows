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
//!   capture the tab turns into `FromTab::ConsoleLog` (source `"js"`) plus a
//!   `FromTab::StatusText`,
//! - every thrown DOM native (`__ironsurfDom*`: bad selector, dead node …)
//!   is recorded in [`HostState::dom_throws`] so the tab can report the
//!   escaped exception as `FromTab::ConsoleLog` with source `"dom"` (see
//!   [`ScriptError::source`]).
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
use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use boa_engine::builtins::promise::PromiseState;
use boa_engine::js_string;
use boa_engine::module::{ModuleLoader, ModuleRequest, Referrer};
use boa_engine::object::JsObject;
use boa_engine::property::Attribute;
use boa_engine::{Context, JsNativeError, JsString, JsValue, Module, NativeFunction, Source};
use blitz_dom::node::{ImageData, SpecialElementData};
use blitz_dom::{local_name, BaseDocument, LocalName, Node, NodeData, NodeId, Prefix, QualName};
use blitz_html::HtmlDocument;
use url::Url;

// Page-side diagnostics: `console.*` and script errors go to stderr at the
// level the page itself asked for (see [`log_console`]).
use ironsurf_common::{debug, error, info, trace, warn, DialogKind};

/// Emit one `console.*` line to stderr at the level the page chose.
/// The same line also reaches the shell (status text) via the host capture.
///
/// `userscript` is the name of the userscript the line came from (task:
/// userscripts) — `None` for the page's own scripts — so `grep`ing a tab log
/// always says which program talked.
fn log_console(level: ConsoleLevel, url: &str, text: &str, userscript: Option<&str>) {
    let origin = match userscript {
        Some(name) => format!("userscript {name}"),
        None => "page".to_string(),
    };
    match level {
        ConsoleLevel::Error => error!("{origin} console [{level}] {url}: {text}"),
        ConsoleLevel::Warn => warn!("{origin} console [{level}] {url}: {text}"),
        ConsoleLevel::Info | ConsoleLevel::Log => {
            info!("{origin} console [{level}] {url}: {text}")
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
    /// Where it came from: `"inline"` for classic `<script>` bodies, otherwise
    /// the resolved `src` URL (used in diagnostics and status texts). ES
    /// modules instead carry the **absolute URL they execute as** (their
    /// resolved `src`, or the document URL for inline modules) — that URL is
    /// their `import.meta.url` and the base their imports resolve against.
    pub origin: String,
    /// Classic or module evaluation (task: `<script type="module">` loading).
    pub kind: ScriptKind,
}

/// How a collected [`ScriptSource`] must be evaluated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScriptKind {
    /// A classic `<script>`: evaluated directly in the realm's global scope.
    Classic,
    /// `<script type="module">`: parsed as an ES module and run through the
    /// document's [`UrlModuleLoader`] — static/dynamic imports resolve
    /// through the document's import map, each dependency is fetched once and
    /// instantiated once, shared by every importer.
    Module,
}

/// What [`extract_scripts`] found: executable scripts in document order, plus
/// human-readable notes about everything it deliberately skipped.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Extraction {
    pub scripts: Vec<ScriptSource>,
    pub notes: Vec<String>,
    /// The document's import map (first valid `<script type="importmap">`),
    /// parsed and validated during the walk — never executed as a script.
    pub import_map: Option<ImportMap>,
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
/// attribute" (a classic script); ES modules are handled separately, as
/// [`ScriptKind::Module`] (task: `<script type="module">` loading).
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

/// ES-module `type` (`<script type="module">`): executed as an ES module
/// through the document's [`UrlModuleLoader`] (task: module loading), not as
/// a classic script.
fn is_module_type(type_attr: Option<&str>) -> bool {
    type_attr
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .is_some_and(|t| t.eq_ignore_ascii_case("module"))
}

/// The import-map `type` (`<script type="importmap">`): a JSON declarative
/// module specifier map. Never executed as code — the body is *parsed*, and a
/// valid one becomes [`Extraction::import_map`] for the runtime.
fn is_importmap_type(type_attr: Option<&str>) -> bool {
    type_attr
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .is_some_and(|t| t.eq_ignore_ascii_case("importmap"))
}

/// A parsed `<script type="importmap">`: the document's declarative module
/// specifier map (WHATWG import-maps). Stored verbatim enough to run the
/// spec's resolution algorithm: exact keys, `/`-terminated prefix keys
/// (`"lib/"` rewrites every deeper specifier), `null` values that *unmap*
/// (resolution failure), and `scopes` overriding `imports` for specifiers
/// requested from inside a URL prefix.
///
/// The JSON walk is as forgiving as the spec's parse algorithm: unknown
/// members are ignored and individually malformed entries are dropped, while
/// invalid JSON (or non-object top-level `imports`/`scopes`) invalidates the
/// whole map — the HTML spec discards the script instead of half-applying it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ImportMap {
    /// `imports` entries, `(key, address)` in document order; `None` address
    /// is a spec `null` (unmap the key / prefix).
    imports: Vec<(String, Option<String>)>,
    /// `scopes` entries: `(scope URL prefix, entries)` in document order.
    scopes: Vec<(String, Vec<(String, Option<String>)>)>,
}

impl ImportMap {
    /// Parse an import-map JSON body. `Ok` only for a spec-valid map; the
    /// `Err` string is the human-readable reason it was thrown out.
    pub fn parse(json: &str) -> Result<Self, String> {
        let value: serde_json::Value =
            serde_json::from_str(json).map_err(|err| err.to_string())?;
        let top = value
            .as_object()
            .ok_or_else(|| "the import map is not a JSON object".to_string())?;
        let mut map = ImportMap::default();
        match top.get("imports") {
            None => {}
            Some(serde_json::Value::Object(entries)) => {
                map.imports = parse_entries(entries);
            }
            Some(_) => return Err("\"imports\" is present but not an object".to_string()),
        }
        match top.get("scopes") {
            None => {}
            Some(serde_json::Value::Object(scopes)) => {
                for (prefix, entries) in scopes {
                    // A scope whose value is not an object is dropped whole.
                    if let Some(entries) = entries.as_object() {
                        let entries = parse_entries(entries);
                        if !entries.is_empty() {
                            map.scopes.push((prefix.clone(), entries));
                        }
                    }
                }
                // Stable document order despite serde_json's map iteration.
                map.scopes.sort_by(|a, b| a.0.cmp(&b.0));
            }
            Some(_) => return Err("\"scopes\" is present but not an object".to_string()),
        }
        Ok(map)
    }

    /// Number of `imports` entries (diagnostics; excludes `scopes`).
    pub fn len(&self) -> usize {
        self.imports.len()
    }

    /// Number of `scopes` entries (diagnostics).
    pub fn scopes_len(&self) -> usize {
        self.scopes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.imports.is_empty() && self.scopes.is_empty()
    }

    /// Resolve `specifier` (a bare specifier, relative path, or absolute URL)
    /// against the document/module `base`. `None` = unresolvable: a bare
    /// specifier no entry covers, an explicit `null` unmapping, or a mapped
    /// address that is itself not a URL.
    pub fn resolve(&self, specifier: &str, base: &Url) -> Option<Url> {
        self.resolve_for(specifier, base, None)
    }

    /// [`resolve`](Self::resolve) with a requesting module's URL, which lets
    /// `scopes` shadow `imports` for requests from inside a scope prefix.
    pub fn resolve_for(
        &self,
        specifier: &str,
        base: &Url,
        referrer: Option<&Url>,
    ) -> Option<Url> {
        // The spec splits specifiers *before* parsing: scheme-absolute URLs
        // and relative-URL references (`/`, `//`, `./`, `../`, `#` starts) are
        // resolved and matched on their absolute form; everything else is a
        // *bare* specifier, matched raw and unresolvable without a map hit
        // (`Url::join` alone cannot tell them apart — it happily resolves
        // `"react"` against any http base).
        let is_relative_url = specifier.starts_with('/')
            || specifier.starts_with("./")
            || specifier.starts_with("../")
            || specifier.starts_with('#')
            || Url::parse(specifier).is_ok();
        let resolved = if is_relative_url { base.join(specifier).ok() } else { None };
        let key = resolved.as_ref().map(Url::as_str).unwrap_or(specifier);

        // Scopes: innermost (longest matching prefix the referrer lives in)
        // wins, then document order on ties.
        if let Some(referrer) = referrer {
            let mut best: Option<(usize, &[(String, Option<String>)])> = None;
            for (prefix, entries) in &self.scopes {
                let Ok(scope) = base.join(prefix) else { continue };
                let scope = scope.as_str();
                if referrer.as_str().starts_with(scope)
                    && best.is_none_or(|(len, _)| scope.len() > len)
                {
                    best = Some((scope.len(), entries.as_slice()));
                }
            }
            if let Some((_, entries)) = best {
                if let Some(entry) = match_entries(entries, key) {
                    return mapped_address(entry, key)
                        .and_then(|address| base.join(&address).ok());
                }
            }
        }

        if let Some(entry) = match_entries(&self.imports, key) {
            // `null` (unmapping) and mapped addresses that are not URLs both
            // fail resolution per spec.
            return mapped_address(entry, key)
                .and_then(|address| base.join(&address).ok());
        }
        // Unmatched URL-ish specifier: pass through; unmatched bare fails.
        resolved
    }
}

/// Flat `(key, address)` entries of one `imports` object: string addresses
/// and `null` (unmap) are kept, every other JSON shape is silently dropped.
fn parse_entries(
    entries: &serde_json::Map<String, serde_json::Value>,
) -> Vec<(String, Option<String>)> {
    let mut out: Vec<_> = entries
        .iter()
        .filter_map(|(key, value)| match value {
            serde_json::Value::String(address) => Some((key.clone(), Some(address.clone()))),
            serde_json::Value::Null => Some((key.clone(), None)),
            _ => None,
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// The address a matched entry yields for `key`: verbatim for an exact key,
/// and the matched prefix **replaced** — key `"lib/"` → address `"/v/lib/"`
/// turns `"lib/util.js"` into `"/v/lib/util.js"`. A `null` address fails
/// resolution (unmapping).
fn mapped_address(entry: &(String, Option<String>), key: &str) -> Option<String> {
    let address = entry.1.as_ref()?;
    let map_key = &entry.0;
    if map_key != key && (map_key.ends_with('/') || map_key.ends_with('\\')) {
        let mut out = address.clone();
        out.push_str(&key[map_key.len()..]);
        Some(out)
    } else {
        Some(address.clone())
    }
}

/// The spec's *match* over one flat entry list: an exact key hit wins;
/// otherwise the **longest** `/`-terminated key that is a prefix of `key`
/// (a `/`-terminated specifier may also complete an unterminated key, e.g.
/// `"react/"` completing `"react"` — the only way a trailing-slash specifier
/// can reach a bare key).
fn match_entries<'a>(
    entries: &'a [(String, Option<String>)],
    key: &str,
) -> Option<&'a (String, Option<String>)> {
    let mut best: Option<(usize, &(String, Option<String>))> = None;
    for entry @ (map_key, _) in entries {
        let hit = if map_key == key {
            // Exact match always wins outright.
            return Some(entry);
        } else if map_key.ends_with('/') || map_key.ends_with('\\') {
            key.starts_with(map_key.as_str())
        } else if key.ends_with('/') {
            key == format!("{map_key}/")
        } else {
            false
        };
        if hit && best.is_none_or(|(len, _)| map_key.len() > len) {
            best = Some((map_key.len(), entry));
        }
    }
    best.map(|(_, entry)| entry)
}

/// One script discovered by the document walk, before its body is known.
///
/// The walk ([`walk_scripts`]) resolves `src=` URLs and applies the type
/// filters, but defers the network. Entries keep their **document order**, and
/// skips already decided during the walk become [`PendingScript::Note`] so the
/// assembly step can interleave them with fetch-failure notes exactly where
/// the sequential fetcher would have produced them.
enum PendingScript {
    /// Inline `<script>` body, ready as-is.
    Inline { code: String },
    /// A resolved `src=` script awaiting its body. `raw` is the attribute
    /// value, kept verbatim for the failure note.
    External { url: Url, raw: String },
    /// Inline `<script type="module">` body. `document_url` is the document's
    /// own URL: the module's identity (`import.meta.url`) and the base its
    /// relative imports resolve against.
    ModuleInline { code: String, document_url: String },
    /// A resolved `src=` module script awaiting its body (fetched exactly
    /// like a classic `src` — same pool, same DevTools reporting).
    ModuleExternal { url: Url, raw: String },
    /// The JSON body of a `<script type="importmap">`, awaiting validation.
    ImportMap { json: String },
    /// A script the walk deliberately skipped (unknown type, unresolvable
    /// `src`) with the note it contributes.
    Note(String),
}

/// The shared `<script>` element walk: document order, `type` filtering,
/// `src` resolution. Both [`extract_scripts`] (sequential) and
/// [`extract_scripts_batch`] (two-phase) go through here, so the rules — and
/// their notes — can never diverge between the two variants.
fn walk_scripts(doc: &BaseDocument, base: &Url) -> Vec<PendingScript> {
    let ids = match doc.query_selector_all("script") {
        Ok(ids) => ids.to_vec(),
        Err(_) => {
            let mut all = Vec::new();
            walk_preorder(doc, doc.root_node().id, &mut all);
            all.into_iter().filter(|id| is_script(doc.get_node(*id).expect("visited node"))).collect()
        }
    };

    let mut pending = Vec::new();
    for id in ids {
        let Some(node) = doc.get_node(id) else { continue };
        let Some(element) = node.element_data() else { continue };
        let type_attr = element.attr(local_name!("type"));
        let src = element
            .attr(local_name!("src"))
            .map(str::trim)
            .filter(|src| !src.is_empty())
            .map(str::to_owned);

        if is_importmap_type(type_attr) {
            // Import maps are declarative JSON, inline only (a `src=` one is
            // a spec-invalid map and never fetched).
            match src {
                None => pending.push(PendingScript::ImportMap {
                    json: node.text_content(),
                }),
                Some(raw) => pending.push(PendingScript::Note(format!(
                    "importmap with src {raw:?} ignored (import maps must be inline)"
                ))),
            }
            continue;
        }

        if is_module_type(type_attr) {
            // Task: `<script type="module">` loading. The body (inline, or
            // fetched from `src` like any external script) is executed as an
            // ES module; its imports resolve later through the document's
            // import map + loader (`UrlModuleLoader`), never here.
            match src {
                Some(raw) => match base.join(&raw) {
                    Ok(url) => pending.push(PendingScript::ModuleExternal { url, raw }),
                    Err(err) => pending.push(PendingScript::Note(format!(
                        "module script src {raw:?} could not be resolved: {err}"
                    ))),
                },
                None => pending.push(PendingScript::ModuleInline {
                    code: node.text_content(),
                    document_url: base.as_str().to_string(),
                }),
            }
            continue;
        }
        if !is_classic_script_type(type_attr) {
            // JSON blobs, `<template>`-style data, unknown languages.
            pending.push(PendingScript::Note(format!(
                "skipped script with unsupported type {:?}",
                type_attr.unwrap_or_default().trim()
            )));
            continue;
        }

        match src {
            // External: resolved now (so unresolvable `src` notes keep their
            // document position), fetched by the caller's pass.
            Some(raw) => match base.join(&raw) {
                Ok(url) => pending.push(PendingScript::External { url, raw }),
                Err(err) => pending.push(PendingScript::Note(format!(
                    "script src {raw:?} could not be resolved: {err}"
                ))),
            },
            // Inline: the parser keeps `<script>` bodies as raw text nodes.
            None => pending.push(PendingScript::Inline {
                code: node.text_content(),
            }),
        }
    }

    pending
}

/// The shared assembly step: turn the walk's pending list plus the fetch
/// results (positionally matched to the [`PendingScript::External`] entries,
/// in document order) into the final [`Extraction`]. Empty bodies are
/// skipped; failed fetches note in their **original relative order** because
/// the note is emitted at the pending item's own position, not at fetch time.
fn assemble_scripts(pending: Vec<PendingScript>, results: Vec<Result<String, String>>) -> Extraction {
    // A misbehaving fetcher never aborts the page: missing results read as
    // fetch failures, extras are ignored.
    let external_count = pending
        .iter()
        .filter(|item| {
            matches!(
                item,
                PendingScript::External { .. } | PendingScript::ModuleExternal { .. }
            )
        })
        .count();
    let mut results = results;
    results.truncate(external_count);
    results.extend(
        (results.len()..external_count).map(|_| Err("script fetcher returned no result".to_string())),
    );

    let mut extraction = Extraction::default();
    let mut next = 0usize;
    for item in pending {
        match item {
            PendingScript::Note(note) => extraction.notes.push(note),
            PendingScript::ImportMap { json } => {
                // One map per document (HTML spec): the first that parses
                // wins; the rest are ignored with a note, never merged.
                if extraction.import_map.is_some() {
                    extraction.notes.push(
                        "skipped additional importmap (the document already has one)".to_string(),
                    );
                } else {
                    match ImportMap::parse(&json) {
                        Ok(map) => extraction.import_map = Some(map),
                        Err(err) => extraction
                            .notes
                            .push(format!("importmap ignored: invalid import map — {err}")),
                    }
                }
            }
            PendingScript::Inline { code } => {
                if !code.trim().is_empty() {
                    extraction.scripts.push(ScriptSource {
                        code,
                        origin: "inline".to_string(),
                        kind: ScriptKind::Classic,
                    });
                }
            }
            PendingScript::ModuleInline { code, document_url } => {
                if !code.trim().is_empty() {
                    extraction.scripts.push(ScriptSource {
                        code,
                        origin: document_url,
                        kind: ScriptKind::Module,
                    });
                }
            }
            PendingScript::ModuleExternal { url, raw } => {
                let index = next;
                next += 1;
                match &results[index] {
                    Ok(body) => extraction.scripts.push(ScriptSource {
                        code: body.clone(),
                        origin: url.as_str().to_string(),
                        kind: ScriptKind::Module,
                    }),
                    Err(err) => extraction
                        .notes
                        .push(format!("module script {raw:?} failed: {err}")),
                }
            }
            PendingScript::External { url, raw } => {
                let index = next;
                next += 1;
                match &results[index] {
                    Ok(body) => extraction.scripts.push(ScriptSource {
                        code: body.clone(),
                        origin: url.as_str().to_string(),
                        kind: ScriptKind::Classic,
                    }),
                    Err(err) => extraction
                        .notes
                        .push(format!("script {raw:?} failed: {err}")),
                }
            }
        }
    }

    extraction
}

/// Collect the `<script>` elements of `doc` **in document order**.
///
/// Inline scripts contribute their text content; `src=` scripts are resolved
/// against `base` and fetched through `fetch` (the runtime passes its own
/// document fetcher, so the tab's net policy applies to scripts too). A script
/// that cannot be fetched, or whose `type` we do not execute, is recorded in
/// [`Extraction::notes`] and skipped — it must never abort the page.
///
/// This is the sequential variant: one `fetch` per external script, inline on
/// the caller's thread. The live tab uses [`extract_scripts_batch`], which
/// defers every `src` body to a single batch callback so the runtime can
/// download them concurrently; both share the exact same walk and assembly.
#[allow(dead_code)] // compat shape: the live tab batches (task D), the unit tests drive this one
pub fn extract_scripts(
    doc: &BaseDocument,
    base: &Url,
    fetch: &mut dyn FnMut(&Url) -> Result<String, String>,
) -> Extraction {
    let pending = walk_scripts(doc, base);
    let mut results = Vec::new();
    for item in &pending {
        match item {
            PendingScript::External { url, .. } | PendingScript::ModuleExternal { url, .. } => {
                results.push(fetch(url));
            }
            _ => {}
        }
    }
    assemble_scripts(pending, results)
}

/// [`extract_scripts`] in two phases: walk the document first (cheap, no I/O),
/// then fetch **all** collected `src` URLs with a single `fetch_many` call
/// (document order in, positionally matched results out), then assemble.
/// The runtime implements `fetch_many` with a capped pool of fetcher threads
/// (never one thread per URL — unbounded fan-out is EAGAIN on script-heavy
/// pages), so a page's external scripts download concurrently while their
/// execution order stays document order.
///
/// `fetch_many` is called exactly once whenever the page has at least one
/// fetchable `src` (never for a page with none — e.g. inline-only). A result
/// list shorter than the URL list reads as fetch failures.
pub fn extract_scripts_batch(
    doc: &BaseDocument,
    base: &Url,
    fetch_many: &mut dyn FnMut(&[&Url]) -> Vec<Result<String, String>>,
) -> Extraction {
    let pending = walk_scripts(doc, base);
    let urls: Vec<&Url> = pending
        .iter()
        .filter_map(|item| match item {
            PendingScript::External { url, .. } | PendingScript::ModuleExternal { url, .. } => {
                Some(url)
            }
            _ => None,
        })
        .collect();
    let results = if urls.is_empty() {
        Vec::new()
    } else {
        fetch_many(&urls)
    };
    assemble_scripts(pending, results)
}

// ---------------------------------------------------------------------------
// ES-module loading (task: `<script type="module">`)

/// The fetcher the module graph runs through: absolute URL in, module body
/// out. The tab hands in its own `<script src>` fetcher, so lazily-imported
/// dependencies ride the same cookie jar, cache, DevTools reporting and
/// archive sink as the rest of the page.
pub type ModuleFetch = Rc<dyn Fn(&Url) -> Result<String, String>>;

/// The tab's `XMLHttpRequest` handle (task: infosec.exchange posts): one
/// blocking request of any method with the page's own request headers and an
/// optional string body, answered with status/headers/body — the tab's own
/// jar, UA cell, and DevTools queue behind it
/// ([`crate::netmod::xhr_fetch_report`]). Handed to the JS host the same way
/// [`ModuleFetch`] is; the bindings call it from inside `send()` and deliver
/// the result through the spec's event sequence.
pub struct XhrFetch {
    call: Rc<
        dyn Fn(
            &str,                                // method
            &str,                                // absolute URL
            &[(String, String)],                 // request headers
            Option<&str>,                        // string body (none = no body)
        ) -> Result<crate::netmod::XhrReply, String>,
    >,
}

// `Clone` clones the `Rc` (one closure, many handles — the same cheap share
// an `Rc<dyn Fn>` alias would give); a hand-written `Debug` because the
// debug-printed `HostState` holds one next to the other shared cells and a
// `dyn Fn` has no `Debug` of its own.
impl Clone for XhrFetch {
    fn clone(&self) -> Self {
        Self {
            call: Rc::clone(&self.call),
        }
    }
}

impl std::fmt::Debug for XhrFetch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("XhrFetch(<tab network closure>)")
    }
}

impl XhrFetch {
    #[must_use]
    pub fn new(call: Rc<dyn Fn(&str, &str, &[(String, String)], Option<&str>) -> Result<crate::netmod::XhrReply, String>>) -> Self {
        Self { call }
    }
}

impl std::ops::Deref for XhrFetch {
    type Target = dyn Fn(
        &str,
        &str,
        &[(String, String)],
        Option<&str>,
    ) -> Result<crate::netmod::XhrReply, String>;
    fn deref(&self) -> &Self::Target {
        &*self.call
    }
}

/// The document's ES-module loader (task: `<script type="module">` loading):
/// resolves every specifier through the document's [`ImportMap`] when it has
/// one (read at resolution time — the map is installed before the first
/// module runs), fetches bodies through the tab's own network layer, and
/// parses them with Boa.
///
/// Modules are keyed by their **absolute URL string**, so every importer of
/// the same specifier gets the same `Module` instance — the spec's dedup
/// requirement, and also what makes cycles work: [`register`](Self::register)
/// files an entry module before its own imports load.
///
/// Boa 0.22 drives loading through job-queue futures; this loader fetches
/// synchronously (the tab's blocking client, like every other tab fetch) and
/// hands out an already-ready future, so a whole import graph settles within
/// one job-queue drain.
/// Forensic debug tool — inert unless `IRONSURF_CHUNK_DEBUG` is set in the
/// environment. A handful of exact-match rewrites that make vendored library
/// code report internal state through `console.log`. The needles are pinned to
/// one minified build (react-redux v9 inside `detect-it.esm-*.js`): the
/// `useSelector` body reads `{store, subscription}` off the context value and
/// the app's abort site was a null member there; these probes log the context
/// object's shape from both the Provider and the subscriber side so a null can
/// be attributed. Identity transform for every other chunk.
fn debug_chunk_transform(body: String) -> String {
    if std::env::var("IRONSURF_CHUNK_DEBUG").is_err() {
        return body;
    }
    let mut out = body;
    const SUB_HOOK: (&str, &str) = (
        "=t();A.useRef(!0);let s=A.useCallback",
        "=t();try{console.log(\"[HOOK] keys=\"+Object.keys(t()).join(\",\")+\" sub=\"+typeof a+\" store=\"+typeof i+\" gs=\"+typeof o+\" @\"+(new Error(\"h\")).stack.replace(/\\n/g,\"|\"))}catch(he){console.log(\"[HOOKF] \"+he)}A.useRef(!0);let s=A.useCallback",
    );
    const PROV_HOOK: (&str, &str) = (
        "let s=n||Hr;return A.createElement(s.Provider,{value:a},t)",
        "let s=n||Hr;try{console.log(\"[PROV] ctxkeys=\"+(s?Object.keys(s).join(\",\"):\"?\")+\" store=\"+(a?typeof a.store:\"?\")+\" sub=\"+(a?typeof a.subscription:\"?\")+\" valkeys=\"+(a?Object.keys(a).join(\",\"):\"?\"))}catch(he){console.log(\"[PROVF] \"+he)}return A.createElement(s.Provider,{value:a},t)",
    );
    const P_ENTRY: (&str, &str) = (
        "P=({href:e,text:t,prevText:n,hashtagAccountId:r,mention:i,collectionId:a,className:o,children:s,...c})=>{let l=_(",
        "P=({href:e,text:t,prevText:n,hashtagAccountId:r,mention:i,collectionId:a,className:o,children:s,...c})=>{console.log(\"[PENTRY] href=\"+String(e)+\" text=\"+String(t).slice(0,40)+\" stack=\"+String((new Error(\"pe\")).stack).replace(/\\n/g,\"|\").slice(0,900));let l=_(",
    );
    for (needle, repl) in [SUB_HOOK, PROV_HOOK, P_ENTRY] {
        if out.contains(needle) {
            out = out.replace(needle, repl);
            eprintln!("[chunk-debug] instrumented one site");
        }
    }
    out
}

pub struct UrlModuleLoader {
    /// The document URL: referrer for loads with no module/script path of
    /// their own — dynamic `import()` from a classic script or a `javascript:`
    /// URL resolves against it, like a referrer-less HTML base.
    base: Url,
    /// The document's import map, installed by [`JsRuntime::set_import_map`].
    import_map: RefCell<Option<ImportMap>>,
    /// The tab's fetcher (jar, cache, net events, archive — locked per
    /// operation, never across another module's fetch).
    fetch: ModuleFetch,
    /// Absolute URL string → parsed module: entries and dependencies alike.
    modules: RefCell<HashMap<String, Module>>,
}

impl UrlModuleLoader {
    /// A loader for one document: `base` is the document URL, `fetch` the
    /// tab's network handle for module bodies.
    pub fn new(base: Url, fetch: ModuleFetch) -> Self {
        UrlModuleLoader {
            base,
            import_map: RefCell::new(None),
            fetch,
            modules: RefCell::new(HashMap::new()),
        }
    }

    /// Install/replace the document's import map (module resolution reads it
    /// per lookup, never caches a decision across maps).
    pub fn set_import_map(&self, import_map: Option<ImportMap>) {
        *self.import_map.borrow_mut() = import_map;
    }

    /// File a module parsed outside the loader — an inline entry module — so
    /// the loader hands its own URL back as this very instance.
    pub fn register(&self, url: &Url, module: &Module) {
        self.modules
            .borrow_mut()
            .insert(url.as_str().to_string(), module.clone());
    }

    /// The module count resolved so far (diagnostics and tests).
    pub fn len(&self) -> usize {
        self.modules.borrow().len()
    }

    pub fn is_empty(&self) -> bool {
        self.modules.borrow().is_empty()
    }

    /// The absolute URL a referrer lives at: the module path is the URL
    /// string this host gave it ([`Module::parse`] was called with
    /// `PathBuf::from(url.as_str())`), so it round-trips. A realm/script
    /// referrer has no path — the document URL answers for it.
    fn referrer_url(referrer: &Referrer) -> Option<Url> {
        referrer
            .path()
            .and_then(|path| Url::parse(&path.to_string_lossy()).ok())
    }

    /// Resolve `specifier` requested by `referrer` (already an absolute URL).
    /// With an import map its rules run first — exact/prefix keys, `null`
    /// unmappings, `scopes` keyed on the referrer; without one (or for a
    /// specifier the map leaves alone), the default applies: a *relative-URL*
    /// specifier joins the referrer, a **bare** specifier fails, exactly like
    /// a browser with no map (`ImportMap`'s empty-map path implements that
    /// split — the bare-vs-relative discrimination is the map's own).
    pub fn resolve_specifier(&self, specifier: &str, referrer: &Url) -> Result<Url, String> {
        let import_map = self.import_map.borrow();
        let resolved = match import_map.as_ref() {
            Some(map) => map.resolve_for(specifier, referrer, Some(referrer)),
            None => ImportMap::default().resolve_for(specifier, referrer, Some(referrer)),
        };
        resolved.ok_or_else(|| {
            format!("Failed to resolve module specifier \"{specifier}\" relative to {referrer}")
        })
    }
}

impl ModuleLoader for UrlModuleLoader {
    fn load_imported_module(
        self: Rc<Self>,
        referrer: Referrer,
        request: ModuleRequest,
        context: &RefCell<&mut Context>,
    ) -> impl Future<Output = Result<Module, boa_engine::JsError>> {
        // Everything below is synchronous: the fetch is the tab's blocking
        // request and parsing needs the live context — the "future" is born
        // complete, and boa runs the whole graph in its job-queue drain.
        let specifier = request.specifier().to_std_string_escaped();
        let referrer = Self::referrer_url(&referrer).unwrap_or_else(|| self.base.clone());
        let resolved = match self.resolve_specifier(&specifier, &referrer) {
            Ok(url) => url,
            Err(err) => return std::future::ready(Err(JsNativeError::typ().with_message(err).into())),
        };
        let key = resolved.as_str().to_string();
        if let Some(module) = self.modules.borrow().get(&key) {
            // Same specifier, same instance: fetched once, one namespace,
            // cycles included.
            return std::future::ready(Ok(module.clone()));
        }
        // The context is borrowed for parsing only, never across the fetch.
        let body = match (self.fetch)(&resolved) {
            Ok(body) => body,
            Err(err) => {
                let message = format!("error fetching module {resolved}: {err}");
                return std::future::ready(Err(JsNativeError::typ().with_message(message).into()));
            }
        };
        let body = debug_chunk_transform(body);
        let path = PathBuf::from(resolved.as_str());
        let mut context = context.borrow_mut();
        let parsed = Module::parse(
            Source::from_bytes(body.as_bytes()).with_path(&path),
            None,
            &mut context,
        );
        drop(context);
        let module = match parsed {
            Ok(module) => module,
            Err(err) => return std::future::ready(Err(err)),
        };
        self.modules.borrow_mut().insert(key, module.clone());
        std::future::ready(Ok(module))
    }

    fn init_import_meta(self: Rc<Self>, import_meta: &JsObject, module: &Module, context: &mut Context) {
        if let Some(path) = module.path() {
            let url = path.to_string_lossy().to_string();
            // `import.meta.url` is the member pages actually use; this host
            // has no other import.meta properties to report.
            let _ = import_meta.set(js_string!("url"), JsValue::from(js_string!(url)), false, context);
        }
    }
}

/// The human-readable message of a module-evaluation rejection: a `message`
/// property when the value carries one, the stringified value otherwise.
fn rejection_message(context: &mut Context, value: &JsValue) -> String {
    if let Some(object) = value.as_object() {
        // A thrown Error carries a Boa backtrace in `stack` — log it, it is
        // the only way to locate the throwing statement in a minified bundle.
        if let Ok(stack) = object.get(js_string!("stack"), context) {
            if let Some(text) = stack.as_string() {
                error!("module rejection stack:\n{}", text.to_std_string_escaped());
            }
        }
        if let Ok(message) = object.get(js_string!("message"), context) {
            if !message.is_undefined() {
                if let Ok(text) = message.to_string(context) {
                    return text.to_std_string_escaped();
                }
            }
        }
    }
    value
        .to_string(context)
        .map(|text| text.to_std_string_escaped())
        .unwrap_or_else(|_| "module evaluation rejected".to_string())
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

/// One captured `console.*` line: the level the caller chose, the already
/// stringified message, and — task: userscripts — **which userscript** produced
/// it (`None` = the page's own scripts). The name rides on the line itself (not
/// on a shared "who is running now" lookup at drain time) because a userscript
/// turn and a page turn are drained separately and each entry must be
/// attributable to its author after the fact.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConsoleLine {
    pub level: ConsoleLevel,
    pub text: String,
    /// Name of the userscript that made the call (`None` for page scripts).
    pub userscript: Option<String>,
}

impl ConsoleLine {
    /// Message as the DevTools console shows it: userscript lines lead with
    /// `"{name}: "` so the culprit is obvious without looking at the source
    /// column; page lines are verbatim.
    pub fn display_message(&self) -> String {
        match &self.userscript {
            Some(name) => format!("{name}: {}", self.text),
            None => self.text.clone(),
        }
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
    /// Captured `console.*` output (level + already stringified message +
    /// originating userscript name, `None` for page scripts).
    pub console: Vec<ConsoleLine>,
    /// Name of the **userscript currently being evaluated** (task: userscripts),
    /// set by [`JsRuntime::run_userscript`] for the duration of one script and
    /// `None` the moment it returns — so `console.*` calls (and the tab's
    /// diagnostics) know their author while the script runs, and page scripts,
    /// timer callbacks and `javascript:` URLs never inherit it. Single-threaded
    /// by construction: one field, one script at a time, no global race.
    pub userscript: Option<String>,
    /// Messages of JS exceptions thrown **by the native DOM bindings**
    /// (`__ironsurfDom*`) while these scripts ran — recorded by the
    /// `native!` registration macro on every native `Err`. The runtime uses
    /// this to classify a script error as DOM-originated
    /// ([`ScriptError::source`] == `"dom"`) instead of plain `"js"`. The
    /// generic script-error path reports the throw exactly once (with the
    /// native's DOM-prefixed message, e.g. `"NotFoundError: parent is gone"`),
    /// so there is no double-report; a page that `catch`es a DOM throw and
    /// rethrows it verbatim still reads as `"dom"` (the message match).
    pub dom_throws: Vec<String>,
    /// Human-readable notes raised by the bindings (timer calls) — the runtime
    /// shows the newest as `FromTab::StatusText`.
    pub notes: Vec<String>,
    /// A page-driven navigation (`location.href=`/`assign`/`replace`/`reload`)
    /// waiting for the tab: `href=`/`assign` push history, `replace`/`reload`
    /// replace the current entry. Taken by the tab after every JS turn
    /// ([`JsRuntime::take_pending_navigation`]); the last write of a turn wins.
    pub pending_nav: Option<PendingNav>,
    /// `history.pushState`/`replaceState` asked for a **URL-only** update
    /// (task: infosec.exchange): the address bar follows, nothing is
    /// fetched. Taken by the tab after every JS turn
    /// ([`JsRuntime::take_pending_url`]); the tab resolves the target
    /// against the document URL and enforces the same-origin rule itself.
    pub pending_url: Option<String>,
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
    /// `window.alert`/`confirm`/`prompt` host (task: JS dialogs): `Some` in a
    /// live IPC tab (the native blocks there until the shell answers — see
    /// [`crate::runtime::DialogHost`]); `None` (bare JS tests, the headless
    /// harness) makes `confirm` read `false` and `prompt` read `null`.
    pub dialog: Option<Arc<crate::runtime::DialogHost>>,
    /// The tab's **effective User-Agent** cell (task: per-container UA):
    /// `navigator.userAgent` reads it live. Like the dialog host above, the
    /// tab hands in its own handle — the very same `Arc` the fetchers use —
    /// so a mid-session `SetUserAgent` reaches scripts with no reload and no
    /// new context. `None` (bare JS tests) reports [`crate::netmod::USER_AGENT`].
    pub user_agent: Option<crate::netmod::SharedUserAgent>,
    /// The container's `localStorage` store cell (task: per-origin DOM
    /// storage). `window.localStorage` reads and writes through it, and
    /// every operation is keyed by the document **origin** re-resolved from
    /// [`HostState::url`] at call time — one origin can never touch
    /// another's area, and documents without a durable origin (`about:`,
    /// `data:`, `file:`, `javascript:`…) get a `SecurityError` on every
    /// access. `None` behaves exactly like an opaque origin: the surface
    /// exists but every access is denied (bare JS tests without a store).
    pub local_storage: Option<SharedLocalStorage>,
    /// The tab's **XHR network handle** (task: infosec.exchange posts):
    /// `XMLHttpRequest.send` runs the request through it — the tab's own
    /// jar/cache-free reporting path — so page XHRs ride the same client,
    /// User-Agent cell, and cookie jar as every other tab fetch. `None`
    /// (bare JS tests, no tab attached) makes every `send()` complete with
    /// a network error, exactly like a browser whose network is dead.
    pub xhr: Option<XhrFetch>,
    /// The container's **cookie jar** cell (task: `document.cookie`).
    /// The very `Arc` the tab's fetchers write `Set-Cookie` into, handed to
    /// the bindings so `document.cookie` reads and writes the live jar:
    /// reads answer the script-visible view for the CURRENT document URL
    /// (jar matching, HttpOnly hidden), writes ride the RFC6265 parser on
    /// the Rust side. `None` (bare JS tests, the headless harness) behaves
    /// like a cookieless document: reads return `""`, writes are ignored.
    pub cookies: Option<SharedCookieJar>,
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

/// The tab's per-container DOM-storage handle (task: per-origin
/// `localStorage`): the very store built from the container profile dir
/// ([`ironsurf_storage::ContainerStore::local_storage`]). Handed to the JS
/// host the same way the UA cell is — the bindings only ever reach an area
/// through an origin re-resolved from the CURRENT document URL, so the
/// per-origin security rule is enforced on the Rust side of the native.
pub type SharedLocalStorage = Arc<std::sync::Mutex<ironsurf_storage::LocalStorage>>;

/// The tab's container cookie jar (task: `document.cookie`): the SAME
/// `Arc<Mutex<CookieJar>>` the network layer captures `Set-Cookie` into
/// ([`crate::netmod`]/the BlitzNet provider own it), borrowed by the JS
/// bindings so scripts see — and extend — exactly the cookies the wire
/// rides on. All matching rules live in `ironsurf_storage::CookieJar`;
/// the bindings only ever re-resolve the document URL at call time.
pub type SharedCookieJar = Arc<std::sync::Mutex<ironsurf_storage::CookieJar>>;

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

/// `document.createElementNS` element names: same validation as `createElement`
/// but the namespace string selects the element's `Namespace` (svg/mathml/
/// xhtml/html; anything else falls back to HTML like browsers do for unknown
/// namespaces created from HTML documents).
fn qual_name_for_ns(tag: &str, ns: &str) -> Result<QualName, &'static str> {
    let lowered = tag.to_ascii_lowercase();
    if lowered.is_empty() {
        return Err("empty tag name");
    }
    if !is_valid_dom_name(&lowered) {
        return Err("invalid tag name");
    }
    let namespace = match ns.to_ascii_lowercase().as_str() {
        "http://www.w3.org/2000/svg" => blitz_dom::ns!(svg),
        "http://www.w3.org/1998/Math/MathML" => blitz_dom::ns!(mathml),
        "http://www.w3.org/1999/xhtml" => blitz_dom::ns!(html),
        _ => blitz_dom::ns!(html),
    };
    Ok(QualName::new(None, namespace, LocalName::from(lowered.as_str())))
}

/// `setAttribute`/`removeAttribute` name rules: like the tag rules but also
/// rejecting `xmlns` redefinitions and prefix shenanigans (simplified).
fn qual_name_for_attr(name: &str) -> Result<QualName, String> {
    // Note: plain `xmlns` IS settable through the DOM API (react-dom does it
    // for every SVG root); only *prefixed* xmlns redefinition (`xmlns:foo`)
    // is a namespace error.
    if name.is_empty() || !is_valid_dom_name(name) {
        return Err(format!("invalid attribute name {name:?}"));
    }
    let (prefix, local) = match name.split_once(':') {
        Some((prefix, local)) if !prefix.is_empty() && !local.is_empty() => {
            (Some(prefix.to_ascii_lowercase()), local)
        }
        None => (None, name),
        Some(_) => return Err(format!("invalid attribute name {name:?}")),
    };
    let local = local.to_ascii_lowercase();
    // Unprefixed HTML attributes are stored in the **empty** namespace: that
    // is how html5ever's tokenizer builds them (`QualName::new(None, ns!(),
    // name)`) and blitz's `Attributes::set/remove` compare the full QualName.
    // Mapping them to ns!(html) here would make every JS `setAttribute` (and
    // every `style` assignment) PUSH a duplicate next to the parsed attribute
    // instead of replacing it — reads then keep answering the stale original.
    let ns = match prefix.as_deref() {
        None => blitz_dom::ns!(),
        Some("xml") => blitz_dom::ns!(xml),
        Some("xlink") => blitz_dom::ns!(xlink),
        Some(_) => blitz_dom::ns!(),
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
    pub(super) const ELEMENT_NAME: &str = "__ironsurfDomElementName";
    pub(super) const COMPUTED_VALUE: &str = "__ironsurfDomComputedValue";
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
    pub(super) const CREATE_ELEMENT_NS: &str = "__ironsurfDomCreateElementNs";
    pub(super) const CREATE_TEXT: &str = "__ironsurfDomCreateText";
    pub(super) const CREATE_COMMENT: &str = "__ironsurfDomCreateComment";
    pub(super) const GET_BY_ID: &str = "__ironsurfDomGetById";
    pub(super) const LIVE_ITEM: &str = "__ironsurfDomLiveItem";
    pub(super) const LIVE_COUNT: &str = "__ironsurfDomLiveCount";
    pub(super) const QUERY: &str = "__ironsurfDomQuery";
    pub(super) const QUERY_ALL: &str = "__ironsurfDomQueryAll";
    pub(super) const MATCHES: &str = "__ironsurfDomMatches";
    pub(super) const CLOSEST: &str = "__ironsurfDomClosest";
    pub(super) const PARSE_HTML: &str = "__ironsurfDomParseHtml";
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
    pub(super) const DOCUMENT_WRITE: &str = "__ironsurfDomDocumentWrite";
    pub(super) const ATTR: &str = "__ironsurfDomAttr";
    pub(super) const ATTR_SET: &str = "__ironsurfDomAttrSet";
    pub(super) const ATTR_REMOVE: &str = "__ironsurfDomAttrRemove";
    pub(super) const ATTR_COUNT: &str = "__ironsurfDomAttrCount";
    pub(super) const ATTR_AT: &str = "__ironsurfDomAttrAt";
    // Images: what the engine knows about one element's image bytes, and the
    // document's own URL resolver (`img.src` must answer an ABSOLUTE url).
    pub(super) const IMAGE_STATE: &str = "__ironsurfDomImageState";
    pub(super) const RESOLVE_URL: &str = "__ironsurfDomResolveUrl";
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
fn live_ids(doc: &BaseDocument, kind: u32, param: &str, scope: Option<NodeId>) -> Vec<NodeId> {
    // Spec: element-level collections (getElementsByTagName(s),
    // getElementsByClassName, querySelectorAll) are SCOPED to the element;
    // `scope` is that element (`None` = the whole document). blitz-dom's
    // `query_selector_all_in` walks any subtree — detached ones included,
    // which is what DOMParser documents need.
    let in_scope = |selector: &str| -> Vec<NodeId> {
        match scope {
            Some(scope) => doc
                .query_selector_all_in(scope, selector)
                .map(|ids| ids.to_vec())
                .unwrap_or_default(),
            None => doc
                .query_selector_all(selector)
                .map(|ids| ids.to_vec())
                .unwrap_or_default(),
        }
    };
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
            in_scope(&selector)
        }
        live_kind::SELECTOR => in_scope(param),
        live_kind::ALL_ELEMENTS => match scope {
            Some(_) => in_scope("*"),
            None => elements_in_tree_order(doc),
        },
        _ => Vec::new(),
    }
}

/// One computed value → its CSS string (stylo's own `ToCss` serializers,
/// so keywords/lengths/colors come back byte-identical to a stylesheet).
fn css(value: &impl style_traits::values::ToCss) -> String {
    style_traits::values::ToCss::to_css_string(value)
}

/// The properties `getComputedStyle` answers (task: infosec.exchange —
/// Mastodon's lightbox boots on `direction`, and layout code reads the
/// sizing/display members next). Everything stylo resolves for the node;
/// the empty string is the honest "not modelled" answer.
fn computed_style_value(
    styles: &style::servo_arc::Arc<style::properties::ComputedValues>,
    property: &str,
) -> String {
    // Every arm reads the generated top-level `clone_<property>` accessor on
    // `ComputedValues` (stylo routes them to the right style struct itself).
    match property {
        "display" => css(&styles.clone_display()),
        "position" => css(&styles.clone_position()),
        "direction" => css(&styles.clone_direction()),
        "visibility" => css(&styles.clone_visibility()),
        "color" => css(&styles.clone_color()),
        "background-color" => css(&styles.clone_background_color()),
        "opacity" => css(&styles.clone_opacity()),
        "cursor" => css(&styles.clone_cursor()),
        "box-sizing" => css(&styles.clone_box_sizing()),
        // The modern split of `white-space` (collapse keyword) — mapped back
        // onto the legacy CSS keyword the page asked about.
        "white-space" => match css(&styles.clone_white_space_collapse()).as_str() {
            "collapse" => "normal".to_string(),
            other => other.to_string(),
        },
        "float" => css(&styles.clone_float()),
        "clear" => css(&styles.clone_clear()),
        "overflow-x" => css(&styles.clone_overflow_x()),
        "overflow-y" => css(&styles.clone_overflow_y()),
        "z-index" => css(&styles.clone_z_index()),
        "font-size" => css(&styles.clone_font_size()),
        "font-weight" => css(&styles.clone_font_weight()),
        "font-style" => css(&styles.clone_font_style()),
        "font-family" => css(&styles.clone_font_family()),
        "line-height" => css(&styles.clone_line_height()),
        "text-align" => css(&styles.clone_text_align()),
        "text-transform" => css(&styles.clone_text_transform()),
        "letter-spacing" => css(&styles.clone_letter_spacing()),
        "word-spacing" => css(&styles.clone_word_spacing()),
        "width" => css(&styles.clone_width()),
        "height" => css(&styles.clone_height()),
        "min-width" => css(&styles.clone_min_width()),
        "min-height" => css(&styles.clone_min_height()),
        "max-width" => css(&styles.clone_max_width()),
        "max-height" => css(&styles.clone_max_height()),
        "margin-top" => css(&styles.clone_margin_top()),
        "margin-right" => css(&styles.clone_margin_right()),
        "margin-bottom" => css(&styles.clone_margin_bottom()),
        "margin-left" => css(&styles.clone_margin_left()),
        "padding-top" => css(&styles.clone_padding_top()),
        "padding-right" => css(&styles.clone_padding_right()),
        "padding-bottom" => css(&styles.clone_padding_bottom()),
        "padding-left" => css(&styles.clone_padding_left()),
        "top" => css(&styles.clone_top()),
        "right" => css(&styles.clone_right()),
        "bottom" => css(&styles.clone_bottom()),
        "left" => css(&styles.clone_left()),
        "pointer-events" => css(&styles.clone_pointer_events()),
        "user-select" => css(&styles.clone_user_select()),
        _ => String::new(),
    }
}

/// `raw` node id → a real query scope: `None` for the whole document (0 /
/// absent node — the sentinel the JS glue passes) and for nodes the store
/// has already dropped.
fn scope_node(raw: u64) -> Option<NodeId> {
    (raw != NO_NODE).then(|| NodeId::from_u64(raw))
}

/// `querySelector` with the element-scope rules of the task: a live scope
/// node restricts the match to its descendants (detached subtrees included
/// — blitz walks from the scope node itself); anything else stays the
/// document-wide query.
fn query_selector_scoped(
    doc: &BaseDocument,
    selector: &str,
    scope: Option<NodeId>,
) -> Result<Option<NodeId>, ()> {
    match scope.filter(|scope| doc.get_node(*scope).is_some()) {
        Some(scope) => doc.query_selector_in(scope, selector).map_err(|_| ()),
        None => doc.query_selector(selector).map_err(|_| ()),
    }
}

/// `querySelectorAll` under the same scope rules.
fn query_selector_all_scoped(
    doc: &BaseDocument,
    selector: &str,
    scope: Option<NodeId>,
) -> Result<Vec<NodeId>, ()> {
    match scope.filter(|scope| doc.get_node(*scope).is_some()) {
        Some(scope) => doc
            .query_selector_all_in(scope, selector)
            .map(|ids| ids.to_vec())
            .map_err(|_| ()),
        None => doc
            .query_selector_all(selector)
            .map(|ids| ids.to_vec())
            .map_err(|_| ()),
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

/// Wrap one DOM native so its **error path** records the throw (first line,
/// e.g. `"Uncaught NotFoundError: parent is gone"`) into the live host's
/// `dom_throws` before it propagates. The explicit `F: Fn…` bound is what
/// pins the native closures' otherwise-ambiguous error type; the wrapper
/// itself stays `Copy` (boa's requirement).
fn record_dom_throw<F>(body: F) -> impl Fn(&JsValue, &[JsValue], &mut Context) -> Result<JsValue, boa_engine::JsError> + Copy + 'static
where
    F: Fn(&JsValue, &[JsValue], &mut Context) -> Result<JsValue, boa_engine::JsError>
        + Copy
        + 'static,
{
    move |this, args, context| {
        let result = body(this, args, context);
        if let Err(err) = &result {
            let message = err
                .to_string()
                .lines()
                .next()
                .unwrap_or("DOM error")
                .to_string();
            with_host(|host| {
                // Bounded: a pathological page throwing in a loop must not
                // grow the host cell without limit.
                if host.dom_throws.len() < 256 {
                    host.dom_throws.push(message);
                }
            });
        }
        result
    }
}

/// Install every DOM native on `context` (design: all behaviour is native;
/// the JS glue only delegates — see the PRELUDE).
fn register_dom_natives(context: &mut Context) {
    // Every DOM native is wrapped: a native `Err` (SyntaxError on a bad
    // selector, NotFoundError on a dead node, …) records its first line in
    // `HostState::dom_throws` before it propagates, so the tab can report the
    // escaped exception over IPC as `FromTab::ConsoleLog { source: "dom" }`
    // instead of a generic `"js"` script error (see `JsRuntime::classify_throw`).
    macro_rules! native {
        ($name:expr, $len:expr, $body:expr) => {
            if let Err(err) = context.register_global_builtin_callable(
                JsString::from($name),
                $len,
                NativeFunction::from_copy_closure(record_dom_throw($body)),
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
    // `getComputedStyle` engine: the REAL stylo computed values of the live
    // node (the very `Arc<ComputedValues>` the tab's `settle()` resolved),
    // serialized per property. Unstyled (never-laid-out) nodes and unknown
    // properties answer "" — the documented gap, versus pretending.
    native!(dom_native::COMPUTED_VALUE, 3, |_this, args, context| {
        let doc = arg_doc(args, 0, context)?;
        let node = arg_node(args, 1, context)?;
        let property = arg_string(args, 2, context)?;
        let out = with_live_doc(doc, |document| {
            let styles = document
                .get_node(node_id(node))?
                .primary_styles()?
                .clone();
            Some(computed_style_value(&styles, &property))
        })
        .unwrap_or_default()
        .unwrap_or_default();
        Ok(JsValue::from(JsString::from(out)))
    });
    // `localName`/`tagName` source: the element's RAW local name (case kept
    // — svg `linearGradient` included) and its namespace, packed as one
    // `local\u{1}ns` string; non-elements answer "" so both JS getters
    // report "" like the spec's empty strings for text/comment nodes.
    native!(dom_native::ELEMENT_NAME, 2, |_this, args, context| {
        let doc = arg_doc(args, 0, context)?;
        let node = arg_node(args, 1, context)?;
        let packed = with_live_doc(doc, |document| {
            document
                .get_node(node_id(node))
                .and_then(|node| node.element_data())
                .map(|element| {
                    // One `local\u{1}h|f` string: the raw local name plus an
                    // HTML/foreign namespace bit (`tagName` uppercases HTML
                    // elements only, the SVG DOM keeps names as-written).
                    let mut packed = element.name.local.to_string();
                    packed.push('\u{1}');
                    packed.push(if element.name.ns == blitz_dom::ns!(html) {
                        'h'
                    } else {
                        'f'
                    });
                    packed
                })
        })
        .unwrap_or_default();
        Ok(JsValue::from(JsString::from(packed.unwrap_or_default())))
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
    native!(dom_native::CREATE_ELEMENT_NS, 3, |_this, args, context| {
        let doc = arg_doc(args, 0, context)?;
        let ns = arg_string(args, 1, context)?;
        let tag = arg_string(args, 2, context)?;
        let name = qual_name_for_ns(&tag, &ns)
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
                query_selector_all_scoped(document, &param, scope_node(raw))
                    .map(|ids| ids.len())
                    .map_err(|_| {
                        JsNativeError::syntax()
                            .with_message(format!("SyntaxError: invalid selector {param:?}"))
                    })
            });
            match count {
                None => return Err(throw_no_doc()),
                Some(Err(err)) => return Err(boa_engine::JsError::from_native(err)),
                Some(Ok(count)) => count,
            }
        } else {
            with_live_doc(doc, |document| {
                live_ids(document, kind, &param, scope_node(raw)).len()
            })
            .unwrap_or(0)
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
                let ids = live_ids(document, kind, &param, scope_node(raw));
                usize::try_from(index)
                    .ok()
                    .and_then(|index| ids.get(index).copied())
            })
            .flatten()
        };
        Ok(JsValue::new(raw_item.map_or(NO_NODE, node_raw) as f64))
    });
    native!(dom_native::QUERY, 3, |_this, args, context| {
        let doc = arg_doc(args, 0, context)?;
        let selector = arg_string(args, 1, context)?;
        // Optional SCOPE node (task: real DOMParser): restrict the match to
        // its descendants — and follow detached subtrees, which is exactly
        // what a `DOMParser.parseFromString` document is. Absent/0 (or a
        // vanished node) keeps the document-wide query.
        let raw = arg_node(args, 2, context)?;
        // An unparsable selector raises SyntaxError (task contract); a
        // valid-but-empty match is just null at the JS layer.
        let outcome = with_live_doc(doc, |document| {
            query_selector_scoped(document, &selector, scope_node(raw))
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
    native!(dom_native::QUERY_ALL, 3, |_this, args, context| {
        let doc = arg_doc(args, 0, context)?;
        let selector = arg_string(args, 1, context)?;
        let raw = arg_node(args, 2, context)?;
        // One-shot snapshot list (a static NodeList): the live-list proxies
        // keep their document scope; a detached subtree gets the snapshot
        // shape the spec itself hands back for `parseFromString` documents.
        let outcome = with_live_doc(doc, |document| {
            query_selector_all_scoped(document, &selector, scope_node(raw)).map_err(|_| {
                JsNativeError::syntax()
                    .with_message(format!("SyntaxError: invalid selector {selector:?}"))
            })
        });
        match outcome {
            None => Err(throw_no_doc()),
            Some(Err(err)) => Err(boa_engine::JsError::from_native(err)),
            Some(Ok(ids)) => {
                let array = boa_engine::object::builtins::JsArray::from_iter(
                    ids.into_iter().map(|id| JsValue::new(node_raw(id) as f64)),
                    context,
                );
                Ok(JsValue::from(array))
            }
        }
    });
    native!(dom_native::MATCHES, 3, |_this, args, context| {
        let doc = arg_doc(args, 0, context)?;
        let node = arg_node(args, 1, context)?;
        let selector = arg_string(args, 2, context)?;
        let outcome = with_live_doc(doc, |document| {
            document.matches_selector(node_id(node), &selector).map_err(|_| {
                JsNativeError::syntax()
                    .with_message(format!("SyntaxError: invalid selector {selector:?}"))
            })
        });
        match outcome {
            None => Err(throw_no_doc()),
            Some(Err(err)) => Err(boa_engine::JsError::from_native(err)),
            Some(Ok(matches)) => Ok(JsValue::new(matches)),
        }
    });
    native!(dom_native::CLOSEST, 3, |_this, args, context| {
        let doc = arg_doc(args, 0, context)?;
        let node = arg_node(args, 1, context)?;
        let selector = arg_string(args, 2, context)?;
        let outcome = with_live_doc(doc, |document| {
            document.closest(node_id(node), &selector).map_err(|_| {
                JsNativeError::syntax()
                    .with_message(format!("SyntaxError: invalid selector {selector:?}"))
            })
        });
        match outcome {
            None => Err(throw_no_doc()),
            Some(Err(err)) => Err(boa_engine::JsError::from_native(err)),
            Some(Ok(found)) => Ok(JsValue::new(found.map_or(NO_NODE, node_raw) as f64)),
        }
    });
    // DOMParser's engine (task: real DOMParser): a REAL detached `<html>`
    // element in the live document's node store, its subtree built by the
    // very same html5ever parser (`parse_inner_html_into_mutator`) that
    // serves `innerHTML=`. The nodes are genuine blitz nodes — every DOM
    // native works on them, subtree-scoped — they are simply never inserted
    // (no layout, no paint, no resource fetches), like a browser's inert
    // `parseFromString` document.
    native!(dom_native::PARSE_HTML, 2, |_this, args, context| {
        let doc = arg_doc(args, 0, context)?;
        let html = arg_string(args, 1, context)?;
        let name = qual_name_for_tag("html")
            .expect("the literal 'html' is a valid tag name");
        let raw = with_mut(doc, move |mutator| {
            let host = mutator.create_element(name, Vec::new());
            blitz_html::DocumentHtmlParser::parse_inner_html_into_mutator(
                mutator,
                host,
                &html,
            );
            node_raw(host)
        })
        .unwrap_or(NO_NODE);
        Ok(JsValue::new(raw as f64))
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
    // document.write()/writeln(): the insertion cursor into the parse stream.
    // IronSurf runs every script *after* the document has parsed, so the
    // cursor sits at the end of the tree: the written markup is really parsed
    // (the same fragment parser `innerHTML`-set uses) and appended under
    // `<body>` (falling back to `<html>`, then the document root), landing in
    // the live DOM where the tab's `settle()` lays it out and paints it like
    // any other JS mutation. Deliberately never wipes the running document
    // (the spec's post-load `document.open()` replacement would destroy the
    // realm the page's scripts — and the userscripts layered on top — live
    // in), and a silent no-op with no live document (snapshot mode).
    native!(dom_native::DOCUMENT_WRITE, 2, |_this, args, context| {
        let doc = arg_doc(args, 0, context)?;
        let html = arg_string(args, 1, context)?;
        with_mut(doc, |mutator| {
            let target = mutator
                .doc
                .find_body_node()
                .map(|node| node.id)
                .or_else(|| html_element_id(mutator.doc))
                .unwrap_or_else(|| mutator.doc.root_node().id);
            blitz_html::DocumentHtmlParser::parse_inner_html_into_mutator(
                mutator, target, &html,
            );
        });
        Ok(JsValue::undefined())
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
            // Remove the stored attribute whose name matches: either the
            // bare local name (the parser stores full QualNames pages
            // cannot spell) or the `prefix:local` serialization react-dom
            // passes to removeAttributeNS for SVG `xlink:href`.
            let existing = mutator
                .doc
                .get_node(node_id(node))
                .and_then(|node| node.element_data())
                .and_then(|element| {
                    element.attrs.iter().find(|attr| {
                        attr.name.prefix.as_ref().map_or_else(
                            || str::eq_ignore_ascii_case(&attr.name.local, name.as_str()),
                            |prefix| {
                                format!("{prefix}:{}", attr.name.local).eq_ignore_ascii_case(name.as_str())
                                    || str::eq_ignore_ascii_case(&attr.name.local, name.as_str())
                            },
                        )
                    })
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

    // -- images (`new Image()`, `img.complete`, `img.naturalWidth`) ----------

    // What the engine knows about one element's image bytes, as
    // `[loaded, naturalWidth, naturalHeight]`. blitz attaches
    // `SpecialElementData::Image` to an `<img>` when its fetch settles (or
    // applies it straight out of the document's image cache), so the triple
    // answers honestly for the image's *intrinsic* size; every other element
    // — and an `<img>` whose bytes have not arrived, or failed — reads
    // `[false, 0, 0]`.
    native!(dom_native::IMAGE_STATE, 2, |_this, args, context| {
        let doc = arg_doc(args, 0, context)?;
        let node = arg_node(args, 1, context)?;
        let state = with_live_doc(doc, |document| {
            let element = document.get_node(node_id(node))?.element_data()?;
            Some(match &element.special_data {
                SpecialElementData::Image(data) => match &**data {
                    ImageData::Raster(raster) => {
                        (true, f64::from(raster.width), f64::from(raster.height))
                    }
                    ImageData::Svg(svg) => (
                        true,
                        f64::from(svg.intrinsic_width().unwrap_or(0.0).max(0.0)),
                        f64::from(svg.intrinsic_height().unwrap_or(0.0).max(0.0)),
                    ),
                    ImageData::None => (false, 0.0, 0.0),
                },
                _ => (false, 0.0, 0.0),
            })
        })
        .flatten()
        .unwrap_or((false, 0.0, 0.0));
        let array = boa_engine::object::builtins::JsArray::from_iter(
            [
                JsValue::from(state.0),
                JsValue::new(state.1),
                JsValue::new(state.2),
            ],
            context,
        );
        Ok(JsValue::from(array))
    });

    // The document's own URL resolver. HTML's `reflect, parsing a URL`
    // rule makes the reflected `src`/`href` getters answer the *absolute*
    // url, and the tab — never the JS glue — owns the parser: this is
    // `Url::join` against the live document URL, the same resolver
    // `Runtime::fetch_media` uses for `<video src>`. `null` = unresolvable
    // (a base-less document), in which case the glue reports the raw text.
    native!(dom_native::RESOLVE_URL, 2, |_this, args, context| {
        let doc = arg_doc(args, 0, context)?;
        let raw = arg_string(args, 1, context)?;
        let resolved = with_live_doc(doc, move |document| {
            if raw.is_empty() {
                return None;
            }
            document
                .url()
                .join(&raw)
                .ok()
                .map(|url| url.to_string())
        })
        .flatten();
        Ok(match resolved {
            Some(url) => JsValue::from(JsString::from(url)),
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
                // Spec: every input has a checkedness; only checkbox/radio
                // expose it meaningfully. React hands `checked={false}` to
                // plain text inputs on controlled forms, and the old hard
                // TypeError killed mastodon's whole search column behind an
                // error boundary. Non-inputs simply have no checked IDL.
                if input_type(element).is_some() {
                    return Ok(());
                }
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

/// The active host's dialog cell (task: JS dialogs). The `Arc` is cloned out
/// before `ask` blocks — no host borrow is ever held across the modal wait.
fn active_dialog() -> Option<Arc<crate::runtime::DialogHost>> {
    ACTIVE_HOSTS
        .with(|stack| stack.borrow().last().cloned())
        .and_then(|scope| scope.host.borrow().dialog.clone())
}

// ---------------------------------------------------------------------------
// Bindings

/// Native/glue names (private to the binding surface; underscore-prefixed so
/// they are obvious in page stack traces).
mod native {
    pub(super) const URL: &str = "__ironsurfUrl";
    pub(super) const USER_AGENT: &str = "__ironsurfUserAgent";
    pub(super) const BODY_TEXT: &str = "__ironsurfBodyText";
    pub(super) const HAS_BODY: &str = "__ironsurfHasBody";
    pub(super) const TITLE_GET: &str = "__ironsurfTitleGet";
    pub(super) const TITLE_SET: &str = "__ironsurfTitleSet";
    pub(super) const CONSOLE: &str = "__ironsurfConsole";
    pub(super) const NAVIGATE: &str = "__ironsurfNavigate";
    pub(super) const SET_URL: &str = "__ironsurfSetUrl";
    pub(super) const SET_TIMEOUT: &str = "__ironsurfSetTimeout";
    pub(super) const SET_INTERVAL: &str = "__ironsurfSetInterval";
    pub(super) const RAF: &str = "__ironsurfRaf";
    pub(super) const TIMER_CLEAR: &str = "__ironsurfClearTimer";
    pub(super) const MEDIA_GET: &str = "__ironsurfMediaGet";
    pub(super) const MEDIA_CMD: &str = "__ironsurfMediaCmd";
    pub(super) const DIALOG: &str = "__ironsurfDialog";
    pub(super) const STORAGE: &str = "__ironsurfStorage";
    pub(super) const COOKIES: &str = "__ironsurfCookies";
    pub(super) const XHR: &str = "__ironsurfXhr";
    pub(super) const RANDOM_BYTES: &str = "__ironsurfRandomBytes";
    pub(super) const RANDOM_UUID: &str = "__ironsurfRandomUUID";
}

/// Process-wide random source for `crypto.getRandomValues`/`randomUUID`
/// (task: infosec.exchange posts): a splitmix64 seeded from the clock.
/// Pages spend this on ids (Mastodon stamps request ids with `randomUUID`),
/// not keys — and nothing here is a security boundary.
fn next_random_u64() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::LazyLock;
    use std::time::{SystemTime, UNIX_EPOCH};
    static STATE: LazyLock<AtomicU64> = LazyLock::new(|| {
        AtomicU64::new(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0x5DEECE_66D),
        )
    });
    let mut z = STATE.fetch_add(0x9E37_79B9_7F4A_7C15, Ordering::Relaxed);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// One-shot delay used for `requestAnimationFrame` (~60 Hz; the tab has no
/// vsync source, a fixed 16 ms frame is good enough).
const RAF_INTERVAL: Duration = Duration::from_millis(16);

/// Storage-area argument (task: per-origin `localStorage`): `null`/`undefined`
/// read as "absent", everything else stringifies — and the coercion runs
/// **before** the host cell is touched, so a page's own `toString` never
/// re-enters through a held borrow (the rule every other native follows).
fn arg_opt_string(
    value: Option<&JsValue>,
    context: &mut Context,
) -> Result<Option<String>, boa_engine::JsError> {
    match value {
        Some(value) if !value.is_undefined() && !value.is_null() => {
            Ok(Some(value.to_string(context)?.to_std_string_escaped()))
        }
        _ => Ok(None),
    }
}

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
  // The element `focus()` last named (see the `focus()` glue).
  var __focusedElement = null;
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
    // Real `Event` instances: every event a page receives satisfies
    // `ev instanceof Event` (function declarations hoist, so the Event
    // class defined further down is available here).
    var ev = Object.create(Event.prototype);
    Object.assign(ev, {
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
    });
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
    // `location.origin`: the tuple origin serialised (scheme://host:port).
    // Mastodon's emoji-data loader does `new URL(path, location.origin)` —
    // with `origin` absent the base is undefined and WHATWG parsing throws
    // "relative URL without a base" out of the fetch. Documents whose scheme
    // has no serialisable origin (about:, data:, file:, …) answer the spec's
    // opaque "null" (a string), which also makes `new URL(x, "null")` fail
    // loudly rather than resolve against garbage.
    get origin() {
      var p = urlParts(host.__ironsurfUrl);
      if ((p.protocol === "http:" || p.protocol === "https:") && p.host) {
        return p.protocol + "//" + p.host;
      }
      return p.host ? p.protocol + "//" + p.host : "null";
    },
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
  // Viewport constants (task: infosec.exchange). These were 0 — and every
  // measuring widget (carousels, lazy-render gates, `matchMedia` fallbacks)
  // behaves pathologically on a 0×0 viewport: the landing carousel's raf loop
  // never settles its measurement. They now default to the engine's own
  // default window (DEFAULT_SIZE, 1024×768 CSS px); a live resize is not yet
  // propagated into the realm (TODO: push ToTab::Resize through here too).
  host.innerWidth = 1024;
  host.innerHeight = 768;
  host.outerWidth = 1024;
  host.outerHeight = 768;
  host.location = location;

  // ---------------------------------------------------------------------
  // navigator: `userAgent` reports the tab's EFFECTIVE User-Agent — the very
  // value every outbound HTTP request carries (`--user-agent` at launch,
  // replaced by `ToTab::SetUserAgent` mid-session, blank/None back to the
  // engine default). The getter calls the `__ironsurfUserAgent` native, which
  // reads the tab's live UA cell, so a change is visible to scripts without
  // any reload — the same dynamic-host pattern as `location.href`.
  // ---------------------------------------------------------------------
  var navigator = {
    get userAgent() { return host.__ironsurfUserAgent(); },
    get appVersion() {
      return String(host.__ironsurfUserAgent()).replace(/^Mozilla\//, "");
    },
    appName: "Netscape",
    platform: "Linux x86_64",
    vendor: "IronSurf",
    onLine: true,
    doNotTrack: null,
    hardwareConcurrency: 1,
    maxTouchPoints: 0,
    language: "en-US",
    languages: ["en-US"],
  };
  host.navigator = navigator;
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
  // window.matchMedia (task: startpage.com search bar). Media features are
  // never evaluated for scripts here, so `matches` answers only the trivial
  // whole-media queries (`""`, `all`, `screen`) and reports false for
  // everything else — `prefers-color-scheme: dark` included. Listeners
  // register and unregister as no-ops and never fire, so `change` events
  // never arrive. What the platform lacked was a CALLABLE matchMedia: its
  // absence made the first inline script of startpage.com's home page die
  // with "TypeError: not a callable function" at `window.matchMedia(...)`,
  // aborting the theme-reveal that lifts the `#root { opacity: 0 }` the
  // page ships to prevent a theme flash — the whole page, search bar
  // included, stayed invisible and unusable.
  // ---------------------------------------------------------------------
  host.matchMedia = function (query) {
    var media = query === undefined || query === null ? "" : String(query);
    var q = media.replace(/^\s+/, "").toLowerCase();
    var matches =
      q === "" || q === "all" || q === "screen" || q === "only screen";
    return {
      media: media,
      matches: matches,
      onchange: null,
      addListener: noop,
      removeListener: noop,
      addEventListener: noop,
      removeEventListener: noop,
      dispatchEvent: function () { return returnTrue(); },
    };
  };

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
          return function (fn, thisArg) {
            for (var i = 0, n = count(); i < n; i++) fn.call(thisArg, item(i), i, view);
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
        if (typeof key === "symbol") {
          // NamedNodeMap is iterable (wetsuit's sanitizer runs
          // `for (let attr of el.attributes)`); yields {name, value} snapshots.
          if (key === Symbol.iterator)
            return function () {
              var arr = [];
              var count = host.__ironsurfDomAttrCount(d, n);
              for (var i = 0; i < count; i++) {
                var a = attrAt(i);
                if (a !== null) arr.push(a);
              }
              return arr[Symbol.iterator]();
            };
          if (key === Symbol.toStringTag) return "NamedNodeMap";
          return undefined;
        }
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
  // `localName`/`tagName` (spec): the raw local name, and the HTML-element
  // uppercase / foreign-element-as-is rule for `tagName`. Non-elements get
  // "" from both, which is exactly what a browser hands back.
  function elementNameParts(node) {
    var raw = host.__ironsurfDomElementName(node.__d, node.__n);
    var cut = raw.indexOf("\u0001");
    if (cut < 0) return null; // not an element
    return { local: raw.slice(0, cut), ns: raw.slice(cut + 1) };
  }
  protoGet("localName", function () {
    var parts = elementNameParts(this);
    return parts === null ? "" : parts.local;
  });
  protoGet("tagName", function () {
    var parts = elementNameParts(this);
    if (parts === null) return "";
    return parts.ns === "h" ? parts.local.toUpperCase() : parts.local;
  });
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
  // HTMLElement.innerText — spec'd as textContent with rendered-line
  // formatting; with no layout engine the textContent value IS the honest
  // approximation. Glitch-timeline link cards build their prop as
  // `text: el.innerText` (collection_preview_card), and a missing member
  // there propagated undefined into `text.startsWith("#")` — the one
  // uncaught TypeError that aborted the whole first render pass on
  // infosec.exchange (Boa reports property reads of undefined as "cannot
  // convert 'null' or 'undefined' to object").
  protoAccessor(
    "innerText",
    function () {
      var v = host.__ironsurfDomText(this.__d, this.__n);
      return v === null ? "" : v;
    },
    function (v) { host.__ironsurfDomTextSet(this.__d, this.__n, String(v)); }
  );
  protoAccessor(
    "innerHTML",
    function () {
      if (isTemplateElement(this)) return templateContents(this).innerHTML;
      return host.__ironsurfDomSerialize(this.__d, this.__n, false);
    },
    function (v) {
      if (isTemplateElement(this)) { setTemplateContents(this, String(v)); return; }
      host.__ironsurfDomSetInner(this.__d, this.__n, String(v));
    }
  );
  // HTMLTemplateElement.content — the inert DocumentFragment behind a
  // <template>. wetsuit (mstdn.social's status composer) sanitizes HTML
  // through `document.createElement("template"); t.innerHTML = html;
  // walk(t.content)`, and without `content` the very first status render
  // died reading `nodeType` of `undefined`. Contents live realm-side (blitz
  // has no template content model): the fragment is a JS list of real,
  // detached nodes, exactly like createDocumentFragment's.
  protoGet("content", function () {
    if (isTemplateElement(this)) return templateContents(this);
    return undefined;
  });
  protoGet("outerHTML", function () { return host.__ironsurfDomSerialize(this.__d, this.__n, true); });

  // ---------------------------------------------------------------------
  // element.dataset — a DOMStringMap view over `data-*` attributes (task:
  // startpage.com search bar, where `#site_theme`.dataset.siteTheme reads
  // `data-site-theme`). Live like `style`: every access re-reads the
  // document, and writes go through the same ATTR_SET native as
  // setAttribute, so they restyle and repaint like any attribute mutation.
  // The spec's camel-case mapping both ways: siteTheme ⇄ data-site-theme,
  // col2 ⇄ data-col2; a leading capital takes no dash (X → data-x). Keys
  // are enumerable (`for (var k in el.dataset)`), and
  // `delete el.dataset.foo` removes the attribute.
  // ---------------------------------------------------------------------
  function datasetAttrName(key) {
    var s = String(key);
    var out = "data-";
    for (var i = 0; i < s.length; i++) {
      var c = s.charAt(i);
      if (c >= "A" && c <= "Z") {
        if (i > 0) out += "-";
        out += c.toLowerCase();
      } else {
        out += c;
      }
    }
    return out;
  }
  function datasetKey(name) {
    var s = String(name).slice(5); // strip "data-"
    return s.replace(/-(.)/g, function (_, c) { return c.toUpperCase(); });
  }
  function makeDataset(d, n) {
    function read(key) {
      var v = host.__ironsurfDomAttr(d, n, datasetAttrName(key));
      return v === null ? undefined : v;
    }
    var handlers = {
      get: function (t, key) {
        if (typeof key === "symbol")
          return key === Symbol.toStringTag ? "DOMStringMap" : undefined;
        return read(key);
      },
      set: function (t, key, value) {
        host.__ironsurfDomAttrSet(
          d,
          n,
          datasetAttrName(key),
          value === null || value === undefined ? "" : String(value)
        );
        return true;
      },
      has: function (t, key) {
        return typeof key !== "symbol" && read(key) !== undefined;
      },
      ownKeys: function () {
        var keys = [];
        var count = host.__ironsurfDomAttrCount(d, n);
        for (var i = 0; i < count; i++) {
          var pair = host.__ironsurfDomAttrAt(d, n, i);
          if (pair !== null && String(pair[0]).indexOf("data-") === 0) {
            keys.push(datasetKey(pair[0]));
          }
        }
        return keys;
      },
      getOwnPropertyDescriptor: function (t, key) {
        if (typeof key === "string" && read(key) !== undefined)
          return { value: read(key), writable: true, enumerable: true, configurable: true };
        return undefined;
      },
      deleteProperty: function (t, key) {
        host.__ironsurfDomAttrRemove(d, n, datasetAttrName(key));
        return true;
      },
    };
    return new Proxy({}, handlers);
  }
  protoGet("dataset", function () { return makeDataset(this.__d, this.__n); });

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

  // Element.style — a CSSStyleDeclaration view over the `style` content
  // attribute. cssText, getPropertyValue/setProperty/removeProperty, and
  // direct member writes (el.style.color = "red", through the same live
  // Proxy trick the collections use) ALL rewrite the attribute via ATTR_SET,
  // as does the non-standard-but-ubiquitous whole-string assignment
  // (document.body.style = "…"): every route invalidates inline style exactly
  // like setAttribute does, so the tab's settle() restyles and repaints the
  // change. Declarations are kept verbatim (later ones win on read); a
  // semicolon inside url()/quoted strings is the usual naive-split limit.
  function cssName(key) {
    var s = String(key);
    if (s === "cssFloat" || s === "styleFloat") return "float";
    return s
      .replace(/[A-Z]/g, function (c) { return "-" + c.toLowerCase(); })
      .toLowerCase();
  }
  function declPairs(text) {
    var pairs = [];
    var parts = String(text).split(";");
    for (var i = 0; i < parts.length; i++) {
      var idx = parts[i].indexOf(":");
      if (idx < 0) continue;
      var name = parts[i].slice(0, idx).replace(/^\s+/, "").toLowerCase();
      var value = parts[i].slice(idx + 1).replace(/^\s+/, "").replace(/\s+$/, "");
      if (name !== "") pairs.push([name, value]);
    }
    return pairs;
  }
  function serializeDecls(pairs) {
    var out = "";
    for (var i = 0; i < pairs.length; i++) {
      if (i > 0) out += "; ";
      out += pairs[i][0] + ": " + pairs[i][1];
    }
    return out === "" ? "" : out + ";";
  }
  function makeStyle(d, n) {
    function text() {
      var v = host.__ironsurfDomAttr(d, n, "style");
      return v === null ? "" : String(v);
    }
    function setText(t) { host.__ironsurfDomAttrSet(d, n, "style", t); }
    function read(name) {
      var pairs = declPairs(text());
      for (var i = pairs.length - 1; i >= 0; i--) {
        if (pairs[i][0] === name) return pairs[i][1];
      }
      return "";
    }
    function put(name, value) {
      var v = value === null || value === undefined ? "" : String(value);
      var pairs = declPairs(text());
      var kept = [];
      for (var i = 0; i < pairs.length; i++) {
        if (pairs[i][0] !== name) kept.push(pairs[i]);
      }
      if (v !== "") kept.push([name, v]);
      setText(serializeDecls(kept));
    }
    var target = {};
    var handlers = {
      get: function (t, key) {
        if (key === "cssText") return text();
        if (key === "getPropertyValue")
          return function (name) { return read(cssName(name)); };
        if (key === "setProperty")
          return function (name, value) { put(cssName(name), value); };
        if (key === "removeProperty")
          return function (name) {
            var want = cssName(name);
            var old = read(want);
            put(want, "");
            return old;
          };
        if (typeof key === "symbol")
          return key === Symbol.toStringTag ? "CSSStyleDeclaration" : undefined;
        var v = read(cssName(key));
        // el.style.color === "red"; an unset member reads undefined, like
        // the (inherited) property it is on a real CSSStyleDeclaration.
        return v === "" ? undefined : v;
      },
      set: function (t, key, value) {
        if (key === "cssText") {
          setText(value === null || value === undefined ? "" : String(value));
          return true;
        }
        if (typeof key !== "symbol") put(cssName(key), value);
        return true;
      },
      has: function (t, key) {
        if (key === "cssText") return true;
        if (typeof key !== "string") return false;
        return read(cssName(key)) !== "";
      },
    };
    return new Proxy(target, handlers);
  }
  protoAccessor(
    "style",
    function () { return makeStyle(this.__d, this.__n); },
    // Whole-string assignment — document.body.style = "color: red". The
    // string lands in the `style` attribute verbatim (like setAttribute), so
    // the change restyles on the tab's next resolve.
    function (v) { host.__ironsurfDomAttrSet(this.__d, this.__n, "style", String(v)); }
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

  // DOMTokenList over the `class` attribute. Mastodon toggles layout classes
  // on <body> (`document.body.classList.toggle(...)`) on every mount, and the
  // missing accessor read as "cannot convert undefined to object". Each
  // access returns a fresh snapshot array (indices + length for free) whose
  // mutators re-read the attribute, so repeated toggles compose correctly.
  var domTokenListMethods = {
    item: function (index) {
      var c = tokenClasses(this.__tokenOwner);
      var i = Number(index);
      if (!isFinite(i) || i < 0) return null;
      return c[i] === undefined ? null : c[i];
    },
    contains: function (token) {
      return tokenClasses(this.__tokenOwner).indexOf(String(token)) !== -1;
    },
    add: function () {
      var node = this.__tokenOwner;
      var c = tokenClasses(node);
      for (var i = 0; i < arguments.length; i++) {
        var t = String(arguments[i]);
        if (c.indexOf(t) === -1) c.push(t);
      }
      host.__ironsurfDomAttrSet(node.__d, node.__n, "class", c.join(" "));
    },
    remove: function () {
      var node = this.__tokenOwner;
      var c = tokenClasses(node);
      for (var i = 0; i < arguments.length; i++) {
        var t = String(arguments[i]);
        var at = c.indexOf(t);
        if (at !== -1) c.splice(at, 1);
      }
      host.__ironsurfDomAttrSet(node.__d, node.__n, "class", c.join(" "));
    },
    toggle: function (token, force) {
      var node = this.__tokenOwner;
      var s = String(token);
      var c = tokenClasses(node);
      var has = c.indexOf(s) !== -1;
      var want = force === undefined || force === null ? !has : !!force;
      if (want && !has) c.push(s);
      if (!want && has) c.splice(c.indexOf(s), 1);
      host.__ironsurfDomAttrSet(node.__d, node.__n, "class", c.join(" "));
      return want;
    },
    replace: function (token, newToken) {
      var node = this.__tokenOwner;
      var c = tokenClasses(node);
      var at = c.indexOf(String(token));
      if (at === -1) return false;
      c[at] = String(newToken);
      host.__ironsurfDomAttrSet(node.__d, node.__n, "class", c.join(" "));
      return true;
    },
    forEach: function (fn, thisArg) {
      var c = tokenClasses(this.__tokenOwner);
      for (var i = 0; i < c.length; i++) fn.call(thisArg, c[i], i, this);
    },
    toString: function () {
      return tokenClasses(this.__tokenOwner).join(" ");
    },
  };
  function tokenClasses(node) {
    var v = host.__ironsurfDomAttr(node.__d, node.__n, "class");
    if (v === null || v === undefined || v === "") return [];
    return String(v).trim().split(/\s+/);
  }
  protoAccessor("classList", function () {
    var list = tokenClasses(this);
    Object.defineProperty(list, "__tokenOwner", { value: this, enumerable: false });
    Object.keys(domTokenListMethods).forEach(function (m) {
      Object.defineProperty(list, m, {
        value: domTokenListMethods[m],
        enumerable: false,
        configurable: true,
        writable: true,
      });
    });
    return list;
  });

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
  // Element.getAttributeNames() — unhead's render loop (on every Mastodon
  // page view) adopts orphaned tag elements with
  // `for (let name of el.getAttributeNames())`, and the missing method
  // threw "not a callable function" out of the provider's scheduler.
  // The *NS variants: react-dom reaches for setAttributeNS the moment it
  // patches an SVG `href`/`xlink:href` (Mastodon's icon <use> elements),
  // and the missing method aborted the whole commit phase. Blitz stores
  // attribute names verbatim, so the qualified name IS the storage key.
  protoMethod("setAttributeNS", function (ns, name, value) {
    host.__ironsurfDomAttrSet(this.__d, this.__n, String(name), String(value));
  });
  protoMethod("getAttributeNS", function (ns, name) {
    return host.__ironsurfDomAttr(this.__d, this.__n, String(name));
  });
  protoMethod("removeAttributeNS", function (ns, name) {
    host.__ironsurfDomAttrRemove(this.__d, this.__n, String(name));
  });
  protoMethod("getAttributeNames", function () {
    var out = [];
    var count = host.__ironsurfDomAttrCount(this.__d, this.__n);
    for (var i = 0; i < count; i++) {
      var pair = host.__ironsurfDomAttrAt(this.__d, this.__n, i);
      if (pair !== null) out.push(pair[0]);
    }
    return out;
  });
  // Element-level queries are SCOPED to the element (spec) — the native
  // walks THIS node's subtree, detached ones included, so `querySelector`
  // works on the real DOM a `DOMParser.parseFromString` returned too.
  protoMethod("querySelector", function (selector) {
    return wrapNode(this.__d, host.__ironsurfDomQuery(this.__d, String(selector), this.__n));
  });
  protoMethod("querySelectorAll", function (selector) {
    // Browsers validate at call time: run the scoped query once so an
    // unparsable selector raises SyntaxError, then hand back the live list.
    host.__ironsurfDomQuery(this.__d, String(selector), this.__n);
    return makeList(this.__d, 3, this.__n, String(selector));
  });
  protoMethod("matches", function (selector) {
    return host.__ironsurfDomMatches(this.__d, this.__n, String(selector));
  });
  protoMethod("closest", function (selector) {
    return wrapNode(this.__d, host.__ironsurfDomClosest(this.__d, this.__n, String(selector)));
  });
  // contains/getRootNode/compareDocumentPosition (task: infosec.exchange):
  // the `inert` polyfill Mastodon ships walks focusability through exactly
  // this trio; their absence threw inside a dynamic import and killed the
  // module graph. Pure JS parent walks — no engine journal needed.
  protoMethod("contains", function (other) {
    var p = other;
    var guard = 0;
    while (p && guard++ < 8192) {
      if (p === this) return true;
      p = p.parentNode;
    }
    return false;
  });
  protoMethod("getRootNode", function () {
    var p = this;
    var guard = 0;
    while (p.parentNode && guard++ < 8192) p = p.parentNode;
    return p;
  });
  protoMethod("compareDocumentPosition", function (other) {
    if (other === null || other === undefined) {
      throw new TypeError("compareDocumentPosition requires a node");
    }
    if (other === this) return 0;
    // CONTAINS(4)|PRECEDING(16) === 20 what browsers report for ancestors.
    var p = other.parentNode;
    var guard = 0;
    while (p && guard++ < 8192) {
      if (p === this) return 20;
      p = p.parentNode;
    }
    p = this.parentNode;
    guard = 0;
    while (p && guard++ < 8192) {
      if (p === other) return 34; // CONTAINED_BY(32)|PRECEDING(2)
      p = p.parentNode;
    }
    // No ancestor relation the page can act on: report disconnected.
    return 9;
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
  // `HTMLElement.prototype.click` — real activation semantics: dispatch a
  // bubbling click through the capture/bubble path, then honour the default
  // action the page did not prevent (an `<a href>` navigates, same host hook
  // `location.href` assignment uses). Without this, `el.click()` — how every
  // SPA test-drive and cookie-banner dismissal works — is a missing property
  // (task: infosec.exchange).
  protoMethod("click", function () {
    var ev = makeEvent("click", this, { detail: 0 });
    firePath(eventPath(this), ev);
    if (ev.defaultPrevented) return;
    var tag = String(this.tagName || "").toUpperCase();
    if (tag === "A") {
      var href = this.getAttribute ? this.getAttribute("href") : null;
      if (href !== null && href !== "" && !/^javascript:/i.test(String(href))) {
        try {
          var target = new URL(String(href), host.location.href).href;
          host.__ironsurfNavigate(target, false);
        } catch (err) {
          /* unresolvable href: no default action, like the spec */
        }
      }
    }
  });
  // `focus()`/`blur()` — the trends hero's callback ref runs
  // `el.focus({ preventScroll: true })` on every mount; `focus` was not a
  // property of our elements, so the scheduled timer task threw
  // "not a callable function" straight through the column (task:
  // infosec.exchange). One focused surface, so this tracks a single element
  // and exposes it as `document.activeElement` (body when nothing is focused,
  // spec default). `preventScroll` is accepted but inert: this engine has no
  // JS-visible scroll model for script to move.
  protoMethod("focus", function (_options) {
    if (__focusedElement === this) return;
    __focusedElement = this;
    firePath(eventPath(this), makeEvent("focus", this));
  });
  protoMethod("blur", function () {
    if (__focusedElement !== this) return;
    __focusedElement = null;
    firePath(eventPath(this), makeEvent("blur", this));
  });
  // Scroll requests are accepted and inert: layout exists, but the realm has
  // no scroll-position model to move (and callers only care that it does not
  // throw).
  protoMethod("scrollIntoView", function (_arg) {});
  protoMethod("scroll", function (_arg) {});
  protoMethod("scrollTo", function (_arg) {});
  // getElementsByClassName on any element (live, like the document one).
  protoMethod("getElementsByClassName", function (name) {
    // Not scoped to descendants in this bridge — document-wide live list.
    return makeList(this.__d, 2, this.__n, String(name));
  });
  protoMethod("getElementsByTagName", function (name) {
    var tag = String(name).toLowerCase();
    return makeList(this.__d, tag === "*" ? 4 : 1, this.__n, tag);
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
      view[Symbol.iterator] = function () { return kids.slice()[Symbol.iterator](); };
      return view;
    },
    hasChildNodes: function () { return this.__frag.length > 0; },
    firstChild: function () {
      var k = this.__frag[0];
      return k === undefined ? null : k;
    },
    lastChild: function () {
      var k = this.__frag[this.__frag.length - 1];
      return k === undefined ? null : k;
    },
    firstElementChild: function () {
      for (var i = 0; i < this.__frag.length; i++)
        if (this.__frag[i].nodeType === 1) return this.__frag[i];
      return null;
    },
    lastElementChild: function () {
      for (var i = this.__frag.length - 1; i >= 0; i--)
        if (this.__frag[i].nodeType === 1) return this.__frag[i];
      return null;
    },
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
    Object.defineProperty(frag, "firstChild", { get: fragOverrides.firstChild });
    Object.defineProperty(frag, "lastChild", { get: fragOverrides.lastChild });
    Object.defineProperty(frag, "firstElementChild", { get: fragOverrides.firstElementChild });
    Object.defineProperty(frag, "lastElementChild", { get: fragOverrides.lastElementChild });
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

  // Template contents, keyed by "docId:nodeId" (wrappers are cached per
  // node, but the fragment must survive wrapper re-creations too). First
  // access adopts any children already under the element — markup parsed
  // with a <template> in it lands its children inside the element, and the
  // spec moves them into the content fragment at parse time.
  var templateFragCache = Object.create(null);
  function isTemplateElement(node) {
    return !!node.__n && node.tagName === "TEMPLATE";
  }
  function templateContents(el) {
    var key = el.__d + ":" + el.__n;
    var frag = templateFragCache[key];
    if (frag !== undefined) return frag;
    frag = fragPatch(makeFragment());
    var kids = el.childNodes;
    var moved = [];
    for (var i = 0; i < kids.length; i++) moved.push(kids[i]);
    for (var j = 0; j < moved.length; j++) frag.appendChild(moved[j]);
    templateFragCache[key] = frag;
    return frag;
  }
  // innerHTML= on a template parses the markup (blitz parses under any node,
  // attached or not) then MOVES the fresh children out of the scratch
  // container into the inert fragment — same parse, none of the nodes
  // rendered.
  function setTemplateContents(el, html) {
    var frag = templateContents(el);
    frag.__frag.length = 0;
    if (html === "") return;
    var scratch = document.createElement("div");
    host.__ironsurfDomSetInner(el.__d, scratch.__n, html);
    var kids = scratch.childNodes;
    var moved = [];
    for (var i = 0; i < kids.length; i++) moved.push(kids[i]);
    for (var j = 0; j < moved.length; j++) frag.appendChild(moved[j]);
  }

  // The document object. When the tab registers a live blitz document the
  // accessors below read through it; with no live document (the plain host
  // snapshot world the bindings tests use) they fall back to host state.
  function bodyFallback() {
    return host.__ironsurfHasBody
      ? { nodeType: 1, textContent: host.__ironsurfBodyText }
      : null;
  }

  // document.write/writeln: the insertion cursor into the parse stream.
  // Scripts run after the parse, so the cursor is the end of the tree — the
  // native really parses the written markup and appends it to the live
  // document (visible in the DOM, laid out and painted on the next settle).
  // With no live document (the snapshot world) this is a no-op.
  function writeMarkup(args, newline) {
    var s = "";
    for (var i = 0; i < args.length; i++) s += String(args[i]);
    if (newline) s += "\n";
    if (host.__ironsurfDomHasDoc()) host.__ironsurfDomDocumentWrite(0, s);
  }

  var document = {
    nodeType: 9,
    __d: 0,
    readyState: "complete",
    visibilityState: "visible",
    hidden: false,
    // This tab always holds focus in IronSurf's single-window model, and no
    // script can take it back, so the honest answer is `true`.
    hasFocus: function () {
      return true;
    },
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
    // document.dir / document.lang mirror the <html> element's attributes
    // (per spec they ARE those attributes), so JS setting them — like every
    // other document-attribute mutation — rides the ATTR_SET native and
    // restyles on the next settle.
    get dir() {
      var el = this.documentElement;
      var v = el === null ? null : el.getAttribute("dir");
      return v === null ? "" : v;
    },
    set dir(value) {
      var el = this.documentElement;
      if (el !== null) el.setAttribute("dir", String(value));
    },
    get lang() {
      var el = this.documentElement;
      var v = el === null ? null : el.getAttribute("lang");
      return v === null ? "" : v;
    },
    set lang(value) {
      var el = this.documentElement;
      if (el !== null) el.setAttribute("lang", String(value));
    },
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
    createElementNS: function (ns, tag) {
      // React-dom creates every <svg>/<math> through this single entry
      // (mastodon's landing renders SVG icons before anything else, and the
      // missing method took the whole tree down behind an error boundary).
      // The namespace lands on the blitz element's QualName namespace.
      return wrapNode(0, host.__ironsurfDomCreateElementNs(0, String(ns), String(tag)));
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
    write: function () { writeMarkup(arguments, false); },
    writeln: function () { writeMarkup(arguments, true); },
    get title() {
      return host.__ironsurfTitleGet();
    },
    set title(value) {
      host.__ironsurfTitleSet(String(value));
    },
    // document.cookie (task: the axios XSRF read): both directions funnel
    // through the ONE cookies native. Reads answer the container jar's
    // script-visible view for THIS document URL (same domain/path/Secure
    // matching the wire "Cookie:" header uses, HttpOnly entries hidden);
    // writes ride the jar's RFC6265 parser, with the native stripping any
    // HttpOnly attribute a page tries to set. Cookieless documents (opaque
    // origin, no jar) read "" and silently drop writes — never `undefined`,
    // which is exactly what made axios die.
    get cookie() {
      return String(host.__ironsurfCookies("get", null));
    },
    set cookie(value) {
      host.__ironsurfCookies("set", String(value));
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

  // ---------------------------------------------------------------------
  // HTMLImageElement glue + the `Image` constructor
  //
  // `new Image()` is the oldest image API on the web and pages reach for it
  // unconditionally (`var i = new Image(); i.onload = …; i.src = url;`), so
  // its absence threw `ReferenceError: Image is not defined` out of the whole
  // script. It creates an `<img>` ELEMENT through exactly the same native as
  // `document.createElement("img")` and leaves it detached — `isConnected ===
  // false`, like a real one — and `instanceof HTMLImageElement` holds because
  // the interface prototype IS the wrapper prototype (in browsers
  // `Image.prototype === HTMLImageElement.prototype`).
  //
  // Two things come with it, both on the wrapper prototype like the media
  // members above, and both only as honest as the engine can be:
  //
  // * Reflected URL attributes. HTML reflects `src`/`href` as URLs: the getter
  //   answers the url RESOLVED AGAINST THE DOCUMENT URL (only the tab owns a
  //   URL parser — `__ironsurfDomResolveUrl`), the setter writes the content
  //   attribute through the same native as `setAttribute`, so a script-built
  //   `<img>` is fetched by blitz through the tab's own network layer the
  //   moment it enters the tree (blitz requests `<img src>` on insertion).
  //   IronSurf has no detached-image preload: `new Image(); i.src = …` on its
  //   own starts no fetch and never fires `load` — append the image and it
  //   loads for real (the natural follow-up task).
  // * The sizing members. `width`/`height` read the content attributes as
  //   HTML's reflected non-negative integers, falling back — per the IDL — to
  //   the image's intrinsic size and finally 0; `naturalWidth`/`naturalHeight`/
  //   `complete`/`currentSrc` read what the engine reported about the bytes.
  //
  // Both step aside on tags that have no such IDL member: `div.href = "x"`
  // stores a string and reads back a string (browsers store an own property),
  // and neither one navigates.
  // ---------------------------------------------------------------------
  var SRC_TAGS = { IMG: 1, INPUT: 1, SCRIPT: 1, IFRAME: 1, AUDIO: 1, VIDEO: 1, SOURCE: 1, TRACK: 1, EMBED: 1 };
  var HREF_TAGS = { A: 1, AREA: 1, LINK: 1, BASE: 1 };
  function tagUpper(self) {
    var name = self.nodeName;
    return typeof name === "string" ? name.toUpperCase() : "";
  }
  function isImg(self) { return tagUpper(self) === "IMG"; }
  function storeOwn(self, key, value) {
    // Browsers let a page hang arbitrary data on an element: where there is no
    // reflected member the accessor steps aside and the value is stored.
    Object.defineProperty(self, key, {
      value: value, writable: true, enumerable: true, configurable: true,
    });
  }
  // The reflected-URL getter: "" when the attribute is absent or empty (the
  // spec default), the ABSOLUTE url otherwise, and the raw text when even the
  // tab's parser cannot resolve it (a base-less document).
  function urlAttr(self, key, tags) {
    if (!tags[tagUpper(self)]) return undefined;
    var raw = self.getAttribute(key);
    if (raw === null || raw === undefined) return "";
    raw = String(raw).trim();
    if (raw === "") return "";
    var absolute = host.__ironsurfDomResolveUrl(self.__d, raw);
    return absolute === null ? raw : String(absolute);
  }
  function urlAttrSet(self, key, tags, value) {
    if (!tags[tagUpper(self)]) {
      storeOwn(self, key, String(value));
      return;
    }
    self.setAttribute(key, String(value));
  }
  protoAccessor("src",
    function () { return urlAttr(this, "src", SRC_TAGS); },
    function (value) { urlAttrSet(this, "src", SRC_TAGS, value); });
  protoAccessor("href",
    function () { return urlAttr(this, "href", HREF_TAGS); },
    function (value) { urlAttrSet(this, "href", HREF_TAGS, value); });
  // Leading non-negative integer (HTML's parsing rules: optional whitespace,
  // then digits — anything else means the attribute has no value).
  function nonNegativeInt(raw, fallback) {
    var text = raw === null || raw === undefined ? "" : String(raw);
    var match = /^[ \t\r\n\f]*([0-9]+)/.exec(text);
    return match === null ? fallback : parseInt(match[1], 10);
  }
  // `[loaded, naturalWidth, naturalHeight]` from the engine, or null when this
  // element has no image (never fetched, failed, or not an <img> at all).
  function imageState(self) {
    var state = host.__ironsurfDomImageState(self.__d, self.__n);
    return state && state[0] === true ? state : null;
  }
  function intrinsic(self, index) {
    var state = imageState(self);
    return state === null ? 0 : Math.round(Number(state[index]));
  }
  // <canvas> width/height: content attributes (unsigned longs), falling back
  // to the HTML defaults when the attribute is absent or garbage.
  function canvasSize(self, key, fallback) {
    var declared = nonNegativeInt(self.getAttribute(key), -1);
    return declared >= 0 ? declared : fallback;
  }
  function isCanvasTag(self) { return tagUpper(self) === "CANVAS"; }
  function sizeAttrSet(self, key, value) {
    if (!isImg(self) && !isCanvasTag(self)) {
      storeOwn(self, key, String(value));
      return;
    }
    var n = Number(value);
    if (!isFinite(n)) n = 0;      // garbage reads as 0 (WebIDL unsigned long)
    n = Math.floor(n);
    self.setAttribute(key, String(n < 0 ? 0 : n));
  }
  // `width`/`height` on `<img>` are the RENDERED size, which falls back to the
  // intrinsic size while the element is not laid out (no layout boxes live on
  // this side of the bridge, so the declared attribute is the best answer for
  // what the engine would render).
  protoAccessor("width",
    function () {
      // <canvas> reflects width/height with the HTML defaults 300×150.
      if (isCanvasTag(this)) return canvasSize(this, "width", 300);
      if (!isImg(this)) return undefined;
      var declared = nonNegativeInt(this.getAttribute("width"), -1);
      return declared >= 0 ? declared : intrinsic(this, 1);
    },
    function (value) { sizeAttrSet(this, "width", value); });
  protoAccessor("height",
    function () {
      if (isCanvasTag(this)) return canvasSize(this, "height", 150);
      if (!isImg(this)) return undefined;
      var declared = nonNegativeInt(this.getAttribute("height"), -1);
      return declared >= 0 ? declared : intrinsic(this, 2);
    },
    function (value) { sizeAttrSet(this, "height", value); });
  protoAccessor("naturalWidth",
    function () { return isImg(this) ? intrinsic(this, 1) : undefined; });
  protoAccessor("naturalHeight",
    function () { return isImg(this) ? intrinsic(this, 2) : undefined; });
  protoAccessor("complete",
    function () {
      if (!isImg(this)) return undefined;
      // HTML: `complete` is true when there is no image to wait for at all
      // (no src, or the empty string), and while the resource is still
      // travelling it is false. A failed fetch cannot be told apart from a
      // pending one from this side, so it keeps reading false.
      var raw = this.getAttribute("src");
      if (raw === null || String(raw).trim() === "") return true;
      return imageState(this) !== null;
    });
  protoAccessor("currentSrc",
    function () { return isImg(this) ? urlAttr(this, "src", SRC_TAGS) : undefined; });
  // `decode()` resolves once the image is decoded; IronSurf decodes as part of
  // fetching, so a settled image decodes immediately.
  protoMethod("decode", function () {
    if (!isImg(this)) return undefined;
    return Promise.resolve();
  });
  // WebIDL `unsigned long` coercion for the factory arguments: garbage and
  // negatives read as 0, fractions truncate.
  function unsignedLong(value) {
    var n = Number(value);
    if (!isFinite(n)) return 0;
    n = Math.floor(n);
    return n < 0 ? 0 : n;
  }
  // The interface itself (what `instanceof` consults) and its legacy factory.
  function HTMLImageElement() {}
  HTMLImageElement.prototype = NodeProto;
  // `new Image([width[, height]])`: an <img> element, and the content
  // attributes are written ONLY when the argument is present — `new Image()`
  // yields a plain <img> with no width/height at all.
  function Image(width, height) {
    var img = document.createElement("img");
    if (img !== null) {
      if (width !== undefined) img.setAttribute("width", String(unsignedLong(width)));
      if (height !== undefined) img.setAttribute("height", String(unsignedLong(height)));
    }
    return img;
  }
  Image.prototype = NodeProto; // === HTMLImageElement.prototype, like browsers
  globalThis.HTMLImageElement = HTMLImageElement;
  globalThis.Image = Image;

  // ---------------------------------------------------------------------
  // HTMLCanvasElement glue + an honest transparent-black 2D context
  // (task: infosec.exchange without console errors).
  //
  // Mastodon renders every custom-emoji blur placeholder through a real
  // <canvas>: the decode itself is pure JS (fine in boa), but the paint path
  // is `canvas.getContext("2d").putImageData(createImageData(w,h), 0, 0)` —
  // with no getContext on the element the CALL threw "not a callable
  // function", the app caught it and logged `Blurhash decoding failure` at
  // console.error on every image-bearing timeline item.
  //
  // There is no GPU and no rasteriser on this side of the bridge, so the
  // context is spec-shaped but draws nothing: state members are plain
  // properties, drawing methods are no-ops, getImageData answers transparent
  // black — exactly what a canvas nobody painted on reads back, so the
  // surface never lies about pixels that were never composited. Blurhash's
  // `createImageData(...).data.set(pixels); putImageData(imgData, 0, 0)`
  // completes silently instead of erroring; the placeholder stays blank.
  // `webgl`/`webgl2` answer null (no GPU, like a browser with it disabled);
  // `bitmaprenderer` answers null too (ImageBitmap is not implemented).
  // toDataURL/toBlob are deliberately absent: rendering a PNG of our own is
  // the honest follow-up, and a fake bytes-free data URL would lie.
  // ---------------------------------------------------------------------
  function canvasImageData(width, height) {
    var w = unsignedLong(width);
    var h = unsignedLong(height);
    return { width: w, height: h, data: new Uint8ClampedArray(w * h * 4), colorSpace: "srgb" };
  }
  function canvasContext2D() {
    var noop = function () {};
    return {
      // canvas back-reference is written by getContext (the element wrapper).
      canvas: null,
      fillStyle: "#000000", strokeStyle: "#000000",
      globalAlpha: 1, globalCompositeOperation: "source-over",
      lineWidth: 1, lineCap: "butt", lineJoin: "miter", miterLimit: 10,
      shadowBlur: 0, shadowColor: "rgba(0, 0, 0, 0)", shadowOffsetX: 0, shadowOffsetY: 0,
      font: "10px sans-serif", textAlign: "start", textBaseline: "alphabetic",
      direction: "inherit", imageSmoothingEnabled: true, imageSmoothingQuality: "low",
      filter: "none",
      save: noop, restore: noop,
      scale: noop, rotate: noop, translate: noop, transform: noop,
      setTransform: noop, resetTransform: noop,
      beginPath: noop, closePath: noop, moveTo: noop, lineTo: noop,
      bezierCurveTo: noop, quadraticCurveTo: noop, arc: noop, arcTo: noop,
      ellipse: noop, rect: noop, roundRect: noop,
      fill: noop, stroke: noop, clip: noop,
      fillRect: noop, strokeRect: noop, clearRect: noop,
      fillText: noop, strokeText: noop,
      setLineDash: noop, getLineDash: function () { return []; },
      drawImage: noop, drawFocusIfNeeded: noop,
      isPointInPath: function () { return false; },
      isPointInStroke: function () { return false; },
      measureText: function (text) {
        // No font engine here: every box reads zero-width.
        return {
          width: 0, actualBoundingBoxLeft: 0, actualBoundingBoxRight: 0,
          actualBoundingBoxAscent: 0, actualBoundingBoxDescent: 0,
          fontBoundingBoxAscent: 0, fontBoundingBoxDescent: 0,
          emHeightAscent: 0, emHeightDescent: 0, alphabeticBaseline: 0,
          hangingBaseline: 0, ideographicBaseline: 0,
          text: text === undefined ? "" : String(text),
        };
      },
      createLinearGradient: function () { return { addColorStop: noop }; },
      createRadialGradient: function () { return { addColorStop: noop }; },
      createConicGradient: function () { return { addColorStop: noop }; },
      createPattern: function () { return null; },
      createImageData: function (a, b) {
        // The spec allows createImageData(imageData) or (w, h).
        if (a !== null && typeof a === "object" && typeof a.width === "number") {
          return canvasImageData(a.width, a.height);
        }
        return canvasImageData(a, b);
      },
      getImageData: function (sx, sy, sw, sh) {
        // Nothing has ever been composited into this context, so reading any
        // rect back answers transparent black — which is what the fresh
        // all-zero buffer is.
        return canvasImageData(sw, sh);
      },
      putImageData: noop,
    };
  }
  protoMethod("getContext", function (type) {
    if (!isCanvasTag(this)) return undefined;
    var kind = String(type === undefined ? "" : type).toLowerCase();
    if (kind !== "2d") return null; // no webgl (no GPU), no bitmaprenderer
    var ctx = canvasContext2D();
    ctx.canvas = this;
    return ctx;
  });
  protoMethod("getContextAttributes", function () {
    if (!isCanvasTag(this)) return undefined;
    return {
      alpha: true, depth: false, stencil: false, antialias: false,
      desynchronized: false, willReadFrequently: false, colorSpace: "srgb",
      pixelFormat: "RGBA8",
    };
  });

  function makeConsole(level) {
    return function () {
      // ToString per argument, except Errors: those print with their stack,
      // which is the difference between "console.error(err) logged nothing
      // useful" and a debuggable page console (Mastodon's error boundary
      // re-logs every render throw this way).
      var parts = [];
      for (var i = 0; i < arguments.length; i++) {
        var a = arguments[i];
        if (a instanceof Error && typeof a.stack === "string" && a.stack !== "") {
          parts.push(String(a.name) + ": " + String(a.message) + "\n" + a.stack);
        } else if (typeof a === "symbol") {
          parts.push(String(a));
        } else {
          try {
            parts.push(String(a));
          } catch (ignored) {
            parts.push("[unprintable]");
          }
        }
      }
      host.__ironsurfConsole(level, parts.join(" "));
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

  // ---------------------------------------------------------------------
  // window.alert / confirm / prompt (task: JS dialogs). The native blocks
  // the calling script right where it stands — real modal semantics — until
  // the shell's dialog is dismissed, then answers: alert `undefined`,
  // confirm the button, prompt the text or `null` (cancelled). With no
  // dialog host (headless, bare tests) the native answers cancelled at once.
  // ---------------------------------------------------------------------
  function dialogMessage(value) {
    return value === undefined ? "" : String(value);
  }
  host.alert = function (message) {
    host.__ironsurfDialog("alert", dialogMessage(message), null);
  };
  host.confirm = function (message) {
    return host.__ironsurfDialog("confirm", dialogMessage(message), null) === true;
  };
  host.prompt = function (message, def) {
    // Spec: `prompt(msg)` prefills the empty string.
    var fallback = def === undefined || def === null ? "" : String(def);
    return host.__ironsurfDialog("prompt", dialogMessage(message), fallback);
  };

  // ---------------------------------------------------------------------
  // window.localStorage (task: per-origin DOM storage). The area lookup —
  // and the whole per-origin security rule — lives in the Rust native:
  // every operation re-resolves the document origin from the host URL at
  // CALL time (never cached here), documents without a durable origin
  // (about:, data:, file:, javascript:…) get a SecurityError out of every
  // access, and a 5 MiB per-origin quota surfaces as QuotaExceededError.
  // The object is a Proxy so `localStorage.foo = 1` stores like setItem
  // and `Object.keys(localStorage)` enumerates the stored names, per spec.
  // ---------------------------------------------------------------------
  function storageError(name, message) {
    var e = new Error(message);
    e.name = name;
    return e;
  }
  function storageKeyArg(value) {
    return value === undefined ? "undefined" : String(value);
  }
  function storageCall(op, key, value, index) {
    return host.__ironsurfStorage(op, key === undefined ? null : key, value === undefined ? null : value, index === undefined ? -1 : index);
  }
  function storageUnwrap(res, what) {
    if (res[0] === "denied") {
      throw storageError(
        "SecurityError",
        "The document has no storage origin; localStorage is not available (" + what + " denied)"
      );
    }
    return res[1];
  }
  var localStorage = (function () {
    var api = {
      getItem: function (key) {
        var value = storageUnwrap(storageCall("get", storageKeyArg(key)), "getItem");
        return value === null || value === undefined ? null : String(value);
      },
      setItem: function (key, value) {
        var res = storageCall("set", storageKeyArg(key), value === undefined ? "undefined" : String(value));
        if (res[0] === "denied") {
          throw storageError("SecurityError", "The document has no storage origin; localStorage is not available (setItem denied)");
        }
        if (res[0] === "quota") {
          throw storageError("QuotaExceededError", "localStorage quota exceeded for this origin (5 MiB of names + values)");
        }
      },
      removeItem: function (key) {
        return storageUnwrap(storageCall("remove", storageKeyArg(key)), "removeItem") === true;
      },
      clear: function () {
        storageUnwrap(storageCall("clear"), "clear");
      },
      key: function (index) {
        var i = Math.floor(Number(index));
        if (!isFinite(i) || i < 0) return null;
        var name = storageUnwrap(storageCall("key", null, null, i), "key");
        return name === null || name === undefined ? null : String(name);
      },
    };
    function storageLen() {
      return Number(storageUnwrap(storageCall("len"), "length"));
    }
    var handlers = {
      get: function (t, k) {
        if (typeof k === "symbol") return t[k];
        if (k === "length") return storageLen();
        if (Object.prototype.hasOwnProperty.call(t, k)) return t[k];
        return api.getItem(k);
      },
      // `localStorage.foo = v` stores (spec: the Proxy treats every unknown
      // name as an item); `delete localStorage.foo` removes it.
      set: function (t, k, v) {
        if (typeof k === "symbol" || Object.prototype.hasOwnProperty.call(t, k) || k === "length") {
          return Reflect.set(t, k, v);
        }
        api.setItem(k, v);
        return true;
      },
      has: function (t, k) {
        if (typeof k === "symbol") return false;
        if (k === "length" || Object.prototype.hasOwnProperty.call(t, k)) return true;
        return api.getItem(k) !== null;
      },
      deleteProperty: function (t, k) {
        if (typeof k === "symbol" || Object.prototype.hasOwnProperty.call(t, k)) {
          return Reflect.deleteProperty(t, k);
        }
        api.removeItem(k);
        return true;
      },
      ownKeys: function (t) {
        var res = storageCall("keys");
        if (res[0] === "denied") return Object.keys(t);
        var names = Object.keys(t);
        var items = res[1];
        for (var i = 0; i < items.length; i++) names.push(String(items[i]));
        return names;
      },
      getOwnPropertyDescriptor: function (t, k) {
        if (k === "length") {
          return { value: storageLen(), writable: false, enumerable: false, configurable: false };
        }
        if (Object.prototype.hasOwnProperty.call(t, k)) {
          return { value: t[k], writable: true, enumerable: false, configurable: true };
        }
        if (typeof k === "symbol") return undefined;
        var v = api.getItem(k);
        if (v === null) return undefined;
        return { value: v, writable: true, enumerable: true, configurable: true };
      },
    };
    return new Proxy(api, handlers);
  })();
  host.localStorage = localStorage;

  // ---------------------------------------------------------------------
  // URLSearchParams (task: infosec.exchange / Mastodon). The whole WHATWG
  // surface over a live pair list: the four constructor inits (string,
  // iterable of pairs, record, another URLSearchParams), append/get/getAll/
  // has/set/delete/sort/keys/values/entries/forEach/size and the stringifier
  // (application/x-www-form-urlencoded: alphanumeric + `*-._` stay literal,
  // space becomes `+`). Iteration results are one-shot iterators in spec
  // shape ({ done, value }). Boa has no `URLSearchParams` in `boa_runtime`
  // (only `URL`), and Mastodon's polyfill bundle dies at construction time
  // without it, which leaves the whole SPA on its <noscript> fallback.
  // ---------------------------------------------------------------------
  function formEncodeComponent(s) {
    return encodeURIComponent(String(s))
      .replace(/'/g, "%27")
      .replace(/!/g, "%21")
      .replace(/\(/g, "%28")
      .replace(/\)/g, "%29")
      .replace(/~/g, "%7E")
      .replace(/\*/g, "%2A")
      .replace(/\+/g, "%2B")
      .replace(/%20/g, "+");
  }
  function formDecodeComponent(s) {
    var text = String(s).replace(/\+/g, " ");
    try {
      return decodeURIComponent(text);
    } catch (err) {
      return text; // a malformed % escape stays literal (spec)
    }
  }
  function URLSearchParams(init) {
    var pairs = [];
    Object.defineProperty(this, "__pairs", { value: pairs });
    if (init === undefined || init === null) return;
    if (typeof init === "string") {
      parseInto(pairs, init);
      return;
    }
    if (init instanceof URLSearchParams) {
      init.forEach(function (v, k) { pairs.push([k, v]); });
      return;
    }
    if (typeof init[Symbol.iterator] === "function") {
      var it = init[Symbol.iterator]();
      for (;;) {
        var step = it.next();
        if (step.done) break;
        var pair = step.value;
        if (typeof pair[Symbol.iterator] !== "function") {
          throw new TypeError("Expected a two-item iterable as init pair");
        }
        var parts = Array.from(pair);
        if (parts.length !== 2) {
          throw new TypeError("Expected a two-item iterable as init pair");
        }
        pairs.push([String(parts[0]), String(parts[1])]);
      }
      return;
    }
    if (typeof init === "object") {
      Object.keys(init).forEach(function (k) {
        pairs.push([String(k), String(init[k])]);
      });
    }
  }
  function parseInto(pairs, query) {
    var text = String(query);
    if (text.charAt(0) === "?") text = text.slice(1);
    if (text === "") return;
    text.split("&").forEach(function (chunk) {
      if (chunk === "") return;
      var eq = chunk.indexOf("=");
      if (eq < 0) pairs.push([formDecodeComponent(chunk), ""]);
      else pairs.push([formDecodeComponent(chunk.slice(0, eq)), formDecodeComponent(chunk.slice(eq + 1))]);
    });
  }
  function makeListIterator(pairs, kind) {
    var index = 0;
    var iterator = {
      next: function () {
        if (index >= pairs.length) {
          index = pairs.length;
          return { done: true, value: undefined };
        }
        var pair = pairs[index++];
        return {
          done: false,
          value: kind === "key" ? pair[0] : kind === "value" ? pair[1] : [pair[0], pair[1]],
        };
      },
    };
    iterator[Symbol.iterator] = function () { return iterator; };
    return iterator;
  }
  URLSearchParams.prototype = {
    constructor: URLSearchParams,
    append: function (name, value) {
      this.__pairs.push([String(name), String(value)]);
    },
    get: function (name) {
      name = String(name);
      for (var i = 0; i < this.__pairs.length; i++) {
        if (this.__pairs[i][0] === name) return this.__pairs[i][1];
      }
      return null;
    },
    getAll: function (name) {
      name = String(name);
      var out = [];
      for (var i = 0; i < this.__pairs.length; i++) {
        if (this.__pairs[i][0] === name) out.push(this.__pairs[i][1]);
      }
      return out;
    },
    has: function (name, value) {
      name = String(name);
      var byValue = value !== undefined;
      for (var i = 0; i < this.__pairs.length; i++) {
        if (this.__pairs[i][0] !== name) continue;
        if (!byValue || this.__pairs[i][1] === String(value)) return true;
      }
      return false;
    },
    set: function (name, value) {
      name = String(name);
      value = String(value);
      for (var i = 0; i < this.__pairs.length; i++) {
        if (this.__pairs[i][0] === name) {
          this.__pairs[i][1] = value;
          while (this.__pairs.length > i + 1) {
            var found = -1;
            for (var j = i + 1; j < this.__pairs.length; j++) {
              if (this.__pairs[j][0] === name) { found = j; break; }
            }
            if (found < 0) break;
            this.__pairs.splice(found, 1);
          }
          return;
        }
      }
      this.__pairs.push([name, value]);
    },
    delete: function (name, value) {
      name = String(name);
      var byValue = value !== undefined;
      for (var i = this.__pairs.length - 1; i >= 0; i--) {
        var pair = this.__pairs[i];
        if (pair[0] === name && (!byValue || pair[1] === String(value))) {
          this.__pairs.splice(i, 1);
        }
      }
    },
    sort: function () {
      // Stable sort by name (spec: SortParameters by ascending code units).
      var indexed = this.__pairs.map(function (pair, i) { return [pair, i]; });
      indexed.sort(function (a, b) {
        if (a[0][0] < b[0][0]) return -1;
        if (a[0][0] > b[0][0]) return 1;
        return a[1] - b[1];
      });
      this.__pairs.splice(0, this.__pairs.length);
      indexed.forEach(function (entry) { this.__pairs.push(entry[0]); }, this);
    },
    replace: function (init) {
      var fresh = new URLSearchParams(init);
      this.__pairs.splice(0, this.__pairs.length);
      fresh.__pairs.forEach(function (pair) { this.__pairs.push(pair); }, this);
    },
    forEach: function (cb, thisArg) {
      // Snapshot: the callback may mutate the live list (spec iterates a copy).
      this.__pairs.slice().forEach(function (pair) {
        cb.call(thisArg, pair[1], pair[0], this);
      }, this);
    },
    keys: function () { return makeListIterator(this.__pairs, "key"); },
    values: function () { return makeListIterator(this.__pairs, "value"); },
    entries: function () { return makeListIterator(this.__pairs, "entry"); },
    toString: function () {
      return this.__pairs
        .map(function (pair) { return formEncodeComponent(pair[0]) + "=" + formEncodeComponent(pair[1]); })
        .join("&");
    },
    get size() {
      return this.__pairs.length;
    },
  };
  URLSearchParams.prototype[Symbol.iterator] = URLSearchParams.prototype.entries;
  host.URLSearchParams = URLSearchParams;

  // ---------------------------------------------------------------------
  // Event / CustomEvent / EventTarget (task: infosec.exchange). `Event`
  // was a plain object made by makeEvent() — pages that `new Event(...)`,
  // `class X extends Event`, or feature-check `Event.prototype` died with
  // "Event is not defined" (Mastodon's common chunk does, at module scope,
  // which kills the whole boot chain). These classes read/write exactly
  // the properties the dispatcher above uses (`_stop`, `defaultPrevented`
  // included), so a real `new Event(...)` flows through the same capture/
  // bubble machinery as the synthetic ones. makeEvent() now produces
  // instances of Event.prototype, so `ev instanceof Event` holds for
  // every event the page ever sees.
  // ---------------------------------------------------------------------
  function Event(type, init) {
    if (type === undefined || type === null) {
      throw new TypeError("Failed to construct 'Event': 1 argument required");
    }
    type = String(type);
    if (type === "") {
      throw new TypeError("Failed to construct 'Event': The type provided is an empty String");
    }
    init = init || {};
    this.type = type;
    this.bubbles = !!init.bubbles;
    this.cancelable = !!init.cancelable;
    this.composed = !!init.composed;
    this.defaultPrevented = false;
    this.eventPhase = 0; // NONE until dispatched
    this.isTrusted = false;
    this.target = null;
    this.currentTarget = null;
    this.timeStamp = Date.now();
  }
  Event.NONE = 0;
  Event.CAPTURING_PHASE = 1;
  Event.AT_TARGET = 2;
  Event.BUBBLING_PHASE = 3;
  Event.prototype.constructor = Event;
  Event.prototype.preventDefault = function () {
    if (this.cancelable) this.defaultPrevented = true;
  };
  Event.prototype.stopPropagation = function () {
    this._stop = true;
  };
  Event.prototype.stopImmediatePropagation = function () {
    this._stop = true;
    this._stopImmediate = true;
  };
  Event.prototype.composedPath = function () {
    return this.__composedPath || [];
  };
  function CustomEvent(type, init) {
    Event.call(this, type, init);
    init = init || {};
    this.detail = init.detail === undefined ? null : init.detail;
  }
  CustomEvent.prototype = Object.create(Event.prototype);
  CustomEvent.prototype.constructor = CustomEvent;
  function EventTarget() {}
  EventTarget.prototype.addEventListener = function (type, fn, options) {
    addListener(this, String(type), fn, options);
  };
  EventTarget.prototype.removeEventListener = function (type, fn, options) {
    removeListener(this, String(type), fn, options);
  };
  EventTarget.prototype.dispatchEvent = function (ev) {
    if (!ev || typeof ev.type !== "string") {
      throw new TypeError("parameter 1 is not of type 'Event'");
    }
    var target = this === EventTarget.prototype ? host : this;
    ev.target = target;
    ev.currentTarget = target;
    if (target.__n) {
      firePath(eventPath(target), ev); // DOM node: real capture/bubble
    } else {
      ev.eventPhase = 2;
      visitNode(target, ev, "target"); // standalone target: own listeners only
    }
    return !ev.defaultPrevented;
  };
  host.Event = Event;
  host.CustomEvent = CustomEvent;
  host.EventTarget = EventTarget;

  // UIEvent / MouseEvent / KeyboardEvent constructors (task: infosec.exchange
  // explore tabs). Page code — and every "drive the page from JS" pattern —
  // builds real interaction events; without these constructors `new
  // MouseEvent(...)` is a ReferenceError and synthetic activation dies
  // before it reaches the app's listeners. Instances are genuine `Event`
  // objects (same makeEvent base, so propagation internals match) with the
  // init dictionary layered on top.
  function eventWith(proto, type, init) {
    var base = makeEvent(String(type), null, init);
    var ev = Object.create(proto);
    Object.keys(base).forEach(function (k) { ev[k] = base[k]; });
    ev.target = null;
    ev.currentTarget = null;
    return ev;
  }
  function UIEvent(type, init) {
    var ev = eventWith(UIEvent.prototype, type, init || {});
    ev.detail = (init && typeof init.detail === "number") ? init.detail : 0;
    ev.view = (init && init.view) || host;
    return ev;
  }
  UIEvent.prototype = Object.create(Event.prototype);
  UIEvent.prototype.constructor = UIEvent;
  function MouseEvent(type, init) {
    init = init || {};
    var ev = eventWith(MouseEvent.prototype, type, init);
    ev.detail = typeof init.detail === "number" ? init.detail : 0;
    ev.view = init.view || host;
    ev.screenX = init.screenX || 0;
    ev.screenY = init.screenY || 0;
    ev.clientX = init.clientX || 0;
    ev.clientY = init.clientY || 0;
    ev.ctrlKey = !!init.ctrlKey;
    ev.shiftKey = !!init.shiftKey;
    ev.altKey = !!init.altKey;
    ev.metaKey = !!init.metaKey;
    ev.button = init.button || 0;
    ev.buttons = init.buttons || 0;
    ev.relatedTarget = init.relatedTarget || null;
    ev.which = ev.button;
    return ev;
  }
  MouseEvent.prototype = Object.create(UIEvent.prototype);
  MouseEvent.prototype.constructor = MouseEvent;
  function KeyboardEvent(type, init) {
    init = init || {};
    var ev = eventWith(KeyboardEvent.prototype, type, init);
    ev.detail = typeof init.detail === "number" ? init.detail : 0;
    ev.view = init.view || host;
    ev.key = init.key === undefined ? "" : String(init.key);
    ev.code = init.code === undefined ? "" : String(init.code);
    ev.location = init.location || 0;
    ev.repeat = !!init.repeat;
    ev.isComposing = !!init.isComposing;
    ev.ctrlKey = !!init.ctrlKey;
    ev.shiftKey = !!init.shiftKey;
    ev.altKey = !!init.altKey;
    ev.metaKey = !!init.metaKey;
    ev.getModifierState = function (name) {
      name = String(name).toLowerCase();
      if (name === "control") return ev.ctrlKey;
      if (name === "shift") return ev.shiftKey;
      if (name === "alt") return ev.altKey;
      if (name === "meta") return ev.metaKey;
      return false;
    };
    return ev;
  }
  KeyboardEvent.prototype = Object.create(UIEvent.prototype);
  KeyboardEvent.prototype.constructor = KeyboardEvent;
  host.UIEvent = UIEvent;
  host.MouseEvent = MouseEvent;
  host.KeyboardEvent = KeyboardEvent;

  // ---------------------------------------------------------------------
  // DOMException (task: infosec.exchange). Missing entirely — and core-js
  // (every modern bundle's polyfill layer) resolves `getBuiltIn
  // ('DOMException')` to undefined, then aborts the WHOLE polyfill module
  // on `wrapper.prototype = undefined.prototype` with Boa's "cannot convert
  // 'null' or 'undefined' to object". Spec shape: extends Error, the
  // (message, { name } | name) constructor forms, the legacy `code` digit,
  // the INDEX_SIZE_ERR… constants on both the constructor and the
  // prototype, and no own `stack` (matching browsers for the feature probe
  // core-js uses here).
  // ---------------------------------------------------------------------
  var DOM_EXCEPTION_CODES = {
    IndexSizeError: 1, HierarchyRequestError: 3, WrongDocumentError: 4,
    InvalidCharacterError: 5, NoModificationAllowedError: 7, NotFoundError: 8,
    NotSupportedError: 9, InvalidStateError: 11, SyntaxError: 12,
    InvalidModificationError: 13, NamespaceError: 14, InvalidAccessError: 15,
    TypeMismatchError: 17, SecurityError: 18, NetworkError: 19, AbortError: 20,
    URLMismatchError: 21, QuotaExceededError: 22, TimeoutError: 23,
    InvalidNodeTypeError: 24, DataCloneError: 25,
  };
  function DOMException(message, options) {
    var text = message === undefined ? "" : String(message);
    // The dictionary form and the legacy name-string form agree here.
    var exceptionName = "Error";
    if (typeof options === "string") exceptionName = options;
    else if (options && typeof options === "object" && options.name !== undefined)
      exceptionName = String(options.name);
    this.message = text;
    this.name = exceptionName;
    this.code = DOM_EXCEPTION_CODES[exceptionName] || 0;
  }
  DOMException.prototype = Object.create(Error.prototype);
  DOMException.prototype.constructor = DOMException;
  (function () {
    var legacy = {
      INDEX_SIZE_ERR: 1, DOMSTRING_SIZE_ERR: 2, HIERARCHY_REQUEST_ERR: 3,
      WRONG_DOCUMENT_ERR: 4, INVALID_CHARACTER_ERR: 5, NO_DATA_ALLOWED_ERR: 6,
      NO_MODIFICATION_ALLOWED_ERR: 7, NOT_FOUND_ERR: 8, NOT_SUPPORTED_ERR: 9,
      INUSE_ATTRIBUTE_ERR: 10, INVALID_STATE_ERR: 11, SYNTAX_ERR: 12,
      INVALID_MODIFICATION_ERR: 13, NAMESPACE_ERR: 14, INVALID_ACCESS_ERR: 15,
      VALIDATION_ERR: 16, TYPE_MISMATCH_ERR: 17, SECURITY_ERR: 18,
      NETWORK_ERR: 19, ABORT_ERR: 20, URL_MISMATCH_ERR: 21,
      QUOTA_EXCEEDED_ERR: 22, TIMEOUT_ERR: 23, INVALID_NODE_TYPE_ERR: 24,
      DATA_CLONE_ERR: 25,
    };
    Object.keys(legacy).forEach(function (key) {
      DOMException[key] = legacy[key];
      DOMException.prototype[key] = legacy[key];
    });
  })();
  host.DOMException = DOMException;

  // ---------------------------------------------------------------------
  // DOMParser (task: real DOMParser). `parseFromString` hands back a REAL
  // DOM built by the engine's own html5ever parser: the `__ironsurfDomParseHtml`
  // native creates a detached `<html>` element in the document's node store
  // and parses the markup into it, so every node is a genuine blitz node —
  // textContent, queries (scoped, so they reach detached subtrees),
  // attributes, mutation — it is simply never inserted: no style, no layout,
  // no paint, no resource fetches. Exactly the inert document a browser
  // gives back. The Document wrapper mirrors the spec surface over that
  // element (documentElement/head/body/…, with the creation methods
  // delegating to the live document like adoption would).
  // ---------------------------------------------------------------------
  var DOMParserDocumentProto = {
    get documentElement() { return this.__root; },
    get head() { return this.__root.querySelector("head"); },
    get body() { return this.__root.querySelector("body"); },
    set body(node) { throw new Error("setting document.body is not supported"); },
    get title() {
      var titleNode = this.querySelector("title");
      return titleNode ? titleNode.textContent : "";
    },
    get textContent() { return this.__root.textContent; },
    get innerHTML() { return this.__root.innerHTML; },
    get childNodes() { return this.__root.childNodes; },
    get firstChild() { return this.__root.firstChild; },
    get lastChild() { return this.__root.lastChild; },
    get doctype() { return null; },
    get documentURI() { return "about:blank"; },
    get URL() { return "about:blank"; },
    get defaultView() { return null; },
    querySelector: function (selector) {
      return wrapNode(0, host.__ironsurfDomQuery(0, String(selector), this.__root.__n));
    },
    querySelectorAll: function (selector) {
      host.__ironsurfDomQuery(0, String(selector), this.__root.__n); // validate
      return makeList(0, 3, this.__root.__n, String(selector));
    },
    getElementById: function (id) {
      // The document-wide id lookup cannot see a detached tree, so walk it.
      var wanted = String(id);
      var all = this.__root.querySelectorAll("*");
      for (var i = 0; i < all.length; i++) {
        var candidate = all[i];
        if (candidate.getAttribute && candidate.getAttribute("id") === wanted) return candidate;
      }
      return null;
    },
    getElementsByTagName: function (tag) {
      var name = String(tag).toLowerCase();
      return this.__root.querySelectorAll(name === "*" ? "*" : name);
    },
    getElementsByClassName: function (names) {
      var joined = String(names).trim().split(/\s+/).join(".");
      if (joined === "") return [];
      return this.__root.querySelectorAll("." + joined);
    },
    createElement: function (tag) { return document.createElement(tag); },
    createElementNS: function (ns, tag) { return document.createElementNS(ns, tag); },
    createTextNode: function (text) { return document.createTextNode(text); },
    createComment: function (text) { return document.createComment(text); },
    createDocumentFragment: function () { return document.createDocumentFragment(); },
    adoptNode: function (node) { return node; },
    importNode: function (node) { return node; },
  };
  function DOMParser() {}
  DOMParser.prototype.parseFromString = function (source, mimeType) {
    var type = String(mimeType === undefined ? "" : mimeType)
      .toLowerCase()
      .split(";")[0]
      .trim();
    // The spec's supported types; all of them parse through the HTML parser
    // here (blitz has no separate XML/image pipeline to hand out — XHTML and
    // SVG markup survives it element-for-element, which is what matters).
    var supported =
      type === "text/html" ||
      type === "application/xhtml+xml" ||
      type === "image/svg+xml" ||
      type === "text/xml" ||
      type === "application/xml";
    if (!supported) {
      throw new TypeError("DOMParser.parseFromString: unsupported mime type " + type);
    }
    var markup = source === undefined ? "" : String(source);
    var root = wrapNode(0, host.__ironsurfDomParseHtml(0, markup));
    var parsed = Object.create(DOMParserDocumentProto);
    Object.defineProperty(parsed, "__root", { value: root });
    return parsed;
  };
  host.DOMParser = DOMParser;

  // ---------------------------------------------------------------------
  // MutationObserver (task: infosec.exchange). Mastodon's boot chain died
  // at `new MutationObserver(...)` ("MutationObserver is not defined") and
  // the page console reported it caught — the app never rendered past the
  // <noscript> shell. This is a REAL observer for every mutation the page
  // itself performs: the childList/attribute/characterData records are
  // emitted by the DOM methods below (appendChild, setAttribute, the
  // textContent/innerHTML setters, …), delivered as microtasks in spec
  // batches, with subtree matching through the live parent chain. Changes
  // blitz makes on its own (user typing into a focused field) are the
  // documented gap — no engine change journal exists to observe.
  // ---------------------------------------------------------------------
  var moObservers = []; // {callback, target, options}
  var moRecords = null; // this microtask's records (null = none queued)
  var moScheduled = false;
  function moIsAncestor(maybeAncestor, node) {
    var seen = 0;
    var parent = node.parentNode;
    while (parent && seen++ < 4096) {
      if (parent === maybeAncestor) return true;
      parent = parent.parentNode;
    }
    return false;
  }
  function moInterested(observer, record) {
    var options = observer.options;
    // Observing the DOCUMENT (what every app actually does) means every
    // element of this page is inside it — the wrapper tree has no node id
    // for the document, so the containment is decided by definition.
    if (observer.isDocument && record.target && record.target !== host) {
      // fall through to the type/filter checks below with containment met
    } else if (record.target !== observer.target) {
      if (!options.subtree || !moIsAncestor(observer.target, record.target)) return false;
    }
    if (record.type === "childList" && !options.childList) return false;
    if (record.type === "attributes" && !options.attributes) return false;
    if (record.type === "characterData" && !options.characterData) return false;
    if (record.type === "attributes" && options.attributeFilter && record.target) {
      var wanted = false;
      for (var i = 0; i < options.attributeFilter.length; i++) {
        if (String(options.attributeFilter[i]) === record.attributeName) {
          wanted = true;
          break;
        }
      }
      if (!wanted) return false;
    }
    return true;
  }
  function moEmit(record) {
    if (moObservers.length === 0) return;
    var relevant = false;
    for (var i = 0; i < moObservers.length; i++) {
      if (moInterested(moObservers[i], record)) {
        relevant = true;
        break;
      }
    }
    if (!relevant) return;
    if (!moRecords) moRecords = [];
    moRecords.push(record);
    if (moScheduled) return;
    moScheduled = true;
    queueMicrotask(function () {
      moScheduled = false;
      var batch = moRecords;
      moRecords = null;
      if (!batch) return;
      // Snapshot the observer list: a callback may observe/disconnect more.
      var observers = moObservers.slice();
      for (var o = 0; o < observers.length; o++) {
        var observer = observers[o];
        var forThis = batch.filter(function (record) {
          return moInterested(observer, record);
        });
        if (forThis.length === 0) continue;
        try {
          observer.callback(forThis, observer);
        } catch (err) {
          reportError("MutationObserver callback: " + err);
        }
      }
    });
  }
  function MutationObserver(callback) {
    if (typeof callback !== "function") {
      throw new TypeError("Failed to construct 'MutationObserver': parameter 1 is not of type 'Function'.");
    }
    this.__callback = callback;
  }
  MutationObserver.prototype.observe = function (target, options) {
    if ((!target || target.__n === undefined) && target !== document) {
      throw new TypeError("Failed to execute 'observe': parameter 1 is not of type 'Node'.");
    }
    var opts = options || {};
    if (!opts.childList && !opts.attributes && !opts.characterData) {
      throw new TypeError("Failed to execute 'observe': The options object must set at least one of 'attributes', 'characterData', or 'childList' to true.");
    }
    // Spec: re-observing the same target replaces the previous registration.
    for (var i = 0; i < moObservers.length; i++) {
      if (moObservers[i].callback === this.__callback && moObservers[i].target === target) {
        moObservers[i].options = opts;
        return;
      }
    }
    moObservers.push({
      callback: this.__callback,
      target: target,
      options: opts,
      isDocument: target === document,
    });
  };
  MutationObserver.prototype.disconnect = function () {
    var callback = this.__callback;
    moObservers = moObservers.filter(function (entry) {
      return entry.callback !== callback;
    });
  };
  MutationObserver.prototype.takeRecords = function () {
    if (!moRecords) return [];
    var callback = this.__callback;
    var mine = [];
    var rest = [];
    for (var i = 0; i < moRecords.length; i++) {
      var entry = { observer: null, record: moRecords[i] };
      var interest = false;
      for (var o = 0; o < moObservers.length; o++) {
        if (moObservers[o].callback === callback && moInterested(moObservers[o], moRecords[i])) {
          interest = true;
          break;
        }
      }
      (interest ? mine : rest).push(moRecords[i]);
    }
    moRecords = rest;
    return mine;
  };
  // The mutation methods emit records (wrap-and-emit keeps every existing
  // behaviour — the wrapper runs the original first, then reports).
  // `before` may capture a pre-mutation snapshot passed on to `after`.
  function moWrap(name, emit) {
    var original = NodeProto[name];
    if (typeof original !== "function") return;
    NodeProto[name] = function () {
      var args = Array.prototype.slice.call(arguments);
      var snapshot = emit.before ? emit.before.apply(this, args) : undefined;
      var result = original.apply(this, args);
      emit.after.call(this, result, args, snapshot);
      return result;
    };
  }
  moWrap("appendChild", {
    after: function (result, args) {
      moEmit({ type: "childList", target: this, addedNodes: [args[0]], removedNodes: [] });
    },
  });
  moWrap("insertBefore", {
    after: function (result, args) {
      moEmit({ type: "childList", target: this, addedNodes: [args[0]], removedNodes: [] });
    },
  });
  moWrap("removeChild", {
    after: function (result, args) {
      moEmit({ type: "childList", target: this, addedNodes: [], removedNodes: [args[0]] });
    },
  });
  moWrap("replaceChild", {
    after: function (result, args) {
      moEmit({ type: "childList", target: this, addedNodes: [args[0]], removedNodes: [args[1]] });
    },
  });
  moWrap("remove", {
    before: function () {
      return this.parentNode;
    },
    after: function (result, args, parent) {
      if (parent) moEmit({ type: "childList", target: parent, addedNodes: [], removedNodes: [this] });
    },
  });
  moWrap("setAttribute", {
    before: function (name) {
      return this.getAttribute(name); // null = the attribute is new
    },
    after: function (result, args, oldValue) {
      moEmit({
        type: "attributes",
        target: this,
        attributeName: String(args[0]),
        oldValue: oldValue === null || oldValue === undefined ? "" : oldValue,
      });
    },
  });
  moWrap("removeAttribute", {
    before: function (name) {
      return this.getAttribute(name);
    },
    after: function (result, args, oldValue) {
      if (oldValue !== null && oldValue !== undefined) {
        moEmit({ type: "attributes", target: this, attributeName: String(args[0]), oldValue: oldValue });
      }
    },
  });
  // The text setters: characterData on text nodes, childList on elements
  // (replacing an element's children — the diff is not reconstructed here,
  // the record still fires like a browser's).
  ["textContent", "innerHTML"].forEach(function (name) {
    var accessor = Object.getOwnPropertyDescriptor(NodeProto, name);
    if (!accessor || typeof accessor.set !== "function") return;
    Object.defineProperty(NodeProto, name, {
      configurable: accessor.configurable,
      enumerable: accessor.enumerable,
      get: accessor.get,
      set: function (value) {
        var isText = this.nodeType === 3;
        var oldValue = isText ? this.nodeValue : undefined;
        accessor.set.call(this, value);
        if (isText) {
          moEmit({ type: "characterData", target: this, oldValue: oldValue === null ? "" : oldValue });
        } else {
          moEmit({ type: "childList", target: this, addedNodes: [], removedNodes: [] });
        }
      },
    });
  });
  host.MutationObserver = MutationObserver;

  // ---------------------------------------------------------------------
  // MessageChannel / MessagePort (task: infosec.exchange). react-dom's
  // scheduler feature-detects `MessageChannel` and dies at `new
  // MessageChannel()` without it ("not a callable function" deep inside
  // the reconciler — the app never painted). Ports are real in the ways
  // this browser can honour: postMessage delivers the *other* port a
  // `message` event as a fresh task (0 ms host timer — like the browser's
  // task queue, never re-entrant into the poster), onmessage plus
  // addEventListener listeners all fire, close() stops delivery, and data
  // passes by reference (one thread, one heap — the honest version of
  // structured clone here). `start()` is accepted; delivery never waits
  // on it.
  // ---------------------------------------------------------------------
  function MessagePort() {
    this.__other = null;
    this.__closed = false;
    this.onmessage = null;
  }
  MessagePort.prototype.postMessage = function (data) {
    var other = this.__other;
    if (!other || other.__closed) return;
    var messageEvent = {
      type: "message",
      data: data,
      source: this,
      origin: urlParts(host.__ironsurfUrl).protocol + (urlParts(host.__ironsurfUrl).host ? "//" + urlParts(host.__ironsurfUrl).host : ""),
      lastEventId: "",
      ports: [],
    };
    host.__ironsurfSetTimeout(function () {
      if (other.__closed) return;
      if (typeof other.onmessage === "function") {
        try {
          other.onmessage.call(other, messageEvent);
        } catch (err) {
          reportError("MessagePort onmessage: " + err);
        }
      }
      var list = listenerList(other, "message", false);
      if (list) {
        var pending = list.slice();
        for (var i = 0; i < pending.length; i++) callListener(pending[i], messageEvent, other);
      }
    }, 0);
  };
  MessagePort.prototype.start = function () {
    // Delivery is task-based from the first postMessage; nothing to arm.
  };
  MessagePort.prototype.close = function () {
    this.__closed = true;
  };
  MessagePort.prototype.addEventListener = function (type, fn, options) {
    addListener(this, String(type), fn, options);
  };
  MessagePort.prototype.removeEventListener = function (type, fn, options) {
    removeListener(this, String(type), fn, options);
  };
  function MessageChannel() {
    var port1 = new MessagePort();
    var port2 = new MessagePort();
    port1.__other = port2;
    port2.__other = port1;
    this.port1 = port1;
    this.port2 = port2;
  }
  host.MessageChannel = MessageChannel;
  host.MessagePort = MessagePort;

  // ---------------------------------------------------------------------
  // IntersectionObserver (task: infosec.exchange). Without this the Mastodon
  // `scrollable_list` component throws `ReferenceError:
  // IntersectionObserver is not defined` mid-render the moment a timeline
  // mounts — the statuses are fetched (200, 97 KB) but the column dies into
  // its error boundary and nothing paints. This engine exposes no layout
  // geometry to JS at all (`getBoundingClientRect`/`clientHeight` are not
  // bridged, `innerHeight` is a constant), so an honest intersection test is
  // impossible; the simplification here is OPTIMISTIC VIEWPORT: every
  // observed target is reported exactly once, as fully intersecting, on the
  // next task — the exact record shape sentinel-driven "load more"/lazy-mount
  // code consumes, once per `observe()` like the spec's initial delivery.
  // A real browser would re-report on scroll; that feedback loop is absent,
  // so nothing here can re-trigger the app in a loop. `unobserve` /
  // `disconnect` stop pending and future records; `takeRecords` drains the
  // (always empty, post-delivery) queue.
  // ---------------------------------------------------------------------
  function ioRect() {
    return { x: 0, y: 0, top: 0, left: 0, right: 0, bottom: 0, width: 0, height: 0 };
  }
  function IntersectionObserver(callback, options) {
    if (typeof callback !== "function") {
      throw new TypeError("Failed to construct 'IntersectionObserver': callback required");
    }
    options = options || {};
    this.root = options.root || null;
    this.rootMargin = String(options.rootMargin || "0px");
    this.threshold = options.threshold === undefined ? 0 : options.threshold;
    this.__callback = callback;
    this.__pending = [];
    this.__dead = false;
  }
  IntersectionObserver.prototype.observe = function (target) {
    if (this.__dead || !target) return;
    for (var i = 0; i < this.__pending.length; i++) {
      if (this.__pending[i] === target) return; // already queued, like the spec
    }
    this.__pending.push(target);
    var self = this;
    host.__ironsurfSetTimeout(function () {
      if (self.__dead) return;
      var idx = self.__pending.indexOf(target);
      if (idx < 0) return; // unobserved before delivery: no record
      self.__pending.splice(idx, 1);
      var entry = {
        target: target,
        isIntersecting: true,
        intersectingTime: 0,
        intersectionRatio: 1,
        boundingClientRect: ioRect(),
        intersectionRect: ioRect(),
        rootBounds: ioRect(),
        time: Date.now(),
      };
      try {
        self.__callback([entry], self);
      } catch (err) {
        reportError("IntersectionObserver callback: " + err);
      }
    }, 0);
  };
  IntersectionObserver.prototype.unobserve = function (target) {
    var idx = this.__pending.indexOf(target);
    if (idx >= 0) this.__pending.splice(idx, 1);
  };
  IntersectionObserver.prototype.disconnect = function () {
    this.__pending = [];
    this.__dead = true;
  };
  IntersectionObserver.prototype.takeRecords = function () {
    return []; // records deliver on task; nothing ever queues behind
  };
  host.IntersectionObserver = IntersectionObserver;

  // ---------------------------------------------------------------------
  // getComputedStyle (task: infosec.exchange). Mastodon's lightbox module
  // boots with `getComputedStyle(document.body).direction !== "rtl"` — no
  // function, no boot. Each read goes through the `__ironsurfDomComputedValue`
  // native, which answers the REAL stylo computed value of the live,
  // settled node; unmodelled properties (and never-laid-out detached nodes)
  // read as "" like an unset custom property. Pseudo-element arguments are
  // accepted and read from the element itself (no pseudo resolution here).
  // ---------------------------------------------------------------------
  function cssPropName(name) {
    var text = String(name);
    if (/^(webkit|moz|ms|o)[A-Z]/.test(text)) {
      return "-" + text.replace(/([A-Z])/g, "-$1").toLowerCase();
    }
    return text.replace(/([A-Z])/g, "-$1").toLowerCase();
  }
  host.getComputedStyle = function (element, pseudoElement) {
    void pseudoElement; // no pseudo tree to resolve against — see header
    var node = element && element.__n ? element : document.documentElement;
    var read = function (name) {
      var property = cssPropName(name);
      if (property.indexOf("-") === 0) return ""; // vendor prefix: not modelled
      return host.__ironsurfDomComputedValue(node.__d, node.__n, property);
    };
    var base = {
      getPropertyValue: read,
      getPropertyPriority: function () {
        return "";
      },
      setProperty: noop,
      removeProperty: function () {
        return "";
      },
      item: function () {
        return "";
      },
      length: 0,
      parentRule: null,
      type: "",
    };
    return new Proxy(base, {
      get: function (target, key) {
        if (typeof key === "symbol" || Object.prototype.hasOwnProperty.call(target, key)) {
          return target[key];
        }
        return read(key);
      },
      set: function (target, key, value) {
        // Setting a computed style writes the INLINE style (spec) — through
        // the live `style` surface when the node has one.
        if (node.style && typeof key === "string") {
          node.style[key] = value;
        }
        return true;
      },
      has: function (target, key) {
        return key in target || (typeof key === "string" && read(key) !== "");
      },
    });
  };

  // ---------------------------------------------------------------------
  // history (task: infosec.exchange). react-router's browser history does
  // `window.history.length` as its FIRST statement — a missing `history`
  // aborted the whole router module ("cannot convert 'null' or 'undefined'
  // to object"), and Mastodon stayed on its <noscript> fallback. The URL
  // stack here mirrors what the page pushed: pushState/replaceState are
  // REAL address-bar updates (the `__ironsurfSetUrl` native, resolve +
  // same-origin enforced tab-side) that fetch nothing; back/forward/go
  // cannot rewind page state here, so they stay inert (the SHELL's back
  // button walks the tab's real document history, untouched by this).
  // ---------------------------------------------------------------------
  var historyStack = [{ url: host.__ironsurfUrl, state: null }];
  var historyTop = 0;
  function historyOrigin(urlText) {
    var parts = urlParts(urlText);
    return parts.protocol + "|" + parts.host;
  }
  function writeHistoryEntry(data, urlArg, replace) {
    var state = data === undefined ? null : data;
    var currentUrl = host.__ironsurfUrl;
    var nextUrl = currentUrl;
    if (urlArg !== undefined && urlArg !== null && String(urlArg) !== "") {
      var resolved = String(urlArg);
      try {
        var absolute = host.__ironsurfDomResolveUrl(0, resolved);
        if (absolute) resolved = absolute;
      } catch (err) {
        throw new DOMException("pushState: bad url argument", "SyntaxError");
      }
      if (historyOrigin(resolved) !== historyOrigin(currentUrl)) {
        // Spec: a cross-origin pushState/replaceState is a SecurityError.
        throw new DOMException("pushState: cross-origin url", "SecurityError");
      }
      nextUrl = resolved;
    }
    var entry = { url: nextUrl, state: state };
    if (replace) {
      historyStack[historyTop] = entry;
    } else {
      historyStack.splice(historyTop + 1, historyStack.length, entry);
      historyTop = historyStack.length - 1;
    }
    if (nextUrl !== currentUrl) {
      // URL-only: the tab re-points the address bar, fetches nothing.
      host.__ironsurfSetUrl(nextUrl);
    }
  }
  var history = {
    scrollRestoration: "auto",
    get length() {
      return historyStack.length;
    },
    get state() {
      var entry = historyStack[historyTop];
      return entry ? entry.state : null;
    },
    go: noop,
    back: noop,
    forward: noop,
    pushState: function (data, title, url) {
      writeHistoryEntry(data, url, false);
    },
    replaceState: function (data, title, url) {
      writeHistoryEntry(data, url, true);
    },
  };
  host.history = history;

  // `process.nextTick` (task: infosec.exchange). boa_runtime ships a `process`
  // object whose `Object.prototype.toString` tag is literally `[object process]`
  // — so core-js's *environment detector* (toRawType(process)==="process")
  // concludes it is running under Node, and its `setImmediate` polyfill then
  // routes every flush through `process.nextTick(...)`, which does not exist:
  // the first microtask after boot dies with "not a callable function" and
  // Mastodon never hydrates. Give the Node-flavoured object the one method its
  // tag promises; semantics per spec (runs before the next microtask checkpoint,
  // extra args forwarded, returns a thenable-ish TickInfo we keep plain).
  if (typeof process === "object" && process) {
    if (typeof process.nextTick !== "function") {
      Object.defineProperty(process, "nextTick", {
        value: function (callback) {
          if (typeof callback !== "function") {
            throw new TypeError("callback must be a function");
          }
          var args = Array.prototype.slice.call(arguments, 1);
          // Node semantics: an exception thrown by a nextTick callback is an
          // uncaught exception, not a rejected promise. Swallow-and-log keeps
          // the error visible (console) instead of vanishing into an
          // unhandledrejection event our realm does not fire — React's
          // scheduler flushes route through here, and silent commit failures
          // are agony to debug otherwise.
          Promise.resolve().then(function () {
            callback.apply(null, args);
          }).catch(function (err) {
            try {
              console.error(err && err.stack ? String(err.stack) : err);
            } catch (ignored) {
              // nothing else to do
            }
          });
        },
        writable: true,
        configurable: true,
      });
    }
    // Feature-detects like `process.on("unhandledRejection", ...)` are common
    // in app boot code that trusts the `[object process]` tag; make them inert
    // instead of fatal.
    if (typeof process.on !== "function") {
      process.on = function () {
        return process;
      };
    }
    if (typeof process.off !== "function") {
      process.off = process.on;
    }
    if (typeof process.once !== "function") {
      process.once = process.on;
    }
    if (typeof process.removeAllListeners !== "function") {
      process.removeAllListeners = function () {
        return process;
      };
    }
    if (!process.versions) {
      process.versions = {};
    }
  }

  // IndexedDB (task: infosec.exchange). Mastodon's emoji localCache opens a
  // real Indexed database (`idb` on npm wraps raw `indexedDB.open`, requests
  // resolved from `success`/`error` events, transactions from `complete`), and
  // the boot chain `await`s it before creating the React root: without IDB the
  // app hangs silently between the `time` chunk import and `createRoot` — zero
  // errors, zero render. This is a small but *real* IndexedDB: object stores,
  // keyPaths (string/array), autoIncrement, indexes (unique + multiEntry),
  // IDBKeyRange queries, cursors, and auto-committing transactions, kept
  // in-memory (mastodon refetches on every cold start anyway). Requests fire
  // their events in creation order on microtasks, which is all the `idb`
  // wrapper relies on; iteration is a sorted full scan (slow-but-correct).
  // Canonical string key for Map-based storage: equal keys ⇒ equal strings,
  // distinct typed keys ⇒ distinct strings (type prefixes prevent "1" vs 1).
  function idbKeyString(key) {
    if (typeof key === "number") return "N:" + key;
    if (typeof key === "string") return "S:" + JSON.stringify(key);
    if (Array.isArray(key)) {
      var parts = ["A:"];
      for (var i = 0; i < key.length; i++) parts.push(idbKeyString(key[i]));
      return parts.join("\u0001");
    }
    if (key instanceof Date) return "T:" + key.getTime();
    return "U:" + String(key);
  }
  function idbCmp(a, b) {
    function rank(v) {
      if (typeof v === "number") return isNaN(v) ? 9 : 0;
      if (typeof v === "string") return 1;
      if (Array.isArray(v)) return 2;
      return 3;
    }
    var ra = rank(a), rb = rank(b);
    if (ra !== rb) return ra < rb ? -1 : 1;
    if (ra === 0) return a === b ? 0 : a < b ? -1 : 1;
    if (ra === 1) return a === b ? 0 : a < b ? -1 : 1;
    if (ra === 2) {
      var n = Math.min(a.length, b.length);
      for (var i = 0; i < n; i++) {
        var c = idbCmp(a[i], b[i]);
        if (c !== 0) return c;
      }
      return a.length === b.length ? 0 : a.length < b.length ? -1 : 1;
    }
    return 0;
  }
  function IDBKeyRange(lower, upper, lowerOpen, upperOpen) {
    this.lower = lower;
    this.upper = upper;
    this.lowerOpen = !!lowerOpen;
    this.upperOpen = !!upperOpen;
  }
  IDBKeyRange.bound = function (lower, upper, lo, up) {
    return new IDBKeyRange(lower, upper, lo, up);
  };
  IDBKeyRange.lowerBound = function (lower, lo) {
    return new IDBKeyRange(lower, undefined, lo, false);
  };
  IDBKeyRange.upperBound = function (upper, up) {
    return new IDBKeyRange(undefined, upper, false, up);
  };
  IDBKeyRange.only = function (value) {
    return new IDBKeyRange(value, value, false, false);
  };
  IDBKeyRange.prototype.includes = function (key) {
    if (key === undefined || key === null) return false;
    if (this.lower !== undefined) {
      var c = idbCmp(key, this.lower);
      if (c < 0 || (c === 0 && this.lowerOpen)) return false;
    }
    if (this.upper !== undefined) {
      var c2 = idbCmp(key, this.upper);
      if (c2 > 0 || (c2 === 0 && this.upperOpen)) return false;
    }
    return true;
  };
  IDBKeyRange.prototype.intersects = function (other) {
    if (this.lower !== undefined && other.upper !== undefined && idbCmp(this.lower, other.upper) > 0) return false;
    if (other.lower !== undefined && this.upper !== undefined && idbCmp(other.lower, this.upper) > 0) return false;
    return true;
  };
  function idbWrapEvent(type, extra) {
    var ev = new Event(type);
    if (extra) for (var k in extra) ev[k] = extra[k];
    return ev;
  }
  class IDBRequest extends EventTarget {
    constructor() {
      super();
      this.result = null;
      this.error = null;
      this.source = null;
      this.transaction = null;
      this.readyState = "pending";
    }
  }
  class IDBOpenDBRequest extends IDBRequest {
    constructor() {
      super();
      this.oldVersion = 0;
      this.newVersion = 0;
    }
  }
  class IDBTransaction extends EventTarget {
    constructor(db, storeNames, mode) {
      super();
      this.db = db;
      this._names = storeNames.slice();
      this.mode = mode;
      this.error = null;
      this.aborted = false;
      this._pending = 0;
      this._completeFired = false;
      var self = this;
      this._names.forEach(function (name, i) {
        self[i] = name;
      });
      this.length = this._names.length;
      queueMicrotask(function () {
        self._maybeComplete();
      });
    }
    get objectStoreNames() {
      var names = this._names.slice();
      names.contains = function (n) {
        return names.indexOf(n) !== -1;
      };
      return names;
    }
    objectStore(name) {
      // versionchange transactions may touch stores the upgrade itself just
      // created, so they are not pinned to the name list captured at open.
      if (this.mode !== "versionchange" && this._names.indexOf(name) === -1) {
        throw new DOMException("object store not in transaction", "NotFoundError");
      }
      var store = this.db.__stores.get(name);
      if (!store) throw new DOMException("object store gone", "NotFoundError");
      // One IDBObjectStore facade per (tx, store): idb keys its wrapper cache
      // by the raw object, and `tx.store` must be the first store's object.
      this.__facades ||= {};
      if (!this.__facades[name]) {
        var facade = Object.create(IDBObjectStore.prototype);
        Object.assign(facade, store);
        facade.__tx = this;
        this.__facades[name] = facade;
      }
      return this.__facades[name];
    }
    _start(op, source) {
      if (this.aborted) throw new DOMException("transaction aborted", "AbortError");
      var req = new IDBRequest();
      req.transaction = this;
      req.source = source || null;
      this._pending++;
      var self = this;
      queueMicrotask(function () {
        if (self.aborted) {
          req.error = new DOMException("transaction aborted", "AbortError");
          req.readyState = "done";
          req.dispatchEvent(idbWrapEvent("error"));
        } else {
          try {
            req.result = op();
            req.readyState = "done";
            req.dispatchEvent(idbWrapEvent("success"));
          } catch (err) {
            req.error = err instanceof DOMException ? err : new DOMException(String(err && err.message));
            req.readyState = "done";
            self.error = req.error;
            req.dispatchEvent(idbWrapEvent("error"));
          }
        }
        self._pending--;
        queueMicrotask(function () {
          self._maybeComplete();
        });
      });
      return req;
    }
    _maybeComplete() {
      if (this.aborted || this._pending > 0 || this._completeFired) return;
      this._completeFired = true;
      this.dispatchEvent(idbWrapEvent("complete"));
    }
    abort() {
      if (this._completeFired) return;
      this.aborted = true;
      this.error = new DOMException("transaction aborted", "AbortError");
      this.dispatchEvent(idbWrapEvent("abort"));
    }
  }
  class IDBIndex {
    constructor(store, name, keyPath, options) {
      this.name = name;
      this.keyPath = keyPath;
      this.multiEntry = !!(options && options.multiEntry);
      this.unique = !!(options && options.unique);
      this.__store = store;
      var self = this;
      this.__keys = function (entry) {
        return indexKeys(entry[1], self.keyPath, self.multiEntry);
      };
    }
    __tx() {
      var tx = this.__store.__tx;
      if (!tx) throw new DOMException("index used outside a transaction", "InvalidStateError");
      return tx;
    }
    getAll(query) {
      var self = this;
      var tx = this.__tx();
      var req = tx._start(function () {
        return self.__match(query).map(function (hit) {
          return hit[1][1];
        });
      }, this);
      return req;
    }
    getAllKeys(query) {
      var self = this;
      var tx = this.__tx();
      return tx._start(function () {
        return self.__match(query).map(function (hit) {
          return hit[1][0];
        });
      }, this);
    }
    get(query) {
      var self = this;
      var tx = this.__tx();
      return tx._start(function () {
        var hits = self.__match(query);
        return hits.length ? hits[0][1][1] : undefined;
      }, this);
    }
    getKey(query) {
      var self = this;
      var tx = this.__tx();
      return tx._start(function () {
        var hits = self.__match(query);
        return hits.length ? hits[0][1][0] : undefined;
      }, this);
    }
    count(query) {
      var self = this;
      var tx = this.__tx();
      return tx._start(function () {
        return self.__match(query).length;
      }, this);
    }
    openCursor(query, direction) {
      var self = this;
      var tx = this.__tx();
      var box = { _rows: null, _i: 0 };
      var req = tx._start(function () {
        box._rows = self.__match(query);
        box._i = 0;
        return cursorValue(box, tx, false);
      }, this);
      req.__box = box;
      return req;
    }
    __match(query) {
      var self = this;
      var rows = this.__store.__rows();
      var hits = [];
      rows.forEach(function (row) {
        self.__keys(row).forEach(function (k) {
          if (query === undefined || query === null || (query instanceof IDBKeyRange && query.includes(k)) || (!(query instanceof IDBKeyRange) && idbCmp(k, query) === 0)) {
            hits.push([k, row]);
          }
        });
      });
      hits.sort(function (a, b) {
        return idbCmp(a[0], b[0]) || idbCmp(a[1][0], b[1][0]);
      });
      return hits;
    }
  }
  function indexKeys(value, keyPath, multiEntry) {
    var raw;
    if (Array.isArray(keyPath)) {
      raw = keyPath.map(function (part) {
        return value ? value[part] : undefined;
      });
      if (raw.some(function (p) { return p === undefined || p === null; })) return [];
      return [raw];
    }
    raw = value ? value[keyPath] : undefined;
    if (raw === undefined || raw === null) return [];
    if (multiEntry) {
      if (!Array.isArray(raw)) raw = [raw];
      return raw.filter(function (k) { return k !== undefined && k !== null; });
    }
    return [raw];
  }
  // Index definitions and store shapes are mirrored into `record.storeDefs`
  // so a later `indexedDB.open` (a reload in the same tab) sees the schema
  // even though the data itself is in-memory and discarded per realm only in
  // spirit — stores survive here for cheap re-imports.
  function idbWirePersist(store, record) {
    store.__persist = function () {
      var defs = {};
      Object.keys(store.__indexDefs).forEach(function (n) {
        var ix = store.__indexDefs[n];
        defs[n] = { keyPath: ix.keyPath, multiEntry: ix.multiEntry, unique: ix.unique };
      });
      record.storeDefs[store.name] = {
        keyPath: store.keyPath,
        autoIncrement: store.autoIncrement,
        indexes: defs,
      };
    };
  }
  function idbHydrate(record) {
    record.stores.forEach(function (store, name) {
      var def = record.storeDefs[name];
      if (!def) return;
      Object.keys(def.indexes || {}).forEach(function (iname) {
        if (store.__indexDefs[iname]) return;
        var d = def.indexes[iname];
        store.__indexDefs[iname] = new IDBIndex(store, iname, d.keyPath, {
          multiEntry: d.multiEntry,
          unique: d.unique,
        });
      });
      idbWirePersist(store, record);
    });
  }
  class IDBObjectStore {
    constructor(name, keyPath, autoIncrement) {
      this.name = name;
      this.keyPath = keyPath;
      this.autoIncrement = !!autoIncrement;
      // Map<canonical key string, [key, value]>: O(1) put/get/delete. Reads
      // that must report IDB order re-sort from the map (n log n) — the
      // bulk-import path (Mastodon: thousands of emoji rows) stays linear.
      this.__entries = new Map();
      this.__nextAuto = 1;
      this.__indexDefs = {};
      this.__tx = null;
    }
    get indexNames() {
      var names = Object.keys(this.__indexDefs);
      names.contains = function (n) {
        return names.indexOf(n) !== -1;
      };
      return names;
    }
    createIndex(name, keyPath, options) {
      var idx = new IDBIndex(this, name, keyPath, options);
      this.__indexDefs[name] = idx;
      if (this.__persist) this.__persist();
      return idx;
    }
    deleteIndex(name) {
      delete this.__indexDefs[name];
      if (this.__persist) this.__persist();
    }
    index(name) {
      if (!this.__indexDefs[name]) {
        throw new DOMException("no such index", "NotFoundError");
      }
      return this.__indexDefs[name];
    }
    __keyFor(value, key) {
      if (key !== undefined && key !== null) return key;
      if (typeof this.keyPath === "string" && value && value[this.keyPath] !== undefined) return value[this.keyPath];
      if (Array.isArray(this.keyPath)) {
        var parts = this.keyPath.map(function (p) { return value ? value[p] : undefined; });
        if (parts.every(function (p) { return p !== undefined; })) return parts;
      }
      return undefined;
    }
    __rows() {
      var rows = [];
      this.__entries.forEach(function (entry) {
        rows.push(entry);
      });
      rows.sort(function (a, b) { return idbCmp(a[0], b[0]); });
      return rows;
    }
    __write(k, value) {
      this.__entries.set(idbKeyString(k), [k, value]);
      if (typeof k === "number" && k >= this.__nextAuto) this.__nextAuto = k + 1;
    }
    __allocKey() {
      var k = this.__nextAuto;
      this.__nextAuto = k + 1;
      return k;
    }
    put(value, key) {
      var self = this;
      var tx = this.__tx;
      return tx._start(function () {
        var k = self.__keyFor(value, key);
        if (k === undefined) {
          if (self.autoIncrement) k = self.__allocKey();
          else throw new DOMException("missing key for put", "DataError");
        }
        self.__write(k, value);
        return k;
      }, this);
    }
    add(value, key) {
      var self = this;
      var tx = this.__tx;
      return tx._start(function () {
        var k = self.__keyFor(value, key);
        if (k === undefined) {
          if (self.autoIncrement) k = self.__allocKey();
          else throw new DOMException("missing key for add", "DataError");
        }
        if (self.__entries.has(idbKeyString(k))) {
          throw new DOMException("key already present", "ConstraintError");
        }
        self.__write(k, value);
        return k;
      }, this);
    }
    get(key) {
      var self = this;
      var tx = this.__tx;
      return tx._start(function () {
        if (key instanceof IDBKeyRange) {
          var rows = self.__match(key);
          return rows.length ? rows[0][1] : undefined;
        }
        var hit = self.__entries.get(idbKeyString(key));
        return hit ? hit[1] : undefined;
      }, this);
    }
    getAll(query, limit) {
      var self = this;
      var tx = this.__tx;
      return tx._start(function () {
        var rows = self.__match(query);
        if (limit !== undefined) rows = rows.slice(0, limit);
        return rows.map(function (row) { return row[1]; });
      }, this);
    }
    getAllKeys(query, limit) {
      var self = this;
      var tx = this.__tx;
      return tx._start(function () {
        var rows = self.__match(query);
        if (limit !== undefined) rows = rows.slice(0, limit);
        return rows.map(function (row) { return row[0]; });
      }, this);
    }
    getKey(query) {
      var self = this;
      var tx = this.__tx;
      return tx._start(function () {
        var rows = self.__match(query);
        return rows.length ? rows[0][0] : undefined;
      }, this);
    }
    count(query) {
      var self = this;
      var tx = this.__tx;
      return tx._start(function () {
        if (query === undefined || query === null) return self.__entries.size;
        return self.__match(query).length;
      }, this);
    }
    delete(key) {
      var self = this;
      var tx = this.__tx;
      return tx._start(function () {
        if (key instanceof IDBKeyRange) {
          var doomed = [];
          self.__entries.forEach(function (entry, mapKey) {
            if (key.includes(entry[0])) doomed.push(mapKey);
          });
          doomed.forEach(function (mapKey) {
            self.__entries.delete(mapKey);
          });
        } else {
          self.__entries.delete(idbKeyString(key));
        }
        return undefined;
      }, this);
    }
    clear() {
      var self = this;
      var tx = this.__tx;
      return tx._start(function () {
        self.__entries.clear();
        return undefined;
      }, this);
    }
    openCursor(query, direction) {
      var self = this;
      var tx = this.__tx;
      var box = { _rows: null, _i: 0 };
      var req = tx._start(function () {
        box._rows = self.__match(query);
        box._i = 0;
        return cursorValue(box, tx, false);
      }, this);
      req.__box = box;
      return req;
    }
    openKeyCursor(query, direction) {
      var self = this;
      var tx = this.__tx;
      var box = { _rows: null, _i: 0 };
      var req = tx._start(function () {
        box._rows = self.__match(query);
        box._i = 0;
        return cursorValue(box, tx, true);
      }, this);
      req.__box = box;
      return req;
    }
    __match(query) {
      var rows;
      if (query === undefined || query === null) {
        rows = this.__rows();
      } else {
        rows = [];
        var self = this;
        this.__entries.forEach(function (row) {
          if (query instanceof IDBKeyRange) {
            if (query.includes(row[0])) rows.push(row);
          } else if (idbCmp(row[0], query) === 0) {
            rows.push(row);
          }
        });
        rows.sort(function (a, b) { return idbCmp(a[0], b[0]); });
      }
      return rows;
    }
  }
  // Shared cursor machinery: `box` holds the sorted hit list; continue() on
  // the SAME cursor object advances the shared index (spec behaviour).
  function cursorValue(box, tx, keyOnly) {
    if (!box._rows || box._i >= box._rows.length) return null;
    var hit = box._rows[box._i];
    if (!box.__cursor) {
      var cur = new IDBCursor();
      cur.__box = box;
      cur.__tx = tx;
      cur.__keyOnly = keyOnly;
      box.__cursor = cur;
    }
    var c = box.__cursor;
    c.key = hit[0];
    c.primaryKey = hit[1][0];
    c.value = hit[1][1];
    return c;
  }
  class IDBCursor extends EventTarget {
    constructor() {
      super();
      this.key = undefined;
      this.primaryKey = undefined;
      this.value = undefined;
    }
    continue(key) {
      var box = this.__box;
      if (key !== undefined && key !== null) {
        // skip ahead to first row at/after key
        while (box._i < box._rows.length && idbCmp(box._rows[box._i][0], key) < 0) box._i++;
      } else {
        box._i++;
      }
      var self = this;
      return this.__tx._start(function () {
        return cursorValue(box, self.__tx, self.__keyOnly);
      }, null);
    }
    advance(count) {
      var box = this.__box;
      box._i += count;
      var self = this;
      return this.__tx._start(function () {
        return cursorValue(box, self.__tx, self.__keyOnly);
      }, null);
    }
    delete() {
      var box = this.__box;
      var pk = this.primaryKey;
      var self = this;
      return this.__tx._start(function () {
        var store = self.__storeObj();
        if (store) {
          store.__entries = store.__entries.filter(function (row) {
            return idbCmp(row[0], pk) !== 0;
          });
        }
        return undefined;
      }, null);
    }
    __storeObj() {
      return null;
    }
  }
  class IDBCursorWithValue extends IDBCursor {}
  class IDBDatabase extends EventTarget {
    constructor(record) {
      super();
      this.__record = record;
      this.name = record.name;
      this.__stores = record.stores;
    }
    get version() {
      return this.__record.version;
    }
    get objectStoreNames() {
      var names = Array.from(this.__stores.keys());
      names.contains = function (n) {
        return names.indexOf(n) !== -1;
      };
      names.item = function (i) {
        return names[i];
      };
      return names;
    }
    createObjectStore(name, options) {
      if (this.__stores.has(name)) {
        throw new DOMException("object store exists", "ConstraintError");
      }
      var store = new IDBObjectStore(name, (options && options.keyPath) || null, !!(options && options.autoIncrement));
      this.__stores.set(name, store);
      idbWirePersist(store, this.__record);
      store.__persist();
      return store;
    }
    deleteObjectStore(name) {
      this.__stores.delete(name);
      delete this.__record.storeDefs[name];
    }
    transaction(storeNames, mode) {
      var names = Array.isArray(storeNames) ? storeNames.slice() : [storeNames];
      var m = mode || "readonly";
      var self = this;
      names.forEach(function (n) {
        if (!self.__stores.has(n)) {
          throw new DOMException("no such object store: " + n, "NotFoundError");
        }
      });
      return new IDBTransaction(self, names, m);
    }
    close() {
      this.__record.open = false;
      this.dispatchEvent(idbWrapEvent("close"));
    }
  }
  var idbDatabases = {};
  class IDBFactory extends EventTarget {
    open(name, version) {
      var req = new IDBOpenDBRequest();
      var record = idbDatabases[name];
      if (!record) {
        record = idbDatabases[name] = {
          name: name,
          version: 0,
          stores: new Map(),
          storeDefs: {},
        };
      }
      var want = version === undefined ? Math.max(record.version, 1) : version;
      var db = new IDBDatabase(record);
      req.result = db;
      req.transaction = null;
      req.oldVersion = record.version;
      req.newVersion = want;
      var self = this;
      queueMicrotask(function () {
        if (record.version > want) {
          req.error = new DOMException("version decrease", "VersionError");
          req.dispatchEvent(idbWrapEvent("error"));
          return;
        }
        idbHydrate(record);
        if (record.version < want) {
          var tx = new IDBTransaction(db, Array.from(record.stores.keys()), "versionchange");
          tx.db = db;
          req.transaction = tx;
          req.oldVersion = record.version;
          req.newVersion = want;
          var ev = idbWrapEvent("upgradeneeded", {
            oldVersion: record.version,
            newVersion: want,
          });
          req.result = db;
          req.dispatchEvent(ev);
          record.version = want;
        }
        req.readyState = "done";
        req.dispatchEvent(idbWrapEvent("success"));
      });
      return req;
    }
    deleteDatabase(name) {
      var req = new IDBOpenDBRequest();
      var record = idbDatabases[name];
      req.oldVersion = record ? record.version : 0;
      req.newVersion = null;
      var self = this;
      queueMicrotask(function () {
        if (idbDatabases[name]) {
          idbDatabases[name] = { name: name, version: 0, stores: new Map(), storeDefs: {} };
        }
        req.readyState = "done";
        req.dispatchEvent(idbWrapEvent("success"));
      });
      return req;
    }
    databases() {
      return Promise.resolve(
        Object.keys(idbDatabases).map(function (n) {
          return { name: n, version: idbDatabases[n].version };
        })
      );
    }
  }
  host.IDBKeyRange = IDBKeyRange;
  host.IDBRequest = IDBRequest;
  host.IDBOpenDBRequest = IDBOpenDBRequest;
  host.IDBTransaction = IDBTransaction;
  host.IDBObjectStore = IDBObjectStore;
  host.IDBIndex = IDBIndex;
  host.IDBCursor = IDBCursor;
  host.IDBCursorWithValue = IDBCursorWithValue;
  host.IDBDatabase = IDBDatabase;
  host.IDBFactory = IDBFactory;
  host.indexedDB = new IDBFactory();

  // crypto (task: infosec.exchange posts): Mastodon's request layer calls
  // `crypto.randomUUID()` during boot — with NO `crypto` global every
  // `crypto.x` reference throws a ReferenceError and the saga graph stalls
  // before a single trending request is issued (the column stays
  // "Loading…" forever). Just getRandomValues/randomUUID, nothing subtle:
  // feature detection via `crypto.subtle === undefined` keeps working.
  host.crypto = {
    getRandomValues: function (target) {
      var u8 = null;
      try {
        if (typeof ArrayBuffer !== "undefined" && target instanceof ArrayBuffer) {
          u8 = new Uint8Array(target);
        } else if (target && target.buffer !== undefined && target.buffer !== null) {
          u8 = new Uint8Array(target.buffer, target.byteOffset || 0, target.byteLength || 0);
        }
      } catch (e) {
        u8 = null;
      }
      if (!u8) {
        throw new TypeError("getRandomValues requires a typed array");
      }
      var bytes = host.__ironsurfRandomBytes(u8.length);
      for (var i = 0; i < u8.length; i++) {
        u8[i] = bytes.charCodeAt(i);
      }
      return target;
    },
    randomUUID: function () {
      return host.__ironsurfRandomUUID();
    },
  };

  // `reportError` (WHATWG): several of our listener-error paths report through
  // it; boa_runtime has no such global, so a listener throw previously died
  // with "reportError is not defined", masking the real error.
  if (typeof host.reportError !== "function") {
    host.reportError = function (e) {
      try {
        console.error(e);
      } catch (ignored) {
        // nothing else to do
      }
    };
  }

  // fetch() accepts our `URL` objects (task: infosec.exchange). boa_runtime's
  // FetchExtension type-checks the resource with `instanceof` against *its*
  // URL class, but the realm's global `URL` is the tab's own native — so
  // `fetch(new URL(x), {headers})` (how Mastodon's API layer calls it) died
  // with "Resource must be a URL or Request object", silently rejecting the
  // custom-emoji request that gates the React render. Unwrap URL-likes to
  // their href; Request objects go through untouched.
  // …and a RequestInit whose NULLABLE members are present-but-null is the
  // spec's way of saying "no value": `signal: null` = no signal, `body:
  // null` = no body. boa_runtime 0.22 converts its init with `TryFromJs`,
  // whose `Option<T>` demuxes only `undefined` — a PRESENT `null` falls
  // into the conversion arm, and `fetch(url, {signal: null})` dies as
  // `TypeError: cannot convert value to a Object` while `body: null` dies
  // as `Request constructor: body is not a supported type`. Both are what
  // Mastodon's axios sends: its fetch adapter builds `Object.assign(
  // Object.create(null), N, {signal: O, method, headers, body: data, …})`,
  // so a GET arrives with `signal: null`-shaped members and a null data.
  // Present-but-null therefore normalises to undefined through a shallow
  // copy — the caller's object is never mutated, and any non-null member
  // rides through untouched.
  // A string resource is resolved against the DOCUMENT base like browsers
  // (Request and fetch share this step). The engine chain is otherwise
  // honest to the string it is handed, and a path-relative "/api/v2/…"
  // (Mastodon's same-origin API style) survives into reqwest as a URI with
  // no authority — where `Request::builder().build()` fails with the http
  // crate's Display, literally "builder error", surfaced to the page as an
  // opaque rejection no one can debug. Absolutizing here means relative
  // requests reach the wire as real URLs.
  function absolutizeUrlLike(input) {
    try {
      if (typeof input === "string") {
        return new URL(input, host.location.href).href;
      }
    } catch (ignored) {
      // an unparseable base or input rides through as-is
    }
    return input;
  }
  var NULLABLE_INIT_MEMBERS = ["signal", "body"];
  function normalizeNullInit(init) {
    try {
      if (init !== null && typeof init === "object") {
        var copy = null;
        for (var i = 0; i < NULLABLE_INIT_MEMBERS.length; i++) {
          var key = NULLABLE_INIT_MEMBERS[i];
          if (init[key] === null) {
            if (copy === null) {
              copy = {};
              Object.keys(init).forEach(function (k) {
                copy[k] = init[k];
              });
            }
            copy[key] = undefined;
          }
        }
        if (copy !== null) return copy;
      }
    } catch (ignored) {
      // fall through with the original argument
    }
    return init;
  }
  if (typeof host.fetch === "function") {
    var rawFetch = host.fetch;
    host.fetch = function (resource, init) {
      try {
        if (
          resource !== null &&
          resource !== undefined &&
          typeof resource !== "string" &&
          !(typeof host.Request === "function" && resource instanceof host.Request)
        ) {
          var href =
            typeof resource.href === "string" && resource.href
              ? resource.href
              : typeof resource.url === "string" && resource.url
                ? resource.url
                : null;
          if (href !== null) resource = href;
        }
      } catch (ignored) {
        // fall through with the original argument
      }
      return rawFetch.call(host, absolutizeUrlLike(resource), normalizeNullInit(init));
    };
  }
  // The `Request` constructor takes the same RequestInit through the same
  // converter, so it gets the same normalisation. The wrapper returns the
  // RAW instance (so `instanceof` answers off the original class) and
  // forwards the prototype + `hasInstance` test, keeping the class-shaped
  // surface browsers expose.
  if (typeof host.Request === "function") {
    var RawRequest = host.Request;
    var RequestGlue = function (input, init) {
      return new RawRequest(absolutizeUrlLike(input), normalizeNullInit(init));
    };
    RequestGlue.prototype = RawRequest.prototype;
    Object.defineProperty(RequestGlue, "name", { value: "Request", configurable: true });
    Object.defineProperty(RequestGlue, Symbol.hasInstance, {
      value: function (x) {
        return x instanceof RawRequest;
      },
      configurable: true,
    });
    host.Request = RequestGlue;
  }

  // ---------------------------------------------------------------------
  // XMLHttpRequest (task: infosec.exchange posts). Mastodon's entire API
  // layer is axios, and axios selects its XHR adapter whenever
  // `typeof XMLHttpRequest === "function"` — without the constructor no
  // `/api/` request is EVER issued, and the timeline stayed empty with zero
  // console output (the rejection is swallowed inside the app's own chain).
  // This is the spec surface with ONE honest simplification: `send()` is
  // BLOCKING (the engine is single-threaded and even the fetch glue settles
  // on the tab's blocking client), so the whole event sequence —
  // readystatechange(2), readystatechange(3), load, readystatechange(4),
  // loadend (or error, readystatechange(4), loadend on a network failure) —
  // fires inside `send()` instead of across task turns. Every library in
  // existence (axios included) assigns its handlers BEFORE calling `send()`,
  // so nothing observes the compression. Binary `responseType`s
  // (arraybuffer/blob) answer an empty `response` — the envelope is text;
  // `responseText` always carries the (lossy-UTF-8) body, which is exactly
  // what axios reads for the default/`text`/`json` types.
  // ---------------------------------------------------------------------
  function XhrUpload() {}
  XhrUpload.prototype.addEventListener = function (type, fn) {
    addListener(this, String(type), fn, false);
  };
  XhrUpload.prototype.removeEventListener = function (type, fn) {
    removeListener(this, String(type), fn, false);
  };
  function fireXhrEvent(xhr, type) {
    // The standalone-EventTarget path of the dispatcher: own registered
    // listeners, then the `on<type>` property, errors reported per listener.
    var ev = makeEvent(type, xhr);
    ev.currentTarget = xhr;
    visitNode(xhr, ev, "target");
  }
  function XMLHttpRequest() {
    this._method = "GET";
    this._url = "";
    this._headers = []; // [[name, value]] in set order
    this._open = false;
    this.readyState = 0; // UNSENT
    this.status = 0;
    this.statusText = "";
    this.response = "";
    this.responseText = "";
    this.responseURL = "";
    this.responseXML = null;
    this.responseType = "";
    this.timeout = 0;
    this.withCredentials = false;
    this.upload = new XhrUpload();
    this.onload = null;
    this.onerror = null;
    this.ontimeout = null;
    this.onabort = null;
    this.onloadstart = null;
    this.onprogress = null;
    this.onloadend = null;
    this.onreadystatechange = null;
  }
  XMLHttpRequest.UNSENT = 0;
  XMLHttpRequest.OPENED = 1;
  XMLHttpRequest.HEADERS_RECEIVED = 2;
  XMLHttpRequest.LOADING = 3;
  XMLHttpRequest.DONE = 4;
  XMLHttpRequest.prototype.UNSENT = 0;
  XMLHttpRequest.prototype.OPENED = 1;
  XMLHttpRequest.prototype.HEADERS_RECEIVED = 2;
  XMLHttpRequest.prototype.LOADING = 3;
  XMLHttpRequest.prototype.DONE = 4;
  XMLHttpRequest.prototype.addEventListener = function (type, fn) {
    addListener(this, String(type), fn, false);
  };
  XMLHttpRequest.prototype.removeEventListener = function (type, fn) {
    removeListener(this, String(type), fn, false);
  };
  XMLHttpRequest.prototype.open = function (method, url) {
    // A relative target resolves against the document URL, like the spec's
    // entry-settings-object base; an unparseable pair throws like the spec.
    var resolved;
    try {
      resolved = new URL(String(url), host.location.href).href;
    } catch (err) {
      throw new TypeError("Failed to execute 'open' on 'XMLHttpRequest': invalid URL");
    }
    this._method = String(method).toUpperCase();
    this._url = resolved;
    this._headers = [];
    this._open = true;
    this.readyState = 1; // OPENED
    fireXhrEvent(this, "readystatechange");
  };
  XMLHttpRequest.prototype.setRequestHeader = function (name, value) {
    if (!this._open || this.readyState === 0) {
      throw new TypeError("Failed to execute 'setRequestHeader' on 'XMLHttpRequest': object is not initialized");
    }
    name = String(name);
    value = String(value);
    for (var i = 0; i < this._headers.length; i++) {
      if (this._headers[i][0].toLowerCase() === name.toLowerCase()) {
        // The spec COMBINES same-name headers with ", ".
        this._headers[i][1] += ", " + value;
        return;
      }
    }
    this._headers.push([name, value]);
  };
  XMLHttpRequest.prototype.getAllResponseHeaders = function () {
    var list = this._responseHeaders || [];
    var names = [];
    var byName = {};
    for (var i = 0; i < list.length; i++) {
      var name = String(list[i][0]).toLowerCase();
      // Forbidden response headers: never visible to a page.
      if (name === "set-cookie" || name === "set-cookie2") continue;
      if (!Object.prototype.hasOwnProperty.call(byName, name)) {
        byName[name] = [];
        names.push(name);
      }
      byName[name].push(list[i][1]);
    }
    names.sort(); // lexicographical order, per spec
    var out = "";
    for (var j = 0; j < names.length; j++) {
      out += names[j] + ": " + byName[names[j]].join(", ") + "\r\n";
    }
    return out;
  };
  XMLHttpRequest.prototype.getResponseHeader = function (name) {
    name = String(name).toLowerCase();
    if (name === "set-cookie" || name === "set-cookie2") return null;
    var list = this._responseHeaders || [];
    var values = null;
    for (var i = 0; i < list.length; i++) {
      if (String(list[i][0]).toLowerCase() === name) {
        if (values === null) values = [];
        values.push(list[i][1]);
      }
    }
    return values === null ? null : values.join(", ");
  };
  XMLHttpRequest.prototype.overrideMimeType = function (mime) {
    this._overriddenMimeType = String(mime);
  };
  XMLHttpRequest.prototype.abort = function () {
    // send() is synchronous here: by the time a page can call abort there
    // is nothing in flight any more. The flag is kept for shape only.
    this._aborted = true;
  };
  XMLHttpRequest.prototype.send = function (body) {
    if (!this._open) {
      throw new TypeError(
        "Failed to execute 'send' on 'XMLHttpRequest': XMLHttpRequest still initialized. Call 'open' first."
      );
    }
    var envelope = null;
    try {
      envelope = JSON.parse(
        host.__ironsurfXhr(
          this._method,
          this._url,
          JSON.stringify(this._headers),
          body === undefined || body === null ? null : String(body)
        )
      );
    } catch (err) {
      // A thrown native IS the network error: the page sees status 0 and the
      // `error` event, exactly like a dead network in a browser.
      envelope = null;
    }
    if (envelope === null || typeof envelope !== "object" || envelope === null) {
      this._responseHeaders = [];
      this.status = 0;
      this.statusText = "";
      this.responseText = "";
      this.response = this.responseType === "" || this.responseType === "text" ? "" : null;
      this.responseURL = "";
      this.readyState = 4;
      fireXhrEvent(this, "error");
      fireXhrEvent(this, "readystatechange");
      fireXhrEvent(this, "loadend");
      return;
    }
    this._responseHeaders = envelope.headers || [];
    this.status = envelope.status | 0;
    this.statusText = String(envelope.statusText || "");
    this.responseText = String(envelope.body || "");
    this.responseURL = String(envelope.url || this._url);
    if (this.responseType === "json") {
      try {
        this.response = JSON.parse(this.responseText);
      } catch (err) {
        this.response = null; // parse failure reads as null, per the spec
      }
    } else if (this.responseType === "" || this.responseType === "text") {
      this.response = this.responseText;
    } else {
      this.response = null; // arraybuffer/blob/document: no channel
    }
    this.readyState = 2; // HEADERS_RECEIVED
    fireXhrEvent(this, "readystatechange");
    this.readyState = 3; // LOADING (the whole body is already here)
    fireXhrEvent(this, "readystatechange");
    this.readyState = 4; // DONE
    fireXhrEvent(this, "load");
    fireXhrEvent(this, "readystatechange");
    fireXhrEvent(this, "loadend");
  };
  host.XMLHttpRequest = XMLHttpRequest;

  // Element interface classes (task: infosec.exchange). react-dom's commit
  // phase does `activeElement instanceof window.HTMLIFrameElement` (and a
  // dozen sibling checks); the realm had none of the HTML* constructors, so
  // the check threw "right-hand side of 'instanceof' should be an object" —
  // inside a scheduler flush, i.e. swallowed by the nextTick promise, i.e.
  // React died with zero console output. Our wrappers all share one
  // prototype, so the classes use `Symbol.hasInstance` to test the wrapper's
  // live nodeType/tagName: instanceof answers what the node actually is,
  // without a per-tag prototype forest under the bridge.
  function defineDomClass(name, predicate) {
    var C = function () {
      throw new TypeError("Illegal constructor");
    };
    Object.defineProperty(C, "name", { value: name, configurable: true });
    // Bind the interface prototype to the shared bridge prototype: pages and
    // polyfills read/write `Element.prototype.matches`, `HTMLElement.prototype
    // .inert`, etc., and the *same* object must answer for our wrappers (all
    // wrappers share NodeProto) — so the interface prototypes ARE NodeProto.
    // instanceof stays accurate through the Symbol.hasInstance predicate.
    if (name !== "Window" && name !== "Document") {
      Object.defineProperty(C, "prototype", { value: NodeProto, writable: false });
    }
    Object.defineProperty(C, Symbol.hasInstance, {
      value: function (x) {
        try {
          return !!predicate(x);
        } catch (ignored) {
          return false;
        }
      },
      configurable: true,
    });
    host[name] = C;
    return C;
  }
  function domNodeType(x) {
    if (x === null || x === undefined || typeof x !== "object") return 0;
    if (x === document || (x.documentElement !== undefined && x.defaultView !== undefined && x.__root === undefined)) return 9;
    if (typeof x.nodeType === "number") return x.nodeType;
    return 0;
  }
  function domTagName(x) {
    try {
      return x && typeof x.tagName === "string" ? x.tagName.toUpperCase() : "";
    } catch (ignored) {
      return "";
    }
  }
  var isElementNode = function (x) {
    return domNodeType(x) === 1;
  };
  var NodeInterface = defineDomClass("Node", function (x) {
    return domNodeType(x) !== 0 || x === document;
  });
  [
    ["ELEMENT_NODE", 1],
    ["ATTRIBUTE_NODE", 2],
    ["TEXT_NODE", 3],
    ["CDATA_SECTION_NODE", 4],
    ["ENTITY_REFERENCE_NODE", 5],
    ["ENTITY_NODE", 6],
    ["PROCESSING_INSTRUCTION_NODE", 7],
    ["COMMENT_NODE", 8],
    ["DOCUMENT_NODE", 9],
    ["DOCUMENT_TYPE_NODE", 10],
    ["DOCUMENT_FRAGMENT_NODE", 11],
    ["NOTATION_NODE", 12],
    ["DOCUMENT_POSITION_DISCONNECTED", 1],
    ["DOCUMENT_POSITION_PRECEDING", 2],
    ["DOCUMENT_POSITION_FOLLOWING", 4],
    ["DOCUMENT_POSITION_IMPLEMENTATION_SPECIFIC", 8],
    ["DOCUMENT_POSITION_CONTAINS", 16],
    ["DOCUMENT_POSITION_CONTAINED_BY", 32],
  ].forEach(function (pair) {
    Object.defineProperty(NodeInterface, pair[0], { value: pair[1] });
  });
  defineDomClass("Element", isElementNode);
  defineDomClass("HTMLElement", isElementNode);
  defineDomClass("Document", function (x) {
    return x === document || (domNodeType(x) === 9 ? true : false);
  });
  defineDomClass("Window", function (x) {
    return x === host;
  });
  defineDomClass("Text", function (x) {
    return domNodeType(x) === 3;
  });
  defineDomClass("Comment", function (x) {
    return domNodeType(x) === 8;
  });
  defineDomClass("DocumentFragment", function (x) {
    return domNodeType(x) === 11;
  });
  var htmlTagClasses = [
    ["IFRAME", "HTMLIFrameElement"],
    ["DIV", "HTMLDivElement"],
    ["SPAN", "HTMLSpanElement"],
    ["A", "HTMLAnchorElement"],
    ["IMG", "HTMLImageElement"],
    ["P", "HTMLParagraphElement"],
    ["UL", "HTMLUListElement"],
    ["OL", "HTMLOListElement"],
    ["LI", "HTMLLIElement"],
    ["INPUT", "HTMLInputElement"],
    ["TEXTAREA", "HTMLTextAreaElement"],
    ["BUTTON", "HTMLButtonElement"],
    ["SELECT", "HTMLSelectElement"],
    ["OPTION", "HTMLOptionElement"],
    ["OPTGROUP", "HTMLOptGroupElement"],
    ["FORM", "HTMLFormElement"],
    ["LABEL", "HTMLLabelElement"],
    ["FIELDSET", "HTMLFieldSetElement"],
    ["LEGEND", "HTMLLegendElement"],
    ["TABLE", "HTMLTableElement"],
    ["THEAD", "HTMLTableSectionElement"],
    ["TBODY", "HTMLTableSectionElement"],
    ["TFOOT", "HTMLTableSectionElement"],
    ["TR", "HTMLTableRowElement"],
    ["TD", "HTMLTableCellElement"],
    ["TH", "HTMLTableHeaderCellElement"],
    ["CANVAS", "HTMLCanvasElement"],
    ["VIDEO", "HTMLVideoElement"],
    ["AUDIO", "HTMLAudioElement"],
    ["SOURCE", "HTMLSourceElement"],
    ["TRACK", "HTMLTrackElement"],
    ["PICTURE", "HTMLPictureElement"],
    ["BODY", "HTMLBodyElement"],
    ["HEAD", "HTMLHeadElement"],
    ["HTML", "HTMLHtmlElement"],
    ["TITLE", "HTMLTitleElement"],
    ["META", "HTMLMetaElement"],
    ["LINK", "HTMLLinkElement"],
    ["SCRIPT", "HTMLScriptElement"],
    ["STYLE", "HTMLStyleElement"],
    ["NOSCRIPT", "HTMLNoScriptElement"],
    ["TEMPLATE", "HTMLTemplateElement"],
    ["DIALOG", "HTMLDialogElement"],
    ["DETAILS", "HTMLDetailsElement"],
    ["SUMMARY", "HTMLSummaryElement"],
    ["PRE", "HTMLPreElement"],
    ["CODE", "HTMLCodeElement"],
    ["B", "HTMLBRElement"],
    ["STRONG", "HTMLStrongElement"],
    ["EM", "HTMLEmElement"],
    ["U", "HTMLUElement"],
    ["I", "HTMLIElement"],
    ["H1", "HTMLHeadingElement"],
    ["H2", "HTMLHeadingElement"],
    ["H3", "HTMLHeadingElement"],
    ["H4", "HTMLHeadingElement"],
    ["H5", "HTMLHeadingElement"],
    ["H6", "HTMLHeadingElement"],
    ["HEADER", "HTMLElement"],
    ["FOOTER", "HTMLElement"],
    ["NAV", "HTMLElement"],
    ["MAIN", "HTMLElement"],
    ["SECTION", "HTMLElement"],
    ["ARTICLE", "HTMLElement"],
    ["ASIDE", "HTMLElement"],
    ["FIGURE", "HTMLElement"],
    ["FIGCAPTION", "HTMLElement"],
    ["BLOCKQUOTE", "HTMLElement"],
    ["SMALL", "HTMLElement"],
    ["BR", "HTMLBreakElement"],
    ["HR", "HTMLHRElement"],
    ["DL", "HTMLDListElement"],
    ["DT", "HTMLDTElement"],
    ["DD", "HTMLDDElement"],
    ["CAPTION", "HTMLElement"],
    ["COL", "HTMLElement"],
    ["COLGROUP", "HTMLElement"],
    ["OBJECT", "HTMLObjectElement"],
    ["PARAM", "HTMLParamElement"],
    ["EMBED", "HTMLEmbedElement"],
    ["AREA", "HTMLAreaElement"],
    ["MAP", "HTMLMapElement"],
    ["BASE", "HTMLBaseElement"],
    ["PROGRESS", "HTMLProgressElement"],
    ["METER", "HTMLMeterElement"],
    ["DATA", "HTMLDataElement"],
    ["TIME", "HTMLTimeElement"],
    ["OUTPUT", "HTMLOutputElement"],
    ["RUBY", "HTMLElement"],
    ["RB", "HTMLElement"],
    ["RTC", "HTMLElement"],
    ["RP", "HTMLElement"],
    ["RT", "HTMLElement"],
    ["BDO", "HTMLElement"],
    ["BDI", "HTMLElement"],
    ["WBR", "HTMLElement"],
    ["ABBR", "HTMLElement"],
    ["CITE", "HTMLElement"],
    ["DFN", "HTMLElement"],
    ["KBD", "HTMLElement"],
    ["SAMP", "HTMLElement"],
    ["VAR", "HTMLElement"],
    ["SUB", "HTMLElement"],
    ["SUP", "HTMLElement"],
    ["INS", "HTMLElement"],
    ["DEL", "HTMLElement"],
    ["Q", "HTMLElement"],
    ["MARK", "HTMLElement"],
    ["PICTURE", "HTMLPictureElement"],
    ["PICTURE", "HTMLPictureElement"],
  ];
  var svgTagClasses = {
    SVG: "SVGSVGElement",
    PATH: "SVGPathElement",
    CIRCLE: "SVGCircleElement",
    RECT: "SVGRectElement",
    LINE: "SVGLineElement",
    POLYLINE: "SVGPolylineElement",
    POLYGON: "SVGPolygonElement",
    ELLIPSE: "SVGEllipseElement",
    G: "SVGGElement",
    DEFS: "SVGDefsElement",
    USE: "SVGUseElement",
    SYMBOL: "SVGSymbolElement",
    TEXT: "SVGTextElement",
    TSPAN: "SVGTSpanElement",
    IMAGE: "SVGImageElement",
    LINEARGRADIENT: "SVGLinearGradientElement",
    RADIALGRADIENT: "SVGRadialGradientElement",
    STOP: "SVGStopElement",
    CLIPPATH: "SVGClipPathElement",
    MASK: "SVGMaskElement",
    FILTER: "SVGFilterElement",
    FEGAUSSIANBLUR: "SVGFEGaussianBlurElement",
    FEDROPLIGHTING: "SVGFEDropShadowElement",
    TITLE: "SVGTitleElement",
    CIRCLE: "SVGCircleElement",
  };
  // cname -> set of tags that satisfy `instanceof <cname>` (HEADINGS, table
  // sections and the generic HTMLElement share one class across tags, so we
  // group first and register exactly one constructor per interface name).
  var htmlTagsByClass = Object.create(null);
  for (var hci = 0; hci < htmlTagClasses.length; hci++) {
    var cname = htmlTagClasses[hci][1];
    if (cname === "HTMLElement") continue;
    (htmlTagsByClass[cname] || (htmlTagsByClass[cname] = [])).push(htmlTagClasses[hci][0]);
  }
  Object.keys(svgTagClasses).forEach(function (tag) {
    var cname = svgTagClasses[tag];
    (htmlTagsByClass[cname] || (htmlTagsByClass[cname] = [])).push(tag);
  });
  Object.keys(htmlTagsByClass).forEach(function (cname) {
    if (host[cname]) return; // e.g. HTMLMediaElement already real
    var tags = {};
    htmlTagsByClass[cname].forEach(function (t) {
      tags[t] = true;
    });
    defineDomClass(cname, function (x) {
      return isElementNode(x) && tags[domTagName(x)] === true;
    });
  });
  // SVGSVGElement/SVGElement base: any lowercase-tagged node with an SVG-ish
  // namespace tag — cheap proxy: tagName in the svg map, or the node answers
  // ownerSVGElement-style checks (we simply treat tags not in the HTML table
  // and not common HTML as SVG when the tag is in svgTagClasses values).
  if (!host.SVGElement) {
    defineDomClass("SVGElement", function (x) {
      if (!isElementNode(x)) return false;
      var tag = domTagName(x);
      for (var key in svgTagClasses) {
        if (svgTagClasses[key] === undefined) continue;
        if (tag === key) return true;
      }
      return false;
    });
  }
  // document.activeElement: the element `focus()` last named, else the body
  // (spec default). One focused surface — see the `focus()` glue above.
  if (document && document.activeElement === undefined) {
    Object.defineProperty(document, "activeElement", {
      get: function () {
        return __focusedElement || document.body || document.documentElement;
      },
      configurable: true,
    });
  }

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
pub fn build_context(host: &HostRef, timers: &Rc<RefCell<TimerQueue>>) -> Context {
    build_context_with_modules(host, timers, None)
}

/// [`build_context`] with the document's ES-module loader installed on the
/// realm (task: `<script type="module">` loading). Boa's default module
/// loader is a **filesystem** loader (`SimpleModuleLoader` over the cwd) — a
/// browser tab has no business with one; when `module_loader` is given the
/// realm is built so every static and dynamic import of the realm resolves
/// through [`UrlModuleLoader`]: the document's import map, then the tab's own
/// network layer.
fn build_context_with_modules(
    host: &HostRef,
    _timers: &Rc<RefCell<TimerQueue>>,
    module_loader: Option<Rc<UrlModuleLoader>>,
) -> Context {
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

    let mut context = match module_loader {
        None => Context::default(),
        Some(loader) => match Context::builder().module_loader(loader).build() {
            Ok(context) => context,
            Err(err) => {
                error!("module-loader realm failed to build: {err}; falling back to the default realm (modules will not load)");
                Context::default()
            }
        },
    };

    // Full Boa runtime Web API (task: as much JavaScript as possible): every
    // boa_engine language feature is compiled in (annex-b, bundled-ICU Intl,
    // Temporal, … — see Cargo.toml), and the whole `boa_runtime` surface lands
    // on the realm FIRST: WinterTC globals (`atob`/`btoa`, `queueMicrotask`,
    // `structuredClone`, `TextEncoder`/`TextDecoder`, `AbortController`/
    // `AbortSignal`, `URL`), a richer `console`, `process`, and `fetch`
    // (reqwest's blocking client; its promise settles from the job-queue
    // drain that runs after every script turn). Registering before the glue
    // below is deliberate: the IronSurf bindings then OVERRIDE the names the
    // tab must own — `console` (stderr + the per-navigation capture) and the
    // timers (`setTimeout`/`setInterval`/`clearTimeout`/`clearInterval` → the
    // tab's poll loop, not the runtime's internal timer channel). Plain
    // assignments in the (sloppy-mode) prelude replace these bindings cleanly;
    // everything else (URL, Intl, TextEncoder, AbortController, …) is purely
    // additive.
    if let Err(err) = boa_runtime::register(
        (
            boa_runtime::extensions::ConsoleExtension::default(),
            boa_runtime::extensions::ProcessExtension,
            boa_runtime::extensions::FetchExtension(
                boa_runtime::fetch::BlockingReqwestFetcher::default(),
            ),
        ),
        None,
        &mut context,
    ) {
        error!("boa_runtime globals failed to install: {err}");
    }
    // The tab leaks no environment: `process` stays (pages feature-detect it)
    // but its env map is emptied before any page code runs.
    if let Err(err) = context.eval(Source::from_bytes("process.env = {};")) {
        error!("process.env reset failed: {err}");
    }

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

    // navigator.userAgent (task: per-container UA): reads the tab's live
    // effective-UA cell, so a `ToTab::SetUserAgent` changes the answer from
    // the next read on — no reload, no new Context. Without a tab attached
    // (bare JS tests) the engine default is reported.
    if let Err(err) = context.register_global_builtin_callable(
        JsString::from(native::USER_AGENT),
        0,
        NativeFunction::from_copy_closure(|_this, _args, _context| {
            let user_agent = with_host(|host| {
                host.user_agent.as_ref().map(|cell| {
                    cell.lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .clone()
                })
            })
            .flatten()
            .unwrap_or_else(|| crate::netmod::USER_AGENT.to_owned());
            Ok(JsValue::from(JsString::from(user_agent)))
        }),
    ) {
        error!("js binding `userAgent` failed to install: {err}");
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
            let (url, userscript) = with_host(|host| {
                // Tag the line with the userscript that is running (if any):
                // the tab turns `Some(name)` into a `source: "userscript"`
                // console entry prefixed with the same name.
                let userscript = host.userscript.clone();
                host.console.push(ConsoleLine {
                    level,
                    text: text.clone(),
                    userscript: userscript.clone(),
                });
                (host.url.clone(), userscript)
            })
            .unwrap_or_default();
            log_console(level, &url, &text, userscript.as_deref());
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
    // `history.pushState`/`replaceState` (task: infosec.exchange): a URL-only
    // change — the tab re-points the address bar (`UrlChanged`) without any
    // fetch; resolution + the same-origin guard live on the Rust side of this
    // native (see `Tab::follow_pending_js_url`), never in page code.
    if let Err(err) = context.register_global_builtin_callable(
        JsString::from(native::SET_URL),
        1,
        NativeFunction::from_copy_closure(|_this, args, context| {
            let target = args
                .first()
                .cloned()
                .unwrap_or(JsValue::undefined())
                .to_string(context)?
                .to_std_string_escaped();
            if !target.is_empty() {
                with_host(|host| {
                    host.pending_url = Some(target);
                });
            }
            Ok(JsValue::undefined())
        }),
    ) {
        error!("js binding `history` failed to install: {err}");
    }
    // window.alert/confirm/prompt (task: JS dialogs): every argument is
    // coerced first (a page's own `valueOf`/`toString` could re-enter JS),
    // then — with a dialog host attached — the call BLOCKS the script until
    // the shell dismisses its dialog. The return shape is the native's call
    // per kind: alert `undefined`, confirm a boolean, prompt a string or
    // `null`; without a host the cancelled readings come back immediately.
    if let Err(err) = context.register_global_builtin_callable(
        JsString::from(native::DIALOG),
        3,
        NativeFunction::from_copy_closure(|_this, args, context| {
            let kind_raw = args
                .first()
                .cloned()
                .unwrap_or(JsValue::undefined())
                .to_string(context)?
                .to_std_string_escaped();
            let message = args
                .get(1)
                .cloned()
                .unwrap_or(JsValue::undefined())
                .to_string(context)?
                .to_std_string_escaped();
            let default = match args.get(2) {
                Some(value) if !value.is_undefined() && !value.is_null() => Some(
                    value
                        .to_string(context)?
                        .to_std_string_escaped(),
                ),
                _ => None,
            };
            let kind = match kind_raw.as_str() {
                "confirm" => DialogKind::Confirm,
                "prompt" => DialogKind::Prompt,
                _ => DialogKind::Alert,
            };
            let reply = match active_dialog() {
                Some(host) => host.ask(kind, &message, default.as_deref()),
                // No IPC shell behind this context (tests, headless): the
                // cancelled answer, no wait.
                None => crate::runtime::DialogReply::default(),
            };
            Ok(match kind {
                DialogKind::Alert => JsValue::undefined(),
                DialogKind::Confirm => JsValue::from(reply.confirmed),
                DialogKind::Prompt => {
                    if reply.confirmed {
                        JsValue::from(JsString::from(reply.text.unwrap_or_default()))
                    } else {
                        JsValue::null()
                    }
                }
            })
        }),
    ) {
        error!("js binding `dialog` failed to install: {err}");
    }
    // window.localStorage (task: per-origin DOM storage): the whole Storage
    // surface funnels through this ONE native, and every call answers an
    // envelope `[status, payload]` — "ok", "denied" or "quota" — that the
    // prelude turns into a SecurityError / QuotaExceededError. The security
    // lives HERE, not in JavaScript: the area key is the document origin
    // re-resolved from the host URL on EVERY call (never cached across
    // documents or calls), only http/https have an area at all, and every
    // operation on an opaque origin answers "denied". A missing store cell
    // (bare tests) denies the same way. Coercion runs before the host is
    // touched — the convention every native follows.
    if let Err(err) = context.register_global_builtin_callable(
        JsString::from(native::STORAGE),
        4,
        NativeFunction::from_copy_closure(|_this, args, context| {
            let envelope = |status: &str, payload: JsValue, context: &mut Context| {
                JsValue::from(boa_engine::object::builtins::JsArray::from_iter(
                    [JsValue::from(JsString::from(status)), payload],
                    context,
                ))
            };
            let op = args
                .first()
                .cloned()
                .unwrap_or(JsValue::undefined())
                .to_string(context)?
                .to_std_string_escaped();
            let key = arg_opt_string(args.get(1), context)?;
            let value = arg_opt_string(args.get(2), context)?;
            let index = args
                .get(3)
                .cloned()
                .unwrap_or(JsValue::undefined())
                .to_number(context)
                .map(|n| if n.is_finite() && n >= 0.0 { n as usize } else { usize::MAX })?;
            let (store, url) = with_host(|host| (host.local_storage.clone(), host.url.clone()))
                .unwrap_or((None, String::new()));
            let origin = ironsurf_storage::storage_origin_key(&url);
            if op == "origin" {
                // Non-throwing probe (tests, diagnostics): the area key this
                // document's calls read/write, or null for no durable origin.
                let payload = origin
                    .as_ref()
                    .map(|key| JsValue::from(JsString::from(key.clone())))
                    .unwrap_or(JsValue::null());
                return Ok(envelope("ok", payload, context));
            }
            let (Some(store), Some(origin)) = (store.as_ref(), origin.as_deref()) else {
                return Ok(envelope("denied", JsValue::null(), context));
            };
            let mut store = store
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            match op.as_str() {
                "get" => {
                    let got = key
                        .as_deref()
                        .and_then(|key| store.get(origin, key))
                        .map(|value| JsValue::from(JsString::from(value)))
                        .unwrap_or(JsValue::null());
                    Ok(envelope("ok", got, context))
                }
                "len" => Ok(envelope(
                    "ok",
                    JsValue::new(store.len(origin) as u32),
                    context,
                )),
                "key" => {
                    let name = store
                        .key(origin, index)
                        .map(|name| JsValue::from(JsString::from(name)))
                        .unwrap_or(JsValue::null());
                    Ok(envelope("ok", name, context))
                }
                "set" => match store.set(origin, &key.unwrap_or_default(), &value.unwrap_or_default()) {
                    Ok(()) => Ok(envelope("ok", JsValue::null(), context)),
                    Err(ironsurf_storage::StoreError::QuotaExceeded { .. }) => {
                        Ok(envelope("quota", JsValue::null(), context))
                    }
                    Err(err) => {
                        // The in-memory area updated fine; only persistence
                        // failed. The page keeps its data — the tab complains.
                        warn!("localStorage persist failed for {origin}: {err}");
                        Ok(envelope("ok", JsValue::null(), context))
                    }
                },
                "remove" => {
                    let removed = match store.remove(origin, &key.unwrap_or_default()) {
                        Ok(removed) => removed,
                        Err(err) => {
                            warn!("localStorage persist failed for {origin}: {err}");
                            false
                        }
                    };
                    Ok(envelope("ok", JsValue::from(removed), context))
                }
                "clear" => {
                    if let Err(err) = store.clear(origin) {
                        warn!("localStorage persist failed for {origin}: {err}");
                    }
                    Ok(envelope("ok", JsValue::null(), context))
                }
                "keys" => {
                    // Insertion order — the same sequence `key(i)` reports,
                    // which is what `Object.keys(localStorage)` must show.
                    let len = store.len(origin);
                    let keys: Vec<JsValue> = (0..len)
                        .filter_map(|i| store.key(origin, i))
                        .map(|name| JsValue::from(JsString::from(name)))
                        .collect();
                    let array = boa_engine::object::builtins::JsArray::from_iter(keys, context);
                    Ok(envelope("ok", JsValue::from(array), context))
                }
                // Unknown ops are inert (only our prelude calls this native).
                _ => Ok(envelope("ok", JsValue::null(), context)),
            }
        }),
    ) {
        error!("js binding `localStorage` failed to install: {err}");
    }
    // document.cookie (task: the axios XSRF read — Mastodon's request layer
    // did `document.cookie.match(...)`, read `undefined`, and died with boa's
    // "cannot convert 'null' or 'undefined' to object"). ONE native funnels
    // both directions, and all rules live HERE, not in JavaScript: the
    // document URL is re-parsed from the host on EVERY call (never cached),
    // only http/https documents have cookies at all, reads answer the jar's
    // script-visible view (domain/path/Secure/expiry matching identical to
    // the wire `Cookie:` header, `HttpOnly` entries hidden), and writes are
    // fed through the jar's RFC6265 `Set-Cookie` parser — after the native
    // strips any `HttpOnly` attribute, since scripts can never set one.
    // No jar / no http(s) URL ⇒ reads answer "" and writes no-op (the
    // spec's behaviour for opaque origins, and the honest answer offline).
    if let Err(err) = context.register_global_builtin_callable(
        JsString::from(native::COOKIES),
        2,
        NativeFunction::from_copy_closure(|_this, args, context| {
            let op = args
                .first()
                .cloned()
                .unwrap_or(JsValue::undefined())
                .to_string(context)?
                .to_std_string_escaped();
            // Coercion runs before the host is touched (the rule every
            // native follows).
            let raw = arg_opt_string(args.get(1), context)?;
            let (jar, url) = with_host(|host| (host.cookies.clone(), host.url.clone()))
                .unwrap_or((None, String::new()));
            let document_url = Url::parse(&url).ok().filter(|u| {
                matches!(u.scheme(), "http" | "https") && u.host_str().is_some()
            });
            match op.as_str() {
                "get" => {
                    let text = match (&jar, document_url.as_ref()) {
                        (Some(jar), Some(document_url)) => {
                            let jar = jar
                                .lock()
                                .unwrap_or_else(|poisoned| poisoned.into_inner());
                            jar.document_cookie(document_url, crate::netmod::now_unix())
                        }
                        _ => String::new(),
                    };
                    Ok(JsValue::from(JsString::from(text)))
                }
                "set" => {
                    let Some(raw) = raw.filter(|raw| !raw.is_empty()) else {
                        return Ok(JsValue::from(false));
                    };
                    // A cookie set from a script is NEVER HttpOnly: strip
                    // the attribute before the jar sees it (RFC 6265 §5.3
                    // reserves it for the `Set-Cookie` path only).
                    let mut parts = raw.splitn(2, ';');
                    let name_value = parts.next().unwrap_or("");
                    let kept: Vec<String> = parts
                        .next()
                        .unwrap_or("")
                        .split(';')
                        .filter(|attr| {
                            let key = attr
                                .trim()
                                .to_ascii_lowercase()
                                .split('=')
                                .next()
                                .unwrap_or("")
                                .to_string();
                            key != "httponly"
                        })
                        .map(|attr| attr.to_string())
                        .collect();
                    let header = if kept.is_empty() {
                        name_value.to_string()
                    } else {
                        format!("{name_value};{}", kept.join(";"))
                    };
                    match (&jar, document_url.as_ref()) {
                        (Some(jar), Some(document_url)) => {
                            let mut jar = jar
                                .lock()
                                .unwrap_or_else(|poisoned| poisoned.into_inner());
                            jar.set_from_set_cookie(document_url, &header, crate::netmod::now_unix());
                            if let Err(err) = jar.save() {
                                // The live jar updated; only persistence
                                // failed — the cookie still rides and reads.
                                warn!("cookie persist failed for {url}: {err}");
                            }
                            Ok(JsValue::from(true))
                        }
                        _ => Ok(JsValue::from(false)),
                    }
                }
                // Non-throwing probe (tests, diagnostics): what a GET would
                // answer for, or whether a jar exists at all.
                "url" => Ok(JsValue::from(JsString::from(
                    document_url
                        .as_ref()
                        .map(|u| u.to_string())
                        .unwrap_or_default(),
                ))),
                "has-jar" => Ok(JsValue::from(jar.is_some())),
                // Unknown ops are inert (only our prelude calls this native).
                _ => Ok(JsValue::undefined()),
            }
        }),
    ) {
        error!("js binding `cookies` failed to install: {err}");
    }

    // `XMLHttpRequest` send (task: infosec.exchange posts): every argument is
    // coerced first (the page's own `toString` could re-enter JS), the host's
    // handle is cloned out BEFORE the request runs — no host borrow is held
    // across the blocking exchange (the dialog native's rule). The reply
    // rides back as a JSON envelope `{status,statusText,url,headers,body}`;
    // a transport failure is a THROWN error message the glue turns into the
    // spec's `error` event. The tab has no network without a live host, so
    // bare realms get the same thrown "no network" every send answers there.
    if let Err(err) = context.register_global_builtin_callable(
        JsString::from(native::XHR),
        4,
        NativeFunction::from_copy_closure(|_this, args, context| {
            let method = args
                .first()
                .cloned()
                .unwrap_or(JsValue::undefined())
                .to_string(context)?
                .to_std_string_escaped();
            let url = args
                .get(1)
                .cloned()
                .unwrap_or(JsValue::undefined())
                .to_string(context)?
                .to_std_string_escaped();
            let headers_json = arg_opt_string(args.get(2), context)?.unwrap_or_default();
            // A null/undefined body means NO body; anything else is the
            // string the page handed `send()` (FormData/Blob/ArrayBuffer
            // bodies stay out of this glue — the envelope is text).
            let body = match args.get(3) {
                Some(value) if !value.is_undefined() && !value.is_null() => Some(
                    value
                        .to_string(context)?
                        .to_std_string_escaped(),
                ),
                _ => None,
            };
            let fetcher = with_host(|host| host.xhr.clone()).flatten();
            let Some(fetcher) = fetcher else {
                return Err(JsNativeError::typ()
                    .with_message("XMLHttpRequest: no network host attached")
                    .into());
            };
            let parsed: Vec<(String, String)> = serde_json::from_str(&headers_json)
                .unwrap_or_default();
            let reply = fetcher(&method, url.as_str(), &parsed, body.as_deref())
                .map_err(|msg| JsNativeError::typ().with_message(msg))?;
            let envelope = serde_json::json!({
                "status": reply.status,
                "statusText": reply.status_text,
                "url": reply.final_url,
                "headers": reply.headers,
                "body": reply.body,
            });
            let text = serde_json::to_string(&envelope)
                .map_err(|err| JsNativeError::typ().with_message(format!("xhr: {err}")))?;
            Ok(JsValue::from(JsString::from(text)))
        }),
    ) {
        error!("js binding `xhr` failed to install: {err}");
    }

    // `crypto.getRandomValues`'s byte source (see `next_random_u64`). The
    // answer is one char per byte (codes ≤ 0xFF, always valid UTF-16) so the
    // glue reads it back with `charCodeAt` — no JSON parse per call.
    if let Err(err) = context.register_global_builtin_callable(
        JsString::from(native::RANDOM_BYTES),
        1,
        NativeFunction::from_copy_closure(|_this, args, context| {
            let len = args
                .first()
                .cloned()
                .unwrap_or(JsValue::undefined())
                .to_u32(context)? as usize;
            let len = len.min(65_536);
            let mut codes = Vec::with_capacity(len);
            for _ in 0..len {
                codes.push((next_random_u64() & 0xFF) as u16);
            }
            Ok(JsValue::from(JsString::from(String::from_utf16_lossy(&codes))))
        }),
    ) {
        error!("js binding `randomBytes` failed to install: {err}");
    }

    // `crypto.randomUUID`: a v4 UUID — 16 random bytes with the version and
    // variant nibbles set per spec.
    if let Err(err) = context.register_global_builtin_callable(
        JsString::from(native::RANDOM_UUID),
        0,
        NativeFunction::from_copy_closure(|_this, _args, _context| {
            let mut b = [0u8; 16];
            for (i, slot) in b.iter_mut().enumerate() {
                *slot = ((next_random_u64() >> (8 * (i % 8))) & 0xFF) as u8;
            }
            b[6] = (b[6] & 0x0F) | 0x40;
            b[8] = (b[8] & 0x3F) | 0x80;
            let hex = b.iter().map(|x| format!("{x:02x}")).collect::<String>();
            let uuid = format!(
                "{}-{}-{}-{}-{}",
                &hex[0..8],
                &hex[8..12],
                &hex[12..16],
                &hex[16..20],
                &hex[20..32]
            );
            Ok(JsValue::from(JsString::from(uuid)))
        }),
    ) {
        error!("js binding `randomUUID` failed to install: {err}");
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
/// `FromTab::StatusText` **and** as `FromTab::ConsoleLog`; never fatal to the
/// tab.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScriptError {
    pub origin: String,
    pub message: String,
    /// DevTools console origin: `"js"` for engine/script errors, `"dom"`
    /// when the throw came out of a native DOM binding (matched against the
    /// [`HostState::dom_throws`] recorded during the very evaluation that
    /// failed). A small honest overlap is accepted: a DOM throw whose message
    /// a page also throws as plain JS reads as `"dom"` for that turn.
    pub source: &'static str,
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
    /// The document's `<script type="importmap">` (task: importmap support),
    /// installed by the tab right after extraction — and pushed on to the
    /// module loader, which resolves every specifier through it (task:
    /// `<script type="module">` loading).
    import_map: Option<ImportMap>,
    /// The document's module graph (task: `<script type="module">` loading):
    /// installed on the realm at construction, fed the import map above.
    /// `None` (the [`JsRuntime::new`] shape) = no module ever loads: every
    /// specifier the engine asks about is reported unresolvable.
    modules: Option<Rc<UrlModuleLoader>>,
}

impl JsRuntime {
    /// Fresh realm for one navigation, seeded from `host` — **without**
    /// module support (a realm whose module graph nobody can fetch): the live
    /// tab builds its runtime through [`new_with_modules`](Self::new_with_modules).
    pub fn new(host: HostRef) -> Self {
        let timers = Rc::new(RefCell::new(TimerQueue::default()));
        let context = build_context(&host, &timers);
        JsRuntime {
            context,
            host,
            timers,
            started: Instant::now(),
            import_map: None,
            modules: None,
        }
    }

    /// Fresh realm for one navigation **with** ES-module support (task:
    /// `<script type="module">` loading): `base` is the document URL (the
    /// referrer inline modules and referrer-less `import()` calls resolve
    /// against), `fetch` the tab's own network handle — module bodies ride
    /// the same jar, cache, DevTools reporting and archive sink as classic
    /// `<script src>` bodies.
    pub fn new_with_modules(host: HostRef, base: &Url, fetch: ModuleFetch) -> Self {
        let timers = Rc::new(RefCell::new(TimerQueue::default()));
        let loader = Rc::new(UrlModuleLoader::new(base.clone(), fetch));
        let context = build_context_with_modules(&host, &timers, Some(Rc::clone(&loader)));
        JsRuntime {
            context,
            host,
            timers,
            started: Instant::now(),
            import_map: None,
            modules: Some(loader),
        }
    }

    /// The document's module loader, if this realm has one (diagnostics,
    /// tests: the resolved module count).
    pub fn module_loader(&self) -> Option<&Rc<UrlModuleLoader>> {
        self.modules.as_ref()
    }

    /// Install the document's import map (from [`Extraction::import_map`]).
    /// `None` clears it (a document without a map, or with an invalid one).
    /// The module loader is fed the same map: bare specifiers resolve through
    /// it, `scopes` key on the importing module's URL (task: module loading).
    pub fn set_import_map(&mut self, import_map: Option<ImportMap>) {
        if let Some(loader) = &self.modules {
            loader.set_import_map(import_map.clone());
        }
        self.import_map = import_map;
    }

    /// The document's import map, if it declared a valid one.
    pub fn import_map(&self) -> Option<&ImportMap> {
        self.import_map.as_ref()
    }

    /// The host cell the bindings write through (title, console, notes).
    pub fn host(&self) -> &HostRef {
        &self.host
    }

    /// Did a native DOM binding throw during the evaluation that just failed
    /// with `message` (given the DOM-throw log had `dom_before` entries when
    /// the evaluation started)? A message match against the newest recorded
    /// native error keeps a page's own `throw new SyntaxError(...)` on the
    /// `"js"` side even when a DOM native threw earlier in the same turn.
    fn classify_throw(&self, dom_before: usize, message: &str) -> &'static str {
        let host = self.host.borrow();
        if host.dom_throws.len() > dom_before {
            if let Some(last) = host.dom_throws.last() {
                if message.contains(last.as_str())
                    || last.contains(message)
                    // Boa appends a position suffix to every error — `(native)`
                    // at the binding's catch, `(unknown at :1:23)` (or `(at …)`)
                    // at the eval catch — so the two reports of the SAME throw
                    // differ by that suffix alone. Compare them with it stripped.
                    || (crate::js::without_engine_suffix(message)
                        == crate::js::without_engine_suffix(last)
                        && !without_engine_suffix(message).is_empty())
                {
                    return "dom";
                }
            }
        }
        "js"
    }

    /// Take the navigation this document's scripts queued since the last turn
    /// (`location.href=`/`assign`/`replace`/`reload`). The tab calls this
    /// after every JS turn and follows it through its own navigation entry
    /// point — never re-entering the stack that queued it.
    pub fn take_pending_navigation(&self) -> Option<PendingNav> {
        self.host.borrow_mut().pending_nav.take()
    }

    /// Take the URL-only address-bar change `history.pushState`/`replaceState`
    /// queued during this JS turn (see the `SET_URL` native). The tab applies
    /// it — resolve, same-origin, `UrlChanged` — never refetching.
    pub fn take_pending_url(&self) -> Option<String> {
        self.host.borrow_mut().pending_url.take()
    }

    /// Re-point the realm's `location.href` after an applied pushState: the
    /// address bar and what page scripts read must agree.
    pub fn set_url(&self, url: &str) {
        self.host.borrow_mut().url = url.to_string();
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
        let dom_before = self.host.borrow().dom_throws.len();
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
                let source = self.classify_throw(dom_before, &message);
                error!("javascript error in javascript: URL: {message}");
                Err(ScriptError {
                    origin: "javascript:".to_string(),
                    message,
                    source,
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
            let dom_before = self.host.borrow().dom_throws.len();
            if script.kind == ScriptKind::Module {
                // `<script type="module">` (task: module loading): parsed as
                // a module, its graph fetched + evaluated through the
                // document loader, the completion promise driven to rest
                // before the NEXT script runs (modules are deferred; the
                // document-order batch already put them in order).
                if let Some(err) = self.run_module(&script.code, &script.origin, dom_before) {
                    errors.push(err);
                }
                continue;
            }
            match self.context.eval(Source::from_bytes(script.code.as_str())) {
                Ok(_) => {}
                Err(err) => {
                    let message = first_line(&err);
                    let backtrace = format!("{err:?}");
                    if !backtrace.trim().is_empty() {
                        error!("javascript error backtrace:\n{backtrace}");
                    }
                    let source = self.classify_throw(dom_before, &message);
                    error!("javascript error in {origin}: {message}", origin = script.origin);
                    errors.push(ScriptError {
                        origin: script.origin.clone(),
                        message,
                        source,
                    });
                }
            }
        }
        // One microtask drain per script turn (after the whole batch, like a
        // classic script's job checkpoint): `Promise.then` callbacks and
        // `boa_runtime` async jobs (fetch) run here, with the live document
        // still registered.
        if let Some(err) = self.drain_jobs() {
            errors.push(err);
        }
        errors
    }

    /// Evaluate one ES module (task: `<script type="module">` loading): parse
    /// as a module whose path is its URL (`import.meta.url`, and the base
    /// `UrlModuleLoader` resolves its imports from), file it with the loader
    /// so its own URL round-trips to this very instance, then run Boa's
    /// `load → link → evaluate` pipeline and drive the job queue until the
    /// completion promise settles — the graph fetches (one job per import,
    /// our loader's futures born ready) and any microtask the module body
    /// queued all land here. A graph failure (unresolvable specifier, failed
    /// fetch, link clash, thrown body) becomes a [`ScriptError`] like any
    /// classic script's throw: reported, never fatal.
    fn run_module(
        &mut self,
        code: &str,
        url: &str,
        dom_before: usize,
    ) -> Option<ScriptError> {
        let report = |message: String, source: &'static str| ScriptError {
            origin: url.to_string(),
            message,
            source,
        };
        let Some(loader) = self.modules.clone() else {
            return Some(report(
                "module loading is unavailable in this realm".to_string(),
                "js",
            ));
        };
        let base = match Url::parse(url) {
            Ok(base) => base,
            Err(err) => {
                return Some(report(
                    format!("module url {url:?} is not absolute: {err}"),
                    "js",
                ))
            }
        };
        let code = debug_chunk_transform(code.to_string());
        let code = code.as_str();
        let path = PathBuf::from(url);
        let module = match Module::parse(
            Source::from_bytes(code.as_bytes()).with_path(&path),
            None,
            &mut self.context,
        ) {
            Ok(module) => module,
            Err(err) => {
                let message = first_line(&err);
                let source = self.classify_throw(dom_before, &message);
                error!("javascript error in module {url}: {message}");
                return Some(report(message, source));
            }
        };
        // The entry module is part of the document's module map before its
        // imports load: a dependency importing this URL (a cycle, or a dep
        // importing the page itself) gets THIS instance, not a second fetch.
        loader.register(&base, &module);
        let promise = module.load_link_evaluate(&mut self.context);
        // Boa drives the load graph and evaluation through job-queue futures;
        // with a synchronous loader the whole graph settles in the first
        // drain, so the extra rounds are a safety net, not the normal path.
        let mut outcome: Option<Result<(), String>> = None;
        for _ in 0..64 {
            match promise.state() {
                PromiseState::Fulfilled(_) => {
                    outcome = Some(Ok(()));
                    break;
                }
                PromiseState::Rejected(value) => {
                    outcome = Some(Err(rejection_message(&mut self.context, &value)));
                    break;
                }
                PromiseState::Pending => match self.context.run_jobs() {
                    Ok(()) => {}
                    Err(err) => {
                        // A job threw on the engine's side: if the promise
                        // settled alongside, report the rejection itself;
                        // this is the fallback reading.
                        let message = first_line(&err);
                        let backtrace = format!("{err:?}");
                        if !backtrace.trim().is_empty() {
                            error!("module job backtrace:\n{backtrace}");
                        }
                        outcome = Some(Err(if message.is_empty() {
                            "module graph failed".to_string()
                        } else {
                            message
                        }));
                        break;
                    }
                },
            }
        }
        match outcome.unwrap_or_else(|| {
            Err("module graph never settled (loader stalled pending)".to_string())
        }) {
            Ok(()) => {
                debug!("module {url} evaluated (graph: {} module(s))", loader.len());
                None
            }
            Err(message) => {
                let source = self.classify_throw(dom_before, &message);
                error!("javascript error in module {url}: {message}");
                Some(report(message, source))
            }
        }
    }

    /// Run **one userscript** (task: userscripts) in this document's realm —
    /// the same `Context`, the same host cell, the same live document (docId 0)
    /// the page's own scripts ran in, so a userscript can read and mutate the
    /// real DOM, change `document.title`, call `console.*`, and open dialogs
    /// exactly like page code.
    ///
    /// Identity is the point: `name` is published on [`HostState::userscript`]
    /// for the duration of this one evaluation, which is what makes every
    /// `console.*` line the script produces attributable (and every diagnostic
    /// line name its author). Nothing else on the host changes, so a timer
    /// callback a userscript scheduled later runs unattributed — like a page
    /// script turn — rather than pretending to be the userscript.
    ///
    /// Returns `Some(ScriptError)` when the script threw (parse errors
    /// included): `origin` = the script name, `message` prefixed with
    /// `"{name}: "`, `source` = `"userscript"` — the wire shape the DevTools
    /// console wants (`[userscript] Boom script: Error: ...`). A throw can only
    /// ever kill **this** script: the evaluation is caught, the identity field
    /// is cleared, and the tab keeps running.
    pub fn run_userscript(
        &mut self,
        name: &str,
        code: &str,
        doc: Option<&mut HtmlDocument>,
    ) -> Option<ScriptError> {
        let _active = self.activate();
        let _live_doc = doc.map(register_live_doc);
        // Publish the author for this evaluation only (single-threaded: no race,
        // and the guard below clears it on every path out of here).
        self.host.borrow_mut().userscript = Some(name.to_string());
        let dom_before = self.host.borrow().dom_throws.len();
        let outcome = self.context.eval(Source::from_bytes(code));
        self.host.borrow_mut().userscript = None;
        // Jobs queued by the userscript run unattributed (page-like turn), and
        // a job throw becomes the run's report when the script itself was ok.
        let job_error = self.drain_jobs();
        match outcome {
            // Success is silent on the console feed (attribution is for errors),
            // but never in the log: a script that runs without logging or
            // throwing must still be visible when debugging why a page was
            // (or was not) touched — the run is logged here at debug level.
            Ok(_) => {
                debug!("userscript {name:?} ran clean ({} bytes)", code.len());
                job_error
            }
            Err(err) => {
                let message = first_line(&err);
                // Native DOM throws during a userscript run are reported as the
                // userscript's own error (the script is the author a user can
                // act on); the throw log is rolled back so a stale entry can
                // never mis-classify a later page-script error as `"dom"`.
                self.host.borrow_mut().dom_throws.truncate(dom_before);
                error!("javascript error in userscript {name}: {message}");
                Some(ScriptError {
                    origin: name.to_string(),
                    message: format!("{name}: {message}"),
                    source: "userscript",
                })
            }
        }
    }

    /// Run Boa's pending **job queue** to empty. Boa 0.22 does NOT drain jobs
    /// at the end of `eval`, so without this call `Promise.then` /
    /// `queueMicrotask` callbacks would never run and `boa_runtime` async
    /// jobs (notably `fetch`) would never settle. One drain per script turn
    /// (after each batch of page scripts, timer callbacks, event dispatch and
    /// userscript) gives pages browser-like microtask semantics; a job that
    /// throws is reported like any script error (origin `"promise"`) and
    /// never kills the queue.
    fn drain_jobs(&mut self) -> Option<ScriptError> {
        let dom_before = self.host.borrow().dom_throws.len();
        match self.context.run_jobs() {
            Ok(()) => None,
            Err(err) => {
                let message = first_line(&err);
                let source = self.classify_throw(dom_before, &message);
                error!("javascript error in queued job: {message}");
                Some(ScriptError {
                    origin: "promise".to_string(),
                    message,
                    source,
                })
            }
        }
    }

    /// Evaluate one snippet with this runtime's scope active (the same world
    /// page scripts see; used by the binding tests).
    #[cfg(test)]
    pub(crate) fn eval(&mut self, source: &str) -> Result<JsValue, boa_engine::JsError> {
        let _active = self.activate();
        let outcome = self.context.eval(Source::from_bytes(source));
        self.drain_jobs();
        outcome
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
        let outcome = self.context.eval(Source::from_bytes(source));
        self.drain_jobs();
        outcome
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
        let prevented = match self.context.eval(Source::from_bytes(&source)) {
            Ok(value) => value.as_boolean().unwrap_or(false),
            Err(err) => {
                warn!("js {kind} dispatch failed: {err}");
                false
            }
        };
        // Microtasks the handlers queued run at the end of this event turn.
        self.drain_jobs();
        prevented
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
                let dom_before = self.host.borrow().dom_throws.len();
                if let Err(err) = callback.call(&JsValue::undefined(), &args, &mut self.context) {
                    let message = first_line(&err);
                    let source = self.classify_throw(dom_before, &message);
                    let backtrace = format!("{err:?}");
                    if !backtrace.trim().is_empty() {
                        error!("javascript error in timer callback backtrace:\n{backtrace}");
                    }
                    error!("javascript error in timer callback: {message}");
                    errors.push(ScriptError {
                        origin: "timer".to_string(),
                        message,
                        source,
                    });
                }
            }
        }
        // Timer callbacks form a script turn too — drain their microtasks
        // (and any fetch jobs) while the live document is still registered.
        if let Some(err) = self.drain_jobs() {
            errors.push(err);
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

/// Boa appends a parenthesised source-position suffix to every error's
/// Display ("(native)", "(unknown at :1:23)", "(at file:1:2)"). Stripping it
/// leaves the thrower's own text — the stable comparison key between a
/// binding-level catch and the eval-level catch ([`JsRuntime::classify_throw`]).
pub(crate) fn without_engine_suffix(message: &str) -> &str {
    let trimmed = message.trim_end();
    if trimmed.ends_with(')') {
        if let Some(open) = trimmed.rfind('(') {
            return trimmed[..open].trim_end();
        }
    }
    trimmed
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

    /// With no dialog host attached (bare tests; the headless harness), the
    /// three window dialogs exist on `window` and answer their cancelled
    /// readings at once — `confirm` false, `prompt` null, `alert` undefined —
    /// and never throw.
    #[test]
    fn dialogs_answer_cancelled_without_a_host() {
        let host = sample_host();
        assert!(
            host.borrow().dialog.is_none(),
            "a bare test host has no shell to answer dialogs"
        );
        assert_eq!(
            eval_snippet("typeof alert === 'function' && typeof window.alert === 'function'", &host)
                .as_boolean(),
            Some(true),
            "alert must exist as a window function"
        );
        assert_eq!(
            eval_snippet("typeof confirm === 'function' && typeof prompt === 'function'", &host)
                .as_boolean(),
            Some(true),
            "confirm/prompt must exist as window functions"
        );
        assert!(eval_snippet("window.alert('hi'); 'done'", &host)
            .as_string()
            .is_some());
        assert!(eval_snippet("alert(undefined); 'ok'", &host).as_string().is_some());
        assert_eq!(eval_snippet("confirm('Delete?')", &host).as_boolean(), Some(false));
        assert!(
            eval_snippet("prompt('Name?', 'Ada')", &host).is_null(),
            "a cancelled prompt reads null, never the default"
        );
        assert!(eval_snippet("window.prompt('Name?')", &host).is_null());
        // `window` IS the global object, so the trio is identity-stable.
        assert_eq!(
            eval_snippet("window.alert === alert && window.confirm === confirm", &host)
                .as_boolean(),
            Some(true)
        );
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

    /// `window.matchMedia` (task: startpage.com search bar): callable, with a
    /// MediaQueryList that answers trivial whole-media queries and reports
    /// `matches: false` otherwise — no media features are evaluated for
    /// scripts, and `change` never fires. What matters is that the very first
    /// inline script of startpage.com survives `window.matchMedia(...)`;
    /// before this existed it threw "not a callable function" and the page
    /// stayed `opacity: 0` forever.
    #[test]
    fn window_match_media_answers_without_evaluating_features() {
        let host = sample_host();
        assert!(
            eval_snippet("typeof window.matchMedia === 'function'", &host)
                .as_boolean()
                .unwrap_or(false),
            "matchMedia must be a callable function on window"
        );
        assert!(
            eval_snippet(
                r#"
            var mql = window.matchMedia("(prefers-color-scheme: dark)");
            mql.matches === false &&
            mql.media === "(prefers-color-scheme: dark)" &&
            typeof mql.addListener === "function" &&
            typeof mql.removeEventListener === "function" &&
            mql.onchange === null;
            "#,
                &host
            )
            .as_boolean()
            .unwrap_or(false),
            "a dark-scheme query must answer (matches=false), never throw"
        );
        // Listeners register and unregister as no-ops; nothing fires.
        assert!(
            eval_snippet(
                r#"
            var m = window.matchMedia("screen");
            m.addEventListener("change", function () {});
            m.addListener(function () {});
            m.removeListener(function () {});
            m.removeEventListener("change", function () {});
            m.matches === true;
            "#,
                &host
            )
            .as_boolean()
            .unwrap_or(false)
        );
        // Only the trivial whole-media queries match.
        assert!(
            eval_snippet(
                r#"
            window.matchMedia("").matches &&
            window.matchMedia("all").matches &&
            window.matchMedia("  screen").matches &&
            window.matchMedia("only screen").matches &&
            !window.matchMedia("print").matches &&
            !window.matchMedia("(min-width: 100px)").matches &&
            window.matchMedia(undefined).matches;
            "#,
                &host
            )
            .as_boolean()
            .unwrap_or(false)
        );
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
    fn inner_text_mirrors_text_content() {
        // HTMLElement.innerText (infosec.exchange: the link-card text prop in
        // collection_preview_card is `text: el.innerText`; a missing member
        // propagated undefined into `text.startsWith("#")` and aborted the
        // whole first render pass — Boa reads a property of undefined as
        // "cannot convert 'null' or 'undefined' to object"). No layout engine
        // exists, so the spec's rendered-line formatting collapses to the
        // textContent value.
        dom_true(
            r##"
            var d = document.createElement("div");
            d.innerHTML = "a<b>bc</b>";
            var read = d.innerText === "abc";
            d.innerText = "z &amp;y";
            var written = d.textContent.indexOf("z") === 0 && d.children.length === 0;
            var span = document.querySelector("span");
            var viaSpan = span ? span.innerText.length > 0 : true;
            read && written && viaSpan;
            "##,
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
            .map(|line| (line.level.as_str().to_string(), line.text.clone()))
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

    /// The whole `boa_runtime` WinterTC surface plus every boa_engine
    /// language feature is live in a document realm (task: maximum JS).
    #[test]
    fn boa_runtime_web_api_and_all_engine_features_are_present() {
        let host = sample_host();
        for expr in [
            // url feature: a real WHATWG URL.
            "typeof URL === 'function' && new URL('/x?q=1','https://e.com/').href === 'https://e.com/x?q=1'",
            // text + base64 + clone (WinterTC).
            "typeof TextEncoder === 'function' && typeof TextDecoder === 'function'",
            "atob('aGVsbG8=') === 'hello' && btoa('hello') === 'aGVsbG8='",
            "structuredClone({a:[1]}).a[0] === 1",
            // fetch feature: abort primitives + the fetch function itself.
            "typeof AbortController === 'function' && typeof AbortSignal === 'function'",
            "typeof fetch === 'function' && typeof queueMicrotask === 'function'",
            // engine features: bundled-ICU Intl, Temporal, annex-b legacy.
            "typeof Intl.NumberFormat === 'function' && typeof Temporal === 'object'",
            "typeof ''.substr === 'function' && typeof ''.trimLeft === 'function'",
        ] {
            assert!(
                eval_snippet(expr, &host).as_boolean() == Some(true),
                "runtime surface failed: {expr}"
            );
        }
    }

    #[test]
    fn intl_formats_with_the_bundled_icu_data() {
        let host = sample_host();
        // No system ICU data is installed anywhere here — intl_bundled means
        // the locale data ships in the binary.
        assert!(
            eval_snippet("new Intl.NumberFormat('de-DE').format(1234.5) === '1.234,5'", &host)
                .as_boolean()
                == Some(true)
        );
        assert!(
            eval_snippet("new Intl.Collator().compare('a', 'b') === -1", &host)
                .as_boolean()
                == Some(true)
        );
    }

    #[test]
    fn promise_jobs_drain_at_the_end_of_the_script_turn() {
        let host = sample_host();
        let mut runtime = JsRuntime::new(Rc::clone(&host));
        runtime
            .eval("var d = false; Promise.resolve().then(function () { d = true; }); queueMicrotask(function () { d = d && 'both'; });")
            .expect("queue");
        // boa 0.22 does not drain jobs inside eval — the runtime's turn-end
        // drain must have run both callbacks.
        assert_eq!(runtime.eval("d").expect("d").as_string().map(|s| s.to_std_string_escaped()), Some("both".to_string()));
    }

    #[test]
    fn process_is_present_but_leaks_no_environment() {
        let host = sample_host();
        assert!(
            eval_snippet(
                "typeof process === 'object' && Object.keys(process.env).length === 0 && typeof process.cwd === 'function'",
                &host
            )
            .as_boolean()
                == Some(true)
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
            kind: ScriptKind::Classic,
            },
            ScriptSource {
                code: "document.title = 'still-running';".to_string(),
                origin: "second.js".to_string(),
            kind: ScriptKind::Classic,
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

    // -- userscripts (task: userscript execution with per-script error attribution)

    /// A userscript runs in the page's own realm: it can assign
    /// `document.title` exactly like a page script (the assignment lands in the
    /// host cell the tab turns into `FromTab::TitleChanged`), and its
    /// `console.*` output is tagged with the **script name** — while the script
    /// is the only thing that was ever "the current userscript" for that long.
    #[test]
    fn userscript_runs_in_the_page_realm_and_tags_its_output() {
        let host = sample_host();
        let mut runtime = JsRuntime::new(Rc::clone(&host));
        assert_eq!(host.borrow().userscript, None, "no script runs yet");
        let err = runtime.run_userscript(
            "Title fixer",
            "document.title = 'fixed by userscript'; console.log('patched');",
            None,
        );
        assert!(err.is_none(), "a good userscript must not report an error");
        assert_eq!(
            host.borrow().title_assigned.as_deref(),
            Some("fixed by userscript"),
            "a userscript mutates the page like any script"
        );
        let lines = host.borrow().console.clone();
        assert_eq!(lines.len(), 1, "one console line was captured");
        assert_eq!(lines[0].level, ConsoleLevel::Log);
        assert_eq!(
            lines[0].userscript.as_deref(),
            Some("Title fixer"),
            "the line knows which userscript made it"
        );
        assert_eq!(
            lines[0].display_message(),
            "Title fixer: patched",
            "the wire message leads with the script name"
        );
        assert_eq!(
            runtime.eval("1 + 1").expect("eval").as_i32(),
            Some(2),
            "the shared realm is intact afterwards"
        );
    }

    /// Identity is scoped to one script: page scripts (and every later JS turn,
    /// timer callbacks included) keep their unattributed lines, and a name never
    /// leaks out of `run_userscript`.
    #[test]
    fn page_script_console_stays_unattributed() {
        let host = sample_host();
        let mut runtime = JsRuntime::new(Rc::clone(&host));
        let scripts = [ScriptSource {
            code: "console.log('from the page');".to_string(),
            origin: "inline".to_string(),
        kind: ScriptKind::Classic,
        }];
        assert!(runtime.run_scripts(&scripts, None).is_empty());
        assert!(runtime
            .run_userscript("Namer", "console.log('from the script');", None)
            .is_none());
        runtime.eval("console.log('after the script')").expect("eval");
        let lines = host.borrow().console.clone();
        assert_eq!(lines.len(), 3, "{lines:?}");
        assert_eq!(lines[0].userscript, None, "page script line stays plain");
        assert_eq!(lines[0].display_message(), "from the page");
        assert_eq!(lines[1].userscript.as_deref(), Some("Namer"));
        assert_eq!(lines[2].userscript, None, "the name never leaked");
        assert_eq!(host.borrow().userscript, None);
    }

    /// A userscript that throws (or cannot even compile) is reported **by
    /// name** with `source: "userscript"`, and only that script dies: the next
    /// one still runs and the realm stays usable. This is the message shape the
    /// shell shows as `[userscript] Boom script: Error: kaput`.
    #[test]
    fn throwing_userscript_is_named_and_never_fatal() {
        let host = sample_host();
        let mut runtime = JsRuntime::new(Rc::clone(&host));
        let err = runtime
            .run_userscript("Boom script", "throw new Error('kaput');", None)
            .expect("the throw is reported");
        assert_eq!(err.origin, "Boom script");
        assert_eq!(err.source, "userscript");
        assert!(
            err.message.starts_with("Boom script: "),
            "the script name must lead the message, got {:?}",
            err.message
        );
        assert!(err.message.contains("kaput"), "{:?}", err.message);

        // A parse error is the same story (nothing ran, still attributed).
        let syntax = runtime
            .run_userscript("Broken script", "function broken( {", None)
            .expect("a compile error is reported");
        assert_eq!(syntax.source, "userscript");
        assert!(syntax.message.starts_with("Broken script: ")); 

        assert_eq!(host.borrow().userscript, None, "identity cleared on the error path");
        // The tab's world is intact: the next script of the same document runs.
        assert!(runtime
            .run_userscript("Next", "document.title = 'after boom';", None)
            .is_none());
        assert_eq!(host.borrow().title_assigned.as_deref(), Some("after boom"));
        assert_eq!(runtime.eval("2 * 21").expect("eval").as_i32(), Some(42));
    }

    #[test]
    fn syntax_error_is_a_reported_script_error() {
        let host = sample_host();
        let scripts = [ScriptSource {
            code: "function broken( {".to_string(),
            origin: "inline".to_string(),
            kind: ScriptKind::Classic,
        }];
        let mut runtime = JsRuntime::new(Rc::clone(&host));
        let errors = runtime.run_scripts(&scripts, None);
        assert_eq!(errors.len(), 1, "syntax errors never abort the tab");
        assert!(!errors[0].message.is_empty());
    }

    /// The DevTools console classification (task: JS/CSS/DOM sources): an
    /// uncaught throw out of a native DOM binding reports `source: "dom"`,
    /// while the page's own throws — even a hand-thrown `SyntaxError` with a
    /// DOM throw in the history — stay `source: "js"`.
    #[test]
    fn script_errors_carry_their_console_source() {
        let (mut runtime, mut doc) = dom_world();
        let errors = runtime.run_scripts(
            &[ScriptSource {
                code: "document.querySelector('[');".to_string(),
                origin: "inline".to_string(),
            kind: ScriptKind::Classic,
            }],
            Some(&mut doc),
        );
        assert_eq!(errors.len(), 1, "the invalid-selector throw escapes once");
        assert_eq!(
            errors[0].source, "dom",
            "a native DOM throw is a dom console entry ({errors:?})"
        );
        assert!(
            errors[0].message.contains("SyntaxError"),
            "the message keeps the DOM-prefixed text: {:?}",
            errors[0].message
        );

        // The page's own `SyntaxError` is NOT a native throw — even with the
        // recorded DOM error above, the message-based match keeps it on the
        // `js` side.
        let errors = runtime.run_scripts(
            &[ScriptSource {
                code: "throw new SyntaxError('mine-not-the-native');".to_string(),
                origin: "inline".to_string(),
            kind: ScriptKind::Classic,
            }],
            Some(&mut doc),
        );
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].source, "js", "hand-thrown errors are js ({errors:?})");

        // A plain engine error (TypeError-ish) is js too.
        let errors = runtime.run_scripts(
            &[ScriptSource {
                code: "null.x;".to_string(),
                origin: "inline".to_string(),
            kind: ScriptKind::Classic,
            }],
            Some(&mut doc),
        );
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].source, "js");
    }

    #[test]
    fn contexts_are_isolated_between_navigations() {
        let first = sample_host();
        let mut first_runtime = JsRuntime::new(Rc::clone(&first));
        first_runtime.run_scripts(
            &[ScriptSource {
                code: "var leaked = 1; document.title = 'one';".to_string(),
                origin: "inline".to_string(),
            kind: ScriptKind::Classic,
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
                "file:///srv/site/index.html",
                "file:///srv/site/gone.js",
            ],
            "head first, then body, in tree order; the inline module keeps its place and is collected too"
        );
        assert_eq!(scripts[0].1, "document.title = \"head\";");
        assert_eq!(scripts[1].1, "JS a.js", "src= bodies come from the fetcher");
        // The only skipped script is the JSON blob; the module is collected
        // (task: `<script type="module">` loading), not dropped.
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].contains("unsupported type"));
    }

    /// The extraction half of `<script type="module">` (task: module loading):
    /// the module's body is collected with `kind = Module`, its origin is the
    /// **absolute URL it runs as** (the document URL when inline) — the base
    /// its imports resolve against and its `import.meta.url`.
    #[test]
    fn module_scripts_collect_with_module_kind_and_url_origin() {
        let doc = dom(SCRIPT_PAGE);
        let base = Url::parse("file:///srv/site/index.html").expect("base");
        let extraction = js_extract(&doc, &base, &mut fake_answer);
        let module = extraction
            .scripts
            .iter()
            .find(|script| script.kind == ScriptKind::Module)
            .expect("the inline module is collected, not skipped");
        assert_eq!(module.origin, "file:///srv/site/index.html");
        assert!(module.code.contains("import { x } from \"./c.js\""));
        assert_eq!(
            extraction
                .scripts
                .iter()
                .filter(|script| script.kind == ScriptKind::Classic)
                .count(),
            3,
            "classic scripts stay classic (the fake fetcher drops gone.js, as always)"
        );
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
        // The broken src is skipped but its siblings still execute — and the
        // inline module (task: module loading) is collected like any script.
        assert_eq!(extraction.scripts.len(), 4);
        assert!(extraction
            .scripts
            .iter()
            .any(|script| script.kind == ScriptKind::Module));
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

    // -----------------------------------------------------------------
    // Batch extraction (task D): ONE fetch_many call, document order kept

    /// The faked fetcher both extraction variants are driven with: every URL
    /// whose path mentions "gone" fails, the rest answer with a tagged body.
    fn fake_answer(url: &Url) -> Result<String, String> {
        if url.path().contains("gone") {
            Err("nope: not found".to_string())
        } else {
            Ok(format!("JS {}", url.path().rsplit('/').next().unwrap_or("?")))
        }
    }

    #[test]
    fn batch_extraction_calls_fetch_many_once_with_all_urls_in_document_order() {
        let doc = dom(SCRIPT_PAGE);
        let base = Url::parse("file:///srv/site/index.html").expect("base");
        let mut calls: Vec<Vec<String>> = Vec::new();
        let extraction = extract_scripts_batch(&doc, &base, &mut |urls| {
            calls.push(urls.iter().map(|u| u.as_str().to_string()).collect());
            urls.iter().map(|u| fake_answer(u)).collect()
        });
        assert_eq!(calls.len(), 1, "fetch_many is called exactly once");
        assert_eq!(
            calls[0],
            vec![
                "file:///srv/site/a.js",
                "file:///srv/site/nested/b.js",
                "file:///srv/site/gone.js",
            ],
            "every fetchable src= URL in document order; inline bodies are not URLs"
        );
        assert_eq!(
            extraction
                .scripts
                .iter()
                .map(|s| s.origin.as_str())
                .collect::<Vec<_>>(),
            vec![
                "inline",
                "file:///srv/site/a.js",
                "file:///srv/site/nested/b.js",
                "file:///srv/site/index.html",
            ],
            "assembled in document order (inline module included, its origin is the document URL); the failed fetch yields no script"
        );
        assert_eq!(extraction.scripts[1].code, "JS a.js");
    }

    #[test]
    fn batch_extraction_notes_keep_document_order_around_failures() {
        let doc = dom(SCRIPT_PAGE);
        let base = Url::parse("file:///srv/site/index.html").expect("base");
        let extraction = extract_scripts_batch(&doc, &base, &mut |urls| {
            urls.iter().map(|u| fake_answer(u)).collect()
        });
        // Walk order decides note order, wherever the fetches settled: the
        // JSON note (before b.js), then the gone.js failure — identical to
        // what the sequential fetcher produced. The module in between is not
        // a note any more (task: module loading): it is collected and runs.
        assert_eq!(extraction.notes.len(), 2, "{:?}", extraction.notes);
        assert!(extraction.notes[0].contains("unsupported type"));
        assert!(extraction.notes[1].contains("gone.js"));
        assert!(extraction.notes[1].contains("nope: not found"));
    }

    #[test]
    fn batch_extraction_matches_sequential_extraction_exactly() {
        let doc = dom(SCRIPT_PAGE);
        let base = Url::parse("file:///srv/site/index.html").expect("base");
        let sequential = js_extract(&doc, &base, &mut fake_answer);
        let batch =
            extract_scripts_batch(&doc, &base, &mut |urls| urls.iter().map(|u| fake_answer(u)).collect());
        assert_eq!(
            sequential, batch,
            "both variants share the walk and the assembly — identical output"
        );
    }

    // -----------------------------------------------------------------
    // `<script type="importmap">` (task: importmap support)

    const IMPORTMAP_PAGE: &str = r#"<!doctype html>
<html><head>
<script type="importmap">
{ "imports": { "react": "https://cdn.example/react.js", "lib/": "/vendor/lib/" },
  "scopes": { "/admin/": { "helper": "/special/helper.js" } } }
</script>
<script>window.x = 1;</script>
</head><body><p>map</p></body></html>"#;

    #[test]
    fn importmap_script_is_parsed_not_executed() {
        let doc = dom(IMPORTMAP_PAGE);
        let base = Url::parse("https://shop.example/page.html").expect("base");
        let extraction = js_extract(&doc, &base, &mut |_| Err("unused".to_string()));
        // Only the classic script is executable; the map never reaches the
        // engine and contributes no "unsupported type" note.
        assert_eq!(extraction.scripts.len(), 1);
        assert!(
            !extraction.notes.iter().any(|n| n.contains("unsupported type")),
            "an importmap is a known type: {:?}",
            extraction.notes
        );
        let map = extraction.import_map.expect("the map parsed");
        assert_eq!(map.len(), 2);
        assert_eq!(map.scopes_len(), 1);
        // Exact key, prefix key (against the document base), and a scope.
        assert_eq!(
            map.resolve("react", &base).map(|u| u.to_string()),
            Some("https://cdn.example/react.js".to_string())
        );
        assert_eq!(
            map.resolve("lib/util.js", &base).map(|u| u.to_string()),
            Some("https://shop.example/vendor/lib/util.js".to_string())
        );
        assert_eq!(
            map.resolve("helper", &base).map(|u| u.to_string()),
            None,
            "the scope entry does not leak into plain resolution"
        );
        let admin = Url::parse("https://shop.example/admin/app.html").expect("referrer");
        assert_eq!(
            map.resolve_for("helper", &base, Some(&admin)).map(|u| u.to_string()),
            Some("https://shop.example/special/helper.js".to_string()),
            "from inside the scope prefix the scoped entry wins"
        );
    }

    #[test]
    fn invalid_importmap_json_is_dropped_with_a_note() {
        let doc = dom(
            "<!doctype html><html><head><script type=importmap>{ oops }</script></head><body></body></html>",
        );
        let base = Url::parse("file:///i/index.html").expect("base");
        let extraction = js_extract(&doc, &base, &mut |_| Err("unused".to_string()));
        assert_eq!(extraction.scripts.len(), 0);
        assert_eq!(extraction.import_map, None);
        assert!(
            extraction.notes.iter().any(|n| n.contains("importmap ignored")),
            "{:?}",
            extraction.notes
        );
    }

    #[test]
    fn second_importmap_is_ignored_first_valid_wins() {
        let doc = dom(
            "<!doctype html><html><head>\
             <script type=importmap>{\"imports\":{\"a\":\"/one.js\"}}</script>\
             <script type=importmap>{\"imports\":{\"a\":\"/two.js\"}}</script>\
             </head><body></body></html>",
        );
        let base = Url::parse("file:///i/index.html").expect("base");
        let extraction = js_extract(&doc, &base, &mut |_| Err("unused".to_string()));
        let map = extraction.import_map.expect("first map kept");
        assert_eq!(
            map.resolve("a", &base).map(|u| u.path().to_string()),
            Some("/one.js".to_string())
        );
        assert!(
            extraction.notes.iter().any(|n| n.contains("already has one")),
            "{:?}",
            extraction.notes
        );
    }

    #[test]
    fn importmap_with_src_is_never_fetched() {
        let doc = dom(
            "<!doctype html><html><head><script type=importmap src=/map.json></script></head><body></body></html>",
        );
        let base = Url::parse("file:///i/index.html").expect("base");
        let mut fetched = 0usize;
        let extraction = extract_scripts_batch(&doc, &base, &mut |urls| {
            fetched += urls.len();
            urls.iter().map(|_| Ok(String::new())).collect()
        });
        assert_eq!(fetched, 0, "importmap src= is not a fetchable script");
        assert_eq!(extraction.import_map, None);
        assert!(
            extraction.notes.iter().any(|n| n.contains("must be inline")),
            "{:?}",
            extraction.notes
        );
    }

    #[test]
    fn importmap_resolution_follows_the_spec_rules() {
        let base = Url::parse("https://app.example/deep/index.html").expect("base");
        let map = ImportMap::parse(
            r#"{ "imports": {
                 "exact": "https://cdn.example/e.js",
                 "pkg/": "https://cdn.example/pkg/",
                 "pkg/special.js": "/local/special.js",
                 "pkg/hidden/": null,
                 "geva": "/geva.js"
               },
               "unknown-member": 42 }"#,
        )
        .expect("valid map");
        assert_eq!(
            map.resolve("exact", &base).map(|u| u.to_string()),
            Some("https://cdn.example/e.js".to_string())
        );
        // Longest prefix wins over the shallower one.
        assert_eq!(
            map.resolve("pkg/special.js", &base).map(|u| u.path().to_string()),
            Some("/local/special.js".to_string())
        );
        assert_eq!(
            map.resolve("pkg/other.js", &base).map(|u| u.to_string()),
            Some("https://cdn.example/pkg/other.js".to_string())
        );
        // A null prefix unmaps: resolution fails instead of falling through.
        assert_eq!(map.resolve("pkg/hidden/x.js", &base), None);
        // Bare specifier no entry covers: unresolvable.
        assert_eq!(map.resolve("nope", &base), None);
        // Relative specifiers pass through untouched by the map.
        assert_eq!(
            map.resolve("./sib.js", &base).map(|u| u.to_string()),
            Some("https://app.example/deep/sib.js".to_string())
        );
        // Unknown top-level members are ignored, never fatal.
        assert_eq!(map.len(), 5);
    }

    #[test]
    fn importmap_parser_is_forgiving_per_entry_but_not_per_map() {
        // Per-entry: numbers/objects inside imports are dropped silently.
        let map = ImportMap::parse(
            r#"{ "imports": { "ok": "/ok.js", "bad": 7, "nested": {"a":"b"}, "gone": null } }"#,
        )
        .expect("entries are individually droppable");
        assert_eq!(map.len(), 2, "ok + gone(null)");
        // Per-map: non-object imports/scopes invalidates the WHOLE map.
        assert!(ImportMap::parse("[]").is_err());
        assert!(ImportMap::parse("not json").is_err());
        assert!(ImportMap::parse("{\"imports\": \"react\"}").is_err());
        assert!(ImportMap::parse("{\"scopes\": []}").is_err());
    }

    #[test]
    fn batch_extraction_skips_fetch_many_for_inline_only_pages() {
        let doc = dom("<!doctype html><html><body><script>var x = 1;</script>  <script>   </script></body></html>");
        let base = Url::parse("file:///srv/site/index.html").expect("base");
        let extraction =
            extract_scripts_batch(&doc, &base, &mut |_urls| -> Vec<Result<String, String>> {
                panic!("an inline-only page must not reach the fetcher")
            });
        assert_eq!(extraction.scripts.len(), 1, "the whitespace-only body is skipped");
        assert_eq!(extraction.scripts[0].origin, "inline");
    }

    #[test]
    fn batch_extraction_treats_missing_results_as_failures() {
        let doc = dom(SCRIPT_PAGE);
        let base = Url::parse("file:///srv/site/index.html").expect("base");
        let extraction = extract_scripts_batch(&doc, &base, &mut |urls| {
            assert_eq!(urls.len(), 3, "the fake is asked for all three srcs at once");
            // A misbehaving fetcher answers only the first URL…
            vec![Ok("JS a.js".to_string())]
        });
        assert_eq!(extraction.scripts.len(), 3, "inline + a.js + inline module");
        // …the unanswered ones degrade to notes, exactly like fetch errors.
        assert!(extraction
            .notes
            .iter()
            .any(|note| note.contains("nested/b.js") && note.contains("returned no result")));
        assert!(extraction
            .notes
            .iter()
            .any(|note| note.contains("gone.js") && note.contains("returned no result")));
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

    /// `element.dataset` (task: startpage.com search bar): a live view over
    /// `data-*` with the spec's camel-case mapping; writes/deletes go through
    /// the attribute natives like `setAttribute`/`removeAttribute`.
    #[test]
    fn dom_dataset_views_data_attributes() {
        dom_true(
            r#"
            var el = document.querySelector("span[data-x]");
            var read = el.dataset.x === "v";
            var missing = el.dataset.siteTheme === undefined && !("siteTheme" in el.dataset);
            el.dataset.siteTheme = "device";
            var written = el.getAttribute("data-site-theme") === "device" && el.dataset.siteTheme === "device";
            el.dataset.col2 = "9";
            var digits = el.getAttribute("data-col2") === "9";
            var keys = []; for (var k in el.dataset) keys.push(k);
            var enumerated = keys.indexOf("x") >= 0 && keys.indexOf("siteTheme") >= 0 && keys.indexOf("col2") >= 0;
            delete el.dataset.x;
            var deleted = el.getAttribute("data-x") === null && el.dataset.x === undefined;
            read && missing && written && digits && enumerated && deleted;
            "#,
        );
    }

    /// The whole startpage.com theme-reveal script (the calls that used to
    /// die at :2:51): matchMedia + dataset + `innerHTML +=` on the mitigation
    /// <style> all survive, and the reveal adds its `opacity: 1` rule while
    /// keeping the original hidden one in the stylesheet text.
    #[test]
    fn dom_startpage_reveal_script_runs_end_to_end() {
        dom_true(
            r##"
            var st = document.createElement("style");
            st.setAttribute("class", "theme-flash-mitigation");
            st.textContent = "#root { opacity: 0; }";
            document.body.appendChild(st);
            var meta = document.createElement("meta");
            meta.setAttribute("id", "site_theme");
            meta.setAttribute("data-site-theme", "device");
            document.body.appendChild(meta);

            var osPrefersDarkTheme = window.matchMedia("(prefers-color-scheme: dark)");
            var themeFlashStyleTag = document.querySelector(".theme-flash-mitigation");
            var siteTheme = document.querySelector("#site_theme").dataset.siteTheme;
            var isDarkOsThemeOnly = osPrefersDarkTheme.matches && siteTheme === "device";
            var isSiteThemeDark = ["dark", "night"].includes(siteTheme);
            if (!isSiteThemeDark && !isDarkOsThemeOnly) {
                themeFlashStyleTag.innerHTML += " #root { opacity: 1; }";
            }
            siteTheme === "device" &&
                themeFlashStyleTag.innerHTML.indexOf("opacity: 1") >= 0 &&
                themeFlashStyleTag.innerHTML.indexOf("opacity: 0") >= 0;
            "##,
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
    fn dom_template_content_is_an_inert_fragment() {
        // wetsuit (mstdn.social's status composer) sanitizes through
        // template.content; mstdn.social live proved the whole composer
        // tree dies without it.
        dom_true(
            r#"
            var t = document.createElement("template");
            t.innerHTML = '<p class="a">hi &amp; <b>b</b></p><img src="https://example.com/x.png" width="50">';
            var frag = t.content;
            var moved = frag.childNodes.length === 2 && frag.firstChild.tagName === "P"
              && frag.firstChild.textContent === "hi & b"
              && t.childNodes.length === 0 && frag.nodeType === 11;
            var seen = 0; for (var n of frag.childNodes) seen += n.nodeType;
            var inert = document.querySelector("p.a") === null;
            var again = t.innerHTML === '<p class="a">hi &amp; <b>b</b></p><img src="https://example.com/x.png" width="50">';
            moved && inert && seen === 2 && again && t.content === frag;
            "#,
        );
    }

    #[test]
    fn dom_attribute_names_iterable_and_ns() {
        // unhead's render loop (getAttributeNames) and react-dom's SVG href
        // patching (setAttributeNS) — both proved live on mstdn.social.
        let (mut runtime, mut doc) = dom_world();
        let value = dom_eval(&mut runtime, &mut doc, r##"
            var d = document.createElement("div");
            d.setAttribute("id", "gn");
            d.setAttribute("class", "k");
            var pairs = []; for (var a of d.attributes) pairs.push(a.name + "=" + a.value);
            var names = d.getAttributeNames();
            var okNames = names.length === 2 && names.indexOf("id") >= 0 && names.indexOf("class") >= 0;
            var okPairs = pairs.length === 2 && pairs.indexOf("id=gn") >= 0 && pairs.indexOf("class=k") >= 0;
            d.setAttributeNS("http://www.w3.org/1999/xlink", "xlink:href", "#icon");
            var okNs = d.getAttributeNS(null, "xlink:href") === "#icon" && d.getAttribute("xlink:href") === "#icon";
            d.removeAttributeNS(null, "xlink:href");
            var empty = document.createElement("span");
            okNames && okPairs && okNs && d.getAttribute("xlink:href") === null && d.getAttributeNames().length === 2 && empty.getAttributeNames().length === 0;
            "##,
        );
        assert!(value.to_boolean(), "getAttributeNames/*NS*/ + NamedNodeMap iteration");
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

    /// DOM world at a real `http` URL (reflected URL attributes resolve their
    /// relatives against the document base) with `page` as its markup, plus
    /// the host cell so a test can see whether a JS write queued a navigation.
    fn image_world(page: &str) -> (HostRef, JsRuntime, blitz_html::HtmlDocument) {
        let host = Rc::new(RefCell::new(HostState::new(
            "https://example.test/page.html",
            Some("Bridge"),
            Some(" body text ".to_string()),
        )));
        let runtime = JsRuntime::new(Rc::clone(&host));
        let mut config = blitz_dom::DocumentConfig::default();
        config.base_url = Some("https://example.test/page.html".to_string());
        let doc = blitz_html::HtmlDocument::from_html(page, config);
        (host, runtime, doc)
    }

    /// `window.Image` (task: `ReferenceError: Image is not defined`): the
    /// factory really creates an `<img>` **in the live document** — detached,
    /// `isConnected` false, with its size arguments written as content
    /// attributes — and the interface name exists so `instanceof` holds. In
    /// browsers `Image.prototype === HTMLImageElement.prototype`, so the two
    /// names share the wrapper prototype here too.
    #[test]
    fn image_constructor_creates_a_detached_img_element() {
        let (_host, mut runtime, mut doc) = image_world(DOM_PAGE);
        let source = r##"
            var made = new Image();
            var sized = new Image(120, 45.9);
            var junk = new Image("big", -7);
            var bare = Image();
            made !== null && made.nodeName === "IMG" && made.nodeType === 1 &&
            made.isConnected === false && made.parentNode === null &&
            made.getAttribute("width") === null &&
            made.getAttribute("height") === null &&
            sized.getAttribute("width") === "120" &&
            sized.getAttribute("height") === "45" &&
            sized.width === 120 && sized.height === 45 &&
            junk.getAttribute("width") === "0" && junk.getAttribute("height") === "0" &&
            bare !== null && bare !== made &&
            typeof Image === "function" && typeof HTMLImageElement === "function" &&
            made instanceof HTMLImageElement &&
            Image.prototype === HTMLImageElement.prototype &&
            // An image with nothing to wait for is complete, with no size.
            made.complete === true && made.naturalWidth === 0 && made.naturalHeight === 0;
            "##;
        assert!(
            dom_eval(&mut runtime, &mut doc, source).to_boolean(),
            "Image() must build a real detached <img>"
        );
    }

    /// Reflected URL attributes — `img.src = "…"` is THE way a page sets an
    /// image: the setter writes the content attribute like `setAttribute`, the
    /// getter answers the url resolved against the **document** url, and a tag
    /// without the IDL member keeps the browser's own-property semantics
    /// (`div.href = "x"` stores a string) instead of swallowing the write.
    /// None of it navigates.
    #[test]
    fn reflected_url_attributes_answer_absolute_urls() {
        let (host, mut runtime, mut doc) = image_world(DOM_PAGE);
        let source = r##"
            var img = document.createElement("img");
            img.src === "" &&
            (img.src = "/img/logo.png",
             img.getAttribute("src") === "/img/logo.png" &&
             img.src === "https://example.test/img/logo.png" &&
             img.currentSrc === "https://example.test/img/logo.png" &&
             // Bytes have not arrived (nothing was fetched): still loading.
             img.complete === false && img.naturalWidth === 0) &&
            (img.src = "https://cdn.example/a.png", img.src === "https://cdn.example/a.png") &&
            (img.src = "   ", img.src === "" && img.complete === true) &&
            (function () {
                var a = document.createElement("a");
                document.body.appendChild(a);
                a.href = "next.html";
                return a.getAttribute("href") === "next.html" &&
                    a.href === "https://example.test/next.html";
            })() &&
            (function () {
                var div = document.createElement("div");
                div.href = "stored";
                return div.href === "stored" && div.getAttribute("href") === null &&
                    div.src === undefined && div.width === undefined &&
                    div.complete === undefined && div.decode() === undefined;
            })();
            "##;
        assert!(
            dom_eval(&mut runtime, &mut doc, source).to_boolean(),
            "src/href must reflect as URLs"
        );
        assert!(
            host.borrow().pending_nav.is_none(),
            "reflecting src/href must never queue a navigation"
        );
    }

    /// The image members read what the engine knows about the bytes: no data
    /// means `complete` false and no intrinsic size, and once blitz has the
    /// image on the element the members report its real dimensions.
    #[test]
    fn image_sizing_members_read_the_engine_data() {
        let page = r####"<!doctype html>
<html><head><title>Pic</title></head>
<body><img id="pic" src="/pic.png"></body></html>"####;
        let (_host, mut runtime, mut doc) = image_world(page);
        let pending = r##"
            var i = document.getElementById("pic");
            i !== null && i.getAttribute("src") === "/pic.png" &&
            i.src === "https://example.test/pic.png" &&
            i.complete === false && i.naturalWidth === 0 && i.width === 0;
            "##;
        assert!(
            dom_eval(&mut runtime, &mut doc, pending).to_boolean(),
            "an <img> without bytes reads as still loading"
        );
        // Hand the element the data blitz attaches when a fetch settles.
        let id = doc.get_element_by_id("pic").expect("img element");
        let element = doc
            .get_node_mut(id)
            .and_then(|node| node.element_data_mut())
            .expect("<img> element data");
        element.special_data = blitz_dom::node::SpecialElementData::Image(Box::new(
            blitz_dom::node::ImageData::Raster(blitz_dom::node::RasterImageData::new(
                7,
                3,
                Arc::new(vec![0_u8; 7 * 3 * 4]),
            )),
        ));
        let loaded = r##"
            var i = document.getElementById("pic");
            i.complete === true &&
            i.naturalWidth === 7 && i.naturalHeight === 3 &&
            i.width === 7 && i.height === 3 &&
            (i.width = 14,
             i.width === 14 && i.getAttribute("width") === "14") &&
            typeof i.decode().then === "function";
            "##;
        assert!(
            dom_eval(&mut runtime, &mut doc, loaded).to_boolean(),
            "loaded image data must read through the members"
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
            kind: ScriptKind::Classic,
            },
            ScriptSource {
                code: r#"document.title = document.getElementById("built").textContent.toUpperCase();
                         window.sawDom = true;"#
                    .to_string(),
                origin: "inline".to_string(),
            kind: ScriptKind::Classic,
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

    #[test]
    fn document_write_lands_in_the_live_dom() {
        // document.write is a cursor into the parse stream; scripts run after
        // the parse, so the written markup is parsed and appended to the live
        // body — visible to every later DOM read, not swallowed.
        dom_true(
            r##"
            var before = document.body.childNodes.length;
            document.write("<p id=\"w1\">written</p>");
            document.write("", "<b id=\"w2\">and</b>"); // multi-arg concatenation
            document.writeln("<i id=\"w3\">line</i>");
            var p = document.getElementById("w1");
            var b = document.getElementById("w2");
            var i = document.getElementById("w3");
            var appended = p !== null && b !== null && i !== null &&
                p.parentNode === document.body &&
                document.body.childNodes.length >= before + 3;
            var textOk = p.textContent === "written" && i.textContent === "line";
            // The written tree is queryable like the parsed one.
            var q = document.querySelector("#w2");
            var html = document.body.innerHTML.indexOf("w1") >= 0;
            appended && textOk && q === b && html;
            "##,
        );
    }

    #[test]
    fn element_style_writes_land_in_the_style_attribute() {
        // Every route to Element.style rewrites the `style` content attribute
        // (the DOM's source of truth for inline style): whole-string
        // assignment, cssText, member writes, setProperty, removeProperty.
        dom_true(
            r##"
            var el = document.getElementById("p1");
            el.style = "color: red; background:#eee";
            var assigned = el.getAttribute("style") === "color: red; background:#eee";
            var member = el.style.color === "red";
            var cssText = el.style.cssText === "color: red; background:#eee";
            // camelCase member writes normalise to the dashed property.
            el.style.backgroundColor = "rgb(0, 128, 0)";
            var replaced = el.style.getPropertyValue("background-color") === "rgb(0, 128, 0)" &&
                el.getAttribute("style").indexOf("background-color: rgb(0, 128, 0)") >= 0;
            var untouched = el.style.color === "red";
            // Writing "" removes the declaration; removeProperty returns it.
            el.style.color = "";
            var cleared = el.style.color === undefined &&
                el.style.getPropertyValue("color") === "";
            var old = el.style.removeProperty("background-color");
            var removed = old === "rgb(0, 128, 0)" &&
                el.getAttribute("style") === "background: #eee;";
            assigned && member && cssText && replaced && untouched && cleared && removed;
            "##,
        );
    }

    #[test]
    fn js_attribute_writes_replace_parsed_ones() {
        // blitz compares the FULL QualName when setting/removing attributes,
        // and html5ever stores HTML attributes in the empty namespace — so a
        // JS write against an attribute the parser already put there must
        // replace it in place, never push a shadow duplicate.
        dom_true(
            r##"
            var p = document.getElementById("p1"); // parsed with class="inner first"
            function classCount() {
                var m = p.attributes, c = 0;
                for (var i = 0; i < m.length; i++) {
                    if (m.item(i).name === "class") c++;
                }
                return c;
            }
            var parsed = classCount() === 1 && p.getAttribute("class") === "inner first";
            p.setAttribute("class", "solo");
            var replaced = classCount() === 1 && p.getAttribute("class") === "solo" &&
                p.className === "solo";
            p.className = "again";
            var mirror = classCount() === 1 && p.getAttribute("class") === "again";
            p.removeAttribute("class");
            var removed = classCount() === 0 && p.getAttribute("class") === null;
            parsed && replaced && mirror && removed;
            "##,
        );
    }

    #[test]
    fn document_dir_and_lang_mirror_the_html_attributes() {
        dom_true(
            r##"
            document.dir = "rtl";
            var d = document.documentElement.getAttribute("dir") === "rtl" &&
                document.dir === "rtl";
            document.lang = "en-GB";
            var l = document.documentElement.getAttribute("lang") === "en-GB" &&
                document.lang === "en-GB";
            d && l;
            "##,
        );
    }

    #[test]
    fn style_assignment_restyles_on_resolve() {
        // The full reflection the tab relies on: a script sets inline style
        // through the JS surface, the runtime's settle() resolves it, and
        // stylo's computed values on the live node change accordingly.
        let host = sample_host();
        let mut runtime = JsRuntime::new(Rc::clone(&host));
        let mut doc = dom(DOM_PAGE);
        let bg = |doc: &blitz_html::HtmlDocument| {
            format!(
                "{:?}",
                doc.find_body_node()
                    .expect("body")
                    .primary_styles()
                    .expect("resolved styles")
                    .get_background()
                    .background_color
            )
        };
        doc.resolve(0.);
        let before = bg(&doc);
        let scripts = [
            ScriptSource {
                code: "document.body.style = 'background: rgb(255, 0, 0)';".to_string(),
                origin: "inline".to_string(),
            kind: ScriptKind::Classic,
            },
            ScriptSource {
                // Userscripts run in the same realm after the page's own
                // scripts — exercise the same mutation through that door.
                code: "document.querySelector('#p1').style.color = 'rgb(0, 0, 255)';".to_string(),
                origin: "inline".to_string(),
            kind: ScriptKind::Classic,
            },
        ];
        let errors = runtime.run_scripts(&scripts, Some(&mut doc));
        assert!(errors.is_empty(), "{errors:?}");
        doc.resolve(0.);
        assert!(
            bg(&doc).contains("ColorComponents(1.0, 0.0, 0.0), alpha: 1.0"),
            "document.body.style = \"…\" must restyle: {before} -> {}",
            bg(&doc)
        );
        let p1_color = format!(
            "{:?}",
            doc.get_node(doc.query_selector("#p1").expect("valid").expect("#p1"))
                .expect("node")
                .primary_styles()
                .expect("resolved styles")
                .clone_color()
        );
        assert!(
            p1_color.contains("ColorComponents(0.0, 0.0, 1.0), alpha: 1.0"),
            "el.style.color = \"…\" must restyle: {p1_color}"
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

    // -----------------------------------------------------------------
    // `<script type="module">` loading (task: module loading)

    /// An origin for module tests: a path → body table served through the
    /// exact `ModuleFetch` shape the tab hands the runtime, recording every
    /// URL fetched so dedup (one instance per specifier) is observable.
    fn fake_site(pages: &[(&str, &str)]) -> (ModuleFetch, Rc<RefCell<Vec<String>>>) {
        let site: Rc<HashMap<String, String>> = Rc::new(
            pages
                .iter()
                .map(|(path, body)| (path.to_string(), body.to_string()))
                .collect(),
        );
        let hits: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
        let fetch: ModuleFetch = {
            let site = Rc::clone(&site);
            let hits = Rc::clone(&hits);
            Rc::new(move |url: &Url| {
                hits.borrow_mut().push(url.as_str().to_string());
                match site.get(url.path()) {
                    Some(body) => Ok(body.clone()),
                    None => Err(format!("404 Not Found: {}", url.path())),
                }
            })
        };
        (fetch, hits)
    }

    /// The pure resolution half of the loader (no realm needed): the import
    /// map's rules decide every specifier — exact keys, prefix keys with the
    /// remainder carried over, `null` unmappings, `scopes` keyed on the
    /// referrer — and without a map (or a hit) a relative-URL specifier
    /// joins the referrer while a **bare** one fails, exactly like a browser.
    #[test]
    fn module_specifier_resolution_matrix() {
        let base = Url::parse("https://m.test/app/main.js").expect("base");
        let (fetch, _hits) = fake_site(&[]);
        let loader = UrlModuleLoader::new(base.clone(), fetch);
        // No map: relative specifiers resolve, bare ones fail outright.
        assert_eq!(
            loader
                .resolve_specifier("./dep.js", &base)
                .expect("relative"),
            Url::parse("https://m.test/app/dep.js").expect("url")
        );
        assert_eq!(
            loader
                .resolve_specifier("/abs/mod.js", &base)
                .expect("root-relative"),
            Url::parse("https://m.test/abs/mod.js").expect("url")
        );
        assert!(loader.resolve_specifier("react", &base).is_err(), "bare, no map");
        // A map answers bare specifiers, prefixes carry the remainder, and
        // `null` unmaps to a resolution failure.
        let map = ImportMap::parse(
            r#"{"imports": {
                "react": "https://esm.sh/react",
                "lib/": "/v/lib/",
                "evil": null
            },
            "scopes": { "/special/": { "cfg": "/special/cfg.js" } }}"#,
        )
        .expect("valid map");
        loader.set_import_map(Some(map));
        assert_eq!(
            loader.resolve_specifier("react", &base).expect("mapped"),
            Url::parse("https://esm.sh/react").expect("url")
        );
        assert_eq!(
            loader
                .resolve_specifier("lib/util.js", &base)
                .expect("prefix"),
            Url::parse("https://m.test/v/lib/util.js").expect("url")
        );
        assert!(loader.resolve_specifier("evil", &base).is_err(), "unmapped null");
        // `scopes` shadow `imports` for referrers inside the scope prefix —
        // and only for them.
        let scoped = Url::parse("https://m.test/special/m.js").expect("scoped url");
        assert_eq!(
            loader
                .resolve_specifier("cfg", &scoped)
                .expect("scoped bare"),
            Url::parse("https://m.test/special/cfg.js").expect("url")
        );
        assert!(
            loader.resolve_specifier("cfg", &base).is_err(),
            "outside the scope the entry does not apply"
        );
    }

    /// The whole pipeline on a real realm: an entry module at
    /// `…/app/main.js` statically imports a relative dep (TWICE — one fetch,
    /// one instance), a bare specifier answered by the document's import
    /// map, and reaches the map through a dependency whose OWN relative
    /// import resolves against the dep's URL, not the document's; the entry
    /// also reads `import.meta.url`. Classic-script semantics next to it are
    /// untouched.
    #[test]
    fn module_graph_fetches_links_evaluates_and_shares_instances() {
        let (fetch, hits) = fake_site(&[
            (
                "/app/dep.js",
                "export const x = 7; globalThis.depEvals = (globalThis.depEvals || 0) + 1;",
            ),
            (
                "/v/lib/other.js",
                "import { s } from '../shared.js'; export const y = s * 2;",
            ),
            ("/v/shared.js", "export const s = 5;"),
        ]);
        let host = sample_host();
        let mut runtime = JsRuntime::new_with_modules(
            Rc::clone(&host),
            &Url::parse("https://modules.test/app/index.html").expect("doc url"),
            fetch,
        );
        runtime
            .set_import_map(Some(
                ImportMap::parse(r#"{"imports": { "lib/": "/v/lib/" }}"#).expect("map"),
            ));
        let entry = ScriptSource {
            code: r#"
                import { x } from "./dep.js";
                import * as ns from "./dep.js";
                import { y } from "lib/other.js";
                globalThis.result = x + y
                    + (ns.x === x ? 100 : 0)
                    + (import.meta.url === "https://modules.test/app/main.js" ? 1000 : 0);
            "#
            .to_string(),
            origin: "https://modules.test/app/main.js".to_string(),
            kind: ScriptKind::Module,
        };
        let errors = runtime.run_scripts(&[entry], None);
        assert!(errors.is_empty(), "{errors:?}");
        assert!(
            runtime.import_map().is_some_and(|map| map.len() == 1),
            "the document keeps its map for the realm's lifetime"
        );
        assert_eq!(
            runtime.module_loader().map(|loader| loader.len()),
            Some(4),
            "entry + three graph modules in the document's module map"
        );
        assert_eq!(
            runtime
                .eval("globalThis.result")
                .expect("result")
                .as_number(),
            Some(1117.0),
            "7 + 10 + shared-instance-100 + import.meta.url-1000"
        );
        assert_eq!(
            runtime
                .eval("globalThis.depEvals")
                .expect("depEvals")
                .as_number(),
            Some(1.0),
            "two importers, one instance: the dep body ran once"
        );
        let mut fetched = hits.borrow().clone();
        fetched.sort();
        assert_eq!(
            fetched,
            vec![
                "https://modules.test/app/dep.js".to_string(),
                "https://modules.test/v/lib/other.js".to_string(),
                "https://modules.test/v/shared.js".to_string(),
            ],
            "each graph module fetched exactly once, bare specifiers through the map, \"../shared.js\" against the DEP's url"
        );
    }

    /// A module that cannot load — bare specifier with no map — reports a
    /// `ScriptError` naming the specifier, and the page KEEPS RUNNING: the
    /// classic script after it still executes, like every other script error
    /// in the contract.
    #[test]
    fn unresolvable_bare_import_reports_and_the_page_survives() {
        let (fetch, _hits) = fake_site(&[]);
        let host = sample_host();
        let mut runtime = JsRuntime::new_with_modules(
            Rc::clone(&host),
            &Url::parse("https://modules.test/page.html").expect("doc url"),
            fetch,
        );
        let scripts = [
            ScriptSource {
                code: "import { render } from 'react'; globalThis.never = render;".to_string(),
                origin: "https://modules.test/page.html".to_string(),
                kind: ScriptKind::Module,
            },
            ScriptSource {
                code: "globalThis.after = 'ran';".to_string(),
                origin: "inline".to_string(),
                kind: ScriptKind::Classic,
            },
        ];
        let errors = runtime.run_scripts(&scripts, None);
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert_eq!(errors[0].origin, "https://modules.test/page.html");
        assert!(
            errors[0].message.contains("react"),
            "the report names the specifier: {:?}",
            errors[0].message
        );
        assert_eq!(
            runtime
                .eval("globalThis.after")
                .expect("after")
                .as_string()
                .map(|s| s.to_std_string_escaped()),
            Some("ran".to_string()),
            "the next script still runs"
        );
    }

    /// A dependency the network refuses (404 from the fetcher) rejects the
    /// importing module with a message naming the failed URL — and nothing
    /// is cached, so the failure is honest, once.
    #[test]
    fn failed_dependency_fetch_rejects_the_importing_module() {
        let (fetch, hits) = fake_site(&[("/present.js", "export const here = true;")]);
        let host = sample_host();
        let mut runtime = JsRuntime::new_with_modules(
            Rc::clone(&host),
            &Url::parse("https://modules.test/page.html").expect("doc url"),
            fetch,
        );
        let scripts = [ScriptSource {
            code: "import './present.js'; import './missing.js';".to_string(),
            origin: "https://modules.test/page.html".to_string(),
            kind: ScriptKind::Module,
        }];
        let errors = runtime.run_scripts(&scripts, None);
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(
            errors[0].message.contains("missing.js"),
            "the report names the failed url: {:?}",
            errors[0].message
        );
        assert!(
            !hits.borrow().iter().any(|url| url.ends_with("page.html")),
            "an inline entry module is never re-fetched — it is the page"
        );
    }

    /// Cycles: A ⇄ B. The ENTRY module is filed with the loader before its
    /// imports load, so B's `import from "./a.js"` gets the very instance
    /// being evaluated — no second fetch, no deadlock — and B (the deepest
    /// dependency) evaluates before A's body, per spec.
    #[test]
    fn module_cycles_resolve_off_the_entry_registration() {
        let (fetch, hits) = fake_site(&[(
            "/b.js",
            "import { fromA } from './a.js'; globalThis.seesA = () => fromA; export const fromB = 2; globalThis.ranB = true;",
        )]);
        let host = sample_host();
        let mut runtime = JsRuntime::new_with_modules(
            Rc::clone(&host),
            &Url::parse("https://modules.test/page.html").expect("doc url"),
            fetch,
        );
        let scripts = [ScriptSource {
            code: "import { fromB } from './b.js'; export const fromA = 1; globalThis.trace = [(globalThis.ranB ? 'b' : 'noB'), 'a', fromB];".to_string(),
            origin: "https://modules.test/a.js".to_string(),
            kind: ScriptKind::Module,
        }];
        let errors = runtime.run_scripts(&scripts, None);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(
            runtime
                .eval("JSON.stringify(globalThis.trace)")
                .expect("trace")
                .as_string()
                .map(|s| s.to_std_string_escaped()),
            Some("[\"b\",\"a\",2]".to_string()),
            "B evaluated first (post-order), A imported its live binding through the cycle"
        );
        assert_eq!(
            hits.borrow().as_slice(),
            vec!["https://modules.test/b.js".to_string()],
            "the cycle fetched B only; A came from the entry registration"
        );
    }

    /// Dynamic `import()` — the runtime-side hook: the specifier resolves
    /// through the import map DURING evaluation (a job the drain runs), the
    /// body is fetched exactly once, and the namespace arrives.
    #[test]
    fn dynamic_import_resolves_through_the_map_at_runtime() {
        let (fetch, hits) = fake_site(&[
            ("/v/late.js", "export const v = 42;"),
            ("/v/once.js", "export const hits = (globalThis.once = (globalThis.once || 0) + 1);"),
        ]);
        let host = sample_host();
        let mut runtime = JsRuntime::new_with_modules(
            Rc::clone(&host),
            &Url::parse("https://modules.test/page.html").expect("doc url"),
            fetch,
        );
        runtime
            .set_import_map(Some(
                ImportMap::parse(r#"{"imports": { "late": "/v/late.js", "once": "/v/once.js" }}"#)
                    .expect("map"),
            ));
        let scripts = [ScriptSource {
            code: r#"
                const ns = await import('late');
                globalThis.dyn = ns.v;
                await import('once');
                await import('once');
                globalThis.onceFetched = (globalThis.once || 0);
            "#
            .to_string(),
            origin: "https://modules.test/page.html".to_string(),
            kind: ScriptKind::Module,
        }];
        let errors = runtime.run_scripts(&scripts, None);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(
            runtime.eval("globalThis.dyn").expect("dyn").as_number(),
            Some(42.0),
            "top-level await + dynamic import through the map"
        );
        assert_eq!(
            runtime
                .eval("globalThis.onceFetched")
                .expect("onceFetched")
                .as_number(),
            Some(1.0),
            "two dynamic imports of one specifier: one fetch, one instance"
        );
        assert_eq!(hits.borrow().len(), 2, "late.js + once.js: {hits:?}");
    }

    /// Inline modules resolve their relative imports against the DOCUMENT
    /// URL (their own URL identity), not the cwd-filesystem loader Boa would
    /// default to — the same page's module graph runs from the network.
    #[test]
    fn inline_module_resolves_against_the_document_url() {
        let (fetch, hits) = fake_site(&[("/app/dep.js", "export const x = 1;")]);
        let host = sample_host();
        let mut runtime = JsRuntime::new_with_modules(
            Rc::clone(&host),
            &Url::parse("https://modules.test/app/index.html").expect("doc url"),
            fetch,
        );
        let scripts = [ScriptSource {
            code: "import { x } from './dep.js'; globalThis.inline = x;".to_string(),
            origin: "https://modules.test/app/index.html".to_string(),
            kind: ScriptKind::Module,
        }];
        let errors = runtime.run_scripts(&scripts, None);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(
            runtime
                .eval("globalThis.inline")
                .expect("inline")
                .as_number(),
            Some(1.0)
        );
        assert_eq!(
            hits.borrow().as_slice(),
            vec!["https://modules.test/app/dep.js".to_string()]
        );
    }

    // ---------------------------------------------------------------------
    // boa 0.22 ENGINE BUG — FIXED in the vendored fork (thirdparty/boa,
    // see thirdparty/boa/PATCH.md; root-caused on infosec.exchange/explore,
    // task #69). Regression tests for the fixed `this` semantics.
    // ---------------------------------------------------------------------

    /// Fresh-default-realm (NO glue) regression test for the forked boa_ast
    /// fix: a class method's `this` must survive **any depth of nested arrow
    /// functions**, in both module and classic-script compilation. Before the
    /// fork fix, upstream boa_ast 0.22 lost `this` (undefined in modules,
    /// globalThis in scripts) through >=2 nested arrows: the scope analyzer's
    /// `escape_this_in_enclosing_function_scope` stopped at the outer arrow's
    /// function scope — which cannot bind `this` — so the method's function
    /// environment (the `this` holder) was never pushed. Live consequence
    /// back then: @react-spring/core's
    /// `observerAdded(e){e==1&&N(this.inputs,e=>N(e,e=>z(e)&&rt(e,this)))}`
    /// registered `undefined` into a spring's FluidValue-observers Set and the
    /// next rAF advance threw `TypeError: cannot convert 'null' or 'undefined'
    /// to object` out of `Set::forEach`. See also
    /// `tests/react_spring_observer_this.rs` (end-to-end fixture).
    #[test]
    fn this_survives_nested_arrows_in_modules_and_scripts() {
        // Same body evaluated two ways: as a module and as a classic script.
        let body = r#"
class C {
  one(){
    [0].forEach(e => {
      globalThis[which + "One"] = typeof this;
    });
  }
  two(){
    [0].forEach(e => {
      [0].forEach(v => {
        globalThis[which + "Two"] = typeof this;
      });
    });
  }
  three(){
    [0].forEach(a => [0].forEach(b => [0].forEach(c => {
      globalThis[which + "Three"] = String(this instanceof C);
    })));
  }
  identity(){
    const o = {};
    [0].forEach(e => [0].forEach(v => {
      globalThis[which + "Identity"] = String(this === o);
    }));
    return o;
  }
}
const c = new C();
c.one();
c.two();
c.three();
c.identity();
globalThis[which + "Self"] = String((function(){ return (() => this === undefined)(); }).call(undefined));
"#;
        let mut context = Context::default();

        // As an ES MODULE (what a `<script type=module>` body becomes).
        context
            .global_object()
            .set(js_string!("which"), JsValue::from(js_string!("m")), false, &mut context)
            .expect("global write");
        let module = Module::parse(
            Source::from_bytes(body.as_bytes()),
            None,
            &mut context,
        )
        .expect("module parses");
        let promise = module.load_link_evaluate(&mut context);
        context.run_jobs().expect("module jobs run");
        assert!(matches!(promise.state(), PromiseState::Fulfilled(_)));

        // As a CLASSIC SCRIPT (identical source text).
        context
            .global_object()
            .set(js_string!("which"), JsValue::from(js_string!("c")), false, &mut context)
            .expect("global write");
        context
            .eval(Source::from_bytes(body.as_bytes()))
            .expect("classic evals");

        let mut probe = |name: &str| -> String {
            context
                .global_object()
                .get(js_string!(name), &mut context)
                .expect("global read")
                .as_string()
                .map(|s| s.to_std_string_escaped())
                .unwrap_or_default()
        };
        assert_eq!(probe("cOne"), "object", "classic script, one arrow level");
        assert_eq!(probe("cTwo"), "object", "classic script, two arrow levels");
        assert_eq!(probe("mOne"), "object", "module, one arrow level");
        assert_eq!(
            probe("mTwo"),
            "object",
            "fork fix: `this` survives two nested arrows in module code"
        );
        assert_eq!(
            probe("mThree"),
            "true",
            "fork fix: `this` keeps receiver identity three arrows deep"
        );
        assert_eq!(
            probe("cThree"),
            "true",
            "classic script: receiver identity three arrows deep"
        );
        assert_eq!(
            probe("mIdentity"),
            "false",
            "`this` is the receiver, never a sibling object"
        );
        assert_eq!(
            probe("mSelf"),
            "true",
            "non-escaping `this` in a plain (strict) function is untouched"
        );
        assert_eq!(
            probe("cSelf"),
            "false",
            "classic/sloppy: .call(undefined) rebinds to globalThis, untouched by the fix"
        );
    }
    /// `location.origin` (task: infosec.exchange emoji-data loader).
    /// Mastodon's chunk does `new URL(path, location.origin)`; with `origin`
    /// absent the base is undefined and WHATWG parsing throws "relative URL
    /// without a base" out of the fetch. The getter must serialise the tuple
    /// origin (keeping explicit ports — origins do NOT fold default ports)
    /// and answer the string "null" for schemes with no serialisable origin.
    #[test]
    fn location_origin_serialises_the_document_origin() {
        let host = sample_host(); // https://example.test/page.html
        assert_eq!(
            eval_snippet("location.origin", &host)
                .as_string()
                .map(|s| s.to_std_string_escaped()),
            Some("https://example.test".to_string())
        );
        // `new URL(path, location.origin)` — the exact Mastodon call shape —
        // resolves instead of throwing.
        assert_eq!(
            eval_snippet(
                r#"new URL("/system/emoji_data/en.json", location.origin).href === "https://example.test/system/emoji_data/en.json""#,
                &host
            )
            .as_boolean(),
            Some(true)
        );
        // Explicit ports are part of the origin.
        host.borrow_mut().url = "http://127.0.0.1:8765/a?b=1#c".into();
        assert_eq!(
            eval_snippet("location.origin", &host)
                .as_string()
                .map(|s| s.to_std_string_escaped()),
            Some("http://127.0.0.1:8765".to_string())
        );
        // Documents without a tuple origin read the opaque string "null"…
        host.borrow_mut().url = "about:blank".into();
        assert_eq!(
            eval_snippet("location.origin", &host)
                .as_string()
                .map(|s| s.to_std_string_escaped()),
            Some("null".to_string())
        );
        // …which fails loudly as a URL base instead of resolving anywhere.
        assert_eq!(
            eval_snippet(
                r#"(function () { try { new URL("/x", location.origin); return false; } catch (e) { return true; } })()"#,
                &host
            )
            .as_boolean(),
            Some(true)
        );
    }

    /// Canvas 2D glue (task: infosec.exchange blurhash placeholders). The
    /// decode is pure JS; the paint path is
    /// `getContext("2d").putImageData(createImageData(w,h), 0, 0)` — it must
    /// complete silently, with a real Uint8ClampedArray buffer, and every
    /// reading must stay honest (nothing painted ⇒ transparent black).
    #[test]
    fn canvas_answers_a_transparent_black_2d_context() {
        let (_host, mut runtime, mut doc) = image_world(DOM_PAGE);
        // The exact blurhash shape that logged `Blurhash decoding failure`.
        let source = r##"
            var el = document.createElement("canvas");
            el.width = 4; el.height = 2;
            var ctx = el.getContext("2d");
            var img = ctx.createImageData(4, 2);
            img.data.set(new Uint8ClampedArray(4 * 2 * 4).fill(7));
            ctx.putImageData(img, 0, 0);
            ctx.canvas === el
                && img.width === 4 && img.height === 2
                && img.data.length === 32
                && img.data[3] === 7
                && el.width === 4 && el.height === 2;
        "##;
        assert!(
            dom_eval(&mut runtime, &mut doc, source).to_boolean(),
            "the blurhash paint path must complete silently"
        );
        // width/height reflect the content attributes with HTML defaults.
        let source = r##"
            var el = document.createElement("canvas");
            if (el.width !== 300 || el.height !== 150) false;
            (el.width === 300 && el.height === 150) &&
            (el.setAttribute("width", "64"), el.width === 64) &&
            (el.height = 32, el.getAttribute("height") === "32");
        "##;
        assert!(
            dom_eval(&mut runtime, &mut doc, source).to_boolean(),
            "canvas width/height must reflect the attributes (defaults 300x150)"
        );
        // No GPU, no rasteriser: webgl/bitmaprenderer answer null like a
        // browser with them disabled; non-canvas elements have no context.
        let source = r##"
            var el = document.createElement("canvas");
            var div = document.createElement("div");
            el.getContext("webgl") === null
                && el.getContext("webgl2") === null
                && el.getContext("bitmaprenderer") === null
                && el.getContext("nonsense") === null
                && div.getContext("2d") === undefined;
        "##;
        assert!(
            dom_eval(&mut runtime, &mut doc, source).to_boolean(),
            "non-2d contexts must read null; elements without canvas have none"
        );
        // getImageData on a never-painted context is transparent black, and
        // the drawing surface is all no-ops with plain state properties.
        let source = r##"
            var ctx = document.createElement("canvas").getContext("2d");
            ctx.fillStyle = "#ff0000";
            ctx.imageSmoothingQuality = "high";
            ctx.fillRect(0, 0, 10, 10);
            ctx.save(); ctx.translate(3, 3); ctx.restore();
            var d = ctx.getImageData(0, 0, 2, 2);
            d.width === 2 && d.height === 2 && d.data.length === 16
                && d.data.every(function (b) { return b === 0; })
                && ctx.fillStyle === "#ff0000"
                && ctx.getLineDash().length === 0
                && ctx.measureText("x").width === 0;
        "##;
        assert!(
            dom_eval(&mut runtime, &mut doc, source).to_boolean(),
            "a never-painted context reads transparent black with plain state"
        );
    }
}


#[cfg(test)]
mod user_agent_tests {
    use super::*;

    fn fresh_host() -> HostRef {
        Rc::new(RefCell::new(HostState::new(
            "https://example.test/ua",
            None,
            None,
        )))
    }

    /// One fresh document-world (prelude + natives, no page scripts) per call.
    fn eval_string(source: &str, host: &HostRef) -> String {
        let mut runtime = JsRuntime::new(Rc::clone(host));
        let value = runtime
            .eval(source)
            .expect("snippet should evaluate");
        value
            .as_string()
            .map(|string| string.to_std_string_escaped())
            .unwrap_or_else(|| format!("<not a string: {}>", value.display()))
    }

    /// Without a tab cell attached (bare JS tests, the headless harness),
    /// `navigator.userAgent` reports the engine default verbatim.
    #[test]
    fn navigator_reports_the_engine_default_without_a_cell() {
        let host = fresh_host();
        assert!(host.borrow().user_agent.is_none());
        assert_eq!(
            eval_string("navigator.userAgent", &host).as_str(),
            crate::netmod::USER_AGENT
        );
    }

    /// With the tab's cell attached, `navigator.userAgent` reports the
    /// effective UA — and reads the cell LIVE: a `SetUserAgent` mid-session
    /// (one write to the shared cell) changes what the very next script
    /// reads, without a new document or Context.
    #[test]
    fn navigator_reads_the_live_effective_cell() {
        let host = fresh_host();
        let cell = crate::netmod::new_user_agent(Some("  Flag/1.0  "));
        host.borrow_mut().user_agent = Some(cell.clone());
        let mut runtime = JsRuntime::new(Rc::clone(&host));
        let read = |runtime: &mut JsRuntime| -> String {
            runtime
                .eval("navigator.userAgent")
                .expect("navigator.userAgent evaluates")
                .as_string()
                .map(|string| string.to_std_string_escaped())
                .expect("a string")
        };
        assert_eq!(read(&mut runtime).as_str(), "Flag/1.0");
        // The exact write `ToTab::SetUserAgent` performs on the tab's cell:
        *cell.lock().expect("ua mutex") =
            crate::netmod::resolve_user_agent(Some("Second/2.0"));
        assert_eq!(
            read(&mut runtime).as_str(),
            "Second/2.0",
            "the same live context must see the new UA without any reload"
        );
        // None ⇒ back to the built-in default.
        *cell.lock().expect("ua mutex") = crate::netmod::resolve_user_agent(None);
        assert_eq!(read(&mut runtime).as_str(), crate::netmod::USER_AGENT);
    }

    /// The whole `navigator` object exists (it was not built before this
    /// task): the standard static properties answer alongside the live UA.
    #[test]
    fn navigator_object_is_present() {
        let host = fresh_host();
        assert_eq!(
            eval_string(
                "String(typeof navigator === 'object' && typeof navigator.userAgent === 'string')",
                &host,
            )
            .as_str(),
            "true"
        );
        assert_eq!(
            eval_string("navigator.platform", &host).as_str(),
            "Linux x86_64"
        );
        // appVersion is the UA minus its "Mozilla/" prefix (spec behaviour).
        assert_eq!(
            eval_string("navigator.appVersion", &host).as_str(),
            crate::netmod::USER_AGENT
                .strip_prefix("Mozilla/")
                .unwrap_or(crate::netmod::USER_AGENT)
        );
    }
}

#[cfg(test)]
mod local_storage_tests {
    use super::*;

    fn store(dir: &std::path::Path) -> SharedLocalStorage {
        Arc::new(std::sync::Mutex::new(
            ironsurf_storage::LocalStorage::open(dir.join("local-storage")),
        ))
    }

    fn host_for(url: &str, cell: Option<SharedLocalStorage>) -> HostRef {
        let mut state = HostState::new(url, None, None);
        state.local_storage = cell;
        Rc::new(RefCell::new(state))
    }

    /// Evaluate a snippet in a fresh per-document world (prelude + natives).
    fn eval_in(host: &HostRef, source: &str) -> JsValue {
        let mut runtime = JsRuntime::new(Rc::clone(host));
        runtime.eval(source).expect("snippet should evaluate")
    }

    fn eval_str(host: &HostRef, source: &str) -> String {
        eval_in(host, source)
            .as_string()
            .map(|s| s.to_std_string_escaped())
            .unwrap_or_default()
    }

    /// The whole WinterTC Storage surface works — methods, `length`,
    /// `key(i)`, and the Proxy forms (`ls.foo = v`, `Object.keys`) — all
    /// landing in the container store, and `window.localStorage ===
    /// localStorage` like browsers.
    #[test]
    fn storage_surface_round_trips_through_the_native() {
        let dir = tempfile::tempdir().expect("tempdir");
        let host = host_for(
            "https://app.example.test:8443/path/page.html",
            Some(store(dir.path())),
        );
        let ok = eval_in(
            &host,
            r#"
            localStorage.setItem("theme", "dark");
            localStorage.setItem("count", "7");
            var ok =
                localStorage.getItem("theme") === "dark" &&
                localStorage.getItem("missing") === null &&
                localStorage.length === 2 &&
                localStorage.key(0) === "theme" &&
                localStorage.key(1) === "count" &&
                localStorage.key(9) === null;
            localStorage.bag = "kitchen";                    // Proxy set = setItem
            ok = ok &&
                localStorage["bag"] === "kitchen" &&
                Object.keys(localStorage).join(",") === "theme,count,bag" &&
                "theme" in localStorage &&
                !("nope" in localStorage);
            delete localStorage.theme;                       // Proxy delete = removeItem
            ok = ok &&
                localStorage.getItem("theme") === null &&
                localStorage.length === 2 &&
                localStorage.removeItem("count") === true &&
                localStorage.length === 1 &&
                window.localStorage === localStorage;
            ok;
            "#,
        );
        assert_eq!(ok.as_boolean(), Some(true), "storage surface failed: {ok:?}");
    }

    /// THE security property: the area is the document origin. Two documents
    /// in one container (one store!) on different origins cannot read or
    /// overwrite each other; same-origin reloads see the same area.
    #[test]
    fn areas_are_isolated_per_origin_and_shared_same_origin() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cell = store(dir.path());
        let http_a = host_for("http://shared.test:8080/one", Some(Arc::clone(&cell)));
        let http_b = host_for("http://other.test:8080/two", Some(Arc::clone(&cell)));
        let https_a = host_for("https://shared.test:8080/", Some(Arc::clone(&cell)));
        let same_a = host_for("http://shared.test:8080/elsewhere", Some(Arc::clone(&cell)));

        eval_in(&http_a, r#"localStorage.setItem("session", "alpha")"#);
        eval_in(&http_b, r#"localStorage.setItem("session", "beta")"#);

        assert_eq!(
            eval_str(&http_a, r#"localStorage.getItem("session")"#),
            "alpha",
            "the first origin keeps its own value"
        );
        assert_eq!(
            eval_str(&http_b, r#"localStorage.getItem("session")"#),
            "beta",
            "the second origin never sees the first's data"
        );
        assert!(
            eval_in(&https_a, r#"localStorage.getItem("session")"#).is_null(),
            "https://shared.test:8080 is a DIFFERENT origin than http://shared.test:8080"
        );
        assert_eq!(
            eval_str(&same_a, r#"localStorage.getItem("session")"#),
            "alpha",
            "same origin, any path — the area is shared like browsers"
        );
        // The origin the native reports matches the storage crate's key.
        assert_eq!(
            eval_str(
                &http_a,
                r#"(function(){ var r = __ironsurfStorage("origin"); return String(r[1]); })()"#
            ),
            "http://shared.test:8080"
        );
    }

    /// Documents WITHOUT a durable origin — `about:blank`, `data:`, `file:`,
    /// `javascript:` — get no area at all: every access throws an error
    /// named `SecurityError`, and nothing is written to the store. This is
    /// what stops a `data:` iframe or a sandboxed about:blank page from
    /// riding on some real origin's cookies-of-the-web.
    #[test]
    fn opaque_origins_throw_security_error_on_every_access() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cell = store(dir.path());
        for url in [
            "about:blank",
            "data:text/html,<script>steal()</script>",
            "file:///home/user/private.html",
            "javascript:alert(1)",
        ] {
            let host = host_for(url, Some(Arc::clone(&cell)));
            let name = eval_str(
                &host,
                r#"
                try { localStorage.setItem("pwned", "yes"); "no-throw"; }
                catch (e) { e.name; }
                "#,
            );
            assert_eq!(name, "SecurityError", "{url} must be denied access");
            let name = eval_str(
                &host,
                r#"try { localStorage.getItem("x"); "no-throw"; } catch (e) { e.name; }"#,
            );
            assert_eq!(name, "SecurityError", "{url}: getItem must throw too");
            let name = eval_str(
                &host,
                r#"try { localStorage.length; "no-throw"; } catch (e) { e.name; }"#,
            );
            assert_eq!(name, "SecurityError", "{url}: length must throw too");
            let name = eval_str(
                &host,
                r#"try { localStorage.clear(); "no-throw"; } catch (e) { e.name; }"#,
            );
            assert_eq!(name, "SecurityError", "{url}: clear must throw too");
        }
        // Nothing from the denied documents reached the container store.
        let area_origin = ironsurf_storage::storage_origin_key("http://shared.test:8080")
            .expect("origin");
        assert!(
            cell.lock().expect("store lock").entries(&area_origin).is_empty(),
            "no opaque document wrote anything"
        );
    }

    /// Without a store cell at all (bare tests) the surface behaves exactly
    /// like an opaque origin: present, callable, denied — never a crash.
    #[test]
    fn missing_store_denies_like_an_opaque_origin() {
        let host = host_for("https://example.test/", None);
        let name = eval_str(
            &host,
            r#"try { localStorage.setItem("a", "b"); "no-throw"; } catch (e) { e.name; }"#,
        );
        assert_eq!(name, "SecurityError");
    }

    /// The per-origin 5 MiB quota surfaces as `QuotaExceededError`, and a
    /// refused write changes nothing.
    #[test]
    fn quota_surfaces_as_quota_exceeded_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let host = host_for("https://quota.test/", Some(store(dir.path())));
        let outcome = eval_str(
            &host,
            r#"
            localStorage.setItem("small", "fits");
            var big = "x".repeat(5 * 1024 * 1024);   // the whole 5 MiB quota in one value
            var name = "no-throw";
            try { localStorage.setItem("huge", big); }
            catch (e) { name = e.name; }
            name + "|" + localStorage.getItem("small") + "|" + localStorage.length;
            "#,
        );
        assert_eq!(outcome, "QuotaExceededError|fits|1");
    }

    /// Persistence: values a document stored are there for the NEXT
    /// navigation (a fresh realm over the same store) and after a process
    /// restart (a fresh store over the same directory).
    #[test]
    fn values_survive_navigation_and_restart() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cell = store(dir.path());
        let writer = host_for("https://persist.test/", Some(Arc::clone(&cell)));
        eval_in(&writer, r#"localStorage.setItem("keep", "me")"#);
        drop(writer);

        let reader = host_for("https://persist.test/next", Some(Arc::clone(&cell)));
        assert_eq!(
            eval_str(&reader, r#"localStorage.getItem("keep")"#),
            "me",
            "a fresh realm on the same origin reads the stored value"
        );
        drop(reader);
        drop(cell);

        // Simulated tab restart: a brand-new store over the same directory.
        let restarted = store(dir.path());
        let late = host_for("https://persist.test/", Some(restarted));
        assert_eq!(
            eval_str(&late, r#"localStorage.getItem("keep")"#),
            "me",
            "localStorage is durable on disk, per origin"
        );
    }

}

#[cfg(test)]
mod document_cookie_tests {
    use super::*;

    fn jar_cell() -> SharedCookieJar {
        Arc::new(std::sync::Mutex::new(ironsurf_storage::CookieJar::new()))
    }

    fn host_for(url: &str, jar: Option<SharedCookieJar>) -> HostRef {
        let mut state = HostState::new(url, None, None);
        state.cookies = jar;
        Rc::new(RefCell::new(state))
    }

    /// Evaluate a snippet in a fresh per-document world (prelude + natives).
    fn eval_in(host: &HostRef, source: &str) -> JsValue {
        let mut runtime = JsRuntime::new(Rc::clone(host));
        runtime.eval(source).expect("snippet should evaluate")
    }

    fn eval_str(host: &HostRef, source: &str) -> String {
        eval_in(host, source)
            .as_string()
            .map(|s| s.to_std_string_escaped())
            .unwrap_or_default()
    }

    /// The regression pin for the axios XSRF crash (the Mastodon console
    /// `TypeError: cannot convert 'null' or 'undefined' to object at read`):
    /// `document.cookie` answers a STRING — possibly empty — on every
    /// document, jar or no jar, so axios's `document.cookie.match(...)`
    /// parses and answers `null` instead of throwing.
    #[test]
    fn cookie_getter_is_always_a_string_never_undefined() {
        for url in ["about:blank", "data:text/html,x", "file:///x/y.html", "https://site.test/"] {
            let host = host_for(url, None);
            assert_eq!(
                eval_str(&host, "typeof document.cookie"),
                "string",
                "{url}: document.cookie must be a string"
            );
            assert_eq!(eval_str(&host, "document.cookie"), "", "{url}: cookieless reads empty");
        }
        // The exact axios XSRF shape, live off a cookieless document.
        let host = host_for("https://site.test/", None);
        let answer = eval_in(
            &host,
            r#"(function () {
                var m = document.cookie.match(/(?:^|;\s*)(XSRF-TOKEN)=([^;]*)/);
                return m ? decodeURIComponent(m[2]) : "none";
            })()"#,
        );
        assert_eq!(answer.as_string().map(|s| s.to_std_string_escaped()), Some("none".into()));
    }

    /// Both directions ride the jar: server-set cookies appear in the
    /// getter (HttpOnly ones NEVER do, while they still go out on the wire),
    /// page-set cookies land in the jar, a page's `HttpOnly` attribute is
    /// stripped (scripts cannot mint one), and longer paths sort first.
    #[test]
    fn cookie_writes_and_reads_ride_the_container_jar() {
        let jar = jar_cell();
        let url = "https://forum.example.test/thread/42";
        let host = host_for(url, Some(Arc::clone(&jar)));
        // Seed the jar the way a server response would.
        jar.lock().unwrap().set_from_set_cookie(
            &Url::parse(url).unwrap(),
            "session=SECRET; Path=/; HttpOnly",
            crate::netmod::now_unix(),
        );
        jar.lock().unwrap().set_from_set_cookie(
            &Url::parse(url).unwrap(),
            "prefs=light; Path=/",
            crate::netmod::now_unix(),
        );
        assert_eq!(
            eval_str(&host, "document.cookie"),
            "prefs=light",
            "HttpOnly stays off the script view"
        );
        // Page writes: a default-path cookie and one path-scoped with an
        // attempted HttpOnly flag.
        eval_in(
            &host,
            r#"
            document.cookie = "draft=d1";
            document.cookie = "note=n; HttpOnly; Path=/thread";
            "#,
        );
        assert_eq!(
            eval_str(&host, "document.cookie"),
            "draft=d1; note=n; prefs=light",
            "RFC6265 longer-path-first order, page cookies visible"
        );
        let wire = jar
            .lock()
            .unwrap()
            .cookie_header(&Url::parse(url).unwrap(), crate::netmod::now_unix());
        assert_eq!(
            wire.as_deref(),
            Some("draft=d1; note=n; session=SECRET; prefs=light"),
            "the wire rides everything, including the script-set cookies"
        );
        {
            let jar = jar.lock().unwrap();
            let note = jar
                .cookies()
                .iter()
                .find(|c| c.name == "note")
                .expect("note landed in the jar");
            assert!(!note.http_only, "a page's HttpOnly attribute must be stripped");
        }
        // Path scoping: from another path on the same host only the
        // root-path cookies remain visible.
        let elsewhere = host_for("https://forum.example.test/other", Some(Arc::clone(&jar)));
        assert_eq!(eval_str(&elsewhere, "document.cookie"), "prefs=light");
        // Deletion the browser way: an expired Max-Age removes the cookie.
        eval_in(&host, r#"document.cookie = "draft=x; Max-Age=0";"#);
        assert_eq!(
            eval_str(&host, "document.cookie"),
            "note=n; prefs=light",
            "Max-Age=0 deletes (name+path must match)"
        );
    }

    /// An expired `Expires=` write deletes like browsers; a second jar (a
    /// different container) shares nothing, and writes on a cookieless
    /// document are silently dropped rather than throwing.
    #[test]
    fn expiry_deletes_and_cookieless_documents_drop_writes() {
        let jar = jar_cell();
        let host = host_for("https://shop.example.test/cart", Some(Arc::clone(&jar)));
        eval_in(&host, r#"document.cookie = "cart=9; Expires=Wed, 09 Jun 2021 10:18:14 GMT";"#);
        assert_eq!(eval_str(&host, "document.cookie"), "");
        let other = host_for("https://shop.example.test/cart", None);
        assert_eq!(
            eval_str(&other, r#"(function(){ document.cookie = "x=1"; return document.cookie; })()"#),
            "",
            "no jar: writes are inert, reads stay empty"
        );
    }


}

#[cfg(test)]
mod request_init_tests {
    use super::*;

    fn host() -> HostRef {
        Rc::new(RefCell::new(HostState::new(
            "https://example.test/page.html",
            None,
            None,
        )))
    }

    /// Evaluate a snippet in a fresh per-document world (prelude + natives,
    /// including boa_runtime's fetch/Request layer).
    fn eval(host: &HostRef, source: &str) -> JsValue {
        let mut runtime = JsRuntime::new(Rc::clone(host));
        runtime.eval(source).expect("snippet should evaluate")
    }

    fn eval_true(host: &HostRef, source: &str) -> bool {
        eval(host, source).as_boolean().unwrap_or(false)
    }

    /// A RequestInit with NULLABLE members present-as-null (`{signal: null}`,
    /// `{body: null}`) is the spec's own way of saying "no signal" / "no
    /// body", but boa_runtime 0.22 converts init through `TryFromJs`, whose
    /// `Option<T>` demuxes only `undefined` — a present `null` fell into the
    /// conversion arm and died as `TypeError: cannot convert value to a
    /// Object` (signal) or "Request constructor: body is not a supported
    /// type" (body). This is the path Mastodon's axios fetch adapter walks:
    /// every request is `new Request(url, Object.assign(Object.create(null),
    /// N, {signal: O, method, headers, body: data, duplex, credentials}))`,
    /// so a GET arrives with null-prototype init and null-ish signal/data.
    /// The prelude normalises present-but-null `signal`/`body` to undefined
    /// through a shallow copy before the raw converter sees them. (Asserts
    /// are behavioural: boa_runtime 0.22 exposes NO `.method`/`.body`/
    /// `.signal` getters on Request, so echo checks happen response-side in
    /// the real-tab `fetch_init` e2e instead of on the instance here.)
    #[test]
    fn request_init_normalises_null_signal_and_body() {
        let host = host();
        // The bare shape: null signal AND null body construct, not throw.
        assert!(
            eval_true(
                &host,
                r#"(function () {
                    var r = new Request("https://example.test/api", { signal: null, body: null });
                    return typeof r === "object" && r !== null;
                })()"#
            ),
            "null signal/body must read as no-signal/no-body, not throw"
        );
        // axios's EXACT shape: null-prototype init, null-prototype headers,
        // null signal + body, unknown members (duplex, credentials)
        // tolerated — and the caller's object left untouched (the copy is a
        // copy, never a mutation).
        assert!(
            eval_true(
                &host,
                r#"(function () {
                    var h = Object.assign(Object.create(null), { Accept: "application/json, text/plain, */*" });
                    var P = Object.assign(Object.create(null), {
                        signal: null, method: "GET", headers: h, body: null,
                        duplex: "half", credentials: "same-origin",
                    });
                    var r = new Request("https://example.test/api2", P);
                    return r !== null && P.signal === null && P.body === null;
                })()"#
            ),
            "the axios-shaped init must construct and stay untouched"
        );
        // Class surface intact through the wrapper.
        assert!(
            eval_true(
                &host,
                r#"(function () {
                    var r = new Request("https://example.test/api3", { signal: null });
                    return Request.name === "Request"
                        && r instanceof Request
                        && Request.prototype.isPrototypeOf(r);
                })()"#
            ),
            "the Request wrapper keeps the class-shaped surface browsers expose"
        );
        // A REAL AbortSignal still flows into the constructor untouched.
        assert!(
            eval_true(
                &host,
                r#"(function () {
                    var c = new AbortController();
                    new Request("https://example.test/api4", { signal: c.signal });
                    return true;
                })()"#
            ),
            "a non-null signal must survive the normalisation"
        );
    }
}

#[cfg(test)]
mod typeerror_message_taxonomy {
    use super::*;

    fn eval_err(src: &str) -> String {
        let host = Rc::new(RefCell::new(HostState::new("https://example.test/p.html", None, None)));
        let mut runtime = JsRuntime::new(Rc::clone(&host));
        match runtime.eval(src) {
            Ok(_) => String::new(),
            Err(e) => format!("{e}").lines().next().unwrap_or_default().to_owned(),
        }
    }

    /// The taxonomy of Boa's TypeErrors, pinned because the infosec.exchange
    /// console forensics turn on it: a failed MEMBER ACCESS on null/undefined
    /// reports "cannot convert 'null' or 'undefined' to object" (`to_object`),
    /// while a failed DESTRUCTURING reports "Cannot destructure 'null' value".
    /// A React component rendering into the first reports the former from the
    /// member reads inside its first hook (e.g. a selector reading a key off a
    /// MISSING preloaded-state slice); seeing the destructure wording instead
    /// means the null sat in a binding site, not an access site. A call on a
    /// non-object reports "not a callable function".
    /// When the failing access sits INSIDE a function body, Boa reports the
    /// access column within that frame, and the frame carries the function
    /// name — used to localize the infosec `at P (:2:6210)` crash.
    #[test]
    fn failure_inside_function_reports_inner_column() {
        let err = eval_err("function P({ text: t }) { return t.startsWith(`#`); } P({})");
        println!("inside-fn err: {err}");
        assert!(err.starts_with("TypeError: cannot convert"), "{err}");
        let line2 = err.lines().nth(1).unwrap_or_default();
        assert!(line2.contains("at P"), "{err}");
    }

    #[test]
    fn access_destructure_and_call_have_distinct_messages() {
        assert_eq!(
            eval_err("var a=null; a.addNestedSub"),
            "TypeError: cannot convert 'null' or 'undefined' to object (unknown at :1:15)"
        );
        assert_eq!(
            eval_err("var b=undefined; b.deep.here"),

            "TypeError: cannot convert 'null' or 'undefined' to object (unknown at :1:20)"
        );
        assert_eq!(
            eval_err("function P({a:e,b:t}){return 1} P.call(undefined, null)"),
            "TypeError: Cannot destructure 'null' value (unknown at :1:39)"
        );
        assert_eq!(
            eval_err("const P=({a:e,b:t,...c})=>1; P.call(undefined, null)"),
            "TypeError: Cannot destructure 'null' value (unknown at :1:36)"
        );
        assert_eq!(eval_err("var t=null; t()"), "TypeError: not a callable function (unknown at :1:14)");
        // Rest-args do NOT shield the binding failure: apply([undefined]) still dies.
        let rest = eval_err("const f=({a})=>1; Reflect.apply(f, undefined, [undefined, {}])");
        assert!(
            rest.starts_with("TypeError: Cannot destructure 'undefined' value"),
            "rest-args do not shield the binding failure: {rest}"
        );
    }
}

#[cfg(test)]
mod boa_site_scratch {
    use super::*;

    #[test]
    #[ignore = "scratch: where boa locates null-arg destructuring failures"]
    fn boa_null_arg_site() {
        for (name, code) in [
            ("arrow-param-null", "const P=({a,b=1,c})=>a; P(null);"),
            ("fn-param-null", "function Q({a}){return a} Q(undefined);"),
            ("arrow-rest-prop-null", "const R=({a, ...c})=>String(a)+typeof c; R(null);"),
            ("arrow-rest-prop-ok", "const S=({a=2, ...c})=>String(a)+typeof c; S({});"),
            ("arrow-defaults-null", "const T=({a=1,b=2})=>a+b; T(null);"),
        ] {
            let host = Rc::new(RefCell::new(HostState::new("https://example.test/p.html", None, None)));
            let mut runtime = JsRuntime::new(Rc::clone(&host));
            match runtime.eval(code) {
                Ok(_) => println!("{name}: no error"),
                Err(e) => println!("{name}: FULL\n{e}"),
            }
        }
    }
}
