//! The popup blocker — the tab half (task: block popups, i.e. a website
//! opening new tabs without a user interaction).
//!
//! Two questions decide every request, and nothing else does:
//!
//! 1. **Does the policy allow it?** [`Policy`] is what the shell pushed in
//!    ([`ironsurf_common::ToTab::SetPopupPolicy`]): a master switch plus a list
//!    of document origins the user allowed explicitly. A tab that was never
//!    told anything blocks with an empty list — the safe reading is the
//!    default, so a shell that knows nothing about popups still blocks.
//! 2. **Did a user just ask for this?** [`Engagement`] is armed by real input
//!    (a pointer press or a key press the shell forwarded, which by
//!    construction came from a human): the gesture keeps the door open for
//!    [`ACTIVATION_WINDOW`] and buys [`MAX_OPEN_PER_ACTIVATION`] windows. That
//!    is the whole trick — it is what separates the `<a>` whose click handler
//!    calls `window.open` (allowed, the user clicked) from the page that opens
//!    a tab from `setTimeout` or on `load` (blocked, nobody clicked), and it is
//!    Firefox's `dom.disable_open_click_delay` model rather than a pretence of
//!    full transient-activation tracking: this engine has no event-loop task
//!    identities to hang a real activation on.
//!
//! The gate itself ([`PopupHook`]) is one shared cell: the `window.open` native
//! ([`crate::js`]) has to answer `null`/a window **synchronously**, so the
//! decision runs inside the JS call, while the *consequences* — "shell, open
//! this" / "shell, tell the user I refused" — are queued here and drained by
//! the poll loop ([`PopupHook::take_pending`]), exactly like the
//! `NavigationProvider` hook drains a link click. The tab never spawns
//! anything: tabs belong to the shell.
//!
//! Blocked is not silently dropped: every refusal becomes
//! [`ironsurf_common::FromTab::PopupBlocked`] so the user can see it happened
//! and allow that site.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use url::Url;

use crate::runtime::SelfPipe;

/// How long a real gesture keeps the door open. Five seconds, the same window
/// Firefox ships as `dom.disable_open_click_delay`: long enough for the
/// "clicked a link, the site fetched a token, now it opens the window" flows
/// that exist in the wild, short enough that a page cannot park a click and
/// spam later.
pub const ACTIVATION_WINDOW: Duration = Duration::from_secs(5);
/// Windows one gesture buys. One is what "the user clicked, so this click may
/// open a window" means; a second click arms a second one. Everything past the
/// first is a page deciding on its own how many tabs the user wanted.
pub const MAX_OPEN_PER_ACTIVATION: u32 = 1;
/// Origins the shell can hand over at once (the allow-list is user data, not
/// an unbounded list, and it travels on every policy message).
pub const MAX_ALLOWED_ORIGINS: usize = 64;

/// What asked for the window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PopupVia {
    /// Page script: `window.open(…)`, or a script activating an
    /// `<a target="_blank">` (`el.click()`). Needs a gesture.
    Script,
    /// The **user** clicked an `<a target="_blank">`. A click is the user
    /// interaction the blocker exists to wait for, so this never needs the
    /// engagement budget.
    Link,
}

/// One window a page asked for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PopupRequest {
    /// Target URL, already resolved against the document URL.
    pub url: String,
    /// The `target` the page named (`"_blank"` or a window name), `None` when
    /// it named none. Carried for the log and the notice; IronSurf opens tabs,
    /// so a name is never honoured as a reusable window.
    pub target: Option<String>,
    pub via: PopupVia,
}

/// The answer a caller gets **synchronously**.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    Allowed,
    Blocked,
}

impl Decision {
    pub fn allowed(self) -> bool {
        self == Decision::Allowed
    }
}

/// The rules the shell owns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Policy {
    /// `true` = block everything a gesture did not authorize.
    pub block: bool,
    /// Origins whose popups are never blocked, as ASCII serializations
    /// (`https://example.com`), kept normalized, deduped and capped.
    pub allowed: Vec<String>,
}

