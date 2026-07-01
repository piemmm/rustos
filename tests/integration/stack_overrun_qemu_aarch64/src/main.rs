//! `plans/PI.md` guard-page fault-form (stage G3c) QEMU integration test:
//! the **production** fault-form for a kthread kernel stack — a live,
//! scheduled kthread running on an arena-backed stack whose one-page guard
//! is *unmapped* takes a **synchronous data abort** the instant it overruns
//! into that guard page, rather than the deferred next-reschedule canary
//! detection a heap-backed `rustos_kernel_core::BoxStack` falls back to.
//!
//! ## Why this exists
//!
//! G1 proved the page-table block-split primitive, and G2 proved a
//! boot-time guard arena can have a single page unmapped (faulting on a
//! *direct* access) without shattering the block the CPU runs on. G3b-2
//! then routed the real spawn paths (`init` and the runtime `spawn`
//! syscall) through that arena, unmapping each kthread stack's guard page
//! in the owning task's root. What was still unproven on the board is the
//! payoff: that an *overrunning kthread* — a task whose execution runs off
//! the bottom of its usable kernel stack — faults **synchronously in
//! hardware** under the live scheduler, instead of being caught only at the
//! next reschedule by `rustos_kernel_core::KernelStack::check_guard` (the
//! software-canary fallback the heap-backed `BoxStack` uses). This vertical
//! closes that gap on the `virt` board.
//!
//! ## What this test asserts
//!
//! 1. Build a stage-1 `AddressSpace` identity-mapping the low 2 GiB and
//!    re-express a 2 MiB-aligned, 2 MiB guard arena (`ARENA`) at 4 KiB
//!    granularity (`AddressSpace::prepare_guard_arena`, G2), all while the
//!    space is inactive — so the running code/stack block is never broken.
//! 2. Carve one kthread stack region out of the arena, laid out exactly as
//!    `rustos_kernel_core::BoxStack` / the production `ArenaStack`:
//!    `[guard page | usable stack]`, the guard immediately *below* the
//!    usable region so a downward overrun crosses it first.
//! 3. Install the EL1 vectors + a `fault` handler, enable the MMU, then
//!    `unmap` the guard page through the Arch HAL + `flush_page` it — the
//!    production guard-page mechanism (G3b-2). The usable stack above it
//!    stays mapped.
//! 4. Build the live `rustos_kernel_sched_eevdf::Scheduler` over
//!    `Aarch64Arch` and admit a kthread on that stack via
//!    `spawn_kthread_with_stack` — the production runtime path, not a bare
//!    function call.
//! 5. The kthread body overruns its stack: it writes the highest byte of
//!    the guard region (the first byte a contiguous downward stack overrun
//!    crosses). Because that page is unmapped, the access raises a
//!    synchronous data abort while the kthread is *running* (not at its next
//!    yield), and the abort is taken on the still-healthy usable stack above
//!    the guard, so the EL1 trampoline does not nest-fault.
//! 6. The handler confirms the trap is a data abort on exactly the guard
//!    page and reports PASS via the ARM semihosting finisher. A regression
//!    that left the page mapped lets the body return cleanly; the
//!    cooperative `step` loop then drains the task and the test reports
//!    FAILURE explicitly (the guard was not enforced) rather than passing.
//!
//! ## How this differs from G2 (`stack_arena`)
//!
//! G2 reads an unmapped arena page *directly from `kernel_main`*, proving
//! the unmap mechanism. G3c proves the *production* fault-form: the
//! unmapped page is the guard of a real kthread kernel stack, the touch
//! happens from inside a **scheduled kthread body** running on that stack,
//! and the abort is therefore the synchronous, run-time defence the
//! arena-backed `ArenaStack` relies on (its `check_guard` is a no-op — the
//! hardware fault *is* the defence), in deliberate contrast to `BoxStack`'s
//! poison-canary scan at the next switch-back.
//!
//! ## How it differs from a production kernel
//!
//! It links the `rustos-kernel-core` kthread runtime, the
//! `rustos-arch-aarch64` port, and the default `rustos-kernel-sched-eevdf`
//! policy directly and supplies its own `kernel_main`. The QEMU-exit
//! shortcut lives in this dedicated bin, never behind a Cargo feature on a
//! library crate (fail closed).

#![cfg_attr(itest_aarch64, no_std)]
#![cfg_attr(itest_aarch64, no_main)]
#![deny(missing_docs)]

