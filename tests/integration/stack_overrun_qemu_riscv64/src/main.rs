//! `plans/OPEN-DEFECTS.md` D82 QEMU integration test: a live, scheduled
//! kthread running on a **window-backed** kernel stack takes a synchronous
//! store page fault the instant it overruns into the stack's guard slot.
//!
//! ## Why this exists
//!
//! A kthread kernel stack is a run of pages in the shared kernel remap
//! window whose lowest slot is reserved but never mapped. Because that
//! window's sub-hierarchy is installed by every translation root, the guard
//! is absent everywhere at once — nothing refines a live leaf, and no root
//! carries a per-task unmap. This vertical proves the payoff on the `virt`
//! board: an *overrunning kthread* faults synchronously in hardware rather
//! than being caught at its next reschedule by the software-canary
//! `tairix_kernel_core::kthread::BoxStack` fallback.
//!
//! ## What this test asserts
//!
//! 1. Reserve the kernel remap window, build a stage-1 identity space (which
//!    installs the window's shared slots), and turn paging on.
//! 2. Install the window-backed kthread stack tier over that window and draw
//!    one stack from it — the production allocation path.
//! 3. The drawn stack's usable run is mapped and writable, and its guard
//!    slot lies one page below the usable base.
//! 4. Admit a kthread on that stack through the live
//!    `tairix_kernel_sched_eevdf::Scheduler` via `spawn_kthread_with_stack`.
//! 5. The body writes the highest byte of its guard slot — the first byte a
//!    contiguous downward overrun crosses — and the unmapped slot raises a
//!    store page fault while the kthread is running.
//! 6. The handler confirms the cause and `stval` name exactly that slot and
//!    reports PASS. A slot left mapped lets the body return; the cooperative
//!    `step` loop drains and the test reports FAILURE rather than passing.
//!
//! ## How it differs from a production kernel
//!
//! It links the `tairix-kernel-core` kthread runtime, the
//! `tairix-arch-riscv64` port, and the default `tairix-kernel-sched-eevdf`
//! policy directly and supplies its own `kernel_main`, installing the window
//! and the stack tier itself rather than booting the whole pipeline. The
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
    use core::num::NonZeroU16;
    use core::panic::PanicInfo;
    use core::sync::atomic::Ordering;

    use alloc::boxed::Box;
    use alloc::sync::Arc;

    use tairix_arch_api::mmu::AddressSpace as _;
    use tairix_arch_api::CpuId;
    use tairix_arch_riscv64::context_hal::ContextSwitchHal;
    use tairix_arch_riscv64::fdt::Fdt;
    use tairix_arch_riscv64::irqmask::PortIrqControl;
    use tairix_arch_riscv64::paging::{
        reserve_kernel_window, AddressSpace, PageTablePool, PAGE_SIZE,
    };
    use tairix_arch_riscv64::{
        fault, handle_panic_via_serial, qemu_exit, trap, RiscvArch, RiscvArchStorage, SERIAL_SINK,
    };
    use tairix_itest_finisher::fail_point;
    use tairix_kalloc::{FreeListAllocator, Heap, HEAP_BYTES};
    use tairix_kernel_core::kstack::{alloc_kernel_stack, install_kernel_stacks};
    use tairix_kernel_core::{spawn_kthread_with_stack, KernelStack};
    use tairix_kernel_mem::{
        BootMemoryMap, DirectPhysMap, FrameAllocator, FrameTableSource, KernelRemap, KernelVirtMap,
        MemoryRegion, PhysAddr, RegionKind,
    };
    use tairix_kernel_sched_eevdf::{Priority, Scheduler, SchedulerConfig};
    use tairix_log::{log, Event, EventId, Field, Level};

    /// The single-hart slice runs logical CPU 0 on the boot hart.
    const BOOT_CPU: CpuId = 0;

    /// Gigapages of identity map the space installs: `[0, 4 GiB)` covers
    /// the `virt` board's low MMIO and the 2 GiB RAM base at `0x8000_0000`
    /// where this kernel and its frame pool run.
    const IDENTITY_GIB: usize = 4;

    /// Width of a kthread stack's guard region: one 4 KiB page, the slot the
    /// tier reserves and never maps immediately *below* the usable stack.
    const STACK_GUARD_BYTES: u64 = PAGE_SIZE as u64;

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
    const FAIL_NO_TIMEBASE: NonZeroU16 = fail_point!(1);
    const FAIL_SETUP: NonZeroU16 = fail_point!(2);
    const FAIL_SPAWN: NonZeroU16 = fail_point!(3);
    const FAIL_WRONG_CAUSE: NonZeroU16 = fail_point!(4);
    const FAIL_WRONG_STVAL: NonZeroU16 = fail_point!(5);
    const FAIL_NO_FAULT: NonZeroU16 = fail_point!(6);

    /// Page-table pool backing the address space (lives in `.bss`).
    static POOL: PageTablePool = PageTablePool::new();

    /// Per-CPU bookkeeping backing for this single-hart vertical: one slot, owned by an allocator-free `static`.
    static STORAGE: RiscvArchStorage<1> = RiscvArchStorage::new();

    /// Identity-mapped RAM the frame allocator hands out: the window's own
    /// leaf tables and the pages backing the drawn kthread stack. Sized well
    /// past both so exhaustion cannot be mistaken for a guard regression.
    #[repr(C, align(0x1000))]
    struct FramePool([u8; 8 * 1024 * 1024]);
    static mut FRAME_POOL: FramePool = FramePool([0; 8 * 1024 * 1024]);

    /// The guard slot of the stack under test, published once it is drawn so
    /// the fault handler can name the address it expects. Zero until then,
    /// which lets [`on_fault`] tell the *expected* overrun from a kernel bug
    /// that faults earlier.
    static GUARD_SLOT: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

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
    static ALLOCATOR: FreeListAllocator =
        unsafe { FreeListAllocator::new(core::ptr::addr_of!(HEAP) as *mut u8, HEAP_BYTES) };

    /// The synchronous-exception handler the trap vector invokes. The
    /// kthread's overrun write must land here as a store page fault on
    /// exactly the guard slot; anything else is a closed failure. Never
    /// returns.
    extern "C" fn on_fault(scause: u64, stval: u64, _sepc: u64) -> ! {
        let base = GUARD_SLOT.load(Ordering::Acquire);
        if base == 0 {
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
            "riscv64 stack-overrun test: kthread overran into the unmapped guard slot",
        );
        qemu_exit::exit_success();
    }

    /// Forward to the shared riscv64 panic bridge (parks the hart; the run
    /// then times out and the harness reports the failure).
    #[panic_handler]
    fn tairix_stack_overrun_riscv64_panic(info: &PanicInfo<'_>) -> ! {
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
    /// calls (via `tairix_arch_riscv64_main`).
    #[no_mangle]
    pub extern "C" fn kernel_main(hartid: u64, dtb: u64) -> ! {
        note(
            Level::Info,
            SO_TEST_START,
            "riscv64 stack-overrun test: drawing a window-backed kthread stack",
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

        // The frame allocator the window's leaf tables and the drawn
        // stack's pages come from. Its RAM is the identity-mapped
        // `FRAME_POOL` static, so a frame's physical address is also the
        // address the kernel reaches it at.
        let mut map = BootMemoryMap::new();
        map.push(MemoryRegion {
            kind: RegionKind::Usable,
            start: PhysAddr::new(core::ptr::addr_of!(FRAME_POOL) as u64),
            length: core::mem::size_of::<FramePool>() as u64,
        });
        let Ok(frames) = FrameAllocator::new(&map) else {
            fail("frame allocator", FAIL_SETUP);
        };
        let frames: &'static FrameAllocator = Box::leak(Box::new(frames));
        let physmap: &'static DirectPhysMap = Box::leak(Box::new(DirectPhysMap::identity(
            (IDENTITY_GIB as u64) << 30,
        )));
        let tables: &'static FrameTableSource =
            Box::leak(Box::new(FrameTableSource::new(frames, physmap)));

        // Reserve the window *before* the identity space is built, so the
        // space installs its shared sub-hierarchy slots and every stack the
        // tier hands out resolves under the root the kthread runs on.
        let Some(window) = reserve_kernel_window(tables) else {
            fail("reserve kernel window", FAIL_SETUP);
        };
        let Some(space) = AddressSpace::new_identity_gigapages(&POOL, IDENTITY_GIB) else {
            fail("identity map", FAIL_SETUP);
        };

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

        // Switch to the space (turns paging on). It identity-maps `pc`,
        // `sp`, the heap, and MMIO, and carries the window's shared slots.
        // SAFETY: every region the running code touches stays mapped across
        // the switch, which is `AddressSpace::switch`'s contract.
        unsafe {
            space.activate();
        }

        // Install the window-backed stack tier over the port's cross-CPU
        // invalidation and draw one stack from it — the production
        // allocation path.
        let arch = Arc::new(RiscvArch::new(&STORAGE, BOOT_CPU, timebase));
        let _ = hartid;
        let arch_static: &'static RiscvArch = Box::leak(Box::new(Arc::clone(&arch)));
        let Some(kwspace) = AddressSpace::new_kernel_window(tables) else {
            fail("kernel window space", FAIL_SETUP);
        };
        let kvmap: &'static dyn KernelVirtMap = Box::leak(Box::new(
            KernelRemap::<_, PortIrqControl>::new(window, kwspace, arch_static),
        ));
        install_kernel_stacks(frames, physmap, kvmap);

        let stack = alloc_kernel_stack();
        // The tier lays a stack out as `[guard slot | usable run]`, so the
        // guard is the page below the usable base. A `BoxStack` fallback
        // would have handed back heap memory outside the window, which is
        // the regression this check catches before the kthread runs.
        let usable_base = stack.top() - stack.usable_bytes();
        let guard = usable_base - STACK_GUARD_BYTES;
        if !window.contains(guard) {
            fail("stack is not window-backed", FAIL_SETUP);
        }
        GUARD_SLOT.store(guard, Ordering::Release);
        // The usable run must be mapped and writable, or the fault below
        // would prove nothing about the guard.
        // SAFETY: the run was installed by the tier and is exclusive to this
        // stack, which nothing is running on yet.
        unsafe {
            core::ptr::write_volatile(usable_base as *mut u8, 0x5A);
            if core::ptr::read_volatile(usable_base as *const u8) != 0x5A {
                fail("usable stack not writable", FAIL_SETUP);
            }
        }

        let Ok(sched) = Scheduler::new(SchedulerConfig::defaults_for(1), arch) else {
            fail("scheduler new", FAIL_SETUP);
        };

        // The first byte a contiguous downward overrun crosses: the highest
        // byte of the unmapped guard slot.
        let target = guard + STACK_GUARD_BYTES - 1;
        let spawned = spawn_kthread_with_stack(
            &sched,
            ContextSwitchHal::new(),
            stack,
            BOOT_CPU,
            Priority::Normal,
            move |_yielder| {
                // Overrun the usable stack into the (unmapped) guard slot:
                // touch the highest guard byte, the first byte a contiguous
                // downward overrun crosses. This must fault synchronously.
                // SAFETY: the access is *expected* to fault — the guard slot
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
            "riscv64 stack-overrun test: kthread spawned on the window-backed stack",
        );

        // Drive the cooperative dispatch loop. The first `step` enters the
        // kthread, whose overrun faults into `on_fault` (which exits PASS).
        // If the guard slot were wrongly left mapped the body would return
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
                    "riscv64 stack-overrun test: kthread overran the guard slot without faulting",
                fields: &[Field {
                    key: "drained",
                    value: tairix_log::FieldValue::Str(if sched.live_task_count() == 0 {
                        "yes"
                    } else {
                        "timeout"
                    }),
                }],
            },
        );
        qemu_exit::exit_failure(FAIL_NO_FAULT);
    }

    /// Log a setup failure and report it to QEMU. Never returns.
    fn fail(what: &'static str, code: NonZeroU16) -> ! {
        log(
            &SERIAL_SINK,
            &Event {
                level: Level::Error,
                id: SO_TEST_FAIL,
                message: "riscv64 stack-overrun test: setup failed",
                fields: &[Field {
                    key: "stage",
                    value: tairix_log::FieldValue::Str(what),
                }],
            },
        );
        qemu_exit::exit_failure(code);
    }
}

// --- Host stub -----------------------------------------------------
#[cfg(not(itest_riscv64))]
fn main() {}
