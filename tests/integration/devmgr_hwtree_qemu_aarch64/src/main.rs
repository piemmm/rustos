//! Design D P-3 QEMU integration test (`.junie/next-pi-prompt.md`): boot the
//! production aarch64 (Raspberry Pi 4) `rustos-kernel` pipeline on the `virt`
//! board, have PID 1 (`init`) launch the **perpetual device-manager service**
//! (`/System/Services/devmgr`), and prove the service's
//! reactive observe loop end to end: it reads the discovered hardware tree
//! (`hw_tree_read`), **truly parks** off the run queue in `hw_tree_wait`
//! (no busy poll), and, on a **real generation
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
//! `HwTreeStore` — and only swaps the injected hardware-tree *source* for the
//! observing `WitnessSource` below (the same dependency-injection seam the
//! boot path already exposes for the log/audit sinks). After
//! `kernel_core::kernel_main` emits `BootCompleted`, PID 1 `init` launches
//! `devmgr` first and then a login session per console; `devmgr` reads the
//! seeded hardware tree (logging it to fd 2) and blocks in
//! `hw_tree_wait(generation, u64::MAX)`, which **registers it on the kernel's
//! `HW_TREE_WAITQ` and parks it** (Design D P-2).
//!
//! ## How the reactive cycle is driven and witnessed — deterministically
//!
//! The witness is driven by `devmgr`'s **own** syscall activity, never by
//! incidental audit traffic: the `hw_tree_wait` handler calls
//! `HwTreeSource::generation` on every loop iteration — in the caller's
//! (`devmgr`'s) context, *after* it has registered on `HW_TREE_WAITQ` and
//! immediately before it commits to park. `devmgr` is the one and only
//! `hw_tree_wait` caller, so at that call a non-empty wait-queue is the
//! unambiguous "devmgr has registered and is about to park" witness. The
//! injected `WitnessSource` forwards every read to the authoritative
//! `HW_TREE_SOURCE` and drives a two-phase machine off that hook:
//!
//! 1. **Armed → Bumped:** the first `generation()` call at which
//!    `HW_TREE_WAITQ` is non-empty proves `devmgr` is about to park. The
//!    source appends a node to the authoritative `HW_TREE` store — a **real**
//!    mutation that bumps the generation and calls `hw_tree_wake`, exactly a
//!    hardware hotplug. `hw_tree_wait` then observes the changed generation
//!    and returns, so `devmgr` never sleeps through the bump; it re-reads the
//!    tree and calls `hw_tree_wait` afresh.
//! 2. **Bumped → PASS:** the next `generation()` call with `HW_TREE_WAITQ`
//!    non-empty is `devmgr` re-registering for its post-bump wait: it woke on
//!    the generation change, re-read the tree (logging the new generation to
//!    fd 2), and re-parked. The full reactive cycle is proven and the source
//!    reports PASS through the ARM semihosting finisher.
//!
//! Because the trigger is `devmgr`'s own read/wait loop, the proof completes
//! whether or not any other task is producing audit events — there is no
//! dependence on a login dialogue "keeping events flowing", the race that made
//! an earlier audit-sink-driven version of this test flaky. A regression where
//! `devmgr` never spawns, never reads, never parks, or never wakes on the bump
//! never reaches the second phase, so the run times out and the harness
//! reports `Outcome::Timeout` — the documented fail-loud behaviour.
//!
//! ## Why acting from `generation()` is a safe context
//!
//! `generation()` runs inside the `hw_tree_wait` handler **before**
//! `reschedule_current(Park)`, so no run-queue lock is held — appending to the
//! store (which takes the store lock, then `hw_tree_wake` → `wake_all` takes
//! the wait-queue lock, both released before any `unpark`) is the same safe
//! task-context hand-off an IPC `send` wakes a receiver from, never re-entrant
//! on a held scheduler lock. Unparking `devmgr` while it is still running the
//! syscall only records the scheduler's wake-pending token, which the Park
//! commit consumes — the same lost-wake interlock the wait path already relies
//! on.
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
extern crate alloc;

#[cfg(itest_aarch64)]
mod kernel {
    use core::panic::PanicInfo;
    use core::sync::atomic::{AtomicU8, Ordering};

    use alloc::vec::Vec;

    use rustos_abi::hwtree::{HwDeviceClass, HwNode, HW_NODE_ROOT};
    use rustos_abi::Errno;
    use rustos_arch_aarch64::{handle_panic_via_serial, qemu_exit, SERIAL_SINK};
    use rustos_kalloc::{FreeListAllocator, Heap, HEAP_BYTES};
    use rustos_kernel::aarch64::boot as boot_aarch64;
    use rustos_kernel::hwtree_store::{HW_TREE, HW_TREE_SOURCE};
    use rustos_kernel_core::waitq::HW_TREE_WAITQ;
    use rustos_kernel_core::HwTreeSource;

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

