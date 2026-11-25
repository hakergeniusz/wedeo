# Unsafe code boundary

Wedeo's default policy is `#![forbid(unsafe_code)]`. Every crate that has no
need for low-level operations declares it at its crate root.

Unsafe is deliberately confined to these modules:

- `adapters/wedeo-rav1d/src/lib.rs`: calls rav1d's C-ABI API and copies decoded
  picture planes through raw pointers. Every block has a `SAFETY` comment.
- `codecs/wedeo-codec-h264/src/`: FFmpeg-derived SIMD/FFI kernels
  (`asm_dispatch.rs`, `asm_ffi.rs`, `deblock.rs`, `idct.rs`, `mc.rs`, etc.)
  and tile-threading raw pointers (`thread_pool.rs`, `decoder.rs`).
  The crate root uses `#![deny(unsafe_code)]`; only those modules opt in.
- `codecs/wedeo-codec-vp9/src/simd.rs`: `core::arch` AVX2/SSE2/NEON kernels.
  `codecs/wedeo-codec-vp9/src/decoder.rs`: tile-column threading with raw
  pointers into disjoint grid ranges. The crate root denies unsafe elsewhere.

All unsafe-bearing modules also use `#![deny(unsafe_op_in_unsafe_fn)]`, so
nested unsafe operations must be spelled out explicitly.

Third-party dependencies may contain their own unsafe code (notably `rav1d`
and `rusty_h264-accel`); their licenses are recorded in `CREDITS.md`.
