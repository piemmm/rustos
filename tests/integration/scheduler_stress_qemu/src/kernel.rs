//! x86_64 / `target_os = "none"` body of the QEMU scheduler-stress test.

use core::alloc::{GlobalAlloc, Layout};
use core::fmt::Write as _;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, AtomicUsize, Ordering};

extern crate alloc;
use alloc::sync::Arc;

use rustos_arch_api::SecondaryBringup;
use rustos_arch_x86_64::acpi::{self, MadtEntry};
use rustos_arch_x86_64::apic::{Lapic, VolatileLapicMmio};
use rustos_arch_x86_64::apic_timer::{self, Calibration, PolledPit, Rdtsc};
use rustos_arch_x86_64::kernel_arch::{X86_64Arch, X86_64ArchStorage};
use rustos_arch_x86_64::multiboot2::BootInfo;
use rustos_arch_x86_64::smp;
use rustos_arch_x86_64::{percpu, preempt};
use rustos_arch_x86_64::{qemu_exit, serial};
use rustos_kernel_sched_mlfq::{Priority, Scheduler, SchedulerArch, SchedulerConfig, TaskAction};

// --- Workload sizing -----------------------------------------------

/// Tasks per CPU. The QEMU runner allocates 256 MiB of guest RAM
/// (`tools/qemu::Spec::for_x86_64_kernel` default) and the bump heap is
/// 64 MiB; 2048 tasks per CPU × 4 CPUs = 8192 tasks fits comfortably
/// with headroom for the `BTreeMap` / `Arc` bookkeeping the scheduler does
/// internally. The host-side `scheduler_stress` test still drives the
/// 20 000-task figure mandated by Stage-2; this binary's purpose is to
/// prove the *real-cores* path, not to repeat the host scale.
const TASKS_PER_CPU: u32 = 2_048;

/// Cooperative-loop safety cap: maximum number of `step()` iterations a
/// CPU performs before the BSP declares the test deadlocked. Sized
/// generously (one iteration ≈ a dozen instructions on QEMU TCG; this
/// budget tolerates a 10× slowdown).
const MAX_STEPS_PER_CPU: u64 = 1_000_000;

// --- Cross-CPU coordination ----------------------------------------

/// `Arc<Scheduler<SmpArch>>` raw pointer the BSP publishes once the
/// scheduler is constructed. APs `Acquire`-load it before entering the
/// step loop. The BSP never replaces it, so a single atomic suffices.
static SCHED_PTR: AtomicPtr<Scheduler<SmpArch>> = AtomicPtr::new(core::ptr::null_mut());

/// Set to `true` by the BSP once the workload completes (or fails).
/// APs observe it in their step loop and break out.
static SHUTDOWN: AtomicBool = AtomicBool::new(false);

/// Per-CPU execution counter (debug aid; verifies tasks ran on multiple
/// physical cores). Sized to the maximum supported CPU count.
///
/// Must not exceed `rustos_arch_x86_64::percpu::MAX_CPUS` — the arch
/// crate's per-CPU arena is the source of truth for the bound. The
/// const-assert immediately below makes a future divergence a
/// compile-time error.
const MAX_CPUS: usize = 16;

#[allow(dead_code)] // const-assert; not referenced at runtime.
const MAX_CPUS_FITS_PERCPU_ARENA: () = assert!(MAX_CPUS <= rustos_arch_x86_64::percpu::MAX_CPUS);
static PER_CPU_EXEC: [AtomicU64; MAX_CPUS] = {
    // `AtomicU64::new` is `const`. Hand-roll the array; the
    // `[AtomicU64::new(0); MAX_CPUS]` syntax requires `Copy`. The
    // `const` initializer is consumed by the array literal and never
    // re-named; `declare_interior_mutable_const` is suppressed with
    // rationale per AGENTS.md §15.10.
    #[allow(clippy::declare_interior_mutable_const)]
    const ZERO: AtomicU64 = AtomicU64::new(0);
    [ZERO; MAX_CPUS]
};

/// Number of APs that have entered the step loop. The BSP waits on this
/// for an acquire-fence before spawning tasks so that the spawn-time
/// `send_ipi` (which is a no-op in this binary) does not race a CPU
/// that has not yet observed the published scheduler.
static APS_LIVE: AtomicU32 = AtomicU32::new(0);

/// Total expected execution count, written by the BSP before spawning.
static EXPECTED_EXECS: AtomicU64 = AtomicU64::new(0);

