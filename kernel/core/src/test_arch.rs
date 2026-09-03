//! Host-only architecture mock implementing [`crate::KernelArch`].
//!
//! `TestArch` lets the host-side integration tests drive
//! [`crate::kernel_main`] and [`crate::handle_panic`] without a real
//! platform. It is gated behind `cfg(any(test, feature = "test-arch"))`
//! so production builds never link it (no hacks: a
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
//! "test-arch"))` (`panic!` allowed in tests).

extern crate std;

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use tairix_arch_api::{ContextSwitch, PrepareError, TaskContext, TaskEntry};

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
        if !stack_top.is_multiple_of(16) {
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
        // QEMU verticals exercise the real switch (no
        // fake primitive).
    }
}

/// Sentinel panic message produced by [`TestArch::halt`].
///
/// Tests assert on this exact string to confirm that the panic handler
/// or [`crate::kernel_main`] reached `halt`.
pub const HALT_SENTINEL: &str = "tairix-kernel-core: TestArch::halt called";

/// Monotonic ordering clock shared by the host doubles.
///
/// Two effects observed through *different* doubles — a filesystem flush
/// and a platform power-off, say — cannot be ordered from their own
/// private records. Each double stamps this one counter as it records, so
/// a smaller stamp really did happen earlier: the counter is global and
/// strictly increasing, so tests running in parallel only widen the gap
/// between two stamps, never swap them.
static HOST_EVENT_CLOCK: AtomicU64 = AtomicU64::new(0);

/// Take the next stamp from the shared host-double ordering clock.
///
/// Never zero, so a double can use zero to mean "this never happened".
#[must_use]
pub fn next_event_stamp() -> u64 {
    HOST_EVENT_CLOCK.fetch_add(1, Ordering::Relaxed) + 1
}

/// In-memory `KernelArch` implementation used by host-side tests.
///
/// The mock is intentionally minimal: it exposes one counter per
/// observable side effect (`halt` calls, IPIs per CPU) so tests assert
/// behaviour numerically instead of relying on flaky timing.
#[derive(Debug)]
pub struct TestArch {
    cpu_count: u32,
    current: AtomicU32,
    /// Number of [`SchedulerArch::current_cpu`] reads observed, so a test can
    /// prove a wait loop resolves the live CPU at each park rather than reusing
    /// one it read before the loop (`plans/OPEN-DEFECTS.md` D44).
    current_cpu_reads: AtomicU64,
    ticks: AtomicU64,
    halts: AtomicU64,
    ipis: AtomicU64,
    /// Number of [`KernelArch::set_device_irqs(true)`](KernelArch::set_device_irqs)
    /// calls observed.
    irq_enables: AtomicU64,
    /// Test-only gate that pauses the first device-IRQ mask operation until a
    /// cooperating thread publishes work in the idle-commit window.
    idle_mask_gate_armed: AtomicBool,
    /// Whether the gated device-IRQ mask operation has been reached.
    idle_mask_gate_entered: AtomicBool,
    /// Release flag for the gated device-IRQ mask operation.
    idle_mask_gate_released: AtomicBool,
    /// Number of [`KernelArch::wait_for_interrupt`] calls observed.
    interrupt_waits: AtomicU64,
    /// Number of [`KernelArch::flush_console_blocking`] calls observed, so a
    /// test can assert the fatal report drained its record before halting.
    console_flushes: AtomicU64,
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
    /// Number of [`KernelArch::poweroff`] calls observed.
    poweroffs: AtomicU64,
    /// Number of [`KernelArch::reboot`] calls observed.
    reboots: AtomicU64,
    /// [`next_event_stamp`] taken by the most recent power primitive, or
    /// `0` while neither has been reached.
    power_stamp: AtomicU64,
    /// Number of [`SchedulerArch::set_wakeup`] calls observed.
    wakeup_calls: AtomicU64,
    /// Last [`SchedulerArch::set_wakeup`] argument, encoded as `0` for
    /// `None` and `deadline + 1` for `Some(deadline)` (meaningful only
    /// once `wakeup_calls > 0`).
    last_wakeup: AtomicU64,
}

