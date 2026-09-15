//! Container demuxers: they turn the bytes of one downloaded file into a
//! [`VideoTrack`] — the codec to use, the sample table, and the sample bytes as
//! ranges of that file.
//!
//! # The one interface every container shares
//!
//! A [`Sample`] is *not* decoded; it is an offset and length into the file the
//! player already has in memory. That keeps the demuxers trivially
//! zero-copy, and it is honest about the trade: IronSurf downloads a video
//! before it plays it (there is no byte-range streaming in this browser yet),
//! so holding the file is not an extra cost.
//!
//! Samples come back in **decode order**, which is the order a prediction
//! coded codec must be fed, with presentation order carried by each sample's
//! `pts`. Seeking is therefore "find the last keyframe at or before the
//! target and decode forward from there" — for every supported container the
//! keyframe flag is either explicit (`stss`, block flags, `idx1`) or the codec
//! is intra-coded so every sample qualifies.

mod avi;
mod byte;
mod mp4;
mod webm;

use std::sync::Arc;

use crate::media::codec::VideoCodec;
use crate::media::MediaError;

/// Which kind of file we are looking at, after sniffing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Container {
    /// ISO base media: `mp4`, `m4v`, `mov`, `3gp`.
    Mp4,
    /// Matroska/WebM.
    WebM,
    /// RIFF AVI.
    Avi,
}

impl Container {
    pub fn label(self) -> &'static str {
        match self {
            Container::Mp4 => "MP4",
            Container::WebM => "WebM",
            Container::Avi => "AVI",
        }
    }
}

/// One encoded frame: a range of the file, plus when to show it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Sample {
    /// Byte offset of the sample data in the file.
    pub offset: usize,
    /// Length of the sample data in bytes.
    pub len: usize,
    /// Presentation timestamp in seconds.
    pub pts: f64,
    /// Whether decoding may restart from this sample.
    pub key: bool,
}

impl Sample {
    /// The sample bytes.
    /// The sample's bytes: a window onto the file, clamped to what the file
    /// actually holds (a truncated tail yields the bytes that are there,
    /// rather than nothing — and never panics).
    pub fn bytes<'a>(&self, file: &'a [u8]) -> &'a [u8] {
        let end = self.offset.saturating_add(self.len).min(file.len());
        file.get(self.offset..end).unwrap_or(&[])
    }
}

/// Everything the player needs to know about a video track before decoding.
#[derive(Clone, Debug)]
pub struct MediaInfo {
    pub container: Container,
    /// Playable length in seconds, when the container declares one.
    pub duration: Option<f64>,
    pub width: u32,
    pub height: u32,
    pub codec: VideoCodec,
    /// Codec configuration (`avcC`, VP8 codec private data, …).
    pub codec_private: Option<Vec<u8>>,
    /// Human-readable name of an audio track, when one exists. IronSurf has no
    /// audio output, but the page and status line should say the sound is there.
    pub audio: Option<String>,
    pub sample_count: usize,
}

impl MediaInfo {
    /// Aspect ratio, or 16:9 if the file claims no size.
    pub fn aspect(&self) -> f32 {
        if self.width == 0 || self.height == 0 {
            return 16.0 / 9.0;
        }
        self.width as f32 / self.height as f32
    }

    /// One line for a status bar or an error overlay.
    pub fn describe(&self) -> String {
        let duration = match self.duration {
            Some(seconds) => format!("{:02}:{:02}", seconds as u32 / 60, seconds as u32 % 60),
            None => "?".to_string(),
        };
        format!(
            "{} {}×{} {} {}",
            self.container.label(),
            self.width,
            self.height,
            self.codec.label(),
            duration
        )
    }
}

/// A demuxed video track: the file it came from, its metadata and sample table.
pub struct VideoTrack {
    file: Arc<Vec<u8>>,
    info: MediaInfo,
    samples: Vec<Sample>,
}

impl VideoTrack {
    /// Identify the container and read its sample table.
    pub fn open(file: Arc<Vec<u8>>) -> Result<Self, MediaError> {
        let sniffed = file.get(..16.min(file.len())).unwrap_or(&[]);
        if avi::looks_like_avi(sniffed) {
            let parsed = avi::parse(&file)?;
            return Ok(Self::assemble(file, parsed.info, parsed.samples));
        }
        if webm::looks_like_ebml(sniffed) {
            let parsed = webm::parse(&file)?;
            return Ok(Self::assemble(file, parsed.info, parsed.samples));
        }
        if mp4::looks_like_mp4(sniffed) {
            let parsed = mp4::parse(&file)?;
            return Ok(Self::assemble(file, parsed.info, parsed.samples));
        }
        Err(MediaError::UnsupportedContainer {
            hint: format_container_hint(sniffed),
        })
    }

    fn assemble(file: Arc<Vec<u8>>, info: MediaInfo, samples: Vec<Sample>) -> Self {
        VideoTrack { file, info, samples }
    }

