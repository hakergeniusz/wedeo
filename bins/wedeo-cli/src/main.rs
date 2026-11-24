// wedeo-cli — FFmpeg-style command-line interface.
//
// Supports:
//   wedeo-cli -i input.mp4                              (probe, like ffprobe)
//   wedeo-cli -i input.mp4 output.wav                   (transcode)
//   wedeo-cli -i input.mp4 -c:v av1 output.mp4          (transcode with codec selection)
//   wedeo-cli -i input.mp4 -c copy -y output.mkv        (remux, bit-exact)
//   wedeo-cli -i a.mp4 -i b.mp4 -map 1:v:0 -c copy out.mkv (multi-input mapping)
//   wedeo-cli -i in.mp4 -filter_complex '[0:v]hflip[v]' -map '[v]' out.mp4
//   wedeo-cli -ss 1 -t 2 -vf hflip -i input.mp4 out.mp4 (time window + filters)
//   wedeo-cli -codecs / -formats                        (list registered codecs/formats)
//
// Reference: FFmpeg fftools/ffmpeg_opt.c option table, opt_map() and
// opt_filter_complex(), fftools/cmdutils.c parse_time(),
// fftools/ffmpeg_filter.c graph setup.

use std::env;
use std::io;
use std::path::Path;
use std::process;
use std::time::Instant;

// Ensure inventory registrations are linked in.
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
use wedeo_format_hevc as _;
use wedeo_format_ivf as _;
use wedeo_format_ivf_mux as _;
use wedeo_format_matroska_mux as _;
use wedeo_format_mp4 as _;
use wedeo_format_mpegts_demux as _;
use wedeo_format_mpegts_mux as _;
use wedeo_format_ogg as _;
use wedeo_format_wav as _;
use wedeo_rav1d as _;
use wedeo_rav1e as _;
use wedeo_symphonia as _;

use wedeo_codec::decoder::{Decoder, DecoderBuilder};
use wedeo_codec::encoder::{CodecFlags, Encoder, EncoderBuilder};
use wedeo_codec::registry as codec_registry;
use wedeo_core::codec_id::CodecId;
use wedeo_core::error::Error;
use wedeo_core::frame::Frame;
use wedeo_core::media_type::MediaType;
use wedeo_core::packet::Packet;
use wedeo_core::rational::{Rational, reduce, rescale_q};
use wedeo_core::timestamp::NOPTS_VALUE;
use wedeo_filter_graph::{FilterLinkConfig, Graph};
use wedeo_format::context::{InputContext, OutputContext};
use wedeo_format::demuxer::{ProbeData, Stream};
use wedeo_format::registry as format_registry;

// ---------------------------------------------------------------------------
// Parsed CLI options
// ---------------------------------------------------------------------------

/// Where an option appeared. FFmpeg's `OPT_INPUT` / `OPT_OUTPUT` flags decide
/// which file an option applies to; with one input, anything before `-i` is an
/// input option and anything after it is an output option.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OptionScope {
    Input(usize),
    Output,
    Global,
}

/// One recognized option, kept so scoping is observable and testable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopedOption {
    pub scope: OptionScope,
    pub key: String,
    pub value: String,
}

/// `-map [-]file_id[:stream_specifier]` stream selector, or a labelled
/// `-filter_complex` output link such as `[v]` (FFmpeg opt_map(),
/// fftools/ffmpeg_opt.c:562).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MapTarget {
    /// The Nth video stream of the given input file.
    Video { file: usize, index: u32 },
    /// The Nth audio stream of the given input file.
    Audio { file: usize, index: u32 },
    /// A bare file id: the default (first video and first audio) streams.
    All { file: usize },
    /// A labelled output of `-filter_complex`, e.g. `[v]`.
    Label(String),
}

impl MapTarget {
    /// Input file index for stream-based maps, `None` for filter labels.
    pub fn file(&self) -> Option<usize> {
        match self {
            MapTarget::Video { file, .. }
            | MapTarget::Audio { file, .. }
            | MapTarget::All { file } => Some(*file),
            MapTarget::Label(_) => None,
        }
    }
}

/// One filter in a comma-separated `-vf` / `-af` chain, with named options.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilterSpec {
    pub name: String,
    pub args: Vec<(String, String)>,
}

/// One `[in]filter[,filter...][out]` chain of a `-filter_complex` graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilterChain {
    /// Input link labels, e.g. `0:v` or a previously defined output label.
    pub inputs: Vec<String>,
    pub filters: Vec<FilterSpec>,
    /// Output link labels, e.g. `v` for a later `-map [v]`.
    pub outputs: Vec<String>,
}

/// Parsed `-filter_complex` graph (semicolon-separated chains).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilterComplexGraph {
    pub chains: Vec<FilterChain>,
}

