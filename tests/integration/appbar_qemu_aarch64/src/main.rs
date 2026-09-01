//! QEMU integration vertical: **launch an application that declares an
//! icon-bar presence, choose a row of the menu it declared from its bar
//! slot, and take that slot's declared default action**
//! (`plans/NEW-TASKBAR.md`).
//!
//! # What this proves that no host test can
//!
//! Every piece of icon-bar *logic* — the bounded menu model and its wire
//! encoding, the strip's layout and hit-testing, the menu's rows and marks,
//! the session's grouping of windows under their attested owner — is already
//! covered by host unit tests. What only a real machine can show is that
//! those pieces are **wired to each other and to the running application**:
//! that an application's `SetAppBar` declaration reaches the session holding
//! the authority, that the session gives its process a slot on the bar the
//! compositor scans out, that a right-click there opens the menu *that
//! application* declared, that choosing a row is delivered back to the
//! declaring process, and that a primary click there is delivered too
//! because the declaration said the application handles it.
//!
//! So the guest boots the **production** aarch64 pipeline
//! (`boot_aarch64::boot`) against a planted encrypted root carrying the
//! signed input and display driver bundles, and the host drives the desktop
//! blind through the QEMU monitor: unlock, log in, start the desktop, then
//! Library → the terminal's row → right-click its new bar slot → *New
//! window* → primary-click that same slot. Only the audit sink is swapped,
//! for the PASS witnesses below.
//!
//! # The PASS gate
//!
//! Two latches, each attributable to exactly one act:
//!
//! 1. **The application launched.** An `APP_LOADED` record naming the
//!    bundle [`BAR_APP_NAME`] spells — the library row's own launch.
//! 2. **Every window the script asks for was opened.**
//!    [`WINDOWS_OPENED`] create replies were served on the reserved window
//!    endpoint, recognised by the distinctive wire length that is unique to
//!    a create among that endpoint's replies. The application is the only
//!    client in this world that creates a window, so the three are its
//!    launch window, the *New window* row it was handed, and its slot's
//!    declared default action — one per act.
//!
//! The second latch attests the declaration itself, end to end and more
//! strongly than any reply-shape witness could: the session refuses to open
//! a menu for an application that declared none, it addresses a chosen row
//! only through the route the declaration recorded, and it delivers a
//! primary click to the application only where the declaration claimed it.
//! So the later windows prove the declaration was accepted, that the slot
//! was the declaring process's, and that both outcomes were delivered to it.
//!
//! # Why the guest latches no frame
//!
//! Reading the screen is the host's job and is gated on the *session's* own
//! `WINDOW_SHOWN` announcement, because only the session knows when a served
//! window's pixels reached the display. The guest deliberately claims
//! nothing about frames: on this endpoint a present, a blur change, a
//! retitle and a declaration all answer with the same four-byte status
//! reply, so recognising a present here would be a guess about how many
//! requests the application makes rather than a fact.
//!
//! # Why the guest cannot exit early
//!
//! The last window is opened by the script's final click, and the runner
//! sends no pointer step until every screendump it has already asked for has
//! been taken and parsed. So the create that completes the PASS cannot
//! happen until the last dump is safely on disk, and the guest can never
//! exit out from under the evidence.
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
    use tairix_test_appbar_qemu_aarch64::{BAR_APP_NAME, WINDOWS_OPENED};
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
    struct AppBarSink {
        /// The declaring application's bundle was loaded.
        app_launched: AtomicBool,
        /// Window creates served on the reserved window endpoint.
        windows_opened: AtomicU32,
    }

    impl AppBarSink {
        /// A sink with no witness latched.
        const fn new() -> Self {
            Self {
                app_launched: AtomicBool::new(false),
                windows_opened: AtomicU32::new(0),
            }
        }

        /// Count a window create served on the reserved window endpoint.
        ///
        /// The endpoint is matched against the exact hex spelling the
        /// kernel/ipc audit fields render (`format_hex_u64`), so the match
        /// can neither false-positive on another endpoint nor drift from the
        /// emitter. A create is then recognised by its reply's wire length,
        /// which is unique among this endpoint's replies — every other
        /// request on it answers with a four-byte status or the
        /// desktop-query record. Nothing else about a reply is read: its
        /// ordinal belongs to no client in particular, because the
        /// rendezvous is shared.
        fn note_call_replied(&self, event: &Event<'_>) {
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
        /// declaring application's bundle.
        fn note_bundle_loaded(&self, event: &Event<'_>) {
            for field in event.fields {
                if field.key != "bundle" {
                    continue;
                }
                if let tairix_log::FieldValue::Str(value) = field.value {
                    if is_bar_bundle(value) {
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

    impl Sink for AppBarSink {
        fn write_event(&self, event: &Event<'_>) {
            // Replay through the serial sink first, so the QEMU transcript
            // records the full boot → unlock → desktop → icon-bar timeline
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

    /// Whether `bundle` is the declaring application's bundle in the system
    /// application store, composed from the shared `lib/abi` spellings
    /// rather than written out as a path.
    fn is_bar_bundle(bundle: &str) -> bool {
        bundle
            .strip_prefix(tairix_abi::SYSTEM_APPLICATION_STORE)
            .and_then(|rest| rest.strip_prefix('/'))
            .and_then(|name| name.strip_suffix(tairix_abi::BUNDLE_SUFFIX))
            .is_some_and(|name| name == BAR_APP_NAME)
    }

    /// The audit observer the boot pipeline is handed.
    static AUDIT_SINK: AppBarSink = AppBarSink::new();

    /// Forward to the shared aarch64 panic bridge. A panic before the PASS
    /// finisher parks the CPU, the run times out, and the harness reports
    /// `Outcome::Timeout` — the documented fail-loud behaviour.
    #[panic_handler]
    fn tairix_appbar_qemu_aarch64_panic(info: &PanicInfo<'_>) -> ! {
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
