//! Stage 3c QEMU integration test: the riscv64 arch primitives drive the
//! *live* `kernel/sched` scheduler.
//!
//! ## What this test asserts
//!
//! The remaining Stage-3 riscv64 deliverable wires the arch port's
//! `preempt` (timer + IPI) and `context` primitives into the
//! architecture-neutral `kernel/sched` scheduler — not the test-local
//! counting callbacks the `timer_preempt` / `ipi_smp` verticals use. This
//! binary exercises that wiring end to end on the `virt` board:
//!
//! 1. **Real context switch.** With interrupts still disabled, it seeds a
//!    second `tairix_arch_riscv64::context::TaskCtx` over a private
//!    stack and `switch`es into it; the inbound task records that it ran
//!    and `switch`es straight back, proving a bidirectional bare-metal
//!    task switch round-trips.
//! 2. **Live scheduler.** It builds a real
//!    `tairix_kernel_sched_mlfq::Scheduler` over the arch port's
//!    `tairix_arch_riscv64::RiscvArch`, publishes it, and installs the
//!    `preempt` timer callback **and** the IPI software-interrupt callback
//!    so both drive `Scheduler::on_timer_tick`.
//! 3. **Timer + IPI + dispatch.** It installs the trap vector, enables the
//!    IPI and the 100 Hz SBI timer, spawns a batch of tasks, sends itself
//!    a directed IPI, and runs the cooperative `step` loop until every
//!    task has executed. It then waits until the supervisor-timer trap has
//!    driven `on_timer_tick` at least `MIN_PREEMPTIONS` times and
//!    the IPI software-interrupt path has driven the scheduler at least
//!    once, then writes the `SiFive` Test PASS finisher.
//!
//! A regression in any wired path (no context switch, no dispatch, no
//! timer tick, no IPI delivery) either trips a dedicated failure finisher
//! or never reaches the PASS write, so the run fails loudly — by an
//! explicit failure code or by the harness `Outcome::Timeout`.
//!
//! ## How it differs from a production kernel
//!
//! It links the `tairix-arch-riscv64` port and the
//! `tairix-kernel-sched-mlfq` policy directly and supplies its own
//! `kernel_main`, so the wiring is
//! exercised without the full `kernel_core::kernel_main` init pipeline
//! (which halts after boot and keeps its scheduler private). The
//! QEMU-exit shortcut lives in this dedicated bin, never behind a Cargo
//! feature on the arch crate (fail closed).

#![cfg_attr(itest_riscv64, no_std)]
#![cfg_attr(itest_riscv64, no_main)]
#![deny(missing_docs)]

// --- Freestanding test bin (`riscv64gc-unknown-none-elf`) ----------

#[cfg(itest_riscv64)]
extern crate alloc;

