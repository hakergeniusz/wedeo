//! HEVC CABAC (Context-Adaptive Binary Arithmetic Coding) decoder engine.
//!
//! FFmpeg reference: `libavcodec/hevc/cabac.c`, `libavcodec/cabac.c`,
//! `libavcodec/cabac_functions.h`, `libavcodec/cabac.h`.
//!
//! This crate is a *library* for the other HEVC work units. It ports the whole
//! arithmetic engine plus every CABAC-coded HEVC syntax element from
//! `hevc/cabac.c`, exposing a reusable [`Cabac`] struct with public context
//! models ([`Cabac::states`] / [`Cabac::stat_coeff`]) and `decode_*` methods.
//! It does not decode video on its own: reconstruction (inverse scans,
//! dequantisation, transforms and residual addition) remains the job of the
//! downstream HEVC decoder / DSP crates.
//!
//! All arithmetic uses `i32`/`u64` and `wrapping_*` where FFmpeg relies on C
//! overflow, with branchless masks (`>> 31`) matching the reference bit-for-bit.
//! No FFI, no bindgen, no c2rust.

use tracing::trace;
use wedeo_core::error::{Error, Result};

use inventory as _;
use wedeo_codec as _;

pub mod tables;

pub use tables::{
    CABAC_MAX_BIN, CTX_IDX_MAP, HEVC_CONTEXTS, HEVC_STAT_COEFFS, INIT_VALUES, LPS_RANGE,
    MLPS_STATE, NORM_SHIFT, ctx,
};
use wedeo_codec_hevc_ps::{SliceHeader, SliceType};

// ---------------------------------------------------------------------------
// Constants from libavcodec/cabac.h / cabac.c
// ---------------------------------------------------------------------------

const CABAC_BITS: u32 = 16;
const CABAC_MASK: i32 = (1 << CABAC_BITS) - 1;

/// `enum ScanType` from libavcodec/hevc/hevcdec.h.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum ScanType {
    #[default]
    Diag = 0,
    Horiz = 1,
    Vert = 2,
}

/// `enum PartMode` from libavcodec/hevc/hevcdec.h.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum PartMode {
    #[default]
    Part2Nx2N = 0,
    Part2NxN = 1,
    PartNx2N = 2,
    PartNxN = 3,
    Part2NxnU = 4,
    Part2NxnD = 5,
    PartNlx2N = 6,
    PartNrx2N = 7,
}

/// `enum InterPredIdc` from libavcodec/hevc/hevcdec.h.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum InterPredIdc {
    #[default]
    PredL0 = 0,
    PredL1 = 1,
    PredBi = 2,
}

/// SAO type returned by [`Cabac::decode_sao_type_idx`] (`enum SAOType`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum SaoType {
    #[default]
    NotApplied = 0,
    Band = 1,
    Edge = 2,
}

/// Inputs to [`Cabac::decode_residual_coefficients`] derived from SPS/PPS/CU.
///
/// The CABAC engine only needs these flags and dimensions; SPS/PPS dependent
/// storage lives in the caller so this crate stays reusable.
#[derive(Debug, Clone, Copy)]
pub struct ResidualParams {
    /// 0 = luma (Y), non-zero = chroma (Cb/Cr).
    pub c_idx: usize,
    /// `log2TrafoSize`, in [2, 5] (4x4 .. 32x32).
    pub log2_trafo_size: u32,
    /// Transform scan type.
    pub scan_idx: ScanType,
    /// SPS/PPS `transform_skip_enabled_flag`.
    pub transform_skip_enabled: bool,
    /// PPS `log2_max_transform_skip_block_size`.
    pub log2_max_transform_skip_block_size: u32,
    /// CU `cu_transquant_bypass_flag`.
    pub cu_transquant_bypass: bool,
    /// CU prediction mode is intra.
    pub is_intra: bool,
    /// SPS `transform_skip_context_enabled_flag`.
    pub transform_skip_context_enabled: bool,
    /// PPS `sign_data_hiding_enabled_flag`.
    pub sign_data_hiding_enabled: bool,
    /// SPS `persistent_rice_adaptation_enabled_flag`.
    pub persistent_rice_adaptation_enabled: bool,
    /// `implicit_rdpcm_enabled && (intra mode == 10 || intra mode == 26)`.
    pub implicit_rdpcm_active: bool,
    /// SPS `explicit_rdpcm_enabled_flag`.
    pub explicit_rdpcm_enabled: bool,
}

/// Information produced by [`Cabac::decode_residual_coefficients`] that the
/// downstream DSP stage needs (transform-skip, RDPCM, last coefficient, ...).
#[derive(Debug, Clone, Copy, Default)]
pub struct ResidualInfo {
    pub transform_skip_flag: u8,
    pub explicit_rdpcm_flag: u8,
    pub explicit_rdpcm_dir_flag: i32,
    pub last_x: i32,
    pub last_y: i32,
    pub max_xy: i32,
    pub col_limit: i32,
    pub num_coeff: i32,
}

// ---------------------------------------------------------------------------
// Free-function CABAC arithmetic primitives
// ---------------------------------------------------------------------------

/// Read a byte at `pos`, returning 0 past the end (FFmpeg pads slice RBSP with
/// `AV_INPUT_BUFFER_PADDING_SIZE` zero bytes, so out-of-range reads are zeros).
#[inline(always)]
fn byte_at(data: &[u8], pos: usize) -> i32 {
    data.get(pos).copied().unwrap_or(0) as i32
}

/// FFmpeg `refill`: refill the low register with 2 bytes when its low 16 bits
/// are all zero.
#[inline(always)]
fn refill(low: &mut i32, data: &[u8], pos: &mut usize) {
    *low += (byte_at(data, *pos) << 9) + (byte_at(data, *pos + 1) << 1);
    *low -= CABAC_MASK;
    // FFmpeg: only advance if bytestream < bytestream_end.
    if *pos < data.len() {
        *pos += CABAC_BITS as usize / 8;
    }
}

/// FFmpeg `renorm_cabac_decoder_once`, used by the terminate bin.
#[inline(always)]
fn renorm_once(range: &mut i32, low: &mut i32, data: &[u8], pos: &mut usize) {
    let shift = ((range.wrapping_sub(0x100) as u32) >> 31) as i32;
    *range <<= shift;
    *low <<= shift;
    if *low & CABAC_MASK == 0 {
        refill(low, data, pos);
    }
}

/// FFmpeg `refill2` (HAVE_FAST_CLZ path): refill using `ff_ctz(low)`.
#[inline(always)]
fn refill2(low: &mut i32, data: &[u8], pos: &mut usize) {
    // FFmpeg's HAVE_FAST_CLZ path: i = ff_ctz(low) - CABAC_BITS.
    let i = u32::from_ne_bytes(low.to_ne_bytes()).trailing_zeros() as i32 - CABAC_BITS as i32;
    let b0 = byte_at(data, *pos);
    let b1 = byte_at(data, *pos + 1);
    let x = -CABAC_MASK + (b0 << 9) + (b1 << 1);
    *low += x.wrapping_shl(i as u32);
    if *pos < data.len() {
        *pos += CABAC_BITS as usize / 8;
    }
}

