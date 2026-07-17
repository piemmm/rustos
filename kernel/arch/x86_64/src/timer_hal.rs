//! x86_64 timer programming ("timer programming").
//!
//! Implements the Arch HAL [`Timer`](tairix_arch_api::Timer) surface for
//! x86_64 over the LAPIC timer wired in [`crate::preempt`]. The HAL
//! handle is the architecture-neutral half of the timer path: it
//! installs the one scheduler-tick callback and dispatches a tick to it.
//! The *hardware* arming/re-arming (programming the LAPIC LVT and the
//! end-of-interrupt write) stays in [`crate::preempt`] — it is per-CPU
//! LAPIC MMIO work with no architecture-neutral shape.
//!
//! Unlike the claim-based ports, the x86_64 timer ISR
//! (`crate::preempt::tairix_arch_x86_64_timer_dispatch`) must read the
//! LAPIC ID from MMIO to derive the firing CPU and issue the LAPIC EOI
//! write — work that has no host analogue and cannot be expressed by the
//! architecture-neutral [`Timer::dispatch_tick`](tairix_arch_api::Timer::dispatch_tick)
//! — so the ISR keeps its
//! own dispatch and this handle is the HAL-facing surface (callback
//! install + the architecture-neutral dispatch the conformance vertical
//! exercises). On the bare-metal target the callback lives in
//! [`crate::preempt`]'s lock-free static (the ISR's source of truth), so
//! the handle forwards to it; on the host build there is no ISR, so the
//! handle backs the callback with an in-handle cell solely for the
//! [`conformance`](tairix_arch_api::timer::conformance) vertical. It is
//! never linked into a kernel image.

use core::sync::atomic::AtomicUsize;
// `Ordering` is only named by the host backing cell; the bare-metal
// build forwards to `preempt` and never touches it.
#[cfg(not(all(target_arch = "x86_64", target_os = "none")))]
use core::sync::atomic::Ordering;

use tairix_arch_api::{CpuId, TickFn, Timer};

/// x86_64 implementation of the Arch HAL timer-programming surface.
///
/// Zero per-handle state on the bare-metal target — the callback lives
/// in [`crate::preempt`]'s static; the in-handle cell backs it only on
/// the host build so the conformance vertical runs under `cargo test`.
#[derive(Debug, Default)]
pub struct TimerHal {
    /// Host-only backing for the tick callback. On the bare-metal target
    /// [`crate::preempt`]'s static is the source of truth and this field
    /// is never read; kept so the host and bare-metal builds share one
    /// struct shape.
    #[cfg_attr(all(target_arch = "x86_64", target_os = "none"), allow(dead_code))]
    host_callback: AtomicUsize,
}

impl TimerHal {
    /// Construct the x86_64 timer handle.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            host_callback: AtomicUsize::new(0),
        }
    }
}

impl Timer for TimerHal {
    fn set_tick_callback(&self, callback: TickFn) {
        #[cfg(all(target_arch = "x86_64", target_os = "none"))]
        {
            crate::preempt::set_timer_callback(callback);
        }
        #[cfg(not(all(target_arch = "x86_64", target_os = "none")))]
        {
            self.host_callback
                .store(callback as usize, Ordering::Relaxed);
        }
    }

    fn tick_callback(&self) -> Option<TickFn> {
        #[cfg(all(target_arch = "x86_64", target_os = "none"))]
        {
            crate::preempt::timer_callback()
        }
        #[cfg(not(all(target_arch = "x86_64", target_os = "none")))]
        {
            let raw = self.host_callback.load(Ordering::Relaxed);
            if raw == 0 {
                None
            } else {
                // SAFETY: every store is the round-trip of a valid
                // `TickFn` pointer through `set_tick_callback`.
                Some(unsafe { core::mem::transmute::<usize, TickFn>(raw) })
            }
        }
    }

    fn dispatch_tick(&self, cpu: CpuId) -> bool {
        match self.tick_callback() {
            Some(cb) => {
                cb(cpu);
                true
            }
            None => false,
        }
    }

    fn arm_oneshot(&self, ticks_from_now: u64) {
        #[cfg(all(target_arch = "x86_64", target_os = "none"))]
        {
            crate::preempt::arm_oneshot(ticks_from_now);
        }
        #[cfg(not(all(target_arch = "x86_64", target_os = "none")))]
        {
            // No LAPIC on the host; the conformance vertical only requires
            // the call to be total.
            let _ = ticks_from_now;
        }
    }

    fn disarm(&self) {
        #[cfg(all(target_arch = "x86_64", target_os = "none"))]
        {
            crate::preempt::disarm();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tairix_arch_api::timer::conformance;

    #[test]
    fn passes_timer_conformance() {
        conformance::run_all(&TimerHal::new());
        let dynamic: &dyn Timer = &TimerHal::new();
        conformance::run_all(dynamic);
    }
}
