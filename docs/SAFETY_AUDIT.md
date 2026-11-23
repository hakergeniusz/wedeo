# Safety Audit: rav1d adapter and H.264 shared picture

Scope: `adapters/wedeo-rav1d/src/lib.rs` and
`codecs/wedeo-codec-h264/src/shared_picture.rs` only. Other unsafe code in the
workspace was out of scope for this work unit.

Method: every `unsafe fn`, `unsafe impl`, `unsafe` block and `#[allow(...)]` in
the two files was reviewed against four questions:

1. **Pointer validity** - is the pointer non-null, aligned and in bounds of a
   live allocation?
2. **Aliasing** - is there a live overlapping `&mut`, or can one be created?
3. **Lifetime** - does the pointee outlive every reference derived from it?
4. **Thread safety** - is concurrent access synchronised, and is the
   `Send`/`Sync` claim justified by the underlying type?

Before this audit the rav1d adapter had 23 unsafe constructs with 17 SAFETY
comments, and `shared_picture.rs` had 7 with 5. After this audit the adapter has
21 constructs (two removed by making `picture_to_frame` safe) and every one has
an adjacent, explicit justification. All 7 constructs in `shared_picture.rs`
are now documented, and the bare `#[allow(clippy::mut_from_ref)]` has a written
justification.

## rav1d adapter (`adapters/wedeo-rav1d/src/lib.rs`)

| File:line | Construct | Rationale | Remaining risk |
| --- | --- | --- | --- |
| `lib.rs:99` | `unsafe fn copy_plane` | Contract requires a rav1d-owned plane buffer alive for the call, with `row_bytes` readable bytes at each `base + row * stride`. `width`/`height`/`stride`/`bpc` must come from the same picture. | Trusts the dav1d allocator contract (one padded allocation per plane). Caller must uphold the guard-lifetime contract. |
| `lib.rs:124` | `base.offset((height - 1) * stride)` | Only for negative stride; the C pointer addresses the bottom row, so the negative offset lands on the first row inside the same allocation. | Pointer arithmetic overflow on 32-bit for extreme frame sizes; unchanged from the previous code. |
| `lib.rs:133` | `slice::from_raw_parts(start, plane_len)` | `NonNull` guarantees non-null, `u8` alignment is 1, caller guarantees the whole `[start, start + plane_len)` range is one live initialized allocation; slice does not escape. | Same allocator-contract trust as above; the single slice now covers the whole plane instead of one slice per row. |
| `lib.rs:162` | `dav1d_picture_unref` in `PictureGuard::drop` | `pic` is an initialized `Dav1dPicture` from a successful `dav1d_get_picture`; stack address valid for read/write; guard owns the only local reference; rav1d buffers are refcounted. | None identified. |
| `lib.rs:197` | `copy_plane(y_ptr, ...)` | Plane pointer from a decoded picture; guard keeps it alive; `w`/`h`/`bpc` describe that plane; read-only copy into owned `Vec<u8>`. | Caller (`drain_pictures`) must keep the guard alive. |
| `lib.rs:216-217` | `copy_plane(u_ptr/v_ptr, ...)` | Same as luma; U and V are distinct owned buffers so the two reads cannot alias. | Same as luma. |
| `lib.rs:243` | `frame_hdr.as_ref()` | Pointer into the `Arc` owned by `pic.frame_hdr_ref`; guard keeps it alive; rav1d treats headers as immutable; `Copy` output does not outlive the borrow. | None identified. |
| `lib.rs:287` | `unsafe impl Send for Rav1dDecoderWrapper` | `Dav1dContext` is `RawArc<Rav1dContext>` (`!Send` wrapper around an `Arc`). rav1d explicitly implements `Send + Sync` for the pointee and synchronises with mutexes/atomics. Single logical owner: created once in `dav1d_open`, transient `Copy`s only for FFI calls, consumed once in `dav1d_close`. Other fields are owned data behind `&mut self`. | Depends on rav1d's upstream `Send`/`Sync` impls being correct; any future stored `RawArc` clone would break the single-owner invariant. |
| `lib.rs:305` | `dav1d_default_settings` | Pointer to writable `MaybeUninit<Dav1dSettings>`; rav1d does one full-struct `ptr::write` and never reads old contents. | None identified. |
| `lib.rs:311` | `settings.assume_init()` | Full-struct write above initialized every field; no invariant beyond field values. | None identified. |
| `lib.rs:327` | `dav1d_open` | `&mut ctx` valid writable slot; `&mut settings` initialized; failure writes `None`; success yields the sole owning raw pointer. | None identified. |
| `lib.rs:374` | `dav1d_data_create` | Pointer to writable `MaybeUninit<Dav1dData>`; struct fully written on success, untouched on failure (null checked before `assume_init`). | None identified. |
| `lib.rs:381` | `dav1d_data.assume_init()` | Succeeded `dav1d_data_create` fully wrote the struct. | None identified. |
| `lib.rs:391` | `copy_nonoverlapping` | Destination is the allocation from `dav1d_data_create` with exactly `data.len()` bytes; source is the caller's distinct packet buffer; `count == 0` makes empty packets a no-op. | None identified. |
| `lib.rs:405` | `dav1d_send_data` | Live context handle from `dav1d_open`; initialized writable `Dav1dData`; `&mut self` plus rav1d internal locking prevents concurrent use. | None identified. |
| `lib.rs:421` | `dav1d_data_unref` (now unconditional) | Initialized, writable struct; unref replaces it with a default value. Previously skipped when `sz == 0`, leaking the refcount for empty packets; now always released. | None identified; fixes a small leak without changing decode output. |
| `lib.rs:440` | `dav1d_get_picture` | Writable `MaybeUninit<Dav1dPicture>` slot; live context; rav1d always writes a complete picture (empty on failure). | None identified. |
| `lib.rs:451` | `pic.assume_init()` | `dav1d_get_picture` wrote every field, including defaults for absent planes; guard takes ownership of the refcounted buffers. | None identified. |
| `lib.rs:544` | `dav1d_flush` | Live context handle; `&mut self` excludes concurrent calls; rav1d locks internally. | None identified. |
| `lib.rs:568` | `dav1d_close` | `ctx_opt` is an initialized local holding the single owning raw pointer; valid for read/write; value is taken and `None` written back; `self.ctx.take()` prevents a second close. | None identified. |

