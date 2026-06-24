//! SPAWN Stage SP1 QEMU integration test: two kernel-thread tasks
//! ping-pong through the **real** Arch HAL `ContextSwitch::switch` under
//! the live `kernel/sched` scheduler on the riscv64 `virt` board.
//!
//! ## What this test asserts
//!
//! `plans/SPAWN.md` Stage SP1 requires the `kernel/core` kthread runtime
//! to make a scheduler task a *resumable kernel thread* with its own
//! kernel stack, driven through the existing `ContextSwitch` HAL, and to
//! prove it on every bare-metal board by having **two** kthreads ping-pong
//! via the real `ContextSwitch::switch`. The aarch64 vertical landed first
//! (`tests/integration/kthread_switch_qemu_aarch64`); this is the riscv64
//! sibling, so the runtime is a *production* scheduling path on riscv64
//! too — until now `ContextSwitch::switch` was exercised here only by the
//! W7 `sched_drive_qemu_riscv64` round-trip.
//!
//! This binary exercises that on the `virt` board:
//!
//! 1. **Discovered timebase.** It reads the generic-timer frequency from
//!    the firmware device tree (the verbatim `a1` pointer OpenSBI hands the
//!    boot hart) and fails closed if the tree omits it, rather than
//!    guessing a divisor.
//! 2. **Live scheduler + two kthreads.** It builds a real
//!    `rustos_kernel_sched_eevdf::Scheduler` over `RiscvArch` and spawns
//!    two kthreads through `rustos_kernel_core::spawn_kthread`. Each kthread
//!    body runs on its own kernel stack and calls `Yielder::yield_now`
//!    `PING_PONGS` times — each yield is a real `ContextSwitch::switch`
//!    back to the dispatcher — then returns (`Exit`).
//! 3. **Ping-pong + drain.** It drives the cooperative `step` loop;
//!    `Scheduler::spawn` raises a (self-)IPI on the home CPU via the SBI
//!    IPI extension, which a hart with supervisor interrupts masked simply
//!    leaves pending (dispatch here is the cooperative `step` loop, so the
//!    kthread switches are the only mechanism under test). PASS once both
//!    kthreads have run exactly `PING_PONGS` times and both have exited (no
//!    task left live).
//!
//! A regression in the runtime (a switch that does not transfer control, a
//! task that never resumes, a stack that is not reclaimed) either trips a
//! dedicated failure finisher or never drains the workload, so the run
//! fails loudly — by an explicit failure code or by the harness
//! `Outcome::Timeout`.
//!
//! ## How it differs from a production kernel
//!
//! It links the `rustos-kernel-core` kthread runtime, the
//! `rustos-arch-riscv64` port, and the default `rustos-kernel-sched-eevdf`
//! policy directly and supplies its own `kernel_main`, so the runtime is
//! exercised without the full `kernel_core::kernel_main` init pipeline. The
//! QEMU-exit shortcut lives in this dedicated bin, never behind a Cargo
//! feature on a library crate (fail closed).

#![cfg_attr(itest_riscv64, no_std)]
#![cfg_attr(itest_riscv64, no_main)]
#![deny(missing_docs)]

// --- Freestanding test bin (`riscv64gc-unknown-none-elf`) ----------

#[cfg(itest_riscv64)]
extern crate alloc;

#[cfg(itest_riscv64)]
mod kernel {
    use core::panic::PanicInfo;
    use core::sync::atomic::{AtomicU64, Ordering};

    use alloc::sync::Arc;

    use rustos_arch_api::CpuId;
    use rustos_arch_riscv64::context_hal::ContextSwitchHal;
    use rustos_arch_riscv64::fdt::Fdt;
    use rustos_arch_riscv64::{
        handle_panic_via_serial, qemu_exit, RiscvArch, RiscvArchStorage, SERIAL_SINK,
    };
    use rustos_kalloc::{FreeListAllocator, Heap, HEAP_BYTES};
    use rustos_kernel_core::spawn_kthread;
    use rustos_kernel_sched_eevdf::{Priority, Scheduler, SchedulerConfig};
    use rustos_log::{log, Event, EventId, Level};

    /// The single-hart slice runs logical CPU 0 on the boot hart.
    const BOOT_CPU: CpuId = 0;

    /// Times each kthread yields back to the dispatcher before exiting.
    /// Large enough that a single accidental run cannot satisfy the PASS
    /// check, small enough to drain well within the harness budget.
    const PING_PONGS: u64 = 32;

    /// Cooperative-loop watchdog: maximum `step` iterations before the
    /// test declares the workload deadlocked. Sized generously for QEMU
    /// TCG; the real drain is a few hundred steps.
    const MAX_STEPS: u64 = 5_000_000;

    /// Stable audit-event ids for the QEMU transcript.
    const TEST_START: EventId = EventId(4270);
    const TEST_SPAWNED: EventId = EventId(4271);
    const TEST_PASS: EventId = EventId(4272);

    /// Failure finisher code: the device tree advertised no timebase.
    const FAIL_NO_TIMEBASE: u16 = 1;
    /// Failure finisher code: the boot hart was not hart 0.
    const FAIL_UNEXPECTED_HART: u16 = 2;
    /// Failure finisher code: the scheduler could not be constructed.
    const FAIL_SCHED_NEW: u16 = 3;
    /// Failure finisher code: a kthread failed to spawn.
    const FAIL_SPAWN: u16 = 4;
    /// Failure finisher code: the cooperative loop did not drain the
    /// kthreads (a switch that never resumed, e.g.).
    const FAIL_DEADLOCK: u16 = 5;
    /// Failure finisher code: a kthread's run count disagreed with the
    /// expected ping-pong count.
    const FAIL_COUNT: u16 = 6;

