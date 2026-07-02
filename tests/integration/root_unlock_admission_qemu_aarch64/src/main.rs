//! `plans/PI.md` P11 Chunk B-2 INCREMENT (2) QEMU integration test: boot
//! the production aarch64 `rustos-kernel` pipeline on the `virt` board with
//! a planted whole-disk encrypted-root image, and prove the **in-kernel
//! root-unlock kthread admission path** mounts the root so `root`/`root`
//! authenticates.
//!
//! ## What this test asserts — and how it differs from its siblings
//!
//! Three landed verticals each prove one slice; this one proves they
//! compose on the *production boot path*:
//!
//! * `irq_kthread_qemu_aarch64` proves a discovered device SPI wakes a
//!   parked in-kernel kthread (the device-IRQ path).
//! * `root_unlock_login_qemu_aarch64` drives the interactive unlock
//!   **policy** (`unlock_root_disk_interactively`) *directly* over the
//!   planted disk — it does not exercise the kthread admission.
//! * `spawn_session_qemu_aarch64` boots the production pipeline with **no**
//!   disk, so `unlock_service::spawn_if_present` is a no-op.
//!
//! This vertical attaches the shared `rustos_test_encrypted_root_image`
//! whole-disk image as a virtio-blk-mmio device and boots
//! `boot_aarch64::boot` verbatim. The production path then:
//!
//! 1. **Discovers + binds the root.** The bootstrap-floor virtio-MMIO bus
//!    enumeration (`root_storage::observe_virtio_mmio_block_devices`,
//!    driven from `boot::audit_root_storage_binding`) probes the populated
//!    slot's `DeviceID`, attaches the probed virtio-block child node, binds
//!    the virtio-blk driver, and stashes the binding for the init seam.
//! 2. **Admits the unlock kthread.** The init seam runs
//!    `unlock_service::spawn_if_present`, which admits the in-kernel
//!    root-unlock kthread onto the boot CPU's run queue (the console-0
//!    `login` is held behind the ownership gate meanwhile).
//! 3. **Mounts + installs.** On its first dispatch the kthread brings the
//!    virtio-blk device up over the production device-IRQ path, prompts
//!    `Root passphrase: ` on the primary (UART) console, reads the
//!    passphrase the runner types, mounts the encrypted `RustFS` root,
//!    installs the users database into `LATE_USERS_DB`, and opens the
//!    console-0 gate — logging the `USERS_DB_INSTALLED_MESSAGE` audit line.
//!
//! ## Why the PASS keys on the install message
//!
//! The audit sink reports PASS once it has seen the unlock-service
//! `USERS_DB_INSTALLED_MESSAGE` (`EventId(4139)`) — the witness that the
//! **in-kernel kthread**, admitted by `spawn_if_present`, brought the
//! discovered virtio-blk root up over the production device-IRQ path,
//! prompted, read the typed passphrase, mounted the encrypted `RustFS`
//! root, and installed the users database into `LATE_USERS_DB`. That is the
//! full *kthread-admission* path this vertical exists to prove (distinct
//! from `root_unlock_login`, which drives the unlock policy directly). A run
//! where discovery never binds the disk, the kthread is never admitted, the
//! device IRQ never wakes it, or the mount fails never reaches that message,
//! so the harness times out — the documented fail-loud behaviour.
//!
//! The *content* of the installed database — that `root`/`root`
//! authenticates and a wrong password is refused — is proven over the same
//! shared fixture by `root_unlock_login` (which inspects the installed cell
//! directly). Driving the per-console `login` to authenticate end to end
//! additionally needs the userland heap to parse the served database, which
//! rides the production `mem_map` producer (`plans/SPAWN.md` `SP5b`, not yet
//! landed); until then `login` runs allocation-free and refuses every
//! attempt. This vertical therefore keys on the
//! install witness, not a `login` success.
//!
//! ## Embedded `virt` device tree
//!
//! QEMU's `-kernel <ELF>` aarch64 path passes no DTB pointer (`x0 = 0`), so
//! the canonical `virt` device tree is dumped and embedded at build time
//! (`build.rs`) and its address handed to the boot pipeline. The tree
//! always describes the board's `virtio,mmio` transport slots; the planted
//! disk populates one slot's live `DeviceID`, which the bootstrap-floor
//! enumeration reads.
//!
//! ## How it differs from a production kernel
//!
//! It reuses the entire production aarch64 boot pipeline and only replaces
//! the audit sink. Splitting the audit-observer behaviour into a separate
//! bin (instead of a Cargo feature on a production crate) prevents feature
//! unification from leaking the QEMU-exit shortcut into any production build
//! (fail closed; the harness never decides what the
//! kernel does next).

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
    use rustos_kernel::unlock_service::USERS_DB_INSTALLED_MESSAGE;
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

    /// Sink that replays every event through [`SERIAL_SINK`] and reports
    /// PASS to QEMU the instant the unlock-service install message appears —
    /// the witness that the in-kernel unlock kthread, admitted by
    /// `spawn_if_present`, brought the discovered virtio-blk root up over the
    /// production device-IRQ path, read the typed passphrase, mounted the
    /// encrypted root, and installed the users database. (The database
    /// *content* authenticating `root`/`root` is proven by
    /// `root_unlock_login`; driving `login` end to end rides SP5b — see the
    /// module docs.)
    struct UnlockAdmissionSink;

    impl Sink for UnlockAdmissionSink {
        fn write_event(&self, event: &Event<'_>) {
            // Replay through the serial sink so the QEMU transcript records
            // the full boot + unlock timeline.
            SerialSink::new().write_event(event);
            if event.message == USERS_DB_INSTALLED_MESSAGE {
                qemu_exit::exit_success();
            }
        }
    }

    static AUDIT_SINK: UnlockAdmissionSink = UnlockAdmissionSink;

    /// Forward to the shared aarch64 panic bridge. A panic before the PASS
    /// finisher parks the CPU, the run times out, and the harness reports
    /// `Outcome::Timeout` — the documented fail-loud behaviour.
    #[panic_handler]
    fn rustos_root_unlock_admission_qemu_aarch64_panic(info: &PanicInfo<'_>) -> ! {
        handle_panic_via_serial(info)
    }

    /// Boot entry point — the symbol the arch crate's `boot.s` trampoline
    /// calls (via `rustos_arch_aarch64_main`).
    ///
    /// QEMU hands no DTB pointer (`_dtb == 0`), so the embedded `virt`
    /// blob's address is forwarded to the production boot pipeline with the
    /// audit-observer sink in place.
    #[no_mangle]
    pub extern "C" fn kernel_main(_dtb: u64) -> ! {
        let dtb = DTB_BLOB.as_ptr() as u64;
        boot_aarch64::boot(
            dtb,
            &SERIAL_SINK,
            &AUDIT_SINK,
            &rustos_kernel::hwtree_store::HW_TREE_SOURCE,
        )
    }
}

// --- Host stub -----------------------------------------------------
#[cfg(not(itest_aarch64))]
fn main() {}

#[cfg(not(itest_aarch64))]
#[allow(dead_code)]
fn _suppress_no_main() {}
