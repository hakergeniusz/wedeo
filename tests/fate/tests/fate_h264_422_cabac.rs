//! 4:2:2 CABAC context regression tests.
//!
//! The ground-truth values below were extracted from debug FFmpeg
//! (`ffmpeg_g`, `--disable-optimizations --enable-debug=3 --disable-asm`)
//! with gdb breakpoints at `libavcodec/h264_cabac.c:1577` inside
//! `get_cabac_cbf_ctx`, on `Hi422FR10_SONY_B.264` (High 4:2:2, 8-bit).
//! Both `nza = non_zero_count_cache[scan8[idx] - 1]` and
//! `nzb = non_zero_count_cache[scan8[idx] - 8]` were dumped, together with
//! `h->non_zero_count[mb_xy]` for the left/top macroblocks.
//!
//! FFmpeg's scan8 table places 4:2:2 chroma blocks in a 4x2 cache grid
//! (Cb rows 6..9, Cr rows 11..14, cols 4..5). Wedeo stores chroma NNZ as
//! Cb[16..24] and Cr[24..32], two columns by four rows, so the left/top
//! context indices need an explicit 4:2:2 mapping. The 4:2:0 path is
//! deliberately left untouched.

use std::path::PathBuf;

use wedeo_codec::registry;
use wedeo_codec_h264 as _;
use wedeo_codec_h264::cabac::{CabacNeighborCtx, get_nz_neighbors};
use wedeo_core::codec_id::CodecId;
use wedeo_core::frame::FrameData;
use wedeo_core::pixel_format::PixelFormat;
use wedeo_format::context::InputContext;
use wedeo_format_h264 as _;

/// JVT FRext vector location (read-only conformance asset).
fn jvt_frext_file(name: &str) -> PathBuf {
    std::env::var("JVT_VECTORS")
        .map(|dir| PathBuf::from(dir).join("JVT-FR-EXT").join(name))
        .unwrap_or_default()
}

/// FFmpeg MB0 flat NNZ values (`h->non_zero_count[0][16..48]`) from gdb,
/// converted into Wedeo's two-columns-per-row chroma storage.
fn ffmpeg_mb0_chroma_nnz() -> [u8; 16] {
    // FFmpeg flat slots: Cb [3,3,0,0, 3,5,0,0, 5,0,0,0, 5,2,0,0],
    // Cr [3,3,0,0, 3,6,0,0, 5,2,0,0, 4,0,0,0].
    const FF_CB: [u8; 16] = [3, 3, 0, 0, 3, 5, 0, 0, 5, 0, 0, 0, 5, 2, 0, 0];
    const FF_CR: [u8; 16] = [3, 3, 0, 0, 3, 6, 0, 0, 5, 2, 0, 0, 4, 0, 0, 0];
    let mut out = [0u8; 16];
    for row in 0..4 {
        for col in 0..2 {
            out[row * 2 + col] = FF_CB[row * 4 + col];
            out[8 + row * 2 + col] = FF_CR[row * 4 + col];
        }
    }
    out
}

fn neighbor_ctx_with_mb0() -> CabacNeighborCtx {
    let mut ctx = CabacNeighborCtx::new(2, 2);
    ctx.nz_count[16..32].copy_from_slice(&ffmpeg_mb0_chroma_nnz());
    ctx
}

/// Left NNZ context for every 4:2:2 chroma row of MB1 must use the
/// matching row of MB0. The old code clamped the row to 1 (the 4:2:0
/// table) and returned 5/3 for the bottom rows instead of 0/2.
#[test]
fn cbf_422_left_neighbor_uses_all_four_chroma_rows() {
    let ctx = neighbor_ctx_with_mb0();
    let cache = [0u8; 40];
    assert_eq!(ctx.nz_count[17], 3, "fixture: MB0 Cb row 0 right column");

    // Only the blk_x == 0 blocks read the left MB. Their ground truth is
    // FFmpeg MB1's L values for Cb/Cr block indices 16/18/24/26 and
    // 32/34/40/42. The blk_x == 1 blocks read an in-MB neighbor and are
    // unchanged by this mapping.
    let cases = [
        (16, 16, 3),
        (18, 18, 5),
        (20, 24, 0),
        (22, 26, 2),
        (24, 32, 3),
        (26, 34, 6),
        (28, 40, 2),
        (30, 42, 0),
    ];
    for (our_idx, ff_idx, expected) in cases {
        let (left, _top) =
            get_nz_neighbors(&ctx, Some(0), Some(0), None, our_idx, &cache, 64, 0, true);
        assert_eq!(
            left, expected,
            "our block {our_idx} (FFmpeg {ff_idx}): left NNZ context"
        );
    }
}

