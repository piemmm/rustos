//! QEMU integration vertical: **an unconfigured machine that can draw boots
//! to the graphical login screen** (`plans/NEW-DESKTOP-LOGIN.md` G7.1).
//!
//! # What this proves that no host test can
//!
//! Which login a round runs is decided by pure, host-tested policy
//! (`tairix_login::session`). What only a real machine can show is that the
//! policy is fed the truth: that the display driver has autoloaded and bound
//! its service, that the encrypted root is mounted by the time the round
//! begins, and that the settings store on that root answers "I hold no
//! configuration" rather than "I am not here". Get any of those wrong and a
//! machine that could show a login screen sits at a text prompt instead —
//! which is exactly the defect this vertical was written for: login probed
//! for the store's *directory*, a directory `configure` only creates on its
//! first write, so every never-configured installation read as an
//! unmountable volume and pinned itself to the text prompt.
//!
//! # The disk is the experiment
//!
//! The backing volume is `FsDisk::GreeterRootDisk`, which carries the same
//! signed input and display driver bundles as the autoload vertical but the
//! **standard** application store — no planted `os.loginType`. That is the
//! state a fresh installation boots in, and the host script types nothing
//! but the unlock passphrase: no account, no `desktop` command, nothing that
//! could ask for a graphical session. So the login screen can only be here
//! because login chose it.
//!
//! # The PASS witnesses
//!
//! Both are **kernel-attested**. A userland record reaches the diagnostic
//! sink only — the audit sink stays kernel-only, so user space can neither
//! forge nor truncate an entry — which is why the greeter's own
//! "the login screen is up" cannot be the gate, however plainly it reads on
//! the serial transcript.
//!
//! 1. **Login chose the graphical round.** An `APP_LOADED` record naming the
//!    greeter's bundle in the system service store: the kernel verified and
//!    loaded it. Nothing else on this disk launches that bundle, and a text
//!    round never asks for it, so the record is login's decision itself.
//!    `PROCESS_SPAWNED` would not do — it carries an entry address and can
//!    attribute no bundle.
//! 2. **A frame reached the screen.** The next reply the display service
//!    serves on its reserved endpoint, once (1) has latched. The greeter is
//!    the only graphical client alive at that point — no desktop, no app,
//!    and the script types nothing after the passphrase — so the exchange is
//!    the login screen's own present. Ordering after (1) is what makes it
//!    attributable: an unordered reply on a shared rendezvous belongs to
//!    nobody in particular.
//!
//! Together they say the machine decided on the graphical login *and*
//! reached the point of drawing it. A text round, a greeter that cannot
//! paint, or a panic all leave a latch open: the guest never exits, and the
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
    struct GreeterDefaultSink {
        /// Login chose the graphical round: the greeter's bundle was
        /// verified and loaded.
        greeter_loaded: AtomicBool,
        /// The display service served the greeter a reply, so the login
        /// screen reached the point of drawing.
        frame_served: AtomicBool,
    }

    impl GreeterDefaultSink {
        /// A sink with no witness latched.
        const fn new() -> Self {
            Self {
                greeter_loaded: AtomicBool::new(false),
                frame_served: AtomicBool::new(false),
            }
        }

        /// Latch the decision witness from an `APP_LOADED` record naming the
        /// greeter's bundle. A record naming any other bundle leaves the
        /// latch alone.
        fn note_bundle_loaded(&self, event: &Event<'_>) {
            for field in event.fields {
                if field.key != "bundle" {
                    continue;
                }
                if let tairix_log::FieldValue::Str(value) = field.value {
                    if is_greeter_bundle(value) {
                        self.greeter_loaded.store(true, Ordering::Release);
                    }
                }
            }
        }

        /// Latch the drawing witness from a reply the display service served
        /// after the greeter loaded.
        ///
        /// The endpoint is matched against the exact hex spelling the
        /// kernel/ipc audit fields render (`format_hex_u64`), so the match
        /// can neither false-positive on another endpoint nor drift from the
        /// emitter.
        fn note_call_replied(&self, event: &Event<'_>) {
            if !self.greeter_loaded.load(Ordering::Acquire) {
                return;
            }
            let mut endpoint_hex = [0u8; 16];
            let expected =
                format_hex_u64(tairix_abi::display_ipc::DISPLAY_ENDPOINT, &mut endpoint_hex);
            for field in event.fields {
                if field.key != "endpoint" {
                    continue;
                }
                if let tairix_log::FieldValue::Str(value) = field.value {
                    if value == expected {
                        self.frame_served.store(true, Ordering::Release);
                    }
                }
            }
        }

        /// Whether every witness is in.
        fn passed(&self) -> bool {
            self.greeter_loaded.load(Ordering::Acquire) && self.frame_served.load(Ordering::Acquire)
        }
    }

    impl Sink for GreeterDefaultSink {
        fn write_event(&self, event: &Event<'_>) {
            // Replay through the serial sink first, so the QEMU transcript
            // records the full boot → autoload → unlock → login → greeter
            // timeline and the host can gate its injection on it.
            SerialSink::new().write_event(event);
            if event.id.0 == tairix_appload::events::APP_LOADED.0 {
                self.note_bundle_loaded(event);
            } else if event.id.0 == tairix_kernel_ipc::AuditEvent::CallReplied.id().0 {
                self.note_call_replied(event);
            } else {
                return;
            }
            if self.passed() {
                qemu_exit::exit_success();
            }
        }
    }

    /// The audit observer the boot pipeline is handed.
    static AUDIT_SINK: GreeterDefaultSink = GreeterDefaultSink::new();

    /// Whether `bundle` is the greeter's own bundle in the system service
    /// store, composed from the shared `lib/abi` spellings rather than
    /// written out as a path.
    fn is_greeter_bundle(bundle: &str) -> bool {
        bundle
            .strip_prefix(tairix_abi::SYSTEM_SERVICE_STORE)
            .and_then(|rest| rest.strip_prefix('/'))
            .and_then(|name| name.strip_suffix(tairix_abi::BUNDLE_SUFFIX))
            .is_some_and(|name| name == GREETER_BUNDLE_NAME)
    }

    /// The greeter bundle's name in the system service store — the bundle
    /// `tairix_login::session::GREETER_SERVICE_PATH` names. A rename makes
    /// this vertical time out loudly, never pass on the wrong bundle.
    const GREETER_BUNDLE_NAME: &str = "greeter";

    /// Forward to the shared aarch64 panic bridge. A panic before the PASS
    /// finisher parks the CPU, the run times out, and the harness reports
    /// `Outcome::Timeout` — the documented fail-loud behaviour.
    #[panic_handler]
    fn tairix_greeter_default_qemu_aarch64_panic(info: &PanicInfo<'_>) -> ! {
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