impl Default for Policy {
    /// Block, allow nobody: what a tab holds until the shell says otherwise.
    fn default() -> Self {
        Policy { block: true, allowed: Vec::new() }
    }
}

impl Policy {
    /// Normalize-and-clean the shell's list: entries that are not a URL or a
    /// host name are dropped rather than stored broken, duplicates collapse,
    /// and the list is capped at [`MAX_ALLOWED_ORIGINS`].
    pub fn new(block: bool, allowed: &[String]) -> Self {
        let mut seen = Vec::new();
        for raw in allowed {
            let Some(origin) = normalize_origin(raw) else {
                continue;
            };
            if !seen.contains(&origin) {
                seen.push(origin);
            }
            if seen.len() >= MAX_ALLOWED_ORIGINS {
                break;
            }
        }
        Policy { block, allowed: seen }
    }

    /// Does the allow-list cover `origin`? A document with no comparable
    /// origin (`about:blank`, a `data:` page) matches nothing — its popups are
    /// judged by gesture alone.
    pub fn allows(&self, origin: Option<&str>) -> bool {
        match origin {
            Some(origin) => self.allowed.iter().any(|entry| entry == origin),
            None => false,
        }
    }
}

/// `https://Example.COM:8443/a?b#c` → `https://example.com:8443`. A bare host
/// (`example.com`) is read as an `https` origin, since that is what a user
/// typing a site name means. `None` = not a site name at all.
pub fn normalize_origin(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.contains(char::is_whitespace) {
        return None;
    }
    let candidate = if trimmed.contains("://") {
        trimmed.to_owned()
    } else {
        format!("https://{trimmed}")
    };
    let parsed = Url::parse(&candidate).ok()?;
    if parsed.host_str().is_none() {
        return None;
    }
    let origin = parsed.origin().ascii_serialization();
    (!origin.is_empty() && origin != "null").then_some(origin)
}

/// Does a `target` attribute ask for a **new window**? `_self`, `_top`,
/// `_parent`, `""` and no attribute at all mean “this tab” (this engine has one
/// top-level browsing context per tab, so the three names really are this tab),
/// and only `_blank` or a window name means another one. The name is capped — it
/// is a window name, not a payload. Mirrored by `popupTargetName` in the JS
/// prelude, which decides the same question for `window.open` and `el.click()`
/// (task: block popups).
pub fn new_window_target(raw: Option<impl AsRef<str>>) -> Option<String> {
    let name = raw.map(|value| value.as_ref().trim().to_owned())?;
    if name.is_empty()
        || name.eq_ignore_ascii_case("_self")
        || name.eq_ignore_ascii_case("_top")
        || name.eq_ignore_ascii_case("_parent")
    {
        return None;
    }
    Some(name.chars().take(120).collect())
}

/// Trim a URL the tab reports to the wire's budget (task: popup blocker): the
/// notice is one line of UI and a `data:` target is arbitrarily large. Cuts on a
/// char boundary and marks the cut with an ellipsis.
pub fn cap_reported_url(url: &str) -> String {
    if url.len() <= ironsurf_common::MAX_POPUP_URL_CHARS {
        return url.to_owned();
    }
    let mut cut = ironsurf_common::MAX_POPUP_URL_CHARS;
    while !url.is_char_boundary(cut) {
        cut -= 1;
    }
    let mut capped = url[..cut].to_owned();
    capped.push('…');
    capped
}

/// Is `url` a target a tab could actually load? Four schemes are not, so asking
/// for one as a **new window** is never legitimate however the rest of the
/// policy reads: `javascript:` runs script in the target instead of loading a
/// document, `data:` and `blob:` carry a whole document inside the URL (no
/// origin, arbitrarily large), and `file:` reaches the local disk from a web
/// page. Everything else — `http(s)`, `about:blank`, `mailto:`, a scheme a site
/// invented — is judged by policy like any other request (task: block popups).
pub fn loadable_target(url: &str) -> bool {
    match url.split_once(':') {
        Some((scheme, _)) => !matches!(
            scheme.to_ascii_lowercase().as_str(),
            "javascript" | "data" | "blob" | "file"
        ),
        // No colon at all: a relative or scheme-less target the caller did not
        // resolve. Not one of the refused schemes.
        None => true,
    }
}

