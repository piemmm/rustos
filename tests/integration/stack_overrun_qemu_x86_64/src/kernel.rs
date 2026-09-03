//! The x86_64 stack-overrun test kernel: boot the production pipeline,
//! then on `BootCompleted` build a 4 GiB-identity address space, re-express
//! a 2 MiB guard arena at 4 KiB granularity, unmap one kthread stack's
//! guard page, admit a kthread on that stack via the production
//! `spawn_kthread_with_stack` runtime path, and prove its overrun into the
//! guard page faults synchronously (`plans/PI.md` G3c, the x86_64 sibling
//! of `stack_overrun_qemu_aarch64`).

use core::panic::PanicInfo;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

extern crate alloc;
use alloc::sync::Arc;

use tairix_arch_api::mmu::AddressSpace as _;
use tairix_arch_api::tlb::TlbShootdown as _;
use tairix_arch_x86_64::context_hal::ContextSwitchHal;
use tairix_arch_x86_64::kernel_arch::{X86_64Arch, X86_64ArchStorage};
use tairix_arch_x86_64::paging::{self, BLOCK_2MIB, KERNEL_VMA_BASE, PAGE_SIZE};
use tairix_arch_x86_64::{fault, qemu_exit, smp};
use tairix_kernel::kalloc::{Heap, HEAP_BYTES};
use tairix_kernel::{
    boot, handle_panic_via_kernel_core, FreeListAllocator, SerialSink, SERIAL_SINK,
};
use tairix_kernel_core::{spawn_kthread_with_stack, KernelStack, KTHREAD_STACK_BYTES};
use tairix_kernel_sched_eevdf::{Priority, Scheduler, SchedulerConfig};
use tairix_log::{log, Event, EventId, Field, Level, Sink};

/// `EventId` emitted when every boot init phase completed.
const BOOT_COMPLETED_EVENT_ID: EventId = EventId(4004);

/// Stable audit-event ids for the QEMU transcript (clear of the
/// `4000..5000` `kernel/core` boot range, the aarch64 overrun (4306-range),
/// and the x86_64 stack-guard (4320-range) verticals).
const SO_TEST_START: EventId = EventId(4323);
const SO_TEST_SPAWNED: EventId = EventId(4324);
const SO_TEST_PASS: EventId = EventId(4325);
const SO_TEST_FAIL: EventId = EventId(4326);

/// The single-core slice runs logical CPU 0 on the boot processor.
const BOOT_CPU: u32 = 0;

/// Width of the kthread-stack guard region: one 4 KiB page, matching
/// `tairix_kernel_core::BoxStack` and the production `ArenaStack` (the
/// guard sits immediately *below* the usable stack).
const STACK_GUARD_BYTES: u64 = PAGE_SIZE as u64;

/// Offset of the kthread stack region within the arena. Chosen well inside
/// the arena (page 4) so the guard page has mapped neighbours on both sides
/// and the region (`STACK_GUARD_BYTES + KTHREAD_STACK_BYTES`, ~68 KiB) fits
/// comfortably within the 2 MiB arena.
const STACK_REGION_OFFSET: u64 = 4 * PAGE_SIZE as u64;

/// Cooperative-loop watchdog: maximum `step` iterations before the test
/// declares the workload drained without faulting (a guard regression).
/// The expected drain faults on the very first dispatch.
const MAX_STEPS: u64 = 1_000_000;

/// Static heap for the bump allocator (per the production bin); the boot
/// pipeline and the test scheduler both allocate from it.
static mut HEAP: Heap = Heap::ZERO;

/// Global allocator backed by [`HEAP`].
///
/// SAFETY: the page-aligned `HEAP` static outlives the binary and the
/// allocator is its only consumer.
#[global_allocator]
static ALLOCATOR: FreeListAllocator =
    unsafe { FreeListAllocator::new(core::ptr::addr_of!(HEAP) as *mut u8, HEAP_BYTES) };

/// Page-table pool backing the new address space (lives in `.bss`).
static PAGE_TABLE_POOL: paging::PageTablePool = paging::PageTablePool::new();

