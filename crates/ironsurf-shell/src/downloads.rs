//! File downloads (task: file downloads) — the **shell** half, pure.
//!
//! A tab decides a top-level fetch is a file (attachment header, a media type
//! no page is made of, a clicked `<a download>`, or a shell
//! [`ironsurf_common::ToTab::DownloadUrl`]) and ships it as
//! `DownloadStart` → `DownloadChunk`* → `DownloadEnd`. **The tab never
//! touches the disk**: the shell files the row, names the file, creates it
//! with `O_CREAT | O_EXCL` and appends every chunk (see the tab's own
//! `downloads.rs` for the sending half).
//!
//! Everything here is paths + bytes — no Slint, no processes, no environment
//! mutation — so the naming rules, the collision counter and the labels are
//! unit-tested headless.
//!
//! Names come from hostile input (`Content-Disposition`, an anchor value, the
//! URL the tab fetched): [`sanitize_file_name`] of the suggestion, else
//! [`filename_from_url`], else [`DEFAULT_NAME`]. The result is then made
//! **unique in the folder** — `report.zip`, `report (2).zip`, `report
//! (3).zip`… — and is always a direct child of the download folder, whatever
//! the suggestion contained (`../../../.ssh/x`, `/etc/passwd`, a
//! `..%2f`-decoded `../x`).

use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::state::DownloadState;
use ironsurf_common::{filename_from_url, sanitize_file_name};

/// The name nothing else offered falls back to (a `data:` URL, a query-only
/// address — nothing names a file, so the file is named after the act).
pub const DEFAULT_NAME: &str = "download";

/// The env var that overrides the built-in folder. An absolute value only:
/// a relative one cannot say where the disk is, so it is ignored (the same
/// rule the archive folder applies to a relative setting).
pub const DOWNLOAD_DIR_ENV: &str = "IRONSURF_DOWNLOAD_DIR";

/// Rows the ⬇ panel lists at most (newest first). The store on each tab is
/// bigger — the panel is a window onto it, like the History menu's window on
/// the visit log.
pub const MENU_ROWS: usize = 50;

/// The folder downloads land in: `$IRONSURF_DOWNLOAD_DIR` when set and
/// absolute, else `$XDG_DOWNLOAD_DIR`, else `$HOME/Downloads` — the default is
/// the same folder the OS and every other browser uses (the archive keeps its
/// own `…/ironsurf-tabs` subfolder inside it, so the two never share a
/// directory).
///
/// A folder in `UiSettings::download_dir` outranks this entirely; a blank one
/// falls back here.
pub fn download_root(dir: Option<&str>) -> PathBuf {
    download_root_with(dir, |name| std::env::var_os(name))
}

/// [`download_root`] with the environment handed in, so a test can say what
/// `$HOME` and friends hold without mutating the process environment (which
/// every other test in this binary shares).
fn download_root_with(
    dir: Option<&str>,
    env: impl Fn(&str) -> Option<OsString>,
) -> PathBuf {
    let setting = dir.map(str::trim).filter(|d| !d.is_empty());
    if let Some(setting) = setting {
        // An absolute setting (after `~` expansion) is the user's answer and
        // outranks every env var; a relative one says nothing about where the
        // disk is, so it is rejected against the live default — which is the
        // env's answer, not the built-in one.
        if let Some(path) = absolute(setting, &env) {
            return path;
        }
    }
    for name in [DOWNLOAD_DIR_ENV, "XDG_DOWNLOAD_DIR"] {
        if let Some(raw) = env(name) {
            if let Some(path) = absolute(&raw.to_string_lossy(), &env) {
                return path;
            }
        }
    }
    // The built-in: `~/Downloads`, or `./Downloads` when `HOME` is unset (the
    // shell is a GUI process; `HOME` is set for every real session).
    let home = ironsurf_common::home_dir_with(&env)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join("Downloads")
}

