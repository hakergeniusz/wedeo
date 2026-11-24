//! Minimal EBML writer, translated from the helpers in FFmpeg's
//! `libavformat/matroskaenc.c` (lines 288-454).
//!
//! EBML uses big-endian element IDs and "vints" (variable-length integers)
//! for sizes and numbers. The leading one bit of the first byte marks the
//! length of the vint; the remaining bits are the value.

/// Number of bytes taken up by an EBML element ID. FFmpeg:
/// `ebml_id_size(id) = (av_log2(id) + 7) / 8`.
pub fn id_size(id: u32) -> usize {
    if id == 0 {
        return 1;
    }
    (32 - id.leading_zeros() as usize).div_ceil(8)
}

/// Write an EBML element ID, big-endian, using the minimal number of bytes.
pub fn put_id(buf: &mut Vec<u8>, id: u32) {
    let n = id_size(id);
    for i in (0..n).rev() {
        buf.push((id >> (i * 8)) as u8);
    }
}

/// Number of bytes needed to write `num` as an EBML vint. FFmpeg:
/// `ebml_num_size()`.
pub fn num_size(mut num: u64) -> usize {
    let mut bytes = 0;
    loop {
        bytes += 1;
        num >>= 7;
        if num == 0 {
            return bytes;
        }
    }
}

/// Number of bytes required for the length field of an element whose payload
/// is `len` bytes. FFmpeg: `ebml_length_size(len) = ebml_num_size(len + 1)`.
pub fn length_size(len: u64) -> usize {
    num_size(len.wrapping_add(1))
}

/// Write `num` as an EBML vint occupying exactly `bytes` bytes. FFmpeg:
/// `put_ebml_num()`. The marker bit is added to the leading byte.
pub fn put_num(buf: &mut Vec<u8>, num: u64, bytes: usize) {
    debug_assert!((1..=8).contains(&bytes));
    let marked = num | (1u64 << (bytes * 7));
    for i in (0..bytes).rev() {
        buf.push((marked >> (i * 8)) as u8);
    }
}

/// Write a length (element payload size) using the minimal vint width.
/// FFmpeg: `put_ebml_length(pb, length, 0)`.
pub fn put_length(buf: &mut Vec<u8>, len: u64) {
    put_num(buf, len, length_size(len));
}

/// Write a vint meaning "unknown size" occupying `bytes` bytes. FFmpeg:
/// `put_ebml_size_unknown()`.
pub fn put_unknown_size(buf: &mut Vec<u8>, bytes: usize) {
    debug_assert!((1..=8).contains(&bytes));
    buf.push((0x1ff >> bytes) as u8);
    for _ in 1..bytes {
        buf.push(0xff);
    }
}

/// Write an unsigned integer element using the minimal number of payload
/// bytes. FFmpeg: `put_ebml_uint()`.
pub fn put_uint(buf: &mut Vec<u8>, id: u32, val: u64) {
    let bytes = uint_size(val);
    put_id(buf, id);
    put_num(buf, bytes as u64, 1);
    for i in (0..bytes).rev() {
        buf.push((val >> (i * 8)) as u8);
    }
}

/// Number of bytes needed for an unsigned integer payload. FFmpeg's
/// `uint_size()`.
fn uint_size(mut val: u64) -> usize {
    let mut bytes = 1;
    loop {
        val >>= 8;
        if val == 0 {
            return bytes;
        }
        bytes += 1;
    }
}

/// Write a signed integer element using the minimal two's-complement width.
/// FFmpeg: `sint_size(val) = uint_size(2 * (val < 0 ? val ^ -1 : val))`.
pub fn put_sint(buf: &mut Vec<u8>, id: u32, val: i64) {
    let tmp = if val < 0 {
        2u64.wrapping_mul((!val) as u64)
    } else {
        2u64.wrapping_mul(val as u64)
    };
    let bytes = uint_size(tmp);
    put_id(buf, id);
    put_num(buf, bytes as u64, 1);
    for i in (0..bytes).rev() {
        buf.push((val >> (i * 8)) as u8);
    }
}

/// Write an IEEE-754 double as an 8-byte big-endian payload. FFmpeg:
/// `put_ebml_float()`.
pub fn put_float(buf: &mut Vec<u8>, id: u32, val: f64) {
    put_id(buf, id);
    put_num(buf, 8, 1);
    buf.extend_from_slice(&val.to_be_bytes());
}

/// Write a UID element as a fixed eight-byte big-endian payload. FFmpeg:
/// `put_ebml_uid()`.
pub fn put_uid(buf: &mut Vec<u8>, id: u32, uid: u64) {
    put_id(buf, id);
    put_num(buf, 8, 1);
    buf.extend_from_slice(&uid.to_be_bytes());
}

