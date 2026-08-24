//! The freestanding aarch64 test kernel: bring up the EL1 synchronous
//! vectors, map one kernel page, and drive the software-managed Access
//! Flag clock through the Arch HAL — proving the Access-Flag fault path
//! resolves a cleared-AF access on cortex-a72 (no HAFDBS).

use core::num::NonZeroU16;
use core::panic::PanicInfo;
use core::sync::atomic::{AtomicBool, Ordering};

use tairix_arch_aarch64::paging::{AddressSpace as ArchAddressSpace, PageTablePool};
use tairix_arch_aarch64::{enable_fp_el1, exceptions, fault, qemu_exit, SERIAL_SINK};
use tairix_arch_api::mmu::{AccessTracking, AddressSpace as _, MapError, PageFlags};
use tairix_itest_finisher::fail_point;
use tairix_kalloc::FreeListAllocator;
use tairix_log::{log, Event, EventId, Level};

/// Gigabytes of identity map the address space provides: `[0, 2 GiB)`
/// covers the `virt` board's device MMIO (GiB 0, PL011 + GIC) and the RAM
/// base at GiB 1 where this kernel and its page-table pool run. The probe
/// page ([`TEST_VADDR`], 64 GiB) sits far above, on freshly walked tables.
const IDENTITY_GIB: usize = 2;

/// Virtual address the test maps its single 4 KiB probe page at. Chosen at
/// 64 GiB — far above the identity window and within the 39-bit (512 GiB)
/// TTBR0 region — so the mapping is a fresh 4 KiB leaf (its own L2/L3),
/// never a huge block.
const TEST_VADDR: u64 = 64 << 30;

/// A misaligned address, to confirm the fail-closed reject.
const MISALIGNED_VADDR: u64 = TEST_VADDR + 0x123;

/// An address mapped nowhere in the test space (128 GiB), to confirm the
/// "not mapped" fail-closed reject.
const UNMAPPED_VADDR: u64 = 128 << 30;

/// Magic byte written into the probe frame so the accesses read real data.
const PROBE_BYTE: u8 = 0x5A;

/// Stable audit-event ids for the QEMU transcript.
const TEST_START: EventId = EventId(4310);
const TEST_PASS: EventId = EventId(4311);
const TEST_FAIL: EventId = EventId(4312);

/// Failure finisher codes, distinct per failure site.
const FAIL_POOL: NonZeroU16 = fail_point!(1);
const FAIL_MAP: NonZeroU16 = fail_point!(2);
const FAIL_NOT_SUPPORTED: NonZeroU16 = fail_point!(3);
const FAIL_MISALIGNED_EDGE: NonZeroU16 = fail_point!(4);
const FAIL_UNMAPPED_EDGE: NonZeroU16 = fail_point!(5);
const FAIL_SEED_READBACK: NonZeroU16 = fail_point!(6);
const FAIL_PROBE_FRESH: NonZeroU16 = fail_point!(7);
const FAIL_PROBE_COLD: NonZeroU16 = fail_point!(8);
const FAIL_PROBE_AFTER_FAULT: NonZeroU16 = fail_point!(9);
const FAIL_PROBE_COLD_AGAIN: NonZeroU16 = fail_point!(10);
const FAIL_PROBE_REACCESS: NonZeroU16 = fail_point!(11);
const FAIL_UNEXPECTED_FAULT: NonZeroU16 = fail_point!(12);

/// `true` once setup is complete. Any unexpected fault before this is a
/// setup bug; after it, a fault the Access-Flag path did not resolve.
static SETUP_DONE: AtomicBool = AtomicBool::new(false);

/// Size of the test's bump heap. The aarch64 port names `alloc`, so a
/// `#[global_allocator]` must be present even though this test allocates
/// nothing itself. Lives in `.bss` (zeroed by the trampoline).
const HEAP_SIZE: usize = 1024 * 1024;

/// Page-aligned backing store for the bump heap.
#[repr(C, align(4096))]
struct HeapStore([u8; HEAP_SIZE]);

static mut HEAP: HeapStore = HeapStore([0; HEAP_SIZE]);

/// Global allocator backed by [`HEAP`].
///
/// SAFETY: the page-aligned `HEAP` static outlives the binary and the
/// allocator is its only consumer.
#[global_allocator]
static ALLOCATOR: FreeListAllocator =
    unsafe { FreeListAllocator::new(core::ptr::addr_of!(HEAP) as *mut u8, HEAP_SIZE) };

