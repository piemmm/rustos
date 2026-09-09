//! Host tests for the frequency subsystem: what the governor asks for as a
//! machine idles, wakes, loads, and launches, and that the live-clock
//! estimator reports a frequency rather than a duty cycle.
//!
//! Both halves keep process-global state — one binding, one installed
//! core-clock source, and the per-CPU slots — so every test that touches it
//! runs under [`with_mechanism`], which serialises and tears down. The clock
//! is scripted rather than read, so nothing here depends on wall time.

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use tairix_abi::cpufreq::CpuFreqLimits;
use tairix_abi::Errno;
use tairix_arch_api::{CoreClock, CoreClockSupport};
use tairix_kernel_sec::ProcessId;

use super::domain::{self, TargetWaiter};
use super::governor::{MAX_HOLD_NS, RESPONSE_WINDOW_NS, UTIL_ONE};
use super::{estimate, note_active, note_idle};
use crate::cpu_state;

/// A Pi 4B's range: 600 MHz to 1.5 GHz in 100 MHz steps.
fn pi4() -> CpuFreqLimits {
    CpuFreqLimits::new(600_000_000, 1_500_000_000, 100_000_000).expect("a real range")
}

/// The mechanism driver in these tests.
const DRIVER: ProcessId = ProcessId(0x0C_9F);

/// Run `body` with `limits` bound to [`DRIVER`] at time `now_ns`, holding the
/// process-global mechanism state for the duration.
///
/// The shared guard serialises the machine-global state and releases the
/// binding however `body` ends; this only names [`DRIVER`] for it.
fn with_mechanism<R>(limits: CpuFreqLimits, now_ns: u64, body: impl FnOnce(u64) -> R) -> R {
    super::with_bound_mechanism(DRIVER, limits, now_ns, body)
}

/// A [`TargetWaiter`] over a scripted clock, standing in for the timed sweep
/// that releases the real waiter.
///
/// A park on a finite deadline advances the clock to it, so a test walks the
/// governor's own pacing rather than guessing at it. A park with **no**
/// deadline is where a real machine sleeps until work arrives, so there is
/// nothing for a test to advance to: the waiter records that it happened and
/// unwinds the wait with [`Errno::Interrupted`], which is what
/// [`settled`] reads as "the governor has nothing further to say".
struct ScriptedWaiter {
    now_ns: AtomicU64,
    /// Set once a park carried no deadline.
    settled: AtomicBool,
}

impl ScriptedWaiter {
    fn at(now_ns: u64) -> Self {
        Self {
            now_ns: AtomicU64::new(now_ns),
            settled: AtomicBool::new(false),
        }
    }

    /// Whether the governor ever parked this waiter with no deadline — a
    /// machine that will take no wakeup until real work arrives.
    fn settled(&self) -> bool {
        self.settled.load(Ordering::Relaxed)
    }
}

impl TargetWaiter for ScriptedWaiter {
    fn now_ns(&self) -> u64 {
        self.now_ns.load(Ordering::Relaxed)
    }

    fn park(&self, deadline_ns: u64) {
        if deadline_ns == crate::waitq::NO_DEADLINE {
            self.settled.store(true, Ordering::Relaxed);
        } else {
            self.now_ns.store(deadline_ns, Ordering::Relaxed);
        }
    }

    fn kill_pending(&self) -> bool {
        self.settled()
    }
}

/// A [`TargetWaiter`] whose task has a termination pending.
struct DoomedWaiter;

impl TargetWaiter for DoomedWaiter {
    fn now_ns(&self) -> u64 {
        0
    }
    fn park(&self, _deadline_ns: u64) {}
    fn kill_pending(&self) -> bool {
        true
    }
}

/// Mark every test CPU idle as of `now_ns`, so a test states the machine it
/// means rather than inheriting one.
fn all_cpus_idle(now_ns: u64) {
    for cpu in 0..u32::try_from(cpu_state::TEST_CPUS).expect("test CPU count fits") {
        note_idle(cpu, now_ns);
    }
}

// --- The binding ----------------------------------------------------------

