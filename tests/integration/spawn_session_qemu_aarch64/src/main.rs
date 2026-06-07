//! `plans/SPAWN.md` `SP3b` QEMU integration test: boot the aarch64 (Raspberry
//! Pi 4) `rustos-kernel` pipeline on the `virt` board, spawn PID 1 (`init`)
//! into EL0, have `init` launch the embedded `Shell` session program through
//! the `spawn` syscall, and report success to QEMU once **both** processes
//! have been built and the session has run.
//!
//! ## What this test asserts
//!
//! `boot_aarch64::boot` installs the `InitSpawn` seam **and** the runtime
//! `ProcessSpawn` producer + embedded-program registry into the `BootInfo`
//! hand-off. After `kernel_core::kernel_main` emits
//! `AuditEvent::BootCompleted` it builds PID 1 `init` through the
//! capability-checked, audited spawn caller (emitting
//! `AuditEvent::ProcessSpawned`, `EventId(4030)`, #1) and `eret`s into it.
//! `init` writes its banner, then issues the `spawn` syscall for
//! `/Apps/Shell.app/Run` (an audited syscall, `AuditEvent::SyscallInvoked`,
//! `EventId(5000)`, #1). The runtime `ProcessSpawn` producer builds the
//! session a *fresh, hardware-isolated* address space through the same
//! audited spawn caller (emitting `ProcessSpawned`, #2) and admits it
//! **Ready** — `init` keeps running, so this is a true concurrent spawn, not
//! an `exec`-style hand-off. `init` then exits (`SyscallInvoked` #2), the
//! cooperative drain loop steps the session, which writes its own banner and
//! exits (`SyscallInvoked` #3).
//!
//! ## Why the PASS keys on two spawns and three audited syscalls
//!
//! Two `ProcessSpawned` records prove the producer built a **second**,
//! distinct image. The session's `exit` (the third audited syscall) is on
//! the critical path *only* if the session actually ran: its banner write is
//! gated — `console_write` reports the accepted byte count, and the session
//! parks fail-closed on a short write (`userland/shell/shell/src/run.rs`),
//! so it reaches `exit` only once its banner landed, which in turn requires
//! its *own* isolated address space to have resolved through the kernel-wide
//! registry so the kernel could copy the banner out of the session's memory
//! (`AGENTS.md` §4 — hardware isolation). The three audited syscalls are
//! `init`'s `spawn`, `init`'s `exit`, and the session's `exit`; the session's
//! is necessarily last. A regression that never spawns the session, whose
//! session never runs, or whose banner fails closed never reaches the third
//! `SyscallInvoked`, so the run times out and the harness reports
//! `Outcome::Timeout` — the documented fail-loud behaviour (`AGENTS.md` §7).
//!
//! ## Embedded `virt` device tree
//!
//! QEMU's `-kernel <ELF>` aarch64 path passes no DTB pointer (`x0 = 0`), so
//! the canonical `virt` device tree is dumped and embedded at build time
//! (`build.rs`) and its address handed to the boot pipeline, which discovers
//! the console / GIC / `/memory` / timer / PSCI from it exactly as it would
//! from real firmware.
//!
//! ## How it differs from a production kernel
//!
//! It reuses the entire production aarch64 boot pipeline — including the
//! `InitSpawn` seam and the runtime `ProcessSpawn` producer — and only
//! replaces the audit sink. Splitting the audit-observer behaviour into a
//! separate bin (instead of a Cargo feature on a production crate) prevents
//! feature unification from leaking the QEMU-exit shortcut into any
//! production build (`AGENTS.md` §5.4.5 — fail closed; the harness never
//! decides what the kernel does next).

#![cfg_attr(itest_aarch64, no_std)]
#![cfg_attr(itest_aarch64, no_main)]
#![deny(missing_docs)]

// --- Freestanding test bin (`aarch64-unknown-none`) ----------------

#[cfg(itest_aarch64)]
mod kernel {
    use core::panic::PanicInfo;
    use core::sync::atomic::{AtomicUsize, Ordering};

