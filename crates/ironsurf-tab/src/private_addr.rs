//! The **private-address rule**: web content may not read the network the
//! browser itself sits on (AUDIT finding 5, PoC `audit_poc_nav::f4`).
//!
//! ## Why
//!
//! A page on the open web that may fetch `http://127.0.0.1:8000/…` (or any
//! `192.168.x.x`, `10.x.x.x`, `169.254.x.x`, `fe80::…` service) turns the
//! browser into an authenticated proxy into the user's LAN: an attacker's script
//! reads the body of a router's admin page, or of the tab's own archive server,
//! or — just by timing a load — maps which ports are open. Nothing behind those
//! addresses chose to talk to the attacker's site, so every one of those requests
//! leaks data somewhere the user never sent it.
//!
//! ## The rule
//!
//! A request for an **http(s) URL whose host is a private address** is refused
//! unless it is **same-origin with the document that makes it**. The same-origin
//! escape hatch is where the leak stops making sense: the user opened
//! `http://192.168.1.10:8080/` themselves, so that document may load its own CSS,
//! call its own API and follow its own links. `http:` versus `https:`,
//! `localhost` versus `127.0.0.1`, and any two ports are all different origins.
//!
//! So the rule lets through:
//!
//! * the browser's own UI — an address-bar navigation, a reload, an internal
//!   page, a download the user started (they pass `None` as the document);
//! * a document on a private address talking to **its own** origin;
//! * anything that is not a private address (public hosts are unaffected).
//!
//! and refuses: `fetch`/`XMLHttpRequest`, `<script src>` and ES-module bodies,
//! Blitz subresources (`<img>`, `<link>`, `<iframe>`, fonts), a worker's own
//! requests, a page-driven `Refresh:` hop and a `location.href=` navigation —
//! every one of them a request a **page** arranged.
//!
//! What the rule deliberately does not touch, because the request is the
//! browser's or the user's rather than the page's: a form the user submitted
//! (its own POST path), a download the user started, and `<video>`/`<audio>`
//! bytes, which the media layer streams on its own client.
//!
//! ## Console
//!
//! Every refusal is reported on the DevTools console
//! ([`crate::logging`] → `FromTab::ConsoleLog { source: "page", level: "warn" }`)
//! naming both ends and the port, plus a `warn!` line in the tab's own log. One
//! origin pair warns once ([`PrivateAddressGuard`]) so a page polling a dead
//! loopback port cannot flood the panel.
//!
//! ## Honest gaps
//!
//! * A host written as a **DNS name** is only refused when it is `.localhost`
//!   (RFC 6761: always loopback). Judging `intranet.example` would mean
//!   resolving it, and a lookup is a network operation of its own whose answer
//!   changes when the user plugs in an Ethernet cable. A name of the attacker's
//!   choosing that happens to point at `10.0.0.1` therefore still gets through;
//!   every address-shaped host is caught.
//! * Subresources are attributed to the **main document's** URL: Blitz hands the
//!   net provider no document handle, so an `<iframe>` inherits the top-level
//!   page's origin for this check — the conservative reading for a public page,
//!   and never blocking what a local iframe's own origin would have allowed.
//! * The rule decides by **address**, so it is not a substitute for the
//!   egress sandbox (`ironsurf-sandbox`'s `--allow-host` list), which is what
//!   actually stops IP packets. It runs at the layer where the page is the one
//!   asking, and blocks the request before the socket opens.

use std::collections::HashSet;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};

use ironsurf_common::warn;
use url::Url;

use crate::logging::{ConsoleEntry, ConsoleQueue};

/// Origin pairs one tab remembers warning about, at most. Past the cap a
/// refusal is still reported (never silently swallowed), just no longer
/// deduplicated — the same degradation [`crate::logging`] uses for stylesheets.
const MAX_REMEMBERED_WARNS: usize = 64;

