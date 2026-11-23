//! H.264 MP4 to Annex B byte stream format filter.
//!
//! FFmpeg reference: `libavcodec/bsf/h264_mp4toannexb.c`.
//!
//! Converts length-prefixed (AVCC / ISO BMFF) H.264 packets into Annex B byte
//! stream format, injecting SPS/PPS from the codec extradata at IDR pictures
//! when they are not already present in-band.

use wedeo_codec::bitstream_filter::{
    BitstreamFilter, BitstreamFilterDescriptor, BitstreamFilterFactory,
};
use wedeo_codec::decoder::CodecParameters;
use wedeo_core::codec_id::CodecId;
use wedeo_core::error::{Error, Result};
use wedeo_core::packet::{Packet, PacketSideDataType};

use inventory as _;
use wedeo_codec as _;
use wedeo_core as _;

// NAL unit types from libavcodec/h264.h.
const H264_NAL_SLICE: u8 = 1;
const H264_NAL_IDR_SLICE: u8 = 5;
const H264_NAL_SEI: u8 = 6;
const H264_NAL_SPS: u8 = 7;
const H264_NAL_PPS: u8 = 8;

// SEI types from libavcodec/sei.h.
const SEI_TYPE_BUFFERING_PERIOD: u8 = 0;

static NALU_HEADER: [u8; 4] = [0, 0, 0, 1];

/// Where a parameter-set (or slice) NAL comes from, matching FFmpeg's
/// `enum PsSource`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PsSource {
    /// Start code already present in the out-of-band PS data; do not add one.
    OutOfBand = -1,
    /// Regular NAL: use a 4-byte start code if it is the first NAL, else 3-byte.
    None = 0,
    /// In-band SPS/PPS always gets a 4-byte start code.
    InBand = 1,
}

/// Append one NAL unit to `out`, choosing the Annex B start code length the
/// same way as FFmpeg's `count_or_copy()`.
fn count_or_copy(out: &mut Vec<u8>, input: &[u8], ps: PsSource) {
    // start code already present in out-of-band ps data, so don't need to
    // add it manually again
    let start_code_size = match ps {
        PsSource::OutOfBand => 0,
        PsSource::InBand => 4,
        PsSource::None if out.is_empty() => 4,
        PsSource::None => 3,
    };

    match start_code_size {
        4 => out.extend_from_slice(&NALU_HEADER),
        3 => out.extend_from_slice(&[0, 0, 1]),
        _ => {}
    }
    out.extend_from_slice(input);
}

/// Parse an AVCC / ISO BMFF `avcC` extradata blob into Annex-B-formatted SPS
/// and PPS buffers plus the length prefix size used by the packets.
///
/// Mirrors FFmpeg's `h264_extradata_to_annexb()`.
fn avcc_extradata_to_annexb(extradata: &[u8]) -> Result<(Vec<u8>, Vec<u8>, usize)> {
    if extradata.len() < 7 {
        return Err(Error::InvalidData);
    }

    let mut pos = 4usize; // configurationVersion, profile, compatibility, level

    // retrieve length coded size
    let length_size = (extradata[pos] & 0x03) as usize + 1;
    pos += 1;

    // retrieve sps and pps unit(s)
    let mut unit_nb = (extradata[pos] & 0x1f) as usize; // number of sps unit(s)
    pos += 1;

    let mut out: Vec<u8> = Vec::new();

    // SPS units. FFmpeg merges the PPS loop into the same iteration via a
    // goto; the two explicit phases below are equivalent.
    for _ in 0..unit_nb {
        if pos + 2 > extradata.len() {
            return Err(Error::InvalidData);
        }
        let unit_size = u16::from_be_bytes([extradata[pos], extradata[pos + 1]]) as usize;
        pos += 2;

        // possible overread ok due to padding; the extra byte accounts for
        // the PPS-count byte that follows the last SPS.
        if extradata.len() - pos < unit_size + 1 {
            return Err(Error::InvalidData);
        }

        out.extend_from_slice(&NALU_HEADER);
        out.extend_from_slice(&extradata[pos..pos + unit_size]);
        pos += unit_size;
    }

    // PPS count (always follows the SPS array, even when it is empty).
    if pos >= extradata.len() {
        return Err(Error::InvalidData);
    }
    unit_nb = extradata[pos] as usize; // number of pps unit(s)
    pos += 1;
    let pps_offset = out.len();

    for _ in 0..unit_nb {
        if pos + 2 > extradata.len() {
            return Err(Error::InvalidData);
        }
        let unit_size = u16::from_be_bytes([extradata[pos], extradata[pos + 1]]) as usize;
        pos += 2;

        if extradata.len() - pos < unit_size {
            return Err(Error::InvalidData);
        }

        out.extend_from_slice(&NALU_HEADER);
        out.extend_from_slice(&extradata[pos..pos + unit_size]);
        pos += unit_size;
    }

    let sps = out[..pps_offset].to_vec();
    let pps = out[pps_offset..].to_vec();

    Ok((sps, pps, length_size))
}

