//! Deblocking and sample adaptive offset filtering.
//!
//! FFmpeg reference: `libavcodec/hevc/filter.c`
//! (`deblocking_filter_CTB`, `sao_filter_CTB`, `copy_CTB_to_hv`) and
//! `libavcodec/h26x/h2656_{deblock,sao}_template.c`.

use wedeo_codec_hevc_dsp as dsp;
use wedeo_codec_hevc_dsp::sao::{
    SaoBorders, SaoEdgeFlags, SaoType, sao_band_filter, sao_edge_filter, sao_edge_restore,
};
use wedeo_codec_hevc_ps::{Pps, Sps};
use wedeo_codec_hevc_refs::{Mv, MvField};
use wedeo_core::Result;

use crate::picture::Picture;

/// Apply deblocking to all CTBs, then SAO to all CTBs. FFmpeg's one-CTB delay
/// guarantees that deblocking of a CTB is complete before SAO of that CTB
/// starts, so processing the whole picture in two passes is equivalent.
pub fn filter_picture(
    pic: &mut Picture,
    sps: &Sps,
    pps: &Pps,
    disable_deblock: bool,
) -> Result<()> {
    let ctb_size = 1i32 << sps.log2_ctb_size;

    if !disable_deblock {
        for ry in 0..sps.ctb_height as i32 {
            for rx in 0..sps.ctb_width as i32 {
                let ctb = (ry * sps.ctb_width as i32 + rx) as usize;
                // FFmpeg's deblocking_filter_CTB() uses the *left* CTB's
                // offsets for the horizontal edges in the first eight columns
                // of a CTB (filter.c:528-534, 586-587 and 651).
                let (left_beta, left_tc) = if rx > 0 {
                    (pic.deblock_beta[ctb - 1], pic.deblock_tc[ctb - 1])
                } else {
                    (0, 0)
                };
                deblock_ctb(
                    pic,
                    sps,
                    pps,
                    rx * ctb_size,
                    ry * ctb_size,
                    pic.deblock_beta[ctb],
                    pic.deblock_tc[ctb],
                    left_beta,
                    left_tc,
                )?;
            }
        }
    }

    if sps.sao_enabled {
        let mut sao_h = vec![0u16; sps.ctb_height as usize * 2 * sps.width as usize];
        let mut sao_v = vec![0u16; sps.ctb_width as usize * 2 * sps.height as usize];
        for ry in 0..sps.ctb_height as i32 {
            for rx in 0..sps.ctb_width as i32 {
                sao_ctb(
                    pic,
                    sps,
                    pps,
                    &mut sao_h,
                    &mut sao_v,
                    rx * ctb_size,
                    ry * ctb_size,
                )?;
            }
        }
    }
    Ok(())
}

