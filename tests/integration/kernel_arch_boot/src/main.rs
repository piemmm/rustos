//! Stage 3a (c7-bin) QEMU integration test: boot the kernel pipeline
//! to `AuditEvent::BootCompleted` and report success to QEMU.
//!
//! ## What this test asserts
//!
//! `kernel_core::kernel_main` emits `AuditEvent::BootCompleted`
//! (`EventId(4004)`) once every init phase has succeeded. The
//! integration test binary observes the audit sink, and as soon as
//! the boot-completed record fires it flips QEMU's `isa-debug-exit`
//! device to success (`qemu_exit::exit_success`). The host-side
//! `tools/qemu::Runner` then registers the test as
//! `rustos_qemu::Outcome::Pass`.
//!
//! ## How it differs from the production `rustos-kernel` binary
//!
//! The binary re-uses the entire boot pipeline from
//! `rustos_kernel::boot`; only the audit Sink is replaced. Splitting
//! the audit-observer behaviour into a separate bin (instead of
//! gating it behind a Cargo feature on `rustos-kernel`) prevents
//! feature-unification under `cargo build --workspace` from ever
//! leaking the QEMU-exit behaviour into the production kernel image
//! (`AGENTS.md` §5.4.5 — fail closed; the harness never decides what
//! the kernel does next).

#![cfg_attr(all(target_arch = "x86_64", target_os = "none"), no_std)]
#![cfg_attr(all(target_arch = "x86_64", target_os = "none"), no_main)]
#![deny(missing_docs)]

// --- Freestanding test bin (`x86_64-unknown-none`) -----------------

#[cfg(all(target_arch = "x86_64", target_os = "none"))]
mod kernel {
    use core::panic::PanicInfo;

    use rustos_arch_x86_64::qemu_exit;
    use rustos_kernel::bumpalloc::{Heap, HEAP_BYTES};
    use rustos_kernel::{
        boot, handle_panic_via_kernel_core, BumpAllocator, SerialSink, SERIAL_SINK,
    };
    use rustos_log::{Event, EventId, Sink};

    // --- Bump-allocator-backed `#[global_allocator]` ---------------
    //
    // Identical to the production `rustos-kernel` bin's allocator
    // declaration. We re-declare it here (rather than re-exporting
    // the production bin's) because `#[global_allocator]` is a
    // per-binary attribute — see `kernel/rustos-kernel/Cargo.toml`'s
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
    static ALLOCATOR: BumpAllocator =
        unsafe { BumpAllocator::new(core::ptr::addr_of!(HEAP) as *mut u8, HEAP_BYTES) };

    // --- Audit observer Sink --------------------------------------

    /// EventId emitted by `kernel_core::kernel_main` when every init
    /// phase completed successfully. Pinned by the
    /// `event_ids_are_unique` test in
    /// `kernel/core/src/audit.rs`; if the catalogue ever renumbers,
    /// the build of `rustos_kernel_core` fails the assertion and the
    /// integration test stops re-shipping a stale literal here.
    const BOOT_COMPLETED_EVENT_ID: EventId = EventId(4004);

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
                qemu_exit::exit_success();
            }
        }
    }

    static AUDIT_SINK: BootCompletedExitSink = BootCompletedExitSink;

    // --- Panic handler --------------------------------------------

    /// Forward to the shared bridge in `rustos_kernel::panic_ctx`.
    ///
    /// Note: the bridge logs through `SERIAL_SINK`, not through our
    /// `AUDIT_SINK`, so a panic before `BootCompleted` does *not*
    /// trip the QEMU-exit short-circuit — it falls through to
    /// `kernel_arch::halt`, the boot test times out, and the
    /// harness reports `Outcome::Timeout`. This is the documented
    /// fail-loud behaviour for `AGENTS.md` §7 (no flaky tests).
    #[panic_handler]
    fn rustos_kernel_arch_boot_panic(info: &PanicInfo<'_>) -> ! {
        handle_panic_via_kernel_core(info)
    }

    // --- Entry point ---------------------------------------------

    /// The symbol the arch crate's boot trampoline calls.
    ///
    /// Forwards to [`rustos_kernel::boot`] with the production COM1
    /// log sink and our audit-observer sink.
    #[no_mangle]
    pub extern "C" fn kernel_main(multiboot_info: u64) -> ! {
        boot(multiboot_info, &SERIAL_SINK, &AUDIT_SINK)
    }
}

// --- Host stub -----------------------------------------------------
#[cfg(not(all(target_arch = "x86_64", target_os = "none")))]
fn main() {}

#[cfg(not(all(target_arch = "x86_64", target_os = "none")))]
#[allow(dead_code)]
fn _suppress_no_main() {}