    use rustos_arch_aarch64::{handle_panic_via_serial, qemu_exit, SerialSink, SERIAL_SINK};
    use rustos_bumpalloc::{BumpAllocator, Heap, HEAP_BYTES};
    use rustos_kernel::boot_aarch64;
    use rustos_log::{Event, EventId, Sink};

    // The canonical QEMU `virt` device tree, dumped and embedded at build
    // time (`build.rs`). The boot pipeline discovers the board from it
    // because QEMU passes no `x0` DTB pointer at an ELF `-kernel` entry.
    include!(concat!(env!("OUT_DIR"), "/dtb_fixture.rs"));

    /// Static boot heap.
    ///
    /// Lives in `.bss` (zeroed by the boot trampoline) exactly as the
    /// production aarch64 kernel binary's heap does. `static mut` because the
    /// bump allocator hands out disjoint slices via an atomic cursor; the
    /// storage is otherwise never aliased.
    static mut HEAP: Heap = Heap::ZERO;

    /// Global allocator backed by [`HEAP`].
    ///
    /// SAFETY: the page-aligned `HEAP` static outlives the binary and the
    /// allocator is its only consumer.
    #[global_allocator]
    static ALLOCATOR: BumpAllocator =
        unsafe { BumpAllocator::new(core::ptr::addr_of!(HEAP) as *mut u8, HEAP_BYTES) };

    /// `EventId` emitted by the spawn caller once an EL0 image is built.
    /// Pinned by the `event_ids_are_unique` test in `kernel/core/src/audit.rs`.
    const PROCESS_SPAWNED_EVENT_ID: EventId = EventId(4030);

    /// `EventId` emitted by the syscall dispatcher for an audited syscall —
    /// `init`'s `spawn` and `exit`, and the session's `exit`. Pinned by the
    /// audit-id test in `kernel/syscall/src/audit.rs`.
    const SYSCALL_INVOKED_EVENT_ID: EventId = EventId(5000);

    /// Number of `ProcessSpawned` records seen so far. PASS requires two:
    /// PID 1 `init` and the session it spawns.
    static SPAWNED: AtomicUsize = AtomicUsize::new(0);

    /// Number of audited `SyscallInvoked` records seen so far. PASS requires
    /// three: `init`'s `spawn`, `init`'s `exit`, and the session's gated
    /// `exit` (necessarily last).
    static SYSCALLS: AtomicUsize = AtomicUsize::new(0);

    /// Sink that replays every event through [`SERIAL_SINK`] and reports PASS
    /// to QEMU once two processes have been built and three audited syscalls
    /// have run — proving PID 1 spawned a second, isolated process that ran
    /// its banner and exited.
    struct SpawnSessionExitSink;

    impl Sink for SpawnSessionExitSink {
        fn write_event(&self, event: &Event<'_>) {
            // Replay through the serial sink so the QEMU transcript records
            // the full boot + spawn timeline.
            SerialSink::new().write_event(event);
            if event.id == PROCESS_SPAWNED_EVENT_ID {
                SPAWNED.fetch_add(1, Ordering::AcqRel);
            } else if event.id == SYSCALL_INVOKED_EVENT_ID {
                SYSCALLS.fetch_add(1, Ordering::AcqRel);
            }
            if SPAWNED.load(Ordering::Acquire) >= 2 && SYSCALLS.load(Ordering::Acquire) >= 3 {
                qemu_exit::exit_success();
            }
        }
    }

    static AUDIT_SINK: SpawnSessionExitSink = SpawnSessionExitSink;

    /// Forward to the shared aarch64 panic bridge. A panic before the PASS
    /// finisher parks the CPU, the run times out, and the harness reports
    /// `Outcome::Timeout` — the documented fail-loud behaviour (`AGENTS.md`
    /// §7).
    #[panic_handler]
    fn rustos_spawn_session_qemu_aarch64_panic(info: &PanicInfo<'_>) -> ! {
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
        boot_aarch64::boot(dtb, &SERIAL_SINK, &AUDIT_SINK)
    }
}

// --- Host stub -----------------------------------------------------
#[cfg(not(itest_aarch64))]
fn main() {}

#[cfg(not(itest_aarch64))]
#[allow(dead_code)]
fn _suppress_no_main() {}
