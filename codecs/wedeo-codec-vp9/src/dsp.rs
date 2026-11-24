// VP9 DSP dispatch — re-exports inverse transforms and intra prediction.
//
// This module is a thin grouping layer. Call sites can import from here or
// directly from `idct` / `intra_pred`.

pub use crate::idct::{
    iadst4, iadst8, iadst16, idct4, idct8, idct16, idct32, itxfm_add, itxfm_add_lossless, iwht4,
};
pub use crate::intra_pred::intra_pred;
pub use crate::simd::{
    SimdBackend, active_backend, available_backends, avg_block, avg_block_with_backend,
    filter_h_8tap, filter_h_8tap_with_backend, filter_hv_8tap, filter_hv_8tap_with_backend,
    filter_v_8tap, filter_v_8tap_with_backend,
};
