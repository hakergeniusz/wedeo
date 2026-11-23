//! framecrc output generator for wedeo.
//!
//! Produces output in FFmpeg's framecrc format. Operates in two modes:
//!
//! 1. **Packet passthrough** (audio/PCM): checksums raw packet data directly,
//!    matching FFmpeg's `-c copy` framecrc output.
//! 2. **Decode mode** (video): decodes packets through the codec, then checksums
//!    the decoded YUV frame data, matching FFmpeg's framecrc decode output.

use std::env;
use std::io::{self, Write};

// Ensure codec/format registrations are linked.
use wedeo_bsf_aac_adtstoasc as _;
use wedeo_bsf_h264_mp4toannexb as _;
use wedeo_bsf_hevc_mp4toannexb as _;
use wedeo_codec_flac_native as _;
use wedeo_codec_h264 as _;
use wedeo_codec_hevc as _;
use wedeo_codec_hevc_cabac as _;
use wedeo_codec_hevc_dsp as _;
use wedeo_codec_hevc_ps as _;
use wedeo_codec_hevc_refs as _;
use wedeo_codec_mpeg12 as _;
use wedeo_codec_mpegvideo as _;
use wedeo_codec_pcm as _;
use wedeo_codec_vp8 as _;
use wedeo_codec_vp9 as _;
use wedeo_filter_copy as _;
use wedeo_filter_crop as _;
use wedeo_filter_format as _;
use wedeo_filter_fps as _;
use wedeo_filter_graph as _;
use wedeo_filter_hflip as _;
use wedeo_filter_pad as _;
use wedeo_filter_transpose as _;
use wedeo_filter_vflip as _;
use wedeo_format_avi_demux as _;
use wedeo_format_flv_demux as _;
use wedeo_format_flv_mux as _;
use wedeo_format_h264 as _;
use wedeo_format_ivf as _;
use wedeo_format_ivf_mux as _;
use wedeo_format_matroska_mux as _;
use wedeo_format_mp4 as _;
use wedeo_format_mpegts_demux as _;
use wedeo_format_mpegts_mux as _;
use wedeo_format_ogg as _;
use wedeo_format_wav as _;
use wedeo_format_webm as _;
use wedeo_rav1d as _;
use wedeo_symphonia as _;

use wedeo_codec::decoder::{Decoder, DecoderBuilder};
use wedeo_core::error::Error;
use wedeo_core::frame::FramePlane;
use wedeo_core::media_type::MediaType;
use wedeo_core::packet::PacketFlags;
use wedeo_core::pixel_format::PixelFormat;
use wedeo_format::context::InputContext;

fn init_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_target(true)
        .with_writer(std::io::stderr)
        .init();
}

/// Adler-32 checksum matching FFmpeg's av_adler32_update(0, ...).
/// FFmpeg passes initial adler=0, giving s1=0, s2=0 (NOT the standard init of s1=1).
fn adler32(data: &[u8]) -> u32 {
    let mut hasher = adler2::Adler32::from_checksum(0);
    hasher.write_slice(data);
    hasher.checksum()
}

/// Check if a stream requires decoding (as opposed to packet passthrough).
/// Video streams always require decoding for framecrc conformance output.
fn needs_decode(media_type: MediaType) -> bool {
    media_type == MediaType::Video
}

/// Plane geometry for the planar YUV formats we can dump as rawvideo.
///
/// log2_chroma_w/log2_chroma_h are FFmpeg's av_pix_fmt_desc_get() fields:
/// "Amount to shift the luma width right to find the chroma width." /
/// "Amount to shift the luma height right to find the chroma height."
#[derive(Clone, Copy)]
struct YuvGeometry {
    log2_chroma_w: u32,
    log2_chroma_h: u32,
    bytes_per_sample: usize,
}

