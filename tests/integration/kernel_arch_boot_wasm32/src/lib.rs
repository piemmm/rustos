//! Stage 3d browser-headless integration test for the wasm32 Arch HAL.
//!
//! Built for `wasm32-unknown-unknown` this `cdylib` is the wasm32
//! analogue of the bare-metal `kernel_arch_boot_*` QEMU verticals. It
//! exercises the three Stage-3 deliverables (`PLAN.md`) in a real
//! browser:
//!
//! * **Boots to `init`.** `kernel_main` brings the `WasmArch` handle up
//!   and prints `BOOT_OK`.
//! * **Memory-isolation test passes.** `kernel_main` builds a victim and
//!   an attacker `AddressSpace` over disjoint WASM-linear-memory regions
//!   and confirms the attacker faults on a victim-only address, printing
//!   `ISOLATION_OK`.
//! * **Timer interrupt drives the scheduler.** `kernel_main` installs a
//!   tick callback and arms cooperative preemption; the host's
//!   `requestAnimationFrame` loop calls the exported `on_frame` each
//!   frame, which drives the callback and prints `TICK`.
//!
//! The browser harness (`web/harness.mjs`, launched by `cargo xtask test
//! --wasm`) scrapes those console markers and reports PASS once it has
//! seen `BOOT_OK`, `ISOLATION_OK`, and at least twenty `TICK`s; any panic
//! traps the instance and fails the run loudly (`AGENTS.md` §7).
//!
//! On a host build (`itest_wasm32` off) this compiles to an inert empty
//! `cdylib`, exactly as the bare-metal verticals are inert host stubs.
#![cfg_attr(itest_wasm32, no_std)]
#![deny(missing_docs)]

#[cfg(itest_wasm32)]
mod kernel {
    use core::panic::PanicInfo;

    use rustos_arch_api::CpuId;
    use rustos_arch_wasm32::console::write_line;
    use rustos_arch_wasm32::isolation::{AddressSpace, MemoryRegion};
    use rustos_arch_wasm32::{handle_panic_via_console, preempt, WasmArch};

    /// Forward this module's panics to the shared console bridge, which
    /// emits one record and traps the instance (`AGENTS.md` §2.9).
    #[panic_handler]
    fn panic(info: &PanicInfo<'_>) -> ! {
        handle_panic_via_console(info)
    }

    /// The cooperative scheduler tick the host frame loop drives. Prints
    /// one `TICK` marker per frame; the harness counts them to prove the
    /// `requestAnimationFrame` loop reaches the scheduler.
    extern "C" fn on_tick(_cpu: CpuId) {
        write_line("TICK");
    }

    /// Boot body the arch crate's exported `rustos_arch_wasm32_main`
    /// trampoline (`kernel/arch/wasm32::entry`) forwards to once the host
    /// has instantiated the module.
    ///
    /// Mirrors the bare-metal ports' `kernel_main`, but returns so the
    /// host event loop can drive the cooperative scheduler. Defining it
    /// here (rather than a bespoke `boot` export) exercises the real
    /// `entry::rustos_arch_wasm32_main` seam end-to-end.
    #[no_mangle]
    pub extern "C" fn kernel_main() {
        let arch = WasmArch::new(0);
        // Constructing the Arch HAL handle proves the port is live; the
        // SMP map is exercised by the host unit tests.
        let _ = arch;
        write_line("BOOT_OK");

        run_isolation_check();

        preempt::set_tick_callback(on_tick);
        preempt::init_local_preempt(0);
    }

    /// Prove the WASM-linear-memory isolation model denies a cross-context
    /// access. Panics (trapping the instance) if isolation fails, so a
    /// regression cannot silently report success (`AGENTS.md` §5.4.5).
    fn run_isolation_check() {
        let victim = AddressSpace::new(MemoryRegion::new(0x10_0000, 0x1000));
        let attacker = AddressSpace::new(MemoryRegion::new(0x20_0000, 0x1000));
        let secret = 0x10_0800; // inside the victim, outside the attacker.

        assert!(victim.can_read(secret), "victim must own its own page");
        assert!(
            attacker.check_access(secret, 1).is_err(),
            "attacker must fault on the victim-only page"
        );
        assert!(
            attacker.check_access(0x20_0000, 0x1000).is_ok(),
            "attacker must reach its own region"
        );
        write_line("ISOLATION_OK");
    }
}
