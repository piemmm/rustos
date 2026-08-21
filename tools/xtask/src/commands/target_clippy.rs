//! The bare-metal `clippy` passes: lint the code the host pass cannot see.
//!
//! `cargo clippy --workspace --all-targets` builds every crate for the *host*.
//! Almost nothing that actually ships is compiled by that pass: a kernel
//! subsystem, an architecture backend, a driver, a system service and an
//! application body are all reached only when the crate is built for a
//! freestanding triple — most of them literally behind the `freestanding` cfg
//! each crate's `build.rs` sets when `CARGO_CFG_TARGET_OS == "none"`, whose
//! host arm is an inert stub. The image and QEMU stages then *compile* those
//! bodies but never lint them, so a lint in shipped code could never fail CI.
//!
//! This module closes that hole by running the same `-D warnings` clippy once
//! per target over the crates that target is built from. Together with the
//! host pass, every Tier-1 target's configuration is linted.
//!
//! ## The product passes
//!
//! Per freestanding Tier-1 target ([`PieArch`]), one pass per **stratum** of
//! the product tree. The selection is derived, never listed: it is every
//! workspace member the image pipeline cross-compiles — less
//!
//! - **`tools/*`**, host-only build orchestration (this crate included) that
//!   never runs on the machine under test, and
//! - **`tests/*`**, test support rather than product — covered by the vertical
//!   passes below, and
//! - a **foreign architecture backend**: `kernel/arch/<other>` cannot compile
//!   for this target, so only the backend named by [`backend_dir`] and the
//!   architecture-neutral `kernel/arch/api` are in the pass.
//!
//! The stratum split is not cosmetic. Cargo unifies features across every
//! package named in one invocation, so selecting the kernel binary alongside
//! the userland programs turns on the `program` features of their shared
//! dependencies and links `lib/rt`'s `#[global_allocator]` and
//! `#[panic_handler]` into the kernel — a duplicate `panic_impl` lang item.
//! The image pipeline builds the kernel and the programs in separate cargo
//! invocations for exactly that reason; the lint passes mirror it.
//!
//! ## The `wasm32` pass
//!
//! The browser target builds only one piece of product code — its own Arch HAL
//! backend, which no bare-metal triple can compile — so `wasm32` gets a pass
//! over that backend and the architecture-neutral `kernel/arch/api`, plus the
//! browser verticals from `wasm_tests`' own table. Without it the `wasm32`
//! backend would be the one Tier-1 configuration nothing ever lints.
//!
//! ## What is *not* here: the QEMU guests
//!
//! The enrolled QEMU guests under `tests/integration/` are test support, not
//! product, and they are **not** linted by this stage. That is a known gap,
//! staged in `plans/CODEVERIFY.md` with the commands and counts: the
//! guests are near-identical triplicates per architecture, so cleaning them
//! means first hoisting their shared logic into `tests/integration/harness`.
//!
//! `--all-targets` is deliberately absent throughout: a bare-metal target has
//! no test harness, so the unit-test targets cannot link there. The host pass
//! lints those.

use std::ffi::OsString;

use tairix_itest_harness::pie::PieArch;

use super::deps_check::{self, Crate};
use super::wasm_tests;
use crate::{Context, LONG_BUILD_COMMAND_TIMEOUT};

/// Lint the shipped product tree for every Tier-1 target.
///
/// `args` are forwarded to clippy after `-D warnings`, exactly as the host
/// pass forwards them.
pub fn run(ctx: &Context, args: &[OsString]) -> Result<(), String> {
    let crates = deps_check::build_graph(&ctx.workspace_root)?;
    for &arch in PieArch::ALL {
        for stratum in Stratum::ALL {
            let packages = selection(&crates, arch, stratum);
            lint(ctx, arch.target_triple(), stratum.label(), &packages, args)?;
        }
    }
    let (wasm_target, wasm_verticals) = wasm_tests::packages();
    lint(ctx, wasm_target, "arch", &wasm_arch(&crates), args)?;
    lint(ctx, wasm_target, "verticals", &wasm_verticals, args)
}

/// The product crates the browser target builds: its own backend and the
/// architecture-neutral HAL surface.
fn wasm_arch(crates: &[Crate]) -> Vec<&str> {
    crates
        .iter()
        .filter(|c| c.rel_dir == "kernel/arch/api" || c.rel_dir == WASM_BACKEND_DIR)
        .map(|c| c.name.as_str())
        .collect()
}

