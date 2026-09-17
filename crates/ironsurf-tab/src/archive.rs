//! Archive-mode sink (the shell's 💾 toggle, tab side).
//!
//! While the mode is on, every successful http(s) response the tab fetches —
//! main documents, `<script src>` / image / CSS subresources, and media —
//! is pushed into a pending queue (`store`, callable from the net-worker
//! threads) and drained by the poll loop into `FromTab::ArchivedFetch`
//! messages, exactly like the DevTools `net_events` queue. The **shell**
//! owns the archive root, the per-tab ISO8601 timestamp directory, and all
//! writing — the tab never touches the archive folder (it stays inside its
//! sandbox).
//!
//! The toggle is cheap on the hot path: one relaxed atomic load per fetched
//! response when off; `store` is a no-op for non-http schemes and while off.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use url::Url;

/// One archived response, awaiting its `FromTab::ArchivedFetch`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArchivedFetch {
    /// The final (post-redirect) request URL.
    pub url: String,
    /// True for a main-document navigation; false for every other body
    /// (subresource, script-src, media). The shell owns the layout: a
    /// query-less document is the host's `<host>/index.html`, and a URL that
    /// carries a query is stored under the full URL (the query names the
    /// file, e.g. `<host>/html/?q=asdf`).
    pub is_document: bool,
    /// The response body, verbatim.
    pub bytes: Vec<u8>,
}

/// The tab-wide archive sink: one `Arc` shared by the poll loop, the
/// [`crate::netmod::BlitzNetProvider`], the script-src fetcher, and the
/// media worker; toggled by `--archive-mode` (spawn) and
/// `ToTab::SetArchiveMode` (live).
#[derive(Debug, Default)]
pub struct ArchiveState {
    enabled: AtomicBool,
    pending: Mutex<Vec<ArchivedFetch>>,
}

impl ArchiveState {
    /// Off unless the tab was spawned with archive mode already on.
    pub fn new(enabled: bool) -> Self {
        ArchiveState {
            enabled: AtomicBool::new(enabled),
            pending: Mutex::new(Vec::new()),
        }
    }

    /// Shared handle (`Arc` for the provider and the worker threads).
    pub fn shared(enabled: bool) -> Arc<Self> {
        Arc::new(Self::new(enabled))
    }

    /// The `ToTab::SetArchiveMode` arm. Turning the mode off also drops any
    /// not-yet-shipped entries: a fetch that settled while on still finishes
    /// its way to the shell, but nothing queued before a later re-enable
    /// rides over (each enable starts a fresh run).
    pub fn set_enabled(&self, enabled: bool) {
        let was = self.enabled.swap(enabled, Ordering::Relaxed);
        if was != enabled && !enabled {
            self.pending.lock().expect("archive mutex").clear();
        }
    }

    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    /// Queue one settled response body for shipping. No-op while off, and
    /// for non-http(s) URLs (`data:`, `file:`, `about:` — a local asset is
    /// not a file "loaded from a website").
    pub fn store(&self, url: &Url, is_document: bool, bytes: &[u8]) {
        if !self.enabled() || !matches!(url.scheme(), "http" | "https") {
            return;
        }
        self.pending
            .lock()
            .expect("archive mutex")
            .push(ArchivedFetch {
                url: url.as_str().to_owned(),
                is_document,
                bytes: bytes.to_vec(),
            });
    }

    /// Everything queued since the last drain (the poll loop ships one
    /// `FromTab::ArchivedFetch` per entry).
    pub fn take_pending(&self) -> Vec<ArchivedFetch> {
        std::mem::take(&mut *self.pending.lock().expect("archive mutex"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(s: &str) -> Url {
        Url::parse(s).expect("test url")
    }

    #[test]
    fn off_by_default_stores_nothing() {
        let state = ArchiveState::new(false);
        state.store(&url("https://example.com/"), true, b"<html>");
        assert!(state.take_pending().is_empty());
    }

    #[test]
    fn on_stores_documents_and_skips_local_schemes() {
        let state = ArchiveState::new(true);
        state.store(&url("https://example.com/"), true, b"<html>");
        state.store(&url("http://127.0.0.1:8000/img.png"), false, b"PNG");
        state.store(&url("data:text/html,hi"), false, b"hi");
        state.store(&url("file:///etc/passwd"), false, b"x");
        let pending = state.take_pending();
        assert_eq!(pending.len(), 2);
        assert_eq!(
            pending[0],
            ArchivedFetch {
                url: "https://example.com/".into(),
                is_document: true,
                bytes: b"<html>".to_vec()
            }
        );
        assert_eq!(pending[1].url, "http://127.0.0.1:8000/img.png");
        // Drained once: the next drain is empty until more arrivals.
        assert!(state.take_pending().is_empty());
    }

    #[test]
    fn toggling_off_drops_the_queue_and_on_resumes() {
        let state = ArchiveState::shared(false);
        state.set_enabled(true);
        state.store(&url("https://example.com/a"), false, b"a");
        state.set_enabled(false);
        assert!(state.take_pending().is_empty(), "a disabled mode ships nothing");
        state.store(&url("https://example.com/b"), false, b"b");
        assert!(state.take_pending().is_empty());
        state.set_enabled(true);
        state.store(&url("https://example.com/c"), false, b"c");
        assert_eq!(state.take_pending().len(), 1);
    }
}
