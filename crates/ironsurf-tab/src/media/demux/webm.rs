//! Matroska / WebM demuxer (EBML).
//!
//! The tree is read with an explicit sibling walk — descend only into the
//! containers we understand, skip everything else by its declared size — so a
//! file full of `SeekHead`, `Cues`, `Tags`, `Attachments` or unknown extension
//! elements still streams past them. Blocks come back as file ranges in
//! **decode order**: a cluster's `Timecode` plus each block's signed relative
//! timecode, scaled by `TimecodeScale`, is the presentation time, and bit 7 of
//! the block flags is the keyframe flag a seek needs.
//!
//! Video only. An audio `TrackEntry` is named in [`MediaInfo::audio`]
//! (IronSurf has no audio output path — `docs/VIDEO.md`) and its blocks are
//! skipped.

use crate::media::codec::VideoCodec;
use crate::media::demux::byte::{corrupt_msg, f32_at, f64_at, i16, uint};
use crate::media::demux::{Container, MediaInfo, Sample};
use crate::media::MediaError;

pub struct ParsedWebM {
    pub info: MediaInfo,
    pub samples: Vec<Sample>,
}

/// The element IDs we care about (an EBML ID is its own type tag).
mod id {
    pub const EBML: [u8; 4] = [0x1a, 0x45, 0xdf, 0xa3];
    pub const VOID: [u8; 1] = [0xec];
    pub const SEGMENT: [u8; 4] = [0x18, 0x53, 0x80, 0x67];
    pub const INFO: [u8; 4] = [0x15, 0x49, 0xa9, 0x66];
    pub const TIMECODE_SCALE: [u8; 3] = [0x2a, 0xd7, 0xb1];
    pub const DURATION: [u8; 2] = [0x44, 0x89];
    pub const TRACKS: [u8; 4] = [0x16, 0x54, 0xae, 0x66];
    pub const TRACK_ENTRY: [u8; 1] = [0xae];
    pub const TRACK_NUMBER: [u8; 1] = [0xd7];
    pub const TRACK_TYPE: [u8; 1] = [0x83];
    pub const CODEC_ID: [u8; 1] = [0x86];
    pub const CODEC_PRIVATE: [u8; 2] = [0x63, 0xa2];
    pub const DEFAULT_DURATION: [u8; 3] = [0x23, 0xe3, 0x83];
    pub const VIDEO: [u8; 1] = [0xe0];
    pub const PIXEL_WIDTH: [u8; 1] = [0xb0];
    pub const PIXEL_HEIGHT: [u8; 1] = [0xba];
    pub const AUDIO: [u8; 1] = [0xe1];
    pub const CLUSTER: [u8; 4] = [0x1f, 0x43, 0xb6, 0x75];
    pub const TIMECODE: [u8; 1] = [0xe7];
    pub const SIMPLE_BLOCK: [u8; 1] = [0xa3];
    pub const BLOCK_GROUP: [u8; 1] = [0xa7];
    pub const BLOCK: [u8; 1] = [0xa1];
    pub const REFERENCE_BLOCK: [u8; 1] = [0xfb];
}

/// One EBML element: its ID bytes and body range in the file.
#[derive(Clone, Copy, Debug)]
struct Element {
    id: [u8; 4],
    body: usize,
    len: usize,
}

fn matches_id(element: &Element, id: &[u8]) -> bool {
    element.id.starts_with(id)
}

/// EBML element ID: the number of leading zero bits of the first byte plus one
/// is the ID's length in bytes.
fn read_id(bytes: &[u8], at: usize) -> Option<([u8; 4], usize)> {
    let first = *bytes.get(at)?;
    let len = usize::try_from(u8::from(first).leading_zeros()).unwrap_or(8) + 1;
    if !(1..=4).contains(&len) || at + len > bytes.len() {
        return None;
    }
    let mut id = [0u8; 4];
    id[..len].copy_from_slice(&bytes[at..at + len]);
    Some((id, at + len))
}

/// EBML variable-length size: `(size, next_byte, unknown)`. `unknown` is the
/// all-ones size, meaning "to the end of the parent" (live streams use it).
fn read_size(bytes: &[u8], at: usize) -> Option<(usize, usize, bool)> {
    let first = *bytes.get(at)?;
    let len = usize::try_from(u8::from(first).leading_zeros()).unwrap_or(8) + 1;
    if len > 8 || at + len > bytes.len() {
        return None;
    }
    let mut value = u64::from(first & (0xff >> len));
    for byte in &bytes[at + 1..at + len] {
        value = (value << 8) | u64::from(*byte);
    }
    let max_for_len = (1u64 << (7 * len)) - 1;
    if value == max_for_len {
        return Some((usize::MAX, at + len, true));
    }
    Some((usize::try_from(value).unwrap_or(usize::MAX), at + len, false))
}

