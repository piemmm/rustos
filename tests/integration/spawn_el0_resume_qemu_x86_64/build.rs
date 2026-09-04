//! Build-time fixture generator for the `plans/PI.md` stage X1 x86_64
//! single-resumable-user-kthread vertical (the cross-port sibling of the
//! aarch64 `spawn_el0_timeshare` build script).
//!
//! Two jobs on the freestanding `x86_64-unknown-none` target:
//!
//! 1. Hand the production x86_64 kernel linker script to the test kernel (the
//!    test boots the real `tairix-kernel` pipeline, so it links exactly like
//!    the other freestanding x86_64 integration binaries).
//! 2. Compile the pure-Rust EL0 fixture program (`tests/integration/
//!    el0_yielder_program`) **position-independent** for the freestanding
//!    x86_64 target (the shared `program.ld` roots `tairix-rt`'s `_start`), into a
//!    private target directory under `OUT_DIR`, pinning its yield count through
//!    the `TAIRIX_EL0_YIELDS` environment variable so this script is the single
//!    source of truth for the count, then convert the linked
//!    PIE ELF to an `rxe` blob with [`tairix_itest_harness::elf2rxe::elf_to_rxe`],
//!    baking relocations for the [`tairix_itest_harness::USER_IMAGE_BIAS`] the kernel maps the image at and
//!    stamping the kernel's compiled-in syscall CFI tag
//!    (`tairix_kernel_syscall::SYSCALL_TABLE_HASH`) so
//!    [`tairix_abi::rxe::LoadImage::parse`] accepts it; emit the
//!    bytes, the bias, and the matching [`YIELDS_PER_TASK`] constant as a Rust
//!    source the test `include!`s.
//!
//! On any non-x86_64 target (host `cargo build --workspace`, clippy) it emits
//! inert stubs so the crate still builds; the kernel body that consumes them
//! compiles only for the freestanding x86_64 target.
//!
//! Re-running `build.rs` produces byte-identical output, so the test is
//! deterministic.

use std::env;
use std::fmt::Write as _;
use std::path::PathBuf;

use tairix_itest_harness::pie::PieArch;

/// How many times the EL0 task yields before exiting. The single source of
/// truth: passed to the program build via `TAIRIX_EL0_YIELDS` *and* emitted as
/// the `YIELDS_PER_TASK` constant the kernel asserts against, so the two halves
/// can never disagree. Large enough that an accidental
/// single run cannot satisfy the PASS check, small enough to drain well within
/// the harness budget.
const YIELDS_PER_TASK: u32 = 16;

/// Freestanding target this vertical cross-compiles for.
const ARCH: PieArch = PieArch::X86_64;

fn main() {
    tairix_itest_harness::emit_target_cfg();
    println!("cargo:rerun-if-changed=build.rs");

    let manifest_dir = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let out_dir = env::var("OUT_DIR").expect("OUT_DIR");
    let manifest_dir = manifest_dir.trim_end_matches('/');

    let rxe_path = PathBuf::from(&out_dir).join("program_rxe.rs");

    let target = env::var("TARGET").unwrap_or_default();
    if target == ARCH.target_triple() {
        // Hand the production x86_64 kernel linker script to the test kernel
        // itself (the single per-arch script the architecture port owns);
        // mirrors `kernel/tairix-kernel/build.rs` and the sibling x86_64
        // integration binaries.
        let linker = format!("{manifest_dir}/../../../kernel/arch/x86_64/linker.ld");
        println!("cargo:rerun-if-changed={linker}");
        println!("cargo:rustc-link-arg=-T{linker}");

        let rxe = tairix_itest_harness::program_fixture::GuestBuild {
            manifest_dir,
            out_dir: &out_dir,
            arch: ARCH,
            package: "tairix-test-el0-yielder",
            variant: None,
            env: &[("TAIRIX_EL0_YIELDS", YIELDS_PER_TASK.to_string())],
        }
        .program_rxe(&tairix_kernel_syscall::SYSCALL_TABLE_HASH);
        write_program_fixture(&rxe_path, &rxe);
    } else {
        // Inert stub for host / other targets; the kernel body that uses these
        // consts compiles only for the freestanding x86_64 target.
        write_program_fixture(&rxe_path, &[]);
    }
}

/// Emit `PROGRAM_RXE`, `USER_BIAS`, and `YIELDS_PER_TASK` as a Rust source the
/// test includes.
fn write_program_fixture(path: &std::path::Path, rxe: &[u8]) {
    let mut out = tairix_itest_harness::program_fixture::fixture_header();
    let _ = writeln!(
        out,
        "/// Times the EL0 task yields before exiting (pinned by build.rs)."
    );
    let _ = writeln!(out, "pub const YIELDS_PER_TASK: u64 = {YIELDS_PER_TASK};");
    tairix_itest_harness::program_fixture::push_rxe_blob(
        &mut out,
        "PROGRAM_RXE",
        "the el0-yielder fixture program",
        rxe,
    );
    tairix_itest_harness::program_fixture::write_fixture(path, &out);
}
