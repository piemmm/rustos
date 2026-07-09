//! The x86_64 stack-guard test kernel: boot the production pipeline, then
//! on `BootCompleted` build a 4 GiB-identity address space, split the
//! 2 MiB huge page covering a guard static, unmap the single guard page,
//! and prove a supervisor-mode access to it faults (`plans/PI.md` G1/G2,
//! the x86_64 sibling of `stack_guard_qemu_{aarch64,riscv64}`).

use core::panic::PanicInfo;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use rustos_arch_api::mmu::AddressSpace as _;
use rustos_arch_api::tlb::TlbShootdown as _;
use rustos_arch_x86_64::paging::{self, KERNEL_VMA_BASE, PAGE_SIZE};
use rustos_arch_x86_64::{fault, qemu_exit};
use rustos_kernel::kalloc::{Heap, HEAP_BYTES};
use rustos_kernel::{
    boot, handle_panic_via_kernel_core, FreeListAllocator, SerialSink, SERIAL_SINK,
};
use rustos_log::{log, Event, EventId, Level, Sink};

/// `EventId` emitted when every boot init phase completed.
const BOOT_COMPLETED_EVENT_ID: EventId = EventId(4004);

/// Stable audit-event ids for the QEMU transcript (clear of the
/// `4000..5000` `kernel/core` boot range and the aarch64 (4300-range) /
/// riscv64 (4310-range) stack-guard verticals).
const SG_TEST_START: EventId = EventId(4320);
const SG_TEST_PASS: EventId = EventId(4321);
const SG_TEST_FAIL: EventId = EventId(4322);

/// Gigabytes of identity map the space installs. The 64 MiB bump heap
/// pushes this binary's `.bss` (and the guard static within it) well past
/// the 32 MiB window the X1/X2 verticals use, so the low identity must
/// cover all of RAM — 4 GiB also covers the architectural LAPIC MMIO page
/// at ~3.98 GiB, mirroring the spawn producer (`plans/PI.md` X3a).
const IDENTITY_GIB: usize = 4;

/// The sentinel written through the guard page after the split, to prove
/// the split preserved the mapping before the page is torn down.
const SENTINEL: u8 = 0xA5;

/// Static heap for the bump allocator (per the production bin); the boot
/// pipeline allocates from it.
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

/// A dedicated 4 KiB page the test unmaps after splitting the 2 MiB huge
/// page that covers it. It is its own aligned page, so tearing down its
/// low-identity alias disturbs no other kernel data: nothing else shares
/// the page, and the running code / stack live at *higher-half* virtual
/// addresses (a different region) which stay mapped.
#[repr(C, align(4096))]
struct GuardPage([u8; PAGE_SIZE]);
static mut GUARD_PAGE: GuardPage = GuardPage([0; PAGE_SIZE]);

/// `true` once the guard page has been unmapped — lets [`on_fault`] tell
/// the *expected* fault from a kernel bug that faults earlier.
static GUARD_UNMAPPED: AtomicBool = AtomicBool::new(false);

/// Set once the test has been driven so a duplicate `BootCompleted` cannot
/// re-enter the test logic.
static TEST_DRIVEN: AtomicU32 = AtomicU32::new(0);

