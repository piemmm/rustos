//! Build script: enable the `freestanding` cfg when the mem-pin fixture
//! program is built for a bare-metal target (`target_os = "none"`), so
//! `src/main.rs` compiles as a freestanding pure-Rust program there and as an
//! inert host stub everywhere else (mirrors `tests/integration/wait_program`).
//!
//! It is deliberately self-contained — it does not depend on the
//! `tests/integration` harness. The aarch64 migration register check is
//! selected through the custom `mem_pin_aarch64` cfg emitted here rather than
//! architecture-conditional source code. The role parameters
//! (bound/within/over bytes) arrive at runtime through the consuming
//! vertical's registry argument vectors, so no geometry environment variables
//! exist here.

fn main() {
    println!("cargo:rustc-check-cfg=cfg(freestanding)");
    println!("cargo:rustc-check-cfg=cfg(mem_pin_aarch64)");
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    if target_os == "none" {
        println!("cargo:rustc-cfg=freestanding");
    }
    if target_arch == "aarch64" {
        println!("cargo:rustc-cfg=mem_pin_aarch64");
    }
    println!("cargo:rerun-if-changed=build.rs");
}
