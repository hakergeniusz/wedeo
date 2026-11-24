//! Minimal AMF0 (Action Message Format 0) reader used by the FLV demuxer.
//!
//! Translated from FFmpeg's `libavformat/flvdec.c`: `amf_get_string()`,
//! `amf_parse_object()` and `amf_skip_tag()`.  Only the value types needed to
//! read `onMetaData` and to skip over unknown tags are decoded into an owned
//! tree; unsupported types are reported as errors just like FFmpeg.
//!
//! AMF0 type constants are from `libavformat/flv.h`.

use wedeo_core::error::{Error, Result};

/// AMF0 type markers (`AMFDataType` in `libavformat/flv.h`).
const AMF_DATA_TYPE_NUMBER: u8 = 0x00;
const AMF_DATA_TYPE_BOOL: u8 = 0x01;
const AMF_DATA_TYPE_STRING: u8 = 0x02;
const AMF_DATA_TYPE_OBJECT: u8 = 0x03;
const AMF_DATA_TYPE_NULL: u8 = 0x05;
const AMF_DATA_TYPE_UNDEFINED: u8 = 0x06;
#[allow(dead_code)] // AMF_DATA_TYPE_REFERENCE is reserved and never emitted in practice.
const AMF_DATA_TYPE_REFERENCE: u8 = 0x07;
const AMF_DATA_TYPE_MIXEDARRAY: u8 = 0x08;
const AMF_DATA_TYPE_OBJECT_END: u8 = 0x09;
const AMF_DATA_TYPE_ARRAY: u8 = 0x0a;
const AMF_DATA_TYPE_DATE: u8 = 0x0b;
const AMF_DATA_TYPE_LONG_STRING: u8 = 0x0c;

/// `MAX_DEPTH 16 ///< arbitrary limit to prevent unbounded recursion` (flvdec.c:48).
const MAX_DEPTH: u32 = 16;

/// A decoded AMF0 value.
#[derive(Debug, Clone, PartialEq)]
pub enum AmfValue {
    Number(f64),
    Bool(bool),
    String(String),
    Object(Vec<(String, AmfValue)>),
    Null,
    Undefined,
    MixedArray(Vec<(String, AmfValue)>),
    Array(Vec<AmfValue>),
    /// milliseconds since the Unix epoch and UTC offset in minutes.
    Date(f64, i16),
}

impl AmfValue {
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Self::Number(v) => Some(*v),
            Self::Bool(v) => Some(if *v { 1.0 } else { 0.0 }),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(v) => Some(v),
            _ => None,
        }
    }

    /// Object-like values carry string-keyed properties.
    pub fn as_entries(&self) -> Option<&[(String, AmfValue)]> {
        match self {
            Self::Object(e) | Self::MixedArray(e) => Some(e),
            _ => None,
        }
    }
}

