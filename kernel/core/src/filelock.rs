//! The advisory byte-range file-lock manager: the kernel side of the
//! `fs_lock` / `fs_lock_query` syscalls (`plans/FILELOCK.md`).
//!
//! A lock is owned by an **open file description** ([`OwnerId`], minted per
//! `fs_open` and shared by every descriptor duplicated or spawn-inherited
//! from it), keyed on the file's stable [`FileId`] rather than on a path, so
//! a rename moves neither the lock nor the identity two participants agree
//! on. What the manager owns is the conflict decision, the wait-for graph,
//! and the record bookkeeping; parking and waking are the handler's.
//!
//! # Indexes, and why these
//!
//! Per file the authoritative state is one non-overlapping ordered record
//! set *per owner*, which is what release-on-close and the replace/split
//! algebra walk. Conflict detection reads two projections of it:
//!
//! * `exclusive`, every exclusive record on the file in one ordered map.
//!   Exclusive records cannot overlap each other — that is what exclusivity
//!   means — so this map is non-overlapping and an interval query over it is
//!   an exact `O(log n)`: the predecessor of the range plus the records
//!   starting inside it are the whole candidate set.
//! * `shared_records`, a count. A shared request conflicts only with an
//!   exclusive holder, so it never reads further than the map above; an
//!   exclusive request against a file with no shared records stops there
//!   too, which is the whole cost for the record-per-row workload where
//!   every lock is exclusive.
//!
//! Only an exclusive request on a file that *does* hold shared records walks
//! owners, and it stops at the first conflict. That walk is over
//! participants — not over records — and the record count is bounded per
//! process by the `file-locks` resource limit, so it cannot grow without
//! someone being charged for it.
//!
//! # Fairness
//!
//! A request conflicts with the current holders and, when it is prepared to
//! wait, with any *earlier-queued* waiter whose range it overlaps
//! incompatibly. That ordering is what stops a stream of readers starving a
//! writer: once the writer queues, an arriving reader that overlaps it
//! queues behind rather than joining the holders ahead of it. Two
//! deliberate exemptions:
//!
//! * A non-blocking request tests only the holders. Its contract is "tell me
//!   whether the lock is free", and it cannot starve anyone by waiting.
//! * A request over a range this owner **already holds** — an upgrade or a
//!   downgrade — tests only the holders. Making it queue behind a newcomer
//!   could only deadlock it: it cannot release what the newcomer waits for
//!   without giving up the range it is converting.
//!
//! Because queue order strictly decreases along a queue edge, queue edges
//! alone cannot form a cycle; a deadlock always involves a holder.
//!
//! # Deadlock
//!
//! A blocking request whose grant would close a cycle is refused with
//! [`Errno::Deadlock`] rather than joining a
//! wait none of the participants could leave. The search is an iterative
//! breadth-first walk of the wait-for graph, bounded by the owners it has
//! already visited, so it neither recurses on a depth a hostile process
//! chooses nor expands an owner twice.
//!
//! Owners are descriptions, so the graph sees a cycle whenever each
//! participant blocks and holds through the same description — which is what
//! a program that locks the file it has open does, and covers every
//! self-deadlock. A cycle whose participants block through one description
//! while holding through a *different* one is not detected; that wait ends
//! when a participant closes or exits, and it stays interruptible
//! throughout, so it is recoverable rather than an unkillable hang. Linux
//! detects none of these cases for its equivalent (`F_OFD_SETLKW`) locks.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use tairix_abi::{Errno, FileId, LockConflict, LockMode, LockRange};
use tairix_kernel_sched_api::TaskId;
use tairix_kernel_sec::ProcessId;
use tairix_sync::SpinLock;

/// Identity of one open file description for locking purposes.
///
/// Minted per `fs_open` and carried by the description, so descriptors
/// duplicated or spawn-inherited from it share one owner while a second open
/// of the same file is a different one.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct OwnerId(u64);

/// The two modes a *held* record can be in. [`LockMode::Unlock`] asks for a
/// release, so it can never be one and is not representable here.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Held {
    /// Coexists with other owners' shared records.
    Shared,
    /// Excludes every other owner.
    Exclusive,
}

impl Held {
    /// The requested mode as a held one, or `None` for
    /// [`LockMode::Unlock`].
    #[must_use]
    pub const fn from_mode(mode: LockMode) -> Option<Self> {
        match mode {
            LockMode::Shared => Some(Self::Shared),
            LockMode::Exclusive => Some(Self::Exclusive),
            LockMode::Unlock => None,
        }
    }

