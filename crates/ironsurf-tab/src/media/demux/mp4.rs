//! ISO Base Media demuxer (`mp4`/`m4v`/`mov`/`3gp`).
//!
//! The sample table is the whole job: `stsd` says what the codec is and where
//! `mdat` starts, `stsz`/`stco`/`stsc` say where each sample's bytes are,
//! `stts`/`ctts` say when it is presentable, and `stss` says which samples may
//! start decoding from scratch (the keyframes a seek snaps to). Samples are
//! returned as ranges of the file, in **decode order**, which is what a
//! prediction-coded codec wants; presentation order comes from `pts`.
//!
//! Supported: progressive files with a `moov` (the `faststart` layout, where
//! `moov` precedes `mdat`, works as well as the trailing one). Fragmented MP4
//! (`moof`/`frag` — the DASH/HLS streaming layout) is refused with a readable
//! error rather than half-parsed.

use crate::media::codec::VideoCodec;
use crate::media::demux::byte::{corrupt_msg, fixed16_16, u16, u32_at, uint};
use crate::media::demux::{MediaInfo, Sample};
use crate::media::MediaError;

/// A `moov`-carrying MP4 parsed down to what playback needs.
#[derive(Debug)]
pub struct ParsedMp4 {
    pub info: MediaInfo,
    pub samples: Vec<Sample>,
}

/// Walk sibling boxes in `[at, end)`.
struct Boxes<'a> {
    data: &'a [u8],
    at: usize,
    end: usize,
}

impl<'a> Boxes<'a> {
    fn new(data: &'a [u8], at: usize, len: usize) -> Self {
        Boxes {
            data,
            at,
            end: at.saturating_add(len).min(data.len()),
        }
    }

    fn of(data: &'a [u8], body: (usize, usize)) -> Self {
        Boxes::new(data, body.0, body.1)
    }
}

/// One box: its fourcc, body offset and body length.
#[derive(Clone, Copy, Debug)]
struct BoxRef {
    kind: [u8; 4],
    body: usize,
    len: usize,
}

impl Iterator for Boxes<'_> {
    type Item = Result<BoxRef, MediaError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.at + 8 > self.end {
            return None;
        }
        let raw_size = u32_at(self.data, self.at)?;
        let mut header = 8usize;
        let mut size = usize::try_from(raw_size).unwrap_or(usize::MAX);
        if size == 1 {
            // 64-bit `largesize`.
            let wide = uint(self.data, self.at + 8, 8)?;
            header = 16;
            size = usize::try_from(wide).unwrap_or(usize::MAX);
        } else if size == 0 {
            // Box runs to the end of its parent.
            size = self.end - self.at;
        }
        if size < header || self.at + size > self.end {
            // Truncated or lying: stop rather than walk into garbage.
            self.at = self.end;
            return Some(Err(corrupt_msg("box size runs past the end of its parent")));
        }
        // ISO BMFF: size(4) then type(4) — the kind is AFTER the size field.
        let kind = self.data[self.at + 4..self.at + 8].try_into().expect("4 bytes");
        let body = self.at + header;
        self.at += size;
        Some(Ok(BoxRef {
            kind,
            body,
            len: size - header,
        }))
    }
}

/// Find the first top-level box of a kind.
fn first<'a>(data: &'a [u8], at: usize, len: usize, kind: &[u8; 4]) -> Option<BoxRef> {
    Boxes::new(data, at, len)
        .next()
        .and_then(|item| item.ok())
        .and_then(|found| (found.kind == *kind).then_some(found))
}

fn find_all(data: &[u8], at: usize, len: usize, kind: &[u8; 4]) -> Vec<BoxRef> {
    let mut found = Vec::new();
    for item in Boxes::new(data, at, len) {
        let Ok(found_box) = item else { break };
        if &found_box.kind == kind {
            found.push(found_box);
        }
    }
    found
}

fn find_child(data: &[u8], parent: BoxRef, kind: &[u8; 4]) -> Option<BoxRef> {
    let mut boxes = Boxes::of(data, (parent.body, parent.len));
    boxes.next()?;
    let first = {
        // `next()` already advanced; re-walk to keep it simple and cheap.
        let mut iter = Boxes::of(data, (parent.body, parent.len));
        loop {
            match iter.next() {
                Some(Ok(found)) => {
                    if &found.kind == kind {
                        break Some(found);
                    }
                }
                _ => break None,
            }
        }
    };
    first
}

