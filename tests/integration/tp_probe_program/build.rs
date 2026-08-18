//! Build script: enable the `tp_probe` cfg when the adversarial thread-pointer
//! fixture is built for the freestanding riscv64 target, so `src/main.rs`
//! compiles as a real U-mode program there and as an inert host stub
//! everywhere else.
//!
//! Unlike the portable fixtures (`el0_yielder_program`, `mem_map_program`)
//! this one must *write the psABI thread pointer* to act as a hostile program,
//! and `tp` (x4) has no architecture-neutral spelling. The instruction-set
//! decision therefore lives here, in build glue, exactly as
//! `lib/abi-trap/build.rs` and `lib/crt0/build.rs` confine their per-target
//! trap and startup selection — so `cargo xtask cfg-check` stays clean and the
//! choice is auditable in one place.

fn main() {
    println!("cargo:rustc-check-cfg=cfg(tp_probe)");
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    if os == "none" && arch == "riscv64" {
        println!("cargo:rustc-cfg=tp_probe");
    }
    println!("cargo:rerun-if-changed=build.rs");
}