    /// The ABI mode this record reports as.
    #[must_use]
    pub const fn as_mode(self) -> LockMode {
        match self {
            Self::Shared => LockMode::Shared,
            Self::Exclusive => LockMode::Exclusive,
        }
    }

    /// Whether two owners may both hold these modes over one byte, decided
    /// by the ABI's single conflict rule.
    #[must_use]
    const fn compatible_with(self, other: Self) -> bool {
        self.as_mode().compatible_with(other.as_mode())
    }
}

/// One owner's record: the range it covers and the mode it holds.
///
/// Keyed by `range.start()` in its owner's map. An owner's records never
/// overlap and same-mode records that abut are merged, so the set is the
/// minimal description of what that owner holds.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
struct Record {
    range: LockRange,
    held: Held,
}

/// Everything one owner holds on one file, plus the process its records are
/// charged to.
struct OwnerLocks {
    /// The process whose `file-locks` count these records consume, fixed
    /// when the owner first locked this file. A later split keeps the charge
    /// here rather than migrating it to whoever split the record.
    pid: ProcessId,
    records: BTreeMap<u64, Record>,
}

/// One file's lock state, alive only while something holds or awaits a lock
/// on it.
struct FileEntry {
    /// The wait key every waiter on this file registers under. Drawn from a
    /// monotonic counter and never reused, so a stale registration can never
    /// alias a later file's waiters.
    key: u64,
    owners: BTreeMap<OwnerId, OwnerLocks>,
    /// Every exclusive record on the file, non-overlapping and ordered by
    /// start — the exact interval index exclusivity makes possible.
    exclusive: BTreeMap<u64, (LockRange, OwnerId)>,
    /// How many shared records the file holds, so an exclusive request can
    /// skip the owner walk when there are none.
    shared_records: usize,
}

impl FileEntry {
    fn new(key: u64) -> Self {
        Self {
            key,
            owners: BTreeMap::new(),
            exclusive: BTreeMap::new(),
            shared_records: 0,
        }
    }

    /// Whether `owner` already holds any byte of `range`, which makes the
    /// request a conversion rather than an arrival.
    fn holds_any(&self, owner: OwnerId, range: LockRange) -> bool {
        self.owners
            .get(&owner)
            .is_some_and(|locks| overlapping(&locks.records, range).next().is_some())
    }

    /// The first holder in the way of `held` over `range`, ignoring
    /// `owner`'s own records.
    fn holder_conflict(&self, owner: OwnerId, range: LockRange, held: Held) -> Option<Conflict> {
        if let Some(found) = self.exclusive_conflict(owner, range) {
            return Some(found);
        }
        // A shared request is compatible with every shared record, so the
        // exclusive projection above was the whole question.
        if matches!(held, Held::Shared) || self.shared_records == 0 {
            return None;
        }
        self.owners
            .iter()
            .filter(|(&other, _)| other != owner)
            .find_map(|(&other, locks)| {
                overlapping(&locks.records, range)
                    .find(|record| !held.compatible_with(record.held))
                    .map(|record| Conflict {
                        owner: other,
                        held: record.held,
                        range: record.range,
                        pid: locks.pid,
                    })
            })
    }

    /// The first exclusive record overlapping `range` that `owner` does not
    /// hold.
    fn exclusive_conflict(&self, owner: OwnerId, range: LockRange) -> Option<Conflict> {
        let predecessor = self
            .exclusive
            .range(..range.start())
            .next_back()
            .filter(|(_, (held, _))| held.end_inclusive() >= range.start());
        predecessor
            .into_iter()
            .chain(self.exclusive.range(range.start()..=range.end_inclusive()))
            .find(|(_, (_, holder))| *holder != owner)
            .map(|(_, &(held_range, holder))| Conflict {
                owner: holder,
                held: Held::Exclusive,
                range: held_range,
                pid: self
                    .owners
                    .get(&holder)
                    .map_or(ProcessId(0), |locks| locks.pid),
            })
    }

    /// Make `range` be `new` for `owner`, applying the edits `plan`
    /// described and rebuilding the projections from the result.
    fn apply(&mut self, owner: OwnerId, pid: ProcessId, plan: &Plan) {
        let locks = self.owners.entry(owner).or_insert_with(|| OwnerLocks {
            pid,
            records: BTreeMap::new(),
        });
        for &start in &plan.remove {
            locks.records.remove(&start);
        }
        for &record in &plan.insert {
            locks.records.insert(record.range.start(), record);
        }
        if locks.records.is_empty() {
            self.owners.remove(&owner);
        }
        self.reindex();
    }

