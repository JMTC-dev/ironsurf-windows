//! Slint keyboard-event translation and browser-chord detection.
//!
//! Slint delivers key events as a single unicode `text` value (control
//! characters for special keys, e.g. `Tab` is `"\t"`, arrows are the
//! private-use code points from the Slint key table). We map those to the
//! DOM `key`/`code` strings the tab hands to Servo, and we detect browser
//! chords (Ctrl-L / F6 / Ctrl-W / Ctrl-R / Ctrl-T / Ctrl-Tab / Alt-1…Alt-9 /
//! Esc / Ctrl-+ / Ctrl-- / Ctrl-0 / F12 / Ctrl-I / Ctrl-F / Ctrl-Alt-T /
//! Ctrl-Alt-H) here so the logic stays pure and testable.
//!
//! macOS spells every one of those chords with **Command** instead of Ctrl
//! (and ⌘-1…⌘-9 instead of Alt-1…Alt-9): see [`Chords`], which folds the
//! platform's chord key into one flag before any chord is matched.

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
    /// Alt-`1` … Alt-`9` (`⌘1` … `⌘9` on macOS, see [`Chords`]): switch to the
    /// N-th tab. The digit is reported verbatim — **which tab it lands on is
    /// resolved at press time** by [`tab_index_for_digit`] against the live tab
    /// count, so the mapping follows every open and close (digit `9` is "the
    /// last tab" from nine tabs on, exactly like Chrome/Firefox).
    SelectTab(u8),
    /// `Alt-←` / `Alt-→` (`⌘-←` / `⌘-→` on macOS, see [`Chords`]): back / forward
    /// in the active tab's history. History and not the tab strip — the arrows
    /// say which way the reader wants to go, and the strip already has Ctrl-Tab
    /// and the digit chords. `↑`/`↓` and PageUp/PageDown are **not** here: they
    /// scroll the page, and the tab does that, so no chord may claim them.
    GoBack,
    GoForward,
    /// Ctrl-`+` (Ctrl-`=`, Ctrl-`+`): zoom the page in one step.
    ZoomIn,
    /// Ctrl-`-`: zoom the page out one step.
    ZoomOut,
    /// Ctrl-`0`: reset zoom to 100%.
    ZoomReset,
    /// F12 / Ctrl-`I`: toggle the DevTools panel of the active tab.
    ToggleDevTools,
    /// Ctrl-Shift-`C`: arm *inspect element* (the DevTools ⌖ picker) on the
    /// active tab, opening the panel and its Inspector pane when either is
    /// closed — and disarming when it is already armed. The shifted spelling
    /// of the chord is the only one `shortcut_for` can see (it takes no shift
    /// flag), which is why plain Ctrl-`C` copy keeps both of its own spellings
    /// (`c`, `\u{03}`) and reaches the page.
    Inspector,
    /// Ctrl-`F`: open the find-in-page bar (and focus its text entry).
    Find,
    /// Ctrl-Alt-`T`: reopen the last closed tab (task: browsing history). The
    /// stack it pops is the shell's own — each close pushes URL + container,
    /// each press pops the newest.
    ReopenClosedTab,
    /// Ctrl-Alt-`R`: toggle **reader view** on the active tab (task: reader
    /// mode) — the same command as the 📄 toolbar button. The tab answers
    /// with `FromTab::ReaderState`, which moves the button.
    ToggleReader,
    /// Ctrl-Alt-`P`: "allow popups from this site" (task: block popups) — puts
    /// the page's own origin on the active tab's allow-list and re-sends the
    /// policy, so it works on the page already on screen. The popup blocker
    /// itself is the ⛔ toolbar button; this is the one thing a user wants after
    /// the status line says a popup was blocked.
    PopupAllowSite,
    /// Ctrl-Alt-`H`: toggle the browsing-History menu (task: browsing
    /// history) — the same menu as the ☰ toolbar button, and the only place
    /// the shared history log is listed and cleared.
    HistoryMenu,
    /// Ctrl-Shift-`D`: toggle the ⬇ downloads menu (task: file downloads) —
    /// the same overlay as the ⬇ toolbar button. Only the shifted glyph is
    /// mapped: a plain Ctrl-`D` stays the page's own (bookmark, in every
    /// browser that still has one), exactly as Ctrl-`C` stays copy.
    DownloadsMenu,
    /// Ctrl-`S`: the "Save page" dialog (task: save-page selector) — pick the
    /// folder the current page is written to (default `~/Downloads`) and its
    /// name; the bytes then land through the ⬇ panel like any other save.
    /// Unlike Ctrl-`D`, the plain chord is the browser's (every browser saves
    /// with it), and its X11 control-character form (`\u{13}`) binds too.
    SavePage,
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

/// Highest digit a tab-number chord can use (`Alt-1` … `Alt-9`).
pub const ALT_TAB_DIGITS: u8 = 9;

/// Which key carries the browser's chords: **`Ctrl`** on Linux and Windows,
/// **`Command` (⌘)** on macOS.
///
/// The whole browser speaks one chord language ([`shortcut_for`]); this type is
/// the single place where that language is spelled for a platform, so a macOS
/// build gets ⌘-everything without a `cfg!` in every chord arm — and the mac
/// spelling stays testable on Linux (see [`shortcut_on`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Chords {
    /// `Ctrl-L`, `Alt-1` …: the Linux/Windows spelling.
    Ctrl,
    /// `⌘L`, `⌘1` …: the macOS spelling.
    Cmd,
}