/// Write a binary element (also used for strings). FFmpeg:
/// `put_ebml_binary()` / `put_ebml_string()`.
pub fn put_binary(buf: &mut Vec<u8>, id: u32, data: &[u8]) {
    put_id(buf, id);
    put_length(buf, data.len() as u64);
    buf.extend_from_slice(data);
}

/// Write an EBML Void element of at least `size` total bytes. FFmpeg:
/// `put_ebml_void()`. Sizes below 10 use a one-byte length field, larger
/// ones always use eight bytes.
pub fn put_void(buf: &mut Vec<u8>, size: usize) {
    debug_assert!(size >= 2);
    put_id(buf, ID_VOID);
    if size < 10 {
        put_num(buf, (size - 2) as u64, 1);
        buf.resize(buf.len() + size - 2, 0);
    } else {
        let payload = size - 9;
        put_num(buf, payload as u64, 8);
        buf.resize(buf.len() + payload, 0);
    }
}

/// Append a six-byte CRC-32 placeholder (`BF 84 00 00 00 00`) like FFmpeg's
/// `start_ebml_master_crc32()`. Returns the offset of the four value bytes.
pub fn put_crc_placeholder(buf: &mut Vec<u8>) -> usize {
    put_id(buf, ID_CRC32);
    put_num(buf, 4, 1);
    let offset = buf.len();
    buf.extend_from_slice(&[0; 4]);
    offset
}

/// Finalize a CRC-32 placeholder: `data[..crc_offset]` must contain the CRC
/// element followed by the element payload. The CRC is computed over the
/// payload only, exactly as in FFmpeg's `end_ebml_master_crc32()`.
pub fn finalize_crc(buf: &mut [u8], crc_offset: usize) {
    let crc = crc32(&buf[crc_offset + 4..]);
    buf[crc_offset..crc_offset + 4].copy_from_slice(&crc.to_le_bytes());
}

/// CRC-32 (IEEE, reflected polynomial 0xEDB88320). FFmpeg uses
/// `AV_CRC_32_IEEE_LE` with initial value `UINT32_MAX` and a final xor, which
/// is the standard zlib/PNG CRC-32.
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xedb8_8320 & mask);
        }
    }
    !crc
}

// --- EBML IDs -------------------------------------------------------------

pub const ID_EBML: u32 = 0x1A45DFA3;
pub const ID_EBML_VERSION: u32 = 0x4286;
pub const ID_EBML_READ_VERSION: u32 = 0x42F7;
pub const ID_EBML_MAX_ID_LENGTH: u32 = 0x42F2;
pub const ID_EBML_MAX_SIZE_LENGTH: u32 = 0x42F3;
pub const ID_DOCTYPE: u32 = 0x4282;
pub const ID_DOCTYPE_VERSION: u32 = 0x4287;
pub const ID_DOCTYPE_READ_VERSION: u32 = 0x4285;
pub const ID_VOID: u32 = 0xEC;
pub const ID_CRC32: u32 = 0xBF;