    /// Rebuild the conflict projections from the records they describe.
    ///
    /// Derived rather than nudged by a delta: an index that accumulates
    /// increments can drift from the records it is meant to describe, and a
    /// conflict check reading a drifted index is a lock granted twice.
    fn reindex(&mut self) {
        self.exclusive.clear();
        self.shared_records = 0;
        for (&owner, locks) in &self.owners {
            for record in locks.records.values() {
                match record.held {
                    Held::Shared => self.shared_records += 1,
                    Held::Exclusive => {
                        self.exclusive
                            .insert(record.range.start(), (record.range, owner));
                    }
                }
            }
        }
    }

    /// Whether nothing is held here any more.
    fn is_vacant(&self) -> bool {
        self.owners.is_empty()
    }
}

/// One task parked in `fs_lock`.
struct Waiter {
    file: FileId,
    owner: OwnerId,
    range: LockRange,
    held: Held,
    /// Arrival order across the whole registry. A waiter is blocked by
    /// earlier-queued waiters, never by later ones, so the queue relation is
    /// a strict order that cannot cycle on its own.
    seq: u64,
}

/// One acquisition request: who is asking, for what, and on what terms.
///
/// Named rather than positional because the eight values are easy to
/// transpose — a `pid` and a `task` are both integers, and a `limit` and a
/// range bound both read as numbers — and a transposition here would charge
/// the wrong process or bound the wrong range.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Request {
    /// The file's stable identity.
    pub file: FileId,
    /// The open file description asking.
    pub owner: OwnerId,
    /// The process to charge the records to, if this owner holds none yet.
    pub pid: ProcessId,
    /// The calling task, which is what parks and what the queue orders.
    pub task: TaskId,
    /// The range asked for.
    pub range: LockRange,
    /// The mode asked for.
    pub held: Held,
    /// The requester's `file-locks` bound.
    pub limit: u64,
    /// Whether the caller is prepared to park, which brings the queue
    /// ordering and the deadlock search into play.
    pub waiting: bool,
}

/// What one lock the caller cannot have looks like.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Conflict {
    /// The owner holding it.
    pub owner: OwnerId,
    /// The mode held.
    pub held: Held,
    /// The range held.
    pub range: LockRange,
    /// The process the record is charged to and reported for.
    pub pid: ProcessId,
}

impl Conflict {
    /// The record as the query syscall reports it.
    #[must_use]
    pub fn to_abi(self) -> LockConflict {
        LockConflict {
            mode: self.held.as_mode(),
            start: self.range.start(),
            len: self.range.wire_len(),
            pid: self.pid.0,
        }
    }
}

/// Why an acquisition could not be granted now.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Refusal {
    /// A holder is in the way; waiting could still succeed.
    Held(Conflict),
    /// An earlier-queued waiter is ahead; waiting could still succeed.
    Queued,
    /// Granting would close a cycle of waiters.
    Deadlock,
    /// The request would push the charged process past its `file-locks`
    /// bound.
    LimitExceeded,
}

impl Refusal {
    /// The stable errno the syscall reports.
    ///
    /// The one mapping, so the acquire path and the release path cannot
    /// disagree about what a refusal means to a caller. A holder in the way
    /// and an earlier waiter ahead are both "you would have had to wait":
    /// which of the two it was is the manager's business, not the caller's.
    #[must_use]
    pub const fn to_errno(self) -> Errno {
        match self {
            Self::Held(_) | Self::Queued => Errno::WouldBlock,
            Self::Deadlock => Errno::Deadlock,
            Self::LimitExceeded => Errno::LimitExceeded,
        }
    }
}

/// The waiters a state change made able to progress, and the file wait key
/// they are registered under.
#[derive(Debug, Default, Eq, PartialEq)]
pub struct Wakes {
    /// The file wait key, or `None` when nothing is to be woken.
    pub key: Option<u64>,
    /// The waiters whose blocked ranges the change freed.
    pub tasks: Vec<TaskId>,
}

impl Wakes {
    /// Whether anything is to be woken.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tasks.is_empty()
    }
}

