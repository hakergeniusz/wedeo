//! Video crop filter, ported from FFmpeg's `libavfilter/vf_crop.c`.
//!
//! Like the C filter, this does not copy pixels: it advances each plane's data
//! pointer (represented here by `FramePlane::offset`) and shrinks the frame's
//! width/height. Subsampling (`hsub`/`vsub`) and the per-plane maximum pixel
//! step (`max_step`) come from the pixel-format descriptor, mirroring
//! `config_crop_area()` and `filter_frame()` in `vf_crop.c`.
//!
//! The options are plain integers rather than FFmpeg's expression strings:
//! `None` means "use the FFmpeg default" (full input size / centered offset).
//! Set `exact` to disable the alignment of width/height/x/y to the chroma
//! subsampling grid (FFmpeg's `exact` option). Set `keep_aspect` to preserve
//! the display aspect ratio via the output sample aspect ratio.

use wedeo_core::Error;
use wedeo_core::Result;
use wedeo_core::frame::Frame;
use wedeo_core::media_type::MediaType;
use wedeo_core::pixel_format::PixelFormat;
use wedeo_core::rational::Rational;
use wedeo_core::rational::reduce;
use wedeo_filter::FilterDescriptor;
use wedeo_filter::FilterFlags;
use wedeo_filter::FilterPadDescriptor;
use wedeo_filter::FilterPadDirection;
use wedeo_filter::registry::FilterFactory;

// Re-exported so the registration FATE test can reach the frozen filter
// registry and format negotiation without adding a direct `wedeo-filter`
// dependency to the harness.
pub use wedeo_filter::Filter;
pub use wedeo_filter::formats::{FilterFormat, FilterFormats, FormatsConfig, PadFormats};
pub use wedeo_filter::registry;

// ---------------------------------------------------------------------------
// Descriptor / registration
// ---------------------------------------------------------------------------

static CROP_INPUTS: &[FilterPadDescriptor] = &[FilterPadDescriptor {
    name: "default",
    media_type: MediaType::Video,
    direction: FilterPadDirection::Input,
}];

static CROP_OUTPUTS: &[FilterPadDescriptor] = &[FilterPadDescriptor {
    name: "default",
    media_type: MediaType::Video,
    direction: FilterPadDirection::Output,
}];

static CROP_DESCRIPTOR: FilterDescriptor = FilterDescriptor {
    name: "crop",
    description: "Crop the input video.",
    inputs: CROP_INPUTS,
    outputs: CROP_OUTPUTS,
    // FFmpeg uses FF_FILTER_FLAG_DYNAMIC_FRAME_SIZE; the frozen FilterFlags
    // set has no equivalent, so no flag is reported here.
    flags: FilterFlags::empty(),
};

// ---------------------------------------------------------------------------
// Pixel-format properties (libavutil/pixdesc.c equivalents)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
struct FormatInfo {
    /// log2_chroma_w: chroma width is luma width >> hsub.
    hsub: u8,
    /// log2_chroma_h: chroma height is luma height >> vsub.
    vsub: u8,
    /// Equivalent of `av_image_fill_max_pixsteps()`, in bytes.
    max_step: [usize; 4],
    /// AV_PIX_FMT_FLAG_PAL: planes 1/2 hold the palette, not chroma.
    is_pal: bool,
}

impl FormatInfo {
    const fn new(hsub: u8, vsub: u8, max_step: [usize; 4], is_pal: bool) -> Self {
        Self {
            hsub,
            vsub,
            max_step,
            is_pal,
        }
    }
}