### Changes made in the adapter

* `picture_to_frame` is now a safe function. Its two risky raw-pointer reads
  remain as small, individually justified `unsafe` blocks (plane copy and frame
  header), so the safety obligation is visible at each use rather than being
  inherited by the whole function. Its call site in `drain_pictures` no longer
  needs `unsafe`.
* `copy_plane` now materialises the plane with a single `from_raw_parts` and
  extracts rows with bounds-checked slicing. This removes `height` unchecked
  slice constructions per plane and keeps all row indexing inside one
  allocation. The empty-plane case (`width == 0` or `height == 0`) now returns
  early instead of calling `from_raw_parts` with length 0.
* `dav1d_data_unref` is now called unconditionally. `dav1d_send_data` writes
  back a `Dav1dData` that holds our refcount copy even on success; the old
  `sz > 0` guard leaked it for zero-length packets.
* Every existing SAFETY comment was expanded to state which of validity,
  aliasing, lifetime and thread safety it relies on. The `unsafe impl Send`
  now names rav1d's upstream `Send + Sync` justification.

## H.264 shared picture (`codecs/wedeo-codec-h264/src/shared_picture.rs`)

| File:line | Construct | Rationale | Remaining risk |
| --- | --- | --- | --- |
| `shared_picture.rs:196` | `unsafe impl Sync for SharedPicture` | `PictureBuffer` is `Send + Sync` (owned `Vec`s and integers). Access is only via `unsafe fn` accessors with a row contract: writer uses `Release` publish, readers use `Acquire` wait or the condvar, so no byte is touched concurrently. `Mutex`/`Condvar`/`Arc<Mutex<BufferPool>>` are thread-safe. | See the whole-struct aliasing limitation below. Required because `Arc<SharedPicture>` is sent to frame-decode worker threads. |
| `shared_picture.rs:237` | `unsafe fn data` | `UnsafeCell::get()` on a live `&self` is non-null/aligned and points to an initialized buffer owned by `self`; the returned reference is bounded by `&self`. | Caller must respect the published-row contract. The `&PictureBuffer` covers the whole struct. |
| `shared_picture.rs:242` | `&*self.data.get()` | Same as above; explicit local SAFETY note added. | Same. |
| `shared_picture.rs:261` | `unsafe fn data_mut` | Same pointer facts; documented sole-writer contract; `#[allow(clippy::mut_from_ref)]` now has a written justification. | Caller must be the only writer; whole-struct `&mut` limitation below. |
| `shared_picture.rs:265` | `&mut *self.data.get()` | Same as above; explicit local SAFETY note added. | Same. |
| `shared_picture.rs:316` | `mem::replace` in `Drop` | `Drop` runs at `Arc` refcount zero, so `&mut self` is exclusive and `get_mut()` is safe; `PictureBuffer::empty()` keeps the cell valid. | None identified. |
| `shared_picture.rs:387` | `Deref` calling `data()` | One `PicHandle` per frame, frame not yet in any DPB ref list, so no concurrent reader; `Arc` keeps storage alive. | Relies on the "not yet published" invariant in `decoder.rs`/`mb.rs`. |
| `shared_picture.rs:397` | `DerefMut` calling `data_mut()` | `&mut self` excludes other borrows of the handle; frame not yet published; `Arc` keeps storage alive. | Same invariant. |

