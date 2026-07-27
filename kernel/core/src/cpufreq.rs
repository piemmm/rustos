//! Per-CPU live-core-frequency estimator.
//!
//! The System Information API reports the *live* clock frequency of every
//! CPU — the "cpu MHz" a `/proc/cpuinfo`-style reader expects — and it must
//! be a real measurement, never a fabricated or nominal figure. The naive
//! source, the [`tairix_arch_api::CpuCycles`] counter, is a **fixed-rate
//! reference** time base and so cannot track dynamic voltage/frequency
//! scaling; the live clock instead comes from the Arch HAL
//! [`CoreClock`] slice, whose core-clock counter
//! advances at the actual core frequency.
//!
//! # How the estimate is taken (no blocking, no busy-wait)
//!
//! A core running at frequency `f` accrues `f · Δt` core-clock cycles while
//! the fixed reference counter accrues `reference_hz · Δt` reference ticks
//! over the same span, so their ratio yields `f` independent of `Δt`
//! ([`tairix_arch_api::frequency_hz`]). The estimator therefore never waits:
//! at each per-CPU preemption tick ([`crate::preempt::note_preempt_tick`]) it
//! reads the calling CPU's core and reference counters, divides the deltas
//! since the previous tick, and publishes the result. An idle CPU that takes
//! no ticks simply keeps its last published value (a reader sees the last
//! measured clock, exactly as Linux's `aperfmperf` sampler does); a port with
//! no core-clock counter publishes nothing and readers fall back to the
//! discovered nominal frequency (fail closed — never a fabricated rate).
//!
//! The sampled counters live in interrupt-context-safe per-CPU atomics
//! (the kernel's per-CPU `CpuState`); each CPU only ever writes its own slot,
//! so the sample path is lock-free and allocation-free.

use core::sync::atomic::{AtomicU64, Ordering};

use tairix_arch_api::{frequency_hz, CoreClock};
use tairix_kernel_sched_api::CpuId;
use tairix_sync::once::OnceCell;

use crate::cpu_state;

/// The installed per-boot core-clock source, or `None` on a port without the
/// Arch HAL `coreclock` slice (the host test arch) — set once at boot.
static CORE_CLOCK: OnceCell<&'static dyn CoreClock> = OnceCell::new();

/// The installed source's fixed reference/timebase frequency in Hz, cached
/// once at [`install`] so the per-tick [`sample`] never re-reads it. `0`
/// means "no supported source" — the estimator then does nothing and readers
/// fall back to the discovered nominal frequency. Support and the reference
/// frequency are boot constants (a port either has a core-clock counter or
/// does not), so hoisting them off the hot interrupt path costs the sampler
/// nothing per tick beyond the two counter reads it genuinely needs.
static REFERENCE_HZ: AtomicU64 = AtomicU64::new(0);

/// Install the port's [`CoreClock`] source and enable it on the boot CPU.
///
/// Called once from [`crate::kernel_main`] with the port's handle (when it
/// exposes one). Idempotent set-once: a second install is ignored. A port
/// whose handle is not [`tairix_arch_api::CoreClockSupport::Supported`] is
/// still installed — its reads fail closed to `0`, so the estimator publishes
/// no frequency and readers fall back to the nominal figure.
pub fn install(clock: &'static dyn CoreClock) {
    if CORE_CLOCK.set(clock).is_ok() {
        clock.enable();
        // Cache the boot-constant reference frequency once: a supported
        // source with a known reference rate arms the per-tick sampler;
        // anything else leaves `REFERENCE_HZ` at `0` (sampler inert).
        if clock.support().is_supported() {
            REFERENCE_HZ.store(clock.reference_hz(), Ordering::Relaxed);
        }
    }
}

/// Enable the installed core-clock counter on the *calling* CPU.
///
/// Called by each secondary as it comes up ([`crate::run_secondary`]) so a
/// per-CPU counter (aarch64 `PMCCNTR_EL0`) is armed on every core, not only
/// the boot CPU. A no-op before [`install`] or on an unsupported port.
pub fn enable_this_cpu() {
    if let Ok(Some(clock)) = CORE_CLOCK.get() {
        clock.enable();
    }
}

/// `true` if a supported live-core-frequency source is installed and its
/// reference frequency is known.
#[must_use]
pub fn is_supported() -> bool {
    REFERENCE_HZ.load(Ordering::Relaxed) != 0
}

/// Take one frequency sample for the calling CPU, `cpu`.
///
/// Called from [`crate::preempt::note_preempt_tick`] on every fired one-shot
/// — a per-CPU periodic point off the context-switch hot path. Reads this
/// CPU's core and reference counters, divides the deltas since the previous
/// sample (scaled by the reference frequency), and publishes the live clock
/// into this CPU's per-CPU `CpuState` slot. Pure per-CPU
/// accounting: lock-free, allocation-free, and safe from interrupt context.
///
/// The very first sample on a CPU only records the baseline counters (there
/// is no prior pair to difference), so the published frequency stays `0`
/// ("not yet measured") until the second tick.
pub fn sample(cpu: CpuId) {
    // Fast, boot-constant gate: an unsupported port (or one with no known
    // reference rate) leaves `REFERENCE_HZ` at `0`, so the hot interrupt path
    // returns after a single relaxed load — it touches no register.
    let reference_hz = REFERENCE_HZ.load(Ordering::Relaxed);
    if reference_hz == 0 {
        return;
    }
    let Ok(Some(clock)) = CORE_CLOCK.get() else {
        return;
    };
    let Some(state) = cpu_state::get(cpu) else {
        return;
    };
    // Read the pair as close together as possible so the ratio reflects one
    // instant. Both are the calling CPU's own registers.
    let core = clock.core_cycles();
    let reference = clock.reference_cycles();

    let last_core = state.freq_last_core.load(Ordering::Relaxed);
    let last_reference = state.freq_last_ref.load(Ordering::Relaxed);

    // Publish the new baseline for the next tick regardless of whether this
    // tick could produce an estimate.
    state.freq_last_core.store(core, Ordering::Relaxed);
    state.freq_last_ref.store(reference, Ordering::Relaxed);

    // A zero stored reference means "no prior sample yet" (the counters are
    // running from boot, so a real prior sample is never zero); skip until
    // the next tick has a genuine pair to difference.
    if last_reference == 0 {
        return;
    }
    // Counters are monotonic on one core but may wrap; wrapping subtraction
    // yields the true elapsed delta for any 64-bit counter.
    let delta_core = core.wrapping_sub(last_core);
    let delta_reference = reference.wrapping_sub(last_reference);
    if let Some(hz) = frequency_hz(delta_core, delta_reference, reference_hz) {
        state.freq_hz.store(hz, Ordering::Relaxed);
    }
}

/// The most recently measured live core frequency of `cpu`, in Hz.
///
/// `0` means "not yet measured" — the honest unknown a reader falls back from
/// to the discovered nominal frequency, never a fabricated rate. An
/// out-of-range `cpu` reports `0` (fail closed).
#[must_use]
pub fn current_freq_hz(cpu: CpuId) -> u64 {
    cpu_state::get(cpu).map_or(0, |state| state.freq_hz.load(Ordering::Relaxed))
}

/// The fixed reference frequency of the installed core-clock source, in Hz,
/// or `0` when no supported source is installed.
///
/// Exposed for the System Information CPU-info gather so a reader can render
/// the reference/timebase frequency alongside the measured core clock. Reads
/// the value cached at [`install`], so it costs one relaxed load.
#[must_use]
pub fn reference_hz() -> u64 {
    REFERENCE_HZ.load(Ordering::Relaxed)
}