/// Backing store for the kthread-stack guard arena: two 2 MiB blocks of
/// page-aligned `.bss`, big enough to carve one 2 MiB-aligned, 2 MiB arena
/// out of wherever the linker places the static. Page (not 2 MiB)
/// alignment keeps the linker from over-aligning the whole `.bss` section;
/// the 2 MiB-aligned arena is rounded up inside this window
/// ([`arena_phys`]). The arena then occupies a whole identity huge page of
/// its own, so re-expressing it at 4 KiB granularity (and unmapping a guard
/// page inside it) never disturbs the block holding the running code, boot
/// stack, or heap.
#[repr(C, align(4096))]
struct ArenaBacking([u8; 2 * BLOCK_2MIB as usize]);
static mut ARENA_BACKING: ArenaBacking = ArenaBacking([0; 2 * BLOCK_2MIB as usize]);

/// `true` once the guard page has been unmapped — lets [`on_fault`] tell
/// the *expected* fault from a kernel bug that faults earlier.
static GUARD_UNMAPPED: AtomicBool = AtomicBool::new(false);

/// Set once the test has been driven so a duplicate `BootCompleted` cannot
/// re-enter the test logic.
static TEST_DRIVEN: AtomicU32 = AtomicU32::new(0);

/// Physical base of the 2 MiB-aligned guard arena — a low-identity
/// address, which the identity map aliases 1:1. `ARENA_BACKING` is a
/// higher-half kernel symbol (linked at `KERNEL_VMA_BASE + p`, loaded at
/// physical `p`), so subtracting [`KERNEL_VMA_BASE`] recovers the physical
/// address `p`; rounding that up to the next 2 MiB boundary yields a
/// 2 MiB-aligned arena that fits within the backing store (which is twice
/// the arena size). The kthread runs on this low-identity alias so the
/// guard page we unmap is exactly the page it overruns into.
fn arena_phys() -> u64 {
    let backing = (core::ptr::addr_of!(ARENA_BACKING) as u64) - KERNEL_VMA_BASE;
    (backing + (BLOCK_2MIB - 1)) & !(BLOCK_2MIB - 1)
}

/// Base (4 KiB-aligned) of the kthread stack region inside the arena: the
/// low edge of its one-page guard, the page the test unmaps.
fn guard_page() -> u64 {
    arena_phys() + STACK_REGION_OFFSET
}

/// The address the kthread body writes to overrun its stack: the highest
/// byte of the guard region — the first byte a contiguous downward stack
/// overrun crosses. It lies inside the (unmapped) guard page, so the access
/// faults synchronously.
fn overrun_target() -> u64 {
    guard_page() + STACK_GUARD_BYTES - 1
}

/// A kthread kernel stack carved from the arena, laid out
/// `[guard page | usable stack]` exactly like the production `ArenaStack`.
/// Its guard page is unmapped before the kthread runs, so
/// [`KernelStack::check_guard`] keeps the default `Ok(())` — the hardware
/// fault is the defence, not a canary scan.
#[derive(Copy, Clone)]
struct ArenaTestStack {
    guard: u64,
}

// SAFETY: `top` returns the region base plus the guard page plus the usable
// `KTHREAD_STACK_BYTES`, rounded down to the 16-byte ABI alignment. The
// usable region `[guard + STACK_GUARD_BYTES, top)` is a sub-range of the
// identity-mapped, single-owner `ARENA` static that outlives the binary;
// only the guard page below it is unmapped. The arena hands this region to
// exactly one kthread, so it is exclusive.
unsafe impl KernelStack for ArenaTestStack {
    fn top(&self) -> u64 {
        let top = self.guard + STACK_GUARD_BYTES + KTHREAD_STACK_BYTES as u64;
        top & !0xF
    }

    fn usable_bytes(&self) -> u64 {
        KTHREAD_STACK_BYTES as u64
    }
}

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

/// Forward to the shared bridge in `tairix_kernel`.
#[panic_handler]
fn stack_overrun_qemu_x86_64_panic(info: &PanicInfo<'_>) -> ! {
    handle_panic_via_kernel_core(info)
}

/// Log a setup failure and report it to QEMU. Never returns.
fn fail(what: &'static str) -> ! {
    log(
        &SERIAL_SINK,
        &Event {
            level: Level::Error,
            id: SO_TEST_FAIL,
            message: "x86_64 stack-overrun test: setup failed",
            fields: &[Field {
                key: "stage",
                value: tairix_log::FieldValue::Str(what),
            }],
        },
    );
    qemu_exit::exit_failure();
}

