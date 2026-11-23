//! FATE registration and conformance tests for `wedeo-bsf-aac-adtstoasc`.
//!
//! The expected AudioSpecificConfig bytes below are ground truth from
//! FFmpeg's own bitstream filter, e.g.:
//!
//! ```text
//! ffmpeg -f lavfi -i 'sine=frequency=440:sample_rate=44100:duration=0.1' \
//!        -ac 2 -c:a aac -b:a 64k -f adts gt_lc.aac
//! ffmpeg -i gt_lc.aac -c:a copy -bsf:a aac_adtstoasc gt_lc.m4a
//! ffprobe -show_data -show_streams gt_lc.m4a   # extradata = 12 10
//! ```

// Ensure the inventory registration is linked into this test binary.
use wedeo_bsf_aac_adtstoasc as _;
use wedeo_bsf_aac_adtstoasc::AacAdtstoAsc;

use wedeo_codec::bitstream_filter::{BitstreamFilter, bitstream_filters, find_bitstream_filter};
use wedeo_core::codec_id::CodecId;
use wedeo_core::packet::{Packet, PacketSideDataType};

/// Build a 7-byte ADTS header without CRC from the fields an ADTS parser reads.
fn adts_header(profile: u32, sr_index: u32, chan_config: u32, payload_len: usize) -> Vec<u8> {
    let mut bits = String::from("111111111111"); // syncword
    bits.push('0'); // id: MPEG-4
    bits.push_str("00"); // layer
    bits.push('1'); // protection_absent
    bits.push_str(&format!("{profile:02b}"));
    bits.push_str(&format!("{sr_index:04b}"));
    bits.push('0'); // private_bit
    bits.push_str(&format!("{chan_config:03b}"));
    bits.push_str("0000"); // original/copy, home, copyright bits
    let frame_len = 7 + payload_len;
    bits.push_str(&format!("{frame_len:013b}"));
    bits.push_str("11111111111"); // adts_buffer_fullness
    bits.push_str("00"); // one raw data block

    assert_eq!(bits.len(), 56);
    (0..7)
        .map(|i| u8::from_str_radix(&bits[i * 8..i * 8 + 8], 2).unwrap())
        .collect()
}

fn adts_packet(profile: u32, sr_index: u32, chan_config: u32, payload: &[u8]) -> Packet {
    let mut data = adts_header(profile, sr_index, chan_config, payload.len());
    data.extend_from_slice(payload);
    Packet::from_slice(&data)
}

fn asc(pkts: &[Packet]) -> Option<Vec<u8>> {
    pkts.iter()
        .flat_map(|p| p.side_data.iter())
        .find(|s| s.data_type == PacketSideDataType::NewExtradata)
        .map(|s| s.data.clone())
}

#[test]
fn registered_by_name() {
    let factory = find_bitstream_filter("aac_adtstoasc")
        .expect("aac_adtstoasc must be registered in the bitstream filter registry");
    assert_eq!(factory.descriptor().name, "aac_adtstoasc");
    assert_eq!(
        factory.descriptor().long_name,
        "MPEG-2/4 AAC ADTS to MPEG-4 Audio Specific Configuration bitstream filter"
    );
    assert_eq!(factory.descriptor().codec_id, CodecId::Aac);
    assert_eq!(factory.descriptor().priority, 100);
}

#[test]
fn present_in_registry_iteration() {
    assert!(bitstream_filters().any(|f| f.descriptor().name == "aac_adtstoasc"));
}

#[test]
fn factory_creates_working_filter() {
    let factory = find_bitstream_filter("aac_adtstoasc").unwrap();
    let mut filter = factory.create().unwrap();

    let payload = [0x21, 0x10, 0x00, 0x00];
    let out = filter.filter(&adts_packet(1, 4, 2, &payload)).unwrap();
    assert_eq!(out.len(), 1);
    assert_eq!(asc(&out), Some(vec![0x12, 0x10]));
    assert_eq!(out[0].data.data(), payload.as_slice());
    assert!(filter.flush().unwrap().is_empty());
}

/// Compare generated ASCs against the output of `ffmpeg -bsf:a aac_adtstoasc`
/// for real ADTS encodes at several sample rates and channel layouts.
#[test]
fn asc_matches_ffmpeg_ground_truth() {
    // (profile, sampling_frequency_index, channel_configuration, ASC bytes)
    let cases: &[(u32, u32, u32, [u8; 2])] = &[
        // ffmpeg -i gt_lc.aac  -c:a copy -bsf:a aac_adtstoasc ... -> 12 10
        (1, 4, 2, [0x12, 0x10]), // AAC LC, 44100 Hz, stereo
        // ffmpeg -i gt_sr22050_2.aac ... -> 13 90
        (1, 7, 2, [0x13, 0x90]), // AAC LC, 22050 Hz, stereo
        // ffmpeg -i gt_sr48000_6.aac ... -> 11 b0
        (1, 3, 6, [0x11, 0xb0]), // AAC LC, 48000 Hz, 5.1
        // ffmpeg -i gt_sr32000_1.aac ... -> 12 88
        (1, 5, 1, [0x12, 0x88]), // AAC LC, 32000 Hz, mono
    ];

    for &(profile, sr, ch, expected) in cases {
        let mut filter = AacAdtstoAsc::new();
        let out = filter
            .filter(&adts_packet(profile, sr, ch, &[0x21, 0x10]))
            .unwrap();
        assert_eq!(
            asc(&out),
            Some(expected.to_vec()),
            "profile={profile} sr={sr} ch={ch}"
        );
    }
}

#[test]
fn only_first_packet_carries_new_extradata() {
    let mut filter = AacAdtstoAsc::new();
    let pkt = adts_packet(1, 4, 2, &[0x21, 0x10]);
    assert_eq!(asc(&filter.filter(&pkt).unwrap()), Some(vec![0x12, 0x10]));
    assert_eq!(asc(&filter.filter(&pkt).unwrap()), None);
}

#[test]
fn adts_header_is_removed_and_packet_properties_preserved() {
    let mut pkt = adts_packet(1, 4, 2, &[0xaa, 0xbb, 0xcc]);
    pkt.pts = 900;
    pkt.dts = 800;
    pkt.duration = 1024;
    pkt.stream_index = 3;

    let out = AacAdtstoAsc::new().filter(&pkt).unwrap();
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].data.data(), &[0xaa, 0xbb, 0xcc]);
    assert_eq!(out[0].pts, 900);
    assert_eq!(out[0].dts, 800);
    assert_eq!(out[0].duration, 1024);
    assert_eq!(out[0].stream_index, 3);
}
