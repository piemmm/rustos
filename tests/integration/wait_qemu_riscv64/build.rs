//! Build-time fixture generator for the `plans/PI.md` stage RV-X4 riscv64
//! `wait`-reap vertical (the cross-port sibling of the `aarch64`/`x86_64`
//! `wait_qemu_*` build scripts).
//!
//! Two jobs on the freestanding `riscv64gc-unknown-none-elf` target:
//!
//! 1. Hand the riscv64 `virt` linker script to the test kernel (the single
//!    per-arch script the architecture port owns — `AGENTS.md` §2.2), exactly
//!    as the sibling riscv64 integration binaries do. Unlike the aarch64
//!    sibling there is no embedded device tree: OpenSBI passes the boot hart
//!    the live `dtb` pointer in `a1`, which the test reads for the generic-timer
//!    rate at boot.
//! 2. Compile the pure-Rust EL0 fixture program (`tests/integration/
//!    wait_program`) **twice** — once as the `child` role and once as the
//!    `parent` role — position-independent for the freestanding riscv64 target
//!    (its own `program.ld` roots `rustos-rt`'s `_start`), into two private
//!    target directories under `OUT_DIR`, pinning the child's exit code through
//!    the `RUSTOS_WAIT_CHILD_CODE` environment variable (and the role through
//!    `RUSTOS_WAIT_ROLE`) so this script is the single source of truth for both
//!    (`AGENTS.md` §2.2). Each linked PIE ELF is converted to an `rxe` blob with
//!    [`rustos_itest_harness::elf2rxe::elf_to_rxe`], baking relocations for the
//!    [`USER_BIAS`] the kernel maps the image at and stamping the kernel's
//!    compiled-in syscall CFI tag (`rustos_kernel_syscall::SYSCALL_TABLE_HASH`)
//!    so [`rustos_abi::rxe::LoadImage::parse`] accepts it (§9 / §19.2); emit the
//!    two blobs, the bias, and the matching [`CHILD_EXIT_CODE`] constant as a
//!    Rust source the test `include!`s.
//!
//! On any non-riscv64 target (host `cargo build --workspace`, clippy) it emits
//! inert stubs so the crate still builds; the kernel body that consumes them
//! compiles only for the freestanding riscv64 target.
//!
//! Re-running `build.rs` produces byte-identical output, so the test is
//! deterministic (`AGENTS.md` §7).

use std::env;
use std::fmt::Write as _;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

/// Virtual base both program images are mapped at, in *its own* address space.
/// Chosen at 64 GiB — far above the kernel's identity map — so each program's
/// pages land on freshly walked Sv39 tables instead of colliding with an
/// identity gigapage leaf. The two programs live in *separate* address spaces,
/// so they share the bias without colliding (`AGENTS.md` §2.2 — the proven
/// riscv64 spawn layout).
const USER_BIAS: u64 = 0x10_0000_0000;

/// Exit code the child terminates with and the parent verifies after reaping
/// it. The single source of truth: passed to *both* program builds via
/// `RUSTOS_WAIT_CHILD_CODE` *and* emitted as the `CHILD_EXIT_CODE` constant the
/// kernel asserts the reaped code against, so the three sites can never
/// disagree (`AGENTS.md` §2.2). A non-trivial, non-zero value so an accidental
/// zero-exit cannot satisfy the check.
const CHILD_EXIT_CODE: i32 = 23;

/// Rust target triple of the freestanding riscv64 build.
const RISCV64_TARGET: &str = "riscv64gc-unknown-none-elf";

fn main() {
    rustos_itest_harness::emit_target_cfg();
    println!("cargo:rerun-if-changed=build.rs");

    let manifest_dir = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let out_dir = env::var("OUT_DIR").expect("OUT_DIR");
    let manifest_dir = manifest_dir.trim_end_matches('/');

    let program_dir = format!("{manifest_dir}/../wait_program");
    println!("cargo:rerun-if-changed={program_dir}/src/main.rs");
    println!("cargo:rerun-if-changed={program_dir}/program.ld");
    println!("cargo:rerun-if-changed={program_dir}/Cargo.toml");

    let rxe_path = PathBuf::from(&out_dir).join("program_rxe.rs");

    let target = env::var("TARGET").unwrap_or_default();
    if target == RISCV64_TARGET {
        // The test kernel itself links with the riscv64 `virt` script the
        // architecture port owns (the single per-arch script, §2.2).
        let linker = format!("{manifest_dir}/../../../kernel/arch/riscv64/link/riscv64-virt.ld");
        println!("cargo:rerun-if-changed={linker}");
        println!("cargo:rustc-link-arg=-T{linker}");

        let child = build_and_convert_program(manifest_dir, &out_dir, &program_dir, "child");
        let parent = build_and_convert_program(manifest_dir, &out_dir, &program_dir, "parent");
        write_program_fixture(&rxe_path, &child, &parent);
    } else {
        // Inert stubs for host / other targets; the kernel body that uses these
        // compiles only for the freestanding riscv64 target.
        write_program_fixture(&rxe_path, &[], &[]);
    }
}