/// The page-fault observer the production `#PF` entry invokes. The
/// overrunning kthread's access to the unmapped guard page must land here
/// as a **supervisor, not-present** fault on exactly the guard page;
/// anything else is a closed failure. Never returns.
extern "C" fn on_fault(error_code: u64, faulting_addr: u64, _rip: u64) -> ! {
    let base = guard_page();
    let page_end = base + STACK_GUARD_BYTES;
    if !GUARD_UNMAPPED.load(Ordering::SeqCst) {
        note(
            Level::Error,
            SO_TEST_FAIL,
            "fault before unmap — kernel bug",
        );
        qemu_exit::exit_failure();
    }
    if !fault::is_not_present(error_code) {
        note(
            Level::Error,
            SO_TEST_FAIL,
            "unexpected fault cause, not a not-present page fault",
        );
        qemu_exit::exit_failure();
    }
    if fault::is_user(error_code) {
        note(
            Level::Error,
            SO_TEST_FAIL,
            "guard fault came from user mode, expected supervisor",
        );
        qemu_exit::exit_failure();
    }
    if faulting_addr < base || faulting_addr >= page_end {
        note(
            Level::Error,
            SO_TEST_FAIL,
            "page fault at the wrong address",
        );
        qemu_exit::exit_failure();
    }
    note(
        Level::Info,
        SO_TEST_PASS,
        "x86_64 stack-overrun test: kthread overran into the unmapped guard page",
    );
    qemu_exit::exit_success();
}

