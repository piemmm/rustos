//! The kernel wait-set registry: caller-owned objects that multiplex the
//! readiness of several heterogeneous event sources (`plans/USB.md` — the
//! asynchronous host-controller event loop).
//!
//! A wait-set is the scalable analogue of `epoll`/`kqueue`. A service that
//! must react to many event streams in one process — for the USB host
//! controller, incoming URB-submit IPC calls on the per-interface endpoints it
//! serves *and* its controller's completion interrupt — registers each source
//! once and then blocks on the set, instead of busy-polling (the charter
//! forbids spinning a core) or re-supplying a per-wait array (which would cap
//! the number of sources at a fixed ceiling). Membership lives in a growable
//! [`Vec`], so a busy controller with many interfaces is bounded only by
//! memory, not by a hand-picked constant.
//!
//! # What lives here, and what does not
//!
//! This module is *pure registry mechanics*, like [`crate::callreg`]: it owns
//! the map from a minted handle to a set's `{owner, members}` and enforces
//! that only the set's owner may read or mutate it. It deliberately does **not**
//! resolve a member's underlying resource (an IPC endpoint, an IRQ line) or
//! test its readiness — that needs the call-endpoint registry and the IRQ
//! table, which the syscall handler in [`crate::syscalls`] holds. The handler
//! owner-checks the *resource* before adding a member and re-checks it while
//! scanning for readiness; this registry only owner-checks the *set*.
//!
//! # Why a global
//!
//! Like [`crate::callreg`] and [`crate::waitq`], the registry is global pure
//! data behind a [`SpinLock`] (never a `static mut`, so not the global mutable
//! static the charter forbids): the wait-set is created, modified, and waited
//! on from the syscall handler, and torn down from the task-exit / driver-
//! unload path, none of which own the others. A global keyed by the minted
//! handle is the natural rendezvous and avoids threading another map through
//! `KernelState`'s cross-crate wiring.

use alloc::vec::Vec;

use tairix_abi::{Errno, FileId, WaitSourceKind};
use tairix_sync::SpinLock;

/// One registered source of a wait-set.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Member {
    /// The kind of resource this member observes.
    pub kind: WaitSourceKind,
    /// The resource id: an IPC call-endpoint id for [`WaitSourceKind::Endpoint`],
    /// or an [`tairix_abi::IrqHandle`] raw value for [`WaitSourceKind::Irq`].
    pub id: u64,
    /// The caller's opaque token, reported back by a successful wait when this
    /// member is the one found ready.
    pub token: u64,
    /// For a [`WaitSourceKind::File`] member: the stable identity of the node
    /// the descriptor named at add time, keyed on by the change-notification
    /// registry. [`FileId::NONE`] for every other kind.
    pub file: FileId,
    /// For a [`WaitSourceKind::File`] member: the change generation last
    /// observed. The member is ready when the node's current generation
    /// differs from this; reporting it ready advances this to the current
    /// generation (the edge consume). Unused (`0`) for every other kind.
    pub observed: u64,
}

/// A caller-owned wait-set: the owning task, its growable membership, and
/// the round-robin position that keeps the membership fair.
struct WaitSet {
    /// Task that created the set. Only this task may add/remove members, wait
    /// on it, or have it observed; every entry point owner-checks against it
    /// (no ambient authority).
    owner: u64,
    members: Vec<Member>,
    /// The `(kind, id)` the previous successful wait reported, so the next
    /// scan resumes *after* it.
    ///
    /// A wait reports one member, so without this the scan would always
    /// start at the head and a source that is ready on every scan would
    /// permanently starve every source behind it — a desktop draining a
    /// dragged pointer would never serve the endpoint its applications are
    /// blocked on.
    resume_after: Option<(WaitSourceKind, u64)>,
}

/// The lock-guarded registry state.
struct Inner {
    /// Next handle value; monotonic for the life of the kernel so a retired
    /// handle is never reused (a stale handle then resolves to nothing rather
    /// than to a different task's set).
    next_handle: u64,
    sets: alloc::collections::BTreeMap<u64, WaitSet>,
}

/// The global wait-set registry. Pure data behind a [`SpinLock`]; see the
/// module docs for why it is global.
static WAIT_SETS: SpinLock<Inner> = SpinLock::new(Inner {
    // Start above zero so a caller-zeroed handle slot is never a live set.
    next_handle: 1,
    sets: alloc::collections::BTreeMap::new(),
});

/// Mint a fresh, empty wait-set owned by `owner`, returning its handle.
#[must_use]
pub fn create(owner: u64) -> u64 {
    let mut g = WAIT_SETS.lock();
    let handle = g.next_handle;
    g.next_handle = g.next_handle.wrapping_add(1);
    g.sets.insert(
        handle,
        WaitSet {
            owner,
            members: Vec::new(),
            resume_after: None,
        },
    );
    handle
}

