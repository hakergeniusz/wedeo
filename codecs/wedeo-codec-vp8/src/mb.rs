// VP8 macroblock mode and motion-vector decoding.
//
// Translated from FFmpeg libavcodec/vp8.c: vp8_decode_mvs, decode_splitmvs,
// decode_intra4x4_modes, decode_mb_mode, get_submv_prob, read_mv_component,
// clamp_mv and the associated tables in vp8data.h.
// LGPL-2.1-or-later, same as FFmpeg.

use crate::bool_decoder::BoolDecoder;
use crate::coeff::MbState;
use crate::data::*;
use crate::header::HeaderInfo;
use crate::types::*;

/// `get_submv_prob` from vp8.c. `left`/`top` are motion vectors (the 32-bit
/// raw value in FFmpeg; equality and zero-ness are all that matter here).
#[inline]
fn get_submv_prob(left: Mv, top: Mv) -> &'static [u8; 3] {
    let l = !left.is_zero();
    let t = !top.is_zero();
    if left == top {
        &SUBMV_PROB[4 - usize::from(l)]
    } else if !t {
        &SUBMV_PROB[2]
    } else {
        &SUBMV_PROB[1 - usize::from(l)]
    }
}

/// `read_mv_component` from vp8.c (VP8 path).
fn read_mv_component(c: &mut BoolDecoder<'_>, p: &[u8; 19]) -> i32 {
    let mut x = 0i32;
    if c.get_prob(p[0]) {
        for i in 0..3 {
            x += i32::from(c.get_prob(p[9 + i])) << i;
        }
        for i in (4..=9).rev() {
            x += i32::from(c.get_prob(p[9 + i])) << i;
        }
        if (x & 0xfff0) == 0 || c.get_prob(p[12]) {
            x += 8;
        }
    } else {
        // small_mvtree
        let mut idx = 2usize;
        let mut bit = c.get_prob(p[idx]);
        idx += 1 + 3 * usize::from(bit);
        x += 4 * i32::from(bit);
        bit = c.get_prob(p[idx]);
        idx += 1 + usize::from(bit);
        x += 2 * i32::from(bit);
        x += i32::from(c.get_prob(p[idx]));
    }
    if x != 0 && c.get_prob(p[1]) { -x } else { x }
}

/// `clamp_mv` from vp8.c.
#[inline]
fn clamp_mv(bounds: &MvBounds, mv: Mv) -> Mv {
    let cx = (mv.x as i32).clamp(bounds.min_x, bounds.max_x);
    let cy = (mv.y as i32).clamp(bounds.min_y, bounds.max_y);
    Mv::new(
        cx.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
        cy.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
    )
}

/// `decode_splitmvs` from vp8.c. Returns the number of pieces decoded.
fn decode_splitmvs(
    c: &mut BoolDecoder<'_>,
    header: &HeaderInfo,
    mb: &mut MbState,
    left: &MbState,
    top: &MbState,
) -> usize {
    let part_idx = if c.get_prob(MBSPLIT_PROB[0]) {
        if c.get_prob(MBSPLIT_PROB[1]) {
            SPLITMVMODE_16x8 + u8::from(c.get_prob(MBSPLIT_PROB[2]))
        } else {
            SPLITMVMODE_8x8
        }
    } else {
        SPLITMVMODE_4x4
    } as usize;

    let num = MBSPLIT_COUNT[part_idx] as usize;
    let mbsplits_cur = MBSPLITS[part_idx];
    let firstidx = MBFIRSTIDX[part_idx];
    mb.partitioning = part_idx as u8;

    let mbsplits_left = MBSPLITS[left.partitioning as usize];
    let mbsplits_top = MBSPLITS[top.partitioning as usize];

    let mut bmv = mb.bmv;
    for n in 0..num {
        let k = firstidx[n] as usize;
        let left_mv = if k & 3 == 0 {
            left.bmv[mbsplits_left[k + 3] as usize]
        } else {
            bmv[mbsplits_cur[k - 1] as usize]
        };
        let above_mv = if k <= 3 {
            top.bmv[mbsplits_top[k + 12] as usize]
        } else {
            bmv[mbsplits_cur[k - 4] as usize]
        };

        let prob = get_submv_prob(left_mv, above_mv);
        if c.get_prob(prob[0]) {
            if c.get_prob(prob[1]) {
                if c.get_prob(prob[2]) {
                    let dy = read_mv_component(c, &header.probs.mvc[0]);
                    let dx = read_mv_component(c, &header.probs.mvc[1]);
                    bmv[n] = Mv::new(
                        (mb.mv.x as i32 + dx).clamp(i16::MIN as i32, i16::MAX as i32) as i16,
                        (mb.mv.y as i32 + dy).clamp(i16::MIN as i32, i16::MAX as i32) as i16,
                    );
                } else {
                    bmv[n] = Mv::new(0, 0);
                }
            } else {
                bmv[n] = above_mv;
            }
        } else {
            bmv[n] = left_mv;
        }
    }
    mb.bmv = bmv;
    num
}

