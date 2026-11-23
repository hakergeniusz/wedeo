//! HEVC intra prediction (`libavcodec/hevc/pred_template.c`).
//!
//! All 35 prediction modes are supported for 4x4, 8x8, 16x16 and 32x32
//! transform blocks, for both luma and chroma. Samples are `u16` values at the
//! caller's bit depth. Reference sample availability and constrained intra
//! prediction are decided by the caller: a missing neighbour is simply passed
//! as `None` to [`build_reference_samples`].

use crate::{DspError, MAX_TB_SIZE, Result, check_bit_depth, check_size, clip_pixel};

/// The 35 HEVC intra prediction modes (`enum IntraPredMode` in `hevc/ps.h`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntraPredMode {
    /// `INTRA_PLANAR` (mode 0).
    Planar,
    /// `INTRA_DC` (mode 1).
    DC,
    /// `INTRA_ANGULAR2` .. `INTRA_ANGULAR34` (modes 2 .. 34).
    Angular(u8),
}

impl TryFrom<u8> for IntraPredMode {
    type Error = DspError;

    fn try_from(mode: u8) -> Result<Self> {
        match mode {
            0 => Ok(Self::Planar),
            1 => Ok(Self::DC),
            2..=34 => Ok(Self::Angular(mode)),
            _ => Err(DspError::InvalidIntraMode(mode)),
        }
    }
}

impl From<IntraPredMode> for u8 {
    fn from(mode: IntraPredMode) -> Self {
        match mode {
            IntraPredMode::Planar => 0,
            IntraPredMode::DC => 1,
            IntraPredMode::Angular(m) => m,
        }
    }
}

/// Available reconstructed neighbours of a prediction block.
///
/// Each present slice holds exactly `size` samples (or fewer at a picture
/// edge, in which case the last sample is replicated, as FFmpeg does with
/// `EXTEND`). `top_left` is the sample at `(-1, -1)`.
#[derive(Debug, Clone, Copy, Default)]
pub struct IntraNeighbors<'a> {
    pub top_left: Option<u16>,
    pub top: Option<&'a [u16]>,
    pub top_right: Option<&'a [u16]>,
    pub left: Option<&'a [u16]>,
    pub bottom_left: Option<&'a [u16]>,
}

/// Reference samples in FFmpeg's layout: `top[0..2*size]`, `left[0..2*size]`
/// plus the shared corner sample `top[-1] == left[-1]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntraRefSamples {
    pub corner: u16,
    pub top: Vec<u16>,
    pub left: Vec<u16>,
}

/// Flags controlling the reference-sample smoothing filter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IntraFilterOptions {
    /// `sps->intra_smoothing_disabled_flag`.
    pub smoothing_disabled: bool,
    /// `sps->strong_intra_smoothing_enabled_flag`.
    pub strong_smoothing: bool,
    /// `sps->chroma_format_idc`; 3 (4:4:4) also smooths chroma.
    pub chroma_format_idc: u8,
}

impl Default for IntraFilterOptions {
    fn default() -> Self {
        Self {
            smoothing_disabled: false,
            strong_smoothing: false,
            chroma_format_idc: 1,
        }
    }
}

