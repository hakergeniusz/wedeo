// Native WAV demuxer for wedeo.
//
// C references:
//   libavformat/wavdec.c   (wav_probe, wav_read_header, wav_read_packet, wav_read_seek)
//   libavformat/riffdec.c  (ff_get_wav_header, parse_waveformatex)
//   libavformat/pcm.c      (ff_pcm_default_packet_size, ff_pcm_read_seek)
//
// Packet framing deliberately follows FFmpeg: `ff_pcm_default_packet_size()`
// picks a multiple of block_align derived from the bitrate, rounded down to a
// power of two. For 48 kHz stereo s16le that is 4096 samples (16384 bytes).

use tracing::debug;
use wedeo_codec::decoder::CodecParameters;
use wedeo_core::buffer::Buffer;
use wedeo_core::channel_layout::ChannelLayout;
use wedeo_core::codec_id::CodecId;
use wedeo_core::error::{Error, Result};
use wedeo_core::media_type::MediaType;
use wedeo_core::metadata::Metadata;
use wedeo_core::packet::{Packet, PacketFlags};
use wedeo_core::rational::Rational;
use wedeo_core::sample_format::SampleFormat;
use wedeo_format::demuxer::{
    Demuxer, DemuxerHeader, InputFormatDescriptor, InputFormatFlags, PROBE_SCORE_MAX, ProbeData,
    SeekFlags, Stream,
};
use wedeo_format::io::{BufferedIo, read_tag};
use wedeo_format::registry::DemuxerFactory;

const WAVE_FORMAT_PCM: u16 = 0x0001;
const WAVE_FORMAT_IEEE_FLOAT: u16 = 0x0003;
const WAVE_FORMAT_ALAW: u16 = 0x0006;
const WAVE_FORMAT_MULAW: u16 = 0x0007;
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;

/// Maximum number of fmt-chunk bytes we parse. WAVEFORMATEXTENSIBLE needs 40.
const FMT_PARSE_BYTES: u64 = 64;

// ---------------------------------------------------------------------------
// Codec / format mapping (libavformat/riffdec.c, libavformat/utils.c)
// ---------------------------------------------------------------------------

/// `ff_get_pcm_codec_id(bps, 0, be, ~1)` from libavformat/utils.c.
/// For WAVE_FORMAT_PCM every sample size except 8 bits is signed.
fn pcm_codec_id(bits_per_sample: u32, big_endian: bool) -> Option<CodecId> {
    if bits_per_sample == 0 || bits_per_sample > 64 {
        return None;
    }
    let bytes = (bits_per_sample + 7) >> 3;
    let signed = bytes > 1; // sflags == ~1: bit 0 clear, all others set
    match (bytes, signed, big_endian) {
        (1, false, _) => Some(CodecId::PcmU8),
        (2, true, false) => Some(CodecId::PcmS16le),
        (2, true, true) => Some(CodecId::PcmS16be),
        (3, true, false) => Some(CodecId::PcmS24le),
        (3, true, true) => Some(CodecId::PcmS24be),
        (4, true, false) => Some(CodecId::PcmS32le),
        (4, true, true) => Some(CodecId::PcmS32be),
        _ => None,
    }
}

/// `ff_get_pcm_codec_id(bps, 1, be, 0)` from libavformat/utils.c.
fn float_codec_id(bits_per_sample: u32, big_endian: bool) -> Option<CodecId> {
    match (bits_per_sample, big_endian) {
        (32, false) => Some(CodecId::PcmF32le),
        (32, true) => Some(CodecId::PcmF32be),
        (64, false) => Some(CodecId::PcmF64le),
        (64, true) => Some(CodecId::PcmF64be),
        _ => None,
    }
}

/// `ff_wav_codec_get_id(tag, bps)` for the PCM family used by WAV files.
fn wav_codec_get_id(tag: u16, bits_per_sample: u32, big_endian: bool) -> Option<CodecId> {
    match tag {
        WAVE_FORMAT_PCM => pcm_codec_id(bits_per_sample, big_endian),
        WAVE_FORMAT_IEEE_FLOAT => float_codec_id(bits_per_sample, big_endian),
        WAVE_FORMAT_ALAW => Some(CodecId::PcmAlaw),
        WAVE_FORMAT_MULAW => Some(CodecId::PcmMulaw),
        _ => None,
    }
}

