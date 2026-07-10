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
//! kernel derives its embedded trust anchor from), so the
//! booted kernel admits it through the signed load gate.
//!
//! The signed-bundle composer and the ELF->rxe converter are the shared
//! definitions the kernel `build.rs` and the autoload fixtures also use
//! (`rustos_itest_harness`), so the wire layout is never
//! re-rolled here. This is host-only build glue; the
//! production image stays Rust-only.

use rustos_abi::{CapabilityId, DriverKind, DriverManifest, DRIVER_MANIFEST_MAGIC};
use rustos_itest_harness::driver_image::build_signed_driver_image;
use rustos_itest_harness::elf2rxe::elf_to_rxe;
use rustos_itest_harness::USER_IMAGE_BIAS;

use super::pie_build::cross_compile_pie_elf;
use crate::Context;

/// The single source of the kernel's driver-signing seed:
/// the kernel build signs its embedded in-kernel manifests with it and derives
/// the `KERNEL_DRIVER_SIGNER_PUBKEY` trust anchor from it, so a bundle signed
/// here with the same seed is admitted by the booted kernel's load gate. The
/// `#[path]` include carries the build script's target-selection helpers too,
/// which this module does not use.
//
// `broken_intra_doc_links` is allowed because this is a foreign shared source
// file authored to live in the `rustos-kernel` crate (the single source of the
// seed); its own `//!`/item doc links resolve in that crate,
// not when it is re-included here as a submodule. Suppressing the check is
// scoped to this included file and silences none of this module's own docs.
#[allow(dead_code, rustdoc::broken_intra_doc_links)]
#[path = "../../../../kernel/rustos-kernel/src/build_support.rs"]
pub(crate) mod build_support;

/// Store path of the `VideoCore` mailbox service-driver bundle, **relative to
/// the `/System` volume root** (whose root *is* `/System`, design B). The
/// namespace is `Drivers/<class>[_<subtype>]/<leaf>/<driver>`: class
/// `bus`, subtype `mailbox`, the `vcmailbox` leaf naming the device, the
/// `Run` entry binary.
pub const VCMAILBOX_STORE_PATH: &[&[u8]] = &[b"Drivers", b"bus_mailbox", b"vcmailbox", b"Run"];

/// Store path of the BCM2711 PCIe root-complex bus-driver bundle: class `bus`,
/// subtype `pcie`, the chip leaf `bcm2711` (the
/// vendor/chip name appears only at the leaf, the class namespace above it
/// stays vendor-neutral).
pub const PCIE_BRCM_STORE_PATH: &[&[u8]] = &[b"Drivers", b"bus_pcie", b"bcm2711", b"Run"];

/// Store path of the VL805 USB host-controller bus-driver bundle: class `bus`,
/// subtype `usb`, the chip leaf `vl805`.
pub const VL805_STORE_PATH: &[&[u8]] = &[b"Drivers", b"bus_usb", b"vl805", b"Run"];

/// Store path of the xHCI USB host-controller driver (HCD) bundle: class
/// `bus`, subtype `usb`, the `xhci` leaf naming the (vendor-neutral) generic
/// host-controller class it drives.
pub const USB_XHCI_STORE_PATH: &[&[u8]] = &[b"Drivers", b"bus_usb", b"xhci", b"Run"];

/// Store path of the USB boot-keyboard class-driver bundle: class `input`, the
/// `usb_kbd` leaf naming the (vendor-neutral) driver.
pub const USB_KBD_STORE_PATH: &[&[u8]] = &[b"Drivers", b"input", b"usb_kbd", b"Run"];

/// Store path of the virtio-input keyboard/pointer driver bundle: class
/// `input`, the `virtio_kbd` leaf naming the (vendor-neutral) driver — the
/// same path the `-M virt` autoload vertical's fixture plants.
pub const VIRTIO_KBD_STORE_PATH: &[&[u8]] = &[b"Drivers", b"input", b"virtio_kbd", b"Run"];

/// Store path of the USB mass-storage class-driver bundle: class `storage`,
/// the `usb_msd` leaf naming the (vendor-neutral) driver.
pub const USB_MSD_STORE_PATH: &[&[u8]] = &[b"Drivers", b"storage", b"usb_msd", b"Run"];

/// Store path of the volume-manager policy-driver bundle: class `storage`,
/// the `volmgr` leaf naming the (vendor-neutral, bus-neutral) policy
/// driver that binds the per-LUN block-service nodes.
pub const VOLMGR_STORE_PATH: &[&[u8]] = &[b"Drivers", b"storage", b"volmgr", b"Run"];

