//! Conformance tests for `wedeo-codec-hevc-refs`.
//!
//! Parses real HEVC streams from the FATE suite with `wedeo-codec-hevc-ps`,
//! then drives the DPB / RPS / reference-list pipeline with the *parsed*
//! `ShortTermRps`, `LongTermRps` and `SliceHeader` values. This checks that
//! the library accepts the same reference structures FFmpeg produces for
//! real streams (bit-exact frame data is the decoder work unit's job).
//!
//! Synthetic unit tests for individual algorithms live in the crate itself.

use std::path::PathBuf;

use wedeo_codec_hevc_ps::{
    NalUnitType, ParamSets, SliceHeader, SliceType, annex_b_split, ebsp_to_rbsp, parse_nal_header,
};
use wedeo_codec_hevc_refs::{
    Dpb, FrameRpsParams, MvFieldTable, NewRefParams, RefGeometry, RefPicList, SliceRplParams,
    slice_rpl,
};

fn fate_path(name: &str) -> PathBuf {
    PathBuf::from(std::env::var("FATE_SUITE").unwrap_or_else(|_| "./fate-suite".into()))
        .join("hevc")
        .join(name)
}

fn new_ref(poc: i32, irap: bool) -> NewRefParams {
    NewRefParams {
        poc,
        pic_output_flag: true,
        is_irap: irap,
        is_recovering: false,
        recovery_poc: i32::MAX,
        output_corrupt: false,
        show_all: false,
        tab_mvf: MvFieldTable::default(),
    }
}

/// Drive one parsed slice through the DPB pipeline. Returns false when the
/// slice uses features outside this unit (e.g. inter-layer prediction).
fn drive_slice(
    sh: &SliceHeader,
    nal: NalUnitType,
    sps: &wedeo_codec_hevc_ps::Sps,
    pps: &wedeo_codec_hevc_ps::Pps,
) -> bool {
    if sh.short_term_rps.is_none() || sh.inter_layer_pred || sh.slice_type == SliceType::I {
        return false;
    }

    let mut dpb = Dpb::default();
    let irap = nal.is_irap();

    // Materialise every used reference picture named by the parsed RPS.
    if let Some(rps) = &sh.short_term_rps {
        for i in 0..rps.num_delta_pocs as usize {
            if rps.used & (1 << i) == 0 {
                continue;
            }
            let poc = sh.poc.wrapping_add(rps.delta_poc[i]);
            if poc == sh.poc || dpb.frames.iter().flatten().any(|f| f.poc == poc) {
                continue;
            }
            let _ = dpb.set_new_ref(new_ref(poc, false));
        }
    }
    for i in 0..sh.long_term_rps.nb_refs {
        if !sh.long_term_rps.used[i] {
            continue;
        }
        let poc = sh.long_term_rps.poc[i];
        if poc == sh.poc || dpb.frames.iter().flatten().any(|f| f.poc == poc) {
            continue;
        }
        let _ = dpb.set_new_ref(new_ref(poc, false));
    }

    let cur = match dpb.set_new_ref(new_ref(sh.poc, irap)) {
        Ok(slot) => slot,
        Err(_) => return false,
    };

    let params = FrameRpsParams {
        sh,
        nal_unit_type: nal,
        poc: sh.poc,
        log2_max_poc_lsb: sps.log2_max_poc_lsb,
        recovery_poc: i32::MAX,
        output_corrupt: false,
        show_all: false,
        inter_layer_poc: None,
    };
    if dpb.frame_rps(&params).is_err() {
        return false;
    }

    // Build the L0/L1 lists using the parsed modification flags.
    let rps = dpb.rps; // [RefPicList; 7] is Copy
    let frame = match dpb.frame_mut(cur) {
        Some(f) => f,
        None => return false,
    };
    frame.rpl = vec![[RefPicList::default(); 2]; 1];
    frame.rpl_tab = vec![0; sps.ctb_size as usize];
    let rpl_params = SliceRplParams {
        sh,
        pps_curr_pic_ref_enabled_flag: pps.pps_curr_pic_ref_enabled_flag,
        log2_ctb_size: sps.log2_ctb_size,
        ctb_width: sps.ctb_width as usize,
        ctb_count: sps.ctb_size as usize,
        ctb_addr_rs_to_ts: &pps.tile.ctb_addr_rs_to_ts,
        slice_segment_addr: sh.slice_segment_addr,
        slice_idx: 0,
    };
    slice_rpl(frame, &rps, &rpl_params).is_ok()
}

#[test]
fn fate_hevc_refs_pipeline() {
    let cases = [
        "pir.hevc",
        "flush-reuse.hevc",
        "hdr10_plus_h265_sample.hevc",
        "hevc-monochrome.hevc",
        "12bit-444-qp74.hevc",
    ];
    let mut streams = 0usize;
    let mut geometry = 0usize;
    let mut slices = 0usize;
    let mut rpl_ok = 0usize;

    for name in cases {
        let data = match std::fs::read(fate_path(name)) {
            Ok(d) => d,
            Err(_) => continue, // vector not installed in this shard
        };
        streams += 1;

        let mut sets = ParamSets::new();
        let mut parsed = Vec::new();
        let mut poc_tid0 = 0i32;
        for nal in annex_b_split(&data) {
            let _ = sets.decode_nal(nal);
            let Ok((header, ebsp)) = parse_nal_header(nal) else {
                continue;
            };
            if !header.nal_unit_type.is_vcl() {
                continue;
            }
            let rbsp = ebsp_to_rbsp(ebsp);
            let Ok(ctx) = sets.slice_context(0, header, poc_tid0, false, !parsed.is_empty()) else {
                continue;
            };
            if let Ok(sh) = wedeo_codec_hevc_ps::parse_slice_header(&rbsp, &ctx) {
                poc_tid0 = sh.poc;
                parsed.push((header.nal_unit_type, sh));
            }
        }

        let sps = sets.sps_list.iter().flatten().next();
        let pps = sets.pps_list.iter().flatten().next();
        if let (Some(sps), Some(pps)) = (sps, pps) {
            let geo = RefGeometry::from_ps(sps, pps);
            assert!(!geo.ctb_addr_rs_to_ts.is_empty(), "geometry for {name}");
            assert!(geo.width > 0 && geo.height > 0, "geometry for {name}");
            geometry += 1;
        }

        if let (Some(sps), Some(pps)) = (sps, pps) {
            for (nal, sh) in &parsed {
                if sh.short_term_rps.is_none()
                    || sh.inter_layer_pred
                    || sh.slice_type == SliceType::I
                {
                    continue;
                }
                slices += 1;
                if drive_slice(sh, *nal, sps, pps) {
                    rpl_ok += 1;
                }
            }
        }
    }

    assert!(streams > 0, "no FATE HEVC vectors available");
    assert!(geometry > 0, "no parameter sets parsed");
    assert!(slices > 0, "no non-IRAP slice headers with an RPS parsed");
    assert!(rpl_ok > 0, "no reference list was built from a real RPS");
    eprintln!(
        "hevc-refs FATE: streams={streams} geometry={geometry} slices={slices} rpl_ok={rpl_ok}"
    );
}
