//! Build-time generator for the autoload-root image fixture
//! (`plans/PI.md` P10 5d-2-ii(b-2-iii); `plans/DISPLAY.md` D7d).
//!
//! Unlike the per-vertical build scripts, this one is **host-only and target-
//! independent**: the fixture's bytes are produced on the build host (the
//! `tools/xtask` harness plants them on the test's virtio-blk backing), so the
//! signed driver bundles must always be built, whatever the outer target. For
//! each planted driver — the pure-Rust virtio-input keyboard driver
//! (`drivers/input/virtio_kbd`) and the framebuffer display service
//! (`drivers/display/framebuffer`) — it:
//!
//! 1. Cross-compiles the driver's `Run` binary **position-independent** for
//!    the freestanding `aarch64-unknown-none` target — the architecture the
//!    `-M virt` vertical boots — using the driver's own `Run.ld` (the
//!    production PIE layout the bundle's payload links with), into a private
//!    target directory under `OUT_DIR`.
//! 2. Converts the linked PIE ELF to an `rxe` blob
//!    ([`rustos_itest_harness::elf2rxe::elf_to_rxe`]), baking relocations for the
//!    shared [`rustos_itest_harness::USER_IMAGE_BIAS`] the production spawn
//!    producer maps every child image at and stamping the kernel's compiled-in
//!    syscall CFI tag (`rustos_kernel_syscall::SYSCALL_TABLE_HASH`) so
//!    [`rustos_abi::rxe::LoadImage::parse`] accepts it.
//! 3. Wraps that `rxe` as the payload of a signed `kind = UserSpace`
//!    `DriverManifest` ([`rustos_itest_harness::driver_image`]) — requesting
//!    exactly the capabilities the driver needs, carrying the driver crate's
//!    own `BIND_KEYS`, and **signed with the kernel's own driver-signing
//!    seed** (`build_support::KERNEL_DRIVER_SIGNING_SEED`, the single source
//!    the kernel build derives its embedded trust anchor from) so the bundle
//!    verifies against `KERNEL_DRIVER_SIGNER_PUBKEY` at boot.
//! 4. Emits the bundle bytes — and the one shared signer public key — as a
//!    Rust source the library `include!`s.
//!
//! Re-running `build.rs` produces byte-identical output, so the fixture is
//! deterministic.

use std::env;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use rustos_abi::{CapabilityId, DriverBindKey, DriverKind};
use rustos_itest_harness::dep_info::emit_dep_info_reruns;
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

/// One driver to build, sign, and embed.
struct PlantedDriver {
    /// Cargo package name (also the `Run` binary's name).
    package: &'static str,
    /// Crate directory relative to the workspace root.
    crate_rel: &'static str,
    /// The exact capability set the signed manifest requests.
    caps: &'static [CapabilityId],
    /// The driver crate's own canonical bind table.
    bind_keys: &'static [DriverBindKey],
    /// Name of the emitted `pub const` holding the bundle bytes.
    const_name: &'static str,
    /// One-line description for the emitted constant's rustdoc.
    describe: &'static str,
}

/// The drivers the fixture plants — each signed from its crate's own
/// canonical `BIND_KEYS`, so the on-disk bundle and the driver never drift.
const PLANTED: &[PlantedDriver] = &[
    PlantedDriver {
        package: "rustos-drv-input-virtio-kbd",
        crate_rel: "drivers/input/virtio_kbd",
        caps: &[
            CapabilityId::MMIO_MAP,
            CapabilityId::MEM_DMA,
            CapabilityId::IRQ_BIND,
            CapabilityId::INPUT_INJECT,
        ],
        bind_keys: rustos_drv_input_virtio_input::BIND_KEYS,
        const_name: "VIRTIO_KBD_BUNDLE",
        describe: "virtio-input keyboard driver",
    },
    PlantedDriver {
        package: "rustos-drv-display-framebuffer",
        crate_rel: "drivers/display/framebuffer",
        caps: &[
            CapabilityId::MMIO_MAP,
            CapabilityId::SHM,
            CapabilityId::IPC_BIND_PRIVILEGED,
        ],
        bind_keys: rustos_drv_display_framebuffer::BIND_KEYS,
        const_name: "FRAMEBUFFER_BUNDLE",
        describe: "framebuffer display service",
    },
];

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    let manifest_dir = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let manifest_dir = manifest_dir.trim_end_matches('/');
    let out_dir = env::var("OUT_DIR").expect("OUT_DIR");

    let kernel_build_support =
        format!("{manifest_dir}/../../../kernel/rustos-kernel/src/build_support.rs");
    println!("cargo:rerun-if-changed={kernel_build_support}");

    let mut out = String::new();
    out.push_str("// Auto-generated by build.rs. DO NOT EDIT.\n");
    let mut signer_pubkey: Option<[u8; 32]> = None;
    for driver in PLANTED {
        let driver_dir = format!("{manifest_dir}/../../../{}", driver.crate_rel);
        // The driver's Rust sources — its own and every transitive `lib/*`
        // dependency — are registered from the inner build's dep-info after
        // the compile below; only the inputs rustc does not record there
        // (the linker script and the manifest) are named by hand.
        println!("cargo:rerun-if-changed={driver_dir}/Run.ld");
        println!("cargo:rerun-if-changed={driver_dir}/Cargo.toml");

        let rxe = build_and_convert_driver(manifest_dir, &out_dir, &driver_dir, driver.package);
        let signed = build_signed_driver_image(
            &build_support::KERNEL_DRIVER_SIGNING_SEED,
            DriverKind::UserSpace,
            driver.caps,
            driver.bind_keys,
            rustos_kernel_syscall::SYSCALL_TABLE_HASH,
            &rxe,
        );
        // Every bundle is signed with the one kernel seed, so the derived
        // public key must be identical across drivers; assert rather than
        // silently emit a fixture whose tests would compare the wrong key.
        assert!(
            signer_pubkey.is_none() || signer_pubkey == Some(signed.signer_pubkey),
            "one signing seed must derive one signer public key"
        );
        signer_pubkey = Some(signed.signer_pubkey);
        write_bundle_const(&mut out, driver, &signed.image);
    }
    write_signer_const(
        &mut out,
        &signer_pubkey.expect("at least one driver is planted"),
    );

    let path = PathBuf::from(&out_dir).join("bundle.rs");
    fs::write(&path, out).expect("write bundle.rs");
}

