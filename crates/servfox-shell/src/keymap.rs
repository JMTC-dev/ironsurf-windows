//! Slint keyboard-event translation and browser-chord detection.
//!
//! Slint delivers key events as a single unicode `text` value (control
//! characters for special keys, e.g. `Tab` is `"\t"`, arrows are the
//! private-use code points from the Slint key table). We map those to the
//! DOM `key`/`code` strings the tab hands to Servo, and we detect browser
//! chords (Ctrl-L / Ctrl-W / Ctrl-R / Ctrl-T / Ctrl-Tab / Esc) here so the
//! logic stays pure and testable.

use servfox_common::{KeyEvent, Modifiers};

/// Browser-level shortcuts intercepted by the shell (never forwarded).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shortcut {
    /// Ctrl-L: focus the URL bar.
    FocusUrl,
    /// Ctrl-W: close the active tab.
    CloseTab,
    /// Ctrl-R: reload the active tab.
    Reload,
    /// Ctrl-T: new tab.
    NewTab,
    /// Ctrl-Tab: cycle to the next tab.
    CycleTab,
    /// Esc: return keyboard focus to the page.
    PageFocus,
}

#[inline]
pub fn mods(ctrl: bool, shift: bool, alt: bool, meta: bool) -> Modifiers {
    Modifiers::new(
        (ctrl as u8) * Modifiers::CTRL_BIT
            | (shift as u8) * Modifiers::SHIFT_BIT
            | (alt as u8) * Modifiers::ALT_BIT
            | (meta as u8) * Modifiers::META_BIT,
    )
}

/// Detect a browser chord from a key press. `text` is the Slint key-event
/// text; control-character forms (`\u{0C}` for Ctrl-L etc.) are accepted so
/// the detection works regardless of how the backend encoded the chord.
pub fn shortcut_for(text: &str, ctrl: bool, alt: bool, meta: bool) -> Option<Shortcut> {
    let mut chars = text.chars();
    let c = match (chars.next(), chars.next()) {
        (Some(c), None) => c,
        _ => return if text == "Escape" { Some(Shortcut::PageFocus) } else { None },
    };
    if c == '\u{1b}' {
        return Some(Shortcut::PageFocus);
    }
    if !ctrl || alt || meta {
        return None;
    }
    match c {
        'l' | 'L' | '\u{0c}' => Some(Shortcut::FocusUrl),
        'w' | 'W' | '\u{17}' => Some(Shortcut::CloseTab),
        'r' | 'R' | '\u{12}' => Some(Shortcut::Reload),
        't' | 'T' | '\u{14}' => Some(Shortcut::NewTab),
        '\t' => Some(Shortcut::CycleTab),
        _ => None,
    }
}

/// Map Slint key text to DOM `key` + DOM `code` strings.
pub fn dom_key(text: &str) -> (String, String) {
    let c = match text.chars().next() {
        Some(c) if text.chars().count() == 1 => c,
        // Multi-char input (IME) is not representable as one key: send as
        // text with an Unidentified-ish key name; the tab prefers `text`.
        _ => return (text.to_string(), String::new()),
    };
    if let Some((key, code)) = named_key(c) {
        return (key.to_string(), code.to_string());
    }
    let key = text.to_string();
    let code = if c.is_ascii_alphabetic() {
        format!("Key{}", c.to_ascii_uppercase())
    } else if c.is_ascii_digit() {
        format!("Digit{c}")
    } else {
        // punctuation: DOM code for US layout approximates the character
        c.to_string()
    };
    (key, code)
}

