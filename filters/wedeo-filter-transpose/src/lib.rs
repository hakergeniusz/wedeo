#![forbid(unsafe_code)]
//! Transposition filter, ported from FFmpeg's `libavfilter/vf_transpose.c`.
//!
//! The filter rotates a video by 90 degrees and optionally flips the result,
//! using the four directions from FFmpeg's `enum TransposeDir`:
//! `cclock_flip` (0), `clock` (1), `cclock` (2) and `clock_flip` (3).
//!
//! Pixel handling mirrors FFmpeg's scalar `transpose_block_*_c` functions:
//! every plane is transposed independently, honouring its chroma subsampling
//! and its input/output linesizes.

use wedeo_core::buffer::Buffer;
use wedeo_core::error::{Error, Result};
use wedeo_core::frame::{Frame, FramePlane, VIDEO_MAX_PLANES};
use wedeo_core::media_type::MediaType;
use wedeo_core::pixel_format::PixelFormat;
pub use wedeo_filter::formats::{FilterFormat, FilterFormats, FormatsConfig, PadFormats};
use wedeo_filter::registry::FilterFactory;
pub use wedeo_filter::registry::{create_filter_with_options, find_filter_by_name};
use wedeo_filter::{
    Filter, FilterDescriptor, FilterFlags, FilterPadDescriptor, FilterPadDirection,
};

/// FFmpeg `enum TransposeDir` values supported by this filter.
///
/// The deprecated values 4..7 (`TRANSPOSE_REVERSAL`, `TRANSPOSE_HFLIP`,
/// `TRANSPOSE_VFLIP` and the "dir & 4" legacy alias) are not part of the
/// public option surface here, matching the four symbolic option values.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[repr(i32)]
pub enum TransposeDirection {
    /// `cclock_flip`: rotate counter-clockwise with vertical flip (dir 0).
    /// FFmpeg's default `dir` value is `TRANSPOSE_CCLOCK_FLIP`.
    #[default]
    CClockFlip = 0,
    /// `clock`: rotate clockwise (dir 1).
    Clock = 1,
    /// `cclock`: rotate counter-clockwise (dir 2).
    CClock = 2,
    /// `clock_flip`: rotate clockwise with vertical flip (dir 3).
    ClockFlip = 3,
}

impl TransposeDirection {
    /// Map an integer `dir` option value to its direction.
    pub fn from_int(value: i32) -> Result<Self> {
        match value {
            0 => Ok(Self::CClockFlip),
            1 => Ok(Self::Clock),
            2 => Ok(Self::CClock),
            3 => Ok(Self::ClockFlip),
            _ => Err(Error::InvalidArgument),
        }
    }

    /// Return the FFmpeg `dir` integer value for this direction.
    pub const fn as_int(self) -> i32 {
        self as i32
    }

    /// True when FFmpeg would inspect `dir & 1` for this direction.
    ///
    /// This selects a vertical flip of the source before transposition.
    const fn flip_source(self) -> bool {
        matches!(self, Self::Clock | Self::ClockFlip)
    }

    /// True when FFmpeg would inspect `dir & 2` for this direction.
    ///
    /// This selects a vertical flip of the destination after transposition.
    const fn flip_destination(self) -> bool {
        matches!(self, Self::CClock | Self::ClockFlip)
    }
}

/// Plane layout used by FFmpeg's `s->hsub`, `s->vsub` and `s->pixsteps[]`.
///
/// FFmpeg's `query_formats()` only accepts formats whose `log2_chroma_w` and
/// `log2_chroma_h` are equal, so a single pair of chroma shifts is sufficient.
struct PlaneLayout {
    hsub: u32,
    vsub: u32,
    pixsteps: [usize; VIDEO_MAX_PLANES],
    planes: usize,
}

