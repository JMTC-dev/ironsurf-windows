//! AVI (RIFF) demuxer.
//!
//! AVI is two things: a `hdrl` header list that names the streams, and a
//! `movi` list that interleaves their chunks (`00dc`, `01db`, …). The index at
//! the end of the file (`idx1` or a `LIST idx `) is preferred when the writer
//! left one, because it also carries the keyframe flags; otherwise the chunks
//! are walked in order. Sample times come from the stream header's
//! `dwScale`/`dwRate`, which is the only timing information AVI has for
//! variable-frame-rate-free content (and AVI's constant-frame-rate assumption
//! is why this is not approximate).
//!
//! In practice AVI means MJPEG: intra-coded JPEG frames, so every sample is a
//! keyframe. H.264-in-AVI is rare and only plays if the file provides an
//! index with keyframe flags.

use crate::media::codec::VideoCodec;
use crate::media::demux::byte::{corrupt_msg, u32_at};
use crate::media::demux::{Container, MediaInfo, Sample};
use crate::media::MediaError;

pub struct ParsedAvi {
    pub info: MediaInfo,
    pub samples: Vec<Sample>,
}

/// Walk sibling RIFF chunks in `[at, end)`.
struct Chunks<'a> {
    data: &'a [u8],
    at: usize,
    end: usize,
}

impl<'a> Chunks<'a> {
    fn new(data: &'a [u8], at: usize, len: usize) -> Self {
        let end = at.saturating_add(len).min(data.len());
        Chunks { data, at, end }
    }
}

impl Iterator for Chunks<'_> {
    type Item = Result<Chunk, MediaError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.at + 8 > self.end {
            return None;
        }
        let kind = self.data[self.at..self.at + 4].try_into().expect("4 bytes");
        let size = match u32_at(self.data, self.at + 4) {
            Some(size) => usize::try_from(size).ok()?,
            None => {
                self.at = self.end;
                return Some(Err(corrupt_msg("truncated RIFF chunk header")));
            }
        };
        let body = self.at + 8;
        if body.checked_add(size).is_none_or(|stop| stop > self.end) {
            self.at = self.end;
            return Some(Err(corrupt_msg("RIFF chunk runs past its parent")));
        }
        // RIFF pads chunks to an even length.
        self.at = body + size + (size % 2);
        Some(Ok(Chunk { kind, body, size }))
    }
}

#[derive(Clone, Copy, Debug)]
struct Chunk {
    kind: [u8; 4],
    body: usize,
    size: usize,
}

/// Which LIST a `LIST` chunk holds.
fn list_type(data: &[u8], chunk: &Chunk) -> Option<[u8; 4]> {
    data.get(chunk.body..chunk.body + 4)
        .and_then(|bytes| bytes.try_into().ok())
}

/// Children of a `LIST` chunk (skipping the fourcc).
fn list_children<'a>(data: &'a [u8], chunk: &Chunk) -> Chunks<'a> {
    Chunks::new(data, chunk.body + 4, chunk.size.saturating_sub(4))
}

/// The video stream header (`strh`) fields we need.
#[derive(Clone, Debug, Default)]
struct StreamHeader {
    /// `'vids'` for video, `'auds'` for audio.
    kind: [u8; 4],
    /// Codec fourcc (`MJPG`, `H264`, `XVID`, …).
    handler: [u8; 4],
    /// Samples per time unit and time units per second: time = n * scale / rate.
    scale: u32,
    rate: u32,
    start: u32,
    length: u32,
    width: u32,
    height: u32,
    stream_index: usize,
}

