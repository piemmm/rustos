//! `plans/PI.md` P6e-3b-ii / P11 QEMU integration test: boot the aarch64
//! (Raspberry Pi 4) `tairix-kernel` pipeline on the `virt` board, spawn
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
//!    view paints the red `1 failed attempt` line, a username one character
//!    past the account format's `MAX_USERNAME_LEN` validation bound (and
//!    nothing more — the last character is the byte that trips the refusal,
//!    so the serial step is fully delivered before login can act on it). The
//!    view refuses the over-long username whole (`LengthOutOfRange`), login
//!    records the console error, and exits fail-closed (audited
//!    `SyscallInvoked` #4 of the supervision chain). `init`'s `wait` then
//!    reaps it and reads its code.
//! 5. `init` relaunches the session — a second login `spawn`, and with it a
//!    fresh `ProcessSpawned`. The second login blocks at its own prompt; the
//!    PASS finisher has already fired by then and the script is exhausted,
//!    so the run ends without typing at it.
//!
//! ## What the PASS keys on
//!
//! The `SupervisionWitness` of `tests/integration/spawn_supervision`, shared
//! with the x86-64 port: login exited, `init` was reaping, and `init` built a
//! replacement image. Each step is recognised by which process acted, never
//! by how many events went past, so the boot-service list can grow without
//! touching this vertical — see that crate for why counting was wrong.
//!
//! A regression that never spawns login, never delivers its input, never
//! reaps it, or never relaunches it leaves the witness short of `Complete`,
//! so the run times out and the harness reports `Outcome::Timeout` — the
//! documented fail-loud behaviour. The runner adds the converse guard: it
//! fails the run if the guest exits before every scripted prompt appeared
//! and every line was sent, so a login that crashes mid-dialogue (e.g. per
//! keystroke) cannot pass on the relaunch alone. The mounted volume's users
//! database serves the credential checks; the scripted wrong password is
//! refused by the real authenticator, never a stub.
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

    use tairix_arch_aarch64::{handle_panic_via_serial, qemu_exit, SerialSink, SERIAL_SINK};
    use tairix_kalloc::{FreeListAllocator, Heap, HEAP_BYTES};
    use tairix_kernel::aarch64::boot as boot_aarch64;
    use tairix_log::{Event, Sink};
    use tairix_test_spawn_supervision::SupervisionWitness;

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

    /// Tracks the launch → run → exit → reap → relaunch cycle by the identity
    /// of the process performing each step.
    static WITNESS: SupervisionWitness = SupervisionWitness::new();

    /// Sink that replays every event through [`SERIAL_SINK`] and reports PASS
    /// to QEMU once [`WITNESS`] has seen the whole supervision cycle —
    /// proving PID 1 launched the session, reaped it when it exited, and
    /// relaunched it (supervision, not spawn-and-forget).
    struct SpawnSessionExitSink;

    impl Sink for SpawnSessionExitSink {
        fn write_event(&self, event: &Event<'_>) {
            // Replay through the serial sink so the QEMU transcript records
            // the full boot + spawn timeline.
            SerialSink::new().write_event(event);
            if WITNESS.observe(event) {
                qemu_exit::exit_success();
            }
        }
    }

    static AUDIT_SINK: SpawnSessionExitSink = SpawnSessionExitSink;

    /// Forward to the shared aarch64 panic bridge. A panic before the PASS
    /// finisher parks the CPU, the run times out, and the harness reports
    /// `Outcome::Timeout` — the documented fail-loud behaviour.
    #[panic_handler]
    fn tairix_spawn_session_qemu_aarch64_panic(info: &PanicInfo<'_>) -> ! {
        handle_panic_via_serial(info)
    }

    /// Boot entry point — the symbol the arch crate's `boot.s` trampoline
    /// calls (via `tairix_arch_aarch64_main`).
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
            // `SyscallInvoked` (`EventId(5000)`) is `Debug`, below the
            // default `Info` filter; this observer's PASS finisher fires
            // on it, so boot with the filter lowered.
            tairix_log::Level::Debug,
            &tairix_kernel::hwtree_store::HW_TREE_SOURCE,
        )
    }
}

// --- Host stub -----------------------------------------------------
#[cfg(not(itest_aarch64))]
fn main() {}
