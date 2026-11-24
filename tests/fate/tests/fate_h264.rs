//! FATE-style integration tests for the H.264 decode pipeline.
//!
//! These tests verify:
//! 1. Correct demuxing of H.264 Annex B streams into per-frame access units
//! 2. Correct frame count, dimensions, and pixel format from the decoder
//! 3. End-to-end decode of conformance files (single-slice I-frame files)
//!
//! Tests that require fate-suite samples are skipped if the directory
//! is not present (controlled by FATE_SUITE env var or ./fate-suite/).

use std::path::{Path, PathBuf};

// Ensure codec and format registrations are linked.
use wedeo_codec_h264 as _;
use wedeo_format_h264 as _;

use wedeo_codec::registry;
use wedeo_codec_h264::nal::{NalUnitType, split_annex_b};
use wedeo_core::codec_id::CodecId;
use wedeo_core::error::Error;
use wedeo_core::frame::FrameData;
use wedeo_core::packet::Packet;
use wedeo_core::pixel_format::PixelFormat;
use wedeo_format::context::InputContext;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Get the fate-suite directory, or None if not available.
fn fate_suite_dir() -> Option<PathBuf> {
    // Check FATE_SUITE env var first
    if let Ok(dir) = std::env::var("FATE_SUITE") {
        let p = PathBuf::from(dir);
        if p.is_dir() {
            return Some(p);
        }
    }
    // Fall back to ./fate-suite/
    let p = PathBuf::from("fate-suite");
    if p.is_dir() {
        return Some(p);
    }
    // Try from workspace root
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("fate-suite");
    if p.is_dir() {
        return Some(p);
    }
    None
}

/// Get a conformance file path, or skip the test if not available.
fn conformance_file(name: &str) -> PathBuf {
    let dir = match fate_suite_dir() {
        Some(d) => d,
        None => {
            eprintln!("SKIP: fate-suite not found");
            return PathBuf::new();
        }
    };
    let path = dir.join("h264-conformance").join(name);
    if !path.exists() {
        eprintln!("SKIP: {} not found", path.display());
    }
    path
}

/// Open a file with InputContext and decode all frames.
/// Returns a list of (width, height, pixel_format, data_size) for each decoded frame.
fn decode_all_frames(path: &Path) -> Vec<(u32, u32, PixelFormat, usize)> {
    if !path.exists() {
        return Vec::new();
    }

    let mut input = InputContext::open(path.to_str().unwrap())
        .unwrap_or_else(|e| panic!("Failed to open {}: {:?}", path.display(), e));

    // Find the video stream
    let stream = &input.streams[0];
    let params = stream.codec_params.clone();

    // Create decoder
    let factory = registry::find_decoder(params.codec_id)
        .unwrap_or_else(|| panic!("No decoder for {:?}", params.codec_id));
    let mut decoder = factory.create(params).unwrap();

    let mut frames = Vec::new();

    // Demux + decode loop
    loop {
        match input.read_packet() {
            Ok(pkt) => {
                decoder.send_packet(Some(&pkt)).unwrap();
                loop {
                    match decoder.receive_frame() {
                        Ok(frame) => {
                            let (w, h, pf, size) = match &frame.data {
                                FrameData::Video(v) => {
                                    let total_size: usize =
                                        v.planes.iter().map(|p| p.buffer.size()).sum();
                                    (v.width, v.height, v.format, total_size)
                                }
                                _ => panic!("Expected video frame"),
                            };
                            frames.push((w, h, pf, size));
                        }
                        Err(Error::Again) => break,
                        Err(e) => panic!("receive_frame error: {:?}", e),
                    }
                }
            }
            Err(Error::Eof) => break,
            Err(e) => panic!("read_packet error: {:?}", e),
        }
    }

    // Drain
    decoder.send_packet(None).unwrap();
    loop {
        match decoder.receive_frame() {
            Ok(frame) => {
                let (w, h, pf, size) = match &frame.data {
                    FrameData::Video(v) => {
                        let total_size: usize = v.planes.iter().map(|p| p.buffer.size()).sum();
                        (v.width, v.height, v.format, total_size)
                    }
                    _ => panic!("Expected video frame"),
                };
                frames.push((w, h, pf, size));
            }
            Err(Error::Eof) => break,
            Err(e) => panic!("drain receive_frame error: {:?}", e),
        }
    }

    frames
}

