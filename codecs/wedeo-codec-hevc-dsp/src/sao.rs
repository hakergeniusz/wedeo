//! Sample adaptive offset (`libavcodec/h26x/h2656_sao_template.c`,
//! `libavcodec/hevc/filter.c`, `libavcodec/hevc/hevcdec.c`).
//!
//! Band offset (BO) and all four edge offset (EO) classes are implemented for
//! luma and both chroma components. The edge filter expects a source picture
//! with one sample of padding on every side (FFmpeg stores it in
//! `edge_emu_buffer`); the padding is read but never written.

use crate::{DspError, Result, check_bit_depth, clip_pixel};

/// `enum SAOType` from `libavcodec/hevc/hevcdec.h`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SaoType {
    NotApplied = 0,
    Band = 1,
    Edge = 2,
    Applied = 3,
}

impl TryFrom<u8> for SaoType {
    type Error = DspError;

    fn try_from(v: u8) -> Result<Self> {
        match v {
            0 => Ok(Self::NotApplied),
            1 => Ok(Self::Band),
            2 => Ok(Self::Edge),
            3 => Ok(Self::Applied),
            _ => Err(DspError::InvalidEoClass(v)),
        }
    }
}

/// `enum SAOEOClass` from `libavcodec/hevc/hevcdec.h`.
pub mod eo_class {
    pub const HORIZ: u8 = 0;
    pub const VERT: u8 = 1;
    pub const DIAG_135: u8 = 2;
    pub const DIAG_45: u8 = 3;
}

/// FFmpeg's `SAOParams` (`libavcodec/hevc/dsp.h`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SaoParams {
    pub offset_abs: [[i32; 4]; 3],
    pub offset_sign: [[i32; 4]; 3],
    pub band_position: [u8; 3],
    pub eo_class: [u8; 3],
    pub offset_val: [[i16; 5]; 3],
    pub type_idx: [SaoType; 3],
}

impl Default for SaoParams {
    fn default() -> Self {
        Self {
            offset_abs: [[0; 4]; 3],
            offset_sign: [[0; 4]; 3],
            band_position: [0; 3],
            eo_class: [0; 3],
            offset_val: [[0; 5]; 3],
            type_idx: [SaoType::NotApplied; 3],
        }
    }
}

impl SaoParams {
    /// Derive `offset_val[c_idx]` from the signalled syntax elements
    /// (`hevcdec.c`, "Inferred parameters" in `sao_parameters()`).
    pub fn compute_offset_vals(&mut self, c_idx: usize, log2_sao_offset_scale: u8) -> Result<()> {
        if c_idx > 2 {
            return Err(DspError::OutOfBounds {
                what: "SAO component index",
                index: c_idx,
                len: 3,
            });
        }
        self.offset_val[c_idx][0] = 0;
        for i in 0..4 {
            let mut v = self.offset_abs[c_idx][i];
            if self.type_idx[c_idx] == SaoType::Edge {
                if i > 1 {
                    v = -v;
                }
            } else if self.offset_sign[c_idx][i] != 0 {
                v = -v;
            }
            self.offset_val[c_idx][i + 1] = (v << log2_sao_offset_scale) as i16;
        }
        Ok(())
    }
}

/// `FUNC(sao_band_filter)`: add the four band offsets selected by
/// `(sample >> (bit_depth - 5)) & 31`.
#[allow(clippy::too_many_arguments)] // mirrors FFmpeg's sao_band_filter
pub fn sao_band_filter(
    dst: &mut [u16],
    dst_stride: usize,
    src: &[u16],
    src_stride: usize,
    offsets: &[i16; 5],
    band_position: u8,
    width: usize,
    height: usize,
    bit_depth: u8,
) -> Result<()> {
    check_bit_depth(bit_depth)?;
    if width == 0 || height == 0 || dst_stride < width || src_stride < width {
        return Err(DspError::BufferTooSmall {
            what: "SAO band block",
            need: width,
            have: dst_stride.min(src_stride),
        });
    }
    if dst.len() < height * dst_stride || src.len() < height * src_stride {
        return Err(DspError::BufferTooSmall {
            what: "SAO band buffers",
            need: height * dst_stride.max(src_stride),
            have: dst.len().min(src.len()),
        });
    }
    let shift = bit_depth - 5;
    let mut offset_table = [0i32; 32];
    for k in 0..4 {
        offset_table[(k + usize::from(band_position)) & 31] = i32::from(offsets[k + 1]);
    }
    for y in 0..height {
        for x in 0..width {
            let v = i32::from(src[x + y * src_stride])
                + offset_table[(usize::from(src[x + y * src_stride] >> shift)) & 31];
            dst[x + y * dst_stride] = clip_pixel(v, bit_depth);
        }
    }
    Ok(())
}

