//! Timer-programming surface of the Arch HAL (
//! "timer programming").
//!
//! RustOS is a **tickless (`NO_HZ`)** kernel: the timer
//! is armed *one-shot*, to the next event the scheduler needs, never at a
//! fixed frequency. Each port owns a per-CPU timer — the x86_64 LAPIC
//! timer, the aarch64 EL1 generic timer, the riscv64 supervisor (SBI)
//! timer, and the wasm32 cooperative `requestAnimationFrame` loop. The
//! hardware that produces the interrupt is wildly different per target,
//! but the kernel needs the *same* two things from each: install the one
//! architecture-neutral scheduler-tick callback, and, on every fire,
//! invoke that callback with the firing CPU's [`crate::CpuId`].
//!
//! The one-shot *arming* (programming the LAPIC one-shot count,
//! `CNTP_TVAL_EL0`, the SBI timer, or requesting the next animation
//! frame) is performed through [`Timer::arm_oneshot`] /
//! [`Timer::disarm`]: the *decision* to arm a relative one-shot deadline
//! is architecture-neutral (the scheduler makes it — see
//! [`crate::SchedulerArch::set_preemption`]), and only the per-port
//! register/MMIO write differs, so the two arming verbs live on this one
//! HAL surface while their bodies stay genuinely
//! per-port. RustOS arms the timer **one-shot, to the next event
//! the scheduler needs, and leaves it disarmed when a CPU has nothing to
//! preempt to** — there is no fixed-frequency periodic re-arm anywhere
//! (PLAN P-4 retired the P-1 100 Hz arming).
//! makes the architecture surface a closed set of traits on the HAL;
//! this module is the "timer programming" member of that set, so the
//! callback install/dispatch *and* the one-shot arm/disarm live behind
//! one vocabulary instead of being re-described per port. The parallel per-arch implementations of this one trait are the
//! deliberate shape of modularity, never collapsed behind
//! `cfg` (carve-out).
//!
//! # What lives here
//!
//! * [`TickFn`] — the scheduler-tick callback type. A plain
//!   `extern "C" fn(CpuId)` (not a closure) so it is safe to invoke from
//!   interrupt/trap context: there is no captured environment that could
//!   be dropped mid-flight.
//! * [`Timer`] — the per-port handle the kernel reaches through. It
//!   installs the tick callback ([`Timer::set_tick_callback`]); from the
//!   port's interrupt/frame path it dispatches a tick
//!   ([`Timer::dispatch_tick`]) which invokes the installed callback; and
//!   it programs the per-CPU hardware timer **one-shot**
//!   ([`Timer::arm_oneshot`]) or stops it ([`Timer::disarm`]). The
//!   one-shot deadline is supplied by the scheduler
//!   ([`crate::SchedulerArch::set_preemption`]); the per-port body is the
//!   LAPIC one-shot count / `CNTP_TVAL_EL0` / SBI `set_timer` / next
//!   animation-frame request. There is no periodic re-arm: a fired timer
//!   recurs only if the scheduler arms it again.
//! * [`conformance`] — the conformance vertical: a host-run
//!   [`conformance::run_all`] check every port runs over its [`Timer`]
//!   handle, proving the installed callback fires on dispatch and that a
//!   handle with no callback installed dispatches harmlessly.
//!
//! # Why the conformance vertical is per-port-driven
//!
//! Like [`crate::irq::conformance`], this vertical is driven from each
//! port's own `conformance` host test rather than folded into
//! [`crate::conformance::run_all`]: a [`Timer`] handle is constructed
//! per port and the dispatch path on the bare-metal targets reaches a
//! port-private callback slot, so the suite runs over the port's real
//! handle in that port's crate.

use crate::CpuId;

/// Default preemption-quantum rate, in slices per second, shared by the
/// down-counter ports (aarch64 generic timer, riscv64 supervisor timer).
///
/// A `100 Hz` (~10 ms) slice bounds a runaway user task's hold on a
/// *contended* CPU while costing negligible trap overhead. This is **not**
/// a periodic tick: RustOS is tickless, so the timer
/// is armed one-shot to one quantum only when a CPU has a competitor, and
/// disarmed otherwise. The value lives here, once, so the aarch64 and
/// riscv64 ports (which both derive their per-quantum counter-tick
/// interval via `interval_for_hz(discovered_hz, DEFAULT_PREEMPT_QUANTUM_HZ)`)
/// share a single definition rather than each carrying their own copy. The x86_64 port arms its LAPIC one-shot from a
/// calibration expressed in microseconds and keeps its own period; a port
/// whose silicon genuinely wants a different slice overrides locally.
pub const DEFAULT_PREEMPT_QUANTUM_HZ: u64 = 100;

