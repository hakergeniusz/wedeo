// VP9 decoder — main entry point.
//
// Implements the `Decoder` trait for VP9 video by orchestrating:
//   1. Frame header parsing   (`header::decode_frame_header`)
//   2. Entropy / block decode (`block::decode_sb`)
//   3. Intra/inter reconstruction
//   4. Loop filtering         (`loopfilter::loop_filter_frame`)
//
// LGPL-2.1-or-later — same licence as FFmpeg.

use std::collections::VecDeque;
use std::sync::Arc;

use tracing::{debug, warn};
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
use wedeo_core::timestamp::NOPTS_VALUE;

use crate::block::{BlockInfo, TileDecodeContext, decode_sb};
use crate::bool_decoder::BoolDecoder;
use crate::context::{AboveContext, LeftContext};
use crate::data::DEFAULT_COEF_PROBS;
use crate::header::{FrameType, decode_frame_header};
use crate::loopfilter::loop_filter_frame;
use crate::prob::{CoefProbArray, CountContext, adapt_probs};
use crate::recon::{FrameBuffer, reconstruct_inter_block, reconstruct_intra_block};
use crate::refs::{MvRefPair, RefFrame, RefStore};
use crate::types::{BlockLevel, CompPredMode, ProbContext};

// ---------------------------------------------------------------------------
// Decoder state
// ---------------------------------------------------------------------------

/// VP9 video decoder.
pub struct Vp9Decoder {
    // Stored for future use (e.g. thread count, pixel format override).
    #[allow(dead_code)]
    params: CodecParameters,
    output_queue: VecDeque<Frame>,
    draining: bool,
    codec_descriptor: CodecDescriptor,
    /// 4 probability context slots indexed by `frame_ctx_id`.
    prob_ctx: [ProbContext; 4],
    /// 4 coefficient probability context slots indexed by `frame_ctx_id`.
    coef_ctx: [CoefProbArray; 4],
    /// Whether the previous frame was a keyframe (for adapt_probs update factor).
    last_keyframe: bool,
    /// Monotonic frame counter used for PTS when packets carry no PTS.
    frame_count: u64,
    /// Reference frame storage for inter prediction.
    ref_store: RefStore,
    /// Previous frame's `invisible` flag (FFmpeg: `s->s.h.invisible`).
    /// `invisible = !show_frame`, so `!last_invisible` means the previous
    /// frame was shown. FFmpeg vp9.c:568-571 uses this (not the *current*
    /// frame's show_frame) to derive `use_last_frame_mvs`.
    last_invisible: bool,
    /// Previous frame's width (for use_last_frame_mvs dimension check).
    last_width: u32,
    /// Previous frame's height.
    last_height: u32,
}

impl Vp9Decoder {
    /// Create a new VP9 decoder from codec parameters.
    pub fn new(params: CodecParameters) -> Result<Self> {
        Ok(Self {
            params,
            output_queue: VecDeque::new(),
            draining: false,
            codec_descriptor: CodecDescriptor {
                id: CodecId::Vp9,
                media_type: MediaType::Video,
                name: "vp9",
                long_name: "VP9 Video",
                properties: CodecProperties::LOSSY,
                profiles: &[],
            },
            prob_ctx: Default::default(),
            coef_ctx: [DEFAULT_COEF_PROBS; 4],
            last_keyframe: false,
            frame_count: 0,
            ref_store: RefStore::new(),
            // FFmpeg's `s->s.h.invisible` starts at 0, but its
            // `use_last_frame_mvs &= frames[CUR_FRAME].tf.f` check (vp9.c:641)
            // makes temporal MV prediction unavailable until a previous frame
            // exists. Starting at `true` (invisible) preserves that behaviour.
            last_invisible: true,
            last_width: 0,
            last_height: 0,
        })
    }

