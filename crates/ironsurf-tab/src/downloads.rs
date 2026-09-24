//! File downloads, tab side (task: file downloads) — pure: no thread, no I/O,
//! no clock. Everything here answers "is this top-level fetch a file, and what
//! should we call it?"; the runtime owns the worker thread, the queue and the
//! messages ([`ARCHITECTURE.md §File downloads`] says the tab decides *whether*,
//! the shell decides *where*).
//!
//! The tab never writes to disk. It fetches (its own cookies, UA, redirects,
//! cache, archive hooks) and ships the bytes to the shell as
//! `FromTab::DownloadStart` → `DownloadChunk`* → `DownloadEnd`, which the shell
//! turns into a file. A name coming out of here is a *suggestion* — sanitized
//! as best effort, still untrusted: the shell sanitizes again.

use ironsurf_common::{
    content_disposition_filename, filename_from_url, is_download_media_type,
    sanitize_file_name, DownloadTrigger, MAX_DOWNLOAD_BYTES, MAX_DOWNLOAD_CHUNK_BYTES,
};

/// The tab's download counter: ids are minted per process from 1 and echoed
/// back in every `DownloadChunk`/`DownloadEnd`, so the shell can key one row
/// per `(tab_id, id)` — ids are never global.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DownloadIds(u64);

impl DownloadIds {
    /// A counter whose next id is 1.
    pub const fn new() -> Self {
        Self(0)
    }

    /// The next download id for this process.
    pub fn next(&mut self) -> u64 {
        self.0 += 1;
        self.0
    }
}

/// Why a fetched top-level response is a file rather than a page — and what
/// the server or the clicked anchor wants to call it.
///
/// Precedence is `Disposition` → `MediaType` → `AnchorAttribute`:
///
/// * `Content-Disposition: attachment` decides whatever the media type says
///   (a server that wants its HTML saved gets its HTML saved);
/// * a media type no page is made of ([`is_download_media_type`]) decides for
///   everything the header did not (and the header said nothing about);
/// * a clicked `<a download>` is the last resort — it still saves a
///   `text/html` response, which is exactly what a site's "Download" button
///   pointing at markup needs;
///
/// `None` means "render it", i.e. this is an ordinary navigation.
///
/// `anchor_download` is `Some(..)` only when the clicked element carried the
/// `download` content attribute ([`blitz_dom::take_anchor_download]`): the
/// outer `Option` is "was it a `<a download>`?", the inner one is its value as
/// a name suggestion.
pub fn attachment_for(
    media_type: Option<&str>,
    content_disposition: Option<&str>,
    anchor_download: Option<Option<&str>>,
) -> Option<(DownloadTrigger, Option<String>)> {
    if let Some(disc) = content_disposition {
        // Everything before the first `;` is the disposition itself; the
        // parameters (`filename=`, `filename*=`, …) follow it.
        let kind = disc.split(';').next().unwrap_or_default().trim();
        if kind.eq_ignore_ascii_case("attachment") {
            return Some((
                DownloadTrigger::Disposition,
                content_disposition_filename(disc),
            ));
        }
    }
    if media_type.is_some_and(|kind| is_download_media_type(kind)) {
        return Some((DownloadTrigger::MediaType, None));
    }
    match anchor_download {
        // A bare `download` (or an empty value) saves without naming.
        Some(Some(name)) => Some((DownloadTrigger::AnchorAttribute, Some(name.to_owned()))),
        Some(None) => Some((DownloadTrigger::AnchorAttribute, None)),
        None => None,
    }
}

/// The name the tab *suggests* for a download: the server's or the anchor's
/// value, sanitized ([`sanitize_file_name`]) so this end never proposes
/// something path-shaped, falling back to the last path segment of the URL it
/// fetched ([`filename_from_url`]). `None` here means the shell names it —
/// which it does again, under its own rules, because a name from the network
/// or from a page's markup is untrusted input.
pub fn name_suggestion(hinted: Option<&str>, url: &str) -> Option<String> {
    hinted
        .and_then(|hint| sanitize_file_name(hint))
        .or_else(|| {
            filename_from_url(url).and_then(|name| sanitize_file_name(&name))
        })
}