impl Chords {
    /// The chord spelling of the platform this binary runs on.
    pub const CURRENT: Chords =
        if cfg!(target_os = "macos") { Chords::Cmd } else { Chords::Ctrl };

    /// Is the chord modifier down, given the raw `ctrl` / `meta` flags from the
    /// toolkit?
    ///
    /// On macOS the answer is *either* flag: Slint swaps the two keys in
    /// `KeyboardModifiers` ("on macOS, Slint maps the Command key to the
    /// control modifier, and the Control key to the meta modifier") so that
    /// cross-platform `@keys(Control-…)` bindings read as ⌘, while backends
    /// that do not swap — and the tab IPC — keep Command in `meta`. Taking both
    /// makes one chord work whichever way the window backend spelled it, and
    /// the Control key answering a ⌘ chord on a Mac costs nothing.
    #[inline]
    pub fn held(self, ctrl: bool, meta: bool) -> bool {
        match self {
            Chords::Ctrl => ctrl,
            Chords::Cmd => ctrl || meta,
        }
    }

    /// Does `Alt` still own the *navigation* chords — the tab-number chords
    /// (`Alt-1` … `Alt-9`) and `←`/`→` for history? Yes on Linux/Windows, no on
    /// macOS: Option is a *typing* key there (`Option-2` is `™`, `Option-←` moves
    /// a word) and ⌘ runs the tab strip and the history instead.
    #[inline]
    pub fn digits_on_alt(self) -> bool {
        matches!(self, Chords::Ctrl)
    }

    /// Human name of the modifier [`Self::digits_on_alt`] names — for logs and
    /// for any hint the UI paints next to a tab's badge.
    #[inline]
    pub fn digit_mod_label(self) -> &'static str {
        if self.digits_on_alt() {
            "Alt"
        } else {
            "\u{2318}"
        }
    }
}

/// Digit of a tab-number chord, from the character Slint hands over.
///
/// Plain digits (`1` … `9`) and the two spellings X11 backends deliver for the
/// same physical keys — the meta-escaped codes `0xB1` … `0xB9` (digit | 0x80)
/// and the shifted glyphs `!` … `(` (`Alt-Shift-1`, which `shortcut_for` cannot
/// tell apart since it takes no shift flag).
fn alt_tab_digit(c: char) -> Option<u8> {
    match c {
        '1'..='9' => Some(c as u8 - b'0'),
        '\u{b1}'..='\u{b9}' => Some(c as u8 - 0xb0), // Alt-1 … Alt-9 (meta-escaped)
        '!'..='(' => Some(c as u8 - 0x20),           // Alt-Shift-1 … Alt-Shift-9
        _ => None,
    }
}

/// Slint's key text for `←` and `→`: private-use keysyms, one character each
/// (`i-slint-common::key_codes`, mirrored by the table in [`dom_key`]).
const KEY_ARROW_LEFT: char = '\u{F702}';
const KEY_ARROW_RIGHT: char = '\u{F703}';

/// Which tab does the digit chord (`Alt-{digit}`, `⌘{digit}` on macOS) select
/// in a strip of `tab_count` tabs?
///
/// `1` … `8` address the first eight positions literally; `9` selects the
/// **last** tab once there are nine or more. `None` means the chord reaches no
/// tab (the position does not exist) and the keystroke is dropped rather than
/// guessed at.
///
/// This is the whole dynamic mapping: callers resolve it on every press, so a
/// tab closing shifts every later shortcut onto a different tab by itself.
pub fn tab_index_for_digit(digit: u8, tab_count: usize) -> Option<usize> {
    if digit == 0 || digit > ALT_TAB_DIGITS || tab_count == 0 {
        return None;
    }
    if digit == ALT_TAB_DIGITS && tab_count >= ALT_TAB_DIGITS as usize {
        return Some(tab_count - 1); // Alt-9 = last tab
    }
    let idx = (digit - 1) as usize;
    (idx < tab_count).then_some(idx)
}

