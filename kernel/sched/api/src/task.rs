//! Task lifecycle vocabulary shared by every scheduler policy.
//!
//! A task is the smallest unit a scheduler dispatches. Conceptually it is
//! an in-kernel thread of control: an identity, a state machine, and a
//! body to execute on its turn. These types are policy-neutral — the
//! concrete per-task storage (the boxed body, the atomics) lives in each
//! `kernel/sched/<impl>` crate, not here.
//!
//! ## State machine
//!
//! ```text
//!         spawn                     unpark
//!           │                          │
//!           ▼                          │
//!         Ready ◀──── Yield ────── Running ──── Park ───▶ Parked
//!                                     │                     │
//!                                     └──── Exit ───▶ Exited
//! ```
//!
//! `park`, `unpark`, and `exit` are *cancellation-safe*: an implementation
//! may be issued one while the task is running on another CPU and must
//! apply it at the next safe point. See `docs/src/architecture/scheduler.md`
//! for the full invariants.

use alloc::collections::BTreeSet;

use tairix_rng::{FastRng, RandU64, StreamKey};
use tairix_sync::SpinLock;

use crate::arch::CpuId;
use crate::error::{SchedError, SchedResult};

/// Stable identifier for a task, and the identity its process is known by.
///
/// Ids are **drawn at random** by [`choose_task_id`], never counted up, so
/// one id discloses nothing about how many tasks the system has admitted or
/// when. The draw spans `1..=`[`MAX_TASK_ID`], the ABI's pid range: a task id
/// is also the pid the `wait`/`signal` surface names it by, and the value
/// peers pack under a namespace tag to derive an IPC endpoint id.
///
/// A *live* id is never handed out twice — admission refuses a candidate the
/// policy already holds — but an id whose task has exited may be drawn again,
/// as on any system that bounds its pid space. Nothing relies on it not
/// happening: state keyed by a task id is dropped when its task dies, and
/// where a stale reference must be told apart from a fresh occupant of the
/// same number, the 128-bit `ProcId` process-instance identity is what
/// distinguishes them.
pub type TaskId = u64;

/// The reserved "no task" id: never drawn, never admitted.
pub const NO_TASK: TaskId = 0;

/// The reserved id of PID 1 (`init`).
///
/// Never drawn by [`choose_task_id`], so the boot path can admit the system's
/// first process under the well-known number a user expects to find it at
/// ([`crate::SchedulerPolicy::spawn_parked_as`]).
pub const INIT_TASK_ID: TaskId = 1;

/// The first id a draw may yield; everything below is reserved for the
/// well-known identities above.
const FIRST_DRAWN_TASK_ID: TaskId = 2;

/// The largest id a draw may yield: the ABI's own bound on a pid.
///
/// The bound lives in `lib/abi` because a pid is an ABI value — it round
/// trips through a signed syscall argument, and peers pack it under a
/// namespace tag to derive an IPC endpoint id — so the scheduler takes the
/// range rather than choosing one that would silently break those packings.
pub const MAX_TASK_ID: TaskId = tairix_abi::PID_MAX;

/// How many candidates a draw tries before failing closed.
///
/// A fixed bound rather than an unbounded retry: the loop must terminate even
/// if the liveness test rejects everything, and over a 64-bit space with a
/// live task count bounded by memory the first candidate is free with
/// overwhelming probability.
const DRAW_ATTEMPTS: usize = 8;

/// The generator's pre-boot seed, used until [`seed_task_ids`] publishes the
/// platform's own. Only the kernel's own service tasks are admitted that
/// early, so a deterministic prefix discloses nothing a user can observe.
const TASK_ID_BOOT_SEED: u64 = 0x5441_534B_5F49_4400;

/// The one process-wide task-id generator.
///
/// System-wide rather than per-policy so no two schedulers can mint the same
/// id: a process-global keyed by task id (the signal kill gate, a wait queue,
/// the endpoint registry) cannot tell two schedulers' tasks apart, so a
/// second instance drawing from its own counter would alias them.
///
/// The generator is the *unpredictable* one, and that matters even though an
/// id is not an authority. Reaching a task is gated by capability and never
/// by naming its id, so guessing one grants nothing — but the endpoint
/// registry answers whether an id is live, and that is an existence oracle. A
/// predictable generator turns a handful of observed ids into the whole
/// stream, and thus into every live and future task and endpoint id on the
/// machine, across every tenant sharing it. Admitting a task costs thousands
/// of cycles, so a cipher-backed draw is free by comparison.
static TASK_IDS: SpinLock<FastRng> = SpinLock::new(FastRng::seed_from_u64(TASK_ID_BOOT_SEED));