/// Registry state behind [`REGISTRY`].
struct Registry {
    files: BTreeMap<FileId, FileEntry>,
    /// Which files each owner holds records on, so releasing a description
    /// does not scan every locked file.
    owned: BTreeMap<OwnerId, BTreeSet<FileId>>,
    /// Live record count per charged process — the `file-locks` usage.
    charged: BTreeMap<ProcessId, u64>,
    waiters: BTreeMap<TaskId, Waiter>,
    /// The tasks queued per file, so a release considers that file's waiters
    /// rather than every waiter in the system.
    queued: BTreeMap<FileId, BTreeSet<TaskId>>,
    next_key: u64,
    next_seq: u64,
}

/// The lock-guarded registry: pure data behind a spin lock, never a mutable
/// static.
static REGISTRY: SpinLock<Registry> = SpinLock::new(Registry {
    files: BTreeMap::new(),
    owned: BTreeMap::new(),
    charged: BTreeMap::new(),
    waiters: BTreeMap::new(),
    queued: BTreeMap::new(),
    next_key: 1,
    next_seq: 1,
});

/// Source of [`OwnerId`]s. Monotonic and never reused, so a reclaimed
/// description's id cannot alias a later one's locks.
static NEXT_OWNER: AtomicU64 = AtomicU64::new(1);

/// How many lock records exist system-wide, so the descriptor-release path
/// pays one relaxed load rather than the registry lock while nothing is
/// locked.
static LIVE_RECORDS: AtomicU64 = AtomicU64::new(0);

/// Serialise a test that touches the process-global lock registry, leaving it
/// empty for the caller.
///
/// The registry, the per-process charge map and the system-wide live count are
/// one machine's worth of state, and the reset this performs clears all of it
/// — so a test still holding a lock while a sibling reset ran would find it
/// gone, and grant a conflicting request. This module's own tests and the
/// `fs_lock` syscall-handler tests all hold this one lock, which is why it
/// lives here rather than beside either of them.
#[cfg(test)]
pub(crate) fn registry_guard() -> std::sync::MutexGuard<'static, ()> {
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let guard = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    reset_for_test();
    guard
}

/// Mint the owner identity for a fresh open file description.
#[must_use]
pub fn mint_owner() -> OwnerId {
    OwnerId(NEXT_OWNER.fetch_add(1, Ordering::Relaxed))
}

/// Whether any advisory lock is currently held system-wide.
#[must_use]
pub fn locks_present() -> bool {
    LIVE_RECORDS.load(Ordering::Relaxed) != 0
}

/// The live record count charged to `pid` — its `file-locks` usage.
#[must_use]
pub fn usage(pid: ProcessId) -> u64 {
    REGISTRY.lock().charged.get(&pid).copied().unwrap_or(0)
}

/// Report the first lock standing in the way of `held` over `range` on
/// `file` for `owner`, or `None` when the request would be granted.
///
/// Holders only: a queued waiter holds nothing, so reporting one would name
/// a lock that does not exist.
#[must_use]
pub fn query(file: FileId, owner: OwnerId, range: LockRange, held: Held) -> Option<Conflict> {
    let reg = REGISTRY.lock();
    reg.files
        .get(&file)
        .and_then(|entry| entry.holder_conflict(owner, range, held))
}

/// Take the lock `request` asks for.
///
/// A request that is prepared to wait ([`Request::waiting`]) is measured
/// against the queue ordering and the deadlock search as well as the
/// holders; a non-blocking one tests only the holders.
///
/// On success the range is held and every earlier record of this owner
/// inside it is replaced, so an upgrade, a downgrade, and a re-lock are one
/// operation. Returns the waiters the change freed — a downgrade from
/// exclusive to shared releases anyone queued on those bytes.
///
/// # Errors
///
/// The [`Refusal`] saying why: a holder in the way, an earlier waiter ahead,
/// a cycle the grant would close, or the record bound.
pub fn acquire(request: &Request) -> Result<Wakes, Refusal> {
    let &Request {
        file,
        owner,
        pid,
        task,
        range,
        held,
        limit,
        waiting,
    } = request;
    let mut reg = REGISTRY.lock();
    let seq = reg.waiters.get(&task).map_or(u64::MAX, |w| w.seq);
    let converting = reg
        .files
        .get(&file)
        .is_some_and(|entry| entry.holds_any(owner, range));

    if let Some(conflict) = reg
        .files
        .get(&file)
        .and_then(|entry| entry.holder_conflict(owner, range, held))
    {
        if waiting && reg.would_deadlock(file, owner, range, held) {
            return Err(Refusal::Deadlock);
        }
        return Err(Refusal::Held(conflict));
    }
    if waiting && !converting && reg.queue_blocked(file, owner, range, held, seq) {
        return Err(Refusal::Queued);
    }

    let plan = reg.files.get(&file).map_or_else(
        || Plan::fresh(range, held),
        |entry| {
            entry.owners.get(&owner).map_or_else(
                || Plan::fresh(range, held),
                |locks| Plan::build(&locks.records, range, Some(held)),
            )
        },
    );
    let charge_pid = reg
        .files
        .get(&file)
        .and_then(|entry| entry.owners.get(&owner))
        .map_or(pid, |locks| locks.pid);
    reg.price(charge_pid, plan.delta(), limit)?;

    let key = reg.ensure_file(file);
    let _ = key;
    if let Some(entry) = reg.files.get_mut(&file) {
        entry.apply(owner, pid, &plan);
    }
    reg.settle_charge(charge_pid, plan.delta());
    reg.owned.entry(owner).or_default().insert(file);
    let wakes = reg.collect_wakes(file, range);
    reg.drop_if_vacant(file);
    Ok(wakes)
}

