//! Per-file change notification: the kernel side of the
//! [`WaitSourceKind::File`](tairix_abi::WaitSourceKind::File) wait source.
//!
//! A file watcher (`tail -f`/`-F`) must block off-CPU until the file it
//! follows changes, never busy-poll a core. This module is the rendezvous
//! that makes that possible without waking every watcher on every write: it
//! keeps, per watched node identity ([`FileId`]), a monotonic **change
//! generation** and the set of tasks currently parked observing it. A
//! filesystem mutation bumps exactly the affected node's generation and
//! unparks exactly that node's waiters — a write to an unwatched file, or to
//! a *different* watched file, disturbs no one.
//!
//! # Why keyed by [`FileId`], not a global counter
//!
//! A single global "the filesystem changed" generation (the shape
//! [`crate::hwtree`] uses for the rarely-changing hardware tree) would wake
//! every `tail -f` on the machine on every write anywhere — a thundering
//! herd on a busy multi-user server. Files change far too often for that.
//! Keying on the node's stable identity (its mount's volume id plus the
//! driver's node number) makes a write's wake-up cost proportional to the
//! number of watchers *of that node*, which is what a server under load
//! needs.
//!
//! # The change generation is edge state, the observed generation lives with
//! the member
//!
//! This module owns only the *current* generation of each watched node.
//! Each wait-set member stores the generation it last observed; the member
//! is "ready" when the node's current generation differs from what the
//! member observed, and reporting it ready advances the member's observed
//! value (the edge consume). A node that changes twice before its watcher
//! runs still fires once and, because the watcher always reads to
//! end-of-file, loses no data; a node that never changes never fires.
//!
//! # The write-path fast path
//!
//! Resolving a mutated path to its [`FileId`] is extra work the common case
//! (no watchers running) must not pay. [`watchers_present`] is a single
//! relaxed atomic load the mutation choke points check first: when no File
//! member exists anywhere, the notify path is skipped entirely and the
//! write hot path is untouched.
//!
//! # Locking and wake context
//!
//! The registry is pure data behind a [`SpinLock`] (never a `static mut`).
//! Like the sibling wait sources ([`crate::waitq`]), [`note_change`]
//! collects the waiter task ids under the lock, releases it, and only then
//! `unpark`s them through the boot-installed [`crate::waitq::wait_arch`]
//! adapter — so the scheduler's locks are never taken while the registry
//! lock is held. Mutations run in task (syscall) context, never an
//! interrupt handler, so the unpark is direct; the register-before-park
//! discipline in `waitset_wait` plus the scheduler's wake-pending token
//! close the check-then-park race, exactly as the other wait sources rely
//! on.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};

use tairix_abi::FileId;
use tairix_kernel_sched_api::TaskId;
use tairix_sync::SpinLock;

/// One watched node: its current change generation, how many wait-set
/// members are watching it, and the tasks currently parked observing it.
struct Entry {
    /// Monotonic change counter, bumped by [`note_change`]. A member is
    /// ready when this differs from the generation it last observed.
    generation: u64,
    /// Number of live wait-set members watching this node. The entry is
    /// dropped once no member references it and no task is parked on it, so
    /// the registry holds only nodes something is actively watching.
    members: usize,
    /// Tasks currently parked inside `waitset_wait` observing this node,
    /// for the targeted unpark. FIFO; a task appears at most once.
    waiters: Vec<TaskId>,
}

/// The lock-guarded registry: watched nodes keyed by their stable
/// [`FileId`].
static REGISTRY: SpinLock<BTreeMap<FileId, Entry>> = SpinLock::new(BTreeMap::new());

/// Number of live File wait-set members across the whole system. The
/// mutation choke points load this (relaxed) before doing any notify work,
/// so the write hot path pays nothing when nobody is watching.
static MEMBER_COUNT: AtomicUsize = AtomicUsize::new(0);

/// Whether any File wait-set member currently exists.
///
/// A single relaxed atomic load: the mutation choke points check it first
/// and skip the [`FileId`] resolution and [`note_change`] entirely when it
/// is `false` (the common case — no watcher running).
#[must_use]
pub fn watchers_present() -> bool {
    MEMBER_COUNT.load(Ordering::Relaxed) > 0
}

/// The node's current change generation, or `0` if it is not watched.
///
/// `0` is also the baseline a freshly watched node starts at, so a member
/// added before any change observes `0` and the first change (generation
/// `1`) makes it ready.
#[must_use]
pub fn current_generation(id: FileId) -> u64 {
    REGISTRY.lock().get(&id).map_or(0, |e| e.generation)
}

/// Register a new wait-set member watching `id`, returning the node's
/// current change generation for the member to record as its baseline.
///
/// Creates the registry entry if this is the node's first member. Paired
/// with [`watch_remove`] over the member's lifetime (wait-set `ADD`/`DEL`
/// and set teardown).
#[must_use]
pub fn watch_add(id: FileId) -> u64 {
    MEMBER_COUNT.fetch_add(1, Ordering::Relaxed);
    let mut registry = REGISTRY.lock();
    let entry = registry.entry(id).or_insert(Entry {
        generation: 0,
        members: 0,
        waiters: Vec::new(),
    });
    entry.members += 1;
    entry.generation
}

