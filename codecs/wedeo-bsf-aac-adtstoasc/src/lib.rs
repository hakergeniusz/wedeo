//! MPEG-2/4 AAC ADTS to MPEG-4 Audio Specific Configuration bitstream filter.
//!
//! FFmpeg reference: `libavcodec/bsf/aac_adtstoasc.c`.
//!
//! This filter creates an MPEG-4 AudioSpecificConfig from an MPEG-2/4 ADTS
//! header and removes the ADTS header. The generated configuration is attached
//! to the first output packet as `AV_PKT_DATA_NEW_EXTRADATA`.

use wedeo_codec::bitstream_filter::{
    BitstreamFilter, BitstreamFilterDescriptor, BitstreamFilterFactory,
};
use wedeo_codec::decoder::CodecParameters;
use wedeo_core::buffer::Buffer;
use wedeo_core::codec_id::CodecId;
use wedeo_core::error::{Error, Result};
use wedeo_core::packet::{Packet, PacketSideData, PacketSideDataType};

use inventory as _;
use wedeo_codec as _;
use wedeo_core as _;

/// Size of the fixed ADTS header (`AV_AAC_ADTS_HEADER_SIZE`).
const AV_AAC_ADTS_HEADER_SIZE: usize = 7;

/// Maximum size of a PCE including the 3-bit ID_PCE (`MAX_PCE_SIZE`).
const MAX_PCE_SIZE: usize = 320;

/// `ff_mpeg4audio_sample_rates` from libavcodec/mpeg4audio.c.
const MPEG4_AUDIO_SAMPLE_RATES: [u32; 16] = [
    96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350, 0, 0,
    0,
];

/// MSB-first bit reader, matching FFmpeg's `GetBitContext`. Reads past the end
/// return zero, like FFmpeg's padded input buffer.
struct BitReader<'a> {
    data: &'a [u8],
    index: usize,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, index: 0 }
    }

    /// `get_bits()`: consume `n` bits, most significant bit first.
    fn get_bits(&mut self, n: usize) -> u32 {
        let mut value = 0u32;
        for _ in 0..n {
            let byte = self.data.get(self.index / 8).copied().unwrap_or(0);
            let bit = (byte >> (7 - (self.index % 8))) & 1;
            value = (value << 1) | u32::from(bit);
            self.index += 1;
        }
        value
    }

    /// `show_bits()`: peek at `n` bits without consuming them.
    fn show_bits(&self, n: usize) -> u32 {
        let mut value = 0u32;
        for i in 0..n {
            let index = self.index + i;
            let byte = self.data.get(index / 8).copied().unwrap_or(0);
            let bit = (byte >> (7 - (index % 8))) & 1;
            value = (value << 1) | u32::from(bit);
        }
        value
    }

    fn skip_bits(&mut self, n: usize) {
        self.index += n;
    }

    /// `align_get_bits()`: skip to the next byte boundary.
    fn align(&mut self) {
        self.index = (self.index + 7) & !7;
    }

    /// `get_bits_count()`.
    fn bits_count(&self) -> usize {
        self.index
    }
}

/// MSB-first bit writer, matching FFmpeg's `PutBitContext`. The backing buffer
/// is zero-initialised, so clearing a bit is a no-op and `flush_put_bits()`
/// merely leaves the trailing zero padding in place.
struct BitWriter<'a> {
    data: &'a mut [u8],
    index: usize,
}

impl<'a> BitWriter<'a> {
    fn new(data: &'a mut [u8]) -> Self {
        Self { data, index: 0 }
    }

    /// `put_bits()`: write the low `n` bits of `value`, most significant first.
    fn put_bits(&mut self, n: usize, value: u32) {
        for i in (0..n).rev() {
            let bit = (value >> i) & 1;
            let byte_index = self.index / 8;
            if byte_index < self.data.len() && bit != 0 {
                self.data[byte_index] |= 1 << (7 - (self.index % 8));
            }
            self.index += 1;
        }
    }

    /// `align_put_bits()`: advance to the next byte boundary, writing zeros.
    fn align(&mut self) {
        self.index = (self.index + 7) & !7;
    }

    /// `put_bits_count()`.
    fn bits_count(&self) -> usize {
        self.index
    }
}

