//! Reporting this process's own reclaimable-cache ledgers to the System
//! Information service (`plans/SMARTRAM.md`).
//!
//! [`pressure`](crate::pressure) carries the memory-pressure band *in*, from
//! the System Information service to every cache in the process; this
//! module carries the resulting cache figures back *out*, from every cache
//! in the process to the service. The two are deliberately symmetric: the
//! runtime holds the one process-wide thing (there, a gauge; here, a set of
//! registered ledgers), and the owning program drives it (there, by calling
//! [`pressure::report`](crate::pressure::report) after reading the band;
//! here, by calling [`publish_if_due`] from its event loop).
//!
//! # Why a process must say this itself
//!
//! The kernel's own caches are visible to
//! [`SysinfoQueryId::CACHE_LEDGERS`](tairix_abi::sysinfo::SysinfoQueryId::CACHE_LEDGERS)
//! because the kernel can read them directly. A glyph atlas in a GUI
//! process's font client, or decoded icon artwork in the desktop session,
//! is memory only that process can see — nobody outside it can sample the
//! counters. [`SysinfoQueryId::CACHE_REPORT`] exists so a process can *say*
//! what it holds, and this module is the one place in the runtime that
//! knows how to say it, so every publisher (`fontd`, `wm`, `taskbar`,
//! `session`, `files`, …) reports identically rather than each hand-rolling
//! the framing, the rate limit, and the change detection.
//!
//! # Never on the caller's critical path
//!
//! A publisher is a compositor, a file manager, or the font service — each
//! owing somebody a frame or an answer — so this reports the figures with a
//! [`crate::submit::Submission`]: posted and collected later, never awaited. A
//! blocking `ipc_call` here parked the caller off the run queue for a full
//! cross-process round trip, four times a second, which the desktop showed as
//! a stutter through every gesture.
//!
//! # Event-driven, not polled
//!
//! There are no caller-side dirty flags and no background timer. A cache
//! that never changes costs nothing beyond the one sample [`publish_if_due`]
//! takes each time it is called, because comparing that sample against the
//! rows already sent *is* the change detection — at most sixteen 128-byte
//! records, cheaper than maintaining a flag on every mutation path a cache
//! has. [`wait_deadline_ns`] is what keeps the caller from polling for the
//! next chance to send: it is `0` whenever nothing is waiting to go out, so
//! an unchanged process arms nothing at all, and it is a small positive
//! bound only while a change is being held back by the rate limiter, so the
//! caller parks for exactly that long and no longer.
//!
//! # Using it
//!
//! A publisher registers each cache's [`CacheLedger`] once, then drives the
//! reporter from its event loop, folding [`wait_deadline_ns`] into whatever
//! timeout the loop's own wait-set park would otherwise use
//! ([`fold_wait_deadline_ns`]) so a suppressed change is flushed without
//! ever turning an indefinite park into a poll, and holding a
//! [`ReportGuard`] so the rows are withdrawn on every way out:
//!
//! ```ignore
//! let _rows = tairix_rt::cachereport::ReportGuard;
//! tairix_rt::cachereport::register(cache.ledger().expect("classified"));
//! loop {
//!     let timeout_ns = tairix_rt::cachereport::fold_wait_deadline_ns(own_timeout_ns);
//!     wait(timeout_ns);
//!     // ... handle whatever the wait woke for ...
//!     tairix_rt::cachereport::publish_if_due();
//! }
//! ```

use alloc::vec::Vec;

use tairix_abi::sysinfo::{
    decode_reply, encode_request, CacheLedgerRecord, CacheReportRequest, SysinfoQueryId,
    MAX_CACHE_REPORT_ENTRIES, SYSINFO_ENDPOINT, SYSINFO_MAX_REPLY, SYSINFO_MAX_REQUEST,
};
use tairix_abi::Errno;
use tairix_reclaim::CacheLedger;
use tairix_sync::SpinLock;

use crate::submit::Submission;

/// Minimum time between two publish attempts for the same process.
///
/// A GUI process can churn its cache counters many times a second (every
/// glyph draw, every icon decode), and reporting every single change would
/// turn a redraw storm into an IPC storm. 250 ms sits comfortably below the
/// fastest refresh a cache monitor plausibly polls at — a human watching a
/// live gauge cannot perceive staleness that short — while capping this
/// process at a handful of report calls a second even under continuous
/// churn, which is what makes the rate limit worth having at all.
const MIN_SEND_INTERVAL_NS: u64 = 250_000_000;

/// Bytes of the largest request [`CacheReportRequest`] payload this process
/// ever needs to encode: the header plus a full complement of rows.
const MAX_REPORT_PAYLOAD: usize =
    CacheReportRequest::WIRE_LEN + MAX_CACHE_REPORT_ENTRIES * CacheLedgerRecord::WIRE_LEN;

