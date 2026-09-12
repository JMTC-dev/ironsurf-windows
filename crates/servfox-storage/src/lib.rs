//! ServFox disk layer: container registry, per-container profile stores,
//! an RFC6265-subset cookie jar, a bounded LRU network cache, and history.
//!
//! No engine dependency — pure files under a data root:
//!
//! ```text
//! $SERVFOX_DATA_DIR (or ~/.local/share/servfox)
//!   containers.json                       # registry, written atomically
//!   profiles/<name-slug>-<id>/            # one dir per container (Servo data path)
//!     cookies.json
//!     net-cache/
//!     local-storage/                      # reserved for engine state
//!     history.jsonl
//! ```
//!
//! Container isolation: every [`ContainerStore`] is bound to exactly one
//! container's profile directory; cookie jars and caches load/save only
//! within it, so data written by container A is never visible to container B.

mod cache;
mod cookies;
mod registry;

pub use cache::{CachedEntry, NetCache, DEFAULT_MAX_ENTRIES, DEFAULT_MAX_TOTAL_BYTES};
pub use cookies::{Cookie, CookieJar};
pub use registry::{Store, StoreError};

use servfox_common::ContainerId;
use std::path::{Path, PathBuf};

/// Per-container disk store: cookies, cache, history — all scoped to one
/// container's profile directory.
pub struct ContainerStore {
    container: ContainerId,
    profile_dir: PathBuf,
}

impl ContainerStore {
    /// Bind to `id`'s profile directory inside `store` (creates it if needed).
    pub fn open(store: &Store, id: ContainerId) -> Self {
        Self {
            container: id,
            profile_dir: store.profile_dir(id),
        }
    }

    pub fn container(&self) -> ContainerId {
        self.container
    }

    pub fn profile_dir(&self) -> &Path {
        &self.profile_dir
    }

    /// Cookie jar for this container only, loaded from this profile's
    /// `cookies.json` with already-expired cookies pruned.
    pub fn cookie_jar(&self) -> CookieJar {
        CookieJar::load(&self.cookies_path())
    }

    /// In-memory bounded response cache for this container.
    pub fn cache(&self) -> NetCache {
        NetCache::new(self.profile_dir.join(cache::CACHE_DIR))
    }

    /// Append one navigation to this container's `history.jsonl`.
    pub fn append_history(&self, url: &str, title: Option<&str>) -> Result<(), StoreError> {
        use std::io::Write;
        let entry = serde_json::json!({
            "url": url,
            "title": title,
            "at": crate::now_unix(),
        });
        let mut line = entry.to_string();
        line.push('\n');
        let path = self.profile_dir.join("history.jsonl");
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| StoreError::io(&path, e))?;
        f.write_all(line.as_bytes())
            .map_err(|e| StoreError::io(&path, e))?;
        Ok(())
    }

    fn cookies_path(&self) -> PathBuf {
        self.profile_dir.join("cookies.json")
    }
}

pub(crate) fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// `$SERVFOX_DATA_DIR` if set, else `~/.local/share/servfox`.
pub fn default_data_root() -> PathBuf {
    if let Ok(dir) = std::env::var("SERVFOX_DATA_DIR") {
        return PathBuf::from(dir);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    Path::new(&home)
        .join(".local")
        .join("share")
        .join("servfox")
}
