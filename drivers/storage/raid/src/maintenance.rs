//! The array **maintenance scheduler** ([`ArrayMaintenance`]) — the one policy
//! that decides *when* a composed array re-admits a returning member, advances
//! a rebuild, and runs a proactive scrub (`plans/FIX-IO.md` IO6).
//!
//! [`RaidArray`] exposes the self-healing surface — `readd_member`,
//! `resync_step`, `begin_scrub`/`scrub_step` — but exposing it is not the same
//! as driving it. An array only heals itself if something decides, on every
//! turn of a serve loop, which of those to do next and when. Every consumer
//! that owns a composed array — the autoloaded serve process and the
//! ARXFS-native multi-device composition alike (`plans/FIX-IO.md` IO6
//! remaining) — needs exactly that decision, and a slip in it is a
//! data-integrity or availability fault rather than a cosmetic one:
//!
//! - A rebuild that is never started leaves the array degraded until the
//!   *next* fault loses data (`AGENTS.md` §26.5).
//! - A rebuild that never yields starves the foreground workload the array
//!   exists to serve (`AGENTS.md` §26.1, §26.2, §2.16).
//! - Re-probing a faulted member in a tight loop is the busy-wait the charter
//!   forbids (`AGENTS.md` §2.23), and re-probing it *never* means a disk that
//!   came back stays out of the array (`AGENTS.md` §18.4).
//! - Scrubbing an array that is mid-rebuild spends the bandwidth the rebuild
//!   needs to restore redundancy.
//!
//! So the decision lives here once, host-provable, rather than being
//! hand-rolled per consumer (`AGENTS.md` §2.2, §27).
//!
//! # Shape
//!
//! The scheduler is pure and **event-timed**: it holds no clock, spawns no
//! timer, and never spins. The caller supplies the monotonic reading it took
//! (`clock_get`) on every entry point, and when there is nothing to do
//! [`wait_deadline_ns`](ArrayMaintenance::wait_deadline_ns) gives the absolute
//! one-shot deadline the serve loop arms its wait to — the same idiom the
//! per-device health machine and the fault domain use
//! (`blkio::BlkHealth::grace_deadline_ns`). It is allocation-free: the
//! per-member re-add backoff records live in a caller-owned slice exactly as
//! the composition engines' members do, so a wide array imposes no fixed
//! ceiling (`AGENTS.md` §24.1).
//!
//! The serve loop's contract per turn is:
//!
//! 1. [`next_action`](ArrayMaintenance::next_action) — decide.
//! 2. Perform the action against the array (or nothing, for
//!    [`MaintenanceAction::Idle`]).
//! 3. [`note_step`](ArrayMaintenance::note_step) — hand back what happened,
//!    which is what paces the next chunk and escalates a refused re-add.
//! 4. On [`MaintenanceAction::Idle`], park until the soonest of the array's
//!    own I/O events and [`wait_deadline_ns`](ArrayMaintenance::wait_deadline_ns).
//!
//! Foreground traffic is reported through
//! [`note_foreground`](ArrayMaintenance::note_foreground), and a member's
//! demonstrated return (the recovery signal a leaf device's health or its
//! fault domain publishes, `plans/FIX-IO.md` IO3/IO4) through
//! [`note_member_returned`](ArrayMaintenance::note_member_returned).
//!
//! # Priority
//!
//! Restoring redundancy always outranks verifying it, and both yield to the
//! foreground workload:
//!
//! 1. **Re-admit a faulted member** whose backoff has elapsed. An array short
//!    a copy is one fault from data loss, so getting the copy back is the most
//!    urgent thing the array can do.
//! 2. **Advance a rebuild** of a member that is already resyncing.
//! 3. **Advance or start a proactive scrub**, and only on a fully `Optimal`
//!    array: while a copy is missing or rebuilding, the array's bandwidth
//!    belongs to restoring redundancy, and a scrub that can detect but not
//!    repair spends I/O to no benefit. An array that degrades mid-pass
//!    therefore *pauses* its scrub where the cursor stands and picks it up
//!    once full redundancy is back, rather than abandoning the work already
//!    done or pressing on without a copy to repair from.
//!
//! # What it deliberately does not do
//!
//! - It never installs or removes a device. [`RaidArray::add_member`] /
//!   [`RaidArray::remove_member`] are the operator/hotplug hot-swap workflow;
//!   an [`Absent`](MemberState::Absent) slot has no device to re-probe, so the
//!   scheduler leaves it alone rather than inventing a spare.
//! - It drives nothing on a [`Failed`](ArrayHealth::Failed) array. With no
//!   in-sync member there is nothing to rebuild a returning copy from, and
//!   admitting one as current would serve data the array cannot vouch for
//!   (`AGENTS.md` §5.4, §26.5). Bringing a failed array back is a re-resolution
//!   of its members' superblocks against their generation counters — an
//!   assembly decision, not a maintenance one.
//! - It drives nothing on a non-redundant RAID0 stripe, which has nothing to
//!   scrub from, rebuild from, or hot-swap
//!   ([`RaidLevel::is_redundant`](crate::RaidLevel::is_redundant)).

