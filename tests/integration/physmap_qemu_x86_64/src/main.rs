//! x86_64 direct-physical-map QEMU integration test: the kernel reaches
//! **every** usable byte of a guest that has more RAM than the boot
//! trampoline's own identity window.
//!
//! ## Why this exists
//!
//! The kernel reaches a frame by pointer through the port's direct physical
//! map — the process-image write, the shared-region zero-on-free scrub, the
//! remap window's record store, the kernel heap's slab page supply. That map
//! used to be a fixed window sized before the firmware memory map was read,
//! so on a machine with more RAM than the window every frame above it
//! translated to nothing and its consumer failed closed while gigabytes sat
//! free (`plans/OPEN-DEFECTS.md` D55). Nothing caught it, because every
//! other x86_64 guest in the matrix is small enough to fit the window.
//!
//! ## What this test asserts
//!
//! The guest is given more RAM than the trampoline's window, so the firmware
//! map reports usable RAM above it. Then:
//!
//! 1. The boot path widened the window past the trampoline's own — proof it
//!    sized it from the discovered map rather than a build-time constant.
//! 2. The early-boot RAM self-test left **no** usable byte unreachable: each
//!    one was written and read back through the direct map. A frame the map
//!    does not cover is left untested and counted, so a window that stopped
//!    short shows up here rather than as a silent skip.
//!
//! Only when both hold does `BootCompleted` report success to QEMU.
//!
//! ## How it differs from the production `tairix-kernel` binary
//!
//! It reuses the whole boot pipeline from `tairix_kernel::boot`; only the
//! sink is replaced. Splitting the observer into its own bin (rather than
//! gating it behind a Cargo feature on `tairix-kernel`) keeps feature
//! unification under `cargo build --workspace` from ever leaking the
//! QEMU-exit behaviour into a real kernel image.

#![cfg_attr(itest_x86_64, no_std)]
#![cfg_attr(itest_x86_64, no_main)]
#![deny(missing_docs)]

// --- Freestanding test bin (`x86_64-unknown-none`) -----------------

#[cfg(itest_x86_64)]
mod kernel {
    use core::panic::PanicInfo;
    use core::sync::atomic::{AtomicBool, Ordering};

    use tairix_arch_x86_64::paging::BOOT_IDENTITY_GIB;
    use tairix_arch_x86_64::qemu_exit;
    use tairix_kernel::kalloc::{Heap, HEAP_BYTES};
    use tairix_kernel::x86_64::boot::KERNEL_BOOT_IDENTITY_WINDOW;
    use tairix_kernel::{boot, handle_panic_via_kernel_core, FreeListAllocator, SerialSink};
    use tairix_log::{Event, EventId, FieldValue, Sink};

    // --- `#[global_allocator]`, as the production bin declares it -----
    //
    // Re-declared rather than re-exported because `#[global_allocator]` is a
    // per-binary attribute (see `kernel/tairix-kernel/Cargo.toml`).

    /// Static heap for the bump allocator.
    static mut HEAP: Heap = Heap::ZERO;

    /// Global allocator backed by [`HEAP`].
    ///
    /// SAFETY: as for the production bin's `ALLOCATOR` — the page-aligned
    /// `HEAP` static outlives the binary and the allocator is its only
    /// consumer.
    #[global_allocator]
    static ALLOCATOR: FreeListAllocator =
        unsafe { FreeListAllocator::new(core::ptr::addr_of!(HEAP) as *mut u8, HEAP_BYTES) };

    // --- Observer ----------------------------------------------------

    /// `kernel_core::AuditEvent::RamSelfTest`, carrying the bytes the
    /// self-test verified and the usable bytes the direct map could not
    /// reach. Pinned by the `event_ids_are_unique` test in
    /// `kernel/core/src/audit.rs`.
    const RAM_SELF_TEST_EVENT_ID: EventId = EventId(4005);