/// The monotonic clock the rate limiter times against.
///
/// A seam so the rate limit is host-testable against a clock a test
/// advances by hand, never a real one.
trait Clock {
    /// A monotonically non-decreasing nanosecond reading.
    fn now_ns(&self) -> u64;
}

/// Carries this process's reports to the sysinfo endpoint.
///
/// A seam so the gate below is host-testable against a recorder that never
/// touches a kernel, never a real transport. The periodic report is *posted*
/// and its verdict collected later, because a publisher owes its own callers a
/// frame; only the withdrawal waits, and [`send`](Self::send) exists for it
/// alone.
trait Sender {
    /// Hand `request` to the service without waiting for it.
    ///
    /// # Errors
    ///
    /// [`Errno::WouldBlock`] when a report is already in flight, else whatever
    /// the hand-off refused.
    fn post(&mut self, request: &[u8]) -> Result<(), Errno>;

    /// The service's verdict on the report handed over earlier, or `None`
    /// while it is unanswered and when there is none.
    fn settle(&mut self) -> Option<Result<(), Errno>>;

    /// Withdraw whatever is in flight, so nothing it carries can be recorded
    /// after what the caller does next.
    fn abandon(&mut self);

    /// Send `request` and wait for its answer, writing the reply into `reply`
    /// and returning the number of bytes written.
    ///
    /// The withdrawal alone uses this. The kernel drops a posted request whose
    /// poster has exited, so a withdrawal that did not wait would be lost
    /// exactly when it matters — as the program goes.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the transport or the service surfaces.
    fn send(&mut self, request: &[u8], reply: &mut [u8]) -> Result<usize, Errno>;
}

/// The process-wide reporter state: the registered caches, the last rows
/// successfully sent, and the bookkeeping the rate limiter needs.
struct Registry {
    /// Registered caches, in registration order. Bounded by
    /// [`MAX_CACHE_REPORT_ENTRIES`]; a process with more reclaimable caches
    /// than the wire report admits keeps the ones it registered first.
    ledgers: Vec<CacheLedger>,
    /// The rows the service is known to hold. Compared against a fresh
    /// sample on every [`publish_if_due`] call — this comparison *is* the
    /// change detection, so no caller-side dirty flag is needed.
    last_sent: Vec<CacheLedgerRecord>,
    /// The rows a posted report carries while its verdict is outstanding.
    /// They become [`last_sent`](Self::last_sent) once the service has
    /// accepted them, and are dropped if it refused — so a refused report
    /// never leaves this gate believing a figure was recorded.
    in_flight: Vec<CacheLedgerRecord>,
    /// When the last send was attempted, successful or not. `None` before
    /// the first attempt, so that attempt is never held back by the rate
    /// limit.
    last_attempt_ns: Option<u64>,
    /// Whether this gate still owes something: a sampled change the rate
    /// limiter is holding back, a hand-off that was refused, or a posted
    /// report whose verdict has yet to be collected. Drives
    /// [`wait_deadline_ns`], so each of those gets exactly one wake.
    pending: bool,
}

impl Registry {
    const fn new() -> Self {
        Self {
            ledgers: Vec::new(),
            last_sent: Vec::new(),
            in_flight: Vec::new(),
            last_attempt_ns: None,
            pending: false,
        }
    }

    /// Register `ledger`, replacing an existing registration with the same
    /// label and otherwise appending, up to [`MAX_CACHE_REPORT_ENTRIES`].
    fn register(&mut self, ledger: CacheLedger) {
        if let Some(slot) = self
            .ledgers
            .iter_mut()
            .find(|l| l.label() == ledger.label())
        {
            *slot = ledger;
            return;
        }
        if self.ledgers.len() >= MAX_CACHE_REPORT_ENTRIES {
            // A process with more reclaimable caches than the report admits
            // is a design smell in that process, not a reason for this
            // registry to grow past the wire bound every reader relies on.
            return;
        }
        self.ledgers.push(ledger);
    }

    /// Sample every registered cache into its wire record, skipping any
    /// whose label the wire format refuses rather than failing the whole
    /// report over one bad cache.
    fn sample(&self) -> Vec<CacheLedgerRecord> {
        self.ledgers
            .iter()
            .filter_map(|ledger| ledger.to_record().ok())
            .collect()
    }
}

/// The one process-wide registry every registered cache in this process
/// shares.
static REGISTRY: SpinLock<Registry> = SpinLock::new(Registry::new());

/// Register `ledger` with the process-wide cache reporter.
///
/// Re-registering the same label replaces the previous entry rather than
/// adding a second one, so a cache that is rebuilt (a font client
/// reinstalling its default cache, say) does not double-count itself in the
/// next report. Silently bounded at [`MAX_CACHE_REPORT_ENTRIES`]: a
/// registration past that bound is dropped, keeping whichever caches
/// registered first.
pub fn register(ledger: CacheLedger) {
    REGISTRY.lock().register(ledger);
}