    /// Decode one VP9 frame from `data` and push the result to `output_queue`.
    fn decode_frame(&mut self, data: &[u8], pts: i64) -> Result<()> {
        // --- 1. Parse frame header ---
        let (header, _hdr_bytes) = decode_frame_header(data, &self.prob_ctx, &self.coef_ctx)?;

        debug!(
            frame_type = ?header.frame_type,
            show_frame = header.show_frame,
            show_existing = header.show_existing_frame,
            width = header.width,
            height = header.height,
            "VP9 frame header decoded"
        );

        // show_existing_frame: output a previously decoded reference frame.
        if header.show_existing_frame {
            let ref_idx = header.show_existing_frame_ref as usize;
            let rf = self.ref_store.slots[ref_idx]
                .as_ref()
                .ok_or(Error::InvalidData)?;
            let fb = &rf.fb;
            let mut frame = Frame::new_video(rf.width, rf.height, PixelFormat::Yuv420p);
            frame.pts = pts;
            if let FrameData::Video(ref mut video) = frame.data {
                video.planes = vec![
                    FramePlane {
                        buffer: Buffer::from_slice(&fb.y),
                        offset: 0,
                        linesize: fb.y_stride,
                    },
                    FramePlane {
                        buffer: Buffer::from_slice(&fb.u),
                        offset: 0,
                        linesize: fb.uv_stride,
                    },
                    FramePlane {
                        buffer: Buffer::from_slice(&fb.v),
                        offset: 0,
                        linesize: fb.uv_stride,
                    },
                ];
                video.picture_type = PictureType::P;
            }
            self.output_queue.push_back(frame);
            self.frame_count += 1;
            return Ok(());
        }

        // Only 8-bit 4:2:0 is fully supported for now.
        if header.bit_depth != 8 {
            warn!(
                bit_depth = header.bit_depth,
                "VP9 non-8-bit frame not yet supported"
            );
            return Ok(());
        }

        // On keyframe or error_resilient: reset all probability context slots to
        // defaults. Matches FFmpeg vp9.c:883-893.
        if header.frame_type == FrameType::KeyFrame || header.error_resilient {
            for slot in &mut self.prob_ctx {
                *slot = crate::header::default_prob_context();
            }
            self.coef_ctx = [crate::data::DEFAULT_COEF_PROBS; 4];
        }
        if header.frame_type == FrameType::KeyFrame {
            self.ref_store.clear();
        }

        // Resolve dimensions from reference frame if needed.
        let mut header = header;
        if header.size_from_ref >= 0 {
            let ri = header.ref_idx[header.size_from_ref as usize] as usize;
            if let Some(rf) = &self.ref_store.slots[ri] {
                header.width = rf.width;
                header.height = rf.height;
            } else {
                warn!("VP9: inter frame references empty slot for dimensions");
                return Ok(());
            }
        }

        // FFmpeg vp9.c:568-571, 641-643:
        //   last_invisible = s->s.h.invisible;                    (previous frame)
        //   s->s.h.invisible = !get_bits1(&s->gb);                (this frame)
        //   use_last_frame_mvs = !errorres && !last_invisible;
        //   use_last_frame_mvs &= prev_frame_exists && same_dimensions;
        header.use_last_frame_mvs = compute_use_last_frame_mvs(
            header.error_resilient,
            self.last_invisible,
            self.last_width == header.width && self.last_height == header.height,
        );

        // --- 2. Set up frame buffer and contexts ---
        let width = header.width;
        let height = header.height;
        let mut fb = FrameBuffer::new(width, height);

        // FFmpeg's coding grid is 8x8-based: `cols = (w + 7) >> 3`,
        // `rows = (h + 7) >> 3` (vp9.c:282-283). All frame-boundary
        // calculations (partition decisions, coefficient end_x/end_y,
        // context arrays) operate on this grid. Express it here in 4x4
        // units so the rest of the decoder can keep using 4x4 indices.
        // Using ceil(w/4) instead breaks for widths not divisible by 8.
        let cols_4x4 = (width as usize).div_ceil(8) * 2;
        let rows_4x4 = (height as usize).div_ceil(8) * 2;
        let sb_cols = cols_4x4.div_ceil(16);
        let sb_rows = rows_4x4.div_ceil(16);
        let is_kf = header.frame_type == FrameType::KeyFrame || header.intra_only;

        let mut above = AboveContext::new(cols_4x4, sb_cols);
        above.reset(is_kf);

        // Allocate per-4×4 MV grid for this frame.
        let mut cur_mv_grid = vec![MvRefPair::default(); rows_4x4 * cols_4x4];

        // --- 3. Locate tile data ---
        // `header.tile_data_offset` points to the first byte after the
        // compressed header (= start of tile bitstream data).
        let tile_data = &data[header.tile_data_offset..];

        // Tile grid dimensions.
        let tile_cols = 1usize << (header.tile_cols_log2 as usize);
        let tile_rows = 1usize << (header.tile_rows_log2 as usize);

        // For each tile-row × tile-col we need to know the byte range.
        // VP9 stores (tile_cols × tile_rows - 1) big-endian 32-bit sizes
        // before the actual tile data, with the last tile getting all
        // remaining bytes.
        let total_tiles = tile_cols * tile_rows;
        let mut tile_offsets: Vec<(usize, usize)> = Vec::with_capacity(total_tiles);
        {
            let mut read_pos = 0usize;
            for t in 0..total_tiles {
                let tile_size = if t == total_tiles - 1 {
                    tile_data.len() - read_pos
                } else {
                    if read_pos + 4 > tile_data.len() {
                        return Err(Error::InvalidData);
                    }
                    let sz =
                        u32::from_be_bytes(tile_data[read_pos..read_pos + 4].try_into().unwrap())
                            as usize;
                    read_pos += 4;
                    sz
                };
                tile_offsets.push((read_pos, tile_size));
                read_pos += tile_size;
            }
        }

        // Decode all tiles and collect decoded blocks + symbol counts.
        // When tile_cols > 1, tile columns within each tile row are decoded
        // in parallel using std::thread::scope. Each tile column writes to
        // a disjoint column range of AboveContext, so no data races.
        let mut all_blocks = Vec::new();
        let mut merged_counts = CountContext::default();

        for tile_row in 0..tile_rows {
            let tr_start = tile_sb_offset(tile_row, header.tile_rows_log2, sb_rows);
            let tr_end = tile_sb_offset(tile_row + 1, header.tile_rows_log2, sb_rows).min(sb_rows);

            if tile_cols <= 1 {
                // Fast path: single tile column — sequential, zero overhead.
                for tile_col in 0..tile_cols {
                    let (blocks, counts) = decode_tile_column(
                        tile_data,
                        &tile_offsets,
                        &header,
                        &mut above,
                        tile_row,
                        tile_col,
                        tile_cols,
                        tr_start,
                        tr_end,
                        sb_cols,
                        &mut cur_mv_grid,
                        self.ref_store.prev_frame_mvs.as_deref(),
                        self.ref_store.prev_cols_4x4,
                    )?;
                    all_blocks.extend(blocks);
                    merged_counts.merge(&counts);
                }
            } else {
                // Parallel tile columns via std::thread::scope.
                // Use raw pointers to avoid creating multiple &mut references
                // (UB under Rust aliasing rules even for disjoint access).
                let sync_above = SyncAbove(&mut above as *mut AboveContext);
                let sync_mv_grid = SyncMvGrid(&mut cur_mv_grid[..] as *mut [MvRefPair]);
                let prev_mvs = self.ref_store.prev_frame_mvs.as_deref();
                let prev_c4 = self.ref_store.prev_cols_4x4;

                // Pre-validate tile data ranges before spawning threads.
                for tile_col in 0..tile_cols {
                    let tile_idx = tile_row * tile_cols + tile_col;
                    let (tile_byte_start, tile_byte_len) = tile_offsets[tile_idx];
                    if tile_byte_start + tile_byte_len > tile_data.len() {
                        return Err(Error::InvalidData);
                    }
                }

                let results: Vec<Result<(Vec<BlockInfo>, CountContext)>> =
                    std::thread::scope(|s| {
                        let mut handles = Vec::with_capacity(tile_cols);

                        for tile_col in 0..tile_cols {
                            let above_ref = &sync_above;
                            let mv_ref = &sync_mv_grid;
                            let header_ref = &header;
                            let td = tile_data;
                            let offsets = &tile_offsets;

                            handles.push(s.spawn(move || {
                                // SAFETY: Each tile column writes to disjoint column
                                // ranges of AboveContext and MV grid.  Raw pointers
                                // avoid the UB of multiple &mut references.
                                let above = unsafe { &mut *above_ref.0 };
                                let mv_grid = unsafe { &mut *mv_ref.0 };
                                decode_tile_column(
                                    td, offsets, header_ref, above, tile_row, tile_col, tile_cols,
                                    tr_start, tr_end, sb_cols, mv_grid, prev_mvs, prev_c4,
                                )
                            }));
                        }

                        handles
                            .into_iter()
                            .map(|h| h.join().unwrap_or(Err(Error::InvalidData)))
                            .collect()
                    });

                for result in results {
                    let (blocks, counts) = result?;
                    all_blocks.extend(blocks);
                    merged_counts.merge(&counts);
                }
            }
        }

        // --- 4. Reconstruct each decoded block ---
        for block in &all_blocks {
            if block.is_inter {
                reconstruct_inter_block(&mut fb, block, &header, &self.ref_store.slots)?;
            } else {
                reconstruct_intra_block(&mut fb, block, &header, block.tile_col_start)?;
            }
        }

        // --- 5. Loop filter ---
        loop_filter_frame(&mut fb, &all_blocks, &header);

        // --- 6. Probability context adaptation ---
        // FFmpeg vp9.c:1726-1743: only update prob_ctx when refreshctx is set.
        let fctx = header.frame_ctx_id as usize;
        if header.refresh_ctx && header.parallel_mode {
            // Parallel mode: store compressed-header probs without adaptation.
            self.prob_ctx[fctx] = header.prob.clone();
            self.coef_ctx[fctx] = header.coef;
        } else if header.refresh_ctx && !header.parallel_mode {
            // Sequential mode: adapt probabilities using symbol counts.
            // adapt_probs modifies prob_ctx[fctx] in-place (starting from its
            // current value, NOT from header.prob), matching FFmpeg's behavior.
            let comp_pred_mode = CompPredMode::try_from(header.comp_pred_mode).unwrap_or_default();
            adapt_probs(
                &mut self.prob_ctx[fctx],
                &mut self.coef_ctx[fctx],
                &merged_counts,
                is_kf,
                self.last_keyframe,
                header.high_precision_mvs,
                header.filter_mode == 4,
                header.tx_mode == crate::header::TxMode::TxModeSelect,
                comp_pred_mode,
            );
            // FFmpeg vp9prob.c:68-73: on keyframe/intra-only, adapt_probs
            // returns early but first copies skip/tx probs from the decoded
            // frame's header back into prob_ctx.
            if is_kf || header.intra_only {
                self.prob_ctx[fctx].skip = header.prob.skip;
                self.prob_ctx[fctx].tx32p = header.prob.tx32p;
                self.prob_ctx[fctx].tx16p = header.prob.tx16p;
                self.prob_ctx[fctx].tx8p = header.prob.tx8p;
            }
        }
        // If !refresh_ctx: prob_ctx stays unchanged (matches FFmpeg line 1741).
        self.last_keyframe = header.frame_type == FrameType::KeyFrame;

        // --- 7. Reference frame management ---
        let ref_frame = Arc::new(RefFrame {
            fb: FrameBuffer {
                y: fb.y.clone(),
                u: fb.u.clone(),
                v: fb.v.clone(),
                y_stride: fb.y_stride,
                uv_stride: fb.uv_stride,
                width,
                height,
            },
            mv_grid: cur_mv_grid.clone(),
            width,
            height,
            cols_4x4,
            rows_4x4,
        });
        self.ref_store.refresh(header.refresh_ref_mask, ref_frame);
        self.ref_store.rotate_mvpair(cur_mv_grid, cols_4x4);

        // Track this frame's `invisible` bit for the *next* frame's
        // use_last_frame_mvs (FFmpeg vp9.c:569: invisible = !show_frame).
        self.last_invisible = !header.show_frame;
        self.last_width = width;
        self.last_height = height;

        // --- 7. Build output Frame (only if show_frame is set) ---
        if header.show_frame {
            let y_buf = Buffer::from_slice(&fb.y);
            let u_buf = Buffer::from_slice(&fb.u);
            let v_buf = Buffer::from_slice(&fb.v);
            let y_plane = FramePlane {
                buffer: y_buf,
                offset: 0,
                linesize: fb.y_stride,
            };
            let u_plane = FramePlane {
                buffer: u_buf,
                offset: 0,
                linesize: fb.uv_stride,
            };
            let v_plane = FramePlane {
                buffer: v_buf,
                offset: 0,
                linesize: fb.uv_stride,
            };

            let mut frame = Frame::new_video(width, height, PixelFormat::Yuv420p);
            frame.pts = pts;
            if let FrameData::Video(ref mut video) = frame.data {
                video.planes = vec![y_plane, u_plane, v_plane];
                video.picture_type = if is_kf {
                    PictureType::I
                } else {
                    PictureType::P
                };
            }
            // Only actual keyframes are KEY — intra_only frames are not
            // keyframes in VP9's reference model.
            if header.frame_type == FrameType::KeyFrame {
                frame.flags |= FrameFlags::KEY;
            }

            self.output_queue.push_back(frame);
            self.frame_count += 1;
        }

        Ok(())
    }
}