use tairix_abi::blkio::BlkDeviceClass;
use tairix_abi::driver::block::Block;

use crate::array::{RaidArray, RaidError};
use crate::mirror::{ArrayHealth, MemberState};

/// How long after the last foreground request an array still counts as busy.
///
/// This measures the *workload's* burstiness, not the device's speed: a
/// second comfortably bridges the gap between the requests of one burst, so a
/// rebuild does not read an inter-request lull as an idle array and grab full
/// bandwidth from a workload that is still running. It is therefore one
/// default for every device class, unlike the pacing share and backoff below.
const FOREGROUND_IDLE_NS: u64 = 1_000_000_000;

/// How long a redundant array goes between proactive scrub passes by default.
///
/// This is the operator's tolerated *exposure window*: the time a latent media
/// error may sit undetected on a copy the read path never consults before a
/// scrub finds and heals it while redundancy still exists (`AGENTS.md` §26.5).
/// It is a property of the risk accepted, not of the hardware, so it is one
/// default for every class and is overridable per array through
/// [`MaintenancePolicy::scrub_period_ns`]. A week matches the cadence a
/// general-purpose desktop and a server both tolerate (`AGENTS.md` §24.2): long
/// enough that the pass is unobtrusive, short enough that a second fault
/// rarely arrives first.
const SCRUB_PERIOD_NS: u64 = 7 * 24 * 60 * 60 * 1_000_000_000;

/// How far the per-member re-add backoff may escalate, as a multiple of its
/// base delay.
///
/// The backoff doubles on every refused attempt so a dead disk is not
/// re-probed at the cadence of a merely-slow one, and stops doubling here so a
/// disk that comes back after a long absence still rejoins within a bounded
/// wait rather than an ever-receding one (`AGENTS.md` §18.4).
const READD_BACKOFF_CEILING: u64 = 32;

/// What a serve loop should do next for one composed array.
///
/// Returned by [`ArrayMaintenance::next_action`] and handed back to
/// [`ArrayMaintenance::note_step`] with its outcome.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum MaintenanceAction {
    /// Re-probe and re-admit the faulted member in this slot
    /// ([`RaidArray::readd_member`]). A refusal escalates that slot's backoff;
    /// success starts its rebuild.
    Readd {
        /// The member slot to re-admit.
        member: usize,
    },
    /// Advance the rebuild of the array's resyncing members by one bounded
    /// chunk ([`RaidArray::resync_step`]).
    Resync,
    /// Start a proactive scrub pass ([`RaidArray::begin_scrub`]).
    BeginScrub,
    /// Advance the in-flight scrub pass by one bounded chunk
    /// ([`RaidArray::scrub_step`]).
    Scrub,
    /// Nothing to do: the array is healthy and verified, is paced out of its
    /// next maintenance chunk, or cannot be helped autonomously. The loop
    /// parks until its own I/O or
    /// [`ArrayMaintenance::wait_deadline_ns`].
    Idle,
}