/// Parsed ADTS header, matching FFmpeg's `AACADTSHeaderInfo`.
#[derive(Debug, Clone, Copy)]
struct AdtsHeaderInfo {
    frame_length: u32,
    crc_absent: bool,
    object_type: u8,
    sampling_index: u8,
    chan_config: u8,
    num_aac_frames: u8,
}

/// Parse the ADTS frame header to the end of the variable header, which is the
/// first 54 bits. Mirrors FFmpeg's `ff_adts_header_parse()`.
fn parse_adts_header(buf: &[u8]) -> Result<AdtsHeaderInfo> {
    if buf.len() < AV_AAC_ADTS_HEADER_SIZE {
        return Err(Error::InvalidData);
    }

    let mut gb = BitReader::new(buf);
    if gb.get_bits(12) != 0xfff {
        return Err(Error::InvalidData); // AAC_PARSE_ERROR_SYNC
    }

    gb.skip_bits(1); // id
    gb.skip_bits(2); // layer
    let crc_absent = gb.get_bits(1) != 0; // protection_absent
    let aot = gb.get_bits(2); // profile_objecttype
    let sr = gb.get_bits(4); // sample_frequency_index
    if MPEG4_AUDIO_SAMPLE_RATES[sr as usize] == 0 {
        return Err(Error::InvalidData); // AAC_PARSE_ERROR_SAMPLE_RATE
    }
    gb.skip_bits(1); // private_bit
    let ch = gb.get_bits(3); // channel_configuration

    gb.skip_bits(1); // original/copy
    gb.skip_bits(1); // home

    /* adts_variable_header */
    gb.skip_bits(1); // copyright_identification_bit
    gb.skip_bits(1); // copyright_identification_start
    let size = gb.get_bits(13); // aac_frame_length
    if size < AV_AAC_ADTS_HEADER_SIZE as u32 {
        return Err(Error::InvalidData); // AAC_PARSE_ERROR_FRAME_SIZE
    }

    gb.skip_bits(11); // adts_buffer_fullness
    let rdb = gb.get_bits(2); // number_of_raw_data_blocks_in_frame

    Ok(AdtsHeaderInfo {
        object_type: (aot + 1) as u8,
        chan_config: ch as u8,
        crc_absent,
        num_aac_frames: (rdb + 1) as u8,
        sampling_index: sr as u8,
        frame_length: size,
    })
}

/// `ff_pce_copy_bits()` from libavcodec/mpeg4audio_copy_pce.h.
fn pce_copy_bits(pb: &mut BitWriter<'_>, gb: &mut BitReader<'_>, n: usize) -> u32 {
    let el = gb.get_bits(n);
    pb.put_bits(n, el);
    el
}

/// `ff_copy_pce_data()` from libavcodec/mpeg4audio_copy_pce.h. Returns the
/// number of bits written to `pb`.
fn copy_pce_data(pb: &mut BitWriter<'_>, gb: &mut BitReader<'_>) -> usize {
    let offset = pb.bits_count();

    pce_copy_bits(pb, gb, 10); // Tag, Object Type, Frequency
    let mut five_bit_ch = pce_copy_bits(pb, gb, 4); // Front
    five_bit_ch += pce_copy_bits(pb, gb, 4); // Side
    five_bit_ch += pce_copy_bits(pb, gb, 4); // Back
    let mut four_bit_ch = pce_copy_bits(pb, gb, 2); // LFE
    four_bit_ch += pce_copy_bits(pb, gb, 3); // Data
    five_bit_ch += pce_copy_bits(pb, gb, 4); // Coupling
    if pce_copy_bits(pb, gb, 1) != 0 {
        // Mono Mixdown
        pce_copy_bits(pb, gb, 4);
    }
    if pce_copy_bits(pb, gb, 1) != 0 {
        // Stereo Mixdown
        pce_copy_bits(pb, gb, 4);
    }
    if pce_copy_bits(pb, gb, 1) != 0 {
        // Matrix Mixdown
        pce_copy_bits(pb, gb, 3);
    }
    let mut bits = (five_bit_ch * 5 + four_bit_ch * 4) as usize;
    while bits > 16 {
        pce_copy_bits(pb, gb, 16);
        bits -= 16;
    }
    if bits != 0 {
        pce_copy_bits(pb, gb, bits);
    }
    pb.align();
    gb.align();
    let mut comment_size = pce_copy_bits(pb, gb, 8);
    while comment_size > 0 {
        pce_copy_bits(pb, gb, 8);
        comment_size -= 1;
    }

    pb.bits_count() - offset
}

