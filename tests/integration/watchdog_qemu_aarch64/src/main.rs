//! `plans/NEW-SERVICEMANAGER.md` SVC-8 QEMU integration test: boot the
//! production aarch64 `tairix-kernel` pipeline and prove the **live
//! liveness-watchdog path** — renew, wedge, detect, force-kill, reap,
//! relaunch — on a machine.
//!
//! ## Why the disk carries a test double
//!
//! A wedge cannot be provoked from outside a process: it is the *absence*
//! of a call, and only the supervised program can stop making one. PID 1's
//! registered set is its compiled-in floor description, so a fixture
//! service cannot simply be added to a disk and supervised. This vertical's
//! disk therefore plants the watchdog fixture
//! (`tests/integration/watchdog_service_program`) **as** the `netstack`
//! service bundle. Everything else is production: PID 1 registers the
//! service from its own floor description, spawns it on the netstack
//! service account, arms its watchdog from the interval that description
//! declares, receives its renewals over the real lifecycle-notice endpoint,
//! and applies the real restart policy. Only the supervised program is the
//! double, and only on this disk.
//!
//! ## What this vertical asserts
//!
//! Both halves of the watchdog, each as a positive witness:
//!
//! * **A renewing service is not killed.** The fixture renews through the
//!   same `tairix_rt::servicenotice::Watchdog` a real service uses, and
//!   records each accepted renewal. Three of them carry it past a whole
//!   interval — the line an un-renewed watchdog would already have crossed
//!   — so the count is what distinguishes a working renewal from a
//!   discarded one. A timeout arriving with too few renewals behind it is a
//!   **failure**, not a pass, because it would mean the timeout was the
//!   trivial one a never-renewing service earns.
//! * **A wedged service is recovered.** The fixture then stops renewing and
//!   parks for good. PID 1 must notice
//!   (`SERVICE_WATCHDOG_TIMEOUT`), force the process down, reap it, and
//!   relaunch it under the `on-failure` policy its floor directive declares
//!   — `SERVICE_STARTED` *after* the timeout is the exit witness.
//!
//! Reaching that record requires the floor directive's options to have been
//! parsed, the interval to have been armed, the notice endpoint to be bound
//! and answering, the engine to resolve an attested sender to a running
//! service, the one-shot deadline to be folded into PID 1's park and
//! expired, the force-terminate to reach a parked process, the reap to
//! classify it as an abnormal exit, and the restart policy to relaunch it.
//! A run where any of those fails never earns it.
//!
//! ## How the run completes
//!
//! The guest exits on the relaunch. No serial script is needed: the service
//! is a boot-floor entry, so the whole sequence happens before (and
//! independently of) the login prompt. A run that never earns the witness
//! fails loud on the runner's inactivity/absolute deadline; a run that
//! earns a *wrong* witness exits with its own distinguishable code.
//!
//! ## How it differs from a production kernel
//!
//! It reuses the entire production aarch64 boot pipeline unchanged. The
//! only difference is that it is a dedicated test bin carrying the
//! QEMU-exit witness — there is no in-kernel exit shortcut to leak into a
//! production build (fail closed).

#![cfg_attr(itest_aarch64, no_std)]
#![cfg_attr(itest_aarch64, no_main)]
#![deny(missing_docs)]

// --- Freestanding test bin (`aarch64-unknown-none`) ----------------

#[cfg(itest_aarch64)]
mod kernel {
    use core::num::NonZeroU16;
    use core::panic::PanicInfo;
    use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

    use tairix_arch_aarch64::{handle_panic_via_serial, qemu_exit, SerialSink, SERIAL_SINK};
    use tairix_kalloc::{FreeListAllocator, Heap, HEAP_BYTES};
    use tairix_kernel::aarch64::boot as boot_aarch64;
    use tairix_log::{Event, FieldValue, Sink};
    use tairix_test_watchdog_service::{
        RENEWALS_BEFORE_WEDGE, RENEWED, SERVICE_FIELD, SUBSTITUTES, UNWATCHED,
    };

    // The canonical QEMU `virt` device tree, dumped and embedded at build
    // time (`build.rs`). The boot pipeline discovers the board from it
    // because QEMU passes no `x0` DTB pointer at an ELF `-kernel` entry.
    include!(concat!(env!("OUT_DIR"), "/dtb_fixture.rs"));

    /// Static boot heap, mirroring the production aarch64 kernel binary's
    /// `.bss`-resident heap (zeroed by the boot trampoline).
    ///
    /// `static mut` because the free-list allocator hands out disjoint
    /// slices via an atomic cursor; the storage is otherwise never aliased.
    static mut HEAP: Heap = Heap::ZERO;

