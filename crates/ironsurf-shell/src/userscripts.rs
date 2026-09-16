//! Pure view-model helpers for the userscript menu (task: userscript menu).
//!
//! The menu (ui/ironsurf.slint) paints plain rows, and the wire contract
//! ships only enabled scripts — both reductions live here, headless and
//! Slint-free except for the row struct itself (the generated `UserscriptRow`
//! from ui/ironsurf.slint), so the mapping and the payload rule are unit
//! tests without a window.

use ironsurf_common::Userscript;

use crate::UserscriptRow;

/// The menu shows a script's URL patterns joined by this (dim, under the name).
pub const PATTERN_SEPARATOR: &str = ", ";

/// Map stored scripts (storage order) onto menu rows: name verbatim, patterns
/// joined by [`PATTERN_SEPARATOR`], enabled carried through. Ids become Slint
/// `int`s — the store hands out 1, 2, 3…, so the cast is display bookkeeping
/// only; Rust-side mutations always go back through the id mirror.
pub fn userscript_rows(scripts: &[Userscript]) -> Vec<UserscriptRow> {
    scripts
        .iter()
        .map(|s| UserscriptRow {
            id: s.id as i32,
            name: s.name.as_str().into(),
            patterns: s.patterns.join(PATTERN_SEPARATOR).into(),
            enabled: s.enabled,
        })
        .collect()
}

/// The `ToTab::SetUserscripts` payload: **all enabled** scripts, storage
/// order, verbatim. Disabled scripts are never sent (ironsurf-common's
/// contract) — an all-disabled store clears every tab.
pub fn enabled_userscripts(scripts: &[Userscript]) -> Vec<Userscript> {
    scripts.iter().filter(|s| s.enabled).cloned().collect()
}

/// Split the editor's patterns field: commas and whitespace separate, empty
/// pieces drop. A lone `*` is one pattern, `a.com, b.com *` is three.
pub fn parse_patterns(input: &str) -> Vec<String> {
    input.split([' ', '\t', '\n', '\r', ',']).filter(|p| !p.is_empty()).map(str::to_string).collect()
}

/// Assemble a new script from the editor fields (id from
/// `Store::next_userscript_id`). Validation is deliberately *not* done here —
/// the Store validates on `upsert_userscript` and the menu surfaces whatever
/// `StoreError` says.
pub fn build_new_userscript(
    id: u64,
    name: &str,
    patterns: &str,
    code: &str,
) -> Userscript {
    Userscript {
        id,
        name: name.trim().to_string(),
        patterns: parse_patterns(patterns),
        code: code.to_string(),
        enabled: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn script(id: u64, name: &str, patterns: &[&str], enabled: bool) -> Userscript {
        Userscript {
            id,
            name: name.into(),
            patterns: patterns.iter().map(|p| p.to_string()).collect(),
            code: "console.log(1)".into(),
            enabled,
        }
    }

    #[test]
    fn rows_carry_name_joined_patterns_and_enabled() {
        let scripts = vec![
            script(1, "Darkify", &["*example.com", "example.org/docs"], true),
            script(2, "Quiet", &["*"], false),
        ];
        let rows = userscript_rows(&scripts);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].name.as_str(), "Darkify");
        assert_eq!(rows[0].patterns.as_str(), "*example.com, example.org/docs");
        assert!(rows[0].enabled);
        assert_eq!(rows[0].id, 1);
        assert_eq!(rows[1].patterns.as_str(), "*");
        assert!(!rows[1].enabled, "the row mirrors the stored flag");
        assert!(userscript_rows(&[]).is_empty());
    }

    #[test]
    fn the_wire_payload_is_the_enabled_subset_only() {
        let scripts = vec![
            script(1, "on", &["*"], true),
            script(2, "off", &["*"], false),
            script(3, "on2", &["example.com"], true),
        ];
        // Disabled scripts are never sent; order and content are verbatim.
        assert_eq!(
            enabled_userscripts(&scripts),
            vec![scripts[0].clone(), scripts[2].clone()]
        );
        // All disabled (or empty) ⇒ the empty set, which clears every tab.
        let off = vec![script(1, "off", &["*"], false)];
        assert!(enabled_userscripts(&off).is_empty());
        assert!(enabled_userscripts(&[]).is_empty());
    }

    #[test]
    fn patterns_split_on_commas_and_whitespace() {
        assert_eq!(parse_patterns("a.com, b.com *"), ["a.com", "b.com", "*"]);
        assert_eq!(parse_patterns("  a.com  b.com ,, c.com "), ["a.com", "b.com", "c.com"]);
        assert_eq!(parse_patterns("example.com/path"), ["example.com/path"]);
        assert!(parse_patterns("   ").is_empty());
        assert!(parse_patterns(",").is_empty());
    }

    #[test]
    fn a_new_script_starts_enabled_with_trimmed_name() {
        let s = build_new_userscript(7, "  Greeter  ", "*example.com", "console.log('hi')\n");
        assert_eq!(s.id, 7);
        assert_eq!(s.name, "Greeter");
        assert_eq!(s.patterns, vec!["*example.com"]);
        assert_eq!(s.code, "console.log('hi')\n", "code is stored verbatim");
        assert!(s.enabled, "a freshly saved script runs immediately");
    }

    #[test]
    fn toggling_persists_through_reopen_and_flows_into_the_payload() {
        // Mirror of the storage test style: a tempdir Store, mutated the same
        // way the menu's CheckBox does it, read back from a fresh Store — the
        // persistence the toggle button relies on.
        let tmp = tempfile::tempdir().unwrap();
        let mut store = ironsurf_storage::Store::open(tmp.path()).unwrap();
        assert_eq!(store.next_userscript_id(), 1);
        store.upsert_userscript(script(1, "Keep", &["*"], true)).unwrap();
        store.upsert_userscript(build_new_userscript(
            store.next_userscript_id(),
            "New",
            "*example.com",
            "var x = 1;",
        ))
        .unwrap();

        store.set_userscript_enabled(1, false).unwrap();

        drop(store);
        let reopened = ironsurf_storage::Store::open(tmp.path()).unwrap();
        let scripts = reopened.userscripts();
        assert_eq!(scripts.iter().map(|s| s.id).collect::<Vec<_>>(), vec![1, 2]);
        assert!(!scripts[0].enabled, "the toggle survived the reopen");
        assert!(scripts[1].enabled);
        // …and the wire payload follows the reopened state.
        assert_eq!(enabled_userscripts(&scripts), vec![scripts[1].clone()]);

        // Delete, then the enabled set empties (and stays empty on disk).
        let mut reopened = ironsurf_storage::Store::open(tmp.path()).unwrap();
        reopened.delete_userscript(2).unwrap();
        assert!(enabled_userscripts(&reopened.userscripts()).is_empty());
    }

    #[test]
    fn a_validation_failure_adds_nothing_to_the_store() {
        // The menu's Save leg relies on this: Store rejects, list untouched,
        // and the StoreError text goes up as the menu's one-line message.
        let tmp = tempfile::tempdir().unwrap();
        let mut store = ironsurf_storage::Store::open(tmp.path()).unwrap();
        let err = store
            .upsert_userscript(build_new_userscript(1, "Empty", "", "code"))
            .unwrap_err();
        assert!(matches!(
            err,
            ironsurf_storage::StoreError::EmptyUserscriptPatterns
        ));
        assert!(store.userscripts().is_empty(), "a rejected script is not stored");
    }
}