/// Re-seed the process-wide generator from the platform's randomness, so ids
/// differ across boots as well as across tasks.
///
/// The boot path calls this once, as soon as the kernel's random reserve can
/// serve a draw. The key is taken at full width rather than stretched from a
/// `u64`, so the generator's entropy is the platform's and not 64 bits of it.
/// A later call simply re-keys: the generator holds no published state, so
/// re-keying costs nothing and is never a security event.
pub fn seed_task_ids(key: &StreamKey) {
    *TASK_IDS.lock() = FastRng::from_key(key);
}

/// Choose the id a policy registers a new task under: the caller's reserved
/// one, or a fresh draw from the process-wide generator.
///
/// This is the one definition of the id-choosing rule every policy shares.
/// `is_live` answers whether the policy already holds a candidate, and the
/// caller evaluates it against its own task table **while holding the lock it
/// registers under** — so a live id can never be handed out twice, however
/// two admissions interleave.
///
/// `requested` is [`Some`] only for a reserved well-known identity
/// ([`INIT_TASK_ID`]); every other admission passes [`None`] and takes a
/// draw.
///
/// # Errors
///
/// * [`SchedError::TaskIdInUse`] when a live task already holds `requested`.
/// * [`SchedError::NoTaskIdAvailable`] when `requested` is [`NO_TASK`], and
///   when a draw's bounded run of candidates was rejected in full.
pub fn choose_task_id(
    requested: Option<TaskId>,
    is_live: impl Fn(TaskId) -> bool,
) -> SchedResult<TaskId> {
    // An id is unavailable if the policy holds a live task under it *or* it is
    // held against the draw ([`reserve_task_id`]) because an identity outlived
    // the task that carried it.
    let taken = |id: TaskId| is_live(id) || task_id_reserved(id);
    if requested.is_some() {
        // A reserved id needs no draw, so the shared generator is left
        // unlocked: the boot path admitting PID 1 never contends with one.
        return choose_reserved(requested, taken);
    }
    draw_id(&mut TASK_IDS.lock(), taken)
}

/// Ids withheld from the draw because an identity outlived the task that
/// carried it.
///
/// Empty in the common case — a process whose leader thread exits before its
/// siblings is the only producer — and bounded by the number of live
/// processes, since every entry is released by that process's teardown.
static RESERVED_TASK_IDS: SpinLock<BTreeSet<TaskId>> = SpinLock::new(BTreeSet::new());

/// Hold `id` against the draw until [`release_task_id`] returns it.
///
/// A process *is* its leader thread's task, so the two share a number. When
/// the leader exits first the scheduler reaps its task and the number becomes
/// drawable, while the process itself is still alive under a surviving sibling
/// thread — a later admission could then be issued the live process's id and
/// overwrite its capability record. This is the zombie leader: the task is
/// reaped normally and only the *number* is withheld.
///
/// Idempotent, so a repeated retire cannot corrupt the set.
///
/// # Errors
///
/// [`SchedError::NoTaskIdAvailable`] if `id` is [`NO_TASK`], which names no
/// task and is never drawn anyway.
pub fn reserve_task_id(id: TaskId) -> SchedResult<()> {
    if id == NO_TASK {
        return Err(SchedError::NoTaskIdAvailable);
    }
    RESERVED_TASK_IDS.lock().insert(id);
    Ok(())
}

/// Return an id [`reserve_task_id`] withheld. Idempotent: releasing an id that
/// was never reserved is not an error, so the teardown path can call it
/// unconditionally.
pub fn release_task_id(id: TaskId) {
    RESERVED_TASK_IDS.lock().remove(&id);
}

/// Whether `id` is currently held against the draw.
#[must_use]
pub fn task_id_reserved(id: TaskId) -> bool {
    RESERVED_TASK_IDS.lock().contains(&id)
}

/// Validate a caller-chosen reserved id.
///
/// # Errors
///
/// * [`SchedError::TaskIdInUse`] if a live task already holds it.
/// * [`SchedError::NoTaskIdAvailable`] if it is [`NO_TASK`] or absent.
fn choose_reserved(
    requested: Option<TaskId>,
    is_live: impl Fn(TaskId) -> bool,
) -> SchedResult<TaskId> {
    let Some(id) = requested.filter(|id| *id != NO_TASK) else {
        return Err(SchedError::NoTaskIdAvailable);
    };
    if is_live(id) {
        Err(SchedError::TaskIdInUse)
    } else {
        Ok(id)
    }
}

