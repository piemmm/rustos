//! x86_64 QEMU integration test: a deliberate kernel-mode `#UD` reaches
//! the fatal-fault report and names its own vector.
//!
//! ## Why this exists
//!
//! Only vector 14 (`#PF`) had a dedicated entry; every other exception
//! routed through one vector-agnostic thunk that could say neither which
//! exception fired nor what error code the CPU pushed, and whose whole
//! body was a write to QEMU's `isa-debug-exit` port followed by a halt.
//! On real hardware that write does nothing, so a kernel-mode `#GP`,
//! `#UD`, `#DF` or machine check parked the machine with no diagnosis at
//! all (`plans/OPEN-DEFECTS.md` D83). This vertical proves the mechanism
//! that replaced it, live.
//!
//! ## What this test asserts
//!
//! 1. The **production** boot pipeline (`tairix_kernel::boot`) installs
//!    the per-vector exception stubs — the mechanism under test lives on
//!    the real boot path, not a test-only install.
//! 2. A deliberate `ud2` executed in kernel mode after
//!    `AuditEvent::BootCompleted` reaches the installed fault handler
//!    rather than parking the CPU.
//! 3. The report *names the vector*: the packed syndrome decodes to
//!    vector 6 (`#UD`, Intel SDM Vol 3A Table 6-1) with no error code and
//!    the kernel-mode privilege verdict. This is what distinguishes a
//!    vector-specific stub from "something faulted": a vector-agnostic
//!    thunk could not report `6`.
//! 4. The faulting `rip` is non-zero, so the record points at real code.
//!
//! ## How it differs from the production `tairix-kernel` binary
//!
//! Only the audit sink is replaced (to hook `BootCompleted`) and the
//! fatal-fault observer installed ahead of `boot` (to inspect the report
//! and exit QEMU). The QEMU-exit shortcut lives in this dedicated bin,
//! never behind a Cargo feature on the kernel (fail closed).

#![cfg_attr(itest_x86_64, no_std)]
#![cfg_attr(itest_x86_64, no_main)]
#![deny(missing_docs)]

// --- Freestanding test bin (`x86_64-unknown-none`) -----------------

#[cfg(itest_x86_64)]
mod kernel {
    use core::panic::PanicInfo;
    use core::sync::atomic::{AtomicU32, Ordering};

    use tairix_arch_x86_64::{fault, qemu_exit};
    use tairix_kernel::kalloc::{Heap, HEAP_BYTES};
    use tairix_kernel::{
        boot, handle_panic_via_kernel_core, FreeListAllocator, SerialSink, SERIAL_SINK,
    };
    use tairix_log::{log, Event, EventId, Field, FieldValue, Level, Sink};

    /// Invalid-opcode exception vector (Intel SDM Vol 3A Table 6-1). The
    /// `ud2` instruction is architecturally guaranteed to raise it.
    const INVALID_OPCODE_VECTOR: u8 = 6;

    /// `EventId` emitted when every boot init phase completed. Pinned by
    /// the `event_ids_are_unique` test in `kernel/core/src/audit.rs`.
    const BOOT_COMPLETED_EVENT_ID: EventId = EventId(4004);

    /// Stable audit-event ids for the QEMU transcript, clear of the ids
    /// the sibling verticals use.
    const KX_TEST_START: EventId = EventId(4344);
    const KX_TEST_PASS: EventId = EventId(4345);
    const KX_TEST_FAIL: EventId = EventId(4346);

    /// Set once the deliberate fault has been raised, so a duplicate
    /// `BootCompleted` cannot re-enter the test logic.
    static TEST_DRIVEN: AtomicU32 = AtomicU32::new(0);

    /// Static heap for the bump allocator (per the production bin).
    static mut HEAP: Heap = Heap::ZERO;

