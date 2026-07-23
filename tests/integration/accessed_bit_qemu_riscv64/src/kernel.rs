//! The freestanding riscv64 test kernel: stand up an Sv39 identity space,
//! install the trap vector, map one supervisor page, and drive the
//! software-managed Accessed-bit clock through the Arch HAL — proving the
//! A/D-setting page-fault path resolves a cleared-A access on a Svade CPU.

use core::panic::PanicInfo;
use core::sync::atomic::{AtomicBool, Ordering};

use tairix_arch_api::mmu::{AccessTracking, AddressSpace as _, MapError, PageFlags};
use tairix_arch_riscv64::{fault, handle_panic_via_serial, paging, qemu_exit, trap, SERIAL_SINK};
use tairix_kalloc::{FreeListAllocator, Heap, HEAP_BYTES};
use tairix_log::{log, Event, EventId, Level};

/// Gigabytes of identity map the address space provides: `[0, 4 GiB)`
/// covers the `virt` board's low MMIO and the RAM base where this kernel
/// and its page-table pool run. The probe page ([`TEST_VADDR`], 64 GiB)
/// sits far above, on freshly walked Sv39 tables.
const IDENTITY_GIGABYTES: usize = 4;

/// Virtual address the test maps its single 4 KiB probe page at. Chosen at
/// 64 GiB — far above the identity window and within the Sv39 39-bit VA
/// space — so the mapping is a fresh 4 KiB leaf (its own L1/L0), never a
/// gigapage/megapage.
const TEST_VADDR: u64 = 64 << 30;

/// A misaligned address, to confirm the fail-closed reject.
const MISALIGNED_VADDR: u64 = TEST_VADDR + 0x123;

/// An address mapped nowhere in the test space (128 GiB), to confirm the
/// "not mapped" fail-closed reject.
const UNMAPPED_VADDR: u64 = 128 << 30;

/// Magic byte written into the probe frame so the accesses read real data.
const PROBE_BYTE: u8 = 0x5A;

/// Stable audit-event ids for the QEMU transcript.
const TEST_START: EventId = EventId(4320);
const TEST_PASS: EventId = EventId(4321);
const TEST_FAIL: EventId = EventId(4322);

/// `SiFive` Test failure codes, distinct per failure site.
const FAIL_POOL: u16 = 1;
const FAIL_MAP: u16 = 2;
const FAIL_NOT_SUPPORTED: u16 = 3;
const FAIL_FAULT_INSTALL: u16 = 4;
const FAIL_MISALIGNED_EDGE: u16 = 5;
const FAIL_UNMAPPED_EDGE: u16 = 6;
const FAIL_SEED_READBACK: u16 = 7;
const FAIL_PROBE_FRESH: u16 = 8;
const FAIL_PROBE_COLD: u16 = 9;
const FAIL_PROBE_AFTER_FAULT: u16 = 10;
const FAIL_PROBE_COLD_AGAIN: u16 = 11;
const FAIL_PROBE_REACCESS: u16 = 12;
const FAIL_UNEXPECTED_FAULT: u16 = 13;

/// `true` once setup is complete. Any unexpected fault before this is a
/// setup bug; after it, a fault the software A/D path did not resolve.
static SETUP_DONE: AtomicBool = AtomicBool::new(false);

/// Static boot heap, in the linker's NOLOAD `.heap` section so the boot
/// trampoline neither zeroes nor counts it in the usable memory map. The
/// riscv64 port names `alloc`, so a `#[global_allocator]` must be present
/// even though this test allocates nothing itself.
#[link_section = ".heap"]
static mut HEAP: Heap = Heap::ZERO;

/// Global allocator backed by [`HEAP`].
///
/// SAFETY: the page-aligned `HEAP` static outlives the binary and the
/// allocator is its only consumer.
#[global_allocator]
static ALLOCATOR: FreeListAllocator =
    unsafe { FreeListAllocator::new(core::ptr::addr_of!(HEAP) as *mut u8, HEAP_BYTES) };

/// Page-table pool backing the Sv39 hierarchy (lives in `.bss`).
static PAGE_TABLE_POOL: paging::PageTablePool = paging::PageTablePool::new();

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
fn fail(message: &'static str, code: u16) -> ! {
    note(TEST_FAIL, message);
    qemu_exit::exit_failure(code);
}

/// Read one byte from `vaddr` so the hart translates the leaf and (when A
/// is clear, under Svade) raises a load page fault. Volatile so the
/// compiler cannot elide it.
fn touch(vaddr: u64) -> u8 {
    // SAFETY: `vaddr` is mapped supervisor-RW in the active space; the
    // read observes the probe frame and exercises the leaf's Accessed bit.
    let byte = unsafe { core::ptr::read_volatile(vaddr as *const u8) };
    core::hint::black_box(byte)
}

