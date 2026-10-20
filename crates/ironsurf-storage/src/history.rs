//! Browsing history (task: browsing history) — **shared across every
//! container**, one append-only `history.jsonl` at the data-root itself (not
//! under `profiles/`): a visit recorded in one container is visible from all
//! of them, which is exactly what the shell's History menu reads (the reopen
//! closed-tab stack is separate, in-memory session state).
//!
//! The file is one JSON object per line, oldest first; readers present it
//! newest first. Corrupt lines are skipped on read and rewritten away on the
//! next trim. Appends are best effort — a failed write loses one entry, never
//! the file.

use crate::{now_unix, StoreError};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Cap on remembered visits: past this the oldest lines are dropped on the
/// next append (rewrite-in-place, same as the net cache's bounded growth).
pub const MAX_HISTORY_ENTRIES: usize = 10_000;

/// One recorded navigation. `title` is the tab's page title **as the shell
/// knew it at navigation time** — usually still empty mid-load, so menus
/// fall back to the URL.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub url: String,
    #[serde(default)]
    pub title: Option<String>,
    pub at: i64,
}

/// The shared history log: `<root>/history.jsonl`.
#[derive(Clone, Debug)]
pub struct History {
    path: PathBuf,
}

impl History {
    /// Point at the data-root's `history.jsonl` (nothing is created until
    /// the first record).
    pub fn open(root: impl AsRef<Path>) -> Self {
        Self {
            path: root.as_ref().join("history.jsonl"),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append one navigation. A visit whose URL **and** title both equal the
    /// current last entry's is skipped (a reload or a re-render of the same
    /// page is not a new visit); anything else lands as a fresh line. Past
    /// [`MAX_HISTORY_ENTRIES`] the oldest entries are dropped.
    pub fn record(&self, url: &str, title: Option<&str>) -> Result<(), StoreError> {
        use std::io::Write;
        let lines = read_lines(&self.path)?;
        // Consecutive-duplicate skip: compare against the raw last line.
        if let Some(last) = lines.last() {
            if let Ok(entry) = serde_json::from_str::<HistoryEntry>(last) {
                let title_eq = match (entry.title.as_deref(), title) {
                    (Some(a), Some(b)) => a == b,
                    (None, None) => true,
                    _ => false,
                };
                if entry.url == url && title_eq {
                    return Ok(());
                }
            }
        }
        let entry = HistoryEntry {
            url: url.to_string(),
            title: title.map(str::to_string),
            at: now_unix(),
        };
        let mut line =
            serde_json::to_string(&entry).map_err(|e| StoreError::json(&self.path, e))?;
        line.push('\n');
        if lines.len() >= MAX_HISTORY_ENTRIES {
            // Trim: rewrite the file with the newest `MAX - 1` lines + this one.
            let keep = &lines[lines.len() - (MAX_HISTORY_ENTRIES - 1)..];
            let mut body = keep.join("\n");
            body.push('\n');
            body.push_str(&line);
            std::fs::write(&self.path, body.as_bytes())
                .map_err(|e| StoreError::io(&self.path, e))?;
            return Ok(());
        }
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(|e| StoreError::io(&self.path, e))?;
        f.write_all(line.as_bytes())
            .map_err(|e| StoreError::io(&self.path, e))?;
        Ok(())
    }

    /// Re-title the **newest** entry for `url` — the visit log's late arrival.
    /// A navigation is recorded the moment the address changes, and the page's
    /// own `<title>` only exists a moment later (the document has to be fetched
    /// and parsed first, and scripts may rename it afterwards), so the shell
    /// writes the visit and then corrects it here as the title lands. `None`/
    /// blank means the page has no title and any stale one carried over from
    /// the previous page is wiped, so an entry never shows a title its address
    /// never had. The newest match is the one this call is about: an older
    /// visit to the same address keeps the title it had. The file is only
    /// rewritten when a title actually changes — an unchanged title costs one
    /// read, so a page that renames itself every second does not hammer disk.
    pub fn set_latest_title(&self, url: &str, title: Option<&str>) -> Result<bool, StoreError> {
        let mut lines = read_lines(&self.path)?;
        let clean = title.map(str::trim).filter(|t| !t.is_empty()).map(str::to_string);
        for index in (0..lines.len()).rev() {
            let Ok(mut entry) = serde_json::from_str::<HistoryEntry>(&lines[index]) else {
                continue; // a corrupt line is not an entry, and not an owner
            };
            if entry.url != url {
                continue;
            }
            if entry.title == clean {
                return Ok(false);
            }
            entry.title = clean;
            lines[index] =
                serde_json::to_string(&entry).map_err(|e| StoreError::json(&self.path, e))?;
            let mut body = lines.join("\n");
            body.push('\n');
            std::fs::write(&self.path, body.as_bytes())
                .map_err(|e| StoreError::io(&self.path, e))?;
            return Ok(true);
        }
        Ok(false)
    }

    /// Every entry, **newest first**. Unparseable lines are skipped (they die
    /// on the next trim).
    pub fn entries(&self) -> Vec<HistoryEntry> {
        let mut out = Vec::new();
        for line in read_lines_ok(&self.path) {
            if let Ok(entry) = serde_json::from_str::<HistoryEntry>(line.trim_end()) {
                out.push(entry);
            }
        }
        out.reverse();
        out
    }

    /// Forget everything: the log file is removed (a missing file is not an
    /// error — clearing twice is fine). Returns how many entries were
    /// discarded, so the shell can say so in its log.
    pub fn clear(&self) -> Result<usize, StoreError> {
        let n = read_lines(&self.path)?.len();
        match std::fs::remove_file(&self.path) {
            Ok(()) => Ok(n),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(0),
            Err(err) => Err(StoreError::io(&self.path, err)),
        }
    }
}

fn read_lines(path: &Path) -> Result<Vec<String>, StoreError> {
    match std::fs::read_to_string(path) {
        Ok(body) => Ok(body
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(str::to_string)
            .collect()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(err) => Err(StoreError::io(path, err)),
    }
}

fn read_lines_ok(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(str::to_string)
        .collect()
}
