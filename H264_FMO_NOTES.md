# H.264 FMO: why Wedeo should not implement it

Measured 2026-11-27 at HEAD `5e47eb4`.

## FFmpeg w62 rejects FMO

`libavcodec/h264_ps.c:755-763` parses `slice_group_count` and rejects anything
above 1:

```c
pps->slice_group_count = get_ue_golomb(gb) + 1;
if (pps->slice_group_count > 1) {
    pps->mb_slice_group_map_type = get_ue_golomb(gb);
    avpriv_report_missing_feature(avctx, "FMO");
    ret = AVERROR_PATCHWELCOME;
    goto fail;
}
```

`h264_slice.c` has no `slice_group` map construction code to mirror.

gdb breakpoint on `h264_ps.c:759` extracted these PPS values:

| Vector | Parsed PPS values |
|---|---|
| FM2_SVA_B | PPS0: `slice_group_count=3`, map type 0 |
| FM1_BT_B | PPS1..7 have counts 8,8,3,2,2,2,7 (map type 0); PPS0 has no FMO |
| FM1_FT_E, SVA_FM1_E | all counts are 1; their names are misleading |

The debug FFmpeg binary exits quickly with zero frames and `non-existing PPS
referenced` / `no frame!` messages for FM1_BT_B, FM2_SVA_B and FM2_SVA_C.

## Wedeo already matches that behaviour

`codecs/wedeo-codec-h264/src/pps.rs:337-342` rejects `slice_group_count > 1`
with `Error::PatchwelcomeNotImplemented`. Implementing FMO would make Wedeo
emit frames exactly where the reference decoder emits none, reducing parity.
The suite ERROR entries for FM2_SVA_B and FM2_SVA_C are therefore correct.

## Historical failure-path hang (fixed at `d86b9d2`)

JVT FM1_BT_B previously timed out in Wedeo (120 s) while FFmpeg exited quickly.
It was a hang on the rejection path, not missing FMO decoding. Two candidate
unbounded waits were identified by code inspection but NOT reproduced as the
cause:

- `shared_picture.rs:278`: `wait_for_row()` waits on a Condvar with no timeout.
- `thread_pool.rs:151`: workers have no `catch_unwind`; if a worker panics,
  `join_oldest_in_flight()` blocks forever on `recv()`.

Do not patch either on hypothesis alone. The next pass must reproduce the hang
with a trace, confirm which wait never returns, then add a bounded wait or
panic-safe result delivery and verify zero frames plus a fast exit, with no new
timeouts in the four-suite baseline (198/283 at HEAD).

## Resolution

Reproduced at HEAD `ce02fc5` with a 30 s timeout and `tracing` probes in
`wait_for_row()`, `worker_loop()` and `join_oldest_in_flight()`. Neither
candidate above was the cause. The trace showed:

- main thread blocked in `join_oldest_in_flight()`, waiting for `sequence_id=0`;
- worker blocked in `wait_for_row(row=1)` on a reference picture with
  `row_progress=-1`;
- no worker panic and no closed result channel.

That picture came from `fill_frame_num_gap()`. FFmpeg's
`h264_slice.c:1547-1548` reports `INT_MAX` progress for gap frames, because no
decode worker will ever publish their rows. Wedeo's mid-grey fallback did not,
so later slices using the non-FMO PPS0 waited forever on a placeholder created
for the FMO-rejected frames.

Two verified changes:

1. `decoder.rs`: mark the mid-grey gap picture complete, matching FFmpeg's
   `ff_thread_report_progress(..., INT_MAX, ...)`.
2. `decoder.rs`: once a PPS is rejected with `PatchwelcomeNotImplemented`, skip
   subsequent slice NALs. FFmpeg w62 leaves the rejected PPS unset and emits no
   frames for FM1_BT_B; Wedeo must not decode the remaining PPS0 slices against
   pictures that were never decoded.

Measured after the fix: FM1_BT_B exits in ~0.08 s with zero framecrc frames
(FFmpeg: zero frames), and the four-suite baseline was 197/283 at that commit
with no new timeouts (198/283 at HEAD after the high-depth IDCT fix).

Full source excerpts, the seven map types and the raster-remap rules are in
`/tmp/ai-agent/H264_FMO_GT.md` (ephemeral; regenerate if needed).
