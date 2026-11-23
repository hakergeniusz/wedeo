//! Residual dequantisation, inverse transform and reconstruction.
//!
//! FFmpeg reference: `libavcodec/hevc/cabac.c`
//! (`ff_hevc_hls_residual_coding`, QP/scaling-list block) and
//! `libavcodec/hevc/dsp_template.c` (`add_residual`).

use wedeo_codec_hevc_cabac::{Cabac, ResidualInfo, ResidualParams, ScanType};
use wedeo_codec_hevc_dsp as dsp;
use wedeo_codec_hevc_ps::{Pps, ScalingList, Sps};
use wedeo_core::Result;

use crate::picture::Picture;
use crate::tables::{chroma_qp, scale_for_qp};

/// Inputs to [`decode_residual`] that depend on SPS/PPS/CU state.
pub struct ResidualCtx<'a> {
    pub sps: &'a Sps,
    pub pps: &'a Pps,
    pub slice_cb_qp_offset: i32,
    pub slice_cr_qp_offset: i32,
    pub cu_qp_offset_cb: i32,
    pub cu_qp_offset_cr: i32,
    pub cu_transquant_bypass: bool,
    /// `qp_y` for the current CU.
    pub qp_y: i32,
    pub is_intra: bool,
    /// Luma intra prediction mode (needed for RDPCM / scan selection).
    pub intra_pred_mode: u8,
    pub intra_pred_mode_c: u8,
}