/// `get_object_type()` from libavcodec/mpeg4audio.c.
fn get_object_type(gb: &mut BitReader<'_>) -> u32 {
    let mut object_type = gb.get_bits(5);
    if object_type == 31 {
        // AOT_ESCAPE
        object_type = 32 + gb.get_bits(6);
    }
    object_type
}

/// `get_sample_rate()` from libavcodec/mpeg4audio.c.
fn get_sample_rate(gb: &mut BitReader<'_>) -> u32 {
    let index = gb.get_bits(4);
    if index == 0x0f {
        gb.get_bits(24)
    } else {
        MPEG4_AUDIO_SAMPLE_RATES[index as usize]
    }
}

/// Validate an AudioSpecificConfig, matching `aac_adtstoasc_init()`'s call to
/// `avpriv_mpeg4audio_get_config2()` on `par_in->extradata`.
fn validate_asc(extradata: &[u8]) -> Result<()> {
    if extradata.is_empty() {
        return Err(Error::InvalidData);
    }
    let mut gb = BitReader::new(extradata);

    let mut object_type = get_object_type(&mut gb);
    let _sample_rate = get_sample_rate(&mut gb);
    let chan_config = gb.get_bits(4);
    if chan_config >= 15 {
        // More entries than ff_mpeg4audio_channels[].
        return Err(Error::InvalidData);
    }

    // The SBR/PS branch re-reads the object type after the extension sampling
    // frequency; keep the same condition as ff_mpeg4audio_get_config_gb().
    let ps_condition =
        object_type == 29 && !((gb.show_bits(3) & 0x03 != 0) && (gb.show_bits(9) & 0x3f == 0));
    if object_type == 5 || ps_condition {
        let _ext_sample_rate = get_sample_rate(&mut gb);
        object_type = get_object_type(&mut gb);
        if object_type == 22 {
            // AOT_ER_BSAC
            let _ext_chan_config = gb.get_bits(4);
        }
    }

    if object_type == 36 {
        // parse_config_ALS(): the marker and the sample rate are mandatory.
        if extradata.len() * 8 < gb.bits_count() + 112 {
            return Err(Error::InvalidData);
        }
        if gb.get_bits(32) != u32::from_be_bytes(*b"ALS\0") {
            return Err(Error::InvalidData);
        }
        if gb.get_bits(32) == 0 {
            return Err(Error::InvalidData);
        }
    }

    // frame_length_short for the AAC family. A zero bit here is the common
    // 1024-sample case; the value is not used by this filter.
    match object_type {
        1..=4 | 17 | 19..=23 => {
            let _frame_length_short = gb.get_bits(1);
        }
        _ => {}
    }

    // The sync-extension scan walks one bit at a time and cannot fail, so no
    // further validation is needed.
    Ok(())
}

/// State of the `aac_adtstoasc` filter, matching FFmpeg's `AACBSFContext`.
#[derive(Debug, Clone)]
pub struct AacAdtstoAsc {
    /// FFmpeg's `first_frame_done`.
    first_frame_done: bool,
    /// Whether the input stream already has MPEG-4 AudioSpecificConfig
    /// extradata (FFmpeg's `bsfc->par_in->extradata`).
    extradata_present: bool,
}

impl Default for AacAdtstoAsc {
    fn default() -> Self {
        Self::new()
    }
}

impl AacAdtstoAsc {
    /// Create a filter with no input extradata. ADTS packets are converted.
    pub fn new() -> Self {
        Self {
            first_frame_done: false,
            extradata_present: false,
        }
    }

    /// Create a filter initialised with input extradata, mirroring FFmpeg's
    /// `aac_adtstoasc_init()`.
    pub fn with_extradata(extradata: &[u8]) -> Result<Self> {
        let mut filter = Self::new();
        filter.init_extradata(extradata)?;
        Ok(filter)
    }

    /// Validate and install input extradata. A stream that already carries an
    /// AudioSpecificConfig is passed through unchanged except when packets
    /// still start with an ADTS syncword.
    pub fn init_extradata(&mut self, extradata: &[u8]) -> Result<()> {
        validate_asc(extradata)?;
        self.extradata_present = true;
        Ok(())
    }
}

