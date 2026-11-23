//! The parameter-set container (`HEVCParamSets` in FFmpeg `ps.c`) and NAL
//! stream / extradata decoding entry points (`parse.c`).

use wedeo_core::{Error, Result};

use crate::nal::{NalUnitType, annex_b_split, ebsp_to_rbsp, parse_nal_header};
use crate::pps::{Pps, parse_pps};
use crate::sei::{SeiMessage, parse_sei_message};
use crate::slice::{SliceContext, SliceHeader, parse_slice_header};
use crate::sps::{Sps, parse_sps};
use crate::vps::{Vps, parse_vps};
use crate::{MAX_PPS_COUNT, MAX_SPS_COUNT, MAX_VPS_COUNT};

/// Which kind of parameter set was decoded by [`ParamSets::decode_nal`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParameterSetKind {
    Vps(u8),
    Sps(u32),
    Pps(u32),
}

/// Storage for VPS/SPS/PPS by id, mirroring FFmpeg's `HEVCParamSets`.
#[derive(Debug, Clone)]
pub struct ParamSets {
    pub vps_list: [Option<Vps>; MAX_VPS_COUNT],
    pub sps_list: [Option<Sps>; MAX_SPS_COUNT],
    pub pps_list: [Option<Pps>; MAX_PPS_COUNT],
}

impl Default for ParamSets {
    fn default() -> Self {
        Self {
            vps_list: std::array::from_fn(|_| None),
            sps_list: std::array::from_fn(|_| None),
            pps_list: std::array::from_fn(|_| None),
        }
    }
}

impl ParamSets {
    pub fn new() -> Self {
        Self::default()
    }

    /// Decode a single NAL unit (including its two-byte header), storing any
    /// parameter set it carries. Returns the kind/id of a parameter set if one
    /// was decoded.
    pub fn decode_nal(&mut self, nal: &[u8]) -> Result<Option<ParameterSetKind>> {
        let (header, ebsp) = parse_nal_header(nal)?;
        let rbsp = ebsp_to_rbsp(ebsp);
        match header.nal_unit_type {
            NalUnitType::Vps => {
                let vps = parse_vps(&rbsp)?;
                let id = vps.vps_id as usize;
                self.vps_list[id] = Some(vps);
                Ok(Some(ParameterSetKind::Vps(id as u8)))
            }
            NalUnitType::Sps => {
                let sps = parse_sps(&rbsp, header.nuh_layer_id)?;
                let id = sps.sps_id as usize;
                self.sps_list[id] = Some(sps);
                Ok(Some(ParameterSetKind::Sps(id as u32)))
            }
            NalUnitType::Pps => {
                // FFmpeg requires the referenced SPS to be present.
                // Peek at sps_id first (first ue(v) after pps_id).
                let mut br = crate::BitReader::new(&rbsp);
                let _ = br.read_ue()?; // pps_id
                let sps_id = br.read_ue()? as usize;
                let sps = self
                    .sps_list
                    .get(sps_id)
                    .and_then(|s| s.as_ref())
                    .ok_or(Error::InvalidData)?;
                let pps = parse_pps(&rbsp, sps)?;
                let id = pps.pps_id as usize;
                self.pps_list[id] = Some(pps);
                Ok(Some(ParameterSetKind::Pps(id as u32)))
            }
            _ => Ok(None),
        }
    }

    /// Convenience: decode SEI NAL payloads into typed message headers.
    pub fn decode_sei(&self, nal: &[u8]) -> Result<Vec<SeiMessage>> {
        let (_, ebsp) = parse_nal_header(nal)?;
        parse_sei_message(ebsp)
    }