/// Demux a file into packets without decoding it.
fn read_packets(path: &Path) -> Vec<Packet> {
    if !path.exists() {
        return Vec::new();
    }
    let mut input = InputContext::open(path.to_str().unwrap())
        .unwrap_or_else(|e| panic!("Failed to open {}: {:?}", path.display(), e));
    let mut packets = Vec::new();
    loop {
        match input.read_packet() {
            Ok(pkt) => packets.push(pkt),
            Err(Error::Eof) => break,
            Err(e) => panic!("read_packet error: {:?}", e),
        }
    }
    packets
}

/// Number of VCL (slice) NAL units in a packet.
fn slice_count(pkt: &Packet) -> usize {
    split_annex_b(pkt.data.data())
        .iter()
        .filter(|n| matches!(n.nal_type, NalUnitType::Slice | NalUnitType::Idr))
        .count()
}

/// Data lines of framecrc output (comments carry headers).
fn framecrc_data(lines: &[String]) -> Vec<String> {
    lines
        .iter()
        .filter(|l| !l.starts_with('#'))
        .map(|l| l.trim_end().to_string())
        .collect()
}

// ---------------------------------------------------------------------------
// Registration tests
// ---------------------------------------------------------------------------

#[test]
fn h264_decoder_registered() {
    let factory = registry::find_decoder(CodecId::H264);
    assert!(factory.is_some(), "H.264 decoder should be registered");
    let desc = factory.unwrap().descriptor();
    assert_eq!(desc.name, "h264");
    assert_eq!(desc.priority, 100);
}

#[test]
fn h264_demuxer_registered() {
    use wedeo_format::registry as fmt_registry;
    // Check that the H.264 demuxer can be found
    let h264_factory = fmt_registry::demuxers().find(|f| f.descriptor().name == "h264");
    assert!(h264_factory.is_some(), "H.264 demuxer should be registered");
}

// ---------------------------------------------------------------------------
// Conformance file tests (single-slice Baseline files)
// ---------------------------------------------------------------------------

/// BA1_Sony_D.jsv: Baseline profile, CAVLC, single slice per frame.
/// 176x144, 17 frames (1 IDR + 16 P-frames).
#[test]
fn fate_h264_ba1_sony_d() {
    let path = conformance_file("BA1_Sony_D.jsv");
    if !path.exists() {
        return;
    }

    let frames = decode_all_frames(&path);
    assert!(
        !frames.is_empty(),
        "Should decode at least one frame from BA1_Sony_D.jsv"
    );

    // All frames should be 176x144 YUV420p
    for (i, &(w, h, pf, size)) in frames.iter().enumerate() {
        assert_eq!(w, 176, "Frame {} width", i);
        assert_eq!(h, 144, "Frame {} height", i);
        assert_eq!(pf, PixelFormat::Yuv420p, "Frame {} pixel format", i);
        // YUV420p: W*H + 2*(W/2)*(H/2) = W*H*3/2
        let expected_size = 176 * 144 * 3 / 2;
        assert_eq!(size, expected_size, "Frame {} data size", i);
    }

    // BA1_Sony_D.jsv has 17 frames
    eprintln!(
        "BA1_Sony_D.jsv: decoded {} frames (expected 17)",
        frames.len()
    );
    assert_eq!(frames.len(), 17, "Expected 17 frames in BA1_Sony_D.jsv");
}

/// BA2_Sony_F.jsv: Baseline profile, CAVLC.
#[test]
fn fate_h264_ba2_sony_f() {
    let path = conformance_file("BA2_Sony_F.jsv");
    if !path.exists() {
        return;
    }

    let frames = decode_all_frames(&path);
    assert!(
        !frames.is_empty(),
        "Should decode at least one frame from BA2_Sony_F.jsv"
    );

    // All frames should be YUV420p
    for (i, &(_, _, pf, _)) in frames.iter().enumerate() {
        assert_eq!(pf, PixelFormat::Yuv420p, "Frame {} pixel format", i);
    }

    eprintln!("BA2_Sony_F.jsv: decoded {} frames", frames.len());
}

/// BASQP1_Sony_C.jsv: Baseline profile, variable QP.
#[test]
fn fate_h264_basqp1_sony_c() {
    let path = conformance_file("BASQP1_Sony_C.jsv");
    if !path.exists() {
        return;
    }

    let frames = decode_all_frames(&path);
    assert!(
        !frames.is_empty(),
        "Should decode at least one frame from BASQP1_Sony_C.jsv"
    );

    for (i, &(_, _, pf, _)) in frames.iter().enumerate() {
        assert_eq!(pf, PixelFormat::Yuv420p, "Frame {} pixel format", i);
    }

    eprintln!("BASQP1_Sony_C.jsv: decoded {} frames", frames.len());
}

