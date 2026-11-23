//! MPEG-1 / MPEG-2 video decoder, translated from FFmpeg's
//! `mpeg12dec.c`, `mpegvideo_dec.c`, `mpegvideo_motion.c`,
//! `mpegvideo_unquantize.c` and `simple_idct_template.c`.

use std::collections::VecDeque;

use wedeo_codec::decoder::{CodecParameters, Decoder, DecoderDescriptor};
use wedeo_codec::descriptor::{CodecCapabilities, CodecDescriptor, CodecProperties};
use wedeo_codec::registry::DecoderFactory;
use wedeo_core::buffer::Buffer;
use wedeo_core::codec_id::CodecId;
use wedeo_core::error::{Error, Result};
use wedeo_core::frame::{Frame, FrameData, FrameFlags, FramePlane, PictureType};
use wedeo_core::media_type::MediaType;
use wedeo_core::packet::Packet;
use wedeo_core::pixel_format::PixelFormat;
use wedeo_core::rational::Rational;

use crate::bitreader::{BitReader, Vlc, sign_extend};
use crate::idct;
use crate::tables;

macro_rules! fail {
    () => {{ Err(Error::InvalidData) }};
}

const MB_TYPE_INTRA: i32 = 1 << 0;
const MB_TYPE_ZERO_MV: i32 = 1 << 9; // FFmpeg's MB_TYPE_CODEC_SPECIFIC
const MB_TYPE_CBP: i32 = 1 << 10;
const MB_TYPE_QUANT: i32 = 1 << 11;
const MB_TYPE_FORWARD_MV: i32 = 1 << 12;

const MV_DIR_FORWARD: i32 = 1;
const MV_DIR_BACKWARD: i32 = 2;

const PICT_TOP_FIELD: u8 = 1;
const PICT_BOTTOM_FIELD: u8 = 2;
const PICT_FRAME: u8 = 3;

// MB motion types (mpeg12dec.c)
const MT_FIELD: u32 = 1;
const MT_FRAME: u32 = 2;
const MT_DMV: u32 = 3;

#[derive(Clone)]
struct Picture {
    y: Vec<u8>,
    u: Vec<u8>,
    v: Vec<u8>,
    width: usize,
    height: usize,
    chroma_w: usize,
    chroma_h: usize,
    pict_type: u8,
}

impl Picture {
    fn new(width: usize, height: usize, chroma_format: usize, pict_type: u8) -> Self {
        let (chroma_w, chroma_h) = match chroma_format {
            3 => (width, height),
            2 => (width / 2, height),
            _ => (width / 2, height / 2),
        };
        // FFmpeg's picture buffers are padded; two extra rows let the 8x8
        // IDCT write a full `8 * stride` span even on the bottom block row.
        Self {
            y: vec![0; width * (height + 2)],
            u: vec![0; chroma_w * (chroma_h + 2)],
            v: vec![0; chroma_w * (chroma_h + 2)],
            width,
            height,
            chroma_w,
            chroma_h,
            pict_type,
        }
    }
}

/// Per-macroblock decoding state (`Mpeg12SliceContext` fields in FFmpeg).
struct MbState {
    intra: bool,
    cbp: i32,
    mv_dir: i32,
    mv_type: u32,
    field_select: [[usize; 2]; 2],
    mv: [[[i32; 2]; 4]; 2],
    interlaced_dct: bool,
    blocks: [[i16; 64]; 12],
}

impl Default for MbState {
    fn default() -> Self {
        Self {
            intra: false,
            cbp: 0,
            mv_dir: 0,
            mv_type: 0,
            field_select: [[0; 2]; 2],
            mv: [[[0; 2]; 4]; 2],
            interlaced_dct: false,
            blocks: [[0; 64]; 12],
        }
    }
}

pub struct Mpeg12Decoder {
    codec_id: CodecId,
    descriptor: CodecDescriptor,

    // Sequence header state
    width: usize,
    height: usize,
    mb_width: usize,
    mb_height: usize,
    chroma_format: usize, // 1 = 4:2:0, 2 = 4:2:2, 3 = 4:4:4
    progressive_sequence: bool,
    progressive_frame: bool,
    intra_matrix: [u16; 64],
    chroma_intra_matrix: [u16; 64],
    inter_matrix: [u16; 64],
    chroma_inter_matrix: [u16; 64],
    frame_rate_index: u8,
    aspect_ratio_info: u8,
    sequence_seen: bool,

    // Picture state
    pict_type: u8,
    temporal_ref: u16,
    full_pel: [bool; 2],
    f_code: [[u8; 2]; 2],
    intra_dc_precision: u8,
    picture_structure: u8,
    top_field_first: bool,
    frame_pred_frame_dct: bool,
    concealment_motion_vectors: bool,
    q_scale_type: bool,
    intra_vlc_format: bool,
    alternate_scan: bool,
    repeat_first_field: bool,
    closed_gop: bool,
    first_field: bool,

    // Per-slice / per-picture state
    qscale: i32,
    last_dc: [i32; 3],
    last_mv: [[[i32; 2]; 2]; 2],
    mb: MbState,

    // References
    last_pic: Option<Picture>,
    next_pic: Option<Picture>,
    cur_pic: Option<Picture>,
    picture_in_progress: bool,

    // Bitstream accumulation / streaming parser
    buf: Vec<u8>,
    pending_code: Option<u8>,
    pending_start: usize,
    scan_pos: usize,

    // Output
    frames: VecDeque<Frame>,
    draining: bool,
    packet_pts: i64,
}

impl Mpeg12Decoder {
    pub fn new(params: &CodecParameters) -> Result<Self> {
        let codec_id = params.codec_id;
        if !matches!(codec_id, CodecId::Mpeg1video | CodecId::Mpeg2video) {
            return Err(Error::InvalidArgument);
        }
        let descriptor = CodecDescriptor {
            id: codec_id,
            media_type: MediaType::Video,
            name: if codec_id == CodecId::Mpeg1video {
                "mpeg1video"
            } else {
                "mpeg2video"
            },
            long_name: if codec_id == CodecId::Mpeg1video {
                "MPEG-1 video"
            } else {
                "MPEG-2 video"
            },
            properties: CodecProperties::LOSSY,
            profiles: &[],
        };

        Ok(Self {
            codec_id,
            descriptor,
            width: 0,
            height: 0,
            mb_width: 0,
            mb_height: 0,
            chroma_format: 1,
            progressive_sequence: true,
            progressive_frame: true,
            intra_matrix: tables::MPEG1_DEFAULT_INTRA_MATRIX,
            chroma_intra_matrix: tables::MPEG1_DEFAULT_INTRA_MATRIX,
            inter_matrix: tables::MPEG1_DEFAULT_NON_INTRA_MATRIX,
            chroma_inter_matrix: tables::MPEG1_DEFAULT_NON_INTRA_MATRIX,
            frame_rate_index: 1,
            aspect_ratio_info: 1,
            sequence_seen: false,
            pict_type: 0,
            temporal_ref: 0,
            full_pel: [false; 2],
            f_code: [[15; 2]; 2],
            intra_dc_precision: 0,
            picture_structure: PICT_FRAME,
            top_field_first: false,
            frame_pred_frame_dct: true,
            concealment_motion_vectors: false,
            q_scale_type: false,
            intra_vlc_format: false,
            alternate_scan: false,
            repeat_first_field: false,
            closed_gop: false,
            first_field: false,
            qscale: 1,
            last_dc: [128; 3],
            last_mv: [[[0; 2]; 2]; 2],
            mb: MbState::default(),
            last_pic: None,
            next_pic: None,
            cur_pic: None,
            picture_in_progress: false,
            buf: Vec::new(),
            pending_code: None,
            pending_start: 0,
            scan_pos: 0,
            frames: VecDeque::new(),
            draining: false,
            packet_pts: wedeo_core::timestamp::NOPTS_VALUE,
        })
    }

