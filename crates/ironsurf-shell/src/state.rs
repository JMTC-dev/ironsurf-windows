//! Per-tab state reduced from the `FromTab` message stream.
//!
//! Pure (no Slint, no processes): the shell's message pump applies every
//! decoded `FromTab` through [`TabState::reduce`] and then syncs the UI.
//! `Frame` messages never touch the state — they are handed to the frame
//! compositor directly (coalesced to the newest per tab).

use ironsurf_common::{FromTab, NetEvent};

/// Cap on retained DevTools console entries per tab: the oldest are dropped
/// once the ring is full, so a chatty page cannot grow shell memory forever.
pub const MAX_CONSOLE_ENTRIES: usize = 500;
/// Cap on retained DevTools network entries per tab (oldest dropped first).
/// The running log is never cleared on navigation — like Firefox's unticked
/// "Persist logs", it accumulates for the tab's lifetime.
pub const MAX_NETWORK_ENTRIES: usize = 500;

/// One DevTools console entry, reduced from [`FromTab::ConsoleLog`]. Stored
/// on [`TabState::console`] in arrival order; the (future) panel reads it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ConsoleEntry {
    /// Origin subsystem: `"js"` | `"css"` | `"dom"` | `"page"`.
    pub source: String,
    /// Severity label as the engine emitted it (`"log"`, `"warn"`, …).
    pub level: String,
    /// Rendered message text.
    pub message: String,
}

/// Everything the shell renders about one tab, minus the frame itself.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TabState {
    pub title: String,
    pub url: Option<String>,
    pub loading: bool,
    pub can_back: bool,
    pub can_forward: bool,
    pub status: Option<String>,
    pub ready: bool,
    pub closed: bool,
    pub error: Option<String>,
    /// DevTools console feed (bounded by [`MAX_CONSOLE_ENTRIES`]).
    pub console: Vec<ConsoleEntry>,
    /// DevTools network feed (bounded by [`MAX_NETWORK_ENTRIES`]).
    pub network: Vec<NetEvent>,
}

impl TabState {
    /// Apply one tab message. Returns `true` when anything observable
    /// changed (so the pump can skip redundant UI syncs).
    pub fn reduce(&mut self, msg: &FromTab) -> bool {
        match msg {
            FromTab::Ready { .. } => {
                let (was, was_err) = (self.ready, self.error.take());
                if was_err.is_some() {
                    return true;
                }
                self.ready = true;
                !was
            }
            FromTab::TitleChanged { title } => {
                let next = title.clone().unwrap_or_default();
                if next == self.title {
                    false
                } else {
                    self.title = next;
                    true
                }
            }
            FromTab::UrlChanged { url } => {
                if url == &self.url {
                    false
                } else {
                    self.url = url.clone();
                    true
                }
            }
            FromTab::LoadingChanged { loading } => {
                // A load that starts replaces the last thing the bar had to
                // say: an error from the previous attempt must never shadow the
                // new one (the UI shows `error` ahead of `status`).
                let started = *loading && !self.loading;
                let was_error = started.then(|| self.error.take()).flatten();
                let changed = *loading != self.loading;
                self.loading = *loading;
                changed || was_error.is_some()
            }
            FromTab::HistoryChanged { can_go_back, can_go_forward } => {
                let changed = *can_go_back != self.can_back
                    || *can_go_forward != self.can_forward;
                self.can_back = *can_go_back;
                self.can_forward = *can_go_forward;
                changed
            }
            FromTab::StatusText { text } => {
                if text == &self.status {
                    false
                } else {
                    self.status = text.clone();
                    true
                }
            }
            // DevTools feed (console + network): appended to bounded per-tab
            // logs the (future) panel reads. They never dirty the frame, so
            // reduce always reports `false` here regardless of acceptance.
            FromTab::ConsoleLog { source, level, message } => {
                push_bounded(
                    &mut self.console,
                    ConsoleEntry {
                        source: source.clone(),
                        level: level.clone(),
                        message: message.clone(),
                    },
                    MAX_CONSOLE_ENTRIES,
                );
                false
            }
            FromTab::NetworkEvent { event } => {
                push_bounded(&mut self.network, event.clone(), MAX_NETWORK_ENTRIES);
                false
            }
            // Actions for the pump (spawn a new tab, open the context menu,
            // file the copy into the session clipboard), not per-tab state.
            FromTab::OpenLinkInNewTab { .. } => false,
            FromTab::ContextMenu { .. } => false,
            FromTab::ClipboardText { .. } => false,
            FromTab::Frame { .. } => false,
            FromTab::Closed => {
                if self.closed {
                    false
                } else {
                    self.closed = true;
                    true
                }
            }
            FromTab::Error { message } => {
                self.error = Some(message.clone());
                self.loading = false;
                true
            }
        }
    }