/// Resolve `handle` to a set owned by `owner`, applying the owner check, and
/// run `f` against its mutable [`WaitSet`].
///
/// Returns [`Errno::NotFound`] if the handle names no set or names one owned by
/// another task — a forged or foreign handle resolves to nothing (identify
/// before acting, fail closed).
fn with_owned<R>(owner: u64, handle: u64, f: impl FnOnce(&mut WaitSet) -> R) -> Result<R, Errno> {
    let mut g = WAIT_SETS.lock();
    let Some(set) = g.sets.get_mut(&handle) else {
        return Err(Errno::NotFound);
    };
    if set.owner != owner {
        return Err(Errno::NotFound);
    }
    Ok(f(set))
}

/// Add `member` to the owner's wait-set `handle`.
///
/// The *resource* the member names is resolved and owner-checked by the
/// syscall handler before this is called; here only the set is owner-checked.
///
/// # Errors
///
/// * [`Errno::NotFound`] if `handle` is not a wait-set owned by `owner`.
/// * [`Errno::AlreadyExists`] if a member with the same `(kind, id)` is already
///   registered (an unambiguous duplicate, mirroring `epoll_ctl(ADD)` of an
///   already-registered source — fail closed rather than silently double-add).
pub fn add(owner: u64, handle: u64, member: Member) -> Result<(), Errno> {
    with_owned(owner, handle, |set| {
        if set
            .members
            .iter()
            .any(|m| m.kind == member.kind && m.id == member.id)
        {
            return Err(Errno::AlreadyExists);
        }
        set.members.push(member);
        Ok(())
    })?
}

/// Remove the member with `(kind, id)` from the owner's wait-set `handle`.
///
/// # Errors
///
/// * [`Errno::NotFound`] if `handle` is not a wait-set owned by `owner`, or if
///   no member with that `(kind, id)` is registered.
pub fn remove(owner: u64, handle: u64, kind: WaitSourceKind, id: u64) -> Result<(), Errno> {
    // Collect the removed File member's identity under the set lock, then
    // drop its file-change watch after releasing the lock (so the fswatch
    // registry lock is never taken nested under this one).
    let removed_file = with_owned(owner, handle, |set| {
        let before = set.members.len();
        let mut file = None;
        set.members.retain(|m| {
            let matches = m.kind == kind && m.id == id;
            if matches && m.kind == WaitSourceKind::File {
                file = Some(m.file);
            }
            !matches
        });
        if set.members.len() == before {
            return Err(Errno::NotFound);
        }
        Ok(file)
    })??;
    if let Some(file) = removed_file {
        crate::fswatch::watch_remove(file);
    }
    Ok(())
}

/// Record the change generation a [`WaitSourceKind::File`] member last
/// observed (the edge consume performed when the member is reported ready).
/// A no-op if the member is absent — the owner may have removed it between
/// the readiness scan and this update.
///
/// # Errors
///
/// [`Errno::NotFound`] if `handle` is not a wait-set owned by `owner`.
pub fn advance_observed(
    owner: u64,
    handle: u64,
    kind: WaitSourceKind,
    id: u64,
    observed: u64,
) -> Result<(), Errno> {
    with_owned(owner, handle, |set| {
        if let Some(member) = set
            .members
            .iter_mut()
            .find(|m| m.kind == kind && m.id == id)
        {
            member.observed = observed;
        }
    })
}

/// Snapshot the members of the owner's wait-set `handle`, in the order the
/// next wait must scan them.
///
/// Returns an owned copy so the syscall handler can scan readiness (which
/// reaches into the call-endpoint registry and IRQ table) without holding this
/// registry's lock. Membership can only be mutated by the owning task, which
/// is parked inside its own `waitset_wait` while this snapshot is in use, so a
/// single snapshot per wait is stable.
///
/// # Ordering discipline
///
/// The snapshot is rotated to begin just after the member
/// [`note_reported`] last recorded, so successive waits hand the ready
/// members out round-robin rather than always awarding the first-registered
/// one. That is what bounds the wait to a fair share per source: the scan
/// itself still takes the first ready member it meets, but *which* member it
/// meets first advances every time one is reported. Registration order is
/// preserved within a rotation, and a set whose last-reported member has
/// since been removed falls back to registration order.
///
/// # Errors
///
/// [`Errno::NotFound`] if `handle` is not a wait-set owned by `owner`.
pub fn members(owner: u64, handle: u64) -> Result<Vec<Member>, Errno> {
    with_owned(owner, handle, |set| {
        let resume = set
            .resume_after
            .and_then(|(kind, id)| {
                set.members
                    .iter()
                    .position(|m| m.kind == kind && m.id == id)
            })
            .map_or(0, |at| at + 1);
        let mut scan = Vec::with_capacity(set.members.len());
        scan.extend_from_slice(&set.members[resume..]);
        scan.extend_from_slice(&set.members[..resume]);
        scan
    })
}

