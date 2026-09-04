//! Build-time fixture generator for the `PLAN.md` Stage 4.HW aarch64
//! driver-spawn vertical.
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
//! 2. Compile the pure-Rust driver-stub fixture program
//!    (`tests/integration/driver_register_program`) **position-independent**
//!    for the freestanding aarch64 target (the shared `program.ld` roots
//!    `tairix-rt`'s `_start`), into a private target directory under
//!    `OUT_DIR`.
//! 3. Convert the linked PIE ELF to an `rxe` blob, baking relocations for the
//!    [`tairix_itest_harness::USER_IMAGE_BIAS`] the production spawn producer
//!    maps every child image at and stamping the kernel's compiled-in syscall
//!    CFI tag (`tairix_kernel_syscall::SYSCALL_TABLE_HASH`) so
//!    [`tairix_abi::rxe::LoadImage::parse`] accepts it; emit the
//!    bytes and the bias as a Rust source the test `include!`s.
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
    let image_path = PathBuf::from(&out_dir).join("driver_image.rs");

    let target = env::var("TARGET").unwrap_or_default();
    if target == ARCH.target_triple() {
        // The test kernel itself links with the aarch64 `virt` script the
        // architecture port owns (the single per-board script).
        let linker = format!("{manifest_dir}/../../../kernel/arch/aarch64/link/aarch64-virt.ld");
        println!("cargo:rerun-if-changed={linker}");
        println!("cargo:rustc-link-arg=-T{linker}");

        // One CPU: this is the single-core driver-spawn handshake slice.
        let out_dir_os = std::ffi::OsString::from(&out_dir);
        let dtb = tairix_itest_harness::dump_aarch64_virt_dtb(&out_dir_os, 1);
        write_dtb_fixture(&dtb_path, &dtb);

        let rxe = tairix_itest_harness::program_fixture::GuestBuild {
            manifest_dir,
            out_dir: &out_dir,
            arch: ARCH,
            package: "tairix-test-driver-register-program",
            variant: None,
            env: &[],
        }
        .program_rxe(&tairix_kernel_syscall::SYSCALL_TABLE_HASH);
        write_bias_fixture(&rxe_path);
        write_driver_image_fixture(&image_path, &rxe);
    } else {
        // Inert stubs for host / other targets; the kernel body that uses these
        // compiles only for the freestanding aarch64 target.
        write_dtb_fixture(&dtb_path, &[]);
        write_bias_fixture(&rxe_path);
        write_driver_image_fixture(&image_path, &[]);
    }
}

/// Deterministic Ed25519 seed the build signs the driver image with.
const DRIVER_SIGNING_SEED: [u8; 32] = *b"tairix-driver-spawn-vertical/v1!";

/// Wrap `rxe` as the payload of a signed `DriverManifest` image the
/// `SpawnDriverLoader` gate admits, and emit it plus the signer public key.
///
/// The manifest is `kind = UserSpace` (the gate then spawns the payload as a
/// process), stamps the kernel's compiled-in `SYSCALL_TABLE_HASH`, and
/// requests the driver-class capabilities the spawned stub needs to send its
/// reply (`MEM_DMA`, the reply port's send gate, plus `IRQ_BIND`). The
/// signature covers `header[..signed_end] || cap_body || bind_table ||
/// payload`, exactly what `drvhost::Host::verify_signature` reconstructs
/// (the payload program is authenticated). The bind
/// table is empty: the device manager matches against the candidate bind
/// keys the kernel constructs, and a malformed/forged image still fails the
/// gate (the bind-table decode path is covered by `drvhost`'s
/// `devmgr_autoload` test).
fn write_driver_image_fixture(path: &std::path::Path, rxe: &[u8]) {
    use tairix_abi::{CapabilityId, DriverKind};
    use tairix_itest_harness::driver_image::build_signed_driver_image;

    let (image, pubkey): (Vec<u8>, [u8; 32]) = if rxe.is_empty() {
        // Host / non-aarch64 build: inert stub (the kernel body is
        // aarch64-only and never reads these on host).
        (Vec::new(), [0u8; 32])
    } else {
        // The shared composer assembles + signs the bundle from the one
        // manifest wire definition: a `kind = UserSpace`
        // image requesting the driver-class caps the spawned stub needs to
        // reply, with an empty bind table (the device manager matches against
        // the candidate bind keys the kernel constructs), signed so the
        // signature covers the payload.
        let signed = build_signed_driver_image(
            &DRIVER_SIGNING_SEED,
            DriverKind::UserSpace,
            &[CapabilityId::MEM_DMA, CapabilityId::IRQ_BIND],
            &[],
            tairix_kernel_syscall::SYSCALL_TABLE_HASH,
            rxe,
        );
        (signed.image, signed.signer_pubkey)
    };

    let mut out = String::new();
    out.push_str("// Auto-generated by build.rs. DO NOT EDIT.\n");
    out.push_str("/// Signed `kind = UserSpace` driver-manifest image whose payload is the\n");
    out.push_str("/// stub program `rxe`; admitted through the `SpawnDriverLoader` gate.\n");
    out.push_str("pub const DRIVER_IMAGE: &[u8] = &[");
    for (i, b) in image.iter().enumerate() {
        if i % 16 == 0 {
            out.push_str("\n    ");
        }
        let _ = write!(out, "0x{b:02x}, ");
    }
    out.push_str("\n];\n");
    out.push_str("/// Public key the build signed `DRIVER_IMAGE` with — the test's sole\n");
    out.push_str("/// `SpawnDriverLoader` trust anchor.\n");
    out.push_str("pub const DRIVER_SIGNER_PUBKEY: [u8; 32] = [");
    for (i, b) in pubkey.iter().enumerate() {
        if i % 8 == 0 {
            out.push_str("\n    ");
        }
        let _ = write!(out, "0x{b:02x}, ");
    }
    out.push_str("\n];\n");
    tairix_itest_harness::program_fixture::write_fixture(path, &out);
}

/// Emit `USER_BIAS` as a Rust source the test includes.
///
/// The converted stub `rxe` bytes are not emitted on their own: they are
/// embedded as the *payload* of the signed `DRIVER_IMAGE`
/// ([`write_driver_image_fixture`]), which is what the `SpawnDriverLoader`
/// gate admits and spawns. Emitting a second loose copy would be unused.
fn write_bias_fixture(path: &std::path::Path) {
    let out = tairix_itest_harness::program_fixture::fixture_header();
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
