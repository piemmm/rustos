//! QEMU integration vertical: **activate a document in the file manager's own
//! window, and watch its authority cross three principals into the viewer that
//! was already running** (`plans/VIEW.md`, `plans/APPWIN.md` AW5).
//!
//! # What this proves that no host test can
//!
//! Every layer is host-tested: the `HandOverLaunch` and open-target wire
//! shapes and their refusals, the window engine's per-client queue and its
//! wake, the session's launch resolution and the relay's fail-closed paths,
//! the kernel's non-widening pass-through of a *held* delegation, and the
//! viewer's drain. What only a real machine can show is that they are **wired
//! to each other across three principals**: activating an item in a window the
//! *file manager* owns makes the manager mint a delegation for a file it
//! opened under the user's identity, the *session* redeem that delegation and
//! grant the same authority on — never its own larger reach — and the
//! *viewer*, which holds no filesystem capability at all, redeem what arrives.
//!
//! So the guest boots the **production** aarch64 pipeline
//! (`boot_aarch64::boot`) against a planted encrypted root, and the host drives
//! the desktop blind through the QEMU monitor: unlock, log in, start the
//! desktop, open a file-manager window from its icon-bar slot, and activate the
//! planted picture in it — once per relay, plus the first activation, which
//! finds no viewer and makes the *manager* start one. Only the audit sink is
//! swapped, for the PASS witnesses below.
//!
//! Nothing pre-launches the viewer, deliberately: the instance every later
//! document must reach is then one the **desktop never spawned**, so the only
//! thing that can name it is the identity the kernel attested. While the script
//! launched it from the program library first, the desktop's own launch table
//! knew it and this vertical could not have failed.
//!
//! # The PASS gate
//!
//! [`RELAY_ROUNDS`] complete relays. One relay is the four dispatched
//! syscalls of [`RELAY_CHAIN`], which must land **in that order**, each
//! attributed by the kernel to the principal that made it:
//!
//! 1. the manager mints (`fd_grant` from `files`),
//! 2. the session redeems it, bound to the manager (`fd_redeem_from` from
//!    `desktop`),
//! 3. the session mints the same authority on (`fd_grant` from `desktop`),
//! 4. the viewer redeems it (`fd_redeem` from `view`).
//!
//! Unrelated records between the steps are ignored rather than breaking the
//! chain — thousands of syscalls separate them — so what the gate asserts is
//! the *relative* order of the four, which is the claim: read as a set they
//! would be consistent with three processes touching descriptors of their own;
//! read as a sequence they can only be one authority travelling to a principal
//! with no authority to open the file itself.
//!
//! Each latched step prints its own marker
//! ([`RELAY_STEP_MARKERS`](tairix_test_handover_qemu_aarch64::RELAY_STEP_MARKERS)),
//! so a run that does not pass states how far the authority travelled rather
//! than only falling silent — the difference between "the gesture never
//! reached an item" and "the desktop never redeemed what it was handed".
//!
//! Every relay after the first additionally requires the viewer's redeem to
//! come from the **same kernel-attested task**. That is what makes this "one
//! viewer process, three documents" rather than "a viewer per document": a task
//! belongs to one process for its whole life, so the same task redeeming each
//! means the desktop's single-instance funnel reached the instance that was
//! already running, and the viewer opened another window rather than another
//! viewer being started. A redeem from any other task resets the chain instead
//! of completing it (fail closed) — which is exactly what a desktop resolving a
//! slot's application from its own launch bookkeeping produced, because the
//! manager, not the desktop, started the viewer.
//!
//! # Why the guest latches no frame
//!
//! Reading the screen is the host's job. The guest deliberately claims nothing
//! about pixels: what it can attest is which principal called which syscall,
//! from which thread of control, and that is the whole security claim here.
//!
//! # Why the guest cannot exit early
//!
//! Each relay is caused by one activation in the manager's window, and both of
//! that activation's gestures are gated on the session's own announcement that
//! the surface they aim at is on screen — the window for the press that opens
//! the item's menu, the drawn plate for the press on its *Open* row. So the
//! records that complete the PASS cannot happen before the gestures that cause
//! them.
//!
//! A panic before the gate parks the CPU, the guest falls silent, and the
//! runner reports a timeout — loud failure, never a false pass.

