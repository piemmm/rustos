//! Build-time generator for the autoload-root image fixture
//! (`plans/PI.md` P10 5d-2-ii(b-2-iii)).
//!
//! Unlike the per-vertical build scripts, this one is **host-only and target-
//! independent**: the fixture's bytes are produced on the build host (the
//! `tools/xtask` harness plants them on the test's virtio-blk backing), so the
//! signed driver bundle must always be built, whatever the outer target. It:
//!
//! 1. Cross-compiles the pure-Rust virtio-input keyboard driver
//!    (`drivers/input/virtio_kbd`) **position-independent** for the freestanding
//!    `aarch64-unknown-none` target — the architecture the `-M virt` vertical
//!    boots — using the driver's own `Run.ld` (the production PIE layout the
//!    bundle's payload links with), into a private target directory under
//!    `OUT_DIR`.
//! 2. Converts the linked PIE ELF to an `rxe` blob
//!    ([`rustos_itest_harness::elf2rxe::elf_to_rxe`]), baking relocations for the
//!    shared [`rustos_itest_harness::USER_IMAGE_BIAS`] the production spawn
//!    producer maps every child image at and stamping the kernel's compiled-in
//!    syscall CFI tag (`rustos_kernel_syscall::SYSCALL_TABLE_HASH`) so
//!    [`rustos_abi::rxe::LoadImage::parse`] accepts it.
//! 3. Wraps that `rxe` as the payload of a signed `kind = UserSpace`
//!    `DriverManifest` ([`rustos_itest_harness::driver_image`]) — requesting the
//!    capabilities the driver needs (`CAP_MMIO_MAP`, `CAP_MEM_DMA`,
//!    `CAP_INPUT_INJECT`), carrying the driver's own `BIND_KEYS`, and
//!    **signed with the kernel's own driver-signing seed**
//!    (`build_support::KERNEL_DRIVER_SIGNING_SEED`, the single source the kernel
//!    build derives its embedded trust anchor from) so the
//!    bundle verifies against `KERNEL_DRIVER_SIGNER_PUBKEY` at boot.
//! 4. Emits the bundle bytes and the signer public key as a Rust source the
//!    library `include!`s.
//!
//! Re-running `build.rs` produces byte-identical output, so the fixture is
//! deterministic.

use std::env;
use std::fmt::Write as _;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

use rustos_abi::{CapabilityId, DriverKind};
use rustos_itest_harness::driver_image::build_signed_driver_image;
use rustos_itest_harness::elf2rxe::elf_to_rxe;
use rustos_itest_harness::USER_IMAGE_BIAS;

/// The single source of the kernel's driver-signing seed:
/// the kernel build signs its embedded in-kernel manifests with it and derives
/// the `KERNEL_DRIVER_SIGNER_PUBKEY` trust anchor from it, so a bundle signed
/// here with the same seed is admitted by the booted kernel's load gate.
/// `dead_code` is allowed because this `#[path]` include also carries the build
/// script's target-selection helpers, which this fixture does not use.
#[allow(dead_code)]
#[path = "../../../kernel/rustos-kernel/src/build_support.rs"]
mod build_support;

/// Rust target triple of the freestanding aarch64 driver build.
const AARCH64_TARGET: &str = "aarch64-unknown-none";

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    let manifest_dir = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let manifest_dir = manifest_dir.trim_end_matches('/');
    let out_dir = env::var("OUT_DIR").expect("OUT_DIR");

    let driver_dir = format!("{manifest_dir}/../../../drivers/input/virtio_kbd");
    let kernel_build_support =
        format!("{manifest_dir}/../../../kernel/rustos-kernel/src/build_support.rs");
    println!("cargo:rerun-if-changed={driver_dir}/src/main.rs");
    println!("cargo:rerun-if-changed={driver_dir}/Run.ld");
    println!("cargo:rerun-if-changed={driver_dir}/Cargo.toml");
    println!("cargo:rerun-if-changed={kernel_build_support}");

    let rxe = build_and_convert_driver(manifest_dir, &out_dir, &driver_dir);

    // The bundle the kernel autoloads: the driver's own bind table, the
    // capabilities it needs (mapped register window, coherent DMA, the device
    // interrupt line it parks on, and keyboard injection), signed with the
    // kernel's driver-signing seed.
    let signed = build_signed_driver_image(
        &build_support::KERNEL_DRIVER_SIGNING_SEED,
        DriverKind::UserSpace,
        &[
            CapabilityId::MMIO_MAP,
            CapabilityId::MEM_DMA,
            CapabilityId::IRQ_BIND,
            CapabilityId::INPUT_INJECT,
        ],
        rustos_drv_input_virtio_input::BIND_KEYS,
        rustos_kernel_syscall::SYSCALL_TABLE_HASH,
        &rxe,
    );

    write_bundle_fixture(&out_dir, &signed.image, &signed.signer_pubkey);
}