### Known limitation: whole-struct aliasing

The row-level protocol prevents *physical* data races on pixel bytes, but
`data()` and `data_mut()` hand out `&PictureBuffer` and `&mut PictureBuffer`
for the entire struct from an `UnsafeCell`. Under Stacked Borrows / Tree
Borrows, a live whole-struct `&mut` invalidates other references to the same
allocation even when the threads only touch disjoint rows. Fixing this
properly requires changing the interface used by `decoder.rs` and `mb.rs`,
for example row-scoped slice accessors or an `RwLock<PictureBuffer>` whose
guards are held only for the duration of a row read. Those files are owned by
other work units, so this is recorded here as a remaining risk rather than
changed. The practical exposure is limited because the writer is the current
frame and readers only call `data()` after `wait_for_row()`.

## Why H.264 multi-slice parallel decode is disabled

Commit `9cacd90` ("fix(h264): disable unsound multi-slice parallel decode")
removed the `decode_frame_multi_slice` path. That path shared an
`UnsafeCell<FrameDecodeContext>` between slice threads behind an `unsafe impl
Sync` whose SAFETY comment claimed per-slice storage, but:

1. `next_slice_first_mb` was computed and never enforced, so every thread
   decoded to end-of-frame and overwrote other slices' macroblocks, violating
   the disjoint-row invariant that made the `UnsafeCell` sound.
2. `bi_scratch_y/u/v` were shared without synchronisation (data race).
3. `qp`, `mc_scratch` and other per-slice state lived on the shared
   `FrameDecodeContext`, contradicting the safety comment.
4. The affected `unsafe impl Sync` / cell invariant was therefore false.

The fallback to sequential decode restored 79/79 progressive conformance
(52/52 CAVLC + 27/27 CABAC). The decision is deliberately kept: frame-level
threading with `SharedPicture` is sound under the row protocol above, but
multi-slice threading must not be re-enabled until per-thread state is truly
isolated (a separate `FrameDecodeContext` per slice, merged after join).
Re-enabling it without that work would reintroduce undefined behaviour, not
just a conformance regression.

## Verification

* `cargo fmt --all -- --check` - clean.
* `cargo clippy -p wedeo-rav1d -p wedeo-codec-h264 --all-targets -- -D warnings` -
  clean (only pre-existing workspace-manifest warnings from Cargo).
* `cargo test -p wedeo-rav1d -p wedeo-codec-h264` - 319 passed, 0 failed
  (4 ignored), plus doc tests.
* FATE integration tests: `fate_av1` 4/4 passed; `fate_h264` 21/22 passed.
  The remaining test, `fate_h264_high422_10bit_frame_layout`, hangs in this
  checkout: a debug-build `attempt to multiply with overflow` panic at
  `codecs/wedeo-codec-h264/src/dequant.rs:210` occurs on a decoder worker
  thread, after which the main test waits forever. That file is outside this
  work unit; the failure reproduces when the test is run alone and is
  unrelated to the unsafe-comment changes made here.
