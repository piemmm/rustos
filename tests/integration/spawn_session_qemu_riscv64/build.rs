//! Build-time fixture generator for the `plans/PI.md` stage RV-X3 riscv64
//! runtime-`spawn` concurrent-producer vertical (the cross-port sibling of the
//! `aarch64`/`x86_64` `spawn_session` build scripts).
//!
//! Two jobs on the freestanding `riscv64gc-unknown-none-elf` target:
//!
//! 1. Hand the riscv64 `virt` linker script to the test kernel (the single
//!    per-arch script the architecture port owns), exactly
//!    as the sibling riscv64 integration binaries do.
//! 2. Compile the pure-Rust spawn-session fixture program
//!    (`tests/integration/spawn_session_program`) **twice** — once as the
//!    **parent** role and once as the **child** (session) role, selected by the
//!    `TAIRIX_SPAWN_ROLE` environment variable — **position-independent** for the
//!    freestanding riscv64 target (the shared `program.ld` roots `tairix-rt`'s
//!    `_start`), each into a private target directory under `OUT_DIR`, pinning
//!    the yield count through `TAIRIX_SPAWN_YIELDS` so this script is the single
//!    source of truth for the count. Each linked PIE ELF is
//!    converted to an `rxe` blob with
//!    [`tairix_itest_harness::elf2rxe::elf_to_rxe`], baking relocations for the
//!    [`tairix_itest_harness::USER_IMAGE_BIAS`] the kernel maps each image at and stamping the kernel's
//!    compiled-in syscall CFI tag (`tairix_kernel_syscall::SYSCALL_TABLE_HASH`)
//!    so [`tairix_abi::rxe::LoadImage::parse`] accepts it; emit the
//!    two blobs, the shared bias, and the matching [`YIELDS_PER_TASK`] constant
//!    as a Rust source the test `include!`s. One source serves both roles: the per-process isolation is the separate page-table
//!    hierarchies the kernel builds, not separate sources.
//!
//! On any non-riscv64 target (host `cargo build --workspace`, clippy) it emits
//! inert stubs so the crate still builds; the kernel body that consumes them
//! compiles only for the freestanding riscv64 target.
//!
//! Re-running `build.rs` produces byte-identical output, so the test is
//! deterministic.

use std::env;
use std::fmt::Write as _;
use std::path::PathBuf;

use tairix_itest_harness::pie::PieArch;

/// How many times each role yields before exiting. The single source of truth:
/// passed to both program builds via `TAIRIX_SPAWN_YIELDS` *and* emitted as the
/// `YIELDS_PER_TASK` constant the kernel asserts against, so the two halves can
/// never disagree. Large enough that an accidental single
/// run cannot satisfy the PASS check, small enough to drain well within the
/// harness budget.
const YIELDS_PER_TASK: u32 = 8;

/// Freestanding target this vertical cross-compiles for.
const ARCH: PieArch = PieArch::Riscv64;

fn main() {
    tairix_itest_harness::emit_target_cfg();
    println!("cargo:rerun-if-changed=build.rs");

    let manifest_dir = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let out_dir = env::var("OUT_DIR").expect("OUT_DIR");
    let manifest_dir = manifest_dir.trim_end_matches('/');

    let program_dir = format!("{manifest_dir}/../spawn_session_program");
    println!("cargo:rerun-if-changed={program_dir}/build.rs");

    let rxe_path = PathBuf::from(&out_dir).join("program_rxe.rs");

    let target = env::var("TARGET").unwrap_or_default();
    if target == ARCH.target_triple() {
        // The test kernel itself links with the riscv64 `virt` script the
        // architecture port owns (the single per-arch script).
        let linker = format!("{manifest_dir}/../../../kernel/arch/riscv64/link/riscv64-virt.ld");
        println!("cargo:rerun-if-changed={linker}");
        println!("cargo:rustc-link-arg=-T{linker}");

        let parent = build_and_convert_program(manifest_dir, &out_dir, "parent");
        let child = build_and_convert_program(manifest_dir, &out_dir, "child");
        write_program_fixture(&rxe_path, &parent, &child);
    } else {
        // Inert stub for host / other targets; the kernel body that uses these
        // consts compiles only for the freestanding riscv64 target.
        write_program_fixture(&rxe_path, &[], &[]);
    }
}

/// Compile the spawn-session fixture program PIE for the freestanding riscv64
/// target in the given `role` (`"parent"` or `"child"`) and convert the linked
/// ELF into an `rxe` blob.
fn build_and_convert_program(manifest_dir: &str, out_dir: &str, role: &str) -> Vec<u8> {
    tairix_itest_harness::program_fixture::GuestBuild {
        manifest_dir,
        out_dir,
        arch: ARCH,
        package: "tairix-test-spawn-session-program",
        variant: Some(role),
        env: &[
            ("TAIRIX_SPAWN_ROLE", role.to_string()),
            ("TAIRIX_SPAWN_YIELDS", YIELDS_PER_TASK.to_string()),
        ],
    }
    .program_rxe(&tairix_kernel_syscall::SYSCALL_TABLE_HASH)
}

/// Emit `PARENT_RXE`, `CHILD_RXE`, `USER_BIAS`, and `YIELDS_PER_TASK` as a Rust
/// source the test includes.
fn write_program_fixture(path: &std::path::Path, parent: &[u8], child: &[u8]) {
    let mut out = tairix_itest_harness::program_fixture::fixture_header();
    let _ = writeln!(
        out,
        "/// Times each role yields before exiting (pinned by build.rs)."
    );
    let _ = writeln!(out, "pub const YIELDS_PER_TASK: u64 = {YIELDS_PER_TASK};");
    tairix_itest_harness::program_fixture::push_rxe_blob(
        &mut out,
        "PARENT_RXE",
        "the spawn-session fixture in the parent role",
        parent,
    );
    tairix_itest_harness::program_fixture::push_rxe_blob(
        &mut out,
        "CHILD_RXE",
        "the spawn-session fixture in the child (session) role",
        child,
    );
    tairix_itest_harness::program_fixture::write_fixture(path, &out);
}
