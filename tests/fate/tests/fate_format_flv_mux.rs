//! Conformance tests for `wedeo-format-flv-mux`.
//!
//! Registration plus bitexact re-mux comparisons against FLV files produced
//! by `ffmpeg -f flv` (ffmpeg n9.1). The reference files are embedded as hex:
//!
//!   ffmpeg -hide_banner -loglevel error -f lavfi \
//!       -i testsrc2=s=64x48:r=5 -frames:v 2 \
//!       -c:v libx264 -bitexact -preset ultrafast -bf 0 -pix_fmt yuv420p \
//!       -an -f flv -y ref_h264.flv
//!
//!   ffmpeg -hide_banner -loglevel error -f lavfi \
//!       -i sine=frequency=440:sample_rate=44100 -t 0.1 \
//!       -c:a aac -bitexact -b:a 32k -f flv -y ref_aac.flv
//!
//! Each test parses the reference tags, feeds the exact payload bytes back
//! through the Wedeo muxer with the same stream parameters, and asserts that
//! the resulting file is byte-for-byte identical. This checks the FLV header,
//! onMetaData AMF encoding (including trailer patches), AVC/AAC sequence
//! headers, per-tag timestamps/frametypes, previous-tag-size fields and the
//! AVC end-of-sequence tag.

// Link the registry entry into the test binary.
use wedeo_format_flv_mux as _;

use wedeo_codec::decoder::CodecParameters;
use wedeo_core::channel_layout::ChannelLayout;
use wedeo_core::codec_id::CodecId;
use wedeo_core::media_type::MediaType;
use wedeo_core::packet::{Packet, PacketFlags};
use wedeo_core::rational::Rational;
use wedeo_format::context::OutputContext;
use wedeo_format::demuxer::Stream;
use wedeo_format::registry::{find_muxer_by_name, guess_muxer_by_extension, muxers};