/// Build the identity space + guard arena, unmap one kthread stack's guard
/// page, admit a kthread on that stack, and drive it until it overruns and
/// faults. Never returns.
fn run_overrun_test() -> ! {
    note(
        Level::Info,
        SO_TEST_START,
        "x86_64 stack-overrun test: arming an unmapped kthread-stack guard page",
    );

    // Keep dispatch deterministic: mask interrupts so the LAPIC timer does
    // not drive the *production* scheduler while the cooperative `step` loop
    // below runs our own (the kthread faults on its first dispatch anyway).
    // SAFETY: `cli` is well-defined in ring 0; the cooperative loop needs no
    // interrupts.
    unsafe {
        core::arch::asm!("cli", options(nomem, nostack, preserves_flags));
    }

    let arena = arena_phys();
    let guard = guard_page();

    // Build a 4 GiB-identity space (low identity + higher-half kernel
    // window) so the running RIP / stack / per-CPU TLS, the heap, and the
    // arena's low-identity alias all stay mapped across the CR3 switch.
    let Some(mut space) = paging::AddressSpace::new_identity_window(&PAGE_TABLE_POOL) else {
        fail("page-table pool exhausted");
    };

    // Activate it before splitting, so `prepare_guard_arena`'s low-identity
    // table dereferences resolve through this space's own 4 GiB identity map
    // (the x86_64 four-level walk recovers tables by their low physical
    // address — see `stack_guard_qemu_x86_64`).
    // SAFETY: the new space maps the low 4 GiB and the higher-half kernel
    // window, so the executing RIP, the current stack, the per-CPU swapgs
    // TLS, the heap, and the page-table pool all stay mapped across the CR3
    // load.
    unsafe { space.activate() };

    // Re-express the 2 MiB identity huge page covering the arena at 4 KiB
    // granularity so a single guard page in it can be torn down. The split
    // only *adds* table levels reproducing the existing translation, so it
    // is safe against the running regime.
    if space.prepare_guard_arena(arena, BLOCK_2MIB).is_err() {
        fail("prepare_guard_arena failed");
    }

    // Tear the kthread stack's guard page down through the Arch HAL and
    // flush its stale TLB entry — exactly the production guard-page
    // mechanism (G3b-2). The usable stack above it stays mapped.
    if space.unmap(guard).is_err() {
        fail("unmap guard page failed");
    }
    space.flush_page(guard);
    GUARD_UNMAPPED.store(true, Ordering::SeqCst);

    // Build the live scheduler over a fresh production arch handle (the
    // cooperative `step` loop drives dispatch; interrupts are masked, so the
    // spawn-time self-IPI is latched and never delivered).
    let bsp_id = smp::bsp_lapic_id();
    // Single-CPU vertical (BSP, dense id 0): per-CPU bookkeeping is sized
    // to one slot (no baked-in `MAX_CPUS`).
    let cpu_to_lapic: [Option<u8>; 1] = [Some(bsp_id)];
    // The arch handle borrows its per-CPU bookkeeping from a caller-sized
    // `&'static` backing; `run_overrun_test` runs once,
    // so a function-local `static` is sound and needs no allocator.
    static ARCH_STORAGE: X86_64ArchStorage<1> = X86_64ArchStorage::new();
    let Ok(handle) = X86_64Arch::new(&ARCH_STORAGE, 0, bsp_id, &cpu_to_lapic) else {
        fail("X86_64Arch::new failed");
    };
    let arch = Arc::new(handle);
    let Ok(sched) = Scheduler::new(SchedulerConfig::defaults_for(1), arch) else {
        fail("scheduler new failed");
    };

    // Admit a kthread on the arena-backed, guard-unmapped stack. The body
    // overruns into the guard page on its first (and only) dispatch.
    let target = overrun_target();
    let spawned = spawn_kthread_with_stack(
        &sched,
        ContextSwitchHal::new(),
        ArenaTestStack { guard },
        BOOT_CPU,
        Priority::Normal,
        move |_yielder| {
            // Overrun the usable stack into the (unmapped) guard page: touch
            // the highest guard byte, the first byte a contiguous downward
            // overrun crosses. This must fault synchronously.
            // SAFETY: the access is *expected* to fault — the guard page is
            // unmapped. The write is volatile so it is not elided; if the
            // MMU wrongly permitted it the body simply returns and the drain
            // loop below reports the guard FAILURE.
            unsafe {
                core::ptr::write_volatile(target as *mut u8, 0xA5);
            }
        },
    );
    if spawned.is_err() {
        fail("spawn kthread failed");
    }
    note(
        Level::Info,
        SO_TEST_SPAWNED,
        "x86_64 stack-overrun test: kthread spawned on the guarded arena stack",
    );

    // Drive the cooperative dispatch loop. The first `step` enters the
    // kthread, whose overrun faults into `on_fault` (which exits PASS). If
    // the guard page were wrongly left mapped the body would return (Exit)
    // and the loop would drain — a guard regression we report below rather
    // than letting it pass silently.
    let mut steps = 0u64;
    while sched.live_task_count() != 0 && steps < MAX_STEPS {
        let _ = sched.step(BOOT_CPU);
        steps += 1;
    }

    // Reaching here means the kthread overran without faulting — the guard
    // was not enforced. Fail loudly.
    log(
        &SERIAL_SINK,
        &Event {
            level: Level::Error,
            id: SO_TEST_FAIL,
            message: "x86_64 stack-overrun test: kthread overran the guard page without faulting",
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
    qemu_exit::exit_failure();
}

/// Outer audit sink: replays every event to serial (so the QEMU transcript
/// captures the boot timeline) and, on the single [`BOOT_COMPLETED_EVENT_ID`],
/// drives [`run_overrun_test`].
struct BootCompletedSink;

impl Sink for BootCompletedSink {
    fn write_event(&self, event: &Event<'_>) {
        SerialSink::new().write_event(event);

        if event.id == BOOT_COMPLETED_EVENT_ID
            && TEST_DRIVEN
                .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
        {
            run_overrun_test();
        }
    }
}

static AUDIT_SINK: BootCompletedSink = BootCompletedSink;

/// The symbol the arch crate's boot trampoline calls.
#[no_mangle]
pub extern "C" fn kernel_main(multiboot_info: u64) -> ! {
    // Claim the set-once fatal-fault slot before `boot` publishes the
    // production reporter into it: this vertical *is* the observer of its
    // own deliberate stack-overrun fault, so it owns the machine's fatal
    // policy for this image and must be first. `on_fault` fail-closes on
    // any fault before the guard is unmapped, so owning the slot from here
    // never hides an unexpected one.
    if fault::set_fault_handler(on_fault).is_err() {
        fail("fault observer already installed");
    }
    boot(
        multiboot_info,
        &ALLOCATOR,
        &SERIAL_SINK,
        &AUDIT_SINK,
        tairix_log::Level::Info,
    )
}