impl YuvGeometry {
    fn for_format(format: PixelFormat) -> Option<Self> {
        let (log2_chroma_w, log2_chroma_h, bytes_per_sample) = match format {
            PixelFormat::Yuv420p | PixelFormat::Yuvj420p => (1, 1, 1),
            PixelFormat::Yuv422p | PixelFormat::Yuvj422p => (1, 0, 1),
            PixelFormat::Yuv444p | PixelFormat::Yuvj444p => (0, 0, 1),
            PixelFormat::Yuv410p => (2, 2, 1),
            PixelFormat::Yuv411p => (2, 0, 1),
            // FFmpeg rawvideo writes >8-bit planar samples in 16-bit containers.
            PixelFormat::Yuv420p10le | PixelFormat::Yuv420p10be => (1, 1, 2),
            PixelFormat::Yuv420p16le | PixelFormat::Yuv420p16be => (1, 1, 2),
            // Newly added planar variants (e.g. yuv422p12le) are derived from
            // the FFmpeg-style format name below.
            _ => {
                let name = format.name();
                let (log2_chroma_w, log2_chroma_h) = if name.contains("p420") {
                    (1, 1)
                } else if name.contains("p422") {
                    (1, 0)
                } else if name.contains("p444") {
                    (0, 0)
                } else if name.contains("p410") {
                    (2, 2)
                } else if name.contains("p411") {
                    (2, 0)
                } else {
                    return None;
                };
                // FFmpeg rawvideo packs 9-16 bit samples in 16-bit containers.
                let bytes_per_sample = if name
                    .split_once('p')
                    .is_some_and(|(_, rest)| rest.chars().any(|c| c.is_ascii_digit()))
                {
                    2
                } else {
                    1
                };
                return Some(Self {
                    log2_chroma_w,
                    log2_chroma_h,
                    bytes_per_sample,
                });
            }
        };
        Some(Self {
            log2_chroma_w,
            log2_chroma_h,
            bytes_per_sample,
        })
    }
}

/// Copy one plane row-by-row into a tightly packed buffer, matching FFmpeg rawvideo.
/// Returns None if the frame buffer is too small (would otherwise read garbage).
fn pack_plane(
    plane: &FramePlane,
    width: usize,
    height: usize,
    bytes_per_sample: usize,
) -> Option<Vec<u8>> {
    let row_bytes = width.checked_mul(bytes_per_sample)?;
    let mut packed = Vec::with_capacity(row_bytes.checked_mul(height)?);
    let data = plane.buffer.data();
    for row in 0..height {
        let start = plane.offset.checked_add(row.checked_mul(plane.linesize)?)?;
        let end = start.checked_add(row_bytes)?;
        packed.extend_from_slice(data.get(start..end)?);
    }
    Some(packed)
}

fn pack_plane_or_exit(
    plane: &FramePlane,
    width: usize,
    height: usize,
    bytes_per_sample: usize,
    label: &str,
) -> Vec<u8> {
    pack_plane(plane, width, height, bytes_per_sample).unwrap_or_else(|| {
        eprintln!(
            "Error: {label} plane is too small for {width}x{height} at {bytes_per_sample} byte(s)/sample; refusing to write wrong YUV bytes"
        );
        std::process::exit(1);
    })
}

fn main() {
    init_tracing();

    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: wedeo-framecrc <input-file>");
        std::process::exit(1);
    }

    // Optional: --raw-yuv <output.yuv> to dump raw decoded frames
    let raw_yuv_path = if args.len() >= 4 && args[2] == "--raw-yuv" {
        Some(args[3].clone())
    } else {
        None
    };
    let mut raw_yuv_file: Option<std::fs::File> = raw_yuv_path.as_ref().map(|p| {
        std::fs::File::create(p).unwrap_or_else(|e| {
            eprintln!("Error creating {p}: {e}");
            std::process::exit(1);
        })
    });

    let path = &args[1];
    let mut ctx = match InputContext::open(path) {
        Ok(ctx) => ctx,
        Err(e) => {
            eprintln!("Error opening {path}: {e}");
            std::process::exit(1);
        }
    };

    let stdout = io::stdout();
    let mut out = stdout.lock();

    // Determine which streams need decoding and create decoders for them.
    let mut decoders: Vec<Option<Box<dyn Decoder>>> = Vec::new();
    let mut decode_stream_indices: Vec<bool> = Vec::new();

    for stream in &ctx.streams {
        let cp = &stream.codec_params;
        let i = stream.index;
        let tb = &stream.time_base;

        if needs_decode(cp.media_type) {
            // Video stream: print decode-mode headers
            writeln!(out, "#tb {i}: {}/{}", tb.num, tb.den).unwrap();
            writeln!(out, "#media_type {i}: video").unwrap();
            writeln!(out, "#codec_id {i}: rawvideo").unwrap();
            writeln!(out, "#dimensions {i}: {}x{}", cp.width, cp.height).unwrap();
            // SAR: use 0/1 if not set (matching FFmpeg)
            writeln!(out, "#sar {i}: 0/1").unwrap();

            // Create a decoder for this stream
            match DecoderBuilder::new(cp.clone()).open() {
                Ok(dec) => {
                    decoders.push(Some(dec));
                    decode_stream_indices.push(true);
                }
                Err(e) => {
                    eprintln!("Error creating decoder for stream {i}: {e}");
                    std::process::exit(1);
                }
            }
        } else {
            // Audio / other stream: print passthrough headers
            writeln!(out, "#tb {i}: {}/{}", tb.num, tb.den).unwrap();
            writeln!(out, "#media_type {i}: {}", cp.media_type).unwrap();
            writeln!(out, "#codec_id {i}: {}", cp.codec_id.name()).unwrap();
            if cp.media_type == MediaType::Audio {
                writeln!(out, "#sample_rate {i}: {}", cp.sample_rate).unwrap();
                writeln!(out, "#channel_layout_name {i}: {}", cp.channel_layout).unwrap();
            }
            decoders.push(None);
            decode_stream_indices.push(false);
        }
    }

    // Demux + decode/passthrough loop
    loop {
        match ctx.read_packet() {
            Ok(packet) => {
                let si = packet.stream_index;

                if si < decode_stream_indices.len() && decode_stream_indices[si] {
                    // Decode mode: feed packet to decoder, output decoded frames
                    let decoder = decoders[si].as_mut().unwrap();
                    if let Err(e) = decoder.send_packet(Some(&packet)) {
                        eprintln!("Error decoding packet for stream {si}: {e}");
                        std::process::exit(1);
                    }
                    drain_frames(decoder.as_mut(), si, &mut out, &mut raw_yuv_file);
                } else {
                    // Passthrough mode: checksum raw packet data
                    let data = packet.data.data();
                    let crc = adler32(data);
                    let size = data.len();

                    let mut line = format!(
                        "{si}, {:>10}, {:>10}, {:>8}, {:>8}, 0x{crc:08x}",
                        packet.dts, packet.pts, packet.duration, size,
                    );

                    if !packet.flags.contains(PacketFlags::KEY) {
                        line.push_str(&format!(", F=0x{:04X}", packet.flags.bits()));
                    }

                    writeln!(out, "{line}").unwrap();
                }
            }
            Err(Error::Eof) => {
                // Drain all decoders
                for (si, dec_opt) in decoders.iter_mut().enumerate() {
                    if let Some(decoder) = dec_opt {
                        let _ = decoder.send_packet(None);
                        drain_frames(decoder.as_mut(), si, &mut out, &mut raw_yuv_file);
                    }
                }
                break;
            }
            Err(e) => {
                eprintln!("Error reading packet: {e}");
                std::process::exit(1);
            }
        }
    }
}