/// Parse a progressive MP4: pick the video track and build its sample table.
pub fn parse(file: &[u8]) -> Result<ParsedMp4, MediaError> {
    let moov = top_box(file, b"moov").ok_or_else(|| {
        let fragmented = top_box(file, b"moof").is_some();
        MediaError::UnsupportedContainer {
            hint: if fragmented {
                "fragmented MP4 (DASH/HLS `moof` segments) cannot be played from one file"
                    .to_string()
            } else {
                "no `moov` box — the file has no sample table".to_string()
            },
        }
    })?;

    let mvhd = find_child(file, moov, b"mvhd").ok_or_else(|| corrupt_msg("moov has no mvhd"))?;
    let movie_timescale = read_mvhd_timescale(file, mvhd).unwrap_or(0);
    let movie_duration = read_mvhd_duration(file, mvhd).unwrap_or(0.0);

    let mut video: Option<(TrackTable, bool)> = None;
    let mut audio: Option<String> = None;
    for trak in find_all(file, moov.body, moov.len, b"trak") {
        let Some(table) = TrackTable::read(file, trak) else {
            continue;
        };
        match table.handler {
            Handler::Video if video.is_none() => {
                let playable = table.codec.is_supported();
                video = Some((table, playable));
            }
            Handler::Video => {}
            Handler::Audio => {
                if audio.is_none() {
                    audio = Some(table.codec.label())
                }
            }
            Handler::Other => {}
        }
    }

    // A non-decodable video track is still *the* track: the caller reports its
    // codec, so the page's `error` event names what is missing.
    let (table, playable) = video.ok_or(MediaError::NoVideoTrack)?;
    if !playable {
        return Err(MediaError::UnsupportedCodec {
            codec: table.codec.label(),
        });
    }

    let samples = table.samples(file)?;
    let duration = table
        .duration_seconds()
        .or(if movie_timescale > 0 && movie_duration > 0.0 {
            Some(movie_duration)
        } else {
            None
        })
        .or_else(|| samples.last().map(|sample| sample.pts));

    Ok(ParsedMp4 {
        info: MediaInfo {
            container: crate::media::demux::Container::Mp4,
            duration,
            width: table.width,
            height: table.height,
            codec: table.codec,
            codec_private: table.codec_private,
            audio,
            sample_count: samples.len(),
        },
        samples,
    })
}

/// The `ftyp`/`styp`/`moov`/`mdat`/`free`/`moof` level of the file.
fn top_box(file: &[u8], kind: &[u8; 4]) -> Option<BoxRef> {
    for item in Boxes::new(file, 0, file.len()) {
        let Ok(found) = item else { return None };
        if &found.kind == kind {
            return Some(found);
        }
    }
    None
}

fn read_mvhd_timescale(file: &[u8], mvhd: BoxRef) -> Option<u64> {
    // v0: version/flags(4) ctime(4) mtime(4) timescale(4) duration(4)
    // v1: version/flags(4) ctime(8) mtime(8) timescale(4) duration(8)
    let version = file.get(mvhd.body)?;
    if *version == 1 {
        uint(file, mvhd.body + 20, 4)
    } else {
        uint(file, mvhd.body + 12, 4)
    }
}

fn read_mvhd_duration(file: &[u8], mvhd: BoxRef) -> Option<f64> {
    let version = *file.get(mvhd.body)?;
    let (timescale, duration) = if version == 1 {
        (
            uint(file, mvhd.body + 20, 4)?,
            uint(file, mvhd.body + 24, 8)?,
        )
    } else {
        (
            uint(file, mvhd.body + 12, 4)?,
            uint(file, mvhd.body + 16, 4)?,
        )
    };
    (timescale > 0).then(|| duration as f64 / timescale as f64)
}

/// Which kind of track a `hdlr` declares.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Handler {
    Video,
    Audio,
    Other,
}

