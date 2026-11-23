//! Work unit: `wedeo-filter-pad`.
//!
//! FFmpeg reference: `libavfilter/vf_pad.c`.
//!
//! A direct port of FFmpeg's video padding filter. The filter allocates a new,
//! enlarged frame, fills the padding border with a color (black by default,
//! using FFmpeg's RGB-to-YUV conversion and limited/full range scaling) and
//! copies the input plane data at the configured offset. All frame metadata is
//! preserved by moving the input frame into the output.

use wedeo_core::buffer::Buffer;
use wedeo_core::error::{Error, Result};
use wedeo_core::frame::{Frame, FramePlane, VideoFrameData};
use wedeo_core::media_type::MediaType;
use wedeo_core::pixel_format::PixelFormat;
use wedeo_filter::registry::FilterFactory;
use wedeo_filter::{FilterDescriptor, FilterFlags, FilterPadDescriptor, FilterPadDirection};

/// Re-exported so downstream crates (and FATE tests) can use the filter trait
/// and look up the registered factory without depending on `wedeo-filter`
/// directly.
pub use wedeo_filter::Filter;
pub use wedeo_filter::registry::find_filter_by_name;

/// Alignment of allocated plane strides, matching FFmpeg's `ALIGN` in
/// `libavutil/frame.c` on targets without AVX-512 (`HAVE_SIMD_ALIGN_64` false).
const STRIDE_ALIGN: usize = 32;

// ---------------------------------------------------------------------------
// Color
// ---------------------------------------------------------------------------

/// Padding color, stored as RGBA (matching FFmpeg's `AV_OPT_TYPE_COLOR`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PadColor {
    /// Red, green, blue and alpha components, each in `[0, 255]`.
    pub rgba: [u8; 4],
}

impl PadColor {
    /// FFmpeg's default color for the pad filter.
    pub const BLACK: Self = Self {
        rgba: [0, 0, 0, 255],
    };
    pub const WHITE: Self = Self {
        rgba: [255, 255, 255, 255],
    };
    pub const RED: Self = Self {
        rgba: [255, 0, 0, 255],
    };
    pub const GREEN: Self = Self {
        rgba: [0, 128, 0, 255],
    };
    pub const BLUE: Self = Self {
        rgba: [0, 0, 255, 255],
    };

    /// Create an opaque color from its RGB components.
    pub fn new(r: u8, g: u8, b: u8) -> Self {
        Self {
            rgba: [r, g, b, 255],
        }
    }

    /// Parse an FFmpeg-style color string: a named color or `0xRRGGBB` /
    /// `#RRGGBB` (optionally with an `A` component for hex forms).
    pub fn parse(text: &str) -> Result<Self> {
        let s = text.trim();
        let named = match s.to_ascii_lowercase().as_str() {
            "black" => Some(Self::BLACK),
            "white" => Some(Self::WHITE),
            "red" => Some(Self::RED),
            "green" => Some(Self::GREEN),
            "blue" => Some(Self::BLUE),
            "yellow" => Some(Self::new(255, 255, 0)),
            "cyan" | "aqua" => Some(Self::new(0, 255, 255)),
            "magenta" | "fuchsia" => Some(Self::new(255, 0, 255)),
            "gray" | "grey" => Some(Self::new(128, 128, 128)),
            "orange" => Some(Self::new(255, 165, 0)),
            "purple" => Some(Self::new(128, 0, 128)),
            "pink" => Some(Self::new(255, 192, 203)),
            _ => None,
        };
        if let Some(c) = named {
            return Ok(c);
        }

        let hex = s.strip_prefix("0x").or_else(|| s.strip_prefix('#'));
        let Some(hex) = hex else {
            return Err(Error::InvalidArgument);
        };
        let parse_byte = |range: std::ops::Range<usize>| -> Result<u8> {
            u8::from_str_radix(hex.get(range).ok_or(Error::InvalidArgument)?, 16)
                .map_err(|_| Error::InvalidArgument)
        };
        match hex.len() {
            6 => Ok(Self::new(
                parse_byte(0..2)?,
                parse_byte(2..4)?,
                parse_byte(4..6)?,
            )),
            8 => Ok(Self {
                rgba: [
                    parse_byte(0..2)?,
                    parse_byte(2..4)?,
                    parse_byte(4..6)?,
                    parse_byte(6..8)?,
                ],
            }),
            _ => Err(Error::InvalidArgument),
        }
    }
}

impl Default for PadColor {
    fn default() -> Self {
        Self::BLACK
    }
}

// ---------------------------------------------------------------------------
// Expression evaluation (the subset used by vf_pad's width/height/x/y options)
// ---------------------------------------------------------------------------

/// Variables made available to pad expressions, mirroring `var_names[]` in
/// `libavfilter/vf_pad.c`.
struct Vars {
    in_w: f64,
    in_h: f64,
    out_w: f64,
    out_h: f64,
    x: f64,
    y: f64,
    a: f64,
    sar: f64,
    dar: f64,
    hsub: f64,
    vsub: f64,
}