/// The cadence and pacing an array's autonomous maintenance runs at.
///
/// Every field is public, so a consumer that knows better about a particular
/// array (an operator-set scrub cadence, a maintenance window) sets it
/// directly; [`for_class`](Self::for_class) derives the default from the
/// array's *discovered* device class rather than freezing one scalar for every
/// machine (`AGENTS.md` §24.2).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct MaintenancePolicy {
    /// How long a redundant array goes between proactive scrub passes,
    /// measured from the end of one pass to the start of the next.
    pub scrub_period_ns: u64,
    /// The share of wall-clock time maintenance may occupy while the array is
    /// also serving foreground I/O, as a percentage. After a chunk taking `d`,
    /// the next is held off for `d × (100 − duty) / duty`, so the share holds
    /// whatever chunk size the caller's scratch buffer implies. An idle array
    /// runs maintenance at full speed. Values outside `1..=100` are clamped, so
    /// a mis-set policy can never stall maintenance completely.
    pub busy_duty_percent: u32,
    /// How long after the last request reported to
    /// [`ArrayMaintenance::note_foreground`] the array still counts as busy.
    pub foreground_idle_ns: u64,
    /// The delay before the first re-add attempt on a member that has just
    /// faulted, and the floor no recovery signal may pull an attempt below.
    pub readd_backoff_ns: u64,
    /// The ceiling the doubling re-add backoff stops at. A value below
    /// [`readd_backoff_ns`](Self::readd_backoff_ns) is raised to it, so a
    /// mis-set policy can never make an escalated wait shorter than the first.
    pub readd_backoff_max_ns: u64,
}

impl MaintenancePolicy {
    /// The default policy for an array of `class` — the class its members'
    /// fold declares through [`Block::device_class`].
    ///
    /// The two class-dependent quantities are derived from that class's own
    /// I/O budget rather than a second hand-written table that could drift
    /// from it (`AGENTS.md` §2.2):
    ///
    /// - The **first re-add delay** is the class's recovery grace window
    ///   (`IoBudget::grace_ns`). Re-probing a member sooner is pointless: it is
    ///   still inside the window its own driver gives it to come back, so the
    ///   probe would only ask a device that has not yet been given up on.
    /// - The **busy duty share** reflects how destructive maintenance is to
    ///   foreground latency on that class. Every seek a scrub or rebuild costs
    ///   a rotational disk is one the workload waits for, and a removable unit
    ///   has a shallow queue that saturates as easily, so both keep a small
    ///   share; a solid-state device absorbs a parallel background stream with
    ///   far less interference, and a paravirtual device sits between the two.
    ///
    /// The scrub cadence and the busy window are properties of the accepted
    /// risk and of the workload rather than of the hardware, so they are the
    /// same for every class (see [`Self::scrub_period_ns`],
    /// [`Self::foreground_idle_ns`]).
    #[must_use]
    pub const fn for_class(class: BlkDeviceClass) -> Self {
        let grace_ns = class.budget().grace_ns;
        Self {
            scrub_period_ns: SCRUB_PERIOD_NS,
            busy_duty_percent: match class {
                BlkDeviceClass::Rotational | BlkDeviceClass::Removable => 10,
                BlkDeviceClass::Virtual => 25,
                BlkDeviceClass::SolidState => 40,
            },
            foreground_idle_ns: FOREGROUND_IDLE_NS,
            readd_backoff_ns: grace_ns,
            readd_backoff_max_ns: grace_ns.saturating_mul(READD_BACKOFF_CEILING),
        }
    }
}

/// One member slot's re-add backoff state, owned by the caller.
///
/// The scheduler borrows a slice of these, one per array slot, so a wide array
/// costs no allocation and hits no fixed member ceiling (`AGENTS.md` §24.1) —
/// the same shape the composition engines' member buffers use. The contents
/// are the scheduler's; a caller only supplies the storage, default-initialised.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct MemberRetry {
    /// Monotonic time the next attempt on this slot may run.
    due_ns: u64,
    /// The current backoff step, doubling towards the policy ceiling.
    backoff_ns: u64,
    /// Monotonic time this slot was last attempted (or armed), the floor a
    /// recovery signal may not pull an attempt below.
    last_attempt_ns: u64,
    /// Whether this slot currently holds a faulted member awaiting re-admission.
    armed: bool,
}

impl MemberRetry {
    /// A fresh, unarmed record, usable in a `const` array initialiser.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            due_ns: 0,
            backoff_ns: 0,
            last_attempt_ns: 0,
            armed: false,
        }
    }
}

/// A reason an [`ArrayMaintenance`] could not be built.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum MaintenanceError {
    /// The caller's per-member retry buffer is not the same length as the
    /// array's member count, so some slot would be untracked or some record
    /// would name a slot that does not exist. Fails closed rather than
    /// scheduling against a width it cannot see (the maintenance-side
    /// counterpart of [`AssembleError::WidthMismatch`](crate::AssembleError::WidthMismatch)).
    WidthMismatch,
}

