//! Bounded in-memory response cache with LRU eviction (~256 MB / 1000
//! entries by default). `store`/`lookup` mirror `&self` (like the rest of
//! this module's best-effort disk API) via interior mutability.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;

/// One cached HTTP response.
#[derive(Clone, Debug, PartialEq)]
pub struct CachedEntry {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub cached_at: i64,
}

pub(crate) const CACHE_DIR: &str = "net-cache";

/// Default total byte budget (~256 MB).
pub const DEFAULT_MAX_TOTAL_BYTES: usize = 256 * 1024 * 1024;
/// Default maximum number of cached entries.
pub const DEFAULT_MAX_ENTRIES: usize = 1000;

struct CacheItem {
    entry: CachedEntry,
    bytes: usize,
}

struct Inner {
    map: HashMap<String, CacheItem>,
    /// Front = most recently used.
    order: VecDeque<String>,
    total_bytes: usize,
}

/// Bounded LRU cache for one container's `net-cache/`.
pub struct NetCache {
    dir: PathBuf,
    max_entries: usize,
    max_total_bytes: usize,
    inner: RefCell<Inner>,
}

impl NetCache {
    pub(crate) fn new(dir: PathBuf) -> Self {
        Self::with_limits(dir, DEFAULT_MAX_ENTRIES, DEFAULT_MAX_TOTAL_BYTES)
    }

    /// Non-default bounds (used by tests; also handy for constrained tabs).
    pub fn with_limits(dir: PathBuf, max_entries: usize, max_total_bytes: usize) -> Self {
        let _ = std::fs::create_dir_all(&dir);
        Self {
            dir,
            max_entries: max_entries.max(1),
            max_total_bytes: max_total_bytes.max(1),
            inner: RefCell::new(Inner {
                map: HashMap::new(),
                order: VecDeque::new(),
                total_bytes: 0,
            }),
        }
    }

    /// Directory backing this container's cache (`profiles/…/net-cache`).
    pub fn dir(&self) -> &PathBuf {
        &self.dir
    }

    /// Clone of the cached response for `url`, marking it most-recently-used.
    pub fn lookup(&self, url: &str) -> Option<CachedEntry> {
        let mut inner = self.inner.borrow_mut();
        if !inner.map.contains_key(url) {
            return None;
        }
        touch(&mut inner, url);
        Some(inner.map[url].entry.clone())
    }

    /// Insert/replace `url`, evicting least-recently-used entries until the
    /// entry and byte bounds hold. A body larger than the whole byte budget
    /// is not cached.
    pub fn store(&self, url: &str, status: u16, headers: &[(String, String)], body: Vec<u8>, now_unix: i64) {
        let header_bytes: usize = headers
            .iter()
            .map(|(k, v)| k.len() + v.len() + 2)
            .sum();
        let bytes = body.len() + header_bytes;
        if bytes > self.max_total_bytes {
            return;
        }
        let etag = find_header(headers, "etag").map(|s| s.to_string());
        let last_modified = find_header(headers, "last-modified").map(|s| s.to_string());
        let entry = CachedEntry {
            status,
            headers: headers.to_vec(),
            body,
            etag,
            last_modified,
            cached_at: now_unix,
        };
        let mut inner = self.inner.borrow_mut();
        if let Some(old) = inner.map.insert(
            url.to_string(),
            CacheItem {
                entry,
                bytes: bytes.min(self.max_total_bytes),
            },
        ) {
            inner.total_bytes -= old.bytes;
            // Re-inserted: refresh recency.
            touch(&mut inner, url);
        } else {
            inner.order.push_front(url.to_string());
        }
        inner.total_bytes += bytes.min(self.max_total_bytes);
        evict(&mut inner, self.max_entries, self.max_total_bytes);
    }