/// Register `ledger` against an arbitrary registry, so a test can exercise
/// registration against its own isolated instance rather than the
/// process-wide [`REGISTRY`] every other test would also touch.
#[cfg(test)]
fn register_into(registry: &SpinLock<Registry>, ledger: CacheLedger) {
    registry.lock().register(ledger);
}

/// Encode `rows` as a [`SysinfoQueryId::CACHE_REPORT`] request into
/// `request`, answering its framed length.
fn frame_report(
    rows: &[CacheLedgerRecord],
    request: &mut [u8; SYSINFO_MAX_REQUEST],
) -> Result<usize, Errno> {
    let header = CacheReportRequest {
        count: u16::try_from(rows.len()).map_err(|_| Errno::LengthOutOfRange)?,
        flags: 0,
        reserved: 0,
    };
    let mut payload = [0u8; MAX_REPORT_PAYLOAD];
    payload[..CacheReportRequest::WIRE_LEN].copy_from_slice(&header.to_le_bytes());
    let mut offset = CacheReportRequest::WIRE_LEN;
    for row in rows {
        let end = offset + CacheLedgerRecord::WIRE_LEN;
        payload[offset..end].copy_from_slice(&row.to_le_bytes());
        offset = end;
    }
    encode_request(
        SysinfoQueryId::CACHE_REPORT,
        &payload[..offset],
        request.as_mut_slice(),
    )
}

/// Hand `rows` to the service without waiting for it.
fn post_report(rows: &[CacheLedgerRecord], sender: &mut dyn Sender) -> Result<(), Errno> {
    let mut request = [0u8; SYSINFO_MAX_REQUEST];
    let len = frame_report(rows, &mut request)?;
    sender.post(&request[..len])
}

/// Send `rows` and wait for the service's answer, unwrapping the
/// (payload-less) reply frame. The withdrawal's path alone.
fn send_report(rows: &[CacheLedgerRecord], sender: &mut dyn Sender) -> Result<(), Errno> {
    let mut request = [0u8; SYSINFO_MAX_REQUEST];
    let len = frame_report(rows, &mut request)?;
    let mut reply = [0u8; SYSINFO_MAX_REPLY];
    let reply_len = sender.send(&request[..len], &mut reply)?;
    decode_reply(&reply[..reply_len])?;
    Ok(())
}

/// Whether a freshly sampled report should be sent now, suppressed by the
/// rate limiter, or dropped because nothing changed.
enum Action {
    NoChange,
    Send,
    Suppressed,
}

/// The shared logic behind [`publish_if_due`], against an injected registry
/// and injected seams.
fn publish_if_due_with(registry: &SpinLock<Registry>, clock: &dyn Clock, sender: &mut dyn Sender) {
    // What the service made of the last report decides what it now holds, so
    // it is collected before a fresh sample is compared against that. A
    // refusal drops the rows it carried rather than adopting them, leaving the
    // change to be restated.
    if let Some(outcome) = sender.settle() {
        let mut state = registry.lock();
        let carried = core::mem::take(&mut state.in_flight);
        if outcome.is_ok() {
            state.last_sent = carried;
        }
    }

    let sampled = registry.lock().sample();
    let now = clock.now_ns();

    let action = {
        let mut state = registry.lock();
        if sampled == state.last_sent {
            // Nothing new to say, but a report still in flight is still owed
            // a collection, and that is what arms the wake for it.
            state.pending = !state.in_flight.is_empty();
            Action::NoChange
        } else if state
            .last_attempt_ns
            .is_some_and(|last| now.saturating_sub(last) < MIN_SEND_INTERVAL_NS)
        {
            state.pending = true;
            Action::Suppressed
        } else {
            Action::Send
        }
    };

    let Action::Send = action else {
        return;
    };

    // A refused hand-off (the service not yet up, a report still in flight,
    // any `Errno`) is never retried in a loop: the change stays pending for
    // the next call, and `last_attempt_ns` advances so the rate limiter holds
    // off the next attempt exactly as it would for a suppressed one. Retrying
    // immediately would turn a dead or overloaded service into the busy loop
    // this design exists to avoid.
    let posted = post_report(&sampled, sender).is_ok();
    let mut state = registry.lock();
    state.last_attempt_ns = Some(now);
    // A posted report is not yet what the service holds, so the change stays
    // pending until its verdict lands — one wake, which collects it.
    state.pending = true;
    state.in_flight = if posted { sampled } else { Vec::new() };
}

/// The shared logic behind [`wait_deadline_ns`], against an injected
/// registry and clock.
fn wait_deadline_ns_with(registry: &SpinLock<Registry>, clock: &dyn Clock) -> u64 {
    let state = registry.lock();
    if !state.pending {
        return 0;
    }
    let Some(last_attempt_ns) = state.last_attempt_ns else {
        // `pending` is only ever set alongside a recorded attempt; this is
        // unreachable in practice, and failing closed to "nothing to wait
        // for" is the safe reading if it is ever somehow not.
        return 0;
    };
    let elapsed = clock.now_ns().saturating_sub(last_attempt_ns);
    MIN_SEND_INTERVAL_NS.saturating_sub(elapsed)
}

