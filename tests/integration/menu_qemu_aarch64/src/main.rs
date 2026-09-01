//! QEMU integration vertical: **open an application's window menu, see the
//! desktop's plate on screen, and choose a row** (`plans/NEW-MENUS.md` D17).
//!
//! # What this proves that no host test can
//!
//! Every piece of menu *logic* is already covered by host unit tests: the
//! bounded wire model and its codec, the chain's plates, placement, grab,
//! traversal, dismissal and lifetime, the terminal's row model and its
//! id↔command inverse. What only a real machine can show is that those pieces
//! are **wired to each other and to the running application** — that a
//! secondary press on a client reaches the application as a pointer event,
//! that the `OpenMenu` it sends is served and accepted, that the session
//! actually *draws* a plate where the chain says it is, that a click on a row
//! is routed into the chain rather than to whatever is behind it, and that the
//! one `MenuClosed` answer reaches the application and runs the row's command.
//!
//! Before this vertical nothing on the system had ever put a plate on a screen.
//!
//! So the guest boots the **production** aarch64 pipeline
//! (`boot_aarch64::boot`) against a planted encrypted root carrying the signed
//! input and display driver bundles, and the host drives the desktop blind
//! through the QEMU monitor: unlock, log in, start the desktop, launch the
//! terminal from the program library, right-click its client, photograph the
//! plate, and click the *Settings…* row. Only the audit sink is swapped, for
//! the PASS witnesses below.
//!
//! # The PASS gate
//!
//! Three latches, in order, each attributable to exactly one act:
//!
//! 1. **The application launched.** An `APP_LOADED` record naming the bundle
//!    [`MENU_APP_NAME`] spells — the library row's own launch.
//! 2. **A menu was asked for and served.** A 12-byte
//!    `WINDOW_MINTED_ID_REPLY_LEN` reply on the reserved window endpoint,
//!    which is the reply of exactly one operation: `OpenMenu`.
//! 3. **A row's command reached the application.** A `WINDOW_CREATE_REPLY_LEN`
//!    reply observed *after* that one — the settings sheet the chosen
//!    *Settings…* row opens. The application is the only client in this world
//!    that creates a window (the desktop's own surfaces, the chain's plates
//!    included, are session-painted compositor windows that never call the
//!    window channel), and it opens that sheet on nothing but the
//!    `MenuOutcome::Chosen` naming that row. So the third latch attests the
//!    whole round trip: the press reached the client, the open was accepted, a
//!    chain came up, the click hit the row the chain had drawn, and the one
//!    answer the desktop owes was delivered back.
//!
//! Ordering is what makes each latch attributable. Counting creates from boot
//! would already be satisfied by the terminal's own launch window, so a create
//! only counts once the menu reply has been seen.
//!
//! # Why the guest latches no frame
//!
//! Reading the screen is the host's job and is gated on the *session's* own
//! `WINDOW_SHOWN` and `MENU_SHOWN` announcements, because only the session
//! knows when a window's or a plate's pixels reached the display. Those records
//! are emitted by userland through `lib/log`, which reaches the kernel's
//! diagnostic sink and the serial line — not the audit sink this guest
//! installs — so the guest could not see them even if it wanted to.
//!
//! # Why the guest cannot exit early
//!
//! The sheet is opened by the script's final click, and the runner sends no
//! pointer step until every screendump it has already asked for has been taken
//! and parsed. So the create that completes the PASS cannot happen until the
//! plate's dump is safely on disk, and the guest can never exit out from under
//! the evidence.
//!
//! A panic before all three latches parks the CPU, the guest falls silent, and
//! the runner reports a timeout — loud failure, never a false pass.

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
    use tairix_test_menu_qemu_aarch64::MENU_APP_NAME;
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
    struct MenuSink {
        /// The application's bundle was loaded.
        app_launched: AtomicBool,
        /// An `OpenMenu` was served on the reserved window endpoint.
        menu_asked: AtomicBool,
        /// A window was created after that — the sheet the chosen row opened.
        sheet_opened: AtomicBool,
    }

    impl MenuSink {
        /// A sink with no witness latched.
        const fn new() -> Self {
            Self {
                app_launched: AtomicBool::new(false),
                menu_asked: AtomicBool::new(false),
                sheet_opened: AtomicBool::new(false),
            }
        }

        /// Latch what one served reply on the reserved window endpoint says.
        ///
        /// The endpoint is matched against the exact hex spelling the
        /// kernel/ipc audit fields render (`format_hex_u64`), so the match can
        /// neither false-positive on another endpoint nor drift from the
        /// emitter. The operation is then recognised by its reply's wire
        /// length, which is what distinguishes the two that matter here from
        /// every other request on the endpoint. Nothing else about a reply is
        /// read: its ordinal belongs to no client in particular, because the
        /// rendezvous is shared.
        ///
        /// The order is the attribution. A create before the menu reply is the
        /// application's own launch window and is not counted; one after it can
        /// only be the surface a chosen row asked for.
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
                    // length and latching nothing (fail closed).
                    "len" => {
                        reply_len = usize::try_from(
                            tairix_util::count::parse_decimal(value).unwrap_or_default(),
                        )
                        .unwrap_or_default();
                    }
                    _ => {}
                }
            }
            if !on_window_endpoint {
                return;
            }
            if reply_len == tairix_abi::window_ipc::WINDOW_MINTED_ID_REPLY_LEN {
                self.menu_asked.store(true, Ordering::Release);
            } else if reply_len == tairix_abi::window_ipc::WINDOW_CREATE_REPLY_LEN
                && self.menu_asked.load(Ordering::Acquire)
            {
                self.sheet_opened.store(true, Ordering::Release);
            }
        }

        /// Latch the launch witness from an `APP_LOADED` record naming the
        /// application's bundle.
        fn note_bundle_loaded(&self, event: &Event<'_>) {
            for field in event.fields {
                if field.key != "bundle" {
                    continue;
                }
                if let tairix_log::FieldValue::Str(value) = field.value {
                    if is_menu_bundle(value) {
                        self.app_launched.store(true, Ordering::Release);
                    }
                }
            }
        }

        /// Whether every witness is in.
        fn passed(&self) -> bool {
            self.app_launched.load(Ordering::Acquire)
                && self.menu_asked.load(Ordering::Acquire)
                && self.sheet_opened.load(Ordering::Acquire)
        }
    }

    impl Sink for MenuSink {
        fn write_event(&self, event: &Event<'_>) {
            // Replay through the serial sink first, so the QEMU transcript
            // records the full boot → unlock → desktop → menu timeline and
            // the host can gate its injection on it.
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

    /// Whether `bundle` is the application's bundle in the system application
    /// store, composed from the shared `lib/abi` spellings rather than written
    /// out as a path.
    fn is_menu_bundle(bundle: &str) -> bool {
        bundle
            .strip_prefix(tairix_abi::SYSTEM_APPLICATION_STORE)
            .and_then(|rest| rest.strip_prefix('/'))
            .and_then(|name| name.strip_suffix(tairix_abi::BUNDLE_SUFFIX))
            .is_some_and(|name| name == MENU_APP_NAME)
    }

    /// The audit observer the boot pipeline is handed.
    static AUDIT_SINK: MenuSink = MenuSink::new();

    /// Forward to the shared aarch64 panic bridge. A panic before the PASS
    /// finisher parks the CPU, the run times out, and the harness reports
    /// `Outcome::Timeout` — the documented fail-loud behaviour.
    #[panic_handler]
    fn tairix_menu_qemu_aarch64_panic(info: &PanicInfo<'_>) -> ! {
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