/// The sample table of one `trak`.
struct TrackTable {
    handler: Handler,
    codec: VideoCodec,
    codec_private: Option<Vec<u8>>,
    width: u32,
    height: u32,
    timescale: u64,
    duration_units: u64,
    sample_sizes: Vec<u32>,
    chunk_offsets: Vec<u64>,
    /// Samples per chunk, expanded one entry per chunk.
    samples_per_chunk: Vec<u32>,
    /// Decode timestamps in media timescale, one per sample.
    dts: Vec<u64>,
    /// Composition (presentation) offsets, one per sample.
    cts_offsets: Vec<u64>,
    sync_samples: Option<Vec<u32>>,
    entry_count: usize,
}

impl TrackTable {
    fn read(file: &[u8], trak: BoxRef) -> Option<Self> {
        let mdia = find_child(file, trak, b"mdia")?;
        let mdhd = find_child(file, mdia, b"mdhd")?;
        let hdlr = find_child(file, mdia, b"hdlr")?;
        let minf = find_child(file, mdia, b"minf")?;
        let stbl = find_child(file, minf, b"stbl")?;

        let (timescale, duration_units, handler) = {
            let version = *file.get(mdhd.body)?;
            let (timescale, duration) = if version == 1 {
                (
                    uint(file, mdhd.body + 20, 4)?,
                    uint(file, mdhd.body + 24, 8)?,
                )
            } else {
                (
                    uint(file, mdhd.body + 12, 4)?,
                    u64::from(u32_at(file, mdhd.body + 16)?),
                )
            };
            // hdlr: version/flags(4) predefined(4) handler_type(4) …
            let mut handler_bytes = [0u8; 4];
            handler_bytes.copy_from_slice(file.get(hdlr.body + 8..hdlr.body + 12)?);
            let handler = match &handler_bytes {
                b"vide" => Handler::Video,
                b"soun" => Handler::Audio,
                _ => Handler::Other,
            };
            (timescale, duration, handler)
        };

        let stsd = find_child(file, stbl, b"stsd")?;
        let (codec, codec_private, entry_width, entry_height) = read_stsd(file, stsd)?;

        let tkhd = find_child(file, trak, b"tkhd");
        // tkhd's 16.16 display size wins when the track is not masked out; the
        // sample entry size is the fallback (and the coded size, for crops).
        let (track_width, track_height) = match tkhd {
            Some(tkhd) => {
                let version = *file.get(tkhd.body)?;
                let size_at = if version == 1 {
                    // v1: version/flags(4) ctime(8) mtime(8) track_id(4) reserved(4) duration(8)
                    tkhd.body + 4 + 8 + 8 + 4 + 4 + 8
                } else {
                    tkhd.body + 4 + 4 + 4 + 4 + 4
                };
                let w = fixed16_16(file, size_at)?.max(0.0) as u32;
                let h = fixed16_16(file, size_at + 4)?.max(0.0) as u32;
                (w, h)
            }
            None => (0, 0),
        };

        let sample_sizes = read_stsz(file, find_child(file, stbl, b"stsz")?).ok()?;
        let chunk_offsets = read_stco_or_co64(file, stbl).unwrap_or_default();
        let samples_per_chunk =
            read_stsc(file, find_child(file, stbl, b"stsc")?, chunk_offsets.len()).ok()?;
        let dts = read_stts(file, find_child(file, stbl, b"stts")?, sample_sizes.len()).ok()?;
        let cts_offsets = read_ctts(file, find_child(file, stbl, b"ctts"), sample_sizes.len());
        let sync_samples = find_child(file, stbl, b"stss").and_then(|stss| read_stss(file, stss));

        Some(TrackTable {
            handler,
            codec,
            codec_private,
            width: track_width.max(entry_width),
            height: track_height.max(entry_height),
            timescale,
            duration_units,
            sample_sizes,
            chunk_offsets,
            samples_per_chunk,
            dts,
            cts_offsets,
            sync_samples,
            entry_count: 1,
        })
    }

    fn duration_seconds(&self) -> Option<f64> {
        (self.timescale > 0 && self.duration_units > 0)
            .then(|| self.duration_units as f64 / self.timescale as f64)
    }

