use bitflags::bitflags;

use wedeo_core::channel_layout::ChannelLayout;
use wedeo_core::codec_id::CodecId;
use wedeo_core::error::Result;
use wedeo_core::frame::Frame;
use wedeo_core::media_type::MediaType;
use wedeo_core::packet::Packet;
use wedeo_core::pixel_format::PixelFormat;
use wedeo_core::rational::Rational;
use wedeo_core::sample_format::SampleFormat;

use crate::descriptor::{CodecCapabilities, CodecProperties};
use crate::options::CodecOptions;

bitflags! {
    /// Codec flags, matching FFmpeg's AV_CODEC_FLAG_*.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct CodecFlags: u32 {
        const UNALIGNED       = 1 << 0;
        const QSCALE          = 1 << 1;
        const OUTPUT_CORRUPT  = 1 << 3;
        const QPEL            = 1 << 4;
        const DROPCHANGED     = 1 << 5;
        const RECON_FRAME     = 1 << 6;
        const COPY_OPAQUE     = 1 << 7;
        const FRAME_DURATION  = 1 << 8;
        const PASS1           = 1 << 9;
        const PASS2           = 1 << 10;
        const LOOP_FILTER     = 1 << 11;
        const GRAY            = 1 << 13;
        const PSNR            = 1 << 15;
        const INTERLACED_DCT  = 1 << 18;
        const LOW_DELAY       = 1 << 19;
        const GLOBAL_HEADER   = 1 << 22;
        const BITEXACT        = 1 << 23;
        const AC_PRED         = 1 << 24;
        const INTERLACED_ME   = 1 << 29;
        const CLOSED_GOP      = 1 << 31;
    }
}

/// Encoder trait — the main abstraction for all encoders.
///
/// Follows FFmpeg's send/receive model:
/// 1. `send_frame()` — feed raw data
/// 2. `receive_packet()` — pull compressed data
/// 3. `flush()` — signal end-of-stream
pub trait Encoder: Send {
    /// Send a raw frame to the encoder.
    /// Pass `None` to signal end-of-stream (drain mode).
    fn send_frame(&mut self, frame: Option<&Frame>) -> Result<()>;

    /// Receive an encoded packet from the encoder.
    /// Returns `Error::Again` when more input is needed.
    /// Returns `Error::Eof` when all data has been drained.
    fn receive_packet(&mut self) -> Result<Packet>;

    /// Flush the encoder (reset internal state).
    fn flush(&mut self);

    /// Codec extradata (e.g. an `avcC` record for H.264 in MP4), if the
    /// encoder produces any. Muxers that need out-of-band headers must be
    /// given this before they write their header. Defaults to `None`, so
    /// existing encoders are unaffected.
    fn extradata(&self) -> Option<Vec<u8>> {
        None
    }
}

/// Builder for creating encoders.
pub struct EncoderBuilder {
    pub codec_id: CodecId,
    pub media_type: MediaType,
    pub bit_rate: i64,
    pub flags: CodecFlags,
    pub time_base: Rational,

    /// Number of threads the encoder should use. `None` means unspecified
    /// (let the runtime decide), matching FFmpeg's `AVCodecContext.thread_count`
    /// default of `0`. `Some(0)` likewise means "auto".
    pub thread_count: Option<u32>,

    // Audio
    pub sample_rate: u32,
    pub sample_format: SampleFormat,
    pub channel_layout: ChannelLayout,

    // Video
    pub width: u32,
    pub height: u32,
    pub pixel_format: PixelFormat,

    /// Codec-private options (string key-value pairs).
    pub options: CodecOptions,
}

impl EncoderBuilder {
    pub fn new(codec_id: CodecId, media_type: MediaType) -> Self {
        Self {
            codec_id,
            media_type,
            bit_rate: 0,
            flags: CodecFlags::empty(),
            time_base: Rational::new(1, 1),
            thread_count: None,
            sample_rate: 0,
            sample_format: SampleFormat::None,
            channel_layout: ChannelLayout::unspec(0),
            width: 0,
            height: 0,
            pixel_format: PixelFormat::None,
            options: CodecOptions::new(),
        }
    }

    /// Set a codec-private option (string key-value pair).
    pub fn option(mut self, key: &str, value: &str) -> Self {
        self.options.set(key, value);
        self
    }

    /// Set the encoder thread count (`0` = auto).
    ///
    /// This is a typed convenience setter for the `thread_count` field; it is
    /// additive and does not change how any existing encoder is opened.
    pub fn with_thread_count(mut self, count: u32) -> Self {
        self.thread_count = Some(count);
        self
    }

    /// Resolve the effective thread count for this encoder.
    ///
    /// The explicit `thread_count` field takes precedence. When it is unset,
    /// this falls back to the `threads` codec-private option, which is where
    /// the CLI currently places `-threads`. This makes `-threads` reachable by
    /// encoders without any CLI change. `None` means the caller did not specify
    /// a count and the encoder should let the runtime decide.
    pub fn effective_thread_count(&self) -> Option<u32> {
        self.thread_count
            .or_else(|| self.options.get_thread_count())
    }

    pub fn open(self) -> Result<Box<dyn Encoder>> {
        crate::registry::find_encoder(self.codec_id)
            .ok_or(wedeo_core::Error::EncoderNotFound)?
            .create(self)
    }
}

/// Descriptor for an encoder implementation, used in the registry.
#[derive(Debug, Clone)]
pub struct EncoderDescriptor {
    pub codec_id: CodecId,
    pub name: &'static str,
    pub long_name: &'static str,
    pub media_type: MediaType,
    pub capabilities: CodecCapabilities,
    pub properties: CodecProperties,
    pub supported_sample_formats: &'static [SampleFormat],
    pub supported_pixel_formats: &'static [PixelFormat],
    /// Priority for registry selection. Higher priority wins when multiple
    /// encoders support the same codec_id. Native implementations use 100,
    /// wrapper/adapter implementations use 50.
    pub priority: i32,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn builder() -> EncoderBuilder {
        EncoderBuilder::new(CodecId::Rawvideo, MediaType::Video)
    }

    #[test]
    fn test_builder_defaults() {
        let enc = builder();
        assert_eq!(enc.thread_count, None);
        assert_eq!(enc.effective_thread_count(), None);
        assert!(enc.options.is_empty());
    }

    #[test]
    fn test_with_thread_count_sets_field() {
        let enc = builder().with_thread_count(4);
        assert_eq!(enc.thread_count, Some(4));
        assert_eq!(enc.effective_thread_count(), Some(4));

        // 0 is a valid explicit value meaning "auto".
        let enc = builder().with_thread_count(0);
        assert_eq!(enc.thread_count, Some(0));
        assert_eq!(enc.effective_thread_count(), Some(0));
    }

    #[test]
    fn test_thread_count_via_options() {
        // The CLI currently stores `-threads` as the `threads` codec-private
        // option; the builder must resolve it without any CLI change.
        let enc = builder().option("threads", "8");
        assert_eq!(enc.thread_count, None);
        assert_eq!(enc.effective_thread_count(), Some(8));

        let mut enc = builder();
        enc.options.set("threads", "auto");
        assert_eq!(enc.effective_thread_count(), Some(0));
    }

    #[test]
    fn test_explicit_thread_count_takes_precedence_over_options() {
        let enc = builder().option("threads", "8").with_thread_count(2);
        assert_eq!(enc.thread_count, Some(2));
        assert_eq!(enc.effective_thread_count(), Some(2));
    }
}