// --- Freestanding test bin (`aarch64-unknown-none`) ----------------

#[cfg(itest_aarch64)]
extern crate alloc;

#[cfg(itest_aarch64)]
mod kernel {
    use core::panic::PanicInfo;

    use alloc::sync::Arc;

    use rustos_arch_aarch64::context_hal::ContextSwitchHal;
    use rustos_arch_aarch64::kernel_arch::timer_frequency_hz;
    use rustos_arch_aarch64::paging::{AddressSpace, PageTablePool, BLOCK_2MIB, PAGE_SIZE};
    use rustos_arch_aarch64::{
        exceptions, fault, gic, handle_panic_via_serial, qemu_exit, Aarch64Arch, SERIAL_SINK,
    };
    use rustos_arch_api::mmu::AddressSpace as _;
    use rustos_arch_api::tlb::TlbShootdown as _;
    use rustos_arch_api::CpuId;
    use rustos_fdt::Fdt;
    use rustos_kalloc::{FreeListAllocator, Heap, HEAP_BYTES};
    use rustos_kernel_core::{spawn_kthread_with_stack, KernelStack, KTHREAD_STACK_BYTES};
    use rustos_kernel_sched_eevdf::{Priority, Scheduler, SchedulerConfig};
    use rustos_log::{log, Event, EventId, Field, Level};

    // The canonical QEMU `virt` device tree, dumped and embedded at build
    // time (`build.rs`): the GICv2 base and the timer frequency are read
    // from it (P4).
    include!(concat!(env!("OUT_DIR"), "/dtb_fixture.rs"));

    /// The single-core slice runs logical CPU 0 on the boot core.
    const BOOT_CPU: CpuId = 0;

    /// Number of GiB the space identity-maps (device MMIO + RAM). The
    /// kernel image, boot stack, heap, and `ARENA` all live in the Normal
    /// RAM gigapage (GiB 1).
    const IDENTITY_GIB: usize = 2;

    /// Width of the kthread-stack guard region: one 4 KiB page, matching
    /// `rustos_kernel_core::BoxStack` and the production `ArenaStack` (the
    /// guard sits immediately *below* the usable stack).
    const STACK_GUARD_BYTES: u64 = PAGE_SIZE as u64;

    /// Offset of the kthread stack region within the arena. Chosen well
    /// inside the arena (page 4) so the guard page has mapped neighbours on
    /// both sides — the test also checks a neighbour stays mapped.
    const STACK_REGION_OFFSET: u64 = 4 * PAGE_SIZE as u64;

    /// Cooperative-loop watchdog: maximum `step` iterations before the test
    /// declares the workload drained without faulting (a guard regression).
    /// Sized generously for QEMU TCG; the expected drain faults on the very
    /// first dispatch.
    const MAX_STEPS: u64 = 1_000_000;

    /// Stable audit-event ids for the QEMU transcript (in the 4300 block
    /// shared by the G1/G2 guard-page verticals).
    const SO_TEST_START: EventId = EventId(4306);
    const SO_TEST_SPAWNED: EventId = EventId(4307);
    const SO_TEST_PASS: EventId = EventId(4308);
    const SO_TEST_FAIL: EventId = EventId(4309);

    /// A deliberately-wrong GICv2 base installed before discovery runs, so
    /// reaching the `virt` distributor base can only mean discovery
    /// overwrote it from the device tree (`plans/PI.md` P4).
    const POISON_GIC_BASE: usize = 0xdead_0000;

    /// Page-table pool backing the address space (lives in `.bss`).
    static POOL: PageTablePool = PageTablePool::new();

    /// The kthread-stack guard arena: a 2 MiB-aligned, 2 MiB region. Its
    /// alignment and size force it to occupy a whole L2 block of its own,
    /// so re-expressing it at 4 KiB granularity (and unmapping a guard page
    /// inside it) never disturbs the 2 MiB block holding the running code,
    /// boot stack, or heap. Its physical address is its identity-mapped
    /// virtual address.
    #[repr(C, align(0x20_0000))]
    struct Arena([u8; BLOCK_2MIB as usize]);
    static mut ARENA: Arena = Arena([0; BLOCK_2MIB as usize]);

    /// Base of the kthread stack region inside the arena: the low edge of
    /// its one-page guard.
    fn region_base() -> u64 {
        core::ptr::addr_of!(ARENA) as u64 + STACK_REGION_OFFSET
    }