    /// `kernel_core::AuditEvent::BootCompleted`: every init phase succeeded.
    const BOOT_COMPLETED_EVENT_ID: EventId = EventId(4004);

    /// Set when the boot path reported an identity window wider than the
    /// trampoline's own — the guest's RAM forced a widening.
    static WINDOW_WIDENED: AtomicBool = AtomicBool::new(false);

    /// Set when the RAM self-test reported verifying every usable byte.
    static RAM_FULLY_REACHED: AtomicBool = AtomicBool::new(false);

    /// Read an unsigned field of `event` by key.
    fn field_u64(event: &Event<'_>, key: &str) -> Option<u64> {
        event.fields.iter().find_map(|field| match field.value {
            FieldValue::UnsignedInt(value) if field.key == key => Some(value),
            _ => None,
        })
    }

    /// Forwards every record to the serial transcript and grades the two
    /// records this vertical turns on, exiting the moment one of them fails
    /// so the transcript ends at the check that broke rather than at a
    /// timeout.
    struct PhysMapObserver;

    impl Sink for PhysMapObserver {
        fn write_event(&self, event: &Event<'_>) {
            SerialSink::new().write_event(event);

            if event.id == KERNEL_BOOT_IDENTITY_WINDOW {
                match field_u64(event, "gigabytes") {
                    // The guest is sized so its RAM tops the trampoline's
                    // window: a window that did not widen means the boot path
                    // never read the discovered map, which is the defect.
                    Some(gib) if gib > BOOT_IDENTITY_GIB as u64 => {
                        WINDOW_WIDENED.store(true, Ordering::Release);
                    }
                    _ => qemu_exit::exit_failure(),
                }
            }

            if event.id == RAM_SELF_TEST_EVENT_ID {
                match (
                    field_u64(event, "verified_bytes"),
                    field_u64(event, "unreachable_bytes"),
                ) {
                    // Every usable byte was written and read back through the
                    // direct map. A window that stopped short leaves the RAM
                    // above it unreachable, which is what shows up here.
                    (Some(verified), Some(0)) if verified != 0 => {
                        RAM_FULLY_REACHED.store(true, Ordering::Release);
                    }
                    _ => qemu_exit::exit_failure(),
                }
            }

            if event.id == BOOT_COMPLETED_EVENT_ID {
                if WINDOW_WIDENED.load(Ordering::Acquire)
                    && RAM_FULLY_REACHED.load(Ordering::Acquire)
                {
                    qemu_exit::exit_success();
                }
                // Booting without either record having been graded means the
                // step this vertical exists to watch never ran.
                qemu_exit::exit_failure();
            }
        }
    }

    static OBSERVER: PhysMapObserver = PhysMapObserver;

    // --- Panic handler ------------------------------------------------

    /// Forward to the shared bridge in `tairix_kernel::x86_64::panic_ctx`.
    ///
    /// The bridge logs through `SERIAL_SINK`, not the observer, so a panic
    /// never trips the QEMU-exit path: the run times out and the harness
    /// reports the failure loudly.
    #[panic_handler]
    fn tairix_physmap_x86_64_panic(info: &PanicInfo<'_>) -> ! {
        handle_panic_via_kernel_core(info)
    }

    // --- Entry point ---------------------------------------------------

    /// The symbol the arch crate's boot trampoline calls.
    ///
    /// The observer stands in for **both** sinks: the identity-window and
    /// RAM-self-test records are diagnostics on the log channel while
    /// `BootCompleted` is an audit record, and this vertical grades all
    /// three.
    #[no_mangle]
    pub extern "C" fn kernel_main(multiboot_info: u64) -> ! {
        boot(
            multiboot_info,
            &ALLOCATOR,
            &OBSERVER,
            &OBSERVER,
            tairix_log::Level::Info,
        )
    }
}

// --- Host stub -----------------------------------------------------
#[cfg(not(itest_x86_64))]
fn main() {}
