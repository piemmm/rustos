//! `plans/OPEN-DEFECTS.md` D7 + D8 QEMU integration test: boot the production
//! x86_64 `tairix-kernel` pipeline with a planted whole-disk encrypted-root
//! image and prove the **virtio-blk-PCI MSI-X disk-completion interrupt**
//! drives the two-kthread admission path all the way through the interactive
//! encrypted-root unlock and the users-database install — the regression for
//! both the D7 triple fault (reaching the read-only `/System` mount) and the
//! D8 admission read stall (reaching the users-DB install).
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
//! virtio-blk-PCI device up over the production **MSI-X** path, mounts the
//! read-only `/System` volume, then — driven by the scripted passphrase on
//! COM1 — unlocks the encrypted user-data root and installs the users
//! database into `LATE_USERS_DB`. Reaching the install requires the disk's
//! completion MSI-X to be delivered on its dedicated vector and to wake the
//! scheduler-parked bring-up over **thousands** of block reads, with a
//! device IRQ preempting ring-3 services without corrupting the per-CPU GS
//! state — the exact path the D7 triple fault broke (an external-IRQ
//! stack-frame-offset bug ran an unbalanced `swapgs`; a shared IO-APIC-pin
//! vector modelled the edge MSI as a level line). The audit sink reports
//! PASS the instant it sees `USERS_DB_INSTALLED_MESSAGE`.
//!
//! A regression of the D7 fix triple-faults on the first disk read (before
//! any mount), so the message never appears and the harness fails loud.
//!
//! ## D8 — the admission read path terminates and installs
//!
//! The *interactive users-database install* over the encrypted root (the
//! scripted passphrase, decrypt, `LATE_USERS_DB` publish) is a strict
//! superset of the `/System`-mount witness and is what this vertical now
//! asserts. It is the two-kthread admission path — the interactive-unlock
//! kthread and the driver-store serve kthread sharing one boot disk through
//! the pressure-governed `BlockCache`/`SharedBlock` on a 256 MiB guest — that
//! D8 reported stalling with no forward progress. That stall is resolved (it
//! was a consequence of the pre-fix kernel-heap OOM/pressure condition the
//! `kernel/mem` `MAX_ORDER` growth + fallible-reserve read fix removed); the
//! admission install now completes deterministically, and this witness is the
//! regression that keeps it that way. The observer `root_unlock_login_qemu_x86_64`
//! drives the unlock policy directly on the boot CPU and so never exercises
//! this concurrent two-kthread path.
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
    /// PASS to QEMU the instant the users-database-installed witness
    /// appears — proof that the in-kernel bring-up, admitted by
    /// `spawn_if_present`, brought the discovered virtio-blk-PCI root up over
    /// the production MSI-X device-IRQ path, mounted the read-only `/System`
    /// volume (D7), and drove the two-kthread admission path through the
    /// encrypted-root unlock and the users-DB install (D8).
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
