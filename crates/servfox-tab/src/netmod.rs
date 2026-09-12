//! Container-scoped network layer (`ARCHITECTURE.md` §Network layer).
//!
//! Two policies:
//!
//! * [`NetPolicy::Passthrough`] (default): Servo's own stack fetches everything;
//!   HTTPS and cookie isolation come from the one-container-per-process model
//!   (engine disk state lives in the container's profile dir, which is bound
//!   into the engine via `Opts.config_dir`).
//! * [`NetPolicy::Managed`] (`--managed-net`): every http(s) load is
//!   intercepted and re-fetched with our own `reqwest` (rustls) client so we
//!   control Cookie-header injection from the container's
//!   [`servfox_storage::CookieJar`], capture `Set-Cookie` responses into the
//!   jar, answer from [`servfox_storage::NetCache`] (with ETag/`If-None-Match`
//!   revalidation), and enforce https-only: `http:` **main-frame** navigations
//!   are upgraded to `https:`; if the upgrade fetch fails the load is blocked
//!   with an error page. `http:` sub-resources and `about:`/`file:`/local
//!   fixtures fall through to the engine (the documented "allow http" escape).
//!
//! Everything policy-related is a pure function over plain types so it can be
//! unit-tested without a live network (see `tests/e2e.rs`).

use std::cell::RefCell;

use servfox_storage::{CachedEntry, CookieJar, NetCache};
use url::Url;

/// Current wall-clock seconds since the Unix epoch (same clock the storage
/// layer uses for cookie expiry).
pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Which network policy this tab applies to http(s) loads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetPolicy {
    /// Servo's own network stack (engine-internal container isolation only).
    Passthrough,
    /// Intercept http(s) loads: container cookies, cache, https-only policy.
    Managed,
}

impl Default for NetPolicy {
    fn default() -> Self {
        NetPolicy::Passthrough
    }
}

/// What the managed policy decides to do with an intercepted request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Plan {
    /// Let Servo's engine fetch it (non-http schemes, `file:` fixtures, and —
    /// for the https-only escape hatch — non-main-frame `http:` loads).
    Pass,
    /// Fetch `Url` ourselves (the request is already `https:`).
    Fetch(Url),
    /// The request is an `http:` main-frame navigation: attempt `Url` (the
    /// `https:` upgrade) and block with an error page if that fails.
    Upgrade(Url),
}

/// True for `http:`/`https:` (the schemes the managed layer understands).
pub fn is_http_scheme(url: &Url) -> bool {
    url.scheme() == "http" || url.scheme() == "https"
}

/// Upgrade an `http:` URL to `https:` (default port folded away).
pub fn upgrade_to_https(url: &Url) -> Option<Url> {
    if url.scheme() != "http" {
        return None;
    }
    let mut upgraded = url.clone();
    upgraded
        .set_scheme("https")
        .map_err(|_| ())
        .ok()?;
    // `set_scheme` keeps an explicit `:80`; the canonical https port is 443,
    // so drop only the insecure default (non-default ports are preserved).
    if upgraded.port() == Some(80) {
        let _ = upgraded.set_port(None);
    }
    Some(upgraded)
}

/// Classify one request under the managed policy.
pub fn plan(policy: NetPolicy, request_url: &Url, is_for_main_frame: bool) -> Plan {
    if policy != NetPolicy::Managed || !is_http_scheme(request_url) {
        return Plan::Pass;
    }
    match request_url.scheme() {
        "https" => Plan::Fetch(request_url.clone()),
        "http" if is_for_main_frame => match upgrade_to_https(request_url) {
            Some(https) => Plan::Upgrade(https),
            // Cannot parse? Still fetch as-is through the engine.
            None => Plan::Pass,
        },
        // Documented escape hatch: http sub-resources keep working.
        _ => Plan::Pass,
    }
}

/// Revalidation outcome for a cached entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Revalidate {
    /// Server answered 304 and the validators still match → serve cached.
    ServeCached,
    /// Server answered something else (or validators mismatched) → use the new
    /// body/headers and update the cache.
    UseFresh {
        status: u16,
        headers: Vec<(String, String)>,
    },
}