/// The scheduler-tick callback a [`Timer`] invokes on every tick.
///
/// `extern "C" fn(CpuId)` rather than a closure: the port stores it in a
/// lock-free slot and invokes it from interrupt/trap/frame context, so it
/// must have no captured environment to drop mid-flight. The argument is the firing CPU's [`CpuId`] — the value the
/// scheduler needs to advance the right run queue without re-deriving it
/// on every tick.
pub type TickFn = extern "C" fn(CpuId);

/// The timer-programming handle an architecture port exposes.
///
/// The kernel installs the scheduler-tick callback once with
/// [`Self::set_tick_callback`] before any tick can fire, and the port's
/// interrupt/frame path calls [`Self::dispatch_tick`] on every tick to
/// run it. The hardware arming that produces those ticks stays in the
/// port (see the module docs); this trait is the architecture-neutral
/// callback install + dispatch the rest of the kernel reaches through.
///
/// Implementations must be [`Send`] + [`Sync`]: the kernel reaches the
/// handle from every CPU's tick path. A port's handle is typically
/// zero-sized — the callback lives in a port-private static, not in the
/// handle — exactly like the [`crate::PerCpu`] handle whose word lives in
/// a register.
pub trait Timer: Send + Sync {
    /// Install the scheduler-tick callback.
    ///
    /// Called once during bring-up, before the timer is armed. A later
    /// call replaces the slot atomically. Storing a [`TickFn`] (not a
    /// closure) keeps it safe to invoke from interrupt context.
    fn set_tick_callback(&self, callback: TickFn);

    /// Read the currently-installed tick callback, or [`None`] if none
    /// has been installed yet.
    fn tick_callback(&self) -> Option<TickFn>;

    /// Dispatch one tick for `cpu`: invoke the installed callback with
    /// `cpu`, returning `true` if a callback ran and `false` if none was
    /// installed.
    ///
    /// This is the architecture-neutral half of the port's
    /// interrupt/frame handler. A tick that arrives before any callback
    /// is installed dispatches harmlessly (returns `false`), never a
    /// panic. The port's handler does **not** re-arm
    /// the timer after this returns: RustOS is tickless, so the next fire
    /// happens only if the scheduler arms it again via
    /// [`Self::arm_oneshot`].
    fn dispatch_tick(&self, cpu: CpuId) -> bool;

    /// Arm the calling CPU's timer **one-shot** to fire once after
    /// `ticks_from_now` of the port's counter ticks, then stop until
    /// armed again.
    ///
    /// This is the per-CPU register/MMIO write the tickless preemption
    /// path uses (the LAPIC one-shot initial count, `CNTP_TVAL_EL0`, an
    /// SBI `set_timer(now + ticks_from_now)`, or the next animation-frame
    /// request). It acts on the **calling** CPU's timer only — a one-shot
    /// deadline is inherently a write to a per-CPU register — so the
    /// scheduler calls it from the CPU whose running task it wants to
    /// bound (: armed to the next event the scheduler
    /// needs, never at a fixed frequency). A `ticks_from_now` of `0` is
    /// clamped by the port to at least one tick so a degenerate deadline
    /// cannot wedge the CPU re-trapping with no progress.
    ///
    /// Calling it before any tick callback is installed is permitted; the
    /// fire will dispatch harmlessly (see [`Self::dispatch_tick`]).
    fn arm_oneshot(&self, ticks_from_now: u64);

    /// Stop the calling CPU's timer so no further interrupt fires until
    /// the next [`Self::arm_oneshot`].
    ///
    /// The scheduler disarms when the calling CPU has nothing to preempt
    /// to — it is idle or runs a single runnable task — so an otherwise
    /// quiet core takes no timer interrupts at all (
    /// tickless / `NO_HZ`; — work paid off the hot path). Like
    /// [`Self::arm_oneshot`] it acts on the calling CPU's per-CPU timer
    /// only. Disarming an already-stopped timer is a harmless no-op.
    fn disarm(&self);
}

