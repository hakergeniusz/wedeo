//! FATE-style tests for H.264 4:2:2 chroma block geometry.
//!
//! Covers the 8x16 chroma intra prediction path (FFmpeg
//! `h264pred_template.c:pred8x16_*`) and the 4:2:2 chroma reconstruction
//! pipeline end to end against a bitexact JVT vector.

use wedeo_codec::registry;
use wedeo_codec_h264::intra_pred::predict_chroma_8x16;
use wedeo_core::error::Error;
use wedeo_core::frame::FrameData;
use wedeo_core::frame::FramePlane;
use wedeo_format::context::InputContext;

use adler2::Adler32;
use std::path::{Path, PathBuf};

// Ensure codec and format registrations are linked.
use wedeo_codec_h264 as _;
use wedeo_format_h264 as _;

/// Allocate an 8x16 destination with padding (stride 16).
fn dst() -> Vec<u8> {
    vec![0u8; 16 * 16]
}

fn row(dst: &[u8], y: usize) -> [u8; 8] {
    let mut out = [0u8; 8];
    out.copy_from_slice(&dst[y * 16..y * 16 + 8]);
    out
}

#[test]
fn chroma_8x16_dc_both_edges() {
    // FFmpeg pred8x16_dc: four 4-row bands, each split at column 4.
    let mut d = dst();
    let top = [124u8; 8];
    let left = [124u8; 16];
    predict_chroma_8x16(&mut d, 16, 0, &top, &left, 124, true, true);
    for y in 0..16 {
        assert_eq!(row(&d, y), [124; 8], "row {y}");
    }
}

#[test]
fn chroma_8x16_dc_top_only_uses_top_dc() {
    // With no left edge FFmpeg remaps DC to TOP_DC_PRED8x8: both column
    // halves are filled from the top samples for all 16 rows (previously
    // wedeo filled the lower bands with mid-gray).
    let mut d = dst();
    let top = [100, 100, 100, 100, 200, 200, 200, 200];
    let left = [0u8; 16];
    predict_chroma_8x16(&mut d, 16, 0, &top, &left, 100, true, false);
    let expected = [100, 100, 100, 100, 200, 200, 200, 200];
    for y in 0..16 {
        assert_eq!(row(&d, y), expected, "row {y}");
    }
}

#[test]
fn chroma_8x16_dc_left_only_uses_four_row_bands() {
    // With no top edge FFmpeg remaps DC to LEFT_DC_PRED8x8: four row bands
    // of four rows, each filled across the whole width.
    let mut d = dst();
    let top = [0u8; 8];
    let left = [
        100, 100, 100, 100, // rows 0..3
        120, 120, 120, 120, // rows 4..7
        140, 140, 140, 140, // rows 8..11
        160, 160, 160, 160, // rows 12..15
    ];
    predict_chroma_8x16(&mut d, 16, 0, &top, &left, 100, false, true);
    for y in 0..16 {
        let value = 100 + 20 * (y / 4) as u8;
        assert_eq!(row(&d, y), [value; 8], "row {y}");
    }
}

#[test]
fn chroma_8x16_dc_no_edges_is_128() {
    let mut d = vec![255u8; 16 * 16];
    let top = [0u8; 8];
    let left = [0u8; 16];
    predict_chroma_8x16(&mut d, 16, 0, &top, &left, 0, false, false);
    for y in 0..16 {
        assert_eq!(row(&d, y), [128; 8], "row {y}");
    }
}

#[test]
fn chroma_8x16_plane_matches_ffmpeg_formula() {
    // FFmpeg pred8x16_plane starts V at left[8] - left[6] (not left[7] -
    // left[5]). Compare against a direct transcription of
    // h264pred_template.c:781-820.
    let top: [u8; 8] = [100, 101, 102, 103, 104, 105, 106, 107];
    let left: [u8; 16] = [
        110, 113, 117, 120, 124, 128, 132, 136, 140, 144, 149, 153, 157, 161, 166, 170,
    ];
    let top_left = 108u8;

    let tl = top_left as i32;
    let mut h = top[4] as i32 - top[2] as i32;
    let mut v = left[8] as i32 - left[6] as i32;
    for k in 2..=4usize {
        let above = if k <= 3 { top[3 - k] as i32 } else { tl };
        h += k as i32 * (top[3 + k] as i32 - above);
        v += k as i32 * (left[7 + k] as i32 - left[7 - k] as i32);
    }
    for k in 5..=8usize {
        let above = if k <= 7 { left[7 - k] as i32 } else { tl };
        v += k as i32 * (left[7 + k] as i32 - above);
    }
    let h = (17 * h + 16) >> 5;
    let v = (5 * v + 32) >> 6;
    let mut a = 16 * (left[15] as i32 + top[7] as i32 + 1) - 7 * v - 3 * h;
    let mut expected = [[0u8; 8]; 16];
    for y in 0..16 {
        let mut b = a;
        a += v;
        for x in 0..8 {
            expected[y][x] = (b >> 5).clamp(0, 255) as u8;
            b += h;
        }
    }

    let mut d = dst();
    predict_chroma_8x16(&mut d, 16, 3, &top, &left, top_left, true, true);
    for y in 0..16 {
        assert_eq!(row(&d, y), expected[y], "row {y}");
    }
}

