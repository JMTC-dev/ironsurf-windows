//! Container-scoped network layer (`ARCHITECTURE.md` §Network layer).
//!
//! Blitz edition (task 7b). Two policies:
//!
//! * [`NetPolicy::Passthrough`] (default): plain `reqwest` (rustls) fetches
//!   served by our own [`BlitzNetProvider`] worker (Blitz has no network stack
//!   of its own once `blitz_net::Provider` is replaced); cookies are **not**
//!   injected for subresources, and isolation comes from the
//!   one-container-per-process model.
//! * [`NetPolicy::Managed`] (`--managed-net`): every http(s) load goes through
//!   our own `reqwest` (rustls) client so we control `Cookie`-header injection
//!   from the container's [`servfox_storage::CookieJar`], capture `Set-Cookie`
//!   responses into the jar, answer from [`servfox_storage::NetCache`] (with
//!   ETag/`If-None-Match` revalidation), and enforce https-only: `http:`
//!   **main-document** navigations are upgraded to `https:`; if the upgrade
//!   fetch fails the load is blocked with an error page. `http:` sub-resources
//!   and `about:`/`file:`/`data:` fixtures keep working unmodified (the
//!   documented "allow http" escape hatch).
//!
//! Main-document navigations always go through [`fetch_document`] in the tab
//! process (never via the provider), so the managed policy applies to
//! top-level requests regardless of provider mode.
//!
//! Everything policy-related is a pure function over plain types so it can be
//! unit-tested without a live network (see `tests/e2e.rs`).
//!
//! NOTE: `blitz_traits::net::NetHandler` has exactly ONE method (`bytes`);
//! status/headers never reach the document, and the trait has no error path —
//! a failed fetch drops the `Box<dyn NetHandler>` (same failure mode as
//! upstream `blitz-net`), see [`deliver`].

use std::fmt;
use std::sync::{Arc, Mutex, OnceLock};

use blitz_traits::net::{Bytes, HeaderMap, NetHandler, NetProvider, NetWaker, Request};
use servfox_storage::{CachedEntry, CookieJar, NetCache, StoreError};
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
    /// Plain reqwest fetches; no cookie injection for subresources.
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
    /// Fetch unmodified (non-http schemes, and — for the https-only escape
    /// hatch — non-main-frame `http:` loads).
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
    upgraded.set_scheme("https").map_err(|_| {}).ok()?;
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
            // Cannot parse? Still fetch as-is.
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

/// Response headers that may be forwarded as-is (hop-by-hop headers and
/// body-transform headers — reqwest already decoded them — never).
// Kept for the shell/interception paths; exercised by the unit tests.
#[allow(dead_code)]
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
// Local schemes (`file:` / `data:` / `about:`)

/// User-Agent sent on every outbound request (we own the whole stack now).
pub const USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64; ServFox/0.1)";

/// Read a `file:` URL from disk (same path semantics as blitz-net: `url.path()`).
pub fn read_file_url(url: &Url) -> Result<Vec<u8>, String> {
    if url.scheme() != "file" {
        return Err(format!("not a file: URL: {url}"));
    }
    std::fs::read(url.path()).map_err(|e| format!("read {}: {e}", url.as_str()))
}

/// Built-in bytes for `about:` URLs we can answer locally (`about:blank`,
/// `about:servfox…`); `None` for anything else.
pub fn builtin_page_bytes(url: &Url) -> Option<Vec<u8>> {
    if url.scheme() != "about" {
        return None;
    }
    let what = url.path().to_ascii_lowercase();
    if what == "blank" {
        Some(
            br#"<!doctype html>
<html><head><meta charset="utf-8"><title>about:blank</title></head>
<body></body></html>
"#
            .to_vec(),
        )
    } else if what.starts_with("servfox") {
        Some(
            format!(
                r#"<!doctype html>
<html><head><meta charset="utf-8"><title>about:servfox</title></head>
<body style="font-family: monospace; margin: 2em">
  <h1>ServFox</h1>
  <p>Container-scoped private browser &mdash; Blitz + Boa engine host.</p>
  <p><code>{url}</code></p>
</body></html>
"#,
                url = url.as_str()
            )
            .into_bytes(),
        )
    } else {
        None
    }
}

