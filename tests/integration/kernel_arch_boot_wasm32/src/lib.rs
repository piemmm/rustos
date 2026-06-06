//! Browser-headless integration test for the wasm32 Arch HAL.
//!
//! Built for `wasm32-unknown-unknown` this `cdylib` is the wasm32
//! analogue of the bare-metal `kernel_arch_boot_*` / `sched_drive_qemu_*`
//! QEMU verticals. It exercises the wasm32 deliverables (`PLAN.md` Stage 3
//! + `plans/WIRING.md` Stage W8) in a real browser:
//!
//! * **Boots to `init`.** `kernel_main` brings the `WasmArch` handle up
//!   on the main thread (logical CPU 0) and prints `BOOT_OK`.
//! * **Per-worker memory isolation.** Every context builds an
//!   `AddressSpace` over its *own* live WASM linear memory
//!   (`isolation::live_memory_region`) and confirms an attacker confined
//!   to a disjoint region — standing in for another worker's separate
//!   linear memory — faults on this context's bytes, printing
//!   `ISOLATION_OK`.
//! * **Live `kernel/sched` scheduler driven by the frame tick.** CPU 0
//!   builds a real `rustos_kernel_sched_mlfq::Scheduler` over
//!   `WasmArch`, installs the `preempt` tick callback so each
//!   `requestAnimationFrame` frame drives `Scheduler::on_timer_tick` and
//!   dispatches a task via `step`, and prints `TICK` per frame.
//! * **Multi-worker SMP + cross-context IPI.** CPU 0 spawns a real Web
//!   Worker as logical CPU 1 (`smp::start_worker`); CPU 1 boots its own
//!   live scheduler and prints `WORKER_OK`. CPU 0 then sends CPU 1 a
//!   directed IPI (`SchedulerArch::send_ipi`); the `MessageChannel`
//!   delivery drives CPU 1's live scheduler, which prints `IPI_RECV`.
//!
//! The browser harness (`web/harness.mjs`, launched by `cargo xtask test
//! --wasm`) scrapes those console markers and reports PASS once it has
//! seen `BOOT_OK`, `ISOLATION_OK`, `WORKER_OK`, `IPI_RECV`, and at least
//! twenty `TICK`s; any panic traps the instance and fails the run loudly
//! (`AGENTS.md` §7).
//!
//! On a host build (`itest_wasm32` off) this compiles to an inert empty
//! `cdylib`, exactly as the bare-metal verticals are inert host stubs.
#![cfg_attr(itest_wasm32, no_std)]
#![deny(missing_docs)]

#[cfg(itest_wasm32)]
extern crate alloc;

