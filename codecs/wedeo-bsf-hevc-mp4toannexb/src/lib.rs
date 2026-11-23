//! HEVC MP4 to Annex B byte stream format filter.
//!
//! FFmpeg reference: `libavcodec/bsf/hevc_mp4toannexb.c`.
//!
//! Converts length-prefixed hvcC/HEVC packets into Annex B byte stream format.
//! The `hvcC` (ISO/IEC 14496-15) extradata is parsed into arrays of VPS/SPS/PPS
//! and prefix/suffix SEI NAL units, which are injected before IRAP access units
//! when they are not already present in-band. In-band parameter sets are matched
//! against the extradata copies by an FNV-1a hash to avoid duplicate emission.

use wedeo_codec::bitstream_filter::{
    BitstreamFilter, BitstreamFilterDescriptor, BitstreamFilterFactory,
};
use wedeo_core::codec_id::CodecId;
use wedeo_core::error::{Error, Result};
use wedeo_core::packet::{Packet, PacketSideDataType};

use inventory as _;
use wedeo_codec as _;
use wedeo_core as _;

// libavcodec/hevc/hevc.h: enum HEVCNALUnitType
const HEVC_NAL_BLA_W_LP: u8 = 16;
const HEVC_NAL_RSV_IRAP_VCL23: u8 = 23;
const HEVC_NAL_RSV_VCL31: u8 = 31;
const HEVC_NAL_VPS: u8 = 32;
const HEVC_NAL_SPS: u8 = 33;
const HEVC_NAL_PPS: u8 = 34;
const HEVC_NAL_AUD: u8 = 35;
const HEVC_NAL_SEI_PREFIX: u8 = 39;
const HEVC_NAL_SEI_SUFFIX: u8 = 40;

// From hevc_mp4toannexb.c
const MIN_HEVCC_LENGTH: usize = 23;
const MAX_EXTRADATA_NALS: usize = 2048;
const MAX_PS_CHAIN_DEPTH: usize = 16;
const MAX_PS_CHECKS_PER_PACKET: usize = 128;

// 32-bit FNV-1a hash algorithm constants
const FNV1A_32_OFFSET_BASIS: u32 = 2166136261;
const FNV1A_32_PRIME: u32 = 16777619;

/// `AV_WB32(dst, 1)` - the Annex B four-byte start code.
const NALU_HEADER: [u8; 4] = [0, 0, 0, 1];

/// 32-bit FNV-1a hash of a parameter-set payload.
fn hash_ps(data: &[u8]) -> u32 {
    let mut h = FNV1A_32_OFFSET_BASIS;
    for &b in data {
        h ^= b as u32;
        h = h.wrapping_mul(FNV1A_32_PRIME);
    }
    h
}

/// libavutil/common.h: `av_log2()` (floor log2, with av_log2(0) == 0).
fn av_log2(v: u32) -> u32 {
    if v == 0 { 0 } else { 31 - v.leading_zeros() }
}

/// libavutil/common.h: `av_ceil_log2_c()` -> `av_log2((x - 1U) << 1)`.
fn av_ceil_log2(x: usize) -> usize {
    let x = x as u32;
    av_log2((x - 1) << 1) as usize
}

/// Whether extradata is an HEVCDecoderConfigurationRecord (ISO/IEC 14496-15)
/// rather than Annex B. Mirrors FFmpeg's `ff_hevc_is_hvcc()`.
///
/// `configurationVersion` 0 predates the standard and is still written by older
/// muxers. A record is at least 23 bytes.
fn is_hvcc(data: &[u8]) -> bool {
    data.len() >= MIN_HEVCC_LENGTH
        && (data[0] == 1 || (data[0] == 0 && (data[1] != 0 || data[2] > 1)))
}

/// `HEVCNALUnit` from hevc_mp4toannexb.c.
#[derive(Debug, Clone)]
struct HevcNalUnit {
    /// Offset into `HevcMp4ToAnnexb::extradata` (including the start code).
    offset: usize,
    /// Size including the four-byte start code.
    size: usize,
    typ: u8,
    hash: u32,
    matched: bool,
    emit: bool,
    /// Index of the next NAL in the same `ps_buckets` chain, or -1.
    next: i32,
}

/// State of the `hevc_mp4toannexb` filter, matching FFmpeg's `HEVCBSFContext`.
#[derive(Debug, Clone)]
pub struct HevcMp4ToAnnexb {
    /// Extradata converted to Annex B form (start codes included).
    extradata: Vec<u8>,
    /// Descriptors for every NAL in `extradata`.
    nals: Vec<HevcNalUnit>,
    /// Number of VPS/SPS/PPS NALs in `extradata`.
    nb_ps: usize,
    /// Hash bucket heads (indices into `nals`), or empty when there are no PS.
    ps_buckets: Vec<i32>,
    /// `ps_buckets.len()`; a power of two.
    ps_table_size: usize,
    /// Number of bytes of the hvcC length prefix.
    length_size: usize,
    /// Whether hvcC extradata was successfully parsed (false => pass through).
    extradata_parsed: bool,
    dup_ps_logged: bool,
    /// Bitmask of extradata NAL types already emitted for this packet.
    emitted_types: u32,
    /// Whether an SPS has been emitted/seen for this packet.
    sps_seen: bool,
}

