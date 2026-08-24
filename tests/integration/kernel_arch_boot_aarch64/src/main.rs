//! `plans/PI.md` P6c-2 QEMU integration test: boot the aarch64 (Raspberry
//! Pi 4) `tairix-kernel` pipeline on the `virt` board to
//! `AuditEvent::BootCompleted` and report success to QEMU.
//!
//! ## What this test asserts
//!
//! `kernel_core::kernel_main` emits `AuditEvent::BootCompleted`
//! (`EventId(4004)`) once every init phase (Log → Mem → Sec → Sched →
//! Irq → Syscall → Ipc) has succeeded. This binary drives the real
//! aarch64 boot pipeline — `tairix_kernel::aarch64::boot::boot` — end to
//! end on the `virt` board:
//!
//! 1. The arch crate's `boot.s` trampoline drops to EL1, establishes a
//!    stack, zeroes `.bss`, and calls `kernel_main`.
//! 2. `boot_aarch64::boot` enables the stage-1 identity MMU + EL1
//!    vectors, discovers the board from the device tree, builds the
//!    `BootMemoryMap`, installs the discovered-UART console + the `svc`
//!    dispatch callback, and hands a validated `BootInfo` to
//!    `kernel_core::kernel_main`.
//! 3. The audit sink observes `BootCompleted`, requires the ramfb
//!    framebuffer boot console to be active (the harness attaches
//!    `-device ramfb`, so the pre-MMU video bring-up must have found
//!    the tree's `fw_cfg` node and programmed the scan-out — the
//!    display path `cargo xtask run` relies on — an inactive video
//!    console is reported as FAIL), then waits for the production SMP
//!    bring-up: the run is `-smp 4`, so `kernel_main` PSCI-starts the
//!    three secondaries the embedded tree's `/cpus` declares, and each
//!    attests its arrival in the kernel dispatch loop with
//!    `AuditEvent::SecondaryCpuOnline` (`EventId(4072)`). The PASS
//!    finisher fires only once all three are online — the end-to-end
//!    proof the production boot brings every discovered core into
//!    service; a `SecondaryCpuStartFailed` (`EventId(4071)`) is an
//!    immediate FAIL.
//!
//! A regression that fails any init phase — or that loses a secondary
//! core — never reaches the finisher, so the run times out and the
//! harness reports `Outcome::Timeout` — the documented fail-loud
//! behaviour.
//!
//! ## Embedded `virt` device tree
//!
//! QEMU's `-kernel <ELF>` aarch64 path passes no DTB pointer (`x0 = 0`),
//! so the canonical `virt` device tree is dumped and embedded at build
//! time (`build.rs`) and its address handed to the boot pipeline, which
//! discovers the console / GIC / `/memory` / timer / PSCI from it exactly
//! as it would from real firmware (`plans/PI.md` P2–P5 watch-out).
//!
//! ## How it differs from a production kernel
//!
//! It reuses the entire production aarch64 boot pipeline; only the audit
//! Sink is replaced. Splitting the audit-observer behaviour into a
//! separate bin (instead of a Cargo feature on the arch crate) prevents
//! feature unification from leaking the QEMU-exit shortcut into any
//! production build (fail closed; the harness never
//! decides what the kernel does next).

#![cfg_attr(itest_aarch64, no_std)]
#![cfg_attr(itest_aarch64, no_main)]
#![deny(missing_docs)]

// --- Freestanding test bin (`aarch64-unknown-none`) ----------------

#[cfg(itest_aarch64)]
mod kernel {
    use core::num::NonZeroU16;
    use core::panic::PanicInfo;
    use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

    use tairix_arch_aarch64::{handle_panic_via_serial, qemu_exit, SerialSink, SERIAL_SINK};
    use tairix_itest_finisher::fail_point;
    use tairix_kalloc::{FreeListAllocator, Heap, HEAP_BYTES};
    use tairix_kernel::aarch64::boot as boot_aarch64;
    use tairix_log::{Event, EventId, Sink};

    // The canonical QEMU `virt` device tree, dumped and embedded at build
    // time (`build.rs`). The boot pipeline discovers the board from it
    // because QEMU passes no `x0` DTB pointer at an ELF `-kernel` entry.
    include!(concat!(env!("OUT_DIR"), "/dtb_fixture.rs"));

    /// Static boot heap.
    ///
    /// Lives in `.bss` (zeroed by the boot trampoline) exactly as the
    /// production aarch64 kernel binary's heap does: the `aarch64-virt.ld`
    /// script brackets `.bss` with `__bss_start`/`__bss_end` and places
    /// `__kernel_end` *after* it, so `boot_aarch64`'s `BootMemoryMap`
    /// reserves the whole `[ram_base, __kernel_end)` span — the heap
    /// included — and never hands a heap frame to the allocator. `static
    /// mut` because the bump allocator hands out disjoint slices via an
    /// atomic cursor; the storage is otherwise never aliased.
    static mut HEAP: Heap = Heap::ZERO;

    /// Global allocator backed by [`HEAP`].
    ///
    /// SAFETY: the page-aligned `HEAP` static outlives the binary and
    /// the allocator is its only consumer.
    #[global_allocator]
    static ALLOCATOR: FreeListAllocator =
        unsafe { FreeListAllocator::new(core::ptr::addr_of!(HEAP) as *mut u8, HEAP_BYTES) };

    /// `EventId` emitted by `kernel_core::kernel_main` once every init
    /// phase completed. Pinned by the `event_ids_are_unique` test in
    /// `kernel/core/src/audit.rs`.
    const BOOT_COMPLETED_EVENT_ID: EventId = EventId(4004);

