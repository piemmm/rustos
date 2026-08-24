//! CCOMPAT stage CC2 QEMU integration test: a full `lib/abi-sys` syscall
//! round-trip on the freestanding `x86_64-unknown-none` target.
//!
//! ## What this test asserts
//!
//! The production `tairix-kernel` boot pipeline runs through
//! `tairix_kernel::boot` until `AuditEvent::BootCompleted`
//! (`EventId(4004)`) fires. By that point the boot pipeline has already
//! installed the production syscall dispatch callback and enabled the
//! `syscall` instruction on the BSP (`init_local_syscalls`). The audit
//! Sink that observes `BootCompleted` then:
//!
//! 1. **Overrides** the dispatch callback with `record_and_exit` via
//!    `syscall_entry::set_dispatch_callback` (the production callback
//!    fail-closes without a user caller context — this test has none).
//! 2. Calls the `lib/abi-sys` stub `tairix_abi_sys::sys_cap_query`
//!    (exported to C as `tairix_sys_cap_query`) with a known capability id.
//!    That stub marshals the syscall number and arguments into the
//!    `rax`/`rdi`/… registers and executes the real `syscall`
//!    instruction (`lib/abi-sys/src/trap.rs`).
//!
//! The CPU's `syscall` enters the kernel's `IA32_LSTAR` stub
//! (`kernel/arch/x86_64/src/syscall_entry.rs`), which `swapgs`es, switches
//! to the per-CPU kernel stack, rebuilds the canonical
//! `[u64; SYSCALL_MAX_ARGS]` argument array, and calls the installed
//! dispatch callback. `record_and_exit` therefore observes the
//! register marshalling end-to-end: it asserts the dispatched number is
//! `SyscallNumber::CAP_QUERY` and that argument 0 is the capability id the
//! stub was handed (with the remaining arguments zero), then flips
//! `qemu_exit::exit_success`. Any mismatch — wrong number, wrong
//! argument, or the `syscall` returning to the caller at all — flips
//! `qemu_exit::exit_failure`.
//!
//! ## Why the callback never returns
//!
//! Issuing `syscall` from ring 0 reaches the kernel entry stub
//! identically to a ring-3 call, but the stub's `sysretq` would drop the
//! CPU to ring 3. There is no user context to return to in this test, so
//! `record_and_exit` does its assertion and exits through the QEMU
//! `isa-debug-exit` device rather than returning. The `syscall`
//! instruction masks `RFLAGS.IF` via `IA32_FMASK`, so the callback runs
//! with interrupts disabled — no timer tick can perturb the assertion.
//!
//! ## How it differs from `tairix-test-syscall-dispatch-qemu`
//!
//! That test drives `Dispatcher::dispatch` directly and never executes a
//! trap instruction. This test issues the `abi-sys` stub, so the
//! `syscall` instruction and the kernel's entry stub are exercised
//! together — the missing CC2 deliverable in `plans/CCOMPAT.md`.
//!
//! ## `test-hooks` Cargo feature
//!
//! The test body only compiles under `#[cfg(feature = "test-hooks")]`.
//! The feature is on by default for this crate; release builds that
//! enable it are rejected by the `compile_error!` guard below
//! (no hacks; — fail closed), mirroring
//! `tairix-test-syscall-dispatch-qemu`.

#![cfg_attr(itest_x86_64, no_std)]
#![cfg_attr(itest_x86_64, no_main)]
#![deny(missing_docs)]

// Test affordances must never reach a release binary.
// `test-hooks` is on by default for this crate; a release build that
// re-enables it is a configuration error, so we fail the build outright,
// exactly as `tairix-test-syscall-dispatch-qemu` does.
#[cfg(all(feature = "test-hooks", not(debug_assertions)))]
compile_error!(
    "tairix-test-abi-sys-syscall-qemu: the `test-hooks` Cargo feature is a \
     debug-only test affordance and must not be enabled in release builds. \
     See AGENTS.md §1 (no hacks) and §5.4.5 (fail closed)."
);