    fn is_mpeg2(&self) -> bool {
        self.codec_id == CodecId::Mpeg2video
    }

    // ---------------------------------------------------------------------
    // Streaming start-code parser
    // ---------------------------------------------------------------------

    fn feed(&mut self, data: &[u8]) -> Result<()> {
        self.buf.extend_from_slice(data);

        while let Some((p, code)) = find_start_code(&self.buf, self.scan_pos) {
            if let Some(pc) = self.pending_code {
                // FFmpeg passes the whole remainder of the access unit to
                // mpeg_decode_slice(); it consumes exactly one MB row. Passing
                // only up to the next start code truncates slices whose last
                // MBs are followed by slice padding bits.
                let end = if (0x01..=0xaf).contains(&pc) {
                    self.buf.len()
                } else {
                    p
                };
                let payload = self.buf[self.pending_start..end].to_vec();
                self.process_start_code(pc, &payload)?;
            }
            self.pending_code = Some(code);
            self.pending_start = p + 4;
            self.scan_pos = p + 4;
        }

        if self.pending_start > 0 {
            self.buf.drain(..self.pending_start);
            self.pending_start = 0;
            self.scan_pos = 0;
        }
        Ok(())
    }

    fn finish(&mut self) -> Result<()> {
        if let Some(pc) = self.pending_code.take() {
            let end = self.buf.len();
            let payload = self.buf[self.pending_start..end].to_vec();
            self.process_start_code(pc, &payload)?;
        }
        self.finalize_picture()
    }

    fn process_start_code(&mut self, code: u8, payload: &[u8]) -> Result<()> {
        let is_slice = (0x01..=0xaf).contains(&code);
        if !is_slice {
            self.finalize_picture()?;
        }

        match code {
            0xb3 => self.decode_sequence(payload)?,
            0xb4 => {}
            0xb5 => self.decode_extension(payload)?,
            0xb6..=0xbf => {}
            0xb2 => {}
            0xb1 => self.decode_gop(payload),
            0x00 => self.decode_picture(payload)?,
            0x01..=0xaf => self.decode_slice(code, payload)?,
            _ => {}
        }
        Ok(())
    }

    // ---------------------------------------------------------------------
    // Headers
    // ---------------------------------------------------------------------

    fn decode_sequence(&mut self, payload: &[u8]) -> Result<()> {
        let mut gb = BitReader::new(payload);
        let width = gb.read_bits(12)? as usize;
        let height = gb.read_bits(12)? as usize;
        self.aspect_ratio_info = gb.read_bits(4)? as u8;
        self.frame_rate_index = gb.read_bits(4)? as u8;
        if self.frame_rate_index == 0 || self.frame_rate_index > 13 {
            self.frame_rate_index = 1;
        }
        let _bit_rate = gb.read_bits(18)?;
        let _marker = gb.read_bit()?;
        let _vbv_buffer = gb.read_bits(10)?;
        let _constraint = gb.read_bit()?;

        // load_matrix with idct_permutation == identity (simple IDCT).
        if gb.read_bit()? != 0 {
            self.load_matrix(&mut gb, true, true)?;
        } else {
            self.intra_matrix = tables::MPEG1_DEFAULT_INTRA_MATRIX;
            self.chroma_intra_matrix = tables::MPEG1_DEFAULT_INTRA_MATRIX;
        }
        if gb.read_bit()? != 0 {
            self.load_matrix(&mut gb, false, true)?;
        } else {
            self.inter_matrix = tables::MPEG1_DEFAULT_NON_INTRA_MATRIX;
            self.chroma_inter_matrix = tables::MPEG1_DEFAULT_NON_INTRA_MATRIX;
        }

        self.width = width;
        self.height = height;
        self.mb_width = width.div_ceil(16);
        self.mb_height = height.div_ceil(16);
        self.chroma_format = 1;
        self.progressive_sequence = true;
        self.progressive_frame = true;
        self.picture_structure = PICT_FRAME;
        self.first_field = false;
        self.frame_pred_frame_dct = true;
        self.codec_id = CodecId::Mpeg1video;
        self.sequence_seen = true;

        self.last_pic = None;
        self.next_pic = None;

        Ok(())
    }

    fn load_matrix(&mut self, gb: &mut BitReader<'_>, intra: bool, both: bool) -> Result<()> {
        let mut matrix = [0u16; 64];
        for i in 0..64 {
            let v = gb.read_bits(8)? as u16;
            if v == 0 {
                return fail!();
            }
            let v = if intra && i == 0 && v != 8 { 8 } else { v };
            let pos = tables::ZIGZAG_DIRECT[i] as usize;
            matrix[pos] = v;
        }
        if intra {
            self.chroma_intra_matrix = matrix;
            if both {
                self.intra_matrix = matrix;
            }
        } else {
            self.chroma_inter_matrix = matrix;
            if both {
                self.inter_matrix = matrix;
            }
        }
        Ok(())
    }

    fn decode_extension(&mut self, payload: &[u8]) -> Result<()> {
        let mut gb = BitReader::new(payload);
        let ext_id = gb.read_bits(4)?;
        match ext_id {
            0x1 => {
                let _escape = gb.read_bit()?;
                let _profile = gb.read_bits(3)?;
                let _level = gb.read_bits(4)?;
                self.progressive_sequence = gb.read_bit()? != 0;
                self.chroma_format = gb.read_bits(2)? as usize;
                if self.chroma_format == 0 {
                    self.chroma_format = 1;
                }
                let horiz_ext = gb.read_bits(2)? as usize;
                let vert_ext = gb.read_bits(2)? as usize;
                self.width |= horiz_ext << 12;
                self.height |= vert_ext << 12;
                self.mb_width = self.width.div_ceil(16);
                self.mb_height = self.height.div_ceil(16);
                let _bit_rate_ext = gb.read_bits(12)?;
                let _marker = gb.read_bit()?;
                let _vbv_ext = gb.read_bits(8)?;
                let _low_delay = gb.read_bit()?;
                let _fr_num_ext = gb.read_bits(2)?;
                let _fr_den_ext = gb.read_bits(5)?;
                self.codec_id = CodecId::Mpeg2video;
            }
            0x2 => {
                let _video_format = gb.read_bits(3)?;
                if gb.read_bit()? != 0 {
                    let _primaries = gb.read_bits(8)?;
                    let _trc = gb.read_bits(8)?;
                    let _matrix = gb.read_bits(8)?;
                }
                let _w = gb.read_bits(14)?;
                let _marker = gb.read_bit()?;
                let _h = gb.read_bits(14)?;
            }
            0x3 => {
                if gb.read_bit()? != 0 {
                    self.load_matrix(&mut gb, true, true)?;
                }
                if gb.read_bit()? != 0 {
                    self.load_matrix(&mut gb, false, true)?;
                }
                if gb.read_bit()? != 0 {
                    self.load_matrix(&mut gb, true, false)?;
                }
                if gb.read_bit()? != 0 {
                    self.load_matrix(&mut gb, false, false)?;
                }
            }
            0x7 => {
                let nofco = if self.progressive_sequence {
                    let mut n = 1;
                    if self.repeat_first_field {
                        n += 1;
                        if self.top_field_first {
                            n += 1;
                        }
                    }
                    n
                } else if self.picture_structure == PICT_FRAME {
                    let mut n = 1;
                    if self.repeat_first_field {
                        n += 1;
                    }
                    n
                } else {
                    1
                };
                for _ in 0..nofco {
                    let _x = gb.read_sbits(16)?;
                    let _marker = gb.read_bit()?;
                    let _y = gb.read_sbits(16)?;
                    let _marker = gb.read_bit()?;
                }
            }
            0x8 => {
                self.full_pel = [false; 2];
                for d in 0..2 {
                    for c in 0..2 {
                        let mut fc = gb.read_bits(4)? as u8;
                        if fc == 0 {
                            fc = 1;
                        }
                        self.f_code[d][c] = fc;
                    }
                }
                self.intra_dc_precision = gb.read_bits(2)? as u8;
                self.picture_structure = gb.read_bits(2)? as u8;
                self.top_field_first = gb.read_bit()? != 0;
                self.frame_pred_frame_dct = gb.read_bit()? != 0;
                self.concealment_motion_vectors = gb.read_bit()? != 0;
                self.q_scale_type = gb.read_bit()? != 0;
                self.intra_vlc_format = gb.read_bit()? != 0;
                self.alternate_scan = gb.read_bit()? != 0;
                self.repeat_first_field = gb.read_bit()? != 0;
                let _chroma_420_type = gb.read_bit()?;
                self.progressive_frame = gb.read_bit()? != 0;
            }
            _ => {}
        }
        Ok(())
    }

