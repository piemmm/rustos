//! The `Run` entry-point binary of the virtio sound driver, installed as a
//! signed `/System/Drivers/` bundle and **autoloaded into user space** by
//! `devmgr` when a virtio-snd device is discovered (`plans/SOUND.md` SND4).
//!
//! This is the "drivers in user space" steady state for audio: the process
//! owns the device (its register window, DMA, and interrupt line) and serves
//! the `audiochan-v1` device-channel contract to the mixer service
//! (`userland/system/audiod`), which runs in its own address space and owns
//! the shared PCM regions. The two never link each other — the driver is the
//! *server* of a claimed reserved endpoint and the mixer is the one *client*
//! the kernel admits — so any audio driver serves any mixer build. Nothing
//! about sound is in the kernel.
//!
//! It is a **pure-Rust** program: TAIRiX is Rust-only, so it links the Rust
//! userland runtime `tairix-rt`, never the C ABI (which exists solely for
//! non-Rust programs). It links no `drivers/*` crate — the virtio transport
//! and the device-channel serve loop are `lib/*` crates and the device engine
//! is this crate's own `lib` target — so the layering holds.
//!
//! # What `main` does
//!
//! 1. Builds the rt-backed `RtDriverHost` from the grants the kernel minted
//!    for this driver's matched node (a register window, a DMA constraint,
//!    and the device interrupt line — and no more).
//! 2. Binds the granted interrupt line the serve loop parks on.
//! 3. Brings the device online over the bus-agnostic virtio transport,
//!    shape-keyed by the grant set so one signed bundle binds on either bus.
//! 4. Hands the opened device to `tairix_audiochan::serve`, the shared
//!    device-channel serve loop every audio driver process runs: it claims a
//!    reserved endpoint bound restricted-sender on `CAP_AUDIO_DEVICE`,
//!    publishes the `audiochan` node `devmgr` hands to the mixer, and parks
//!    on {call endpoint, device IRQ} for the life of the driver.
//!
//! A bring-up failure exits with a reserved fail-closed code
//! (`tairix_audiochan::exit`), leaving the machine without sound rather than
//! wedged; the spawning supervisor decides whether to relaunch. On the host
//! it is an inert stub so `cargo build --workspace`, clippy, and fmt still
//! cover the file.

#![cfg_attr(freestanding, no_std)]
#![cfg_attr(freestanding, no_main)]
#![deny(missing_docs)]

// --- Pure-Rust program --------------------------------------------------
#[cfg(freestanding)]
mod program {
    use tairix_abi::driver::sole_register_window;
    use tairix_abi::driver::virtio::VirtioHost;
    use tairix_abi::driver::virtio_pci::{virtio_pci_windows, VirtioPciWindows};
    use tairix_abi::time::MonotonicClock;
    use tairix_abi::{CapabilityId, DriverError, MmioMapper};
    use tairix_audiochan::{exit, fail};
    use tairix_caps::CapabilitySet;
    use tairix_drv_audio_virtio_snd::VirtioSnd;
    use tairix_drvrt::{RtDriverHost, RtGrantSyscalls};
    use tairix_rt::ClockDelay;
    use tairix_virtio::{MmioTransport, PciTransport, PciTransportWindows};

    /// MSI-X table entry the kernel PCI probe routes this device's single
    /// interrupt to. The driver parks on one bound handle, so one shared
    /// vector — entry `0` — carries every device notification; the kernel
    /// programs that entry's table slot at spawn and the driver only echoes
    /// the entry number into the device's own mapped common window. Ignored
    /// on the single-aperture MMIO bus, which has no MSI-X.
    const MSIX_ENTRY: u16 = 0;

    /// The capability set the driver host re-checks up front before issuing a
    /// `mmio_map` / `dma_alloc` / `irq_bind` trap, so a missing grant fails
    /// fast without a round trip. It mirrors the resources the matched node
    /// requested — the register window (`CAP_MMIO_MAP`), the DMA region
    /// (`CAP_MEM_DMA`), and the device interrupt line the serve loop parks on
    /// (`CAP_IRQ_BIND`) — plus the authority to map the mixer's granted PCM
    /// regions (`CAP_SHM`), to claim and bind the reserved device-channel
    /// endpoint (`CAP_IPC_ENDPOINT`, `CAP_IPC_BIND_PRIVILEGED`), to publish
    /// the `audiochan` node (`CAP_HW_EMIT`), and to emit its readiness beacon
    /// (`CAP_LOG_EMIT`). It deliberately does **not** hold
    /// `CAP_AUDIO_DEVICE`: that is the authority to *command* an audio
    /// driver, which the mixer holds and this process is the subject of. The
    /// kernel is the authority and re-checks every trap regardless.
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