/// A path the user (or an env var) meant, expanded and taken only when it is
/// absolute — `~/Downloads` counts as absolute the way the archive folder
/// teaches it to.
fn absolute(raw: &str, env: impl Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    let expanded = expand_tilde_with(raw, env);
    let path = PathBuf::from(&expanded);
    path.is_absolute().then_some(path)
}

/// Expand a leading `~`/`~/` against `$HOME` — the same service the archive
/// folder gives the user: a setting may be written `~/Downloads`.
pub fn expand_tilde(raw: &str) -> String {
    expand_tilde_with(raw, |name| std::env::var_os(name))
}

/// [`expand_tilde`] with `$HOME` handed in (see [`download_root_with`]).
fn expand_tilde_with(raw: &str, env: impl Fn(&str) -> Option<OsString>) -> String {
    if raw == "~" {
        return ironsurf_common::home_dir_with(&env)
            .map(|home| PathBuf::from(home).display().to_string())
            .unwrap_or_else(|| raw.to_owned());
    }
    if let Some(rest) = raw.strip_prefix("~/") {
        if let Some(home) = ironsurf_common::home_dir_with(&env) {
            let mut path = PathBuf::from(home);
            path.push(rest);
            return path.display().to_string();
        }
    }
    raw.to_owned()
}

/// Accept or refuse a folder typed into the ⬇ panel — the archive folder's
/// rules, verbatim: it must be absolute (after `~` expansion) and creatable
/// right now (`create_dir_all` is what an accepted folder becomes). The
/// rejected draft survives in the LineEdit; only an accepted one is saved.
pub fn validate_dir(text: &str) -> Result<PathBuf, String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err("give an absolute path (or ~/…)".to_owned());
    }
    let expanded = expand_tilde(trimmed);
    let path = PathBuf::from(&expanded);
    if !path.is_absolute() {
        return Err(format!("{expanded} is not absolute — try ~/{expanded}"));
    }
    // A folder that cannot be created now is refused before it is saved: a
    // silent write failure later is the worse outcome.
    if let Err(err) = std::fs::create_dir_all(&path) {
        return Err(format!("cannot create {expanded}: {err}"));
    }
    Ok(path)
}

/// The name a download is shown by while it streams: the tab's suggestion
/// sanitized, else the URL's last segment, else [`DEFAULT_NAME`]. Not yet
/// made unique — that happens on disk, see [`download_path`].
pub fn display_name(suggested: Option<&str>, url: &str) -> String {
    suggested
        .and_then(sanitize_file_name)
        .or_else(|| filename_from_url(url))
        .unwrap_or_else(|| DEFAULT_NAME.to_owned())
}

/// The name the "Save page" dialog (task: save-page selector) proposes for
/// the page on screen: the URL's own file name when it has one, else the
/// page title with `.html` put back on it (a page is saved as a page), else
/// `"page.html"`. Sanitized the way every download name is, so the dialog
/// always opens holding a legal name.
pub fn save_page_name(url: &str, title: &str) -> String {
    if let Some(name) = filename_from_url(url) {
        return name;
    }
    if let Some(from_title) = sanitize_file_name(title.trim()) {
        let (stem, ext) = split_ext(&from_title);
        return if ext.is_empty() { format!("{stem}.html") } else { from_title };
    }
    "page.html".to_owned()
}

/// The name a "Save page" dialog confirms with: the draft trimmed and
/// sanitized, or `fallback` when it holds nothing a file may be called
/// (blank, `.`, `..`, a bare separator). Never a path — one sanitized
/// component whatever was typed, the way every download name is.
pub fn save_name_or(raw: &str, fallback: &str) -> String {
    sanitize_file_name(raw.trim()).unwrap_or_else(|| fallback.to_owned())
}