impl BitstreamFilter for AacAdtstoAsc {
    fn set_parameters(&mut self, params: &CodecParameters) -> Result<()> {
        // Mirrors FFmpeg's aac_adtstoasc_init(): validate the extradata only
        // when the stream already carries an AudioSpecificConfig. Plain ADTS
        // input has no extradata and must still initialize successfully.
        if params.extradata.is_empty() {
            return Ok(());
        }
        self.init_extradata(&params.extradata)
    }

    fn filter(&mut self, packet: &Packet) -> Result<Vec<Packet>> {
        // Extract the raw payload before any side-data handling. FFmpeg changes
        // pkt->data/pkt->size in place and preserves every other field, so
        // cloning the packet here preserves all packet properties.
        let data = packet.data.data().to_vec();
        let mut out_packet = packet.clone();

        // Validate the extradata if the stream is already MPEG-4
        // AudioSpecificConfig. Such packets have no ADTS syncword and pass
        // through untouched.
        if self.extradata_present
            && data.len() >= 2
            && (u16::from_be_bytes([data[0], data[1]]) >> 4) != 0xfff
        {
            return Ok(vec![out_packet]);
        }

        if data.len() < AV_AAC_ADTS_HEADER_SIZE {
            return Err(Error::InvalidData); // packet_too_small
        }

        let hdr = parse_adts_header(&data)?;
        debug_assert!(hdr.frame_length >= AV_AAC_ADTS_HEADER_SIZE as u32);

        // FFmpeg reports "Multiple RDBs per frame with CRC" as unsupported.
        if !hdr.crc_absent && hdr.num_aac_frames > 1 {
            return Err(Error::PatchwelcomeNotImplemented);
        }

        // Remove the ADTS header (and the two CRC bytes when present). Any
        // additional raw data blocks in this packet stay concatenated in the
        // payload, exactly as FFmpeg leaves them.
        let header_size = AV_AAC_ADTS_HEADER_SIZE + usize::from(!hdr.crc_absent) * 2;
        if data.len() <= header_size {
            return Err(Error::InvalidData); // packet_too_small
        }
        let mut payload_offset = header_size;

        if !self.first_frame_done {
            let mut pce_size = 0usize;
            let mut pce_data = [0u8; MAX_PCE_SIZE];

            if hdr.chan_config == 0 {
                // PCE-based channel configuration: the first syntax element
                // must be a program_config_element (ID_PCE == 5).
                let mut gb = BitReader::new(&data[payload_offset..]);
                if gb.get_bits(3) != 5 {
                    return Err(Error::PatchwelcomeNotImplemented);
                }
                let mut pb = BitWriter::new(&mut pce_data);
                let pce_bits = copy_pce_data(&mut pb, &mut gb);
                if pce_bits > MAX_PCE_SIZE * 8 {
                    return Err(Error::InvalidData);
                }
                pce_size = pce_bits / 8;

                // Advance past the PCE bytes that were just consumed.
                payload_offset += gb.bits_count() / 8;
                if payload_offset >= data.len() {
                    return Err(Error::InvalidData);
                }
            }

            // init_put_bits(&pb, extradata, 2 + pce_size);
            let mut extradata = vec![0u8; 2 + pce_size];
            {
                let mut pb = BitWriter::new(&mut extradata);
                pb.put_bits(5, u32::from(hdr.object_type));
                pb.put_bits(4, u32::from(hdr.sampling_index));
                pb.put_bits(4, u32::from(hdr.chan_config));
                pb.put_bits(1, 0); // frame length - 1024 samples
                pb.put_bits(1, 0); // does not depend on core coder
                pb.put_bits(1, 0); // is not extension
                // flush_put_bits(): the remaining bits are already zero.
            }
            if pce_size != 0 {
                extradata[2..2 + pce_size].copy_from_slice(&pce_data[..pce_size]);
            }

            // av_packet_new_side_data(pkt, AV_PKT_DATA_NEW_EXTRADATA, ...)
            out_packet.side_data.push(PacketSideData {
                data_type: PacketSideDataType::NewExtradata,
                data: extradata,
            });

            self.first_frame_done = true;
        }

        out_packet.data = Buffer::from_slice(&data[payload_offset..]);
        Ok(vec![out_packet])
    }