impl FilterComplexGraph {
    /// All labelled outputs available to `-map [label]`.
    pub fn output_labels(&self) -> Vec<&str> {
        self.chains
            .iter()
            .flat_map(|chain| chain.outputs.iter().map(String::as_str))
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliOptions {
    pub inputs: Vec<String>,
    pub output: Option<String>,
    pub video_codec: Option<String>,
    pub audio_codec: Option<String>,
    pub subtitle_codec: Option<String>,
    pub format: Option<String>,
    pub bitrate_video: Option<i64>,
    pub bitrate_audio: Option<i64>,
    pub overwrite: bool,
    pub no_overwrite: bool,
    pub frames_video: Option<u64>,
    pub codec_options: Vec<(String, String)>,
    pub loglevel: String,
    // Special modes
    pub list_codecs: bool,
    pub list_formats: bool,
    pub list_decoders: bool,
    pub list_encoders: bool,
    pub help: bool,
    pub hide_banner: bool,
    pub stats: bool,
    pub shortest: bool,
    pub copy_video: bool,
    pub copy_audio: bool,
    pub video_disabled: bool,
    pub audio_disabled: bool,
    pub subtitle_disabled: bool,
    // Time / rate / mapping / filters
    pub seek_us: Option<i64>,
    pub duration_us: Option<i64>,
    pub frame_rate: Option<String>,
    pub video_filters: Vec<FilterSpec>,
    pub audio_filters: Vec<FilterSpec>,
    /// Parsed graph description from `-filter_complex` / `-lavfi`.
    pub filter_complex: Option<FilterComplexGraph>,
    /// `-threads`: `None` means auto (let the encoder decide).
    pub thread_count: Option<u32>,
    pub maps: Vec<MapTarget>,
    pub metadata: Vec<(String, String)>,
    pub movflags: Vec<String>,
    /// Every recognized option with the scope in which it was parsed.
    pub scoped_options: Vec<ScopedOption>,
    /// Set instead of exiting from the parser so tests can inspect errors.
    pub parse_error: Option<String>,
}

impl CliOptions {
    pub fn new() -> Self {
        Self {
            inputs: Vec::new(),
            output: None,
            video_codec: None,
            audio_codec: None,
            subtitle_codec: None,
            format: None,
            bitrate_video: None,
            bitrate_audio: None,
            overwrite: false,
            no_overwrite: false,
            frames_video: None,
            codec_options: Vec::new(),
            loglevel: "warn".to_string(),
            list_codecs: false,
            list_formats: false,
            list_decoders: false,
            list_encoders: false,
            help: false,
            hide_banner: false,
            stats: false,
            shortest: false,
            copy_video: false,
            copy_audio: false,
            video_disabled: false,
            audio_disabled: false,
            subtitle_disabled: false,
            seek_us: None,
            duration_us: None,
            frame_rate: None,
            video_filters: Vec::new(),
            audio_filters: Vec::new(),
            filter_complex: None,
            thread_count: None,
            maps: Vec::new(),
            metadata: Vec::new(),
            movflags: Vec::new(),
            scoped_options: Vec::new(),
            parse_error: None,
        }
    }

    fn fail(&mut self, msg: impl Into<String>) {
        if self.parse_error.is_none() {
            self.parse_error = Some(msg.into());
        }
    }

    fn record(&mut self, scope: OptionScope, key: &str, value: &str) {
        self.scoped_options.push(ScopedOption {
            scope,
            key: key.to_string(),
            value: value.to_string(),
        });
    }

    /// Last value of `key` given before `-i`.
    pub fn input_option(&self, key: &str) -> Option<&str> {
        self.scoped_options
            .iter()
            .rev()
            .find(|o| o.key == key && matches!(o.scope, OptionScope::Input(_)))
            .map(|o| o.value.as_str())
    }

    /// Last value of `key` given after `-i`.
    pub fn output_option(&self, key: &str) -> Option<&str> {
        self.scoped_options
            .iter()
            .rev()
            .find(|o| o.key == key && o.scope == OptionScope::Output)
            .map(|o| o.value.as_str())
    }

    /// Output value if present, otherwise the input-side value.
    pub fn effective_option(&self, key: &str) -> Option<&str> {
        self.output_option(key).or_else(|| self.input_option(key))
    }
}

impl Default for CliOptions {
    fn default() -> Self {
        Self::new()
    }
}

/// Scope of the option at value index `i`: options followed by another `-i`
/// belong to the input being opened, everything after the final input is an
/// output option (fftools/ffmpeg_opt.c).
fn scope_at(args: &[String], i: usize, input_index: usize) -> OptionScope {
    if args[i + 1..].iter().any(|a| a == "-i") {
        OptionScope::Input(input_index)
    } else {
        OptionScope::Output
    }
}

/// Parse arguments without ever exiting; errors are reported in
/// `CliOptions::parse_error` so they can be asserted in unit tests.
pub fn parse_args_from(args: &[String]) -> CliOptions {
    let mut opts = CliOptions::new();
    let mut i = 0usize;
    let mut input_index = 0usize;

    macro_rules! take_value {
        ($display:expr) => {{
            i += 1;
            if i >= args.len() {
                opts.fail(format!("Option {} requires an argument", $display));
                return opts;
            }
            args[i].clone()
        }};
    }

    while i < args.len() {
        let arg = args[i].as_str();

        // Positional argument: the output file.
        if arg == "-" || !arg.starts_with('-') {
            if opts.output.is_some() {
                opts.fail(format!("Unexpected argument: {arg}"));
                return opts;
            }
            opts.output = Some(arg.to_string());
            i += 1;
            continue;
        }

        match arg {
            "-i" => {
                let value = take_value!("-i");
                opts.record(OptionScope::Input(input_index), "-i", &value);
                opts.inputs.push(value);
                input_index += 1;
            }

            "-c:v" | "-vcodec" | "-codec:v" | "-c:V" => {
                let value = take_value!(arg);
                opts.record(scope_at(args, i, input_index), arg, &value);
                set_codec(&mut opts, MediaType::Video, &value);
            }
            "-c:a" | "-acodec" | "-codec:a" => {
                let value = take_value!(arg);
                opts.record(scope_at(args, i, input_index), arg, &value);
                set_codec(&mut opts, MediaType::Audio, &value);
            }
            "-c:s" | "-scodec" | "-codec:s" => {
                let value = take_value!(arg);
                opts.record(scope_at(args, i, input_index), arg, &value);
                if value == "copy" {
                    opts.subtitle_codec = Some("copy".to_string());
                } else {
                    opts.subtitle_codec = Some(value);
                }
            }
            "-c" | "-codec" => {
                let value = take_value!(arg);
                opts.record(scope_at(args, i, input_index), arg, &value);
                set_codec(&mut opts, MediaType::Video, &value);
                set_codec(&mut opts, MediaType::Audio, &value);
            }

            "-f" | "-format" => {
                let value = take_value!(arg);
                opts.record(scope_at(args, i, input_index), arg, &value);
                opts.format = Some(value);
            }

            "-b:v" | "-vb" | "-b" => {
                let value = take_value!(arg);
                opts.record(scope_at(args, i, input_index), arg, &value);
                opts.bitrate_video = Some(parse_bitrate(&value));
            }
            "-b:a" | "-ab" => {
                let value = take_value!(arg);
                opts.record(scope_at(args, i, input_index), arg, &value);
                opts.bitrate_audio = Some(parse_bitrate(&value));
            }

            "-q:v" | "-qscale:v" | "-q" => {
                let value = take_value!(arg);
                opts.record(scope_at(args, i, input_index), arg, &value);
                // FFmpeg stores qscale as a codec option; `qp` is the name
                // used by the AV1 encoder adapter in Wedeo.
                opts.codec_options.push(("q:v".into(), value.clone()));
                opts.codec_options.push(("qp".into(), value));
            }
            "-q:a" | "-qscale:a" => {
                let value = take_value!(arg);
                opts.record(scope_at(args, i, input_index), arg, &value);
                opts.codec_options.push(("q:a".into(), value));
            }
            "-crf" => {
                let value = take_value!(arg);
                opts.record(scope_at(args, i, input_index), arg, &value);
                opts.codec_options.push(("crf".into(), value.clone()));
                opts.codec_options.push(("qp".into(), value));
            }
            "-preset" => {
                let value = take_value!(arg);
                opts.record(scope_at(args, i, input_index), arg, &value);
                opts.codec_options.push(("preset".into(), value));
            }
            "-threads" => {
                let value = take_value!(arg);
                opts.record(scope_at(args, i, input_index), arg, &value);
                let normalized = if value == "auto" {
                    "0".to_string()
                } else {
                    value.clone()
                };
                match normalized.parse::<u32>() {
                    Ok(count) => opts.thread_count = Some(count),
                    Err(_) => {
                        opts.fail(format!("Invalid threads value: '{value}'"));
                        return opts;
                    }
                }
                // Keep the codec-private option too: encoders that read
                // `options.get_thread_count()` still see `-threads`.
                opts.codec_options.push(("threads".into(), normalized));
            }

            "-frames:v" | "-vframes" => {
                let value = take_value!(arg);
                opts.record(scope_at(args, i, input_index), arg, &value);
                match value.parse::<u64>() {
                    Ok(n) => opts.frames_video = Some(n),
                    Err(_) => {
                        opts.fail(format!("Invalid frame count for option {arg}: '{value}'"));
                        return opts;
                    }
                }
            }

            "-r" => {
                let value = take_value!(arg);
                opts.record(scope_at(args, i, input_index), arg, &value);
                if let Err(e) = parse_frame_rate(&value) {
                    opts.fail(format!("Invalid frame rate for option -r: {e}"));
                    return opts;
                }
                opts.frame_rate = Some(value);
            }
            "-ss" => {
                let value = take_value!(arg);
                opts.record(scope_at(args, i, input_index), arg, &value);
                match parse_time(&value) {
                    Ok(us) => opts.seek_us = Some(us),
                    Err(e) => {
                        opts.fail(format!("Invalid time for option -ss: {e}"));
                        return opts;
                    }
                }
            }
            "-t" => {
                let value = take_value!(arg);
                opts.record(scope_at(args, i, input_index), arg, &value);
                match parse_time(&value) {
                    Ok(us) => opts.duration_us = Some(us),
                    Err(e) => {
                        opts.fail(format!("Invalid duration for option -t: {e}"));
                        return opts;
                    }
                }
            }

            "-vf" | "-filter:v" => {
                let value = take_value!(arg);
                opts.record(scope_at(args, i, input_index), arg, &value);
                match parse_filter_chain(&value, true) {
                    Ok(specs) => opts.video_filters.extend(specs),
                    Err(e) => {
                        opts.fail(format!("Invalid filter chain for {arg}: {e}"));
                        return opts;
                    }
                }
            }
            "-af" | "-filter:a" => {
                let value = take_value!(arg);
                opts.record(scope_at(args, i, input_index), arg, &value);
                match parse_filter_chain(&value, false) {
                    Ok(specs) => opts.audio_filters.extend(specs),
                    Err(e) => {
                        opts.fail(format!("Invalid filter chain for {arg}: {e}"));
                        return opts;
                    }
                }
            }

            "-map" => {
                let value = take_value!(arg);
                opts.record(scope_at(args, i, input_index), arg, &value);
                match parse_map_spec(&value) {
                    Ok(target) => opts.maps.push(target),
                    Err(e) => {
                        opts.fail(format!("Invalid stream specifier for -map: {e}"));
                        return opts;
                    }
                }
            }

            // FFmpeg treats -lavfi as an alias of -filter_complex
            // (fftools/ffmpeg_opt.c option table).
            "-filter_complex" | "-lavfi" => {
                let value = take_value!(arg);
                opts.record(scope_at(args, i, input_index), arg, &value);
                match parse_filter_complex(&value) {
                    Ok(graph) => opts.filter_complex = Some(graph),
                    Err(e) => {
                        opts.fail(format!("Invalid filter graph for {arg}: {e}"));
                        return opts;
                    }
                }
            }

            "-metadata" => {
                let value = take_value!(arg);
                opts.record(scope_at(args, i, input_index), arg, &value);
                match value.split_once('=') {
                    Some((k, v)) if !k.is_empty() => {
                        opts.metadata.push((k.to_string(), v.to_string()));
                    }
                    _ => {
                        opts.fail(format!(
                            "Invalid metadata value for -metadata: '{value}' (expected key=value)"
                        ));
                        return opts;
                    }
                }
            }
            "-movflags" => {
                let value = take_value!(arg);
                opts.record(scope_at(args, i, input_index), arg, &value);
                opts.movflags.extend(
                    value
                        .split('+')
                        .filter(|flag| !flag.is_empty())
                        .map(str::to_string),
                );
            }

            // Global flags: accepted before or after -i, like FFmpeg.
            "-y" => {
                opts.record(OptionScope::Global, arg, "1");
                opts.overwrite = true;
                opts.no_overwrite = false;
            }
            "-n" => {
                opts.record(OptionScope::Global, arg, "1");
                opts.no_overwrite = true;
                opts.overwrite = false;
            }
            "-hide_banner" => {
                opts.record(OptionScope::Global, arg, "1");
                opts.hide_banner = true;
            }
            "-stats" => {
                opts.record(OptionScope::Global, arg, "1");
                opts.stats = true;
            }
            "-vn" => {
                opts.record(scope_at(args, i, input_index), arg, "1");
                opts.video_disabled = true;
            }
            "-an" => {
                opts.record(scope_at(args, i, input_index), arg, "1");
                opts.audio_disabled = true;
            }
            "-sn" => {
                opts.record(scope_at(args, i, input_index), arg, "1");
                opts.subtitle_disabled = true;
            }
            "-shortest" => {
                opts.record(scope_at(args, i, input_index), arg, "1");
                opts.shortest = true;
            }
            "-v" | "-loglevel" => {
                let value = take_value!(arg);
                opts.record(OptionScope::Global, arg, &value);
                opts.loglevel = value;
            }

            "-codecs" | "-formats" | "-decoders" | "-encoders" => {
                opts.record(OptionScope::Global, arg, "1");
                match arg {
                    "-codecs" => opts.list_codecs = true,
                    "-formats" => opts.list_formats = true,
                    "-decoders" => opts.list_decoders = true,
                    _ => opts.list_encoders = true,
                }
            }

            "-h" | "-help" | "--help" => {
                opts.record(OptionScope::Global, arg, "1");
                opts.help = true;
            }

            // Codec-private options: -key value (anything starting with -).
            other if other.len() > 1 => {
                let key = other.trim_start_matches('-').to_string();
                i += 1;
                if i >= args.len() || args[i].starts_with('-') {
                    opts.fail(format!("Unrecognized option '{other}': expected a value"));
                    return opts;
                }
                let value = args[i].clone();
                opts.record(scope_at(args, i, input_index), other, &value);
                opts.codec_options.push((key, value));
            }

            _ => {
                opts.fail(format!("Unrecognized option '{arg}'"));
                return opts;
            }
        }
        i += 1;
    }

    // At most one video and one audio stream can be mapped per input file.
    let mut mapped_video: Vec<(usize, u32)> = Vec::new();
    let mut mapped_audio: Vec<(usize, u32)> = Vec::new();
    let maps = opts.maps.clone();
    for target in &maps {
        match target {
            MapTarget::Video { file, index } => {
                if !note_mapped(&mut mapped_video, *file, *index, "video", &mut opts) {
                    return opts;
                }
            }
            MapTarget::Audio { file, index } => {
                if !note_mapped(&mut mapped_audio, *file, *index, "audio", &mut opts) {
                    return opts;
                }
            }
            MapTarget::All { file } => {
                // A bare file id maps the first video and first audio streams.
                if !note_mapped(&mut mapped_video, *file, 0, "video", &mut opts) {
                    return opts;
                }
                if !note_mapped(&mut mapped_audio, *file, 0, "audio", &mut opts) {
                    return opts;
                }
            }
            MapTarget::Label(_) => {}
        }
    }

    opts
}

/// Record `(file, index)` for one kind of stream map, rejecting a second,
/// different stream for the same file.
fn note_mapped(
    maps: &mut Vec<(usize, u32)>,
    file: usize,
    index: u32,
    kind: &str,
    opts: &mut CliOptions,
) -> bool {
    if let Some(prev) = maps.iter().find(|(f, _)| *f == file) {
        if prev.1 != index {
            opts.fail(format!("Only one {kind} stream can be mapped with -map"));
            return false;
        }
        return true; // Same file and stream: already mapped.
    }
    maps.push((file, index));
    true
}

fn set_codec(opts: &mut CliOptions, media_type: MediaType, value: &str) {
    let slot = match media_type {
        MediaType::Video => &mut opts.video_codec,
        MediaType::Audio => &mut opts.audio_codec,
        _ => &mut opts.subtitle_codec,
    };
    if value == "copy" {
        match media_type {
            MediaType::Video => opts.copy_video = true,
            MediaType::Audio => opts.copy_audio = true,
            _ => {}
        }
        *slot = None;
    } else {
        *slot = Some(value.to_string());
    }
}

pub fn parse_args() -> CliOptions {
    let args: Vec<String> = env::args().skip(1).collect();

    if args.is_empty() {
        print_usage();
        process::exit(0);
    }

    let opts = parse_args_from(&args);
    if let Some(err) = &opts.parse_error {
        die(err);
    }
    opts
}

// ---------------------------------------------------------------------------
// FFmpeg-style option value parsers
// ---------------------------------------------------------------------------

/// FFmpeg `av_parse_time()` subset: plain seconds, `HH:MM:SS[.frac]`,
/// `MM:SS[.frac]`, and the `ms`, `s`, `m`, `h` suffixes. Returns microseconds
/// (AV_TIME_BASE units), like `InputContext::duration`.
pub fn parse_time(s: &str) -> Result<i64, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("empty time value".to_string());
    }