    /// Global allocator backed by [`HEAP`].
    ///
    /// SAFETY: the page-aligned `HEAP` static outlives the binary and the
    /// allocator is its only consumer.
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
                id: KX_TEST_FAIL,
                message: "x86_64 kernel-exception test: failed",
                fields: &[Field {
                    key: "stage",
                    value: FieldValue::Str(what),
                }],
            },
        );
        qemu_exit::exit_failure();
    }

    /// The fatal observer: the report the deliberate `#UD` must reach.
    ///
    /// Every field is checked against what a `#UD` taken in kernel mode
    /// must carry, so a stub that reported the wrong vector — or a
    /// vector-agnostic one that could report none — fails the test rather
    /// than passing on "a fault happened".
    extern "C" fn on_fault(syndrome: u64, faulting_addr: u64, rip: u64) -> ! {
        if TEST_DRIVEN.load(Ordering::Acquire) == 0 {
            fail("fault before the deliberate ud2 — kernel bug");
        }
        if fault::syndrome_vector(syndrome) != INVALID_OPCODE_VECTOR {
            fail("report names the wrong vector");
        }
        if fault::syndrome_error_code(syndrome) != 0 {
            fail("#UD pushes no error code, yet one was reported");
        }
        if fault::syndrome_from_user(syndrome) {
            fail("kernel-mode fault reported as taken from ring 3");
        }
        if faulting_addr != 0 {
            fail("#UD supplies no faulting address, yet one was reported");
        }
        if rip == 0 {
            fail("report carries no faulting instruction");
        }
        note(
            Level::Info,
            KX_TEST_PASS,
            "x86_64 kernel-exception test: #UD reported with its vector",
        );
        qemu_exit::exit_success();
    }

    /// Raise a kernel-mode `#UD` and never come back: the dispatcher
    /// diverges into [`on_fault`], which exits QEMU.
    fn raise_invalid_opcode() -> ! {
        note(
            Level::Info,
            KX_TEST_START,
            "x86_64 kernel-exception test: raising a deliberate kernel-mode #UD",
        );
        // SAFETY: `ud2` is the architecturally-defined always-invalid
        // opcode (Intel SDM Vol 2B). It touches no memory and raises `#UD`
        // unconditionally, which is precisely what this test provokes; the
        // exception entry under test diverges, so control never returns.
        unsafe {
            core::arch::asm!("ud2", options(nomem, nostack, preserves_flags));
        }
        fail("ud2 did not raise #UD")
    }

    /// Sink that forwards every event to the serial log (so the QEMU
    /// transcript captures the boot timeline) and, on the single
    /// [`BOOT_COMPLETED_EVENT_ID`], raises the deliberate fault.
    struct BootCompletedSink;

    impl Sink for BootCompletedSink {
        fn write_event(&self, event: &Event<'_>) {
            SerialSink::new().write_event(event);

            if event.id == BOOT_COMPLETED_EVENT_ID
                && TEST_DRIVEN
                    .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
            {
                raise_invalid_opcode();
            }
        }
    }

    static AUDIT_SINK: BootCompletedSink = BootCompletedSink;

    /// Forward to the shared bridge in `tairix_kernel`.
    #[panic_handler]
    fn tairix_kernel_exception_x86_64_panic(info: &PanicInfo<'_>) -> ! {
        handle_panic_via_kernel_core(info)
    }

    /// The symbol the arch crate's boot trampoline calls.
    #[no_mangle]
    pub extern "C" fn kernel_main(multiboot_info: u64) -> ! {
        // Claim the set-once fatal slot before `boot` publishes the
        // production reporter into it: this vertical is the observer of
        // its own deliberate fault, so it owns the machine's fatal policy
        // for this image and must be first. The production pipeline
        // installs the per-vector exception stubs itself.
        if fault::set_fault_handler(on_fault).is_err() {
            fail("set_fault_handler");
        }
        boot(
            multiboot_info,
            &ALLOCATOR,
            &SERIAL_SINK,
            &AUDIT_SINK,
            tairix_log::Level::Info,
        )
    }
}

// --- Host stub -----------------------------------------------------
#[cfg(not(itest_x86_64))]
fn main() {}