pub const ID_SEGMENT: u32 = 0x18538067;
pub const ID_SEEKHEAD: u32 = 0x114D9B74;
pub const ID_SEEK: u32 = 0x4DBB;
pub const ID_SEEK_ID: u32 = 0x53AB;
pub const ID_SEEK_POSITION: u32 = 0x53AC;
pub const ID_INFO: u32 = 0x1549A966;
pub const ID_TIMECODE_SCALE: u32 = 0x2AD7B1;
pub const ID_DURATION: u32 = 0x4489;
pub const ID_MUXING_APP: u32 = 0x4D80;
pub const ID_WRITING_APP: u32 = 0x5741;
pub const ID_TRACKS: u32 = 0x1654AE6B;
pub const ID_TRACK_ENTRY: u32 = 0xAE;
pub const ID_TRACK_NUMBER: u32 = 0xD7;
pub const ID_TRACK_UID: u32 = 0x73C5;
pub const ID_TRACK_TYPE: u32 = 0x83;
pub const ID_TRACK_LANGUAGE: u32 = 0x22B59C;
pub const ID_TRACK_FLAG_DEFAULT: u32 = 0x88;
pub const ID_TRACK_FLAG_LACING: u32 = 0x9C;
pub const ID_CODEC_ID: u32 = 0x86;
pub const ID_CODEC_PRIVATE: u32 = 0x63A2;
pub const ID_TRACK_VIDEO: u32 = 0xE0;
pub const ID_VIDEO_PIXEL_WIDTH: u32 = 0xB0;
pub const ID_VIDEO_PIXEL_HEIGHT: u32 = 0xBA;
pub const ID_VIDEO_DISPLAY_UNIT: u32 = 0x54B2;
pub const ID_TRACK_AUDIO: u32 = 0xE1;
pub const ID_AUDIO_CHANNELS: u32 = 0x9F;
pub const ID_AUDIO_SAMPLING_FREQUENCY: u32 = 0xB5;
pub const ID_AUDIO_BIT_DEPTH: u32 = 0x6264;
pub const ID_TRACK_MAX_BLOCK_ADDITION_ID: u32 = 0x55EE;
pub const ID_CLUSTER: u32 = 0x1F43B675;
pub const ID_CLUSTER_TIMECODE: u32 = 0xE7;
pub const ID_SIMPLE_BLOCK: u32 = 0xA3;
pub const ID_BLOCK_GROUP: u32 = 0xA0;
pub const ID_BLOCK: u32 = 0xA1;
pub const ID_BLOCK_DURATION: u32 = 0x9B;
pub const ID_CUES: u32 = 0x1C53BB6B;
pub const ID_POINT_ENTRY: u32 = 0xBB;
pub const ID_CUE_TIME: u32 = 0xB3;
pub const ID_CUE_TRACK_POSITION: u32 = 0xB7;
pub const ID_CUE_TRACK: u32 = 0xF7;
pub const ID_CUE_CLUSTER_POSITION: u32 = 0xF1;
pub const ID_CUE_RELATIVE_POSITION: u32 = 0xF0;
pub const ID_TAGS: u32 = 0x1254C367;
pub const ID_TAG: u32 = 0x7373;
pub const ID_SIMPLE_TAG: u32 = 0x67C8;
pub const ID_TAG_NAME: u32 = 0x45A3;
pub const ID_TAG_STRING: u32 = 0x4487;
pub const ID_TARGETS: u32 = 0x63C0;
pub const ID_TAG_TRACK_UID: u32 = 0x63C5;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_sizes_and_encoding() {
        assert_eq!(id_size(ID_VOID), 1);
        assert_eq!(id_size(ID_CODEC_ID), 1);
        assert_eq!(id_size(ID_CRC32), 1);
        assert_eq!(id_size(ID_EBML), 4);
        let mut buf = Vec::new();
        put_id(&mut buf, ID_SEGMENT);
        assert_eq!(buf, [0x18, 0x53, 0x80, 0x67]);
    }

    #[test]
    fn vint_encoding_matches_ffmpeg() {
        let cases = [
            (0u64, 1, vec![0x80]),
            (1, 1, vec![0x81]),
            (126, 1, vec![0xfe]),
            (127, 2, vec![0x40, 0x7f]),
            (16382, 2, vec![0x7f, 0xfe]),
            (16383, 3, vec![0x20, 0x3f, 0xff]),
            (1, 8, vec![0x01, 0, 0, 0, 0, 0, 0, 0x01]),
        ];
        for (num, bytes, expected) in cases {
            let mut buf = Vec::new();
            put_num(&mut buf, num, bytes);
            assert_eq!(buf, expected, "num={num} bytes={bytes}");
        }
        assert_eq!(length_size(126), 1);
        assert_eq!(length_size(127), 2);
        assert_eq!(num_size(127), 1);
        assert_eq!(num_size(128), 2);
    }

    #[test]
    fn length_and_unknown_size() {
        let mut buf = Vec::new();
        put_length(&mut buf, 35);
        assert_eq!(buf, [0xa3]);
        buf.clear();
        put_unknown_size(&mut buf, 8);
        assert_eq!(buf, [0x01, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff]);
    }

    #[test]
    fn uint_element_widths() {
        let mut buf = Vec::new();
        put_uint(&mut buf, ID_TRACK_NUMBER, 1);
        assert_eq!(buf, [0xd7, 0x81, 0x01]);
        buf.clear();
        put_uid(&mut buf, ID_TRACK_UID, 1);
        assert_eq!(buf, [0x73, 0xc5, 0x88, 0, 0, 0, 0, 0, 0, 0, 1]);
        buf.clear();
        put_uint(&mut buf, ID_TIMECODE_SCALE, 1_000_000);
        assert_eq!(buf, [0x2a, 0xd7, 0xb1, 0x83, 0x0f, 0x42, 0x40]);
    }

    #[test]
    fn crc_matches_zlib_ieee() {
        // Standard CRC-32 test vector ("123456789" -> 0xCBF43926).
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
        let mut buf = vec![0xbf, 0x84, 0, 0, 0, 0];
        buf.extend_from_slice(b"123456789");
        finalize_crc(&mut buf, 2);
        assert_eq!(&buf[2..6], &0xcbf4_3926u32.to_le_bytes());
    }

    #[test]
    fn void_sizes() {
        let mut buf = Vec::new();
        put_void(&mut buf, 11);
        assert_eq!(buf.len(), 11);
        assert_eq!(buf[0], 0xec);
        assert_eq!(&buf[1..9], &[0x01, 0, 0, 0, 0, 0, 0, 0x02]);
        buf.clear();
        put_void(&mut buf, 161);
        assert_eq!(buf.len(), 161);
        assert_eq!(&buf[1..9], &[0x01, 0, 0, 0, 0, 0, 0, 0x98]);
    }
}