    /// The samples, in decode (file) order, each a range of the file plus its
    /// presentation time.
    fn samples(&self, file: &[u8]) -> Result<Vec<Sample>, MediaError> {
        let count = self.sample_sizes.len();
        if count == 0 {
            return Err(MediaError::NoVideoTrack);
        }
        if self.chunk_offsets.is_empty() {
            return Err(corrupt_msg("sample table has no chunk offsets (stco/co64)"));
        }

        let mut samples = Vec::with_capacity(count);
        let mut sample = 0usize;
        let key_samples = self.sync_samples.as_deref();
        let mut key_index = 0usize;
        for (chunk, &chunk_offset) in self.chunk_offsets.iter().enumerate() {
            let per_chunk = usize::try_from(
                self.samples_per_chunk
                    .get(chunk)
                    .copied()
                    .unwrap_or(1)
                    .max(1),
            )
            .unwrap_or(1);
            let mut within = 0usize;
            let mut offset = usize::try_from(chunk_offset).unwrap_or(usize::MAX);
            while within < per_chunk && sample < count {
                let len = usize::try_from(self.sample_sizes[sample]).unwrap_or(usize::MAX);
                if offset.checked_add(len).is_none_or(|end| end > file.len()) {
                    return Err(corrupt_msg(format!(
                        "sample {sample} ({len} B at {offset}) runs past the end of the file"
                    )));
                }
                let pts = self.pts(sample)?;
                // `stss` lists 1-based sync sample numbers in increasing order.
                let key = match key_samples {
                    None => true,
                    Some(list) => {
                        while key_index < list.len() && usize::try_from(list[key_index]).unwrap_or(0) < sample + 1 {
                            key_index += 1;
                        }
                        key_index < list.len()
                            && usize::try_from(list[key_index]).unwrap_or(0) == sample + 1
                    }
                };
                samples.push(Sample {
                    offset,
                    len,
                    pts,
                    key,
                });
                offset += len;
                within += 1;
                sample += 1;
            }
        }
        if samples.len() != count {
            return Err(corrupt_msg(format!(
                "sample table describes {count} samples but the chunks only hold {}",
                samples.len()
            )));
        }
        Ok(samples)
    }

    fn pts(&self, index: usize) -> Result<f64, MediaError> {
        if self.timescale == 0 {
            return Err(corrupt_msg("track timescale (mdhd) is zero"));
        }
        let dts = self.dts.get(index).copied().unwrap_or(0);
        let offset = self.cts_offsets.get(index).copied().unwrap_or(0);
        Ok((dts + offset) as f64 / self.timescale as f64)
    }
}

/// `stsd`: the first sample description names the codec; visual sample entries
/// carry the width/height and the codec-private box (`avcC`, `hvcC`, …).
fn read_stsd(file: &[u8], stsd: BoxRef) -> Option<(VideoCodec, Option<Vec<u8>>, u32, u32)> {
    // version/flags(4) entry_count(4), then entries.
    let entries = Boxes::new(file, stsd.body + 8, stsd.len.saturating_sub(8));
    for item in entries {
        let Ok(entry) = item else { break };
        let codec = VideoCodec::from_isom_fourcc(&entry.kind);
        // Visual sample entries are 78 bytes of header before child boxes;
        // audio entries (mp4a etc.) are shorter and have no width/height.
        let (width, height) = match (u16(file, entry.body + 24), u16(file, entry.body + 26)) {
            (Some(w), Some(h)) => (w, h),
            _ => (0, 0),
        };
        let codec_private = if entry.len > 78 {
            let children = Boxes::new(file, entry.body + 78, entry.len - 78);
            let mut private = None;
            for child in children {
                let Ok(child) = child else { break };
                if matches!(&child.kind, b"avcC" | b"hvcC" | b"colr") && private.is_none() {
                    if &child.kind == b"avcC" {
                        private = file.get(child.body..child.body + child.len).map(|s| s.to_vec());
                    }
                }
            }
            private
        } else {
            None
        };
        return Some((codec, codec_private, width, height));
    }
    None
}

