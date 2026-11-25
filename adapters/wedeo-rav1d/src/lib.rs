#![deny(unsafe_op_in_unsafe_fn)]
//! rav1d AV1 decoder adapter for wedeo.
//!
//! Wraps the `rav1d` crate (pure Rust AV1 decoder, port of dav1d) behind
//! wedeo's `Decoder` trait, following the same pattern as `wedeo-symphonia`.
//!
//! ## Supported pixel formats
//!
//! AV1 Profile 0 (8-bit 420) and Profile 0 (10-bit 420) are fully supported.
//! 8-bit 422/444/monochrome are also handled. 10-bit 422/444 and 12-bit content
//! (Profile 2) require `PixelFormat` variants that wedeo doesn't have yet.

use std::collections::VecDeque;
use std::ffi::c_int;
use std::ffi::c_void;
use std::ptr::NonNull;

use rav1d::Dav1dResult;
use rav1d::include::dav1d::data::Dav1dData;
use rav1d::include::dav1d::dav1d::{Dav1dContext, Dav1dSettings};
use rav1d::include::dav1d::headers::{
    DAV1D_FRAME_TYPE_INTER, DAV1D_FRAME_TYPE_INTRA, DAV1D_FRAME_TYPE_KEY, DAV1D_FRAME_TYPE_SWITCH,
    DAV1D_PIXEL_LAYOUT_I400, DAV1D_PIXEL_LAYOUT_I420, DAV1D_PIXEL_LAYOUT_I422,
    DAV1D_PIXEL_LAYOUT_I444, Dav1dPixelLayout,
};
use rav1d::include::dav1d::picture::Dav1dPicture;
use rav1d::{
    dav1d_close, dav1d_data_create, dav1d_data_unref, dav1d_default_settings, dav1d_flush,
    dav1d_get_picture, dav1d_open, dav1d_picture_unref, dav1d_send_data,
};

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

/// Convert a `Dav1dResult` to a wedeo `Result`.
fn check_result(r: Dav1dResult) -> Result<()> {
    let code = r.0;
    if code == 0 {
        Ok(())
    } else if code == -(libc::EAGAIN as c_int) {
        Err(Error::Again)
    } else {
        Err(Error::InvalidData)
    }
}

/// Map a rav1d pixel layout + bit depth to a wedeo PixelFormat.
fn map_pixel_format(layout: Dav1dPixelLayout, bpc: c_int) -> Result<PixelFormat> {
    match (layout, bpc) {
        (DAV1D_PIXEL_LAYOUT_I420, 8) => Ok(PixelFormat::Yuv420p),
        (DAV1D_PIXEL_LAYOUT_I420, 10) => Ok(PixelFormat::Yuv420p10le),
        (DAV1D_PIXEL_LAYOUT_I422, 8) => Ok(PixelFormat::Yuv422p),
        (DAV1D_PIXEL_LAYOUT_I444, 8) => Ok(PixelFormat::Yuv444p),
        (DAV1D_PIXEL_LAYOUT_I400, 8) => Ok(PixelFormat::Gray8),
        // 10-bit 422/444 and all 12-bit layouts need PixelFormat variants
        // that wedeo doesn't have yet (AV1 Profile 2 / Professional).
        _ => {
            tracing::warn!("unsupported rav1d pixel format: layout={layout:?} bpc={bpc}");
            Err(Error::PatchwelcomeNotImplemented)
        }
    }
}

/// Chroma dimensions for a given layout.
fn chroma_dims(layout: Dav1dPixelLayout, w: usize, h: usize) -> (usize, usize) {
    match layout {
        DAV1D_PIXEL_LAYOUT_I420 => (w / 2, h / 2),
        DAV1D_PIXEL_LAYOUT_I422 => (w / 2, h),
        DAV1D_PIXEL_LAYOUT_I444 => (w, h),
        _ => (0, 0), // I400 monochrome
    }
}

