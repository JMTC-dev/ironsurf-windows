//! Software video decoders, all pure Rust.
//!
//! IronSurf ships no codecs of its own and links no C. What it does have is a
//! small set of decoders that are (a) software-only, (b) free of a C toolchain,
//! and (c) honest about the rest:
//!
//! | codec | container(s) | how |
//! |---|---|---|
//! | H.264/AVC | MP4, MOV, (MKV) | [`rusty_h264_decoder`] — Constrained Baseline → most of High, CAVLC + CABAC, bit-exact vs Cisco `h264dec` |
//! | VP9 | WebM, MKV, IVF | [`vp9dec`] — clean-room VP9 per the Google spec, zero dependencies |
//! | MJPEG | AVI | `jpeg-decoder` (every sample is a baseline JPEG) |
//! | raw RGB / YUV | AVI (`RGB `, `DIB `, `YUY2`, `I420`, …) | direct byte-order conversion, see [`crate::media::frame`] |
//!
//! VP8 (the other half of the early WebM world), Theora, HEVC and AV1 have no
//! maintained pure-Rust *decoder*: they are reported as
//! [`MediaError::UnsupportedCodec`] — an `error` event on the element and a
//! readable explanation in its placeholder — rather than failing silently.
//!
//! Decoders speak a tiny contract: feed samples in **decode order**, get
//! pictures out (with the presentation timestamp attached), then `flush`.
//! Inter prediction is stateful, so a seek is "build a fresh decoder, feed it
//! from the nearest keyframe" — the same thing every player does.

use std::collections::VecDeque;

use crate::media::frame::{self, Frame};
use crate::media::MediaError;

/// The video coding of a track, as named by its container.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VideoCodec {
    /// `avc1`/`avc3`/`avcs` in ISO base media, `V_MPEG4/ISO/AVC` in Matroska.
    H264,
    /// `hvc1`/`hev1` — no pure-Rust decoder is wired in.
    Hevc,
    /// `V_VP8` — libvpx territory, no pure-Rust decoder.
    Vp8,
    /// `V_VP9` — decoded by [`vp9dec`].
    Vp9,
    /// `av01`/`V_AV1` — no pure-Rust decoder.
    Av1,
    /// Motion JPEG: every sample is a JPEG still (`MJPG`/`DIB `in AVI, `jpeg` in MOV).
    Mjpeg,
    /// Uncompressed pixel data (`RGB `, `BGR `, `YUY2`, `UYVY`, `I420`, `IYU1/2`).
    Raw(RawFormat),
    /// Something else; `String` is the container's own name for it.
    Unknown(String),
}

/// Uncompressed pixel layouts we can map straight to RGBA8.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RawFormat {
    /// 24-bit packed red/green/blue.
    Rgb24,
    /// 32-bit packed RGB with an ignored fourth byte.
    Rgba32,
    /// 4:2:2 packed, Y0 Cb Y1 Cr.
    Yuyv,
    /// 4:2:2 packed, Cb Y0 Cr Y1.
    Uyvy,
    /// Planar 4:2:0.
    I420,
}

impl VideoCodec {
    /// Human-readable codec name, for status text, `error` events and the
    /// placeholder drawn on an unplayable element.
    pub fn label(&self) -> String {
        match self {
            VideoCodec::H264 => "H.264/AVC".to_string(),
            VideoCodec::Hevc => "H.265/HEVC".to_string(),
            VideoCodec::Vp8 => "VP8".to_string(),
            VideoCodec::Vp9 => "VP9".to_string(),
            VideoCodec::Av1 => "AV1".to_string(),
            VideoCodec::Mjpeg => "Motion JPEG".to_string(),
            VideoCodec::Raw(fmt) => format!("{fmt:?} (uncompressed)"),
            VideoCodec::Unknown(other) => other.clone(),
        }
    }

    /// Whether IronSurf can decode this at all (no runtime probing needed).
    pub fn is_supported(&self) -> bool {
        matches!(
            self,
            VideoCodec::H264 | VideoCodec::Vp9 | VideoCodec::Mjpeg | VideoCodec::Raw(_)
        )
    }