    fn flush(&mut self) -> Result<Vec<Packet>> {
        // FFmpeg has no flush callback for this bitstream filter, so
        // first_frame_done deliberately persists across a flush.
        Ok(Vec::new())
    }
}

static DESCRIPTOR: BitstreamFilterDescriptor = BitstreamFilterDescriptor {
    name: "aac_adtstoasc",
    long_name: "MPEG-2/4 AAC ADTS to MPEG-4 Audio Specific Configuration bitstream filter",
    codec_id: CodecId::Aac,
    priority: 100,
};

/// Factory for the `aac_adtstoasc` bitstream filter.
pub struct AacAdtstoAscFactory;

impl BitstreamFilterFactory for AacAdtstoAscFactory {
    fn descriptor(&self) -> &BitstreamFilterDescriptor {
        &DESCRIPTOR
    }

    fn create(&self) -> Result<Box<dyn BitstreamFilter>> {
        // The factory's default init() forwards CodecParameters (including a
        // possible AudioSpecificConfig) to `BitstreamFilter::set_parameters`.
        // Callers constructing the filter directly may still use
        // `AacAdtstoAsc::with_extradata()`.
        Ok(Box::new(AacAdtstoAsc::new()))
    }
}

inventory::submit!(&AacAdtstoAscFactory as &dyn BitstreamFilterFactory);

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a 7-byte ADTS header (no CRC) with the given fields.
    fn adts_header(
        profile: u32,
        sr_index: u32,
        chan_config: u32,
        payload_len: usize,
        rdb: u32,
    ) -> Vec<u8> {
        let mut bits = String::new();
        bits.push_str("111111111111"); // syncword
        bits.push('0'); // id: MPEG-4
        bits.push_str("00"); // layer
        bits.push('1'); // protection_absent (no CRC)
        bits.push_str(&format!("{profile:02b}"));
        bits.push_str(&format!("{sr_index:04b}"));
        bits.push('0'); // private_bit
        bits.push_str(&format!("{chan_config:03b}"));
        bits.push_str("00"); // original/copy, home
        bits.push_str("00"); // copyright id bit / start
        let frame_len = AV_AAC_ADTS_HEADER_SIZE + payload_len;
        bits.push_str(&format!("{frame_len:013b}"));
        bits.push_str("11111111111"); // adts_buffer_fullness (VBR)
        bits.push_str(&format!("{rdb:02b}"));
        assert_eq!(bits.len(), 56);

        (0..7)
            .map(|i| u8::from_str_radix(&bits[i * 8..i * 8 + 8], 2).unwrap())
            .collect()
    }

    fn adts_packet(profile: u32, sr_index: u32, chan_config: u32, payload: &[u8]) -> Packet {
        let mut data = adts_header(profile, sr_index, chan_config, payload.len(), 0);
        data.extend_from_slice(payload);
        Packet::from_slice(&data)
    }

    fn new_extradata(pkts: &[Packet]) -> Option<Vec<u8>> {
        pkts.iter()
            .flat_map(|p| p.side_data.iter())
            .find(|s| s.data_type == PacketSideDataType::NewExtradata)
            .map(|s| s.data.clone())
    }

    #[test]
    fn set_parameters_handles_empty_and_present_extradata() {
        use wedeo_codec::decoder::CodecParameters;
        use wedeo_core::media_type::MediaType;

        // ADTS input has no extradata: initialization must still succeed.
        let mut params = CodecParameters::new(CodecId::Aac, MediaType::Audio);
        let mut filter = AacAdtstoAsc::new();
        filter.set_parameters(&params).unwrap();
        assert!(!filter.extradata_present);
        let pkt = adts_packet(1, 4, 2, &[0x21, 0x10]);
        assert_eq!(
            new_extradata(&filter.filter(&pkt).unwrap()),
            Some(vec![0x12, 0x10])
        );

        // A stream that already carries an AudioSpecificConfig is validated
        // and installed, and non-ADTS packets pass through untouched.
        params.extradata = vec![0x12, 0x10];
        let mut filter = AacAdtstoAsc::new();
        filter.set_parameters(&params).unwrap();
        assert!(filter.extradata_present);
        let pkt = Packet::from_slice(&[0x21, 0x10, 0x00, 0x00]);
        let out = filter.filter(&pkt).unwrap();
        assert_eq!(out[0].data.data(), &[0x21, 0x10, 0x00, 0x00]);
        assert!(new_extradata(&out).is_none());

        // Invalid ASC is rejected at parameter setup, like FFmpeg's init.
        params.extradata = vec![0x12, 0x78];
        assert_eq!(
            AacAdtstoAsc::new().set_parameters(&params).err(),
            Some(Error::InvalidData)
        );
    }

    #[test]
    fn parse_header_fields() {
        let hdr = adts_header(1, 4, 2, 4, 0);
        let info = parse_adts_header(&hdr).unwrap();
        assert_eq!(info.object_type, 2);
        assert_eq!(info.sampling_index, 4);
        assert_eq!(info.chan_config, 2);
        assert!(info.crc_absent);
        assert_eq!(info.num_aac_frames, 1);
        assert_eq!(info.frame_length, 11);
    }

    #[test]
    fn reject_bad_sync_and_sample_rate() {
        assert_eq!(
            parse_adts_header(&[0x00; 7]).err(),
            Some(Error::InvalidData)
        );
        // Syncword ok, but sampling_frequency_index 13 is reserved.
        let mut hdr = adts_header(1, 4, 2, 4, 0);
        hdr[2] = (hdr[2] & 0x03) | (13 << 2);
        assert_eq!(parse_adts_header(&hdr).err(), Some(Error::InvalidData));
    }

    #[test]
    fn asc_matches_ffmpeg_lc_44100_stereo() {
        // Ground truth: ffmpeg -i gt_lc.aac -c:a copy -bsf:a aac_adtstoasc
        // ... -> extradata 12 10.
        let mut filter = AacAdtstoAsc::new();
        let pkt = adts_packet(1, 4, 2, &[0x21, 0x10, 0x00, 0x00]);
        let out = filter.filter(&pkt).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(new_extradata(&out), Some(vec![0x12, 0x10]));
        assert_eq!(out[0].data.data(), &[0x21, 0x10, 0x00, 0x00]);
    }

    #[test]
    fn asc_matches_ffmpeg_for_several_configs() {
        // The first four values come from FFmpeg's aac_adtstoasc on real ADTS
        // encodes; the remaining profiles use the same ASC bit layout that
        // FFmpeg writes (FFmpeg's native encoder does not support them).
        for (profile, sr, ch, expected) in [
            (1, 4, 2, [0x12, 0x10]), // AAC LC, 44100, stereo
            (1, 7, 2, [0x13, 0x90]), // AAC LC, 22050, stereo
            (1, 3, 6, [0x11, 0xb0]), // AAC LC, 48000, 5.1
            (1, 5, 1, [0x12, 0x88]), // AAC LC, 32000, mono
            (0, 3, 1, [0x09, 0x88]), // AAC Main, 48000, mono
            (2, 7, 6, [0x1b, 0xb0]), // AAC SSR, 22050, 5.1
            (3, 0, 2, [0x20, 0x10]), // AAC LTP, 96000, stereo
        ] {
            let mut filter = AacAdtstoAsc::new();
            let pkt = adts_packet(profile, sr, ch, &[0x21, 0x10]);
            let out = filter.filter(&pkt).unwrap();
            assert_eq!(
                new_extradata(&out),
                Some(expected.to_vec()),
                "profile={profile} sr={sr} ch={ch}"
            );
        }
    }

    #[test]
    fn extradata_only_on_first_packet() {
        let mut filter = AacAdtstoAsc::new();
        let pkt = adts_packet(1, 4, 2, &[0x21, 0x10]);
        let first = filter.filter(&pkt).unwrap();
        assert!(new_extradata(&first).is_some());
        let second = filter.filter(&pkt).unwrap();
        assert!(new_extradata(&second).is_none());
        assert_eq!(second[0].data.data(), &[0x21, 0x10]);
    }

    #[test]
    fn preserve_packet_properties() {
        let mut pkt = adts_packet(1, 4, 2, &[0x21, 0x10]);
        pkt.pts = 1234;
        pkt.dts = 1200;
        pkt.duration = 1024;
        pkt.stream_index = 7;
        pkt.flags = wedeo_core::packet::PacketFlags::KEY;
        pkt.pos = 42;
        pkt.trim_start = 3;
        pkt.trim_end = 4;
        pkt.side_data.push(PacketSideData {
            data_type: PacketSideDataType::SkipSamples,
            data: vec![1, 2, 3],
        });

        let out = AacAdtstoAsc::new().filter(&pkt).unwrap();
        assert_eq!(out.len(), 1);
        let out = &out[0];
        assert_eq!(out.pts, 1234);
        assert_eq!(out.dts, 1200);
        assert_eq!(out.duration, 1024);
        assert_eq!(out.stream_index, 7);
        assert_eq!(out.flags, wedeo_core::packet::PacketFlags::KEY);
        assert_eq!(out.pos, 42);
        assert_eq!(out.trim_start, 3);
        assert_eq!(out.trim_end, 4);
        assert!(
            out.side_data
                .iter()
                .any(|s| s.data_type == PacketSideDataType::SkipSamples)
        );
    }

    #[test]
    fn multiple_raw_data_blocks_are_left_concatenated() {
        let mut filter = AacAdtstoAsc::new();
        let mut data = adts_header(1, 4, 2, 8, 1); // two RDBs, no CRC
        let payload = [0x21, 0x10, 0x21, 0x10, 0x00, 0x00, 0x00, 0x00];
        data.extend_from_slice(&payload);
        let out = filter.filter(&Packet::from_slice(&data)).unwrap();
        assert_eq!(out[0].data.data(), payload.as_slice());
        assert_eq!(new_extradata(&out), Some(vec![0x12, 0x10]));
    }

    #[test]
    fn multiple_rdb_with_crc_is_rejected() {
        // protection_absent = 0, number_of_raw_data_blocks = 1.
        let mut bits = String::new();
        bits.push_str("111111111111");
        bits.push('0');
        bits.push_str("00");
        bits.push('0'); // CRC present
        bits.push_str("01"); // AAC LC
        bits.push_str("0100"); // 44100
        bits.push('0');
        bits.push_str("010"); // stereo
        bits.push_str("0000");
        bits.push_str(&format!("{:013b}", 7 + 2 + 4));
        bits.push_str("11111111111");
        bits.push_str("01"); // two RDBs
        let mut data: Vec<u8> = (0..7)
            .map(|i| u8::from_str_radix(&bits[i * 8..i * 8 + 8], 2).unwrap())
            .collect();
        data.extend_from_slice(&[0xab, 0xcd]); // CRC
        data.extend_from_slice(&[0x21, 0x10, 0x00, 0x00]);

        let mut filter = AacAdtstoAsc::new();
        assert_eq!(
            filter.filter(&Packet::from_slice(&data)).err(),
            Some(Error::PatchwelcomeNotImplemented)
        );
    }

    #[test]
    fn crc_bytes_are_stripped() {
        let mut bits = String::new();
        bits.push_str("111111111111");
        bits.push('0');
        bits.push_str("00");
        bits.push('0'); // CRC present
        bits.push_str("01"); // AAC LC
        bits.push_str("0100"); // 44100
        bits.push('0');
        bits.push_str("010"); // stereo
        bits.push_str("0000");
        bits.push_str(&format!("{:013b}", 7 + 2 + 4));
        bits.push_str("11111111111");
        bits.push_str("00"); // one RDB
        let mut data: Vec<u8> = (0..7)
            .map(|i| u8::from_str_radix(&bits[i * 8..i * 8 + 8], 2).unwrap())
            .collect();
        data.extend_from_slice(&[0xab, 0xcd]); // CRC
        data.extend_from_slice(&[0x21, 0x10, 0x00, 0x00]);

        let out = AacAdtstoAsc::new()
            .filter(&Packet::from_slice(&data))
            .unwrap();
        assert_eq!(out[0].data.data(), &[0x21, 0x10, 0x00, 0x00]);
        assert_eq!(new_extradata(&out), Some(vec![0x12, 0x10]));
    }

    #[test]
    fn pce_is_copied_into_extradata_and_skipped() {
        // A minimal program_config_element: 3-bit ID_PCE (5), then the fields
        // copied by ff_copy_pce_data(), alignment bits and the comment length.
        // No channel configuration bits are present (all channel counts 0).
        let pce_bits = "101" // id_syn_ele = ID_PCE (not copied into the ASC)
            .to_string()
            + "0000" // element_instance_tag
            + "01" // object_type = AAC LC
            + "0100" // sampling_frequency_index = 44100
            + "0000" // front channel elements
            + "0000" // side
            + "0000" // back
            + "00" // LFE
            + "000" // data
            + "0000" // coupling
            + "0" // mono mixdown present
            + "0" // stereo mixdown present
            + "0" // matrix mixdown present
            + "000" // align_get_bits() to the next byte boundary
            + "00000000"; // comment_data_length = 0
        assert_eq!(pce_bits.len(), 48);
        let pce_bytes: Vec<u8> = (0..6)
            .map(|i| u8::from_str_radix(&pce_bits[i * 8..i * 8 + 8], 2).unwrap())
            .collect();
        assert_eq!(pce_bytes, vec![0xa0, 0xa0, 0, 0, 0, 0]);
        let payload_tail = [0xaa, 0xbb, 0xcc];

        let mut data = adts_header(1, 4, 0, pce_bytes.len() + payload_tail.len(), 0);
        data.extend_from_slice(&pce_bytes);
        data.extend_from_slice(&payload_tail);

        let out = AacAdtstoAsc::new()
            .filter(&Packet::from_slice(&data))
            .unwrap();
        // The ASC is 16 bits (object type / sampling index / chan_config = 0 /
        // flags), followed by the PCE bytes starting at element_instance_tag,
        // shifted by the removed 3-bit element id and re-aligned by the writer.
        let expected_extradata = vec![0x12, 0x00, 0x05, 0, 0, 0, 0, 0];
        assert_eq!(new_extradata(&out), Some(expected_extradata));
        assert_eq!(out[0].data.data(), payload_tail.as_slice());
    }

    #[test]
    fn non_pce_first_element_is_rejected() {
        // chan_config = 0 but the first element is not ID_PCE.
        let data = adts_packet(1, 4, 0, &[0x21, 0x10]);
        let mut filter = AacAdtstoAsc::new();
        assert_eq!(
            filter.filter(&data).err(),
            Some(Error::PatchwelcomeNotImplemented)
        );
    }

    #[test]
    fn existing_asc_extradata_passes_through() {
        let mut filter = AacAdtstoAsc::with_extradata(&[0x12, 0x10]).unwrap();
        // A packet that is not ADTS must pass through unchanged.
        let pkt = Packet::from_slice(&[0x21, 0x10, 0x00, 0x00]);
        let out = filter.filter(&pkt).unwrap();
        assert_eq!(out[0].data.data(), &[0x21, 0x10, 0x00, 0x00]);
        assert!(new_extradata(&out).is_none());

        // An ADTS packet in the same stream is still converted.
        let pkt = adts_packet(1, 4, 2, &[0x21, 0x10]);
        let out = filter.filter(&pkt).unwrap();
        assert_eq!(new_extradata(&out), Some(vec![0x12, 0x10]));
    }

    #[test]
    fn invalid_extradata_is_rejected() {
        assert_eq!(
            AacAdtstoAsc::with_extradata(&[]).err(),
            Some(Error::InvalidData)
        );
        // chan_config = 15 is invalid (ff_mpeg4audio_channels[] has 15 entries).
        // 00010 (AAC LC) 0100 (44100) 1111 (chan_config) 000 (flags) = 12 78.
        assert_eq!(
            AacAdtstoAsc::with_extradata(&[0x12, 0x78]).err(),
            Some(Error::InvalidData)
        );
    }

    #[test]
    fn too_small_packets_are_rejected() {
        let mut filter = AacAdtstoAsc::new();
        assert_eq!(
            filter.filter(&Packet::from_slice(&[0x0f])).err(),
            Some(Error::InvalidData)
        );
        // Header only: nothing left after stripping it.
        let hdr = adts_header(1, 4, 2, 0, 0);
        assert_eq!(
            filter.filter(&Packet::from_slice(&hdr)).err(),
            Some(Error::InvalidData)
        );
    }

    #[test]
    fn flush_is_empty_and_state_persists() {
        let mut filter = AacAdtstoAsc::new();
        assert!(filter.flush().unwrap().is_empty());
        let pkt = adts_packet(1, 4, 2, &[0x21, 0x10]);
        assert!(new_extradata(&filter.filter(&pkt).unwrap()).is_some());
        assert!(filter.flush().unwrap().is_empty());
        // FFmpeg has no flush callback: no NEW_EXTRADATA on later packets.
        assert!(new_extradata(&filter.filter(&pkt).unwrap()).is_none());
    }
}
