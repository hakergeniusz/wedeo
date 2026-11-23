//! Registration conformance checks for `wedeo-filter-transpose`.
//!
//! The transpose filter is linked into this test binary by the workspace FATE
//! harness, so the scalar transpose behaviour is covered by the crate's own
//! unit tests (synthetic frames for all four directions). Here we verify that
//! the `inventory::submit!` registration is actually visible to the filter
//! registry and that the descriptor exposed to the graph matches FFmpeg's
//! `ff_vf_transpose`.

use wedeo_filter_transpose as _;

#[test]
fn transpose_factory_is_registered() {
    assert!(
        wedeo_filter_transpose::is_registered(),
        "transpose filter factory was not submitted to the global inventory"
    );
}

#[test]
fn transpose_descriptor_matches_ffmpeg() {
    let descriptor = &wedeo_filter_transpose::TRANSPOSE_DESCRIPTOR;
    assert_eq!(descriptor.name, "transpose");
    assert_eq!(descriptor.description, "Transpose input video.");
    assert_eq!(descriptor.inputs.len(), 1);
    assert_eq!(descriptor.outputs.len(), 1);
    assert_eq!(descriptor.inputs[0].name, "default");
    assert_eq!(descriptor.outputs[0].name, "default");
}