    let (negative, body) = match s.as_bytes()[0] {
        b'-' => (true, &s[1..]),
        b'+' => (false, &s[1..]),
        _ => (false, s),
    };
    if body.is_empty() {
        return Err(format!("'{s}'"));
    }

    let seconds = if body.contains(':') {
        let parts: Vec<&str> = body.split(':').collect();
        if parts.len() != 2 && parts.len() != 3 {
            return Err(format!("'{s}'"));
        }
        let mut total = 0.0f64;
        for part in parts {
            let val: f64 = part.parse().map_err(|_| format!("'{s}'"))?;
            if !val.is_finite() {
                return Err(format!("'{s}'"));
            }
            total = total * 60.0 + val;
        }
        total
    } else {
        let lower = body.to_ascii_lowercase();
        let (number, unit) = if let Some(v) = lower.strip_suffix("ms") {
            (v, 0.001)
        } else if let Some(v) = lower.strip_suffix("us") {
            (v, 0.000_001)
        } else if let Some(v) = lower.strip_suffix('s') {
            (v, 1.0)
        } else if let Some(v) = lower.strip_suffix('m') {
            (v, 60.0)
        } else if let Some(v) = lower.strip_suffix('h') {
            (v, 3600.0)
        } else {
            (body, 1.0)
        };
        let val: f64 = number.parse().map_err(|_| format!("'{s}'"))?;
        if !val.is_finite() {
            return Err(format!("'{s}'"));
        }
        val * unit
    };

    let us = seconds * 1_000_000.0;
    if !us.is_finite() || us > i64::MAX as f64 || us < i64::MIN as f64 {
        return Err(format!("time value out of range: '{s}'"));
    }
    let us = us.round() as i64;
    Ok(if negative { -us } else { us })
}

/// Parse `-r` values such as `30`, `30000/1001` or `29.97`.
pub fn parse_frame_rate(s: &str) -> Result<Rational, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("empty frame rate".to_string());
    }
    let (num, den) = if let Some((n, d)) = s.split_once('/') {
        let n: i64 = n.parse().map_err(|_| format!("'{s}'"))?;
        let d: i64 = d.parse().map_err(|_| format!("'{s}'"))?;
        if n <= 0 || d <= 0 {
            return Err(format!("'{s}'"));
        }
        (n, d)
    } else {
        let v: f64 = s.parse().map_err(|_| format!("'{s}'"))?;
        if !v.is_finite() || v <= 0.0 || v > i32::MAX as f64 {
            return Err(format!("'{s}'"));
        }
        ((v * 1_000_000.0).round() as i64, 1_000_000i64)
    };
    let (num, den, _) = reduce(num, den, i32::MAX as i64);
    if num <= 0 || den <= 0 {
        return Err(format!("frame rate out of range: '{s}'"));
    }
    Ok(Rational::new(num, den))
}

/// FFmpeg `-map [-]file_id[:stream_specifier]`, or a labelled output of
/// `-filter_complex` such as `[v]` (FFmpeg opt_map(),
/// fftools/ffmpeg_opt.c:562).
pub fn parse_map_spec(spec: &str) -> Result<MapTarget, String> {
    if spec.starts_with('-') {
        return Err(format!(
            "negative stream mapping is not supported: '{spec}'"
        ));
    }
    if let Some(inner) = spec.strip_prefix('[') {
        let inner = inner
            .strip_suffix(']')
            .ok_or_else(|| format!("invalid -map label in '{spec}'"))?;
        if inner.is_empty()
            || inner
                .chars()
                .any(|c| "[] ,;:".contains(c) || c.is_whitespace())
        {
            return Err(format!("invalid -map label in '{spec}'"));
        }
        return Ok(MapTarget::Label(inner.to_string()));
    }
    let parts: Vec<&str> = spec.split(':').collect();
    let file: usize = parts[0]
        .parse()
        .map_err(|_| format!("invalid input file index in -map: '{spec}'"))?;
    match parts.len() {
        1 => Ok(MapTarget::All { file }),
        2 | 3 => {
            let index = if parts.len() == 3 {
                parts[2]
                    .parse::<u32>()
                    .map_err(|_| format!("invalid stream index in -map: '{spec}'"))?
            } else {
                0
            };
            match parts[1] {
                "v" => Ok(MapTarget::Video { file, index }),
                "a" => Ok(MapTarget::Audio { file, index }),
                "s" => Err(format!(
                    "subtitle stream mapping is not supported: '{spec}'"
                )),
                _ => Err(format!("unsupported stream specifier: '{spec}'")),
            }
        }
        _ => Err(format!("unsupported stream specifier: '{spec}'")),
    }
}

