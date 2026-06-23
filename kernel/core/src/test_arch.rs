//! Host-only architecture mock implementing [`crate::KernelArch`].
//!
//! `TestArch` lets the host-side integration tests drive
//! [`crate::kernel_main`] and [`crate::handle_panic`] without a real
//! platform. It is gated behind `cfg(any(test, feature = "test-arch"))`
//! so production builds never link it — `AGENTS.md` §1 (no hacks: a
//! production kernel must not carry a fake `halt`/`current_cpu`).
//!
//! # Driving the `!` return type of `halt`
//!
//! Real arch ports implement [`crate::KernelArch::halt`] as an infinite
//! `hlt`/`wfi` loop. Tests cannot loop forever, so `TestArch::halt`
//! records the call in an internal counter and then invokes
//! [`core::panic!`] with a sentinel message. The integration tests wrap
//! the call site in [`std::panic::catch_unwind`] to observe the halt
//! without blocking the test thread. This is the same pattern Rust's
//! `std::process::abort` test harnesses use; it is permitted here
//! because the code only exists under `cfg(any(test, feature =
//! "test-arch"))` (`AGENTS.md` §2.9 — `panic!` allowed in tests).

extern crate std;

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use rustos_arch_api::{ContextSwitch, PrepareError, TaskContext, TaskEntry};

use crate::sched::{CpuId, SchedulerArch};

use crate::bootinfo::KernelArch;

/// Frame size the [`TestContextSwitch`] double reserves when seeding a
/// task's first frame — below the kthread test stacks but above the
/// 16-byte too-small probe, matching the `kernel/arch/api` conformance
/// double.
const TEST_FRAME_BYTES: u64 = 64;

/// Host-only [`ContextSwitch`] double backing [`TestArch`]'s
/// [`KernelArch::Cs`].
///
/// `prepare` honours the same fail-closed contract a real port owes (it
/// rejects a null, misaligned, or too-small `stack_top`) so the
/// user-kthread admission path can be exercised on the host; `switch` is
/// never reached under `cargo test` (the host never enters EL0), so its
/// body is empty — exactly the `kernel/arch/api` `conformance` precedent.
#[derive(Debug, Default, Clone, Copy)]
pub struct TestContextSwitch;

impl ContextSwitch for TestContextSwitch {
    fn prepare(
        &self,
        ctx: &mut TaskContext,
        stack_top: u64,
        _entry: TaskEntry,
        _arg: usize,
    ) -> Result<(), PrepareError> {
        if stack_top == 0 {
            return Err(PrepareError::NullStack);
        }
        if stack_top % 16 != 0 {
            return Err(PrepareError::Misaligned);
        }
        if stack_top < TEST_FRAME_BYTES {
            return Err(PrepareError::TooSmall);
        }
        ctx.stack_pointer = stack_top - TEST_FRAME_BYTES;
        Ok(())
    }

    unsafe fn switch(&self, _prev: *mut TaskContext, _next: *mut TaskContext) {
        // The host never switches into an EL0 task; the bare-metal ports'
        // QEMU verticals exercise the real switch (`AGENTS.md` §1 — no
        // fake primitive).
    }
}

/// Sentinel panic message produced by [`TestArch::halt`].
///
/// Tests assert on this exact string to confirm that the panic handler
/// or [`crate::kernel_main`] reached `halt`.
pub const HALT_SENTINEL: &str = "rustos-kernel-core: TestArch::halt called";

/// In-memory `KernelArch` implementation used by host-side tests.
///
/// The mock is intentionally minimal: it exposes one counter per
/// observable side effect (`halt` calls, IPIs per CPU) so tests assert
/// behaviour numerically instead of relying on flaky timing.
#[derive(Debug)]
pub struct TestArch {
    cpu_count: u32,
    current: AtomicU32,
    ticks: AtomicU64,
    halts: AtomicU64,
    ipis: AtomicU64,
    /// Number of [`KernelArch::pump_console_tx`] calls observed.
    ///
    /// The dispatch loop calls the buffered-console-transmit hook on every
    /// iteration (after each dispatch and before the idle park), so a test
    /// asserts numerically that the per-dispatch servicing reached the seam
    /// rather than relying on timing.
    pump_tx_calls: AtomicU64,
    /// Monotonic-ns counter backing [`KernelArch::monotonic_ns`].
    ///
    /// Each call increments the counter and returns the new value, so
    /// host tests of `clock_get` get a deterministic, strictly
    /// increasing reading without depending on wall-clock time.
    monotonic_ns: AtomicU64,
}