/// Verify that the demuxer correctly splits NALs into per-frame access units.
/// BA1_Sony_D.jsv should produce 17 packets (one per frame).
#[test]
fn fate_h264_demuxer_au_grouping() {
    let path = conformance_file("BA1_Sony_D.jsv");
    if !path.exists() {
        return;
    }

    let packets = read_packets(&path);
    let packet_count = packets.len();
    eprintln!("BA1_Sony_D.jsv: {} packets from demuxer", packet_count);
    // Should have 17 packets (one per frame, not one giant packet)
    assert_eq!(
        packet_count, 17,
        "Demuxer should produce 17 packets for BA1_Sony_D.jsv"
    );

    // One access unit per packet: exactly one slice, monotonic PTS/DTS.
    for (i, pkt) in packets.iter().enumerate() {
        assert_eq!(slice_count(pkt), 1, "Packet {} slice count", i);
        assert_eq!(pkt.pts, i as i64, "Packet {} PTS", i);
        assert_eq!(pkt.dts, i as i64, "Packet {} DTS", i);
    }
    assert!(
        packets[0]
            .flags
            .contains(wedeo_core::packet::PacketFlags::KEY)
    );
}

/// SVA_Base_B.264 has three slices per frame: they must stay in one AU.
#[test]
fn fate_h264_demuxer_multi_slice_per_au() {
    let path = conformance_file("SVA_Base_B.264");
    if !path.exists() {
        return;
    }
    let packets = read_packets(&path);
    assert_eq!(packets.len(), 17, "SVA_Base_B.264 packet count");
    for (i, pkt) in packets.iter().enumerate() {
        assert_eq!(slice_count(pkt), 3, "Packet {} should hold 3 slices", i);
        assert_eq!(pkt.pts, i as i64, "Packet {} PTS", i);
        assert_eq!(pkt.dts, i as i64, "Packet {} DTS", i);
    }
}

/// AUD_MW_E.264 carries an access unit delimiter before every picture.
#[test]
fn fate_h264_demuxer_aud_delimiter() {
    let path = conformance_file("AUD_MW_E.264");
    if !path.exists() {
        return;
    }
    let packets = read_packets(&path);
    assert_eq!(packets.len(), 100, "AUD_MW_E.264 packet count");
    for (i, pkt) in packets.iter().enumerate() {
        assert_eq!(slice_count(pkt), 1, "Packet {} slice count", i);
        assert_eq!(pkt.dts, i as i64, "Packet {} DTS", i);
        assert!(
            split_annex_b(pkt.data.data())
                .iter()
                .any(|n| n.nal_type == NalUnitType::Aud),
            "Packet {} should carry its AUD NAL",
            i
        );
    }
    let mut pts_sorted: Vec<i64> = packets.iter().map(|p| p.pts).collect();
    pts_sorted.sort_unstable();
    assert_eq!(pts_sorted, (0..100).collect::<Vec<_>>(), "AUD stream PTS");
}

/// src19td.IBP.264 has B-frames: packet PTS follows display order (POC),
/// DTS follows decode order, and framecrc output matches FFmpeg including
/// the timestamps.
#[test]
fn fate_h264_demuxer_b_frame_pts() {
    let path = conformance_file("src19td.IBP.264");
    if !path.exists() {
        return;
    }
    let packets = read_packets(&path);
    assert_eq!(packets.len(), 257, "src19td.IBP.264 packet count");
    for (i, pkt) in packets.iter().enumerate() {
        assert_eq!(pkt.dts, i as i64, "Packet {} DTS", i);
    }
    assert_eq!(
        packets[..8].iter().map(|p| p.pts).collect::<Vec<_>>(),
        vec![0, 2, 1, 4, 3, 6, 5, 8],
        "B-frame packet PTS should be in POC order"
    );
    let mut pts_sorted: Vec<i64> = packets.iter().map(|p| p.pts).collect();
    pts_sorted.sort_unstable();
    assert_eq!(
        pts_sorted,
        (0..257).collect::<Vec<_>>(),
        "PTS values should form a dense display-order sequence"
    );

    let wedeo = wedeo_fate::run_wedeo_framecrc(&path).expect("wedeo-framecrc failed");
    let Some(ffmpeg) = wedeo_fate::run_ffmpeg_framecrc_video(&path) else {
        eprintln!("SKIP: ffmpeg not available for cross-validation");
        return;
    };
    assert_eq!(
        framecrc_data(&wedeo),
        framecrc_data(&ffmpeg),
        "B-frame framecrc (including PTS) must match FFmpeg"
    );
}

