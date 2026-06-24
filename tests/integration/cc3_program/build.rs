//! Build script: classify the target so the program body compiles as a
//! freestanding C-ABI program on the native Tier-1 targets and as an inert
//! host stub otherwise. Mirrors the other
//! `tests/integration` build scripts.

fn main() {
    rustos_itest_harness::emit_target_cfg();
    println!("cargo:rerun-if-changed=build.rs");
}