impl Vars {
    fn get(&self, name: &str) -> Option<f64> {
        match name {
            "in_w" | "iw" => Some(self.in_w),
            "in_h" | "ih" => Some(self.in_h),
            "out_w" | "ow" => Some(self.out_w),
            "out_h" | "oh" => Some(self.out_h),
            "x" => Some(self.x),
            "y" => Some(self.y),
            "a" => Some(self.a),
            "sar" => Some(self.sar),
            "dar" => Some(self.dar),
            "hsub" => Some(self.hsub),
            "vsub" => Some(self.vsub),
            _ => None,
        }
    }
}

/// Minimal recursive-descent parser for pad expressions: `+ - * /`, unary
/// signs, parentheses, decimal numbers and the variables listed in `Vars`.
struct ExprParser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> ExprParser<'a> {
    fn new(text: &'a str) -> Self {
        Self {
            bytes: text.as_bytes(),
            pos: 0,
        }
    }

    fn eval(text: &str, vars: &Vars) -> Result<f64> {
        let mut p = ExprParser::new(text);
        let value = p.expr(vars)?;
        p.whitespace();
        if p.pos == p.bytes.len() {
            Ok(value)
        } else {
            Err(Error::InvalidArgument)
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn whitespace(&mut self) {
        while matches!(self.peek(), Some(b' ') | Some(b'\t')) {
            self.pos += 1;
        }
    }

    fn expr(&mut self, vars: &Vars) -> Result<f64> {
        let mut value = self.term(vars)?;
        loop {
            self.whitespace();
            match self.peek() {
                Some(b'+') => {
                    self.pos += 1;
                    value += self.term(vars)?;
                }
                Some(b'-') => {
                    self.pos += 1;
                    value -= self.term(vars)?;
                }
                _ => return Ok(value),
            }
        }
    }

    fn term(&mut self, vars: &Vars) -> Result<f64> {
        let mut value = self.factor(vars)?;
        loop {
            self.whitespace();
            match self.peek() {
                Some(b'*') => {
                    self.pos += 1;
                    value *= self.factor(vars)?;
                }
                Some(b'/') => {
                    self.pos += 1;
                    value /= self.factor(vars)?;
                }
                _ => return Ok(value),
            }
        }
    }

    fn factor(&mut self, vars: &Vars) -> Result<f64> {
        self.whitespace();
        match self.peek() {
            Some(b'+') => {
                self.pos += 1;
                self.factor(vars)
            }
            Some(b'-') => {
                self.pos += 1;
                Ok(-self.factor(vars)?)
            }
            Some(b'(') => {
                self.pos += 1;
                let value = self.expr(vars)?;
                self.whitespace();
                if self.peek() != Some(b')') {
                    return Err(Error::InvalidArgument);
                }
                self.pos += 1;
                Ok(value)
            }
            Some(c) if c.is_ascii_digit() || c == b'.' => {
                let start = self.pos;
                while let Some(c) = self.peek() {
                    if c.is_ascii_digit() || matches!(c, b'.' | b'e' | b'E' | b'+' | b'-') {
                        self.pos += 1;
                    } else {
                        break;
                    }
                }
                std::str::from_utf8(&self.bytes[start..self.pos])
                    .ok()
                    .and_then(|s| s.parse::<f64>().ok())
                    .ok_or(Error::InvalidArgument)
            }
            Some(c) if c.is_ascii_alphabetic() || c == b'_' => {
                let start = self.pos;
                while let Some(c) = self.peek() {
                    if c.is_ascii_alphanumeric() || c == b'_' {
                        self.pos += 1;
                    } else {
                        break;
                    }
                }
                let name = std::str::from_utf8(&self.bytes[start..self.pos])
                    .map_err(|_| Error::InvalidArgument)?;
                vars.get(name).ok_or(Error::InvalidArgument)
            }
            _ => Err(Error::InvalidArgument),
        }
    }
}

// ---------------------------------------------------------------------------
// Pixel format geometry (libavutil/pixdesc.c + libavfilter/drawutils.c)
// ---------------------------------------------------------------------------

/// One pixel format component, laid out as in FFmpeg's `AVComponentDescriptor`.
#[derive(Debug, Clone, Copy)]
struct Component {
    plane: usize,
    /// Byte offset of the component within a pixel group.
    offset: usize,
    /// Size in bytes (1 or 2).
    bytes: usize,
    /// Index into the computed YUV/RGBA values: 0=Y/R, 1=U/G, 2=V/B, 3=A.
    value: usize,
    /// Bits the stored sample is shifted left (6 for 10-bit-in-16-bit formats).
    shift: u16,
    depth: u16,
}

#[derive(Debug, Clone)]
struct FormatInfo {
    nb_planes: usize,
    plane_hsub: [u32; 4],
    plane_vsub: [u32; 4],
    /// Bytes per pixel group in each plane (a group is one sample for planar
    /// formats, one RGB triple/quad, or two packed YUYV pixels).
    plane_step: [usize; 4],
    components: Vec<Component>,
    is_rgb: bool,
    /// Default to full range (yuvj* formats).
    full_range: bool,
    big_endian: bool,
}

impl FormatInfo {
    fn hsub_max(&self) -> u32 {
        self.plane_hsub[..self.nb_planes]
            .iter()
            .copied()
            .max()
            .unwrap_or(0)
    }

    fn vsub_max(&self) -> u32 {
        self.plane_vsub[..self.nb_planes]
            .iter()
            .copied()
            .max()
            .unwrap_or(0)
    }
}

fn planar_yuv(
    hlog: u32,
    vlog: u32,
    bytes: usize,
    shift: u16,
    depth: u16,
    full: bool,
    be: bool,
) -> FormatInfo {
    FormatInfo {
        nb_planes: 3,
        plane_hsub: [0, hlog, hlog, 0],
        plane_vsub: [0, vlog, vlog, 0],
        plane_step: [bytes, bytes, bytes, 0],
        components: vec![
            Component {
                plane: 0,
                offset: 0,
                bytes,
                value: 0,
                shift,
                depth,
            },
            Component {
                plane: 1,
                offset: 0,
                bytes,
                value: 1,
                shift,
                depth,
            },
            Component {
                plane: 2,
                offset: 0,
                bytes,
                value: 2,
                shift,
                depth,
            },
        ],
        is_rgb: false,
        full_range: full,
        big_endian: be,
    }
}

fn semiplanar_yuv(hlog: u32, vlog: u32, swap_uv: bool) -> FormatInfo {
    let (u_off, v_off) = if swap_uv { (1, 0) } else { (0, 1) };
    FormatInfo {
        nb_planes: 2,
        plane_hsub: [0, hlog, 0, 0],
        plane_vsub: [0, vlog, 0, 0],
        plane_step: [1, 2, 0, 0],
        components: vec![
            Component {
                plane: 0,
                offset: 0,
                bytes: 1,
                value: 0,
                shift: 0,
                depth: 8,
            },
            Component {
                plane: 1,
                offset: u_off,
                bytes: 1,
                value: 1,
                shift: 0,
                depth: 8,
            },
            Component {
                plane: 1,
                offset: v_off,
                bytes: 1,
                value: 2,
                shift: 0,
                depth: 8,
            },
        ],
        is_rgb: false,
        full_range: false,
        big_endian: false,
    }
}

fn packed_rgb(
    step: usize,
    offsets: [usize; 4],
    with_alpha: bool,
    depth: u16,
    be: bool,
) -> FormatInfo {
    let mut components = Vec::with_capacity(if with_alpha { 4 } else { 3 });
    for value in 0..4 {
        if value == 3 && !with_alpha {
            continue;
        }
        components.push(Component {
            plane: 0,
            offset: offsets[value],
            bytes: if depth > 8 { 2 } else { 1 },
            value,
            shift: 0,
            depth,
        });
    }
    FormatInfo {
        nb_planes: 1,
        plane_hsub: [0; 4],
        plane_vsub: [0; 4],
        plane_step: [step, 0, 0, 0],
        components,
        is_rgb: true,
        full_range: true,
        big_endian: be,
    }
}

fn planar_gray(bytes: usize, shift: u16, depth: u16, be: bool) -> FormatInfo {
    FormatInfo {
        nb_planes: 1,
        plane_hsub: [0; 4],
        plane_vsub: [0; 4],
        plane_step: [bytes, 0, 0, 0],
        components: vec![Component {
            plane: 0,
            offset: 0,
            bytes,
            value: 0,
            shift,
            depth,
        }],
        is_rgb: false,
        full_range: false,
        big_endian: be,
    }
}

fn packed_yuv422(y_off: usize, u_off: usize, v_off: usize) -> FormatInfo {
    FormatInfo {
        nb_planes: 1,
        // Packed 4:2:2 stores two pixels per 4-byte group.
        plane_hsub: [1, 0, 0, 0],
        plane_vsub: [0; 4],
        plane_step: [4, 0, 0, 0],
        components: vec![
            Component {
                plane: 0,
                offset: y_off,
                bytes: 1,
                value: 0,
                shift: 0,
                depth: 8,
            },
            Component {
                plane: 0,
                offset: u_off,
                bytes: 1,
                value: 1,
                shift: 0,
                depth: 8,
            },
            Component {
                plane: 0,
                offset: v_off,
                bytes: 1,
                value: 2,
                shift: 0,
                depth: 8,
            },
        ],
        is_rgb: false,
        full_range: false,
        big_endian: false,
    }
}

fn format_info(pf: PixelFormat) -> Option<FormatInfo> {
    match pf {
        PixelFormat::Yuv420p => Some(planar_yuv(1, 1, 1, 0, 8, false, false)),
        PixelFormat::Yuv422p => Some(planar_yuv(1, 0, 1, 0, 8, false, false)),
        PixelFormat::Yuv444p => Some(planar_yuv(0, 0, 1, 0, 8, false, false)),
        PixelFormat::Yuv410p => Some(planar_yuv(2, 2, 1, 0, 8, false, false)),
        PixelFormat::Yuv411p => Some(planar_yuv(2, 0, 1, 0, 8, false, false)),
        PixelFormat::Yuvj420p => Some(planar_yuv(1, 1, 1, 0, 8, true, false)),
        PixelFormat::Yuvj422p => Some(planar_yuv(1, 0, 1, 0, 8, true, false)),
        PixelFormat::Yuvj444p => Some(planar_yuv(0, 0, 1, 0, 8, true, false)),
        PixelFormat::Yuv420p10le => Some(planar_yuv(1, 1, 2, 6, 10, false, false)),
        PixelFormat::Yuv420p10be => Some(planar_yuv(1, 1, 2, 6, 10, false, true)),
        PixelFormat::Yuv420p16le => Some(planar_yuv(1, 1, 2, 0, 16, false, false)),
        PixelFormat::Yuv420p16be => Some(planar_yuv(1, 1, 2, 0, 16, false, true)),
        PixelFormat::Nv12 => Some(semiplanar_yuv(1, 1, false)),
        PixelFormat::Nv21 => Some(semiplanar_yuv(1, 1, true)),
        PixelFormat::Yuyv422 => Some(packed_yuv422(0, 1, 3)),
        PixelFormat::Uyvy422 => Some(packed_yuv422(1, 0, 2)),
        PixelFormat::Rgb24 => Some(packed_rgb(3, [0, 1, 2, 0], false, 8, false)),
        PixelFormat::Bgr24 => Some(packed_rgb(3, [2, 1, 0, 0], false, 8, false)),
        PixelFormat::Argb => Some(packed_rgb(4, [1, 2, 3, 0], true, 8, false)),
        PixelFormat::Rgba => Some(packed_rgb(4, [0, 1, 2, 3], true, 8, false)),
        PixelFormat::Abgr => Some(packed_rgb(4, [3, 2, 1, 0], true, 8, false)),
        PixelFormat::Bgra => Some(packed_rgb(4, [2, 1, 0, 3], true, 8, false)),
        PixelFormat::Rgb48le => Some(packed_rgb(6, [0, 2, 4, 0], false, 16, false)),
        PixelFormat::Rgb48be => Some(packed_rgb(6, [0, 2, 4, 0], false, 16, true)),
        PixelFormat::Gray8 => Some(planar_gray(1, 0, 8, false)),
        PixelFormat::Gray16le => Some(planar_gray(2, 0, 16, false)),
        PixelFormat::Gray16be => Some(planar_gray(2, 0, 16, true)),
        // Paletted and bitstream formats are not supported by ff_draw_init().
        _ => None,
    }
}

/// Luma coefficients from FFmpeg's `av_csp_luma_coeffs_from_avcsp()`.
/// `AVCOL_SPC_UNSPECIFIED` maps to SMPTE170M (BT.601), as in `ff_draw_init2()`.
fn luma_coefficients(csp: wedeo_core::frame::ColorSpace) -> (f64, f64, f64) {
    use wedeo_core::frame::ColorSpace;
    match csp {
        ColorSpace::Fcc => (0.30, 0.59, 0.11),
        ColorSpace::Bt709 => (0.2126, 0.7152, 0.0722),
        ColorSpace::Smpte240m => (0.212, 0.701, 0.087),
        ColorSpace::Bt2020Ncl | ColorSpace::Bt2020Cl => (0.2627, 0.6780, 0.0593),
        ColorSpace::Rgb => (1.0, 1.0, 1.0),
        _ => (0.299, 0.587, 0.114),
    }
}

/// Convert the RGBA color into per-plane fill patterns, following FFmpeg's
/// `ff_draw_color()` (`libavfilter/drawutils.c:179`).
fn color_templates(
    info: &FormatInfo,
    color: PadColor,
    csp: wedeo_core::frame::ColorSpace,
    range: wedeo_core::frame::ColorRange,
) -> [Vec<u8>; 4] {
    use wedeo_core::frame::ColorRange;

    let rgbad = [
        color.rgba[0] as f64 / 255.0,
        color.rgba[1] as f64 / 255.0,
        color.rgba[2] as f64 / 255.0,
        color.rgba[3] as f64 / 255.0,
    ];
    let mut yuvad = [0.0f64; 4];
    if info.is_rgb {
        yuvad[..3].copy_from_slice(&rgbad[..3]);
    } else if csp == wedeo_core::frame::ColorSpace::Ycgco {
        yuvad[0] = 0.25 * rgbad[0] + 0.5 * rgbad[1] + 0.25 * rgbad[2];
        yuvad[1] = -0.25 * rgbad[0] + 0.5 * rgbad[1] - 0.25 * rgbad[2];
        yuvad[2] = 0.5 * rgbad[0] - 0.5 * rgbad[2];
    } else if csp == wedeo_core::frame::ColorSpace::Rgb {
        yuvad[0] = rgbad[1];
        yuvad[1] = -0.5 * rgbad[1] + 0.5 * rgbad[2];
        yuvad[2] = 0.5 * rgbad[0] - 0.5 * rgbad[1];
    } else {
        let (cr, cg, cb) = luma_coefficients(csp);
        let bscale = 0.5 / (cb - 1.0);
        let rscale = 0.5 / (cr - 1.0);
        yuvad[0] = rgbad[0] * cr + rgbad[1] * cg + rgbad[2] * cb;
        yuvad[1] = rgbad[0] * bscale * cr + rgbad[1] * bscale * cg + rgbad[2] * 0.5;
        yuvad[2] = rgbad[0] * 0.5 + rgbad[1] * rscale * cg + rgbad[2] * rscale * cb;
    }
    yuvad[3] = rgbad[3];

    let mpeg_range = match range {
        ColorRange::Mpeg => true,
        ColorRange::Jpeg => false,
        ColorRange::Unspecified => !info.full_range,
    };
    for i in 0..3 {
        let chroma = !info.is_rgb && i > 0;
        if mpeg_range {
            yuvad[i] = yuvad[i] * (if chroma { 224.0 } else { 219.0 }) / 255.0
                + (if chroma { 128.0 } else { 16.0 }) / 255.0;
        } else if chroma {
            yuvad[i] += 0.5;
        }
    }

    let mut templates: [Vec<u8>; 4] = std::array::from_fn(|p| vec![0u8; info.plane_step[p]]);
    for comp in &info.components {
        let max_value = ((1u64 << (comp.depth + comp.shift)) - 1) as f64;
        let value = (yuvad[comp.value] * max_value + 0.5) as u64;
        let slot = &mut templates[comp.plane][comp.offset..comp.offset + comp.bytes];
        if comp.bytes == 1 {
            slot[0] = value as u8;
        } else {
            let v = value as u16;
            let bytes = if info.big_endian {
                v.to_be_bytes()
            } else {
                v.to_le_bytes()
            };
            slot.copy_from_slice(&bytes);
        }
    }
    templates
}

/// Fill a rectangle using per-plane pixel templates (`ff_fill_rectangle`).
fn fill_rect(
    info: &FormatInfo,
    templates: &[Vec<u8>; 4],
    planes: &mut [FramePlane],
    x: i32,
    y: i32,
    w: i32,
    h: i32,
) -> Result<()> {
    for p in 0..info.nb_planes {
        let hsub = info.plane_hsub[p];
        let vsub = info.plane_vsub[p];
        let step = info.plane_step[p];
        let px = (x >> hsub) as usize;
        let py = (y >> vsub) as usize;
        let count_x = (w >> hsub) as usize;
        let count_y = (h >> vsub) as usize;
        if count_x == 0 || count_y == 0 {
            continue;
        }
        let linesize = planes[p].linesize;
        let offset = planes[p].offset;
        let data = planes[p].buffer.data_mut();
        for row in 0..count_y {
            let mut pos = offset + (py + row) * linesize + px * step;
            for _ in 0..count_x {
                if pos + step > data.len() {
                    return Err(Error::InvalidData);
                }
                data[pos..pos + step].copy_from_slice(&templates[p]);
                pos += step;
            }
        }
    }
    Ok(())
}

/// Copy a rectangle between two sets of planes (`ff_copy_rectangle2`).
fn copy_rect(
    src: &VideoFrameData,
    dst: &mut [FramePlane],
    info: &FormatInfo,
    (src_x, src_y): (i32, i32),
    (dst_x, dst_y): (i32, i32),
    (w, h): (i32, i32),
) -> Result<()> {
    for p in 0..info.nb_planes {
        let src_plane = src.planes.get(p).ok_or(Error::InvalidData)?;
        let dst_plane = dst.get_mut(p).ok_or(Error::InvalidData)?;
        let hsub = info.plane_hsub[p];
        let vsub = info.plane_vsub[p];
        let step = info.plane_step[p];
        let sx = (src_x >> hsub) as usize;
        let sy = (src_y >> vsub) as usize;
        let dx = (dst_x >> hsub) as usize;
        let dy = (dst_y >> vsub) as usize;
        let count_x = (w >> hsub) as usize;
        let count_y = (h >> vsub) as usize;
        if count_x == 0 || count_y == 0 {
            continue;
        }
        let bytes = count_x * step;
        let src_data = src_plane.buffer.data();
        let dst_data = dst_plane.buffer.data_mut();
        for row in 0..count_y {
            let s = src_plane.offset + (sy + row) * src_plane.linesize + sx * step;
            let d = dst_plane.offset + (dy + row) * dst_plane.linesize + dx * step;
            if s + bytes > src_data.len() || d + bytes > dst_data.len() {
                return Err(Error::InvalidData);
            }
            dst_data[d..d + bytes].copy_from_slice(&src_data[s..s + bytes]);
        }
    }
    Ok(())
}

fn align_up(value: usize, align: usize) -> usize {
    (value + align - 1) & !(align - 1)
}

fn round_down_to_sub(value: i32, sub: u32) -> i32 {
    (value >> sub) << sub
}

// ---------------------------------------------------------------------------
// Filter
// ---------------------------------------------------------------------------

/// The `pad` video filter. Width and height expressions default to the input
/// size (`iw`/`ih`), offsets to zero, and the border color to black.
#[derive(Debug, Clone)]
pub struct PadFilter {
    w_expr: String,
    h_expr: String,
    x_expr: String,
    y_expr: String,
    color: PadColor,
}

impl PadFilter {
    /// Create a filter with explicit pixel dimensions and offset.
    ///
    /// A zero width or height means "use the input dimension", matching the
    /// FFmpeg option comment "a value of 0 will result in the input size".
    pub fn new(width: u32, height: u32, x: i32, y: i32) -> Self {
        Self::with_color(width, height, x, y, PadColor::BLACK)
    }

    /// Create a filter with explicit dimensions, offset and border color.
    pub fn with_color(width: u32, height: u32, x: i32, y: i32, color: PadColor) -> Self {
        Self {
            w_expr: if width == 0 {
                "iw".to_owned()
            } else {
                width.to_string()
            },
            h_expr: if height == 0 {
                "ih".to_owned()
            } else {
                height.to_string()
            },
            x_expr: x.to_string(),
            y_expr: y.to_string(),
            color,
        }
    }

    /// Build a filter from FFmpeg-style option pairs (`w`/`width`, `h`/`height`,
    /// `x`, `y`, `color`). Missing options keep their defaults.
    pub fn from_options<'a>(options: impl IntoIterator<Item = (&'a str, &'a str)>) -> Result<Self> {
        let mut filter = Self::default();
        for (key, value) in options {
            let target = match key {
                "w" | "width" => &mut filter.w_expr,
                "h" | "height" => &mut filter.h_expr,
                "x" => &mut filter.x_expr,
                "y" => &mut filter.y_expr,
                "color" => {
                    filter.color = PadColor::parse(value)?;
                    continue;
                }
                _ => return Err(Error::OptionNotFound),
            };
            *target = value.to_owned();
        }
        Ok(filter)
    }

    /// Border color used for the padding area.
    pub fn color(&self) -> PadColor {
        self.color
    }

    /// Resolve the configured expressions against an input frame size using
    /// the same evaluation order as `config_input()` in `vf_pad.c`.
    fn resolve(
        &self,
        in_w: i32,
        in_h: i32,
        sar: wedeo_core::rational::Rational,
        info: &FormatInfo,
    ) -> Result<(i32, i32, i32, i32)> {
        let aspect = if in_h != 0 {
            in_w as f64 / in_h as f64
        } else {
            f64::INFINITY
        };
        let sar_f64 = if sar.den != 0 && sar.num != 0 {
            sar.num as f64 / sar.den as f64
        } else {
            1.0
        };
        let mut vars = Vars {
            in_w: in_w as f64,
            in_h: in_h as f64,
            // FFmpeg initializes ow/oh to NAN for the first evaluation.
            out_w: f64::NAN,
            out_h: f64::NAN,
            x: 0.0,
            y: 0.0,
            a: aspect,
            sar: sar_f64,
            dar: aspect * sar_f64,
            hsub: (1u64 << info.hsub_max()) as f64,
            vsub: (1u64 << info.vsub_max()) as f64,
        };

        // Evaluate width and height, then evaluate width again since it may
        // depend on the evaluated output height (vf_pad.c:135-155).
        let w = ExprParser::eval(&self.w_expr, &vars)? as i32;
        vars.out_w = w as f64;
        let mut h = ExprParser::eval(&self.h_expr, &vars)? as i32;
        vars.out_h = h as f64;
        if h == 0 {
            h = in_h;
            vars.out_h = h as f64;
        }
        let w = ExprParser::eval(&self.w_expr, &vars)? as i32;
        let w = if w == 0 { in_w } else { w };
        vars.out_w = w as f64;

        let mut x = ExprParser::eval(&self.x_expr, &vars)? as i32;
        vars.x = x as f64;
        let mut y = ExprParser::eval(&self.y_expr, &vars)? as i32;
        vars.y = y as f64;
        x = ExprParser::eval(&self.x_expr, &vars)? as i32;
        y = ExprParser::eval(&self.y_expr, &vars)? as i32;

        // Center the input when the requested offset would put it outside the
        // padded area (vf_pad.c:183-186).
        if x < 0 || x as i64 + in_w as i64 > w as i64 {
            x = (w - in_w) / 2;
        }
        if y < 0 || y as i64 + in_h as i64 > h as i64 {
            y = (h - in_h) / 2;
        }

        let hsub = info.hsub_max();
        let vsub = info.vsub_max();
        let w = round_down_to_sub(w, hsub);
        let h = round_down_to_sub(h, vsub);
        let x = round_down_to_sub(x, hsub);
        let y = round_down_to_sub(y, vsub);
        let in_w = round_down_to_sub(in_w, hsub);
        let in_h = round_down_to_sub(in_h, vsub);

        if w < in_w || h < in_h || x < 0 || y < 0 || x + in_w > w || y + in_h > h {
            return Err(Error::InvalidArgument);
        }
        Ok((w, h, x, y))
    }
}

