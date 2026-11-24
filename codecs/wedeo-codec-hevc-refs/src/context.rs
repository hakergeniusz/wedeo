//! Geometry / parameter bundle shared by the prediction algorithms.
//!
//! All fields correspond directly to `HEVCSPS` / `HEVCPPS` fields used by
//! FFmpeg's `mvs.c`. Callers building a synthetic context can construct this
//! struct directly, or use [`RefGeometry::from_ps`] when they already have
//! parsed parameter sets from `wedeo-codec-hevc-ps`.

use wedeo_codec_hevc_ps::{Pps, Sps};

/// FFmpeg's `enum PartMode` from `hevcdec.h`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PartMode {
    N2Nx2N = 0,
    N2xN = 1,
    Nx2N = 2,
    NxN = 3,
    N2xnU = 4,
    N2xnD = 5,
    NLx2N = 6,
    NRx2N = 7,
}

/// The subset of SPS/PPS geometry needed by reference-list and MV prediction.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RefGeometry {
    pub width: usize,
    pub height: usize,
    pub log2_ctb_size: u32,
    pub log2_min_pu_size: u32,
    pub log2_min_tb_size: u32,
    pub ctb_width: usize,
    pub tb_mask: u32,
    pub log2_parallel_merge_level: u32,
    /// `pps->ctb_addr_rs_to_ts`.
    pub ctb_addr_rs_to_ts: Vec<i32>,
    /// `pps->min_tb_addr_zs_tab` (flat `(tb_mask+2)^2` table).
    pub min_tb_addr_zs_tab: Vec<i32>,
    /// Offset of `min_tb_addr_zs[0][0]` inside `min_tb_addr_zs_tab`.
    pub min_tb_addr_zs_offset: usize,
}

/// FFmpeg's `ff_hevc_decode_nal_pps()` z-scan table construction
/// (`libavcodec/hevc/ps.c`).
///
/// The table is indexed through `pps->min_tb_addr_zs`, which points at
/// `&pps->min_tb_addr_zs_tab[1 * (sps->tb_mask+2) + 1]`; row 0 and column 0
/// therefore hold the -1 sentinel. Build it here from the CTB address map
/// rather than relying on the precomputed table: the latter is stored at the
/// wrong offset, which makes `MIN_TB_ADDR_ZS` read shifted values and breaks
/// `z_scan_block_avail()` for neighbours above the current CTB row.
fn min_tb_addr_zs_table(sps: &Sps, pps: &Pps) -> (Vec<i32>, usize) {
    let tb_mask = sps.tb_mask as usize;
    let side = tb_mask + 2;
    let mut tab = vec![0i32; side * side];
    for y in 0..side {
        tab[y * side] = -1;
        tab[y] = -1;
    }
    let log2_diff = sps.log2_ctb_size.saturating_sub(sps.log2_min_tb_size) as usize;
    let ctb_width = sps.ctb_width as usize;
    let addr = &pps.tile.ctb_addr_rs_to_ts;
    for y in 0..=tb_mask {
        for x in 0..=tb_mask {
            let tb_x = x >> log2_diff;
            let tb_y = y >> log2_diff;
            let Some(&rs) = addr.get(tb_y * ctb_width + tb_x) else {
                continue;
            };
            let mut val = rs << (log2_diff * 2);
            for i in 0..log2_diff {
                let m = 1usize << i;
                if m & x != 0 {
                    val += (m * m) as i32;
                }
                if m & y != 0 {
                    val += (2 * m * m) as i32;
                }
            }
            // `min_tb_addr_zs[y][x]` is at `tab[(y+1)*side + (x+1)]`.
            tab[(y + 1) * side + x + 1] = val;
        }
    }
    (tab, side + 1)
}

impl RefGeometry {
    /// Build a geometry from merged `wedeo-codec-hevc-ps` parameter sets.
    pub fn from_ps(sps: &Sps, pps: &Pps) -> Self {
        let (min_tb_addr_zs_tab, min_tb_addr_zs_offset) = min_tb_addr_zs_table(sps, pps);
        Self {
            width: sps.width as usize,
            height: sps.height as usize,
            log2_ctb_size: sps.log2_ctb_size,
            log2_min_pu_size: sps.log2_min_pu_size,
            log2_min_tb_size: sps.log2_min_tb_size,
            ctb_width: sps.ctb_width as usize,
            tb_mask: sps.tb_mask,
            log2_parallel_merge_level: pps.log2_parallel_merge_level,
            ctb_addr_rs_to_ts: pps.tile.ctb_addr_rs_to_ts.clone(),
            min_tb_addr_zs_tab,
            min_tb_addr_zs_offset,
        }
    }

    /// FFmpeg's `MIN_TB_ADDR_ZS(x, y)` macro.
    pub fn min_tb_addr_zs(&self, x: usize, y: usize) -> Option<i32> {
        let side = self.tb_mask as usize + 2;
        let idx = self
            .min_tb_addr_zs_offset
            .checked_add(y.checked_mul(side)?)?
            .checked_add(x)?;
        self.min_tb_addr_zs_tab.get(idx).copied()
    }
}
