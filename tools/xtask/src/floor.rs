//! Per-image build-time **CPU floor**: the single source of truth for the
//! `-C target-cpu` / `-C target-feature` baseline each shipped image is
//! compiled against.
//!
//! This is the lowest layer of the boot-time CPU-feature framework
//! (`plans/FIX-HARDWARE-FEATURES.md`, phase P0). The framework is split into
//! two layers, both mandatory:
//!
//! - **Build-time floor (this module):** the compiler may emit the
//!   *common-baseline* instructions of every SoC/PC an image must boot,
//!   inline and image-wide. It is chosen *per image*, never as a `cfg` in
//!   shared source, because one image boots many boards and the floor is
//!   forced down to their common set.
//! - **Runtime ceiling (P1–P3, not this module):** an ops table selects the
//!   extension-using or measured-fastest implementation *only on cores that
//!   have the extension*, recovering per booted CPU everything the
//!   conservative floor gives up.
//!
//! ## Why not `.cargo/config.toml`
//!
//! The shared `[target.<triple>]` `rustflags` blocks in `.cargo/config.toml`
//! are consumed by *every* cargo invocation for that triple — the shipped
//! image kernel, the QEMU-`virt` run kernel, the QEMU test builds, and every
//! PIE user-space cross-build alike. A floor set there would leak across
//! images and violate the "floor is per-image" decision. The floor is
//! therefore injected at the point each image's binaries are built, via the
//! `CARGO_ENCODED_RUSTFLAGS` environment variable.
//!
//! ## Flag precedence (the subtle part)
//!
//! Setting `CARGO_ENCODED_RUSTFLAGS` / `RUSTFLAGS` **replaces** — it does not
//! merge with — the `.cargo/config.toml` `[target.*]` `rustflags` block:
//! cargo takes flags from exactly one source, env outranking config. So the
//! injected floor string must **also carry** the flags the shared block
//! currently supplies (frame pointers on every bare-metal target; the x86_64
//! soft-crypto `--cfg`s). Those base flags live here in [`base_rustflags`] so
//! the config block and the injected set can never diverge; the
//! `base_rustflags_match_cargo_config` test pins the two together.
//!
//! ## The decided floors
//!
//! TAIRiX ships **one generic floor image per architecture, not per-board**
//! (`plans/FIX-HARDWARE-FEATURES.md`): a single `aarch64` media boots `RPi` 4 /
//! `CM4` / `OrangePi` / other `ARMv8` SBCs and a single `x86_64` ISO boots
//! arbitrary PCs, so each image's floor is forced to the *common* feature set
//! of every part it must boot. Every extension above that floor is recovered
//! per booted CPU by the P1–P3 runtime dispatch, so the low floor costs
//! nothing at runtime.

use tairix_itest_harness::pie::PieArch;

/// A shipped (or QEMU-run) image whose binaries share one CPU floor.
///
/// The set is closed: adding a variant is adding a shipped image, and every
/// variant must resolve to a documented [`CpuFloor`] in [`floor_for_image`]
/// (pinned total by `floor_for_image_is_total`).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum ImageKind {
    /// Universal ARM media (boots `RPi` 4 / `CM4` / `OrangePi` / other `ARMv8`
    /// SBCs).
    AArch64Generic,
    /// The BIOS/UEFI PC ISO (boots arbitrary PCs).
    X86_64Iso,
    /// The generic riscv64 image.
    Riscv64Generic,
    /// The QEMU-`virt` aarch64 development kernel booted by `cargo xtask run`
    /// (not shipped hardware).
    AArch64Virt,
}