/// Equivalent of FFmpeg's `av_pix_fmt_desc_get()` + `av_image_fill_max_pixsteps()`
/// for the pixel formats exposed by `wedeo-core`.
fn plane_layout(format: PixelFormat) -> Result<PlaneLayout> {
    let (hsub, vsub, pixsteps, planes) = match format {
        PixelFormat::Gray8 => (0, 0, [1, 0, 0, 0], 1),
        PixelFormat::Gray16be | PixelFormat::Gray16le => (0, 0, [2, 0, 0, 0], 1),
        PixelFormat::Rgb24 | PixelFormat::Bgr24 => (0, 0, [3, 0, 0, 0], 1),
        PixelFormat::Argb | PixelFormat::Rgba | PixelFormat::Abgr | PixelFormat::Bgra => {
            (0, 0, [4, 0, 0, 0], 1)
        }
        PixelFormat::Rgb48be | PixelFormat::Rgb48le => (0, 0, [6, 0, 0, 0], 1),
        PixelFormat::Yuv420p | PixelFormat::Yuvj420p => (1, 1, [1, 1, 1, 0], 3),
        PixelFormat::Yuv444p | PixelFormat::Yuvj444p => (0, 0, [1, 1, 1, 0], 3),
        PixelFormat::Yuv410p => (2, 2, [1, 1, 1, 0], 3),
        PixelFormat::Yuv420p10le | PixelFormat::Yuv420p10be => (1, 1, [2, 2, 2, 0], 3),
        PixelFormat::Yuv420p16le | PixelFormat::Yuv420p16be => (1, 1, [2, 2, 2, 0], 3),
        // 10-bit 4:2:2/4:4:4 (H.264 FRext).
        PixelFormat::Yuv422p10le | PixelFormat::Yuv422p10be => (1, 0, [2, 2, 2, 0], 3),
        PixelFormat::Yuv444p10le | PixelFormat::Yuv444p10be => (0, 0, [2, 2, 2, 0], 3),
        _ => return Err(Error::InvalidArgument),
    };

    Ok(PlaneLayout {
        hsub,
        vsub,
        pixsteps,
        planes,
    })
}

/// Formats advertised by `query_formats()`.
///
/// FFmpeg:
/// ```c
/// for (fmt = 0; desc = av_pix_fmt_desc_get(fmt); fmt++) {
///     if (!(desc->flags & AV_PIX_FMT_FLAG_PAL ||
///           desc->flags & AV_PIX_FMT_FLAG_HWACCEL ||
///           desc->flags & AV_PIX_FMT_FLAG_BITSTREAM ||
///           desc->log2_chroma_w != desc->log2_chroma_h) &&
///         (ret = ff_add_format(&pix_fmts, fmt)) < 0)
///         return ret;
/// }
/// return ff_set_common_formats2(ctx, cfg_in, cfg_out, pix_fmts);
/// ```
///
/// This is the same set for which the local `plane_layout()` above returns a
/// layout (paletted, bitstream and non-planar 4:2:2 formats are excluded).
const SUPPORTED_FORMATS: &[PixelFormat] = &[
    PixelFormat::Gray8,
    PixelFormat::Gray16be,
    PixelFormat::Gray16le,
    PixelFormat::Rgb24,
    PixelFormat::Bgr24,
    PixelFormat::Argb,
    PixelFormat::Rgba,
    PixelFormat::Abgr,
    PixelFormat::Bgra,
    PixelFormat::Rgb48be,
    PixelFormat::Rgb48le,
    PixelFormat::Yuv420p,
    PixelFormat::Yuvj420p,
    PixelFormat::Yuv444p,
    PixelFormat::Yuvj444p,
    PixelFormat::Yuv410p,
    PixelFormat::Yuv420p10le,
    PixelFormat::Yuv420p10be,
    PixelFormat::Yuv420p16le,
    PixelFormat::Yuv420p16be,
    PixelFormat::Yuv422p10le,
    PixelFormat::Yuv422p10be,
    PixelFormat::Yuv444p10le,
    PixelFormat::Yuv444p10be,
];