    /// Global allocator backed by [`HEAP`].
    ///
    /// SAFETY: the page-aligned `HEAP` static outlives the binary and the
    /// allocator is its only consumer.
    #[global_allocator]
    static ALLOCATOR: FreeListAllocator =
        unsafe { FreeListAllocator::new(core::ptr::addr_of!(HEAP) as *mut u8, HEAP_BYTES) };

    /// The manager reported no watchdog, so the fixture had nothing to
    /// renew and the run could prove neither half.
    const FAIL_UNWATCHED: u16 = 11;
    /// The watchdog fired before the fixture had renewed its way past a
    /// whole interval — so the kill is the trivial one a never-renewing
    /// service earns, and renewal is not doing anything.
    const FAIL_KILLED_WHILE_HEALTHY: u16 = 12;
    /// The manager gave up relaunching the double: its crash-loop budget was
    /// consumed before the first relaunch was observed.
    const FAIL_NOT_RELAUNCHED: u16 = 13;

    /// Replays every event through [`SERIAL_SINK`] and decides the run.
    ///
    /// It keys on the *manager's* own audit ids and the *fixture's* own
    /// records, never on anything printed to a console: what matters is
    /// that the engine acted, and a service has no terminal to say so on.
    struct WatchdogExitSink {
        /// Renewals the fixture has had accepted so far.
        renewals: AtomicU32,
        /// Whether the manager has since force-terminated it for wedging.
        timed_out: AtomicBool,
    }

    impl Sink for WatchdogExitSink {
        fn write_event(&self, event: &Event<'_>) {
            SerialSink::new().write_event(event);
            if event.id == UNWATCHED {
                fail(FAIL_UNWATCHED);
            }
            if event.id == RENEWED {
                self.renewals.fetch_add(1, Ordering::Relaxed);
                return;
            }
            if event.id == tairix_init::events::SERVICE_WATCHDOG_TIMEOUT && names_the_double(event)
            {
                if self.renewals.load(Ordering::Relaxed) < RENEWALS_BEFORE_WEDGE {
                    fail(FAIL_KILLED_WHILE_HEALTHY);
                }
                self.timed_out.store(true, Ordering::Relaxed);
                return;
            }
            // Only the double's budget bears on the run. Anything that needs a
            // working stack fails on this disk, and giving up on it is the
            // manager's bound working, not the watchdog failing.
            if event.id == tairix_init::events::SERVICE_RESTART_EXHAUSTED && names_the_double(event)
            {
                fail(FAIL_NOT_RELAUNCHED);
            }
            // The relaunch. Only counted after the timeout, so the boot's
            // own first start of the service cannot be mistaken for it.
            if event.id == tairix_init::events::SERVICE_STARTED
                && self.timed_out.load(Ordering::Relaxed)
                && names_the_double(event)
            {
                qemu_exit::exit_success();
            }
        }
    }

    /// Whether a manager record is about the service the disk doubled.
    ///
    /// The manager names the service in its own audit field, so the witness
    /// reads that attribution rather than inferring one from ordering: the
    /// service that came back must be the service that was killed.
    fn names_the_double(event: &Event<'_>) -> bool {
        event.fields.iter().any(|field| {
            field.key == SERVICE_FIELD && matches!(field.value, FieldValue::Str(SUBSTITUTES))
        })
    }

    /// End the run with a distinguishable code, so a wrong outcome is told
    /// apart from a hang rather than both reading as a timeout.
    fn fail(code: u16) -> ! {
        match NonZeroU16::new(code) {
            Some(code) => qemu_exit::exit_failure(code),
            // Unreachable for the constants above, and a zero code would
            // read as success — refuse rather than pass.
            None => qemu_exit::exit_failure(NonZeroU16::MIN),
        }
    }

    static WITNESS_SINK: WatchdogExitSink = WatchdogExitSink {
        renewals: AtomicU32::new(0),
        timed_out: AtomicBool::new(false),
    };

    /// Forward to the shared aarch64 panic bridge. A panic parks the CPU
    /// before the witness can fire, so the run times out and the harness
    /// reports `Outcome::Timeout` — the documented fail-loud behaviour.
    #[panic_handler]
    fn tairix_watchdog_qemu_aarch64_panic(info: &PanicInfo<'_>) -> ! {
        handle_panic_via_serial(info)
    }

    /// Boot entry point — the symbol the arch crate's `boot.s` trampoline
    /// calls (via `tairix_arch_aarch64_main`).
    ///
    /// QEMU hands no DTB pointer (`_dtb == 0`), so the embedded `virt`
    /// blob's address is forwarded to the production boot pipeline. The
    /// witness sits on the **diagnostic** stream because PID 1's service
    /// records and the fixture's own reach only that one; the audit stream
    /// goes straight to the transcript.
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