/// The maintenance scheduler for one composed [`RaidArray`]: the single policy
/// deciding *when* the array re-admits a returning member, advances a rebuild,
/// and runs a proactive scrub.
///
/// Per turn of a serve loop, [`next_action`](Self::next_action) decides, the
/// caller performs the action against the array, and
/// [`note_step`](Self::note_step) records what came of it — which is what paces
/// the next chunk and escalates a refused re-add. On
/// [`MaintenanceAction::Idle`] the loop parks until the soonest of the array's
/// own I/O and [`wait_deadline_ns`](Self::wait_deadline_ns). Foreground traffic
/// is reported through [`note_foreground`](Self::note_foreground) and a
/// member's demonstrated return through
/// [`note_member_returned`](Self::note_member_returned).
///
/// Restoring redundancy outranks verifying it: re-admit a faulted member whose
/// backoff has elapsed, then advance a rebuild, then advance or start a
/// proactive scrub — the last only on a fully
/// [`Optimal`](ArrayHealth::Optimal) array, pausing at its cursor while
/// redundancy is reduced. The scheduler is pure and event-timed: it holds no
/// clock, never spins, and allocates nothing (the per-member backoff records
/// are the caller's slice). It never installs or removes a device, and drives
/// nothing on a [`Failed`](ArrayHealth::Failed) array or a non-redundant
/// stripe. The crate docs carry the full rationale.
pub struct ArrayMaintenance<'a> {
    policy: MaintenancePolicy,
    retries: &'a mut [MemberRetry],
    /// Monotonic time the next paced maintenance chunk may run.
    next_chunk_ns: u64,
    /// Monotonic time the next proactive scrub pass may begin.
    next_scrub_ns: u64,
    /// The last foreground request the caller reported, if any.
    last_foreground_ns: Option<u64>,
    /// Whether a scrub pass this scheduler started is still running, so its
    /// completion re-arms the period.
    scrub_active: bool,
    /// The deadline the last [`Self::next_action`] computed for its caller.
    wake_ns: Option<u64>,
}

impl<'a> ArrayMaintenance<'a> {
    /// Build a scheduler for `array`, tracking its members in the caller-owned
    /// `retries` buffer.
    ///
    /// `since_last_scrub_ns` is how long ago the array's last scrub pass
    /// completed, as the caller knows it from the array's persisted
    /// maintenance record; the first pass is then due once the remainder of
    /// [`MaintenancePolicy::scrub_period_ns`] has elapsed. A caller with **no**
    /// record passes [`u64::MAX`], which makes the first pass due immediately:
    /// an array whose verification history is unknown is verified rather than
    /// assumed clean (`AGENTS.md` §5.4, §26.5), and the duty pacing bounds what
    /// that costs.
    ///
    /// # Errors
    ///
    /// [`MaintenanceError::WidthMismatch`] if `retries` is not exactly one
    /// record per array member.
    pub fn new<B: Block>(
        array: &RaidArray<'_, B>,
        retries: &'a mut [MemberRetry],
        policy: MaintenancePolicy,
        now_ns: u64,
        since_last_scrub_ns: u64,
    ) -> Result<Self, MaintenanceError> {
        if retries.len() != array.member_count() {
            return Err(MaintenanceError::WidthMismatch);
        }
        for retry in retries.iter_mut() {
            *retry = MemberRetry::new();
        }
        let next_scrub_ns =
            now_ns.saturating_add(policy.scrub_period_ns.saturating_sub(since_last_scrub_ns));
        Ok(Self {
            policy,
            retries,
            next_chunk_ns: now_ns,
            next_scrub_ns,
            last_foreground_ns: None,
            scrub_active: false,
            wake_ns: None,
        })
    }

    /// Report that the array served a foreground request at `now_ns`, so
    /// maintenance holds to its busy duty share instead of running flat out.
    pub fn note_foreground(&mut self, now_ns: u64) {
        self.last_foreground_ns = Some(now_ns);
    }

