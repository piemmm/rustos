//! [`IrqTable`] — kernel IRQ binding table and per-handle ready
//! flag.
//!
//! See the crate-level docs for the design rationale; this module
//! is the implementation.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use tairix_abi::IrqHandle;
use tairix_kernel_sec::TaskId;
use tairix_sync::{OnceCell, RwLock};

use crate::error::{IrqError, MaskError};

/// One row in [`IrqTable`].
///
/// Public so kernel/core's `irq_release` audit emission can read
/// the bound owner; otherwise opaque.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct IrqEntry {
    /// Handle minted at [`IrqTable::bind`] time.
    pub handle: IrqHandle,
    /// Security-attribution task id of the bound owner.
    pub owner: TaskId,
    /// Architecture-defined IRQ line. Stable for the lifetime of
    /// the binding.
    pub line: u32,
}

/// Controller-mask seam.
///
/// The production [`IrqTable::fire`] path calls
/// [`Self::mask`] before it sets the per-line ready flag — the
/// load-bearing safety property of the user-space IRQ contract
/// (`docs/src/security/irq.md`). Architecture ports without a
/// programmable controller return [`MaskError::Unsupported`].
pub trait IrqController {
    /// Mask `line` at the controller. Must complete before
    /// [`IrqTable::fire`] sets the per-entry ready flag.
    ///
    /// # Errors
    ///
    /// * [`MaskError::Unsupported`] if the architecture has no
    ///   programmable interrupt controller wired in this build.
    /// * [`MaskError::OutOfRange`] if `line` exceeds the
    ///   controller's addressable range.
    fn mask(&self, line: u32) -> Result<(), MaskError>;

    /// Route `line` to a waiting CPU and unmask it so the next device
    /// interrupt on it is delivered.
    ///
    /// [`IrqTable::fire`] masks a line before a waiter observes the wake
    /// (mask-before-wake, `docs/src/security/irq.md`), so once a driver has
    /// drained the completion the line must be re-enabled for the next one.
    /// A user-space interrupt-driven driver cannot touch the controller, so
    /// the `irq_wait` park path re-arms the bound line through this method on
    /// the driver's behalf (no ambient hardware access). It
    /// is idempotent: re-routing an already-routed line and clearing an
    /// already-clear mask are both no-ops.
    ///
    /// The default is a no-op for controllers without a programmable unmask
    /// (placeholders, mask-only test doubles, or ports with no interrupt-driven
    /// user-space driver consumer yet, no interface ahead of a caller).
    /// The aarch64 `GicIrqController` overrides it to route the line to the
    /// boot CPU and clear its enable bit, which is what the user-space
    /// virtio-input keyboard driver's `irq_wait` park path drives.
    ///
    /// # Errors
    ///
    /// * [`MaskError::Unsupported`] if the architecture has no programmable
    ///   controller wired in this build.
    /// * [`MaskError::OutOfRange`] if `line` exceeds the controller's range.
    fn rearm(&self, line: u32) -> Result<(), MaskError> {
        let _ = line;
        Ok(())
    }
}

/// Outcome of one [`IrqTable::try_wait_step`] poll.
///
/// The syscall handler runs the polling loop, mapping each
/// outcome to the documented stable `Errno`:
///
/// * [`Self::Ready`] → `Ok(())`
/// * [`Self::Continue`] → drop back to user space via
///   `Scheduler::yield_current` and retry on the next quantum.
/// * [`Self::TimedOut`] → `Err(Errno::TimedOut)`.
/// * [`Self::NotFound`] → `Err(Errno::NotFound)` — handle was
///   not minted for the caller, or was released between two
///   polls (e.g. the task exited and `release_for` ran).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum WaitStep {
    /// The line has fired since the last poll; the ready flag has
    /// been consumed. The waiter must observe `Ok(())`.
    Ready,
    /// No fire yet; the deadline has not yet been reached. The
    /// waiter should yield and retry.
    Continue,
    /// `now_ns >= deadline_ns` without a fire.
    TimedOut,
    /// The handle does not belong to the caller (forged), or the
    /// binding has been released since the last poll.
    NotFound,
    /// The bound line was **quarantined**: it fired far faster than any
    /// real device serviced through the user-space round-trip could, so
    /// [`IrqTable::fire`] disabled it (kept it masked, stopped delivering
    /// wakes) to stop it monopolising a CPU. A terminal, fail-closed
    /// outcome: the waiter must surface an error rather than re-arm and
    /// re-park, since re-arming would immediately re-storm. The binding is
    /// cleared for the line's next [`IrqTable::bind`].
    Quarantined,
}

/// Outcome of an [`IrqTable::bind`] success path.
///
/// A separate type rather than a bare `IrqHandle` so the syscall
/// handler's audit record can name the bound line explicitly
/// (without re-reading the table).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct BindOutcome {
    /// Newly minted opaque handle.
    pub handle: IrqHandle,
    /// Line the binding is recorded against.
    pub line: u32,
}

/// Outcome of [`IrqTable::fire`].
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum FireOutcome {
    /// The line had a binding; the ready flag was set.
    /// Mask-before-wake was honoured.
    Marked,
    /// The line has no binding (a stray interrupt from a line no
    /// driver claims). The mask write still happened so the
    /// stray edge does not re-fire; ready was not touched.
    Stray,
    /// The line fired past its rate budget within the accounting window and
    /// has been **quarantined**: the mask write happened (so it stays
    /// contained) but the ready flag was deliberately **not** set — a
    /// quarantined line delivers no further wakes until it is rebound. This
    /// is the runaway-interrupt safety net: a device (or user-space driver)
    /// that re-asserts a line as fast as the kernel can mask/wake it would
    /// otherwise peg a CPU, so the kernel disables the line and lets its
    /// waiter fail closed rather than spin. The dispatch caller emits the
    /// audit record; the parked waiter observes [`WaitStep::Quarantined`].
    Quarantined,
}

/// Placeholder [`IrqController`] for architecture ports without a
/// programmable interrupt controller wired in this build.
///
/// Every call to [`Self::mask`] returns [`MaskError::Unsupported`],
/// which [`IrqTable::fire`] translates to
/// [`IrqError::ArchUnsupported`] and the syscall handler in turn
/// translates to `Errno::NotImplemented`. The kernel binary's
/// startup audit log records one event naming the architecture
/// before installing the unsupported controller, per the
/// failure-mode table in `docs/src/security/irq.md`.
#[derive(Copy, Clone, Debug, Default)]
pub struct UnsupportedController;

impl IrqController for UnsupportedController {
    fn mask(&self, _line: u32) -> Result<(), MaskError> {
        Err(MaskError::Unsupported)
    }
}