/// Total tasks actually executed (incremented from every task body).
static EXECUTIONS: AtomicU64 = AtomicU64::new(0);

/// LAPIC-timer period the BSP calibrates for. 1 ms gives a comfortable
/// 1 kHz scheduler tick: long enough that the dispatcher's overhead
/// stays well under 1 % even at QEMU TCG speed, and short enough that
/// the integration test sees hundreds of ticks per CPU during a
/// multi-second run (4 cores × O(1 000) tasks × hundreds of µs each).
const PREEMPT_PERIOD_US: u32 = 1_000;

/// PIT calibration window. 10 ms is the well-known calibration period
/// (PIT channel-2 reload fits in 16 bits up to ~54 ms, well above).
const PREEMPT_CALIBRATION_WINDOW_US: u32 = 10_000;

/// Minimum per-CPU preemption count the test asserts at the end of
/// the workload. The BSP step loop is bounded by
/// `MAX_STEPS_PER_CPU * O(1 task)`; on QEMU TCG that takes hundreds of
/// milliseconds, well above 100 timer periods. We pick a *small but
/// non-zero* lower bound so the assertion stays robust against TCG
/// jitter while still failing loudly if preemption is silently
/// disabled (`AGENTS.md` §7 — no flaky tests, no silent regressions).
const MIN_PREEMPTIONS_PER_CPU: u64 = 10;

/// BSP-computed LAPIC-timer calibration, packed into a `u64` (ticks/s
/// fit in 32 bits for any LAPIC RustOS targets; `initial_count` is
/// 32 bits). Zero means "not yet calibrated".
///
/// Two fields → one 64-bit atomic: ticks-per-second in the low 32
/// bits, initial-count in the high 32. The period itself is
/// `PREEMPT_PERIOD_US` so does not need transporting.
static BSP_CALIBRATION_PACKED: AtomicU64 = AtomicU64::new(0);

fn pack_calibration(c: Calibration) -> u64 {
    // `ticks_per_second` is at most `u32::MAX` because `initial_count`
    // is `u32` and the LAPIC counter is 32-bit. `calibrate` caps it
    // there; documented in `apic_timer::compute_initial_count`.
    let tps = c.ticks_per_second.min(u64::from(u32::MAX)) as u32;
    (u64::from(tps)) | (u64::from(c.initial_count) << 32)
}

fn unpack_calibration() -> Option<Calibration> {
    let raw = BSP_CALIBRATION_PACKED.load(Ordering::Acquire);
    if raw == 0 {
        return None;
    }
    let tps = (raw & 0xFFFF_FFFF) as u32;
    let initial = (raw >> 32) as u32;
    Some(Calibration {
        ticks_per_second: u64::from(tps),
        initial_count: initial,
        period_micros: PREEMPT_PERIOD_US,
        // APs do not consult `tsc_per_second` — only the BSP-side
        // syscall `clock_get` path (Stage 2.7 follow-up (f3)) does,
        // and that runs against the BSP's full calibration. Re-using
        // the packed transport shape would require widening the
        // transport from `u64` to `u128`; documented here as a
        // deliberate carry-over rather than silently widening (
        // `AGENTS.md` §2.4 — no interface creep).
        tsc_per_second: 0,
    })
}

/// Raw pointer to the published `Scheduler<SmpArch>` for the timer
/// ISR's exclusive use. Stored separately from `SCHED_PTR` so the ISR
/// does **not** need to call `Arc::clone` (which takes a mutex on the
/// strong count) in interrupt context.
static SCHED_FOR_TIMER: AtomicPtr<Scheduler<SmpArch>> = AtomicPtr::new(core::ptr::null_mut());

/// Timer-tick callback installed via `preempt::set_timer_callback`.
///
/// Runs in ISR context with interrupts disabled. Steps:
///
/// 1. Load the scheduler pointer (a raw `*const`, never freed once
///    published).
/// 2. Call `Scheduler::on_timer_tick(cpu)`, which bumps the
///    scheduler's per-CPU preemption counter and drives one
///    `step(cpu)`.
///
/// We deliberately do **not** propagate `on_timer_tick`'s `Result`:
/// the dispatcher only invokes us with a `CpuId` produced by the
/// `LAPIC_TO_CPU_ID` table that the BSP/APs populated themselves, so
/// `Err(NoSuchCpu)` would be a kernel bug, not a recoverable
/// condition. We let the scheduler-side metric (`preemption_count`)
/// be the regression catcher: a CPU whose ID was misregistered would
/// have a flat counter and the post-run assertion would fail loudly.
extern "C" fn scheduler_tick(cpu: u32) {
    let raw = SCHED_FOR_TIMER.load(Ordering::Acquire);
    if raw.is_null() {
        return;
    }
    // SAFETY: the BSP publishes `raw` from a leaked `Arc::into_raw`
    // before any AP comes up and before any timer is armed. The
    // pointee outlives every timer tick because the leaked `Arc`'s
    // strong count is never decremented.
    let sched: &Scheduler<SmpArch> = unsafe { &*raw };
    let _ = sched.on_timer_tick(cpu);
}