/// Infer the unavailable reference samples, following the
/// "Infer the unavailable samples" block of `FUNC(intra_pred)`.
pub fn build_reference_samples(
    log2_size: u8,
    neighbors: IntraNeighbors<'_>,
    bit_depth: u8,
) -> Result<IntraRefSamples> {
    let size = check_size(log2_size)?;
    check_bit_depth(bit_depth)?;

    let mut refs = IntraRefSamples {
        corner: neighbors.top_left.unwrap_or(1 << (bit_depth - 1)),
        top: vec![refs_default(bit_depth); 2 * size],
        left: vec![refs_default(bit_depth); 2 * size],
    };

    let cand_up_left = neighbors.top_left.is_some();
    let cand_up = neighbors.top.is_some();
    let cand_up_right = neighbors.top_right.is_some();
    let cand_left = neighbors.left.is_some();
    let cand_bottom_left = neighbors.bottom_left.is_some();

    if let Some(top) = neighbors.top {
        extend(&mut refs.top, 0, top, bit_depth);
    }
    if let Some(top_right) = neighbors.top_right {
        extend(&mut refs.top, size, top_right, bit_depth);
    }
    if let Some(left) = neighbors.left {
        extend(&mut refs.left, 0, left, bit_depth);
    }
    if let Some(bottom_left) = neighbors.bottom_left {
        extend(&mut refs.left, size, bottom_left, bit_depth);
    }

    let extend_range = |buf: &mut [u16], start: usize, len: usize, value: u16| {
        for v in buf.iter_mut().take(start + len).skip(start) {
            *v = value;
        }
    };

    if !cand_bottom_left {
        if cand_left {
            let last = refs.left[size - 1];
            extend_range(&mut refs.left, size, size, last);
        } else if cand_up_left {
            let corner = refs.corner;
            extend_range(&mut refs.left, 0, 2 * size, corner);
        } else if cand_up {
            refs.corner = refs.top[0];
            let corner = refs.corner;
            extend_range(&mut refs.left, 0, 2 * size, corner);
        } else if cand_up_right {
            let v = refs.top[size];
            extend_range(&mut refs.top, 0, size, v);
            refs.corner = v;
            extend_range(&mut refs.left, 0, 2 * size, v);
        } else {
            let v = 1 << (bit_depth - 1);
            refs.corner = v;
            extend_range(&mut refs.top, 0, 2 * size, v);
            extend_range(&mut refs.left, 0, 2 * size, v);
        }
    }
    if !cand_left {
        let v = refs.left[size];
        extend_range(&mut refs.left, 0, size, v);
    }
    if !cand_up_left {
        refs.corner = refs.left[0];
    }
    if !cand_up {
        let corner = refs.corner;
        extend_range(&mut refs.top, 0, size, corner);
    }
    if !cand_up_right {
        let v = refs.top[size - 1];
        extend_range(&mut refs.top, size, size, v);
    }

    Ok(refs)
}

#[inline]
fn refs_default(bit_depth: u8) -> u16 {
    1 << (bit_depth - 1)
}

/// `EXTEND`: copy `src`, then replicate its last sample up to `size` entries.
fn extend(dst: &mut [u16], start: usize, src: &[u16], bit_depth: u8) {
    let size = dst.len() / 2;
    let n = src.len().min(size);
    dst[start..start + n].copy_from_slice(&src[..n]);
    if n < size && n > 0 {
        let last = src[n - 1];
        for i in n..size {
            dst[start + i] = last;
        }
    } else if n == 0 {
        let v = refs_default(bit_depth);
        for i in 0..size {
            dst[start + i] = v;
        }
    }
}

/// 3-tap reference-sample filter (`FUNC(ref_filter_3tap)`).
///
/// `left`/`top` contain `2 * size` samples; `corner` is the shared `[-1]`
/// sample. The filtered arrays replace the input arrays.
pub fn ref_filter_3tap(corner: u16, left: &mut [u16], top: &mut [u16]) -> Result<u16> {
    let size = left.len() / 2;
    if size == 0 || size > MAX_TB_SIZE || top.len() != left.len() {
        return Err(DspError::BufferTooSmall {
            what: "reference samples",
            need: 2,
            have: left.len().min(top.len()),
        });
    }
    let n = 2 * size;
    let mut fl = vec![0u16; n];
    let mut ft = vec![0u16; n];
    fl[n - 1] = left[n - 1];
    ft[n - 1] = top[n - 1];
    for i in (0..n - 1).rev() {
        let lp = if i == 0 {
            u32::from(corner)
        } else {
            u32::from(left[i - 1])
        };
        fl[i] = ((u32::from(left[i + 1]) + 2 * u32::from(left[i]) + lp + 2) >> 2) as u16;
        let tp = if i == 0 {
            u32::from(corner)
        } else {
            u32::from(top[i - 1])
        };
        ft[i] = ((u32::from(top[i + 1]) + 2 * u32::from(top[i]) + tp + 2) >> 2) as u16;
    }
    let new_corner =
        ((u32::from(left[0]) + 2 * u32::from(corner) + u32::from(top[0]) + 2) >> 2) as u16;
    left.copy_from_slice(&fl);
    top.copy_from_slice(&ft);
    Ok(new_corner)
}

