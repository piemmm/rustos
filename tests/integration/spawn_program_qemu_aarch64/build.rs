//! Build-time fixture generator for the CCOMPAT stage CC3 aarch64 spawn
//! round-trip.
//!
//! The kernel-side test spawns the separately-linked fixture program
//! (`tests/integration/cc3_program`). The kernel spawn path consumes an `rxe`
//! load image, not a raw ELF, so this script:
//!
//! 1. compiles the fixture program **position-independent** for the freestanding
//!    aarch64 target (the shared `program.ld` roots crt0's `_start`), into a
//!    private target directory under `OUT_DIR` so it never collides with the
//!    outer build (one program source, built two ways);
//! 2. converts the linked PIE ELF to an `rxe` blob with
//!    [`tairix_itest_harness::elf2rxe::elf_to_rxe`], baking relocations for the
//!    [`tairix_itest_harness::USER_IMAGE_BIAS`] the kernel maps the image at and stamping the kernel's
//!    compiled-in syscall CFI tag (`tairix_kernel_syscall::SYSCALL_TABLE_HASH`)
//!    so [`tairix_abi::rxe::LoadImage::parse`] accepts it;
//! 3. emits the bytes and `USER_BIAS` as a Rust source the test `include!`s.
//!
//! On any non-aarch64 target (host `cargo build --workspace`, clippy) it emits
//! an inert stub so the crate still builds; the kernel body that consumes the
//! blob compiles only for the freestanding aarch64 target.
//!
//! This mirrors the riscv64 sibling
//! (`tests/integration/spawn_program_qemu_riscv64/build.rs`); only the target
//! triple, its `RUSTFLAGS` environment-variable name, and the per-arch linker
//! script differ.

use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

/// Rust target triple of the freestanding aarch64 build.
const AARCH64_TARGET: &str = "aarch64-unknown-none";

fn main() {
    tairix_itest_harness::emit_target_cfg();
    println!("cargo:rerun-if-changed=build.rs");

    let manifest_dir = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let out_dir = env::var("OUT_DIR").expect("OUT_DIR");
    let manifest_dir = manifest_dir.trim_end_matches('/');

    let program_dir = format!("{manifest_dir}/../cc3_program");
    println!("cargo:rerun-if-changed={program_dir}/src/main.rs");
    println!(
        "cargo:rerun-if-changed={}",
        tairix_itest_harness::program_fixture::PROGRAM_LD
    );
    println!("cargo:rerun-if-changed={program_dir}/Cargo.toml");

    let rxe_path = PathBuf::from(&out_dir).join("program_rxe.rs");

    let target = env::var("TARGET").unwrap_or_default();
    if target == AARCH64_TARGET {
        // Hand the aarch64 `virt` linker script to the test kernel itself
        // (the single per-arch script the architecture port owns).
        let linker = format!("{manifest_dir}/../../../kernel/arch/aarch64/link/aarch64-virt.ld");
        println!("cargo:rerun-if-changed={linker}");
        println!("cargo:rustc-link-arg=-T{linker}");

        let rxe = build_and_convert_program(manifest_dir, &out_dir);
        write_fixture(&rxe_path, &rxe);
    } else {
        // Inert stub for host / other targets; the kernel body that uses these
        // consts compiles only for the freestanding aarch64 target.
        write_fixture(&rxe_path, &[]);
    }
}

/// Compile the fixture program PIE for the freestanding aarch64 target and
/// convert the linked ELF into an `rxe` blob.
fn build_and_convert_program(manifest_dir: &str, out_dir: &str) -> Vec<u8> {
    let program_ld = tairix_itest_harness::program_fixture::PROGRAM_LD;
    let target_dir = format!("{out_dir}/cc3-target");

    // Cargo fingerprints the RUSTFLAGS *string* (which names the linker script
    // by path) but not the script's *content*, so a `program.ld` edit would not
    // by itself trigger a relink and the converter could read a stale ELF.
    // `build.rs` only reruns when its `rerun-if-changed` inputs (including
    // `program.ld`) actually change, so wiping the private target directory
    // here forces a clean rebuild against the current script without churning
    // ordinary incremental builds.
    let _ = fs::remove_dir_all(&target_dir);

    // The program links no architecture crate, so `program.ld`'s
    // `ENTRY(_start)` roots crt0's trampoline; it is built position-independent. Scope the PIE link flags to the aarch64 target so
    // the program's own host build script is unaffected, and build `core` /
    // `compiler_builtins` as PIC alongside it (`-Z build-std`).
    let cargo = env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let status = Command::new(cargo)
        .current_dir(manifest_dir)
        // The outer build exports `CARGO_ENCODED_RUSTFLAGS` / `RUSTFLAGS` into
        // this build script's environment; both outrank the target-scoped var
        // below, so a nested cargo would inherit the outer kernel's flags and
        // drop the PIE link recipe. Clear them so the target-scoped flags win
        // and apply only to the aarch64 program crates (not the program's own
        // host build script).
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .env_remove("RUSTFLAGS")
        .env(
            "CARGO_TARGET_AARCH64_UNKNOWN_NONE_RUSTFLAGS",
            format!("-C relocation-model=pie -C link-arg=-pie -C link-arg=-T{program_ld}"),
        )
        .args([
            "build",
            "-p",
            "tairix-test-cc3-program",
            "--target",
            AARCH64_TARGET,
            "-Z",
            "build-std=core,compiler_builtins",
            "--target-dir",
            &target_dir,
        ])
        .status()
        .expect("spawn cargo to build the cc3 fixture program");
    assert!(status.success(), "building the cc3 fixture program failed");

    let elf_path = format!("{target_dir}/{AARCH64_TARGET}/debug/tairix-test-cc3-program");
    let elf = fs::read(&elf_path).unwrap_or_else(|e| panic!("read {elf_path}: {e}"));

    tairix_itest_harness::elf2rxe::elf_to_rxe(
        &elf,
        &tairix_kernel_syscall::SYSCALL_TABLE_HASH,
        tairix_itest_harness::USER_IMAGE_BIAS,
    )
    .expect("convert the cc3 fixture program ELF into an rxe image")
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
