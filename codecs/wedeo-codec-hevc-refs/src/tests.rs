//! Unit tests exercising the DPB, RPS application and reference-list
//! construction together with synthetic contexts.

use wedeo_codec_hevc_ps::{L0, NalUnitType, ShortTermRps, SliceHeader, SliceType};

use crate::dpb::{Dpb, FrameRpsParams, NewRefParams};
use crate::frame::{FrameFlags, FrameRef, MvFieldTable};
use crate::rpl::{NB_RPS_TYPE, RpsType, SliceRplParams, slice_rpl};

fn new_ref_params(poc: i32) -> NewRefParams {
    NewRefParams {
        poc,
        pic_output_flag: true,
        is_irap: poc == 0,
        is_recovering: false,
        recovery_poc: i32::MAX,
        output_corrupt: false,
        show_all: false,
        tab_mvf: MvFieldTable::default(),
    }
}

fn frame_rps_params<'a>(sh: &'a SliceHeader, poc: i32) -> FrameRpsParams<'a> {
    FrameRpsParams {
        sh,
        // IRAP so that an unavailable/missing reference is allowed to be
        // generated (FFmpeg's corrupt-ref guard is skipped for IRAP pictures).
        nal_unit_type: NalUnitType::CraNut,
        poc,
        log2_max_poc_lsb: 8,
        recovery_poc: i32::MAX,
        output_corrupt: false,
        show_all: false,
        inter_layer_poc: None,
    }
}

#[test]
fn rps_application_marks_short_and_long_term_refs() {
    let mut dpb = Dpb::default();
    for poc in [0, 4, 8, 12] {
        dpb.set_new_ref(new_ref_params(poc)).unwrap();
    }
    let cur = dpb.set_new_ref(new_ref_params(16)).unwrap();

    // Two refs before current (POC 12, 8).
    let mut rps = ShortTermRps {
        num_negative_pics: 2,
        num_delta_pocs: 2,
        used: 0b11,
        ..Default::default()
    };
    rps.delta_poc[0] = -4;
    rps.delta_poc[1] = -8;
    let mut sh = SliceHeader {
        short_term_rps: Some(rps),
        ..Default::default()
    };
    // `LongTermRps` is not re-exported by wedeo-codec-hevc-ps; mutate the
    // public field on SliceHeader directly instead of naming the type.
    sh.long_term_rps.nb_refs = 1;
    sh.long_term_rps.poc[0] = 0;
    sh.long_term_rps.used[0] = true;
    sh.long_term_rps.poc_msb_present[0] = false;

    dpb.frame_rps(&frame_rps_params(&sh, 16)).unwrap();

    assert_eq!(dpb.rps[RpsType::StCurrBef as usize].nb_refs, 2);
    assert_eq!(dpb.rps[RpsType::StCurrBef as usize].list[0], 12);
    assert_eq!(dpb.rps[RpsType::StCurrBef as usize].list[1], 8);
    assert_eq!(dpb.rps[RpsType::LtCurr as usize].nb_refs, 1);
    assert_eq!(dpb.rps[RpsType::LtCurr as usize].list[0], 0);

    // Referenced frames are marked, the current frame keeps its own flags.
    assert!(dpb.frame(3).unwrap().flags.contains(FrameFlags::SHORT_REF));
    assert!(dpb.frame(0).unwrap().flags.contains(FrameFlags::LONG_REF));
    assert!(
        dpb.frame(cur)
            .unwrap()
            .flags
            .contains(FrameFlags::SHORT_REF)
    );
}

#[test]
fn rps_application_generates_missing_reference() {
    let mut dpb = Dpb::default();
    dpb.set_new_ref(new_ref_params(0)).unwrap();
    dpb.set_new_ref(new_ref_params(8)).unwrap();

    let mut rps = ShortTermRps {
        num_negative_pics: 1,
        num_delta_pocs: 1,
        used: 1,
        ..Default::default()
    };
    rps.delta_poc[0] = -6; // POC 10 does not exist in the DPB
    let sh = SliceHeader {
        short_term_rps: Some(rps),
        ..Default::default()
    };

    dpb.frame_rps(&frame_rps_params(&sh, 16)).unwrap();
    let list = &dpb.rps[RpsType::StCurrBef as usize];
    assert_eq!(list.list[0], 10);
    let handle = list.r#ref[0].unwrap();
    assert!(
        dpb.resolve(handle)
            .unwrap()
            .flags
            .contains(FrameFlags::UNAVAILABLE)
    );
}

