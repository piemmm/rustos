//! `plans/NEW-SERVICEMANAGER.md` SVC-8 / `plans/TIMESYNC.md` TS-5 QEMU
//! integration test: boot the production aarch64 `tairix-kernel` pipeline
//! against the planted encrypted-root disk, log in as the seeded `root`
//! account, and run the **on-disk `servicectl` bundle** from the shell to stop
//! a running service.
//!
//! ## What this vertical asserts
//!
//! Every link of the live control path, none of which a host test can reach:
//!
//! * **PID 1 serves a control endpoint at all.** Its reactor binds
//!   `SERVICE_CONTROL_ENDPOINT` restricted-sender and parks on a wait-set
//!   carrying that endpoint *beside* any-child readiness — so a control
//!   request is answered while the login session sits blocked on its console,
//!   rather than waiting for some unrelated process to exit.
//! * **The capability gate is the kernel's, not the tool's.** The call is
//!   admitted only because the seeded `root` account's administrator ceiling
//!   carries `CAP_SERVICE_CONTROL` and `servicectl`'s signed manifest requests
//!   it; the tool itself checks nothing.
//! * **The bundle is a real on-disk app.** `servicectl` is resolved from the
//!   `/System/Commands` store by the shared command-resolution policy and
//!   loaded through the ordinary app-load gate — it is not embedded in the
//!   kernel and not on a compiled-in list.
//! * **The engine applied the stop.** The witness is PID 1's own
//!   `SERVICE_CONTROL_STOPPED` audit record, so the vertical and the engine
//!   cannot disagree about what "it worked" means.
//!
//! Reaching that record requires the unlock, the users-database install, the
//! login, the shell, the store lookup, the app load, the kernel's endpoint
//! gate, the reactor's wake, and the engine's reverse-dependency stop — all of
//! them. A run where any step fails never earns it.
//!
//! ## How the run completes
//!
//! The guest exits on the audit record. The serial script types the unlock
//! passphrase, authenticates, and then runs the tool; a run that never earns
//! the witness fails loud on the runner's inactivity/absolute deadline.
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

    /// Sink that replays every event through [`SERIAL_SINK`] and reports PASS
    /// once PID 1's engine has applied a service-control stop.
    ///
    /// It keys on the manager's own audit id rather than any output the tool
    /// printed: what matters is that the *engine* acted, not that a message
    /// reached a terminal.
    struct ServiceControlExitSink;

    impl Sink for ServiceControlExitSink {
        fn write_event(&self, event: &Event<'_>) {
            SerialSink::new().write_event(event);
            if event.id == tairix_init::events::SERVICE_CONTROL_STOPPED {
                qemu_exit::exit_success();
            }
        }
    }

    static WITNESS_SINK: ServiceControlExitSink = ServiceControlExitSink;

    /// Forward to the shared aarch64 panic bridge. A panic parks the CPU
    /// before the witness can fire, so the run times out and the harness
    /// reports `Outcome::Timeout` — the documented fail-loud behaviour.
    #[panic_handler]
    fn tairix_servicectl_qemu_aarch64_panic(info: &PanicInfo<'_>) -> ! {
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
