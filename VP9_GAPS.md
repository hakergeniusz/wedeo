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

### Sweep status

Current at HEAD `58d2c0c`: 226 local VP9 vectors, 215 bit-exact, 3 CRC
differences, 8 zero-frame vectors (the high-depth set); see `STATUS.md`. At
`8e13c1e` the sweep was 214 bit-exact, 4 CRC differences, 8 zero-frame
vectors. The earlier 211 bit-exact / 7 CRC baseline was recorded before the
`d7ac5ba` clipped-NNZ edge-context fix. No regressions.

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

## Stage 2: sample storage (implemented and verified)

`FrameBuffer` now holds `Plane::{U8(Vec<u8>), U16(Vec<u16>)}` for Y/U/V.
`FrameBuffer::new(width, height, bit_depth, ss_h, ss_v)` allocates `u16`
planes for 10/12-bit frames; strides stay in samples and the output
`linesize` is `stride_samples * bytes_per_sample` (1 or 2). `RefFrame`
clones the whole buffer, so reference slots keep the same variant. Byte
views (`as_u8`/`as_u8_mut` plus `Deref`/`Index`) keep the 8-bit call sites,
including the loop filter, source-compatible; no reconstruction math
changed and the early return stays in place.

Verification: `cargo test --workspace`, `cargo clippy -p wedeo-codec-vp9
--all-targets -- -D warnings`, `cargo fmt -p wedeo-codec-vp9`, and full
FATE VP9 framecrc parity with no regressions. At that time the sweep was
214 bit-exact, 4 known differences and 8 high-depth zero-frame vectors (the
agent's isolated worktree was based on the older 211/7 baseline before the
NNZ edge-context fix). Current parity is in `STATUS.md`.

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

---

## Other 8-bit failures: segkey_adpq and 210x198 (2026-11-27)

# VP9 8-bit non-210x202 ground-truth triage (2026-11-27)

Worktree: `/home/hakergeniusz/.local/share/opencode/worktree/695063/vp9-other8`
Base commit: `5e47eb4` (clean tree, no patch committed).
Binaries: `wedeo-framecrc` from `CARGO_TARGET_DIR=/var/cache/zstd5/wedeo-target-vp9-other8`
and FFmpeg debug build `/var/cache/zstd5/ffmpeg-src-w62/ffmpeg_g`.
Corpus: `/var/cache/zstd5/wedeo-ref/fate-suite/vp9-test-vectors/`.

## Reproduction and symmetric comparison

CRCs were compared on the CRC field only (wedeo uses tb 1/1000, FFmpeg 1001/30000).
Raw YUV was decoded from both binaries with the **same** loop-filter setting;
an earlier asymmetric comparison (wedeo `WEDEO_NO_DEBLOCK=1`, FFmpeg default)
was discarded.

### `vp90-2-15-segkey_adpq.webm` (852x480, 150 shown frames)

- Wedeo CRCs match FFmpeg exactly for frames 0..91; first mismatch at frame 92.
- Raw-YUV diff frame 92: 1318 Y pixels, first at (x=849,y=286), Wedeo 125 /
  FFmpeg 124, magnitude max 172. U/V also differ (max 13 / 5).
- Affected columns form 8x8-MI-aligned groups (192..217, 271..288, 656..687,
  839..851); affected rows are 286..313, 335..393, 422..432, 470..479.
- FFmpeg header ground truth via gdb (`vp9.c:805`, one stop per header):
  frame 0 is keyframe with `seg_enabled=1, update_map=1, prob=255,255,128`.
  Hidden altref frames occur in the stream; `update_map=1` again at decoded
  header index 89, and displayed frame 92 is the first affected output.
- Wedeo's segmentation state (`decoder.rs:498-506`, `block.rs:333-394`) mirrors
  FFmpeg's retention rule (`vp9.c:1618-1670`) and temporal pred-prob path
  (`vp9block.c:98-141`). No single measured field mismatch was obtained before
  the time budget expired, so no patch was made.

### `vp90-2-03-size-210x198.webm` (210x198, 10 frames)

- Wedeo CRCs match frames 0..5; first mismatch at frame 6 (frames 6..9 differ).
- Symmetric raw-YUV diff frame 6: 1646 Y pixels, first at (x=209,y=126),
  Wedeo 33 / FFmpeg 32; U 264 px (max 4), V 325 px (max 3).
- A flat +1/+2 luma band at y=127..132, x=196..207 and a large mismatch at the
  bottom-right corner (y=192..197, x>=178, max |diff|=106) were measured.
- FFmpeg header GT at decoded header index 6 (`gdb ignore 1 6`, `vp9.c:805`):
  `w=210 h=198 kf=0 invis=0 errres=0 intraonly=0 resetctx=0 refreshrefmask=1`,
  `refidx=0,1,2 signbias=0,0,0 framectx=0 refreshctx=1 parallelmode=0`,
  `tile_cols_log2=0 tile_rows_log2=0 baseq=174 ydc=0 uvdc=0 uvac=0`,
  `filtmode=4 use_last_frame_mvs=1`. Wedeo parses the same fields
  (`refresh=1 ref_idx=[0,1,2] base_q_idx=174 tile 0,0`).
- **Ruled out:** `use_last_frame_mvs`. Wedeo's `decoder.rs` debug log prints
  the header default (`false`) *before* the `compute_use_last_frame_mvs`
  override at line 245; a temporary trace at the override measured
  `use_last=true` for frames 1..7, matching FFmpeg.
- FFmpeg block GT (`vp9block.c:1292`, bottom-right region) and Wedeo block
  trace (temporary, released) both show frame 5 matching, e.g. MI (24,24)
  `bs=7 skip=0 mv=(-28,16)`. At frame 6 MI (24,24) FFmpeg decodes
  `bs=11 (4x8 px), skip=1, inter, mv=(84,0)`, while Wedeo decodes a
  `32x16 intra, skip=false` block. The partition/mode symbols have already
  diverged by that point; no single upstream symbol was isolated before the
  time budget expired.
- The loop filter is not the cause: symmetric no-loop-filter and filtered
  comparisons give the same 1646-pixel mismatch at frame 6.

## Conclusion

