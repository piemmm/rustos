//! `plans/PI.md` guard-page fault-form (riscv64 stage G1) QEMU
//! integration test: the riscv64 Sv39 block-split turns a single 4 KiB
//! page inside a coarse identity leaf into an unmappable page, so an
//! access to it raises a synchronous load page fault.
//!
//! ## Why this exists
//!
//! The kthread kernel-stack guard (`kernel/core::kthread`) catches a
//! stack overflow with a poison canary checked at the next reschedule
//! (the binding defence). The *deployment* form turns the overflow
//! into an immediate hardware fault by **unmapping** the guard page. But
//! the boot path identity-maps RAM with coarse 1 GiB gigapage / 2 MiB
//! megapage *leaves*, and such a leaf has no per-4 KiB entry to clear — so
//! the region must first be re-expressed at 4 KiB granularity. That is
//! exactly `AddressSpace::split_block`, and this vertical proves the live
//! mechanism end to end on the `virt` board. It is the riscv64 sibling of
//! `tests/integration/stack_guard_qemu_aarch64`.
//!
//! ## What this test asserts
//!
//! 1. Build an Sv39 `AddressSpace` identity-mapping the low 4 GiB (so the
//!    kernel's code/stack and the device MMIO stay reachable). `GUARD_PAGE`
//!    — a dedicated, page-aligned static in RAM — is therefore mapped by a
//!    coarse leaf, *not* a 4 KiB page.
//! 2. `split_block(guard_va)`: shatter the 1 GiB gigapage to 2 MiB
//!    megapages and the covering megapage to 4 KiB pages, preserving every
//!    mapping. The split only *adds* table levels reproducing the existing
//!    translation, so it is safe against the running region.
//! 3. Activate the space (turn paging on). Write a sentinel through
//!    `guard_va` and read it back: the split preserved the mapping under
//!    the live MMU (a regression here reports FAILURE, it does not hang).
//! 4. `unmap(guard_va)` + `flush_page(guard_va)`: tear the single page down
//!    through the Arch HAL and flush its stale TLB entry. The kernel's
//!    code/stack live in *other* pages of the same megapage and stay
//!    mapped.
//! 5. Read `guard_va`: the MMU raises a load page fault; the handler
//!    confirms it is a load page fault on exactly `guard_va` and writes the
//!    `SiFive` Test PASS finisher. A regression that left the page mapped
//!    reads it without faulting and reports FAILURE explicitly.
//!
//! ## How it differs from a production kernel
//!
//! It links only the `rustos-arch-riscv64` port and supplies its own
//! `kernel_main`. The QEMU-exit shortcut lives in this dedicated bin,
//! never behind a Cargo feature on the arch crate (fail closed).

#![cfg_attr(itest_riscv64, no_std)]
#![cfg_attr(itest_riscv64, no_main)]
#![deny(missing_docs)]

#[cfg(itest_riscv64)]
mod kernel {
    use core::panic::PanicInfo;
    use core::sync::atomic::{AtomicBool, Ordering};

    use rustos_arch_api::mmu::AddressSpace as _;
    use rustos_arch_api::tlb::TlbShootdown as _;
    use rustos_arch_riscv64::paging::{AddressSpace, PageTablePool, PAGE_SIZE};
    use rustos_arch_riscv64::{fault, handle_panic_via_serial, qemu_exit, trap, SERIAL_SINK};
    use rustos_log::{log, Event, EventId, Field, Level};

    /// Gigapages of identity map the space installs: `[0, 4 GiB)` covers
    /// the `virt` board's low MMIO and the 2 GiB RAM base at `0x8000_0000`
    /// where this kernel (and `GUARD_PAGE`) runs.
    const IDENTITY_GIB: usize = 4;

    /// The sentinel written through the guard page after the split, to
    /// prove the split preserved the mapping before the page is torn down.
    const SENTINEL: u8 = 0xA5;

