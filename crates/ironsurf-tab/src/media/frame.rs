//! Decoded pictures: RGBA8 frames plus the colour conversions that produce them.
//!
//! Every decoder in [`crate::media::codec`] ends up here: planar 4:2:0
//! (H.264, VP9), packed 4:2:2 (raw AVI `YUY2`), BGR/BGRx and JPEG samples all
//! become a [`Frame`] of tightly packed RGBA8, which is what blitz and
//! `anyrender` want (`RasterImageData` layout), and what the headless tests
//! assert against.
//!
//! The colour matrix is BT.601 at the limited (video) range — what H.264 and
//! VP9 streams declare by default, and what every hardware player assumes for
//! standard definition. Software playback on a no-GPU box wants branch-light
//! fixed point rather than a floating-point multiply per channel, so the
//! coefficients below are the usual 8-bit approximations (298 ≈ 1.164 · 256).

/// One decoded picture: `width * height` pixels, RGBA8, straight alpha, no row
/// padding.
#[derive(Clone, PartialEq, Eq)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

impl core::fmt::Debug for Frame {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "Frame {}x{} ({} B rgba)",
            self.width,
            self.height,
            self.rgba.len()
        )
    }
}

impl Frame {
    /// A black, opaque frame.
    pub fn black(width: u32, height: u32) -> Self {
        Frame {
            width,
            height,
            rgba: vec![0u8; (width as usize) * (height as usize) * 4],
        }
    }

    /// Wrap caller-supplied RGBA8 bytes; `None` unless the length matches.
    pub fn from_rgba(width: u32, height: u32, rgba: Vec<u8>) -> Option<Self> {
        let expected = (width as usize) * (height as usize) * 4;
        (rgba.len() == expected).then_some(Frame {
            width,
            height,
            rgba,
        })
    }

    pub fn pixel_count(&self) -> usize {
        (self.width as usize) * (self.height as usize)
    }

    /// `(x, y)` pixel as `(r, g, b, a)`; out-of-range reads give transparent
    /// black rather than panicking (tests ask).
    pub fn pixel(&self, x: u32, y: u32) -> (u8, u8, u8, u8) {
        let w = self.width as usize;
        let (x, y) = (x as usize, y as usize);
        if x >= w || y >= self.height as usize {
            return (0, 0, 0, 0);
        }
        let i = (y * w + x) * 4;
        (
            self.rgba[i],
            self.rgba[i + 1],
            self.rgba[i + 2],
            self.rgba[i + 3],
        )
    }

    /// Paint every pixel one colour (the widget's poster/placeholder uses this).
    pub fn fill(&mut self, rgba: [u8; 4]) {
        for chunk in self.rgba.chunks_exact_mut(4) {
            chunk.copy_from_slice(&rgba);
        }
    }

    /// Average colour of the whole frame — enough to assert "this picture is
    /// mostly red" without depending on exact decoder output.
    pub fn average(&self) -> (u8, u8, u8) {
        let n = self.pixel_count().max(1);
        let (mut r, mut g, mut b) = (0u64, 0u64, 0u64);
        for px in self.rgba.chunks_exact(4) {
            r += u64::from(px[0]);
            g += u64::from(px[1]);
            b += u64::from(px[2]);
        }
        let n = n.max(1) as u64;
        (
            (r / n).min(255) as u8,
            (g / n).min(255) as u8,
            (b / n).min(255) as u8,
        )
    }

    /// Cheap fingerprint that changes when the picture changes.
    pub fn checksum(&self) -> u64 {
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for byte in self.rgba.iter().step_by(7).copied() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x100_0000_01b3);
        }
        hash
    }
}

#[inline(always)]
fn clamp8(v: i32) -> u8 {
    if v < 0 {
        0
    } else if v > 255 {
        255
    } else {
        v as u8
    }
}

