//! `plans/OPEN-DEFECTS.md` D7 / `plans/ARCHSUPPORT.md` A3 QEMU integration
//! test: boot the production x86_64 `tairix-kernel` pipeline with a planted
//! whole-disk encrypted-root image, and prove the **in-kernel root-unlock
//! kthread admission path** mounts the root over the production MSI-X
//! device-IRQ wake path.
//!
//! ## What this test asserts — and how it differs from its siblings
//!
//! * `root_unlock_login_qemu_x86_64` drives the interactive unlock
//!   **policy** (`unlock_root_disk_interactively`) *directly* over the
//!   planted disk from a boot-observer context — it does not exercise the
//!   production kthread admission (the observer scenario hijacks the boot
//!   CPU on `BootCompleted`, so the admitted unlock kthread never
//!   dispatches).
//! * `spawn_session_qemu_x86_64` boots the production pipeline with **no**
//!   disk, so `unlock_service::spawn_if_present` is a no-op.
//!
//! This vertical attaches the shared `tairix_test_encrypted_root_image`
//! whole-disk image as a virtio-blk-pci device and boots `boot_x86_64::boot`
//! verbatim. The production path then discovers + binds the root, the init
//! seam admits the in-kernel root-unlock kthread, and on its first dispatch
//! the kthread brings the virtio-blk-PCI device up over the production
//! MSI-X path, prompts `ARXFS passphrase: ` on the COM1 console, reads the
//! passphrase the runner types, mounts the encrypted `ARXFS` root, and
//! installs the users database — logging `USERS_DB_INSTALLED_MESSAGE`.
//!
//! The audit sink reports PASS once it sees that install message — the
//! witness that the disk-completion MSI-X woke the scheduler-parked unlock
//! kthread, the exact path `plans/OPEN-DEFECTS.md` D7 tracks. A run where
//! the wake never reaches the parked kthread never reaches that message, so
//! the harness times out (fail-loud).
//!
//! ## How it differs from a production kernel
//!
//! It reuses the entire production x86_64 boot pipeline and only replaces
//! the audit sink. Splitting the audit-observer behaviour into a separate
//! bin (instead of a Cargo feature on a production crate) prevents feature
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
    use tairix_kernel::{boot, handle_panic_via_kernel_core, FreeListAllocator, SerialSink, SERIAL_SINK};
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

    /// Sink that replays every event through [`SERIAL_SINK`] and reports
    /// PASS to QEMU the instant the unlock-service install message appears —
    /// the witness that the in-kernel unlock kthread, admitted by
    /// `spawn_if_present`, brought the discovered virtio-blk root up over
    /// the production MSI-X device-IRQ path, read the typed passphrase,
    /// mounted the encrypted root, and installed the users database.
    struct UnlockAdmissionSink;

    impl Sink for UnlockAdmissionSink {
        fn write_event(&self, event: &Event<'_>) {
            SerialSink::new().write_event(event);
            if event.message == USERS_DB_INSTALLED_MESSAGE {
                qemu_exit::exit_success();
            }
        }
    }

    static AUDIT_SINK: UnlockAdmissionSink = UnlockAdmissionSink;

    /// Forward to the shared bridge in `tairix_kernel::x86_64::panic_ctx`.
    /// The bridge logs through `SERIAL_SINK`, not `AUDIT_SINK`, so a panic
    /// before PASS does not trip the QEMU-exit short-circuit — it halts, the
    /// run times out, and the harness reports `Outcome::Timeout` (fail-loud).
    #[panic_handler]
    fn tairix_root_unlock_admission_qemu_x86_64_panic(info: &PanicInfo<'_>) -> ! {
        handle_panic_via_kernel_core(info)
    }

    /// The symbol the arch crate's boot trampoline calls. Forwards to
    /// [`tairix_kernel::boot`] with the production COM1 log sink and the
    /// audit-observer sink.
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