/// Field pictures are separate access units; complementary fields share a
/// presentation timestamp because the decoder outputs them as one frame.
#[test]
fn fate_h264_demuxer_field_picture_au() {
    let path = conformance_file("Sharp_MP_Field_1_B.jvt");
    if !path.exists() {
        return;
    }
    let packets = read_packets(&path);
    assert_eq!(packets.len(), 30, "Sharp_MP_Field_1_B.jvt packet count");
    for (i, pkt) in packets.iter().enumerate() {
        assert_eq!(pkt.dts, i as i64, "Packet {} DTS", i);
        assert_eq!(slice_count(pkt), 1, "Packet {} slice count", i);
    }
    assert_eq!(
        packets[..8].iter().map(|p| p.pts).collect::<Vec<_>>(),
        vec![0, 0, 1, 1, 2, 2, 3, 3],
        "Complementary fields should share a PTS"
    );
}

// ---------------------------------------------------------------------------
// Bitexact regression tests (framecrc parity with FFmpeg)
// ---------------------------------------------------------------------------

/// BA1_Sony_D.jsv: Baseline BITEXACT — 17 frames, 176x144, QP=28, I+P.
#[test]
fn fate_h264_ba1_sony_d_bitexact() {
    let path = conformance_file("BA1_Sony_D.jsv");
    if !path.exists() {
        return;
    }

    let wedeo = match wedeo_fate::run_wedeo_framecrc(&path) {
        Ok(lines) => lines,
        Err(e) => panic!("wedeo-framecrc failed: {e}"),
    };

    let Some(ffmpeg) = wedeo_fate::run_ffmpeg_framecrc_video(&path) else {
        eprintln!("SKIP: ffmpeg not available for cross-validation");
        return;
    };

    match wedeo_fate::compare_framecrc_lines(&wedeo, &ffmpeg) {
        Ok(()) => eprintln!("PASS: BA1_Sony_D.jsv BITEXACT"),
        Err(e) => panic!("BA1_Sony_D.jsv NOT BITEXACT:\n{e}"),
    }
}

/// SVA_BA1_B.264: Baseline BITEXACT — single-slice I-frames, 176x144.
#[test]
fn fate_h264_sva_ba1_b_bitexact() {
    let path = conformance_file("SVA_BA1_B.264");
    if !path.exists() {
        return;
    }

    let wedeo = match wedeo_fate::run_wedeo_framecrc(&path) {
        Ok(lines) => lines,
        Err(e) => panic!("wedeo-framecrc failed: {e}"),
    };

    let Some(ffmpeg) = wedeo_fate::run_ffmpeg_framecrc_video(&path) else {
        eprintln!("SKIP: ffmpeg not available for cross-validation");
        return;
    };

    match wedeo_fate::compare_framecrc_lines(&wedeo, &ffmpeg) {
        Ok(()) => eprintln!("PASS: SVA_BA1_B.264 BITEXACT"),
        Err(e) => panic!("SVA_BA1_B.264 NOT BITEXACT:\n{e}"),
    }
}

/// SVA_NL1_B.264: Baseline BITEXACT — single-slice I-frames, 176x144.
#[test]
fn fate_h264_sva_nl1_b_bitexact() {
    let path = conformance_file("SVA_NL1_B.264");
    if !path.exists() {
        return;
    }

    let wedeo = match wedeo_fate::run_wedeo_framecrc(&path) {
        Ok(lines) => lines,
        Err(e) => panic!("wedeo-framecrc failed: {e}"),
    };

    let Some(ffmpeg) = wedeo_fate::run_ffmpeg_framecrc_video(&path) else {
        eprintln!("SKIP: ffmpeg not available for cross-validation");
        return;
    };

    match wedeo_fate::compare_framecrc_lines(&wedeo, &ffmpeg) {
        Ok(()) => eprintln!("PASS: SVA_NL1_B.264 BITEXACT"),
        Err(e) => panic!("SVA_NL1_B.264 NOT BITEXACT:\n{e}"),
    }
}

