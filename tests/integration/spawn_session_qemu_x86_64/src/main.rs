//! `plans/PI.md` X3b + X4-follow-on QEMU integration test (x86_64 port): boot
//! the production `rustos-kernel` pipeline, spawn PID 1 (`init`) into **ring
//! 3**, and prove `init` launches the embedded login session
//! (`/System/Services/login.app/Run`, `plans/PI.md` P11) through the runtime `spawn`
//! producer (a **hardware-isolated, concurrently-runnable** process under
//! the live scheduler — X3b) **and** then runs the full
//! `wait`→reap→relaunch **supervision cycle** (X4 follow-on) — the cross-port
//! sibling of the aarch64 `spawn_session_qemu_aarch64` (`plans/SPAWN.md` `SP3b`
//! / `plans/PI.md` `P6e-3b-ii`).
//!
//! ## What this test asserts
//!
//! `rustos_kernel::boot` installs the x86_64 PID 1 spawn seam
//! (`init_spawn_x86_64`, through `BootInfo::with_init`), the runtime
//! `ProcessSpawn` producer + embedded-program registry
//! (`spawn_producer_x86_64`, through `BootInfo::with_spawn`), and the COM1
//! console backing (`BootInfo::with_consoles`). After `kernel_core::kernel_main`
//! emits `AuditEvent::BootCompleted` it builds `init`'s ring-3 image through
//! the capability-checked, audited spawn caller (emitting
//! `AuditEvent::ProcessSpawned`, `EventId(4030)`, #1) and admits it as a
//! resumable user kthread, then drains the boot CPU's run queue. PID 1 `init`
//! (`userland/system/init/src/run.rs`):
//!
//! 1. Writes its gated banner to fd 1 (`stream_write` over the COM1 backing).
//! 2. Launches the configured long-running services first — the device
//!    manager `/System/Services/devmgr.app/Run` — with an
//!    (audited) `spawn`, building it a fresh PML4 (emitting `ProcessSpawned`,
//!    #2). `devmgr` reads the discovered hardware tree (unaudited
//!    `hw_tree_read`) and parks in `hw_tree_wait` (unaudited), so it adds
//!    exactly one `ProcessSpawned` and one audited `spawn`, then contributes
//!    no further records.
//! 3. Issues the (audited) `spawn` syscall to launch
//!    `/System/Services/login.app/Run` (P11). The X3b producer resolves it against
//!    the registry and builds login a *fresh, hardware-isolated* PML4
//!    (emitting `ProcessSpawned`, #3), admits it **Ready**, and returns its
//!    PID — the X3b deliverable.
//! 4. Calls `wait` on that child. The cooperative drain steps login: its
//!    `users_db_read` fails closed (no root volume, no database held), it
//!    wires the deny-all authenticator, writes its
//!    `Username: ` prompt, and reads a dead console (the x86_64 boot path
//!    installs no console-read backing, so its `stream_read` on fd 0 fails
//!    closed at `NULL_CONSOLE_READ` and `stdin` clamps to a zero-length
//!    read), records the console error, and `exit`s fail-closed. `init`'s
//!    `wait` then reaps it, returns to ring 3, and **relaunches** the
//!    session with a second `spawn` — the full `wait`→reap→relaunch
//!    supervision cycle (`plans/PI.md` X4 follow-on, the cross-port sibling
//!    of the aarch64 `spawn_session_qemu_aarch64`).
//!
//! ## Why the PASS keys on five spawns and six audited syscalls
//!
//! The second and third `ProcessSpawned` are the boot services `init`
//! launches first (`sysinfod`, `devmgr`); the **fourth** proves the runtime
//! producer authorised the login `spawn`, built an isolated address space,
//! and admitted it (the X3b deliverable). The **fifth** `ProcessSpawned` is
//! the supervision-cycle
//! witness: it can only be emitted if `init`'s `wait` reaped the first login,
//! returned to ring 3, and issued its relaunch `spawn` — so it proves the whole
//! cycle, not just a single concurrent spawn. The six certain audited
//! `SyscallInvoked`
//! records are `init`'s three service `spawn`s, login's `exit`, `init`'s
//! `wait`, and `init`'s relaunch `spawn`
//! (`init`'s `wait` only completes after login exits and is reaped, so login's
//! `exit` necessarily precedes the `wait` record; the audited `call_create`
//! binds — `sysinfod`'s query endpoint and, when a console is attested,
//! login's elevation rendezvous — ride on top, absorbed by the `>=`
//! thresholds). A regression that never
//! reaps+relaunches (`< 5` spawns / `< 6` certain audited syscalls) never
//! reaches the
//! threshold, so the run times out and the harness reports `Outcome::Timeout`
//! — the documented fail-loud behaviour.
//! (`stream_write`/`stream_read` and `devmgr`'s `hw_tree_read`/`hw_tree_wait`
//! are unaudited, and login's refused `users_db_read` audits as a *rejected*
//! record, so neither `devmgr` after its spawn nor login but its `exit` and
//! its endpoint bind
//! contributes a `SyscallInvoked`.)
//!
//! ## How it differs from the production binary
//!
//! It reuses the entire production x86_64 boot pipeline — including the
//! `with_init` seam, the `with_spawn` producer, and the COM1 console — and only
//! replaces the audit sink. Splitting the audit-observer behaviour into a
//! separate bin (instead of a Cargo feature on a production crate) prevents
//! feature unification from leaking the QEMU-exit shortcut into any production
//! build (fail closed; the harness never decides what the
//! kernel does next).

