//! Build-time fixture generator for the x86_64 ring-3 wild-fault vertical
//! (`plans/OPEN-DEFECTS.md` D42, D86).
//!
//! Two jobs on the freestanding `x86_64-unknown-none` target:
//!
//! 1. Hand the production x86_64 kernel linker script to the test kernel
//!    (the test runs the shared production board bring-up, so it links
//!    exactly like the other freestanding x86_64 integration binaries).
//! 2. Compile the pure-Rust ring-3 fixture program
//!    (`tests/integration/wild_fault_program`) **once** — the four roles are
//!    selected at runtime from the registry argument vector —
//!    position-independent against the shared fixture PIE layout
//!    (`tests/integration/harness/program.ld`), then convert the linked ELF
//!    to an `rxe` blob with
//!    [`tairix_itest_harness::elf2rxe::elf_to_rxe`], baking relocations for
//!    the [`USER_BIAS`] the production spawn producer maps every image at
//!    and stamping the kernel's compiled-in syscall CFI tag
//!    (`tairix_kernel_syscall::SYSCALL_TABLE_HASH`) so
//!    `tairix_abi::rxe::LoadImage::parse` accepts it; emit the blob and the
//!    bias as a Rust source the test `include!`s.
//!
//! On any non-x86_64 target (host `cargo build --workspace`, clippy) it
//! emits inert stubs so the crate still builds; the kernel body that
//! consumes them compiles only for the freestanding x86_64 target.
//!
//! Re-running `build.rs` produces byte-identical output, so the test is
//! deterministic.

use std::env;
use std::fmt::Write as _;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

/// Virtual base the program image is mapped at. Must equal the production
/// x86_64 spawn producer's `CHILD_USER_BIAS` (64 GiB) — the kernel body
/// asserts the two agree before spawning anything.
const USER_BIAS: u64 = 0x10_0000_0000;

/// Rust target triple of the freestanding x86_64 build.
const X86_64_TARGET: &str = "x86_64-unknown-none";

fn main() {
    tairix_itest_harness::emit_target_cfg();
    println!("cargo:rerun-if-changed=build.rs");

    let manifest_dir = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let out_dir = env::var("OUT_DIR").expect("OUT_DIR");
    let manifest_dir = manifest_dir.trim_end_matches('/');

    let program_dir = format!("{manifest_dir}/../wild_fault_program");
    println!("cargo:rerun-if-changed={program_dir}/src/main.rs");
    println!("cargo:rerun-if-changed={program_dir}/build.rs");
    println!("cargo:rerun-if-changed={program_dir}/Cargo.toml");

    let rxe_path = PathBuf::from(&out_dir).join("program_rxe.rs");

    let target = env::var("TARGET").unwrap_or_default();
    if target == X86_64_TARGET {
        // The test kernel itself links with the production x86_64 kernel
        // linker script the architecture port owns (the single per-arch
        // script); mirrors `kernel/tairix-kernel/build.rs` and the sibling
        // x86_64 integration binaries.
        let linker = format!("{manifest_dir}/../../../kernel/arch/x86_64/linker.ld");
        println!("cargo:rerun-if-changed={linker}");
        println!("cargo:rustc-link-arg=-T{linker}");

        let rxe = build_and_convert_program(manifest_dir, &out_dir);
        write_program_fixture(&rxe_path, &rxe);
    } else {
        // Inert stub for host / other targets; the kernel body that uses
        // these compiles only for the freestanding x86_64 target.
        write_program_fixture(&rxe_path, &[]);
    }
}

/// Compile the ring-3 fixture program PIE for the freestanding x86_64
/// target and convert the linked ELF into an `rxe` blob.
fn build_and_convert_program(manifest_dir: &str, out_dir: &str) -> Vec<u8> {
    // The one shared fixture PIE layout, owned by the harness beside the
    // rest of the fixture build glue.
    let program_ld = format!("{manifest_dir}/../harness/program.ld");
    println!("cargo:rerun-if-changed={program_ld}");
    let target_dir = format!("{out_dir}/wild-fault-target");

    // Cargo fingerprints the RUSTFLAGS *string* (which names the linker
    // script by path) but not the script's *content*, so a `program.ld` edit
    // would not by itself trigger a relink and the converter could read a
    // stale ELF. Wiping the private target directory here forces a clean
    // rebuild against the current script without churning ordinary
    // incremental builds.
    let _ = fs::remove_dir_all(&target_dir);

    // The program links no architecture crate, so `program.ld`'s
    // `ENTRY(_start)` roots `tairix-rt`'s trampoline; it is built
    // position-independent. Scope the PIE link flags to the x86_64 target so
    // the program's own host build script is unaffected, and build `core` /
    // `alloc` / `compiler_builtins` as PIC alongside it (`-Z build-std`).
    // `alloc` is required because `tairix-rt` registers a
    // `#[global_allocator]`, so the program names `alloc`.
    let cargo = env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let mut command = Command::new(cargo);
    command
        .current_dir(manifest_dir)
        // The outer build exports `CARGO_ENCODED_RUSTFLAGS` / `RUSTFLAGS`
        // into this build script's environment; both outrank the
        // target-scoped var below, so a nested cargo would inherit the outer
        // kernel's flags and drop the PIE link recipe. Clear them so the
        // target-scoped flags win and apply only to the x86_64 program
        // crates.
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .env_remove("RUSTFLAGS")
        .env(
            "CARGO_TARGET_X86_64_UNKNOWN_NONE_RUSTFLAGS",
            format!("-C relocation-model=pie -C link-arg=-pie -C link-arg=-T{program_ld}"),
        )
        .args([
            "build",
            "-p",
            "tairix-test-wild-fault",
            "--target",
            X86_64_TARGET,
            "-Z",
            "build-std=core,compiler_builtins,alloc",
            "--target-dir",
            &target_dir,
        ]);
    let status = command
        .status()
        .expect("spawn cargo to build the wild-fault fixture program");
    assert!(
        status.success(),
        "building the wild-fault fixture program failed"
    );

    let elf_path = format!("{target_dir}/{X86_64_TARGET}/debug/tairix-test-wild-fault");
    let elf = fs::read(&elf_path).unwrap_or_else(|e| panic!("read {elf_path}: {e}"));

    tairix_itest_harness::elf2rxe::elf_to_rxe(
        &elf,
        &tairix_kernel_syscall::SYSCALL_TABLE_HASH,
        USER_BIAS,
    )
    .unwrap_or_else(|_| panic!("convert the wild-fault fixture program ELF into an rxe image"))
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