/// FFmpeg `get_cabac_inline`: decode one regular (context-adaptive) bin.
#[inline]
fn decode_bin(low: &mut i32, range: &mut i32, state: &mut u8, data: &[u8], pos: &mut usize) -> u8 {
    let s = *state as i32;
    let range_lps = LPS_RANGE[(2 * (*range & 0xC0) + s) as usize] as i32;

    *range -= range_lps;
    let lps_mask = ((*range << (CABAC_BITS + 1)) - *low) >> 31;

    *low -= (*range << (CABAC_BITS + 1)) & lps_mask;
    *range += (range_lps - *range) & lps_mask;

    let s = s ^ lps_mask;
    *state = MLPS_STATE[(128 + s) as usize];
    let bit = s & 1;

    let shift = NORM_SHIFT[*range as usize] as i32;
    *range <<= shift;
    *low <<= shift;
    if *low & CABAC_MASK == 0 {
        refill2(low, data, pos);
    }

    bit as u8
}

/// FFmpeg `get_cabac_bypass`: decode one equiprobable (bypass) bin.
#[inline]
fn decode_bypass(low: &mut i32, range: &mut i32, data: &[u8], pos: &mut usize) -> u8 {
    // FFmpeg relies on C i32 wraparound here; debug builds must not panic.
    *low = low.wrapping_add(*low);
    if *low & CABAC_MASK == 0 {
        refill(low, data, pos);
    }
    let r = range.wrapping_shl(CABAC_BITS + 1);
    if *low < r {
        0
    } else {
        *low = low.wrapping_sub(r);
        1
    }
}

/// A run of bypass bins is the quotient of the offset by `range << 17`.
/// FFmpeg `cabac_bypass_bits`.
fn bypass_bits(low: &mut i32, range: &mut i32, data: &[u8], pos: &mut usize, n: usize) -> i32 {
    let avail = CABAC_BITS as i32 - u32::from_ne_bytes(low.to_ne_bytes()).trailing_zeros() as i32;
    let r = (*range as u64) << (CABAC_BITS + 1);
    let mut bytestream = *pos;
    let x: u64;

    if (n as i32) < avail {
        x = (*low as u64) << n;
    } else {
        // fill = (bytestream[0] << 9) + (bytestream[1] << 1) - CABAC_MASK
        let fill =
            (byte_at(data, bytestream) << 9) + (byte_at(data, bytestream + 1) << 1) - CABAC_MASK;
        x = ((((*low as i64).wrapping_shl(avail.max(0) as u32)) + fill as i64) as u64)
            .wrapping_shl((n as i32 - avail).max(0) as u32);
        if bytestream < data.len() {
            bytestream += CABAC_BITS as usize / 8;
        }
    }

    if x < (r << n) {
        *low = (x % r) as i32;
        *pos = bytestream;
        (x / r) as i32
    } else {
        let mut ret = 0;
        for _ in 0..n {
            ret = (ret << 1) | decode_bypass(low, range, data, pos) as i32;
        }
        ret
    }
}

/// Leading ones of the quotient, consumed only up to the first zero.
/// FFmpeg `cabac_unary_prefix`.
fn unary_prefix(low: &mut i32, range: &mut i32, data: &[u8], pos: &mut usize, max: usize) -> usize {
    let mut prefix = 0;
    while prefix < max {
        let avail =
            CABAC_BITS as i32 - u32::from_ne_bytes(low.to_ne_bytes()).trailing_zeros() as i32;
        let n = (avail - 1).min((max - prefix) as i32);
        if n < 1 {
            if decode_bypass(low, range, data, pos) == 0 {
                return prefix;
            }
            prefix += 1;
            continue;
        }
        let r = (*range as u64) << (CABAC_BITS + 1);
        let x = (*low as u64) << n;
        if x >= (r << n) {
            if decode_bypass(low, range, data, pos) == 0 {
                return prefix;
            }
            prefix += 1;
            continue;
        }

        let q = (x / r) as i32;
        let t = !q & ((1i32 << n) - 1);
        if t != 0 {
            // av_log2(t) == 31 - clz(t) for t > 0
            let log2t = 31 - (t as u32).leading_zeros() as i32;
            let p = n - 1 - log2t;
            let k = n - (p + 1);
            *low = ((x >> k) - ((q as u64) >> k) * r) as i32;
            return prefix + p as usize;
        }
        *low = (x - (q as u64) * r) as i32;
        prefix += n as usize;
    }
    prefix
}

// ---------------------------------------------------------------------------
// Cabac: the reusable engine
// ---------------------------------------------------------------------------

/// HEVC CABAC binary arithmetic decoder with all context models.
///
/// The engine is initialised from byte-aligned slice RBSP data (emulation
/// prevention bytes already removed). The public [`states`](Self::states) and
/// [`stat_coeff`](Self::stat_coeff) arrays hold the context models; downstream
/// crates may snapshot/restore them for entropy-synchronisation substreams via
/// [`save_states`](Self::save_states) / [`load_states`](Self::load_states).
pub struct Cabac<'a> {
    /// Current interval offset (scaled by 2^CABAC_BITS). FFmpeg `low`.
    low: i32,
    /// Current interval range (9-bit, 256..510). FFmpeg `range`.
    range: i32,
    /// Slice RBSP data (the caller keeps the padding).
    data: &'a [u8],
    /// Current byte position (FFmpeg `bytestream - bytestream_start`).
    pos: usize,
    /// Context models, `HEVC_CONTEXTS` entries (FFmpeg `cabac_state`).
    pub states: [u8; HEVC_CONTEXTS],
    /// Persistent Rice adaptation counters (FFmpeg `stat_coeff`).
    pub stat_coeff: [u8; HEVC_STAT_COEFFS],
}

impl<'a> Cabac<'a> {
    /// Open CABAC on `data`.
    ///
    /// Mirrors FFmpeg `ff_init_cabac_decoder` using the fixed aligned path
    /// (`low += 1 << 9`, first two bytes consumed). FFmpeg's other branch only
    /// exists to keep 2-byte loads aligned; it is pointer-alignment dependent
    /// and the H.264 port in Wedeo already established this fixed path as the
    /// parity-preserving choice.
    pub fn new(data: &'a [u8]) -> Result<Self> {
        if data.len() < 2 {
            return Err(Error::InvalidData);
        }
        let low = ((data[0] as i32) << 18) | ((data[1] as i32) << 10) | (1 << 9);
        let range = 0x1FE;
        if (range << (CABAC_BITS + 1)) < low {
            return Err(Error::InvalidData);
        }
        Ok(Self {
            low,
            range,
            data,
            pos: 2,
            states: [0; HEVC_CONTEXTS],
            stat_coeff: [0; HEVC_STAT_COEFFS],
        })
    }

    /// Open CABAC with pre-supplied context models (FFmpeg `load_states` then
    /// `ff_init_cabac_decoder`). Useful for dependent slice segments.
    pub fn new_with_states(
        data: &'a [u8],
        states: [u8; HEVC_CONTEXTS],
        stat_coeff: [u8; HEVC_STAT_COEFFS],
    ) -> Result<Self> {
        let mut c = Self::new(data)?;
        c.states = states;
        c.stat_coeff = stat_coeff;
        Ok(c)
    }