    /// Stable audit-event ids for the QEMU transcript.
    const SG_TEST_START: EventId = EventId(4310);
    const SG_TEST_PASS: EventId = EventId(4311);
    const SG_TEST_FAIL: EventId = EventId(4312);

    /// `SiFive` Test failure codes, distinct per failure site so a failing
    /// run's exit status pinpoints the broken invariant.
    const FAIL_FAULT_BEFORE_UNMAP: u16 = 1;
    const FAIL_WRONG_CAUSE: u16 = 2;
    const FAIL_WRONG_STVAL: u16 = 3;
    const FAIL_NO_FAULT: u16 = 4;
    const FAIL_SETUP: u16 = 5;
    const FAIL_SPLIT_LOST_MAPPING: u16 = 6;

    /// Page-table pool backing the address space (lives in `.bss`).
    static POOL: PageTablePool = PageTablePool::new();

    /// A dedicated 4 KiB page that the test unmaps after splitting the
    /// coarse leaf that covers it. It is its own aligned page, so tearing
    /// it down disturbs no other kernel data: nothing else shares the page,
    /// and the running code/stack live in *other* pages of the same
    /// megapage (which stay mapped). Its physical address is its
    /// identity-mapped virtual address.
    #[repr(C, align(4096))]
    struct GuardPage([u8; PAGE_SIZE]);
    static mut GUARD_PAGE: GuardPage = GuardPage([0; PAGE_SIZE]);

    /// `true` once the guard page has been unmapped — lets [`on_fault`]
    /// tell the *expected* fault from a kernel bug that faults earlier.
    static GUARD_UNMAPPED: AtomicBool = AtomicBool::new(false);

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

    /// The synchronous-exception handler the trap vector invokes. The read
    /// of the unmapped guard page must land here as a load page fault on
    /// exactly `guard_va`; anything else is a closed failure.
    extern "C" fn on_fault(scause: u64, stval: u64, _sepc: u64) -> ! {
        let guard_va = core::ptr::addr_of!(GUARD_PAGE) as u64;
        if !GUARD_UNMAPPED.load(Ordering::SeqCst) {
            note(
                Level::Error,
                SG_TEST_FAIL,
                "fault before unmap — kernel bug",
            );
            qemu_exit::exit_failure(FAIL_FAULT_BEFORE_UNMAP);
        }
        if scause != fault::SCAUSE_LOAD_PAGE_FAULT {
            note(
                Level::Error,
                SG_TEST_FAIL,
                "unexpected trap cause, not a load page fault",
            );
            qemu_exit::exit_failure(FAIL_WRONG_CAUSE);
        }
        if stval != guard_va {
            note(
                Level::Error,
                SG_TEST_FAIL,
                "load page fault at the wrong address",
            );
            qemu_exit::exit_failure(FAIL_WRONG_STVAL);
        }
        note(
            Level::Info,
            SG_TEST_PASS,
            "riscv64 stack-guard test: faulted on the unmapped guard page",
        );
        qemu_exit::exit_success();
    }

    /// Forward to the shared riscv64 panic bridge (parks the hart; the run
    /// then times out and the harness reports the failure).
    #[panic_handler]
    fn rustos_stack_guard_riscv64_panic(info: &PanicInfo<'_>) -> ! {
        handle_panic_via_serial(info)
    }

    /// Log a setup failure and report it to QEMU. Never returns.
    fn fail(what: &'static str, code: u16) -> ! {
        log(
            &SERIAL_SINK,
            &Event {
                level: Level::Error,
                id: SG_TEST_FAIL,
                message: "riscv64 stack-guard test: setup failed",
                fields: &[Field {
                    key: "stage",
                    value: rustos_log::FieldValue::Str(what),
                }],
            },
        );
        qemu_exit::exit_failure(code);
    }