#[cfg(itest_riscv64)]
mod kernel {
    use core::panic::PanicInfo;
    use core::ptr::addr_of_mut;
    use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, Ordering};

    use alloc::sync::Arc;

    use tairix_arch_api::{CpuId, SchedulerArch};
    use tairix_arch_riscv64::context::TaskCtx;
    use tairix_arch_riscv64::fdt::Fdt;
    use tairix_arch_riscv64::{
        context, halt_current_hart, handle_panic_via_serial, preempt, qemu_exit, trap, RiscvArch,
        RiscvArchStorage, SERIAL_SINK,
    };
    use tairix_kalloc::{FreeListAllocator, Heap, HEAP_BYTES};
    use tairix_kernel_sched_mlfq::{Priority, Scheduler, SchedulerConfig, TaskAction};
    use tairix_log::{log, Event, EventId, Level};

    /// The single-hart slice runs logical CPU 0 on the boot hart.
    const BOOT_CPU: CpuId = 0;

    /// Scheduler-tick frequency to drive the SBI timer at.
    const TICK_HZ: u64 = 100;

    /// Number of tasks to spawn and run to completion.
    const TASK_COUNT: u64 = 64;

    /// Minimum supervisor-timer ticks the live scheduler must observe
    /// through [`Scheduler::on_timer_tick`] before the test passes. Large
    /// enough that a single spurious interrupt cannot satisfy it, yet
    /// reached well within the harness budget at 100 Hz.
    pub const MIN_PREEMPTIONS: u64 = 20;

    /// Cooperative-loop watchdog: maximum `step` iterations before the
    /// test declares the workload deadlocked. Sized generously for QEMU
    /// TCG (each `step` is a handful of instructions).
    const MAX_STEPS: u64 = 5_000_000;

    /// Stable audit-event ids for the QEMU transcript.
    const TEST_START: EventId = EventId(4220);
    const TEST_CTX_SWITCHED: EventId = EventId(4221);
    const TEST_TASKS_DONE: EventId = EventId(4222);
    const TEST_PASS: EventId = EventId(4223);

    /// Failure finisher code: the device tree advertised no timebase.
    const FAIL_NO_TIMEBASE: u16 = 1;
    /// Failure finisher code: the boot hart was not hart 0.
    const FAIL_UNEXPECTED_HART: u16 = 2;
    /// Failure finisher code: the context switch never ran the inbound task.
    const FAIL_CTX_SWITCH: u16 = 3;
    /// Failure finisher code: the scheduler could not be constructed.
    const FAIL_SCHED_NEW: u16 = 4;
    /// Failure finisher code: a task failed to spawn.
    const FAIL_SPAWN: u16 = 5;
    /// Failure finisher code: the cooperative loop did not drain every task.
    const FAIL_DEADLOCK: u16 = 6;
    /// Failure finisher code: the executed-task count disagreed with the
    /// spawned count.
    const FAIL_EXEC_COUNT: u16 = 7;

    /// Static boot heap, placed in the linker's dedicated `.heap` (NOLOAD)
    /// section so the boot trampoline does not zero it and it is excluded
    /// from any usable-memory map. `static mut` because the bump allocator
    /// hands out disjoint slices via an atomic cursor; the storage is
    /// otherwise never aliased.
    #[link_section = ".heap"]
    static mut HEAP: Heap = Heap::ZERO;

    /// Global allocator backed by [`HEAP`].
    ///
    /// SAFETY: the page-aligned `HEAP` static outlives the binary and the
    /// allocator is its only consumer.
    #[global_allocator]
    static ALLOCATOR: FreeListAllocator =
        unsafe { FreeListAllocator::new(core::ptr::addr_of!(HEAP) as *mut u8, HEAP_BYTES) };

    /// Published `Scheduler<RiscvArch>` raw pointer the trap-path callbacks
    /// consult. The boot hart stores it (from a leaked `Arc`) before any
    /// timer or IPI is armed; the pointee is never freed, so the trap path
    /// reads it without touching the `Arc` strong count.
    static SCHED_FOR_TRAP: AtomicPtr<Scheduler<RiscvArch>> = AtomicPtr::new(core::ptr::null_mut());

    /// Set `true` by the inbound task of the [`context::switch`] round-trip.
    static CTX_SWITCH_RAN: AtomicBool = AtomicBool::new(false);

    /// Number of task bodies that have executed.
    static EXECUTIONS: AtomicU64 = AtomicU64::new(0);

    /// Set `true` the first time the IPI software-interrupt callback runs.
    static IPI_DROVE_SCHED: AtomicBool = AtomicBool::new(false);

    /// Outbound context of the boot path; `switch` records the boot path's
    /// suspended `sp` here so the inbound task can `switch` back to it.
    static mut MAIN_CTX: TaskCtx = TaskCtx::new();

    /// Inbound task's context, seeded by [`TaskCtx::prepare`].
    static mut WORKER_CTX: TaskCtx = TaskCtx::new();

    /// Private kernel stack for the inbound task of the round-trip.
    /// 16-byte aligned per the RISC-V ABI; sized well above the synthesised
    /// frame plus the trivial body's needs.
    #[repr(C, align(16))]
    struct WorkerStack([u8; 16 * 1024]);

    static mut WORKER_STACK: WorkerStack = WorkerStack([0; 16 * 1024]);

    /// Exclusive upper bound (one past the last byte) of [`WORKER_STACK`].
    ///
    /// Forms a one-past-the-end address of the static stack; it is never
    /// dereferenced. The `align(16)` struct keeps the top 16-byte aligned,
    /// as [`TaskCtx::prepare`] requires.
    fn worker_stack_top() -> u64 {
        let base = addr_of_mut!(WORKER_STACK) as u64;
        base + core::mem::size_of::<WorkerStack>() as u64
    }

    /// Inbound task of the [`context::switch`] round-trip. Records that it
    /// ran, then switches straight back to the boot path. It never returns
    /// to its caller (the boot path never switches back into it), so the
    /// tail parks the hart to satisfy the `-> !` contract.
    unsafe extern "C" fn worker_entry(_arg: usize) -> ! {
        CTX_SWITCH_RAN.store(true, Ordering::SeqCst);
        // SAFETY: `WORKER_CTX` is the running task's context (exclusive to
        // this hart for the call) and `MAIN_CTX` holds the boot path's
        // suspended `sp`, written by the outbound `switch`. Both are
        // non-null, aligned `*mut TaskCtx`s the kernel owns.
        unsafe {
            context::switch(addr_of_mut!(WORKER_CTX), addr_of_mut!(MAIN_CTX));
        }
        halt_current_hart()
    }

    /// Per-quantum interval (`time`-CSR ticks) the one-shot is re-armed to,
    /// published before interrupts are enabled so [`on_tick`] can read it.
    static TIMER_INTERVAL: AtomicU64 = AtomicU64::new(0);

    /// The supervisor-timer scheduler-tick callback. Drives the live
    /// scheduler's per-CPU preemption counter through
    /// [`Scheduler::on_timer_tick`]. ISR-safe: `on_timer_tick` is wait-free
    /// (one bounds check + one `fetch_add`). TAIRiX is tickless: the one-shot does not auto-reload, so the
    /// callback re-arms the next one-shot itself (standing in for the
    /// scheduler's `set_preemption`) so the timer keeps driving the live
    /// scheduler while this hart idles in `wfi` below.
    extern "C" fn on_tick(cpu: CpuId) {
        if let Some(sched) = scheduler() {
            let _ = sched.on_timer_tick(cpu);
        }
        let interval = TIMER_INTERVAL.load(Ordering::Relaxed);
        if interval != 0 {
            preempt::arm_oneshot(interval);
        }
    }

    /// The IPI software-interrupt callback. Also drives the live scheduler
    /// (a delivered IPI is a reschedule request), and records that the IPI
    /// path reached the scheduler.
    extern "C" fn on_ipi(cpu: CpuId) {
        IPI_DROVE_SCHED.store(true, Ordering::SeqCst);
        if let Some(sched) = scheduler() {
            let _ = sched.on_timer_tick(cpu);
        }
    }

    /// Borrow the published scheduler, if any.
    fn scheduler() -> Option<&'static Scheduler<RiscvArch>> {
        let raw = SCHED_FOR_TRAP.load(Ordering::Acquire);
        if raw.is_null() {
            None
        } else {
            // SAFETY: the boot hart publishes `raw` from a leaked `Arc`
            // before any timer or IPI is armed; the pointee outlives every
            // callback because the leaked strong count is never dropped.
            Some(unsafe { &*raw })
        }
    }

    /// Forward to the shared riscv64 panic bridge (parks the hart; the run
    /// then times out and the harness reports the failure).
    #[panic_handler]
    fn tairix_sched_drive_riscv64_panic(info: &PanicInfo<'_>) -> ! {
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
    /// calls (via `tairix_arch_riscv64_main`).
    #[no_mangle]
    pub extern "C" fn kernel_main(hartid: u64, dtb: u64) -> ! {
        note(TEST_START, "riscv64 sched-drive test: starting");

        // Read the timer frequency. Fail closed (finisher) if the device
        // tree omits it rather than guessing a divisor.
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

        // 1. Real bidirectional context switch, before interrupts are
        //    enabled (no trap can fire mid-switch onto the inbound stack).
        if let Err(_e) = (unsafe { &mut *addr_of_mut!(WORKER_CTX) }).prepare(
            worker_stack_top(),
            worker_entry,
            0xC0FF_EE00,
        ) {
            qemu_exit::exit_failure(FAIL_CTX_SWITCH);
        }
        // SAFETY: `MAIN_CTX`/`WORKER_CTX` are non-null, aligned `*mut
        // TaskCtx`s the kernel owns; `WORKER_CTX.sp` was just seeded by
        // `prepare` over `WORKER_STACK`, which is mapped and exclusive to
        // this hart. Control returns here once `worker_entry` switches back.
        unsafe {
            context::switch(addr_of_mut!(MAIN_CTX), addr_of_mut!(WORKER_CTX));
        }
        if !CTX_SWITCH_RAN.load(Ordering::SeqCst) {
            qemu_exit::exit_failure(FAIL_CTX_SWITCH);
        }
        note(
            TEST_CTX_SWITCHED,
            "riscv64 sched-drive test: context switch round-tripped",
        );

        // 2. Build the live scheduler over the arch port and publish it for
        //    the trap-path callbacks.
        // Single-hart slice: one per-CPU slot, owned by an allocator-free
        // `static` backing.
        static STORAGE: RiscvArchStorage<1> = RiscvArchStorage::new();
        let arch = Arc::new(RiscvArch::new(&STORAGE, BOOT_CPU, timebase));
        let Ok(sched) = Scheduler::new(SchedulerConfig::defaults_for(1), Arc::clone(&arch)) else {
            qemu_exit::exit_failure(FAIL_SCHED_NEW);
        };
        let sched = Arc::new(sched);
        SCHED_FOR_TRAP.store(
            Arc::into_raw(Arc::clone(&sched)).cast_mut(),
            Ordering::Release,
        );

        // Install both callbacks before any source is armed so the first
        // tick / IPI finds them in place.
        preempt::set_timer_callback(on_tick);
        preempt::set_ipi_callback(on_ipi);

        // 3. Trap vector + global interrupt enable, then the IPI enable and
        //    the 100 Hz SBI timer.
        // SAFETY: called once on the boot hart with a stack established and
        // before any source is armed; both callbacks are installed.
        let interval = preempt::interval_for_hz(timebase, TICK_HZ);
        TIMER_INTERVAL.store(interval, Ordering::Relaxed);
        unsafe {
            trap::init_traps();
            preempt::enable_ipi();
            // `init_local_preempt` records the interval + enables `sie.STIE`
            // but leaves the timer disarmed (tickless); arm the first
            // one-shot, after which `on_tick` re-arms each fire.
            preempt::init_local_preempt(BOOT_CPU, interval);
            preempt::arm_oneshot(interval);
        }

        // Spawn the workload. Each body increments the shared counter and
        // exits; `spawn` raises a (self-)IPI on the home CPU, exercising the
        // software-interrupt path on top of the explicit IPI below.
        for _ in 0..TASK_COUNT {
            let spawned = sched.spawn(BOOT_CPU, Priority::Normal, move |_ctx| {
                EXECUTIONS.fetch_add(1, Ordering::Relaxed);
                TaskAction::Exit
            });
            if spawned.is_err() {
                qemu_exit::exit_failure(FAIL_SPAWN);
            }
        }

        // Send an explicit directed IPI to this CPU (a self-reschedule) so
        // the software-interrupt → scheduler path is driven deterministically.
        arch.send_ipi(BOOT_CPU);

        // Cooperative dispatch loop: drive `step` until every task has run.
        let mut steps = 0u64;
        while sched.live_task_count() != 0 && steps < MAX_STEPS {
            let _ = sched.step(BOOT_CPU);
            steps += 1;
        }
        if sched.live_task_count() != 0 {
            qemu_exit::exit_failure(FAIL_DEADLOCK);
        }
        if EXECUTIONS.load(Ordering::Relaxed) != TASK_COUNT {
            qemu_exit::exit_failure(FAIL_EXEC_COUNT);
        }
        note(
            TEST_TASKS_DONE,
            "riscv64 sched-drive test: all tasks dispatched",
        );

        // 4. Wait until the supervisor-timer trap has driven the live
        //    scheduler at least MIN_PREEMPTIONS times. A regression that
        //    fails to deliver or re-arm the timer never reaches the bound,
        //    so the harness reports a timeout (fail-loud).
        while sched.preemption_count(BOOT_CPU).unwrap_or(0) < MIN_PREEMPTIONS {
            // SAFETY: `wfi` is a wait-for-interrupt hint with no
            // architectural side effects; the timer interrupt wakes it.
            unsafe {
                core::arch::asm!("wfi", options(nomem, nostack, preserves_flags));
            }
        }

        // And until the IPI software-interrupt path has driven the
        // scheduler at least once (same fail-loud-by-timeout contract).
        while !IPI_DROVE_SCHED.load(Ordering::SeqCst) {
            // SAFETY: as above.
            unsafe {
                core::arch::asm!("wfi", options(nomem, nostack, preserves_flags));
            }
        }

        note(
            TEST_PASS,
            "riscv64 sched-drive test: live scheduler driven by timer + IPI",
        );
        qemu_exit::exit_success();
    }
}

// --- Host stub -----------------------------------------------------
#[cfg(not(itest_riscv64))]
fn main() {}