    fn decode_gop(&mut self, payload: &[u8]) {
        if payload.len() < 4 {
            return;
        }
        // time_code: 1 drop_frame_flag + 25 bits; then closed_gop, broken_link.
        let word = u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
        self.closed_gop = (word >> 6) & 1 != 0;
        self.first_field = false;
    }

    fn decode_picture(&mut self, payload: &[u8]) -> Result<()> {
        if !self.sequence_seen || self.width == 0 || self.height == 0 {
            return fail!();
        }
        let mut gb = BitReader::new(payload);
        self.temporal_ref = gb.read_bits(10)? as u16;
        self.pict_type = gb.read_bits(3)? as u8;
        if self.pict_type == 0 || self.pict_type > 3 {
            return fail!();
        }
        let _vbv_delay = gb.read_bits(16)?;
        if !self.is_mpeg2() {
            // MPEG-1: full_pel / f_code for forward and (for B) backward.
            if self.pict_type == 2 || self.pict_type == 3 {
                self.full_pel[0] = gb.read_bit()? != 0;
                let mut fc = gb.read_bits(3)? as u8;
                if fc == 0 {
                    fc = 1;
                }
                self.f_code[0] = [fc, fc];
            }
            if self.pict_type == 3 {
                self.full_pel[1] = gb.read_bit()? != 0;
                let mut fc = gb.read_bits(3)? as u8;
                if fc == 0 {
                    fc = 1;
                }
                self.f_code[1] = [fc, fc];
            }
            self.intra_dc_precision = 0;
            self.picture_structure = PICT_FRAME;
            self.top_field_first = false;
            self.frame_pred_frame_dct = true;
            self.concealment_motion_vectors = false;
            self.q_scale_type = false;
            self.intra_vlc_format = false;
            self.alternate_scan = false;
            self.repeat_first_field = false;
            self.progressive_frame = true;
            self.progressive_sequence = true;
        }
        Ok(())
    }

    fn finalize_picture(&mut self) -> Result<()> {
        if !self.picture_in_progress {
            return Ok(());
        }
        self.picture_in_progress = false;
        let Some(pic) = self.cur_pic.take() else {
            return Ok(());
        };
        // `next_pic` was installed as a blank placeholder at frame start;
        // replace it with the fully decoded picture (they alias in FFmpeg).
        if pic.pict_type != 3 {
            self.next_pic = Some(pic.clone());
        }
        let frame = self.picture_to_frame(&pic);
        self.frames.push_back(frame);
        Ok(())
    }

    // ---------------------------------------------------------------------
    // Slice / macroblock decoding
    // ---------------------------------------------------------------------

    fn start_picture(&mut self) -> Result<()> {
        if self.picture_in_progress {
            return Ok(());
        }
        let coded_w = self.mb_width * 16;
        let coded_h = self.mb_height * 16;
        let pic = Picture::new(coded_w, coded_h, self.chroma_format, self.pict_type);
        // FFmpeg's ff_mpv_frame_start(): for I/P frames the previous future
        // reference becomes the past reference, and the new picture becomes
        // the future reference for subsequent B frames.
        if self.pict_type != 3 {
            self.last_pic = self.next_pic.take();
            self.next_pic = Some(pic.clone());
        }
        self.cur_pic = Some(pic);
        self.picture_in_progress = true;
        self.qscale = 1;
        self.last_dc = [128 << self.intra_dc_precision; 3];
        self.last_mv = [[[0; 2]; 2]; 2];
        self.mb = MbState::default();
        Ok(())
    }