/// Descriptor values used by `vf_crop.c`: log2_chroma_w, log2_chroma_h and the
/// maximum per-plane component `step` from `libavutil/pixdesc.c`.
fn format_info(format: PixelFormat) -> FormatInfo {
    match format {
        PixelFormat::Yuv420p | PixelFormat::Yuvj420p => FormatInfo::new(1, 1, [1, 1, 1, 0], false),
        PixelFormat::Yuv422p | PixelFormat::Yuvj422p => FormatInfo::new(1, 0, [1, 1, 1, 0], false),
        PixelFormat::Yuv444p | PixelFormat::Yuvj444p => FormatInfo::new(0, 0, [1, 1, 1, 0], false),
        PixelFormat::Yuv410p => FormatInfo::new(2, 2, [1, 1, 1, 0], false),
        PixelFormat::Yuv411p => FormatInfo::new(2, 0, [1, 1, 1, 0], false),
        // Packed 4:2:2: Y step 2, U/V step 4 -> max_step 4.
        PixelFormat::Yuyv422 | PixelFormat::Uyvy422 => FormatInfo::new(1, 0, [4, 0, 0, 0], false),
        PixelFormat::Rgb24 | PixelFormat::Bgr24 => FormatInfo::new(0, 0, [3, 0, 0, 0], false),
        // NV12/NV21: interleaved UV, component step 2 in the second plane.
        PixelFormat::Nv12 | PixelFormat::Nv21 => FormatInfo::new(1, 1, [1, 2, 0, 0], false),
        PixelFormat::Argb | PixelFormat::Rgba | PixelFormat::Abgr | PixelFormat::Bgra => {
            FormatInfo::new(0, 0, [4, 0, 0, 0], false)
        }
        PixelFormat::Gray8 | PixelFormat::MonoWhite | PixelFormat::MonoBlack => {
            FormatInfo::new(0, 0, [1, 0, 0, 0], false)
        }
        PixelFormat::Pal8 => FormatInfo::new(0, 0, [1, 0, 0, 0], true),
        PixelFormat::Gray16be | PixelFormat::Gray16le => FormatInfo::new(0, 0, [2, 0, 0, 0], false),
        PixelFormat::Rgb48be | PixelFormat::Rgb48le => FormatInfo::new(0, 0, [6, 0, 0, 0], false),
        PixelFormat::Yuv420p10le
        | PixelFormat::Yuv420p10be
        | PixelFormat::Yuv420p16le
        | PixelFormat::Yuv420p16be => FormatInfo::new(1, 1, [2, 2, 2, 0], false),
        // 10-bit 4:2:2/4:4:4 (H.264 FRext).
        PixelFormat::Yuv422p10le | PixelFormat::Yuv422p10be => {
            FormatInfo::new(1, 0, [2, 2, 2, 0], false)
        }
        PixelFormat::Yuv444p10le | PixelFormat::Yuv444p10be => {
            FormatInfo::new(0, 0, [2, 2, 2, 0], false)
        }
        PixelFormat::None => FormatInfo::new(0, 0, [0, 0, 0, 0], false),
    }
}

/// Formats advertised by `query_formats()`.
///
/// FFmpeg:
/// ```c
/// int reject_flags = AV_PIX_FMT_FLAG_BITSTREAM | FF_PIX_FMT_FLAG_SW_FLAT_SUB;
///
/// return ff_set_common_formats2(ctx, cfg_in, cfg_out,
///                               ff_formats_pixdesc_filter(0, reject_flags));
/// ```
///
/// `FF_PIX_FMT_FLAG_SW_FLAT_SUB` marks non-planar formats with subsampling, so
/// the packed 4:2:2 formats (`yuyv422`, `uyvy422`) are excluded together with
/// the bitstream formats (`monow`, `monoblack`). Paletted formats are accepted
/// by FFmpeg's crop filter and remain in the list.
const SUPPORTED_FORMATS: &[PixelFormat] = &[
    PixelFormat::Yuv420p,
    PixelFormat::Rgb24,
    PixelFormat::Bgr24,
    PixelFormat::Yuv422p,
    PixelFormat::Yuv444p,
    PixelFormat::Yuv410p,
    PixelFormat::Yuv411p,
    PixelFormat::Gray8,
    PixelFormat::Pal8,
    PixelFormat::Yuvj420p,
    PixelFormat::Yuvj422p,
    PixelFormat::Yuvj444p,
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
    PixelFormat::Yuv422p10le,
    PixelFormat::Yuv422p10be,
    PixelFormat::Yuv444p10le,
    PixelFormat::Yuv444p10be,
    PixelFormat::Yuv420p10le,
    PixelFormat::Yuv420p10be,
];

// ---------------------------------------------------------------------------
// Crop filter
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Crop {
    /// x offset option; `None` means centered (FFmpeg's default expression).
    x: Option<i32>,
    /// y offset option; `None` means centered (FFmpeg's default expression).
    y: Option<i32>,
    /// width option; `None` means the full input width (`iw`).
    width: Option<i32>,
    /// height option; `None` means the full input height (`ih`).
    height: Option<i32>,
    /// Preserve display aspect ratio (FFmpeg's `keep_aspect`).
    keep_aspect: bool,
    /// Disable subsampling alignment (FFmpeg's `exact`).
    exact: bool,
    active: bool,
}