/// Strong 4:2:0 8x8 reference smoothing (`FUNC(ref_filter_strong)`).
///
/// Only the first 64 samples are used, exactly as in FFmpeg's 32x32 path.
pub fn ref_filter_strong(corner: u16, left: &mut [u16], top: &mut [u16]) -> u16 {
    for i in 0..63 {
        top[i] = ((((64 - (i + 1)) as u32) * u32::from(corner)
            + (i + 1) as u32 * u32::from(top[63])
            + 32)
            >> 6) as u16;
    }
    for i in 0..63 {
        left[i] = ((((64 - (i + 1)) as u32) * u32::from(corner)
            + (i + 1) as u32 * u32::from(left[63])
            + 32)
            >> 6) as u16;
    }
    corner
}

/// Angular mode direction table from `FUNC(pred_angular)`, indexed by
/// `mode - 2`.
pub const INTRA_PRED_ANGLE: [i32; 33] = [
    32, 26, 21, 17, 13, 9, 5, 2, 0, -2, -5, -9, -13, -17, -21, -26, -32, -26, -21, -17, -13, -9,
    -5, -2, 0, 2, 5, 9, 13, 17, 21, 26, 32,
];

/// Inverse-angle table from `FUNC(pred_angular)`, indexed by `mode - 11`.
pub const INV_ANGLE: [i32; 15] = [
    -4096, -1638, -910, -630, -482, -390, -315, -256, -315, -390, -482, -630, -910, -1638, -4096,
];

/// Apply the reference-sample smoothing decision from `FUNC(intra_pred)`.
pub fn filter_reference_samples(
    refs: &mut IntraRefSamples,
    log2_size: u8,
    mode: IntraPredMode,
    c_idx: usize,
    bit_depth: u8,
    options: IntraFilterOptions,
) -> Result<()> {
    let size = check_size(log2_size)?;
    check_bit_depth(bit_depth)?;
    if options.smoothing_disabled || (c_idx != 0 && options.chroma_format_idc != 3) {
        return Ok(());
    }
    if mode == IntraPredMode::DC || size == 4 {
        return Ok(());
    }
    let mode_byte = u8::from(mode);
    let min_dist = (i32::from(mode_byte) - 26)
        .abs()
        .min((i32::from(mode_byte) - 10).abs());
    let thresholds = [7, 1, 0];
    if min_dist <= thresholds[usize::from(log2_size - 3)] {
        return Ok(());
    }
    let threshold = 1 << (bit_depth - 5);
    let strong = options.strong_smoothing
        && c_idx == 0
        && log2_size == 5
        && (i32::from(refs.corner) + i32::from(refs.top[63]) - 2 * i32::from(refs.top[31])).abs()
            < threshold
        && (i32::from(refs.corner) + i32::from(refs.left[63]) - 2 * i32::from(refs.left[31])).abs()
            < threshold;
    if strong {
        refs.corner = ref_filter_strong(refs.corner, &mut refs.left, &mut refs.top);
    } else {
        refs.corner = ref_filter_3tap(refs.corner, &mut refs.left, &mut refs.top)?;
    }
    Ok(())
}