impl Default for HevcMp4ToAnnexb {
    fn default() -> Self {
        Self::new()
    }
}

impl HevcMp4ToAnnexb {
    /// Create a filter with no extradata. Without extradata the filter is a
    /// transparent pass-through until an `AV_PKT_DATA_NEW_EXTRADATA` packet
    /// side data (or [`Self::parse_extradata`]) supplies hvcC parameters.
    pub fn new() -> Self {
        Self {
            extradata: Vec::new(),
            nals: Vec::new(),
            nb_ps: 0,
            ps_buckets: Vec::new(),
            ps_table_size: 0,
            length_size: 4,
            extradata_parsed: false,
            dup_ps_logged: false,
            emitted_types: 0,
            sps_seen: false,
        }
    }

    /// Create a filter initialized from codec extradata, mirroring FFmpeg's
    /// `hevc_mp4toannexb_init()`.
    pub fn with_extradata(extradata: &[u8]) -> Result<Self> {
        let mut filter = Self::new();
        filter.init_extradata(extradata)?;
        Ok(filter)
    }

    /// Initialize from codec extradata. Annex B extradata (or empty/short
    /// extradata) leaves the filter in pass-through mode, as in FFmpeg.
    pub fn init_extradata(&mut self, extradata: &[u8]) -> Result<()> {
        if extradata.len() < MIN_HEVCC_LENGTH
            || (extradata.len() >= 3 && extradata[..3] == [0, 0, 1])
            || (extradata.len() >= 4 && extradata[..4] == [0, 0, 0, 1])
        {
            // The input looks like it is Annex B already. Leave
            // extradata_parsed = false so packets pass through unchanged.
            return Ok(());
        }
        self.parse_extradata(extradata)
    }

    /// Parse an hvcC extradata blob into Annex B parameter sets and the
    /// parameter-set hash table. Mirrors FFmpeg's `hevc_extradata_to_annexb()`.
    pub fn parse_extradata(&mut self, extradata: &[u8]) -> Result<()> {
        // bytestream2 and AVCodecParameters.extradata_size are both int.
        if extradata.len() > i32::MAX as usize {
            return Err(Error::InvalidData);
        }
        if extradata.len() < MIN_HEVCC_LENGTH {
            return Err(Error::InvalidData);
        }

        // bytestream2_skip(&gb, 21);
        let length_size = ((extradata[21] & 3) as usize) + 1;
        let num_arrays = extradata[22] as usize;
        let mut pos = MIN_HEVCC_LENGTH;

        // First pass: validate hvcC arrays and compute exact size and NAL count
        // upfront.
        let mut total_nals = 0usize;
        let mut total_size = 0usize;
        for _ in 0..num_arrays {
            if extradata.len() - pos < 3 {
                return Err(Error::InvalidData);
            }
            let typ = extradata[pos] & 0x3f;
            let cnt = u16::from_be_bytes([extradata[pos + 1], extradata[pos + 2]]) as usize;
            pos += 3;

            if !(typ == HEVC_NAL_VPS
                || typ == HEVC_NAL_SPS
                || typ == HEVC_NAL_PPS
                || typ == HEVC_NAL_SEI_PREFIX
                || typ == HEVC_NAL_SEI_SUFFIX)
            {
                return Err(Error::InvalidData);
            }
            if cnt > MAX_EXTRADATA_NALS - total_nals {
                return Err(Error::InvalidData);
            }

            for _ in 0..cnt {
                if extradata.len() - pos < 2 {
                    return Err(Error::InvalidData);
                }
                let nalu_len = u16::from_be_bytes([extradata[pos], extradata[pos + 1]]) as usize;
                pos += 2;

                if nalu_len < 2 || nalu_len > extradata.len() - pos {
                    return Err(Error::InvalidData);
                }
                // Each hvcC length prefix grows into a 4-byte start code.
                total_size = total_size
                    .checked_add(4 + nalu_len)
                    .ok_or(Error::InvalidData)?;
                total_nals += 1;
                pos += nalu_len;
            }
        }

        // Second pass: copy NAL units and fill descriptor array without
        // reallocations.
        let mut new_extradata = Vec::with_capacity(total_size);
        let mut new_nals = Vec::with_capacity(total_nals);
        let mut pos = MIN_HEVCC_LENGTH;
        for _ in 0..num_arrays {
            let typ = extradata[pos] & 0x3f;
            let cnt = u16::from_be_bytes([extradata[pos + 1], extradata[pos + 2]]) as usize;
            pos += 3;

            for _ in 0..cnt {
                let nalu_len = u16::from_be_bytes([extradata[pos], extradata[pos + 1]]) as usize;
                pos += 2;

                let offset = new_extradata.len();
                new_nals.push(HevcNalUnit {
                    offset,
                    size: 4 + nalu_len,
                    typ,
                    matched: false,
                    emit: true,
                    hash: 0,
                    next: -1,
                });

                // AV_WB32(new_extradata + new_extradata_size, 1);
                new_extradata.extend_from_slice(&NALU_HEADER);
                new_extradata.extend_from_slice(&extradata[pos..pos + nalu_len]);
                pos += nalu_len;
            }
        }

        let nb_ps = new_nals
            .iter()
            .filter(|n| (HEVC_NAL_VPS..=HEVC_NAL_PPS).contains(&n.typ))
            .count();

        let mut ps_buckets = Vec::new();
        let mut ps_table_size = 0usize;
        if nb_ps > 0 {
            ps_table_size = 1usize << av_ceil_log2(usize::max(32, nb_ps * 2));
            ps_buckets.resize(ps_table_size, -1);

            for i in 0..new_nals.len() {
                if (HEVC_NAL_VPS..=HEVC_NAL_PPS).contains(&new_nals[i].typ) {
                    let hash = hash_ps(
                        &new_extradata
                            [new_nals[i].offset + 4..new_nals[i].offset + new_nals[i].size],
                    );
                    new_nals[i].hash = hash;
                    let bucket = (hash as usize) & (ps_table_size - 1);
                    new_nals[i].next = ps_buckets[bucket];
                    ps_buckets[bucket] = i as i32;
                }
            }
        }

        self.extradata = new_extradata;
        self.nals = new_nals;
        self.nb_ps = nb_ps;
        self.ps_buckets = ps_buckets;
        self.ps_table_size = ps_table_size;
        self.length_size = length_size;
        self.extradata_parsed = true;
        self.dup_ps_logged = false;
        Ok(())
    }