    // -- low-level engine ---------------------------------------------------

    /// Decode one regular bin using context `ctx` (`GET_CABAC(ctx)`).
    #[inline]
    pub fn get_cabac(&mut self, ctx: usize) -> u8 {
        debug_assert!(ctx < HEVC_CONTEXTS);
        decode_bin(
            &mut self.low,
            &mut self.range,
            &mut self.states[ctx],
            self.data,
            &mut self.pos,
        )
    }

    /// Decode one bypass bin (`get_cabac_bypass`).
    #[inline]
    pub fn get_cabac_bypass(&mut self) -> u8 {
        decode_bypass(&mut self.low, &mut self.range, self.data, &mut self.pos)
    }

    /// Decode a bypass bin and apply it as a sign: returns `val` for 0, `-val`
    /// for 1 (`get_cabac_bypass_sign`).
    #[inline]
    pub fn get_cabac_bypass_sign(&mut self, val: i32) -> i32 {
        self.low += self.low;
        if self.low & CABAC_MASK == 0 {
            refill(&mut self.low, self.data, &mut self.pos);
        }
        let r = self.range << (CABAC_BITS + 1);
        self.low -= r;
        let mask = self.low >> 31;
        self.low += r & mask;
        (val ^ mask) - mask
    }

    /// Decode the terminate bin. Returns 0 when not terminated, otherwise the
    /// number of bytes consumed (`get_cabac_terminate`).
    #[inline]
    pub fn get_cabac_terminate(&mut self) -> u32 {
        self.range -= 2;
        if self.low < (self.range << (CABAC_BITS + 1)) {
            renorm_once(&mut self.range, &mut self.low, self.data, &mut self.pos);
            0
        } else {
            self.pos as u32
        }
    }

    /// `cabac_reinit`: skip bytes and reset the engine, used after I_PCM and
    /// when continuing a substream (FFmpeg `skip_bytes(c, 0)`).
    pub fn reinit(&mut self) -> Result<()> {
        let mut ptr = self.pos;
        if self.low & 0x1 != 0 {
            ptr -= 1;
        }
        if self.low & 0x1FF != 0 {
            ptr -= 1;
        }
        if self.data.len() < ptr + 2 {
            return Err(Error::InvalidData);
        }
        self.low = ((self.data[ptr] as i32) << 18) | ((self.data[ptr + 1] as i32) << 10) | (1 << 9);
        self.range = 0x1FE;
        if (self.range << (CABAC_BITS + 1)) < self.low {
            return Err(Error::InvalidData);
        }
        self.pos = ptr + 2;
        Ok(())
    }

    /// Return a snapshot of the context models (FFmpeg `ff_hevc_save_states`).
    pub fn save_states(&self) -> ([u8; HEVC_CONTEXTS], [u8; HEVC_STAT_COEFFS]) {
        (self.states, self.stat_coeff)
    }

    /// Restore a snapshot of the context models (FFmpeg `load_states`).
    pub fn load_states(&mut self, states: [u8; HEVC_CONTEXTS], stat_coeff: [u8; HEVC_STAT_COEFFS]) {
        self.states = states;
        self.stat_coeff = stat_coeff;
    }

    /// Current byte position in `data`.
    pub fn pos(&self) -> usize {
        self.pos
    }
    /// Current `low` (debugging / I_PCM byte-position recovery).
    pub fn low(&self) -> i32 {
        self.low
    }
    /// Current `range` (debugging).
    pub fn range(&self) -> i32 {
        self.range
    }

    // -- context initialisation --------------------------------------------

    /// FFmpeg `cabac_init_state`: derive every context from `init_type` and the
    /// slice QP.
    ///
    /// `init_type` is `2 - slice_type`, XORed with 3 when `cabac_init_flag` is
    /// set on a non-I slice (`initType` in the HEVC spec, table 9-11).
    pub fn init_state(&mut self, init_type: usize, slice_qp: i32) {
        debug_assert!(init_type < 3);
        let qp = slice_qp.clamp(0, 51);
        for (i, &init_value) in INIT_VALUES[init_type].iter().enumerate() {
            let init_value = init_value as i32;
            let m = (init_value >> 4) * 5 - 45;
            let n = ((init_value & 15) << 3) - 16;
            let mut pre = 2 * (((m * qp) >> 4) + n) - 127;

            pre ^= pre >> 31;
            if pre > 124 {
                pre = 124 + (pre & 1);
            }
            self.states[i] = pre as u8;
        }
        self.stat_coeff = [0; HEVC_STAT_COEFFS];
    }

    /// Derive `init_type` from a [`SliceHeader`] and call [`init_state`](Self::init_state).
    pub fn init_state_from_slice(&mut self, sh: &SliceHeader) {
        let is_i = sh.slice_type == SliceType::I;
        let mut init_type = 2 - sh.slice_type as usize;
        if sh.cabac_init_flag && !is_i {
            init_type ^= 3;
        }
        self.init_state(init_type, sh.slice_qp);
    }

    // -- SAO ----------------------------------------------------------------

    /// `ff_hevc_sao_merge_flag_decode`.
    pub fn decode_sao_merge_flag(&mut self) -> u8 {
        self.get_cabac(ctx::SAO_MERGE_FLAG_OFFSET)
    }

    /// `ff_hevc_sao_type_idx_decode` (`enum SAOType`: 0/1/2).
    pub fn decode_sao_type_idx(&mut self) -> SaoType {
        if self.get_cabac(ctx::SAO_TYPE_IDX_OFFSET) == 0 {
            return SaoType::NotApplied;
        }
        if self.get_cabac_bypass() == 0 {
            SaoType::Band
        } else {
            SaoType::Edge
        }
    }

    /// `ff_hevc_sao_band_position_decode` (5 bypass bins).
    pub fn decode_sao_band_position(&mut self) -> i32 {
        let mut value = self.get_cabac_bypass() as i32;
        for _ in 0..4 {
            value = (value << 1) | self.get_cabac_bypass() as i32;
        }
        value
    }

    /// `ff_hevc_sao_offset_abs_decode`.
    pub fn decode_sao_offset_abs(&mut self, bit_depth: u32) -> i32 {
        let length = (1 << (bit_depth.min(10) - 5)) - 1;
        let mut i = 0;
        while i < length && self.get_cabac_bypass() != 0 {
            i += 1;
        }
        i
    }

    /// `ff_hevc_sao_offset_sign_decode`.
    pub fn decode_sao_offset_sign(&mut self) -> u8 {
        self.get_cabac_bypass()
    }

    /// `ff_hevc_sao_eo_class_decode` (2 bypass bins).
    pub fn decode_sao_eo_class(&mut self) -> u8 {
        let mut ret = self.get_cabac_bypass() << 1;
        ret |= self.get_cabac_bypass();
        ret
    }

    // -- CU / slice level ---------------------------------------------------

    /// `ff_hevc_end_of_slice_flag_decode`.
    pub fn decode_end_of_slice_flag(&mut self) -> bool {
        self.get_cabac_terminate() != 0
    }

    /// `ff_hevc_cu_transquant_bypass_flag_decode`.
    pub fn decode_cu_transquant_bypass_flag(&mut self) -> u8 {
        self.get_cabac(ctx::CU_TRANSQUANT_BYPASS_FLAG_OFFSET)
    }