    fn decode_slice(&mut self, slice_code: u8, payload: &[u8]) -> Result<()> {
        if !self.sequence_seen || self.pict_type == 0 {
            return Ok(());
        }
        self.start_picture()?;
        let field_pic = self.picture_structure != PICT_FRAME;
        let mut mb_y = usize::from(slice_code - 1) << usize::from(field_pic);
        if self.picture_structure == PICT_BOTTOM_FIELD {
            mb_y += 1;
        }
        if mb_y >= self.mb_height {
            return Ok(());
        }

        let mut gb = BitReader::new(payload);
        if self.is_mpeg2() && self.mb_height > 2800 / 16 {
            gb.skip_bits(3);
        }
        self.qscale = mpeg_get_qscale(&mut gb, self.q_scale_type)?;
        if self.qscale == 0 {
            return fail!();
        }
        skip_1stop_8data_bits(&mut gb)?;

        let mut mb_x = 0usize;
        loop {
            let code = decode_vlc_ctx(&mut gb, tables::mb_incr_vlc(), "mb_incr")?;
            if code >= 33 {
                if code == 33 {
                    mb_x += 33;
                }
            } else {
                mb_x += code as usize;
                break;
            }
            if gb.bits_left() <= 0 {
                return fail!();
            }
        }
        if mb_x >= self.mb_width {
            return fail!();
        }

        self.last_dc = [128 << self.intra_dc_precision; 3];
        self.last_mv = [[[0; 2]; 2]; 2];

        let mut mb_skip_run = 0i32;
        'mb_loop: loop {
            self.decode_mb(&mut gb, mb_x, mb_y, &mut mb_skip_run)?;
            self.reconstruct_mb(mb_x, mb_y)?;
            mb_x += 1;

            if mb_x >= self.mb_width {
                mb_x = 0;
                mb_y += 1 + usize::from(field_pic);
                if mb_y >= self.mb_height {
                    // FFmpeg's end-of-slice leftover check: the remaining bits
                    // must be zero padding (or absent).
                    let left = gb.bits_left();
                    if left < 0 {
                        return fail!();
                    }
                    break 'mb_loop;
                }
                self.last_dc = [128 << self.intra_dc_precision; 3];
                self.last_mv = [[[0; 2]; 2]; 2];
            }

            if mb_skip_run == -1 {
                mb_skip_run = 0;
                loop {
                    let code = decode_vlc_ctx(&mut gb, tables::mb_incr_vlc(), "mb_incr")?;
                    if code >= 33 {
                        if code == 33 {
                            mb_skip_run += 33;
                        } else if code == 35 {
                            break 'mb_loop; // end of slice
                        }
                    } else {
                        mb_skip_run += code;
                        break;
                    }
                }
                if mb_skip_run > 0 {
                    if self.pict_type == 1 {
                        return fail!();
                    }
                    // A skipped macroblock: zero (P) or reused (B) vectors.
                    self.mb.intra = false;
                    self.mb.blocks = [[0; 64]; 12];
                    self.last_dc = [128 << self.intra_dc_precision; 3];
                    self.mb.mv_type = if self.picture_structure == PICT_FRAME {
                        0
                    } else {
                        3
                    };
                    if self.pict_type == 2 {
                        self.mb.mv_dir = MV_DIR_FORWARD;
                        self.mb.mv = [[[0; 2]; 4]; 2];
                        self.last_mv = [[[0; 2]; 2]; 2];
                    } else {
                        self.mb.mv[0][0] = self.last_mv[0][0];
                        self.mb.mv[1][0] = self.last_mv[1][0];
                    }
                }
            }
        }
        Ok(())
    }

    /// Translated from `mpeg_decode_mb` (mpeg12dec.c:406).
    fn decode_mb(
        &mut self,
        gb: &mut BitReader<'_>,
        _mb_x: usize,
        _mb_y: usize,
        mb_skip_run: &mut i32,
    ) -> Result<()> {
        let mb_block_count = 4 + (1 << self.chroma_format);

        if *mb_skip_run != 0 {
            *mb_skip_run -= 1;
            self.mb.intra = false;
            self.mb.blocks = [[0; 64]; 12];
            self.mb.mv = [[[0; 2]; 4]; 2];
            if self.pict_type == 2 {
                self.mb.mv_dir = MV_DIR_FORWARD;
                self.mb.mv_type = 0;
                self.last_mv = [[[0; 2]; 2]; 2];
            }
            return Ok(());
        }
        *mb_skip_run = -1;

        let mb_type = match self.pict_type {
            1 => {
                if gb.read_bit()? == 0 {
                    if gb.read_bit()? == 0 {
                        return fail!();
                    }
                    MB_TYPE_QUANT | MB_TYPE_INTRA
                } else {
                    MB_TYPE_INTRA
                }
            }
            2 => decode_vlc_ctx(gb, tables::mb_ptype_vlc(), "mb_ptype")?,
            3 => decode_vlc_ctx(gb, tables::mb_btype_vlc(), "mb_btype")?,
            _ => return fail!(),
        };
        if mb_type < 0 {
            return fail!();
        }

        self.mb.intra = mb_type & MB_TYPE_INTRA != 0;
        self.mb.cbp = 0;
        self.mb.interlaced_dct = false;
        self.mb.mv_dir = 0;
        self.mb.mv_type = 0;
        self.mb.field_select = [[0; 2]; 2];
        self.mb.mv = [[[0; 2]; 4]; 2];
        self.mb.blocks = [[0; 64]; 12];

        if self.mb.intra {
            if self.picture_structure == PICT_FRAME && !self.frame_pred_frame_dct {
                self.mb.interlaced_dct = gb.read_bit()? != 0;
            }
            if mb_type & MB_TYPE_QUANT != 0 {
                self.qscale = mpeg_get_qscale(gb, self.q_scale_type)?;
            }
            if self.concealment_motion_vectors {
                if self.picture_structure != PICT_FRAME {
                    let _ = gb.read_bit()?;
                }
                let mvx = mpeg_decode_motion(gb, self.f_code[0][0], self.last_mv[0][0][0])?;
                self.last_mv[0][0][0] = mvx;
                self.last_mv[0][1][0] = mvx;
                self.mb.mv[0][0][0] = mvx;
                let mvy = mpeg_decode_motion(gb, self.f_code[0][1], self.last_mv[0][0][1])?;
                self.last_mv[0][0][1] = mvy;
                self.last_mv[0][1][1] = mvy;
                self.mb.mv[0][0][1] = mvy;
                let _marker = gb.read_bit()?;
            } else {
                self.last_mv = [[[0; 2]; 2]; 2];
            }

            let scan = if self.alternate_scan {
                tables::ALTERNATE_VERTICAL_SCAN
            } else {
                tables::ZIGZAG_DIRECT
            };
            for i in 0..mb_block_count {
                self.mb.blocks[i] = if self.is_mpeg2() {
                    let m = if i < 4 {
                        self.intra_matrix
                    } else {
                        self.chroma_intra_matrix
                    };
                    decode_block_mpeg2_intra(
                        gb,
                        i,
                        &scan,
                        &m,
                        self.qscale,
                        self.intra_dc_precision,
                        self.intra_vlc_format,
                        &mut self.last_dc,
                    )?
                } else {
                    let m = self.intra_matrix;
                    decode_block_mpeg1_intra(gb, i, &scan, &m, self.qscale, &mut self.last_dc)?
                };
            }
        } else {
            if mb_type & MB_TYPE_ZERO_MV != 0 {
                self.mb.mv_dir = MV_DIR_FORWARD;
                if self.picture_structure == PICT_FRAME {
                    if !self.frame_pred_frame_dct {
                        self.mb.interlaced_dct = gb.read_bit()? != 0;
                    }
                    self.mb.mv_type = 0;
                } else {
                    self.mb.mv_type = 3;
                    self.mb.field_select[0][0] = usize::from(self.picture_structure - 1);
                }
                if mb_type & MB_TYPE_QUANT != 0 {
                    self.qscale = mpeg_get_qscale(gb, self.q_scale_type)?;
                }
                self.last_mv = [[[0; 2]; 2]; 2];
                self.mb.mv[0][0] = [0, 0];
            } else {
                self.mb.mv_dir = (mb_type >> 12) & 3;
                let (motion_type, interlaced_dct) =
                    if self.picture_structure == PICT_FRAME && self.frame_pred_frame_dct {
                        (MT_FRAME, false)
                    } else {
                        let mt = gb.read_bits(2)?;
                        let dct =
                            if self.picture_structure == PICT_FRAME && mb_type & MB_TYPE_CBP != 0 {
                                gb.read_bit()? != 0
                            } else {
                                false
                            };
                        (mt, dct)
                    };
                self.mb.interlaced_dct = interlaced_dct;
                if mb_type & MB_TYPE_QUANT != 0 {
                    self.qscale = mpeg_get_qscale(gb, self.q_scale_type)?;
                }

                match motion_type {
                    MT_FRAME => {
                        if self.picture_structure == PICT_FRAME {
                            self.mb.mv_type = 0;
                            for d in 0..2 {
                                if mb_type & (MB_TYPE_FORWARD_MV << d) != 0 {
                                    let mx = mpeg_decode_motion(
                                        gb,
                                        self.f_code[d][0],
                                        self.last_mv[d][0][0],
                                    )?;
                                    self.last_mv[d][0][0] = mx;
                                    self.last_mv[d][1][0] = mx;
                                    self.mb.mv[d][0][0] = mx;
                                    let my = mpeg_decode_motion(
                                        gb,
                                        self.f_code[d][1],
                                        self.last_mv[d][0][1],
                                    )?;
                                    self.last_mv[d][0][1] = my;
                                    self.last_mv[d][1][1] = my;
                                    self.mb.mv[d][0][1] = my;
                                    if self.full_pel[d] {
                                        self.mb.mv[d][0][0] *= 2;
                                        self.mb.mv[d][0][1] *= 2;
                                    }
                                }
                            }
                        } else {
                            self.mb.mv_type = 2;
                            for d in 0..2 {
                                if mb_type & (MB_TYPE_FORWARD_MV << d) != 0 {
                                    for j in 0..2 {
                                        self.mb.field_select[d][j] = gb.read_bit()? as usize;
                                        for k in 0..2 {
                                            let val = mpeg_decode_motion(
                                                gb,
                                                self.f_code[d][k],
                                                self.last_mv[d][j][k],
                                            )?;
                                            self.last_mv[d][j][k] = val;
                                            self.mb.mv[d][j][k] = val;
                                        }
                                    }
                                }
                            }
                        }
                    }
                    MT_FIELD => {
                        self.mb.mv_type = 3;
                        if self.picture_structure == PICT_FRAME {
                            for d in 0..2 {
                                if mb_type & (MB_TYPE_FORWARD_MV << d) != 0 {
                                    for j in 0..2 {
                                        self.mb.field_select[d][j] = gb.read_bit()? as usize;
                                        let val = mpeg_decode_motion(
                                            gb,
                                            self.f_code[d][0],
                                            self.last_mv[d][j][0],
                                        )?;
                                        self.last_mv[d][j][0] = val;
                                        self.mb.mv[d][j][0] = val;
                                        let val = mpeg_decode_motion(
                                            gb,
                                            self.f_code[d][1],
                                            self.last_mv[d][j][1] >> 1,
                                        )?;
                                        self.last_mv[d][j][1] = 2 * val;
                                        self.mb.mv[d][j][1] = val;
                                    }
                                }
                            }
                        } else {
                            for d in 0..2 {
                                if mb_type & (MB_TYPE_FORWARD_MV << d) != 0 {
                                    self.mb.field_select[d][0] = gb.read_bit()? as usize;
                                    for k in 0..2 {
                                        let val = mpeg_decode_motion(
                                            gb,
                                            self.f_code[d][k],
                                            self.last_mv[d][0][k],
                                        )?;
                                        self.last_mv[d][0][k] = val;
                                        self.last_mv[d][1][k] = val;
                                        self.mb.mv[d][0][k] = val;
                                    }
                                }
                            }
                        }
                    }
                    MT_DMV => {
                        self.mb.mv_type = 4;
                        for d in 0..2 {
                            if mb_type & (MB_TYPE_FORWARD_MV << d) != 0 {
                                let my_shift = usize::from(self.picture_structure == PICT_FRAME);
                                let mx = mpeg_decode_motion(
                                    gb,
                                    self.f_code[d][0],
                                    self.last_mv[d][0][0],
                                )?;
                                self.last_mv[d][0][0] = mx;
                                self.last_mv[d][1][0] = mx;
                                let dmx = get_dmv(gb)?;
                                let my = mpeg_decode_motion(
                                    gb,
                                    self.f_code[d][1],
                                    self.last_mv[d][0][1] >> my_shift,
                                )?;
                                let dmy = get_dmv(gb)?;
                                self.last_mv[d][0][1] = my * (1 << my_shift);
                                self.last_mv[d][1][1] = my * (1 << my_shift);
                                self.mb.mv[d][0][0] = mx;
                                self.mb.mv[d][0][1] = my;
                                if self.picture_structure == PICT_FRAME {
                                    let m = if self.top_field_first { 1 } else { 3 };
                                    self.mb.mv[d][2][0] = ((mx * m + i32::from(mx > 0)) >> 1) + dmx;
                                    self.mb.mv[d][2][1] =
                                        ((my * m + i32::from(my > 0)) >> 1) + dmy - 1;
                                    let m2 = 4 - m;
                                    self.mb.mv[d][3][0] =
                                        ((mx * m2 + i32::from(mx > 0)) >> 1) + dmx;
                                    self.mb.mv[d][3][1] =
                                        ((my * m2 + i32::from(my > 0)) >> 1) + dmy + 1;
                                } else {
                                    self.mb.mv[d][2][0] = ((mx + i32::from(mx > 0)) >> 1) + dmx;
                                    self.mb.mv[d][2][1] = ((my + i32::from(my > 0)) >> 1) + dmy;
                                    if self.picture_structure == PICT_TOP_FIELD {
                                        self.mb.mv[d][2][1] -= 1;
                                    } else {
                                        self.mb.mv[d][2][1] += 1;
                                    }
                                }
                            }
                        }
                    }
                    _ => return fail!(),
                }
            }

            self.last_dc = [128 << self.intra_dc_precision; 3];
            if mb_type & MB_TYPE_CBP != 0 {
                let mut cbp = decode_vlc_ctx(gb, tables::mb_pat_vlc(), "mb_pat")?;
                if mb_block_count > 6 {
                    cbp *= 1 << (mb_block_count - 6);
                    cbp |= gb.read_bits(mb_block_count - 6)? as i32;
                }
                if cbp <= 0 {
                    return fail!();
                }
                self.mb.cbp = cbp;
                let scan = if self.alternate_scan {
                    tables::ALTERNATE_VERTICAL_SCAN
                } else {
                    tables::ZIGZAG_DIRECT
                };
                let mut c = cbp;
                if self.is_mpeg2() {
                    c <<= 12 - mb_block_count;
                    for i in 0..mb_block_count {
                        if c & (1 << 11) != 0 {
                            let m = if i < 4 {
                                self.inter_matrix
                            } else {
                                self.chroma_inter_matrix
                            };
                            self.mb.blocks[i] =
                                decode_block_mpeg2_inter(gb, i, &scan, &m, self.qscale)?;
                        }
                        c = c.wrapping_shl(1);
                    }
                } else {
                    let m = self.inter_matrix;
                    for i in 0..6 {
                        if c & 32 != 0 {
                            self.mb.blocks[i] =
                                decode_block_mpeg1_inter(gb, i, &scan, &m, self.qscale)?;
                        }
                        c = c.wrapping_shl(1);
                    }
                }
            }
        }
        Ok(())
    }

    fn reconstruct_mb(&mut self, mb_x: usize, mb_y: usize) -> Result<()> {
        let mut pic = self.cur_pic.take().ok_or(Error::InvalidData)?;
        let px = mb_x * 16;
        let py = mb_y * 16;
        let stride = pic.width;
        let uv_stride = pic.chroma_w;

        if !self.mb.intra {
            self.motion_compensate(&mut pic, px, py)?;
        }

        let blocks = self.mb.blocks;
        let mb_block_count = 4 + (1 << self.chroma_format);
        let y_size = stride * pic.height;
        let uv_size = uv_stride * pic.chroma_h;
        for (i, b) in blocks.iter().enumerate().take(mb_block_count) {
            if i < 4 {
                let bx = px + (i % 2) * 8;
                let by = py + (i / 2) * 8;
                let start = by * stride + bx;
                if start + 7 * stride + 8 <= y_size {
                    let dest = &mut pic.y[start..start + 8 * stride];
                    if self.mb.intra {
                        idct::idct_put(b, dest, stride);
                    } else {
                        idct::idct_add(b, dest, stride);
                    }
                    if mb_x == 4 && mb_y == 1 && i == 3 {
                        let mut rows = Vec::new();
                        for r in 0..8 {
                            rows.push(dest[r * stride..r * stride + 8].to_vec());
                        }
                    }
                }
            } else {
                // Chroma block placement from FFmpeg's
                // mpv_reconstruct_mb_internal(): for 4:2:0 both chroma blocks
                // cover the whole MB at the same position; for 4:2:2/4:4:4
                // they are stacked/tiled.
                let bi = i - 4;
                let (plane_is_cb, bx, by) = match self.chroma_format {
                    1 => (bi == 0, px / 2, py / 2),
                    2 => (bi % 2 == 0, px / 2, py + (bi / 2) * 8),
                    _ => (bi % 2 == 0, px + (bi / 2 % 2) * 8, py + (bi / 4) * 8),
                };
                let start = by * uv_stride + bx;
                if start + 7 * uv_stride + 8 <= uv_size {
                    let plane = if plane_is_cb { &mut pic.u } else { &mut pic.v };
                    let dest = &mut plane[start..start + 8 * uv_stride];
                    if self.mb.intra {
                        idct::idct_put(b, dest, uv_stride);
                    } else {
                        idct::idct_add(b, dest, uv_stride);
                    }
                }
            }
            if mb_x == 4 && mb_y == 1 {}
        }
        self.cur_pic = Some(pic);
        Ok(())
    }

    fn motion_compensate(&self, pic: &mut Picture, px: usize, py: usize) -> Result<()> {
        let (have_forward, have_backward) = match self.pict_type {
            2 => (true, false),
            3 => (
                self.mb.mv_dir & MV_DIR_FORWARD != 0,
                self.mb.mv_dir & MV_DIR_BACKWARD != 0,
            ),
            _ => (false, false),
        };

        if have_forward {
            let Some(r) = self.last_pic.as_ref().or(self.next_pic.as_ref()) else {
                return Ok(());
            };
            self.do_motion(pic, r, px, py, 0, false)?;
        }
        if have_backward {
            let Some(r) = self.next_pic.as_ref() else {
                return Ok(());
            };
            // FFmpeg switches to avg_pixels only after a forward prediction
            // has written the destination; backward-only MBs use put_pixels.
            self.do_motion(pic, r, px, py, 1, have_forward)?;
        }
        Ok(())
    }

    fn do_motion(
        &self,
        pic: &mut Picture,
        r: &Picture,
        px: usize,
        py: usize,
        dir: usize,
        avg: bool,
    ) -> Result<()> {
        let stride = pic.width;
        let uv_stride = pic.chroma_w;
        let ref_stride = r.width;
        let ref_uv_stride = r.chroma_w;

        let (mx, my) = (self.mb.mv[dir][0][0], self.mb.mv[dir][0][1]);
        let sx_base = px as i32 + (mx >> 1);
        let sy_base = py as i32 + (my >> 1);
        if (sx_base as u32) >= (r.width as i32 - (mx & 1) - 15).max(0) as u32
            || (sy_base as u32) >= (r.height as i32 - (my & 1) - 15).max(0) as u32
        {
            return Ok(());
        }

        let dxy = (((my & 1) << 1) | (mx & 1)) as usize;
        mc_pixels(
            &r.y, ref_stride, sx_base, sy_base, &mut pic.y, stride, px, py, 16, 16, dxy, avg,
        );

        let cmx = mx / 2;
        let cmy = my / 2;
        let uv_dxy = (((cmy & 1) << 1) | (cmx & 1)) as usize;
        let csx = px as i32 / 2 + (cmx >> 1);
        let csy = py as i32 / 2 + (cmy >> 1);
        mc_pixels(
            &r.u,
            ref_uv_stride,
            csx,
            csy,
            &mut pic.u,
            uv_stride,
            px / 2,
            py / 2,
            8,
            8,
            uv_dxy,
            avg,
        );
        mc_pixels(
            &r.v,
            ref_uv_stride,
            csx,
            csy,
            &mut pic.v,
            uv_stride,
            px / 2,
            py / 2,
            8,
            8,
            uv_dxy,
            avg,
        );
        Ok(())
    }

    // ---------------------------------------------------------------------
    // Output
    // ---------------------------------------------------------------------

    fn picture_to_frame(&self, pic: &Picture) -> Frame {
        let width = self.width;
        let height = self.height;
        let format = match self.chroma_format {
            2 => PixelFormat::Yuv422p,
            3 => PixelFormat::Yuv444p,
            _ => PixelFormat::Yuv420p,
        };
        let chroma_w = match self.chroma_format {
            3 => width,
            _ => width / 2,
        };
        let chroma_h = match self.chroma_format {
            2 | 3 => height, // 4:2:2 and 4:4:4 keep full vertical resolution
            _ => height / 2,
        };

        let copy_plane = |src: &[u8], src_stride: usize, w: usize, h: usize| -> FramePlane {
            let mut data = Vec::with_capacity(w * h);
            for row in 0..h {
                let start = row * src_stride;
                data.extend_from_slice(&src[start..start + w]);
            }
            FramePlane {
                buffer: Buffer::from_slice(&data),
                offset: 0,
                linesize: w,
            }
        };

        let y_plane = copy_plane(&pic.y, pic.width, width, height);
        let u_plane = copy_plane(&pic.u, pic.chroma_w, chroma_w, chroma_h);
        let v_plane = copy_plane(&pic.v, pic.chroma_w, chroma_w, chroma_h);

        let mut frame = Frame::new_video(width as u32, height as u32, format);
        frame.pts = self.packet_pts;
        frame.best_effort_timestamp = self.packet_pts;
        frame.pkt_dts = self.packet_pts;
        frame.time_base = Rational::new(1, 90000);
        let pict_type = match pic.pict_type {
            1 => PictureType::I,
            2 => PictureType::P,
            3 => PictureType::B,
            _ => PictureType::None,
        };
        if let FrameData::Video(ref mut video) = frame.data {
            video.planes = vec![y_plane, u_plane, v_plane];
            video.picture_type = pict_type;
        }
        if pic.pict_type == 1 {
            frame.flags |= FrameFlags::KEY;
        }
        frame.repeat_pict = if self.repeat_first_field {
            if self.progressive_sequence {
                if self.top_field_first { 4 } else { 2 }
            } else if self.progressive_frame {
                1
            } else {
                0
            }
        } else {
            0
        };
        frame
    }
}