    /// `emit_extradata_type()` from hevc_mp4toannexb.c: emit every still-pending
    /// extradata NAL of the given type at `dst`.
    fn emit_extradata_type(&mut self, dst: &mut Vec<u8>, typ: u8) {
        let bit = 1u32 << (typ as u32 - HEVC_NAL_VPS as u32);
        if self.emitted_types & bit != 0 {
            return;
        }
        self.emitted_types |= bit;

        for i in 0..self.nals.len() {
            let (emit, nal_type, offset, size) = {
                let nal = &self.nals[i];
                (nal.emit, nal.typ, nal.offset, nal.size)
            };
            if emit && nal_type == typ {
                dst.extend_from_slice(&self.extradata[offset..offset + size]);
                self.nals[i].emit = false;
                if typ == HEVC_NAL_SPS {
                    self.sps_seen = true;
                }
            }
        }
    }

    /// `flush_extradata_before()` from hevc_mp4toannexb.c: emit the extradata
    /// parameter sets a NAL of `nalu_type` depends on, before that NAL.
    fn flush_extradata_before(&mut self, dst: &mut Vec<u8>, nalu_type: u8, has_irap: bool) {
        if !has_irap {
            return;
        }

        match nalu_type {
            HEVC_NAL_AUD => {}
            HEVC_NAL_VPS => self.emit_extradata_type(dst, HEVC_NAL_VPS),
            HEVC_NAL_SPS => {
                self.emit_extradata_type(dst, HEVC_NAL_VPS);
                self.emit_extradata_type(dst, HEVC_NAL_SPS);
            }
            HEVC_NAL_PPS => {
                self.emit_extradata_type(dst, HEVC_NAL_VPS);
                self.emit_extradata_type(dst, HEVC_NAL_SPS);
                self.emit_extradata_type(dst, HEVC_NAL_PPS);
            }
            HEVC_NAL_SEI_PREFIX => {
                self.emit_extradata_type(dst, HEVC_NAL_VPS);
                self.emit_extradata_type(dst, HEVC_NAL_SPS);
                if self.sps_seen {
                    self.emit_extradata_type(dst, HEVC_NAL_PPS);
                }
            }
            HEVC_NAL_SEI_SUFFIX => {
                self.emit_extradata_type(dst, HEVC_NAL_VPS);
                self.emit_extradata_type(dst, HEVC_NAL_SPS);
                self.emit_extradata_type(dst, HEVC_NAL_PPS);
                self.emit_extradata_type(dst, HEVC_NAL_SEI_PREFIX);
                self.emit_extradata_type(dst, HEVC_NAL_SEI_SUFFIX);
            }
            _ => {
                if nalu_type <= HEVC_NAL_RSV_VCL31 {
                    self.emit_extradata_type(dst, HEVC_NAL_VPS);
                    self.emit_extradata_type(dst, HEVC_NAL_SPS);
                    self.emit_extradata_type(dst, HEVC_NAL_PPS);
                    self.emit_extradata_type(dst, HEVC_NAL_SEI_PREFIX);
                }
            }
        }
    }

