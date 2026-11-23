//! Port of FFmpeg's `libavfilter/vf_format.c` (the `format` filter).
//!
//! FFmpeg's `format` filter is `AVFILTER_FLAG_METADATA_ONLY`: it has no
//! `filter_frame()` callback at all. Its entire purpose is the
//! `query_formats()` callback, which restricts the pixel formats / color
//! spaces / color ranges / alpha modes / chroma locations negotiated on both
//! the input and the output link to the lists given as options. Frames are
//! then passed through by the framework, untouched.
//!
//! The frozen Wedeo `Filter` interface has no link-format negotiation hook
//! (`descriptor()` only carries a `MediaType` per pad). The maximal valid
//! behavior implemented here is therefore:
//!
//!   - parse and retain all five option lists exactly like FFmpeg's `init()`;
//!   - pass frames through unchanged, never converting or mislabelling pixel
//!     data (doing so would be wrong without a link negotiation step);
//!   - record the requested lists as frame metadata, the only channel the
//!     frozen `Frame` interface exposes for carrying that information.
//!
//! See the final report for the precise interface gap.

use wedeo_core::ChromaLocation;
use wedeo_core::error::{Error, Result};
use wedeo_core::frame::{ColorRange, ColorSpace, Frame};
use wedeo_core::media_type::MediaType;
use wedeo_core::pixel_format::PixelFormat;

// Re-exported so that `wedeo-fate` (which does not depend on `wedeo-filter`
// directly) can inspect registrations through this crate.
pub use wedeo_filter::registry::{FilterFactory, filters, find_filter_by_name};
pub use wedeo_filter::{
    Filter, FilterDescriptor, FilterFlags, FilterPadDescriptor, FilterPadDirection,
};

/// Metadata key holding the requested `'|'`-separated pixel format names.
pub const METADATA_PIX_FMTS: &str = "wedeo.format.pix_fmts";
/// Metadata key holding the requested `'|'`-separated color space names.
pub const METADATA_COLOR_SPACES: &str = "wedeo.format.color_spaces";
/// Metadata key holding the requested `'|'`-separated color range names.
pub const METADATA_COLOR_RANGES: &str = "wedeo.format.color_ranges";
/// Metadata key holding the requested `'|'`-separated alpha mode names.
pub const METADATA_ALPHA_MODES: &str = "wedeo.format.alpha_modes";
/// Metadata key holding the requested `'|'`-separated chroma location names.
pub const METADATA_CHROMA_LOCATIONS: &str = "wedeo.format.chroma_locations";

// FFmpeg:
// static const AVFilterPad inputs[] = {
//     {
//         .name             = "default",
//         .type             = AVMEDIA_TYPE_VIDEO,
//         .get_buffer.video = ff_null_get_video_buffer,
//     },
// };
static INPUT_PADS: &[FilterPadDescriptor] = &[FilterPadDescriptor {
    name: "default",
    media_type: MediaType::Video,
    direction: FilterPadDirection::Input,
}];

static OUTPUT_PADS: &[FilterPadDescriptor] = &[FilterPadDescriptor {
    name: "default",
    media_type: MediaType::Video,
    direction: FilterPadDirection::Output,
}];

// FFmpeg:
// .p.name        = "format",
// .p.description = NULL_IF_CONFIG_SMALL("Convert the input video to one of the specified pixel formats."),
// .p.flags       = AVFILTER_FLAG_METADATA_ONLY,
pub(crate) static FORMAT_DESCRIPTOR: FilterDescriptor = FilterDescriptor {
    name: "format",
    description: "Convert the input video to one of the specified pixel formats.",
    inputs: INPUT_PADS,
    outputs: OUTPUT_PADS,
    flags: FilterFlags::METADATA_ONLY,
};

