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

use std::sync::OnceLock;

use rustos_abi::{CapabilityId, DriverKind, DriverManifest, DRIVER_MANIFEST_MAGIC};
use rustos_itest_harness::driver_image::build_signed_driver_image;
use rustos_itest_harness::elf2rxe::elf_to_rxe;
use rustos_itest_harness::pie::PieArch;
use rustos_itest_harness::USER_IMAGE_BIAS;

use super::image_apps::AppStoreFile;
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

/// Store path of the USB boot-mouse class-driver bundle: class `input`, the
/// `usb_mouse` leaf naming the (vendor-neutral) driver.
pub const USB_MOUSE_STORE_PATH: &[&[u8]] = &[b"Drivers", b"input", b"usb_mouse", b"Run"];

/// Store path of the virtio-input keyboard/pointer driver bundle: class
/// `input`, the `virtio_kbd` leaf naming the (vendor-neutral) driver — the
/// same path the `-M virt` autoload vertical's fixture plants.
pub const VIRTIO_KBD_STORE_PATH: &[&[u8]] = &[b"Drivers", b"input", b"virtio_kbd", b"Run"];

/// Store path of the framebuffer display-service bundle: class `display`,
/// the `framebuffer` leaf naming the (vendor-neutral) service that drives
/// any platform-published linear scan-out surface.
pub const FRAMEBUFFER_STORE_PATH: &[&[u8]] = &[b"Drivers", b"display", b"framebuffer", b"Run"];

