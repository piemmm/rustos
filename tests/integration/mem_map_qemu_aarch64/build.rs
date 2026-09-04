//! Build-time fixture generator for the SPAWN stage `SP5b-2` aarch64
//! `mem_map`/`mem_unmap` vertical.
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
//!    mem_map_program`) **position-independent** for the freestanding aarch64
//!    target (the shared `program.ld` roots `tairix-rt`'s `_start`), into a
//!    private target directory under `OUT_DIR`, pinning the anonymous-region
//!    base + length through the `TAIRIX_MEM_MAP_ADDR` / `TAIRIX_MEM_MAP_LEN`
//!    environment variables so this script is the single source of truth for
//!    the region the program maps *and* the kernel's fault check verifies.
//! 3. Convert the linked PIE ELF to an `rxe` blob with
//!    [`tairix_itest_harness::elf2rxe::elf_to_rxe`], baking relocations for the
//!    [`tairix_itest_harness::USER_IMAGE_BIAS`] the kernel maps the image at and stamping the kernel's
//!    compiled-in syscall CFI tag (`tairix_kernel_syscall::SYSCALL_TABLE_HASH`)
//!    so [`tairix_abi::rxe::LoadImage::parse`] accepts it; emit the
//!    bytes, the bias, and the matching [`REGION_VA`] / [`REGION_LEN`]
//!    constants as a Rust source the test `include!`s.
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

/// Virtual base of the anonymous region the program maps with `mem_map`
/// (FIXED). 16 MiB above [`tairix_itest_harness::USER_IMAGE_BIAS`] — clear of the program image, its user
/// stack, and the startup-vector block — so the region lands on fresh stage-1
/// tables and never overlaps the spawn-time image. The single source of truth:
/// passed to the program build via `TAIRIX_MEM_MAP_ADDR` *and* emitted as the
/// `REGION_VA` constant the kernel's fault handler checks the faulting address
/// against, so the two halves can never disagree.
const REGION_VA: u64 = tairix_itest_harness::USER_IMAGE_BIAS + (16 << 20);

/// Length in bytes of the anonymous region (two pages). Passed to the program
/// build via `TAIRIX_MEM_MAP_LEN` and emitted as the `REGION_LEN` constant the
/// kernel sizes its fault-range check from.
const REGION_LEN: u64 = 2 * 4096;

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

        let rxe = tairix_itest_harness::program_fixture::GuestBuild {
            manifest_dir,
            out_dir: &out_dir,
            arch: ARCH,
            package: "tairix-test-mem-map",
            variant: None,
            env: &[
                ("TAIRIX_MEM_MAP_ADDR", REGION_VA.to_string()),
                ("TAIRIX_MEM_MAP_LEN", REGION_LEN.to_string()),
            ],
        }
        .program_rxe(&tairix_kernel_syscall::SYSCALL_TABLE_HASH);
        write_program_fixture(&rxe_path, &rxe);
    } else {
        // Inert stubs for host / other targets; the kernel body that uses these
        // compiles only for the freestanding aarch64 target.
        write_dtb_fixture(&dtb_path, &[]);
        write_program_fixture(&rxe_path, &[]);
    }
}

/// Emit `PROGRAM_RXE`, `USER_BIAS`, `REGION_VA`, and `REGION_LEN` as a Rust
/// source the test includes.
fn write_program_fixture(path: &std::path::Path, rxe: &[u8]) {
    let mut out = tairix_itest_harness::program_fixture::fixture_header();
    let _ = writeln!(
        out,
        "/// Virtual base the program maps its anonymous region at (build.rs)."
    );
    let _ = writeln!(out, "pub const REGION_VA: u64 = {REGION_VA:#x};");
    let _ = writeln!(
        out,
        "/// Length in bytes of the anonymous region (pinned by build.rs)."
    );
    let _ = writeln!(out, "pub const REGION_LEN: u64 = {REGION_LEN};");
    tairix_itest_harness::program_fixture::push_rxe_blob(
        &mut out,
        "PROGRAM_RXE",
        "the mem-map fixture program",
        rxe,
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
