//! `plans/OPEN-DEFECTS.md` D82 QEMU integration test: a live, scheduled
//! kthread running on a **window-backed** kernel stack takes a synchronous
//! data abort the instant it overruns into the stack's guard slot.
//!
//! ## Why this exists
//!
//! A kthread kernel stack is a run of pages in the shared kernel remap
//! window whose lowest slot is reserved but never mapped. Because that
//! window's sub-hierarchy is installed by every translation root, the guard
//! is absent everywhere at once — nothing refines a live block, and no root
//! carries a per-task unmap. What this vertical proves on the board is the
//! payoff: an *overrunning kthread* faults **synchronously in hardware**,
//! rather than being caught only at the next reschedule by the
//! software-canary `tairix_kernel_core::kthread::BoxStack` fallback.
//!
//! ## What this test asserts
//!
//! 1. Reserve the kernel remap window, build a stage-1 identity space (which
//!    installs the window's shared slots), and activate it.
//! 2. Install the window-backed kthread stack tier over that window and draw
//!    one stack from it — the production allocation path.
//! 3. The drawn stack's usable run is mapped and writable, and its guard
//!    slot lies one page below the usable base.
//! 4. Build the live `tairix_kernel_sched_eevdf::Scheduler` over
//!    `Aarch64Arch` and admit a kthread on that stack via
//!    `spawn_kthread_with_stack` — the production runtime path, not a bare
//!    function call.
//! 5. The kthread body overruns: it writes the highest byte of its guard
//!    slot, the first byte a contiguous downward overrun crosses. That slot
//!    is unmapped, so the access raises a synchronous data abort while the
//!    kthread is *running*, taken on the still-healthy usable stack above the
//!    guard so the EL1 trampoline does not nest-fault.
//! 6. The handler confirms the trap is a data abort on exactly the guard
//!    slot and reports PASS. A regression that left the slot mapped lets the
//!    body return cleanly; the cooperative `step` loop then drains the task
//!    and the test reports FAILURE explicitly rather than passing.
//!
//! ## How it differs from a production kernel
//!
//! It links the `tairix-kernel-core` kthread runtime, the
//! `tairix-arch-aarch64` port, and the default `tairix-kernel-sched-eevdf`
//! policy directly and supplies its own `kernel_main`, installing the window
//! and the stack tier itself rather than booting the whole pipeline. The
//! QEMU-exit shortcut lives in this dedicated bin, never behind a Cargo
//! feature on a library crate (fail closed).

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

    use alloc::sync::Arc;

    use alloc::boxed::Box;

    use tairix_arch_aarch64::context_hal::ContextSwitchHal;
    use tairix_arch_aarch64::irqmask::PortIrqControl;
    use tairix_arch_aarch64::kernel_arch::timer_frequency_hz;
    use tairix_arch_aarch64::paging::{
        configure_ram_gigapages, identity_ram_mask, reserve_kernel_window, AddressSpace,
        PageTablePool, PAGE_SIZE,
    };
    use tairix_arch_aarch64::{
        exceptions, fault, gic, handle_panic_via_serial, qemu_exit, Aarch64Arch, SERIAL_SINK,
    };
    use tairix_arch_api::mmu::AddressSpace as _;
    use tairix_arch_api::CpuId;
    use tairix_fdt::Fdt;
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

    // The canonical QEMU `virt` device tree, dumped and embedded at build
    // time (`build.rs`): the GICv2 base and the timer frequency are read
    // from it (P4).
    include!(concat!(env!("OUT_DIR"), "/dtb_fixture.rs"));

    /// The single-core slice runs logical CPU 0 on the boot core.
    const BOOT_CPU: CpuId = 0;

    /// Number of GiB the space identity-maps (device MMIO + RAM). The
    /// kernel image, boot stack, heap, and the frame pool all live in the
    /// Normal RAM gigapage (GiB 1).
    const IDENTITY_GIB: usize = 2;

    /// The `virt` board's RAM window, which the RAM gigapage mask is narrowed
    /// to before the remap window is reserved.
    const RAM_BASE: u64 = 0x4000_0000;
    const RAM_BYTES: u64 = 1 << 30;

    /// Width of a kthread stack's guard region: one 4 KiB page, the slot the
    /// tier reserves and never maps immediately *below* the usable stack.
    const STACK_GUARD_BYTES: u64 = PAGE_SIZE as u64;

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
    /// Failure finisher codes, distinct per failure site.
    const FAIL_NO_FAULT: NonZeroU16 = fail_point!(2);
    const FAIL_UNEXPECTED_FAULT: NonZeroU16 = fail_point!(3);
    const FAIL_SETUP: NonZeroU16 = fail_point!(4);

    /// A deliberately-wrong GICv2 base installed before discovery runs, so
    /// reaching the `virt` distributor base can only mean discovery
    /// overwrote it from the device tree (`plans/PI.md` P4).
    const POISON_GIC_BASE: usize = 0xdead_0000;

    /// Page-table pool backing the identity address space (lives in
    /// `.bss`).
    static POOL: PageTablePool = PageTablePool::new();

    /// Identity-mapped RAM the frame allocator hands out: the window's own
    /// leaf tables and the pages backing the drawn kthread stack. Sized well
    /// past both so exhaustion cannot be mistaken for a guard regression.
    #[repr(C, align(0x1000))]
    struct FramePool([u8; 8 * 1024 * 1024]);
    static mut FRAME_POOL: FramePool = FramePool([0; 8 * 1024 * 1024]);

    /// The guard slot of the stack under test, published once it is drawn so
    /// the fault handler can name the address it expects.
    static GUARD_SLOT: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

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

    /// The fault handler: confirm the trap is a data/instruction abort on
    /// the unmapped guard slot, then report PASS. Anything else is a
    /// FAILURE. Never returns.
    extern "C" fn on_fault(esr: u64, far: u64, _elr: u64) -> ! {
        let base = GUARD_SLOT.load(core::sync::atomic::Ordering::Acquire);
        if fault::is_abort(esr) && far >= base && far < base + STACK_GUARD_BYTES {
            log(
                &SERIAL_SINK,
                &Event {
                    level: Level::Info,
                    id: SO_TEST_PASS,
                    message:
                        "aarch64 stack-overrun test: kthread overran into the unmapped guard slot",
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
        qemu_exit::exit_failure(FAIL_UNEXPECTED_FAULT);
    }

    /// Forward to the shared aarch64 panic bridge (parks the CPU; the run
    /// then times out and the harness reports the failure).
    #[panic_handler]
    fn tairix_stack_overrun_aarch64_panic(info: &PanicInfo<'_>) -> ! {
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
    /// calls (via `tairix_arch_aarch64_main`).
    #[no_mangle]
    pub extern "C" fn kernel_main(_dtb: u64) -> ! {
        note(
            SO_TEST_START,
            "aarch64 stack-overrun test: drawing a window-backed kthread stack",
        );

        // P4: read the board from the embedded `virt` device tree.
        let Ok(fdt) = Fdt::new(DTB_BLOB) else {
            fail("parse virt dtb");
        };
        let counter_hz = timer_frequency_hz(&fdt);
        if counter_hz == 0 {
            fail("zero timer frequency");
        }

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
            fail("frame allocator");
        };
        let frames: &'static FrameAllocator = Box::leak(Box::new(frames));
        let physmap: &'static DirectPhysMap = Box::leak(Box::new(DirectPhysMap::identity(
            (IDENTITY_GIB as u64) << 30,
        )));
        let tables: &'static FrameTableSource =
            Box::leak(Box::new(FrameTableSource::new(frames, physmap)));

        // Narrow the RAM gigapage mask to the board's own window first: the
        // pre-discovery default claims every gigapage, and the reservation
        // refuses a slot RAM claims (fail closed), so an unconfigured mask
        // leaves no window at all. This is what the boot path does with the
        // facts in hand.
        configure_ram_gigapages(identity_ram_mask(&[(RAM_BASE, RAM_BYTES)]));

        // Reserve the window *before* the identity space is built, so the
        // space installs its shared sub-hierarchy slots and every stack the
        // tier hands out resolves under the root the kthread runs on.
        let Some(window) = reserve_kernel_window(tables) else {
            fail("reserve kernel window");
        };
        let space = AddressSpace::new_identity_gigapages(&POOL, IDENTITY_GIB)
            .unwrap_or_else(|| fail("identity map"));

        // Install the vectors + fault handler before enabling the MMU so the
        // kthread's deliberate overrun is routed to `on_fault`.
        fault::set_fault_handler(on_fault).unwrap_or_else(|_| fail("set_fault_handler"));
        // SAFETY: called once on the boot CPU before any fault can fire.
        unsafe {
            exceptions::init_vectors();
        }

        // Switch to the space (enables the MMU). It identity-maps `pc`,
        // `sp`, the heap, and MMIO, and carries the window's shared slots.
        // SAFETY: every region the running code touches stays mapped across
        // the switch, which is `AddressSpace::switch`'s contract.
        unsafe {
            space.activate();
        }

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

        // Build the live scheduler over the arch port, then install the
        // window-backed stack tier over that port's cross-CPU invalidation
        // and draw one stack from it — the production allocation path.
        // Per-CPU bookkeeping backing for this single-CPU vertical.
        static ARCH_STORAGE: tairix_arch_aarch64::Aarch64ArchStorage<1> =
            tairix_arch_aarch64::Aarch64ArchStorage::new();
        let arch = Arc::new(Aarch64Arch::new(&ARCH_STORAGE, BOOT_CPU, counter_hz));
        let arch_static: &'static Aarch64Arch = Box::leak(Box::new(Arc::clone(&arch)));
        let Some(kwspace) = AddressSpace::new_kernel_window(tables) else {
            fail("kernel window space");
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
            fail("stack is not window-backed");
        }
        GUARD_SLOT.store(guard, core::sync::atomic::Ordering::Release);
        // The usable run must be mapped and writable, or the abort below
        // would prove nothing about the guard.
        // SAFETY: the run was installed by the tier and is exclusive to this
        // stack, which nothing is running on yet.
        unsafe {
            core::ptr::write_volatile(usable_base as *mut u8, 0x5A);
            if core::ptr::read_volatile(usable_base as *const u8) != 0x5A {
                fail("usable stack not writable");
            }
        }

        let Ok(sched) = Scheduler::new(SchedulerConfig::defaults_for(1), arch) else {
            fail("scheduler new");
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
            fail("spawn kthread");
        }
        note(
            SO_TEST_SPAWNED,
            "aarch64 stack-overrun test: kthread spawned on the window-backed stack",
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
                    "aarch64 stack-overrun test: kthread overran the guard slot without faulting",
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
    fn fail(what: &'static str) -> ! {
        log(
            &SERIAL_SINK,
            &Event {
                level: Level::Error,
                id: SO_TEST_FAIL,
                message: "aarch64 stack-overrun test: setup failed",
                fields: &[Field {
                    key: "stage",
                    value: tairix_log::FieldValue::Str(what),
                }],
            },
        );
        qemu_exit::exit_failure(FAIL_SETUP);
    }
}

// --- Host stub -----------------------------------------------------
#[cfg(not(itest_aarch64))]
fn main() {}
