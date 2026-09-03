//! Cross-CPU **quiesce** coordinator — the bounded, fail-closed handshake
//! that stops every other logical CPU before a one-way, irreversible
//! whole-machine operation (the pre-boot Supervisor's `memtest`,
//! `plans/NEW-SUPERVISOR.md` §9).
//!
//! # Why this lives here, and why it is architecture-neutral
//!
//! Taking the machine over to test *all* of RAM (see [`crate::takeover`])
//! first requires that no other CPU is executing: a second core still
//! running the scheduler would fault the instant the sweep destroys the page
//! tables or stack it depends on. Stopping the other cores is two halves:
//!
//! * **The decision, the bookkeeping, and the bounded wait are the same on
//!   every architecture** — set a "stop" request, poke each *online* peer,
//!   and wait a bounded time for each to acknowledge — so they live here,
//!   once, never copied into each port (that would be the duplication the
//!   charter forbids). The peer set is data the kernel discovered at boot,
//!   threaded in through the published liveness/ack tables, not a
//!   compile-time constant.
//! * **Only the two genuinely per-silicon acts stay per-port:** *delivering*
//!   the poke (the directed inter-processor interrupt, already
//!   [`crate::SchedulerArch::send_ipi`]) and, on the receiving core, *parking*
//!   masked and memory-free forever. Each port's interrupt-receive path calls
//!   [`stop_requested`] and, when it is set, [`acknowledge`] then enters its
//!   own masked halt loop (executing only from the reserved kernel image, so
//!   the sweep cannot pull the ground out from under it).
//!
//! # The handshake (fail closed, bounded, never a busy task)
//!
//! 1. The boot CPU calls [`quiesce_others`], naming itself. It clears the ack
//!    table, publishes the stop request, and sends a directed IPI to every
//!    *online* CPU other than itself.
//! 2. Each poked CPU takes the IPI, sees the request in its handler,
//!    [`acknowledge`]s, and parks masked forever — it never runs kernel code
//!    or touches destructible RAM again.
//! 3. The boot CPU waits a **bounded** number of spins for every expected
//!    peer to acknowledge. If all do, it returns [`Ok`] and the caller
//!    proceeds with the tear-down. If the budget elapses first, it returns
//!    [`Err`] naming a CPU that did not stop, and the caller **fails closed**
//!    (abandons the takeover, machine unchanged).
//!
//! The wait is the narrow, bounded hardware-handshake spin the charter
//! permits — the machine is being deliberately torn down and the boot CPU has
//! nothing else it may safely do until its peers are halted — never a task's
//! steady state, and never unbounded.
//!
//! If no liveness table has been published (a single-CPU boot, or a boot that
//! never brought secondaries up), there is nothing to quiesce and
//! [`quiesce_others`] succeeds immediately.
//!
//! # Two initiators, one handshake
//!
//! [`quiesce_others`] is the *fail-closed* initiator: a tear-down that cannot
//! prove every peer stopped must abandon, so it waits patiently and reports an
//! error naming the peer. [`stop_others_best_effort`] is the initiator a fatal
//! kernel report uses: it cannot abandon — the kernel is already dying — so it
//! waits a much shorter budget and *reports what it achieved* instead of
//! failing. Both latch the same request, poke through the same per-port IPI,
//! and are acknowledged by the same per-port receive path; only the patience
//! and the failure policy differ.

use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicUsize, Ordering};

use crate::CpuId;

/// The published stop request. `true` once an initiator has asked the other
/// CPUs to halt; each core's interrupt-receive path reads it through
/// [`stop_requested`].
static STOP_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Sentinel for "no stop has been requested" in [`REQUESTER`]. No dense
/// [`CpuId`] can name a CPU at `u32::MAX`.
const NO_REQUESTER: u32 = u32::MAX;

/// The CPU that latched the request, so its own receive path can ignore it.
///
/// Without this the requester can be halted by its own poke arriving late: a
/// panic raised with interrupts enabled (a syscall body runs that way) latches
/// the request and then, before it has written its record, takes an IPI a peer
/// had already sent — whereupon its own receive path would acknowledge and
/// park it, losing the one diagnosis the machine was about to produce. The
/// requester must always reach its own halt.
static REQUESTER: AtomicU32 = AtomicU32::new(NO_REQUESTER);