No measured single-cause fix was established for either vector within the
budget. No source change was committed; the worktree is clean. Next agent:
extract the first divergent bool-decoder symbol in frame 6 of 210x198 by
logging every `get_tree`/`get_prob` symbol in both decoders up to the block at
MI (24,24), and check `vp9.c` partition probability context updates for the
odd-width MI grid (`s->cols = (210+7)>>3 = 27`, `s->sb_cols = 4`).

---

## RESOLVED: clipped NNZ edge context (commit d7ac5ba, 2026-11-27)

All three previously failing 8-bit vectors (`210x198`, `210x202`,
`segkey_adpq`) are now bit-exact. Root cause: Wedeo's `merge_ctx()` in
`codecs/wedeo-codec-vp9/src/block.rs` kept only the first NNZ entry when a
`step`-wide merge was clipped at the frame edge, whereas FFmpeg's MERGE reads
a full step-wide word from the backing NNZ array. With a clipped local copy,
OR-ing all remaining entries reproduces FFmpeg's value.

Measured result: target vector all 10 frames identical; VP9 sweep 211 -> 214
exact vectors out of 226, with zero regressions. Earlier docs about NNZ
context, loop-filter inputs and segmentation timing in these vectors are
historical evidence of the same root cause and no longer describe current
behaviour.

---

## Remaining non-high-depth VP9 differences (historical, 2026-11-27)

This section records the four differences enumerated at base `d86b9d2`. At
HEAD `58d2c0c`, `vp91-2-04-yuv444.webm` is exact after `1486b7d` and `9153d35`,
leaving three CRC differences; see `STATUS.md`.

# VP9 remaining 8-bit differences: ground truth (2026-11-27)

Worktree: `/home/hakergeniusz/.local/share/opencode/worktree/695063/vp9-remaining`
Base: `d86b9d2` (clean; all experiments were reverted, no commit).
Corpus: `/var/cache/zstd5/wedeo-ref/fate-suite/vp9-test-vectors/` (226 files).
FFmpeg: `/var/cache/zstd5/ffmpeg-src-w62/ffmpeg_g`, run with
`-threads 1 -loglevel error -bitexact -i FILE -f framecrc -`.
Wedeo: release `wedeo-framecrc` built in
`CARGO_TARGET_DIR=/var/cache/zstd5/wedeo-target-vp9-rem`.

## Enumerated set (baseline binary at `d86b9d2`)

`214 exact, 4 diff, 8 high-depth zero-frame` at that base:

- `vp90-2-05-resize.ivf`: first CRC difference at frame 3.
- `vp91-2-04-yuv422.webm`: first CRC difference at frame 1.
- `vp91-2-04-yuv440.webm`: Wedeo 9 frames vs FFmpeg 10, first difference at frame 0.
- `vp91-2-04-yuv444.webm`: first CRC difference at frame 1.

## Experiment A: sticky colour config (measured, insufficient)

GDB at `libavcodec/vp9.c:805` on all 11 headers of `vp91-2-04-yuv444.webm`
prints `kf=1 ss=0,0 pixfmt=5 bd=8` for the keyframe and `kf=0 ss=0,0
pixfmt=5 bd=8` for every subsequent header. FFmpeg only reads colour config
for keyframes / intra-only frames (`vp9.c:580-610`); `s->ss_h`, `s->ss_v`,
`s->bpp_index`, `s->pix_fmt` persist across ordinary inter frames.

Wedeo `header.rs:640-644` defaults inter headers to 8-bit 4:2:0. Baseline
`vp91-2-04-yuv444.webm` output sizes: frame 0 304128 bytes (correct 4:4:4),
frames 1+ 152064 bytes (4:2:0), while FFmpeg emits 304128 for every frame.

A patch added a sticky `(bit_depth, ss_x, ss_y)` to `Vp9Decoder`, updated it
on keyframes / intra-only frames and restored it into inter headers, guarded
by `output_pixel_format(...).is_some()` so 4:4:0 frames keep their old output
count. Result: output sizes became correct for 4:2:2 and 4:4:4, no regression
in the 226-vector sweep (`214 exact, 4 diff, 8 zero`), but **no CRC became
exact**. Hypothesis "wrong output format is the only cause" is disproved.

Raw YUV after experiment A (`--raw-yuv` in wedeo-framecrc, FFmpeg
`-f rawvideo -pix_fmt yuv444p`), frame 1, 352x288:

- Y plane: 0 differing pixels (bit-exact).
- U plane: 81156 differing pixels, first at (x=32,y=0), Wedeo 0 / FFmpeg 135.
- V plane: 81007 differing pixels, first at (x=32,y=0), Wedeo 0 / FFmpeg 124.
- The first 32 columns of both chroma planes match; from x=32 onward Wedeo
  is largely zero, so chroma reconstruction/writeback coverage is suspect.

## Experiment B: chroma MV scaling (measured, disproved)

FFmpeg GDB (`inter_pred_8bpp`, shown frame 1 = header index 3 in the
breakpoint counter) first blocks:

```text
n=2 row=0 col=0 bs=6 mv=(32,-64)
n=3 row=0 col=2 bs=7 mv=(0,-84)
n=7 row=0 col=4 bs=5 mv=(18,-70)
n=10 row=4 col=4 bs=8 mv=(16,-90)
```

FFmpeg C passes the luma MV directly to chroma MC when `ss_h == 0`
(`vp9_mc_template.c:81`, `:122`, `:157`); there is no rescale.

Temporary Wedeo `trace!(target: "vp9_mc", ...)` in `mc.rs::mc_chroma` for
row=0, col=32 measured:

```text
mv=(22,-90) ss_h=false ss_v=false mx=44 my=-180
mv=(6,-19)  ss_h=false ss_v=false mx=12 my=-38
```

i.e. Wedeo doubled the MV via `1 << !ss_h`. Removing that scaling and
rebuilding produced **no CRC improvement**: frame 1 still first differs at
U(32,0), Wedeo 0 / FFmpeg 135, with 83778 U and 83506 V differing pixels.
Hypothesis B is disproved; the change was reverted.

## State at handoff

- No source changes remain; `git status --short` is empty; HEAD `d86b9d2`.
- No commit was made and nothing was pushed.
- `/var/cache/zstd5/wedeo-target-vp9-rem/release/wedeo-framecrc` still holds
  the experiment-B binary; rebuild from the clean worktree before reuse.