    /// The MP4/QuickTime sample-entry fourcc (`avc1`, `hvc1`, `JPEG`, …).
    pub fn from_isom_fourcc(fourcc: &[u8; 4]) -> Self {
        match fourcc {
            b"avc1" | b"avc3" | b"avcs" | b"Avc1" | b"Avc3" => VideoCodec::H264,
            b"hvc1" | b"hev1" | b"hvt1" | b"hevC" => VideoCodec::Hevc,
            b"vp09" | b"VP90" => VideoCodec::Vp9,
            b"mp4v" | b"MP4V" => VideoCodec::Unknown("MPEG-4 Part 2".to_string()),
            b"jpeg" | b"jpg " | b"JPGL" | b"MJPG" | b"mj2s" => VideoCodec::Mjpeg,
            b"YUY2" => VideoCodec::Raw(RawFormat::Yuyv),
            b"UYVY" => VideoCodec::Raw(RawFormat::Uyvy),
            b"I420" | b"iv42" | b"YV12" => VideoCodec::Raw(RawFormat::I420),
            b"RGB " | b"raw " => VideoCodec::Raw(RawFormat::Rgb24),
            b"RGBA" | b"rgb3" => VideoCodec::Raw(RawFormat::Rgba32),
            other => VideoCodec::Unknown(fourcc_label(other)),
        }
    }

    /// The Matroska/WebM `CodecID` (`V_MPEG4/ISO/AVC`, `V_VP9`, `V_UNCOMPRESSED`, …).
    pub fn from_matroska_codec_id(id: &str) -> Self {
        if id == "V_MPEG4/ISO/AVC" || id.starts_with("V_MPEG4/ISO/AVC") {
            VideoCodec::H264
        } else if id == "V_MPEGH/ISO/HEVC" {
            VideoCodec::Hevc
        } else if id == "V_VP8" {
            VideoCodec::Vp8
        } else if id == "V_VP9" {
            VideoCodec::Vp9
        } else if id == "V_AV1" {
            VideoCodec::Av1
        } else if id.starts_with("V_MJPEG") {
            VideoCodec::Mjpeg
        } else if id == "V_UNCOMPRESSED" {
            // Matroska names the fourcc in CodecDecode; without it, assume RGB.
            VideoCodec::Raw(RawFormat::Rgb24)
        } else {
            VideoCodec::Unknown(id.to_string())
        }
    }

    /// The AVI `BI_COMPRESSION` value: a fourcc for video codecs, or a
    /// `BI_RGB`-style integer for uncompressed bitmaps.
    pub fn from_avi_fourcc(value: u32) -> Self {
        const BI_RGB: u32 = 0;
        const BI_BITFIELDS: u32 = 3;
        match value {
            BI_RGB | BI_BITFIELDS => VideoCodec::Raw(RawFormat::Rgb24),
            other => {
                // Fourcc-as-int padding: trailing NULs are not part of the name,
                // but trailing spaces are (`b"RGB "` vs `b"RGB\0"` are different
                // codecs), so trim only NULs. A Vec is needed because `[u8; 4]`
                // has no `pop()`.
                let mut bytes = other.to_le_bytes().to_vec();
                while bytes.last() == Some(&0) && bytes.len() > 1 {
                    bytes.pop();
                }
                match bytes.as_slice() {
                    b"YUY2" | b"yuvs" | b"YUNV" => VideoCodec::Raw(RawFormat::Yuyv),
                    b"UYVY" | b"uyvy" => VideoCodec::Raw(RawFormat::Uyvy),
                    b"I420" | b"IV12" | b"YV12" | b"IYU1" | b"iyuv" => {
                        VideoCodec::Raw(RawFormat::I420)
                    }
                    b"RGB " | b"DIB " | b"raw " | b"rgb " => VideoCodec::Raw(RawFormat::Rgb24),
                    b"RGBA" | b"rgb3" | b"ARGB" => VideoCodec::Raw(RawFormat::Rgba32),
                    b"MJPG" | b"jpeg" | b"mjpg" | b"MJFJ" | b"1MJG" => VideoCodec::Mjpeg,
                    b"H264" | b"h264" | b"X264" | b"avc1" => VideoCodec::H264,
                    b"VP90" => VideoCodec::Vp9,
                    // VP8 is named honestly; `make_decoder` refuses it with
                    // MediaError::UnsupportedCodec (no pure-Rust VP8 decoder).
                    b"VP80" => VideoCodec::Vp8,
                    other => VideoCodec::Unknown(fourcc_label(other)),
                }
            }
        }
    }
}

