//! Build-time fixture generator for the SPAWN stage `SP6b` aarch64 `wait`-reap
//! vertical.
//!
//! Three jobs on the freestanding `aarch64-unknown-none` target:
//!
//! 1. Hand the aarch64 `virt` linker script to the test kernel (the single
//!    per-board script the architecture port owns) and dump
//!    the canonical QEMU `virt` flattened device tree, embedding it so the test
//!    discovers the GICv2 base and the generic-timer rate from the firmware
//!    tree (`plans/PI.md` P3/P4). QEMU's `-kernel <ELF>` aarch64 path passes no
//!    DTB pointer (`x0 = 0`), so the board tree is embedded at build time; the
//!    dump helper lives in the shared harness so no aarch64 build script
//!    re-rolls it.
//! 2. Compile the pure-Rust EL0 fixture program (`tests/integration/
//!    wait_program`) **twice** — once as the `child` role and once as the
//!    `parent` role — position-independent for the freestanding aarch64 target
//!    (the shared `program.ld` roots `tairix-rt`'s `_start`), into two private
//!    target directories under `OUT_DIR`, pinning the child's exit code through
//!    the `TAIRIX_WAIT_CHILD_CODE` environment variable (and the role through
//!    `TAIRIX_WAIT_ROLE`) so this script is the single source of truth for both.
//! 3. Convert each linked PIE ELF to an `rxe` blob with
//!    [`tairix_itest_harness::elf2rxe::elf_to_rxe`], baking relocations for the
//!    [`tairix_itest_harness::USER_IMAGE_BIAS`] the kernel maps the image at and stamping the kernel's
//!    compiled-in syscall CFI tag (`tairix_kernel_syscall::SYSCALL_TABLE_HASH`)
//!    so [`tairix_abi::rxe::LoadImage::parse`] accepts it; emit the
//!    two blobs, the bias, and the matching [`CHILD_EXIT_CODE`] constant as a
//!    Rust source the test `include!`s.
//!
//! On any non-aarch64 target (host `cargo build --workspace`, clippy) it emits
//! inert stubs so the crate still builds; the kernel body that consumes them
//! compiles only for the freestanding aarch64 target.
//!
//! Re-running `build.rs` produces byte-identical output, so the test is
//! deterministic.

use std::env;
use std::fmt::Write as _;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

/// Exit code the child terminates with and the parent verifies after reaping
/// it. The single source of truth: passed to *both* program builds via
/// `TAIRIX_WAIT_CHILD_CODE` *and* emitted as the `CHILD_EXIT_CODE` constant the
/// kernel asserts the reaped code against, so the three sites can never
/// disagree. A non-trivial, non-zero value so an accidental
/// zero-exit cannot satisfy the check.
const CHILD_EXIT_CODE: i32 = 23;

/// Rust target triple of the freestanding aarch64 build.
const AARCH64_TARGET: &str = "aarch64-unknown-none";

fn main() {
    tairix_itest_harness::emit_target_cfg();
    println!("cargo:rerun-if-changed=build.rs");

    let manifest_dir = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let out_dir = env::var("OUT_DIR").expect("OUT_DIR");
    let manifest_dir = manifest_dir.trim_end_matches('/');

    let program_dir = format!("{manifest_dir}/../wait_program");
    println!("cargo:rerun-if-changed={program_dir}/src/main.rs");
    println!(
        "cargo:rerun-if-changed={}",
        tairix_itest_harness::program_fixture::PROGRAM_LD
    );
    println!("cargo:rerun-if-changed={program_dir}/Cargo.toml");

    let rxe_path = PathBuf::from(&out_dir).join("program_rxe.rs");
    let dtb_path = PathBuf::from(&out_dir).join("dtb_fixture.rs");

    let target = env::var("TARGET").unwrap_or_default();
    if target == AARCH64_TARGET {
        // The test kernel itself links with the aarch64 `virt` script the
        // architecture port owns (the single per-board script).
        let linker = format!("{manifest_dir}/../../../kernel/arch/aarch64/link/aarch64-virt.ld");
        println!("cargo:rerun-if-changed={linker}");
        println!("cargo:rustc-link-arg=-T{linker}");

        // One CPU: this is the single-core live-scheduler slice.
        let out_dir_os = std::ffi::OsString::from(&out_dir);
        let dtb = tairix_itest_harness::dump_aarch64_virt_dtb(&out_dir_os, 1);
        write_dtb_fixture(&dtb_path, &dtb);

        let child = build_and_convert_program(manifest_dir, &out_dir, "child");
        let parent = build_and_convert_program(manifest_dir, &out_dir, "parent");
        write_program_fixture(&rxe_path, &child, &parent);
    } else {
        // Inert stubs for host / other targets; the kernel body that uses these
        // compiles only for the freestanding aarch64 target.
        write_dtb_fixture(&dtb_path, &[]);
        write_program_fixture(&rxe_path, &[], &[]);
    }
}