/// Pack one plane row-by-row, matching FFmpeg rawvideo / framecrc layout.
fn pack_plane(plane: &FramePlane, width: usize, height: usize) -> Vec<u8> {
    let data = plane.buffer.data();
    let mut out = Vec::with_capacity(width * height);
    for y in 0..height {
        let start = plane.offset + y * plane.linesize;
        out.extend_from_slice(&data[start..start + width]);
    }
    out
}

/// Non-standard Adler-32 used by FFmpeg framecrc (`s1 = s2 = 0`).
fn framecrc_crc(raw: &[u8]) -> u32 {
    let mut hasher = Adler32::from_checksum(0);
    hasher.write_slice(raw);
    hasher.checksum()
}

/// Locate the JVT 4:2:2 vector (read-only conformance asset).
fn jvt_vector(name: &str) -> PathBuf {
    if let Ok(dir) = std::env::var("JVT_VECTORS") {
        let path = PathBuf::from(dir).join("JVT-FR-EXT").join(name);
        if path.exists() {
            return path;
        }
    }
    PathBuf::new()
}

fn decode_crcs(path: &Path) -> Vec<u32> {
    let mut input = match InputContext::open(path.to_str().unwrap()) {
        Ok(input) => input,
        Err(_) => return Vec::new(),
    };
    let params = input.streams[0].codec_params.clone();
    let factory = match registry::find_decoder(params.codec_id) {
        Some(factory) => factory,
        None => return Vec::new(),
    };
    let mut decoder = match factory.create(params) {
        Ok(decoder) => decoder,
        Err(_) => return Vec::new(),
    };

    let mut crcs = Vec::new();
    loop {
        let packet = match input.read_packet() {
            Ok(pkt) => Some(pkt),
            Err(Error::Eof) => None,
            Err(_) => break,
        };
        if decoder.send_packet(packet.as_ref()).is_err() {
            break;
        }
        loop {
            match decoder.receive_frame() {
                Ok(frame) => {
                    if let FrameData::Video(video) = frame.data {
                        let width = video.width as usize;
                        let height = video.height as usize;
                        let cw = width / 2; // yuv422p: log2_chroma_w = 1
                        let ch = height; // yuv422p: log2_chroma_h = 0
                        let mut raw = pack_plane(&video.planes[0], width, height);
                        raw.extend_from_slice(&pack_plane(&video.planes[1], cw, ch));
                        raw.extend_from_slice(&pack_plane(&video.planes[2], cw, ch));
                        crcs.push(framecrc_crc(&raw));
                    }
                }
                Err(Error::Again) => break,
                Err(_) => break,
            }
        }
        if packet.is_none() {
            break;
        }
    }
    crcs
}

#[test]
fn frext1_422_vector_is_bitexact() {
    // FFmpeg `ffmpeg -bitexact -i FREXT1_TANDBERG_A.264 -f framecrc -`
    // produces these five CRCs. The 4:2:2 geometry path (intra prediction,
    // chroma AC QP, and quarter-pel vertical chroma MC) must preserve them.
    let path = jvt_vector("FREXT1_TANDBERG_A.264");
    if !path.exists() {
        eprintln!("SKIP: JVT FRext vector not found");
        return;
    }
    let expected = [
        0x399b_f17e,
        0xf07b_c6ff,
        0x577d_9189,
        0x0ab8_f54b,
        0xf6bb_7e00,
    ];
    let crcs = decode_crcs(&path);
    assert_eq!(crcs.len(), expected.len(), "frame count");
    for (i, (&got, &want)) in crcs.iter().zip(expected.iter()).enumerate() {
        assert_eq!(got, want, "frame {i} CRC mismatch");
    }
}