/// The shared logic behind [`withdraw`], against an injected registry and
/// sender.
fn withdraw_with(registry: &SpinLock<Registry>, sender: &mut dyn Sender) {
    // A report posted moments ago would otherwise be recorded *after* this
    // withdrawal and resurrect the very rows it removes, so it is withdrawn
    // first and its figures forgotten.
    sender.abandon();
    registry.lock().in_flight = Vec::new();
    // Withdrawal is a deliberate, one-shot action a program takes as it
    // tears its caches down, not a background flush, so it always attempts
    // the send now rather than waiting on the rate limiter — and it is the
    // one report that waits for its answer, because the kernel drops a
    // posted request whose poster has exited.
    if send_report(&[], sender).is_ok() {
        let mut state = registry.lock();
        state.ledgers.clear();
        state.last_sent = Vec::new();
        state.pending = false;
    }
}

/// Sample every registered cache and send a report if, and only if, the
/// sampled rows differ from the rows already sent and the minimum send
/// interval has elapsed since the last attempt.
///
/// This is the whole rate-limit and change-detection design: there is no
/// dirty flag to set on a mutation path, because comparing the fresh sample
/// against the last sent one *is* the change detection, and it costs one
/// sample plus a comparison of at most [`MAX_CACHE_REPORT_ENTRIES`]
/// 128-byte records. A process with nothing registered, that has never
/// sent anything, does nothing here — never an empty report on every loop
/// iteration.
///
/// Call this once per iteration of the owning program's event loop, then
/// pass [`wait_deadline_ns`] as the loop's wait timeout.
pub fn publish_if_due() {
    publish_if_due_with(&REGISTRY, &SyscallClock, &mut *CHANNEL.lock());
}

/// Nanoseconds until a change currently held back by the rate limiter may
/// be sent, or `0` when nothing is pending.
///
/// A caller passes this as the `timeout_ns` of its `waitset_wait`. `0`
/// means the caller may wait indefinitely on its other wait-set members —
/// an unchanged process arms nothing here at all — while a positive value
/// arms exactly one bounded, one-shot wait, after which the next loop
/// iteration's [`publish_if_due`] flushes the change. This is what keeps
/// the design event-driven: no program using this API ever polls.
#[must_use]
pub fn wait_deadline_ns() -> u64 {
    wait_deadline_ns_with(&REGISTRY, &SyscallClock)
}

/// Send an empty report, removing this process's rows from the registry a
/// monitor reads.
///
/// Call this as the owning program tears its caches down, so a monitor
/// never keeps showing memory nobody holds anymore.
pub fn withdraw() {
    withdraw_with(&REGISTRY, &mut *CHANNEL.lock());
}

/// Withdraws this process's reported rows when it drops, so every way out of
/// a program — a clean return, a fail-loud exit, a panic unwind — takes the
/// rows with it rather than only the paths a future edit remembers to spell.
///
/// Held for the scope in which the process's caches are registered. Every
/// publisher uses this one guard rather than its own copy of the same three
/// lines.
pub struct ReportGuard;

impl Drop for ReportGuard {
    fn drop(&mut self) {
        withdraw();
    }
}

/// The shared logic behind [`fold_wait_deadline_ns`], against an injected
/// registry and clock.
fn fold_wait_deadline_ns_with(
    loop_timeout_ns: u64,
    registry: &SpinLock<Registry>,
    clock: &dyn Clock,
) -> u64 {
    match wait_deadline_ns_with(registry, clock) {
        0 => loop_timeout_ns,
        pending => loop_timeout_ns.min(pending),
    }
}

/// Fold [`wait_deadline_ns`] into a loop's own wait timeout: the smaller of
/// the two, never the larger.
///
/// Every publisher parks on its own wait-set with its own reasons for the
/// timeout it would otherwise pass — an indefinite wait (`u64::MAX`) when it
/// has nothing else to time, or a real deadline of its own (a redraw, a
/// refresh). Folding this in must never *lengthen* that: a suppressed report
/// with nothing pending returns `loop_timeout_ns` unchanged (an indefinite
/// wait stays indefinite), and a pending report only *tightens* the wait to
/// whichever of the two is sooner. This is the one place every publisher
/// (`fontd`, `session`, `files`, …) folds the two together, so the
/// "smaller of the non-zero values" arithmetic exists exactly once.
#[must_use]
pub fn fold_wait_deadline_ns(loop_timeout_ns: u64) -> u64 {
    fold_wait_deadline_ns_with(loop_timeout_ns, &REGISTRY, &SyscallClock)
}

/// The production [`Clock`]: the kernel monotonic clock behind [`crate::clock_get`].
struct SyscallClock;

impl Clock for SyscallClock {
    fn now_ns(&self) -> u64 {
        crate::clock_get()
    }
}

