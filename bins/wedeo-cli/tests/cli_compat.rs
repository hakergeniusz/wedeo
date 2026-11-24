//! Parser compatibility tests for the FFmpeg-style wedeo-cli.
//!
//! `wedeo-cli` is a binary-only crate, so `main.rs` is pulled in as a module to
//! exercise `parse_args_from` and the shared value parsers. No media files are
//! used here; the bit-exact remux check is performed manually against FFmpeg.

// main.rs is compiled as a module here; command handlers are driven by the
// binary itself, not by these parser-only tests.
#[allow(dead_code)]
#[path = "../src/main.rs"]
mod cli;

use cli::{
    CliOptions, FilterSpec, MapTarget, OptionScope, parse_args_from, parse_filter_chain,
    parse_frame_rate, parse_map_spec, parse_time,
};

fn args(tokens: &[&str]) -> Vec<String> {
    tokens.iter().map(|s| (*s).to_string()).collect()
}

fn ok(tokens: &[&str]) -> CliOptions {
    let opts = parse_args_from(&args(tokens));
    assert!(
        opts.parse_error.is_none(),
        "unexpected parse error: {:?}",
        opts.parse_error
    );
    opts
}

fn err(tokens: &[&str]) -> String {
    let opts = parse_args_from(&args(tokens));
    opts.parse_error
        .unwrap_or_else(|| panic!("expected a parse error for {tokens:?}"))
}

#[test]
fn codec_aliases_and_bitrate_aliases() {
    let opts = ok(&[
        "-vcodec",
        "av1",
        "-acodec",
        "pcm_s16le",
        "-scodec",
        "srt",
        "-vb",
        "2M",
        "-ab",
        "128k",
        "-i",
        "in.mp4",
        "out.mkv",
    ]);
    assert_eq!(opts.video_codec.as_deref(), Some("av1"));
    assert_eq!(opts.audio_codec.as_deref(), Some("pcm_s16le"));
    assert_eq!(opts.subtitle_codec.as_deref(), Some("srt"));
    assert_eq!(opts.bitrate_video, Some(2_000_000));
    assert_eq!(opts.bitrate_audio, Some(128_000));
    assert_eq!(opts.output.as_deref(), Some("out.mkv"));
}

#[test]
fn c_copy_alias_preserves_verified_remux_options() {
    let opts = ok(&["-i", "in.mp4", "-c", "copy", "-y", "out.mkv"]);
    assert!(opts.copy_video);
    assert!(opts.copy_audio);
    assert!(opts.overwrite);
    assert_eq!(opts.inputs, vec!["in.mp4"]);
    assert_eq!(opts.output.as_deref(), Some("out.mkv"));
}

#[test]
fn stream_specific_codec_aliases() {
    let opts = ok(&[
        "-i",
        "in.mp4",
        "-c:v",
        "copy",
        "-c:a",
        "pcm_s16le",
        "-c:s",
        "copy",
        "out.mkv",
    ]);
    assert!(opts.copy_video);
    assert_eq!(opts.audio_codec.as_deref(), Some("pcm_s16le"));
    assert_eq!(opts.subtitle_codec.as_deref(), Some("copy"));
}

#[test]
fn options_before_i_are_input_scoped_and_after_are_output_scoped() {
    let opts = ok(&[
        "-ss", "00:00:02", "-y", "-i", "in.mp4", "-t", "1.5", "-vf", "hflip", "-n", "out.mkv",
    ]);
    assert_eq!(opts.input_option("-ss"), Some("00:00:02"));
    assert_eq!(opts.input_option("-t"), None);
    assert_eq!(opts.output_option("-t"), Some("1.5"));
    assert_eq!(opts.output_option("-vf"), Some("hflip"));
    assert_eq!(opts.seek_us, Some(2_000_000));
    assert_eq!(opts.duration_us, Some(1_500_000));
    assert_eq!(opts.effective_option("-ss"), Some("00:00:02"));

    assert!(
        opts.scoped_options
            .iter()
            .any(|o| { o.key == "-ss" && o.scope == OptionScope::Input(0) })
    );
    assert!(
        opts.scoped_options
            .iter()
            .any(|o| { o.key == "-vf" && o.scope == OptionScope::Output })
    );
    assert_eq!(
        opts.scoped_options
            .iter()
            .filter(|o| o.key == "-y" || o.key == "-n")
            .map(|o| o.scope)
            .collect::<Vec<_>>(),
        vec![OptionScope::Global, OptionScope::Global]
    );
    // The last overwrite flag wins.
    assert!(opts.no_overwrite);
    assert!(!opts.overwrite);
    assert_eq!(
        opts.video_filters,
        vec![FilterSpec {
            name: "hflip".into(),
            args: Vec::new(),
        }]
    );
}