/// Parse a comma-separated `-vf` / `-af` chain into filter instances with
/// named options. `video == false` only allows the pass-through audio filters
/// that Wedeo currently registers.
pub fn parse_filter_chain(chain: &str, video: bool) -> Result<Vec<FilterSpec>, String> {
    if chain.trim().is_empty() {
        return Err("empty filter chain".to_string());
    }
    let mut specs = Vec::new();
    for element in chain.split(',') {
        let element = element.trim();
        if element.is_empty() {
            return Err(format!("empty filter in chain: '{chain}'"));
        }
        let allowed_video = [
            "hflip",
            "vflip",
            "crop",
            "transpose",
            "pad",
            "fps",
            "format",
            "copy",
            "null",
        ];
        let allowed_audio = ["anull", "copy"];
        let allowed = if video {
            &allowed_video[..]
        } else {
            &allowed_audio[..]
        };
        let spec = parse_filter_spec(element)?;
        if !allowed.contains(&spec.name.as_str()) {
            return Err(format!(
                "no such {}filter '{}' in chain '{chain}'",
                if video { "video " } else { "audio " },
                spec.name
            ));
        }
        specs.push(spec);
    }
    Ok(specs)
}

/// Parse one filter instance `name[:arg=value...]` or `name=arg...` with
/// positional `name:arg` values, used by `-vf`/`-af` chains and by
/// `-filter_complex` chains (FFmpeg `avfilter_graph_parse2`,
/// libavfilter/graphparser.c).
fn parse_filter_spec(element: &str) -> Result<FilterSpec, String> {
    let element = element.trim();
    if element.is_empty() {
        return Err("empty filter name".to_string());
    }
    let (name, rest) = match element.split_once('=') {
        Some((n, r)) => (n.trim(), Some(r.trim())),
        None => (element, None),
    };

    let values: Vec<&str> = match rest {
        None => Vec::new(),
        Some("") => return Err(format!("empty arguments for filter '{name}'")),
        Some(r) => r.split(':').map(str::trim).collect(),
    };
    let named = |keys: &[&str]| -> Result<Vec<(String, String)>, String> {
        if values.len() > keys.len() {
            return Err(format!(
                "too many arguments for filter '{name}': expected at most {}",
                keys.len()
            ));
        }
        if values.iter().all(|v| v.contains('=')) {
            let mut out = Vec::new();
            for value in &values {
                let (k, v) = value.split_once('=').expect("checked above");
                let k = k.trim();
                if !keys.contains(&k) {
                    return Err(format!("unknown option '{k}' for filter '{name}'"));
                }
                out.push((k.to_string(), v.trim().to_string()));
            }
            Ok(out)
        } else {
            Ok(keys
                .iter()
                .zip(values.iter())
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect())
        }
    };

    let args = match name {
        "hflip" | "vflip" | "copy" | "null" | "anull" => {
            if !values.is_empty() {
                return Err(format!("filter '{name}' takes no arguments"));
            }
            Vec::new()
        }
        "crop" => named(&["w", "h", "x", "y"])?,
        "pad" => named(&["w", "h", "x", "y", "color"])?,
        "transpose" => {
            let args = named(&["dir"])?;
            if let Some((_, value)) = args.first()
                && value.parse::<i32>().is_err()
            {
                return Err(format!("invalid transpose direction '{value}'"));
            }
            args
        }
        "fps" => {
            let args = named(&["fps"])?;
            if let Some((_, value)) = args.first() {
                parse_frame_rate(value).map_err(|e| format!("invalid fps value: {e}"))?;
            }
            args
        }
        "format" => named(&["pix_fmts"])?,
        // Filters outside the `-vf`/`-af` allowlists are parsed generically:
        // named `key=value` pairs keep their keys, positional values are kept
        // keyed by index so nothing is lost.
        _ => named_any(&values),
    };
    Ok(FilterSpec {
        name: name.to_string(),
        args,
    })
}

fn named_any(values: &[&str]) -> Vec<(String, String)> {
    if !values.is_empty() && values.iter().all(|v| v.contains('=')) {
        values
            .iter()
            .filter_map(|v| v.split_once('='))
            .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
            .collect()
    } else {
        values
            .iter()
            .enumerate()
            .map(|(i, v)| (i.to_string(), v.trim().to_string()))
            .collect()
    }
}

/// Parse a `-filter_complex` graph: chains separated by `;`, each chain
/// `[in]filter[,filter...][out]` (FFmpeg opt_filter_complex(),
/// fftools/ffmpeg_opt.c:441).
pub fn parse_filter_complex(graph: &str) -> Result<FilterComplexGraph, String> {
    let mut chains = Vec::new();
    for raw in graph.split(';') {
        let chain = raw.trim();
        if chain.is_empty() {
            return Err(format!("empty filter chain in '{graph}'"));
        }
        chains.push(parse_filter_complex_chain(chain)?);
    }
    if chains.is_empty() {
        return Err("empty filter graph".to_string());
    }
    Ok(FilterComplexGraph { chains })
}

/// Scan a `[label]` starting at byte offset `i`, returning the label and the
/// offset just past the closing bracket.
fn scan_chain_label(s: &str, i: usize) -> Result<Option<(String, usize)>, String> {
    if !s[i..].starts_with('[') {
        return Ok(None);
    }
    let rest = &s[i + 1..];
    let Some(end) = rest.find(']') else {
        return Err(format!("unterminated label in '{s}'"));
    };
    let label = &rest[..end];
    if label.is_empty() || label.contains(['[', ']', ',', ';']) {
        return Err(format!("invalid label '[{label}]' in '{s}'"));
    }
    Ok(Some((label.to_string(), i + 1 + end + 1)))
}

fn parse_filter_complex_chain(chain: &str) -> Result<FilterChain, String> {
    let mut inputs: Vec<String> = Vec::new();
    let mut outputs: Vec<String> = Vec::new();
    let mut filters: Vec<FilterSpec> = Vec::new();

    // Leading input labels, e.g. `0:v`.
    let mut i = 0usize;
    while let Some((label, end)) = scan_chain_label(chain, i)? {
        inputs.push(label);
        i = end;
    }
    // Filters run up to the next `[`; output labels follow them.
    let filter_end = chain[i..].find('[').map(|p| i + p).unwrap_or(chain.len());
    let filter_section = chain[i..filter_end].trim();
    if !filter_section.is_empty() {
        for element in filter_section.split(',') {
            if element.trim().is_empty() {
                return Err(format!("empty filter in chain: '{chain}'"));
            }
            filters.push(parse_filter_spec(element)?);
        }
    }
    // Trailing output labels, e.g. `v` for a later `-map [v]`.
    let mut j = filter_end;
    while let Some((label, end)) = scan_chain_label(chain, j)? {
        outputs.push(label);
        j = end;
    }
    if j != chain.len() {
        return Err(format!("unexpected text in filter chain '{chain}'"));
    }
    if filters.is_empty() {
        return Err(format!("empty filter chain: '{chain}'"));
    }
    Ok(FilterChain {
        inputs,
        filters,
        outputs,
    })
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn main() {
    let opts = parse_args();

    init_tracing(&opts.loglevel);

    if !opts.hide_banner {
        print_banner();
    }
    if opts.help {
        print_usage();
        return;
    }

    // List modes
    if opts.list_codecs {
        cmd_codecs();
        return;
    }
    if opts.list_formats {
        cmd_formats();
        return;
    }
    if opts.list_decoders {
        cmd_decoders();
        return;
    }
    if opts.list_encoders {
        cmd_encoders();
        return;
    }

    if opts.inputs.is_empty() {
        die("At least one input file is required. Use -i <file>.");
    }

    // No output file = probe mode (like ffprobe / ffmpeg -i with no output)
    if opts.output.is_none() {
        for input in &opts.inputs {
            if let Err(e) = cmd_probe(input) {
                eprintln!("Error probing {input}: {e}");
                process::exit(1);
            }
        }
        return;
    }

    // Transcode/remux mode
    if let Err(e) = cmd_transcode(&opts) {
        eprintln!("Error: {e}");
        process::exit(1);
    }
}

fn init_tracing(level: &str) {
    let filter = match level {
        "quiet" | "-8" => "off",
        "error" => "error",
        "warning" | "warn" => "warn",
        "info" => "info",
        "verbose" | "debug" => "debug",
        "trace" => "trace",
        other => other,
    };

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(filter)),
        )
        .with_target(true)
        .with_writer(io::stderr)
        .init();
}

fn print_banner() {
    eprintln!("ffmpeg version wedeo (pure Rust FFmpeg rewrite)");
    eprintln!(
        "  built on {} with the wedeo codec/format/filter crates",
        env!("CARGO_PKG_VERSION")
    );
    eprintln!("  configuration: --enable-wedeo --disable-ffmpeg-native");
}