/// Compile the virtio-input keyboard driver PIE for the freestanding aarch64
/// target and convert the linked ELF into an `rxe` blob relocated for
/// [`USER_IMAGE_BIAS`].
fn build_and_convert_driver(manifest_dir: &str, out_dir: &str, driver_dir: &str) -> Vec<u8> {
    let run_ld = format!("{driver_dir}/Run.ld");
    let target_dir = format!("{out_dir}/virtio-kbd-target");

    // Cargo fingerprints the RUSTFLAGS *string* (which names the linker script
    // by path) but not the script's *content*, so a `Run.ld` edit would not by
    // itself trigger a relink and the converter could read a stale ELF. The
    // `rerun-if-changed` inputs cover `Run.ld`, so wiping the private target
    // directory forces a clean rebuild against the current script without
    // churning ordinary incremental builds.
    let _ = fs::remove_dir_all(&target_dir);

    // The driver links no architecture crate, so `Run.ld`'s `ENTRY(_start)`
    // roots `rustos-rt`'s trampoline; it is built position-independent. Scope the PIE link flags to the aarch64 target and
    // build `core`/`alloc`/`compiler_builtins` as PIC alongside it
    // (`-Z build-std`); `alloc` is required because `rustos-rt` registers a
    // `#[global_allocator]`. The outer build's RUSTFLAGS are cleared so the
    // target-scoped recipe wins and applies only to the aarch64 driver crates.
    let cargo = env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let status = Command::new(cargo)
        .current_dir(manifest_dir)
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .env_remove("RUSTFLAGS")
        .env(
            "CARGO_TARGET_AARCH64_UNKNOWN_NONE_RUSTFLAGS",
            format!("-C relocation-model=pie -C link-arg=-pie -C link-arg=-T{run_ld}"),
        )
        .args([
            "build",
            "-p",
            "rustos-drv-input-virtio-kbd",
            "--target",
            AARCH64_TARGET,
            "-Z",
            "build-std=core,compiler_builtins,alloc",
            "--target-dir",
            &target_dir,
        ])
        .status()
        .expect("spawn cargo to build the virtio_kbd driver");
    assert!(status.success(), "building the virtio_kbd driver failed");

    let elf_path = format!("{target_dir}/{AARCH64_TARGET}/debug/rustos-drv-input-virtio-kbd");
    let elf = fs::read(&elf_path).unwrap_or_else(|e| panic!("read {elf_path}: {e}"));

    elf_to_rxe(
        &elf,
        &rustos_kernel_syscall::SYSCALL_TABLE_HASH,
        USER_IMAGE_BIAS,
    )
    .expect("convert the virtio_kbd driver ELF into an rxe image")
}

/// Emit the signed bundle bytes and signer public key as a Rust source the
/// library includes.
fn write_bundle_fixture(out_dir: &str, image: &[u8], pubkey: &[u8; 32]) {
    let mut out = String::new();
    out.push_str("// Auto-generated by build.rs. DO NOT EDIT.\n");
    out.push_str("/// Signed `kind = UserSpace` driver bundle whose payload is the\n");
    out.push_str("/// virtio-input keyboard driver `rxe`, planted into the encrypted\n");
    out.push_str("/// root's `/System/Drivers/` store and admitted through the kernel's\n");
    out.push_str("/// signed autoload gate against `KERNEL_DRIVER_SIGNER_PUBKEY`.\n");
    out.push_str("pub const VIRTIO_KBD_BUNDLE: &[u8] = &[");
    for (i, b) in image.iter().enumerate() {
        if i % 16 == 0 {
            out.push_str("\n    ");
        }
        let _ = write!(out, "0x{b:02x}, ");
    }
    out.push_str("\n];\n");
    out.push_str("/// Public key the bundle was signed with — derived from the kernel's\n");
    out.push_str("/// driver-signing seed, so it equals the kernel's embedded trust\n");
    out.push_str("/// anchor `KERNEL_DRIVER_SIGNER_PUBKEY` by construction.\n");
    out.push_str("pub const VIRTIO_KBD_SIGNER_PUBKEY: [u8; 32] = [");
    for (i, b) in pubkey.iter().enumerate() {
        if i % 8 == 0 {
            out.push_str("\n    ");
        }
        let _ = write!(out, "0x{b:02x}, ");
    }
    out.push_str("\n];\n");

    let path = PathBuf::from(out_dir).join("bundle.rs");
    fs::write(&path, out).expect("write bundle.rs");
}
