//! The freestanding aarch64 test kernel: admit one busy in-kernel kthread and
//! prove the generic-timer IRQ is delivered **while it runs** (the EL1 handler
//! accounts the tick) without ever preempting the kernel (`plans/PI.md` /
//! `AGENTS.md` §17.1).

use core::panic::PanicInfo;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use alloc::sync::Arc;

use rustos_arch_aarch64::context_hal::ContextSwitchHal;
use rustos_arch_aarch64::kernel_arch::timer_frequency_hz;
use rustos_arch_aarch64::preempt::{self, PreemptStorage};
use rustos_arch_aarch64::{
    exceptions, gic, handle_panic_via_serial, qemu_exit, Aarch64Arch, Aarch64ArchStorage,
    SERIAL_SINK,
};
use rustos_arch_api::CpuId;
use rustos_fdt::Fdt;
use rustos_kernel_core::{reschedule_current, spawn_kthread, RescheduleAction, Yielder};
use rustos_kernel_sched_eevdf::{Priority, Scheduler, SchedulerConfig};
use rustos_log::{log, Event, EventId, Level};

// The canonical QEMU `virt` device tree, dumped and embedded at build time.
include!(concat!(env!("OUT_DIR"), "/dtb_fixture.rs"));

/// The single-core slice runs logical CPU 0 on the boot core.
const BOOT_CPU: CpuId = 0;

/// Preemption quantum rate (slices/second) — the shared production rate
/// [`DEFAULT_PREEMPT_QUANTUM_HZ`](rustos_arch_api::timer::DEFAULT_PREEMPT_QUANTUM_HZ),
/// a ~10 ms one-shot. The kthread arms one quantum and busy-loops; the tick
/// fires well within the loop's span on QEMU TCG.
const PREEMPT_TICK_HZ: u64 = rustos_arch_api::timer::DEFAULT_PREEMPT_QUANTUM_HZ;

/// Timer ticks the kthread waits to observe before declaring the in-kernel
/// IRQ-delivery property proven and exiting. One is enough: it proves the
/// generic-timer IRQ was *taken* while the never-yielding kthread ran.
const TARGET_TICKS: u64 = 1;

/// Busy-loop iterations before the kthread re-arms the one-shot as a fallback.
/// The generic timer is wall-clock, so re-arming resets the down-counter:
/// this must be large enough that the first ~10 ms quantum fires long before a
/// re-arm (under QEMU TCG a tick arrives within a few hundred-thousand spins),
/// so a re-arm only ever happens if the first arming somehow did not take.
const REARM_EVERY_SPINS: u64 = 2_000_000_000;

/// Hard in-kernel-loop watchdog: maximum busy-spin iterations before the
/// kthread gives up and exits **without** the tick, so the post-loop check
/// reports a clear failure rather than relying solely on the harness
/// wall-clock budget. Sized generously above [`REARM_EVERY_SPINS`] so a
/// healthy run (the tick fires) always breaks first.
const MAX_SPINS: u64 = 40_000_000_000;

/// Dispatch-loop watchdog: the kthread runs to completion inside a single
/// `step` (it never yields), so this only backstops a kthread that never
/// returns. The harness wall-clock budget is the ultimate backstop.
const MAX_STEPS: u64 = 1_000_000;

/// Bump heap backing the scheduler, the arch handle, and the kthread's kernel
/// stack. 2 MiB is generous headroom; it lives in `.bss` (zeroed by the boot
/// trampoline).
const HEAP_SIZE: usize = 2 * 1024 * 1024;

/// Page-aligned backing store for the bump heap.
#[repr(C, align(4096))]
struct HeapStore([u8; HEAP_SIZE]);

static mut HEAP: HeapStore = HeapStore([0; HEAP_SIZE]);

/// Global allocator backed by [`HEAP`].
///
/// SAFETY: the page-aligned `HEAP` static outlives the binary and the
/// allocator is its only consumer.
#[global_allocator]
static ALLOCATOR: rustos_kalloc::FreeListAllocator = unsafe {
    rustos_kalloc::FreeListAllocator::new(core::ptr::addr_of!(HEAP) as *mut u8, HEAP_SIZE)
};

/// Per-CPU preemption backing for the single boot CPU (`AGENTS.md` §24.1 —
/// `PreemptStorage<1>` covers a single-core slice).
static PREEMPT_STORAGE: PreemptStorage<1> = PreemptStorage::new();

/// Generic-timer ticks taken (the tick callback increments this). A non-zero
/// value after the run proves the timer IRQ was delivered **while the
/// in-kernel kthread was busy-looping** — the P-5 in-kernel property.
static TICKS: AtomicU64 = AtomicU64::new(0);

/// EL0-preemption callbacks fired. The production preempt path is installed
/// verbatim, but every tick here is taken from EL1 (the in-kernel kthread), so
/// the IRQ path never reaches `on_el0_preempt_point`: this must stay `0`, the
/// evidence that the kernel itself was never preempted (`AGENTS.md` §4).
static PREEMPTIONS: AtomicU64 = AtomicU64::new(0);

