//! Multi-worker (SMP) bring-up primitives for the wasm32 browser sandbox.
//!
//! This module is the wasm32 analogue of the bare-metal ports' secondary-
//! core bring-up (`kernel/arch/riscv64::smp` over SBI HSM,
//! `kernel/arch/aarch64::smp` over PSCI `CPU_ON`). A bare-metal port asks
//! firmware to start a parked physical core at a trampoline; the wasm32
//! port asks its JavaScript host to **spawn a Web Worker** that
//! instantiates this same module as a new logical CPU.
//!
//! Like riscv64 and aarch64, SMP is kept **port-side** here, not behind
//! an `Smp` Arch HAL trait — an `Smp` HAL slice remains a future §17.2
//! decision shared by all three ports (`plans/WIRING.md` Stage W6). The
//! architecture-neutral kernel works in dense `CpuId`s and reaches the
//! worker map through [`crate::kernel_arch::WasmArch`].
//!
//! # CPUs are Web Workers
//!
//! The boot context (the main browser thread) is logical CPU `0`. Each
//! secondary CPU is a distinct Web Worker running its own WebAssembly
//! module instance with its own linear memory — the wasm32 isolation
//! boundary (`crate::isolation`). `start_worker` starts logical CPU
//! `index` (where `0 < index < MAX_WORKERS`); `current_worker` recovers
//! the running context's logical CPU id from the host.
//!
//! # Host testability
//!
//! `MAX_WORKERS`, `is_valid_secondary`, and the `StartWorkerError`
//! decode build and are unit-tested on the host. The host `Worker` spawn
//! and the worker-index read are gated to the wasm target; the host build
//! substitutes a counter so `start_worker` is exercised under `cargo
//! test` without a browser (`AGENTS.md` §7 — never silently skip a test).

use rustos_arch_api::CpuId;

pub use crate::kernel_arch::MAX_WORKERS;

/// `true` iff `worker` names a *secondary* logical CPU the host can
/// spawn: in range `1..MAX_WORKERS`. Logical CPU `0` is the boot context
/// (the main thread), which is already running and is never spawned.
#[must_use]
pub const fn is_valid_secondary(worker: CpuId) -> bool {
    worker != 0 && (worker as usize) < MAX_WORKERS
}

/// Failure modes of [`start_worker`].
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum StartWorkerError {
    /// `worker` was `0` (the boot context) or `>= MAX_WORKERS`, so it
    /// does not name a spawnable secondary CPU.
    IndexOutOfRange,
    /// The host refused to start the worker (a duplicate index, or a
    /// context — such as a worker without nested-worker support — that
    /// cannot spawn). The caller fails closed (`AGENTS.md` §5.4.5).
    HostRefused,
}

/// Start logical CPU `worker` as a new Web Worker running this module.
///
/// Range-checks `worker` against the spawnable secondary range before
/// asking the host to spawn it, so an out-of-range index never reaches
/// the host (`AGENTS.md` §2.9 — fail closed). The freshly-spawned worker
/// instantiates the same module and enters through the arch crate's
/// `rustos_arch_wasm32_main` export trampoline; the host reports its
/// logical CPU id through [`current_worker`].
///
/// # Errors
///
/// [`StartWorkerError::IndexOutOfRange`] if `worker` is not a spawnable
/// secondary id; [`StartWorkerError::HostRefused`] if the host declined
/// to start it.
pub fn start_worker(worker: CpuId) -> Result<(), StartWorkerError> {
    if !is_valid_secondary(worker) {
        return Err(StartWorkerError::IndexOutOfRange);
    }
    if host_start_worker(worker) {
        Ok(())
    } else {
        Err(StartWorkerError::HostRefused)
    }
}

/// The logical CPU id of the context executing this call.
///
/// The boot context (the main thread) is `0`; each spawned worker
/// reports the dense index the host assigned it.
#[must_use]
pub fn current_worker() -> CpuId {
    #[cfg(target_arch = "wasm32")]
    {
        crate::bindings::host_current_worker()
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        0
    }
}

/// Ask the host to spawn `worker`.
#[cfg(target_arch = "wasm32")]
fn host_start_worker(worker: CpuId) -> bool {
    crate::bindings::host_start_worker(worker)
}

/// Host substitute for the `Worker` spawn: record the request against a
/// counter and report success for an in-range secondary so the unit tests
/// observe [`start_worker`]'s success path without a browser. Never
/// linked into a wasm image (`AGENTS.md` §1 — no fake primitives in
/// production).
#[cfg(not(target_arch = "wasm32"))]
fn host_start_worker(worker: CpuId) -> bool {
    HOST_STARTED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    is_valid_secondary(worker)
}

/// Host-only count of [`start_worker`] host-spawn requests, observed by
/// the unit tests.
#[cfg(not(target_arch = "wasm32"))]
static HOST_STARTED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Host-test accessor: number of host-spawn requests issued so far.
#[must_use]
#[cfg(not(target_arch = "wasm32"))]
pub fn host_started_count() -> u64 {
    HOST_STARTED.load(core::sync::atomic::Ordering::Relaxed)
}

#[cfg(test)]
#[path = "smp_tests.rs"]
mod tests;