#[cfg(itest_wasm32)]
mod kernel {
    use core::panic::PanicInfo;
    use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, Ordering};

    use alloc::sync::Arc;

    use rustos_arch_api::{CpuId, SchedulerArch, SecondaryBringup};
    use rustos_arch_wasm32::console::write_line;
    use rustos_arch_wasm32::isolation::{live_memory_region, AddressSpace, MemoryRegion};
    use rustos_arch_wasm32::{handle_panic_via_console, preempt, smp, WasmArch};
    use rustos_bumpalloc::{BumpAllocator, Heap, HEAP_BYTES};
    use rustos_kernel_sched_mlfq::{Priority, Scheduler, SchedulerConfig, TaskAction};

    /// The main browser thread is logical CPU 0; the spawned worker is 1.
    const BOOT_CPU: CpuId = 0;
    const WORKER_CPU: CpuId = 1;

    /// Tasks CPU 0 spawns and dispatches through the live scheduler's
    /// cooperative `step` loop, one per frame tick.
    const PRIMARY_TASKS: u64 = 16;

    /// Tasks the secondary worker spawns and dispatches through its own
    /// `setTimeout`-driven tick loop.
    const WORKER_TASKS: u64 = 4;

    /// Per-instance boot heap. Each WebAssembly instance (the main thread
    /// and every worker) has its own linear memory, so each carries its
    /// own copy of this static — no cross-instance aliasing is possible
    /// (the wasm32 isolation boundary).
    ///
    /// `static mut` because the bump allocator hands out disjoint slices
    /// via an atomic cursor; the storage is otherwise never aliased.
    static mut HEAP: Heap = Heap::ZERO;

    /// Global allocator backed by [`HEAP`].
    ///
    /// SAFETY: the `HEAP` static outlives the instance and the allocator
    /// is its only consumer.
    #[global_allocator]
    static ALLOCATOR: BumpAllocator =
        unsafe { BumpAllocator::new(core::ptr::addr_of!(HEAP) as *mut u8, HEAP_BYTES) };

    /// This instance's published `Scheduler<WasmArch>` raw pointer, which
    /// the frame-tick and IPI callbacks consult. Each instance publishes
    /// its own (from a leaked `Arc`) before arming its tick source; the
    /// pointee is never freed, so the callbacks read it without touching
    /// the `Arc` strong count.
    static SCHED: AtomicPtr<Scheduler<WasmArch>> = AtomicPtr::new(core::ptr::null_mut());

    /// Number of task bodies that have executed in this instance.
    static EXECUTIONS: AtomicU64 = AtomicU64::new(0);

    /// Set `true` the first time a cross-context IPI drives this
    /// instance's scheduler, so the `IPI_RECV` marker is emitted once.
    static IPI_DROVE_SCHED: AtomicBool = AtomicBool::new(false);

    /// Forward this module's panics to the shared console bridge, which
    /// emits one record and traps the instance (`AGENTS.md` §2.9).
    #[panic_handler]
    fn panic(info: &PanicInfo<'_>) -> ! {
        handle_panic_via_console(info)
    }

    /// Borrow this instance's published scheduler, if any.
    fn scheduler() -> Option<&'static Scheduler<WasmArch>> {
        let raw = SCHED.load(Ordering::Acquire);
        if raw.is_null() {
            None
        } else {
            // SAFETY: the boot path publishes `raw` from a leaked `Arc`
            // before any tick or IPI is armed; the pointee outlives every
            // callback because the leaked strong count is never dropped.
            Some(unsafe { &*raw })
        }
    }

    /// Build a live scheduler managing `cpus` logical CPUs over `arch`,
    /// publish it for the callbacks, and return the borrow. `cpus` must
    /// cover this context's logical id (the per-CPU run queues are indexed
    /// by it). Panics (trapping the instance) if the scheduler cannot be
    /// constructed, so a regression fails loudly.
    fn publish_scheduler(arch: Arc<WasmArch>, cpus: u32) -> &'static Scheduler<WasmArch> {
        let sched = Scheduler::new(SchedulerConfig::defaults_for(cpus), arch)
            .expect("scheduler construction must succeed in the test");
        let sched = Arc::new(sched);
        SCHED.store(
            Arc::into_raw(Arc::clone(&sched)).cast_mut(),
            Ordering::Release,
        );
        // SAFETY: as `scheduler()` — the leaked strong count keeps the
        // pointee alive for the rest of the run.
        scheduler().expect("scheduler was just published")
    }

    /// Spawn `count` trivial tasks on `cpu`; each increments the
    /// execution counter and exits. Panics (fail loud) if a spawn fails.
    fn spawn_tasks(sched: &Scheduler<WasmArch>, cpu: CpuId, count: u64) {
        for _ in 0..count {
            sched
                .spawn(cpu, Priority::Normal, move |_ctx| {
                    EXECUTIONS.fetch_add(1, Ordering::Relaxed);
                    TaskAction::Exit
                })
                .expect("task spawn must succeed in the test");
        }
    }

    /// CPU 0's frame-tick callback: drive the live scheduler's preemption
    /// accounting and dispatch one ready task, then emit `TICK`. The
    /// harness counts the ticks to prove the `requestAnimationFrame` loop
    /// reaches the live scheduler.
    extern "C" fn on_primary_tick(cpu: CpuId) {
        if let Some(sched) = scheduler() {
            let _ = sched.on_timer_tick(cpu);
            let _ = sched.step(cpu);
        }
        write_line("TICK");
    }

    /// The secondary worker's tick callback: drive its own live scheduler
    /// and dispatch its tasks, silently (CPU 0 owns the `TICK` markers).
    extern "C" fn on_worker_tick(cpu: CpuId) {
        if let Some(sched) = scheduler() {
            let _ = sched.on_timer_tick(cpu);
            let _ = sched.step(cpu);
        }
    }

    /// The secondary worker's IPI callback: a delivered cross-context IPI
    /// is a reschedule request, so it drives the live scheduler. Emits
    /// `IPI_RECV` the first time, proving the `MessageChannel` IPI reached
    /// the worker's live scheduler.
    extern "C" fn on_worker_ipi(cpu: CpuId) {
        if let Some(sched) = scheduler() {
            let _ = sched.on_timer_tick(cpu);
        }
        if !IPI_DROVE_SCHED.swap(true, Ordering::SeqCst) {
            write_line("IPI_RECV");
        }
    }

    /// Boot body the arch crate's exported `rustos_arch_wasm32_main`
    /// trampoline (`kernel/arch/wasm32::entry`) forwards to once the host
    /// has instantiated the module — in the main thread and in every
    /// spawned worker. It branches on this context's logical CPU id.
    ///
    /// Mirrors the bare-metal ports' `kernel_main`, but returns so the
    /// host event loop can drive the cooperative scheduler.
    #[no_mangle]
    pub extern "C" fn kernel_main() {
        // Every context proves it enforces its own linear-memory
        // isolation against its real memory (`ISOLATION_OK`).
        run_isolation_check();

        if smp::current_worker() == BOOT_CPU {
            boot_primary();
        } else {
            boot_secondary();
        }
    }

    /// Main-thread (logical CPU 0) bring-up: build the live scheduler,
    /// arm the frame tick, spawn the workload, then start a secondary
    /// worker and send it a directed IPI.
    fn boot_primary() {
        write_line("BOOT_OK");

        // Build the live scheduler over a handle that knows both CPUs, so
        // a directed IPI to CPU 1 resolves to the spawned worker.
        let arch = Arc::new(WasmArch::with_workers(BOOT_CPU, &[BOOT_CPU, WORKER_CPU]));
        // The boot context schedules only on CPU 0; the second worker map
        // entry exists solely so a directed IPI to CPU 1 routes.
        let sched = publish_scheduler(Arc::clone(&arch), 1);

        preempt::set_tick_callback(on_primary_tick);
        spawn_tasks(sched, BOOT_CPU, PRIMARY_TASKS);

        // Bring up logical CPU 1 as a real Web Worker, then deliver it a
        // directed cross-context IPI. This goes through the
        // `SecondaryBringup` Arch HAL trait (`plans/WIRING.md` Stage
        // W14/W15) rather than the port-private `smp::start_worker`, so
        // the wasm32 vertical exercises the same neutral bring-up surface
        // the bare-metal SMP verticals use. The MessageChannel buffers
        // the post until the worker is live, so a single send suffices
        // (no retry-until-it-works, `AGENTS.md` §2.1).
        //
        // SAFETY: a wasm32 secondary is a fresh module instance entered
        // through the fixed `rustos_arch_wasm32_main` export (no settable
        // entry to install), and `WORKER_CPU` maps to a real, distinct
        // worker slot in the handle's topology, distinct from CPU 0.
        if unsafe { arch.start_secondary(WORKER_CPU) }.is_err() {
            write_line("HARNESS_ERROR primary could not start worker 1");
            return;
        }
        arch.send_ipi(WORKER_CPU);

        // Arm the cooperative requestAnimationFrame loop, which drives the
        // live scheduler through `on_primary_tick`.
        preempt::init_local_preempt(BOOT_CPU);
    }

    /// Secondary-worker (logical CPU 1) bring-up: build its own live
    /// scheduler, install the tick + IPI callbacks, spawn a small
    /// workload, and arm the cooperative tick loop.
    fn boot_secondary() {
        write_line("WORKER_OK");

        let arch = Arc::new(WasmArch::new(WORKER_CPU));
        // The run queues are indexed by logical CPU id, so the worker's
        // scheduler must cover index `WORKER_CPU`.
        let sched = publish_scheduler(arch, WORKER_CPU + 1);

        preempt::set_tick_callback(on_worker_tick);
        preempt::set_ipi_callback(on_worker_ipi);
        spawn_tasks(sched, WORKER_CPU, WORKER_TASKS);

        preempt::init_local_preempt(WORKER_CPU);
    }

    /// Prove the WASM-linear-memory isolation model denies a cross-context
    /// access against *this instance's real linear memory*. Panics
    /// (trapping the instance) if isolation fails, so a regression cannot
    /// silently report success (`AGENTS.md` §5.4.5).
    fn run_isolation_check() {
        // The victim region is this worker's actual linear memory.
        let own = live_memory_region();
        let victim = AddressSpace::new(own);

        // A real, in-bounds byte: the address of a live stack local. The
        // engine owns it, so the victim space must accept it.
        let probe = 0u8;
        let secret = core::ptr::addr_of!(probe) as u64;
        assert!(
            victim.can_read(secret),
            "victim must own a live address in its own memory"
        );

        // An attacker confined to a disjoint region beyond this memory —
        // standing in for another worker's separate linear memory — must
        // fault on the victim's address and may only reach its own region.
        let attacker_base = own.end().max(0x1_0000_0000);
        let attacker = AddressSpace::new(MemoryRegion::new(attacker_base, 0x1000));
        assert!(
            attacker.check_access(secret, 1).is_err(),
            "attacker must fault on the victim-only address"
        );
        assert!(
            attacker.check_access(attacker_base, 0x1000).is_ok(),
            "attacker must reach its own region"
        );
        write_line("ISOLATION_OK");
    }
}