/// State of the `h264_mp4toannexb` filter, matching FFmpeg's `H264BSFContext`.
#[derive(Debug, Clone)]
pub struct H264Mp4ToAnnexb {
    /// SPS NAL units with Annex B start codes (from extradata or in-band).
    sps: Vec<u8>,
    /// PPS NAL units with Annex B start codes (from extradata or in-band).
    pps: Vec<u8>,
    /// Number of bytes of the AVCC length prefix.
    length_size: usize,
    /// Whether the next IDR slice may need SPS/PPS prepended.
    new_idr: bool,
    /// Whether an SPS has been seen since the last non-IDR slice.
    idr_sps_seen: bool,
    /// Whether a PPS has been seen since the last non-IDR slice.
    idr_pps_seen: bool,
    /// Whether AVCC extradata was successfully parsed (false => pass through).
    extradata_parsed: bool,
}

impl Default for H264Mp4ToAnnexb {
    fn default() -> Self {
        Self::new()
    }
}

impl H264Mp4ToAnnexb {
    /// Create a filter with no extradata. Without extradata the filter is a
    /// transparent pass-through until an `AV_PKT_DATA_NEW_EXTRADATA` packet
    /// side data (or [`Self::parse_extradata`]) supplies AVCC parameters.
    pub fn new() -> Self {
        Self {
            sps: Vec::new(),
            pps: Vec::new(),
            length_size: 4,
            new_idr: false,
            idr_sps_seen: false,
            idr_pps_seen: false,
            extradata_parsed: false,
        }
    }

    /// Create a filter initialized from codec extradata, mirroring FFmpeg's
    /// `h264_mp4toannexb_init()`.
    pub fn with_extradata(extradata: &[u8]) -> Result<Self> {
        let mut filter = Self::new();
        filter.init_extradata(extradata)?;
        Ok(filter)
    }

    /// Initialize from codec extradata. Annex B extradata (or empty extradata)
    /// leaves the filter in pass-through mode, as in FFmpeg.
    pub fn init_extradata(&mut self, extradata: &[u8]) -> Result<()> {
        if extradata.is_empty()
            || (extradata.len() >= 3 && extradata[..3] == [0, 0, 1])
            || (extradata.len() >= 4 && extradata[..4] == [0, 0, 0, 1])
        {
            // The input looks like it is Annex B already. Leave
            // extradata_parsed = false so packets pass through unchanged.
            return Ok(());
        }
        self.parse_avcc_extradata(extradata)
    }

    /// Parse an AVCC extradata blob (including `AV_PKT_DATA_NEW_EXTRADATA`
    /// side data, whose first byte is 1) and reset IDR tracking state.
    fn parse_avcc_extradata(&mut self, extradata: &[u8]) -> Result<()> {
        let (sps, pps, length_size) = avcc_extradata_to_annexb(extradata)?;
        self.sps = sps;
        self.pps = pps;
        self.length_size = length_size;
        self.new_idr = true;
        self.idr_sps_seen = false;
        self.idr_pps_seen = false;
        self.extradata_parsed = true;
        Ok(())
    }