/// Drop a wait-set member watching `id`. Removes the entry once no member
/// references it and no task is parked on it. Idempotent against an unknown
/// id (a fail-safe no-op).
pub fn watch_remove(id: FileId) {
    // Every `watch_remove` is paired with a prior `watch_add`, so the count
    // never underflows; `fetch_sub` keeps the decrement atomic under SMP.
    MEMBER_COUNT.fetch_sub(1, Ordering::Relaxed);
    let mut registry = REGISTRY.lock();
    if let Some(entry) = registry.get_mut(&id) {
        entry.members = entry.members.saturating_sub(1);
        if entry.members == 0 && entry.waiters.is_empty() {
            registry.remove(&id);
        }
    }
}

/// Register `task` as parked observing `id` (called by `waitset_wait`
/// before it parks, so a change in the register/park window is not lost).
/// Idempotent for a task already registered on this node.
pub fn park_add(id: FileId, task: TaskId) {
    let mut registry = REGISTRY.lock();
    let entry = registry.entry(id).or_insert(Entry {
        generation: 0,
        members: 0,
        waiters: Vec::new(),
    });
    if !entry.waiters.contains(&task) {
        entry.waiters.push(task);
    }
}

/// Deregister `task` from the waiters of `id` (called when `waitset_wait`
/// stops observing it). Removes the entry once nothing references it.
/// Idempotent.
pub fn park_remove(id: FileId, task: TaskId) {
    let mut registry = REGISTRY.lock();
    if let Some(entry) = registry.get_mut(&id) {
        entry.waiters.retain(|&t| t != task);
        if entry.members == 0 && entry.waiters.is_empty() {
            registry.remove(&id);
        }
    }
}

/// Record that the node `id` changed: bump its change generation and unpark
/// every task parked observing it.
///
/// A no-op for a node no member watches (no entry), so an unwatched write
/// costs only the map lookup — and the mutation choke points skip even that
/// via [`watchers_present`]. Each unparked waiter re-scans its wait-set,
/// finds this member's generation advanced past what it observed, and
/// returns; a wake for a node whose generation a given waiter has already
/// caught up on is a harmless spurious wake it re-parks through.
pub fn note_change(id: FileId) {
    // Collect the waiter ids under the lock, then unpark after releasing it
    // so the scheduler's locks are never taken while the registry lock is
    // held (the sibling wait-source discipline).
    let waiters: Vec<TaskId> = {
        let mut registry = REGISTRY.lock();
        let Some(entry) = registry.get_mut(&id) else {
            return;
        };
        entry.generation = entry.generation.wrapping_add(1);
        entry.waiters.clone()
    };
    if waiters.is_empty() {
        return;
    }
    if let Some(arch) = crate::waitq::wait_arch() {
        for task in waiters {
            arch.unpark(task);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, PoisonError};

    fn id(node: u64) -> FileId {
        FileId {
            volume: [7u8; 16],
            node,
        }
    }

    // The registry and `MEMBER_COUNT` are process-global. Per-node isolation
    // keeps generations from colliding, but a test that asserts on the shared
    // system-wide member total would still race another test adding or
    // removing a member concurrently, so the registry tests run one at a time
    // under this guard (recovering a poisoned lock so one failure does not
    // wedge the rest).
    static SERIAL: Mutex<()> = Mutex::new(());

    fn serial() -> std::sync::MutexGuard<'static, ()> {
        SERIAL.lock().unwrap_or_else(PoisonError::into_inner)
    }

    #[test]
    fn watch_add_baselines_at_zero_and_change_advances_it() {
        let _serial = serial();
        let f = id(0x1001);
        assert_eq!(current_generation(f), 0, "unwatched reads zero");
        let baseline = watch_add(f);
        assert_eq!(baseline, 0, "first member baselines at zero");
        note_change(f);
        assert_eq!(current_generation(f), 1, "a change advances the generation");
        note_change(f);
        assert_eq!(current_generation(f), 2);
        watch_remove(f);
        assert_eq!(
            current_generation(f),
            0,
            "the entry is dropped with no members"
        );
    }

    #[test]
    fn a_second_member_baselines_at_the_current_generation() {
        let _serial = serial();
        let f = id(0x1002);
        let _a = watch_add(f);
        note_change(f);
        let b = watch_add(f);
        assert_eq!(b, 1, "a later member starts from the current generation");
        watch_remove(f);
        watch_remove(f);
    }

    #[test]
    fn note_change_on_an_unwatched_node_is_a_no_op() {
        let _serial = serial();
        let f = id(0x1003);
        note_change(f);
        assert_eq!(
            current_generation(f),
            0,
            "no entry is created for an unwatched node"
        );
    }

    #[test]
    fn watchers_present_tracks_member_count() {
        let _serial = serial();
        let f = id(0x1004);
        let g = id(0x1005);
        let before = MEMBER_COUNT.load(Ordering::Relaxed);
        let _ = watch_add(f);
        let _ = watch_add(g);
        assert!(watchers_present());
        watch_remove(f);
        watch_remove(g);
        assert_eq!(
            MEMBER_COUNT.load(Ordering::Relaxed),
            before,
            "member count returns to its starting value"
        );
    }

    #[test]
    fn park_registration_keeps_the_entry_alive_without_a_member() {
        let _serial = serial();
        let f = id(0x1006);
        park_add(f, 42);
        note_change(f);
        assert_eq!(
            current_generation(f),
            1,
            "a parked waiter alone tracks the generation"
        );
        park_remove(f, 42);
        assert_eq!(
            current_generation(f),
            0,
            "the entry drops once nothing references it"
        );
    }
}