#![cfg_attr(itest_aarch64, no_std)]
#![cfg_attr(itest_aarch64, no_main)]
#![deny(missing_docs)]

// --- Freestanding test bin (`aarch64-unknown-none`) ----------------

#[cfg(itest_aarch64)]
mod kernel {
    use core::panic::PanicInfo;
    use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

    use tairix_arch_aarch64::{handle_panic_via_serial, qemu_exit, SerialSink, SERIAL_SINK};
    use tairix_kalloc::{FreeListAllocator, Heap, HEAP_BYTES};
    use tairix_kernel::aarch64::boot as boot_aarch64;
    use tairix_log::{Event, Sink};
    use tairix_test_handover_qemu_aarch64::{
        FOREIGN_VIEWER_MARKER, RELAY_CHAIN, RELAY_ROUNDS, RELAY_STEP_MARKERS, VIEWER_REDEEM_STEP,
    };

    // The canonical QEMU `virt` device tree, dumped and embedded at build
    // time (`build.rs`). The boot pipeline discovers the board from it
    // because QEMU passes no `x0` DTB pointer at an ELF `-kernel` entry.
    include!(concat!(env!("OUT_DIR"), "/dtb_fixture.rs"));

    /// Static boot heap, mirroring the production aarch64 kernel binary's
    /// `.bss`-resident heap (zeroed by the boot trampoline).
    ///
    /// `static mut` because the allocator hands out disjoint slices under
    /// its own lock; the storage is otherwise never aliased.
    static mut HEAP: Heap = Heap::ZERO;

    /// Global allocator backed by [`HEAP`].
    ///
    /// SAFETY: the page-aligned `HEAP` static outlives the binary and the
    /// allocator is its only consumer.
    #[global_allocator]
    static ALLOCATOR: FreeListAllocator =
        unsafe { FreeListAllocator::new(core::ptr::addr_of!(HEAP) as *mut u8, HEAP_BYTES) };

    /// The task field's value for "no viewer has redeemed yet".
    ///
    /// Task 0 is the in-kernel system context, which issues no user-space
    /// syscall, so it can never be a captured value and needs no second flag
    /// to distinguish "unset" from a real reading. Keeping the whole identity
    /// in one atomic means nothing here depends on the visibility of one
    /// location relative to another.
    const NO_VIEWER_TASK: u64 = 0;

    /// Audit observer that replays the whole trail to serial and latches the
    /// ordered relay chains described in the module docs.
    struct HandOverSink {
        /// How far the relay currently being watched has got: the index in
        /// [`RELAY_CHAIN`] of the record still owed.
        step: AtomicU32,
        /// Complete relays seen so far.
        relays: AtomicU32,
        /// The kernel-attested task of the viewer that redeemed the first
        /// relayed document, or [`NO_VIEWER_TASK`].
        viewer_task: AtomicU64,
    }

    impl HandOverSink {
        /// A sink watching for the first record of the first relay.
        const fn new() -> Self {
            Self {
                step: AtomicU32::new(0),
                relays: AtomicU32::new(0),
                viewer_task: AtomicU64::new(NO_VIEWER_TASK),
            }
        }

        /// Advance the watched relay if this dispatched syscall is the record
        /// it is owed.
        ///
        /// The calling process, the syscall, and the calling task are all read
        /// from the record's own kernel-attested fields, so no principal can
        /// present itself as another. A record that is not the one owed is
        /// ignored: unrelated calls separate the steps of a real relay, so
        /// only their relative order is asserted.
        fn note_syscall(&self, event: &Event<'_>) {
            let mut comm = "";
            let mut call = "";
            let mut task = "";
            for field in event.fields {
                let tairix_log::FieldValue::Str(value) = field.value else {
                    continue;
                };
                match field.key {
                    "comm" => comm = value,
                    "sc" => call = value,
                    "task" => task = value,
                    _ => {}
                }
            }
            let step = self.step.load(Ordering::Relaxed) as usize;
            let Some(&(want_comm, want_call)) = RELAY_CHAIN.get(step) else {
                return;
            };
            if comm != want_comm || call != want_call {
                return;
            }
            if step == VIEWER_REDEEM_STEP && !self.viewer_is_the_running_instance(task) {
                // A redeem by some other viewer says nothing about the funnel
                // reaching the instance already running, so the relay does not
                // complete: start watching for a fresh one.
                emit_marker(FOREIGN_VIEWER_MARKER);
                self.step.store(0, Ordering::Relaxed);
                return;
            }
            if let Some(&marker) = RELAY_STEP_MARKERS.get(step) {
                emit_marker(marker);
            }
            if step + 1 < RELAY_CHAIN.len() {
                #[allow(clippy::cast_possible_truncation)] // A chain index is tiny.
                self.step.store(step as u32 + 1, Ordering::Relaxed);
                return;
            }
            self.step.store(0, Ordering::Relaxed);
            self.relays.fetch_add(1, Ordering::Relaxed);
        }

