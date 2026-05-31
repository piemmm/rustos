//! Stage 3b QEMU integration test: boot the aarch64 `virt` board to a
//! placeholder init.
//!
//! ## What this test asserts
//!
//! The Stage-3 per-sub-stage checklist requires that each architecture
//! "boots to `init`" in QEMU. This binary exercises exactly that path on
//! the aarch64 `virt` board, end to end:
//!
//! 1. The arch crate's `boot.s` trampoline drops to EL1 (if entered at
//!    EL2), establishes a stack, zeroes `.bss`, and calls `kernel_main`
//!    with the DTB pointer.
//! 2. `kernel_main` logs a record over the PL011 UART (proving the
//!    console works) and reaches the placeholder init point.
//! 3. It reports PASS through the ARM semihosting `SYS_EXIT` finisher,
//!    which exits QEMU with status `0` — the host runner's success
//!    condition.
//!
//! A regression that fails to boot never reaches the finisher, so the
//! run times out and the harness reports `Outcome::Timeout` — the
//! documented fail-loud behaviour (`AGENTS.md` §7).
//!
//! ## How it differs from a production kernel
//!
//! It links only the `rustos-arch-aarch64` port (the boot path needs no
//! `kernel/*` subsystem) and supplies its own `kernel_main`. The
//! QEMU-exit shortcut lives in this dedicated bin, never behind a Cargo
//! feature on the arch crate (`AGENTS.md` §5.4.5 — fail closed).

#![cfg_attr(itest_aarch64, no_std)]
#![cfg_attr(itest_aarch64, no_main)]
#![deny(missing_docs)]

// --- Freestanding test bin (`aarch64-unknown-none`) ----------------

#[cfg(itest_aarch64)]
mod kernel {
    use core::panic::PanicInfo;

    use rustos_arch_aarch64::{handle_panic_via_serial, qemu_exit, SERIAL_SINK};
    use rustos_log::{log, Event, EventId, Level};

    /// Stable audit-event ids for the QEMU transcript.
    const BOOT_TEST_START: EventId = EventId(4210);
    const BOOT_TEST_PASS: EventId = EventId(4211);

    /// Forward to the shared aarch64 panic bridge (parks the CPU; the run
    /// then times out and the harness reports the failure).
    #[panic_handler]
    fn rustos_boot_aarch64_panic(info: &PanicInfo<'_>) -> ! {
        handle_panic_via_serial(info)
    }

    /// Boot entry point — the symbol the arch crate's `boot.s`
    /// trampoline calls (via `rustos_arch_aarch64_main`).
    #[no_mangle]
    pub extern "C" fn kernel_main(_dtb: u64) -> ! {
        log(
            &SERIAL_SINK,
            &Event {
                level: Level::Info,
                id: BOOT_TEST_START,
                message: "aarch64 boot test: reached kernel_main at EL1",
                fields: &[],
            },
        );

        // Placeholder init: a real kernel would hand off to
        // `kernel_core::kernel_main` here. The Stage-3 deliverable is
        // only that control reaches a Rust init point with a working
        // console, which the log above proves.
        log(
            &SERIAL_SINK,
            &Event {
                level: Level::Info,
                id: BOOT_TEST_PASS,
                message: "aarch64 boot test: reached placeholder init",
                fields: &[],
            },
        );
        qemu_exit::exit_success();
    }
}

// --- Host stub -----------------------------------------------------
#[cfg(not(itest_aarch64))]
fn main() {}

#[cfg(not(itest_aarch64))]
#[allow(dead_code)]
fn _suppress_no_main() {}
