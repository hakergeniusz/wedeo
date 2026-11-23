// Portions derived from OxiMedia (https://github.com/cool-japan/oximedia),
// crates/oximedia-codec/src/util/limits.rs.
// Copyright 2024-2026 COOLJAPAN OU (Team Kitasan).
// Licensed under the Apache License, Version 2.0.
// See LICENSE-APACHE in this crate and CREDITS.md at the workspace root.
// Modified for Wedeo: returns wedeo_core::error::Result and forbids unsafe.

#![forbid(unsafe_code)]

//! Shared decode-safety limits (allocation and dimension ceilings).
//!
//! Image and video decoders size their output buffers from values taken
//! directly out of attacker-controlled headers. A handful of malformed header
//! bytes can therefore declare enormous dimensions or element counts, turning a
//! tiny input into either a multi-gigabyte allocation (OOM / DoS) or, when the
//! product overflows a 32-bit intermediate, an under-allocated buffer that is
//! then written out of bounds. These helpers centralise the ceiling and
//! checked-arithmetic guards so every decoder rejects such inputs uniformly.

use wedeo_core::error::{Error, Result};

fn err(message: impl Into<String>) -> Error {
    Error::Other(message.into())
}

/// Maximum accepted image dimension (width or height), in pixels.
pub const MAX_DIMENSION: usize = 16_384;

/// Maximum accepted size of a single decoded buffer, in bytes (4 GiB).
///
/// Stored as `u64` so `1 << 32` does not overflow at const-eval time on 32-bit
/// targets such as `wasm32-unknown-unknown`. Byte counts are widened to `u64`
/// at the comparison site in [`checked_dims`].
pub const MAX_ALLOC_BYTES: u64 = 1u64 << 32;

/// Validate a `width` x `height` image and return the element count
/// (`width * height * components`) computed with checked arithmetic.
///
/// Rejects zero dimensions, dimensions above [`MAX_DIMENSION`], zero
/// components, element-count or byte-size overflow, and byte sizes above
/// [`MAX_ALLOC_BYTES`]. `bytes_per` is used only for the memory ceiling.
pub fn checked_dims(
    width: usize,
    height: usize,
    components: usize,
    bytes_per: usize,
) -> Result<usize> {
    if width == 0 || height == 0 {
        return Err(err("zero image dimension"));
    }
    if width > MAX_DIMENSION || height > MAX_DIMENSION {
        return Err(err(format!(
            "image dimension {width}x{height} exceeds ceiling {MAX_DIMENSION}"
        )));
    }
    if components == 0 {
        return Err(err("zero component count"));
    }
    let elements = width
        .checked_mul(height)
        .and_then(|n| n.checked_mul(components))
        .ok_or_else(|| err("image element count overflow"))?;
    let bytes = elements
        .checked_mul(bytes_per.max(1))
        .ok_or_else(|| err("image byte size overflow"))?;
    if bytes as u64 > MAX_ALLOC_BYTES {
        return Err(err(format!(
            "declared image needs {bytes} bytes, exceeds ceiling {MAX_ALLOC_BYTES}"
        )));
    }
    Ok(elements)
}

/// Validate a `width` x `height` pair against the dimension ceiling only.
///
/// Use when the caller does its own element-count math but still needs to
/// reject a header declaring an impossibly large frame.
pub fn check_dimensions(width: usize, height: usize) -> Result<()> {
    if width == 0 || height == 0 {
        return Err(err("zero image dimension"));
    }
    if width > MAX_DIMENSION || height > MAX_DIMENSION {
        return Err(err(format!(
            "image dimension {width}x{height} exceeds ceiling {MAX_DIMENSION}"
        )));
    }
    Ok(())
}

/// Validate a declared element `count` against the input bytes actually
/// remaining, where each element needs at least `min_elem_bytes` of input.
///
/// Defends `Vec::with_capacity(count)` and `vec![_; count]` pre-allocations from
/// a tiny header declaring billions of entries the input cannot back.
/// Returns the validated `count` for convenience.
pub fn checked_count(count: usize, min_elem_bytes: usize, remaining: usize) -> Result<usize> {
    let need = count
        .checked_mul(min_elem_bytes.max(1))
        .ok_or_else(|| err("element count overflow"))?;
    if need > remaining {
        return Err(err(format!(
            "declared {count} elements need {need} bytes but only {remaining} remain"
        )));
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_reasonable_dims() {
        assert_eq!(checked_dims(1920, 1080, 4, 1).unwrap(), 1920 * 1080 * 4);
        assert_eq!(checked_dims(64, 64, 1, 2).unwrap(), 64 * 64);
    }

    #[test]
    fn rejects_zero_dims() {
        assert!(checked_dims(0, 16, 3, 1).is_err());
        assert!(checked_dims(16, 0, 3, 1).is_err());
        assert!(check_dimensions(0, 0).is_err());
    }

    #[test]
    fn rejects_oversized_dims() {
        assert!(checked_dims(MAX_DIMENSION + 1, 16, 3, 1).is_err());
        assert!(checked_dims(16, MAX_DIMENSION + 1, 3, 1).is_err());
        assert!(check_dimensions(MAX_DIMENSION + 1, 1).is_err());
    }

    #[test]
    fn rejects_zero_components() {
        assert!(checked_dims(16, 16, 0, 1).is_err());
    }

    #[test]
    fn rejects_alloc_bomb_within_dim_ceiling() {
        assert!(checked_dims(MAX_DIMENSION, MAX_DIMENSION, 8, 4).is_err());
    }

    #[test]
    fn checked_count_rejects_unbacked_declaration() {
        assert!(checked_count(1_000_000_000, 8, 16).is_err());
        assert!(checked_count(usize::MAX, 8, usize::MAX).is_err());
        assert_eq!(checked_count(2, 8, 64).unwrap(), 2);
    }
}