/// The production [`Sender`]: a [`Submission`] to [`SYSINFO_ENDPOINT`] for the
/// periodic report, and one [`crate::ipc_call`] for the withdrawal.
struct RtSender {
    report: Submission,
}

impl RtSender {
    const fn new() -> Self {
        Self {
            // A report the service has not answered within the interval that
            // brings the next one due is one it was never going to answer in
            // time, so it is abandoned rather than blocking the restatement.
            report: Submission::new(MIN_SEND_INTERVAL_NS),
        }
    }
}

impl Sender for RtSender {
    fn post(&mut self, request: &[u8]) -> Result<(), Errno> {
        self.report.post(request)
    }

    fn settle(&mut self) -> Option<Result<(), Errno>> {
        self.report.settle()
    }

    fn abandon(&mut self) {
        self.report.abandon();
    }

    fn send(&mut self, request: &[u8], reply: &mut [u8]) -> Result<usize, Errno> {
        crate::ipc_call(SYSINFO_ENDPOINT, request, reply).map_err(Errno::from_syscall)
    }
}

/// The one channel this process's reports go out over.
///
/// Separate from [`REGISTRY`] because it holds a *ticket* rather than a
/// figure, and because the two are locked in this order — never the other —
/// by the only two callers that take both.
static CHANNEL: SpinLock<RtSender> = SpinLock::new(RtSender::new());

#[cfg(test)]
mod tests {
    use alloc::sync::Arc;
    use core::cell::Cell;

    use tairix_abi::sysinfo::CacheLedgerOrigin;
    use tairix_reclaim::{CacheAccounting, ReclaimClass, ReclaimOwner};

    use super::*;

    /// A clock a test advances by hand.
    struct FakeClock {
        now_ns: Cell<u64>,
    }

    impl FakeClock {
        const fn new() -> Self {
            Self {
                now_ns: Cell::new(0),
            }
        }

        fn advance(&self, delta_ns: u64) {
            self.now_ns.set(self.now_ns.get() + delta_ns);
        }
    }

    impl Clock for FakeClock {
        fn now_ns(&self) -> u64 {
            self.now_ns.get()
        }
    }

    /// What a test arranges for one hand-off.
    #[derive(Clone, Copy)]
    enum Verdict {
        /// Taken, and the service records it.
        Accepted,
        /// Taken, but the service refuses it.
        Refused(Errno),
        /// Not taken at all: the hand-off itself is refused.
        NotTaken(Errno),
    }

    /// A channel that records every hand-off and answers however the test
    /// pre-arms it to. A taken report settles on the very next
    /// [`Sender::settle`], which is the ordinary case.
    struct RecordingSender {
        verdicts: Vec<Verdict>,
        /// Hand-offs attempted, taken or not.
        attempts: usize,
        /// Reports sent the *blocking* way. The periodic report must never
        /// use it: waiting on the service is the stall this design removes.
        blocking_sends: usize,
        /// The verdict a taken report has yet to give.
        in_flight: Option<Result<(), Errno>>,
    }

    impl RecordingSender {
        fn always_ok() -> Self {
            Self {
                verdicts: Vec::new(),
                attempts: 0,
                blocking_sends: 0,
                in_flight: None,
            }
        }

        fn queue(verdicts: Vec<Verdict>) -> Self {
            Self {
                verdicts,
                ..Self::always_ok()
            }
        }

        fn next_verdict(&mut self) -> Verdict {
            if self.verdicts.is_empty() {
                Verdict::Accepted
            } else {
                self.verdicts.remove(0)
            }
        }
    }

    impl Sender for RecordingSender {
        fn post(&mut self, _request: &[u8]) -> Result<(), Errno> {
            self.attempts += 1;
            if self.in_flight.is_some() {
                return Err(Errno::WouldBlock);
            }
            match self.next_verdict() {
                Verdict::Accepted => {
                    self.in_flight = Some(Ok(()));
                    Ok(())
                }
                Verdict::Refused(errno) => {
                    self.in_flight = Some(Err(errno));
                    Ok(())
                }
                Verdict::NotTaken(errno) => Err(errno),
            }
        }

        fn settle(&mut self) -> Option<Result<(), Errno>> {
            self.in_flight.take()
        }

        fn abandon(&mut self) {
            self.in_flight = None;
        }

        fn send(&mut self, _request: &[u8], reply: &mut [u8]) -> Result<usize, Errno> {
            self.attempts += 1;
            self.blocking_sends += 1;
            match self.next_verdict() {
                Verdict::Accepted => {
                    // A real reply carries no payload.
                    let n = tairix_abi::sysinfo::encode_reply_ok(&[], reply)
                        .expect("reply buffer fits");
                    Ok(n)
                }
                Verdict::Refused(errno) | Verdict::NotTaken(errno) => Err(errno),
            }
        }
    }

