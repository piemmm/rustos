//! Cross-compile and sign the autoloaded `/System/Drivers/` bundles the
//! flashable Raspberry Pi image ships (`plans/PI.md` P10 D4).
//!
//! `tools/mkimage` is a pure library: it plants bundle *bytes* into the
//! read-only `/System` store but never drives `cargo`. This module is the
//! orchestration half — it builds each user-space driver crate
//! position-independent for the freestanding `aarch64-unknown-none` target,
//! converts the linked PIE ELF to an `rxe` image (relocated for the
//! production user-image bias and stamped with the kernel's compiled-in
//! syscall CFI tag), and wraps that `rxe` as the signed payload of a
//! `kind = UserSpace` [`DriverManifest`]. The bundle is signed with the
//! kernel's own driver-signing seed
//! ([`build_support::KERNEL_DRIVER_SIGNING_SEED`], the single source the
//! kernel derives its embedded trust anchor from, `AGENTS.md` §2.2), so the
//! booted kernel admits it through the §8 / §18.6 signed load gate.
//!
//! The signed-bundle composer and the ELF->rxe converter are the shared
//! definitions the kernel `build.rs` and the autoload fixtures also use
//! (`rustos_itest_harness`, `AGENTS.md` §2.2), so the wire layout is never
//! re-rolled here. This is host-only build glue (`AGENTS.md` §1 / §12); the
//! production image stays Rust-only.

use std::process::Command;

use rustos_abi::{CapabilityId, DriverKind, DriverManifest, DRIVER_MANIFEST_MAGIC};
use rustos_itest_harness::driver_image::build_signed_driver_image;
use rustos_itest_harness::elf2rxe::elf_to_rxe;
use rustos_itest_harness::USER_IMAGE_BIAS;

use crate::Context;

/// The single source of the kernel's driver-signing seed (`AGENTS.md` §2.2):
/// the kernel build signs its embedded in-kernel manifests with it and derives
/// the `KERNEL_DRIVER_SIGNER_PUBKEY` trust anchor from it, so a bundle signed
/// here with the same seed is admitted by the booted kernel's load gate. The
/// `#[path]` include carries the build script's target-selection helpers too,
/// which this module does not use.
//
// `broken_intra_doc_links` is allowed because this is a foreign shared source
// file authored to live in the `rustos-kernel` crate (the single source of the
// seed, `AGENTS.md` §2.2); its own `//!`/item doc links resolve in that crate,
// not when it is re-included here as a submodule. Suppressing the check is
// scoped to this included file and silences none of this module's own docs.
#[allow(dead_code, rustdoc::broken_intra_doc_links)]
#[path = "../../../../kernel/rustos-kernel/src/build_support.rs"]
mod build_support;

/// Rust target triple of the freestanding aarch64 driver build — the
/// architecture the Pi image boots.
const AARCH64_TARGET: &str = "aarch64-unknown-none";

/// Store path of the `VideoCore` mailbox service-driver bundle, **relative to
/// the `/System` volume root** (whose root *is* `/System`, design B). The
/// §16.2 namespace is `Drivers/<class>[_<subtype>]/<leaf>/<driver>`: class
/// `bus`, subtype `mailbox`, the `vcmailbox` leaf naming the device, the
/// `Run` entry binary (`AGENTS.md` §8 / §16.2).
pub const VCMAILBOX_STORE_PATH: &[&[u8]] = &[b"Drivers", b"bus_mailbox", b"vcmailbox", b"Run"];

/// Build and sign the user-space `VideoCore` mailbox service-driver bundle for
/// installation into the image's `/System/Drivers/` store.
///
/// Returns the signed `.rxe` bundle bytes exactly as the §18.6 store scan
/// reads them back. The driver requests only the capabilities it needs — a
/// mapped doorbell window (`CAP_MMIO_MAP`), a coherent DMA property buffer
/// (`CAP_MEM_DMA`), and the privilege to create the restricted-sender mailbox
/// endpoint (`CAP_IPC_BIND_PRIVILEGED`) — and carries the driver crate's own
/// canonical §18.3 bind table, so the autoload match data never drifts from
/// the driver (`AGENTS.md` §2.2 / §18.3).
///
/// # Errors
///
/// A string describing a failed cross-compile, a missing ELF artefact, or an
/// ELF->rxe conversion failure.
pub fn build_vcmailbox_bundle(ctx: &Context) -> Result<Vec<u8>, String> {
    let elf = cross_compile_driver(ctx, "rustos-drv-bus-mailbox-vcmailbox")?;
    let rxe = elf_to_rxe(
        &elf,
        &rustos_kernel_syscall::SYSCALL_TABLE_HASH,
        USER_IMAGE_BIAS,
    )
    .map_err(|e| format!("image: convert vcmailbox driver ELF to rxe: {e}"))?;

    let signed = build_signed_driver_image(
        &build_support::KERNEL_DRIVER_SIGNING_SEED,
        DriverKind::UserSpace,
        &[
            CapabilityId::MMIO_MAP,
            CapabilityId::MEM_DMA,
            CapabilityId::IPC_BIND_PRIVILEGED,
        ],
        rustos_vcmailbox::BIND_KEYS,
        rustos_kernel_syscall::SYSCALL_TABLE_HASH,
        &rxe,
    );
    verify_signed_bundle(&signed.image)?;
    Ok(signed.image)
}