/// Decode, dequantise, inverse-transform and add one residual block.
///
/// `x0`/`y0` are luma coordinates; `c_idx` selects the plane. `coeffs` is a
/// scratch buffer of at least `1 << (2 * log2_trafo_size)` i32 entries.
#[allow(clippy::too_many_arguments)] // mirrors FFmpeg's ff_hevc_hls_residual_coding
pub fn decode_residual(
    cabac: &mut Cabac<'_>,
    ctx: &ResidualCtx<'_>,
    picture: &mut Picture,
    x0: usize,
    y0: usize,
    log2_trafo_size: u32,
    scan_idx: ScanType,
    c_idx: usize,
    coeffs: &mut [i32],
) -> Result<()> {
    let sps = ctx.sps;
    let pps = ctx.pps;
    let bit_depth = if c_idx == 0 {
        sps.bit_depth as u8
    } else {
        sps.bit_depth_chroma as u8
    };
    let size = 1usize << log2_trafo_size;

    let params = ResidualParams {
        c_idx,
        log2_trafo_size,
        scan_idx,
        transform_skip_enabled: pps.transform_skip_enabled_flag,
        log2_max_transform_skip_block_size: pps.log2_max_transform_skip_block_size,
        cu_transquant_bypass: ctx.cu_transquant_bypass,
        is_intra: ctx.is_intra,
        transform_skip_context_enabled: sps.range_ext.transform_skip_context_enabled,
        sign_data_hiding_enabled: pps.sign_data_hiding_flag,
        persistent_rice_adaptation_enabled: sps.range_ext.persistent_rice_adaptation_enabled,
        implicit_rdpcm_active: sps.range_ext.implicit_rdpcm_enabled
            && ctx.is_intra
            && (ctx.intra_pred_mode == 10 || ctx.intra_pred_mode == 26),
        explicit_rdpcm_enabled: sps.range_ext.explicit_rdpcm_enabled,
    };

    let info: ResidualInfo = crate::coefficients::decode_coefficients(cabac, &params, coeffs)?;

    // ---- QP / scaling-list dequantisation (cabac.c 1120-1189, 1542-1558) ----
    let (shift, add, scale, use_scaling) = if ctx.cu_transquant_bypass {
        (0i32, 0i32, 0i32, false)
    } else {
        let qp = if c_idx == 0 {
            ctx.qp_y + sps.qp_bd_offset as i32
        } else {
            let offset = if c_idx == 1 {
                pps.cb_qp_offset + ctx.slice_cb_qp_offset + ctx.cu_qp_offset_cb
            } else {
                pps.cr_qp_offset + ctx.slice_cr_qp_offset + ctx.cu_qp_offset_cr
            };
            chroma_qp(ctx.qp_y, offset, sps.chroma_format_idc, sps.qp_bd_offset)
        };
        let scale = scale_for_qp(qp).unwrap_or(0);
        let shift = (bit_depth as i32) + log2_trafo_size as i32 - 5;
        let add = 1i32 << (shift - 1);
        let use_scaling =
            sps.scaling_list_enabled && !(info.transform_skip_flag != 0 && log2_trafo_size > 2);
        (shift, add, scale, use_scaling)
    };

    let sl: Option<&ScalingList> = if use_scaling {
        Some(if pps.scaling_list_data_present_flag {
            &pps.scaling_list
        } else {
            &sps.scaling_list
        })
    } else {
        None
    };

    let size_id = (log2_trafo_size - 2) as usize;
    let matrix_id = usize::from(!ctx.is_intra) * 3 + c_idx;

    if !ctx.cu_transquant_bypass {
        let dc_scale = sl.map_or(16, |list| {
            if log2_trafo_size >= 4 {
                list.sl_dc[(log2_trafo_size - 4) as usize][matrix_id]
            } else {
                16
            }
        });
        for y in 0..size {
            for x in 0..size {
                let i = y * size + x;
                if coeffs[i] == 0 {
                    continue;
                }
                let mut scale_m = dc_scale;
                if let Some(list) = sl
                    && (y != 0 || x != 0 || log2_trafo_size < 4)
                {
                    let pos = match log2_trafo_size {
                        3 => (y << 3) + x,
                        4 => ((y >> 1) << 3) + (x >> 1),
                        5 => ((y >> 2) << 3) + (x >> 2),
                        _ => (y << 2) + x,
                    };
                    if size_id < 4 {
                        scale_m = list.sl[size_id][matrix_id][pos.min(63)];
                    }
                }
                let level = (i64::from(coeffs[i]) * i64::from(scale) * i64::from(scale_m)
                    + i64::from(add))
                    >> shift;
                coeffs[i] = level.clamp(-32768, 32767) as i32;
            }
        }
    }

    // ---- inverse transform ----
    let mut transformed = vec![0i16; size * size];
    for (i, &v) in coeffs.iter().take(size * size).enumerate() {
        transformed[i] = v as i16;
    }

    if ctx.cu_transquant_bypass {
        if info.explicit_rdpcm_flag != 0
            || (sps.range_ext.implicit_rdpcm_enabled
                && ctx.is_intra
                && (ctx.intra_pred_mode == 10 || ctx.intra_pred_mode == 26))
        {
            let mode = if sps.range_ext.implicit_rdpcm_enabled {
                u8::from(ctx.intra_pred_mode == 26)
            } else {
                info.explicit_rdpcm_dir_flag as u8
            };
            dsp::transform::transform_rdpcm(&mut transformed, log2_trafo_size as u8, mode)
                .map_err(|e| wedeo_core::Error::Other(e.to_string()))?;
        }
    } else if info.transform_skip_flag != 0 {
        dsp::transform::dequant(&mut transformed, log2_trafo_size as u8, bit_depth)
            .map_err(|e| wedeo_core::Error::Other(e.to_string()))?;
        if info.explicit_rdpcm_flag != 0
            || (sps.range_ext.implicit_rdpcm_enabled
                && ctx.is_intra
                && (ctx.intra_pred_mode == 10 || ctx.intra_pred_mode == 26))
        {
            let mode = if info.explicit_rdpcm_flag != 0 {
                info.explicit_rdpcm_dir_flag as u8
            } else {
                u8::from(ctx.intra_pred_mode == 26)
            };
            dsp::transform::transform_rdpcm(&mut transformed, log2_trafo_size as u8, mode)
                .map_err(|e| wedeo_core::Error::Other(e.to_string()))?;
        }
    } else if ctx.is_intra && c_idx == 0 && log2_trafo_size == 2 {
        dsp::transform::transform_4x4_luma(&mut transformed, bit_depth)
            .map_err(|e| wedeo_core::Error::Other(e.to_string()))?;
    } else if info.max_xy == 0 {
        dsp::transform::idct_dc(&mut transformed, log2_trafo_size as u8, bit_depth)
            .map_err(|e| wedeo_core::Error::Other(e.to_string()))?;
    } else {
        dsp::transform::idct(
            &mut transformed,
            log2_trafo_size as u8,
            info.col_limit.max(0) as usize,
            bit_depth,
        )
        .map_err(|e| wedeo_core::Error::Other(e.to_string()))?;
    }

    // ---- add residual (FFmpeg's FUNC(add_residual)) ----
    let (px, py) = if c_idx == 0 {
        (x0, y0)
    } else {
        (x0 >> sps.hshift[1], y0 >> sps.vshift[1])
    };
    let max = ((1u32 << bit_depth) - 1) as i32;
    for j in 0..size {
        for i in 0..size {
            let pidx = picture.index(c_idx, px + i, py + j);
            let v = i32::from(picture.planes[c_idx][pidx]) + i32::from(transformed[j * size + i]);
            picture.planes[c_idx][pidx] = v.clamp(0, max) as u16;
        }
    }
    Ok(())
}
