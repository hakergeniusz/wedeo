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

impl RefGeometry {
    /// Build a geometry from merged `wedeo-codec-hevc-ps` parameter sets.
    pub fn from_ps(sps: &Sps, pps: &Pps) -> Self {
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
            min_tb_addr_zs_tab: pps.tile.min_tb_addr_zs_tab.clone(),
            min_tb_addr_zs_offset: pps.tile.min_tb_addr_zs_offset,
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