const FFMPEG_H264_HEX: &str = concat!(
    "464c56010100000009000000001200009f0000000000000002000a6f6e4d6574614461746108000000070008647572617469",
    "6f6e003fd999999999999a000577696474680040500000000000000006686569676874004048000000000000000d76696465",
    "6f646174617261746500000000000000000000096672616d6572617465004014000000000000000c766964656f636f646563",
    "696400401c000000000000000866696c6573697a650040a3a40000000000000009000000aa0900002a000000000000001700",
    "0000000142c00affe100166742c00ada11ec044000000300400000030283c489a801000468ce0fc800000035090008480000",
    "00000000001701000000000002530605ffff4fdc45e9bde6d948b7962cd820d923eeef78323634202d20636f726520313635",
    "2072333232322062333536303561202d20482e3236342f4d5045472d342041564320636f646563202d20436f70796c656674",
    "20323030332d32303235202d20687474703a2f2f7777772e766964656f6c616e2e6f72672f783236342e68746d6c202d206f",
    "7074696f6e733a2063616261633d30207265663d31206465626c6f636b3d303a303a3020616e616c7973653d303a30206d65",
    "3d646961207375626d653d30207073793d31207073795f72643d312e30303a302e3030206d697865645f7265663d30206d65",
    "5f72616e67653d3136206368726f6d615f6d653d31207472656c6c69733d30203878386463743d302063716d3d3020646561",
    "647a6f6e653d32312c313120666173745f70736b69703d31206368726f6d615f71705f6f66667365743d3020746872656164",
    "733d31206c6f6f6b61686561645f746872656164733d3120736c696365645f746872656164733d30206e723d302064656369",
    "6d6174653d3120696e7465726c616365643d3020626c757261795f636f6d7061743d3020636f6e73747261696e65645f696e",
    "7472613d3020626672616d65733d3020776569676874703d30206b6579696e743d323530206b6579696e745f6d696e3d3520",
    "7363656e656375743d3020696e7472615f726566726573683d302072633d637266206d62747265653d30206372663d32332e",
    "302071636f6d703d302e36302071706d696e3d302071706d61783d3639207170737465703d342069705f726174696f3d312e",
    "34302061713d300080000005e86588843a0c60016c4211e5051b8e2632b407fbc00ad360cc636d42020432202046000e465e",
    "14d510661280945ffe1c040432380013000219d1824c14c53ccd25213e7ffb4417a29286154d40debffc380808c51904a838",
    "4a0a7ce7abc380808c03c000aa0521ead240cf5fbe511068838420afcefabd4e0970a378874a7abd20001550843958481bfb",
    "f7c900009e1d8d45b419cbf7c080004018000c071909000100504002000376328d12a517c31c5bb1b8b64007d8b1891fac60",
    "589efffd1c000950622a611a40066015cd0932ce3c6f82591a4d056b22164f55b00083499fffb58403e84ca3c1370efee0be",
    "003e1ab83adcb0fa885ee3043f600ad310ac3cdb500ac2d05205f529f72d5f880000802c03149c820202191c1010c9000020",
    "0a062a580130bd38e375e96ecd4f74a0d0e28ce2260afabd4b1e0eab53c0299f57b000f8cc2988a6835b29d3d004e19d6a0f",
    "28cd56077170eae53802bdeaf5385ce70753814f7abd00270cfe50794f556c00f1cc197d6b0e6ca68f05411c2001603875c6",
    "83b1c80e5c5f4d800e9780a2e40f6089895a204889ffdf60d4667a03d6b7efb4ecc228b5e417e2f6fe8a9f1094bebb01ba1d",
    "8b7940798effd604bd91b284b885fef831800e198147a9b92af7befae0bdc0423a08c8e8233a19930539449da4ac2d07ffb4",
    "232f0a7a883b09505a2fff0e8232380080319d0cc9829ca24ed2561683ffc1d1103640a1482e888fabd46409d0285a0b2221",
    "eaf480005d431042b090168dfbe480005d03904ab4901485fbe51103640a1482e888fabc0b8c345192e2dc6fabd20001750c",
    "410ac2405a37ef8c011cc4b9e6a0ea0802000203090001012080043a6188c088f401836a60d7104babb73031bff60000c0e8",
    "10f5a636a7a0445116c8c50cb427d7ff984835440e36a603b281c5e480eca35800048a14bba1b53c0d73a3dae2fcc7ffa175",
    "00390dda89703cfc3a3c42f0c015a6215879b6a81a8251c9e6158fe16af7f840404323820219d18b4148e6916df17c7ffb44",
    "2f452798453785f5ffe05e6128ce438a06a9eaf5384a0468be201b45eaf300239a87039441d58c03415cd45bc1c3d151f294",
    "34023457110da3f57a9c2502345f100da2f57a801f118cea682c7ad5fbf5003c2b1bd6d050f58bf7e0980040014200010000",
    "0300a1c3f4bd059d5d042f3134250f836a7a06c8729c4d40ba47c93ffe5000091c10a10a0d157083433889e35c68c9aad309",
    "0000804950070078da9e84f510c770cdab9c7f3000054479e2f701b530c3838100088000104f4c1c040463c7010118ffe380",
    "808c47010118f880004c000910000d000590840b71900029ae074205b854000a6b8220a048000a8d7074140a000151ae0631",
    "e0250146a67bc02c4400b1ffc400b11002c7860054d0a0034394ee064bb990222dc0c3888b730c9224001000100328781d17",
    "d63e3df1e2479e3df1ef8f3c79e3c48f3c7891e78f3c79e3cf1e78f0c08060008860020e3ef078a003c4079c00796fc82c03",
    "a03bf0e580740ba8f01280a07267bc02c4400b1ffc400b11002c783045880002018000201f3d159c64bb9900472ae7870239",
    "5730b3800e2e40096809c645f0408447dccdc0145a4042b4db5160405338381014c838001200029910080ac63810158f8402",
    "02381e10001901c1424008bd5d331423a07738d577841112e1680980021323206edd4818ff92320a678d84566e9100008147",
    "dc2c000f80008103017c0012482c40e840b6c8262e6a164400b10051684234f36d5961014c838405332c0200ac41c0200ac7",
    "c20002e0c183a10001200053c040145c149bf0e760400040a112c809c4b9810d91104ee5110f289273964000150080bb9859",
    "c00109d813f89e3434a2888ca71060f06641e0cce0f06641c0080399ff084000409cc0002068100012500ac0e00081026580",
    "ca000eaa082e5f6b2c404c144a580563129f0dcb5eed658004002b017c0012482c40a840b6de445ccc3088016200a2d08469",
    "e6dab2c2029907080a6658040158838040158f86007d8400020220002050010017028b8724fc42f02044796410c4b980b898",
    "a72a8ce205a78d92200020508964000080300008018011738000000853090000640000c80000000027010000000000005b41",
    "9a205af06103685cc99932520ca31ac2000101100141a4c2000300c212000788442101dce3c3522a74fbb1711e152d2263d5",
    "7a356dac52a549294000aeccd827730224e14c295f1c7554572bedda6fbfa09cbac57d7093975ab80000006f090000050000",
    "c800000000170200000000000010"
);

