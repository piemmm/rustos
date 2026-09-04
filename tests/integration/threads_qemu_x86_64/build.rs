//! Build-time fixture generator for the `SP11e` x86_64 demand-grown stack
//! vertical (the sibling of the aarch64/riscv64 `build.rs`).
//!
//! Two jobs on the freestanding `x86_64-unknown-none` target:
//!
//! 1. Hand the production x86_64 kernel linker script to the test kernel
//!    (the test runs the shared production board bring-up, so it links
//!    exactly like the other freestanding x86_64 integration binaries).
//! 2. Compile the pure-Rust EL0 fixture program
//!    (`tests/integration/threads_program`) **once** — the four roles
//!    are selected at runtime from the registry argument vector —
//!    position-independent for the freestanding x86_64 target, then convert
//!    the linked PIE ELF to an `rxe` blob with
//!    [`tairix_itest_harness::elf2rxe::elf_to_rxe`], baking relocations for
//!    the [`tairix_itest_harness::USER_IMAGE_BIAS`] the production spawn producer maps every image at
//!    and stamping the kernel's compiled-in syscall CFI tag
//!    (`tairix_kernel_syscall::SYSCALL_TABLE_HASH`) so
//!    [`tairix_abi::rxe::LoadImage::parse`] accepts it; emit the blob and
//!    the bias as a Rust source the test `include!`s.
//!
//! On any non-x86_64 target (host `cargo build --workspace`, clippy) it
//! emits inert stubs so the crate still builds; the kernel body that
//! consumes them compiles only for the freestanding x86_64 target.
//!
//! Re-running `build.rs` produces byte-identical output, so the test is
//! deterministic.

use std::env;
use std::path::PathBuf;

use tairix_itest_harness::pie::PieArch;

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
        // The test kernel itself links with the production x86_64 kernel
        // linker script the architecture port owns (the single per-arch
        // script); mirrors `kernel/tairix-kernel/build.rs` and the sibling
        // x86_64 integration binaries.
        let linker = format!("{manifest_dir}/../../../kernel/arch/x86_64/linker.ld");
        println!("cargo:rerun-if-changed={linker}");
        println!("cargo:rustc-link-arg=-T{linker}");

        let rxe = tairix_itest_harness::program_fixture::GuestBuild {
            manifest_dir,
            out_dir: &out_dir,
            arch: ARCH,
            package: "tairix-test-threads",
            variant: None,
            env: &[],
        }
        .program_rxe(&tairix_kernel_syscall::SYSCALL_TABLE_HASH);
        write_program_fixture(&rxe_path, &rxe);
    } else {
        // Inert stub for host / other targets; the kernel body that uses
        // these compiles only for the freestanding x86_64 target.
        write_program_fixture(&rxe_path, &[]);
    }
}

/// Emit `PROGRAM_RXE` and `USER_BIAS` as a Rust source the test includes.
fn write_program_fixture(path: &std::path::Path, rxe: &[u8]) {
    let mut out = tairix_itest_harness::program_fixture::fixture_header();
    tairix_itest_harness::program_fixture::push_rxe_blob(
        &mut out,
        "PROGRAM_RXE",
        "the four-role fixture program",
        rxe,
    );
    tairix_itest_harness::program_fixture::write_fixture(path, &out);
}