- Scripts/logs: `/tmp/ai-agent/vp9_rem_sweep.py`, `vp9-rem-verify.log`,
  `vp9_rem_ff_hdr.out`, `vp9_rem_ff_chroma2.out`, `vp9_rem_w_mc_trace.log`,
  `vp9_rem_w444_patched.yuv`, `vp9_rem_ff444_f01.yuv`,
  `vp9_rem_w444_fix.yuv`.

## Suggested next step (not attempted)

For 4:4:4 (`ss_h=false, ss_v=false`), trace chroma reconstruction coverage
in `recon.rs` (`uv_end_x`, `uv_end_y`, `uv_w4`, `uv_frame_w`, `uv_stride`) and
compare the first 4x4 chroma write at pixel (32,0) against FFmpeg
`vp9recon.c` / `vp9_mc_template.c`. The measured symptom is not the MV value:
the whole chroma region from x=32 is left at zero while luma is already
bit-exact. Do not patch `loopfilter.rs` or high-depth-format code on this
evidence.

## vp90-2-05-resize.ivf: scaled MC implemented but not sufficient (2026-11-27)

GDB at `vp9.c:805`/`:884`: the 5th decode header is 282x173 with a 352x288
reference, `mvscale=(20450,27275)`, `mvstep=(19,26)`. FFmpeg `vp9recon.c:444`
confirms the scaled MC path. Wedeo's `mc.rs` only implements non-scaled
prediction.

A full scaled path was implemented (mvscale/mvstep parsing, mc_luma_scaled,
mc_chroma_scaled, scaled 8tap and bilinear filters with edge emulation). All
10 frames decoded after fixing two negative-slice-offset panics, but frame 3
CRC still differed (`0x17261146` vs `0x7557ec0c`) and the differing-pixel
count did not drop. Gate 1 failed, so the patch was reverted; no commit.

Critical finding: FFmpeg's CLI inserts swscale automatically for this vector,
upscaling 282x173 to 352x288 before computing framecrc. A decoder-only fix
cannot make this vector bit-exact; output scaling must be handled too. Do not
retry scaled MC expecting framecrc parity on this vector.

---

## RESOLVED: 8-bit 4:4:0 pixel format (commit 0bc9420, 2026-11-27)

FFmpeg gdb at `vp9.c:520` measured `bits=0 ss_h=0 ss_v=1 pix_fmt=31`, i.e.
8-bit 4:4:0 maps to `AV_PIX_FMT_YUV440P`. Wedeo had no such PixelFormat, so
`output_pixel_format(8,false,true)` returned None, the keyframe was dropped,
and sticky config then parsed inter frames as 4:2:0 (9 frames of 21600 bytes
instead of FFmpeg's 10 frames of 28800 bytes).

Added `PixelFormat::Yuv440p` and the mapping, plus framecrc chroma geometry
(0,1,1) and hflip/crop exhaustive matches. Verified frame count 9 -> 10, frame 0
CRC exact (`0x4a56762b`), all luma bytes exact, full sweep unchanged at
220/226 with no new failures. Remaining inter-frame chroma differences are a
separate root cause (suspected sub-8x8 chroma MV under mixed subsampling).

Follow-up: the inter-frame chroma differences were in sub-8x8 chroma MC for
4:4:0 (`ss_h=0, ss_v=1`). FFmpeg's `vp9_mc_template.c` covers the whole 8x8
luma region with one 8x4 chroma block, averaging the two vertical sub-block
MVs for 8x4/4x4 and using separate left/right MVs for 4x8/4x4. Wedeo used a
single 4x4 chroma prediction with the first sub-block MV, so the right half
of `bs=11` block (row 0, col 4) used `(27,2)` instead of `(9,0)` (FFmpeg gdb
at `vp9_mc_template.c:157`, `ss_h=0 ss_v=1`). Added `sub8x8_chroma_mc_440`
and the matching 8x4 compound-average size in `mc.rs`. All 10 frames now
match FFmpeg bit-for-bit (framecrc and raw YUV, all planes exact); the sweep
improves to 220/226 with no new failures.

---

## Stage 3 complete: u16 MC (commit 366b35a, 2026-11-27)

`mc.rs` now has a `McSample` trait: 8-bit keeps the original SIMD dispatch,
10/12-bit uses scalar 8tap/avg kernels with i32 intermediates clipped to
`(1<<bit_depth)-1`. `mc_luma`, `mc_chroma`, `mc_apply` and `emu_edge` are generic
over u8/u16 with bit depth from frame config. Seven new tests use golden values
from a direct copy of FFmpeg's `vp9dsp_template.c` reference program (the first
version of that program mistakenly applied the -3 offset twice, making the Rust
code look wrong; after correction they matched exactly).

Verified: 8-bit sweep unchanged at 220/226 with identical match names, 1226
workspace tests pass. The 8-bit gate remains; stage 4 (u16 loop filter with
bit-depth-scaled thresholds, then gate removal) is still open.

## vp91-2-04-yuv422.webm: divergence in chroma loop filter (2026-11-27)

Frame 0 bit-exact; frame 1 first differences U(0,16) Wedeo 105 vs FFmpeg 101,
V(0,17) 123 vs 124; luma fully exact. Paired extraction on luma 8x8 block
(32,0) / chroma (16,0): FFmpeg gdb `inter_pred_8bpp` and
`mc_chroma_unscaled` measured `bs=BS_8x8`, mv=(90,-1),
filter=FILTER_8TAP_SHARP, mx=90 my=-2 x=21 y=-1, mx_frac=10 my_frac=14,
src_stride_u=80, dst_u[0]=76, dst_v[0]=125. Wedeo tracing produced identical
inputs and predictions; forcing Scalar backend changed nothing, excluding SIMD.
Coefficients also matched exactly: `{62,0,...,-73,0,73,0,73,0,73,-73}`,
eob=16, uvtx=TX_4X4.