fn fourcc_label(bytes: &[u8]) -> String {
    if bytes.is_empty() || bytes.len() > 8 {
        return format!("codec 0x{:x}", u32::from_le_bytes(bytes.first_chunk::<4>().copied().unwrap_or_default()));
    }
    String::from_utf8_lossy(bytes)
        .trim_end()
        .trim_matches('\0')
        .to_string()
}

/// One decoded picture, stamped with the presentation time it was made for.
#[derive(Debug)]
pub struct Picture {
    pub pts: f64,
    pub frame: Frame,
}

/// What a decoder needs beyond the sample bytes.
#[derive(Clone, Debug)]
pub struct CodecParams {
    pub codec: VideoCodec,
    /// Track display size from the container (raw formats need it).
    pub width: u32,
    pub height: u32,
    /// `CodecPrivate`/`avcC`/`hvcC`/`PVCE` blob, verbatim from the container.
    pub codec_private: Option<Vec<u8>>,
}

/// A software picture decoder.
///
/// Samples arrive in **container (decode) order** with their presentation
/// time; implementations that reorder (H.264 with B-frames) buffer internally
/// and hand out pictures in display order. A picture is emitted at most once
/// per sample that carries a coded picture.
pub trait PictureDecoder: Send {
    /// Decode one sample. `Ok(())` with a pushed picture is the normal path;
    /// a corrupt sample returns [`MediaError::Decode`].
    fn decode(&mut self, sample: &[u8], pts: f64, out: &mut Vec<Picture>) -> Result<(), MediaError>;
    /// Drain anything still buffered (end of stream / end of seek range).
    fn flush(&mut self, out: &mut Vec<Picture>) -> Result<(), MediaError>;
    /// Codec this instance decodes (for error messages).
    fn codec(&self) -> VideoCodec;
}

/// Build the decoder for a track's codec, or say honestly why there isn't one.
pub fn make_decoder(params: &CodecParams) -> Result<Box<dyn PictureDecoder>, MediaError> {
    match params.codec {
        VideoCodec::H264 => Ok(Box::new(H264Decoder::new(params)?)),
        VideoCodec::Vp9 => Ok(Box::new(Vp9Decoder::default())),
        VideoCodec::Mjpeg => Ok(Box::new(JpegDecoder::default())),
        VideoCodec::Raw(fmt) => Ok(Box::new(RawDecoder::new(
            fmt,
            params.width.max(1) as usize,
            params.height.max(1) as usize,
        ))),
        VideoCodec::Hevc | VideoCodec::Vp8 | VideoCodec::Av1 | VideoCodec::Unknown(_) => {
            Err(MediaError::UnsupportedCodec {
                codec: params.codec.label(),
            })
        }
    }
}

// ---------------------------------------------------------------------------
// H.264

/// Parameter sets (`avcC`) that prime the H.264 decoder before the first sample.
#[derive(Clone, Debug, Default)]
struct AvcC {
    /// Length in bytes of the per-NALU length prefix in samples (1/2/3/4).
    length_size: usize,
    /// SPS + PPS NALUs already in Annex-B form (start codes included).
    parameter_sets: Vec<u8>,
}

impl AvcC {
    /// Parse the `avcC` box payload (ISO/IEC 14496-15 §5.3.3.1).
    fn parse(data: &[u8]) -> Option<Self> {
        // configurationVersion, AVCProfileIndication, profile_compatibility,
        // AVCLevelIndication, 111111|lengthSizeMinusOne, 111|numOfSequenceParameterSets
        let head = data.first()?;
        if *head == 0 || data.len() < 7 {
            return None;
        }
        let length_size = usize::from(data[4] & 0x03) + 1;
        let num_sps = usize::from(data[5] & 0x1f);
        let mut cursor = 6usize;
        let mut parameter_sets = Vec::new();
        for _ in 0..num_sps {
            // Explicit binding: `try_into()` needs a target type to infer.
            let len_bytes: [u8; 2] = data.get(cursor..cursor + 2)?.try_into().ok()?;
            let len = usize::from(u16::from_be_bytes(len_bytes));
            cursor += 2;
            let nalu = data.get(cursor..cursor + len)?;
            parameter_sets.extend_from_slice(&[0, 0, 0, 1]);
            parameter_sets.extend_from_slice(nalu);
            cursor += len;
        }
        let num_pps = usize::from(*data.get(cursor)?);
        cursor += 1;
        for _ in 0..num_pps {
            let len_bytes: [u8; 2] = data.get(cursor..cursor + 2)?.try_into().ok()?;
            let len = usize::from(u16::from_be_bytes(len_bytes));
            cursor += 2;
            let nalu = data.get(cursor..cursor + len)?;
            parameter_sets.extend_from_slice(&[0, 0, 0, 1]);
            parameter_sets.extend_from_slice(nalu);
            cursor += len;
        }
        if parameter_sets.is_empty() {
            return None;
        }
        Some(AvcC {
            length_size,
            parameter_sets,
        })
    }