impl TestArch {
    /// Build a `TestArch` reporting `cpu_count` logical CPUs.
    ///
    /// Panics if `cpu_count == 0`, which is a test-only programming
    /// error permits panics in tests.
    #[must_use]
    pub fn with_cpus(cpu_count: u32) -> Self {
        assert!(cpu_count > 0, "TestArch requires at least one CPU");
        Self {
            cpu_count,
            current: AtomicU32::new(0),
            current_cpu_reads: AtomicU64::new(0),
            ticks: AtomicU64::new(0),
            halts: AtomicU64::new(0),
            ipis: AtomicU64::new(0),
            irq_enables: AtomicU64::new(0),
            idle_mask_gate_armed: AtomicBool::new(false),
            idle_mask_gate_entered: AtomicBool::new(false),
            idle_mask_gate_released: AtomicBool::new(false),
            interrupt_waits: AtomicU64::new(0),
            console_flushes: AtomicU64::new(0),
            pump_tx_calls: AtomicU64::new(0),
            monotonic_ns: AtomicU64::new(0),
            poweroffs: AtomicU64::new(0),
            reboots: AtomicU64::new(0),
            power_stamp: AtomicU64::new(0),
            wakeup_calls: AtomicU64::new(0),
            last_wakeup: AtomicU64::new(0),
        }
    }

    /// Override the "current CPU" reported by [`Self::current_cpu`].
    ///
    /// Tests use this to simulate panic handlers fired from a non-boot
    /// CPU.
    /// Number of [`SchedulerArch::current_cpu`] reads this handle has served.
    #[must_use]
    pub fn current_cpu_reads(&self) -> u64 {
        self.current_cpu_reads.load(Ordering::Relaxed)
    }

    /// Point [`SchedulerArch::current_cpu`] at `cpu`.
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

    /// Number of times device interrupt delivery was enabled.
    #[must_use]
    pub fn irq_enable_count(&self) -> u64 {
        self.irq_enables.load(Ordering::Relaxed)
    }

    /// Pause the next device-IRQ mask operation until
    /// [`Self::release_idle_mask_gate`] is called.
    pub fn arm_idle_mask_gate(&self) {
        self.idle_mask_gate_entered.store(false, Ordering::Relaxed);
        self.idle_mask_gate_released.store(false, Ordering::Relaxed);
        self.idle_mask_gate_armed.store(true, Ordering::Release);
    }

    /// Whether the dispatch loop has reached the armed IRQ-mask gate.
    #[must_use]
    pub fn idle_mask_gate_entered(&self) -> bool {
        self.idle_mask_gate_entered.load(Ordering::Acquire)
    }

    /// Release an IRQ-mask operation paused by [`Self::arm_idle_mask_gate`].
    pub fn release_idle_mask_gate(&self) {
        self.idle_mask_gate_released.store(true, Ordering::Release);
    }

    /// Number of idle interrupt waits observed.
    #[must_use]
    pub fn interrupt_wait_count(&self) -> u64 {
        self.interrupt_waits.load(Ordering::Relaxed)
    }

    /// Number of times [`KernelArch::pump_console_tx`] was reached.
    ///
    /// Lets a test assert the dispatch loop tops up the buffered console
    /// transmit on every iteration, not only when it idles.
    #[must_use]
    pub fn pump_console_tx_count(&self) -> u64 {
        self.pump_tx_calls.load(Ordering::Relaxed)
    }

    /// Number of times [`KernelArch::flush_console_blocking`] was reached.
    ///
    /// Lets a test assert the fatal report drained its own record to the
    /// device before halting.
    #[must_use]
    pub fn console_flush_count(&self) -> u64 {
        self.console_flushes.load(Ordering::Relaxed)
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

    /// Set the value [`SchedulerArch::ticks_now`] reports.
    ///
    /// Lets a test drive the process-admit start-time attestation with a
    /// known monotonic reading.
    pub fn set_ticks(&self, value: u64) {
        self.ticks.store(value, Ordering::Relaxed);
    }

    /// Number of times the platform was asked to power off.
    #[must_use]
    pub fn poweroff_count(&self) -> u64 {
        self.poweroffs.load(Ordering::Relaxed)
    }

    /// Number of times the platform was asked to reset.
    #[must_use]
    pub fn reboot_count(&self) -> u64 {
        self.reboots.load(Ordering::Relaxed)
    }

    /// The shared-clock stamp of the most recent power primitive, or
    /// `None` while the platform has not been asked to stop.
    ///
    /// Lets a test order the power request against work recorded by a
    /// different double — that every volume was flushed first, say.
    #[must_use]
    pub fn power_stamp(&self) -> Option<u64> {
        match self.power_stamp.load(Ordering::Relaxed) {
            0 => None,
            stamp => Some(stamp),
        }
    }

    /// Number of [`SchedulerArch::set_wakeup`] calls observed.
    #[must_use]
    pub fn set_wakeup_count(&self) -> u64 {
        self.wakeup_calls.load(Ordering::Relaxed)
    }

    /// The last [`SchedulerArch::set_wakeup`] argument, or `None` when the
    /// last call cleared the one-shot (meaningful only once
    /// [`Self::set_wakeup_count`] is non-zero).
    ///
    /// Lets a wait-syscall test assert the epilogue re-armed the shared
    /// one-shot to the nearest deadline across *every* timed wait-queue
    /// rather than clearing it from its own queue's empty view.
    #[must_use]
    pub fn last_set_wakeup(&self) -> Option<u64> {
        match self.last_wakeup.load(Ordering::Relaxed) {
            0 => None,
            encoded => Some(encoded - 1),
        }
    }
}

impl SchedulerArch for TestArch {
    fn current_cpu(&self) -> CpuId {
        self.current_cpu_reads.fetch_add(1, Ordering::Relaxed);
        self.current.load(Ordering::Relaxed)
    }