/// Set by the kthread once it observed [`TARGET_TICKS`] timer ticks during its
/// busy span and is exiting normally — the evidence the interrupt returned to
/// the *same* task, which then ran to its voluntary completion.
static KTHREAD_COMPLETED: AtomicBool = AtomicBool::new(false);

/// Set once the scenario has been driven so a re-entry cannot re-run it.
static TEST_DRIVEN: AtomicBool = AtomicBool::new(false);

/// Stable audit-event ids for the QEMU transcript.
const TEST_START: EventId = EventId(4320);
const TEST_SPAWNED: EventId = EventId(4321);
const TEST_PASS: EventId = EventId(4322);

/// Semihosting failure codes, distinct per failure site.
const FAIL_REENTRY: u16 = 1;
const FAIL_FDT: u16 = 2;
const FAIL_ZERO_FREQ: u16 = 3;
const FAIL_GIC: u16 = 4;
const FAIL_PREEMPT_STORAGE: u16 = 5;
const FAIL_SCHED_NEW: u16 = 6;
const FAIL_SPAWN: u16 = 7;
const FAIL_DEADLOCK: u16 = 8;
const FAIL_NO_TICK: u16 = 9;
const FAIL_KERNEL_PREEMPTED: u16 = 10;
const FAIL_KTHREAD_INCOMPLETE: u16 = 11;

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

/// Forward to the shared aarch64 panic bridge (parks the CPU; the run then
/// times out and the harness reports the failure).
#[panic_handler]
fn preempt_inkernel_qemu_aarch64_panic(info: &PanicInfo<'_>) -> ! {
    handle_panic_via_serial(info)
}

/// The timer-tick callback `on_timer_interrupt` dispatches on **every** tick,
/// regardless of the interrupted privilege level. It only records that a tick
/// was taken — the proof that the generic-timer IRQ reached the CPU while the
/// in-kernel kthread was running. It is a `fn` with no captured environment, so
/// it is safe to invoke from interrupt context.
extern "C" fn timer_tick(_cpu: CpuId) {
    TICKS.fetch_add(1, Ordering::SeqCst);
}

/// The EL0-preemption callback — the production dispatch shape verbatim
/// (`AGENTS.md` §2.2): the IRQ path invokes it **only** for a tick taken from
/// EL0, to suspend the running user task back to the scheduler. Here every tick
/// is taken from EL1, so this must never run; if it ever did it would record
/// the violation (and still do the production reschedule), and the post-run
/// check fails loudly (`AGENTS.md` §4 — the kernel is non-preemptible).
extern "C" fn preempt_dispatch(cpu: CpuId) {
    PREEMPTIONS.fetch_add(1, Ordering::SeqCst);
    let _ = reschedule_current(cpu, RescheduleAction::Yield);
}

