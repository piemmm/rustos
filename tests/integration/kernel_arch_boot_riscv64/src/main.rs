//! Stage 4.D Item 4 QEMU integration test: boot the riscv64 `virt`
//! pipeline to `AuditEvent::BootCompleted` and report success to QEMU.
//!
//! ## What this test asserts
//!
//! `kernel_core::kernel_main` emits `AuditEvent::BootCompleted`
//! (`EventId(4004)`) once every init phase has succeeded. This binary
//! observes the audit sink and, on the boot-completed record, exercises
//! the growable kernel heap, requires the production boot to have
//! installed the fatal fault handler — with that slot empty an S-mode
//! trap parks the hart with interrupts masked and prints nothing, so the
//! machine dies mutely and deadlocks every peer waiting on a lock the
//! parked hart still holds (`plans/OPEN-DEFECTS.md` D13) — and then
//! writes the `SiFive` Test PASS finisher (`qemu_exit::exit_success`).
//! The host-side `tools/qemu::Runner` then registers `Outcome::Pass`.
//!
//! ## How it differs from a production kernel
//!
//! It re-uses the entire `tairix-arch-riscv64` boot pipeline
//! (`boot`); only the audit Sink is replaced. Splitting the
//! audit-observer behaviour into a separate bin (instead of a Cargo
//! feature on the arch crate) prevents feature unification from
//! leaking the QEMU-exit shortcut into any production build
//! (fail closed; the harness never decides what
//! the kernel does next).

#![cfg_attr(itest_riscv64, no_std)]
#![cfg_attr(itest_riscv64, no_main)]
#![deny(missing_docs)]

// --- Freestanding test bin (`riscv64gc-unknown-none-elf`) ----------

#[cfg(itest_riscv64)]
mod kernel {
    use core::num::NonZeroU16;
    use core::panic::PanicInfo;

    use tairix_arch_riscv64::{handle_panic_via_serial, qemu_exit, SerialSink, SERIAL_SINK};
    use tairix_itest_finisher::fail_point;
    use tairix_kalloc::{FreeListAllocator, Heap, HEAP_BYTES};
    use tairix_log::{Event, EventId, Sink};
    use tairix_test_kheap_growth as kheap_growth;
    use tairix_test_riscv64_boot::boot;

    /// Static boot heap.
    ///
    /// Placed in the linker's dedicated `.heap` (NOLOAD) section so the
    /// boot trampoline does not zero its 64 MiB (the bump allocator
    /// does not require zeroed backing) and the boot pipeline excludes
    /// it from the usable physical-memory map. `static mut` because the
    /// bump allocator hands out disjoint slices via an atomic cursor;
    /// the storage is otherwise never aliased.
    #[link_section = ".heap"]
    static mut HEAP: Heap = Heap::ZERO;

    /// Global allocator backed by [`HEAP`].
    ///
    /// SAFETY: the page-aligned `HEAP` static outlives the binary and
    /// the allocator is its only consumer.
    #[global_allocator]
    static ALLOCATOR: FreeListAllocator =
        unsafe { FreeListAllocator::new(core::ptr::addr_of!(HEAP) as *mut u8, HEAP_BYTES) };

    /// `EventId` emitted by `kernel_core::kernel_main` once every init
    /// phase completed. Pinned by the `event_ids_are_unique` test in
    /// `kernel/core/src/audit.rs`.
    const BOOT_COMPLETED_EVENT_ID: EventId = EventId(4004);

    /// Sink that replays every event through [`SERIAL_SINK`] and, on
    /// [`BOOT_COMPLETED_EVENT_ID`], reports PASS to QEMU.
    struct BootCompletedExitSink;

    impl Sink for BootCompletedExitSink {
        fn write_event(&self, event: &Event<'_>) {
            // Replay through the serial sink so the QEMU transcript
            // records the full boot timeline.
            SerialSink::new().write_event(event);
            if event.id == BOOT_COMPLETED_EVENT_ID {
                // Boot proved the remap window exists; this proves the heap
                // can grow a region into it and dereference every page.
                if kheap_growth::verify(&ALLOCATOR, &SERIAL_SINK).is_err() {
                    qemu_exit::exit_failure(FAIL_KHEAP_GROWTH);
                }
                // A booted kernel must be able to say why it died. With the
                // fatal-fault slot empty an S-mode trap parks the hart with
                // interrupts masked and prints nothing, so the machine goes
                // silent — and deadlocks every peer waiting on a lock the
                // parked hart still holds.
                if tairix_arch_riscv64::fault::fault_handler().is_none() {
                    qemu_exit::exit_failure(FAIL_NO_FAULT_HANDLER);
                }
                qemu_exit::exit_success();
            }
        }
    }

    /// Failure finisher code for a growth-path fault.
    const FAIL_KHEAP_GROWTH: NonZeroU16 = fail_point!(1);

    /// Failure finisher code for a boot that left the fatal-fault slot
    /// empty, so a kernel-mode trap would park the hart mutely.
    const FAIL_NO_FAULT_HANDLER: NonZeroU16 = fail_point!(2);

    static AUDIT_SINK: BootCompletedExitSink = BootCompletedExitSink;

    /// Forward to the shared riscv64 panic bridge. A panic before
    /// `BootCompleted` parks the hart, the run times out, and the
    /// harness reports `Outcome::Timeout` — the documented fail-loud
    /// behaviour.
    #[panic_handler]
    fn tairix_kernel_arch_boot_riscv64_panic(info: &PanicInfo<'_>) -> ! {
        handle_panic_via_serial(info)
    }

    /// Boot entry point — the symbol the arch crate's `boot.s`
    /// trampoline calls (via `tairix_arch_riscv64_main`). Forwards the
    /// SBI hand-off values to the production boot pipeline with the
    /// audit-observer sink in place.
    #[no_mangle]
    pub extern "C" fn kernel_main(hartid: u64, dtb: u64) -> ! {
        boot(
            hartid,
            dtb,
            &ALLOCATOR,
            &SERIAL_SINK,
            &AUDIT_SINK,
            tairix_log::Level::Info,
        )
    }
}

// --- Host stub -----------------------------------------------------
#[cfg(not(itest_riscv64))]
fn main() {}