/// `CMP(a, b)` from `h2656_sao_template.c`.
#[inline]
fn cmp(a: i32, b: i32) -> i32 {
    i32::from(a > b) - i32::from(a < b)
}

/// `FUNC(sao_edge_filter)`: EO offset classification from the two neighbours
/// selected by `eo_class`. `src_stride` is the padded source stride.
#[allow(clippy::too_many_arguments)] // mirrors FFmpeg's sao_edge_filter
pub fn sao_edge_filter(
    dst: &mut [u16],
    dst_stride: usize,
    src: &[u16],
    src_stride: usize,
    offsets: &[i16; 5],
    eo: u8,
    width: usize,
    height: usize,
    bit_depth: u8,
) -> Result<()> {
    check_bit_depth(bit_depth)?;
    if eo > 3 {
        return Err(DspError::InvalidEoClass(eo));
    }
    if height == 0 || width == 0 || dst_stride < width || src_stride < width + 2 {
        return Err(DspError::BufferTooSmall {
            what: "SAO edge block",
            need: width + 2,
            have: dst_stride.min(src_stride),
        });
    }
    if dst.len() < height * dst_stride || src.len() < (height + 2) * src_stride {
        return Err(DspError::BufferTooSmall {
            what: "SAO edge buffers",
            need: (height + 2) * src_stride,
            have: src.len(),
        });
    }
    // pos[4][2][2] from h2656_sao_template.c, expressed as flat strides.
    const OFFSETS: [[isize; 2]; 4] = [
        [-1, 1], // horizontal
        [0, 0],  // vertical (strides below)
        [-1, 1], // 135 degree
        [1, -1], // 45 degree
    ];
    const ROW_OFFSETS: [[isize; 2]; 4] = [
        [0, 0],  // horizontal
        [-1, 1], // vertical
        [-1, 1], // 135 degree
        [-1, 1], // 45 degree
    ];
    const EDGE_IDX: [usize; 5] = [1, 2, 0, 3, 4];
    let idx = usize::from(eo);
    let src_stride_isize = src_stride as isize;
    let a_stride = OFFSETS[idx][0] + ROW_OFFSETS[idx][0] * src_stride_isize;
    let b_stride = OFFSETS[idx][1] + ROW_OFFSETS[idx][1] * src_stride_isize;
    for y in 0..height {
        for x in 0..width {
            let base = 1 + x + (1 + y) * src_stride;
            let a = (base as isize + a_stride) as usize;
            let b = (base as isize + b_stride) as usize;
            let center = i32::from(src[base]);
            let diff0 = cmp(center, i32::from(src[a]));
            let diff1 = cmp(center, i32::from(src[b]));
            let offset_val = EDGE_IDX[(2 + diff0 + diff1) as usize];
            dst[x + y * dst_stride] =
                clip_pixel(center + i32::from(offsets[offset_val]), bit_depth);
        }
    }
    Ok(())
}

/// Borders of the CTB being filtered: left, top, right, bottom.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SaoBorders(pub [bool; 4]);

/// Edge flags of the four CTB corners/edges (left, top, right, bottom).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SaoEdgeFlags {
    pub vert: [bool; 2],
    pub horiz: [bool; 2],
    pub diag: [bool; 4],
}