    /// Scan a length-prefixed packet and return `(payload_offset, size, type)`
    /// for every NAL. Mirrors the validation done by FFmpeg's first
    /// `bytestream2` pass in `hevc_mp4toannexb_filter()`.
    fn packet_nals(data: &[u8], length_size: usize) -> Result<Vec<(usize, usize, u8)>> {
        let mut nals = Vec::new();
        let mut pos = 0usize;
        while pos < data.len() {
            if data.len() - pos < length_size {
                return Err(Error::InvalidData);
            }
            let mut nalu_size: u32 = 0;
            for i in 0..length_size {
                nalu_size = (nalu_size << 8) | data[pos + i] as u32;
            }
            pos += length_size;

            let nalu_size = nalu_size as usize;
            if nalu_size < 2 || nalu_size > data.len() - pos {
                return Err(Error::InvalidData);
            }
            // bytestream2_peek_byte(&gb) >> 1 & 0x3f
            let nalu_type = (data[pos] >> 1) & 0x3f;
            nals.push((pos, nalu_size, nalu_type));
            pos += nalu_size;
        }
        Ok(nals)
    }

    /// Convert one length-prefixed packet's payload into Annex B form.
    fn filter_data(&mut self, data: &[u8]) -> Result<Vec<u8>> {
        let packet_nals = Self::packet_nals(data, self.length_size)?;

        // Scan the packet: match any in-band VPS/SPS/PPS against the extradata
        // copies so they can be omitted from the injected parameter sets.
        for nal in &mut self.nals {
            nal.matched = false;
        }
        let mut unmatched_ps = self.nb_ps;
        let mut ps_checks = 0usize;
        let mut max_inband_ps_level = 0i32;
        let mut got_irap = false;
        let mut got_ps = false;

        for &(offset, size, nalu_type) in &packet_nals {
            // Prefix SEI can flush dependent extradata before later VPS/SPS.
            if !got_irap && nalu_type == HEVC_NAL_SEI_PREFIX {
                max_inband_ps_level = HEVC_NAL_PPS as i32 - HEVC_NAL_VPS as i32;
            }
            if !got_irap && (HEVC_NAL_VPS..=HEVC_NAL_PPS).contains(&nalu_type) {
                let inband_level = nalu_type as i32 - HEVC_NAL_VPS as i32;
                if inband_level < max_inband_ps_level {
                    // Inverted topological order (e.g. PPS before SPS).
                    // Earlier dependencies cannot be suppressed by this
                    // out-of-order PS.
                } else {
                    max_inband_ps_level = inband_level;
                    if unmatched_ps > 0
                        && !self.ps_buckets.is_empty()
                        && ps_checks < MAX_PS_CHECKS_PER_PACKET
                    {
                        ps_checks += 1;
                        let hash = hash_ps(&data[offset..offset + size]);
                        let mut idx = self.ps_buckets[(hash as usize) & (self.ps_table_size - 1)];
                        let mut depth = 0usize;
                        while idx >= 0 && depth < MAX_PS_CHAIN_DEPTH {
                            depth += 1;
                            let (nal_type, nal_size, nal_hash, nal_next, nal_offset) = {
                                let nal = &self.nals[idx as usize];
                                (nal.typ, nal.size, nal.hash, nal.next, nal.offset)
                            };
                            if nal_type == nalu_type
                                && nal_size - 4 == size
                                && nal_hash == hash
                                && self.extradata[nal_offset + 4..nal_offset + nal_size]
                                    == data[offset..offset + size]
                                && !self.nals[idx as usize].matched
                            {
                                self.nals[idx as usize].matched = true;
                                unmatched_ps -= 1;
                            }
                            idx = nal_next;
                        }
                    }
                }
            }
            got_ps |= !got_irap && (HEVC_NAL_VPS..=HEVC_NAL_PPS).contains(&nalu_type);
            got_irap |= (HEVC_NAL_BLA_W_LP..=HEVC_NAL_RSV_IRAP_VCL23).contains(&nalu_type);
        }
        let seen_irap_ps = got_irap && got_ps;

        self.emitted_types = 0;
        self.sps_seen = false;

        if got_irap {
            let mut omitted = 0usize;
            for i in 0..self.nals.len() {
                let (typ, matched) = {
                    let nal = &self.nals[i];
                    (nal.typ, nal.matched)
                };
                let emit =
                    !(seen_irap_ps && matched && (HEVC_NAL_VPS..=HEVC_NAL_PPS).contains(&typ));
                if !emit {
                    omitted += 1;
                }
                self.nals[i].emit = emit;
            }
            if omitted > 0 && !self.dup_ps_logged {
                // FFmpeg logs "Omitting %d extradata parameter sets already
                // present in-band" here (once per extradata generation).
                self.dup_ps_logged = true;
            }
        } else {
            for nal in &mut self.nals {
                nal.emit = false;
            }
        }

        let in_annexb_size: usize = packet_nals.iter().map(|&(_, size, _)| 4 + size).sum();
        let extra_insert_size: usize = self
            .nals
            .iter()
            .filter(|nal| nal.emit)
            .map(|nal| nal.size)
            .sum();
        let total_out_size = in_annexb_size + extra_insert_size;
        let mut out_data = Vec::with_capacity(total_out_size);

        for &(offset, size, nalu_type) in &packet_nals {
            self.flush_extradata_before(&mut out_data, nalu_type, got_irap);

            if nalu_type == HEVC_NAL_SPS {
                self.sps_seen = true;
            }

            // AV_WB32(out->data + out_offset, 1);
            out_data.extend_from_slice(&NALU_HEADER);
            out_data.extend_from_slice(&data[offset..offset + size]);
        }

        if got_irap {
            self.emit_extradata_type(&mut out_data, HEVC_NAL_SEI_SUFFIX);
        }

        debug_assert_eq!(out_data.len(), total_out_size);
        Ok(out_data)
    }
}