/// Sibling walk over the elements of one container.
struct Walk<'a> {
    data: &'a [u8],
    at: usize,
    end: usize,
}

impl<'a> Walk<'a> {
    fn new(data: &'a [u8], at: usize, len: usize) -> Self {
        let end = at.saturating_add(len).min(data.len());
        Walk { data, at, end }
    }
}

impl Iterator for Walk<'_> {
    type Item = Result<Element, MediaError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.at >= self.end {
            return None;
        }
        let stop_with = |this: &mut Self, detail: &'static str| {
            this.at = this.end;
            Some(Err(corrupt_msg(detail)))
        };
        let (id, after_id) = match read_id(self.data, self.at) {
            Some(found) => found,
            None => return stop_with(self, "truncated EBML element ID"),
        };
        let (len, body, unknown) = match read_size(self.data, after_id) {
            Some((len, body, unknown)) => (len, body, unknown),
            None => return stop_with(self, "truncated EBML element size"),
        };
        let len = if unknown {
            self.end.saturating_sub(body)
        } else {
            len
        };
        if body.checked_add(len).is_none_or(|stop| stop > self.end) {
            return stop_with(self, "EBML element runs past its parent");
        }
        self.at = body + len;
        Some(Ok(Element { id, body, len }))
    }
}

fn uint_body(data: &[u8], element: &Element) -> Option<u64> {
    uint(data, element.body, element.len)
}

fn float_body(data: &[u8], element: &Element) -> Option<f64> {
    match element.len {
        4 => f32_at(data, element.body).map(f64::from),
        8 => f64_at(data, element.body),
        // Some muxers write an integer where a float is declared.
        other if (1..=8).contains(&other) => uint(data, element.body, other).map(|v| v as f64),
        _ => None,
    }
}

fn string_body(data: &[u8], element: &Element) -> Option<String> {
    let raw = data.get(element.body..element.body + element.len)?;
    let trimmed = raw.split(|byte| *byte == 0).next().unwrap_or(raw);
    Some(String::from_utf8_lossy(trimmed).into_owned())
}

fn sint_body(data: &[u8], element: &Element) -> Option<i64> {
    let raw = data.get(element.body..element.body + element.len)?;
    if raw.is_empty() {
        return None;
    }
    // Signed EBML integer: the first byte's top bit is the sign, and the
    // whole `len`-byte field is two's complement.
    let mut value: u64 = 0;
    for byte in raw {
        value = (value << 8) | u64::from(*byte);
    }
    let value = if raw[0] & 0x80 != 0 && raw.len() < 8 {
        (value as i64) - (1i64 << (8 * raw.len()))
    } else {
        // `as i64` is exactly the two's-complement reading for 8-byte fields.
        value as i64
    };
    Some(value)
}

/// The audio codecs WebM/MKV names, spelled the way a status bar should.
pub fn audio_codec_label(codec_id: &str) -> String {
    match codec_id {
        "A_VORBIS" => "Vorbis audio".to_string(),
        "A_OPUS" => "Opus audio".to_string(),
        "A_AAC" => "AAC audio".to_string(),
        "A_AC3" => "AC-3 audio".to_string(),
        "A_MPEG/L3" => "MP3 audio".to_string(),
        "A_FLAC" => "FLAC audio".to_string(),
        "A_PCM" => "PCM audio".to_string(),
        other if codec_id.starts_with("A_") => format!("audio ({})", &other[2..]),
        other => other.to_string(),
    }
}

/// One `TrackEntry` as far as video playback cares.
///
/// `Default` is written by hand because `VideoCodec` has no sensible default
/// of its own — an unnamed codec is `Unknown("")`, which is what we report if
/// a track has no `CodecID` at all.
#[derive(Clone, Debug)]
struct TrackEntry {
    number: u64,
    is_video: bool,
    is_audio: bool,
    codec: VideoCodec,
    codec_id: String,
    codec_private: Option<Vec<u8>>,
    width: u32,
    height: u32,
}

