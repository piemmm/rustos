//! The `Run` entry-point binary of the BCM2711 (Raspberry Pi 4) PCIe
//! root-complex **bus driver**, installed as a signed `/System/Drivers/`
//! bundle and **autoloaded into user space** by `devmgr` when the
//! `brcm,bcm2711-pcie` node is discovered (`plans/PI.md` P10
//! D5b.2b).
//!
//! This moves the Pi 4 PCIe bring-up out of the kernel scaffold into a
//! user-space bus driver: the kernel mints this process
//! exactly the device-resource grants its matched `brcm,bcm2711-pcie` node
//! requested — the controller register window (`CAP_MMIO_MAP`), the
//! inbound-DMA aperture, and the outbound bus window, and no more — and this program reaches them through the
//! rt-backed `RtDriverHost`. It then:
//!
//! 1. trains the BCM2711 root-complex link over the granted controller
//!    window;
//! 2. enumerates the single USB host controller (the VL805) behind the
//!    bridge through the windowed PCI configuration accessor;
//! 3. assigns and enables that controller's register BAR; and
//! 4. publishes it into the live hardware tree as a bindable xHCI node
//!    carrying the BAR (resolved to its CPU-physical address) and a DMA
//!    constraint as grant *requests* — so `devmgr` autoloads the next driver
//!    in the chain (`drivers/bus/usb/vl805`) against it.
//!
//! The whole composition lives in this crate's own device-support library
//! (`crate::wiring::emit_vl805_node` — `src/lib.rs`), where it is host-tested
//! against a mock bus; this binary is the thin freestanding wiring that builds
//! the real host and drives it. The device logic is co-located here, in the
//! driver, rather than in `lib/*`: a BCM2711 PCIe bus driver sits above the
//! bootstrap floor (the kernel floor is the storage path only), so it
//! has no charter-legal non-driver consumer and the carve-out does not
//! apply. Every capability and bound is re-checked
//! kernel-side, on the far side of each trap; the driver
//! adds no authority, and the kernel owns the published node's identity.
//!
//! It is a **pure-Rust** program: it links the Rust userland
//! runtime `rustos-rt` (`_start`, the stack canary, the panic handler,
//! the syscall wrappers, the clock-backed `ClockDelay`, and `yield_now`),
//! never the C ABI, which exists solely for non-Rust programs. It carves no DMA itself, so it supplies no architecture-specific
//! cache-maintenance shim and names no board detail beyond the discovered
//! grants (`coherency = None`, keeping the program platform-neutral).
//!
//! After publishing the node `main` parks, yielding forever so PID 1 and
//! every other task keeps running while this driver stays resident holding
//! the trained root complex (a genuine yield loop, never a
//! busy spin). A bring-up failure exits with a reserved fail-closed code,
//! leaving the bus unbrought-up rather than wedged; the
//! spawning supervisor decides whether to relaunch.
//!
//! On the host it is an inert stub so `cargo build --workspace`, clippy, and
//! fmt still cover the file.
//!
//! # No QEMU vertical
//!
//! QEMU models no Pi PCIe link timing, so the live
//! train-link → enumerate → publish chain is the on-metal acceptance item;
//! this crate's own host tests (`src/lib.rs`, `src/wiring.rs`) prove the
//! composition and its fail-closed paths up to the controller hand-off.

#![cfg_attr(freestanding, no_std)]
#![cfg_attr(freestanding, no_main)]
#![deny(missing_docs)]

// --- Pure-Rust program --------------------------------------------------
#[cfg(freestanding)]
mod program {
    use rustos_abi::CapabilityId;
    use rustos_caps::CapabilitySet;
    use rustos_drv_bus_pcie_brcm::wiring::{emit_vl805_node, pcie_bringup_from_resources};
    use rustos_drvrt::{RtDriverHost, RtGrantSyscalls};
    use rustos_rt::ClockDelay;

    /// Exit code when the rt-backed driver host could not be built from the
    /// kernel-delivered grants (the `resource_grants` query was refused or the
    /// delivery did not fit). A reserved, fail-closed value.
    const EXIT_NO_HOST: i32 = 80;

    /// Exit code when the delivered grants do not name the controller window,
    /// inbound aperture, and outbound window this bus driver needs — an
    /// unbound or mis-provisioned node. A reserved,
    /// fail-closed value.
    const EXIT_NO_RESOURCES: i32 = 81;

    /// Exit code when the PCIe bring-up / VL805 publish failed (the link never
    /// trained, no USB function was found, the BAR could not be assigned or
    /// mapped, or the node emission was refused). A reserved, fail-closed
    /// value; the bus is left unbrought-up, never wedged.
    const EXIT_BRINGUP_FAILED: i32 = 82;

    /// The capability set the driver host re-checks up front before issuing a
    /// `mmio_map` / `hw_emit_node` trap, so a missing grant fails fast without
    /// a round trip. It mirrors the authority this driver needs — mapping the
    /// controller window and the BAR probe (`CAP_MMIO_MAP`) and publishing the
    /// enumerated child node (`CAP_HW_EMIT`). The kernel is the authority and
    /// re-checks every trap regardless: claiming a
    /// capability the process was not granted only fails the trap kernel-side,
    /// never widens authority.
    fn driver_caps() -> CapabilitySet {
        let mut caps = CapabilitySet::empty();
        caps.insert(CapabilityId::MMIO_MAP);
        caps.insert(CapabilityId::HW_EMIT);
        caps
    }

    /// Program entry point. `rustos-rt`'s `_start` calls it once the runtime
    /// is set up and routes its return value through the `exit` syscall.
    ///
    /// On success this never returns: the driver stays resident holding the
    /// trained root complex for the life of the system.
    fn main() -> i32 {
        // Build the host from the grants the kernel minted for this driver.
        // It carves no DMA, so no architecture-specific cache shim is supplied
        // (`coherency = None`).
        let Ok(host) = RtDriverHost::from_grants_query(driver_caps(), RtGrantSyscalls, None) else {
            return EXIT_NO_HOST;
        };
        // Parse the discovered controller/inbound/outbound windows from the
        // same delivered grants the host maps over — no build-time board
        // constant, no second `resource_grants` syscall.
        let Ok(bringup) = pcie_bringup_from_resources(host.resources()) else {
            return EXIT_NO_RESOURCES;
        };
        // The one userland clock-backed `Delay` for the link bring-up's
        // hardware-dictated microsecond waits.
        let delay = ClockDelay::new();
        if emit_vl805_node(&host, &bringup, &delay).is_err() {
            return EXIT_BRINGUP_FAILED;
        }
        // The root complex must stay trained and this driver resident for the
        // life of the system; park yielding so PID 1 and every other task
        // keeps running (a genuine yield loop, never a hard
        // spin).
        loop {
            rustos_rt::yield_now();
        }
    }

    rustos_rt::entry!(main);
}

// --- Host stub ----------------------------------------------------------
//
// On the host (`cargo build --workspace`, clippy, fmt) the program's real
// entry — the freestanding `rustos-rt` `_start` path — is not compiled, so
// this inert `main` keeps the crate building under the host tooling. It
// performs no I/O.
#[cfg(not(freestanding))]
fn main() {}
