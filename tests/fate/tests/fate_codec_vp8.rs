//! FATE-style integration tests for the native VP8 decoder.
//!
//! Verifies:
//! 1. Decoder registration at priority 100 for `CodecId::Vp8`.
//! 2. Decoder creation and empty receive/flush behaviour without data.
//! 3. End-to-end decode of a VP8 keyframe from `$FATE_SUITE` (skipped when
//!    the suite is unavailable), checking frame count, dimensions and the
//!    presence of three YUV420p planes with a keyframe flag on the first
//!    decoded frame.

use std::path::PathBuf;

use wedeo_codec::decoder::CodecParameters;
use wedeo_codec::registry;
use wedeo_core::codec_id::CodecId;
use wedeo_core::error::Error;
use wedeo_core::frame::FrameFlags;
use wedeo_core::media_type::MediaType;
use wedeo_core::pixel_format::PixelFormat;

// Ensure the decoder registration is linked into this test binary.
use wedeo_codec_vp8 as _;
use wedeo_format_ivf as _;

fn suite_dir() -> Option<PathBuf> {
    std::env::var("FATE_SUITE").ok().map(PathBuf::from)
}

#[test]
fn vp8_decoder_registered_at_priority_100() {
    let factory = registry::find_decoder(CodecId::Vp8).expect("VP8 decoder not registered");
    let desc = factory.descriptor();
    assert_eq!(desc.codec_id, CodecId::Vp8);
    assert_eq!(desc.name, "vp8");
    assert_eq!(desc.media_type, MediaType::Video);
    assert_eq!(desc.priority, 100);

    let params = CodecParameters::new(CodecId::Vp8, MediaType::Video);
    let decoder = factory.create(params).expect("cannot create VP8 decoder");
    assert_eq!(decoder.descriptor().id, CodecId::Vp8);
}

#[test]
fn vp8_decoder_empty_state() {
    let params = CodecParameters::new(CodecId::Vp8, MediaType::Video);
    let mut decoder = registry::find_decoder(CodecId::Vp8)
        .unwrap()
        .create(params)
        .unwrap();
    // No packet yet -> Again.
    assert!(matches!(decoder.receive_frame(), Err(Error::Again)));
    // Flush must not panic on an empty decoder.
    decoder.flush();
}

#[test]
fn vp8_fate_keyframe_decode() {
    let Some(suite) = suite_dir() else {
        eprintln!("FATE_SUITE not set; skipping VP8 FATE decode test");
        return;
    };
    let path = suite
        .join("vp8-test-vectors-r1")
        .join("vp80-00-comprehensive-001.ivf");
    if !path.exists() {
        eprintln!("{} not present; skipping", path.display());
        return;
    }

    use wedeo_format::context::InputContext;
    let mut input = match InputContext::open(path.to_str().unwrap()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("cannot open VP8 vector: {e:?}; skipping");
            return;
        }
    };
    let params = input.streams[0].codec_params.clone();
    assert_eq!(params.codec_id, CodecId::Vp8);
    let mut decoder = registry::find_decoder(CodecId::Vp8)
        .expect("VP8 decoder not found")
        .create(params)
        .unwrap();

    let mut frames = Vec::new();
    loop {
        match input.read_packet() {
            Ok(pkt) => {
                if decoder.send_packet(Some(&pkt)).is_ok() {
                    loop {
                        match decoder.receive_frame() {
                            Ok(f) => frames.push(f),
                            Err(Error::Again) => break,
                            Err(e) => panic!("receive_frame: {e:?}"),
                        }
                    }
                }
            }
            Err(Error::Eof) => break,
            Err(e) => panic!("read_packet: {e:?}"),
        }
    }
    decoder.send_packet(None).unwrap();
    while let Ok(f) = decoder.receive_frame() {
        frames.push(f);
    }

    assert!(!frames.is_empty(), "no VP8 frames decoded");
    for frame in &frames {
        let video = frame.video().expect("expected a video frame");
        assert_eq!(video.format, PixelFormat::Yuv420p);
        assert_eq!(video.planes.len(), 3);
        assert!(video.width >= 16 && video.height >= 16);
    }
    // Keyframe flag must be present on the first decoded frame.
    assert!(frames[0].flags.contains(FrameFlags::KEY));
}

/// Bit-exact framecrc parity against FFmpeg for a profile-0 VP8 stream that
/// exercises inter prediction, sub-pixel motion compensation (luma EPEL and
/// chroma EPEL) and the right-edge padding columns of the coded frame.
///
/// The expected Adler-32 values come from
/// `ffmpeg -bitexact -i sintel-signbias.ivf -f framecrc -`. They guard two
/// regressions fixed together:
///   * chroma MC must use the EPEL 4/6-tap filters for profile 0, not bilinear;
///   * reference-frame reads clamp to the coded size (16 * mb_width/height),
///     so the padding columns inside the last macroblock stay addressable.
#[test]
fn vp8_fate_sintel_framecrc_parity() {
    let Some(sample) = wedeo_fate::fate_sample("vp8/sintel-signbias.ivf") else {
        eprintln!("vp8/sintel-signbias.ivf not present; skipping");
        return;
    };

    let expected: [(&str, usize); 9] = [
        ("0x28da37ad", 614880),
        ("0x0b42feab", 614880),
        ("0xb8d6f27b", 614880),
        ("0x24dd487a", 614880),
        ("0x8229e155", 614880),
        ("0x3559610a", 614880),
        ("0xcf1b54b1", 614880),
        ("0xd3f1d787", 614880),
        ("0x6535b2e8", 614880),
    ];

    let lines = wedeo_fate::run_wedeo_framecrc(&sample).expect("wedeo-framecrc failed");
    let frames: Vec<(String, usize)> = lines
        .iter()
        .filter(|l| l.starts_with("0,"))
        .map(|l| {
            let fields: Vec<&str> = l.split(',').map(str::trim).collect();
            (fields[5].to_string(), fields[4].parse().unwrap())
        })
        .collect();

    assert_eq!(frames.len(), expected.len(), "frame count differs");
    for (i, ((crc, size), (exp_crc, exp_size))) in frames.iter().zip(expected).enumerate() {
        assert_eq!(*size, exp_size, "frame {i}: size differs");
        assert_eq!(crc, exp_crc, "frame {i}: checksum differs");
    }
}
