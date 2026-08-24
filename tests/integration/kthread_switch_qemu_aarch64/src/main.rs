//! SPAWN Stage SP1 QEMU integration test: two kernel-thread tasks
//! ping-pong through the **real** Arch HAL `ContextSwitch::switch` under
//! the live `kernel/sched` scheduler on the aarch64 `virt` board.
//!
//! ## What this test asserts
//!
//! `plans/SPAWN.md` Stage SP1 requires the `kernel/core` kthread runtime
//! to make a scheduler task a *resumable kernel thread* with its own
//! kernel stack, driven through the existing `ContextSwitch` HAL, and to
//! prove it on at least one bare-metal board by having **two** kthreads
//! ping-pong via the real `ContextSwitch::switch`. That promotes
//! `ContextSwitch::switch` — until now exercised only by the W7
//! `sched_drive_*` round-trip — to a *production* scheduling path.
//!
//! This binary exercises that on the `virt` board:
//!
//! 1. **Discovered GICv2 + timer rate (PI Stage P4).** It reads the GICv2
//!    base and the generic-timer frequency from the embedded canonical
//!    `virt` device tree and brings up the EL1 vectors + GICv2 so the
//!    scheduler's spawn IPIs target a configured controller. The two
//!    kthreads deliberately hold opposite IRQ-mask states across each yield,
//!    proving processor continuation state follows the task rather than
//!    leaking between task and dispatcher.
//! 2. **Live scheduler + two kthreads.** It builds a real
//!    `tairix_kernel_sched_eevdf::Scheduler` over `Aarch64Arch` and spawns
//!    two kthreads through `tairix_kernel_core::spawn_kthread`. Each
//!    kthread body runs on its own kernel stack and calls
//!    `Yielder::yield_now` `PING_PONGS` times — each yield is a real
//!    `ContextSwitch::switch` back to the dispatcher — then returns
//!    (`Exit`).
//! 3. **Ping-pong + drain.** It drives the cooperative `step` loop; the
//!    scheduler interleaves the two runnable tasks, so they alternate. PASS
//!    once both kthreads have run exactly `PING_PONGS` times and both
//!    have exited (no task left live).
//!
//! A regression in the runtime (a switch that does not transfer control,
//! a task that never resumes, a stack that is not reclaimed) either trips
//! a dedicated failure finisher or never drains the workload, so the run
//! fails loudly — by an explicit failure code or by the harness
//! `Outcome::Timeout`.
//!
//! ## How it differs from a production kernel
//!
//! It links the `tairix-kernel-core` kthread runtime, the
//! `tairix-arch-aarch64` port, and the default `tairix-kernel-sched-eevdf`
//! policy directly and supplies its own `kernel_main`, so the runtime is
//! exercised without the full `kernel_core::kernel_main` init pipeline.
//! The QEMU-exit shortcut lives in this dedicated bin, never behind a
//! Cargo feature on a library crate (fail closed).

#![cfg_attr(itest_aarch64, no_std)]
#![cfg_attr(itest_aarch64, no_main)]
#![deny(missing_docs)]

// --- Freestanding test bin (`aarch64-unknown-none`) ----------------

#[cfg(itest_aarch64)]
extern crate alloc;

#[cfg(itest_aarch64)]
mod kernel {
    use core::num::NonZeroU16;
    use core::panic::PanicInfo;
    use core::sync::atomic::{AtomicU64, Ordering};

    use alloc::sync::Arc;

    use tairix_arch_aarch64::context_hal::ContextSwitchHal;
    use tairix_arch_aarch64::kernel_arch::timer_frequency_hz;
    use tairix_arch_aarch64::{
        exceptions, gic, handle_panic_via_serial, qemu_exit, Aarch64Arch, SERIAL_SINK,
    };
    use tairix_arch_api::CpuId;
    use tairix_fdt::Fdt;
    use tairix_itest_finisher::fail_point;
    use tairix_kalloc::{FreeListAllocator, Heap, HEAP_BYTES};
    use tairix_kernel_core::spawn_kthread;
    use tairix_kernel_sched_eevdf::{Priority, Scheduler, SchedulerConfig};
    use tairix_log::{log, Event, EventId, Level};

    // The canonical QEMU `virt` device tree, dumped and embedded at build
    // time (`build.rs`): the GICv2 base and the timer frequency are read
    // from it (P4).
    include!(concat!(env!("OUT_DIR"), "/dtb_fixture.rs"));

    /// The single-core slice runs logical CPU 0 on the boot core.
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
    const TEST_START: EventId = EventId(4260);
    const TEST_SPAWNED: EventId = EventId(4261);
    const TEST_PASS: EventId = EventId(4262);