    /// A track assembled by hand, for the player and widget tests (a real
    /// track always comes out of a demuxer).
    #[cfg(test)]
    pub(crate) fn for_test(file: Arc<Vec<u8>>, info: MediaInfo, samples: Vec<Sample>) -> Arc<Self> {
        Arc::new(Self::assemble(file, info, samples))
    }

    pub fn info(&self) -> &MediaInfo {
        &self.info
    }

    pub fn file(&self) -> &[u8] {
        &self.file
    }

    pub fn samples(&self) -> &[Sample] {
        &self.samples
    }

    pub fn len(&self) -> usize {
        self.samples.len()
    }

    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    pub fn sample(&self, index: usize) -> Option<&Sample> {
        self.samples.get(index)
    }

    /// Highest presentation timestamp in the track.
    pub fn last_pts(&self) -> f64 {
        self.samples
            .iter()
            .map(|sample| sample.pts)
            .fold(0.0, f64::max)
    }

    /// Playable duration: what the container says, or the last frame's time.
    pub fn duration(&self) -> f64 {
        self.info
            .duration
            .filter(|seconds| *seconds > 0.0)
            .unwrap_or_else(|| self.last_pts())
    }

    /// Index to resume decoding from when seeking to `target` seconds: the
    /// keyframe whose presentation time is the greatest one at or before the
    /// target. Falls back to the first keyframe, then to sample 0.
    pub fn seek_start(&self, target: f64) -> usize {
        let mut best: Option<(f64, usize)> = None;
        let mut first_key: Option<(f64, usize)> = None;
        for (index, sample) in self.samples.iter().enumerate() {
            if !sample.key {
                continue;
            }
            match first_key {
                Some((time, _)) if time > sample.pts => {}
                None => first_key = Some((sample.pts, index)),
                _ => {}
            }
            if sample.pts <= target {
                match best {
                    Some((time, _)) if time >= sample.pts => {}
                    _ => best = Some((sample.pts, index)),
                }
            }
        }
        best.or(first_key)
            .map(|(_, index)| index)
            .unwrap_or(0)
    }

    /// The sample index whose presentation time is the first one at or after
    /// `target` — used to know when the decoder has caught up after a seek.
    pub fn first_presented_at_or_after(&self, target: f64) -> usize {
        // Decode order is not presentation order (B-frames), so scan for the
        // earliest *presentation time* at or after the target, wherever in the
        // decode order that sample sits.
        let mut best: Option<(f64, usize)> = None;
        for (index, sample) in self.samples.iter().enumerate() {
            if sample.pts >= target && best.is_none_or(|(pts, _)| sample.pts < pts) {
                best = Some((sample.pts, index));
            }
        }
        best.map(|(_, index)| index)
            .unwrap_or_else(|| self.samples.len().saturating_sub(1))
    }
}

/// A readable hint for "this is not a video file we know".
fn format_container_hint(sniffed: &[u8]) -> String {
    if sniffed.starts_with(b"OggS") {
        // Ogg is a real container with real video in it (Theora), but there is
        // no pure-Rust Theora decoder to put inside it.
        return "Ogg container (Theora video needs a decoder IronSurf does not ship)".to_string();
    }
    if sniffed.starts_with(b"\x1a\x45\xdf\xa3") {
        return "EBML file with no readable Segment".to_string();
    }
    if sniffed.starts_with(b"\xff\xd8") {
        return "a JPEG image, not a video".to_string();
    }
    if sniffed.starts_with(b"\x89PNG") {
        return "a PNG image, not a video".to_string();
    }
    let mut head = String::new();
    for byte in sniffed.iter().take(8) {
        head.push_str(&format!("{byte:02x}"));
    }
    format!("unrecognised file (starts {head})")
}

/// The container name for a byte string, for logging and error text.
pub fn sniff_label(bytes: &[u8]) -> String {
    if avi::looks_like_avi(bytes) {
        Container::Avi.label().to_string()
    } else if webm::looks_like_ebml(bytes) {
        Container::WebM.label().to_string()
    } else if mp4::looks_like_mp4(bytes) {
        Container::Mp4.label().to_string()
    } else {
        format_container_hint(bytes)
    }
}

// ---------------------------------------------------------------------------
// Tests

#[cfg(test)]
mod tests {
    use super::*;

    fn track(samples: Vec<Sample>) -> VideoTrack {
        let file = Arc::new(vec![0u8; 1024]);
        let info = MediaInfo {
            container: Container::Mp4,
            duration: Some(10.0),
            width: 320,
            height: 180,
            codec: VideoCodec::H264,
            codec_private: None,
            audio: None,
            sample_count: samples.len(),
        };
        VideoTrack::assemble(file, info, samples)
    }

    fn sample(index: usize, pts: f64, key: bool) -> Sample {
        Sample {
            offset: index * 8,
            len: 8,
            pts,
            key,
        }
    }