    /// Report that `member`'s device has demonstrably returned — the recovery
    /// signal its leaf health machine or its fault domain published
    /// (`plans/FIX-IO.md` IO3/IO4) — so its re-add is attempted without waiting
    /// out an escalated backoff.
    ///
    /// The attempt is still floored at [`MaintenancePolicy::readd_backoff_ns`]
    /// after the previous one, so a member that flaps — or a signal source that
    /// repeats — can never turn this into a re-probe storm. A slot that is not
    /// currently faulted, and an index this array does not have, are ignored.
    pub fn note_member_returned(&mut self, member: usize, now_ns: u64) {
        let floor_step = self.policy.readd_backoff_ns;
        let Some(retry) = self.retries.get_mut(member) else {
            return;
        };
        if !retry.armed {
            return;
        }
        let floor = retry.last_attempt_ns.saturating_add(floor_step);
        retry.due_ns = retry.due_ns.min(now_ns.max(floor));
    }

    /// Decide what the array should do next.
    ///
    /// Reads the array's live observation surface (health, member states,
    /// rebuild and scrub progress) and this scheduler's pacing and backoff
    /// state. When the answer is [`MaintenanceAction::Idle`],
    /// [`wait_deadline_ns`](Self::wait_deadline_ns) then carries the deadline
    /// that could change it, which is always strictly in the future — so a loop
    /// that parks on it can never spin.
    pub fn next_action<B: Block>(
        &mut self,
        array: &RaidArray<'_, B>,
        now_ns: u64,
    ) -> MaintenanceAction {
        self.wake_ns = None;
        if !array.level().is_redundant() {
            return MaintenanceAction::Idle;
        }
        self.sync_retries(array, now_ns);
        if self.scrub_active && !array.scrubbing() {
            self.scrub_active = false;
            self.next_scrub_ns = now_ns.saturating_add(self.policy.scrub_period_ns);
        }
        if array.health() == ArrayHealth::Failed {
            return MaintenanceAction::Idle;
        }
        if let Some(member) = self.due_readd(now_ns) {
            return MaintenanceAction::Readd { member };
        }
        if let Some(chunk) = chunk_pending(array) {
            if now_ns >= self.next_chunk_ns {
                return chunk;
            }
        } else if array.health() == ArrayHealth::Optimal && now_ns >= self.next_scrub_ns {
            return MaintenanceAction::BeginScrub;
        }
        self.wake_ns = self.idle_deadline(array);
        MaintenanceAction::Idle
    }

    /// Record what came of the action [`next_action`](Self::next_action) last
    /// returned, so the next chunk is paced and a refused re-add escalates.
    ///
    /// `started_ns` is the monotonic reading taken before the action ran and
    /// `now_ns` the one after it; their difference is the work the duty share
    /// is measured against. [`MaintenanceAction::Idle`] records nothing.
    pub fn note_step(
        &mut self,
        action: MaintenanceAction,
        started_ns: u64,
        now_ns: u64,
        outcome: Result<(), RaidError>,
    ) {
        match action {
            MaintenanceAction::Readd { member } => self.note_readd(member, now_ns, outcome),
            MaintenanceAction::Resync | MaintenanceAction::Scrub => {
                self.pace_chunk(started_ns, now_ns, outcome);
            }
            MaintenanceAction::BeginScrub => {
                if outcome.is_ok() {
                    self.scrub_active = true;
                } else {
                    self.next_scrub_ns = now_ns.saturating_add(self.policy.scrub_period_ns);
                }
            }
            MaintenanceAction::Idle => {}
        }
    }

    /// The absolute monotonic deadline the caller arms its one-shot wait to
    /// after [`next_action`](Self::next_action) returned
    /// [`MaintenanceAction::Idle`], or [`None`] when nothing timed is pending —
    /// a non-redundant or failed array, or one whose only route back is an
    /// operator installing a spare. The loop then parks on the array's own I/O
    /// alone.
    #[must_use]
    pub fn wait_deadline_ns(&self) -> Option<u64> {
        self.wake_ns
    }

    /// Arm a retry for each slot that has just faulted, and clear the record of
    /// any slot that no longer holds a faulted member.
    ///
    /// Only a faulted slot is a candidate: it still holds its device, so
    /// [`RaidArray::readd_member`] can re-probe it. An absent slot holds none,
    /// and an in-sync or resyncing one needs nothing.
    fn sync_retries<B: Block>(&mut self, array: &RaidArray<'_, B>, now_ns: u64) {
        let base = self.policy.readd_backoff_ns;
        for (index, retry) in self.retries.iter_mut().enumerate() {
            if array.member_state(index) != Some(MemberState::Faulted) {
                *retry = MemberRetry::new();
                continue;
            }
            if !retry.armed {
                *retry = MemberRetry {
                    due_ns: now_ns.saturating_add(base),
                    backoff_ns: base,
                    last_attempt_ns: now_ns,
                    armed: true,
                };
            }
        }
    }