/// Page-table pool backing the address space.
static PAGE_TABLES: PageTablePool = PageTablePool::new();

/// 4 KiB frame the test maps at [`TEST_VADDR`]. `#[repr(align(4096))]` so
/// its physical address is a valid page frame; it lives in identity-mapped
/// RAM, so its kernel virtual address equals its physical address.
#[repr(C, align(4096))]
struct ProbeFrame([u8; 4096]);

static mut PROBE_FRAME: ProbeFrame = ProbeFrame([0; 4096]);

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

/// Report a failed expectation and exit QEMU with a failure code.
fn fail(id_msg: &'static str, code: NonZeroU16) -> ! {
    note(TEST_FAIL, id_msg);
    qemu_exit::exit_failure(code);
}

/// Read one byte from `vaddr` so the CPU translates the leaf and (when AF
/// is clear) raises an Access-Flag fault. Volatile so the compiler cannot
/// elide it.
fn touch(vaddr: u64) -> u8 {
    // SAFETY: `vaddr` is mapped kernel-RW in the active space; the read
    // observes the probe frame and exercises the leaf's Access Flag.
    let byte = unsafe { core::ptr::read_volatile(vaddr as *const u8) };
    core::hint::black_box(byte)
}

/// Forward to the shared aarch64 panic bridge (parks the CPU; the run then
/// times out and the harness reports the failure).
#[panic_handler]
fn accessed_bit_qemu_aarch64_panic(info: &PanicInfo<'_>) -> ! {
    tairix_arch_aarch64::handle_panic_via_serial(info)
}

/// The unexpected-fault handler: this test provokes no *unresolved* fault
/// (the Access-Flag faults are resolved inside the exception dispatch and
/// never reach here), so any fault reported to this handler is a bug.
extern "C" fn on_fault(_esr: u64, _far: u64, _elr: u64) -> ! {
    let msg = if SETUP_DONE.load(Ordering::SeqCst) {
        "accessed_bit test: unexpected synchronous fault after setup (access-flag path did not resolve)"
    } else {
        "accessed_bit test: unexpected synchronous fault during setup"
    };
    fail(msg, FAIL_UNEXPECTED_FAULT);
}