/// Parse an AVI file into its first video track.
pub fn parse(file: &[u8]) -> Result<ParsedAvi, MediaError> {
    if !looks_like_avi(file) {
        return Err(MediaError::UnsupportedContainer {
            hint: "not a RIFF/AVI file".to_string(),
        });
    }
    let riff_size = u32_at(file, 4).and_then(|size| usize::try_from(size).ok()).unwrap_or(0);
    let end = riff_size.saturating_add(8).min(file.len());

    let mut streams: Vec<StreamHeader> = Vec::new();
    let mut audio: Option<String> = None;
    let mut movi: Option<Chunk> = None;
    let mut index: Option<Chunk> = None;

    for item in Chunks::new(file, 12, end.saturating_sub(12)) {
        let chunk = item?;
        if chunk.kind != *b"LIST" {
            continue;
        }
        match list_type(file, &chunk) {
            Some(kind) if kind == *b"hdrl" => {
                streams = read_hdrl(file, &chunk);
                if audio.is_none() {
                    audio = streams
                        .iter()
                        .find(|stream| stream.kind == *b"auds")
                        .map(|stream| {
                            let fourcc = String::from_utf8_lossy(&stream.handler).trim_end().to_string();
                            format!("AVI audio ({fourcc})")
                        });
                }
            }
            Some(kind) if kind == *b"movi" => movi = Some(chunk),
            Some(kind) if kind == *b"idx " => index = Some(chunk),
            _ => {}
        }
    }

    let video = streams
        .iter()
        .find(|stream| stream.kind == *b"vids")
        .ok_or(MediaError::NoVideoTrack)?;
    let codec = VideoCodec::from_avi_fourcc(u32::from_le_bytes(video.handler));
    if !codec.is_supported() {
        return Err(MediaError::UnsupportedCodec {
            codec: codec.label(),
        });
    }

    let movi = movi.ok_or_else(|| corrupt_msg("AVI has no movi list"))?;
    // `idx1`/`idx0` offsets are relative to the start of the `movi` list data.
    let movi_base = movi.body;
    let mut samples = match index {
        Some(index) => read_index(file, &index, video.stream_index, movi_base, video)?,
        None => read_movi_chunks(file, &movi, video)?,
    };
    if samples.is_empty() {
        return Err(MediaError::Corrupt {
            detail: "AVI movi list holds no chunks for the video stream".to_string(),
        });
    }
    // MJPEG frames are intra-coded; without an index there is nothing else
    // claiming a frame is inter-coded, so every sample is a restart point.
    if codec == VideoCodec::Mjpeg {
        for sample in &mut samples {
            sample.key = true;
        }
    }

    let duration = if video.rate > 0 && video.scale > 0 {
        let frames = if video.length > 0 {
            video.length
        } else {
            u32::try_from(samples.len()).unwrap_or(0)
        };
        let last = (f64::from(video.start) + f64::from(frames) * f64::from(video.scale) / f64::from(video.rate))
            .max(0.0);
        Some(last)
    } else {
        samples.last().map(|sample| sample.pts)
    };

    Ok(ParsedAvi {
        info: MediaInfo {
            container: Container::Avi,
            duration,
            width: video.width,
            height: video.height,
            codec: codec.clone(),
            codec_private: None,
            audio,
            sample_count: samples.len(),
        },
        samples,
    })
}

/// Read the `strh` headers out of the `hdrl` list.
fn read_hdrl(file: &[u8], hdrl: &Chunk) -> Vec<StreamHeader> {
    let mut streams = Vec::new();
    for item in list_children(file, hdrl) {
        let Ok(chunk) = item else { break };
        if chunk.kind != *b"LIST" || list_type(file, &chunk) != Some(*b"strl") {
            continue;
        }
        let mut header = StreamHeader::default();
        header.stream_index = streams.len();
        for item in list_children(file, &chunk) {
            let Ok(child) = item else { break };
            if child.kind == *b"strh" {
                // AVISTREAMHEADER: fcc_type(0) fcc_handler(4) flags(8)
                // priority+language(12) initial_frames(16) scale(20) rate(24)
                // start(28) length(32) buffer(36) quality(40) sample_size(44)
                // frame rect: left(48) top(52) right(56) bottom(60).
                let read = |offset: usize| u32_at(file, child.body + offset);
                header.kind = file
                    .get(child.body..child.body + 4)
                    .and_then(|bytes| bytes.try_into().ok())
                    .unwrap_or(*b"????");
                header.handler = file
                    .get(child.body + 4..child.body + 8)
                    .and_then(|bytes| bytes.try_into().ok())
                    .unwrap_or(*b"????");
                header.scale = read(20).unwrap_or(1);
                header.rate = read(24).unwrap_or(0);
                header.start = read(28).unwrap_or(0);
                header.length = read(32).unwrap_or(0);
                // The frame rectangle holds the display size in pixels.
                header.width = read(56).unwrap_or(0);
                header.height = read(60).unwrap_or(0);
            } else if child.kind == *b"strf" {
                // For video this is a BITMAPINFOHEADER: biSize(4) biWidth(4)
                // biHeight(4) biPlanes(2) biBitCount(2) biCompression(4).
                if let (Some(w), Some(h)) = (u32_at(file, child.body + 4), u32_at(file, child.body + 8)) {
                    if header.width == 0 || header.height == 0 {
                        header.width = w;
                        // biHeight is negative for a top-down DIB; read as an
                        // unsigned value it is still the height we want.
                        header.height = h;
                    }
                }
            }
        }
        streams.push(header);
    }
    streams
}