/// Derive `use_last_frame_mvs` exactly as FFmpeg vp9.c:568-571, 641-643 does.
///
/// FFmpeg reads the previous frame's `invisible` flag from persistent decoder
/// state (`last_invisible = s->s.h.invisible`); it does **not** use the current
/// frame's `show_frame`. `invisible` is `!show_frame`, so temporal MV
/// prediction is enabled only when the previous frame was shown, the current
/// frame is not error-resilient, and both frames share dimensions.
fn compute_use_last_frame_mvs(
    error_resilient: bool,
    last_invisible: bool,
    same_dimensions: bool,
) -> bool {
    !error_resilient && !last_invisible && same_dimensions
}

// ---------------------------------------------------------------------------
// Parallel tile column helpers
// ---------------------------------------------------------------------------

/// Raw pointer wrapper for `AboveContext` that implements `Send + Sync`.
///
/// SAFETY: VP9 tile columns write to disjoint column ranges of AboveContext.
/// Each tile column only accesses positions in [tile_col_start_4x4, tile_col_end_4x4).
/// We use raw pointers to avoid creating multiple `&mut` references (which
/// would be UB under Rust's aliasing rules even for disjoint access).
struct SyncAbove(*mut AboveContext);

// SAFETY: Disjoint column ranges — see SyncAbove doc comment.
unsafe impl Send for SyncAbove {}
unsafe impl Sync for SyncAbove {}

