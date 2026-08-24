//! Stage 3b QEMU integration test: the aarch64 MMU enforces memory
//! isolation between two stage-1 address spaces.
//!
//! ## What this test asserts
//!
//! The Stage-3 per-sub-stage checklist requires that "the
//! memory-isolation test passes" on each architecture — that the MMU,
//! not software, isolates one address space's frames from another. This binary exercises that on the aarch64 `virt`
//! board, end to end:
//!
//! 1. Build two stage-1 `AddressSpace`s that each identity-map the low
//!    2 GiB (so the kernel's code/stack and the device MMIO stay
//!    reachable), but disagree on a single page at `VICTIM_VA` (well
//!    above the identity window): the *victim* maps it, the *attacker*
//!    does not.
//! 2. Install an EL1 vector table and a fault handler.
//! 3. Switch to the *attacker* space (enabling the MMU) and read
//!    `VICTIM_VA`.
//! 4. The MMU raises a translation (data abort) fault; the handler
//!    confirms it is an abort on exactly `VICTIM_VA` and reports PASS
//!    through the ARM semihosting finisher. Any other outcome reports
//!    FAILURE.
//!
//! A regression that fails to isolate the page never faults, so the read
//! returns and the binary reports FAILURE explicitly (it does not hang).
//!
//! ## How it differs from a production kernel
//!
//! It links only the `tairix-arch-aarch64` port and supplies its own
//! `kernel_main`. The QEMU-exit shortcut lives in this dedicated bin,
//! never behind a Cargo feature on the arch crate (fail closed).

#![cfg_attr(itest_aarch64, no_std)]
#![cfg_attr(itest_aarch64, no_main)]
#![deny(missing_docs)]

// --- Freestanding test bin (`aarch64-unknown-none`) ----------------

#[cfg(itest_aarch64)]
mod kernel {
    use core::num::NonZeroU16;
    use core::panic::PanicInfo;

    use tairix_arch_aarch64::paging::{AddressSpace, PageTablePool, PAGE_SIZE};
    use tairix_arch_aarch64::{exceptions, fault, handle_panic_via_serial, qemu_exit, SERIAL_SINK};
    use tairix_arch_api::mmu::{AddressSpace as _, PageFlags};
    use tairix_itest_finisher::fail_point;
    use tairix_log::{log, Event, EventId, Level};

    /// Virtual address the victim space maps and the attacker does not.
    /// 64 GiB — well above the 2 GiB identity window — so the walk uses
    /// fresh L2/L3 tables rather than shattering an identity block.
    const VICTIM_VA: u64 = 64 << 30;

    /// Number of GiB both spaces identity-map (device MMIO + RAM).
    const IDENTITY_GIB: usize = 2;

    /// Stable audit-event ids for the QEMU transcript.
    const MISO_TEST_START: EventId = EventId(4230);
    const MISO_TEST_PASS: EventId = EventId(4231);
    const MISO_TEST_FAIL: EventId = EventId(4232);
    /// Failure finisher codes, distinct per failure site.
    const FAIL_NO_FAULT: NonZeroU16 = fail_point!(2);
    const FAIL_UNEXPECTED_FAULT: NonZeroU16 = fail_point!(3);
    const FAIL_SETUP: NonZeroU16 = fail_point!(4);

    /// Page-table pool backing both address spaces (lives in `.bss`).
    static POOL: PageTablePool = PageTablePool::new();

    /// A 4 KiB page the victim space maps at [`VICTIM_VA`]. Its physical
    /// address is its identity-mapped address (the MMU is off when we
    /// read it). The attacker never maps it.
    #[repr(C, align(4096))]
    struct VictimPage([u8; PAGE_SIZE]);
    static mut VICTIM_PAGE: VictimPage = VictimPage([0; PAGE_SIZE]);

