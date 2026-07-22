//! `plans/OPEN-DEFECTS.md` D7 QEMU integration test: boot the production
//! x86_64 `tairix-kernel` pipeline with a planted whole-disk encrypted-root
//! image and prove the **virtio-blk-PCI MSI-X disk-completion interrupt**
//! drives the in-kernel bring-up all the way to the read-only `/System`
//! mount — the regression for the D7 triple fault.
//!
//! ## What this test asserts — and how it differs from its siblings
//!
//! * `root_unlock_login_qemu_x86_64` drives the interactive unlock
//!   **policy** *directly* over the planted disk from a boot-observer
//!   context that hijacks the boot CPU on `BootCompleted`, so the admitted
//!   unlock kthread never dispatches and the production MSI-X device-IRQ
//!   wake path is never exercised.
//! * `spawn_session_qemu_x86_64` boots the production pipeline with **no**
//!   disk, so `unlock_service::spawn_if_present` is a no-op.
//!
//! This vertical attaches the shared `tairix_test_encrypted_root_image`
//! whole-disk image as a virtio-blk-pci device and boots `boot_x86_64::boot`
//! verbatim. The production path discovers + binds the root, admits the
//! in-kernel root-unlock kthread, and that kthread brings the
//! virtio-blk-PCI device up over the production **MSI-X** path and mounts
//! the read-only `/System` volume. Reaching the mount requires the disk's
//! completion MSI-X to be delivered on its dedicated vector and to wake the
//! scheduler-parked bring-up **repeatedly** (dozens of block reads), with a
//! device IRQ preempting ring-3 services without corrupting the per-CPU GS
//! state — the exact path the D7 triple fault broke (an external-IRQ
//! stack-frame-offset bug ran an unbalanced `swapgs`; a shared IO-APIC-pin
//! vector modelled the edge MSI as a level line). The audit sink reports
//! PASS the instant it sees `SYSTEM_VOLUME_MOUNTED_MESSAGE`.
//!
//! A regression of the D7 fix triple-faults on the first disk read (before
//! any mount), so the message never appears and the harness fails loud.
//!
//! ## Scope note (see `plans/OPEN-DEFECTS.md` D8)
//!
//! The *interactive users-database install* over the encrypted root (typing
//! the passphrase, decrypting, publishing `LATE_USERS_DB`) is a strict
//! superset of this witness and is **not** asserted here: with the disk
//! interrupt fixed, that path exposes a separate, unrelated unbounded
//! disk-read loop in the encrypted-root/users-DB read path (D8), which the
//! observer `root_unlock_login_qemu_x86_64` does not hit. This vertical is
//! extended to key on the users-DB install once D8 is fixed.
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
    use tairix_kernel::root_mount::SYSTEM_VOLUME_MOUNTED_MESSAGE;
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

    /// Sink that replays every event through [`SERIAL_SINK`] and reports
    /// PASS to QEMU the instant the `/System`-volume-mounted witness
    /// appears — proof that the in-kernel bring-up, admitted by
    /// `spawn_if_present`, brought the discovered virtio-blk-PCI root up over
    /// the production MSI-X device-IRQ path and read enough of the disk to
    /// mount the read-only `/System` volume, without the D7 triple fault.
    struct UnlockAdmissionSink;

    impl Sink for UnlockAdmissionSink {
        fn write_event(&self, event: &Event<'_>) {
            SerialSink::new().write_event(event);
            if event.message == SYSTEM_VOLUME_MOUNTED_MESSAGE {
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
