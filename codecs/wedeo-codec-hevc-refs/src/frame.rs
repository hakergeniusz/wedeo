//! Reference frames and their per-prediction-unit motion-vector tables.
//!
//! FFmpeg reference: `hevcdec.h` (`HEVCFrame`, `HEVC_FRAME_FLAG_*`) and
//! `refs.c` (`ff_hevc_unref_frame`).

use crate::mv::MvField;
use crate::rpl::RefPicList;

/// FFmpeg's `HEVC_FRAME_FLAG_*` bits.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FrameFlags(pub u32);

impl FrameFlags {
    pub const OUTPUT: Self = Self(1 << 0);
    pub const SHORT_REF: Self = Self(1 << 1);
    pub const LONG_REF: Self = Self(1 << 2);
    pub const UNAVAILABLE: Self = Self(1 << 3);
    pub const CORRUPT: Self = Self(1 << 4);

    pub fn bits(self) -> u32 {
        self.0
    }

    pub fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    pub fn intersects(self, other: Self) -> bool {
        self.0 & other.0 != 0
    }

    pub fn insert(&mut self, other: Self) {
        self.0 |= other.0;
    }

    pub fn remove(&mut self, other: Self) {
        self.0 &= !other.0;
    }

    pub fn is_empty(self) -> bool {
        self.0 == 0
    }
}

/// A stable handle to a frame stored in the DPB.
///
/// FFmpeg stores a `HEVCFrame *` in `RefPicList.ref[]`. A DPB slot may be
/// recycled once a frame is fully unreferenced, so the handle also carries the
/// allocation id: readers can reject a stale handle after slot reuse.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FrameRef {
    pub slot: usize,
    pub id: u64,
    pub poc: i32,
}

impl FrameRef {
    pub fn new(slot: usize, id: u64, poc: i32) -> Self {
        Self { slot, id, poc }
    }
}

/// A 2-D table of per-prediction-unit `MvField`s, FFmpeg's `tab_mvf`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MvFieldTable {
    /// Number of prediction units per row (`sps->min_pu_width`).
    pub width_pu: usize,
    pub data: Vec<MvField>,
}

impl MvFieldTable {
    pub fn new(width_pu: usize, height_pu: usize) -> Self {
        Self {
            width_pu,
            data: vec![MvField::INTRA; width_pu * height_pu],
        }
    }

    pub fn get(&self, x_pu: usize, y_pu: usize) -> Option<&MvField> {
        if x_pu >= self.width_pu {
            return None;
        }
        self.data
            .get(y_pu.checked_mul(self.width_pu)?.checked_add(x_pu)?)
    }

    pub fn get_mut(&mut self, x_pu: usize, y_pu: usize) -> Option<&mut MvField> {
        if x_pu >= self.width_pu {
            return None;
        }
        self.data
            .get_mut(y_pu.checked_mul(self.width_pu)?.checked_add(x_pu)?)
    }
}

/// A decoded/reference picture, FFmpeg's `HEVCFrame`.
///
/// Payload buffers (the actual image samples) live in the caller's decoder;
/// this type carries everything the reference-list and MV-prediction
/// algorithms need: POC, reference flags, the collocated `tab_mvf` table and
/// the per-slice reference lists (`rpl` / `rpl_tab`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HevcFrame {
    /// Monotonically increasing allocation id, filled in by [`crate::Dpb`].
    pub id: u64,
    pub poc: i32,
    /// Combination of `HEVC_FRAME_FLAG_*`.
    pub flags: FrameFlags,
    pub needs_fg: bool,
    /// Per-PU motion vectors used as collocated data by later pictures.
    pub tab_mvf: MvFieldTable,
    /// Reference lists, one `[L0, L1]` pair per slice segment/NAL.
    pub rpl: Vec<[RefPicList; 2]>,
    /// `rpl_tab[ctb_addr_ts]` indexes into `rpl` (FFmpeg's `rpl_tab`).
    pub rpl_tab: Vec<usize>,
}

impl HevcFrame {
    /// FFmpeg's `ff_hevc_unref_frame()` without the refstruct attachments.
    ///
    /// `if (!(frame->flags & ~HEVC_FRAME_FLAG_CORRUPT)) frame->flags = 0;`
    pub fn unref(&mut self, flags: FrameFlags) {
        self.flags.remove(flags);
        if !self.flags.intersects(FrameFlags::CORRUPT.complement()) {
            self.flags = FrameFlags::default();
        }
        if self.flags.is_empty() {
            self.needs_fg = false;
            self.rpl.clear();
            self.rpl_tab.clear();
        }
    }

    /// Number of valid entries in `rpl` (FFmpeg's `nb_rpl_elems`).
    pub fn nb_rpl_elems(&self) -> usize {
        self.rpl.len()
    }

    /// FFmpeg's `ff_hevc_get_ref_list()`: select the `[L0, L1]` list pair for
    /// the CTB containing luma sample `(x0, y0)`.
    pub fn get_ref_list(
        &self,
        x0: i32,
        y0: i32,
        log2_ctb_size: u32,
        ctb_width: usize,
        ctb_addr_rs_to_ts: &[i32],
    ) -> Option<&[RefPicList; 2]> {
        let x_cb = (x0 >> log2_ctb_size) as usize;
        let y_cb = (y0 >> log2_ctb_size) as usize;
        let addr = y_cb.checked_mul(ctb_width)?.checked_add(x_cb)?;
        let ctb_addr_ts = usize::try_from(*ctb_addr_rs_to_ts.get(addr)?).ok()?;
        let elem = *self.rpl_tab.get(ctb_addr_ts)?;
        self.rpl.get(elem)
    }
}

impl FrameFlags {
    /// All bits except `other` (FFmpeg's `~flags` in `ff_hevc_unref_frame`).
    pub fn complement(self) -> Self {
        Self(!self.0)
    }
}

impl std::ops::BitOr for FrameFlags {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        Self(self.0 | rhs.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unref_clears_everything_except_persistent_corrupt_flag() {
        let mut f = HevcFrame {
            flags: FrameFlags::SHORT_REF,
            rpl: vec![[RefPicList::default(); 2]],
            rpl_tab: vec![0],
            ..Default::default()
        };
        f.unref(FrameFlags::SHORT_REF);
        assert!(f.flags.is_empty());
        assert!(f.rpl.is_empty());
        assert!(f.rpl_tab.is_empty());

        // FFmpeg clears even a lone CORRUPT flag: `flags & ~CORRUPT == 0`.
        f.flags = FrameFlags::CORRUPT;
        f.unref(FrameFlags::default());
        assert!(f.flags.is_empty());
    }

    #[test]
    fn mv_field_table_round_trip() {
        let mut t = MvFieldTable::new(4, 3);
        let field = MvField {
            mv: [MvField::INTRA.mv[0], MvField::INTRA.mv[1]],
            ref_idx: [1, 2],
            pred_flag: crate::mv::PredFlag::BI,
        };
        *t.get_mut(2, 1).unwrap() = field;
        assert_eq!(t.get(2, 1), Some(&field));
        assert_eq!(t.get(4, 1), None);
    }
}