/// Raw pointer wrapper for `[MvRefPair]` that implements `Send + Sync`.
///
/// SAFETY: Each tile column writes to disjoint 4×4 column ranges of the MV grid.
struct SyncMvGrid(*mut [MvRefPair]);

// SAFETY: Disjoint column ranges.
unsafe impl Send for SyncMvGrid {}
unsafe impl Sync for SyncMvGrid {}

/// Decode all superblocks in one tile column for a given tile row.
/// Returns the decoded blocks.
#[allow(clippy::too_many_arguments)]
fn decode_tile_column(
    tile_data: &[u8],
    tile_offsets: &[(usize, usize)],
    header: &crate::header::FrameHeader,
    above: &mut AboveContext,
    tile_row: usize,
    tile_col: usize,
    tile_cols: usize,
    tr_start: usize,
    tr_end: usize,
    sb_cols: usize,
    cur_mv_grid: &mut [MvRefPair],
    prev_mv_grid: Option<&[MvRefPair]>,
    prev_cols_4x4: usize,
) -> Result<(Vec<BlockInfo>, CountContext)> {
    let tile_col_start_sb = tile_sb_offset(tile_col, header.tile_cols_log2, sb_cols);
    let tile_col_end_sb = tile_sb_offset(tile_col + 1, header.tile_cols_log2, sb_cols).min(sb_cols);
    let tile_col_start_4x4 = tile_col_start_sb * 16;

    let tile_idx = tile_row * tile_cols + tile_col;
    let (tile_byte_start, tile_byte_len) = tile_offsets[tile_idx];
    let tile_byte_end = tile_byte_start + tile_byte_len;
    if tile_byte_end > tile_data.len() {
        return Err(Error::InvalidData);
    }
    let tile_bytes = &tile_data[tile_byte_start..tile_byte_end];

    let mut bd = BoolDecoder::new(tile_bytes).ok_or(Error::InvalidData)?;
    // Marker bit — must be 0 for valid streams.
    if bd.get_prob(128) {
        return Err(Error::InvalidData);
    }

    let mut left = LeftContext::new();
    let mut tile_ctx = TileDecodeContext::new(
        bd,
        header,
        above,
        &mut left,
        tile_col_start_4x4,
        cur_mv_grid,
        prev_mv_grid,
        prev_cols_4x4,
    );

    for sb_row in tr_start..tr_end {
        let is_kf = header.frame_type == crate::header::FrameType::KeyFrame || header.intra_only;
        tile_ctx.left.reset(is_kf);
        for sb_col in tile_col_start_sb..tile_col_end_sb {
            let row = sb_row * 16;
            let col = sb_col * 16;
            decode_sb(&mut tile_ctx, row, col, BlockLevel::Bl64x64)?;
        }
    }

    let counts = std::mem::take(&mut tile_ctx.counts);
    let blocks = std::mem::take(&mut tile_ctx.blocks);
    Ok((blocks, counts))
}

