//! Archive-mode disk side (the 💾 toolbar toggle, shell half).
//!
//! Tabs never write the archive — they ship every successful http(s)
//! response as [`ironsurf_common::FromTab::ArchivedFetch`] and the shell
//! lays the bytes out under the configured root:
//!
//! ```text
//! <root>/<ISO8601 tab-spawn timestamp>/<host[:port]>/index.html
//! <root>/<ISO8601 tab-spawn timestamp>/<host[:port]>/js/app.js
//! <root>/<ISO8601 tab-spawn timestamp>/<host[:port]>/html/?q=asdf
//! ```
//!
//! The URL path names the folder chain and the **query rides on the last
//! path segment as part of the file name** — `?q=asdf` is its own file next
//! to `index.html`, so two different query URLs of the same page keep two
//! archived bodies.
//!
//! The timestamp directory is taken once per tab process (its spawn
//! instant, or the moment archive mode was switched on for a tab that was
//! already live), so one run of archive mode per tab lands in one folder
//! and later tab generations never overwrite it. Everything here is pure
//! (paths + bytes, no Slint, no processes) so the mapping is unit-tested.

use std::path::{Component, Path, PathBuf};

use url::Url;

/// The default folder name under `$HOME/Downloads` (see
/// [`default_archive_root`]).
pub const ARCHIVE_SUBDIR: &str = "ironsurf-tabs";

/// The built-in archive root: `$HOME/Downloads/ironsurf-tabs` (relative
/// `./Downloads/ironsurf-tabs` when `HOME` is unset — the shell is a GUI
/// process, `HOME` is set for every real session).
pub fn default_archive_root() -> PathBuf {
    let home = ironsurf_common::home_dir()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join("Downloads").join(ARCHIVE_SUBDIR)
}

/// The per-tab timestamp directory name: local wall-clock time in ISO8601
/// with the numeric UTC offset, second precision — e.g.
/// `2026-09-17T07:20:48+10:00` (`chrono` formats the offset exactly like
/// that). Taken once per tab spawn, so subsequent tab generations land in
/// their own folder.
///
/// Windows forbids `:` in file names, so there every colon becomes `-`
/// (`2026-09-17T07-20-48+10-00`): same digits, same sort order.
pub fn visit_dir_name() -> String {
    let name = chrono::Local::now()
        .format("%Y-%m-%dT%H:%M:%S%:z")
        .to_string();
    if cfg!(windows) {
        name.replace(':', "-")
    } else {
        name
    }
}