    /// Turn one length-prefixed ISO-BMFF sample into an Annex-B access unit.
    fn annex_b(&self, sample: &[u8], out: &mut Vec<u8>) -> Option<()> {
        let n = self.length_size;
        out.clear();
        out.extend_from_slice(&self.parameter_sets);
        let mut at = 0usize;
        loop {
            if at + n > sample.len() {
                return if at == sample.len() { Some(()) } else { None };
            }
            let mut len = 0usize;
            for byte in &sample[at..at + n] {
                len = (len << 8) | usize::from(*byte);
            }
            at += n;
            let nalu = sample.get(at..at + len)?;
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.extend_from_slice(nalu);
            at += len;
            if at == sample.len() {
                return Some(());
            }
        }
    }
}

/// How many decoded pictures to hold before releasing the oldest, so that
/// B-pictures come out in display order. Real streams reorder within a GOP;
/// four pictures is enough for the two-reference-deep case and adds only a
/// frame or two of latency, which the decode-ahead queue hides anyway.
const H264_REORDER_WINDOW: usize = 4;

/// H.264/AVC in Annex-B, via `rusty_h264-decoder`.
pub struct H264Decoder {
    decoder: rusty_h264_decoder::Decoder,
    avcc: AvcC,
    au: Vec<u8>,
    /// Presentation times of samples fed but not yet turned into a released
    /// picture (decode order, one entry per coded picture).
    pending: VecDeque<f64>,
    /// Decoded but unreleased pictures: `(poc, pts, picture)`, released in
    /// picture-order-count order.
    reorder: Vec<(i32, f64, Frame)>,
}

impl H264Decoder {
    fn new(params: &CodecParams) -> Result<Self, MediaError> {
        // A stream with no parameter sets in the container is fine only if the
        // samples carry in-band SPS/PPS; `avc1` in the wild always ships avcC.
        let avcc = params
            .codec_private
            .as_deref()
            .and_then(AvcC::parse)
            .unwrap_or(AvcC {
                length_size: 4,
                parameter_sets: Vec::new(),
            });
        Ok(H264Decoder {
            decoder: rusty_h264_decoder::Decoder::new(),
            avcc,
            au: Vec::new(),
            pending: VecDeque::new(),
            reorder: Vec::new(),
        })
    }

    fn release_oldest(&mut self, out: &mut Vec<Picture>) {
        while self.reorder.len() > H264_REORDER_WINDOW {
            // Smallest POC first within the window (a GOP never spans more
            // than the window in practice, so this is just display order).
            let index = self
                .reorder
                .iter()
                .enumerate()
                .min_by_key(|(_, (poc, _, _))| *poc)
                .map(|(index, _)| index)
                .unwrap_or(0);
            let (_, pts, frame) = self.reorder.swap_remove(index);
            out.push(Picture { pts, frame });
        }
    }
}