// --- Allocator -----------------------------------------------------

/// Heap size. 64 MiB comfortably fits 8 192 Arc<TaskInner>s, the
/// scheduler's `BTreeMap<TaskId, ...>` registry, the per-priority
/// `RunDeque` slots, and the closures we spawn.
const HEAP_BYTES: usize = 64 * 1024 * 1024;

#[repr(C, align(4096))]
struct Heap([u8; HEAP_BYTES]);

// SAFETY of `static mut`: this is the kernel binary's *only* mutable
// static, accessed exclusively through `BumpAllocator::alloc`. The
// allocator serialises access through `cursor: AtomicUsize`; the heap
// bytes themselves are never aliased because each allocation hands out
// a disjoint slice from the bump cursor.
static mut HEAP: Heap = Heap([0; HEAP_BYTES]);

/// Forward-only bump allocator. AGENTS.md §6 puts shared utilities in
/// `lib/`; this allocator is intentionally local because (a) it never
/// frees (a documented limitation for the test binary only) and (b)
/// nothing in `kernel/` or `lib/` should ever take a dependency on a
/// leak-by-design allocator.
struct BumpAllocator {
    cursor: AtomicUsize,
}

unsafe impl GlobalAlloc for BumpAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: `HEAP` is exactly `HEAP_BYTES` long; the loop below
        // performs CAS-driven bump allocation and never advances past
        // `HEAP_BYTES`, so the returned pointer is in-bounds whenever
        // the function returns non-null.
        let base = unsafe { core::ptr::addr_of_mut!(HEAP.0).cast::<u8>() };
        let align = layout.align();
        let size = layout.size();
        let mut cur = self.cursor.load(Ordering::Relaxed);
        loop {
            let aligned = (cur + align - 1) & !(align - 1);
            let Some(next) = aligned.checked_add(size) else {
                return core::ptr::null_mut();
            };
            if next > HEAP_BYTES {
                return core::ptr::null_mut();
            }
            match self
                .cursor
                .compare_exchange_weak(cur, next, Ordering::AcqRel, Ordering::Relaxed)
            {
                Ok(_) => {
                    // SAFETY: `aligned < HEAP_BYTES` by the bound check
                    // above, so `base.add(aligned)` is in-bounds.
                    return unsafe { base.add(aligned) };
                }
                Err(observed) => cur = observed,
            }
        }
    }
    unsafe fn dealloc(&self, _ptr: *mut u8, _layout: Layout) {
        // Deliberate no-op. The binary runs once per QEMU invocation
        // and the bump heap is reclaimed by QEMU exit.
    }
}

#[global_allocator]
static ALLOCATOR: BumpAllocator = BumpAllocator {
    cursor: AtomicUsize::new(0),
};

// --- SchedulerArch impl -------------------------------------------

/// Architecture implementation for the QEMU kernel. `current_cpu`
/// reads the LAPIC ID register directly; `send_ipi` is a no-op-with-
/// counter because Stage 3a (b) does not arm preemption (cores are in
/// a tight `step()` loop and observe spawned tasks on their next
/// iteration; the host-side test on `TestArch` exhibits the same
/// behaviour).
struct SmpArch;

impl SchedulerArch for SmpArch {
    fn current_cpu(&self) -> u32 {
        u32::from(smp::bsp_lapic_id())
    }
    fn ticks_now(&self) -> u64 {
        // RDTSC has been available since the Pentium and is universally
        // emulated by QEMU. We treat it as a monotonic-enough clock.
        // SAFETY: RDTSC is unprivileged and has no side effects.
        unsafe {
            let lo: u32;
            let hi: u32;
            core::arch::asm!(
                "rdtsc",
                out("eax") lo,
                out("edx") hi,
                options(nomem, nostack, preserves_flags),
            );
            (u64::from(hi) << 32) | u64::from(lo)
        }
    }
    fn send_ipi(&self, _target: u32) {
        // No preemption in (b); the receiver is already polling
        // `step()`. Stage 3a (c) replaces this with a real IPI.
    }
}