    /// Scan a packet for in-band SPS/PPS and remember them, mirroring FFmpeg's
    /// `h264_mp4toannexb_filter_ps()`.
    fn filter_ps(&mut self, data: &[u8]) -> Result<()> {
        let mut buf = data;
        let mut sps_count = 0usize;
        let mut pps_count = 0usize;

        while !buf.is_empty() {
            if buf.len() < self.length_size {
                return Err(Error::InvalidData);
            }
            let mut nal_size: u32 = 0;
            for i in 0..self.length_size {
                nal_size = (nal_size << 8) | buf[i] as u32;
            }
            buf = &buf[self.length_size..];

            // This check requires the cast as the right side might otherwise
            // be promoted to an unsigned value.
            if nal_size as u64 > buf.len() as u64 {
                return Err(Error::InvalidData);
            }
            if nal_size == 0 {
                continue;
            }

            let nal_size = nal_size as usize;
            let nal = &buf[..nal_size];

            match nal[0] & 0x1f {
                H264_NAL_SPS => {
                    // h264_mp4toannexb_save_ps(..., first = !sps_count)
                    if sps_count == 0 {
                        self.sps.clear();
                    }
                    self.sps.extend_from_slice(&NALU_HEADER);
                    self.sps.extend_from_slice(nal);
                    sps_count += 1;
                }
                H264_NAL_PPS => {
                    if pps_count == 0 {
                        self.pps.clear();
                    }
                    self.pps.extend_from_slice(&NALU_HEADER);
                    self.pps.extend_from_slice(nal);
                    pps_count += 1;
                }
                _ => {}
            }

            buf = &buf[nal_size..];
        }

        Ok(())
    }

    /// Transform one AVCC packet's payload into Annex B, mirroring FFmpeg's
    /// `h264_mp4toannexb_filter()` core loop.
    fn filter_data(&mut self, data: &[u8]) -> Result<Vec<u8>> {
        // Retrieve SPS/PPS units from the packet itself first.
        self.filter_ps(data)?;

        let mut new_idr = self.new_idr;
        let mut sps_seen = self.idr_sps_seen;
        let mut pps_seen = self.idr_pps_seen;
        let mut out: Vec<u8> = Vec::new();

        let mut buf = data;
        while !buf.is_empty() {
            if buf.len() < self.length_size {
                return Err(Error::InvalidData);
            }
            let mut nal_size: u32 = 0;
            for i in 0..self.length_size {
                nal_size = (nal_size << 8) | buf[i] as u32;
            }
            buf = &buf[self.length_size..];

            // This check requires the cast as the right side might otherwise
            // be promoted to an unsigned value.
            if nal_size as u64 > buf.len() as u64 {
                return Err(Error::InvalidData);
            }
            if nal_size == 0 {
                continue;
            }

            let nal_size = nal_size as usize;
            let nal = &buf[..nal_size];
            let unit_type = nal[0] & 0x1f;
            // FFmpeg reads buf[1] relying on input padding; a one-byte NAL
            // therefore reads a zero padding byte.
            let nal1 = nal.get(1).copied().unwrap_or(0);

            if unit_type == H264_NAL_SPS {
                sps_seen = true;
                new_idr = true;
            } else if unit_type == H264_NAL_PPS {
                pps_seen = true;
                new_idr = true;
                // if SPS has not been seen yet, prepend the AVCC one to PPS
                if !sps_seen {
                    if self.sps.is_empty() {
                        // SPS not present in the stream, nor in AVCC, stream
                        // may be unreadable.
                    } else {
                        let sps = &self.sps;
                        count_or_copy(&mut out, sps, PsSource::OutOfBand);
                        sps_seen = true;
                    }
                }
            }

            // If this is a new IDR picture following an IDR picture, reset the
            // idr flag. Just check first_mb_in_slice to be 0 as this is the
            // simplest solution. This could be checking idr_pic_id instead,
            // but would complexify the parsing.
            if !new_idr && unit_type == H264_NAL_IDR_SLICE && (nal1 & 0x80) != 0 {
                new_idr = true;
            }

            // If this is a buffering period SEI without a corresponding
            // sps/pps then prepend any existing sps/pps before the SEI.
            if unit_type == H264_NAL_SEI
                && nal1 == SEI_TYPE_BUFFERING_PERIOD
                && !sps_seen
                && !pps_seen
            {
                if !self.sps.is_empty() {
                    let sps = &self.sps;
                    count_or_copy(&mut out, sps, PsSource::OutOfBand);
                    sps_seen = true;
                }
                if !self.pps.is_empty() {
                    let pps = &self.pps;
                    count_or_copy(&mut out, pps, PsSource::OutOfBand);
                    pps_seen = true;
                }
            }

            // prepend only to the first type 5 NAL unit of an IDR picture, if
            // no sps/pps are already present
            if new_idr && unit_type == H264_NAL_IDR_SLICE && !sps_seen && !pps_seen {
                if !self.sps.is_empty() {
                    let sps = &self.sps;
                    count_or_copy(&mut out, sps, PsSource::OutOfBand);
                }
                if !self.pps.is_empty() {
                    let pps = &self.pps;
                    count_or_copy(&mut out, pps, PsSource::OutOfBand);
                }
                new_idr = false;
                // if only SPS has been seen, also insert PPS
            } else if new_idr && unit_type == H264_NAL_IDR_SLICE && sps_seen && !pps_seen {
                if self.pps.is_empty() {
                    // PPS not present in the stream, nor in AVCC, stream may
                    // be unreadable.
                } else {
                    let pps = &self.pps;
                    count_or_copy(&mut out, pps, PsSource::OutOfBand);
                }
            }

            let ps = if unit_type == H264_NAL_SPS || unit_type == H264_NAL_PPS {
                PsSource::InBand
            } else {
                PsSource::None
            };
            count_or_copy(&mut out, nal, ps);

            if unit_type == H264_NAL_SLICE {
                new_idr = true;
                sps_seen = false;
                pps_seen = false;
            }

            buf = &buf[nal_size..];
        }

        self.new_idr = new_idr;
        self.idr_sps_seen = sps_seen;
        self.idr_pps_seen = pps_seen;

        Ok(out)
    }
}

