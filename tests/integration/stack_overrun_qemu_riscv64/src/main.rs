//! `plans/PI.md` guard-page fault-form (riscv64 stage G3c) QEMU
//! integration test: the **production** fault-form for a kthread kernel
//! stack — a live, scheduled kthread running on an arena-backed stack whose
//! one-page guard is *unmapped* takes a **synchronous store page fault** the
//! instant it overruns into that guard page, rather than the deferred
//! next-reschedule canary detection a heap-backed
//! `rustos_kernel_core::BoxStack` falls back to.
//!
//! ## Why this exists
//!
//! G1/G2 proved the riscv64 Sv39 block-split primitive and that a
//! boot-time guard arena can have a single page unmapped (faulting on a
//! *direct* access) without shattering the leaf the hart runs on
//! (`tests/integration/stack_guard_qemu_riscv64`). What is still unproven
//! on the board is the payoff: that an *overrunning kthread* — a task whose
//! execution runs off the bottom of its usable kernel stack — faults
//! **synchronously in hardware** under the live scheduler, instead of being
//! caught only at the next reschedule by
//! `rustos_kernel_core::KernelStack::check_guard` (the software-canary
//! fallback the heap-backed `BoxStack` uses). This vertical closes that gap
//! on the `virt` board and is the riscv64 sibling of
//! `tests/integration/stack_overrun_qemu_aarch64`.
//!
//! ## What this test asserts
//!
//! 1. Build an Sv39 `AddressSpace` identity-mapping the low 4 GiB (so the
//!    kernel's code/stack and the device MMIO stay reachable) and
//!    re-express a 2 MiB-aligned, 2 MiB guard arena (`ARENA`) at 4 KiB
//!    granularity (`AddressSpace::prepare_guard_arena`, G2), all while
//!    paging is off — so the running code/stack leaf is never broken.
//! 2. Carve one kthread stack region out of the arena, laid out exactly as
//!    `rustos_kernel_core::BoxStack` / the production `ArenaStack`:
//!    `[guard page | usable stack]`, the guard immediately *below* the
//!    usable region so a downward overrun crosses it first.
//! 3. Install the S-mode trap vector + a `fault` handler, turn paging on,
//!    then `unmap` the guard page through the Arch HAL + `flush_page` it —
//!    the production guard-page mechanism (G3b-2). The usable stack above it
//!    stays mapped.
//! 4. Build the live `rustos_kernel_sched_eevdf::Scheduler` over `RiscvArch`
//!    and admit a kthread on that stack via `spawn_kthread_with_stack` — the
//!    production runtime path, not a bare function call.
//! 5. The kthread body overruns its stack: it writes the highest byte of
//!    the guard region (the first byte a contiguous downward stack overrun
//!    crosses). Because that page is unmapped, the access raises a
//!    synchronous store page fault while the kthread is *running* (not at
//!    its next yield), and the trap is taken on the still-healthy usable
//!    stack above the guard, so the S-mode trap vector does not nest-fault.
//! 6. The handler confirms the trap is a store page fault on exactly the
//!    guard page and writes the `SiFive` Test PASS finisher. A regression
//!    that left the page mapped lets the body return cleanly; the
//!    cooperative `step` loop then drains the task and the test reports
//!    FAILURE explicitly (the guard was not enforced) rather than passing.
//!
//! ## How this differs from G1/G2 (`stack_guard`)
//!
//! `stack_guard` reads an unmapped page *directly from `kernel_main`*,
//! proving the unmap mechanism. G3c proves the *production* fault-form: the
//! unmapped page is the guard of a real kthread kernel stack, the touch
//! happens from inside a **scheduled kthread body** running on that stack,
//! and the fault is therefore the synchronous, run-time defence the
//! arena-backed `ArenaStack` relies on (its `check_guard` is a no-op — the
//! hardware fault *is* the defence), in deliberate contrast to `BoxStack`'s
//! poison-canary scan at the next switch-back.
//!
//! ## How it differs from a production kernel
//!
//! It links the `rustos-kernel-core` kthread runtime, the
//! `rustos-arch-riscv64` port, and the default `rustos-kernel-sched-eevdf`
//! policy directly and supplies its own `kernel_main`. The QEMU-exit
//! shortcut lives in this dedicated bin, never behind a Cargo feature on a
//! library crate (`AGENTS.md` §5.4.5 — fail closed).