    /// Witness state machine: `ARMED` until `devmgr` is observed about to
    /// park, `BUMPED` after the generation bump is delivered, then PASS.
    const ARMED: u8 = 0;
    const BUMPED: u8 = 1;
    static PHASE: AtomicU8 = AtomicU8::new(ARMED);

    /// The node the witness appends as the simulated hardware hotplug. Its
    /// content is irrelevant to the proof — appending *anything* bumps the
    /// `HwTreeStore` generation and wakes the parked `devmgr`; a distinctive
    /// id keeps the serial transcript legible.
    fn hotplug_node() -> HwNode {
        HwNode::new(0x7E57, HW_NODE_ROOT, HwDeviceClass::Other)
    }

    /// The injected [`HwTreeSource`]: it forwards every read to the
    /// authoritative [`HW_TREE_SOURCE`] and drives the deterministic
    /// two-phase reactive proof off the `hw_tree_wait` handler's own
    /// [`generation`](HwTreeSource::generation) polls (see the module docs).
    struct WitnessSource;

    impl HwTreeSource for WitnessSource {
        fn generation(&self) -> Result<u64, Errno> {
            // Called by the `hw_tree_wait` handler on each loop iteration,
            // in `devmgr`'s context, after it has registered on
            // `HW_TREE_WAITQ` and just before it commits to park. `devmgr`
            // is the sole `hw_tree_wait` caller, so a non-empty queue here
            // means it has registered and is about to park. The fast-path
            // pre-register `generation()` check sees an empty queue and
            // takes no action.
            if !HW_TREE_WAITQ.is_empty() {
                match PHASE.load(Ordering::Acquire) {
                    ARMED => {
                        // First park witnessed: deliver a real generation
                        // bump (simulated hotplug) via the authoritative
                        // store — the same wake path the floor bus bring-up
                        // uses, not a test back-channel. `hw_tree_wait` then
                        // sees the changed generation and returns, so
                        // `devmgr` wakes, re-reads, and re-parks.
                        HW_TREE.append(&hotplug_node());
                        PHASE.store(BUMPED, Ordering::Release);
                    }
                    BUMPED => {
                        // `devmgr` woke on the bump, re-read the tree, and is
                        // re-registering for its next wait: the full
                        // park → bump → wake → re-read → re-park cycle is
                        // proven.
                        qemu_exit::exit_success();
                    }
                    _ => {}
                }
            }
            HW_TREE_SOURCE.generation()
        }

        fn snapshot(&self) -> Result<Vec<u8>, Errno> {
            HW_TREE_SOURCE.snapshot()
        }

        fn publish(&self, parent_id: u32, node: HwNode) -> Result<u32, Errno> {
            HW_TREE_SOURCE.publish(parent_id, node)
        }

        fn remove(&self, parent_id: u32, node_id: u32) -> Result<(), Errno> {
            HW_TREE_SOURCE.remove(parent_id, node_id)
        }
    }

    static WITNESS_SOURCE: WitnessSource = WitnessSource;

    /// Forward to the shared aarch64 panic bridge. A panic before the PASS
    /// finisher parks the CPU, the run times out, and the harness reports
    /// `Outcome::Timeout` — the documented fail-loud behaviour.
    #[panic_handler]
    fn rustos_devmgr_hwtree_qemu_aarch64_panic(info: &PanicInfo<'_>) -> ! {
        handle_panic_via_serial(info)
    }

    /// Boot entry point — the symbol the arch crate's `boot.s` trampoline
    /// calls (via `rustos_arch_aarch64_main`).
    ///
    /// QEMU hands no DTB pointer (`_dtb == 0`), so the embedded `virt` blob's
    /// address is forwarded to the production boot pipeline with the observing
    /// [`WitnessSource`] installed as the hardware-tree source. Both the log
    /// and audit sinks are the production PL011-backed [`SERIAL_SINK`], so the
    /// QEMU transcript still records the full boot + spawn + devmgr timeline.
    #[no_mangle]
    pub extern "C" fn kernel_main(_dtb: u64) -> ! {
        let dtb = DTB_BLOB.as_ptr() as u64;
        boot_aarch64::boot(dtb, &SERIAL_SINK, &SERIAL_SINK, &WITNESS_SOURCE)
    }
}

// --- Host stub -----------------------------------------------------
#[cfg(not(itest_aarch64))]
fn main() {}

#[cfg(not(itest_aarch64))]
#[allow(dead_code)]
fn _suppress_no_main() {}