/// Pure 304/revalidation decision: 304 + (matching `ETag` or matching
/// `Last-Modified`) serves the cached entry; everything else is fresh.
pub fn revalidate(
    cached: &CachedEntry,
    response_status: u16,
    response_etag: Option<&str>,
    response_last_modified: Option<&str>,
) -> Revalidate {
    if response_status == 304 {
        let etag_ok = match (&cached.etag, response_etag) {
            (Some(c), Some(r)) => c == r,
            _ => false,
        };
        let lm_ok = match (&cached.last_modified, response_last_modified) {
            (Some(c), Some(r)) => c == r,
            _ => false,
        };
        if etag_ok || lm_ok {
            return Revalidate::ServeCached;
        }
    }
    Revalidate::UseFresh {
        status: response_status,
        headers: Vec::new(),
    }
}

/// Extract the validators from a header list (case-insensitive names).
pub fn validators(headers: &[(String, String)]) -> (Option<String>, Option<String>) {
    let mut etag = None;
    let mut last_modified = None;
    for (name, value) in headers {
        match name.to_ascii_lowercase().as_str() {
            "etag" => etag = Some(value.clone()),
            "last-modified" => last_modified = Some(value.clone()),
            _ => {}
        }
    }
    (etag, last_modified)
}

/// Response headers that are forwarded to the engine on an intercepted load.
/// Body-transfer transforms (`Content-Encoding`/lengths — reqwest already
/// decoded) and hop-by-hop headers must **not** pass through.
pub fn forwarded_response_headers(headers: &[(String, String)]) -> Vec<(String, String)> {
    const DROP: &[&str] = &[
        "content-encoding",
        "content-length",
        "transfer-encoding",
        "connection",
        "keep-alive",
        "upgrade",
        "proxy-connection",
        "te",
        "trailer",
        "set-cookie", // captured into the jar, never forwarded
    ];
    headers
        .iter()
        .filter(|(name, _)| !DROP.contains(&name.to_ascii_lowercase().as_str()))
        .cloned()
        .collect()
}

/// Capture every `Set-Cookie` header of a response into the jar.
pub fn capture_set_cookies<'a, I: IntoIterator<Item = &'a str>>(
    jar: &mut CookieJar,
    request_url: &Url,
    set_cookie_headers: I,
    now_unix: i64,
) {
    for value in set_cookie_headers {
        jar.set_from_set_cookie(request_url, value, now_unix);
    }
}

/// Minimal HTML error page used to block failed https upgrades.
pub fn blocked_error_page(request_url: &Url, https_url: &Url, reason: &str) -> String {
    format!(
        r#"<!doctype html>
<html>
  <head><meta charset="utf-8"><title>Blocked: insecure navigation</title></head>
  <body style="font-family: monospace; margin: 2em">
    <h1>⛬ Navigation blocked</h1>
    <p>ServFox refuses to load <code>{request}</code>: this tab enforces
    HTTPS for navigations and the upgrade to <code>{https}</code> failed.</p>
    <p><small>{reason}</small></p>
  </body>
</html>
"#,
        request = request_url.as_str(),
        https = https_url.as_str(),
        reason = reason,
    )
}

// ---------------------------------------------------------------------------
// Live managed fetch (blocking reqwest/rustls; main thread only)

/// Mutable per-container network state shared with the webview delegate.
pub struct ManagedNet {
    pub jar: RefCell<CookieJar>,
    pub cache: NetCache,
    client: reqwest::blocking::Client,
}

impl ManagedNet {
    pub fn new(jar: CookieJar, cache: NetCache) -> Self {
        let client = reqwest::blocking::Client::builder()
            .use_rustls_tls()
            .redirect(reqwest::redirect::Policy::limited(10))
            // Keep the tab responsive even if a server hangs.
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .expect("reqwest client");
        ManagedNet {
            jar: RefCell::new(jar),
            cache,
            client,
        }
    }

