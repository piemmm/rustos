//! Stage 3a (c7-bin) QEMU integration test: boot the kernel pipeline
//! to `AuditEvent::BootCompleted` and report success to QEMU.
//!
//! ## What this test asserts
//!
//! `kernel_core::kernel_main` emits `AuditEvent::BootCompleted`
//! (`EventId(4004)`) once every init phase has succeeded. The
//! integration test binary observes the audit sink, and on the
//! boot-completed record it exercises the growable kernel heap and
//! requires the production boot to have installed the fatal fault
//! handler — with that slot empty the dedicated `#PF` entry keeps its
//! fail-closed default (park the CPU with interrupts masked, print
//! nothing), so the machine dies mutely and deadlocks every peer
//! waiting on a lock the parked CPU still holds
//! (`plans/OPEN-DEFECTS.md` D13) — before flipping QEMU's
//! `isa-debug-exit` device to success (`qemu_exit::exit_success`). The
//! host-side `tools/qemu::Runner` then registers the test as
//! `tairix_qemu::Outcome::Pass`.
//!
//! ## How it differs from the production `tairix-kernel` binary
//!
//! The binary re-uses the entire boot pipeline from
//! `tairix_kernel::boot`; only the audit Sink is replaced. Splitting
//! the audit-observer behaviour into a separate bin (instead of
//! gating it behind a Cargo feature on `tairix-kernel`) prevents
//! feature-unification under `cargo build --workspace` from ever
//! leaking the QEMU-exit behaviour into the production kernel image
//! (fail closed; the harness never decides what
//! the kernel does next).

#![cfg_attr(itest_x86_64, no_std)]
#![cfg_attr(itest_x86_64, no_main)]
#![deny(missing_docs)]

// --- Freestanding test bin (`x86_64-unknown-none`) -----------------

#[cfg(itest_x86_64)]
mod kernel {
    use core::panic::PanicInfo;

    use tairix_arch_x86_64::qemu_exit;
    use tairix_kernel::kalloc::{Heap, HEAP_BYTES};
    use tairix_kernel::{
        boot, handle_panic_via_kernel_core, FreeListAllocator, SerialSink, SERIAL_SINK,
    };
    use tairix_log::{Event, EventId, Sink};
    use tairix_test_kheap_growth as kheap_growth;

    // --- Bump-allocator-backed `#[global_allocator]` ---------------
    //
    // Identical to the production `tairix-kernel` bin's allocator
    // declaration. We re-declare it here (rather than re-exporting
    // the production bin's) because `#[global_allocator]` is a
    // per-binary attribute — see `kernel/tairix-kernel/Cargo.toml`'s
    // top-level rationale comment.

    /// Static heap for the bump allocator.
    ///
    /// `static mut` because the bump allocator hands out disjoint
    /// slices via an `AtomicUsize` cursor; the storage itself is
    /// otherwise immutable from any other call site.
    static mut HEAP: Heap = Heap::ZERO;

    /// Global allocator backed by [`HEAP`].
    ///
    /// SAFETY: as for the production bin's `ALLOCATOR` — the
    /// page-aligned `HEAP` static outlives the binary, the allocator
    /// is the only consumer.
    #[global_allocator]
    static ALLOCATOR: FreeListAllocator =
        unsafe { FreeListAllocator::new(core::ptr::addr_of!(HEAP) as *mut u8, HEAP_BYTES) };

    // --- Audit observer Sink --------------------------------------

    /// `EventId` emitted by `kernel_core::kernel_main` when every init
    /// phase completed successfully. Pinned by the
    /// `event_ids_are_unique` test in
    /// `kernel/core/src/audit.rs`; if the catalogue ever renumbers,
    /// the build of `tairix_kernel_core` fails the assertion and the
    /// integration test stops re-shipping a stale literal here.
    const BOOT_COMPLETED_EVENT_ID: EventId = EventId(4004);

    /// `EventId` this vertical's own FAIL diagnostics carry, so the
    /// transcript names *which* post-boot check refused. Outside the
    /// `kernel/core` 4000-range boot ids above.
    const BOOT_TEST_FAIL_EVENT_ID: EventId = EventId(4310);

    /// Sink that forwards every event to [`SERIAL_SINK`] (so the
    /// serial log captured by `tools/qemu::Runner` records the full
    /// boot timeline) and, on observing [`BOOT_COMPLETED_EVENT_ID`],
    /// flips QEMU's `isa-debug-exit` device to success.
    ///
    /// `qemu_exit::exit_success` does not return, so the audit sink
    /// is effectively the final caller of the kernel's normal exit
    /// path. The trailing `kernel_core::KernelArch::halt` in
    /// `kernel_main` is preserved for production builds — only the
    /// integration-test sink short-circuits ahead of it.
    struct BootCompletedExitSink;

    impl Sink for BootCompletedExitSink {
        fn write_event(&self, event: &Event<'_>) {
            // Always replay through the serial sink so the QEMU
            // serial transcript captures the full boot timeline.
            SerialSink::new().write_event(event);

            if event.id == BOOT_COMPLETED_EVENT_ID {
                // Boot proved the remap window exists; this proves the heap
                // can grow a region into it and dereference every page.
                if kheap_growth::verify(&ALLOCATOR, &SERIAL_SINK).is_err() {
                    qemu_exit::exit_failure();
                }
                // A booted kernel must be able to say why it died. With the
                // fatal-fault slot empty the dedicated `#PF` entry keeps its
                // fail-closed default — park the CPU with interrupts masked,
                // print nothing — so the machine goes silent, and deadlocks
                // every peer waiting on a lock the parked CPU still holds.
                if tairix_arch_x86_64::fault::fault_handler().is_none() {
                    SerialSink::new().write_event(&Event {
                        level: tairix_log::Level::Error,
                        id: BOOT_TEST_FAIL_EVENT_ID,
                        message: "boot left the fatal-fault slot empty",
                        fields: &[],
                    });
                    qemu_exit::exit_failure();
                }
                qemu_exit::exit_success();
            }
        }
    }

    static AUDIT_SINK: BootCompletedExitSink = BootCompletedExitSink;

    // --- Panic handler --------------------------------------------

    /// Forward to the shared bridge in `tairix_kernel::x86_64::panic_ctx`.
    ///
    /// Note: the bridge logs through `SERIAL_SINK`, not through our
    /// `AUDIT_SINK`, so a panic before `BootCompleted` does *not*
    /// trip the QEMU-exit short-circuit — it falls through to
    /// `kernel_arch::halt`, the boot test times out, and the
    /// harness reports `Outcome::Timeout`. This is the documented
    /// fail-loud behaviour for (no flaky tests).
    #[panic_handler]
    fn tairix_kernel_arch_boot_panic(info: &PanicInfo<'_>) -> ! {
        handle_panic_via_kernel_core(info)
    }

    // --- Entry point ---------------------------------------------

    /// The symbol the arch crate's boot trampoline calls.
    ///
    /// Forwards to [`tairix_kernel::boot`] with the production COM1
    /// log sink and our audit-observer sink.
    #[no_mangle]
    pub extern "C" fn kernel_main(multiboot_info: u64) -> ! {
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