#[test]
fn a_second_mechanism_is_refused_rather_than_arbitrated() {
    with_mechanism(pi4(), 1_000, |_handle| {
        assert_eq!(
            domain::bind(ProcessId(0x0C_A0), pi4(), 1_000),
            Err(Errno::AlreadyExists),
            "two drivers on one clock is worse than one"
        );
    });
}

#[test]
fn a_dead_driver_frees_the_role_for_its_replacement() {
    let first = with_mechanism(pi4(), 1_000, |handle| handle);
    // `with_mechanism` released it, exactly as the task-reclaim path does.
    let second = with_mechanism(pi4(), 2_000, |handle| handle);
    assert_ne!(
        first, second,
        "a handle held over from the dead binding must not name the new one"
    );
}

#[test]
fn releasing_another_processs_binding_does_nothing() {
    with_mechanism(pi4(), 1_000, |handle| {
        assert!(
            !domain::release_process(ProcessId(0x0C_A1)),
            "a process that never bound cannot release the role"
        );
        // The binding is still live and still usable.
        let waiter = ScriptedWaiter::at(1_000);
        assert!(domain::wait(DRIVER, handle, 0, &waiter).is_ok());
    });
}

#[test]
fn a_wait_on_a_foreign_or_forged_handle_fails_closed() {
    with_mechanism(pi4(), 1_000, |handle| {
        let waiter = ScriptedWaiter::at(1_000);
        assert_eq!(
            domain::wait(DRIVER, handle.wrapping_add(1), 0, &waiter),
            Err(Errno::NotFound),
            "a handle the binding did not issue"
        );
        assert_eq!(
            domain::wait(ProcessId(0x0C_A2), handle, 0, &waiter),
            Err(Errno::NotFound),
            "a process the binding is not held by"
        );
    });
}

#[test]
fn a_wait_with_no_mechanism_bound_fails_closed() {
    // Deliberately outside `with_mechanism`: nothing is bound, so there is no
    // published target to observe and none is invented.
    let waiter = ScriptedWaiter::at(1_000);
    assert_eq!(
        domain::wait(ProcessId(0x0C_A3), 1, 0, &waiter),
        Err(Errno::NotFound)
    );
}

#[test]
fn a_terminated_mechanism_unwinds_its_wait() {
    with_mechanism(pi4(), 1_000, |handle| {
        // The first wait answers immediately (the bind boost is a change), so
        // observe that sequence and then wait again on a machine that has
        // nothing new to say.
        let waiter = ScriptedWaiter::at(1_000);
        let first = domain::wait(DRIVER, handle, 0, &waiter).expect("the bind boost");
        assert_eq!(
            domain::wait(DRIVER, handle, first.seq, &DoomedWaiter),
            Err(Errno::Interrupted)
        );
    });
}

// --- What the governor asks for -------------------------------------------

#[test]
fn the_first_target_after_bind_is_full_speed_not_the_minimum() {
    // The defect this guards: a driver binding on a demonstrably busy machine
    // — it was just launched — would otherwise find every filter dated time
    // zero, read the whole boot as idle, and ask the firmware for the
    // *minimum*. The bind boost is what makes the first move the right one.
    let limits = pi4();
    with_mechanism(limits, 1_000_000, |handle| {
        let waiter = ScriptedWaiter::at(1_000_000);
        let target = domain::wait(DRIVER, handle, 0, &waiter).expect("a first target");
        assert_eq!(target.target_hz, limits.max_hz);
        assert_eq!(target.seq, 1, "the first publication advances the sequence");
    });
}

/// Walk a quiescing machine down until the governor settles, returning the
/// last target it asked for and the sequence it was published at.
///
/// The caller must have marked the machine idle first ([`all_cpus_idle`]):
/// while anything is running the governor keeps looking, and rightly never
/// settles.
///
/// Each wait parks to the deadline the governor itself chose, so this follows
/// the governor's own pacing rather than a cadence the test invented. It stops
/// when the governor parks with no deadline — the machine will take no wakeup
/// until real work arrives — which the waiter surfaces as
/// [`Errno::Interrupted`].
fn settle(handle: u64, waiter: &ScriptedWaiter, limits: &CpuFreqLimits) -> (u64, u64) {
    let mut seq = 0;
    let mut target = 0;
    loop {
        match domain::wait(DRIVER, handle, seq, waiter) {
            Ok(observed) => {
                if target != 0 {
                    assert!(
                        observed.target_hz <= target,
                        "a quiescing machine must not speed up: {} after {}",
                        observed.target_hz,
                        target
                    );
                }
                target = observed.target_hz;
                seq = observed.seq;
            }
            Err(Errno::Interrupted) => break,
            Err(err) => panic!("the binding must stay live: {err:?}"),
        }
        assert!(
            target >= limits.min_hz,
            "a target below the floor: {target}"
        );
    }
    (target, seq)
}

