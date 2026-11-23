// VP8 frame header parsing.
//
// Translated from FFmpeg's `vp8_decode_frame_header` and its helpers in
// libavcodec/vp8.c: parse_segment_info, update_lf_deltas, setup_partitions,
// vp8_get_quants, ref_to_update, update_refs, vp78_reset_probability_tables,
// vp78_update_probability_tables and
// vp78_update_pred16x16_pred8x8_mvc_probabilities.
//
// LGPL-2.1-or-later, same as FFmpeg.

use wedeo_core::error::{Error, Result};

use crate::bool_decoder::BoolDecoder;
use crate::data::*;
use crate::types::*;

/// Parsed frame header state needed for macroblock decoding.
#[derive(Clone)]
pub struct HeaderInfo {
    pub keyframe: bool,
    pub profile: u8,
    pub invisible: bool,
    pub width: u32,
    pub height: u32,
    pub mb_width: usize,
    pub mb_height: usize,
    pub colorspace: bool,
    pub fullrange: bool,
    pub segmentation: Segmentation,
    pub filter_simple: bool,
    pub filter_level: u8,
    pub filter_sharpness: u8,
    pub lf_delta: LfDelta,
    pub qmat: [QMat; 4],
    pub probs: Probabilities,
    pub update_last: bool,
    /// `VP8_FRAME_NONE` (-1) or a `FrameType` index.
    pub update_golden: i32,
    pub update_altref: i32,
    pub update_probabilities: bool,
    pub mbskip_enabled: bool,
    pub sign_bias: [i8; 4],
    pub num_coeff_partitions: usize,
    pub header_size: usize,
}

/// Result of header parsing: header state, the header range decoder
/// (positioned just after the header bits), and the coefficient partitions.
pub struct ParsedHeader<'a> {
    pub info: HeaderInfo,
    pub coder: BoolDecoder<'a>,
    pub coeff_partitions: Vec<&'a [u8]>,
}

/// Create the default probability table for a keyframe.
/// Mirrors `vp78_reset_probability_tables` plus the keyframe setup in
/// `vp8_decode_frame_header`.
pub fn initial_probabilities() -> Probabilities {
    let mut p = Probabilities::default();
    for i in 0..4 {
        for j in 0..16 {
            let band = COEFF_BAND[j] as usize;
            p.token[i][j] = TOKEN_DEFAULT_PROBS[i][band];
        }
    }
    p.pred16x16 = PRED16X16_PROB_INTER;
    p.pred8x8c = PRED8X8C_PROB_INTER;
    p.mvc = MV_DEFAULT_PROB;
    p
}

