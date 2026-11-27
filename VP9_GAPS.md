# VP9 remaining gaps

## 10-bit and 12-bit vectors emit zero frames

Measured 2026-11-27 against FFmpeg at commit `f6d844c`.

### Root cause

`codecs/wedeo-codec-vp9/src/decoder.rs:194-201` contains an explicit early
return:

```rust
if header.bit_depth != 8 {
    warn!(...);
    return Ok(());
}
```

`header.rs:445-451` already parses 10-bit and 12-bit correctly, so the parsed
keyframe is discarded at this gate. Reference slots stay empty, and every
later inter frame is dropped with
`inter frame references empty slot for dimensions`. That is the complete
chain producing zero output frames for all 8 high-depth vectors.

A temporary experiment replacing the condition with `if false && ...` made
frames appear, but each frame had only 21600 bytes (8-bit 4:2:0 size) instead
of FFmpeg's 43200 bytes (16-bit container), and the CRC did not match. The
experiment was reverted; no commit was made.

FFmpeg ground truth: `vp9.c:482` parses profile/bit depth, `vp9.c:503-515`
selects `yuv420p10le` and friends. A gdb breakpoint at that site measured
`profile=2`, `bits=1`, 10 frames, 43200 bytes per frame.

### Why there is no small fix

The whole VP9 sample path is hardcoded to `u8`: `FrameBuffer`
(`recon.rs:28-60`), IDCT, intra prediction, motion compensation, loop filter
(`clip_u8` clamps to 255), and the output format is always `Yuv420p`.
`PixelFormat` has no `Yuv420p10le`, `Yuv420p12le` or 4:4:4 10/12-bit variants.

A bit-exact fix requires a `u8`/`u16` generic conversion across those modules
plus new pixel format mappings. That is a staged project, not a one-line gate
removal. Do not remove the early return without it: the output would be
silently wrong.

### Baseline after the experiment was reverted

226 local VP9 vectors: 211 bit-exact, 7 CRC differences, 8 zero-frame vectors
(the high-depth set). No regressions.

## Odd-size 210x202 divergence is upstream of the loop filter

Measured 2026-11-27 at `f6d844c`, no source change committed.

Vector `vp90-2-03-size-210x202.webm`, frame 1 first luma difference at x=157,
y=203: Wedeo 154, FFmpeg 155. The earlier hypothesis blamed the horizontal row
filter and its `p7` input. Per-bin ground truth disproves that:
`loop_filter_edge` input at y=160, x=203 in Wedeo is
`p7=152, p3=154, p2=154, p1=155, p0=155, q0=155, q1=155` with
`E=61, I=19, H=1`. A gdb breakpoint at `vp9dsp_template.c:1826` in FFmpeg gave
matching `p` values but `q0=156, q1=156, q2=157, q3=157`. The filter kernels
in `loopfilter.rs:557-666` match FFmpeg tap by tap for filter4/filter8/
filter16, and the E/I/H LUT agrees. The 8-tap formula on Wedeo's inputs yields
154; on FFmpeg's q0/q1 it yields 155. So the row filter is correct and the
divergence enters through q0/q1 before filtering.

Wedeo's frame-1 vertical filter mask at row 160 only runs a 16-tap edge at
x=192 covering columns 184..199, so it cannot write column 203. No FFmpeg
vertical filter call at x=200 was demonstrated either. Next step: extract the
pre-loop-filter sample at (x=203, y=160) in both decoders and follow inter
prediction / inverse transform for the block at (192,160).

Evidence: `/tmp/ai-agent/VP9_LPF_GT.md` (ephemeral). Do not patch the loop
filter based on the old p7 hypothesis.

---

## Staged implementation plan and completed stage 1

# VP9 10/12-bit support: staged design

## Measured root cause

`codecs/wedeo-codec-vp9/src/decoder.rs` returns early for every frame with
`header.bit_depth != 8`. Removing that gate alone produces frames with
8-bit-sized planes and wrong sample values: the entire reconstruction path
(`FrameBuffer`, IDCT, intra prediction, motion compensation, loop filter) stores
and clamps `u8`, while FFmpeg writes 9-16-bit planar samples in 16-bit
containers. See `VP9_GAPS.md`.

## Stage 1 (implemented and verified)

1. Core pixel formats: add FFmpeg-numbered `Yuv420p12le/be`, `Yuv422p12le/be`,
   `Yuv444p12le/be` and fix the stale 10-bit discriminants (FFmpeg
   `libavutil/pixfmt.h`: 420p10le=62, 422p10le=64, 444p10le=68,
   420p12le=123, 422p12le=127, 444p12le=131).
2. VP9 output format selection: `output_pixel_format(bit_depth, ss_h, ss_v)`
   mirrors FFmpeg `vp9.c:497-504` (`pix_fmt_for_ss`) for 8/10/12-bit 4:2:0,
   4:2:2 and 4:4:4. `RefFrame` now carries bit depth and chroma subsampling so
   `show_existing_frame` outputs use the stored frame's format as well.
3. `FrameBuffer::new` now allocates chroma planes using the parsed
   `ss_h`/`ss_v`, so 4:2:2 and 4:4:4 frames get full-height chroma. Their
   first (key) frames now match FFmpeg frame CRCs; inter frames still differ
   for reasons outside this stage.
4. The 8-bit gate stays in place. High-depth frames are still skipped instead
   of emitting mislabeled data. 8-bit 4:2:0 output is unchanged and bit-exact.
   Unsupported output formats (4:4:0, RGB) are skipped with a warning rather
   than aborting the decoder.

Stage 1 is self-contained: format descriptors and output labeling can be
verified with unit tests and an 8-bit framecrc comparison, without changing
sample data.

## Stage 2: sample storage

Introduce a `u8`/`u16` plane storage enum in `FrameBuffer` with accessor
helpers. `new(width, height, bit_depth)` allocates `u16` planes for 10/12-bit
frames, with strides in samples (as FFmpeg does) and output byte size
`stride_samples * 2`. Reference frames must carry the same variant. Keep the
`u8` path byte-for-byte identical for 8-bit vectors.

## Stage 3: reconstruction math

Generalize `idct.rs` (`itxfm_add`, `itxfm_add_lossless`), `intra_pred.rs`, and
`mc.rs` over `u8`/`u16`. FFmpeg keeps intermediate sums in `int16_t`/`int32_t`
and clamps to `(1 << bit_depth) - 1` (`vp9recon.c`, `vp9dsp_template.c`).
Quantizer tables in `quant.rs` already accept 10/12-bit indices, so no change
is needed there.

## Stage 4: loop filter and gate removal

The loop filter is owned by another agent. It needs `clip_u16` and
high-depth edge thresholds (`vp9loopfilter.c`). Only after stages 2-4 are
bit-exact on FATE high-depth vectors may the early return be removed.

## Verification for stage 1

- `cargo check --workspace`, `cargo test -p wedeo-codec-vp9` and
  `cargo clippy -p wedeo-codec-vp9 -p wedeo-core`.
- `wedeo-framecrc` versus `ffmpeg -bitexact -i <8-bit VP9 vector> -f framecrc -`
  on a representative 8-bit vector.
