//! QEMU integration test: hold the graphical desktop to its icon artwork
//! while the machine is under real memory pressure
//! (`plans/SMARTRAM.md`, `plans/ICONS.md`).
//!
//! The production aarch64 boot pipeline runs unchanged — bootstrap-floor
//! virtio-MMIO discovery, the unlock kthread, the encrypted `ARXFS` root,
//! driver autoload, the display service and the desktop session — and only
//! the audit sink is swapped for the PASS witnesses. The host logs in as the
//! seeded fixture account, starts the desktop, launches the terminal from the
//! program library, and then clicks its icon-bar slot once per further
//! window, each click gated on the session's own witness that the previous
//! window reached the screen.
//!
//! # What the guest attests
//!
//! Three facts, all three required, none of them inferable from the others:
//!
//! 1. **The application launched** — an `APP_LOADED` record naming the
//!    terminal's bundle in the system application store.
//! 2. **Every window opened** — [`WINDOWS_OPENED`] create replies served on
//!    the reserved window endpoint. A create is recognised by its reply's
//!    wire length, which is unique among that endpoint's replies; nothing
//!    about a reply's ordinal is read, because the rendezvous is shared.
//! 3. **The machine really was under pressure** — the system pressure gauge's
//!    published band left normal at least once. Read through the kernel's own
//!    diagnostics registry, so the test observes the production gauge rather
//!    than steering it: there is no test hook in the pressure path, and a run
//!    that never left normal cannot pass.
//!
//! # What the host attests
//!
//! The pixels. The runner dumps the display on the first revealed desktop
//! frame and again once the whole screenful of windows bar one is on screen
//! (`WINDOWS_SHOWN_AT_DUMP`, which the guest outlives), and requires the icon
//! bar to carry the same artwork in both frames. A desktop that answered
//! pressure by dropping its decoded icons draws built-in glyphs in their place,
//! so the two frames diverge and the assertion fails — which is the defect this
//! vertical exists to keep fixed.
//!
//! A panic before every witness is in parks the CPU, the guest falls silent,
//! and the runner reports a timeout — loud failure, never a false pass.

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
    use tairix_kernel_core::memstats::MEM_STATS;
    use tairix_log::{Event, Sink};
    use tairix_reclaim::PressureBand;
    use tairix_test_desktop_pressure_qemu_aarch64::{BAR_APP_NAME, WINDOWS_OPENED};
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
    struct DesktopPressureSink {
        /// The terminal's bundle was loaded.
        app_launched: AtomicBool,
        /// Window creates served on the reserved window endpoint.
        windows_opened: AtomicU32,
        /// The system pressure gauge has published a band above normal.
        left_normal_band: AtomicBool,
    }

    impl DesktopPressureSink {
        /// A sink with no witness latched.
        const fn new() -> Self {
            Self {
                app_launched: AtomicBool::new(false),
                windows_opened: AtomicU32::new(0),
                left_normal_band: AtomicBool::new(false),
            }
        }

        /// Count a window create served on the reserved window endpoint.
        ///
        /// The endpoint is matched against the exact hex spelling the
        /// kernel/ipc audit fields render, so the match can neither
        /// false-positive on another endpoint nor drift from the emitter.
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
        /// terminal's bundle.
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

        /// Latch the pressure witness if the published band is above normal.
        ///
        /// A peek at the band the production gauge last folded, never a
        /// reading of its own: taking a reading here would make the observer
        /// spend the frame-allocator lock inside an audit callback, and the
        /// band is refreshed by whoever actually spends the memory.
        fn note_pressure(&self) {
            if MEM_STATS.published_band() != PressureBand::Normal {
                self.left_normal_band.store(true, Ordering::Release);
            }
        }

        /// Whether every witness is in.
        fn passed(&self) -> bool {
            self.app_launched.load(Ordering::Acquire)
                && self.left_normal_band.load(Ordering::Acquire)
                && self.windows_opened.load(Ordering::Acquire) >= WINDOWS_OPENED
        }
    }

    impl Sink for DesktopPressureSink {
        fn write_event(&self, event: &Event<'_>) {
            // Replay through the serial sink first, so the QEMU transcript
            // records the full boot → unlock → desktop → windows timeline and
            // the host can gate its injection on it.
            SerialSink::new().write_event(event);
            self.note_pressure();
            if event.id.0 == tairix_kernel_ipc::AuditEvent::CallReplied.id().0 {
                self.note_call_replied(event);
            } else if event.id.0 == tairix_appload::events::APP_LOADED.0 {
                self.note_bundle_loaded(event);
            }
            if self.passed() {
                qemu_exit::exit_success();
            }
        }
    }

    /// Whether `bundle` is the driven application's bundle in the system
    /// application store, composed from the shared `lib/abi` spellings rather
    /// than written out as a path.
    fn is_bar_bundle(bundle: &str) -> bool {
        bundle
            .strip_prefix(tairix_abi::SYSTEM_APPLICATION_STORE)
            .and_then(|rest| rest.strip_prefix('/'))
            .and_then(|name| name.strip_suffix(tairix_abi::BUNDLE_SUFFIX))
            .is_some_and(|name| name == BAR_APP_NAME)
    }

    /// The audit observer the boot pipeline is handed.
    static AUDIT_SINK: DesktopPressureSink = DesktopPressureSink::new();

    /// Forward to the shared aarch64 panic bridge. A panic before the PASS
    /// finisher parks the CPU, the run times out, and the harness reports
    /// `Outcome::Timeout` — the documented fail-loud behaviour.
    #[panic_handler]
    fn tairix_desktop_pressure_qemu_aarch64_panic(info: &PanicInfo<'_>) -> ! {
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
