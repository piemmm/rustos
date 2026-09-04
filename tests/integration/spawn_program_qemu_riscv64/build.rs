//! Build-time fixture generator for the CCOMPAT stage CC3 riscv64 spawn
//! round-trip.
//!
//! The kernel-side test spawns the separately-linked fixture program
//! (`tests/integration/cc3_program`). The kernel spawn path consumes an `rxe`
//! load image, not a raw ELF, so this script:
//!
//! 1. compiles the fixture program **position-independent** for the freestanding
//!    riscv64 target (the shared `program.ld` roots crt0's `_start`), into a
//!    private target directory under `OUT_DIR` so it never collides with the
//!    outer build (one program source, built two ways);
//! 2. converts the linked PIE ELF to an `rxe` blob with
//!    [`tairix_itest_harness::elf2rxe::elf_to_rxe`], baking relocations for the
//!    [`tairix_itest_harness::USER_IMAGE_BIAS`] the kernel maps the image at and stamping the kernel's
//!    compiled-in syscall CFI tag (`tairix_kernel_syscall::SYSCALL_TABLE_HASH`)
//!    so [`tairix_abi::rxe::LoadImage::parse`] accepts it;
//! 3. emits the bytes and `USER_BIAS` as a Rust source the test `include!`s.
//!
//! On any non-riscv64 target (host `cargo build --workspace`, clippy) it emits
//! an inert stub so the crate still builds; the kernel body that consumes the
//! blob compiles only for the freestanding riscv64 target.

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
        // Hand the riscv64 `virt` linker script to the test kernel itself
        // (the single per-arch script the architecture port owns).
        let linker = format!("{manifest_dir}/../../../kernel/arch/riscv64/link/riscv64-virt.ld");
        println!("cargo:rerun-if-changed={linker}");
        println!("cargo:rustc-link-arg=-T{linker}");

        let rxe = tairix_itest_harness::program_fixture::GuestBuild {
            manifest_dir,
            out_dir: &out_dir,
            arch: ARCH,
            package: "tairix-test-cc3-program",
            variant: None,
            env: &[],
        }
        .program_rxe(&tairix_kernel_syscall::SYSCALL_TABLE_HASH);
        write_fixture(&rxe_path, &rxe);
    } else {
        // Inert stub for host / other targets; the kernel body that uses these
        // consts compiles only for the freestanding riscv64 target.
        write_fixture(&rxe_path, &[]);
    }
}

/// Emit `PROGRAM_RXE` and `USER_BIAS` as a Rust source file the test includes.
fn write_fixture(path: &std::path::Path, rxe: &[u8]) {
    let mut out = tairix_itest_harness::program_fixture::fixture_header();
    tairix_itest_harness::program_fixture::push_rxe_blob(
        &mut out,
        "PROGRAM_RXE",
        "the cc3 fixture program",
        rxe,
    );
    tairix_itest_harness::program_fixture::write_fixture(path, &out);
}
