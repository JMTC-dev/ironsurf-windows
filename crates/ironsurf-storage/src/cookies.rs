//! RFC6265-subset cookie jar, persisted per container as `cookies.json`.
//!
//! Rules implemented (and unit-tested):
//! - `Secure` cookies are only ever **set** from an `https:` request URL and
//!   only ever **sent** to `https:` URLs;
//! - domain-matched cookies (leading `.` / `Domain=` attribute) act as
//!   wildcards: they match the cookie domain itself and any `.sub.domain`;
//!   host-only cookies (no `Domain=`) match the exact host only — a cookie
//!   set by `www.example.com` is never sent to `example.com` or
//!   `sub.www.example.com`, and `.example.com` never matches `notexample.com`;
//! - a `Domain=` attribute is honoured only when it is a suffix of the
//!   request host; against an IP request host it is ignored (host-only), and
//!   single-label (undotted) domains can never widen scope;
//! - path matching is RFC6265 section 5.1.4 verbatim (`/foo` matches `/foo`
//!   and `/foo/bar` but not `/foobar`; `/foo/` never matches bare `/foo`);
//! - `HttpOnly` is captured and persisted (never exposed to scripts — the
//!   `document_cookie` view omits such cookies entirely) but still sent
//!   normally over HTTP;
//! - expired cookies are pruned on load, on access, and on `set` replacement
//!   (a `Set-Cookie` whose `Expires` is already past deletes the cookie it
//!   would have replaced, exactly like a `Max-Age<=0`);
//! - `Max-Age` takes precedence over `Expires`; a `Max-Age<=0` deletes.

use crate::registry::StoreError;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Cookie {
    pub name: String,
    pub value: String,
    pub domain: String,
    pub path: String,
    pub secure: bool,
    pub http_only: bool,
    pub expires_unix: Option<i64>,
}

#[derive(Default)]
#[derive(Clone, Debug)]
pub struct CookieJar {
    path: Option<PathBuf>,
    cookies: Vec<Cookie>,
}

impl CookieJar {
    /// Empty jar (not persisted).
    pub fn new() -> Self {
        Self::default()
    }

    /// Load from `path`, pruning already-expired cookies. A missing or
    /// corrupt file yields an empty jar (best-effort persistence must never
    /// take a tab down).
    pub(crate) fn load(path: &Path) -> Self {
        let mut jar = CookieJar {
            path: Some(path.to_path_buf()),
            cookies: Vec::new(),
        };
        if let Ok(text) = fs::read_to_string(path) {
            if let Ok(cookies) = serde_json::from_str::<Vec<Cookie>>(&text) {
                jar.cookies = cookies;
            }
        }
        jar.prune_expired(crate::now_unix());
        jar
    }

    pub fn cookies(&self) -> &[Cookie] {
        &self.cookies
    }

