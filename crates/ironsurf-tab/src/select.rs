//! Page-wide text selection — the tab half of `Ctrl+A`.
//!
//! blitz's own Ctrl+A lives in the editor code (`blitz-dom/src/node/text.rs`)
//! and only ever runs for a focused text input. With no editor focused blitz
//! does nothing with the chord, so the tab fills the gap here: **all of the
//! document's laid-out text becomes the selection** — anchor at offset 0 of
//! the first inline root that holds text, focus at the end of the last one.
//!
//! "Laid-out" is exactly the criterion [`crate::find`] matches against, and
//! for the same reason: an inline root's parley `text` is what the user sees,
//! so `display: none` text, `<head>`, `<script>`/`<style>` source and form
//! control values are never selected, while a `<span>`'s text is (merged into
//! its root) and mixed content lives in the anonymous inline roots the DOM
//! never shows. The whole laid-out document counts — text scrolled out of the
//! viewport included, exactly like every real browser's Ctrl+A.
//!
//! The result is blitz's ordinary text selection: it is written with the same
//! [`BaseDocument::set_text_selection`] a mouse drag ends with, so the painted
//! highlight, the context menu's Copy row, and Ctrl+C (`blitz-dom`
//! `events/keyboard.rs` → `ShellProvider::set_clipboard_text` →
//! `FromTab::ClipboardText`) all pick it up with no changes of their own.

use blitz_dom::{BaseDocument, NodeId};

/// Select all laid-out text of the document. Returns false (and leaves any
/// existing selection alone) when the page holds no laid-out text at all —
/// nothing to select, nothing to announce.
///
/// Textual inline roots are visited in reading order via the **layout** tree
/// (depth first from the root element, like `find::collect`): the first root
/// with non-empty text anchors the selection, the last one focuses it.
pub fn select_all(doc: &mut BaseDocument) -> bool {
    let Some(root) = doc.try_root_element() else {
        return false;
    };
    // Iterative, as in `find::collect`: real pages nest deeper than a
    // recursive descent should reasonably assume.
    let mut stack = vec![root.id];
    let mut anchor: Option<NodeId> = None;
    let mut focus = root.id;
    let mut focus_offset = 0usize;
    while let Some(id) = stack.pop() {
        let Some(node) = doc.get_node(id) else { continue };
        if let Some(layout) = node.element_data().and_then(|e| e.inline_layout_data.as_ref()) {
            if !layout.text.is_empty() {
                anchor.get_or_insert(id);
                focus = id;
                focus_offset = layout.text.len();
            }
        }
        let children: Vec<NodeId> = node
            .layout_children
            .borrow()
            .as_ref()
            .map(|kids| kids.iter().copied().collect())
            .unwrap_or_default();
        // Reversed so the first child is visited first (the stack is LIFO).
        stack.extend(children.into_iter().rev());
    }
    let Some(anchor) = anchor else {
        return false;
    };
    // The same call a drag ends with, anonymous-block bookkeeping included
    // (`set_text_selection` stores mixed-content endpoints stably).
    doc.set_text_selection(anchor, 0, focus, focus_offset);
    true
}