/// `AV_CEIL_RSHIFT(a, b)`
const fn ceil_shift(a: u32, b: u32) -> u32 {
    a.div_ceil(1u32 << b)
}

/// Round `value` up to a multiple of `align` (a power of two).
const fn align_up(value: usize, align: usize) -> usize {
    value.div_ceil(align) * align
}

/// Transpose one plane, scalar C fallback equivalent of FFmpeg's
/// `transpose_block_*_c()` functions followed by the `dir & 1` / `dir & 2`
/// pointer arithmetic from `filter_slice()`.
///
/// `src_plane_h` is FFmpeg's `inh` and `dst_plane_h` is FFmpeg's `outh`.
// The parameter list deliberately mirrors FFmpeg's transpose_block_*_c()
// signature (src/src_linesize, dst/dst_linesize, w/h, pixstep, dir); splitting
// it into a struct would obscure the 1:1 mapping to the C source.
#[allow(clippy::too_many_arguments)]
fn transpose_plane(
    src: &[u8],
    src_linesize: usize,
    src_plane_h: usize,
    dst: &mut [u8],
    dst_linesize: usize,
    dst_plane_w: usize,
    dst_plane_h: usize,
    pixstep: usize,
    dir: TransposeDirection,
) -> Result<()> {
    for row in 0..dst_plane_h {
        for col in 0..dst_plane_w {
            // FFmpeg filter_slice(): when dir & 1 the source base is moved to
            // the last row and srclinesize is negated; when dir & 2 the
            // destination base is moved to the last row and dstlinesize is
            // negated. Express that directly as source coordinates.
            let src_row = if dir.flip_source() {
                src_plane_h.saturating_sub(1 + col)
            } else {
                col
            };
            let src_col = if dir.flip_destination() {
                dst_plane_h.saturating_sub(1 + row)
            } else {
                row
            };

            let src_off = src_row
                .checked_mul(src_linesize)
                .and_then(|v| v.checked_add(src_col.checked_mul(pixstep)?))
                .ok_or(Error::Bug)?;
            let dst_off = row
                .checked_mul(dst_linesize)
                .and_then(|v| v.checked_add(col.checked_mul(pixstep)?))
                .ok_or(Error::Bug)?;
            let src_end = src_off.checked_add(pixstep).ok_or(Error::Bug)?;
            let dst_end = dst_off.checked_add(pixstep).ok_or(Error::Bug)?;

            let s = src.get(src_off..src_end).ok_or(Error::Bug)?;
            let d = dst.get_mut(dst_off..dst_end).ok_or(Error::Bug)?;
            d.copy_from_slice(s);
        }
    }
    Ok(())
}

/// The transpose filter instance.
#[derive(Debug, Clone, Copy)]
pub struct TransposeFilter {
    dir: TransposeDirection,
}

impl TransposeFilter {
    /// Create a filter with the given FFmpeg `dir` value.
    pub fn new(dir: TransposeDirection) -> Self {
        Self { dir }
    }
}

impl Default for TransposeFilter {
    fn default() -> Self {
        Self::new(TransposeDirection::default())
    }
}

static INPUT_PADS: [FilterPadDescriptor; 1] = [FilterPadDescriptor {
    name: "default",
    media_type: MediaType::Video,
    direction: FilterPadDirection::Input,
}];

static OUTPUT_PADS: [FilterPadDescriptor; 1] = [FilterPadDescriptor {
    name: "default",
    media_type: MediaType::Video,
    direction: FilterPadDirection::Output,
}];

/// FFmpeg: `.p.name = "transpose"`, `.p.description = "Transpose input video."`,
/// `.p.flags = AVFILTER_FLAG_SLICE_THREADS`.
pub static TRANSPOSE_DESCRIPTOR: FilterDescriptor = FilterDescriptor {
    name: "transpose",
    description: "Transpose input video.",
    inputs: &INPUT_PADS,
    outputs: &OUTPUT_PADS,
    flags: FilterFlags::SLICE_THREADS,
};