fn print_usage() {
    println!("ffmpeg version wedeo (pure Rust FFmpeg rewrite)");
    println!();
    println!(
        "usage: wedeo-cli [global_options] [input_options] -i input_file [output_options] output_file"
    );
    println!("       wedeo-cli -i input_file                    (probe input)");
    println!("       wedeo-cli -codecs | -formats | -decoders | -encoders");
    println!();
    println!("Global options:");
    println!("  -y                     overwrite output files without asking");
    println!("  -n                     never overwrite output files");
    println!("  -hide_banner           suppress the startup banner");
    println!("  -stats                 print progress statistics");
    println!("  -v, -loglevel <level>  set logging level (quiet, error, warn, info, debug, trace)");
    println!("  -h, -help, --help      show this help");
    println!();
    println!("Input / output options:");
    println!("  -i <file>              input file (repeatable for multiple inputs)");
    println!("  -ss <time>             start at time (seconds, HH:MM:SS, or Ns/Nm/Nh/Nms)");
    println!("  -t <duration>          stop after duration");
    println!("  -map <spec>            map input stream: file[:v[:N]|a[:N]], or [label]");
    println!("  -filter_complex <gr>   build a filter graph; use -map [label] to select outputs");
    println!("  -lavfi <gr>            alias of -filter_complex");
    println!("  -c, -codec <codec>     select encoder for all streams ('copy' to remux)");
    println!("  -c:v, -vcodec <codec>  select video encoder ('copy' to remux video)");
    println!("  -c:a, -acodec <codec>  select audio encoder ('copy' to remux audio)");
    println!("  -c:s, -scodec <codec>  select subtitle codec (subtitles not supported yet)");
    println!("  -f <format>            force output container format");
    println!("  -b:v, -vb <bitrate>    video bitrate (e.g. 2M, 500k)");
    println!("  -b:a, -ab <bitrate>    audio bitrate");
    println!("  -q:v, -crf <value>     video quality (passed to the encoder)");
    println!("  -preset <name>         encoder preset");
    println!("  -threads <n|auto>      encoder thread count");
    println!("  -r <rate>              output frame rate (N, N/D or N.N)");
    println!("  -frames:v, -vframes <n> stop after n video frames");
    println!(
        "  -vf <chain>            video filter chain (hflip,vflip,crop,transpose,pad,fps,format)"
    );
    println!("  -af <chain>            audio filter chain (anull,copy)");
    println!("  -shortest              finish when the shortest selected stream ends");
    println!("  -vn / -an / -sn        disable video / audio / subtitle streams");
    println!("  -metadata key=value    set output metadata");
    println!("  -movflags +flag        container flags (parsed; muxer options not exposed yet)");
    println!();
    println!("Codec-private options of the form -key value are passed to the encoder.");
    println!();
    println!("Examples:");
    println!("  wedeo-cli -i input.mp4 -c copy -y output.mkv");
    println!("  wedeo-cli -ss 1 -t 2 -vf hflip -i input.mp4 out.mp4");
    println!("  wedeo-cli -i input.mp4 -map 0:a:0 -c:a pcm_s16le out.wav");
}

// ---------------------------------------------------------------------------
// Probe (ffprobe equivalent)
// ---------------------------------------------------------------------------

fn probe_format_name(path: &str) -> Option<String> {
    let mut file = std::fs::File::open(path).ok()?;
    let mut buf = vec![0u8; 4096];
    let n = io::Read::read(&mut file, &mut buf).ok()?;
    buf.truncate(n);
    let data = ProbeData {
        filename: path,
        buf: &buf,
    };
    format_registry::probe(&data).map(|f| f.descriptor().name.to_string())
}

fn format_duration(us: i64) -> String {
    let secs = us as f64 / 1_000_000.0;
    let h = (secs / 3600.0) as u32;
    let m = ((secs % 3600.0) / 60.0) as u32;
    let s = secs % 60.0;
    format!("{h:02}:{m:02}:{s:06.3}")
}

fn cmd_probe(path: &str) -> wedeo_core::Result<()> {
    let ctx = InputContext::open(path)?;
    let format_name = probe_format_name(path).unwrap_or_else(|| "unknown".to_string());

    eprintln!("Input #0, {format_name}, from '{path}':");
    let file_size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let bitrate = if ctx.duration > 0 {
        (file_size as f64 * 8.0 / (ctx.duration as f64 / 1_000_000.0) / 1000.0).round() as i64
    } else {
        0
    };
    eprintln!(
        "  Duration: {}, start: {:.6}, bitrate: {bitrate} kb/s",
        format_duration(ctx.duration),
        ctx.start_time as f64 / 1_000_000.0
    );

    for (key, value) in ctx.metadata.iter() {
        eprintln!("    {key} : {value}");
    }

    for stream in &ctx.streams {
        let cp = &stream.codec_params;
        match cp.media_type {
            MediaType::Video => {
                eprintln!(
                    "    Stream #0:{}: Video: {}, {}, {}x{}, {} kb/s, {} tbr, {} tbn",
                    stream.index,
                    cp.codec_id.name(),
                    cp.pixel_format.name(),
                    cp.width,
                    cp.height,
                    cp.bit_rate / 1000,
                    frame_rate_from_time_base(stream.time_base),
                    stream.time_base,
                );
            }
            MediaType::Audio => {
                eprintln!(
                    "    Stream #0:{}: Audio: {}, {} Hz, {} ch, {}, {} kb/s, {} tbn",
                    stream.index,
                    cp.codec_id.name(),
                    cp.sample_rate,
                    cp.channel_layout.nb_channels,
                    cp.sample_format.name(),
                    cp.bit_rate / 1000,
                    stream.time_base,
                );
            }
            other => {
                eprintln!(
                    "    Stream #0:{}: {}: {}, {} tbn",
                    stream.index,
                    other,
                    cp.codec_id.name(),
                    stream.time_base,
                );
            }
        }
    }

    Ok(())
}

fn frame_rate_from_time_base(tb: Rational) -> String {
    if tb.num == 0 || tb.den == 0 {
        "0".to_string()
    } else {
        format!("{}", tb.invert().to_f64())
    }
}

// ---------------------------------------------------------------------------
// Transcode / remux
// ---------------------------------------------------------------------------

/// Microsecond window `[start, end)` used to skip packets outside `-ss`/`-t`.
struct TimeWindow {
    start_us: i64,
    end_us: Option<i64>,
}

fn nth_stream(input: &InputContext, media_type: MediaType, n: u32) -> Option<usize> {
    input
        .streams
        .iter()
        .filter(|s| s.codec_params.media_type == media_type)
        .nth(n as usize)
        .map(|s| s.index)
}

/// One input file selected for the output, with its chosen streams.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputSelection {
    /// Index into `CliOptions::inputs` and `cmd_transcode`'s open inputs.
    pub file: usize,
    pub video: Option<usize>,
    pub audio: Option<usize>,
}

/// Resolve `-map` targets to per-file stream selections, or apply the default
/// first-video/first-audio streams when no `-map` was given (FFmpeg with no
/// mappings, fftools/ffmpeg_opt.c: opt_map()).
fn select_streams(
    opts: &CliOptions,
    inputs: &[InputContext],
) -> wedeo_core::Result<Vec<InputSelection>> {
    let mut selections = Vec::new();
    for (file, input) in inputs.iter().enumerate() {
        let mut video = None;
        let mut audio = None;

        let file_maps: Vec<&MapTarget> = opts
            .maps
            .iter()
            .filter(|m| m.file() == Some(file))
            .collect();

        if !file_maps.is_empty() {
            for target in file_maps {
                match target {
                    MapTarget::Video { index, .. } => {
                        if opts.video_disabled {
                            return Err(Error::Other(
                                "video stream mapped with -map but disabled with -vn".into(),
                            ));
                        }
                        let idx = nth_stream(input, MediaType::Video, *index).ok_or_else(|| {
                            Error::Other(format!(
                                "Stream specifier '{file}:v:{index}' matches no streams"
                            ))
                        })?;
                        if video.is_some() && video != Some(idx) {
                            return Err(Error::Other("Only one video stream can be mapped".into()));
                        }
                        video = Some(idx);
                    }
                    MapTarget::Audio { index, .. } => {
                        if opts.audio_disabled {
                            return Err(Error::Other(
                                "audio stream mapped with -map but disabled with -an".into(),
                            ));
                        }
                        let idx = nth_stream(input, MediaType::Audio, *index).ok_or_else(|| {
                            Error::Other(format!(
                                "Stream specifier '{file}:a:{index}' matches no streams"
                            ))
                        })?;
                        if audio.is_some() && audio != Some(idx) {
                            return Err(Error::Other("Only one audio stream can be mapped".into()));
                        }
                        audio = Some(idx);
                    }
                    MapTarget::All { .. } => {
                        if !opts.video_disabled && video.is_none() {
                            video = nth_stream(input, MediaType::Video, 0);
                        }
                        if !opts.audio_disabled && audio.is_none() {
                            audio = nth_stream(input, MediaType::Audio, 0);
                        }
                    }
                    // Labelled `-map [label]` entries have no input stream;
                    // they resolve against `-filter_complex` outputs.
                    MapTarget::Label(_) => {}
                }
            }
        } else if opts.maps.is_empty() {
            // No -map anywhere: default to the first video/audio stream of
            // every input, like FFmpeg with default mappings.
            for stream in &input.streams {
                match stream.codec_params.media_type {
                    MediaType::Video if !opts.video_disabled && video.is_none() => {
                        video = Some(stream.index);
                    }
                    MediaType::Audio if !opts.audio_disabled && audio.is_none() => {
                        audio = Some(stream.index);
                    }
                    _ => {}
                }
            }
        }
        // When -map was given but nothing targets this file, no streams are
        // selected from it, matching FFmpeg.

        if video.is_some() || audio.is_some() {
            selections.push(InputSelection { file, video, audio });
        }
    }

    if selections.is_empty() {
        return Err(Error::Other("No streams selected for output".into()));
    }
    Ok(selections)
}

