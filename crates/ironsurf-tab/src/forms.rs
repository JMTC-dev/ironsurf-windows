//! HTML `<form>` support (task: forms).
//!
//! Blitz parses forms and toggles checkbox/radio/text-input state, but its
//! submission algorithm is incomplete: `<select>` values are a TODO in
//! `construct_entry_list` (blitz-dom `form.rs`), and submissions whose action
//! resolves to anything other than `http(s):`/`data:`/`mailto:` are dropped
//! on the floor. Rather than fork blitz, the tab owns the submission
//! algorithm here and recognises its two triggers alongside the normal input
//! pipeline (nothing is swallowed — the page's own JS click/keydown handlers
//! run through the glue's `__ironsurfFire` either way, and a `preventDefault`
//! on the click cancels the submission):
//!
//! * a left-button press on a submit control ([`submit_control_for_click`],
//!   decided from blitz's live hover node — the shell always moves the mouse
//!   before pressing) *arms* the submission and the matching release
//!   finalises it, replacing whatever blitz's own (select-less) submission
//!   its click default put in the navigation hook — and
//! * Enter in a single-line text field ([`implicit_submission`], mirroring
//!   blitz's blocking rule: more than one text field in the form ⇒ no
//!   implicit submission), which skips feeding the Enter to blitz so the
//!   engine cannot double-submit.
//!
//! Both feed [`build_submission`]: the entry list ([`entry_list`]) follows
//! the HTML spec's "constructing the form data set" as far as this engine
//! goes (tree order, `form=`/ancestor ownership, disabled/nameless/`datalist`
//! exclusions, checkbox/radio checkedness, live parley editor contents for
//! text inputs and textareas, select selectedness), then GET becomes a query
//! string and POST a request body per `enctype` (urlencoded, text/plain,
//! multipart/form-data for string entries).
//!
//! `<select>` gets a second job here: blitz renders its `<option>` children
//! as plain text with no interaction, so the tab implements selection
//! directly ([`option_for_click`] + [`apply_option_click`]): clicking an
//! option toggles its `selected` attribute (radio-like for single selects,
//! additive for `multiple`), restyles the row, and keeps the `<select>`'s
//! `value` **attribute** in sync — the attribute is what a spec-faithful
//! fallback reads, so a selection made in this engine flows into any
//! submission path.
//!
//! [`prepare_document`] runs once per parsed document (before the first
//! layout): it initialises textarea editors from their child text (blitz's
//! editor would otherwise start empty) and the selected/value state of every
//! `select` that never marked an option.

use blitz_dom::{local_name, BaseDocument, ElementData, LocalName, NodeId, QualName};

/// The HTML input types that render as single-line text editors (the ones
/// whose Enter key means "submit", per the single- vs multiline flag of the
/// parley editors blitz builds).
pub const SINGLE_LINE_TEXT_TYPES: &[&str] = &[
    "text",
    "search",
    "email",
    "url",
    "tel",
    "password",
    "number",
];

/// Where the submission travels: GET carries everything in the URL query,
/// POST carries [`FormSubmission::body`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FormMethod {
    Get,
    Post,
}

/// A form submission resolved against the document URL, ready for the tab's
/// fetch layer. `content_type`/`body` are meaningful for POST only.
#[derive(Clone, Debug, PartialEq)]
pub struct FormSubmission {
    pub url: url::Url,
    pub method: FormMethod,
    pub content_type: Option<String>,
    pub body: Vec<u8>,
    /// Number of entry pairs submitted (logging only).
    pub entries: usize,
}

// ---------------------------------------------------------------------------
// Tiny DOM helpers over blitz's read-only tree + mutator

fn element<'a>(doc: &'a BaseDocument, id: NodeId) -> Option<&'a ElementData> {
    doc.get_node(id)?.element_data()
}

fn is_tag(el: &ElementData, name: &LocalName) -> bool {
    el.name.local == *name
}

fn attr(doc: &BaseDocument, id: NodeId, name: LocalName) -> Option<&str> {
    element(doc, id).and_then(|el| el.attr(name))
}

fn html_attr(name: LocalName) -> QualName {
    QualName::new(None, blitz_dom::ns!(html), name)
}

/// Walk strictly above `id` up the tree, returning the first ancestor `f`
/// accepts.
fn find_ancestor(doc: &BaseDocument, id: NodeId, f: impl Fn(NodeId) -> bool) -> Option<NodeId> {
    let mut cur = doc.get_node(id)?.parent?;
    loop {
        if f(cur) {
            return Some(cur);
        }
        cur = doc.get_node(cur)?.parent?;
    }
}

fn in_datalist(doc: &BaseDocument, id: NodeId) -> bool {
    find_ancestor(doc, id, |a| {
        element(doc, a).is_some_and(|el| is_tag(el, &local_name!("datalist")))
    })
    .is_some()
}