    fn set_wakeup(&self, deadline_ns: Option<u64>) {
        self.wakeup_calls.fetch_add(1, Ordering::Relaxed);
        self.last_wakeup.store(
            deadline_ns.map_or(0, |d| d.saturating_add(1)),
            Ordering::Relaxed,
        );
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

    fn arch_id(&self) -> Option<tairix_abi::Arch> {
        // The host test arch is not a shippable Tier-1 target: it states
        // no identity, so the boot facts stay uninstalled (fail closed)
        // rather than impersonating a real architecture.
        None
    }

    fn cpu_name(&self) -> Option<tairix_abi::CpuName> {
        // The host test arch runs on whatever machine hosts the tests: it
        // discovers no CPU model, stating an honest `None` rather than
        // fabricating a name.
        None
    }

    fn set_device_irqs(&self, enabled: bool) {
        if enabled {
            self.irq_enables.fetch_add(1, Ordering::Relaxed);
        } else if self.idle_mask_gate_armed.swap(false, Ordering::AcqRel) {
            self.idle_mask_gate_entered.store(true, Ordering::Release);
            while !self.idle_mask_gate_released.load(Ordering::Acquire) {
                core::hint::spin_loop();
            }
        }
    }

    fn wait_for_interrupt(&self) {
        self.interrupt_waits.fetch_add(1, Ordering::Relaxed);
    }

    fn pump_console_tx(&self) {
        // The host mock has no buffered device; it only records that the
        // dispatch loop reached the seam so a test can assert the
        // per-dispatch console-transmit top-up happens.
        self.pump_tx_calls.fetch_add(1, Ordering::Relaxed);
    }

    fn flush_console_blocking(&self) {
        self.console_flushes.fetch_add(1, Ordering::Relaxed);
    }

    fn poweroff(&self) {
        self.poweroffs.fetch_add(1, Ordering::Relaxed);
        self.power_stamp
            .store(next_event_stamp(), Ordering::Relaxed);
        // Returning is the honest host answer: no test machine may be
        // stopped, which is exactly what a port with no power-off
        // primitive does, so the caller sees the unsupported path.
    }

    fn reboot(&self) {
        self.reboots.fetch_add(1, Ordering::Relaxed);
        self.power_stamp
            .store(next_event_stamp(), Ordering::Relaxed);
        // Returns for the same reason [`Self::poweroff`] does.
    }
}

/// A host port double for the Arch HAL "enter user mode" surface.
///
/// Entering user mode is only meaningful on a bare-metal target, so a host
/// test can never execute the transition — but it still has to *name* a port
/// handle to build a [`crate::ProcessSpace`] or a
/// [`crate::BuiltImage`]. This is that handle, in one place rather than
/// re-declared in every test module.
pub struct NeverEnterUser;

impl tairix_arch_api::EnterUser for NeverEnterUser {
    unsafe fn enter_user(&self, _regs: tairix_arch_api::UserEntry) -> ! {
        unreachable!("enter_user is only meaningful on the bare-metal target")
    }
}

/// The shared `'static` [`NeverEnterUser`] host tests borrow.
pub static NEVER_ENTER_USER: NeverEnterUser = NeverEnterUser;

/// A switch-in hook that records nothing, for a host test whose subject is
/// not the port's register program.
#[must_use]
pub fn inert_process_resume() -> crate::spawn::ProcessResume {
    alloc::sync::Arc::new(|_stack_top: u64, _tls_base: u64| {})
}
