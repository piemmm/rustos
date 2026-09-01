//! `plans/TIMESYNC.md` TS-5b QEMU integration test: boot the production
//! aarch64 `tairix-kernel` pipeline against the planted encrypted-root disk,
//! log in as the seeded `root` account, and drive the **on-disk `servicectl`
//! bundle**'s enrolment verbs from the shell.
//!
//! ## What this vertical asserts
//!
//! Every link of the live *enrolment* path, none of which a host test can
//! reach:
//!
//! * **PID 1 serves a second, distinct endpoint.** Its reactor binds
//!   `SERVICE_ENROL_ENDPOINT` restricted-sender *beside*
//!   `SERVICE_CONTROL_ENDPOINT` and parks on a wait-set carrying both, so an
//!   enrolment request is answered while the login session sits blocked on
//!   its console. The runtime and durable halves are separable by
//!   construction rather than by convention.
//! * **The capability gate is the kernel's.** The call is admitted only
//!   because the seeded `root` account's administrator ceiling carries
//!   `CAP_SERVICE_CONTROL` and `servicectl`'s signed manifest requests it.
//! * **The decision reaches the disk before it is acknowledged.** PID 1
//!   writes the administrator's override document to
//!   `/System/Settings/Services/overrides` on the encrypted root and only
//!   *then* replies success, so the tool's `timed is now disabled` line
//!   cannot be printed by a manager that failed to persist the record. That
//!   line is therefore the witness that a disable survives a reboot — the
//!   next boot reads the same document.
//! * **The round trip is symmetric.** Re-enabling empties the override
//!   document rather than pinning the image's default, so a later system
//!   update is obeyed again.
//!
//! Reaching the exit requires the unlock, the users-database install, the
//! login, the shell, the store lookup, the app load, the kernel's endpoint
//! gate, the reactor's wake, the engine's record, and the filesystem write —
//! all of them. A run where any step fails never earns it.
//!
//! ## How the run completes
//!
//! The guest exits on the **second** enrolment change. The serial script's
//! last step is the `enable` that causes it, and that step is gated on the
//! `disable`'s own success line, so a guest that never persisted the record
//! never types it and the run fails loud on the runner's deadline. Counting
//! rather than exiting on the first is what keeps the script from being cut
//! off mid-way (a step gated on the record the guest exits upon would still
//! be pending when it does).
//!
//! ## How it differs from a production kernel
//!
//! It reuses the entire production aarch64 boot pipeline unchanged. The only
//! difference is that it is a dedicated test bin carrying the QEMU-exit
//! witness — there is no in-kernel exit shortcut to leak into a production
//! build (fail closed).

#![cfg_attr(itest_aarch64, no_std)]
#![cfg_attr(itest_aarch64, no_main)]
#![deny(missing_docs)]

// --- Freestanding test bin (`aarch64-unknown-none`) ----------------

#[cfg(itest_aarch64)]
mod kernel {
    use core::panic::PanicInfo;
    use core::sync::atomic::{AtomicU32, Ordering};

    use tairix_arch_aarch64::{handle_panic_via_serial, qemu_exit, SerialSink, SERIAL_SINK};
    use tairix_kalloc::{FreeListAllocator, Heap, HEAP_BYTES};
    use tairix_kernel::aarch64::boot as boot_aarch64;
    use tairix_log::{Event, Sink};

    // The canonical QEMU `virt` device tree, dumped and embedded at build
    // time (`build.rs`). The boot pipeline discovers the board from it
    // because QEMU passes no `x0` DTB pointer at an ELF `-kernel` entry.
    include!(concat!(env!("OUT_DIR"), "/dtb_fixture.rs"));

    /// Static boot heap, mirroring the production aarch64 kernel binary's
    /// `.bss`-resident heap (zeroed by the boot trampoline).
    ///
    /// `static mut` because the free-list allocator hands out disjoint slices
    /// via an atomic cursor; the storage is otherwise never aliased.
    static mut HEAP: Heap = Heap::ZERO;

    /// Global allocator backed by [`HEAP`].
    ///
    /// SAFETY: the page-aligned `HEAP` static outlives the binary and the
    /// allocator is its only consumer.
    #[global_allocator]
    static ALLOCATOR: FreeListAllocator =
        unsafe { FreeListAllocator::new(core::ptr::addr_of!(HEAP) as *mut u8, HEAP_BYTES) };

    /// How many enrolment changes the run waits for before exiting.
    ///
    /// The `disable` and the `enable` that follows it. Exiting on the first
    /// would cut the script off before the `enable` is typed.
    const EXPECTED_CHANGES: u32 = 2;

    /// Sink that replays every event through [`SERIAL_SINK`] and reports PASS
    /// once PID 1's engine has recorded [`EXPECTED_CHANGES`] enrolment
    /// changes.
    ///
    /// It keys on the manager's own audit id rather than any output the tool
    /// printed: what matters is that the *engine* recorded the decision. The
    /// serial script's gate on the tool's success line is what additionally
    /// proves the manager persisted it before answering.
    struct EnrolmentExitSink {
        changes: AtomicU32,
    }

    impl Sink for EnrolmentExitSink {
        fn write_event(&self, event: &Event<'_>) {
            SerialSink::new().write_event(event);
            if event.id == tairix_init::events::SERVICE_ENROLMENT_CHANGED
                && self.changes.fetch_add(1, Ordering::Relaxed) + 1 >= EXPECTED_CHANGES
            {
                qemu_exit::exit_success();
            }
        }
    }

    static WITNESS_SINK: EnrolmentExitSink = EnrolmentExitSink {
        changes: AtomicU32::new(0),
    };

    /// Forward to the shared aarch64 panic bridge. A panic parks the CPU
    /// before the witness can fire, so the run times out and the harness
    /// reports `Outcome::Timeout` — the documented fail-loud behaviour.
    #[panic_handler]
    fn tairix_enrol_qemu_aarch64_panic(info: &PanicInfo<'_>) -> ! {
        handle_panic_via_serial(info)
    }

    /// Boot entry point — the symbol the arch crate's `boot.s` trampoline
    /// calls (via `tairix_arch_aarch64_main`).
    ///
    /// QEMU hands no DTB pointer (`_dtb == 0`), so the embedded `virt` blob's
    /// address is forwarded to the production boot pipeline. The witness sits
    /// on the **diagnostic** stream because PID 1's service records reach only
    /// that one; the audit stream goes straight to the transcript.
    #[no_mangle]
    pub extern "C" fn kernel_main(_dtb: u64) -> ! {
        let dtb = DTB_BLOB.as_ptr() as u64;
        boot_aarch64::boot(
            dtb,
            &ALLOCATOR,
            &WITNESS_SINK,
            &SERIAL_SINK,
            tairix_log::Level::Info,
            &tairix_kernel::hwtree_store::HW_TREE_SOURCE,
        )
    }
}

// --- Host stub -----------------------------------------------------
#[cfg(not(itest_aarch64))]
fn main() {}