    /// Drop everything cached (and the cache dir's stale contents).
    pub fn clear(&self) {
        let mut inner = self.inner.borrow_mut();
        inner.map.clear();
        inner.order.clear();
        inner.total_bytes = 0;
        if let Ok(entries) = std::fs::read_dir(&self.dir) {
            for e in entries.flatten() {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }

    /// Number of live entries (tests).
    pub fn len(&self) -> usize {
        self.inner.borrow().map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Total counted bytes of live entries (tests).
    pub fn total_bytes(&self) -> usize {
        self.inner.borrow().total_bytes
    }
}

fn find_header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

// ---------------------------------------------------------------------------
// Explicit freshness (RFC 9111 §4.2) — pure, dependency-free

/// Parse a `delta-seconds` value (bare or quoted, e.g. `max-age="60"`).
fn parse_delta(value: &str) -> Option<i64> {
    value.trim().trim_matches('"').trim().parse::<i64>().ok()
}

/// Days between 0000-03-01 and y-m-d (Howard Hinnant's `days_from_civil`),
/// offset to the Unix epoch. Pure integer arithmetic, no calendar tables.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let year = if m <= 2 { y - 1 } else { y };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let yoe = year - era * 400; // [0, 399]
    let mp = if m > 2 { m - 3 } else { m + 9 }; // [0, 11]
    let doy = (153 * mp + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146097 + doe - 719468
}

/// Minimal IMF-fixdate parser (`Mon, 23 Nov 2037 09:15:42 GMT`), the only
/// date format HTTP allows. Day-of-week is accepted but not cross-checked
/// (a mismatch cannot make a date ambiguous); anything else malformed is
/// `None` — which includes the legacy `Expires: -1` (never-fresh idiom).
fn parse_imf_fixdate(value: &str) -> Option<i64> {
    let (_, rest) = value.split_once(',')?;
    let mut parts = rest.trim().split_whitespace();
    let day: i64 = parts.next()?.parse().ok()?;
    let month = match parts.next()?.to_ascii_lowercase().as_str() {
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
    let year: i64 = parts.next()?.parse().ok()?;
    let (hh, mm, ss) = match parts.next() {
        Some(t) => {
            let mut c = t.split(':');
            let hh: i64 = c.next()?.parse().ok()?;
            let mm: i64 = c.next()?.parse().ok()?;
            let ss: i64 = c.next()?.parse().ok()?;
            (hh, mm, ss)
        }
        None => return None,
    };
    // Time zone: RFC mandates GMT; UTC shows up in the wild and means the same.
    match parts.next() {
        Some(z) if z.eq_ignore_ascii_case("GMT") || z.eq_ignore_ascii_case("UTC") => {}
        _ => return None,
    }
    if parts.next().is_some() {
        return None;
    }
    // Ranges: reject nonsense before it silently wraps into a real instant.
    if year < 1970 || !(1..=31).contains(&day) || !(0..=23).contains(&hh) || !(0..=59).contains(&mm) || !(0..=60).contains(&ss) {
        return None;
    }
    Some(days_from_civil(year, month, day) * 86_400 + hh * 3_600 + mm * 60 + ss)
}

/// The instant (unix seconds) until which the CACHED response described by
/// `headers` (stored at `cached_at`) is **explicitly** fresh — RFC 9111 §4.2
/// explicit freshness only, no heuristics, no sub-second precision.
///
/// * `no-store`, `no-cache`, `must-revalidate` or `proxy-revalidate` ⇒ `None`
///   (the entry is never servable straight from cache).
/// * `s-maxage` wins over `max-age` wins over `Expires` (RFC 9111: when
///   `max-age` is present `Expires` is ignored). Relative to `cached_at`.
/// * No cacheable directives but a valid `Expires` ⇒ that instant; nothing
///   usable ⇒ `None` — unknown freshness must keep revalidating (the
///   behaviour before this function existed). A negative `max-age` yields a
///   past instant, i.e. already expired. Malformed values (`max-age=abc`,
///   `Expires: -1`, garbage dates) are ignored/fall through, never trusted.
///
/// A caller serves directly iff this returns `Some(t)` with `t > now`.
/// Computed from the entry's headers at lookup time — nothing extra is
/// stored on [`CachedEntry`].
pub fn fresh_until(headers: &[(String, String)], cached_at: i64) -> Option<i64> {
    let mut s_maxage: Option<i64> = None;
    let mut max_age: Option<i64> = None;
    // Cache-Control may arrive as several folded headers: analyse them all.
    for (_, value) in headers
        .iter()
        .filter(|(k, _)| k.eq_ignore_ascii_case("cache-control"))
    {
        for directive in value.split(',') {
            let (name, value) = match directive.split_once('=') {
                Some((name, value)) => (name.trim().to_ascii_lowercase(), value.trim()),
                None => (directive.trim().to_ascii_lowercase(), ""),
            };
            match name.as_str() {
                // Any of these forbids answering from cache without the origin.
                "no-store" | "no-cache" | "must-revalidate" | "proxy-revalidate" => {
                    return None;
                }
                // First valid value wins over later malformed duplicates.
                "s-maxage" => s_maxage = parse_delta(value).or(s_maxage),
                "max-age" => max_age = parse_delta(value).or(max_age),
                _ => {} // unknown/extension directives are ignored
            }
        }
    }
    if let Some(seconds) = s_maxage.or(max_age) {
        return Some(cached_at.saturating_add(seconds));
    }
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("expires"))
        .and_then(|(_, value)| parse_imf_fixdate(value))
}

/// Move `url` to the front of the LRU order (it must exist in `map`).
fn touch(inner: &mut Inner, url: &str) {
    if let Some(pos) = inner.order.iter().position(|u| u == url) {
        if let Some(u) = inner.order.remove(pos) {
            inner.order.push_front(u);
        }
    } else {
        inner.order.push_front(url.to_string());
    }
}

// ---------------------------------------------------------------------------
// fresh_until unit tests (pure — no I/O, no clock)
#[cfg(test)]
mod freshness_tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    const CACHED_AT: i64 = 1_700_000_000;

    fn fresh(pairs: &[(&str, &str)]) -> Option<i64> {
        fresh_until(&headers(pairs), CACHED_AT)
    }

    #[test]
    fn max_age_is_relative_to_cached_at() {
        assert_eq!(fresh(&[("Cache-Control", "max-age=3600")]), Some(CACHED_AT + 3600));
        // Quoted values and sloppiness around `=` are still valid delta-seconds.
        assert_eq!(fresh(&[("Cache-Control", "max-age=\"3600\"")]), Some(CACHED_AT + 3600));
        assert_eq!(fresh(&[("Cache-Control", " max-age = 3600 , private ")]), Some(CACHED_AT + 3600));
        // Directive names are case-insensitive.
        assert_eq!(fresh(&[("cache-control", "MAX-AGE=60")]), Some(CACHED_AT + 60));
        assert_eq!(fresh(&[("CACHE-CONTROL", "max-age=60")]), Some(CACHED_AT + 60));
    }

    #[test]
    fn s_maxage_beats_max_age_beats_expires() {
        let expires = "Mon, 23 Nov 2037 09:15:42 GMT";
        assert_eq!(
            fresh(&[
                ("Cache-Control", "max-age=10, s-maxage=20"),
                ("Expires", expires),
            ]),
            Some(CACHED_AT + 20)
        );
        // With both present, max-age WINS over Expires (RFC 9111: Expires is
        // ignored when max-age is present) — NOT the other way round.
        assert_eq!(
            fresh(&[("Cache-Control", "max-age=10"), ("Expires", expires)]),
            Some(CACHED_AT + 10)
        );
        // A malformed s-maxage falls back to max-age, not to Expires.
        assert_eq!(
            fresh(&[
                ("Cache-Control", "s-maxage=abc, max-age=5"),
                ("Expires", expires),
            ]),
            Some(CACHED_AT + 5)
        );
    }

    #[test]
    fn revalidation_directives_forbid_direct_serving() {
        let max_age = [("Cache-Control", "max-age=31536000")];
        assert_eq!(fresh(&[("Cache-Control", "no-store, max-age=31536000")]), None);
        assert_eq!(fresh(&[("Cache-Control", "no-cache, max-age=31536000")]), None);
        assert_eq!(fresh(&[("Cache-Control", "must-revalidate, max-age=31536000")]), None);
        assert_eq!(
            fresh(&[("Cache-Control", "PROXY-REVALIDATE, max-age=31536000")]),
            None
        );
        // Control: without any of them the same max-age IS fresh for a year.
        assert_eq!(fresh(&max_age), Some(CACHED_AT + 31_536_000));
        // no-store beats everything, even a far-future Expires.
        assert_eq!(
            fresh(&[
                ("Cache-Control", "no-store"),
                ("Expires", "Mon, 23 Nov 2037 09:15:42 GMT"),
            ]),
            None
        );
    }

    #[test]
    fn expires_alone_when_no_valid_delta_seconds() {
        // The classic RFC example instant.
        assert_eq!(
            fresh(&[("Expires", "Tue, 15 Nov 1994 12:45:26 GMT")]),
            Some(784_903_526)
        );
        assert_eq!(
            fresh(&[("Expires", "Thu, 01 Jan 1970 00:00:00 GMT")]),
            Some(0)
        );
        // Malformed, out-of-range and the legacy `-1` idiom ⇒ unknown.
        assert_eq!(fresh(&[("Expires", "-1")]), None);
        assert_eq!(fresh(&[("Expires", "tomorrow")]), None);
        assert_eq!(fresh(&[("Expires", "Tue, 15 Nov 1994 12:45:26 UTC+2")]), None);
        assert_eq!(fresh(&[("Expires", "Tue, 32 Nov 1994 12:45:26 GMT")]), None);
        assert_eq!(fresh(&[("Expires", "15 Nov 1994 12:45:26 GMT")]), None); // no weekday+comma
    }

    #[test]
    fn no_directives_at_all_is_unknown_freshness() {
        assert_eq!(fresh(&[("Content-Type", "text/html")]), None);
        assert_eq!(fresh(&[]), None);
    }

    #[test]
    fn negative_seconds_is_a_past_instant_not_an_error() {
        // Some(past) means "already expired", which revalidates like None —
        // but it is an *explicit* statement, so it stays Some.
        assert_eq!(fresh(&[("Cache-Control", "max-age=-10")]), Some(CACHED_AT - 10));
        assert_eq!(
            fresh(&[("Cache-Control", "s-maxage=-1, max-age=3600")]),
            Some(CACHED_AT - 1)
        );
    }

    #[test]
    fn folded_cache_control_headers_are_all_analysed() {
        assert_eq!(
            fresh(&[
                ("Cache-Control", "public"),
                ("Cache-Control", "max-age=90"),
            ]),
            Some(CACHED_AT + 90)
        );
        assert_eq!(
            fresh(&[
                ("Cache-Control", "max-age=90"),
                ("Cache-Control", "no-cache"),
            ]),
            None
        );
    }

    #[test]
    fn unknown_directives_are_ignored() {
        assert_eq!(
            fresh(&[("Cache-Control", "public, immutable, stale-while-revalidate=60, max-age=30")]),
            Some(CACHED_AT + 30)
        );
    }
}

/// Evict LRU entries (back of `order`) until both bounds hold.
fn evict(inner: &mut Inner, max_entries: usize, max_total_bytes: usize) {
    while inner.map.len() > max_entries || inner.total_bytes > max_total_bytes {
        match inner.order.back().cloned() {
            Some(url) => {
                inner.order.pop_back();
                if let Some(item) = inner.map.remove(&url) {
                    inner.total_bytes = inner.total_bytes.saturating_sub(item.bytes);
                }
            }
            None => break,
        }
    }
}
