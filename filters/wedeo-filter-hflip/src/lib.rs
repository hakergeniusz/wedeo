#![forbid(unsafe_code)]
//! Work unit: `wedeo-filter-hflip`.
//!
//! FFmpeg reference: `libavfilter/vf_hflip.c` (plus `vf_hflip_init.h` for the
//! C `flip_line` helpers and `libavutil/imgutils.c` for `max_step`).
//!
//! A pure-Rust port of FFmpeg's horizontal flip video filter. It allocates a
//! new output frame (copying all frame properties), then reverses every
//! present video plane row by row. Each row is reversed in units of the
//! plane's maximum pixel step in bytes, so multi-byte samples keep their
//! internal byte order while their order within the row is reversed.

use wedeo_core::error::{Error, Result};
use wedeo_core::frame::{Frame, VIDEO_MAX_PLANES};
use wedeo_core::media_type::MediaType;
use wedeo_core::pixel_format::PixelFormat;
use wedeo_filter::registry::FilterFactory;
use wedeo_filter::{
    Filter, FilterDescriptor, FilterFlags, FilterPadDescriptor, FilterPadDirection,
};

// Re-exported so the FATE harness can reach the registry and traits without
// depending on `wedeo-filter` directly (only the filter crates are linked in).
pub use wedeo_filter::Filter as RegisteredFilter;
pub use wedeo_filter::formats::{FilterFormat, FilterFormats, FormatsConfig, PadFormats};
pub use wedeo_filter::registry;
pub use wedeo_filter::registry::FilterFactory as RegisteredFilterFactory;

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

static HFLIP_DESCRIPTOR: FilterDescriptor = FilterDescriptor {
    name: "hflip",
    description: "Horizontally flip the input video.",
    inputs: INPUT_PADS,
    outputs: OUTPUT_PADS,
    flags: FilterFlags::SLICE_THREADS.union(FilterFlags::SUPPORT_TIMELINE_GENERIC),
};

/// `max_step` is `av_image_fill_max_pixsteps`: the maximum `comp.step`
/// (in bytes) of any component living on a plane. `bayer_plus1` is 2 for
/// Bayer formats and 1 otherwise; no wedeo pixel format is Bayer today, but
/// the field is kept for parity with FFmpeg's `FlipContext`.
#[derive(Debug, Clone, Copy)]
struct FormatInfo {
    max_step: [usize; VIDEO_MAX_PLANES],
    log2_chroma_w: u32,
    log2_chroma_h: u32,
    bayer_plus1: usize,
}

impl FormatInfo {
    const fn new(
        max_step: [usize; VIDEO_MAX_PLANES],
        log2_chroma_w: u32,
        log2_chroma_h: u32,
    ) -> Self {
        Self {
            max_step,
            log2_chroma_w,
            log2_chroma_h,
            bayer_plus1: 1,
        }
    }

    const fn none() -> Self {
        Self::new([0; VIDEO_MAX_PLANES], 0, 0)
    }
}

