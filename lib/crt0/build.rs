//! Build script for the `tairix-crt0` crate.
//!
//! Sole responsibility, and it is build glue (confines
//! target-conditional decisions to the architecture ports and the build
//! glue; a build script is build glue): select which per-architecture
//! program-entry trampoline (`_start`) the crate compiles in.
//!
//! The crate cannot select the entry trampoline inline with a
//! target-architecture `cfg` predicate forbids that outside the
//! architecture ports, and `cargo xtask cfg-check` enforces it. Instead this
//! script reads the target Cargo is building for and emits one of the
//! `crt0_native_<arch>` conditional-compilation names when (and only when)
//! the target is one of the three **native** Tier-1 targets:
//! `x86_64-unknown-none`, `aarch64-unknown-none`, and
//! `riscv64gc-unknown-none-elf`. The crate's `start` module gates its
//! `_start` assembly carve-out on those names, so the instruction-set choice
//! lives in this one audited place (mirroring `lib/abi-sys/build.rs`).
//!
//! Every other target — the host (for unit tests and `cargo xtask ci`), and
//! `wasm32-unknown-unknown`, which has no trap instruction and is out of
//! scope for this runtime (`plans/CCOMPAT.md` §1) — leaves all three names
//! unset and compiles only the host-testable `build_c_runtime` marshalling
//! core.

fn main() {
    println!("cargo:rustc-check-cfg=cfg(crt0_native_x86_64)");
    println!("cargo:rustc-check-cfg=cfg(crt0_native_aarch64)");
    println!("cargo:rustc-check-cfg=cfg(crt0_native_riscv64)");

    if let Some(name) = native_start_cfg() {
        println!("cargo:rustc-cfg={name}");
    }
}

/// The `crt0_native_<arch>` name to emit for the current build, or `None`
/// when the target is not one of the three native Tier-1 targets the
/// startup trampoline supports.
fn native_start_cfg() -> Option<&'static str> {
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if os != "none" {
        return None;
    }
    match std::env::var("CARGO_CFG_TARGET_ARCH")
        .unwrap_or_default()
        .as_str()
    {
        "x86_64" => Some("crt0_native_x86_64"),
        "aarch64" => Some("crt0_native_aarch64"),
        "riscv64" => Some("crt0_native_riscv64"),
        _ => None,
    }
}