// ---------------------------------------------------------------------------
// Tile layout helpers  (matches set_tile_offset in vp9.c)
// ---------------------------------------------------------------------------

/// Compute the start SB index for tile number `tile_num` with `log2_tiles`
/// columns/rows log2 and `n_sbs` total superblocks.
///
/// Matches FFmpeg's `set_tile_offset`: floor division via `>> log2_n`.
fn tile_sb_offset(tile_num: usize, log2_tiles: u8, n_sbs: usize) -> usize {
    ((tile_num * n_sbs) >> (log2_tiles as usize)).min(n_sbs)
}

// ---------------------------------------------------------------------------
// Superframe parsing  (VP9 Annex B)
// ---------------------------------------------------------------------------

/// Parse the `invisible` (i.e. `!show_frame`) bit out of a VP9 frame header.
///
/// Matches FFmpeg's `vp9_superframe_split.c:113-121`: the `invisible` flag is
/// only meaningful when `show_existing_frame` is not set. Bits are read MSB
/// first directly from the byte stream (uncompressed header prefix).
fn subframe_is_invisible(data: &[u8]) -> bool {
    fn rb(data: &[u8], pos: &mut usize) -> Option<u8> {
        if *pos >= data.len() * 8 {
            return None;
        }
        let bit = (data[*pos / 8] >> (7 - (*pos % 8))) & 1;
        *pos += 1;
        Some(bit)
    }

    let mut p = 0usize;
    // frame_marker (2 bits)
    if rb(data, &mut p).is_none() || rb(data, &mut p).is_none() {
        return false;
    }
    let lo = match rb(data, &mut p) {
        Some(v) => v,
        None => return false,
    };
    let hi = match rb(data, &mut p) {
        Some(v) => v,
        None => return false,
    };
    let profile = lo | (hi << 1);
    if profile == 3 && rb(data, &mut p).is_none() {
        return false;
    }
    let show_existing = match rb(data, &mut p) {
        Some(v) => v,
        None => return false,
    };
    if show_existing != 0 {
        return false;
    }
    // frame_type bit (ignored by FFmpeg).
    if rb(data, &mut p).is_none() {
        return false;
    }
    let show_frame = match rb(data, &mut p) {
        Some(v) => v,
        None => return false,
    };
    show_frame == 0
}

