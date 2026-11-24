//! Format negotiation types for filter pads.
//!
//! FFmpeg references: `libavfilter/avfilter.h` (`AVFilterFormatsConfig`),
//! `libavfilter/filters.h` (`query_func2`, `FilterFormatsState`) and
//! `libavfilter/vf_copy.c` (`ff_set_common_formats2`).
//!
//! FFmpeg stores format lists as refcounted arrays of `int` values; in pure
//! Rust we use typed [`PixelFormat`]/[`SampleFormat`] values in a small enum.

use wedeo_core::error::{Error, Result};
use wedeo_core::media_type::MediaType;
use wedeo_core::pixel_format::PixelFormat;
use wedeo_core::sample_format::SampleFormat;

use crate::{FilterDescriptor, FilterPadDescriptor, FilterPadDirection};

/// One entry of an FFmpeg `AVFilterFormats` list.
///
/// FFmpeg stores both `AVPixelFormat` and `AVSampleFormat` values as plain
/// integers; the media type of the pad tells which kind a number denotes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FilterFormat {
    /// A pixel format (FFmpeg `AVPixelFormat`).
    Pixel(PixelFormat),
    /// A sample format (FFmpeg `AVSampleFormat`).
    Sample(SampleFormat),
}

impl FilterFormat {
    /// The media type this format belongs to.
    pub fn media_type(self) -> MediaType {
        match self {
            Self::Pixel(_) => MediaType::Video,
            Self::Sample(_) => MediaType::Audio,
        }
    }

    /// The FFmpeg numeric value of this format.
    pub fn as_raw(self) -> i32 {
        match self {
            Self::Pixel(format) => format as i32,
            Self::Sample(format) => format as i32,
        }
    }

    /// Interpret an FFmpeg numeric format value for the given media type.
    ///
    /// Returns `None` for media types without format lists and for values not
    /// represented by wedeo's core enums.
    pub fn from_raw(media_type: MediaType, value: i32) -> Option<Self> {
        match media_type {
            MediaType::Video => pixel_format_from_raw(value).map(Self::Pixel),
            MediaType::Audio => sample_format_from_raw(value).map(Self::Sample),
            _ => None,
        }
    }
}

/// Match an FFmpeg pixel-format number against the variants known to wedeo.
///
/// Written as an if-chain rather than an exhaustive `match` so that adding a
/// new core `PixelFormat` variant does not make this conversion stale.
fn pixel_format_from_raw(value: i32) -> Option<PixelFormat> {
    macro_rules! check {
        ($($variant:ident),+ $(,)?) => {
            $(if value == PixelFormat::$variant as i32 {
                return Some(PixelFormat::$variant);
            })+
        };
    }
    check!(
        None,
        Yuv420p,
        Yuyv422,
        Rgb24,
        Bgr24,
        Yuv422p,
        Yuv444p,
        Yuv410p,
        Yuv411p,
        Gray8,
        MonoWhite,
        MonoBlack,
        Pal8,
        Yuvj420p,
        Yuvj422p,
        Yuvj444p,
        Uyvy422,
        Nv12,
        Nv21,
        Argb,
        Rgba,
        Abgr,
        Bgra,
        Gray16be,
        Gray16le,
        Yuv420p16le,
        Yuv420p16be,
        Rgb48be,
        Rgb48le,
        Yuv420p10le,
        Yuv420p10be,
    );
    None
}

fn sample_format_from_raw(value: i32) -> Option<SampleFormat> {
    macro_rules! check {
        ($($variant:ident),+ $(,)?) => {
            $(if value == SampleFormat::$variant as i32 {
                return Some(SampleFormat::$variant);
            })+
        };
    }
    check!(
        None, U8, S16, S32, Flt, Dbl, U8p, S16p, S32p, Fltp, Dblp, S64, S64p,
    );
    None
}

/// List of formats supported on a pad, mirroring FFmpeg's `AVFilterFormats`.
///
/// FFmpeg uses a `NULL` list to mean "unset"; wedeo spells that state out as
/// [`FilterFormats::Any`], which matches every format. An explicit empty list
/// means the pad supports no formats at all and fails negotiation.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum FilterFormats {
    /// No restriction. This is the default for pads of filters that do not
    /// implement [`crate::Filter::query_formats`].
    #[default]
    Any,
    /// Explicit list of supported formats.
    List(Vec<FilterFormat>),
}

impl FilterFormats {
    /// Unrestricted formats.
    pub fn any() -> Self {
        Self::Any
    }

    /// Build a list from pixel formats.
    pub fn from_pixels(formats: impl IntoIterator<Item = PixelFormat>) -> Self {
        Self::List(formats.into_iter().map(FilterFormat::Pixel).collect())
    }

    /// Build a list from sample formats.
    pub fn from_samples(formats: impl IntoIterator<Item = SampleFormat>) -> Self {
        Self::List(formats.into_iter().map(FilterFormat::Sample).collect())
    }

