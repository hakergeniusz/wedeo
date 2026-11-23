// Thread-safe picture with row-level progress tracking for frame-level threading.
//
// One writer thread decodes rows top-to-bottom, calling `publish_row()` after
// each MB row. Reader threads (decoding later frames) call `wait_for_row()`
// before accessing reference pixels.
//
// Reference: FFmpeg libavcodec/pthread_frame.c (ff_thread_report_progress,
// ff_thread_await_progress), WEDEO_SYNTHESIS.md §4 (hybrid spin→condvar).

use std::cell::UnsafeCell;
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use crate::deblock::PictureBuffer;

/// Progress sentinel: all rows decoded + deblocked.
pub const PROGRESS_COMPLETE: i32 = i32::MAX;

// ---------------------------------------------------------------------------
// BufferPool — reuse PictureBuffer allocations across frames
// ---------------------------------------------------------------------------

/// Geometry and sample format requested from the buffer pool.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PictureFormat {
    pub width: u32,
    pub height: u32,
    pub mb_width: u32,
    pub mb_height: u32,
    pub uv_fill: u8,
    pub bit_depth: u8,
    pub chroma_format_idc: u8,
}

/// Pool of reusable PictureBuffers to avoid per-frame malloc/free.
///
/// Lives on `H264Decoder`. On resolution change, the pool is flushed
/// (mismatched buffers are dropped, not recycled).
pub struct BufferPool {
    free: Vec<PictureBuffer>,
    width: u32,
    height: u32,
    bit_depth: u8,
    chroma_format_idc: u8,
}

impl Default for BufferPool {
    fn default() -> Self {
        Self::new()
    }
}

impl BufferPool {
    pub fn new() -> Self {
        Self {
            free: Vec::new(),
            width: 0,
            height: 0,
            bit_depth: 8,
            chroma_format_idc: 1,
        }
    }

    /// Acquire a buffer of the given dimensions, pre-filled with `y_fill` / `uv_fill`.
    /// Returns a pooled buffer if one is available and dimensions match;
    /// otherwise allocates fresh.
    pub fn acquire(&mut self, format: PictureFormat) -> PictureBuffer {
        let PictureFormat {
            width,
            height,
            mb_width,
            mb_height,
            uv_fill,
            bit_depth,
            chroma_format_idc,
        } = format;

        // Resolution or sample-format change: flush all pooled buffers.
        if width != self.width
            || height != self.height
            || bit_depth != self.bit_depth
            || chroma_format_idc != self.chroma_format_idc
        {
            self.free.clear();
            self.width = width;
            self.height = height;
            self.bit_depth = bit_depth;
            self.chroma_format_idc = chroma_format_idc;
        }

        // Chroma geometry from H.264 Table 6-1. 4:2:2 has full chroma height;
        // 4:4:4 has full chroma width as well.
        let (shift_w, shift_h) = match chroma_format_idc {
            1 => (1, 1),
            2 => (1, 0),
            3 => (0, 0),
            _ => (1, 1),
        };
        let uv_stride = (width >> shift_w) as usize;
        let uv_height = (height >> shift_h) as usize;

        // Mid-grey for high bit depth is 1 << (bit_depth - 1), not 128.
        let uv_fill16 = if bit_depth > 8 {
            1u16 << (bit_depth - 1)
        } else {
            u16::from(uv_fill)
        };

        if let Some(mut buf) = self.free.pop() {
            // Reuse: fill with zeros (luma) and uv_fill (chroma)
            buf.y.fill(0);
            buf.u.fill(uv_fill);
            buf.v.fill(uv_fill);
            if buf.is_high_depth() {
                buf.y16.fill(0);
                buf.u16.fill(uv_fill16);
                buf.v16.fill(uv_fill16);
            }
            buf
        } else {
            // Fresh allocation. High bit depths also get full-precision planes.
            let y_stride = width as usize;
            let (y16, u16, v16) = if bit_depth > 8 {
                (
                    vec![0u16; y_stride * height as usize],
                    vec![uv_fill16; uv_stride * uv_height],
                    vec![uv_fill16; uv_stride * uv_height],
                )
            } else {
                (Vec::new(), Vec::new(), Vec::new())
            };
            PictureBuffer {
                y: vec![0u8; y_stride * height as usize],
                u: vec![uv_fill; uv_stride * uv_height],
                v: vec![uv_fill; uv_stride * uv_height],
                y16,
                u16,
                v16,
                y_stride,
                uv_stride,
                width,
                height,
                mb_width,
                mb_height,
                bit_depth,
                chroma_format_idc,
            }
        }
    }