/// Draw an id no live task holds from `rng`.
///
/// Takes the generator as an argument so the rule — reserved ids are never
/// drawn, a live candidate is redrawn past, and a bounded run fails closed —
/// is exercised against a generator of the test's own rather than the
/// process-wide one every other admission shares.
///
/// # Errors
///
/// [`SchedError::NoTaskIdAvailable`] when every one of [`DRAW_ATTEMPTS`]
/// candidates was rejected — fail closed rather than register a task at an id
/// already in use.
fn draw_id<const N: usize>(
    rng: &mut FastRng<N>,
    is_live: impl Fn(TaskId) -> bool,
) -> SchedResult<TaskId> {
    for _ in 0..DRAW_ATTEMPTS {
        let candidate = rng.next_u64() & MAX_TASK_ID;
        if candidate >= FIRST_DRAWN_TASK_ID && !is_live(candidate) {
            return Ok(candidate);
        }
    }
    Err(SchedError::NoTaskIdAvailable)
}

#[cfg(test)]
mod id_tests {
    use super::{
        choose_reserved, choose_task_id, draw_id, seed_task_ids, FIRST_DRAWN_TASK_ID, INIT_TASK_ID,
        NO_TASK,
    };
    use crate::error::SchedError;
    use alloc::collections::BTreeSet;
    use tairix_rng::{FastRng, RandU64, StreamKey};

    /// A generator of this test's own, so what it draws never depends on what
    /// another test drew from the process-wide one.
    fn rng(seed: u64) -> FastRng {
        FastRng::seed_from_u64(seed)
    }

    /// A full-width key, so the re-key path is exercised with the shape the
    /// boot path hands in.
    fn key(fill: u8) -> StreamKey {
        [fill; tairix_rng::STREAM_KEY_LEN]
    }

    #[test]
    fn a_draw_never_yields_a_reserved_id() {
        let mut rng = rng(1);
        for _ in 0..10_000 {
            let id = draw_id(&mut rng, |_| false).expect("a draw succeeds");
            assert_ne!(id, NO_TASK);
            assert_ne!(id, INIT_TASK_ID);
        }
    }

    /// Every id survives the round trip through the signed pid the
    /// `wait`/`signal` surface names it by — a negative one would name a task
    /// no caller could reach — and stays inside the ABI's pid bound, which is
    /// what keeps a tag-packed endpoint id derived from it lossless.
    #[test]
    fn every_drawn_id_is_a_positive_pid_within_the_abi_bound() {
        let mut rng = rng(7);
        for _ in 0..10_000 {
            let id = draw_id(&mut rng, |_| false).expect("a draw succeeds");
            assert!(id <= tairix_abi::PID_MAX);
            assert!(id.cast_signed() > 0, "{id} is not a positive pid");
        }
    }

    #[test]
    fn ids_are_not_a_counter_and_no_live_id_is_reissued() {
        let mut rng = rng(2);
        let mut seen = BTreeSet::new();
        let mut successors = 0usize;
        let mut previous: Option<u64> = None;
        for _ in 0..4_096 {
            let id = draw_id(&mut rng, |c| seen.contains(&c)).expect("a draw succeeds");
            assert!(seen.insert(id), "a live id is never handed out twice");
            if previous.is_some_and(|p| id == p.wrapping_add(1)) {
                successors += 1;
            }
            previous = Some(id);
        }
        assert_eq!(
            successors, 0,
            "ids must carry no sequence, but one followed its predecessor"
        );
    }

    #[test]
    fn a_live_candidate_is_redrawn_past() {
        // Reject the first candidate this generator offers: the draw must go
        // on rather than return an id a live task holds.
        let first = rng(3).next_u64();
        let id = draw_id(&mut rng(3), |c| c == first).expect("a later draw succeeds");
        assert_ne!(id, first);
    }

    #[test]
    fn a_draw_that_can_never_succeed_fails_closed() {
        assert_eq!(
            draw_id(&mut rng(4), |_| true),
            Err(SchedError::NoTaskIdAvailable)
        );
    }

    #[test]
    fn a_reserved_id_is_honoured_but_refused_when_live() {
        assert_eq!(
            choose_reserved(Some(INIT_TASK_ID), |_| false),
            Ok(INIT_TASK_ID)
        );
        assert_eq!(
            choose_reserved(Some(INIT_TASK_ID), |c| c == INIT_TASK_ID),
            Err(SchedError::TaskIdInUse)
        );
        assert_eq!(
            choose_reserved(Some(NO_TASK), |_| false),
            Err(SchedError::NoTaskIdAvailable)
        );
    }