impl Decoder for Mpeg12Decoder {
    fn send_packet(&mut self, packet: Option<&Packet>) -> Result<()> {
        match packet {
            Some(pkt) => {
                self.packet_pts = pkt.pts;
                self.feed(pkt.data.data())
            }
            None => {
                self.draining = true;
                self.finish()
            }
        }
    }

    fn receive_frame(&mut self) -> Result<Frame> {
        if let Some(frame) = self.frames.pop_front() {
            Ok(frame)
        } else if self.draining {
            Err(Error::Eof)
        } else {
            Err(Error::Again)
        }
    }

    fn flush(&mut self) {
        self.buf.clear();
        self.pending_code = None;
        self.pending_start = 0;
        self.scan_pos = 0;
        self.frames.clear();
        self.draining = false;
        self.picture_in_progress = false;
        self.cur_pic = None;
        self.last_pic = None;
        self.next_pic = None;
        self.pict_type = 0;
        self.sequence_seen = false;
    }

    fn descriptor(&self) -> &CodecDescriptor {
        &self.descriptor
    }
}

// ---------------------------------------------------------------------------
// Coefficient decoding helpers
// ---------------------------------------------------------------------------

/// FFmpeg's `(level ^ SHOW_SBITS(gb, 1)) - SHOW_SBITS(gb, 1)`.
fn apply_sign(level: i32, sign_bit: u32) -> i32 {
    let s = if sign_bit != 0 { -1 } else { 0 };
    (level ^ s) - s
}

