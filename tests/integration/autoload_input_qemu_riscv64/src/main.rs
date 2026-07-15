//! `plans/NETWORK.md` N4e-riscv64 (first stage) QEMU integration test: boot
//! the production riscv64 (QEMU `virt` / SiFive) `rustos-kernel` pipeline with
//! a planted whole-disk encrypted-root image that carries the **kernel-signed
//! virtio-input keyboard driver bundle** in its always-readable `/System`
//! store, plus an attached `virtio-keyboard-device`, and prove the full
//! **driver-loading-by-discovery autoload path**: one user-space driver
//! instance for the discovered virtio-input node delivering a typed key to the
//! kernel input-focus arbiter.
//!
//! ## What this test asserts — and how it differs from its siblings
//!
//! * `spawn_init_qemu_riscv64` proves PID 1 reaches U-mode and traps back.
//! * `input_virtio_mmio_qemu_riscv64` proves a discovered virtio-input device
//!   reaches an *in-kernel* scaffold decode path (load → use → unload).
//!
//! This vertical composes autoload on the production boot path: it attaches
//! the shared `rustos_test_encrypted_root_image` whole-disk image, planted by
//! the `image_drivers` pipeline with the signed `virtio_kbd` bundle at the
//! `/System`-volume-relative `Drivers/input/virtio_kbd/Run` (cross-compiled
//! for riscv64), as a virtio-blk-mmio device **plus** a `virtio-keyboard-
//! device`, and boots `boot_riscv64::boot` verbatim. The production path then:
//!
//! 1. **Discovers** the virtio-block root *and* the virtio-input node
//!    (bootstrap-floor virtio-MMIO enumeration). The input node carries its
//!    register window, a coherent DMA constraint, **and** its discovered PLIC
//!    interrupt line as capability-grant requests.
//! 2. **Admits the unlock kthread**, which brings the root block device up
//!    over the production PLIC device-IRQ path, mounts the always-readable
//!    `/System` volume, and serves its signed driver store over the
//!    capability-gated IPC endpoint. Crucially the store binds **independently
//!    of** the encrypted-root passphrase (the riscv64 SBI console exposes no
//!    interactive input this slice, so the interactive unlock fails closed) —
//!    so the keyboard driver still autoloads.
//! 3. **Reactive user-space autoload (Design D)**: the long-running `devmgr`
//!    service reads the hardware tree, lists the `/System` store over the IPC
//!    service, matches the signed bundle to the discovered virtio-input node
//!    (`lib/devmatch`), and asks the kernel to load it; the kernel re-runs the
//!    full signed gate (verified against the embedded
//!    `KERNEL_DRIVER_SIGNER_PUBKEY`) and **spawns it into its own user-space
//!    process** with exactly the node's resource grants.
//! 4. The spawned driver instance maps its register window, brings its
//!    virtio-input device up, **then binds its granted interrupt line and
//!    parks on `irq_wait`** (interrupt-driven, never a busy poll), and on each
//!    device interrupt pumps decoded events into the arbiter via `key_inject`.
//!
//! ## Why the PASS keys on one witness
//!
//! The audit sink reports PASS once it has seen `AuditEvent::InputDelivered`
//! with `kind=key` — the one-shot witness the `key_inject` handler emits the
//! first time a keyboard-class driver delivers to the arbiter. Reaching it
//! requires every preceding step to have succeeded: the `/System` volume
//! mounted and served, the store listed, the signed bundle verified, the node
//! matched, one user-space process spawned with exactly its node's grants, the
//! device brought up, the driver's interrupt armed, and the typed key decoded
//! and delivered. A run where any step fails never reaches the witness, so the
//! harness times out — the documented fail-loud behaviour.
//!
//! This vertical deliberately does **not** key on the encrypted-root unlock
//! (`UsersDbLoaded`): the riscv64 SBI console has no interactive input drain
//! this slice, so no passphrase can be typed and the unlock fails closed by
//! design. Proving the passphrase-typed unlock and the desktop click-through
//! is the aarch64 `autoload_input_qemu_aarch64` vertical's job (a display
//! world); this one proves the autoload-input path end to end on riscv64.
//!
//! ## Real firmware device tree
//!
//! QEMU's riscv64 `virt` OpenSBI firmware hands the boot hart a valid
//! device-tree pointer in `a1`, so — unlike the aarch64 `-kernel` path — this
//! vertical forwards the verbatim pointer to the boot pipeline, which
//! discovers the board (including the `virtio,mmio` transport slots the disk
//! and the keyboard populate) from it exactly as it would from real firmware.
//!
//! ## How it differs from a production kernel
//!
//! It reuses the entire production riscv64 boot pipeline and only replaces the
//! audit sink. Splitting the audit-observer behaviour into a separate bin
//! (instead of a Cargo feature on a production crate) prevents feature
//! unification from leaking the QEMU-exit shortcut into any production build
//! (fail closed; the harness never decides what the kernel does next).

#![cfg_attr(itest_riscv64, no_std)]
#![cfg_attr(itest_riscv64, no_main)]
#![deny(missing_docs)]

// --- Freestanding test bin (`riscv64gc-unknown-none-elf`) ----------

#[cfg(itest_riscv64)]
mod kernel {
    use core::panic::PanicInfo;

    use rustos_arch_riscv64::{handle_panic_via_serial, qemu_exit, SerialSink, SERIAL_SINK};
    use rustos_kalloc::{FreeListAllocator, Heap, HEAP_BYTES};
    use rustos_kernel::riscv64::boot as boot_riscv64;
    use rustos_kernel_core::AuditEvent;
    use rustos_log::{Event, FieldValue, Sink};

    /// Static boot heap.
    ///
    /// Placed in the linker's dedicated `.heap` (NOLOAD) section so the boot
    /// trampoline does not zero its bytes (the bump allocator does not require
    /// zeroed backing) and the boot pipeline excludes it from the usable
    /// physical-memory map, exactly as the production riscv64 kernel binary's
    /// heap does. `static mut` because the bump allocator hands out disjoint
    /// slices via an atomic cursor; the storage is otherwise never aliased.
    #[link_section = ".heap"]
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

    /// Forward to the shared riscv64 panic bridge. A panic before the PASS
    /// finisher parks the hart, the run times out, and the harness reports
    /// `Outcome::Timeout` — the documented fail-loud behaviour.
    #[panic_handler]
    fn rustos_autoload_input_qemu_riscv64_panic(info: &PanicInfo<'_>) -> ! {
        handle_panic_via_serial(info)
    }

    /// Boot entry point — the symbol the arch crate's `boot.s` trampoline
    /// calls (via `rustos_arch_riscv64_main`).
    ///
    /// Forwards the SBI hand-off values (`a0` = hartid, `a1` = DTB) to the
    /// production boot pipeline with the audit-observer sink in place.
    #[no_mangle]
    pub extern "C" fn kernel_main(hartid: u64, dtb: u64) -> ! {
        // The autoloaded driver's arm step (`irq_bind`) is an audited syscall
        // whose `SyscallInvoked` record is `Debug`, below the default `Info`
        // filter; the harness waits for that record's `sc=irq_bind` serial
        // marker before injecting the key, so boot with the filter lowered.
        boot_riscv64::boot(
            hartid,
            dtb,
            &SERIAL_SINK,
            &AUDIT_SINK,
            rustos_log::Level::Debug,
        )
    }
}

// --- Host stub -----------------------------------------------------
#[cfg(not(itest_riscv64))]
fn main() {}

#[cfg(not(itest_riscv64))]
#[allow(dead_code)]
fn _suppress_no_main() {}
