//! IronSurf disk layer: container registry (including each container's
//! optional per-container User-Agent), global userscripts, per-container
//! profile stores, an RFC6265-subset cookie jar, a bounded LRU network cache,
//! and history.
//!
//! No engine dependency — pure files under a data root:
//!
//! ```text
//! $IRONSURF_DATA_DIR (or ~/.local/share/ironsurf)
//!   containers.json                       # registry, written atomically
//!   userscripts.json                      # global userscript list, atomic
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

pub use cache::{
    fresh_until, CachedEntry, NetCache, DEFAULT_MAX_ENTRIES, DEFAULT_MAX_TOTAL_BYTES,
};
pub use cookies::{Cookie, CookieJar};
pub use registry::{Store, StoreError, MAX_USERSCRIPT_CODE_BYTES, MAX_USERSCRIPT_NAME_CHARS};

use ironsurf_common::ContainerId;
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

    /// Bind directly to an existing profile directory (used by `ironsurf-tab`,
    /// which receives the profile path from the shell and does not need the
    /// full registry).
    pub fn from_profile_dir(container: ContainerId, profile_dir: &std::path::Path) -> Self {
        let profile_dir = profile_dir.to_path_buf();
        let _ = std::fs::create_dir_all(&profile_dir);
        Self {
            container,
            profile_dir,
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

/// `$IRONSURF_DATA_DIR` if set, else `~/.local/share/ironsurf`.
pub fn default_data_root() -> PathBuf {
    if let Ok(dir) = std::env::var("IRONSURF_DATA_DIR") {
        return PathBuf::from(dir);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    Path::new(&home)
        .join(".local")
        .join("share")
        .join("ironsurf")
}