/// Release whatever `owner` holds over `range` on `file`, splitting a record
/// that straddles the range's edges.
///
/// Bounded by `limit` for the same reason an acquisition is: unlocking the
/// middle of a held range turns one record into two, so a loop could
/// manufacture records without ever locking a new byte. Releasing the whole
/// of a held range never grows the count, so a caller at the bound always
/// has a way out.
///
/// # Errors
///
/// [`Refusal::LimitExceeded`] when the split would pass the record bound.
pub fn release(
    file: FileId,
    owner: OwnerId,
    range: LockRange,
    limit: u64,
) -> Result<Wakes, Refusal> {
    let mut reg = REGISTRY.lock();
    let Some((charge_pid, plan)) = reg.files.get(&file).and_then(|entry| {
        entry
            .owners
            .get(&owner)
            .map(|locks| (locks.pid, Plan::build(&locks.records, range, None)))
    }) else {
        return Ok(Wakes::default());
    };
    reg.price(charge_pid, plan.delta(), limit)?;
    if let Some(entry) = reg.files.get_mut(&file) {
        entry.apply(owner, charge_pid, &plan);
        if !entry.owners.contains_key(&owner) {
            if let Some(files) = reg.owned.get_mut(&owner) {
                files.remove(&file);
                if files.is_empty() {
                    reg.owned.remove(&owner);
                }
            }
        }
    }
    reg.settle_charge(charge_pid, plan.delta());
    let wakes = reg.collect_wakes(file, range);
    reg.drop_if_vacant(file);
    Ok(wakes)
}

/// Release everything `owner` holds, on every file — the last descriptor on
/// a description closing, which a process exit does for all of them.
///
/// Never refuses: dropping records only ever lowers the count.
#[must_use]
pub fn release_owner(owner: OwnerId) -> Vec<Wakes> {
    if !locks_present() {
        return Vec::new();
    }
    let mut reg = REGISTRY.lock();
    let Some(files) = reg.owned.remove(&owner) else {
        return Vec::new();
    };
    let mut wakes = Vec::new();
    for file in files {
        let Some(entry) = reg.files.get_mut(&file) else {
            continue;
        };
        let Some(locks) = entry.owners.remove(&owner) else {
            continue;
        };
        entry.reindex();
        let spans: Vec<LockRange> = locks.records.values().map(|record| record.range).collect();
        let freed = i64::try_from(spans.len()).unwrap_or(i64::MAX);
        reg.settle_charge(locks.pid, -freed);
        for span in spans {
            let released = reg.collect_wakes(file, span);
            if !released.is_empty() {
                wakes.push(released);
            }
        }
        reg.drop_if_vacant(file);
    }
    wakes
}