    /// Build a list from FFmpeg-style numeric values for a media type.
    ///
    /// An unknown value is an invalid argument, matching FFmpeg's refusal to
    /// negotiate an unsupported format number.
    pub fn from_raw_list(media_type: MediaType, values: &[i32]) -> Result<Self> {
        values
            .iter()
            .map(|&value| {
                FilterFormat::from_raw(media_type, value).ok_or_else(|| {
                    Error::Other(format!("unsupported {media_type} format number {value}"))
                })
            })
            .collect::<Result<Vec<_>>>()
            .map(Self::List)
    }

    /// Whether this is an unrestricted format set.
    pub fn is_any(&self) -> bool {
        matches!(self, Self::Any)
    }

    /// Whether this is an explicit, empty list.
    pub fn is_empty(&self) -> bool {
        matches!(self, Self::List(list) if list.is_empty())
    }

    /// Number of explicit entries (`0` for [`Self::Any`]).
    pub fn len(&self) -> usize {
        match self {
            Self::Any => 0,
            Self::List(list) => list.len(),
        }
    }

    /// Iterate over the explicit entries.
    pub fn iter(&self) -> impl Iterator<Item = FilterFormat> + '_ {
        match self {
            Self::Any => [].iter().copied(),
            Self::List(list) => list.iter().copied(),
        }
    }

    /// Whether the given format is supported. [`Self::Any`] accepts anything.
    pub fn contains(&self, format: FilterFormat) -> bool {
        match self {
            Self::Any => true,
            Self::List(list) => list.contains(&format),
        }
    }

    /// Append a format to an explicit list. No-op for [`Self::Any`].
    pub fn push(&mut self, format: FilterFormat) {
        if let Self::List(list) = self {
            list.push(format);
        }
    }

    /// Keep only entries of the given media type.
    ///
    /// Used when one list is applied to pads of several media types, like
    /// FFmpeg's `ff_set_common_formats2()`. Returns `None` when there is no
    /// applicable entry for that media type.
    pub fn for_media_type(&self, media_type: MediaType) -> Option<Self> {
        match self {
            Self::Any => Some(Self::Any),
            Self::List(list) => {
                let filtered: Vec<_> = list
                    .iter()
                    .copied()
                    .filter(|format| format.media_type() == media_type)
                    .collect();
                (!filtered.is_empty()).then_some(Self::List(filtered))
            }
        }
    }

    /// Return the pixel formats in this list.
    pub fn to_pixels(&self) -> Vec<PixelFormat> {
        self.iter()
            .filter_map(|format| match format {
                FilterFormat::Pixel(pixel) => Some(pixel),
                FilterFormat::Sample(_) => None,
            })
            .collect()
    }

    /// Return the sample formats in this list.
    pub fn to_samples(&self) -> Vec<SampleFormat> {
        self.iter()
            .filter_map(|format| match format {
                FilterFormat::Sample(sample) => Some(sample),
                FilterFormat::Pixel(_) => None,
            })
            .collect()
    }

    /// Intersect two format lists, preserving the order of `self`.
    ///
    /// An unrestricted list intersected with an explicit list yields the
    /// explicit list; two unrestricted lists yield an unrestricted list.
    pub fn intersect(&self, other: &Self) -> Self {
        match (self, other) {
            (Self::Any, other) => other.clone(),
            (this, Self::Any) => this.clone(),
            (Self::List(this), Self::List(other)) => Self::List(
                this.iter()
                    .filter(|f| other.contains(*f))
                    .copied()
                    .collect(),
            ),
        }
    }
}

impl From<Vec<FilterFormat>> for FilterFormats {
    fn from(formats: Vec<FilterFormat>) -> Self {
        Self::List(formats)
    }
}

impl FromIterator<FilterFormat> for FilterFormats {
    fn from_iter<T: IntoIterator<Item = FilterFormat>>(iter: T) -> Self {
        Self::List(iter.into_iter().collect())
    }
}

/// Format configuration of a single filter pad.
///
/// FFmpeg keeps one `AVFilterFormatsConfig` per link direction in an
/// `AVFilterContext`; here every pad carries its own copy, keyed by pad index
/// in [`FormatsConfig`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PadFormats {
    /// Pad name, copied from the filter's pad descriptor.
    pub name: &'static str,
    /// Media type of the pad.
    pub media_type: MediaType,
    /// Whether this is an input or an output pad.
    pub direction: FilterPadDirection,
    /// Supported pixel/sample formats on this pad.
    pub formats: FilterFormats,
}

impl PadFormats {
    /// Whether this pad accepts the given format.
    pub fn accepts(&self, format: FilterFormat) -> bool {
        self.formats.contains(format)
    }
}

