//! The `Run` entry-point binary of the Raspberry Pi 4 (BCM2711) **VL805**
//! USB **bus driver**, installed as a signed `/System/Drivers/` bundle and
//! **autoloaded into user space** by `devmgr` when the VL805 PCI node is
//! discovered (`plans/PI.md` P10 D5c).
//!
//! This is the device-specific link in the Pi 4 USB chain:
//! the PCIe root-complex bus driver (`drivers/bus/pcie_brcm`) trains the link,
//! enumerates the VL805 behind the bridge, assigns its register BAR, and
//! publishes it as a VL805 PCI node (`node A`) carrying that BAR (at its
//! CPU-physical address) and the inbound-DMA constraint as grant requests.
//! `devmgr` autoloads this driver against node A; the kernel mints it exactly
//! those two grants — and **no** mapping capability for them. This program holds only `CAP_MAILBOX` and `CAP_HW_EMIT`, so
//! it cannot touch the controller's registers or DMA; its job is narrower:
//!
//! 1. reload the VL805's firmware over the `VideoCore` mailbox IPC — the
//!    link bring-up's `PERST#` drops the firmware on EEPROM-less Pi 4 boards,
//!    and only the `VideoCore` can reload it (reached through the rt-backed
//!    host's `MailboxChannel`, which marshals to the user-space `vcmailbox`
//!    service over the kernel's call surface); then
//! 2. publish the controller as `node B`, an `usb,xhci` node **forwarding**
//!    node A's BAR + DMA grants, so `devmgr` autoloads the controller's own
//!    driver (`drivers/input/usb_kbd`) against it.
//!
//! Firmware-before-bring-up holds **by construction**: node B does not exist
//! until this program runs the reload, so the driver that binds node B can
//! never bring the controller up before its firmware is loaded.
//!
//! The whole reload-and-publish composition lives in this crate's own
//! device-support library (`crate::wiring::{build_xhci_node,
//! reload_firmware_and_publish}` — `src/lib.rs`), where it is host-tested
//! against `DriverHost` doubles; this binary is the thin freestanding wiring
//! that builds the real host and drives it. The device logic is co-located
//! here, in the driver, rather than in `lib/*`: a VL805 USB driver sits above
//! the bootstrap floor, so it has no charter-legal non-driver consumer
//! and the carve-out does not apply. Every
//! capability and bound is re-checked kernel-side, on the far side of each
//! trap; the driver adds no authority, and the kernel owns
//! the published node's identity.
//!
//! It is a **pure-Rust** program: it links the Rust userland
//! runtime `tairix-rt` (`_start`, the stack canary, the panic handler,
//! the syscall wrappers, and `park_forever`), never the C ABI, which exists
//! solely for non-Rust programs. It maps no DMA and no
//! registers, so it supplies no architecture-specific cache shim and names no
//! board detail (`coherency = None`, keeping the program platform-neutral).
//!
//! After publishing the node `main` parks off the run queue for the life of
//! the system (`tairix_rt::park_forever`) while this driver stays resident —
//! a real park consuming no CPU, never a yield loop. A bring-up failure
//! exits with a reserved fail-closed code, leaving the controller
//! unpublished rather than wedged; the spawning supervisor
//! decides whether to relaunch.
//!
//! On the host it is an inert stub so `cargo build --workspace`, clippy, and
//! fmt still cover the file.
//!
//! # No QEMU vertical
//!
//! QEMU models no `VideoCore` mailbox or Pi USB timing, so
//! the live reload → publish chain is the on-metal acceptance item; this
//! crate's own host tests (`src/lib.rs`, `src/wiring.rs`) prove the
//! composition and its fail-closed paths.

#![cfg_attr(freestanding, no_std)]
#![cfg_attr(freestanding, no_main)]
#![deny(missing_docs)]

// --- Pure-Rust program --------------------------------------------------
#[cfg(freestanding)]
mod program {
    use tairix_abi::CapabilityId;
    use tairix_caps::CapabilitySet;
    use tairix_drv_bus_usb_vl805::wiring::{build_xhci_node, reload_firmware_and_publish};
    use tairix_drvrt::{RtDriverHost, RtGrantSyscalls};