/// Fixed-point BT.601 limited-range conversion of one (y, u, v) triple:
/// 1.1644·(Y−16) plus 1.5960·(V−128) / −0.3918·(U−128) − 0.8130·(V−128) /
/// +2.0172·(U−128), at 16-bit precision with rounding. The precise scale is
/// what makes Y=235 — white in video range — land on 255; the 8-bit
/// approximation (×298/256) stops one short at 254.
#[inline(always)]
fn yuv_to_rgb(y: u8, u: u8, v: u8) -> [u8; 3] {
    let yy = (i32::from(y) - 16) * 76_307;
    let cu = i32::from(u) - 128;
    let cv = i32::from(v) - 128;
    [
        clamp8((yy + 104_571 * cv + 32_768) >> 16),
        clamp8((yy - 25_674 * cu - 53_279 * cv + 32_768) >> 16),
        clamp8((yy + 132_201 * cu + 32_768) >> 16),
    ]
}

/// Planar 4:2:0 (`I420`) with row strides → RGBA8.
///
/// `u`/`v` are half size in both directions. A stride smaller than the plane
/// width is read as the plane width (decoders hand back tight planes).
pub fn yuv420_to_rgba(
    width: usize,
    height: usize,
    y: &[u8],
    stride_y: usize,
    u: &[u8],
    v: &[u8],
    stride_uv: usize,
) -> Option<Frame> {
    if width == 0 || height == 0 {
        return None;
    }
    let cw = width.div_ceil(2);
    let ch = height.div_ceil(2);
    let stride_y = stride_y.max(width);
    let stride_uv = stride_uv.max(cw);
    if y.len() < (height - 1) * stride_y + width
        || u.len() < (ch - 1) * stride_uv + cw
        || v.len() < (ch - 1) * stride_uv + cw
    {
        return None;
    }

    let mut frame = Frame::black(width as u32, height as u32);
    for row in 0..height {
        let yrow = &y[row * stride_y..row * stride_y + width];
        // Chroma is subsampled vertically: one chroma row per two luma rows.
        let crow = row / 2;
        let (urow, vrow) = (&u[crow * stride_uv..], &v[crow * stride_uv..]);
        let out_row = &mut frame.rgba[row * width * 4..(row + 1) * width * 4];
        for col in 0..width {
            let [r, g, b] = yuv_to_rgb(yrow[col], urow[col / 2], vrow[col / 2]);
            let o = col * 4;
            out_row[o] = r;
            out_row[o + 1] = g;
            out_row[o + 2] = b;
            out_row[o + 3] = 255;
        }
    }
    Some(frame)
}

/// Packed 4:2:2 (`YUYV` / `UYVY`) → RGBA8. Two luma samples share one (U, V).
pub fn yuyv_to_rgba(
    width: usize,
    height: usize,
    data: &[u8],
    stride: usize,
    uyvy: bool,
) -> Option<Frame> {
    if width == 0 || height == 0 {
        return None;
    }
    let stride = stride.max(width * 2);
    if data.len() < (height - 1) * stride + width * 2 {
        return None;
    }
    let mut frame = Frame::black(width as u32, height as u32);
    // A four-byte group is [Y0, Cb, Y1, Cr] (YUYV) or [Cb, Y0, Cr, Y1] (UYVY).
    for row in 0..height {
        let src = &data[row * stride..row * stride + width * 2];
        let out_row = &mut frame.rgba[row * width * 4..(row + 1) * width * 4];
        for group in 0..src.len() / 4 {
            let b = &src[group * 4..group * 4 + 4];
            let (u, y0, v, y1) = if uyvy { (b[0], b[1], b[2], b[3]) } else { (b[1], b[0], b[3], b[2]) };
            for (luma, out) in [(y0, 0usize), (y1, 1)] {
                let [r, g, bb] = yuv_to_rgb(luma, u, v);
                // Each group covers the two pixels at `group * 2` in the row.
                let o = (group * 2 + out) * 4;
                out_row[o] = r;
                out_row[o + 1] = g;
                out_row[o + 2] = bb;
                out_row[o + 3] = 255;
            }
        }
    }
    Some(frame)
}