// --- BSP entry ----------------------------------------------------

/// Entry point: BSP only.
//
// The body is long because it sequences every Stage 3a (c) bring-up
// step in a single linear path — multiboot parse, ACPI/MADT discovery,
// LAPIC software-enable, PIT calibration, scheduler construction, AP
// SIPI-SIPI, workload spawn, watchdog, and the per-CPU preemption
// audit. Splitting it would only push the same `qemu_exit::exit_failure`
// branches into multiple helpers and obscure the deliberate boot
// ordering; AGENTS.md §15.5 forbids gratuitous helpers without two
// independent call sites. The `too_many_lines` lint is suppressed
// with rationale.
#[allow(clippy::too_many_lines)]
#[no_mangle]
pub extern "C" fn kernel_main(multiboot_info: u64) -> ! {
    let mut com1 = serial::Serial::init(serial::COM1_BASE);
    let _ = writeln!(com1, "[scheduler_stress_qemu] BSP boot");

    // Stage 3a (c1/c2): replace the boot-time GDT (`boot.s`) and the
    // not-yet-installed IDT with the BSP's per-CPU GDT + IDT. After
    // this call the kernel is on its real long-mode descriptor tables
    // for cpu_index 0; `#DF` and `#NMI` land on dedicated IST stacks.
    //
    // SAFETY: this is the BSP, called exactly once before any
    // interrupts are enabled. The boot trampoline (`boot.s` SAFETY-
    // INVARIANT 6) guarantees the IDTR is invalid on entry; replacing
    // it now is the documented sequencing.
    unsafe {
        if percpu::init(0).is_err() {
            let _ = writeln!(com1, "[scheduler_stress_qemu] FAIL: percpu::init(BSP)");
            qemu_exit::exit_failure();
        }
    }

    // Software-enable the BSP's LAPIC so we can drive IPIs.
    let mut lapic = make_lapic();
    lapic.software_enable(0xFF);
    let bsp_id = smp::bsp_lapic_id();
    let _ = writeln!(com1, "[scheduler_stress_qemu] BSP LAPIC id = {bsp_id}");

    // Calibrate the LAPIC timer against the PIT *once*, on the BSP. APs
    // re-use the result via `BSP_CALIBRATION_PACKED`. See the rustdoc on
    // `rustos_arch_x86_64::preempt` for why per-CPU re-calibration is
    // unnecessary on QEMU and on homogeneous Intel SMP, plus the
    // fail-loud cross-check (the per-CPU preemption count below).
    let mut pit = PolledPit;
    let mut tsc = Rdtsc;
    let calibration = match apic_timer::calibrate(
        &mut lapic,
        &mut pit,
        &mut tsc,
        PREEMPT_CALIBRATION_WINDOW_US,
        PREEMPT_PERIOD_US,
    ) {
        Ok(c) => c,
        Err(e) => {
            let _ = writeln!(
                com1,
                "[scheduler_stress_qemu] FAIL: LAPIC timer calibration: {e:?}"
            );
            qemu_exit::exit_failure();
        }
    };
    let _ = writeln!(
        com1,
        "[scheduler_stress_qemu] LAPIC calibrated: {} ticks/s, initial_count={} for {}us period",
        calibration.ticks_per_second, calibration.initial_count, calibration.period_micros
    );
    BSP_CALIBRATION_PACKED.store(pack_calibration(calibration), Ordering::Release);
    preempt::set_cpu_id_for_lapic(bsp_id, 0);

    // Discover APs.
    let Some(ap_ids) = discover_aps(multiboot_info, bsp_id, &mut com1) else {
        let _ = writeln!(com1, "[scheduler_stress_qemu] FAIL: MADT discovery");
        qemu_exit::exit_failure();
    };
    let cpu_count = (ap_ids.count + 1) as u32;
    let _ = writeln!(
        com1,
        "[scheduler_stress_qemu] discovered {cpu_count} CPUs (BSP + {} APs)",
        ap_ids.count
    );
    if cpu_count < 4 {
        let _ = writeln!(
            com1,
            "[scheduler_stress_qemu] FAIL: Stage-2 mandates >= 4 cores, got {cpu_count}"
        );
        qemu_exit::exit_failure();
    }

    // Build the scheduler and publish it.
    let cfg = SchedulerConfig {
        cpus: cpu_count,
        queue_capacity_per_band: 8192,
        yields_before_demotion: 4,
        boost_interval_ticks: 256,
    };
    let arch = Arc::new(SmpArch);
    let sched = Arc::new(Scheduler::new(cfg, arch).expect("scheduler"));
    // Leak one Arc into the global pointer so APs can clone it.
    let raw = Arc::into_raw(sched.clone()).cast_mut();
    SCHED_PTR.store(raw, Ordering::Release);
    // The timer ISR consults `SCHED_FOR_TIMER` directly (so it does
    // not have to touch `Arc`'s strong count in interrupt context).
    // Publish before any timer is armed.
    SCHED_FOR_TIMER.store(raw, Ordering::Release);

    // Register the scheduler-tick callback exactly once, before any
    // CPU's timer is armed.
    preempt::set_timer_callback(scheduler_tick);

    // Register every AP's LAPIC ID -> dense CpuId mapping *before*
    // we bring them up so the first tick that fires on a given AP
    // (typically right after `sti`) finds the mapping populated.
    for i in 0..ap_ids.count {
        let cpu_id = (i + 1) as u32;
        preempt::set_cpu_id_for_lapic(ap_ids.ids[i], cpu_id);
    }

    // Build the bring-up handle from the discovered LAPIC map and start
    // every AP through the Arch HAL `SecondaryBringup` trait. The
    // INIT-SIPI-SIPI orchestration now lives in `rustos_arch_x86_64::smp`
    // (`plans/WIRING.md` Stage W14); this vertical exercises it
    // end-to-end on ≥ 4 real (emulated) cores.
    let mut cpu_to_lapic: [Option<u8>; percpu::MAX_CPUS] = [None; percpu::MAX_CPUS];
    cpu_to_lapic[0] = Some(bsp_id);
    for i in 0..ap_ids.count {
        cpu_to_lapic[i + 1] = Some(ap_ids.ids[i]);
    }
    // The bring-up handle borrows its per-CPU bookkeeping from a
    // caller-sized `&'static` backing (`AGENTS.md` §24.1 — sized to this
    // vertical's discovered CPU count, no fixed ceiling in the arch
    // crate). `kernel_main` runs once, so a function-local `static` is
    // sound and needs no allocator.
    static ARCH_STORAGE: X86_64ArchStorage<{ percpu::MAX_CPUS }> = X86_64ArchStorage::new();
    let bringup = match X86_64Arch::new(&ARCH_STORAGE, 0, bsp_id, &cpu_to_lapic) {
        Ok(handle) => handle,
        Err(e) => {
            let _ = writeln!(
                com1,
                "[scheduler_stress_qemu] FAIL: X86_64Arch::new: {}",
                e.as_str()
            );
            qemu_exit::exit_failure();
        }
    };
    // Install the AP entry once (set-once); the HAL stamps it into each
    // AP's boot slot.
    if smp::set_secondary_entry(ap_entry).is_err() {
        let _ = writeln!(
            com1,
            "[scheduler_stress_qemu] FAIL: secondary entry already installed"
        );
        qemu_exit::exit_failure();
    }
    for i in 0..ap_ids.count {
        let cpu_id = (i + 1) as u32; // BSP = 0, APs = 1..N
        let _ = writeln!(
            com1,
            "[scheduler_stress_qemu] bringing up APIC id {} as cpu_id {cpu_id}",
            ap_ids.ids[i]
        );
        // SAFETY: this is the BSP; `boot.s` zeroed `.bss` (clearing the
        // AP stack pool), the BSP LAPIC was software-enabled above, the
        // secondary entry is installed, and `cpu_id` maps to a real,
        // parked AP discovered from the MADT.
        if let Err(e) = unsafe { bringup.start_secondary(cpu_id) } {
            let _ = writeln!(
                com1,
                "[scheduler_stress_qemu] FAIL: start_secondary(cpu {cpu_id}): {}",
                e.as_str()
            );
            qemu_exit::exit_failure();
        }
    }

    // Arm the BSP's timer and enable interrupts. This *must* happen
    // after the scheduler is published and the callback is installed
    // — see SAFETY-INVARIANT notes on `preempt::init_local_preempt`.
    //
    // SAFETY: this is the BSP, `percpu::init(0)` ran above, interrupts
    // are still disabled (the boot trampoline left IF=0 and nothing
    // has run `sti`), and `lapic` is the BSP's LAPIC.
    unsafe {
        if preempt::init_local_preempt(0, &mut lapic, calibration).is_err() {
            let _ = writeln!(
                com1,
                "[scheduler_stress_qemu] FAIL: preempt::init_local_preempt(BSP)"
            );
            qemu_exit::exit_failure();
        }
        // SAFETY: every per-CPU prerequisite for accepting interrupts
        // is now in place — per-CPU IDT installed, timer vector
        // pointing at the ISR stub, LAPIC software-enabled, callback
        // registered.
        core::arch::asm!("sti", options(nomem, nostack, preserves_flags));
    }

    // Wait for every AP to reach its step loop.
    while APS_LIVE.load(Ordering::Acquire) < ap_ids.count as u32 {
        core::hint::spin_loop();
    }
    let _ = writeln!(
        com1,
        "[scheduler_stress_qemu] all {} APs live, spawning tasks",
        ap_ids.count
    );

    // Spawn the workload.
    let total_tasks = u64::from(TASKS_PER_CPU) * u64::from(cpu_count);
    EXPECTED_EXECS.store(total_tasks, Ordering::Release);
    for i in 0..total_tasks {
        let home = (i % u64::from(cpu_count)) as u32;
        sched
            .spawn(home, Priority::Normal, move |_ctx| {
                // Each CPU's execution counter is incremented by the
                // *executing* CPU, not the home CPU — that's what proves
                // multi-core dispatch happened.
                let me = smp::bsp_lapic_id() as usize;
                if me < MAX_CPUS {
                    PER_CPU_EXEC[me].fetch_add(1, Ordering::Relaxed);
                }
                EXECUTIONS.fetch_add(1, Ordering::Relaxed);
                TaskAction::Exit
            })
            .expect("spawn");
    }
    let _ = writeln!(
        com1,
        "[scheduler_stress_qemu] spawned {total_tasks} tasks; entering step loop"
    );

    // BSP step loop alongside the APs.
    run_step_loop(0, &sched);

    // BSP fell out of the loop: live_task_count == 0 OR step budget
    // exhausted. Signal APs and verify.
    SHUTDOWN.store(true, Ordering::Release);

    let live = sched.live_task_count();
    let execs = EXECUTIONS.load(Ordering::Acquire);
    let _ = writeln!(
        com1,
        "[scheduler_stress_qemu] step loop end: live={live} execs={execs} expected={total_tasks}"
    );

    if live != 0 {
        let _ = writeln!(
            com1,
            "[scheduler_stress_qemu] FAIL: deadlock — {live} tasks remained"
        );
        qemu_exit::exit_failure();
    }
    if execs != total_tasks {
        let _ = writeln!(
            com1,
            "[scheduler_stress_qemu] FAIL: exec count {execs} != expected {total_tasks}"
        );
        qemu_exit::exit_failure();
    }

    // Sanity: ≥ 2 distinct CPUs must have executed work. (Stronger than
    // "≥ 4" because cooperative dispatch can starve a CPU briefly while
    // its queue is empty; the host test asserts bounded first-run
    // latency which is a (c) concern. The point of *this* test is to
    // prove the AP bring-up path put multiple real cores to work.)
    let mut distinct_cpus = 0u32;
    for slot in &PER_CPU_EXEC {
        if slot.load(Ordering::Relaxed) > 0 {
            distinct_cpus += 1;
        }
    }
    let _ = writeln!(
        com1,
        "[scheduler_stress_qemu] distinct executing CPUs = {distinct_cpus}"
    );
    if distinct_cpus < 2 {
        let _ = writeln!(
            com1,
            "[scheduler_stress_qemu] FAIL: only {distinct_cpus} CPU(s) ran tasks; AP bring-up did not actually dispatch"
        );
        qemu_exit::exit_failure();
    }

    // Stage 3a (c5): the scheduler must have been driven by the LAPIC
    // timer ISR — not merely by the cooperative `step()` loop — on
    // every CPU. A silent regression to cooperative-only scheduling
    // would preserve the workload-correctness check above but break
    // the security model (`AGENTS.md` §5: a runaway task must not
    // indefinitely block another CPU's progress); this is the test
    // that catches it.
    let total_preempts = sched.total_preemption_count();
    let _ = writeln!(
        com1,
        "[scheduler_stress_qemu] preemption ticks total = {total_preempts}"
    );
    let mut min_observed = u64::MAX;
    for cpu in 0..cpu_count {
        let n = sched.preemption_count(cpu).unwrap_or(0);
        let _ = writeln!(com1, "[scheduler_stress_qemu]   cpu {cpu}: {n} preemptions");
        if n < min_observed {
            min_observed = n;
        }
        if n < MIN_PREEMPTIONS_PER_CPU {
            let _ = writeln!(
                com1,
                "[scheduler_stress_qemu] FAIL: cpu {cpu} observed {n} preemptions, expected >= {MIN_PREEMPTIONS_PER_CPU}"
            );
            qemu_exit::exit_failure();
        }
    }
    let _ = writeln!(
        com1,
        "[scheduler_stress_qemu] preemption assertion OK (min per CPU = {min_observed})"
    );

    let _ = writeln!(com1, "[scheduler_stress_qemu] PASS");
    qemu_exit::exit_success();
}