    #[test]
    fn seeking_lands_on_the_last_keyframe_at_or_before_the_target() {
        // 0(k) 0.04 0.08 0.12(k) 0.16 0.2(k) 0.24
        let track = track(vec![
            sample(0, 0.0, true),
            sample(1, 0.04, false),
            sample(2, 0.08, false),
            sample(3, 0.12, true),
            sample(4, 0.16, false),
            sample(5, 0.2, true),
            sample(6, 0.24, false),
        ]);
        assert_eq!(track.seek_start(0.05), 0, "before the second keyframe");
        assert_eq!(track.seek_start(0.13), 3);
        assert_eq!(track.seek_start(0.2), 5, "exactly on a keyframe");
        assert_eq!(track.seek_start(0.19), 3);
        assert_eq!(track.seek_start(99.0), 5, "past the end still restarts at the last keyframe");
    }

    #[test]
    fn seeking_a_track_whose_first_keyframe_is_late_still_returns_a_keyframe() {
        let track = track(vec![
            sample(0, 0.0, false),
            sample(1, 0.04, false),
            sample(2, 1.0, true),
            sample(3, 1.04, false),
        ]);
        assert_eq!(track.seek_start(0.1), 2, "the first decodable point wins");
        assert_eq!(track.seek_start(10.0), 2);
    }

    #[test]
    fn a_track_with_no_keyframes_at_all_degrades_to_sample_zero() {
        let track = track(vec![sample(0, 0.0, false), sample(1, 0.04, false)]);
        assert_eq!(track.seek_start(5.0), 0);
    }

    #[test]
    fn duration_prefers_the_container_and_falls_back_to_the_last_frame() {
        // (named so the helper `track` is not shadowed by this binding)
        let with_container_duration = track(vec![sample(0, 0.0, true), sample(1, 0.5, false)]);
        assert_eq!(with_container_duration.duration(), 10.0, "the container says 10 s");

        let mut infoless = track(vec![sample(0, 0.0, true), sample(1, 0.5, false)]);
        infoless.info.duration = None;
        assert_eq!(infoless.duration(), 0.5);

        let mut bogus = track(vec![sample(0, 0.0, true)]);
        bogus.info.duration = Some(0.0);
        assert_eq!(bogus.duration(), 0.0);
    }

    #[test]
    fn presentation_order_lookup_ignores_decode_order_gaps() {
        // Decode order with B-frames: pts 0.0, 0.2, 0.1, 0.3.
        let track = track(vec![
            sample(0, 0.0, true),
            sample(1, 0.2, false),
            sample(2, 0.1, false),
            sample(3, 0.3, false),
        ]);
        assert_eq!(track.first_presented_at_or_after(0.05), 2);
        assert_eq!(track.first_presented_at_or_after(0.15), 1);
        assert_eq!(track.last_pts(), 0.3);
    }

    #[test]
    fn unknown_bytes_are_described_not_misread() {
        // VideoTrack is !Debug, so take the error side directly.
        let err = VideoTrack::open(Arc::new(b"OggS\0\0\0\0theora-ish".to_vec()))
            .err()
            .expect("Ogg is refused");
        assert!(matches!(err, MediaError::UnsupportedContainer { .. }));
        assert!(err.to_string().contains("Ogg"), "{err}");

        let png = VideoTrack::open(Arc::new(b"\x89PNG\r\n\x1a\n".to_vec()))
            .err()
            .expect("an image is not a video");
        assert!(png.to_string().contains("PNG"), "{png}");
    }

    #[test]
    fn sample_bytes_are_a_window_onto_the_file() {
        let file = Arc::new(b"0123456789abcdef".to_vec());
        let sample = Sample {
            offset: 4,
            len: 4,
            pts: 0.0,
            key: true,
        };
        assert_eq!(sample.bytes(&file), b"4567");
        // A sample the file cannot back yields nothing rather than panicking.
        let beyond = Sample {
            offset: 14,
            len: 8,
            pts: 0.0,
            key: true,
        };
        assert_eq!(beyond.bytes(&file), b"ef");
    }

    #[test]
    fn media_info_describes_itself_for_a_status_bar() {
        let info = MediaInfo {
            container: Container::WebM,
            duration: Some(75.0),
            width: 1280,
            height: 720,
            codec: VideoCodec::Vp9,
            codec_private: None,
            audio: Some("Opus audio".to_string()),
            sample_count: 1800,
        };
        assert_eq!(info.describe(), "WebM 1280×720 VP9 01:15");
        assert!((info.aspect() - 16.0 / 9.0).abs() < 0.01);
    }

    #[test]
    fn a_zero_sized_track_still_has_an_aspect_ratio() {
        let mut info = MediaInfo {
            container: Container::Mp4,
            duration: None,
            width: 0,
            height: 0,
            codec: VideoCodec::H264,
            codec_private: None,
            audio: None,
            sample_count: 0,
        };
        assert!((info.aspect() - 16.0 / 9.0).abs() < 0.01);
        info.width = 640;
        info.height = 480;
        assert!((info.aspect() - 4.0 / 3.0).abs() < 0.01);
    }
}