impl Filter for TransposeFilter {
    fn descriptor(&self) -> &FilterDescriptor {
        &TRANSPOSE_DESCRIPTOR
    }

    fn activate(&mut self) -> Result<()> {
        // The C filter configures its pixel steps and output dimensions in
        // config_props_output(). Here those values are derived from each frame
        // in filter_frame(), so there is no persistent state to prepare.
        Ok(())
    }

    fn query_formats(&self, cfg: &mut FormatsConfig) -> Result<()> {
        // FFmpeg: ff_set_common_formats2() with every non-palette, non-bitstream
        // format whose chroma shifts match. Both pads get the same list.
        cfg.set_common_pixel_formats(SUPPORTED_FORMATS);
        Ok(())
    }

    fn filter_frame(&mut self, frame: Frame) -> Result<Vec<Frame>> {
        let (in_w, in_h, format, in_planes) = {
            let video = frame.video().ok_or(Error::InvalidArgument)?;
            (
                video.width,
                video.height,
                video.format,
                video.planes.clone(),
            )
        };

        let layout = plane_layout(format)?;
        if in_planes.len() != layout.planes {
            return Err(Error::InvalidArgument);
        }

        // FFmpeg config_props_output(): outlink->w = inlink->h,
        // outlink->h = inlink->w.
        let out_w = in_h;
        let out_h = in_w;

        let mut out_planes = Vec::with_capacity(layout.planes);
        for (plane_index, src_plane) in in_planes.iter().enumerate() {
            let pixstep = layout.pixsteps[plane_index];
            let (hsub, vsub) = if plane_index == 1 || plane_index == 2 {
                (layout.hsub, layout.vsub)
            } else {
                (0, 0)
            };

            let src_plane_w = ceil_shift(in_w, hsub) as usize;
            let src_plane_h = ceil_shift(in_h, vsub) as usize;
            let dst_plane_w = ceil_shift(out_w, hsub) as usize;
            let dst_plane_h = ceil_shift(out_h, vsub) as usize;

            let src_linesize = src_plane.linesize;
            let dst_linesize = align_up(dst_plane_w * pixstep, 64);

            let src_start = src_plane.offset;
            let src_end = src_start
                .checked_add(
                    src_linesize
                        .checked_mul(src_plane_h.saturating_sub(1))
                        .and_then(|v| v.checked_add(src_plane_w.checked_mul(pixstep)?))
                        .ok_or(Error::Bug)?,
                )
                .ok_or(Error::Bug)?;
            let src = src_plane
                .buffer
                .data()
                .get(src_start..src_end)
                .ok_or(Error::InvalidData)?;

            let mut dst_buffer = Buffer::new(dst_linesize * dst_plane_h);
            {
                let dst = dst_buffer.data_mut();
                transpose_plane(
                    src,
                    src_linesize,
                    src_plane_h,
                    dst,
                    dst_linesize,
                    dst_plane_w,
                    dst_plane_h,
                    pixstep,
                    self.dir,
                )?;
            }

            out_planes.push(FramePlane {
                buffer: dst_buffer,
                offset: 0,
                linesize: dst_linesize,
            });
        }

        // FFmpeg: av_frame_copy_props(out, in) followed by the sample aspect
        // ratio inversion from config_props_output() / filter_frame().
        let mut out = frame;
        let video = out.video_mut().ok_or(Error::Bug)?;
        video.width = out_w;
        video.height = out_h;
        video.planes = out_planes;
        if video.sample_aspect_ratio.num != 0 {
            video.sample_aspect_ratio = video.sample_aspect_ratio.invert();
        }
        Ok(vec![out])
    }
}