#[test]
fn a_quiet_machine_settles_at_the_minimum_and_then_stops_waking() {
    let limits = pi4();
    let start = 1_000_000;
    with_mechanism(limits, start, |handle| {
        all_cpus_idle(start);
        let waiter = ScriptedWaiter::at(start);
        let (target, _) = settle(handle, &waiter, &limits);
        assert_eq!(
            target, limits.min_hz,
            "an idle machine must reach the minimum"
        );
        assert!(
            waiter.settled(),
            "settled at the minimum, the governor must arm no further wakeup"
        );
    });
}

#[test]
fn sustained_work_climbs_to_full_speed() {
    // Work that keeps running produces no transition to observe, so the rate
    // has to rise from the waiter looking again on its own cadence. It must
    // reach the ceiling, and get there within about the window the filter
    // measures over.
    let limits = pi4();
    let start = 1_000_000;
    with_mechanism(limits, start, |handle| {
        all_cpus_idle(start);
        let waiter = ScriptedWaiter::at(start);
        let (settled_at, mut seq) = settle(handle, &waiter, &limits);
        assert_eq!(settled_at, limits.min_hz);

        // One CPU picks up work and keeps it.
        let began = waiter.now_ns() + 1_000;
        note_active(3, began);
        let busy = ScriptedWaiter::at(began);
        let mut target = limits.min_hz;
        let mut climbed = false;
        for _ in 0..16 {
            match domain::wait(DRIVER, handle, seq, &busy) {
                Ok(observed) => {
                    assert!(
                        observed.target_hz >= target,
                        "a busy machine must not slow down: {} after {}",
                        observed.target_hz,
                        target
                    );
                    target = observed.target_hz;
                    seq = observed.seq;
                }
                Err(err) => panic!("a busy machine must keep looking: {err:?}"),
            }
            if target == limits.max_hz {
                climbed = true;
                break;
            }
        }
        assert!(
            climbed,
            "sustained work never reached the ceiling: {target}"
        );
        assert!(
            busy.now_ns() - began <= RESPONSE_WINDOW_NS * 2,
            "the climb took longer than the filter's own window"
        );
    });
}

/// Drive a busy machine's rate upward until it stops climbing, returning the
/// last target and its sequence.
///
/// Stops at the ceiling, because a rate that has arrived there stops changing
/// and a further wait would never return. Any refusal is a failure: a machine
/// with work running must keep being looked at.
fn climb(handle: u64, waiter: &ScriptedWaiter, limits: &CpuFreqLimits) -> (u64, u64) {
    let mut seq = 0;
    let mut target = 0;
    for _ in 0..64 {
        let observed = domain::wait(DRIVER, handle, seq, waiter)
            .expect("the governor must keep looking at a machine with work running");
        assert!(
            observed.target_hz >= target,
            "a busy machine must not slow down: {} after {}",
            observed.target_hz,
            target
        );
        target = observed.target_hz;
        seq = observed.seq;
        if target == limits.max_hz {
            break;
        }
    }
    (target, seq)
}

#[test]
fn work_that_resumes_inside_the_attention_span_is_still_noticed() {
    // The hole this guards. A wake inside the attention span is deliberately
    // not flagged, because the waiter is expected to look again by itself. So
    // if the waiter were allowed to park indefinitely while that span stands,
    // a CPU that resumed just after a survey found the machine quiet would
    // have no edge left to announce it — and a machine that then stayed busy
    // would sit at the floor for ever.
    let limits = pi4();
    let start = 1_000_000;
    with_mechanism(limits, start, |handle| {
        all_cpus_idle(start);
        let quiet = ScriptedWaiter::at(start);
        let (settled_at, _) = settle(handle, &quiet, &limits);
        assert_eq!(settled_at, limits.min_hz);

        // A blip: one CPU wakes and parks again at once, stamping an
        // attention span that will suppress the next wake's flag.
        let blip = quiet.now_ns() + 1_000;
        note_active(5, blip);
        note_idle(5, blip + 1_000);

        // Real work starts inside that span and never stops, so no further
        // edge will announce it.
        note_active(5, blip + 2_000);
        let busy = ScriptedWaiter::at(blip + 2_000);
        let (target, _) = climb(handle, &busy, &limits);
        assert_eq!(
            target, limits.max_hz,
            "sustained work inside the attention span never raised the rate"
        );
    });
}