    /// `ff_hevc_skip_flag_decode`. `left`/`above` are the availability-gated
    /// neighbouring skip flags (FFmpeg's `inc` terms).
    pub fn decode_skip_flag(&mut self, left: bool, above: bool) -> u8 {
        let inc = left as usize + above as usize;
        self.get_cabac(ctx::SKIP_FLAG_OFFSET + inc)
    }

    /// `ff_hevc_cu_qp_delta_abs`.
    pub fn decode_cu_qp_delta_abs(&mut self) -> Result<i32> {
        let mut prefix_val = 0;
        let mut suffix_val = 0;
        let mut inc = 0;
        while prefix_val < 5 && self.get_cabac(ctx::CU_QP_DELTA_OFFSET + inc) != 0 {
            prefix_val += 1;
            inc = 1;
        }
        if prefix_val >= 5 {
            let mut k = 0;
            while k < 7 && self.get_cabac_bypass() != 0 {
                suffix_val += 1 << k;
                k += 1;
            }
            if k == 7 {
                trace!("CABAC_MAX_BIN : {}", k);
                return Err(Error::InvalidData);
            }
            while k > 0 {
                k -= 1;
                suffix_val += (self.get_cabac_bypass() as i32) << k;
            }
        }
        Ok(prefix_val + suffix_val)
    }

    /// `ff_hevc_cu_qp_delta_sign_flag`.
    pub fn decode_cu_qp_delta_sign_flag(&mut self) -> u8 {
        self.get_cabac_bypass()
    }

    /// `ff_hevc_cu_chroma_qp_offset_flag`.
    pub fn decode_cu_chroma_qp_offset_flag(&mut self) -> u8 {
        self.get_cabac(ctx::CU_CHROMA_QP_OFFSET_FLAG_OFFSET)
    }

    /// `ff_hevc_cu_chroma_qp_offset_idx`.
    pub fn decode_cu_chroma_qp_offset_idx(&mut self, chroma_qp_offset_list_len_minus1: i32) -> i32 {
        let c_max = chroma_qp_offset_list_len_minus1.max(5);
        let mut i = 0;
        while i < c_max && self.get_cabac(ctx::CU_CHROMA_QP_OFFSET_IDX_OFFSET) != 0 {
            i += 1;
        }
        i
    }

    /// `ff_hevc_pred_mode_decode`.
    pub fn decode_pred_mode_flag(&mut self) -> u8 {
        self.get_cabac(ctx::PRED_MODE_FLAG_OFFSET)
    }

    /// `ff_hevc_split_coding_unit_flag_decode`. `left_deeper` / `above_deeper`
    /// are `(depth_left > ct_depth)` / `(depth_above > ct_depth)`.
    pub fn decode_split_coding_unit_flag(&mut self, left_deeper: bool, above_deeper: bool) -> u8 {
        let inc = left_deeper as usize + above_deeper as usize;
        self.get_cabac(ctx::SPLIT_CODING_UNIT_FLAG_OFFSET + inc)
    }

    /// `ff_hevc_part_mode_decode`.
    pub fn decode_part_mode(
        &mut self,
        log2_cb_size: u32,
        log2_min_cb_size: u32,
        pred_mode_intra: bool,
        amp_enabled: bool,
    ) -> PartMode {
        if self.get_cabac(ctx::PART_MODE_OFFSET) != 0 {
            return PartMode::Part2Nx2N;
        }
        if log2_cb_size == log2_min_cb_size {
            if pred_mode_intra {
                return PartMode::PartNxN;
            }
            if self.get_cabac(ctx::PART_MODE_OFFSET + 1) != 0 {
                return PartMode::Part2NxN;
            }
            if log2_cb_size == 3 {
                return PartMode::PartNx2N;
            }
            if self.get_cabac(ctx::PART_MODE_OFFSET + 2) != 0 {
                return PartMode::PartNx2N;
            }
            return PartMode::PartNxN;
        }
        if !amp_enabled {
            if self.get_cabac(ctx::PART_MODE_OFFSET + 1) != 0 {
                return PartMode::Part2NxN;
            }
            return PartMode::PartNx2N;
        }
        if self.get_cabac(ctx::PART_MODE_OFFSET + 1) != 0 {
            if self.get_cabac(ctx::PART_MODE_OFFSET + 3) != 0 {
                return PartMode::Part2NxN;
            }
            if self.get_cabac_bypass() != 0 {
                return PartMode::Part2NxnD;
            }
            return PartMode::Part2NxnU;
        }
        if self.get_cabac(ctx::PART_MODE_OFFSET + 3) != 0 {
            return PartMode::PartNx2N;
        }
        if self.get_cabac_bypass() != 0 {
            return PartMode::PartNrx2N;
        }
        PartMode::PartNlx2N
    }

    /// `ff_hevc_pcm_flag_decode`.
    pub fn decode_pcm_flag(&mut self) -> u32 {
        self.get_cabac_terminate()
    }

    /// `ff_hevc_prev_intra_luma_pred_flag_decode`.
    pub fn decode_prev_intra_luma_pred_flag(&mut self) -> u8 {
        self.get_cabac(ctx::PREV_INTRA_LUMA_PRED_FLAG_OFFSET)
    }

    /// `ff_hevc_mpm_idx_decode` (at most 2 bypass bins).
    pub fn decode_mpm_idx(&mut self) -> i32 {
        let mut i = 0;
        while i < 2 && self.get_cabac_bypass() != 0 {
            i += 1;
        }
        i
    }

    /// `ff_hevc_rem_intra_luma_pred_mode_decode` (5 bypass bins).
    pub fn decode_rem_intra_luma_pred_mode(&mut self) -> i32 {
        let mut value = self.get_cabac_bypass() as i32;
        for _ in 0..4 {
            value = (value << 1) | self.get_cabac_bypass() as i32;
        }
        value
    }

    /// `ff_hevc_intra_chroma_pred_mode_decode` (returns 4 for "same").
    pub fn decode_intra_chroma_pred_mode(&mut self) -> i32 {
        if self.get_cabac(ctx::INTRA_CHROMA_PRED_MODE_OFFSET) == 0 {
            return 4;
        }
        let mut ret = (self.get_cabac_bypass() as i32) << 1;
        ret |= self.get_cabac_bypass() as i32;
        ret
    }

    /// `ff_hevc_merge_idx_decode`.
    pub fn decode_merge_idx(&mut self, max_num_merge_cand: u32) -> i32 {
        let mut i = self.get_cabac(ctx::MERGE_IDX_OFFSET) as i32;
        if i != 0 {
            while i < max_num_merge_cand as i32 - 1 && self.get_cabac_bypass() != 0 {
                i += 1;
            }
        }
        i
    }

    /// `ff_hevc_merge_flag_decode`.
    pub fn decode_merge_flag(&mut self) -> u8 {
        self.get_cabac(ctx::MERGE_FLAG_OFFSET)
    }