impl Default for TrackEntry {
    fn default() -> Self {
        TrackEntry {
            number: 0,
            is_video: false,
            is_audio: false,
            codec: VideoCodec::Unknown(String::new()),
            codec_id: String::new(),
            codec_private: None,
            width: 0,
            height: 0,
        }
    }
}

/// Parse a WebM/Matroska file into its video track.
pub fn parse(file: &[u8]) -> Result<ParsedWebM, MediaError> {
    let mut timecode_scale_ns: u64 = 1_000_000; // the EBML default (1 ms)
    let mut duration_units: Option<f64> = None;
    let mut tracks: Vec<TrackEntry> = Vec::new();
    let mut audio: Option<String> = None;
    let mut samples: Vec<Sample> = Vec::new();

    // The `Segment` must come straight after the header (and/or `Void`).
    let segment = {
        let mut top = Walk::new(file, 0, file.len());
        loop {
            match top.next() {
                None | Some(Err(_)) => {
                    return Err(MediaError::UnsupportedContainer {
                        hint: "the EBML header is not followed by a Segment".to_string(),
                    })
                }
                Some(Ok(element)) => {
                    if matches_id(&element, &id::SEGMENT) {
                        break element;
                    }
                    if !matches_id(&element, &id::EBML) && !matches_id(&element, &id::VOID) {
                        return Err(MediaError::UnsupportedContainer {
                            hint: "the EBML header is not followed by a Segment".to_string(),
                        });
                    }
                }
            }
        }
    };

    for item in Walk::new(file, segment.body, segment.len) {
        let element = item?;
        if matches_id(&element, &id::INFO) {
            for child in Walk::new(file, element.body, element.len) {
                let child = child?;
                if matches_id(&child, &id::TIMECODE_SCALE) {
                    timecode_scale_ns = uint_body(file, &child)
                        .filter(|value| *value > 0)
                        .unwrap_or(1_000_000);
                } else if matches_id(&child, &id::DURATION) {
                    duration_units = float_body(file, &child);
                }
            }
        } else if matches_id(&element, &id::TRACKS) {
            for child in Walk::new(file, element.body, element.len) {
                let child = child?;
                if !matches_id(&child, &id::TRACK_ENTRY) {
                    continue;
                }
                let entry = read_track(file, &child);
                if entry.is_audio && audio.is_none() {
                    audio = Some(audio_codec_label(&entry.codec_id));
                }
                tracks.push(entry);
            }
        } else if matches_id(&element, &id::CLUSTER) {
            let cluster_tc = Walk::new(file, element.body, element.len)
                .next()
                .and_then(Result::ok)
                // Cluster Timecode is an UNSIGNED EBML integer (only block
                // relative timecodes are signed).
                .filter(|child| matches_id(child, &id::TIMECODE))
                .and_then(|child| uint_body(file, &child))
                .map(|value| value.min(i64::MAX as u64) as i64)
                .unwrap_or(0);
            read_blocks(file, &element, cluster_tc, timecode_scale_ns, &tracks, &mut samples)?;
        }
    }

    let video = tracks
        .iter()
        .find(|track| track.is_video)
        .ok_or(MediaError::NoVideoTrack)?;
    if !video.codec.is_supported() {
        return Err(MediaError::UnsupportedCodec {
            codec: video.codec.label(),
        });
    }
    if samples.is_empty() {
        return Err(MediaError::Corrupt {
            detail: "no video blocks in any cluster".to_string(),
        });
    }

    let duration = duration_units
        .filter(|value| *value > 0.0)
        .map(|units| units * timecode_scale_ns as f64 / 1_000_000_000.0)
        .or_else(|| samples.last().map(|sample| sample.pts));

    Ok(ParsedWebM {
        info: MediaInfo {
            container: Container::WebM,
            duration,
            width: video.width,
            height: video.height,
            codec: video.codec.clone(),
            codec_private: video.codec_private.clone(),
            audio,
            sample_count: samples.len(),
        },
        samples,
    })
}