#![cfg_attr(itest_riscv64, no_std)]
#![cfg_attr(itest_riscv64, no_main)]
#![deny(missing_docs)]

// --- Freestanding test bin (`riscv64gc-unknown-none-elf`) ----------

#[cfg(itest_riscv64)]
extern crate alloc;

#[cfg(itest_riscv64)]
mod kernel {
    use core::panic::PanicInfo;
    use core::sync::atomic::{AtomicBool, Ordering};

    use alloc::sync::Arc;

    use rustos_arch_api::mmu::AddressSpace as _;
    use rustos_arch_api::tlb::TlbShootdown as _;
    use rustos_arch_api::CpuId;
    use rustos_arch_riscv64::context_hal::ContextSwitchHal;
    use rustos_arch_riscv64::fdt::Fdt;
    use rustos_arch_riscv64::paging::{AddressSpace, PageTablePool, BLOCK_2MIB, PAGE_SIZE};
    use rustos_arch_riscv64::{
        fault, handle_panic_via_serial, qemu_exit, trap, RiscvArch, RiscvArchStorage, SERIAL_SINK,
    };
    use rustos_bumpalloc::{BumpAllocator, Heap, HEAP_BYTES};
    use rustos_kernel_core::{spawn_kthread_with_stack, KernelStack, KTHREAD_STACK_BYTES};
    use rustos_kernel_sched_eevdf::{Priority, Scheduler, SchedulerConfig};
    use rustos_log::{log, Event, EventId, Field, Level};

    /// The single-hart slice runs logical CPU 0 on the boot hart.
    const BOOT_CPU: CpuId = 0;

    /// Gigapages of identity map the space installs: `[0, 4 GiB)` covers
    /// the `virt` board's low MMIO and the 2 GiB RAM base at `0x8000_0000`
    /// where this kernel (and `ARENA`) runs.
    const IDENTITY_GIB: usize = 4;

    /// Width of the kthread-stack guard region: one 4 KiB page, matching
    /// `rustos_kernel_core::BoxStack` and the production `ArenaStack` (the
    /// guard sits immediately *below* the usable stack).
    const STACK_GUARD_BYTES: u64 = PAGE_SIZE as u64;

    /// Offset of the kthread stack region within the arena. Chosen well
    /// inside the arena (page 4) so the guard page has mapped neighbours on
    /// both sides.
    const STACK_REGION_OFFSET: u64 = 4 * PAGE_SIZE as u64;

    /// Cooperative-loop watchdog: maximum `step` iterations before the test
    /// declares the workload drained without faulting (a guard regression).
    /// Sized generously for QEMU TCG; the expected drain faults on the very
    /// first dispatch.
    const MAX_STEPS: u64 = 1_000_000;

    /// Stable audit-event ids for the QEMU transcript (in the 4300 block
    /// shared by the G1/G2 guard-page verticals; the riscv64 `stack_guard`
    /// vertical owns 4310–4312).
    const SO_TEST_START: EventId = EventId(4313);
    const SO_TEST_SPAWNED: EventId = EventId(4314);
    const SO_TEST_PASS: EventId = EventId(4315);
    const SO_TEST_FAIL: EventId = EventId(4316);

    /// `SiFive` Test failure codes, distinct per failure site so a failing
    /// run's exit status pinpoints the broken invariant.
    const FAIL_NO_TIMEBASE: u16 = 1;
    const FAIL_SETUP: u16 = 2;
    const FAIL_SPAWN: u16 = 3;
    const FAIL_WRONG_CAUSE: u16 = 4;
    const FAIL_WRONG_STVAL: u16 = 5;
    const FAIL_NO_FAULT: u16 = 6;

    /// Page-table pool backing the address space (lives in `.bss`).
    static POOL: PageTablePool = PageTablePool::new();

    /// Per-CPU bookkeeping backing for this single-hart vertical
    /// (`AGENTS.md` §24.1): one slot, owned by an allocator-free `static`.
    static STORAGE: RiscvArchStorage<1> = RiscvArchStorage::new();

    /// The arena's byte length, expressed in `usize` (= 2 MiB = 512 × the
    /// 4 KiB `PAGE_SIZE`) so the array type below needs no `u64`→`usize`
    /// cast. The const-assert ties it to the Arch HAL's `BLOCK_2MIB` so the
    /// two can never drift (`AGENTS.md` §2.2).
    const ARENA_BYTES: usize = 512 * PAGE_SIZE;
    const _: () = assert!(ARENA_BYTES as u64 == BLOCK_2MIB);