/// Which addresses mean "the network the browser is on": loopback, RFC 1918,
/// link-local, and their IPv6 equivalents — unique-local (`fc00::/7`, RFC 4193),
/// link-local (`fe80::/10`) and `::1`/`::`.
#[must_use]
pub fn is_private_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, _, _] = v4.octets();
            // 127.0.0.0/8 loopback (RFC 6761) — the whole block, not just .0.1:
            // Linux answers on every address in it.
            a == 127
            // RFC 1918 private space.
                || a == 10
                || (a == 172 && (16..=31).contains(&b))
                || (a == 192 && b == 168)
                // 169.254.0.0/16 link-local (RFC 3927): zeroconf printers,
                // router setup pages, 169.254.169.254 metadata services.
                || (a == 169 && b == 254)
                // 0.0.0.0/8 "this network" (RFC 1122): on Linux a client that
                // dials it reaches the local host, not a wildcard. The tab's own
                // archive server is commonly bound to 0.0.0.0.
                || a == 0
                // 100.64.0.0/10 carrier-grade NAT (RFC 6598): the provider's
                // box, never a public site, carrying the user's own traffic.
                || (a == 100 && (64..=127).contains(&b))
            // Reserved (240/4) and documentation (192.0.2/24) ranges are
            // deliberately NOT private: nothing legitimate is served there.
        }
        IpAddr::V6(v6) => {
            if v6.is_loopback() || v6.is_unspecified() {
                return true;
            }
            let words = v6.segments();
            // fe80::/10 link-local (RFC 4291).
            let link_local = (words[0] & 0xffc0) == 0xfe80;
            // fc00::/7 unique local (RFC 4193): fc00…fdff, the IPv6 RFC 1918.
            let unique_local = (words[0] & 0xfe00) == 0xfc00;
            link_local || unique_local
        }
    }
}

/// The URL's host as written (IPv6 brackets kept, as `Url` reports them) and the
/// address behind it when the host is an IP literal. `None` for hostless URLs
/// (`about:blank`, `data:`).
fn host_and_ip(url: &Url) -> Option<(String, Option<IpAddr>)> {
    let host = url.host_str()?.to_owned();
    let ip = match host.strip_prefix('[').and_then(|h| h.strip_suffix(']')) {
        // A zone id (`fe80::1%eth0`) is not part of the address.
        Some(inner) => inner.split('%').next().unwrap_or(inner).parse().ok(),
        None => host.parse::<IpAddr>().ok(),
    };
    Some((host, ip))
}

/// True when `url` names a **private address**: an IP literal in one of the
/// local ranges ([`is_private_ip`]) or a `.localhost` host, which by RFC 6761
/// can only resolve to loopback. Other schemes are never private addresses —
/// `file:`, `data:` and friends are the scheme policy's business, not this one.
#[must_use]
pub fn is_private_address(url: &Url) -> bool {
    if !matches!(url.scheme(), "http" | "https" | "ws" | "wss") {
        return false;
    }
    match host_and_ip(url) {
        Some((_, Some(ip))) => is_private_ip(&ip),
        Some((host, None)) => {
            let host = host.to_ascii_lowercase();
            host == "localhost" || host.ends_with(".localhost")
        }
        None => false,
    }
}

/// How to name `url` in a warning: its origin when it has one (the port is the
/// whole point of a port probe, so it has to show), its scheme when it does not.
fn label(url: &Url) -> String {
    match url.origin() {
        url::Origin::Opaque(_) => format!("{}: document", url.scheme()),
        origin => origin.ascii_serialization(),
    }
}

/// `true` when both URLs share an origin (scheme, host and port). Opaque
/// origins (`file:`, `data:`, `about:`) match nothing, not even themselves.
fn same_origin(a: &Url, b: &Url) -> bool {
    match (a.origin(), b.origin()) {
        (url::Origin::Tuple(ka, ha, pa), url::Origin::Tuple(kb, hb, pb)) => {
            ka == kb && ha == hb && pa == pb
        }
        (url::Origin::Opaque(a), url::Origin::Opaque(b)) => a == b,
        _ => false,
    }
}

/// Resolve a page-chosen request target the way the platform does: an absolute
/// URL as given, a relative one against the document (a page's `fetch("/api/x")`,
/// `new Worker("w.js")`). `None` when neither parses.
#[must_use]
pub fn resolve(document: Option<&Url>, requested: &str) -> Option<Url> {
    if let Ok(absolute) = Url::parse(requested) {
        if absolute.has_authority() {
            return Some(absolute);
        }
    }
    document.and_then(|base| base.join(requested).ok())
}