/// Join the queue for `held` over `range` on `file`, returning the wait key
/// to park under.
///
/// Registered before the caller parks, so a release landing in the window
/// between the failed attempt and the park still finds the waiter and wakes
/// it rather than leaving it parked on a lock that is already free.
#[must_use]
pub fn enqueue(file: FileId, owner: OwnerId, task: TaskId, range: LockRange, held: Held) -> u64 {
    let mut reg = REGISTRY.lock();
    let key = reg.ensure_file(file);
    // Re-queuing after a spurious wake keeps the original arrival order, so
    // a waiter that loops never loses its place to a newcomer.
    let seq = if let Some(existing) = reg.waiters.get(&task) {
        existing.seq
    } else {
        let seq = reg.next_seq;
        reg.next_seq = reg.next_seq.saturating_add(1);
        seq
    };
    reg.waiters.insert(
        task,
        Waiter {
            file,
            owner,
            range,
            held,
            seq,
        },
    );
    reg.queued.entry(file).or_default().insert(task);
    key
}

/// Leave the queue, whether the wait ended in a grant, a timeout, or a
/// signal. Idempotent.
pub fn dequeue(task: TaskId) {
    let mut reg = REGISTRY.lock();
    let Some(waiter) = reg.waiters.remove(&task) else {
        return;
    };
    if let Some(tasks) = reg.queued.get_mut(&waiter.file) {
        tasks.remove(&task);
        if tasks.is_empty() {
            reg.queued.remove(&waiter.file);
        }
    }
    reg.drop_if_vacant(waiter.file);
}

/// The record edits one replace performs: which starts leave, and which
/// records arrive. Computed before anything is touched so the caller can
/// price it against the resource limit first.
struct Plan {
    remove: Vec<u64>,
    insert: Vec<Record>,
}

impl Plan {
    /// The edits for an owner that holds nothing on this file yet.
    fn fresh(range: LockRange, held: Held) -> Self {
        Self {
            remove: Vec::new(),
            insert: Vec::from([Record { range, held }]),
        }
    }

    /// The edits that make `range` be `new` in `records`.
    ///
    /// A record of the *same* mode that overlaps or merely touches the range
    /// is absorbed into one covering record, so extending a lock byte by
    /// byte does not accumulate a record per byte. A record of the *other*
    /// mode is cut back to the parts outside the range, which is how a
    /// partial unlock and a partial upgrade split one record into two.
    fn build(records: &BTreeMap<u64, Record>, range: LockRange, new: Option<Held>) -> Self {
        let mut remove = Vec::new();
        let mut insert = Vec::new();
        let mut span = range;
        for record in touching(records, range) {
            let same_mode = new == Some(record.held);
            if !same_mode && !record.range.overlaps(range) {
                // Abutting, different mode: untouched.
                continue;
            }
            remove.push(record.range.start());
            if same_mode {
                span = span.joined(record.range);
                continue;
            }
            if record.range.start() < range.start() {
                if let Ok(prefix) =
                    LockRange::between(record.range.start(), range.start().saturating_sub(1))
                {
                    insert.push(Record {
                        range: prefix,
                        held: record.held,
                    });
                }
            }
            if record.range.end_inclusive() > range.end_inclusive() {
                if let Some(from) = range.end_inclusive().checked_add(1) {
                    if let Ok(suffix) = LockRange::between(from, record.range.end_inclusive()) {
                        insert.push(Record {
                            range: suffix,
                            held: record.held,
                        });
                    }
                }
            }
        }
        if let Some(held) = new {
            insert.push(Record { range: span, held });
        }
        Self { remove, insert }
    }

    /// How many records the owner's count changes by.
    fn delta(&self) -> i64 {
        let added = i64::try_from(self.insert.len()).unwrap_or(i64::MAX);
        let removed = i64::try_from(self.remove.len()).unwrap_or(i64::MAX);
        added - removed
    }
}

/// The records of one non-overlapping ordered set that overlap `range` or
/// touch it end to end.
///
/// The predecessor is the only record starting before `range` that can reach
/// it — that is what non-overlap buys — so the candidate set is that one
/// plus everything starting at or just past the range, and the walk costs
/// `O(log n)` plus the records it yields.
fn touching(
    records: &BTreeMap<u64, Record>,
    range: LockRange,
) -> impl Iterator<Item = Record> + '_ {
    let predecessor = records
        .range(..range.start())
        .next_back()
        .filter(|(_, record)| record.range.overlaps(range) || record.range.abuts(range))
        .map(|(_, record)| *record);
    let upper = range
        .end_inclusive()
        .checked_add(1)
        .unwrap_or(range.end_inclusive());
    predecessor.into_iter().chain(
        records
            .range(range.start()..=upper)
            .map(|(_, record)| *record)
            .filter(move |record| record.range.overlaps(range) || record.range.abuts(range)),
    )
}

