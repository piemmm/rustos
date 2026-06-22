//! Design D P-3 QEMU integration test (`.junie/next-pi-prompt.md`): boot the
//! production aarch64 (Raspberry Pi 4) `rustos-kernel` pipeline on the `virt`
//! board, have PID 1 (`init`) launch the **perpetual device-manager service**
//! (`/System/Services/devmgr`, `AGENTS.md` §18.3), and prove the service's
//! reactive observe loop end to end: it reads the discovered hardware tree
//! (`hw_tree_read`), **truly parks** off the run queue in `hw_tree_wait`
//! (`AGENTS.md` §2.1 / §17.1 — no busy poll), and, on a **real generation
//! bump**, wakes, re-reads, and re-parks — with no starvation of a
//! single-CPU system.
//!
//! ## What this test asserts — and how it differs from its siblings
//!
//! * `spawn_session_qemu_aarch64` proves PID 1 launches *and supervises* the
//!   login session (and, now, that it launches `devmgr` first). It does not
//!   exercise `devmgr`'s reactive wake.
//! * This vertical adds the `devmgr` end-to-end reactive proof: park →
//!   generation bump → wake → re-read → re-park.
//!
//! It reuses `boot_aarch64::boot` **verbatim** — the same production pipeline,
//! `InitSpawn` seam, runtime `ProcessSpawn` producer + embedded-program
//! registry (which carries `devmgr` in `spawn_layout::SPAWN_PROGRAMS`), and
//! `HwTreeStore` source — and only swaps the audit sink. After
//! `kernel_core::kernel_main` emits `BootCompleted`, PID 1 `init` launches
//! `devmgr` first (`AGENTS.md` §18.3) and then a login session per console;
//! `devmgr` reads the seeded hardware tree (logging it to fd 2) and blocks in
//! `hw_tree_wait(generation, u64::MAX)`, which **registers it on the kernel's
//! `HW_TREE_WAITQ` and parks it** (Design D P-2).
//!
//! ## How the reactive cycle is driven and witnessed
//!
//! `hw_tree_read` / `hw_tree_wait` are unaudited (`lib/abi/src/syscalls.rs`,
//! high-volume reactive consumer), so the sink cannot observe `devmgr`'s read
//! or wait through audit events. Instead it observes the kernel's own
//! `HW_TREE_WAITQ` directly — a `devmgr` parked in `hw_tree_wait` is the one
//! and only waiter it can hold, so a non-empty wait-queue is the unambiguous
//! "devmgr has truly parked" witness. The sink runs a two-phase state machine
//! on every audit event (the events come from `init`'s spawns and the scripted
//! login dialogue below — enough to interleave with the cooperative drain):
//!
//! 1. **Armed → Bumped:** the first event at which `HW_TREE_WAITQ` is
//!    non-empty proves `devmgr` parked. The sink then appends a node to the
//!    authoritative `HW_TREE` store — a **real** mutation that bumps the
//!    store generation and calls `hw_tree_wake` (`AGENTS.md` §18.4), exactly a
//!    hardware hotplug. This unparks `devmgr`.
//! 2. **Bumped → PASS:** at the next event with `HW_TREE_WAITQ` non-empty,
//!    `devmgr` has been scheduled by the cooperative drain (login blocks on
//!    `stream_read` between events, so the drain runs the unparked `devmgr`
//!    first): its `hw_tree_wait` returned on the generation change, it
//!    re-read the tree (logging the new generation to fd 2), and it re-parked
//!    in a fresh `hw_tree_wait` — re-registering on the wait-queue. The sink
//!    reports PASS through the ARM semihosting finisher.
//!
//! A regression where `devmgr` never spawns, never reads, never parks, or
//! never wakes on the bump never reaches the second phase, so the run times
//! out and the harness reports `Outcome::Timeout` — the documented fail-loud
//! behaviour (`AGENTS.md` §7).
//!
//! ## Embedded `virt` device tree
//!
//! QEMU's `-kernel <ELF>` aarch64 path passes no DTB pointer (`x0 = 0`), so
//! the canonical `virt` device tree is dumped and embedded at build time
//! (`build.rs`) and its address handed to the boot pipeline, which discovers
//! the console / GIC / `/memory` / timer / PSCI from it exactly as it would
//! from real firmware.

#![cfg_attr(itest_aarch64, no_std)]
#![cfg_attr(itest_aarch64, no_main)]
#![deny(missing_docs)]

// --- Freestanding test bin (`aarch64-unknown-none`) ----------------

#[cfg(itest_aarch64)]
mod kernel {
    use core::panic::PanicInfo;
    use core::sync::atomic::{AtomicU8, Ordering};

    use rustos_abi::hwtree::{HwDeviceClass, HwNode, HW_NODE_ROOT};
    use rustos_arch_aarch64::{handle_panic_via_serial, qemu_exit, SerialSink, SERIAL_SINK};
    use rustos_kalloc::{FreeListAllocator, Heap, HEAP_BYTES};
    use rustos_kernel::aarch64::boot as boot_aarch64;
    use rustos_kernel::hwtree_store::HW_TREE;
    use rustos_kernel_core::waitq::HW_TREE_WAITQ;
    use rustos_log::{Event, EventId, Sink};

