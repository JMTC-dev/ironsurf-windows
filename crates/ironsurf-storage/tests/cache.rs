//! NetCache bounds: entry-count and byte-budget LRU eviction.

use ironsurf_storage::{NetCache, DEFAULT_MAX_ENTRIES, DEFAULT_MAX_TOTAL_BYTES};

#[test]
fn default_bounds_match_contract() {
    assert_eq!(DEFAULT_MAX_ENTRIES, 1000);
    assert_eq!(DEFAULT_MAX_TOTAL_BYTES, 256 * 1024 * 1024);
}

fn cache(entries: usize, bytes: usize) -> NetCache {
    let dir = tempfile::tempdir().unwrap();
    // keep() leaks the tempdir for the process lifetime (cache cleans its
    // own dir); it lands under /tmp.
    NetCache::with_limits(dir.keep().to_path_buf(), entries, bytes)
}

fn body(n: usize) -> Vec<u8> {
    vec![b'x'; n]
}

#[test]
fn store_and_lookup_roundtrip() {
    let cache = cache(10, 1 << 20);
    let headers = vec![
        ("Content-Type".to_string(), "text/html".to_string()),
        ("ETag".to_string(), "\"abc\"".to_string()),
        ("Last-Modified".to_string(), "Wed, 09 Jun 2021 10:18:14 GMT".to_string()),
    ];
    cache.store("https://example.com/", 200, &headers, body(11), 1234);
    let e = cache.lookup("https://example.com/").expect("hit");
    assert_eq!(e.status, 200);
    assert_eq!(e.body.len(), 11);
    assert_eq!(e.etag.as_deref(), Some("\"abc\""));
    assert_eq!(e.last_modified.as_deref(), Some("Wed, 09 Jun 2021 10:18:14 GMT"));
    assert_eq!(e.cached_at, 1234);
    assert_eq!(e.headers, headers);
    assert_eq!(cache.len(), 1);
    // Unknown url misses.
    assert!(cache.lookup("https://example.com/other").is_none());
}

#[test]
fn evicts_least_recently_used_at_entry_bound() {
    let cache = cache(3, 1 << 20);
    for u in ["a", "b", "c"] {
        cache.store(u, 200, &[], body(1), 0);
    }
    assert_eq!(cache.len(), 3);

    // Touch "a" so it becomes most-recently-used, then insert a 4th:
    // the LRU ("b") must be evicted, not "a".
    assert!(cache.lookup("a").is_some());
    cache.store("d", 200, &[], body(1), 0);

    assert_eq!(cache.len(), 3);
    assert!(cache.lookup("a").is_some(), "touched entry survives");
    assert!(cache.lookup("d").is_some());
    assert!(cache.lookup("c").is_some());
    assert!(cache.lookup("b").is_none(), "LRU entry evicted");
}

#[test]
fn evicts_until_under_byte_budget() {
    let cache = cache(1000, 100);
    cache.store("a", 200, &[], body(60), 0);
    assert_eq!(cache.len(), 1);
    cache.store("b", 200, &[], body(60), 0);
    // 120 counted bytes > 100 budget => evict LRU until it fits.
    assert_eq!(cache.len(), 1);
    assert!(cache.lookup("b").is_some());
    assert!(cache.total_bytes() <= 100);
}

#[test]
fn oversized_body_is_not_cached() {
    let cache = cache(10, 100);
    cache.store("big", 200, &[], body(101), 0);
    assert!(cache.is_empty());
    assert!(cache.lookup("big").is_none());
}

#[test]
fn re_storing_same_url_replaces_not_duplicates() {
    let cache = cache(10, 1 << 20);
    cache.store("u", 200, &[], body(10), 1);
    cache.store("u", 304, &[], body(20), 2);
    assert_eq!(cache.len(), 1);
    let e = cache.lookup("u").unwrap();
    assert_eq!(e.status, 304);
    assert_eq!(e.cached_at, 2);
}

#[test]
fn clear_drops_everything() {
    let cache = cache(10, 1 << 20);
    cache.store("a", 200, &[], body(1), 0);
    cache.store("b", 200, &[], body(1), 0);
    cache.clear();
    assert!(cache.is_empty());
    assert_eq!(cache.total_bytes(), 0);
    assert!(cache.lookup("a").is_none());
}