/// Record `(kind, id)` as the member the wait just reported, so the next
/// [`members`] snapshot resumes after it.
///
/// # Errors
///
/// [`Errno::NotFound`] if `handle` is not a wait-set owned by `owner`.
pub fn note_reported(owner: u64, handle: u64, kind: WaitSourceKind, id: u64) -> Result<(), Errno> {
    with_owned(owner, handle, |set| {
        set.resume_after = Some((kind, id));
    })
}

/// Tear down every wait-set owned by the exiting task `owner`, returning how
/// many were removed.
///
/// A wait-set holds no resource of its own (its members only *name* endpoints
/// and IRQ lines the task owns, which are reclaimed by their own teardown), so
/// dropping the sets is the whole reclamation. Idempotent.
pub fn release_owned_by(owner: u64) -> usize {
    // Collect the File members' identities while dropping the owner's sets,
    // then release each one's file-change watch after the lock is dropped so
    // the watch_add/watch_remove pairing survives task teardown.
    let (removed, files) = {
        let mut g = WAIT_SETS.lock();
        let before = g.sets.len();
        let mut files: Vec<FileId> = Vec::new();
        g.sets.retain(|_, set| {
            if set.owner == owner {
                for member in &set.members {
                    if member.kind == WaitSourceKind::File {
                        files.push(member.file);
                    }
                }
                false
            } else {
                true
            }
        });
        (before - g.sets.len(), files)
    };
    for file in files {
        crate::fswatch::watch_remove(file);
    }
    removed
}

