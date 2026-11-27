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

## The real remaining issue: failure-path hang

JVT FM1_BT_B still times out in Wedeo (120 s) while FFmpeg exits quickly.
This is a hang on the rejection path, not missing FMO decoding. Two candidate
unbounded waits were identified by code inspection but NOT yet reproduced:

- `shared_picture.rs:278`: `wait_for_row()` waits on a Condvar with no timeout.
- `thread_pool.rs:151`: workers have no `catch_unwind`; if a worker panics,
  `join_oldest_in_flight()` blocks forever on `recv()`.

Do not patch either on hypothesis alone. The next pass must reproduce the hang
with a trace, confirm which wait never returns, then add a bounded wait or
panic-safe result delivery and verify zero frames plus a fast exit, with no new
timeouts in the four-suite baseline (197/283).

Full source excerpts, the seven map types and the raster-remap rules are in
`/tmp/ai-agent/H264_FMO_GT.md` (ephemeral; regenerate if needed).
