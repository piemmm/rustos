//! Build script: enable the `fp_probe` cfg when the floating-point fixture is
//! built for the freestanding riscv64 target, so `src/main.rs` compiles as a
//! real U-mode program there and as an inert host stub everywhere else.
//!
//! The fixture names `f0`-`f31` directly, which has no architecture-neutral
//! spelling, so the instruction-set decision lives here in build glue exactly
//! as `lib/abi-trap/build.rs` and `lib/crt0/build.rs` confine their per-target
//! trap and startup selection — keeping `cargo xtask cfg-check` clean and the
//! choice auditable in one place.

fn main() {
    println!("cargo:rustc-check-cfg=cfg(fp_probe)");
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    if os == "none" && arch == "riscv64" {
        println!("cargo:rustc-cfg=fp_probe");
    }
    println!("cargo:rerun-if-changed=build.rs");
}