    /// Apply one `Set-Cookie` header value against `request_url` at `now_unix`.
    pub fn set_from_set_cookie(&mut self, request_url: &url::Url, header_value: &str, now_unix: i64) {
        let is_https = request_url.scheme() == "https";
        let request_host = request_url.host_str().unwrap_or("").to_ascii_lowercase();
        // An IP request host never accepts a Domain attribute (RFC6265 is
        // silent; every browser treats such cookies as host-only).
        let host_is_ip = request_host.parse::<std::net::IpAddr>().is_ok();
        let mut it = header_value.split(';');
        let Some(name_value) = it.next() else { return };
        let (name, value) = match name_value.split_once('=') {
            Some((n, v)) => (n.trim(), v.trim()),
            // A value with no `=` is a cookie named with an empty value only
            // if the whole pair is just a name; reject everything else.
            None => return,
        };
        if name.is_empty() {
            return;
        }

        let mut domain: Option<String> = None;
        let mut path: Option<String> = None;
        let mut secure = false;
        let mut http_only = false;
        let mut expires_hdr: Option<i64> = None;
        let mut max_age: Option<i64> = None;
        let mut max_age_delete = false;

        for attr in it {
            let (key, val) = match attr.split_once('=') {
                Some((k, v)) => (k.trim().to_ascii_lowercase(), v.trim()),
                None => (attr.trim().to_ascii_lowercase(), ""),
            };
            match key.as_str() {
                "domain" => {
                    // A Domain= against an IP request host rejects the
                    // cookie (no IP domain-matches a name); browsers do the
                    // same and never fall back to an implicit host-only set.
                    if host_is_ip {
                        return;
                    }
                    // An unusable Domain value (empty, single-label, junk)
                    // drops the cookie rather than silently widening it to
                    // host-only (§5.2.3: no domain-match ⇒ ignore).
                    match normalize_domain(val) {
                        Some(d) => domain = Some(d),
                        None => return,
                    }
                }
                "path" => {
                    if !val.is_empty() && val.starts_with('/') {
                        path = Some(val.to_string());
                    }
                }
                "secure" => secure = true,
                "httponly" => http_only = true,
                // Max-Age takes precedence over Expires (§5.3), so they are
                // kept separate until the end. A value that is not a plain
                // run of digits (signed, floating, junk) means expiry-time 0
                // — i.e. delete (§5.2.2 "char-after-digits" rule).
                "max-age" => match val.parse::<i64>() {
                    Ok(d) if d > 0 && !val.starts_with('-') && !val.starts_with('+') => {
                        max_age = Some(now_unix.saturating_add(d))
                    }
                    Ok(_) => max_age_delete = true,
                    Err(_) => max_age_delete = true,
                },
                "expires" => {
                    if let Some(t) = parse_http_date(val) {
                        expires_hdr = Some(t);
                    }
                }
                _ => {} // Unknown attributes are ignored (RFC6265 §5.2).
            }
        }

        let expires = max_age.or(expires_hdr);

        // Secure cookies may only be set over https.
        if secure && !is_https {
            return;
        }

        // Domain-scope cookies keep a leading dot (".example.com"); host-only
        // cookies keep the bare host — matching the on-disk RFC6265 convention.
        let cookie_domain = match domain {
            Some(d) => {
                // A Public-Suffix-style check is out of scope; refuse a
                // Domain attribute that is not a suffix of the request host.
                if request_host == d || request_host.ends_with(&format!(".{d}")) {
                    format!(".{d}")
                } else {
                    return;
                }
            }
            None => request_host,
        };
        if cookie_domain.is_empty() {
            return;
        }

        let cookie_path = path.unwrap_or_else(|| default_cookie_path(&request_url));

        // A cookie that is already dead at set time (Max-Age<=0, a malformed
        // Max-Age, or an Expires in the past) deletes the cookie it names
        // instead of being stored.
        if max_age_delete || expires.map(|e| e <= now_unix).unwrap_or(false) {
            self.remove_matching(name, &cookie_domain, &cookie_path);
            return;
        }

        // Replacement: same name/domain/path overwrites the old cookie.
        self.remove_matching(name, &cookie_domain, &cookie_path);
        self.cookies.push(Cookie {
            name: name.to_string(),
            value: value.to_string(),
            domain: cookie_domain,
            path: cookie_path,
            secure,
            http_only,
            expires_unix: expires,
        });
    }

    /// `Cookie:` header value ("a=1; b=2") for `request_url`, or `None` if
    /// no cookie applies. Expired cookies are pruned during the scan.
    /// Includes `HttpOnly` cookies — this is the wire view.
    pub fn cookie_header(&self, request_url: &url::Url, now_unix: i64) -> Option<String> {
        self.cookie_string(request_url, now_unix, false)
    }

    /// The `document.cookie` getter view: the same matching rules as
    /// [`Self::cookie_header`] but **`HttpOnly` cookies never appear** —
    /// that attribute exists precisely to hide a cookie from scripts while
    /// it keeps riding on HTTP requests. Answers `""` (not `None`) when
    /// nothing visible applies, which is what browsers hand scripts.
    pub fn document_cookie(&self, request_url: &url::Url, now_unix: i64) -> String {
        self.cookie_string(request_url, now_unix, true)
            .unwrap_or_default()
    }

    fn cookie_string(
        &self,
        request_url: &url::Url,
        now_unix: i64,
        skip_http_only: bool,
    ) -> Option<String> {
        // Interior mutability is not worth it here: prune via a const-safe
        // scan and rely on callers mutating through &mut elsewhere.
        let is_https = request_url.scheme() == "https";
        let host = request_url.host_str()?.to_ascii_lowercase();
        let req_path = request_path(request_url);
        let mut pairs: Vec<&Cookie> = self
            .cookies
            .iter()
            .filter(|c| c.expires_unix.map(|e| e > now_unix).unwrap_or(true))
            .filter(|c| !(c.secure && !is_https))
            .filter(|c| !(skip_http_only && c.http_only))
            .filter(|c| domain_match(&host, &c.domain))
            .filter(|c| path_match(&req_path, &c.path))
            .collect();
        if pairs.is_empty() {
            return None;
        }
        // RFC6265 §5.4: longer paths first (stable for equal lengths).
        pairs.sort_by_key(|c| std::cmp::Reverse(c.path.chars().count()));
        let mut out = String::new();
        for c in pairs {
            if !out.is_empty() {
                out.push_str("; ");
            }
            out.push_str(&c.name);
            out.push('=');
            out.push_str(&c.value);
        }
        Some(out)
    }

