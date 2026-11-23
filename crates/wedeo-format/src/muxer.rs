use bitflags::bitflags;

use wedeo_core::codec_id::CodecId;
use wedeo_core::error::Result;
use wedeo_core::packet::Packet;

use wedeo_codec::bitstream_filter::{BitstreamFilter, create_initialized_bitstream_filter};
use wedeo_codec::decoder::CodecParameters;

use crate::demuxer::Stream;
use crate::io::BufferedIo;

bitflags! {
    /// Output format flags.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct OutputFormatFlags: u32 {
        const NOFILE        = 1 << 0;
        const NEEDNUMBER    = 1 << 1;
        const GLOBALHEADER  = 1 << 6;
        const NOTIMESTAMPS  = 1 << 7;
        const VARIABLE_FPS  = 1 << 10;
        const NODIMENSIONS  = 1 << 11;
        const NOSTREAMS     = 1 << 12;
        const TS_NONSTRICT  = 1 << 17;
        const TS_NEGATIVE   = 1 << 18;
    }
}

/// Muxer trait — the main abstraction for all muxers.
pub trait Muxer: Send {
    /// Write the file header.
    fn write_header(&mut self, io: &mut BufferedIo, streams: &[Stream]) -> Result<()>;

    /// Write a single packet.
    fn write_packet(&mut self, io: &mut BufferedIo, packet: &Packet) -> Result<()>;

    /// Write the file trailer (finalize).
    fn write_trailer(&mut self, io: &mut BufferedIo) -> Result<()>;

    /// Names of the per-stream bitstream filters FFmpeg applies before muxing
    /// (e.g. `h264_mp4toannexb`, `vp9_superframe`, `aac_adtstoasc`).
    ///
    /// Additive default: an empty slice, so every existing muxer compiles
    /// unchanged. Use [`create_bitstream_filters`] to instantiate the chain and
    /// [`apply_filters`] to run packets through it before `write_packet()`.
    fn bitstream_filters(&self) -> &'static [&'static str] {
        &[]
    }
}

/// Instantiate and initialize the bitstream filter chain declared by a muxer
/// for a stream. Mirrors FFmpeg's per-stream BSF list built in
/// `avformat_write_header()` via `av_bsf_list_parse_str()` / `av_bsf_init()`.
pub fn create_bitstream_filters(
    names: &[&str],
    params: &CodecParameters,
) -> Result<Vec<Box<dyn BitstreamFilter>>> {
    names
        .iter()
        .map(|name| create_initialized_bitstream_filter(name, params))
        .collect()
}

/// Run a packet through an initialized bitstream filter chain. Each filter may
/// emit zero or more packets; every output is fed to the next filter, and the
/// final packets are ready for [`Muxer::write_packet`].
///
/// An empty output stops the chain early (a filter that buffers or drops the
/// packet produces nothing for this call, matching FFmpeg's `EAGAIN`).
pub fn apply_filters(
    filters: &mut [Box<dyn BitstreamFilter>],
    packet: &Packet,
) -> Result<Vec<Packet>> {
    let mut packets = vec![packet.clone()];
    for filter in filters.iter_mut() {
        let mut next = Vec::new();
        for pkt in &packets {
            next.extend(filter.filter(pkt)?);
        }
        packets = next;
        if packets.is_empty() {
            break;
        }
    }
    Ok(packets)
}

/// Descriptor for an output format.
#[derive(Debug, Clone)]
pub struct OutputFormatDescriptor {
    pub name: &'static str,
    pub long_name: &'static str,
    pub extensions: &'static str,
    pub mime_types: &'static str,
    pub flags: OutputFormatFlags,
    pub audio_codec: CodecId,
    pub video_codec: CodecId,
}

#[cfg(test)]
mod tests {
    use wedeo_core::buffer::Buffer;
    use wedeo_core::error::Error;
    use wedeo_core::media_type::MediaType;

    use wedeo_codec::bitstream_filter::{BitstreamFilterDescriptor, BitstreamFilterFactory};

    use super::*;

    struct TestMuxer;