/// BAMQ1_JVC_C.264: Baseline BITEXACT — I-only with per-MB QP variation, 176x144.
/// 30 frames, all I-frames. The per-MB QP delta exercises the full dequant/IDCT
/// path at various quantization levels within each frame.
#[test]
fn fate_h264_bamq1_jvc_c_bitexact() {
    let path = conformance_file("BAMQ1_JVC_C.264");
    if !path.exists() {
        return;
    }

    let wedeo = match wedeo_fate::run_wedeo_framecrc(&path) {
        Ok(lines) => lines,
        Err(e) => panic!("wedeo-framecrc failed: {e}"),
    };

    let Some(ffmpeg) = wedeo_fate::run_ffmpeg_framecrc_video(&path) else {
        eprintln!("SKIP: ffmpeg not available for cross-validation");
        return;
    };

    match wedeo_fate::compare_framecrc_lines(&wedeo, &ffmpeg) {
        Ok(()) => eprintln!("PASS: BAMQ1_JVC_C.264 BITEXACT"),
        Err(e) => panic!("BAMQ1_JVC_C.264 NOT BITEXACT:\n{e}"),
    }
}

// ---------------------------------------------------------------------------
// P-frame decode regression tests
// ---------------------------------------------------------------------------

/// BA_MW_D.264: Baseline I+P, 100 frames. All 100 frames should decode.
/// P-frame decode works after DPB reference marking and CAVLC ref count fixes.
#[test]
fn fate_h264_ba_mw_d_frame_count() {
    let path = conformance_file("BA_MW_D.264");
    if !path.exists() {
        return;
    }

    let frames = decode_all_frames(&path);
    assert_eq!(frames.len(), 100, "BA_MW_D.264 should decode 100 frames");
    for (i, &(w, h, pf, _)) in frames.iter().enumerate() {
        assert_eq!(w, 176, "Frame {} width", i);
        assert_eq!(h, 144, "Frame {} height", i);
        assert_eq!(pf, PixelFormat::Yuv420p, "Frame {} pixel format", i);
    }
}

/// SVA_Base_B.264: Multi-slice Baseline I+P, 17 frames.
/// Previously panicked with OOB at fill_mb_gray; now decodes all 17 frames.
#[test]
fn fate_h264_sva_base_b_frame_count() {
    let path = conformance_file("SVA_Base_B.264");
    if !path.exists() {
        return;
    }

    let frames = decode_all_frames(&path);
    assert_eq!(frames.len(), 17, "SVA_Base_B.264 should decode 17 frames");
}

/// SVA_FM1_E.264: Multi-slice Baseline, 17 frames.
/// Previously panicked; now decodes all 17 frames.
#[test]
fn fate_h264_sva_fm1_e_frame_count() {
    let path = conformance_file("SVA_FM1_E.264");
    if !path.exists() {
        return;
    }

    let frames = decode_all_frames(&path);
    assert_eq!(frames.len(), 17, "SVA_FM1_E.264 should decode 17 frames");
}

/// BA1_FT_C.264: Baseline 352x288, 299 frames.
/// Previously decoded only 26/299; now all 299 frames.
#[test]
fn fate_h264_ba1_ft_c_frame_count() {
    let path = conformance_file("BA1_FT_C.264");
    if !path.exists() {
        return;
    }

    let frames = decode_all_frames(&path);
    assert_eq!(frames.len(), 299, "BA1_FT_C.264 should decode 299 frames");
    for (i, &(w, h, _, _)) in frames.iter().enumerate() {
        assert_eq!(w, 352, "Frame {} width", i);
        assert_eq!(h, 288, "Frame {} height", i);
    }
}

// ---------------------------------------------------------------------------
// Demuxer tests
// ---------------------------------------------------------------------------

/// Test that the demuxer produces correct packet count for a multi-slice file.
/// SVA_Base_B.264 has 3 slices per frame.
#[test]
fn fate_h264_demuxer_multi_slice() {
    let path = conformance_file("SVA_Base_B.264");
    if !path.exists() {
        return;
    }

    let mut input = InputContext::open(path.to_str().unwrap()).unwrap();

    let mut packet_count = 0;
    loop {
        match input.read_packet() {
            Ok(_pkt) => {
                packet_count += 1;
            }
            Err(Error::Eof) => break,
            Err(e) => panic!("read_packet error: {:?}", e),
        }
    }

    eprintln!(
        "SVA_Base_B.264: {} packets from demuxer (multi-slice)",
        packet_count
    );
    // This file has multiple slices per frame; the demuxer should group
    // them into per-frame AUs. The exact count depends on the file structure.
    assert!(
        packet_count > 0,
        "Should produce at least one packet for SVA_Base_B.264"
    );
}