const FFMPEG_AAC_HEX: &str = concat!(
    "464c5601040000000900000000120000a80000000000000002000a6f6e4d6574614461746108000000070008647572617469",
    "6f6e003fbf7ced916872b0000d617564696f646174617261746500403f400000000000000f617564696f73616d706c657261",
    "74650040e5888000000000000f617564696f73616d706c6573697a65004030000000000000000673746572656f0100000c61",
    "7564696f636f6465636964004024000000000000000866696c6573697a6500408a900000000000000009000000b308000007",
    "00000000000000af00120856e500000000120800006200000000000000af01014c5418c4b1b94ef1d6bab686925a4b496922",
    "4480cececf4c97f0f2d155557cbe534d171e355444422aaa420ad55842102a210158421054200201584210ad6a02a02a4202",
    "a42102a02b5402b5842102a202b5421057556b086a856a821083800000006d0800003a00001700000000af0101508fdd88eb",
    "279e7f1d6b5c7fd1d79e35c02d05a4e43d9182282282282240446011426c25dfe10ab0d1f02afc1c5dd404042820a020a0e0",
    "000000450800005d00002e00000000af010138f42d04715e8c43a1149b07475342ed2e2e38a4e2d6884002a14a9585db18e7",
    "dad5a408ac3a2cd49866fdfde11f1f0d8f7f7858bc265f11e4f0997c476f0997c45079bc453f8812808808970a8b810bc950",
    "08858013004409b8000000680800004d00004500000000af01010a340896319e8c43a1148fb7fc3efff25ddda5aa39f3b1d9",
    "51843005d1420a824b2dbf3c06cd91d8d8896d8b2c259890675c428500ac01b0162a009da202c048580440982604ae004380",
    "000000580800006e00005d00000000af01013e342ccc5450974621d08a4e66c7dc2da8b324d1d32712e42402c336b5740905",
    "77abf2d7628672946cece8f94eb2a32c603c4516cc0f4844f888e9180f1145b303e055c40443470cecd7da897b4808548948",
    "684906d94675a3470920b23300ba428d1c33a1719827543407000000790800006a00007400000000af010136342104a1468c",
    "43a11481969a744125c9239a4e2ee4484093436494aa93dad0dd4b9aa68fef937439b3e5e7d1f0eccde8c8926daa13224a18",
    "6d8909e5a46e92221308016a2017ac984431e5a46f5922052382b2458a509da91c18e48dea84ed4046f8a50c36a700000075"
);

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

fn be24(b: &[u8], at: usize) -> u32 {
    (b[at] as u32) << 16 | (b[at + 1] as u32) << 8 | b[at + 2] as u32
}

struct RefTag {
    ty: u8,
    ts: i64,
    data: Vec<u8>,
}

fn parse_tags(bytes: &[u8]) -> Vec<RefTag> {
    assert_eq!(&bytes[0..3], b"FLV");
    let mut pos = 13usize;
    let mut tags = Vec::new();
    while pos + 11 <= bytes.len() {
        let size = be24(bytes, pos + 1) as usize;
        let ts = be24(bytes, pos + 4) as i64 | (((bytes[pos + 7] as i64) & 0x7f) << 24);
        let data = bytes[pos + 11..pos + 11 + size].to_vec();
        assert_eq!(
            u32::from_be_bytes(bytes[pos + 11 + size..pos + 15 + size].try_into().unwrap())
                as usize,
            size + 11,
            "previous-tag-size field"
        );
        tags.push(RefTag {
            ty: bytes[pos],
            ts,
            data,
        });
        pos += 15 + size;
    }
    assert_eq!(pos, bytes.len());
    tags
}

