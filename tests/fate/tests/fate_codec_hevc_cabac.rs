//! Conformance / integration tests for `wedeo-codec-hevc-cabac`.
//!
//! The engine is a library (not a framecrc-producing decoder), so these tests
//! verify two things: exact FFmpeg-derived bin values on synthetic streams,
//! and panic-free, bounded decoding on real HEVC slice data from the FATE
//! suite. Per-syntax-element ground-truth values live in the crate's unit
//! tests.

use std::path::PathBuf;

use wedeo_codec_hevc_cabac::{Cabac, ScanType};
use wedeo_codec_hevc_ps::{
    ParamSets, annex_b_split, ebsp_to_rbsp, parse_nal_header, parse_slice_header,
};

fn fate_path(name: &str) -> PathBuf {
    PathBuf::from(std::env::var("FATE_SUITE").unwrap_or_else(|_| "./fate-suite".into()))
        .join("hevc")
        .join(name)
}

#[test]
fn fate_hevc_cabac_synthetic_stream() {
    // Fixed aligned-init path; exact bins cross-checked against FFmpeg's
    // ff_init_cabac_decoder + get_cabac_bypass built standalone.
    let data = vec![0xAAu8; 40];
    let mut c = Cabac::new(&data).unwrap();
    let bins: String = (0..24)
        .map(|_| char::from(b'0' + c.get_cabac_bypass()))
        .collect();
    assert_eq!(bins, "101010110101011000000000");

    let data55 = vec![0x55u8; 40];
    let mut c = Cabac::new(&data55).unwrap();
    c.init_state(0, 26);
    assert_eq!(c.decode_sao_merge_flag(), 0);
    assert_eq!(c.decode_pred_mode_flag(), 0);
    assert!(c.range() >= 256 && c.range() <= 510);
    assert_eq!(c.pos(), 2);
}

#[test]
fn fate_hevc_cabac_real_slices() {
    let cases = [
        "food.hevc",
        "hevc-monochrome.hevc",
        "pir.hevc",
        "12bit-444-qp74.hevc",
        "hdr10_plus_h265_sample.hevc",
    ];
    let mut slices_checked = 0usize;
    for name in cases {
        let Ok(data) = std::fs::read(fate_path(name)) else {
            continue; // sample not installed in this shard
        };
        let mut sets = ParamSets::new();
        for nal in annex_b_split(&data) {
            let _ = sets.decode_nal(nal);
        }
        for nal in annex_b_split(&data) {
            let Ok((header, ebsp)) = parse_nal_header(nal) else {
                continue;
            };
            if !header.nal_unit_type.is_vcl() {
                continue;
            }
            let Ok(ctx) = sets.slice_context(0, header, 0, false, false) else {
                continue;
            };
            let rbsp = ebsp_to_rbsp(ebsp);
            let Ok(sh) = parse_slice_header(&rbsp, &ctx) else {
                continue;
            };
            let start = sh.data_offset.min(rbsp.len().saturating_sub(2));
            let cabac_data = &rbsp[start..];
            let mut cabac = match Cabac::new(cabac_data) {
                Ok(c) => c,
                Err(_) => continue,
            };
            cabac.init_state_from_slice(&sh);
            // Deterministic prefix of bins across a spread of contexts.
            for &ctx_idx in &[0usize, 6, 12, 37, 42, 53, 71, 93, 137, 161] {
                let _ = cabac.get_cabac(ctx_idx);
                assert!(
                    cabac.range() >= 256 && cabac.range() <= 510,
                    "invalid range on {name} ctx {ctx_idx}"
                );
            }
            for _ in 0..16 {
                let _ = cabac.get_cabac_bypass();
            }
            assert!(cabac.pos() <= cabac_data.len() + 8, "overrun on {name}");
            slices_checked += 1;
            break; // one slice per sample is enough to exercise the engine
        }
    }
    wedeo_fate::skip_if_no_samples!(slices_checked, "HEVC CABAC slice");
}

#[test]
fn fate_hevc_cabac_scan_tables_from_ps() {
    // The crate must re-export the PS crate's diagonal scans unchanged.
    use wedeo_codec_hevc_cabac::tables as t;
    assert_eq!(t::DIAG_SCAN4X4_X.len(), 16);
    assert_eq!(t::DIAG_SCAN8X8_X.len(), 64);
    assert_eq!(t::DIAG_SCAN8X8_X[63], 7);
    assert_eq!(t::DIAG_SCAN8X8_Y[63], 7);
    assert_eq!(ScanType::Diag as u8, 0);
    assert_eq!(ScanType::Vert as u8, 2);
}
