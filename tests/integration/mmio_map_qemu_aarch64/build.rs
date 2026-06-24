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
//!    [`rustos_itest_harness::elf2rxe::elf_to_rxe`], baking relocations for the
//!    [`USER_BIAS`] the kernel maps the image at and stamping the kernel's
//!    compiled-in syscall CFI tag (`rustos_kernel_syscall::SYSCALL_TABLE_HASH`)
//!    so [`rustos_abi::rxe::LoadImage::parse`] accepts it; emit the
//!    bytes, the bias, and the matching grant/window constants as a Rust source
//!    the test `include!`s.
//!
//! On any non-aarch64 target (host `cargo build --workspace`, clippy) it emits
//! inert stubs so the crate still builds; the kernel body that consumes them
//! compiles only for the freestanding aarch64 target.

use std::env;
use std::fmt::Write as _;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

/// Virtual base the program image is mapped at. Chosen at 64 GiB — far above
/// the kernel's 2 GiB identity map and within the 39-bit (512 GiB) TTBR0
/// region — so the image lands on freshly walked stage-1 tables (the proven
/// spawn layout).
const USER_BIAS: u64 = 0x10_0000_0000;

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

/// Rust target triple of the freestanding aarch64 build.
const AARCH64_TARGET: &str = "aarch64-unknown-none";

fn main() {
    rustos_itest_harness::emit_target_cfg();
    println!("cargo:rerun-if-changed=build.rs");

    let manifest_dir = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let out_dir = env::var("OUT_DIR").expect("OUT_DIR");
    let manifest_dir = manifest_dir.trim_end_matches('/');

    let program_dir = format!("{manifest_dir}/../mmio_map_program");
    println!("cargo:rerun-if-changed={program_dir}/src/main.rs");
    println!("cargo:rerun-if-changed={program_dir}/program.ld");
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
        let dtb = rustos_itest_harness::dump_aarch64_virt_dtb(&out_dir_os, 1);
        write_dtb_fixture(&dtb_path, &dtb);

        let rxe = build_and_convert_program(manifest_dir, &out_dir, &program_dir);
        write_program_fixture(&rxe_path, &rxe);
    } else {
        // Inert stubs for host / other targets; the kernel body that uses these
        // compiles only for the freestanding aarch64 target.
        write_dtb_fixture(&dtb_path, &[]);
        write_program_fixture(&rxe_path, &[]);
    }
}

/// Compile the EL0 fixture program PIE for the freestanding aarch64 target and
/// convert the linked ELF into an `rxe` blob.
fn build_and_convert_program(manifest_dir: &str, out_dir: &str, program_dir: &str) -> Vec<u8> {
    let program_ld = format!("{program_dir}/program.ld");
    let target_dir = format!("{out_dir}/mmio-map-target");

    // Wipe the private target directory so a `program.ld` edit forces a clean
    // relink (cargo fingerprints the RUSTFLAGS string, not the script's
    // content); `build.rs` only reruns when its `rerun-if-changed` inputs
    // actually change, so this does not churn ordinary incremental builds.
    let _ = fs::remove_dir_all(&target_dir);

    let cargo = env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let status = Command::new(cargo)
        .current_dir(manifest_dir)
        // Clear the outer build's RUSTFLAGS so the target-scoped PIE recipe
        // below wins and applies only to the aarch64 program crates.
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .env_remove("RUSTFLAGS")
        // Pin the grant handle, the expected register magic, and the register
        // offset (the single source of truth shared with the kernel).
        .env("RUSTOS_MMIO_GRANT_HANDLE", GRANT_HANDLE.to_string())
        .env("RUSTOS_MMIO_MAGIC", u64::from(MAGIC).to_string())
        .env("RUSTOS_MMIO_REG_OFFSET", REG_OFFSET.to_string())
        .env(
            "CARGO_TARGET_AARCH64_UNKNOWN_NONE_RUSTFLAGS",
            format!("-C relocation-model=pie -C link-arg=-pie -C link-arg=-T{program_ld}"),
        )
        .args([
            "build",
            "-p",
            "rustos-test-mmio-map",
            "--target",
            AARCH64_TARGET,
            "-Z",
            "build-std=core,compiler_builtins,alloc",
            "--target-dir",
            &target_dir,
        ])
        .status()
        .expect("spawn cargo to build the mmio-map fixture program");
    assert!(
        status.success(),
        "building the mmio-map fixture program failed"
    );

    let elf_path = format!("{target_dir}/{AARCH64_TARGET}/debug/rustos-test-mmio-map");
    let elf = fs::read(&elf_path).unwrap_or_else(|e| panic!("read {elf_path}: {e}"));

    rustos_itest_harness::elf2rxe::elf_to_rxe(
        &elf,
        &rustos_kernel_syscall::SYSCALL_TABLE_HASH,
        USER_BIAS,
    )
    .expect("convert the mmio-map fixture program ELF into an rxe image")
}

/// Emit `PROGRAM_RXE`, `USER_BIAS`, and the grant/window constants as a Rust
/// source the test includes.
fn write_program_fixture(path: &std::path::Path, rxe: &[u8]) {
    let mut out = String::new();
    out.push_str("// Auto-generated by build.rs. DO NOT EDIT.\n");
    let _ = writeln!(out, "/// Virtual base the program image is mapped at.");
    let _ = writeln!(out, "pub const USER_BIAS: u64 = {USER_BIAS:#x};");
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
    out.push_str("/// The converted `rxe` image of the mmio-map fixture program.\n");
    out.push_str("pub const PROGRAM_RXE: &[u8] = &[");
    for (i, b) in rxe.iter().enumerate() {
        if i % 16 == 0 {
            out.push_str("\n    ");
        }
        let _ = write!(out, "0x{b:02x}, ");
    }
    out.push_str("\n];\n");
    fs::write(path, out).expect("write program_rxe.rs");
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
    fs::write(path, out).expect("write dtb_fixture.rs");
}