/// Boot entry point — the symbol the arch crate's `boot.s` trampoline calls
/// (via `rustos_arch_aarch64_main`).
#[no_mangle]
pub extern "C" fn kernel_main(_dtb: u64) -> ! {
    if TEST_DRIVEN
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        qemu_exit::exit_failure(FAIL_REENTRY);
    }

    note(
        TEST_START,
        "aarch64 in-kernel IRQ-delivery / non-preemption test: starting",
    );

    // 1. Discover the board from the embedded `virt` device tree: GICv2 bases
    //    and the generic-timer rate (`AGENTS.md` §2.20 / §18.2 — no hard-coded
    //    board constants).
    let Ok(fdt) = Fdt::new(DTB_BLOB) else {
        qemu_exit::exit_failure(FAIL_FDT);
    };
    let counter_hz = timer_frequency_hz(&fdt);
    if counter_hz == 0 {
        qemu_exit::exit_failure(FAIL_ZERO_FREQ);
    }
    if gic::configure_from_fdt(&fdt).is_none() {
        qemu_exit::exit_failure(FAIL_GIC);
    }

    // 2. EL1 vector table + GICv2 bring-up.
    // SAFETY: called once on the boot CPU with a stack established and before
    // any interrupt source is armed.
    unsafe {
        exceptions::init_vectors();
        gic::init();
    }

    // 3. Register the production tickless preemption path verbatim (`AGENTS.md`
    //    §2.2): per-CPU storage, the EL0-preemption callback (must never fire
    //    here), the timer-tick callback (records each tick), the per-quantum
    //    interval, and the enabled timer PPI — but leave the timer disarmed.
    if PREEMPT_STORAGE.register().is_err() {
        qemu_exit::exit_failure(FAIL_PREEMPT_STORAGE);
    }
    preempt::set_preempt_callback(preempt_dispatch);
    preempt::set_timer_callback(timer_tick);
    let interval = preempt::interval_for_hz(counter_hz, PREEMPT_TICK_HZ);
    // SAFETY: the boot CPU (id 0); the callbacks are installed (above), the
    // per-CPU storage is registered (above), the EL1 vector table is installed
    // (`init_vectors`), and the GIC is up (`gic::init`). This records the
    // quantum, enables the timer PPI, and leaves the timer disarmed; the
    // kthread arms the one-shot itself.
    unsafe {
        preempt::init_local_preempt(BOOT_CPU, interval);
    }

    // 4. Build the live eevdf scheduler over the arch port. Per-CPU bookkeeping
    //    backing for this single-CPU vertical (`AGENTS.md` §24.1).
    static ARCH_STORAGE: Aarch64ArchStorage<1> = Aarch64ArchStorage::new();
    let arch = Arc::new(Aarch64Arch::new(&ARCH_STORAGE, BOOT_CPU, counter_hz));
    let Ok(sched) = Scheduler::new(SchedulerConfig::defaults_for(1), arch) else {
        qemu_exit::exit_failure(FAIL_SCHED_NEW);
    };

    // 5. Admit ONE in-kernel kthread. Its body arms the generic-timer one-shot
    //    and then busy-loops issuing **no** `yield` and **no** syscall: the
    //    only thing that can interrupt it before its own `return` is a timer
    //    IRQ taken from EL1. It spins until it observes the tick callback fire
    //    (proving the IRQ reached the CPU mid-span and returned to this same
    //    task) and then exits normally.
    let spawned = spawn_kthread(
        &sched,
        ContextSwitchHal::new(),
        BOOT_CPU,
        Priority::Normal,
        move |_yielder: &mut Yielder<ContextSwitchHal>| {
            // Arm one quantum. The timer was left disarmed by
            // `init_local_preempt`; this starts the down-counter so a tick
            // fires ~one quantum from now, while this loop is still running.
            preempt::arm_oneshot(interval);
            let mut spins = 0u64;
            while TICKS.load(Ordering::SeqCst) < TARGET_TICKS {
                spins += 1;
                if spins >= MAX_SPINS {
                    // Watchdog: give up so the post-loop check reports a clear
                    // failure (`TICKS` still 0) rather than spinning until the
                    // harness wall-clock budget expires.
                    return;
                }
                if spins % REARM_EVERY_SPINS == 0 {
                    // Fallback only: re-arm in case the first arming did not
                    // take. Under a healthy run the tick fires long before
                    // this, so re-arming never resets a live countdown.
                    preempt::arm_oneshot(interval);
                }
                core::hint::spin_loop();
            }
            // Disarm so no further tick fires after the kthread exits, and
            // record that the interrupt returned to this same task, which then
            // ran to its voluntary completion.
            preempt::disarm();
            KTHREAD_COMPLETED.store(true, Ordering::SeqCst);
        },
    );
    if spawned.is_err() {
        qemu_exit::exit_failure(FAIL_SPAWN);
    }
    note(
        TEST_SPAWNED,
        "aarch64 in-kernel IRQ-delivery test: busy kthread admitted",
    );

    // 6. Enable device IRQs at the PE — the aarch64 backing of the production
    //    `KernelArch::set_device_irqs(true)` the P-5 dispatch loop calls once
    //    it begins steady-state dispatching (`AGENTS.md` §17.1). Every kthread
    //    the loop runs below now executes with the timer IRQ deliverable.
    // SAFETY: the vector table and GIC are up (step 2) and the timer
    // PPI/callbacks are installed (step 3), so an incoming tick dispatches
    // correctly.
    unsafe {
        exceptions::enable_irq();
    }

    // 7. Dispatch loop. The kthread never yields, so a single `step` runs its
    //    whole body: it arms the one-shot, the timer fires mid-loop (taken from
    //    EL1 — the IRQ path accounts the tick but does NOT reschedule), the
    //    loop observes the tick and the kthread returns. `step` then returns
    //    and the run queue drains.
    let mut steps = 0u64;
    while sched.live_task_count() != 0 && steps < MAX_STEPS {
        let _ = sched.step(BOOT_CPU);
        steps += 1;
    }
    if sched.live_task_count() != 0 {
        qemu_exit::exit_failure(FAIL_DEADLOCK);
    }

    // 8. The kthread must have reached its normal exit (the interrupt returned
    //    to the same task, which ran to completion).
    if !KTHREAD_COMPLETED.load(Ordering::SeqCst) {
        qemu_exit::exit_failure(FAIL_KTHREAD_INCOMPLETE);
    }
    // 9. In-kernel IRQ-delivery: the timer IRQ was taken while the busy kthread
    //    ran, so the tick callback fired at least [`TARGET_TICKS`] times. Under
    //    the old cooperative loop (IRQs masked across the task run) this would
    //    be `0` and the kthread would never have observed a tick.
    if TICKS.load(Ordering::SeqCst) < TARGET_TICKS {
        qemu_exit::exit_failure(FAIL_NO_TICK);
    }
    // 10. Non-preemptible kernel: every tick was taken from EL1, so the
    //     EL0-preemption callback never fired and the running task was never
    //     rescheduled out from under itself (`AGENTS.md` §4).
    if PREEMPTIONS.load(Ordering::SeqCst) != 0 {
        qemu_exit::exit_failure(FAIL_KERNEL_PREEMPTED);
    }

    note(
        TEST_PASS,
        "aarch64 in-kernel IRQ-delivery test: timer IRQ taken mid-span; kernel never preempted",
    );
    qemu_exit::exit_success();
}