impl PictureDecoder for H264Decoder {
    fn decode(&mut self, sample: &[u8], pts: f64, out: &mut Vec<Picture>) -> Result<(), MediaError> {
        let codec = self.codec();
        self.pending.push_back(pts);
        self.avcc
            .annex_b(sample, &mut self.au)
            .ok_or_else(|| MediaError::Decode {
                codec: codec.label(),
                detail: "sample is not length-prefixed H.264".to_string(),
            })?;
        let au = std::mem::take(&mut self.au);
        let picture = self
            .decoder
            .decode(&au)
            .map_err(|err| MediaError::Decode {
                codec: codec.label(),
                detail: format!("{err:?}"),
            })?;
        if let Some(yuv) = picture {
            // The picture corresponds to the oldest sample fed that has not
            // produced output yet — decode order, one picture per AU.
            let pts = self.pending.pop_front().unwrap_or(pts);
            let frame = frame::yuv420_to_rgba(
                yuv.width,
                yuv.height,
                &yuv.y,
                yuv.width,
                &yuv.u,
                &yuv.v,
                yuv.width.div_ceil(2),
            )
            .ok_or_else(|| MediaError::Decode {
                codec: codec.label(),
                detail: "decoder returned truncated planes".to_string(),
            })?;
            self.reorder.push((self.decoder.last_poc(), pts, frame));
            self.release_oldest(out);
        }
        Ok(())
    }

    fn flush(&mut self, out: &mut Vec<Picture>) -> Result<(), MediaError> {
        let mut rest = std::mem::take(&mut self.reorder);
        // POC order; the pictures still queued are the tail of the last GOP.
        rest.sort_by_key(|(poc, _, _)| *poc);
        for (_, pts, frame) in rest {
            out.push(Picture { pts, frame });
        }
        Ok(())
    }

    fn codec(&self) -> VideoCodec {
        VideoCodec::H264
    }
}

// ---------------------------------------------------------------------------
// VP9

/// VP9 → RGBA8, handling 8/10/12-bit planes and 4:2:0 / 4:2:2 / 4:4:4
/// subsampling by nearest-neighbour chroma resampling.
///
/// A free function, not an inherent method: E0116 forbids `impl vp9dec::Frame`
/// on a foreign type.
fn vp9_to_rgba(frame: &vp9dec::Frame) -> Option<Frame> {
    let (w, h) = (frame.width as usize, frame.height as usize);
    if w == 0 || h == 0 {
        return None;
    }
    let plane8 = |data: &vp9dec::PlaneData| -> Vec<u8> {
        match data {
            vp9dec::PlaneData::U8(bytes) => bytes.clone(),
            // 10/12-bit streams: drop to 8 bits (the low bits are invisible
            // on a software-rendered, no-GPU display).
            vp9dec::PlaneData::U16(words) => {
                let shift = u8::from(frame.bit_depth > 8 && frame.bit_depth <= 12) as u32
                    * (frame.bit_depth as u32 - 8).min(8);
                words
                    .iter()
                    .map(|word| ((u32::from(*word) >> shift.max(1)) & 0xff) as u8)
                    .collect()
            }
        }
    };
    let (y, u, v) = (plane8(&frame.y), plane8(&frame.u), plane8(&frame.v));
    // subsampling_x/y are exponents: 0 = no subsampling, 1 = halved.
    let cw = if frame.subsampling_x == 1 {
        w.div_ceil(2)
    } else {
        w
    };
    let ch = if frame.subsampling_y == 1 {
        h.div_ceil(2)
    } else {
        h
    };
    if y.len() < w * h || u.len() < cw * ch || v.len() < cw * ch {
        return None;
    }
    let mut out = Frame::black(frame.width, frame.height);
    for row in 0..h {
        // Nearest-neighbour chroma: chroma row/col by proportional index.
        let crow = (row * ch) / h.max(1);
        let yrow = &y[row * w..row * w + w];
        let urow = &u[crow * cw..crow * cw + cw];
        let vrow = &v[crow * cw..crow * cw + cw];
        for col in 0..w {
            let ccol = (col * cw) / w.max(1);
            let [r, g, b] = yuv_to_rgb(yrow[col], urow[ccol], vrow[ccol]);
            let o = (row * w + col) * 4;
            out.rgba[o] = r;
            out.rgba[o + 1] = g;
            out.rgba[o + 2] = b;
            out.rgba[o + 3] = 255;
        }
    }
    Some(out)
}

#[inline(always)]
fn yuv_to_rgb(y: u8, u: u8, v: u8) -> [u8; 3] {
    let yy = (i32::from(y) - 16) * 298;
    let cu = i32::from(u) - 128;
    let cv = i32::from(v) - 128;
    let clamp = |v: i32| -> u8 { v.clamp(0, 255) as u8 };
    [
        clamp((yy + 409 * cv) >> 8),
        clamp((yy - 100 * cu - 208 * cv) >> 8),
        clamp((yy + 516 * cu) >> 8),
    ]
}