fn codec_id_to_sample_format(codec_id: CodecId) -> SampleFormat {
    match codec_id {
        CodecId::PcmU8 => SampleFormat::U8,
        CodecId::PcmS16le | CodecId::PcmS16be => SampleFormat::S16,
        CodecId::PcmS24le | CodecId::PcmS24be | CodecId::PcmS32le | CodecId::PcmS32be => {
            SampleFormat::S32
        }
        CodecId::PcmF32le | CodecId::PcmF32be => SampleFormat::Flt,
        CodecId::PcmF64le | CodecId::PcmF64be => SampleFormat::Dbl,
        // A-law / mu-law are decoded to signed 16-bit PCM.
        CodecId::PcmAlaw | CodecId::PcmMulaw => SampleFormat::S16,
        _ => SampleFormat::None,
    }
}

fn default_channel_layout(channels: u32) -> ChannelLayout {
    match channels {
        1 => ChannelLayout::mono(),
        2 => ChannelLayout::stereo(),
        6 => ChannelLayout::surround_5_1(),
        8 => ChannelLayout::surround_7_1(),
        n => ChannelLayout::unspec(n as i32),
    }
}

// ---------------------------------------------------------------------------
// Packet sizing (libavformat/pcm.c: ff_pcm_default_packet_size)
// ---------------------------------------------------------------------------

/// Return the packet size in bytes, exactly as `ff_pcm_default_packet_size()`.
///
/// ```text
/// #define PCM_DEMUX_TARGET_FPS  10
/// max_samples = INT_MAX / par->block_align;
/// bits_per_sample = av_get_bits_per_sample(par->codec_id);
/// bitrate = par->bit_rate;
/// if (bits_per_sample > 0 && sample_rate > 0 && channels > 0)
///     bitrate = bits_per_sample * sample_rate * channels;
/// nb_samples = av_clip64(bitrate / 8 / 10 / block_align, 1, max_samples);
/// nb_samples = 1 << av_log2(nb_samples);
/// return block_align * nb_samples;
/// ```
fn pcm_default_packet_size(
    bits_per_sample: u32,
    sample_rate: u32,
    channels: u32,
    block_align: u32,
    bit_rate: i64,
) -> Result<usize> {
    if block_align == 0 {
        return Err(Error::InvalidArgument);
    }

    let max_samples = i32::MAX as u64 / u64::from(block_align);
    let mut bitrate = bit_rate;

    // Don't trust the codecpar bitrate if we can calculate it ourselves.
    if bits_per_sample > 0 && sample_rate > 0 && channels > 0 {
        let bps = i64::from(bits_per_sample);
        if i64::from(sample_rate) * i64::from(channels) < i64::MAX / bps {
            bitrate = bps * i64::from(sample_rate) * i64::from(channels);
        }
    }

    let nb_samples = if bitrate > 0 {
        let n = (bitrate / 8 / 10 / i64::from(block_align)).clamp(1, max_samples as i64) as u64;
        // av_log2(n) = floor(log2(n)); n >= 1 here.
        1u64 << (63 - n.leading_zeros())
    } else {
        // Fallback to a size based method for a non-pcm codec with unknown bitrate.
        (4096 / u64::from(block_align)).clamp(1, max_samples)
    };

    Ok((u64::from(block_align) * nb_samples) as usize)
}

// ---------------------------------------------------------------------------
// Little/big endian field readers
// ---------------------------------------------------------------------------

fn field_u16(data: &[u8], offset: usize, big_endian: bool) -> Option<u16> {
    let bytes = data.get(offset..offset + 2)?;
    Some(if big_endian {
        u16::from_be_bytes([bytes[0], bytes[1]])
    } else {
        u16::from_le_bytes([bytes[0], bytes[1]])
    })
}

fn field_u32(data: &[u8], offset: usize, big_endian: bool) -> Option<u32> {
    let bytes = data.get(offset..offset + 4)?;
    Some(if big_endian {
        u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
    } else {
        u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
    })
}