    /// The armed slot whose attempt is due soonest and no later than `now_ns`,
    /// lowest slot first on a tie, so the choice is deterministic.
    fn due_readd(&self, now_ns: u64) -> Option<usize> {
        self.retries
            .iter()
            .enumerate()
            .filter(|(_, retry)| retry.armed && retry.due_ns <= now_ns)
            .min_by_key(|(index, retry)| (retry.due_ns, *index))
            .map(|(index, _)| index)
    }

    fn note_readd(&mut self, member: usize, now_ns: u64, outcome: Result<(), RaidError>) {
        let base = self.policy.readd_backoff_ns;
        let ceiling = self.policy.readd_backoff_max_ns.max(base);
        let Some(retry) = self.retries.get_mut(member) else {
            return;
        };
        if outcome.is_ok() {
            *retry = MemberRetry::new();
            return;
        }
        retry.backoff_ns = retry.backoff_ns.max(base).saturating_mul(2).min(ceiling);
        retry.last_attempt_ns = now_ns;
        retry.due_ns = now_ns.saturating_add(retry.backoff_ns);
    }

    /// Hold the next maintenance chunk off long enough to keep maintenance
    /// inside its duty share of a busy array, or let it run flat out on an idle
    /// one.
    fn pace_chunk(&mut self, started_ns: u64, now_ns: u64, outcome: Result<(), RaidError>) {
        if outcome.is_err() {
            // The members reported the transfer failed. Give them the recovery
            // grace window their class allows before asking again, rather than
            // hammering hardware that is already unwell.
            self.next_chunk_ns = now_ns.saturating_add(self.policy.readd_backoff_ns);
            return;
        }
        if !self.foreground_busy(now_ns) {
            self.next_chunk_ns = now_ns;
            return;
        }
        let duty = u64::from(self.policy.busy_duty_percent.clamp(1, 100));
        let elapsed = now_ns.saturating_sub(started_ns);
        self.next_chunk_ns = now_ns.saturating_add(elapsed.saturating_mul(100 - duty) / duty);
    }

    fn foreground_busy(&self, now_ns: u64) -> bool {
        self.last_foreground_ns
            .is_some_and(|at| now_ns.saturating_sub(at) < self.policy.foreground_idle_ns)
    }

    /// The soonest time at which [`Self::next_action`] could answer differently:
    /// a paced-out chunk becoming runnable, the scrub period elapsing, or a
    /// faulted member's backoff expiring.
    ///
    /// It reads the same [`chunk_pending`] predicate the decision does, so the
    /// caller is never woken for a chunk the next decision would not run —
    /// notably a scrub paused behind a degraded array, which resumes on a health
    /// change rather than on a clock.
    fn idle_deadline<B: Block>(&self, array: &RaidArray<'_, B>) -> Option<u64> {
        let next_retry = self
            .retries
            .iter()
            .filter(|retry| retry.armed)
            .map(|retry| retry.due_ns)
            .min();
        let next_cycle = if chunk_pending(array).is_some() {
            Some(self.next_chunk_ns)
        } else if array.health() == ArrayHealth::Optimal {
            Some(self.next_scrub_ns)
        } else {
            None
        };
        match (next_retry, next_cycle) {
            (Some(retry), Some(cycle)) => Some(retry.min(cycle)),
            (retry, cycle) => retry.or(cycle),
        }
    }
}

/// The bounded maintenance chunk the array has outstanding, before pacing is
/// considered: a rebuild to advance, or a scrub pass to carry on with.
///
/// A scrub only counts while the array is fully redundant. A pass interrupted
/// by a member dropping out keeps its cursor and waits, so the array spends its
/// I/O restoring redundancy first and then resumes the verification it had
/// already partly done.
fn chunk_pending<B: Block>(array: &RaidArray<'_, B>) -> Option<MaintenanceAction> {
    if array.needs_resync() {
        return Some(MaintenanceAction::Resync);
    }
    if array.scrubbing() && array.health() == ArrayHealth::Optimal {
        return Some(MaintenanceAction::Scrub);
    }
    None
}

#[cfg(test)]
mod tests;