fn stream_duration_us(stream: &Stream, fallback_us: i64) -> i64 {
    if stream.time_base.num != 0 && stream.time_base.den != 0 && stream.duration > 0 {
        rescale_q(
            stream.duration,
            stream.time_base,
            Rational::new(1, 1_000_000),
        )
    } else {
        fallback_us
    }
}

fn time_window(
    opts: &CliOptions,
    input: &InputContext,
    video: Option<usize>,
    audio: Option<usize>,
) -> Option<TimeWindow> {
    let start_us = opts.seek_us.unwrap_or(0).max(0);
    let mut end_us = opts.duration_us.map(|d| start_us.saturating_add(d.max(0)));

    if opts.shortest {
        let mut shortest = None;
        for idx in [video, audio].into_iter().flatten() {
            if let Some(stream) = input.streams.get(idx) {
                let d = stream_duration_us(stream, input.duration);
                if d > 0 {
                    shortest = Some(shortest.map_or(d, |prev: i64| prev.min(d)));
                }
            }
        }
        if let Some(d) = shortest {
            let candidate = start_us.saturating_add(d);
            end_us = Some(end_us.map_or(candidate, |e| e.min(candidate)));
        }
    }

    if start_us == 0 && end_us.is_none() {
        None
    } else {
        Some(TimeWindow { start_us, end_us })
    }
}

fn packet_time_us(pkt: &Packet, stream: &Stream) -> Option<i64> {
    // Demux in decode order: DTS is monotonic, and for stream copy FFmpeg
    // also bounds the output window by DTS when discarding packets. Using PTS
    // here would let reordered B-frames with PTS past -t extend the window.
    let ts = if pkt.dts != NOPTS_VALUE {
        pkt.dts
    } else if pkt.pts != NOPTS_VALUE {
        pkt.pts
    } else {
        return None;
    };
    if stream.time_base.num == 0 || stream.time_base.den == 0 {
        return None;
    }
    Some(rescale_q(ts, stream.time_base, Rational::new(1, 1_000_000)))
}

/// Build a linear graph: buffer -> filter -> ... -> buffersink.
fn build_graph(
    specs: &[FilterSpec],
    time_base: Rational,
    video: bool,
) -> wedeo_core::Result<Option<Graph>> {
    if specs.is_empty() {
        return Ok(None);
    }
    let mut graph = Graph::new();
    let src_name = if video { "buffer" } else { "abuffer" };
    let sink_name = if video { "buffersink" } else { "abuffersink" };
    let media_type = if video {
        MediaType::Video
    } else {
        MediaType::Audio
    };
    let config = FilterLinkConfig {
        media_type,
        time_base,
    };
    let mut prev = graph.add_node_by_name("src", src_name)?;
    for (n, spec) in specs.iter().enumerate() {
        let options: Vec<(&str, &str)> = spec
            .args
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        let node = graph.add_node_with_options(format!("f{n}"), &spec.name, &options)?;
        graph.link(prev, 0, node, 0, config.clone())?;
        prev = node;
    }
    let sink = graph.add_node_by_name("sink", sink_name)?;
    graph.link(prev, 0, sink, 0, config)?;
    Ok(Some(graph))
}

fn apply_metadata(stream: &mut Stream, metadata: &[(String, String)]) {
    for (key, value) in metadata {
        stream.metadata.set(key, value);
    }
}

fn print_stats(enabled: bool, frames: u64, packets: u64, started: Instant, time_us: i64) {
    if !enabled {
        return;
    }
    let elapsed = started.elapsed().as_secs_f64().max(0.001);
    eprintln!(
        "frame={frames:>6} packets={packets:>6} fps={:>5.1} time={:08.2} bitrate=   N/A",
        frames as f64 / elapsed,
        time_us as f64 / 1_000_000.0
    );
}

fn cmd_transcode(opts: &CliOptions) -> wedeo_core::Result<()> {
    let output_path = opts.output.as_deref().unwrap();

    // Check overwrite
    if !opts.overwrite && Path::new(output_path).exists() {
        return Err(Error::Other(format!(
            "File '{output_path}' already exists.{}",
            if opts.no_overwrite {
                " -n was specified, refusing to overwrite"
            } else {
                " Use -y to overwrite."
            }
        )));
    }

    // Open all inputs.
    let mut inputs: Vec<InputContext> = Vec::with_capacity(opts.inputs.len());
    for path in &opts.inputs {
        inputs.push(InputContext::open(path)?);
    }

    // Determine output format
    let format_name = if let Some(ref f) = opts.format {
        f.clone()
    } else {
        guess_format_from_path(output_path).ok_or_else(|| {
            Error::Other(format!(
                "Cannot determine format for '{output_path}'. Use -f."
            ))
        })?
    };

    let selections = select_streams(opts, &inputs)?;

    // Video filters: -r behaves like an implicit fps filter on transcode.
    let mut video_filter_specs = opts.video_filters.clone();
    if !opts.copy_video {
        if let Some(rate) = &opts.frame_rate {
            let parsed = parse_frame_rate(rate)
                .map_err(|e| Error::Other(format!("Invalid frame rate for -r: {e}")))?;
            video_filter_specs.push(FilterSpec {
                name: "fps".to_string(),
                args: vec![("fps".to_string(), format!("{}/{}", parsed.num, parsed.den))],
            });
        }
    } else {
        if opts.frame_rate.is_some() {
            eprintln!("Warning: -r is ignored with stream copy (-c:v copy)");
        }
        if !video_filter_specs.is_empty() {
            return Err(Error::Other(
                "-vf cannot be used with stream copy (-c:v copy)".into(),
            ));
        }
    }
    if opts.copy_audio && !opts.audio_filters.is_empty() {
        return Err(Error::Other(
            "-af cannot be used with stream copy (-c:a copy)".into(),
        ));
    }
    if opts.subtitle_codec.is_some() && !opts.subtitle_disabled {
        eprintln!(
            "Warning: subtitle codec selection is parsed but subtitles are not supported yet"
        );
    }
    if !opts.movflags.is_empty() {
        eprintln!(
            "Warning: -movflags {:?} is parsed but muxer options are not exposed by the framework yet",
            opts.movflags
        );
    }

    // Build the muxer stream list and the per-input pipelines.
    let mut output_streams: Vec<Stream> = Vec::new();
    let mut pipelines: Vec<SelectionPipeline> = Vec::with_capacity(selections.len());
    for sel in &selections {
        pipelines.push(build_pipeline(
            opts,
            &format_name,
            &video_filter_specs,
            &inputs[sel.file],
            sel,
            &mut output_streams,
        )?);
    }

    // Open output
    let mut output = OutputContext::create(output_path, &format_name, &output_streams)?;

    // Demux -> decode -> (filter) -> encode -> mux, one input at a time.
    let mut total_video_frames = 0u64;
    for pipeline in &mut pipelines {
        total_video_frames +=
            run_pipeline(opts, &mut inputs[pipeline.file], pipeline, &mut output)?;
    }

    output.finish()?;

    eprintln!("Output: {output_path}");
    if pipelines.iter().any(|p| p.video_stream_idx.is_some()) {
        eprintln!("  Video: {total_video_frames} frames");
    }

    Ok(())
}

/// Everything needed to process one selected input file: decoders/encoders
/// for its selected streams plus the graphs for `-vf` / `-af`.
struct SelectionPipeline {
    file: usize,
    video_stream_idx: Option<usize>,
    audio_stream_idx: Option<usize>,
    video_decoder: Option<Box<dyn Decoder>>,
    video_encoder: Option<Box<dyn Encoder>>,
    audio_decoder: Option<Box<dyn Decoder>>,
    audio_encoder: Option<Box<dyn Encoder>>,
    video_graph: Option<Graph>,
    audio_graph: Option<Graph>,
}