/// Copy pixel data from a `Dav1dPicture` plane into a compact byte buffer.
///
/// Handles both positive strides (top-down) and negative strides (bottom-up).
/// For 8-bit, copies `width` bytes per row. For 10/12-bit, copies `width * 2`.
///
/// The whole plane is materialised as one slice, so extracting each row below
/// is plain bounds-checked slicing instead of one `from_raw_parts` per row.
///
/// # Safety
///
/// `plane_ptr` must point to a rav1d-owned plane buffer that stays alive for
/// the duration of this call, with at least `row_bytes` readable bytes at
/// every row address `plane_ptr + row * stride` for `row` in `0..height`.
/// rav1d allocates each plane as a single padded allocation (see
/// `Rav1dPictureDataComponentInner`), so all those bytes lie in one allocation;
/// `width`, `height`, `stride` and `bpc` must come from the same `Dav1dPicture`
/// that owns `plane_ptr`. The caller must keep that picture alive (via
/// `PictureGuard`) until this function returns.
unsafe fn copy_plane(
    plane_ptr: NonNull<c_void>,
    stride: isize,
    width: usize,
    height: usize,
    bpc: c_int,
) -> Vec<u8> {
    let bytes_per_pixel = if bpc > 8 { 2 } else { 1 };
    let row_bytes = width * bytes_per_pixel;
    if row_bytes == 0 || height == 0 {
        return Vec::new();
    }

    let base = plane_ptr.as_ptr() as *const u8;
    // Compute the lowest-addressed byte touched by any row and the plane's
    // total extent. With a negative stride the C pointer addresses the bottom
    // (last) row, so the first row in memory is `base + (height - 1) * stride`.
    let (start, plane_len) = if stride >= 0 {
        let stride = stride as usize;
        (base, (height - 1) * stride + row_bytes)
    } else {
        let abs_stride = stride.unsigned_abs();
        // SAFETY: `stride` is negative, so `(height - 1) * stride` is a
        // non-positive offset landing on the first row of the plane. The
        // caller guarantees the plane allocation covers all `height` rows.
        let start = unsafe { base.offset((height as isize - 1) * stride) };
        (start, (height - 1) * abs_stride + row_bytes)
    };

    // SAFETY: `base` is non-null because it came from `NonNull<c_void>`;
    // `u8` has alignment 1. The caller guarantees `[start, start + plane_len)`
    // lies inside one live, initialized plane allocation. The slice does not
    // escape this function and we only read through it, so it cannot alias a
    // live mutable borrow of the plane for longer than this call.
    let plane = unsafe { std::slice::from_raw_parts(start, plane_len) };

    let mut buf = Vec::with_capacity(row_bytes * height);
    for row in 0..height {
        let row_start = if stride >= 0 {
            row * stride as usize
        } else {
            (height - 1 - row) * stride.unsigned_abs()
        };
        buf.extend_from_slice(&plane[row_start..row_start + row_bytes]);
    }

    buf
}

/// RAII guard that calls `dav1d_picture_unref` on drop, preventing leaks
/// if pixel copying panics (e.g. OOM in Vec allocation).
struct PictureGuard(Option<Dav1dPicture>);

impl Drop for PictureGuard {
    fn drop(&mut self) {
        if let Some(ref mut pic) = self.0 {
            // SAFETY: `pic` is an initialized `Dav1dPicture` returned by a
            // successful `dav1d_get_picture`. The stack address is non-null
            // and valid for the read/write that `dav1d_picture_unref`
            // performs. `PictureGuard` owns the only local reference to this
            // picture, nothing else borrows it at drop time, and rav1d's
            // picture buffers are reference counted, so unref cannot
            // invalidate any other live object.
            unsafe {
                dav1d_picture_unref(NonNull::new(pic as *mut _));
            }
        }
    }
}