/// Shared `'static` [`UnsupportedController`] suitable as the default
/// controller reference handed back from
/// `tairix_kernel_core::KernelArch::irq_routing` on architectures
/// or boot paths that have not yet installed a real controller.
///
/// Exposed as a `pub static` (not a `const`) so callers can take a
/// `&'static (dyn IrqController + Send + Sync)` reference without
/// risking the `clippy::declare_interior_mutable_const` footgun. The
/// unit-like type has no interior mutability, so the lint does not
/// fire here, but the `static` form keeps the address stable and
/// allows the type-erased reference to round-trip through the
/// `tairix_kernel_core` handover without surprise. — no global mutable state; this is an *immutable* static.
pub static UNSUPPORTED_CONTROLLER: UnsupportedController = UnsupportedController;

/// Outcome of [`IrqTable::release_for`].
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct ReleaseOutcome {
    /// Number of bindings the call dropped.
    pub released: usize,
}

/// An [`IrqTable::set_observer`] call was rejected because an observer was
/// already installed. The hook is set-once at boot; a second install is a
/// defect, not a runtime condition.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct ObserverAlreadyInstalled;

/// A passive observer notified on every interrupt dispatch.
///
/// [`IrqTable::fire`] calls [`Self::on_irq`] at its entry for **every**
/// interrupt arrival — bound *and* stray — before the controller mask and the
/// ready-flag store. The kernel installs one implementation whose only job is
/// to feed the interrupt-arrival *timing* into the kernel entropy pool
/// (`lib/rng`), turning the physically-unpredictable inter-arrival intervals
/// of real devices into an independent entropy input.
///
/// # Contract
///
/// * It runs in **interrupt context**: it must be wait-free and must never
///   block, take a lock, allocate, or panic.
/// * It must **not** influence the mask-before-wake path — it is purely
///   observational, so a slow or absent observer can never weaken the IRQ
///   security contract (`docs/src/security/irq.md`).
/// * The [`Sync`] supertrait lets a `&'static dyn IrqDispatchObserver` be
///   shared across CPUs, which every SMP IRQ path requires.
pub trait IrqDispatchObserver: Sync {
    /// Notify the observer that `line` fired. See the trait contract.
    fn on_irq(&self, line: u32);
}

/// Monotonic-clock seam the runaway-interrupt safety net reads.
///
/// [`IrqTable::fire`] runs in interrupt context and must stay wait-free, so
/// the rate-accounting clock is injected as a set-once, lock-free hook (like
/// [`IrqDispatchObserver`]) rather than threaded through every architecture's
/// dispatch call site. `kernel/irq` keeps no architecture dependency: the
/// kernel binary installs one whose `now_ns` reads the arch monotonic timer.
/// Until a clock is installed, rate accounting is inert (a line is never
/// quarantined), so a host test or an early-boot fire behaves exactly as
/// before.
///
/// # Contract
///
/// * It runs in **interrupt context**: it must be wait-free and must never
///   block, take a lock, allocate, or panic.
/// * `now_ns` must be non-decreasing on a given CPU; the accounting tolerates
///   cross-CPU skew (it is a coarse safety net, not a precise meter).
/// * The [`Sync`] supertrait lets a `&'static dyn MonotonicClock` be shared
///   across CPUs, which every SMP IRQ path requires.
pub trait MonotonicClock: Sync {
    /// Current kernel monotonic time in nanoseconds.
    fn now_ns(&self) -> u64;
}

/// Length of the sliding window over which a line's fires are counted, in
/// nanoseconds (1 second). Chosen with [`STORM_FIRE_BUDGET`] so the trip
/// threshold is a sustained *rate* no correctly-serviced device reaches.
pub(crate) const STORM_WINDOW_NS: u64 = 1_000_000_000;

/// Maximum fires one line may take within a [`STORM_WINDOW_NS`] window before
/// it is quarantined.
///
/// This is a **safety net against a runaway line**, not a throughput cap.
/// Under mask-before-wake a bound line can only re-fire *after* its
/// user-space driver drains the completion and the kernel re-arms it, so
/// every legitimate interrupt costs a full user-space round-trip (syscall →
/// driver work → syscall). No real device driven that way sustains anywhere
/// near 100 000 interrupts per second on one line; a line that does is
/// re-asserting with no work being done (a wedged controller, a
/// never-quiesced source, or a hostile device), i.e. exactly the storm that
/// would otherwise peg a CPU. The budget is deliberately generous so a busy
/// but healthy line never trips, mirroring Linux's `note_interrupt`
/// spurious-IRQ disable in spirit while fitting this kernel's userspace-IRQ
/// model.
pub(crate) const STORM_FIRE_BUDGET: u32 = 100_000;

/// Kernel IRQ table.
///
/// One per running kernel. Interior synchronisation through a
/// writer-preference [`RwLock`] mirroring the `CapTable`
/// lock-ordering policy (no global mutable
/// static; the table is owned by `KernelState`, which itself lives
/// for the lifetime of the running kernel).
pub struct IrqTable {
    inner: RwLock<Inner>,
    max_line: u32,
    /// Set-once, lock-free-read hook notified on every [`IrqTable::fire`]
    /// (see [`IrqDispatchObserver`]). Read through [`OnceCell::get`] (an
    /// `Acquire` load, no lock) so the interrupt-context `fire` path stays
    /// wait-free; empty until the kernel installs the entropy observer at
    /// boot, and a no-op when empty.
    observer: OnceCell<&'static dyn IrqDispatchObserver>,
    /// Set-once, lock-free-read monotonic clock the runaway-interrupt
    /// safety net reads from the interrupt-context [`IrqTable::fire`] path
    /// (see [`MonotonicClock`]). Empty until the kernel installs it at
    /// boot; while empty, rate accounting is inert (no line is
    /// quarantined), so early-boot fires and host tests are unaffected.
    clock: OnceCell<&'static dyn MonotonicClock>,
    /// Per-line start of the current fire-accounting window (arch monotonic
    /// ns), paired with [`Self::storm_fire_count`]. Lock-free like
    /// [`Self::ready`] so [`IrqTable::fire`] never blocks. Indexed by line.
    storm_window_start_ns: Vec<AtomicU64>,
    /// Per-line count of fires observed in the current window. When it
    /// exceeds [`STORM_FIRE_BUDGET`] the line is quarantined. Indexed by
    /// line.
    storm_fire_count: Vec<AtomicU32>,
    /// Per-line sticky "this line was quarantined" flag. Once set,
    /// [`IrqTable::fire`] stops delivering wakes for the line (it stays
    /// masked) and [`IrqTable::try_wait_step`] reports
    /// [`WaitStep::Quarantined`]; only a fresh [`IrqTable::bind`] clears
    /// it. Indexed by line.
    quarantined: Vec<AtomicBool>,
    /// Per-line "fired since last consume" flags, kept **outside**
    /// [`Inner`]'s [`RwLock`] so [`IrqTable::fire`] — which runs in
    /// interrupt context — can record a wake-up with a single atomic
    /// store and **never** blocks on the lock. A task parked in
    /// [`IrqTable::try_wait_step`] (which holds only a *read* guard)
    /// can therefore be woken by the same-CPU completion ISR without
    /// the ISR spinning on a lock the parked task holds (no hacks; this is the interrupt-reentrancy-safe design).
    /// Indexed by line; length is `max_line + 1`.
    ready: Vec<AtomicBool>,
    /// Per-line "a binding exists" flags, maintained under the same
    /// `Inner` write lock as [`Inner::entries`] but readable lock-free
    /// by [`IrqTable::fire`] so a stray edge on an unbound line is
    /// reported as [`FireOutcome::Stray`] without taking the lock.
    bound: Vec<AtomicBool>,
}