/// The timer-programming conformance vertical.
///
/// Every architecture port runs [`conformance::run_all`] against its
/// [`Timer`] handle. The suite is portable — it names only the trait —
/// and runs on the host, exactly like the sibling
/// [`crate::percpu::conformance`] and [`crate::irq::conformance`]
/// verticals. It is driven per port (not folded into
/// [`crate::conformance::run_all`]) because a [`Timer`] handle is
/// constructed per port and its dispatch reaches a port-private callback
/// slot.
pub mod conformance {
    use super::{TickFn, Timer};
    use crate::CpuId;
    use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

    /// Records the ticks the conformance callback observes, so the suite
    /// can assert the dispatched callback actually ran with the CPU it
    /// was handed.
    static TICKS: AtomicU64 = AtomicU64::new(0);
    /// The last [`CpuId`] the conformance callback saw.
    static LAST_CPU: AtomicU32 = AtomicU32::new(u32::MAX);

    /// The conformance scheduler-tick callback: counts each tick and
    /// records the CPU it fired on.
    extern "C" fn probe_tick(cpu: CpuId) {
        TICKS.fetch_add(1, Ordering::Relaxed);
        LAST_CPU.store(cpu, Ordering::Relaxed);
    }

    /// Run the entire [`Timer`] conformance suite against `timer`.
    ///
    /// Because the probe callback and its counters are process-global,
    /// the suite is not safe to run from two threads at once; the port
    /// host tests run it single-threaded under `cargo test`, the same as
    /// the other HAL verticals.
    ///
    /// # Panics
    ///
    /// Panics (failing the test) if a handle with no callback installed
    /// reports one, if the installed callback does not round-trip, if
    /// dispatching a tick does not invoke the callback with the CPU it
    /// was handed, or if arming/disarming the one-shot is not a total
    /// operation.
    pub fn run_all<T: Timer + ?Sized>(timer: &T) {
        no_callback_dispatches_harmlessly(timer);
        installed_callback_fires_on_dispatch(timer);
        arm_and_disarm_are_total(timer);
    }

    /// Arming a one-shot deadline and disarming are total operations that
    /// never panic for any input — including a zero deadline (clamped by
    /// the port) and a disarm of an already-stopped timer. The cross-core *effect* (an interrupt actually firing once)
    /// is not observable from a single-threaded host test; it is proven
    /// by the `preempt_el0_qemu_*` verticals. Here we only pin that the
    /// two arming verbs are callable and harmless on the port's real
    /// handle.
    fn arm_and_disarm_are_total<T: Timer + ?Sized>(timer: &T) {
        timer.arm_oneshot(1);
        timer.arm_oneshot(0);
        timer.arm_oneshot(u64::MAX);
        timer.disarm();
        // Disarming twice is still a no-op.
        timer.disarm();
    }

    /// A freshly observed handle reports no callback and a dispatch on it
    /// is a harmless no-op that runs nothing.
    fn no_callback_dispatches_harmlessly<T: Timer + ?Sized>(timer: &T) {
        // The suite installs a callback below; this branch only holds
        // before any install, which a port's handle guarantees by
        // construction. A port that pre-installs a callback would report
        // `Some` here, which is itself a faithful answer — so we only
        // assert the dispatch is harmless, not that the slot is empty.
        if timer.tick_callback().is_none() {
            assert!(
                !timer.dispatch_tick(0),
                "dispatching with no callback installed must run nothing"
            );
        }
    }

    /// Installing a callback round-trips, and dispatching a tick invokes
    /// it with the CPU it was handed.
    fn installed_callback_fires_on_dispatch<T: Timer + ?Sized>(timer: &T) {
        TICKS.store(0, Ordering::Relaxed);
        LAST_CPU.store(u32::MAX, Ordering::Relaxed);

        timer.set_tick_callback(probe_tick);
        let installed: TickFn = timer.tick_callback().expect("callback must round-trip");
        // Cast `probe_tick` through its `TickFn` pointer type before the
        // integer cast: a direct function-item → integer cast trips the
        // `function_casts_as_integer` lint.
        let probe_addr = probe_tick as TickFn as usize;
        assert_eq!(
            installed as usize, probe_addr,
            "the installed callback must round-trip unchanged"
        );

        for cpu in [0u32, 1, 7] {
            let before = TICKS.load(Ordering::Relaxed);
            assert!(
                timer.dispatch_tick(cpu),
                "dispatch must run the installed callback (cpu {cpu})"
            );
            assert_eq!(
                TICKS.load(Ordering::Relaxed),
                before + 1,
                "dispatch must invoke the callback exactly once (cpu {cpu})"
            );
            assert_eq!(
                LAST_CPU.load(Ordering::Relaxed),
                cpu,
                "the callback must receive the dispatched CPU (cpu {cpu})"
            );
        }
    }

