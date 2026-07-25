//! Build script for the `tairix-pagezero` crate.
//!
//! Sole responsibility, and it is build glue (confining target-conditional
//! decisions to the architecture ports and the build glue; a build script is
//! build glue): decide which per-architecture hardware page-zero candidate the
//! crate compiles in.
//!
//! The crate cannot select the instruction inline with a target-architecture
//! `cfg` predicate — the charter forbids that outside the architecture ports,
//! and `cargo xtask cfg-check` enforces it. Instead this script reads the
//! target Cargo is building for and emits one of the `pagezero_<arch>`
//! conditional-compilation names when the target is an architecture whose ISA
//! carries a fast memory-fill primitive the crate has a candidate for (aarch64
//! `DC ZVA`, x86_64 ERMS `rep stosb`). The crate gates its candidate on that
//! name, so the instruction-set choice lives in this one audited place
//! (mirroring `lib/crc32c/build.rs`).
//!
//! The name is emitted for the architecture regardless of OS, so the host test
//! build on an `x86_64`/`aarch64` developer machine compiles the real hardware
//! candidate and exercises it against the portable reference (the candidate is
//! still only *selected* when the delivered `CpuFeatureSet` reports the
//! extension present). Every other target (riscv64, wasm32) has no hardware
//! candidate and falls to the portable baseline.

fn main() {
    println!("cargo:rustc-check-cfg=cfg(pagezero_x86_64)");
    println!("cargo:rustc-check-cfg=cfg(pagezero_aarch64)");

    match std::env::var("CARGO_CFG_TARGET_ARCH")
        .unwrap_or_default()
        .as_str()
    {
        "x86_64" => println!("cargo:rustc-cfg=pagezero_x86_64"),
        "aarch64" => println!("cargo:rustc-cfg=pagezero_aarch64"),
        _ => {}
    }
}