impl core::fmt::Debug for IrqTable {
    /// Reports only lock-free fields: it must not take the `Inner` lock (a
    /// `fire`-context or parked-waiter deadlock hazard) and never reveals
    /// bindings. The observer is shown by presence only.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("IrqTable")
            .field("max_line", &self.max_line)
            .field(
                "observer_installed",
                &matches!(self.observer.get(), Ok(Some(_))),
            )
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
struct Inner {
    /// Monotonically incrementing source of fresh [`IrqHandle`]
    /// values. Starts at 1 because [`IrqHandle::INVALID`] is 0.
    next_handle: u64,
    /// `line → IrqEntry`. The line is the primary key because a
    /// hardware interrupt arrives addressed by line, not by
    /// handle.
    entries: BTreeMap<u32, IrqEntry>,
    /// Secondary index `handle.raw() → line` so
    /// [`IrqTable::try_wait_step`] is O(log n) on the handle
    /// lookup without scanning every entry.
    by_handle: BTreeMap<u64, u32>,
}

impl IrqTable {
    /// Construct an empty IRQ table.
    ///
    /// `max_line` is the inclusive upper bound on the architecture
    /// port's IRQ-line identifier space — on x86_64 this is the
    /// IO-APIC's `max_redirection_entry`; on architectures whose
    /// production [`IrqController::mask`] returns
    /// [`MaskError::Unsupported`], pass `0` so every `bind` call
    /// fails-fast with [`IrqError::LineOutOfRange`] before any
    /// state is touched (fail closed).
    #[must_use]
    pub fn new(max_line: u32) -> Self {
        // One flag slot per addressable line (`0..=max_line`). `bind`
        // rejects any line above `max_line`, so every bound line
        // indexes a valid slot.
        let slots = (max_line as usize).saturating_add(1);
        let mut ready = Vec::with_capacity(slots);
        let mut bound = Vec::with_capacity(slots);
        let mut storm_window_start_ns = Vec::with_capacity(slots);
        let mut storm_fire_count = Vec::with_capacity(slots);
        let mut quarantined = Vec::with_capacity(slots);
        for _ in 0..slots {
            ready.push(AtomicBool::new(false));
            bound.push(AtomicBool::new(false));
            storm_window_start_ns.push(AtomicU64::new(0));
            storm_fire_count.push(AtomicU32::new(0));
            quarantined.push(AtomicBool::new(false));
        }
        Self {
            inner: RwLock::new(Inner {
                next_handle: 1,
                entries: BTreeMap::new(),
                by_handle: BTreeMap::new(),
            }),
            max_line,
            observer: OnceCell::new(),
            clock: OnceCell::new(),
            storm_window_start_ns,
            storm_fire_count,
            quarantined,
            ready,
            bound,
        }
    }

    /// Install the set-once monotonic clock the runaway-interrupt safety net
    /// reads (see [`MonotonicClock`]).
    ///
    /// Called **exactly once** at boot, after the arch timer is discovered.
    /// Until it is installed, [`IrqTable::fire`] performs no rate accounting
    /// and no line is ever quarantined, so an early-boot interrupt behaves
    /// exactly as before.
    ///
    /// # Errors
    ///
    /// [`ObserverAlreadyInstalled`] if a clock is already installed — a
    /// second install is a defect (set-once), not a runtime condition.
    pub fn set_clock(
        &self,
        clock: &'static dyn MonotonicClock,
    ) -> Result<(), ObserverAlreadyInstalled> {
        self.clock.set(clock).map_err(|_| ObserverAlreadyInstalled)
    }

    /// Install the set-once interrupt-dispatch observer (see
    /// [`IrqDispatchObserver`]).
    ///
    /// Called **exactly once** at boot, after the arch entropy source is
    /// available, to feed interrupt-arrival timing into the kernel entropy
    /// pool. The observer reference outlives the running kernel (the kernel
    /// leaks it, like the table itself).
    ///
    /// # Errors
    ///
    /// [`ObserverAlreadyInstalled`] if an observer is already installed — a
    /// second install is a defect (set-once), not a runtime condition.
    pub fn set_observer(
        &self,
        observer: &'static dyn IrqDispatchObserver,
    ) -> Result<(), ObserverAlreadyInstalled> {
        self.observer
            .set(observer)
            .map_err(|_| ObserverAlreadyInstalled)
    }

    /// Inclusive upper bound on accepted line numbers.
    #[must_use]
    pub fn max_line(&self) -> u32 {
        self.max_line
    }

    /// Number of bindings currently recorded.
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.read().entries.len()
    }