    /// `ff_hevc_inter_pred_idc_decode`.
    pub fn decode_inter_pred_idc(
        &mut self,
        n_pb_w: i32,
        n_pb_h: i32,
        ct_depth: usize,
    ) -> InterPredIdc {
        if n_pb_w + n_pb_h == 12 {
            return if self.get_cabac(ctx::INTER_PRED_IDC_OFFSET + 4) != 0 {
                InterPredIdc::PredL1
            } else {
                InterPredIdc::PredL0
            };
        }
        if self.get_cabac(ctx::INTER_PRED_IDC_OFFSET + ct_depth) != 0 {
            return InterPredIdc::PredBi;
        }
        if self.get_cabac(ctx::INTER_PRED_IDC_OFFSET + 4) != 0 {
            InterPredIdc::PredL1
        } else {
            InterPredIdc::PredL0
        }
    }

    /// `ff_hevc_ref_idx_lx_decode`.
    pub fn decode_ref_idx_lx(&mut self, num_ref_idx_lx: i32) -> i32 {
        let max = num_ref_idx_lx - 1;
        let max_ctx = max.min(2);
        let mut i = 0;
        while i < max_ctx && self.get_cabac(ctx::REF_IDX_L0_OFFSET + i as usize) != 0 {
            i += 1;
        }
        if i == 2 {
            while i < max && self.get_cabac_bypass() != 0 {
                i += 1;
            }
        }
        i
    }

    /// `ff_hevc_mvp_lx_flag_decode`.
    pub fn decode_mvp_lx_flag(&mut self) -> u8 {
        self.get_cabac(ctx::MVP_LX_FLAG_OFFSET)
    }

    /// `ff_hevc_no_residual_syntax_flag_decode`.
    pub fn decode_no_residual_syntax_flag(&mut self) -> u8 {
        self.get_cabac(ctx::NO_RESIDUAL_DATA_FLAG_OFFSET)
    }

    // -- motion vector difference ------------------------------------------

    /// `abs_mvd_greater0_flag_decode`.
    pub fn decode_abs_mvd_greater0_flag(&mut self) -> u8 {
        self.get_cabac(ctx::ABS_MVD_GREATER0_FLAG_OFFSET)
    }

    /// `abs_mvd_greater1_flag_decode`.
    pub fn decode_abs_mvd_greater1_flag(&mut self) -> u8 {
        self.get_cabac(ctx::ABS_MVD_GREATER1_FLAG_OFFSET + 1)
    }

    /// `mvd_decode`: absolute MVD > 1 (exp-Golomb-like bypass code with sign).
    pub fn decode_mvd(&mut self) -> i32 {
        let mut ret = 2;
        let mut k = 1;
        while k < CABAC_MAX_BIN && self.get_cabac_bypass() != 0 {
            ret += 1 << k;
            k += 1;
        }
        if k == CABAC_MAX_BIN {
            trace!("CABAC_MAX_BIN : {}", k);
            return 0;
        }
        while k > 0 {
            k -= 1;
            ret += (self.get_cabac_bypass() as i32) << k;
        }
        self.get_cabac_bypass_sign(-ret)
    }

    /// `mvd_sign_flag_decode`.
    pub fn decode_mvd_sign_flag(&mut self) -> i32 {
        self.get_cabac_bypass_sign(-1)
    }

    /// `ff_hevc_hls_mvd_coding`: decode the (x, y) MVD pair.
    pub fn decode_mvd_coding(&mut self) -> (i32, i32) {
        let mut x = self.decode_abs_mvd_greater0_flag() as i32;
        let mut y = self.decode_abs_mvd_greater0_flag() as i32;
        if x != 0 {
            x += self.decode_abs_mvd_greater1_flag() as i32;
        }
        if y != 0 {
            y += self.decode_abs_mvd_greater1_flag() as i32;
        }
        let mv_x = match x {
            2 => self.decode_mvd(),
            1 => self.decode_mvd_sign_flag(),
            _ => 0,
        };
        let mv_y = match y {
            2 => self.decode_mvd(),
            1 => self.decode_mvd_sign_flag(),
            _ => 0,
        };
        (mv_x, mv_y)
    }

    // -- transform tree / residual contexts ---------------------------------

    /// `ff_hevc_split_transform_flag_decode`.
    pub fn decode_split_transform_flag(&mut self, log2_trafo_size: u32) -> u8 {
        self.get_cabac(ctx::SPLIT_TRANSFORM_FLAG_OFFSET + 5 - log2_trafo_size as usize)
    }

    /// `ff_hevc_cbf_cb_cr_decode`.
    pub fn decode_cbf_cb_cr(&mut self, trafo_depth: usize) -> u8 {
        self.get_cabac(ctx::CBF_CB_CR_OFFSET + trafo_depth)
    }

    /// `ff_hevc_cbf_luma_decode`.
    pub fn decode_cbf_luma(&mut self, trafo_depth: usize) -> u8 {
        self.get_cabac(ctx::CBF_LUMA_OFFSET + usize::from(trafo_depth == 0))
    }

    /// `hevc_transform_skip_flag_decode`.
    pub fn decode_transform_skip_flag(&mut self, c_idx: usize) -> u8 {
        self.get_cabac(ctx::TRANSFORM_SKIP_FLAG_OFFSET + usize::from(c_idx != 0))
    }

    /// `explicit_rdpcm_flag_decode`.
    pub fn decode_explicit_rdpcm_flag(&mut self, c_idx: usize) -> u8 {
        self.get_cabac(ctx::EXPLICIT_RDPCM_FLAG_OFFSET + usize::from(c_idx != 0))
    }

    /// `explicit_rdpcm_dir_flag_decode`.
    pub fn decode_explicit_rdpcm_dir_flag(&mut self, c_idx: usize) -> u8 {
        self.get_cabac(ctx::EXPLICIT_RDPCM_DIR_FLAG_OFFSET + usize::from(c_idx != 0))
    }

    /// `ff_hevc_log2_res_scale_abs`.
    pub fn decode_log2_res_scale_abs(&mut self, idx: usize) -> i32 {
        let mut i = 0;
        while i < 4 && self.get_cabac(ctx::LOG2_RES_SCALE_ABS_OFFSET + 4 * idx + i) != 0 {
            i += 1;
        }
        i as i32
    }

    /// `ff_hevc_res_scale_sign_flag`.
    pub fn decode_res_scale_sign_flag(&mut self, idx: usize) -> u8 {
        self.get_cabac(ctx::RES_SCALE_SIGN_FLAG_OFFSET + idx)
    }

    // -- coefficient coding -------------------------------------------------

    /// `last_significant_coeff_xy_prefix_decode`. Returns `(x_prefix, y_prefix)`.
    pub fn decode_last_significant_coeff_prefix(
        &mut self,
        c_idx: usize,
        log2_size: u32,
    ) -> (i32, i32) {
        let max = (log2_size << 1) - 1;
        let (ctx_offset, ctx_shift) = if c_idx == 0 {
            (
                3 * (log2_size - 2) + ((log2_size - 1) >> 2),
                (log2_size + 1) >> 2,
            )
        } else {
            (15, log2_size - 2)
        };
        let mut x_prefix = 0;
        while x_prefix < max as i32
            && self.get_cabac(
                ctx::LAST_SIGNIFICANT_COEFF_X_PREFIX_OFFSET
                    + ((x_prefix as u32 >> ctx_shift) + ctx_offset) as usize,
            ) != 0
        {
            x_prefix += 1;
        }
        let mut y_prefix = 0;
        while y_prefix < max as i32
            && self.get_cabac(
                ctx::LAST_SIGNIFICANT_COEFF_Y_PREFIX_OFFSET
                    + ((y_prefix as u32 >> ctx_shift) + ctx_offset) as usize,
            ) != 0
        {
            y_prefix += 1;
        }
        (x_prefix, y_prefix)
    }