fn read_track(file: &[u8], entry: &Element) -> TrackEntry {
    let mut track = TrackEntry::default();
    for item in Walk::new(file, entry.body, entry.len) {
        let Ok(child) = item else { break };
        if matches_id(&child, &id::TRACK_NUMBER) {
            track.number = uint_body(file, &child).unwrap_or(0);
        } else if matches_id(&child, &id::TRACK_TYPE) {
            let kind = uint_body(file, &child);
            track.is_video = kind == Some(1);
            track.is_audio = kind == Some(2);
        } else if matches_id(&child, &id::CODEC_ID) {
            track.codec_id = string_body(file, &child).unwrap_or_default();
            track.codec = VideoCodec::from_matroska_codec_id(&track.codec_id);
        } else if matches_id(&child, &id::CODEC_PRIVATE) {
            track.codec_private = file
                .get(child.body..child.body + child.len)
                .map(|bytes| bytes.to_vec());
        } else if matches_id(&child, &id::VIDEO) {
            for dimension in Walk::new(file, child.body, child.len) {
                let Ok(dimension) = dimension else { break };
                if matches_id(&dimension, &id::PIXEL_WIDTH) {
                    track.width = uint_body(file, &dimension).unwrap_or(0) as u32;
                } else if matches_id(&dimension, &id::PIXEL_HEIGHT) {
                    track.height = uint_body(file, &dimension).unwrap_or(0) as u32;
                }
            }
        }
    }
    track
}

/// Pull one cluster's video blocks into `samples`.
fn read_blocks(
    file: &[u8],
    cluster: &Element,
    cluster_tc: i64,
    timecode_scale_ns: u64,
    tracks: &[TrackEntry],
    samples: &mut Vec<Sample>,
) -> Result<(), MediaError> {
    for item in Walk::new(file, cluster.body, cluster.len) {
        let element = item?;
        // A `SimpleBlock` is its own `BlockGroup`; a `BlockGroup`'s first
        // `Block` is the presentation unit, and any `ReferenceBlock` means it
        // is not a clean restart point.
        let (block_body, block_len, references) = if matches_id(&element, &id::SIMPLE_BLOCK) {
            (element.body, element.len, false)
        } else if matches_id(&element, &id::BLOCK_GROUP) {
            let mut block = None;
            let mut references = false;
            for child in Walk::new(file, element.body, element.len) {
                let Ok(child) = child else { break };
                if matches_id(&child, &id::BLOCK) && block.is_none() {
                    block = Some((child.body, child.len));
                } else if matches_id(&child, &id::REFERENCE_BLOCK) {
                    references = true;
                }
            }
            match block {
                Some((body, len)) => (body, len, references),
                None => continue,
            }
        } else {
            continue;
        };

        // Block payload: TrackNumber (vint), i16 relative timecode, flags, data.
        let Some((track_number, after_track)) = read_block_track_number(file, block_body) else {
            return Err(corrupt_msg("block starts with an invalid track number"));
        };
        if !tracks
            .iter()
            .any(|track| track.is_video && track.number == track_number)
        {
            continue; // audio, or a track we are not showing
        }
        let relative_tc = i16(file, after_track).ok_or_else(|| {
            corrupt_msg("block has no relative timecode")
        })?;
        let flags = *file.get(after_track + 2).ok_or_else(|| {
            corrupt_msg("block has no flags byte")
        })?;
        let data_at = after_track + 3;
        if data_at > block_body + block_len {
            return Err(corrupt_msg("block header runs past the end of the block"));
        }
        let pts_ns = cluster_tc.saturating_mul(timecode_scale_ns as i64)
            + i64::from(relative_tc).saturating_mul(1_000_000);
        samples.push(Sample {
            offset: data_at,
            len: (block_body + block_len) - data_at,
            // Blocks may carry a negative timecode relative to a later cluster;
            // playback starts at 0, so clamp rather than drop them.
            pts: pts_ns.max(0) as f64 / 1_000_000_000.0,
            key: !references && flags & 0x80 != 0,
        });
    }
    Ok(())
}

/// A block's `TrackNumber` varint (unsigned EBML vint, 1–4 bytes in practice).
fn read_block_track_number(file: &[u8], at: usize) -> Option<(u64, usize)> {
    let first = *file.get(at)?;
    let len = usize::try_from(u8::from(first).leading_zeros()).unwrap_or(8) + 1;
    if len > 4 || at + len > file.len() {
        return None;
    }
    let mut value = u64::from(first & (0xff >> len));
    for byte in &file[at + 1..at + len] {
        value = (value << 8) | u64::from(*byte);
    }
    Some((value, at + len))
}

/// Whether these bytes are EBML (and therefore WebM or Matroska).
pub fn looks_like_ebml(bytes: &[u8]) -> bool {
    bytes.starts_with(&id::EBML)
}

// ---------------------------------------------------------------------------
// Tests

#[cfg(test)]
mod tests {
    use super::*;