/// Run one `-D warnings` clippy pass over `packages` built for `target`.
fn lint(
    ctx: &Context,
    target: &str,
    what: &str,
    packages: &[&str],
    args: &[OsString],
) -> Result<(), String> {
    let mut cmd = ctx.cargo();
    cmd.args(["clippy", "--locked", "--target", target]);
    // Report every finding in one pass rather than stopping at the first crate
    // that trips: a lint gate a developer has to re-run per error wastes a
    // cross-compile each time.
    cmd.arg("--keep-going");
    for package in packages {
        cmd.args(["-p", package]);
    }
    cmd.args(["--", "-D", "warnings"]);
    cmd.args(args);
    let label = format!("clippy ({target} {what}: {} pkg)", packages.len());
    // A cross-compile of the whole product tree rebuilds `core`/`alloc` through
    // `-Z build-std` on a cold `target/`, which outruns the default budget.
    ctx.run_with_timeout(&label, cmd, LONG_BUILD_COMMAND_TIMEOUT)
}

/// The backend the browser target implements, which no [`PieArch`] builds.
const WASM_BACKEND_DIR: &str = "kernel/arch/wasm32";

/// The `kernel/arch/<dir>` backend that implements `arch`.
///
/// The `match` is the guard: a new [`PieArch`] variant cannot be added without
/// naming the backend that implements it.
const fn backend_dir(arch: PieArch) -> &'static str {
    match arch {
        PieArch::Aarch64 => "kernel/arch/aarch64",
        PieArch::Riscv64 => "kernel/arch/riscv64",
        PieArch::X86_64 => "kernel/arch/x86_64",
    }
}

/// A layer of the product tree that is linted in its own cargo invocation.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Stratum {
    /// `kernel/*`: the subsystems, this target's backend, and the kernel binary.
    Kernel,
    /// `lib/*`: the shared `no_std` crates, linted without any program feature.
    Lib,
    /// `drivers/*` and `userland/*`: the programs, as the image build groups them.
    Programs,
}

impl Stratum {
    const ALL: [Self; 3] = [Self::Kernel, Self::Lib, Self::Programs];

    /// How the stratum names itself in the step label.
    const fn label(self) -> &'static str {
        match self {
            Self::Kernel => "kernel",
            Self::Lib => "lib",
            Self::Programs => "programs",
        }
    }

    /// The stratum a workspace member belongs to, or `None` when it is host
    /// build orchestration or test support rather than product.
    fn of(rel_dir: &str) -> Option<Self> {
        match rel_dir.split('/').next().unwrap_or(rel_dir) {
            "kernel" => Some(Self::Kernel),
            "lib" => Some(Self::Lib),
            "drivers" | "userland" => Some(Self::Programs),
            _ => None,
        }
    }
}

/// The packages `arch`'s `stratum` pass lints, in the graph's order.
fn selection(crates: &[Crate], arch: PieArch, stratum: Stratum) -> Vec<&str> {
    crates
        .iter()
        .filter(|c| Stratum::of(&c.rel_dir) == Some(stratum) && backend_applies(&c.rel_dir, arch))
        .map(|c| c.name.as_str())
        .collect()
}

