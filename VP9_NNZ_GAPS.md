# VP9 NNZ context divergence (210x202 frame 1)

Historical ground truth measured 2026-11-27 at HEAD `5e47eb4`. The root cause
was fixed by `d7ac5ba`; see the RESOLVED section in `VP9_GAPS.md`. Current VP9
parity is 222/226; see `STATUS.md`.

## Pre-loop-filter evidence

Vector: `vp9-test-vectors/vp90-2-03-size-210x202.webm`, frame 1.
Pre-loop-filter sample at x=203, y=160: FFmpeg 157, Wedeo 155. This confirms
the divergence is upstream of the loop filter.

The difference is not one pixel: the whole 32x32 block at (192,160) has a
different residual shape. Wedeo shows a ramp across x=192..207, while FFmpeg
is flat 157 across x=192..204.

Target block: FFmpeg `row=20, col=24` (8x8 units, pixel (160,192)),
`bs=BS_32x32`, `tx=TX_32X32`, `skip=0`, `mv=(0,0)`.

## Coefficient evidence

Quantizer multipliers agree: DC 272, AC 394. Probability tables agree:
Wedeo `header.coef[3][0][1][0]` begins
`[[36,41,235],[29,36,193],[10,27,111],[0,0,0]]`, which is exactly what
FFmpeg uses at the matching band/context when fed its NNZ context.

Dequantized coefficients:

- FFmpeg: `{0: 544, 32: -394, 96: 197}`
- Wedeo: `{0: -272, 32: 394, 36: 197, 64: -197, 65: 394, 128: -197}`

FFmpeg token probes: `(i=0, rc=0, val=4, qmul=272, nnz=1, p=[29,36,193])`,
then `(i=2, rc=32, val=2, qmul=394, nnz=3, p=[0,0,0])`, then
`(i=10, rc=96, val=1, qmul=394, nnz=0, p=[36,41,235])`.
Wedeo token probes start `i=0 nnz=0 band=0 eob_prob=36`, then `i=1 nnz=2
band=1 eob_prob=110`, i.e. a completely different token stream.

## First measured divergence

At the first coefficient of the target block:

- FFmpeg: `nnz = a[0] + l[0] = 1 + 0 = 1`, EOB probability 29.
- Wedeo: `nnz = 0`, EOB probability 36.

That context difference cascades into different tokens, coefficients and the
32x32 residual. Bitstream MVs are zero in both decoders, so motion compensation
cannot explain it.

The IDCT 2-D pass order was checked and is index-equivalent between FFmpeg and
Wedeo. A temporary "transposed intermediate buffer" patch was tried and
reverted as a misdiagnosis. Do not patch the loop filter or IDCT again.

## Next measured step

For every inter block of frame 1 in raster order, compare FFmpeg
`above_y_nnz_ctx[48]` and `left_y_nnz_ctx[(row&7)<<1]` against Wedeo
`td.above.coef[0][48]` and `td.left.coef[0][row&15]`. Find the first block
where they diverge, then inspect `MERGE_CTX`/`SPLAT_CTX` in FFmpeg's
`vp9block.c` versus `merge_ctx`/`splat_val` in Wedeo's `block.rs` and the NNZ
writeback for 32x32 blocks.

Evidence file: `/tmp/ai-agent/VP9_PRESAMPLE_GT.md` (ephemeral).