    /// The process-wide path: whatever another test has drawn from the shared
    /// generator, and whenever the boot path re-seeds it, a draw still
    /// answers with a usable non-reserved id.
    #[test]
    fn the_shared_generator_serves_usable_ids_across_a_re_seed() {
        assert!(choose_task_id(None, |_| false).expect("a draw succeeds") >= FIRST_DRAWN_TASK_ID);
        seed_task_ids(&key(0x5a));
        assert!(
            choose_task_id(None, |_| false).expect("a draw succeeds after re-seeding")
                >= FIRST_DRAWN_TASK_ID
        );
        assert_eq!(
            choose_task_id(Some(INIT_TASK_ID), |_| false),
            Ok(INIT_TASK_ID)
        );
    }
}

/// Priority band a task occupies.
///
/// Three bands are sufficient for an MLFQ-style policy (see
/// `docs/src/architecture/scheduler.md`). Adding more bands is an explicit
/// interface change (no interface creep): a run-queue
/// type sizes itself with `Priority::COUNT` worth of per-CPU deques.
#[repr(u8)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum Priority {
    /// Highest priority: interactive / short-running tasks.
    High = 0,
    /// Default priority: most kernel work.
    Normal = 1,
    /// Background work; ready to be preempted by either band above.
    Low = 2,
}

impl Priority {
    /// Number of bands. Run-queues sized at construction with this constant.
    pub const COUNT: usize = 3;

    /// Returns the band index (`0..COUNT`).
    #[must_use]
    pub const fn as_index(self) -> usize {
        self as u8 as usize
    }

    /// Returns the priority for a band index, or `None` for out-of-range.
    #[must_use]
    pub const fn from_index(i: usize) -> Option<Self> {
        match i {
            0 => Some(Self::High),
            1 => Some(Self::Normal),
            2 => Some(Self::Low),
            _ => None,
        }
    }

    /// One step less urgent (or `self` if already [`Priority::Low`]).
    ///
    /// Used by an MLFQ demotion rule: a task that consumes a full
    /// quantum without yielding voluntarily is demoted on the next
    /// re-enqueue.
    #[must_use]
    pub const fn demote(self) -> Self {
        match self {
            Self::High => Self::Normal,
            Self::Normal | Self::Low => Self::Low,
        }
    }
}

/// Scheduling class a task competes in.
///
/// Orthogonal to [`Priority`], which selects a fair-share weight and core
/// class *within* the time-shared band: the scheduling class decides which
/// band competes at all. The rule every policy enforces is strict:
///
/// * A ready [`SchedClass::Realtime`] task is dispatched before **any**
///   [`SchedClass::TimeShared`] task on the same CPU, regardless of the
///   time-shared task's accumulated virtual runtime, priority, or how long
///   it has waited.
/// * A running real-time task is **never** preempted in favour of a
///   time-shared task. Only another real-time task (round-robin among equal
///   peers on the CPU's periodic tick), a voluntary block/yield, or
///   termination takes the CPU from it.
/// * Real-time peers on one CPU are ordered **FIFO** (arrival order); the
///   periodic preemption tick rotates the running peer to the back so equal
///   real-time tasks share the CPU and none monopolises it (round-robin,
///   the `SCHED_RR` shape).
///
/// This is the strict-priority guarantee an interrupt-serving driver needs:
/// woken by its device IRQ, it must run *now*, ahead of any CPU-bound
/// workload, so a report/completion is captured before the hardware ring it
/// polls drains — the microkernel analogue of Linux's threaded-IRQ /
/// `SCHED_FIFO` context. Entry to the class is capability-gated
/// (`CAP_SCHED_REALTIME`) at the syscall boundary; the scheduler itself only
/// honours the class, it does not decide who may hold it.
///
/// A real-time task that never blocks would monopolise its CPU against
/// time-shared work; that is inherent to strict priority and is bounded by
/// making the class a guarded capability granted only to trusted,
/// IRQ-driven drivers, exactly as a `SCHED_FIFO` grant is trusted on other
/// systems. The default class is [`SchedClass::TimeShared`].
#[repr(u8)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Default)]
pub enum SchedClass {
    /// Strict-priority real-time band: picked before any time-shared task
    /// and never preempted by one. FIFO among peers, round-robin on the
    /// periodic tick.
    Realtime = 0,
    /// Default fair-share band, governed by the policy's own algorithm
    /// (CFQ / EEVDF / MLFQ). The default class of every task.
    #[default]
    TimeShared = 1,
}