    /// Per-kthread run counters; index `i` counts kthread `i`'s yields.
    static RUNS: [AtomicU64; 2] = [AtomicU64::new(0), AtomicU64::new(0)];

    /// Static boot heap, placed in the linker's dedicated `.heap` (NOLOAD)
    /// section so the boot trampoline does not zero it. `static mut`
    /// because the bump allocator hands out disjoint slices via an atomic
    /// cursor; the storage is otherwise never aliased.
    #[link_section = ".heap"]
    static mut HEAP: Heap = Heap::ZERO;

    /// Global allocator backed by [`HEAP`].
    ///
    /// SAFETY: the page-aligned `HEAP` static outlives the binary and the
    /// allocator is its only consumer.
    #[global_allocator]
    static ALLOCATOR: FreeListAllocator =
        unsafe { FreeListAllocator::new(core::ptr::addr_of!(HEAP) as *mut u8, HEAP_BYTES) };

    /// Forward to the shared riscv64 panic bridge (parks the hart; the run
    /// then times out and the harness reports the failure).
    #[panic_handler]
    fn rustos_kthread_switch_riscv64_panic(info: &PanicInfo<'_>) -> ! {
        handle_panic_via_serial(info)
    }

    /// Emit a transcript marker through the serial sink.
    fn note(id: EventId, message: &'static str) {
        log(
            &SERIAL_SINK,
            &Event {
                level: Level::Info,
                id,
                message,
                fields: &[],
            },
        );
    }

    /// Boot entry point — the symbol the arch crate's `boot.s` trampoline
    /// calls (via `rustos_arch_riscv64_main`).
    #[no_mangle]
    pub extern "C" fn kernel_main(hartid: u64, dtb: u64) -> ! {
        note(TEST_START, "riscv64 kthread-switch test: starting");

        // Read the timer frequency from the firmware tree. Fail closed
        // (finisher) if it is omitted rather than guessing a divisor.
        // SAFETY: `dtb` is the verbatim `a1` pointer OpenSBI handed the boot
        // hart; `boot.s` forwards it unchanged.
        let Some(timebase) = (unsafe { Fdt::from_ptr(dtb as *const u8) })
            .ok()
            .and_then(|f| f.timebase_frequency())
        else {
            qemu_exit::exit_failure(FAIL_NO_TIMEBASE);
        };

        // The single-hart slice only brings up logical CPU 0 on hart 0.
        if hartid != u64::from(BOOT_CPU) {
            qemu_exit::exit_failure(FAIL_UNEXPECTED_HART);
        }

        // Build the live scheduler over the arch port.
        // Single-hart slice: one per-CPU slot, owned by an allocator-free
        // `static` backing.
        static STORAGE: RiscvArchStorage<1> = RiscvArchStorage::new();
        let arch = Arc::new(RiscvArch::new(&STORAGE, BOOT_CPU, timebase));
        let Ok(sched) = Scheduler::new(SchedulerConfig::defaults_for(1), arch) else {
            qemu_exit::exit_failure(FAIL_SCHED_NEW);
        };

        // Spawn two kthreads. Each runs on its own kernel stack and yields
        // back to the dispatcher PING_PONGS times via the real
        // `ContextSwitch::switch`, then returns (Exit). The
        // `ContextSwitchHal` handle is the riscv64 context-switch
        // primitive. `spawn` raises a self-IPI via SBI; with supervisor
        // interrupts masked it stays pending and never disturbs the
        // cooperative `step` loop below.
        for index in 0..2usize {
            let spawned = spawn_kthread(
                &sched,
                ContextSwitchHal::new(),
                BOOT_CPU,
                Priority::Normal,
                move |yielder| {
                    for _ in 0..PING_PONGS {
                        RUNS[index].fetch_add(1, Ordering::SeqCst);
                        yielder.yield_now();
                    }
                },
            );
            if spawned.is_err() {
                qemu_exit::exit_failure(FAIL_SPAWN);
            }
        }
        note(
            TEST_SPAWNED,
            "riscv64 kthread-switch test: two kthreads spawned",
        );

        // Cooperative dispatch loop: drive `step` until both kthreads have
        // exited. Each `step` enters a task, which yields straight back, so
        // the two tasks ping-pong through the real context switch. A switch
        // that never resumed its task would stall the drain and the harness
        // would time out (fail-loud).
        let mut steps = 0u64;
        while sched.live_task_count() != 0 && steps < MAX_STEPS {
            let _ = sched.step(BOOT_CPU);
            steps += 1;
        }
        if sched.live_task_count() != 0 {
            qemu_exit::exit_failure(FAIL_DEADLOCK);
        }
        if RUNS[0].load(Ordering::SeqCst) != PING_PONGS
            || RUNS[1].load(Ordering::SeqCst) != PING_PONGS
        {
            qemu_exit::exit_failure(FAIL_COUNT);
        }

        note(
            TEST_PASS,
            "riscv64 kthread-switch test: two kthreads ping-ponged via the real context switch",
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