impl ImageKind {
    /// Every image, in a stable order, so the totality test can iterate the
    /// whole set without hard-coding the members. Test-only: production code
    /// resolves a specific image, never the whole set.
    #[cfg(test)]
    pub(crate) const ALL: &'static [ImageKind] = &[
        ImageKind::AArch64Generic,
        ImageKind::X86_64Iso,
        ImageKind::Riscv64Generic,
        ImageKind::AArch64Virt,
    ];

    /// The freestanding target triple this image's binaries are built for.
    #[must_use]
    pub const fn triple(self) -> &'static str {
        match self {
            ImageKind::AArch64Generic | ImageKind::AArch64Virt => "aarch64-unknown-none",
            ImageKind::X86_64Iso => "x86_64-unknown-none",
            ImageKind::Riscv64Generic => "riscv64gc-unknown-none-elf",
        }
    }

    /// The generic *shipped* image for a PIE cross-compile architecture.
    ///
    /// Every user-space bundle the image pipeline cross-compiles is part of
    /// the generic per-arch userspace, so its floor is a pure function of the
    /// arch — the same floor the generic image's kernel uses. This is how the
    /// PIE recipe resolves its floor without threading a derivable value
    /// through every builder (the charter forbids carrying a value two ways).
    #[must_use]
    pub const fn generic_for_pie_arch(arch: PieArch) -> ImageKind {
        match arch {
            PieArch::Aarch64 => ImageKind::AArch64Generic,
            PieArch::X86_64 => ImageKind::X86_64Iso,
            PieArch::Riscv64 => ImageKind::Riscv64Generic,
        }
    }
}

/// The CPU floor an image is compiled against: an optional `target-cpu`, a
/// set of `target-feature` tokens, and the human rationale for the choice.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CpuFloor {
    /// The `-C target-cpu=<name>` model, or `None` for the triple's generic
    /// default (`target-cpu=generic`).
    pub target_cpu: Option<&'static str>,
    /// LLVM `target-feature` tokens (e.g. `"+aes"`), joined comma-separated
    /// into one `-C target-feature=` flag. Empty for a baseline floor.
    pub target_features: &'static [&'static str],
    /// Why this floor was chosen — which parts the image must boot forced it
    /// down (or up). Never empty (pinned by `floor_for_image_is_total`).
    pub rationale: &'static str,
    /// The triple this floor's binaries build for; drives [`Self::rustflags`].
    pub triple: &'static str,
}

impl CpuFloor {
    /// The floor-specific tokens only — the `-C target-cpu` / `-C
    /// target-feature` flags this floor *raises above* the triple's default.
    /// An empty vec for a genuinely generic (baseline) floor.
    ///
    /// These are what the PIE user-space recipe *prepends* to its own link
    /// recipe (its link flags stand in for the base set for user-space).
    #[must_use]
    pub fn floor_tokens(&self) -> Vec<String> {
        let mut tokens = Vec::new();
        if let Some(cpu) = self.target_cpu {
            tokens.push("-C".to_string());
            tokens.push(format!("target-cpu={cpu}"));
        }
        if !self.target_features.is_empty() {
            tokens.push("-C".to_string());
            tokens.push(format!("target-feature={}", self.target_features.join(",")));
        }
        tokens
    }

    /// The full kernel `rustflags` for this image: the triple's base flags
    /// (the ones the `.cargo/config.toml` block supplies, reproduced by
    /// [`base_rustflags`]) followed by this floor's tokens. This is the exact
    /// token list injected as `CARGO_ENCODED_RUSTFLAGS` on the kernel build,
    /// so config and the injected set can never diverge.
    #[must_use]
    pub fn rustflags(&self) -> Vec<String> {
        let mut flags: Vec<String> = base_rustflags(self.triple)
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        flags.extend(self.floor_tokens());
        flags
    }

    /// The [`Self::rustflags`] token list encoded for `CARGO_ENCODED_RUSTFLAGS`
    /// — the flags joined by the `0x1f` unit separator cargo expects.
    #[must_use]
    pub fn encoded_rustflags(&self) -> String {
        self.rustflags().join("\u{1f}")
    }
}

/// The per-triple base `rustflags` the `.cargo/config.toml` `[target.<triple>]`
/// block supplies — reproduced here so an injected `CARGO_ENCODED_RUSTFLAGS`
/// (which *replaces* the config block) still carries them. Kept in lockstep
/// with the config by `base_rustflags_match_cargo_config`.
#[must_use]
pub fn base_rustflags(triple: &str) -> &'static [&'static str] {
    match triple {
        "x86_64-unknown-none" => &[
            "-C",
            "code-model=kernel",
            "-C",
            "relocation-model=static",
            "-C",
            "force-frame-pointers=yes",
            "--cfg",
            "curve25519_dalek_backend=\"serial\"",
            "--cfg",
            "chacha20_force_soft",
            "--cfg",
            "poly1305_force_soft",
        ],
        "aarch64-unknown-none" | "riscv64gc-unknown-none-elf" => {
            &["-C", "force-frame-pointers=yes"]
        }
        // wasm32 and any other triple carry no base rustflags.
        _ => &[],
    }
}

