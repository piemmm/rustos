//! `plans/NEW-SUPERVISOR.md` §9 Stage E QEMU integration test: boot the
//! production riscv64 `virt` pipeline and drive the pre-boot Supervisor's
//! one-way **destructive `memtest full` takeover**, proving the riscv64
//! `MachineTakeover` mechanism takes the whole machine over, tests all of
//! RAM, and ends in a **machine reset**.
//!
//! ## What this vertical asserts
//!
//! It exercises the riscv64 machine-takeover mechanism end to end — the one
//! part of `memtest full` that can only be proven on real silicon/QEMU (the
//! confirmation, command parsing, and the arch-neutral sweep are host-tested
//! in `lib/supervisor` and `kernel/mem`). On `AuditEvent::BootCompleted`
//! (`EventId(4004)`) — the point at which the Supervisor system is published
//! and the kernel state (frame allocator, boot memory map, direct physical
//! map) is fully built — the audit sink drives the real published
//! `SupervisorSystem::memtest_takeover` seam, exactly as the confirmed
//! `memtest full` command does after its typed `DESTROY` confirmation (which
//! is itself host-tested in `lib/supervisor`; the riscv64 SBI console offers
//! no interactive input, so the REPL cannot be driven by keystrokes on this
//! port — the seam is driven directly instead).
//!
//! On the wired riscv64 port `memtest_takeover` **never returns**: the
//! `MachineTakeover` body quiesces (single-hart, nothing to stop), masks
//! S-mode interrupts, flattens paging to bare mode, destructively tests every
//! usable frame on a reserved stack (rendering the memtest86-style display to
//! this serial console), tests the kernel-image region with the relocated
//! stub, and issues an SBI System-Reset. QEMU runs the riscv64 board with
//! `-no-reboot`, so that reset exits the host process with status 0 and the
//! runner registers `Outcome::Pass`.
//!
//! A normal boot that reached idle without resetting would time out
//! (`Outcome::Timeout`), and a takeover that *returned* (refused/unsupported)
//! makes the sink write a fail finisher — so a regression that stops the port
//! resetting fails loud rather than passing by accident.
//!
//! ## How it differs from a production kernel
//!
//! It reuses the entire production riscv64 boot pipeline and only replaces
//! the audit sink. Splitting the observer behaviour into a separate bin
//! (instead of a Cargo feature on a production crate) prevents feature
//! unification from leaking the takeover trigger into any production build
//! (fail closed; the harness never decides what the kernel does next).

#![cfg_attr(itest_riscv64, no_std)]
#![cfg_attr(itest_riscv64, no_main)]
#![deny(missing_docs)]

// --- Freestanding test bin (`riscv64gc-unknown-none-elf`) ----------

#[cfg(itest_riscv64)]
mod kernel {
    use core::panic::PanicInfo;

    use tairix_arch_riscv64::{
        handle_panic_via_serial, qemu_exit, serial, SerialSink, SERIAL_SINK,
    };
    use tairix_kalloc::{FreeListAllocator, Heap, HEAP_BYTES};
    use tairix_log::{Event, EventId, Sink};
    use tairix_supervisor::Report;
    use tairix_test_riscv64_boot::boot;

    /// Static boot heap, placed in the linker's dedicated `.heap` (NOLOAD)
    /// section (excluded from the usable physical-memory map, so the
    /// destructive sweep never overwrites it). `static mut` because the
    /// bump allocator hands out disjoint slices via an atomic cursor.
    #[link_section = ".heap"]
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
    /// straight to the SBI console, so the QEMU transcript records it.
    struct SerialReport;

    impl Report for SerialReport {
        fn write_bytes(&mut self, bytes: &[u8]) {
            let _ = serial::write_console_bytes(bytes);
        }
    }

    /// Sink that replays every event through [`SERIAL_SINK`] and, on
    /// [`BOOT_COMPLETED_EVENT_ID`], drives the destructive `memtest full`
    /// takeover through the published `SupervisorSystem` seam.
    struct MemtestTakeoverSink;

    impl Sink for MemtestTakeoverSink {
        fn write_event(&self, event: &Event<'_>) {
            // Replay through the serial sink so the QEMU transcript records
            // the full boot + takeover timeline.
            SerialSink::new().write_event(event);
            if event.id != BOOT_COMPLETED_EVENT_ID {
                return;
            }
            // Drive the real published takeover seam. On the wired riscv64
            // port this never returns (the machine resets → QEMU `-no-reboot`
            // exits 0 = PASS).
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

    /// Forward to the shared riscv64 panic bridge. A panic before the reset
    /// parks the hart, the run times out, and the harness reports
    /// `Outcome::Timeout` — the documented fail-loud behaviour.
    #[panic_handler]
    fn tairix_supervisor_memtest_takeover_qemu_riscv64_panic(info: &PanicInfo<'_>) -> ! {
        handle_panic_via_serial(info)
    }

    /// Boot entry point — the symbol the arch crate's `boot.s` trampoline
    /// calls (via `tairix_arch_riscv64_main`). Forwards the SBI hand-off
    /// values to the production boot pipeline with the takeover-driving audit
    /// sink in place.
    #[no_mangle]
    pub extern "C" fn kernel_main(hartid: u64, dtb: u64) -> ! {
        boot(
            hartid,
            dtb,
            &SERIAL_SINK,
            &AUDIT_SINK,
            tairix_log::Level::Info,
        )
    }
}

// --- Host stub -----------------------------------------------------
#[cfg(not(itest_riscv64))]
fn main() {}

#[cfg(not(itest_riscv64))]
#[allow(dead_code)]
fn _suppress_no_main() {}