impl BitstreamFilter for H264Mp4ToAnnexb {
    fn set_parameters(&mut self, params: &CodecParameters) -> Result<()> {
        // Mirrors FFmpeg's h264_mp4toannexb_init(): consume par_in->extradata.
        // init_extradata() keeps pass-through mode for empty / Annex B data.
        self.init_extradata(&params.extradata)
    }

    fn filter(&mut self, packet: &Packet) -> Result<Vec<Packet>> {
        // Extract the raw payload before any side-data handling.
        let data = packet.data.data().to_vec();

        // FFmpeg: AV_PKT_DATA_NEW_EXTRADATA triggers a re-parse of the
        // extradata when its first byte is 1, and the side data is removed.
        let mut handled_new_extradata = false;
        for side in &packet.side_data {
            if side.data_type == PacketSideDataType::NewExtradata && side.data.first() == Some(&1) {
                self.parse_avcc_extradata(&side.data)?;
                handled_new_extradata = true;
                break;
            }
        }

        let mut out_packet = if self.extradata_parsed {
            let out_data = self.filter_data(&data)?;
            let mut pkt = packet.clone();
            pkt.data = wedeo_core::buffer::Buffer::from_slice(&out_data);
            pkt
        } else {
            // nothing to filter
            packet.clone()
        };

        if handled_new_extradata {
            out_packet
                .side_data
                .retain(|s| s.data_type != PacketSideDataType::NewExtradata);
        }

        Ok(vec![out_packet])
    }

    fn flush(&mut self) -> Result<Vec<Packet>> {
        // FFmpeg's h264_mp4toannexb_flush(): reset IDR tracking state.
        self.idr_sps_seen = false;
        self.idr_pps_seen = false;
        self.new_idr = self.extradata_parsed;
        Ok(Vec::new())
    }
}

static DESCRIPTOR: BitstreamFilterDescriptor = BitstreamFilterDescriptor {
    name: "h264_mp4toannexb",
    long_name: "H.264 MP4 to Annex B byte stream format filter",
    codec_id: CodecId::H264,
    priority: 100,
};