/// Decode a `data:` URL (RFC 2397): `data:[<mediatype>][;base64],<payload>`.
pub fn decode_data_url(url: &Url) -> Option<Vec<u8>> {
    let rest = url.as_str().strip_prefix("data:")?;
    let (meta, payload) = rest.split_once(',')?;
    if meta.split(';').any(|p| p.eq_ignore_ascii_case("base64")) {
        decode_base64(payload)
    } else {
        Some(percent_decode(payload.as_bytes()))
    }
}

/// Minimal percent decoding (byte oriented; UTF-8 is reassembled downstream).
fn percent_decode(input: &[u8]) -> Vec<u8> {
    let digit = |c: u8| -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    };
    let mut out = Vec::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        if input[i] == b'%' && i + 2 < input.len() {
            if let (Some(hi), Some(lo)) = (digit(input[i + 1]), digit(input[i + 2])) {
                out.push(hi << 4 | lo);
                i += 3;
                continue;
            }
        }
        out.push(input[i]);
        i += 1;
    }
    out
}

/// Strict base64 decoder (RFC 4648 standard + URL-safe alphabets, optional
/// `=` padding) — enough for `data:` URLs, no extra dependency needed.
pub fn decode_base64(input: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u64> {
        match c {
            b'A'..=b'Z' => Some((c - b'A') as u64),
            b'a'..=b'z' => Some(((c - b'a') as u64) + 26),
            b'0'..=b'9' => Some(((c - b'0') as u64) + 52),
            b'+' | b'-' => Some(62),
            b'/' | b'_' => Some(63),
            _ => None,
        }
    }
    let mut acc: u64 = 0;
    let mut nbits: u32 = 0;
    let mut out: Vec<u8> = Vec::new();
    for &b in input.as_bytes() {
        if matches!(b, b'\n' | b'\r' | b' ' | b'\t') {
            continue;
        }
        if b == b'=' {
            break;
        }
        let v = val(b)?;
        acc = acc << 6 | v;
        nbits += 6;
        if nbits >= 8 {
            nbits -= 8;
            out.push((acc >> nbits) as u8);
            acc &= (1u64 << nbits) - 1; // drop the bits already emitted
        }
    }
    Some(out)
}

// ---------------------------------------------------------------------------
// Worker-thread fetcher (blocking reqwest/rustls on spawned threads)

/// Owns the container's blocking `reqwest` (rustls) client and runs GETs on
/// freshly-spawned worker threads, feeding `Box<dyn NetHandler>` callbacks.
pub struct FetchWorker {
    client: reqwest::blocking::Client,
}

impl FetchWorker {
    /// 30 s-timeout rustls client (keeps tabs responsive on hung servers).
    pub fn new() -> Arc<Self> {
        Self::with_timeout(std::time::Duration::from_secs(30))
    }

    pub fn with_timeout(timeout: std::time::Duration) -> Arc<Self> {
        Arc::new(Self::built(timeout))
    }

    fn built(timeout: std::time::Duration) -> Self {
        let client = reqwest::blocking::Client::builder()
            .use_rustls_tls()
            .redirect(reqwest::redirect::Policy::limited(10))
            .timeout(timeout)
            .build()
            .expect("reqwest client");
        FetchWorker { client }
    }

    pub fn client(&self) -> &reqwest::blocking::Client {
        &self.client
    }

    /// Execute a blocking fetch job on a spawned thread. The job's closure
    /// owns the `Box<dyn NetHandler>` and must call `deliver`/drop exactly
    /// once (the trait has no error path).
    pub fn spawn<F: FnOnce() + Send + 'static>(&self, job: F) {
        std::thread::spawn(job);
    }
}

impl Default for FetchWorker {
    fn default() -> Self {
        Self::built(std::time::Duration::from_secs(30))
    }
}

/// Everything an intercepted load yields after cookies/validators were fed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FetchOutcome {
    /// Post-redirect URL the response actually came from.
    pub final_url: Url,
    pub status: u16,
    /// Header list (original casing, `Set-Cookie`s still included).
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// True when the body came from [`NetCache`] (304 revalidation).
    pub from_cache: bool,
}

fn collect_headers(headers: &reqwest::header::HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .map(|(k, v)| (k.as_str().to_owned(), v.to_str().unwrap_or_default().to_owned()))
        .collect()
}

