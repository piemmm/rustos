//! `plans/PI.md` P10 5d-2-ii(b-2-iii) QEMU integration test: boot the
//! production aarch64 `rustos-kernel` pipeline on the `virt` board with a
//! planted whole-disk encrypted-root image that carries a **kernel-signed
//! virtio-input keyboard driver bundle** in `/System/Drivers/`, and prove the
//! full **driver-loading-by-discovery autoload path** spawns that user-space
//! driver, which delivers an injected keystroke to the kernel input-focus
//! arbiter.
//!
//! ## What this test asserts — and how it differs from its siblings
//!
//! * `root_unlock_admission_qemu_aarch64` proves the in-kernel unlock kthread
//!   mounts the encrypted root and installs the users database (the
//!   *root-mount* path). It attaches no keyboard and plants no driver store.
//! * `input_virtio_mmio_qemu_aarch64` proves a discovered virtio-input device
//!   reaches the *in-kernel* scaffold decode path.
//! * `driver_spawn_qemu_aarch64` proves a discovered node → signed gate →
//!   process spawn handshake with a stub program.
//!
//! This vertical composes them on the production boot path: it attaches the
//! shared `rustos_test_autoload_root_image` whole-disk image (a three-partition
//! disk whose **read-only `/System` volume** carries the signed `virtio_kbd`
//! bundle at the volume-relative `Drivers/input/virtio_kbd/Run`, design B) as a
//! virtio-blk-mmio device **and** a `virtio-keyboard-device`, and boots
//! `boot_aarch64::boot` verbatim. The production path then:
//!
//! 1. **Discovers** the virtio-block root *and* the virtio-input keyboard node
//!    (bootstrap-floor virtio-MMIO enumeration). The input node carries its
//!    register window, a coherent DMA constraint, **and** its discovered GICv2
//!    interrupt line as capability-grant requests; the full
//!    hardware tree is stashed for the init seam.
//! 2. **Admits the unlock kthread**, which brings the root block device up over
//!    the device-IRQ path, mounts the read-only `/System` volume, and serves
//!    its signed driver store over the capability-gated IPC endpoint.
//! 3. **Reactive user-space autoload (Design D)**: the long-running
//!    `devmgr` service reads the hardware tree, lists the `/System` store over
//!    the IPC service, matches the signed `virtio_kbd` bundle to the discovered
//!    virtio-input node (`lib/devmatch`), and asks the kernel to load it; the
//!    kernel re-runs the full signed gate (verified against the embedded
//!    `KERNEL_DRIVER_SIGNER_PUBKEY`) and **spawns it into its own user-space
//!    process** with exactly that node's resource grants (register window, DMA,
//!    and the IRQ line) plus the delegated `CAP_INPUT_INJECT`.
//! 4. The spawned driver maps its register window, brings the virtio-input
//!    device up, **binds its granted interrupt line and parks on `irq_wait`**
//!    (interrupt-driven, never a busy poll), and on
//!    each device interrupt pumps decoded key edges into the arbiter via
//!    `key_inject`.
//!
//! ## Why the PASS keys on the first-delivery witness
//!
//! The audit sink reports PASS once it sees `AuditEvent::InputDelivered`
//! (`EventId` 4050) — the one-shot witness the
//! `key_inject` handler emits the first time an input driver delivers a key
//! edge to the arbiter (it carries no key content, count, or
//! timing). Reaching it requires every preceding step to have succeeded: the
//! `/System` volume mounted and served, the store listed, the signed bundle
//! verified, the node matched, the user-space driver spawned and granted its
//! resources plus `CAP_INPUT_INJECT`, the device brought up, its interrupt
//! bound and routed, and the injected keystroke decoded and delivered. The
//! harness injects the key only once the driver has armed its interrupt (the
//! audited `irq_bind` syscall, `AUTOLOAD_INPUT_KEY_MARKER`), so the device is
//! active with posted buffers and the keypress is delivered rather than dropped
//! against an un-ready device. A run where any step fails never reaches that
//! witness, so the harness times out — the documented fail-loud behaviour.
//!
//! ## Embedded `virt` device tree
//!
//! QEMU's `-kernel <ELF>` aarch64 path passes no DTB pointer (`x0 = 0`), so the
//! canonical `virt` device tree is dumped and embedded at build time
//! (`build.rs`) and its address handed to the boot pipeline. The tree describes
//! the board's `virtio,mmio` transport slots; the planted disk and the attached
//! keyboard populate two slots' live `DeviceID`s, which the bootstrap-floor
//! enumeration reads.
//!
//! ## How it differs from a production kernel
//!
//! It reuses the entire production aarch64 boot pipeline and only replaces the
//! audit sink. Splitting the audit-observer behaviour into a separate bin
//! (instead of a Cargo feature on a production crate) prevents feature
//! unification from leaking the QEMU-exit shortcut into any production build
//! (fail closed; the harness never decides what the kernel
//! does next).

