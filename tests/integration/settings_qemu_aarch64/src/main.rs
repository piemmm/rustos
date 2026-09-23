//! QEMU integration vertical: **open Settings from the system quick-actions
//! menu, walk its sidebar, and change the desktop's appearance through both
//! routes to it** (`plans/NEW-DESKTOP-SETTINGS.md` DS13).
//!
//! # What this proves that no host test can
//!
//! Every pane is host-tested: the registry, the strip and its scroll, the
//! forms, the volume cards, the absence statements, and the apply that asks
//! the session to adopt a change. What only a running machine can show is that
//! they are wired to each other and to the desktop — that the capsule's menu
//! launches the installed bundle, that a press on a strip row reaches the
//! window and moves it to the pane drawn there, that the pane's frame reaches
//! the screen, and that choosing an appearance becomes a durable change the
//! desktop redraws itself in.
//!
//! So the guest boots the **production** aarch64 pipeline
//! (`boot_aarch64::boot`) against a planted encrypted root carrying the signed
//! input and display driver bundles and the complete app store, and the host
//! drives the desktop blind through the QEMU monitor: unlock, log in, start
//! the desktop, open the capsule's menu, choose *Settings…*, photograph the
//! window on General, walk to a pane that states an absence, scroll the strip,
//! walk to Storage, walk to Appearance and choose Light, photograph the
//! desktop redrawn light, and finally choose *Dark Appearance* from the
//! capsule's menu. Only the audit sink is swapped, for the PASS witnesses
//! below.
//!
//! # The PASS gate
//!
//! Four latches, in order, each attributable to exactly one act:
//!
//! 1. **Settings launched.** An `APP_LOADED` record naming the bundle
//!    [`SETTINGS_APP_NAME`] spells — the menu row's own launch.
//! 2. **Its window opened.** A `WINDOW_CREATE_REPLY_LEN` reply on the
//!    reserved window endpoint after that load. Settings opens one window,
//!    and the desktop's own surfaces never call the window channel.
//! 3. **The pane's choice became durable.** A rename replacing the desktop's
//!    published settings document after that create: the app-data service
//!    commits a document by writing it whole and renaming it over the live
//!    one, and only an adopted appearance change rewrites it in this run.
//! 4. **The menu's choice did too.** A second such rename — the system
//!    menu's *Dark Appearance* row, which reaches the document through the
//!    same persist-then-adopt path the pane does, so a row that re-themed the
//!    screen without writing anything would leave this latch unmet.
//!
//! Counting renames of that one path is attributable because the path is the
//! desktop's alone and nothing else in this world rewrites it.
//!
//! # Why the guest cannot exit early
//!
//! The last dump photographs the desktop after the pane's choice, and the
//! menu row that completes the PASS is chosen only after the runner has read
//! that dump back: the runner sends no pointer step until every screendump it
//! has asked for is on disk and parsed.
//!
//! A panic before all four latches parks the CPU, the guest falls silent, and
//! the runner reports a timeout — loud failure, never a false pass.

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
    use tairix_log::{Event, FieldValue, Sink};
    use tairix_test_settings_qemu_aarch64::{
        is_desktop_document, APPEARANCE_CHANGES, RENAME_OP, SETTINGS_APP_NAME,
    };
    use tairix_util::fmt::format_hex_u64;

    // The canonical QEMU `virt` device tree, dumped and embedded at build
    // time (`build.rs`). The boot pipeline discovers the board from it
    // because QEMU passes no `x0` DTB pointer at an ELF `-kernel` entry.
    include!(concat!(env!("OUT_DIR"), "/dtb_fixture.rs"));

    /// Static boot heap, mirroring the production aarch64 kernel binary's
    /// `.bss`-resident heap (zeroed by the boot trampoline).
    static HEAP: Heap = Heap::ZERO;

    /// Global allocator backed by [`HEAP`].
    ///
    /// SAFETY: the page-aligned `HEAP` static outlives the binary and the
    /// allocator is its only consumer.
    #[global_allocator]
    static ALLOCATOR: FreeListAllocator =
        unsafe { FreeListAllocator::new(HEAP.as_mut_ptr(), HEAP_BYTES) };

    /// Audit observer that replays the whole trail to serial and latches the
    /// four PASS witnesses described in the module docs.
    struct SettingsSink {
        /// The settings bundle was loaded.
        launched: AtomicBool,
        /// Its window was created after that.
        window_opened: AtomicBool,
        /// Commits of the desktop's published document after that.
        committed: AtomicU32,
    }

    impl SettingsSink {
        /// A sink with no witness latched.
        const fn new() -> Self {
            Self {
                launched: AtomicBool::new(false),
                window_opened: AtomicBool::new(false),
                committed: AtomicU32::new(0),
            }
        }

        /// Latch the launch witness from an `APP_LOADED` record naming the
        /// settings bundle.
        fn note_bundle_loaded(&self, event: &Event<'_>) {
            if str_field(event, "bundle").is_some_and(is_settings_bundle) {
                self.launched.store(true, Ordering::Release);
            }
        }

        /// Latch the window witness from a create reply on the reserved
        /// window endpoint, once the bundle has loaded.
        ///
        /// The endpoint is matched against the exact hex spelling the
        /// kernel/ipc audit fields render (`format_hex_u64`), and the
        /// operation by its reply's wire length; nothing else about a reply
        /// is read, because the rendezvous is shared.
        fn note_call_replied(&self, event: &Event<'_>) {
            if !self.launched.load(Ordering::Acquire) {
                return;
            }
            let mut endpoint_hex = [0u8; 16];
            let expected =
                format_hex_u64(tairix_abi::window_ipc::WINDOW_ENDPOINT, &mut endpoint_hex);
            if str_field(event, "endpoint") != Some(expected) {
                return;
            }
            // An unparsable length stays zero, matching no reply length and
            // latching nothing (fail closed).
            let reply_len = str_field(event, "len")
                .and_then(tairix_util::count::parse_decimal)
                .and_then(|len| usize::try_from(len).ok())
                .unwrap_or_default();
            if reply_len == tairix_abi::window_ipc::WINDOW_CREATE_REPLY_LEN {
                self.window_opened.store(true, Ordering::Release);
            }
        }

        /// Count a commit of the desktop's published document, once the
        /// window is open.
        ///
        /// The operation and the path it replaced are read from the record's
        /// own fields, so a mutation of anything else matches nothing (fail
        /// closed).
        fn note_mutation(&self, event: &Event<'_>) {
            if !self.window_opened.load(Ordering::Acquire) {
                return;
            }
            if str_field(event, "op") == Some(RENAME_OP)
                && str_field(event, "to").is_some_and(is_desktop_document)
            {
                self.committed.fetch_add(1, Ordering::AcqRel);
            }
        }

        /// Whether every witness is in.
        fn passed(&self) -> bool {
            self.committed.load(Ordering::Acquire) >= APPEARANCE_CHANGES
        }
    }

    impl Sink for SettingsSink {
        fn write_event(&self, event: &Event<'_>) {
            // Replay through the serial sink first, so the QEMU transcript
            // records the full boot → unlock → desktop → Settings timeline and
            // the host can gate its injection on it.
            SerialSink::new().write_event(event);
            if event.id.0 == tairix_appload::events::APP_LOADED.0 {
                self.note_bundle_loaded(event);
            } else if event.id.0 == tairix_kernel_ipc::AuditEvent::CallReplied.id().0 {
                self.note_call_replied(event);
            } else if event.id.0 == tairix_kernel_core::audit::AuditEvent::FsNodeMutated.id().0 {
                self.note_mutation(event);
            } else {
                return;
            }
            if self.passed() {
                qemu_exit::exit_success();
            }
        }
    }

    /// The string value `event` carries under `key`, if it carries one.
    fn str_field<'e>(event: &Event<'e>, key: &str) -> Option<&'e str> {
        event.fields.iter().find_map(|field| match field.value {
            FieldValue::Str(value) if field.key == key => Some(value),
            _ => None,
        })
    }

    /// Whether `bundle` is the settings bundle in the system application
    /// store, composed from the shared `lib/abi` spellings rather than written
    /// out as a path.
    fn is_settings_bundle(bundle: &str) -> bool {
        bundle
            .strip_prefix(tairix_abi::SYSTEM_APPLICATION_STORE)
            .and_then(|rest| rest.strip_prefix('/'))
            .and_then(|name| name.strip_suffix(tairix_abi::BUNDLE_SUFFIX))
            .is_some_and(|name| name == SETTINGS_APP_NAME)
    }

    /// The audit observer the boot pipeline is handed.
    static AUDIT_SINK: SettingsSink = SettingsSink::new();

    /// Forward to the shared aarch64 panic bridge. A panic before the PASS
    /// finisher parks the CPU, the run times out, and the harness reports
    /// `Outcome::Timeout` — the documented fail-loud behaviour.
    #[panic_handler]
    fn tairix_settings_qemu_aarch64_panic(info: &PanicInfo<'_>) -> ! {
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