/// Create decoders/encoders for one selection's streams and append their
/// output streams to `output_streams`.
fn build_pipeline(
    opts: &CliOptions,
    format_name: &str,
    video_filter_specs: &[FilterSpec],
    input: &InputContext,
    sel: &InputSelection,
    output_streams: &mut Vec<Stream>,
) -> wedeo_core::Result<SelectionPipeline> {
    let video_stream_idx = sel.video;
    let audio_stream_idx = sel.audio;
    let mut video_decoder = None;
    let mut video_encoder = None;
    let mut audio_decoder = None;
    let mut audio_encoder = None;

    // --- Video ---
    if let Some(idx) = video_stream_idx {
        let in_stream = &input.streams[idx];
        let in_params = &in_stream.codec_params;

        if opts.copy_video {
            let mut out_stream = Stream::new(output_streams.len(), in_params.clone());
            out_stream.time_base = in_stream.time_base;
            out_stream.duration = in_stream.duration;
            apply_metadata(&mut out_stream, &opts.metadata);
            output_streams.push(out_stream);
        } else {
            let dec = DecoderBuilder::new(in_params.clone()).open()?;
            video_decoder = Some(dec);

            let out_codec_id = if let Some(ref name) = opts.video_codec {
                CodecId::from_name(name)
                    .ok_or_else(|| Error::Other(format!("Unknown video codec: '{name}'")))?
            } else {
                let muxer_factory =
                    format_registry::find_muxer_by_name(format_name).ok_or(Error::MuxerNotFound)?;
                let default_codec = muxer_factory.descriptor().video_codec;
                if default_codec == CodecId::None {
                    return Err(Error::Other(format!(
                        "Format '{format_name}' has no default video codec. Use -c:v."
                    )));
                }
                default_codec
            };

            let mut enc_builder = EncoderBuilder::new(out_codec_id, MediaType::Video);
            enc_builder.width = in_params.width;
            enc_builder.height = in_params.height;
            enc_builder.pixel_format = in_params.pixel_format;
            enc_builder.time_base = in_stream.time_base;
            // Honor `EncoderBuilder::with_thread_count`; `-threads` also keeps
            // its codec-private entry, which the builder reads as a fallback.
            if let Some(count) = opts.thread_count {
                enc_builder = enc_builder.with_thread_count(count);
            }

            if let Some(br) = opts.bitrate_video {
                enc_builder.bit_rate = br;
            }

            // Check if muxer needs GLOBAL_HEADER
            if let Some(muxer_factory) = format_registry::find_muxer_by_name(format_name) {
                let mflags = muxer_factory.descriptor().flags;
                if mflags.contains(wedeo_format::muxer::OutputFormatFlags::GLOBALHEADER) {
                    enc_builder.flags |= CodecFlags::GLOBAL_HEADER;
                }
            }

            // Apply codec-private options
            for (key, value) in &opts.codec_options {
                enc_builder.options.set(key, value);
            }

            let enc = enc_builder.open()?;
            video_encoder = Some(enc);

            let mut out_params =
                wedeo_codec::decoder::CodecParameters::new(out_codec_id, MediaType::Video);
            out_params.width = in_params.width;
            out_params.height = in_params.height;
            out_params.pixel_format = in_params.pixel_format;
            let mut out_stream = Stream::new(output_streams.len(), out_params);
            out_stream.time_base = in_stream.time_base;
            out_stream.duration = in_stream.duration;
            apply_metadata(&mut out_stream, &opts.metadata);
            output_streams.push(out_stream);
        }
    }

    // --- Audio ---
    if let Some(idx) = audio_stream_idx {
        let in_stream = &input.streams[idx];
        let in_params = &in_stream.codec_params;

        if opts.copy_audio {
            let mut out_stream = Stream::new(output_streams.len(), in_params.clone());
            out_stream.time_base = in_stream.time_base;
            out_stream.duration = in_stream.duration;
            apply_metadata(&mut out_stream, &opts.metadata);
            output_streams.push(out_stream);
        } else {
            let dec = DecoderBuilder::new(in_params.clone()).open()?;
            audio_decoder = Some(dec);

            let out_codec_id = if let Some(ref name) = opts.audio_codec {
                CodecId::from_name(name)
                    .ok_or_else(|| Error::Other(format!("Unknown audio codec: '{name}'")))?
            } else {
                let muxer_factory =
                    format_registry::find_muxer_by_name(format_name).ok_or(Error::MuxerNotFound)?;
                let default_codec = muxer_factory.descriptor().audio_codec;
                if default_codec == CodecId::None {
                    return Err(Error::Other(format!(
                        "Format '{format_name}' has no default audio codec. Use -c:a."
                    )));
                }
                default_codec
            };

            let mut enc_builder = EncoderBuilder::new(out_codec_id, MediaType::Audio);
            enc_builder.sample_rate = in_params.sample_rate;
            enc_builder.sample_format = in_params.sample_format;
            enc_builder.channel_layout = in_params.channel_layout.clone();
            enc_builder.time_base = in_stream.time_base;
            if let Some(count) = opts.thread_count {
                enc_builder = enc_builder.with_thread_count(count);
            }

            if let Some(br) = opts.bitrate_audio {
                enc_builder.bit_rate = br;
            }

            let enc = enc_builder.open()?;
            audio_encoder = Some(enc);

            let mut out_params =
                wedeo_codec::decoder::CodecParameters::new(out_codec_id, MediaType::Audio);
            out_params.sample_rate = in_params.sample_rate;
            out_params.sample_format = in_params.sample_format;
            out_params.channel_layout = in_params.channel_layout.clone();
            let mut out_stream = Stream::new(output_streams.len(), out_params);
            out_stream.time_base = in_stream.time_base;
            out_stream.duration = in_stream.duration;
            apply_metadata(&mut out_stream, &opts.metadata);
            output_streams.push(out_stream);
        }
    }

    let video_graph = match video_stream_idx {
        Some(idx) if !opts.copy_video => {
            build_graph(video_filter_specs, input.streams[idx].time_base, true)?
        }
        _ => None,
    };
    let audio_graph = match audio_stream_idx {
        Some(idx) if !opts.copy_audio && !opts.audio_filters.is_empty() => {
            build_graph(&opts.audio_filters, input.streams[idx].time_base, false)?
        }
        _ => None,
    };

    Ok(SelectionPipeline {
        file: sel.file,
        video_stream_idx,
        audio_stream_idx,
        video_decoder,
        video_encoder,
        audio_decoder,
        audio_encoder,
        video_graph,
        audio_graph,
    })
}