After reconstruction Wedeo's 4x4 U block starts `[80,84,97,92]`; disabling
`loop_filter_frame` left final pixels `[105,83,79,104]` unchanged. FFmpeg's
final U row0[16..19] is `[101,79,75,100]` and `-skip_loop_filter all` does not
change it (that FFmpeg tree has no skip_loop_filter check for VP9). Conclusion:
upstream MC/MV/coefficients are aligned; the remaining difference is in 4:2:2
chroma loop filtering (`loopfilter.rs`), which was out of scope for that agent.
No patch. Evidence: `/tmp/ai-agent/` vp9-422 logs (ephemeral).

## vp90-2-05-resize.ivf: corrected ground truth and full design (2026-11-27)

Earlier measurements used the wrong FFmpeg reference CRC. Ground truth:

- The actual sweep command `ffmpeg -bitexact -i FILE -f framecrc -` uses
  default `SWS_BICUBIC` autoscale and produces `0x8974d724`, not the
  previously cited `0x7557ec0c` (which is `bicubic+accurate_rnd` or
  `+bitexact`). `-sws_flags A/B` confirmed only plain bicubic matches the
  default CRC.
- FFmpeg CLI inserts `scaler_out`: 282x173 is upscaled to 352x288.
- Frames 0-2 are bit-exact. Frames 3-5 differ in raw decode CRC even before
  scaling. Frames 6-9 are natively 352x288 but still differ because they
  reference the resized frames. Both scaled MC AND output bicubic scaling are
  required; neither alone suffices.
- `tests/fate/src/framecrc.rs` packs planes by `video.width/height` with no
  display-size concept. `wedeo-cli` has no auto-scale. Scale target is the
  first decoded frame size 352x288, not the IVF header's 320x240.

Full design (182 lines) in `/tmp/ai-agent/VP9_RESIZE_SCALE.md`: add a
`wedeo-filter-scale` crate porting swscale bicubic with fixed-point rounding
and odd chroma-height edge handling, wire it into framecrc and CLI, restore
scaled MC, and debug against `-noautoscale` raw CRCs. No patch; too large for
one 20-minute round.

---

## CORRECTION: vp91-2-04-yuv422 divergence is compound inter prediction (2026-11-27)

The earlier "chroma loop filter" diagnosis was wrong: the matched block with
coefficients `{62,...,-73}` and mv=(90,-1) was a frame 0 8x8 block, not the
divergent frame 1 16x8 block.

Paired values at frame 1 SB0, chroma U row0[16..19]:

- FFmpeg: 101/79/75/100 both before and after chroma LPF - the LPF does not
  change these pixels, so it is not the cause.
- FFmpeg gdb at `vp9recon.c:646 itxfm_add`: same coefficients and eob=3,
  pre-ITXFM prediction `[109,84,76,98]`, post-ITXFM `[101,79,75,100]`.
- Wedeo `INTER_POST` trace: bs=16x8, pred_u0=113, coefficients
  `[-105,0,0,0,-128,...]`, eob=3, reconstructed `[105,83,79,104]`; disabling
  `loop_filter_frame` changed nothing.
- The residual side matches; prediction differs: 109 vs 113.
- FFmpeg `mc_chroma_unscaled`: x=16,y=-1,mx=11,my=14,filter=regular(1),
  refidx=0, bw=bh=8, first predictor `[117,85,75,92]`. The +4 difference
  likely comes from the second predictor/compound averaging.
- Frame 0 U/V match byte-for-byte for row0..9 col8..35, so reference-frame
  LPF writes cannot explain it.

Next step: pair Wedeo's second predictor MV/filter and compound averaging
rounding at that 16x8 block. No patch.

---

## RESOLVED: 4:4:0 sub-8x8 chroma MC (commit 2d6dcff, 2026-11-27)

FFmpeg gdb at `vp9_mc_template.c:157`: the first divergent block is bs=11
(Bs4x8), row=0 col=4, mv[0]=(27,2), mv[1]=(9,0). FFmpeg predicts the left and
right 4x4 chroma halves with their own MVs; the 8x8 luma block covers 8x4
chroma in 4:4:0 (ss_h=0, ss_v=1). Wedeo predicted once with the first MV.

Added `sub8x8_chroma_mc_440` with vertical-MV averaging and per-half prediction,
and corrected the compound average region to 8x4. Verified `yuv440` all 10
frames byte-exact on Y/U/V; VP9 sweep 215/226 -> 220/226 with only `resize`
and `yuv422` remaining. Full workspace tests pass.

## Stage 4 partial: u16 loop filter (commit dd63189, 2026-11-28)

`loopfilter.rs` now has a `LpfSample` trait over u8/u16, `clip_u16`, and
bit-depth-scaled E/I/H and flatness thresholds (`1 << (bit_depth-8)`), following
FFmpeg `vp9dsp_template.c`. Eight new tests use golden values from a C reference
copy of the FFmpeg kernel (the first version misused `av_clip_intp2_c`; after
correcting it all matched). Eight-bit path is byte-identical; VP9 sweep stayed
215/226 at integration time (220/226 after the 4:4:0 sub-8x8 fix).

The 8-bit gate was temporarily removed as an experiment and REVERTED: 0/8
high-depth vectors were exact, 5 had wrong first-frame CRC and 3 errored out.
Root cause: `recon.rs` and `mc::inter_pred` still pass `Plane`'s byte view to
the u16 kernels, and the emulation edge buffers are still `u8`, so high-depth
samples get truncated. Next step: wire the `Plane::U16` variant through
reconstruction and MC before removing the gate.

---

## Verified u16 wiring plan (2026-11-28, no patch)

# VP9 high-depth U16 wiring: verification and implementation design

Scope: verification only. No source file was modified. Worktree
`/home/hakergeniusz/.local/share/opencode/worktree/695063/vp9-highdepth-verify`,
HEAD `f827688`, tree clean at the end of this review.

## 1. Verified state

### 1.1 Unit tests

`cargo test -p wedeo-codec-vp9` (flock-serialised, low priority, dedicated
`CARGO_TARGET_DIR=/var/cache/zstd5/wedeo-target-hdverify`) passes:

    test result: ok. 72 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out

Only pre-existing workspace manifest warnings (`minifb`, `wedeo-limits`,
`workspace.package.description/readme`) appear; no VP9 warning or failure.

The 10/12-bit tests are genuine reference comparisons, not self-consistency
checks:

* `idct.rs:2975-3355` hardcodes literal `expected` arrays for
  `itxfm_add` / `itxfm_add_lossless` at 4x4/8x8/16x16/32x32, both depths,
  doc-commented as `FFmpeg-derived reference values (ff_vp9dsp_init_10/12)`.
  `check_u16_case` (2944-2972) feeds the same prediction ramp
  `(i * 37 + 100) & mask` that the FFmpeg DSP harness used.
* `mc.rs:1529-1702` hardcodes literal golden blocks for horizontal, vertical,
  and 2D 8tap plus compound average and `mc_luma` edge emulation at both
  depths. Comments (e.g. 1504-1506, 1531, 1558, 1585) identify the FFmpeg
  `vp9dsp_template.c` reference program as the source. The harness itself is
  still at `/tmp/ai-agent/vp9_lpf_ref.c` for the loop filter.
* `loopfilter.rs:1443-1558` hardcodes literal input/expected arrays, commented
  as a direct copy of `vp9dsp_template.c:1780-1888` instantiated with
  `pixel = uint16_t`, produced by `/tmp/ai-agent/vp9_lpf_ref.c`.
* `intra_pred.rs:802-905` uses hand-computed values from the FFmpeg formulas
  (for example `(128 << (BIT_DEPTH - 8))` and the DC shift sum) rather than
  saved reference arrays. These exercise u16 width and scaling but are
  formula-derived, not captured FFmpeg output.
* `recon.rs:1104-1168` only checks `FrameBuffer` layout and `Plane` variants.
  There is no test that calls `reconstruct_intra_block`,
  `reconstruct_inter_block`, or `mc::inter_pred` with a U16 `FrameBuffer`.

### 1.2 Sweep

A fresh 226-vector sweep was run with
`/tmp/ai-agent/vp9_hdverify_sweep.py` using
`/var/cache/zstd5/wedeo-target-jvthash/release/wedeo-framecrc`. That binary
was built from worktree `h264-jvt-hash`, verified to be at exactly `f827688`,
the same commit as this worktree. Reference was system
`ffmpeg -v error -threads 1 -bitexact -i FILE -f framecrc -`.

    TOTAL=226 EXACT=216 ZERO=8 DIFF=2 ERR=0
    DIFF vp90-2-05-resize.ivf frame=3 got=10 ref=10
    DIFF vp91-2-04-yuv422.webm frame=1 got=10 ref=10

All eight high-depth vectors are `ZERO` (zero emitted frames) and match the
documented list exactly:

    vp92-2-20-10bit-yuv420.webm
    vp92-2-20-12bit-yuv420.webm
    vp93-2-20-10bit-yuv422.webm
    vp93-2-20-10bit-yuv440.webm
    vp93-2-20-10bit-yuv444.webm
    vp93-2-20-12bit-yuv422.webm
    vp93-2-20-12bit-yuv440.webm
    vp93-2-20-12bit-yuv444.webm

The only 8-bit mismatches are the two pre-existing known ones (`resize`,
`yuv422`), so no 8-bit vector regressed. This matches `STATUS.md` and the
earlier `vp9440c_sweep.log` (220/226, 2 fail, 8 zero, 0 error).

## 2. Root blocker

`decoder.rs:225-232` still returns early for `header.bit_depth != 8`, so all
high-depth frames are discarded. A temporary experiment that removed the gate
produced frames but 0/8 exact results (5 wrong first-frame CRC, 3 errors).
`VP9_GAPS.md:494-508` records the cause: `recon.rs` and `mc::inter_pred`
still pass `Plane`'s byte view into reconstruction and MC, and MC emulation
scratch buffers are still `u8`, truncating 10/12-bit samples to their low
byte.

The low-level kernels are already depth-generic:

* `intra_pred::intra_pred<P: Sample>` (`intra_pred.rs:91-119`)
* `idct::itxfm_add<P: ItxfmSample>` / `itxfm_add_lossless<P>` (`idct.rs:2377`,
  `2409`), with `ItxfmSample: Sample` (`2338`) implemented for `u8`/`u16`
* `mc::McSample` (`mc.rs:38-91`) with `u8`/`u16` impls (`93`, `148`);
  `emu_edge` (`412`), `mc_luma` (`447`), `mc_chroma` (`514`), `mc_apply`
  (`612`), `filter_h/v/hv_8tap` (`323`, `340`, `359`), `copy_block` (`376`),
  `avg_block` (`392`) are all generic
* `loopfilter::loop_filter_frame` (`loopfilter.rs:1367-1402`) already dispatches
  to `as_u8_mut()` for 8-bit and `as_u16_mut()` for high depth
* Output framing (`decoder.rs:203-216`, `562-564`) intentionally uses
  `as_u8()` with byte line strides (`y_byte_stride`, `uv_byte_stride`),
  which is correct for 16-bit planar output and needs no change

Therefore the fix is a thin typed-plumbing layer, not a math change.

## 3. Exact functions that still take `&[u8]` on the high-depth path

### recon.rs

* `prepare_intra_edges` (`423-436`): `plane_buf: &[u8]`, `above_buf: &mut [u8]`,
  `left_buf: &mut [u8]`. Hardcoded padding constants at `500` (`127`), `509`
  (`127`), `534` (`129`), `536` (`127`), `559` (`129`), `576` (`129`),
  `583` (`129`). It has no `bit_depth` parameter.
* `reconstruct_intra_block` (`602-940`): luma reads `&fb.y` at `679` and writes
  through `&mut fb.y[dst_offset..]` at `700`, `718`, `727`; U reads `&fb.u`
  at `796` and writes at `815`, `831`, `838`; V reads `&fb.v` at `879` and
  writes at `898`, `914`, `922`. Scratch `vec![128u8; ...]` / `vec![129u8; ...]`
  at `641-642` and `762-763`. Boundary check at `788` and `871` uses
  `fb.u.len()` / `fb.v.len()`, which via `Deref` returns byte length, not
  sample length.
* `reconstruct_inter_block` (`950-1098`): residual writes through byte views
  at `999`, `1002` (Y), `1046`, `1049` (U), `1078`, `1081` (V). It calls
  `mc::inter_pred` at `959`.

All of these byte views come from `Plane`'s `Deref`/`DerefMut`/`Index`
implementations (`recon.rs:115-141`), which return `&[u8]` even for `Plane::U16`
via `as_u8()` (`78-85`). Any `dst_offset` computed in samples indexes the
wrong element for U16.