/// Cross-compile `package` for the freestanding aarch64 target, convert the
/// linked PIE ELF to a production-biased `rxe`, and wrap it as the signed
/// payload of a `kind = UserSpace` bundle requesting exactly `caps` and
/// carrying the driver crate's own canonical `bind_keys`. The single composer every installed user-space
/// bundle shares, so the wire layout, the signing seed, and the fail-closed
/// sanity check live in one place.
///
/// # Errors
///
/// A string describing a failed cross-compile, a missing ELF artefact, an
/// ELF->rxe conversion failure, or a structurally invalid composed bundle.
fn build_bundle(
    ctx: &Context,
    package: &str,
    caps: &[CapabilityId],
    bind_keys: &[rustos_abi::DriverBindKey],
) -> Result<Vec<u8>, String> {
    // Map the package name to its source directory under `drivers/`. Only
    // the crates installed into the image are listed; an unknown package is
    // a programming error in the image pipeline, never a runtime input.
    let rel_dir = match package {
        "rustos-drv-bus-mailbox-vcmailbox" => "drivers/bus/mailbox/vcmailbox",
        "rustos-drv-bus-pcie-brcm" => "drivers/bus/pcie_brcm",
        "rustos-drv-bus-usb-vl805" => "drivers/bus/usb/vl805",
        "rustos-drv-bus-usb" => "drivers/bus/usb/xhci",
        "rustos-drv-input-usb-kbd" => "drivers/input/usb_kbd",
        "rustos-drv-input-virtio-kbd" => "drivers/input/virtio_kbd",
        "rustos-drv-storage-usb-msd" => "drivers/storage/usb_msd",
        "rustos-drv-storage-volmgr" => "drivers/storage/volmgr",
        other => return Err(format!("image: no source dir mapped for driver {other}")),
    };
    // A driver crate's `Run` binary shares the package name.
    let crate_dir = ctx.workspace_root.join(rel_dir);
    let elf = cross_compile_pie_elf(ctx, "image-drivers", package, package, &crate_dir)?;
    let rxe = elf_to_rxe(
        &elf,
        &rustos_kernel_syscall::SYSCALL_TABLE_HASH,
        USER_IMAGE_BIAS,
    )
    .map_err(|e| format!("image: convert {package} driver ELF to rxe: {e}"))?;

    let signed = build_signed_driver_image(
        &build_support::KERNEL_DRIVER_SIGNING_SEED,
        DriverKind::UserSpace,
        caps,
        bind_keys,
        rustos_kernel_syscall::SYSCALL_TABLE_HASH,
        &rxe,
    );
    verify_signed_bundle(&signed.image)?;
    Ok(signed.image)
}

/// Build and sign the user-space `VideoCore` mailbox service-driver bundle for
/// installation into the image's `/System/Drivers/` store.
///
/// Returns the signed `.rxe` bundle bytes exactly as the store scan
/// reads them back. The driver requests only the capabilities it needs — a
/// mapped doorbell window (`CAP_MMIO_MAP`), a coherent DMA property buffer
/// (`CAP_MEM_DMA`), and the privilege to create the restricted-sender mailbox
/// endpoint (`CAP_IPC_BIND_PRIVILEGED`) — and carries the driver crate's own
/// canonical bind table, so the autoload match data never drifts from
/// the driver.
///
/// # Errors
///
/// A string describing a failed cross-compile, a missing ELF artefact, or an
/// ELF->rxe conversion failure.
pub fn build_vcmailbox_bundle(ctx: &Context) -> Result<Vec<u8>, String> {
    build_bundle(
        ctx,
        "rustos-drv-bus-mailbox-vcmailbox",
        &[
            CapabilityId::MMIO_MAP,
            CapabilityId::MEM_DMA,
            CapabilityId::IPC_BIND_PRIVILEGED,
        ],
        rustos_vcmailbox::BIND_KEYS,
    )
}

/// Build and sign the BCM2711 PCIe root-complex bus-driver bundle.
///
/// It maps its discovered register window (`CAP_MMIO_MAP`), trains the link,
/// assigns the VL805 BAR, allocates the controller's MSI vector so the matched
/// xHCI driver parks on its completion interrupt rather than busy-polling
/// (`CAP_IRQ_BIND`, which the `msi_alloc` trap is gated on), and publishes the
/// enumerated USB host function into the live hardware tree (`CAP_HW_EMIT`) —
/// and nothing more (no ambient authority). Carries
/// `rustos_drv_bus_pcie_brcm::BIND_KEYS`, so it autoloads against the
/// discovered `brcm,bcm2711-pcie` node.
///
/// # Errors
///
/// As [`build_vcmailbox_bundle`].
pub fn build_pcie_brcm_bundle(ctx: &Context) -> Result<Vec<u8>, String> {
    build_bundle(
        ctx,
        "rustos-drv-bus-pcie-brcm",
        &[
            CapabilityId::MMIO_MAP,
            CapabilityId::IRQ_BIND,
            CapabilityId::HW_EMIT,
        ],
        rustos_drv_bus_pcie_brcm::BIND_KEYS,
    )
}

