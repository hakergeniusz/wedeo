//! Conformance tests for `wedeo-codec-hevc-ps`.
//!
//! Parses real HEVC parameter sets from the FATE suite and checks the derived
//! SPS fields against FFmpeg's own `Parsed SPS` debug output (coded wxh,
//! cropped wxh, pix_fmt) and ffprobe profile reporting. Synthetic bitstream
//! tests built from the FFmpeg field semantics live in the crate's unit tests.

use std::path::PathBuf;

use wedeo_codec_hevc_ps::{
    NalUnitType, annex_b_split, ebsp_to_rbsp, parse_nal_header, parse_pps, parse_slice_header,
    parse_sps, parse_vps,
};

fn fate_path(name: &str) -> PathBuf {
    PathBuf::from(std::env::var("FATE_SUITE").unwrap_or_else(|_| "./fate-suite".into()))
        .join("hevc")
        .join(name)
}

// (file, coded w, coded h, cropped w, cropped h, chroma_format_idc,
//  bit_depth, profile_idc) -- ground truth from `ffmpeg -debug bitstream`.
type FateCase = (&'static str, u32, u32, u32, u32, u32, u32, u32);

#[test]
fn fate_hevc_parameter_sets() {
    let cases: &[FateCase] = &[
        ("food.hevc", 3840, 2160, 3840, 2160, 2, 10, 4),
        ("hevc-monochrome.hevc", 384, 256, 384, 240, 0, 8, 4),
        ("12bit-444-qp74.hevc", 128, 64, 128, 64, 3, 12, 4),
        (
            "hdr10_plus_h265_sample.hevc",
            1280,
            736,
            1280,
            720,
            1,
            10,
            2,
        ),
        ("pir.hevc", 1920, 1080, 1920, 1080, 1, 8, 1),
        ("flush-reuse.hevc", 128, 96, 128, 96, 1, 8, 1),
    ];
    let mut checked = 0usize;
    let mut slice_headers_checked = 0usize;
    for &(name, width, height, crop_w, crop_h, chroma, depth, profile) in cases {
        let data = match std::fs::read(fate_path(name)) {
            Ok(d) => d,
            Err(_) => continue, // sample not installed in this shard
        };
        let mut vps_rbsp = None;
        let mut sps_rbsp = None;
        let mut pps_rbsp = None;
        let mut n_vcl = 0usize;
        let mut nuh_layer = 0u8;
        for nal in annex_b_split(&data) {
            let Ok((header, ebsp)) = parse_nal_header(nal) else {
                continue;
            };
            let rbsp = ebsp_to_rbsp(ebsp);
            match header.nal_unit_type {
                NalUnitType::Vps if vps_rbsp.is_none() => vps_rbsp = Some(rbsp),
                NalUnitType::Sps if sps_rbsp.is_none() => {
                    sps_rbsp = Some(rbsp);
                    nuh_layer = header.nuh_layer_id;
                }
                NalUnitType::Pps if pps_rbsp.is_none() => pps_rbsp = Some(rbsp),
                t if t.is_vcl() => n_vcl += 1,
                _ => {}
            }
        }
        let vps_rbsp = vps_rbsp.unwrap_or_else(|| panic!("no VPS in {name}"));
        let vps =
            parse_vps(&vps_rbsp).unwrap_or_else(|e| panic!("VPS parse failed for {name}: {e}"));
        assert!(vps.vps_max_sub_layers >= 1, "{name}");

        let sps_rbsp = sps_rbsp.unwrap_or_else(|| panic!("no SPS in {name}"));
        let sps = parse_sps(&sps_rbsp, nuh_layer)
            .unwrap_or_else(|e| panic!("SPS parse failed for {name}: {e}"));
        assert_eq!(sps.width, width, "coded width mismatch for {name}");
        assert_eq!(sps.height, height, "coded height mismatch for {name}");
        assert_eq!(
            sps.cropped_width(),
            crop_w,
            "cropped width mismatch for {name}"
        );
        assert_eq!(
            sps.cropped_height(),
            crop_h,
            "cropped height mismatch for {name}"
        );
        assert_eq!(sps.chroma_format_idc, chroma, "chroma mismatch for {name}");
        assert_eq!(sps.bit_depth, depth, "bit depth mismatch for {name}");
        assert_eq!(sps.ptl.profile_idc, profile, "profile mismatch for {name}");

        let pps_rbsp = pps_rbsp.unwrap_or_else(|| panic!("no PPS in {name}"));
        let pps = parse_pps(&pps_rbsp, &sps)
            .unwrap_or_else(|e| panic!("PPS parse failed for {name}: {e}"));
        assert_eq!(pps.sps_id, sps.sps_id, "{name}");
        assert_eq!(
            pps.tile.ctb_addr_rs_to_ts.len(),
            sps.ctb_size as usize,
            "{name}"
        );
        assert!(n_vcl > 0, "no VCL NALs in {name}");

        // Exercise the slice-header API on the first IRAP slice.
        let mut sets = wedeo_codec_hevc_ps::ParamSets::new();
        for nal in annex_b_split(&data) {
            if nal.len() < 2 {
                continue;
            }
            let _ = sets.decode_nal(nal);
            if let Ok((h, ebsp)) = parse_nal_header(nal)
                && h.nal_unit_type.is_irap()
            {
                let rbsp = ebsp_to_rbsp(ebsp);
                let ctx = sets
                    .slice_context(0, h, 0, false, false)
                    .unwrap_or_else(|e| panic!("slice context failed for {name}: {e}"));
                let sh = parse_slice_header(&rbsp, &ctx)
                    .unwrap_or_else(|e| panic!("slice header parse failed for {name}: {e}"));
                assert_eq!(sh.slice_type, wedeo_codec_hevc_ps::SliceType::I, "{name}");
                assert!(sh.data_offset > 0, "{name}");
                slice_headers_checked += 1;
                break;
            }
        }
        checked += 1;
    }
    assert!(checked > 0, "no FATE HEVC vectors were available");
    assert!(
        slice_headers_checked > 0,
        "no real slice headers were parsed"
    );
}
