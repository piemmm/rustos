//! Build script for the `rustos-abi-trap` crate.
//!
//! Sole responsibility, and it is build glue (confines
//! target-conditional decisions to the architecture ports and the build
//! glue; a build script is build glue): select which per-architecture
//! syscall-trap instruction the crate compiles in.
//!
//! The crate cannot select the instruction set inline with a
//! target-architecture `cfg` predicate forbids that outside the
//! architecture ports, and `cargo xtask cfg-check` enforces it. Instead this
//! script reads the target Cargo is building for and emits one of the
//! `abi_trap_<arch>` conditional-compilation names (plus the `abi_trap_native`
//! umbrella name) when (and only when) the target is one of the three
//! **native** Tier-1 targets: `x86_64-unknown-none`, `aarch64-unknown-none`,
//! and `riscv64gc-unknown-none-elf`. The crate gates its `syscall`/`svc`/
//! `ecall` assembly carve-out on those names, so the instruction-set choice
//! lives in this one audited place (mirroring `lib/crt0/build.rs`).
//!
//! Every other target — the host (for unit tests and `cargo xtask ci`),
//! and `wasm32-unknown-unknown`, which has no trap instruction and is out
//! of scope for this primitive (`plans/CCOMPAT.md` §1) — leaves all four
//! names unset and compiles the host trap seam instead.

fn main() {
    println!("cargo:rustc-check-cfg=cfg(abi_trap_x86_64)");
    println!("cargo:rustc-check-cfg=cfg(abi_trap_aarch64)");
    println!("cargo:rustc-check-cfg=cfg(abi_trap_riscv64)");
    println!("cargo:rustc-check-cfg=cfg(abi_trap_native)");

    if let Some(name) = native_trap_cfg() {
        println!("cargo:rustc-cfg={name}");
        println!("cargo:rustc-cfg=abi_trap_native");
    }
}

/// The `abi_trap_<arch>` name to emit for the current build, or `None`
/// when the target is not one of the three native Tier-1 targets the
/// primitive traps on.
fn native_trap_cfg() -> Option<&'static str> {
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if os != "none" {
        return None;
    }
    match std::env::var("CARGO_CFG_TARGET_ARCH")
        .unwrap_or_default()
        .as_str()
    {
        "x86_64" => Some("abi_trap_x86_64"),
        "aarch64" => Some("abi_trap_aarch64"),
        "riscv64" => Some("abi_trap_riscv64"),
        _ => None,
    }
}