/// The file a download becomes: `name` sanitized, falling back to
/// [`filename_from_url`] then [`DEFAULT_NAME`], then made unique in `root`
/// (see [`unique_in_root`]). Traversal-safe by construction: only a single
/// sanitized component is ever joined onto `root`, so the result is a direct
/// child of the folder whatever the suggestion carried.
pub fn download_path(root: &Path, name: Option<&str>, url: &str) -> PathBuf {
    unique_in_root(root, &display_name(name, url))
}

/// The unique path for `base` in `root`: `root/base` when free, else the
/// counter rides **before** the extension (`report.zip` → `report (2).zip`),
/// starting at 2 — the counter is what makes a second `report.zip` a
/// different file instead of overwriting the first. A name that already
/// carries a counter extends the run (`report (2).zip` → `report (3).zip`).
pub fn unique_in_root(root: &Path, base: &str) -> PathBuf {
    let base = sanitize_file_name(base).unwrap_or_else(|| DEFAULT_NAME.to_owned());
    let (stem, ext) = split_ext(&base);
    // A name that already carries a counter (`report (3).zip`) is claimed by
    // the number it holds: whoever asks for it wants the next file in that
    // run, not the one they already have. A plain name starts at itself.
    let (stem, carried) = carried_counter(&stem);
    let mut counter = carried.map(|n| n + 1).unwrap_or(1);
    loop {
        let name = match counter {
            1 if carried.is_none() => base.clone(),
            n => format!("{stem} ({n}){ext}"),
        };
        let path = root.join(name);
        // `exists()` cannot close the race this counter is about (another
        // writer may create the same file between this check and the
        // `O_EXCL` open) — the open is the arbiter, so a caller that finds
        // the path taken simply calls again.
        if !path.exists() {
            return path;
        }
        if counter >= 9999 {
            // A folder holding 9999 `report (…).zip` is a bug elsewhere;
            // give up rather than spin forever.
            return root.join(format!("{stem} ({n})", n = counter + 1));
        }
        counter += 1;
    }
}

/// `report (2)` → (`report`, `Some(2)`); a stem with no trailing counter is
/// returned as it was, with `None`. The counter a name carries is what makes
/// `report (2).zip` continue the run at `report (3).zip`.
fn carried_counter(stem: &str) -> (String, Option<usize>) {
    match stem.rfind(" (") {
        Some(open) => match stem.get(open + 1..)
            .and_then(|head| head.strip_prefix('('))
            .and_then(|digits| digits.strip_suffix(')'))
            .and_then(|digits| digits.parse::<usize>().ok())
        {
            Some(number) => (stem[..open].to_owned(), Some(number)),
            None => (stem.to_owned(), None),
        },
        None => (stem.to_owned(), None),
    }
}

/// `report.zip` → (`report`, `.zip`); `archive` → (`archive`, `""`); a
/// leading dot belongs to the stem (`.hidden` is a stem, not an extension).
fn split_ext(name: &str) -> (String, String) {
    match name.find('.') {
        Some(idx) if idx > 0 => {
            let (stem, ext) = name.split_at(idx);
            (stem.to_owned(), ext.to_owned())
        }
        _ => (name.to_owned(), String::new()),
    }
}

/// Create the file and write the whole body, returning the byte count. Like
/// the archive's writer it never trusts the caller's path: the parent is
/// created, the file must be **new** (`O_CREAT | O_EXCL` through the unique
/// path) and the write must complete.
pub fn write_download(
    root: &Path,
    name: Option<&str>,
    url: &str,
    bytes: &[u8],
) -> Result<u64, String> {
    let path = download_path(root, name, url);
    let mut file = create(&path)?;
    file.write_all(bytes)
        .map(|_| bytes.len() as u64)
        .map_err(|err| format!("cannot write {}: {err}", path.display()))
}

/// Open a **new** file at `path` (`O_CREAT | O_EXCL`, folder created on
/// demand). A download starts here: the file exists the moment the tab says
/// it started, so a crash mid-fetch leaves a partial file the shell can
/// delete (see [`discard`]).
pub fn create(path: &Path) -> Result<File, String> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)
        .map_err(|err| format!("cannot create {}: {err}", parent.display()))?;
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|err| format!("cannot create {}: {err}", path.display()))
}