impl Default for PadFilter {
    fn default() -> Self {
        Self {
            w_expr: "iw".to_owned(),
            h_expr: "ih".to_owned(),
            x_expr: "0".to_owned(),
            y_expr: "0".to_owned(),
            color: PadColor::BLACK,
        }
    }
}

static INPUTS: &[FilterPadDescriptor] = &[FilterPadDescriptor {
    name: "default",
    media_type: MediaType::Video,
    direction: FilterPadDirection::Input,
}];
static OUTPUTS: &[FilterPadDescriptor] = &[FilterPadDescriptor {
    name: "default",
    media_type: MediaType::Video,
    direction: FilterPadDirection::Output,
}];

static DESCRIPTOR: FilterDescriptor = FilterDescriptor {
    name: "pad",
    description: "Pad the input video.",
    inputs: INPUTS,
    outputs: OUTPUTS,
    flags: FilterFlags::empty(),
};

impl Filter for PadFilter {
    fn descriptor(&self) -> &FilterDescriptor {
        &DESCRIPTOR
    }

    fn activate(&mut self) -> Result<()> {
        Ok(())
    }

    fn filter_frame(&mut self, frame: Frame) -> Result<Vec<Frame>> {
        let (planes, out_w, out_h) = {
            let video = frame.video().ok_or(Error::InvalidData)?;
            let format = video.format;
            let info = format_info(format).ok_or(Error::InvalidArgument)?;
            let (out_w, out_h, x, y) = self.resolve(
                video.width as i32,
                video.height as i32,
                video.sample_aspect_ratio,
                &info,
            )?;
            let templates =
                color_templates(&info, self.color, video.color_space, video.color_range);

            let mut planes = Vec::with_capacity(info.nb_planes);
            for p in 0..info.nb_planes {
                let stride = align_up(
                    (out_w as usize >> info.plane_hsub[p]) * info.plane_step[p],
                    STRIDE_ALIGN,
                );
                let height = out_h as usize >> info.plane_vsub[p];
                planes.push(FramePlane {
                    buffer: Buffer::new(stride * height),
                    offset: 0,
                    linesize: stride,
                });
            }

            // Fill the whole output with the border color, then overwrite the
            // rectangle occupied by the input. This is equivalent to FFmpeg's
            // top/bottom/left/right fill_rectangle calls plus copy_rectangle2.
            fill_rect(&info, &templates, &mut planes, 0, 0, out_w, out_h)?;
            copy_rect(
                video,
                &mut planes,
                &info,
                (0, 0),
                (x, y),
                (video.width as i32, video.height as i32),
            )?;
            (planes, out_w, out_h)
        };

        let mut out = frame;
        let video = out.video_mut().ok_or(Error::InvalidData)?;
        video.planes = planes;
        video.width = out_w as u32;
        video.height = out_h as u32;
        Ok(vec![out])
    }
}

