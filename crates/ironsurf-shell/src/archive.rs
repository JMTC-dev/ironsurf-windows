//! Archive-mode disk side (the 💾 toolbar toggle, shell half).
//!
//! Tabs never write the archive — they ship every successful http(s)
//! response as [`ironsurf_common::FromTab::ArchivedFetch`] and the shell
//! lays the bytes out under the configured root:
//!
//! ```text
//! <root>/<ISO8601 tab-spawn timestamp>/<host[:port]>/index.html
//! <root>/<ISO8601 tab-spawn timestamp>/<host[:port]>/js/app.js
//! ```
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
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join("Downloads").join(ARCHIVE_SUBDIR)
}

/// The per-tab timestamp directory name: local wall-clock time in ISO8601
/// with the numeric UTC offset, second precision — e.g.
/// `2026-09-17T07:20:48+10:00` (`chrono` formats the offset exactly like
/// that). Taken once per tab spawn, so subsequent tab generations land in
/// their own folder.
pub fn visit_dir_name() -> String {
    chrono::Local::now()
        .format("%Y-%m-%dT%H:%M:%S%:z")
        .to_string()
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
/// the host segment (`127.0.0.1:8000`), and a main document is always the
/// host's `index.html` (the visit's front page). `None` = not archivable
/// (non-http(s) scheme, or no host). Query and fragment never reach disk:
/// the same URL minus `?a=b` is the same file.
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
    let mut rel = PathBuf::from(authority);
    if is_document {
        rel.push("index.html");
        return Some(rel);
    }
    let mut segs: Vec<String> = url
        .path_segments()?
        .filter_map(sanitize_segment)
        .collect();
    if segs.is_empty() {
        // A subresource fetched from the bare root (`/`) has no name of its
        // own; the root document's file name is the honest one.
        segs.push("index.html".into());
    }
    for seg in segs {
        rel.push(seg);
    }
    Some(rel)
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

    fn rel(url: &str, is_document: bool) -> PathBuf {
        archive_relative_path(url, is_document).expect("archivable url")
    }

    #[test]
    fn documents_land_on_the_host_index() {
        assert_eq!(rel("http://example.com/", true), Path::new("example.com/index.html"));
        assert_eq!(
            rel("https://example.com/deep/path?a=1#frag", true),
            Path::new("example.com/index.html")
        );
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
        assert_eq!(
            rel("https://example.com/assets/logo@2x.png?v=7", false),
            Path::new("example.com/assets/logo@2x.png")
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
        let tail = name.split_once('T').unwrap().1;
        let (clock, zone) = match tail.split_once(['+', '-']) {
            Some((clock, zone)) => (clock, Some(zone)),
            None => (tail, None),
        };
        assert!(clock.bytes().all(|b| b.is_ascii_digit() || b == b':') && clock.len() == 8, "{name}");
        if let Some(zone) = zone {
            assert!(zone.len() == 5 || zone.ends_with(":00"), "{name}");
        }
    }

    #[test]
    fn write_archived_stays_inside_the_visit_folder() {
        let root = std::env::temp_dir().join(format!("ironsurf-archive-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        write_archived(&root, "2026-09-17T07:20:48+10:00", Path::new("example.com/index.html"), b"<html>")
            .expect("write index");
        let written = root.join("2026-09-17T07:20:48+10:00").join("example.com/index.html");
        assert_eq!(std::fs::read(&written).unwrap(), b"<html>");
        // a hostile relative path (however it got built) is refused, and
        // nothing outside the visit folder is touched.
        assert!(write_archived(&root, "v", Path::new("../../escape.txt"), b"x").is_err());
        assert!(!root.join("escape.txt").exists());
        let _ = std::fs::remove_dir_all(&root);
    }
}
