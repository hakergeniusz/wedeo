//! CABAC residual coefficient level decoding.
//!
//! FFmpeg reference: `libavcodec/hevc/cabac.c`,
//! `ff_hevc_hls_residual_coding()` (the whole bin-decoding part, lines
//! 1138-1560). This is a direct port using `Cabac`'s public low-level API.
//!
//! The shared `wedeo-codec-hevc-cabac` crate exposes the arithmetic engine
//! and every individual syntax-element helper, but its high-level
//! `decode_residual_coefficients()` wrapper consumes a different number of
//! bins for some coefficient levels than FFmpeg's C loop. Decoding the levels
//! here from the same primitives keeps the bin stream bit-for-bit aligned
//! with FFmpeg.

use wedeo_codec_hevc_cabac::{Cabac, ResidualInfo, ResidualParams, ScanType, tables};
use wedeo_core::Result;

/// Decode signed quantisation levels into `coeffs[y * size + x]`.
pub fn decode_coefficients(
    cabac: &mut Cabac<'_>,
    params: &ResidualParams,
    coeffs: &mut [i32],
) -> Result<ResidualInfo> {
    let c_idx = params.c_idx;
    let log2 = params.log2_trafo_size as usize;
    let size = 1usize << log2;
    if coeffs.len() < size * size {
        return Err(wedeo_core::Error::InvalidArgument);
    }
    coeffs[..size * size].fill(0);
    let mut info = ResidualInfo::default();

    if params.transform_skip_enabled && log2 <= params.log2_max_transform_skip_block_size as usize {
        info.transform_skip_flag = cabac.decode_transform_skip_flag(c_idx);
    }
    let bypass_ctx = info.transform_skip_flag != 0 || params.cu_transquant_bypass;
    if !params.is_intra && params.explicit_rdpcm_enabled && bypass_ctx {
        info.explicit_rdpcm_flag = cabac.decode_explicit_rdpcm_flag(c_idx);
        if info.explicit_rdpcm_flag != 0 {
            info.explicit_rdpcm_dir_flag = cabac.decode_explicit_rdpcm_dir_flag(c_idx) as i32;
        }
    }

    let (mut last_x, mut last_y) = cabac.decode_last_significant_coeff_prefix(c_idx, log2 as u32);
    if last_x > 3 {
        let s = cabac.decode_last_significant_coeff_suffix(last_x);
        last_x = (1 << ((last_x >> 1) - 1)) * (2 + (last_x & 1)) + s;
    }
    if last_y > 3 {
        let s = cabac.decode_last_significant_coeff_suffix(last_y);
        last_y = (1 << ((last_y >> 1) - 1)) * (2 + (last_y & 1)) + s;
    }
    if params.scan_idx == ScanType::Vert {
        std::mem::swap(&mut last_x, &mut last_y);
    }
    info.last_x = last_x;
    info.last_y = last_y;

    let x_cg_last = (last_x >> 2) as usize;
    let y_cg_last = (last_y >> 2) as usize;

    // Scan tables and the number of coefficients up to the last one.
    let (scan_x_cg, scan_y_cg, scan_x_off, scan_y_off, nc): (&[u8], &[u8], &[u8], &[u8], i32) =
        match params.scan_idx {
            ScanType::Diag => {
                let lx = (last_x & 3) as usize;
                let ly = (last_y & 3) as usize;
                let mut n = tables::DIAG_SCAN4X4_INV[ly * 4 + lx] as i32;
                if size == 4 {
                    (
                        &tables::SCAN_1X1[..],
                        &tables::SCAN_1X1[..],
                        &tables::DIAG_SCAN4X4_X[..],
                        &tables::DIAG_SCAN4X4_Y[..],
                        n,
                    )
                } else if size == 8 {
                    n += (tables::DIAG_SCAN2X2_INV[y_cg_last * 2 + x_cg_last] as i32) << 4;
                    (
                        &tables::DIAG_SCAN2X2_X[..],
                        &tables::DIAG_SCAN2X2_Y[..],
                        &tables::DIAG_SCAN4X4_X[..],
                        &tables::DIAG_SCAN4X4_Y[..],
                        n,
                    )
                } else if size == 16 {
                    n += (tables::DIAG_SCAN4X4_INV[y_cg_last * 4 + x_cg_last] as i32) << 4;
                    (
                        &tables::DIAG_SCAN4X4_X[..],
                        &tables::DIAG_SCAN4X4_Y[..],
                        &tables::DIAG_SCAN4X4_X[..],
                        &tables::DIAG_SCAN4X4_Y[..],
                        n,
                    )
                } else {
                    n += (tables::DIAG_SCAN8X8_INV[y_cg_last * 8 + x_cg_last] as i32) << 4;
                    (
                        &tables::DIAG_SCAN8X8_X[..],
                        &tables::DIAG_SCAN8X8_Y[..],
                        &tables::DIAG_SCAN4X4_X[..],
                        &tables::DIAG_SCAN4X4_Y[..],
                        n,
                    )
                }
            }
            ScanType::Horiz => (
                &tables::HORIZ_SCAN2X2_X[..],
                &tables::HORIZ_SCAN2X2_Y[..],
                &tables::HORIZ_SCAN4X4_X[..],
                &tables::HORIZ_SCAN4X4_Y[..],
                tables::HORIZ_SCAN8X8_INV[last_y as usize * 8 + last_x as usize] as i32,
            ),
            ScanType::Vert => (
                &tables::HORIZ_SCAN2X2_Y[..],
                &tables::HORIZ_SCAN2X2_X[..],
                &tables::HORIZ_SCAN4X4_Y[..],
                &tables::HORIZ_SCAN4X4_X[..],
                tables::HORIZ_SCAN8X8_INV[last_x as usize * 8 + last_y as usize] as i32,
            ),
        };
    let num_coeff = nc + 1;
    info.num_coeff = num_coeff;
    let num_last_subset = (num_coeff - 1) >> 4;

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

    let x_cg_limit = (1usize << (log2 - 2)).saturating_sub(1);
    let y_cg_limit = x_cg_limit;
    let mut sig_cg = [[0u8; 8]; 8];
    let mut greater1_ctx = 1i32;

    for i in (0..=num_last_subset).rev() {
        let i = i as usize;
        let offset = i << 4;
        let x_cg = scan_x_cg[i] as usize;
        let y_cg = scan_y_cg[i] as usize;
        let mut implicit_non_zero = 0;

        if i < num_last_subset as usize && i > 0 {
            let mut ctx_cg = 0i32;
            if x_cg < x_cg_limit {
                ctx_cg += i32::from(sig_cg[x_cg + 1][y_cg]);
            }
            if y_cg < y_cg_limit {
                ctx_cg += i32::from(sig_cg[x_cg][y_cg + 1]);
            }
            sig_cg[x_cg][y_cg] = cabac.decode_significant_coeff_group_flag(c_idx, ctx_cg);
            implicit_non_zero = 1;
        } else {
            sig_cg[x_cg][y_cg] =
                u8::from((x_cg == x_cg_last && y_cg == y_cg_last) || (x_cg == 0 && y_cg == 0));
        }

        let last_scan_pos = num_coeff - offset as i32 - 1;
        let mut n_end;
        let mut sig_idx = [0u8; 16];
        let mut nb_sig: usize = 0;
        if i as i32 == num_last_subset {
            n_end = last_scan_pos - 1;
            sig_idx[0] = last_scan_pos as u8;
            nb_sig = 1;
        } else {
            n_end = 15;
        }

        let mut prev_sig = 0usize;
        if x_cg < x_cg_limit {
            prev_sig += sig_cg[x_cg + 1][y_cg] as usize;
        }
        if y_cg < y_cg_limit {
            prev_sig += (sig_cg[x_cg][y_cg + 1] as usize) << 1;
        }

        if sig_cg[x_cg][y_cg] != 0 && n_end >= 0 {
            let mut scf_offset = 0usize;
            let map: &[u8];
            if params.transform_skip_context_enabled && bypass_ctx {
                map = &tables::CTX_IDX_MAP[params.scan_idx as usize][64..80];
                scf_offset = if c_idx == 0 { 40 } else { 14 + 27 };
            } else {
                if c_idx != 0 {
                    scf_offset = 27;
                }
                if log2 == 2 {
                    map = &tables::CTX_IDX_MAP[params.scan_idx as usize][0..16];
                } else {
                    map = &tables::CTX_IDX_MAP[params.scan_idx as usize]
                        [((prev_sig + 1) << 4)..((prev_sig + 2) << 4)];
                    if c_idx == 0 {
                        if x_cg > 0 || y_cg > 0 {
                            scf_offset += 3;
                        }
                        if log2 == 3 {
                            scf_offset += if params.scan_idx == ScanType::Diag {
                                9
                            } else {
                                15
                            };
                        } else {
                            scf_offset += 21;
                        }
                    } else if log2 == 3 {
                        scf_offset += 9;
                    } else {
                        scf_offset += 12;
                    }
                }
            }

            let nb0 = nb_sig;
            let mut n = n_end;
            while n > 0 {
                let sig = cabac.decode_significant_coeff_flag(map, n as usize, scf_offset);
                sig_idx[nb_sig] = n as u8;
                nb_sig += sig as usize;
                n -= 1;
            }
            if nb_sig != nb0 {
                implicit_non_zero = 0;
            }
            if implicit_non_zero == 0 {
                if params.transform_skip_context_enabled && bypass_ctx {
                    scf_offset = if c_idx == 0 { 42 } else { 16 + 27 };
                } else if i == 0 {
                    scf_offset = if c_idx == 0 { 0 } else { 27 };
                } else {
                    scf_offset += 2;
                }
                sig_idx[nb_sig] = 0;
                nb_sig += cabac.decode_significant_coeff_flag_0(c_idx, scf_offset) as usize;
            } else {
                sig_idx[nb_sig] = 0;
                nb_sig += 1;
            }
        }

        n_end = nb_sig as i32;

        if n_end <= 0 {
            continue;
        }

        let mut c_rice_param = 0usize;
        let mut first_greater1 = -1i32;
        let mut greater1_flag = [0u8; 8];
        let mut gt1_mask = 0u32;
        let mut ctx_set = if i > 0 && c_idx == 0 { 2usize } else { 0usize };

        if params.persistent_rice_adaptation_enabled {
            let sb_type = if bypass_ctx {
                2 * usize::from(c_idx == 0) + 1
            } else {
                2 * usize::from(c_idx == 0)
            };
            c_rice_param = (cabac.stat_coeff[sb_type] / 4) as usize;
        }
        if i as i32 != num_last_subset && greater1_ctx == 0 {
            ctx_set += 1;
        }
        greater1_ctx = 1;
        let last_nz_pos = sig_idx[0] as i32;

        for m in 0..(n_end as usize).min(8) {
            let inc = (ctx_set << 2) + greater1_ctx as usize;
            let flag = cabac.decode_coeff_abs_level_greater1_flag(c_idx, inc);
            greater1_flag[m] = flag;
            gt1_mask |= (flag as u32) << m;
            greater1_ctx = (greater1_ctx + i32::from(greater1_ctx - 1 < 2)) & (flag as i32 - 1);
        }
        if gt1_mask != 0 {
            first_greater1 = gt1_mask.trailing_zeros() as i32;
        }

        let first_nz_pos = sig_idx[n_end as usize - 1] as i32;

        let sign_hidden_disabled = params.cu_transquant_bypass
            || (params.is_intra && params.implicit_rdpcm_active && info.transform_skip_flag != 0)
            || info.explicit_rdpcm_flag != 0;
        let sign_hidden = !sign_hidden_disabled && last_nz_pos - first_nz_pos >= 4;

        if first_greater1 != -1 {
            let fg = first_greater1 as usize;
            greater1_flag[fg] += cabac.decode_coeff_abs_level_greater2_flag(c_idx, ctx_set);
        }
        let nb_signs = if params.sign_data_hiding_enabled && sign_hidden {
            nb_sig - 1
        } else {
            nb_sig
        };
        let mut sign_bits = decode_signs(cabac, nb_signs);

        let mut rice_init = false;
        let mut sum_abs = 0i32;
        for m in 0..n_end as usize {
            let n = sig_idx[m] as usize;
            let x_c = (x_cg << 2) + scan_x_off[n] as usize;
            let y_c = (y_cg << 2) + scan_y_off[n] as usize;

            let mut level: i32;
            if m < 8 {
                level = 1 + i32::from(greater1_flag[m]);
                if level == if m as i32 == first_greater1 { 3 } else { 2 } {
                    let remaining = coeff_level_remaining(cabac, c_rice_param);

                    level += remaining;
                    if level > (3 << c_rice_param) {
                        c_rice_param = if params.persistent_rice_adaptation_enabled {
                            c_rice_param + 1
                        } else {
                            (c_rice_param + 1).min(4)
                        };
                    }
                    adapt_rice(cabac, params, bypass_ctx, c_idx, remaining, &mut rice_init);
                }
            } else {
                let remaining = coeff_level_remaining(cabac, c_rice_param);
                level = 1 + remaining;

                if level > (3 << c_rice_param) {
                    c_rice_param = if params.persistent_rice_adaptation_enabled {
                        c_rice_param + 1
                    } else {
                        (c_rice_param + 1).min(4)
                    };
                }
                adapt_rice(cabac, params, bypass_ctx, c_idx, remaining, &mut rice_init);
            }

            if params.sign_data_hiding_enabled && sign_hidden {
                sum_abs += level;
                if n as i32 == first_nz_pos && sum_abs & 1 != 0 {
                    level = -level;
                }
            }
            let sign = -((sign_bits >> 15) as i32);
            level = (level ^ sign) - sign;
            sign_bits = (sign_bits << 1) & 0xFFFF;

            coeffs[y_c * size + x_c] = level;
        }
    }
    Ok(info)
}

