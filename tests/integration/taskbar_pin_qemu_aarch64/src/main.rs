//! QEMU integration vertical: **pin an application to the taskbar from the
//! program library, open the Switchboard, and launch the app from its new
//! pin** (`plans/NEW-TASKBAR.md` T15).
//!
//! # What this proves that no host test can
//!
//! Every piece of taskbar *logic* — the pin store grammar, the context
//! menu's rows, the strip's layout and hit-testing, the Switchboard
//! capsule's gesture — is already covered by host unit tests. What only a
//! real machine can show is that those pieces are **wired to each other and
//! to the volume**: that a right-click on a library row reaches the session
//! holding the authority, that the session's write lands on a home shaped
//! like a real one, that the bar the compositor scans out gains a slot the
//! user can hit, and that hitting it spawns the bundle the pin names,
//! through the ordinary capability-checked load path.
//!
//! So the guest boots the **production** aarch64 pipeline
//! (`boot_aarch64::boot`) against a planted encrypted root carrying the
//! signed input and display driver bundles, and the host drives the desktop
//! blind through the QEMU monitor: unlock, log in, start the desktop, then
//! Library → right-click the entry → *Pin to taskbar* → dismiss → the
//! Switchboard capsule → the new pin. Only the audit sink is swapped, for
//! the PASS witnesses below.
//!
//! # The PASS gate
//!
//! Three latches, each attributable to exactly one act:
//!
//! 1. **The pin was persisted.** The session's file writer creates the pin
//!    store's parent before writing it, and a provisioned home carries only
//!    the five fixed top-level subdirectories (`tairix_users::HOME_SUBDIRS`)
//!    — no `Settings/Taskbar`. So the audited `mkdir` of a directory named
//!    [`PINS_SETTINGS_SUBDIR`](tairix_taskpins::PINS_SETTINGS_SUBDIR) is the
//!    pin store coming into existence on the volume, and nothing else in the
//!    system creates it.
//! 2. **The Switchboard panel was created and painted.** The reserved window
//!    endpoint has served
//!    [`SWITCHBOARD_WINDOW_CALLS`](tairix_test_taskbar_pin_qemu_aarch64::SWITCHBOARD_WINDOW_CALLS)
//!    replies — the create round-trip, then the first present. This vertical
//!    opens exactly one window, so the count is a position in one client's
//!    own call order, not a tally of unrelated traffic. Reaching it also
//!    emits
//!    [`SWITCHBOARD_PANEL_MARKER`](tairix_test_taskbar_pin_qemu_aarch64::SWITCHBOARD_PANEL_MARKER),
//!    which is what the host holds the pin click behind.
//! 3. **The pin launched its application.** An `APP_LOADED` record naming
//!    the pinned bundle. The bundle is one no other stage launches, so the
//!    record cannot be anybody else's load; `PROCESS_SPAWNED` would not do,
//!    as it carries only an entry address and can attribute no bundle.
//!
//! A panic before all three parks the CPU, the guest falls silent, and the
//! runner reports a timeout — loud failure, never a false pass.

#![cfg_attr(itest_aarch64, no_std)]
#![cfg_attr(itest_aarch64, no_main)]
#![deny(missing_docs)]

// --- Freestanding test bin (`aarch64-unknown-none`) ----------------

#[cfg(itest_aarch64)]
mod kernel {
    use core::panic::PanicInfo;
    use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

    use tairix_arch_aarch64::{handle_panic_via_serial, qemu_exit, SerialSink, SERIAL_SINK};
    use tairix_kalloc::{FreeListAllocator, Heap, HEAP_BYTES};
    use tairix_kernel::aarch64::boot as boot_aarch64;
    use tairix_kernel_core::AuditEvent;
    use tairix_log::{Event, Sink};
    use tairix_test_taskbar_pin_qemu_aarch64::{
        PIN_APP_NAME, SWITCHBOARD_PANEL_MARKER, SWITCHBOARD_WINDOW_CALLS,
    };
    use tairix_util::fmt::format_hex_u64;

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
    /// three PASS witnesses described in the module docs.
    struct TaskbarPinSink {
        /// The pin store's directory was created on the volume.
        pin_store_created: AtomicBool,
        /// Replies the reserved window endpoint has served.
        window_calls: AtomicU32,
        /// The Switchboard panel was created and painted, and the host was
        /// told so exactly once.
        panel_presented: AtomicBool,
        /// The pinned bundle was loaded — the pin launched its application.
        pin_launched: AtomicBool,
    }

    impl TaskbarPinSink {
        /// A sink with no witness latched.
        const fn new() -> Self {
            Self {
                pin_store_created: AtomicBool::new(false),
                window_calls: AtomicU32::new(0),
                panel_presented: AtomicBool::new(false),
                pin_launched: AtomicBool::new(false),
            }
        }

        /// Latch the pin-store witness from a successful filesystem
        /// mutation: a `mkdir` whose target's last component is the pin
        /// store's own directory name. Any other operation, or a directory
        /// that merely ends in those letters without a separator before
        /// them, leaves the latch alone (fail closed — a near miss can
        /// never satisfy PASS).
        fn note_fs_mutation(&self, event: &Event<'_>) {
            let mut is_mkdir = false;
            let mut is_pin_store = false;
            for field in event.fields {
                let tairix_log::FieldValue::Str(value) = field.value else {
                    continue;
                };
                match field.key {
                    "op" => is_mkdir = value == "mkdir",
                    "path" => is_pin_store = is_pin_store_dir(value),
                    _ => {}
                }
            }
            if is_mkdir && is_pin_store {
                self.pin_store_created.store(true, Ordering::Release);
            }
        }

