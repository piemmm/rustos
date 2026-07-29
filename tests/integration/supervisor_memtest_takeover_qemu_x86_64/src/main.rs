//! `plans/NEW-SUPERVISOR.md` §9 Stage E QEMU integration test: boot the
//! production x86_64 `tairix-kernel` pipeline and drive the pre-boot
//! Supervisor's one-way **`memtest` takeover**, proving the
//! x86_64 `MachineTakeover` mechanism takes the whole machine over and tests
//! all of RAM continuously. The x86_64 sibling of the aarch64/riscv64 takeover
//! verticals.
//!
//! ## What this vertical asserts
//!
//! It exercises the x86_64 machine-takeover mechanism end to end — the one
//! part of `memtest` that can only be proven on QEMU (the confirmation,
//! command parsing, and the arch-neutral sweep are host-tested in
//! `lib/supervisor` and `kernel/mem`). On `AuditEvent::BootCompleted`
//! (`EventId(4004)`) — the point at which the Supervisor system is published
//! and the kernel state (frame allocator, boot memory map, direct physical
//! map) is fully built — the audit sink drives the real published
//! `SupervisorSystem::memtest_takeover` seam, exactly as the `memtest` command
//! does (there is only the one whole-RAM test and no confirmation prompt;
//! the command dispatch is host-tested in `lib/supervisor`, and the takeover
//! is driven directly here because the takeover run cannot return to key
//! the REPL).
//!
//! The guest boots with four CPUs, so this genuinely exercises the cross-CPU
//! quiesce: `drive_takeover` stops the three application processors through
//! the bounded IPI-halt handshake before the tear-down begins.
//!
//! On the wired x86_64 port `memtest_takeover` **never returns**: the caller
//! quiesces every other CPU, then the `MachineTakeover` body masks interrupts
//! (`cli`), switches onto a reserved `.bss` stack, installs the reserved boot
//! page tables, and tests every usable frame on that stack — cycling every
//! pattern over all of RAM, over and over, rendering the memtest86-style
//! display (elapsed timer, completed-loop count, error log) to COM1. The
//! takeover never resets the board itself; once the guest prints the
//! completed-test-loop marker the harness issues a QEMU-monitor `system_reset`,
//! and under `-no-reboot` that reset exits the host process so the runner
//! registers `Outcome::Pass`.
//!
//! A boot that never reached a completed test loop would fall silent and time
//! out (`Outcome::Timeout`), and a takeover that *returned* (refused/
//! unsupported) makes the sink write a fail finisher — so a regression that
//! stops the test running fails loud rather than passing by accident.
//!
//! ## How it differs from a production kernel
//!
//! It reuses the entire production x86_64 boot pipeline and only replaces the
//! audit sink. Splitting the observer behaviour into a separate bin (instead
//! of a Cargo feature on a production crate) prevents feature unification from
//! leaking the takeover trigger into any production build (fail closed; the
//! harness never decides what the kernel does next).

#![cfg_attr(itest_x86_64, no_std)]
#![cfg_attr(itest_x86_64, no_main)]
#![deny(missing_docs)]

// --- Freestanding test bin (`x86_64-unknown-none`) -----------------

#[cfg(itest_x86_64)]
mod kernel {
    use core::panic::PanicInfo;

    use tairix_arch_x86_64::qemu_exit;
    use tairix_arch_x86_64::serial::{Serial, COM1_BASE};
    use tairix_kernel::kalloc::{Heap, HEAP_BYTES};
    use tairix_kernel::{
        boot, handle_panic_via_kernel_core, FreeListAllocator, SerialSink, SERIAL_SINK,
    };
    use tairix_log::{Event, EventId, Sink};
    use tairix_supervisor::Report;

    /// Static heap for the bump allocator (identical to the production bin's
    /// declaration; `#[global_allocator]` is per-binary).
    ///
    /// It lives inside the kernel image (`.bss`), which the boot memory map
    /// reserves, so the whole-RAM sweep (which tests only *usable* frames)
    /// never overwrites it.
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
    /// straight to COM1, so the QEMU transcript records it. It uses the
    /// polled 16550 transmit path directly (no interrupt, no lock), so it
    /// never spins on a wake source even after interrupts are masked and the
    /// page tables are switched; the display is best-effort and the PASS keys
    /// on the reset.
    struct SerialReport;

    impl Report for SerialReport {
        fn write_bytes(&mut self, bytes: &[u8]) {
            let mut uart = Serial::at(COM1_BASE);
            for &b in bytes {
                uart.write_byte(b);
            }
        }
    }

    /// Sink that replays every event through [`SERIAL_SINK`] and, on
    /// [`BOOT_COMPLETED_EVENT_ID`], drives the `memtest`
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
            // Drive the real published takeover seam. On the wired x86_64
            // port this never returns: it tests RAM continuously until the
            // harness issues a monitor `system_reset` after a completed loop
            // (QEMU `-no-reboot` exits = PASS).
            let mut report = SerialReport;
            if let Some(system) = tairix_kernel_core::supervisor_system() {
                system.memtest_takeover(&mut report);
            }
            // Reaching here means the takeover was refused or unsupported —
            // a regression on a port that is supposed to reset. Fail loud.
            qemu_exit::exit_failure();
        }
    }

    static AUDIT_SINK: MemtestTakeoverSink = MemtestTakeoverSink;

    /// Forward to the shared bridge in `tairix_kernel::x86_64::panic_ctx`.
    /// The bridge logs through `SERIAL_SINK`, not `AUDIT_SINK`, so a panic
    /// before the reset does not trip the takeover trigger — it halts, the
    /// run times out, and the harness reports `Outcome::Timeout` (fail-loud).
    #[panic_handler]
    fn tairix_supervisor_memtest_takeover_qemu_x86_64_panic(info: &PanicInfo<'_>) -> ! {
        handle_panic_via_kernel_core(info)
    }

    /// The symbol the arch crate's boot trampoline calls. Forwards to
    /// [`tairix_kernel::boot`] with the production COM1 log sink and the
    /// takeover-driving audit sink, so the boot pipeline installs the real
    /// `SupervisorHost` and publishes the live `SupervisorSystem` exactly as
    /// in production.
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
