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
//!    [`rustos_itest_harness::elf2rxe::elf_to_rxe`], baking relocations
//!    for the [`USER_BIAS`] the production spawn producer maps every image
//!    at and stamping the kernel's compiled-in syscall CFI tag
//!    (`rustos_kernel_syscall::SYSCALL_TABLE_HASH`) so
//!    [`rustos_abi::rxe::LoadImage::parse`] accepts it; emit the blob and
//!    the bias as a Rust source the test `include!`s.
//!
//! On any non-riscv64 target (host `cargo build --workspace`, clippy) it
//! emits inert stubs so the crate still builds; the kernel body that
//! consumes them compiles only for the freestanding riscv64 target.
//!
//! Re-running `build.rs` produces byte-identical output, so the test is
//! deterministic.

use std::env;
use std::fmt::Write as _;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

/// Virtual base the program image is mapped at. Must equal the production
/// spawn layout's `CHILD_USER_BIAS` (64 GiB) — the kernel body asserts the
/// two agree before spawning anything.
const USER_BIAS: u64 = 0x10_0000_0000;

/// Rust target triple of the freestanding riscv64 build.
const RISCV64_TARGET: &str = "riscv64gc-unknown-none-elf";

fn main() {
    rustos_itest_harness::emit_target_cfg();
    println!("cargo:rerun-if-changed=build.rs");

    let manifest_dir = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let out_dir = env::var("OUT_DIR").expect("OUT_DIR");
    let manifest_dir = manifest_dir.trim_end_matches('/');

    let program_dir = format!("{manifest_dir}/../stack_grow_program");
    println!("cargo:rerun-if-changed={program_dir}/src/main.rs");
    println!("cargo:rerun-if-changed={program_dir}/program.ld");
    println!("cargo:rerun-if-changed={program_dir}/Cargo.toml");

    let rxe_path = PathBuf::from(&out_dir).join("program_rxe.rs");

    let target = env::var("TARGET").unwrap_or_default();
    if target == RISCV64_TARGET {
        // The test kernel itself links with the riscv64 `virt` script the
        // architecture port owns (the single per-board script).
        let linker = format!("{manifest_dir}/../../../kernel/arch/riscv64/link/riscv64-virt.ld");
        println!("cargo:rerun-if-changed={linker}");
        println!("cargo:rustc-link-arg=-T{linker}");

        let rxe = build_and_convert_program(manifest_dir, &out_dir, &program_dir);
        write_program_fixture(&rxe_path, &rxe);
    } else {
        // Inert stubs for host / other targets; the kernel body that uses
        // these compiles only for the freestanding riscv64 target.
        write_program_fixture(&rxe_path, &[]);
    }
}

/// Compile the U-mode fixture program PIE for the freestanding riscv64
/// target and convert the linked ELF into an `rxe` blob.
fn build_and_convert_program(manifest_dir: &str, out_dir: &str, program_dir: &str) -> Vec<u8> {
    let program_ld = format!("{program_dir}/program.ld");
    let target_dir = format!("{out_dir}/stack-grow-target");

    // Cargo fingerprints the RUSTFLAGS *string* (which names the linker
    // script by path) but not the script's *content*, so a `program.ld` edit
    // would not by itself trigger a relink and the converter could read a
    // stale ELF. Wiping the private target directory here forces a clean
    // rebuild against the current script without churning ordinary
    // incremental builds.
    let _ = fs::remove_dir_all(&target_dir);

    // The program links no architecture crate, so `program.ld`'s
    // `ENTRY(_start)` roots `rustos-rt`'s trampoline; it is built
    // position-independent. Scope the PIE link flags to the riscv64 target
    // so the program's own host build script is unaffected, and build
    // `core` / `alloc` / `compiler_builtins` as PIC alongside it
    // (`-Z build-std`). `alloc` is required because `rustos-rt` registers a
    // `#[global_allocator]`, so the program names `alloc`.
    let cargo = env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let mut command = Command::new(cargo);
    command
        .current_dir(manifest_dir)
        // The outer build exports `CARGO_ENCODED_RUSTFLAGS` / `RUSTFLAGS`
        // into this build script's environment; both outrank the
        // target-scoped var below, so a nested cargo would inherit the outer
        // kernel's flags and drop the PIE link recipe. Clear them so the
        // target-scoped flags win and apply only to the riscv64 program
        // crates.
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .env_remove("RUSTFLAGS")
        .env(
            "CARGO_TARGET_RISCV64GC_UNKNOWN_NONE_ELF_RUSTFLAGS",
            format!("-C relocation-model=pie -C link-arg=-pie -C link-arg=-T{program_ld}"),
        )
        .args([
            "build",
            "-p",
            "rustos-test-stack-grow",
            "--target",
            RISCV64_TARGET,
            "-Z",
            "build-std=core,compiler_builtins,alloc",
            "--target-dir",
            &target_dir,
        ]);
    let status = command
        .status()
        .expect("spawn cargo to build the stack-grow fixture program");
    assert!(
        status.success(),
        "building the stack-grow fixture program failed"
    );

    let elf_path = format!("{target_dir}/{RISCV64_TARGET}/debug/rustos-test-stack-grow");
    let elf = fs::read(&elf_path).unwrap_or_else(|e| panic!("read {elf_path}: {e}"));

    rustos_itest_harness::elf2rxe::elf_to_rxe(
        &elf,
        &rustos_kernel_syscall::SYSCALL_TABLE_HASH,
        USER_BIAS,
    )
    .unwrap_or_else(|_| panic!("convert the stack-grow fixture program ELF into an rxe image"))
}

/// Emit `PROGRAM_RXE` and `USER_BIAS` as a Rust source the test includes.
fn write_program_fixture(path: &std::path::Path, rxe: &[u8]) {
    let mut out = String::new();
    out.push_str("// Auto-generated by build.rs. DO NOT EDIT.\n");
    let _ = writeln!(out, "/// Virtual base the program image is mapped at.");
    let _ = writeln!(out, "pub const USER_BIAS: u64 = {USER_BIAS:#x};");
    let _ = writeln!(
        out,
        "/// The converted `rxe` image of the four-role fixture program."
    );
    let _ = write!(out, "pub const PROGRAM_RXE: &[u8] = &[");
    for (i, b) in rxe.iter().enumerate() {
        if i % 16 == 0 {
            out.push_str("\n    ");
        }
        let _ = write!(out, "0x{b:02x}, ");
    }
    out.push_str("\n];\n");
    fs::write(path, out).expect("write program_rxe.rs");
}
