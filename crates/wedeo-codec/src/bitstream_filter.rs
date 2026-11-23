//! Bitstream filter interface, matching FFmpeg's `AVBSFContext` and
//! `FFBitstreamFilter`.
//!
//! A bitstream filter consumes one encoded packet and produces zero or more
//! encoded packets, without decoding. Implementations register a factory via
//! `inventory::submit!` in the same way as decoders and muxers.

use wedeo_core::codec_id::CodecId;
use wedeo_core::error::{Error, Result};
use wedeo_core::packet::Packet;

use crate::decoder::CodecParameters;

/// A bitstream filter instance, matching FFmpeg's `av_bsf_send_packet` /
/// `av_bsf_receive_packet` pair collapsed into a single call.
pub trait BitstreamFilter: Send {
    /// Transform one input packet into zero or more output packets.
    fn filter(&mut self, packet: &Packet) -> Result<Vec<Packet>>;

    /// Flush any buffered packets at end of stream.
    fn flush(&mut self) -> Result<Vec<Packet>>;

    /// Configure the filter from the stream's codec parameters.
    ///
    /// Mirrors the body of FFmpeg's `FFBitstreamFilter.init` callback, which
    /// `av_bsf_init()` invokes once `par_in` is known. The default is a no-op,
    /// so existing filters keep compiling unchanged; filters that need
    /// extradata (e.g. `h264_mp4toannexb`) override this method instead of
    /// requiring a factory-side downcast.
    fn set_parameters(&mut self, _params: &CodecParameters) -> Result<()> {
        Ok(())
    }
}

/// Registry metadata for a bitstream filter.
#[derive(Debug, Clone, Copy)]
pub struct BitstreamFilterDescriptor {
    pub name: &'static str,
    pub long_name: &'static str,
    pub codec_id: CodecId,
    pub priority: i32,
}

/// Factory for creating bitstream filter instances.
pub trait BitstreamFilterFactory: Send + Sync {
    fn descriptor(&self) -> &BitstreamFilterDescriptor;
    fn create(&self) -> Result<Box<dyn BitstreamFilter>>;

    /// Initialize a freshly created filter from the stream codec parameters,
    /// matching FFmpeg's `av_bsf_init()` / `FFBitstreamFilter.init`.
    ///
    /// Additive: the default forwards to [`BitstreamFilter::set_parameters`],
    /// itself a no-op unless a concrete filter overrides it. Factories that
    /// manage their own state may override this instead.
    fn init(&self, filter: &mut dyn BitstreamFilter, params: &CodecParameters) -> Result<()> {
        filter.set_parameters(params)
    }
}

inventory::collect!(&'static dyn BitstreamFilterFactory);

/// Find a bitstream filter by name.
pub fn find_bitstream_filter(name: &str) -> Option<&'static dyn BitstreamFilterFactory> {
    inventory::iter::<&'static dyn BitstreamFilterFactory>()
        .find(|f| f.descriptor().name == name)
        .copied()
}

/// Find a bitstream filter registered for `codec_id`.
///
/// When several filters claim the same codec, the one with the highest
/// `priority` is returned, so muxers can pick a sensible default per-stream
/// filter without hard-coding names.
pub fn find_bitstream_filter_for(codec_id: CodecId) -> Option<&'static dyn BitstreamFilterFactory> {
    inventory::iter::<&'static dyn BitstreamFilterFactory>()
        .filter(|f| f.descriptor().codec_id == codec_id)
        .max_by_key(|f| f.descriptor().priority)
        .copied()
}

/// Iterate over all registered bitstream filters.
pub fn bitstream_filters() -> impl Iterator<Item = &'static dyn BitstreamFilterFactory> {
    inventory::iter::<&'static dyn BitstreamFilterFactory>().copied()
}

