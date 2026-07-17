//! Build script for the `tairix-dma-barrier` crate.
//!
//! Sole responsibility, and it is build glue (confines
//! target-conditional decisions to the architecture ports and the build
//! glue; a build script is build glue): select which per-architecture
//! barrier instruction the crate compiles in.
//!
//! The crate cannot select the instruction inline with a target-architecture
//! `cfg` predicate forbids that outside the architecture ports, and
//! `cargo xtask cfg-check` enforces it. Instead this script reads the target
//! Cargo is building for and emits one of the `dma_barrier_<arch>`
//! conditional-compilation names (plus the `dma_barrier_native` umbrella name)
//! when (and only when) the target is one of the three **native** Tier-1
//! targets: `x86_64-unknown-none`, `aarch64-unknown-none`, and
//! `riscv64gc-unknown-none-elf`. The crate gates its `dsb`/`dmb`/`fence`
//! assembly carve-out on those names, so the instruction choice lives in this
//! one audited place (mirroring `lib/abi-trap/build.rs`).
//!
//! Every other target — the host (for unit tests and `cargo xtask ci`) and
//! `wasm32-unknown-unknown` (a single-threaded sandbox with no separate DMA
//! master, so ordering is a no-op) — leaves all four names unset and compiles
//! the no-op host fallback instead.

fn main() {
    println!("cargo:rustc-check-cfg=cfg(dma_barrier_x86_64)");
    println!("cargo:rustc-check-cfg=cfg(dma_barrier_aarch64)");
    println!("cargo:rustc-check-cfg=cfg(dma_barrier_riscv64)");
    println!("cargo:rustc-check-cfg=cfg(dma_barrier_native)");

    if let Some(name) = native_barrier_cfg() {
        println!("cargo:rustc-cfg={name}");
        println!("cargo:rustc-cfg=dma_barrier_native");
    }
}

/// The `dma_barrier_<arch>` name to emit for the current build, or `None`
/// when the target is not one of the three native Tier-1 targets whose
/// silicon the barrier instructions are written for.
fn native_barrier_cfg() -> Option<&'static str> {
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if os != "none" {
        return None;
    }
    match std::env::var("CARGO_CFG_TARGET_ARCH")
        .unwrap_or_default()
        .as_str()
    {
        "x86_64" => Some("dma_barrier_x86_64"),
        "aarch64" => Some("dma_barrier_aarch64"),
        "riscv64" => Some("dma_barrier_riscv64"),
        _ => None,
    }
}