struct PadFilterFactory;

impl FilterFactory for PadFilterFactory {
    fn descriptor(&self) -> &FilterDescriptor {
        &DESCRIPTOR
    }

    fn create(&self) -> Result<Box<dyn Filter>> {
        Ok(Box::new(PadFilter::default()))
    }
}

inventory::submit!(&PadFilterFactory as &dyn FilterFactory);

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use wedeo_core::frame::{ColorRange, ColorSpace, FrameData, FrameFlags, PictureType};
    use wedeo_core::metadata::Metadata;
    use wedeo_core::rational::Rational;
    use wedeo_core::timestamp::NOPTS_VALUE;

    fn make_yuv420(w: u32, h: u32, y: u8, u: u8, v: u8) -> Frame {
        let cw = (w / 2) as usize;
        let ch = (h / 2) as usize;
        let mut frame = Frame::new_video(w, h, PixelFormat::Yuv420p);
        frame.pts = 42;
        frame.duration = 3;
        frame.flags = FrameFlags::KEY;
        frame.time_base = Rational::new(1, 30);
        frame.metadata = Metadata::new();
        frame.metadata.set("key", "value");
        if let FrameData::Video(ref mut video) = frame.data {
            video.planes = vec![
                FramePlane {
                    buffer: Buffer::from_slice(&vec![y; (w * h) as usize]),
                    offset: 0,
                    linesize: w as usize,
                },
                FramePlane {
                    buffer: Buffer::from_slice(&vec![u; cw * ch]),
                    offset: 0,
                    linesize: cw,
                },
                FramePlane {
                    buffer: Buffer::from_slice(&vec![v; cw * ch]),
                    offset: 0,
                    linesize: cw,
                },
            ];
            video.picture_type = PictureType::I;
            video.color_range = ColorRange::Mpeg;
            video.color_space = ColorSpace::Unspecified;
        }
        frame
    }

    fn plane_byte(frame: &Frame, plane: usize, x: usize, y: usize) -> u8 {
        let video = frame.video().unwrap();
        let p = &video.planes[plane];
        let hsub = if plane == 0 { 0 } else { 1 };
        let vsub = if plane == 0 { 0 } else { 1 };
        p.buffer.data()[p.offset + (y >> vsub) * p.linesize + (x >> hsub)]
    }

    #[test]
    fn black_padding_yuv420p() {
        let input = make_yuv420(4, 4, 100, 128, 128);
        let mut filter = PadFilter::with_color(8, 8, 2, 2, PadColor::BLACK);
        filter.activate().unwrap();
        let out = filter.filter_frame(input).unwrap();
        assert_eq!(out.len(), 1);
        let out = &out[0];
        let video = out.video().unwrap();
        assert_eq!((video.width, video.height), (8, 8));
        assert_eq!(video.format, PixelFormat::Yuv420p);
        assert_eq!(video.planes.len(), 3);
        assert_eq!(video.planes[0].linesize, 32);
        assert_eq!(video.planes[1].linesize, 32);
        // Limited-range black: Y=16, U=V=128.
        assert_eq!(plane_byte(out, 0, 0, 0), 16);
        assert_eq!(plane_byte(out, 1, 0, 0), 128);
        assert_eq!(plane_byte(out, 2, 0, 0), 128);
        // Source pixels land at (2, 2).
        assert_eq!(plane_byte(out, 0, 2, 2), 100);
        assert_eq!(plane_byte(out, 0, 5, 5), 100);
        // Right/bottom border.
        assert_eq!(plane_byte(out, 0, 7, 7), 16);
        assert_eq!(plane_byte(out, 1, 7, 7), 128);
        // Metadata is preserved.
        assert_eq!(out.pts, 42);
        assert_eq!(out.duration, 3);
        assert_eq!(out.flags, FrameFlags::KEY);
        assert_eq!(out.metadata.get("key"), Some("value"));
    }

    #[test]
    fn default_keeps_input_size() {
        let input = make_yuv420(4, 4, 77, 90, 160);
        let mut filter = PadFilter::default();
        let out = filter.filter_frame(input).unwrap().remove(0);
        let video = out.video().unwrap();
        assert_eq!((video.width, video.height), (4, 4));
        assert_eq!(plane_byte(&out, 0, 3, 3), 77);
    }

    #[test]
    fn centered_when_offset_out_of_range() {
        let input = make_yuv420(4, 4, 100, 128, 128);
        // w=8, in_w=4 -> x = (8-4)/2 = 2 ; negative y is centered to 2.
        let mut filter = PadFilter::new(8, 8, -1, -1);
        let out = filter.filter_frame(input).unwrap().remove(0);
        assert_eq!(plane_byte(&out, 0, 2, 2), 100);
        assert_eq!(plane_byte(&out, 0, 1, 2), 16);
    }

    #[test]
    fn expressions_and_color_string() {
        let input = make_yuv420(4, 4, 100, 128, 128);
        let mut filter = PadFilter::from_options([
            ("w", "iw+4"),
            ("h", "ih+4"),
            ("x", "2"),
            ("y", "2"),
            ("color", "white"),
        ])
        .unwrap();
        let out = filter.filter_frame(input).unwrap().remove(0);
        let video = out.video().unwrap();
        assert_eq!((video.width, video.height), (8, 8));
        // Limited-range white: Y=235, U=V=128.
        assert_eq!(plane_byte(&out, 0, 0, 0), 235);
        assert_eq!(plane_byte(&out, 1, 0, 0), 128);
        assert_eq!(plane_byte(&out, 0, 2, 2), 100);
    }

    #[test]
    fn rgb24_white_padding() {
        let w = 2usize;
        let h = 2usize;
        let mut frame = Frame::new_video(w as u32, h as u32, PixelFormat::Rgb24);
        let data = [255u8, 0, 0].repeat(w * h);
        if let FrameData::Video(ref mut video) = frame.data {
            video.planes = vec![FramePlane {
                buffer: Buffer::from_slice(&data),
                offset: 0,
                linesize: w * 3,
            }];
        }
        let mut filter = PadFilter::with_color(4, 4, 1, 1, PadColor::WHITE);
        let out = filter.filter_frame(frame).unwrap().remove(0);
        let video = out.video().unwrap();
        assert_eq!((video.width, video.height), (4, 4));
        let p = &video.planes[0];
        let data = p.buffer.data();
        let at = |x: usize, y: usize| {
            &data[p.offset + y * p.linesize + x * 3..p.offset + y * p.linesize + x * 3 + 3]
        };
        assert_eq!(at(0, 0), &[255, 255, 255]);
        assert_eq!(at(1, 1), &[255, 0, 0]);
        assert_eq!(at(2, 2), &[255, 0, 0]);
        assert_eq!(at(3, 3), &[255, 255, 255]);
    }

    #[test]
    fn rejects_smaller_padded_size_and_audio() {
        let input = make_yuv420(8, 8, 100, 128, 128);
        let mut filter = PadFilter::new(4, 8, 0, 0);
        assert!(filter.filter_frame(input).is_err());

        let audio = Frame::new_audio(
            128,
            wedeo_core::sample_format::SampleFormat::S16,
            44100,
            wedeo_core::channel_layout::ChannelLayout::mono(),
        );
        let mut filter = PadFilter::default();
        assert_eq!(filter.filter_frame(audio).unwrap_err(), Error::InvalidData);
    }

    #[test]
    fn factory_is_registered() {
        let factory = find_filter_by_name("pad").expect("pad filter not registered");
        assert_eq!(factory.descriptor().name, "pad");
        assert_eq!(factory.descriptor().description, "Pad the input video.");
        let mut filter = factory.create().unwrap();
        assert_eq!(filter.descriptor().name, "pad");
        filter.activate().unwrap();
    }

    #[test]
    fn side_data_survives_padding() {
        use wedeo_core::frame_side_data::{FrameSideData, FrameSideDataType};
        let mut input = make_yuv420(2, 2, 50, 128, 128);
        input.add_side_data(FrameSideData {
            data_type: FrameSideDataType::DisplayMatrix,
            data: vec![1, 2, 3],
        });
        let mut filter = PadFilter::new(4, 4, 1, 1);
        let out = filter.filter_frame(input).unwrap().remove(0);
        assert_eq!(out.side_data.len(), 1);
        assert_eq!(out.side_data[0].data, vec![1, 2, 3]);
    }

    #[test]
    fn unsupported_format_is_error() {
        let mut frame = Frame::new_video(2, 2, PixelFormat::Pal8);
        frame.data = FrameData::Video(VideoFrameData {
            planes: Vec::new(),
            width: 2,
            height: 2,
            format: PixelFormat::Pal8,
            picture_type: PictureType::None,
            color_range: ColorRange::Unspecified,
            color_space: ColorSpace::Unspecified,
            color_primaries: Default::default(),
            color_trc: Default::default(),
            chroma_location: Default::default(),
            sample_aspect_ratio: Rational::new(0, 1),
            crop_top: 0,
            crop_bottom: 0,
            crop_left: 0,
            crop_right: 0,
        });
        let mut filter = PadFilter::new(4, 4, 1, 1);
        assert_eq!(
            filter.filter_frame(frame).unwrap_err(),
            Error::InvalidArgument
        );
        let _ = NOPTS_VALUE;
    }
}
