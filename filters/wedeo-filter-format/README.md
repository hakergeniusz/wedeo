# `wedeo-filter-format`

- Kind: filter
- FFmpeg reference: `~/Projects/FFmpeg/libavfilter/vf_format.c`
- Owner: one subagent, this crate directory only
- Status: implemented (metadata-only `format` filter, registered via `inventory`)

## Semantics

FFmpeg's `format` filter is `AVFILTER_FLAG_METADATA_ONLY`. It has no
`filter_frame()` callback: `query_formats()` restricts the formats negotiated
on the input/output links, and frames pass through untouched.

The frozen Wedeo `Filter` interface has no link-format negotiation hook, so
this crate parses all five FFmpeg option lists (`pix_fmts`, `color_spaces`,
`color_ranges`, `alpha_modes`, `chroma_locations`), passes frames through
unchanged (never mislabelling pixel data), and records the requested lists as
frame metadata (`wedeo.format.*` keys). See the integrator report for the
exact interface gap.

Conformance tests: `tests/fate/tests/fate_filter_format.rs`.