/// The guard page's low-identity virtual address — its physical address,
/// which the identity map aliases 1:1. The static is a higher-half kernel
/// symbol (linked at `KERNEL_VMA_BASE + p`, loaded at physical `p`), so
/// subtracting [`KERNEL_VMA_BASE`] recovers the physical address `p`.
fn guard_phys() -> u64 {
    (core::ptr::addr_of!(GUARD_PAGE) as u64) - KERNEL_VMA_BASE
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

/// Forward to the shared bridge in `rustos_kernel`.
#[panic_handler]
fn stack_guard_qemu_x86_64_panic(info: &PanicInfo<'_>) -> ! {
    handle_panic_via_kernel_core(info)
}

/// Log a setup failure and report it to QEMU. Never returns.
fn fail(what: &'static str) -> ! {
    note(Level::Error, SG_TEST_FAIL, what);
    qemu_exit::exit_failure();
}

/// The page-fault observer the production `#PF` entry invokes. The read of
/// the unmapped guard page must land here as a **supervisor, not-present**
/// fault on exactly the guard page; anything else is a closed failure.
extern "C" fn on_fault(error_code: u64, faulting_addr: u64, _rip: u64) -> ! {
    let base = guard_phys();
    let page_end = base + PAGE_SIZE as u64;
    if !GUARD_UNMAPPED.load(Ordering::SeqCst) {
        note(
            Level::Error,
            SG_TEST_FAIL,
            "fault before unmap — kernel bug",
        );
        qemu_exit::exit_failure();
    }
    if !fault::is_not_present(error_code) {
        note(
            Level::Error,
            SG_TEST_FAIL,
            "unexpected fault cause, not a not-present page fault",
        );
        qemu_exit::exit_failure();
    }
    if fault::is_user(error_code) {
        note(
            Level::Error,
            SG_TEST_FAIL,
            "guard fault came from user mode, expected supervisor",
        );
        qemu_exit::exit_failure();
    }
    if faulting_addr < base || faulting_addr >= page_end {
        note(
            Level::Error,
            SG_TEST_FAIL,
            "page fault at the wrong address",
        );
        qemu_exit::exit_failure();
    }
    note(
        Level::Info,
        SG_TEST_PASS,
        "x86_64 stack-guard test: faulted on the unmapped guard page",
    );
    qemu_exit::exit_success();
}

/// Build the identity space, split the guard block, unmap the guard page,
/// and trigger the deliberate fault. Never returns.
fn run_guard_test() -> ! {
    note(
        Level::Info,
        SG_TEST_START,
        "x86_64 stack-guard test: splitting a huge page to unmap one guard page",
    );

    let guard = guard_phys();

    // Install the fault observer before any user mapping can fault. The
    // production boot already installed the dedicated, error-code-aware
    // `#PF` entry; this routes it here (fail-closed if already taken).
    if fault::set_fault_handler(on_fault).is_err() {
        fail("x86_64 stack-guard test: fault observer already installed");
    }

    // Build a 4 GiB-identity space (low identity + higher-half kernel
    // window) so the running RIP / stack / per-CPU TLS and the guard
    // static's low-identity alias all stay mapped across the CR3 switch.
    let Some(mut space) =
        paging::AddressSpace::new_identity_first_gib(&PAGE_TABLE_POOL, IDENTITY_GIB)
    else {
        fail("x86_64 stack-guard test: page-table pool exhausted");
    };

    // Activate it before splitting, so `split_block`'s low-identity table
    // dereferences resolve through this space's own 4 GiB identity map.
    // SAFETY: the new space maps the low 4 GiB and the higher-half kernel
    // window, so the executing RIP, the current stack, the per-CPU swapgs
    // TLS, and the page-table pool all stay mapped across the CR3 load.
    unsafe { space.activate() };

    // Re-express the 2 MiB huge page covering the guard page at 4 KiB
    // granularity. The split only *adds* table levels reproducing the
    // existing translation, so it is safe against the running regime.
    if space.split_block(guard).is_err() {
        fail("x86_64 stack-guard test: split_block failed");
    }

    // Prove the split preserved the guard page's mapping under the live
    // MMU: write a sentinel through the low-identity alias and read it back.
    // SAFETY: `guard` identity-maps `GUARD_PAGE`, a live page-aligned static
    // mapped RW; the access is well-defined while the page is mapped.
    unsafe {
        core::ptr::write_volatile(guard as *mut u8, SENTINEL);
    }
    let readback = unsafe { core::ptr::read_volatile(guard as *const u8) };
    if readback != SENTINEL {
        fail("x86_64 stack-guard test: split did not preserve the guard-page mapping");
    }

    // Tear the single guard page down through the Arch HAL and flush its
    // stale TLB entry — exactly the production guard-page mechanism.
    if space.unmap(guard).is_err() {
        fail("x86_64 stack-guard test: unmap guard page failed");
    }
    space.flush_page(guard);
    GUARD_UNMAPPED.store(true, Ordering::SeqCst);

    // Read the now-unmapped guard page. This must raise a not-present page
    // fault → `on_fault` (which exits PASS).
    // SAFETY: the access is *expected* to fault; if the MMU wrongly
    // permitted it the read is still of a valid pointer-sized region we
    // then report as a FAILURE below.
    let observed = unsafe { core::ptr::read_volatile(guard as *const u8) };

    // Reaching here means the unmapped page was read without a fault — the
    // guard FAILED. Reference `observed` so the read is not elided.
    if observed == SENTINEL {
        fail("x86_64 stack-guard test: read the unmapped guard page (sentinel, no fault)");
    }
    fail("x86_64 stack-guard test: read the unmapped guard page (no fault)");
}

/// Outer audit sink: replays every event to serial (so the QEMU transcript
/// captures the boot timeline) and, on the single [`BOOT_COMPLETED_EVENT_ID`],
/// drives [`run_guard_test`].
struct BootCompletedSink;

impl Sink for BootCompletedSink {
    fn write_event(&self, event: &Event<'_>) {
        SerialSink::new().write_event(event);

        if event.id == BOOT_COMPLETED_EVENT_ID
            && TEST_DRIVEN
                .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
        {
            run_guard_test();
        }
    }
}

static AUDIT_SINK: BootCompletedSink = BootCompletedSink;

/// The symbol the arch crate's boot trampoline calls.
#[no_mangle]
pub extern "C" fn kernel_main(multiboot_info: u64) -> ! {
    boot(
        multiboot_info,
        &SERIAL_SINK,
        &AUDIT_SINK,
        rustos_log::Level::Info,
    )
}
