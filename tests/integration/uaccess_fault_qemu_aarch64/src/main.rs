//! `tests/SECURITY.md` §5 `copy_from_user` hardware fault fix-up
//! (aarch64) QEMU integration test: a same-EL data abort taken inside
//! the guarded user-copy window is redirected to the copy's fix-up and
//! surfaces as an error — the CPU keeps running instead of halting.
//!
//! ## Why this exists
//!
//! The kernel's validated user-copy path (`kernel/mem::uaccess`) proves
//! every page before it moves a byte, so a mid-copy hardware fault means
//! that proof was violated underneath it. The per-port fault window
//! (`rustos_arch_aarch64::uaccess`) is the backstop that turns such a
//! fault into an error return; this vertical proves the whole mechanism
//! live on the `virt` board: real EL1 data abort → vector table →
//! frame-ELR rewrite → fix-up return.
//!
//! ## What this test asserts
//!
//! 1. Build a stage-1 `AddressSpace` identity-mapping the low 2 GiB and
//!    activate it. A virtual page *beyond* the identity map
//!    (`UNMAPPED_VA`, 3 GiB) has no translation.
//! 2. `exceptions::init_vectors` points `VBAR_EL1` at the vector table
//!    **and arms the Arch HAL guarded-copy slot** — the one chokepoint
//!    pairing the recovery with the vectors.
//! 3. The shared `rustos_arch_api::uaccess::conformance` checks pass:
//!    an intact copy moves its bytes exactly; a copy reading the
//!    unmapped page returns the fault error; a copy writing it returns
//!    the fault error — each taking a *real* hardware data abort whose
//!    redirect lets execution continue to the next check.
//! 4. The fatal `fault` handler reports FAILURE: reaching it means the
//!    window redirect did not absorb the deliberate abort.
//!
//! ## How it differs from a production kernel
//!
//! It links only the `rustos-arch-aarch64` port and supplies its own
//! `kernel_main`. The QEMU-exit shortcut lives in this dedicated bin,
//! never behind a Cargo feature on the arch crate (fail closed).

#![cfg_attr(itest_aarch64, no_std)]
#![cfg_attr(itest_aarch64, no_main)]
#![deny(missing_docs)]

#[cfg(itest_aarch64)]
mod kernel {
    use core::panic::PanicInfo;

    use rustos_arch_aarch64::paging::{AddressSpace, PageTablePool};
    use rustos_arch_aarch64::{
        enable_fp_el1, exceptions, fault, handle_panic_via_serial, qemu_exit, SERIAL_SINK,
    };
    use rustos_arch_api::mmu::AddressSpace as _;
    use rustos_arch_api::uaccess::conformance::{self, Verdict};
    use rustos_log::{log, Event, EventId, Field, Level};

    /// Number of GiB the space identity-maps (device MMIO + RAM). The
    /// kernel image and stack live in the Normal RAM gigapage (GiB 1).
    const IDENTITY_GIB: usize = 2;

    /// A page-aligned virtual address beyond the identity map (3 GiB):
    /// inside the TTBR0 span but with no translation, so any access to it
    /// raises a synchronous same-EL data abort.
    const UNMAPPED_VA: u64 = 3 * (1 << 30);

    /// Stable audit-event ids for the QEMU transcript.
    const UA_TEST_START: EventId = EventId(4320);
    const UA_TEST_PASS: EventId = EventId(4321);
    const UA_TEST_FAIL: EventId = EventId(4322);

    /// Semihosting failure codes, distinct per failure site so a failing
    /// run's exit status pinpoints the broken invariant.
    const FAIL_SETUP: u16 = 1;
    const FAIL_FATAL_FAULT: u16 = 2;
    const FAIL_NOT_INSTALLED: u16 = 3;
    const FAIL_INTACT_COPY: u16 = 4;
    const FAIL_FAULT_NOT_REPORTED: u16 = 5;

    /// Page-table pool backing the address space (lives in `.bss`).
    static POOL: PageTablePool = PageTablePool::new();

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

