//! The peer-exit watch: how a service learns that a process it holds state
//! for has gone (`plans/ZEROCONF.md` Z4).
//!
//! A service keeping state on a client's behalf — a socket, a browse session,
//! a connection it counts — has no event when that client dies. It is not the
//! client's parent, and nothing the client left behind rings. This registry is
//! that event: a thread watches a process *instance* by the attested
//! [`ProcId`] it read from the client's `Origin`, parks on one
//! [`WaitSourceKind::PeerExit`](tairix_abi::WaitSourceKind::PeerExit) member
//! for every watch it holds, and takes each exit as it lands.
//!
//! # Why no exit is lost
//!
//! A watch is taken only on an instance the capability table still holds,
//! checked with the table's read lock held across the registration; teardown
//! removes the record under the write lock and fires only after. So a watch is
//! either registered before the removal, and fired, or finds the instance gone
//! and is refused — and the watcher learns the peer is dead either way.
//!
//! # Bounds
//!
//! A watch names a live process and is dropped when it fires, so a thread
//! holds at most one per live process. Each watch reserves the slot its exit
//! will occupy when it is taken, so firing never allocates and never fails.
//! Watches and untaken exits die with the watching thread.

use alloc::collections::VecDeque;
use alloc::vec::Vec;

use tairix_abi::{Errno, ProcId};
use tairix_collections::{HashMap, HashSet};
use tairix_hash::BuildFastHash;
use tairix_kernel_sched_api::TaskId;
use tairix_kernel_sec::{CapTable, ProcessId, TaskCapabilities};
use tairix_sync::{RwLock, SpinLock};

use crate::waitq::{wait_arch, WaitQueue, NO_DEADLINE};

/// One watching thread's watches and the exits it has not taken.
struct Watcher {
    watching: HashSet<ProcId, BuildFastHash>,
    /// Holds room for one exit per watch, so a firing never allocates.
    exits: VecDeque<ProcId>,
}

impl Watcher {
    const fn new() -> Self {
        Self {
            watching: HashSet::with_hasher(BuildFastHash::new()),
            exits: VecDeque::new(),
        }
    }
}

struct Registry {
    /// The threads watching each live instance.
    peers: HashMap<ProcId, Vec<TaskId>, BuildFastHash>,
    watchers: HashMap<TaskId, Watcher, BuildFastHash>,
}

/// Every thread's watches, and the queue a thread parks on while its wait-set
/// holds a `PeerExit` member — joined by no other waiter, so an exit wakes
/// only the thread it is for.
///
/// Owned by the kernel state; the syscall handlers and every teardown path
/// reach the same one by reference.
pub struct PeerWatch {
    registry: SpinLock<Registry>,
    parked: WaitQueue,
}

impl Default for PeerWatch {
    fn default() -> Self {
        Self::new()
    }
}

