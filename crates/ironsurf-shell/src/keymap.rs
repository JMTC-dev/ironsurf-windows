//! Slint keyboard-event translation and browser-chord detection.
//!
//! Slint delivers key events as a single unicode `text` value (control
//! characters for special keys, e.g. `Tab` is `"\t"`, arrows are the
//! private-use code points from the Slint key table). We map those to the
//! DOM `key`/`code` strings the tab hands to Servo, and we detect browser
//! chords (Ctrl-L / F6 / Ctrl-W / Ctrl-R / Ctrl-T / Ctrl-Tab / Esc / Ctrl-+ /
//! Ctrl-- / Ctrl-0 / F12 / Ctrl-I / Ctrl-F) here so the logic stays pure and
//! testable.

use ironsurf_common::{KeyEvent, Modifiers};

/// Browser-level shortcuts intercepted by the shell (never forwarded).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shortcut {
    /// Ctrl-L / F6: focus the URL bar (and highlight its whole text).
    FocusUrl,
    /// Ctrl-W: close the active tab.
    CloseTab,
    /// Ctrl-R: reload the active tab.
    Reload,
    /// Ctrl-T: new tab.
    NewTab,
    /// Ctrl-Tab: cycle to the next tab.
    CycleTab,
    /// Ctrl-`+` (Ctrl-`=`, Ctrl-`+`): zoom the page in one step.
    ZoomIn,
    /// Ctrl-`-`: zoom the page out one step.
    ZoomOut,
    /// Ctrl-`0`: reset zoom to 100%.
    ZoomReset,
    /// F12 / Ctrl-`I`: toggle the DevTools panel of the active tab.
    ToggleDevTools,
    /// Ctrl-`F`: open the find-in-page bar (and focus its text entry).
    Find,
    /// Esc: return keyboard focus to the page.
    PageFocus,
}

/// Browser zoom policy (shell-owned): multiplicative steps, clamped range.
pub const ZOOM_STEP: f32 = 1.1;
pub const ZOOM_MIN: f32 = 0.3;
pub const ZOOM_MAX: f32 = 5.0;

/// Round to whole percent to keep the discrete zoom levels exact under
/// repeated stepping (`1.1 * 1.1` drift would otherwise compound).
#[inline]
fn round_zoom(z: f32) -> f32 {
    (z * 100.0).round() / 100.0
}

/// One zoom-in step (`Ctrl`+`+`), clamped to [`ZOOM_MAX`].
pub fn zoom_in(zoom: f32) -> f32 {
    let base = if zoom.is_finite() && zoom > 0.0 { zoom } else { 1.0 };
    round_zoom(base * ZOOM_STEP).clamp(ZOOM_MIN, ZOOM_MAX)
}

