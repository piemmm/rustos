//! `plans/PI.md` P6e-3b-ii QEMU integration test: boot the aarch64 (Raspberry
//! Pi 4) `rustos-kernel` pipeline on the `virt` board, spawn PID 1 (`init`)
//! into EL0, and prove `init` **supervises** the embedded `Shell` session —
//! launching it, waiting on and reaping it when it exits, and relaunching it —
//! rather than spawning it and forgetting it (`AGENTS.md` §20).
//!
//! ## What this test asserts
//!
//! `boot_aarch64::boot` installs the `InitSpawn` seam, the runtime
//! `ProcessSpawn` producer + embedded-program registry, and (through
//! `kernel_core`'s `run_phases`) the `KernelProcessWait` producer into the
//! `BootInfo` hand-off. After `kernel_core::kernel_main` emits
//! `AuditEvent::BootCompleted` it builds PID 1 `init` through the
//! capability-checked, audited spawn caller (emitting
//! `AuditEvent::ProcessSpawned`, `EventId(4030)`, #1) and `eret`s into it.
//! `init` writes its banner, then runs its supervise loop
//! (`userland/system/init/src/run.rs`):
//!
//! 1. `spawn` for `/Apps/Shell.app/Run` (audited `SyscallInvoked`,
//!    `EventId(5000)`, #1). The runtime `ProcessSpawn` producer builds the
//!    session a *fresh, hardware-isolated* address space (emitting
//!    `ProcessSpawned`, #2) and admits it **Ready**.
//! 2. `wait` on that child (audited `SyscallInvoked` #2), which parks `init`
//!    back on the scheduler until the child is reapable.
//! 3. The cooperative drain loop steps the session; it writes its prompt,
//!    reads end-of-input (no `-M virt` serial RX), and `exit`s (audited
//!    `SyscallInvoked` #3). `init`'s `wait` then reaps it and reads its code.
//! 4. `init` relaunches the session — a second `spawn` (audited
//!    `SyscallInvoked` #4) producing a **third** `ProcessSpawned` (#3).
//!
//! ## Why the PASS keys on three spawns and four audited syscalls
//!
//! The **third** `ProcessSpawned` is the supervision witness: `init` only
//! reaches its second `spawn` *after* its `wait` returned, which only happens
//! once the first session was reaped — so a third built image proves the full
//! reap-and-restart cycle, not merely a single concurrent spawn. The first
//! session's `exit` (the third audited syscall) is on the critical path only
//! if the session actually ran: its prompt write is gated through its *own*
//! isolated address space (`AGENTS.md` §4), and `init`'s `wait` cannot return
//! until that `exit` recorded the child's code. The four audited syscalls are
//! `init`'s first `spawn`, `init`'s `wait`, the session's `exit`, and `init`'s
//! second `spawn`. A regression that never spawns the session, never reaps it,
//! or never relaunches it never reaches the third `ProcessSpawned`, so the run
//! times out and the harness reports `Outcome::Timeout` — the documented
//! fail-loud behaviour (`AGENTS.md` §7).
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
    /// `init`'s `spawn` / `wait` and the session's `exit`. Pinned by the
    /// audit-id test in `kernel/syscall/src/audit.rs`.
    const SYSCALL_INVOKED_EVENT_ID: EventId = EventId(5000);

    /// Number of `ProcessSpawned` records seen so far. PASS requires three:
    /// PID 1 `init` and the **two** session instances it launches — the
    /// second launch can only happen after `init` reaped the first, so a third
    /// `ProcessSpawned` is the witness that supervision (reap + restart) ran.
    static SPAWNED: AtomicUsize = AtomicUsize::new(0);

    /// Number of audited `SyscallInvoked` records seen so far. PASS requires
    /// four, the prefix of `init`'s supervise loop up to the relaunch:
    /// `init`'s first `spawn`, `init`'s `wait` (which parks it), the first
    /// session's gated `exit`, and `init`'s second `spawn` (the relaunch).
    static SYSCALLS: AtomicUsize = AtomicUsize::new(0);

    /// Sink that replays every event through [`SERIAL_SINK`] and reports PASS
    /// to QEMU once three processes have been built and four audited syscalls
    /// have run — proving PID 1 launched the session, waited on and reaped it
    /// when it exited, and relaunched it (supervision, not spawn-and-forget).
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
            if SPAWNED.load(Ordering::Acquire) >= 3 && SYSCALLS.load(Ordering::Acquire) >= 4 {
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
