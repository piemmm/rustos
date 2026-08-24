//! [`WasmArch`] — the wasm32 implementation of the Arch HAL
//! ([`tairix_arch_api::SchedulerArch`]).
//!
//! Like the bare-metal ports, the wasm32 port is a pure Arch HAL
//! implementation: it implements [`SchedulerArch`]
//! and exposes the monotonic clock, but it does not name `kernel/core`
//! or implement its `KernelArch` super-trait. The downstream boot
//! consumer wraps [`WasmArch`] in a local `KernelArch` type.
//!
//! # CPUs are Web Workers
//!
//! A bare-metal port maps [`CpuId`] to a hart id / APIC id; the wasm32
//! port maps it to a **Web Worker context**. The boot context is logical
//! CPU `0`; each spawned worker the host starts gets a dense worker
//! index, and [`WasmArch`] holds the `CpuId` → worker-index map the
//! scheduler reaches through. [`SchedulerArch::current_cpu`] recovers
//! the running worker index from the host and reverse-maps it;
//! [`SchedulerArch::send_ipi`] forward-maps a target `CpuId` to the
//! worker index the `MessageChannel` post addresses.
//!
//! # Clock
//!
//! The monotonic clock reads `performance.now()` (fractional
//! milliseconds) through `crate::bindings` and converts to
//! nanoseconds via [`ms_to_ns`], so the tick source and the conversion
//! share one host clock (no parallel measurement).
//!
//! # Host testability
//!
//! The struct, the worker map, and [`ms_to_ns`] build on the host so
//! the unit tests run under `cargo test`. The wasm build reads the real
//! `performance.now()` / worker index through `crate::bindings`; the
//! host build substitutes a monotonic counter *solely* so the host
//! tests observe a non-decreasing clock and is never linked into a wasm
//! image (no fake primitives in production).

use core::sync::atomic::{AtomicU64, Ordering};

use alloc::boxed::Box;
use alloc::vec::Vec;

use tairix_arch_api::{CpuId, SchedulerArch, SecondaryBringup, SmpError};

/// Upper bound on the *worker-index value* the host may be asked to
/// spawn as a secondary logical CPU ([`crate::smp::start_worker`]).
///
/// This is the host worker-addressing bound, **not** the size of the
/// per-worker bookkeeping: [`WasmArch`] now sizes its bookkeeping from
/// the discovered worker count (see
/// [`WasmArch::worker_capacity`]). Converting this remaining bound to a
/// discovered-hardware capacity is tracked as the next L3b
/// increment (the secondary-bring-up item).
pub const MAX_WORKERS: usize = 8;

/// per-CPU sizing policy for [`WasmArch`]: one bookkeeping slot
/// per discovered worker context, with a floor that always covers the
/// boot context's own slot.
///
/// Web-Worker contexts are fixed at boot — the host reports them once —
/// so the discovered count *is* the hardware quantity and no speculative
/// headroom is reserved beyond it. The floor of
/// `boot_cpu + 1` guarantees the boot CPU's slot is always representable
/// even for a single-worker handle.
fn worker_storage_len(boot_cpu: CpuId, discovered: usize) -> usize {
    discovered.max(boot_cpu as usize + 1)
}

/// wasm32 architecture handle the downstream boot consumer wraps for
/// `kernel_core::kernel_main`.
///
/// Carries the dense [`CpuId`] → worker-index map the SMP scheduler
/// reaches through. Stable for the lifetime of the kernel image (the map
/// is populated once at construction; the host-only counters exist
/// solely for deterministic unit tests, mirroring the bare-metal ports).
#[derive(Debug)]
pub struct WasmArch {
    boot_cpu: CpuId,

    /// Forward map: dense `CpuId` index → host worker index of that CPU.
    /// `None` for unpopulated slots. Set once at construction; its length
    /// is the discovered worker count (see
    /// `worker_storage_len`), never a fixed ceiling.
    cpu_to_worker: Box<[Option<CpuId>]>,

    /// Host-only IPI accounting — incremented on every `send_ipi` with
    /// an in-range, mapped target. Never read on the wasm path; the host
    /// `MessageChannel` post replaces it there. Sized to match
    /// [`Self::cpu_to_worker`], so a target index that resolves through
    /// [`Self::worker_of`] is always in range here too.
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    host_ipi_count: Box<[AtomicU64]>,

    /// Host-only stray-IPI counter for unmapped / out-of-range targets.
    host_stray_ipi: AtomicU64,
}

