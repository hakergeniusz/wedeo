# `wedeo-filter-format`

- Kind: filter
- FFmpeg reference: `~/Projects/FFmpeg/libavfilter/vf_format.c`
- Owner: one subagent, this crate directory only
- Status: implemented (metadata-only `format` filter, registered via `inventory`)

## Semantics

FFmpeg's `format` filter is `AVFILTER_FLAG_METADATA_ONLY`. It has no
`filter_frame()` callback: `query_formats()` restricts the formats negotiated
on the input/output links, and frames pass through untouched.

This crate parses all five FFmpeg option lists (`pix_fmts`, `color_spaces`,
`color_ranges`, `alpha_modes`, `chroma_locations`). `query_formats()` narrows
both video pads to the parsed `pix_fmts` list, matching FFmpeg's
`ff_set_common_formats2()`. `FilterFactory::create_with_options` makes those
options reachable from `FilterGraph::add_node_with_options`. No ad-hoc
`wedeo.format.*` frame metadata is attached anymore.

`FormatsConfig` currently carries only pixel/sample formats, so color spaces,
color ranges, alpha modes and chroma locations are parsed and retained but
cannot narrow a pad yet. This is reported as an interface need.

Conformance tests: `tests/fate/tests/fate_filter_format.rs`.