/// The records of one owner that overlap `range`.
fn overlapping(
    records: &BTreeMap<u64, Record>,
    range: LockRange,
) -> impl Iterator<Item = Record> + '_ {
    touching(records, range).filter(move |record| record.range.overlaps(range))
}

impl Registry {
    /// The wait key for `file`, creating its entry if nothing has locked or
    /// awaited it yet.
    fn ensure_file(&mut self, file: FileId) -> u64 {
        let fresh = self.next_key;
        let entry = self
            .files
            .entry(file)
            .or_insert_with(|| FileEntry::new(fresh));
        if entry.key == fresh {
            self.next_key = self.next_key.saturating_add(1);
        }
        entry.key
    }

    /// Refuse a plan that would push `pid` past `limit` records.
    ///
    /// # Errors
    ///
    /// [`Refusal::LimitExceeded`] when the growth does not fit.
    fn price(&self, pid: ProcessId, delta: i64, limit: u64) -> Result<(), Refusal> {
        if delta <= 0 {
            return Ok(());
        }
        let held = self.charged.get(&pid).copied().unwrap_or(0);
        if held.saturating_add(delta.unsigned_abs()) > limit {
            return Err(Refusal::LimitExceeded);
        }
        Ok(())
    }

    /// Charge or credit `delta` records to `pid`, keeping the system-wide
    /// live count in step.
    fn settle_charge(&mut self, pid: ProcessId, delta: i64) {
        if delta == 0 {
            return;
        }
        let magnitude = delta.unsigned_abs();
        let slot = self.charged.entry(pid).or_insert(0);
        if delta > 0 {
            *slot = slot.saturating_add(magnitude);
            LIVE_RECORDS.fetch_add(magnitude, Ordering::Relaxed);
        } else {
            *slot = slot.saturating_sub(magnitude);
            let shed = magnitude.min(LIVE_RECORDS.load(Ordering::Relaxed));
            LIVE_RECORDS.fetch_sub(shed, Ordering::Relaxed);
        }
        if *slot == 0 {
            self.charged.remove(&pid);
        }
    }

    /// Forget a file nothing holds or awaits, so the registry carries only
    /// files something is actually using.
    fn drop_if_vacant(&mut self, file: FileId) {
        if self.queued.contains_key(&file) {
            return;
        }
        if self.files.get(&file).is_some_and(FileEntry::is_vacant) {
            self.files.remove(&file);
        }
    }

    /// The queued waiters whose blocked ranges `range` overlaps — exactly
    /// those a change to it can advance, so a release of one range never
    /// disturbs a waiter queued on a disjoint one.
    fn collect_wakes(&self, file: FileId, range: LockRange) -> Wakes {
        let Some(entry) = self.files.get(&file) else {
            return Wakes::default();
        };
        let tasks: Vec<TaskId> = self
            .queued
            .get(&file)
            .into_iter()
            .flatten()
            .copied()
            .filter(|task| {
                self.waiters
                    .get(task)
                    .is_some_and(|waiter| waiter.range.overlaps(range))
            })
            .collect();
        Wakes {
            key: (!tasks.is_empty()).then_some(entry.key),
            tasks,
        }
    }

    /// Whether an earlier-queued waiter blocks `held` over `range`.
    fn queue_blocked(
        &self,
        file: FileId,
        owner: OwnerId,
        range: LockRange,
        held: Held,
        seq: u64,
    ) -> bool {
        self.file_waiters(file).any(|waiter| {
            waiter.owner != owner
                && waiter.seq < seq
                && waiter.range.overlaps(range)
                && !held.compatible_with(waiter.held)
        })
    }

    /// The waiters queued on `file`.
    fn file_waiters(&self, file: FileId) -> impl Iterator<Item = &Waiter> {
        self.queued
            .get(&file)
            .into_iter()
            .flatten()
            .filter_map(|task| self.waiters.get(task))
    }

    /// Every owner that blocks `held` over `range` on `file` — the holders
    /// in the way, plus the earlier-queued waiters that go first.
    fn blocking_owners(
        &self,
        file: FileId,
        owner: OwnerId,
        range: LockRange,
        held: Held,
    ) -> BTreeSet<OwnerId> {
        let mut blockers = BTreeSet::new();
        if let Some(entry) = self.files.get(&file) {
            for (&other, locks) in &entry.owners {
                if other == owner {
                    continue;
                }
                if overlapping(&locks.records, range)
                    .any(|record| !held.compatible_with(record.held))
                {
                    blockers.insert(other);
                }
            }
        }
        for waiter in self.file_waiters(file) {
            if waiter.owner != owner
                && waiter.range.overlaps(range)
                && !held.compatible_with(waiter.held)
            {
                blockers.insert(waiter.owner);
            }
        }
        blockers
    }