impl WasmArch {
    /// Construct a single-worker handle for `boot_cpu` running in the
    /// boot context (worker index equal to `boot_cpu`).
    ///
    /// Use [`Self::with_workers`] to register a multi-worker map.
    #[must_use]
    pub fn new(boot_cpu: CpuId) -> Self {
        let cap = worker_storage_len(boot_cpu, 0);
        let mut cpu_to_worker: Vec<Option<CpuId>> = (0..cap).map(|_| None).collect();
        // `boot_cpu < cap` holds by construction of `worker_storage_len`.
        cpu_to_worker[boot_cpu as usize] = Some(boot_cpu);
        Self::from_map(boot_cpu, cpu_to_worker.into_boxed_slice())
    }

    /// Construct a multi-worker handle from a dense `CpuId` →
    /// worker-index slice (`workers[cpu] == worker_index`).
    ///
    /// The handle's per-worker bookkeeping is sized to the discovered
    /// worker count — `workers.len()`, floored at `boot_cpu + 1` (see
    /// `worker_storage_len`) — so a larger machine is never silently
    /// truncated to a fixed ceiling. `boot_cpu` names
    /// the logical CPU of the boot context.
    #[must_use]
    pub fn with_workers(boot_cpu: CpuId, workers: &[CpuId]) -> Self {
        let cap = worker_storage_len(boot_cpu, workers.len());
        let cpu_to_worker: Vec<Option<CpuId>> =
            (0..cap).map(|cpu| workers.get(cpu).copied()).collect();
        Self::from_map(boot_cpu, cpu_to_worker.into_boxed_slice())
    }

    fn from_map(boot_cpu: CpuId, cpu_to_worker: Box<[Option<CpuId>]>) -> Self {
        let host_ipi_count: Vec<AtomicU64> = (0..cpu_to_worker.len())
            .map(|_| AtomicU64::new(0))
            .collect();
        Self {
            boot_cpu,
            cpu_to_worker,
            host_ipi_count: host_ipi_count.into_boxed_slice(),
            host_stray_ipi: AtomicU64::new(0),
        }
    }

    /// Number of dense `CpuId` slots this handle tracks — the discovered
    /// worker count its per-worker bookkeeping was sized to.
    #[must_use]
    pub fn worker_capacity(&self) -> usize {
        self.cpu_to_worker.len()
    }

    /// Worker index mapped to `cpu`, or `None` for an unpopulated slot.
    #[must_use]
    pub fn worker_of(&self, cpu: CpuId) -> Option<CpuId> {
        let idx = usize::try_from(cpu).ok()?;
        self.cpu_to_worker.get(idx).copied().flatten()
    }

    /// Dense `CpuId` whose mapped worker index is `worker`, or `None` if
    /// no CPU maps to it.
    #[must_use]
    pub fn cpu_for_worker(&self, worker: CpuId) -> Option<CpuId> {
        let mut cpu = 0;
        while cpu < self.cpu_to_worker.len() {
            if self.cpu_to_worker[cpu] == Some(worker) {
                // The index is bounded by `cpu_to_worker`, which is sized
                // from the `u32` CPU count.
                #[allow(clippy::cast_possible_truncation)]
                return Some(cpu as CpuId);
            }
            cpu += 1;
        }
        None
    }

    /// Host-test accessor: total IPIs dispatched to `target`.
    #[must_use]
    #[cfg(not(target_arch = "wasm32"))]
    pub fn host_ipi_count(&self, target: CpuId) -> u64 {
        let idx = match usize::try_from(target) {
            Ok(i) if i < self.host_ipi_count.len() => i,
            _ => return 0,
        };
        self.host_ipi_count[idx].load(Ordering::Relaxed)
    }

    /// Host-test accessor: IPIs whose target was unmapped / out of range.
    #[must_use]
    #[cfg(not(target_arch = "wasm32"))]
    pub fn host_stray_ipi_count(&self) -> u64 {
        self.host_stray_ipi.load(Ordering::Relaxed)
    }

    /// Monotonic nanoseconds since the host clock's epoch.
    ///
    /// Reads `performance.now()` (fractional milliseconds) and converts
    /// via [`ms_to_ns`], so the tick source and the conversion share one
    /// host clock. The downstream `KernelArch`
    /// wrapper forwards `monotonic_ns` here.
    #[must_use]
    pub fn monotonic_ns(&self) -> u64 {
        ms_to_ns(read_now_ms())
    }
}

