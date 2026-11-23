//! Typed-pixel abstraction shared by the H.264 reconstruction kernels.
//!
//! FFmpeg compiles the reconstruction templates once per `BIT_DEPTH`: the
//! `pixel` type is `uint8_t` for 8-bit and `uint16_t` for 9..14-bit, and
//! `av_clip_pixel()` clips to `(1 << BIT_DEPTH) - 1`. Wedeo mirrors that with
//! a small trait implemented by `u8` (8-bit) and `u16` (10-bit).
//!
//! Reference: FFmpeg `libavcodec/bit_depth_template.c`.

/// A native H.264 sample type.
pub trait Pixel: Copy + Default + PartialEq + Eq + std::fmt::Debug {
    /// Number of bits per sample (`BIT_DEPTH` in FFmpeg).
    const BITS: u8;
    /// Largest representable sample (`(1 << BITS) - 1`).
    const MAX: i32;
    /// Neutral chroma / unavailable-neighbor sample (`1 << (BITS - 1)`).
    const MID: i32;

    /// Widen a sample for intermediate arithmetic.
    fn to_i32(self) -> i32;

    /// Clip an intermediate value to the sample range.
    fn from_i32(v: i32) -> Self;

    /// Neutral sample for unavailable neighbors.
    #[inline(always)]
    fn mid() -> Self {
        Self::from_i32(Self::MID)
    }

    /// Clipped sum of two samples.
    #[inline(always)]
    fn add(self, other: i32) -> Self {
        Self::from_i32(self.to_i32() + other)
    }
}

impl Pixel for u8 {
    const BITS: u8 = 8;
    const MAX: i32 = 255;
    const MID: i32 = 128;

    #[inline(always)]
    fn to_i32(self) -> i32 {
        self as i32
    }

    #[inline(always)]
    fn from_i32(v: i32) -> Self {
        v.clamp(0, 255) as u8
    }
}

/// 10-bit samples are stored as native-endian `u16`, matching FFmpeg's
/// `uint16_t` pixel buffers. The clipping range is `0..=1023`.
impl Pixel for u16 {
    const BITS: u8 = 10;
    const MAX: i32 = 1023;
    const MID: i32 = 512;

    #[inline(always)]
    fn to_i32(self) -> i32 {
        self as i32
    }

    #[inline(always)]
    fn from_i32(v: i32) -> Self {
        v.clamp(0, 1023) as u16
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clip_ranges() {
        assert_eq!(u8::from_i32(-1), 0);
        assert_eq!(u8::from_i32(256), 255);
        assert_eq!(u16::from_i32(-1), 0);
        assert_eq!(u16::from_i32(1024), 1023);
        assert_eq!(u8::MID, 128);
        assert_eq!(u16::MID, 512);
    }
}