/// The control's form owner: an explicit `form="id"` attribute pointing at a
/// `<form>`, else the nearest ancestor `<form>` (blitz's `reset_form_owner`
/// semantics, re-derived live so DOM mutations are never stale).
pub fn owner_form(doc: &BaseDocument, node_id: NodeId) -> Option<NodeId> {
    if let Some(el) = element(doc, node_id) {
        if is_tag(el, &local_name!("form")) {
            return Some(node_id);
        }
        // A `form=` attribute wins over the ancestors — but only when it
        // resolves to a real form; a dangling one falls through (spec:
        // reset-the-form-owner).
        if let Some(owner) = el.attr(local_name!("form")) {
            if let Some(owner_id) = doc.get_element_by_id(owner) {
                if element(doc, owner_id).is_some_and(|owner| is_tag(owner, &local_name!("form")))
                {
                    return Some(owner_id);
                }
            }
        }
    }
    find_ancestor(doc, node_id, |a| {
        element(doc, a).is_some_and(|el| is_tag(el, &local_name!("form")))
    })
}

/// Every node in tree (document) order — the order the spec's entry
/// construction walks the tree in.
fn tree_order(doc: &BaseDocument) -> Vec<NodeId> {
    let root = doc.root_element().id;
    let mut out = Vec::new();
    let mut stack = vec![root];
    while let Some(id) = stack.pop() {
        out.push(id);
        if let Some(node) = doc.get_node(id) {
            for child in node.children.iter().rev() {
                stack.push(*child);
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Triggers

/// Is `el` a control whose click submits the form (submit-button semantics,
/// mirroring blitz: `input type=submit` or a `button` with no `type` or
/// `type=submit`; `command`/`commandfor` buttons excluded)?
fn is_submit_control(el: &ElementData) -> bool {
    if is_tag(el, &local_name!("input")) && el.attr(local_name!("type")) == Some("submit") {
        return true;
    }
    if is_tag(el, &local_name!("button")) {
        let type_attr = el.attr(local_name!("type"));
        return (type_attr.is_none() || type_attr == Some("submit"))
            && !el.has_attr(LocalName::from("command"))
            && !el.has_attr(LocalName::from("commandfor"));
    }
    false
}

/// The submit control a left press (hovered at `hover`) activates, and its
/// form owner. The walk stops at an `<a href>` first (blitz turns that into
/// a link navigation, not a form one).
pub fn submit_control_for_click(doc: &BaseDocument, hover: NodeId) -> Option<(NodeId, NodeId)> {
    // An `<a href>` anywhere above the press makes this a link click first
    // (blitz's own default handles it) — even when the innermost element is
    // itself a submit control (invalid but seen-in-the-wild nesting).
    let mut above = doc.get_node(hover)?.parent;
    while let Some(cur) = above {
        if let Some(el) = element(doc, cur) {
            if is_tag(el, &local_name!("a")) && el.attr(local_name!("href")).is_some() {
                return None;
            }
        }
        above = doc.get_node(cur)?.parent;
    }
    let mut id = hover;
    loop {
        if let Some(el) = element(doc, id) {
            if !is_tag(el, &local_name!("form"))
                && !el.has_attr(local_name!("disabled"))
                && is_submit_control(el)
            {
                return owner_form(doc, id).map(|form| (id, form));
            }
        }
        id = doc.get_node(id)?.parent?;
    }
}

/// The `(select, option)` pair under a left press (hovered at `hover`).
pub fn option_for_click(doc: &BaseDocument, hover: NodeId) -> Option<(NodeId, NodeId)> {
    let option = find_ancestor(doc, hover, |a| {
        element(doc, a).is_some_and(|el| is_tag(el, &local_name!("option")))
    })?;
    let select = find_ancestor(doc, option, |a| {
        element(doc, a).is_some_and(|el| is_tag(el, &local_name!("select")))
    })?;
    Some((select, option))
}

/// The option's submission value: its `value` attribute, else its text.
fn option_value(doc: &BaseDocument, option: NodeId) -> String {
    match attr(doc, option, local_name!("value")) {
        Some(value) => value.to_owned(),
        None => doc
            .get_node(option)
            .map(|node| node.text_content())
            .unwrap_or_default(),
    }
}

/// Is the option selected (blitz keeps a CHECKED state bit for `selected`)?
fn option_selected(doc: &BaseDocument, option: NodeId) -> bool {
    element(doc, option)
        .and_then(ElementData::checkbox_input_checked)
        .unwrap_or_else(|| {
            element(doc, option).is_some_and(|el| el.has_attr(local_name!("selected")))
        })
}

/// Apply a click on `option` inside `select` — this engine renders selects
/// as always-open option lists, so a press selects: single selects keep
/// exactly the clicked option (clicking the incumbent changes nothing),
/// `multiple` selects toggle. The `selected` attributes, the highlight and
/// the `<select>`'s `value` attribute all follow.
pub fn apply_option_click(doc: &mut BaseDocument, select: NodeId, option: NodeId) {
    let multiple = element(doc, select)
        .is_some_and(|el| el.has_attr(local_name!("multiple")) || has_size_gt_1(el));
    let clicked_selected = option_selected(doc, option);
    let decisions: Vec<(NodeId, bool)> = select_option_ids(doc, select)
        .into_iter()
        .map(|child| {
            let keep = if child == option {
                if multiple {
                    !clicked_selected
                } else {
                    true
                }
            } else if multiple {
                option_selected(doc, child)
            } else {
                false
            };
            (child, keep)
        })
        .collect();

    let mut mutator = doc.mutate();
    for (child, keep) in decisions {
        if keep {
            mutator.set_attribute(child, html_attr(local_name!("selected")), "");
            mutator.set_style_property(child, "background-color", "#dce9ff");
        } else {
            mutator.clear_attribute(child, html_attr(local_name!("selected")));
            mutator.remove_style_property(child, "background-color");
        }
    }
    mutator.flush();
    drop(mutator);

    sync_select_value(doc, select);
}

fn has_size_gt_1(el: &ElementData) -> bool {
    el.attr(local_name!("size"))
        .and_then(|s| s.trim().parse::<u32>().ok())
        .unwrap_or(1)
        > 1
}

/// Make the `<select>`'s `value` attribute name its currently-selected
/// option (first selected, else first option) — the fallback a
/// spec-faithful entry construction reads when a control has no edit state.
pub fn sync_select_value(doc: &mut BaseDocument, select: NodeId) {
    let options = select_option_ids(doc, select);
    let chosen = options
        .iter()
        .copied()
        .find(|id| option_selected(doc, *id))
        .or_else(|| options.first().copied());
    let value = chosen.map(|id| option_value(doc, id));
    let mut mutator = doc.mutate();
    match value {
        Some(value) => mutator.set_attribute(select, html_attr(local_name!("value")), &value),
        None => mutator.clear_attribute(select, html_attr(local_name!("value"))),
    };
    mutator.flush();
}

/// Option children of a `select` in document order (`optgroup` contents are
/// flattened in place, which is all this engine renders anyway).
fn select_option_ids(doc: &BaseDocument, select: NodeId) -> Vec<NodeId> {
    let mut out = Vec::new();
    let Some(node) = doc.get_node(select) else {
        return out;
    };
    let mut stack: Vec<NodeId> = node.children.iter().copied().rev().collect();
    while let Some(id) = stack.pop() {
        if let Some(el) = element(doc, id) {
            if is_tag(el, &local_name!("option")) {
                out.push(id);
            } else if is_tag(el, &local_name!("optgroup")) {
                if let Some(group) = doc.get_node(id) {
                    stack.extend(group.children.iter().copied().rev());
                }
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Entry construction (HTML spec, engine-faithful subset)

fn is_control(el: &ElementData) -> bool {
    local_name!("input") == el.name.local
        || local_name!("textarea") == el.name.local
        || local_name!("select") == el.name.local
        || local_name!("button") == el.name.local
}

/// The live text of a text input / textarea: the parley editor blitz owns
/// (present once the document has been laid out).
fn edit_state(doc: &BaseDocument, id: NodeId) -> Option<String> {
    let editor = element(doc, id)?.text_input_data()?;
    Some(editor.editor.raw_text().to_owned())
}

/// https://html.spec.whatwg.org/#constructing-the-form-data-set (subset).
/// Returns name/value pairs in tree order for the controls owned by `form`;
/// `submitter` is the clicked button (submitter buttons contribute their own
/// name/value; all other buttons are excluded).
pub fn entry_list(
    doc: &BaseDocument,
    form: NodeId,
    submitter: Option<NodeId>,
) -> Vec<(String, String)> {
    let mut entries = Vec::new();
    for id in tree_order(doc) {
        let Some(el) = element(doc, id) else { continue };
        if !is_control(el) {
            continue;
        }
        // Only controls owned by this form, never inside a <datalist>, never
        // disabled, and (per spec) never without a non-empty name.
        if owner_form(doc, id) != Some(form) || in_datalist(doc, id) {
            continue;
        }
        if el.has_attr(local_name!("disabled")) {
            continue;
        }
        let name = match el.attr(local_name!("name")) {
            Some(name) if !name.is_empty() => name.to_owned(),
            _ => continue,
        };
        let tag_input = is_tag(el, &local_name!("input"));
        let type_attr = el.attr(local_name!("type"));

        // <button>: only the submitter contributes, value = `value` attr.
        if is_tag(el, &local_name!("button")) {
            if submitter == Some(id) {
                entries.push((
                    name,
                    el.attr(local_name!("value")).unwrap_or("").to_owned(),
                ));
            }
            continue;
        }
        // Non-field input states never contribute an entry (file picking is
        // not implemented, image buttons have no coordinates here).
        if tag_input
            && matches!(
                type_attr,
                Some("submit") | Some("button") | Some("reset") | Some("image") | Some("file")
            )
        {
            continue;
        }
        // Checkbox/radio: contribute only when checked, defaulting to "on".
        if tag_input && matches!(type_attr, Some("checkbox") | Some("radio")) {
            let checked = el
                .checkbox_input_checked()
                .unwrap_or_else(|| el.has_attr(local_name!("checked")));
            if checked {
                entries.push((
                    name,
                    el.attr(local_name!("value")).unwrap_or("on").to_owned(),
                ));
            }
            continue;
        }
        // <select>: `multiple` contributes every selected option; a single
        // select contributes the `value` attribute this engine keeps in sync
        // with selectedness (falling back to the selected/first option).
        if is_tag(el, &local_name!("select")) {
            let multiple = el.has_attr(local_name!("multiple")) || has_size_gt_1(el);
            if multiple {
                for option in select_option_ids(doc, id) {
                    if option_selected(doc, option) {
                        entries.push((name.clone(), option_value(doc, option)));
                    }
                }
            } else if let Some(value) = attr(doc, id, local_name!("value")) {
                entries.push((name, value.to_owned()));
            } else {
                let options = select_option_ids(doc, id);
                let chosen = options
                    .iter()
                    .copied()
                    .find(|o| option_selected(doc, *o))
                    .or_else(|| options.first().copied());
                if let Some(option) = chosen {
                    entries.push((name, option_value(doc, option)));
                }
            }
            continue;
        }
        // Everything else: the live edit state (text inputs and textareas
        // once laid out), then the `value` attribute (hidden, date-family),
        // then the textarea's child text.
        if let Some(text) = edit_state(doc, id) {
            entries.push((name, text));
            continue;
        }
        if tag_input {
            entries.push((name, el.attr(local_name!("value")).unwrap_or("").to_owned()));
        } else if is_tag(el, &local_name!("textarea")) {
            let text = doc.get_node(id).map(|n| n.text_content()).unwrap_or_default();
            entries.push((name, sanitize_textarea(&text)));
        }
    }
    entries
}

/// The textarea value sanitisation algorithm: a single leading newline is
/// dropped (`<textarea>\nfoo</textarea>` has the value "foo").
fn sanitize_textarea(text: &str) -> String {
    match text.strip_prefix("\r\n") {
        Some(rest) => rest.to_owned(),
        None => text
            .strip_prefix('\n')
            .map(str::to_owned)
            .unwrap_or_else(|| text.to_owned()),
    }
}

// ---------------------------------------------------------------------------
// Submission building

/// The form's default button: its first enabled submit control in tree order
/// (the implicit-submission submitter).
pub fn default_button(doc: &BaseDocument, form: NodeId) -> Option<NodeId> {
    tree_order(doc).into_iter().find(|id| {
        element(doc, *id).is_some_and(is_submit_control)
            && owner_form(doc, *id) == Some(form)
            && !element(doc, *id).is_some_and(|el| el.has_attr(local_name!("disabled")))
    })
}

/// A submitter's `formaction`/`formmethod`/`formenctype` override, honoured
/// only when `submitter` really is one of the form's submit controls.
fn submitter_override(
    doc: &BaseDocument,
    submitter: Option<NodeId>,
    name: LocalName,
) -> Option<String> {
    let submitter = submitter?;
    element(doc, submitter)
        .filter(|el| is_submit_control(el))
        .and_then(|el| el.attr(name))
        .map(str::to_owned)
}

/// Does Enter in `focused` mean implicit form submission? Returns `None`
/// when the key must reach the editor (not a single-line text field, not in
/// a form), `Err` when the form swallows Enter (more than one text field —
/// blitz's blocking rule, with typeless `<input>` counted as text), and the
/// resolved submission otherwise.
pub fn implicit_submission(
    doc: &BaseDocument,
    focused: NodeId,
) -> Option<Result<FormSubmission, String>> {
    let el = element(doc, focused)?;
    if !is_tag(el, &local_name!("input")) {
        return None; // textareas insert newlines; buttons want clicks
    }
    let single_line = el
        .attr(local_name!("type"))
        .is_none_or(|t| SINGLE_LINE_TEXT_TYPES.contains(&t));
    if !single_line || el.has_attr(local_name!("disabled")) {
        return None;
    }
    let form = owner_form(doc, focused)?;
    let text_fields = tree_order(doc)
        .into_iter()
        .filter(|id| {
            element(doc, *id).is_some_and(|el| {
                is_tag(el, &local_name!("input"))
                    && el
                        .attr(local_name!("type"))
                        .is_none_or(|t| SINGLE_LINE_TEXT_TYPES.contains(&t))
            }) && owner_form(doc, *id) == Some(form)
        })
        .count();
    if text_fields > 1 {
        return Some(Err(format!(
            "form has {text_fields} text fields; Enter does not submit it"
        )));
    }
    let submitter = default_button(doc, form);
    Some(build_submission(doc, form, submitter))
}

/// Resolve method/enctype/action (with the submitter's `formaction`/
/// `formmethod`/`formenctype` overrides) and encode the entry list into a
/// GET query or a POST body.
pub fn build_submission(
    doc: &BaseDocument,
    form: NodeId,
    submitter: Option<NodeId>,
) -> Result<FormSubmission, String> {
    let method = submitter_override(doc, submitter, local_name!("formmethod"))
        .or_else(|| attr(doc, form, local_name!("method")).map(str::to_owned))
        .map(|m| m.trim().to_ascii_lowercase())
        .map(|m| match m.as_str() {
            "post" => FormMethod::Post,
            // The spec folds every invalid method to GET; `dialog` has no
            // meaning for a single-process tab and folds too.
            _ => FormMethod::Get,
        })
        .unwrap_or(FormMethod::Get);

    let enctype = submitter_override(doc, submitter, local_name!("formenctype"))
        .or_else(|| attr(doc, form, local_name!("enctype")).map(str::to_owned))
        .map(|e| e.trim().to_ascii_lowercase())
        .unwrap_or_default();

    let action = submitter_override(doc, submitter, local_name!("formaction"))
        .or_else(|| attr(doc, form, local_name!("action")).map(str::to_owned))
        .unwrap_or_default();
    let base = doc.url().clone();
    let url = if action.trim().is_empty() {
        base
    } else {
        base.join(action.trim())
            .map_err(|err| format!("invalid form action {action:?}: {err}"))?
    };

    let entries = entry_list(doc, form, submitter);
    let count = entries.len();
    if method == FormMethod::Get {
        let mut url = url;
        url.set_query(Some(&serialize_urlencoded(&entries)));
        return Ok(FormSubmission {
            url,
            method,
            content_type: None,
            body: Vec::new(),
            entries: count,
        });
    }
    let (content_type, body) = match enctype.as_str() {
        "text/plain" => ("text/plain".to_owned(), encode_text_plain(&entries)),
        "multipart/form-data" => {
            let boundary = "----IronSurfFormBoundary7MA4YWxkTrZu0gW";
            (
                format!("multipart/form-data; boundary={boundary}"),
                encode_multipart(boundary, &entries),
            )
        }
        // Any invalid enctype falls back to the default encoding.
        _ => (
            "application/x-www-form-urlencoded".to_owned(),
            serialize_urlencoded(&entries).into_bytes(),
        ),
    };
    Ok(FormSubmission {
        url,
        method,
        content_type: Some(content_type),
        body,
        entries: count,
    })
}

/// https://html.spec.whatwg.org/#converting-an-entry-list-to-a-list-of-name-value-pairs:
/// lone CR/LF become CRLF pairs before encoding.
fn normalize_line_endings(input: &str) -> String {
    if !input.contains(['\r', '\n']) {
        return input.to_owned();
    }
    let mut out = String::with_capacity(input.len() + 8);
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        match (c, chars.peek()) {
            ('\r', Some('\n')) => {
                out.push_str("\r\n");
                chars.next();
            }
            ('\r' | '\n', _) => out.push_str("\r\n"),
            _ => out.push(c),
        }
    }
    out
}

/// `application/x-www-form-urlencoded` query/body encoding (the same
/// serializer blitz applies to GET submissions).
pub fn serialize_urlencoded(entries: &[(String, String)]) -> String {
    let mut query = String::new();
    url::form_urlencoded::Serializer::new(&mut query).extend_pairs(
        entries
            .iter()
            .map(|(name, value)| (normalize_line_endings(name), normalize_line_endings(value))),
    );
    query
}

/// The `text/plain` encoding algorithm.
fn encode_text_plain(entries: &[(String, String)]) -> Vec<u8> {
    let mut out = String::new();
    for (name, value) in entries {
        out.push_str(&normalize_line_endings(name));
        out.push('=');
        out.push_str(&normalize_line_endings(value));
        out.push_str("\r\n");
    }
    out.into_bytes()
}

/// `multipart/form-data` for the entry lists this engine produces (string
/// entries only — no file picker exists, so no part ever carries a
/// filename).
fn encode_multipart(boundary: &str, entries: &[(String, String)]) -> Vec<u8> {
    let mut out = Vec::new();
    for (name, value) in entries {
        out.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        // Quotes in the name become %22 so the content-disposition header
        // stays well-formed.
        let name = normalize_line_endings(name).replace('"', "%22");
        out.extend_from_slice(
            format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n").as_bytes(),
        );
        out.extend_from_slice(normalize_line_endings(value).as_bytes());
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    out
}

// ---------------------------------------------------------------------------
// Per-document preparation

/// UA-level CSS blitz's own `assets/default.css` lacks, appended after it via
/// `DocumentConfig::ua_stylesheets` (which **replaces** the default when
/// `Some`, so the runtime feeds back `blitz_dom::DEFAULT_CSS` first — see
/// `runtime::Tab::load_html`).
///
/// `input[type=hidden] { display: none }` is mandated by the HTML spec's UA
/// stylesheet ("hidden controls generate no box") but missing from blitz
/// beta.2, so hidden controls got layout boxes: on startpage.com the search
/// form ships ~11 `<input type=hidden>` siblings inside a flexed `<form>`,
/// they ate every flex pixel, and the query field collapsed to width 0 — the
/// painted search bar was real but unhittable, so clicking it focused nothing
/// (task: startpage.com search bar).
pub const UA_PATCH_CSS: &str = r#"input[type="hidden"] { display: none; }"#;

/// Prepare a freshly parsed document, before its first layout:
///
/// * **textarea**: blitz's parley editor starts from the `value` *attribute*
///   — which textareas never have — so seed the attribute from the element's
///   child text. The editor then displays and submits exactly what was
///   parsed, and later entry reads see the live edits.
/// * **select**: mark a selected option (the first `selected` one, else the
///   first option) and sync the `value` attribute, so an untouched select
///   still submits its first option like a real one.
pub fn prepare_document(doc: &mut BaseDocument) {
    // Every read happens before the mutator borrow below.
    enum Target {
        Textarea { id: NodeId, value: String },
        Select { first_option: Option<NodeId> },
    }
    let mut targets = Vec::new();
    for id in tree_order(doc) {
        let Some(el) = element(doc, id) else { continue };
        if in_datalist(doc, id) {
            continue;
        }
        if is_tag(el, &local_name!("textarea")) {
            if attr(doc, id, local_name!("value")).is_none() {
                let text = doc.get_node(id).map(|n| n.text_content()).unwrap_or_default();
                targets.push(Target::Textarea {
                    id,
                    value: sanitize_textarea(&text),
                });
            }
        } else if is_tag(el, &local_name!("select")) {
            let options = select_option_ids(doc, id);
            let selected = options.iter().copied().find(|o| option_selected(doc, *o));
            match selected.or_else(|| options.first().copied()) {
                Some(chosen) => targets.push(Target::Select {
                    first_option: Some(chosen),
                }),
                None => targets.push(Target::Select { first_option: None }),
            }
        }
    }
    if targets.is_empty() {
        return;
    }
    let mut mutator = doc.mutate();
    // Selects need their chosen option's *value*, which must be read through
    // the document — collect it into owned strings first via the outer loop
    // not being possible mid-borrow, so re-read inside using mutator.doc.
    for target in targets {
        match target {
            Target::Textarea { id, value } => {
                mutator.set_attribute(id, html_attr(local_name!("value")), &value);
            }
            Target::Select { first_option } => {
                if let Some(chosen) = first_option {
                    mutator.set_attribute(chosen, html_attr(local_name!("selected")), "");
                    let value = match attr(mutator.doc, chosen, local_name!("value")) {
                        Some(value) => value.to_owned(),
                        None => mutator
                            .doc
                            .get_node(chosen)
                            .map(|n| n.text_content())
                            .unwrap_or_default(),
                    };
                    let select_id = find_ancestor(mutator.doc, chosen, |a| {
                        element(mutator.doc, a).is_some_and(|el| is_tag(el, &local_name!("select")))
                    });
                    if let Some(select_id) = select_id {
                        mutator.set_attribute(select_id, html_attr(local_name!("value")), &value);
                    }
                }
            }
        }
    }
    mutator.flush();
}

#[cfg(test)]
mod preparation_notes {
    // This module documents the one quirk of `prepare_document` above: a
    // single-select whose options were never marked ends up marking the
    // first option (idempotently), which is exactly the state a real
    // browser renders it in. See `prepare_seeds_selects_and_textareas`.
}

// ---------------------------------------------------------------------------
// Unit tests (pure DOM manipulation — no painting, no network, no display)

#[cfg(test)]
mod tests {
    use super::*;
    use blitz_dom::DocumentConfig;
    use blitz_html::HtmlDocument;

    fn parse(html: &str) -> HtmlDocument {
        HtmlDocument::from_html(
            html,
            DocumentConfig {
                base_url: Some("https://example.test/page".to_owned()),
                ..Default::default()
            },
        )
    }

    fn find_tag(doc: &BaseDocument, name: LocalName) -> Option<NodeId> {
        tree_order(doc)
            .into_iter()
            .find(|id| element(doc, *id).is_some_and(|el| is_tag(el, &name)))
    }

    fn find_form(doc: &BaseDocument) -> NodeId {
        find_tag(doc, local_name!("form")).expect("the fixture has a form")
    }

    #[test]
    fn entries_follow_tree_order_with_defaults() {
        let doc = parse(
            r#"<form action="/s" method="get">
                 <input name="hidden" type="hidden" value="h">
                 <input name="text" value="t">
                 <input name="todo" type="checkbox" value="yes">
                 <input name="done" type="checkbox" value="yes" checked>
                 <input name="off" type="radio" value="r">
                 <input name="on" type="radio" value="r" checked>
                 <textarea name="note">hello</textarea>
                 <select name="colour">
                   <option value="red">Red</option>
                   <option value="blue" selected>Blue</option>
                 </select>
                 <button name="go" value="now">Go</button>
                 <button>Plain button</button>
                 <input name="dis" value="never" disabled>
                 <input value="noname">
               </form>"#,
        );
        let form = find_form(&doc);
        let entries = entry_list(&doc, form, None);
        assert_eq!(
            entries,
            vec![
                ("hidden".to_string(), "h".to_string()),
                ("text".to_string(), "t".to_string()),
                ("done".to_string(), "yes".to_string()),
                ("on".to_string(), "r".to_string()),
                ("note".to_string(), "hello".to_string()),
                ("colour".to_string(), "blue".to_string()),
            ],
            "unchecked controls, nameless, disabled and non-submitter buttons drop out"
        );
        // The clicked submitter contributes its own name/value.
        let button = find_tag(&doc, local_name!("button"));
        let with_button = entry_list(&doc, form, button);
        assert!(with_button.contains(&("go".to_string(), "now".to_string())));
    }

    #[test]
    fn get_submission_encodes_the_query() {
        let doc = parse(
            r#"<form action="/search" method="get">
                 <input name="q" value="a b&amp;c">
                 <select name="colour">
                   <option>red</option>
                   <option value="bl ue" selected>Blue</option>
                 </select>
               </form>"#,
        );
        let form = find_form(&doc);
        let sub = build_submission(&doc, form, None).expect("get submits");
        assert_eq!(sub.method, FormMethod::Get);
        assert_eq!(
            sub.url.as_str(),
            "https://example.test/search?q=a+b%26c&colour=bl+ue"
        );
        assert!(sub.body.is_empty());
        assert_eq!(sub.entries, 2);
    }

    #[test]
    fn post_submission_encodes_the_body_and_types_it() {
        // The method attribute is case-folded, so both spellings submit as POST.
        for method in ["post", "POST"] {
            let doc = parse(&format!(
                r#"<form action="/submit" method="{method}"><input name="q" value="iron surf"></form>"#,
            ));
            let form = find_form(&doc);
            let sub = build_submission(&doc, form, None).expect("POST submits");
            assert_eq!(sub.method, FormMethod::Post, "method={method}");
            assert_eq!(
                sub.content_type.as_deref(),
                Some("application/x-www-form-urlencoded")
            );
            assert_eq!(sub.body, b"q=iron+surf");
            assert_eq!(sub.url.as_str(), "https://example.test/submit");
        }
    }

    #[test]
    fn submitter_overrides_win() {
        let doc = parse(
            r#"<form action="/a" method="post" enctype="application/x-www-form-urlencoded">
                 <input name="q" value="x">
                 <button name="b" formaction="/b" formmethod="post" formenctype="text/plain">B</button>
               </form>"#,
        );
        let form = find_form(&doc);
        let button = tree_order(&doc)
            .into_iter()
            .filter(|id| {
                element(&doc, *id).is_some_and(|el| {
                    is_tag(el, &local_name!("button"))
                        && el.attr(local_name!("name")) == Some("b")
                })
            })
            .next()
            .expect("button b");
        let sub = build_submission(&doc, form, Some(button)).expect("submitter submits");
        assert_eq!(sub.url.as_str(), "https://example.test/b");
        assert_eq!(sub.content_type.as_deref(), Some("text/plain"));
        assert_eq!(sub.body, b"q=x\r\nb=\r\n");
    }

    #[test]
    fn multipart_body_shape() {
        let entries = vec![
            ("name".to_string(), "value".to_string()),
            ("quote\"d".to_string(), "two\r\nlines".to_string()),
        ];
        let body = String::from_utf8(encode_multipart("BND", &entries)).expect("utf8");
        assert_eq!(
            body,
            "--BND\r\nContent-Disposition: form-data; name=\"name\"\r\n\r\nvalue\r\n\
             --BND\r\nContent-Disposition: form-data; name=\"quote%22d\"\r\n\r\ntwo\r\nlines\r\n\
             --BND--\r\n"
        );
    }

    #[test]
    fn prepare_seeds_selects_and_textareas() {
        let mut doc = parse(
            r#"<form>
                 <select name="s"><option value="one">One</option><option value="two">Two</option></select>
                 <textarea name="t">
line two</textarea>
               </form>"#,
        );
        prepare_document(&mut doc);
        let form = find_form(&doc);
        let entries = entry_list(&doc, form, None);
        assert!(
            entries.contains(&("s".to_string(), "one".to_string())),
            "an untouched select submits its first option: {entries:?}"
        );
        // The leading newline of the textarea content is sanitised away and
        // the value attribute (what the editor will start from) agrees.
        assert!(
            entries.contains(&("t".to_string(), "line two".to_string())),
            "textarea default: {entries:?}"
        );
    }

    #[test]
    fn option_click_moves_selection_and_value_attribute() {
        let mut doc = parse(
            r#"<form>
                 <select name="s">
                   <option value="one">One</option>
                   <option value="two">Two</option>
                   <option value="three">Three</option>
                 </select>
               </form>"#,
        );
        prepare_document(&mut doc);
        let form = find_form(&doc);
        let select = find_tag(&doc, local_name!("select")).expect("select");
        let third = select_option_ids(&doc, select)[2];

        apply_option_click(&mut doc, select, third);
        let entries = entry_list(&doc, form, None);
        assert!(
            entries.contains(&("s".to_string(), "three".to_string())),
            "the clicked option submits: {entries:?}"
        );

        // A single select keeps exactly one selection; clicking the current
        // one changes nothing.
        apply_option_click(&mut doc, select, third);
        let entries = entry_list(&doc, form, None);
        assert_eq!(
            entries.iter().filter(|(n, _)| n == "s").count(),
            1,
            "single selects stay single-valued: {entries:?}"
        );
    }

    #[test]
    fn multiple_select_toggles_and_submits_every_selection() {
        let mut doc = parse(
            r#"<form><select name="m" multiple>
                 <option value="a">A</option><option value="b">B</option><option value="c">C</option>
               </select></form>"#,
        );
        prepare_document(&mut doc);
        let form = find_form(&doc);
        let select = find_tag(&doc, local_name!("select")).expect("select");
        let ids = select_option_ids(&doc, select);
        // Seed starts with A selected (prepare marks the first option); the
        // fixture is multiple, so prepare's seed is one of possibly many.
        // Deselect A, select C, and re-select B.
        apply_option_click(&mut doc, select, ids[0]);
        apply_option_click(&mut doc, select, ids[2]);
        apply_option_click(&mut doc, select, ids[1]);
        let entries = entry_list(&doc, form, None);
        let selected: Vec<&str> = entries
            .iter()
            .filter(|(n, _)| n == "m")
            .map(|(_, v)| v.as_str())
            .collect();
        assert_eq!(selected, vec!["b", "c"], "toggled multi-select: {entries:?}");
    }

    #[test]
    fn explicit_form_attribute_owns_controls_outside_the_form() {
        let doc = parse(
            r#"<form id="f" action="/x"><input name="a" value="1"></form>
               <input form="f" name="b" value="2">
               <input form="nope" name="c" value="3">"#,
        );
        let form = find_form(&doc);
        let entries = entry_list(&doc, form, None);
        assert!(
            entries.contains(&("b".to_string(), "2".to_string())),
            "{entries:?}"
        );
        assert!(
            !entries.iter().any(|(n, _)| n == "c"),
            "dangling form= owns nothing"
        );
    }

    #[test]
    fn submit_control_walk_stops_at_links() {
        let doc = parse(
            r#"<form><button name="go">Go</button></form>
               <form><a href="/x"><button name="inner">wrap</button></a></form>"#,
        );
        let buttons: Vec<NodeId> = tree_order(&doc)
            .into_iter()
            .filter(|id| element(&doc, *id).is_some_and(|el| is_tag(el, &local_name!("button"))))
            .collect();
        assert_eq!(buttons.len(), 2);
        assert!(submit_control_for_click(&doc, buttons[0]).is_some());
        // A button inside a link is a link click, not a form submit.
        assert!(submit_control_for_click(&doc, buttons[1]).is_none());
    }

    #[test]
    fn implicit_submission_blocks_multi_field_forms_and_uses_the_default_button() {
        let one = parse(
            r#"<form action="/i" method="get">
                 <input name="q" value="solo">
                 <button formaction="/d">Go</button>
               </form>"#,
        );
        let focused = find_tag(&one, local_name!("input")).expect("input");
        let sub = implicit_submission(&one, focused)
            .expect("single text field: Enter submits")
            .expect("and it resolves");
        assert_eq!(sub.url.as_str(), "https://example.test/d?q=solo");

        let many = parse(r#"<form><input name="a"><input name="b"></form>"#);
        let first = find_tag(&many, local_name!("input")).expect("input");
        let err = implicit_submission(&many, first)
            .expect("multi-field forms are reported, not silent")
            .expect_err("Enter must not submit");
        assert!(err.contains("text fields"), "{err}");
    }

    #[test]
    fn datalist_options_never_submit() {
        let doc = parse(
            r#"<form action="/d">
                 <input name="q" value="keep" list="opts">
                 <datalist id="opts"><option name="q" value="drop"></option></datalist>
               </form>"#,
        );
        let form = find_form(&doc);
        let entries = entry_list(&doc, form, None);
        assert_eq!(entries, vec![("q".to_string(), "keep".to_string())]);
    }

    #[test]
    fn unknown_method_folds_to_get_and_action_is_document_url() {
        let doc = parse(r#"<form method="POSTT"><input name="x" value="1"></form>"#);
        let form = find_form(&doc);
        let sub = build_submission(&doc, form, None).expect("submits");
        assert_eq!(sub.method, FormMethod::Get);
        assert_eq!(sub.url.as_str(), "https://example.test/page?x=1");
    }
}
