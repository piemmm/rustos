//! Build script: publish the per-target cfg the source selects its body
//! with, exactly as the bare-metal verticals do.

fn main() {
    tairix_itest_harness::emit_target_cfg();
    println!("cargo:rerun-if-changed=build.rs");
}