/// Compile the EL0 fixture program in `role` ("child" / "parent") PIE for the
/// freestanding riscv64 target and convert the linked ELF into an `rxe` blob.
fn build_and_convert_program(
    manifest_dir: &str,
    out_dir: &str,
    program_dir: &str,
    role: &str,
) -> Vec<u8> {
    let program_ld = format!("{program_dir}/program.ld");
    let target_dir = format!("{out_dir}/wait-{role}-target");

    // Cargo fingerprints the RUSTFLAGS *string* (which names the linker script
    // by path) but not the script's *content*, so a `program.ld` edit would not
    // by itself trigger a relink and the converter could read a stale ELF. A
    // role / code change is keyed by the per-role target dir and the
    // `rerun-if-env-changed` declarations in the program's own build script.
    // Wiping the private target directory here forces a clean rebuild against
    // the current script + role without churning ordinary incremental builds.
    let _ = fs::remove_dir_all(&target_dir);

    // The program links no architecture crate, so `program.ld`'s
    // `ENTRY(_start)` roots `rustos-rt`'s trampoline; it is built
    // position-independent (`AGENTS.md` §19.2). Scope the PIE link flags to the
    // riscv64 target so the program's own host build script is unaffected, and
    // build `core` / `alloc` / `compiler_builtins` as PIC alongside it
    // (`-Z build-std`). `alloc` is required because `rustos-rt` registers a
    // `#[global_allocator]`, so the program names `alloc`.
    let cargo = env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let status = Command::new(cargo)
        .current_dir(manifest_dir)
        // The outer build exports `CARGO_ENCODED_RUSTFLAGS` / `RUSTFLAGS` into
        // this build script's environment; both outrank the target-scoped var
        // below, so a nested cargo would inherit the outer kernel's flags and
        // drop the PIE link recipe. Clear them so the target-scoped flags win
        // and apply only to the riscv64 program crates.
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .env_remove("RUSTFLAGS")
        // Pin the role + child exit code (the §2.2 single source of truth).
        .env("RUSTOS_WAIT_ROLE", role)
        .env("RUSTOS_WAIT_CHILD_CODE", CHILD_EXIT_CODE.to_string())
        .env(
            "CARGO_TARGET_RISCV64GC_UNKNOWN_NONE_ELF_RUSTFLAGS",
            format!("-C relocation-model=pie -C link-arg=-pie -C link-arg=-T{program_ld}"),
        )
        .args([
            "build",
            "-p",
            "rustos-test-wait",
            "--target",
            RISCV64_TARGET,
            "-Z",
            "build-std=core,compiler_builtins,alloc",
            "--target-dir",
            &target_dir,
        ])
        .status()
        .expect("spawn cargo to build the wait fixture program");
    assert!(
        status.success(),
        "building the wait fixture program ({role}) failed"
    );

    let elf_path = format!("{target_dir}/{RISCV64_TARGET}/debug/rustos-test-wait");
    let elf = fs::read(&elf_path).unwrap_or_else(|e| panic!("read {elf_path}: {e}"));

    rustos_itest_harness::elf2rxe::elf_to_rxe(
        &elf,
        &rustos_kernel_syscall::SYSCALL_TABLE_HASH,
        USER_BIAS,
    )
    .unwrap_or_else(|_| panic!("convert the wait fixture program ELF ({role}) into an rxe image"))
}

/// Emit `CHILD_RXE`, `PARENT_RXE`, `USER_BIAS`, and `CHILD_EXIT_CODE` as a Rust
/// source the test includes.
fn write_program_fixture(path: &std::path::Path, child: &[u8], parent: &[u8]) {
    let mut out = String::new();
    out.push_str("// Auto-generated by build.rs. DO NOT EDIT.\n");
    let _ = writeln!(out, "/// Virtual base each program image is mapped at.");
    let _ = writeln!(out, "pub const USER_BIAS: u64 = {USER_BIAS:#x};");
    let _ = writeln!(
        out,
        "/// Exit code the child terminates with and the parent verifies (pinned by build.rs)."
    );
    let _ = writeln!(out, "pub const CHILD_EXIT_CODE: i32 = {CHILD_EXIT_CODE};");
    emit_blob(&mut out, "CHILD_RXE", "the child role", child);
    emit_blob(&mut out, "PARENT_RXE", "the parent role", parent);
    fs::write(path, out).expect("write program_rxe.rs");
}

/// Emit one named `&[u8]` blob constant.
fn emit_blob(out: &mut String, name: &str, what: &str, bytes: &[u8]) {
    let _ = writeln!(out, "/// The converted `rxe` image of {what}.");
    let _ = write!(out, "pub const {name}: &[u8] = &[");
    for (i, b) in bytes.iter().enumerate() {
        if i % 16 == 0 {
            out.push_str("\n    ");
        }
        let _ = write!(out, "0x{b:02x}, ");
    }
    out.push_str("\n];\n");
}
