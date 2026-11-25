#![forbid(unsafe_code)]
// IVF muxer, translated from FFmpeg's libavformat/ivfenc.c.
//
// File structure (all integers little-endian):
//   [32-byte file header]
//   [12-byte frame header] [frame_size bytes of frame data]
//   ...
//
// The file header is:
//   bytes  0-3 : magic "DKIF"
//   bytes  4-5 : version (0)
//   bytes  6-7 : header length (32)
//   bytes  8-11: fourcc ("VP80", "VP90" or "AV01")
//   bytes 12-13: width
//   bytes 14-15: height
//   bytes 16-19: time_base.den (frames per second numerator)
//   bytes 20-23: time_base.num (frames per second denominator)
//   bytes 24-27: number of frames, overwritten in the trailer
//   bytes 28-31: unused
//
// The frame header is:
//   bytes 0-3 : packet size
//   bytes 4-11: packet pts (raw 64-bit value, AV_NOPTS_VALUE included)
//
// FFmpeg's ivf_init() additionally attaches the `vp9_superframe` and
// `av1_metadata` bitstream filters. The Wedeo `Muxer` trait has no BSF hook,
// so those transforms remain the responsibility of the pipeline that feeds
// packets to this muxer.

use tracing::{error, trace};
use wedeo_core::codec_id::CodecId;
use wedeo_core::error::{Error, Result};
use wedeo_core::packet::Packet;
use wedeo_format::demuxer::Stream;
use wedeo_format::io::BufferedIo;
use wedeo_format::muxer::{Muxer, OutputFormatDescriptor, OutputFormatFlags};
use wedeo_format::registry::MuxerFactory;

const IVF_MAGIC: &[u8; 4] = b"DKIF";
const IVF_FILE_HEADER_LEN: u16 = 32;
const IVF_FRAME_COUNT_OFFSET: u64 = 24;

/// IVF muxer state. Mirrors FFmpeg's `IVFEncContext`.
struct IvfMuxer {
    frame_cnt: u32,
    last_pts: u64,
    last_pkt_duration: i64,
}

impl IvfMuxer {
    fn new() -> Self {
        Self {
            frame_cnt: 0,
            last_pts: 0,
            last_pkt_duration: 0,
        }
    }

    fn fourcc(codec_id: CodecId) -> Option<[u8; 4]> {
        // FFmpeg's codec_ivf_tags table:
        // { AV_CODEC_ID_VP8, MKTAG('V','P','8','0') },
        // { AV_CODEC_ID_VP9, MKTAG('V','P','9','0') },
        // { AV_CODEC_ID_AV1, MKTAG('A','V','0','1') },
        match codec_id {
            CodecId::Vp9 => Some(*b"VP90"),
            CodecId::Vp8 => Some(*b"VP80"),
            CodecId::Av1 => Some(*b"AV01"),
            _ => None,
        }
    }
}

impl Muxer for IvfMuxer {
    fn write_header(&mut self, io: &mut BufferedIo, streams: &[Stream]) -> Result<()> {
        let stream = streams.first().ok_or(Error::StreamNotFound)?;
        let par = &stream.codec_params;

        let fourcc = Self::fourcc(par.codec_id).ok_or_else(|| {
            error!("Currently only VP8, VP9 and AV1 are supported!");
            Error::InvalidArgument
        })?;

        // IVF has no metadata fields and FFmpeg's muxer writes none. Metadata
        // stays in the caller's insertion-ordered Vec<(String, String)> and is
        // never reordered here.
        io.write_bytes(IVF_MAGIC)?;
        io.write_u16le(0)?; // version
        io.write_u16le(IVF_FILE_HEADER_LEN)?; // header length
        io.write_bytes(&fourcc)?;
        io.write_u16le(par.width as u16)?;
        io.write_u16le(par.height as u16)?;
        io.write_u32le(stream.time_base.den as u32)?;
        io.write_u32le(stream.time_base.num as u32)?;
        io.write_u32le(0xFFFF_FFFF)?; // "number of frames" is overwritten at the end of muxing
        io.write_u32le(0)?; // unused

        Ok(())
    }

    fn write_packet(&mut self, io: &mut BufferedIo, packet: &Packet) -> Result<()> {
        let data = packet.data.data();
        io.write_u32le(data.len() as u32)?;
        io.write_u64le(packet.pts as u64)?;
        io.write_bytes(data)?;

        self.last_pkt_duration = packet.duration;
        self.frame_cnt = self.frame_cnt.wrapping_add(1);
        self.last_pts = packet.pts as u64;
        trace!(
            frame_cnt = self.frame_cnt,
            last_pts = self.last_pts,
            last_pkt_duration = self.last_pkt_duration,
            "IVF packet written"
        );

        Ok(())
    }

