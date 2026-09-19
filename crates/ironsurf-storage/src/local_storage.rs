//! DOM `localStorage` persistence: one JSON area **per origin**, stored
//! inside the owning container's profile directory.
//!
//! The security contract (task: per-origin localStorage):
//!
//! * An area is keyed by the document **origin** — the tuple
//!   `(scheme, host, port-with-default)` serialized as
//!   `scheme://host:port`. Two URLs share storage iff this key matches, so
//!   `http://example.com` never reads `https://example.com`, port 8080
//!   never reads 80, and one container's disk never holds another's
//!   (the whole directory lives in the container profile, which the OS
//!   sandbox makes unreachable to other containers).
//! * Every other scheme — `about:`, `data:`, `file:`, `javascript:`,
//!   `blob:` of opaque parents, `ws:`… — has **no** durable storage area:
//!   [`storage_origin_key`] returns `None` and the caller must deny access
//!   (the tab throws `SecurityError`), matching what Chrome/Firefox do for
//!   opaque origins.
//! * One area's quota ([`MAX_LOCAL_STORAGE_BYTES`, 5 MiB of UTF-16 code
//!   units over names + values, like browsers]) is enforced at `set`; a
//!   write that would cross it fails with
//!   [`StoreError::QuotaExceeded`] and changes nothing.
//!
//! Files never contain raw origin text: the area file name is the lowercase
//! hex of the origin key's UTF-8 bytes (`https-example.com-443.json` with
//! the key inside), so no host name ends up in a file name.

use crate::StoreError;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

/// Directory name under a container profile dir (see `ARCHITECTURE.md` §Layout).
pub(crate) const LOCAL_STORAGE_DIR: &str = "local-storage";

/// Upper bound on one origin's area, counting key length + value length in
/// UTF-16 code units (the WHATWG Storage spec's accounting, which every
/// major browser implements at 5 MiB per origin).
pub const MAX_LOCAL_STORAGE_BYTES: usize = 5 * 1024 * 1024;

/// The durable storage **origin** of `url`, per the security rules above:
/// `scheme://host:port` for `http`/`https` (default ports folded in),
/// `None` for opaque origins — which get no area at all.
pub fn storage_origin_key(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    match parsed.scheme() {
        "http" | "https" => {
            let host = parsed.host_str()?;
            let port = parsed.port_or_known_default()?;
            Some(format!("{}://{}:{}", parsed.scheme(), host, port))
        }
        _ => None,
    }
}

/// UTF-16 code units of a string — the spec's storage-size unit.
fn utf16_len(s: &str) -> usize {
    s.chars().map(|c| c.len_utf16()).sum()
}

/// One origin's in-memory area plus its insertion-ordered key list (the
/// order `Storage.key(i)` must report; updating an existing key keeps its
/// position, deletes remove it).
#[derive(Default)]
struct Area {
    data: BTreeMap<String, String>,
    order: Vec<String>,
}

impl Area {
    fn size(&self) -> usize {
        self.data
            .iter()
            .map(|(k, v)| utf16_len(k) + utf16_len(v))
            .sum()
    }
}

/// Per-container DOM storage: many origin areas, each lazily loaded from and
/// atomically persisted to its own JSON file under `dir`.
pub struct LocalStorage {
    dir: PathBuf,
    areas: HashMap<String, Area>,
}