fn decode_dc(gb: &mut BitReader<'_>, component: usize, last_dc: &mut [i32; 3]) -> Result<i32> {
    let vlc = if component == 0 {
        tables::dc_lum_vlc()
    } else {
        tables::dc_chroma_vlc()
    };
    let size = gb.decode_vlc(vlc)?;
    if !(0..=11).contains(&size) {
        return fail!();
    }
    let diff = if size == 0 {
        0
    } else {
        gb.read_xbits(size as usize)?
    };
    let dc = last_dc[component] + diff;
    last_dc[component] = dc;
    Ok(dc)
}

fn decode_block_mpeg1_intra(
    gb: &mut BitReader<'_>,
    n: usize,
    scan: &[u8; 64],
    matrix: &[u16; 64],
    qscale: i32,
    last_dc: &mut [i32; 3],
) -> Result<[i16; 64]> {
    let component = n.saturating_sub(3);
    let dc = decode_dc(gb, component, last_dc)?;
    let mut block = [0i16; 64];
    block[0] = (dc * matrix[0] as i32) as i16;

    if gb.show_bits(2) == 2 {
        gb.skip_bits(2);
        return Ok(block);
    }
    let mut i = 0usize;
    loop {
        let (level, run) = decode_rl(gb, tables::rl_mpeg1_vlc())?;
        if level == 127 {
            break;
        }
        if level != 0 {
            i += run as usize;
            if i > 63 {
                break;
            }
            let j = scan[i] as usize;
            let mut l = (level * qscale * matrix[j] as i32) >> 4;
            l = (l - 1) | 1;
            l = apply_sign(l, gb.read_bit()?);
            block[j] = l as i16;
        } else {
            let run_esc = gb.read_bits(6)? as usize + 1;
            let mut level_esc = gb.read_sbits(8)?;
            if level_esc == -128 {
                level_esc = gb.read_bits(8)? as i32 - 256;
            } else if level_esc == 0 {
                level_esc = gb.read_bits(8)? as i32;
            }
            i += run_esc;
            if i > 63 {
                break;
            }
            let j = scan[i] as usize;
            let l = if level_esc < 0 {
                -(((-level_esc * qscale * matrix[j] as i32) >> 4) | 1)
            } else {
                ((level_esc * qscale * matrix[j] as i32) >> 4) | 1
            };
            block[j] = l as i16;
        }
        if gb.show_bits(2) == 2 {
            gb.skip_bits(2);
            break;
        }
    }
    if i > 63 {
        return fail!();
    }
    Ok(block)
}