#[test]
fn list_building_concatenates_and_applies_modification() {
    let mut dpb = Dpb::default();
    let slot_a = dpb.set_new_ref(new_ref_params(4)).unwrap();
    let slot_b = dpb.set_new_ref(new_ref_params(8)).unwrap();
    let cur = dpb.set_new_ref(new_ref_params(12)).unwrap();

    // Fill L0 with two refs (A then B) and LT_CURR with one.
    let mut rps = std::array::from_fn(|_| crate::RefPicList::default());
    for (rps_type, slot, poc) in [
        (RpsType::StCurrBef, slot_a, 4),
        (RpsType::StCurrAft, slot_b, 8),
    ] {
        let list = &mut rps[rps_type as usize];
        let id = dpb.frame(slot).unwrap().id;
        list.list[0] = poc;
        list.r#ref[0] = Some(FrameRef::new(slot, id, poc));
        list.nb_refs = 1;
    }

    let mut sh = SliceHeader {
        slice_type: SliceType::P,
        nb_refs: [2, 0],
        ..Default::default()
    };
    sh.rpl_modification_flag[L0] = true;
    sh.list_entry_lx[L0][0] = 1; // B first
    sh.list_entry_lx[L0][1] = 0; // A second

    let frame = dpb.frame_mut(cur).unwrap();
    // One RPL slot per NAL, all CTBs mapped to slot 0.
    frame.rpl = vec![[crate::RefPicList::default(); 2]; 1];
    frame.rpl_tab = vec![0; 1];

    let params = SliceRplParams {
        sh: &sh,
        pps_curr_pic_ref_enabled_flag: false,
        log2_ctb_size: 6,
        ctb_width: 1,
        ctb_count: 1,
        ctb_addr_rs_to_ts: &[0],
        slice_segment_addr: 0,
        slice_idx: 0,
    };
    slice_rpl(frame, &rps, &params).unwrap();

    let built = &frame.rpl[0][L0];
    assert_eq!(built.nb_refs, 2);
    assert_eq!(built.list[0], 8);
    assert_eq!(built.list[1], 4);
}

#[test]
fn list_building_rejects_invalid_modification_index() {
    let mut dpb = Dpb::default();
    let cur = dpb.set_new_ref(new_ref_params(12)).unwrap();
    let mut rps = std::array::from_fn(|_| crate::RefPicList::default());
    let id = dpb.frame(cur).unwrap().id;
    rps[RpsType::StCurrBef as usize].list[0] = 4;
    rps[RpsType::StCurrBef as usize].r#ref[0] = Some(FrameRef::new(cur, id, 4));
    rps[RpsType::StCurrBef as usize].nb_refs = 1;

    let mut sh = SliceHeader {
        slice_type: SliceType::P,
        nb_refs: [1, 0],
        ..Default::default()
    };
    sh.rpl_modification_flag[L0] = true;
    sh.list_entry_lx[L0][0] = 5; // out of range

    let frame = dpb.frame_mut(cur).unwrap();
    frame.rpl = vec![[crate::RefPicList::default(); 2]; 1];
    frame.rpl_tab = vec![0; 1];

    let params = SliceRplParams {
        sh: &sh,
        pps_curr_pic_ref_enabled_flag: false,
        log2_ctb_size: 6,
        ctb_width: 1,
        ctb_count: 1,
        ctb_addr_rs_to_ts: &[0],
        slice_segment_addr: 0,
        slice_idx: 0,
    };
    assert!(slice_rpl(frame, &rps, &params).is_err());
    assert_eq!(NB_RPS_TYPE, 7);
}
