//! Timer-programming surface of the Arch HAL (`AGENTS.md` §17.2
//! "timer programming").
//!
//! RustOS is a **tickless (`NO_HZ`)** kernel (`AGENTS.md` §17.1): the timer
//! is armed *one-shot*, to the next event the scheduler needs, never at a
//! fixed frequency. Each port owns a per-CPU timer — the x86_64 LAPIC
//! timer, the aarch64 EL1 generic timer, the riscv64 supervisor (SBI)
//! timer, and the wasm32 cooperative `requestAnimationFrame` loop. The
//! hardware that produces the interrupt is wildly different per target,
//! but the kernel needs the *same* two things from each: install the one
//! architecture-neutral scheduler-tick callback, and, on every fire,
//! invoke that callback with the firing CPU's [`crate::CpuId`].
//!
//! The one-shot *arming* (programming the LAPIC deadline, `CNTP_TVAL_EL0`,
//! the SBI timer, or requesting the next animation frame) stays in the
//! port; this surface owns only the callback slot and the dispatch. The
//! production preemption path landed in PLAN P-1 currently re-arms a
//! *fixed-frequency periodic* interval — a §17.1 defect being migrated to
//! the one-shot form under PLAN P-4. §17.2 makes the architecture
//! surface a closed set of traits on the HAL; this module is the "timer
//! programming" member of that set, so the callback install/dispatch
//! lives behind one vocabulary instead of being re-described per port
//! (`AGENTS.md` §2.2). The parallel per-arch implementations of this one
//! trait are the deliberate shape of §17.1/§17.2 modularity, never
//! collapsed behind `cfg` (§2.2 carve-out).
//!
//! # What lives here
//!
//! * [`TickFn`] — the scheduler-tick callback type. A plain
//!   `extern "C" fn(CpuId)` (not a closure) so it is safe to invoke from
//!   interrupt/trap context: there is no captured environment that could
//!   be dropped mid-flight.
//! * [`Timer`] — the per-port handle the kernel reaches through. It
//!   installs the tick callback ([`Timer::set_tick_callback`]) and, from
//!   the port's interrupt/frame path, dispatches a tick
//!   ([`Timer::dispatch_tick`]) which invokes the installed callback. The
//!   *hardware* arming/re-arming (programming the LAPIC LVT,
//!   `CNTP_TVAL_EL0`, the SBI timer, or requesting the next animation
//!   frame) stays in the port — it is per-CPU register/MMIO work with no
//!   architecture-neutral shape, and folding it into this surface would
//!   be interface creep (`AGENTS.md` §2.4). What *is* shared is the
//!   callback slot and the dispatch, which is what this trait owns.
//! * [`conformance`] — the §17.2 conformance vertical: a host-run
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

/// The scheduler-tick callback a [`Timer`] invokes on every tick.
///
/// `extern "C" fn(CpuId)` rather than a closure: the port stores it in a
/// lock-free slot and invokes it from interrupt/trap/frame context, so it
/// must have no captured environment to drop mid-flight (`AGENTS.md`
/// §2.1). The argument is the firing CPU's [`CpuId`] — the value the
/// scheduler needs to advance the right run queue without re-deriving it
/// on every tick.
pub type TickFn = extern "C" fn(CpuId);

/// The timer-programming handle an architecture port exposes
/// (`AGENTS.md` §17.2).
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
    /// interrupt/frame handler. The port's handler calls it and then
    /// re-arms its hardware timer; a tick that arrives before any
    /// callback is installed dispatches harmlessly (returns `false`),
    /// never a panic (`AGENTS.md` §2.9).
    fn dispatch_tick(&self, cpu: CpuId) -> bool;
}

/// The §17.2 timer-programming conformance vertical.
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
    /// reports one, if the installed callback does not round-trip, or if
    /// dispatching a tick does not invoke the callback with the CPU it
    /// was handed.
    pub fn run_all<T: Timer + ?Sized>(timer: &T) {
        no_callback_dispatches_harmlessly(timer);
        installed_callback_fires_on_dispatch(timer);
    }

    /// A freshly observed handle reports no callback and a dispatch on it
    /// is a harmless no-op that runs nothing (`AGENTS.md` §2.9).
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
        }

        #[test]
        fn suite_accepts_a_faithful_timer() {
            let timer = CellTimer::default();
            run_all(&timer);
            let dynamic: &dyn Timer = &timer;
            run_all(dynamic);
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