        /// Count a reply served on the reserved window endpoint — compared
        /// against the exact hex spelling the kernel/ipc audit fields render
        /// (`format_hex_u64`), so the match can neither false-positive on
        /// another endpoint nor drift from the emitter — and announce the
        /// painted panel to the host on the reply that completes it.
        fn note_call_replied(&self, event: &Event<'_>) {
            let mut expected = [0u8; 16];
            let expected = format_hex_u64(tairix_abi::window_ipc::WINDOW_ENDPOINT, &mut expected);
            for field in event.fields {
                if field.key != "endpoint" {
                    continue;
                }
                let tairix_log::FieldValue::Str(value) = field.value else {
                    continue;
                };
                if value != expected {
                    continue;
                }
                let served = self.window_calls.fetch_add(1, Ordering::AcqRel) + 1;
                if served >= SWITCHBOARD_WINDOW_CALLS
                    && !self.panel_presented.swap(true, Ordering::AcqRel)
                {
                    emit_marker(SWITCHBOARD_PANEL_MARKER);
                }
            }
        }

        /// Latch the launch witness from an `APP_LOADED` record naming the
        /// pinned bundle.
        fn note_bundle_loaded(&self, event: &Event<'_>) {
            for field in event.fields {
                if field.key != "bundle" {
                    continue;
                }
                if let tairix_log::FieldValue::Str(value) = field.value {
                    if is_pinned_bundle(value) {
                        self.pin_launched.store(true, Ordering::Release);
                    }
                }
            }
        }

        /// Whether every witness is in.
        fn passed(&self) -> bool {
            self.pin_store_created.load(Ordering::Acquire)
                && self.panel_presented.load(Ordering::Acquire)
                && self.pin_launched.load(Ordering::Acquire)
        }
    }

    impl Sink for TaskbarPinSink {
        fn write_event(&self, event: &Event<'_>) {
            // Replay through the serial sink first, so the QEMU transcript
            // records the full boot → unlock → desktop → pin timeline and
            // the host can gate its injection on it.
            SerialSink::new().write_event(event);
            if event.id.0 == AuditEvent::FsNodeMutated.id().0 {
                self.note_fs_mutation(event);
            } else if event.id.0 == tairix_kernel_ipc::AuditEvent::CallReplied.id().0 {
                self.note_call_replied(event);
            } else if event.id.0 == tairix_appload::events::APP_LOADED.0 {
                self.note_bundle_loaded(event);
            } else {
                return;
            }
            if self.passed() {
                qemu_exit::exit_success();
            }
        }
    }

    /// Whether `path` names the per-user pin store's own directory: its last
    /// component is the store's directory name, spelled once by the store
    /// engine. A provisioned home carries no such directory, so the audited
    /// `mkdir` that creates it is the pin store being written for the first
    /// time.
    fn is_pin_store_dir(path: &str) -> bool {
        path.strip_suffix(tairix_taskpins::PINS_SETTINGS_SUBDIR)
            .is_some_and(|parent| parent.ends_with('/'))
    }

    /// Whether `bundle` is the pinned application's bundle in the system
    /// application store, composed from the shared `lib/abi` spellings
    /// rather than written out as a path.
    fn is_pinned_bundle(bundle: &str) -> bool {
        bundle
            .strip_prefix(tairix_abi::SYSTEM_APPLICATION_STORE)
            .and_then(|rest| rest.strip_prefix('/'))
            .and_then(|name| name.strip_suffix(tairix_abi::BUNDLE_SUFFIX))
            .is_some_and(|name| name == PIN_APP_NAME)
    }

    /// Write one bare marker line to serial for the host runner to gate on.
    fn emit_marker(message: &str) {
        SerialSink::new().write_event(&Event {
            level: tairix_log::Level::Info,
            id: tairix_log::EventId(0),
            message,
            fields: &[],
        });
    }

    /// The audit observer the boot pipeline is handed.
    static AUDIT_SINK: TaskbarPinSink = TaskbarPinSink::new();

    /// Forward to the shared aarch64 panic bridge. A panic before the PASS
    /// finisher parks the CPU, the run times out, and the harness reports
    /// `Outcome::Timeout` — the documented fail-loud behaviour.
    #[panic_handler]
    fn tairix_taskbar_pin_qemu_aarch64_panic(info: &PanicInfo<'_>) -> ! {
        handle_panic_via_serial(info)
    }

    /// Boot entry point — the symbol the arch crate's `boot.s` trampoline
    /// calls (via `tairix_arch_aarch64_main`).
    ///
    /// QEMU hands no DTB pointer (`_dtb == 0`), so the embedded `virt`
    /// blob's address is forwarded to the production boot pipeline with the
    /// audit observer in place.
    #[no_mangle]
    pub extern "C" fn kernel_main(_dtb: u64) -> ! {
        let dtb = DTB_BLOB.as_ptr() as u64;
        boot_aarch64::boot(
            dtb,
            &SERIAL_SINK,
            &AUDIT_SINK,
            // `SyscallInvoked` (`EventId(5000)`) is `Debug`, below the
            // default `Info` filter; the host waits for that record's
            // `sc=irq_bind` marker before typing the unlock passphrase, so
            // boot with the filter lowered.
            tairix_log::Level::Debug,
            &tairix_kernel::hwtree_store::HW_TREE_SOURCE,
        )
    }
}

// --- Host stub -----------------------------------------------------
#[cfg(not(itest_aarch64))]
fn main() {}