    /// Failure finisher code: the discovered timer frequency was zero.
    const FAIL_ZERO_FREQ: NonZeroU16 = fail_point!(1);
    /// Failure finisher code: the GICv2 base was not discovered from the
    /// embedded `virt` device tree (P4).
    const FAIL_GIC_NOT_DISCOVERED: NonZeroU16 = fail_point!(2);
    /// Failure finisher code: the scheduler could not be constructed.
    const FAIL_SCHED_NEW: NonZeroU16 = fail_point!(3);
    /// Failure finisher code: a kthread failed to spawn.
    const FAIL_SPAWN: NonZeroU16 = fail_point!(4);
    /// Failure finisher code: the cooperative loop did not drain the
    /// kthreads (a switch that never resumed, e.g.).
    const FAIL_DEADLOCK: NonZeroU16 = fail_point!(5);
    /// Failure finisher code: a kthread's run count disagreed with the
    /// expected ping-pong count.
    const FAIL_COUNT: NonZeroU16 = fail_point!(6);
    /// Failure finisher code: a resumed kthread inherited another
    /// continuation's IRQ mask.
    const FAIL_DAIF: NonZeroU16 = fail_point!(7);

    /// A deliberately-wrong GICv2 base installed before discovery runs, so
    /// reaching the `virt` distributor base can only mean discovery
    /// overwrote it from the device tree (`plans/PI.md` P4).
    const POISON_GIC_BASE: usize = 0xdead_0000;

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

    /// Forward to the shared aarch64 panic bridge (parks the core; the run
    /// then times out and the harness reports the failure).
    #[panic_handler]
    fn tairix_kthread_switch_aarch64_panic(info: &PanicInfo<'_>) -> ! {
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

    /// Whether the calling continuation currently has IRQ taking masked.
    fn irq_is_masked() -> bool {
        let daif: u64;
        // SAFETY: reading DAIF is side-effect free at EL1.
        unsafe {
            core::arch::asm!(
                "mrs {}, DAIF",
                out(reg) daif,
                options(nomem, nostack, preserves_flags)
            );
        }
        daif & (1 << 7) != 0
    }

    /// Boot entry point — the symbol the arch crate's `boot.s` trampoline
    /// calls (via `tairix_arch_aarch64_main`).
    #[no_mangle]
    pub extern "C" fn kernel_main(_dtb: u64) -> ! {
        note(TEST_START, "aarch64 kthread-switch test: starting");

        // P4: read the board from the embedded `virt` device tree.
        let Ok(fdt) = Fdt::new(DTB_BLOB) else {
            qemu_exit::exit_failure(FAIL_GIC_NOT_DISCOVERED);
        };
        let counter_hz = timer_frequency_hz(&fdt);
        if counter_hz == 0 {
            qemu_exit::exit_failure(FAIL_ZERO_FREQ);
        }

        // Prove the GICv2 base is *discovered*, not assumed, then bring up
        // the vectors + GICv2 so the scheduler's spawn IPIs target a
        // configured controller. Dispatch is the cooperative `step` loop
        // below; each kthread controls its own IRQ mask to prove the context
        // switch preserves processor continuation state.
        gic::configure(POISON_GIC_BASE, POISON_GIC_BASE);
        if gic::configure_from_fdt(&fdt).is_none() || gic::current().0 != gic::DEFAULT_GICD_BASE {
            qemu_exit::exit_failure(FAIL_GIC_NOT_DISCOVERED);
        }
        // SAFETY: called once on the boot core with a stack established.
        unsafe {
            exceptions::init_vectors();
            gic::init();
        }

        // Build the live scheduler over the arch port.
        // Per-CPU bookkeeping backing for this single-CPU vertical.
        static ARCH_STORAGE: tairix_arch_aarch64::Aarch64ArchStorage<1> =
            tairix_arch_aarch64::Aarch64ArchStorage::new();
        let arch = Arc::new(Aarch64Arch::new(&ARCH_STORAGE, BOOT_CPU, counter_hz));
        let Ok(sched) = Scheduler::new(SchedulerConfig::defaults_for(1), arch) else {
            qemu_exit::exit_failure(FAIL_SCHED_NEW);
        };

        // Spawn two kthreads. Each runs on its own kernel stack and yields
        // back to the dispatcher PING_PONGS times via the real
        // `ContextSwitch::switch`, then returns (Exit). The `ContextSwitchHal`
        // handle is the aarch64 context-switch primitive.
        for index in 0..2usize {
            let spawned = spawn_kthread(
                &sched,
                ContextSwitchHal::new(),
                BOOT_CPU,
                Priority::Normal,
                move |yielder| {
                    for _ in 0..PING_PONGS {
                        // Give the two continuations opposite IRQ-mask state.
                        // The vector table and GIC are live, so unmasking is
                        // safe; no periodic timer source is armed.
                        unsafe {
                            if index == 0 {
                                exceptions::mask_irq();
                            } else {
                                exceptions::enable_irq();
                            }
                        }
                        RUNS[index].fetch_add(1, Ordering::SeqCst);
                        yielder.yield_now();
                        if irq_is_masked() != (index == 0) {
                            qemu_exit::exit_failure(FAIL_DAIF);
                        }
                    }
                },
            );
            if spawned.is_err() {
                qemu_exit::exit_failure(FAIL_SPAWN);
            }
        }
        note(
            TEST_SPAWNED,
            "aarch64 kthread-switch test: two kthreads spawned",
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
            "aarch64 kthread-switch test: two kthreads ping-ponged via the real context switch",
        );
        qemu_exit::exit_success();
    }
}

// --- Host stub -----------------------------------------------------
#[cfg(not(itest_aarch64))]
fn main() {}
