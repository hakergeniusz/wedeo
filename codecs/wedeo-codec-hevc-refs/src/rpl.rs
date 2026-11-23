//! Reference picture sets and L0/L1 reference-list construction.
//!
//! FFmpeg reference: `hevcdec.h` (`enum RPSType`, `RefPicList`),
//! `refs.c` (`ff_hevc_slice_rpl`, `init_slice_rpl`, `ff_hevc_frame_nb_refs`).

use wedeo_codec_hevc_ps::{L0, MAX_REFS, SliceHeader, SliceType};
use wedeo_core::{Error, Result};

use crate::frame::{FrameRef, HevcFrame};

/// FFmpeg's `enum RPSType` (order matters: it indexes `HEVCContext.rps[]`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
pub enum RpsType {
    StCurrBef = 0,
    StCurrAft,
    StFoll,
    LtCurr,
    LtFoll,
    InterLayer0,
    InterLayer1,
}

pub const NB_RPS_TYPE: usize = 7;

/// FFmpeg's `RefPicList`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RefPicList {
    /// POC of each reference (`list[]` in C).
    pub list: [i32; MAX_REFS],
    /// Stable handle to the referenced frame (`ref[]` in C).
    pub r#ref: [Option<FrameRef>; MAX_REFS],
    /// Whether each entry is long-term (`isLongTerm[]` in C).
    pub is_long_term: [bool; MAX_REFS],
    pub nb_refs: usize,
}

/// FFmpeg's `ff_hevc_frame_nb_refs()`.
///
/// Counts the distinct pictures actually used by the current frame's RPS,
/// matching `refs.c` rather than the slice-header parser's approximation.
pub fn frame_nb_refs(
    sh: &SliceHeader,
    pps_curr_pic_ref_enabled_flag: bool,
    inter_layer_pred: bool,
) -> usize {
    let mut ret = 0usize;
    if let Some(rps) = &sh.short_term_rps {
        for i in 0..rps.num_delta_pocs as usize {
            if rps.used & (1 << i) != 0 {
                ret += 1;
            }
        }
    }
    for i in 0..sh.long_term_rps.nb_refs.min(sh.long_term_rps.used.len()) {
        if sh.long_term_rps.used[i] {
            ret += 1;
        }
    }
    if inter_layer_pred {
        ret += 1;
    }
    if pps_curr_pic_ref_enabled_flag {
        ret += 1;
    }
    ret
}

/// Parameters needed by [`slice_rpl`] beyond the slice header itself.
pub struct SliceRplParams<'a> {
    pub sh: &'a SliceHeader,
    pub pps_curr_pic_ref_enabled_flag: bool,
    pub log2_ctb_size: u32,
    pub ctb_width: usize,
    pub ctb_count: usize,
    pub ctb_addr_rs_to_ts: &'a [i32],
    pub slice_segment_addr: u32,
    /// Index of this NAL/slice segment in `cur_frame.rpl`.
    pub slice_idx: usize,
}

/// FFmpeg's `init_slice_rpl()`: map every CTB of this slice to its RPL slot.
pub fn init_slice_rpl(frame: &mut HevcFrame, params: &SliceRplParams<'_>) -> Result<usize> {
    if params.slice_idx >= frame.nb_rpl_elems() {
        return Err(Error::InvalidData);
    }
    let ctb_addr_ts = params
        .ctb_addr_rs_to_ts
        .get(params.slice_segment_addr as usize)
        .copied()
        .ok_or(Error::InvalidData)?;
    if ctb_addr_ts < 0 {
        return Err(Error::InvalidData);
    }
    let start = ctb_addr_ts as usize;
    if start > params.ctb_count {
        return Err(Error::InvalidData);
    }
    for elem in frame.rpl_tab[start..params.ctb_count].iter_mut() {
        *elem = params.slice_idx;
    }
    Ok(start)
}

