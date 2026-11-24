// VP8 transform-coefficient decoding.
//
// Translated from FFmpeg libavcodec/vp8.c: decode_block_coeffs_internal,
// decode_block_coeffs, decode_mb_coeffs, and the DCT category probability
// tables in libavcodec/vp8data.c.
// LGPL-2.1-or-later, same as FFmpeg.

use crate::bool_decoder::BoolDecoder;
use crate::data::*;
use crate::header::HeaderInfo;
use crate::idct;
use crate::types::*;

// DCT category probabilities for cat3..cat6 (index cat = 0..3), which are
// ff_vp8_dct_cat_prob[cat3..cat6] in FFmpeg.
const CAT_PROB3: [&[u8]; 4] = [
    DCT_CAT_PROB[2],
    DCT_CAT_PROB[3],
    DCT_CAT_PROB[4],
    DCT_CAT_PROB[5],
];

/// Decoded coefficients for one macroblock. Blocks 0..3 are luma, 4 is U,
/// 5 is V. `block_dc` is the separate luma DC block before the WHT.
#[derive(Clone, Default)]
pub struct CoeffState {
    /// FFmpeg layout: `[6][4][16]` - 6 planes, 4 blocks each.
    pub blocks: [[[i16; 16]; 4]; 6],
    pub block_dc: [i16; 16],
    /// non_zero_count_cache[6][4]
    pub nz: [[u8; 4]; 6],
}

/// `decode_block_coeffs_internal` from vp8.c. Returns the number of decoded
/// coefficients (index of the last one plus one), or 0 for EOB/no coeffs.
#[allow(clippy::too_many_arguments)]
fn decode_block_internal(
    c: &mut BoolDecoder<'_>,
    block: &mut [i16; 16],
    probs: &[[[u8; 11]; 3]; 16],
    mut i: usize,
    start_token_prob: &[u8],
    qmul: [i32; 2],
    scan: &[u8; 16],
) -> usize {
    let mut token_prob = start_token_prob;
    // FFmpeg advances `token_prob` to `probs[i+1]`; when i==15 that pointer
    // is never dereferenced again, so we clamp the index here.
    // FFmpeg enters the loop with `goto skip_eob`, so the EOB check is
    // skipped for the first coefficient (already checked by the caller),
    // but performed on every subsequent iteration at `restart:`.
    let mut first = true;
    loop {
        if !first && !c.get_prob(token_prob[0]) {
            break; // DCT_EOB
        }
        first = false;

        // skip_eob: skip zero coefficients.
        loop {
            if !c.get_prob(token_prob[1]) {
                // DCT_0
                i += 1;
                if i == 16 {
                    return i;
                }
                token_prob = &probs[i][0];
                continue;
            }
            break;
        }

        let coeff: i32;
        if !c.get_prob(token_prob[2]) {
            // DCT_1
            coeff = 1;
            token_prob = &probs[(i + 1).min(15)][1];
        } else {
            if !c.get_prob(token_prob[3]) {
                // DCT 2,3,4
                let mut v = i32::from(c.get_prob(token_prob[4]));
                if v != 0 {
                    v += i32::from(c.get_prob(token_prob[5]));
                }
                coeff = v + 2;
            } else {
                // DCT_CAT*
                if !c.get_prob(token_prob[6]) {
                    if !c.get_prob(token_prob[7]) {
                        coeff = 5 + i32::from(c.get_prob(DCT_CAT_PROB[0][0]));
                    } else {
                        coeff = 7
                            + (i32::from(c.get_prob(DCT_CAT_PROB[1][0])) << 1)
                            + i32::from(c.get_prob(DCT_CAT_PROB[1][1]));
                    }
                } else {
                    let a = i32::from(c.get_prob(token_prob[8]));
                    let b = i32::from(c.get_prob(token_prob[9 + a as usize]));
                    let cat = ((a << 1) + b) as usize;
                    coeff = 3 + (8 << cat) + c.get_coeff(CAT_PROB3[cat]);
                }
            }
            token_prob = &probs[(i + 1).min(15)][2];
        }

        let signed = if c.get() { -coeff } else { coeff };
        block[scan[i] as usize] = (signed * qmul[usize::from(i != 0)]) as i16;
        i += 1;
        if i >= 16 {
            return i;
        }
    }
    i
}

/// `decode_block_coeffs` from vp8.c: checks EOB up front, then decodes.
// Mirrors FFmpeg's `decode_block_coeffs` signature (8 parameters).
#[allow(clippy::too_many_arguments)]
fn decode_block(
    c: &mut BoolDecoder<'_>,
    block: &mut [i16; 16],
    probs: &[[[[u8; 11]; 3]; 16]; 4],
    type_index: usize,
    i: usize,
    zero_nhood: usize,
    qmul: [i32; 2],
    scan: &[u8; 16],
) -> usize {
    let table = &probs[type_index];
    if !c.get_prob(table[i][zero_nhood][0]) {
        return 0;
    }
    decode_block_internal(c, block, table, i, &table[i][zero_nhood], qmul, scan)
}

