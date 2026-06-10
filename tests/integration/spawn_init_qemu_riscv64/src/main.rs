//! `plans/PI.md` RV-P3 QEMU integration test: boot the riscv64 (QEMU
//! `virt` / SiFive) `rustos-kernel` pipeline, spawn PID 1 (`init`) into
//! U-mode, and report success to QEMU once `init` traps back with an
//! audited syscall.
//!
//! ## What this test asserts
//!
//! `boot_riscv64::boot` installs the `InitSpawn` seam into the `BootInfo`
//! hand-off; `kernel_core::kernel_main` invokes it after every init phase
//! has succeeded and `AuditEvent::BootCompleted` (`EventId(4004)`) has been
//! emitted. The seam builds the embedded `init` (`Run`) program's U-mode
//! image through the production capability-checked, audited spawn caller
//! (`spawn_image` + `admit_init`, gated on `CAP_PROC_SPAWN`), emitting
//! `AuditEvent::ProcessSpawned` (`EventId(4030)`), and dispatches it.
//! `init` runs in U-mode and writes its startup banner through the
//! `abi-v1` `stream_write` syscall, then issues the `spawn` syscall to
//! launch its session (the first act of its supervise loop); that `ecall`
//! traps back through the S-mode vector to the production dispatch
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
//! of user memory (through the SBI console backing this slice installs), not
//! merely that `init` reached U-mode. (This vertical proves the U-mode
//! transition + banner; that `init` then *supervises* the session is the
//! riscv64 sibling of `spawn_session_qemu_aarch64`.)
//!
//! ## Real firmware device tree
//!
//! QEMU's riscv64 `virt` OpenSBI firmware hands the boot hart a valid
//! device-tree pointer in `a1`, so — unlike the aarch64 `-kernel` path —
//! this vertical forwards the verbatim pointer to the boot pipeline, which
//! discovers the `/memory` window and timer rate from it exactly as it
//! would from real firmware.
//!
//! ## How it differs from a production kernel
//!
//! It reuses the entire production riscv64 boot pipeline — including the
//! `InitSpawn` seam — and only replaces the audit sink. Splitting the
//! audit-observer behaviour into a separate bin (instead of a Cargo feature
//! on a production crate) prevents feature unification from leaking the
//! QEMU-exit shortcut into any production build (`AGENTS.md` §5.4.5 — fail
//! closed; the harness never decides what the kernel does next).

#![cfg_attr(itest_riscv64, no_std)]
#![cfg_attr(itest_riscv64, no_main)]
#![deny(missing_docs)]

// --- Freestanding test bin (`riscv64gc-unknown-none-elf`) ----------

#[cfg(itest_riscv64)]
mod kernel {
    use core::panic::PanicInfo;
    use core::sync::atomic::{AtomicBool, Ordering};

    use rustos_arch_riscv64::{handle_panic_via_serial, qemu_exit, SerialSink, SERIAL_SINK};
    use rustos_bumpalloc::{BumpAllocator, Heap, HEAP_BYTES};
    use rustos_kernel::boot_riscv64;
    use rustos_log::{Event, EventId, Sink};

    /// Static boot heap.
    ///
    /// Placed in the linker's dedicated `.heap` (NOLOAD) section so the
    /// boot trampoline does not zero its bytes (the bump allocator does
    /// not require zeroed backing) and the boot pipeline excludes it from
    /// the usable physical-memory map, exactly as the production riscv64
    /// kernel binary's heap does. `static mut` because the bump allocator
    /// hands out disjoint slices via an atomic cursor; the storage is
    /// otherwise never aliased.
    #[link_section = ".heap"]
    static mut HEAP: Heap = Heap::ZERO;

    /// Global allocator backed by [`HEAP`].
    ///
    /// SAFETY: the page-aligned `HEAP` static outlives the binary and the
    /// allocator is its only consumer.
    #[global_allocator]
    static ALLOCATOR: BumpAllocator =
        unsafe { BumpAllocator::new(core::ptr::addr_of!(HEAP) as *mut u8, HEAP_BYTES) };

    /// `EventId` emitted by `spawn_image` once the PID 1 image is built and
    /// it is about to be dispatched into U-mode. Pinned by the
    /// `event_ids_are_unique` test in `kernel/core/src/audit.rs`.
    const PROCESS_SPAWNED_EVENT_ID: EventId = EventId(4030);

    /// `EventId` emitted by the syscall dispatcher for an audited syscall —
    /// `init`'s first audited syscall is the `spawn` that launches its
    /// session. Pinned by the audit-id test in `kernel/syscall/src/audit.rs`.
    const SYSCALL_INVOKED_EVENT_ID: EventId = EventId(5000);

    /// Set once `ProcessSpawned` has been observed, so a `SyscallInvoked`
    /// only reports PASS *after* PID 1 entered U-mode — proving the order
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

    /// Forward to the shared riscv64 panic bridge. A panic before the PASS
    /// finisher parks the hart, the run times out, and the harness reports
    /// `Outcome::Timeout` — the documented fail-loud behaviour (`AGENTS.md`
    /// §7).
    #[panic_handler]
    fn rustos_spawn_init_qemu_riscv64_panic(info: &PanicInfo<'_>) -> ! {
        handle_panic_via_serial(info)
    }

    /// Boot entry point — the symbol the arch crate's `boot.s` trampoline
    /// calls (via `rustos_arch_riscv64_main`).
    ///
    /// Forwards the SBI hand-off values (`a0` = hartid, `a1` = DTB) to the
    /// production boot pipeline with the audit-observer sink in place.
    #[no_mangle]
    pub extern "C" fn kernel_main(hartid: u64, dtb: u64) -> ! {
        boot_riscv64::boot(hartid, dtb, &SERIAL_SINK, &AUDIT_SINK)
    }
}

// --- Host stub -----------------------------------------------------
#[cfg(not(itest_riscv64))]
fn main() {}

#[cfg(not(itest_riscv64))]
#[allow(dead_code)]
fn _suppress_no_main() {}