/// Generate a prediction block from already-inferred reference samples.
///
/// `dst` must hold at least `size * dst_stride` samples; `log2_size` is 2..=5.
/// This is the `switch (mode)` at the end of `FUNC(intra_pred)` plus the
/// single-pass `pred_planar` / `pred_dc` / `pred_angular` kernels.
pub fn intra_predict(
    dst: &mut [u16],
    dst_stride: usize,
    log2_size: u8,
    mode: IntraPredMode,
    c_idx: usize,
    refs: &IntraRefSamples,
    bit_depth: u8,
) -> Result<()> {
    let size = check_size(log2_size)?;
    check_bit_depth(bit_depth)?;
    if dst.len() < size * dst_stride {
        return Err(DspError::BufferTooSmall {
            what: "intra prediction destination",
            need: size * dst_stride,
            have: dst.len(),
        });
    }
    if refs.top.len() < 2 * size || refs.left.len() < 2 * size {
        return Err(DspError::BufferTooSmall {
            what: "intra reference samples",
            need: 2 * size,
            have: refs.top.len().min(refs.left.len()),
        });
    }
    match mode {
        IntraPredMode::Planar => pred_planar(dst, dst_stride, log2_size, refs),
        IntraPredMode::DC => pred_dc(dst, dst_stride, log2_size, c_idx, refs),
        IntraPredMode::Angular(mode_byte) => pred_angular(
            dst, dst_stride, log2_size, c_idx, mode_byte, refs, bit_depth,
        )?,
    }
    Ok(())
}

/// Convenience wrapper: infer references, smooth them and predict in one call.
// All eight parameters are needed for geometry, neighbours and filter flags.
#[allow(clippy::too_many_arguments)]
pub fn intra_predict_with_neighbors(
    dst: &mut [u16],
    dst_stride: usize,
    log2_size: u8,
    mode: IntraPredMode,
    c_idx: usize,
    bit_depth: u8,
    neighbors: IntraNeighbors<'_>,
    options: IntraFilterOptions,
) -> Result<()> {
    let mut refs = build_reference_samples(log2_size, neighbors, bit_depth)?;
    filter_reference_samples(&mut refs, log2_size, mode, c_idx, bit_depth, options)?;
    intra_predict(dst, dst_stride, log2_size, mode, c_idx, &refs, bit_depth)
}

/// `FUNC(pred_planar)`: bilateral interpolation between the four edges.
fn pred_planar(dst: &mut [u16], stride: usize, log2_size: u8, refs: &IntraRefSamples) {
    let size = 1usize << log2_size;
    for y in 0..size {
        for x in 0..size {
            let v = ((size - 1 - x) as i32 * i32::from(refs.left[y])
                + (x + 1) as i32 * i32::from(refs.top[size])
                + (size - 1 - y) as i32 * i32::from(refs.top[x])
                + (y + 1) as i32 * i32::from(refs.left[size])
                + size as i32)
                >> (log2_size + 1);
            dst[x + y * stride] = v as u16;
        }
    }
}

/// `FUNC(pred_dc)` with the luma first-row/first-column smoothing.
fn pred_dc(dst: &mut [u16], stride: usize, log2_size: u8, c_idx: usize, refs: &IntraRefSamples) {
    let size = 1usize << log2_size;
    let mut dc = size as i32;
    for i in 0..size {
        dc += i32::from(refs.left[i]) + i32::from(refs.top[i]);
    }
    dc >>= log2_size + 1;
    for y in 0..size {
        for x in 0..size {
            dst[x + y * stride] = dc as u16;
        }
    }
    if c_idx == 0 && size < 32 {
        dst[0] = ((i32::from(refs.left[0]) + 2 * dc + i32::from(refs.top[0]) + 2) >> 2) as u16;
        for x in 1..size {
            dst[x] = ((i32::from(refs.top[x]) + 3 * dc + 2) >> 2) as u16;
        }
        for y in 1..size {
            dst[y * stride] = ((i32::from(refs.left[y]) + 3 * dc + 2) >> 2) as u16;
        }
    }
}

