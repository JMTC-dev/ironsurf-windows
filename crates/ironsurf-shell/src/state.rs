//! Per-tab state reduced from the `FromTab` message stream.
//!
//! Pure (no Slint, no processes): the shell's message pump applies every
//! decoded `FromTab` through [`TabState::reduce`] and then syncs the UI.
//! `Frame` messages never touch the state — they are handed to the frame
//! compositor directly (coalesced to the newest per tab).

use ironsurf_common::FromTab;

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
                if *loading == self.loading {
                    false
                } else {
                    self.loading = *loading;
                    true
                }
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
            FromTab::ConsoleLog { .. } => false,
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
    fn error_clears_loading_and_records_message() {
        let mut s = TabState::default();
        s.reduce(&FromTab::LoadingChanged { loading: true });
        assert!(s.reduce(&FromTab::Error { message: "boom".into() }));
        assert_eq!(s.error.as_deref(), Some("boom"));
        assert!(!s.loading);
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
