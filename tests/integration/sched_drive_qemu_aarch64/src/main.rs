//! WIRING Stage W7 QEMU integration test: the aarch64 arch primitives
//! drive the *live* `kernel/sched` scheduler.
//!
//! ## What this test asserts
//!
//! `plans/WIRING.md` Stage W7 requires the aarch64 port's `preempt`
//! (generic timer + GICv2 IPI) and `context` primitives to drive the
//! architecture-neutral `kernel/sched` scheduler — not the test-local
//! counting callbacks the `timer_preempt` / `ipi_smp` verticals use.
//! This binary exercises that wiring end to end on the `virt` board, the
//! EL1/GICv2 analogue of `sched_drive_qemu_riscv64`:
//!
//! 1. **Real context switch.** With interrupts still disabled, it seeds a
//!    second `rustos_arch_aarch64::context::TaskCtx` over a private stack
//!    and `switch`es into it; the inbound task records that it ran and
//!    `switch`es straight back, proving a bidirectional bare-metal task
//!    switch round-trips.
//! 2. **Live scheduler.** It builds a real
//!    `rustos_kernel_sched_mlfq::Scheduler` over the arch port's
//!    `rustos_arch_aarch64::Aarch64Arch`, publishes it, and installs the
//!    `preempt` generic-timer callback **and** the GICv2 IPI (SGI)
//!    callback so both drive `Scheduler::on_timer_tick`.
//! 3. **Timer + IPI + dispatch.** It installs the EL1 vector table and
//!    GICv2, enables the IPI SGI and the 100 Hz generic timer, unmasks
//!    IRQs, spawns a batch of tasks, sends itself a directed IPI, and
//!    runs the cooperative `step` loop until every task has executed. It
//!    then waits until the generic-timer IRQ has driven `on_timer_tick`
//!    at least `MIN_PREEMPTIONS` times and the IPI SGI path has driven
//!    the scheduler at least once, then writes the ARM semihosting PASS
//!    finisher.
//!
//! A regression in any wired path (no context switch, no dispatch, no
//! timer tick, no IPI delivery) either trips a dedicated failure finisher
//! or never reaches the PASS write, so the run fails loudly — by an
//! explicit failure code or by the harness `Outcome::Timeout`
//! (`AGENTS.md` §7).
//!
//! ## Discovered GICv2 base + timer rate (PI Stage P4)
//!
//! Before `gic::init`, the boot core **poisons** the runtime GICv2 base
//! and then reads the GICD/GICC bases from the canonical `virt` device
//! tree embedded at build time (`gic::configure_from_fdt`), asserting the
//! base moved off the poison value to the `virt` GICv2 distributor base.
//! The generic-timer frequency that sizes the 100 Hz tick interval is
//! likewise taken from the tree via `kernel_arch::timer_frequency_hz`
//! (the `/timer` `clock-frequency` override when present, else
//! `CNTFRQ_EL0`). Every subsequent GIC access — `gic::init`, the timer
//! PPI enable, the directed SGI — targets that discovered base, so the
//! timer + IPI ticks that drive the live scheduler are the runtime proof
//! the discovered base and rate work (`plans/PI.md` P4). The board tree
//! is embedded, not read from `x0`, because QEMU's ELF `-kernel` aarch64
//! boot hands the kernel no DTB pointer.
//!
//! ## How it differs from a production kernel
//!
//! It links the `rustos-arch-aarch64` port and the
//! `rustos-kernel-sched-mlfq` policy directly and supplies its own
//! `kernel_main`, so the wiring is exercised without the full
//! `kernel_core::kernel_main` init pipeline (which halts after boot and
//! keeps its scheduler private). The QEMU-exit shortcut lives in this
//! dedicated bin, never behind a Cargo feature on the arch crate
//! (`AGENTS.md` §5.4.5 — fail closed).

#![cfg_attr(itest_aarch64, no_std)]
#![cfg_attr(itest_aarch64, no_main)]
#![deny(missing_docs)]

// --- Freestanding test bin (`aarch64-unknown-none`) ----------------

#[cfg(itest_aarch64)]
extern crate alloc;

