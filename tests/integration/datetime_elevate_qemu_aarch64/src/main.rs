//! QEMU integration vertical: **open the taskbar clock's *Set Date & Time…*
//! row, authenticate through the elevation broker, and witness the Date &
//! Time window on screen**.
//!
//! # What this proves that no host test can
//!
//! Every piece of elevation *logic* — the credential prompt, the broker's
//! re-authentication, the Launch reply — is already covered by host unit
//! tests. What only a real machine can show is that those pieces are
//! **wired to each other and to the running application**: that the
//! session's `SetDateTime` outcome opens the prompt, that the broker starts
//! `/System/Applications/datetime.app/Run` as the authenticated account,
//! and that the application creates a window the desktop session serves.
//!
//! So the guest boots the **production** aarch64 pipeline
//! (`boot_aarch64::boot`) against a planted encrypted root, and the host
//! drives the desktop blind through the QEMU monitor: unlock, log in, start
//! the desktop, right-click the clock, choose *Set Date & Time…*, type the
//! fixture account into the prompt. Only the audit sink is swapped, for the
//! PASS witnesses below.
//!
//! # The PASS gate
//!
//! Two latches, each attributable to exactly one act:
//!
//! 1. **The application launched.** An `APP_LOADED` record naming the
//!    Date & Time bundle.
//! 2. **Its window was opened.** [`WINDOWS_OPENED`] create replies were
//!    served on the reserved window endpoint, recognised by the distinctive
//!    wire length unique to a create among that endpoint's replies.
//!
//! # Why the guest cannot exit early
//!
//! The window is opened by the elevated program after the credentials are
//! typed, and the runner sends no keystroke until the pointer script has
//! opened the prompt. So the create that completes the PASS cannot happen
//! until the prompt is up and the credentials are offered.
//!
//! A panic before both latches parks the CPU, the guest falls silent, and
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
    use tairix_log::{Event, Sink};
    use tairix_test_datetime_elevate_qemu_aarch64::{DATETIME_APP_NAME, WINDOWS_OPENED};
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
    /// two PASS witnesses described in the module docs.
    struct DateTimeElevateSink {
        /// The elevated application's bundle was loaded.
        app_launched: AtomicBool,
        /// Window creates served on the reserved window endpoint.
        windows_opened: AtomicU32,
    }

    impl DateTimeElevateSink {
        /// A sink with no witness latched.
        const fn new() -> Self {
            Self {
                app_launched: AtomicBool::new(false),
                windows_opened: AtomicU32::new(0),
            }
        }

        /// Count a window create served on the reserved window endpoint.
        ///
        /// Only creates that land **after** the elevated Date & Time bundle
        /// has loaded are counted: the autostarted file manager also creates
        /// a window on this endpoint at desktop bring-up, and counting it
        /// would complete the PASS before the elevated program ever ran.
        fn note_call_replied(&self, event: &Event<'_>) {
            if !self.app_launched.load(Ordering::Acquire) {
                return;
            }
            let mut endpoint_hex = [0u8; 16];
            let expected =
                format_hex_u64(tairix_abi::window_ipc::WINDOW_ENDPOINT, &mut endpoint_hex);
            let mut on_window_endpoint = false;
            let mut reply_len = 0usize;
            for field in event.fields {
                let tairix_log::FieldValue::Str(value) = field.value else {
                    continue;
                };
                match field.key {
                    "endpoint" => on_window_endpoint = value == expected,
                    // An unparsable length stays zero, matching no reply
                    // length and counting nothing (fail closed).
                    "len" => {
                        reply_len = usize::try_from(
                            tairix_util::count::parse_decimal(value).unwrap_or_default(),
                        )
                        .unwrap_or_default();
                    }
                    _ => {}
                }
            }
            if on_window_endpoint && reply_len == tairix_abi::window_ipc::WINDOW_CREATE_REPLY_LEN {
                self.windows_opened.fetch_add(1, Ordering::AcqRel);
            }
        }

        /// Latch the launch witness from an `APP_LOADED` record naming the
        /// elevated application's bundle.
        fn note_bundle_loaded(&self, event: &Event<'_>) {
            for field in event.fields {
                if field.key != "bundle" {
                    continue;
                }
                if let tairix_log::FieldValue::Str(value) = field.value {
                    if is_datetime_bundle(value) {
                        self.app_launched.store(true, Ordering::Release);
                    }
                }
            }
        }

        /// Whether every witness is in.
        fn passed(&self) -> bool {
            self.app_launched.load(Ordering::Acquire)
                && self.windows_opened.load(Ordering::Acquire) >= WINDOWS_OPENED
        }
    }

    impl Sink for DateTimeElevateSink {
        fn write_event(&self, event: &Event<'_>) {
            // Replay through the serial sink first, so the QEMU transcript
            // records the full boot → unlock → desktop → elevate timeline
            // and the host can gate its injection on it.
            SerialSink::new().write_event(event);
            if event.id.0 == tairix_kernel_ipc::AuditEvent::CallReplied.id().0 {
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

    /// Whether `bundle` is the Date & Time bundle in the system application
    /// store, composed from the shared `lib/abi` spellings rather than
    /// written out as a path.
    fn is_datetime_bundle(bundle: &str) -> bool {
        bundle
            .strip_prefix(tairix_abi::SYSTEM_APPLICATION_STORE)
            .and_then(|rest| rest.strip_prefix('/'))
            .and_then(|name| name.strip_suffix(tairix_abi::BUNDLE_SUFFIX))
            .is_some_and(|name| name == DATETIME_APP_NAME)
    }

    /// The audit observer the boot pipeline is handed.
    static AUDIT_SINK: DateTimeElevateSink = DateTimeElevateSink::new();

    /// Forward to the shared aarch64 panic bridge. A panic before the PASS
    /// finisher parks the CPU, the run times out, and the harness reports
    /// `Outcome::Timeout` — the documented fail-loud behaviour.
    #[panic_handler]
    fn tairix_datetime_elevate_qemu_aarch64_panic(info: &PanicInfo<'_>) -> ! {
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