/// `stsz`: sample sizes (one default, or one per sample).
fn read_stsz(file: &[u8], stsz: BoxRef) -> Result<Vec<u32>, MediaError> {
    let default_size = u32_at(file, stsz.body + 4).ok_or_else(|| corrupt_msg("stsz truncated"))?;
    let count = usize::try_from(u32_at(file, stsz.body + 8).ok_or_else(|| corrupt_msg("stsz truncated"))?)
        .map_err(|_| corrupt_msg("stsz sample count is absurd"))?;
    if count == 0 {
        return Err(corrupt_msg("stsz declares no samples"));
    }
    // A sample bigger than a whole file is a lie about a lie.
    const MAX_SAMPLE: u32 = 1 << 28;
    if default_size > MAX_SAMPLE {
        return Err(corrupt_msg("stsz declares an absurdly large sample"));
    }
    if default_size != 0 {
        return Ok(vec![default_size; count]);
    }
    let mut sizes = Vec::with_capacity(count);
    for index in 0..count {
        let size = u32_at(file, stsz.body + 12 + index * 4).ok_or_else(|| {
            corrupt_msg("stsz entry list is shorter than the sample count")
        })?;
        if size > MAX_SAMPLE {
            return Err(corrupt_msg("stsz declares an absurdly large sample"));
        }
        sizes.push(size);
    }
    Ok(sizes)
}

/// `stsc`: expanded into one "samples in this chunk" entry per chunk.
fn read_stsc(file: &[u8], stsc: BoxRef, chunk_count: usize) -> Result<Vec<u32>, MediaError> {
    let entries = usize::try_from(u32_at(file, stsc.body + 4).ok_or_else(|| corrupt_msg("stsc truncated"))?)
        .map_err(|_| corrupt_msg("stsc entry count is absurd"))?;
    if entries == 0 {
        return Ok(vec![1; chunk_count.max(1)]);
    }
    let mut parsed: Vec<(u32, u32)> = Vec::with_capacity(entries);
    for index in 0..entries {
        let at = stsc.body + 8 + index * 12;
        let first_chunk = u32_at(file, at).ok_or_else(|| corrupt_msg("stsc entry truncated"))?;
        let per_chunk = u32_at(file, at + 4).ok_or_else(|| corrupt_msg("stsc entry truncated"))?;
        parsed.push((first_chunk, per_chunk.max(1)));
    }
    let chunks = chunk_count.max(1).min(1 << 20);
    let mut expanded = vec![1u32; chunks];
    for (entry, &(first_chunk, per_chunk)) in parsed.iter().enumerate() {
        let start = usize::try_from(first_chunk.saturating_sub(1)).unwrap_or(usize::MAX);
        if start >= chunks {
            continue;
        }
        let stop = if entry + 1 < parsed.len() {
            usize::try_from(parsed[entry + 1].0.saturating_sub(1)).unwrap_or(chunks).min(chunks)
        } else {
            chunks
        };
        for slot in expanded.iter_mut().take(stop).skip(start) {
            *slot = per_chunk;
        }
    }
    Ok(expanded)
}

/// `stts`: expanded into one decode timestamp per sample.
fn read_stts(file: &[u8], stts: BoxRef, sample_count: usize) -> Result<Vec<u64>, MediaError> {
    let entries = usize::try_from(u32_at(file, stts.body + 4).ok_or_else(|| corrupt_msg("stts truncated"))?)
        .map_err(|_| corrupt_msg("stts entry count is absurd"))?;
    if entries == 0 {
        return Ok(vec![0; sample_count]);
    }
    let mut dts = Vec::with_capacity(sample_count);
    let mut cursor = stts.body + 8;
    let mut stamp: u64 = 0;
    for _ in 0..entries {
        let count = usize::try_from(u32_at(file, cursor).ok_or_else(|| corrupt_msg("stts entry truncated"))?)
            .map_err(|_| corrupt_msg("stts sample count is absurd"))?;
        let delta = uint(file, cursor + 4, 4).ok_or_else(|| corrupt_msg("stts entry truncated"))?;
        cursor += 8;
        for _ in 0..count {
            if dts.len() >= sample_count {
                break;
            }
            dts.push(stamp);
            stamp = stamp.saturating_add(delta);
        }
        if dts.len() >= sample_count {
            break;
        }
    }
    dts.resize(sample_count, stamp);
    Ok(dts)
}