/// Create and initialize a bitstream filter by name, following FFmpeg's
/// `av_bsf_alloc()` + `av_bsf_init()` sequence.
pub fn create_initialized_bitstream_filter(
    name: &str,
    params: &CodecParameters,
) -> Result<Box<dyn BitstreamFilter>> {
    let factory = find_bitstream_filter(name).ok_or(Error::FilterNotFound)?;

    // FFmpeg's av_bsf_init() rejects a codec the filter does not support.
    if factory.descriptor().codec_id != params.codec_id {
        return Err(Error::InvalidArgument);
    }

    let mut filter = factory.create()?;
    factory.init(filter.as_mut(), params)?;
    Ok(filter)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use wedeo_core::media_type::MediaType;

    use super::*;

    fn params(codec_id: CodecId, extradata: &[u8]) -> CodecParameters {
        let mut params = CodecParameters::new(codec_id, MediaType::Video);
        params.extradata = extradata.to_vec();
        params
    }

    /// Pass-through filter with the default `set_parameters` no-op.
    struct PlainFilter;

    impl BitstreamFilter for PlainFilter {
        fn filter(&mut self, packet: &Packet) -> Result<Vec<Packet>> {
            Ok(vec![packet.clone()])
        }

        fn flush(&mut self) -> Result<Vec<Packet>> {
            Ok(Vec::new())
        }
    }

    /// Filter that fails initialization without extradata, proving the
    /// factory's default `init` forwards parameters to the instance.
    struct NeedyFilter;

    impl BitstreamFilter for NeedyFilter {
        fn filter(&mut self, packet: &Packet) -> Result<Vec<Packet>> {
            Ok(vec![packet.clone()])
        }

        fn flush(&mut self) -> Result<Vec<Packet>> {
            Ok(Vec::new())
        }

        fn set_parameters(&mut self, params: &CodecParameters) -> Result<()> {
            if params.extradata.is_empty() {
                Err(Error::InvalidArgument)
            } else {
                Ok(())
            }
        }
    }

    static INIT_CALLED: AtomicBool = AtomicBool::new(false);

    struct CountingFactory;

    static COUNTING_DESC: BitstreamFilterDescriptor = BitstreamFilterDescriptor {
        name: "counting_test_bsf",
        long_name: "counting test filter",
        codec_id: CodecId::Vp8,
        priority: 50,
    };

    impl BitstreamFilterFactory for CountingFactory {
        fn descriptor(&self) -> &BitstreamFilterDescriptor {
            &COUNTING_DESC
        }

        fn create(&self) -> Result<Box<dyn BitstreamFilter>> {
            Ok(Box::new(NeedyFilter))
        }

        fn init(&self, filter: &mut dyn BitstreamFilter, params: &CodecParameters) -> Result<()> {
            INIT_CALLED.store(true, Ordering::SeqCst);
            filter.set_parameters(params)
        }
    }

    struct PlainFactory;

    static PLAIN_DESC: BitstreamFilterDescriptor = BitstreamFilterDescriptor {
        name: "plain_test_bsf",
        long_name: "plain test filter",
        codec_id: CodecId::Vp9,
        priority: 1,
    };

    impl BitstreamFilterFactory for PlainFactory {
        fn descriptor(&self) -> &BitstreamFilterDescriptor {
            &PLAIN_DESC
        }

        fn create(&self) -> Result<Box<dyn BitstreamFilter>> {
            Ok(Box::new(PlainFilter))
        }
    }

    struct HighPriorityFactory;

    static HIGH_DESC: BitstreamFilterDescriptor = BitstreamFilterDescriptor {
        name: "high_priority_test_bsf",
        long_name: "high priority test filter",
        codec_id: CodecId::Mpeg4,
        priority: 100,
    };

    impl BitstreamFilterFactory for HighPriorityFactory {
        fn descriptor(&self) -> &BitstreamFilterDescriptor {
            &HIGH_DESC
        }

        fn create(&self) -> Result<Box<dyn BitstreamFilter>> {
            Ok(Box::new(PlainFilter))
        }
    }

    struct LowPriorityFactory;

    static LOW_DESC: BitstreamFilterDescriptor = BitstreamFilterDescriptor {
        name: "low_priority_test_bsf",
        long_name: "low priority test filter",
        codec_id: CodecId::Mpeg4,
        priority: 1,
    };

    impl BitstreamFilterFactory for LowPriorityFactory {
        fn descriptor(&self) -> &BitstreamFilterDescriptor {
            &LOW_DESC
        }

        fn create(&self) -> Result<Box<dyn BitstreamFilter>> {
            Ok(Box::new(PlainFilter))
        }
    }

    inventory::submit!(&CountingFactory as &'static dyn BitstreamFilterFactory);
    inventory::submit!(&PlainFactory as &'static dyn BitstreamFilterFactory);
    inventory::submit!(&HighPriorityFactory as &'static dyn BitstreamFilterFactory);
    inventory::submit!(&LowPriorityFactory as &'static dyn BitstreamFilterFactory);

    #[test]
    fn default_init_is_a_noop_for_plain_filters() {
        let factory = PlainFactory;
        let mut filter = PlainFilter;
        assert!(
            factory
                .init(&mut filter, &params(CodecId::Vp9, &[]))
                .is_ok()
        );
    }

    #[test]
    fn default_init_forwards_parameters_to_the_filter() {
        let factory = PlainFactory;
        let mut needy = NeedyFilter;
        assert_eq!(
            factory.init(&mut needy, &params(CodecId::Vp9, &[])),
            Err(Error::InvalidArgument)
        );
        assert!(
            factory
                .init(&mut needy, &params(CodecId::Vp9, &[1, 2, 3]))
                .is_ok()
        );
    }

    #[test]
    fn find_by_name_and_registry_iteration() {
        assert!(find_bitstream_filter("counting_test_bsf").is_some());
        assert!(find_bitstream_filter("does_not_exist").is_none());
        assert!(bitstream_filters().any(|f| f.descriptor().name == "plain_test_bsf"));
    }

    #[test]
    fn find_for_codec_prefers_highest_priority() {
        let found = find_bitstream_filter_for(CodecId::Mpeg4).expect("MPEG-4 test BSFs");
        assert_eq!(found.descriptor().name, "high_priority_test_bsf");
        assert!(find_bitstream_filter_for(CodecId::Flac).is_none());
    }

    #[test]
    fn create_initialized_checks_name_and_codec_and_runs_init() {
        INIT_CALLED.store(false, Ordering::SeqCst);

        assert_eq!(
            create_initialized_bitstream_filter("missing", &params(CodecId::Vp8, &[1])).err(),
            Some(Error::FilterNotFound)
        );
        // Wrong codec for the registered filter.
        assert_eq!(
            create_initialized_bitstream_filter("counting_test_bsf", &params(CodecId::Vp9, &[1]))
                .err(),
            Some(Error::InvalidArgument)
        );
        // Matching codec, but init still rejects empty extradata.
        assert_eq!(
            create_initialized_bitstream_filter("counting_test_bsf", &params(CodecId::Vp8, &[]))
                .err(),
            Some(Error::InvalidArgument)
        );
        assert!(INIT_CALLED.load(Ordering::SeqCst));

        let mut filter =
            create_initialized_bitstream_filter("counting_test_bsf", &params(CodecId::Vp8, &[1]))
                .expect("initialized filter");
        let out = filter.filter(&Packet::from_slice(&[9])).expect("filter");
        assert_eq!(out.len(), 1);
    }
}
