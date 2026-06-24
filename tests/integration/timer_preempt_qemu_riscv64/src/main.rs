//! Stage 3c QEMU integration test: the riscv64 supervisor-timer
//! interrupt drives the scheduler-tick callback.
//!
//! ## What this test asserts
//!
//! The Stage-3 per-sub-stage checklist requires that "the timer
//! interrupt drives the scheduler" on each architecture. This binary
//! exercises exactly that path on the riscv64 `virt` board, end to end:
//!
//! 1. Read the `/cpus` `timebase-frequency` from the device tree.
//! 2. Install a tick callback through the Arch HAL
//!    `rustos_arch_riscv64::timer_hal::TimerHal` (`rustos_arch_api::Timer`)
//!    that counts each supervisor-timer interrupt; the trap path
//!    dispatches back through the same HAL handle.
//! 3. Install the S-mode trap vector and enable interrupts
//!    (`rustos_arch_riscv64::trap::init_traps`).
//! 4. Record the per-quantum interval and enable `sie.STIE`
//!    (`rustos_arch_riscv64::preempt::init_local_preempt` leaves the timer
//!    **disarmed** — RustOS is tickless), then arm the
//!    first **one-shot** (`preempt::arm_oneshot`).
//! 5. Spin on `wfi`; the tick callback re-arms the next one-shot
//!    (`preempt::arm_oneshot`) on every fire, so the timer trap path is
//!    exercised `TARGET_TICKS` times under explicit scheduler-style
//!    re-arming (there is no periodic auto-reload). Once the callback has
//!    fired `TARGET_TICKS` times, write the `SiFive` Test PASS finisher.
//!
//! A regression that fails to deliver the one-shot or whose callback
//! fails to re-arm never reaches `TARGET_TICKS`, so the run times out and
//! the harness reports `Outcome::Timeout` — the documented fail-loud
//! behaviour.
//!
//! ## How it differs from a production kernel
//!
//! It links only the `rustos-arch-riscv64` port (the timer path needs
//! no `kernel/*` subsystem) and supplies its own `kernel_main`. The
//! QEMU-exit shortcut lives in this dedicated bin, never behind a Cargo
//! feature on the arch crate (fail closed).

#![cfg_attr(itest_riscv64, no_std)]
#![cfg_attr(itest_riscv64, no_main)]
#![deny(missing_docs)]

// --- Freestanding test bin (`riscv64gc-unknown-none-elf`) ----------

#[cfg(itest_riscv64)]
mod kernel {
    use core::panic::PanicInfo;
    use core::sync::atomic::{AtomicU64, Ordering};

    use rustos_arch_api::{CpuId, Timer};
    use rustos_arch_riscv64::fdt::Fdt;
    use rustos_arch_riscv64::timer_hal::TimerHal;
    use rustos_arch_riscv64::{
        halt_current_hart, handle_panic_via_serial, preempt, qemu_exit, trap, SERIAL_SINK,
    };
    use rustos_log::{log, Event, EventId, Level};

    /// Scheduler-tick frequency to drive the timer at.
    const TICK_HZ: u64 = 100;

    /// Number of timer ticks to observe before declaring success. Large
    /// enough that a single spurious interrupt cannot pass the test, yet
    /// reached well within the harness budget at 100 Hz.
    const TARGET_TICKS: u64 = 20;

    /// Stable audit-event ids for the QEMU transcript.
    const TIMER_TEST_START: EventId = EventId(4200);
    const TIMER_TEST_PASS: EventId = EventId(4201);

    /// Count of supervisor-timer interrupts the callback has serviced.
    static TICKS: AtomicU64 = AtomicU64::new(0);

    /// Per-quantum interval (`time`-CSR ticks) the one-shot is re-armed to,
    /// published before interrupts are enabled so the callback can read it.
    static INTERVAL: AtomicU64 = AtomicU64::new(0);