    /// `true` iff there are no recorded bindings.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.inner.read().entries.is_empty()
    }

    /// Bind `line` to `owner`, minting a fresh [`IrqHandle`].
    ///
    /// # Errors
    ///
    /// * [`IrqError::LineOutOfRange`] if `line > self.max_line()`.
    /// * [`IrqError::LineAlreadyBound`] if a binding for `line`
    ///   already exists (regardless of owner). `abi-v1` does not
    ///   support shared lines.
    pub fn bind(&self, line: u32, owner: TaskId) -> Result<BindOutcome, IrqError> {
        if line > self.max_line {
            return Err(IrqError::LineOutOfRange);
        }
        let mut g = self.inner.write();
        if g.entries.contains_key(&line) {
            return Err(IrqError::LineAlreadyBound);
        }
        let raw = g.next_handle;
        // `next_handle` starts at 1 and is monotonic; saturating at
        // `u64::MAX` is a fail-closed limit, not a wrap, so a
        // theoretical `2^63` rebind storm cannot collide with a
        // live handle.
        g.next_handle = g.next_handle.saturating_add(1);
        let handle = IrqHandle::from_raw(raw);
        let entry = IrqEntry {
            handle,
            owner,
            line,
        };
        g.entries.insert(line, entry);
        g.by_handle.insert(raw, line);
        // Reset the lock-free flags for this line, then publish the
        // binding. The store order (clear `ready`, clear the storm
        // accounting + quarantine, set `bound` last) means `fire` only
        // ever observes `bound == true` for a line whose `ready` slot and
        // rate-accounting were already re-initialised. Clearing
        // `quarantined` here is what lets a driver recover a previously
        // stormed line: a fresh bind starts it clean.
        self.ready[line as usize].store(false, Ordering::SeqCst);
        self.storm_window_start_ns[line as usize].store(0, Ordering::SeqCst);
        self.storm_fire_count[line as usize].store(0, Ordering::SeqCst);
        self.quarantined[line as usize].store(false, Ordering::SeqCst);
        self.bound[line as usize].store(true, Ordering::SeqCst);
        Ok(BindOutcome { handle, line })
    }

    /// Atomically inspect the binding for `handle` on behalf of
    /// `caller`, returning the next step the syscall handler must
    /// take.
    ///
    /// `now_ns` and `deadline_ns` come from
    /// `KernelArch::monotonic_ns` at the caller's CPU. The handler
    /// computes the deadline once (at entry to `irq_wait`) and
    /// passes it verbatim on every iteration so the polling loop
    /// is monotonic in wall-clock terms even if the per-CPU clock
    /// jitters.
    ///
    /// # Ordering
    ///
    /// The check order is documented to make the forgery / quarantine /
    /// timeout /  ready hierarchy explicit:
    ///
    /// 1. Look up by handle. If the handle is unknown or its
    ///    binding's owner is not `caller`, return
    ///    [`WaitStep::NotFound`]. The forgery check beats every
    ///    other check (identify before any
    ///    state-touching transition).
    /// 2. If the line has been quarantined by the runaway-interrupt
    ///    safety net, return [`WaitStep::Quarantined`] (terminal,
    ///    fail-closed). A quarantined line never delivers a `ready`, so
    ///    this cannot mask a real fire.
    /// 3. If `ready` is set, clear it and return
    ///    [`WaitStep::Ready`]. The ready flag wins over a
    ///    near-simultaneous deadline because the wake-up did
    ///    happen — surfacing `TimedOut` here would silently drop
    ///    a successful interrupt.
    /// 4. If `now_ns >= deadline_ns`, return
    ///    [`WaitStep::TimedOut`].
    /// 5. Otherwise [`WaitStep::Continue`].
    #[must_use]
    pub fn try_wait_step(
        &self,
        handle: IrqHandle,
        caller: TaskId,
        now_ns: u64,
        deadline_ns: u64,
    ) -> WaitStep {
        // Only a *read* guard is needed: the forgery check reads the
        // immutable `by_handle` / `entries` maps, and the ready flag
        // lives in the lock-free `self.ready` array. Holding a read
        // guard (rather than a write guard) is what lets the same-CPU
        // completion ISR run `fire` — which takes no `Inner` lock at
        // all — without deadlocking against a parked waiter.
        let line = {
            let g = self.inner.read();
            let raw = handle.as_u64();
            let Some(&line) = g.by_handle.get(&raw) else {
                return WaitStep::NotFound;
            };
            let Some(entry) = g.entries.get(&line) else {
                // by_handle and entries are kept consistent; this is a
                // belt-and-braces fail-closed.
                return WaitStep::NotFound;
            };
            if entry.owner != caller {
                return WaitStep::NotFound;
            }
            line
        };
        // A quarantined line is terminal and fail-closed: the runaway
        // safety net disabled it, so report it before anything else (a
        // real fire cannot have out-raced the quarantine — `fire` stops
        // setting `ready` the moment it quarantines). The waiter surfaces
        // an error instead of re-arming, which would immediately re-storm.
        if self.quarantined[line as usize].load(Ordering::Acquire) {
            return WaitStep::Quarantined;
        }
        // The ready flag wins over a near-simultaneous deadline: a
        // wake-up that happened must not be masked by `TimedOut`. The
        // `swap` consumes the flag with `SeqCst`, pairing with the
        // `SeqCst` fence `IrqController::mask` issues before `fire`
        // sets it, so the mask-before-wake ordering holds.
        if self.ready[line as usize].swap(false, Ordering::SeqCst) {
            return WaitStep::Ready;
        }
        if now_ns >= deadline_ns {
            return WaitStep::TimedOut;
        }
        WaitStep::Continue
    }

    /// The line bound to `handle` for `caller`, or [`None`] if the handle is
    /// unknown or its binding is owned by another task.
    ///
    /// Applies the same owner check [`Self::try_wait_step`] performs
    /// (identify before acting), so the `irq_wait` park
    /// path can resolve the line to re-arm without trusting a caller-supplied
    /// value: a forged or foreign handle yields [`None`] and re-arms nothing.
    #[must_use]
    pub fn line_for(&self, handle: IrqHandle, caller: TaskId) -> Option<u32> {
        let g = self.inner.read();
        let line = *g.by_handle.get(&handle.as_u64())?;
        let entry = g.entries.get(&line)?;
        if entry.owner != caller {
            return None;
        }
        Some(line)
    }

    /// The task that owns the binding for `line`, or [`None`] if `line` is
    /// bound to no task.
    ///
    /// A read-only, owner-agnostic lookup (no caller check): it answers
    /// *who* a line belongs to, used by the CPU-lockup watchdog to attribute
    /// a stuck controller line to the driver that owns it — turning a raw
    /// interrupt id in a lockup report into `stuck_owner=<task>` for a bound
    /// line, or `unbound` for a spurious/contained line no driver owns. It
    /// grants no authority and mutates nothing.
    #[must_use]
    pub fn owner_of_line(&self, line: u32) -> Option<TaskId> {
        self.inner
            .read()
            .entries
            .get(&line)
            .map(|entry| entry.owner)
    }

    /// Fire `line`: mask the controller, then set the per-entry
    /// ready flag.
    ///
    /// **Mask-before-wake is the load-bearing invariant**: the
    /// kernel's user-space IRQ contract
    /// (`docs/src/security/irq.md`) requires the controller-level
    /// mask to be installed *before* the waiter observes the
    /// fire, so an edge-triggered device cannot re-fire while the
    /// driver is still draining its completion queue. The call
    /// order in this function — `controller.mask(line)?` first,
    /// `entry.ready = true` second — is exactly this invariant.
    ///
    /// # Errors
    ///
    /// * [`IrqError::ArchUnsupported`] if the controller's `mask`
    ///   returned [`MaskError::Unsupported`].
    /// * [`IrqError::LineOutOfRange`] if `controller.mask` returned
    ///   [`MaskError::OutOfRange`] — the arch port disagreed with
    ///   the table's `max_line`. A bug rather than a runtime
    ///   condition, but routed to a stable errno (fail closed, never panic).
    pub fn fire(&self, line: u32, controller: &dyn IrqController) -> Result<FireOutcome, IrqError> {
        // Feed the interrupt-arrival timing to the entropy observer first, so
        // the sample is taken as close to arrival as possible. It is purely
        // observational (wait-free, no lock) and never affects the
        // mask-before-wake path below. A poisoned or empty cell is a no-op.
        if let Ok(Some(observer)) = self.observer.get() {
            observer.on_irq(line);
        }
        controller.mask(line).map_err(|e| match e {
            MaskError::Unsupported => IrqError::ArchUnsupported,
            MaskError::OutOfRange => IrqError::LineOutOfRange,
        })?;
        // Interrupt-context fast path: consult only the lock-free
        // per-line flags. Taking `Inner`'s lock here would deadlock a
        // single CPU whose parked task already holds it in
        // `try_wait_step`; the `bound` / `ready` atoms exist precisely
        // so `fire` never blocks.
        let Some(bound) = self.bound.get(line as usize) else {
            // Line outside the addressable range — the mask still
            // happened (or failed above); treat as a contained stray.
            return Ok(FireOutcome::Stray);
        };
        if !bound.load(Ordering::SeqCst) {
            // No binding — the mask still happened, the stray edge is
            // contained (and its arrival timing was already fed to the entropy
            // observer at the top of `fire`). Surface to the caller so an
            // arch-port audit observer can record stray-IRQ rate.
            return Ok(FireOutcome::Stray);
        }
        // Runaway-line safety net: the line stays masked (done above) but
        // stops being re-armed/re-delivered once it fires past its rate
        // budget, so a never-quiesced or hostile source cannot peg a CPU
        // through the mask/wake/re-arm cycle. A line already quarantined
        // never re-delivers.
        if self.note_fire_and_quarantined(line as usize) {
            return Ok(FireOutcome::Quarantined);
        }
        // `mask` issued a `SeqCst` fence before returning; setting
        // `ready` after it preserves the mask-before-wake invariant a
        // `try_wait_step` consumer observes through the paired load.
        self.ready[line as usize].store(true, Ordering::SeqCst);
        Ok(FireOutcome::Marked)
    }

    /// Record one fire of `line` against its sliding-window rate budget and
    /// report whether the line is now (or already) **quarantined**.
    ///
    /// Runs in interrupt context from [`Self::fire`], so it is wait-free:
    /// it reads the set-once [`MonotonicClock`] and the per-line atomics
    /// only, never a lock. Until a clock is installed it is inert and
    /// returns `false` (accounting off), so an early-boot fire and a host
    /// test without an installed clock behave exactly as before.
    ///
    /// The window is a coarse safety net, not a precise meter: cross-CPU
    /// races on the count merely shift the trip point by a few fires, which
    /// is immaterial against a [`STORM_FIRE_BUDGET`] of 100 000. Once the
    /// sticky quarantine flag is set it stays set until the next
    /// [`Self::bind`] clears it.
    fn note_fire_and_quarantined(&self, line: usize) -> bool {
        if self.quarantined[line].load(Ordering::Acquire) {
            return true;
        }
        let Ok(Some(clock)) = self.clock.get() else {
            // No clock installed yet: accounting is inert (fail *open* on
            // the safety net, never on security — the line is still masked
            // and delivered normally).
            return false;
        };
        let now = clock.now_ns();
        let start = self.storm_window_start_ns[line].load(Ordering::Relaxed);
        // A fresh window (first fire, or the previous one elapsed) resets
        // the count to this fire and starts the clock again.
        if now.saturating_sub(start) >= STORM_WINDOW_NS {
            self.storm_window_start_ns[line].store(now, Ordering::Relaxed);
            self.storm_fire_count[line].store(1, Ordering::Relaxed);
            return false;
        }
        let count = self.storm_fire_count[line]
            .fetch_add(1, Ordering::Relaxed)
            .saturating_add(1);
        if count > STORM_FIRE_BUDGET {
            // Trip: keep the line masked (already masked by `fire`) and
            // deliver no wake. The parked waiter observes
            // `WaitStep::Quarantined` on its next poll and fails closed.
            self.quarantined[line].store(true, Ordering::Release);
            return true;
        }
        false
    }

    /// Whether the line bound to `handle` for `caller` has been
    /// quarantined by the runaway-line safety net.
    ///
    /// Owner-checked like [`Self::line_for`] (identify before acting): a
    /// forged or foreign handle, or an unknown line, reports `false`. Used
    /// by the wait-set path to fail a quarantined IRQ member closed rather
    /// than re-arm and re-storm it. A pure read, safe from any context.
    #[must_use]
    pub fn is_quarantined(&self, handle: IrqHandle, caller: TaskId) -> bool {
        match self.line_for(handle, caller) {
            Some(line) => self.quarantined[line as usize].load(Ordering::Acquire),
            None => false,
        }
    }

    /// Drop every binding owned by `task`.
    ///
    /// Called from `KernelSyscallHandlers::exit` on the syscall
    /// path and (in future stages) from the scheduler-driven task
    /// teardown path. Idempotent: a second call is a no-op.
    pub fn release_for(&self, task: TaskId) -> ReleaseOutcome {
        let mut g = self.inner.write();
        let to_drop: alloc::vec::Vec<u32> = g
            .entries
            .iter()
            .filter_map(|(line, e)| (e.owner == task).then_some(*line))
            .collect();
        let released = to_drop.len();
        for line in to_drop {
            if let Some(entry) = g.entries.remove(&line) {
                g.by_handle.remove(&entry.handle.as_u64());
                // Clear the binding's lock-free flags so a late edge
                // on the now-unbound line is reported as a stray, and
                // reset the runaway accounting so a future rebind starts
                // clean (bind resets these too; clearing here keeps a
                // released line's state tidy in the meantime).
                self.bound[line as usize].store(false, Ordering::SeqCst);
                self.ready[line as usize].store(false, Ordering::SeqCst);
                self.quarantined[line as usize].store(false, Ordering::SeqCst);
                self.storm_fire_count[line as usize].store(0, Ordering::SeqCst);
                self.storm_window_start_ns[line as usize].store(0, Ordering::SeqCst);
            }
        }
        ReleaseOutcome { released }
    }

    /// Snapshot of an entry by handle, for diagnostic / audit
    /// emission only. Returns `None` if the handle is unknown.
    #[must_use]
    pub fn lookup(&self, handle: IrqHandle) -> Option<IrqEntry> {
        let g = self.inner.read();
        let line = *g.by_handle.get(&handle.as_u64())?;
        g.entries.get(&line).copied()
    }

    /// Whether the line bound to `handle` has a pending, *un-consumed*
    /// fire.
    ///
    /// Read-only poll/diagnostic companion to [`Self::lookup`]: it
    /// reports the lock-free per-line ready flag without clearing it
    /// (only [`Self::try_wait_step`] consumes the flag). Returns
    /// `false` for an unknown handle. Taking only a read guard, it is
    /// safe to call from any context, including alongside an
    /// in-flight [`Self::fire`] (a narrow read-only
    /// query, not a new mutation surface).
    #[must_use]
    pub fn ready_for(&self, handle: IrqHandle) -> bool {
        let g = self.inner.read();
        let Some(&line) = g.by_handle.get(&handle.as_u64()) else {
            return false;
        };
        self.ready[line as usize].load(Ordering::SeqCst)
    }

    /// Current value of the lock-free ready flag for `line`. Test-only
    /// observer for the mask-before-wake ordering assertions.
    #[cfg(test)]
    #[must_use]
    fn ready_flag(&self, line: u32) -> bool {
        self.ready[line as usize].load(Ordering::SeqCst)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;
    use core::cell::RefCell;

    extern crate std;

    /// Deterministic mock controller. Records the sequence of
    /// `mask(line)` calls so tests can assert ordering against
    /// table state changes (the mask-before-wake invariant).
    struct MockController {
        calls: RefCell<Vec<u32>>,
        unsupported: bool,
        out_of_range_above: Option<u32>,
    }

    impl MockController {
        fn ok() -> Self {
            Self {
                calls: RefCell::new(Vec::new()),
                unsupported: false,
                out_of_range_above: None,
            }
        }

        fn unsupported() -> Self {
            Self {
                calls: RefCell::new(Vec::new()),
                unsupported: true,
                out_of_range_above: None,
            }
        }

        fn with_max(max: u32) -> Self {
            Self {
                calls: RefCell::new(Vec::new()),
                unsupported: false,
                out_of_range_above: Some(max),
            }
        }

        fn calls(&self) -> Vec<u32> {
            self.calls.borrow().clone()
        }
    }

    impl IrqController for MockController {
        fn mask(&self, line: u32) -> Result<(), MaskError> {
            if self.unsupported {
                return Err(MaskError::Unsupported);
            }
            if let Some(max) = self.out_of_range_above {
                if line > max {
                    return Err(MaskError::OutOfRange);
                }
            }
            self.calls.borrow_mut().push(line);
            Ok(())
        }
    }

    #[test]
    fn bind_mints_handle_and_records_owner() {
        let t = IrqTable::new(31);
        let out = t.bind(7, TaskId(42)).expect("bind");
        assert_eq!(out.line, 7);
        assert_ne!(out.handle, IrqHandle::INVALID);
        let entry = t.lookup(out.handle).expect("present");
        assert_eq!(entry.line, 7);
        assert_eq!(entry.owner, TaskId(42));
        assert!(!t.ready_flag(7));
    }

    #[test]
    fn owner_of_line_names_the_bound_task_and_is_none_when_unbound() {
        let t = IrqTable::new(31);
        // Unbound line: no owner.
        assert_eq!(t.owner_of_line(7), None);
        let _ = t.bind(7, TaskId(42)).expect("bind");
        // Bound line: names the owner, without a caller check (any reader).
        assert_eq!(t.owner_of_line(7), Some(TaskId(42)));
        // A different, still-unbound line stays None.
        assert_eq!(t.owner_of_line(8), None);
        // Releasing the owner's bindings makes the line unbound again.
        let _ = t.release_for(TaskId(42));
        assert_eq!(t.owner_of_line(7), None);
    }

    #[test]
    fn bind_refuses_duplicate_line() {
        let t = IrqTable::new(31);
        let _ = t.bind(7, TaskId(1)).unwrap();
        // Same task, same line: still refused — `abi-v1` does not
        // share lines.
        assert_eq!(t.bind(7, TaskId(1)), Err(IrqError::LineAlreadyBound));
        // Different task, same line: refused for the same reason.
        assert_eq!(t.bind(7, TaskId(2)), Err(IrqError::LineAlreadyBound));
    }

    #[test]
    fn bind_refuses_out_of_range_line() {
        let t = IrqTable::new(15);
        assert_eq!(t.bind(16, TaskId(1)), Err(IrqError::LineOutOfRange));
        // Boundary case: max_line itself is accepted.
        let _ = t.bind(15, TaskId(1)).expect("boundary accepted");
    }

    #[test]
    fn try_wait_step_returns_continue_when_no_ready_and_not_expired() {
        let t = IrqTable::new(31);
        let out = t.bind(7, TaskId(42)).unwrap();
        assert_eq!(
            t.try_wait_step(out.handle, TaskId(42), 0, 1_000),
            WaitStep::Continue
        );
    }

    #[test]
    fn try_wait_step_returns_ready_after_fire_and_consumes_flag() {
        let t = IrqTable::new(31);
        let ctl = MockController::ok();
        let out = t.bind(7, TaskId(42)).unwrap();
        assert_eq!(t.fire(7, &ctl), Ok(FireOutcome::Marked));
        // First poll consumes the ready flag.
        assert_eq!(
            t.try_wait_step(out.handle, TaskId(42), 0, 1_000),
            WaitStep::Ready
        );
        // Second poll without another fire is Continue.
        assert_eq!(
            t.try_wait_step(out.handle, TaskId(42), 0, 1_000),
            WaitStep::Continue
        );
    }

    #[test]
    fn try_wait_step_returns_timed_out_when_now_meets_deadline() {
        let t = IrqTable::new(31);
        let out = t.bind(7, TaskId(42)).unwrap();
        assert_eq!(
            t.try_wait_step(out.handle, TaskId(42), 1_000, 1_000),
            WaitStep::TimedOut
        );
    }

    #[test]
    fn try_wait_step_returns_not_found_on_forged_handle() {
        let t = IrqTable::new(31);
        // No bind: any handle is unknown.
        assert_eq!(
            t.try_wait_step(IrqHandle::from_raw(0xDEAD_BEEF), TaskId(42), 0, 1_000),
            WaitStep::NotFound
        );
    }

    #[test]
    fn try_wait_step_returns_not_found_on_handle_minted_for_another_task() {
        let t = IrqTable::new(31);
        let out = t.bind(7, TaskId(42)).unwrap();
        // Same handle, different caller — forgery defence.
        assert_eq!(
            t.try_wait_step(out.handle, TaskId(99), 0, 1_000),
            WaitStep::NotFound
        );
    }

    #[test]
    fn ready_beats_timeout_in_a_tie() {
        let t = IrqTable::new(31);
        let ctl = MockController::ok();
        let out = t.bind(7, TaskId(42)).unwrap();
        t.fire(7, &ctl).unwrap();
        // The wake-up happened; even though `now == deadline` we
        // must surface `Ready` rather than `TimedOut`.
        assert_eq!(
            t.try_wait_step(out.handle, TaskId(42), 1_000, 1_000),
            WaitStep::Ready
        );
    }

    #[test]
    fn fire_returns_stray_when_no_binding_but_still_masks() {
        let t = IrqTable::new(31);
        let ctl = MockController::ok();
        assert_eq!(t.fire(7, &ctl), Ok(FireOutcome::Stray));
        // Mask still happened — the controller-level write is
        // load-bearing even for stray edges.
        assert_eq!(ctl.calls(), std::vec![7]);
    }

    #[test]
    fn fire_returns_arch_unsupported_when_controller_unsupported() {
        let t = IrqTable::new(31);
        let ctl = MockController::unsupported();
        let _ = t.bind(7, TaskId(42)).unwrap();
        assert_eq!(t.fire(7, &ctl), Err(IrqError::ArchUnsupported));
    }

    #[test]
    fn fire_returns_out_of_range_when_controller_rejects_line() {
        let t = IrqTable::new(31);
        let ctl = MockController::with_max(5);
        assert_eq!(t.fire(7, &ctl), Err(IrqError::LineOutOfRange));
    }

    #[test]
    fn mask_is_observed_before_wake() {
        // Mask-before-wake invariant: the controller's `mask`
        // must observe the line *before* the per-entry `ready`
        // flag is set. The mock records `mask` calls; we verify
        // the entry's `ready` flag is still `false` while
        // `controller.mask` is executing by snapshotting state
        // through a `RefCell` interlock.
        struct OrderingProbe<'a> {
            table: &'a IrqTable,
            line: u32,
            observed_ready_during_mask: RefCell<Option<bool>>,
        }
        impl IrqController for OrderingProbe<'_> {
            fn mask(&self, _line: u32) -> Result<(), MaskError> {
                // Read the table's current ready flag for `line`
                // *while* the mask is in flight. If the table set
                // `ready = true` before calling us, this test
                // fails.
                let bound = self.table.bound.get(self.line as usize).is_some();
                *self.observed_ready_during_mask.borrow_mut() =
                    bound.then(|| self.table.ready_flag(self.line));
                Ok(())
            }
        }
        let t = IrqTable::new(31);
        let _ = t.bind(7, TaskId(42)).unwrap();
        let probe = OrderingProbe {
            table: &t,
            line: 7,
            observed_ready_during_mask: RefCell::new(None),
        };
        t.fire(7, &probe).unwrap();
        assert_eq!(
            *probe.observed_ready_during_mask.borrow(),
            Some(false),
            "ready must still be false while mask is executing"
        );
        // And after fire returns, ready is set.
        assert!(t.ready_flag(7));
    }

    #[test]
    fn release_for_evicts_bindings_and_returns_subsequent_wait_with_not_found() {
        let t = IrqTable::new(31);
        let a = t.bind(7, TaskId(42)).unwrap();
        let b = t.bind(9, TaskId(42)).unwrap();
        let c = t.bind(10, TaskId(99)).unwrap();
        assert_eq!(t.release_for(TaskId(42)).released, 2);
        // Releases are idempotent.
        assert_eq!(t.release_for(TaskId(42)).released, 0);
        // 42's handles are now unknown.
        assert_eq!(
            t.try_wait_step(a.handle, TaskId(42), 0, 1_000),
            WaitStep::NotFound
        );
        assert_eq!(
            t.try_wait_step(b.handle, TaskId(42), 0, 1_000),
            WaitStep::NotFound
        );
        // 99's binding survives.
        assert_eq!(
            t.try_wait_step(c.handle, TaskId(99), 0, 1_000),
            WaitStep::Continue
        );
    }

    #[test]
    fn lookup_returns_none_for_unknown_handle() {
        let t = IrqTable::new(31);
        assert!(t.lookup(IrqHandle::from_raw(0xDEAD)).is_none());
    }

    #[test]
    fn len_and_is_empty_track_bindings() {
        let t = IrqTable::new(31);
        assert!(t.is_empty());
        let _ = t.bind(7, TaskId(1)).unwrap();
        assert_eq!(t.len(), 1);
        assert!(!t.is_empty());
        let _ = t.bind(8, TaskId(2)).unwrap();
        assert_eq!(t.len(), 2);
        t.release_for(TaskId(1));
        assert_eq!(t.len(), 1);
        t.release_for(TaskId(2));
        assert!(t.is_empty());
    }

    #[test]
    fn max_line_is_reported() {
        let t = IrqTable::new(23);
        assert_eq!(t.max_line(), 23);
    }

    #[test]
    fn handles_are_unique_across_rebinds() {
        let t = IrqTable::new(31);
        let a = t.bind(7, TaskId(1)).unwrap();
        t.release_for(TaskId(1));
        let b = t.bind(7, TaskId(1)).unwrap();
        assert_ne!(a.handle, b.handle, "fresh bind must mint a fresh handle");
    }

    use core::sync::atomic::{AtomicU32, AtomicU64};

    /// Test observer: counts calls and remembers the last line, so a test can
    /// assert `fire` notified it. Interior-atomic so it is `Sync`, mirroring
    /// the production entropy observer's shape.
    struct CountingObserver {
        calls: AtomicU32,
        last_line: AtomicU64,
    }

    impl CountingObserver {
        fn new() -> Self {
            Self {
                calls: AtomicU32::new(0),
                last_line: AtomicU64::new(u64::MAX),
            }
        }
    }

    impl IrqDispatchObserver for CountingObserver {
        fn on_irq(&self, line: u32) {
            self.calls.fetch_add(1, Ordering::Relaxed);
            self.last_line.store(u64::from(line), Ordering::Relaxed);
        }
    }

    #[test]
    fn observer_is_notified_on_every_fire_including_strays() {
        let t = IrqTable::new(31);
        let obs: &'static CountingObserver =
            alloc::boxed::Box::leak(alloc::boxed::Box::new(CountingObserver::new()));
        t.set_observer(obs).expect("first install succeeds");
        let ctl = MockController::ok();
        // Bound line: fire notifies the observer.
        let _ = t.bind(7, TaskId(1)).unwrap();
        t.fire(7, &ctl).expect("fire bound line");
        assert_eq!(obs.calls.load(Ordering::Relaxed), 1);
        assert_eq!(obs.last_line.load(Ordering::Relaxed), 7);
        // Stray (unbound) line: still an arrival, still fed to the observer.
        assert_eq!(t.fire(9, &ctl), Ok(FireOutcome::Stray));
        assert_eq!(obs.calls.load(Ordering::Relaxed), 2);
        assert_eq!(obs.last_line.load(Ordering::Relaxed), 9);
    }

    #[test]
    fn set_observer_is_set_once() {
        let t = IrqTable::new(31);
        let a: &'static CountingObserver =
            alloc::boxed::Box::leak(alloc::boxed::Box::new(CountingObserver::new()));
        let b: &'static CountingObserver =
            alloc::boxed::Box::leak(alloc::boxed::Box::new(CountingObserver::new()));
        assert_eq!(t.set_observer(a), Ok(()));
        assert_eq!(t.set_observer(b), Err(ObserverAlreadyInstalled));
    }

    #[test]
    fn fire_without_observer_is_a_noop() {
        // The observer is optional: a table with none installed fires exactly
        // as before (no panic, correct outcome).
        let t = IrqTable::new(31);
        let ctl = MockController::ok();
        let _ = t.bind(3, TaskId(1)).unwrap();
        assert_eq!(t.fire(3, &ctl), Ok(FireOutcome::Marked));
    }

    /// Test monotonic clock with a settable time, so the runaway-line
    /// rate accounting can be driven deterministically. Interior-atomic so
    /// it is `Sync`, mirroring the production clock's shape.
    struct MockClock {
        now: AtomicU64,
    }

    impl MockClock {
        fn at(now: u64) -> Self {
            Self {
                now: AtomicU64::new(now),
            }
        }
        fn set(&self, now: u64) {
            self.now.store(now, Ordering::Relaxed);
        }
    }

    impl MonotonicClock for MockClock {
        fn now_ns(&self) -> u64 {
            self.now.load(Ordering::Relaxed)
        }
    }

    fn leak_clock(now: u64) -> &'static MockClock {
        alloc::boxed::Box::leak(alloc::boxed::Box::new(MockClock::at(now)))
    }

    #[test]
    fn without_a_clock_a_line_is_never_quarantined() {
        // Rate accounting is inert until a clock is installed: even far past
        // the budget every fire is delivered exactly as before.
        let t = IrqTable::new(31);
        let ctl = MockController::ok();
        let out = t.bind(7, TaskId(1)).unwrap();
        for _ in 0..(STORM_FIRE_BUDGET + 10) {
            assert_eq!(t.fire(7, &ctl), Ok(FireOutcome::Marked));
        }
        assert_eq!(
            t.try_wait_step(out.handle, TaskId(1), 0, u64::MAX),
            WaitStep::Ready
        );
        assert!(!t.is_quarantined(out.handle, TaskId(1)));
    }

    #[test]
    fn a_line_firing_past_its_budget_in_one_window_is_quarantined() {
        let t = IrqTable::new(31);
        t.set_clock(leak_clock(0)).expect("clock installs once");
        let ctl = MockController::ok();
        let out = t.bind(7, TaskId(1)).unwrap();
        // Every fire lands at the same instant (one window). The budget-th
        // is still delivered; the one past it trips the quarantine.
        for _ in 0..STORM_FIRE_BUDGET {
            assert_eq!(t.fire(7, &ctl), Ok(FireOutcome::Marked));
        }
        assert_eq!(t.fire(7, &ctl), Ok(FireOutcome::Quarantined));
        // The tripping fire delivered no wake, and the waiter observes the
        // terminal, fail-closed quarantine rather than a ready or a spin.
        assert!(t.is_quarantined(out.handle, TaskId(1)));
        assert_eq!(
            t.try_wait_step(out.handle, TaskId(1), 0, u64::MAX),
            WaitStep::Quarantined
        );
        // A quarantined line keeps reporting quarantined and never re-delivers.
        assert_eq!(t.fire(7, &ctl), Ok(FireOutcome::Quarantined));
    }

    #[test]
    fn fires_spread_across_windows_never_trip_the_budget() {
        // A busy-but-healthy line: it fires the full budget, then the window
        // rolls over and the count resets, so it is never quarantined.
        let t = IrqTable::new(31);
        let clock = leak_clock(0);
        t.set_clock(clock).expect("clock installs once");
        let ctl = MockController::ok();
        let out = t.bind(7, TaskId(1)).unwrap();
        for window in 0..4u64 {
            clock.set(window * STORM_WINDOW_NS);
            for _ in 0..STORM_FIRE_BUDGET {
                assert_eq!(t.fire(7, &ctl), Ok(FireOutcome::Marked));
                // Consume the ready flag each time, mirroring a driver that
                // services every interrupt (so `fire` re-sets it next round).
                let _ = t.try_wait_step(out.handle, TaskId(1), 0, u64::MAX);
            }
        }
        assert!(!t.is_quarantined(out.handle, TaskId(1)));
    }

    #[test]
    fn a_fresh_bind_clears_a_previous_quarantine() {
        let t = IrqTable::new(31);
        t.set_clock(leak_clock(0)).expect("clock installs once");
        let ctl = MockController::ok();
        let first = t.bind(7, TaskId(1)).unwrap();
        for _ in 0..=STORM_FIRE_BUDGET {
            let _ = t.fire(7, &ctl);
        }
        assert!(t.is_quarantined(first.handle, TaskId(1)));
        // The driver releases and rebinds the line to recover it.
        t.release_for(TaskId(1));
        let second = t.bind(7, TaskId(1)).unwrap();
        assert!(!t.is_quarantined(second.handle, TaskId(1)));
        assert_eq!(t.fire(7, &ctl), Ok(FireOutcome::Marked));
        assert_eq!(
            t.try_wait_step(second.handle, TaskId(1), 0, u64::MAX),
            WaitStep::Ready
        );
    }

    #[test]
    fn is_quarantined_owner_checks_and_ignores_unknown_handles() {
        let t = IrqTable::new(31);
        t.set_clock(leak_clock(0)).expect("clock installs once");
        let ctl = MockController::ok();
        let out = t.bind(7, TaskId(1)).unwrap();
        for _ in 0..=STORM_FIRE_BUDGET {
            let _ = t.fire(7, &ctl);
        }
        // Owner sees the quarantine; a foreign task and a forged handle do
        // not (identify before acting).
        assert!(t.is_quarantined(out.handle, TaskId(1)));
        assert!(!t.is_quarantined(out.handle, TaskId(2)));
        assert!(!t.is_quarantined(IrqHandle::from_raw(0xDEAD), TaskId(1)));
    }

    #[test]
    fn set_clock_is_set_once() {
        let t = IrqTable::new(31);
        assert_eq!(t.set_clock(leak_clock(0)), Ok(()));
        assert_eq!(t.set_clock(leak_clock(0)), Err(ObserverAlreadyInstalled));
    }
}