/// Delivery: the ONE success path of `blitz_traits::net::NetHandler`.
/// Passing `None` on failure models the trait's missing error path exactly
/// like upstream `blitz-net` does (the handler is dropped, the resource
/// silently never loads).
pub fn deliver(handler: Option<Box<dyn NetHandler>>, resolved_url: String, bytes: Vec<u8>) {
    match handler {
        Some(handler) => handler.bytes(resolved_url, Bytes::from(bytes)),
        None => drop(handler),
    }
}

/// Plain (`Passthrough`) HTTP GET: our User-Agent plus whatever the Blitz
/// `Request` carried; no cookies, no cache.
pub fn plain_http_fetch(
    client: &reqwest::blocking::Client,
    url: &Url,
    extra: Option<&HeaderMap>,
) -> Result<FetchOutcome, String> {
    let mut req = client.get(url.as_str()).header("User-Agent", USER_AGENT);
    if let Some(headers) = extra {
        if !headers.is_empty() {
            req = req.headers(headers.clone());
        }
    }
    let response = req.send().map_err(|e| format!("fetch {url}: {e}"))?;
    let status = response.status().as_u16();
    let final_url = response.url().clone();
    let headers = collect_headers(response.headers());
    let body = response
        .bytes()
        .map(|b| b.to_vec())
        .map_err(|e| format!("body {url}: {e}"))?;
    Ok(FetchOutcome {
        final_url,
        status,
        headers,
        body,
        from_cache: false,
    })
}

/// Managed HTTP GET: injects the container jar's `Cookie` header, sends the
/// cached entry's `If-None-Match`/`If-Modified-Since` validators, captures
/// every `Set-Cookie` (against the **final** URL) and serves/stores
/// [`NetCache`] entries. `extra` (from the Blitz `Request`) never overrides
/// our `Cookie` header.
pub fn managed_http_fetch(
    client: &reqwest::blocking::Client,
    url: &Url,
    jar: &mut CookieJar,
    cache: &NetCache,
    now: i64,
    extra: Option<&HeaderMap>,
) -> Result<FetchOutcome, String> {
    let mut req = client.get(url.as_str()).header("User-Agent", USER_AGENT);
    if let Some(headers) = extra {
        for (name, value) in headers {
            if name.as_str().eq_ignore_ascii_case("cookie") {
                continue; // the jar owns the Cookie header
            }
            req = req.header(name.clone(), value.clone());
        }
    }
    if let Some(cookie) = jar.cookie_header(url, now) {
        req = req.header(reqwest::header::COOKIE, cookie);
    }

    let cached = cache.lookup(url.as_str());
    if let Some(entry) = cached.as_ref() {
        let (etag, last_modified) = validators(&entry.headers);
        if let Some(etag) = etag {
            req = req.header(reqwest::header::IF_NONE_MATCH, etag);
        } else if let Some(lm) = last_modified {
            req = req.header(reqwest::header::IF_MODIFIED_SINCE, lm);
        }
    }

    let response = req.send().map_err(|e| format!("fetch {url}: {e}"))?;
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
    let headers = collect_headers(response.headers());
    let body = response
        .bytes()
        .map(|b| b.to_vec())
        .map_err(|e| format!("body {url}: {e}"))?;

    capture_set_cookies(
        jar,
        &final_url,
        set_cookies.iter().map(String::as_str),
        now,
    );

    let (etag, last_modified) = validators(&headers);
    let empty = || CachedEntry {
        status: 0,
        headers: Vec::new(),
        body: Vec::new(),
        etag: None,
        last_modified: None,
        cached_at: 0,
    };
    match revalidate(
        &cached.clone().unwrap_or_else(empty),
        status,
        etag.as_deref(),
        last_modified.as_deref(),
    ) {
        Revalidate::ServeCached => {
            let entry = cached.expect("ServeCached implies a cached entry existed");
            Ok(FetchOutcome {
                final_url,
                status: entry.status,
                headers: entry.headers,
                body: entry.body,
                from_cache: true,
            })
        }
        Revalidate::UseFresh { .. } => {
            if status < 400 {
                cache.store(url.as_str(), status, &headers, body.clone(), now);
            }
            Ok(FetchOutcome {
                final_url,
                status,
                headers,
                body,
                from_cache: false,
            })
        }
    }
}