    /// Return a buffer to the pool for reuse.
    pub fn reclaim(&mut self, buf: PictureBuffer) {
        // Only keep buffers that match current dimensions
        if buf.width == self.width
            && buf.height == self.height
            && buf.bit_depth == self.bit_depth
            && buf.chroma_format_idc == self.chroma_format_idc
            && !buf.y.is_empty()
        {
            self.free.push(buf);
        }
    }
}

/// Thread-safe picture with row-level progress tracking.
///
/// SAFETY: Concurrent access is mediated by `row_progress`:
/// - Writer has exclusive access to rows > row_progress
/// - Readers may access rows <= row_progress.load(Acquire)
pub struct SharedPicture {
    data: UnsafeCell<PictureBuffer>,
    row_progress: AtomicI32,
    mb_height: u32,
    // For blocking wait when spin is insufficient
    progress_mutex: Mutex<()>,
    progress_cond: Condvar,
    /// If set, the buffer returns to this pool when the last Arc drops.
    return_to: Option<Arc<Mutex<BufferPool>>>,
}

// SAFETY: Concurrent access is mediated by `row_progress` atomic.
// Writer has exclusive access to rows > row_progress; readers only
// access rows <= row_progress.load(Acquire).
unsafe impl Sync for SharedPicture {}

impl SharedPicture {
    pub fn new(pic: PictureBuffer) -> Arc<Self> {
        let mb_height = pic.mb_height;
        Arc::new(Self {
            data: UnsafeCell::new(pic),
            row_progress: AtomicI32::new(-1),
            mb_height,
            progress_mutex: Mutex::new(()),
            progress_cond: Condvar::new(),
            return_to: None,
        })
    }

    /// Create a SharedPicture that returns its buffer to `pool` on drop.
    pub fn new_pooled(pic: PictureBuffer, pool: &Arc<Mutex<BufferPool>>) -> Arc<Self> {
        let mb_height = pic.mb_height;
        Arc::new(Self {
            data: UnsafeCell::new(pic),
            row_progress: AtomicI32::new(-1),
            mb_height,
            progress_mutex: Mutex::new(()),
            progress_cond: Condvar::new(),
            return_to: Some(Arc::clone(pool)),
        })
    }

    /// Get a shared reference to the inner PictureBuffer.
    ///
    /// # Safety
    ///
    /// Caller must only access pixel rows that have been published
    /// (rows <= self.row_progress). For fully-complete pictures
    /// (PROGRESS_COMPLETE), all access is safe.
    pub unsafe fn data(&self) -> &PictureBuffer {
        unsafe { &*self.data.get() }
    }

    /// Get a mutable reference to the inner PictureBuffer.
    ///
    /// # Safety
    ///
    /// Caller must be the sole writer, accessing rows > row_progress.
    /// Only the decode thread for this picture should call this.
    /// This returns `&mut` from `&self` via `UnsafeCell`, which is the
    /// standard interior mutability pattern for concurrent data structures.
    #[allow(clippy::mut_from_ref)]
    pub unsafe fn data_mut(&self) -> &mut PictureBuffer {
        unsafe { &mut *self.data.get() }
    }

    /// Called by decode thread after completing MB row `row`.
    pub fn publish_row(&self, row: i32) {
        self.row_progress.store(row, Ordering::Release);
        self.progress_cond.notify_all();
    }