/// Row-major RGB24 / BGR24 / RGBx / BGRx → RGBA8.
///
/// AVI DIBs are bottom-up (row 0 of the file is the bottom of the image), so
/// `top_down` says which way the rows run. `bgr` is the byte order the fourcc
/// implies (`RGB ` = B,G,R little-endian over a packed 24-bit pixel).
pub fn rgb_to_rgba(
    width: usize,
    height: usize,
    data: &[u8],
    stride: usize,
    bytes_per_pixel: usize,
    bgr: bool,
    top_down: bool,
) -> Option<Frame> {
    if width == 0 || height == 0 || !matches!(bytes_per_pixel, 3 | 4) {
        return None;
    }
    let stride = stride.max(width * bytes_per_pixel);
    if data.len() < (height - 1) * stride + width * bytes_per_pixel {
        return None;
    }
    let mut frame = Frame::black(width as u32, height as u32);
    for row in 0..height {
        let src_row = if top_down { row } else { height - 1 - row };
        let src = &data[src_row * stride..src_row * stride + width * bytes_per_pixel];
        let out_row = &mut frame.rgba[row * width * 4..(row + 1) * width * 4];
        for col in 0..width {
            let px = &src[col * bytes_per_pixel..];
            let (r, g, b) = if bgr {
                (px[2], px[1], px[0])
            } else {
                (px[0], px[1], px[2])
            };
            let o = col * 4;
            out_row[o] = r;
            out_row[o + 1] = g;
            out_row[o + 2] = b;
            out_row[o + 3] = 255;
        }
    }
    Some(frame)
}