    fn write_trailer(&mut self, io: &mut BufferedIo) -> Result<()> {
        // overwrite the "number of frames"
        if io.is_seekable() {
            io.flush()?;
            let end = io.tell()?;

            io.seek(IVF_FRAME_COUNT_OFFSET)?;
            io.write_u32le(self.frame_cnt)?;
            io.seek(end)?;
            io.flush()?;
        }

        Ok(())
    }
}

/// Muxer factory registration, following the pattern used by the WAV muxer.
struct IvfMuxerFactory;

impl MuxerFactory for IvfMuxerFactory {
    fn descriptor(&self) -> &OutputFormatDescriptor {
        static DESC: OutputFormatDescriptor = OutputFormatDescriptor {
            name: "ivf",
            long_name: "On2 IVF",
            extensions: "ivf",
            mime_types: "video/x-ivf",
            flags: OutputFormatFlags::empty(),
            audio_codec: CodecId::None,
            video_codec: CodecId::Vp8,
        };
        &DESC
    }

    fn create(&self) -> Result<Box<dyn Muxer>> {
        Ok(Box::new(IvfMuxer::new()))
    }
}

inventory::submit!(&IvfMuxerFactory as &dyn MuxerFactory);

#[cfg(test)]
mod tests {
    use std::io::SeekFrom;
    use std::sync::{Arc, Mutex};

    use wedeo_codec::decoder::CodecParameters;
    use wedeo_core::media_type::MediaType;
    use wedeo_core::rational::Rational;
    use wedeo_format::io::{BufferedIo, IoContext};

    use super::*;

    /// In-memory IoContext. Bytes are shared through an `Arc<Mutex<_>>` so the
    /// test can inspect the output after `BufferedIo` has consumed the writer.
    struct MemoryIo {
        shared: Arc<Mutex<Vec<u8>>>,
        pos: usize,
        seekable: bool,
    }

    impl MemoryIo {
        fn new(seekable: bool) -> (Self, Arc<Mutex<Vec<u8>>>) {
            let shared = Arc::new(Mutex::new(Vec::new()));
            (
                Self {
                    shared: Arc::clone(&shared),
                    pos: 0,
                    seekable,
                },
                shared,
            )
        }
    }

    impl IoContext for MemoryIo {
        fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
            let shared = self.shared.lock().unwrap();
            let remaining = shared.len().saturating_sub(self.pos);
            let n = buf.len().min(remaining);
            buf[..n].copy_from_slice(&shared[self.pos..self.pos + n]);
            self.pos += n;
            Ok(n)
        }

        fn write(&mut self, buf: &[u8]) -> Result<usize> {
            let mut shared = self.shared.lock().unwrap();
            let end = self.pos + buf.len();
            if end > shared.len() {
                shared.resize(end, 0);
            }
            shared[self.pos..end].copy_from_slice(buf);
            self.pos = end;
            Ok(buf.len())
        }

        fn seek(&mut self, pos: SeekFrom) -> Result<u64> {
            assert!(self.seekable, "seek() on non-seekable MemoryIo");
            let shared = self.shared.lock().unwrap();
            let new_pos = match pos {
                SeekFrom::Start(p) => p as i64,
                SeekFrom::End(off) => shared.len() as i64 + off,
                SeekFrom::Current(off) => self.pos as i64 + off,
            };
            self.pos = new_pos.max(0) as usize;
            Ok(self.pos as u64)
        }

        fn tell(&mut self) -> Result<u64> {
            Ok(self.pos as u64)
        }

        fn size(&mut self) -> Result<u64> {
            Ok(self.shared.lock().unwrap().len() as u64)
        }

