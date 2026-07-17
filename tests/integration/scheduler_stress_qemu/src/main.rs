//! Stage 3a (b) QEMU integration test: scheduler stress on real cores.
//!
//! ## What this test asserts
//!
//! `PLAN.md`'s Stage 2 deliverable text mandates that the scheduler
//! stress test exercise `≥ 4 emulated cores`. The host-side
//! `tairix-test-scheduler-stress` crate satisfies the
//! cross-crate contract by simulating four cores through `TestArch`;
//! this binary executes the same kind of workload under QEMU on
//! `-smp 4` with the actual x86_64 AP bring-up code from
//! `tairix_arch_x86_64::smp` driving the three application processors.
//!
//! ## How it asserts it
//!
//! 1. The BSP parses Multiboot2 → RSDP → XSDT/RSDT → MADT to enumerate
//!    every LAPIC the firmware reports, then identifies its own LAPIC
//!    ID via the LAPIC ID register.
//! 2. The BSP software-enables the LAPIC, installs the AP trampoline at
//!    `AP_TRAMPOLINE_PHYS = 0x8000`, and walks the discovered AP list
//!    serially: per AP it writes the `ApBootSlot` and drives the
//!    INIT-SIPI-SIPI handshake through `init_sipi_sipi`, then waits
//!    (acquire-load) on the per-slot `ready` flag the trampoline's
//!    `xchg` raised.
//! 3. The BSP constructs a single `Arc<Scheduler<SmpArch>>` and
//!    publishes it through a `static AtomicPtr`. Each AP `Acquire`-loads
//!    the pointer before entering its cooperative `step` loop.
//! 4. The BSP spawns `TASKS_PER_CPU * cpu_count` tasks evenly across
//!    home CPUs. Each task increments `EXECUTIONS` once and `Exit`s.
//! 5. All cores spin in `Scheduler::step(cpu_id)` until
//!    `live_task_count() == 0`. A round counter caps the busy loop
//!    (deadlock-freedom check). The BSP then asserts the recorded
//!    execution count equals the expected count and exits via
//!    `qemu_exit::exit_success()`.
//!
//! No preemption: the LAPIC timer is not armed in Stage 3a (b) (it lands
//! in Stage 3a (c) per `PLAN.md`); the cooperative `step` loop suffices
//! to prove "real CPUs actually executed scheduler work in parallel"
//! because each `step` body returns `TaskAction::Exit` immediately.

#![cfg_attr(itest_x86_64, no_std)]
#![cfg_attr(itest_x86_64, no_main)]
#![deny(missing_docs)]
// Workload sizing constants are hand-tuned for the 256 MiB QEMU spec; a
// few lossless `as` casts (CPU id u32 → i32 for arithmetic, atomic u32
// reads) keep the boot-path code readable. rule 10
// requires every `#[allow]` carry a justification — this is it.
#![cfg_attr(
    itest_x86_64,
    allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)
)]

#[cfg(itest_x86_64)]
mod kernel;

// Host-target stub mirrors `tests/integration/memory_isolation/src/main.rs`.
// The crate produces a meaningful artefact only for x86_64-unknown-none;
// on the host triple it has nothing to run.
#[cfg(not(itest_x86_64))]
fn main() {}
#[cfg(not(itest_x86_64))]
#[allow(dead_code)]
fn _suppress_no_main() {}