/// The origin a popup request is judged against (`None` = opaque).
pub fn document_origin(url: &Url) -> Option<String> {
    match url.origin() {
        origin if origin.is_tuple() => {
            let serialized = origin.ascii_serialization();
            (!serialized.is_empty() && serialized != "null").then_some(serialized)
        }
        _ => None,
    }
}

/// User activation: armed by real input, spent by an allowed window.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Engagement {
    armed: Option<Instant>,
    spent: u32,
}

impl Engagement {
    /// A human pressed a button or a key. Re-arming resets the budget: a new
    /// gesture buys a fresh window even if the last one was spent seconds ago.
    pub fn arm(&mut self, now: Instant) {
        self.armed = Some(now);
        self.spent = 0;
    }

    /// Spend one window of the current gesture, if any is left and unexpired.
    pub fn take(&mut self, now: Instant) -> bool {
        let Some(armed) = self.armed else { return false };
        if now.duration_since(armed) > ACTIVATION_WINDOW || self.spent >= MAX_OPEN_PER_ACTIVATION
        {
            return false;
        }
        self.spent += 1;
        true
    }

    /// Windows this gesture still buys (diagnostics and tests).
    pub fn remaining(&self, now: Instant) -> u32 {
        match self.armed {
            Some(armed) if now.duration_since(armed) <= ACTIVATION_WINDOW => {
                MAX_OPEN_PER_ACTIVATION.saturating_sub(self.spent)
            }
            _ => 0,
        }
    }

    /// Forget the gesture (a navigation: a fresh document starts unengaged —
    /// the click that got the user here belongs to the page they left).
    pub fn clear(&mut self) {
        self.armed = None;
        self.spent = 0;
    }
}

/// Gate state: policy + engagement + the two queues the poll loop ships.
#[derive(Clone, Debug, Default)]
pub struct PopupState {
    policy: Policy,
    engagement: Engagement,
    origin: Option<String>,
    allowed: Vec<PopupRequest>,
    blocked: Vec<PopupRequest>,
}

/// Both queues at once (what [`PopupState::take_pending`] hands back).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Pending {
    /// Requests the shell should open (in a new tab, same container).
    pub allowed: Vec<PopupRequest>,
    /// Requests the tab refused — report them, never open them.
    pub blocked: Vec<PopupRequest>,
}

impl Pending {
    pub fn is_empty(&self) -> bool {
        self.allowed.is_empty() && self.blocked.is_empty()
    }
}

impl PopupState {
    /// Replace the policy wholesale (the shell always sends both halves).
    pub fn configure(&mut self, block: bool, allowed: Vec<String>) {
        self.policy = Policy::new(block, &allowed);
    }

    pub fn policy(&self) -> &Policy {
        &self.policy
    }

    /// A new document: its origin is what requests are matched against now,
    /// and the previous page's gesture does not carry over.
    pub fn set_document(&mut self, url: &Url) {
        self.origin = document_origin(url);
        self.engagement.clear();
    }

    /// The origin requests are matched against (tests, `about:blank`).
    pub fn set_origin(&mut self, origin: Option<String>) {
        self.origin = origin;
    }

    pub fn origin(&self) -> Option<&str> {
        self.origin.as_deref()
    }

    /// Real input arrived: the page gets one gesture's worth of credit.
    pub fn arm(&mut self, now: Instant) {
        self.engagement.arm(now);
    }

    pub fn remaining(&self, now: Instant) -> u32 {
        self.engagement.remaining(now)
    }

    /// Decide a request and queue it on the right side. Deterministic clock
    /// parameter so every rule is testable without sleeping.
    pub fn request_at(
        &mut self,
        url: &str,
        target: Option<String>,
        via: PopupVia,
        now: Instant,
    ) -> Decision {
        let decision = self.decide_at(url, via, now);
        let request =
            PopupRequest { url: url.to_owned(), target, via };
        match decision {
            Decision::Allowed => self.allowed.push(request),
            Decision::Blocked => self.blocked.push(request),
        }
        decision
    }

