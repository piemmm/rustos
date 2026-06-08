//! `plans/PI.md` P6c-3 QEMU integration test: boot the aarch64 (Raspberry
//! Pi 4) `rustos-kernel` pipeline on the `virt` board, spawn PID 1 (`init`)
//! into EL0, and report success to QEMU once `init` traps back.
//!
//! ## What this test asserts
//!
//! `boot_aarch64::boot` installs the `InitSpawn` seam into the `BootInfo`
//! hand-off; `kernel_core::kernel_main` invokes it after every init phase
//! has succeeded and `AuditEvent::BootCompleted` (`EventId(4004)`) has been
//! emitted. The seam builds the embedded `init` (`Run`) program's EL0 image
//! through the production capability-checked, audited spawn caller
//! (`spawn_and_enter`, gated on `CAP_PROC_SPAWN`), emitting
//! `AuditEvent::ProcessSpawned` (`EventId(4030)`), and `eret`s into it.
//! `init` runs in EL0 and writes its startup banner through the `abi-v1`
//! `stream_write` syscall, then issues the `spawn` syscall to launch its
//! session (the first act of its supervise loop, `plans/PI.md` P6e-3b-ii);
//! that `svc` traps back through the EL1 vector to the production dispatch
//! callback, which emits the audited `AuditEvent::SyscallInvoked`
//! (`EventId(5000)`).
//!
//! ## The banner write is on the critical path
//!
//! `init` only issues its `spawn` (and so only reaches the first audited
//! syscall) once its `stream_write` reports the whole banner accepted; on a
//! short write it parks fail-closed (`userland/system/init`). `stream_write`
//! is itself *not* audited (`lib/abi` `audit: false`), so it emits no
//! `SyscallInvoked` of its own. The PASS finisher therefore fires only after
//! the banner write actually landed — proving PID 1's address space resolved
//! through the kernel-wide registry so the kernel could copy the banner out
//! of user memory (`plans/PI.md` P6c-3 follow-up), not merely that `init`
//! reached EL0. (This vertical proves the EL0 transition + banner; that
//! `init` then *supervises* the session is `spawn_session_qemu_aarch64`.)
//!
//! This binary drives the real aarch64 boot pipeline end to end on the
//! `virt` board and replaces only the audit sink: observing
//! `ProcessSpawned` then `SyscallInvoked` proves PID 1 reached user mode,
//! wrote its banner, and trapped back, and flips the ARM semihosting PASS
//! finisher. A regression that never spawns `init`, whose banner write fails
//! closed (so `init` parks without exiting), or that never reaches the
//! syscall never reaches the finisher, so the run times out and the harness
//! reports `Outcome::Timeout` — the documented fail-loud behaviour
//! (`AGENTS.md` §7).
//!
//! ## Embedded `virt` device tree
//!
//! QEMU's `-kernel <ELF>` aarch64 path passes no DTB pointer (`x0 = 0`), so
//! the canonical `virt` device tree is dumped and embedded at build time
//! (`build.rs`) and its address handed to the boot pipeline, which
//! discovers the console / GIC / `/memory` / timer / PSCI from it exactly
//! as it would from real firmware.
//!
//! ## How it differs from a production kernel
//!
//! It reuses the entire production aarch64 boot pipeline — including the
//! `InitSpawn` seam — and only replaces the audit sink. Splitting the
//! audit-observer behaviour into a separate bin (instead of a Cargo feature
//! on a production crate) prevents feature unification from leaking the
//! QEMU-exit shortcut into any production build (`AGENTS.md` §5.4.5 — fail
//! closed; the harness never decides what the kernel does next).

#![cfg_attr(itest_aarch64, no_std)]
#![cfg_attr(itest_aarch64, no_main)]
#![deny(missing_docs)]

// --- Freestanding test bin (`aarch64-unknown-none`) ----------------

#[cfg(itest_aarch64)]
mod kernel {
    use core::panic::PanicInfo;
    use core::sync::atomic::{AtomicBool, Ordering};

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
    /// SAFETY: the page-aligned `HEAP` static outlives the binary and the
    /// allocator is its only consumer.
    #[global_allocator]
    static ALLOCATOR: BumpAllocator =
        unsafe { BumpAllocator::new(core::ptr::addr_of!(HEAP) as *mut u8, HEAP_BYTES) };

    /// `EventId` emitted by `spawn_and_enter` once the PID 1 image is built
    /// and it is about to `eret` into EL0. Pinned by the
    /// `event_ids_are_unique` test in `kernel/core/src/audit.rs`.
    const PROCESS_SPAWNED_EVENT_ID: EventId = EventId(4030);

    /// `EventId` emitted by the syscall dispatcher for an audited syscall —
    /// `init`'s first audited syscall is the `spawn` that launches its
    /// session. Pinned by the audit-id test in `kernel/syscall/src/audit.rs`.
    const SYSCALL_INVOKED_EVENT_ID: EventId = EventId(5000);

    /// Set once `ProcessSpawned` has been observed, so a `SyscallInvoked`
    /// only reports PASS *after* PID 1 entered EL0 — proving the order
    /// (spawn → user-mode syscall), not merely that some syscall ran.
    static INIT_SPAWNED: AtomicBool = AtomicBool::new(false);

    /// Sink that replays every event through [`SERIAL_SINK`] and reports
    /// PASS to QEMU once PID 1 has both spawned and trapped back via an
    /// audited syscall.
    struct SpawnInitExitSink;

    impl Sink for SpawnInitExitSink {
        fn write_event(&self, event: &Event<'_>) {
            // Replay through the serial sink so the QEMU transcript records
            // the full boot + spawn timeline.
            SerialSink::new().write_event(event);
            if event.id == PROCESS_SPAWNED_EVENT_ID {
                INIT_SPAWNED.store(true, Ordering::Release);
            } else if event.id == SYSCALL_INVOKED_EVENT_ID && INIT_SPAWNED.load(Ordering::Acquire) {
                qemu_exit::exit_success();
            }
        }
    }

    static AUDIT_SINK: SpawnInitExitSink = SpawnInitExitSink;

    /// Forward to the shared aarch64 panic bridge. A panic before the PASS
    /// finisher parks the CPU, the run times out, and the harness reports
    /// `Outcome::Timeout` — the documented fail-loud behaviour (`AGENTS.md`
    /// §7).
    #[panic_handler]
    fn rustos_spawn_init_qemu_aarch64_panic(info: &PanicInfo<'_>) -> ! {
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
