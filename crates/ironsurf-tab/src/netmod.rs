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
//!   from the container's [`ironsurf_storage::CookieJar`], capture `Set-Cookie`
//!   responses into the jar, answer from [`ironsurf_storage::NetCache`] (with
//!   ETag/`If-None-Match` revalidation), and enforce https-only: `http:`
//!   **main-document** navigations are upgraded to `https:`; if the upgrade
//!   fetch fails the load is blocked with an error page. `http:` sub-resources
//!   and `about:`/`file:`/`data:` fixtures keep working unmodified (the
//!   documented "allow http" escape hatch).
//!
//! Main-document navigations always go through [`fetch_document`] in the tab
//! process (never via the provider), so **the container cookie jar always
//! applies to top-level requests, regardless of provider mode**: cookies a
//! site sets on a navigation are captured and the matching ones are sent
//! back on every later navigation and on every redirect hop. What
//! `Passthrough` skips is only the https-upgrade and the [`NetCache`]; what
//! it additionally skips for *subresources* is cookie handling entirely (the
//! documented escape hatch).
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
use ironsurf_storage::{CachedEntry, CookieJar, NetCache, StoreError};
use url::Url;

// Every request/response decision is logged to stderr at `debug`; only things
// that silently broke a load (transport failures, dropped subresources) reach
// `warn`, so a normal page stays quiet at the default level.
use ironsurf_common::{debug, trace, warn};
use ironsurf_common::NetEvent;

// ---------------------------------------------------------------------------
// DevTools network reporting (`FromTab::NetworkEvent`)

/// Sink for network events: called exactly once per *completed or failed*
/// request (redirect hops count as their own events; a response served
/// entirely from [`NetCache`] without touching the wire is not a request and
/// stays silent). `None` keeps the fetcher quiet — every call site that does
/// not care about the DevTools network panel passes `None`.
pub type NetReport<'a> = Option<&'a (dyn Fn(NetEvent) + Send + Sync)>;

/// A completed request: status + size + measured wall clock, no error.
fn net_ok(method: &str, url: &Url, status: u16, response_bytes: u64, duration_ms: u64) -> NetEvent {
    NetEvent {
        method: method.to_owned(),
        url: url.as_str().to_owned(),
        status: Some(status),
        response_bytes: Some(response_bytes),
        duration_ms: Some(duration_ms),
        error: None,
    }
}

/// A failed request: `status == None`, `error == Some(reason)`.
fn net_failed(method: &str, url: &Url, duration_ms: u64, error: String) -> NetEvent {
    NetEvent {
        method: method.to_owned(),
        url: url.as_str().to_owned(),
        status: None,
        response_bytes: None,
        duration_ms: Some(duration_ms),
        error: Some(error),
    }
}

fn elapsed_ms(started: std::time::Instant) -> u64 {
    started.elapsed().as_millis() as u64
}

/// A loggable rendering of `url`: `data:` URLs are reduced to their length so
/// a 2 MB inline asset cannot spray itself across stderr.
pub fn url_for_log(url: &Url) -> String {
    if url.scheme() == "data" {
        format!("data: URL ({} bytes)", url.as_str().len())
    } else {
        url.as_str().to_owned()
    }
}

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

/// Classify one **main-document** navigation. Unlike subresources (where
/// `Passthrough` keeps the documented no-cookie escape hatch), the top-level
/// fetch always rides on [`managed_http_fetch`] — cookies are always sent
/// and captured for navigations. `Passthrough` only skips the https-upgrade
/// (and the cache, via the `cacheable` argument at the call site); `Managed`
/// upgrades `http:` and blocks with an error page when the upgrade fails.
pub fn plan_document(policy: NetPolicy, request_url: &Url) -> Plan {
    if !is_http_scheme(request_url) {
        return Plan::Pass; // local schemes never reach the network layer
    }
    match request_url.scheme() {
        "https" => Plan::Fetch(request_url.clone()),
        _ => match upgrade_to_https(request_url) {
            Some(https) if policy == NetPolicy::Managed => Plan::Upgrade(https),
            // Passthrough (or an unparseable upgrade): fetch the URL as
            // asked for — still with cookies from the container jar.
            _ => Plan::Fetch(request_url.clone()),
        },
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
    <p>IronSurf refuses to load <code>{request}</code>: this tab enforces
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
/// This is the **default**: a tab launched with `--user-agent`, or fed a
/// `ToTab::SetUserAgent`, sends that value instead (see [`SharedUserAgent`]).
pub const USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64; IronSurf/0.1)";

/// The tab's **effective User-Agent** in a shared mutable cell: the runtime
/// owns it (the `--user-agent` flag seeds it, `ToTab::SetUserAgent` replaces
/// it), the fetchers read it at request time, and the JS bindings report it
/// through `navigator.userAgent`. One cell per tab — a change takes effect on
/// every request issued after it without touching any request builder.
pub type SharedUserAgent = Arc<Mutex<String>>;

/// Resolve a `ToTab::SetUserAgent` value (and the `--user-agent` flag) to the
/// effective UA: `Some(v)` is used trimmed, a value that is empty after
/// trimming resets to the built-in default, and `None` is the default.
pub fn resolve_user_agent(value: Option<&str>) -> String {
    match value {
        Some(raw) => {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                USER_AGENT.to_owned()
            } else {
                trimmed.to_owned()
            }
        }
        None => USER_AGENT.to_owned(),
    }
}

/// A fresh effective-UA cell seeded from a launch override (`None`/blank ⇒
/// the built-in default).
#[must_use]
pub fn new_user_agent(initial: Option<&str>) -> SharedUserAgent {
    Arc::new(Mutex::new(resolve_user_agent(initial)))
}