#[test]
fn a_machine_past_half_busy_is_given_the_whole_range() {
    // The proportional rate with its headroom only reaches the ceiling at
    // four fifths of a core, which leaves a genuinely busy machine climbing
    // through rates it will not stay at.
    let limits = pi4();
    let start = 1_000_000;
    with_mechanism(limits, start, |handle| {
        all_cpus_idle(start);
        let quiet = ScriptedWaiter::at(start);
        let (_, seq) = settle(handle, &quiet, &limits);

        // Hand one CPU a filter just past half busy, dated now, and let the
        // bind boost lapse so utilisation alone decides.
        let now = quiet.now_ns() + RESPONSE_WINDOW_NS;
        let state = cpu_state::get(2).expect("a test CPU");
        state.gov_util.store(UTIL_ONE / 2 + 1, Ordering::Relaxed);
        state.gov_folded_ns.store(now, Ordering::Relaxed);
        let waiter = ScriptedWaiter::at(now);
        assert_eq!(
            domain::wait(DRIVER, handle, seq, &waiter)
                .expect("a target")
                .target_hz,
            limits.max_hz,
            "past half a core the whole range is asked for"
        );
    });
}

#[test]
fn the_ceiling_is_held_before_it_is_given_up() {
    // Reaching the top and dropping straight off it again costs a mechanism
    // round trip each way and serves the part of the burst that mattered at
    // the lower rate. So once asked for, the ceiling is kept for a while
    // whatever the machine does next.
    //
    // The wait itself is the observation: it returns only when the published
    // rate actually changes, so the waiter's clock at the first reduction is
    // when the reduction happened. Its park advances that clock to whatever
    // deadline the governor chose, so nothing here invents a cadence.
    let limits = pi4();
    let start = 1_000_000;
    with_mechanism(limits, start, |handle| {
        let waiter = ScriptedWaiter::at(start);
        let seq = domain::wait(DRIVER, handle, 0, &waiter)
            .expect("the bind boost puts the machine at the ceiling")
            .seq;
        // Nothing runs from here on, so utilisation alone would give the
        // ceiling up as soon as the boost lapsed.
        all_cpus_idle(start);

        let dropped = domain::wait(DRIVER, handle, seq, &waiter).expect("a reduction");
        assert!(
            dropped.target_hz < limits.max_hz,
            "the rate never came off the ceiling"
        );
        assert!(
            waiter.now_ns() - start >= MAX_HOLD_NS,
            "the ceiling was given up after only {} ns",
            waiter.now_ns() - start
        );
    });
}

#[test]
fn the_hold_never_delays_a_rise() {
    // The hold exists to stop the rate flapping off the top, not to slow it
    // reaching the top: a machine that gets busy while a hold stands on a
    // *lower* rate must not be made to wait.
    let limits = pi4();
    let start = 1_000_000;
    with_mechanism(limits, start, |handle| {
        all_cpus_idle(start);
        let quiet = ScriptedWaiter::at(start);
        let (settled_at, seq) = settle(handle, &quiet, &limits);
        assert_eq!(settled_at, limits.min_hz);

        let busy_at = quiet.now_ns() + 1_000;
        note_active(4, busy_at);
        let busy = ScriptedWaiter::at(busy_at);
        let (target, _) = climb(handle, &busy, &limits);
        assert_eq!(target, limits.max_hz);
        assert!(
            busy.now_ns() - busy_at < MAX_HOLD_NS,
            "the climb waited out a hold it should never have seen"
        );
        let _ = seq;
    });
}

