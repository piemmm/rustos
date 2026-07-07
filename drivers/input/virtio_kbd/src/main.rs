//! The `Run` entry-point binary of the virtio-input keyboard driver, installed
//! as a signed `/System/Drivers/` bundle and **autoloaded into user space** by
//! `devmgr` when a virtio-input device is discovered (
//! `plans/PI.md` P10 chunk 5d-2-ii).
//!
//! This is the "drivers in user space" steady state on the
//! hardware QEMU `-M virt` actually presents (a virtio-input keyboard; the
//! metal Pi 4 keyboard is the USB `drivers/input/usb_kbd`). The kernel mints
//! this process exactly the device-resource grants its matched node requested
//! — the device's register window and a DMA constraint, and no more — and this program reaches them through the
//! rt-backed `RtDriverHost`. It names no board, bus, or transport detail: it maps a register window by address, carves a DMA
//! region, and speaks the bus-agnostic virtio split-virtqueue protocol via the
//! arch-neutral `rustos_virtio_input` composition over the `rustos_virtio`
//! MMIO transport.
//!
//! It is a **pure-Rust** program: RustOS is Rust-only, so it
//! links the Rust userland runtime `rustos-rt`, never the C ABI (which exists
//! solely for non-Rust programs). `rustos-rt` provides
//! `_start`, the per-process stack canary, the panic
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
//!   bound is re-checked kernel-side, on the far side of the trap; the host adds no authority. The DMA carve is coherent kernel-side
//!   (the QEMU `virt` virtio interconnect snoops the CPU caches), so no
//!   architecture-specific cache-maintenance shim is supplied here
//!   (`coherency = None`, keeping the program platform-neutral).
//! * `sole_register_window` over the delivered grants: the device's register
//!   window `(base, len)` is read from the grants the kernel delivered, never a
//!   build-time board constant.
//! * `MmioTransport::new` over the mapped window, then
//!   `VirtioInput::open_armed`: brings the virtio-input device online, posts
//!   its event queue, and only then binds the granted interrupt (the arm
//!   step), so readiness is never advertised before the device can accept a
//!   keystroke.
//! * The poll/feed/inject loop: each decoded `InputEvent` key edge is
//!   resolved into a `KeyInput` record by `VirtioKeyboardConsole` and
//!   injected into the kernel input-focus arbiter through the `key_inject`
//!   syscall, which routes it by who holds focus. The
//!   driver no longer chooses the encoding or the destination.
//!
//! After bring-up `main` pumps the device forever, **parking on the granted
//! device interrupt** between events: `VirtioInput::poll` waits through the
//! host's `notify_wait` (the kernel `irq_wait` park) and acknowledges the
//! device each cycle, so an idle keyboard costs no CPU — never a yield-poll
//! loop. The interrupt bind is the `open_armed` arm step, issued only once
//! the device's eventq is live (buffers posted, device kicked): the audited
//! `irq_bind` syscall is the kernel-observable readiness witness, and binding
//! any earlier would advertise a keyboard that can still silently drop a
//! keystroke. A bind failure or a hard poll fault
//! exits fail-loud; a broken device is never retried in a spin.
//! A bring-up failure exits with a reserved fail-closed code, leaving the
//! console without a keyboard rather than wedged; the
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
    /// delivery did not fit). A reserved, fail-closed value.
    const EXIT_NO_HOST: i32 = 80;

    /// Exit code when the delivered grants do not name the single register
    /// window this driver needs — an unbound or mis-provisioned node. A reserved, fail-closed value.
    const EXIT_NO_RESOURCES: i32 = 81;

    /// Exit code when the device bring-up failed (the register window could
    /// not be mapped, the window is not a virtio-MMIO device, the device
    /// rejected the virtio init sequence, or the granted interrupt line
    /// could not be bound after the device came up — the event pump parks
    /// on it, so a driver that cannot bind it would degrade into the busy
    /// re-poll the charter forbids). A reserved, fail-closed value; the
    /// console is left without a keyboard, never wedged.
    const EXIT_BRINGUP_FAILED: i32 = 82;

    /// Exit code when the running device faulted (a corrupted completion or
    /// descriptor). Structural, never transient: the driver exits fail-loud
    /// rather than spin retrying a broken device. A reserved value.
    const EXIT_DEVICE_FAULT: i32 = 83;

    /// Events drained from the device per poll. A batch size, not a capacity: undrained events stay queued in the eventq and are
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
    /// regardless: claiming a capability the process was not
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
        // cache-maintenance shim is supplied.
        let Ok(host) = RtDriverHost::from_grants_query(driver_caps(), RtGrantSyscalls, None) else {
            return EXIT_NO_HOST;
        };
        // Resolve the single granted register window — the one definition of
        // "which window did the kernel grant me".
        let Ok((base, len)) = sole_register_window(host.resources()) else {
            return EXIT_NO_RESOURCES;
        };
        // Map the register window and build the bus-agnostic transport over it,
        // then bring the virtio-input device online. The host is borrowed as
        // the `VirtioHost` the device carves its event buffers from.
        // The interrupt bind is the *arm* step of `open_armed`, run strictly
        // after the eventq is live: the audited `irq_bind` syscall is the
        // kernel-observable "keyboard ready" witness, and binding before the
        // device has posted buffers advertises readiness while a keystroke
        // can still be silently dropped against an un-ready device. The bind
        // stays mandatory and fail-loud — the event pump parks on this line
        // between keystrokes, so a driver that cannot bind it must exit here
        // rather than silently degrade into a busy re-poll; on that failure
        // `open_armed` has already reset the device.
        let Ok(window) = host.map_window(base, len) else {
            return EXIT_BRINGUP_FAILED;
        };
        let Ok(transport) = MmioTransport::new(window) else {
            return EXIT_BRINGUP_FAILED;
        };
        let vhost: &dyn VirtioHost = &host;
        let Ok(mut input) = VirtioInput::open_armed(transport, vhost, |_| host.bind_irq()) else {
            return EXIT_BRINGUP_FAILED;
        };

        // Pump the device forever, resolving each decoded key edge into a
        // `KeyInput` record and injecting it into the input-focus arbiter.
        // `poll` parks on the bound device interrupt while nothing is
        // pending (and acknowledges the device each cycle), so an idle
        // keyboard holds the task off the run queue — no yield loop. An
        // empty return is a spurious wake and simply re-parks; a hard fault
        // is structural and exits fail-loud rather than spinning on a
        // broken device.
        let mut console = VirtioKeyboardConsole::new();
        let mut events = [EVENT_ZERO; EVENT_BATCH];
        loop {
            match input.poll(&mut events) {
                Ok(drained) => {
                    for event in &events[..drained] {
                        if let Some(record) = console.feed(*event) {
                            let _ = rustos_rt::key_inject(&record);
                        }
                    }
                }
                Err(_) => return EXIT_DEVICE_FAULT,
            }
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
