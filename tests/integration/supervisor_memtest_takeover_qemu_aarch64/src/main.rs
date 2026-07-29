//! `plans/NEW-SUPERVISOR.md` §9 Stage E QEMU integration test: boot the
//! production aarch64 `virt` pipeline and drive the pre-boot Supervisor's
//! one-way **`memtest` takeover**, proving the aarch64 `MachineTakeover`
//! mechanism takes the whole machine over and tests all of RAM continuously.
//! The test never stops on its own — the machine only leaves it by a reset —
//! so the harness resets the VM once the guest has completed one full test
//! loop, which registers as a **machine reset**.
//!
//! ## What this vertical asserts
//!
//! It exercises the aarch64 machine-takeover mechanism end to end — the one
//! part of `memtest` that can only be proven on real silicon/QEMU (the
//! confirmation, command parsing, and the arch-neutral sweep are host-tested
//! in `lib/supervisor` and `kernel/mem`). On `AuditEvent::BootCompleted`
//! (`EventId(4004)`) — the point at which the Supervisor system is published
//! and the kernel state (frame allocator, boot memory map, direct physical
//! map) is fully built — the audit sink drives the real published
//! `SupervisorSystem::memtest_takeover` seam, exactly as the `memtest` command
//! does (there is only the one whole-RAM test and no confirmation prompt; the
//! command dispatch is host-tested in `lib/supervisor`, and the takeover is
//! driven directly here because it never returns to key the REPL).
//!
//! The guest boots single-core (embedded 1-CPU DTB), so the cross-CPU quiesce
//! runs its "no online peers, succeed immediately" path here — the same
//! handshake code the x86_64 sibling proves with real secondaries. Keeping
//! this continuous-memtest guest single-core also keeps it a light citizen in
//! the parallel QEMU matrix.
//!
//! On the wired aarch64 port `memtest_takeover` **never returns**: the caller
//! quiesces every other core (none here), then the `MachineTakeover` body masks
//! interrupts and stops the watchdog cadence, flattens paging (MMU off), and
//! tests every usable frame on a reserved stack — cycling every pattern over
//! all of RAM, over and over, rendering the memtest86-style display (elapsed
//! timer, completed-loop count, and error log) to this serial console. Once
//! the guest prints the completed-test-loop marker the harness issues a QEMU
//! monitor `system_reset`; under `-no-reboot` that reset exits the host
//! process with status 0 and the runner registers `Outcome::Pass`.
//!
//! A boot that never reached a completed test loop would fall silent and time
//! out (`Outcome::Timeout`), and a takeover that *returned* (refused/
//! unsupported) makes the sink write a fail finisher — so a regression that
//! stops the test running fails loud rather than passing by accident.
//!
//! ## Embedded `virt` device tree
//!
//! QEMU's `-kernel <ELF>` aarch64 path passes no DTB pointer (`x0 = 0`), so
//! the canonical `virt` device tree is dumped and embedded at build time
//! (`build.rs`, a single CPU node) and its address handed to the boot
//! pipeline, exactly as the ESC boot-screen vertical does. The takeover never
//! resets the board itself (the harness does), so it needs no reset conduit —
//! and it is available on a real spin-table Pi 4 whose tree has no `/psci`
//! node at all.
//!
//! ## How it differs from a production kernel
//!
//! It reuses the entire production aarch64 boot pipeline and only replaces
//! the audit sink. Splitting the observer behaviour into a separate bin
//! (instead of a Cargo feature on a production crate) prevents feature
//! unification from leaking the takeover trigger into any production build
//! (fail closed; the harness never decides what the kernel does next).

#![cfg_attr(itest_aarch64, no_std)]
#![cfg_attr(itest_aarch64, no_main)]
#![deny(missing_docs)]

// --- Freestanding test bin (`aarch64-unknown-none`) ----------------

#[cfg(itest_aarch64)]
mod kernel {
    use core::panic::PanicInfo;