/// The rule: `None` when the request may go out, `Some(reason)` when a page may
/// not make it. `document` is the URL of the document making the request —
/// `None` means the browser's own UI asked for it (address bar, reload, internal
/// page, a download the user clicked), which is never refused.
#[must_use]
pub fn private_address_rule(document: Option<&Url>, target: &Url) -> Option<String> {
    if !is_private_address(target) {
        return None;
    }
    let Some(document) = document else {
        return None;
    };
    if same_origin(document, target) {
        return None;
    }
    Some(format!(
        "{} is a private address, and this page is on {}",
        label(target),
        label(document)
    ))
}

/// The two ends of a refusal, as origins.
fn origin_pair(document: Option<&Url>, target: &Url) -> String {
    let document = match document {
        Some(url) => label(url),
        None => "<browser ui>".to_owned(),
    };
    format!("{document} -> {}", label(target))
}

/// One tab's copy of the rule: the decision, plus the console warning each
/// refusal prints — once per (document origin → private origin, method) pair, so
/// a page polling a dead local port every 100 ms does not drown the panel.
///
/// Cheap to clone and `Send + Sync` (both fields are `Arc`s), so one guard
/// serves the tab loop, the JS host and the fetch threads together.
#[derive(Clone, Debug)]
pub struct PrivateAddressGuard {
    console: ConsoleQueue,
    warned: Arc<Mutex<HashSet<String>>>,
}

impl Default for PrivateAddressGuard {
    fn default() -> Self {
        Self::new(crate::logging::console_queue())
    }
}

