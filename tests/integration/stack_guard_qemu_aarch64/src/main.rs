//! `plans/PI.md` guard-page fault-form (stage G1) QEMU integration test:
//! the aarch64 page-table block-split turns a single 4 KiB page inside a
//! coarse identity block into an unmappable leaf, so an access to it
//! raises a synchronous hardware fault.
//!
//! ## Why this exists
//!
//! The kthread kernel-stack guard (`kernel/core::kthread`) currently
//! catches a stack overflow with a poison canary checked at the next
//! reschedule (the binding defence). The *deployment* form turns the
//! overflow into an immediate hardware fault by **unmapping** the guard
//! page. But the boot path identity-maps RAM with coarse 1 GiB / 2 MiB
//! *block* descriptors, and a block has no per-4 KiB leaf to clear — so the
//! region must first be re-expressed at 4 KiB granularity. That is exactly
//! `AddressSpace::split_block`, and this vertical proves the live
//! mechanism end to end on the `virt` board.
//!
//! ## What this test asserts
//!
//! 1. Build a stage-1 `AddressSpace` identity-mapping the low 2 GiB (so the
//!    kernel's code/stack and the device MMIO stay reachable). `GUARD_PAGE`
//!    — a dedicated, page-aligned static in RAM — is therefore mapped by a
//!    coarse block, *not* a 4 KiB leaf.
//! 2. `split_block(guard_va)`: shatter the 1 GiB block to 2 MiB blocks and
//!    the covering 2 MiB block to 4 KiB pages, preserving every mapping.
//!    The split only *adds* table levels reproducing the existing
//!    translation, so it is safe against the running region.
//! 3. Activate the space (enabling the MMU). Write a sentinel through
//!    `guard_va` and read it back: the split preserved the mapping under
//!    the live MMU (a regression here reports FAILURE, it does not hang).
//! 4. `unmap(guard_va)` + `flush_page(guard_va)`: tear the single page down
//!    through the Arch HAL and flush its stale TLB entry. The kernel's
//!    code/stack live in *other* pages of the same 2 MiB region and stay
//!    mapped.
//! 5. Read `guard_va`: the MMU raises a data abort; the handler confirms it
//!    is an abort on exactly `guard_va` and reports PASS via the ARM
//!    semihosting finisher. A regression that left the page mapped reads it
//!    without faulting and reports FAILURE explicitly.
//!
//! ## How it differs from a production kernel
//!
//! It links only the `rustos-arch-aarch64` port and supplies its own
//! `kernel_main`. The QEMU-exit shortcut lives in this dedicated bin,
//! never behind a Cargo feature on the arch crate (fail closed).

#![cfg_attr(itest_aarch64, no_std)]
#![cfg_attr(itest_aarch64, no_main)]
#![deny(missing_docs)]

// --- Freestanding test bin (`aarch64-unknown-none`) ----------------

#[cfg(itest_aarch64)]
mod kernel {
    use core::panic::PanicInfo;

    use rustos_arch_aarch64::paging::{AddressSpace, PageTablePool, PAGE_SIZE};
    use rustos_arch_aarch64::{exceptions, fault, handle_panic_via_serial, qemu_exit, SERIAL_SINK};
    use rustos_arch_api::mmu::AddressSpace as _;
    use rustos_arch_api::tlb::TlbShootdown as _;
    use rustos_log::{log, Event, EventId, Field, Level};

    /// Number of GiB the space identity-maps (device MMIO + RAM). The
    /// kernel image, stack, and `GUARD_PAGE` all live in the Normal RAM
    /// gigapage (GiB 1).
    const IDENTITY_GIB: usize = 2;

    /// The sentinel written through the guard page after the split, to
    /// prove the split preserved the mapping before the page is torn down.
    const SENTINEL: u8 = 0xA5;

    /// Stable audit-event ids for the QEMU transcript.
    const SG_TEST_START: EventId = EventId(4300);
    const SG_TEST_PASS: EventId = EventId(4301);
    const SG_TEST_FAIL: EventId = EventId(4302);

    /// Page-table pool backing the address space (lives in `.bss`).
    static POOL: PageTablePool = PageTablePool::new();