impl LocalStorage {
    /// Bind to `dir` (created if missing; creation errors surface on the
    /// first save instead of killing the tab).
    pub fn open(dir: impl AsRef<Path>) -> Self {
        let dir = dir.as_ref().to_path_buf();
        let _ = std::fs::create_dir_all(&dir);
        Self {
            dir,
            areas: HashMap::new(),
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The area file for an origin: hex of its key's UTF-8 bytes.
    fn area_path(&self, origin: &str) -> PathBuf {
        let hex: String = origin
            .as_bytes()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        self.dir.join(format!("{hex}.json"))
    }

    /// Load `origin`'s area from disk (once; corrupt files read as empty).
    fn area_mut(&mut self, origin: &str) -> &mut Area {
        if !self.areas.contains_key(origin) {
            let area = std::fs::read(self.area_path(origin))
                .ok()
                .and_then(|bytes| {
                    serde_json::from_str::<BTreeMap<String, String>>(&String::from_utf8_lossy(&bytes)).ok()
                })
                .map(|data| {
                    let order = data.keys().cloned().collect();
                    Area { data, order }
                })
                .unwrap_or_default();
            self.areas.insert(origin.to_owned(), area);
        }
        self.areas.get_mut(origin).expect("area just inserted")
    }

    fn persist(&self, origin: &str) -> Result<(), StoreError> {
        let area = self
            .areas
            .get(origin)
            .ok_or_else(|| StoreError::io(self.area_path(origin), std::io::Error::other("no such area")))?;
        let path = self.area_path(origin);
        let body = serde_json::to_string_pretty(&area.data)
            .map_err(|e| StoreError::json(&path, e))?;
        let tmp = self.dir.join(format!(
            "{}.json.tmp",
            path.file_stem().expect("stem").to_string_lossy()
        ));
        std::fs::write(&tmp, body.as_bytes()).map_err(|e| StoreError::io(&tmp, e))?;
        std::fs::rename(&tmp, &path).map_err(|e| StoreError::io(&path, e))?;
        Ok(())
    }

    /// `localStorage.getItem(key)` for `origin`.
    pub fn get(&mut self, origin: &str, key: &str) -> Option<String> {
        self.area_mut(origin).data.get(key).cloned()
    }

    /// `localStorage.setItem(key, value)` for `origin`: quota-checked before
    /// anything changes, persisted atomically after. A persistence failure
    /// is returned but the in-memory write stays (the page keeps working;
    /// the tab logs it).
    pub fn set(&mut self, origin: &str, key: &str, value: &str) -> Result<(), StoreError> {
        let area = self.area_mut(origin);
        let delta = utf16_len(key) + utf16_len(value)
            - area
                .data
                .get(key)
                .map(|old| utf16_len(key) + utf16_len(old))
                .unwrap_or(0);
        if area.size() + delta > MAX_LOCAL_STORAGE_BYTES {
            return Err(StoreError::QuotaExceeded {
                origin: origin.to_owned(),
                max: MAX_LOCAL_STORAGE_BYTES,
            });
        }
        if !area.data.contains_key(key) {
            area.order.push(key.to_owned());
        }
        area.data.insert(key.to_owned(), value.to_owned());
        self.persist(origin)
    }

    /// `localStorage.removeItem(key)`; `true` when a value was removed.
    pub fn remove(&mut self, origin: &str, key: &str) -> Result<bool, StoreError> {
        let removed = {
            let area = self.area_mut(origin);
            let removed = area.data.remove(key).is_some();
            if removed {
                area.order.retain(|k| k != key);
            }
            removed
        };
        if removed {
            self.persist(origin)?;
        }
        Ok(removed)
    }

    /// `localStorage.clear()`.
    pub fn clear(&mut self, origin: &str) -> Result<(), StoreError> {
        self.area_mut(origin);
        let area = self.areas.get_mut(origin).expect("area_mut just made it");
        area.data.clear();
        area.order.clear();
        self.persist(origin)
    }

    /// `localStorage.length`.
    pub fn len(&mut self, origin: &str) -> usize {
        self.area_mut(origin).data.len()
    }

    pub fn is_empty(&mut self, origin: &str) -> bool {
        self.len(origin) == 0
    }

    /// `localStorage.key(i)` — insertion order.
    pub fn key(&mut self, origin: &str, index: usize) -> Option<String> {
        self.area_mut(origin).order.get(index).cloned()
    }

    /// All key/value pairs (for tests and diagnostics).
    pub fn entries(&mut self, origin: &str) -> Vec<(String, String)> {
        let area = self.area_mut(origin);
        area.data.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HTTP: &str = "http://example.com:8080/app/page.html";
    const HTTP_OTHER_PATH: &str = "http://example.com:8080/other";
    const HTTP_DIFF_PORT: &str = "http://example.com:9000/";
    const HTTPS_SAME: &str = "https://example.com:8080/";

    #[test]
    fn origin_key_is_scheme_host_and_effective_port_only() {
        assert_eq!(storage_origin_key(HTTP).as_deref(), Some("http://example.com:8080"));
        assert_eq!(
            storage_origin_key(HTTP_OTHER_PATH),
            storage_origin_key(HTTP),
            "path/query/fragment/user never split an http origin"
        );
        assert_ne!(storage_origin_key(HTTP_DIFF_PORT), storage_origin_key(HTTP));
        assert_ne!(
            storage_origin_key(HTTPS_SAME),
            storage_origin_key(HTTP),
            "https is never the http origin"
        );
        // Default ports fold into the canonical form: :80 IS the http origin.
        assert_eq!(
            storage_origin_key("http://example.com/"),
            storage_origin_key("http://example.com:80/x")
        );
        assert_eq!(
            storage_origin_key("https://example.com:443/").as_deref(),
            Some("https://example.com:443")
        );
        // Hosts are host-name-serialised by `url` (lowercase, IDN → punycode).
        assert_eq!(
            storage_origin_key("HTTPS://Example.COM/%41").as_deref(),
            Some("https://example.com:443")
        );
        assert_eq!(
            storage_origin_key("http://例え.jp/").as_deref(),
            Some("http://xn--r8jz45g.jp:80")
        );
    }

    #[test]
    fn opaque_origins_get_no_area_at_all() {
        for url in [
            "about:blank",
            "about:config",
            "data:text/html,<script>1</script>",
            "file:///home/user/index.html",
            "javascript:alert(1)",
            "ws://example.com:8080/socket",
            "blob:http://example.com:8080/uuid",
            "not a url",
        ] {
            assert_eq!(storage_origin_key(url), None, "{url} must have no storage area");
        }
    }

    #[test]
    fn areas_are_isolated_per_origin_inside_one_container() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut store = LocalStorage::open(dir.path());
        let a = storage_origin_key(HTTP).expect("origin");
        let b = storage_origin_key(HTTP_DIFF_PORT).expect("origin");
        store.set(&a, "token", "secret-a").expect("set a");
        store.set(&b, "token", "secret-b").expect("set b");
        assert_eq!(store.get(&a, "token").as_deref(), Some("secret-a"));
        assert_eq!(store.get(&b, "token").as_deref(), Some("secret-b"));
        assert_eq!(store.get(&a, "missing"), None);
        // One origin's clear never touches its sibling.
        store.clear(&a).expect("clear a");
        assert_eq!(store.get(&a, "token"), None);
        assert_eq!(store.get(&b, "token").as_deref(), Some("secret-b"));
    }

    #[test]
    fn areas_persist_across_reopen_and_land_as_one_file_per_origin() {
        let dir = tempfile::tempdir().expect("tempdir");
        let a = storage_origin_key(HTTP).expect("origin");
        {
            let mut store = LocalStorage::open(dir.path());
            store.set(&a, "theme", "dark").expect("set");
            store.set(&a, "count", "42").expect("set");
        }
        let files: Vec<_> = std::fs::read_dir(dir.path())
            .expect("read_dir")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(files.len(), 1, "exactly one area file: {files:?}");
        assert!(
            !files[0].contains("example"),
            "the file name must not leak the origin verbatim: {}",
            files[0]
        );
        let body = std::fs::read_to_string(dir.path().join(&files[0])).expect("read area");
        assert!(body.contains("dark") && body.contains("42"));
        let mut reopened = LocalStorage::open(dir.path());
        assert_eq!(reopened.get(&a, "theme").as_deref(), Some("dark"));
        assert_eq!(reopened.len(&a), 2);
    }

    #[test]
    fn key_reports_insertion_order_and_update_keeps_position() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut store = LocalStorage::open(dir.path());
        let a = storage_origin_key(HTTP).expect("origin");
        store.set(&a, "b", "1").expect("set b");
        store.set(&a, "a", "1").expect("set a");
        store.set(&a, "c", "1").expect("set c");
        store.set(&a, "b", "2").expect("update b");
        assert_eq!(store.key(&a, 0).as_deref(), Some("b"));
        assert_eq!(store.key(&a, 1).as_deref(), Some("a"));
        assert_eq!(store.key(&a, 2).as_deref(), Some("c"));
        assert_eq!(store.key(&a, 3), None, "out of range reads None");
        assert!(store.remove(&a, "a").expect("remove"));
        assert_eq!(store.key(&a, 1).as_deref(), Some("c"), "removed keys drop out of order");
        assert!(!store.remove(&a, "a").expect("second remove answers false"));
    }

    #[test]
    fn quota_is_enforced_per_origin_and_changes_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut store = LocalStorage::open(dir.path());
        let a = storage_origin_key(HTTP).expect("origin");
        let b = storage_origin_key(HTTP_DIFF_PORT).expect("origin");
        let big = "x".repeat(MAX_LOCAL_STORAGE_BYTES - 16);
        store.set(&a, "big", &big).expect("just under quota");
        let over = "y".repeat(64);
        let err = store.set(&a, "more", &over).expect_err("crosses the 5 MiB quota");
        assert!(matches!(err, StoreError::QuotaExceeded { .. }), "{err}");
        assert_eq!(store.get(&a, "more"), None, "a refused write stores nothing");
        // Replacing the big value with a bigger one is refused too…
        assert!(store.set(&a, "big", &"z".repeat(MAX_LOCAL_STORAGE_BYTES)).is_err());
        assert_eq!(store.get(&a, "big").as_deref(), Some(big.as_str()), "old value survives");
        // …and the other origin is unaffected (quota is per area).
        store.set(&b, "fine", "still works").expect("b has its own quota");
        assert_eq!(store.get(&b, "fine").as_deref(), Some("still works"));
    }

    #[test]
    fn corrupt_area_files_read_as_empty_never_crash() {
        let dir = tempfile::tempdir().expect("tempdir");
        let a = storage_origin_key(HTTP).expect("origin");
        let store = LocalStorage::open(dir.path());
        std::fs::write(store.area_path(&a), b"{ not json").expect("write garbage");
        drop(store);
        let mut reopened = LocalStorage::open(dir.path());
        assert_eq!(reopened.len(&a), 0);
        reopened.set(&a, "fresh", "1").expect("writes recover over garbage");
        assert_eq!(reopened.get(&a, "fresh").as_deref(), Some("1"));
    }
}

impl std::fmt::Debug for LocalStorage {
    /// Prints the directory and how many origin areas are loaded — never
    /// keys or values (page data must not end up in a crash log).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalStorage")
            .field("dir", &self.dir)
            .field("loaded_areas", &self.areas.len())
            .finish()
    }
}