impl SchedulerArch for WasmArch {
    fn current_cpu(&self) -> CpuId {
        #[cfg(target_arch = "wasm32")]
        {
            // Recover the running worker index from the host and
            // reverse-map it to a dense `CpuId`. An unmapped worker falls
            // back to the boot CPU rather than inventing an id
            // (fail closed).
            let worker = crate::bindings::host_current_worker();
            self.cpu_for_worker(worker).unwrap_or(self.boot_cpu)
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.boot_cpu
        }
    }

    fn ticks_now(&self) -> u64 {
        ms_to_ns(read_now_ms())
    }

    fn send_ipi(&self, target: CpuId) {
        // Resolve the destination worker first. Sending to the calling
        // CPU is permitted (a self-reschedule). An unmapped / out-of-range
        // target is dropped rather than panicking — `send_ipi` is
        // best-effort, and stray IPIs are recorded for host tests.
        let Some(worker) = self.worker_of(target) else {
            self.host_stray_ipi.fetch_add(1, Ordering::Relaxed);
            return;
        };

        #[cfg(target_arch = "wasm32")]
        {
            crate::bindings::host_post_ipi(worker);
        }

        #[cfg(not(target_arch = "wasm32"))]
        {
            // Host: count the IPI against the target CPU; `worker_of`
            // already validated the index is in range.
            let _ = worker;
            self.host_ipi_count[target as usize].fetch_add(1, Ordering::Relaxed);
        }
    }
}

impl SecondaryBringup for WasmArch {
    unsafe fn start_secondary(&self, cpu: CpuId) -> Result<(), SmpError> {
        // Fail closed before asking the host to spawn anything: the boot
        // context is already running, and an unmapped dense id has no
        // worker to spawn.
        if cpu == self.boot_cpu {
            return Err(SmpError::InvalidCpu);
        }
        let Some(worker) = self.worker_of(cpu) else {
            return Err(SmpError::InvalidCpu);
        };
        // A secondary is a fresh Web Worker instantiating this same
        // module; there is no settable entry pointer to install (the
        // worker enters through the fixed `tairix_arch_wasm32_main`
        // export), so wasm32 never reports `NotReady`.
        match crate::smp::start_worker(worker) {
            Ok(()) => Ok(()),
            Err(crate::smp::StartWorkerError::IndexOutOfRange) => Err(SmpError::InvalidCpu),
            Err(crate::smp::StartWorkerError::HostRefused) => Err(SmpError::StartRejected(0)),
        }
    }
}

/// Convert a `performance.now()` reading in fractional milliseconds to
/// whole nanoseconds.
///
/// A negative or non-finite reading (which the host clock never produces)
/// clamps to `0`, and an out-of-range product saturates at [`u64::MAX`],
/// so the conversion never panics or traps.
#[must_use]
pub fn ms_to_ns(ms: f64) -> u64 {
    if !ms.is_finite() || ms <= 0.0 {
        return 0;
    }
    let ns = ms * 1_000_000.0;
    // `u64::MAX as f64` rounds to the nearest representable `f64`; that
    // is precise enough for a saturation bound, where any reading at or
    // beyond it is clamped anyway. The precision loss is intentional.
    #[allow(clippy::cast_precision_loss)]
    let u64_max_as_f64 = u64::MAX as f64;
    if ns >= u64_max_as_f64 {
        u64::MAX
    } else {
        // `ns` is finite, positive, and below `u64::MAX`, so the cast is
        // exact-to-truncation toward zero.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let truncated = ns as u64;
        truncated
    }
}

/// Read the host monotonic clock in fractional milliseconds.
#[cfg(target_arch = "wasm32")]
pub(crate) fn read_now_ms() -> f64 {
    crate::bindings::host_now_ms()
}

/// Host substitute for `performance.now()`: a strictly increasing
/// counter (in "milliseconds") so the unit tests observe a monotonic
/// clock. Never linked into a wasm image (the wasm build uses the
/// `crate::bindings` reading above).
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn read_now_ms() -> f64 {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let ticks = COUNTER.fetch_add(1, Ordering::Relaxed) + 1;
    // Each host tick advances the synthetic clock by one millisecond.
    // This counter is a test-only monotonic source whose absolute value
    // is irrelevant; the precision loss past 2^52 ticks is immaterial
    // and never reached in a unit test.
    #[allow(clippy::cast_precision_loss)]
    let ms = ticks as f64;
    ms
}

#[cfg(test)]
#[path = "kernel_arch_tests.rs"]
mod tests;
