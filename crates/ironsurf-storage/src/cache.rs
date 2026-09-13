//! Bounded in-memory response cache with LRU eviction (~256 MB / 1000
//! entries by default). `store`/`lookup` mirror `&self` (like the rest of
//! this module's best-effort disk API) via interior mutability.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;

/// One cached HTTP response.
#[derive(Clone, Debug, PartialEq)]
pub struct CachedEntry {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub cached_at: i64,
}

pub(crate) const CACHE_DIR: &str = "net-cache";

/// Default total byte budget (~256 MB).
pub const DEFAULT_MAX_TOTAL_BYTES: usize = 256 * 1024 * 1024;
/// Default maximum number of cached entries.
pub const DEFAULT_MAX_ENTRIES: usize = 1000;

struct CacheItem {
    entry: CachedEntry,
    bytes: usize,
}

struct Inner {
    map: HashMap<String, CacheItem>,
    /// Front = most recently used.
    order: VecDeque<String>,
    total_bytes: usize,
}

/// Bounded LRU cache for one container's `net-cache/`.
pub struct NetCache {
    dir: PathBuf,
    max_entries: usize,
    max_total_bytes: usize,
    inner: RefCell<Inner>,
}

impl NetCache {
    pub(crate) fn new(dir: PathBuf) -> Self {
        Self::with_limits(dir, DEFAULT_MAX_ENTRIES, DEFAULT_MAX_TOTAL_BYTES)
    }

    /// Non-default bounds (used by tests; also handy for constrained tabs).
    pub fn with_limits(dir: PathBuf, max_entries: usize, max_total_bytes: usize) -> Self {
        let _ = std::fs::create_dir_all(&dir);
        Self {
            dir,
            max_entries: max_entries.max(1),
            max_total_bytes: max_total_bytes.max(1),
            inner: RefCell::new(Inner {
                map: HashMap::new(),
                order: VecDeque::new(),
                total_bytes: 0,
            }),
        }
    }

    /// Directory backing this container's cache (`profiles/…/net-cache`).
    pub fn dir(&self) -> &PathBuf {
        &self.dir
    }

    /// Clone of the cached response for `url`, marking it most-recently-used.
    pub fn lookup(&self, url: &str) -> Option<CachedEntry> {
        let mut inner = self.inner.borrow_mut();
        if !inner.map.contains_key(url) {
            return None;
        }
        touch(&mut inner, url);
        Some(inner.map[url].entry.clone())
    }

    /// Insert/replace `url`, evicting least-recently-used entries until the
    /// entry and byte bounds hold. A body larger than the whole byte budget
    /// is not cached.
    pub fn store(&self, url: &str, status: u16, headers: &[(String, String)], body: Vec<u8>, now_unix: i64) {
        let header_bytes: usize = headers
            .iter()
            .map(|(k, v)| k.len() + v.len() + 2)
            .sum();
        let bytes = body.len() + header_bytes;
        if bytes > self.max_total_bytes {
            return;
        }
        let etag = find_header(headers, "etag").map(|s| s.to_string());
        let last_modified = find_header(headers, "last-modified").map(|s| s.to_string());
        let entry = CachedEntry {
            status,
            headers: headers.to_vec(),
            body,
            etag,
            last_modified,
            cached_at: now_unix,
        };
        let mut inner = self.inner.borrow_mut();
        if let Some(old) = inner.map.insert(
            url.to_string(),
            CacheItem {
                entry,
                bytes: bytes.min(self.max_total_bytes),
            },
        ) {
            inner.total_bytes -= old.bytes;
            // Re-inserted: refresh recency.
            touch(&mut inner, url);
        } else {
            inner.order.push_front(url.to_string());
        }
        inner.total_bytes += bytes.min(self.max_total_bytes);
        evict(&mut inner, self.max_entries, self.max_total_bytes);
    }

    /// Drop everything cached (and the cache dir's stale contents).
    pub fn clear(&self) {
        let mut inner = self.inner.borrow_mut();
        inner.map.clear();
        inner.order.clear();
        inner.total_bytes = 0;
        if let Ok(entries) = std::fs::read_dir(&self.dir) {
            for e in entries.flatten() {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }

    /// Number of live entries (tests).
    pub fn len(&self) -> usize {
        self.inner.borrow().map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Total counted bytes of live entries (tests).
    pub fn total_bytes(&self) -> usize {
        self.inner.borrow().total_bytes
    }
}

fn find_header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

/// Move `url` to the front of the LRU order (it must exist in `map`).
fn touch(inner: &mut Inner, url: &str) {
    if let Some(pos) = inner.order.iter().position(|u| u == url) {
        if let Some(u) = inner.order.remove(pos) {
            inner.order.push_front(u);
        }
    } else {
        inner.order.push_front(url.to_string());
    }
}

/// Evict LRU entries (back of `order`) until both bounds hold.
fn evict(inner: &mut Inner, max_entries: usize, max_total_bytes: usize) {
    while inner.map.len() > max_entries || inner.total_bytes > max_total_bytes {
        match inner.order.back().cloned() {
            Some(url) => {
                inner.order.pop_back();
                if let Some(item) = inner.map.remove(&url) {
                    inner.total_bytes = inner.total_bytes.saturating_sub(item.bytes);
                }
            }
            None => break,
        }
    }
}