        /// Whether the `task` that just redeemed is the viewer instance this
        /// run has been watching — capturing it if this is the first redeem.
        ///
        /// An unparsable task field is refused rather than treated as a match:
        /// the gate would otherwise pass on a record whose identity it could
        /// not read.
        fn viewer_is_the_running_instance(&self, task: &str) -> bool {
            let Ok(task) = u64::from_str_radix(task, 16) else {
                return false;
            };
            if task == NO_VIEWER_TASK {
                return false;
            }
            match self.viewer_task.compare_exchange(
                NO_VIEWER_TASK,
                task,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => true,
                Err(captured) => captured == task,
            }
        }

        /// Whether enough complete relays have been seen.
        fn passed(&self) -> bool {
            self.relays.load(Ordering::Relaxed) >= RELAY_ROUNDS
        }
    }

    impl Sink for HandOverSink {
        fn write_event(&self, event: &Event<'_>) {
            // Replay through the serial sink first, so the QEMU transcript
            // records the full boot → unlock → desktop → hand-over timeline
            // and the host can gate its injection on it.
            SerialSink::new().write_event(event);
            if event.id.0 != tairix_kernel_syscall::AuditEvent::SyscallInvoked.id().0 {
                return;
            }
            self.note_syscall(event);
            if self.passed() {
                qemu_exit::exit_success();
            }
        }
    }

    /// Print one progress marker on serial. Carries no fields: the marker *is*
    /// the fact, and it names a step the kernel attested rather than anything
    /// this sink inferred.
    fn emit_marker(message: &'static str) {
        SerialSink::new().write_event(&Event {
            level: tairix_log::Level::Info,
            id: tairix_log::EventId(0),
            message,
            fields: &[],
        });
    }

    /// The audit observer the boot pipeline is handed.
    static AUDIT_SINK: HandOverSink = HandOverSink::new();

    /// Forward to the shared aarch64 panic bridge. A panic before the PASS
    /// finisher parks the CPU, the run times out, and the harness reports
    /// `Outcome::Timeout` — the documented fail-loud behaviour.
    #[panic_handler]
    fn tairix_handover_qemu_aarch64_panic(info: &PanicInfo<'_>) -> ! {
        handle_panic_via_serial(info)
    }

    /// Boot entry point — the symbol the arch crate's `boot.s` trampoline
    /// calls (via `tairix_arch_aarch64_main`).
    ///
    /// QEMU hands no DTB pointer (`_dtb == 0`), so the embedded `virt` blob's
    /// address is forwarded to the production boot pipeline with the audit
    /// observer in place.
    #[no_mangle]
    pub extern "C" fn kernel_main(_dtb: u64) -> ! {
        let dtb = DTB_BLOB.as_ptr() as u64;
        boot_aarch64::boot(
            dtb,
            &ALLOCATOR,
            &SERIAL_SINK,
            &AUDIT_SINK,
            // `SyscallInvoked` (`EventId(5000)`) is `Debug`, below the
            // default `Info` filter, and it carries every PASS witness; the
            // host also waits for that record's `sc=irq_bind` marker before
            // typing the unlock passphrase, so boot with the filter lowered.
            tairix_log::Level::Debug,
            &tairix_kernel::hwtree_store::HW_TREE_SOURCE,
        )
    }
}

// --- Host stub -----------------------------------------------------
#[cfg(not(itest_aarch64))]
fn main() {}