impl FilterPadDescriptor {
    /// Default format configuration for this pad: unrestricted.
    ///
    /// Filters that need a narrower set should set it in
    /// [`crate::Filter::query_formats`].
    pub fn default_formats(&self) -> PadFormats {
        PadFormats {
            name: self.name,
            media_type: self.media_type,
            direction: self.direction,
            formats: FilterFormats::Any,
        }
    }
}

/// Format configuration of one filter instance's pads.
///
/// Mirrors FFmpeg 8's `query_func2(const AVFilterContext *ctx,
/// AVFilterFormatsConfig **cfg_in, AVFilterFormatsConfig **cfg_out)`: inputs
/// and outputs are parallel to the filter's pad descriptors, so a filter can
/// fill them in by pad index without knowing the graph layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormatsConfig {
    /// Configurations for input pads, in descriptor order.
    pub inputs: Vec<PadFormats>,
    /// Configurations for output pads, in descriptor order.
    pub outputs: Vec<PadFormats>,
}

impl FormatsConfig {
    /// An empty configuration (for filters with a dynamically built pad set).
    pub fn new() -> Self {
        Self {
            inputs: Vec::new(),
            outputs: Vec::new(),
        }
    }

    /// Build a configuration with one unrestricted entry per pad of `desc`.
    pub fn from_descriptor(desc: &FilterDescriptor) -> Self {
        Self {
            inputs: desc
                .inputs
                .iter()
                .map(FilterPadDescriptor::default_formats)
                .collect(),
            outputs: desc
                .outputs
                .iter()
                .map(FilterPadDescriptor::default_formats)
                .collect(),
        }
    }

    /// Iterate over all pad configurations, inputs first.
    pub fn pads_mut(&mut self) -> impl Iterator<Item = &mut PadFormats> {
        self.inputs.iter_mut().chain(self.outputs.iter_mut())
    }

    /// Look up a pad configuration by direction and index.
    pub fn pad(&self, direction: FilterPadDirection, index: usize) -> Result<&PadFormats> {
        let pads = match direction {
            FilterPadDirection::Input => &self.inputs,
            FilterPadDirection::Output => &self.outputs,
        };
        pads.get(index).ok_or(Error::InvalidArgument)
    }

    /// Look up a pad configuration by direction and index, mutably.
    pub fn pad_mut(
        &mut self,
        direction: FilterPadDirection,
        index: usize,
    ) -> Result<&mut PadFormats> {
        let pads = match direction {
            FilterPadDirection::Input => &mut self.inputs,
            FilterPadDirection::Output => &mut self.outputs,
        };
        pads.get_mut(index).ok_or(Error::InvalidArgument)
    }

    /// Apply a format list to every pad that carries a compatible media type.
    ///
    /// This is the equivalent of FFmpeg's `ff_set_common_formats2()`: pads
    /// whose media type has no entry in `formats` are left unchanged.
    pub fn set_common_formats(&mut self, formats: FilterFormats) {
        for pad in self.pads_mut() {
            if let Some(compatible) = formats.for_media_type(pad.media_type) {
                pad.formats = compatible;
            }
        }
    }

    /// Apply a pixel format list to all video pads.
    pub fn set_common_pixel_formats(&mut self, formats: &[PixelFormat]) {
        self.set_common_formats(FilterFormats::from_pixels(formats.iter().copied()));
    }

    /// Apply a sample format list to all audio pads.
    pub fn set_common_sample_formats(&mut self, formats: &[SampleFormat]) {
        self.set_common_formats(FilterFormats::from_samples(formats.iter().copied()));
    }

    /// Pick the common format between output pad `out_pad` and input pad
    /// `in_pad` of the same filter.
    ///
    /// Returns an error when the pads have different media types or no format
    /// in common, so graph negotiation can report a precise failure.
    pub fn negotiate(&self, out_pad: usize, in_pad: usize) -> Result<FilterFormats> {
        self.negotiate_with(out_pad, self, in_pad)
    }

    /// Pick the common format between output pad `out_pad` of this filter and
    /// input pad `in_pad` of another filter, i.e. the two ends of a link.
    pub fn negotiate_with(
        &self,
        out_pad: usize,
        other: &Self,
        in_pad: usize,
    ) -> Result<FilterFormats> {
        let output = self.outputs.get(out_pad).ok_or(Error::InvalidArgument)?;
        let input = other.inputs.get(in_pad).ok_or(Error::InvalidArgument)?;
        if output.media_type != input.media_type {
            return Err(Error::InvalidArgument);
        }
        let common = output.formats.intersect(&input.formats);
        if common.is_empty() {
            return Err(Error::Other(format!(
                "no common format between output pad {out_pad} and input pad {in_pad}: {:?} vs {:?}",
                output.formats, input.formats,
            )));
        }
        Ok(common)
    }
}

impl Default for FormatsConfig {
    fn default() -> Self {
        Self::new()
    }
}