    /// Boot entry point — the symbol the arch crate's `boot.s` trampoline
    /// calls (via `rustos_arch_riscv64_main`).
    #[no_mangle]
    pub extern "C" fn kernel_main(_hartid: u64, _dtb: u64) -> ! {
        note(
            Level::Info,
            SG_TEST_START,
            "riscv64 stack-guard test: splitting a leaf to unmap one guard page",
        );

        // The guard page's address is its identity-mapped physical address.
        let guard_va = core::ptr::addr_of!(GUARD_PAGE) as u64;

        // Build the identity space. `GUARD_PAGE` is mapped by a coarse leaf
        // (no 4 KiB page yet).
        let mut space = match AddressSpace::new_identity_gigapages(&POOL, IDENTITY_GIB) {
            Some(space) => space,
            None => fail("identity map", FAIL_SETUP),
        };

        // Re-express the region covering the guard page at 4 KiB
        // granularity. Done while paging is off, so even the coarse
        // gigapage→megapage step cannot disturb the running region.
        if space.split_block(guard_va).is_err() {
            fail("split_block", FAIL_SETUP);
        }

        // Install the trap vector + fault handler before turning paging on
        // so the deliberate fault is routed to `on_fault`.
        if fault::set_fault_handler(on_fault).is_err() {
            fail("set_fault_handler", FAIL_SETUP);
        }
        // SAFETY: called once on the boot hart with a stack established and
        // the fault handler installed; no interrupt source is armed, so
        // only the synchronous page fault below reaches the vector.
        unsafe {
            trap::init_traps();
        }

        // Switch to the space (turns paging on). It identity-maps this
        // code, the stack, and the device MMIO, so execution continues.
        // SAFETY: the space identity-maps `pc`, `sp`, and MMIO per
        // `new_identity_gigapages`; the split only re-expressed the same
        // translation at finer granularity.
        unsafe {
            space.activate();
        }

        // Prove the split preserved the guard page's mapping under the live
        // MMU: write a sentinel and read it back.
        // SAFETY: `guard_va` maps `GUARD_PAGE`, a live page-aligned static
        // mapped RW; the access is well-defined while the page is mapped.
        unsafe {
            core::ptr::write_volatile(guard_va as *mut u8, SENTINEL);
        }
        let readback = unsafe { core::ptr::read_volatile(guard_va as *const u8) };
        if readback != SENTINEL {
            fail(
                "split did not preserve the guard-page mapping",
                FAIL_SPLIT_LOST_MAPPING,
            );
        }

        // Tear the single guard page down through the Arch HAL and flush
        // its stale TLB entry — exactly the production guard-page mechanism.
        if space.unmap(guard_va).is_err() {
            fail("unmap guard page", FAIL_SETUP);
        }
        space.flush_page(guard_va);
        GUARD_UNMAPPED.store(true, Ordering::SeqCst);

        // Read the now-unmapped guard page. This must raise a load page
        // fault → `on_fault` (which exits PASS).
        // SAFETY: the access is *expected* to fault; if the MMU wrongly
        // permitted it the read is still of a valid pointer-sized region we
        // then report as a FAILURE below.
        let observed = unsafe { core::ptr::read_volatile(guard_va as *const u8) };

        // Reaching here means the unmapped page was read without a fault —
        // the guard FAILED. Reference `observed` so the read is not elided.
        log(
            &SERIAL_SINK,
            &Event {
                level: Level::Error,
                id: SG_TEST_FAIL,
                message: "riscv64 stack-guard test: read the unmapped guard page (no fault)",
                fields: &[Field {
                    key: "observed",
                    value: rustos_log::FieldValue::Str(if observed == SENTINEL {
                        "sentinel"
                    } else {
                        "other"
                    }),
                }],
            },
        );
        qemu_exit::exit_failure(FAIL_NO_FAULT);
    }
}

// --- Host stub -----------------------------------------------------
#[cfg(not(itest_riscv64))]
fn main() {}

#[cfg(not(itest_riscv64))]
#[allow(dead_code)]
fn _suppress_no_main() {}