/// Slint's special keys arrive as control characters or unicode private-use
/// code points (see `i-slint-common::for_each_keys`).
fn named_key(c: char) -> Option<(&'static str, &'static str)> {
    Some(match c {
        '\u{08}' => ("Backspace", "Backspace"),
        '\t' => ("Tab", "Tab"),
        '\u{19}' => ("Backtab", "Backtab"),
        '\r' | '\n' => ("Enter", "Enter"),
        '\u{1b}' => ("Escape", "Escape"),
        '\u{7f}' => ("Delete", "Delete"),
        ' ' => (" ", "Space"),
        // arrows + navigation (unicode APPLE CORPCHAR private use area)
        '\u{F700}' => ("ArrowUp", "ArrowUp"),
        '\u{F701}' => ("ArrowDown", "ArrowDown"),
        '\u{F702}' => ("ArrowLeft", "ArrowLeft"),
        '\u{F703}' => ("ArrowRight", "ArrowRight"),
        '\u{F727}' => ("Insert", "Insert"),
        '\u{F729}' => ("Home", "Home"),
        '\u{F72B}' => ("End", "End"),
        '\u{F72C}' => ("PageUp", "PageUp"),
        '\u{F72D}' => ("PageDown", "PageDown"),
        _ => {
            // F1..F12 = U+F704..U+F70F, F13..F24 = U+F710..U+F71B
            if matches!(c, '\u{F704}'..='\u{F71B}') {
                let n = (c as u32) - ('\u{F704}' as u32) + 1;
                // (leaks nothing: const string table)
                const F: [&str; 24] = [
                    "F1", "F2", "F3", "F4", "F5", "F6", "F7", "F8", "F9", "F10", "F11", "F12",
                    "F13", "F14", "F15", "F16", "F17", "F18", "F19", "F20", "F21", "F22", "F23",
                    "F24",
                ];
                return Some((F[n as usize - 1], F[n as usize - 1]));
            }
            return None;
        }
    })
}

/// Build the IPC key event for a raw key press/release from the page.
pub fn to_ipc_key(text: &str, ctrl: bool, shift: bool, alt: bool, meta: bool, repeat: bool) -> KeyEvent {
    let (key, code) = dom_key(text);
    KeyEvent {
        key,
        code,
        text: if text.is_empty() { None } else { Some(text.to_string()) },
        repeat,
        mods: mods(ctrl, shift, alt, meta),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chords_are_detected_in_letter_and_control_char_forms() {
        assert_eq!(
            shortcut_for("l", true, false, false),
            Some(Shortcut::FocusUrl)
        );
        assert_eq!(
            shortcut_for("\u{0c}", true, false, false),
            Some(Shortcut::FocusUrl)
        );
        assert_eq!(
            shortcut_for("w", true, false, false),
            Some(Shortcut::CloseTab)
        );
        assert_eq!(
            shortcut_for("\u{17}", true, false, false),
            Some(Shortcut::CloseTab)
        );
        assert_eq!(shortcut_for("r", true, false, false), Some(Shortcut::Reload));
        assert_eq!(shortcut_for("t", true, false, false), Some(Shortcut::NewTab));
        assert_eq!(
            shortcut_for("\t", true, false, false),
            Some(Shortcut::CycleTab)
        );
        assert_eq!(
            shortcut_for("\u{1b}", false, false, false),
            Some(Shortcut::PageFocus)
        );
    }

    #[test]
    fn plain_text_is_not_a_chord() {
        assert_eq!(shortcut_for("a", false, false, false), None);
        assert_eq!(shortcut_for("\t", false, false, false), None); // bare Tab
        assert_eq!(shortcut_for("l", false, false, false), None);
        // Alt/Meta chords are not browser chords (window manager etc.)
        assert_eq!(shortcut_for("l", true, true, false), None);
        assert_eq!(shortcut_for("w", true, false, true), None);
    }

    #[test]
    fn page_down_key_maps_to_dom_names() {
        assert_eq!(dom_key("\u{F72D}"), ("PageDown".into(), "PageDown".into()));
        assert_eq!(dom_key("\u{F702}"), ("ArrowLeft".into(), "ArrowLeft".into()));
        assert_eq!(dom_key("\r"), ("Enter".into(), "Enter".into()));
        assert_eq!(dom_key("\u{7f}"), ("Delete".into(), "Delete".into()));
        assert_eq!(dom_key("\u{F704}"), ("F1".into(), "F1".into()));
        assert_eq!(dom_key("\u{F71B}"), ("F24".into(), "F24".into()));
    }

    #[test]
    fn printable_keys_get_key_and_code() {
        assert_eq!(dom_key("a"), ("a".into(), "KeyA".into()));
        assert_eq!(dom_key("Z"), ("Z".into(), "KeyZ".into()));
        assert_eq!(dom_key("7"), ("7".into(), "Digit7".into()));
        assert_eq!(dom_key(" "), (" ".into(), "Space".into()));
    }

    #[test]
    fn ipc_key_event_carries_text_and_modifiers() {
        let ev = to_ipc_key("a", true, false, true, false, true);
        assert_eq!(ev.key, "a");
        assert_eq!(ev.code, "KeyA");
        assert_eq!(ev.text.as_deref(), Some("a"));
        assert!(ev.repeat);
        assert!(ev.mods.ctrl());
        assert!(ev.mods.alt());
        assert!(!ev.mods.shift());
        assert!(!ev.mods.meta());
    }
}
