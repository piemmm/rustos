//! `plans/PI.md` X3b + X4-follow-on QEMU integration test (x86_64 port): boot
//! the production `tairix-kernel` pipeline, spawn PID 1 (`init`) into **ring
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
//! `tairix_kernel::boot` installs the x86_64 PID 1 spawn seam
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
//!    wires the deny-all authenticator, and draws its full-screen view —
//!    the `Username:` label — then **blocks** in `stream_read` on the
//!    kernel-core `BlockingConsoleRead` backing over the interrupt-fed,
//!    unlock-gated COM1 receive queue. Because this boot binds no root
//!    disk, the in-kernel unlock seam opens the console-0 gate immediately
//!    (`root_unlock::spawn_if_present`), so login owns console 0 and its
//!    read is a live, poll-backed COM1 read — never a fail-closed
//!    `NULL_CONSOLE_READ`. The runner's scripted serial dialogue then types
//!    one character past the account format's `MAX_USERNAME_LEN` validation
//!    bound at the `Username:` field; the view refuses the over-long line
//!    whole (`LengthOutOfRange`), login records the console error, and
//!    `exit`s fail-closed. `init`'s `wait` then reaps it, returns to ring 3,
//!    and **relaunches** the session with a second `spawn` — the full
//!    `wait`→reap→relaunch supervision cycle (`plans/PI.md` X4 follow-on,
//!    the cross-port sibling of the aarch64 `spawn_session_qemu_aarch64`,
//!    which drives login to the same over-long-username exit).
//!
//! ## What the PASS keys on
//!
//! The `SupervisionWitness` of `tests/integration/spawn_supervision`, shared
//! with the aarch64 port: login exited, `init` was reaping, and `init` built a
//! replacement image — which it can only do once its `wait` reaped the first
//! login and returned to ring 3. Each step is recognised by which process
//! acted, never by how many events went past, so the boot-service list can
//! grow without touching this vertical — see that crate for why counting was
//! wrong.
//!
//! A regression that never reaps and relaunches leaves the witness short of
//! `Complete`, so the run times out and the harness reports
//! `Outcome::Timeout` — the documented fail-loud behaviour.
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

    use tairix_arch_x86_64::qemu_exit;
    use tairix_kernel::kalloc::{Heap, HEAP_BYTES};
    use tairix_kernel::{
        boot, handle_panic_via_kernel_core, FreeListAllocator, SerialSink, SERIAL_SINK,
    };
    use tairix_log::{Event, Sink};
    use tairix_test_spawn_supervision::SupervisionWitness;

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

    /// Tracks the launch → run → exit → reap → relaunch cycle by the identity
    /// of the process performing each step (`plans/PI.md` X4 follow-on).
    /// Shared with the aarch64 port so the two cannot drift.
    static WITNESS: SupervisionWitness = SupervisionWitness::new();

    /// Sink that replays every event through [`SERIAL_SINK`] (so the QEMU
    /// transcript captures the full boot + spawn timeline) and reports PASS to
    /// QEMU once [`WITNESS`] has seen the whole supervision cycle — proving
    /// PID 1 launched the session into its own isolated ring-3 space, the
    /// session executed there, and `init` reaped it and relaunched a fresh
    /// session.
    struct SpawnSessionExitSink;

    impl Sink for SpawnSessionExitSink {
        fn write_event(&self, event: &Event<'_>) {
            SerialSink::new().write_event(event);
            if WITNESS.observe(event) {
                qemu_exit::exit_success();
            }
        }
    }

    static AUDIT_SINK: SpawnSessionExitSink = SpawnSessionExitSink;

    /// Forward to the shared bridge in `tairix_kernel::x86_64::panic_ctx`. The bridge
    /// logs through `SERIAL_SINK`, not `AUDIT_SINK`, so a panic before PASS
    /// does not trip the QEMU-exit short-circuit — it halts, the run times
    /// out, and the harness reports `Outcome::Timeout` (fail-loud).
    #[panic_handler]
    fn tairix_spawn_session_qemu_x86_64_panic(info: &PanicInfo<'_>) -> ! {
        handle_panic_via_kernel_core(info)
    }

    /// The symbol the arch crate's boot trampoline calls. Forwards to
    /// [`tairix_kernel::boot`] with the production COM1 log sink and the
    /// audit-observer sink.
    #[no_mangle]
    pub extern "C" fn kernel_main(multiboot_info: u64) -> ! {
        // `SyscallInvoked` (`EventId(5000)`) is `Debug`, below the default
        // `Info` filter; this observer's PASS finisher fires on it, so
        // boot with the filter lowered.
        boot(
            multiboot_info,
            &SERIAL_SINK,
            &AUDIT_SINK,
            tairix_log::Level::Debug,
        )
    }
}

// --- Host stub -----------------------------------------------------
#[cfg(not(itest_x86_64))]
fn main() {}
