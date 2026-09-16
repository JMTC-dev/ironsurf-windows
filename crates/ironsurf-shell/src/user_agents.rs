//! Pure view-model helpers for the User-Agent switcher (task: per-container UA).
//!
//! The menu (ui/ironsurf.slint) paints plain rows over the *active tab's
//! container*, and the wire ships one `Option<String>` — both reductions
//! live here, headless and Slint-free except for the row struct itself (the
//! generated `UaRow` from ui/ironsurf.slint), so the mapping — default row
//! first, then the [`BUILTIN_USER_AGENTS`] presets, one highlighted `active`
//! row, custom values answering "Custom" — is unit-testable without a window.

use ironsurf_common::builtin_user_agent_label;
use ironsurf_common::BUILTIN_USER_AGENTS;

use crate::UaRow;

/// Label of the menu's first row: the container carries **no** override and
/// every tab in it uses the engine's built-in User-Agent.
pub const DEFAULT_UA_LABEL: &str = "Default (IronSurf)";

/// What the switcher shows while the container holds a custom (non-preset)
/// UA string — the string itself can be long, the button stays small.
pub const CUSTOM_UA_LABEL: &str = "Custom";

/// The toolbar/menu label for a container's stored value: `None` ⇒
/// [`DEFAULT_UA_LABEL`], an **exact** [`BUILTIN_USER_AGENTS`] match ⇒ that
/// preset's name, anything else ⇒ [`CUSTOM_UA_LABEL`].
pub fn ua_display(current: Option<&str>) -> String {
    match current {
        None => DEFAULT_UA_LABEL.to_string(),
        Some(v) => builtin_user_agent_label(v)
            .unwrap_or(CUSTOM_UA_LABEL)
            .to_string(),
    }
}

/// The menu rows for one container's stored value: [`DEFAULT_UA_LABEL`] first
/// (its `ua` is empty — selecting it restores the tab default), then every
/// [`BUILTIN_USER_AGENTS`] preset in menu order. Exactly one row is `active`:
/// the default row while the container has no override, otherwise the preset
/// whose string matches **exactly** (a custom value highlights nothing).
pub fn ua_menu_rows(current: Option<&str>) -> Vec<UaRow> {
    let mut rows = Vec::with_capacity(1 + BUILTIN_USER_AGENTS.len());
    rows.push(UaRow {
        label: DEFAULT_UA_LABEL.into(),
        ua: "".into(),
        active: current.is_none(),
    });
    rows.extend(BUILTIN_USER_AGENTS.iter().map(|(label, ua)| UaRow {
        label: (*label).into(),
        ua: (*ua).into(),
        active: current == Some(*ua),
    }));
    rows
}

/// Normalise the custom-UA LineEdit before it goes to the Store: trim, and
/// an all-blank input is `None` — "restore the default", not a stored empty
/// string. Length/character validation stays the Store's business
/// (`Store::set_container_user_agent`), whose `StoreError` the menu shows.
pub fn normalize_custom_ua(input: &str) -> Option<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironsurf_common::MAX_USER_AGENT_CHARS;

    fn preset(idx: usize) -> &'static str {
        BUILTIN_USER_AGENTS[idx].1
    }

    #[test]
    fn display_covers_default_preset_and_custom() {
        assert_eq!(ua_display(None), DEFAULT_UA_LABEL);
        for (label, ua) in BUILTIN_USER_AGENTS {
            assert_eq!(ua_display(Some(ua)), *label);
        }
        assert_eq!(ua_display(Some("My Browser 1.0")), CUSTOM_UA_LABEL);
        // Exact match only (the common contract): near-misses are custom.
        assert_eq!(ua_display(Some(&format!(" {}", preset(0)))), CUSTOM_UA_LABEL);
        assert_eq!(ua_display(Some("firefox")), CUSTOM_UA_LABEL);
    }

    #[test]
    fn rows_are_default_then_the_five_presets() {
        let rows = ua_menu_rows(None);
        assert_eq!(rows.len(), 1 + BUILTIN_USER_AGENTS.len());
        assert_eq!(rows[0].label.as_str(), DEFAULT_UA_LABEL);
        assert_eq!(rows[0].ua.as_str(), "", "the default row selects no string");
        assert!(rows[0].active, "no override ⇒ the default row is the live one");
        assert!(rows[1..].iter().all(|r| !r.active));
        for (i, (label, ua)) in BUILTIN_USER_AGENTS.iter().enumerate() {
            assert_eq!(rows[i + 1].label.as_str(), *label);
            assert_eq!(rows[i + 1].ua.as_str(), *ua, "the row carries the wire string");
        }
    }

    #[test]
    fn exactly_one_row_is_active_and_a_custom_value_highlights_none() {
        let rows = ua_menu_rows(Some(preset(2))); // Bingbot
        assert_eq!(
            rows.iter().filter(|r| r.active).count(),
            1,
            "exactly one row matches the container value"
        );
        assert_eq!(rows[3].label.as_str(), "Bingbot");
        assert!(rows[3].active);
        assert!(!rows[0].active, "an override un-highlights the default row");

        let custom = ua_menu_rows(Some("Totally Custom UA/1.0"));
        assert!(custom.iter().all(|r| !r.active), "a custom value matches no preset");

        // Every preset, in turn, activates its own row and only that row.
        for i in 0..BUILTIN_USER_AGENTS.len() {
            let rows = ua_menu_rows(Some(preset(i)));
            assert_eq!(rows.iter().filter(|r| r.active).count(), 1);
            assert!(rows[i + 1].active);
        }
    }

    #[test]
    fn custom_input_is_trimmed_and_blank_means_default() {
        assert_eq!(
            normalize_custom_ua("  Mozilla/5.0 (custom)  ").as_deref(),
            Some("Mozilla/5.0 (custom)")
        );
        assert_eq!(normalize_custom_ua(""), None, "empty restores the default");
        assert_eq!(normalize_custom_ua("   \t \n"), None, "all-blank too");
        // The Store enforces the cap; a cap-length string passes normalising
        // untouched so `set_container_user_agent` can accept the boundary.
        let exact = "u".repeat(MAX_USER_AGENT_CHARS);
        assert_eq!(normalize_custom_ua(&exact).as_deref(), Some(exact.as_str()));
    }
}