/// All pixel formats representable by `wedeo_core::PixelFormat`.
///
/// FFmpeg iterates `av_pix_fmt_descriptors[]` for name lookup and accepts any
/// numeric value with a descriptor; wedEO can only represent the variants in
/// this table, so numeric values outside it are rejected.
const ALL_PIXEL_FORMATS: &[PixelFormat] = &[
    PixelFormat::Yuv420p,
    PixelFormat::Yuyv422,
    PixelFormat::Rgb24,
    PixelFormat::Bgr24,
    PixelFormat::Yuv422p,
    PixelFormat::Yuv444p,
    PixelFormat::Yuv410p,
    PixelFormat::Yuv411p,
    PixelFormat::Gray8,
    PixelFormat::MonoWhite,
    PixelFormat::MonoBlack,
    PixelFormat::Pal8,
    PixelFormat::Yuvj420p,
    PixelFormat::Yuvj422p,
    PixelFormat::Yuvj444p,
    PixelFormat::Uyvy422,
    PixelFormat::Nv12,
    PixelFormat::Nv21,
    PixelFormat::Argb,
    PixelFormat::Rgba,
    PixelFormat::Abgr,
    PixelFormat::Bgra,
    PixelFormat::Gray16be,
    PixelFormat::Gray16le,
    PixelFormat::Yuv420p16le,
    PixelFormat::Yuv420p16be,
    PixelFormat::Rgb48be,
    PixelFormat::Rgb48le,
    PixelFormat::Yuv420p10le,
    PixelFormat::Yuv420p10be,
];

const ALL_COLOR_SPACES: &[ColorSpace] = &[
    ColorSpace::Rgb,
    ColorSpace::Bt709,
    ColorSpace::Unspecified,
    ColorSpace::Fcc,
    ColorSpace::Bt470bg,
    ColorSpace::Smpte170m,
    ColorSpace::Smpte240m,
    ColorSpace::Ycgco,
    ColorSpace::Bt2020Ncl,
    ColorSpace::Bt2020Cl,
];

const ALL_COLOR_RANGES: &[ColorRange] =
    &[ColorRange::Unspecified, ColorRange::Mpeg, ColorRange::Jpeg];

const ALL_CHROMA_LOCATIONS: &[ChromaLocation] = &[
    ChromaLocation::Unspecified,
    ChromaLocation::Left,
    ChromaLocation::Center,
    ChromaLocation::TopLeft,
    ChromaLocation::Top,
    ChromaLocation::BottomLeft,
    ChromaLocation::Bottom,
];

// FFmpeg: av_color_range_from_name() uses color_range_names[] =
// { [UNSPECIFIED] = "unknown", [MPEG] = "tv", [JPEG] = "pc" }.
const COLOR_RANGE_NAMES: &[&str] = &["unknown", "tv", "pc"];

// FFmpeg: static const char * const color_space_names[] (the subset Wedeo has
// enum variants for).
const COLOR_SPACE_NAMES: &[&str] = &[
    "gbr",
    "bt709",
    "unknown",
    "reserved",
    "fcc",
    "bt470bg",
    "smpte170m",
    "smpte240m",
    "ycgco",
    "bt2020nc",
    "bt2020c",
];

// FFmpeg: static const char * const alpha_mode_names[] =
// { "unspecified", "premultiplied", "straight" }.
const ALPHA_MODE_NAMES: &[&str] = &["unspecified", "premultiplied", "straight"];

/// Options of the `format` filter, mirroring FFmpeg's `AVOption` table.
#[derive(Debug, Clone, Default)]
pub struct FormatOptions {
    /// A '|'-separated list of pixel formats
    pub pix_fmts: String,
    /// A '|'-separated list of color spaces
    pub color_spaces: String,
    /// A '|'-separated list of color ranges
    pub color_ranges: String,
    /// A '|'-separated list of alpha modes
    pub alpha_modes: String,
    /// A '|'-separated list of chroma sample locations
    pub chroma_locations: String,
}

impl FormatOptions {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn pix_fmts(mut self, value: impl Into<String>) -> Self {
        self.pix_fmts = value.into();
        self
    }

    pub fn color_spaces(mut self, value: impl Into<String>) -> Self {
        self.color_spaces = value.into();
        self
    }