/// Parse a VP9 superframe index and return sub-frame slices together with a
/// flag telling whether each sub-frame is `invisible` (matches FFmpeg's
/// `vp9_superframe_split` BSF).
///
/// If the packet is not a superframe, returns a single-element vec containing
/// the entire packet (with `invisible = false`, i.e. the packet is passed
/// through unchanged).
fn parse_superframe(data: &[u8]) -> Vec<(&[u8], bool)> {
    let len = data.len();
    if len < 1 {
        return vec![(data, false)];
    }
    let marker = data[len - 1];
    // Superframe marker: top 3 bits = 0b110.
    if marker & 0xE0 != 0xC0 {
        return vec![(data, false)];
    }
    // FFmpeg vp9_superframe_split.c:59-60:
    //   length_size = 1 + ((marker >> 3) & 0x3);
    //   nb_frames   = 1 + (marker & 0x7);
    let length_size = 1 + ((marker >> 3) & 0x3) as usize;
    let num_frames = 1 + (marker & 0x7) as usize; // up to 8 frames
    let idx_size = 2 + num_frames * length_size; // leading marker + sizes + trailing marker
    if idx_size > len {
        return vec![(data, false)];
    }
    // Verify leading marker matches trailing marker.
    if data[len - idx_size] != marker {
        return vec![(data, false)];
    }
    // Parse per-frame sizes (little-endian, one byte per `length_size`).
    let mut frames = Vec::with_capacity(num_frames);
    let mut pos = len - idx_size + 1; // skip leading marker byte
    let mut data_pos = 0usize;
    for _ in 0..num_frames {
        let mut sz = 0usize;
        for j in 0..length_size {
            sz |= (data[pos] as usize) << (j * 8);
            pos += 1;
        }
        // FFmpeg: invalid frame size (<= 0) or sum exceeding the payload area.
        if sz == 0 || data_pos + sz > len - idx_size {
            // Malformed — fall back to treating as single frame.
            return vec![(data, false)];
        }
        let sub = &data[data_pos..data_pos + sz];
        let invisible = subframe_is_invisible(sub);
        frames.push((sub, invisible));
        data_pos += sz;
    }
    frames
}