/// One zoom-out step (`Ctrl`+`-`), clamped to [`ZOOM_MIN`].
pub fn zoom_out(zoom: f32) -> f32 {
    let base = if zoom.is_finite() && zoom > 0.0 { zoom } else { 1.0 };
    round_zoom(base / ZOOM_STEP).clamp(ZOOM_MIN, ZOOM_MAX)
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
    // F12 needs no modifier (Slint sends function keys as unicode private-use
    // code points, F1 = U+F704 … F12 = U+F70F — see `i-slint-common`
    // `key_codes.rs`, which the table in `named_key` mirrors). Some backends
    // hand over the Apple-VPK-spaced form instead, so U+F70B is accepted on
    // the same chord: the only cost is F8, which no page we ship needs.
    if matches!(c, '\u{F70F}' | '\u{F70B}') && !alt && !meta {
        return Some(Shortcut::ToggleDevTools);
    }
    // F6 focuses the URL bar with no modifier (classic convention). F6 =
    // U+F704 + 5 = U+F709 in the same private-use table as F12 above.
    if c == '\u{F709}' && !ctrl && !alt && !meta {
        return Some(Shortcut::FocusUrl);
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
        // Ctrl+= and Ctrl+shift+= (i.e. Ctrl-`+`) both zoom in; control-char
        // forms: Ctrl-= = 0x1D (GS), Ctrl-- = 0x1F (US).
        '=' | '+' | '\u{1d}' => Some(Shortcut::ZoomIn),
        '-' | '_' | '\u{1f}' => Some(Shortcut::ZoomOut),
        '0' => Some(Shortcut::ZoomReset),
        // Ctrl-`I` (and Ctrl-Shift-`I`, which arrives as the same code point
        // with shift ignored here). The control-character spelling of this
        // chord is `\t`, which already means Ctrl-Tab, so only the letter form
        // — what Slint actually delivers for KeyI — toggles DevTools.
        'i' | 'I' => Some(Shortcut::ToggleDevTools),
        // Ctrl-`F` opens the find bar. Its control-character spelling is
        // ACK (`\u{06}`), which X11 backends hand over for the same chord.
        'f' | 'F' | '\u{06}' => Some(Shortcut::Find),
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
    fn clipboard_chords_always_reach_the_page() {
        // Copy lives in the page's engine (its selection), and paste is the
        // shell's push-then-chord protocol (`is_paste_chord`) — so none of
        // these may ever be consumed as a browser shortcut, in either the
        // letter or the control-character spelling X hands over.
        for (text, ctrl) in [
            ("c", true),
            ("C", true),
            ("\u{03}", true), // Ctrl-C (ETX)
            ("v", true),
            ("V", true),
            ("\u{16}", true), // Ctrl-V (SYN)
            ("x", true),
            ("\u{18}", true), // Ctrl-X (CAN)
            ("a", true),
            ("\u{01}", true), // Ctrl-A (SOH)
        ] {
            assert_eq!(
                shortcut_for(text, ctrl, false, false),
                None,
                "{text:?} ctrl={ctrl} belongs to the page"
            );
        }
    }

    #[test]
    fn f6_focuses_url_bar_like_ctrl_l() {
        // F6 = U+F704 + 5 = U+F709, unmodified, focuses the URL bar.
        assert_eq!(
            shortcut_for("\u{F709}", false, false, false),
            Some(Shortcut::FocusUrl)
        );
        // Modified F6 combos stay with the window manager / page.
        assert_eq!(shortcut_for("\u{F709}", true, false, false), None);
        assert_eq!(shortcut_for("\u{F709}", false, true, false), None);
        assert_eq!(shortcut_for("\u{F709}", false, false, true), None);
        // Neighbouring function keys are not URL focus.
        assert_eq!(shortcut_for("\u{F708}", false, false, false), None); // F5
        assert_eq!(shortcut_for("\u{F70A}", false, false, false), None); // F7
    }

    #[test]
    fn zoom_chords_are_detected() {
        // Ctrl-= (the usual unshifted form of Ctrl-+) and Ctrl-+ both zoom in.
        assert_eq!(shortcut_for("=", true, false, false), Some(Shortcut::ZoomIn));
        assert_eq!(shortcut_for("+", true, false, false), Some(Shortcut::ZoomIn));
        assert_eq!(
            shortcut_for("\u{1d}", true, false, false),
            Some(Shortcut::ZoomIn)
        );
        assert_eq!(shortcut_for("-", true, false, false), Some(Shortcut::ZoomOut));
        assert_eq!(shortcut_for("_", true, false, false), Some(Shortcut::ZoomOut));
        assert_eq!(
            shortcut_for("\u{1f}", true, false, false),
            Some(Shortcut::ZoomOut)
        );
        assert_eq!(shortcut_for("0", true, false, false), Some(Shortcut::ZoomReset));
    }

    #[test]
    fn devtools_chords_are_detected() {
        // F12 = U+F70F (Slint's own table: F1 = U+F704 + 11).
        assert_eq!(shortcut_for("\u{F70F}", false, false, false), Some(Shortcut::ToggleDevTools));
        // The alternate spelling the task brief reports from the backend.
        assert_eq!(shortcut_for("\u{F70B}", false, false, false), Some(Shortcut::ToggleDevTools));
        // Ctrl-I, and Ctrl-Shift-I (`shortcut_for` takes no shift flag: the
        // shifted form simply arrives as the uppercase letter).
        assert_eq!(shortcut_for("i", true, false, false), Some(Shortcut::ToggleDevTools));
        assert_eq!(shortcut_for("I", true, false, false), Some(Shortcut::ToggleDevTools));
    }

    #[test]
    fn find_chord_is_detected() {
        // Ctrl-F, and the ACK control-character spelling the X11 backend
        // delivers for the same chord.
        assert_eq!(shortcut_for("f", true, false, false), Some(Shortcut::Find));
        assert_eq!(shortcut_for("F", true, false, false), Some(Shortcut::Find));
        assert_eq!(shortcut_for("\u{06}", true, false, false), Some(Shortcut::Find));
        // Bare `f` types into the page (and Alt/Meta-F stays with the WM).
        assert_eq!(shortcut_for("f", false, false, false), None);
        assert_eq!(shortcut_for("f", false, true, false), None);
        assert_eq!(shortcut_for("f", false, false, true), None);
    }

    #[test]
    fn devtools_chords_do_not_steal_other_keys() {
        // Bare `i` types into the page; Ctrl-Tab stays the tab cycle even
        // though it is Ctrl-I's control-character spelling.
        assert_eq!(shortcut_for("i", false, false, false), None);
        assert_eq!(shortcut_for("\t", true, false, false), Some(Shortcut::CycleTab));
        // Alt/Meta function-key combos stay with the window manager.
        assert_eq!(shortcut_for("\u{F70F}", false, true, false), None);
        assert_eq!(shortcut_for("i", true, true, false), None);
        assert_eq!(shortcut_for("i", true, false, true), None);
        // Neighbouring function keys are not DevTools.
        assert_eq!(shortcut_for("\u{F70E}", false, false, false), None);
        assert_eq!(shortcut_for("\u{F70C}", false, false, false), None);
    }

    #[test]
    fn plain_zoom_characters_are_not_chords() {
        assert_eq!(shortcut_for("=", false, false, false), None);
        assert_eq!(shortcut_for("-", false, false, false), None);
        assert_eq!(shortcut_for("0", false, false, false), None);
        // Alt/Meta combinations are window-manager territory, not zoom.
        assert_eq!(shortcut_for("=", true, true, false), None);
        assert_eq!(shortcut_for("-", true, false, true), None);
    }

    #[test]
    fn zoom_steps_are_multiplicative_clamped_whole_percent() {
        assert_eq!(zoom_in(1.0), 1.1);
        assert_eq!(zoom_in(1.1), 1.21);
        assert_eq!(zoom_out(1.0), 0.91); // 1/1.1 = 0.909… → whole percent
        assert_eq!(zoom_out(zoom_in(1.0)).min(1.0), zoom_out(zoom_in(1.0)));
        // Repeated stepping clamps at both ends…
        let mut z = 1.0;
        for _ in 0..40 {
            z = zoom_in(z);
        }
        assert_eq!(z, ZOOM_MAX);
        for _ in 0..80 {
            z = zoom_out(z);
        }
        assert_eq!(z, ZOOM_MIN);
        // …and nonsense input restarts from 100%.
        assert_eq!(zoom_in(f32::NAN), 1.1);
        assert_eq!(zoom_out(0.0), 0.91);
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