fn tmp_path(name: &str) -> String {
    std::env::temp_dir()
        .join(name)
        .to_string_lossy()
        .into_owned()
}

#[test]
fn muxer_is_registered() {
    let factory = find_muxer_by_name("flv").expect("flv muxer not registered");
    let d = factory.descriptor();
    assert_eq!(d.name, "flv");
    assert_eq!(d.long_name, "FLV (Flash Video)");
    assert_eq!(d.extensions, "flv");
    assert_eq!(d.mime_types, "video/x-flv");
    assert_eq!(d.video_codec, CodecId::H263);
    assert_eq!(d.audio_codec, CodecId::Mp3);
    assert!(factory.create().is_ok());

    assert_eq!(
        guess_muxer_by_extension("flv").map(|f| f.descriptor().name),
        Some("flv")
    );
    assert!(muxers().any(|f| f.descriptor().name == "flv"));
}

/// Re-mux the exact payload bytes of an `ffmpeg -f flv` H.264 file.
#[test]
fn bitexact_against_ffmpeg_h264() {
    let reference = unhex(FFMPEG_H264_HEX);
    let tags = parse_tags(&reference);
    assert_eq!(tags.len(), 5); // metadata, AVC sequence header, 2 frames, EOS
    assert_eq!(tags[0].ty, 0x12);
    assert_eq!(tags[1].ty, 0x09);
    assert_eq!(tags[1].data[0], 0x17); // key frame + AVC
    assert_eq!(tags[1].data[1], 0x00); // AVC sequence header
    assert_eq!(tags[4].data, [0x17, 0x02, 0x00, 0x00, 0x00]); // EOS tag

    // Sequence header carries the AVCDecoderConfigurationRecord.
    let extradata = tags[1].data[5..].to_vec();
    assert_eq!(extradata[0], 1);

    let mut par = CodecParameters::new(CodecId::H264, MediaType::Video);
    par.width = 64;
    par.height = 48;
    par.extradata = extradata;
    let mut stream = Stream::new(0, par);
    // avg_frame_rate 5/1 -> framerate 5.0 in onMetaData.
    stream.time_base = Rational::new(1, 5);
    stream.duration = 2; // 2 frames @ 1/5 s -> initial duration guess 0.4

    let mut packets = Vec::new();
    let frames = &tags[2..tags.len() - 1];
    for (i, tag) in frames.iter().enumerate() {
        let mut p = Packet::from_slice(&tag.data[5..]);
        let cts = be24(&tag.data, 2) as i32;
        p.pts = tag.ts + cts as i64;
        p.dts = tag.ts;
        // FFmpeg counts packet duration in the 1/1000 FLV time base.
        p.duration = if i + 1 < frames.len() {
            frames[i + 1].ts - tag.ts
        } else {
            tag.ts - frames[i - 1].ts
        };
        if tag.data[0] >> 4 == 1 {
            p.flags = PacketFlags::KEY;
        }
        packets.push(p);
    }

    let path = tmp_path("wedeo_fate_flv_mux_h264.flv");
    let mut out = OutputContext::create(&path, "flv", &[stream]).unwrap();
    for p in &packets {
        out.write_packet(p).unwrap();
    }
    out.finish().unwrap();
    assert_eq!(
        std::fs::read(&path).unwrap(),
        reference,
        "H.264 FLV output must match `ffmpeg -f flv` byte for byte"
    );
}