impl Crop {
    /// Create a filter with FFmpeg's defaults: full size, centered crop,
    /// subsampling-aligned and with the input sample aspect ratio preserved.
    pub fn new() -> Self {
        Self {
            x: None,
            y: None,
            width: None,
            height: None,
            keep_aspect: false,
            exact: false,
            active: false,
        }
    }

    /// Create a filter with explicit options. `None` selects the FFmpeg
    /// default for that option (full size for width/height, centered for x/y).
    pub fn with_options(
        x: Option<i32>,
        y: Option<i32>,
        width: Option<i32>,
        height: Option<i32>,
    ) -> Self {
        Self {
            x,
            y,
            width,
            height,
            keep_aspect: false,
            exact: false,
            active: false,
        }
    }

    /// Set the x offset.
    pub fn with_x(mut self, x: i32) -> Self {
        self.x = Some(x);
        self
    }

    /// Set the y offset.
    pub fn with_y(mut self, y: i32) -> Self {
        self.y = Some(y);
        self
    }

    /// Set the crop width (FFmpeg's `out_w`/`w`).
    pub fn with_width(mut self, width: i32) -> Self {
        self.width = Some(width);
        self
    }

    /// Set the crop height (FFmpeg's `out_h`/`h`).
    pub fn with_height(mut self, height: i32) -> Self {
        self.height = Some(height);
        self
    }

    /// Enable/disable exact cropping (no alignment to the chroma grid).
    pub fn with_exact(mut self, exact: bool) -> Self {
        self.exact = exact;
        self
    }

    /// Enable/disable display-aspect-ratio preservation.
    pub fn with_keep_aspect(mut self, keep_aspect: bool) -> Self {
        self.keep_aspect = keep_aspect;
        self
    }

    /// Whether `activate()` has been called.
    pub fn is_active(&self) -> bool {
        self.active
    }

    /// Crop one video frame. Mirrors `filter_frame()` in `vf_crop.c`.
    fn crop_frame(&self, frame: Frame) -> Result<Frame> {
        let (w_in, h_in, format) = {
            let video = frame.video().ok_or(Error::InvalidData)?;
            if video.format == PixelFormat::None {
                return Err(Error::InvalidData);
            }
            (video.width as i32, video.height as i32, video.format)
        };
        let info = format_info(format);

        // config_crop_area(): out_w/out_h default to iw/ih.
        let mut w = self.width.unwrap_or(w_in);
        let mut h = self.height.unwrap_or(h_in);
        if !self.exact {
            w &= !((1i32 << info.hsub) - 1);
            h &= !((1i32 << info.vsub) - 1);
        }
        if w <= 0 || h <= 0 || w > w_in || h > h_in {
            return Err(Error::InvalidArgument);
        }

        // config_crop_area() installs centered defaults; filter_frame()
        // re-evaluates x/y for every frame (dynamic frame size parity).
        let mut x = self.x.unwrap_or((w_in - w) / 2);
        let mut y = self.y.unwrap_or((h_in - h) / 2);
        if x < 0 {
            x = 0;
        }
        if y < 0 {
            y = 0;
        }
        // The C source uses unsigned arithmetic here.
        if (x as u32).wrapping_add(w as u32) > w_in as u32 {
            x = w_in - w;
        }
        if (y as u32).wrapping_add(h as u32) > h_in as u32 {
            y = h_in - h;
        }
        if !self.exact {
            x &= !((1i32 << info.hsub) - 1);
            y &= !((1i32 << info.vsub) - 1);
        }

        // FFmpeg logs: "n:%d t:%f x:%d y:%d x+w:%d y+h:%d"; the resolved
        // geometry above is what matters for the actual crop.

        let mut frame = frame;
        let video = frame.video_mut().ok_or(Error::Bug)?;

        if self.keep_aspect {
            // AVRational dar = av_mul_q(sar, (AVRational){ in_w, in_h });
            // av_reduce(&out_sar, dar.num * h, dar.den * w, INT_MAX);
            let sar = if video.sample_aspect_ratio.num != 0 {
                video.sample_aspect_ratio
            } else {
                Rational::new(1, 1)
            };
            let dar_num = sar.num as i64 * w_in as i64;
            let dar_den = sar.den as i64 * h_in as i64;
            let (num, den, _) = reduce(dar_num * h as i64, dar_den * w as i64, i32::MAX as i64);
            video.sample_aspect_ratio = Rational::new(num, den);
        }

        // frame->width = s->w; frame->height = s->h;
        video.width = w as u32;
        video.height = h as u32;

        // frame->data[0] += s->y * linesize[0] + s->x * max_step[0];
        if let Some(plane) = video.planes.get_mut(0) {
            plane.offset += (y as usize) * plane.linesize + (x as usize) * info.max_step[0];
        }

        // if (!(desc->flags & AV_PIX_FMT_FLAG_PAL)) { for (i = 1; i < 3; i++) ... }
        if !info.is_pal {
            for i in 1..3 {
                if let Some(plane) = video.planes.get_mut(i) {
                    // frame->data[i] += (s->y >> s->vsub) * linesize[i];
                    // frame->data[i] += (s->x * s->max_step[i]) >> s->hsub;
                    let dy = (y >> info.vsub) as usize * plane.linesize;
                    let dx = ((x as i64 * info.max_step[i] as i64) >> info.hsub) as usize;
                    plane.offset += dy + dx;
                }
            }
        }

        // /* alpha plane */ if (frame->data[3]) { ... }
        if let Some(plane) = video.planes.get_mut(3) {
            plane.offset += (y as usize) * plane.linesize + (x as usize) * info.max_step[3];
        }

        Ok(frame)
    }
}