/// Convert a `Dav1dPicture` to a wedeo `Frame`.
///
/// The caller must keep the picture (and its `PictureGuard`) alive for the
/// duration of this call: the raw plane and frame-header pointers stored in
/// `Dav1dPicture` are owned by the picture's reference-counted buffers.
fn picture_to_frame(pic: &Dav1dPicture) -> Result<Frame> {
    let w = pic.p.w;
    let h = pic.p.h;
    debug_assert!(w >= 0 && h >= 0, "negative picture dimensions from rav1d");
    let w = w as usize;
    let h = h as usize;
    let bpc = pic.p.bpc;
    let layout = pic.p.layout;
    let pixel_format = map_pixel_format(layout, bpc)?;

    let bytes_per_pixel = if bpc > 8 { 2usize } else { 1 };

    let mut frame = Frame::new_video(w as u32, h as u32, pixel_format);
    frame.pts = pic.m.timestamp;
    frame.duration = pic.m.duration;

    // Luma plane
    let y_ptr = pic.data[0].ok_or(Error::InvalidData)?;
    let y_stride = pic.stride[0];
    // SAFETY: `y_ptr` is a valid plane pointer from a successfully decoded
    // picture. The caller's `PictureGuard` keeps its allocation alive for
    // this call, and `w`/`h`/`bpc` describe that same plane. We only read
    // pixels and copy them into an owned `Vec<u8>`.
    let y_data = unsafe { copy_plane(y_ptr, y_stride, w, h, bpc) };

    let y_plane = FramePlane {
        buffer: Buffer::from_slice(&y_data),
        offset: 0,
        linesize: w * bytes_per_pixel,
    };

    // Chroma planes (if not monochrome)
    let (cw, ch) = chroma_dims(layout, w, h);
    let planes = if cw > 0 && ch > 0 {
        let u_ptr = pic.data[1].ok_or(Error::InvalidData)?;
        let v_ptr = pic.data[2].ok_or(Error::InvalidData)?;
        let c_stride = pic.stride[1];

        // SAFETY: As for the luma plane: `u_ptr`/`v_ptr` are valid plane
        // pointers kept alive by the caller's `PictureGuard`, and `cw`/`ch`/
        // `bpc` describe those planes. `u_data` and `v_data` are independent
        // owned buffers, so the two reads cannot alias each other.
        let u_data = unsafe { copy_plane(u_ptr, c_stride, cw, ch, bpc) };
        let v_data = unsafe { copy_plane(v_ptr, c_stride, cw, ch, bpc) };

        vec![
            y_plane,
            FramePlane {
                buffer: Buffer::from_slice(&u_data),
                offset: 0,
                linesize: cw * bytes_per_pixel,
            },
            FramePlane {
                buffer: Buffer::from_slice(&v_data),
                offset: 0,
                linesize: cw * bytes_per_pixel,
            },
        ]
    } else {
        vec![y_plane]
    };

    // Determine picture type from frame header
    let pict_type = if let Some(frame_hdr) = pic.frame_hdr {
        // SAFETY: `frame_hdr` points into the Arc-owned `Dav1dFrameHeader`
        // held by `pic.frame_hdr_ref`. The caller's `PictureGuard` keeps that
        // reference alive, rav1d treats the header as immutable while the
        // picture exists, and the `frame_type` we read is a `Copy` value that
        // does not outlive this borrow.
        let hdr = unsafe { frame_hdr.as_ref() };
        if hdr.frame_type == DAV1D_FRAME_TYPE_KEY || hdr.frame_type == DAV1D_FRAME_TYPE_INTRA {
            PictureType::I
        } else if hdr.frame_type == DAV1D_FRAME_TYPE_INTER
            || hdr.frame_type == DAV1D_FRAME_TYPE_SWITCH
        {
            PictureType::P
        } else {
            PictureType::None
        }
    } else {
        PictureType::None
    };

    if pict_type == PictureType::I {
        frame.flags |= FrameFlags::KEY;
    }

    if let FrameData::Video(ref mut video) = frame.data {
        video.planes = planes;
        video.picture_type = pict_type;
    }

    Ok(frame)
}

/// rav1d AV1 decoder wrapper — implements wedeo's Decoder trait.
///
/// # Safety
///
/// `Dav1dContext` is `RawArc<Rav1dContext>`, a `Copy` raw pointer to an
/// `Arc<Rav1dContext>`. `RawArc` is deliberately `!Send`/`!Sync` to make C API
/// callers reason about ownership, but the pointee is `Send + Sync`: rav1d
/// explicitly implements both for `Rav1dContext` (see
/// <https://github.com/memorysafety/rav1d/pull/1329>) and synchronises every
/// access with its state `Mutex`, task-thread locks and atomics.
///
/// We keep a single logical owner: `ctx` is populated once by `dav1d_open`,
/// copied only into transient locals for FFI calls, and consumed exactly once
/// by `dav1d_close` in `Drop`. Moving this wrapper to another thread therefore
/// moves the one owning pointer; no `RawArc` copy is ever closed or used after
/// the close. The other fields (`VecDeque<Packet>`, `VecDeque<Frame>`, flags,
/// descriptor) are plain owned data touched only through `&mut self` and add
/// no extra thread-safety obligations.
unsafe impl Send for Rav1dDecoderWrapper {}