/// Re-mux the exact payload bytes of an `ffmpeg -f flv` AAC file.
#[test]
fn bitexact_against_ffmpeg_aac() {
    let reference = unhex(FFMPEG_AAC_HEX);
    let tags = parse_tags(&reference);
    assert_eq!(tags.len(), 8); // metadata, AAC sequence header, 6 frames
    assert_eq!(tags[0].ty, 0x12);
    assert_eq!(tags[1].ty, 0x08);
    assert_eq!(&tags[1].data[..2], &[0xaf, 0x00]); // AAC sequence header
    let extradata = tags[1].data[2..].to_vec();
    assert_eq!(extradata.len(), 5);

    let mut par = CodecParameters::new(CodecId::Aac, MediaType::Audio);
    par.sample_rate = 44100;
    par.channel_layout = ChannelLayout::mono();
    par.bit_rate = 32_000;
    par.bits_per_coded_sample = 16;
    par.extradata = extradata;
    let mut stream = Stream::new(0, par);
    stream.time_base = Rational::new(1, 1000);
    stream.duration = 123; // initial duration guess 0.123

    // FFmpeg's last AAC frame has a truncated duration: 116 + 7 = 123 ms.
    let last_duration = 7i64;
    let frames = &tags[2..];
    let mut packets = Vec::new();
    for (i, tag) in frames.iter().enumerate() {
        let mut p = Packet::from_slice(&tag.data[2..]); // af 01 + raw AAC
        p.pts = tag.ts;
        p.dts = tag.ts;
        p.duration = if i + 1 < frames.len() {
            frames[i + 1].ts - tag.ts
        } else {
            last_duration
        };
        packets.push(p);
    }

    let path = tmp_path("wedeo_fate_flv_mux_aac.flv");
    let mut out = OutputContext::create(&path, "flv", &[stream]).unwrap();
    for p in &packets {
        out.write_packet(p).unwrap();
    }
    out.finish().unwrap();
    assert_eq!(
        std::fs::read(&path).unwrap(),
        reference,
        "AAC FLV output must match `ffmpeg -f flv` byte for byte"
    );
}

/// Verify the metadata values embedded by both references are reproduced.
#[test]
fn metadata_fields_match_ffmpeg() {
    for (hex, want_width, want_height, want_rate) in [
        (FFMPEG_H264_HEX, Some(64u32), Some(48u32), Some(5.0f64)),
        (FFMPEG_AAC_HEX, None, None, None),
    ] {
        let reference = unhex(hex);
        let tags = parse_tags(&reference);
        let meta = &tags[0].data;
        assert_eq!(&meta[..13], b"\x02\x00\x0aonMetaData");
        assert_eq!(meta[13], 0x08); // AMF mixed array
        assert_eq!(u32::from_be_bytes(meta[14..18].try_into().unwrap()), 7);

        let mut cursor = 18usize;
        let mut keys = Vec::new();
        while cursor + 2 <= meta.len() {
            // AMF keys here are bare `put_amf_string()` calls (2-byte length),
            // followed by a typed value. An empty key ends the object.
            let klen = u16::from_be_bytes([meta[cursor], meta[cursor + 1]]) as usize;
            if klen == 0 {
                assert_eq!(meta[cursor + 2], 0x09, "AMF end of object");
                break;
            }
            let key = std::str::from_utf8(&meta[cursor + 2..cursor + 2 + klen]).unwrap();
            let value_at = cursor + 2 + klen;
            let value = match meta[value_at] {
                0x00 => f64::from_bits(u64::from_be_bytes(
                    meta[value_at + 1..value_at + 9].try_into().unwrap(),
                )),
                0x01 => f64::from(u8::from(meta[value_at + 1] != 0)),
                other => panic!("unexpected AMF type {other:#x} for {key}"),
            };
            keys.push((key.to_string(), value));
            cursor = value_at + if meta[value_at] == 0x00 { 9 } else { 2 };
        }
        assert_eq!(keys.len(), 7, "7 metadata entries");
        assert!(keys.iter().any(|(k, _)| k == "duration"));
        assert!(keys.iter().any(|(k, _)| k == "filesize"));
        if let Some(w) = want_width {
            assert_eq!(keys.iter().find(|(k, _)| k == "width").unwrap().1, w as f64);
            assert_eq!(
                keys.iter().find(|(k, _)| k == "height").unwrap().1,
                want_height.unwrap() as f64
            );
            assert_eq!(
                keys.iter().find(|(k, _)| k == "framerate").unwrap().1,
                want_rate.unwrap()
            );
            assert_eq!(
                keys.iter().find(|(k, _)| k == "videocodecid").unwrap().1,
                7.0
            );
        } else {
            assert_eq!(
                keys.iter().find(|(k, _)| k == "audiocodecid").unwrap().1,
                10.0
            );
            assert_eq!(
                keys.iter().find(|(k, _)| k == "audiosamplerate").unwrap().1,
                44100.0
            );
            assert_eq!(
                keys.iter().find(|(k, _)| k == "audiosamplesize").unwrap().1,
                16.0
            );
            assert_eq!(keys.iter().find(|(k, _)| k == "stereo").unwrap().1, 0.0);
        }
    }
}