/// `true` if `handle` is a live wait-set owned by `owner`. Diagnostic / test
/// observer.
#[must_use]
pub fn owned_by(owner: u64, handle: u64) -> bool {
    WAIT_SETS
        .lock()
        .sets
        .get(&handle)
        .is_some_and(|set| set.owner == owner)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ep(id: u64, token: u64) -> Member {
        Member {
            kind: WaitSourceKind::Endpoint,
            id,
            token,
            file: FileId::NONE,
            observed: 0,
        }
    }

    #[test]
    fn create_then_owner_checked_access() {
        let h = create(0x1001);
        assert!(owned_by(0x1001, h));
        // A different task cannot see or mutate the set.
        assert!(!owned_by(0x2002, h));
        assert_eq!(members(0x2002, h), Err(Errno::NotFound));
        assert_eq!(
            add(0x2002, h, ep(7, 70)),
            Err(Errno::NotFound),
            "foreign add is refused"
        );
        // Clean up so the global registry does not leak across tests.
        assert_eq!(release_owned_by(0x1001), 1);
    }

    #[test]
    fn add_remove_and_duplicate_is_refused() {
        let h = create(0x3003);
        assert_eq!(add(0x3003, h, ep(7, 70)), Ok(()));
        assert_eq!(add(0x3003, h, ep(8, 80)), Ok(()));
        // Same (kind,id) is a duplicate, even with a different token.
        assert_eq!(add(0x3003, h, ep(7, 99)), Err(Errno::AlreadyExists));
        assert_eq!(members(0x3003, h).map(|m| m.len()), Ok(2));
        assert_eq!(remove(0x3003, h, WaitSourceKind::Endpoint, 7), Ok(()));
        assert_eq!(members(0x3003, h).map(|m| m.len()), Ok(1));
        // Removing an absent member fails closed.
        assert_eq!(
            remove(0x3003, h, WaitSourceKind::Endpoint, 7),
            Err(Errno::NotFound)
        );
        assert_eq!(release_owned_by(0x3003), 1);
    }

    #[test]
    fn an_endpoint_and_an_irq_of_the_same_id_are_distinct_members() {
        let h = create(0x4004);
        assert_eq!(add(0x4004, h, ep(5, 50)), Ok(()));
        assert_eq!(
            add(
                0x4004,
                h,
                Member {
                    kind: WaitSourceKind::Irq,
                    id: 5,
                    token: 55,
                    file: FileId::NONE,
                    observed: 0,
                }
            ),
            Ok(()),
            "same id, different kind is not a duplicate"
        );
        assert_eq!(members(0x4004, h).map(|m| m.len()), Ok(2));
        assert_eq!(release_owned_by(0x4004), 1);
    }

    #[test]
    fn release_owned_by_drops_only_the_owner_and_is_idempotent() {
        let a = create(0x5005);
        let b = create(0x6006);
        assert_eq!(release_owned_by(0x5005), 1);
        assert!(!owned_by(0x5005, a));
        assert!(owned_by(0x6006, b));
        assert_eq!(release_owned_by(0x5005), 0, "idempotent");
        assert_eq!(release_owned_by(0x6006), 1);
    }

    #[test]
    fn handles_are_unique_across_creates() {
        let a = create(0x7007);
        let b = create(0x7007);
        assert_ne!(a, b, "each create mints a fresh handle");
        assert_eq!(release_owned_by(0x7007), 2);
    }

    fn ids(owner: u64, handle: u64) -> Vec<u64> {
        members(owner, handle)
            .expect("owned set")
            .iter()
            .map(|m| m.id)
            .collect()
    }

    #[test]
    fn a_fresh_set_scans_in_registration_order() {
        let h = create(0x8008);
        for id in 1..=3 {
            assert_eq!(add(0x8008, h, ep(id, id * 10)), Ok(()));
        }
        assert_eq!(ids(0x8008, h), alloc::vec![1, 2, 3]);
        assert_eq!(release_owned_by(0x8008), 1);
    }

    /// The regression this cursor exists for: the scan takes the first ready
    /// member, so a source ready on every scan would hold the head forever
    /// and never let the members behind it be reported. Each report moves the
    /// start on, so every member reaches the head within one lap.
    #[test]
    fn reporting_a_member_moves_the_scan_past_it() {
        let h = create(0x9009);
        for id in 1..=3 {
            assert_eq!(add(0x9009, h, ep(id, id * 10)), Ok(()));
        }
        assert_eq!(
            note_reported(0x9009, h, WaitSourceKind::Endpoint, 1),
            Ok(())
        );
        assert_eq!(ids(0x9009, h), alloc::vec![2, 3, 1]);
        assert_eq!(
            note_reported(0x9009, h, WaitSourceKind::Endpoint, 2),
            Ok(())
        );
        assert_eq!(ids(0x9009, h), alloc::vec![3, 1, 2]);
        // A lap brings the first member back to the head, so the rotation
        // cycles rather than walking off the end.
        assert_eq!(
            note_reported(0x9009, h, WaitSourceKind::Endpoint, 3),
            Ok(())
        );
        assert_eq!(ids(0x9009, h), alloc::vec![1, 2, 3]);
        assert_eq!(release_owned_by(0x9009), 1);
    }

    #[test]
    fn the_rotation_is_per_kind_as_well_as_per_id() {
        let h = create(0xa00a);
        assert_eq!(add(0xa00a, h, ep(5, 50)), Ok(()));
        assert_eq!(
            add(
                0xa00a,
                h,
                Member {
                    kind: WaitSourceKind::Irq,
                    id: 5,
                    token: 55,
                    file: FileId::NONE,
                    observed: 0,
                }
            ),
            Ok(())
        );
        // Reporting the endpoint must not move past the same-id IRQ member.
        assert_eq!(
            note_reported(0xa00a, h, WaitSourceKind::Endpoint, 5),
            Ok(())
        );
        assert_eq!(
            members(0xa00a, h).expect("owned set")[0].kind,
            WaitSourceKind::Irq
        );
        assert_eq!(release_owned_by(0xa00a), 1);
    }

    #[test]
    fn removing_the_last_reported_member_falls_back_to_registration_order() {
        let h = create(0xb00b);
        for id in 1..=3 {
            assert_eq!(add(0xb00b, h, ep(id, id * 10)), Ok(()));
        }
        assert_eq!(
            note_reported(0xb00b, h, WaitSourceKind::Endpoint, 2),
            Ok(())
        );
        assert_eq!(ids(0xb00b, h), alloc::vec![3, 1, 2]);
        assert_eq!(remove(0xb00b, h, WaitSourceKind::Endpoint, 2), Ok(()));
        assert_eq!(ids(0xb00b, h), alloc::vec![1, 3]);
        assert_eq!(release_owned_by(0xb00b), 1);
    }

    #[test]
    fn an_empty_set_rotates_to_nothing() {
        let h = create(0xc00c);
        assert_eq!(ids(0xc00c, h), Vec::<u64>::new());
        assert_eq!(
            note_reported(0xc00c, h, WaitSourceKind::Endpoint, 1),
            Ok(()),
            "recording a member the set no longer holds is not an error"
        );
        assert_eq!(ids(0xc00c, h), Vec::<u64>::new());
        assert_eq!(release_owned_by(0xc00c), 1);
    }

    #[test]
    fn note_reported_is_owner_checked() {
        let h = create(0xd00d);
        assert_eq!(
            note_reported(0xe00e, h, WaitSourceKind::Endpoint, 1),
            Err(Errno::NotFound)
        );
        assert_eq!(release_owned_by(0xd00d), 1);
    }
}
