//! The x86_64 stack-overrun test kernel: boot the production pipeline, then
//! on `BootCompleted` draw a kthread kernel stack from the **window-backed**
//! stack tier the boot path installed, admit a kthread on it via the
//! production `spawn_kthread_with_stack` runtime path, and prove its overrun
//! into the stack's unmapped guard slot faults synchronously
//! (`plans/OPEN-DEFECTS.md` D82, the x86_64 sibling of
//! `stack_overrun_qemu_aarch64`).
//!
//! Unlike its siblings this bin boots the whole production pipeline, so the
//! remap window and the stack tier are installed by the kernel itself — a
//! stack that came back outside the window would mean the production install
//! silently degraded to the software-canary fallback, which the window check
//! below catches before the kthread runs.

use core::panic::PanicInfo;
use core::sync::atomic::{AtomicU32, Ordering};

extern crate alloc;
use alloc::sync::Arc;

use tairix_arch_x86_64::context_hal::ContextSwitchHal;
use tairix_arch_x86_64::kernel_arch::{X86_64Arch, X86_64ArchStorage};
use tairix_arch_x86_64::paging::PAGE_SIZE;
use tairix_arch_x86_64::{fault, qemu_exit, smp};
use tairix_kernel::kalloc::{Heap, HEAP_BYTES};
use tairix_kernel::{
    boot, handle_panic_via_kernel_core, FreeListAllocator, SerialSink, SERIAL_SINK,
};
use tairix_kernel_core::kstack::alloc_kernel_stack;
use tairix_kernel_core::{spawn_kthread_with_stack, KernelStack};
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

/// Width of a kthread stack's guard region: one 4 KiB page, the slot the
/// tier reserves and never maps immediately *below* the usable stack.
const STACK_GUARD_BYTES: u64 = PAGE_SIZE as u64;

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

/// The guard slot of the stack under test, published once it is drawn so
/// the fault observer can name the address it expects. Zero until then,
/// which lets [`on_fault`] tell the *expected* overrun from a kernel bug
/// that faults earlier.
static GUARD_SLOT: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Set once the test has been driven so a duplicate `BootCompleted` cannot
/// re-enter the test logic.
static TEST_DRIVEN: AtomicU32 = AtomicU32::new(0);

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
/// overrunning kthread's access to the unmapped guard slot must land here
/// as a **supervisor, not-present** fault on exactly the guard slot;
/// anything else is a closed failure. Never returns.
extern "C" fn on_fault(error_code: u64, faulting_addr: u64, _rip: u64) -> ! {
    let base = GUARD_SLOT.load(Ordering::Acquire);
    let page_end = base + STACK_GUARD_BYTES;
    if base == 0 {
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
        "x86_64 stack-overrun test: kthread overran into the unmapped guard slot",
    );
    qemu_exit::exit_success();
}

/// Draw a stack from the boot-installed window tier, admit a kthread on it,
/// and drive it until it overruns and faults. Never returns.
fn run_overrun_test() -> ! {
    note(
        Level::Info,
        SO_TEST_START,
        "x86_64 stack-overrun test: drawing a window-backed kthread stack",
    );

    // Keep dispatch deterministic: mask interrupts so the LAPIC timer does
    // not drive the *production* scheduler while the cooperative `step` loop
    // below runs our own (the kthread faults on its first dispatch anyway).
    // SAFETY: `cli` is well-defined in ring 0; the cooperative loop needs no
    // interrupts.
    unsafe {
        core::arch::asm!("cli", options(nomem, nostack, preserves_flags));
    }

    // Draw a stack from the tier the production boot installed. The active
    // CR3 already carries the window's shared sub-hierarchy, so the run
    // resolves without this test touching a page table.
    let stack = alloc_kernel_stack();
    // The tier lays a stack out as `[guard slot | usable run]`, so the guard
    // is the page below the usable base.
    let usable_base = stack.top() - stack.usable_bytes();
    let guard = usable_base - STACK_GUARD_BYTES;
    // A `BoxStack` fallback would have handed back heap memory, which means
    // the production install silently degraded. The window sits above the
    // higher-half kernel image and far above the bump heap, so a heap
    // address cannot reach it.
    if guard < tairix_arch_x86_64::paging::kernel_window_base() {
        fail("stack is not window-backed");
    }
    GUARD_SLOT.store(guard, Ordering::Release);
    // The usable run must be mapped and writable, or the fault below would
    // prove nothing about the guard.
    // SAFETY: the run was installed by the tier and is exclusive to this
    // stack, which nothing is running on yet.
    unsafe {
        core::ptr::write_volatile(usable_base as *mut u8, 0x5A);
        if core::ptr::read_volatile(usable_base as *const u8) != 0x5A {
            fail("usable stack not writable");
        }
    }

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

    // Admit a kthread on the window-backed stack. The body overruns into
    // the unmapped guard slot on its first (and only) dispatch: the highest
    // byte of that slot is the first byte a contiguous downward overrun
    // crosses.
    let target = guard + STACK_GUARD_BYTES - 1;
    let spawned = spawn_kthread_with_stack(
        &sched,
        ContextSwitchHal::new(),
        stack,
        BOOT_CPU,
        Priority::Normal,
        move |_yielder| {
            // Overrun the usable stack into the (unmapped) guard slot: touch
            // the highest guard byte, the first byte a contiguous downward
            // overrun crosses. This must fault synchronously.
            // SAFETY: the access is *expected* to fault — the guard slot is
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
        "x86_64 stack-overrun test: kthread spawned on the window-backed stack",
    );

    // Drive the cooperative dispatch loop. The first `step` enters the
    // kthread, whose overrun faults into `on_fault` (which exits PASS). If
    // the guard slot were wrongly left mapped the body would return (Exit)
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
            message: "x86_64 stack-overrun test: kthread overran the guard slot without faulting",
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