// ---------------------------------------------------------------------------
// BlitzNetProvider — `blitz_traits::net::NetProvider` for DocumentConfig

/// Sub-resource network provider handed to
/// `DocumentConfig { net_provider: Some(Arc<BlitzNetProvider>) }`.
///
/// `NetProvider`/`NetHandler` are `Send + Sync + 'static` (called from Blitz
/// worker contexts), so the mutable container state ([`CookieJar`],
/// [`NetCache`]) lives behind `Mutex` — never `RefCell`.
pub struct BlitzNetProvider {
    policy: NetPolicy,
    worker: Arc<FetchWorker>,
    jar: Arc<Mutex<CookieJar>>,
    cache: Arc<Mutex<NetCache>>,
    waker: Arc<Mutex<Option<Arc<dyn NetWaker>>>>,
}

impl BlitzNetProvider {
    /// Jar + cache come from the container's profile dir
    /// (`ContainerStore::cookie_jar` / `::cache`).
    pub fn new(policy: NetPolicy, jar: CookieJar, cache: NetCache) -> Self {
        BlitzNetProvider {
            policy,
            worker: FetchWorker::new(),
            jar: Arc::new(Mutex::new(jar)),
            cache: Arc::new(Mutex::new(cache)),
            waker: Arc::new(Mutex::new(None)),
        }
    }

    /// Ready-to-share (`Arc<dyn NetProvider>`) form for `DocumentConfig`.
    #[allow(dead_code)] // convenience constructor (used by tests/embedders)
    pub fn shared(policy: NetPolicy, jar: CookieJar, cache: NetCache) -> Arc<Self> {
        Arc::new(Self::new(policy, jar, cache))
    }

    /// Builder: poke the tab loop when a fetch settles — Blitz has no error
    /// delivery, so the waker is the only way failed sub-fetches wake us.
    #[must_use]
    pub fn with_waker(self, waker: Arc<dyn NetWaker>) -> Self {
        *self.waker.lock().expect("waker mutex") = Some(waker);
        self
    }

    /// Builder: share one [`FetchWorker`] (and its client) with the runtime.
    #[must_use]
    #[allow(dead_code)] // worker sharing is wired when fetch_document moves in-process
    pub fn with_worker(mut self, worker: Arc<FetchWorker>) -> Self {
        self.worker = worker;
        self
    }

    pub fn policy(&self) -> NetPolicy {
        self.policy
    }

    /// Flush the container cookie jar to disk (tab `Destroy` path).
    pub fn save_cookies(&self) -> Result<(), StoreError> {
        self.jar.lock().expect("jar mutex poisoned").save()
    }

    #[allow(dead_code)] // waker fires via the NetWaker installed by with_waker
    fn wake(&self, doc_id: usize) {
        let guard = self.waker.lock().expect("waker mutex poisoned");
        if let Some(waker) = guard.as_ref() {
            waker.wake(doc_id);
        }
    }

    fn spawn_http(
        &self,
        doc_id: usize,
        url: Url,
        request: Request,
        handler: Box<dyn NetHandler>,
    ) {
        let worker = Arc::clone(&self.worker);
        let managed = self.policy == NetPolicy::Managed;
        let jar = Arc::clone(&self.jar);
        let cache = Arc::clone(&self.cache);
        let waker = self.waker.lock().expect("waker mutex poisoned").clone();
        self.worker.spawn(move || {
            let signal = request.signal.clone();
            let outcome = if managed {
                let mut jar = jar.lock().expect("jar mutex poisoned");
                let cache = cache.lock().expect("cache mutex poisoned");
                managed_http_fetch(worker.client(), &url, &mut jar, &cache, now_unix(), None)
            } else {
                plain_http_fetch(worker.client(), &url, Some(&request.headers))
            };
            // Same as upstream blitz-net: wake the doc thread even on error,
            // and drop the handler when the fetch failed (no error path).
            if let Some(waker) = waker {
                waker.wake(doc_id);
            }
            let aborted = signal.as_ref().is_some_and(|s| s.aborted());
            match outcome {
                Ok(o) if !aborted && o.status < 400 => {
                    deliver(Some(handler), o.final_url.to_string(), o.body)
                }
                _ => deliver(None, String::new(), Vec::new()),
            }
        });
    }
}