impl TestArch {
    /// Build a `TestArch` reporting `cpu_count` logical CPUs.
    ///
    /// Panics if `cpu_count == 0`, which is a test-only programming
    /// error — `AGENTS.md` §2.9 permits panics in tests.
    #[must_use]
    pub fn with_cpus(cpu_count: u32) -> Self {
        assert!(cpu_count > 0, "TestArch requires at least one CPU");
        Self {
            cpu_count,
            current: AtomicU32::new(0),
            ticks: AtomicU64::new(0),
            halts: AtomicU64::new(0),
            ipis: AtomicU64::new(0),
            pump_tx_calls: AtomicU64::new(0),
            monotonic_ns: AtomicU64::new(0),
        }
    }

    /// Override the "current CPU" reported by [`Self::current_cpu`].
    ///
    /// Tests use this to simulate panic handlers fired from a non-boot
    /// CPU.
    pub fn set_current_cpu(&self, cpu: CpuId) {
        assert!(cpu < self.cpu_count, "current cpu out of range");
        self.current.store(cpu, Ordering::Relaxed);
    }

    /// Number of times [`Self::halt`] was reached.
    ///
    /// Always either `0` (boot succeeded) or `1` (boot/panic halted)
    /// because `halt` panics on first call and cannot be re-entered.
    #[must_use]
    pub fn halt_count(&self) -> u64 {
        self.halts.load(Ordering::Relaxed)
    }

    /// Total IPIs the scheduler has requested through this arch.
    #[must_use]
    pub fn ipi_count(&self) -> u64 {
        self.ipis.load(Ordering::Relaxed)
    }

    /// Number of times [`KernelArch::pump_console_tx`] was reached.
    ///
    /// Lets a test assert the dispatch loop tops up the buffered console
    /// transmit on every iteration, not only when it idles.
    #[must_use]
    pub fn pump_console_tx_count(&self) -> u64 {
        self.pump_tx_calls.load(Ordering::Relaxed)
    }

    /// Stage the value the *next* [`KernelArch::monotonic_ns`] call
    /// returns.
    ///
    /// `monotonic_ns` increments before returning, so it yields
    /// `value + 1` on the next call. Tests use this to drive
    /// `clock_get` with a known raw reading (e.g. to assert the
    /// coarsening boundary).
    pub fn set_monotonic_ns(&self, value: u64) {
        self.monotonic_ns.store(value, Ordering::Relaxed);
    }
}

impl SchedulerArch for TestArch {
    fn current_cpu(&self) -> CpuId {
        self.current.load(Ordering::Relaxed)
    }

    fn ticks_now(&self) -> u64 {
        self.ticks.load(Ordering::Relaxed)
    }

    fn send_ipi(&self, _target: CpuId) {
        self.ipis.fetch_add(1, Ordering::Relaxed);
    }
}

impl KernelArch for TestArch {
    type Cs = TestContextSwitch;

    fn context_switch(&self) -> Self::Cs {
        TestContextSwitch
    }

    fn halt(&self) -> ! {
        self.halts.fetch_add(1, Ordering::Relaxed);
        // SAFETY-INVARIANT: `halt` must not return on production ports;
        // in tests we substitute `panic!` (which also has `!` return)
        // so the test harness can observe the halt via
        // `std::panic::catch_unwind` without blocking the runner.
        std::panic!("{HALT_SENTINEL}");
    }

    fn monotonic_ns(&self, _cpu: CpuId) -> u64 {
        // `fetch_add` returns the previous value; `+ 1` makes the
        // first call return `1` and every subsequent call return a
        // strictly larger value, satisfying the
        // "monotonically-non-decreasing" contract documented on
        // [`KernelArch::monotonic_ns`].
        self.monotonic_ns.fetch_add(1, Ordering::Relaxed) + 1
    }

    fn pump_console_tx(&self) {
        // The host mock has no buffered device; it only records that the
        // dispatch loop reached the seam so a test can assert the
        // per-dispatch console-transmit top-up happens.
        self.pump_tx_calls.fetch_add(1, Ordering::Relaxed);
    }
}
