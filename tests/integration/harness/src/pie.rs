//! The freestanding cross-compile target the image pipeline builds a
//! position-independent `Run` binary for.
//!
//! The `tools/xtask` image pipeline (`pie_build`, `image_apps`,
//! `image_drivers`) cross-compiles the PIE program images the kernel
//! spawn/autoload path loads. Every such build needs the same two
//! architecture-derived facts: the Rust target triple
//! to build for, and the `CARGO_TARGET_<triple>_RUSTFLAGS` environment
//! variable that scopes the PIE link recipe to that target (and to it alone,
//! so a crate's own host build script is never affected).
//!
//! Spelling those two facts by hand in each of the pipeline's builders is the
//! duplication the charter forbids: a mistyped variable name silently drops
//! the link flags and the converter reads a stale or wrongly-linked ELF. This
//! type is the one definition every builder draws from, so the arch selection
//! cannot drift between them.

/// The one PIE link script every pure-Rust `Run` program links against,
/// as a path relative to the workspace root.
///
/// Every `Run` program — PID 1, the system services, and every `/Apps` /
/// `/System/Drivers` bundle — shares this single layout (it names only
/// architecture-neutral section classes), so it lives in exactly one file
/// rather than being copied beside each program crate. Both PIE build
/// recipes — the kernel `build.rs` embedded-program build and the
/// `tools/xtask` image pipeline — resolve the script through this one
/// constant, so the layout cannot drift between them and, because the script
/// path they hand `rustc` is now identical for every program, the
/// `-Z build-std` artefacts are built once and shared across all of them
/// instead of being rebuilt per program.
pub const RUN_LD_WORKSPACE_RELPATH: &str = "lib/rt/Run.ld";

/// A freestanding Tier-1 target the image pipeline can cross-compile a
/// position-independent `Run` binary for.
///
/// `wasm32` is deliberately absent: it is a `cdylib` host module, not a
/// bare-metal PIE image the kernel spawn path loads, so it is not a member of
/// this cross-compile vocabulary.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum PieArch {
    /// `aarch64-unknown-none` — the Raspberry Pi image and the aarch64 QEMU
    /// verticals boot this.
    Aarch64,
    /// `riscv64gc-unknown-none-elf` — the QEMU `virt` / SiFive verticals.
    Riscv64,
    /// `x86_64-unknown-none` — the BIOS/UEFI PC image and the x86_64 QEMU
    /// verticals.
    X86_64,
}

impl PieArch {
    /// Every architecture in this vocabulary, in a stable order, so a caller
    /// (or a test) can iterate the whole set without hard-coding the members.
    pub const ALL: &'static [PieArch] = &[PieArch::Aarch64, PieArch::Riscv64, PieArch::X86_64];

    /// The Rust target triple this architecture cross-compiles for.
    #[must_use]
    pub const fn target_triple(self) -> &'static str {
        match self {
            PieArch::Aarch64 => "aarch64-unknown-none",
            PieArch::Riscv64 => "riscv64gc-unknown-none-elf",
            PieArch::X86_64 => "x86_64-unknown-none",
        }
    }

    /// This architecture's stable index into a per-arch table (its position
    /// in [`Self::ALL`]), so a builder can memoise one composed artefact per
    /// target in a fixed-size `[_; PieArch::COUNT]` array without a runtime
    /// map. Kept in lockstep with [`Self::ALL`] by
    /// `index_matches_all_position`.
    #[must_use]
    pub const fn index(self) -> usize {
        match self {
            PieArch::Aarch64 => 0,
            PieArch::Riscv64 => 1,
            PieArch::X86_64 => 2,
        }
    }

    /// The number of architectures in this vocabulary — the length a per-arch
    /// memo table indexed by [`Self::index`] must have.
    pub const COUNT: usize = 3;

    /// The architecture whose freestanding target triple is `triple`, or
    /// `None` for a triple this vocabulary does not name. The inverse of
    /// [`Self::target_triple`], so the image pipeline can recover the arch a
    /// QEMU enrolment's `target` string selects without a second table.
    #[must_use]
    pub fn from_target_triple(triple: &str) -> Option<PieArch> {
        PieArch::ALL
            .iter()
            .copied()
            .find(|a| a.target_triple() == triple)
    }

    /// The `CARGO_TARGET_<triple>_RUSTFLAGS` variable that scopes the PIE
    /// link recipe to [`Self::target_triple`].
    ///
    /// Cargo derives this variable from the triple by upper-casing it and
    /// replacing every character that is not a letter or digit with `_`; the
    /// `target_scoped_rustflags_var_matches_cargos_mangling` test pins each
    /// value against that rule so a typo cannot ship.
    #[must_use]
    pub const fn rustflags_env_var(self) -> &'static str {
        match self {
            PieArch::Aarch64 => "CARGO_TARGET_AARCH64_UNKNOWN_NONE_RUSTFLAGS",
            PieArch::Riscv64 => "CARGO_TARGET_RISCV64GC_UNKNOWN_NONE_ELF_RUSTFLAGS",
            PieArch::X86_64 => "CARGO_TARGET_X86_64_UNKNOWN_NONE_RUSTFLAGS",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Cargo's own rule for turning a target triple into the infix of its
    /// per-target environment variables: upper-case, and every byte that is
    /// not ASCII-alphanumeric becomes `_`.
    fn cargo_env_infix(triple: &str) -> String {
        triple
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() {
                    c.to_ascii_uppercase()
                } else {
                    '_'
                }
            })
            .collect()
    }

    #[test]
    fn target_triples_are_the_tier_one_freestanding_triples() {
        assert_eq!(PieArch::Aarch64.target_triple(), "aarch64-unknown-none");
        assert_eq!(
            PieArch::Riscv64.target_triple(),
            "riscv64gc-unknown-none-elf"
        );
        assert_eq!(PieArch::X86_64.target_triple(), "x86_64-unknown-none");
    }

    #[test]
    fn target_scoped_rustflags_var_matches_cargos_mangling() {
        for &arch in PieArch::ALL {
            let expected = format!(
                "CARGO_TARGET_{}_RUSTFLAGS",
                cargo_env_infix(arch.target_triple())
            );
            assert_eq!(
                arch.rustflags_env_var(),
                expected,
                "{arch:?} rustflags var must match cargo's triple mangling",
            );
        }
    }

    #[test]
    fn all_lists_each_arch_once() {
        let mut seen = PieArch::ALL.to_vec();
        seen.sort_by_key(|a| format!("{a:?}"));
        seen.dedup();
        assert_eq!(
            seen.len(),
            PieArch::ALL.len(),
            "ALL must not repeat an arch"
        );
    }

    #[test]
    fn index_matches_all_position() {
        assert_eq!(PieArch::ALL.len(), PieArch::COUNT);
        for (position, &arch) in PieArch::ALL.iter().enumerate() {
            assert_eq!(
                arch.index(),
                position,
                "{arch:?} index must be its position in ALL"
            );
            assert!(arch.index() < PieArch::COUNT);
        }
    }

    #[test]
    fn from_target_triple_inverts_target_triple() {
        for &arch in PieArch::ALL {
            assert_eq!(
                PieArch::from_target_triple(arch.target_triple()),
                Some(arch)
            );
        }
        assert_eq!(PieArch::from_target_triple("wasm32-unknown-unknown"), None);
        assert_eq!(PieArch::from_target_triple(""), None);
    }
}