/// Base of the kernel-published per-CPU **liveness** table (one
/// [`AtomicBool`] per dense [`CpuId`], `true` once that CPU is online). Null
/// until [`publish_tables`] runs; a null table means "nothing to quiesce".
static ONLINE_BASE: AtomicPtr<AtomicBool> = AtomicPtr::new(core::ptr::null_mut());

/// Base of the kernel-published per-CPU **acknowledgement** table (one
/// [`AtomicBool`] per dense [`CpuId`]). A halting CPU sets its own slot from
/// [`acknowledge`]; the boot CPU's bounded wait reads it. Null until
/// [`publish_tables`] runs.
static ACK_BASE: AtomicPtr<AtomicBool> = AtomicPtr::new(core::ptr::null_mut());

/// Length of both published tables (`0` until [`publish_tables`] runs). Both
/// tables are the same length — one slot per dense CPU id — sized by the
/// kernel to the discovered CPU count, never a compile-time ceiling.
static TABLE_LEN: AtomicUsize = AtomicUsize::new(0);

/// Set-once guard so a second [`publish_tables`] is refused rather than
/// silently re-pointing the live handshake at different storage.
static PUBLISHED: AtomicBool = AtomicBool::new(false);

/// Why [`publish_tables`] refused.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum PublishError {
    /// A pair of tables was already published; the slot is set-once per boot.
    AlreadyPublished,
    /// The two tables were different lengths. They must have exactly one slot
    /// per dense CPU id, so a mismatch is a caller bug and is refused rather
    /// than risking an out-of-bounds ack.
    LengthMismatch,
}

/// The bounded number of ack-poll spins the boot CPU waits before declaring a
/// peer stuck.
///
/// This is a *safety* bound on a deliberate tear-down handshake, not a
/// scalable capacity: it only has to be large enough that a healthy CPU —
/// which merely has to take one already-pending IPI and run a few
/// instructions — always acknowledges well within it, while still being small
/// enough that a genuinely wedged core makes the takeover fail closed in a
/// fraction of a second rather than hang. A core that cannot answer a pending
/// interrupt in a hundred million spins is not going to.
const QUIESCE_WAIT_SPINS: u64 = 100_000_000;

/// The bounded number of ack-poll spins a **fatal report** waits for its
/// peers, two orders of magnitude below [`QUIESCE_WAIT_SPINS`].
///
/// The tear-down handshake can afford patience: the machine is otherwise idle
/// and its caller abandons on timeout. A fatal report cannot — it is holding
/// up the one diagnosis the machine will ever emit, and it halts either way.
/// A healthy peer only has to take an already-pending IPI and run a handful of
/// instructions, so it acknowledges orders of magnitude inside this budget; a
/// peer that *cannot* is wedged with interrupts masked, which is precisely
/// what the report wants to name rather than stall behind.
const REPORT_WAIT_SPINS: u64 = 1_000_000;

/// Publish the kernel's per-CPU liveness and acknowledgement tables.
///
/// Called once by the kernel during SMP bring-up, before any quiesce can be
/// requested. `online` is the same table the kernel marks as each secondary
/// comes online; `ack` is a companion table of the same length reserved for
/// this handshake. Both are sized to the discovered CPU count (one slot per
/// dense [`CpuId`]) and must outlive the kernel (`&'static`).
///
/// # Errors
///
/// [`PublishError::AlreadyPublished`] on a second publish (set-once per
/// boot); [`PublishError::LengthMismatch`] if the tables differ in length.
pub fn publish_tables(
    online: &'static [AtomicBool],
    ack: &'static [AtomicBool],
) -> Result<(), PublishError> {
    if online.len() != ack.len() {
        return Err(PublishError::LengthMismatch);
    }
    if PUBLISHED
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return Err(PublishError::AlreadyPublished);
    }
    // Publish the bases before the length: a reader that observes a non-zero
    // length has, by the release/acquire pairing, also observed both bases.
    ONLINE_BASE.store(online.as_ptr().cast_mut(), Ordering::Release);
    ACK_BASE.store(ack.as_ptr().cast_mut(), Ordering::Release);
    TABLE_LEN.store(online.len(), Ordering::Release);
    Ok(())
}