    /// `EventId` of `AuditEvent::SecondaryCpuStartFailed` — a refused
    /// PSCI `CPU_ON` on this fully-emulable board is a regression, not a
    /// degrade to tolerate. Pinned by `event_ids_are_unique`.
    const SECONDARY_START_FAILED_EVENT_ID: EventId = EventId(4071);

    /// `EventId` of `AuditEvent::SecondaryCpuOnline` — each started
    /// secondary core's own attestation that it reached the kernel
    /// dispatch loop. Pinned by `event_ids_are_unique`.
    const SECONDARY_ONLINE_EVENT_ID: EventId = EventId(4072);

    /// Secondary cores the `-smp 4` run must bring online (the embedded
    /// tree's `/cpus` minus the boot core). Matches the harness `cpus`
    /// and the `build.rs` DTB dump — all three name the same topology.
    const EXPECTED_SECONDARIES: u32 = 3;
    /// Failure finisher codes, distinct per failure site.
    const FAIL_VIDEO_INACTIVE: NonZeroU16 = fail_point!(1);
    const FAIL_SECONDARY_START: NonZeroU16 = fail_point!(2);

    /// Set once `BootCompleted` was observed (with the video console
    /// active); the PASS finisher additionally requires every secondary
    /// online.
    static BOOT_COMPLETED: AtomicBool = AtomicBool::new(false);

    /// Count of `SecondaryCpuOnline` attestations observed.
    static SECONDARIES_ONLINE: AtomicU32 = AtomicU32::new(0);

    /// Sink that replays every event through [`SERIAL_SINK`] and reports
    /// PASS to QEMU once `BootCompleted` **and** all
    /// [`EXPECTED_SECONDARIES`] `SecondaryCpuOnline` attestations have
    /// been observed — but only if the ramfb framebuffer boot console
    /// came up.
    ///
    /// The harness attaches `-device ramfb`, so the production pre-MMU
    /// video bring-up must have discovered the `virt` tree's `fw_cfg`
    /// node, programmed the scan-out, and switched the console to the
    /// screen (`video::is_active`). A boot that completed with the
    /// console still on the UART is a display regression reported as
    /// FAIL, not a pass with a dark screen. A `SecondaryCpuStartFailed`
    /// is an immediate FAIL: on the emulated `virt` board with a
    /// discovered PSCI conduit every `CPU_ON` must be accepted.
    ///
    /// `write_event` runs concurrently once the secondaries are live
    /// (each core emits through this same sink), so the completion
    /// bookkeeping is plain atomics and the PASS condition is checked on
    /// both the boot-completed and the online edges — whichever lands
    /// last fires the finisher exactly once (the semihosting exit ends
    /// the whole machine).
    struct BootCompletedExitSink;

    impl BootCompletedExitSink {
        /// Fire the PASS finisher iff boot completed and every
        /// secondary attested.
        fn exit_if_complete() {
            if BOOT_COMPLETED.load(Ordering::SeqCst)
                && SECONDARIES_ONLINE.load(Ordering::SeqCst) >= EXPECTED_SECONDARIES
            {
                qemu_exit::exit_success();
            }
        }
    }

    impl Sink for BootCompletedExitSink {
        fn write_event(&self, event: &Event<'_>) {
            // Replay through the serial sink so the QEMU transcript
            // records the full boot timeline.
            SerialSink::new().write_event(event);
            if event.id == BOOT_COMPLETED_EVENT_ID {
                if !tairix_arch_aarch64::video::is_active() {
                    qemu_exit::exit_failure(FAIL_VIDEO_INACTIVE);
                }
                BOOT_COMPLETED.store(true, Ordering::SeqCst);
                Self::exit_if_complete();
            } else if event.id == SECONDARY_ONLINE_EVENT_ID {
                SECONDARIES_ONLINE.fetch_add(1, Ordering::SeqCst);
                Self::exit_if_complete();
            } else if event.id == SECONDARY_START_FAILED_EVENT_ID {
                qemu_exit::exit_failure(FAIL_SECONDARY_START);
            }
        }
    }

    static AUDIT_SINK: BootCompletedExitSink = BootCompletedExitSink;

    /// Forward to the shared aarch64 panic bridge. A panic before
    /// `BootCompleted` parks the CPU, the run times out, and the harness
    /// reports `Outcome::Timeout` — the documented fail-loud behaviour.
    #[panic_handler]
    fn tairix_kernel_arch_boot_aarch64_panic(info: &PanicInfo<'_>) -> ! {
        handle_panic_via_serial(info)
    }

    /// Boot entry point — the symbol the arch crate's `boot.s`
    /// trampoline calls (via `tairix_arch_aarch64_main`).
    ///
    /// QEMU hands no DTB pointer (`_dtb == 0`), so the embedded `virt`
    /// blob's address is forwarded to the production boot pipeline with
    /// the audit-observer sink in place.
    #[no_mangle]
    pub extern "C" fn kernel_main(_dtb: u64) -> ! {
        let dtb = DTB_BLOB.as_ptr() as u64;
        boot_aarch64::boot(
            dtb,
            &SERIAL_SINK,
            &AUDIT_SINK,
            tairix_log::Level::Info,
            &tairix_kernel::hwtree_store::HW_TREE_SOURCE,
        )
    }
}

// --- Host stub -----------------------------------------------------
#[cfg(not(itest_aarch64))]
fn main() {}
