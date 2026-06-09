//! `plans/PI.md` X3b QEMU integration test (x86_64 port): boot the production
//! `rustos-kernel` pipeline, spawn PID 1 (`init`) into **ring 3**, and prove
//! `init` launches the embedded `Shell` session through the runtime `spawn`
//! producer so a **second, hardware-isolated, concurrently-runnable** process
//! runs under the live scheduler — the cross-port sibling of the aarch64
//! `spawn_session_qemu_aarch64` (`plans/SPAWN.md` `SP3b` / `plans/PI.md`
//! `P6e-3b-ii`).
//!
//! ## What this test asserts
//!
//! `rustos_kernel::boot` installs the x86_64 PID 1 spawn seam
//! (`init_spawn_x86_64`, through `BootInfo::with_init`), the runtime
//! `ProcessSpawn` producer + embedded-program registry
//! (`spawn_producer_x86_64`, through `BootInfo::with_spawn`), and the COM1
//! console backing (`BootInfo::with_console`). After `kernel_core::kernel_main`
//! emits `AuditEvent::BootCompleted` it builds `init`'s ring-3 image through
//! the capability-checked, audited spawn caller (emitting
//! `AuditEvent::ProcessSpawned`, `EventId(4030)`, #1) and admits it as a
//! resumable user kthread, then drains the boot CPU's run queue. PID 1 `init`
//! (`userland/system/init/src/run.rs`):
//!
//! 1. Writes its gated banner to fd 1 (`stream_write` over the COM1 backing).
//! 2. Issues the (audited) `spawn` syscall to launch `/Apps/Shell.app/Run`.
//!    The X3b producer resolves it against the registry and builds the session
//!    a *fresh, hardware-isolated* PML4 (emitting `ProcessSpawned`, #2), admits
//!    it **Ready**, and returns its PID — the X3b deliverable.
//! 3. Calls `wait` on that child. The cooperative drain then steps the session,
//!    which writes its prompt, reads end-of-input (its `stream_read` on fd 0 is
//!    denied — the session holds only `CAP_CONSOLE_WRITE`, so `stdin` clamps to
//!    a zero-length read), and `exit`s. (`init`'s subsequent `wait`→reap→relaunch
//!    supervision cycle is the x86_64 `wait` validation — `plans/PI.md` X4 — and
//!    is *not* asserted here; this vertical proves only the X3b concurrent
//!    spawn.)
//!
//! ## Why the PASS keys on two spawns and two audited syscalls
//!
//! The **second** `ProcessSpawned` is the X3b witness: it proves the runtime
//! producer authorised the `spawn`, built a second isolated address space, and
//! admitted it. The two audited `SyscallInvoked` records are `init`'s `spawn`
//! (the dispatcher emits it once the handler completes) and the **session's**
//! `exit`: `init` cannot reach any later audited syscall (its `wait` only
//! completes *after* the session exits and is reaped), so the second audited
//! `SyscallInvoked` can only be the session's `exit`, proving the session
//! actually *ran* in its own ring-3 space, not merely that its image was built.
//! A regression that never builds the session (`< 2` spawns) or never runs it
//! (`< 2` audited syscalls) never reaches the threshold, so the run times out
//! and the harness reports `Outcome::Timeout` — the documented fail-loud
//! behaviour (`AGENTS.md` §7). (`stream_write`/`stream_read` are unaudited
//! high-frequency I/O, so the session contributes no audited record but its
//! `exit`.)
//!
//! ## How it differs from the production binary
//!
//! It reuses the entire production x86_64 boot pipeline — including the
//! `with_init` seam, the `with_spawn` producer, and the COM1 console — and only
//! replaces the audit sink. Splitting the audit-observer behaviour into a
//! separate bin (instead of a Cargo feature on a production crate) prevents
//! feature unification from leaking the QEMU-exit shortcut into any production
//! build (`AGENTS.md` §5.4.5 — fail closed; the harness never decides what the
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
    use rustos_kernel::bumpalloc::{Heap, HEAP_BYTES};
    use rustos_kernel::{
        boot, handle_panic_via_kernel_core, BumpAllocator, SerialSink, SERIAL_SINK,
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
    static ALLOCATOR: BumpAllocator =
        unsafe { BumpAllocator::new(core::ptr::addr_of!(HEAP) as *mut u8, HEAP_BYTES) };

    /// `EventId` the spawn caller emits once a ring-3 image is built. Pinned
    /// by the `event_ids_are_unique` test in `kernel/core/src/audit.rs`. PASS
    /// requires two: PID 1 `init` and the session it launches — the second is
    /// the witness that the runtime `spawn` producer built a concurrent
    /// process (`plans/PI.md` X3b).
    const PROCESS_SPAWNED_EVENT_ID: EventId = EventId(4030);

    /// `EventId` the syscall dispatcher emits for a successfully dispatched
    /// audited syscall — `init`'s `spawn`/`wait`/`exit` and the session's
    /// `exit`. Pinned by the audit-id test in `kernel/syscall/src/audit.rs`.
    const SYSCALL_INVOKED_EVENT_ID: EventId = EventId(5000);

    /// Number of `ProcessSpawned` records seen so far.
    static SPAWNED: AtomicUsize = AtomicUsize::new(0);
    /// Number of audited `SyscallInvoked` records seen so far.
    static SYSCALLS: AtomicUsize = AtomicUsize::new(0);

    /// Sink that replays every event through [`SERIAL_SINK`] (so the QEMU
    /// transcript captures the full boot + spawn timeline) and reports PASS to
    /// QEMU once **two** processes were built and **two** audited syscalls
    /// have run — proving PID 1 launched the session into its own isolated
    /// ring-3 space and the session executed there (`init`'s `spawn` is the
    /// first audited syscall; the session's `exit` is necessarily the second).
    struct SpawnSessionExitSink;

    impl Sink for SpawnSessionExitSink {
        fn write_event(&self, event: &Event<'_>) {
            SerialSink::new().write_event(event);
            if event.id == PROCESS_SPAWNED_EVENT_ID {
                SPAWNED.fetch_add(1, Ordering::AcqRel);
            } else if event.id == SYSCALL_INVOKED_EVENT_ID {
                SYSCALLS.fetch_add(1, Ordering::AcqRel);
            }
            if SPAWNED.load(Ordering::Acquire) >= 2 && SYSCALLS.load(Ordering::Acquire) >= 2 {
                qemu_exit::exit_success();
            }
        }
    }

    static AUDIT_SINK: SpawnSessionExitSink = SpawnSessionExitSink;

    /// Forward to the shared bridge in `rustos_kernel::panic_ctx`. The bridge
    /// logs through `SERIAL_SINK`, not `AUDIT_SINK`, so a panic before PASS
    /// does not trip the QEMU-exit short-circuit — it halts, the run times
    /// out, and the harness reports `Outcome::Timeout` (fail-loud, §7).
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
