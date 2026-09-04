//! Build-time fixture generator for the `SP11c` riscv64 demand-grown stack
//! vertical — the riscv64 twin of
//! `tests/integration/stack_grow_qemu_aarch64/build.rs`.
//!
//! Two jobs on the freestanding `riscv64gc-unknown-none-elf` target
//! (unlike the aarch64 twin there is no DTB dump: OpenSBI passes the live
//! board tree in `a1`, so the test kernel reads it at boot):
//!
//! 1. Hand the riscv64 `virt` linker script to the test kernel (the single
//!    per-board script the architecture port owns).
//! 2. Compile the pure-Rust U-mode fixture program
//!    (`tests/integration/stack_grow_program`) **once** — the four roles
//!    are selected at runtime from the registry argument vector —
//!    position-independent for the freestanding riscv64 target, and
//!    convert the linked PIE ELF to an `rxe` blob with
//!    [`tairix_itest_harness::elf2rxe::elf_to_rxe`], baking relocations
//!    for the [`tairix_itest_harness::USER_IMAGE_BIAS`] the production spawn producer maps every image
//!    at and stamping the kernel's compiled-in syscall CFI tag
//!    (`tairix_kernel_syscall::SYSCALL_TABLE_HASH`) so
//!    [`tairix_abi::rxe::LoadImage::parse`] accepts it; emit the blob and
//!    the bias as a Rust source the test `include!`s.
//!
//! On any non-riscv64 target (host `cargo build --workspace`, clippy) it
//! emits inert stubs so the crate still builds; the kernel body that
//! consumes them compiles only for the freestanding riscv64 target.
//!
//! Re-running `build.rs` produces byte-identical output, so the test is
//! deterministic.

use std::env;
use std::path::PathBuf;

use tairix_itest_harness::pie::PieArch;

/// Freestanding target this vertical cross-compiles for.
const ARCH: PieArch = PieArch::Riscv64;

fn main() {
    tairix_itest_harness::emit_target_cfg();
    println!("cargo:rerun-if-changed=build.rs");

    let manifest_dir = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let out_dir = env::var("OUT_DIR").expect("OUT_DIR");
    let manifest_dir = manifest_dir.trim_end_matches('/');

    let rxe_path = PathBuf::from(&out_dir).join("program_rxe.rs");

    let target = env::var("TARGET").unwrap_or_default();
    if target == ARCH.target_triple() {
        // The test kernel itself links with the riscv64 `virt` script the
        // architecture port owns (the single per-board script).
        let linker = format!("{manifest_dir}/../../../kernel/arch/riscv64/link/riscv64-virt.ld");
        println!("cargo:rerun-if-changed={linker}");
        println!("cargo:rustc-link-arg=-T{linker}");

        let rxe = tairix_itest_harness::program_fixture::GuestBuild {
            manifest_dir,
            out_dir: &out_dir,
            arch: ARCH,
            package: "tairix-test-stack-grow",
            variant: None,
            env: &[],
        }
        .program_rxe(&tairix_kernel_syscall::SYSCALL_TABLE_HASH);
        write_program_fixture(&rxe_path, &rxe);
    } else {
        // Inert stubs for host / other targets; the kernel body that uses
        // these compiles only for the freestanding riscv64 target.
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
