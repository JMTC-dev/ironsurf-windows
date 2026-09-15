//! Video playback (see `ARCHITECTURE.md` §Video).
//!
//! The tab owns the whole media path: the container is demuxed by us
//! ([`demux`]), frames are decoded by pure-Rust software decoders ([`codec`]),
//! converted to RGBA ([`frame`]), scheduled against a media clock
//! ([`player`]) and composited into the page by a blitz
//! `custom-widget` on the `<video>` element ([`widget`]).
//!
//! Nothing here touches the GPU, and nothing outside the crate is required:
//! no ffmpeg, no gstreamer, no C in the tree.

pub mod codec;
pub mod demux;
pub mod frame;
pub mod player;
pub mod widget;

use std::fmt;

/// Everything that can go wrong between "the page has a `<video src>`" and
/// "pixels are on screen". Rendered into status text and into the element's
/// placeholder, so the strings are user-facing, not internal jargon.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MediaError {
    /// We could not identify the container (or recognised it but cannot read
    /// this variant of it, e.g. fragmented MP4 without a `moov`).
    UnsupportedContainer { hint: String },
    /// The container parsed but has no decodable video track.
    NoVideoTrack,
    /// The container is fine, the codec inside it has no pure-Rust decoder.
    UnsupportedCodec { codec: String },
    /// The bytes stopped making sense.
    Corrupt { detail: String },
    /// The bytes never arrived (the tab's network layer said no).
    Unreachable { detail: String },
    /// A bigger-than-any-video file was requested.
    TooLarge { bytes: usize, limit: usize },
    /// The codec choked on one sample.
    Decode { codec: String, detail: String },
}

impl fmt::Display for MediaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MediaError::UnsupportedContainer { hint } => {
                write!(f, "unsupported media container ({hint})")
            }
            MediaError::NoVideoTrack => write!(f, "no video track in this media"),
            MediaError::UnsupportedCodec { codec } => write!(
                f,
                "no decoder for {codec} — IronSurf decodes H.264, VP9 and raw/MJPEG in pure Rust"
            ),
            MediaError::Corrupt { detail } => write!(f, "media is corrupt: {detail}"),
            MediaError::Unreachable { detail } => write!(f, "cannot fetch media: {detail}"),
            MediaError::TooLarge { bytes, limit } => write!(
                f,
                "media is {} KiB, over IronSurf's {} KiB per-video limit",
                bytes / 1024,
                limit / 1024
            ),
            MediaError::Decode { codec, detail } => {
                write!(f, "{codec} decode error: {detail}")
            }
        }
    }
}

impl std::error::Error for MediaError {}
