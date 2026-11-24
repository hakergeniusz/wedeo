//! Bitexact comparison against FFmpeg's `-bsf:v h264_mp4toannexb`.
//!
//! Fixture generation (system ffmpeg n9.1, libx264):
//!
//! ```sh
//! ffmpeg -f lavfi -i 'testsrc2=size=64x64:rate=10' -frames:v 6 \
//!   -c:v libx264 -preset ultrafast -pix_fmt yuv420p \
//!   -x264-params 'keyint=3:min-keyint=3:scenecut=0:open-gop=0' \
//!   -f h264 src.h264
//! ffmpeg -i src.h264 -c:v copy avcc.mp4
//! ffmpeg -i avcc.mp4 -map 0:v:0 -c:v copy \
//!   -bsf:v h264_mp4toannexb -f h264 expected_annexb.h264
//! ```
//!
//! `extradata.bin` and `packets.bin` are the `avcC` extradata and the
//! length-prefixed packets as reported by `ffprobe -show_data`. `packets.bin`
//! is a 4-byte big-endian packet count followed by, for each packet, a 4-byte
//! big-endian length and the packet bytes. `expected_annexb.bin` is FFmpeg's
//! byte stream filter output.

use wedeo_bsf_h264_mp4toannexb::H264Mp4ToAnnexb;
use wedeo_codec::bitstream_filter::BitstreamFilter;
use wedeo_core::packet::Packet;

const EXTRADATA: &[u8] = include_bytes!("data/extradata.bin");
const PACKETS: &[u8] = include_bytes!("data/packets.bin");
const EXPECTED: &[u8] = include_bytes!("data/expected_annexb.bin");

fn be32(bytes: &[u8]) -> usize {
    u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize
}

/// Run the filter over every AVCC packet in the fixture and concatenate the
/// resulting Annex B byte stream.
fn filter_fixture() -> Vec<u8> {
    let count = be32(&PACKETS[..4]);
    let mut pos = 4;
    let mut bsf = H264Mp4ToAnnexb::with_extradata(EXTRADATA).unwrap();
    let mut out = Vec::new();

    for _ in 0..count {
        let len = be32(&PACKETS[pos..pos + 4]);
        pos += 4;
        let data = &PACKETS[pos..pos + len];
        pos += len;

        let packet = Packet::from_slice(data);
        let filtered = bsf.filter(&packet).unwrap();
        assert_eq!(filtered.len(), 1, "this bsf emits one packet per input");
        out.extend_from_slice(filtered[0].data.data());
    }
    assert_eq!(pos, PACKETS.len(), "fixture packet table fully consumed");
    out
}

#[test]
fn bitexact_against_ffmpeg_h264_mp4toannexb() {
    let out = filter_fixture();
    assert_eq!(
        out.len(),
        EXPECTED.len(),
        "output length differs from FFmpeg's bsf output"
    );
    assert_eq!(
        out, EXPECTED,
        "output bytes differ from FFmpeg's -bsf:v h264_mp4toannexb"
    );
}

#[test]
fn fixture_is_nontrivial() {
    // Guard against an empty or degenerate embedded fixture.
    assert!(EXTRADATA.len() >= 7);
    assert!(be32(&PACKETS[..4]) >= 2);
    assert!(EXPECTED.len() > 100);
    assert!(EXPECTED.starts_with(&[0, 0, 0, 1, 0x67]));
}
