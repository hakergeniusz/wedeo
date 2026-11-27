# H.264 high-depth gaps

The first section below is the historical ground truth recorded before the fix;
the RESOLVED section at the end records the current state (FRext 44/69).

## FREH10: localized high-depth intra 4x4 reconstruction

Measured 2026-11-27 at HEAD `5e47eb4`. No source change was committed.

The FREH10 vectors named in `test_suites/h264/JVT-FR-EXT.json` are not in the
FATE `FRext/` directory. They live in
`/var/cache/zstd5/wedeo-ref/jvt-vectors/JVT-FR-EXT/`:

- FREH10-1: `WB_10bit_QP21_I-Only_1920x1088.264` (High 10, yuv420p10le,
  1920x1088, 360 frames, one slice per frame, `transform_8x8_mode=false`,
  `slice_qp=33`)
- FREH10-2: `WB_10bit_QP21_1920x1088.264` (High 10, 358 frames)

`freh10.264` in FATE is a different 8-bit High-profile vector, not the FREH10
high-depth case.

### Measured divergence

Frame 0 first differing sample: luma (x=607, y=964), FFmpeg 468, Wedeo 0.
2137 differing bytes, all luma, confined to one four-row band y=964..967,
x=607..939 (MB row 60, columns 37..58). U and V are byte-identical, and rows
outside the band are identical. FREH10-1 has 20 differing frames in the first
194 (0, 60, 75, 97, 98, ...), so this is a localized per-frame defect, not a
global dequant or output-scale error.

### Ruled out by measurement

- Deblocking: disabling it on both decoders leaves the exact same first byte,
  byte count and delta.
- Motion compensation: frame 0 is an IDR I-slice, all affected MBs are
  `intra4x4`; no I_PCM MBs occur in frame 0.
- Output packing/cropping: U/V match byte for byte and only one luma MB-row
  band differs.
- Bitstream parsing: Wedeo trace and FFmpeg gdb agree at MB(37,60) on QP 33,
  MB type I_4x4 and CBP 35; FFmpeg also reports NNZ 14 and intra prediction
  mode 4 for that block.

### What remains

The distinction between high-depth `intra_pred::predict_4x4` and
`dequant_4x4`/`idct4x4_add` on the `u16` plane is not yet measured. Wedeo's
`INTRA_FINAL_ROWS` trace prints the `u8` plane, which the high-depth path does
not write, so those all-zero values are not evidence.

Next measured steps: gdb FFmpeg frame 0 MB(37,60) block 7 with
`sl->poc == 0` and a u16 row stride of 1920 (`sl->linesize` is in pixels),
dumping the prediction before IDCT and the 16 dequantized coefficients before
`ff_h264_idct_add_10_c`; add a temporary `trace!()` in
`codecs/wedeo-codec-h264/src/hires.rs::decode_intra4x4_hires` for the same
block to print u16 prediction, coefficients and post-IDCT rows. Patch only the
kernel that actually diverges, then verify all 20 frames and keep FRext at
43/69 or better.

---

## RESOLVED: high-depth 4x4 IDCT overflow (commit 4a0c0ea, 2026-11-27)

Measured at MB(37,60) block 7 of FREH10-1: u16 4x4 prediction and dequantized
coefficients match FFmpeg exactly, so both were exonerated. The divergence is
after the IDCT: FFmpeg produces 468/994/1011/540 in column 3 while Wedeo's
column was all zero. FFmpeg compiles `h264idct_template.c` with
`dctcoef = int32_t` for BIT_DEPTH > 8; Wedeo reused the i16 butterfly and
truncated a first-pass value of 44800, zeroing the whole column.

`idct.rs` now uses i32 intermediates for u16 4x4 IDCT and DC-only add; the 8-bit
path is unchanged. Verified FREH10-1 is 360/360 frames bit-exact, FREH10-2
differences fell from 160 to 13 frames (a separate issue), and FRext rose from
42/69 to 43/69 with zero errors and no new timeouts.

---

## RESOLVED: high-depth coefficient storage overflow (FREH10-2)

Measured on `WB_10bit_QP21_1920x1088.264`, frame 0 MB(24,60) block at spatial
(4,4): FFmpeg's gdb reports dequantized DC 34496 at QP 33, while Wedeo's
`Macroblock::luma_coeffs` (i16) held -31040, i.e. 34496 - 65536. The other 15
coefficients matched FFmpeg's int32 values. FFmpeg's `sl->mb` stores `dctcoef`
as int32 when BIT_DEPTH > 8 (h264dec.h), so the i16 storage is the cause.

The remaining 12 frames (280..285, 328..333) were resolved by applying the same
full-precision path to inter 4x4 luma residual: the generic u8 pass dequantized
into i16 before the u16 pass consumed those values. `dequant_4x4_wide` now
produces the int32 coefficients and both intra4x4 and inter u16 paths feed them
to `idct4x4_add_wide`.

Verified FREH10-2 is 358/358 frames bit-exact, FREH10-1 stays 360/360, JVT-FR-EXT
stays 43/69 with no new timeouts, and `cargo clippy -p wedeo-codec-h264
--all-targets -- -D warnings` plus `cargo fmt --check` pass.

---

## RESOLVED: FREH10-2 i32 coefficient precision (commit b46b0fc, 2026-11-27)

FREH10-2's remaining 13 divergent frames came from i16 truncation of
dequantized coefficients reaching 34496. `cavlc.rs`, `dequant.rs`, `hires.rs`,
`idct.rs` and `mb.rs` now keep high-depth coefficients at i32 precision end to
end. Verified FREH10-2 358/358 frames bit-exact, FREH10-1 stays 360/360, and
FRext rose from 43/69 to 44/69 with zero errors and no new timeouts.
