//! `plans/STRESSTEST.md` ST5 QEMU integration test: boot the *production*
//! aarch64 `rustos-kernel` pipeline on the `virt` board with the planted
//! whole-disk encrypted-root image, log in as the seeded `root` account,
//! run an oversubscribed background CPU load plus a mixed **`stress`** load
//! under `--timeout` on the console, prove foreground service progress while
//! the CPU load is active, watch both runs tear themselves down, and render
//! the post-load `sysinfo pressure` / `sysinfo reclaim` figures on the
//! transcript.
//!
//! ## What this test asserts
//!
//! The production boot path unlocks and mounts the encrypted `RustFS`
//! root, login authenticates `root`/`root`, and the session shell runs the
//! runner's ordered script:
//!
//! 1. `stress --cpu 10 --timeout 4s &` on four emulated CPUs — the shell's
//!    background-job form returns the prompt, the controller confirms all ten
//!    workers were dispatched, then `sysinfo uptime` must render `since boot:`
//!    while all CPUs are saturated. This proves timer-driven CFQ preemption
//!    keeps the shell, IPC service, and controller progressing while CPU
//!    workers issue no syscalls. The command-level `--background` detach path
//!    has separate parser/controller coverage and feeds the same controller.
//! 2. `stress --cpu 1 --vm 1 --vm-bytes 16M --io 1 --timeout 2s` — the
//!    store bundle spawns through the full signature + capability +
//!    interface-hash load gate; the controller pins itself
//!    (`CAP_MEM_PIN`), opts into the signal intake, and re-enters its own
//!    attested binary as three workers through the kernel's `@self`
//!    token. The `dispatching hogs` line on the transcript witnesses the
//!    dispatch; the `successful run completed` line witnesses the timeout
//!    teardown — every worker `Terminate`d, reaped, and the scratch files
//!    removed — and the prompt returning at all is the reap-and-exit
//!    witness.
//! 3. `sysinfo pressure` — the `reserve bytes:` token witnesses the gated
//!    `MEMORY_PRESSURE` figures rendered after the load.
//! 4. `sysinfo reclaim` — the `clean-file-data` class row witnesses the
//!    `RECLAIM_STATS` ledger rendered after the io worker churned the
//!    write path.
//! 5. `exit` — typed only after both renders appeared.
//!
//! ## Why the PASS keys on `stress`'s exit *then* the shell's exit
//!
//! The kernel-side witness is the stress **controller's** audited `exit`
//! (`SyscallInvoked`, `EventId(5000)`, `sc=exit`, `comm=stress`) — the
//! workers are ended by `Terminate` and never invoke the `exit` syscall,
//! so the controller's is unambiguous — which only lands after the whole
//! dispatch/teardown ran. Exiting QEMU there would tear the run down
//! before the post-load `sysinfo` renders were observed, so the sink only
//! *arms* on it and reports PASS on the audited `exit` of the **shell**
//! (`comm=elsh`), typed by the runner only after both renders appeared —
//! so the verified lines provably reached the transcript before the run
//! ended (the arm-then-exit discipline). The two `sysinfo` exits between
//! them do not fire the PASS: only the shell's name does. A refused
//! spawn, a failed teardown, or a hung controller never reaches the armed
//! exit: the run times out with the failing step in the serial transcript
//! — the documented fail-loud behaviour.
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
    use core::sync::atomic::{AtomicBool, Ordering};

    use rustos_arch_aarch64::{handle_panic_via_serial, qemu_exit, SerialSink, SERIAL_SINK};
    use rustos_kalloc::{FreeListAllocator, Heap, HEAP_BYTES};
    use rustos_kernel::aarch64::boot as boot_aarch64;
    use rustos_log::{Event, EventId, FieldValue, Sink};

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

    /// The session shell's attested process name: the PASS finisher fires
    /// on its audited `exit` — typed by the runner only after both
    /// post-load `sysinfo` renders appeared — never on the intervening
    /// `sysinfo` exits.
    const SHELL_COMM: &str = "elsh";

    /// Set once `stress`'s audited `exit` has been observed. The PASS
    /// finisher fires on the shell's audited `exit`, so the verified
    /// post-load renders provably reached the transcript before the run
    /// ended.
    static CONTROLLER_DONE: AtomicBool = AtomicBool::new(false);

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
    /// PASS once `stress` has exited and the shell's subsequent scripted
    /// `exit` dispatches (see the module docs for why the PASS is deferred
    /// to the shell's own exit).
    struct StressSink;

    impl Sink for StressSink {
        fn write_event(&self, event: &Event<'_>) {
            // Replay through the serial sink so the QEMU transcript records
            // the full boot + session timeline.
            SerialSink::new().write_event(event);
            if event.id != SYSCALL_INVOKED_EVENT_ID || field_str(event, "sc") != Some("exit") {
                return;
            }
            match field_str(event, "comm") {
                Some(CONTROLLER_COMM) => CONTROLLER_DONE.store(true, Ordering::Release),
                Some(SHELL_COMM) if CONTROLLER_DONE.load(Ordering::Acquire) => {
                    qemu_exit::exit_success();
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
    fn rustos_stress_qemu_aarch64_panic(info: &PanicInfo<'_>) -> ! {
        handle_panic_via_serial(info)
    }

    /// Boot entry point — the symbol the arch crate's `boot.s` trampoline
    /// calls (via `rustos_arch_aarch64_main`).
    ///
    /// QEMU hands no DTB pointer (`_dtb == 0`), so the embedded `virt`
    /// blob's address is forwarded to the production boot pipeline with the
    /// audit-observer sink in place.
    #[no_mangle]
    pub extern "C" fn kernel_main(_dtb: u64) -> ! {
        let dtb = DTB_BLOB.as_ptr() as u64;
        boot_aarch64::boot(
            dtb,
            &SERIAL_SINK,
            &AUDIT_SINK,
            // `SyscallInvoked` (`EventId(5000)`) is `Debug`, below the
            // default `Info` filter; this observer counts it, so boot
            // with the filter lowered.
            rustos_log::Level::Debug,
            &rustos_kernel::hwtree_store::HW_TREE_SOURCE,
        )
    }
}

// --- Host stub -----------------------------------------------------
#[cfg(not(itest_aarch64))]
fn main() {}
