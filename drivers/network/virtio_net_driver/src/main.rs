//! The `Run` entry-point binary of the virtio-net driver, installed as a
//! signed `/System/Drivers/` bundle and **autoloaded into user space** by
//! `devmgr` when a virtio-net device is discovered (`plans/NETWORK.md` N4d).
//!
//! This is the "drivers in user space" steady state for networking: the process
//! owns the NIC (its register window, DMA, and interrupt line) and serves the
//! `netchan-v1` device-channel contract to the network stack
//! (`userland/net/netstack`), which runs in its own address space and owns the
//! shared frame-ring region. The two never link each other — the driver is the
//! *server* of a claimed reserved endpoint and the stack is the *client* — so
//! any NIC driver serves any stack build.
//!
//! It is a **pure-Rust** program: TAIRiX is Rust-only, so it links the Rust
//! userland runtime `tairix-rt`, never the C ABI (which exists solely for
//! non-Rust programs). `tairix-rt` provides `_start`, the per-process stack
//! canary, the panic handler, the allocator, and the syscall wrappers;
//! `tairix_rt::entry!` names this program's `main`. It links no `drivers/*`
//! crate — the virtio-MMIO transport and the virtio-net device engine are
//! `lib/*` crates — so the layering holds and the kernel never pulls the
//! userland runtime into its graph.
//!
//! # What `main` does
//!
//! 1. Builds the rt-backed `RtDriverHost` from the grants the kernel minted
//!    for this driver's matched node (a register window, a DMA constraint, and
//!    the device interrupt line — and no more), and maps the single register
//!    window the node named (never a build-time board constant).
//! 2. Brings the virtio-net device online over the bus-agnostic virtio
//!    transport (`VirtioNet::open`) and binds the granted interrupt line.
//! 3. Hands the opened device to `tairix_netchan::serve`, the shared
//!    device-channel serve loop every NIC driver process runs: it claims a
//!    reserved endpoint bound restricted-sender, publishes the `netchan` node
//!    `devmgr` hands to the stack, and parks on {call endpoint, device IRQ} for
//!    the life of the driver (never busy-polls).
//!
//! A bring-up failure exits with a reserved fail-closed code
//! (`tairix_netchan::exit`), leaving the system without this NIC rather than
//! wedged; the spawning supervisor decides whether to relaunch. On the host it
//! is an inert stub so `cargo build --workspace`, clippy, and fmt still cover
//! the file.

#![cfg_attr(freestanding, no_std)]
#![cfg_attr(freestanding, no_main)]
#![deny(missing_docs)]

// --- Pure-Rust program --------------------------------------------------
#[cfg(freestanding)]
mod program {
    use tairix_abi::driver::sole_register_window;
    use tairix_abi::driver::virtio::VirtioHost;
    use tairix_abi::driver::virtio_pci::{virtio_pci_windows, VirtioPciWindows};
    use tairix_abi::{CapabilityId, DriverError, MmioMapper};
    use tairix_caps::CapabilitySet;
    use tairix_drvrt::{RtDriverHost, RtGrantSyscalls};
    use tairix_netchan::exit;
    use tairix_virtio::{MmioTransport, PciTransport, PciTransportWindows};
    use tairix_virtio_net::VirtioNet;

    /// MSI-X table entry the kernel PCI probe routes this device's single
    /// interrupt to, and the entry the driver selects for the config change
    /// and every virtqueue. The driver uses one interrupt line (it parks on
    /// one bound handle), so one shared MSI-X vector — entry `0` — carries
    /// every device notification; the kernel programs that entry's table
    /// slot at spawn and the driver only echoes the entry number into the
    /// device's vector registers (writing its own mapped common window, no
    /// PCI config access). Ignored on the single-aperture MMIO bus, which
    /// has no MSI-X.
    const MSIX_ENTRY: u16 = 0;

    /// The capability set the driver host re-checks up front before issuing a
    /// `mmio_map` / `dma_alloc` / `irq_bind` trap, so a missing grant fails
    /// fast without a round trip. It mirrors the resources the matched node
    /// requested — the register window (`CAP_MMIO_MAP`), the DMA region
    /// (`CAP_MEM_DMA`), and the device interrupt line the serve loop parks on
    /// (`CAP_IRQ_BIND`) — plus the authority to claim and bind the reserved
    /// device-channel endpoint (`CAP_IPC_ENDPOINT`, `CAP_IPC_BIND_PRIVILEGED`)
    /// and publish the `netchan` node (`CAP_HW_EMIT`). The kernel is the
    /// authority and re-checks every trap regardless.
    fn driver_caps() -> CapabilitySet {
        let mut caps = CapabilitySet::empty();
        caps.insert(CapabilityId::MMIO_MAP);
        caps.insert(CapabilityId::MEM_DMA);
        caps.insert(CapabilityId::IRQ_BIND);
        caps.insert(CapabilityId::SHM);
        caps.insert(CapabilityId::IPC_ENDPOINT);
        caps.insert(CapabilityId::IPC_BIND_PRIVILEGED);
        caps.insert(CapabilityId::HW_EMIT);
        caps.insert(CapabilityId::LOG_EMIT);
        caps
    }