    /// Fetch `url` with container cookies + cache revalidation, capturing
    /// `Set-Cookie` into the jar. Returns `(status, headers, body)` ready to
    /// be handed to the engine.
    pub fn fetch(&self, url: &Url) -> Result<(u16, Vec<(String, String)>, Vec<u8>), String> {
        let now = now_unix();
        let mut req = self.client.get(url.as_str());

        if let Some(cookie) = self.jar.borrow().cookie_header(url, now) {
            req = req.header(reqwest::header::COOKIE, cookie);
        }

        let cached = self.cache.lookup(url.as_str());
        if let Some(entry) = cached.as_ref() {
            let (etag, last_modified) = validators(&entry.headers);
            if let Some(etag) = etag {
                req = req.header(reqwest::header::IF_NONE_MATCH, etag);
            } else if let Some(lm) = last_modified {
                req = req.header(reqwest::header::IF_MODIFIED_SINCE, lm);
            }
        }

        let response = req.send().map_err(|e| format!("{e}"))?;
        let status = response.status().as_u16();
        // Cookies are captured against the final URL (redirect target).
        let final_url = response.url().clone();
        let set_cookies: Vec<String> = response
            .headers()
            .get_all(reqwest::header::SET_COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .map(str::to_owned)
            .collect();
        let headers: Vec<(String, String)> = response
            .headers()
            .iter()
            .map(|(k, v)| (k.as_str().to_owned(), v.to_str().unwrap_or_default().to_owned()))
            .collect();
        let body: Vec<u8> = response.bytes().map(|b| b.to_vec()).map_err(|e| e.to_string())?;

        {
            let mut jar = self.jar.borrow_mut();
            capture_set_cookies(&mut jar, &final_url, set_cookies.iter().map(String::as_str), now);
        }

        let (etag, last_modified) = validators(&headers);
        match revalidate(
            &cached.clone().unwrap_or_else(|| CachedEntry {
                status: 0,
                headers: Vec::new(),
                body: Vec::new(),
                etag: None,
                last_modified: None,
                cached_at: 0,
            }),
            status,
            etag.as_deref(),
            last_modified.as_deref(),
        ) {
            Revalidate::ServeCached => {
                let entry = cached.expect("ServeCached implies a cached entry existed");
                Ok((entry.status, entry.headers, entry.body))
            }
            Revalidate::UseFresh { .. } => {
                if status < 400 {
                    self.cache.store(url.as_str(), status, &headers, body.clone(), now);
                }
                Ok((status, headers, body))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Policy unit tests (no live network — every rule is a pure function)

#[cfg(test)]
mod tests {
    use super::*;

    fn url(s: &str) -> Url {
        Url::parse(s).expect("test url")
    }

    fn cached(etag: Option<&str>, last_modified: Option<&str>) -> CachedEntry {
        CachedEntry {
            status: 200,
            headers: Vec::new(),
            body: b"cached body".to_vec(),
            etag: etag.map(str::to_owned),
            last_modified: last_modified.map(str::to_owned),
            cached_at: 100,
        }
    }

    #[test]
    fn passthrough_policy_never_intercepts() {
        assert_eq!(
            plan(NetPolicy::Passthrough, &url("https://example.com"), true),
            Plan::Pass
        );
        assert_eq!(
            plan(NetPolicy::Passthrough, &url("http://example.com"), true),
            Plan::Pass
        );
    }

    #[test]
    fn managed_fetches_https_and_upgrades_http_navigations() {
        let https = url("https://example.com/page");
        assert_eq!(
            plan(NetPolicy::Managed, &https, true),
            Plan::Fetch(https.clone())
        );
        let http = url("http://example.com/page");
        assert_eq!(
            plan(NetPolicy::Managed, &http, true),
            Plan::Upgrade(url("https://example.com/page"))
        );
        // Escape hatch: http sub-resources keep working unmodified.
        assert_eq!(plan(NetPolicy::Managed, &http, false), Plan::Pass);
    }

    #[test]
    fn managed_passes_non_http_schemes() {
        for scheme_url in ["file:///tmp/x/index.html", "about:blank", "data:text/html,x"] {
            assert_eq!(
                plan(NetPolicy::Managed, &url(scheme_url), true),
                Plan::Pass,
                "{scheme_url} must pass through"
            );
        }
    }

    #[test]
    fn https_upgrade_folding() {
        assert_eq!(
            upgrade_to_https(&url("http://example.com/")).unwrap().as_str(),
            "https://example.com/"
        );
        // The insecure default port is folded away…
        assert_eq!(
            upgrade_to_https(&url("http://example.com:80/x")).unwrap().as_str(),
            "https://example.com/x"
        );
        // …but a real port survives.
        assert_eq!(
            upgrade_to_https(&url("http://example.com:8080/x"))
                .unwrap()
                .as_str(),
            "https://example.com:8080/x"
        );
        assert!(upgrade_to_https(&url("https://example.com")).is_none());
    }

    #[test]
    fn revalidation_serves_cached_only_on_matching_validators() {
        // 304 + matching ETag → cached.
        assert_eq!(
            revalidate(&cached(Some("W\"1\""), None), 304, Some("W\"1\""), None),
            Revalidate::ServeCached
        );
        // 304 + matching Last-Modified → cached.
        assert_eq!(
            revalidate(&cached(None, Some("Tue, 1 Jul 2025 00:00:00 GMT")), 304, None, Some("Tue, 1 Jul 2025 00:00:00 GMT")),
            Revalidate::ServeCached
        );
        // 304 without any validators → fresh.
        match revalidate(&cached(None, None), 304, None, None) {
            Revalidate::UseFresh { status, .. } => assert_eq!(status, 304),
            other => panic!("expected UseFresh, got {other:?}"),
        }
        // 200 always wins over the cache.
        assert!(matches!(
            revalidate(&cached(Some("a"), None), 200, Some("b"), None),
            Revalidate::UseFresh { status: 200, .. }
        ));
    }

    #[test]
    fn forwarded_headers_drop_hop_by_hop_and_transformed_body_headers() {
        let headers = vec![
            ("content-type".into(), "text/html".into()),
            ("Set-Cookie".into(), "a=1".into()),
            ("content-encoding".into(), "gzip".into()),
            ("Content-Length".into(), "9".into()),
            ("connection".into(), "keep-alive".into()),
            ("etag".into(), "\"v1\"".into()),
        ];
        let kept = forwarded_response_headers(&headers);
        let names: Vec<&str> = kept.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["content-type", "etag"]);
    }

    #[test]
    fn managed_cookie_rules_over_the_container_jar() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store =
            servfox_storage::ContainerStore::from_profile_dir(servfox_common::ContainerId(9), dir.path());
        let mut jar = store.cookie_jar();
        let now = 1_700_000_000;

        // Secure cookie set over https is stored and only sent over https.
        capture_set_cookies(
            &mut jar,
            &url("https://example.com/"),
            ["sid=42; Secure; Path=/"],
            now,
        );
        assert_eq!(jar.cookies().len(), 1);
        assert_eq!(
            jar.cookie_header(&url("https://example.com/"), now).as_deref(),
            Some("sid=42")
        );
        assert_eq!(
            jar.cookie_header(&url("http://example.com/"), now),
            None,
            "Secure cookies must never be sent over http"
        );

        // A Secure cookie may not be *set* over http at all.
        capture_set_cookies(
            &mut jar,
            &url("http://example.org/"),
            ["steal=1; Secure; Path=/"],
            now,
        );
        assert_eq!(jar.cookies().len(), 1, "http may not set Secure cookies");

        // Host-only cookie does not leak to subdomains; domain cookie does.
        capture_set_cookies(
            &mut jar,
            &url("https://a.example.com/"),
            ["h=1; Path=/", "shared=2; Domain=example.com; Path=/"],
            now,
        );
        let header = jar
            .cookie_header(&url("https://b.example.com/"), now)
            .expect("header");
        assert!(header.contains("shared=2"));
        assert!(!header.contains("h=1"), "host-only cookie leaked to subdomain");

        // Persistence round-trip: exactly what the tab does on Destroy.
        jar.save().expect("save jar");
        let reloaded = store.cookie_jar();
        assert_eq!(reloaded.cookies().len(), 3);
        assert!(reloaded
            .cookies()
            .iter()
            .any(|c| c.name == "sid" && c.value == "42" && c.secure));
    }

    #[test]
    fn blocked_page_mentions_both_urls() {
        let page = blocked_error_page(&url("http://example.com/"), &url("https://example.com/"), "boom");
        assert!(page.contains("http://example.com/"));
        assert!(page.contains("https://example.com/"));
        assert!(page.contains("boom"));
    }
}