### mc.rs

* `inter_pred` (`684-934`): allocates `emu_buf = vec![0u8; 160 * 80]` at `711`;
  reads `&fb.y` / `&ref0.fb.y` at `721`/`723`, `&fb.u`, `&fb.v`, `&ref0.fb.u`,
  `&ref0.fb.v` at `772-777`; allocates compound scratch `tmp_y/tmp_u/tmp_v`
  as `vec![0u8; ...]` at `826-828`; averages through byte views at `864-870`,
  `917-924`, `925-932`. Ref1 accesses at `847`, `899-901`, `914` are the same
  problem. Sub-8x8 calls at `716`, `753`, `832`, `876` pass `&mut emu_buf`.
* `sub8x8_luma_mc` (`1303-1390`): `emu_buf: &mut [u8]`; writes `&mut fb.y[...]`
  at `1324`, `1347`, `1371`; reads `&ref_frame.fb.y` at `1326`, `1349`, `1373`.
* `sub8x8_chroma_mc` (`1394-1494`): `emu_buf: &mut [u8]`; writes `fb.u`/`fb.v`
  byte views at `1413-1414`, `1431-1432`, `1475-1476`; reads ref planes at
  `1478-1479`.
* `sub8x8_luma_mc_buf` (`944-1030`): `dst: &mut [u8]`, `emu_buf: &mut [u8]`,
  ref plane byte views at `968`, `990`, `1013`.
* `sub8x8_chroma_mc_444` (`1040-1142`): `dst_u/dst_v: &mut [u8]`,
  `emu_buf: &mut [u8]`, ref planes bound at `1055-1057`.
* `sub8x8_chroma_mc_440` (`1150-1229`): same shape, ref planes at `1165-1167`.
* `sub8x8_chroma_mc_buf` (`1235-1300`): `dst_u/dst_v: &mut [u8]`,
  `emu_buf: &mut [u8]`, ref planes at `1284-1285`.

The `160` emu stride hardcoded at `486`, `494`, `558`, `561`, `566`, `569` is
in samples, so it stays valid once the buffer element type is widened. The
scratch capacity `160 * 80` samples also stays sufficient (max luma window
71 rows).

Not blockers: `refs.rs:50-69` stores the whole `FrameBuffer`, so `RefFrame.fb`
already carries the `U16` variant; `FrameBuffer::new` (`recon.rs:166-196`)
allocates `u16` planes at 10/12-bit; `output_pixel_format` is already mapped.

## 4. Recommended line-level design

### 4.1 Add one typed-plane dispatch trait

In `recon.rs`, next to `Plane` (`after line 110`), add:

    pub trait PlaneSample: crate::intra_pred::Sample {
        fn plane_view(plane: &Plane) -> Option<&[Self]>;
        fn plane_view_mut(plane: &mut Plane) -> Option<&mut [Self]>;
    }
    impl PlaneSample for u8 { ... plane.as_u8() ... plane.as_u8_mut() ... }
    impl PlaneSample for u16 { ... plane.as_u16() ... plane.as_u16_mut() ... }

This gives every generic helper a safe way to obtain typed slices from both
the working `FrameBuffer` and each `Arc<RefFrame>` without exposing `Deref`.
Keep `Deref`/`Index` for output framing only; reconstruction must stop using
them.

### 4.2 Make `prepare_intra_edges` generic (`recon.rs:423-588`)

New signature:

    fn prepare_intra_edges<P: PlaneSample>(
        plane_buf: &[P],
        stride: usize,
        px: usize, py: usize,
        frame_w: usize, frame_h: usize,
        tile_col_start_px: usize,
        tx_px: usize,
        above_buf: &mut [P],
        left_buf: &mut [P],
        mode: IntraMode,
        have_right: bool,
        bit_depth: u8,
    ) -> IntraMode

Replace only the literal pad values, using FFmpeg `vp9recon.c:153`, `173`,
`212` (`128 << (bit_depth - 8)`):

    line 500, 509: P::from_i32((128 << (bit_depth - 8)) - 1)
    line 534:      P::from_i32((128 << (bit_depth - 8)) + 1)   // have_top true
    line 536:      P::from_i32((128 << (bit_depth - 8)) - 1)
    line 559, 576, 583: P::from_i32((128 << (bit_depth - 8)) + 1)

All other lines of the function are index arithmetic and stay unchanged.

### 4.3 Split `reconstruct_intra_block` into wrapper + generic impl

Keep the public signature at `602-607`. Replace the body with a match on the
three `Plane` variants (a frame never mixes variants):

    match (&mut fb.y, &mut fb.u, &mut fb.v) {
        (Plane::U8(y), Plane::U8(u), Plane::U8(v)) =>
            reconstruct_intra_impl(y, u, v, fb.y_stride, fb.uv_stride, block, header, tile_col_start),
        (Plane::U16(y), Plane::U16(u), Plane::U16(v)) =>
            reconstruct_intra_impl(y, u, v, fb.y_stride, fb.uv_stride, block, header, tile_col_start),
        _ => return Err(Error::InvalidData), // mixed variants cannot occur
    }

Define

    fn reconstruct_intra_impl<P: crate::idct::ItxfmSample>(
        y: &mut [P], u: &mut [P], v: &mut [P],
        y_stride: usize, uv_stride: usize,
        block: &BlockInfo, header: &FrameHeader, tile_col_start: usize,
    )

which is the current body with these substitutions:

* `641-642`, `762-763`: scratch typed `vec![P::from_i32(128 << (bit_depth-8)); ...]`
  and `vec![P::from_i32((128 << (bit_depth-8)) + 1); ...]`. Same constants as
  4.2, matching FFmpeg's `check_intra_mode` initial values.
* `679`, `796`, `879`: pass `&y[...]` / `&u[...]` / `&v[...]` (whole typed
  slice) to `prepare_intra_edges`, plus the new `bit_depth` argument.
* `700`, `718`, `727`, `815`, `831`, `838`, `898`, `914`, `922`: pass
  `&mut y[dst_offset..]`, `&mut u[dst_offset..]`, `&mut v[dst_offset..]`
  where `dst_offset` stays in samples.