    /// The size field of an element of `len` bytes, EBML-encoded: a length
    /// marker bit, then the value big-endian.
    fn size_bytes(len: usize) -> Vec<u8> {
        let value = len as u64;
        let width = if value <= 127 {
            1
        } else if value <= 16_383 {
            2
        } else if value <= 2_097_151 {
            3
        } else if value <= 268_435_455 {
            4
        } else {
            8
        };
        let marker = 0x80u8 >> (width - 1);
        let mut out = vec![marker | ((value >> (8 * (width - 1))) as u8 & (0xff >> width))];
        for shift in (0..width - 1).rev() {
            out.push((value >> (8 * shift)) as u8);
        }
        out
    }

    /// Element writer for the fixtures below.
    fn element(id: &[u8], body: &[u8]) -> Vec<u8> {
        let mut out = id.to_vec();
        out.extend_from_slice(&size_bytes(body.len()));
        out.extend_from_slice(body);
        out
    }

    fn container(id: &[u8], children: &[&[u8]]) -> Vec<u8> {
        let mut body = Vec::new();
        for child in children {
            body.extend_from_slice(child);
        }
        element(id, &body)
    }

    /// Unsigned EBML integer with the minimal number of bytes.
    fn uint_element(id: &[u8], value: u64) -> Vec<u8> {
        let bytes = value.to_be_bytes();
        let start = (8 - usize::try_from((64 - value.leading_zeros()).div_ceil(8)).unwrap_or(1))
            .clamp(0, 7);
        element(id, &bytes[start..])
    }

    /// A two-track WebM: video track 1, Vorbis audio track 2, two clusters.
    fn fixture(codec_id: &str) -> Vec<u8> {
        let video_track = container(
            &id::TRACK_ENTRY,
            &[
                uint_element(&id::TRACK_NUMBER, 1).as_slice(),
                element(&id::TRACK_TYPE, &[1]).as_slice(),
                element(&id::CODEC_ID, codec_id.as_bytes()).as_slice(),
                container(
                    &id::VIDEO,
                    &[
                        uint_element(&id::PIXEL_WIDTH, 640).as_slice(),
                        uint_element(&id::PIXEL_HEIGHT, 360).as_slice(),
                    ],
                )
                .as_slice(),
            ],
        );
        let audio_track = container(
            &id::TRACK_ENTRY,
            &[
                uint_element(&id::TRACK_NUMBER, 2).as_slice(),
                element(&id::TRACK_TYPE, &[2]).as_slice(),
                element(&id::CODEC_ID, b"A_VORBIS").as_slice(),
                container(&id::AUDIO, &[uint_element(&id::TRACK_NUMBER, 2).as_slice()]).as_slice(),
            ],
        );
        let tracks = container(&id::TRACKS, &[&video_track, &audio_track]);
        let info = container(
            &id::INFO,
            &[
                uint_element(&id::TIMECODE_SCALE, 1_000_000).as_slice(),
                element(&id::DURATION, &2500.0f64.to_be_bytes()).as_slice(),
            ],
        );

        let mut block_one = vec![0x81]; // track 1
        block_one.extend_from_slice(&0i16.to_be_bytes());
        block_one.push(0x80); // keyframe
        block_one.extend_from_slice(b"FRAME-ONE");
        let mut block_two = vec![0x81];
        block_two.extend_from_slice(&40i16.to_be_bytes());
        block_two.push(0x00); // inter frame
        block_two.extend_from_slice(b"FRAME-TWO");
        let mut audio_block = vec![0x82]; // track 2
        audio_block.extend_from_slice(&40i16.to_be_bytes());
        audio_block.push(0x80);
        audio_block.extend_from_slice(b"SOUND-BLOCK");

        let cluster_one = container(
            &id::CLUSTER,
            &[
                uint_element(&id::TIMECODE, 0).as_slice(),
                element(&id::SIMPLE_BLOCK, &block_one).as_slice(),
                element(&id::SIMPLE_BLOCK, &audio_block).as_slice(),
            ],
        );
        let cluster_two = container(
            &id::CLUSTER,
            &[
                uint_element(&id::TIMECODE, 1000).as_slice(),
                element(&id::SIMPLE_BLOCK, &block_two).as_slice(),
            ],
        );
        let void_ = element(&id::VOID, b"padding");

        container(
            &id::SEGMENT,
            &[&info, &tracks, &void_, &cluster_one, &cluster_two],
        )
    }

    fn webm(segment: &[u8]) -> Vec<u8> {
        let mut out = element(&id::EBML, b"\x85"); // header body of one byte
        out.extend_from_slice(segment);
        out
    }