impl NetProvider for BlitzNetProvider {
    fn fetch(&self, doc_id: usize, request: Request, handler: Box<dyn NetHandler>) {
        let url = request.url.clone();
        match url.scheme() {
            // Local schemes complete inline — the doc thread will resolve again.
            "data" => match decode_data_url(&url) {
                Some(bytes) => deliver(Some(handler), url.to_string(), bytes),
                None => deliver(None, String::new(), Vec::new()),
            },
            "file" => match read_file_url(&url) {
                Ok(bytes) => deliver(Some(handler), url.to_string(), bytes),
                Err(_) => deliver(None, String::new(), Vec::new()),
            },
            "about" => match builtin_page_bytes(&url) {
                Some(bytes) => deliver(Some(handler), url.to_string(), bytes),
                None => deliver(None, String::new(), Vec::new()),
            },
            "http" | "https" => self.spawn_http(doc_id, url, request, handler),
            // javascript:/blob:/…: silently unsupported (dropped handler).
            _ => deliver(None, String::new(), Vec::new()),
        }
    }
}

// ---------------------------------------------------------------------------
// Main-document fetch (always ours, both policies; task 7c drives this)

/// A fetched main document: post-redirect URL + status + raw bytes
/// (`String::from_utf8_lossy` at parse time).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DocFetch {
    pub url: Url,
    pub status: u16,
    pub body: Vec<u8>,
}

impl DocFetch {
    /// Body as UTF-8 text (lossy, like every browser's fallback decoding).
    pub fn body_text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// Why a navigation-level fetch could not produce a [`DocFetch`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NetError {
    /// Managed policy: the `http:` → `https:` upgrade failed, so the
    /// insecure navigation is blocked ([`blocked_error_page`] to render).
    Blocked {
        request: Url,
        https: Url,
        reason: String,
    },
    /// The (already https, or passthrough) fetch failed.
    Transport { url: Url, message: String },
    /// I/O error reading a `file:` URL.
    Io { url: Url, message: String },
    /// Scheme we refuse to navigate to (e.g. `javascript:`).
    Unsupported { url: Url },
}

impl fmt::Display for NetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NetError::Blocked {
                request,
                https,
                reason,
            } => write!(
                f,
                "blocked insecure navigation {request}: https upgrade to {https} failed ({reason})"
            ),
            NetError::Transport { url, message } => write!(f, "fetch {url}: {message}"),
            NetError::Io { url, message } => write!(f, "read {url}: {message}"),
            NetError::Unsupported { url } => write!(f, "unsupported scheme: {url}"),
        }
    }
}

impl std::error::Error for NetError {}

impl NetError {
    /// Error-page HTML for this failure, if we render one (only Blocked).
    pub fn error_page(&self) -> Option<String> {
        match self {
            NetError::Blocked {
                request,
                https,
                reason,
            } => Some(blocked_error_page(request, https, reason)),
            _ => None,
        }
    }
}

/// Shared client for main-document fetches (the runtime may also pass its own
/// [`FetchWorker`] to the provider so everything shares one rustls pool).
fn shared_worker() -> Arc<FetchWorker> {
    static WORKER: OnceLock<Arc<FetchWorker>> = OnceLock::new();
    WORKER.get_or_init(FetchWorker::new).clone()
}

/// Fetch a **main document** under `policy` (never via the NetProvider, per
/// ARCHITECTURE). `file:` is read from disk and never upgraded;
/// `about:`/local fixture pages are answered from built-ins; `data:` decodes
/// inline. `Passthrough` fetches http(s) as-is; `Managed` injects cookies,
/// captures `Set-Cookie`, serves/stores [`NetCache`] entries with ETag
/// revalidation, and https-upgrades `http:` navigations — blocking with
/// [`NetError::Blocked`] (carrying the error page) when the upgrade fails.
pub fn fetch_document(
    url: &Url,
    policy: NetPolicy,
    jar: &mut CookieJar,
    cache: &NetCache,
    now: i64,
) -> Result<DocFetch, NetError> {
    let worker = shared_worker();
    fetch_document_with_worker(url, policy, jar, cache, now, &worker)
}