/// VP9 via `vp9dec` (clean-room, spec v0.7, no dependencies of its own).
///
/// VP9 carries no reordering: container order is display order. A "superframe"
/// chunk can pack a hidden reference frame plus the visible one, hence the
/// `Vec` return, and hidden frames (`show_frame == 0`) carry no picture.
#[derive(Default)]
pub struct Vp9Decoder {
    decoder: vp9dec::Decoder,
}

impl PictureDecoder for Vp9Decoder {
    fn decode(&mut self, sample: &[u8], pts: f64, out: &mut Vec<Picture>) -> Result<(), MediaError> {
        let decoded = self
            .decoder
            .decode_frame(sample)
            .map_err(|err| MediaError::Decode {
                codec: VideoCodec::Vp9.label().clone(),
                detail: format!("{err:?}"),
            })?;
        for packet in decoded {
            if let Some(picture) = packet.frame {
                let frame = vp9_to_rgba(&picture).ok_or_else(|| MediaError::Decode {
                    codec: "VP9".to_string(),
                    detail: "decoded frame planes were truncated".to_string(),
                })?;
                out.push(Picture { pts, frame });
            }
        }
        Ok(())
    }

    fn flush(&mut self, _out: &mut Vec<Picture>) -> Result<(), MediaError> {
        Ok(()) // nothing buffered: VP9 has no display reorder
    }

    fn codec(&self) -> VideoCodec {
        VideoCodec::Vp9
    }
}

// ---------------------------------------------------------------------------
// Motion JPEG

/// One JPEG sample per picture (AVI `MJPG`, QuickTime `jpeg`).
#[derive(Default)]
pub struct JpegDecoder;

impl PictureDecoder for JpegDecoder {
    fn decode(&mut self, sample: &[u8], pts: f64, out: &mut Vec<Picture>) -> Result<(), MediaError> {
        // jpeg-decoder 0.3: `Decoder::new(Read + Seek)`, `decode()` yields the
        // raw pixel bytes, `info()` the image descriptor. A sample is a byte
        // string, so hand the reader an in-memory Cursor.
        let mut decoder = jpeg_decoder::Decoder::new(std::io::Cursor::new(sample));
        let pixels = decoder.decode().map_err(|err| MediaError::Decode {
            codec: "Motion JPEG".to_string(),
            detail: err.to_string(),
        })?;
        let info = decoder.info().ok_or_else(|| MediaError::Decode {
            codec: "Motion JPEG".to_string(),
            detail: "no image descriptor (not a JPEG sample?)".to_string(),
        })?;
        let mut frame = Frame::black(u32::from(info.width), u32::from(info.height));
        match info.pixel_format {
            jpeg_decoder::PixelFormat::L8 => {
                for (src, dst) in pixels.iter().zip(frame.rgba.chunks_exact_mut(4)) {
                    let grey = *src;
                    dst[0] = grey;
                    dst[1] = grey;
                    dst[2] = grey;
                    dst[3] = 255;
                }
            }
            // L16 emits two bytes per pixel; keep the high byte (8-bit output,
            // consistent with the no-GPU 8-bit pipeline everywhere else).
            jpeg_decoder::PixelFormat::L16 => {
                for (src, dst) in pixels.chunks_exact(2).zip(frame.rgba.chunks_exact_mut(4)) {
                    let grey = src[1];
                    dst[0] = grey;
                    dst[1] = grey;
                    dst[2] = grey;
                    dst[3] = 255;
                }
            }
            jpeg_decoder::PixelFormat::RGB24 => {
                for (src, dst) in pixels.chunks_exact(3).zip(frame.rgba.chunks_exact_mut(4)) {
                    dst[0] = src[0];
                    dst[1] = src[1];
                    dst[2] = src[2];
                    dst[3] = 255;
                }
            }
            // CMYK/YCCK JPEGs (4 bytes/pixel): keep the first channel of each
            // pixel so the motion is still visible rather than failing the
            // whole clip. Last arm — with L8/L16/RGB24 covered above, CMYK32
            // is the only remaining format in jpeg-decoder 0.3.
            jpeg_decoder::PixelFormat::CMYK32 => {
                for (src, dst) in pixels.chunks_exact(4).zip(frame.rgba.chunks_exact_mut(4)) {
                    let grey = src[0];
                    dst[0] = grey;
                    dst[1] = grey;
                    dst[2] = grey;
                    dst[3] = 255;
                }
            }
        }
        out.push(Picture { pts, frame });
        Ok(())
    }

