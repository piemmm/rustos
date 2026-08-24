//! `tests/SECURITY.md` §5 `copy_from_user` hardware fault fix-up
//! (x86_64) QEMU integration test: a kernel-mode `#PF` taken inside the
//! guarded user-copy window is redirected to the copy's fix-up and
//! surfaces as an error — the CPU keeps running instead of halting.
//!
//! ## Why this exists
//!
//! The kernel's validated user-copy path (`kernel/mem::uaccess`) proves
//! every page before it moves a byte, so a mid-copy hardware fault means
//! that proof was violated underneath it. The per-port fault window
//! (`tairix_arch_x86_64::uaccess`) is the backstop that turns such a
//! fault into an error return; this vertical proves the whole mechanism
//! live: real kernel-mode `#PF` → dedicated `#PF` entry → frame-`RIP`
//! rewrite → fix-up return.
//!
//! ## What this test asserts
//!
//! 1. The **production** boot pipeline (`tairix_kernel::boot`) installs
//!    the dedicated `#PF` entry *and* arms the Arch HAL guarded-copy
//!    slot — the pairing under test lives on the real boot path, not a
//!    test-only install.
//! 2. On `AuditEvent::BootCompleted`, the shared
//!    `tairix_arch_api::uaccess::conformance` checks pass: an intact
//!    copy moves its bytes exactly; a copy reading an unmapped canonical
//!    address returns the fault error; a copy writing it returns the
//!    fault error — each taking a *real* kernel-mode `#PF` whose
//!    redirect lets execution continue to the next check.
//! 3. The fatal `fault` observer reports FAILURE: reaching it means the
//!    window redirect did not absorb the deliberate fault.
//!
//! ## How it differs from the production `tairix-kernel` binary
//!
//! Only the audit Sink is replaced (to hook `BootCompleted`) and the
//! fault observer installed (to fail loud). The QEMU-exit shortcut lives
//! in this dedicated bin, never behind a Cargo feature on the kernel
//! (fail closed).

#![cfg_attr(itest_x86_64, no_std)]
#![cfg_attr(itest_x86_64, no_main)]
#![deny(missing_docs)]

// --- Freestanding test bin (`x86_64-unknown-none`) -----------------

#[cfg(itest_x86_64)]
mod kernel {
    use core::panic::PanicInfo;
    use core::sync::atomic::{AtomicU32, Ordering};

    use tairix_arch_api::uaccess::conformance::{self, Verdict};
    use tairix_arch_x86_64::{fault, qemu_exit};
    use tairix_kernel::kalloc::{Heap, HEAP_BYTES};
    use tairix_kernel::{
        boot, handle_panic_via_kernel_core, FreeListAllocator, SerialSink, SERIAL_SINK,
    };
    use tairix_log::{log, Event, EventId, Field, Level, Sink};

    /// A page-aligned canonical virtual address the production kernel
    /// space leaves unmapped (256 GiB: far beyond the low identity
    /// window and below the higher-half kernel window), so any access to
    /// it raises a kernel-mode not-present `#PF`.
    const UNMAPPED_VA: u64 = 0x40_0000_0000;

    /// `EventId` emitted when every boot init phase completed. Pinned by
    /// the `event_ids_are_unique` test in `kernel/core/src/audit.rs`.
    const BOOT_COMPLETED_EVENT_ID: EventId = EventId(4004);

    /// Stable audit-event ids for the QEMU transcript (clear of the
    /// `4000..5000` `kernel/core` boot range collisions used by other
    /// tests).
    const UA_TEST_START: EventId = EventId(4340);
    const UA_TEST_PASS: EventId = EventId(4341);
    const UA_TEST_FAIL: EventId = EventId(4342);

    /// Set once the conformance run has been driven so a duplicate
    /// `BootCompleted` cannot re-enter the test logic.
    static TEST_DRIVEN: AtomicU32 = AtomicU32::new(0);

    /// Static heap for the bump allocator (per the production bin).
    static mut HEAP: Heap = Heap::ZERO;