* `788`, `871`: compare `dst_end` against `u.len()` and `v.len()` (samples),
  not `fb.u.len()` / `fb.v.len()` (bytes).
* Calls to `intra_pred` (`699`, `814`, `897`) and `itxfm_add` /
  `itxfm_add_lossless` (`717`, `726`, `830`, `837`, `913`, `921`) need no
  change other than the slice type, since both are already generic.

### 4.4 Make `mc::inter_pred` generic (`mc.rs:684-934`)

Change to:

    pub fn inter_pred<P: McPlane>(
        fb: &mut FrameBuffer,
        block: &BlockInfo,
        ref_slots: &[Option<Arc<RefFrame>>; 8],
        ref_idx: &[u8; 3],
        ss_h: bool, ss_v: bool,
    )

where `McPlane: McSample + PlaneSample` (defined in `mc.rs` or reuse
`recon::PlaneSample` plus the existing `McSample` bound). Add a thin public
wrapper that dispatches on `fb.y` and calls `inter_pred_impl::<u8>` or
`inter_pred_impl::<u16>`. Inside the impl:

* `711`: `let mut emu_buf = vec![P::default_or_zero(); 160 * 80]`; use
  `P::from_i32(0)` from `Sample` (no `Default` bound needed), i.e.
  `vec![P::from_i32(0); 160 * 80]`.
* `721`, `772-773`, `864`, `918`, `926`: obtain dst slices with
  `P::plane_view_mut(&mut fb.y)` etc. before taking the subslice at
  `dst_off` / `u_off`. Use disjoint field borrows; never borrow all of `fb`
  mutably and a ref frame through `fb`.
* `723`, `775-777`, `847`, `899-901`, `914`, and every `&ref_frame.fb.y/u/v`
  in the sub-8x8 helpers: replace with `P::plane_view(&ref.fb.y).unwrap()`.
  Since all frames in a VP9 stream share bit depth, a mismatch is invalid;
  return early (skip) rather than panic if the variant differs.
* `826-828`: allocate scratch as `vec![P::from_i32(0); fb.y_stride * avg_bh]`
  and the two UV buffers with `fb.uv_stride * avg_uv_bh`. Strides remain in
  samples.
* `716`, `753`, `832`, `876`: pass the widened `&mut emu_buf`.

### 4.5 Generalise the seven sub-8x8 MC helpers

For each of `sub8x8_luma_mc_buf` (`944`), `sub8x8_chroma_mc_444` (`1040`),
`sub8x8_chroma_mc_440` (`1150`), `sub8x8_chroma_mc_buf` (`1235`),
`sub8x8_luma_mc` (`1303`), `sub8x8_chroma_mc` (`1394`), and the
`inter_pred_impl` call sites, change:

    &mut [u8]  ->  &mut [P]
    &[u8]      ->  &[P]
    emu_buf: &mut [u8]  ->  emu_buf: &mut [P]

with `P: McSample`. All index math and MV averaging (`rounded_div_mv_x2/x4`,
`652-670`) are sample-count based and need no change. `mc_luma` / `mc_chroma`
already accept `P`, so their call sites only need typed ref/dst slices.

### 4.6 Make `reconstruct_inter_block` generic similarly

Keep the public signature at `950-955`, dispatch on the `Plane` triple, and
delegate to:

    fn reconstruct_inter_impl<P: ItxfmSample + McSample>(
        y: &mut [P], u: &mut [P], v: &mut [P],
        fb: &FrameBuffer, block: &BlockInfo, header: &FrameHeader,
        ref_slots: &[Option<Arc<RefFrame>>; 8],
    )

Pass the typed slices to `mc::inter_pred` (or its impl) and use
`&mut y[dst_offset..]`, `&mut u[dst_offset..]`, `&mut v[dst_offset..]` for the
residual `itxfm_add` calls at `999-1009`, `1046-1056`, `1078-1088`.

### 4.7 Remove the gate last

Only after 4.2-4.6 are unit-tested and the sweep shows high-depth vectors
producing frames, delete `decoder.rs:225-232`. Then rerun:

    cargo test -p wedeo-codec-vp9
    cargo clippy -p wedeo-codec-vp9 --all-targets -- -D warnings
    python3 /tmp/ai-agent/vp9_hdverify_sweep.py <newly built wedeo-framecrc>

Acceptance: sweep stays at 220/226 for 8-bit with the same two known diffs,
and high-depth vectors become EXACT or produce a small, measurable first
difference. Expect frame byte counts to double (8-bit 21600 vs high-depth
43200 for 4:2:0), which the existing `as_u8()` output path and byte line
strides already handle.

## 5. Known risks after wiring

* `VP9_GAPS.md:503-504` gate-removal experiment still had 3 error cases after
  the byte truncation was visible; after typed plumbing these need rechecking.
* FFmpeg `vp9recon.c:105-117` uses pre-loopfilter `s->intra_pred_data[]` at
  superblock row boundaries. Wedeo's `prepare_intra_edges` always reads
  `plane_buf[(py-1)*stride + px]`. This is an existing 8-bit divergence
  candidate that may also limit high-depth parity; it is outside this wiring.
* `intra_pred::Sample::from_i32` does not clip. The scaled pad constants are
  within range by construction, so no extra clipping is needed.
* Mixed `Plane` variants in one frame or between current/reference frames
  cannot occur in conformant streams; handle defensively with early return.

## 6. Effort estimate for one 20-minute round

The mechanical edits above are roughly 10 targeted functions and about
150-220 changed lines, with no new algorithm. A competent implementer with
this document can finish steps 4.1-4.6 in one 20-minute editing round, then
compile and fix borrow/type errors in a second round. Step 4.7 (gate removal)
plus a full sweep and parity triage will not fit in that same 20 minutes,
because the gate experiment previously needed ground-truth extraction when
0/8 vectors matched. Budget: one round for the typing refactor, one round for
compilation/unit tests, one round for gate removal and sweep triage.

## yuv422 second-predictor edge samples (2026-11-28, no patch)

FFmpeg gdb at `vp9_mc_template.c:423/436`, `col=4 row=0 bs=7 comp=1`:
mv0=(11,-1), mv1=(-81,3), filter=1, ref0=0, ref1=2. ref0 U row0
`[117,85,75,92,79,78,86,92]`; compound U row0
`[109,84,76,98,86,79,85,95]`; derived second predictor
`[100,82,76,103,92,79,83,97]`.