    /// A fresh, isolated registry for one test. Each test builds its own
    /// rather than sharing [`REGISTRY`], so `cargo test`'s parallel test
    /// threads can never interleave one test's sends into another's.
    fn registry() -> SpinLock<Registry> {
        SpinLock::new(Registry::new())
    }

    fn test_ledger(label: &'static str) -> CacheLedger {
        CacheLedger::new(
            label,
            ReclaimOwner::UserlandProcess("test"),
            ReclaimClass::DisposableUi,
            Arc::new(CacheAccounting::new()),
        )
    }

    #[test]
    fn nothing_registered_and_never_sent_does_nothing() {
        let registry = registry();
        let clock = FakeClock::new();
        let mut sender = RecordingSender::always_ok();
        publish_if_due_with(&registry, &clock, &mut sender);
        assert_eq!(sender.attempts, 0);
        assert_eq!(wait_deadline_ns_with(&registry, &clock), 0);
    }

    #[test]
    fn the_first_sample_is_handed_over_and_never_waited_on() {
        let registry = registry();
        register_into(&registry, test_ledger("a"));
        let clock = FakeClock::new();
        let mut sender = RecordingSender::always_ok();
        publish_if_due_with(&registry, &clock, &mut sender);
        assert_eq!(sender.attempts, 1);
        assert_eq!(
            sender.blocking_sends, 0,
            "the periodic report must never wait on the service"
        );
        assert!(
            wait_deadline_ns_with(&registry, &clock) > 0,
            "a report in flight is owed a collection, and that arms the wake"
        );

        // The next pass collects the verdict, adopts the figure, and finds
        // nothing more owed.
        publish_if_due_with(&registry, &clock, &mut sender);
        assert_eq!(sender.attempts, 1);
        assert_eq!(wait_deadline_ns_with(&registry, &clock), 0);
    }

    #[test]
    fn an_unchanged_sample_is_not_resent() {
        let registry = registry();
        register_into(&registry, test_ledger("a"));
        let clock = FakeClock::new();
        let mut sender = RecordingSender::always_ok();
        publish_if_due_with(&registry, &clock, &mut sender);
        assert_eq!(sender.attempts, 1);
        publish_if_due_with(&registry, &clock, &mut sender);
        assert_eq!(sender.attempts, 1);
    }

    #[test]
    fn a_changed_sample_inside_the_interval_is_suppressed_and_arms_a_deadline() {
        let registry = registry();
        let ledger = test_ledger("a");
        register_into(&registry, ledger.clone());
        let clock = FakeClock::new();
        let mut sender = RecordingSender::always_ok();
        publish_if_due_with(&registry, &clock, &mut sender);
        assert_eq!(sender.attempts, 1);

        ledger
            .accounting()
            .charge(ReclaimClass::DisposableUi, 4096, 0)
            .expect("fresh ledger accepts a charge");
        clock.advance(MIN_SEND_INTERVAL_NS / 2);
        publish_if_due_with(&registry, &clock, &mut sender);
        assert_eq!(sender.attempts, 1, "suppressed inside the interval");
        assert!(wait_deadline_ns_with(&registry, &clock) > 0);
    }

    #[test]
    fn the_deadline_shrinks_as_the_clock_advances_and_reaches_zero_pending_once_flushed() {
        let registry = registry();
        let ledger = test_ledger("a");
        register_into(&registry, ledger.clone());
        let clock = FakeClock::new();
        let mut sender = RecordingSender::always_ok();
        publish_if_due_with(&registry, &clock, &mut sender);

        ledger
            .accounting()
            .charge(ReclaimClass::DisposableUi, 4096, 0)
            .expect("fresh ledger accepts a charge");
        clock.advance(MIN_SEND_INTERVAL_NS / 4);
        publish_if_due_with(&registry, &clock, &mut sender);
        let first = wait_deadline_ns_with(&registry, &clock);
        assert!(first > 0);

        clock.advance(MIN_SEND_INTERVAL_NS / 4);
        publish_if_due_with(&registry, &clock, &mut sender);
        let second = wait_deadline_ns_with(&registry, &clock);
        assert!(second < first, "the deadline shrinks as time passes");

        // Advance past the interval: the next call actually hands it over,
        // and the one after that collects the verdict.
        clock.advance(MIN_SEND_INTERVAL_NS);
        publish_if_due_with(&registry, &clock, &mut sender);
        assert_eq!(sender.attempts, 2);
        publish_if_due_with(&registry, &clock, &mut sender);
        assert_eq!(wait_deadline_ns_with(&registry, &clock), 0);
    }