/// Demux -> decode -> (filter) -> encode -> mux for one selected input,
/// draining filters, decoders and encoders at EOF. Returns the video frame
/// count for the final `Output:` summary.
fn run_pipeline(
    opts: &CliOptions,
    input: &mut InputContext,
    pipeline: &mut SelectionPipeline,
    output: &mut OutputContext,
) -> wedeo_core::Result<u64> {
    let video_stream_idx = pipeline.video_stream_idx;
    let audio_stream_idx = pipeline.audio_stream_idx;
    let window = time_window(opts, input, video_stream_idx, audio_stream_idx);
    let mut video_frames_out = 0u64;
    let mut packets_out = 0u64;
    let mut last_time_us = 0i64;
    let frame_limit = opts.frames_video.unwrap_or(u64::MAX);
    let started = Instant::now();

    // Demux -> decode -> (filter) -> encode -> mux
    loop {
        let packet = match input.read_packet() {
            Ok(pkt) => pkt,
            Err(Error::Eof) => break,
            Err(e) => return Err(e),
        };

        let stream_idx = packet.stream_index;
        let selected = Some(stream_idx) == video_stream_idx || Some(stream_idx) == audio_stream_idx;
        if !selected {
            continue;
        }

        // -ss / -t / -shortest: skip packets outside the requested window.
        if let Some(ref w) = window
            && let Some(us) = packet_time_us(&packet, &input.streams[stream_idx])
        {
            if us < w.start_us {
                continue;
            }
            if let Some(end) = w.end_us
                && us >= end
            {
                break;
            }
            last_time_us = us;
        }

        // Video stream
        if Some(stream_idx) == video_stream_idx {
            if video_frames_out >= frame_limit {
                continue;
            }

            if opts.copy_video {
                output.write_packet(&packet)?;
                video_frames_out += 1;
                packets_out += 1;
            } else if let (Some(dec), Some(enc)) =
                (&mut pipeline.video_decoder, &mut pipeline.video_encoder)
            {
                dec.send_packet(Some(&packet))?;
                loop {
                    match dec.receive_frame() {
                        Ok(frame) => {
                            let frames = filter_frames(pipeline.video_graph.as_mut(), frame)?;
                            for frame in frames {
                                if video_frames_out >= frame_limit {
                                    break;
                                }
                                enc.send_frame(Some(&frame))?;
                                drain_encoder_to_output(&mut **enc, output)?;
                                video_frames_out += 1;
                                packets_out += 1;
                            }
                        }
                        Err(Error::Again) => break,
                        Err(e) => return Err(e),
                    }
                }
            }
            if packets_out.is_multiple_of(50) {
                print_stats(
                    opts.stats,
                    video_frames_out,
                    packets_out,
                    started,
                    last_time_us,
                );
            }
            continue;
        }

        // Audio stream
        if Some(stream_idx) == audio_stream_idx {
            if opts.copy_audio {
                output.write_packet(&packet)?;
                packets_out += 1;
            } else if let (Some(dec), Some(enc)) =
                (&mut pipeline.audio_decoder, &mut pipeline.audio_encoder)
            {
                dec.send_packet(Some(&packet))?;
                loop {
                    match dec.receive_frame() {
                        Ok(frame) => {
                            let frames = filter_frames(pipeline.audio_graph.as_mut(), frame)?;
                            for frame in frames {
                                enc.send_frame(Some(&frame))?;
                                drain_encoder_to_output(&mut **enc, output)?;
                                packets_out += 1;
                            }
                        }
                        Err(Error::Again) => break,
                        Err(e) => return Err(e),
                    }
                }
            }
            if packets_out.is_multiple_of(50) {
                print_stats(
                    opts.stats,
                    video_frames_out,
                    packets_out,
                    started,
                    last_time_us,
                );
            }
        }
    }

    // Drain filters, decoders and encoders.
    if let Some(graph) = pipeline.video_graph.as_mut() {
        graph.close()?;
        for frame in graph.drain() {
            if video_frames_out < frame_limit
                && let Some(enc) = pipeline.video_encoder.as_mut()
            {
                enc.send_frame(Some(&frame))?;
                drain_encoder_to_output(&mut **enc, output)?;
                video_frames_out += 1;
                packets_out += 1;
            }
        }
    }
    if let (Some(dec), Some(enc)) = (&mut pipeline.video_decoder, &mut pipeline.video_encoder) {
        dec.send_packet(None)?;
        loop {
            match dec.receive_frame() {
                Ok(frame) => {
                    let frames = filter_frames(pipeline.video_graph.as_mut(), frame)?;
                    for frame in frames {
                        if video_frames_out < frame_limit {
                            enc.send_frame(Some(&frame))?;
                            drain_encoder_to_output(&mut **enc, output)?;
                            video_frames_out += 1;
                            packets_out += 1;
                        }
                    }
                }
                Err(Error::Eof | Error::Again) => break,
                Err(e) => return Err(e),
            }
        }
        enc.send_frame(None)?;
        drain_encoder_to_output(&mut **enc, output)?;
    }

    if let Some(graph) = pipeline.audio_graph.as_mut() {
        graph.close()?;
        for frame in graph.drain() {
            if let Some(enc) = pipeline.audio_encoder.as_mut() {
                enc.send_frame(Some(&frame))?;
                drain_encoder_to_output(&mut **enc, output)?;
                packets_out += 1;
            }
        }
    }
    if let (Some(dec), Some(enc)) = (&mut pipeline.audio_decoder, &mut pipeline.audio_encoder) {
        dec.send_packet(None)?;
        loop {
            match dec.receive_frame() {
                Ok(frame) => {
                    let frames = filter_frames(pipeline.audio_graph.as_mut(), frame)?;
                    for frame in frames {
                        enc.send_frame(Some(&frame))?;
                        drain_encoder_to_output(&mut **enc, output)?;
                        packets_out += 1;
                    }
                }
                Err(Error::Eof | Error::Again) => break,
                Err(e) => return Err(e),
            }
        }
        enc.send_frame(None)?;
        drain_encoder_to_output(&mut **enc, output)?;
    }

    print_stats(
        opts.stats,
        video_frames_out,
        packets_out,
        started,
        last_time_us,
    );
    Ok(video_frames_out)
}

fn filter_frames(graph: Option<&mut Graph>, frame: Frame) -> wedeo_core::Result<Vec<Frame>> {
    match graph {
        Some(graph) => graph.process(frame),
        None => Ok(vec![frame]),
    }
}

fn drain_encoder_to_output(
    enc: &mut dyn wedeo_codec::encoder::Encoder,
    output: &mut OutputContext,
) -> wedeo_core::Result<()> {
    loop {
        match enc.receive_packet() {
            Ok(pkt) => output.write_packet(&pkt)?,
            Err(Error::Again | Error::Eof) => break,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// List commands
// ---------------------------------------------------------------------------

fn cmd_codecs() {
    println!("Codecs:");
    println!(" D..... = Decoding supported");
    println!(" .E.... = Encoding supported");
    println!(" ..V... = Video codec");
    println!(" ..A... = Audio codec");
    println!(" ..S... = Subtitle codec");
    println!(" ------");

    // Collect all known codec IDs from decoders and encoders
    let mut seen = std::collections::BTreeMap::<&str, (bool, bool, &str)>::new();

    for factory in codec_registry::decoders() {
        let desc = factory.descriptor();
        let entry = seen.entry(desc.name).or_insert((false, false, ""));
        entry.0 = true;
        if entry.2.is_empty() {
            entry.2 = desc.long_name;
        }
    }
    for factory in codec_registry::encoders() {
        let desc = factory.descriptor();
        let entry = seen.entry(desc.name).or_insert((false, false, ""));
        entry.1 = true;
        if entry.2.is_empty() {
            entry.2 = desc.long_name;
        }
    }

    for (name, (has_dec, has_enc, long_name)) in &seen {
        let d = if *has_dec { 'D' } else { '.' };
        let e = if *has_enc { 'E' } else { '.' };
        let t = match CodecId::from_name(name).map(media_type_for_codec) {
            Some(MediaType::Video) => 'V',
            Some(MediaType::Audio) => 'A',
            Some(MediaType::Subtitle) => 'S',
            _ => '.',
        };
        println!(" {d}{e}{t}... {name:<20} {long_name}");
    }
}

fn cmd_decoders() {
    println!("Decoders:");
    for factory in codec_registry::decoders() {
        let desc = factory.descriptor();
        println!(
            "  {:<20} {} ({})",
            desc.name, desc.long_name, desc.media_type
        );
    }
}

fn cmd_encoders() {
    println!("Encoders:");
    for factory in codec_registry::encoders() {
        let desc = factory.descriptor();
        println!(
            "  {:<20} {} ({})",
            desc.name, desc.long_name, desc.media_type
        );
    }
}

fn cmd_formats() {
    println!("Formats:");
    println!(" D. = Demuxing supported");
    println!(" .M = Muxing supported");
    println!(" --");

    let mut seen = std::collections::BTreeMap::<&str, (bool, bool, &str)>::new();

    for factory in format_registry::demuxers() {
        let desc = factory.descriptor();
        let entry = seen.entry(desc.name).or_insert((false, false, ""));
        entry.0 = true;
        if entry.2.is_empty() {
            entry.2 = desc.long_name;
        }
    }
    for factory in format_registry::muxers() {
        let desc = factory.descriptor();
        let entry = seen.entry(desc.name).or_insert((false, false, ""));
        entry.1 = true;
        if entry.2.is_empty() {
            entry.2 = desc.long_name;
        }
    }

    for (name, (has_demux, has_mux, long_name)) in &seen {
        let d = if *has_demux { 'D' } else { '.' };
        let m = if *has_mux { 'M' } else { '.' };
        println!(" {d}{m} {name:<20} {long_name}");
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn guess_format_from_path(path: &str) -> Option<String> {
    let ext = Path::new(path).extension()?.to_str()?;
    let factory = format_registry::guess_muxer_by_extension(ext)?;
    Some(factory.descriptor().name.to_string())
}

fn parse_bitrate(s: &str) -> i64 {
    let s = s.trim();
    if let Some(prefix) = s.strip_suffix('k').or_else(|| s.strip_suffix('K')) {
        prefix.parse::<i64>().unwrap_or(0) * 1000
    } else if let Some(prefix) = s.strip_suffix('M') {
        prefix.parse::<i64>().unwrap_or(0) * 1_000_000
    } else {
        s.parse::<i64>().unwrap_or(0)
    }
}

fn media_type_for_codec(id: CodecId) -> MediaType {
    match id {
        CodecId::None => MediaType::Data,
        CodecId::Mpeg1video
        | CodecId::Mpeg2video
        | CodecId::H261
        | CodecId::H263
        | CodecId::Rv10
        | CodecId::Rv20
        | CodecId::Mjpeg
        | CodecId::Mpeg4
        | CodecId::Rawvideo
        | CodecId::H264
        | CodecId::Vp8
        | CodecId::Vp9
        | CodecId::Hevc
        | CodecId::Av1 => MediaType::Video,
        CodecId::SubDvdSubtitle
        | CodecId::SubDvbSubtitle
        | CodecId::SubText
        | CodecId::SubXsub
        | CodecId::SubSsa
        | CodecId::SubMovText
        | CodecId::SubSrt
        | CodecId::SubWebvtt => MediaType::Subtitle,
        _ => MediaType::Audio,
    }
}

fn die(msg: &str) -> ! {
    eprintln!("Error: {msg}");
    process::exit(1);
}
