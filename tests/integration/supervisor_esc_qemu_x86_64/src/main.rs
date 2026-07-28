//! `plans/NEW-SUPERVISOR.md` §7 / `plans/ARCHSUPPORT.md` QEMU integration
//! test: the x86_64 sibling of `supervisor_esc_qemu_aarch64`. Boot the
//! production x86_64 `tairix-kernel` pipeline with a planted whole-disk
//! encrypted-root image, press **`ESC`** at the pre-mount boot screen to drop
//! into the in-kernel **pre-boot Supervisor** REPL, run a read-only command,
//! `continue` back to the normal unlock, and prove the resumed unlock still
//! mounts the encrypted `ARXFS` root.
//!
//! ## What this vertical asserts
//!
//! It exercises the ESC boot-screen contract end to end on the *production
//! boot path* — the same path `root_unlock_admission_qemu_x86_64` drives,
//! which already installs the real `SupervisorHost` and draws the boot screen
//! over COM1. The runner's serial script (`tools/xtask`
//! `SUPERVISOR_ESC_SCRIPT`, shared byte-for-byte with the aarch64 sibling)
//! walks the frozen, byte-exact screen states in order:
//!
//! 1. `[Press ESC for supervisor]` — the announcement
//!    (`root_mount::SUPERVISOR_ANNOUNCE`); the runner types a lone `ESC`.
//! 2. `Supervisor` — the enter banner
//!    (`root_mount::SUPERVISOR_ENTER_BANNER`, `\rARXFS\x1b[K\r\n\r\nSupervisor`)
//!    proving the collapse-to-`ARXFS`-then-`Supervisor` drop happened; the
//!    runner types `help` at the `*` prompt.
//! 3. `commands:` — the dispatcher's host-independent `Supervisor commands:`
//!    header, proving a real command ran inside the REPL; the runner types
//!    `continue`.
//! 4. `ARXFS passphrase: ` — the normal unlock prompt redrawn *after* the REPL
//!    exited, proving a Supervisor session is transparent to boot; the runner
//!    types the fixture passphrase.
//!
//! Reaching each marker in order is the byte-exact assertion (the run fails
//! loud if the guest exits before every scripted step was sent), and the PASS
//! witness below proves the resumed unlock completed.
//!
//! Because the ESC window and a lone `ESC` at the live passphrase prompt are
//! the *same* `enter_supervisor` drop, the vertical is robust to the 2-second
//! window race: if the announcement's timed window elapses before the scripted
//! `ESC` byte lands, that same `ESC` is read as the first byte of the
//! passphrase line and still drops into the Supervisor (`root_mount`
//! `PassphraseReadError::Escape`), so the `Supervisor` banner appears either
//! way and `continue` returns to the redrawn prompt in both cases.
//!
//! ## Why the PASS keys on the install message
//!
//! The audit sink reports PASS once it has seen the unlock-service
//! `USERS_DB_INSTALLED_MESSAGE` (`EventId(4139)`) — the witness that the
//! in-kernel unlock kthread mounted the encrypted `ARXFS` root and installed
//! the users database. That message can only be reached *after* the Supervisor
//! REPL returned via `continue` and the resumed unlock accepted the typed
//! passphrase, so the PASS proves the whole ESC → REPL → `continue` → unlock
//! round-trip. A run where `continue` never resumed the unlock, or the resumed
//! unlock never mounted, never reaches the message and the harness times out —
//! the documented fail-loud behaviour. The database *content* (that
//! `root`/`root` authenticates) is proven over the same shared fixture by
//! `root_unlock_login`, so this vertical keys on the install witness, not a
//! `login` success.
//!
//! ## How it differs from a production kernel
//!
//! It reuses the entire production x86_64 boot pipeline and only replaces the
//! audit sink. Splitting the audit-observer behaviour into a separate bin
//! (instead of a Cargo feature on a production crate) prevents feature
//! unification from leaking the QEMU-exit shortcut into any production build
//! (fail closed; the harness never decides what the kernel does next).