/// `vp8_decode_mvs` from vp8.c.
pub fn decode_mvs(
    c: &mut BoolDecoder<'_>,
    header: &HeaderInfo,
    mb: &mut MbState,
    left: &MbState,
    top: &MbState,
    topleft: &MbState,
    bounds: &MvBounds,
) {
    let edges = [top, left, topleft];
    let mut near_mv = [Mv::new(0, 0); 4];
    let mut cnt = [0u8; 4];
    let mut idx = 0usize;
    let cur_sign_bias = header.sign_bias[mb.ref_frame.index()];

    for (n, edge) in edges.iter().enumerate() {
        if edge.ref_frame == FrameType::Current {
            continue;
        }
        let mut mv = edge.mv;
        if !mv.is_zero() {
            if cur_sign_bias != header.sign_bias[edge.ref_frame.index()] {
                mv = Mv::new(-mv.x, -mv.y);
            }
            if n == 0 || mv != near_mv[idx] {
                idx += 1;
                near_mv[idx] = mv;
            }
            cnt[idx] += 1 + u8::from(n != 2);
        } else {
            cnt[0] += 1 + u8::from(n != 2);
        }
    }

    mb.partitioning = SPLITMVMODE_NONE;
    if c.get_prob(MODE_CONTEXTS[cnt[0] as usize][0]) {
        mb.mode = MVMODE_MV;
        if cnt[3] != 0 && near_mv[1] == near_mv[3] {
            cnt[1] += 1;
        }
        if cnt[2] > cnt[1] {
            cnt.swap(1, 2);
            near_mv.swap(1, 2);
        }

        if c.get_prob(MODE_CONTEXTS[cnt[1] as usize][1]) {
            if c.get_prob(MODE_CONTEXTS[cnt[2] as usize][2]) {
                let pick = if cnt[1] >= cnt[0] { 1 } else { 0 };
                mb.mv = clamp_mv(bounds, near_mv[pick]);
                let split_cnt =
                    (u8::from(left.mode == MVMODE_SPLIT) + u8::from(top.mode == MVMODE_SPLIT)) * 2
                        + u8::from(topleft.mode == MVMODE_SPLIT);
                if c.get_prob(MODE_CONTEXTS[split_cnt as usize][3]) {
                    mb.mode = MVMODE_SPLIT;
                    let num = decode_splitmvs(c, header, mb, left, top);
                    mb.mv = mb.bmv[num - 1];
                } else {
                    let dy = read_mv_component(c, &header.probs.mvc[0]);
                    let dx = read_mv_component(c, &header.probs.mvc[1]);
                    mb.mv = Mv::new(
                        (mb.mv.x as i32 + dx).clamp(i16::MIN as i32, i16::MAX as i32) as i16,
                        (mb.mv.y as i32 + dy).clamp(i16::MIN as i32, i16::MAX as i32) as i16,
                    );
                    mb.bmv[0] = mb.mv;
                }
            } else {
                mb.mv = clamp_mv(bounds, near_mv[2]);
                mb.bmv[0] = mb.mv;
            }
        } else {
            mb.mv = clamp_mv(bounds, near_mv[1]);
            mb.bmv[0] = mb.mv;
        }
    } else {
        mb.mode = MVMODE_ZERO;
        mb.mv = Mv::new(0, 0);
        mb.bmv[0] = mb.mv;
    }
}

/// `decode_intra4x4_modes` from vp8.c.
pub fn decode_intra4x4_modes(
    c: &mut BoolDecoder<'_>,
    header: &HeaderInfo,
    mb: &mut MbState,
    mb_x: usize,
    intra4x4_top: &[u8],
    intra4x4_left: &mut [u8; 4],
) {
    if header.keyframe {
        for y in 0..4 {
            for x in 0..4 {
                let probs = &PRED4X4_PROB_INTRA[intra4x4_top[4 * mb_x + x] as usize]
                    [intra4x4_left[y] as usize];
                let mode = c.get_tree(&PRED4X4_TREE, probs) as u8;
                mb.intra4x4_pred_mode[4 * y + x] = mode;
                intra4x4_left[y] = mode;
                // The top array is shared across macroblock rows; we cannot
                // write to it through a shared reference, so the caller
                // updates it after this call.
            }
        }
        // Write the updated top entries back (handled via a separate slice
        // in decode_mb_mode below). Here we only need to remember them.
    } else {
        for i in 0..16 {
            mb.intra4x4_pred_mode[i] = c.get_tree(&PRED4X4_TREE, &PRED4X4_PROB_INTER) as u8;
        }
    }
}