    pub fn color_ranges(mut self, value: impl Into<String>) -> Self {
        self.color_ranges = value.into();
        self
    }

    pub fn alpha_modes(mut self, value: impl Into<String>) -> Self {
        self.alpha_modes = value.into();
        self
    }

    pub fn chroma_locations(mut self, value: impl Into<String>) -> Self {
        self.chroma_locations = value.into();
        self
    }
}

/// The `format` filter instance.
///
/// Field comments are copied from FFmpeg's `FormatContext`.
#[derive(Debug, Clone)]
pub struct FormatFilter {
    options: FormatOptions,
    /// parsed from `pix_fmts`
    pub formats: Vec<PixelFormat>,
    /// parsed from `color_spaces`
    pub color_spaces: Vec<ColorSpace>,
    /// parsed from `color_ranges`
    pub color_ranges: Vec<ColorRange>,
    /// parsed from `alpha_modes` (Wedeo has no alpha-mode enum)
    pub alpha_modes: Vec<String>,
    /// parsed from `chroma_locations`
    pub chroma_locations: Vec<ChromaLocation>,
    activated: bool,
}

impl FormatFilter {
    /// Construct a `format` filter requesting the given pixel formats.
    pub fn new(formats: impl IntoIterator<Item = PixelFormat>) -> Self {
        let formats: Vec<PixelFormat> = formats.into_iter().collect();
        let pix_fmts = formats
            .iter()
            .map(|fmt| fmt.name())
            .collect::<Vec<_>>()
            .join("|");
        Self {
            options: FormatOptions::new().pix_fmts(pix_fmts),
            formats,
            color_spaces: Vec::new(),
            color_ranges: Vec::new(),
            alpha_modes: Vec::new(),
            chroma_locations: Vec::new(),
            activated: false,
        }
    }

    /// Construct a `format` filter from FFmpeg-style options.
    ///
    /// This is the equivalent of FFmpeg's `init()`: `PARSE_LIST` over each of
    /// the five option strings. An empty option means "no restriction",
    /// matching FFmpeg's `invert_formats()` comment and `query_formats()` gate.
    pub fn with_options(options: FormatOptions) -> Result<Self> {
        Ok(Self {
            // FFmpeg's PARSE_LIST(pix_fmts, formats, pix_fmt, parse_pixel_format)
            formats: parse_list(&options.pix_fmts, parse_pixel_format)?,
            color_spaces: parse_list(&options.color_spaces, parse_color_space)?,
            color_ranges: parse_list(&options.color_ranges, parse_color_range)?,
            alpha_modes: parse_list(&options.alpha_modes, parse_alpha_mode)?,
            chroma_locations: parse_list(&options.chroma_locations, parse_chroma_location)?,
            options,
            activated: false,
        })
    }

    pub fn options(&self) -> &FormatOptions {
        &self.options
    }

    pub fn is_activated(&self) -> bool {
        self.activated
    }
}

impl Filter for FormatFilter {
    fn descriptor(&self) -> &FilterDescriptor {
        &FORMAT_DESCRIPTOR
    }

    fn activate(&mut self) -> Result<()> {
        // FFmpeg has no activate() callback; negotiation happens once in
        // init()/query_formats(). The frozen interface has no equivalent, so
        // activation only records that the filter is live.
        self.activated = true;
        Ok(())
    }