/// Why a download of `len` bytes is refused **before a single byte is
/// buffered** (task: file downloads): the tab holds a whole response body in
/// RAM before it ships it, so a 4 GiB `.iso` link must not be allowed to grow
/// the process until it dies. `None` = the size is fine; `Some(reason)`
/// becomes the `DownloadEnd { error }` that settles the row — a refusal is
/// the last message for its id and no chunk was ever sent, so the shell's
/// counter stays at 0 bytes for a reason the user can read.
pub fn refusal_for(len: usize) -> Option<String> {
    (len as u64 > MAX_DOWNLOAD_BYTES).then(|| {
        format!(
            "{len} bytes is past the {} MiB download cap — not saved",
            MAX_DOWNLOAD_BYTES / (1024 * 1024)
        )
    })
}

/// Split a buffered body into the `DownloadChunk` payloads the shell accepts,
/// **taking the pieces out of `body`** (each chunk is `mem::take`n from the
/// front — never a copy of the whole), so a 512 MiB download never exists
/// twice in RAM. At most [`MAX_DOWNLOAD_CHUNK_BYTES`] each, so every frame the
/// tab writes stays under `MAX_FRAME_BYTES`. An empty body yields no chunk at
/// all (an empty file is still a completed download, settled by `DownloadEnd`).
pub fn into_chunks(body: &mut Vec<u8>) -> Vec<Vec<u8>> {
    let mut chunks = Vec::new();
    while !body.is_empty() {
        if body.len() <= MAX_DOWNLOAD_CHUNK_BYTES {
            chunks.push(std::mem::take(body));
            break;
        }
        // `split_off` keeps the head and returns the tail: the head leaves the
        // buffer, the tail stays in it, and no byte is copied.
        let tail = body.split_off(MAX_DOWNLOAD_CHUNK_BYTES);
        chunks.push(std::mem::take(body));
        *body = tail;
    }
    chunks
}

