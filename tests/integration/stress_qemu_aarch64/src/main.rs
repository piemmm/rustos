//! `plans/STRESSTEST.md` ST5 QEMU integration test: boot the *production*
//! aarch64 `tairix-kernel` pipeline on the `virt` board with the planted
//! whole-disk encrypted-root image, log in as the seeded `root` account,
//! run the reported oversubscribed detached **`stress`** command under its full
//! 120-second timeout, and prove the shell and `sysmon` remain interactive
//! while all four CPUs are saturated.
//!
//! ## What this test asserts
//!
//! The production boot path unlocks and mounts the encrypted `ARXFS`
//! root, login authenticates `root`/`root`, and the session shell runs the
//! runner's ordered script:
//!
//! 1. After the authenticated shell prompt appears, the runner waits one
//!    second and types exactly
//!    `stress --cpu 10 --timeout 120s --background`. The command-level detach
//!    path respawns a quiet controller and returns the shell prompt.
//! 2. The returned prompt must accept `sysmon`. Its `Pressure:` frame must
//!    render while all CPUs are saturated; `r` must refresh to the
//!    `reclaimable` panel, and `q` must return to the shell. This proves
//!    timer-driven preemption, input delivery, IPC, and the system-information
//!    service all progress while CPU workers issue no syscalls.
//! 3. After `sysmon` returns, the script advances past the launcher's early
//!    stress-worker syscalls and, on the next `comm=stress` line (the detached
//!    controller waking to tear its 120-second run down), types `exit`. PASS
//!    ordering does not depend on that marker — see below.
//!
//! ## Why the PASS keys on two `stress` exits *and* the shell's exit
//!
//! `--background` creates two `comm=stress` audited exits: the foreground
//! launcher after it has spawned the detached controller, then the detached
//! controller after the full load and teardown. Workers are ended by
//! `Terminate` and never invoke `exit`. PASS requires all three witnesses —
//! both `comm=stress` exits and the scripted `comm=elsh` exit — and fires
//! once the *last* of them is observed, in **any** order. Order-independence
//! is deliberate: the detached controller's exit and the shell's scripted
//! `exit` are concurrent (the shell can exit while the reparented controller
//! is still tearing down its 120-second run), so keying PASS on a fixed
//! arrival order would race — exactly the flake this shape removes. The three
//! events still each prove their property: the two stress exits that the full
//! load spawned, pinned, dispatched, timed out, reaped and summarised; the
//! shell exit that the console stayed interactive enough to accept it. A
//! refused spawn, unresponsive console, failed teardown, or hung controller
//! leaves a witness unreached and the run fails loud by timeout.
//!
//! ## Embedded `virt` device tree
//!
//! QEMU's `-kernel <ELF>` aarch64 path passes no DTB pointer (`x0 = 0`),
//! so the canonical `virt` device tree is dumped and embedded at build
//! time (`build.rs`) and its address handed to the boot pipeline, which
//! discovers the board from it exactly as it would from real firmware.
//!
//! ## How it differs from a production kernel
//!
//! It reuses the entire production aarch64 boot pipeline and only replaces
//! the audit sink. Splitting the audit-observer behaviour into a separate
//! bin (instead of a Cargo feature on a production crate) prevents feature
//! unification from leaking the QEMU-exit shortcut into any production
//! build (fail closed; the harness never decides what the kernel does
//! next).

#![cfg_attr(itest_aarch64, no_std)]
#![cfg_attr(itest_aarch64, no_main)]
#![deny(missing_docs)]

// --- Freestanding test bin (`aarch64-unknown-none`) ----------------

#[cfg(itest_aarch64)]
mod kernel {
    use core::panic::PanicInfo;
    use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    use tairix_arch_aarch64::{handle_panic_via_serial, qemu_exit, SerialSink, SERIAL_SINK};
    use tairix_kalloc::{FreeListAllocator, Heap, HEAP_BYTES};
    use tairix_kernel::aarch64::boot as boot_aarch64;
    use tairix_log::{Event, EventId, FieldValue, Sink};

    // The canonical QEMU `virt` device tree, dumped and embedded at build
    // time (`build.rs`). The boot pipeline discovers the board from it
    // because QEMU passes no `x0` DTB pointer at an ELF `-kernel` entry.
    include!(concat!(env!("OUT_DIR"), "/dtb_fixture.rs"));

    /// Static boot heap, mirroring the production aarch64 kernel binary's
    /// `.bss`-resident heap (zeroed by the boot trampoline).
    ///
    /// `static mut` because the bump allocator hands out disjoint slices via
    /// an atomic cursor; the storage is otherwise never aliased.
    static mut HEAP: Heap = Heap::ZERO;

    /// Global allocator backed by [`HEAP`].
    ///
    /// SAFETY: the page-aligned `HEAP` static outlives the binary and the
    /// allocator is its only consumer.
    #[global_allocator]
    static ALLOCATOR: FreeListAllocator =
        unsafe { FreeListAllocator::new(core::ptr::addr_of!(HEAP) as *mut u8, HEAP_BYTES) };