    use tairix_arch_aarch64::{
        handle_panic_via_serial, qemu_exit, serial, SerialSink, SERIAL_SINK,
    };
    use tairix_kalloc::{FreeListAllocator, Heap, HEAP_BYTES};
    use tairix_kernel::aarch64::boot as boot_aarch64;
    use tairix_log::{Event, EventId, Sink};
    use tairix_supervisor::Report;

    // The canonical QEMU `virt` device tree, dumped and embedded at build
    // time (`build.rs`). The boot pipeline discovers the board from it
    // because QEMU passes no `x0` DTB pointer at an ELF `-kernel` entry.
    include!(concat!(env!("OUT_DIR"), "/dtb_fixture.rs"));

    /// Static boot heap, mirroring the production aarch64 kernel binary's
    /// `.bss`-resident heap (zeroed by the boot trampoline). It lives inside
    /// the kernel image, which the boot memory map reserves, so the whole-RAM
    /// sweep (which tests only *usable* frames) never overwrites it.
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

    /// `EventId` emitted by `kernel_core::kernel_main` once every init phase
    /// completed (the Supervisor system is published in an earlier phase, so
    /// it is live by the time this record is emitted). Pinned by the
    /// `event_ids_are_unique` test in `kernel/core/src/audit.rs`.
    const BOOT_COMPLETED_EVENT_ID: EventId = EventId(4004);

    /// A [`Report`] sink that streams the takeover's memtest86-style display
    /// straight to the UART console, so the QEMU transcript records it. It
    /// uses the bounded, non-blocking `write_console_bytes` (which falls back
    /// to a direct, lock-free UART write when the ring is momentarily held),
    /// so it never spins even after paging is flattened and interrupts are
    /// masked; the display is best-effort and the PASS keys on the reset.
    struct SerialReport;

    impl Report for SerialReport {
        fn write_bytes(&mut self, bytes: &[u8]) {
            let _ = serial::write_console_bytes(bytes);
        }
    }

    /// Sink that replays every event through [`SERIAL_SINK`] and, on
    /// [`BOOT_COMPLETED_EVENT_ID`], drives the `memtest` takeover through the
    /// published `SupervisorSystem` seam.
    struct MemtestTakeoverSink;

    impl Sink for MemtestTakeoverSink {
        fn write_event(&self, event: &Event<'_>) {
            // Replay through the serial sink so the QEMU transcript records
            // the full boot + takeover timeline.
            SerialSink::new().write_event(event);
            if event.id != BOOT_COMPLETED_EVENT_ID {
                return;
            }
            // Drive the real published takeover seam. On the wired aarch64
            // port this never returns: it tests RAM continuously until the
            // harness issues a monitor `system_reset` after a completed loop
            // (QEMU `-no-reboot` exits 0 = PASS).
            let mut report = SerialReport;
            if let Some(system) = tairix_kernel_core::supervisor_system() {
                system.memtest_takeover(&mut report);
            }
            // Reaching here means the takeover was refused or unsupported —
            // a regression on a port that is supposed to reset. Fail loud.
            qemu_exit::exit_failure(1);
        }
    }

    static AUDIT_SINK: MemtestTakeoverSink = MemtestTakeoverSink;

    /// Forward to the shared aarch64 panic bridge. A panic before the reset
    /// parks the CPU, the run times out, and the harness reports
    /// `Outcome::Timeout` — the documented fail-loud behaviour.
    #[panic_handler]
    fn tairix_supervisor_memtest_takeover_qemu_aarch64_panic(info: &PanicInfo<'_>) -> ! {
        handle_panic_via_serial(info)
    }

    /// Boot entry point — the symbol the arch crate's `boot.s` trampoline
    /// calls (via `tairix_arch_aarch64_main`).
    ///
    /// QEMU hands no DTB pointer (`_dtb == 0`), so the embedded `virt` blob's
    /// address is forwarded to the production boot pipeline with the
    /// takeover-driving audit sink in place.
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

#[cfg(not(itest_aarch64))]
#[allow(dead_code)]
fn _suppress_no_main() {}