    /// `last_significant_coeff_suffix_decode`. Only called for prefixes > 3.
    pub fn decode_last_significant_coeff_suffix(&mut self, prefix: i32) -> i32 {
        let length = (prefix >> 1) - 1;
        let mut value = self.get_cabac_bypass() as i32;
        for _ in 1..length {
            value = (value << 1) | self.get_cabac_bypass() as i32;
        }
        value
    }

    /// `significant_coeff_group_flag_decode`.
    pub fn decode_significant_coeff_group_flag(&mut self, c_idx: usize, ctx_cg: i32) -> u8 {
        let inc = ctx_cg.min(1) as usize + usize::from(c_idx > 0) * 2;
        self.get_cabac(ctx::SIGNIFICANT_COEFF_GROUP_FLAG_OFFSET + inc)
    }

    /// `significant_coeff_flag_decode`: `inc = ctx_idx_map[n] + offset`.
    pub fn decode_significant_coeff_flag(
        &mut self,
        ctx_idx_map: &[u8],
        n: usize,
        offset: usize,
    ) -> u8 {
        self.get_cabac(ctx::SIGNIFICANT_COEFF_FLAG_OFFSET + ctx_idx_map[n] as usize + offset)
    }

    /// `significant_coeff_flag_decode_0` (scan position 0 uses a fixed ctx).
    pub fn decode_significant_coeff_flag_0(&mut self, c_idx: usize, offset: usize) -> u8 {
        let _ = c_idx;
        self.get_cabac(ctx::SIGNIFICANT_COEFF_FLAG_OFFSET + offset)
    }

    /// `coeff_abs_level_greater1_flag_decode`.
    pub fn decode_coeff_abs_level_greater1_flag(&mut self, c_idx: usize, mut inc: usize) -> u8 {
        if c_idx > 0 {
            inc += 16;
        }
        self.get_cabac(ctx::COEFF_ABS_LEVEL_GREATER1_FLAG_OFFSET + inc)
    }

    /// `coeff_abs_level_greater2_flag_decode`.
    pub fn decode_coeff_abs_level_greater2_flag(&mut self, c_idx: usize, mut inc: usize) -> u8 {
        if c_idx > 0 {
            inc += 4;
        }
        self.get_cabac(ctx::COEFF_ABS_LEVEL_GREATER2_FLAG_OFFSET + inc)
    }

    /// `coeff_abs_level_remaining_decode` (Rice/bypass coding).
    pub fn decode_coeff_abs_level_remaining(&mut self, rc_rice_param: usize) -> i32 {
        let prefix = unary_prefix(
            &mut self.low,
            &mut self.range,
            self.data,
            &mut self.pos,
            CABAC_MAX_BIN,
        );
        if prefix < 3 {
            let suffix = if rc_rice_param > 2 {
                bypass_bits(
                    &mut self.low,
                    &mut self.range,
                    self.data,
                    &mut self.pos,
                    rc_rice_param,
                )
            } else {
                let mut suffix = 0;
                for _ in 0..rc_rice_param {
                    suffix = (suffix << 1) | self.get_cabac_bypass() as i32;
                }
                suffix
            };
            ((prefix as i32) << rc_rice_param) + suffix
        } else {
            let prefix_minus3 = prefix - 3;
            if prefix == CABAC_MAX_BIN || prefix_minus3 + rc_rice_param > 16 + 6 {
                trace!("CABAC_MAX_BIN : {}", prefix);
                return 0;
            }
            let k = prefix_minus3 + rc_rice_param;
            let suffix = if k > 16 {
                (bypass_bits(&mut self.low, &mut self.range, self.data, &mut self.pos, 16)
                    << (k - 16))
                    | bypass_bits(
                        &mut self.low,
                        &mut self.range,
                        self.data,
                        &mut self.pos,
                        k - 16,
                    )
            } else if k > 2 {
                bypass_bits(&mut self.low, &mut self.range, self.data, &mut self.pos, k)
            } else {
                let mut suffix = 0;
                for _ in 0..k {
                    suffix = (suffix << 1) | self.get_cabac_bypass() as i32;
                }
                suffix
            };
            (((1i32 << prefix_minus3) + 3 - 1) << rc_rice_param) + suffix
        }
    }

    /// `coeff_sign_flag_decode`: returns a bitmask of `nb` signs.
    pub fn decode_coeff_sign_flag(&mut self, nb: usize) -> u32 {
        if nb > 2 {
            return bypass_bits(&mut self.low, &mut self.range, self.data, &mut self.pos, nb)
                as u32;
        }
        let mut ret = 0u32;
        for _ in 0..nb {
            ret = (ret << 1) | self.get_cabac_bypass() as u32;
        }
        ret
    }