impl PeerWatch {
    /// A registry holding no watches.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            registry: SpinLock::new(Registry {
                peers: HashMap::with_hasher(BuildFastHash::new()),
                watchers: HashMap::with_hasher(BuildFastHash::new()),
            }),
            parked: WaitQueue::new(),
        }
    }

    /// Watch `peer` on `watcher`'s behalf. Idempotent.
    ///
    /// # Errors
    ///
    /// [`Errno::NotFound`] for an instance no live record names — the kernel
    /// sentinel, one never admitted, or one already gone, which the caller
    /// then treats as exited. [`Errno::OutOfMemory`] when the registry cannot
    /// grow; the registry is left as it was.
    pub fn watch(
        &self,
        caps: &RwLock<CapTable>,
        watcher: TaskId,
        peer: ProcId,
    ) -> Result<(), Errno> {
        // Held across the registration, so the peer's teardown cannot remove
        // its record between the check and the watch.
        let table = caps.read();
        if table.process_of_instance(peer).is_none() {
            return Err(Errno::NotFound);
        }
        let mut guard = self.registry.lock();
        let registry = &mut *guard;
        if registry.watchers.get(&watcher).is_none() {
            registry
                .watchers
                .try_insert(watcher, Watcher::new())
                .map_err(|_| Errno::OutOfMemory)?;
        }
        let record = registry
            .watchers
            .get_mut(&watcher)
            .ok_or(Errno::OutOfMemory)?;
        if record.watching.contains(&peer) {
            return Ok(());
        }
        // Every allocation is made before anything is recorded, so a refusal
        // leaves the registry exactly as it was.
        record
            .exits
            .try_reserve(record.watching.len() + 1)
            .map_err(|_| Errno::OutOfMemory)?;
        let fresh_list = registry.peers.get(&peer).is_none();
        if fresh_list {
            registry
                .peers
                .try_insert(peer, Vec::new())
                .map_err(|_| Errno::OutOfMemory)?;
        }
        let listed = registry
            .peers
            .get_mut(&peer)
            .is_some_and(|list| list.try_reserve(1).is_ok());
        if !listed || record.watching.try_insert(peer).is_err() {
            if fresh_list {
                registry.peers.remove(&peer);
            }
            return Err(Errno::OutOfMemory);
        }
        if let Some(list) = registry.peers.get_mut(&peer) {
            list.push(watcher);
        }
        drop(guard);
        drop(table);
        Ok(())
    }

    /// Stop watching `peer`.
    ///
    /// # Errors
    ///
    /// [`Errno::NotFound`] when `watcher` was not watching it — including a
    /// watch that has already fired, whose exit is waiting to be taken.
    pub fn unwatch(&self, watcher: TaskId, peer: ProcId) -> Result<(), Errno> {
        let mut guard = self.registry.lock();
        let registry = &mut *guard;
        let removed = registry
            .watchers
            .get_mut(&watcher)
            .is_some_and(|record| record.watching.remove(&peer));
        if !removed {
            return Err(Errno::NotFound);
        }
        drop_from_peer(&mut registry.peers, peer, watcher);
        Ok(())
    }

    /// The oldest exit `watcher` has not taken, left in place.
    ///
    /// # Errors
    ///
    /// [`Errno::WouldBlock`] when none is waiting.
    pub fn oldest(&self, watcher: TaskId) -> Result<ProcId, Errno> {
        self.registry
            .lock()
            .watchers
            .get(&watcher)
            .and_then(|record| record.exits.front().copied())
            .ok_or(Errno::WouldBlock)
    }

    /// Consume the oldest exit once the caller has delivered it. Only the
    /// watching thread takes from its own feed, so what [`Self::oldest`]
    /// returned is still at the front.
    pub fn consume(&self, watcher: TaskId, exited: ProcId) {
        if let Some(record) = self.registry.lock().watchers.get_mut(&watcher) {
            if record.exits.front() == Some(&exited) {
                record.exits.pop_front();
            }
        }
    }

    /// Whether an exit is waiting for `watcher` — the wait-set's peek, never a
    /// take.
    #[must_use]
    pub fn ready(&self, watcher: TaskId) -> bool {
        self.registry
            .lock()
            .watchers
            .get(&watcher)
            .is_some_and(|record| !record.exits.is_empty())
    }

    /// Park `watcher` for its next exit, alongside its wait-set's other
    /// sources. Idempotent.
    pub fn join(&self, watcher: TaskId) {
        self.parked.register(watcher, NO_DEADLINE);
    }

    /// Stop parking `watcher` for an exit.
    pub fn leave(&self, watcher: TaskId) {
        self.parked.deregister(watcher);
    }

    /// Queue `peer`'s exit for every thread watching it and wake each.
    fn on_exit(&self, peer: ProcId) {
        let watchers = {
            let mut guard = self.registry.lock();
            let registry = &mut *guard;
            let Some(watchers) = registry.peers.remove(&peer) else {
                return;
            };
            for task in &watchers {
                if let Some(record) = registry.watchers.get_mut(task) {
                    if record.watching.remove(&peer) {
                        // Room was reserved when the watch was taken.
                        record.exits.push_back(peer);
                    }
                }
            }
            watchers
        };
        // A thread not parked finds the exit on its next wait.
        if let Some(arch) = wait_arch() {
            for task in watchers {
                let _ = self.parked.wake_task(arch, task);
            }
        }
    }

    /// Drop everything `watcher` holds: its watches, its untaken exits, and
    /// its place in the queue. Driven by the thread's teardown, so a later
    /// thread drawing its id inherits nothing.
    pub fn forget_watcher(&self, watcher: TaskId) {
        self.parked.deregister_task(watcher);
        let mut guard = self.registry.lock();
        let registry = &mut *guard;
        let Some(record) = registry.watchers.remove(&watcher) else {
            return;
        };
        for &peer in &record.watching {
            drop_from_peer(&mut registry.peers, peer, watcher);
        }
    }
}

/// Remove `process`'s capability record and fire every watch `peers` holds on
/// the instance it carried, returning the record. The one path a record leaves
/// the table by, so no teardown can forget to fire; a kernel that wired no
/// registry took no watch to fire.
pub fn remove_record(
    peers: Option<&PeerWatch>,
    caps: &RwLock<CapTable>,
    process: ProcessId,
) -> Option<TaskCapabilities> {
    // The write guard is a temporary, released before any watch fires.
    let removed = caps.write().remove(process);
    if let (Some(peers), Some(record)) = (peers, &removed) {
        peers.on_exit(record.proc_id());
    }
    removed
}

fn drop_from_peer(
    peers: &mut HashMap<ProcId, Vec<TaskId>, BuildFastHash>,
    peer: ProcId,
    watcher: TaskId,
) {
    let now_empty = peers.get_mut(&peer).is_some_and(|list| {
        if let Some(index) = list.iter().position(|task| *task == watcher) {
            list.swap_remove(index);
        }
        list.is_empty()
    });
    if now_empty {
        peers.remove(&peer);
    }
}

#[cfg(test)]
#[path = "peerwatch_tests.rs"]
mod tests;