impl PrivateAddressGuard {
    /// A guard whose refusals print into `console` — the tab's DevTools console
    /// queue, which the tab loop drains into `FromTab::ConsoleLog`.
    #[must_use]
    pub fn new(console: ConsoleQueue) -> Self {
        PrivateAddressGuard {
            console,
            warned: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    /// The queue refusals print to (a handle on the tab's own).
    #[must_use]
    pub fn console(&self) -> ConsoleQueue {
        ConsoleQueue::clone(&self.console)
    }

    /// Decide one request. `Ok(())` lets it reach the network; `Err(reason)`
    /// means the caller must not send it — and should surface `reason` to
    /// whoever asked (a page sees a network error naming it, and the console
    /// warning has already been queued by then).
    ///
    /// `via` names the method in the console line: `"navigation"`, `"fetch"`,
    /// `"xhr"`, `"script"`, `"subresource"`, `"worker"`, …
    pub fn check(&self, document: Option<&Url>, target: &Url, via: &str) -> Result<(), String> {
        let Some(reason) = private_address_rule(document, target) else {
            return Ok(());
        };
        let message = format!(
            "Blocked {via} to {target}: {reason} — only the browser's own UI may reach it"
        );
        // Dedupe on origins, not URLs: `/api/1` and `/api/2` are one warning.
        let key = format!("{via} {}", origin_pair(document, target));
        let mut warned = self.warned.lock().expect("private-address warn set");
        if warned.len() < MAX_REMEMBERED_WARNS {
            if warned.contains(&key) {
                drop(warned);
                warn!("private-address rule: {message} (log only, already on the console)");
                return Err(message);
            }
            warned.insert(key);
        }
        drop(warned);
        self.console
            .lock()
            .expect("console events mutex")
            .push(ConsoleEntry {
                source: "page".to_owned(),
                level: "warn".to_owned(),
                message: message.clone(),
            });
        warn!("private-address rule: {message}");
        Err(message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(raw: &str) -> Url {
        Url::parse(raw).unwrap_or_else(|err| panic!("test url {raw}: {err}"))
    }

    #[test]
    fn private_ip_ranges() {
        let private = [
            "127.0.0.1",
            "127.1.2.3",
            "127.255.255.254",
            "10.0.0.1",
            "10.255.255.255",
            "172.16.0.1",
            "172.23.9.9",
            "192.168.1.1",
            "169.254.10.20",
            "0.0.0.0",
            "100.64.0.1",
            "100.127.255.255",
            "::1",
            "::",
            "fe80::1",
            "febf:ffff:ffff:ffff::",
            "fc00::1",
            "fd12:3456::7890",
        ];
        for raw in private {
            let ip: IpAddr = raw.parse().unwrap_or_else(|_| panic!("parse {raw}"));
            assert!(is_private_ip(&ip), "{raw} must count as private");
        }
        let public = [
            "8.8.8.8",
            "172.32.0.1",
            "100.128.0.1",
            "100.63.255.255",
            "192.0.2.1",
            "2001:4860:4860::8888",
            "2606:4700::1111",
        ];
        for raw in public {
            let ip: IpAddr = raw.parse().unwrap_or_else(|_| panic!("parse {raw}"));
            assert!(!is_private_ip(&ip), "{raw} must be public");
        }
    }

    #[test]
    fn address_shapes() {
        // IPv6 keeps its brackets in `Url::host_str()`, and a zone id may follow.
        assert!(is_private_address(&url("http://[::1]/x")));
        assert!(is_private_address(&url("http://[fe80::1]/x")));
        assert!(!is_private_address(&url("http://[2606:4700::1]/x")));
        // Names: only `.localhost` is refused (RFC 6761), and never as the tail
        // of an unrelated domain.
        assert!(is_private_address(&url("http://localhost:8000/x")));
        assert!(is_private_address(&url("http://web.localhost/x")));
        assert!(!is_private_address(&url("http://mylocalhost/x")));
        assert!(!is_private_address(&url("http://intranet.local/x")));
        // Other schemes are the scheme policy's business.
        for raw in [
            "file:///etc/passwd",
            "data:text/html,x",
            "about:blank",
            "blob:null/1-2",
            "ironsurf:settings",
        ] {
            assert!(!is_private_address(&url(raw)), "{raw} is no address");
        }
    }

    #[test]
    fn browser_ui_may_reach_it() {
        // `document: None` is the browser's own UI: address bar, reload, an
        // internal page, a download the user clicked.
        for target in [
            "http://127.0.0.1:9/",
            "http://10.0.0.5/",
            "https://192.168.1.1/admin",
            "http://[::1]:631/",
            "http://169.254.1.1/",
        ] {
            let target = url(target);
            assert_eq!(private_address_rule(None, &target), None, "{target}");
        }
    }

    #[test]
    fn a_page_on_the_web_may_not_reach_it() {
        let document = url("https://example.com/a/page.html");
        for target in [
            "http://127.0.0.1:9/api/x",
            "http://127.1.2.3:8000/",
            "http://10.20.30.40:3000/api",
            "http://172.16.0.9/",
            "http://192.168.0.7:8080/",
            "http://169.254.169.254/latest/meta-data/",
            "http://[fd00::5]/x",
            "http://[fe80::1]/x",
            "http://localhost:631/printers/",
        ] {
            let target = url(target);
            let reason = private_address_rule(Some(&document), &target)
                .unwrap_or_else(|| panic!("{target} must be refused"));
            assert!(reason.contains("private address"), "{reason}");
            // Both ends named, ports included: that is the useful half.
            assert!(reason.contains(&label(&target)), "{reason}");
            assert!(reason.contains("example.com"), "{reason}");
        }
    }

    #[test]
    fn same_origin_may_reach_it() {
        // The user opened the local page, so it may talk to itself.
        let document = url("http://127.0.0.1:8000/index.html");
        assert_eq!(
            private_address_rule(Some(&document), &url("http://127.0.0.1:8000/api/x")),
            None
        );
        // …but a different port, scheme or spelling of the host is a different
        // origin, and is refused.
        for target in [
            "http://127.0.0.1:9/api",
            "https://127.0.0.1:8000/api",
            "http://localhost:8000/api",
            "http://127.0.0.2:8000/api",
        ] {
            let target = url(target);
            assert!(
                private_address_rule(Some(&document), &target)
                    .is_some_and(|reason| reason.contains("private address")),
                "{target} must be refused from {document}"
            );
        }
    }

    #[test]
    fn a_local_page_may_navigate_to_a_public_one() {
        // A page on a private address pulling in a public resource (CDN, font,
        // analytics) is unaffected: only the local end is restricted.
        let document = url("http://192.168.1.10:8080/");
        assert_eq!(
            private_address_rule(Some(&document), &url("https://cdn.example/x.css")),
            None
        );
    }

    #[test]
    fn public_pages_and_public_targets_are_untouched() {
        let document = url("https://example.com/");
        for target in [
            "https://example.com/api",
            "https://api.example.org/v1",
            "data:text/html,x",
            "file:///etc/passwd",
            "about:blank",
            "blob:https://example.com/1-2",
        ] {
            let target = url(target);
            assert_eq!(private_address_rule(Some(&document), &target), None, "{target}");
        }
    }

    #[test]
    fn hostless_targets_are_never_refused() {
        // A page on a private address with an opaque-target URL (an about:
        // iframe, say) is not an address at all.
        let document = url("http://127.0.0.1:8000/");
        assert_eq!(
            private_address_rule(Some(&document), &url("about:blank")),
            None
        );
    }

    #[test]
    fn resolves_relative_targets_against_the_document() {
        let document = url("http://127.0.0.1:8000/a/b.html");
        assert_eq!(
            resolve(Some(&document), "/api/x").map(|u| u.to_string()),
            Some("http://127.0.0.1:8000/api/x".to_owned())
        );
        assert_eq!(
            resolve(Some(&document), "http://10.0.0.9:3000/y").map(|u| u.to_string()),
            Some("http://10.0.0.9:3000/y".to_owned())
        );
        assert_eq!(resolve(Some(&document), "data:text/html,x").map(|u| u.to_string()), Some("data:text/html,x".to_owned()));
        assert_eq!(resolve(None, "/api/x"), None);
    }

    #[test]
    fn a_refusal_warns_once_per_origin_pair() {
        let console = crate::logging::ConsoleQueue::default();
        let guard = PrivateAddressGuard::new(console.clone());
        let document = url("https://example.com/a.html");
        let target = url("http://127.0.0.1:9/api/one");

        let first = guard.check(Some(&document), &target, "fetch");
        assert!(first.is_err(), "a web page must not fetch a private address");
        let reason = first.unwrap_err();
        assert!(reason.contains("Blocked fetch"), "{reason}");
        assert!(reason.contains("127.0.0.1:9"), "{reason}");
        assert!(reason.contains("example.com"), "{reason}");

        let drain = |q: &ConsoleQueue| -> Vec<String> {
            q.lock().unwrap().drain(..).map(|e| e.message).collect()
        };
        let warned = drain(&console);
        assert_eq!(warned.len(), 1, "one warning, not one per byte");
        assert!(warned[0].starts_with("Blocked fetch to "), "{}", warned[0]);

        // Same origin pair, different path and method: still silent.
        assert!(guard
            .check(Some(&document), &url("http://127.0.0.1:9/api/two"), "fetch")
            .is_err());
        assert!(drain(&console).is_empty(), "the pair warned already");
        // A different method is a different line, and a different private
        // origin is a new pair.
        assert!(guard
            .check(Some(&document), &target, "xhr")
            .is_err());
        assert_eq!(drain(&console).len(), 1);
        assert!(guard
            .check(Some(&document), &url("http://10.0.0.9:9/"), "fetch")
            .is_err());
        assert_eq!(drain(&console).len(), 1);
        // Letting a request through never prints anything.
        assert!(guard
            .check(Some(&document), &url("https://cdn.example/x.css"), "fetch")
            .is_ok());
        assert!(drain(&console).is_empty());
    }

    #[test]
    fn guard_allows_what_the_rule_allows() {
        let guard = PrivateAddressGuard::default();
        let local = url("http://127.0.0.1:8000/page.html");
        assert!(guard
            .check(Some(&local), &url("http://127.0.0.1:8000/api"), "fetch")
            .is_ok());
        assert!(guard.check(None, &url("http://127.0.0.1:9/"), "navigation").is_ok());
    }

    #[test]
    fn one_guard_serves_every_thread() {
        fn is_send_sync<T: Send + Sync>(_: &T) {}
        let guard = PrivateAddressGuard::default();
        is_send_sync(&guard);
        let shares = [
            guard.clone(),
            guard.clone(),
            guard.clone(),
            guard.clone(),
            guard.clone(),
            guard.clone(),
            guard.clone(),
            guard.clone(),
        ];
        std::thread::scope(|scope| {
            let handles = shares
                .iter()
                .map(|shared| {
                    scope.spawn(|| {
                        shared
                            .check(
                                Some(&url("https://example.com/")),
                                &url("http://127.0.0.1:9/"),
                                "fetch",
                            )
                            .is_err()
                    })
                })
                .collect::<Vec<_>>();
                // Every clone refuses: the guard is shared, not copied state.
            assert_eq!(handles.len(), 8);
            for handle in handles {
                assert!(handle.join().expect("thread joins"));
            }
        });
    }
}