#[test]
fn a_low_duty_wake_pattern_does_not_pin_the_ceiling() {
    // The reported defect. An idle desktop showing a live monitor wakes a CPU
    // many times a second to do almost nothing, and used to sit at its
    // ceiling for ever: every wake re-extended a boost that granted the
    // maximum outright, so the measured utilisation never got a say. Leaving
    // idle now grants no rate at all.
    let limits = pi4();
    let mut now = 1_000_000;
    with_mechanism(limits, now, |handle| {
        all_cpus_idle(now);
        // Twenty wakes a second, each a millisecond of work: about 2% of one
        // core, and far more often than a boost window would have lapsed.
        for _ in 0..40 {
            note_active(1, now);
            now += 1_000_000;
            note_idle(1, now);
            now += 49_000_000;
        }
        let waiter = ScriptedWaiter::at(now);
        let (target, _) = settle(handle, &waiter, &limits);
        assert_eq!(
            target, limits.min_hz,
            "a 2%-busy machine settled at {target} rather than the floor"
        );
    });
}

#[test]
fn a_program_launch_is_served_at_full_speed_from_an_idle_machine() {
    // A launch spends most of its time waiting on the volume, so every CPU
    // can be idle throughout and utilisation would never notice it.
    let limits = pi4();
    let start = 1_000_000;
    with_mechanism(limits, start, |handle| {
        all_cpus_idle(start);
        let waiter = ScriptedWaiter::at(start);
        let (settled_at, seq) = settle(handle, &waiter, &limits);
        assert_eq!(settled_at, limits.min_hz);

        let launched_at = waiter.now_ns() + 1_000;
        domain::note_launch(launched_at);
        let launching = ScriptedWaiter::at(launched_at);
        assert_eq!(
            domain::wait(DRIVER, handle, seq, &launching)
                .expect("a target")
                .target_hz,
            limits.max_hz
        );
    });
}

#[test]
fn sustained_partial_load_settles_below_full_speed() {
    // The proportional half: a CPU busy a fifth of the time must not hold the
    // whole machine at its ceiling once the boost has lapsed.
    let limits = pi4();
    let mut now = 1_000_000;
    with_mechanism(limits, now, |handle| {
        all_cpus_idle(now);
        // Twenty windows of a 20% duty cycle on one CPU.
        for _ in 0..20 {
            for _ in 0..10 {
                note_active(1, now);
                now += RESPONSE_WINDOW_NS / 50; // 2 ms busy
                note_idle(1, now);
                now += RESPONSE_WINDOW_NS / 10 - RESPONSE_WINDOW_NS / 50; // 8 ms idle
            }
        }
        // Let the last wake's boost lapse, then ask once — the target the
        // governor publishes here is the one utilisation alone justifies.
        now += RESPONSE_WINDOW_NS + 1;
        let waiter = ScriptedWaiter::at(now);
        let target = domain::wait(DRIVER, handle, 0, &waiter)
            .expect("a target")
            .target_hz;
        assert!(
            target < limits.max_hz,
            "a fifth-busy machine asked for its ceiling: {target}"
        );
        assert!(
            target >= limits.min_hz,
            "a busy machine must not drop below the floor: {target}"
        );
    });
}

#[test]
fn an_unbound_machine_does_no_governor_accounting() {
    // Bound to no mechanism but still holding the mechanism lock: the hooks
    // run on every dispatch step of every CPU on every port, so with no
    // frequency driver bound they must do none of the governor's work —
    // nothing folded, no edge stamped, no demand raised. Without the lock a
    // sibling's *bound* machine is what these hooks would see, and the
    // accounting they then do is not this test's to observe.
    super::with_mechanism_lock(|| {
        let state = cpu_state::get(7).expect("a test CPU");
        state.cpu_active_since.store(0, Ordering::Relaxed);
        state.gov_util.store(0, Ordering::Relaxed);
        state.gov_folded_ns.store(0, Ordering::Relaxed);
        note_active(7, 5_000_000);
        note_idle(7, 9_000_000);
        domain::note_launch(9_000_000);
        assert_eq!(state.gov_util.load(Ordering::Relaxed), 0, "nothing folded");
        assert_eq!(
            state.gov_folded_ns.load(Ordering::Relaxed),
            0,
            "the filter's clock never started"
        );
        assert_eq!(
            state.cpu_active_since.load(Ordering::Relaxed),
            0,
            "no edge stamped for a governor that is not running"
        );
    });
}

