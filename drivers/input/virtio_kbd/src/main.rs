//! The `Run` entry-point binary of the virtio-input keyboard driver, installed
//! as a signed `/System/Drivers/` bundle and **autoloaded into user space** by
//! `devmgr` when a virtio-input device is discovered (`AGENTS.md` §18,
//! `plans/PI.md` P10 chunk 5d-2-ii).
//!
//! This is the "drivers in user space" steady state (`AGENTS.md` §4) on the
//! hardware QEMU `-M virt` actually presents (a virtio-input keyboard; the
//! metal Pi 4 keyboard is the USB `drivers/input/usb_kbd`). The kernel mints
//! this process exactly the device-resource grants its matched node requested
//! — the device's register window and a DMA constraint, and no more
//! (`AGENTS.md` §4 / §18.3) — and this program reaches them through the
//! rt-backed `RtDriverHost`. It names no board, bus, or transport detail
//! (`AGENTS.md` §2.20): it maps a register window by address, carves a DMA
//! region, and speaks the bus-agnostic virtio split-virtqueue protocol via the
//! arch-neutral `rustos_virtio_input` composition over the `rustos_virtio`
//! MMIO transport.
//!
//! It is a **pure-Rust** program: RustOS is Rust-only (`AGENTS.md` §1), so it
//! links the Rust userland runtime `rustos-rt`, never the C ABI (which exists
//! solely for non-Rust programs, `AGENTS.md` §16.4). `rustos-rt` provides
//! `_start`, the per-process stack canary (`AGENTS.md` §19.2), the panic
//! handler, and the syscall wrappers; `rustos_rt::entry!` names this program's
//! `main`. It is a separate crate from the `rustos-drv-input-virtio-input`
//! driver shell (which the kernel still links for the transitional in-kernel
//! `-M virt` verticals) so the userland runtime never enters the kernel's
//! dependency graph.
//!
//! `main` wires the real seams the bring-up and the report pump drive:
//!
//! * `RtDriverHost::from_grants_query` over `RtGrantSyscalls`: the host
//!   learns its kernel-issued grants through the `resource_grants` syscall and
//!   maps/carves them through `mmio_map` / `dma_alloc`. Every capability and
//!   bound is re-checked kernel-side, on the far side of the trap (`AGENTS.md`
//!   §5.4); the host adds no authority. The DMA carve is coherent kernel-side
//!   (the QEMU `virt` virtio interconnect snoops the CPU caches), so no
//!   architecture-specific cache-maintenance shim is supplied here
//!   (`coherency = None`, keeping the program platform-neutral, §2.20).
//! * `sole_register_window` over the delivered grants: the device's register
//!   window `(base, len)` is read from the grants the kernel delivered, never a
//!   build-time board constant (`AGENTS.md` §2.16 / §2.20).
//! * `MmioTransport::new` over the mapped window, then `VirtioInput::open`:
//!   brings the virtio-input device online and posts its event queue.
//! * The poll/feed/inject loop: each decoded `InputEvent` key edge is
//!   resolved into a `KeyInput` record by `VirtioKeyboardConsole` and
//!   injected into the kernel input-focus arbiter through the `key_inject`
//!   syscall, which routes it by who holds focus (`AGENTS.md` §17.4). The
//!   driver no longer chooses the encoding or the destination.
//!
//! After bring-up `main` polls the device forever, yielding between polls so
//! the rest of the system runs (`AGENTS.md` §2.1 — a cooperative poll loop,
//! never a hard spin); a `poll` error is non-fatal and the next poll retries.
//! A bring-up failure exits with a reserved fail-closed code, leaving the
//! console without a keyboard rather than wedged (`AGENTS.md` §2.9); the
//! spawning supervisor decides whether to relaunch.
//!
//! On the host it is an inert stub so `cargo build --workspace`, clippy, and
//! fmt still cover the file.

#![cfg_attr(freestanding, no_std)]
#![cfg_attr(freestanding, no_main)]
#![deny(missing_docs)]

// --- Pure-Rust program --------------------------------------------------
#[cfg(freestanding)]
mod program {
    use rustos_abi::driver::input::{Input, InputEvent, InputEventKind};
    use rustos_abi::driver::sole_register_window;
    use rustos_abi::driver::virtio::VirtioHost;
    use rustos_abi::{CapabilityId, MmioMapper};
    use rustos_caps::CapabilitySet;
    use rustos_drvrt::{RtDriverHost, RtGrantSyscalls};
    use rustos_virtio::MmioTransport;
    use rustos_virtio_input::{VirtioInput, VirtioKeyboardConsole};