/// The `idx1`/`idx0` index: id, flags, offset, size per entry.
fn read_index(
    file: &[u8],
    index: &Chunk,
    stream_index: usize,
    movi_base: usize,
    video: &StreamHeader,
) -> Result<Vec<Sample>, MediaError> {
    // A `LIST idx ` wraps its entries in a chunk whose fourcc names the
    // stream (`idx0`, `ix00`, …); a bare `idx1` is a flat array of 16-byte
    // entries at the end of the file.
    let entries_start = if index.kind == *b"LIST" {
        index.body + 8
    } else {
        index.body
    };
    let entry_size = 16usize;
    let count = (index.body + index.size).saturating_sub(entries_start) / entry_size;
    let mut samples = Vec::new();
    for entry in 0..count {
        let at = entries_start + entry * entry_size;
        let Some(id) = file.get(at..at + 4) else { break };
        let Some(this_stream) = chunk_belongs_to(id, stream_index) else {
            continue;
        };
        if !this_stream {
            continue;
        }
        let flags = u32_at(file, at + 4).unwrap_or(0);
        let offset = u32_at(file, at + 8).unwrap_or(0);
        let size = u32_at(file, at + 12).unwrap_or(0);
        let data_at = movi_base
            .checked_add(usize::try_from(offset).unwrap_or(usize::MAX))
            .filter(|offset| *offset >= movi_base)
            .and_then(|offset| offset.checked_add(8))
            .ok_or_else(|| corrupt_msg("AVI index offset is out of range"))?;
        if data_at.checked_add(usize::try_from(size).unwrap_or(usize::MAX)).is_none_or(|stop| stop > file.len()) {
            return Err(corrupt_msg("AVI index points past the end of the file"));
        }
        samples.push(Sample {
            offset: data_at,
            len: usize::try_from(size).unwrap_or(0),
            pts: sample_time(video, entry)?,
            // AVIF_KEYFRAME
            key: flags & 0x10 != 0,
        });
    }
    Ok(samples)
}

/// Walk `movi` chunks in file order, keeping the video stream's data chunks.
fn read_movi_chunks(file: &[u8], movi: &Chunk, video: &StreamHeader) -> Result<Vec<Sample>, MediaError> {
    let mut samples = Vec::new();
    let mut ordinal = 0usize;
    // `movi` is a LIST: its chunk children start after the list fourcc.
    for item in list_children(file, movi) {
        let chunk = item?;
        // `LIST` wrappers appear in some files ("odml" etc.); unwrap once.
        if chunk.kind == *b"LIST" {
            for inner in Chunks::new(file, chunk.body + 4, chunk.size.saturating_sub(4)) {
                let Ok(inner) = inner else { break };
                if let Some(true) = chunk_belongs_to(&inner.kind, video.stream_index) {
                    push_sample(file, &inner, video, ordinal, &mut samples)?;
                    ordinal += 1;
                }
            }
            continue;
        }
        if chunk_belongs_to(&chunk.kind, video.stream_index) == Some(true) {
            push_sample(file, &chunk, video, ordinal, &mut samples)?;
            ordinal += 1;
        }
    }
    Ok(samples)
}

fn push_sample(
    file: &[u8],
    chunk: &Chunk,
    video: &StreamHeader,
    ordinal: usize,
    samples: &mut Vec<Sample>,
) -> Result<(), MediaError> {
    if chunk.body + chunk.size > file.len() {
        return Err(corrupt_msg("movi chunk runs past the end of the file"));
    }
    samples.push(Sample {
        offset: chunk.body,
        len: chunk.size,
        pts: sample_time(video, ordinal)?,
        key: true,
    });
    Ok(())
}

