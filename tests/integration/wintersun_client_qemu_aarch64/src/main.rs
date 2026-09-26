//! QEMU integration vertical: **launch `WinterSun` on its reference scene from
//! a terminal and read its window back in every size state**
//! (`plans/WINTERSUN.md` WS23).
//!
//! # What this proves that no host test can
//!
//! The reference scene is host-tested to the bit, and the window manager's
//! size states, the session's witnesses and the compositor's presentation
//! each have their own suites. What only a running machine can show is that
//! the installed bundle is complete and launchable by name from a shell, that
//! its window reaches the screen holding exactly the scene the host draws, and
//! that fullscreen, restore and maximise each put exactly that picture where
//! the window manager says the window is.
//!
//! So the guest boots the **production** aarch64 pipeline
//! (`boot_aarch64::boot`) against a planted encrypted root carrying the signed
//! input and display driver bundles and the complete app store, and the host
//! drives the desktop blind through the QEMU monitor: unlock, log in, start the
//! desktop, open a terminal from the program library, type
//! [`COMMAND_LINE`], photograph the window, press `F11`, `Escape` and the
//! title bar's size toggle, photograph each state, and finally press the
//! title bar's close control. Only the audit sink is swapped, for the PASS
//! witnesses below.
//!
//! # The PASS gate
//!
//! Three latches, in order, each attributable to exactly one act:
//!
//! 1. **The game launched.** An `APP_LOADED` record naming the game's bundle
//!    in the system application store — the shell resolving the typed word.
//! 2. **Its window opened.** A `WINDOW_CREATE_REPLY_LEN` reply on the
//!    reserved window endpoint after that load. The game opens one window,
//!    and the terminal's was created before the game was loaded.
//! 3. **It left cleanly.** An `APP_LOADED` record naming [`THEN_COMMAND`]'s
//!    bundle after that create: the typed line joins the two with `&&`, so
//!    the shell loads it only once the game exited with status zero.
//!
//! # Why the guest cannot exit early
//!
//! The game leaves only when its window is closed, and the runner sends no
//! pointer step while a screendump it has asked for is still being taken, so
//! the last latch cannot land before the last dump is on disk.
//!
//! A panic before all three latches parks the CPU, the guest falls silent,
//! and the runner reports a timeout — loud failure, never a false pass.

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
    use tairix_log::{Event, FieldValue, Sink};
    use tairix_test_wintersun_client_qemu_aarch64::{is_bundle, GAME_APP_NAME, THEN_COMMAND};
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
    /// three PASS witnesses described in the module docs.
    struct ClientSink {
        /// The game's bundle was loaded.
        launched: AtomicBool,
        /// Its window was created after that.
        window_opened: AtomicBool,
        /// The witness command was loaded after that.
        left_cleanly: AtomicBool,
    }

    impl ClientSink {
        /// A sink with no witness latched.
        const fn new() -> Self {
            Self {
                launched: AtomicBool::new(false),
                window_opened: AtomicBool::new(false),
                left_cleanly: AtomicBool::new(false),
            }
        }

        /// Latch the launch witness, then the clean-exit one, from the
        /// bundle an `APP_LOADED` record names. Each is composed from the
        /// shared `lib/abi` store spellings, and the second is honoured only
        /// once the game's window exists, so an earlier `true` cannot pass
        /// for it.
        fn note_bundle_loaded(&self, event: &Event<'_>) {
            let Some(bundle) = str_field(event, "bundle") else {
                return;
            };
            let suffix = tairix_abi::BUNDLE_SUFFIX;
            if is_bundle(
                bundle,
                tairix_abi::SYSTEM_APPLICATION_STORE,
                GAME_APP_NAME,
                suffix,
            ) {
                self.launched.store(true, Ordering::Release);
            } else if self.window_opened.load(Ordering::Acquire)
                && is_bundle(
                    bundle,
                    tairix_abi::SYSTEM_COMMAND_STORE,
                    THEN_COMMAND,
                    suffix,
                )
            {
                self.left_cleanly.store(true, Ordering::Release);
            }
        }

        /// Latch the window witness from a create reply on the reserved
        /// window endpoint, once the game has loaded.
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
    }

    impl Sink for ClientSink {
        fn write_event(&self, event: &Event<'_>) {
            // Replay through the serial sink first, so the QEMU transcript
            // records the full boot → unlock → desktop → game timeline and the
            // host can gate its injection on it.
            SerialSink::new().write_event(event);
            if event.id.0 == tairix_appload::events::APP_LOADED.0 {
                self.note_bundle_loaded(event);
            } else if event.id.0 == tairix_kernel_ipc::AuditEvent::CallReplied.id().0 {
                self.note_call_replied(event);
            } else {
                return;
            }
            if self.left_cleanly.load(Ordering::Acquire) {
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

    /// The audit observer the boot pipeline is handed.
    static AUDIT_SINK: ClientSink = ClientSink::new();

    /// Forward to the shared aarch64 panic bridge. A panic before the PASS
    /// finisher parks the CPU, the run times out, and the harness reports
    /// `Outcome::Timeout` — the documented fail-loud behaviour.
    #[panic_handler]
    fn tairix_wintersun_client_qemu_aarch64_panic(info: &PanicInfo<'_>) -> ! {
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