// ---------------------------------------------------------------------------
// FRext typed-pixel support (High 10 / High 4:2:2)
// ---------------------------------------------------------------------------

/// Locate a JVT FRext vector (read-only conformance asset).
fn jvt_frext_file(name: &str) -> PathBuf {
    if let Ok(dir) = std::env::var("JVT_VECTORS") {
        let path = PathBuf::from(dir).join("JVT-FR-EXT").join(name);
        if path.exists() {
            return path;
        }
    }
    PathBuf::new()
}

/// Decode the first frame and report its format, plane linesizes and sizes.
fn decode_first_frame_layout(path: &Path) -> Option<(PixelFormat, Vec<usize>, Vec<usize>)> {
    if !path.exists() {
        return None;
    }
    let mut input = InputContext::open(path.to_str().unwrap()).unwrap();
    let params = input.streams[0].codec_params.clone();
    let factory = registry::find_decoder(params.codec_id).unwrap();
    let mut decoder = factory.create(params).unwrap();

    loop {
        match input.read_packet() {
            Ok(pkt) => {
                decoder.send_packet(Some(&pkt)).unwrap();
                loop {
                    match decoder.receive_frame() {
                        Ok(frame) => {
                            if let FrameData::Video(v) = frame.data {
                                return Some((
                                    v.format,
                                    v.planes.iter().map(|p| p.linesize).collect(),
                                    v.planes.iter().map(|p| p.buffer.size()).collect(),
                                ));
                            }
                        }
                        Err(Error::Again) => break,
                        Err(e) => panic!("receive_frame error: {:?}", e),
                    }
                }
            }
            Err(Error::Eof) => break,
            Err(e) => panic!("read_packet error: {:?}", e),
        }
    }
    None
}

#[test]
fn pixel_format_yuv422p10le_matches_ffmpeg_value() {
    // FFmpeg libavutil/pixfmt.h: AV_PIX_FMT_YUV422P10LE = 71.
    assert_eq!(PixelFormat::Yuv422p10le as i32, 71);
    assert_eq!(PixelFormat::Yuv422p10le.name(), "yuv422p10le");
}

/// High 4:2:2, 10-bit: output must be yuv422p10le with 2 bytes/sample,
/// full chroma height and half chroma width.
#[test]
fn fate_h264_high422_10bit_frame_layout() {
    let path = jvt_frext_file("Hi422FR13_SONY_B.264");
    let Some((format, linesizes, sizes)) = decode_first_frame_layout(&path) else {
        return; // vector not present
    };
    assert_eq!(format, PixelFormat::Yuv422p10le);
    assert_eq!(linesizes.len(), 3);
    // Read dimensions from the first frame again through the helper's sizes:
    // luma plane = w*h*2, chroma plane = (w/2)*h*2.
    assert!(
        sizes[0] >= sizes[1] * 2,
        "4:2:2 chroma must be half-width: {sizes:?}"
    );
    assert_eq!(linesizes[1], linesizes[0] / 2);
    assert_eq!(linesizes[2], linesizes[0] / 2);
}

/// High 4:2:2, 8-bit: output must be yuv422p with full chroma height.
#[test]
fn fate_h264_high422_8bit_frame_layout() {
    let path = jvt_frext_file("FREXT1_TANDBERG_A.264");
    let Some((format, linesizes, _)) = decode_first_frame_layout(&path) else {
        return; // vector not present
    };
    assert_eq!(format, PixelFormat::Yuv422p);
    assert_eq!(linesizes.len(), 3);
    assert_eq!(linesizes[1], linesizes[0] / 2);
    assert_eq!(linesizes[2], linesizes[0] / 2);
}