/// Fail-closed sanity check on a freshly composed bundle before it is planted
/// into the image (`AGENTS.md` §2.9 — never ship a malformed store entry).
///
/// It re-decodes the bundle through the same `rustos_abi` definition the
/// kernel's store scan and load gate use, asserting it is a well-formed,
/// signed `kind = UserSpace` manifest carrying a non-empty payload — so a
/// broken cross-compile/sign step fails the image build loudly instead of
/// emitting an image whose driver the kernel would reject at boot. The
/// signature *verifies* against the kernel's embedded anchor by construction
/// (it is signed with the same `KERNEL_DRIVER_SIGNING_SEED` the kernel
/// derives that anchor from); the end-to-end signed-gate→spawn path is proven
/// by the `-M virt` autoload vertical (`AGENTS.md` §2.2).
///
/// # Errors
///
/// A string describing the structural defect found.
fn verify_signed_bundle(image: &[u8]) -> Result<(), String> {
    if image.len() <= DriverManifest::WIRE_LEN {
        return Err("image: composed driver bundle carries no payload".to_string());
    }
    let manifest = DriverManifest::from_bytes(image)
        .map_err(|e| format!("image: composed driver bundle's manifest does not decode: {e:?}"))?;
    if manifest.magic != DRIVER_MANIFEST_MAGIC {
        return Err("image: composed driver bundle has the wrong manifest magic".to_string());
    }
    if manifest.kind != DriverKind::UserSpace {
        return Err("image: composed driver bundle is not kind = UserSpace".to_string());
    }
    if manifest.signature == [0u8; 64] {
        return Err("image: composed driver bundle is unsigned".to_string());
    }
    Ok(())
}

/// Compile a user-space driver crate's `Run` binary position-independent for
/// the freestanding aarch64 target (against the crate's own `Run.ld`) and
/// return the linked ELF bytes.
///
/// Mirrors the kernel `build.rs` / autoload-fixture recipe (`AGENTS.md`
/// §2.2): `core`/`alloc`/`compiler_builtins` are built PIC alongside the
/// crate (`-Z build-std`); the outer build's `RUSTFLAGS` are cleared so the
/// target-scoped PIE link recipe wins and applies only to the aarch64 driver
/// crates, never the driver's own host build script.
fn cross_compile_driver(ctx: &Context, package: &str) -> Result<Vec<u8>, String> {
    // Map the package name to its source directory under `drivers/`. Only the
    // crates installed into the image are listed; an unknown package is a
    // programming error in the image pipeline, never a runtime input.
    let rel_dir = match package {
        "rustos-drv-bus-mailbox-vcmailbox" => "drivers/bus/mailbox/vcmailbox",
        other => return Err(format!("image: no source dir mapped for driver {other}")),
    };
    let driver_dir = ctx.workspace_root.join(rel_dir);
    let run_ld = driver_dir.join("Run.ld");
    let target_dir = ctx.target_dir().join("image-drivers").join(package);

    // Cargo fingerprints the RUSTFLAGS *string* (which names the linker script
    // by path) but not the script's *content*, so a `Run.ld` edit alone would
    // not trigger a relink and the converter could read a stale ELF. Wipe the
    // private target directory to force a clean rebuild against the current
    // script (this is image authoring, not an incremental dev build).
    let _ = std::fs::remove_dir_all(&target_dir);

    let mut cmd = Command::new(&ctx.cargo);
    cmd.current_dir(&ctx.workspace_root)
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .env_remove("RUSTFLAGS")
        .env(
            "CARGO_TARGET_AARCH64_UNKNOWN_NONE_RUSTFLAGS",
            format!(
                "-C relocation-model=pie -C link-arg=-pie -C link-arg=-T{}",
                run_ld.display()
            ),
        )
        .args([
            "build",
            "--locked",
            "-p",
            package,
            "--target",
            AARCH64_TARGET,
            "-Z",
            "build-std=core,compiler_builtins,alloc",
            "--target-dir",
        ])
        .arg(&target_dir);
    ctx.run(
        &format!("image: driver build ({package}, {AARCH64_TARGET})"),
        cmd,
    )?;

    let elf_path = target_dir.join(AARCH64_TARGET).join("debug").join(package);
    std::fs::read(&elf_path)
        .map_err(|e| format!("image: cannot read driver ELF {}: {e}", elf_path.display()))
}