impl BitstreamFilter for HevcMp4ToAnnexb {
    fn filter(&mut self, packet: &Packet) -> Result<Vec<Packet>> {
        let data = packet.data.data();

        // FFmpeg: AV_PKT_DATA_NEW_EXTRADATA triggers a re-parse of the extradata
        // when it is an hvcC record, and the side data is removed.
        let mut handled_new_extradata = false;
        for side in &packet.side_data {
            if side.data_type == PacketSideDataType::NewExtradata && is_hvcc(&side.data) {
                self.parse_extradata(&side.data)?;
                handled_new_extradata = true;
                break;
            }
        }

        let mut out_packet = if self.extradata_parsed {
            let out_data = self.filter_data(data)?;
            let mut pkt = packet.clone();
            pkt.data = wedeo_core::buffer::Buffer::from_slice(&out_data);
            pkt
        } else {
            // Nothing to filter: the input is Annex B already (or has no hvcC
            // extradata yet), so pass the packet through.
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
        // FFmpeg's hevc_mp4toannexb has no flush callback: all per-packet state
        // (matched/emit/emitted_types/sps_seen) is reset at the start of every
        // filter() call, so there is nothing buffered to drain here.
        Ok(Vec::new())
    }
}

static DESCRIPTOR: BitstreamFilterDescriptor = BitstreamFilterDescriptor {
    name: "hevc_mp4toannexb",
    long_name: "HEVC MP4 to Annex B byte stream format filter",
    codec_id: CodecId::Hevc,
    priority: 100,
};

/// Factory for the `hevc_mp4toannexb` bitstream filter.
pub struct HevcMp4ToAnnexbFactory;

impl BitstreamFilterFactory for HevcMp4ToAnnexbFactory {
    fn descriptor(&self) -> &BitstreamFilterDescriptor {
        &DESCRIPTOR
    }

    fn create(&self) -> Result<Box<dyn BitstreamFilter>> {
        // The framework's factory has no init parameters; hvcC extradata is
        // supplied by NEW_EXTRADATA packet side data or by constructing
        // `HevcMp4ToAnnexb::with_extradata()` directly.
        Ok(Box::new(HevcMp4ToAnnexb::new()))
    }
}

inventory::submit!(&HevcMp4ToAnnexbFactory as &dyn BitstreamFilterFactory);

#[cfg(test)]
mod tests {
    use super::*;

    // NAL payload bytes. Only the header byte is meaningful to the BSF
    // (type = payload[0] >> 1 & 0x3f); the rest is arbitrary test data.
    fn nal(typ: u8, payload: &[u8]) -> Vec<u8> {
        let mut n = vec![(typ << 1) | 1];
        n.extend_from_slice(payload);
        n
    }

    /// Build a minimal hvcC extradata blob with `length_size`-byte lengths.
    /// Arrays are given as `(nal_type, &[nal_payloads])`.
    fn hvcc(length_size: usize, arrays: &[(u8, &[Vec<u8>])]) -> Vec<u8> {
        let mut e = vec![0u8; MIN_HEVCC_LENGTH];
        e[0] = 1; // configurationVersion
        e[21] = 0xfc | (length_size as u8 - 1); // lengthSizeMinusOne
        e[22] = arrays.len() as u8; // numOfArrays
        for &(typ, nals) in arrays {
            e.push(typ & 0x3f);
            e.extend_from_slice(&(nals.len() as u16).to_be_bytes());
            for nal in nals {
                e.extend_from_slice(&(nal.len() as u16).to_be_bytes());
                e.extend_from_slice(nal);
            }
        }
        e
    }

    /// Wrap raw NAL units in length prefixes.
    fn length_prefixed(length_size: usize, nals: &[Vec<u8>]) -> Vec<u8> {
        let mut out = Vec::new();
        for n in nals {
            match length_size {
                1 => out.push(n.len() as u8),
                2 => out.extend_from_slice(&(n.len() as u16).to_be_bytes()),
                _ => out.extend_from_slice(&(n.len() as u32).to_be_bytes()),
            }
            out.extend_from_slice(n);
        }
        out
    }

    fn packet(data: Vec<u8>) -> Packet {
        Packet::from_slice(&data)
    }