#[test]
fn back_to_back_dispatches_are_one_busy_span() {
    // `note_active` runs after every dispatch that ran a task body, so a CPU
    // working steadily reaches it thousands of times a second. Only the edge
    // may do anything: re-stamping per dispatch would chop one busy span into
    // arbitrary slices and hand the filter's non-composability a workload it
    // was never meant to measure.
    let cpu = 9;
    with_mechanism(pi4(), 1_000, |_handle| {
        let state = cpu_state::get(cpu).expect("a test CPU");
        note_active(cpu, 1_000);
        let stamped = state.cpu_active_since.load(Ordering::Relaxed);
        assert_ne!(stamped, 0, "the edge must be recorded");
        for step in 1..8u64 {
            note_active(cpu, 1_000 + step * 1_000);
            assert_eq!(
                state.cpu_active_since.load(Ordering::Relaxed),
                stamped,
                "dispatch step {step} restarted the busy span"
            );
        }
        note_idle(cpu, 100_000);
        assert_eq!(state.cpu_active_since.load(Ordering::Relaxed), 0);
    });
}

#[test]
fn an_out_of_range_cpu_is_dropped_rather_than_indexed() {
    with_mechanism(pi4(), 1_000, |_handle| {
        // The hooks take a CPU id from the dispatch loop; one outside the
        // installed set fails closed rather than reaching past the slots.
        note_active(u32::MAX, 2_000);
        note_idle(u32::MAX, 3_000);
    });
}

// --- The live-clock estimator ---------------------------------------------

/// A [`CoreClock`] whose two counters a test advances by hand: `core`
/// standing in for a counter gated while the PE idles, `reference` for one
/// that is not.
struct ScriptedCoreClock {
    core: AtomicU64,
    reference: AtomicU64,
}

/// The installed source. `estimate::install` is set-once for the life of the
/// process, so the estimator tests share this one and script it.
///
/// Both counters start well clear of zero, because a real one has been running
/// since reset and the estimator reads a stored reference of zero as "no prior
/// sample". A clock scripted from zero would hand the first seeding sample
/// that sentinel back, so the sample after it would discard too — and a test
/// would then assert against a figure that was never measured.
static SCRIPTED_CLOCK: ScriptedCoreClock = ScriptedCoreClock {
    core: AtomicU64::new(1_000_000_000),
    reference: AtomicU64::new(1_000_000_000),
};

/// The scripted reference rate: 1 GHz, so a reference tick is a nanosecond
/// and a core-to-reference ratio reads directly as a frequency.
const SCRIPTED_REFERENCE_HZ: u64 = 1_000_000_000;

/// Serialise the estimator tests: they share the one installed
/// [`SCRIPTED_CLOCK`], whose counters each of them advances by hand, so two
/// running at once would measure each other's trace.
fn with_scripted_clock<R>(body: impl FnOnce() -> R) -> R {
    static CLOCK_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = CLOCK_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    estimate::install(&SCRIPTED_CLOCK);
    assert!(
        estimate::is_supported(),
        "the scripted source must be the installed one"
    );
    body()
}

impl CoreClock for ScriptedCoreClock {
    fn enable(&self) {}
    fn core_cycles(&self) -> u64 {
        self.core.load(Ordering::Relaxed)
    }
    fn reference_cycles(&self) -> u64 {
        self.reference.load(Ordering::Relaxed)
    }
    fn reference_hz(&self) -> u64 {
        SCRIPTED_REFERENCE_HZ
    }
    fn support(&self) -> CoreClockSupport {
        CoreClockSupport::Supported
    }
}

impl ScriptedCoreClock {
    /// Advance both counters over a span in which the core ran at `hz`.
    fn run(&self, span_ns: u64, hz: u64) {
        self.core
            .fetch_add(span_ns * hz / SCRIPTED_REFERENCE_HZ, Ordering::Relaxed);
        self.reference.fetch_add(span_ns, Ordering::Relaxed);
    }

