//! `plans/PI.md` X3a QEMU integration test (x86_64 port): boot the production
//! `rustos-kernel` pipeline, spawn PID 1 (`init`) into **ring 3**, and prove
//! it runs there — the cross-port sibling of the aarch64
//! `spawn_init_qemu_aarch64` (P6c-3).
//!
//! ## What this test asserts
//!
//! `rustos_kernel::boot` installs the x86_64 PID 1 spawn seam
//! (`init_spawn_x86_64`, through `BootInfo::with_init`) and the COM1 console
//! backing (`BootInfo::with_console`). After `kernel_core::kernel_main` emits
//! `AuditEvent::BootCompleted` it builds `init`'s ring-3 image through the
//! capability-checked, audited spawn caller (emitting `AuditEvent::ProcessSpawned`,
//! `EventId(4030)`) and admits it as a resumable user kthread, then drains the
//! boot CPU's run queue. PID 1 `init` (`userland/system/init/src/run.rs`):
//!
//! 1. Writes its banner to fd 1 (`stream_write` over the COM1 backing). The
//!    write is *gated* — `init` parks fail-closed on a short write — so any
//!    later progress proves the banner fully landed (`AGENTS.md` §20 / §2.9).
//! 2. Issues the `spawn` syscall to launch its session. The x86_64 runtime
//!    `ProcessSpawn` producer is not wired yet (`plans/PI.md` X3b), so the
//!    fail-closed `NULL_PROCESS_SPAWN` rejects it (`NotImplemented`), `init`
//!    treats the negative result as a failed launch, and the runtime routes
//!    its return through the `exit` syscall — an audited `SyscallInvoked`
//!    (`EventId(5000)`).
//!
//! ## Why the PASS keys on a spawn + an audited syscall
//!
//! The `ProcessSpawned` record (`EventId(4030)`) proves the kernel built PID
//! 1's ring-3 image; the audited `SyscallInvoked` (`EventId(5000)`, PID 1's
//! `exit`) proves PID 1 actually executed in ring 3, made its gated banner
//! write land, and trapped back into the kernel through the `syscall` entry
//! path landing on PID 1's own kernel stack (the X1 durable user-`%rsp` save +
//! per-task `kernel_rsp0`). `init`'s `exit` is reached only *after* the gated
//! banner landed and the (rejected) `spawn` returned, so it is a sufficient
//! witness. A regression that never reaches ring 3 (a bad image, a fault on
//! entry, an unhandled first `syscall`) never emits the audited syscall, so
//! the run times out and the harness reports `Outcome::Timeout` — the
//! documented fail-loud behaviour (`AGENTS.md` §7).
//!
//! ## How it differs from the production binary
//!
//! It reuses the entire production x86_64 boot pipeline — including the
//! `with_init` seam and the COM1 console — and only replaces the audit sink.
//! Splitting the audit-observer behaviour into a separate bin (instead of a
//! Cargo feature on a production crate) prevents feature unification from
//! leaking the QEMU-exit shortcut into any production build (`AGENTS.md`
//! §5.4.5 — fail closed; the harness never decides what the kernel does next).

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
    /// by the `event_ids_are_unique` test in `kernel/core/src/audit.rs`.
    const PROCESS_SPAWNED_EVENT_ID: EventId = EventId(4030);

    /// `EventId` the syscall dispatcher emits for a successfully dispatched
    /// audited syscall — here PID 1 `init`'s `exit` (its `spawn` is *rejected*
    /// while the X3b producer is unwired, which emits a different id). Pinned
    /// by the audit-id test in `kernel/syscall/src/audit.rs`.
    const SYSCALL_INVOKED_EVENT_ID: EventId = EventId(5000);

    /// Number of `ProcessSpawned` records seen so far.
    static SPAWNED: AtomicUsize = AtomicUsize::new(0);
    /// Number of audited `SyscallInvoked` records seen so far.
    static SYSCALLS: AtomicUsize = AtomicUsize::new(0);

    /// Sink that replays every event through [`SERIAL_SINK`] (so the QEMU
    /// transcript captures the full boot + spawn timeline) and reports PASS to
    /// QEMU once PID 1's image was built **and** PID 1 issued at least one
    /// audited syscall — proving it reached and executed in ring 3.
    ///
    /// It deliberately does **not** act on `BootCompleted`: returning lets
    /// `kernel_main` proceed to spawn PID 1, which is the whole point of the
    /// test (unlike the boot-only `kernel_arch_boot` vertical).
    struct SpawnInitExitSink;

    impl Sink for SpawnInitExitSink {
        fn write_event(&self, event: &Event<'_>) {
            SerialSink::new().write_event(event);
            if event.id == PROCESS_SPAWNED_EVENT_ID {
                SPAWNED.fetch_add(1, Ordering::AcqRel);
            } else if event.id == SYSCALL_INVOKED_EVENT_ID {
                SYSCALLS.fetch_add(1, Ordering::AcqRel);
            }
            if SPAWNED.load(Ordering::Acquire) >= 1 && SYSCALLS.load(Ordering::Acquire) >= 1 {
                qemu_exit::exit_success();
            }
        }
    }

    static AUDIT_SINK: SpawnInitExitSink = SpawnInitExitSink;

    /// Forward to the shared bridge in `rustos_kernel::panic_ctx`. The bridge
    /// logs through `SERIAL_SINK`, not `AUDIT_SINK`, so a panic before PASS
    /// does not trip the QEMU-exit short-circuit — it halts, the run times
    /// out, and the harness reports `Outcome::Timeout` (fail-loud, §7).
    #[panic_handler]
    fn rustos_spawn_init_qemu_x86_64_panic(info: &PanicInfo<'_>) -> ! {
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