// ---------------------------------------------------------------------------
// Decoder trait implementation
// ---------------------------------------------------------------------------

impl Decoder for Vp9Decoder {
    fn send_packet(&mut self, packet: Option<&Packet>) -> Result<()> {
        match packet {
            None => {
                // End-of-stream: switch to draining mode.
                self.draining = true;
                Ok(())
            }
            Some(pkt) => {
                let data = pkt.data.data();
                let pts = pkt.pts;
                // Parse VP9 superframe index to split compound packets.
                // FFmpeg's vp9_superframe_split BSF clears the PTS of
                // invisible (show_frame == 0) sub-frames.
                let sub_frames = parse_superframe(data);
                for (sub, invisible) in &sub_frames {
                    let sub_pts = if *invisible { NOPTS_VALUE } else { pts };
                    match self.decode_frame(sub, sub_pts) {
                        Ok(()) => {}
                        Err(Error::PatchwelcomeNotImplemented) => {
                            debug!("VP9: skipping unsupported frame");
                        }
                        Err(e) => return Err(e),
                    }
                }
                Ok(())
            }
        }
    }

    fn receive_frame(&mut self) -> Result<Frame> {
        if let Some(frame) = self.output_queue.pop_front() {
            return Ok(frame);
        }
        if self.draining {
            Err(Error::Eof)
        } else {
            Err(Error::Again)
        }
    }

    fn flush(&mut self) {
        self.output_queue.clear();
        self.draining = false;
        self.ref_store.clear();
        // No previous frame after a flush: keep temporal MV prediction off
        // until a frame has been decoded (matches FFmpeg's tf.f check).
        self.last_invisible = true;
    }

    fn descriptor(&self) -> &CodecDescriptor {
        &self.codec_descriptor
    }
}

// ---------------------------------------------------------------------------
// Factory & registry
// ---------------------------------------------------------------------------

struct Vp9DecoderFactory;

impl DecoderFactory for Vp9DecoderFactory {
    fn descriptor(&self) -> &DecoderDescriptor {
        static DESC: DecoderDescriptor = DecoderDescriptor {
            codec_id: CodecId::Vp9,
            name: "vp9",
            long_name: "VP9 Video",
            media_type: MediaType::Video,
            capabilities: CodecCapabilities::DR1,
            properties: CodecProperties::LOSSY,
            priority: 100,
        };
        &DESC
    }

    fn create(&self, params: CodecParameters) -> Result<Box<dyn Decoder>> {
        Ok(Box::new(Vp9Decoder::new(params)?))
    }
}