    impl Muxer for TestMuxer {
        fn write_header(&mut self, _io: &mut BufferedIo, _streams: &[Stream]) -> Result<()> {
            Ok(())
        }

        fn write_packet(&mut self, _io: &mut BufferedIo, _packet: &Packet) -> Result<()> {
            Ok(())
        }

        fn write_trailer(&mut self, _io: &mut BufferedIo) -> Result<()> {
            Ok(())
        }
    }

    /// Replace the packet payload with a single tag byte.
    struct TagFilter(u8);

    impl BitstreamFilter for TagFilter {
        fn filter(&mut self, packet: &Packet) -> Result<Vec<Packet>> {
            let mut out = packet.clone();
            out.data = Buffer::from_slice(&[self.0]);
            Ok(vec![out])
        }

        fn flush(&mut self) -> Result<Vec<Packet>> {
            Ok(Vec::new())
        }
    }

    /// Emit nothing, like a filter that buffers the packet for later.
    struct DropFilter;

    impl BitstreamFilter for DropFilter {
        fn filter(&mut self, _packet: &Packet) -> Result<Vec<Packet>> {
            Ok(Vec::new())
        }

        fn flush(&mut self) -> Result<Vec<Packet>> {
            Ok(Vec::new())
        }
    }

    struct TagFactory;

    static TAG_DESC: BitstreamFilterDescriptor = BitstreamFilterDescriptor {
        name: "test_tag_bsf",
        long_name: "test tagging filter",
        codec_id: CodecId::Vp9,
        priority: 1,
    };

    impl BitstreamFilterFactory for TagFactory {
        fn descriptor(&self) -> &BitstreamFilterDescriptor {
            &TAG_DESC
        }

        fn create(&self) -> Result<Box<dyn BitstreamFilter>> {
            Ok(Box::new(TagFilter(7)))
        }
    }

    inventory::submit!(&TagFactory as &'static dyn BitstreamFilterFactory);

    fn vp9_params() -> CodecParameters {
        CodecParameters::new(CodecId::Vp9, MediaType::Video)
    }

    #[test]
    fn muxer_default_has_no_bitstream_filters() {
        let muxer = TestMuxer;
        assert!(muxer.bitstream_filters().is_empty());
    }

    #[test]
    fn empty_chain_passes_packet_through_unchanged() {
        let packet = Packet::from_slice(&[1, 2, 3]);
        let mut filters: Vec<Box<dyn BitstreamFilter>> = Vec::new();
        let out = apply_filters(&mut filters, &packet).expect("empty filter chain");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].data.data(), packet.data.data());
    }

    #[test]
    fn apply_filters_feeds_each_output_to_the_next_filter() {
        let packet = Packet::from_slice(&[1, 2, 3]);
        let mut filters: Vec<Box<dyn BitstreamFilter>> =
            vec![Box::new(TagFilter(1)), Box::new(TagFilter(2))];
        let out = apply_filters(&mut filters, &packet).expect("two-stage chain");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].data.data(), &[2]);

        // A filter that emits nothing terminates the chain.
        let mut filters: Vec<Box<dyn BitstreamFilter>> =
            vec![Box::new(DropFilter), Box::new(TagFilter(3))];
        assert!(
            apply_filters(&mut filters, &packet)
                .expect("dropping chain")
                .is_empty()
        );
    }

    #[test]
    fn create_bitstream_filters_uses_registry_and_validates_names() {
        let filters = create_bitstream_filters(&["test_tag_bsf"], &vp9_params()).expect("chain");
        assert_eq!(filters.len(), 1);

        assert_eq!(
            create_bitstream_filters(&["no_such_bsf"], &vp9_params()).err(),
            Some(Error::FilterNotFound)
        );
        assert_eq!(
            create_bitstream_filters(
                &["test_tag_bsf"],
                &CodecParameters::new(CodecId::H264, MediaType::Video)
            )
            .err(),
            Some(Error::InvalidArgument)
        );
        assert!(
            create_bitstream_filters(&[], &vp9_params())
                .expect("empty names")
                .is_empty()
        );
    }
}