/// Append one chunk to a download in flight, reopening each time. Written as
/// it arrives — the shell never buffers a file the size of a `.iso` in RAM.
pub fn append(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let mut file = OpenOptions::new()
        .append(true)
        .open(path)
        .map_err(|err| format!("cannot append {}: {err}", path.display()))?;
    file.write_all(bytes)
        .map_err(|err| format!("cannot write {}: {err}", path.display()))
}

/// A partial file a tab left behind (its process died): delete it, best
/// effort — an unfinished download is not something to leave in the folder.
pub fn discard(path: &Path) {
    let _ = std::fs::remove_file(path);
}

/// The panel's `status` cell: `Saving…`, `Done {bytes} B`, `Failed`.
pub fn status_label(state: DownloadState, bytes: u64) -> String {
    match state {
        DownloadState::Saving => "Saving…".to_owned(),
        DownloadState::Done => format!("Done {bytes} B"),
        DownloadState::Failed => "Failed".to_owned(),
    }
}

/// The panel's `sub` cell: `{bytes} B · {host}` (task: file downloads).
pub fn sub_line(bytes: u64, url: &str) -> String {
    format!("{bytes} B · {}", host_of(url))
}

/// The authority a URL was fetched from — scheme, path, query and fragment
/// dropped, a written port kept (`127.0.0.1:8000`). `""` when no authority
/// exists at all (a `data:` save, a bad scheme): the row already shows a
/// name, it does not show a whole address too.
pub fn host_of(url: &str) -> String {
    // An authority exists only in a `scheme://host…` URL. A `data:` save has
    // none, and its first words are a piece of the payload, not an address.
    let rest = match url.split_once("://") {
        Some((_, rest)) => rest,
        None => return String::new(),
    };
    let authority = rest
        .split(['/', '?', '#', ' ', '\\'])
        .next()
        .unwrap_or_default();
    // A username would be a secret to put in a panel; keep host[:port] only.
    authority.rsplit('@').next().unwrap_or_default().to_owned()
}

/// The ⬇ panel's one-line summary: rows stored and the folder receiving them
/// (the same folder the LineEdit edits, so the panel explains itself).
pub fn summary_line(count: usize, folder: &Path) -> String {
    format!(
        "{count} {} · {}",
        if count == 1 { "download" } else { "downloads" },
        folder.display()
    )
}

/// The status line's byte count, `1.2 MB`-style: a 900-byte row reads
/// `900 B`, a 4 GB one reads `4.3 GB` instead of `4294967296 B`.
pub fn human_bytes(bytes: u64) -> String {
    // SI, one decimal: a 900-byte row reads `900 B`, a 4 GB one reads
    // `4.3 GB` instead of `4294967296 B`.
    const UNITS: [(f64, f64, &str); 3] = [
        (1e6, 1e3, "kB"),
        (1e9, 1e6, "MB"),
        (1e12, 1e9, "GB"),
    ];
    let value = bytes as f64;
    if value < 1e3 {
        return format!("{bytes} B");
    }
    for (threshold, divisor, name) in UNITS {
        if value < threshold {
            return format!("{:.1} {name}", value / divisor);
        }
    }
    format!("{:.1} TB", value / 1e12)
}