    fn flush(&mut self, _out: &mut Vec<Picture>) -> Result<(), MediaError> {
        Ok(())
    }

    fn codec(&self) -> VideoCodec {
        VideoCodec::Mjpeg
    }
}

// ---------------------------------------------------------------------------
// Uncompressed

/// Uncompressed AVI/ISOM samples: no decoding, just pixel-format conversion.
pub struct RawDecoder {
    format: RawFormat,
    width: usize,
    height: usize,
}

impl RawDecoder {
    fn new(format: RawFormat, width: usize, height: usize) -> Self {
        RawDecoder {
            format,
            width,
            height,
        }
    }
}

impl PictureDecoder for RawDecoder {
    fn decode(&mut self, sample: &[u8], pts: f64, out: &mut Vec<Picture>) -> Result<(), MediaError> {
        let codec = VideoCodec::Raw(self.format);
        // AVI bitmaps are bottom-up DIBs; everything else we decode is top-down.
        let converted = match self.format {
            RawFormat::Rgb24 => frame::rgb_to_rgba(self.width, self.height, sample, self.width * 3, 3, true, false),
            RawFormat::Rgba32 => frame::rgb_to_rgba(self.width, self.height, sample, self.width * 4, 4, true, false),
            RawFormat::Yuyv => frame::yuyv_to_rgba(self.width, self.height, sample, self.width * 2, false),
            RawFormat::Uyvy => frame::yuyv_to_rgba(self.width, self.height, sample, self.width * 2, true),
            RawFormat::I420 => {
                let cw = self.width.div_ceil(2);
                let y_len = self.width * self.height;
                let c_len = cw * self.height.div_ceil(2);
                if sample.len() >= y_len + 2 * c_len {
                    frame::yuv420_to_rgba(
                        self.width,
                        self.height,
                        &sample[..y_len],
                        self.width,
                        &sample[y_len..y_len + c_len],
                        &sample[y_len + c_len..y_len + 2 * c_len],
                        cw,
                    )
                } else {
                    None
                }
            }
        };
        let frame = converted.ok_or_else(|| MediaError::Decode {
            codec: codec.label(),
            detail: format!(
                "sample is {} B, too small for {}x{} {}",
                sample.len(),
                self.width,
                self.height,
                "uncompressed pixels"
            ),
        })?;
        out.push(Picture { pts, frame });
        Ok(())
    }

    fn flush(&mut self, _out: &mut Vec<Picture>) -> Result<(), MediaError> {
        Ok(())
    }