/// Pixel-format geometry needed by the flip, mirroring FFmpeg's
/// `config_props()` (see `libavutil/pixdesc.c` for the step values).
fn format_info(format: PixelFormat) -> FormatInfo {
    match format {
        // Planar 4:2:0, 8-bit.
        PixelFormat::Yuv420p | PixelFormat::Yuvj420p => FormatInfo::new([1, 1, 1, 0], 1, 1),
        // Planar 4:2:2, 8-bit.
        PixelFormat::Yuv422p | PixelFormat::Yuvj422p => FormatInfo::new([1, 1, 1, 0], 1, 0),
        // Planar 4:4:4, 8-bit.
        PixelFormat::Yuv444p | PixelFormat::Yuvj444p => FormatInfo::new([1, 1, 1, 0], 0, 0),
        // Planar 4:1:0 and 4:1:1, 8-bit.
        PixelFormat::Yuv410p => FormatInfo::new([1, 1, 1, 0], 2, 2),
        PixelFormat::Yuv411p => FormatInfo::new([1, 1, 1, 0], 2, 0),
        // 8-bit single plane / palette.
        PixelFormat::Gray8 | PixelFormat::Pal8 => FormatInfo::new([1, 0, 0, 0], 0, 0),
        // 1-bit bitstream formats (rejected by FFmpeg's query_formats, but
        // harmless to flip byte-wise here).
        PixelFormat::MonoWhite | PixelFormat::MonoBlack => FormatInfo::new([1, 0, 0, 0], 0, 0),
        // Semi-planar 4:2:0, 8-bit (UV are interleaved => step 2 on plane 1).
        PixelFormat::Nv12 | PixelFormat::Nv21 => FormatInfo::new([1, 2, 0, 0], 1, 1),
        // Packed 24-bit RGB (step 3).
        PixelFormat::Rgb24 | PixelFormat::Bgr24 => FormatInfo::new([3, 0, 0, 0], 0, 0),
        // Packed 32-bit RGB (step 4).
        PixelFormat::Argb | PixelFormat::Rgba | PixelFormat::Abgr | PixelFormat::Bgra => {
            FormatInfo::new([4, 0, 0, 0], 0, 0)
        }
        // Packed 48-bit RGB (step 6).
        PixelFormat::Rgb48be | PixelFormat::Rgb48le => FormatInfo::new([6, 0, 0, 0], 0, 0),
        // 16-bit single plane (step 2); endianness is irrelevant because the
        // flip copies each sample's bytes verbatim.
        PixelFormat::Gray16be | PixelFormat::Gray16le => FormatInfo::new([2, 0, 0, 0], 0, 0),
        // Planar 4:2:0, 10-bit stored in 16 bits (step 2).
        PixelFormat::Yuv420p10le | PixelFormat::Yuv420p10be => FormatInfo::new([2, 2, 2, 0], 1, 1),
        // Planar 4:2:2 / 4:4:4, 10-bit (H.264 FRext).
        PixelFormat::Yuv422p10le | PixelFormat::Yuv422p10be => FormatInfo::new([2, 2, 2, 0], 1, 0),
        PixelFormat::Yuv444p10le | PixelFormat::Yuv444p10be => FormatInfo::new([2, 2, 2, 0], 0, 0),
        // Planar 12-bit variants (VP9 profiles 2/3), stored in 16 bits.
        PixelFormat::Yuv420p12le | PixelFormat::Yuv420p12be => FormatInfo::new([2, 2, 2, 0], 1, 1),
        PixelFormat::Yuv422p12le | PixelFormat::Yuv422p12be => FormatInfo::new([2, 2, 2, 0], 1, 0),
        PixelFormat::Yuv444p12le | PixelFormat::Yuv444p12be => FormatInfo::new([2, 2, 2, 0], 0, 0),
        // Planar 4:2:0, 16-bit (step 2).
        PixelFormat::Yuv420p16le | PixelFormat::Yuv420p16be => FormatInfo::new([2, 2, 2, 0], 1, 1),
        // Packed 4:2:2 8-bit formats are rejected by FFmpeg's query_formats
        // (log2_chroma_w != log2_chroma_h with all components on plane 0).
        // Keep their geometry here anyway for callers that ignore negotiation.
        PixelFormat::Yuyv422 | PixelFormat::Uyvy422 => FormatInfo::new([4, 0, 0, 0], 1, 0),
        PixelFormat::None => FormatInfo::none(),
    }
}

/// Formats advertised by `query_formats()`.
///
/// FFmpeg:
/// ```c
/// for (fmt = 0; desc = av_pix_fmt_desc_get(fmt); fmt++) {
///     if (!(desc->flags & AV_PIX_FMT_FLAG_HWACCEL ||
///           desc->flags & AV_PIX_FMT_FLAG_BITSTREAM ||
///           (desc->log2_chroma_w != desc->log2_chroma_h &&
///            desc->comp[0].plane == desc->comp[1].plane)) &&
///         (ret = ff_add_format(&pix_fmts, fmt)) < 0)
///         return ret;
/// }
/// return ff_set_common_formats2(ctx, cfg_in, cfg_out, pix_fmts);
/// ```
///
/// The last condition rejects packed 4:2:2 formats (`yuyv422`, `uyvy422`),
/// whose two chroma components live on plane 0. Bitstream formats are
/// rejected too; Wedeo has no hardware-accelerated pixel format.
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
    PixelFormat::Yuv420p10le,
    PixelFormat::Yuv420p10be,
    PixelFormat::Rgb48be,
    PixelFormat::Rgb48le,
    PixelFormat::Yuv420p16le,
    PixelFormat::Yuv420p16be,
    PixelFormat::Yuv422p10le,
    PixelFormat::Yuv422p10be,
    PixelFormat::Yuv444p10le,
    PixelFormat::Yuv444p10be,
];