// Mirrors FFmpeg's `mpeg2_decode_block_intra` signature; keeping the
// arguments explicit makes the port easy to diff against the C source.
#[allow(clippy::too_many_arguments)]
fn decode_block_mpeg2_intra(
    gb: &mut BitReader<'_>,
    n: usize,
    scan: &[u8; 64],
    matrix: &[u16; 64],
    qscale: i32,
    dc_precision: u8,
    intra_vlc_format: bool,
    last_dc: &mut [i32; 3],
) -> Result<[i16; 64]> {
    let component = if n < 4 { 0 } else { (n & 1) + 1 };
    let dc = decode_dc(gb, component, last_dc)?;
    let mut block = [0i16; 64];
    block[0] = (dc * (1 << (3 - dc_precision))) as i16;
    let mut mismatch = block[0] as i32 ^ 1;

    let rl = if intra_vlc_format {
        tables::rl_mpeg2_vlc()
    } else {
        tables::rl_mpeg1_vlc()
    };
    let mut i = 0usize;
    loop {
        let (level, run) = decode_rl(gb, rl)?;
        if level == 127 {
            break;
        }
        if level != 0 {
            i += run as usize;
            if i > 63 {
                break;
            }
            let j = scan[i] as usize;
            let mut l = (level * qscale * matrix[j] as i32) >> 4;
            l = apply_sign(l, gb.read_bit()?);
            block[j] = l as i16;
            mismatch ^= l;
        } else {
            let run_esc = gb.read_bits(6)? as usize + 1;
            let level_esc = gb.read_sbits(12)?;
            i += run_esc;
            if i > 63 {
                break;
            }
            let j = scan[i] as usize;
            let l = if level_esc < 0 {
                -((-level_esc * qscale * matrix[j] as i32) >> 4)
            } else {
                (level_esc * qscale * matrix[j] as i32) >> 4
            };
            block[j] = l as i16;
            mismatch ^= l;
        }
    }
    if i > 63 {
        return fail!();
    }
    block[63] ^= (mismatch & 1) as i16;
    Ok(block)
}

fn decode_block_mpeg2_inter(
    gb: &mut BitReader<'_>,
    _n: usize,
    scan: &[u8; 64],
    matrix: &[u16; 64],
    qscale: i32,
) -> Result<[i16; 64]> {
    let mut block = [0i16; 64];
    let mut mismatch = 1i32;
    let mut i: i32 = -1;

    if gb.show_bits(2) & 0x2 != 0 {
        let mut level = (3 * qscale * matrix[0] as i32) >> 5;
        if gb.show_bits(2) & 0x1 != 0 {
            level = -level;
        }
        block[0] = level as i16;
        mismatch ^= level;
        i = 0;
        gb.skip_bits(2);
        if gb.show_bits(2) == 2 {
            gb.skip_bits(2);
            block[63] ^= (mismatch & 1) as i16;
            return Ok(block);
        }
    }

    loop {
        let (level, run) = decode_rl(gb, tables::rl_mpeg1_vlc())?;
        if level != 0 {
            i += run;
            if i > 63 {
                break;
            }
            let j = scan[i as usize] as usize;
            let mut l = ((level * 2 + 1) * qscale * matrix[j] as i32) >> 5;
            l = apply_sign(l, gb.read_bit()?);
            block[j] = l as i16;
            mismatch ^= l;
        } else {
            let run_esc = gb.read_bits(6)? as i32 + 1;
            let level_esc = gb.read_sbits(12)?;
            i += run_esc;
            if i > 63 {
                break;
            }
            let j = scan[i as usize] as usize;
            let l = if level_esc < 0 {
                -(((-level_esc * 2 + 1) * qscale * matrix[j] as i32) >> 5)
            } else {
                ((level_esc * 2 + 1) * qscale * matrix[j] as i32) >> 5
            };
            block[j] = l as i16;
            mismatch ^= l;
        }
        if gb.show_bits(2) == 2 {
            gb.skip_bits(2);
            break;
        }
    }
    if i > 63 {
        return fail!();
    }
    block[63] ^= (mismatch & 1) as i16;
    Ok(block)
}

