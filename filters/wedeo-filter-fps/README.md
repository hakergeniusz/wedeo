# `wedeo-filter-fps`

- Kind: filter
- FFmpeg reference: `~/Projects/FFmpeg/libavfilter/vf_fps.c`
- Owner: one subagent, this crate directory only
- Status: implemented (constant framerate filter, registered via `inventory`)

## Semantics

Buffers up to two input frames, rescales their timestamps into the output
time base (the inverse of the requested frame rate) and emits a copy at every
output timestamp until the next input frame is due. Frames whose interval has
passed are dropped; long intervals are duplicated.

The end-of-stream tail is emitted through `Filter::flush()` so a `FilterGraph`
can drain delayed frames at EOF, matching FFmpeg's `activate()` status path.
The last frame's `duration` stands in for FFmpeg's input status timestamp, and
`first_output_due` is reproduced. `FilterFactory::create_with_options`
supports the `fps`, `start_time`, `round` and `eof_action` options.

Conformance tests: `tests/fate/tests/fate_filter_fps.rs`.