    /// Whether granting `owner` a wait for `held` over `range` on `file`
    /// would close a cycle.
    fn would_deadlock(&self, file: FileId, owner: OwnerId, range: LockRange, held: Held) -> bool {
        let mut visited = BTreeSet::new();
        let mut frontier = Vec::from_iter(self.blocking_owners(file, owner, range, held));
        while let Some(blocker) = frontier.pop() {
            if blocker == owner {
                return true;
            }
            if !visited.insert(blocker) {
                continue;
            }
            // What that owner is itself waiting for. Registry-wide because
            // an owner may wait on any file; this runs only for a request
            // that is already blocked, and the waiter set is bounded by the
            // tasks currently parked in `fs_lock`.
            for waiter in self.waiters.values().filter(|w| w.owner == blocker) {
                for next in
                    self.blocking_owners(waiter.file, waiter.owner, waiter.range, waiter.held)
                {
                    if next == owner {
                        return true;
                    }
                    if !visited.contains(&next) {
                        frontier.push(next);
                    }
                }
            }
        }
        false
    }
}

/// Discard every lock and waiter — a test fixture's reset, so one case
/// cannot observe another's records through the shared registry.
#[cfg(test)]
pub(crate) fn reset_for_test() {
    let mut reg = REGISTRY.lock();
    reg.files.clear();
    reg.owned.clear();
    reg.charged.clear();
    reg.waiters.clear();
    reg.queued.clear();
    LIVE_RECORDS.store(0, Ordering::Relaxed);
}

/// The ranges `owner` holds on `file`, in order — the shape a test asserts
/// the record algebra against.
#[cfg(test)]
pub(crate) fn held_ranges(file: FileId, owner: OwnerId) -> Vec<(u64, u64, Held)> {
    let reg = REGISTRY.lock();
    reg.files
        .get(&file)
        .and_then(|entry| entry.owners.get(&owner))
        .map(|locks| {
            locks
                .records
                .values()
                .map(|record| {
                    (
                        record.range.start(),
                        record.range.end_inclusive(),
                        record.held,
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Whether the derived indexes still describe the records they project, and
/// the per-owner sets are minimal and non-overlapping — the invariant every
/// operation must preserve.
#[cfg(test)]
pub(crate) fn invariants_hold(file: FileId) -> bool {
    let reg = REGISTRY.lock();
    let Some(entry) = reg.files.get(&file) else {
        return true;
    };
    let mut exclusive: Vec<(u64, u64, OwnerId)> = Vec::new();
    let mut shared = 0usize;
    for (&owner, locks) in &entry.owners {
        if locks.records.is_empty() {
            return false;
        }
        let mut previous: Option<Record> = None;
        for (&key, record) in &locks.records {
            if key != record.range.start() {
                return false;
            }
            if let Some(earlier) = previous {
                // Non-overlapping, and never two same-mode records that
                // merely abut — the set would not be minimal.
                if earlier.range.overlaps(record.range) {
                    return false;
                }
                if earlier.range.abuts(record.range) && earlier.held == record.held {
                    return false;
                }
            }
            previous = Some(*record);
            match record.held {
                Held::Shared => shared += 1,
                Held::Exclusive => {
                    exclusive.push((record.range.start(), record.range.end_inclusive(), owner));
                }
            }
        }
    }
    if shared != entry.shared_records {
        return false;
    }
    let mut projected: Vec<(u64, u64, OwnerId)> = entry
        .exclusive
        .iter()
        .map(|(_, &(range, owner))| (range.start(), range.end_inclusive(), owner))
        .collect();
    projected.sort_unstable();
    exclusive.sort_unstable();
    if projected != exclusive {
        return false;
    }
    // Exclusive records are globally non-overlapping, which is what makes
    // the projection an exact interval index.
    projected.windows(2).all(|pair| pair[0].1 < pair[1].0)
}

/// The system-wide live record count — the figure the fast-path check reads.
#[cfg(test)]
pub(crate) fn live_records() -> u64 {
    LIVE_RECORDS.load(Ordering::Relaxed)
}

#[cfg(test)]
#[path = "filelock_tests.rs"]
mod tests;
