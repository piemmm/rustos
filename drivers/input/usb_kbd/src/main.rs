//! The `Run` entry-point binary of the USB boot-keyboard driver, installed as
//! a signed `/System/Drivers/` bundle and **autoloaded into user space** by
//! `devmgr` when a HID boot-keyboard interface is discovered behind a USB host
//! (`AGENTS.md` §18, `plans/PI.md` P10 chunk 5d-2-ii).
//!
//! This is the "drivers in user space" steady state (`AGENTS.md` §4): the
//! board bus chain (`drivers/bus/pcie_brcm` + `drivers/bus/usb`) brings the
//! controller up and emits the enumerated HID device into the hardware tree,
//! the kernel mints this process exactly the device-resource grants its
//! matched node requested — its already-assigned xHCI register BAR and a DMA
//! constraint, and no more (`AGENTS.md` §4 / §18.3) — and this program reaches
//! them through the rt-backed `RtDriverHost`. It names no board, PCI, or
//! BCM2711 detail (`AGENTS.md` §2.20): it maps a register window by address,
//! carves a DMA region, and speaks the bus-agnostic xHCI protocol via the
//! arch-neutral `rustos_hid` composition.
//!
//! It is a **pure-Rust** program: RustOS is Rust-only (`AGENTS.md` §1), so it
//! links the Rust userland runtime `rustos-rt` — never the C ABI, which exists
//! solely for programs **not** written in Rust (`AGENTS.md` §16.4).
//! `rustos-rt` provides `_start`, the per-process stack canary (`AGENTS.md`
//! §19.2), the panic handler, and the syscall wrappers; `rustos_rt::entry!`
//! names this program's `main`. It is a separate crate from the
//! `rustos-drv-input-usb-hid` driver (which the kernel still links for the
//! transitional in-kernel scaffold) so the userland runtime never enters the
//! kernel's dependency graph.
//!
//! `main` wires the real seams the bring-up and the report pump drive:
//!
//! * `RtDriverHost::from_grants_query` over `RtGrantSyscalls`: the host
//!   learns its kernel-issued grants through the `resource_grants` syscall and
//!   maps/carves them through `mmio_map` / `dma_alloc`. Every capability and
//!   bound is re-checked kernel-side, on the far side of the trap (`AGENTS.md`
//!   §5.4); the host adds no authority. The DMA carve is coherent kernel-side,
//!   so no architecture-specific cache-maintenance shim is supplied here
//!   (`coherency = None`, keeping the program platform-neutral, `AGENTS.md`
//!   §2.20).
//! * `derive_keyboard_resources` over the same delivered grants
//!   (`RtDriverHost::resources`): the register BAR window and the DMA
//!   aperture bound are read from the grants the kernel delivered, never a
//!   build-time board constant (`AGENTS.md` §2.16 / §2.20).
//! * `bring_up_boot_keyboard`: carves the device-shared DMA region (aperture
//!   checked before any register is touched, `AGENTS.md` §5.4), maps the BAR,
//!   brings the xHCI controller up, and enumerates the boot keyboard.
//! * The `KeyInjectSink` over the `key_inject` syscall: each decoded key
//!   edge is injected into the kernel input-focus arbiter, which routes it by
//!   who holds focus (`AGENTS.md` §17.4). The driver no longer chooses the
//!   encoding or the destination.
//!
//! After bring-up `main` polls the keyboard forever with `pump_once`,
//! yielding between polls so the rest of the system runs (`AGENTS.md` §2.1 — a
//! cooperative poll loop, never a hard spin); a `pump_once` error is non-fatal
//! and the next poll retries. A bring-up failure exits with a reserved
//! fail-closed code, leaving the console without a keyboard rather than wedged
//! (`AGENTS.md` §2.9); the spawning supervisor decides whether to relaunch.
//!
//! On the host it is an inert stub so `cargo build --workspace`, clippy, and
//! fmt still cover the file.

#![cfg_attr(freestanding, no_std)]
#![cfg_attr(freestanding, no_main)]
#![deny(missing_docs)]

// --- Pure-Rust program --------------------------------------------------
#[cfg(freestanding)]
mod program {
    use rustos_abi::input::KeyInput;
    use rustos_abi::{CapabilityId, DriverError};
    use rustos_caps::CapabilitySet;
    use rustos_drvrt::{RtDriverHost, RtGrantSyscalls};
    use rustos_hid::{
        bring_up_boot_keyboard, derive_keyboard_resources, pump_once, ConsoleSink, KeyboardConsole,
    };
    use rustos_rt::ClockDelay;