/// `ctts`: composition-time offsets from decode time (B-frames).
fn read_ctts(file: &[u8], ctts: Option<BoxRef>, sample_count: usize) -> Vec<u64> {
    let Some(ctts) = ctts else {
        return vec![0; sample_count];
    };
    let version = file.get(ctts.body).copied().unwrap_or(0);
    let entries = match u32_at(file, ctts.body + 4) {
        Some(entries) => usize::try_from(entries).unwrap_or(0),
        None => return vec![0; sample_count],
    };
    let mut offsets = Vec::with_capacity(sample_count);
    let mut cursor = ctts.body + 8;
    for _ in 0..entries {
        let Some(count) = u32_at(file, cursor).map(|v| usize::try_from(v).unwrap_or(0)) else {
            break;
        };
        let offset = if version == 1 {
            // Signed (v1); negative offsets are legal and mean the sample is
            // presented before its decode stamp.
            uint(file, cursor + 4, 4)
                .map(|v| (v as i32).max(0) as u64)
                .unwrap_or(0)
        } else {
            uint(file, cursor + 4, 4).unwrap_or(0)
        };
        cursor += 8;
        for _ in 0..count {
            if offsets.len() >= sample_count {
                return offsets;
            }
            offsets.push(offset);
        }
    }
    offsets.resize(sample_count, 0);
    offsets
}

/// `stss`: the sync (keyframe) sample numbers, 1-based.
fn read_stss(file: &[u8], stss: BoxRef) -> Option<Vec<u32>> {
    let entries = usize::try_from(u32_at(file, stss.body + 4)?).ok()?;
    let mut list = Vec::with_capacity(entries.min(1 << 20));
    for index in 0..entries {
        list.push(u32_at(file, stss.body + 8 + index * 4)?);
    }
    Some(list)
}

/// `stco` (32-bit) or `co64` (64-bit) chunk offsets.
fn read_stco_or_co64(file: &[u8], stbl: BoxRef) -> Option<Vec<u64>> {
    let (box_, wide) = match find_child(file, stbl, b"stco") {
        Some(found) => (found, 4usize),
        None => (find_child(file, stbl, b"co64")?, 8),
    };
    let entries = usize::try_from(u32_at(file, box_.body + 4)?).ok()?;
    if entries > (1 << 24) {
        return None;
    }
    let mut offsets = Vec::with_capacity(entries);
    for index in 0..entries {
        offsets.push(uint(file, box_.body + 8 + index * wide, wide)?);
    }
    Some(offsets)
}