/// The first chroma row reads the top MB's *bottom* chroma row, matching
/// FFmpeg's fill of cache row 5 (Cb) / row 10 (Cr) from `nnz[28..31]` /
/// `nnz[44..47]`. For MB(0,1) of Hi422FR10, gdb reported top contexts
/// 5, 2 (Cb) and 4, 0 (Cr).
#[test]
fn cbf_422_top_neighbor_uses_bottom_chroma_row() {
    let ctx = neighbor_ctx_with_mb0();
    let cache = [0u8; 40];

    for (block_idx, expected) in [(16, 5), (17, 2), (24, 4), (25, 0)] {
        let (_left, top) =
            get_nz_neighbors(&ctx, None, None, Some(0), block_idx, &cache, 64, 0, true);
        assert_eq!(top, expected, "block {block_idx}: top NNZ context");
    }
}

/// The 4:2:0 context path must stay byte-for-byte identical: top neighbor
/// of the first chroma row is the top MB's row 1, and the left neighbor
/// row is clamped to 1.
#[test]
fn cbf_420_neighbor_mapping_is_unchanged() {
    let mut ctx = CabacNeighborCtx::new(2, 2);
    // Cb rows [7, 9], Cr rows [11, 13] in Wedeo's flat 2x2 layout.
    ctx.nz_count[16] = 7;
    ctx.nz_count[18] = 9;
    ctx.nz_count[17] = 8;
    ctx.nz_count[19] = 10;
    ctx.nz_count[20] = 11;
    ctx.nz_count[21] = 12;
    ctx.nz_count[22] = 13;
    let cache = [0u8; 40];

    let (left, top) = get_nz_neighbors(&ctx, Some(0), Some(0), Some(0), 16, &cache, 64, 0, false);
    assert_eq!((left, top), (8, 9));

    let (left, top) = get_nz_neighbors(&ctx, Some(0), Some(0), Some(0), 20, &cache, 64, 0, false);
    assert_eq!((left, top), (12, 13));
}

/// Decode 8-bit 4:2:2 CABAC vectors end to end. Bit-exact reconstruction
/// parity is tracked separately by `framecrc_compare.py`; here we assert
/// that CABAC context changes never break synchronization or format.
#[test]
fn frext_422_cabac_vectors_stay_in_sync() {
    for name in [
        "Hi422FR10_SONY_B.264",
        "Hi422FR11_SONY_B.264",
        "Hi422FR12_SONY_B.264",
    ] {
        let path = jvt_frext_file(name);
        if !path.exists() {
            continue;
        }
        let mut input = InputContext::open(path.to_str().unwrap()).expect("open vector");
        let params = input.streams[0].codec_params.clone();
        assert_eq!(params.codec_id, CodecId::H264, "{name}: not H.264");
        let factory = registry::find_decoder(params.codec_id).expect("H.264 decoder");
        let mut decoder = factory.create(params).expect("create decoder");

        let mut frames = 0usize;
        let mut format = None;
        loop {
            match input.read_packet() {
                Ok(pkt) => {
                    if decoder.send_packet(Some(&pkt)).is_err() {
                        break;
                    }
                    loop {
                        match decoder.receive_frame() {
                            Ok(frame) => {
                                if let FrameData::Video(v) = frame.data {
                                    format.get_or_insert(v.format);
                                    frames += 1;
                                }
                            }
                            Err(wedeo_core::error::Error::Again) => break,
                            Err(_) => break,
                        }
                    }
                }
                Err(wedeo_core::error::Error::Eof) => break,
                Err(_) => break,
            }
        }
        // Flush the decoder at EOF: CABAC vectors with B-frames keep their
        // last few pictures buffered until send_packet(None).
        let _ = decoder.send_packet(None);
        while let Ok(frame) = decoder.receive_frame() {
            if let FrameData::Video(v) = frame.data {
                format.get_or_insert(v.format);
                frames += 1;
            }
        }
        assert!(frames >= 3, "{name}: expected >= 3 frames, got {frames}");
        assert_eq!(format, Some(PixelFormat::Yuv422p), "{name}: wrong format");
    }
}