    fn codec(&self) -> VideoCodec {
        VideoCodec::Raw(self.format)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn container_names_resolve_to_codecs() {
        assert_eq!(
            VideoCodec::from_isom_fourcc(b"avc1"),
            VideoCodec::H264,
            "the codec the whole web ships"
        );
        assert_eq!(VideoCodec::from_isom_fourcc(b"vp09"), VideoCodec::Vp9);
        assert_eq!(VideoCodec::from_isom_fourcc(b"hvc1"), VideoCodec::Hevc);
        assert_eq!(
            VideoCodec::from_matroska_codec_id("V_MPEG4/ISO/AVC"),
            VideoCodec::H264
        );
        assert_eq!(VideoCodec::from_matroska_codec_id("V_VP9"), VideoCodec::Vp9);
        assert_eq!(VideoCodec::from_matroska_codec_id("V_VP8"), VideoCodec::Vp8);
        assert_eq!(VideoCodec::from_avi_fourcc(0), VideoCodec::Raw(RawFormat::Rgb24));
        assert_eq!(
            VideoCodec::from_avi_fourcc(u32::from_le_bytes(*b"YUY2")),
            VideoCodec::Raw(RawFormat::Yuyv)
        );
        assert_eq!(
            VideoCodec::from_avi_fourcc(u32::from_le_bytes(*b"MJPG")),
            VideoCodec::Mjpeg
        );
    }

    #[test]
    fn only_the_codecs_we_ship_are_declared_supported() {
        assert!(VideoCodec::H264.is_supported());
        assert!(VideoCodec::Vp9.is_supported());
        assert!(VideoCodec::Mjpeg.is_supported());
        assert!(VideoCodec::Raw(RawFormat::Yuyv).is_supported());
        // These need libvpx/libaom/openh264-class machinery we refuse to link.
        assert!(!VideoCodec::Vp8.is_supported());
        assert!(!VideoCodec::Av1.is_supported());
        assert!(!VideoCodec::Hevc.is_supported());
        assert!(VideoCodec::label(&VideoCodec::Vp8) == "VP8");
    }

    #[test]
    fn asking_for_a_codec_we_lack_says_so() {
        let params = CodecParams {
            codec: VideoCodec::Vp8,
            width: 320,
            height: 180,
            codec_private: None,
        };
        let err = make_decoder(&params).expect_err("VP8 must be refused");
        assert_eq!(
            err,
            MediaError::UnsupportedCodec {
                codec: "VP8".to_string()
            }
        );
        assert!(err.to_string().contains("no decoder for VP8"));
    }

    #[test]
    fn avcc_describes_length_prefix_and_parameter_sets() {
        // A minimal avcC: config version 1, profile 100, level 3.1, 4-byte
        // lengths, one 4-byte SPS (0x67 …) and one 3-byte PPS (0x68 …).
        let avcc = [
            1u8, 100, 0, 49, 0xff, 0xe1, // 111|numSPS = 1
            0x00, 0x04, 0x67, 0x42, 0x00, 0x1f, // SPS (4 B)
            0x01, // numPPS = 1
            0x00, 0x03, 0x68, 0xce, 0x38, // PPS (3 B)
        ];
        let parsed = AvcC::parse(&avcc).expect("parses");
        assert_eq!(parsed.length_size, 4);
        // Two parameter sets, each start-code prefixed.
        assert_eq!(parsed.parameter_sets, vec![
            0, 0, 0, 1, 0x67, 0x42, 0x00, 0x1f, 0, 0, 0, 1, 0x68, 0xce, 0x38
        ]);
    }

    #[test]
    fn length_prefixed_samples_become_annex_b_access_units() {
        let avcc = AvcC {
            length_size: 4,
            parameter_sets: vec![0, 0, 0, 1, 0x67, 0x01],
        };
        let sample = [0x00, 0x00, 0x00, 0x03, 0x41, 0xba, 0x10];
        let mut out = Vec::new();
        avcc.annex_b(&sample, &mut out).expect("converts");
        assert_eq!(
            out,
            vec![0, 0, 0, 1, 0x67, 0x01, // priming parameter sets
                 0, 0, 0, 1, 0x41, 0xba, 0x10]
        );
        // A sample that stops mid-NALU is corrupt, not silently truncated.
        assert!(avcc
            .annex_b(&[0x00, 0x00, 0x00, 0x09, 0x41], &mut out)
            .is_none());
    }

    #[test]
    fn raw_decoder_turns_bytes_into_a_frame() {
        let mut decoder = RawDecoder::new(RawFormat::Rgb24, 2, 1);
        let mut out = Vec::new();
        // Bottom-up 2x1 BGR image: blue then red.
        decoder
            .decode(&[255, 0, 0, 0, 0, 255], 0.0, &mut out)
            .expect("decodes");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].frame.pixel(0, 0), (0, 0, 255, 255));
        assert_eq!(out[0].frame.pixel(1, 0), (255, 0, 0, 255));
        assert_eq!(out[0].pts, 0.0);
        // Half the bytes needed: an honest decode error.
        let mut again = Vec::new();
        assert!(matches!(
            decoder.decode(&[0, 0, 0], 0.04, &mut again),
            Err(MediaError::Decode { .. })
        ));
    }

    #[test]
    fn h264_rejects_a_sample_that_is_not_length_prefixed() {
        let mut decoder = H264Decoder::new(&CodecParams {
            codec: VideoCodec::H264,
            width: 16,
            height: 16,
            codec_private: None,
        })
        .expect("builds without avcC (samples may carry parameter sets)");
        let mut out = Vec::new();
        let err = decoder.decode(&[0, 1, 2], 0.0, &mut out).expect_err("refused");
        assert!(matches!(err, MediaError::Decode { .. }), "{err}");
        assert!(err.to_string().contains("length-prefixed"));
    }
}
