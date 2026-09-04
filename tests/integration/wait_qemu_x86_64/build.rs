//! Build-time fixture generator for the `plans/PI.md` stage X4 x86_64 `wait`
//! vertical (the cross-port sibling of the aarch64 `wait_qemu_aarch64` build
//! script).
//!
//! Two jobs on the freestanding `x86_64-unknown-none` target:
//!
//! 1. Hand the production x86_64 kernel linker script to the test kernel (the
//!    test boots the real `tairix-kernel` pipeline, so it links exactly like
//!    the other freestanding x86_64 integration binaries).
//! 2. Compile the pure-Rust EL0 fixture program (`tests/integration/
//!    wait_program`) **twice** — once as the `child` role and once as the
//!    `parent` role — position-independent for the freestanding x86_64 target
//!    (the shared `program.ld` roots `tairix-rt`'s `_start`), into two private
//!    target directories under `OUT_DIR`, pinning the child's exit code through
//!    the `TAIRIX_WAIT_CHILD_CODE` environment variable (and the role through
//!    `TAIRIX_WAIT_ROLE`) so this script is the single source of truth for both, then convert each linked PIE ELF to an `rxe` blob with
//!    [`tairix_itest_harness::elf2rxe::elf_to_rxe`], baking relocations for the
//!    [`tairix_itest_harness::USER_IMAGE_BIAS`] the kernel maps the image at and stamping the kernel's
//!    compiled-in syscall CFI tag (`tairix_kernel_syscall::SYSCALL_TABLE_HASH`)
//!    so [`tairix_abi::rxe::LoadImage::parse`] accepts it; emit the
//!    two blobs, the bias, and the matching [`CHILD_EXIT_CODE`] constant as a
//!    Rust source the test `include!`s.
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

/// Exit code the child terminates with and the parent verifies after reaping
/// it. The single source of truth: passed to *both* program builds via
/// `TAIRIX_WAIT_CHILD_CODE` *and* emitted as the `CHILD_EXIT_CODE` constant the
/// kernel asserts the reaped code against, so the three sites can never
/// disagree. A non-trivial, non-zero value so an accidental
/// zero-exit cannot satisfy the check.
const CHILD_EXIT_CODE: i32 = 23;

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

        let child = build_and_convert_program(manifest_dir, &out_dir, "child");
        let parent = build_and_convert_program(manifest_dir, &out_dir, "parent");
        write_program_fixture(&rxe_path, &child, &parent);
    } else {
        // Inert stubs for host / other targets; the kernel body that uses these
        // compiles only for the freestanding x86_64 target.
        write_program_fixture(&rxe_path, &[], &[]);
    }
}

/// Compile the EL0 fixture program in `role` ("child" / "parent") PIE for the
/// freestanding x86_64 target and convert the linked ELF into an `rxe` blob.
fn build_and_convert_program(manifest_dir: &str, out_dir: &str, role: &str) -> Vec<u8> {
    tairix_itest_harness::program_fixture::GuestBuild {
        manifest_dir,
        out_dir,
        arch: ARCH,
        package: "tairix-test-wait",
        variant: Some(role),
        env: &[
            ("TAIRIX_WAIT_ROLE", role.to_string()),
            ("TAIRIX_WAIT_CHILD_CODE", CHILD_EXIT_CODE.to_string()),
        ],
    }
    .program_rxe(&tairix_kernel_syscall::SYSCALL_TABLE_HASH)
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