/// Drain all available frames from a decoder and write framecrc lines.
fn drain_frames(
    decoder: &mut dyn Decoder,
    si: usize,
    out: &mut impl Write,
    raw_yuv_file: &mut Option<std::fs::File>,
) {
    loop {
        match decoder.receive_frame() {
            Ok(frame) => {
                if let Some(video) = frame.video() {
                    let geom = match YuvGeometry::for_format(video.format) {
                        Some(geom) => geom,
                        None => {
                            eprintln!(
                                "Error: unsupported pixel format {} for framecrc/raw-yuv output (no plane geometry)",
                                video.format.name()
                            );
                            std::process::exit(1);
                        }
                    };

                    // Concatenate Y+U+V plane data for checksum.
                    // Each plane is stored row-by-row with linesize that may
                    // differ from the actual width, so we copy row-by-row.
                    let width = video.width as usize;
                    let height = video.height as usize;
                    let chroma_width = width >> geom.log2_chroma_w;
                    let chroma_height = height >> geom.log2_chroma_h;

                    if video.planes.len() < 3 {
                        eprintln!(
                            "Error: expected 3 YUV planes for {}, got {}",
                            video.format.name(),
                            video.planes.len()
                        );
                        std::process::exit(1);
                    }

                    let y = pack_plane_or_exit(
                        &video.planes[0],
                        width,
                        height,
                        geom.bytes_per_sample,
                        "Y",
                    );
                    let u = pack_plane_or_exit(
                        &video.planes[1],
                        chroma_width,
                        chroma_height,
                        geom.bytes_per_sample,
                        "U",
                    );
                    let v = pack_plane_or_exit(
                        &video.planes[2],
                        chroma_width,
                        chroma_height,
                        geom.bytes_per_sample,
                        "V",
                    );

                    let mut raw = Vec::with_capacity(y.len() + u.len() + v.len());
                    raw.extend_from_slice(&y);
                    raw.extend_from_slice(&u);
                    raw.extend_from_slice(&v);

                    // Optionally dump raw YUV
                    if let Some(f) = raw_yuv_file.as_mut() {
                        use std::io::Write;
                        f.write_all(&raw).unwrap();
                    }

                    let crc = adler32(&raw);
                    let size = raw.len();

                    writeln!(
                        out,
                        "{si}, {:>10}, {:>10}, {:>8}, {:>8}, 0x{crc:08x}",
                        frame.pts, frame.pts, 1, size,
                    )
                    .unwrap();
                }
            }
            Err(Error::Again) => break,
            Err(Error::Eof) => break,
            Err(e) => {
                eprintln!("Error receiving frame from stream {si}: {e}");
                std::process::exit(1);
            }
        }
    }
}