    /// A dedicated 4 KiB page that the test unmaps after splitting the
    /// coarse block that covers it. It is its own aligned page, so tearing
    /// it down disturbs no other kernel data: nothing else shares the page,
    /// and the running code/stack live in *other* pages of the same 2 MiB
    /// region (which stay mapped). Its physical address is its
    /// identity-mapped virtual address.
    #[repr(C, align(4096))]
    struct GuardPage([u8; PAGE_SIZE]);
    static mut GUARD_PAGE: GuardPage = GuardPage([0; PAGE_SIZE]);

    /// The fault handler: confirm the trap is a data/instruction abort on
    /// exactly the (now-unmapped) guard page, then report PASS. Anything
    /// else is a FAILURE. Never returns.
    extern "C" fn on_fault(esr: u64, far: u64, _elr: u64) -> ! {
        let guard_va = core::ptr::addr_of!(GUARD_PAGE) as u64;
        if fault::is_abort(esr) && far == guard_va {
            log(
                &SERIAL_SINK,
                &Event {
                    level: Level::Info,
                    id: SG_TEST_PASS,
                    message: "aarch64 stack-guard test: faulted on the unmapped guard page",
                    fields: &[],
                },
            );
            qemu_exit::exit_success();
        }
        log(
            &SERIAL_SINK,
            &Event {
                level: Level::Error,
                id: SG_TEST_FAIL,
                message: "aarch64 stack-guard test: unexpected fault",
                fields: &[],
            },
        );
        qemu_exit::exit_failure(3);
    }

    /// Forward to the shared aarch64 panic bridge (parks the CPU; the run
    /// then times out and the harness reports the failure).
    #[panic_handler]
    fn rustos_stack_guard_aarch64_panic(info: &PanicInfo<'_>) -> ! {
        handle_panic_via_serial(info)
    }

    /// Boot entry point — the symbol the arch crate's `boot.s` trampoline
    /// calls (via `rustos_arch_aarch64_main`).
    #[no_mangle]
    pub extern "C" fn kernel_main(_dtb: u64) -> ! {
        log(
            &SERIAL_SINK,
            &Event {
                level: Level::Info,
                id: SG_TEST_START,
                message: "aarch64 stack-guard test: splitting a block to unmap one guard page",
                fields: &[],
            },
        );

        // The guard page's address is its identity-mapped physical address.
        let guard_va = core::ptr::addr_of!(GUARD_PAGE) as u64;

        // Build the identity space. `GUARD_PAGE` is mapped by a coarse
        // block (no 4 KiB leaf yet).
        let mut space = AddressSpace::new_identity_gigapages(&POOL, IDENTITY_GIB)
            .unwrap_or_else(|| fail("identity map"));

        // Re-express the region covering the guard page at 4 KiB
        // granularity. Done while the space is inactive, so even the coarse
        // 1 GiB→2 MiB step cannot disturb the running region.
        space
            .split_block(guard_va)
            .unwrap_or_else(|_| fail("split_block"));

        // Install the vector table and fault handler before enabling the
        // MMU so the deliberate abort is routed to `on_fault`.
        fault::set_fault_handler(on_fault).unwrap_or_else(|_| fail("set_fault_handler"));
        // SAFETY: called once on the boot CPU before any fault can fire.
        unsafe {
            exceptions::init_vectors();
        }

        // Switch to the space (enables the MMU). It identity-maps this
        // code, the stack, and the device MMIO, so execution continues.
        // SAFETY: the space identity-maps `pc`, `sp`, and MMIO (RAM Normal,
        // device-0 Device) per `new_identity_gigapages`; the split only
        // re-expressed the same translation at finer granularity.
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
            fail("split did not preserve the guard-page mapping");
        }

        // Tear the single guard page down through the Arch HAL and flush
        // its stale TLB entry — exactly the production guard-page mechanism.
        space
            .unmap(guard_va)
            .unwrap_or_else(|_| fail("unmap guard page"));
        space.flush_page(guard_va);

        // Read the now-unmapped guard page. This must raise a translation
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
                message: "aarch64 stack-guard test: read the unmapped guard page (no fault)",
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
        qemu_exit::exit_failure(2);
    }

    /// Log a setup failure and report it to QEMU. Never returns.
    fn fail(what: &str) -> ! {
        log(
            &SERIAL_SINK,
            &Event {
                level: Level::Error,
                id: SG_TEST_FAIL,
                message: "aarch64 stack-guard test: setup failed",
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