fn decode_block_mpeg1_inter(
    gb: &mut BitReader<'_>,
    _n: usize,
    scan: &[u8; 64],
    matrix: &[u16; 64],
    qscale: i32,
) -> Result<[i16; 64]> {
    let mut block = [0i16; 64];
    let mut i: i32 = -1;

    if gb.show_bits(2) & 0x2 != 0 {
        let mut level = (3 * qscale * matrix[0] as i32) >> 5;
        level = (level - 1) | 1;
        if gb.show_bits(2) & 0x1 != 0 {
            level = -level;
        }
        block[0] = level as i16;
        i = 0;
        gb.skip_bits(2);
        if gb.show_bits(2) == 2 {
            gb.skip_bits(2);
            return Ok(block);
        }
    }

    loop {
        let (level, run) = decode_rl(gb, tables::rl_mpeg1_vlc())?;
        if level != 0 {
            i += run;
            if i > 63 {
                break;
            }
            let j = scan[i as usize] as usize;
            let mut l = ((level * 2 + 1) * qscale * matrix[j] as i32) >> 5;
            l = (l - 1) | 1;
            l = apply_sign(l, gb.read_bit()?);
            block[j] = l as i16;
        } else {
            let run_esc = gb.read_bits(6)? as i32 + 1;
            let mut level_esc = gb.read_sbits(8)?;
            if level_esc == -128 {
                level_esc = gb.read_bits(8)? as i32 - 256;
            } else if level_esc == 0 {
                level_esc = gb.read_bits(8)? as i32;
            }
            i += run_esc;
            if i > 63 {
                break;
            }
            let j = scan[i as usize] as usize;
            let l = if level_esc < 0 {
                -((((-level_esc * 2 + 1) * qscale * matrix[j] as i32) >> 5) | 1)
            } else {
                (((level_esc * 2 + 1) * qscale * matrix[j] as i32) >> 5) | 1
            };
            block[j] = l as i16;
        }
        if gb.show_bits(2) == 2 {
            gb.skip_bits(2);
            break;
        }
    }
    if i > 63 {
        return fail!();
    }
    Ok(block)
}

// ---------------------------------------------------------------------------
// Smaller helpers
// ---------------------------------------------------------------------------

fn decode_vlc_ctx(gb: &mut BitReader<'_>, vlc: &Vlc, _what: &str) -> Result<i32> {
    vlc.decode(gb)
}

fn find_start_code(buf: &[u8], from: usize) -> Option<(usize, u8)> {
    if buf.len() < 4 || from + 4 > buf.len() {
        return None;
    }
    let mut i = from;
    while i + 3 < buf.len() {
        if buf[i] == 0 && buf[i + 1] == 0 && buf[i + 2] == 1 {
            return Some((i, buf[i + 3]));
        }
        i += 1;
    }
    None
}

fn mpeg_get_qscale(gb: &mut BitReader<'_>, q_scale_type: bool) -> Result<i32> {
    let q = gb.read_bits(5)? as usize;
    if q_scale_type {
        Ok(tables::NON_LINEAR_QSCALE[q] as i32)
    } else {
        Ok((q << 1) as i32)
    }
}

fn get_dmv(gb: &mut BitReader<'_>) -> Result<i32> {
    if gb.read_bit()? != 0 {
        Ok(1 - (gb.read_bit()? as i32) * 2)
    } else {
        Ok(0)
    }
}

fn mpeg_decode_motion(gb: &mut BitReader<'_>, fcode: u8, pred: i32) -> Result<i32> {
    let code = gb.decode_vlc(tables::mv_vlc())?;
    if code == 0 {
        return Ok(pred);
    }
    if code < 0 {
        return Ok(0xffff);
    }
    let sign = gb.read_bit()?;
    let shift = (fcode - 1) as usize;
    let mut val = code;
    if shift != 0 {
        val = (val - 1) << shift;
        val |= gb.read_bits(shift)? as i32;
        val += 1;
    }
    if sign != 0 {
        val = -val;
    }
    val += pred;
    Ok(sign_extend(val as u32, 5 + shift))
}

fn decode_rl(gb: &mut BitReader<'_>, vlc: &Vlc) -> Result<(i32, i32)> {
    let idx = gb.decode_vlc(vlc)?;
    let n = 111i32;
    if idx == n {
        Ok((0, 66))
    } else if idx == n + 1 {
        Ok((127, 0))
    } else {
        let level = tables::RL_LEVEL[idx as usize] as i32;
        let run = tables::RL_RUN[idx as usize] as i32 + 1;
        Ok((level, run))
    }
}

fn skip_1stop_8data_bits(gb: &mut BitReader<'_>) -> Result<()> {
    for _ in 0..32 {
        if gb.read_bit()? == 0 {
            return Ok(());
        }
        gb.skip_bits(8);
    }
    Ok(())
}

// Mirrors FFmpeg's `put_pixels_*` signature (src/dst geometry + mode).
#[allow(clippy::too_many_arguments)]
fn mc_pixels(
    src: &[u8],
    src_stride: usize,
    src_x: i32,
    src_y: i32,
    dst: &mut [u8],
    dst_stride: usize,
    dst_x: usize,
    dst_y: usize,
    w: usize,
    h: usize,
    dxy: usize,
    avg: bool,
) {
    for row in 0..h {
        let sy = (src_y + row as i32) as usize;
        let dy = dst_y + row;
        for col in 0..w {
            let sx = (src_x + col as i32) as usize;
            let a = src[sy * src_stride + sx] as i32;
            let v = match dxy {
                0 => a,
                1 => (a + src[sy * src_stride + sx + 1] as i32 + 1) >> 1,
                2 => (a + src[(sy + 1) * src_stride + sx] as i32 + 1) >> 1,
                3 => {
                    let b = src[sy * src_stride + sx + 1] as i32;
                    let c = src[(sy + 1) * src_stride + sx] as i32;
                    let d = src[(sy + 1) * src_stride + sx + 1] as i32;
                    (a + b + c + d + 2) >> 2
                }
                _ => a,
            };
            let p = &mut dst[dy * dst_stride + dst_x + col];
            *p = if avg {
                ((*p as i32 + v + 1) >> 1) as u8
            } else {
                v as u8
            };
        }
    }
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

struct Mpeg12DecoderFactory;

impl DecoderFactory for Mpeg12DecoderFactory {
    fn descriptor(&self) -> &DecoderDescriptor {
        static DESC: DecoderDescriptor = DecoderDescriptor {
            codec_id: CodecId::Mpeg2video,
            name: "mpeg2video",
            long_name: "MPEG-2 video",
            media_type: MediaType::Video,
            capabilities: CodecCapabilities::DR1,
            properties: CodecProperties::LOSSY,
            priority: 100,
        };
        &DESC
    }

    fn create(&self, params: CodecParameters) -> Result<Box<dyn Decoder>> {
        Ok(Box::new(Mpeg12Decoder::new(&params)?))
    }
}

inventory::submit!(&Mpeg12DecoderFactory as &dyn DecoderFactory);

struct Mpeg1DecoderFactory;

impl DecoderFactory for Mpeg1DecoderFactory {
    fn descriptor(&self) -> &DecoderDescriptor {
        static DESC: DecoderDescriptor = DecoderDescriptor {
            codec_id: CodecId::Mpeg1video,
            name: "mpeg1video",
            long_name: "MPEG-1 video",
            media_type: MediaType::Video,
            capabilities: CodecCapabilities::DR1,
            properties: CodecProperties::LOSSY,
            priority: 100,
        };
        &DESC
    }

    fn create(&self, params: CodecParameters) -> Result<Box<dyn Decoder>> {
        Ok(Box::new(Mpeg12Decoder::new(&params)?))
    }
}

inventory::submit!(&Mpeg1DecoderFactory as &dyn DecoderFactory);
