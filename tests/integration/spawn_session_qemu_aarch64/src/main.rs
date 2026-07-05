//! `plans/PI.md` P6e-3b-ii / P11 QEMU integration test: boot the aarch64
//! (Raspberry Pi 4) `rustos-kernel` pipeline on the `virt` board, spawn
//! PID 1 (`init`) into EL0, and prove `init` **supervises** the embedded
//! login session (`/System/Services/login.app/Run`) — launching it, waiting on and
//! reaping it when it exits, and relaunching it — rather than spawning it
//! and forgetting it.
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
//! 1. `spawn` for the device-manager service `/System/Services/devmgr.app/Run`
//!    (audited `SyscallInvoked`, `EventId(5000)`, #1) — the long-running
//!    service `init` launches *first*. The producer
//!    builds it a fresh address space (emitting `ProcessSpawned`, #2);
//!    `devmgr` reads the discovered hardware tree (unaudited `hw_tree_read`)
//!    and parks in `hw_tree_wait` (unaudited), contributing no further
//!    records after its spawn.
//! 2. `spawn` for `/System/Services/login.app/Run` (audited `SyscallInvoked` #2) —
//!    the P11 session. The runtime `ProcessSpawn` producer builds login a
//!    *fresh, hardware-isolated* address space (emitting `ProcessSpawned`,
//!    #3) and admits it **Ready**.
//! 3. `wait` on the children (audited `SyscallInvoked` #3), which parks
//!    `init` back on the scheduler until a child is reapable.
//! 4. The xtask enrolment's ordered `serial` script first answers the
//!    root-unlock passphrase prompt (this vertical boots the shared
//!    encrypted-root whole-disk image: the aarch64 production boot embeds
//!    no program rows, so every service above is read, verified, and
//!    spawned from its on-disk `/System` store bundle — `plans/APPS.md`
//!    deliverable 8 — and the unlock loads the volume's users database
//!    login waits for). Login then draws its full-screen view — the
//!    `Username:` label inside the login box — and **blocks** in
//!    `stream_read` on the kernel-core `BlockingConsoleRead` backing (the
//!    backing owns blocking). The runner holds the scripted dialogue with
//!    it (each line typed only after its anchor appeared past the previous
//!    exchange): `root` at the `Username:` label, a wrong password once
//!    the `Password` label repaints it — which happens only if login read
//!    the username line whole and advanced (the per-keystroke-crash
//!    regression witness) — then, after the authenticator refuses and the
//!    view paints the red `1 failed attempt` line, a 513-byte line — one
//!    byte past login's 512-byte `INPUT_LINE_MAX` validation bound. The
//!    view refuses the over-long line whole (`LengthOutOfRange`), login
//!    records the console error, and exits fail-closed (audited
//!    `SyscallInvoked` #4 of the supervision chain). `init`'s `wait` then
//!    reaps it and reads its code.
//! 5. `init` relaunches the session — a second login `spawn` (an audited
//!    `SyscallInvoked` of the chain) producing a **fifth**
//!    `ProcessSpawned`. The second login blocks at its own prompt;
//!    the PASS finisher has already fired by then and the script is
//!    exhausted, so the run ends without typing at it.
//!
//! ## Why the PASS keys on five spawns and six audited syscalls
//!
//! The second and third `ProcessSpawned` are the boot services `init`
//! launches first (`sysinfod`, `devmgr`). The **fifth** is the supervision
//! witness: `init` only reaches its second login `spawn` *after* its `wait`
//! returned, which only happens once the first login was reaped — so a fifth
//! built image proves the full reap-and-restart cycle, not merely a single
//! concurrent spawn. Login's `exit` is on the critical path only if login
//! actually ran and its blocked `stream_read` received the injected UART RX
//! bytes: its prompt write is gated through its *own* isolated address space, and `init`'s `wait` cannot return until that `exit`
//! recorded the child's code. The chain's certain audited syscalls are
//! `init`'s three service `spawn`s, `init`'s `wait`,
//! login's `exit`, and `init`'s second login `spawn` (login's own audited
//! `users_db_read`, `sysinfod`'s `call_create`, and login's elevation
//! `call_create` ride on top, which the `>=` thresholds absorb; `devmgr`'s
//! `hw_tree_read`/`hw_tree_wait` are unaudited). A regression that never
//! spawns login, never delivers its input, never reaps it, or never
//! relaunches it never reaches the fifth
//! `ProcessSpawned`, so the run times out and the harness reports
//! `Outcome::Timeout` — the documented fail-loud behaviour. The runner adds the converse guard: it fails the run if the guest
//! exits before every scripted prompt appeared and every line was sent, so
//! a login that crashes mid-dialogue (e.g. per keystroke) cannot pass on
//! the relaunch's event counts alone. The mounted volume's users database
//! serves the credential checks; the scripted wrong password is refused by
//! the real authenticator, never a stub.
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
//! production build (fail closed; the harness never
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
    use rustos_kalloc::{FreeListAllocator, Heap, HEAP_BYTES};
    use rustos_kernel::aarch64::boot as boot_aarch64;
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
    static ALLOCATOR: FreeListAllocator =
        unsafe { FreeListAllocator::new(core::ptr::addr_of!(HEAP) as *mut u8, HEAP_BYTES) };

    /// `EventId` emitted by the spawn caller once an EL0 image is built.
    /// Pinned by the `event_ids_are_unique` test in `kernel/core/src/audit.rs`.
    const PROCESS_SPAWNED_EVENT_ID: EventId = EventId(4030);

    /// `EventId` emitted by the syscall dispatcher for an audited syscall —
    /// `init`'s `spawn` / `wait` and the session's `exit`. Pinned by the
    /// audit-id test in `kernel/syscall/src/audit.rs`.
    const SYSCALL_INVOKED_EVENT_ID: EventId = EventId(5000);

    /// Number of `ProcessSpawned` records seen so far. PASS requires five:
    /// PID 1 `init`, the `sysinfod` and `devmgr` services it launches first,
    /// and the **two** login instances — the second login launch can only
    /// happen after `init` reaped the first, so a fifth `ProcessSpawned` is
    /// the witness that supervision (reap + restart) ran.
    static SPAWNED: AtomicUsize = AtomicUsize::new(0);

    /// Number of audited `SyscallInvoked` records seen so far. PASS requires
    /// six, the certain prefix of `init`'s supervise loop up to the
    /// relaunch: `init`'s three service `spawn`s (`sysinfod`, `devmgr`,
    /// login), `init`'s `wait` (which parks it), the first login's
    /// fail-closed `exit`, and `init`'s second login `spawn` (the relaunch).
    /// The audited `call_create` binds (`sysinfod`'s query endpoint, login's
    /// elevation rendezvous) ride on top, absorbed by the `>=` threshold;
    /// `devmgr`'s `hw_tree_read`/`hw_tree_wait` are unaudited.
    static SYSCALLS: AtomicUsize = AtomicUsize::new(0);

    /// Sink that replays every event through [`SERIAL_SINK`] and reports PASS
    /// to QEMU once five processes have been built and six audited syscalls
    /// have run — proving PID 1 launched the boot services and the session,
    /// waited on and reaped the session when it exited, and relaunched it
    /// (supervision, not spawn-and-forget).
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
            if SPAWNED.load(Ordering::Acquire) >= 5 && SYSCALLS.load(Ordering::Acquire) >= 6 {
                qemu_exit::exit_success();
            }
        }
    }

    static AUDIT_SINK: SpawnSessionExitSink = SpawnSessionExitSink;

    /// Forward to the shared aarch64 panic bridge. A panic before the PASS
    /// finisher parks the CPU, the run times out, and the harness reports
    /// `Outcome::Timeout` — the documented fail-loud behaviour.
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
        boot_aarch64::boot(
            dtb,
            &SERIAL_SINK,
            &AUDIT_SINK,
            &rustos_kernel::hwtree_store::HW_TREE_SOURCE,
        )
    }
}

// --- Host stub -----------------------------------------------------
#[cfg(not(itest_aarch64))]
fn main() {}

#[cfg(not(itest_aarch64))]
#[allow(dead_code)]
fn _suppress_no_main() {}