    /// Persist to the loaded path (no-op for an unpersisted jar).
    pub fn save(&self) -> Result<(), StoreError> {
        match &self.path {
            Some(p) => crate::registry::write_atomic(p, &self.cookies),
            None => Ok(()),
        }
    }

    pub fn clear(&mut self) {
        self.cookies.clear();
    }

    fn remove_matching(&mut self, name: &str, domain: &str, path: &str) {
        self.cookies
            .retain(|c| !(c.name == name && c.domain == domain && c.path == path));
    }

    pub(crate) fn prune_expired(&mut self, now_unix: i64) {
        self.cookies
            .retain(|c| c.expires_unix.map(|e| e > now_unix).unwrap_or(true));
    }
}

/// RFC6265 §5.1.3 domain match (both sides pre-lowercased). A leading dot
/// marks a domain-scope cookie: match the domain itself or any subdomain.
/// Without the dot the cookie is host-only: exact host match only.
fn domain_match(host: &str, cookie_domain: &str) -> bool {
    if let Some(bare) = cookie_domain.strip_prefix('.') {
        host == bare || host.ends_with(cookie_domain)
    } else {
        host == cookie_domain
    }
}

/// RFC6265 §5.1.4 path-match, verbatim: equal paths match; otherwise the
/// cookie-path must be a prefix of the request-path AND either end in `/`
/// or stop exactly at a `/` boundary in the request-path. `/foo` therefore
/// matches `/foo` and `/foo/bar` but never `/foobar`, and `/foo/` never
/// matches the bare `/foo`.
fn path_match(request_path: &str, cookie_path: &str) -> bool {
    if cookie_path == "/" {
        return true;
    }
    if request_path == cookie_path {
        return true;
    }
    request_path.starts_with(cookie_path)
        && (cookie_path.ends_with('/') || request_path.as_bytes()[cookie_path.len()] == b'/')
}

/// Default cookie path = the directory of the request path (§5.1.4).
fn default_cookie_path(url: &url::Url) -> String {
    let path = request_path(url);
    match path.rfind('/') {
        Some(0) => "/".to_string(),
        Some(i) => path[..=i].to_string(),
        None => "/".to_string(),
    }
}

fn request_path(url: &url::Url) -> String {
    let p = url.path();
    if p.is_empty() {
        "/".to_string()
    } else {
        p.to_string()
    }
}

/// Trim, strip optional leading dot, lowercase. Reject empty/undotted-ish junk.
fn normalize_domain(raw: &str) -> Option<String> {
    let d = raw.trim().trim_start_matches('.').to_ascii_lowercase();
    if d.is_empty() || !d.contains('.') {
        return None;
    }
    Some(d)
}

/// Parse the handful of HTTP-date shapes RFC6265 §5.1.1 allows.
/// Returns Unix seconds.
pub(crate) fn parse_http_date(s: &str) -> Option<i64> {
    // Example: "Wed, 09 Jun 2021 10:18:14 GMT"
    let (_weekday, rest) = s.split_once(',').unwrap_or(("", s));
    let mut it = rest.trim().split_whitespace();
    let (Some(day), Some(month), Some(year)) = (it.next(), it.next(), it.next()) else {
        // Old-school "Sun, 06 Nov 1994 ..." always has 5 fields after weekday.
        return None;
    };
    let (Some(hms), Some(_gmt)) = (it.next(), it.next()) else {
        return None;
    };
    let day: i64 = day.parse().ok()?;
    let year: i64 = year.parse().ok()?;
    let month = match month.to_ascii_lowercase().as_str() {
        // 1-indexed, the convention `days_from_civil` implements (a 0-based
        // table here made every month after the first land one calendar month
        // in the past — an Anubis challenge cookie with `Expires` 30 minutes
        // out was stored as already expired and never sent, so every
        // pass-challenge answered "cookies disabled").
        "jan" => 1,
        "feb" => 2,
        "mar" => 3,
        "apr" => 4,
        "may" => 5,
        "jun" => 6,
        "jul" => 7,
        "aug" => 8,
        "sep" => 9,
        "oct" => 10,
        "nov" => 11,
        "dec" => 12,
        _ => return None,
    };
    let mut hms_it = hms.split(':');
    let (Some(h), Some(m), Some(sec)) = (hms_it.next(), hms_it.next(), hms_it.next()) else {
        return None;
    };
    let hours: i64 = h.parse().ok()?;
    let minutes: i64 = m.parse().ok()?;
    let seconds: i64 = sec.parse().ok()?;
    Some(days_from_civil(year, month, day) * 86_400 + hours * 3600 + minutes * 60 + seconds)
}

/// Days since Unix epoch for a civil date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146097 + doe - 719468
}