struct Rav1dDecoderWrapper {
    ctx: Option<Dav1dContext>,
    pending_packets: VecDeque<Packet>,
    /// Packet timestamps retained for pictures that rav1d returns with the
    /// sentinel timestamp. rav1d does not always propagate `Dav1dData.m`
    /// through to `Dav1dPicture.m`, so this is the authoritative fallback.
    timestamps: VecDeque<(i64, i64)>,
    pending_frames: VecDeque<Frame>,
    drained: bool,
    codec_descriptor: CodecDescriptor,
}

impl Rav1dDecoderWrapper {
    fn new(params: CodecParameters) -> Result<Self> {
        let mut settings = std::mem::MaybeUninit::<Dav1dSettings>::uninit();

        // SAFETY: `settings` points to writable memory sized for a
        // `Dav1dSettings` in this stack frame. rav1d's
        // `dav1d_default_settings` does a single full-struct `ptr::write`
        // and never reads the old, uninitialised contents.
        unsafe {
            dav1d_default_settings(NonNull::new(settings.as_mut_ptr()).unwrap());
        }
        // SAFETY: the full-struct write above initialized every field of
        // `Dav1dSettings`; it has no validity invariant beyond its field
        // values, so `assume_init` is sound.
        let mut settings = unsafe { settings.assume_init() };

        // Configure threading
        if params.thread_count > 0 {
            settings.n_threads = params.thread_count as c_int;
        }
        tracing::debug!(
            requested = params.thread_count,
            effective = settings.n_threads,
            "rav1d decoder threading"
        );
        // Apply film grain by default (matches FFmpeg behavior)
        settings.apply_grain = 1;

        let mut ctx: Option<Dav1dContext> = None;

        // SAFETY: `&mut ctx` is a valid, writable `Option<Dav1dContext>` slot
        // and `&mut settings` points to the fully initialized settings above.
        // On failure rav1d writes `None` into `ctx`, so the slot is always
        // safe to inspect. A successful call yields the sole owning raw Arc
        // pointer to a fresh context; no other copy exists yet.
        let result = unsafe {
            dav1d_open(
                Some(NonNull::new(&mut ctx as *mut _).unwrap()),
                Some(NonNull::new(&mut settings as *mut _).unwrap()),
            )
        };
        check_result(result)?;

        let ctx = ctx.ok_or(Error::InvalidData)?;

        let mut wrapper = Self {
            ctx: Some(ctx),
            pending_packets: VecDeque::new(),
            timestamps: VecDeque::new(),
            pending_frames: VecDeque::new(),
            drained: false,
            codec_descriptor: CodecDescriptor {
                id: params.codec_id,
                media_type: MediaType::Video,
                name: "av1_rav1d",
                long_name: "AV1 Video [rav1d]",
                properties: CodecProperties::LOSSY,
                profiles: &[],
            },
        };

        // If extradata is provided (av1C from MP4), send it as initial data
        if !params.extradata.is_empty() {
            let _ = wrapper.send_raw_data(&params.extradata, i64::MIN, 0);
            // Drain any frames produced by sequence header
            wrapper.drain_pictures();
        }

        Ok(wrapper)
    }