    /// Build a [`SliceContext`] for a given PPS id.
    pub fn slice_context<'a>(
        &'a self,
        pps_id: u32,
        header: crate::NalUnitHeader,
        poc_tid0: i32,
        last_eos: bool,
        slice_initialized: bool,
    ) -> Result<SliceContext<'a>> {
        let pps = self
            .pps_list
            .get(pps_id as usize)
            .and_then(|p| p.as_ref())
            .ok_or(Error::InvalidData)?;
        let sps = self
            .sps_list
            .get(pps.sps_id as usize)
            .and_then(|s| s.as_ref())
            .ok_or(Error::InvalidData)?;
        let vps = self
            .vps_list
            .get(sps.vps_id as usize)
            .and_then(|v| v.as_ref())
            .ok_or(Error::InvalidData)?;
        Ok(SliceContext {
            nal_unit_type: header.nal_unit_type,
            nuh_layer_id: header.nuh_layer_id,
            poc_tid0,
            last_eos,
            slice_initialized,
            vps,
            sps,
            pps,
        })
    }

    /// Parse a slice header whose PPS id is already known, resolving SPS/VPS
    /// automatically.
    pub fn parse_slice_header(
        &self,
        rbsp: &[u8],
        pps_id: u32,
        header: crate::NalUnitHeader,
        poc_tid0: i32,
        last_eos: bool,
        slice_initialized: bool,
    ) -> Result<SliceHeader> {
        let ctx = self.slice_context(pps_id, header, poc_tid0, last_eos, slice_initialized)?;
        parse_slice_header(rbsp, &ctx)
    }

    /// Decode all NAL units in a byte stream.
    ///
    /// `nal_length_size == 0` selects Annex B start-code splitting; otherwise
    /// NALs are prefixed by a big-endian length of `nal_length_size` bytes.
    pub fn decode_nal_units(
        &mut self,
        data: &[u8],
        nal_length_size: usize,
    ) -> Result<Vec<ParameterSetKind>> {
        let mut nals: Vec<&[u8]> = Vec::new();
        if nal_length_size == 0 {
            nals.extend(annex_b_split(data));
        } else {
            if nal_length_size > 4 {
                return Err(Error::InvalidData);
            }
            let mut pos = 0usize;
            while pos + nal_length_size <= data.len() {
                let mut len = 0usize;
                for &b in &data[pos..pos + nal_length_size] {
                    len = (len << 8) | b as usize;
                }
                pos += nal_length_size;
                if len == 0 || pos + len > data.len() {
                    return Err(Error::InvalidData);
                }
                nals.push(&data[pos..pos + len]);
                pos += len;
            }
        }
        let mut found = Vec::new();
        for nal in nals {
            if let Some(kind) = self.decode_nal(nal)? {
                found.push(kind);
            }
        }
        Ok(found)
    }

    /// Decode extradata, either an Annex B stream or an ISO/IEC 14496-15
    /// `hvcC` configuration record (FFmpeg's `ff_hevc_is_hvcc`).
    pub fn decode_extradata(&mut self, data: &[u8]) -> Result<Vec<ParameterSetKind>> {
        if is_hvcc(data) {
            // hvcC: 22 bytes of header, then lengthSizeMinusOne and numArrays.
            let nal_len_size = (data[21] & 3) as usize + 1;
            let num_arrays = data[22] as usize;
            let mut pos = 23usize;
            let mut found = Vec::new();
            for _ in 0..num_arrays {
                if pos + 3 > data.len() {
                    return Err(Error::InvalidData);
                }
                pos += 1; // array_completeness / nal_unit_type
                let count = u16::from_be_bytes([data[pos], data[pos + 1]]) as usize;
                pos += 2;
                for _ in 0..count {
                    if pos + 2 > data.len() {
                        return Err(Error::InvalidData);
                    }
                    let len = u16::from_be_bytes([data[pos], data[pos + 1]]) as usize;
                    pos += 2;
                    if pos + len > data.len() {
                        return Err(Error::InvalidData);
                    }
                    let nal = &data[pos..pos + len];
                    if let Some(kind) = self.decode_nal(nal)? {
                        found.push(kind);
                    }
                    pos += len;
                }
            }
            // hvcC NAL length field is 2 bytes; the configured
            // `nal_length_size` (data[21]) is returned via `decode_nal_units`
            // by the caller if needed.
            let _ = nal_len_size;
            Ok(found)
        } else {
            self.decode_nal_units(data, 0)
        }
    }
}

/// `ff_hevc_is_hvcc()` from `hevc.h`.
pub fn is_hvcc(data: &[u8]) -> bool {
    data.len() >= 23 && (data[0] == 1 || (data[0] == 0 && (data[1] != 0 || data[2] > 1)))
}

/// Decode all NAL units in an Annex B byte stream into a fresh container.
pub fn decode_nal_units(data: &[u8]) -> Result<ParamSets> {
    let mut ps = ParamSets::new();
    ps.decode_nal_units(data, 0)?;
    Ok(ps)
}

/// Decode extradata into a fresh container.
pub fn decode_extradata(data: &[u8]) -> Result<ParamSets> {
    let mut ps = ParamSets::new();
    ps.decode_extradata(data)?;
    Ok(ps)
}