/// Store path of the virtio-net link-layer driver bundle: class `network`,
/// the `virtio_net` leaf naming the (vendor-neutral) driver — the path the
/// `-M virt` two-process netstack autoload vertical's disk plants it at.
pub const VIRTIO_NET_STORE_PATH: &[&[u8]] = &[b"Drivers", b"network", b"virtio_net", b"Run"];

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
    arch: PieArch,
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
        "rustos-drv-input-usb-mouse" => "drivers/input/usb_mouse",
        "rustos-drv-input-virtio-kbd" => "drivers/input/virtio_kbd",
        "rustos-drv-network-virtio-net-driver" => "drivers/network/virtio_net_driver",
        "rustos-drv-display-framebuffer" => "drivers/display/framebuffer",
        "rustos-drv-storage-usb-msd" => "drivers/storage/usb_msd",
        "rustos-drv-storage-volmgr" => "drivers/storage/volmgr",
        other => return Err(format!("image: no source dir mapped for driver {other}")),
    };
    // A driver crate's `Run` binary shares the package name.
    let crate_dir = ctx.workspace_root.join(rel_dir);
    let elf = cross_compile_pie_elf(ctx, arch, "image-drivers", package, package, &crate_dir)?;
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
pub fn build_vcmailbox_bundle(ctx: &Context, arch: PieArch) -> Result<Vec<u8>, String> {
    build_bundle(
        ctx,
        arch,
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
pub fn build_pcie_brcm_bundle(ctx: &Context, arch: PieArch) -> Result<Vec<u8>, String> {
    build_bundle(
        ctx,
        arch,
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
pub fn build_vl805_bundle(ctx: &Context, arch: PieArch) -> Result<Vec<u8>, String> {
    build_bundle(
        ctx,
        arch,
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
pub fn build_xhci_bundle(ctx: &Context, arch: PieArch) -> Result<Vec<u8>, String> {
    build_bundle(
        ctx,
        arch,
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
pub fn build_usb_kbd_bundle(ctx: &Context, arch: PieArch) -> Result<Vec<u8>, String> {
    build_bundle(
        ctx,
        arch,
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

/// Build and sign the USB boot-mouse **class**-driver bundle.
///
/// A pure HID class driver: it injects decoded pointer records into the
/// kernel input-focus arbiter (`CAP_INPUT_INJECT`), maps the shared URB
/// buffer its host-controller driver forwarded (`CAP_SHM`), submits URBs on
/// its one interface's transport endpoint (`CAP_IPC_ENDPOINT`), and emits a
/// one-shot beacon (`CAP_LOG_EMIT`) — and nothing more. It holds **no** MMIO,
/// DMA, or IRQ authority. Carries `rustos_drv_input_usb_mouse::BIND_KEYS`, so
/// it autoloads against the HID boot-mouse interface node the HCD emitted.
///
/// # Errors
///
/// As [`build_vcmailbox_bundle`].
pub fn build_usb_mouse_bundle(ctx: &Context, arch: PieArch) -> Result<Vec<u8>, String> {
    build_bundle(
        ctx,
        arch,
        "rustos-drv-input-usb-mouse",
        &[
            CapabilityId::INPUT_INJECT,
            CapabilityId::SHM,
            CapabilityId::IPC_ENDPOINT,
            CapabilityId::LOG_EMIT,
        ],
        rustos_drv_input_usb_mouse::BIND_KEYS,
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
pub fn build_usb_msd_bundle(ctx: &Context, arch: PieArch) -> Result<Vec<u8>, String> {
    build_bundle(
        ctx,
        arch,
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
pub fn build_volmgr_bundle(ctx: &Context, arch: PieArch) -> Result<Vec<u8>, String> {
    build_bundle(
        ctx,
        arch,
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
pub fn build_virtio_kbd_bundle(ctx: &Context, arch: PieArch) -> Result<Vec<u8>, String> {
    build_bundle(
        ctx,
        arch,
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

/// Build and sign the framebuffer display-service bundle.
///
/// The zero-copy, lease-gated display half of the desktop present path
/// (`plans/DISPLAY.md` D7b/D7d): it maps its granted scan-out surface
/// (`CAP_MMIO_MAP` — the geometry rides the node's `Framebuffer`
/// resource), maps each session's granted frame region at `Configure`
/// (`CAP_SHM`), binds the reserved `DISPLAY_ENDPOINT` rendezvous
/// (`CAP_IPC_BIND_PRIVILEGED`), and emits its one-shot first-present
/// record (`CAP_LOG_EMIT`) — and nothing more. Every present is gated
/// kernel-side on the caller's live seat lease (`call_peer_seat`, no
/// capability — the authority is serving the in-flight call). Carries
/// `rustos_drv_display_framebuffer::BIND_KEYS`, so it autoloads against
/// the boot display node the kernel publishes for its platform-programmed
/// scan-out surface (and stays unbound on a headless boot, §18.4).
///
/// # Errors
///
/// As [`build_vcmailbox_bundle`].
pub fn build_framebuffer_bundle(ctx: &Context, arch: PieArch) -> Result<Vec<u8>, String> {
    build_bundle(
        ctx,
        arch,
        "rustos-drv-display-framebuffer",
        &[
            CapabilityId::MMIO_MAP,
            CapabilityId::SHM,
            CapabilityId::IPC_BIND_PRIVILEGED,
            CapabilityId::LOG_EMIT,
        ],
        rustos_drv_display_framebuffer::BIND_KEYS,
    )
}

/// Build and sign the virtio-net link-layer driver bundle.
///
/// The `-M virt` two-process netstack path's link driver: it maps its
/// granted register window (`CAP_MMIO_MAP`), carves its virtqueue DMA slab
/// (`CAP_MEM_DMA`), parks on the device interrupt its serve loop waits on
/// (`CAP_IRQ_BIND`), maps the shared frame region (`CAP_SHM`), claims and
/// binds the reserved device-channel endpoint (`CAP_IPC_ENDPOINT`,
/// `CAP_IPC_BIND_PRIVILEGED`), publishes its `netchan` node (`CAP_HW_EMIT`),
/// and emits its readiness beacon (`CAP_LOG_EMIT`) — and nothing more.
/// Carries `rustos_drv_network_virtio_net::BIND_KEYS`, so it autoloads
/// against a discovered virtio-net node (and stays unbound on a machine
/// whose tree carries none — §18.4).
///
/// # Errors
///
/// As [`build_vcmailbox_bundle`].
pub fn build_virtio_net_bundle(ctx: &Context, arch: PieArch) -> Result<Vec<u8>, String> {
    build_bundle(
        ctx,
        arch,
        "rustos-drv-network-virtio-net-driver",
        &[
            CapabilityId::MMIO_MAP,
            CapabilityId::MEM_DMA,
            CapabilityId::IRQ_BIND,
            CapabilityId::SHM,
            CapabilityId::IPC_ENDPOINT,
            CapabilityId::IPC_BIND_PRIVILEGED,
            CapabilityId::HW_EMIT,
            CapabilityId::LOG_EMIT,
        ],
        rustos_drv_network_virtio_net::BIND_KEYS,
    )
}

/// The composed, signed driver bundles the `-M virt` autoload verticals
/// plant into their whole-disk fixture's `/System/Drivers/` store, each
/// paired with its store path as an [`AppStoreFile`] the planter lays down:
/// the virtio-input keyboard driver, the framebuffer display service, and
/// the virtio-net link-layer driver.
///
/// Built **once per xtask process** and shared by every consumer (the
/// concurrent QEMU matrix and the long-CI flake hunt plant the identical
/// set), so the three cross-compiles are paid a single time; a build
/// failure is memoised too and returned to every caller (fail closed,
/// never a partial store). Concurrent first callers are serialised by
/// cargo's own build locking and one result wins.
///
/// # Errors
///
/// A string describing a failed cross-compile, ELF→rxe conversion, or a
/// structurally invalid composed bundle.
pub fn autoload_driver_store_files(
    ctx: &Context,
    arch: PieArch,
) -> Result<&'static [AppStoreFile], String> {
    static FILES: [OnceLock<Result<Vec<AppStoreFile>, String>>; PieArch::COUNT] =
        [const { OnceLock::new() }; PieArch::COUNT];
    FILES[arch.index()]
        .get_or_init(|| {
            Ok(vec![
                store_file(VIRTIO_KBD_STORE_PATH, build_virtio_kbd_bundle(ctx, arch)?),
                store_file(FRAMEBUFFER_STORE_PATH, build_framebuffer_bundle(ctx, arch)?),
                store_file(VIRTIO_NET_STORE_PATH, build_virtio_net_bundle(ctx, arch)?),
            ])
        })
        .as_ref()
        .map(Vec::as_slice)
        .map_err(Clone::clone)
}

/// Pair a `/System`-volume-relative store path with a built bundle's bytes
/// as the [`AppStoreFile`] the planter accepts.
fn store_file(path: &[&[u8]], bytes: Vec<u8>) -> AppStoreFile {
    AppStoreFile {
        components: path.iter().map(|c| c.to_vec()).collect(),
        bytes,
    }
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

#[cfg(test)]
mod tests {
    //! The autoload-decision coverage for the `-M virt` driver store: that
    //! each driver crate's own `BIND_KEYS` (the same table
    //! [`autoload_driver_store_files`] signs into its bundle) is discovered
    //! by the production store scan and resolved by the shared match policy
    //! to the intended node — and to no other. The bundles are signed here
    //! from a stub payload rather than a cross-compiled `rxe`: the scan and
    //! the match decode only the manifest and its bind table, never the
    //! program image, so a real payload adds nothing to what is exercised.
    use super::*;

    use rustos_abi::{DriverBindKey, Errno, HwMatchKey, SIMPLE_FRAMEBUFFER_COMPATIBLE};
    use rustos_devmatch::{resolve, MatchResolution};
    use rustos_drv_network_virtio_net::VIRTIO_NET_DEVICE_ID;
    use rustos_drvhost::store::scan_store;
    use rustos_drvhost::{DriverStore, ImageSource, Sink};
    use rustos_log::Event;
    use rustos_virtio_input::VIRTIO_INPUT_DEVICE_ID;

    /// An arbitrary non-empty program image: the store scan and the match
    /// policy never look at the payload, only the signed manifest and bind
    /// table, so its exact bytes are irrelevant to these tests.
    const STUB_PAYLOAD: &[u8] = b"payload-stub";

    /// The `/`-joined string the store scanner addresses a bundle by,
    /// derived from its single store-path definition so the scan address and
    /// the plant path cannot drift.
    fn path_str(components: &[&[u8]]) -> String {
        let mut s = String::new();
        for c in components {
            s.push('/');
            s.push_str(core::str::from_utf8(c).expect("store path components are ASCII"));
        }
        s
    }

    /// Sign a `kind = UserSpace` bundle carrying `bind_keys` — the exact table
    /// the matching autoload builder signs into the shipped bundle.
    fn sign(bind_keys: &[DriverBindKey]) -> Vec<u8> {
        build_signed_driver_image(
            &build_support::KERNEL_DRIVER_SIGNING_SEED,
            DriverKind::UserSpace,
            &[],
            bind_keys,
            rustos_kernel_syscall::SYSCALL_TABLE_HASH,
            STUB_PAYLOAD,
        )
        .image
    }

    /// The three signed bundles keyed by their store path, serving the
    /// production store scanner's [`ImageSource`] reads.
    struct BundleSource {
        kbd: (String, Vec<u8>),
        framebuffer: (String, Vec<u8>),
        network: (String, Vec<u8>),
    }

    impl BundleSource {
        fn new() -> Self {
            Self {
                kbd: (
                    path_str(VIRTIO_KBD_STORE_PATH),
                    sign(rustos_drv_input_virtio_input::BIND_KEYS),
                ),
                framebuffer: (
                    path_str(FRAMEBUFFER_STORE_PATH),
                    sign(rustos_drv_display_framebuffer::BIND_KEYS),
                ),
                network: (
                    path_str(VIRTIO_NET_STORE_PATH),
                    sign(rustos_drv_network_virtio_net::BIND_KEYS),
                ),
            }
        }
    }

    impl ImageSource for BundleSource {
        fn read(&self, path: &str, buf: &mut Vec<u8>) -> Result<(), Errno> {
            for (store_path, bytes) in [&self.kbd, &self.framebuffer, &self.network] {
                if store_path == path {
                    buf.extend_from_slice(bytes);
                    return Ok(());
                }
            }
            Err(Errno::NotFound)
        }
    }

    /// Discarding audit sink — these tests assert through the resolved match,
    /// not the audit stream.
    struct DiscardSink;

    impl Sink for DiscardSink {
        fn write_event(&self, _event: &Event<'_>) {}
    }

    /// Scan the three-bundle store, candidate indices pinned by scan order
    /// (input 0, display 1, network 2).
    fn scanned_store(source: &BundleSource) -> DriverStore {
        scan_store(
            source,
            &[
                source.kbd.0.as_str(),
                source.framebuffer.0.as_str(),
                source.network.0.as_str(),
            ],
            &DiscardSink,
        )
    }

    #[test]
    fn each_autoload_bundle_is_a_signed_userspace_driver_manifest() {
        for bundle in [
            sign(rustos_drv_input_virtio_input::BIND_KEYS),
            sign(rustos_drv_display_framebuffer::BIND_KEYS),
            sign(rustos_drv_network_virtio_net::BIND_KEYS),
        ] {
            // The same fail-closed structural check the image build applies
            // to every planted bundle accepts each one.
            verify_signed_bundle(&bundle).expect("the signed bundle is well-formed");
        }
    }

    #[test]
    fn the_store_scan_discovers_the_bundles_and_each_binds_its_node() {
        // The production store scan decodes each bundle's bind table
        // fail-closed, and the shared match policy resolves a discovered
        // virtio-input node to the keyboard driver, a boot display node (the
        // kernel's `simple-framebuffer` publication) to the display service,
        // and a virtio-net node to the network driver — the exact autoload
        // decisions the booted kernel makes off the mounted root, with no
        // cross-binding.
        let source = BundleSource::new();
        let store = scanned_store(&source);
        let candidates = store.candidates();
        assert_eq!(
            candidates.len(),
            3,
            "all three signed bundles are candidates"
        );

        let input_keys = [HwMatchKey::virtio(VIRTIO_INPUT_DEVICE_ID)];
        match resolve(&input_keys, &candidates) {
            MatchResolution::Winner { candidate, .. } => assert_eq!(candidate, 0),
            other => panic!("the virtio-input node must bind the keyboard bundle, got {other:?}"),
        }

        let display_keys = [HwMatchKey::compatible(SIMPLE_FRAMEBUFFER_COMPATIBLE).expect("fits")];
        match resolve(&display_keys, &candidates) {
            MatchResolution::Winner { candidate, .. } => assert_eq!(candidate, 1),
            other => panic!("the boot display node must bind the display bundle, got {other:?}"),
        }

        let network_keys = [HwMatchKey::virtio(VIRTIO_NET_DEVICE_ID)];
        match resolve(&network_keys, &candidates) {
            MatchResolution::Winner { candidate, .. } => assert_eq!(candidate, 2),
            other => panic!("the virtio-net node must bind the network bundle, got {other:?}"),
        }
    }

    #[test]
    fn an_unrelated_node_binds_no_bundle() {
        // A node advertising a different virtio device id matches nothing —
        // each bundle binds only its declared device.
        let source = BundleSource::new();
        let store = scanned_store(&source);
        let candidates = store.candidates();
        let node_keys = [HwMatchKey::virtio(VIRTIO_INPUT_DEVICE_ID + 1)];
        assert_eq!(resolve(&node_keys, &candidates), MatchResolution::Unmatched);
    }
}