    /// The fault handler: confirm the trap is a data/instruction abort on
    /// exactly [`VICTIM_VA`], then report PASS. Anything else is a
    /// FAILURE. Never returns.
    extern "C" fn on_fault(esr: u64, far: u64, _elr: u64) -> ! {
        if fault::is_abort(esr) && far == VICTIM_VA {
            log(
                &SERIAL_SINK,
                &Event {
                    level: Level::Info,
                    id: MISO_TEST_PASS,
                    message: "aarch64 memory-isolation test: attacker faulted on victim page",
                    fields: &[],
                },
            );
            qemu_exit::exit_success();
        }
        log(
            &SERIAL_SINK,
            &Event {
                level: Level::Error,
                id: MISO_TEST_FAIL,
                message: "aarch64 memory-isolation test: unexpected fault",
                fields: &[],
            },
        );
        qemu_exit::exit_failure(FAIL_UNEXPECTED_FAULT);
    }

    /// Forward to the shared aarch64 panic bridge (parks the CPU; the run
    /// then times out and the harness reports the failure).
    #[panic_handler]
    fn tairix_memory_isolation_aarch64_panic(info: &PanicInfo<'_>) -> ! {
        handle_panic_via_serial(info)
    }

    /// Boot entry point — the symbol the arch crate's `boot.s`
    /// trampoline calls (via `tairix_arch_aarch64_main`).
    #[no_mangle]
    pub extern "C" fn kernel_main(_dtb: u64) -> ! {
        log(
            &SERIAL_SINK,
            &Event {
                level: Level::Info,
                id: MISO_TEST_START,
                message: "aarch64 memory-isolation test: building address spaces",
                fields: &[],
            },
        );

        // Physical address of the victim page (MMU is off → VA == PA).
        let victim_pa = core::ptr::addr_of!(VICTIM_PAGE) as u64;

        // Victim space: identity map + the extra VICTIM_VA mapping.
        let mut victim = AddressSpace::new_identity_gigapages(&POOL, IDENTITY_GIB)
            .unwrap_or_else(|| fail("victim identity map"));
        // Install the victim mapping through the Arch HAL MMU surface
        // (`tairix_arch_api::mmu::AddressSpace::map_page`), the path
        // the architecture-neutral kernel uses, rather than the port's
        // inherent `map_4k` (`plans/WIRING.md` W5b).
        victim
            .map_page(VICTIM_VA, victim_pa, PageFlags::READ | PageFlags::WRITE)
            .unwrap_or_else(|_| fail("victim map_page"));

        // Attacker space: identity map only — VICTIM_VA stays unmapped.
        let attacker = AddressSpace::new_identity_gigapages(&POOL, IDENTITY_GIB)
            .unwrap_or_else(|| fail("attacker identity map"));

        // Install the vector table and fault handler before enabling the
        // MMU so the abort is routed to `on_fault`.
        fault::set_fault_handler(on_fault).unwrap_or_else(|_| fail("set_fault_handler"));
        // SAFETY: called once on the boot CPU before any fault can fire.
        unsafe {
            exceptions::init_vectors();
        }

        // Switch to the attacker space (enables the MMU). The attacker
        // identity-maps this code, the stack, and the device MMIO, so
        // execution continues; only VICTIM_VA is absent.
        // SAFETY: the attacker space identity-maps `pc`, `sp`, and MMIO
        // (RAM Normal, device-0 Device) per `new_identity_gigapages`.
        unsafe {
            attacker.activate();
        }

        // Read the victim-only address. With the attacker space active
        // this must raise a translation fault → `on_fault` (which exits).
        // SAFETY: the access is *expected* to fault; if the MMU wrongly
        // permitted it the read is still of a valid pointer-sized region
        // we then report as a FAILURE below.
        let observed = unsafe { core::ptr::read_volatile(VICTIM_VA as *const u8) };

        // Reaching here means the attacker read the victim page without a
        // fault — isolation FAILED. Reference `observed` so the read is
        // not elided, and `victim` so it is not dropped before the switch.
        log(
            &SERIAL_SINK,
            &Event {
                level: Level::Error,
                id: MISO_TEST_FAIL,
                message: "aarch64 memory-isolation test: attacker read victim page (no fault)",
                fields: &[],
            },
        );
        let _ = (observed, victim.root_phys());
        qemu_exit::exit_failure(FAIL_NO_FAULT);
    }

    /// Log a setup failure and report it to QEMU. Never returns.
    fn fail(what: &str) -> ! {
        log(
            &SERIAL_SINK,
            &Event {
                level: Level::Error,
                id: MISO_TEST_FAIL,
                message: "aarch64 memory-isolation test: setup failed",
                fields: &[tairix_log::Field {
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