    /// Exit code when the rt-backed driver host could not be built from the
    /// kernel-delivered grants (the `resource_grants` query was refused or the
    /// delivery did not fit). A reserved, fail-closed value (`AGENTS.md`
    /// §2.9).
    const EXIT_NO_HOST: i32 = 80;

    /// Exit code when the delivered grants do not name the single register
    /// window this driver needs — an unbound or mis-provisioned node
    /// (`AGENTS.md` §18.4 / §5.4). A reserved, fail-closed value.
    const EXIT_NO_RESOURCES: i32 = 81;

    /// Exit code when the device bring-up failed (the register window could
    /// not be mapped, the window is not a virtio-MMIO device, or the device
    /// rejected the virtio init sequence). A reserved, fail-closed value
    /// (`AGENTS.md` §2.9); the console is left without a keyboard, never
    /// wedged.
    const EXIT_BRINGUP_FAILED: i32 = 82;

    /// Events drained from the device per poll. A batch size, not a capacity
    /// (`AGENTS.md` §24.4): undrained events stay queued in the eventq and are
    /// read on the next poll.
    const EVENT_BATCH: usize = 16;

    /// A zeroed [`InputEvent`] used to initialise the poll batch; overwritten
    /// by [`Input::poll`] before it is read.
    const EVENT_ZERO: InputEvent = InputEvent {
        kind: InputEventKind::Key,
        reserved0: 0,
        code: 0,
        value: 0,
    };

    /// The capability set the driver host re-checks up front before issuing a
    /// `mmio_map` / `dma_alloc` / `irq_bind` trap, so a missing grant fails
    /// fast without a round trip. It mirrors the resources the matched node
    /// requested — the register window (`CAP_MMIO_MAP`), the DMA region
    /// (`CAP_MEM_DMA`), and the device interrupt line the report pump parks on
    /// (`CAP_IRQ_BIND`). The kernel is the authority and re-checks every trap
    /// regardless (`AGENTS.md` §5.4): claiming a capability the process was not
    /// granted only fails the trap kernel-side, never widens authority. It
    /// must list every capability the host gates on locally, or the host
    /// short-circuits a real, granted operation before it ever traps.
    fn driver_caps() -> CapabilitySet {
        let mut caps = CapabilitySet::empty();
        caps.insert(CapabilityId::MMIO_MAP);
        caps.insert(CapabilityId::MEM_DMA);
        caps.insert(CapabilityId::IRQ_BIND);
        caps
    }

    /// Program entry point. `rustos-rt`'s `_start` calls it once the runtime
    /// is set up and routes its return value through the `exit` syscall.
    ///
    /// On success this never returns: the report pump runs for the life of the
    /// driver process.
    fn main() -> i32 {
        // Build the host from the grants the kernel minted for this driver.
        // Coherent DMA is carved kernel-side, so no architecture-specific
        // cache-maintenance shim is supplied (`AGENTS.md` §2.20).
        let Ok(host) = RtDriverHost::from_grants_query(driver_caps(), RtGrantSyscalls, None) else {
            return EXIT_NO_HOST;
        };
        // Resolve the single granted register window — the one definition of
        // "which window did the kernel grant me" (`AGENTS.md` §2.2 / §18.3).
        let Ok((base, len)) = sole_register_window(host.resources()) else {
            return EXIT_NO_RESOURCES;
        };
        // Map the register window and build the bus-agnostic transport over it,
        // then bring the virtio-input device online. The host is borrowed as
        // the `VirtioHost` the device carves its event buffers from.
        let Ok(window) = host.map_window(base, len) else {
            return EXIT_BRINGUP_FAILED;
        };
        let Ok(transport) = MmioTransport::new(window) else {
            return EXIT_BRINGUP_FAILED;
        };
        let vhost: &dyn VirtioHost = &host;
        let Ok(mut input) = VirtioInput::open(transport, vhost) else {
            return EXIT_BRINGUP_FAILED;
        };

        // Poll the device forever, resolving each decoded key edge into a
        // `KeyInput` record and injecting it into the input-focus arbiter,
        // yielding between polls so PID 1 and every other task keeps running
        // (`AGENTS.md` §2.1). A `poll` error is non-fatal: the next poll
        // retries rather than dropping the driver.
        let mut console = VirtioKeyboardConsole::new();
        let mut events = [EVENT_ZERO; EVENT_BATCH];
        loop {
            if let Ok(drained) = input.poll(&mut events) {
                for event in &events[..drained] {
                    if let Some(record) = console.feed(*event) {
                        let _ = rustos_rt::key_inject(&record);
                    }
                }
            }
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