impl Default for Crop {
    fn default() -> Self {
        Self::new()
    }
}

impl Filter for Crop {
    fn descriptor(&self) -> &FilterDescriptor {
        &CROP_DESCRIPTOR
    }

    fn activate(&mut self) -> Result<()> {
        self.active = true;
        Ok(())
    }

    fn query_formats(&self, cfg: &mut FormatsConfig) -> Result<()> {
        // FFmpeg: ff_set_common_formats2(ctx, cfg_in, cfg_out,
        // ff_formats_pixdesc_filter(0, reject_flags)) - both pads get the same
        // list, and frames are never converted by this filter.
        cfg.set_common_pixel_formats(SUPPORTED_FORMATS);
        Ok(())
    }

    fn filter_frame(&mut self, frame: Frame) -> Result<Vec<Frame>> {
        Ok(vec![self.crop_frame(frame)?])
    }
}

/// Parse the integer subset of FFmpeg's crop expressions accepted by this
/// port. The geometry options are stored as plain integers (see module docs).
fn parse_int_option(value: &str) -> Result<i32> {
    value
        .trim()
        .parse::<i32>()
        .map_err(|_| Error::InvalidArgument)
}

/// FFmpeg's `AV_OPT_TYPE_BOOL` accepts both numeric and symbolic values.
fn parse_bool_option(value: &str) -> Result<bool> {
    match value {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => Err(Error::InvalidArgument),
    }
}

struct CropFactory;

impl FilterFactory for CropFactory {
    fn descriptor(&self) -> &FilterDescriptor {
        &CROP_DESCRIPTOR
    }

    fn create(&self) -> Result<Box<dyn Filter>> {
        Ok(Box::new(Crop::new()))
    }

    // FFmpeg applies the private options with av_opt_set() before
    // avfilter_graph_config(). `w`/`out_w` and `h`/`out_h` are aliases for the
    // same AVOption in vf_crop.c, so both names are accepted here.
    fn create_with_options(&self, options: &[(&str, &str)]) -> Result<Box<dyn Filter>> {
        let mut filter = Crop::new();
        for &(key, value) in options {
            match key {
                "w" | "out_w" => filter.width = Some(parse_int_option(value)?),
                "h" | "out_h" => filter.height = Some(parse_int_option(value)?),
                "x" => filter.x = Some(parse_int_option(value)?),
                "y" => filter.y = Some(parse_int_option(value)?),
                "keep_aspect" => filter.keep_aspect = parse_bool_option(value)?,
                "exact" => filter.exact = parse_bool_option(value)?,
                // av_opt_set() rejects unknown option names.
                _ => return Err(Error::InvalidArgument),
            }
        }
        Ok(Box::new(filter))
    }
}

inventory::submit!(&CropFactory as &dyn FilterFactory);

