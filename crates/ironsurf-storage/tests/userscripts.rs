//! Global userscript list: `userscripts.json` round-trips, upsert-by-id,
//! enable/disable persistence, delete, id allocation, and validation.

use ironsurf_common::Userscript;
use ironsurf_storage::{Store, StoreError, MAX_USERSCRIPT_CODE_BYTES, MAX_USERSCRIPT_NAME_CHARS};

fn open(root: &std::path::Path) -> Store {
    Store::open(root).expect("open store")
}

fn script(id: u64, name: &str) -> Userscript {
    Userscript {
        id,
        name: name.to_string(),
        patterns: vec![format!("*{name}.example.com")],
        code: "console.log('hi')".to_string(),
        enabled: true,
    }
}

fn ids(store: &Store) -> Vec<u64> {
    store.userscripts().iter().map(|s| s.id).collect()
}

#[test]
fn empty_store_has_no_userscripts_and_no_file() {
    let tmp = tempfile::tempdir().unwrap();
    let store = open(tmp.path());
    assert!(store.userscripts().is_empty());
    // Missing file = empty list, never an error, never seeded.
    assert!(!tmp.path().join("userscripts.json").exists());
    assert_eq!(store.next_userscript_id(), 1);
}

#[test]
fn add_round_trips_and_survives_reopen() {
    let tmp = tempfile::tempdir().unwrap();
    let mut store = open(tmp.path());

    store.upsert_userscript(script(1, "darkify")).unwrap();
    store.upsert_userscript(Userscript {
        id: 2,
        name: "  unicode 🔧 日  ".into(),
        patterns: vec!["example.org/docs".into(), "*".into()],
        code: "var x = 1;".into(),
        enabled: false,
    }).unwrap();
    assert!(tmp.path().join("userscripts.json").exists());

    let scripts = store.userscripts();
    assert_eq!(ids(&store), vec![1, 2]);
    // Names are stored trimmed; everything else verbatim.
    assert_eq!(scripts[1].name, "unicode 🔧 日");
    assert_eq!(scripts[1].patterns, vec!["example.org/docs", "*"]);
    assert!(!scripts[1].enabled);

    // A fresh Store reads the same list back off disk.
    drop(store);
    let store = open(tmp.path());
    assert_eq!(ids(&store), vec![1, 2]);
    assert_eq!(
        store.userscripts(),
        vec![script(1, "darkify"), Userscript {
            id: 2,
            name: "unicode 🔧 日".into(),
            patterns: vec!["example.org/docs".into(), "*".into()],
            code: "var x = 1;".into(),
            enabled: false,
        }]
    );
}

#[test]
fn toggle_enabled_survives_reopen() {
    let tmp = tempfile::tempdir().unwrap();
    let mut store = open(tmp.path());
    store.upsert_userscript(script(1, "a")).unwrap();
    store.upsert_userscript(script(2, "b")).unwrap();

    store.set_userscript_enabled(1, false).unwrap();
    store.set_userscript_enabled(2, true).unwrap();
    let flags: Vec<bool> = store.userscripts().iter().map(|s| s.enabled).collect();
    assert_eq!(flags, vec![false, true]);

    drop(store);
    let store = open(tmp.path());
    let flags: Vec<bool> = store.userscripts().iter().map(|s| s.enabled).collect();
    assert_eq!(flags, vec![false, true]);

    // Unknown ids error out (mirrors NoSuchContainer), list unchanged.
    let mut store = store;
    assert!(matches!(
        store.set_userscript_enabled(999, true),
        Err(StoreError::NoSuchUserscript(999))
    ));
    assert_eq!(ids(&store), vec![1, 2]);
}

#[test]
fn delete_removes_script_and_persists() {
    let tmp = tempfile::tempdir().unwrap();
    let mut store = open(tmp.path());
    store.upsert_userscript(script(1, "a")).unwrap();
    store.upsert_userscript(script(2, "b")).unwrap();

    store.delete_userscript(1).unwrap();
    assert_eq!(ids(&store), vec![2]);

    drop(store);
    let mut store = open(tmp.path());
    assert_eq!(ids(&store), vec![2]);
    store.delete_userscript(2).unwrap();
    assert!(store.userscripts().is_empty());
    // Deleting an unknown id is the same NotFound error as the toggle.
    assert!(matches!(
        store.delete_userscript(2),
        Err(StoreError::NoSuchUserscript(2))
    ));
    // Empty list still round-trips (file exists, decodes to []).
    drop(store);
    assert!(open(tmp.path()).userscripts().is_empty());
}