    /// Advance only the reference counter, as an idle park does on a port
    /// whose core-cycle counter is gated with the PE clock.
    fn park(&self, span_ns: u64) {
        self.reference.fetch_add(span_ns, Ordering::Relaxed);
    }
}

#[test]
fn an_idle_park_does_not_turn_the_measured_frequency_into_a_duty_cycle() {
    // The reported defect's second half. `PMCCNTR_EL0` (aarch64) and the
    // `cycle` CSR (riscv64) stop while the PE sits in `wfi` while their
    // reference counters keep running, so a sampling window straddling a park
    // divides running cycles by running-plus-idle ticks. A core flat out at
    // 1.5 GHz for 40% of a window then reads as 600 MHz — the exact figure
    // the Switchboard was showing — which is a different quantity wearing the
    // same units.
    with_scripted_clock(|| {
        let cpu = 11;
        let state = cpu_state::get(cpu).expect("a test CPU");

        // Seed a baseline, then run one window that is 40% busy at 1.5 GHz.
        estimate::sample(cpu);
        let full_speed = 1_500_000_000;
        SCRIPTED_CLOCK.run(4_000_000, full_speed);
        SCRIPTED_CLOCK.park(6_000_000);
        SCRIPTED_CLOCK.run(1_000_000, full_speed);
        estimate::sample(cpu);
        let contaminated = state.freq_hz.load(Ordering::Relaxed);
        assert!(
            contaminated < full_speed / 2,
            "the unbracketed window should read low, not {contaminated}"
        );

        // Now the same trace with the park bracketed, as the dispatch loop does:
        // the baseline is discarded on the way in and moves to the resumption, so
        // the window measures running time only.
        SCRIPTED_CLOCK.run(1_000_000, full_speed);
        estimate::sample(cpu);
        estimate::invalidate(cpu);
        SCRIPTED_CLOCK.park(6_000_000);
        estimate::rebase(cpu);
        SCRIPTED_CLOCK.run(4_000_000, full_speed);
        estimate::sample(cpu);
        assert_eq!(
            state.freq_hz.load(Ordering::Relaxed),
            full_speed,
            "a bracketed window must report the frequency the core ran at"
        );
    });
}

#[test]
fn the_interrupt_that_ends_a_park_cannot_publish_a_duty_cycle() {
    // The half of the park bracket a resumption hook cannot cover. Ports call
    // `note_preempt_tick` — and so `sample` — from the timer one-shot *and*
    // from a device interrupt that requested a wake, which is exactly the
    // interrupt that ends an idle park. It runs in interrupt context, before
    // the dispatch loop regains control, so re-seeding on resumption is too
    // late: without the invalidation on the way in, that first sample divides
    // pre-park running cycles by running-plus-parked reference ticks and
    // publishes a duty cycle wearing hertz — which is what made an idle core
    // look as though its clock had been lowered.
    with_scripted_clock(|| {
        let cpu = 12;
        let state = cpu_state::get(cpu).expect("a test CPU");
        let full_speed = 1_500_000_000;

        // A window of real work, measured: the figure a reader would see.
        estimate::sample(cpu);
        SCRIPTED_CLOCK.run(4_000_000, full_speed);
        estimate::sample(cpu);
        assert_eq!(state.freq_hz.load(Ordering::Relaxed), full_speed);

        // Now: a brief run, the park, and the waking interrupt's own sample —
        // ordered as the machine orders them, with nothing between the park and
        // the interrupt.
        SCRIPTED_CLOCK.run(20_000, full_speed);
        estimate::invalidate(cpu);
        SCRIPTED_CLOCK.park(50_000_000);
        estimate::sample(cpu);
        assert_eq!(
            state.freq_hz.load(Ordering::Relaxed),
            full_speed,
            "the waking interrupt's sample must re-seed, not publish"
        );

        // And the baseline it re-seeded is usable: the next window reports the
        // frequency the core ran at, with no measurement lost to the park.
        estimate::rebase(cpu);
        let lowered = 600_000_000;
        SCRIPTED_CLOCK.run(4_000_000, lowered);
        estimate::sample(cpu);
        assert_eq!(
            state.freq_hz.load(Ordering::Relaxed),
            lowered,
            "a real change of clock must be reported after a park"
        );
    });
}