/// Read the current value of a UA cell (poison-tolerant: the string is the
/// whole payload, a poisoned lock still holds the honest last value).
#[must_use]
pub fn read_user_agent(user_agent: &SharedUserAgent) -> String {
    user_agent
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

/// Read a `file:` URL from disk (same path semantics as blitz-net: `url.path()`).
pub fn read_file_url(url: &Url) -> Result<Vec<u8>, String> {
    if url.scheme() != "file" {
        return Err(format!("not a file: URL: {url}"));
    }
    std::fs::read(url.path()).map_err(|e| format!("read {}: {e}", url.as_str()))
}

/// The browser's internal scheme. `ironsurf://…` URLs are **always** answered
/// locally by the tab — they never reach the network, are never upgraded,
/// and never carry cookies. `ironsurf://welcome` is the default start page
/// (`ironsurf_common::WELCOME_URL`).
pub const INTERNAL_SCHEME: &str = "ironsurf";

/// Which built-in page an `ironsurf://…` URL selects. `url` puts the first
/// authority segment of a non-special scheme into `host()` and the rest into
/// `path()`, so `ironsurf://welcome`, `ironsurf:///welcome` and
/// `ironsurf://ironsurf/welcome` all select `welcome`. Lowercased (schemes and
/// hosts are case-insensitive).
pub fn internal_page_name(url: &Url) -> String {
    let host = url.host_str().unwrap_or("").to_ascii_lowercase();
    let path = url.path().trim_matches('/').to_ascii_lowercase();
    if host.is_empty() || host == INTERNAL_SCHEME {
        path
    } else {
        host
    }
}

/// Escape text for HTML PCDATA/attributes (internal pages interpolate the
/// requested URL, which is attacker-influenced text — same rule as the
/// `data:` source view: never parse user text as markup).
pub fn escape_html_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// Built-in bytes for the internal `ironsurf:` scheme: `ironsurf://welcome`
/// (the start page); any other page name renders an honest "no such internal
/// page" instead of a fetch error, so typos land on a real page.
pub fn internal_page_bytes(url: &Url) -> Option<Vec<u8>> {
    if url.scheme() != INTERNAL_SCHEME {
        return None;
    }
    let page = internal_page_name(url);
    let shown = escape_html_text(url.as_str());
    let html = if page == "welcome" || page.is_empty() {
        // The welcome page doubles as the form demo: it echoes whatever
        // query string a GET submission against it carried.
        let query = match url.query() {
            Some(q) if !q.is_empty() => escape_html_text(&q.replace('+', " ")),
            _ => "nothing yet".to_owned(),
        };
        WELCOME_PAGE
            .replace("{url}", &shown)
            .replace("{query}", &query)
    } else {
        INTERNAL_NOT_FOUND
            .replace("{page}", &escape_html_text(&page))
            .replace("{url}", &shown)
    };
    Some(html.into_bytes())
}

/// The built-in start page (`ironsurf://welcome`). Self-contained by design:
/// inline styles only, no subresources, no scripts — it must render on a
/// machine with no network, which is exactly when it matters most.
const WELCOME_PAGE: &str = r#"<!doctype html>
<html>
<head>
<meta charset="utf-8">
<title>Welcome to IronSurf</title>
<style>
  body { margin: 0; background: #14161a; color: #e6e6e6;
         font-family: monospace; line-height: 1.5; }
  .wrap { max-width: 46em; margin: 0 auto; padding: 3em 1.5em; }
  h1 { color: #8fce8f; margin: 0 0 0.2em 0; }
  .sub { color: #9aa0a6; margin: 0 0 2em 0; }
  h2 { color: #8fce8f; font-size: 1.1em; margin: 2em 0 0.5em 0; }
  code, kbd { background: #1f2227; padding: 0.1em 0.4em; border-radius: 3px; }
  ul { padding-left: 1.2em; }
  a { color: #8fce8f; }
  .foot { margin-top: 3em; color: #6b7178; font-size: 0.9em; }
</style>
</head>
<body>
<div class="wrap">
  <h1>IronSurf</h1>
  <p class="sub">Container-scoped private browsing &mdash; Blitz + Boa engine host,
     every tab a sandboxed OS process.</p>

  <h2>Getting around</h2>
  <ul>
    <li><kbd>Ctrl-L</kbd> &mdash; focus the URL bar; type a host or a search</li>
    <li><kbd>Ctrl-T</kbd> &mdash; new tab &middot; <kbd>Ctrl-W</kbd> &mdash; close tab</li>
    <li><kbd>Ctrl-Tab</kbd> &mdash; switch tab &middot; <kbd>Ctrl-R</kbd> &mdash; reload</li>
    <li>middle-click a link &mdash; open it in a new tab of this container</li>
  </ul>

  <h2>Containers</h2>
  <p>Each tab lives in a container with its own cookie jar, cache and profile
     directory on disk. Tabs in different containers share nothing &mdash; not
     memory, not cookies, not cache. Open the container manager from the tab
     bar to create, color and reassign.</p>

  <h2>Network policy</h2>
  <p>Typed <code>host/path</code> navigations are upgraded to HTTPS; an upgrade
     that fails is <em>blocked</em>, not silently downgraded. Internal
     <code>ironsurf://</code> pages like this one never touch the network.</p>

  <h2>Try a form</h2>
  <p>HTML forms work end to end &mdash; text fields, selects, checkboxes,
     radios, textareas, GET and POST. This page answers submissions itself:
     press Search and the browser builds the entry list and re-navigates with
     your values as a query string.</p>
  <form method="get">
    <p>
      <input type="text" name="q" value="hello" size="18">
      <select name="where">
        <option value="everywhere" selected>everywhere</option>
        <option value="internal">internal pages</option>
        <option value="nothing">nowhere</option>
      </select>
      <input type="checkbox" name="safe" value="on" checked>
      <button type="submit">Search</button>
    </p>
  </form>
  <p>You submitted: <code>{query}</code></p>

  <p class="foot">You are looking at <code>{url}</code> &mdash; a page built into
     the tab process. It is the default start page.</p>
</div>
</body>
</html>
"#;

/// Shown for an unknown `ironsurf://<page>` so a typo is a page, not an error.
const INTERNAL_NOT_FOUND: &str = r#"<!doctype html>
<html>
<head>
<meta charset="utf-8">
<title>ironsurf: no such page</title>
<style>
  body { margin: 0; background: #14161a; color: #e6e6e6;
         font-family: monospace; line-height: 1.5; }
  .wrap { max-width: 40em; margin: 0 auto; padding: 3em 1.5em; }
  h1 { color: #e57373; margin: 0 0 0.2em 0; }
  a { color: #8fce8f; }
  code { background: #1f2227; padding: 0.1em 0.4em; border-radius: 3px; }
</style>
</head>
<body>
<div class="wrap">
  <h1>No such internal page</h1>
  <p><code>ironsurf://{page}</code> is not built into the browser.
     The request never left this process &mdash; nothing failed on a network.</p>
  <p>Known page: <a href="ironsurf://welcome"><code>ironsurf://welcome</code></a></p>
  <p><code>{url}</code></p>
</div>
</body>
</html>
"#;

/// Built-in bytes for `about:` URLs we can answer locally (`about:blank`,
/// `about:ironsurf…`); `None` for anything else.
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
    } else if what.starts_with("ironsurf") {
        Some(
            format!(
                r#"<!doctype html>
<html><head><meta charset="utf-8"><title>about:ironsurf</title></head>
<body style="font-family: monospace; margin: 2em">
  <h1>IronSurf</h1>
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

/// A `data:` URL (RFC 2397) parsed into media type + decoded bytes.
///
/// `data:[<mediatype>][;base64],<payload>` where `mediatype` is
/// `type/subtype[;charset=…]`. Callers need the media type, not just the
/// bytes: `data:text/javascript;base64,…` is a *program*, and handing it to the
/// HTML parser (or executing it) would both be wrong — see
/// [`DataUrl::renders_as_source`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DataUrl {
    /// Lowercased media type without parameters (`text/javascript`).
    /// Missing/empty type means `text/plain`, and an unknown type is never
    /// guessed from the payload (no sniffing).
    pub mime: String,
    /// `charset` parameter, lowercased (`utf-8`, `iso-8859-1`, …).
    pub charset: Option<String>,
    /// Whether the payload used the `;base64` transfer encoding.
    pub base64: bool,
    /// Decoded payload bytes.
    pub bytes: Vec<u8>,
}

impl DataUrl {
    /// Media types we hand to the HTML parser (`image/svg+xml` is included
    /// because Blitz rasterises/inline-renders it like a markup document).
    pub fn is_html(&self) -> bool {
        matches!(
            self.mime.as_str(),
            "text/html"
                | "application/xhtml+xml"
                | "image/svg+xml"
                | "text/xml"
                | "application/xml"
        )
    }

    /// JavaScript media types (same set the `<script type>` matcher accepts,
    /// see `js::is_classic_script_type`), plus the `module` suffix type.
    pub fn is_javascript(&self) -> bool {
        matches!(
            self.mime.as_str(),
            "text/javascript"
                | "application/javascript"
                | "application/x-javascript"
                | "text/ecmascript"
                | "application/ecmascript"
                | "text-javascript"
        )
    }

    /// True for documents we display as **inert source text** instead of
    /// parsing: scripts, stylesheets, JSON, and any `text/*` that is not
    /// markup. Binary types (`image/png`, `application/octet-stream`, …) keep
    /// the previous behaviour — the subresource path is where they belong.
    pub fn renders_as_source(&self) -> bool {
        if self.is_html() {
            return false;
        }
        self.is_javascript()
            || self.mime.starts_with("text/")
            || (self.mime.starts_with("application/")
                && matches!(
                    self.mime.as_str(),
                    "application/json"
                        | "application/manifest+json"
                        | "application/ld+json"
                        | "application/x-web-app-manifest+json"
                        | "application/css+json"
                ))
    }

    /// Payload as text. We honour `charset` only as far as recognising UTF-8:
    /// every other label decodes lossily, like our other document reads.
    pub fn text(&self) -> String {
        match self.charset.as_deref() {
            // fast path — no copy for the overwhelmingly common case
            Some("utf-8") | None => match std::str::from_utf8(&self.bytes) {
                Ok(s) => s.to_owned(),
                Err(_) => String::from_utf8_lossy(&self.bytes).into_owned(),
            },
            _ => String::from_utf8_lossy(&self.bytes).into_owned(),
        }
    }
}

/// Parse a `data:` URL into its media type and decoded bytes; `None` for other
/// schemes, for payloads without the `,` separator, and for invalid base64.
pub fn parse_data_url(url: &Url) -> Option<DataUrl> {
    if url.scheme() != "data" {
        return None;
    }
    let rest = url.as_str().strip_prefix("data:")?;
    let (meta, payload) = rest.split_once(',')?;
    let mut parts = meta.split(';');
    let mime_raw = parts.next().unwrap_or_default().trim();
    let params: Vec<&str> = parts.map(str::trim).collect();
    let base64 = params.iter().any(|p| p.eq_ignore_ascii_case("base64"));
    let charset = params.iter().find_map(|p| {
        p.to_ascii_lowercase()
            .strip_prefix("charset=")
            .map(|c| c.trim_matches('"').trim().to_owned())
    });
    let bytes = if base64 {
        decode_base64(payload)?
    } else {
        percent_decode(payload.as_bytes())
    };
    let mime = if mime_raw.is_empty() {
        "text/plain".to_owned()
    } else {
        mime_raw.to_ascii_lowercase()
    };
    Some(DataUrl {
        mime,
        charset,
        base64,
        bytes,
    })
}

/// Decode a `data:` URL (RFC 2397): `data:[<mediatype>][;base64],<payload>`.
/// Use [`parse_data_url`] when the media type matters (it always does for
/// top-level documents).
pub fn decode_data_url(url: &Url) -> Option<Vec<u8>> {
    parse_data_url(url).map(|d| d.bytes)
}

/// Minimal percent decoding (byte oriented; UTF-8 is reassembled downstream).
pub(crate) fn percent_decode(input: &[u8]) -> Vec<u8> {
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

/// Standard-alphabet base64 encoder (RFC 4648, `=` padded). Only tests build
/// `data:` URLs, so it is test-only code kept beside [`decode_base64`] — the
/// pair cannot drift.
#[cfg(test)]
pub fn encode_base64(input: &[u8]) -> String {
    const ALPHA: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ\
                                    abcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        let chars = [
            ALPHA[(n >> 18) as usize & 63],
            ALPHA[(n >> 12) as usize & 63],
            ALPHA[(n >> 6) as usize & 63],
            ALPHA[n as usize & 63],
        ];
        let used = chunk.len() + 1; // 1..=3 real bytes ⇒ 2..=4 characters
        for &c in &chars[..used] {
            out.push(c as char);
        }
        for _ in used..4 {
            out.push('=');
        }
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
    /// Same transport, redirects left to the caller (the managed cookie path).
    redirect_manual: reqwest::blocking::Client,
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
        // Managed fetches follow redirects themselves: reqwest's automatic
        // policy would replay the first hop's `Cookie` header verbatim and
        // only expose the final hop's `Set-Cookie`s, silently breaking the
        // per-origin cookie rules. Manual policy hands us every hop.
        let redirect_manual = reqwest::blocking::Client::builder()
            .use_rustls_tls()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(timeout)
            .build()
            .expect("reqwest manual-redirect client");
        FetchWorker {
            client,
            redirect_manual,
        }
    }

    pub fn client(&self) -> &reqwest::blocking::Client {
        &self.client
    }

    /// The client managed cookie fetches must use (3xx responses come back
    /// to us instead of being followed blindly).
    pub fn managed_client(&self) -> &reqwest::blocking::Client {
        &self.redirect_manual
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
        .map(|(k, v)| {
            (
                k.as_str().to_owned(),
                v.to_str().unwrap_or_default().to_owned(),
            )
        })
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

/// A main-document `POST` (an HTML form submission): the encoded body and
/// the enctype we are sending it as. `None` everywhere below means GET.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MainDocPost {
    pub content_type: Option<String>,
    pub body: Vec<u8>,
}

/// Build the request line: GET, or POST with the form body (and its
/// `Content-Type`) riding along.
fn build_request(
    client: &reqwest::blocking::Client,
    url: &Url,
    post: Option<&MainDocPost>,
) -> reqwest::blocking::RequestBuilder {
    match post {
        Some(body) => {
            let req = client.post(url.as_str()).body(body.body.clone());
            match &body.content_type {
                Some(content_type) => req.header(reqwest::header::CONTENT_TYPE, content_type.as_str()),
                None => req,
            }
        }
        None => client.get(url.as_str()),
    }
}

/// Plain (`Passthrough`) HTTP fetch (GET, or a form `POST` with `post`):
/// our User-Agent plus whatever the Blitz `Request` carried; no cookies, no
/// cache — and a POST never consults the cache either.
#[allow(dead_code)] // quiet convenience wrapper (the live paths report; tests use it)
pub fn plain_http_fetch(
    client: &reqwest::blocking::Client,
    url: &Url,
    extra: Option<&HeaderMap>,
    post: Option<&MainDocPost>,
) -> Result<FetchOutcome, String> {
    plain_http_fetch_report(client, url, extra, post, USER_AGENT, None)
}

/// [`plain_http_fetch`] with the DevTools reporter attached: one event for
/// the finished request (reqwest's automatic redirects mean only the final
/// response is visible here) or one failure event naming the failure.
pub fn plain_http_fetch_report(
    client: &reqwest::blocking::Client,
    url: &Url,
    extra: Option<&HeaderMap>,
    post: Option<&MainDocPost>,
    user_agent: &str,
    report: NetReport,
) -> Result<FetchOutcome, String> {
    let verb = if post.is_some() { "POST" } else { "GET" };
    let started = std::time::Instant::now();
    let mut req = build_request(client, url, post).header("User-Agent", user_agent);
    if let Some(headers) = extra {
        if !headers.is_empty() {
            req = req.headers(headers.clone());
        }
    }
    let response = req.send().map_err(|e| {
        warn!("passthrough {verb} {url} failed: {e}");
        let message = format!("fetch {url}: {e}");
        if let Some(report) = report {
            report(net_failed(verb, url, elapsed_ms(started), message.clone()));
        }
        message
    })?;
    let status = response.status().as_u16();
    let final_url = response.url().clone();
    let headers = collect_headers(response.headers());
    let body = response.bytes().map(|b| b.to_vec()).map_err(|e| {
        let message = format!("body {url}: {e}");
        if let Some(report) = report {
            report(net_failed(verb, &final_url, elapsed_ms(started), message.clone()));
        }
        message
    })?;
    debug!("{verb} {url} -> {status} ({} B, passthrough)", body.len());
    if let Some(report) = report {
        report(net_ok(verb, &final_url, status, body.len() as u64, elapsed_ms(started)));
    }
    Ok(FetchOutcome {
        final_url,
        status,
        headers,
        body,
        from_cache: false,
    })
}

/// Maximum redirects a single managed fetch follows before giving up.
const MAX_MANAGED_REDIRECTS: u32 = 10;

/// Managed HTTP fetch (GET, or a form `POST` with `post`): the full cookie
/// workhorse. Redirects are followed **hop by hop** (the caller must hand us
/// a `Policy::none()` client — [`FetchWorker::managed_client`]) so that
/// every hop gets its own `Cookie` header from the container jar and every
/// hop's `Set-Cookie`s are captured against **that hop's** URL — reqwest's
/// automatic redirect would replay hop 1's cookies to hop 2 verbatim and
/// only ever see the final hop's `Set-Cookie`s, silently breaking the
/// per-origin rules. Method conversion follows the browser convention: 303
/// always retries as GET; 301/302 turn a POST into a GET; 307/308 keep the
/// method and body.
///
/// `If-None-Match`/`If-Modified-Since` validators go on the first hop and
/// [`NetCache`] entries are stored under the original `url` — but only when
/// `cacheable` (a `POST` is never cached, look up or store). `extra` (from
/// the Blitz `Request`) never overrides our `Cookie` header.
#[allow(dead_code)] // quiet convenience wrapper (the live paths report; tests use it)
pub fn managed_http_fetch(
    client: &reqwest::blocking::Client,
    url: &Url,
    jar: &mut CookieJar,
    cache: &NetCache,
    now: i64,
    extra: Option<&HeaderMap>,
    post: Option<&MainDocPost>,
    cacheable: bool,
) -> Result<FetchOutcome, String> {
    managed_http_fetch_report(
        client, url, jar, cache, now, extra, post, cacheable, USER_AGENT, None,
    )
}

/// [`managed_http_fetch`] with the DevTools reporter attached. Exactly one
/// event per completed or failed request: every followed redirect hop gets
/// its own event (that hop's method, URL, 3xx status, declared body size),
/// the terminating response gets the final event, and a send/body failure
/// gets a single failure event (`status: None`). A response served entirely
/// from [`NetCache`] without touching the wire is not a request and emits
/// nothing (the caller sees `from_cache` and may report it itself).
pub fn managed_http_fetch_report(
    client: &reqwest::blocking::Client,
    url: &Url,
    jar: &mut CookieJar,
    cache: &NetCache,
    now: i64,
    extra: Option<&HeaderMap>,
    post: Option<&MainDocPost>,
    cacheable: bool,
    user_agent: &str,
    report: NetReport,
) -> Result<FetchOutcome, String> {
    let is_post = post.is_some();
    let cacheable = cacheable && !is_post;
    let started = std::time::Instant::now();
    let cached = if cacheable {
        let cached = cache.lookup(url.as_str());
        if cached.is_some() {
            debug!("cache entry for {url}: revalidating");
        }
        cached
    } else {
        None
    };

    let mut current = url.clone();
    let mut method = post; // POST -> GET conversions happen on redirect
    let mut first_hop = true;
    let mut hops_left = MAX_MANAGED_REDIRECTS;
    let (verb, status, final_url, headers, body) = loop {
        let mut req = build_request(client, &current, method).header("User-Agent", user_agent);
        if let Some(extra_headers) = extra {
            for (name, value) in extra_headers {
                if name.as_str().eq_ignore_ascii_case("cookie") {
                    continue; // the jar owns the Cookie header
                }
                req = req.header(name.clone(), value.clone());
            }
        }
        // The jar answers for this hop's origin, at this hop's URL — a
        // redirect into another host must never inherit cookies blindly.
        if let Some(cookie) = jar.cookie_header(&current, now) {
            req = req.header(reqwest::header::COOKIE, cookie);
        }
        if first_hop {
            if let Some(entry) = cached.as_ref() {
                let (etag, last_modified) = validators(&entry.headers);
                if let Some(etag) = etag {
                    req = req.header(reqwest::header::IF_NONE_MATCH, etag);
                } else if let Some(lm) = last_modified {
                    req = req.header(reqwest::header::IF_MODIFIED_SINCE, lm);
                }
            }
        }
        let verb = if method.is_some() { "POST" } else { "GET" };
        let hop_start = std::time::Instant::now();
        let response = req.send().map_err(|e| {
            warn!("{verb} {current} failed: {e}");
            let message = format!("fetch {current}: {e}");
            if let Some(report) = report {
                report(net_failed(
                    verb,
                    &current,
                    elapsed_ms(hop_start),
                    message.clone(),
                ));
            }
            message
        })?;
        let status = response.status().as_u16();

        // Every hop's Set-Cookies bind to the hop that sent them.
        let set_cookies: Vec<String> = response
            .headers()
            .get_all(reqwest::header::SET_COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .map(str::to_owned)
            .collect();
        capture_set_cookies(jar, &current, set_cookies.iter().map(String::as_str), now);
        if !set_cookies.is_empty() {
            debug!(
                "captured {} Set-Cookie header(s) for {}",
                set_cookies.len(),
                current
            );
        }

        if matches!(status, 301 | 302 | 303 | 307 | 308) {
            let location = response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .map(str::trim)
                .filter(|loc| !loc.is_empty())
                .and_then(|loc| current.join(loc).ok());
            match location {
                Some(target) if hops_left > 0 => {
                    debug!("{verb} {current} -> {status} redirect to {target} ({hops_left} hops left)");
                    // DevTools sees the hop as its own completed request; the
                    // hop body is never read, so the declared size is the
                    // honest answer (0 when the server declared none).
                    if let Some(report) = report {
                        let declared = response
                            .headers()
                            .get(reqwest::header::CONTENT_LENGTH)
                            .and_then(|v| v.to_str().ok())
                            .and_then(|v| v.trim().parse::<u64>().ok())
                            .unwrap_or(0);
                        report(net_ok(
                            verb,
                            &current,
                            status,
                            declared,
                            elapsed_ms(hop_start),
                        ));
                    }
                    // 303 always retries with GET; 301/302 convert a POST.
                    // 307/308 keep the method and body.
                    if status == 303 || (matches!(status, 301 | 302) && method.is_some()) {
                        method = None;
                    }
                    current = target;
                    first_hop = false;
                    hops_left -= 1;
                    continue; // response dropped here: hop body unread
                }
                Some(target) => {
                    warn!("redirect loop budget exhausted at {current} (to {target}); serving the {status}");
                }
                None => {} // no usable Location: the 3xx is the answer
            }
        }

        let headers = collect_headers(response.headers());
        let body = response.bytes().map(|b| b.to_vec()).map_err(|e| {
            let message = format!("body {current}: {e}");
            if let Some(report) = report {
                report(net_failed(
                    verb,
                    &current,
                    elapsed_ms(hop_start),
                    message.clone(),
                ));
            }
            message
        })?;
        break (verb, status, current.clone(), headers, body);
    };

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
            debug!(
                "GET {url} -> {status} served from cache ({} B, cached at {})",
                entry.body.len(),
                entry.cached_at
            );
            // The 304 exchange reported its own event; the document the
            // caller actually gets is the cached one, so the DevTools entry
            // carries the cache entry's status/size, not the 304.
            if let Some(report) = report {
                report(net_ok(
                    verb,
                    &final_url,
                    entry.status,
                    entry.body.len() as u64,
                    elapsed_ms(started),
                ));
            }
            Ok(FetchOutcome {
                final_url,
                status: entry.status,
                headers: entry.headers,
                body: entry.body,
                from_cache: true,
            })
        }
        Revalidate::UseFresh { .. } => {
            if cacheable && status < 400 {
                debug!("GET {url} -> {status} ({} B, stored in cache)", body.len());
                cache.store(url.as_str(), status, &headers, body.clone(), now);
            } else if is_post {
                debug!("POST {url} -> {status} ({} B, never cached)", body.len());
            } else {
                debug!("managed {url} -> {status} ({} B, not cacheable)", body.len());
            }
            if let Some(report) = report {
                report(net_ok(
                    verb,
                    &final_url,
                    status,
                    body.len() as u64,
                    elapsed_ms(started),
                ));
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
    /// The tab's effective User-Agent cell (shared with the runtime and the
    /// JS bindings): read per request, so a `ToTab::SetUserAgent` applies to
    /// every subresource fetched after it.
    user_agent: SharedUserAgent,
    /// Completed subresource requests (one [`NetEvent`] each), queued by the
    /// fetch threads and drained by the tab loop into
    /// `FromTab::NetworkEvent` messages. The NetWaker already wakes the loop
    /// when a fetch settles, so nothing extra is needed to make the drain
    /// timely.
    net_events: Arc<Mutex<Vec<NetEvent>>>,
    /// DevTools **console** entries raised by the fetch threads — today the
    /// wrong-`Content-Type` seam for `.css` responses (see
    /// [`crate::logging::css_content_type_warning`]); drained by the tab loop
    /// exactly like `net_events` (same wake, same tick).
    console_events: crate::logging::ConsoleQueue,
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
            user_agent: new_user_agent(None),
            net_events: Arc::new(Mutex::new(Vec::new())),
            console_events: crate::logging::console_queue(),
        }
    }

    /// The queue of finished subresource requests: the tab loop drains this
    /// every poll tick and emits one `FromTab::NetworkEvent` per entry.
    pub fn net_events(&self) -> Arc<Mutex<Vec<NetEvent>>> {
        Arc::clone(&self.net_events)
    }

    /// The DevTools console queue (fetch-thread side): drained every poll
    /// tick into `FromTab::ConsoleLog` entries (source `"css"` today).
    pub fn console_events(&self) -> crate::logging::ConsoleQueue {
        Arc::clone(&self.console_events)
    }

    /// A locally served subresource answered as an instant 200.
    fn report_local(&self, method: &str, url: &Url, status: u16, response_bytes: u64) {
        self.net_events
            .lock()
            .expect("net events mutex")
            .push(net_ok(method, url, status, response_bytes, 0));
    }

    /// A locally served subresource that failed: `status: None`, reason in
    /// `error`.
    fn report_local_failed(&self, method: &str, url: &Url, error: String) {
        self.net_events
            .lock()
            .expect("net events mutex")
            .push(net_failed(method, url, 0, error));
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

    /// Builder: share the tab's effective User-Agent cell (the `--user-agent`
    /// flag / `ToTab::SetUserAgent` value) with subresource fetches.
    #[must_use]
    pub fn with_user_agent(mut self, user_agent: SharedUserAgent) -> Self {
        self.user_agent = user_agent;
        self
    }

    pub fn policy(&self) -> NetPolicy {
        self.policy
    }

    /// Flush the container cookie jar to disk (tab `Destroy` path).
    pub fn save_cookies(&self) -> Result<(), StoreError> {
        self.jar.lock().expect("jar mutex poisoned").save()
    }

    /// The tab's handle on the **same** jar the provider uses for
    /// subresources — one jar per container per process, so a `Set-Cookie`
    /// from a main-document navigation is immediately visible to
    /// subresources (and vice versa), and persistence cannot overwrite one
    /// handle's cookies with the other's.
    pub fn jar(&self) -> Arc<Mutex<CookieJar>> {
        Arc::clone(&self.jar)
    }

    #[allow(dead_code)] // waker fires via the NetWaker installed by with_waker
    fn wake(&self, doc_id: usize) {
        let guard = self.waker.lock().expect("waker mutex poisoned");
        if let Some(waker) = guard.as_ref() {
            waker.wake(doc_id);
        }
    }

    fn spawn_http(&self, doc_id: usize, url: Url, request: Request, handler: Box<dyn NetHandler>) {
        let worker = Arc::clone(&self.worker);
        let managed = self.policy == NetPolicy::Managed;
        let jar = Arc::clone(&self.jar);
        let cache = Arc::clone(&self.cache);
        let waker = self.waker.lock().expect("waker mutex poisoned").clone();
        let events = Arc::clone(&self.net_events);
        let console = Arc::clone(&self.console_events);
        let user_agent = Arc::clone(&self.user_agent);
        debug!(
            "subresource GET {url} (doc {doc_id}, {})",
            if managed { "managed" } else { "passthrough" }
        );
        self.worker.spawn(move || {
            let signal = request.signal.clone();
            // Every settled request lands in the tab loop's DevTools queue
            // (redirect hops report individually — same rule as documents).
            let report = |event: NetEvent| events.lock().expect("net events mutex").push(event);
            let outcome = if managed {
                let mut jar = jar.lock().expect("jar mutex poisoned");
                let cache = cache.lock().expect("cache mutex poisoned");
                managed_http_fetch_report(
                    worker.managed_client(),
                    &url,
                    &mut jar,
                    &cache,
                    now_unix(),
                    None,
                    None,
                    true,
                    &read_user_agent(&user_agent),
                    Some(&report),
                )
            } else {
                plain_http_fetch_report(
                    worker.client(),
                    &url,
                    Some(&request.headers),
                    None,
                    &read_user_agent(&user_agent),
                    Some(&report),
                )
            };
            // Same as upstream blitz-net: wake the doc thread even on error,
            // and drop the handler when the fetch failed (no error path).
            if let Some(waker) = waker {
                waker.wake(doc_id);
            }
            let aborted = signal.as_ref().is_some_and(|s| s.aborted());
            match outcome {
                Ok(o) if !aborted && o.status < 400 => {
                    debug!(
                        "subresource {} -> {} delivered ({} B)",
                        url,
                        o.status,
                        o.body.len()
                    );
                    // DevTools console seam: a `.css` response that is not
                    // `text/css` is the one CSS problem this side of blitz
                    // can see (stylesheet parse errors are invisible —
                    // src/logging.rs documents the gap).
                    if let Some(entry) =
                        crate::logging::css_content_type_warning(&o.final_url, o.status, &o.headers)
                    {
                        console.lock().expect("console events mutex").push(entry);
                    }
                    deliver(Some(handler), o.final_url.to_string(), o.body)
                }
                Ok(o) if aborted => {
                    trace!("subresource {url} aborted (status {})", o.status);
                    deliver(None, String::new(), Vec::new())
                }
                Ok(o) => {
                    warn!("subresource {url} -> HTTP {} (not delivered)", o.status);
                    deliver(None, String::new(), Vec::new())
                }
                Err(message) => {
                    warn!("subresource {url} failed: {message}");
                    deliver(None, String::new(), Vec::new())
                }
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
                Some(bytes) => {
                    debug!(
                        "subresource data: URL decoded ({} B) for doc {doc_id}",
                        bytes.len()
                    );
                    deliver(Some(handler), url.to_string(), bytes)
                }
                None => {
                    warn!("subresource {} could not be decoded", url_for_log(&url));
                    deliver(None, String::new(), Vec::new())
                }
            },
            // Local schemes report instant 200s (or the read failure) so the
            // DevTools panel shows the subresource; `data:` URLs stay silent
            // — an inline asset is no request, and pages carry many.
            "file" => match read_file_url(&url) {
                Ok(bytes) => {
                    debug!("subresource {url} read from disk ({} B)", bytes.len());
                    self.report_local("GET", &url, 200, bytes.len() as u64);
                    deliver(Some(handler), url.to_string(), bytes)
                }
                Err(err) => {
                    warn!("subresource {url} unreadable: {err}");
                    self.report_local_failed("GET", &url, err);
                    deliver(None, String::new(), Vec::new())
                }
            },
            "about" => match builtin_page_bytes(&url) {
                Some(bytes) => {
                    debug!(
                        "subresource {url} served from built-ins ({} B)",
                        bytes.len()
                    );
                    self.report_local("GET", &url, 200, bytes.len() as u64);
                    deliver(Some(handler), url.to_string(), bytes)
                }
                None => {
                    debug!("subresource {url}: no built-in page (dropped)");
                    self.report_local_failed(
                        "GET",
                        &url,
                        "no built-in page for this about: URL".to_owned(),
                    );
                    deliver(None, String::new(), Vec::new())
                }
            },
            INTERNAL_SCHEME => match internal_page_bytes(&url) {
                Some(bytes) => {
                    debug!(
                        "subresource {url} served from internal pages ({} B)",
                        bytes.len()
                    );
                    self.report_local("GET", &url, 200, bytes.len() as u64);
                    deliver(Some(handler), url.to_string(), bytes)
                }
                None => {
                    debug!("subresource {url}: no internal page (dropped)");
                    self.report_local_failed(
                        "GET",
                        &url,
                        "no internal page for this ironsurf: URL".to_owned(),
                    );
                    deliver(None, String::new(), Vec::new())
                }
            },
            "http" | "https" => self.spawn_http(doc_id, url, request, handler),
            // javascript:/blob:/…: unsupported (dropped handler).
            other => {
                debug!(
                    "subresource scheme {other} unsupported ({}), dropped",
                    url_for_log(&url)
                );
                deliver(None, String::new(), Vec::new())
            }
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
    /// Scheme the fetcher never fetches (`javascript:` — the tab runs these as
    /// programs; everything else unsupported).
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
/// inline. Both policies send and capture container cookies on the
/// navigation (the jar is consulted per hop through [`managed_http_fetch`],
/// including redirects and redirect chains across origins); `Managed` adds
/// [`NetCache`] service/storage and https-upgrades `http:` navigations —
/// blocking with [`NetError::Blocked`] (carrying the error page) when the
/// upgrade fails.
#[allow(dead_code)] // quiet convenience wrapper (default UA; the live tab reports with its own cell)
pub fn fetch_document(
    url: &Url,
    policy: NetPolicy,
    jar: &mut CookieJar,
    cache: &NetCache,
    now: i64,
) -> Result<DocFetch, NetError> {
    fetch_document_post(url, None, policy, jar, cache, now)
}

/// The default effective-UA cell the convenience wrappers below carry (the
/// built-in [`USER_AGENT`]); the live tab always passes its own cell.
fn default_user_agent() -> SharedUserAgent {
    new_user_agent(None)
}

/// [`fetch_document`] with the DevTools reporter attached (see
/// [`fetch_document_post_report`]).
pub fn fetch_document_report(
    url: &Url,
    policy: NetPolicy,
    jar: &mut CookieJar,
    cache: &NetCache,
    now: i64,
    user_agent: &SharedUserAgent,
    report: NetReport,
) -> Result<DocFetch, NetError> {
    fetch_document_post_report(url, None, policy, jar, cache, now, user_agent, report)
}

/// [`fetch_document`] with an HTML form submission's POST body attached
/// (`post = Some(..)`): only http(s) accepts a POST; every local scheme
/// (`file:`/`about:`/`data:`/`ironsurf:`) is answered as GET and refuses a
/// POST body outright, so the caller (the tab's form layer) routes those
/// before ever calling here.
#[allow(dead_code)] // quiet convenience wrapper (default UA; the live tab posts through the report form)
pub fn fetch_document_post(
    url: &Url,
    post: Option<&MainDocPost>,
    policy: NetPolicy,
    jar: &mut CookieJar,
    cache: &NetCache,
    now: i64,
) -> Result<DocFetch, NetError> {
    fetch_document_post_report(
        url,
        post,
        policy,
        jar,
        cache,
        now,
        &default_user_agent(),
        None,
    )
}

/// [`fetch_document_post`] with the DevTools reporter attached: one
/// [`NetEvent`] per completed or failed main-document request, including one
/// per followed redirect hop (the redirect chain is followed inside
/// [`managed_http_fetch_report`]). Local-scheme loads (`file:`/`about:`/
/// `ironsurf:`/`data:`) are reported as instant 200s; an https-upgrade that
/// gets blocked is reported against the **requested** URL with the policy
/// reason, and a transport failure against the URL that was tried.
pub fn fetch_document_post_report(
    url: &Url,
    post: Option<&MainDocPost>,
    policy: NetPolicy,
    jar: &mut CookieJar,
    cache: &NetCache,
    now: i64,
    user_agent: &SharedUserAgent,
    report: NetReport,
) -> Result<DocFetch, NetError> {
    let worker = shared_worker();
    fetch_document_with_worker_report(url, post, policy, jar, cache, now, &worker, user_agent, report)
}

#[allow(dead_code)] // quiet convenience wrapper (tests drive it; the runtime reports)
fn fetch_document_with_worker(
    url: &Url,
    post: Option<&MainDocPost>,
    policy: NetPolicy,
    jar: &mut CookieJar,
    cache: &NetCache,
    now: i64,
    worker: &FetchWorker,
) -> Result<DocFetch, NetError> {
    fetch_document_with_worker_report(
        url,
        post,
        policy,
        jar,
        cache,
        now,
        worker,
        &default_user_agent(),
        None,
    )
}

#[allow(clippy::too_many_arguments)]
fn fetch_document_with_worker_report(
    url: &Url,
    post: Option<&MainDocPost>,
    policy: NetPolicy,
    jar: &mut CookieJar,
    cache: &NetCache,
    now: i64,
    worker: &FetchWorker,
    user_agent: &SharedUserAgent,
    report: NetReport,
) -> Result<DocFetch, NetError> {
    // Snapshot the effective UA once per document load: a mid-load
    // `SetUserAgent` applies to the NEXT request, never mid-flight.
    let user_agent = read_user_agent(user_agent);
    if post.is_some() && !is_http_scheme(url) {
        // Defensive: a body to a local scheme has nowhere to go.
        warn!("refusing to POST to non-http scheme: {url}");
        return Err(NetError::Unsupported { url: url.clone() });
    }
    // Local schemes: identical under both policies. Reported as instant
    // 200s — the DevTools network panel lists the document either way.
    let local_ok = |url: &Url, body: Vec<u8>| {
        if let Some(report) = report {
            report(net_ok("GET", url, 200, body.len() as u64, 0));
        }
        DocFetch {
            url: url.clone(),
            status: 200,
            body,
        }
    };
    match url.scheme() {
        "file" => {
            let body = read_file_url(url).map_err(|e| {
                warn!("cannot read {url}: {e}");
                if let Some(report) = report {
                    report(net_failed("GET", url, 0, e.clone()));
                }
                NetError::Io {
                    url: url.clone(),
                    message: e,
                }
            })?;
            debug!("document {url} read from disk ({} B)", body.len());
            return Ok(local_ok(url, body));
        }
        "about" => {
            if let Some(body) = builtin_page_bytes(url) {
                debug!("document {url} served from built-ins ({} B)", body.len());
                return Ok(local_ok(url, body));
            }
        }
        INTERNAL_SCHEME => {
            // Always local: no network, no cookies, no https-upgrade — whatever
            // the page (welcome or not-found), it is answered from built-ins.
            if let Some(body) = internal_page_bytes(url) {
                debug!("document {url} served from internal pages ({} B)", body.len());
                return Ok(local_ok(url, body));
            }
        }
        "data" => {
            if let Some(body) = decode_data_url(url) {
                debug!("document {} decoded ({} B)", url_for_log(url), body.len());
                return Ok(local_ok(url, body));
            }
        }
        _ => {}
    }

    if !is_http_scheme(url) {
        warn!("refusing to navigate to unsupported scheme: {url}");
        return Err(NetError::Unsupported { url: url.clone() });
    }

    // The jar always applies to top-level loads; only the cache follows the
    // managed policy.
    let cacheable = policy == NetPolicy::Managed;
    match plan_document(policy, url) {
        // Non-http scheme that still reached here (unknown scheme): refuse.
        Plan::Pass => {
            plain_http_fetch_report(worker.client(), url, None, post, &user_agent, report)
                .map(|o| DocFetch {
                    url: o.final_url,
                    status: o.status,
                    body: o.body,
                })
                .map_err(|message| NetError::Transport {
                    url: url.clone(),
                    message,
                })
        }
        Plan::Fetch(target) => managed_http_fetch_report(
            worker.managed_client(),
            &target,
            jar,
            cache,
            now,
            None,
            post,
            cacheable,
            &user_agent,
            report,
        )
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
            debug!("https-upgrade: {url} -> {https}");
            // The upgraded navigation is reported as exactly ONE event named
            // by the URL the user asked for: success carries the response the
            // https attempt produced, failure names the policy block (the raw
            // connect error would only name the upgraded URL, which the user
            // never asked for). `Managed` `https:` loads and Passthrough
            // navigations keep the full per-hop reporting above.
            let started = std::time::Instant::now();
            let method = if post.is_some() { "POST" } else { "GET" };
            match managed_http_fetch_report(
                worker.managed_client(),
                &https,
                jar,
                cache,
                now,
                None,
                post,
                true,
                &user_agent,
                None,
            ) {
                Ok(o) => {
                    if let Some(report) = report {
                        report(net_ok(
                            method,
                            &o.final_url,
                            o.status,
                            o.body.len() as u64,
                            elapsed_ms(started),
                        ));
                    }
                    Ok(DocFetch {
                        url: o.final_url,
                        status: o.status,
                        body: o.body,
                    })
                }
                Err(reason) => {
                    warn!("https upgrade for {url} failed, navigation blocked: {reason}");
                    if let Some(report) = report {
                        report(net_failed(
                            method,
                            url,
                            elapsed_ms(started),
                            format!("https-upgrade to {https} blocked: {reason}"),
                        ));
                    }
                    Err(NetError::Blocked {
                        request: url.clone(),
                        https,
                        reason,
                    })
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Policy unit tests (no live network — every rule is a pure function)

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

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
    fn document_plan_fetches_under_both_policies_never_without_the_jar() {
        // The whole point: a navigation is never "Pass"ed to a cookie-less
        // plain fetch. Passthrough fetches the URL as asked (jar attached);
        // Managed upgrades http: first.
        assert_eq!(
            plan_document(NetPolicy::Passthrough, &url("https://example.com")),
            Plan::Fetch(url("https://example.com"))
        );
        assert_eq!(
            plan_document(NetPolicy::Passthrough, &url("http://example.com/x")),
            Plan::Fetch(url("http://example.com/x"))
        );
        assert_eq!(
            plan_document(NetPolicy::Managed, &url("https://example.com/page")),
            Plan::Fetch(url("https://example.com/page"))
        );
        assert_eq!(
            plan_document(NetPolicy::Managed, &url("http://example.com/page")),
            Plan::Upgrade(url("https://example.com/page"))
        );
    }

    #[test]
    fn document_plan_passes_non_http_schemes() {
        for scheme_url in [
            "file:///tmp/x/index.html",
            "about:blank",
            "data:text/html,x",
            "ironsurf://welcome",
        ] {
            assert_eq!(
                plan_document(NetPolicy::Managed, &url(scheme_url)),
                Plan::Pass,
                "{scheme_url} must pass through"
            );
            assert_eq!(
                plan_document(NetPolicy::Passthrough, &url(scheme_url)),
                Plan::Pass,
                "{scheme_url} must pass through"
            );
        }
    }

    #[test]
    fn https_upgrade_folding() {
        assert_eq!(
            upgrade_to_https(&url("http://example.com/"))
                .unwrap()
                .as_str(),
            "https://example.com/"
        );
        // The insecure default port is folded away…
        assert_eq!(
            upgrade_to_https(&url("http://example.com:80/x"))
                .unwrap()
                .as_str(),
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
            revalidate(
                &cached(None, Some("Tue, 1 Jul 2025 00:00:00 GMT")),
                304,
                None,
                Some("Tue, 1 Jul 2025 00:00:00 GMT")
            ),
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
        let store = ironsurf_storage::ContainerStore::from_profile_dir(
            ironsurf_common::ContainerId(9),
            dir.path(),
        );
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
            jar.cookie_header(&url("https://example.com/"), now)
                .as_deref(),
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
        assert!(
            !header.contains("h=1"),
            "host-only cookie leaked to subdomain"
        );

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
        let page = blocked_error_page(
            &url("http://example.com/"),
            &url("https://example.com/"),
            "boom",
        );
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
        let store = ironsurf_storage::ContainerStore::from_profile_dir(
            ironsurf_common::ContainerId(7),
            dir.path(),
        );
        let provider =
            BlitzNetProvider::shared(NetPolicy::Managed, store.cookie_jar(), store.cache());
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
        let store = ironsurf_storage::ContainerStore::from_profile_dir(
            ironsurf_common::ContainerId(7),
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
        provider.fetch(1, Request::get(url("data:text/html,hi%21")), handler);
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

        let about = url("about:ironsurf");
        let text =
            String::from_utf8(builtin_page_bytes(&about).expect("ironsurf page")).expect("utf8");
        assert!(text.contains("IronSurf"));

        assert!(builtin_page_bytes(&url("about:whatever")).is_none());
    }

    #[test]
    fn ironsurf_scheme_names_pages_locally() {
        // The default start page is the internal welcome page.
        assert_eq!(ironsurf_common::default_url(), "ironsurf://welcome");
        let welcome = url(&ironsurf_common::default_url());
        assert_eq!(welcome.scheme(), INTERNAL_SCHEME);
        assert_eq!(internal_page_name(&welcome), "welcome");
        // Authority vs path placement of non-special schemes all select welcome.
        assert_eq!(internal_page_name(&url("ironsurf:///welcome")), "welcome");
        assert_eq!(internal_page_name(&url("ironsurf://welcome/")), "welcome");
        assert_eq!(
            internal_page_name(&url("ironsurf://ironsurf/welcome")),
            "welcome"
        );
        assert_eq!(internal_page_name(&url("IRONSURF://Welcome")), "welcome");
        assert_eq!(internal_page_name(&url("ironsurf://about/config")), "about");
        // Not our scheme → not our business.
        assert_eq!(internal_page_name(&url("https://welcome")), "welcome");
        assert!(internal_page_bytes(&url("https://welcome")).is_none());
    }

    #[test]
    fn internal_welcome_page_is_self_contained_and_safe() {
        let bytes = internal_page_bytes(&url("ironsurf://welcome")).expect("welcome page");
        let text = String::from_utf8(bytes).expect("utf8");
        assert!(text.contains("Welcome to IronSurf"));
        // Self-contained: no subresources, no scripts — must render with no network.
        assert!(!text.contains("<script"));
        assert!(!text.contains("src="));
        assert!(!text.contains("http://"));
        // An unknown page name is an honest internal 404 page, not an error…
        let gone = internal_page_bytes(&url("ironsurf://nope")).expect("not-found page");
        let gone = String::from_utf8(gone).expect("utf8");
        assert!(gone.contains("No such internal page"));
        assert!(gone.contains("ironsurf://welcome"));
        // …and the requested URL is escaped, never injected as markup. The URL
        // parser itself rejects raw `<` in a host, and an encoded hostile page
        // name must still produce no live script tag; the escape helper itself
        // is asserted directly since it is the last line of defence.
        assert_eq!(
            escape_html_text("<s\"'>"),
            "&lt;s&quot;&#39;&gt;"
        );
        let xss = internal_page_bytes(&url("ironsurf://%3Cscript%3Ealert%281%29"))
            .expect("not-found for hostile page name");
        let xss = String::from_utf8(xss).expect("utf8");
        assert!(!xss.contains("<script"));
    }

    #[test]
    fn fetch_document_file_and_about_without_network() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = ironsurf_storage::ContainerStore::from_profile_dir(
            ironsurf_common::ContainerId(7),
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
                None,
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
            &url("about:ironsurf"),
            None,
            NetPolicy::Managed,
            &mut jar,
            &cache,
            now,
            worker_for_test(),
        )
        .expect("about page");
        assert!(doc.body_text().contains("IronSurf"));

        // ironsurf:// built-ins: answered locally under Managed too — never
        // upgraded, never on the wire, URL round-trips for UrlChanged.
        let doc = fetch_document_with_worker(
            &url("ironsurf://welcome"),
            None,
            NetPolicy::Managed,
            &mut jar,
            &cache,
            now,
            worker_for_test(),
        )
        .expect("internal welcome page");
        assert_eq!(doc.status, 200);
        assert_eq!(doc.url.as_str(), "ironsurf://welcome");
        assert!(doc.body_text().contains("Welcome to IronSurf"));

        // Unsupported schemes are refused, not fetched.
        let err = fetch_document_with_worker(
            &url("javascript:alert(1)"),
            None,
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
        let store = ironsurf_storage::ContainerStore::from_profile_dir(
            ironsurf_common::ContainerId(7),
            dir.path(),
        );
        let mut jar = store.cookie_jar();
        let cache = store.cache();
        // Port 1 on loopback: connect is refused instantly, no real network.
        let http = url("http://127.0.0.1:1/");
        let err = fetch_document_with_worker(
            &http,
            None,
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
            None,
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
        assert!(
            !https_header.contains(','),
            "pairs are '; '-joined, not comma"
        );
        // Over http the Secure cookie must drop out of the assembled header.
        let http_header = jar
            .cookie_header(&url("http://example.com/path"), now)
            .expect("http header");
        assert_eq!(http_header, "a=1");
    }

    #[test]
    fn netcache_etag_round_trip_decides_revalidation() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = ironsurf_storage::ContainerStore::from_profile_dir(
            ironsurf_common::ContainerId(7),
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
        assert_eq!(
            decode_base64("aGVsbG8=").as_deref(),
            Some(b"hello".as_slice())
        );
        assert_eq!(
            decode_base64("aGVsbG8").as_deref(),
            Some(b"hello".as_slice())
        );
        assert_eq!(decode_base64("").as_deref(), Some(b"".as_slice()));
        assert_eq!(
            decode_base64("////").as_deref(),
            Some([0xff, 0xff, 0xff].as_slice())
        );
        // URL-safe alphabet maps to the same values as the standard one.
        assert_eq!(
            decode_base64("____").as_deref(),
            Some([0xff, 0xff, 0xff].as_slice())
        );
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

    #[test]
    fn base64_encoder_round_trips_with_the_decoder() {
        assert_eq!(encode_base64(b""), "");
        assert_eq!(encode_base64(b"h"), "aA==");
        assert_eq!(encode_base64(b"he"), "aGU=");
        assert_eq!(encode_base64(b"hel"), "aGVs");
        assert_eq!(encode_base64(b"hello"), "aGVsbG8=");
        assert_eq!(encode_base64(&[0xff, 0xff, 0xff]), "////");
        // Every byte value survives the round trip.
        let all: Vec<u8> = (0..=255u8).collect();
        assert_eq!(
            decode_base64(&encode_base64(&all)).as_deref(),
            Some(all.as_slice())
        );
        // The canonical JS payload used by the data: tests.
        assert_eq!(
            encode_base64("console.log(\"hi\"); document.title = \"js\";".as_bytes()),
            JS_B64
        );
    }

    /// `data:text/javascript;base64,…` is the shape real pages use for inline
    /// program text: `console.log("hi"); document.title = "js";`.
    const JS_B64: &str = "Y29uc29sZS5sb2coImhpIik7IGRvY3VtZW50LnRpdGxlID0gImpzIjs=";

    #[test]
    fn data_url_javascript_base64_parses() {
        let data =
            parse_data_url(&url(&format!("data:text/javascript;base64,{JS_B64}"))).expect("parses");
        assert_eq!(data.mime, "text/javascript");
        assert_eq!(data.charset.as_deref(), None);
        assert!(data.base64);
        assert_eq!(data.text(), "console.log(\"hi\"); document.title = \"js\";");
        assert!(data.is_javascript());
        assert!(!data.is_html());
        // A program is displayed, never parsed/evaluated as a document.
        assert!(data.renders_as_source());
    }

    #[test]
    fn data_url_media_types_and_parameters() {
        // Uppercase scheme/parameters are normalised; `;base64` may follow a
        // charset parameter.
        let data = parse_data_url(&url(
            "data:Text/JavaScript;charset=UTF-8;BASE64,Y29uc29sZS5sb2coMSk=",
        ))
        .expect("parses");
        assert_eq!(data.mime, "text/javascript");
        assert_eq!(data.charset.as_deref(), Some("utf-8"));
        assert!(data.base64);
        assert_eq!(data.text(), "console.log(1)");

        // No media type at all ⇒ text/plain (percent-encoded), still source.
        let plain = parse_data_url(&url("data:,a%20b")).expect("parses");
        assert_eq!(plain.mime, "text/plain");
        assert!(!plain.base64);
        assert_eq!(plain.text(), "a b");
        assert!(plain.renders_as_source());

        // Markup keeps going through the parser.
        assert!(parse_data_url(&url("data:text/html,<b>x</b>")).is_some_and(|d| d.is_html()));
        assert!(!parse_data_url(&url("data:text/html,<b>x</b>"))
            .unwrap()
            .renders_as_source());
        assert!(parse_data_url(&url("data:application/xhtml+xml,x")).is_some_and(|d| d.is_html()));
        assert!(parse_data_url(&url("data:image/svg+xml,%3Csvg/%3E")).is_some_and(|d| d.is_html()));

        // Other script/stylesheet/JSON flavours are source too.
        for mime in [
            "application/javascript",
            "application/x-javascript",
            "text/ecmascript",
            "text/css",
            "application/json",
            "application/ld+json",
        ] {
            let d = parse_data_url(&url(&format!("data:{mime},x"))).expect("parses");
            assert!(
                d.renders_as_source(),
                "{mime} must render as source, not be parsed as HTML"
            );
        }

        // Binary types are not documents (unchanged behaviour, no sniffing).
        let png = parse_data_url(&url("data:image/png;base64,iVBORw0KGgo=")).expect("parses");
        assert!(!png.renders_as_source());
        assert_eq!(png.bytes, [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]);
    }

    #[test]
    fn data_url_parse_rejects_what_decoding_cannot_produce() {
        // Not a data: URL.
        assert_eq!(parse_data_url(&url("http://example.com/x")), None);
        assert_eq!(parse_data_url(&url("file:///tmp/x")), None);
        // No `,` separator.
        assert_eq!(parse_data_url(&url("data:text/javascript;base64")), None);
        // Invalid base64 characters in the payload (`(` is not in the alphabet).
        assert_eq!(
            parse_data_url(&url("data:text/javascript;base64,Y29uc29sZS(bG9nKDEp")),
            None
        );
        assert_eq!(
            parse_data_url(&url("data:text/javascript;base64,a!b")),
            None
        );
        // Empty payload after `,` is legal and decodes to nothing.
        assert_eq!(
            parse_data_url(&url("data:text/javascript;base64,")).map(|d| d.bytes),
            Some(Vec::new())
        );
    }

    #[test]
    fn data_url_round_trips_through_the_provider_and_the_fetcher() {
        let raw = format!("data:text/javascript;base64,{JS_B64}");
        let u = url(&raw);

        // Main-document fetch (what `Tab::navigate` uses).
        let mut jar = CookieJar::default();
        let dir = tempfile::tempdir().expect("tempdir");
        let store = ironsurf_storage::ContainerStore::from_profile_dir(
            ironsurf_common::ContainerId(9),
            dir.path(),
        );
        let cache = store.cache();
        let fetch = fetch_document(&u, NetPolicy::Managed, &mut jar, &cache, now_unix())
            .expect("data: URLs always fetch");
        assert_eq!(fetch.status, 200);
        assert_eq!(fetch.url, u);
        assert_eq!(
            String::from_utf8(fetch.body).expect("utf8"),
            "console.log(\"hi\"); document.title = \"js\";"
        );

        // Subresource fetch (`<script src="data:…">`) delivers the same bytes.
        let provider = BlitzNetProvider::new(NetPolicy::Managed, store.cookie_jar(), store.cache());
        let (handler, log) = mock_handler();
        provider.fetch(1, Request::get(u.clone()), handler);
        let got = wait_for(&log);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, u.as_str());
        assert_eq!(
            String::from_utf8(got[0].1.clone()).expect("utf8"),
            "console.log(\"hi\"); document.title = \"js\";"
        );
    }

    // Small helper: dedicated short-timeout client for connection-refused tests.
    #[allow(clippy::redundant_clone)]
    fn worker_for_test() -> &'static FetchWorker {
        static WORKER: OnceLock<Arc<FetchWorker>> = OnceLock::new();
        WORKER.get_or_init(|| FetchWorker::with_timeout(std::time::Duration::from_secs(5)))
    }

    // -- live localhost HTTP: the cookie goes out and comes back -------------
    //
    // A tiny HTTP/1.0 server on 127.0.0.1 answering fixed routes, used to
    // prove the *wire* behaviour the jar unit tests cannot: `Cookie` headers
    // actually riding on requests, `Set-Cookie` actually coming back, every
    // hop of a redirect chain handled against its own origin.

    enum Route {
        /// 302 to `to` with the listed Set-Cookie headers.
        Redirect { to: String, set_cookie: Vec<String> },
        /// 200 echoing the `Cookie` header it received (`-` when none).
        Echo,
    }

    /// Spawn the test server; returns its port. Serves one request per
    /// connection (HTTP/1.0: close after the response). Dual-stack bind so
    /// BOTH `127.0.0.1` and `localhost` (-> ::1) reach the same server on
    /// the same port — two distinct origin host names for the cross-origin
    /// redirect test.
    fn spawn_local_http_server(routes: Vec<(&str, Route)>) -> u16 {
        let listener = TcpListener::bind(("[::]", 0))
            .or_else(|_| TcpListener::bind(("127.0.0.1", 0)))
            .expect("bind loopback");
        let port = listener.local_addr().expect("addr").port();
        let routes: Vec<(String, Route)> = routes
            .into_iter()
            .map(|(p, r)| (p.to_string(), r))
            .collect();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                // Read the request head (non-verbose GETs only).
                let mut buf: Vec<u8> = Vec::new();
                let mut byte = [0u8; 1];
                while !buf.ends_with(b"\r\n\r\n") {
                    match stream.read(&mut byte) {
                        Ok(0) | Err(_) => break,
                        Ok(_) => buf.push(byte[0]),
                    }
                    if buf.len() > 64 * 1024 {
                        break;
                    }
                }
                let head = String::from_utf8_lossy(&buf).to_string();
                let path = head
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or("/")
                    .to_string();
                let cookie = head
                    .lines()
                    .find(|line| line.to_ascii_lowercase().starts_with("cookie:"))
                    .map(|line| line["cookie:".len()..].trim().to_owned())
                    .unwrap_or_default();
                let route = routes
                    .iter()
                    .find(|(p, _)| *p == path)
                    .map(|(_, r)| match r {
                        Route::Redirect { to, set_cookie } => Route::Redirect {
                            to: to.replace("{port}", &port.to_string()),
                            set_cookie: set_cookie.clone(),
                        },
                        Route::Echo => Route::Echo,
                    });
                let response = match route {
                    Some(Route::Redirect { to, set_cookie }) => {
                        let mut r = format!("HTTP/1.0 302 Found\r\nLocation: {to}\r\n");
                        for value in set_cookie {
                            r.push_str(&format!("Set-Cookie: {value}\r\n"));
                        }
                        r.push_str("Connection: close\r\n\r\n");
                        r
                    }
                    Some(Route::Echo) => {
                        let body = if cookie.is_empty() { "-".into() } else { cookie };
                        format!(
                            "HTTP/1.0 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        )
                    }
                    None => "HTTP/1.0 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string(),
                };
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
        });
        port
    }

    fn test_cache(name: &str) -> NetCache {
        let dir = std::env::temp_dir().join(format!("ironsurf-tab-cookie-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        NetCache::with_limits(dir, 16, 4096)
    }

    fn get(url: &str) -> Url {
        Url::parse(url).expect("test url")
    }

    /// One jar, one worker, both redirect legs: cookies set on earlier hops
    /// must be on the later hops' requests, and vice versa.
    fn fetch(path: &str, port: u16, jar: &mut CookieJar, cache: &NetCache) -> FetchOutcome {
        managed_http_fetch(
            worker_for_test().managed_client(),
            &get(&format!("http://127.0.0.1:{port}{path}")),
            jar,
            cache,
            now_unix(),
            None,
            None,
            false,
        )
        .expect("localhost fetch")
    }

    #[test]
    fn redirect_chain_cookies_are_captured_and_sent_per_hop() {
        let port = spawn_local_http_server(vec![
            (
                "/multi",
                Route::Redirect {
                    to: "/hop2".into(),
                    set_cookie: vec!["m1=1; Path=/".into()],
                },
            ),
            (
                "/hop2",
                Route::Redirect {
                    to: "/echo".into(),
                    set_cookie: vec!["m2=2; Path=/".into()],
                },
            ),
            ("/echo", Route::Echo),
        ]);
        let mut jar = CookieJar::new();
        let out = fetch("/multi", port, &mut jar, &test_cache("chain"));
        let echoed = String::from_utf8_lossy(&out.body).to_string();
        assert_eq!(out.final_url.as_str(), format!("http://127.0.0.1:{port}/echo"));
        // The final hop saw BOTH chain cookies — each captured on its own
        // hop and replayed by the jar on the following one.
        assert!(echoed.contains("m1=1"), "hop-1 cookie missing from {echoed:?}");
        assert!(echoed.contains("m2=2"), "hop-2 cookie missing from {echoed:?}");
    }

    #[test]
    fn path_and_domain_scope_are_enforced_on_the_wire() {
        let port = spawn_local_http_server(vec![
            (
                "/start",
                Route::Redirect {
                    to: "/echo".into(),
                    set_cookie: vec![
                        "hop=1; Path=/".into(),
                        // Scoped to /start: must NOT ride along to /echo.
                        "scoped=9; Path=/start".into(),
                        // Domain attributes against these hosts are invalid:
                        // foreign suffix and single-label both reject.
                        "badhost=7; Domain=example.com; Path=/".into(),
                        "single=8; Domain=localhost; Path=/".into(),
                        // Secure over plain http: rejected out right.
                        "sec=5; Path=/; Secure".into(),
                    ],
                },
            ),
            ("/echo", Route::Echo),
        ]);
        let mut jar = CookieJar::new();
        let out = fetch("/start", port, &mut jar, &test_cache("scope"));
        let echoed = String::from_utf8_lossy(&out.body).to_string();
        assert_eq!(echoed, "hop=1", "only the Path=/ cookie may reach /echo, got {echoed:?}");
        // The jar itself kept nothing but the valid host-only cookies.
        let domains: Vec<&str> = jar.cookies().iter().map(|c| c.domain.as_str()).collect();
        assert!(domains.iter().all(|d| *d == "127.0.0.1"), "{domains:?}");
        assert!(!jar.cookies().iter().any(|c| c.name == "sec"), "Secure cookie set over http");
    }

    #[test]
    fn host_only_cookie_never_follows_a_redirect_to_another_host() {
        // Reached via `localhost`, /cross sets a host-only cookie and
        // redirects to 127.0.0.1 — a DIFFERENT origin. The echo on that
        // host must not see the cookie, but the jar must still have it.
        let port = spawn_local_http_server(vec![
            (
                "/cross",
                Route::Redirect {
                    to: format!("http://127.0.0.1:{{port}}/echo"),
                    set_cookie: vec!["mk=keep; Path=/".into()],
                },
            ),
            ("/echo", Route::Echo),
        ]);
        let mut jar = CookieJar::new();
        let out = managed_http_fetch(
            worker_for_test().managed_client(),
            &get(&format!("http://localhost:{port}/cross")),
            &mut jar,
            &test_cache("cross"),
            now_unix(),
            None,
            None,
            false,
        )
        .expect("localhost redirect fetch");
        let echoed = String::from_utf8_lossy(&out.body).to_string();
        assert_eq!(echoed, "-", "host-only cookie leaked to the other host: {echoed:?}");
        assert!(jar.cookies().iter().any(|c| c.name == "mk" && c.domain == "localhost"));
    }

    #[test]
    fn main_document_navigations_carry_the_jar_even_under_passthrough() {
        // The headline rule: even the *default* (Passthrough) policy sends
        // cookies on top-level loads — a site that set a cookie on one
        // navigation must see it come back on the next. (Under Managed the
        // navigation rides the very same managed_http_fetch the tests above
        // exercise against this server; its only addition is the http→https
        // upgrade, which a plain-http loopback server cannot show.)
        let port = spawn_local_http_server(vec![
            (
                "/set",
                Route::Redirect {
                    to: "/echo".into(),
                    set_cookie: vec!["back=1; Path=/".into()],
                },
            ),
            ("/echo", Route::Echo),
        ]);
        let cache = test_cache("nav");
        let mut jar = CookieJar::new();
        let first = fetch_document(
            &get(&format!("http://127.0.0.1:{port}/set")),
            NetPolicy::Passthrough,
            &mut jar,
            &cache,
            now_unix(),
        )
        .expect("navigation 1");
        assert_eq!(first.url.as_str(), format!("http://127.0.0.1:{port}/echo"));
        let second = fetch_document(
            &get(&format!("http://127.0.0.1:{port}/echo")),
            NetPolicy::Passthrough,
            &mut jar,
            &cache,
            now_unix(),
        )
        .expect("navigation 2");
        let echoed = String::from_utf8_lossy(&second.body).to_string();
        assert_eq!(echoed, "back=1", "Passthrough navigation never returned the cookie");
    }
}

// ---------------------------------------------------------------------------
// Unit tests — effective User-Agent resolution (task: per-container UA)

#[cfg(test)]
mod user_agent_tests {
    use super::*;

    #[test]
    fn some_value_is_used_verbatim_once_trimmed() {
        assert_eq!(
            resolve_user_agent(Some("  IronRaven/12.0 (test)  ")).as_str(),
            "IronRaven/12.0 (test)"
        );
    }

    #[test]
    fn empty_or_whitespace_some_resets_to_default() {
        assert_eq!(resolve_user_agent(Some("")), USER_AGENT);
        assert_eq!(resolve_user_agent(Some("   \t \n")), USER_AGENT);
    }

    #[test]
    fn none_is_the_builtin_default() {
        assert_eq!(resolve_user_agent(None), USER_AGENT);
    }

    #[test]
    fn cell_starts_from_the_flag_and_follows_every_write() {
        let cell = new_user_agent(Some("  Flag/1.0  "));
        assert_eq!(read_user_agent(&cell), "Flag/1.0");
        // The flag is resolved exactly like a SetUserAgent: blank ⇒ default.
        assert_eq!(read_user_agent(&new_user_agent(Some("   "))), USER_AGENT);
        assert_eq!(read_user_agent(&new_user_agent(None)), USER_AGENT);
        // A write lands on every later read — the request builders read this
        // same cell per request, so the next fetch carries the new value.
        *cell.lock().expect("ua mutex") =
            resolve_user_agent(Some("Second/2.0"));
        assert_eq!(read_user_agent(&cell), "Second/2.0");
        *cell.lock().expect("ua mutex") = resolve_user_agent(None);
        assert_eq!(read_user_agent(&cell), USER_AGENT);
    }
}