/// Compile `package` (in `driver_dir`) PIE for the freestanding aarch64
/// target and convert the linked ELF into an `rxe` blob relocated for
/// [`USER_IMAGE_BIAS`].
fn build_and_convert_driver(
    manifest_dir: &str,
    out_dir: &str,
    driver_dir: &str,
    package: &str,
) -> Vec<u8> {
    let run_ld = format!("{driver_dir}/Run.ld");
    let target_dir = format!("{out_dir}/{package}-target");

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
        // The inner build produces the production bytes the fixture embeds;
        // it must compile with plain rustc whatever tool drove the outer
        // build (a `cargo clippy` outer run exports its lint driver through
        // these wrappers, which would fail or skew the byte-producing
        // compile).
        .env_remove("RUSTC_WRAPPER")
        .env_remove("RUSTC_WORKSPACE_WRAPPER")
        .env(
            "CARGO_TARGET_AARCH64_UNKNOWN_NONE_RUSTFLAGS",
            format!("-C relocation-model=pie -C link-arg=-pie -C link-arg=-T{run_ld}"),
        )
        .args([
            "build",
            "-p",
            package,
            "--target",
            AARCH64_TARGET,
            "-Z",
            "build-std=core,compiler_builtins,alloc",
            "--target-dir",
            &target_dir,
        ])
        .status()
        .unwrap_or_else(|e| panic!("spawn cargo to build {package}: {e}"));
    assert!(status.success(), "building {package} failed");

    let elf_path = format!("{target_dir}/{AARCH64_TARGET}/debug/{package}");
    let elf = fs::read(&elf_path).unwrap_or_else(|e| panic!("read {elf_path}: {e}"));

    // Register every source the inner build consumed — the driver's own
    // files *and* its transitive workspace dependencies — so editing any of
    // them rebuilds this fixture. A hand-kept list misses a `lib/*` edit and
    // silently plants a stale driver bundle.
    let dep_info = PathBuf::from(format!("{elf_path}.d"));
    emit_dep_info_reruns(&dep_info, Path::new(manifest_dir));

    elf_to_rxe(
        &elf,
        &rustos_kernel_syscall::SYSCALL_TABLE_HASH,
        USER_IMAGE_BIAS,
    )
    .unwrap_or_else(|e| panic!("convert the {package} ELF into an rxe image: {e:?}"))
}

/// Append one signed bundle's bytes as a documented `pub const`.
fn write_bundle_const(out: &mut String, driver: &PlantedDriver, image: &[u8]) {
    out.push_str("/// Signed `kind = UserSpace` driver bundle whose payload is the\n");
    let _ = writeln!(
        out,
        "/// {} `rxe`, planted into the encrypted root's",
        driver.describe
    );
    out.push_str("/// `/System/Drivers/` store and admitted through the kernel's\n");
    out.push_str("/// signed autoload gate against `KERNEL_DRIVER_SIGNER_PUBKEY`.\n");
    let _ = write!(out, "pub const {}: &[u8] = &[", driver.const_name);
    for (i, b) in image.iter().enumerate() {
        if i % 16 == 0 {
            out.push_str("\n    ");
        }
        let _ = write!(out, "0x{b:02x}, ");
    }
    out.push_str("\n];\n");
}

/// Append the shared signer public key as a documented `pub const`.
fn write_signer_const(out: &mut String, pubkey: &[u8; 32]) {
    out.push_str("/// Public key every planted bundle was signed with — derived from the\n");
    out.push_str("/// kernel's driver-signing seed, so it equals the kernel's embedded\n");
    out.push_str("/// trust anchor `KERNEL_DRIVER_SIGNER_PUBKEY` by construction.\n");
    out.push_str("pub const DRIVER_SIGNER_PUBKEY: [u8; 32] = [");
    for (i, b) in pubkey.iter().enumerate() {
        if i % 8 == 0 {
            out.push_str("\n    ");
        }
        let _ = write!(out, "0x{b:02x}, ");
    }
    out.push_str("\n];\n");
}