/// Parse a VP8 frame header from `data`, using `prev_probs` for interframes
/// and `width`/`height` as the current codec dimensions.
pub fn decode_frame_header<'a>(
    data: &'a [u8],
    prev_probs: &Probabilities,
    prev_lf_delta: &LfDelta,
    width: u32,
    height: u32,
) -> Result<ParsedHeader<'a>> {
    if data.len() < 3 {
        return Err(Error::InvalidData);
    }

    let keyframe = data[0] & 1 == 0;
    let profile = (data[0] >> 1) & 7;
    let invisible = data[0] & 0x10 == 0;
    let header_size =
        ((data[0] as usize) | ((data[1] as usize) << 8) | ((data[2] as usize) << 16)) >> 5;

    let mut buf = &data[3..];
    let mut buf_size = buf.len();

    if header_size > buf_size.saturating_sub(7 * usize::from(keyframe)) {
        return Err(Error::InvalidData);
    }

    let mut probs = if keyframe {
        initial_probabilities()
    } else {
        prev_probs.clone()
    };

    let mut width = width;
    let mut height = height;
    let mut colorspace = false;
    let mut fullrange = false;
    let mut segmentation = Segmentation::default();
    // FFmpeg keeps lf_delta in VP8Context across frames; when `update` is not
    // set the previous frame's deltas remain in effect.
    let mut lf_delta = *prev_lf_delta;
    let mut update_golden = FRAME_NONE;
    let mut update_altref = FRAME_NONE;
    let mut sign_bias = [0i8; 4];

    if keyframe {
        if buf.len() < 7 {
            return Err(Error::InvalidData);
        }
        if buf[0] as u32 | ((buf[1] as u32) << 8) | ((buf[2] as u32) << 16) != 0x2a_019d {
            return Err(Error::InvalidData);
        }
        width = (u16::from_le_bytes([buf[3], buf[4]]) & 0x3fff) as u32;
        height = (u16::from_le_bytes([buf[5], buf[6]]) & 0x3fff) as u32;
        // hscale = buf[4] >> 6; vscale = buf[6] >> 6; upscaling unsupported.
        if buf[4] >> 6 != 0 || buf[6] >> 6 != 0 {
            return Err(Error::PatchwelcomeNotImplemented);
        }
        buf = &buf[7..];
        buf_size -= 7;
        update_golden = FrameType::Current as i32;
        update_altref = FrameType::Current as i32;
        segmentation = Segmentation::default();
        lf_delta = LfDelta::default();
    }

    if header_size > buf.len() {
        return Err(Error::InvalidData);
    }
    let mut c = BoolDecoder::new(&buf[..header_size]).ok_or(Error::InvalidData)?;
    buf = &buf[header_size..];
    buf_size -= header_size;

    if keyframe {
        colorspace = c.get();
        fullrange = c.get();
    }

    segmentation.enabled = c.get();
    if segmentation.enabled {
        segmentation.update_map = c.get();
        segmentation.update_feature_data = c.get();
        if segmentation.update_feature_data {
            segmentation.absolute_vals = c.get();
            for i in 0..4 {
                segmentation.base_quant[i] = c.get_sint(7) as i8;
            }
            for i in 0..4 {
                segmentation.filter_level[i] = c.get_sint(6) as i8;
            }
        }
        if segmentation.update_map {
            for i in 0..3 {
                probs.segmentid[i] = if c.get() { c.get_uint(8) as u8 } else { 255 };
            }
        }
    } else {
        segmentation.update_map = false;
    }

    let filter_simple = c.get();
    let filter_level = c.get_uint(6) as u8;
    let filter_sharpness = c.get_uint(3) as u8;

    lf_delta.enabled = c.get();
    if lf_delta.enabled {
        lf_delta.update = c.get();
        if lf_delta.update {
            for i in 0..4 {
                if c.get() {
                    let mut v = c.get_uint(6) as i8;
                    if c.get() {
                        v = -v;
                    }
                    lf_delta.ref_delta[i] = v;
                }
            }
            for i in MODE_I4x4 as usize..=MVMODE_SPLIT as usize {
                if c.get() {
                    let mut v = c.get_uint(6) as i8;
                    if c.get() {
                        v = -v;
                    }
                    lf_delta.mode[i] = v;
                }
            }
        }
    }

    // --- setup_partitions ---
    // num_coeff_partitions = 1 << vp89_rac_get_uint(c, 2)
    let num_coeff_partitions = 1usize << c.get_uint(2);
    if num_coeff_partitions > 8 {
        return Err(Error::InvalidData);
    }
    let size_bytes = 3 * (num_coeff_partitions - 1);
    if buf_size < size_bytes {
        return Err(Error::InvalidData);
    }
    let sizes_start = buf;
    buf = &buf[size_bytes..];
    buf_size -= size_bytes;

    let mut coeff_partitions = Vec::with_capacity(num_coeff_partitions);
    for i in 0..num_coeff_partitions - 1 {
        let sz = (sizes_start[3 * i] as usize)
            | ((sizes_start[3 * i + 1] as usize) << 8)
            | ((sizes_start[3 * i + 2] as usize) << 16);
        if buf_size < sz {
            return Err(Error::InvalidData);
        }
        if sz == 0 {
            return Err(Error::InvalidData);
        }
        coeff_partitions.push(&buf[..sz]);
        buf = &buf[sz..];
        buf_size -= sz;
    }
    // The last partition takes all remaining bytes.
    if buf_size == 0 {
        return Err(Error::InvalidData);
    }
    coeff_partitions.push(&buf[..buf_size]);

    // --- vp8_get_quants ---
    let yac_qi = c.get_uint(7) as i32;
    let ydc_delta = c.get_sint(4);
    let y2dc_delta = c.get_sint(4);
    let y2ac_delta = c.get_sint(4);
    let uvdc_delta = c.get_sint(4);
    let uvac_delta = c.get_sint(4);

    let mut qmat = [QMat::default(); 4];
    for i in 0..4 {
        let base_qi = if segmentation.enabled {
            let mut b = segmentation.base_quant[i] as i32;
            if !segmentation.absolute_vals {
                b += yac_qi;
            }
            b
        } else {
            yac_qi
        };
        let clip = |v: i32| (v as u32 & 127) as usize;
        qmat[i].luma_qmul[0] = DC_QLOOKUP[clip(base_qi + ydc_delta)] as i32;
        qmat[i].luma_qmul[1] = AC_QLOOKUP[clip(base_qi)] as i32;
        qmat[i].luma_dc_qmul[0] = DC_QLOOKUP[clip(base_qi + y2dc_delta)] as i32 * 2;
        // 101581>>16 is equivalent to 155/100.
        qmat[i].luma_dc_qmul[1] = (AC_QLOOKUP[clip(base_qi + y2ac_delta)] as i32 * 101581) >> 16;
        qmat[i].chroma_qmul[0] = DC_QLOOKUP[clip(base_qi + uvdc_delta)] as i32;
        qmat[i].chroma_qmul[1] = AC_QLOOKUP[clip(base_qi + uvac_delta)] as i32;
        qmat[i].luma_dc_qmul[1] = qmat[i].luma_dc_qmul[1].max(8);
        qmat[i].chroma_qmul[0] = qmat[i].chroma_qmul[0].min(132);
    }

    if !keyframe {
        // update_refs
        let update_golden_flag = c.get();
        let update_altref_flag = c.get();
        update_golden = ref_to_update(&mut c, update_golden_flag, FrameType::Golden as i32);
        update_altref = ref_to_update(&mut c, update_altref_flag, FrameType::AltRef as i32);
        sign_bias[FrameType::Golden.index()] = i8::from(c.get());
        sign_bias[FrameType::AltRef.index()] = i8::from(c.get());
    }

    let update_probabilities = c.get();
    // The caller saved the previous probabilities before header updates if
    // !update_probabilities; no need to do it here.

    let update_last = keyframe || c.get();

    // --- vp78_update_probability_tables ---
    for i in 0..4 {
        for j in 0..8 {
            for k in 0..3 {
                for l in 0..11 {
                    if c.get_prob(TOKEN_UPDATE_PROBS[i][j][k][l]) {
                        let prob = c.get_uint(8) as u8;
                        for &m in COEFF_BAND_INDEXES[j].iter() {
                            if m < 0 {
                                break;
                            }
                            probs.token[i][m as usize][k][l] = prob;
                        }
                    }
                }
            }
        }
    }

    let mbskip_enabled = c.get();
    if mbskip_enabled {
        probs.mbskip = c.get_uint(8) as u8;
    }

    if !keyframe {
        probs.intra = c.get_uint(8) as u8;
        probs.last = c.get_uint(8) as u8;
        probs.golden = c.get_uint(8) as u8;
        // pred16x16 / pred8x8c updates
        if c.get() {
            for i in 0..4 {
                probs.pred16x16[i] = c.get_uint(8) as u8;
            }
        }
        if c.get() {
            for i in 0..3 {
                probs.pred8x8c[i] = c.get_uint(8) as u8;
            }
        }
        // MV probability updates
        for i in 0..2 {
            for j in 0..19 {
                if c.get_prob(MV_UPDATE_PROB[i][j]) {
                    probs.mvc[i][j] = c.get_nn() as u8;
                }
            }
        }
    }

    let mb_width = width.div_ceil(16) as usize;
    let mb_height = height.div_ceil(16) as usize;

    Ok(ParsedHeader {
        info: HeaderInfo {
            keyframe,
            profile,
            invisible,
            width,
            height,
            mb_width,
            mb_height,
            colorspace,
            fullrange,
            segmentation,
            filter_simple,
            filter_level,
            filter_sharpness,
            lf_delta,
            qmat,
            probs,
            update_last,
            update_golden,
            update_altref,
            update_probabilities,
            mbskip_enabled,
            sign_bias,
            num_coeff_partitions,
            header_size,
        },
        coder: c,
        coeff_partitions,
    })
}

/// `ref_to_update` from vp8.c: decide which reference buffer a golden/altref
/// update copies from.
fn ref_to_update(c: &mut BoolDecoder<'_>, update: bool, ref_type: i32) -> i32 {
    if update {
        return FrameType::Current as i32;
    }
    match c.get_uint(2) {
        1 => FrameType::Previous as i32,
        2 => {
            if ref_type == FrameType::Golden as i32 {
                FrameType::AltRef as i32
            } else {
                FrameType::Golden as i32
            }
        }
        _ => FRAME_NONE,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_probs_reset_token_table() {
        let p = initial_probabilities();
        assert_eq!(
            p.token[0][0][0][0],
            TOKEN_DEFAULT_PROBS[0][COEFF_BAND[0] as usize][0][0]
        );
        assert_eq!(p.mvc, MV_DEFAULT_PROB);
        assert_eq!(p.pred16x16, PRED16X16_PROB_INTER);
    }

    #[test]
    fn short_packet_rejected() {
        assert!(
            decode_frame_header(
                &[0u8; 2],
                &initial_probabilities(),
                &LfDelta::default(),
                0,
                0
            )
            .is_err()
        );
    }
}