/// `AV_CEIL_RSHIFT(a, b)`: ceil(a / 2^b) for non-negative a and small b.
fn ceil_rshift(value: usize, shift: u32) -> usize {
    (value + ((1usize << shift) - 1)) >> shift
}

/// Reverse one plane, row by row, in units of `step` bytes.
///
/// This is the C `hflip_*_c` family from `vf_hflip_init.h` written once:
/// `src` points at the last sample of the row, `dst` at the first, and
/// `dst[j] = src[-j]` for `j` in `0..width`.
fn flip_plane(
    src: &[u8],
    dst: &mut [u8],
    width: usize,
    height: usize,
    src_linesize: usize,
    dst_linesize: usize,
    step: usize,
) -> Result<()> {
    if step == 0 || width == 0 || height == 0 || src_linesize == 0 || dst_linesize == 0 {
        return Ok(());
    }

    let row_bytes = width.checked_mul(step).ok_or(Error::InvalidData)?;
    for row in 0..height {
        let src_row = row.checked_mul(src_linesize).ok_or(Error::InvalidData)?;
        let dst_row = row.checked_mul(dst_linesize).ok_or(Error::InvalidData)?;
        if src_row
            .checked_add(row_bytes)
            .is_none_or(|end| end > src.len())
            || dst_row
                .checked_add(row_bytes)
                .is_none_or(|end| end > dst.len())
        {
            return Err(Error::InvalidData);
        }

        // inrow = in->data[plane] + start * linesize + (width - 1) * step;
        let src_base = src_row + row_bytes - step;
        for j in 0..width {
            let s = src_base - j * step;
            let d = dst_row + j * step;
            dst[d..d + step].copy_from_slice(&src[s..s + step]);
        }
    }

    Ok(())
}

/// The FFmpeg `hflip` video filter, ported from `vf_hflip.c`.
pub struct HflipFilter;

impl HflipFilter {
    /// Create a new hflip filter instance.
    pub fn new() -> Self {
        Self
    }
}

impl Default for HflipFilter {
    fn default() -> Self {
        Self::new()
    }
}

impl Filter for HflipFilter {
    fn descriptor(&self) -> &FilterDescriptor {
        &HFLIP_DESCRIPTOR
    }

    fn activate(&mut self) -> Result<()> {
        Ok(())
    }

    fn query_formats(&self, cfg: &mut FormatsConfig) -> Result<()> {
        // FFmpeg: ff_set_common_formats2() applies the same list to both pads;
        // this filter never converts pixels, it only reorders them.
        cfg.set_common_pixel_formats(SUPPORTED_FORMATS);
        Ok(())
    }

    fn filter_frame(&mut self, frame: Frame) -> Result<Vec<Frame>> {
        let video = frame.video().ok_or(Error::InvalidArgument)?;
        let info = format_info(video.format);
        let width = video.width as usize;
        let height = video.height as usize;

        // ff_get_video_buffer() + av_frame_copy_props(): cloning the Frame
        // carries pts, metadata and side data across unchanged.
        let mut out = frame.clone();

        for plane in 0..VIDEO_MAX_PLANES {
            let step = info.max_step[plane];
            if step == 0
                || plane >= video.planes.len()
                || plane >= out.video().unwrap().planes.len()
            {
                continue;
            }

            let src_plane = &video.planes[plane];
            let src_linesize = src_plane.linesize;
            if src_linesize == 0 {
                continue;
            }

            let (plane_width, plane_height) = if plane == 1 || plane == 2 {
                (
                    ceil_rshift(width, info.log2_chroma_w),
                    ceil_rshift(height, info.log2_chroma_h),
                )
            } else {
                (width, height)
            };
            let flip_width = plane_width / info.bayer_plus1;

            let offset = src_plane.offset;
            let out_video = out.video_mut().ok_or(Error::Bug)?;
            let dst_plane = &mut out_video.planes[plane];
            let dst_linesize = dst_plane.linesize;
            let src = src_plane.buffer.data();
            let src = src.get(offset..).ok_or(Error::InvalidData)?;
            dst_plane.buffer.make_writable();
            let dst = dst_plane.buffer.data_mut();
            let dst = dst.get_mut(offset..).ok_or(Error::InvalidData)?;
            flip_plane(
                src,
                dst,
                flip_width,
                plane_height,
                src_linesize,
                dst_linesize,
                step,
            )?;
        }

        Ok(vec![out])
    }
}

struct HflipFilterFactory;

