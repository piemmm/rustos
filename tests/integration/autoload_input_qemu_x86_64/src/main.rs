//! `plans/ARCHSUPPORT.md` A4 QEMU integration test: boot the production
//! x86_64 `tairix-kernel` pipeline with a planted whole-disk encrypted-root
//! image that carries the **kernel-signed virtio-input keyboard driver
//! bundle** in its always-readable `/System` store, plus an attached
//! `virtio-keyboard-pci` device, and prove the full **driver-loading-by-
//! discovery autoload path** over the virtio-**PCI** bus: one user-space
//! driver instance for the discovered virtio-input node delivering a typed
//! key to the kernel input-focus arbiter.
//!
//! ## What this test asserts — and how it differs from its siblings
//!
//! * `autoload_input_qemu_aarch64` / `autoload_input_qemu_riscv64` prove the
//!   same path over the virtio-**MMIO** bus. This vertical is their x86_64
//!   virtio-PCI sibling: the discovery, match, spawn, and interrupt bind all
//!   run over the PCI enumeration path (`lib/pci`, ECAM / mechanism #1, MSI-X)
//!   rather than the single-aperture MMIO transport.
//! * `root_unlock_admission_qemu_x86_64` proves the in-kernel unlock kthread
//!   mounts `/System` and installs the users database. This vertical composes
//!   the *user-space* autoload path on top: `devmgr` matches a signed bundle
//!   to a discovered node and the kernel spawns it into its own process.
//!
//! This vertical attaches the shared `tairix_test_encrypted_root_image`
//! whole-disk image, planted by the `image_drivers` pipeline with the signed
//! `virtio_kbd` bundle at the `/System`-volume-relative
//! `Drivers/input/virtio_kbd/Run` (cross-compiled for x86_64), as a
//! virtio-blk-pci device **plus** a `virtio-keyboard-pci` device, and boots
//! `boot_x86_64::boot` verbatim. The production path then:
//!
//! 1. **Discovers** the virtio-block root *and* the virtio-input node
//!    (bootstrap-floor virtio-PCI enumeration). The input node carries its
//!    four role-tagged config windows, a coherent DMA constraint, **and** its
//!    routed PCI interrupt line as capability-grant requests.
//! 2. **Admits the unlock kthread**, which brings the root block device up
//!    over the production MSI-X device-IRQ path, mounts the always-readable
//!    `/System` volume, and serves its signed driver store over the
//!    capability-gated IPC endpoint. The store binds **independently of** the
//!    encrypted-root passphrase, so the keyboard driver autoloads regardless
//!    of whether the interactive unlock completes.
//! 3. **Reactive user-space autoload (Design D)**: the long-running `devmgr`
//!    service reads the hardware tree, lists the `/System` store over the IPC
//!    service, matches the signed bundle to the discovered virtio-input node
//!    (`lib/devmatch`), and asks the kernel to load it; the kernel re-runs the
//!    full signed gate (verified against the embedded
//!    `KERNEL_DRIVER_SIGNER_PUBKEY`) and **spawns it into its own user-space
//!    process** with exactly the node's resource grants — the four
//!    role-tagged windows + DMA + the routed MSI-X line.
//! 4. The spawned driver instance maps its register windows, brings its
//!    virtio-input device up over `PciTransport` (`enable_msix(0)`), **then
//!    binds its granted interrupt line and parks on `irq_wait`**
//!    (interrupt-driven, never a busy poll), and on each device interrupt
//!    pumps decoded events into the arbiter via `key_inject`.
//!
//! ## Why the PASS keys on one witness
//!
//! The audit sink reports PASS once it has seen `AuditEvent::InputDelivered`
//! with `kind=key` — the one-shot witness the `key_inject` handler emits the
//! first time a keyboard-class driver delivers to the arbiter. Reaching it
//! requires every preceding step to have succeeded: the `/System` volume
//! mounted and served, the store listed, the signed bundle verified, the node
//! matched, one user-space process spawned with exactly its node's grants, the
//! device brought up over PCI/MSI-X, the driver's interrupt armed, and the
//! typed key decoded and delivered. A run where any step fails never reaches
//! the witness, so the harness times out — the documented fail-loud behaviour.
//!
//! This vertical deliberately does **not** key on the encrypted-root unlock:
//! the injected key proves the autoload-input path, and the interactive unlock
//! (users-DB install) is the `root_unlock_admission_qemu_x86_64` vertical's
//! job.
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
    use tairix_kernel::{
        boot, handle_panic_via_kernel_core, FreeListAllocator, SerialSink, SERIAL_SINK,
    };
    use tairix_kernel_core::AuditEvent;
    use tairix_log::{Event, FieldValue, Sink};

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
    /// to QEMU the first time an `AuditEvent::InputDelivered` record with
    /// `kind=key` appears — the autoloaded user-space virtio-input keyboard
    /// driver delivering the typed key to the input-focus arbiter. An
    /// unrecognised `kind` value flips nothing (fail closed — a malformed
    /// witness can never satisfy PASS).
    struct AutoloadInputSink;

    impl Sink for AutoloadInputSink {
        fn write_event(&self, event: &Event<'_>) {
            // Replay through the serial sink so the QEMU transcript records the
            // full boot + unlock + autoload + input timeline (the harness also
            // gates its key injection on the `sc=irq_bind` line of this
            // replay — the autoloaded driver's arm step).
            SerialSink::new().write_event(event);
            if event.id.0 != AuditEvent::InputDelivered.id().0 {
                return;
            }
            for field in event.fields {
                if field.key == "kind" && matches!(field.value, FieldValue::Str("key")) {
                    qemu_exit::exit_success();
                }
            }
        }
    }

    static AUDIT_SINK: AutoloadInputSink = AutoloadInputSink;

    /// Forward to the shared bridge in `tairix_kernel::x86_64::panic_ctx`.
    /// The bridge logs through `SERIAL_SINK`, not `AUDIT_SINK`, so a panic
    /// before PASS does not trip the QEMU-exit short-circuit — it halts, the
    /// run times out, and the harness reports `Outcome::Timeout` (fail-loud).
    #[panic_handler]
    fn tairix_autoload_input_qemu_x86_64_panic(info: &PanicInfo<'_>) -> ! {
        handle_panic_via_kernel_core(info)
    }

    /// The symbol the arch crate's boot trampoline calls. Forwards to
    /// [`tairix_kernel::boot`] with the production COM1 log sink and the
    /// audit-observer sink.
    ///
    /// Boot at the `Debug` filter: the autoloaded driver's arm step
    /// (`irq_bind`) is an audited syscall whose `SyscallInvoked` record is
    /// `Debug`, and the harness waits for that record's `sc=irq_bind` serial
    /// marker before injecting the key.
    #[no_mangle]
    pub extern "C" fn kernel_main(multiboot_info: u64) -> ! {
        boot(
            multiboot_info,
            &SERIAL_SINK,
            &AUDIT_SINK,
            tairix_log::Level::Debug,
        )
    }
}

// --- Host stub -----------------------------------------------------
#[cfg(not(itest_x86_64))]
fn main() {}

#[cfg(not(itest_x86_64))]
#[allow(dead_code)]
fn _suppress_no_main() {}