/// Parse FFmpeg's `dir` AVOption: the four symbolic constants or their
/// integer values. The deprecated values 4..7 are deliberately rejected, as
/// in [`TransposeDirection::from_int`].
fn parse_dir_option(value: &str) -> Result<TransposeDirection> {
    let dir = match value {
        "cclock_flip" => TransposeDirection::CClockFlip,
        "clock" => TransposeDirection::Clock,
        "cclock" => TransposeDirection::CClock,
        "clock_flip" => TransposeDirection::ClockFlip,
        _ => {
            TransposeDirection::from_int(value.parse::<i32>().map_err(|_| Error::InvalidArgument)?)?
        }
    };
    Ok(dir)
}

/// FFmpeg: `AVFILTER_DEFINE_CLASS(transpose)` + `ff_vf_transpose` registration.
pub struct TransposeFactory;

impl FilterFactory for TransposeFactory {
    fn descriptor(&self) -> &FilterDescriptor {
        &TRANSPOSE_DESCRIPTOR
    }

    fn create(&self) -> Result<Box<dyn Filter>> {
        Ok(Box::new(TransposeFilter::default()))
    }

    // FFmpeg applies private options with av_opt_set() before graph
    // configuration. Only `dir` is implemented; `passthrough` is reported as
    // an interface gap in the final deliverable.
    fn create_with_options(&self, options: &[(&str, &str)]) -> Result<Box<dyn Filter>> {
        let mut dir = TransposeDirection::default();
        for &(key, value) in options {
            match key {
                "dir" => dir = parse_dir_option(value)?,
                // av_opt_set() rejects unknown option names.
                _ => return Err(Error::InvalidArgument),
            }
        }
        Ok(Box::new(TransposeFilter::new(dir)))
    }
}

inventory::submit!(&TransposeFactory as &dyn FilterFactory);