/// `FUNC(pred_angular)`: all modes 2..=34, including the negative-angle
/// reference extension and the luma mode 10 / mode 26 edge adjustments.
fn pred_angular(
    dst: &mut [u16],
    stride: usize,
    log2_size: u8,
    c_idx: usize,
    mode: u8,
    refs: &IntraRefSamples,
    bit_depth: u8,
) -> Result<()> {
    if !(2..=34).contains(&mode) {
        return Err(DspError::InvalidIntraMode(mode));
    }
    let size = 1usize << log2_size;
    let angle = INTRA_PRED_ANGLE[usize::from(mode - 2)];
    let last = (size as i32 * angle) >> 5;

    // FFmpeg's ref_array[3 * MAX_TB_SIZE + 4] with `ref_tmp = ref_array + size`.
    let mut ref_array: Vec<i32> = vec![0; 3 * MAX_TB_SIZE + 4];
    let base = size;
    let set_ref = |v: i32| clip_pixel(v, bit_depth);

    if mode >= 18 {
        // FFmpeg points `ref` at `top - 1` (or `left - 1`) over the full
        // 2 * size reference array, not just the first size samples.
        for x in 0..=2 * size {
            let v = if x == 0 { refs.corner } else { refs.top[x - 1] };
            ref_array[base + x] = i32::from(v);
        }
        if angle < 0 && last < -1 {
            let inv = INV_ANGLE[usize::from(mode - 11)];
            for x in last..=-1 {
                let idx = ((x * inv + 128) >> 8) - 1;
                let v = if idx == -1 {
                    refs.corner
                } else {
                    refs.left[idx as usize]
                };
                ref_array[(base as i32 + x) as usize] = i32::from(v);
            }
        }
        let get = |k: i32| -> i32 { ref_array[(base as i32 + k) as usize] };
        for y in 0..size {
            let idx = ((y as i32 + 1) * angle) >> 5;
            let fact = ((y as i32 + 1) * angle) & 31;
            for x in 0..size {
                let k = x as i32 + idx + 1;
                let v = ((32 - fact) * get(k) + fact * get(k + 1) + 16) >> 5;
                dst[x + y * stride] = set_ref(v);
            }
        }
        if mode == 26 && c_idx == 0 && size < 32 {
            for y in 0..size {
                let v = i32::from(refs.top[0])
                    + ((i32::from(refs.left[y]) - i32::from(refs.corner)) >> 1);
                dst[y * stride] = clip_pixel(v, bit_depth);
            }
        }
    } else {
        for x in 0..=2 * size {
            let v = if x == 0 {
                refs.corner
            } else {
                refs.left[x - 1]
            };
            ref_array[base + x] = i32::from(v);
        }
        if angle < 0 && last < -1 {
            let inv = INV_ANGLE[usize::from(mode - 11)];
            for x in last..=-1 {
                let idx = ((x * inv + 128) >> 8) - 1;
                let v = if idx == -1 {
                    refs.corner
                } else {
                    refs.top[idx as usize]
                };
                ref_array[(base as i32 + x) as usize] = i32::from(v);
            }
        }
        let get = |k: i32| -> i32 { ref_array[(base as i32 + k) as usize] };
        for x in 0..size {
            let idx = ((x as i32 + 1) * angle) >> 5;
            let fact = ((x as i32 + 1) * angle) & 31;
            for y in 0..size {
                let k = y as i32 + idx + 1;
                let v = ((32 - fact) * get(k) + fact * get(k + 1) + 16) >> 5;
                dst[x + y * stride] = set_ref(v);
            }
        }
        if mode == 10 && c_idx == 0 && size < 32 {
            for x in 0..size {
                let v = i32::from(refs.left[0])
                    + ((i32::from(refs.top[x]) - i32::from(refs.corner)) >> 1);
                dst[x] = clip_pixel(v, bit_depth);
            }
        }
    }
    Ok(())
}
