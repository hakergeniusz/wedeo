# Credits and third-party code

Wedeo is a pure-Rust rewrite of FFmpeg. Most code is translated directly from
FFmpeg's C sources (LGPL-2.1-or-later) by this project's contributors.

## OxiMedia derived code

The crate `crates/wedeo-limits/` contains code derived from OxiMedia:

- Source: `crates/oximedia-codec/src/util/limits.rs` at
  https://github.com/cool-japan/oximedia
- Copyright 2024-2026 COOLJAPAN OU (Team Kitasan)
- Licensed under the Apache License, Version 2.0
  (http://www.apache.org/licenses/LICENSE-2.0)
- A copy of the Apache 2.0 text is included at
  `crates/wedeo-limits/LICENSE-APACHE`.
- The originating file is attributed in `crates/wedeo-limits/src/lib.rs`.

Modifications made in Wedeo: the API returns `wedeo_core::error::Result`
instead of `Result<_, String>`, and the crate declares `#![forbid(unsafe_code)]`.
The safety guards and their arithmetic are otherwise unchanged.

## License compatibility note

Wedeo's workspace is licensed `LGPL-2.1-or-later`. Apache 2.0 is not compatible
with LGPL-2.1-only, but it is compatible with GPLv3/LGPLv3. Wedeo therefore
exercises its "or later" option: any distribution that includes Apache 2.0
derived code is distributed under LGPL-3.0-or-later for the combined work,
while each third-party source file keeps its original Apache 2.0 notice.

References: GNU license list, entries for Apache License 2.0 and LGPL 2.1
(https://www.gnu.org/licenses/license-list.html).