    /// Send raw OBU data to the decoder with a timestamp and duration.
    fn send_raw_data(&mut self, data: &[u8], timestamp: i64, duration: i64) -> Result<()> {
        let ctx = self.ctx.ok_or(Error::InvalidData)?;

        let mut dav1d_data = std::mem::MaybeUninit::<Dav1dData>::zeroed();
        let dav1d_data_ptr = NonNull::new(dav1d_data.as_mut_ptr()).unwrap();

        // SAFETY: `dav1d_data_ptr` points to writable memory sized for a
        // `Dav1dData` in this stack frame. On success `dav1d_data_create`
        // writes every field of that struct; on failure it leaves the
        // `MaybeUninit` untouched and returns null, which we check before
        // calling `assume_init`.
        let buf_ptr = unsafe { dav1d_data_create(Some(dav1d_data_ptr), data.len()) };
        if buf_ptr.is_null() {
            return Err(Error::InvalidData);
        }

        // SAFETY: `dav1d_data_create` succeeded, so the full struct write
        // described above happened; no `Dav1dData` field is left uninitialized.
        let mut dav1d_data = unsafe { dav1d_data.assume_init() };

        // Copy packet data into the rav1d buffer
        if let Some(ptr) = dav1d_data.data {
            // SAFETY: `dav1d_data_create` allocated exactly `data.len()`
            // readable/writable bytes at `ptr`, so the copy is in bounds.
            // `data.as_ptr()` points into the caller's packet buffer, a
            // distinct allocation, so the ranges cannot overlap. With an
            // empty packet the pointers may be dangling, but `count == 0`
            // makes `copy_nonoverlapping` a no-op.
            unsafe {
                std::ptr::copy_nonoverlapping(data.as_ptr(), ptr.as_ptr(), data.len());
            }
        }

        // Set timestamp and duration so rav1d propagates them to the output picture
        dav1d_data.m.timestamp = timestamp;
        dav1d_data.m.duration = duration;

        // SAFETY: `ctx` came from `dav1d_open` and has not been closed;
        // `&mut dav1d_data` is an initialized, writable `Dav1dData` that
        // rav1d reads and writes back (taking its own refcount on the
        // buffer). Ownership stays single-threaded because `send_raw_data`
        // takes `&mut self`, and rav1d synchronises the context internally.
        let result = unsafe {
            dav1d_send_data(
                Some(ctx),
                Some(NonNull::new(&mut dav1d_data as *mut _).unwrap()),
            )
        };

        // Always release our local Arc reference to the data buffer. Even on
        // success, `dav1d_send_data` writes back a `Dav1dData` holding its own
        // refcount copy; skipping unref would leak ours (notably for
        // zero-length packets where `sz == 0`). On EAGAIN or other errors
        // rav1d may not have taken the data at all, in which case this is the
        // only reference and must be released.
        // SAFETY: `dav1d_data` is initialized and valid for read/write;
        // `dav1d_data_unref` replaces it with a default (empty) value and
        // never dereferences stale pointers.
        unsafe {
            dav1d_data_unref(Some(NonNull::new(&mut dav1d_data as *mut _).unwrap()));
        }

        check_result(result)
    }

    /// Pull all available pictures from the decoder into our frame queue.
    fn drain_pictures(&mut self) {
        let Some(ctx) = self.ctx else { return };

        loop {
            let mut pic = std::mem::MaybeUninit::<Dav1dPicture>::zeroed();
            let pic_ptr = NonNull::new(pic.as_mut_ptr()).unwrap();

            // SAFETY: `pic_ptr` points to writable memory sized for a
            // `Dav1dPicture` in this stack frame. `ctx` is from `dav1d_open`
            // and has not been closed. rav1d writes a complete
            // `Dav1dPicture` (possibly all-empty on failure) through `out`.
            let result = unsafe { dav1d_get_picture(Some(ctx), Some(pic_ptr)) };

            if result.0 < 0 {
                break; // EAGAIN or error — no more pictures available
            }

            // SAFETY: on success `dav1d_get_picture` has written every field
            // of `Dav1dPicture`, including zeros/defaults for absent planes,
            // so `assume_init` is valid. The picture owns its plane buffers
            // and header Arc's; the guard now takes responsibility for
            // releasing them, even if pixel copying panics (e.g. OOM).
            let guard = PictureGuard(Some(unsafe { pic.assume_init() }));

            // SAFETY: `guard.0` is an initialized picture whose buffers stay
            // alive until `drop(guard)` below. `picture_to_frame` only reads
            // through the raw pointers and copies pixels into owned Vecs;
            // no reference to the picture escapes the call.
            let frame_result = picture_to_frame(guard.0.as_ref().unwrap());

            // Explicitly unref via the guard (drop releases it)
            drop(guard);

            match frame_result {
                Ok(mut frame) => {
                    if frame.pts == i64::MIN
                        && let Some((pts, duration)) = self.timestamps.pop_front()
                    {
                        frame.pts = pts;
                        frame.duration = duration;
                    }
                    self.pending_frames.push_back(frame);
                }
                Err(e) => {
                    tracing::warn!("failed to convert rav1d picture to frame: {e:?}");
                }
            }
        }
    }
}