    /// Why a bring-up gave up once the transport was built, on either bus.
    /// The `error` field the record carries is what the device or the kernel
    /// actually refused with.
    const OPEN_REFUSED: &str = "virtio-snd: the device refused its bring-up";

    /// Program entry point. `tairix-rt`'s `_start` calls it once the runtime
    /// is set up and routes its return value through the `exit` syscall.
    ///
    /// On success this never returns: the device-channel serve loop runs for
    /// the life of the driver process.
    fn main() -> i32 {
        // The QEMU `virt` virtio interconnect snoops the CPU caches, so the
        // DMA carve is coherent kernel-side and no cache-maintenance shim is
        // supplied here, which keeps the program platform-neutral.
        let host = match RtDriverHost::from_grants_query(driver_caps(), RtGrantSyscalls, None) {
            Ok(host) => host,
            Err(err) => {
                return fail(
                    exit::NO_HOST,
                    "virtio-snd: the driver host could not be built from the delivered grants",
                    Some(err),
                )
            }
        };
        if host.irq_line().is_none() {
            return fail(
                exit::NO_RESOURCES,
                "virtio-snd: the matched node granted no interrupt line",
                None,
            );
        }
        // Bound through the host, which caches the handle, rather than by
        // calling the trap directly: the device bring-up below parks on
        // completions through this same host, so a direct bind would bind the
        // line a second time and the kernel refuses that. Binding before the
        // device is live is safe here because its event sources stay masked
        // until the mixer attaches a region, so no event can be dropped in
        // the window.
        if let Err(err) = host.bind_irq() {
            return fail(
                exit::BRINGUP_FAILED,
                "virtio-snd: the granted interrupt line could not be bound",
                Some(err),
            );
        }
        let Some(irq_handle) = host.irq_handle() else {
            return fail(
                exit::BRINGUP_FAILED,
                "virtio-snd: the interrupt line bound but minted no handle",
                None,
            );
        };

        // The clock every period's `(position, sampled_at)` pair is stamped
        // from. Monotonic, because a wall clock stepped by the time service
        // would corrupt the mixer's rate fit.
        let clock = ClockDelay::new();
        let vhost: &dyn VirtioHost = &host;
        let mclock: &dyn MonotonicClock = &clock;

        match virtio_pci_windows(host.resources()) {
            Ok(windows) => {
                let Some(transport) = build_pci_transport(&host, &windows) else {
                    return fail(
                        exit::BRINGUP_FAILED,
                        "virtio-snd: a granted virtio-PCI config window could not be mapped",
                        None,
                    );
                };
                let audio = match VirtioSnd::open(transport, vhost, mclock) {
                    Ok(audio) => audio,
                    Err(err) => return fail(exit::BRINGUP_FAILED, OPEN_REFUSED, Some(err)),
                };
                tairix_audiochan::serve(audio, irq_handle)
            }
            // No role-tagged window at all: a single-aperture MMIO delivery.
            Err(DriverError::NotFound) => {
                let (base, len) = match sole_register_window(host.resources()) {
                    Ok(window) => window,
                    Err(err) => {
                        return fail(
                            exit::NO_RESOURCES,
                            "virtio-snd: the matched node granted no single register window",
                            Some(err),
                        )
                    }
                };
                let window = match host.map_window(base, len) {
                    Ok(window) => window,
                    Err(err) => {
                        return fail(
                            exit::BRINGUP_FAILED,
                            "virtio-snd: the granted register window could not be mapped",
                            Some(err.as_driver_error()),
                        )
                    }
                };
                let transport = match MmioTransport::new(window) {
                    Ok(transport) => transport,
                    Err(err) => {
                        return fail(
                            exit::BRINGUP_FAILED,
                            "virtio-snd: the mapped window is not a virtio-MMIO transport",
                            Some(err.as_driver_error()),
                        )
                    }
                };
                let audio = match VirtioSnd::open(transport, vhost, mclock) {
                    Ok(audio) => audio,
                    Err(err) => return fail(exit::BRINGUP_FAILED, OPEN_REFUSED, Some(err)),
                };
                tairix_audiochan::serve(audio, irq_handle)
            }
            // Some virtio-PCI windows but not the full four — a malformed,
            // mis-provisioned node. Fail closed rather than half-bind.
            Err(err) => fail(
                exit::NO_RESOURCES,
                "virtio-snd: the node's virtio-PCI windows are incomplete",
                Some(err),
            ),
        }
    }

    /// Build the modern virtio-PCI [`PciTransport`] from the four
    /// kernel-resolved config windows, mapping each through the host's
    /// capability-gated MMIO facility and selecting the kernel-routed MSI-X
    /// entry before the transport programs the device's virtqueues.
    ///
    /// Returns [`None`] on any window map failure or a malformed
    /// common-configuration window — fail closed, never a half-built
    /// transport.
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