Wedeo trace at the same block: identical mv0/mv1/filter/ref indices and
identical ref0 U row0, but ref1 U row0 `[108,91,85,111,101,80,84,99]`,
averaged U row0 `[113,88,80,102,90,79,85,96]`. The difference is in the
second predictor's edge-emulation source pixels: FFmpeg emu row0 cols 0..14
`[108,97,94,105,102,76,67,100,95,80,84,97,92,96,116]` vs Wedeo
`[116,105,102,113,110,84,75,108,103,80,84,97,92,96,116]`. Only the first
nine columns (chroma source x=7..15) differ; the rest match exactly.

Conclusion: MV, filter, averaging rounding and first predictor are aligned.
The cause is left-edge chroma samples of the reference frame (x=7..15), i.e.
reference-frame content or left-edge loop filtering, outside the 8-bit
compound-only scope. Frame 0 framecrc matches on both decoders (`0x37ac72e4`).
No patch. Evidence: `/tmp/ai-agent/vp9422comp-*logs` (ephemeral).

## RESOLVED: high-depth reconstruction through typed planes (commit ac50b39, 2026-11-28)

`recon.rs` dispatches intra/inter reconstruction to `PlaneSample` u8/u16
implementations with FFmpeg-scaled neighbour defaults `128 << (bit_depth-8)`
plus/minus one (8-bit keeps 127/129). `mc::inter_pred` and all sub-8x8 helpers
now operate by sample type; emulation edges and compound scratch buffers are
allocated at sample width, so high-depth samples are no longer truncated. The
8-bit gate was removed. Also fixed 10/12-bit 4:4:0 output format, 12-bit
framecrc plane geometry, and colour-config inheritance on high-depth inter
frames.

Verified with a freshly built binary: all 8 high-depth vectors now decode with
correct frame counts and no panics; 10-bit 4:2:0 and 10-bit 4:4:4 are
byte-exact. VP9 sweep 216/226 -> 220/226 with 0 ZERO-FRAME and 8 CRC
differences (resize, yuv422, plus six 10/12-bit chroma-subsampling vectors).
8-bit match names unchanged. Note: the earlier stale sweep script pointed at
`wedeo-target-verifydocs` and misleadingly reported 214/226 with 8 ZERO.

## Resize scaling: new swscale graph backend (2026-11-28, no patch)

FFmpeg n9.1's CLI uses the NEW swscale graph backend, not legacy
`initFilter`/`ff_swscale`: `vf_scale.c:321` calls `sws_alloc_context()` with
`is_legacy_init=0` and runs `ff_sws_graph_run()` (graph.c + ops/uops). The
system ffmpeg has `--disable-debug`, so op-list logs are unavailable.

A Python prototype of the legacy bicubic path (`/tmp/ai-agent/scale_proto.py`)
scaled 282x173 -> 352x288 with 6276/101376 luma and 1392/25344 U pixels off by
one: algorithm family is right but rounding differs from the new backend, so it
is not bit-exact.

Frame-offset trap: in the raw dump frames 0-2 and 6-9 are 152064 bytes while
frames 3-5 are 73320 bytes; frame 3 starts at `3*152064`, not `3*73320`.

Gate 1 remains blocked by decoder-side scaled MC: frames 6-9 are natively
352x288 yet still differ because they reference resized frames. Debug order:
restore scaled MC against `-noautoscale` raw CRC (`0x86150420` for frame 3),
then implement output bicubic against the autoscale CRC `0x8974d724`.

Full design: `/tmp/ai-agent/VP9_SCALE_IMPL_DESIGN.md` (ephemeral). Estimate:
5-10 min to capture the op-list with a debug ffmpeg/C harness, 20-40 min for
150-250 lines of Rust plus golden tests, and scaled MC is a separate prerequisite.

## RESOLVED: high-depth coefficient precision (commit 3a3b67b, 2026-11-28)

12-bit 4:2:0 frames had 3473 pixels with delta exactly +2048. FFmpeg `vp9block.c:906-918` writes 32-bit coefficients with `AV_WN32A` on high-depth paths; Wedeo cast every coefficient through `as i16 as i32`, truncating 12-bit values. `block.rs` now keeps high-depth coefficients at 32-bit precision. Verified with a freshly built binary: 12-bit 4:2:0 and 12-bit 4:4:4 are byte-exact; VP9 sweep 218/226 -> 220/226 with 0 ZERO-FRAME, 0 ERROR and no new failures (the six remaining diffs are a strict subset of the prior eight).

## yuv422 divergence is in the invisible ALTREF reference frame (2026-11-28, no patch)

Paired gdb (ffmpeg_g vp9recon.c:377) and Wedeo source-level trace at frame 1,
SB0, bs=7 comp=1 col=4 row=0, second predictor mx=15 my=6 x=10 y=0, ref1=slot 2:
- FFmpeg ref1 U row0 cols0..21 `[128,130,102,82,103,108,91,108,97,94,105,102,76,67,100,95,80,84,97,92,96,116]`
- Wedeo ref1 row0 `[128,131,106,84,111,116,99,116,105,102,113,110,84,75,108,103,80,84,97,92,96,116]`

ref0 matched exactly, and MV/filter/ref indices/averaging were aligned; the
emulated window 7..21 is inside the 80-wide chroma plane, so out-of-bounds
reading is ruled out. The difference comes from slot 2's own content.

FFmpeg gdb: slot 2 is decode-order frame 3 (packet pts=20, show_frame=0,
refresh_frame_flags=4), later displayed by show_existing_frame at pts=180 - an
invisible ALTREF frame. That frame's U/V row0 differ at cols1..15 while Y is
byte-identical; -skip_loop_filter showed it is not LPF. Both decoders used
comp=0 with matching MVs (236,0)/(90,-1) and matching emulated inputs at
x=21 y=-1, so the ALTREF chroma error is in single-reference reconstruction /
ITXFM coefficient handling, NOT the 8-bit compound path.

Next round should target invisible ALTREF single-reference chroma reconstruction,
not compound averaging. No patch, gates not run, tree clean.
