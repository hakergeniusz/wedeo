//! FATE regressions for the HEVC intra / reference-prediction work unit.
//!
//! Covers three ground-truth findings:
//! 1. 4:4:4 `intra_chroma_pred_mode` is decoded once per prediction unit
//!    (`hevcdec.c:2381-2393`), not once per coding unit.
//! 2. `intra_prediction_unit_default_value()` only marks `tab_mvf` entries
//!    PF_INTRA for intra CUs (`hevcdec.c:2437-2439`); skip/inter CUs keep
//!    their MVs for the next block's candidate derivation.
//! 3. `MIN_TB_ADDR_ZS()` uses a table shifted by one row and column into the
//!    padded allocation (`ps.c:2177-2193`); `z_scan_block_avail()` depends on
//!    that offset.

use std::path::PathBuf;

use wedeo_codec::decoder::DecoderBuilder;
use wedeo_codec_hevc as _;
use wedeo_codec_hevc::intra::{IntraState, intra_defaults};
use wedeo_codec_hevc::picture::Picture;
use wedeo_codec_hevc_ps::{ParamSets, Sps, annex_b_split};
use wedeo_codec_hevc_refs::{Mv, MvField, PredFlag, RefGeometry};
use wedeo_core::codec_id::CodecId;
use wedeo_core::media_type::MediaType;
use wedeo_core::packet::Packet;

fn fate_path(name: &str) -> PathBuf {
    PathBuf::from(std::env::var("FATE_SUITE").unwrap_or_else(|_| "./fate-suite".into()))
        .join("hevc")
        .join(name)
}

fn params(extradata: Vec<u8>) -> wedeo_codec::decoder::CodecParameters {
    let mut p = wedeo_codec::decoder::CodecParameters::new(CodecId::Hevc, MediaType::Video);
    p.extradata = extradata;
    p
}

fn decode_pir(data: &[u8]) -> usize {
    let mut decoder = DecoderBuilder::new(params(data.to_vec()))
        .open()
        .expect("native HEVC decoder must open");
    let mut packet = Packet::from_slice(data);
    packet.pts = 0;
    decoder.send_packet(Some(&packet)).expect("decode packet");
    decoder.send_packet(None).expect("drain decoder");
    let mut frames = 0usize;
    loop {
        match decoder.receive_frame() {
            Ok(frame) => {
                let video = frame.video().expect("video frame");
                assert_eq!(video.width, 1920, "frame {frames} width");
                assert_eq!(video.height, 1080, "frame {frames} height");
                frames += 1;
            }
            Err(wedeo_core::Error::Again) | Err(wedeo_core::Error::Eof) => break,
            Err(e) => panic!("unexpected decoder error after frame {frames}: {e}"),
        }
    }
    frames
}

/// pir.hevc used to abort on frame 1 with
/// `motion compensation source index 2170640 out of bounds (len 2170624)`.
#[test]
fn pir_hevc_decodes_every_frame() {
    let data = match std::fs::read(fate_path("pir.hevc")) {
        Ok(d) => d,
        Err(_) => return,
    };
    let frames = decode_pir(&data);
    assert_eq!(frames, 10, "FFmpeg decodes ten frames of pir.hevc");
}

/// `intra_defaults` must not erase the motion field of a skip/inter CU.
#[test]
fn intra_defaults_preserves_inter_motion_fields() {
    let sps = Sps {
        log2_min_pu_size: 2,
        chroma_format_idc: 1,
        ..Default::default()
    };
    let mut pic = Picture::new(0, 64, 64, 8, 1, 16, 16, 16, 16, 16, 16, 4);
    let mut state = IntraState::new(16, 16);

    let saved = MvField {
        mv: [Mv { x: 4, y: 8 }, Mv::ZERO],
        ref_idx: [0, 0],
        pred_flag: PredFlag::L0,
    };
    // PU (2, 2) covers luma samples (8..11, 8..11).
    *pic.tab_mvf.get_mut(2, 2).unwrap() = saved;
    intra_defaults(&mut state, &sps, 8, 8, 3, &mut pic);

    assert_eq!(*pic.tab_mvf.get(2, 2).unwrap(), saved);
    assert_eq!(state.tab_ipm[2 * 16 + 2], 1, "tab_ipm still set to DC");
}

/// Rebuild FFmpeg's `min_tb_addr_zs` table from the parsed CTB address map
/// and compare every entry with the one used by `wedeo-codec-hevc-refs`.
#[test]
fn z_scan_table_matches_ffmpeg_on_pir() {
    let data = match std::fs::read(fate_path("pir.hevc")) {
        Ok(d) => d,
        Err(_) => return,
    };
    let mut sets = ParamSets::new();
    for nal in annex_b_split(&data) {
        let _ = sets.decode_nal(nal);
    }
    let (Some(sps), Some(pps)) = (
        sets.sps_list.iter().flatten().next(),
        sets.pps_list.iter().flatten().next(),
    ) else {
        return;
    };
    let geo = RefGeometry::from_ps(sps, pps);

    let log2_diff = sps.log2_ctb_size - sps.log2_min_tb_size;
    let side = sps.tb_mask as usize + 2;
    let mut expected = vec![0i32; side * side];
    for y in 0..side {
        expected[y * side] = -1;
        expected[y] = -1;
    }
    for y in 0..=sps.tb_mask as usize {
        for x in 0..=sps.tb_mask as usize {
            let tb_x = x >> log2_diff;
            let tb_y = y >> log2_diff;
            let rs = sps.ctb_width as usize * tb_y + tb_x;
            let mut val = pps.tile.ctb_addr_rs_to_ts[rs] << (log2_diff * 2);
            for i in 0..log2_diff {
                let m = 1usize << i;
                if m & x != 0 {
                    val += (m * m) as i32;
                }
                if m & y != 0 {
                    val += (2 * m * m) as i32;
                }
            }
            expected[(y + 1) * side + x + 1] = val;
        }
    }
    assert_eq!(geo.min_tb_addr_zs_tab, expected);
    assert_eq!(geo.min_tb_addr_zs(0, 0), Some(0));
    assert_eq!(geo.min_tb_addr_zs(8, 8), Some(192));
    assert_eq!(geo.min_tb_addr_zs(8, 7), Some(106));
}

/// 4:4:4 smoke test: the extra chroma mode bins must not desynchronise CABAC.
#[test]
fn hevc_444_intra_still_decodes() {
    let data = match std::fs::read(fate_path("cbf_cr_cb_TUDepth_4_circle.h265")) {
        Ok(d) => d,
        Err(_) => return,
    };
    let mut decoder = DecoderBuilder::new(params(data.clone()))
        .open()
        .expect("native HEVC decoder must open");
    let mut packet = Packet::from_slice(&data);
    packet.pts = 0;
    decoder
        .send_packet(Some(&packet))
        .expect("decode 4:4:4 packet");
    decoder.send_packet(None).expect("drain 4:4:4 decoder");
    let mut frames = 0;
    loop {
        match decoder.receive_frame() {
            Ok(frame) => {
                let video = frame.video().expect("video frame");
                assert_eq!(video.width, 64);
                assert_eq!(video.height, 64);
                assert_eq!(video.planes.len(), 3, "4:4:4 has three planes");
                frames += 1;
            }
            Err(wedeo_core::Error::Again) | Err(wedeo_core::Error::Eof) => break,
            Err(e) => panic!("unexpected 4:4:4 decoder error: {e}"),
        }
    }
    assert_eq!(frames, 1);
}