/// The status line a download keeps while it runs (see the pump's rule): the
/// newest row is what the reader should see without opening the panel — a
/// `Saving` row leads with its name and progress, a finished one with size.
pub fn status_line(name: &str, state: DownloadState, bytes: u64) -> String {
    match state {
        DownloadState::Saving => format!("Saving {name} — {}", human_bytes(bytes)),
        DownloadState::Done => format!("Saved {name} — {}", human_bytes(bytes)),
        DownloadState::Failed => format!("Failed {name}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// An environment for one test: `None` for every name the case does not
    /// name (so a stray `XDG_DOWNLOAD_DIR` in the runner's env cannot steer).
    fn env(pairs: &[(&str, Option<&str>)]) -> impl Fn(&str) -> Option<OsString> {
        let owned: Vec<(String, Option<OsString>)> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.map(OsString::from)))
            .collect();
        move |name| {
            // Last wins: a case pushes its override after the shared
            // `home_env()` defaults, and says nothing about names it omits.
            owned
                .iter()
                .rev()
                .find(|(k, _)| k == name)
                .and_then(|(_, v)| v.clone())
        }
    }

    /// An absolute path in this platform's spelling: the Unix fixtures below
    /// gain a drive on Windows (`/home/ada` → `C:/home/ada`), where a path
    /// without one is not absolute.
    fn abs(path: &str) -> &'static str {
        if cfg!(windows) {
            Box::leak(format!("C:{path}").into_boxed_str())
        } else {
            Box::leak(path.to_owned().into_boxed_str())
        }
    }

    fn home_env() -> Vec<(&'static str, Option<&'static str>)> {
        vec![
            ("HOME", Some(abs("/home/ada"))),
            ("XDG_DOWNLOAD_DIR", None),
            ("IRONSURF_DOWNLOAD_DIR", None),
        ]
    }

    #[test]
    fn the_default_folder_is_downloads_inside_home() {
        let e = env(&home_env());
        assert_eq!(
            download_root_with(None, &e),
            PathBuf::from(abs("/home/ada/Downloads")),
            "the built-in is the folder the OS itself uses"
        );
        assert_ne!(
            download_root_with(None, &e),
            PathBuf::from(abs("/home/ada/Downloads/ironsurf-tabs")),
            "…and never the archive's directory (its own subfolder inside)"
        );
    }

    #[test]
    fn an_env_var_only_wins_when_absolute() {
        let mut pairs = home_env();
        pairs.push(("IRONSURF_DOWNLOAD_DIR", Some(abs("/mnt/nas/saves"))));
        assert_eq!(download_root_with(None, &env(&pairs)), PathBuf::from(abs("/mnt/nas/saves")));
        let mut pairs = home_env();
        pairs.push(("IRONSURF_DOWNLOAD_DIR", Some("relative/thing")));
        assert_eq!(
            download_root_with(None, &env(&pairs)),
            PathBuf::from(abs("/home/ada/Downloads")),
            "a relative override says nothing about where the disk is"
        );
        let mut pairs = home_env();
        pairs.push(("XDG_DOWNLOAD_DIR", Some(abs("/mnt/xdg"))));
        assert_eq!(download_root_with(None, &env(&pairs)), PathBuf::from(abs("/mnt/xdg")));
    }

    #[test]
    fn a_setting_outranks_the_env_and_a_blank_one_reverts() {
        let mut pairs = home_env();
        pairs.push(("IRONSURF_DOWNLOAD_DIR", Some(abs("/mnt/nas/saves"))));
        let e = env(&pairs);
        assert_eq!(download_root_with(Some(abs("/tmp/dl")), &e), PathBuf::from(abs("/tmp/dl")));
        assert_eq!(
            download_root_with(Some("   "), &e),
            PathBuf::from(abs("/mnt/nas/saves")),
            "a blank setting reverts to the default"
        );
        assert_eq!(
            download_root_with(Some("relative/x"), &e),
            PathBuf::from(abs("/mnt/nas/saves")),
            "a relative setting is rejected against the live default"
        );
    }

    #[test]
    fn tilde_expands_a_setting_the_way_the_archive_folder_does() {
        let e = env(&home_env());
        assert_eq!(
            absolute("~/Downloads/saves", &e),
            Some(PathBuf::from(abs("/home/ada/Downloads/saves")))
        );
        // Compared as paths: Windows joins `~/x` with its own separator.
        assert_eq!(PathBuf::from(expand_tilde_with("~/x", &e)), PathBuf::from(abs("/home/ada/x")));
        assert_eq!(PathBuf::from(expand_tilde_with("~", &e)), PathBuf::from(abs("/home/ada")));
        assert_eq!(expand_tilde_with("./x", &e), "./x", "only ~ and ~/ expand");
    }

    #[test]
    fn names_come_from_the_suggestion_then_the_url() {
        assert_eq!(display_name(Some("report.zip"), "https://e.com/x"), "report.zip");
        assert_eq!(
            display_name(Some("../../.ssh/x"), "https://e.com/files/note.txt"),
            "note.txt",
            "a suggestion that is only directories names nothing, so the URL does"
        );
        assert_eq!(
            display_name(Some("a/b"), "https://e.com/files/note.txt"),
            "a",
            "a separator inside a suggestion ends the useful part"
        );
        assert_eq!(display_name(None, "https://e.com/files/report.zip"), "report.zip");
        assert_eq!(
            display_name(None, "https://e.com/f/1?x=2"),
            "1",
            "the last path segment names a query-only tail"
        );
        assert_eq!(
            display_name(None, "data:text/plain,hi"),
            DEFAULT_NAME,
            "a URL with nothing to name after falls back"
        );
        assert_eq!(display_name(Some("  "), "https://e.com/"), DEFAULT_NAME);
    }

    #[test]
    fn collisions_count_before_the_extension() {
        let dir = temp_folder("unique");
        let unique = |name: &str| download_path(&dir, Some(name), "https://e.com/f/1");
        assert_eq!(unique("report.zip"), dir.join("report.zip"));
        fs::write(dir.join("report.zip"), b"first").unwrap();
        assert_eq!(unique("report.zip"), dir.join("report (2).zip"));
        fs::write(dir.join("report (2).zip"), b"second").unwrap();
        assert_eq!(unique("report.zip"), dir.join("report (3).zip"));
        // An already-numbered name extends the run of its own stem.
        assert_eq!(unique("report (3).zip"), dir.join("report (4).zip"));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn traversal_shaped_names_stay_inside_the_folder() {
        let dir = temp_folder("traversal");
        for suggestion in [
            "../../../.ssh/x",
            "/etc/passwd",
            "..",
            "sub/../../x",
            ".",
            "%2e%2e%2fpasswd",
        ] {
            let path = download_path(&dir, Some(suggestion), "https://e.com/f/1");
            assert_eq!(
                path.parent(),
                Some(dir.as_path()),
                "{suggestion} → {}",
                path.display()
            );
            assert!(path.file_name().is_some(), "{suggestion} lost its name");
        }
        // A URL whose last segment is hostile names the same way.
        let path = download_path(&dir, None, "https://e.com/../../etc/passwd");
        assert_eq!(path.parent(), Some(dir.as_path()));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn write_download_creates_new_and_counts_bytes() {
        let dir = temp_folder("write");
        let body: &[u8] = b"PK\x03\x04fake-zip";
        let count = write_download(&dir, Some("pkg.zip"), "https://e.com/f/1", body).unwrap();
        assert_eq!(count, body.len() as u64);
        assert_eq!(
            fs::read(dir.join("pkg.zip")).unwrap(),
            body,
            "the folder is created on demand"
        );
        // Second time: a new file, never an overwrite.
        let count = write_download(&dir, Some("pkg.zip"), "https://e.com/f/2", body).unwrap();
        assert_eq!(count, body.len() as u64);
        assert!(dir.join("pkg (2).zip").exists());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn append_adds_to_a_new_file_and_discard_removes_it() {
        let dir = temp_folder("append");
        let path = dir.join("chunked.bin");
        drop(create(&path).unwrap());
        append(&path, b"first ").unwrap();
        append(&path, b"second").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"first second".as_slice());
        discard(&path);
        assert!(!path.exists(), "a partial file does not stay in the folder");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_folder_is_accepted_only_when_absolute_and_creatable() {
        let dir = temp_folder("validate");
        let accepted = validate_dir(&dir.display().to_string()).unwrap();
        assert!(accepted.is_dir(), "an accepted folder is created right there");
        let err = validate_dir("relative/thing").unwrap_err();
        assert!(err.contains("absolute"), "{err}");
        assert!(validate_dir("").is_err(), "blank is not a folder");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn labels_read_the_way_the_panel_shows_them() {
        assert_eq!(status_label(DownloadState::Saving, 0), "Saving…");
        assert_eq!(status_label(DownloadState::Done, 4608), "Done 4608 B");
        assert_eq!(status_label(DownloadState::Failed, 0), "Failed");
        assert_eq!(sub_line(4608, "https://e.com/f/1"), "4608 B · e.com");
        assert_eq!(
            sub_line(1, "http://127.0.0.1:8000/report.zip"),
            "1 B · 127.0.0.1:8000",
            "a written port belongs to the authority"
        );
        assert_eq!(sub_line(1, "data:text/plain,hi"), "1 B · ", "nothing to name");
        assert_eq!(
            sub_line(1, "https://ada:secret@e.com/x"),
            "1 B · e.com",
            "credentials never reach the panel"
        );
        assert_eq!(
            summary_line(2, Path::new("/tmp/dl")),
            "2 downloads · /tmp/dl"
        );
        assert_eq!(summary_line(1, Path::new("/tmp/dl")), "1 download · /tmp/dl");
    }

    #[test]
    fn byte_counts_stay_readable() {
        assert_eq!(human_bytes(900), "900 B");
        assert_eq!(human_bytes(4608), "4.6 kB");
        assert_eq!(human_bytes(1_200_000), "1.2 MB");
        assert_eq!(human_bytes(5 * 1024 * 1024 * 1024), "5.4 GB");
        assert_eq!(
            status_line("report.zip", DownloadState::Saving, 1_200_000),
            "Saving report.zip — 1.2 MB"
        );
        assert_eq!(
            status_line("report.zip", DownloadState::Done, 1_200_000),
            "Saved report.zip — 1.2 MB"
        );
        assert_eq!(
            status_line("report.zip", DownloadState::Failed, 4096),
            "Failed report.zip"
        );
    }

    /// A folder for one test case, named after the case and the process, so
    /// parallel tests never share a `report (2).zip`.
    fn temp_folder(kind: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ironsurf-downloads-{kind}-{}-{}",
            std::process::id(),
            unique_suffix()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A per-call suffix, since two tests of the same kind may run in one
    /// process at the same instant.
    fn unique_suffix() -> u64 {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEEN: AtomicU64 = AtomicU64::new(0);
        SEEN.fetch_add(1, Ordering::SeqCst)
    }

    // "Save page" (task: save-page selector): the dialog's proposed name —
    // the URL's own file when it has one, else the title wearing `.html`.
    #[test]
    fn save_page_name_prefers_the_url_file_then_the_title() {
        assert_eq!(save_page_name("http://example.com/report.pdf", "Report"), "report.pdf");
        assert_eq!(save_page_name("http://example.com/", "Welcome!"), "Welcome!.html");
        // A titleless page (about:blank) still gets a legal name.
        assert_eq!(save_page_name("about:blank", ""), "page.html");
    }

    // A confirmed save name is one sanitized component or the fallback —
    // blank, dot, and traversal drafts never name a path.
    #[test]
    fn save_name_or_never_yields_a_path() {
        assert_eq!(save_name_or("  notes.txt ", "page.html"), "notes.txt");
        assert_eq!(save_name_or("..", "page.html"), "page.html");
        assert_eq!(save_name_or("", "page.html"), "page.html");
        assert_eq!(save_name_or("../../etc/passwd", "page.html"), "page.html");
    }
}