/// True unless `rel_dir` is an architecture backend other than `arch`'s.
fn backend_applies(rel_dir: &str, arch: PieArch) -> bool {
    !rel_dir.starts_with("kernel/arch/")
        || rel_dir == "kernel/arch/api"
        || rel_dir == backend_dir(arch)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The real workspace graph, so these assertions are about the tree as it
    /// is rather than a fixture that can drift from it.
    fn graph() -> Vec<Crate> {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(std::path::Path::parent)
            .expect("xtask sits two levels below the workspace root")
            .to_path_buf();
        deps_check::build_graph(&root).expect("the workspace graph parses")
    }

    /// Every directory `arch` lints, across all of its strata.
    fn dirs_of(crates: &[Crate], arch: PieArch) -> Vec<&str> {
        let picked: Vec<&str> = Stratum::ALL
            .iter()
            .flat_map(|&s| selection(crates, arch, s))
            .collect();
        crates
            .iter()
            .filter(|c| picked.contains(&c.name.as_str()))
            .map(|c| c.rel_dir.as_str())
            .collect()
    }

    /// The kernel binary and the userland programs must never share a cargo
    /// invocation: cargo would unify their features and link `lib/rt`'s
    /// allocator and panic handler into the kernel, a duplicate lang item.
    #[test]
    fn the_kernel_and_the_programs_are_linted_separately() {
        let crates = graph();
        for &arch in PieArch::ALL {
            let kernel = selection(&crates, arch, Stratum::Kernel);
            let programs = selection(&crates, arch, Stratum::Programs);
            assert!(kernel.contains(&"tairix-kernel"));
            assert!(!programs.contains(&"tairix-kernel"));
            assert!(programs.contains(&"tairix-elsh"));
            assert!(!kernel.contains(&"tairix-elsh"));
            // `lib/rt` is the crate whose allocator collides; it is linted on
            // its own, with no program feature turned on by a sibling.
            assert!(selection(&crates, arch, Stratum::Lib).contains(&"tairix-rt"));
        }
    }

    /// A crate belongs to exactly one stratum, so no pass repeats another's
    /// work and none is silently skipped.
    #[test]
    fn the_strata_partition_the_product_tree() {
        let crates = graph();
        for &arch in PieArch::ALL {
            let mut seen: Vec<&str> = Vec::new();
            for &stratum in &Stratum::ALL {
                for package in selection(&crates, arch, stratum) {
                    assert!(!seen.contains(&package), "{package} linted twice");
                    seen.push(package);
                }
            }
            assert_eq!(seen.len(), dirs_of(&crates, arch).len());
        }
    }

    /// Host-only orchestration and test support are never cross-compiled, so
    /// linting them for a bare-metal triple would lint a configuration that
    /// does not exist. Before the target passes were added, the whole product
    /// tree went unlinted instead.
    #[test]
    fn no_pass_lints_host_orchestration_or_test_support() {
        let crates = graph();
        for &arch in PieArch::ALL {
            for dir in dirs_of(&crates, arch) {
                assert!(
                    Stratum::of(dir).is_some(),
                    "{dir} is not cross-compiled for {}",
                    arch.target_triple()
                );
            }
        }
    }

    /// Every pass covers the shipped strata; a stratum silently missing from
    /// the selection would be a hole in the gate.
    #[test]
    fn every_pass_covers_the_shipped_strata() {
        let crates = graph();
        for &arch in PieArch::ALL {
            let dirs = dirs_of(&crates, arch);
            for stratum in ["lib/", "kernel/", "drivers/", "userland/"] {
                assert!(
                    dirs.iter().any(|d| d.starts_with(stratum)),
                    "{} lints no {stratum} crate",
                    arch.target_triple()
                );
            }
        }
    }

    /// A backend only compiles for its own target; `kernel/arch/api` is
    /// architecture-neutral and belongs in every pass.
    #[test]
    fn a_pass_takes_only_its_own_architecture_backend() {
        let crates = graph();
        for &arch in PieArch::ALL {
            let dirs = dirs_of(&crates, arch);
            assert!(dirs.contains(&"kernel/arch/api"));
            assert!(dirs.contains(&backend_dir(arch)));
            for &other in PieArch::ALL {
                if other != arch {
                    assert!(
                        !dirs.contains(&backend_dir(other)),
                        "{} must not lint {}",
                        arch.target_triple(),
                        backend_dir(other)
                    );
                }
            }
            // No bare-metal triple can compile the browser backend.
            assert!(!dirs.contains(&WASM_BACKEND_DIR));
        }
    }

    /// The browser backend is the one Tier-1 configuration no other pass can
    /// reach, so its own pass must name it; before this stage nothing linted
    /// it at all.
    #[test]
    fn the_browser_backend_has_a_pass_of_its_own() {
        let crates = graph();
        let picked = wasm_arch(&crates);
        let dirs: Vec<&str> = crates
            .iter()
            .filter(|c| picked.contains(&c.name.as_str()))
            .map(|c| c.rel_dir.as_str())
            .collect();
        assert!(dirs.contains(&WASM_BACKEND_DIR));
        assert!(dirs.contains(&"kernel/arch/api"));
        assert_eq!(dirs.len(), 2);
    }
}