    fn filter_frame(&mut self, mut frame: Frame) -> Result<Vec<Frame>> {
        // FFmpeg's format filter has NO filter_frame(): frames are passed
        // through by the framework unchanged (AVFILTER_FLAG_METADATA_ONLY).
        // We must not rewrite frame.format either: without link negotiation
        // the incoming format is already the negotiated one, and changing it
        // would mislabel the pixel data. Record the requests on the frame,
        // the only channel the frozen Frame interface exposes.
        if !self.formats.is_empty() {
            frame.metadata.set(
                METADATA_PIX_FMTS,
                self.formats
                    .iter()
                    .map(|fmt| fmt.name())
                    .collect::<Vec<_>>()
                    .join("|"),
            );
        }
        if !self.color_spaces.is_empty() {
            frame.metadata.set(
                METADATA_COLOR_SPACES,
                self.color_spaces
                    .iter()
                    .map(|csp| color_space_name(*csp))
                    .collect::<Vec<_>>()
                    .join("|"),
            );
        }
        if !self.color_ranges.is_empty() {
            frame.metadata.set(
                METADATA_COLOR_RANGES,
                self.color_ranges
                    .iter()
                    .map(|range| color_range_name(*range))
                    .collect::<Vec<_>>()
                    .join("|"),
            );
        }
        if !self.alpha_modes.is_empty() {
            frame
                .metadata
                .set(METADATA_ALPHA_MODES, self.alpha_modes.join("|"));
        }
        if !self.chroma_locations.is_empty() {
            frame.metadata.set(
                METADATA_CHROMA_LOCATIONS,
                self.chroma_locations
                    .iter()
                    .map(|loc| loc.name())
                    .collect::<Vec<_>>()
                    .join("|"),
            );
        }
        Ok(vec![frame])
    }
}

// ---------------------------------------------------------------------------
// Option parsing
// ---------------------------------------------------------------------------

// FFmpeg: PARSE_LIST(strfield, fmtfield, tvar, parse) splits on '|' and parses
// each token. The loop is skipped entirely for an unset/empty option.
fn parse_list<T>(value: &str, parse_one: fn(&str) -> Result<T>) -> Result<Vec<T>> {
    if value.is_empty() {
        return Ok(Vec::new());
    }
    value.split('|').map(parse_one).collect()
}

// FFmpeg: strtol(arg, &tail, 0) - accept decimal, 0x hex and 0-prefixed octal.
fn parse_c_int(arg: &str) -> Option<i32> {
    let (negative, rest) = if let Some(rest) = arg.strip_prefix('-') {
        (true, rest)
    } else if let Some(rest) = arg.strip_prefix('+') {
        (false, rest)
    } else {
        (false, arg)
    };
    let (radix, digits) =
        if let Some(digits) = rest.strip_prefix("0x").or_else(|| rest.strip_prefix("0X")) {
            (16, digits)
        } else if rest.len() > 1 && rest.starts_with('0') {
            (8, &rest[1..])
        } else {
            (10, rest)
        };
    if digits.is_empty() {
        return None;
    }
    let value = i32::from_str_radix(digits, radix).ok()?;
    Some(if negative { -value } else { value })
}

// FFmpeg: av_get_pix_fmt() special-cases "rgb32"/"bgr32" and retries with the
// host-endian suffix ("le" on little-endian, "be" on big-endian) appended.
fn pixel_format_from_name(arg: &str) -> Option<PixelFormat> {
    let little_endian = cfg!(target_endian = "little");
    let name = match arg {
        "rgb32" if little_endian => "bgra",
        "rgb32" => "argb",
        "bgr32" if little_endian => "rgba",
        "bgr32" => "abgr",
        _ => arg,
    };
    if let Some(fmt) = ALL_PIXEL_FORMATS.iter().find(|fmt| fmt.name() == name) {
        return Some(*fmt);
    }
    let suffix = if little_endian { "le" } else { "be" };
    let suffixed = format!("{name}{suffix}");
    ALL_PIXEL_FORMATS
        .iter()
        .find(|fmt| fmt.name() == suffixed)
        .copied()
}

fn pixel_format_from_i32(value: i32) -> Option<PixelFormat> {
    ALL_PIXEL_FORMATS
        .iter()
        .copied()
        .find(|fmt| *fmt as i32 == value)
}

// FFmpeg: DEFINE_PARSE(pixel_format, AVPixelFormat, av_get_pix_fmt, "pixel
// format", !av_pix_fmt_desc_get(val))
fn parse_pixel_format(arg: &str) -> Result<PixelFormat> {
    if let Some(fmt) = pixel_format_from_name(arg) {
        return Ok(fmt);
    }
    let value = parse_c_int(arg).ok_or(Error::InvalidArgument)?;
    // extra_test rejects AVPixelFormat values without a descriptor, i.e. -1.
    pixel_format_from_i32(value).ok_or(Error::InvalidArgument)
}

