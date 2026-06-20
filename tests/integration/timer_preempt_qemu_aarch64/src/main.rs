//! Stage 3b QEMU integration test: the aarch64 generic-timer interrupt
//! drives the scheduler-tick callback.
//!
//! ## What this test asserts
//!
//! The Stage-3 per-sub-stage checklist requires that "the timer
//! interrupt drives the scheduler" on each architecture. This binary
//! exercises exactly that path on the aarch64 `virt` board, end to end:
//!
//! 1. Read the counter frequency from `CNTFRQ_EL0`.
//! 2. Install a tick callback through the Arch HAL
//!    `rustos_arch_aarch64::timer_hal::TimerHal` (`rustos_arch_api::Timer`)
//!    that counts each generic-timer interrupt; the IRQ path dispatches
//!    back through the same HAL handle (`AGENTS.md` §17.2).
//! 3. Install the EL1 exception vector table
//!    (`rustos_arch_aarch64::exceptions::init_vectors`) and initialise the
//!    GICv2 (`rustos_arch_aarch64::gic::init`).
//! 4. Record the per-quantum interval and enable the GIC PPI
//!    (`rustos_arch_aarch64::preempt::init_local_preempt` leaves the timer
//!    **disarmed** — RustOS is tickless, `AGENTS.md` §17.1), then arm the
//!    first **one-shot** (`preempt::arm_oneshot`) and unmask IRQs at the
//!    PE (`exceptions::enable_irq`).
//! 5. Spin on `wfi`; the tick callback re-arms the next one-shot
//!    (`preempt::arm_oneshot`) on every fire, so the timer ISR path is
//!    exercised `TARGET_TICKS` times under explicit scheduler-style
//!    re-arming (there is no periodic auto-reload). Once the callback has
//!    fired `TARGET_TICKS` times, report PASS through the ARM semihosting
//!    finisher.
//!
//! A regression that fails to deliver the one-shot or whose callback
//! fails to re-arm never reaches `TARGET_TICKS`, so the run times out and
//! the harness reports `Outcome::Timeout` — the documented fail-loud
//! behaviour (`AGENTS.md` §7).
//!
//! ## How it differs from a production kernel
//!
//! It links only the `rustos-arch-aarch64` port (the timer path needs no
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
    use core::sync::atomic::{AtomicU64, Ordering};

    use rustos_arch_aarch64::kernel_arch::read_cntfrq;
    use rustos_arch_aarch64::timer_hal::TimerHal;
    use rustos_arch_aarch64::{
        exceptions, gic, handle_panic_via_serial, preempt, qemu_exit, SERIAL_SINK,
    };
    use rustos_arch_api::{CpuId, Timer};
    use rustos_log::{log, Event, EventId, Level};

    /// Scheduler-tick frequency to drive the timer at.
    const TICK_HZ: u64 = 100;

    /// Number of timer ticks to observe before declaring success. Large
    /// enough that a single spurious interrupt cannot pass the test, yet
    /// reached well within the harness budget at 100 Hz.
    const TARGET_TICKS: u64 = 20;

    /// Stable audit-event ids for the QEMU transcript.
    const TIMER_TEST_START: EventId = EventId(4220);
    const TIMER_TEST_PASS: EventId = EventId(4221);

    /// Count of generic-timer interrupts the callback has serviced.
    static TICKS: AtomicU64 = AtomicU64::new(0);

    /// Per-quantum interval (counter ticks) the one-shot is re-armed to,
    /// published before IRQs are unmasked so the callback can read it.
    static INTERVAL: AtomicU64 = AtomicU64::new(0);

    /// The scheduler-tick callback the timer IRQ path invokes. RustOS is
    /// tickless (`AGENTS.md` §17.1): the one-shot does not auto-reload, so
    /// the callback re-arms the next one-shot itself — standing in for the
    /// scheduler's `set_preemption` on a contended CPU. A real scheduler
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

    /// Forward to the shared aarch64 panic bridge (parks the CPU; the run
    /// then times out and the harness reports the failure).
    #[panic_handler]
    fn rustos_timer_preempt_aarch64_panic(info: &PanicInfo<'_>) -> ! {
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
                id: TIMER_TEST_START,
                message: "aarch64 timer-preempt test: arming generic timer",
                fields: &[],
            },
        );

        // 1. Read the counter frequency the timer interval is derived from.
        let counter_hz = read_cntfrq();
        if counter_hz == 0 {
            // Fail closed (park → timeout) rather than dividing by zero.
            log(
                &SERIAL_SINK,
                &Event {
                    level: Level::Error,
                    id: TIMER_TEST_START,
                    message: "CNTFRQ_EL0 reports a zero frequency",
                    fields: &[],
                },
            );
            qemu_exit::exit_failure(1);
        }

        // 2. Install the tick callback through the Arch HAL timer handle
        //    before any timer can fire.
        TimerHal::new().set_tick_callback(on_tick);

        // 3. Vector table + GIC bring-up.
        // SAFETY: called once on the boot CPU with a stack established and
        // before any source is armed; the callback is installed.
        unsafe {
            exceptions::init_vectors();
            gic::init();
        }

        // 4. Register the per-CPU preemption backing (sized to this
        //    single-CPU vertical, `AGENTS.md` §24.1) so the timer slot
        //    `init_local_preempt` records exists, then arm the EL1
        //    physical timer at TICK_HZ and unmask IRQs.
        static PREEMPT_STORAGE: preempt::PreemptStorage<1> = preempt::PreemptStorage::new();
        if PREEMPT_STORAGE.register().is_err() {
            qemu_exit::exit_failure(2);
        }
        let interval = preempt::interval_for_hz(counter_hz, TICK_HZ);
        INTERVAL.store(interval, Ordering::Relaxed);
        // SAFETY: `cpu` is the boot CPU's id, the callback is installed,
        // the per-CPU storage is registered, and the vector table and GIC
        // are up (step 3). `init_local_preempt` records the interval and
        // enables the PPI but leaves the timer disarmed (tickless); we arm
        // the first one-shot explicitly, after which `on_tick` re-arms each
        // fire.
        unsafe {
            preempt::init_local_preempt(0, interval);
            preempt::arm_oneshot(interval);
            exceptions::enable_irq();
        }

        // 5. Idle until the timer has driven the callback TARGET_TICKS
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
                message: "aarch64 timer-preempt test: scheduler tick driven",
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