    /// The kthread-stack guard arena: a 2 MiB-aligned, 2 MiB region. Its
    /// alignment and size force it to occupy a whole megapage of its own, so
    /// re-expressing it at 4 KiB granularity (and unmapping a guard page
    /// inside it) never disturbs the megapage holding the running code, boot
    /// stack, or heap. Its physical address is its identity-mapped virtual
    /// address.
    #[repr(C, align(0x20_0000))]
    struct Arena([u8; ARENA_BYTES]);
    static mut ARENA: Arena = Arena([0; ARENA_BYTES]);

    /// `true` once the guard page has been unmapped — lets [`on_fault`] tell
    /// the *expected* overrun fault from a kernel bug that faults earlier.
    static GUARD_UNMAPPED: AtomicBool = AtomicBool::new(false);

    /// Base of the kthread stack region inside the arena: the low edge of
    /// its one-page guard.
    fn region_base() -> u64 {
        core::ptr::addr_of!(ARENA) as u64 + STACK_REGION_OFFSET
    }

    /// Base (4 KiB-aligned) of the guard page the spawn path unmaps.
    fn guard_page() -> u64 {
        region_base()
    }

    /// The address the kthread body writes to overrun its stack: the highest
    /// byte of the guard region — the first byte a contiguous downward stack
    /// overrun crosses. It lies inside the (unmapped) guard page, so the
    /// access faults synchronously.
    fn overrun_target() -> u64 {
        guard_page() + STACK_GUARD_BYTES - 1
    }

    /// Static boot heap, placed in the linker's dedicated `.heap` (NOLOAD)
    /// section so the boot trampoline does not zero it. `static mut` because
    /// the bump allocator hands out disjoint slices via an atomic cursor;
    /// the storage is otherwise never aliased.
    #[link_section = ".heap"]
    static mut HEAP: Heap = Heap::ZERO;

    /// Global allocator backed by [`HEAP`].
    ///
    /// SAFETY: the page-aligned `HEAP` static outlives the binary and the
    /// allocator is its only consumer.
    #[global_allocator]
    static ALLOCATOR: BumpAllocator =
        unsafe { BumpAllocator::new(core::ptr::addr_of!(HEAP) as *mut u8, HEAP_BYTES) };

    /// A kthread kernel stack carved from the arena, laid out
    /// `[guard page | usable stack]` exactly like the production
    /// `ArenaStack`. Its guard page is unmapped before the kthread runs, so
    /// [`KernelStack::check_guard`] keeps the default `Ok(())` — the hardware
    /// fault is the defence, not a canary scan.
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

    /// The synchronous-exception handler the trap vector invokes. The
    /// kthread's overrun write must land here as a store page fault on
    /// exactly the guard page; anything else is a closed failure. Never
    /// returns (`AGENTS.md` §2.9).
    extern "C" fn on_fault(scause: u64, stval: u64, _sepc: u64) -> ! {
        let base = guard_page();
        if !GUARD_UNMAPPED.load(Ordering::SeqCst) {
            note(
                Level::Error,
                SO_TEST_FAIL,
                "riscv64 stack-overrun test: fault before unmap — kernel bug",
            );
            qemu_exit::exit_failure(FAIL_SETUP);
        }
        if scause != fault::SCAUSE_STORE_PAGE_FAULT {
            note(
                Level::Error,
                SO_TEST_FAIL,
                "riscv64 stack-overrun test: unexpected trap cause, not a store page fault",
            );
            qemu_exit::exit_failure(FAIL_WRONG_CAUSE);
        }
        if stval < base || stval >= base + STACK_GUARD_BYTES {
            note(
                Level::Error,
                SO_TEST_FAIL,
                "riscv64 stack-overrun test: store page fault at the wrong address",
            );
            qemu_exit::exit_failure(FAIL_WRONG_STVAL);
        }
        note(
            Level::Info,
            SO_TEST_PASS,
            "riscv64 stack-overrun test: kthread overran into the unmapped guard page",
        );
        qemu_exit::exit_success();
    }

