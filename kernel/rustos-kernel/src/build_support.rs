//! Pure target-selection logic for the `rustos-kernel` build script.
//!
//! This file is pulled in by `build.rs` as a `#[path]` module (so the
//! build script has no dependency to pull in) and is *also* compiled
//! into the crate's host test build as a module, so the same selection
//! rules are unit tested rather than only exercised implicitly by a
//! cross-compile (`AGENTS.md` §7 — tests are part of the change).
//!
//! Every decision the build script makes about *which* instruction set
//! the production kernel is being built for, and *which* per-board boot
//! linker script to hand `rustc`, lives here as a pure function of the
//! Cargo-provided target strings. Keeping it target-string-driven — not
//! a target-conditional `cfg` predicate — is what lets the build script
//! (the `AGENTS.md` §17.2 build-glue carve-out) make the instruction-set
//! choice in one audited place instead of leaking that predicate into
//! the crate body, which `cargo xtask cfg-check` forbids outside the
//! architecture ports and the build glue.

/// The freestanding bare-metal target triples the production kernel
/// builds for, paired with the per-board boot linker script (a path
/// relative to `CARGO_MANIFEST_DIR`) the §1 boot-stub carve-out pins for
/// that board.
///
/// The aarch64 production image targets the Raspberry Pi 4 (load address
/// `0x8_0000`, `aarch64-rpi4.ld`); the QEMU `virt` board's
/// `aarch64-virt.ld` is used only by the per-test bins under
/// `tests/integration/*`, which supply their own build scripts.
const FREESTANDING_TARGETS: &[(&str, &str)] = &[
    ("x86_64-unknown-none", "../arch/x86_64/linker.ld"),
    (
        "aarch64-unknown-none",
        "../arch/aarch64/link/aarch64-rpi4.ld",
    ),
    // The riscv64 production image targets the QEMU `virt` / SiFive
    // board (its only Tier-1 board), so it reuses the arch port's
    // `riscv64-virt.ld` (load `0x8020_0000` above the OpenSBI firmware
    // region) rather than a separate per-board script (`AGENTS.md`
    // §2.2; there is no Pi-equivalent second riscv64 board yet).
    (
        "riscv64gc-unknown-none-elf",
        "../arch/riscv64/link/riscv64-virt.ld",
    ),
];

/// The boot linker script (relative to `CARGO_MANIFEST_DIR`) for a
/// freestanding target triple, or `None` for any other triple (the host
/// build, which links no kernel image).
#[must_use]
pub fn linker_script_for(target: &str) -> Option<&'static str> {
    FREESTANDING_TARGETS
        .iter()
        .find(|(triple, _)| *triple == target)
        .map(|(_, script)| *script)
}

/// The `kernel_isa` conditional-compilation value for a target
/// instruction set, or `None` if the crate ships no production kernel
/// for it.
///
/// Emitted by the build script for *every* build (host included) so the
/// crate body can gate each architecture's modules on the chosen
/// instruction set without naming `target_arch` inline.
#[must_use]
pub fn kernel_isa(target_arch: &str) -> Option<&'static str> {
    match target_arch {
        "x86_64" => Some("x86_64"),
        "aarch64" => Some("aarch64"),
        "riscv64" => Some("riscv64"),
        _ => None,
    }
}

/// True when the crate is being built as the bare-metal production
/// kernel: a supported instruction set with no host operating system.
#[must_use]
pub fn is_freestanding(target_os: &str, target_arch: &str) -> bool {
    target_os == "none" && kernel_isa(target_arch).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn x86_64_freestanding_selects_the_x86_64_linker_script() {
        assert_eq!(
            linker_script_for("x86_64-unknown-none"),
            Some("../arch/x86_64/linker.ld")
        );
        assert!(is_freestanding("none", "x86_64"));
        assert_eq!(kernel_isa("x86_64"), Some("x86_64"));
    }

    #[test]
    fn aarch64_freestanding_selects_the_rpi4_linker_script() {
        assert_eq!(
            linker_script_for("aarch64-unknown-none"),
            Some("../arch/aarch64/link/aarch64-rpi4.ld")
        );
        assert!(is_freestanding("none", "aarch64"));
        assert_eq!(kernel_isa("aarch64"), Some("aarch64"));
    }

    #[test]
    fn host_targets_link_no_kernel_script_but_still_pick_an_isa() {
        // A host build (an OS present) is never freestanding, links no
        // kernel image, yet still reports its instruction set so the
        // crate's host test build compiles the right architecture's
        // modules.
        assert_eq!(linker_script_for("x86_64-unknown-linux-gnu"), None);
        assert!(!is_freestanding("linux", "x86_64"));
        assert_eq!(kernel_isa("x86_64"), Some("x86_64"));
    }

    #[test]
    fn riscv64_freestanding_selects_the_virt_linker_script() {
        assert_eq!(
            linker_script_for("riscv64gc-unknown-none-elf"),
            Some("../arch/riscv64/link/riscv64-virt.ld")
        );
        assert!(is_freestanding("none", "riscv64"));
        assert_eq!(kernel_isa("riscv64"), Some("riscv64"));
    }

    #[test]
    fn unsupported_instruction_sets_have_no_kernel() {
        assert_eq!(kernel_isa("wasm32"), None);
        assert!(!is_freestanding("none", "wasm32"));
        assert_eq!(linker_script_for("wasm32-unknown-unknown"), None);
    }
}
