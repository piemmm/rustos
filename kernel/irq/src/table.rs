//! [`IrqTable`] — kernel IRQ binding table and per-handle ready
//! flag.
//!
//! See the crate-level docs for the design rationale; this module
//! is the implementation.

use alloc::collections::BTreeMap;

use rustos_abi::IrqHandle;
use rustos_kernel_sec::TaskId;
use rustos_kernel_sync::RwLock;

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
    /// Whether the line has fired since the last consume.
    ///
    /// `IrqTable::try_wait_step` clears this on a successful
    /// consume; `IrqTable::fire` sets it after masking the line
    /// at the controller.
    pub ready: bool,
}

/// Controller-mask seam.
///
/// The production [`IrqTable::fire`] path calls
/// [`Self::mask`] before it sets [`IrqEntry::ready`] — the
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
/// [`rustos_kernel_core::KernelArch::irq_routing`] on architectures
/// or boot paths that have not yet installed a real controller.
///
/// Exposed as a `pub static` (not a `const`) so callers can take a
/// `&'static (dyn IrqController + Send + Sync)` reference without
/// risking the `clippy::declare_interior_mutable_const` footgun. The
/// unit-like type has no interior mutability, so the lint does not
/// fire here, but the `static` form keeps the address stable and
/// allows the type-erased reference to round-trip through the
/// [`rustos_kernel_core`] handover without surprise. `AGENTS.md`
/// §2.1 — no global mutable state; this is an *immutable* static.
pub static UNSUPPORTED_CONTROLLER: UnsupportedController = UnsupportedController;

/// Outcome of [`IrqTable::release_for`].
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct ReleaseOutcome {
    /// Number of bindings the call dropped.
    pub released: usize,
}

/// Kernel IRQ table.
///
/// One per running kernel. Interior synchronisation through a
/// writer-preference [`RwLock`] mirroring the `CapTable`
/// lock-ordering policy (`AGENTS.md` §2.1 — no global mutable
/// static; the table is owned by `KernelState`, which itself lives
/// for the lifetime of the running kernel).
#[derive(Debug)]
pub struct IrqTable {
    inner: RwLock<Inner>,
    max_line: u32,
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
    /// state is touched (`AGENTS.md` §5.4.5 — fail closed).
    #[must_use]
    pub fn new(max_line: u32) -> Self {
        Self {
            inner: RwLock::new(Inner {
                next_handle: 1,
                entries: BTreeMap::new(),
                by_handle: BTreeMap::new(),
            }),
            max_line,
        }
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
        // live handle (`AGENTS.md` §5.4.5).
        g.next_handle = g.next_handle.saturating_add(1);
        let handle = IrqHandle::from_raw(raw);
        let entry = IrqEntry {
            handle,
            owner,
            line,
            ready: false,
        };
        g.entries.insert(line, entry);
        g.by_handle.insert(raw, line);
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
    /// The check order is documented to make the forgery /
    /// timeout /  ready hierarchy explicit:
    ///
    /// 1. Look up by handle. If the handle is unknown or its
    ///    binding's owner is not `caller`, return
    ///    [`WaitStep::NotFound`]. The forgery check beats every
    ///    other check (`AGENTS.md` §5.4 — identify before any
    ///    state-touching transition).
    /// 2. If `ready` is set, clear it and return
    ///    [`WaitStep::Ready`]. The ready flag wins over a
    ///    near-simultaneous deadline because the wake-up did
    ///    happen — surfacing `TimedOut` here would silently drop
    ///    a successful interrupt.
    /// 3. If `now_ns >= deadline_ns`, return
    ///    [`WaitStep::TimedOut`].
    /// 4. Otherwise [`WaitStep::Continue`].
    #[must_use]
    pub fn try_wait_step(
        &self,
        handle: IrqHandle,
        caller: TaskId,
        now_ns: u64,
        deadline_ns: u64,
    ) -> WaitStep {
        let mut g = self.inner.write();
        let raw = handle.as_u64();
        let Some(&line) = g.by_handle.get(&raw) else {
            return WaitStep::NotFound;
        };
        let Some(entry) = g.entries.get_mut(&line) else {
            // by_handle and entries are kept consistent; this is a
            // belt-and-braces fail-closed (`AGENTS.md` §5.4.5).
            return WaitStep::NotFound;
        };
        if entry.owner != caller {
            return WaitStep::NotFound;
        }
        if entry.ready {
            entry.ready = false;
            return WaitStep::Ready;
        }
        if now_ns >= deadline_ns {
            return WaitStep::TimedOut;
        }
        WaitStep::Continue
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
    ///   condition, but routed to a stable errno (`AGENTS.md`
    ///   §5.4.5 — fail closed, never panic).
    pub fn fire(&self, line: u32, controller: &dyn IrqController) -> Result<FireOutcome, IrqError> {
        controller.mask(line).map_err(|e| match e {
            MaskError::Unsupported => IrqError::ArchUnsupported,
            MaskError::OutOfRange => IrqError::LineOutOfRange,
        })?;
        let mut g = self.inner.write();
        let Some(entry) = g.entries.get_mut(&line) else {
            // No binding — the mask still happened, the stray edge
            // is contained. Surface to the caller so an arch-port
            // audit observer can record stray-IRQ rate.
            return Ok(FireOutcome::Stray);
        };
        entry.ready = true;
        Ok(FireOutcome::Marked)
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
        assert!(!entry.ready);
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
                let g = self.table.inner.read();
                let entry = g.entries.get(&self.line).copied();
                *self.observed_ready_during_mask.borrow_mut() = entry.map(|e| e.ready);
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
        let entry = t.lookup(IrqHandle::from_raw(1)).expect("entry");
        assert!(entry.ready);
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
}