#![cfg_attr(itest_x86_64, no_std)]
#![cfg_attr(itest_x86_64, no_main)]
#![deny(missing_docs)]

// --- Freestanding test bin (`x86_64-unknown-none`) -----------------

#[cfg(itest_x86_64)]
mod kernel {
    use core::panic::PanicInfo;
    use core::sync::atomic::{AtomicUsize, Ordering};

    use rustos_arch_x86_64::qemu_exit;
    use rustos_kernel::kalloc::{Heap, HEAP_BYTES};
    use rustos_kernel::{
        boot, handle_panic_via_kernel_core, FreeListAllocator, SerialSink, SERIAL_SINK,
    };
    use rustos_log::{Event, EventId, Sink};

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

    /// `EventId` the spawn caller emits once a ring-3 image is built. Pinned
    /// by the `event_ids_are_unique` test in `kernel/core/src/audit.rs`. PASS
    /// requires five: PID 1 `init`, the `sysinfod` and `devmgr` services it
    /// launches first, the login it then launches, and the login it
    /// **relaunches** after reaping the first — the fifth is the witness that
    /// the `wait`→reap→relaunch supervision cycle completed (`plans/PI.md` X4
    /// follow-on).
    const PROCESS_SPAWNED_EVENT_ID: EventId = EventId(4030);

    /// `EventId` the syscall dispatcher emits for a successfully dispatched
    /// audited syscall — `init`'s `spawn`/`wait`/`exit` and login's
    /// `exit`. Pinned by the audit-id test in `kernel/syscall/src/audit.rs`.
    const SYSCALL_INVOKED_EVENT_ID: EventId = EventId(5000);

    /// Number of `ProcessSpawned` records seen so far.
    static SPAWNED: AtomicUsize = AtomicUsize::new(0);
    /// Number of audited `SyscallInvoked` records seen so far.
    static SYSCALLS: AtomicUsize = AtomicUsize::new(0);

    /// Sink that replays every event through [`SERIAL_SINK`] (so the QEMU
    /// transcript captures the full boot + spawn timeline) and reports PASS to
    /// QEMU once **five** processes were built and **six** audited syscalls
    /// have run — proving PID 1 launched the boot services, launched the
    /// session into its own isolated ring-3 space, the session executed
    /// there, and `init` reaped it and relaunched a fresh session (the full
    /// supervision cycle, `plans/PI.md` X4 follow-on).
    struct SpawnSessionExitSink;

    impl Sink for SpawnSessionExitSink {
        fn write_event(&self, event: &Event<'_>) {
            SerialSink::new().write_event(event);
            if event.id == PROCESS_SPAWNED_EVENT_ID {
                SPAWNED.fetch_add(1, Ordering::AcqRel);
            } else if event.id == SYSCALL_INVOKED_EVENT_ID {
                SYSCALLS.fetch_add(1, Ordering::AcqRel);
            }
            if SPAWNED.load(Ordering::Acquire) >= 5 && SYSCALLS.load(Ordering::Acquire) >= 6 {
                qemu_exit::exit_success();
            }
        }
    }

    static AUDIT_SINK: SpawnSessionExitSink = SpawnSessionExitSink;

    /// Forward to the shared bridge in `rustos_kernel::x86_64::panic_ctx`. The bridge
    /// logs through `SERIAL_SINK`, not `AUDIT_SINK`, so a panic before PASS
    /// does not trip the QEMU-exit short-circuit — it halts, the run times
    /// out, and the harness reports `Outcome::Timeout` (fail-loud).
    #[panic_handler]
    fn rustos_spawn_session_qemu_x86_64_panic(info: &PanicInfo<'_>) -> ! {
        handle_panic_via_kernel_core(info)
    }

    /// The symbol the arch crate's boot trampoline calls. Forwards to
    /// [`rustos_kernel::boot`] with the production COM1 log sink and the
    /// audit-observer sink.
    #[no_mangle]
    pub extern "C" fn kernel_main(multiboot_info: u64) -> ! {
        boot(multiboot_info, &SERIAL_SINK, &AUDIT_SINK)
    }
}

// --- Host stub -----------------------------------------------------
#[cfg(not(itest_x86_64))]
fn main() {}

#[cfg(not(itest_x86_64))]
#[allow(dead_code)]
fn _suppress_no_main() {}