    /// Decide without queueing (the rule, spelled out).
    pub fn decide_at(&mut self, url: &str, via: PopupVia, now: Instant) -> Decision {
        if !loadable_target(url) {
            // A `javascript:`/`data:`/`blob:`/`file:` "window" is refused before
            // any policy is consulted: it is not a page, and no site allow-list
            // or user gesture makes script-in-a-URL into a document.
            return Decision::Blocked;
        }
        if !self.policy.block {
            return Decision::Allowed;
        }
        if self.policy.allows(self.origin.as_deref()) {
            return Decision::Allowed;
        }
        if via == PopupVia::Link {
            // The user clicked this exact link; that is the interaction the
            // blocker waits for, and it is not charged to the budget.
            return Decision::Allowed;
        }
        if self.engagement.take(now) {
            return Decision::Allowed;
        }
        Decision::Blocked
    }

    /// Take everything the poll loop has to report.
    pub fn take_pending(&mut self) -> Pending {
        Pending {
            allowed: std::mem::take(&mut self.allowed),
            blocked: std::mem::take(&mut self.blocked),
        }
    }
}

/// The shared cell: page JavaScript decides through it, the tab loop drains
/// it. `pipe` wakes the poll loop the instant a request lands, so an allowed
/// popup is a new tab inside the same frame rather than a tick later (the
/// NavHook/NetWaker pattern).
pub struct PopupHook {
    pipe: Option<Arc<SelfPipe>>,
    pub state: Mutex<PopupState>,
}

/// The handle the JS host and the tab both hold.
pub type SharedPopupHook = Arc<PopupHook>;

impl PopupHook {
    /// A gate with nobody to wake (bare JS tests, the headless harness).
    pub fn detached() -> Self {
        PopupHook { pipe: None, state: Mutex::new(PopupState::default()) }
    }

    pub fn with_pipe(pipe: Arc<SelfPipe>) -> Self {
        PopupHook { pipe: Some(Arc::clone(&pipe)), state: Mutex::new(PopupState::default()) }
    }