fn fetch_document_with_worker(
    url: &Url,
    policy: NetPolicy,
    jar: &mut CookieJar,
    cache: &NetCache,
    now: i64,
    worker: &FetchWorker,
) -> Result<DocFetch, NetError> {
    // Local schemes: identical under both policies.
    match url.scheme() {
        "file" => {
            let body = read_file_url(url).map_err(|e| NetError::Io {
                url: url.clone(),
                message: e,
            })?;
            return Ok(DocFetch {
                url: url.clone(),
                status: 200,
                body,
            });
        }
        "about" => {
            if let Some(body) = builtin_page_bytes(url) {
                return Ok(DocFetch {
                    url: url.clone(),
                    status: 200,
                    body,
                });
            }
        }
        "data" => {
            if let Some(body) = decode_data_url(url) {
                return Ok(DocFetch {
                    url: url.clone(),
                    status: 200,
                    body,
                });
            }
        }
        _ => {}
    }

    if !is_http_scheme(url) {
        return Err(NetError::Unsupported { url: url.clone() });
    }

    match plan(policy, url, true) {
        // Escape hatch keeps working: passthrough fetches exactly what was asked.
        Plan::Pass => plain_http_fetch(worker.client(), url, None)
            .map(|o| DocFetch {
                url: o.final_url,
                status: o.status,
                body: o.body,
            })
            .map_err(|message| NetError::Transport {
                url: url.clone(),
                message,
            }),
        Plan::Fetch(target) => managed_http_fetch(worker.client(), &target, jar, cache, now, None)
            .map(|o| DocFetch {
                url: o.final_url,
                status: o.status,
                body: o.body,
            })
            .map_err(|message| NetError::Transport {
                url: url.clone(),
                message,
            }),
        Plan::Upgrade(https) => {
            match managed_http_fetch(worker.client(), &https, jar, cache, now, None) {
                Ok(o) => Ok(DocFetch {
                    url: o.final_url,
                    status: o.status,
                    body: o.body,
                }),
                Err(reason) => Err(NetError::Blocked {
                    request: url.clone(),
                    https,
                    reason,
                }),
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

    // -- task 7b: Blitz NetProvider + fetch_document --------------------------

    /// Test-only `NetHandler` recording the single callback the trait has.
    struct MockHandler {
        log: Arc<Mutex<Vec<(String, Vec<u8>)>>>,
    }

    impl NetHandler for MockHandler {
        fn bytes(self: Box<Self>, resolved_url: String, bytes: Bytes) {
            *self.log.lock().unwrap() = vec![(resolved_url, bytes.to_vec())];
        }
    }

    fn mock_handler() -> (Box<dyn NetHandler>, Arc<Mutex<Vec<(String, Vec<u8>)>>>) {
        let log = Arc::new(Mutex::new(Vec::new()));
        (
            Box::new(MockHandler {
                log: Arc::clone(&log),
            }),
            log,
        )
    }

    fn wait_for(log: &Arc<Mutex<Vec<(String, Vec<u8>)>>>) -> Vec<(String, Vec<u8>)> {
        for _ in 0..400 {
            {
                let guard = log.lock().unwrap();
                if !guard.is_empty() {
                    return guard.clone();
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        panic!("no handler.bytes() delivery within 10s");
    }

    #[test]
    fn net_provider_is_send_sync_and_not_noop() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = servfox_storage::ContainerStore::from_profile_dir(
            servfox_common::ContainerId(7),
            dir.path(),
        );
        let provider = BlitzNetProvider::shared(
            NetPolicy::Managed,
            store.cookie_jar(),
            store.cache(),
        );
        fn assert_net_provider(_: &Arc<dyn NetProvider>) {}
        let dyn_provider: Arc<dyn NetProvider> = provider;
        assert_net_provider(&dyn_provider);
        assert!(
            !dyn_provider.is_noop(),
            "our provider must never report noop (else Blitz blocks critical resources)"
        );
    }

    #[test]
    fn provider_delivers_file_and_data_schemes_to_handler() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = servfox_storage::ContainerStore::from_profile_dir(
            servfox_common::ContainerId(7),
            dir.path(),
        );
        let provider = BlitzNetProvider::new(NetPolicy::Managed, store.cookie_jar(), store.cache());

        // file: sub-resource (e.g. an <img> from a file:// fixture page).
        let fixture = dir.path().join("sub.txt");
        std::fs::write(&fixture, b"file bytes").expect("write fixture");
        let file_url = Url::from_file_path(&fixture).expect("file url");
        let (handler, log) = mock_handler();
        provider.fetch(1, Request::get(file_url.clone()), handler);
        let got = wait_for(&log);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, file_url.as_str());
        assert_eq!(got[0].1, b"file bytes");

        // data: — percent-encoded and base64 payloads.
        let (handler, log) = mock_handler();
        provider.fetch(
            1,
            Request::get(url("data:text/html,hi%21")),
            handler,
        );
        let got = wait_for(&log);
        assert_eq!(got[0].1, b"hi!");

        let (handler, log) = mock_handler();
        provider.fetch(
            1,
            Request::get(url("data:text/plain;base64,aGVsbG8=")),
            handler,
        );
        let got = wait_for(&log);
        assert_eq!(got[0].1, b"hello");
    }

    #[test]
    fn about_urls_answer_from_builtins() {
        let blank = url("about:blank");
        let bytes = builtin_page_bytes(&blank).expect("blank page");
        let text = String::from_utf8(bytes).expect("utf8");
        assert!(text.contains("about:blank"));

        let about = url("about:servfox");
        let text =
            String::from_utf8(builtin_page_bytes(&about).expect("servfox page")).expect("utf8");
        assert!(text.contains("ServFox"));

        assert!(builtin_page_bytes(&url("about:whatever")).is_none());
    }

    #[test]
    fn fetch_document_file_and_about_without_network() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = servfox_storage::ContainerStore::from_profile_dir(
            servfox_common::ContainerId(7),
            dir.path(),
        );
        let mut jar = store.cookie_jar();
        let cache = store.cache();
        let now = 1_700_000_000;

        // file:// documents load under BOTH policies (never upgraded).
        let fixture = dir.path().join("index.html");
        std::fs::write(&fixture, b"<html><body>fixture</body></html>").expect("write");
        let file_url = Url::from_file_path(&fixture).expect("file url");
        for policy in [NetPolicy::Passthrough, NetPolicy::Managed] {
            let doc = fetch_document_with_worker(
                &file_url,
                policy,
                &mut jar,
                &cache,
                now,
                worker_for_test(),
            )
            .expect("file document");
            assert_eq!(doc.status, 200);
            assert_eq!(doc.url, file_url);
            assert!(doc.body_text().contains("fixture"));
        }

        // about: built-ins.
        let doc = fetch_document_with_worker(
            &url("about:servfox"),
            NetPolicy::Managed,
            &mut jar,
            &cache,
            now,
            worker_for_test(),
        )
        .expect("about page");
        assert!(doc.body_text().contains("ServFox"));

        // Unsupported schemes are refused, not fetched.
        let err = fetch_document_with_worker(
            &url("javascript:alert(1)"),
            NetPolicy::Managed,
            &mut jar,
            &cache,
            now,
            worker_for_test(),
        )
        .expect_err("javascript: must be refused");
        assert!(matches!(err, NetError::Unsupported { .. }));
    }

    #[test]
    fn managed_document_https_upgrade_failure_blocks() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = servfox_storage::ContainerStore::from_profile_dir(
            servfox_common::ContainerId(7),
            dir.path(),
        );
        let mut jar = store.cookie_jar();
        let cache = store.cache();
        // Port 1 on loopback: connect is refused instantly, no real network.
        let http = url("http://127.0.0.1:1/");
        let err = fetch_document_with_worker(
            &http,
            NetPolicy::Managed,
            &mut jar,
            &cache,
            1_700_000_000,
            worker_for_test(),
        )
        .expect_err("upgraded fetch must fail (connection refused)");
        let blocked = match err {
            NetError::Blocked {
                request,
                https,
                reason,
            } => {
                assert_eq!(request, http);
                assert_eq!(https, url("https://127.0.0.1:1/"));
                assert!(!reason.is_empty());
                NetError::Blocked {
                    request,
                    https,
                    reason,
                }
            }
            other => panic!("expected Blocked, got {other:?}"),
        };
        let page = blocked.error_page().expect("blocked error page");
        assert!(page.contains("http://127.0.0.1:1/"));
        assert!(page.contains("https://127.0.0.1:1/"));

        // Passthrough to the same dead port is a plain transport error.
        let err = fetch_document_with_worker(
            &http,
            NetPolicy::Passthrough,
            &mut jar,
            &cache,
            1_700_000_000,
            worker_for_test(),
        )
        .expect_err("transport failure surfaces");
        assert!(matches!(err, NetError::Transport { .. }));
    }

    #[test]
    fn managed_cookie_header_shape_including_secure_only_over_https() {
        let mut jar = CookieJar::new();
        let now = 1_700_000_000;
        capture_set_cookies(
            &mut jar,
            &url("https://example.com/"),
            ["b=2; Path=/; Secure", "a=1; Path=/"],
            now,
        );
        // The header the managed fetcher would attach (jar-side assembly).
        let https_header = jar
            .cookie_header(&url("https://example.com/path"), now)
            .expect("https header");
        let pairs: std::collections::BTreeSet<&str> = https_header.split("; ").collect();
        assert_eq!(
            pairs,
            std::collections::BTreeSet::from(["a=1", "b=2"]),
            "unexpected assembled header {https_header:?}"
        );
        assert!(!https_header.contains(','), "pairs are '; '-joined, not comma");
        // Over http the Secure cookie must drop out of the assembled header.
        let http_header = jar
            .cookie_header(&url("http://example.com/path"), now)
            .expect("http header");
        assert_eq!(http_header, "a=1");
    }

    #[test]
    fn netcache_etag_round_trip_decides_revalidation() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = servfox_storage::ContainerStore::from_profile_dir(
            servfox_common::ContainerId(7),
            dir.path(),
        );
        let cache = store.cache();
        let now = 1_700_000_000;
        let target = "https://example.com/page";
        let headers = vec![
            ("content-type".to_string(), "text/html".to_string()),
            ("ETag".to_string(), "\"v7\"".to_string()),
        ];
        cache.store(target, 200, &headers, b"fresh body".to_vec(), now);

        let entry = cache.lookup(target).expect("stored entry");
        assert_eq!(entry.status, 200);
        assert_eq!(entry.body, b"fresh body");
        // Revalidation: 304 + same ETag → ServeCached (this is exactly the
        // decision managed_http_fetch makes from these values).
        let (etag, lm) = validators(&entry.headers);
        assert_eq!(etag.as_deref(), Some("\"v7\""));
        assert!(lm.is_none());
        assert_eq!(
            revalidate(&entry, 304, Some("\"v7\""), None),
            Revalidate::ServeCached
        );
        // A different ETag → fresh body replaces the cache entry.
        assert!(matches!(
            revalidate(&entry, 304, Some("\"v8\""), None),
            Revalidate::UseFresh { status: 304, .. }
        ));
    }

    #[test]
    fn data_url_decode_helpers() {
        assert_eq!(decode_base64("aGVsbG8=").as_deref(), Some(b"hello".as_slice()));
        assert_eq!(decode_base64("aGVsbG8").as_deref(), Some(b"hello".as_slice()));
        assert_eq!(decode_base64("").as_deref(), Some(b"".as_slice()));
        assert_eq!(decode_base64("////").as_deref(), Some([0xff, 0xff, 0xff].as_slice()));
        // URL-safe alphabet maps to the same values as the standard one.
        assert_eq!(decode_base64("____").as_deref(), Some([0xff, 0xff, 0xff].as_slice()));
        assert_eq!(decode_base64("__8="), decode_base64("//8="));
        assert_eq!(
            decode_base64("TWFuw6c=").as_deref(),
            Some("Man\u{E7}".as_bytes())
        );
        assert_eq!(
            decode_base64("TWF"), // truncated partial group must still decode to "Ma"
            decode_base64("TWF=")
        );
        assert_eq!(decode_base64("a!b"), None);
        assert_eq!(
            decode_data_url(&url("data:text/html,%3Cb%3Ehi")).as_deref(),
            Some(b"<b>hi".as_slice())
        );
        assert_eq!(
            decode_data_url(&url("data:text/html;charset=utf-8,x")),
            Some(b"x".to_vec())
        );
        assert_eq!(decode_data_url(&url("data:text/html")), None);
    }

    // Small helper: dedicated short-timeout client for connection-refused tests.
    #[allow(clippy::redundant_clone)]
    fn worker_for_test() -> &'static FetchWorker {
        static WORKER: OnceLock<Arc<FetchWorker>> = OnceLock::new();
        WORKER
            .get_or_init(|| FetchWorker::with_timeout(std::time::Duration::from_secs(5)))
    }
}
