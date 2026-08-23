//! The `Run` entry-point binary of the GENET driver, installed as a signed
//! `/System/Drivers/` bundle and **autoloaded into user space** by `devmgr`
//! when the Raspberry Pi 4B's `brcm,bcm2711-genet-v5` node is discovered
//! (`plans/NETWORK.md` N14).
//!
//! The process owns the NIC — its register window, its frame-buffer DMA
//! carve, and its interrupt line — and serves the `netchan-v1`
//! device-channel contract to the network stack, which runs in its own
//! address space and owns the shared frame-ring region. The two never link
//! each other, so this driver serves any stack build.
//!
//! It is a **pure-Rust** program: TAIRiX is Rust-only, so it links the Rust
//! userland runtime `tairix-rt`, never the C ABI (which exists solely for
//! non-Rust programs). `tairix-rt` provides `_start`, the per-process stack
//! canary, the panic handler, the allocator, and the syscall wrappers;
//! `tairix_rt::entry!` names this program's `main`.
//!
//! # What `main` does
//!
//! 1. Builds the rt-backed `RtDriverHost` from the grants the kernel minted
//!    for this driver's matched node — the register window, the DMA
//!    constraint, and the device interrupt line, and no more.
//! 2. Binds the granted interrupt line the serve loop parks on.
//! 3. Brings the controller up through `wiring::open_discovered`, which maps
//!    the window the node named (never a build-time board constant), carves
//!    the frame buffers, verifies the core really is a GENET v5, programs
//!    the firmware-published MAC, builds both DMA rings, and negotiates the
//!    PHY link.
//! 4. Hands the opened device to `tairix_netchan::serve`, the shared
//!    device-channel serve loop every NIC driver process runs.
//!
//! A bring-up failure exits with a reserved fail-closed code
//! (`tairix_netchan::exit`), leaving the system without this NIC rather than
//! wedged; the spawning supervisor decides whether to relaunch. On the host
//! it is an inert stub so `cargo build --workspace`, clippy, and fmt still
//! cover the file.

#![cfg_attr(freestanding, no_std)]
#![cfg_attr(freestanding, no_main)]
#![deny(missing_docs)]

// --- Pure-Rust program --------------------------------------------------
#[cfg(freestanding)]
mod program {
    use tairix_abi::CapabilityId;
    use tairix_caps::CapabilitySet;
    use tairix_drv_network_genet::wiring::open_discovered;
    use tairix_drvrt::{RtDriverHost, RtGrantSyscalls};
    use tairix_netchan::exit;
    use tairix_rt::ClockDelay;

    /// The capability set the driver host re-checks up front before issuing a
    /// `mmio_map` / `dma_alloc` / `irq_bind` trap, so a missing grant fails
    /// fast without a round trip. It mirrors the resources the matched node
    /// requested — the register window (`CAP_MMIO_MAP`), the frame-buffer
    /// carve (`CAP_MEM_DMA`), and the device interrupt line the serve loop
    /// parks on (`CAP_IRQ_BIND`) — plus the authority to map the stack's
    /// granted frame region (`CAP_SHM`), claim and bind the reserved
    /// device-channel endpoint (`CAP_IPC_ENDPOINT`,
    /// `CAP_IPC_BIND_PRIVILEGED`), publish the `netchan` node
    /// (`CAP_HW_EMIT`), and emit its readiness beacon (`CAP_LOG_EMIT`). The
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

    /// Program entry point. `tairix-rt`'s `_start` calls it once the runtime
    /// is set up and routes its return value through the `exit` syscall.
    ///
    /// On success this never returns: the device-channel serve loop runs for
    /// the life of the driver process.
    fn main() -> i32 {
        // The kernel maps a DMA carve Normal Non-Cacheable, so the frame
        // buffers are coherent with the controller by construction and no
        // cache-maintenance shim is supplied (`coherency = None`).
        let Ok(host) = RtDriverHost::from_grants_query(driver_caps(), RtGrantSyscalls, None) else {
            return exit::NO_HOST;
        };
        let Some(irq_line) = host.irq_line() else {
            return exit::NO_RESOURCES;
        };

        // Bind the interrupt the serve loop parks on before the device can
        // raise one. A driver that cannot bind it must exit rather than
        // degrade into the busy re-poll the charter forbids.
        let irq_ret = tairix_rt::irq_bind(irq_line);
        if irq_ret <= 0 {
            return exit::BRINGUP_FAILED;
        }
        #[allow(clippy::cast_sign_loss)] // `irq_ret > 0` is the minted IrqHandle.
        let irq_handle = irq_ret as u64;

        let Ok(net) = open_discovered(
            &host,
            host.resources(),
            host.link_address(),
            ClockDelay::new(),
        ) else {
            return exit::BRINGUP_FAILED;
        };
        tairix_netchan::serve(net, irq_handle)
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
