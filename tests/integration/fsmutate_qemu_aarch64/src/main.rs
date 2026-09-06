//! QEMU integration vertical: **a user's own pointer gestures mutate the
//! filesystem, and the kernel's audit trail records it**
//! (`plans/NEW-FILEMANAGER.md` FM9).
//!
//! # What this proves that no host test can
//!
//! Each write syscall's mutation record is host-tested per operation. What no
//! host test can show is that a *gesture* reaches one: that a click on a drawn
//! menu row is routed to the surface owning it, becomes the write the desktop
//! intended, is authorised under the logged-in account's own identity, and
//! lands in the trail naming the path the user acted on.
//!
//! No run in the tree produced a mutation record before this vertical, so that
//! whole path — gesture to audit trail — had never been exercised on a running
//! kernel.
//!
//! So the guest boots the **production** aarch64 pipeline
//! (`boot_aarch64::boot`) against a planted encrypted root, and the host drives
//! the desktop blind through the QEMU monitor: unlock, log in, start the
//! desktop, then right-click the backdrop and choose *New Folder*. Only the
//! audit sink is swapped, for the PASS witness below.
//!
//! # The PASS gate
//!
//! One latch: a `FsNodeMutated` record whose `op` is [`MKDIR_OP`] and whose
//! `path` ends in [`CREATED_LEAF`] — the name the desktop's own New Folder
//! command chooses for a directory it creates.
//!
//! It is attributed by the *path* the record carries, never by how many
//! mutations have gone by. That matters even here: the file manager creates its
//! Trash directory when a window opens, so a count could latch on an unrelated
//! write and would shift the moment a start-up path changed.
//!
//! The refusing counterpart (`FsMutationDenied`) carries a different event id
//! and so can never latch the witness — a gesture the kernel refuses fails the
//! run rather than passing it.
//!
//! # What this vertical does not cover
//!
//! Only the create. The `rename` half of `plans/NEW-FILEMANAGER.md` FM9-a
//! needs typed characters, and the harness advances its typed-key cursor
//! independently of its pointer cursor, so nothing orders the typing after the
//! click that opens the inline editor — the gate would race by construction.
//! Tracked in `plans/OPEN-DEFECTS.md`.
//!
//! # Why the guest cannot exit early
//!
//! The witness is caused by the last click the runner sends, and that click
//! goes out only once the session has announced the menu it targets is on
//! screen.
//!
//! A panic before the latch parks the CPU, the guest falls silent, and the
//! runner reports a timeout — loud failure, never a false pass.

#![cfg_attr(itest_aarch64, no_std)]
#![cfg_attr(itest_aarch64, no_main)]
#![deny(missing_docs)]

// --- Freestanding test bin (`aarch64-unknown-none`) ----------------

#[cfg(itest_aarch64)]
mod kernel {
    use core::panic::PanicInfo;
    use core::sync::atomic::{AtomicBool, Ordering};

    use tairix_arch_aarch64::{handle_panic_via_serial, qemu_exit, SerialSink, SERIAL_SINK};
    use tairix_kalloc::{FreeListAllocator, Heap, HEAP_BYTES};
    use tairix_kernel::aarch64::boot as boot_aarch64;
    use tairix_log::{Event, Sink};
    use tairix_test_fsmutate_qemu_aarch64::{CREATED_LEAF, MKDIR_OP};

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

    /// Audit observer that replays the whole trail to serial and latches the
    /// PASS witness described in the module docs.
    struct MutationSink {
        /// The desktop's New Folder command created its directory.
        created: AtomicBool,
    }

    impl MutationSink {
        /// A sink with no witness latched.
        const fn new() -> Self {
            Self {
                created: AtomicBool::new(false),
            }
        }

        /// Latch the witness if this successful mutation is the created
        /// directory.
        ///
        /// The operation and the path are read from the record's own fields, so
        /// a mutation of anything else — the manager's own Trash directory, a
        /// service writing its settings — matches nothing (fail closed).
        fn note_mutation(&self, event: &Event<'_>) {
            let mut op = "";
            let mut path = "";
            for field in event.fields {
                let tairix_log::FieldValue::Str(value) = field.value else {
                    continue;
                };
                match field.key {
                    "op" => op = value,
                    "path" => path = value,
                    _ => {}
                }
            }
            if op == MKDIR_OP && path.ends_with(CREATED_LEAF) {
                self.created.store(true, Ordering::Release);
            }
        }

        /// Whether the gesture reached the trail.
        fn passed(&self) -> bool {
            self.created.load(Ordering::Acquire)
        }
    }

    impl Sink for MutationSink {
        fn write_event(&self, event: &Event<'_>) {
            // Replay through the serial sink first, so the QEMU transcript
            // records the full boot → unlock → desktop → gesture timeline and
            // the host can gate its injection on it.
            SerialSink::new().write_event(event);
            if event.id.0 != tairix_kernel_core::audit::AuditEvent::FsNodeMutated.id().0 {
                return;
            }
            self.note_mutation(event);
            if self.passed() {
                qemu_exit::exit_success();
            }
        }
    }

    /// The audit observer the boot pipeline is handed.
    static AUDIT_SINK: MutationSink = MutationSink::new();

    /// Forward to the shared aarch64 panic bridge. A panic before the PASS
    /// finisher parks the CPU, the run times out, and the harness reports
    /// `Outcome::Timeout` — the documented fail-loud behaviour.
    #[panic_handler]
    fn tairix_fsmutate_qemu_aarch64_panic(info: &PanicInfo<'_>) -> ! {
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
            // The mutation records are `Info`, but the host waits for the
            // `Debug` `sc=irq_bind` marker before typing the unlock
            // passphrase, so boot with the filter lowered.
            tairix_log::Level::Debug,
            &tairix_kernel::hwtree_store::HW_TREE_SOURCE,
        )
    }
}

// --- Host stub -----------------------------------------------------
#[cfg(not(itest_aarch64))]
fn main() {}