/// Percent-decode UTF-8 `%XX` escapes; malformed escapes pass through
/// verbatim so a name never gains meaning it did not have on the wire.
fn decode_percent(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && bytes[i + 1].is_ascii_hexdigit()
            && bytes[i + 2].is_ascii_hexdigit()
        {
            let hex = &s[i + 1..i + 3];
            if let Ok(byte) = u8::from_str_radix(hex, 16) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    // A decoded name may split UTF-8 sequences only if the URL carried raw
    // non-UTF-8 bytes; keep the lossy reading rather than dropping the file.
    String::from_utf8_lossy(&out).into_owned()
}

/// One URL path segment as a file/dir name: percent-decoded, control chars
/// and separators stripped, dot-segments dropped. `None` = the segment
/// contributes nothing (empty, `.`, `..`, all-noise).
fn sanitize_segment(raw: &str) -> Option<String> {
    let decoded = decode_percent(raw);
    let cleaned: String = decoded
        .chars()
        .filter(|c| !c.is_control() && !matches!(c, '/' | '\\'))
        .collect();
    let cleaned = cleaned.trim_matches(|c| c == ' ' || c == '.').to_string();
    // `trim_matches` may have emptied a "...." / dots-only name; and the
    // dot-segments themselves are exactly what the traversal guard is about.
    if cleaned.is_empty() || cleaned == "." || cleaned == ".." {
        return None;
    }
    Some(cleaned)
}

/// Where one archived fetch lands, relative to the per-tab timestamp
/// directory: `<host[:port]>/<sanitized URL path>` — a plain port rides in
/// the host segment (`127.0.0.1:8000`). The URL **query** is kept: it
/// appends `?<query>` to the last path segment, so `…/html/?q=asdf` is the
/// file `?q=asdf` inside `…/html/`, and `…/search?q=a?q=b` (an empty last
/// segment) is `search?q=a?q=b` — the same URL minus `?a=b` is a different
/// file. A document whose URL carries no query is the host's `index.html`
/// (the visit's front page). `None` = not archivable (non-http(s) scheme,
/// or no host). The fragment never reaches disk.
pub fn archive_relative_path(raw_url: &str, is_document: bool) -> Option<PathBuf> {
    let url = Url::parse(raw_url).ok()?;
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?;
    // Only an explicitly written port joins the host segment; the scheme's
    // default (80/443) would split `example.com:80` from `example.com`.
    let authority = match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_owned(),
    };
    let mut rel = PathBuf::from(fs_safe_name(&authority));
    // `url.query()` is Some("") for a bare trailing `?`, which still names a
    // file (`<name>?`) distinct from the same URL with no query at all.
    let query = url.query().map(|q| sanitize_query(q));
    // A query-less document is the visit's front page — the host's
    // `index.html`, whatever path redirected into it. Once the URL carries a
    // query the file is named by the full URL (path + `?query`), so
    // `/html/?q=a` and `/html/?q=b` archive side by side.
    let mut segs: Vec<String> = if is_document && query.is_none() {
        Vec::new()
    } else {
        url.path_segments()?.filter_map(sanitize_segment).collect()
    };
    if query.is_some() && url.path().ends_with('/') {
        // A URL that ends in `/` names a *directory*: with a query its own
        // file name is empty, and the query becomes the whole name —
        // `/html/?q=asdf` is `?q=asdf` inside `html/`, not `html?q=asdf`.
        segs.push(String::new());
    } else if segs.is_empty() {
        // The bare root (`/`) has no name of its own; the root document's
        // file name is the honest one.
        segs.push("index.html".into());
    }
    let last = segs.len() - 1;
    if let Some(q) = query {
        segs[last].push('?');
        segs[last].push_str(&q);
    }
    for seg in segs {
        rel.push(clamp_name(&fs_safe_name(&seg)));
    }
    Some(rel)
}

/// One path component as this platform's file system will take it.
///
/// Unix takes every name built above. Windows refuses `< > : " | ? *` (the
/// `host:port` folder, every `?query` name, `/wiki/Special:Search`), the DOS
/// device names (`CON`, `NUL`, `COM1`… with or without an extension) and a
/// trailing `.` or space. There those characters — and `%` itself, so two
/// different wire names never meet on disk — are percent-escaped
/// (`?q=1` → `%3Fq=1`) and a device name gains a leading `_`. The archive
/// server serves whatever names are on disk, so nothing else changes.
pub fn fs_safe_name(name: &str) -> String {
    if !cfg!(windows) {
        return name.to_owned();
    }
    let mut out = String::with_capacity(name.len());
    for c in name.chars() {
        match c {
            '<' | '>' | ':' | '"' | '|' | '?' | '*' | '%' => {
                out.push_str(&format!("%{:02X}", c as u32))
            }
            c => out.push(c),
        }
    }
    if let Some(last) = out.pop() {
        if matches!(last, '.' | ' ') {
            out.push_str(&format!("%{:02X}", last as u32));
        } else {
            out.push(last);
        }
    }
    let stem = out.split('.').next().unwrap_or_default().trim_end().to_ascii_uppercase();
    let device = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (stem.len() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && matches!(stem.as_bytes()[3], b'1'..=b'9'));
    if device {
        out.insert(0, '_');
    }
    out
}