    /// Exit code when the rt-backed driver host could not be built from the
    /// kernel-delivered grants (the `resource_grants` query was refused or the
    /// delivery did not fit). A reserved, fail-closed value.
    const EXIT_NO_HOST: i32 = 80;

    /// Exit code when the delivered grants do not name the controller's
    /// register BAR and inbound-DMA constraint this driver forwards — an
    /// unbound or mis-provisioned node. A reserved,
    /// fail-closed value.
    const EXIT_NO_RESOURCES: i32 = 81;

    /// Exit code when publishing the xHCI controller node was refused (the
    /// driver lacks `CAP_HW_EMIT`, or a forwarded resource is not covered by a
    /// grant). A reserved, fail-closed value; the firmware
    /// reload is best-effort and never fails the driver (its authoritative
    /// gate is the controller driver's `Xhci::open`).
    const EXIT_PUBLISH_FAILED: i32 = 82;

    /// Exit code when the resident park failed — the kernel refused the
    /// wait-set the lifetime park runs on. Exiting fail-loud beats the
    /// yield-forever spin a parkless residency would degrade into; the
    /// spawning supervisor decides whether to relaunch. A reserved value.
    const EXIT_PARK_FAILED: i32 = 83;

    /// The capability set the driver host re-checks up front before issuing an
    /// `ipc_call` (the mailbox) / `hw_emit_node` (the publish) trap, so a
    /// missing grant fails fast without a round trip. It mirrors the authority
    /// this driver needs — reloading the firmware over the `VideoCore` mailbox
    /// (`CAP_MAILBOX`) and publishing the controller node (`CAP_HW_EMIT`) —
    /// and deliberately **excludes** `CAP_MMIO_MAP` / `CAP_MEM_DMA`: this
    /// driver forwards the BAR/DMA grants without ever mapping them
    /// (least privilege). The kernel re-checks every trap
    /// regardless.
    fn driver_caps() -> CapabilitySet {
        let mut caps = CapabilitySet::empty();
        caps.insert(CapabilityId::MAILBOX);
        caps.insert(CapabilityId::HW_EMIT);
        caps
    }

    /// Program entry point. `tairix-rt`'s `_start` calls it once the runtime
    /// is set up and routes its return value through the `exit` syscall.
    ///
    /// On success this never returns: the driver stays resident for the life
    /// of the system after publishing the controller node.
    fn main() -> i32 {
        // Build the host from the grants the kernel minted for this driver. It
        // maps no DMA and no registers, so no architecture-specific cache shim
        // is supplied (`coherency = None`).
        let Ok(host) = RtDriverHost::from_grants_query(driver_caps(), RtGrantSyscalls, None) else {
            return EXIT_NO_HOST;
        };
        // Build node B from the same delivered grants the host holds — the
        // controller's BAR + DMA, forwarded to the next driver in the chain
        // (no build-time board constant, no second `resource_grants` syscall).
        let Ok(node) = build_xhci_node(host.resources()) else {
            return EXIT_NO_RESOURCES;
        };
        // Reload the firmware over the mailbox IPC, then publish node B. The
        // reload is best-effort (the firmware outcome is discarded here — the
        // authoritative liveness gate is the bound driver's `Xhci::open`); only a refused publish fails the driver.
        if reload_firmware_and_publish(&host, node).is_err() {
            return EXIT_PUBLISH_FAILED;
        }
        // The controller node must stay published and this driver resident
        // for the life of the system: park off the run queue for good. A
        // failed park exits fail-loud rather than degrading into a yield
        // spin.
        let _ = tairix_rt::park_forever();
        EXIT_PARK_FAILED
    }

    tairix_rt::entry!(main);
}

// --- Host stub ----------------------------------------------------------
//
// On the host (`cargo build --workspace`, clippy, fmt) the program's real
// entry — the freestanding `tairix-rt` `_start` path — is not compiled, so
// this inert `main` keeps the crate building under the host tooling. It
// performs no I/O.
#[cfg(not(freestanding))]
fn main() {}