    #[test]
    fn a_changed_sample_after_the_interval_is_handed_over_and_clears_the_deadline() {
        let registry = registry();
        let ledger = test_ledger("a");
        register_into(&registry, ledger.clone());
        let clock = FakeClock::new();
        let mut sender = RecordingSender::always_ok();
        publish_if_due_with(&registry, &clock, &mut sender);
        assert_eq!(sender.attempts, 1);

        ledger
            .accounting()
            .charge(ReclaimClass::DisposableUi, 4096, 0)
            .expect("fresh ledger accepts a charge");
        clock.advance(MIN_SEND_INTERVAL_NS);
        publish_if_due_with(&registry, &clock, &mut sender);
        assert_eq!(sender.attempts, 2);
        publish_if_due_with(&registry, &clock, &mut sender);
        assert_eq!(wait_deadline_ns_with(&registry, &clock), 0);
    }

    #[test]
    fn a_refused_hand_off_leaves_the_change_pending_and_does_not_clear_the_deadline() {
        let registry = registry();
        register_into(&registry, test_ledger("a"));
        let clock = FakeClock::new();
        let mut sender = RecordingSender::queue(alloc::vec![Verdict::NotTaken(Errno::NotFound)]);
        publish_if_due_with(&registry, &clock, &mut sender);
        assert_eq!(sender.attempts, 1);
        assert!(
            wait_deadline_ns_with(&registry, &clock) > 0,
            "a refused hand-off leaves the change pending"
        );

        // Retrying immediately (no time advanced) must not attempt another
        // hand-off: the rate limiter holds off exactly as it would for a
        // suppressed change, so a dead service is never hammered.
        publish_if_due_with(&registry, &clock, &mut sender);
        assert_eq!(sender.attempts, 1);
    }

    #[test]
    fn a_report_the_service_refuses_is_not_adopted_and_is_restated() {
        let registry = registry();
        register_into(&registry, test_ledger("a"));
        let clock = FakeClock::new();
        let mut sender =
            RecordingSender::queue(alloc::vec![Verdict::Refused(Errno::LengthOutOfRange)]);
        publish_if_due_with(&registry, &clock, &mut sender);
        assert_eq!(sender.attempts, 1);

        // The verdict lands on the next pass. The figure it carried is *not*
        // what the service holds, so it is restated once the interval allows.
        clock.advance(MIN_SEND_INTERVAL_NS);
        publish_if_due_with(&registry, &clock, &mut sender);
        assert_eq!(
            sender.attempts, 2,
            "a refused report is restated, never assumed recorded"
        );
        assert_eq!(sender.blocking_sends, 0);
    }

    #[test]
    fn a_report_still_in_flight_is_never_replaced_by_a_second() {
        let registry = registry();
        let ledger = test_ledger("a");
        register_into(&registry, ledger.clone());
        let clock = FakeClock::new();
        // A channel that never answers, so the first report stays in flight.
        let mut sender = RecordingSender::always_ok();
        publish_if_due_with(&registry, &clock, &mut sender);
        assert_eq!(sender.attempts, 1);

        ledger
            .accounting()
            .charge(ReclaimClass::DisposableUi, 4096, 0)
            .expect("fresh ledger accepts a charge");
        clock.advance(MIN_SEND_INTERVAL_NS);
        // The gate hands the fresh figure over; the channel refuses because
        // the previous report is still outstanding, and the change stays owed
        // rather than being lost or waited on.
        sender.in_flight = Some(Ok(()));
        publish_if_due_with(&registry, &clock, &mut sender);
        assert_eq!(sender.attempts, 2);
        assert_eq!(sender.blocking_sends, 0);
        assert!(wait_deadline_ns_with(&registry, &clock) > 0);
    }

    #[test]
    fn withdraw_sends_an_empty_report_and_waits_for_it() {
        let registry = registry();
        register_into(&registry, test_ledger("a"));
        let mut sender = RecordingSender::always_ok();
        withdraw_with(&registry, &mut sender);
        assert_eq!(sender.attempts, 1);
        assert_eq!(
            sender.blocking_sends, 1,
            "the withdrawal is the one report that waits: the kernel drops a \
             posted request whose poster has exited"
        );

        // The registry is empty again: a fresh sample matches the withdrawn
        // (empty) snapshot, so nothing more is sent.
        let clock = FakeClock::new();
        publish_if_due_with(&registry, &clock, &mut sender);
        assert_eq!(sender.attempts, 1);
    }

    #[test]
    fn a_withdrawal_abandons_a_report_in_flight_so_it_cannot_land_after() {
        let registry = registry();
        let ledger = test_ledger("a");
        register_into(&registry, ledger.clone());
        let clock = FakeClock::new();
        let mut sender = RecordingSender::always_ok();
        publish_if_due_with(&registry, &clock, &mut sender);
        assert!(sender.in_flight.is_some(), "a report is outstanding");

        withdraw_with(&registry, &mut sender);
        assert!(
            sender.in_flight.is_none(),
            "the outstanding report is withdrawn, not left to resurrect the rows"
        );
        assert!(registry.lock().in_flight.is_empty());

        // Nothing the abandoned report carried is ever adopted.
        publish_if_due_with(&registry, &clock, &mut sender);
        assert!(registry.lock().last_sent.is_empty());
    }