// ---------------------------------------------------------------------------
// Demuxer state
// ---------------------------------------------------------------------------

struct WavDemuxer {
    /// RIFX files store integers big-endian and use big-endian PCM codecs.
    big_endian: bool,
    /// Absolute offset of the first data byte.
    data_offset: u64,
    /// Absolute offset just past the last data byte.
    data_end: u64,
    /// Block align used to split packets at sample boundaries.
    block_align: u32,
    /// Packet size in bytes from `ff_pcm_default_packet_size()`.
    max_size: usize,
    /// Next packet PTS, in samples (the stream time base is 1/sample_rate).
    next_pts: i64,
    /// Samples per second, for seeking.
    sample_rate: u32,
    /// Bit rate from the fmt chunk, for seeking.
    bit_rate: i64,
    /// WAVE format tag from the fmt chunk (or the SubFormat GUID code).
    format_tag: u16,
    header_read: bool,
}

impl WavDemuxer {
    fn new() -> Self {
        Self {
            big_endian: false,
            data_offset: 0,
            data_end: 0,
            block_align: 1,
            max_size: 4096,
            next_pts: 0,
            sample_rate: 0,
            bit_rate: 0,
            format_tag: 0,
            header_read: false,
        }
    }

    /// Parse the `fmt ` chunk data (see `ff_get_wav_header()` in riffdec.c).
    #[allow(clippy::too_many_arguments)]
    fn parse_fmt(
        &mut self,
        data: &[u8],
        fmt_size: u64,
        channels: &mut u32,
        bits_per_sample: &mut u32,
        byte_rate: &mut u32,
        channel_mask: &mut u32,
        codec_id: &mut CodecId,
    ) -> Result<()> {
        let be = self.big_endian;
        let format_tag = field_u16(data, 0, be).ok_or(Error::InvalidData)?;
        *channels = u32::from(field_u16(data, 2, be).ok_or(Error::InvalidData)?);
        let sample_rate = field_u32(data, 4, be).ok_or(Error::InvalidData)?;
        *byte_rate = field_u32(data, 8, be).ok_or(Error::InvalidData)?;
        let block_align = u32::from(field_u16(data, 12, be).ok_or(Error::InvalidData)?);

        // WAVEFORMAT (size 14) implies 8 bits per sample.
        *bits_per_sample = if fmt_size == 14 {
            8
        } else {
            u32::from(field_u16(data, 14, be).ok_or(Error::InvalidData)?)
        };

        let mut effective_tag = format_tag;
        if fmt_size >= 18 {
            let cb_size = u32::from(field_u16(data, 16, be).ok_or(Error::InvalidData)?);
            if cb_size >= 22 && format_tag == WAVE_FORMAT_EXTENSIBLE && data.len() >= 40 {
                // WAVEFORMATEXTENSIBLE: parse_waveformatex() in riffdec.c.
                let valid_bits = u32::from(field_u16(data, 18, be).ok_or(Error::InvalidData)?);
                if valid_bits != 0 {
                    *bits_per_sample = valid_bits;
                }
                *channel_mask = field_u32(data, 20, be).ok_or(Error::InvalidData)?;
                // SubFormat GUID: the first 4 bytes hold the WAVE format code.
                effective_tag = field_u32(data, 24, be).ok_or(Error::InvalidData)? as u16;
            }
        }

        if sample_rate == 0 {
            return Err(Error::InvalidData);
        }

        // ff_get_wav_header() overrides the bitrate with the value computed
        // from the format when it is inconsistent. The packet-size helper
        // recomputes it again from bits/sample rate/channels, so we only need
        // a sane value for seek().
        *codec_id = wav_codec_get_id(effective_tag, *bits_per_sample, be).unwrap_or(CodecId::None);

        self.sample_rate = sample_rate;
        self.block_align = if block_align > 0 {
            block_align
        } else {
            (*bits_per_sample * *channels) >> 3
        };
        self.bit_rate = i64::from(*byte_rate) * 8;
        self.format_tag = effective_tag;

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Demuxer trait implementation
// ---------------------------------------------------------------------------

impl Demuxer for WavDemuxer {
    fn read_header(&mut self, io: &mut BufferedIo) -> Result<DemuxerHeader> {
        // Read the chunk ID (RIFF / RIFX / RF64 / BW64).
        let tag = read_tag(io)?;
        let mut rf64 = false;
        match &tag {
            b"RIFF" => self.big_endian = false,
            b"RIFX" => self.big_endian = true,
            b"RF64" | b"BW64" => rf64 = true,
            _ => return Err(Error::InvalidData),
        }

        // RIFF chunk size (unused; RF64 keeps the real one in ds64).
        let _riff_size = io.read_u32le()?;
        if read_tag(io)? != *b"WAVE" {
            return Err(Error::InvalidData);
        }

        let mut ds64_data_size: u64 = 0;
        let mut ds64_sample_count: i64 = 0;
        if rf64 {
            if read_tag(io)? != *b"ds64" {
                return Err(Error::InvalidData);
            }
            let ds64_size = io.read_u32le()? as u64;
            if ds64_size < 24 {
                return Err(Error::InvalidData);
            }
            let _riff_size_large = io.read_u64le()?;
            ds64_data_size = io.read_u64le()?;
            ds64_sample_count = io.read_u64le()? as i64;
            if ds64_data_size > i64::MAX as u64 || ds64_sample_count < 0 {
                return Err(Error::InvalidData);
            }
            let extra = ds64_size - 24;
            if extra > 0 {
                io.skip(extra)?;
            }
        }

        let mut channels: u32 = 0;
        let mut bits_per_sample: u32 = 0;
        let mut byte_rate: u32 = 0;
        let mut sample_count: i64 = ds64_sample_count;
        let mut channel_mask: u32 = 0;
        let mut codec_id = CodecId::None;
        let mut got_fmt = false;
        let mut data_size: u64 = ds64_data_size;
        let mut got_data = false;

        loop {
            let chunk_tag = match read_tag(io) {
                Ok(t) => t,
                Err(Error::Eof) => break,
                Err(e) => return Err(e),
            };
            let size_u32 = if self.big_endian {
                io.read_u32be()?
            } else {
                io.read_u32le()?
            };
            let size = u64::from(size_u32);
            let next_tag_ofs = io.tell()?.saturating_add(size).saturating_add(size & 1);

            if &chunk_tag == b"fmt " {
                // Only parse the first fmt chunk.
                if !got_fmt {
                    if size < 14 {
                        return Err(Error::InvalidData);
                    }
                    let read_len = size.min(FMT_PARSE_BYTES) as usize;
                    let fmt_data = io.read_bytes(read_len)?;
                    self.parse_fmt(
                        &fmt_data,
                        size,
                        &mut channels,
                        &mut bits_per_sample,
                        &mut byte_rate,
                        &mut channel_mask,
                        &mut codec_id,
                    )?;
                    if size > read_len as u64 {
                        io.skip(size - read_len as u64)?;
                    }
                    got_fmt = true;
                }
            } else if &chunk_tag == b"data" {
                let data_ofs = io.tell()?;
                if rf64 {
                    // ds64 holds the real data size.
                    data_size = ds64_data_size;
                } else if size > 0 && size_u32 != u32::MAX {
                    data_size = size;
                } else {
                    // Unknown length: read until EOF, like FFmpeg's INT64_MAX.
                    data_size = io.size().unwrap_or(0).saturating_sub(data_ofs);
                }
                self.data_offset = data_ofs;
                self.data_end = data_ofs.saturating_add(data_size);
                got_data = true;
                // Stop here: some files carry junk chunks after data.
                break;
            } else if &chunk_tag == b"fact" && size >= 4 && sample_count == 0 {
                let count = if self.big_endian {
                    io.read_u32be()?
                } else {
                    io.read_u32le()?
                };
                sample_count = i64::from(count);
            }

            // Seek/skip to the next RIFF chunk (chunks are word aligned).
            match io.tell() {
                Ok(cur) if cur < next_tag_ofs => {
                    if io.skip(next_tag_ofs - cur).is_err() {
                        break;
                    }
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }

        if !got_fmt || !got_data {
            return Err(Error::InvalidData);
        }
        if self.block_align == 0 {
            return Err(Error::InvalidData);
        }

        // Position the stream at the start of the data payload.
        io.seek(self.data_offset)?;

        self.max_size = pcm_default_packet_size(
            bits_per_sample,
            self.sample_rate,
            channels,
            self.block_align,
            self.bit_rate,
        )
        .unwrap_or(4096);
        self.next_pts = 0;
        self.header_read = true;

        let sample_format = codec_id_to_sample_format(codec_id);
        let mut params = CodecParameters::new(codec_id, MediaType::Audio);
        params.sample_rate = self.sample_rate;
        params.sample_format = sample_format;
        params.channel_layout = if channel_mask != 0 {
            let layout = ChannelLayout::from_wav_channel_mask(channel_mask);
            if layout.nb_channels == channels as i32 {
                layout
            } else {
                default_channel_layout(channels)
            }
        } else {
            default_channel_layout(channels)
        };
        params.bits_per_coded_sample = bits_per_sample;
        params.codec_tag = u32::from(self.format_tag);
        params.bit_rate = self.bit_rate;
        params.block_align = self.block_align;

        let mut stream = Stream::new(0, params);
        stream.time_base = Rational::new(1, self.sample_rate as i32);
        if sample_count > 0 {
            stream.duration = sample_count;
        } else if data_size > 0 {
            stream.duration = (data_size / u64::from(self.block_align)) as i64;
        }

        debug!(
            data_offset = self.data_offset,
            data_size,
            block_align = self.block_align,
            max_size = self.max_size,
            sample_rate = self.sample_rate,
            channels,
            ?codec_id,
            "WAV header"
        );

        let duration = stream.duration;
        Ok(DemuxerHeader {
            streams: vec![stream],
            metadata: Metadata::new(),
            duration,
            start_time: 0,
        })
    }

    fn read_packet(&mut self, io: &mut BufferedIo) -> Result<Packet> {
        if !self.header_read {
            return Err(Error::InvalidData);
        }

        let pos = io.tell()?;
        let left = self.data_end.saturating_sub(pos);
        if left == 0 {
            return Err(Error::Eof);
        }

        // wav_read_packet(): size = FFMIN(max_size, left), then align down to
        // a whole number of samples.
        let block_align = u64::from(self.block_align);
        let mut size = (self.max_size as u64).min(left);
        size = (size / block_align) * block_align;
        if size == 0 {
            return Err(Error::Eof);
        }

        let data = io.read_up_to(size as usize)?;
        if data.is_empty() {
            return Err(Error::Eof);
        }
        let nb_samples = (data.len() / self.block_align as usize) as i64;

        let mut pkt = Packet::new();
        pkt.data = Buffer::from_slice(&data);
        pkt.pts = self.next_pts;
        pkt.dts = self.next_pts;
        pkt.duration = nb_samples;
        pkt.stream_index = 0;
        pkt.flags = PacketFlags::KEY;
        pkt.pos = pos as i64;

        self.next_pts += nb_samples;
        Ok(pkt)
    }

    fn seek(
        &mut self,
        io: &mut BufferedIo,
        stream_index: usize,
        timestamp: i64,
        flags: SeekFlags,
    ) -> Result<()> {
        if !self.header_read || stream_index != 0 {
            return Err(Error::InvalidArgument);
        }
        if self.sample_rate == 0 || self.block_align == 0 {
            return Err(Error::InvalidArgument);
        }

        // ff_pcm_read_seek() from libavformat/pcm.c. With time_base 1/sample_rate
        // this reduces to timestamp samples * block_align bytes.
        let byte_rate = if self.bit_rate > 0 {
            self.bit_rate / 8
        } else {
            i64::from(self.block_align) * i64::from(self.sample_rate)
        };
        if byte_rate <= 0 {
            return Err(Error::InvalidArgument);
        }

        let timestamp = timestamp.max(0);
        let num = (timestamp as i128) * i128::from(byte_rate);
        let den = i128::from(self.sample_rate) * i128::from(self.block_align);
        let samples = if flags.contains(SeekFlags::BACKWARD) {
            num.div_euclid(den)
        } else {
            // Round up, like AV_ROUND_UP in ff_pcm_read_seek().
            num / den + i128::from(num % den != 0)
        };
        let samples = samples.max(0) as u64;
        let offset = samples * u64::from(self.block_align);
        let target = self.data_offset.saturating_add(offset).min(self.data_end);

        io.seek(target)?;
        self.next_pts = (target - self.data_offset) as i64 / i64::from(self.block_align);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Factory / registration
// ---------------------------------------------------------------------------

struct WavDemuxerFactory;

impl DemuxerFactory for WavDemuxerFactory {
    fn descriptor(&self) -> &InputFormatDescriptor {
        static DESC: InputFormatDescriptor = InputFormatDescriptor {
            name: "wav",
            long_name: "WAV / WAVE (Waveform Audio)",
            extensions: "wav",
            mime_types: "audio/x-wav",
            flags: InputFormatFlags::empty(),
            priority: 100,
        };
        &DESC
    }

    fn probe(&self, data: &ProbeData<'_>) -> i32 {
        // wav_probe() from libavformat/wavdec.c.
        if data.buf.len() <= 32 {
            return 0;
        }
        if &data.buf[8..12] == b"WAVE" {
            if &data.buf[0..4] == b"RIFF" || &data.buf[0..4] == b"RIFX" {
                // Since the ACT demuxer has a standard WAV header at the top of
                // its own, the returned score is decreased to avoid a probe
                // conflict between ACT and WAV.
                return PROBE_SCORE_MAX - 1;
            }
            if (&data.buf[0..4] == b"RF64" || &data.buf[0..4] == b"BW64")
                && data.buf.len() > 16
                && &data.buf[12..16] == b"ds64"
            {
                return PROBE_SCORE_MAX;
            }
        }
        0
    }

    fn create(&self) -> Result<Box<dyn Demuxer>> {
        Ok(Box::new(WavDemuxer::new()))
    }
}

inventory::submit!(&WavDemuxerFactory as &dyn DemuxerFactory);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pcm_packet_size_matches_ffmpeg() {
        // 48 kHz, stereo, s16le: bitrate/8/10/block_align == 4800,
        // av_log2(4800) == 12 -> 4096 samples * 4 bytes.
        assert_eq!(
            pcm_default_packet_size(16, 48000, 2, 4, 1_536_000).unwrap(),
            16384
        );
        // 44.1 kHz, stereo, s16le: 4410 -> 4096 samples.
        assert_eq!(
            pcm_default_packet_size(16, 44100, 2, 4, 1_411_200).unwrap(),
            16384
        );
        // 44.1 kHz, mono, s16le: 4410 -> 4096 samples * 2 bytes.
        assert_eq!(
            pcm_default_packet_size(16, 44100, 1, 2, 705_600).unwrap(),
            8192
        );
        // 8 kHz, mono, u8: 800 -> 512 samples * 1 byte.
        assert_eq!(pcm_default_packet_size(8, 8000, 1, 1, 64_000).unwrap(), 512);
        // A zero block align is rejected, like ff_pcm_default_packet_size().
        assert!(pcm_default_packet_size(16, 48000, 2, 0, 0).is_err());
    }

    #[test]
    fn probe_matches_ffmpeg() {
        let factory = WavDemuxerFactory;
        let mut buf = vec![0u8; 64];
        buf[0..4].copy_from_slice(b"RIFF");
        buf[8..12].copy_from_slice(b"WAVE");
        let probe = ProbeData {
            filename: "test.wav",
            buf: &buf,
        };
        assert_eq!(factory.probe(&probe), PROBE_SCORE_MAX - 1);

        buf[0..4].copy_from_slice(b"RF64");
        buf[12..16].copy_from_slice(b"ds64");
        let probe = ProbeData {
            filename: "test.wav",
            buf: &buf,
        };
        assert_eq!(factory.probe(&probe), PROBE_SCORE_MAX);

        let short = ProbeData {
            filename: "test.wav",
            buf: &buf[..32],
        };
        assert_eq!(factory.probe(&short), 0);
    }
}