/// `Some(true)` when a chunk id (`00dc`, `01db`, …) belongs to `stream`.
/// `Some(false)` when it is another stream's data chunk, `None` when it is not
/// a stream chunk at all (`movi`, `rec `, padding, …).
fn chunk_belongs_to(id: &[u8], stream: usize) -> Option<bool> {
    let (first, second) = (id.first()?, id.get(1)?);
    let hex = |n: u8| -> Option<u8> {
        match n {
            b'0'..=b'9' => Some(n - b'0'),
            b'a'..=b'f' => Some(n - b'a' + 10),
            b'A'..=b'F' => Some(n - b'A' + 10),
            _ => None,
        }
    };
    let (high, low) = (hex(*first)?, hex(*second)?);
    let digits = high * 16 + low;
    // The last two bytes are the chunk type: dc/db (video), wb (audio),
    // and the other codes AVI muxers emit.
    if !matches!(id.get(2..4)?, b"dc" | b"db" | b"wb" | b"dj" | b"cj" | b"pc" | b"tc" | b"px") {
        return None;
    }
    Some(usize::from(digits) == stream)
}

/// Presentation time of sample `n`: AVI's `dwScale`/`dwRate` plus its start.
fn sample_time(video: &StreamHeader, n: usize) -> Result<f64, MediaError> {
    if video.rate == 0 || video.scale == 0 {
        return Err(corrupt_msg(
            "AVI video stream declares no frame rate (strh dwScale/dwRate)",
        ));
    }
    // `usize` has no lossless `From` for `f64`, but a frame index this large
    // is a corrupt file, and clamping beats a panic.
    let n = if n > u32::MAX as usize { f64::from(u32::MAX) } else { n as f64 };
    Ok((f64::from(video.start) + n * f64::from(video.scale)) / f64::from(video.rate))
}