/// Factory for the `h264_mp4toannexb` bitstream filter.
pub struct H264Mp4ToAnnexbFactory;

impl BitstreamFilterFactory for H264Mp4ToAnnexbFactory {
    fn descriptor(&self) -> &BitstreamFilterDescriptor {
        &DESCRIPTOR
    }

    fn create(&self) -> Result<Box<dyn BitstreamFilter>> {
        // The factory's default init() forwards CodecParameters (including
        // avcC extradata) to `BitstreamFilter::set_parameters`. Extradata can
        // also arrive later via NEW_EXTRADATA packet side data. Callers that
        // construct the filter directly may still use `with_extradata()`.
        Ok(Box::new(H264Mp4ToAnnexb::new()))
    }
}

inventory::submit!(&H264Mp4ToAnnexbFactory as &dyn BitstreamFilterFactory);

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal `avcC` extradata blob with 4-byte length prefixes.
    fn avcc_extradata(length_size: usize, sps: &[&[u8]], pps: &[&[u8]]) -> Vec<u8> {
        let mut e = vec![1, 0x42, 0x00, 0x1e, 0xff];
        e[4] = 0xfc | (length_size as u8 - 1);
        e.push(sps.len() as u8);
        for nal in sps {
            e.extend_from_slice(&(nal.len() as u16).to_be_bytes());
            e.extend_from_slice(nal);
        }
        e.push(pps.len() as u8);
        for nal in pps {
            e.extend_from_slice(&(nal.len() as u16).to_be_bytes());
            e.extend_from_slice(nal);
        }
        e
    }

    /// Wrap raw NAL units in AVCC length prefixes.
    fn avcc_packet(length_size: usize, nals: &[&[u8]]) -> Vec<u8> {
        let mut out = Vec::new();
        for nal in nals {
            match length_size {
                1 => out.push(nal.len() as u8),
                2 => out.extend_from_slice(&(nal.len() as u16).to_be_bytes()),
                _ => out.extend_from_slice(&(nal.len() as u32).to_be_bytes()),
            }
            out.extend_from_slice(nal);
        }
        out
    }

    /// Three-byte Annex B start code, used for non-first NALs once output data
    /// has already been written (matching FFmpeg's `count_or_copy`).
    const SC3: [u8; 3] = [0, 0, 1];

    fn packet(data: Vec<u8>) -> Packet {
        Packet::from_slice(&data)
    }

    fn out_data(pkts: Vec<Packet>) -> Vec<u8> {
        assert_eq!(pkts.len(), 1);
        pkts.into_iter().next().unwrap().data.data().to_vec()
    }

    #[test]
    fn set_parameters_consumes_avcc_extradata() {
        let sps = [0x67, 0x42, 0x00, 0x1e];
        let pps = [0x68, 0xce, 0x3c, 0x80];
        let ed = avcc_extradata(4, &[&sps], &[&pps]);
        let mut params = wedeo_codec::decoder::CodecParameters::new(
            CodecId::H264,
            wedeo_core::media_type::MediaType::Video,
        );
        params.extradata = ed;

        let mut bsf = H264Mp4ToAnnexb::new();
        bsf.set_parameters(&params).unwrap();
        assert!(bsf.extradata_parsed);
        assert_eq!(bsf.length_size, 4);

        let idr = [0x65, 0x00, 0xaa];
        let out = out_data(bsf.filter(&packet(avcc_packet(4, &[&idr]))).unwrap());
        let expected = [
            &NALU_HEADER[..],
            &sps[..],
            &NALU_HEADER[..],
            &pps[..],
            &SC3[..],
            &idr[..],
        ]
        .concat();
        assert_eq!(out, expected);
    }

    #[test]
    fn parse_avcc_extradata_splits_sps_pps() {
        let sps = [0x67, 0x42, 0x00, 0x1e];
        let pps = [0x68, 0xce, 0x3c, 0x80];
        let ed = avcc_extradata(4, &[&sps], &[&pps]);
        let (parsed_sps, parsed_pps, ls) = avcc_extradata_to_annexb(&ed).unwrap();
        assert_eq!(ls, 4);
        assert_eq!(parsed_sps, [&NALU_HEADER[..], &sps[..]].concat());
        assert_eq!(parsed_pps, [&NALU_HEADER[..], &pps[..]].concat());
    }

    #[test]
    fn parse_avcc_extradata_rejects_too_short() {
        assert_eq!(
            avcc_extradata_to_annexb(&[1, 2, 3]),
            Err(Error::InvalidData)
        );
    }

    #[test]
    fn idr_gets_sps_pps_injected() {
        let sps = [0x67, 0x42, 0x00, 0x1e];
        let pps = [0x68, 0xce, 0x3c, 0x80];
        let ed = avcc_extradata(4, &[&sps], &[&pps]);
        let mut bsf = H264Mp4ToAnnexb::with_extradata(&ed).unwrap();

        // IDR slice: first_mb_in_slice = 0 (buf[1] & 0x80 == 0).
        let idr = [0x65, 0x00, 0xaa, 0xbb];
        let pkt = packet(avcc_packet(4, &[&idr]));
        let out = out_data(bsf.filter(&pkt).unwrap());

        let expected = [
            &NALU_HEADER[..],
            &sps[..],
            &NALU_HEADER[..],
            &pps[..],
            &SC3[..],
            &idr[..],
        ]
        .concat();
        assert_eq!(out, expected);
    }

    #[test]
    fn non_idr_slice_uses_three_byte_start_code_and_no_ps() {
        let sps = [0x67, 0x42, 0x00, 0x1e];
        let pps = [0x68, 0xce, 0x3c, 0x80];
        let ed = avcc_extradata(4, &[&sps], &[&pps]);
        let mut bsf = H264Mp4ToAnnexb::with_extradata(&ed).unwrap();

        let non_idr = [0x41, 0x00, 0x11];
        let pkt = packet(avcc_packet(4, &[&non_idr]));
        let out = out_data(bsf.filter(&pkt).unwrap());

        let expected = [&NALU_HEADER[..], &non_idr[..]].concat();
        assert_eq!(out, expected);
    }

    #[test]
    fn in_band_sps_pps_are_not_duplicated_from_extradata() {
        let sps = [0x67, 0x42, 0x00, 0x1e];
        let pps = [0x68, 0xce, 0x3c, 0x80];
        let ed = avcc_extradata(4, &[&sps], &[&pps]);
        let mut bsf = H264Mp4ToAnnexb::with_extradata(&ed).unwrap();

        let idr = [0x65, 0x00, 0xaa];
        // SPS and PPS appear in-band before the IDR slice.
        let pkt = packet(avcc_packet(4, &[&sps, &pps, &idr]));
        let out = out_data(bsf.filter(&pkt).unwrap());

        let expected = [
            &NALU_HEADER[..],
            &sps[..],
            &NALU_HEADER[..],
            &pps[..],
            &SC3[..],
            &idr[..],
        ]
        .concat();
        assert_eq!(out, expected);
    }

    #[test]
    fn annexb_extradata_passes_packets_through() {
        let mut bsf = H264Mp4ToAnnexb::new();
        bsf.init_extradata(&[0, 0, 0, 1, 0x67, 0x42]).unwrap();
        let raw = [0, 0, 0, 1, 0x65, 0x00];
        let pkt = packet(raw.to_vec());
        let out = out_data(bsf.filter(&pkt).unwrap());
        assert_eq!(out, raw);
    }

    #[test]
    fn new_extradata_side_data_enables_filtering() {
        let sps = [0x67, 0x42, 0x00, 0x1e];
        let pps = [0x68, 0xce, 0x3c, 0x80];
        let ed = avcc_extradata(2, &[&sps], &[&pps]);
        let mut bsf = H264Mp4ToAnnexb::new();

        let idr = [0x65, 0x00, 0xaa];
        let mut pkt = packet(avcc_packet(2, &[&idr]));
        pkt.side_data.push(wedeo_core::packet::PacketSideData {
            data_type: PacketSideDataType::NewExtradata,
            data: ed,
        });
        let out_pkts = bsf.filter(&pkt).unwrap();
        assert_eq!(out_pkts.len(), 1);
        assert!(
            out_pkts[0]
                .side_data
                .iter()
                .all(|s| s.data_type != PacketSideDataType::NewExtradata)
        );

        let expected = [
            &NALU_HEADER[..],
            &sps[..],
            &NALU_HEADER[..],
            &pps[..],
            &SC3[..],
            &idr[..],
        ]
        .concat();
        assert_eq!(out_pkts[0].data.data(), expected.as_slice());
    }

    #[test]
    fn multiple_idrs_only_inject_on_first_and_after_flush() {
        let sps = [0x67, 0x42, 0x00, 0x1e];
        let pps = [0x68, 0xce, 0x3c, 0x80];
        let ed = avcc_extradata(4, &[&sps], &[&pps]);
        let mut bsf = H264Mp4ToAnnexb::with_extradata(&ed).unwrap();

        let idr = [0x65, 0x00, 0xaa];
        let first = out_data(bsf.filter(&packet(avcc_packet(4, &[&idr]))).unwrap());
        let with_ps = [
            &NALU_HEADER[..],
            &sps[..],
            &NALU_HEADER[..],
            &pps[..],
            &SC3[..],
            &idr[..],
        ]
        .concat();
        assert_eq!(first, with_ps);

        // Second IDR in the same access unit sequence: no injection.
        let second = out_data(bsf.filter(&packet(avcc_packet(4, &[&idr]))).unwrap());
        let no_ps = [&NALU_HEADER[..], &idr[..]].concat();
        assert_eq!(second, no_ps);

        // flush() resets the IDR flags, so injection happens again.
        assert!(bsf.flush().unwrap().is_empty());
        let third = out_data(bsf.filter(&packet(avcc_packet(4, &[&idr]))).unwrap());
        assert_eq!(third, with_ps);
    }

    #[test]
    fn buffering_period_sei_gets_sps_pps_before_it() {
        let sps = [0x67, 0x42, 0x00, 0x1e];
        let pps = [0x68, 0xce, 0x3c, 0x80];
        let ed = avcc_extradata(4, &[&sps], &[&pps]);
        let mut bsf = H264Mp4ToAnnexb::with_extradata(&ed).unwrap();

        let sei = [0x06, 0x00, 0x01, 0x02];
        let out = out_data(bsf.filter(&packet(avcc_packet(4, &[&sei]))).unwrap());
        let expected = [
            &NALU_HEADER[..],
            &sps[..],
            &NALU_HEADER[..],
            &pps[..],
            &SC3[..],
            &sei[..],
        ]
        .concat();
        assert_eq!(out, expected);
    }

    #[test]
    fn in_band_sps_only_does_not_inject_pps() {
        let sps = [0x67, 0x42, 0x00, 0x1e];
        let pps = [0x68, 0xce, 0x3c, 0x80];
        let ed = avcc_extradata(4, &[&sps], &[&pps]);
        let mut bsf = H264Mp4ToAnnexb::with_extradata(&ed).unwrap();

        // Only an in-band SPS: FFmpeg inserts the AVCC SPS before a PPS (not
        // the other way around), and the "insert PPS" branch applies to IDR
        // slices only. The in-band SPS is simply emitted with a 4-byte start
        // code.
        let out = out_data(bsf.filter(&packet(avcc_packet(4, &[&sps]))).unwrap());
        let expected = [&NALU_HEADER[..], &sps[..]].concat();
        assert_eq!(out, expected);
    }

    #[test]
    fn truncated_avcc_packet_is_rejected() {
        let sps = [0x67, 0x42, 0x00, 0x1e];
        let ed = avcc_extradata(4, &[&sps], &[]);
        let mut bsf = H264Mp4ToAnnexb::with_extradata(&ed).unwrap();
        // length prefix says 100 bytes but only 2 follow.
        let bad = [0, 0, 0, 100, 0x65, 0x00];
        assert_eq!(
            bsf.filter(&packet(bad.to_vec())).unwrap_err(),
            Error::InvalidData
        );
    }
}