    /// `register`'s label must be `'static`; a fixed table covers every
    /// index [`over_registration_is_bounded_and_keeps_the_first_registered`]
    /// reaches.
    const OVER_REGISTRATION_LABELS: [&str; 20] = [
        "l0", "l1", "l2", "l3", "l4", "l5", "l6", "l7", "l8", "l9", "l10", "l11", "l12", "l13",
        "l14", "l15", "l16", "l17", "l18", "l19",
    ];

    #[test]
    fn over_registration_is_bounded_and_keeps_the_first_registered() {
        let registry = registry();
        for label in OVER_REGISTRATION_LABELS
            .iter()
            .copied()
            .take(MAX_CACHE_REPORT_ENTRIES + 4)
        {
            register_into(&registry, test_ledger(label));
        }
        let state = registry.lock();
        assert_eq!(state.ledgers.len(), MAX_CACHE_REPORT_ENTRIES);
        assert_eq!(state.ledgers[0].label(), "l0");
        assert_eq!(
            state.ledgers[MAX_CACHE_REPORT_ENTRIES - 1].label(),
            OVER_REGISTRATION_LABELS[MAX_CACHE_REPORT_ENTRIES - 1]
        );
    }

    #[test]
    fn every_emitted_row_carries_the_unset_origin_and_no_reporter_pid() {
        let registry = registry();
        let ledger = test_ledger("a");
        ledger
            .accounting()
            .charge(ReclaimClass::DisposableUi, 1024, 0)
            .expect("fresh ledger accepts a charge");
        register_into(&registry, ledger);
        let sampled = registry.lock().sample();
        assert_eq!(sampled.len(), 1);
        assert_eq!(sampled[0].origin, CacheLedgerOrigin::Unset);
        assert_eq!(sampled[0].reporter_pid, 0);
    }

    /// The process-wide [`register`]/[`REGISTRY`] pair is exercised
    /// separately from the isolated-registry tests above, since it is
    /// shared with every other test in this binary; this only proves the
    /// public entry point reaches the same logic, not the rate limit or
    /// change detection (already covered against an isolated registry).
    #[test]
    fn the_public_register_reaches_the_process_wide_registry() {
        let unique_label: &'static str = "rt.cachereport.process_wide_smoke_test";
        register(test_ledger(unique_label));
        let found = REGISTRY
            .lock()
            .ledgers
            .iter()
            .any(|ledger| ledger.label() == unique_label);
        assert!(found, "register() reached the process-wide registry");
    }

    #[test]
    fn an_indefinite_wait_stays_indefinite_when_nothing_is_pending() {
        let registry = registry();
        let clock = FakeClock::new();
        // Nothing registered, so nothing is ever sampled as changed and
        // `wait_deadline_ns` reports nothing pending.
        assert_eq!(
            fold_wait_deadline_ns_with(u64::MAX, &registry, &clock),
            u64::MAX
        );
    }

    #[test]
    fn a_pending_report_bounds_an_otherwise_indefinite_wait() {
        let registry = registry();
        let ledger = test_ledger("a");
        register_into(&registry, ledger.clone());
        let clock = FakeClock::new();
        let mut sender = RecordingSender::always_ok();
        // The first sample always sends, so hold a second change back
        // inside the rate-limit window to arm a pending deadline.
        publish_if_due_with(&registry, &clock, &mut sender);
        ledger
            .accounting()
            .charge(ReclaimClass::DisposableUi, 4096, 0)
            .expect("fresh ledger accepts a charge");
        clock.advance(MIN_SEND_INTERVAL_NS / 4);
        publish_if_due_with(&registry, &clock, &mut sender);

        let pending = wait_deadline_ns_with(&registry, &clock);
        assert!(pending > 0, "a suppressed change must arm a deadline");
        assert_eq!(
            fold_wait_deadline_ns_with(u64::MAX, &registry, &clock),
            pending,
            "the indefinite wait is bounded to exactly the pending deadline"
        );
    }

    #[test]
    fn an_existing_shorter_deadline_is_not_lengthened_by_the_reporter() {
        let registry = registry();
        let ledger = test_ledger("a");
        register_into(&registry, ledger.clone());
        let clock = FakeClock::new();
        let mut sender = RecordingSender::always_ok();
        publish_if_due_with(&registry, &clock, &mut sender);
        ledger
            .accounting()
            .charge(ReclaimClass::DisposableUi, 4096, 0)
            .expect("fresh ledger accepts a charge");
        clock.advance(MIN_SEND_INTERVAL_NS / 4);
        publish_if_due_with(&registry, &clock, &mut sender);

        let pending = wait_deadline_ns_with(&registry, &clock);
        assert!(pending > 0);
        let shorter = pending / 2;
        assert_eq!(
            fold_wait_deadline_ns_with(shorter, &registry, &clock),
            shorter,
            "a shorter deadline the loop already carries must win"
        );
    }
}