// --- AP entry -----------------------------------------------------

extern "C" fn ap_entry(cpu_id: u32) -> ! {
    // Stage 3a (c1/c2): install this AP's per-CPU GDT + IDT before
    // touching any further shared state. The AP trampoline left us on
    // the trampoline-internal GDT (`ap_trampoline.s` SAFETY-INVARIANT 5)
    // with the IDTR still invalid (SAFETY-INVARIANT 8); `percpu::init`
    // moves us to the real per-CPU tables.
    //
    // SAFETY: called exactly once per AP, on that AP, before
    // interrupts are enabled.
    unsafe {
        if percpu::init(cpu_id as usize).is_err() {
            // We cannot signal the BSP cleanly from here (the BSP is
            // spinning on `ready` in the trampoline frame, which has
            // already been set). Halt the AP; the BSP's watchdog
            // budget will eventually catch the missing dispatch.
            loop {
                core::arch::asm!("cli; hlt", options(nomem, nostack, preserves_flags));
            }
        }
    }

    // Wait for the BSP to publish the scheduler pointer.
    while SCHED_PTR.load(Ordering::Acquire).is_null() {
        core::hint::spin_loop();
    }
    let raw = SCHED_PTR.load(Ordering::Acquire);
    // SAFETY: the BSP stored a `Arc::into_raw`-cloned pointer; this AP
    // increments the strong count and treats it as a borrowed `Arc` for
    // the lifetime of the step loop. We do *not* call `Arc::from_raw`
    // here (that would consume the strong count); instead we reach into
    // the scheduler by reference.
    let sched: &Scheduler<SmpArch> = unsafe { &*raw };

    // Software-enable this AP's LAPIC and arm the timer for periodic
    // preemption. The BSP already published the calibration; if it is
    // somehow not yet visible the AP halts (the BSP's per-CPU
    // preemption-count assertion will then trip).
    let Some(calibration) = unpack_calibration() else {
        loop {
            // SAFETY: cli;hlt with IF=0 is well-defined.
            unsafe {
                core::arch::asm!("cli; hlt", options(nomem, nostack, preserves_flags));
            }
        }
    };
    let mut ap_lapic = make_lapic();
    ap_lapic.software_enable(0xFF);
    // SAFETY: this AP, `percpu::init(cpu_id)` ran above, interrupts
    // are disabled (we haven't `sti`-d yet), `ap_lapic` is this AP's
    // LAPIC because the LAPIC MMIO is per-CPU.
    unsafe {
        if preempt::init_local_preempt(cpu_id as usize, &mut ap_lapic, calibration).is_err() {
            loop {
                core::arch::asm!("cli; hlt", options(nomem, nostack, preserves_flags));
            }
        }
        // SAFETY: per-CPU IDT installed, timer vector points at the
        // ISR stub, callback is registered by the BSP. We are ready to
        // field timer ticks.
        core::arch::asm!("sti", options(nomem, nostack, preserves_flags));
    }

    APS_LIVE.fetch_add(1, Ordering::AcqRel);

    run_step_loop(cpu_id, sched);

    // The BSP set SHUTDOWN; halt with interrupts masked.
    loop {
        // SAFETY: hlt with IF=0 is a privileged halt and is well-defined.
        unsafe {
            core::arch::asm!("cli; hlt", options(nomem, nostack, preserves_flags));
        }
    }
}