        fn is_seekable(&self) -> bool {
            self.seekable
        }
    }

    fn stream(codec_id: CodecId, width: u32, height: u32, time_base: Rational) -> Stream {
        let mut params = CodecParameters::new(codec_id, MediaType::Video);
        params.width = width;
        params.height = height;
        let mut stream = Stream::new(0, params);
        stream.time_base = time_base;
        stream
    }

    /// Write a complete IVF file into an in-memory buffer.
    fn write_ivf(
        codec_id: CodecId,
        width: u32,
        height: u32,
        time_base: Rational,
        packets: &[(i64, &[u8])],
        seekable: bool,
    ) -> Vec<u8> {
        let (io, shared) = MemoryIo::new(seekable);
        let mut io = BufferedIo::new(Box::new(io));
        let mut muxer = IvfMuxer::new();
        muxer
            .write_header(&mut io, &[stream(codec_id, width, height, time_base)])
            .unwrap();
        for (pts, payload) in packets {
            let mut packet = Packet::from_slice(payload);
            packet.pts = *pts;
            packet.duration = 1;
            muxer.write_packet(&mut io, &packet).unwrap();
        }
        muxer.write_trailer(&mut io).unwrap();
        io.flush().unwrap();
        shared.lock().unwrap().clone()
    }

    fn le_u32(bytes: &[u8]) -> u32 {
        u32::from_le_bytes(bytes[..4].try_into().unwrap())
    }

    fn le_u64(bytes: &[u8]) -> u64 {
        u64::from_le_bytes(bytes[..8].try_into().unwrap())
    }

    #[test]
    fn header_matches_ffmpeg_layout() {
        let bytes = write_ivf(CodecId::Vp9, 640, 360, Rational::new(1, 30), &[], true);
        assert_eq!(&bytes[0..4], b"DKIF");
        assert_eq!(u16::from_le_bytes([bytes[4], bytes[5]]), 0); // version
        assert_eq!(u16::from_le_bytes([bytes[6], bytes[7]]), 32); // header length
        assert_eq!(&bytes[8..12], b"VP90");
        assert_eq!(u16::from_le_bytes([bytes[12], bytes[13]]), 640);
        assert_eq!(u16::from_le_bytes([bytes[14], bytes[15]]), 360);
        assert_eq!(le_u32(&bytes[16..]), 30); // time_base.den
        assert_eq!(le_u32(&bytes[20..]), 1); // time_base.num
        assert_eq!(le_u32(&bytes[24..]), 0); // frame count patched by trailer
        assert_eq!(le_u32(&bytes[28..]), 0);
        assert_eq!(bytes.len(), 32);
    }

    #[test]
    fn placeholder_frame_count_without_trailer() {
        let (io, shared) = MemoryIo::new(true);
        let mut io = BufferedIo::new(Box::new(io));
        IvfMuxer::new()
            .write_header(
                &mut io,
                &[stream(CodecId::Vp8, 16, 16, Rational::new(1, 30))],
            )
            .unwrap();
        io.flush().unwrap();
        let bytes = shared.lock().unwrap();
        assert_eq!(le_u32(&bytes[24..]), 0xFFFF_FFFF);
    }

    #[test]
    fn fourcc_mapping() {
        let cases = [
            (CodecId::Vp8, *b"VP80"),
            (CodecId::Vp9, *b"VP90"),
            (CodecId::Av1, *b"AV01"),
        ];
        for (codec_id, expected) in cases {
            let bytes = write_ivf(codec_id, 64, 48, Rational::new(1, 25), &[], true);
            assert_eq!(&bytes[8..12], &expected, "fourcc for {codec_id:?}");
        }
    }

    #[test]
    fn rejects_unsupported_codec_and_empty_streams() {
        let (io, _) = MemoryIo::new(true);
        let mut io = BufferedIo::new(Box::new(io));
        let mut muxer = IvfMuxer::new();

        assert_eq!(
            muxer.write_header(&mut io, &[]).err(),
            Some(Error::StreamNotFound)
        );
        assert_eq!(
            muxer
                .write_header(
                    &mut io,
                    &[stream(CodecId::H264, 16, 16, Rational::new(1, 30))]
                )
                .err(),
            Some(Error::InvalidArgument)
        );
    }

    #[test]
    fn packets_and_frame_count_patch() {
        let bytes = write_ivf(
            CodecId::Vp8,
            16,
            16,
            Rational::new(1, 30),
            &[(0, b"aa"), (7, b"bbb"), (14, b"cccc")],
            true,
        );
        assert_eq!(bytes.len(), 32 + 3 * 12 + 9);
        assert_eq!(le_u32(&bytes[24..]), 3, "frame count patched in trailer");
        assert_eq!(le_u32(&bytes[32..]), 2);
        assert_eq!(le_u64(&bytes[36..]), 0);
        assert_eq!(&bytes[44..46], b"aa");
        assert_eq!(le_u32(&bytes[46..]), 3);
        assert_eq!(le_u64(&bytes[50..]), 7);
        assert_eq!(&bytes[58..61], b"bbb");
        assert_eq!(le_u32(&bytes[61..]), 4);
        assert_eq!(le_u64(&bytes[65..]), 14);
        assert_eq!(&bytes[73..77], b"cccc");
    }

    #[test]
    fn raw_pts_bits_and_non_seekable_trailer() {
        let bytes = write_ivf(
            CodecId::Av1,
            16,
            16,
            Rational::new(1, 30),
            &[(i64::MIN, b"x")],
            false,
        );
        assert_eq!(le_u32(&bytes[32..]), 1);
        // AV_NOPTS_VALUE: FFmpeg writes the raw 64-bit pattern.
        assert_eq!(le_u64(&bytes[36..]), i64::MIN as u64);
        // Non-seekable output keeps the placeholder frame count.
        assert_eq!(le_u32(&bytes[24..]), 0xFFFF_FFFF);
    }

    #[test]
    fn factory_descriptor() {
        let factory = IvfMuxerFactory;
        let desc = factory.descriptor();
        assert_eq!(desc.name, "ivf");
        assert_eq!(desc.long_name, "On2 IVF");
        assert_eq!(desc.extensions, "ivf");
        assert_eq!(desc.mime_types, "video/x-ivf");
        assert_eq!(desc.video_codec, CodecId::Vp8);
        assert_eq!(desc.audio_codec, CodecId::None);
        assert!(factory.create().is_ok());
    }
}