/// Whether these bytes are a RIFF AVI file.
pub fn looks_like_avi(bytes: &[u8]) -> bool {
    bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"AVI ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn put(out: &mut Vec<u8>, kind: &[u8; 4], body: &[u8]) {
        out.extend_from_slice(kind);
        out.extend_from_slice(&(body.len() as u32).to_be_bytes());
        out.extend_from_slice(body);
        if body.len() % 2 == 1 {
            out.push(0);
        }
    }

    fn list(out: &mut Vec<u8>, list_type: &[u8; 4], body: &[u8]) {
        let mut inner = Vec::new();
        inner.extend_from_slice(list_type);
        inner.extend_from_slice(body);
        put(out, b"LIST", &inner);
    }

    /// `list`, returning the RIFF record instead of appending into a buffer.
    fn listed(list_type: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        list(&mut out, list_type, body);
        out
    }

    fn strh(kind: &[u8; 4], handler: &[u8; 4], scale: u32, rate: u32, length: u32, w: u32, h: u32) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(kind);
        body.extend_from_slice(handler);
        body.extend_from_slice(&0u32.to_be_bytes()); // flags
        body.extend_from_slice(&[0u8; 4]); // priority + language
        body.extend_from_slice(&0u32.to_be_bytes()); // initial frames
        body.extend_from_slice(&scale.to_be_bytes());
        body.extend_from_slice(&rate.to_be_bytes());
        body.extend_from_slice(&0u32.to_be_bytes()); // start
        body.extend_from_slice(&length.to_be_bytes());
        body.extend_from_slice(&0u32.to_be_bytes()); // suggested buffer
        body.extend_from_slice(&0u32.to_be_bytes()); // quality
        body.extend_from_slice(&0u32.to_be_bytes()); // sample size
        body.extend_from_slice(&[0u8; 8]); // frame rect left/top
        body.extend_from_slice(&w.to_be_bytes()); // rect right
        body.extend_from_slice(&h.to_be_bytes()); // rect bottom
        body
    }

    /// An AVI with a video stream 0, an audio stream 1, and two video chunks.
    fn fixture(handler: &[u8; 4]) -> Vec<u8> {
        let video_stream = {
            let mut strl = Vec::new();
            // `strh`/`strf` are plain chunks inside the `strl` list.
            put(&mut strl, b"strh", &strh(b"vids", handler, 1, 30, 2, 160, 120));
            let mut strf = vec![0u8; 40];
            strf[0..4].copy_from_slice(&40u32.to_be_bytes());
            strf[4..8].copy_from_slice(&160u32.to_be_bytes());
            strf[8..12].copy_from_slice(&120u32.to_be_bytes());
            put(&mut strl, b"strf", &strf);
            listed(b"strl", &strl)
        };
        let audio_stream = {
            let mut strl = Vec::new();
            // An audio stream has no fourcc handler — four spaces it is.
            put(&mut strl, b"strh", &strh(b"auds", b"    ", 1, 44100, 0, 0, 0));
            listed(b"strl", &strl)
        };
        let hdrl = {
            let mut body = Vec::new();
            let mut avih = vec![0u8; 40];
            avih[0..4].copy_from_slice(&33_333u32.to_be_bytes()); // µs per frame
            avih[24..28].copy_from_slice(&2u32.to_be_bytes()); // total frames
            let mut avih_chunk = Vec::new();
            put(&mut avih_chunk, b"avih", &avih);
            body.extend_from_slice(&avih_chunk);
            body.extend_from_slice(&video_stream);
            body.extend_from_slice(&audio_stream);
            listed(b"hdrl", &body)
        };
        let movi = {
            let mut body = Vec::new();
            put(&mut body, b"00dc", b"FRAME-ONE");
            put(&mut body, b"01wb", b"SOUND");
            put(&mut body, b"00dc", b"FRAME-TWO");
            listed(b"movi", &body)
        };
        let mut file = Vec::new();
        file.extend_from_slice(b"RIFF");
        let size = (hdrl.len() + movi.len() + 4) as u32;
        file.extend_from_slice(&size.to_le_bytes());
        file.extend_from_slice(b"AVI ");
        file.extend_from_slice(&hdrl);
        file.extend_from_slice(&movi);
        file
    }

    #[test]
    fn movi_chunks_and_strh_timing_make_a_sample_table() {
        let file = fixture(b"MJPG");
        let parsed = parse(&file).expect("parses");
        assert_eq!(parsed.info.container, Container::Avi);
        assert_eq!(parsed.info.codec, VideoCodec::Mjpeg);
        assert_eq!((parsed.info.width, parsed.info.height), (160, 120));
        assert_eq!(parsed.info.sample_count, 2, "the audio chunk is not a video sample");
        assert_eq!(parsed.samples[0].pts, 0.0);
        assert!((parsed.samples[1].pts - 1.0 / 30.0).abs() < 1e-9);
        assert!(parsed.samples.iter().all(|sample| sample.key), "MJPEG is all keyframes");
        assert_eq!(
            &file[parsed.samples[1].offset..parsed.samples[1].offset + parsed.samples[1].len],
            b"FRAME-TWO"
        );
        // 2 frames at 30 fps ≈ 66 ms.
        assert!((parsed.info.duration.unwrap() - 0.0666).abs() < 0.01);
        assert!(parsed.info.audio.unwrap().starts_with("AVI audio"), "audio is named");
    }

    #[test]
    fn an_unsupported_handler_names_itself() {
        let file = fixture(b"XVID");
        // ParsedAvi holds non-Debug pieces, so take the error side directly.
        let err = parse(&file).err().expect("no MPEG-4 Part 2 decoder");
        assert_eq!(
            err,
            MediaError::UnsupportedCodec {
                codec: "MPEG-4 Part 2".to_string()
            }
        );
    }

    #[test]
    fn a_non_avi_file_is_refused() {
        assert!(matches!(parse(b"not an avi at all"), Err(MediaError::UnsupportedContainer { .. })));
    }

    #[test]
    fn stream_chunk_ids_are_classified() {
        assert_eq!(chunk_belongs_to(b"00dc", 0), Some(true));
        assert_eq!(chunk_belongs_to(b"00db", 0), Some(true));
        assert_eq!(chunk_belongs_to(b"01wb", 0), Some(false));
        assert_eq!(chunk_belongs_to(b"0adc", 10), Some(true));
        assert_eq!(chunk_belongs_to(b"LIST", 0), None);
        assert_eq!(chunk_belongs_to(b"rec ", 0), None);
    }

    #[test]
    fn sniffing_recognises_riff_avi() {
        assert!(looks_like_avi(b"RIFF\0\0\0\0AVI list"));
        assert!(!looks_like_avi(b"RIFF\0\0\0\0WEBP"));
        assert!(!looks_like_avi(b"MNG\r\n"));
    }

    #[test]
    fn missing_frame_rate_is_an_error_not_a_division_by_zero() {
        let mut header = StreamHeader::default();
        header.rate = 0;
        assert!(sample_time(&header, 3).is_err());
        header.rate = 30;
        header.scale = 0;
        assert!(sample_time(&header, 3).is_err());
    }

}