#![cfg_attr(itest_x86_64, no_std)]
#![cfg_attr(itest_x86_64, no_main)]
#![deny(missing_docs)]

// --- Freestanding test bin (`x86_64-unknown-none`) -----------------

#[cfg(itest_x86_64)]
mod kernel {
    use core::panic::PanicInfo;

    use tairix_arch_x86_64::qemu_exit;
    use tairix_kernel::kalloc::{Heap, HEAP_BYTES};
    use tairix_kernel::unlock_service::USERS_DB_INSTALLED_MESSAGE;
    use tairix_kernel::{
        boot, handle_panic_via_kernel_core, FreeListAllocator, SerialSink, SERIAL_SINK,
    };
    use tairix_log::{Event, Sink};

    /// Static heap for the bump allocator (identical to the production bin's
    /// declaration; `#[global_allocator]` is per-binary).
    ///
    /// `static mut` because the bump allocator hands out disjoint slices via
    /// an atomic cursor; the storage is otherwise never aliased.
    static mut HEAP: Heap = Heap::ZERO;

    /// Global allocator backed by [`HEAP`].
    ///
    /// SAFETY: the page-aligned `HEAP` static outlives the binary and the
    /// allocator is its only consumer.
    #[global_allocator]
    static ALLOCATOR: FreeListAllocator =
        unsafe { FreeListAllocator::new(core::ptr::addr_of!(HEAP) as *mut u8, HEAP_BYTES) };

    /// Sink that replays every event through [`SERIAL_SINK`] and reports PASS
    /// to QEMU the instant the unlock-service install message appears — the
    /// witness that the ESC → Supervisor REPL → `continue` round-trip resumed
    /// the normal unlock, which then mounted the encrypted `ARXFS` root and
    /// installed the users database. (The database *content* authenticating
    /// `root`/`root` is proven by `root_unlock_login`; the byte-exact
    /// boot-screen states are asserted by the runner's serial script — see the
    /// module docs.)
    struct SupervisorEscSink;

    impl Sink for SupervisorEscSink {
        fn write_event(&self, event: &Event<'_>) {
            // Replay through the serial sink so the QEMU transcript records
            // the full boot + Supervisor + unlock timeline.
            SerialSink::new().write_event(event);
            if event.message == USERS_DB_INSTALLED_MESSAGE {
                qemu_exit::exit_success();
            }
        }
    }

    static AUDIT_SINK: SupervisorEscSink = SupervisorEscSink;

    /// Forward to the shared bridge in `tairix_kernel::x86_64::panic_ctx`.
    /// The bridge logs through `SERIAL_SINK`, not `AUDIT_SINK`, so a panic
    /// before PASS does not trip the QEMU-exit short-circuit — it halts, the
    /// run times out, and the harness reports `Outcome::Timeout` (fail-loud).
    #[panic_handler]
    fn tairix_supervisor_esc_qemu_x86_64_panic(info: &PanicInfo<'_>) -> ! {
        handle_panic_via_kernel_core(info)
    }

    /// The symbol the arch crate's boot trampoline calls. Forwards to
    /// [`tairix_kernel::boot`] with the production COM1 log sink and the
    /// audit-observer sink, so the boot pipeline installs the real
    /// `SupervisorHost` and draws the ESC boot screen exactly as in
    /// production.
    #[no_mangle]
    pub extern "C" fn kernel_main(multiboot_info: u64) -> ! {
        boot(
            multiboot_info,
            &SERIAL_SINK,
            &AUDIT_SINK,
            tairix_log::Level::Info,
        )
    }
}

// --- Host stub -----------------------------------------------------
#[cfg(not(itest_x86_64))]
fn main() {}

#[cfg(not(itest_x86_64))]
#[allow(dead_code)]
fn _suppress_no_main() {}