    /// `EventId` emitted by the syscall dispatcher for an audited syscall
    /// that passed every check. Pinned by the audit-id test in
    /// `kernel/syscall/src/audit.rs`.
    const SYSCALL_INVOKED_EVENT_ID: EventId = EventId(5000);

    /// The scripted session's tool under test. Its audited `exit` is the
    /// kernel-side witness that the whole load run — spawn, pin, worker
    /// dispatch through `@self`, the timeout teardown, the reap, the
    /// scratch cleanup, the summary — ran to completion. Its workers are
    /// ended by `Terminate` and never invoke `exit`, so the controller's
    /// record is unambiguous.
    const CONTROLLER_COMM: &str = "stress";

    /// The session shell's attested process name. Its audited `exit` is one
    /// of the three PASS witnesses; the runner types it once (`comm=sysmon`
    /// exits in between never match), and it may arrive before or after the
    /// detached controller's exit, so the finisher fires on whichever witness
    /// completes the set.
    const SHELL_COMM: &str = "elsh";

    /// The foreground detach launcher and the detached load controller each
    /// invoke `exit`; workers are terminated by the controller without doing
    /// so themselves.
    const REQUIRED_STRESS_EXITS: u64 = 2;

    /// Number of audited `stress` exits observed by the sink.
    static STRESS_EXIT_COUNT: AtomicU64 = AtomicU64::new(0);

    /// Whether the scripted session shell's audited `exit` has been observed.
    static SHELL_EXITED: AtomicBool = AtomicBool::new(false);

    /// The string value of `event`'s field `key`, if present.
    fn field_str<'e>(event: &Event<'e>, key: &str) -> Option<&'e str> {
        event.fields.iter().find_map(|field| {
            if field.key == key {
                match field.value {
                    FieldValue::Str(s) => Some(s),
                    _ => None,
                }
            } else {
                None
            }
        })
    }

    /// Sink that replays every event through [`SERIAL_SINK`] and reports
    /// PASS once both `stress` processes and the scripted session shell have
    /// each dispatched their audited `exit`, in any order (see the module docs
    /// for why the order is not fixed).
    struct StressSink;

    impl StressSink {
        /// Fire the QEMU success exit once every required witness has been
        /// observed: both `stress` exits and the shell exit. Called after each
        /// witness so PASS fires on whichever arrives last.
        fn finish_if_complete() {
            if SHELL_EXITED.load(Ordering::Acquire)
                && STRESS_EXIT_COUNT.load(Ordering::Acquire) >= REQUIRED_STRESS_EXITS
            {
                qemu_exit::exit_success();
            }
        }
    }

    impl Sink for StressSink {
        fn write_event(&self, event: &Event<'_>) {
            // Replay through the serial sink so the QEMU transcript records
            // the full boot + session timeline.
            SerialSink::new().write_event(event);
            if event.id != SYSCALL_INVOKED_EVENT_ID || field_str(event, "sc") != Some("exit") {
                return;
            }
            match field_str(event, "comm") {
                Some(CONTROLLER_COMM) => {
                    STRESS_EXIT_COUNT.fetch_add(1, Ordering::AcqRel);
                    Self::finish_if_complete();
                }
                Some(SHELL_COMM) => {
                    SHELL_EXITED.store(true, Ordering::Release);
                    Self::finish_if_complete();
                }
                _ => {}
            }
        }
    }

    static AUDIT_SINK: StressSink = StressSink;

    /// Forward to the shared aarch64 panic bridge. A panic before the PASS
    /// finisher parks the CPU, the run times out, and the harness reports
    /// `Outcome::Timeout` — the documented fail-loud behaviour.
    #[panic_handler]
    fn tairix_stress_qemu_aarch64_panic(info: &PanicInfo<'_>) -> ! {
        handle_panic_via_serial(info)
    }

    /// Boot entry point — the symbol the arch crate's `boot.s` trampoline
    /// calls (via `tairix_arch_aarch64_main`).
    ///
    /// QEMU hands no DTB pointer (`_dtb == 0`), so the embedded `virt`
    /// blob's address is forwarded to the production boot pipeline with the
    /// audit-observer sink in place.
    #[no_mangle]
    pub extern "C" fn kernel_main(_dtb: u64) -> ! {
        let dtb = DTB_BLOB.as_ptr() as u64;
        boot_aarch64::boot(
            dtb,
            &ALLOCATOR,
            &SERIAL_SINK,
            &AUDIT_SINK,
            // `SyscallInvoked` (`EventId(5000)`) is `Debug`, below the
            // default `Info` filter; this observer counts it, so boot
            // with the filter lowered.
            tairix_log::Level::Debug,
            &tairix_kernel::hwtree_store::HW_TREE_SOURCE,
        )
    }
}

// --- Host stub -----------------------------------------------------
#[cfg(not(itest_aarch64))]
fn main() {}