/// Boot entry point — the symbol the arch crate's `boot.s` trampoline
/// calls (via `tairix_arch_aarch64_main`).
#[no_mangle]
pub extern "C" fn kernel_main(_dtb: u64) -> ! {
    note(TEST_START, "aarch64 accessed-bit test: starting");

    // Enable FP/SIMD at EL1 before any code that may compile to NEON
    // (logging's formatting helpers can).
    // SAFETY: boot CPU, called once, before any FP/SIMD executes.
    unsafe { enable_fp_el1() };

    // Build the identity address space and activate it so the probe
    // mapping lands in the live translation regime.
    let Some(space) = ArchAddressSpace::new_identity_gigapages(&PAGE_TABLES, IDENTITY_GIB) else {
        fail("accessed_bit test: page-table pool exhausted", FAIL_POOL);
    };
    // SAFETY: the identity map covers this kernel's `pc`, `sp`, heap, the
    // page-table pool, and the `virt` device MMIO (all within `[0, 2 GiB)`),
    // so enabling translation does not move the ground under the running
    // code. Boot CPU.
    unsafe { space.switch() };
    let mut space = space;

    // The probe frame is identity-mapped RAM, so its physical address is
    // its kernel virtual address.
    let probe_paddr = core::ptr::addr_of!(PROBE_FRAME) as u64;

    // Map the probe page kernel read/write through the Arch HAL MMU
    // surface (the path the architecture-neutral kernel uses). `map_page`
    // sets AF eagerly, so the fresh leaf reads accessed.
    if space
        .map_page(TEST_VADDR, probe_paddr, PageFlags::READ | PageFlags::WRITE)
        .is_err()
    {
        fail("accessed_bit test: probe-page mapping refused", FAIL_MAP);
    }

    // The port must declare it can report a referenced bit.
    if !matches!(space.access_tracking(), AccessTracking::Supported) {
        fail(
            "accessed_bit test: aarch64 must declare AccessTracking::Supported",
            FAIL_NOT_SUPPORTED,
        );
    }

    // Bring up the EL1 synchronous vectors so the Access-Flag fault is
    // dispatched (and resolved) by the production exception path, and
    // install the unexpected-fault handler.
    // SAFETY: called once on the boot CPU with a stack established and the
    // MMU enabled (the address-space build switched it on).
    unsafe { exceptions::init_vectors() };
    if fault::set_fault_handler(on_fault).is_err() {
        fail(
            "accessed_bit test: fault handler already installed",
            FAIL_MAP,
        );
    }
    SETUP_DONE.store(true, Ordering::SeqCst);
    note(TEST_START, "accessed_bit test: vectors up, probe mapped");

    // ---- Fail-closed edges. ----
    match space.test_and_clear_accessed(MISALIGNED_VADDR) {
        Err(MapError::Misaligned) => {}
        _ => fail(
            "accessed_bit test: misaligned address must be rejected",
            FAIL_MISALIGNED_EDGE,
        ),
    }
    match space.test_and_clear_accessed(UNMAPPED_VADDR) {
        Err(MapError::NotMapped) => {}
        _ => fail(
            "accessed_bit test: unmapped address must report NotMapped",
            FAIL_UNMAPPED_EDGE,
        ),
    }
    note(TEST_START, "accessed_bit test: fail-closed edges OK");

    // Seed the probe frame through the mapping (AF is set from the eager
    // map, so this first access does not fault) and read it back.
    // SAFETY: `TEST_VADDR` is mapped kernel-RW in the active space.
    unsafe { core::ptr::write_volatile(TEST_VADDR as *mut u8, PROBE_BYTE) };
    if touch(TEST_VADDR) != PROBE_BYTE {
        fail(
            "accessed_bit test: probe frame read back wrong byte",
            FAIL_SEED_READBACK,
        );
    }

    // ---- Probe 1: the eager map left AF set → reads accessed, clears AF. ----
    match space.test_and_clear_accessed(TEST_VADDR) {
        Ok(true) => {}
        _ => fail(
            "accessed_bit test: fresh (accessed) page must read set",
            FAIL_PROBE_FRESH,
        ),
    }
    note(TEST_START, "accessed_bit test: probe 1 (fresh) = set OK");

    // ---- Probe 2: no access since the clear → cold. ----
    match space.test_and_clear_accessed(TEST_VADDR) {
        Ok(false) => {}
        _ => fail(
            "accessed_bit test: untouched page must read clear",
            FAIL_PROBE_COLD,
        ),
    }
    note(TEST_START, "accessed_bit test: probe 2 (cold) = clear OK");

    // ---- Access the page: AF is clear, so this takes an Access-Flag
    // fault the exception path resolves by setting AF and retrying. If the
    // software AF path did not work, we would loop or hit `on_fault`. ----
    if touch(TEST_VADDR) != PROBE_BYTE {
        fail(
            "accessed_bit test: post-fault read returned wrong byte",
            FAIL_SEED_READBACK,
        );
    }

    // ---- Probe 3: the fault handler re-set AF → reads accessed. ----
    match space.test_and_clear_accessed(TEST_VADDR) {
        Ok(true) => {}
        _ => fail(
            "accessed_bit test: page must read set after the access-flag fault",
            FAIL_PROBE_AFTER_FAULT,
        ),
    }
    note(
        TEST_START,
        "accessed_bit test: probe 3 (after fault) = set OK",
    );

    // ---- Probe 4: cold again. ----
    match space.test_and_clear_accessed(TEST_VADDR) {
        Ok(false) => {}
        _ => fail(
            "accessed_bit test: page must read clear again",
            FAIL_PROBE_COLD_AGAIN,
        ),
    }

    // ---- Access again, then probe 5 → set (the CPU re-faults and the
    // handler re-sets AF after every clear). ----
    let _ = touch(TEST_VADDR);
    match space.test_and_clear_accessed(TEST_VADDR) {
        Ok(true) => {}
        _ => fail(
            "accessed_bit test: page must read set after re-access",
            FAIL_PROBE_REACCESS,
        ),
    }
    note(
        TEST_START,
        "accessed_bit test: probe 5 (re-accessed) = set OK",
    );

    note(
        TEST_PASS,
        "accessed_bit test: software Access-Flag clock proven through the HAL",
    );
    qemu_exit::exit_success();
}