fn color_space_from_i32(value: i32) -> Option<ColorSpace> {
    ALL_COLOR_SPACES
        .iter()
        .copied()
        .find(|csp| *csp as i32 == value)
}

fn color_space_name(csp: ColorSpace) -> &'static str {
    let index = ALL_COLOR_SPACES
        .iter()
        .position(|candidate| *candidate == csp)
        .unwrap_or(2);
    COLOR_SPACE_NAMES.get(index).copied().unwrap_or("unknown")
}

// FFmpeg: DEFINE_PARSE(color_space, AVColorSpace, av_color_space_from_name,
// "color space", 0)
fn parse_color_space(arg: &str) -> Result<ColorSpace> {
    if let Some(index) = COLOR_SPACE_NAMES.iter().position(|name| *name == arg)
        && let Some(csp) = color_space_from_i32(index as i32)
    {
        return Ok(csp);
    }
    let value = parse_c_int(arg).ok_or(Error::InvalidArgument)?;
    color_space_from_i32(value).ok_or(Error::InvalidArgument)
}

fn color_range_name(range: ColorRange) -> &'static str {
    let index = match range {
        ColorRange::Unspecified => 0,
        ColorRange::Mpeg => 1,
        ColorRange::Jpeg => 2,
    };
    COLOR_RANGE_NAMES[index]
}

// FFmpeg: DEFINE_PARSE(color_range, AVColorRange, av_color_range_from_name,
// "color range", 0)
fn parse_color_range(arg: &str) -> Result<ColorRange> {
    if let Some(index) = COLOR_RANGE_NAMES.iter().position(|name| *name == arg) {
        return Ok(ALL_COLOR_RANGES[index]);
    }
    let value = parse_c_int(arg).ok_or(Error::InvalidArgument)?;
    ALL_COLOR_RANGES
        .get(usize::try_from(value).map_err(|_| Error::InvalidArgument)?)
        .copied()
        .ok_or(Error::InvalidArgument)
}

// FFmpeg: DEFINE_PARSE(chroma_location, AVChromaLocation,
// av_chroma_location_from_name, "chroma location", 0)
fn parse_chroma_location(arg: &str) -> Result<ChromaLocation> {
    if let Some(loc) = ALL_CHROMA_LOCATIONS.iter().find(|loc| loc.name() == arg) {
        return Ok(*loc);
    }
    let value = parse_c_int(arg).ok_or(Error::InvalidArgument)?;
    ALL_CHROMA_LOCATIONS
        .get(usize::try_from(value).map_err(|_| Error::InvalidArgument)?)
        .copied()
        .ok_or(Error::InvalidArgument)
}