/// `FUNC(sao_edge_restore_1)`: copies source samples back at CTU boundaries
/// where an EO offset must not be applied.
#[allow(clippy::too_many_arguments)] // mirrors FFmpeg's sao_edge_restore
pub fn sao_edge_restore(
    dst: &mut [u16],
    dst_stride: usize,
    src: &[u16],
    src_stride: usize,
    sao: &SaoParams,
    borders: SaoBorders,
    width: usize,
    height: usize,
    c_idx: usize,
    edges: SaoEdgeFlags,
    bit_depth: u8,
) -> Result<()> {
    check_bit_depth(bit_depth)?;
    if c_idx > 2 {
        return Err(DspError::OutOfBounds {
            what: "SAO component index",
            index: c_idx,
            len: 3,
        });
    }
    if width == 0 || height == 0 || dst_stride < width || src_stride < width {
        return Err(DspError::BufferTooSmall {
            what: "SAO restore block",
            need: width,
            have: dst_stride.min(src_stride),
        });
    }
    if dst.len() < height * dst_stride || src.len() < height * src_stride {
        return Err(DspError::BufferTooSmall {
            what: "SAO restore buffers",
            need: height * dst_stride.max(src_stride),
            have: dst.len().min(src.len()),
        });
    }
    let eo = sao.eo_class[c_idx];
    let offset = sao.offset_val[c_idx][0];
    let offset_val = |v: u16| clip_pixel(i32::from(v) + i32::from(offset), bit_depth);
    let mut init_x = 0usize;
    let mut init_y = 0usize;
    let mut w = width;
    let mut h = height;

    if eo != eo_class::VERT {
        if borders.0[0] {
            for y in 0..h {
                dst[y * dst_stride] = offset_val(src[y * src_stride]);
            }
            init_x = 1;
        }
        if borders.0[2] {
            let o = w - 1;
            for y in 0..h {
                dst[y * dst_stride + o] = offset_val(src[y * src_stride + o]);
            }
            w -= 1;
        }
    }
    if eo != eo_class::HORIZ {
        if borders.0[1] {
            for x in init_x..w {
                dst[x] = offset_val(src[x]);
            }
            init_y = 1;
        }
        if borders.0[3] {
            let dy = (h - 1) * dst_stride;
            let sy = (h - 1) * src_stride;
            for x in init_x..w {
                dst[dy + x] = offset_val(src[sy + x]);
            }
            h -= 1;
        }
    }

    let save_upper_left =
        !edges.diag[0] && eo == eo_class::DIAG_135 && !borders.0[0] && !borders.0[1];
    let save_upper_right =
        !edges.diag[1] && eo == eo_class::DIAG_45 && !borders.0[1] && !borders.0[2];
    let save_lower_right =
        !edges.diag[2] && eo == eo_class::DIAG_135 && !borders.0[2] && !borders.0[3];
    let save_lower_left =
        !edges.diag[3] && eo == eo_class::DIAG_45 && !borders.0[0] && !borders.0[3];

    if edges.vert[0] && eo != eo_class::VERT {
        for y in init_y + usize::from(save_upper_left)..h - usize::from(save_lower_left) {
            dst[y * dst_stride] = src[y * src_stride];
        }
    }
    if edges.vert[1] && eo != eo_class::VERT {
        for y in init_y + usize::from(save_upper_right)..h - usize::from(save_lower_right) {
            dst[y * dst_stride + w - 1] = src[y * src_stride + w - 1];
        }
    }
    if edges.horiz[0] && eo != eo_class::HORIZ {
        let range = init_x + usize::from(save_upper_left)..w - usize::from(save_upper_right);
        dst[range.clone()].copy_from_slice(&src[range]);
    }
    if edges.horiz[1] && eo != eo_class::HORIZ {
        let dy = (h - 1) * dst_stride;
        let sy = (h - 1) * src_stride;
        let range = init_x + usize::from(save_lower_left)..w - usize::from(save_lower_right);
        let d = range.start + dy..range.end + dy;
        let s = range.start + sy..range.end + sy;
        dst[d].copy_from_slice(&src[s]);
    }
    if edges.diag[0] && eo == eo_class::DIAG_135 {
        dst[0] = src[0];
    }
    if edges.diag[1] && eo == eo_class::DIAG_45 {
        dst[w - 1] = src[w - 1];
    }
    if edges.diag[2] && eo == eo_class::DIAG_135 {
        dst[(h - 1) * dst_stride + w - 1] = src[(h - 1) * src_stride + w - 1];
    }
    if edges.diag[3] && eo == eo_class::DIAG_45 {
        dst[(h - 1) * dst_stride] = src[(h - 1) * src_stride];
    }
    Ok(())
}