/// Cursor over an AMF0 byte slice.
pub struct AmfReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> AmfReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    /// Current cursor offset.  Used by the unit tests.
    #[cfg(test)]
    pub fn pos(&self) -> usize {
        self.pos
    }

    /// `avio_feof()` equivalent for our bounded slice.
    pub fn eof(&self) -> bool {
        self.pos >= self.data.len()
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.pos.checked_add(n).ok_or(Error::InvalidData)?;
        let slice = self.data.get(self.pos..end).ok_or(Error::InvalidData)?;
        self.pos = end;
        Ok(slice)
    }

    fn read_u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn read_u16be(&mut self) -> Result<u16> {
        let b = self.take(2)?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }

    fn read_u32be(&mut self) -> Result<u32> {
        let b = self.take(4)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn read_f64be(&mut self) -> Result<f64> {
        let b = self.take(8)?;
        Ok(f64::from_bits(u64::from_be_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ])))
    }

    /// `amf_get_string()`: a big-endian u16 length followed by raw bytes.
    fn read_string(&mut self) -> Result<String> {
        let len = self.read_u16be()? as usize;
        let bytes = self.take(len)?;
        Ok(String::from_utf8_lossy(bytes).into_owned())
    }

    fn skip(&mut self, n: usize) -> Result<()> {
        self.take(n)?;
        Ok(())
    }

    /// Parse one complete AMF0 value (including its type byte).
    pub fn read_value(&mut self) -> Result<AmfValue> {
        self.read_value_inner(0)
    }

    fn read_value_inner(&mut self, depth: u32) -> Result<AmfValue> {
        if depth > MAX_DEPTH {
            return Err(Error::PatchwelcomeNotImplemented);
        }
        if self.eof() {
            return Err(Error::Eof);
        }
        let amf_type = self.read_u8()?;
        match amf_type {
            AMF_DATA_TYPE_NUMBER => Ok(AmfValue::Number(self.read_f64be()?)),
            AMF_DATA_TYPE_BOOL => Ok(AmfValue::Bool(self.read_u8()? != 0)),
            AMF_DATA_TYPE_STRING => Ok(AmfValue::String(self.read_string()?)),
            AMF_DATA_TYPE_NULL => Ok(AmfValue::Null),
            AMF_DATA_TYPE_UNDEFINED => Ok(AmfValue::Undefined),
            AMF_DATA_TYPE_REFERENCE => {
                // Not handled by FFmpeg's parser; skip the 16-bit reference.
                self.skip(2)?;
                Ok(AmfValue::Null)
            }
            AMF_DATA_TYPE_OBJECT => {
                let entries = self.read_object_entries(depth)?;
                Ok(AmfValue::Object(entries))
            }
            AMF_DATA_TYPE_MIXEDARRAY => {
                // `avio_skip(ioc, 4); // skip 32-bit max array index`
                let _ = self.read_u32be()?;
                let entries = self.read_object_entries(depth)?;
                Ok(AmfValue::MixedArray(entries))
            }
            AMF_DATA_TYPE_ARRAY => {
                let arraylen = self.read_u32be()? as usize;
                // Guard against absurd counts (FFmpeg bounds it against max_pos).
                if arraylen > self.data.len().saturating_sub(self.pos) {
                    return Err(Error::InvalidData);
                }
                let mut values = Vec::with_capacity(arraylen);
                for _ in 0..arraylen {
                    values.push(self.read_value_inner(depth + 1)?);
                }
                Ok(AmfValue::Array(values))
            }
            AMF_DATA_TYPE_DATE => {
                let milliseconds = self.read_f64be()?;
                let timezone = self.read_u16be()? as i16;
                Ok(AmfValue::Date(milliseconds, timezone))
            }
            AMF_DATA_TYPE_LONG_STRING => {
                let len = self.read_u32be()? as usize;
                let bytes = self.take(len)?;
                Ok(AmfValue::String(
                    String::from_utf8_lossy(bytes).into_owned(),
                ))
            }
            // AMF_DATA_TYPE_OBJECT_END appears only as the terminator, consumed by
            // `read_object_entries`; treat a stray one as an empty object.
            AMF_DATA_TYPE_OBJECT_END => Ok(AmfValue::Object(Vec::new())),
            _ => {
                // `default: av_log(... "unsupported amf type %d") ; return -1;`
                Err(Error::InvalidData)
            }
        }
    }

    /// Read the `(key, value)` pairs of an OBJECT or MIXEDARRAY, consuming the
    /// trailing `AMF_END_OF_OBJECT` marker (0x09).
    fn read_object_entries(&mut self, depth: u32) -> Result<Vec<(String, AmfValue)>> {
        let mut entries = Vec::new();
        loop {
            if self.eof() {
                return Err(Error::InvalidData);
            }
            let key = self.read_string()?;
            if key.is_empty() {
                break;
            }
            let value = self.read_value_inner(depth + 1)?;
            entries.push((key, value));
        }
        let marker = self.read_u8()?;
        if marker != AMF_DATA_TYPE_OBJECT_END {
            // `Missing AMF_END_OF_OBJECT in AMF_DATA_TYPE_OBJECT`
            return Err(Error::InvalidData);
        }
        Ok(entries)
    }

    /// Skip an AMF value whose type byte has already been consumed.
    /// Mirrors `amf_skip_tag()` for the script-data packet path.
    #[cfg(test)]
    pub fn skip_tag(&mut self, amf_type: u8, depth: u32) -> Result<()> {
        if depth > MAX_DEPTH {
            return Err(Error::PatchwelcomeNotImplemented);
        }
        if self.eof() {
            return Err(Error::Eof);
        }
        match amf_type {
            AMF_DATA_TYPE_NUMBER => self.skip(8),
            AMF_DATA_TYPE_BOOL => self.skip(1),
            AMF_DATA_TYPE_STRING => {
                let len = self.read_u16be()? as usize;
                self.skip(len)
            }
            AMF_DATA_TYPE_REFERENCE => self.skip(2),
            AMF_DATA_TYPE_ARRAY | AMF_DATA_TYPE_MIXEDARRAY => {
                let nb = self.read_u32be()?;
                let parse_name = amf_type != AMF_DATA_TYPE_ARRAY;
                let mut remaining = nb as i64;
                while !self.eof() && (remaining > 0 || !parse_name) {
                    if parse_name {
                        let size = self.read_u16be()? as usize;
                        if size == 0 {
                            self.skip(1)?;
                            break;
                        }
                        self.skip(size)?;
                    }
                    let t = self.read_u8()?;
                    self.skip_tag(t, depth + 1)?;
                    if parse_name {
                        remaining -= 1;
                    }
                }
                Ok(())
            }
            AMF_DATA_TYPE_OBJECT => {
                while !self.eof() {
                    let size = self.read_u16be()? as usize;
                    if size == 0 {
                        self.skip(1)?;
                        break;
                    }
                    self.skip(size)?;
                    let t = self.read_u8()?;
                    self.skip_tag(t, depth + 1)?;
                }
                Ok(())
            }
            AMF_DATA_TYPE_NULL | AMF_DATA_TYPE_OBJECT_END => Ok(()),
            _ => Err(Error::InvalidData),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn number_and_string() {
        let mut data = Vec::new();
        data.push(AMF_DATA_TYPE_STRING);
        data.extend_from_slice(&3u16.to_be_bytes());
        data.extend_from_slice(b"abc");
        data.push(AMF_DATA_TYPE_NUMBER);
        data.extend_from_slice(&1.5f64.to_bits().to_be_bytes());
        let mut r = AmfReader::new(&data);
        assert_eq!(r.read_value().unwrap(), AmfValue::String("abc".into()));
        assert_eq!(r.read_value().unwrap(), AmfValue::Number(1.5));
        assert!(r.eof());
    }

    #[test]
    fn object_entries_and_end_marker() {
        let mut data = Vec::new();
        data.push(AMF_DATA_TYPE_OBJECT);
        // key "k" -> number 2.0
        data.extend_from_slice(&1u16.to_be_bytes());
        data.push(b'k');
        data.push(AMF_DATA_TYPE_NUMBER);
        data.extend_from_slice(&2.0f64.to_bits().to_be_bytes());
        // end of object: empty key + 0x09
        data.extend_from_slice(&0u16.to_be_bytes());
        data.push(AMF_DATA_TYPE_OBJECT_END);

        let v = AmfReader::new(&data).read_value().unwrap();
        let entries = v.as_entries().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, "k");
        assert_eq!(entries[0].1.as_f64(), Some(2.0));
    }

    #[test]
    fn skip_tag_number_and_string() {
        let mut data = vec![AMF_DATA_TYPE_NUMBER];
        data.extend_from_slice(&7u64.to_be_bytes());
        data.push(AMF_DATA_TYPE_STRING);
        data.extend_from_slice(&2u16.to_be_bytes());
        data.extend_from_slice(b"hi");
        let mut r = AmfReader::new(&data);
        // FFmpeg reads the type byte before calling amf_skip_tag().
        let t = r.read_u8().unwrap();
        r.skip_tag(t, 0).unwrap();
        assert_eq!(r.pos(), 9);
        let t = r.read_u8().unwrap();
        r.skip_tag(t, 0).unwrap();
        assert!(r.eof());
    }
}