/// Deblock one CTB (FFmpeg's `deblocking_filter_CTB`).
#[allow(clippy::too_many_arguments)] // mirrors FFmpeg's deblocking_filter_CTB inputs
fn deblock_ctb(
    pic: &mut Picture,
    sps: &Sps,
    pps: &Pps,
    x0: i32,
    y0: i32,
    beta_offset: i32,
    tc_offset: i32,
    left_beta_offset: i32,
    left_tc_offset: i32,
) -> Result<()> {
    let ctb_size = 1i32 << sps.log2_ctb_size;
    let width = sps.width as i32;
    let height = sps.height as i32;
    let x_end = (x0 + ctb_size).min(width);
    let y_end = (y0 + ctb_size).min(height);
    let mut x_end2;
    let bit_depth = sps.bit_depth as u8;

    for y in (y0..y_end).step_by(8) {
        // Vertical luma boundaries.
        let mut x = if x0 == 0 { 8 } else { x0 };
        while x < x_end {
            let bs0 = pic.bs_v_at(x as usize, y as usize);
            let bs1 = pic.bs_v_at(x as usize, (y + 4) as usize);
            if bs0 != 0 || bs1 != 0 {
                let qp = (qp_at(pic, sps, x - 1, y) + qp_at(pic, sps, x, y) + 1) >> 1;
                let beta = dsp::deblock::beta_from_qp(qp, beta_offset);
                let tc = [
                    if bs0 != 0 {
                        dsp::deblock::tc_from_qp(qp, bs0 as i32, tc_offset)
                    } else {
                        0
                    },
                    if bs1 != 0 {
                        dsp::deblock::tc_from_qp(qp, bs1 as i32, tc_offset)
                    } else {
                        0
                    },
                ];
                let base = pic.index(0, x as usize, y as usize);
                dsp::deblock::loop_filter_luma(
                    &mut pic.planes[0],
                    base,
                    1,
                    pic.stride[0] as isize,
                    beta,
                    &tc,
                    &[false, false],
                    &[false, false],
                    bit_depth,
                )
                .map_err(|e| wedeo_core::Error::Other(e.to_string()))?;
            }
            x += 8;
        }
        if y == 0 {
            continue;
        }
        x_end2 = x_end;
        if x_end2 != width {
            x_end2 -= 8;
        }
        // Horizontal luma boundaries.
        let mut x = if x0 == 0 { 0 } else { x0 - 8 };
        while x < x_end2 {
            let bs0 = pic.bs_h_at(x as usize, y as usize);
            let bs1 = pic.bs_h_at((x + 4) as usize, y as usize);
            if bs0 != 0 || bs1 != 0 {
                let qp = (qp_at(pic, sps, x, y - 1) + qp_at(pic, sps, x, y) + 1) >> 1;
                let (beta_offset, tc_offset) = if x >= x0 {
                    (beta_offset, tc_offset)
                } else {
                    (left_beta_offset, left_tc_offset)
                };
                let beta = dsp::deblock::beta_from_qp(qp, beta_offset);
                let tc = [
                    if bs0 != 0 {
                        dsp::deblock::tc_from_qp(qp, bs0 as i32, tc_offset)
                    } else {
                        0
                    },
                    if bs1 != 0 {
                        dsp::deblock::tc_from_qp(qp, bs1 as i32, tc_offset)
                    } else {
                        0
                    },
                ];
                let base = pic.index(0, x as usize, y as usize);
                dsp::deblock::loop_filter_luma(
                    &mut pic.planes[0],
                    base,
                    pic.stride[0] as isize,
                    1,
                    beta,
                    &tc,
                    &[false, false],
                    &[false, false],
                    bit_depth,
                )
                .map_err(|e| wedeo_core::Error::Other(e.to_string()))?;
            }
            x += 8;
        }
    }

    if sps.chroma_format_idc != 0 {
        for chroma in 1..=2 {
            let h = 1i32 << sps.hshift[chroma];
            let v = 1i32 << sps.vshift[chroma];
            let c = chroma;
            let bd = sps.bit_depth_chroma as u8;
            let mut y = y0;
            while y < y_end {
                let mut x = if x0 == 0 { 8 * h } else { x0 };
                while x < x_end {
                    let bs0 = pic.bs_v_at(x as usize, y as usize);
                    let bs1 = pic.bs_v_at(x as usize, (y + 4 * v) as usize);
                    if bs0 == 2 || bs1 == 2 {
                        let qp0 = (qp_at(pic, sps, x - 1, y) + qp_at(pic, sps, x, y) + 1) >> 1;
                        let qp1 =
                            (qp_at(pic, sps, x - 1, y + 4 * v) + qp_at(pic, sps, x, y + 4 * v) + 1)
                                >> 1;
                        let c_tc = [
                            if bs0 == 2 {
                                dsp::deblock::chroma_tc(
                                    qp0,
                                    c,
                                    pps.cb_qp_offset,
                                    pps.cr_qp_offset,
                                    tc_offset,
                                    sps.chroma_format_idc as u8,
                                )
                            } else {
                                0
                            },
                            if bs1 == 2 {
                                dsp::deblock::chroma_tc(
                                    qp1,
                                    c,
                                    pps.cb_qp_offset,
                                    pps.cr_qp_offset,
                                    tc_offset,
                                    sps.chroma_format_idc as u8,
                                )
                            } else {
                                0
                            },
                        ];
                        let base = pic.index(
                            c,
                            (x >> sps.hshift[c]) as usize,
                            (y >> sps.vshift[c]) as usize,
                        );
                        dsp::deblock::loop_filter_chroma(
                            &mut pic.planes[c],
                            base,
                            1,
                            pic.stride[c] as isize,
                            &c_tc,
                            &[false, false],
                            &[false, false],
                            bd,
                        )
                        .map_err(|e| wedeo_core::Error::Other(e.to_string()))?;
                    }
                    x += 8 * h;
                }
                if y == 0 {
                    y += 8 * v;
                    continue;
                }
                let mut x_end_c = x_end;
                if x_end_c != width {
                    x_end_c -= 8 * h;
                }
                let mut x = if x0 == 0 { 0 } else { x0 - 8 * h };
                while x < x_end_c {
                    let bs0 = pic.bs_h_at(x as usize, y as usize);
                    let bs1 = pic.bs_h_at((x + 4 * h) as usize, y as usize);
                    if bs0 == 2 || bs1 == 2 {
                        let off = if x >= x0 { tc_offset } else { left_tc_offset };
                        let qp0 = if bs0 == 2 {
                            (qp_at(pic, sps, x, y - 1) + qp_at(pic, sps, x, y) + 1) >> 1
                        } else {
                            0
                        };
                        let qp1 = if bs1 == 2 {
                            (qp_at(pic, sps, x + 4 * h, y - 1) + qp_at(pic, sps, x + 4 * h, y) + 1)
                                >> 1
                        } else {
                            0
                        };
                        let c_tc = [
                            if bs0 == 2 {
                                dsp::deblock::chroma_tc(
                                    qp0,
                                    c,
                                    pps.cb_qp_offset,
                                    pps.cr_qp_offset,
                                    off,
                                    sps.chroma_format_idc as u8,
                                )
                            } else {
                                0
                            },
                            if bs1 == 2 {
                                dsp::deblock::chroma_tc(
                                    qp1,
                                    c,
                                    pps.cb_qp_offset,
                                    pps.cr_qp_offset,
                                    off,
                                    sps.chroma_format_idc as u8,
                                )
                            } else {
                                0
                            },
                        ];
                        let base = pic.index(
                            c,
                            (x >> sps.hshift[c]) as usize,
                            (y >> sps.vshift[c]) as usize,
                        );
                        dsp::deblock::loop_filter_chroma(
                            &mut pic.planes[c],
                            base,
                            pic.stride[c] as isize,
                            1,
                            &c_tc,
                            &[false, false],
                            &[false, false],
                            bd,
                        )
                        .map_err(|e| wedeo_core::Error::Other(e.to_string()))?;
                    }
                    x += 8 * h;
                }
                y += 8 * v;
            }
        }
    }
    Ok(())
}