#[test]
fn global_flags_work_in_any_position() {
    for tokens in [
        vec!["-y", "-hide_banner", "-stats", "-i", "in.mp4", "out.mkv"],
        vec!["-i", "in.mp4", "-y", "-hide_banner", "-stats", "out.mkv"],
    ] {
        let opts = ok(&tokens);
        assert!(opts.overwrite);
        assert!(opts.hide_banner);
        assert!(opts.stats);
    }
    let help = ok(&["-i", "in.mp4", "-h"]);
    assert!(help.help);
    let banner = ok(&["-hide_banner", "-codecs"]);
    assert!(banner.hide_banner && banner.list_codecs);
}

#[test]
fn map_specs_supported_and_rejected() {
    assert_eq!(parse_map_spec("0"), Ok(MapTarget::All));
    assert_eq!(parse_map_spec("0:v"), Ok(MapTarget::Video(0)));
    assert_eq!(parse_map_spec("0:v:0"), Ok(MapTarget::Video(0)));
    assert_eq!(parse_map_spec("0:v:2"), Ok(MapTarget::Video(2)));
    assert_eq!(parse_map_spec("0:a"), Ok(MapTarget::Audio(0)));
    assert_eq!(parse_map_spec("0:a:1"), Ok(MapTarget::Audio(1)));

    for bad in ["1:v", "0:s", "-0:v", "0:v:x", "0:v:1:2", "abc", "0:t:0"] {
        let e = parse_map_spec(bad).unwrap_err();
        assert!(!e.is_empty(), "empty error for {bad}");
    }
}

#[test]
fn multiple_maps_are_combined_and_conflicts_rejected() {
    let opts = ok(&["-i", "in.mp4", "-map", "0:v:0", "-map", "0:a:1", "out.mkv"]);
    assert_eq!(opts.maps, vec![MapTarget::Video(0), MapTarget::Audio(1)]);

    let msg = err(&["-i", "in.mp4", "-map", "0:v:0", "-map", "0:v:1", "out.mkv"]);
    assert!(msg.contains("Only one video stream"), "{msg}");
    let msg = err(&["-i", "in.mp4", "-map", "0", "-map", "0:a:2", "out.mkv"]);
    assert!(msg.contains("Only one audio stream"), "{msg}");
}

#[test]
fn time_values_are_parsed_like_ffmpeg() {
    assert_eq!(parse_time("12"), Ok(12_000_000));
    assert_eq!(parse_time("12.5"), Ok(12_500_000));
    assert_eq!(parse_time("00:01:02.5"), Ok(62_500_000));
    assert_eq!(parse_time("1:30"), Ok(90_000_000));
    assert_eq!(parse_time("500ms"), Ok(500_000));
    assert_eq!(parse_time("2m"), Ok(120_000_000));
    assert_eq!(parse_time("1h"), Ok(3_600_000_000));
    assert_eq!(parse_time("-1.5"), Ok(-1_500_000));
    for bad in ["abc", "", "1:2:3:4", "1x", "1.2.3s"] {
        assert!(parse_time(bad).is_err(), "accepted bad time {bad:?}");
    }
}

#[test]
fn frame_rate_values_are_parsed() {
    assert_eq!(parse_frame_rate("30").unwrap().to_string(), "30/1");
    assert_eq!(
        parse_frame_rate("30000/1001").unwrap().to_string(),
        "30000/1001"
    );
    let fps = parse_frame_rate("29.97").unwrap();
    assert!(fps.to_f64() > 29.96 && fps.to_f64() < 29.98);
    for bad in ["0", "-1", "abc", "1/0"] {
        assert!(parse_frame_rate(bad).is_err(), "accepted bad rate {bad:?}");
    }
    let opts = ok(&["-i", "in.mp4", "-r", "30000/1001", "out.mp4"]);
    assert_eq!(opts.frame_rate.as_deref(), Some("30000/1001"));
    assert!(err(&["-i", "in.mp4", "-r", "abc", "out.mp4"]).contains("-r"));
}

