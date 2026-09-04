//! Build-time fixture generator for the SPAWN stage `SP7b` aarch64
//! `signal`-delivery vertical.
//!
//! Three jobs on the freestanding `aarch64-unknown-none` target:
//!
//! 1. Hand the aarch64 `virt` linker script to the test kernel (the single
//!    per-board script the architecture port owns) and dump the canonical QEMU
//!    `virt` flattened device tree, embedding it so the test discovers the
//!    GICv2 base and the generic-timer rate from the firmware tree
//!    (`plans/PI.md` P3/P4). QEMU's `-kernel <ELF>` aarch64 path passes no DTB
//!    pointer (`x0 = 0`), so the board tree is embedded at build time; the dump
//!    helper lives in the shared harness so no aarch64 build script re-rolls it.
//! 2. Compile the pure-Rust EL0 fixture program (`tests/integration/
//!    signal_program`) **three times** — as the `child`, `parent`, and
//!    `intake` roles — position-independent for the freestanding aarch64 target
//!    (the shared `program.ld` roots `tairix-rt`'s `_start`), into two private
//!    target directories under `OUT_DIR`, selecting the role through the
//!    `TAIRIX_SIGNAL_ROLE` environment variable so this script is its single
//!    source of truth. Unlike the `wait` vertical the child's PID is threaded
//!    at *runtime* (the kernel writes it into the parent's startup arguments
//!    once the scheduler assigns it), so no build-time code is pinned.
//! 3. Convert each linked PIE ELF to an `rxe` blob with
//!    [`tairix_itest_harness::elf2rxe::elf_to_rxe`], baking relocations for the
//!    [`tairix_itest_harness::USER_IMAGE_BIAS`] the kernel maps the image at and stamping the kernel's
//!    compiled-in syscall CFI tag (`tairix_kernel_syscall::SYSCALL_TABLE_HASH`)
//!    so [`tairix_abi::rxe::LoadImage::parse`] accepts it; emit the three blobs
//!    and the bias as a Rust source the test `include!`s.
//!
//! On any non-aarch64 target (host `cargo build --workspace`, clippy) it emits
//! inert stubs so the crate still builds; the kernel body that consumes them
//! compiles only for the freestanding aarch64 target.
//!
//! Re-running `build.rs` produces byte-identical output, so the test is
//! deterministic.

use std::env;
use std::fmt::Write as _;
use std::path::PathBuf;

use tairix_itest_harness::pie::PieArch;

/// Freestanding target this vertical cross-compiles for.
const ARCH: PieArch = PieArch::Aarch64;

fn main() {
    tairix_itest_harness::emit_target_cfg();
    println!("cargo:rerun-if-changed=build.rs");

    let manifest_dir = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let out_dir = env::var("OUT_DIR").expect("OUT_DIR");
    let manifest_dir = manifest_dir.trim_end_matches('/');

    let rxe_path = PathBuf::from(&out_dir).join("program_rxe.rs");
    let dtb_path = PathBuf::from(&out_dir).join("dtb_fixture.rs");

    let target = env::var("TARGET").unwrap_or_default();
    if target == ARCH.target_triple() {
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
        let intake = build_and_convert_program(manifest_dir, &out_dir, "intake");
        write_program_fixture(&rxe_path, &child, &parent, &intake);
    } else {
        // Inert stubs for host / other targets; the kernel body that uses these
        // compiles only for the freestanding aarch64 target.
        write_dtb_fixture(&dtb_path, &[]);
        write_program_fixture(&rxe_path, &[], &[], &[]);
    }
}

/// Compile the EL0 fixture program in `role` ("child" / "parent" /
/// "intake") PIE for the
/// freestanding aarch64 target and convert the linked ELF into an `rxe` blob.
fn build_and_convert_program(manifest_dir: &str, out_dir: &str, role: &str) -> Vec<u8> {
    tairix_itest_harness::program_fixture::GuestBuild {
        manifest_dir,
        out_dir,
        arch: ARCH,
        package: "tairix-test-signal",
        variant: Some(role),
        env: &[("TAIRIX_SIGNAL_ROLE", role.to_string())],
    }
    .program_rxe(&tairix_kernel_syscall::SYSCALL_TABLE_HASH)
}

/// Emit `CHILD_RXE`, `PARENT_RXE`, `INTAKE_RXE`, and `USER_BIAS` as a Rust
/// source the test includes.
fn write_program_fixture(path: &std::path::Path, child: &[u8], parent: &[u8], intake: &[u8]) {
    let mut out = tairix_itest_harness::program_fixture::fixture_header();
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
    tairix_itest_harness::program_fixture::push_rxe_blob(
        &mut out,
        "INTAKE_RXE",
        "the intake role",
        intake,
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