fn qp_at(pic: &Picture, sps: &Sps, x: i32, y: i32) -> i32 {
    let x_cb = (x as u32 >> sps.log2_min_cb_size) as usize;
    let y_cb = (y as u32 >> sps.log2_min_cb_size) as usize;
    pic.qp_y_tab
        .get(y_cb * pic.min_cb_width + x_cb)
        .copied()
        .unwrap_or(26)
}

/// SAO-filter one CTB (FFmpeg's `sao_filter_CTB`).
#[allow(clippy::too_many_arguments)] // mirrors FFmpeg's sao_filter_CTB inputs
fn sao_ctb(
    pic: &mut Picture,
    sps: &Sps,
    pps: &Pps,
    sao_h: &mut [u16],
    sao_v: &mut [u16],
    x: i32,
    y: i32,
) -> Result<()> {
    let ctb_size = 1i32 << sps.log2_ctb_size;
    let x_ctb = (x >> sps.log2_ctb_size) as usize;
    let y_ctb = (y >> sps.log2_ctb_size) as usize;
    let ctb_addr_rs = y_ctb * sps.ctb_width as usize + x_ctb;
    let mut params = pic.sao[ctb_addr_rs].clone();

    let edges = [
        x_ctb == 0,
        y_ctb == 0,
        x_ctb + 1 == sps.ctb_width as usize,
        y_ctb + 1 == sps.ctb_height as usize,
    ];

    // FFmpeg's sao_filter_CTB() (filter.c:286-329): unfilterable edges at
    // slice/tile boundaries, used by sao_edge_restore to copy the original
    // samples back instead of applying an EO offset across the boundary.
    let lfase = pic.filter_slice_edges[ctb_addr_rs];
    let no_tile_filter = pps.tiles_enabled_flag && !pps.loop_filter_across_tiles_enabled_flag;
    let restore = no_tile_filter || !lfase;
    let slice_addr = pic.tab_slice_address[ctb_addr_rs];
    // FFmpeg indexes tile_id by CTB *tile-scan* address.
    let tile_addr = |ctb: usize| {
        pps.tile
            .ctb_addr_rs_to_ts
            .get(ctb)
            .and_then(|&ts| pps.tile.tile_id.get(usize::try_from(ts).unwrap_or(0)))
            .copied()
            .unwrap_or(0)
    };
    let mut vert_edge = [false; 2];
    let mut horiz_edge = [false; 2];
    let mut diag_edge = [false; 4];
    if restore {
        let mut tile_edge = [false; 4]; // left, right, up, bottom
        if !edges[0] {
            tile_edge[0] = no_tile_filter && tile_addr(ctb_addr_rs - 1) != tile_addr(ctb_addr_rs);
            vert_edge[0] =
                (!lfase && slice_addr != pic.tab_slice_address[ctb_addr_rs - 1]) || tile_edge[0];
        }
        if !edges[2] {
            tile_edge[1] = no_tile_filter && tile_addr(ctb_addr_rs + 1) != tile_addr(ctb_addr_rs);
            vert_edge[1] =
                (!lfase && slice_addr != pic.tab_slice_address[ctb_addr_rs + 1]) || tile_edge[1];
        }
        if !edges[1] {
            let up = ctb_addr_rs - sps.ctb_width as usize;
            tile_edge[2] = no_tile_filter && tile_addr(up) != tile_addr(ctb_addr_rs);
            horiz_edge[0] = (!lfase && slice_addr != pic.tab_slice_address[up]) || tile_edge[2];
        }
        if !edges[3] {
            let down = ctb_addr_rs + sps.ctb_width as usize;
            tile_edge[3] = no_tile_filter && tile_addr(down) != tile_addr(ctb_addr_rs);
            horiz_edge[1] = (!lfase && slice_addr != pic.tab_slice_address[down]) || tile_edge[3];
        }
        if !edges[0] && !edges[1] {
            diag_edge[0] = (!lfase
                && slice_addr != pic.tab_slice_address[ctb_addr_rs - sps.ctb_width as usize - 1])
                || tile_edge[0]
                || tile_edge[2];
        }
        if !edges[1] && !edges[2] {
            diag_edge[1] = (!lfase
                && slice_addr != pic.tab_slice_address[ctb_addr_rs - sps.ctb_width as usize + 1])
                || tile_edge[1]
                || tile_edge[2];
        }
        if !edges[2] && !edges[3] {
            diag_edge[2] = (!lfase
                && slice_addr != pic.tab_slice_address[ctb_addr_rs + sps.ctb_width as usize + 1])
                || tile_edge[1]
                || tile_edge[3];
        }
        if !edges[0] && !edges[3] {
            diag_edge[3] = (!lfase
                && slice_addr != pic.tab_slice_address[ctb_addr_rs + sps.ctb_width as usize - 1])
                || tile_edge[0]
                || tile_edge[3];
        }
    }

    for c_idx in 0..if sps.chroma_format_idc != 0 { 3 } else { 1 } {
        let sao_type = params.type_idx[c_idx];
        if sao_type == SaoType::NotApplied || sao_type == SaoType::Applied {
            continue;
        }
        let hshift = if c_idx == 0 { 0 } else { sps.hshift[1] };
        let vshift = if c_idx == 0 { 0 } else { sps.vshift[1] };
        let x0 = (x >> hshift) as usize;
        let y0 = (y >> vshift) as usize;
        let pw = pic.plane_size[c_idx].0;
        let ph = pic.plane_size[c_idx].1;
        let width = ((ctb_size >> hshift) as usize).min(pw.saturating_sub(x0));
        let height = ((ctb_size >> vshift) as usize).min(ph.saturating_sub(y0));
        if width == 0 || height == 0 {
            continue;
        }
        let bit_depth = if c_idx == 0 {
            sps.bit_depth
        } else {
            sps.bit_depth_chroma
        } as u8;

        match sao_type {
            SaoType::Band => {
                let pic_ref = &*pic;
                let original: Vec<u16> = (0..height)
                    .flat_map(|yy| {
                        (0..width).map(move |xx| pic_ref.sample(c_idx, x0 + xx, y0 + yy))
                    })
                    .collect();
                let mut filtered = vec![0u16; width * height];
                sao_band_filter(
                    &mut filtered,
                    width,
                    &original,
                    width,
                    &params.offset_val[c_idx],
                    params.band_position[c_idx],
                    width,
                    height,
                    bit_depth,
                )
                .map_err(|e| wedeo_core::Error::Other(e.to_string()))?;
                // FFmpeg's copy_CTB_to_hv() (filter.c:346) saves the pre-SAO
                // samples for neighbouring CTBs' EO edge padding.
                store_hv(
                    pic, c_idx, sao_h, sao_v, x_ctb, y_ctb, x0, y0, width, height, pw, ph,
                );
                for yy in 0..height {
                    for xx in 0..width {
                        pic.set_sample(c_idx, x0 + xx, y0 + yy, filtered[yy * width + xx]);
                    }
                }
                params.type_idx[c_idx] = SaoType::Applied;
            }
            SaoType::Edge => {
                // Build the padded neighbour block (1 sample on every side).
                let dstride = width + 3;
                let mut padded = vec![0u16; dstride * (height + 3)];
                let applied = |ctbx: usize, ctby: usize| -> bool {
                    ctbx < sps.ctb_width as usize
                        && ctby < sps.ctb_height as usize
                        && pic.sao[ctby * sps.ctb_width as usize + ctbx].type_idx[c_idx]
                            == SaoType::Applied
                };
                for yy in 0..height + 3 {
                    for xx in 0..width + 3 {
                        let px = x0 + xx.saturating_sub(1);
                        let py = y0 + yy.saturating_sub(1);
                        let cx = px.min(pw.saturating_sub(1));
                        let cy = py.min(ph.saturating_sub(1));
                        let is_top = yy == 0;
                        let is_bottom = yy == height + 2;
                        let is_left = xx == 0;
                        let is_right = xx == width + 2;
                        let value = if is_top && y_ctb > 0 && applied(x_ctb, y_ctb - 1) {
                            let v = sao_h.get((2 * y_ctb - 1) * pw + cx).copied();
                            v.unwrap_or_else(|| pic.sample(c_idx, cx, cy))
                        } else if is_bottom
                            && y_ctb + 1 < sps.ctb_height as usize
                            && applied(x_ctb, y_ctb + 1)
                        {
                            let v = sao_h.get((2 * (y_ctb + 1)) * pw + cx).copied();
                            v.unwrap_or_else(|| pic.sample(c_idx, cx, cy))
                        } else if is_left && x_ctb > 0 && applied(x_ctb - 1, y_ctb) {
                            let v = sao_v.get((2 * x_ctb - 1) * ph + cy).copied();
                            v.unwrap_or_else(|| pic.sample(c_idx, cx, cy))
                        } else if is_right
                            && x_ctb + 1 < sps.ctb_width as usize
                            && applied(x_ctb + 1, y_ctb)
                        {
                            let v = sao_v.get((2 * (x_ctb + 1)) * ph + cy).copied();
                            v.unwrap_or_else(|| pic.sample(c_idx, cx, cy))
                        } else {
                            pic.sample(c_idx, cx, cy)
                        };
                        padded[yy * dstride + xx] = value;
                    }
                }

                let mut filtered = vec![0u16; pic.stride[c_idx] * (ph + 2)];
                sao_edge_filter(
                    &mut filtered,
                    pic.stride[c_idx],
                    &padded,
                    dstride,
                    &params.offset_val[c_idx],
                    params.eo_class[c_idx],
                    width,
                    height,
                    bit_depth,
                )
                .map_err(|e| wedeo_core::Error::Other(e.to_string()))?;
                let restore = SaoEdgeFlags {
                    vert: vert_edge,
                    horiz: horiz_edge,
                    diag: diag_edge,
                };
                // `sao_edge_restore` writes to its first argument (the filtered
                // picture) and reads unfiltered samples from its second.
                let src_offset = dstride + 1;
                sao_edge_restore(
                    &mut filtered,
                    pic.stride[c_idx],
                    &padded[src_offset..],
                    dstride,
                    &params,
                    SaoBorders(edges),
                    width,
                    height,
                    c_idx,
                    restore,
                    bit_depth,
                )
                .map_err(|e| wedeo_core::Error::Other(e.to_string()))?;
                // FFmpeg's copy_CTB_to_hv() (filter.c:459) saves the pre-SAO
                // samples for neighbouring CTBs' EO edge padding.
                store_hv(
                    pic, c_idx, sao_h, sao_v, x_ctb, y_ctb, x0, y0, width, height, pw, ph,
                );
                for yy in 0..height {
                    for xx in 0..width {
                        pic.set_sample(
                            c_idx,
                            x0 + xx,
                            y0 + yy,
                            filtered[yy * pic.stride[c_idx] + xx],
                        );
                    }
                }
                params.type_idx[c_idx] = SaoType::Applied;
            }
            SaoType::Applied | SaoType::NotApplied => unreachable!(),
        }
    }
    pic.sao[ctb_addr_rs] = params;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn store_hv(
    pic: &Picture,
    c: usize,
    sao_h: &mut [u16],
    sao_v: &mut [u16],
    x_ctb: usize,
    y_ctb: usize,
    x0: usize,
    y0: usize,
    width: usize,
    height: usize,
    pw: usize,
    ph: usize,
) {
    for i in 0..width {
        if let Some(s) = sao_h.get_mut((2 * y_ctb) * pw + x0 + i) {
            *s = pic.sample(c, x0 + i, y0);
        }
        if let Some(s) = sao_h.get_mut((2 * y_ctb + 1) * pw + x0 + i) {
            *s = pic.sample(c, x0 + i, y0 + height - 1);
        }
    }
    for i in 0..height {
        if let Some(s) = sao_v.get_mut((2 * x_ctb) * ph + y0 + i) {
            *s = pic.sample(c, x0, y0 + i);
        }
        if let Some(s) = sao_v.get_mut((2 * x_ctb + 1) * ph + y0 + i) {
            *s = pic.sample(c, x0 + width - 1, y0 + i);
        }
    }
}

/// Boundary strength derivation for one TU (FFmpeg's
/// `ff_hevc_deblocking_boundary_strengths`).
pub fn boundary_strengths(
    pic: &mut Picture,
    sps: &Sps,
    x0: i32,
    y0: i32,
    log2_trafo_size: u32,
    same_slice: bool,
) {
    let size = 1i32 << log2_trafo_size;
    if y0 > 0 && (y0 & 7) == 0 {
        for i in (0..size).step_by(4) {
            let bs = edge_bs(pic, sps, x0 + i, y0 - 1, x0 + i, y0, same_slice);
            if let Some(slot) = pic.bs_h_mut((x0 + i) as usize, y0 as usize) {
                *slot = bs;
            }
        }
    }
    if x0 > 0 && (x0 & 7) == 0 {
        for i in (0..size).step_by(4) {
            let bs = edge_bs(pic, sps, x0 - 1, y0 + i, x0, y0 + i, same_slice);
            if let Some(slot) = pic.bs_v_mut(x0 as usize, (y0 + i) as usize) {
                *slot = bs;
            }
        }
    }
    if log2_trafo_size > sps.log2_min_pu_size {
        let is_intra = is_intra_at(pic, sps, x0, y0);
        if !is_intra {
            for j in (8..size).step_by(8) {
                for i in (0..size).step_by(4) {
                    let bs = internal_edge_bs(pic, sps, x0 + i, y0 + j - 1, x0 + i, y0 + j);
                    if let Some(slot) = pic.bs_h_mut((x0 + i) as usize, (y0 + j) as usize) {
                        *slot = bs;
                    }
                }
            }
            for j in (0..size).step_by(4) {
                for i in (8..size).step_by(8) {
                    let bs = internal_edge_bs(pic, sps, x0 + i - 1, y0 + j, x0 + i, y0 + j);
                    if let Some(slot) = pic.bs_v_mut((x0 + i) as usize, (y0 + j) as usize) {
                        *slot = bs;
                    }
                }
            }
        }
    }
}

fn is_intra_at(pic: &Picture, sps: &Sps, x: i32, y: i32) -> bool {
    let pu = |v: i32| (v as u32 >> sps.log2_min_pu_size) as usize;
    pic.tab_mvf
        .get(pu(x), pu(y))
        .is_some_and(|f| f.pred_flag.is_intra())
}

fn cbf_at(pic: &Picture, sps: &Sps, x: i32, y: i32) -> u8 {
    let tx = (x as u32 >> sps.log2_min_tb_size) as usize;
    let ty = (y as u32 >> sps.log2_min_tb_size) as usize;
    pic.cbf_luma
        .get(ty * pic.min_tb_width + tx)
        .copied()
        .unwrap_or(0)
}

fn edge_bs(pic: &Picture, sps: &Sps, xp: i32, yp: i32, xq: i32, yq: i32, same_slice: bool) -> u8 {
    let pu = |v: i32| (v as u32 >> sps.log2_min_pu_size) as usize;
    let (Some(p), Some(q)) = (
        pic.tab_mvf.get(pu(xp), pu(yp)),
        pic.tab_mvf.get(pu(xq), pu(yq)),
    ) else {
        return 2;
    };
    if p.pred_flag.is_intra() || q.pred_flag.is_intra() {
        return 2;
    }
    if cbf_at(pic, sps, xp, yp) != 0 || cbf_at(pic, sps, xq, yq) != 0 {
        return 1;
    }
    if same_slice && mvf_equal(*p, *q) {
        return 0;
    }
    boundary_strength(*p, *q)
}

/// Boundary strength for TU internal boundaries of inter TUs.
///
/// FFmpeg's `ff_hevc_deblocking_boundary_strengths()` (filter.c 858-902) only
/// compares MvFields here; it does not consult `cbf_luma`, which is used only
/// for the TU's own top/left edges. Treating a coded residual as a boundary
/// would filter every 8x8 edge inside an inter TU.
fn internal_edge_bs(pic: &Picture, sps: &Sps, xp: i32, yp: i32, xq: i32, yq: i32) -> u8 {
    let pu = |v: i32| (v as u32 >> sps.log2_min_pu_size) as usize;
    let (Some(p), Some(q)) = (
        pic.tab_mvf.get(pu(xp), pu(yp)),
        pic.tab_mvf.get(pu(xq), pu(yq)),
    ) else {
        return 2;
    };
    if mvf_equal(*p, *q) {
        0
    } else {
        boundary_strength(*p, *q)
    }
}

fn mvf_equal(a: MvField, b: MvField) -> bool {
    a.pred_flag == b.pred_flag
        && a.mv[0] == b.mv[0]
        && a.mv[1] == b.mv[1]
        && a.ref_idx[0] == b.ref_idx[0]
        && a.ref_idx[1] == b.ref_idx[1]
}

/// FFmpeg's `boundary_strength()` (filter.c 698-761), simplified: POC-based
/// reference comparison is approximated by reference-index comparison.
fn boundary_strength(curr: MvField, neigh: MvField) -> u8 {
    let mv_diff = |a: Mv, b: Mv| (a.x - b.x).abs() >= 4 || (a.y - b.y).abs() >= 4;
    if curr.pred_flag == wedeo_codec_hevc_refs::PredFlag::BI
        && neigh.pred_flag == wedeo_codec_hevc_refs::PredFlag::BI
    {
        if curr.ref_idx == neigh.ref_idx {
            if mv_diff(curr.mv[0], neigh.mv[0]) || mv_diff(curr.mv[1], neigh.mv[1]) {
                return 1;
            }
            return 0;
        }
        return 1;
    }
    if curr.pred_flag != wedeo_codec_hevc_refs::PredFlag::BI
        && neigh.pred_flag != wedeo_codec_hevc_refs::PredFlag::BI
    {
        let i = usize::from(curr.pred_flag == wedeo_codec_hevc_refs::PredFlag::L1);
        let j = usize::from(neigh.pred_flag == wedeo_codec_hevc_refs::PredFlag::L1);
        if curr.ref_idx[i] == neigh.ref_idx[j] {
            if mv_diff(curr.mv[i], neigh.mv[j]) {
                return 1;
            }
            return 0;
        }
        return 1;
    }
    1
}