    /// Global allocator backed by [`HEAP`].
    ///
    /// SAFETY: the page-aligned `HEAP` static outlives the binary and
    /// the allocator is its only consumer.
    #[global_allocator]
    static ALLOCATOR: FreeListAllocator =
        unsafe { FreeListAllocator::new(core::ptr::addr_of!(HEAP) as *mut u8, HEAP_BYTES) };

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

    /// Log a check failure and report it to QEMU. Never returns.
    fn fail(what: &'static str) -> ! {
        log(
            &SERIAL_SINK,
            &Event {
                level: Level::Error,
                id: UA_TEST_FAIL,
                message: "x86_64 uaccess-fault test: failed",
                fields: &[Field {
                    key: "stage",
                    value: tairix_log::FieldValue::Str(what),
                }],
            },
        );
        qemu_exit::exit_failure();
    }

    /// The fatal `#PF` observer: reaching it means a fault escaped the
    /// guarded-copy window redirect (or something else faulted) — a
    /// closed failure either way.
    extern "C" fn on_fault(_error_code: u64, _faulting_addr: u64, _rip: u64) -> ! {
        fail("fault escaped the copy window redirect")
    }

    /// Drive the shared checks: positive control, then a real `#PF` on
    /// the read side and the write side of the window. Never returns.
    fn run_checks() -> ! {
        note(
            Level::Info,
            UA_TEST_START,
            "x86_64 uaccess-fault test: faulting inside the guarded copy window",
        );
        let mut scratch = [0u8; 64];
        // SAFETY: `UNMAPPED_VA` has no translation in the boot address
        // space, so the page is genuinely unmapped and the deliberate
        // page faults are absorbed by the guarded copy window the boot
        // pipeline armed before `BOOT_COMPLETED` fired.
        let verdict = unsafe { conformance::run(UNMAPPED_VA as *mut u8, &mut scratch) };
        match verdict {
            Verdict::Pass => {
                note(
                    Level::Info,
                    UA_TEST_PASS,
                    "x86_64 uaccess-fault test: in-window faults surfaced as errors",
                );
                qemu_exit::exit_success();
            }
            Verdict::NotInstalled => fail("guarded copy not installed"),
            Verdict::IntactCopyBroken => fail("intact copy broken"),
            Verdict::FaultNotReported => fail("fault not reported as an error"),
        }
    }

    /// Sink that forwards every event to the serial log (so the QEMU
    /// transcript captures the boot timeline) and, on the single
    /// [`BOOT_COMPLETED_EVENT_ID`], drives [`run_checks`].
    struct BootCompletedSink;

    impl Sink for BootCompletedSink {
        fn write_event(&self, event: &Event<'_>) {
            SerialSink::new().write_event(event);

            if event.id == BOOT_COMPLETED_EVENT_ID
                && TEST_DRIVEN
                    .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
            {
                run_checks();
            }
        }
    }

    static AUDIT_SINK: BootCompletedSink = BootCompletedSink;

    /// Forward to the shared bridge in `tairix_kernel`.
    #[panic_handler]
    fn tairix_uaccess_fault_x86_64_panic(info: &PanicInfo<'_>) -> ! {
        handle_panic_via_kernel_core(info)
    }

    /// The symbol the arch crate's boot trampoline calls.
    #[no_mangle]
    pub extern "C" fn kernel_main(multiboot_info: u64) -> ! {
        // Install the fail-loud fault observer before any deliberate
        // fault can fire; the production pipeline installs the dedicated
        // `#PF` entry and arms the guarded-copy slot itself.
        if fault::set_fault_handler(on_fault).is_err() {
            fail("set_fault_handler");
        }
        boot(
            multiboot_info,
            &SERIAL_SINK,
            &AUDIT_SINK,
            tairix_log::Level::Info,
        )
    }
}

// --- Host stub -----------------------------------------------------
#[cfg(not(itest_x86_64))]
fn main() {}