#![cfg_attr(itest_aarch64, no_std)]
#![cfg_attr(itest_aarch64, no_main)]
#![deny(missing_docs)]

// --- Freestanding test bin (`aarch64-unknown-none`) ----------------

#[cfg(itest_aarch64)]
mod kernel {
    use core::panic::PanicInfo;

    use rustos_arch_aarch64::{handle_panic_via_serial, qemu_exit, SerialSink, SERIAL_SINK};
    use rustos_kalloc::{FreeListAllocator, Heap, HEAP_BYTES};
    use rustos_kernel::aarch64::boot as boot_aarch64;
    use rustos_kernel_core::AuditEvent;
    use rustos_log::{Event, Sink};

    // The canonical QEMU `virt` device tree, dumped and embedded at build
    // time (`build.rs`). The boot pipeline discovers the board from it
    // because QEMU passes no `x0` DTB pointer at an ELF `-kernel` entry.
    include!(concat!(env!("OUT_DIR"), "/dtb_fixture.rs"));

    /// Static boot heap, mirroring the production aarch64 kernel binary's
    /// `.bss`-resident heap (zeroed by the boot trampoline).
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
    /// to QEMU the instant the one-shot first-input-delivery witness appears —
    /// proof that the autoloaded user-space virtio-input driver came up and
    /// delivered a key edge to the input-focus arbiter (the full
    /// discovery → signed gate → spawn → inject path).
    struct AutoloadInputSink;

    impl Sink for AutoloadInputSink {
        fn write_event(&self, event: &Event<'_>) {
            // Replay through the serial sink so the QEMU transcript records the
            // full boot + unlock + autoload + input timeline.
            SerialSink::new().write_event(event);
            if event.id.0 == AuditEvent::InputDelivered.id().0 {
                qemu_exit::exit_success();
            }
        }
    }

    static AUDIT_SINK: AutoloadInputSink = AutoloadInputSink;

    /// Forward to the shared aarch64 panic bridge. A panic before the PASS
    /// finisher parks the CPU, the run times out, and the harness reports
    /// `Outcome::Timeout` — the documented fail-loud behaviour.
    #[panic_handler]
    fn rustos_autoload_input_qemu_aarch64_panic(info: &PanicInfo<'_>) -> ! {
        handle_panic_via_serial(info)
    }

    /// Boot entry point — the symbol the arch crate's `boot.s` trampoline
    /// calls (via `rustos_arch_aarch64_main`).
    ///
    /// QEMU hands no DTB pointer (`_dtb == 0`), so the embedded `virt` blob's
    /// address is forwarded to the production boot pipeline with the
    /// audit-observer sink in place.
    #[no_mangle]
    pub extern "C" fn kernel_main(_dtb: u64) -> ! {
        let dtb = DTB_BLOB.as_ptr() as u64;
        boot_aarch64::boot(dtb, &SERIAL_SINK, &AUDIT_SINK)
    }
}

// --- Host stub -----------------------------------------------------
#[cfg(not(itest_aarch64))]
fn main() {}

#[cfg(not(itest_aarch64))]
#[allow(dead_code)]
fn _suppress_no_main() {}