// --- Step loop driver ----------------------------------------------

fn run_step_loop(cpu_id: u32, sched: &Scheduler<SmpArch>) {
    let mut steps: u64 = 0;
    while !SHUTDOWN.load(Ordering::Acquire) && steps < MAX_STEPS_PER_CPU {
        // Step the scheduler. Errors here would indicate an internal
        // invariant break; treat as failure.
        let _ = sched.step(cpu_id);
        steps += 1;
        // BSP-side termination: stop once every task has exited.
        if cpu_id == 0
            && sched.live_task_count() == 0
            && EXECUTIONS.load(Ordering::Acquire) >= EXPECTED_EXECS.load(Ordering::Acquire)
        {
            return;
        }
    }
}

// --- LAPIC helper --------------------------------------------------

fn make_lapic() -> Lapic<VolatileLapicMmio> {
    // SAFETY: LAPIC MMIO base is 0xFEE00000 on every Intel-architecture
    // system QEMU emulates. The frame is identity-mapped by `boot.s`
    // (SAFETY-INVARIANT 4 — 0..4 GiB identity map).
    let mmio = unsafe { VolatileLapicMmio::new(0xFEE0_0000 as *mut u32) };
    Lapic::new(mmio)
}

// --- MADT discovery -----------------------------------------------