    /// Full residual-bin assembly, port of `ff_hevc_hls_residual_coding`'s CABAC
    /// part. Writes signed quantised coefficient *levels* (sign hiding applied)
    /// to `coeffs` at raster index `y * trafo_size + x` and returns the flags
    /// needed by the downstream inverse-transform / dequant stage.
    ///
    /// Note: dequantisation, scaling lists and the inverse transform are
    /// deliberately **not** performed here; that belongs to the DSP crate.
    pub fn decode_residual_coefficients(
        &mut self,
        params: &ResidualParams,
        coeffs: &mut [i32],
    ) -> Result<ResidualInfo> {
        let c_idx = params.c_idx;
        let log2_trafo_size = params.log2_trafo_size as usize;
        let trafo_size = 1usize << log2_trafo_size;
        let scan_idx = params.scan_idx as usize;

        let mut info = ResidualInfo::default();

        // transform_skip_flag is coded only when enabled for this block size.
        if params.transform_skip_enabled
            && log2_trafo_size as u32 <= params.log2_max_transform_skip_block_size
        {
            info.transform_skip_flag = self.decode_transform_skip_flag(c_idx);
        }

        let ts = info.transform_skip_flag != 0;
        let bypass_ctx = ts || params.cu_transquant_bypass;

        // explicit_rdpcm_flag / dir (INTER + RDPCM + transform skip/bypass).
        if !params.is_intra && params.explicit_rdpcm_enabled && (ts || params.cu_transquant_bypass)
        {
            info.explicit_rdpcm_flag = self.decode_explicit_rdpcm_flag(c_idx);
            if info.explicit_rdpcm_flag != 0 {
                info.explicit_rdpcm_dir_flag = self.decode_explicit_rdpcm_dir_flag(c_idx) as i32;
            }
        }

        // last_significant_coeff prefix + suffix.
        let (mut last_x, mut last_y) =
            self.decode_last_significant_coeff_prefix(c_idx, log2_trafo_size as u32);
        if last_x > 3 {
            let suffix = self.decode_last_significant_coeff_suffix(last_x);
            last_x = (1 << ((last_x >> 1) - 1)) * (2 + (last_x & 1)) + suffix;
        }
        if last_y > 3 {
            let suffix = self.decode_last_significant_coeff_suffix(last_y);
            last_y = (1 << ((last_y >> 1) - 1)) * (2 + (last_y & 1)) + suffix;
        }
        if params.scan_idx == ScanType::Vert {
            std::mem::swap(&mut last_x, &mut last_y);
        }
        info.last_x = last_x;
        info.last_y = last_y;

        let x_cg_last_sig = (last_x >> 2) as usize;
        let y_cg_last_sig = (last_y >> 2) as usize;

        // Scan pointers and the number of coefficients up to the last one.
        let (scan_x_cg, scan_y_cg, scan_x_off, scan_y_off, mut num_coeff): (
            &[u8],
            &[u8],
            &[u8],
            &[u8],
            i32,
        );
        match params.scan_idx {
            ScanType::Diag => {
                let last_x_c = (last_x & 3) as usize;
                let last_y_c = (last_y & 3) as usize;
                let mut nc = tables::DIAG_SCAN4X4_INV[last_y_c * 4 + last_x_c] as i32;
                if trafo_size == 4 {
                    scan_x_cg = &tables::SCAN_1X1;
                    scan_y_cg = &tables::SCAN_1X1;
                } else if trafo_size == 8 {
                    nc += (tables::DIAG_SCAN2X2_INV[y_cg_last_sig * 2 + x_cg_last_sig] as i32) << 4;
                    scan_x_cg = &tables::DIAG_SCAN2X2_X;
                    scan_y_cg = &tables::DIAG_SCAN2X2_Y;
                } else if trafo_size == 16 {
                    nc += (tables::DIAG_SCAN4X4_INV[y_cg_last_sig * 4 + x_cg_last_sig] as i32) << 4;
                    scan_x_cg = &tables::DIAG_SCAN4X4_X;
                    scan_y_cg = &tables::DIAG_SCAN4X4_Y;
                } else {
                    nc += (tables::DIAG_SCAN8X8_INV[y_cg_last_sig * 8 + x_cg_last_sig] as i32) << 4;
                    scan_x_cg = &tables::DIAG_SCAN8X8_X;
                    scan_y_cg = &tables::DIAG_SCAN8X8_Y;
                }
                scan_x_off = &tables::DIAG_SCAN4X4_X;
                scan_y_off = &tables::DIAG_SCAN4X4_Y;
                num_coeff = nc;
            }
            ScanType::Horiz => {
                scan_x_cg = &tables::HORIZ_SCAN2X2_X;
                scan_y_cg = &tables::HORIZ_SCAN2X2_Y;
                scan_x_off = &tables::HORIZ_SCAN4X4_X;
                scan_y_off = &tables::HORIZ_SCAN4X4_Y;
                num_coeff = tables::HORIZ_SCAN8X8_INV[last_y as usize * 8 + last_x as usize] as i32;
            }
            ScanType::Vert => {
                scan_x_cg = &tables::HORIZ_SCAN2X2_Y;
                scan_y_cg = &tables::HORIZ_SCAN2X2_X;
                scan_x_off = &tables::HORIZ_SCAN4X4_Y;
                scan_y_off = &tables::HORIZ_SCAN4X4_X;
                num_coeff = tables::HORIZ_SCAN8X8_INV[last_x as usize * 8 + last_y as usize] as i32;
            }
        }
        num_coeff += 1;
        let num_last_subset = (num_coeff - 1) >> 4;
        info.num_coeff = num_coeff;

        let max_xy = last_x.max(last_y);
        let mut col_limit = last_x + last_y + 4;
        if max_xy < 4 {
            col_limit = col_limit.min(4);
        } else if max_xy < 8 {
            col_limit = col_limit.min(8);
        } else if max_xy < 12 {
            col_limit = col_limit.min(24);
        }
        info.max_xy = max_xy;
        info.col_limit = col_limit;

        // The coefficient buffer must be large enough for the whole block.
        if coeffs.len() < trafo_size * trafo_size {
            return Err(Error::InvalidArgument);
        }
        coeffs[..trafo_size * trafo_size].fill(0);

        let mut significant_coeff_group_flag = [[0u8; 8]; 8];
        let x_cg_limit = (1usize << (log2_trafo_size - 2)).saturating_sub(1);
        let y_cg_limit = x_cg_limit;

        let mut greater1_ctx = 1i32;

        for i in (0..=num_last_subset).rev() {
            let i = i as usize;
            let offset = i << 4;
            let x_cg = scan_x_cg[i] as usize;
            let y_cg = scan_y_cg[i] as usize;
            let mut implicit_non_zero_coeff = 0;

            // significant_coeff_group_flag for this group.
            if i < num_last_subset as usize && i > 0 {
                let mut ctx_cg = 0;
                if x_cg < x_cg_limit {
                    ctx_cg += significant_coeff_group_flag[x_cg + 1][y_cg] as i32;
                }
                if y_cg < y_cg_limit {
                    ctx_cg += significant_coeff_group_flag[x_cg][y_cg + 1] as i32;
                }
                significant_coeff_group_flag[x_cg][y_cg] =
                    self.decode_significant_coeff_group_flag(c_idx, ctx_cg);
                implicit_non_zero_coeff = 1;
            } else {
                significant_coeff_group_flag[x_cg][y_cg] = u8::from(
                    (x_cg == x_cg_last_sig && y_cg == y_cg_last_sig) || (x_cg == 0 && y_cg == 0),
                );
            }

            let last_scan_pos = num_coeff - offset as i32 - 1;
            let mut n_end;
            let mut significant_coeff_flag_idx = [0u8; 16];
            let mut nb_significant_coeff_flag: usize = 0;
            if i as i32 == num_last_subset {
                n_end = last_scan_pos - 1;
                significant_coeff_flag_idx[0] = last_scan_pos as u8;
                nb_significant_coeff_flag = 1;
            } else {
                n_end = 15;
            }

            let mut prev_sig = 0usize;
            if x_cg < x_cg_limit {
                prev_sig += significant_coeff_group_flag[x_cg + 1][y_cg] as usize;
            }
            if y_cg < y_cg_limit {
                prev_sig += (significant_coeff_group_flag[x_cg][y_cg + 1] as usize) << 1;
            }

            if significant_coeff_group_flag[x_cg][y_cg] != 0 && n_end >= 0 {
                let mut scf_offset = 0usize;
                let map: &[u8];
                if params.transform_skip_context_enabled && bypass_ctx {
                    map = &CTX_IDX_MAP[scan_idx][64..80];
                    scf_offset = if c_idx == 0 { 40 } else { 14 + 27 };
                } else {
                    if c_idx != 0 {
                        scf_offset = 27;
                    }
                    if log2_trafo_size == 2 {
                        map = &CTX_IDX_MAP[scan_idx][0..16];
                    } else {
                        map = &CTX_IDX_MAP[scan_idx][((prev_sig + 1) << 4)..((prev_sig + 2) << 4)];
                        if c_idx == 0 {
                            if x_cg > 0 || y_cg > 0 {
                                scf_offset += 3;
                            }
                            if log2_trafo_size == 3 {
                                scf_offset += if params.scan_idx == ScanType::Diag {
                                    9
                                } else {
                                    15
                                };
                            } else {
                                scf_offset += 21;
                            }
                        } else if log2_trafo_size == 3 {
                            scf_offset += 9;
                        } else {
                            scf_offset += 12;
                        }
                    }
                }

                let nb0 = nb_significant_coeff_flag;
                let mut n = n_end;
                while n > 0 {
                    let sig = self.decode_significant_coeff_flag(map, n as usize, scf_offset);
                    significant_coeff_flag_idx[nb_significant_coeff_flag] = n as u8;
                    nb_significant_coeff_flag += sig as usize;
                    n -= 1;
                }
                if nb_significant_coeff_flag != nb0 {
                    implicit_non_zero_coeff = 0;
                }
                if implicit_non_zero_coeff == 0 {
                    if params.transform_skip_context_enabled && bypass_ctx {
                        scf_offset = if c_idx == 0 { 42 } else { 16 + 27 };
                    } else if i == 0 {
                        scf_offset = if c_idx == 0 { 0 } else { 27 };
                    } else {
                        scf_offset += 2;
                    }
                    significant_coeff_flag_idx[nb_significant_coeff_flag] = 0;
                    nb_significant_coeff_flag +=
                        self.decode_significant_coeff_flag_0(c_idx, scf_offset) as usize;
                } else {
                    significant_coeff_flag_idx[nb_significant_coeff_flag] = 0;
                    nb_significant_coeff_flag += 1;
                }
            }

            n_end = nb_significant_coeff_flag as i32;
            if n_end <= 0 {
                continue;
            }

            // Level coding for this coefficient group.
            let mut c_rice_param = 0usize;
            let mut first_greater1_coeff_idx = -1i32;
            let mut greater1_flag = [0u8; 8];
            let mut gt1_mask = 0u32;
            let mut ctx_set = if i > 0 && c_idx == 0 { 2usize } else { 0usize };

            if params.persistent_rice_adaptation_enabled {
                let sb_type = (if c_idx == 0 { 2 } else { 0 }) + usize::from(bypass_ctx);
                c_rice_param = (self.stat_coeff[sb_type] / 4) as usize;
            }
            if i as i32 != num_last_subset && greater1_ctx == 0 {
                ctx_set += 1;
            }
            greater1_ctx = 1;
            let last_nz_pos_in_cg = significant_coeff_flag_idx[0] as i32;

            let m_limit = (n_end as usize).min(8);
            for m in 0..m_limit {
                let inc = (ctx_set << 2) + greater1_ctx as usize;
                let flag = self.decode_coeff_abs_level_greater1_flag(c_idx, inc);
                greater1_flag[m] = flag;
                gt1_mask |= (flag as u32) << m;
                greater1_ctx = (greater1_ctx + i32::from(greater1_ctx - 1 < 2)) & (flag as i32 - 1);
            }
            if gt1_mask != 0 {
                first_greater1_coeff_idx = gt1_mask.trailing_zeros() as i32;
            }
            let first_nz_pos_in_cg = significant_coeff_flag_idx[n_end as usize - 1] as i32;

            // sign_hidden: FFmpeg's sign-data-hiding condition.
            let sign_hidden_disabled = params.cu_transquant_bypass
                || (params.is_intra && params.implicit_rdpcm_active && ts)
                || info.explicit_rdpcm_flag != 0;
            let sign_hidden = !sign_hidden_disabled && last_nz_pos_in_cg - first_nz_pos_in_cg >= 4;

            if first_greater1_coeff_idx != -1 {
                let fg = first_greater1_coeff_idx as usize;
                greater1_flag[fg] += self.decode_coeff_abs_level_greater2_flag(c_idx, ctx_set);
            }
            let sign_bits = if !params.sign_data_hiding_enabled || !sign_hidden {
                self.decode_coeff_sign_flag(nb_significant_coeff_flag)
                    << (16 - nb_significant_coeff_flag)
            } else {
                self.decode_coeff_sign_flag(nb_significant_coeff_flag - 1)
                    << (16 - (nb_significant_coeff_flag - 1))
            };
            let mut sign_flag = sign_bits;

            let mut rice_init = false;
            let sb_type = (if c_idx == 0 { 2 } else { 0 }) + usize::from(bypass_ctx);
            let mut sum_abs = 0i32;

            for m in 0..n_end as usize {
                let n = significant_coeff_flag_idx[m] as usize;
                let x_c = (x_cg << 2) + scan_x_off[n] as usize;
                let y_c = (y_cg << 2) + scan_y_off[n] as usize;

                let mut level: i32;
                if m < 8 {
                    level = 1 + greater1_flag[m] as i32;
                    if level
                        == if m as i32 == first_greater1_coeff_idx {
                            3
                        } else {
                            2
                        }
                    {
                        let remaining = self.decode_coeff_abs_level_remaining(c_rice_param);
                        level += remaining;
                        if level > (3 << c_rice_param) {
                            c_rice_param = if params.persistent_rice_adaptation_enabled {
                                c_rice_param + 1
                            } else {
                                (c_rice_param + 1).min(4)
                            };
                        }
                        self.adapt_rice(
                            sb_type,
                            remaining,
                            params.persistent_rice_adaptation_enabled,
                            &mut rice_init,
                        );
                    }
                } else {
                    let remaining = self.decode_coeff_abs_level_remaining(c_rice_param);
                    level = 1 + remaining;
                    if level > (3 << c_rice_param) {
                        c_rice_param = if params.persistent_rice_adaptation_enabled {
                            c_rice_param + 1
                        } else {
                            (c_rice_param + 1).min(4)
                        };
                    }
                    self.adapt_rice(
                        sb_type,
                        remaining,
                        params.persistent_rice_adaptation_enabled,
                        &mut rice_init,
                    );
                }

                if params.sign_data_hiding_enabled && sign_hidden {
                    sum_abs += level;
                    if n as i32 == first_nz_pos_in_cg && sum_abs & 1 != 0 {
                        level = -level;
                    }
                }
                // Equiprobable sign: apply branchlessly (FFmpeg).
                let sign = -((sign_flag >> 15) as i32);
                level = (level ^ sign) - sign;
                sign_flag <<= 1;

                coeffs[y_c * trafo_size + x_c] = level;
            }
        }
        Ok(info)
    }

    /// Persistent Rice adaptation bookkeeping (`stat_coeff`) shared by the
    /// greater1 and "remaining" branches of the coefficient loop.
    #[inline]
    fn adapt_rice(
        &mut self,
        sb_type: usize,
        remaining: i32,
        persistent: bool,
        rice_init: &mut bool,
    ) {
        if !persistent || *rice_init {
            return;
        }
        let c_rice_p_init = (self.stat_coeff[sb_type] / 4) as i32;
        if remaining >= (3 << c_rice_p_init) {
            self.stat_coeff[sb_type] = self.stat_coeff[sb_type].wrapping_add(1);
        } else if 2 * remaining < (1 << c_rice_p_init) && self.stat_coeff[sb_type] > 0 {
            self.stat_coeff[sb_type] -= 1;
        }
        *rice_init = true;
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests;