// --- Freestanding test bin (`x86_64-unknown-none`) -----------------

#[cfg(all(itest_x86_64, feature = "test-hooks"))]
mod kernel {
    use core::panic::PanicInfo;
    use core::sync::atomic::{AtomicU32, Ordering};

    use tairix_abi::{CapabilityId, SyscallNumber, SYSCALL_MAX_ARGS};
    use tairix_arch_x86_64::qemu_exit;
    use tairix_arch_x86_64::syscall_entry;
    use tairix_kernel::kalloc::{Heap, HEAP_BYTES};
    use tairix_kernel::{
        boot, handle_panic_via_kernel_core, FreeListAllocator, SerialSink, SERIAL_SINK,
    };
    use tairix_log::{Event, EventId, Sink};

    // --- Bump-allocator-backed `#[global_allocator]` ---------------
    //
    // Mirrors the production `tairix-kernel` bin and the
    // `tairix-test-syscall-dispatch-qemu` test bin: `#[global_allocator]`
    // is a per-binary attribute, so each freestanding bin declares its
    // own over the shared `kalloc` heap.

    /// Static heap for the bump allocator.
    ///
    /// `static mut` because the bump allocator hands out disjoint slices
    /// via an `AtomicUsize` cursor; the storage itself is otherwise
    /// immutable from any other call site.
    static mut HEAP: Heap = Heap::ZERO;

    /// Global allocator backed by [`HEAP`].
    ///
    /// SAFETY: as for the production bin's `ALLOCATOR` — the page-aligned
    /// `HEAP` static outlives the binary and the allocator is its only
    /// consumer.
    #[global_allocator]
    static ALLOCATOR: FreeListAllocator =
        unsafe { FreeListAllocator::new(core::ptr::addr_of!(HEAP) as *mut u8, HEAP_BYTES) };

    // --- Stable audit identifiers --------------------------------

    /// `EventId` emitted by `kernel_core::kernel_main` when every init
    /// phase completed successfully. Pinned by the `event_ids_are_unique`
    /// test in `kernel/core/src/audit.rs`.
    const BOOT_COMPLETED_EVENT_ID: EventId = EventId(4004);

    /// The capability id [`run_round_trip`] passes to `tairix_sys_cap_query`
    /// and [`record_and_exit`] expects to see marshalled into argument 0.
    /// Any well-known [`CapabilityId`] works — the test asserts the
    /// stub's *marshalling*, not the kernel's grant decision (the dispatch
    /// callback is intercepted before the kernel evaluates the query).
    const EXPECTED_CAP: CapabilityId = CapabilityId::TIME_SET;

    /// Set once the round-trip has been driven so a stray duplicate
    /// `BootCompleted` (which the audit catalogue disallows but the
    /// pipeline cannot statically prove) never re-enters the test logic.
    /// — fail closed.
    static TEST_DRIVEN: AtomicU32 = AtomicU32::new(0);

    // --- Dispatch callback ---------------------------------------

    /// The syscall dispatch callback installed for the round-trip.
    ///
    /// Reached from the kernel's `IA32_LSTAR` entry stub after the
    /// `abi-sys` stub executed `syscall`. It asserts the marshalled
    /// `(number, args)` match what `tairix_sys_cap_query(EXPECTED_CAP)`
    /// should have placed in the registers, then exits QEMU. It never
    /// returns to the caller (see the module docs): a `sysretq` here would
    /// drop the CPU to ring 3 with no user context.
    ///
    /// The signature matches `syscall_entry::SyscallDispatchFn`.
    extern "C" fn record_and_exit(number: u64, args_ptr: *const [u64; SYSCALL_MAX_ARGS]) -> u64 {
        // SAFETY: the entry stub built the `[u64; SYSCALL_MAX_ARGS]` array
        // on the kernel stack and passes a pointer to it that is valid for
        // the duration of this call (`syscall_entry` contract).
        let args = unsafe { *args_ptr };

        let expected_number = u64::from(SyscallNumber::CAP_QUERY.as_u16());
        let expected_arg0 = u64::from(EXPECTED_CAP.as_u16());
        let args_ok = args[0] == expected_arg0 && args[1..] == [0, 0, 0, 0, 0];

        if number == expected_number && args_ok {
            qemu_exit::exit_success();
        }
        qemu_exit::exit_failure();
    }