    fn out_data(pkts: Vec<Packet>) -> Vec<u8> {
        assert_eq!(pkts.len(), 1);
        pkts.into_iter().next().unwrap().data.data().to_vec()
    }

    fn sc(nal: &[u8]) -> Vec<u8> {
        [&NALU_HEADER[..], nal].concat()
    }

    #[test]
    fn parse_hvcc_arrays_and_length_size() {
        let vps = nal(HEVC_NAL_VPS, &[1, 2]);
        let sps = nal(HEVC_NAL_SPS, &[3, 4, 5]);
        let pps = nal(HEVC_NAL_PPS, &[6]);
        let sei = nal(HEVC_NAL_SEI_PREFIX, &[7, 8]);
        let ed = hvcc(
            4,
            &[
                (HEVC_NAL_VPS, std::slice::from_ref(&vps)),
                (HEVC_NAL_SPS, std::slice::from_ref(&sps)),
                (HEVC_NAL_PPS, std::slice::from_ref(&pps)),
                (HEVC_NAL_SEI_PREFIX, std::slice::from_ref(&sei)),
            ],
        );

        let bsf = HevcMp4ToAnnexb::with_extradata(&ed).unwrap();
        assert!(bsf.extradata_parsed);
        assert_eq!(bsf.length_size, 4);
        assert_eq!(bsf.nals.len(), 4);
        assert_eq!(bsf.nb_ps, 3);
        // VPS/SPS/PPS got hash-table entries; SEI did not.
        assert!(bsf.nals[0].hash != 0 || bsf.nals[0].size > 4);
        assert_eq!(bsf.nals[3].hash, 0);
        assert_eq!(bsf.nals[3].next, -1);

        let expected_extradata = [sc(&vps), sc(&sps), sc(&pps), sc(&sei)].concat();
        assert_eq!(bsf.extradata, expected_extradata);

        // ps_buckets is a power of two and every PS NAL is reachable.
        assert!(bsf.ps_table_size >= 32 && bsf.ps_table_size.is_power_of_two());
        assert_eq!(bsf.ps_buckets.len(), bsf.ps_table_size);
    }

    #[test]
    fn length_size_from_hvcc_is_honored() {
        for &ls in &[1usize, 2, 4] {
            let sps = nal(HEVC_NAL_SPS, &[0xaa, 0xbb]);
            let ed = hvcc(ls, &[(HEVC_NAL_SPS, std::slice::from_ref(&sps))]);
            let bsf = HevcMp4ToAnnexb::with_extradata(&ed).unwrap();
            assert_eq!(bsf.length_size, ls);
        }
    }

    #[test]
    fn irap_gets_vps_sps_pps_injected() {
        let vps = nal(HEVC_NAL_VPS, &[1]);
        let sps = nal(HEVC_NAL_SPS, &[2]);
        let pps = nal(HEVC_NAL_PPS, &[3]);
        let ed = hvcc(
            4,
            &[
                (HEVC_NAL_VPS, std::slice::from_ref(&vps)),
                (HEVC_NAL_SPS, std::slice::from_ref(&sps)),
                (HEVC_NAL_PPS, std::slice::from_ref(&pps)),
            ],
        );
        let mut bsf = HevcMp4ToAnnexb::with_extradata(&ed).unwrap();

        // NAL type 19 = IDR_W_RADL, an IRAP.
        let idr = nal(19, &[0x99]);
        let pkt = packet(length_prefixed(4, std::slice::from_ref(&idr)));
        let out = out_data(bsf.filter(&pkt).unwrap());

        let expected = [sc(&vps), sc(&sps), sc(&pps), sc(&idr)].concat();
        assert_eq!(out, expected);
    }

    #[test]
    fn non_irap_slice_gets_no_injection() {
        let vps = nal(HEVC_NAL_VPS, &[1]);
        let sps = nal(HEVC_NAL_SPS, &[2]);
        let pps = nal(HEVC_NAL_PPS, &[3]);
        let ed = hvcc(
            4,
            &[
                (HEVC_NAL_VPS, std::slice::from_ref(&vps)),
                (HEVC_NAL_SPS, std::slice::from_ref(&sps)),
                (HEVC_NAL_PPS, std::slice::from_ref(&pps)),
            ],
        );
        let mut bsf = HevcMp4ToAnnexb::with_extradata(&ed).unwrap();

        // TRAIL_R (type 1) is a non-IRAP VCL NAL.
        let slice = nal(1, &[0xaa, 0xbb]);
        let out = out_data(
            bsf.filter(&packet(length_prefixed(4, std::slice::from_ref(&slice))))
                .unwrap(),
        );
        assert_eq!(out, sc(&slice));
    }

