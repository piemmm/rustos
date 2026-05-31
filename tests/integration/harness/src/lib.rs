//! Build-time target classification shared by the freestanding QEMU
//! integration binaries (`AGENTS.md` §2.2, §17.2).
//!
//! The integration binaries under `tests/integration/` compile two ways:
//! as freestanding `no_std`/`no_main` kernels for a bare-metal QEMU
//! target, and as inert host stubs for `cargo build --workspace`. They
//! must choose between those forms without naming the target instruction
//! set in their own source, which §17.2 confines to the architecture
//! ports and the build glue.
//!
//! This crate is that build glue. Each binary's build script calls
//! [`emit_target_cfg`], which inspects the cargo-provided target
//! description and enables the matching conditional-compilation names:
//!
//! * `freestanding` — the crate is being built for a bare-metal
//!   (`os = "none"`) target and should compile its kernel body.
//! * `itest_x86_64` — freestanding on the 64-bit x86 port.
//! * `itest_riscv64` — freestanding on the 64-bit RISC-V port.
//! * `itest_aarch64` — freestanding on the 64-bit Arm port.
//! * `itest_wasm32` — the browser-sandbox wasm32 port
//!   (`wasm32-unknown-unknown`, `os = "unknown"`). Unlike the bare-metal
//!   ports this is a `cdylib`, not a `no_main` kernel, so it gets its
//!   own cfg *without* `freestanding`.
//!
//! Binaries gate on those names instead of a raw target predicate, so the
//! instruction-set choice lives in this one audited place.

/// Cargo environment key naming the target operating system.
const TARGET_OS_KEY: &str = "CARGO_CFG_TARGET_OS";
/// Cargo environment key naming the target instruction set.
const TARGET_ARCH_KEY: &str = "CARGO_CFG_TARGET_ARCH";

/// Every conditional-compilation name this crate may enable. Declared to
/// the compiler unconditionally so `--cfg`-aware lints accept the gates
/// even on host builds where none of them are active.
pub const KNOWN_CFGS: &[&str] = &[
    "freestanding",
    "itest_x86_64",
    "itest_riscv64",
    "itest_aarch64",
    "itest_wasm32",
];

/// Classify a target into the conditional-compilation names its
/// freestanding integration binary should enable.
///
/// Bare-metal targets (`os == "none"`) are freestanding; the matching
/// per-port name is added when the instruction set is one the QEMU
/// verticals cover. Hosted targets enable nothing, leaving the binary as
/// an inert stub.
#[must_use]
pub fn active_cfgs(os: &str, arch: &str) -> Vec<&'static str> {
    // The wasm32 browser target (`wasm32-unknown-unknown`, `os =
    // "unknown"`) is a `cdylib` the host loads, not a bare-metal
    // `no_main` kernel, so it enables its own cfg without
    // `freestanding`.
    if os == "unknown" && arch == "wasm32" {
        return vec!["itest_wasm32"];
    }
    if os != "none" {
        return Vec::new();
    }
    let mut cfgs = vec!["freestanding"];
    match arch {
        "x86_64" => cfgs.push("itest_x86_64"),
        "riscv64" => cfgs.push("itest_riscv64"),
        "aarch64" => cfgs.push("itest_aarch64"),
        _ => {}
    }
    cfgs
}

/// Emit the conditional-compilation flags for the current build.
///
/// Call this from a binary's build script. It declares every
/// [`KNOWN_CFGS`] name to the compiler and enables those returned by
/// [`active_cfgs`] for the target cargo is building.
pub fn emit_target_cfg() {
    for name in KNOWN_CFGS {
        println!("cargo:rustc-check-cfg=cfg({name})");
    }
    let os = std::env::var(TARGET_OS_KEY).unwrap_or_default();
    let arch = std::env::var(TARGET_ARCH_KEY).unwrap_or_default();
    for name in active_cfgs(&os, &arch) {
        println!("cargo:rustc-cfg={name}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosted_targets_are_inert() {
        assert!(active_cfgs("linux", "x86_64").is_empty());
        assert!(active_cfgs("macos", "aarch64").is_empty());
    }

    #[test]
    fn bare_metal_x86_64_is_freestanding() {
        assert_eq!(
            active_cfgs("none", "x86_64"),
            ["freestanding", "itest_x86_64"]
        );
    }

    #[test]
    fn bare_metal_riscv64_is_freestanding() {
        assert_eq!(
            active_cfgs("none", "riscv64"),
            ["freestanding", "itest_riscv64"]
        );
    }

    #[test]
    fn bare_metal_aarch64_is_freestanding() {
        assert_eq!(
            active_cfgs("none", "aarch64"),
            ["freestanding", "itest_aarch64"]
        );
    }

    #[test]
    fn unknown_bare_metal_arch_is_freestanding_only() {
        assert_eq!(active_cfgs("none", "wasm32"), ["freestanding"]);
    }

    #[test]
    fn wasm32_browser_target_is_a_cdylib_not_freestanding() {
        assert_eq!(active_cfgs("unknown", "wasm32"), ["itest_wasm32"]);
    }

    #[test]
    fn every_active_cfg_is_declared() {
        for (os, arch) in [("none", "x86_64"), ("none", "riscv64"), ("none", "wasm32")] {
            for name in active_cfgs(os, arch) {
                assert!(KNOWN_CFGS.contains(&name), "{name} not declared");
            }
        }
    }
}