    /// Forward to the shared riscv64 panic bridge (parks the hart; the run
    /// then times out and the harness reports the failure).
    #[panic_handler]
    fn rustos_stack_overrun_riscv64_panic(info: &PanicInfo<'_>) -> ! {
        handle_panic_via_serial(info)
    }

    /// Emit a transcript marker through the serial sink.
    fn note(level: Level, id: EventId, message: &'static str) {
        log(
            &SERIAL_SINK,
            &Event {
                level,
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
        note(
            Level::Info,
            SO_TEST_START,
            "riscv64 stack-overrun test: arming an unmapped kthread-stack guard page",
        );

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

        let arena_base = core::ptr::addr_of!(ARENA) as u64;
        let guard = guard_page();

        // Build the identity space (the arena is mapped by a coarse leaf)
        // and re-express the arena's block at 4 KiB granularity. Done while
        // paging is off, so the running region's mapping is never broken (it
        // only adds table levels reproducing the translation).
        let Some(mut space) = AddressSpace::new_identity_gigapages(&POOL, IDENTITY_GIB) else {
            fail("identity map", FAIL_SETUP);
        };
        if space.prepare_guard_arena(arena_base, BLOCK_2MIB).is_err() {
            fail("prepare_guard_arena", FAIL_SETUP);
        }

        // Install the trap vector + fault handler before turning paging on
        // so the kthread's deliberate overrun is routed to `on_fault`.
        if fault::set_fault_handler(on_fault).is_err() {
            fail("set_fault_handler", FAIL_SETUP);
        }
        // SAFETY: called once on the boot hart with a stack established and
        // the fault handler installed.
        unsafe {
            trap::init_traps();
        }

        // Switch to the space (turns paging on). The running code/stack/heap
        // megapage stayed mapped, so execution continues.
        // SAFETY: the space identity-maps `pc`, `sp`, the heap, and MMIO;
        // preparing the arena only re-expressed its own block at finer
        // granularity.
        unsafe {
            space.activate();
        }

        // Tear the kthread stack's guard page down through the Arch HAL and
        // flush its stale TLB entry — exactly the production guard-page
        // mechanism (G3b-2). The usable stack above it stays mapped.
        if space.unmap(guard).is_err() {
            fail("unmap guard page", FAIL_SETUP);
        }
        space.flush_page(guard);
        GUARD_UNMAPPED.store(true, Ordering::SeqCst);

        // Build the live scheduler over the arch port and admit a kthread on
        // the arena-backed, guard-unmapped stack. The body overruns into the
        // guard page on its first (and only) dispatch.
        let arch = Arc::new(RiscvArch::new(&STORAGE, BOOT_CPU, timebase));
        let _ = hartid;
        let Ok(sched) = Scheduler::new(SchedulerConfig::defaults_for(1), arch) else {
            fail("scheduler new", FAIL_SETUP);
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
            fail("spawn kthread", FAIL_SPAWN);
        }
        note(
            Level::Info,
            SO_TEST_SPAWNED,
            "riscv64 stack-overrun test: kthread spawned on the guarded arena stack",
        );

        // Drive the cooperative dispatch loop. The first `step` enters the
        // kthread, whose overrun faults into `on_fault` (which exits PASS).
        // If the guard page were wrongly left mapped the body would return
        // (Exit) and the loop would drain — a guard regression we report
        // below rather than letting it pass silently (`AGENTS.md` §2.9).
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
                    "riscv64 stack-overrun test: kthread overran the guard page without faulting",
                fields: &[Field {
                    key: "drained",
                    value: if sched.live_task_count() == 0 {
                        "yes"
                    } else {
                        "timeout"
                    },
                }],
            },
        );
        qemu_exit::exit_failure(FAIL_NO_FAULT);
    }

    /// Log a setup failure and report it to QEMU. Never returns.
    fn fail(what: &'static str, code: u16) -> ! {
        log(
            &SERIAL_SINK,
            &Event {
                level: Level::Error,
                id: SO_TEST_FAIL,
                message: "riscv64 stack-overrun test: setup failed",
                fields: &[Field {
                    key: "stage",
                    value: what,
                }],
            },
        );
        qemu_exit::exit_failure(code);
    }
}

// --- Host stub -----------------------------------------------------
#[cfg(not(itest_riscv64))]
fn main() {}

#[cfg(not(itest_riscv64))]
#[allow(dead_code)]
fn _suppress_no_main() {}