/// Planar 4:4:4 (`I444` — a VP9 stream with no subsampling) → RGBA8.
pub fn yuv444_to_rgba(
    width: usize,
    height: usize,
    y: &[u8],
    u: &[u8],
    v: &[u8],
    stride: usize,
) -> Option<Frame> {
    if width == 0 || height == 0 {
        return None;
    }
    let stride = stride.max(width);
    let plane = (height - 1) * stride + width;
    if y.len() < plane || u.len() < plane || v.len() < plane {
        return None;
    }
    let mut frame = Frame::black(width as u32, height as u32);
    for row in 0..height {
        let i = row * stride;
        for col in 0..width {
            let [r, g, b] = yuv_to_rgb(y[i + col], u[i + col], v[i + col]);
            let o = (row * width + col) * 4;
            frame.rgba[o] = r;
            frame.rgba[o + 1] = g;
            frame.rgba[o + 2] = b;
            frame.rgba[o + 3] = 255;
        }
    }
    Some(frame)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yuv420_maps_the_video_range_endpoints_to_black_and_white() {
        // BT.601: Y=16 is black, Y=235 is white, chroma neutral at 128.
        let black = yuv420_to_rgba(2, 2, &[16; 4], 2, &[128; 1], &[128; 1], 1).expect("converts");
        assert_eq!(black.pixel(0, 0), (0, 0, 0, 255));
        let white = yuv420_to_rgba(2, 2, &[235; 4], 2, &[128; 1], &[128; 1], 1).expect("converts");
        assert_eq!(white.pixel(1, 1), (255, 255, 255, 255));
    }

    #[test]
    fn yuv420_shares_one_chroma_sample_between_four_pixels() {
        // Pure red in BT.601 video range is Y=76, Cb=85, Cr=255 (the earlier
        // fixture put white's luminance 235 with neutral Cb, which is a
        // magenta-tinted grey — no correct conversion makes its blue < 40).
        // The 2x2 pixel blocks sharing a chroma sample must be uniformly red.
        let red = yuv420_to_rgba(4, 2, &[76; 8], 4, &[85; 2], &[255; 2], 2).expect("converts");
        assert!(red.pixel(0, 0).0 > 250, "r = {}", red.pixel(0, 0).0);
        assert!(red.pixel(0, 0).2 < 40, "b = {}", red.pixel(0, 0).2);
        assert_eq!(red.pixel(0, 0), red.pixel(1, 1));
        assert_eq!(red.pixel(2, 1), red.pixel(3, 1));
    }

    #[test]
    fn yuv420_rejects_truncated_planes() {
        assert!(yuv420_to_rgba(8, 8, &[0; 8], 8, &[0; 1], &[0; 1], 4).is_none());
        assert!(yuv420_to_rgba(0, 4, &[], 0, &[], &[], 0).is_none());
    }

    #[test]
    fn yuyv_decodes_two_luma_samples_per_four_bytes() {
        // YUYV group: Y0=16 (black), Cb=128, Y1=235 (white), Cr=128.
        let f = yuyv_to_rgba(2, 1, &[16, 128, 235, 128], 4, false).expect("converts");
        assert_eq!(f.pixel(0, 0), (0, 0, 0, 255));
        assert_eq!(f.pixel(1, 0), (255, 255, 255, 255));
        // The same group in UYVY order is [Cb, Y0, Cr, Y1].
        let f = yuyv_to_rgba(2, 1, &[128, 16, 128, 235], 4, true).expect("converts");
        assert_eq!(f.pixel(0, 0), (0, 0, 0, 255));
        assert_eq!(f.pixel(1, 0), (255, 255, 255, 255));
    }

    #[test]
    fn rgb_and_bgr_orders_agree_and_rows_can_flip() {
        let rgb = rgb_to_rgba(1, 1, &[255, 0, 0], 3, 3, false, true).expect("converts");
        assert_eq!(rgb.pixel(0, 0), (255, 0, 0, 255));
        let bgr = rgb_to_rgba(1, 1, &[0, 0, 255], 3, 3, true, true).expect("converts");
        assert_eq!(bgr.pixel(0, 0), (255, 0, 0, 255));
        // Bottom-up (AVI DIB): the first byte row is the bottom of the image.
        let bottom_up =
            rgb_to_rgba(1, 2, &[0, 255, 0, 255, 0, 0], 3, 3, false, false).expect("converts");
        assert_eq!(bottom_up.pixel(0, 0), (255, 0, 0, 255));
        assert_eq!(bottom_up.pixel(0, 1), (0, 255, 0, 255));
    }

    #[test]
    fn yuv444_keeps_one_chroma_per_pixel() {
        // Two pixels, different chroma: 4:4:4 must not blur them together.
        // Dark luma (Y=76) with a high-Cb second pixel: 4:2:0-style sharing
        // or a U/V mix-up would make the two blues agree.
        let f = yuv444_to_rgba(
            2,
            1,
            &[76, 76],
            &[128, 240],
            &[128, 128],
            2,
        )
        .expect("converts");
        let neutral = f.pixel(0, 0);
        let blue = f.pixel(1, 0);
        // saturating: the channels themselves max at 255.
        assert!(blue.2 > neutral.2.saturating_add(60), "{blue:?} vs {neutral:?}");
    }

    #[test]
    fn frame_helpers_do_what_the_tests_need() {
        let mut f = Frame::black(4, 2);
        assert_eq!(f.rgba.len(), 32);
        assert_eq!(f.pixel_count(), 8);
        f.fill([1, 2, 3, 4]);
        assert_eq!(f.pixel(3, 1), (1, 2, 3, 4));
        assert_eq!(f.average(), (1, 2, 3));
        assert_eq!(f.pixel(9, 9), (0, 0, 0, 0));
        assert!(format!("{f:?}").contains("4x2"));
        let before = f.checksum();
        f.fill([9, 9, 9, 9]);
        assert_ne!(before, f.checksum());
        assert_eq!(Frame::from_rgba(1, 1, vec![0; 3]), None);
    }
}