    /// The status-bar security chip for the current URL: a lock for https,
    /// a warning for plain http, nothing for `about:`/`file:`/etc.
    pub fn security(&self) -> Security {
        Security::of(self.url.as_deref().unwrap_or(""))
    }
}

// ---- DevTools panel view-models ---------------------------------------------
//
// The panel (ui/ironsurf.slint) paints plain `Text` cells, so everything the
// renderer needs is pre-formatted here: this is the reduction the panel shows,
// kept pure (no Slint) so it is unit-testable headless and drives the UI through
// `app::Core::sync_ui`.

/// Placeholder for "the engine never said" cells in the panel tables (an em
/// dash, like Firefox's network table).
pub const MISSING: &str = "\u{2014}";

/// One rendered DevTools console row.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ConsoleRow {
    /// Severity label (`"log"` | `"info"` | `"warn"` | `"error"`): drives the
    /// panel's level icon and colour.
    pub level: String,
    /// `"[source] message"`, exactly as the panel paints it.
    pub text: String,
}

/// One rendered DevTools network row: every column pre-formatted, missing
/// values [`MISSING`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NetRow {
    pub method: String,
    pub url: String,
    /// Status code, or the failure text when the request never answered.
    pub status: String,
    pub size: String,
    pub time: String,
    /// `true` ⇒ the request failed ([`NetEvent::error`] is `Some`) and the
    /// panel paints `status` in red.
    pub failed: bool,
}

impl TabState {
    /// Console rows for the panel, in arrival order.
    pub fn console_rows(&self) -> Vec<ConsoleRow> {
        self.console
            .iter()
            .map(|e| ConsoleRow {
                level: e.level.clone(),
                text: format!("[{}] {}", e.source, e.message),
            })
            .collect()
    }

    /// Network rows for the panel, in arrival order.
    pub fn net_rows(&self) -> Vec<NetRow> {
        self.network
            .iter()
            .map(|e| NetRow {
                method: e.method.clone(),
                url: e.url.clone(),
                // A failed request has no status: say *why* instead (the panel
                // paints the whole cell red when `failed`).
                status: match (&e.status, &e.error) {
                    (Some(status), _) => status.to_string(),
                    (None, Some(err)) => err.clone(),
                    (None, None) => MISSING.to_string(),
                },
                size: format_size(e.response_bytes),
                time: format_duration(e.duration_ms),
                failed: e.error.is_some(),
            })
            .collect()
    }
}

/// Response size for the panel: bytes below 1 KiB, then decimal units
/// (`1.4 kB`, `2.0 MB`). Unknown (`None`) is [`MISSING`].
pub fn format_size(bytes: Option<u64>) -> String {
    const UNITS: [&str; 5] = ["B", "kB", "MB", "GB", "TB"];
    match bytes {
        None => MISSING.to_string(),
        Some(b) => {
            let mut value = b as f64;
            let mut unit = 0;
            while value >= 1024.0 && unit + 1 < UNITS.len() {
                value /= 1024.0;
                unit += 1;
            }
            if unit == 0 {
                format!("{b} B")
            } else {
                format!("{value:.1} {}", UNITS[unit])
            }
        }
    }
}