    /// Program entry point. `tairix-rt`'s `_start` calls it once the runtime
    /// is set up and routes its return value through the `exit` syscall.
    ///
    /// On success this never returns: the device-channel serve loop runs for
    /// the life of the driver process.
    fn main() -> i32 {
        // The QEMU `virt` virtio interconnect snoops the CPU caches, so the
        // DMA carve is coherent kernel-side and no cache-maintenance shim is
        // supplied here (`coherency = None`, keeping the program neutral).
        let Ok(host) = RtDriverHost::from_grants_query(driver_caps(), RtGrantSyscalls, None) else {
            return exit::NO_HOST;
        };
        let Some(irq_line) = host.irq_line() else {
            return exit::NO_RESOURCES;
        };

        // Bind the device interrupt line the serve loop parks on. This is the
        // audited readiness witness, issued once the device is live: a driver
        // that cannot bind it must exit rather than degrade into a busy
        // re-poll. It is bound once here and shared by both bus paths (the
        // MMIO line and the kernel-routed MSI-X vector alike surface as one
        // bound handle).
        let irq_ret = tairix_rt::irq_bind(irq_line);
        if irq_ret <= 0 {
            return exit::BRINGUP_FAILED;
        }
        #[allow(clippy::cast_sign_loss)] // `irq_ret > 0` is the minted IrqHandle.
        let irq_handle = irq_ret as u64;

        // Shape-key the transport by the grant set the kernel minted, so one
        // signed bundle binds on either virtio bus: a scattered PCI device
        // grants the four role-tagged config windows (`common`/`notify`/`isr`/
        // `device`), while a single-aperture MMIO device grants one register
        // window. The kernel resolved and role-tagged every window — the
        // driver never reads PCI configuration space.
        match virtio_pci_windows(host.resources()) {
            Ok(windows) => {
                let Some(transport) = build_pci_transport(&host, &windows) else {
                    return exit::BRINGUP_FAILED;
                };
                let vhost: &dyn VirtioHost = &host;
                let Ok(net) = VirtioNet::open(transport, vhost) else {
                    return exit::BRINGUP_FAILED;
                };
                tairix_netchan::serve(net, irq_handle)
            }
            // No role-tagged window at all: a single-aperture MMIO delivery.
            Err(DriverError::NotFound) => {
                let Ok((base, len)) = sole_register_window(host.resources()) else {
                    return exit::NO_RESOURCES;
                };
                let Ok(window) = host.map_window(base, len) else {
                    return exit::BRINGUP_FAILED;
                };
                let Ok(transport) = MmioTransport::new(window) else {
                    return exit::BRINGUP_FAILED;
                };
                let vhost: &dyn VirtioHost = &host;
                let Ok(net) = VirtioNet::open(transport, vhost) else {
                    return exit::BRINGUP_FAILED;
                };
                tairix_netchan::serve(net, irq_handle)
            }
            // Some virtio-PCI windows but not the full four — a malformed,
            // mis-provisioned node. Fail closed rather than half-bind.
            Err(_) => exit::NO_RESOURCES,
        }
    }

    /// Build the modern virtio-PCI [`PciTransport`] from the four
    /// kernel-resolved config windows, mapping each through the host's
    /// capability-gated MMIO facility and selecting the kernel-routed MSI-X
    /// entry before the transport programs the device's virtqueues.
    ///
    /// Returns [`None`] on any window map failure (a refused or malformed
    /// grant) or a malformed common-configuration window — fail closed, never
    /// a half-built transport.
    fn build_pci_transport(
        mapper: &dyn MmioMapper,
        windows: &VirtioPciWindows,
    ) -> Option<PciTransport> {
        let common = mapper.map_window(windows.common.0, windows.common.1).ok()?;
        let notify = mapper.map_window(windows.notify.0, windows.notify.1).ok()?;
        let isr = mapper.map_window(windows.isr.0, windows.isr.1).ok()?;
        let device = mapper.map_window(windows.device.0, windows.device.1).ok()?;
        let mut transport = PciTransport::new(PciTransportWindows {
            common,
            notify,
            isr,
            device,
            notify_off_multiplier: windows.notify_off_multiplier,
        })
        .ok()?;
        // Select the MSI-X entry the kernel routed this device's interrupt to,
        // before `VirtioNet::open` drives the virtqueue programming that reads
        // it back. Writes only the driver's own mapped common window.
        transport.enable_msix(MSIX_ENTRY);
        Some(transport)
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