/// FFmpeg's `ff_hevc_slice_rpl()`.
///
/// Builds `cur_frame.rpl[slice_idx][L0]` (and `[L1]` for B slices) from the
/// seven candidate RPS lists, applies `rpl_modification_flag` /
/// `list_entry_lx`, and returns the collocated frame handle
/// (`s->collocated_ref`).
pub fn slice_rpl(
    cur_frame: &mut HevcFrame,
    rps: &[RefPicList; NB_RPS_TYPE],
    params: &SliceRplParams<'_>,
) -> Result<Option<FrameRef>> {
    let sh = params.sh;
    let nb_list = if sh.slice_type == SliceType::B { 2 } else { 1 };

    init_slice_rpl(cur_frame, params)?;

    let nb_rps_refs: usize = rps.iter().map(|r| r.nb_refs).sum();
    if nb_rps_refs == 0 && !params.pps_curr_pic_ref_enabled_flag {
        return Err(Error::InvalidData);
    }

    let cur_poc = cur_frame.poc;
    let cur_ref = FrameRef::new(
        // The current frame's own handle is supplied by the caller through
        // `cur_frame.id`; slot is not needed for identity here.
        usize::MAX,
        cur_frame.id,
        cur_poc,
    );

    let mut collocated = None;

    for list_idx in 0..nb_list {
        let mut rpl_tmp = RefPicList::default();

        // The order of the elements is
        // ST_CURR_BEF - INTER_LAYER0 - ST_CURR_AFT - LT_CURR - INTER_LAYER1 for the L0 and
        // ST_CURR_AFT - INTER_LAYER1 - ST_CURR_BEF - LT_CURR - INTER_LAYER0 for the L1.
        let cand_lists: [RpsType; 5] = if list_idx == L0 {
            [
                RpsType::StCurrBef,
                RpsType::InterLayer0,
                RpsType::StCurrAft,
                RpsType::LtCurr,
                RpsType::InterLayer1,
            ]
        } else {
            [
                RpsType::StCurrAft,
                RpsType::InterLayer1,
                RpsType::StCurrBef,
                RpsType::LtCurr,
                RpsType::InterLayer0,
            ]
        };

        // Concatenate the candidate lists for the current frame.
        while rpl_tmp.nb_refs < sh.nb_refs[list_idx] as usize {
            let before = rpl_tmp.nb_refs;
            for &rps_type in &cand_lists {
                let src = &rps[rps_type as usize];
                for j in 0..src.nb_refs.min(MAX_REFS) {
                    if rpl_tmp.nb_refs >= MAX_REFS {
                        break;
                    }
                    let dst = rpl_tmp.nb_refs;
                    rpl_tmp.list[dst] = src.list[j];
                    rpl_tmp.r#ref[dst] = src.r#ref[j];
                    // multiview inter-layer refs are treated as long-term here,
                    // cf. G.8.1.3
                    rpl_tmp.is_long_term[dst] = matches!(
                        rps_type,
                        RpsType::LtCurr | RpsType::InterLayer0 | RpsType::InterLayer1
                    );
                    rpl_tmp.nb_refs += 1;
                }
            }
            // Construct RefPicList0, RefPicList1 (8-8, 8-10)
            if params.pps_curr_pic_ref_enabled_flag && rpl_tmp.nb_refs < MAX_REFS {
                let dst = rpl_tmp.nb_refs;
                rpl_tmp.list[dst] = cur_poc;
                rpl_tmp.r#ref[dst] = Some(cur_ref);
                rpl_tmp.is_long_term[dst] = true;
                rpl_tmp.nb_refs += 1;
            }
            // FFmpeg's loop assumes at least one candidate; guard against a
            // malformed synthetic RPS that would spin forever.
            if rpl_tmp.nb_refs == before {
                return Err(Error::InvalidData);
            }
        }

        let rpl = &mut cur_frame.rpl[params.slice_idx][list_idx];

        // Reorder the references if necessary.
        if sh.rpl_modification_flag[list_idx] {
            for i in 0..sh.nb_refs[list_idx] as usize {
                let idx = sh.list_entry_lx[list_idx][i] as usize;
                if idx >= rpl_tmp.nb_refs {
                    return Err(Error::InvalidData);
                }
                rpl.list[i] = rpl_tmp.list[idx];
                rpl.r#ref[i] = rpl_tmp.r#ref[idx];
                rpl.is_long_term[i] = rpl_tmp.is_long_term[idx];
                rpl.nb_refs += 1;
            }
        } else {
            *rpl = rpl_tmp;
            rpl.nb_refs = rpl.nb_refs.min(sh.nb_refs[list_idx] as usize);
        }

        // 8-9
        if params.pps_curr_pic_ref_enabled_flag
            && !sh.rpl_modification_flag[list_idx]
            && rpl_tmp.nb_refs > sh.nb_refs[L0] as usize
        {
            let idx = sh.nb_refs[L0] as usize - 1;
            rpl.list[idx] = cur_poc;
            rpl.r#ref[idx] = Some(cur_ref);
        }

        if sh.collocated_list == list_idx && (sh.collocated_ref_idx as usize) < rpl.nb_refs {
            collocated = rpl.r#ref[sh.collocated_ref_idx as usize];
        }
    }

    Ok(collocated)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_sh() -> SliceHeader {
        SliceHeader {
            slice_type: SliceType::P,
            nb_refs: [1, 0],
            ..Default::default()
        }
    }

    #[test]
    fn frame_nb_refs_counts_used_entries() {
        use wedeo_codec_hevc_ps::ShortTermRps;

        let mut sh = empty_sh();
        sh.short_term_rps = Some(ShortTermRps {
            num_negative_pics: 3,
            num_delta_pocs: 5,
            used: 0b10101, // 3 used
            ..Default::default()
        });
        sh.long_term_rps.nb_refs = 2;
        sh.long_term_rps.used[0] = true;
        assert_eq!(frame_nb_refs(&sh, false, false), 4);
        assert_eq!(frame_nb_refs(&sh, true, true), 6);
    }
}