#[test]
fn video_filter_chains_are_parsed_into_named_options() {
    assert_eq!(
        parse_filter_chain("hflip,vflip", true).unwrap(),
        vec![
            FilterSpec {
                name: "hflip".into(),
                args: Vec::new()
            },
            FilterSpec {
                name: "vflip".into(),
                args: Vec::new()
            },
        ]
    );
    assert_eq!(
        parse_filter_chain("crop=320:240:10:10", true).unwrap(),
        vec![FilterSpec {
            name: "crop".into(),
            args: vec![
                ("w".into(), "320".into()),
                ("h".into(), "240".into()),
                ("x".into(), "10".into()),
                ("y".into(), "10".into()),
            ],
        }]
    );
    assert_eq!(
        parse_filter_chain("transpose=dir=1", true).unwrap(),
        vec![FilterSpec {
            name: "transpose".into(),
            args: vec![("dir".into(), "1".into())],
        }]
    );
    assert_eq!(
        parse_filter_chain("pad=640:480:0:0:black", true).unwrap(),
        vec![FilterSpec {
            name: "pad".into(),
            args: vec![
                ("w".into(), "640".into()),
                ("h".into(), "480".into()),
                ("x".into(), "0".into()),
                ("y".into(), "0".into()),
                ("color".into(), "black".into()),
            ],
        }]
    );
    assert_eq!(
        parse_filter_chain("fps=30", true).unwrap(),
        vec![FilterSpec {
            name: "fps".into(),
            args: vec![("fps".into(), "30".into())],
        }]
    );
    assert_eq!(
        parse_filter_chain("crop=w=100:h=80", true).unwrap(),
        vec![FilterSpec {
            name: "crop".into(),
            args: vec![("w".into(), "100".into()), ("h".into(), "80".into())],
        }]
    );
    assert_eq!(parse_filter_chain("anull,copy", false).unwrap().len(), 2);
}

#[test]
fn unsupported_filter_chains_fail_clearly() {
    for (chain, video) in [
        ("bogus", true),
        ("crop=1:2:3:4:5", true),
        ("transpose=dir=banana", true),
        ("fps=abc", true),
        ("hflip=x", true),
        ("crop=w=1:bogus=2", true),
        ("hflip", false),
        ("", true),
        ("hflip,,vflip", true),
    ] {
        let e = parse_filter_chain(chain, video).unwrap_err();
        assert!(!e.is_empty(), "empty error for {chain:?}");
    }
    let msg = err(&["-i", "in.mp4", "-vf", "bogus", "out.mp4"]);
    assert!(msg.contains("-vf"), "{msg}");
}

#[test]
fn quality_preset_threads_metadata_and_movflags_are_parsed() {
    let opts = ok(&[
        "-i",
        "in.mp4",
        "-q:v",
        "3",
        "-crf",
        "28",
        "-preset",
        "fast",
        "-threads",
        "4",
        "-metadata",
        "title=demo",
        "-movflags",
        "+faststart",
        "out.mp4",
    ]);
    let has = |key: &str, value: &str| {
        opts.codec_options
            .iter()
            .any(|(k, v)| k == key && v == value)
    };
    assert!(has("q:v", "3"));
    assert!(has("crf", "28"));
    assert!(has("preset", "fast"));
    assert!(has("threads", "4"));
    assert_eq!(
        opts.metadata,
        vec![("title".to_string(), "demo".to_string())]
    );
    assert_eq!(opts.movflags, vec!["faststart"]);
    assert_eq!(
        opts.codec_options.iter().filter(|(k, _)| k == "qp").count(),
        2
    );
}

#[test]
fn miscellaneous_flags_are_parsed() {
    let opts = ok(&[
        "-i",
        "in.mp4",
        "-vn",
        "-an",
        "-sn",
        "-shortest",
        "-frames:v",
        "25",
        "-v",
        "info",
        "out.mkv",
    ]);
    assert!(opts.video_disabled);
    assert!(opts.audio_disabled);
    assert!(opts.subtitle_disabled);
    assert!(opts.shortest);
    assert_eq!(opts.frames_video, Some(25));
    assert_eq!(opts.loglevel, "info");
}

#[test]
fn parse_errors_are_reported_instead_of_exiting() {
    let missing_i = err(&["-i"]);
    assert!(missing_i.contains("-i"), "{missing_i}");
    let unknown = err(&["-i", "in.mp4", "-totallyunknown", "out.mkv", "-bogusflag"]);
    assert!(unknown.contains("Unrecognized option"), "{unknown}");
    let metadata = err(&["-i", "in.mp4", "-metadata", "novalue", "out.mkv"]);
    assert!(metadata.contains("key=value"), "{metadata}");
    let frames = err(&["-i", "in.mp4", "-frames:v", "many", "out.mkv"]);
    assert!(frames.contains("frame count"), "{frames}");
    let too_many = err(&["-i", "in.mp4", "out.mkv", "extra.mkv"]);
    assert!(too_many.contains("Unexpected argument"), "{too_many}");
    let threads = err(&["-i", "in.mp4", "-threads", "many", "out.mkv"]);
    assert!(threads.contains("threads"), "{threads}");
}
