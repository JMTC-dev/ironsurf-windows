//! Shared browsing history (task: browsing history): one `history.jsonl` at
//! the data root — recorded from any container, visible from all of them —
//! newest-first reads, the consecutive-duplicate skip, the bounded trim, and
//! clear.

use std::io::Write;

use ironsurf_storage::{History, MAX_HISTORY_ENTRIES};

fn urls(entries: &[ironsurf_storage::HistoryEntry]) -> Vec<String> {
    entries.iter().map(|e| e.url.clone()).collect()
}

#[test]
fn history_is_shared_across_containers() {
    let tmp = tempfile::tempdir().unwrap();
    let mut store = ironsurf_storage::Store::open(tmp.path()).unwrap();
    let a = store.create_container("A", "#ff0000").unwrap();
    let b = store.default_container_id();
    assert_ne!(a, b);

    // A visit recorded "in" container A and one "in" container B…
    let h = History::open(store.root());
    h.record("https://a.example/", Some("A page")).unwrap();
    h.record("https://b.example/", Some("B page")).unwrap();

    // …land in ONE file at the root, newest first, from any reader.
    assert!(tmp.path().join("history.jsonl").exists());
    let shared = History::open(store.root());
    assert_eq!(
        urls(&shared.entries()),
        ["https://b.example/", "https://a.example/"]
    );
    assert_eq!(shared.entries()[0].title.as_deref(), Some("B page"));
}

#[test]
fn consecutive_duplicate_visit_is_skipped() {
    let tmp = tempfile::tempdir().unwrap();
    let h = History::open(tmp.path());
    h.record("https://example.com/", Some("Home")).unwrap();
    // Same URL and title again (a reload, a re-render): no new line.
    h.record("https://example.com/", Some("Home")).unwrap();
    assert_eq!(h.entries().len(), 1);
    // Same URL, different title is a new visit (e.g. the title arriving late).
    h.record("https://example.com/", None).unwrap();
    assert_eq!(h.entries().len(), 2);
    // Leaving and coming back records again.
    h.record("https://other.example/", None).unwrap();
    h.record("https://example.com/", None).unwrap();
    assert_eq!(h.entries().len(), 4);
    assert_eq!(h.entries()[0].url, "https://example.com/");
}

#[test]
fn clear_forgets_everything_and_survives_reopening() {
    let tmp = tempfile::tempdir().unwrap();
    let h = History::open(tmp.path());
    h.record("https://example.com/one", None).unwrap();
    h.record("https://example.com/two", None).unwrap();
    assert!(!h.entries().is_empty());
    h.clear().unwrap();
    assert!(h.entries().is_empty());
    assert!(!tmp.path().join("history.jsonl").exists());
    // Clearing twice is fine; recording works again afterwards.
    h.clear().unwrap();
    h.record("https://example.com/three", None).unwrap();
    assert_eq!(urls(&h.entries()), ["https://example.com/three"]);
}

#[test]
fn log_is_bounded_and_drops_oldest_first() {
    let tmp = tempfile::tempdir().unwrap();
    let h = History::open(tmp.path());
    for i in 0..MAX_HISTORY_ENTRIES + 5 {
        h.record(&format!("https://example.com/{i}"), None).unwrap();
    }
    let entries = h.entries();
    assert_eq!(entries.len(), MAX_HISTORY_ENTRIES);
    // Newest first…
    assert_eq!(entries[0].url, format!("https://example.com/{}", MAX_HISTORY_ENTRIES + 4));
    // …and the very oldest visits are gone.
    assert!(!entries.iter().any(|e| e.url.ends_with("/0")));
}

#[test]
fn corrupt_lines_are_skipped_on_read() {
    let tmp = tempfile::tempdir().unwrap();
    let h = History::open(tmp.path());
    h.record("https://good.example/", None).unwrap();
    std::fs::OpenOptions::new()
        .append(true)
        .open(h.path())
        .unwrap()
        .write_all(b"not json at all\n")
        .unwrap();
    h.record("https://newer.example/", None).unwrap();
    assert_eq!(urls(&h.entries()), ["https://newer.example/", "https://good.example/"]);
}