    /// Exit code when the rt-backed driver host could not be built from the
    /// kernel-delivered grants (the `resource_grants` query was refused or the
    /// delivery did not fit). A reserved, fail-closed value (`AGENTS.md`
    /// §2.9).
    const EXIT_NO_HOST: i32 = 80;

    /// Exit code when the delivered grants do not name the register BAR and a
    /// DMA constraint this driver needs — an unbound or mis-provisioned node
    /// (`AGENTS.md` §18.4 / §5.4). A reserved, fail-closed value.
    const EXIT_NO_RESOURCES: i32 = 81;

    /// Exit code when the controller/keyboard bring-up failed (no USB
    /// function, a DMA carve outside the aperture, a mapping failure, or an
    /// empty enumeration). A reserved, fail-closed value (`AGENTS.md` §2.9);
    /// the console is left without a keyboard, never wedged.
    const EXIT_BRINGUP_FAILED: i32 = 82;

    /// The capability set the driver host re-checks up front before issuing a
    /// `mmio_map` / `dma_alloc` trap, so a missing grant fails fast without a
    /// round trip. It mirrors the resources the matched node requested — the
    /// register BAR (`CAP_MMIO_MAP`) and the DMA region (`CAP_MEM_DMA`). The
    /// kernel is the authority and re-checks every trap regardless
    /// (`AGENTS.md` §5.4): claiming a capability the process was not granted
    /// only fails the trap kernel-side, never widens authority.
    fn driver_caps() -> CapabilitySet {
        let mut caps = CapabilitySet::empty();
        caps.insert(CapabilityId::MMIO_MAP);
        caps.insert(CapabilityId::MEM_DMA);
        caps
    }

    /// A [`ConsoleSink`] that injects each decoded keyboard record into the
    /// kernel input-focus arbiter through the `key_inject` syscall.
    ///
    /// The user-space counterpart of the in-kernel `ArbiterConsoleSink`
    /// (`AGENTS.md` §2.2): [`pump_once`] hands it one whole [`KeyInput`]
    /// record's wire bytes per key edge; it decodes them fail-closed and
    /// injects the record. The kernel validates `CAP_INPUT_INJECT` and routes
    /// the record by who holds input focus (`AGENTS.md` §17.4). A malformed
    /// record or a refused injection surfaces as [`DriverError::DeviceFault`]
    /// rather than silently dropping input (`AGENTS.md` §2.9); the pump loop
    /// treats it as a non-fatal poll error and retries.
    struct KeyInjectSink;

    impl ConsoleSink for KeyInjectSink {
        fn write(&mut self, bytes: &[u8]) -> Result<(), DriverError> {
            let record = KeyInput::from_bytes(bytes).map_err(|_| DriverError::DeviceFault)?;
            if rustos_rt::key_inject(&record) < 0 {
                return Err(DriverError::DeviceFault);
            }
            Ok(())
        }
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
        // Derive the BAR window and DMA aperture from the same delivered
        // grants the host maps over — no build-time board constant, no second
        // `resource_grants` syscall (`AGENTS.md` §2.16 / §2.20).
        let Ok(resources) = derive_keyboard_resources(host.resources()) else {
            return EXIT_NO_RESOURCES;
        };
        // The one userland clock-backed `Delay` for the hardware-dictated
        // hub settle windows (`AGENTS.md` §2.2).
        let delay = ClockDelay::new();
        let Ok(mut keyboard) = bring_up_boot_keyboard(
            &host,
            &delay,
            resources.bar_base,
            resources.bar_len,
            resources.dma_aperture_top,
        ) else {
            return EXIT_BRINGUP_FAILED;
        };

        // Poll the keyboard forever, injecting each decoded key edge into the
        // input-focus arbiter and yielding between polls so PID 1 and every
        // other task keeps running (`AGENTS.md` §2.1). A `pump_once` error is
        // non-fatal: the next poll retries rather than dropping the driver.
        let mut console = KeyboardConsole::new();
        let mut sink = KeyInjectSink;
        loop {
            let _ = pump_once(&mut keyboard, &mut console, &mut sink);
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