/// Longest file-system component we ever emit (Linux `NAME_MAX`). A query
/// can be arbitrarily long, and a name over the limit is a write that fails;
/// cutting at a UTF-8 boundary keeps a readable (if last-writer-wins) file.
const NAME_MAX: usize = 255;

fn clamp_name(name: &str) -> String {
    if name.len() <= NAME_MAX {
        return name.to_owned();
    }
    let mut end = NAME_MAX;
    while end > 0 && !name.is_char_boundary(end) {
        end -= 1;
    }
    name[..end].to_owned()
}

/// The query as a file-name suffix: percent-decoded, control chars and
/// separators stripped. Unlike a path segment it keeps leading/trailing `.`
/// and `?` (`?q=..` means what it says on the wire); an all-noise query
/// still leaves the `?` marker, so it is always Some.
fn sanitize_query(raw: &str) -> String {
    decode_percent(raw)
        .chars()
        .filter(|c| !c.is_control() && !matches!(c, '/' | '\\'))
        .collect()
}

/// Write one archived body to `root/visit/rel`. The relative path is
/// re-checked (no absolute, no parent escapes) before anything is created,
/// so a hostile URL can only ever land inside its timestamp folder. Parents
/// are created on demand; an existing file is overwritten (same-tab
/// re-fetch = the newest bytes win).
pub fn write_archived(root: &Path, visit: &str, rel: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut path = root.join(visit);
    for comp in rel.components() {
        match comp {
            Component::Normal(name) => path.push(name),
            Component::CurDir => {}
            // Rejected: a traversal component never belongs to a mapped URL.
            _ => return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "unsafe path")),
        }
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The archive path for `url`, read back in wire spelling: on Windows the
    /// file-system escapes ([`fs_safe_name`], which escapes `%` too, so this
    /// is exact) are undone, and one expectation serves every platform. The
    /// escaping itself has its own test below.
    fn rel(url: &str, is_document: bool) -> PathBuf {
        let path = archive_relative_path(url, is_document).expect("archivable url");
        if cfg!(windows) {
            path.iter().map(|c| decode_percent(&c.to_string_lossy())).collect()
        } else {
            path
        }
    }

    #[test]
    fn names_are_escaped_where_the_file_system_refuses_them() {
        let url = "http://127.0.0.1:8000/wiki/Special:Search?q=a*b";
        let path = archive_relative_path(url, false).unwrap();
        if cfg!(windows) {
            assert_eq!(path, Path::new("127.0.0.1%3A8000/wiki/Special%3ASearch%3Fq=a%2Ab"));
            assert_eq!(fs_safe_name("CON"), "_CON");
            assert_eq!(fs_safe_name("nul.txt"), "_nul.txt");
            assert_eq!(fs_safe_name("com1.js"), "_com1.js");
            assert_eq!(fs_safe_name("console.js"), "console.js");
            assert_eq!(fs_safe_name("100%.html"), "100%25.html");
            assert_eq!(fs_safe_name("dots."), "dots%2E");
            // …and the escaped path is one Windows actually creates.
            let root = std::env::temp_dir().join(format!("ironsurf-archive-names-{}", std::process::id()));
            write_archived(&root, "v", &path, b"x").expect("Windows takes the escaped path");
            let _ = std::fs::remove_dir_all(&root);
        } else {
            assert_eq!(path, Path::new("127.0.0.1:8000/wiki/Special:Search?q=a*b"));
            assert_eq!(fs_safe_name("CON"), "CON");
        }
    }

    #[test]
    fn documents_land_on_the_host_index() {
        assert_eq!(rel("http://example.com/", true), Path::new("example.com/index.html"));
        // No query keeps the old rule: a deep (or redirected) path is still
        // the visit's front page.
        assert_eq!(rel("https://example.com/deep/path#frag", true), Path::new("example.com/index.html"));
    }

    #[test]
    fn the_query_rides_on_the_file_name() {
        // The requested shape: the folder chain is the URL path, the query is
        // part of the file name.
        assert_eq!(
            rel("http://localhost/html/?q=asdf", true),
            Path::new("localhost/html/?q=asdf")
        );
        // Different queries of the same page are different files…
        assert_eq!(
            rel("http://localhost/html/?q=other", true),
            Path::new("localhost/html/?q=other")
        );
        // …and so is the same page with no query at all.
        assert_eq!(rel("http://localhost/html/", true), Path::new("localhost/index.html"));
        // The fragment still never reaches disk; a bare `?` names its own file.
        assert_eq!(
            rel("http://localhost/html/?q=a%20b#frag", true),
            Path::new("localhost/html/?q=a b")
        );
        assert_eq!(rel("http://localhost/html/?", true), Path::new("localhost/html/?"));
        // A query on the bare root is a file of the host folder itself.
        assert_eq!(rel("http://localhost/?q=1", true), Path::new("localhost/?q=1"));
        assert_eq!(rel("http://localhost?q=1", true), Path::new("localhost/?q=1"));
        // Without the trailing slash the query rides on the last name.
        assert_eq!(
            rel("http://localhost/html?q=asdf", true),
            Path::new("localhost/html?q=asdf")
        );
        // Subresources keep their query too, attached to the file name.
        assert_eq!(
            rel("http://localhost/html/app.js?q=1", false),
            Path::new("localhost/html/app.js?q=1")
        );
        // A `?` in the raw URL that is not a query separator (percent-encoded)
        // stays an ordinary character of the name.
        assert_eq!(
            rel("http://localhost/a%3fb.js", false),
            Path::new("localhost/a?b.js")
        );
    }

    #[test]
    fn query_names_stay_traversable_and_bounded() {
        // Separators inside a query cannot climb out of the host folder.
        assert_eq!(
            rel("http://localhost/html/?q=..%2f..%2fetc%2fpasswd", true),
            Path::new("localhost/html/?q=....etcpasswd")
        );
        assert_eq!(
            rel("https://example.com/search?q=%2e%2e%2f%2e%2eshadow", false),
            Path::new("example.com/search?q=....shadow")
        );
        // An over-long query is cut to one legal component, not a failed write.
        let long = format!("http://localhost/html/?q={}", "x".repeat(400));
        let path = rel(&long, true);
        let file = path.file_name().unwrap().to_str().unwrap();
        assert!(file.starts_with("?q="), "{file}");
        assert!(file.len() <= 255, "{} bytes", file.len());
    }

    #[test]
    fn explicit_ports_join_the_host_segment() {
        assert_eq!(
            rel("http://127.0.0.1:8000/index.html", true),
            Path::new("127.0.0.1:8000/index.html")
        );
        assert_eq!(
            rel("http://127.0.0.1:8000/image.png", false),
            Path::new("127.0.0.1:8000/image.png")
        );
        // The scheme's default port does NOT: 80 must not split example.com.
        assert_eq!(
            rel("http://example.com:80/a.js", false),
            Path::new("example.com/a.js")
        );
    }

    #[test]
    fn subresources_keep_their_url_path() {
        assert_eq!(
            rel("https://example.com/js/app.min.js", false),
            Path::new("example.com/js/app.min.js")
        );
        // The query is part of the name, so cache-busted versions never
        // overwrite each other.
        assert_eq!(
            rel("https://example.com/assets/logo@2x.png?v=7", false),
            Path::new("example.com/assets/logo@2x.png?v=7")
        );
        assert_eq!(
            rel("https://example.com/assets/logo@2x.png?v=8", false),
            Path::new("example.com/assets/logo@2x.png?v=8")
        );
        // A bare-root subresource falls back to the index name.
        assert_eq!(rel("https://example.com", false), Path::new("example.com/index.html"));
    }

    #[test]
    fn traversal_and_noise_segments_are_neutralized() {
        // `%2e%2e` dot-segments are resolved by URL parsing itself, and
        // anything left over (a raw `..`, a decoded `..`) is dropped by the
        // sanitizer — a hostile path can only ever land deeper inside the
        // host's own folder.
        assert_eq!(
            rel("https://example.com/a/%2e%2e/%2e%2e/etc/passwd", false),
            Path::new("example.com/etc/passwd")
        );
        assert_eq!(sanitize_segment("..").as_deref(), None);
        assert_eq!(sanitize_segment("%2e%2e").as_deref(), None);
        assert_eq!(sanitize_segment("a%2fb%5cc").as_deref(), Some("abc"));
        assert_eq!(sanitize_segment("....").as_deref(), None);
        assert_eq!(
            rel("https://example.com/..%2f..%2fshadow", false),
            Path::new("example.com/shadow")
        );
        // empty segments collapse (`//` in a path is just `/`)
        assert_eq!(
            rel("https://example.com//css//site.css", false),
            Path::new("example.com/css/site.css")
        );
    }

    #[test]
    fn percent_escapes_decode_to_readable_names() {
        assert_eq!(
            rel("https://example.com/my%20file.png", false),
            Path::new("example.com/my file.png")
        );
        // a malformed escape passes through verbatim
        assert_eq!(
            rel("https://example.com/100%zz.png", false),
            Path::new("example.com/100%zz.png")
        );
    }

    #[test]
    fn only_http_urls_are_archivable() {
        for url in [
            "file:///etc/passwd",
            "data:text/html,hi",
            "about:blank",
            "ironsurf:welcome",
            "not a url",
        ] {
            assert!(
                archive_relative_path(url, false).is_none(),
                "{url} must not be archived"
            );
            assert!(archive_relative_path(url, true).is_none(), "{url} must not be archived");
        }
    }

    #[test]
    fn visit_names_are_iso8601_with_a_numeric_offset() {
        let name = visit_dir_name();
        // The contract shape: 2026-09-17T07:20:48+10:00 (chrono %:z — the
        // offset always carries its colon). Accept the two offset extremes
        // a UTC-configured box may print, but insist on the digits.
        assert!(name.len() >= 25, "{name}");
        let (naive, _) = name.split_once('T').expect("T separator");
        assert_eq!(naive.len(), 10, "{name}");
        // Windows spells every colon as `-` (file names cannot hold `:`).
        let sep = if cfg!(windows) { '-' } else { ':' };
        let tail = name.split_once('T').unwrap().1;
        let (clock, zone) = (&tail[..8], &tail[8..]);
        assert!(
            clock.bytes().all(|b| b.is_ascii_digit() || b == sep as u8)
                && clock.as_bytes()[2] == sep as u8,
            "{name}"
        );
        assert!(zone.starts_with(['+', '-']) && zone.len() == 6, "{name}");
        assert_eq!(zone.as_bytes()[3], sep as u8, "{name}");
        if cfg!(windows) {
            assert!(!name.contains(':'), "{name}");
        }
    }

    #[test]
    fn write_archived_stays_inside_the_visit_folder() {
        let root = std::env::temp_dir().join(format!("ironsurf-archive-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        // A colon-free visit name: valid on every platform (Windows' shape).
        write_archived(&root, "2026-09-17T07-20-48+10-00", Path::new("example.com/index.html"), b"<html>")
            .expect("write index");
        let written = root.join("2026-09-17T07-20-48+10-00").join("example.com/index.html");
        assert_eq!(std::fs::read(&written).unwrap(), b"<html>");
        // a hostile relative path (however it got built) is refused, and
        // nothing outside the visit folder is touched.
        assert!(write_archived(&root, "v", Path::new("../../escape.txt"), b"x").is_err());
        assert!(!root.join("escape.txt").exists());
        let _ = std::fs::remove_dir_all(&root);
    }
}
