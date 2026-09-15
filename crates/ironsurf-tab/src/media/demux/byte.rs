//! Byte-level readers shared by the container demuxers.
//!
//! Every parse is offset-based against the whole media file, because samples
//! are *ranges of the file*: the demuxer never copies a sample out of the
//! buffer, the decoder reads it in place. Everything here is checked — a
//! truncated or hostile file must produce [`MediaError::Corrupt`], never a
//! panic and never an out-of-bounds read.

use crate::media::MediaError;

/// Big-endian unsigned integer of 1/2/3/4/8 bytes (`mp4` and `ebml` sizes).
pub fn uint(bytes: &[u8], at: usize, width: usize) -> Option<u64> {
    let end = at.checked_add(width)?;
    let slice = bytes.get(at..end)?;
    let mut value: u64 = 0;
    for byte in slice {
        value = (value << 8) | u64::from(*byte);
    }
    Some(value)
}

pub fn u16(bytes: &[u8], at: usize) -> Option<u32> {
    uint(bytes, at, 2).map(|v| v as u32)
}

pub fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    uint(bytes, at, 4)
        .map(|v| u32::try_from(v).unwrap_or(u32::MAX))
}

pub fn i16(bytes: &[u8], at: usize) -> Option<i16> {
    uint(bytes, at, 2).map(|v| v as i16)
}

/// Big-endian IEEE-754 float32 (`Duration` in WebM, `tkhd` matrix terms use
/// 16.16 fixed point instead — see [`fixed16_16`]).
pub fn f32_at(bytes: &[u8], at: usize) -> Option<f32> {
    let bits = uint(bytes, at, 4)? as u32;
    // `f32::from_bits` accepts every bit pattern, so no validity check needed.
    Some(f32::from_bits(bits))
}

/// Big-endian IEEE-754 double (`mvhd`/`Duration` in some muxers).
pub fn f64_at(bytes: &[u8], at: usize) -> Option<f64> {
    let bits = uint(bytes, at, 8)?;
    Some(f64::from_bits(bits))
}

/// 16.16 fixed point as a float (`tkhd` display width/height).
pub fn fixed16_16(bytes: &[u8], at: usize) -> Option<f32> {
    uint(bytes, at, 4).map(|v| (v as i32) as f32 / 65536.0)
}

/// The one error every parser here returns on garbage:
/// `return Err(corrupt_msg("stsz has no entries"))`.
pub fn corrupt_msg(detail: impl Into<String>) -> MediaError {
    MediaError::Corrupt {
        detail: detail.into(),
    }
}

/// An offset/size pair checked against the buffer length.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Span {
    pub offset: usize,
    pub len: usize,
}

impl Span {
    pub fn new(offset: usize, len: usize) -> Self {
        Span { offset, len }
    }

    /// Borrow the range out of `data`, or `None` if it runs past the end.
    pub fn read<'a>(&self, data: &'a [u8]) -> Option<&'a [u8]> {
        let end = self.offset.checked_add(self.len)?;
        data.get(self.offset..end)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn big_endian_integers_of_every_width() {
        let bytes = [0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08];
        assert_eq!(uint(&bytes, 0, 1), Some(1));
        assert_eq!(u16(&bytes, 0), Some(0x0102));
        assert_eq!(u32_at(&bytes, 0), Some(0x0102_0304));
        assert_eq!(uint(&bytes, 0, 8), Some(0x0102_0304_0506_0708));
        assert_eq!(uint(&bytes, 4, 8), None, "reads past the end fail, never wrap");
        assert_eq!(uint(&bytes, 6, 4), None);
        assert_eq!(i16(&[0xff, 0xfe], 0), Some(-2));
    }

    #[test]
    fn floats_and_fixed_point_are_read_as_declared() {
        assert_eq!(f32_at(&1.5f32.to_be_bytes(), 0), Some(1.5));
        assert_eq!(f64_at(&2.25f64.to_be_bytes(), 0), Some(2.25));
        // tkhd stores 320.0 as 0x01400000 in 16.16 (320 << 16).
        assert_eq!(fixed16_16(&[0x01, 0x40, 0x00, 0x00], 0), Some(320.0));
    }

    #[test]
    fn spans_only_borrow_what_the_file_holds() {
        let data = b"0123456789";
        assert_eq!(Span::new(2, 3).read(data), Some(&b"234"[..]));
        assert_eq!(Span::new(8, 4).read(data), None);
        assert_eq!(Span::new(usize::MAX, 4).read(data), None);
    }
}