    #[cfg(test)]
    mod tests {
        use super::super::{TickFn, Timer};
        use super::run_all;
        use crate::CpuId;
        use core::sync::atomic::{AtomicUsize, Ordering};

        /// A faithful host double: an in-handle callback slot standing in
        /// for the port's static, with the shared invoke-on-dispatch
        /// logic.
        #[derive(Default)]
        struct CellTimer {
            callback: AtomicUsize,
            /// Last armed deadline (`u64::MAX` sentinel = disarmed), so a
            /// local unit test can assert arm/disarm round-trips. The
            /// generic `run_all` only proves the calls are total.
            armed: core::sync::atomic::AtomicU64,
        }

        impl Timer for CellTimer {
            fn set_tick_callback(&self, callback: TickFn) {
                self.callback.store(callback as usize, Ordering::Relaxed);
            }
            fn tick_callback(&self) -> Option<TickFn> {
                let raw = self.callback.load(Ordering::Relaxed);
                if raw == 0 {
                    None
                } else {
                    // SAFETY: every store is the round-trip of a valid
                    // `TickFn` pointer through `set_tick_callback`.
                    Some(unsafe { core::mem::transmute::<usize, TickFn>(raw) })
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
                // Mirror the port clamp so the recorded deadline is the
                // one the hardware would see.
                self.armed.store(ticks_from_now.max(1), Ordering::Relaxed);
            }
            fn disarm(&self) {
                self.armed.store(u64::MAX, Ordering::Relaxed);
            }
        }

        #[test]
        fn suite_accepts_a_faithful_timer() {
            let timer = CellTimer::default();
            run_all(&timer);
            let dynamic: &dyn Timer = &timer;
            run_all(dynamic);
        }

        #[test]
        fn arm_records_a_clamped_deadline_and_disarm_clears_it() {
            let timer = CellTimer::default();
            timer.arm_oneshot(42);
            assert_eq!(timer.armed.load(Ordering::Relaxed), 42);
            // A zero deadline is clamped to one tick so the CPU cannot
            // re-trap with no progress.
            timer.arm_oneshot(0);
            assert_eq!(timer.armed.load(Ordering::Relaxed), 1);
            timer.disarm();
            assert_eq!(timer.armed.load(Ordering::Relaxed), u64::MAX);
        }

        /// A broken timer that never invokes the callback must be
        /// rejected by the fires-on-dispatch check.
        struct DeadTimer {
            callback: AtomicUsize,
        }

        impl Timer for DeadTimer {
            fn set_tick_callback(&self, callback: TickFn) {
                self.callback.store(callback as usize, Ordering::Relaxed);
            }
            fn tick_callback(&self) -> Option<TickFn> {
                let raw = self.callback.load(Ordering::Relaxed);
                if raw == 0 {
                    None
                } else {
                    // SAFETY: as in `CellTimer`.
                    Some(unsafe { core::mem::transmute::<usize, TickFn>(raw) })
                }
            }
            fn dispatch_tick(&self, _cpu: CpuId) -> bool {
                // Faithful about the no-callback case (so it clears the
                // harmless-dispatch check), but broken about firing: once
                // a callback is installed it claims success yet never
                // runs it.
                self.tick_callback().is_some()
            }
            fn arm_oneshot(&self, _ticks_from_now: u64) {}
            fn disarm(&self) {}
        }

        #[test]
        #[should_panic(expected = "must invoke the callback exactly once")]
        fn suite_rejects_a_timer_that_never_fires() {
            run_all(&DeadTimer {
                callback: AtomicUsize::new(0),
            });
        }
    }
}