/// Forward to the shared riscv64 panic bridge (parks the hart; the run
/// then times out and the harness reports the failure).
#[panic_handler]
fn accessed_bit_qemu_riscv64_panic(info: &PanicInfo<'_>) -> ! {
    handle_panic_via_serial(info)
}

/// The unexpected-fault handler: the software A/D faults are resolved
/// inside the trap dispatch and never reach here, so any fault reported to
/// this handler is a bug.
extern "C" fn on_fault(_scause: u64, _stval: u64, _sepc: u64) -> ! {
    let msg = if SETUP_DONE.load(Ordering::SeqCst) {
        "accessed_bit test: unexpected fault after setup (A/D path did not resolve)"
    } else {
        "accessed_bit test: unexpected fault during setup"
    };
    fail(msg, FAIL_UNEXPECTED_FAULT);
}

/// Boot entry point — the symbol the arch crate's `boot.s` trampoline
/// calls (via `tairix_arch_riscv64_main`).
#[no_mangle]
pub extern "C" fn kernel_main(_hartid: u64, _dtb: u64) -> ! {
    note(TEST_START, "riscv64 accessed-bit test: starting");

    // Build the Sv39 identity address space and activate it so the probe
    // mapping lands in the live translation regime.
    let Some(mut space) =
        paging::AddressSpace::new_identity_gigapages(&PAGE_TABLE_POOL, IDENTITY_GIGABYTES)
    else {
        fail("accessed_bit test: page-table pool exhausted", FAIL_POOL);
    };
    // SAFETY: the identity map covers this kernel's `pc`, `sp`, heap, the
    // page-table pool, and the `virt` device MMIO (all within `[0, 4 GiB)`),
    // so the `satp` switch does not move the ground under the running code.
    // Boot hart.
    unsafe { space.switch() };

    // Install the trap vector so the software A/D page fault is dispatched
    // (and resolved) by the production trap path, and install the
    // unexpected-fault handler.
    // SAFETY: called once on the boot hart with a stack established; only
    // this test's deliberate accesses reach the vector (no interrupt
    // source is armed).
    unsafe { trap::init_traps() };
    if fault::set_fault_handler(on_fault).is_err() {
        fail(
            "accessed_bit test: fault handler already installed",
            FAIL_FAULT_INSTALL,
        );
    }

    // The probe frame is identity-mapped RAM, so its physical address is
    // its kernel virtual address.
    let probe_paddr = core::ptr::addr_of!(PROBE_FRAME) as u64;

    // Map the probe page supervisor read/write (no USER bit) through the
    // Arch HAL MMU surface. `map_page` sets A eagerly, so the fresh leaf
    // reads accessed.
    if space
        .map_page(TEST_VADDR, probe_paddr, PageFlags::READ | PageFlags::WRITE)
        .is_err()
    {
        fail("accessed_bit test: probe-page mapping refused", FAIL_MAP);
    }

    // The port must declare it can report a referenced bit.
    if !matches!(space.access_tracking(), AccessTracking::Supported) {
        fail(
            "accessed_bit test: riscv64 must declare AccessTracking::Supported",
            FAIL_NOT_SUPPORTED,
        );
    }
    SETUP_DONE.store(true, Ordering::SeqCst);
    note(TEST_START, "accessed_bit test: traps up, probe mapped");

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

    // Seed the probe frame through the mapping (A is set from the eager
    // map, so this first access does not fault) and read it back.
    // SAFETY: `TEST_VADDR` is mapped supervisor-RW in the active space.
    unsafe { core::ptr::write_volatile(TEST_VADDR as *mut u8, PROBE_BYTE) };
    if touch(TEST_VADDR) != PROBE_BYTE {
        fail(
            "accessed_bit test: probe frame read back wrong byte",
            FAIL_SEED_READBACK,
        );
    }

    // ---- Probe 1: the eager map left A set → reads accessed, clears A. ----
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

    // ---- Access the page: A is clear, so under Svade this takes a load
    // page fault the trap path resolves by setting A and retrying. If the
    // software A/D path did not work, we would loop or hit `on_fault`. ----
    if touch(TEST_VADDR) != PROBE_BYTE {
        fail(
            "accessed_bit test: post-fault read returned wrong byte",
            FAIL_SEED_READBACK,
        );
    }

    // ---- Probe 3: the fault path re-set A → reads accessed. ----
    match space.test_and_clear_accessed(TEST_VADDR) {
        Ok(true) => {}
        _ => fail(
            "accessed_bit test: page must read set after the A/D fault",
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

    // ---- Access again, then probe 5 → set (the hart re-faults and the
    // trap path re-sets A after every clear). ----
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
        "accessed_bit test: software Accessed-bit clock proven through the HAL",
    );
    qemu_exit::exit_success();
}