impl FilterFactory for HflipFilterFactory {
    fn descriptor(&self) -> &FilterDescriptor {
        &HFLIP_DESCRIPTOR
    }

    fn create(&self) -> Result<Box<dyn Filter>> {
        Ok(Box::new(HflipFilter::new()))
    }

    // FFmpeg's hflip filter has no private AVOptions, so av_opt_set() rejects
    // every option name. Taking this path also ensures the factory honours the
    // graph's `create_with_options()` interface instead of silently discarding
    // the supplied options.
    fn create_with_options(&self, options: &[(&str, &str)]) -> Result<Box<dyn Filter>> {
        if options.is_empty() {
            return self.create();
        }
        Err(Error::InvalidArgument)
    }
}

inventory::submit!(&HflipFilterFactory as &dyn FilterFactory);

#[cfg(test)]
mod tests {
    use super::*;
    use wedeo_core::buffer::Buffer;
    use wedeo_core::frame::FramePlane;

    fn make_frame(
        format: PixelFormat,
        width: u32,
        height: u32,
        plane_specs: &[(usize, usize, &[u8])],
    ) -> Frame {
        let mut frame = Frame::new_video(width, height, format);
        let video = frame.video_mut().unwrap();
        for &(linesize, offset, data) in plane_specs {
            let mut buffer = Buffer::new(offset + data.len());
            buffer.data_mut()[offset..offset + data.len()].copy_from_slice(data);
            video.planes.push(FramePlane {
                buffer,
                offset,
                linesize,
            });
        }
        frame
    }

    fn plane(frame: &Frame, index: usize) -> Vec<u8> {
        let p = &frame.video().unwrap().planes[index];
        p.buffer.data()[p.offset..].to_vec()
    }

    #[test]
    fn descriptor_matches_ffmpeg() {
        let filter = HflipFilter::new();
        let desc = filter.descriptor();
        assert_eq!(desc.name, "hflip");
        assert_eq!(desc.description, "Horizontally flip the input video.");
        assert_eq!(desc.inputs.len(), 1);
        assert_eq!(desc.inputs[0].name, "default");
        assert_eq!(desc.inputs[0].media_type, MediaType::Video);
        assert_eq!(desc.outputs.len(), 1);
        assert_eq!(desc.outputs[0].name, "default");
        assert!(desc.flags.contains(FilterFlags::SLICE_THREADS));
        assert!(desc.flags.contains(FilterFlags::SUPPORT_TIMELINE_GENERIC));
    }

    #[test]
    fn registered_factory_creates_filter() {
        let factory = registry::find_filter_by_name("hflip").expect("hflip not registered");
        assert_eq!(factory.descriptor().name, "hflip");
        let mut filter = factory.create().unwrap();
        filter.activate().unwrap();
        assert_eq!(filter.descriptor().name, "hflip");
    }

