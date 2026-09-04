//! Build-time fixture generator for the `plans/PI.md` stage X2 x86_64
//! two-task EL0 timeshare vertical (the cross-port sibling of the aarch64
//! `spawn_el0_timeshare` build script).
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
//!    source the test `include!`s. Both isolated address spaces are built from
//!    the same validated image.
//!
//! On any non-x86_64 target (host `cargo build --workspace`, clippy) it emits
//! inert stubs so the crate still builds; the kernel body that consumes them
//! compiles only for the freestanding x86_64 target.
//!
//! Re-running `build.rs` produces byte-identical output, so the test is
//! deterministic.

use std::env;
use std::fmt::Write as _;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

/// How many times each EL0 task yields before exiting. The single source of
/// truth: passed to the program build via `TAIRIX_EL0_YIELDS` *and* emitted as
/// the `YIELDS_PER_TASK` constant the kernel asserts against, so the two halves
/// can never disagree. Large enough that an accidental
/// single run cannot satisfy the PASS check, small enough to drain well within
/// the harness budget.
const YIELDS_PER_TASK: u32 = 16;

/// Rust target triple of the freestanding x86_64 build.
const X86_64_TARGET: &str = "x86_64-unknown-none";

fn main() {
    tairix_itest_harness::emit_target_cfg();
    println!("cargo:rerun-if-changed=build.rs");

    let manifest_dir = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let out_dir = env::var("OUT_DIR").expect("OUT_DIR");
    let manifest_dir = manifest_dir.trim_end_matches('/');

    let program_dir = format!("{manifest_dir}/../el0_yielder_program");
    println!("cargo:rerun-if-changed={program_dir}/src/main.rs");
    println!(
        "cargo:rerun-if-changed={}",
        tairix_itest_harness::program_fixture::PROGRAM_LD
    );
    println!("cargo:rerun-if-changed={program_dir}/Cargo.toml");

    let rxe_path = PathBuf::from(&out_dir).join("program_rxe.rs");

    let target = env::var("TARGET").unwrap_or_default();
    if target == X86_64_TARGET {
        // Hand the production x86_64 kernel linker script to the test kernel
        // itself (the single per-arch script the architecture port owns);
        // mirrors `kernel/tairix-kernel/build.rs` and the sibling x86_64
        // integration binaries.
        let linker = format!("{manifest_dir}/../../../kernel/arch/x86_64/linker.ld");
        println!("cargo:rerun-if-changed={linker}");
        println!("cargo:rustc-link-arg=-T{linker}");

        let rxe = build_and_convert_program(manifest_dir, &out_dir);
        write_program_fixture(&rxe_path, &rxe);
    } else {
        // Inert stub for host / other targets; the kernel body that uses these
        // consts compiles only for the freestanding x86_64 target.
        write_program_fixture(&rxe_path, &[]);
    }
}

/// Compile the EL0 fixture program PIE for the freestanding x86_64 target and
/// convert the linked ELF into an `rxe` blob.
fn build_and_convert_program(manifest_dir: &str, out_dir: &str) -> Vec<u8> {
    let program_ld = tairix_itest_harness::program_fixture::PROGRAM_LD;
    let target_dir = format!("{out_dir}/el0-yielder-target");

    // Cargo fingerprints the RUSTFLAGS *string* (which names the linker script
    // by path) but not the script's *content*, so a `program.ld` edit would not
    // by itself trigger a relink and the converter could read a stale ELF.
    // `build.rs` only reruns when its `rerun-if-changed` inputs (including
    // `program.ld`) actually change, so wiping the private target directory
    // here forces a clean rebuild against the current script without churning
    // ordinary incremental builds.
    let _ = fs::remove_dir_all(&target_dir);

    // The program links no architecture crate, so `program.ld`'s
    // `ENTRY(_start)` roots `tairix-rt`'s trampoline; it is built
    // position-independent. Scope the PIE link flags to the
    // x86_64 target so the program's own host build script is unaffected, and
    // build `core` / `alloc` / `compiler_builtins` as PIC alongside it
    // (`-Z build-std`). `alloc` is required because `tairix-rt` registers a
    // `#[global_allocator]`, so the program names `alloc`; omitting it would
    // pull `alloc` from the prebuilt sysroot while `core` is built fresh, a
    // duplicate-lang-item link error.
    let cargo = env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let status = Command::new(cargo)
        .current_dir(manifest_dir)
        // The outer build exports `CARGO_ENCODED_RUSTFLAGS` / `RUSTFLAGS` into
        // this build script's environment; both outrank the target-scoped var
        // below, so a nested cargo would inherit the outer kernel's flags and
        // drop the PIE link recipe. Clear them so the target-scoped flags win
        // and apply only to the x86_64 program crates (not the program's own
        // host build script).
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .env_remove("RUSTFLAGS")
        // Pin the program's yield count (the single source of truth).
        .env("TAIRIX_EL0_YIELDS", YIELDS_PER_TASK.to_string())
        .env(
            "CARGO_TARGET_X86_64_UNKNOWN_NONE_RUSTFLAGS",
            format!("-C relocation-model=pie -C link-arg=-pie -C link-arg=-T{program_ld}"),
        )
        .args([
            "build",
            "-p",
            "tairix-test-el0-yielder",
            "--target",
            X86_64_TARGET,
            "-Z",
            "build-std=core,compiler_builtins,alloc",
            "--target-dir",
            &target_dir,
        ])
        .status()
        .expect("spawn cargo to build the el0-yielder fixture program");
    assert!(
        status.success(),
        "building the el0-yielder fixture program failed"
    );

    let elf_path = format!("{target_dir}/{X86_64_TARGET}/debug/tairix-test-el0-yielder");
    let elf = fs::read(&elf_path).unwrap_or_else(|e| panic!("read {elf_path}: {e}"));

    tairix_itest_harness::elf2rxe::elf_to_rxe(
        &elf,
        &tairix_kernel_syscall::SYSCALL_TABLE_HASH,
        tairix_itest_harness::USER_IMAGE_BIAS,
    )
    .expect("convert the el0-yielder fixture program ELF into an rxe image")
}

/// Emit `PROGRAM_RXE`, `USER_BIAS`, and `YIELDS_PER_TASK` as a Rust source the
/// test includes.
fn write_program_fixture(path: &std::path::Path, rxe: &[u8]) {
    let mut out = tairix_itest_harness::program_fixture::fixture_header();
    let _ = writeln!(
        out,
        "/// Times each EL0 task yields before exiting (pinned by build.rs)."
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