/// Whether these bytes look like ISO base media (`ftyp`/`styp`/`moov` first).
pub fn looks_like_mp4(bytes: &[u8]) -> bool {
    let Some(kind) = bytes.get(4..8) else {
        return false;
    };
    matches!(kind, b"ftyp" | b"styp" | b"moov" | b"free" | b"mdat" | b"skip" | b"wide")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A muxer for tests: one video track, N equal samples, single chunk.
    /// Deliberately minimal — enough of `moov` for the demuxer, nothing more.
    fn build_mp4(fourcc: &[u8; 4], sample_len: u32, sample_count: usize, timescale: u32, delta: u32, key_first_only: bool) -> Vec<u8> {
        fn put(body: &mut Vec<u8>, kind: &[u8; 4], payload: &[u8]) {
            body.extend_from_slice(&(payload.len() as u32 + 8).to_be_bytes());
            body.extend_from_slice(kind);
            body.extend_from_slice(payload);
        }
        let avcc: Vec<u8> = vec![
            1, 100, 0, 31, 0xff, 0xe1, 0x00, 0x04, 0x67, 0x42, 0x00, 0x1f, 0x01, 0x00, 0x03, 0x68,
            0xce, 0x38,
        ];
        // VisualSampleEntry: 78 B header + avcC child.
        let mut entry_body = vec![0u8; 78];
        entry_body[24..26].copy_from_slice(&320u16.to_be_bytes());
        entry_body[26..28].copy_from_slice(&180u16.to_be_bytes());
        entry_body[64] = b'f'; // frame_count-ish filler, harmless
        let mut entry = entry_body;
        put(&mut entry, b"avcC", &avcc);
        let mut stsd = vec![0u8; 8];
        stsd[3] = 1; // entry_count = 1
        put(&mut stsd, fourcc, &entry);

        let mut stts = vec![0, 0, 0, 0];
        stts.extend_from_slice(&1u32.to_be_bytes());
        stts.extend_from_slice(&(sample_count as u32).to_be_bytes());
        stts.extend_from_slice(&delta.to_be_bytes());

        let mut stss = vec![0, 0, 0, 0];
        let keys: Vec<u32> = if key_first_only {
            vec![1]
        } else {
            (1..=sample_count as u32).collect()
        };
        stss.extend_from_slice(&(keys.len() as u32).to_be_bytes());
        for key in &keys {
            stss.extend_from_slice(&key.to_be_bytes());
        }

        let mut stsz = vec![0, 0, 0, 0];
        stsz.extend_from_slice(&sample_len.to_be_bytes());
        stsz.extend_from_slice(&(sample_count as u32).to_be_bytes());

        // mdat payload laid out after the whole moov; offsets patched below.
        let mut mdat_payload = vec![0u8; sample_len as usize * sample_count];
        for (i, byte) in mdat_payload.iter_mut().enumerate().take(sample_len as usize * sample_count) {
            *byte = (i / sample_len.max(1) as usize) as u8;
        }

        // Chunk layout: everything in one chunk.
        // One entry: first_chunk=1, samples_per_chunk=N, desc index 1 —
        // everything in one chunk. (The entry needs all three fields.)
        let mut stsc = vec![0, 0, 0, 0];
        stsc.extend_from_slice(&1u32.to_be_bytes());
        stsc.extend_from_slice(&1u32.to_be_bytes());
        stsc.extend_from_slice(&(sample_count as u32).to_be_bytes());
        stsc.extend_from_slice(&1u32.to_be_bytes());

        let mut file = Vec::new();
        put(&mut file, b"ftyp", b"isom\0\0\0\x02isom");
        // moov: mvhd + trak(tkhd, mdia(mdhd, hdlr, minf(stbl(...))))
        let mut mvhd = vec![0u8; 100];
        mvhd[12..16].copy_from_slice(&timescale.to_be_bytes());
        mvhd[16..20]
            .copy_from_slice(&((sample_count as u32) * delta).to_be_bytes());

        let mut mdhd = vec![0u8; 32];
        mdhd[12..16].copy_from_slice(&timescale.to_be_bytes());
        mdhd[16..20].copy_from_slice(&((sample_count as u32) * delta).to_be_bytes());
        let mut hdlr = vec![0u8; 17];
        hdlr[8..12].copy_from_slice(b"vide");

        let stbl = {
            let mut body = Vec::new();
            put(&mut body, b"stsd", &stsd);
            put(&mut body, b"stts", &stts);
            put(&mut body, b"stss", &stss);
            put(&mut body, b"stsz", &stsz);
            put(&mut body, b"stsc", &stsc);
            // stco: version/flags(4) + entry_count(4) + one placeholder
            // offset, patched once the length (and so `mdat`'s position) is
            // known. Skipping version/flags here used to make the patch land
            // past the box's end, scribbling over `mdat`'s size field.
            let mut stco = vec![0u8; 8];
            stco[4..8].copy_from_slice(&1u32.to_be_bytes());
            stco.extend_from_slice(&0u32.to_be_bytes());
            put(&mut body, b"stco", &stco);
            body
        };
        let mut minf = Vec::new();
        put(&mut minf, b"vmhd", &[0u8; 8]);
        put(&mut minf, b"dinf", &[0u8; 12]);
        put(&mut minf, b"stbl", &stbl);

        let mut mdia = Vec::new();
        put(&mut mdia, b"mdhd", &mdhd);
        put(&mut mdia, b"hdlr", &hdlr);
        put(&mut mdia, b"minf", &minf);

        let mut tkhd = vec![0u8; 84];
        tkhd[0] = 0; // v0
        tkhd[8..12].copy_from_slice(&1u32.to_be_bytes()); // track id
        tkhd[76..80].copy_from_slice(&(320u32 << 16).to_be_bytes());
        tkhd[80..84].copy_from_slice(&(180u32 << 16).to_be_bytes());

        let mut trak = Vec::new();
        put(&mut trak, b"tkhd", &tkhd);
        put(&mut trak, b"mdia", &mdia);

        let mut moov = Vec::new();
        put(&mut moov, b"mvhd", &mvhd);
        put(&mut moov, b"trak", &trak);
        put(&mut file, b"moov", &moov);
        put(&mut file, b"mdat", &mdat_payload);

        // Patch stco: one chunk, offset = start of mdat payload.
        let mdat_payload_at = file.len() - mdat_payload.len();
        let stco_at = find_stco_payload(&file).expect("stco");
        file[stco_at..stco_at + 4].copy_from_slice(&(mdat_payload_at as u32).to_be_bytes());
        file
    }

    /// Locate the single chunk-offset slot inside our fixture's `stco`.
    fn find_stco_payload(file: &[u8]) -> Option<usize> {
        let moov = top_box(file, b"moov")?;
        let trak = find_all(file, moov.body, moov.len, b"trak").into_iter().next()?;
        let mdia = find_child(file, trak, b"mdia")?;
        let minf = find_child(file, mdia, b"minf")?;
        let stbl = find_child(file, minf, b"stbl")?;
        let stco = find_child(file, stbl, b"stco")?;
        // version/flags(4) entry_count(4) then the offsets.
        Some(stco.body + 8)
    }

    #[test]
    fn a_well_formed_moov_yields_samples_offsets_times_and_keys() {
        let file = build_mp4(b"avc1", 8, 5, 90_000, 3000, true);
        let parsed = parse(&file).expect("parses");
        assert_eq!(parsed.info.width, 320);
        assert_eq!(parsed.info.height, 180);
        assert_eq!(parsed.info.codec, VideoCodec::H264);
        assert_eq!(parsed.info.sample_count, 5);
        assert!(parsed.info.codec_private.as_deref().unwrap().starts_with(&[1, 100]));
        // 3000 ticks at 90 kHz = 33.3 ms per frame, five of them: the whole
        // movie is 15000/90000 = 0.1667 s (the old expected 0.1333 — that is
        // four intervals, one frame short of the fixture's five).
        assert!((parsed.info.duration.unwrap() - 0.1667).abs() < 0.01);
        assert_eq!(parsed.samples.len(), 5);
        assert_eq!(parsed.samples[0].len, 8);
        assert!((parsed.samples[2].pts - 0.0666).abs() < 0.01);
        assert!(parsed.samples[0].key, "stss names sample 1");
        assert!(!parsed.samples[3].key, "and only sample 1");
        // Offsets step by the sample size within the single chunk.
        let second = parsed.samples[1];
        assert_eq!(second.offset, parsed.samples[0].offset + 8);
        assert_eq!(&file[second.offset..second.offset + second.len], &[1u8; 8]);
    }

    #[test]
    fn a_codecd_we_cannot_decode_names_itself() {
        let file = build_mp4(b"hvc1", 8, 2, 90_000, 3000, false);
        let err = parse(&file).expect_err("HEVC is refused");
        assert_eq!(
            err,
            MediaError::UnsupportedCodec {
                codec: "H.265/HEVC".to_string()
            }
        );
    }

    #[test]
    fn every_sample_must_fit_in_the_file() {
        let mut file = build_mp4(b"avc1", 8, 3, 90_000, 1000, false);
        let stco_at = find_stco_payload(&file).expect("stco");
        // Point the chunk at the very end of the file: sample 1 is then off the end.
        let end = file.len();
        file[stco_at..stco_at + 4].copy_from_slice(&((end - 2) as u32).to_be_bytes());
        let err = parse(&file).expect_err("corrupt offsets must not pass");
        assert!(matches!(err, MediaError::Corrupt { .. }), "{err}");
    }

    #[test]
    fn a_moov_less_file_is_refused_by_name() {
        let mut file = Vec::new();
        file.extend_from_slice(&20u32.to_be_bytes());
        file.extend_from_slice(b"ftyp");
        file.extend_from_slice(&[0u8; 12]);
        file.extend_from_slice(&16u32.to_be_bytes());
        file.extend_from_slice(b"moof");
        file.extend_from_slice(&[0u8; 8]);
        let err = parse(&file).expect_err("no moov");
        assert!(matches!(err, MediaError::UnsupportedContainer { .. }), "{err}");
        assert!(err.to_string().contains("fragmented"), "{err}");
    }

    #[test]
    fn sniffing_recognises_the_usual_brands() {
        assert!(looks_like_mp4(b"\0\0\0\x20ftypisom"));
        assert!(looks_like_mp4(b"\0\0\0\x02moov"));
        assert!(!looks_like_mp4(b"RIFFjunkAVI "));
        assert!(!looks_like_mp4(b""));
    }
}