impl Decoder for Rav1dDecoderWrapper {
    fn send_packet(&mut self, packet: Option<&Packet>) -> Result<()> {
        match packet {
            Some(pkt) => {
                self.timestamps.push_back((pkt.pts, pkt.duration));
                self.pending_packets.push_back(pkt.clone());
                Ok(())
            }
            None => {
                self.drained = true;
                Ok(())
            }
        }
    }

    fn receive_frame(&mut self) -> Result<Frame> {
        // First check if we already have decoded frames queued
        if let Some(frame) = self.pending_frames.pop_front() {
            return Ok(frame);
        }

        // Try to decode from pending packets
        while let Some(pkt) = self.pending_packets.pop_front() {
            let data = pkt.data.data().to_vec();
            let pts = pkt.pts;
            let duration = pkt.duration;

            match self.send_raw_data(&data, pts, duration) {
                Ok(()) => {}
                Err(Error::Again) => {
                    // Decoder's internal buffer is busy with previous data.
                    // Drain available pictures, then return to caller so it
                    // can call receive_frame() again (avoids infinite loop).
                    self.drain_pictures();
                    self.pending_packets.push_front(pkt);
                    return if let Some(frame) = self.pending_frames.pop_front() {
                        Ok(frame)
                    } else {
                        Err(Error::Again)
                    };
                }
                Err(e) => return Err(e),
            }

            // Try to get pictures after sending data
            self.drain_pictures();

            if let Some(frame) = self.pending_frames.pop_front() {
                return Ok(frame);
            }
        }

        // If draining, pull remaining frames from the decoder.
        // dav1d_get_picture automatically enters drain mode when no more
        // data is sent (it sets state.drain = true internally).
        if self.drained {
            self.drain_pictures();

            if let Some(frame) = self.pending_frames.pop_front() {
                return Ok(frame);
            }
            return Err(Error::Eof);
        }

        Err(Error::Again)
    }

    fn flush(&mut self) {
        if let Some(ctx) = self.ctx {
            // SAFETY: `ctx` is a live handle from `dav1d_open` that has not
            // been closed. `flush` takes `&mut self`, so no other call through
            // this wrapper can be in flight, and rav1d additionally
            // synchronises with its internal state mutex.
            unsafe {
                dav1d_flush(ctx);
            }
        }
        self.pending_packets.clear();
        self.pending_frames.clear();
        self.drained = false;
    }

    fn descriptor(&self) -> &CodecDescriptor {
        &self.codec_descriptor
    }
}

impl Drop for Rav1dDecoderWrapper {
    fn drop(&mut self) {
        if let Some(ctx) = self.ctx.take() {
            let mut ctx_opt: Option<Dav1dContext> = Some(ctx);
            // SAFETY: `ctx_opt` is an initialized local `Option<Dav1dContext>`
            // holding the single owning raw Arc pointer created by
            // `dav1d_open`. It is valid for the read/write `dav1d_close`
            // performs; `dav1d_close` takes the value and writes `None` back,
            // so the pointer is consumed exactly once and cannot be
            // double-freed. `self.ctx.take()` above prevents a second close.
            unsafe {
                dav1d_close(NonNull::new(&mut ctx_opt as *mut _));
            }
        }
    }
}

// --- Factory registration ---

struct Rav1dAv1DecoderFactory;

impl DecoderFactory for Rav1dAv1DecoderFactory {
    fn descriptor(&self) -> &DecoderDescriptor {
        static DESC: DecoderDescriptor = DecoderDescriptor {
            codec_id: CodecId::Av1,
            name: "av1_rav1d",
            long_name: "AV1 Video [rav1d]",
            media_type: MediaType::Video,
            capabilities: CodecCapabilities::empty(),
            properties: CodecProperties::LOSSY,
            priority: 50, // adapter, not native
        };
        &DESC
    }

    fn create(&self, params: CodecParameters) -> Result<Box<dyn Decoder>> {
        Ok(Box::new(Rav1dDecoderWrapper::new(params)?))
    }
}

inventory::submit!(&Rav1dAv1DecoderFactory as &dyn DecoderFactory);
