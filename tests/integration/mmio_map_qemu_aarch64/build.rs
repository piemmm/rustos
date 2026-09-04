//! Build-time fixture generator for the PI `5d-0-ii (b′)-2` aarch64 `mmio_map`
//! vertical (mirrors `mem_map_qemu_aarch64/build.rs`).
//!
//! Three jobs on the freestanding `aarch64-unknown-none` target:
//!
//! 1. Hand the aarch64 `virt` linker script to the test kernel (the single
//!    per-board script the architecture port owns) and dump
//!    the canonical QEMU `virt` flattened device tree, embedding it so the test
//!    discovers the GICv2 base and the generic-timer rate from the firmware
//!    tree (`plans/PI.md` P3/P4). QEMU's `-kernel <ELF>` aarch64 path passes no
//!    DTB pointer (`x0 = 0`), so the board tree is embedded at build time.
//! 2. Compile the pure-Rust EL0 fixture program (`tests/integration/
//!    mmio_map_program`) **position-independent** for the freestanding aarch64
//!    target, pinning the grant handle, the expected register magic, and the
//!    register offset through environment variables so this script is the
//!    single source of truth shared by the program and the kernel.
//! 3. Convert the linked PIE ELF to an `rxe` blob with
//!    [`tairix_itest_harness::elf2rxe::elf_to_rxe`], baking relocations for the
//!    [`tairix_itest_harness::USER_IMAGE_BIAS`] the kernel maps the image at and stamping the kernel's
//!    compiled-in syscall CFI tag (`tairix_kernel_syscall::SYSCALL_TABLE_HASH`)
//!    so [`tairix_abi::rxe::LoadImage::parse`] accepts it; emit the
//!    bytes, the bias, and the matching grant/window constants as a Rust source
//!    the test `include!`s.
//!
//! On any non-aarch64 target (host `cargo build --workspace`, clippy) it emits
//! inert stubs so the crate still builds; the kernel body that consumes them
//! compiles only for the freestanding aarch64 target.

use std::env;
use std::fmt::Write as _;
use std::path::PathBuf;

use tairix_itest_harness::pie::PieArch;

/// The device-resource grant handle the program maps. The first grant a task
/// is minted is handle `1` (the registry issues per-task handles monotonic
/// from `1`, `0` reserved-invalid); the kernel mints exactly one for this
/// program. Passed to the program build *and* emitted for the kernel so the
/// two halves can never disagree.
const GRANT_HANDLE: u64 = 1;

/// Physical base of the granted device window: the first QEMU `virt`
/// virtio-MMIO transport. Its register block reports the virtio `MagicValue`
/// at offset 0 unconditionally, so the read-back proves the window points at
/// genuine device MMIO (emitted for the kernel grant).
const GRANT_PHYS: u64 = 0x0a00_0000;

/// Length in bytes of the granted window (one page covers the transport's
/// register block).
const GRANT_LEN: u64 = 0x1000;

/// Expected value of the device's first register: the virtio-MMIO `MagicValue`
/// ("virt", little-endian).
const MAGIC: u32 = 0x7472_6976;

/// Offset of the register the program reads back (the virtio-MMIO `MagicValue`
/// is at offset 0).
const REG_OFFSET: u64 = 0;

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
            package: "tairix-test-mmio-map",
            variant: None,
            env: &[
                ("TAIRIX_MMIO_GRANT_HANDLE", GRANT_HANDLE.to_string()),
                ("TAIRIX_MMIO_MAGIC", u64::from(MAGIC).to_string()),
                ("TAIRIX_MMIO_REG_OFFSET", REG_OFFSET.to_string()),
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

/// Emit `PROGRAM_RXE`, `USER_BIAS`, and the grant/window constants as a Rust
/// source the test includes.
fn write_program_fixture(path: &std::path::Path, rxe: &[u8]) {
    let mut out = tairix_itest_harness::program_fixture::fixture_header();
    let _ = writeln!(
        out,
        "/// Grant handle the kernel mints and the program maps (build.rs)."
    );
    let _ = writeln!(out, "pub const GRANT_HANDLE: u64 = {GRANT_HANDLE};");
    let _ = writeln!(
        out,
        "/// Physical base of the granted virtio-MMIO window (build.rs)."
    );
    let _ = writeln!(out, "pub const GRANT_PHYS: u64 = {GRANT_PHYS:#x};");
    let _ = writeln!(out, "/// Length in bytes of the granted window (build.rs).");
    let _ = writeln!(out, "pub const GRANT_LEN: u64 = {GRANT_LEN:#x};");
    tairix_itest_harness::program_fixture::push_rxe_blob(
        &mut out,
        "PROGRAM_RXE",
        "the mmio-map fixture program",
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