    // --- Audit observer Sink -------------------------------------

    /// Outer audit sink installed via [`tairix_kernel::boot`].
    ///
    /// Replays every event through the serial sink (so the QEMU serial
    /// transcript captures the boot timeline) and, on observing
    /// [`BOOT_COMPLETED_EVENT_ID`] exactly once, drives [`run_round_trip`].
    struct BootCompletedSink;

    impl Sink for BootCompletedSink {
        fn write_event(&self, event: &Event<'_>) {
            SerialSink::new().write_event(event);

            if event.id == BOOT_COMPLETED_EVENT_ID
                && TEST_DRIVEN
                    .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
            {
                run_round_trip();
            }
        }
    }

    static AUDIT_SINK: BootCompletedSink = BootCompletedSink;

    /// Override the dispatch callback and issue the `abi-sys` stub.
    ///
    /// Never returns: control diverts into [`record_and_exit`] from inside
    /// the `syscall`, which exits QEMU. Reaching the trailing
    /// `exit_failure` means the `syscall` returned to its caller — the
    /// trap was not delivered — which is itself a failure.
    fn run_round_trip() -> ! {
        syscall_entry::set_dispatch_callback(record_and_exit);
        let _ = tairix_abi_sys::sys_cap_query(EXPECTED_CAP.as_u16());
        qemu_exit::exit_failure();
    }

    // --- Panic handler --------------------------------------------

    /// Forward to the shared bridge in `tairix_kernel::x86_64::panic_ctx`.
    #[panic_handler]
    fn tairix_test_abi_sys_syscall_qemu_panic(info: &PanicInfo<'_>) -> ! {
        handle_panic_via_kernel_core(info)
    }

    // --- Entry point ---------------------------------------------

    /// The symbol the arch crate's boot trampoline calls.
    #[no_mangle]
    pub extern "C" fn kernel_main(multiboot_info: u64) -> ! {
        boot(
            multiboot_info,
            &SERIAL_SINK,
            &AUDIT_SINK,
            tairix_log::Level::Info,
        )
    }
}

// --- Stub when the test-hooks feature is off ----------------------
//
// The test body only compiles when `feature = "test-hooks"` is on.
// Disabling it leaves the bin as a no-op so a layout sanity check
// (`cargo build --no-default-features -p tairix-test-abi-sys-syscall-qemu`)
// still builds (a disabled test must compile cleanly).
#[cfg(all(itest_x86_64, not(feature = "test-hooks")))]
#[no_mangle]
pub extern "C" fn kernel_main(_multiboot_info: u64) -> ! {
    loop {
        // SAFETY: `cli; hlt` is a well-defined parked-CPU sequence on
        // x86_64. Looping defends against spurious
        // wake-ups.
        unsafe {
            core::arch::asm!("cli; hlt", options(nomem, nostack, preserves_flags));
        }
    }
}

#[cfg(all(itest_x86_64, not(feature = "test-hooks")))]
#[panic_handler]
fn tairix_test_abi_sys_syscall_qemu_panic_stub(_info: &core::panic::PanicInfo<'_>) -> ! {
    loop {
        // SAFETY: same as above.
        unsafe {
            core::arch::asm!("cli; hlt", options(nomem, nostack, preserves_flags));
        }
    }
}

// --- Host stub -----------------------------------------------------
#[cfg(not(itest_x86_64))]
fn main() {}