    /// Base (4 KiB-aligned) of the guard page the spawn path unmaps.
    fn guard_page() -> u64 {
        region_base()
    }

    /// The address the kthread body writes to overrun its stack: the
    /// highest byte of the guard region — the first byte a contiguous
    /// downward stack overrun crosses. It lies inside the (unmapped) guard
    /// page, so the access faults synchronously.
    fn overrun_target() -> u64 {
        guard_page() + STACK_GUARD_BYTES - 1
    }

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

    /// A kthread kernel stack carved from the arena, laid out
    /// `[guard page | usable stack]` exactly like the production
    /// `ArenaStack`. Its guard page is unmapped before the kthread runs, so
    /// [`KernelStack::check_guard`] keeps the default `Ok(())` — the
    /// hardware fault is the defence, not a canary scan.
    #[derive(Copy, Clone)]
    struct ArenaTestStack {
        guard: u64,
    }

    // SAFETY: `top` returns the region base plus the guard page plus the
    // usable `KTHREAD_STACK_BYTES`, rounded down to the 16-byte ABI
    // alignment. The usable region `[guard + STACK_GUARD_BYTES, top)` is a
    // sub-range of the identity-mapped, single-owner `ARENA` static that
    // outlives the binary; only the guard page below it is unmapped. The
    // arena hands this region to exactly one kthread, so it is exclusive.
    unsafe impl KernelStack for ArenaTestStack {
        fn top(&self) -> u64 {
            let top = self.guard + STACK_GUARD_BYTES + KTHREAD_STACK_BYTES as u64;
            top & !0xF
        }
    }

    /// The fault handler: confirm the trap is a data/instruction abort on
    /// the unmapped guard page, then report PASS. Anything else is a
    /// FAILURE. Never returns.
    extern "C" fn on_fault(esr: u64, far: u64, _elr: u64) -> ! {
        let base = guard_page();
        if fault::is_abort(esr) && far >= base && far < base + STACK_GUARD_BYTES {
            log(
                &SERIAL_SINK,
                &Event {
                    level: Level::Info,
                    id: SO_TEST_PASS,
                    message:
                        "aarch64 stack-overrun test: kthread overran into the unmapped guard page",
                    fields: &[],
                },
            );
            qemu_exit::exit_success();
        }
        log(
            &SERIAL_SINK,
            &Event {
                level: Level::Error,
                id: SO_TEST_FAIL,
                message: "aarch64 stack-overrun test: unexpected fault",
                fields: &[],
            },
        );
        qemu_exit::exit_failure(3);
    }