// FFmpeg: DEFINE_PARSE(alpha_mode, AVAlphaMode, av_alpha_mode_from_name,
// "alpha mode", 0). Wedeo has no alpha-mode enum, so keep the canonical name.
fn parse_alpha_mode(arg: &str) -> Result<String> {
    if ALPHA_MODE_NAMES.contains(&arg) {
        return Ok(arg.to_string());
    }
    let value = parse_c_int(arg).ok_or(Error::InvalidArgument)?;
    ALPHA_MODE_NAMES
        .get(usize::try_from(value).map_err(|_| Error::InvalidArgument)?)
        .map(|name| (*name).to_string())
        .ok_or(Error::InvalidArgument)
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

struct FormatFilterFactory;

impl FilterFactory for FormatFilterFactory {
    fn descriptor(&self) -> &FilterDescriptor {
        &FORMAT_DESCRIPTOR
    }

    // `FilterFactory::create()` has no options parameter, so a factory-created
    // filter has no restriction (FFmpeg's default when pix_fmts is unset).
    // Callers needing a restriction construct FormatFilter directly.
    fn create(&self) -> Result<Box<dyn Filter>> {
        Ok(Box::new(FormatFilter::new([])))
    }
}

inventory::submit!(&FormatFilterFactory as &dyn FilterFactory);

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use wedeo_core::Buffer;
    use wedeo_core::error::Error;
    use wedeo_core::frame::FramePlane;

    #[test]
    fn descriptor_matches_ffmpeg() {
        let factory = FormatFilterFactory;
        let desc = factory.descriptor();
        assert_eq!(desc.name, "format");
        assert_eq!(
            desc.description,
            "Convert the input video to one of the specified pixel formats."
        );
        assert_eq!(desc.flags, FilterFlags::METADATA_ONLY);
        assert_eq!(desc.inputs.len(), 1);
        assert_eq!(desc.inputs[0].name, "default");
        assert_eq!(desc.inputs[0].media_type, MediaType::Video);
        assert_eq!(desc.inputs[0].direction, FilterPadDirection::Input);
        assert_eq!(desc.outputs.len(), 1);
        assert_eq!(desc.outputs[0].direction, FilterPadDirection::Output);
    }

    #[test]
    fn constructor_accepts_pixel_formats() {
        let filter = FormatFilter::new([PixelFormat::Yuv420p, PixelFormat::Rgb24]);
        assert_eq!(
            filter.formats,
            vec![PixelFormat::Yuv420p, PixelFormat::Rgb24]
        );
        assert_eq!(filter.options().pix_fmts, "yuv420p|rgb24");
        assert!(filter.color_spaces.is_empty());
        assert!(!filter.is_activated());
    }

    #[test]
    fn parse_pipe_separated_pixel_formats() {
        let filter =
            FormatFilter::with_options(FormatOptions::new().pix_fmts("yuv420p|rgb24|rgba"))
                .unwrap();
        assert_eq!(
            filter.formats,
            vec![PixelFormat::Yuv420p, PixelFormat::Rgb24, PixelFormat::Rgba]
        );
    }

    #[test]
    fn parse_pixel_format_names_and_numeric_fallback() {
        assert_eq!(parse_pixel_format("yuv420p").unwrap(), PixelFormat::Yuv420p);
        assert_eq!(parse_pixel_format("0").unwrap(), PixelFormat::Yuv420p);
        assert_eq!(parse_pixel_format("28").unwrap(), PixelFormat::Rgba);
        assert_eq!(parse_pixel_format("0x1c").unwrap(), PixelFormat::Rgba);
        // FFmpeg's av_get_pix_fmt() endianness / alias behavior.
        assert_eq!(
            parse_pixel_format("yuv420p10").unwrap(),
            PixelFormat::Yuv420p10le
        );
        assert_eq!(parse_pixel_format("rgb32").unwrap(), PixelFormat::Bgra);
        assert_eq!(parse_pixel_format("bgr32").unwrap(), PixelFormat::Rgba);
        // extra_test rejects AVPixelFormat values with no descriptor.
        assert_eq!(parse_pixel_format("-1"), Err(Error::InvalidArgument));
        assert_eq!(parse_pixel_format("bogus"), Err(Error::InvalidArgument));
    }

    #[test]
    fn parse_color_options() {
        let filter = FormatFilter::with_options(
            FormatOptions::new()
                .color_spaces("bt709|bt2020nc")
                .color_ranges("tv|pc")
                .chroma_locations("left|topleft")
                .alpha_modes("straight"),
        )
        .unwrap();
        assert_eq!(
            filter.color_spaces,
            vec![ColorSpace::Bt709, ColorSpace::Bt2020Ncl]
        );
        assert_eq!(
            filter.color_ranges,
            vec![ColorRange::Mpeg, ColorRange::Jpeg]
        );
        assert_eq!(
            filter.chroma_locations,
            vec![ChromaLocation::Left, ChromaLocation::TopLeft]
        );
        assert_eq!(filter.alpha_modes, vec!["straight".to_string()]);
        assert_eq!(parse_color_space("gbr").unwrap(), ColorSpace::Rgb);
        assert_eq!(
            parse_color_space("unknown").unwrap(),
            ColorSpace::Unspecified
        );
        assert_eq!(
            parse_color_range("unknown").unwrap(),
            ColorRange::Unspecified
        );
        assert_eq!(
            parse_chroma_location("unspecified").unwrap(),
            ChromaLocation::Unspecified
        );
        assert_eq!(parse_alpha_mode("1").unwrap(), "premultiplied");
        assert_eq!(parse_color_space("nope"), Err(Error::InvalidArgument));
    }

    #[test]
    fn empty_options_mean_no_restriction() {
        let filter = FormatFilter::with_options(FormatOptions::new()).unwrap();
        assert!(filter.formats.is_empty());
        assert!(filter.color_spaces.is_empty());
        assert!(filter.color_ranges.is_empty());
        assert!(filter.alpha_modes.is_empty());
        assert!(filter.chroma_locations.is_empty());
    }

    #[test]
    fn filter_frame_passes_through_without_conversion() {
        let mut filter = FormatFilter::new([PixelFormat::Rgb24]);
        filter.activate().unwrap();
        assert!(filter.is_activated());

        let mut frame = Frame::new_video(16, 8, PixelFormat::Yuv420p);
        frame.pts = 123;
        frame.duration = 7;
        frame.metadata.set("title", "keep me");
        frame.video_mut().unwrap().planes.push(FramePlane {
            buffer: Buffer::from_slice(&[1, 2, 3, 4]),
            offset: 0,
            linesize: 4,
        });

        let out = filter.filter_frame(frame).unwrap();
        assert_eq!(out.len(), 1);
        let out = &out[0];
        assert_eq!(out.pts, 123);
        assert_eq!(out.duration, 7);
        assert_eq!(out.metadata.get("title"), Some("keep me"));
        assert_eq!(out.metadata.get(METADATA_PIX_FMTS), Some("rgb24"));
        let video = out.video().unwrap();
        assert_eq!(video.width, 16);
        assert_eq!(video.height, 8);
        // Never rewrite the pixel format without an actual conversion.
        assert_eq!(video.format, PixelFormat::Yuv420p);
        assert_eq!(video.planes.len(), 1);
        assert_eq!(video.planes[0].linesize, 4);
        assert_eq!(video.planes[0].buffer.data(), &[1, 2, 3, 4]);
    }

    #[test]
    fn filter_frame_records_all_requested_lists() {
        let mut filter = FormatFilter::with_options(
            FormatOptions::new()
                .pix_fmts("yuv420p")
                .color_spaces("bt709")
                .color_ranges("tv")
                .alpha_modes("straight")
                .chroma_locations("left"),
        )
        .unwrap();
        let out = filter
            .filter_frame(Frame::new_video(2, 2, PixelFormat::Yuv420p))
            .unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].metadata.get(METADATA_PIX_FMTS), Some("yuv420p"));
        assert_eq!(out[0].metadata.get(METADATA_COLOR_SPACES), Some("bt709"));
        assert_eq!(out[0].metadata.get(METADATA_COLOR_RANGES), Some("tv"));
        assert_eq!(out[0].metadata.get(METADATA_ALPHA_MODES), Some("straight"));
        assert_eq!(out[0].metadata.get(METADATA_CHROMA_LOCATIONS), Some("left"));
    }

    #[test]
    fn filter_frame_without_restriction_adds_no_metadata() {
        let mut filter = FormatFilter::new([]);
        let out = filter
            .filter_frame(Frame::new_video(2, 2, PixelFormat::Yuv420p))
            .unwrap();
        assert_eq!(out.len(), 1);
        assert!(out[0].metadata.is_empty());
    }

    #[test]
    fn registration_is_discoverable() {
        let factory = find_filter_by_name("format").expect("format filter not registered");
        assert_eq!(factory.descriptor().name, "format");
        let filter = factory.create().unwrap();
        assert!(
            filter
                .descriptor()
                .flags
                .contains(FilterFlags::METADATA_ONLY)
        );
        assert!(filters().any(|f| f.descriptor().name == "format"));
    }
}
