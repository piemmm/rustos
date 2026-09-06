//! QEMU integration vertical: **launch an application that holds no
//! filesystem capability, and watch the desktop session delegate one
//! descriptor into it for the file the user picked**
//! (`plans/CAPABILITY_USE.md` CU6, `plans/APPWIN.md` AW5).
//!
//! # What this proves that no host test can
//!
//! The delegation's pieces are host-tested: the kernel's mint and its
//! instance gate, one-shot redemption, the grantor-identity re-check, the
//! write-extent ceiling, the picker's browse model, and the viewer's view
//! engine. What only a real machine can show is that they are **wired to each
//! other across two principals** — that a click in a window the session owns
//! makes the *session* mint a descriptor for a file the *viewer* holds no
//! authority to open, and that the viewer redeems it.
//!
//! So the guest boots the **production** aarch64 pipeline
//! (`boot_aarch64::boot`) against a planted encrypted root, and the host drives
//! the desktop blind through the QEMU monitor: unlock, log in, start the
//! desktop, then Library → the viewer's row → the document's row in the
//! picker that opens. Only the audit sink is swapped, for the PASS witnesses
//! below.
//!
//! # The PASS gate
//!
//! Two latches which must land **in order**, each attributed by the kernel to
//! the principal that made the call:
//!
//! 1. **The session minted the delegation.** A `SyscallInvoked` record naming
//!    [`GRANT_SYSCALL`] from [`GRANTOR_COMM`] — the picker concluding the
//!    user's choice.
//! 2. **The viewer redeemed it.** A `SyscallInvoked` record naming
//!    [`REDEEM_SYSCALL`] from [`RECIPIENT_COMM`], counted **only** once the
//!    mint has been seen.
//!
//! Attributing each half to its own `comm` is what makes this a statement
//! about a hand-off *between* principals rather than about one process
//! touching its own descriptor, and requiring the order rules out a redeem
//! that could not have come from this pick. Neither syscall is reachable
//! without the whole chain: the viewer requests a pick because it was handed
//! no document, the session opens the picker under its own authority, and the
//! mint happens only where a click concluded on a regular file.
//!
//! # Why the guest latches no frame
//!
//! Reading the screen is the host's job. The guest deliberately claims nothing
//! about pixels: what it can attest is which principal called which syscall,
//! and that is the whole security claim here.
//!
//! # Why the guest cannot exit early
//!
//! The redeem is caused by the script's final click, and that click is gated
//! on the session's own announcement that the picker is on screen with rows in
//! it. So the record that completes the PASS cannot happen before the gesture
//! that causes it.
//!
//! A panic before both latches parks the CPU, the guest falls silent, and the
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
    use tairix_test_filepick_qemu_aarch64::{
        GRANTOR_COMM, GRANT_SYSCALL, RECIPIENT_COMM, REDEEM_SYSCALL,
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

    /// Audit observer that replays the whole trail to serial and latches the
    /// two ordered PASS witnesses described in the module docs.
    struct DelegationSink {
        /// The session minted the one-shot delegation.
        granted: AtomicBool,
        /// The viewer redeemed a delegation the session had already minted.
        redeemed: AtomicBool,
    }

    impl DelegationSink {
        /// A sink with no witness latched.
        const fn new() -> Self {
            Self {
                granted: AtomicBool::new(false),
                redeemed: AtomicBool::new(false),
            }
        }

        /// Latch whichever half of the delegation this dispatched syscall is.
        ///
        /// Both the calling process and the syscall are read from the record's
        /// own kernel-attested fields, so a call by any other principal — or
        /// any other syscall — matches nothing. The redeem latches only after
        /// the mint, so a redemption that could not have come from this pick
        /// cannot complete the gate (fail closed).
        fn note_syscall(&self, event: &Event<'_>) {
            let mut comm = "";
            let mut call = "";
            for field in event.fields {
                let tairix_log::FieldValue::Str(value) = field.value else {
                    continue;
                };
                match field.key {
                    "comm" => comm = value,
                    "sc" => call = value,
                    _ => {}
                }
            }
            if comm == GRANTOR_COMM && call == GRANT_SYSCALL {
                self.granted.store(true, Ordering::Release);
            } else if comm == RECIPIENT_COMM
                && call == REDEEM_SYSCALL
                && self.granted.load(Ordering::Acquire)
            {
                self.redeemed.store(true, Ordering::Release);
            }
        }

        /// Whether the delegation crossed, mint before redeem.
        fn passed(&self) -> bool {
            self.redeemed.load(Ordering::Acquire)
        }
    }

    impl Sink for DelegationSink {
        fn write_event(&self, event: &Event<'_>) {
            // Replay through the serial sink first, so the QEMU transcript
            // records the full boot → unlock → desktop → pick timeline and
            // the host can gate its injection on it.
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

    /// The audit observer the boot pipeline is handed.
    static AUDIT_SINK: DelegationSink = DelegationSink::new();

    /// Forward to the shared aarch64 panic bridge. A panic before the PASS
    /// finisher parks the CPU, the run times out, and the harness reports
    /// `Outcome::Timeout` — the documented fail-loud behaviour.
    #[panic_handler]
    fn tairix_filepick_qemu_aarch64_panic(info: &PanicInfo<'_>) -> ! {
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
            // default `Info` filter, and it carries both PASS witnesses; the
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