inventory::submit!(&Vp9DecoderFactory as &dyn DecoderFactory);

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a synthetic superframe the way FFmpeg's muxer does:
    /// `marker = 0xC0 | ((length_size - 1) << 3) | (nb_frames - 1)`.
    fn make_superframe(sizes: &[usize], length_size: usize, invisible: &[bool]) -> Vec<u8> {
        let nb = sizes.len();
        assert_eq!(nb, invisible.len());
        let marker = 0xC0u8 | (((length_size - 1) as u8) << 3) | ((nb - 1) as u8);
        let idx_size = 2 + nb * length_size;
        let payload_len: usize = sizes.iter().sum();
        let mut data = vec![0u8; payload_len + idx_size];

        let mut pos = 0usize;
        for (i, &sz) in sizes.iter().enumerate() {
            if sz >= 1 {
                // VP9 header prefix (MSB first): frame_marker=0b10,
                // profile=0b00, show_existing_frame=0, frame_type=0,
                // show_frame = !invisible.
                let show = u8::from(!invisible[i]);
                data[pos] = 0x80 | (show << 1);
            }
            pos += sz;
        }
        data[payload_len] = marker;
        let mut p = payload_len + 1;
        for &sz in sizes {
            for j in 0..length_size {
                data[p] = (sz >> (j * 8)) as u8;
                p += 1;
            }
        }
        data[p] = marker;
        data
    }

    #[test]
    fn superframe_swapped_bit_layout() {
        // nb_frames lives in bits 0..2, length_size in bits 3..4.
        let data = make_superframe(&[10, 20], 1, &[false, false]);
        let frames = parse_superframe(&data);
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].0.len(), 10);
        assert_eq!(frames[1].0.len(), 20);
        assert!(!frames[0].1);
        assert!(!frames[1].1);
    }

    #[test]
    fn superframe_eight_frames_two_byte_sizes() {
        let sizes = [4usize; 8];
        let data = make_superframe(&sizes, 2, &[false; 8]);
        let frames = parse_superframe(&data);
        assert_eq!(frames.len(), 8);
        for (sub, _) in &frames {
            assert_eq!(sub.len(), 4);
        }
    }

    #[test]
    fn superframe_invisible_subframe() {
        let data = make_superframe(&[10, 10], 1, &[false, true]);
        let frames = parse_superframe(&data);
        assert_eq!(frames.len(), 2);
        assert!(!frames[0].1);
        assert!(frames[1].1);
    }

    #[test]
    fn non_superframe_passthrough() {
        let data = [0x82u8, 1, 2, 3];
        let frames = parse_superframe(&data);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].0, &data);
        assert!(!frames[0].1);
    }

    #[test]
    fn malformed_zero_size_falls_back_to_passthrough() {
        let data = make_superframe(&[0, 10], 1, &[false, false]);
        let frames = parse_superframe(&data);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].0, &data[..]);
    }

    #[test]
    fn use_last_frame_mvs_uses_previous_invisible_not_current_show_frame() {
        // FFmpeg vp9.c:568-571: `use_last_frame_mvs = !errorres &&
        // !last_invisible`. A previous *invisible* frame (show_frame == 0)
        // disables temporal MV prediction regardless of the current frame's
        // show_frame bit.
        assert!(compute_use_last_frame_mvs(false, false, true));
        assert!(!compute_use_last_frame_mvs(false, true, true));
        assert!(!compute_use_last_frame_mvs(true, false, true));
        assert!(!compute_use_last_frame_mvs(false, false, false));
    }

    #[test]
    fn invisible_state_tracks_previous_frame_across_superframe() {
        // Simulate the decoder state update for a superframe whose second
        // subframe is invisible: [visible, invisible, visible]. Only the
        // frame after the visible one may use last-frame MVs.
        let mut last_invisible = true;
        let show_frames = [true, false, true];
        let expected = [false, true, false];
        for (show_frame, expected_use) in show_frames.into_iter().zip(expected) {
            let use_last = compute_use_last_frame_mvs(false, last_invisible, true);
            assert_eq!(use_last, expected_use);
            // vp9.c:569: invisible = !show_frame.
            last_invisible = !show_frame;
        }
    }
}