/// Decode all coefficients for one macroblock (`decode_mb_coeffs`).
/// `top_nnz` is the per-column top-neighbour state; `left_nnz` is the
/// per-row left-neighbour state.
#[allow(clippy::too_many_arguments)]
pub fn decode_mb_coeffs(
    c: &mut BoolDecoder<'_>,
    header: &HeaderInfo,
    mb: &mut MbState,
    coeffs: &mut CoeffState,
    top_nnz: &mut [[u8; 9]],
    left_nnz: &mut [u8; 9],
    mb_x: usize,
) {
    let segment = mb.segment;
    let mut luma_start = 0usize;
    let mut luma_ctx = 3usize;
    let mut nnz_total = 0usize;
    let mut block_dc = false;

    if mb.mode != MODE_I4x4 && mb.mode != MVMODE_SPLIT {
        let nnz_pred = top_nnz[mb_x][8] as usize + left_nnz[8] as usize;
        let nnz = decode_block(
            c,
            &mut coeffs.block_dc,
            &header.probs.token,
            1,
            0,
            nnz_pred,
            header.qmat[segment].luma_dc_qmul,
            &ZIGZAG_SCAN,
        );
        top_nnz[mb_x][8] = u8::from(nnz != 0);
        left_nnz[8] = u8::from(nnz != 0);
        if nnz != 0 {
            nnz_total += nnz;
            block_dc = true;
            if nnz == 1 {
                idct::luma_dc_wht_dc(&mut coeffs.blocks, &mut coeffs.block_dc);
            } else {
                idct::luma_dc_wht(&mut coeffs.blocks, &mut coeffs.block_dc);
            }
        }
        luma_start = 1;
        luma_ctx = 0;
    }

    // Luma 4x4 blocks (4x4 grid).
    for y in 0..4 {
        for x in 0..4 {
            let nnz_pred = left_nnz[y] as usize + top_nnz[mb_x][x] as usize;
            let nnz = decode_block(
                c,
                &mut coeffs.blocks[y][x],
                &header.probs.token,
                luma_ctx,
                luma_start,
                nnz_pred,
                header.qmat[segment].luma_qmul,
                &header.probs.scan,
            );
            // nnz+block_dc may be one more than the actual last index,
            // but we don't care.
            coeffs.nz[y][x] = (nnz + usize::from(block_dc)) as u8;
            top_nnz[mb_x][x] = u8::from(nnz != 0);
            left_nnz[y] = u8::from(nnz != 0);
            nnz_total += nnz;
        }
    }

    // Chroma blocks: i = 4 (U), 5 (V); each has a 2x2 grid.
    // `nnz_pred` indexes: l_nnz[i + 2*y], t_nnz[i + 2*x].
    for i in 4..6 {
        for y in 0..2 {
            for x in 0..2 {
                // FFmpeg uses l_nnz[i + 2*y] / t_nnz[i + 2*x] with i in
                // {4,5}: indices 4,6 for U and 5,7 for V (entry 8 is the
                // luma DC predictor). Do NOT remap to 0..3 - that would
                // collide with the luma row predictors.
                let li = i + 2 * y;
                let ti = i + 2 * x;
                let nnz_pred = left_nnz[li] as usize + top_nnz[mb_x][ti] as usize;
                let nnz = decode_block(
                    c,
                    &mut coeffs.blocks[i][(y << 1) + x],
                    &header.probs.token,
                    2,
                    0,
                    nnz_pred,
                    header.qmat[segment].chroma_qmul,
                    &header.probs.scan,
                );
                coeffs.nz[i][(y << 1) + x] = nnz as u8;
                top_nnz[mb_x][ti] = u8::from(nnz != 0);
                left_nnz[li] = u8::from(nnz != 0);
                nnz_total += nnz;
            }
        }
    }

    // If there were no coded coeffs despite the MB not being marked skip,
    // we MUST not do the inner loop filter or IDCT.
    if nnz_total == 0 {
        mb.skip = true;
    }
}

/// Full per-macroblock decode state.
#[derive(Clone, Default)]
pub struct MbState {
    pub skip: bool,
    pub mode: u8,
    pub ref_frame: FrameType,
    pub partitioning: u8,
    pub chroma_pred_mode: u8,
    pub segment: usize,
    pub intra4x4_pred_mode: [u8; 16],
    pub mv: Mv,
    pub bmv: [Mv; 16],
}

// FrameType's Default impl lives in types.rs next to the enum definition.