/// Whether the calling CPU has been asked to stop.
///
/// Each port's interrupt-receive path reads this on every delivered IPI,
/// naming itself; when it is `true` the receiving CPU must [`acknowledge`]
/// and park masked forever rather than return to normal kernel execution.
///
/// Always `false` on the CPU that latched the request: that core is tearing
/// the machine down or reporting its death and must reach its own halt rather
/// than be parked by its own poke arriving late.
#[must_use]
pub fn stop_requested(cpu: CpuId) -> bool {
    // The latch is read first: a reader that observes it has, by the
    // release/acquire pairing in `latch_and_poke`, also observed the
    // requester it was stored with.
    STOP_REQUESTED.load(Ordering::Acquire) && REQUESTER.load(Ordering::Acquire) != cpu
}

/// Acknowledge, from the calling CPU, that it has seen the stop request and
/// is about to park.
///
/// Sets `cpu`'s slot in the published ack table. A no-op if no table is
/// published or `cpu` is out of range (defence in depth — a real dense id is
/// always in range). The caller parks immediately after, so the slot is set
/// exactly once per quiesce.
pub fn acknowledge(cpu: CpuId) {
    let base = ACK_BASE.load(Ordering::Acquire);
    let len = TABLE_LEN.load(Ordering::Acquire);
    let idx = cpu as usize;
    if base.is_null() || idx >= len {
        return;
    }
    // SAFETY: `base` is the `&'static [AtomicBool]` published by
    // `publish_tables` and `idx < len`, so the offset addresses a live slot in
    // that table. `AtomicBool` shares its layout with the published slice's
    // element, and the store is atomic.
    let slot = unsafe { &*base.add(idx) };
    slot.store(true, Ordering::Release);
}

/// Request that every *online* CPU other than `current` stop, poke each one,
/// and wait a bounded time for them all to acknowledge.
///
/// `current` is the calling (boot) CPU, which is not asked to stop and is
/// recorded as the requester, so a poke arriving back at it is ignored by its
/// own receive path rather than parking it. `send_ipi` delivers the port's
/// directed inter-processor interrupt to a peer (the caller supplies
/// [`crate::SchedulerArch::send_ipi`]).
///
/// Returns `Ok(())` once every expected peer has acknowledged (or there are
/// none — a single-CPU or not-yet-published system). Returns `Err(cpu)`
/// naming a peer that did not acknowledge within the bounded budget, so the
/// caller **fails closed**.
///
/// The stop request stays latched after this returns (acknowledged peers are
/// parked forever), which is exactly what the one-way tear-down wants.
pub fn quiesce_others(current: CpuId, send_ipi: impl FnMut(CpuId)) -> Result<(), CpuId> {
    let Some((online, ack)) = published_tables() else {
        // No liveness table published: single-CPU boot, or secondaries were
        // never brought up. Nothing to quiesce.
        return Ok(());
    };
    latch_and_poke(current, online, ack, send_ipi);
    match wait_counted(current, online, ack, QUIESCE_WAIT_SPINS).unresponsive {
        None => Ok(()),
        Some(cpu) => Err(cpu),
    }
}

/// What a best-effort stop achieved.
///
/// A count rather than a verdict, because the caller is a fatal report that
/// proceeds either way — and because "which peer would not stop" is itself
/// evidence: a peer that cannot answer a pending IPI is wedged with interrupts
/// masked, the shape of failure a hard lockup takes.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub struct StopOutcome {
    /// Online peers the stop asked to halt (never counting `current`).
    pub asked: u32,
    /// How many of them acknowledged within the budget.
    pub stopped: u32,
    /// One peer that did not, when any remained.
    pub unresponsive: Option<CpuId>,
}

impl StopOutcome {
    /// The outcome on a machine with no peers to stop.
    const NONE: Self = Self {
        asked: 0,
        stopped: 0,
        unresponsive: None,
    };
}

/// Stop every other *online* CPU **best effort**, for a fatal kernel report.
///
/// Latches the same stop request [`quiesce_others`] does, pokes every online
/// peer through `send_ipi`, and waits a short bounded budget for their
/// acknowledgements — then returns what it achieved instead of failing. The
/// caller is a panic or a fatal-fault report: it cannot abandon and must not
/// stall, so an unresponsive peer is *named in the report*, not waited on.
///
/// The stop request stays latched, so a peer that takes its IPI later still
/// parks rather than running on over state the dying core abandoned.
pub fn stop_others_best_effort(current: CpuId, send_ipi: impl FnMut(CpuId)) -> StopOutcome {
    let Some((online, ack)) = published_tables() else {
        return StopOutcome::NONE;
    };
    latch_and_poke(current, online, ack, send_ipi);
    wait_counted(current, online, ack, REPORT_WAIT_SPINS)
}