/// `decode_mb_mode` from vp8.c.
#[allow(clippy::too_many_arguments)]
pub fn decode_mb_mode(
    c: &mut BoolDecoder<'_>,
    header: &HeaderInfo,
    mb: &mut MbState,
    mb_x: usize,
    mb_y: usize,
    seg_map_prev: Option<&[u8]>,
    seg_map_cur: &mut [u8],
    left: &MbState,
    top: &MbState,
    topleft: &MbState,
    bounds: &MvBounds,
    intra4x4_top: &mut [u8],
    intra4x4_left: &mut [u8; 4],
) {
    let mb_xy = mb_y * header.mb_width + mb_x;

    // Segmentation.
    let mut segment = if header.segmentation.update_map {
        let bit = usize::from(c.get_prob(header.probs.segmentid[0]));
        (usize::from(c.get_prob(header.probs.segmentid[1 + bit])) + 2 * bit) as u8
    } else if header.segmentation.enabled {
        seg_map_prev.map_or(seg_map_cur[mb_xy], |m| m[mb_xy])
    } else {
        0
    } as usize;
    if !header.segmentation.enabled {
        segment = 0;
    }
    seg_map_cur[mb_xy] = segment as u8;
    mb.segment = segment;

    mb.skip = header.mbskip_enabled && c.get_prob(header.probs.mbskip);

    if header.keyframe {
        mb.mode = c.get_tree(&PRED16X16_TREE_INTRA, &PRED16X16_PROB_INTRA) as u8;
        if mb.mode == MODE_I4x4 {
            decode_intra4x4_modes_keyframe(c, mb, intra4x4_top, intra4x4_left, mb_x);
        } else {
            let v = PRED4X4_MODE[mb.mode as usize];
            for x in 0..4 {
                intra4x4_top[4 * mb_x + x] = v;
            }
            *intra4x4_left = [v; 4];
            mb.intra4x4_pred_mode = [v; 16];
        }
        mb.chroma_pred_mode = c.get_tree(&PRED8X8C_TREE, &PRED8X8C_PROB_INTRA) as u8;
        mb.ref_frame = FrameType::Current;
    } else if c.get_prob(header.probs.intra) {
        // Inter MB, 16.2.
        if c.get_prob(header.probs.last) {
            mb.ref_frame = if c.get_prob(header.probs.golden) {
                FrameType::AltRef
            } else {
                FrameType::Golden
            };
        } else {
            mb.ref_frame = FrameType::Previous;
        }
        decode_mvs(c, header, mb, left, top, topleft, bounds);
    } else {
        // Intra MB, 16.1.
        mb.mode = c.get_tree(&PRED16X16_TREE_INTER, &header.probs.pred16x16) as u8;
        if mb.mode == MODE_I4x4 {
            decode_intra4x4_modes(c, header, mb, mb_x, intra4x4_top, intra4x4_left);
        }
        mb.chroma_pred_mode = c.get_tree(&PRED8X8C_TREE, &header.probs.pred8x8c) as u8;
        mb.ref_frame = FrameType::Current;
        mb.partitioning = SPLITMVMODE_NONE;
        mb.bmv[0] = Mv::new(0, 0);
    }
}

/// Keyframe 4x4 prediction-mode decode with a mutable top array (the
/// keyframe path updates `top[x]` after every block).
fn decode_intra4x4_modes_keyframe(
    c: &mut BoolDecoder<'_>,
    mb: &mut MbState,
    top: &mut [u8],
    left: &mut [u8; 4],
    mb_x: usize,
) {
    for y in 0..4 {
        for x in 0..4 {
            let ti = 4 * mb_x + x;
            let probs = &PRED4X4_PROB_INTRA[top[ti] as usize][left[y] as usize];
            let mode = c.get_tree(&PRED4X4_TREE, probs) as u8;
            mb.intra4x4_pred_mode[4 * y + x] = mode;
            left[y] = mode;
            top[ti] = mode;
        }
    }
}