    #[test]
    fn flips_8bit_planar_yuv420p_and_preserves_metadata() {
        // Y: width 4, height 2, linesize 6 (two padding bytes per row).
        let y = [1u8, 2, 3, 4, 0xaa, 0xaa, 5, 6, 7, 8, 0xbb, 0xbb];
        let u = [10u8, 11, 0xcc, 0xcc];
        let v = [20u8, 21, 0xdd, 0xdd];
        let mut frame = make_frame(
            PixelFormat::Yuv420p,
            4,
            2,
            &[(6, 0, &y), (4, 0, &u), (4, 0, &v)],
        );
        frame.pts = 1234;
        frame.metadata.set("comment", "keep me");

        let out = HflipFilter::new().filter_frame(frame).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].pts, 1234);
        assert_eq!(out[0].metadata.get("comment"), Some("keep me"));

        assert_eq!(
            plane(&out[0], 0),
            vec![4, 3, 2, 1, 0xaa, 0xaa, 8, 7, 6, 5, 0xbb, 0xbb]
        );
        assert_eq!(plane(&out[0], 1), vec![11, 10, 0xcc, 0xcc]);
        assert_eq!(plane(&out[0], 2), vec![21, 20, 0xdd, 0xdd]);
    }

    #[test]
    fn flips_16bit_samples_without_byte_swapping() {
        // gray16le samples: 1, 2, 3 -> 3, 2, 1 (each 2-byte group intact).
        let data = [0x01u8, 0x00, 0x02, 0x00, 0x03, 0x00];
        let frame = make_frame(PixelFormat::Gray16le, 3, 1, &[(6, 0, &data)]);
        let out = HflipFilter::new().filter_frame(frame).unwrap();
        assert_eq!(plane(&out[0], 0), vec![0x03, 0x00, 0x02, 0x00, 0x01, 0x00]);

        // gray16be samples keep their byte order too.
        let data = [0x00u8, 0x01, 0x00, 0x02, 0x00, 0x03];
        let frame = make_frame(PixelFormat::Gray16be, 3, 1, &[(6, 0, &data)]);
        let out = HflipFilter::new().filter_frame(frame).unwrap();
        assert_eq!(plane(&out[0], 0), vec![0x00, 0x03, 0x00, 0x02, 0x00, 0x01]);
    }

    #[test]
    fn flips_24bit_pixels() {
        let data = [1u8, 2, 3, 4, 5, 6];
        let frame = make_frame(PixelFormat::Rgb24, 2, 1, &[(6, 0, &data)]);
        let out = HflipFilter::new().filter_frame(frame).unwrap();
        assert_eq!(plane(&out[0], 0), vec![4, 5, 6, 1, 2, 3]);
    }

    #[test]
    fn flips_semi_planar_nv12() {
        // Y plane width 4; UV plane width 2 with 2-byte samples.
        let y = [1u8, 2, 3, 4];
        let uv = [10u8, 11, 12, 13];
        let frame = make_frame(PixelFormat::Nv12, 4, 1, &[(4, 0, &y), (4, 0, &uv)]);
        let out = HflipFilter::new().filter_frame(frame).unwrap();
        assert_eq!(plane(&out[0], 0), vec![4, 3, 2, 1]);
        assert_eq!(plane(&out[0], 1), vec![12, 13, 10, 11]);
    }

    #[test]
    fn honors_plane_offset() {
        let mut frame = Frame::new_video(4, 1, PixelFormat::Gray8);
        let mut buffer = Buffer::new(8);
        buffer
            .data_mut()
            .copy_from_slice(&[0xff, 0xff, 1, 2, 3, 4, 0xee, 0xee]);
        frame.video_mut().unwrap().planes.push(FramePlane {
            buffer,
            offset: 2,
            linesize: 8,
        });

        let out = HflipFilter::new().filter_frame(frame).unwrap();
        let p = &out[0].video().unwrap().planes[0];
        assert_eq!(&p.buffer.data()[0..2], &[0xff, 0xff]);
        assert_eq!(&p.buffer.data()[2..6], &[4, 3, 2, 1]);
        assert_eq!(&p.buffer.data()[6..8], &[0xee, 0xee]);
    }

    #[test]
    fn rejects_audio_frames() {
        let frame = Frame::new_audio(
            0,
            wedeo_core::sample_format::SampleFormat::U8,
            44100,
            wedeo_core::channel_layout::ChannelLayout::mono(),
        );
        let err = HflipFilter::new().filter_frame(frame).unwrap_err();
        assert_eq!(err, Error::InvalidArgument);
    }

    #[test]
    fn query_formats_rejects_bitstream_and_packed_422() {
        let filter = HflipFilter::new();
        let mut cfg = FormatsConfig::from_descriptor(filter.descriptor());
        filter.query_formats(&mut cfg).unwrap();

        let expected = FilterFormats::from_pixels(SUPPORTED_FORMATS.iter().copied());
        assert_eq!(cfg.inputs[0].formats, expected);
        assert_eq!(cfg.outputs[0].formats, expected);
        for format in [
            PixelFormat::Yuv420p,
            PixelFormat::Yuv422p10le,
            PixelFormat::Nv12,
        ] {
            assert!(cfg.outputs[0].accepts(FilterFormat::Pixel(format)));
        }
        // AV_PIX_FMT_FLAG_BITSTREAM or log2_chroma_w != log2_chroma_h with both
        // chroma components on plane 0.
        for format in [
            PixelFormat::None,
            PixelFormat::MonoWhite,
            PixelFormat::MonoBlack,
            PixelFormat::Yuyv422,
            PixelFormat::Uyvy422,
        ] {
            assert!(!cfg.outputs[0].accepts(FilterFormat::Pixel(format)));
        }
    }

    #[test]
    fn factory_rejects_options_as_ffmpeg_does() {
        let factory = registry::find_filter_by_name("hflip").unwrap();
        assert!(factory.create_with_options(&[]).is_ok());
        assert!(factory.create_with_options(&[("dir", "1")]).is_err());
    }
}