/// The published liveness and acknowledgement tables, or [`None`] when none
/// have been published (nothing to stop).
fn published_tables() -> Option<(&'static [AtomicBool], &'static [AtomicBool])> {
    let len = TABLE_LEN.load(Ordering::Acquire);
    let online_base = ONLINE_BASE.load(Ordering::Acquire);
    if len == 0 || online_base.is_null() {
        return None;
    }
    let ack_base = ACK_BASE.load(Ordering::Acquire);
    // SAFETY: both bases are the `&'static [AtomicBool]` tables published by
    // `publish_tables`, both of length `len`; reconstructing the slices is
    // sound for the kernel's lifetime.
    unsafe {
        Some((
            core::slice::from_raw_parts(online_base, len),
            core::slice::from_raw_parts(ack_base, len),
        ))
    }
}

/// Clear stale acknowledgements, latch the stop request, and poke every online
/// peer. Shared by both initiators so the request can never be latched one way
/// in one of them and another way in the other.
fn latch_and_poke(
    current: CpuId,
    online: &[AtomicBool],
    ack: &[AtomicBool],
    send_ipi: impl FnMut(CpuId),
) {
    // Clear the ack table before latching the request, so a stale ack from a
    // prior handshake can never satisfy this one.
    for slot in ack {
        slot.store(false, Ordering::Relaxed);
    }
    // The requester is published before the latch, so no CPU can observe the
    // request without also observing who is exempt from it.
    REQUESTER.store(current, Ordering::Release);
    STOP_REQUESTED.store(true, Ordering::Release);
    poke_others(current, online, send_ipi);
}

/// Send a stop IPI to every online CPU other than `current`.
///
/// Factored out (with [`wait_counted`]) so the peer-selection logic is
/// exercised on the host over plain slices, independently of the published
/// statics and the real per-port IPI.
fn poke_others(current: CpuId, online: &[AtomicBool], mut send_ipi: impl FnMut(CpuId)) {
    for (idx, live) in online.iter().enumerate() {
        // A dense CpuId is a `u32`; a table longer than `u32::MAX` cannot name
        // a real CPU, so a non-representable index is skipped (never poked).
        let Ok(cpu) = CpuId::try_from(idx) else {
            continue;
        };
        if cpu != current && live.load(Ordering::Acquire) {
            send_ipi(cpu);
        }
    }
}

/// Spin, bounded by `budget` iterations, until every online CPU other than
/// `current` has acknowledged, and report what was achieved.
///
/// The one wait both initiators use: [`quiesce_others`] narrows the outcome to
/// its fail-closed `Result`, while [`stop_others_best_effort`] returns it
/// whole. Pure over its slices and budget so the host suite can drive the
/// all-acknowledged, partial, and timed-out paths with a tiny budget and no
/// dependence on the published statics.
///
/// The spin is the narrow, bounded hardware handshake the charter permits: the
/// caller is tearing the machine down or reporting its death and has nothing
/// else it may safely do, and the budget makes it terminate regardless.
fn wait_counted(
    current: CpuId,
    online: &[AtomicBool],
    ack: &[AtomicBool],
    budget: u64,
) -> StopOutcome {
    let mut spins = 0u64;
    loop {
        let outcome = tally(current, online, ack);
        if outcome.unresponsive.is_none() {
            return outcome;
        }
        spins += 1;
        if spins >= budget {
            return outcome;
        }
        core::hint::spin_loop();
    }
}

/// Count the online peers of `current` and how many have acknowledged, naming
/// the first that has not.
fn tally(current: CpuId, online: &[AtomicBool], ack: &[AtomicBool]) -> StopOutcome {
    let mut asked = 0u32;
    let mut stopped = 0u32;
    let mut unresponsive = None;
    for (idx, live) in online.iter().enumerate() {
        // A dense CpuId is a `u32`; a table longer than `u32::MAX` cannot name
        // a real CPU, so a non-representable index is never awaited.
        let Ok(cpu) = CpuId::try_from(idx) else {
            continue;
        };
        if cpu == current || !live.load(Ordering::Acquire) {
            continue;
        }
        asked += 1;
        if ack[idx].load(Ordering::Acquire) {
            stopped += 1;
        } else if unresponsive.is_none() {
            unresponsive = Some(cpu);
        }
    }
    StopOutcome {
        asked,
        stopped,
        unresponsive,
    }
}

#[cfg(test)]
#[path = "quiesce_tests.rs"]
mod tests;