#[inline]
fn adapt_rice(
    cabac: &mut Cabac<'_>,
    params: &ResidualParams,
    bypass_ctx: bool,
    c_idx: usize,
    remaining: i32,
    rice_init: &mut bool,
) {
    if !params.persistent_rice_adaptation_enabled || *rice_init {
        return;
    }
    let sb_type = if bypass_ctx {
        2 * usize::from(c_idx == 0) + 1
    } else {
        2 * usize::from(c_idx == 0)
    };
    let init = (cabac.stat_coeff[sb_type] / 4) as i32;
    let stat = &mut cabac.stat_coeff[sb_type];
    if remaining >= (3 << init) {
        *stat = stat.wrapping_add(1);
    } else if 2 * remaining < (1 << init) && *stat > 0 {
        *stat -= 1;
    }
    *rice_init = true;
}

/// `coeff_abs_level_remaining_decode()` using plain bypass bins. The shared
/// crate's `bypass_bits` fast path is only an optimisation over this loop.
fn coeff_level_remaining(cabac: &mut Cabac<'_>, rice: usize) -> i32 {
    let mut prefix = 0i32;
    while prefix < 31 && cabac.get_cabac_bypass() != 0 {
        prefix += 1;
    }
    if prefix < 3 {
        let mut suffix = 0i32;
        for _ in 0..rice {
            suffix = (suffix << 1) | cabac.get_cabac_bypass() as i32;
        }
        (((prefix as usize) << rice) | (suffix as usize)) as i32
    } else {
        let pm3 = (prefix - 3) as usize;
        let k = pm3 + rice;
        let mut suffix = 0i32;
        for _ in 0..k {
            suffix = (suffix << 1) | cabac.get_cabac_bypass() as i32;
        }
        (((1usize << pm3) + 3 - 1) << rice) as i32 + suffix
    }
}

/// `coeff_sign_flag_decode()`: `nb` bypass bins packed MSB-first in bits 15..0.
fn decode_signs(cabac: &mut Cabac<'_>, nb: usize) -> u32 {
    let mut ret = 0u32;
    for _ in 0..nb {
        ret = (ret << 1) | cabac.get_cabac_bypass() as u32;
    }
    (ret << (16 - nb.min(16))) & 0xFFFF
}