impl SchedClass {
    /// Returns the raw discriminant as stored in an atomic.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// Inverse of [`Self::as_u8`]; returns `None` for unknown encodings.
    #[must_use]
    pub const fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::Realtime),
            1 => Some(Self::TimeShared),
            _ => None,
        }
    }

    /// Whether this is the strict-priority [`SchedClass::Realtime`] band.
    #[must_use]
    pub const fn is_realtime(self) -> bool {
        matches!(self, Self::Realtime)
    }
}

/// Task lifecycle state.
///
/// An implementation stores this (typically in an `AtomicU8`) inside each
/// task. Allowed transitions:
///
/// | from      | to         | trigger                         |
/// | --------- | ---------- | ------------------------------- |
/// | `Ready`   | `Running`  | scheduler picks the task        |
/// | `Running` | `Ready`    | body returns [`TaskAction::Yield`] |
/// | `Running` | `Parked`   | body returns [`TaskAction::Park`] or external `park` |
/// | `Running` | `Exited`   | body returns [`TaskAction::Exit`] or external `exit` |
/// | `Ready`   | `Parked`   | external `park` while queued    |
/// | `Parked`  | `Ready`    | external `unpark`               |
/// | any       | `Exited`   | external `exit`                 |
#[repr(u8)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum TaskState {
    /// In a run queue or about to be enqueued.
    Ready = 0,
    /// Currently executing on some CPU.
    Running = 1,
    /// Not runnable until a matching `unpark`.
    Parked = 2,
    /// Terminal state. The task body has been dropped.
    Exited = 3,
}

impl TaskState {
    /// Returns the raw discriminant as stored in an atomic.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// Inverse of [`Self::as_u8`]; returns `None` for unknown encodings.
    #[must_use]
    pub const fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::Ready),
            1 => Some(Self::Running),
            2 => Some(Self::Parked),
            3 => Some(Self::Exited),
            _ => None,
        }
    }
}

/// What the scheduler should do with a task whose body has just returned.
///
/// Returned from the closure passed to [`crate::SchedulerPolicy::spawn`].
/// Combined with externally-issued `park`/`unpark`/`exit`, this gives the
/// scheduler a full picture of the task's intent.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum TaskAction {
    /// Re-enqueue at the current priority (subject to MLFQ demotion).
    Yield,
    /// Transition to [`TaskState::Parked`]; do not re-enqueue.
    Park,
    /// Terminal: transition to [`TaskState::Exited`] and drop the body.
    Exit,
}

/// Argument passed to a task body on each scheduling step.
///
/// Tests and userland alike read the current CPU and tick to make
/// reproducible decisions (e.g. cooperative time-slice accounting).
#[derive(Copy, Clone, Debug)]
pub struct TaskContext {
    /// The CPU dispatching this task.
    pub cpu: CpuId,
    /// The arch-provided tick at the start of this step.
    pub tick: u64,
    /// The task's identity.
    pub task_id: TaskId,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn priority_round_trip() {
        for i in 0..Priority::COUNT {
            let p = Priority::from_index(i).expect("valid band");
            assert_eq!(p.as_index(), i);
        }
        assert!(Priority::from_index(Priority::COUNT).is_none());
    }

    #[test]
    fn priority_demote_saturates() {
        assert_eq!(Priority::High.demote(), Priority::Normal);
        assert_eq!(Priority::Normal.demote(), Priority::Low);
        assert_eq!(Priority::Low.demote(), Priority::Low);
    }

    #[test]
    fn sched_class_round_trip_and_default() {
        for c in [SchedClass::Realtime, SchedClass::TimeShared] {
            assert_eq!(SchedClass::from_u8(c.as_u8()), Some(c));
        }
        assert_eq!(SchedClass::from_u8(2), None);
        assert_eq!(SchedClass::default(), SchedClass::TimeShared);
        assert!(SchedClass::Realtime.is_realtime());
        assert!(!SchedClass::TimeShared.is_realtime());
    }

    #[test]
    fn task_state_round_trip() {
        for s in [
            TaskState::Ready,
            TaskState::Running,
            TaskState::Parked,
            TaskState::Exited,
        ] {
            assert_eq!(TaskState::from_u8(s.as_u8()), Some(s));
        }
        assert_eq!(TaskState::from_u8(99), None);
    }
}