    #[test]
    fn in_band_ps_not_duplicated_from_extradata() {
        let vps = nal(HEVC_NAL_VPS, &[1]);
        let sps = nal(HEVC_NAL_SPS, &[2]);
        let pps = nal(HEVC_NAL_PPS, &[3]);
        let ed = hvcc(
            4,
            &[
                (HEVC_NAL_VPS, std::slice::from_ref(&vps)),
                (HEVC_NAL_SPS, std::slice::from_ref(&sps)),
                (HEVC_NAL_PPS, std::slice::from_ref(&pps)),
            ],
        );
        let mut bsf = HevcMp4ToAnnexb::with_extradata(&ed).unwrap();

        // Identical VPS/SPS/PPS appear in-band before the IRAP slice. FFmpeg's
        // hash matching must suppress the extradata copies.
        let idr = nal(19, &[0x99]);
        let pkt = packet(length_prefixed(
            4,
            &[vps.clone(), sps.clone(), pps.clone(), idr.clone()],
        ));
        let out = out_data(bsf.filter(&pkt).unwrap());

        let expected = [sc(&vps), sc(&sps), sc(&pps), sc(&idr)].concat();
        assert_eq!(out, expected);
    }

    #[test]
    fn partial_in_band_ps_only_omits_matching_types() {
        let vps = nal(HEVC_NAL_VPS, &[1]);
        let sps = nal(HEVC_NAL_SPS, &[2]);
        let pps = nal(HEVC_NAL_PPS, &[3]);
        let ed = hvcc(
            4,
            &[
                (HEVC_NAL_VPS, std::slice::from_ref(&vps)),
                (HEVC_NAL_SPS, std::slice::from_ref(&sps)),
                (HEVC_NAL_PPS, std::slice::from_ref(&pps)),
            ],
        );
        let mut bsf = HevcMp4ToAnnexb::with_extradata(&ed).unwrap();

        // Only SPS in-band: VPS and PPS still injected, SPS not duplicated.
        let idr = nal(19, &[0x99]);
        let pkt = packet(length_prefixed(4, &[sps.clone(), idr.clone()]));
        let out = out_data(bsf.filter(&pkt).unwrap());
        let expected = [sc(&vps), sc(&sps), sc(&pps), sc(&idr)].concat();
        assert_eq!(out, expected);
    }

    #[test]
    fn new_extradata_side_data_enables_filtering_and_is_removed() {
        let sps = nal(HEVC_NAL_SPS, &[2]);
        let pps = nal(HEVC_NAL_PPS, &[3]);
        let ed = hvcc(
            2,
            &[
                (HEVC_NAL_SPS, std::slice::from_ref(&sps)),
                (HEVC_NAL_PPS, std::slice::from_ref(&pps)),
            ],
        );
        let mut bsf = HevcMp4ToAnnexb::new();

        let idr = nal(19, &[0x99]);
        let mut pkt = packet(length_prefixed(2, std::slice::from_ref(&idr)));
        pkt.side_data.push(wedeo_core::packet::PacketSideData {
            data_type: PacketSideDataType::NewExtradata,
            data: ed,
        });

        let out = bsf.filter(&pkt).unwrap();
        assert_eq!(out.len(), 1);
        assert!(
            out[0]
                .side_data
                .iter()
                .all(|s| s.data_type != PacketSideDataType::NewExtradata)
        );

        let expected = [sc(&sps), sc(&pps), sc(&idr)].concat();
        assert_eq!(out[0].data.data(), expected.as_slice());
    }

    #[test]
    fn annexb_extradata_passes_packets_through() {
        let mut bsf = HevcMp4ToAnnexb::new();
        bsf.init_extradata(&[0, 0, 0, 1, 0x40, 0x01]).unwrap();
        assert!(!bsf.extradata_parsed);

        let raw = [0, 0, 0, 1, 0x26, 0x01, 0xaa];
        let out = out_data(bsf.filter(&packet(raw.to_vec())).unwrap());
        assert_eq!(out, raw);
    }

    #[test]
    fn two_iraps_emit_extradata_once_then_flush_keeps_state() {
        let vps = nal(HEVC_NAL_VPS, &[1]);
        let sps = nal(HEVC_NAL_SPS, &[2]);
        let pps = nal(HEVC_NAL_PPS, &[3]);
        let ed = hvcc(
            4,
            &[
                (HEVC_NAL_VPS, std::slice::from_ref(&vps)),
                (HEVC_NAL_SPS, std::slice::from_ref(&sps)),
                (HEVC_NAL_PPS, std::slice::from_ref(&pps)),
            ],
        );
        let mut bsf = HevcMp4ToAnnexb::with_extradata(&ed).unwrap();

        let idr = nal(19, &[0x99]);
        let with_ps = [sc(&vps), sc(&sps), sc(&pps), sc(&idr)].concat();
        let first = out_data(
            bsf.filter(&packet(length_prefixed(4, std::slice::from_ref(&idr))))
                .unwrap(),
        );
        assert_eq!(first, with_ps);

        // FFmpeg's emitted_types is reset per packet, so every IRAP packet
        // gets its own copy of the extradata parameter sets.
        let second = out_data(
            bsf.filter(&packet(length_prefixed(4, std::slice::from_ref(&idr))))
                .unwrap(),
        );
        assert_eq!(second, with_ps);

        // flush() drains nothing; state is per-packet.
        assert!(bsf.flush().unwrap().is_empty());
    }

