//! Build script: enable the `itest_wasm32` cfg when building for the
//! browser target (`wasm32-unknown-unknown`), so the kernel body in
//! `src/lib.rs` is gated on a central cfg name rather than a raw target
//! predicate. Mirrors the bare-metal
//! verticals' build scripts.

fn main() {
    tairix_itest_harness::emit_target_cfg();
}