// ---------------------------------------------------------------------------
// Unit tests (synthetic frames, exact pixel output)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use wedeo_core::Buffer;
    use wedeo_core::ChromaLocation;
    use wedeo_core::ColorPrimaries;
    use wedeo_core::ColorTransferCharacteristic;
    use wedeo_core::Metadata;
    use wedeo_core::NOPTS_VALUE;
    use wedeo_core::frame::ColorRange;
    use wedeo_core::frame::ColorSpace;
    use wedeo_core::frame::FrameData;
    use wedeo_core::frame::FrameFlags;
    use wedeo_core::frame::FramePlane;
    use wedeo_core::frame::PictureType;
    use wedeo_core::frame::VideoFrameData;

    /// Build a frame whose plane bytes follow a deterministic pattern.
    /// `specs` is one `(plane_width_bytes, plane_height, linesize)` per plane.
    fn make_frame(w: u32, h: u32, format: PixelFormat, specs: &[(usize, usize, usize)]) -> Frame {
        let planes: Vec<FramePlane> = specs
            .iter()
            .enumerate()
            .map(|(p, &(_pw, ph, linesize))| {
                let size = ph * linesize;
                let data: Vec<u8> = (0..size)
                    .map(|i| {
                        let row = i / linesize;
                        let col = i % linesize;
                        (p as u8)
                            .wrapping_mul(50)
                            .wrapping_add((row as u8).wrapping_mul(13))
                            .wrapping_add((col as u8).wrapping_mul(7))
                    })
                    .collect();
                FramePlane {
                    buffer: Buffer::from_slice(&data),
                    offset: 0,
                    linesize,
                }
            })
            .collect();

        Frame {
            data: FrameData::Video(VideoFrameData {
                planes,
                width: w,
                height: h,
                format,
                picture_type: PictureType::None,
                color_range: ColorRange::Unspecified,
                color_space: ColorSpace::Unspecified,
                color_primaries: ColorPrimaries::Unspecified,
                color_trc: ColorTransferCharacteristic::Unspecified,
                chroma_location: ChromaLocation::Unspecified,
                sample_aspect_ratio: Rational::new(1, 1),
                crop_top: 0,
                crop_bottom: 0,
                crop_left: 0,
                crop_right: 0,
            }),
            pts: 123,
            pkt_dts: NOPTS_VALUE,
            best_effort_timestamp: NOPTS_VALUE,
            duration: 40,
            time_base: Rational::new(1, 25),
            flags: FrameFlags::empty(),
            repeat_pict: 0,
            metadata: Metadata::new(),
            side_data: Vec::new(),
        }
    }

    fn plane_byte(plane: &FramePlane, row: usize, col: usize) -> u8 {
        plane.buffer.data()[plane.offset + row * plane.linesize + col]
    }

    /// Compare a `rows x cols` byte region of `dst` against the region of `src`
    /// starting at `(src_row, src_col)` (both in byte coordinates).
    fn assert_region(
        src: &FramePlane,
        dst: &FramePlane,
        src_row: usize,
        src_col: usize,
        rows: usize,
        cols: usize,
    ) {
        for r in 0..rows {
            for c in 0..cols {
                assert_eq!(
                    plane_byte(dst, r, c),
                    plane_byte(src, src_row + r, src_col + c),
                    "mismatch at dst row {r}, col {c}",
                );
            }
        }
    }

    fn crop_one(filter: &Crop, frame: Frame) -> Frame {
        let mut filter = filter.clone();
        filter.activate().unwrap();
        let mut out = filter.filter_frame(frame).unwrap();
        assert_eq!(out.len(), 1);
        out.remove(0)
    }

    #[test]
    fn descriptor_and_activation() {
        let mut crop = Crop::new();
        assert_eq!(crop.descriptor().name, "crop");
        assert_eq!(crop.descriptor().description, "Crop the input video.");
        assert_eq!(crop.descriptor().inputs.len(), 1);
        assert_eq!(crop.descriptor().inputs[0].media_type, MediaType::Video);
        assert_eq!(crop.descriptor().outputs.len(), 1);
        assert!(!crop.is_active());
        crop.activate().unwrap();
        assert!(crop.is_active());
    }

    #[test]
    fn gray8_exact_geometry() {
        // 8x8 luma, linesize 8. Crop (x=2,y=1) size 4x4.
        let src = make_frame(8, 8, PixelFormat::Gray8, &[(8, 8, 8)]);
        let crop = Crop::with_options(Some(2), Some(1), Some(4), Some(4));
        let out = crop_one(&crop, src.clone());

        let sv = src.video().unwrap();
        let dv = out.video().unwrap();
        assert_eq!((dv.width, dv.height), (4, 4));
        // y * linesize + x = 1*8 + 2 = 10.
        assert_eq!(dv.planes[0].offset, 10);
        assert_eq!(dv.planes[0].linesize, sv.planes[0].linesize);
        assert_region(&sv.planes[0], &dv.planes[0], 1, 2, 4, 4);
        // All non-cropped metadata is carried over untouched.
        assert_eq!(out.pts, 123);
        assert_eq!(out.duration, 40);
    }

    #[test]
    fn yuv420p_crops_luma_and_chroma_with_subsampling() {
        // 8x6 4:2:0, linesizes [8, 4, 4]. Crop (x=2,y=2) size 4x4.
        // Chroma offset: (2 >> 1)*4 + ((2 * 1) >> 1) = 4 + 1 = 5.
        let src = make_frame(
            8,
            6,
            PixelFormat::Yuv420p,
            &[(8, 6, 8), (4, 3, 4), (4, 3, 4)],
        );
        let crop = Crop::with_options(Some(2), Some(2), Some(4), Some(4));
        let out = crop_one(&crop, src.clone());

        let sv = src.video().unwrap();
        let dv = out.video().unwrap();
        assert_eq!((dv.width, dv.height), (4, 4));
        assert_eq!(dv.planes[0].offset, 2 * 8 + 2);
        assert_eq!(dv.planes[1].offset, 5);
        assert_eq!(dv.planes[2].offset, 5);
        assert_region(&sv.planes[0], &dv.planes[0], 2, 2, 4, 4);
        assert_region(&sv.planes[1], &dv.planes[1], 1, 1, 2, 2);
        assert_region(&sv.planes[2], &dv.planes[2], 1, 1, 2, 2);
    }

    #[test]
    fn yuv420p_aligns_to_chroma_grid_without_exact() {
        // Request an odd x/y/w/h: without `exact`, all four snap down to even.
        let src = make_frame(
            8,
            8,
            PixelFormat::Yuv420p,
            &[(8, 8, 12), (4, 4, 6), (4, 4, 6)],
        );
        let crop = Crop::with_options(Some(3), Some(3), Some(5), Some(5));
        let out = crop_one(&crop, src.clone());

        let dv = out.video().unwrap();
        assert_eq!((dv.width, dv.height), (4, 4));
        assert_eq!(dv.planes[0].offset, 2 * 12 + 2);
        // (y >> 1) * chroma_linesize + ((x * max_step) >> 1) = 1*6 + 1 = 7.
        assert_eq!(dv.planes[1].offset, 7);
    }

    #[test]
    fn yuv420p_exact_cropping_keeps_odd_geometry() {
        let src = make_frame(
            8,
            8,
            PixelFormat::Yuv420p,
            &[(8, 8, 12), (4, 4, 6), (4, 4, 6)],
        );
        let crop = Crop::with_options(Some(3), Some(3), Some(5), Some(5)).with_exact(true);
        let out = crop_one(&crop, src.clone());

        let sv = src.video().unwrap();
        let dv = out.video().unwrap();
        assert_eq!((dv.width, dv.height), (5, 5));
        assert_eq!(dv.planes[0].offset, 3 * 12 + 3);
        // Chroma: (3 >> 1) * 6 + ((3 * 1) >> 1) = 6 + 1 = 7.
        assert_eq!(dv.planes[1].offset, 7);
        assert_region(&sv.planes[0], &dv.planes[0], 3, 3, 5, 5);
    }

    #[test]
    fn rgb24_uses_component_step_three() {
        // 4x4 RGB24, linesize 12. Crop (x=1,y=1) size 2x2:
        // data += 1*12 + 1*3 = 15 bytes.
        let src = make_frame(4, 4, PixelFormat::Rgb24, &[(12, 4, 12)]);
        let crop = Crop::with_options(Some(1), Some(1), Some(2), Some(2));
        let out = crop_one(&crop, src.clone());

        let sv = src.video().unwrap();
        let dv = out.video().unwrap();
        assert_eq!((dv.width, dv.height), (2, 2));
        // y * linesize + x * max_step[0] = 1*12 + 1*3 = 15.
        assert_eq!(dv.planes[0].offset, 15);
        // Source region starts at row 1, byte column 3 (pixel x=1).
        assert_region(&sv.planes[0], &dv.planes[0], 1, 3, 2, 6);
    }

    #[test]
    fn yuv422p_crops_chroma_height_not() {
        // 8x4 4:2:2, hsub=1, vsub=0. Crop (x=2,y=1) size 4x2.
        let src = make_frame(
            8,
            4,
            PixelFormat::Yuv422p,
            &[(8, 4, 8), (4, 4, 4), (4, 4, 4)],
        );
        let crop = Crop::with_options(Some(2), Some(1), Some(4), Some(2));
        let out = crop_one(&crop, src.clone());

        let sv = src.video().unwrap();
        let dv = out.video().unwrap();
        assert_eq!((dv.width, dv.height), (4, 2));
        // y * linesize + x * max_step[0] = 1*8 + 2 = 10.
        assert_eq!(dv.planes[0].offset, 10);
        // (y >> vsub) * linesize + ((x * max_step) >> hsub) = 1*4 + 1 = 5.
        assert_eq!(dv.planes[1].offset, 5);
        assert_region(&sv.planes[1], &dv.planes[1], 1, 1, 2, 2);
    }

    #[test]
    fn nv12_interleaved_chroma_step() {
        // 8x4 NV12, linesizes [8, 8]. Crop (x=2,y=2) size 4x2.
        // Chroma: (2 >> 1)*8 + ((2 * 2) >> 1) = 8 + 2 = 10.
        let src = make_frame(8, 4, PixelFormat::Nv12, &[(8, 4, 8), (8, 2, 8)]);
        let crop = Crop::with_options(Some(2), Some(2), Some(4), Some(2));
        let out = crop_one(&crop, src.clone());

        let sv = src.video().unwrap();
        let dv = out.video().unwrap();
        assert_eq!((dv.width, dv.height), (4, 2));
        assert_eq!(dv.planes[0].offset, 2 * 8 + 2);
        assert_eq!(dv.planes[1].offset, 10);
        assert_region(&sv.planes[1], &dv.planes[1], 1, 2, 1, 4);
    }

    #[test]
    fn pal8_leaves_palette_plane_alone() {
        // PAL8: plane 0 is pixel data, plane 1 is the palette. vf_crop.c
        // skips planes 1 and 2 for AV_PIX_FMT_FLAG_PAL formats.
        let src = make_frame(4, 4, PixelFormat::Pal8, &[(4, 4, 4), (256, 1, 1024)]);
        let crop = Crop::with_options(Some(1), Some(1), Some(2), Some(2));
        let out = crop_one(&crop, src.clone());

        let sv = src.video().unwrap();
        let dv = out.video().unwrap();
        assert_eq!(dv.planes[0].offset, 5);
        assert_eq!(dv.planes[1].offset, sv.planes[1].offset);
    }

    #[test]
    fn defaults_crop_full_frame() {
        let src = make_frame(6, 4, PixelFormat::Gray8, &[(6, 4, 6)]);
        let out = crop_one(&Crop::new(), src.clone());
        let sv = src.video().unwrap();
        let dv = out.video().unwrap();
        assert_eq!((dv.width, dv.height), (6, 4));
        assert_eq!(dv.planes[0].offset, sv.planes[0].offset);
        assert_region(&sv.planes[0], &dv.planes[0], 0, 0, 4, 6);
    }

    #[test]
    fn offsets_are_clamped_into_the_input() {
        let src = make_frame(8, 8, PixelFormat::Gray8, &[(8, 8, 8)]);
        // Negative offsets clamp to zero.
        let out = crop_one(
            &Crop::with_options(Some(-4), Some(-4), Some(4), Some(4)),
            src.clone(),
        );
        assert_eq!(out.video().unwrap().planes[0].offset, 0);

        // Overflowing x+w clamps x to width - crop_width.
        let out = crop_one(
            &Crop::with_options(Some(6), Some(0), Some(4), Some(4)),
            src.clone(),
        );
        assert_eq!(out.video().unwrap().planes[0].offset, 4);
    }

    #[test]
    fn invalid_geometry_is_rejected() {
        let src = make_frame(4, 4, PixelFormat::Gray8, &[(4, 4, 4)]);
        let mut crop = Crop::with_options(Some(0), Some(0), Some(8), Some(4));
        assert!(crop.filter_frame(src.clone()).is_err());

        let mut crop = Crop::with_options(Some(0), Some(0), Some(0), Some(4));
        assert!(crop.filter_frame(src.clone()).is_err());

        let mut crop = Crop::with_options(Some(0), Some(0), Some(4), Some(4));
        assert!(
            crop.filter_frame(Frame::new_audio(
                0,
                wedeo_core::SampleFormat::S16,
                44100,
                wedeo_core::ChannelLayout::mono()
            ))
            .is_err()
        );
    }

    #[test]
    fn keep_aspect_updates_sample_aspect_ratio() {
        // sar 4/3, in 8x8 -> dar 4/3; crop 4x4 -> out sar 4/3 (unchanged here).
        let src = make_frame(8, 8, PixelFormat::Gray8, &[(8, 8, 8)]);
        let mut src = src;
        src.video_mut().unwrap().sample_aspect_ratio = Rational::new(4, 3);
        let out = crop_one(
            &Crop::with_options(Some(0), Some(0), Some(4), Some(4)).with_keep_aspect(true),
            src,
        );
        assert_eq!(
            out.video().unwrap().sample_aspect_ratio,
            Rational::new(4, 3)
        );

        // Non-square display: in 16x8, sar 1/1 -> dar 2/1; cropping to 8x8
        // doubles the sample aspect ratio to 2/1 to keep that DAR.
        let src = make_frame(16, 8, PixelFormat::Gray8, &[(16, 8, 16)]);
        let out = crop_one(
            &Crop::with_options(Some(0), Some(0), Some(8), Some(8)).with_keep_aspect(true),
            src,
        );
        assert_eq!(
            out.video().unwrap().sample_aspect_ratio,
            Rational::new(2, 1)
        );
    }

    #[test]
    fn query_formats_mirrors_ffmpeg_crop_pixdesc_filter() {
        let filter = Crop::new();
        let mut cfg = FormatsConfig::from_descriptor(filter.descriptor());
        filter.query_formats(&mut cfg).unwrap();

        let expected = FilterFormats::from_pixels(SUPPORTED_FORMATS.iter().copied());
        assert_eq!(cfg.inputs.len(), 1);
        assert_eq!(cfg.outputs.len(), 1);
        assert_eq!(cfg.inputs[0].formats, expected);
        assert_eq!(cfg.outputs[0].formats, expected);
        // Planar and packed 4:4:4 formats, plus NV12 (planar), are accepted.
        for format in [
            PixelFormat::Yuv420p,
            PixelFormat::Yuv422p,
            PixelFormat::Nv12,
            PixelFormat::Rgb24,
            PixelFormat::Pal8,
        ] {
            assert!(cfg.inputs[0].accepts(FilterFormat::Pixel(format)));
        }
        // AV_PIX_FMT_FLAG_BITSTREAM and FF_PIX_FMT_FLAG_SW_FLAT_SUB are
        // rejected by ff_formats_pixdesc_filter(0, reject_flags).
        for format in [
            PixelFormat::None,
            PixelFormat::MonoWhite,
            PixelFormat::MonoBlack,
            PixelFormat::Yuyv422,
            PixelFormat::Uyvy422,
        ] {
            assert!(!cfg.inputs[0].accepts(FilterFormat::Pixel(format)));
        }
    }

    #[test]
    fn factory_options_reach_the_crop_filter() {
        let filter = registry::create_filter_with_options(
            "crop",
            &[
                ("w", "4"),
                ("h", "2"),
                ("x", "2"),
                ("y", "1"),
                ("exact", "1"),
            ],
        )
        .unwrap();
        let mut filter = filter;
        filter.activate().unwrap();

        let src = make_frame(8, 8, PixelFormat::Gray8, &[(8, 8, 8)]);
        let out = filter.filter_frame(src.clone()).unwrap().remove(0);
        let video = out.video().unwrap();
        assert_eq!((video.width, video.height), (4, 2));
        assert_eq!(video.planes[0].offset, 8 + 2);
        assert_region(
            &src.video().unwrap().planes[0],
            &video.planes[0],
            1,
            2,
            2,
            4,
        );

        // `out_w`/`out_h` are aliases and the boolean options accept symbolic
        // values, as in FFmpeg's AVOption table.
        assert!(
            registry::create_filter_with_options(
                "crop",
                &[("out_w", "4"), ("out_h", "4"), ("keep_aspect", "true")]
            )
            .is_ok()
        );
        assert!(registry::create_filter_with_options("crop", &[("w", "iw/2")]).is_err());
        assert!(registry::create_filter_with_options("crop", &[("bogus", "1")]).is_err());
    }
}