/// The documented CPU floor for `image`. Total over [`ImageKind`].
///
/// The generic images deliberately stay at their architecture baseline: one
/// media boots many parts, so the floor is their common feature set, and
/// everything above it is recovered per booted CPU by the P1–P3 runtime
/// dispatch. A raised floor here must additionally pass the codegen-lowering
/// re-verification the plan mandates before it is adopted.
#[must_use]
pub fn floor_for_image(image: ImageKind) -> CpuFloor {
    let triple = image.triple();
    match image {
        ImageKind::AArch64Generic => CpuFloor {
            target_cpu: None,
            target_features: &[],
            rationale: "Universal ARM media boots RPi 4 / CM4 / OrangePi / other ARMv8 SBCs; \
                        floor is their common set (A53 ∩ A72 ∩ A76 ∩ Allwinner ∩ …) ≈ baseline \
                        ARMv8.0-A. Extensions (CRC32, AES/PMULL/SHA, wider NEON) are recovered \
                        per booted CPU by runtime dispatch.",
            triple,
        },
        ImageKind::X86_64Iso => CpuFloor {
            target_cpu: Some("x86-64"),
            target_features: &[],
            rationale: "The PC ISO boots arbitrary PCs, so the floor is x86-64 (v1) for maximum \
                        reach. AES-NI / AVX2 / SHA-NI, when the booted PC has them, are recovered \
                        by runtime dispatch. Raised only if a published minimum-hardware \
                        requirement is documented.",
            triple,
        },
        ImageKind::Riscv64Generic => CpuFloor {
            target_cpu: None,
            target_features: &[],
            rationale: "The base rv64gc the triple already implies; no extra extensions baked in. \
                        Z-extensions / vector, where a booted hart has them, are recovered by \
                        runtime dispatch.",
            triple,
        },
        ImageKind::AArch64Virt => CpuFloor {
            target_cpu: None,
            target_features: &[],
            rationale: "The QEMU-virt development kernel (`cargo xtask run`) is not shipped \
                        hardware; kept at ARMv8.0-A baseline so it changes no codegen relative to \
                        the shipped generic build, with runtime dispatch recovering extensions.",
            triple,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn floor_for_image_is_total() {
        for &image in ImageKind::ALL {
            let floor = floor_for_image(image);
            assert!(
                !floor.rationale.trim().is_empty(),
                "{image:?} must document a non-empty floor rationale",
            );
            assert_eq!(
                floor.triple,
                image.triple(),
                "{image:?} floor triple must match the image triple",
            );
        }
    }

    #[test]
    fn generic_floors_emit_no_floor_tokens() {
        // Every decided floor is baseline (no raised target-cpu *feature*
        // set), so the floor-only token list — what user-space prepends — is
        // empty and the builds stay byte-for-byte as today.
        for &image in ImageKind::ALL {
            let floor = floor_for_image(image);
            if floor.target_cpu.is_none() && floor.target_features.is_empty() {
                assert!(
                    floor.floor_tokens().is_empty(),
                    "{image:?} baseline floor must emit no floor tokens",
                );
            }
        }
    }

    #[test]
    fn floor_tokens_shape_when_raised() {
        let floor = CpuFloor {
            target_cpu: Some("cortex-a72"),
            target_features: &["+aes", "+sha2"],
            rationale: "test",
            triple: "aarch64-unknown-none",
        };
        assert_eq!(
            floor.floor_tokens(),
            vec![
                "-C".to_string(),
                "target-cpu=cortex-a72".to_string(),
                "-C".to_string(),
                "target-feature=+aes,+sha2".to_string(),
            ],
        );
    }

    #[test]
    fn rustflags_is_base_then_floor() {
        // A raised aarch64 floor: base (frame pointers) then the floor tokens,
        // in that order, so the injected env reproduces the config block and
        // then adds the floor.
        let floor = CpuFloor {
            target_cpu: None,
            target_features: &["+crc"],
            rationale: "test",
            triple: "aarch64-unknown-none",
        };
        assert_eq!(
            floor.rustflags(),
            vec![
                "-C".to_string(),
                "force-frame-pointers=yes".to_string(),
                "-C".to_string(),
                "target-feature=+crc".to_string(),
            ],
        );
    }

    #[test]
    fn encoded_rustflags_uses_the_unit_separator() {
        let floor = floor_for_image(ImageKind::AArch64Generic);
        assert_eq!(
            floor.encoded_rustflags(),
            "-C\u{1f}force-frame-pointers=yes"
        );
    }

    #[test]
    fn generic_image_kernel_and_pie_share_one_floor() {
        // The shipped generic image's kernel and its user-space bundles must
        // build against the identical floor (they cannot skew). The PIE
        // recipe resolves its floor from the arch via `generic_for_pie_arch`,
        // so this pins that resolution to the kernel's `floor_for_image`.
        for &arch in PieArch::ALL {
            let via_arch = floor_for_image(ImageKind::generic_for_pie_arch(arch));
            let kernel = match arch {
                PieArch::Aarch64 => floor_for_image(ImageKind::AArch64Generic),
                PieArch::X86_64 => floor_for_image(ImageKind::X86_64Iso),
                PieArch::Riscv64 => floor_for_image(ImageKind::Riscv64Generic),
            };
            assert_eq!(
                via_arch.floor_tokens(),
                kernel.floor_tokens(),
                "{arch:?}: PIE floor tokens must equal the generic kernel floor tokens",
            );
            assert_eq!(via_arch.triple, arch.target_triple());
        }
    }

    /// Extract the flat token list of a `[target.<triple>]` `rustflags = [ … ]`
    /// array from `.cargo/config.toml` text, honouring `#` comments and `\`
    /// string escapes, so the base flags reproduced in [`base_rustflags`]
    /// cannot silently drift from the config the non-image builds consume.
    fn config_rustflags(config: &str, triple: &str) -> Vec<String> {
        let header = format!("[target.{triple}]");
        let section_start = config
            .find(&header)
            .unwrap_or_else(|| panic!("config has no {header} section"))
            + header.len();
        let after = &config[section_start..];
        let arr_start = after
            .find("rustflags")
            .and_then(|i| after[i..].find('[').map(|j| i + j + 1))
            .expect("target section has no rustflags array");
        let body = &after[arr_start..];

        let mut tokens = Vec::new();
        let mut chars = body.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                ']' => break,
                '#' => {
                    // Skip a comment to end of line.
                    for cc in chars.by_ref() {
                        if cc == '\n' {
                            break;
                        }
                    }
                }
                '"' => {
                    let mut s = String::new();
                    while let Some(cc) = chars.next() {
                        match cc {
                            '\\' => {
                                if let Some(esc) = chars.next() {
                                    s.push(esc);
                                }
                            }
                            '"' => break,
                            _ => s.push(cc),
                        }
                    }
                    tokens.push(s);
                }
                _ => {}
            }
        }
        tokens
    }

    #[test]
    fn base_rustflags_match_cargo_config() {
        let config_path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../.cargo/config.toml");
        let config = std::fs::read_to_string(config_path)
            .unwrap_or_else(|e| panic!("cannot read {config_path}: {e}"));

        for triple in [
            "x86_64-unknown-none",
            "aarch64-unknown-none",
            "riscv64gc-unknown-none-elf",
            "wasm32-unknown-unknown",
        ] {
            let from_config = config_rustflags(&config, triple);
            let from_code: Vec<String> = base_rustflags(triple)
                .iter()
                .map(|s| (*s).to_string())
                .collect();
            assert_eq!(
                from_config, from_code,
                "base_rustflags({triple}) must equal the .cargo/config.toml block",
            );
        }
    }
}
