//! System clipboard bridge (task: text-selection copy & paste).
//!
//! A tab is sandboxed — it cannot touch any display server's resources — so
//! the shell is the ONE process that speaks for the browser to the OS
//! clipboard. Every copy the shell learns about (page Ctrl+C/Ctrl+X, the
//! menu's Copy rows, the UA panel's Copy) is mirrored out to the system, and
//! every paste source (the Ctrl+V chord, the menu's Paste row) prefers the
//! system clipboard and falls back to the session copy. With no reachable
//! system clipboard (no display server, `IRONSURF_NO_OS_CLIPBOARD=1`, or a
//! platform failure) the session clipboard alone keeps behaving exactly as
//! before: the bridge is an enhancement, never a dependency.
//!
//! Tests opt out via `IRONSURF_NO_OS_CLIPBOARD` because the X clipboard is
//! mutable global state shared by the whole desktop — a test must not read
//! (or be read through) it; unit tests never touch the desktop at all.

use ironsurf_common::{debug, info, warn};

/// What `IRONSURF_NO_OS_CLIPBOARD` means: a set value other than blank,
/// `0`, `false` or `off` disables every system access. Pure, so the rule
/// itself is testable without any environment mutation.
pub fn flag_disables(value: Option<&str>) -> bool {
    match value.map(str::trim) {
        Some(v) => !matches!(v.to_ascii_lowercase().as_str(), "" | "0" | "false" | "off"),
        None => false,
    }
}

/// Whether the system clipboard is switched off for this process.
fn disabled() -> bool {
    // Unit tests run many threads over one process; none of them may race
    // the desktop clipboard, so the bridge is compiled off under `cfg(test)`.
    if cfg!(test) {
        return true;
    }
    flag_disables(std::env::var("IRONSURF_NO_OS_CLIPBOARD").ok().as_deref())
}

/// The shell's window onto the system clipboard. Kept alive for the process
/// (arboard holds clipboard *ownership* on X11 with an internal thread, so
/// one instance outlives every individual copy).
pub struct OsClipboard {
    inner: Option<arboard::Clipboard>,
}

impl OsClipboard {
    /// Connect once; a failure is remembered as "no system clipboard" and
    /// never retried (no per-copy connection storms when there is none).
    pub fn new() -> Self {
        let inner = if disabled() {
            info!(
                "clipboard: no system clipboard ({}) — session clipboard only",
                if cfg!(test) { "test binary" } else { "disabled by IRONSURF_NO_OS_CLIPBOARD" }
            );
            None
        } else {
            match arboard::Clipboard::new() {
                Ok(c) => Some(c),
                Err(err) => {
                    info!(
                        "clipboard: no system clipboard ({err}) — session clipboard only"
                    );
                    None
                }
            }
        };
        OsClipboard { inner }
    }

    /// Whether a system clipboard is reachable at all.
    pub fn active(&self) -> bool {
        self.inner.is_some()
    }

    /// Mirror one copy out. A failure is logged, never fatal: the session
    /// clipboard already holds the text, so pages can still paste it.
    pub fn set(&mut self, text: &str) {
        let Some(inner) = self.inner.as_mut() else { return };
        match inner.set_text(text) {
            Ok(()) => {
                debug!("clipboard: system set {} char(s)", text.chars().count())
            }
            Err(err) => {
                warn!("clipboard: system copy failed ({err}) — session clipboard still holds it")
            }
        }
    }

    /// Read the system clipboard; `None` = unreachable, unreadable or empty
    /// (an empty system clipboard is no paste source, exactly as a copy of
    /// nothing is no copy).
    pub fn get(&mut self) -> Option<String> {
        let inner = self.inner.as_mut()?;
        match inner.get_text() {
            Ok(text) if !text.is_empty() => Some(text),
            Ok(_) => None,
            Err(err) => {
                debug!("clipboard: system read failed ({err}) — falling back to the session copy");
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_opt_out_flag_reads_naturally_in_both_directions() {
        // Values that mean "keep the system clipboard".
        for off in [None, Some(""), Some("0"), Some("false"), Some("OFF"), Some(" false ")] {
            assert!(!flag_disables(off), "{off:?} must not disable the system clipboard");
        }
        // Values that mean "session clipboard only".
        for on in [Some("1"), Some("yes"), Some("true")] {
            assert!(flag_disables(on), "{on:?} must disable the system clipboard");
        }
        // And this test binary is off by construction.
        assert!(disabled());
    }
}