/// Digit of the Alt-chord that reaches the tab at `index` of `tab_count`
/// tabs — the inverse of [`tab_index_for_digit`], used to paint the mapping
/// onto the tab strip so it stays visible as tabs come and go.
pub fn tab_shortcut_digit(index: usize, tab_count: usize) -> Option<u8> {
    if index >= tab_count {
        return None;
    }
    if index + 1 >= ALT_TAB_DIGITS as usize {
        // Positions 9 and beyond share Alt-9, which lands on the last tab only.
        return (index + 1 == tab_count).then_some(ALT_TAB_DIGITS);
    }
    Some((index + 1) as u8)
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

/// Detect a browser chord from a key press, spelled for this platform (see
/// [`Chords::CURRENT`]). `text` is the Slint key-event text; control-character
/// forms (`\u{0C}` for Ctrl-L etc.) are accepted so the detection works
/// regardless of how the backend encoded the chord.
pub fn shortcut_for(text: &str, ctrl: bool, alt: bool, meta: bool) -> Option<Shortcut> {
    shortcut_on(Chords::CURRENT, text, ctrl, alt, meta)
}

/// The browser chords that stay live while the URL/find bar holds focus
/// (task: keep keyboard shortcuts usable while the URL bar is focused).
/// The bars keep everything local — plain typing, caret motion, and their own
/// editing chords (Ctrl-A/C/V/X/Y/Z are not in [`shortcut_for`] at all, so
/// select-all, copy and paste keep working mid-URL) — while the tabled
/// browser chords fire as usual: Ctrl-T new tab, the tab-number digits,
/// Ctrl-Tab, Ctrl-W, Ctrl-R, the zoom chords, Ctrl-F, F12, history and the
/// menus. The one exception is Esc ([`Shortcut::PageFocus`]): inside a bar it
/// belongs to the bar (it closes the completion dropdown), not to the page.
pub fn bar_shortcut(text: &str, ctrl: bool, alt: bool, meta: bool) -> Option<Shortcut> {
    bar_shortcut_on(Chords::CURRENT, text, ctrl, alt, meta)
}

/// [`bar_shortcut`] with the platform's chord spelling chosen by the caller
/// (parity with [`shortcut_on`]).
pub fn bar_shortcut_on(
    chords: Chords,
    text: &str,
    ctrl: bool,
    alt: bool,
    meta: bool,
) -> Option<Shortcut> {
    match shortcut_on(chords, text, ctrl, alt, meta) {
        // Esc belongs to the bar (completion dropdown); the history arrows
        // belong to the caret — on macOS ⌘-← is “home”, and a bar that ate it
        // would break the one promise the history chords make to text
        // fields (see the history-arrow bullet in ARCHITECTURE.md).
        Some(Shortcut::PageFocus) | Some(Shortcut::GoBack) | Some(Shortcut::GoForward) | None => {
            None
        }
        Some(sc) => Some(sc),
    }
}

/// Is the platform's chord key held *alone* — the shape every chord below
/// insists on? Linux/Windows: Control without Meta. macOS: Command (which a
/// backend may report in either flag, so either spelling alone counts) or the
/// bare unix-Control habit, but never both at once — a press holding Ctrl
/// *and* ⌘ is somebody else's chord (macOS test failures: Ctrl-⌘-W closed a
/// tab it should have left alone).
fn platform_key_alone(chords: Chords, ctrl: bool, meta: bool) -> bool {
    if chords == Chords::Cmd {
        (ctrl || meta) && !(ctrl && meta)
    } else {
        ctrl && !meta
    }
}

/// [`shortcut_for`] with the platform's chord spelling chosen by the caller —
/// `Chords::Cmd` is how the macOS chords get tested on a Linux box.
pub fn shortcut_on(
    chords: Chords,
    text: &str,
    ctrl: bool,
    alt: bool,
    meta: bool,
) -> Option<Shortcut> {
    // A bare mac modifier tap is an echo, not a chord:
    // i-slint-backend-winit swaps the logical Control key for Super
    // (`swap_cmd_ctrl` in its event_loop.rs), and i-slint-common's
    // `key_codes.rs` reports Super as its *control character* `"\u{17}"` —
    // which is precisely the Linux spelling of Ctrl+W. Without this guard a
    // simple Ctrl tap closes a tab on macOS (and its swap-side Control code
    // `"\u{11}"` — plus both right-hand forms — echoes off the same table).
    // The Cmd spelling takes Command chords as plain letters, never as these
    // bytes, so refusing them here costs nothing.
    if chords == Chords::Cmd && matches!(text, "\u{11}" | "\u{16}" | "\u{17}" | "\u{18}") {
        return None;
    }
    // Keep the raw press: the fold below deliberately blurs Ctrl and ⌘ on
    // macOS, and two arms must tell them apart again — a press holding BOTH
    // is somebody else's chord (Ctrl-⌘-W closes nothing on either platform,
    // while bare Ctrl-W — the unix habit — and bare ⌘-W both do).
    let raw_ctrl = ctrl;
    let raw_meta = meta;
    // Fold the platform's chord key into `ctrl`. On macOS `meta` *is* that key
    // (`Chords::held`), so it must stop meaning "somebody else's chord" there.
    let ctrl = chords.held(ctrl, meta);
    let meta = if chords == Chords::Cmd { false } else { meta };
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
    // Ctrl-Alt-`T` / Ctrl-Alt-`H` / Ctrl-Alt-`R` (tasks: browsing history,
    // reader mode): reopen the last closed tab, toggle the History menu, and
    // toggle reader view. Caught ahead of the plain-Ctrl letter chords below,
    // so plain Ctrl-`T` (new tab) and Ctrl-`R` (reload) are untouched; both
    // spellings X11 backends hand over (letter, control char) are accepted.
    if alt && platform_key_alone(chords, raw_ctrl, raw_meta) {
        return match c {
            't' | 'T' | '\u{14}' => Some(Shortcut::ReopenClosedTab),
            'h' | 'H' => Some(Shortcut::HistoryMenu),
            'r' | 'R' => Some(Shortcut::ToggleReader),
            // popups (task: block popups): Ctrl-Alt-`P` says "this site may
            // open windows anyway" — the thing to do after the status line
            // reports a blocked popup. Raising/lowering the blocker itself is
            // the ⛔ button, which states the switch and flips it.
            'p' | 'P' | '\u{10}' => Some(Shortcut::PopupAllowSite),
            _ => None,
        };
    }
    // The platform's chord key held *alone*: `Alt` off a Mac, `⌘` on one — and
    // `Chords` has already folded Command into `ctrl`, which is why one expression
    // answers both spellings. It owns the history arrows here and the tab-number
    // chords below.
    let chord = if chords.digits_on_alt() {
        alt && !ctrl && !meta
    } else {
        !alt && platform_key_alone(chords, raw_ctrl, raw_meta)
    };
    // History on the arrows — and only on the plain chord of the platform's own
    // key: `Ctrl-←` off a Mac and `⌥-←` on one keep moving a text caret a word at a
    // time, so those presses belong to the page.
    if chord {
        match c {
            KEY_ARROW_LEFT => return Some(Shortcut::GoBack),
            KEY_ARROW_RIGHT => return Some(Shortcut::GoForward),
            _ => {}
        }
    }
    // The digit chords: switch to the N-th tab — `Alt-1` … `Alt-9` where Ctrl is
    // the chord key, `⌘1` … `⌘9` on macOS. Resolution against the live tab list
    // happens in the shell, so the chord stays valid as tabs open and close, and
    // a digit that is not a digit (⌘0 stays zoom reset, see below) falls
    // through to the letter chords rather than being swallowed here.
    if chord {
        if let Some(digit) = alt_tab_digit(c) {
            return Some(Shortcut::SelectTab(digit));
        }
    }
    if alt || meta || !platform_key_alone(chords, raw_ctrl, raw_meta) {
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
        // Copy lives above this line for ever: `c` and `\u{03}` (ETX) stay
        // unmapped so a plain Ctrl-`C` always reaches the page's own copy. Only
        // the shifted glyph — what Ctrl-Shift-`C` arrives as — arms the picker.
        'C' => Some(Shortcut::Inspector),
        // …and the same rule for Ctrl-Shift-`D` (task: file downloads): only
        // the shifted glyph opens the ⬇ panel, so a plain Ctrl-`D` (`\u{04}`)
        // still reaches the page.
        'D' => Some(Shortcut::DownloadsMenu),
        // Ctrl-`S` (task: save-page selector): unlike Ctrl-`D`, "save" is the
        // browser's own chord everywhere, so the plain chord opens the dialog
        // — both spellings backends hand over (letter, DC3 control char).
        's' | 'S' | '\u{13}' => Some(Shortcut::SavePage),
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

    // macOS spells every browser chord with Command instead of Ctrl, and moves
    // the tab strip from Alt-digits to ⌘-digits. `Chords::Cmd` is passed
    // explicitly so all of this runs on a Linux box too.
    #[test]
    fn mac_spells_every_chord_with_cmd() {
        // Both spellings of Command the window backend may report: `ctrl` (how
        // Slint hands it over on Apple platforms) and `meta` (how the tab IPC
        // and non-swapping backends do).
        for (ctrl, meta) in [(true, false), (false, true)] {
            for (text, want) in [
                ("l", Shortcut::FocusUrl),
                ("w", Shortcut::CloseTab),
                ("r", Shortcut::Reload),
                ("t", Shortcut::NewTab),
                ("i", Shortcut::ToggleDevTools),
                ("f", Shortcut::Find),
                ("s", Shortcut::SavePage),
                ("\u{13}", Shortcut::SavePage),
                ("C", Shortcut::Inspector),
                ("D", Shortcut::DownloadsMenu),
                ("=", Shortcut::ZoomIn),
                ("-", Shortcut::ZoomOut),
                ("0", Shortcut::ZoomReset),
                ("\t", Shortcut::CycleTab),
            ] {
                assert_eq!(
                    shortcut_on(Chords::Cmd, text, ctrl, false, meta),
                    Some(want),
                    "⌘-{text:?} ctrl={ctrl} meta={meta}"
                );
            }
            // Ctrl-Alt-… chords read the same as ⌘-Option-… (Slint keeps Option
            // in `alt` on every platform).
            assert_eq!(shortcut_on(Chords::Cmd, "h", ctrl, true, meta), Some(Shortcut::HistoryMenu));
            assert_eq!(shortcut_on(Chords::Cmd, "t", ctrl, true, meta), Some(Shortcut::ReopenClosedTab));
            assert_eq!(shortcut_on(Chords::Cmd, "r", ctrl, true, meta), Some(Shortcut::ToggleReader));
        }
        // Nothing typed on its own is a chord, and Option alone is not either.
        for text in ["l", "t", "1", "0"] {
            assert_eq!(shortcut_on(Chords::Cmd, text, false, false, false), None);
            assert_eq!(shortcut_on(Chords::Cmd, text, false, true, false), None);
        }
    }

    #[test]
    fn ctrl_and_cmd_together_are_nobody_s_chord() {
        // macOS-host test failures taught this one: with Command folded into
        // the chord key, a press holding Ctrl *and* ⌘ used to fire the plain
        // chord — Ctrl-⌘-W closed a tab, Ctrl-⌘-F searched, Ctrl-⌘-Alt-T
        // reopened, Ctrl-⌘-- zoomed. They belong to somebody else now.
        for text in ["w", "f", "i", "t", "r", "-", "=", "0"] {
            assert_eq!(shortcut_on(Chords::Cmd, text, true, false, true), None, "Ctrl-Cmd-{text}");
        }
        assert_eq!(shortcut_on(Chords::Cmd, "t", true, true, true), None, "Ctrl-Cmd-Alt-T");
        // Each key alone still chords — in whichever flag the backend
        // reports Command, and bare unix Control keeps the habit alive.
        for (ctrl, meta) in [(true, false), (false, true)] {
            assert_eq!(shortcut_on(Chords::Cmd, "w", ctrl, false, meta), Some(Shortcut::CloseTab));
        }
        // And the same holds for the bar-facing table.
        assert_eq!(bar_shortcut_on(Chords::Cmd, "w", true, false, true), None);
        assert_eq!(bar_shortcut_on(Chords::Cmd, "w", false, false, true), Some(Shortcut::CloseTab));
    }

    /// macOS bug: pressing the Ctrl key closed tabs. Slint's winit backend
    /// swaps the logical Control key for Super (`swap_cmd_ctrl`), and the key
    /// table reports Super as its control character `"\u{17}"` — the very
    /// byte the Linux table uses for Ctrl+W. A bare modifier tap must fire
    /// nothing, whichever flags the backend reports the key under; Linux keeps
    /// its Ctrl+W spelling untouched.
    #[test]
    fn mac_modifier_taps_echo_nothing_but_linux_ctrl_w_closes() {
        for (text, ctrl, meta) in [
            ("\u{17}", true, false),  // Ctrl — flag-folded backend
            ("\u{17}", false, true),  // Ctrl — mac meta spelling
            ("\u{17}", true, true),   // Ctrl — both-flag backend
            ("\u{18}", true, false),  // right Ctrl
            ("\u{11}", true, false),  // bare ⌘ (the swap-side Control code)
            ("\u{16}", true, false),  // right ⌘
        ] {
            assert_eq!(shortcut_on(Chords::Cmd, text, ctrl, false, meta), None,
                "modifier echo {text:?} ctrl={ctrl} meta={meta} fired a chord");
            assert_eq!(bar_shortcut_on(Chords::Cmd, text, ctrl, false, meta), None);
        }
        // Off the Mac these bytes stay the ordinary Ctrl+W spelling…
        assert_eq!(shortcut_on(Chords::Ctrl, "\u{17}", true, false, false), Some(Shortcut::CloseTab));
        // …and the mac letters are untouched: ⌘-W still closes, as does the
        // unix Ctrl-W habit.
        assert_eq!(shortcut_on(Chords::Cmd, "w", true, false, false), Some(Shortcut::CloseTab));
    }

    #[test]
    fn mac_tab_digits_are_cmds_not_alts() {
        for d in '1'..='9' {
            let digit = d as u8 - b'0';
            assert_eq!(
                shortcut_on(Chords::Cmd, d.to_string().as_str(), true, false, false),
                Some(Shortcut::SelectTab(digit)),
                "⌘-{d}"
            );
            // …and Option-1 … Option-9 are the page's (they type ¡ ™ etc.).
            assert_eq!(shortcut_on(Chords::Cmd, d.to_string().as_str(), false, true, false), None);
            // Off the Mac, the same digit keys keep their old owners exactly.
            assert_eq!(
                shortcut_on(Chords::Ctrl, d.to_string().as_str(), false, true, false),
                Some(Shortcut::SelectTab(digit))
            );
            assert_eq!(shortcut_on(Chords::Ctrl, d.to_string().as_str(), true, false, false), None);
        }
        // ⌘0 is still "reset zoom": the digit chord does not swallow a digit it
        // cannot use.
        assert_eq!(shortcut_on(Chords::Cmd, "0", true, false, false), Some(Shortcut::ZoomReset));
        // The labels the shell logs / hints with follow the same rule.
        assert_eq!(Chords::Cmd.digit_mod_label(), "\u{2318}");
        assert_eq!(Chords::Ctrl.digit_mod_label(), "Alt");
        assert!(Chords::Cmd != Chords::Ctrl);
        // The binary's own spelling is the platform's.
        assert_eq!(
            shortcut_for("t", true, false, false),
            shortcut_on(Chords::CURRENT, "t", true, false, false)
        );
    }

    /// Back / forward on the arrows: `Alt-←` / `Alt-→` on Linux/Windows, `⌘-←` /
    /// `⌘-→` on macOS — one rule, because `Chords` folds the platform's chord key
    /// before anything is matched. PageUp/PageDown are bound to *nothing* here on
    /// purpose: they scroll the page, which the tab does itself (modifier held or
    /// not), so no chord may claim them.
    #[test]
    fn the_arrows_are_history_and_paging_is_no_chord() {
        let (left, right, up, down) = ("\u{F702}", "\u{F703}", "\u{F72C}", "\u{F72D}");
        // Linux/Windows: Alt owns the arrows…
        assert_eq!(
            shortcut_on(Chords::Ctrl, left, false, true, false),
            Some(Shortcut::GoBack)
        );
        assert_eq!(
            shortcut_on(Chords::Ctrl, right, false, true, false),
            Some(Shortcut::GoForward)
        );
        // …while Ctrl-← (a word at a time in a field) and the same arrow under the
        // Windows key stay the page's.
        assert_eq!(shortcut_on(Chords::Ctrl, left, true, false, false), None);
        assert_eq!(shortcut_on(Chords::Ctrl, right, false, false, true), None);
        // macOS: ⌘ owns them, in whichever of the two flags the backend reported
        // Command, and `⌥-←` keeps moving words instead.
        for (ctrl, meta) in [(true, false), (false, true)] {
            assert_eq!(
                shortcut_on(Chords::Cmd, left, ctrl, false, meta),
                Some(Shortcut::GoBack),
                "\u{2318}-left ctrl={ctrl} meta={meta}"
            );
            assert_eq!(
                shortcut_on(Chords::Cmd, right, ctrl, false, meta),
                Some(Shortcut::GoForward)
            );
        }
        assert_eq!(shortcut_on(Chords::Cmd, left, false, true, false), None);
        // Paging is no chord on either spelling, bare or with a modifier held — and
        // neither are `↑` / `↓`, which are the page's to scroll a line at a time:
        // the tab does that, watching the scroll offset (its own tests).
        for text in [up, down, "\u{F700}", "\u{F701}"] {
            for chords in [Chords::Ctrl, Chords::Cmd] {
                for (ctrl, alt, meta) in [(false, false, false), (true, false, false), (false, true, false)] {
                    assert_eq!(
                        shortcut_on(chords, text, ctrl, alt, meta),
                        None,
                        "{text:?} {chords:?} ctrl={ctrl} alt={alt} meta={meta}"
                    );
                }
            }
        }
    }

    // Ctrl-Alt-`R` (task: reader mode): the reader toggle. Plain Ctrl-`R`
    // must keep meaning reload — including in its control-char spelling —
    // and Ctrl-Alt-`R` arrives as the letter (the control-char form is
    // Ctrl-`R`'s alone, so it is deliberately not claimed here).
    #[test]
    fn ctrl_alt_r_toggles_reader() {
        assert_eq!(shortcut_for("r", true, true, false), Some(Shortcut::ToggleReader));
        assert_eq!(shortcut_for("R", true, true, false), Some(Shortcut::ToggleReader));
        // popups (task: block popups): Ctrl-Alt-`P` allows the current site,
        // both spellings backends hand over; plain Ctrl-`P` and Alt-`P` stay
        // somebody else's, and the reader/History chords are untouched.
        assert_eq!(shortcut_for("p", true, true, false), Some(Shortcut::PopupAllowSite));
        assert_eq!(shortcut_for("P", true, true, false), Some(Shortcut::PopupAllowSite));
        assert_eq!(shortcut_for("\u{10}", true, true, false), Some(Shortcut::PopupAllowSite));
        assert_eq!(shortcut_for("p", true, false, false), None);
        assert_eq!(shortcut_for("p", false, true, false), None);
        assert_eq!(shortcut_for("t", true, true, false), Some(Shortcut::ReopenClosedTab));
        assert_eq!(shortcut_for("r", true, false, false), Some(Shortcut::Reload));
        assert_eq!(shortcut_for("\u{12}", true, false, false), Some(Shortcut::Reload));
    }

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
        // The control-character spellings are named, not trusted from the host:
        // ETX-form Ctrl-W arrives as `\u{17}` on Linux/Windows, while on macOS
        // that very byte is what a bare ⌘ tap reports (see the modifier-echo
        // tests) and is therefore never a chord there. The mac form of the same
        // chord — `⌘W` as a plain letter — is asserted below.
        assert_eq!(
            shortcut_on(Chords::Ctrl, "\u{17}", true, false, false),
            Some(Shortcut::CloseTab)
        );
        assert_eq!(
            shortcut_on(Chords::Cmd, "\u{17}", true, false, false),
            None,
            "on macOS that byte is a modifier tap, not Ctrl-W"
        );
        assert_eq!(
            shortcut_for("w", true, false, false),
            Some(Shortcut::CloseTab),
            "the letter form is the one spelling macOS delivers"
        );
        assert_eq!(shortcut_for("r", true, false, false), Some(Shortcut::Reload));
        assert_eq!(shortcut_for("t", true, false, false), Some(Shortcut::NewTab));
        // browsing history (task): Ctrl-Alt-T reopens the last closed tab,
        // Ctrl-Alt-H toggles the History menu, both spellings each backend
        // delivers; plain Ctrl-T keeps making a new tab.
        assert_eq!(shortcut_for("t", true, true, false), Some(Shortcut::ReopenClosedTab));
        assert_eq!(shortcut_for("\u{14}", true, true, false), Some(Shortcut::ReopenClosedTab));
        assert_eq!(shortcut_for("h", true, true, false), Some(Shortcut::HistoryMenu));
        assert_eq!(shortcut_for("H", true, true, false), Some(Shortcut::HistoryMenu));
        // Ctrl-Shift-D opens the downloads panel; plain Ctrl-D (and its
        // control character) stays the page's.
        assert_eq!(shortcut_for("D", true, false, false), Some(Shortcut::DownloadsMenu));
        assert_eq!(shortcut_for("d", true, false, false), None);
        // Ctrl-S (task: save-page selector) opens the "Save page" dialog —
        // both spellings backends hand over (letter, DC3 control char); the
        // Ctrl-Alt-S relative stays the page's, like other Ctrl-Alt chords.
        assert_eq!(shortcut_for("s", true, false, false), Some(Shortcut::SavePage));
        assert_eq!(shortcut_for("S", true, false, false), Some(Shortcut::SavePage));
        assert_eq!(shortcut_for("\u{13}", true, false, false), Some(Shortcut::SavePage));
        assert_eq!(shortcut_for("s", true, true, false), None);
        assert_eq!(shortcut_for("\u{04}", true, false, false), None);
        // \u{08} (Backspace, and Alt-Backspace's word-delete) belongs to the
        // text being edited, never to the History chord.
        assert_eq!(shortcut_for("\u{08}", true, true, false), None);
        // Meta spellings and bare Alt stay with the window manager / page.
        assert_eq!(shortcut_for("t", true, true, true), None);
        assert_eq!(shortcut_for("t", false, true, false), None);
        // Alt-T is not a tab digit either (see the Alt-digit chord tests).
        assert_eq!(shortcut_for("a", true, true, false), None);
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
    fn inspect_element_is_ctrl_shift_c_and_copy_wins_the_rest() {
        // Ctrl-Shift-`C` arms *inspect element*; the two spellings a plain
        // Ctrl-`C` arrives in stay with the page, so copy keeps working.
        assert_eq!(shortcut_for("C", true, false, false), Some(Shortcut::Inspector));
        assert_eq!(shortcut_for("c", true, false, false), None);
        assert_eq!(shortcut_for("\u{03}", true, false, false), None, "Ctrl-C (ETX) is copy");
        // Unmodified and Alt/Meta spellings belong to the page.
        assert_eq!(shortcut_for("C", false, false, false), None);
        assert_eq!(shortcut_for("C", false, true, false), None);
        assert_eq!(shortcut_for("C", true, true, false), None);
    }

    #[test]
    fn clipboard_chords_always_reach_the_page() {
        // Copy lives in the page's engine (its selection), and paste is the
        // shell's push-then-chord protocol (`is_paste_chord`) — so none of
        // these may ever be consumed as a browser shortcut, in either the
        // letter or the control-character spelling X hands over.
        for (text, ctrl) in [
            ("c", true),
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
        // Bare `f` types into the page, and Alt/Meta-F stays with the WM —
        // in the Ctrl spelling, which the host platform may well not be
        // (macOS spells the very chord ⌘F, and there it *is* Find).
        assert_eq!(shortcut_for("f", false, false, false), None);
        assert_eq!(shortcut_on(Chords::Ctrl, "f", false, true, false), None);
        assert_eq!(shortcut_on(Chords::Ctrl, "f", false, false, true), None);
        assert_eq!(shortcut_on(Chords::Cmd, "f", false, false, true), Some(Shortcut::Find));
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
    fn alt_digit_chords_select_tabs_in_either_x11_spelling() {
        // The X11 spelling puts the digits on Alt; name the spelling instead
        // of trusting the host platform (macOS runs the very same table as
        // ⌘-digits — asserted below).
        for d in '1'..='9' {
            let digit = d as u8 - b'0';
            // each digit resolves to itself, plain and meta-escaped (0x80 | d)
            assert_eq!(
                shortcut_on(Chords::Ctrl, d.to_string().as_str(), false, true, false),
                Some(Shortcut::SelectTab(digit)),
                "Alt-{d}"
            );
            let escaped = char::from_u32(0x80 + d as u32).unwrap();
            assert_eq!(
                shortcut_on(Chords::Ctrl, escaped.to_string().as_str(), false, true, false),
                Some(Shortcut::SelectTab(digit)),
                "Alt-{d} (meta-escaped)"
            );
            // …and the Mac spelling resolves the very same digit.
            assert_eq!(
                shortcut_on(Chords::Cmd, d.to_string().as_str(), false, false, true),
                Some(Shortcut::SelectTab(digit)),
                "Cmd-{d}"
            );
        }
        assert_eq!(shortcut_on(Chords::Ctrl, "!", false, true, false), Some(Shortcut::SelectTab(1)));
        assert_eq!(shortcut_on(Chords::Ctrl, "(", false, true, false), Some(Shortcut::SelectTab(8)));
        // Alt-0 is not a tab chord (and Ctrl-0 stays zoom reset).
        assert_eq!(shortcut_on(Chords::Ctrl, "0", false, true, false), None);
        // Bare digits and other modifier combos stay with the page / WM.
        assert_eq!(shortcut_on(Chords::Ctrl, "3", false, false, false), None);
        assert_eq!(shortcut_on(Chords::Ctrl, "3", true, false, false), None); // Ctrl-3
        assert_eq!(shortcut_on(Chords::Ctrl, "3", true, true, false), None); // Ctrl-Alt-3
        assert_eq!(shortcut_on(Chords::Ctrl, "3", false, false, true), None); // Meta-3
        assert_eq!(shortcut_on(Chords::Ctrl, "c", false, true, false), None); // Alt-C
        assert_eq!(shortcut_on(Chords::Ctrl, "l", false, true, false), None); // Alt-L
        // Alt-Tab is still nobody's tab chord (the WM cycles windows).
        assert_eq!(shortcut_on(Chords::Ctrl, "\t", false, true, false), None);
    }

    #[test]
    fn alt_digit_mapping_is_resolved_against_the_live_tab_count() {
        // Positions 1..8 are addressed literally while they exist.
        assert_eq!(tab_index_for_digit(1, 1), Some(0));
        assert_eq!(tab_index_for_digit(2, 3), Some(1));
        assert_eq!(tab_index_for_digit(3, 3), Some(2));
        // A digit past the end reaches nothing — including Alt-9 with 8 tabs.
        assert_eq!(tab_index_for_digit(4, 3), None);
        assert_eq!(tab_index_for_digit(9, 3), None);
        assert_eq!(tab_index_for_digit(9, 8), None);
        assert_eq!(tab_index_for_digit(0, 12), None);
        assert_eq!(tab_index_for_digit(1, 0), None);
        assert_eq!(tab_index_for_digit(10, 12), None);
        // Alt-9 becomes "the last tab" from nine tabs on, and keeps following
        // the end as tabs open and close underneath it.
        assert_eq!(tab_index_for_digit(9, 9), Some(8));
        assert_eq!(tab_index_for_digit(9, 12), Some(11));
        assert_eq!(tab_index_for_digit(9, 250), Some(249));
        assert_eq!(tab_index_for_digit(8, 12), Some(7));
    }

    #[test]
    fn tab_strip_badges_are_the_inverse_of_the_chord_mapping() {
        // Three tabs: each shows its own digit.
        assert_eq!(tab_shortcut_digit(0, 3), Some(1));
        assert_eq!(tab_shortcut_digit(1, 3), Some(2));
        assert_eq!(tab_shortcut_digit(2, 3), Some(3));
        assert_eq!(tab_shortcut_digit(3, 3), None);
        // Closing tab 1 moves every badge (and every chord) onto a new tab:
        // what was Alt-3 is Alt-2 now, and Alt-2's old tab answers Alt-1.
        let before: Vec<Option<u8>> = (0..3).map(|i| tab_shortcut_digit(i, 3)).collect();
        let after: Vec<Option<u8>> = (0..2).map(|i| tab_shortcut_digit(i, 2)).collect();
        assert_eq!(before, vec![Some(1), Some(2), Some(3)]);
        assert_eq!(after, vec![Some(1), Some(2)]);
        // Eight positions keep their digits; Alt-9 takes the last tab, and the
        // tabs behind it carry no badge.
        assert_eq!(tab_shortcut_digit(7, 12), Some(8));
        assert_eq!(tab_shortcut_digit(8, 12), None);
        assert_eq!(tab_shortcut_digit(11, 12), Some(9));
        // Exactly nine tabs: every one of them is reachable.
        let nine: Vec<Option<u8>> = (0..9).map(|i| tab_shortcut_digit(i, 9)).collect();
        assert_eq!(nine, (1..=9).map(Some).collect::<Vec<_>>());
        // Every badge round-trips through the chord mapping.
        for count in [1usize, 2, 8, 9, 10, 30] {
            for idx in 0..count {
                if let Some(d) = tab_shortcut_digit(idx, count) {
                    assert_eq!(tab_index_for_digit(d, count), Some(idx), "{idx} of {count}");
                }
            }
        }
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
    fn the_url_bar_keeps_typing_and_loses_the_browser_chords_to_nobody() {
        use super::*;
        // The task: with the URL bar focused, Ctrl-T still makes a tab and the
        // digits still switch tabs…
        assert_eq!(bar_shortcut("t", true, false, false), Some(Shortcut::NewTab));
        assert_eq!(
            bar_shortcut_on(Chords::Ctrl, "2", false, true, false),
            Some(Shortcut::SelectTab(2))
        );
        // …in whichever spelling the platform owns: the bar hands over ⌘2 on a
        // Mac, where Option is a typing key and Option-2 is `™` — so that press
        // must stay the entry's rather than jumping to a tab.
        assert_eq!(
            bar_shortcut_on(Chords::Cmd, "2", false, false, true),
            Some(Shortcut::SelectTab(2))
        );
        assert_eq!(bar_shortcut_on(Chords::Cmd, "2", false, true, false), None);
        assert_eq!(bar_shortcut("\t", true, false, false), Some(Shortcut::CycleTab));
        assert_eq!(bar_shortcut("w", true, false, false), Some(Shortcut::CloseTab));
        assert_eq!(bar_shortcut("r", true, false, false), Some(Shortcut::Reload));
        assert_eq!(bar_shortcut("f", true, false, false), Some(Shortcut::Find));
        // …the modifierless function chords work too (F12 = U+F70F, F6 = U+F709)…
        assert_eq!(bar_shortcut("\u{F70F}", false, false, false), Some(Shortcut::ToggleDevTools));
        assert_eq!(bar_shortcut("\u{F709}", false, false, false), Some(Shortcut::FocusUrl));
        // …while plain typing, bare Enter/Tab and the entry's own editing chords
        // belong to the LineEdit…
        assert_eq!(bar_shortcut("a", false, false, false), None);
        assert_eq!(bar_shortcut("\t", false, false, false), None);
        assert_eq!(bar_shortcut("a", true, false, false), None); // Ctrl-A select-all
        assert_eq!(bar_shortcut("c", true, false, false), None); // Ctrl-C copy
        assert_eq!(bar_shortcut("v", true, false, false), None); // Ctrl-V paste
        // …and Esc stays the bar's own (it closes the completion dropdown).
        assert_eq!(bar_shortcut("\u{1b}", false, false, false), None);
        // …and so do the history arrows, which the caret needs (macOS ⌘-← is
        // “home”; Linux Ctrl-← was never a chord) — while the bar holds focus
        // the entry moves, the shell does not navigate.
        assert_eq!(bar_shortcut_on(Chords::Cmd, "\u{F702}", false, false, true), None);
        assert_eq!(bar_shortcut_on(Chords::Cmd, "\u{F703}", false, false, true), None);
        // macOS spells the same with ⌘.
        assert_eq!(
            bar_shortcut_on(Chords::Cmd, "t", false, false, true),
            Some(Shortcut::NewTab)
        );
        assert_eq!(
            bar_shortcut_on(Chords::Cmd, "3", false, false, true),
            Some(Shortcut::SelectTab(3))
        );
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