    /// The fatal synchronous-exception handler: reaching it means an abort
    /// escaped the guarded-copy window redirect (or something else
    /// faulted) — a closed failure either way.
    extern "C" fn on_fault(esr: u64, far: u64, elr: u64) -> ! {
        log(
            &SERIAL_SINK,
            &Event {
                level: Level::Error,
                id: UA_TEST_FAIL,
                message: "aarch64 uaccess-fault test: abort escaped the copy window redirect",
                fields: &[
                    Field {
                        key: "esr",
                        value: rustos_log::FieldValue::UnsignedInt(esr),
                    },
                    Field {
                        key: "far",
                        value: rustos_log::FieldValue::UnsignedInt(far),
                    },
                    Field {
                        key: "elr",
                        value: rustos_log::FieldValue::UnsignedInt(elr),
                    },
                ],
            },
        );
        qemu_exit::exit_failure(FAIL_FATAL_FAULT);
    }

    /// Forward to the shared aarch64 panic bridge (parks the CPU; the run
    /// then times out and the harness reports the failure).
    #[panic_handler]
    fn rustos_uaccess_fault_aarch64_panic(info: &PanicInfo<'_>) -> ! {
        handle_panic_via_serial(info)
    }

    /// Log a setup failure and report it to QEMU. Never returns.
    fn fail(what: &'static str, code: u16) -> ! {
        log(
            &SERIAL_SINK,
            &Event {
                level: Level::Error,
                id: UA_TEST_FAIL,
                message: "aarch64 uaccess-fault test: failed",
                fields: &[Field {
                    key: "stage",
                    value: rustos_log::FieldValue::Str(what),
                }],
            },
        );
        qemu_exit::exit_failure(code);
    }

    /// Boot entry point — the symbol the arch crate's `boot.s` trampoline
    /// calls (via `rustos_arch_aarch64_main`).
    #[no_mangle]
    pub extern "C" fn kernel_main(_dtb: u64) -> ! {
        // SAFETY: runs once on the boot CPU before any FP/SIMD
        // instruction executes (see `enable_fp_el1`).
        unsafe {
            enable_fp_el1();
        }

        note(
            Level::Info,
            UA_TEST_START,
            "aarch64 uaccess-fault test: faulting inside the guarded copy window",
        );

        // Build the identity space; `UNMAPPED_VA` lies beyond it.
        let space = match AddressSpace::new_identity_gigapages(&POOL, IDENTITY_GIB) {
            Some(space) => space,
            None => fail("identity map", FAIL_SETUP),
        };

        // Install the fatal handler (the FAILURE reporter), then the
        // vector table — which also arms the Arch HAL guarded-copy slot,
        // the pairing under test.
        if fault::set_fault_handler(on_fault).is_err() {
            fail("set_fault_handler", FAIL_SETUP);
        }
        // SAFETY: called once on the boot CPU with a stack established;
        // no interrupt source is armed, so only the deliberate synchronous
        // aborts below reach the vectors.
        unsafe {
            exceptions::init_vectors();
        }

        // Switch to the space (enables the MMU). It identity-maps this
        // code, the stack, and the device MMIO, so execution continues.
        // SAFETY: the space identity-maps `pc`, `sp`, and MMIO (RAM
        // Normal, device-0 Device) per `new_identity_gigapages`.
        unsafe {
            space.activate();
        }

        // Drive the shared checks: positive control, then a real abort on
        // the read side and the write side of the window.
        let mut scratch = [0u8; 64];
        // SAFETY: `UNMAPPED_VA` lies beyond the identity space just
        // activated, so the page is genuinely unmapped and the deliberate
        // aborts are absorbed by the guarded copy window armed above.
        let verdict = unsafe { conformance::run(UNMAPPED_VA as *mut u8, &mut scratch) };
        match verdict {
            Verdict::Pass => {
                note(
                    Level::Info,
                    UA_TEST_PASS,
                    "aarch64 uaccess-fault test: in-window aborts surfaced as errors",
                );
                qemu_exit::exit_success();
            }
            Verdict::NotInstalled => fail("guarded copy not installed", FAIL_NOT_INSTALLED),
            Verdict::IntactCopyBroken => fail("intact copy broken", FAIL_INTACT_COPY),
            Verdict::FaultNotReported => {
                fail("abort not reported as an error", FAIL_FAULT_NOT_REPORTED)
            }
        }
    }
}

// --- Host stub -----------------------------------------------------
#[cfg(not(itest_aarch64))]
fn main() {}

#[cfg(not(itest_aarch64))]
#[allow(dead_code)]
fn _suppress_no_main() {}