    fn wake(&self) {
        if let Some(pipe) = self.pipe.as_ref() {
            pipe.wake();
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, PopupState> {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn configure(&self, block: bool, allowed: Vec<String>) {
        self.lock().configure(block, allowed);
        // A policy the user just changed can un-block requests already queued?
        // No: queues hold decisions already taken. Nothing to wake for.
    }

    pub fn set_document(&self, url: &Url) {
        self.lock().set_document(url);
    }

    /// Real user input (pointer press / key press).
    pub fn arm(&self) {
        self.lock().arm(Instant::now());
    }

    pub fn remaining(&self) -> u32 {
        self.lock().remaining(Instant::now())
    }

    /// The synchronous question (called from the `window.open` native).
    pub fn request(&self, url: &str, target: Option<String>, via: PopupVia) -> Decision {
        let decision = self.request_at(url, target, via, Instant::now());
        self.wake();
        decision
    }

    pub fn request_at(
        &self,
        url: &str,
        target: Option<String>,
        via: PopupVia,
        now: Instant,
    ) -> Decision {
        self.lock().request_at(url, target, via, now)
    }

    pub fn take_pending(&self) -> Pending {
        self.lock().take_pending()
    }
}

impl std::fmt::Debug for PopupHook {
    /// By hand: the wake pipe is a pair of file descriptors and has nothing to
    /// show.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PopupHook")
            .field("wired", &self.pipe.is_some())
            .field("state", &self.state)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(raw: &str) -> Url {
        Url::parse(raw).expect("test url parses")
    }

    fn gate(block: bool, allowed: &[&str]) -> PopupHook {
        let gate = PopupHook::detached();
        gate.configure(
            block,
            allowed.iter().map(|entry| (*entry).to_owned()).collect(),
        );
        gate
    }

    #[test]
    fn a_script_window_open_is_blocked_when_nobody_clicked() {
        let gate = gate(true, &[]);
        gate.state.lock().unwrap().set_document(&url("https://example.com/a"));
        let now = Instant::now();
        assert_eq!(
            gate.request_at("https://ads.test/x", None, PopupVia::Script, now),
            Decision::Blocked,
            "a page may not open a tab on its own"
        );
        let pending = gate.take_pending();
        assert!(pending.allowed.is_empty());
        assert_eq!(pending.blocked.len(), 1);
        assert_eq!(pending.blocked[0].url, "https://ads.test/x");
    }

    #[test]
    fn a_recent_click_allows_one_window_and_no_more() {
        let gate = gate(true, &[]);
        let now = Instant::now();
        {
            let mut state = gate.state.lock().unwrap();
            state.set_document(&url("https://shop.test/cart"));
            state.arm(now);
        }
        assert_eq!(
            gate.request_at("https://shop.test/checkout", None, PopupVia::Script, now),
            Decision::Allowed,
            "the user clicked, so this click may open its window"
        );
        assert_eq!(
            gate.request_at("https://shop.test/second", None, PopupVia::Script, now),
            Decision::Blocked,
            "one click does not mean the page decides how many tabs"
        );
        let pending = gate.take_pending();
        assert_eq!(pending.allowed.len(), 1);
        assert_eq!(pending.blocked.len(), 1);
    }

    #[test]
    fn the_click_window_closes_after_five_seconds_but_a_new_click_arms_again() {
        let gate = gate(true, &[]);
        let now = Instant::now();
        gate.state.lock().unwrap().arm(now);
        let late = now + ACTIVATION_WINDOW + Duration::from_millis(1);
        assert_eq!(
            gate.request_at("https://ads.test/x", None, PopupVia::Script, late),
            Decision::Blocked,
            "a stale click is not a user interaction any more"
        );
        gate.state.lock().unwrap().arm(late);
        assert_eq!(
            gate.request_at("https://ads.test/y", None, PopupVia::Script, late),
            Decision::Allowed,
            "a fresh click buys a fresh window"
        );
    }

    #[test]
    fn a_user_click_on_a_target_blank_link_needs_no_budget() {
        let gate = gate(true, &[]);
        let now = Instant::now();
        // No arm at all: the click itself IS the interaction.
        assert_eq!(
            gate.request_at(
                "https://docs.test/page",
                Some("_blank".to_owned()),
                PopupVia::Link,
                now
            ),
            Decision::Allowed
        );
    }

    #[test]
    fn an_allowed_origin_opens_with_no_gesture_at_all() {
        let gate = gate(true, &["https://shop.test"]);
        let mut state = gate.state.lock().unwrap();
        state.set_origin(Some("https://shop.test".to_owned()));
        drop(state);
        assert_eq!(
            gate.request_at(
                "https://shop.test/invoice",
                None,
                PopupVia::Script,
                Instant::now()
            ),
            Decision::Allowed
        );
        // …and only that origin: a subdomain is a different site.
        let mut state = gate.state.lock().unwrap();
        state.set_origin(Some("https://checkout.shop.test".to_owned()));
        drop(state);
        assert_eq!(
            gate.request_at("https://shop.test/x", None, PopupVia::Script, Instant::now()),
            Decision::Blocked
        );
    }

    #[test]
    fn switching_the_master_switch_off_blocks_nothing() {
        let gate = gate(false, &[]);
        assert_eq!(
            gate.request_at("https://ads.test/x", None, PopupVia::Script, Instant::now()),
            Decision::Allowed
        );
    }

    #[test]
    fn a_new_document_starts_unengaged() {
        let gate = gate(true, &[]);
        let now = Instant::now();
        gate.state.lock().unwrap().arm(now);
        assert_eq!(gate.state.lock().unwrap().remaining(now), 1);
        gate.state.lock().unwrap().set_document(&url("https://other.test/"));
        assert_eq!(
            gate.state.lock().unwrap().remaining(now),
            0,
            "the click that got the user to a new page belongs to the old page"
        );
    }

    #[test]
    fn origins_are_normalized_deduped_and_junk_dropped() {
        let policy = Policy::new(
            true,
            &[
                "  HTTPS://Example.COM:8443/path?q=1  ".to_owned(),
                "example.com".to_owned(),
                "https://example.com/other".to_owned(),
                "https://ok.test".to_owned(),
                "not a site".to_owned(),
                "".to_owned(),
                "https://late.test".to_owned(),
            ],
        );
        assert_eq!(
            policy.allowed,
            vec![
                "https://example.com:8443".to_owned(),
                "https://example.com".to_owned(),
                "https://ok.test".to_owned(),
                "https://late.test".to_owned()
            ],
            "case, path and query collapse to the origin, a bare host reads as https, duplicates \
             merge, and junk never enters the list — but a different port is a different site, so \
             `:8443` stays its own entry"
        );
        assert!(policy.allows(Some("https://example.com")));
        assert!(policy.allows(Some("https://example.com:8443")));
        assert!(!policy.allows(Some("http://example.com")), "the scheme is part of the origin");
        assert!(!policy.allows(Some("https://www.example.com")), "and so is the host");
        assert!(!policy.allows(None), "an opaque origin matches nothing");
    }

    #[test]
    fn a_script_carrying_target_is_refused_however_the_policy_reads() {
        // Allowed origin, master switch off, gesture armed — and still no.
        let gate = gate(false, &["https://shop.test"]);
        let now = Instant::now();
        {
            let mut state = gate.state.lock().unwrap();
            state.set_document(&url("https://shop.test/cart"));
            state.arm(now);
        }
        for target in [
            "javascript:alert(1)",
            "data:text/html,<h1>hi</h1>",
            "blob:https://shop.test/8a2b",
            "file:///etc/passwd",
        ] {
            assert_eq!(
                gate.request_at(target, None, PopupVia::Link, now),
                Decision::Blocked,
                "{target} is not a page a tab can load"
            );
        }
        // Ordinary targets, including `about:blank` (what `window.open()` with no
        // url asks for), stay in the judge-by-policy group.
        assert!(loadable_target("about:blank"));
        assert!(loadable_target("https://shop.test/x"));
        assert!(loadable_target("mailto:hi@example.com"));
        assert!(loadable_target("/relative/thing"));
    }

    #[test]
    fn only_a_target_that_names_another_window_is_a_popup() {
        assert_eq!(new_window_target(None::<&str>), None);
        assert_eq!(new_window_target(Some("")), None);
        assert_eq!(new_window_target(Some("  ")), None);
        assert_eq!(new_window_target(Some("_self")), None);
        assert_eq!(new_window_target(Some("_TOP")), None);
        assert_eq!(new_window_target(Some("_parent")), None);
        assert_eq!(
            new_window_target(Some("_blank")).as_deref(),
            Some("_blank")
        );
        assert_eq!(
            new_window_target(Some(" ShareWindow ")).as_deref(),
            Some("ShareWindow")
        );
        assert_eq!(
            new_window_target(Some(&"x".repeat(400))).map(|name| name.len()),
            Some(120)
        );
    }

    #[test]
    fn a_reported_url_is_cut_on_a_char_boundary_with_a_mark() {
        let huge = "data:text/html,".to_owned() + &"あ".repeat(600);
        let capped = cap_reported_url(&huge);
        assert!(capped.ends_with('…'));
        assert!(capped.len() <= ironsurf_common::MAX_POPUP_URL_CHARS + '…'.len_utf8());
        assert_eq!(cap_reported_url("https://a.test/short"), "https://a.test/short");
    }

    #[test]
    fn only_comparable_origins_are_tracked_for_a_document() {
        assert_eq!(
            document_origin(&url("https://a.test/p")).as_deref(),
            Some("https://a.test")
        );
        assert_eq!(document_origin(&url("about:blank")), None);
        assert_eq!(document_origin(&url("data:text/html,<p>")), None);
        assert_eq!(document_origin(&url("file:///tmp/a.html")), None);
    }

    #[test]
    fn the_allow_list_is_capped_not_unbounded() {
        let many: Vec<String> =
            (0..(MAX_ALLOWED_ORIGINS + 40)).map(|i| format!("https://s{i}.test")).collect();
        let policy = Policy::new(true, &many);
        assert_eq!(policy.allowed.len(), MAX_ALLOWED_ORIGINS);
    }
}