/// Compile the EL0 fixture program in `role` ("child" / "parent") PIE for the
/// freestanding aarch64 target and convert the linked ELF into an `rxe` blob.
fn build_and_convert_program(manifest_dir: &str, out_dir: &str, role: &str) -> Vec<u8> {
    let program_ld = tairix_itest_harness::program_fixture::PROGRAM_LD;
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
    // `ENTRY(_start)` roots `tairix-rt`'s trampoline; it is built
    // position-independent. Scope the PIE link flags to the
    // aarch64 target so the program's own host build script is unaffected, and
    // build `core` / `alloc` / `compiler_builtins` as PIC alongside it
    // (`-Z build-std`). `alloc` is required because `tairix-rt` registers a
    // `#[global_allocator]`, so the program names `alloc`.
    let cargo = env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let status = Command::new(cargo)
        .current_dir(manifest_dir)
        // The outer build exports `CARGO_ENCODED_RUSTFLAGS` / `RUSTFLAGS` into
        // this build script's environment; both outrank the target-scoped var
        // below, so a nested cargo would inherit the outer kernel's flags and
        // drop the PIE link recipe. Clear them so the target-scoped flags win
        // and apply only to the aarch64 program crates.
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .env_remove("RUSTFLAGS")
        // Pin the role + child exit code (the single source of truth).
        .env("TAIRIX_WAIT_ROLE", role)
        .env("TAIRIX_WAIT_CHILD_CODE", CHILD_EXIT_CODE.to_string())
        .env(
            "CARGO_TARGET_AARCH64_UNKNOWN_NONE_RUSTFLAGS",
            format!("-C relocation-model=pie -C link-arg=-pie -C link-arg=-T{program_ld}"),
        )
        .args([
            "build",
            "-p",
            "tairix-test-wait",
            "--target",
            AARCH64_TARGET,
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

    let elf_path = format!("{target_dir}/{AARCH64_TARGET}/debug/tairix-test-wait");
    let elf = fs::read(&elf_path).unwrap_or_else(|e| panic!("read {elf_path}: {e}"));

    tairix_itest_harness::elf2rxe::elf_to_rxe(
        &elf,
        &tairix_kernel_syscall::SYSCALL_TABLE_HASH,
        tairix_itest_harness::USER_IMAGE_BIAS,
    )
    .unwrap_or_else(|_| panic!("convert the wait fixture program ELF ({role}) into an rxe image"))
}

/// Emit `CHILD_RXE`, `PARENT_RXE`, `USER_BIAS`, and `CHILD_EXIT_CODE` as a Rust
/// source the test includes.
fn write_program_fixture(path: &std::path::Path, child: &[u8], parent: &[u8]) {
    let mut out = tairix_itest_harness::program_fixture::fixture_header();
    let _ = writeln!(
        out,
        "/// Exit code the child terminates with and the parent verifies (pinned by build.rs)."
    );
    let _ = writeln!(out, "pub const CHILD_EXIT_CODE: i32 = {CHILD_EXIT_CODE};");
    tairix_itest_harness::program_fixture::push_rxe_blob(
        &mut out,
        "CHILD_RXE",
        "the child role",
        child,
    );
    tairix_itest_harness::program_fixture::push_rxe_blob(
        &mut out,
        "PARENT_RXE",
        "the parent role",
        parent,
    );
    tairix_itest_harness::program_fixture::write_fixture(path, &out);
}

/// Emit the embedded `virt` device tree as a Rust source the test includes.
fn write_dtb_fixture(path: &std::path::Path, dtb: &[u8]) {
    let mut out = String::new();
    out.push_str("// Auto-generated by build.rs. DO NOT EDIT.\n");
    out.push_str("/// Canonical QEMU `virt` flattened device tree, dumped at build\n");
    out.push_str("/// time for the aarch64-none target (empty on host builds).\n");
    out.push_str("pub const DTB_BLOB: &[u8] = &[");
    for (i, b) in dtb.iter().enumerate() {
        if i % 16 == 0 {
            out.push_str("\n    ");
        }
        let _ = write!(out, "0x{b:02x}, ");
    }
    out.push_str("\n];\n");
    tairix_itest_harness::program_fixture::write_fixture(path, &out);
}