    /// `EventId` the syscall dispatcher emits for an audited syscall
    /// (`init`'s `spawn`/`wait` and the session's `exit`). Pinned by the
    /// audit-id test in `kernel/syscall/src/audit.rs`. The sink only acts on
    /// these: they are emitted by the dispatcher *after* the handler runs,
    /// outside any scheduler lock, so calling the wake path
    /// (`HW_TREE.append` → `hw_tree_wake` → `unpark`) from here is the same
    /// safe context an IPC `send` wakes a receiver from — never re-entrant on
    /// a held run-queue lock (`AGENTS.md` §2.1).
    const SYSCALL_INVOKED_EVENT_ID: EventId = EventId(5000);

    // The canonical QEMU `virt` device tree, dumped and embedded at build
    // time (`build.rs`). The boot pipeline discovers the board from it
    // because QEMU passes no `x0` DTB pointer at an ELF `-kernel` entry.
    include!(concat!(env!("OUT_DIR"), "/dtb_fixture.rs"));

    /// Static boot heap, mirroring the production aarch64 kernel binary's
    /// `.bss`-resident heap (zeroed by the boot trampoline).
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

    /// Sink state machine: `ARMED` until `devmgr` is observed parked,
    /// `BUMPED` after the generation bump is delivered, then PASS.
    const ARMED: u8 = 0;
    const BUMPED: u8 = 1;
    static PHASE: AtomicU8 = AtomicU8::new(ARMED);

    /// The node the sink appends as the simulated hardware hotplug. Its
    /// content is irrelevant to the proof — appending *anything* bumps the
    /// `HwTreeStore` generation and wakes the parked `devmgr` (`AGENTS.md`
    /// §18.4); a distinctive id keeps the serial transcript legible.
    fn hotplug_node() -> HwNode {
        HwNode::new(0x7E57, HW_NODE_ROOT, HwDeviceClass::Other)
    }

    /// Sink that replays every event through [`SERIAL_SINK`] (so the QEMU
    /// transcript records the full boot + spawn + devmgr timeline) and drives
    /// the two-phase reactive proof off the kernel's [`HW_TREE_WAITQ`] (see
    /// the module docs): bump the [`HW_TREE`] store the instant `devmgr` is
    /// seen parked, then report PASS once it has woken, re-read, and
    /// re-parked.
    struct DevmgrReactiveSink;

    impl Sink for DevmgrReactiveSink {
        fn write_event(&self, event: &Event<'_>) {
            SerialSink::new().write_event(event);

            // Act only on audited-syscall events (emitted by the dispatcher
            // outside any scheduler lock), so the wake path the bump triggers
            // is never re-entrant on a held run-queue lock. Other events
            // (spawn/exit audit records, scheduler-internal) only replay to
            // the serial transcript.
            if event.id != SYSCALL_INVOKED_EVENT_ID {
                return;
            }

            // A `devmgr` parked in `hw_tree_wait` is the sole possible waiter
            // on this queue, so non-empty == "devmgr has truly parked".
            let parked = !HW_TREE_WAITQ.is_empty();
            match PHASE.load(Ordering::Acquire) {
                ARMED => {
                    if parked {
                        // Deliver a real generation bump (simulated hotplug):
                        // append to the authoritative store, which bumps the
                        // generation and wakes the parked `devmgr`
                        // (`AGENTS.md` §18.4). Same path the floor bus
                        // bring-up uses, so this is the production wake, not a
                        // test back-channel.
                        HW_TREE.append(&hotplug_node());
                        PHASE.store(BUMPED, Ordering::Release);
                    }
                }
                BUMPED => {
                    // `devmgr` has been scheduled since the bump (login blocks
                    // between events, so the cooperative drain ran the
                    // unparked `devmgr` first): it re-read the tree and
                    // re-parked, re-registering here. The full reactive cycle
                    // is proven.
                    if parked {
                        qemu_exit::exit_success();
                    }
                }
                _ => {}
            }
        }
    }

    static AUDIT_SINK: DevmgrReactiveSink = DevmgrReactiveSink;

    /// Forward to the shared aarch64 panic bridge. A panic before the PASS
    /// finisher parks the CPU, the run times out, and the harness reports
    /// `Outcome::Timeout` — the documented fail-loud behaviour (`AGENTS.md`
    /// §7).
    #[panic_handler]
    fn rustos_devmgr_hwtree_qemu_aarch64_panic(info: &PanicInfo<'_>) -> ! {
        handle_panic_via_serial(info)
    }

    /// Boot entry point — the symbol the arch crate's `boot.s` trampoline
    /// calls (via `rustos_arch_aarch64_main`).
    ///
    /// QEMU hands no DTB pointer (`_dtb == 0`), so the embedded `virt` blob's
    /// address is forwarded to the production boot pipeline with the
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