/// High 10 (4:2:0) with inter frames: the decoder must run the full-precision
/// (`u16`) reconstruction path for skip/inter macroblocks without panicking,
/// and every output frame must keep the 10-bit geometry (2 bytes/sample,
/// half-resolution chroma). This exercises `mc_luma_u16` / `mc_chroma_u16` and
/// `reconstruct_inter_mb_hires` end-to-end.
#[test]
fn fate_h264_high10_inter_full_precision_decode() {
    let dir = match std::env::var("FATE_SUITE") {
        Ok(d) => PathBuf::from(d),
        Err(_) => return,
    };
    let path = dir.join("h264-high-depth").join("normal-10.h264");
    if !path.exists() {
        return;
    }

    let mut input = InputContext::open(path.to_str().unwrap()).unwrap();
    let params = input.streams[0].codec_params.clone();
    let factory = registry::find_decoder(params.codec_id).unwrap();
    let mut decoder = factory.create(params).unwrap();

    let mut frames = 0usize;
    loop {
        match input.read_packet() {
            Ok(pkt) => {
                decoder.send_packet(Some(&pkt)).unwrap();
                loop {
                    match decoder.receive_frame() {
                        Ok(frame) => {
                            if let FrameData::Video(v) = frame.data {
                                assert_eq!(v.format, PixelFormat::Yuv420p10le);
                                assert_eq!(v.width, 352);
                                assert_eq!(v.height, 288);
                                assert_eq!(v.planes.len(), 3);
                                for plane in &v.planes {
                                    assert_eq!(plane.buffer.size() % 2, 0);
                                }
                                assert_eq!(
                                    v.planes[1].buffer.size(),
                                    v.planes[0].buffer.size() / 4
                                );
                                assert_eq!(
                                    v.planes[2].buffer.size(),
                                    v.planes[0].buffer.size() / 4
                                );
                            }
                            frames += 1;
                        }
                        Err(Error::Again) => break,
                        Err(e) => panic!("receive_frame error: {:?}", e),
                    }
                }
            }
            Err(Error::Eof) => {
                let _ = decoder.send_packet(None);
                loop {
                    match decoder.receive_frame() {
                        Ok(frame) => {
                            if let FrameData::Video(v) = frame.data {
                                assert_eq!(v.format, PixelFormat::Yuv420p10le);
                                frames += 1;
                            }
                        }
                        Err(Error::Again) | Err(Error::Eof) => break,
                        Err(e) => panic!("receive_frame error: {:?}", e),
                    }
                }
                break;
            }
            Err(e) => panic!("read_packet error: {:?}", e),
        }
    }
    assert_eq!(frames, 10, "normal-10.h264 must decode all 10 frames");
}

/// Bitexact raw-YUV parity target for the High 10 inter path. Currently blocked
/// by high-depth dequant coefficient overflow (`dequant.rs`) and intra residual
/// divergence outside this work unit; kept ignored so the targeted parity can
/// be flipped on once those land. Enable with
/// `cargo test ... fate_h264_high10_inter_bitexact -- --ignored`.
#[test]
#[ignore = "blocked by high-depth dequant/intra fixes in other files"]
fn fate_h264_high10_inter_bitexact() {
    let dir = match std::env::var("FATE_SUITE") {
        Ok(d) => PathBuf::from(d),
        Err(_) => return,
    };
    let path = dir.join("h264-high-depth").join("normal-10.h264");
    if !path.exists() {
        return;
    }
    let wedeo = std::env::var("CARGO_BIN_EXE_wedeo-framecrc")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("wedeo-framecrc"));
    let output = std::process::Command::new(wedeo)
        .arg(&path)
        .output()
        .expect("run wedeo-framecrc");
    let wedeo_out = String::from_utf8_lossy(&output.stdout);
    let wedeo_crcs: Vec<String> = wedeo_out
        .lines()
        .filter(|l| !l.starts_with('#'))
        .map(|l| l.rsplit(',').next().unwrap().trim().to_string())
        .collect();
    let ffmpeg = std::process::Command::new("ffmpeg")
        .args(["-v", "error", "-bitexact", "-i"])
        .arg(&path)
        .args(["-f", "framecrc", "-"])
        .output()
        .expect("run ffmpeg");
    let ffmpeg_out = String::from_utf8_lossy(&ffmpeg.stdout);
    let ffmpeg_crcs: Vec<String> = ffmpeg_out
        .lines()
        .filter(|l| !l.starts_with('#'))
        .map(|l| l.rsplit(',').next().unwrap().trim().to_string())
        .collect();
    assert!(!wedeo_crcs.is_empty());
    assert_eq!(wedeo_crcs, ffmpeg_crcs);
}