    #[test]
    fn the_fixture_writer_encodes_sizes_the_reader_accepts() {
        // The two forms our fixtures need.
        assert_eq!(size_bytes(0), vec![0x80]);
        assert_eq!(size_bytes(15), vec![0x8f]);
        // 4000 fits the 2-byte form: marker 0x40 with the value packed in.
        assert_eq!(size_bytes(4000), vec![0x4f, 0xa0]);
        assert_eq!(read_size(&size_bytes(4000), 0).map(|(len, _, _)| len), Some(4000));
        assert_eq!(read_size(&size_bytes(300_000), 0).map(|(len, _, _)| len), Some(300_000));
    }

    #[test]
    fn tracks_blocks_and_timecodes_all_come_out() {
        let file = webm(&fixture("V_VP9"));
        let parsed = parse(&file).expect("parses");
        assert_eq!(parsed.info.container, Container::WebM);
        assert_eq!(parsed.info.codec, VideoCodec::Vp9);
        assert_eq!((parsed.info.width, parsed.info.height), (640, 360));
        // Info/Duration is 2500 at the default 1 ms timecode scale.
        assert!((parsed.info.duration.unwrap() - 2.5).abs() < 0.001);
        assert_eq!(parsed.info.audio.as_deref(), Some("Vorbis audio"));
        assert_eq!(parsed.info.sample_count, 2, "the audio block is not a video sample");
        assert_eq!(parsed.samples[0].pts, 0.0);
        assert!(parsed.samples[0].key);
        assert_eq!(parsed.samples[1].pts, 1.04, "cluster timecode + relative timecode");
        assert!(!parsed.samples[1].key);
        assert_eq!(
            &file[parsed.samples[0].offset..parsed.samples[0].offset + parsed.samples[0].len],
            b"FRAME-ONE"
        );
    }

    #[test]
    fn vp8_is_refused_by_name_not_silently() {
        let file = webm(&fixture("V_VP8"));
        // ParsedWebM holds non-Debug pieces, so take the error side directly.
        let err = parse(&file).err().expect("VP8 has no decoder");
        assert_eq!(
            err,
            MediaError::UnsupportedCodec {
                codec: "VP8".to_string()
            }
        );
    }

    #[test]
    fn a_file_without_a_segment_stops_at_the_header() {
        let mut garbage = vec![0x1a, 0x45, 0xdf, 0xa3, 0x81, 0x00];
        garbage.extend_from_slice(b"junk");
        let err = parse(&garbage).err().expect("no segment");
        assert!(matches!(err, MediaError::UnsupportedContainer { .. }), "{err}");
    }

    #[test]
    fn ebml_varints_decode_the_documented_lengths() {
        assert_eq!(read_size(&[0x8f], 0), Some((15, 1, false)));
        // The 4-byte vint form carries its marker in 0x10 (the fixture once
        // said 0x40, which EBML documents as the 2-byte form, not this one).
        assert_eq!(read_size(&[0x10, 0x00, 0x10, 0x00], 0), Some((4096, 4, false)));
        // All-ones means "unknown: to the end of the parent".
        assert_eq!(read_size(&[0xff], 0), Some((usize::MAX, 1, true)));
        assert_eq!(
            read_id(&[0x1a, 0x45, 0xdf, 0xa3], 0),
            Some(([0x1a, 0x45, 0xdf, 0xa3], 4))
        );
        assert_eq!(read_id(&[0xae], 0).map(|(id, next)| (id[0], next)), Some((0xae, 1)));
        assert_eq!(read_id(&[0x00], 0), None, "a zero first byte is not an ID");
    }

    #[test]
    fn signed_block_timecodes_are_read_as_signed() {
        let element = Element {
            id: [0xe7, 0, 0, 0],
            body: 1,
            len: 2,
        };
        let data = [0, 0xff, 0xfe]; // -2
        assert_eq!(sint_body(&data, &element), Some(-2));
        let positive = [0, 0x7f, 0x01];
        assert_eq!(
            sint_body(&positive, &element),
            Some(i64::from_be_bytes([0, 0, 0, 0, 0, 0, 0x7f, 1]))
        );
    }

    #[test]
    fn sniffing_recognises_ebml() {
        assert!(looks_like_ebml(&[0x1a, 0x45, 0xdf, 0xa3, 0x80]));
        assert!(!looks_like_ebml(b"ftyp"));
    }
}