#[cfg(itest_aarch64)]
mod kernel {
    use core::panic::PanicInfo;
    use core::ptr::addr_of_mut;
    use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, Ordering};

    use alloc::sync::Arc;

    use rustos_arch_aarch64::context::TaskCtx;
    use rustos_arch_aarch64::kernel_arch::timer_frequency_hz;
    use rustos_arch_aarch64::{
        context, exceptions, gic, halt_current_cpu, handle_panic_via_serial, preempt, qemu_exit,
        Aarch64Arch, SERIAL_SINK,
    };
    use rustos_arch_api::{CpuId, SchedulerArch};
    use rustos_fdt::Fdt;
    use rustos_kalloc::{FreeListAllocator, Heap, HEAP_BYTES};
    use rustos_kernel_sched_mlfq::{Priority, Scheduler, SchedulerConfig, TaskAction};
    use rustos_log::{log, Event, EventId, Level};

    // The canonical QEMU `virt` device tree, dumped and embedded at build
    // time (`build.rs`): the GICv2 base and the timer frequency are read
    // from it (P4), proving the live scheduler is driven over a
    // *discovered* base + rate, not the pre-discovery defaults.
    include!(concat!(env!("OUT_DIR"), "/dtb_fixture.rs"));

    /// The single-core slice runs logical CPU 0 on the boot core.
    const BOOT_CPU: CpuId = 0;

    /// Scheduler-tick frequency to drive the generic timer at.
    const TICK_HZ: u64 = 100;

    /// Number of tasks to spawn and run to completion.
    const TASK_COUNT: u64 = 64;

    /// Minimum generic-timer ticks the live scheduler must observe
    /// through [`Scheduler::on_timer_tick`] before the test passes. Large
    /// enough that a single spurious interrupt cannot satisfy it, yet
    /// reached well within the harness budget at 100 Hz.
    pub const MIN_PREEMPTIONS: u64 = 20;

    /// Cooperative-loop watchdog: maximum `step` iterations before the
    /// test declares the workload deadlocked. Sized generously for QEMU
    /// TCG (each `step` is a handful of instructions).
    const MAX_STEPS: u64 = 5_000_000;

    /// Stable audit-event ids for the QEMU transcript.
    const TEST_START: EventId = EventId(4240);
    const TEST_CTX_SWITCHED: EventId = EventId(4241);
    const TEST_TASKS_DONE: EventId = EventId(4242);
    const TEST_PASS: EventId = EventId(4243);

    /// Failure finisher code: `CNTFRQ_EL0` reported a zero frequency.
    const FAIL_ZERO_FREQ: u16 = 1;
    /// Failure finisher code: the context switch never ran the inbound task.
    const FAIL_CTX_SWITCH: u16 = 2;
    /// Failure finisher code: the scheduler could not be constructed.
    const FAIL_SCHED_NEW: u16 = 3;
    /// Failure finisher code: a task failed to spawn.
    const FAIL_SPAWN: u16 = 4;
    /// Failure finisher code: the cooperative loop did not drain every task.
    const FAIL_DEADLOCK: u16 = 5;
    /// Failure finisher code: the executed-task count disagreed with the
    /// spawned count.
    const FAIL_EXEC_COUNT: u16 = 6;
    /// Failure finisher code: the GICv2 base was not discovered from the
    /// embedded `virt` device tree (P4).
    const FAIL_GIC_NOT_DISCOVERED: u16 = 7;

    /// A deliberately-wrong GICv2 distributor/CPU-interface base installed
    /// before discovery runs. It is **not** the `virt` GICv2 base, so the
    /// timer/IPI ticks the live scheduler later observes can only mean
    /// discovery overwrote it with the base read from the device tree.
    const POISON_GIC_BASE: usize = 0xdead_0000;

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

    /// Published `Scheduler<Aarch64Arch>` raw pointer the IRQ-path
    /// callbacks consult. The boot core stores it (from a leaked `Arc`)
    /// before any timer or IPI is armed; the pointee is never freed, so
    /// the IRQ path reads it without touching the `Arc` strong count.
    static SCHED_FOR_IRQ: AtomicPtr<Scheduler<Aarch64Arch>> = AtomicPtr::new(core::ptr::null_mut());

    /// Set `true` by the inbound task of the [`context::switch`] round-trip.
    static CTX_SWITCH_RAN: AtomicBool = AtomicBool::new(false);

    /// Number of task bodies that have executed.
    static EXECUTIONS: AtomicU64 = AtomicU64::new(0);

    /// Set `true` the first time the IPI SGI callback runs.
    static IPI_DROVE_SCHED: AtomicBool = AtomicBool::new(false);

    /// Outbound context of the boot path; `switch` records the boot path's
    /// suspended `sp` here so the inbound task can `switch` back to it.
    static mut MAIN_CTX: TaskCtx = TaskCtx::new();

    /// Inbound task's context, seeded by [`TaskCtx::prepare`].
    static mut WORKER_CTX: TaskCtx = TaskCtx::new();

    /// Private kernel stack for the inbound task of the round-trip.
    /// 16-byte aligned per the AArch64 ABI; sized well above the
    /// synthesised frame plus the trivial body's needs.
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
    /// tail parks the core to satisfy the `-> !` contract.
    unsafe extern "C" fn worker_entry(_arg: usize) -> ! {
        CTX_SWITCH_RAN.store(true, Ordering::SeqCst);
        // SAFETY: `WORKER_CTX` is the running task's context (exclusive to
        // this core for the call) and `MAIN_CTX` holds the boot path's
        // suspended `sp`, written by the outbound `switch`. Both are
        // non-null, aligned `*mut TaskCtx`s the kernel owns.
        unsafe {
            context::switch(addr_of_mut!(WORKER_CTX), addr_of_mut!(MAIN_CTX));
        }
        halt_current_cpu()
    }

    /// Per-quantum interval (counter ticks) the one-shot is re-armed to,
    /// published before IRQs are unmasked so [`on_tick`] can read it.
    static TIMER_INTERVAL: AtomicU64 = AtomicU64::new(0);

    /// The generic-timer scheduler-tick callback. Drives the live
    /// scheduler's per-CPU preemption counter through
    /// [`Scheduler::on_timer_tick`]. ISR-safe: `on_timer_tick` is wait-free
    /// (one bounds check + one `fetch_add`). RustOS is tickless
    /// (`AGENTS.md` §17.1): the one-shot does not auto-reload, so the
    /// callback re-arms the next one-shot itself (standing in for the
    /// scheduler's `set_preemption`) so the timer keeps driving the live
    /// scheduler while this CPU idles in `wfi` below.
    extern "C" fn on_tick(cpu: CpuId) {
        if let Some(sched) = scheduler() {
            let _ = sched.on_timer_tick(cpu);
        }
        let interval = TIMER_INTERVAL.load(Ordering::Relaxed);
        if interval != 0 {
            preempt::arm_oneshot(interval);
        }
    }

    /// The IPI SGI callback. Also drives the live scheduler (a delivered
    /// IPI is a reschedule request), and records that the IPI path reached
    /// the scheduler.
    extern "C" fn on_ipi(cpu: CpuId) {
        IPI_DROVE_SCHED.store(true, Ordering::SeqCst);
        if let Some(sched) = scheduler() {
            let _ = sched.on_timer_tick(cpu);
        }
    }

    /// Borrow the published scheduler, if any.
    fn scheduler() -> Option<&'static Scheduler<Aarch64Arch>> {
        let raw = SCHED_FOR_IRQ.load(Ordering::Acquire);
        if raw.is_null() {
            None
        } else {
            // SAFETY: the boot core publishes `raw` from a leaked `Arc`
            // before any timer or IPI is armed; the pointee outlives every
            // callback because the leaked strong count is never dropped.
            Some(unsafe { &*raw })
        }
    }

    /// Forward to the shared aarch64 panic bridge (parks the core; the run
    /// then times out and the harness reports the failure).
    #[panic_handler]
    fn rustos_sched_drive_aarch64_panic(info: &PanicInfo<'_>) -> ! {
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
    /// calls (via `rustos_arch_aarch64_main`).
    #[no_mangle]
    pub extern "C" fn kernel_main(_dtb: u64) -> ! {
        note(TEST_START, "aarch64 sched-drive test: starting");

        // P4: read the board from the embedded `virt` device tree. The
        // counter frequency is the tree's `/timer` `clock-frequency`
        // override when present, else `CNTFRQ_EL0` (the `virt` board
        // omits the override, so this exercises the register fallback at
        // runtime while the override branch is host-tested). It feeds both
        // the arch handle's monotonic clock and the timer interval; fail
        // closed (finisher) if it resolves to zero rather than dividing by
        // it.
        let Ok(fdt) = Fdt::new(DTB_BLOB) else {
            qemu_exit::exit_failure(FAIL_GIC_NOT_DISCOVERED);
        };
        let counter_hz = timer_frequency_hz(&fdt);
        if counter_hz == 0 {
            qemu_exit::exit_failure(FAIL_ZERO_FREQ);
        }

        // Prove the GICv2 base is *discovered*, not assumed. Poison the
        // runtime base, then read the GICD/GICC bases from the embedded
        // `virt` device tree. Every later GIC access — `gic::init`, the
        // timer PPI enable, the directed SGI — targets this discovered
        // base, so the timer + IPI ticks the live scheduler observes are
        // the runtime proof the discovered base works (`plans/PI.md` P4).
        gic::configure(POISON_GIC_BASE, POISON_GIC_BASE);
        if gic::configure_from_fdt(&fdt).is_none() || gic::current().0 != gic::DEFAULT_GICD_BASE {
            qemu_exit::exit_failure(FAIL_GIC_NOT_DISCOVERED);
        }

        // 1. Real bidirectional context switch, before interrupts are
        //    enabled (no IRQ can fire mid-switch onto the inbound stack).
        if (unsafe { &mut *addr_of_mut!(WORKER_CTX) })
            .prepare(worker_stack_top(), worker_entry, 0xC0FF_EE00)
            .is_err()
        {
            qemu_exit::exit_failure(FAIL_CTX_SWITCH);
        }
        // SAFETY: `MAIN_CTX`/`WORKER_CTX` are non-null, aligned `*mut
        // TaskCtx`s the kernel owns; `WORKER_CTX.sp` was just seeded by
        // `prepare` over `WORKER_STACK`, which is mapped and exclusive to
        // this core. Control returns here once `worker_entry` switches back.
        unsafe {
            context::switch(addr_of_mut!(MAIN_CTX), addr_of_mut!(WORKER_CTX));
        }
        if !CTX_SWITCH_RAN.load(Ordering::SeqCst) {
            qemu_exit::exit_failure(FAIL_CTX_SWITCH);
        }
        note(
            TEST_CTX_SWITCHED,
            "aarch64 sched-drive test: context switch round-tripped",
        );

        // 2. Build the live scheduler over the arch port and publish it for
        //    the IRQ-path callbacks.
        // Per-CPU bookkeeping backing for this single-CPU vertical
        // (`AGENTS.md` §24.1).
        static ARCH_STORAGE: rustos_arch_aarch64::Aarch64ArchStorage<1> =
            rustos_arch_aarch64::Aarch64ArchStorage::new();
        let arch = Arc::new(Aarch64Arch::new(&ARCH_STORAGE, BOOT_CPU, counter_hz));
        let Ok(sched) = Scheduler::new(SchedulerConfig::defaults_for(1), Arc::clone(&arch)) else {
            qemu_exit::exit_failure(FAIL_SCHED_NEW);
        };
        let sched = Arc::new(sched);
        SCHED_FOR_IRQ.store(
            Arc::into_raw(Arc::clone(&sched)).cast_mut(),
            Ordering::Release,
        );

        // Install both callbacks before any source is armed so the first
        // tick / IPI finds them in place.
        preempt::set_timer_callback(on_tick);
        preempt::set_ipi_callback(on_ipi);

        // Register the per-CPU preemption backing (sized to this
        // single-CPU vertical, `AGENTS.md` §24.1) so the timer slot
        // `init_local_preempt` records exists.
        static PREEMPT_STORAGE: preempt::PreemptStorage<1> = preempt::PreemptStorage::new();
        if PREEMPT_STORAGE.register().is_err() {
            qemu_exit::exit_failure(FAIL_SCHED_NEW);
        }

        // 3. Vector table + GIC bring-up, then the IPI SGI enable, the
        //    100 Hz generic timer, and the PE IRQ unmask.
        // SAFETY: called once on the boot core with a stack established and
        // before any source is armed; both callbacks and the per-CPU
        // preemption storage are installed.
        let interval = preempt::interval_for_hz(counter_hz, TICK_HZ);
        TIMER_INTERVAL.store(interval, Ordering::Relaxed);
        unsafe {
            exceptions::init_vectors();
            gic::init();
            preempt::enable_ipi();
            // `init_local_preempt` records the interval + enables the PPI
            // but leaves the timer disarmed (tickless); arm the first
            // one-shot, after which `on_tick` re-arms each fire.
            preempt::init_local_preempt(BOOT_CPU, interval);
            preempt::arm_oneshot(interval);
            exceptions::enable_irq();
        }

        // Spawn the workload. Each body increments the shared counter and
        // exits; `spawn` raises a (self-)IPI on the home CPU, exercising the
        // SGI path on top of the explicit IPI below.
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
        // the GICv2 SGI → scheduler path is driven deterministically.
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
            "aarch64 sched-drive test: all tasks dispatched",
        );

        // 4. Wait until the generic-timer IRQ has driven the live scheduler
        //    at least MIN_PREEMPTIONS times. A regression that fails to
        //    deliver or re-arm the timer never reaches the bound, so the
        //    harness reports a timeout (fail-loud, `AGENTS.md` §7).
        while sched.preemption_count(BOOT_CPU).unwrap_or(0) < MIN_PREEMPTIONS {
            // SAFETY: `wfi` is a wait-for-interrupt hint with no
            // architectural side effects; the timer interrupt wakes it.
            unsafe {
                core::arch::asm!("wfi", options(nomem, nostack, preserves_flags));
            }
        }

        // And until the IPI SGI path has driven the scheduler at least once
        // (same fail-loud-by-timeout contract).
        while !IPI_DROVE_SCHED.load(Ordering::SeqCst) {
            // SAFETY: as above.
            unsafe {
                core::arch::asm!("wfi", options(nomem, nostack, preserves_flags));
            }
        }

        note(
            TEST_PASS,
            "aarch64 sched-drive test: live scheduler driven by timer + IPI",
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