    /// The scheduler-tick callback the timer trap path invokes. RustOS is
    /// tickless: the one-shot does not auto-reload, so
    /// the callback re-arms the next one-shot itself — standing in for the
    /// scheduler's `set_preemption` on a contended hart. A real scheduler
    /// would `Scheduler::on_timer_tick(cpu)` here; the test only needs to
    /// prove the interrupt is delivered and that re-arming drives the next
    /// fire.
    extern "C" fn on_tick(_cpu: CpuId) {
        TICKS.fetch_add(1, Ordering::Relaxed);
        let interval = INTERVAL.load(Ordering::Relaxed);
        if interval != 0 {
            preempt::arm_oneshot(interval);
        }
    }

    /// Forward to the shared riscv64 panic bridge (parks the hart; the
    /// run then times out and the harness reports the failure).
    #[panic_handler]
    fn rustos_timer_preempt_riscv64_panic(info: &PanicInfo<'_>) -> ! {
        handle_panic_via_serial(info)
    }

    /// Boot entry point — the symbol the arch crate's `boot.s`
    /// trampoline calls (via `rustos_arch_riscv64_main`).
    #[no_mangle]
    pub extern "C" fn kernel_main(_hartid: u64, dtb: u64) -> ! {
        log(
            &SERIAL_SINK,
            &Event {
                level: Level::Info,
                id: TIMER_TEST_START,
                message: "riscv64 timer-preempt test: arming SBI timer",
                fields: &[],
            },
        );

        // 1. Read the timer frequency. Fail closed (park → timeout) if
        //    the device tree omits it, rather than guessing a divisor.
        // SAFETY: `dtb` is the verbatim `a1` pointer OpenSBI handed the
        // boot hart; `boot.s` forwards it unchanged.
        let timebase = match unsafe { Fdt::from_ptr(dtb as *const u8) }
            .ok()
            .and_then(|f| f.timebase_frequency())
        {
            Some(hz) => hz,
            None => {
                log(
                    &SERIAL_SINK,
                    &Event {
                        level: Level::Error,
                        id: TIMER_TEST_START,
                        message: "no timebase-frequency in device tree",
                        fields: &[],
                    },
                );
                halt_current_hart();
            }
        };

        // 2. Install the tick callback through the Arch HAL timer handle
        //    before any timer can fire.
        TimerHal::new().set_tick_callback(on_tick);

        // 3. Trap vector + global interrupt enable.
        // SAFETY: called once on the boot hart with a stack established
        // and before any source is armed; the callback is installed.
        unsafe {
            trap::init_traps();
        }

        // 4. Register the per-hart preemption backing sized to this
        //    single-hart vertical before arming the timer; the per-hart
        //    interval/`CpuId` slots are caller-owned storage scaled to the
        //    hart count, not a fixed `const`.
        static PREEMPT_STORAGE: preempt::PreemptStorage<1> = preempt::PreemptStorage::new();
        if PREEMPT_STORAGE.register().is_err() {
            halt_current_hart();
        }

        // 5. Arm the first one-shot SBI timer; `init_local_preempt` enables
        //    `sie.STIE` but leaves the timer disarmed (tickless), and
        //    `on_tick` re-arms each fire.
        let interval = preempt::interval_for_hz(timebase, TICK_HZ);
        INTERVAL.store(interval, Ordering::Relaxed);
        // SAFETY: `cpu` is the boot hart's id, the callback is installed,
        // the per-hart storage is registered, and the trap vector is in
        // place (step 3).
        unsafe {
            preempt::init_local_preempt(0, interval);
            preempt::arm_oneshot(interval);
        }

        // 6. Idle until the timer has driven the callback TARGET_TICKS
        //    times, then report PASS.
        while TICKS.load(Ordering::Relaxed) < TARGET_TICKS {
            // SAFETY: `wfi` is a wait-for-interrupt hint with no
            // architectural side effects; the timer interrupt wakes it.
            unsafe {
                core::arch::asm!("wfi", options(nomem, nostack, preserves_flags));
            }
        }

        log(
            &SERIAL_SINK,
            &Event {
                level: Level::Info,
                id: TIMER_TEST_PASS,
                message: "riscv64 timer-preempt test: scheduler tick driven",
                fields: &[],
            },
        );
        qemu_exit::exit_success();
    }
}

// --- Host stub -----------------------------------------------------
#[cfg(not(itest_riscv64))]
fn main() {}

#[cfg(not(itest_riscv64))]
#[allow(dead_code)]
fn _suppress_no_main() {}
