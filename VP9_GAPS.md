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
216/226 with no new failures. Remaining inter-frame chroma differences are a
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
improves to 216/226 with no new failures.

---

## Stage 3 complete: u16 MC (commit 366b35a, 2026-11-27)

`mc.rs` now has a `McSample` trait: 8-bit keeps the original SIMD dispatch,
10/12-bit uses scalar 8tap/avg kernels with i32 intermediates clipped to
`(1<<bit_depth)-1`. `mc_luma`, `mc_chroma`, `mc_apply` and `emu_edge` are generic
over u8/u16 with bit depth from frame config. Seven new tests use golden values
from a direct copy of FFmpeg's `vp9dsp_template.c` reference program (the first
version of that program mistakenly applied the -3 offset twice, making the Rust
code look wrong; after correction they matched exactly).

Verified: 8-bit sweep unchanged at 216/226 with identical match names, 1226
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
frames byte-exact on Y/U/V; VP9 sweep 215/226 -> 216/226 with only `resize`
and `yuv422` remaining. Full workspace tests pass.