    #[test]
    fn sei_suffix_extradata_emitted_at_irap_end() {
        let vps = nal(HEVC_NAL_VPS, &[1]);
        let sps = nal(HEVC_NAL_SPS, &[2]);
        let pps = nal(HEVC_NAL_PPS, &[3]);
        let sei_pre = nal(HEVC_NAL_SEI_PREFIX, &[4]);
        let sei_suf = nal(HEVC_NAL_SEI_SUFFIX, &[5]);
        let ed = hvcc(
            4,
            &[
                (HEVC_NAL_VPS, std::slice::from_ref(&vps)),
                (HEVC_NAL_SPS, std::slice::from_ref(&sps)),
                (HEVC_NAL_PPS, std::slice::from_ref(&pps)),
                (HEVC_NAL_SEI_PREFIX, std::slice::from_ref(&sei_pre)),
                (HEVC_NAL_SEI_SUFFIX, std::slice::from_ref(&sei_suf)),
            ],
        );
        let mut bsf = HevcMp4ToAnnexb::with_extradata(&ed).unwrap();

        let idr = nal(19, &[0x99]);
        let pkt = packet(length_prefixed(4, std::slice::from_ref(&idr)));
        let out = out_data(bsf.filter(&pkt).unwrap());
        let expected = [
            sc(&vps),
            sc(&sps),
            sc(&pps),
            sc(&sei_pre),
            sc(&idr),
            sc(&sei_suf),
        ]
        .concat();
        assert_eq!(out, expected);
    }

    #[test]
    fn aud_does_not_flush_ps_before_it() {
        let vps = nal(HEVC_NAL_VPS, &[1]);
        let sps = nal(HEVC_NAL_SPS, &[2]);
        let pps = nal(HEVC_NAL_PPS, &[3]);
        let ed = hvcc(
            4,
            &[
                (HEVC_NAL_VPS, std::slice::from_ref(&vps)),
                (HEVC_NAL_SPS, std::slice::from_ref(&sps)),
                (HEVC_NAL_PPS, std::slice::from_ref(&pps)),
            ],
        );
        let mut bsf = HevcMp4ToAnnexb::with_extradata(&ed).unwrap();

        let aud = nal(HEVC_NAL_AUD, &[0x10]);
        let idr = nal(19, &[0x99]);
        let out = out_data(
            bsf.filter(&packet(length_prefixed(4, &[aud.clone(), idr.clone()])))
                .unwrap(),
        );
        // AUD is emitted alone; VPS/SPS/PPS are flushed before the IDR slice.
        let expected = [sc(&aud), sc(&vps), sc(&sps), sc(&pps), sc(&idr)].concat();
        assert_eq!(out, expected);
    }

    #[test]
    fn invalid_extradata_and_packets_are_rejected() {
        // Too short / not an hvcC record.
        assert!(HevcMp4ToAnnexb::with_extradata(&[1, 2, 3]).is_ok()); // Annex B pass-through
        assert_eq!(
            HevcMp4ToAnnexb::parse_extradata(&mut HevcMp4ToAnnexb::new(), &[1, 2, 3]),
            Err(Error::InvalidData)
        );

        // Invalid NAL type in an array.
        let mut ed = hvcc(4, &[(HEVC_NAL_VPS, &[nal(HEVC_NAL_VPS, &[1])])]);
        ed[MIN_HEVCC_LENGTH] = 60; // not VPS/SPS/PPS/SEI
        assert_eq!(
            HevcMp4ToAnnexb::parse_extradata(&mut HevcMp4ToAnnexb::new(), &ed),
            Err(Error::InvalidData)
        );

        // Truncated length-prefixed packet: length says 100, only two follow.
        let sps = nal(HEVC_NAL_SPS, &[2]);
        let ed = hvcc(4, &[(HEVC_NAL_SPS, &[sps])]);
        let mut bsf = HevcMp4ToAnnexb::with_extradata(&ed).unwrap();
        let bad = [0, 0, 0, 100, 0x40, 0x01];
        assert_eq!(
            bsf.filter(&packet(bad.to_vec())).unwrap_err(),
            Error::InvalidData
        );

        // NAL size below the minimum of two.
        let bad = [0, 0, 0, 1, 0x40];
        assert_eq!(
            bsf.filter(&packet(bad.to_vec())).unwrap_err(),
            Error::InvalidData
        );
    }

    #[test]
    fn is_hvcc_detects_records_and_annexb() {
        let mut ed = vec![0u8; MIN_HEVCC_LENGTH];
        ed[0] = 1;
        assert!(is_hvcc(&ed));
        ed[0] = 0;
        ed[1] = 1;
        assert!(is_hvcc(&ed)); // old muxers, configurationVersion 0
        ed[1] = 0;
        ed[2] = 2;
        assert!(is_hvcc(&ed));
        assert!(!is_hvcc(&[0, 0, 1])); // too short / Annex B
    }
}