    /// Forward to the shared aarch64 panic bridge (parks the CPU; the run
    /// then times out and the harness reports the failure).
    #[panic_handler]
    fn rustos_stack_overrun_aarch64_panic(info: &PanicInfo<'_>) -> ! {
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
        note(
            SO_TEST_START,
            "aarch64 stack-overrun test: arming an unmapped kthread-stack guard page",
        );

        // P4: read the board from the embedded `virt` device tree.
        let Ok(fdt) = Fdt::new(DTB_BLOB) else {
            fail("parse virt dtb");
        };
        let counter_hz = timer_frequency_hz(&fdt);
        if counter_hz == 0 {
            fail("zero timer frequency");
        }

        let arena_base = core::ptr::addr_of!(ARENA) as u64;
        let guard = guard_page();

        // Build the identity space (the arena is mapped by a coarse block)
        // and re-express the arena's block at 4 KiB granularity. Done while
        // the space is inactive, so the running region's mapping is never
        // broken (it only adds table levels reproducing the translation).
        let mut space = AddressSpace::new_identity_gigapages(&POOL, IDENTITY_GIB)
            .unwrap_or_else(|| fail("identity map"));
        space
            .prepare_guard_arena(arena_base, BLOCK_2MIB)
            .unwrap_or_else(|_| fail("prepare_guard_arena"));

        // Install the vectors + fault handler before enabling the MMU so the
        // kthread's deliberate overrun is routed to `on_fault`.
        fault::set_fault_handler(on_fault).unwrap_or_else(|_| fail("set_fault_handler"));
        // SAFETY: called once on the boot CPU before any fault can fire.
        unsafe {
            exceptions::init_vectors();
        }

        // Switch to the space (enables the MMU). The running code/stack/heap
        // block stayed a coarse block and is identity-mapped, so execution
        // continues.
        // SAFETY: the space identity-maps `pc`, `sp`, the heap, and MMIO;
        // preparing the arena only re-expressed its own block at finer
        // granularity.
        unsafe {
            space.activate();
        }

        // Tear the kthread stack's guard page down through the Arch HAL and
        // flush its stale TLB entry — exactly the production guard-page
        // mechanism (G3b-2). The usable stack above it stays mapped.
        space
            .unmap(guard)
            .unwrap_or_else(|_| fail("unmap guard page"));
        space.flush_page(guard);

        // Discover + bring up the GICv2 so the scheduler's spawn IPIs target
        // a configured controller (the kthread switches here are the
        // cooperative `step` loop, but the scheduler still configures the
        // controller on spawn). Prove the base is *discovered*, not assumed.
        gic::configure(POISON_GIC_BASE, POISON_GIC_BASE);
        if gic::configure_from_fdt(&fdt).is_none() || gic::current().0 != gic::DEFAULT_GICD_BASE {
            fail("gic not discovered");
        }
        // SAFETY: called once on the boot core with a stack established and
        // the vectors installed.
        unsafe {
            gic::init();
        }

        // Build the live scheduler over the arch port and admit a kthread on
        // the arena-backed, guard-unmapped stack. The body overruns into the
        // guard page on its first (and only) dispatch.
        // Per-CPU bookkeeping backing for this single-CPU vertical.
        static ARCH_STORAGE: rustos_arch_aarch64::Aarch64ArchStorage<1> =
            rustos_arch_aarch64::Aarch64ArchStorage::new();
        let arch = Arc::new(Aarch64Arch::new(&ARCH_STORAGE, BOOT_CPU, counter_hz));
        let Ok(sched) = Scheduler::new(SchedulerConfig::defaults_for(1), arch) else {
            fail("scheduler new");
        };

        let target = overrun_target();
        let spawned = spawn_kthread_with_stack(
            &sched,
            ContextSwitchHal::new(),
            ArenaTestStack { guard },
            BOOT_CPU,
            Priority::Normal,
            move |_yielder| {
                // Overrun the usable stack into the (unmapped) guard page:
                // touch the highest guard byte, the first byte a contiguous
                // downward overrun crosses. This must fault synchronously.
                // SAFETY: the access is *expected* to fault — the guard page
                // is unmapped. The write is volatile so it is not elided; if
                // the MMU wrongly permitted it the body simply returns and
                // the drain loop below reports the guard FAILURE.
                unsafe {
                    core::ptr::write_volatile(target as *mut u8, 0xA5);
                }
            },
        );
        if spawned.is_err() {
            fail("spawn kthread");
        }
        note(
            SO_TEST_SPAWNED,
            "aarch64 stack-overrun test: kthread spawned on the guarded arena stack",
        );

        // Drive the cooperative dispatch loop. The first `step` enters the
        // kthread, whose overrun faults into `on_fault` (which exits PASS).
        // If the guard page were wrongly left mapped the body would return
        // (Exit) and the loop would drain — a guard regression we report
        // below rather than letting it pass silently.
        let mut steps = 0u64;
        while sched.live_task_count() != 0 && steps < MAX_STEPS {
            let _ = sched.step(BOOT_CPU);
            steps += 1;
        }

        // Reaching here means the kthread overran without faulting — the
        // guard was not enforced. Fail loudly.
        log(
            &SERIAL_SINK,
            &Event {
                level: Level::Error,
                id: SO_TEST_FAIL,
                message:
                    "aarch64 stack-overrun test: kthread overran the guard page without faulting",
                fields: &[Field {
                    key: "drained",
                    value: rustos_log::FieldValue::Str(if sched.live_task_count() == 0 {
                        "yes"
                    } else {
                        "timeout"
                    }),
                }],
            },
        );
        qemu_exit::exit_failure(2);
    }

    /// Log a setup failure and report it to QEMU. Never returns.
    fn fail(what: &'static str) -> ! {
        log(
            &SERIAL_SINK,
            &Event {
                level: Level::Error,
                id: SO_TEST_FAIL,
                message: "aarch64 stack-overrun test: setup failed",
                fields: &[Field {
                    key: "stage",
                    value: rustos_log::FieldValue::Str(what),
                }],
            },
        );
        qemu_exit::exit_failure(4);
    }
}

// --- Host stub -----------------------------------------------------
#[cfg(not(itest_aarch64))]
fn main() {}

#[cfg(not(itest_aarch64))]
#[allow(dead_code)]
fn _suppress_no_main() {}