/// Build and sign the VL805 USB host-controller bus-driver bundle.
///
/// It reloads the controller's firmware over the `vcmailbox` property mailbox
/// (`CAP_MAILBOX`) and then publishes the controller as an xHCI node
/// forwarding the BAR + DMA grants it received (`CAP_HW_EMIT`) — and nothing
/// more. It holds neither `CAP_MMIO_MAP` nor `CAP_MEM_DMA`: it forwards the
/// grants without mapping them (least privilege). Carries
/// `rustos_drv_bus_usb_vl805::BIND_KEYS`, so it autoloads against the VL805
/// PCI node the PCIe driver emitted.
///
/// # Errors
///
/// As [`build_vcmailbox_bundle`].
pub fn build_vl805_bundle(ctx: &Context) -> Result<Vec<u8>, String> {
    build_bundle(
        ctx,
        "rustos-drv-bus-usb-vl805",
        &[CapabilityId::MAILBOX, CapabilityId::HW_EMIT],
        rustos_drv_bus_usb_vl805::BIND_KEYS,
    )
}

/// Build and sign the xHCI USB host-controller driver (HCD) bundle.
///
/// It maps the controller's register BAR (`CAP_MMIO_MAP`), carves its DMA
/// working set (`CAP_MEM_DMA`), binds the completion interrupt
/// (`CAP_IRQ_BIND`), creates the shared URB data buffer (`CAP_SHM`), binds the
/// restricted-sender URB transport endpoint (`CAP_IPC_BIND_PRIVILEGED`),
/// publishes the per-interface node (`CAP_HW_EMIT`), and emits a one-shot
/// bring-up diagnostic (`CAP_LOG_EMIT`) — and nothing more. Carries
/// `rustos_drv_bus_usb::BIND_KEYS`, so it autoloads against the `usb,xhci`
/// node the VL805 driver emitted.
///
/// # Errors
///
/// As [`build_vcmailbox_bundle`].
pub fn build_xhci_bundle(ctx: &Context) -> Result<Vec<u8>, String> {
    build_bundle(
        ctx,
        "rustos-drv-bus-usb",
        &[
            CapabilityId::MMIO_MAP,
            CapabilityId::MEM_DMA,
            CapabilityId::IRQ_BIND,
            CapabilityId::SHM,
            CapabilityId::IPC_BIND_PRIVILEGED,
            CapabilityId::HW_EMIT,
            CapabilityId::LOG_EMIT,
        ],
        rustos_drv_bus_usb::BIND_KEYS,
    )
}

/// Build and sign the USB boot-keyboard **class**-driver bundle.
///
/// A pure HID class driver: it injects decoded key edges into the kernel
/// input-focus arbiter (`CAP_INPUT_INJECT`), maps the shared URB buffer its
/// host-controller driver forwarded (`CAP_SHM`), submits URBs on its one
/// interface's transport endpoint (`CAP_IPC_ENDPOINT`), and emits a one-shot
/// beacon (`CAP_LOG_EMIT`) — and nothing more. It holds **no** MMIO, DMA, or
/// IRQ authority. Carries `rustos_drv_input_usb_kbd::BIND_KEYS`, so it
/// autoloads against the HID boot-keyboard interface node the HCD emitted.
///
/// # Errors
///
/// As [`build_vcmailbox_bundle`].
pub fn build_usb_kbd_bundle(ctx: &Context) -> Result<Vec<u8>, String> {
    build_bundle(
        ctx,
        "rustos-drv-input-usb-kbd",
        &[
            CapabilityId::INPUT_INJECT,
            CapabilityId::SHM,
            CapabilityId::IPC_ENDPOINT,
            CapabilityId::LOG_EMIT,
        ],
        rustos_drv_input_usb_kbd::BIND_KEYS,
    )
}