/// The `DownloadHint::media_type` to report: the response's `Content-Type`
/// with any `;param` tail and surrounding whitespace gone, `None` when the
/// server named no type. The shell shows it, never trusts it.
pub fn media_type_label(content_type: Option<&str>) -> Option<String> {
    let raw = content_type?;
    let kind = raw.split(';').next().unwrap_or_default().trim();
    (!kind.is_empty()).then(|| kind.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_start_at_one_and_never_repeat() {
        let mut ids = DownloadIds::new();
        assert_eq!(ids.next(), 1);
        assert_eq!(ids.next(), 2);
        assert_eq!(DownloadIds::new().next(), 1, "fresh process, fresh ids");
    }

    #[test]
    fn disposition_beats_media_type_beats_anchor() {
        // Disposition wins even for a media type that would render.
        assert_eq!(
            attachment_for(
                Some("text/html"),
                Some("attachment; filename=\"page.html\""),
                Some(Some("clicked.html")),
            ),
            Some((
                DownloadTrigger::Disposition,
                Some("page.html".to_owned())
            )),
        );
        // Media type wins over an anchor attribute, and names nothing.
        assert_eq!(
            attachment_for(Some("application/zip"), None, Some(Some("a.zip"))),
            Some((DownloadTrigger::MediaType, None)),
        );
        // The anchor is the last word, and only for media types that render.
        assert_eq!(
            attachment_for(Some("text/html"), None, Some(Some("report.html"))),
            Some((
                DownloadTrigger::AnchorAttribute,
                Some("report.html".to_owned())
            )),
        );
        assert_eq!(
            attachment_for(Some("text/html"), None, Some(None)),
            Some((DownloadTrigger::AnchorAttribute, None)),
            "a bare `download` saves without naming"
        );
    }

    #[test]
    fn pages_are_not_attachments() {
        for media in [Some("text/html"), Some("text/plain"), None] {
            assert_eq!(
                attachment_for(media, Some("inline; filename=\"x.png\""), None),
                None,
                "media {media:?}"
            );
        }
        assert_eq!(attachment_for(Some("text/html"), None, None), None);
        assert_eq!(attachment_for(None, None, None), None);
    }

    #[test]
    fn attachment_is_a_download_even_for_markup() {
        for disc in [
            "attachment",
            "  attachment; filename=\"x.zip\"",
            "ATTACHMENT; filename*=UTF-8''r%C3%A9.zip",
            "attachment; filename=\"bad\"; filename*=UTF-8''good.zip",
        ] {
            assert_eq!(
                attachment_for(Some("text/html"), Some(disc), None).map(|(t, _)| t),
                Some(DownloadTrigger::Disposition),
                "{disc:?}"
            );
        }
        assert_eq!(
            attachment_for(Some("text/html"), Some("attachment"), Some(Some("a.zip")))
                .unwrap()
                .1,
            None,
            "no filename= in the header: no suggestion from it"
        );
    }

    #[test]
    fn suggestions_are_sanitized_then_taken_from_the_url() {
        assert_eq!(
            name_suggestion(Some("report.zip"), "https://example.com/f/1").unwrap(),
            "report.zip"
        );
        assert_eq!(
            name_suggestion(Some("../../.ssh/x"), "https://example.com/f/1").unwrap(),
            "1",
            "a traversal-shaped suggestion loses its directories"
        );
        assert_eq!(
            name_suggestion(Some("a\\b"), "https://example.com/keepme.bin").unwrap(),
            "a",
            "a separator ends the useful part, exactly like the shared sanitizer"
        );
        assert_eq!(
            name_suggestion(None, "https://example.com/files/report.zip").unwrap(),
            "report.zip"
        );
        assert_eq!(
            name_suggestion(None, "https://example.com/files/report.zip%20final").unwrap(),
            "report.zip final",
            "percent-decoded: %20 is a space, not part of the name"
        );
        assert_eq!(name_suggestion(Some("  "), "https://example.com/"), None);
    }

    #[test]
    fn chunks_never_exceed_the_cap() {
        let mut empty = Vec::new();
        assert_eq!(into_chunks(&mut empty), Vec::<Vec<u8>>::new());
        let body: Vec<u8> = (0..(MAX_DOWNLOAD_CHUNK_BYTES * 2 + 7)).map(|i| i as u8).collect();
        let mut buffer = body.clone();
        let chunks = into_chunks(&mut buffer);
        assert_eq!(chunks.len(), 3, "two full chunks and a tail");
        assert!(
            chunks.iter().all(|c| c.len() <= MAX_DOWNLOAD_CHUNK_BYTES),
            "every chunk fits a frame: {:?}",
            chunks.iter().map(|c| c.len()).collect::<Vec<_>>()
        );
        assert!(buffer.is_empty(), "chunks are taken out, not copied");
        assert_eq!(
            chunks.iter().map(|c| c.len()).sum::<usize>(),
            body.len(),
            "no byte lost or duplicated"
        );
        assert!(chunks.concat() == body.to_vec(), "order preserved");
    }

    #[test]
    fn refusal_for_says_why_a_body_is_too_big() {
        assert_eq!(refusal_for(0), None);
        assert_eq!(refusal_for(MAX_DOWNLOAD_BYTES as usize), None);
        let reason = refusal_for(MAX_DOWNLOAD_BYTES as usize + 1).expect("over the cap");
        assert!(reason.contains("cap"), "the reason names a cap: {reason}");
        assert!(
            reason.contains("512"),
            "and says how big the cap is: {reason}"
        );
    }

    #[test]
    fn media_labels_lose_their_parameters() {
        assert_eq!(
            media_type_label(Some("application/zip; charset=binary")).as_deref(),
            Some("application/zip")
        );
        assert_eq!(media_type_label(Some("")).as_ref().map(String::as_str), None);
        assert_eq!(media_type_label(None), None);
    }
}