/// Convenience for checking inventory registration from integration tests.
pub fn is_registered() -> bool {
    wedeo_filter::registry::find_filter_by_name("transpose").is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use wedeo_core::frame::{FrameData, PictureType};
    use wedeo_core::metadata::Metadata;
    use wedeo_core::rational::Rational;

    /// Build a one-plane 8-bit frame with an explicit input linesize.
    fn gray_frame(width: u32, height: u32, linesize: usize, pixels: &[&[u8]]) -> Frame {
        let mut buffer = Buffer::new(linesize * height as usize);
        {
            let data = buffer.data_mut();
            for (row, src) in pixels.iter().enumerate() {
                data[row * linesize..row * linesize + src.len()].copy_from_slice(src);
            }
        }
        let mut frame = Frame::new_video(width, height, PixelFormat::Gray8);
        match &mut frame.data {
            FrameData::Video(video) => {
                video.planes = vec![FramePlane {
                    buffer,
                    offset: 0,
                    linesize,
                }];
                video.picture_type = PictureType::I;
            }
            FrameData::Audio(_) => unreachable!(),
        }
        frame
    }

    /// Read a single 8-bit pixel from a plane.
    fn pixel(frame: &Frame, plane: usize, col: usize, row: usize) -> u8 {
        let video = match &frame.data {
            FrameData::Video(v) => v,
            FrameData::Audio(_) => unreachable!(),
        };
        let p = &video.planes[plane];
        p.buffer.data()[p.offset + row * p.linesize + col]
    }

    fn plane_dims(frame: &Frame) -> (u32, u32) {
        match &frame.data {
            FrameData::Video(v) => (v.width, v.height),
            FrameData::Audio(_) => unreachable!(),
        }
    }

    #[test]
    fn direction_enum_matches_ffmpeg_dir_values() {
        assert_eq!(TransposeDirection::default().as_int(), 0);
        for value in 0..=3 {
            let dir = TransposeDirection::from_int(value).unwrap();
            assert_eq!(dir.as_int(), value);
        }
        assert!(TransposeDirection::from_int(4).is_err());
    }

    #[test]
    fn descriptor_is_transpose() {
        assert_eq!(TRANSPOSE_DESCRIPTOR.name, "transpose");
        assert_eq!(TRANSPOSE_DESCRIPTOR.description, "Transpose input video.");
        assert_eq!(TRANSPOSE_DESCRIPTOR.inputs.len(), 1);
        assert_eq!(TRANSPOSE_DESCRIPTOR.outputs.len(), 1);
        assert!(
            TRANSPOSE_DESCRIPTOR
                .flags
                .contains(FilterFlags::SLICE_THREADS)
        );
    }

    /// 2x2 sample from the module reasoning:
    /// A B
    /// C D
    #[test]
    fn all_directions_2x2() {
        let expected: &[(TransposeDirection, [[u8; 2]; 2])] = &[
            (TransposeDirection::CClockFlip, [*b"AC", *b"BD"]),
            (TransposeDirection::Clock, [*b"CA", *b"DB"]),
            (TransposeDirection::CClock, [*b"BD", *b"AC"]),
            (TransposeDirection::ClockFlip, [*b"DB", *b"CA"]),
        ];

        for (dir, want) in expected {
            let input = gray_frame(2, 2, 2, &[b"AB", b"CD"]);
            let mut filter = TransposeFilter::new(*dir);
            filter.activate().unwrap();
            let output = filter.filter_frame(input).unwrap().remove(0);
            assert_eq!(plane_dims(&output), (2, 2), "dir {}", dir.as_int());
            for row in 0..2 {
                for col in 0..2 {
                    assert_eq!(
                        pixel(&output, 0, col, row),
                        want[row][col],
                        "dir {} at ({}, {})",
                        dir.as_int(),
                        col,
                        row
                    );
                }
            }
        }
    }

    /// Non-square frame with padded input linesize and a 3-plane yuv420p layout.
    #[test]
    fn yuv420p_subsampling_and_linesize() {
        // 4x2 luma; chroma planes are 2x1.
        let y = [vec![1u8, 2, 3, 4], vec![5, 6, 7, 8]];
        let u = [vec![10u8, 20]];
        let v = [vec![30u8, 40]];

        let mut planes = Vec::new();
        for rows in [y.as_slice(), u.as_slice(), v.as_slice()] {
            let linesize = 16usize; // deliberately larger than the visible width
            let mut buffer = Buffer::new(linesize * rows.len());
            {
                let data = buffer.data_mut();
                for (row, src) in rows.iter().enumerate() {
                    data[row * linesize..row * linesize + src.len()].copy_from_slice(src);
                }
            }
            planes.push(FramePlane {
                buffer,
                offset: 0,
                linesize,
            });
        }

        let mut frame = Frame::new_video(4, 2, PixelFormat::Yuv420p);
        match &mut frame.data {
            FrameData::Video(video) => video.planes = planes,
            FrameData::Audio(_) => unreachable!(),
        }

        let mut filter = TransposeFilter::new(TransposeDirection::CClock);
        let output = filter.filter_frame(frame).unwrap().remove(0);
        assert_eq!(plane_dims(&output), (2, 4));

        let video = match &output.data {
            FrameData::Video(v) => v,
            FrameData::Audio(_) => unreachable!(),
        };
        assert_eq!(video.planes.len(), 3);
        // dir=2 (cclock): out(row, col) = in(row=col, col=outh-1-row).
        // outh = 4 for luma, outh = 2 for chroma.
        let want = |out_row: usize, out_col: usize, in_h: usize| -> u8 {
            let in_row = out_col;
            let in_col = in_h - 1 - out_row;
            match in_row {
                0 => y[0][in_col],
                _ => y[1][in_col],
            }
        };
        for out_row in 0..4 {
            for out_col in 0..2 {
                assert_eq!(
                    pixel(&output, 0, out_col, out_row),
                    want(out_row, out_col, 4),
                    "luma ({}, {})",
                    out_col,
                    out_row
                );
            }
        }
        // Chroma planes: out is 1x2 (width 1, height 2).
        for out_row in 0..2 {
            assert_eq!(pixel(&output, 1, 0, out_row), u[0][1 - out_row]);
            assert_eq!(pixel(&output, 2, 0, out_row), v[0][1 - out_row]);
        }
    }

    #[test]
    fn metadata_and_sar_preserved() {
        let mut frame = gray_frame(2, 3, 4, &[&[1, 2], &[3, 4], &[5, 6]]);
        frame.pts = 42;
        frame.duration = 7;
        frame.metadata = Metadata::new();
        frame.metadata.set("key", "value");
        match &mut frame.data {
            FrameData::Video(video) => {
                video.sample_aspect_ratio = Rational::new(4, 3);
                video.color_range = wedeo_core::frame::ColorRange::Jpeg;
            }
            FrameData::Audio(_) => unreachable!(),
        }

        let mut filter = TransposeFilter::new(TransposeDirection::Clock);
        let output = filter.filter_frame(frame).unwrap().remove(0);
        assert_eq!(output.pts, 42);
        assert_eq!(output.duration, 7);
        assert_eq!(output.metadata.get("key"), Some("value"));
        let video = match &output.data {
            FrameData::Video(v) => v,
            FrameData::Audio(_) => unreachable!(),
        };
        assert_eq!(video.sample_aspect_ratio.num, 3);
        assert_eq!(video.sample_aspect_ratio.den, 4);
        assert_eq!(video.color_range, wedeo_core::frame::ColorRange::Jpeg);
        assert_eq!((video.width, video.height), (3, 2));
    }

    #[test]
    fn factory_and_inventory_registration() {
        assert!(is_registered());
        let factory = TransposeFactory;
        assert_eq!(factory.descriptor().name, "transpose");
        let filter = factory.create().unwrap();
        assert_eq!(filter.descriptor().name, "transpose");
    }

    #[test]
    fn query_formats_advertises_plane_layout_formats() {
        let filter = TransposeFilter::default();
        let mut cfg = FormatsConfig::from_descriptor(filter.descriptor());
        filter.query_formats(&mut cfg).unwrap();

        let expected = FilterFormats::from_pixels(SUPPORTED_FORMATS.iter().copied());
        assert_eq!(cfg.inputs[0].formats, expected);
        assert_eq!(cfg.outputs[0].formats, expected);
        for &format in SUPPORTED_FORMATS {
            assert!(plane_layout(format).is_ok(), "{format:?} lacks a layout");
        }
        // FFmpeg rejects palette, bitstream and packed 4:2:2 formats.
        for format in [
            PixelFormat::None,
            PixelFormat::Pal8,
            PixelFormat::MonoWhite,
            PixelFormat::Yuyv422,
            PixelFormat::Uyvy422,
            PixelFormat::Yuv422p,
        ] {
            assert!(!cfg.outputs[0].accepts(FilterFormat::Pixel(format)));
        }
    }

    #[test]
    fn factory_options_reach_the_transpose_filter() {
        let factory = TransposeFactory;
        // Symbolic and numeric `dir` values select the same rotation.
        for &(name, number) in &[
            ("cclock_flip", "0"),
            ("clock", "1"),
            ("cclock", "2"),
            ("clock_flip", "3"),
        ] {
            let mut named = factory.create_with_options(&[("dir", name)]).unwrap();
            let mut numeric = factory.create_with_options(&[("dir", number)]).unwrap();
            let a = named
                .filter_frame(gray_frame(2, 2, 2, &[b"AB", b"CD"]))
                .unwrap()
                .remove(0);
            let b = numeric
                .filter_frame(gray_frame(2, 2, 2, &[b"AB", b"CD"]))
                .unwrap()
                .remove(0);
            for row in 0..2 {
                for col in 0..2 {
                    assert_eq!(pixel(&a, 0, col, row), pixel(&b, 0, col, row));
                }
            }
        }
        assert!(factory.create_with_options(&[("dir", "4")]).is_err());
        assert!(factory.create_with_options(&[("dir", "sideways")]).is_err());
        assert!(factory.create_with_options(&[("bogus", "1")]).is_err());
    }
}