    /// Called by reader threads before accessing reference pixels from `row`.
    /// Uses hybrid spin → condvar strategy (WEDEO_SYNTHESIS §4).
    pub fn wait_for_row(&self, row: i32) {
        // Fast path: already done
        if self.row_progress.load(Ordering::Acquire) >= row {
            return;
        }

        // Phase 1: brief spin (microsecond waits)
        let mut spin = 0;
        while self.row_progress.load(Ordering::Relaxed) < row && spin < 100 {
            spin += 1;
            std::hint::spin_loop();
        }
        if self.row_progress.load(Ordering::Acquire) >= row {
            return;
        }

        // Phase 2: condvar wait (millisecond+ waits)
        let mut guard = self.progress_mutex.lock().unwrap();
        while self.row_progress.load(Ordering::Acquire) < row {
            guard = self.progress_cond.wait(guard).unwrap();
        }
    }

    /// Mark picture as fully complete (all rows decoded + deblocked).
    pub fn mark_complete(&self) {
        self.publish_row(PROGRESS_COMPLETE);
    }

    pub fn is_complete(&self) -> bool {
        self.row_progress.load(Ordering::Relaxed) == PROGRESS_COMPLETE
    }

    pub fn mb_height(&self) -> u32 {
        self.mb_height
    }
}

impl Drop for SharedPicture {
    fn drop(&mut self) {
        if let Some(pool) = self.return_to.take() {
            // SAFETY: &mut self in drop() guarantees exclusive access.
            // No other references exist (Arc refcount hit 0).
            let buf = std::mem::replace(self.data.get_mut(), PictureBuffer::empty());
            if let Ok(mut pool) = pool.lock() {
                pool.reclaim(buf);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// PicHandle — transparent Deref wrapper for current-frame decode
// ---------------------------------------------------------------------------

/// Handle to the current frame's picture during decode.
///
/// Derefs to `PictureBuffer` transparently, so `ctx.pic.y[offset]` works
/// without any changes to mb.rs, mc.rs, intra_pred.rs. Holds an
/// `Arc<SharedPicture>` that can be extracted for DPB storage.
pub struct PicHandle {
    shared: Arc<SharedPicture>,
}

impl PicHandle {
    pub fn new(pic: PictureBuffer) -> Self {
        Self {
            shared: SharedPicture::new(pic),
        }
    }

    /// Create a PicHandle whose buffer returns to `pool` when all references drop.
    pub fn new_pooled(pic: PictureBuffer, pool: &Arc<Mutex<BufferPool>>) -> Self {
        Self {
            shared: SharedPicture::new_pooled(pic, pool),
        }
    }

    /// Extract the inner Arc<SharedPicture> (consumes the handle).
    /// Used when storing in the DPB after decode + deblock.
    pub fn into_shared(self) -> Arc<SharedPicture> {
        self.shared
    }

    /// Get the Arc for cloning (e.g., for in-flight ref pic lists).
    pub fn shared(&self) -> &Arc<SharedPicture> {
        &self.shared
    }

    /// Signal a row is decoded (delegates to SharedPicture).
    pub fn publish_row(&self, row: i32) {
        self.shared.publish_row(row);
    }

    /// Mark picture as fully complete.
    pub fn mark_complete(&self) {
        self.shared.mark_complete();
    }
}

impl Deref for PicHandle {
    type Target = PictureBuffer;

    fn deref(&self) -> &PictureBuffer {
        // SAFETY: During decode, only one PicHandle exists per frame, and the
        // decode thread is the sole writer. Deref for read access is safe because
        // no other thread accesses this picture's data until it's published.
        unsafe { self.shared.data() }
    }
}

impl DerefMut for PicHandle {
    fn deref_mut(&mut self) -> &mut PictureBuffer {
        // SAFETY: PicHandle has exclusive ownership semantics — only one exists
        // per frame, and &mut self guarantees no concurrent borrows. The decode
        // thread is the sole writer to this picture's data.
        unsafe { self.shared.data_mut() }
    }
}