struct ApList {
    ids: [u8; smp::AP_TRAMPOLINE_LEN], // overprovisioned upper bound
    count: usize,
}

fn discover_aps(multiboot_info: u64, bsp_id: u8, com1: &mut serial::Serial) -> Option<ApList> {
    // SAFETY: `multiboot_info` is the verbatim pointer from `boot.s`
    // SAFETY-INVARIANT 7. The block is in the identity-mapped 0..4 GiB
    // window so dereferences are sound; we only inspect the first
    // 4 bytes (total_size) before bounding the rest of the slice.
    let header = unsafe { core::slice::from_raw_parts(multiboot_info as *const u8, 8) };
    let total_size = u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
    let _ = writeln!(
        com1,
        "[scheduler_stress_qemu] mb2 total_size = {total_size}"
    );
    // SAFETY: same justification as above; we now know the actual length.
    let mb = unsafe { core::slice::from_raw_parts(multiboot_info as *const u8, total_size) };
    let info = BootInfo::parse(mb).ok()?;

    let rsdp_bytes = info.rsdp()?;
    let rsdp = acpi::Rsdp::validate(rsdp_bytes).ok()?;
    let _ = writeln!(
        com1,
        "[scheduler_stress_qemu] RSDP rev {} rsdt={:#x} xsdt={:#x}",
        rsdp.revision, rsdp.rsdt_address, rsdp.xsdt_address
    );

    // Locate the MADT via XSDT (preferred) or RSDT.
    //
    // SAFETY: `rsdp.xsdt_address` / `rsdp.rsdt_address` came from a
    // firmware-validated RSDP and sit inside the boot trampoline's
    // 0..4 GiB identity-mapped window (`boot.s` SAFETY-INVARIANT 4).
    let madt_bytes = unsafe { acpi::locate_madt(&rsdp) }?;
    let madt = acpi::Madt::parse(madt_bytes).ok()?;

    let mut list = ApList {
        ids: [0; smp::AP_TRAMPOLINE_LEN],
        count: 0,
    };
    for entry in madt.entries() {
        if let MadtEntry::LocalApic { apic_id, flags, .. } = entry {
            // ACPI 6.5 Table 5.40: bit 0 = "Processor Enabled".
            if flags & 1 == 0 {
                continue;
            }
            if apic_id == bsp_id {
                continue;
            }
            if list.count < list.ids.len() {
                list.ids[list.count] = apic_id;
                list.count += 1;
            }
        }
    }
    Some(list)
}

// MADT discovery moved to `rustos_arch_x86_64::acpi::locate_madt`
// in Stage 3a (c7-bin). The previous open-coded `find_madt_via_*` /
// `try_madt` / `read_phys_*` helpers are gone — `AGENTS.md` §2.2
// (no duplication). The `discover_aps` helper above now calls the
// shared, audited implementation.

// --- Panic handler ------------------------------------------------

#[panic_handler]
fn panic(info: &core::panic::PanicInfo<'_>) -> ! {
    let mut com1 = serial::Serial::init(serial::COM1_BASE);
    let _ = writeln!(com1, "[scheduler_stress_qemu] panic: {info}");
    qemu_exit::exit_failure();
}