#[test]
fn upsert_replaces_by_id_without_duplicating() {
    let tmp = tempfile::tempdir().unwrap();
    let mut store = open(tmp.path());
    store.upsert_userscript(script(1, "orig")).unwrap();

    let replacement = Userscript {
        id: 1,
        name: "replaced".into(),
        patterns: vec!["*example.net/*".into()],
        code: "alert(2)".into(),
        enabled: false,
    };
    store.upsert_userscript(replacement.clone()).unwrap();

    assert_eq!(ids(&store), vec![1], "replace must not append a second entry");
    assert_eq!(store.userscripts()[0], replacement);

    drop(store);
    assert_eq!(open(tmp.path()).userscripts(), vec![replacement]);
}

#[test]
fn next_id_starts_at_one_and_bumps_per_script() {
    let tmp = tempfile::tempdir().unwrap();
    let mut store = open(tmp.path());
    assert_eq!(store.next_userscript_id(), 1);

    let id = store.next_userscript_id();
    store.upsert_userscript(script(id, "a")).unwrap();
    assert_eq!(store.next_userscript_id(), 2);

    let id = store.next_userscript_id();
    store.upsert_userscript(script(id, "b")).unwrap();
    assert_eq!(store.next_userscript_id(), 3);

    // Monotonic across reloads (max existing + 1, even after deletes).
    drop(store);
    let mut store = open(tmp.path());
    assert_eq!(store.next_userscript_id(), 3);
    store.delete_userscript(2).unwrap();
    assert_eq!(store.next_userscript_id(), 2, "max is 1 again: id 2 is offered once more");
    store.delete_userscript(1).unwrap();
    assert_eq!(store.next_userscript_id(), 1, "empty list restarts at 1");
}

#[test]
fn caller_supplied_sparse_ids_are_kept_verbatim() {
    let tmp = tempfile::tempdir().unwrap();
    let mut store = open(tmp.path());
    store.upsert_userscript(script(10, "ten")).unwrap();
    store.upsert_userscript(script(4_000_000, "big")).unwrap();
    assert_eq!(ids(&store), vec![10, 4_000_000]);
    assert_eq!(store.next_userscript_id(), 4_000_001);
    drop(store);
    assert_eq!(open(tmp.path()).next_userscript_id(), 4_000_001);
}

#[test]
fn validation_rejects_bad_scripts_without_persisting_them() {
    let tmp = tempfile::tempdir().unwrap();
    let mut store = open(tmp.path());

    // Blank / whitespace-only name.
    assert!(matches!(
        store.upsert_userscript(script(1, "   ")),
        Err(StoreError::EmptyUserscriptName)
    ));
    // No patterns at all.
    let mut no_patterns = script(2, "p");
    no_patterns.patterns = vec![];
    assert!(matches!(
        store.upsert_userscript(no_patterns),
        Err(StoreError::EmptyUserscriptPatterns)
    ));
    // Empty code.
    let mut no_code = script(3, "c");
    no_code.code = String::new();
    assert!(matches!(
        store.upsert_userscript(no_code),
        Err(StoreError::EmptyUserscriptCode)
    ));
    // Oversized code (1 MiB + 1 byte).
    let mut huge = script(4, "big");
    huge.code = "x".repeat(MAX_USERSCRIPT_CODE_BYTES + 1);
    assert!(matches!(
        store.upsert_userscript(huge),
        Err(StoreError::UserscriptCodeTooLarge { .. })
    ));
    // Over-long name (200 chars is the cap).
    let mut long_name = script(5, &"n".repeat(MAX_USERSCRIPT_NAME_CHARS + 1));
    long_name.name = "n".repeat(MAX_USERSCRIPT_NAME_CHARS + 1);
    assert!(matches!(
        store.upsert_userscript(long_name),
        Err(StoreError::UserscriptNameTooLong { length, max })
            if length == MAX_USERSCRIPT_NAME_CHARS + 1 && max == MAX_USERSCRIPT_NAME_CHARS
    ));

    assert!(store.userscripts().is_empty(), "rejected scripts never land");
    assert!(!tmp.path().join("userscripts.json").exists());

    // Boundary: exactly-at-cap name and exactly-1-MiB code are accepted.
    let mut ok = script(7, &"n".repeat(MAX_USERSCRIPT_NAME_CHARS));
    ok.code = "y".repeat(MAX_USERSCRIPT_CODE_BYTES);
    store.upsert_userscript(ok).unwrap();
    assert_eq!(ids(&store), vec![7]);
}