/// Request duration for the panel: whole ms below a second, seconds above.
/// Never measured (`None`) is [`MISSING`].
pub fn format_duration(ms: Option<u64>) -> String {
    match ms {
        None => MISSING.to_string(),
        Some(ms) if ms < 1000 => format!("{ms} ms"),
        Some(ms) => format!("{:.2} s", ms as f64 / 1000.0),
    }
}

/// Append `item`, dropping the oldest front entries until the vec is at most
/// `cap` long. Shared by the console and network logs.
fn push_bounded<T>(log: &mut Vec<T>, item: T, cap: usize) {
    log.push(item);
    if let Some(excess) = log.len().checked_sub(cap) {
        log.drain(..excess);
    }
}

/// Lock/http indicator rendered next to the status text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Security {
    Https,
    InsecureHttp,
    None,
}

impl Security {
    pub fn of(url: &str) -> Self {
        if url.starts_with("https:") {
            Security::Https
        } else if url.starts_with("http:") {
            Security::InsecureHttp
        } else {
            Security::None
        }
    }

    /// `(glyph, css color)` for the status bar.
    pub fn render(self) -> (&'static str, &'static str) {
        match self {
            // U+26EC, per ARCHITECTURE.md ("⛬ / http warning")
            Security::Https => ("⛬", "#8fce8f"),
            Security::InsecureHttp => ("⚠ http", "#e57373"),
            Security::None => ("", "#888888"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironsurf_common::TabId;

    #[test]
    fn reduce_full_message_sequence() {
        let mut s = TabState::default();
        assert!(s.reduce(&FromTab::Ready { tab_id: TabId(1) }));
        assert!(s.ready);

        assert!(s.reduce(&FromTab::UrlChanged { url: Some("https://example.com".into()) }));
        assert!(s.reduce(&FromTab::LoadingChanged { loading: true }));
        assert!(!s.reduce(&FromTab::LoadingChanged { loading: true }), "no-op must report false");
        assert!(s.reduce(&FromTab::TitleChanged { title: Some("Example".into()) }));
        assert!(s.reduce(&FromTab::HistoryChanged { can_go_back: true, can_go_forward: false }));
        assert!(s.reduce(&FromTab::LoadingChanged { loading: false }));
        assert!(s.reduce(&FromTab::StatusText { text: Some("Waiting…".into()) }));
        // frames carry no state
        assert!(!s.reduce(&FromTab::Frame {
            width: 2,
            height: 2,
            scale: 1.0,
            rgba: vec![0u8; 16],
        }));

        assert_eq!(s.title, "Example");
        assert_eq!(s.url.as_deref(), Some("https://example.com"));
        assert!(!s.loading);
        assert!(s.can_back);
        assert!(!s.can_forward);
        assert_eq!(s.status.as_deref(), Some("Waiting…"));

        assert!(s.reduce(&FromTab::Closed));
        assert!(s.closed);
        assert!(!s.reduce(&FromTab::Closed));
    }

    #[test]
    fn starting_a_load_drops_the_stale_error_so_status_text_shows() {
        let mut s = TabState::default();
        s.reduce(&FromTab::Error { message: "dns failed".into() });
        assert_eq!(s.error.as_deref(), Some("dns failed"));
        // The user clicks a link / retries: the new load must clear the bar of
        // the old failure, or "Waiting for …" is invisible behind it forever.
        assert!(s.reduce(&FromTab::LoadingChanged { loading: true }));
        assert_eq!(s.error, None, "a superseded error must not shadow the new status");
        assert!(s.loading);
        assert!(s.reduce(&FromTab::StatusText {
            text: Some("Waiting for example.com".into())
        }));
        assert_eq!(s.status.as_deref(), Some("Waiting for example.com"));
        // …and the status is replaced, never appended to.
        assert!(s.reduce(&FromTab::StatusText { text: None }));
        assert_eq!(s.status, None);
        assert!(!s.reduce(&FromTab::StatusText { text: None }));
    }

    #[test]
    fn error_clears_loading_and_records_message() {
        let mut s = TabState::default();
        s.reduce(&FromTab::LoadingChanged { loading: true });
        assert!(s.reduce(&FromTab::Error { message: "boom".into() }));
        assert_eq!(s.error.as_deref(), Some("boom"));
        assert!(!s.loading);
    }

    #[test]
    fn console_logs_append_in_order_and_never_dirty_the_frame() {
        let mut s = TabState::default();
        assert!(s.console.is_empty());
        for (i, src) in ["js", "css", "dom", "page"].iter().enumerate() {
            assert!(
                !s.reduce(&FromTab::ConsoleLog {
                    source: (*src).into(),
                    level: "warn".into(),
                    message: format!("m{i}"),
                }),
                "console logs must not dirty the frame"
            );
        }
        // Appended in arrival order, all fields carried through verbatim.
        assert_eq!(s.console.len(), 4);
        assert_eq!(
            s.console,
            vec![
                ConsoleEntry {
                    source: "js".into(),
                    level: "warn".into(),
                    message: "m0".into()
                },
                ConsoleEntry {
                    source: "css".into(),
                    level: "warn".into(),
                    message: "m1".into()
                },
                ConsoleEntry {
                    source: "dom".into(),
                    level: "warn".into(),
                    message: "m2".into()
                },
                ConsoleEntry {
                    source: "page".into(),
                    level: "warn".into(),
                    message: "m3".into()
                },
            ]
        );
    }

    #[test]
    fn console_log_ring_drops_oldest_at_the_cap() {
        let mut s = TabState::default();
        for i in 0..(MAX_CONSOLE_ENTRIES + 25) {
            s.reduce(&FromTab::ConsoleLog {
                source: "js".into(),
                level: "log".into(),
                message: format!("{i}"),
            });
        }
        assert_eq!(s.console.len(), MAX_CONSOLE_ENTRIES, "cap must be enforced");
        // The 25 oldest were dropped; the tail is the freshest entries.
        assert_eq!(s.console.first().unwrap().message, "25");
        assert_eq!(
            s.console.last().unwrap().message,
            (MAX_CONSOLE_ENTRIES + 24).to_string()
        );
    }

    #[test]
    fn network_events_append_and_never_dirty_the_frame() {
        let mut s = TabState::default();
        assert!(s.network.is_empty());
        // A success event, then a failure event — both must land on the log.
        assert!(
            !s.reduce(&FromTab::NetworkEvent {
                event: net_event("GET", "https://example.com", Some(200), None)
            }),
            "network events must not dirty the frame"
        );
        assert!(!s.reduce(&FromTab::NetworkEvent {
            event: net_event("GET", "https://evil.test", None, Some("dns failed".into()))
        }));
        assert_eq!(s.network.len(), 2);
        assert_eq!(s.network[0].url, "https://example.com");
        assert_eq!(s.network[0].status, Some(200));
        assert_eq!(s.network[0].error, None);
        assert_eq!(s.network[1].status, None);
        assert_eq!(s.network[1].error.as_deref(), Some("dns failed"));
    }

    #[test]
    fn network_log_ring_drops_oldest_at_the_cap() {
        let mut s = TabState::default();
        for i in 0..(MAX_NETWORK_ENTRIES + 10) {
            s.reduce(&FromTab::NetworkEvent {
                event: net_event("GET", &format!("https://e.test/{i}"), Some(200), None),
            });
        }
        assert_eq!(s.network.len(), MAX_NETWORK_ENTRIES, "cap must be enforced");
        assert_eq!(s.network.first().unwrap().url, "https://e.test/10");
        assert_eq!(
            s.network.last().unwrap().url,
            format!("https://e.test/{}", MAX_NETWORK_ENTRIES + 9)
        );
    }

    #[test]
    fn devtools_feed_runs_alongside_normal_state() {
        // DevTools messages must leave the rest of the reduce behaviour intact
        // (title/loading/status unchanged) while still accumulating.
        let mut s = TabState::default();
        s.reduce(&FromTab::Ready { tab_id: TabId(1) });
        s.reduce(&FromTab::UrlChanged { url: Some("https://example.com".into()) });
        s.reduce(&FromTab::ConsoleLog {
            source: "js".into(),
            level: "error".into(),
            message: "boom".into(),
        });
        s.reduce(&FromTab::NetworkEvent {
            event: net_event("POST", "https://example.com/api", Some(204), None),
        });
        assert!(s.ready);
        assert_eq!(s.url.as_deref(), Some("https://example.com"));
        assert_eq!(s.console.len(), 1);
        assert_eq!(s.network.len(), 1);
        assert_eq!(s.network[0].method, "POST");
    }

    fn net_event(
        method: &str,
        url: &str,
        status: Option<u16>,
        error: Option<String>,
    ) -> ironsurf_common::NetEvent {
        ironsurf_common::NetEvent {
            method: method.into(),
            url: url.into(),
            status,
            response_bytes: status.map(|_| 1234),
            duration_ms: status.map(|_| 42),
            error,
        }
    }

    #[test]
    fn console_rows_carry_the_level_and_a_bracketed_source() {
        let mut s = TabState::default();
        s.reduce(&FromTab::ConsoleLog {
            source: "js".into(),
            level: "error".into(),
            message: "boom".into(),
        });
        s.reduce(&FromTab::ConsoleLog {
            source: "css".into(),
            level: "warn".into(),
            message: "bad selector".into(),
        });
        assert_eq!(
            s.console_rows(),
            vec![
                ConsoleRow { level: "error".into(), text: "[js] boom".into() },
                ConsoleRow { level: "warn".into(), text: "[css] bad selector".into() },
            ]
        );
    }

    #[test]
    fn net_rows_pre_format_every_column() {
        let mut s = TabState::default();
        s.reduce(&FromTab::NetworkEvent {
            event: net_event("GET", "https://example.com", Some(200), None),
        });
        s.reduce(&FromTab::NetworkEvent {
            event: ironsurf_common::NetEvent {
                method: "POST".into(),
                url: "https://api.example/upload".into(),
                status: None,
                response_bytes: None,
                duration_ms: Some(1500),
                error: Some("dns failed".into()),
            },
        });
        let rows = s.net_rows();
        assert_eq!(
            rows[0],
            NetRow {
                method: "GET".into(),
                url: "https://example.com".into(),
                status: "200".into(),
                // the fixture event is 1234 B / 42 ms
                size: "1.2 kB".into(),
                time: "42 ms".into(),
                failed: false,
            }
        );
        // A failure speaks for itself in the status column, in red.
        assert_eq!(rows[1].status, "dns failed");
        assert!(rows[1].failed);
        assert_eq!(rows[1].size, MISSING);
        assert_eq!(rows[1].time, "1.50 s");
    }

    #[test]
    fn panel_formatters_cover_missing_values_and_units() {
        assert_eq!(format_size(None), MISSING);
        assert_eq!(format_size(Some(0)), "0 B");
        assert_eq!(format_size(Some(999)), "999 B");
        assert_eq!(format_size(Some(1024)), "1.0 kB");
        assert_eq!(format_size(Some(5 * 1024 * 1024)), "5.0 MB");
        assert_eq!(format_duration(None), MISSING);
        assert_eq!(format_duration(Some(0)), "0 ms");
        assert_eq!(format_duration(Some(999)), "999 ms");
        assert_eq!(format_duration(Some(1000)), "1.00 s");
    }

    #[test]
    fn security_chips() {
        assert_eq!(Security::of("https://example.com"), Security::Https);
        assert_eq!(Security::of("http://example.com"), Security::InsecureHttp);
        assert_eq!(Security::of("about:blank"), Security::None);
        assert_eq!(Security::of(""), Security::None);
        assert_eq!(Security::Https.render().0, "⛬");
        assert!(Security::InsecureHttp.render().0.contains("http"));
    }
}