/// Build and sign the USB mass-storage **class**-driver bundle.
///
/// A pure BOT/SCSI class driver: it maps the shared URB buffer its
/// host-controller driver forwarded and creates the per-LUN data windows
/// (`CAP_SHM`), submits URBs on its one interface's transport endpoint
/// (`CAP_IPC_ENDPOINT`), binds the per-LUN block-service endpoints it
/// serves (`CAP_IPC_BIND_PRIVILEGED`), publishes/retracts the per-LUN
/// storage nodes (`CAP_HW_EMIT`), and emits diagnostics (`CAP_LOG_EMIT`) —
/// and nothing more. It holds **no** MMIO, DMA, or IRQ authority. Carries
/// `rustos_drv_storage_usb_msd::BIND_KEYS`, so it autoloads against the
/// mass-storage interface node the HCD emitted (`plans/DEVICES.md` D2).
///
/// # Errors
///
/// As [`build_vcmailbox_bundle`].
pub fn build_usb_msd_bundle(ctx: &Context) -> Result<Vec<u8>, String> {
    build_bundle(
        ctx,
        "rustos-drv-storage-usb-msd",
        &[
            CapabilityId::SHM,
            CapabilityId::IPC_ENDPOINT,
            CapabilityId::IPC_BIND_PRIVILEGED,
            CapabilityId::HW_EMIT,
            CapabilityId::LOG_EMIT,
        ],
        rustos_drv_storage_usb_msd::BIND_KEYS,
    )
}

/// Build and sign the volume-manager **policy**-driver bundle.
///
/// A pure policy driver: it maps the shared data window its block driver
/// forwarded (`CAP_SHM`), issues blkio calls on its one granted
/// block-service endpoint (`CAP_IPC_ENDPOINT`), requests the audited
/// kernel attach of each recognised volume (`CAP_FS_MOUNT`), and emits
/// diagnostics (`CAP_LOG_EMIT`) — and nothing more. It holds **no** MMIO,
/// DMA, IRQ, or node-emission authority. Carries
/// `rustos_drv_storage_volmgr::BIND_KEYS`, so it autoloads against the
/// per-LUN block-service storage node the mass-storage class driver
/// emitted (`plans/DEVICES.md` D3c).
///
/// # Errors
///
/// As [`build_vcmailbox_bundle`].
pub fn build_volmgr_bundle(ctx: &Context) -> Result<Vec<u8>, String> {
    build_bundle(
        ctx,
        "rustos-drv-storage-volmgr",
        &[
            CapabilityId::SHM,
            CapabilityId::IPC_ENDPOINT,
            CapabilityId::FS_MOUNT,
            CapabilityId::LOG_EMIT,
        ],
        rustos_drv_storage_volmgr::BIND_KEYS,
    )
}

/// Build and sign the virtio-input keyboard/pointer driver bundle.
///
/// The QEMU `virt` sibling of the USB keyboard: it maps its granted
/// register window (`CAP_MMIO_MAP`), carves its virtqueue DMA slab
/// (`CAP_MEM_DMA`), parks on the device's interrupt line (`CAP_IRQ_BIND`),
/// and injects decoded key edges into the kernel input-focus arbiter
/// (`CAP_INPUT_INJECT`) — and nothing more. Carries
/// `rustos_drv_input_virtio_input::BIND_KEYS`, so it autoloads against a
/// discovered virtio-input node (and stays unbound on the Pi, whose tree
/// carries none — §18.4).
///
/// # Errors
///
/// As [`build_vcmailbox_bundle`].
pub fn build_virtio_kbd_bundle(ctx: &Context) -> Result<Vec<u8>, String> {
    build_bundle(
        ctx,
        "rustos-drv-input-virtio-kbd",
        &[
            CapabilityId::MMIO_MAP,
            CapabilityId::MEM_DMA,
            CapabilityId::IRQ_BIND,
            CapabilityId::INPUT_INJECT,
        ],
        rustos_drv_input_virtio_input::BIND_KEYS,
    )
}

/// Fail-closed sanity check on a freshly composed bundle before it is planted
/// into the image (never ship a malformed store entry).
///
/// It re-decodes the bundle through the same `rustos_abi` definition the
/// kernel's store scan and load gate use, asserting it is a well-formed,
/// signed `kind = UserSpace` manifest carrying a non-empty payload — so a
/// broken cross-compile/sign step fails the image build loudly instead of
/// emitting an image whose driver the kernel would reject at boot. The
/// signature *verifies* against the kernel's embedded anchor by construction
/// (it is signed with the same `KERNEL_DRIVER_SIGNING_SEED` the kernel
/// derives that anchor from); the end-to-end signed-gate→spawn path is proven
/// by the `-M virt` autoload vertical.
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
