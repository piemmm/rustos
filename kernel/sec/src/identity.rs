//! User and group identity state held by the kernel.
//!
//! The on-disk identity records persist in `/etc/rustos/users` and
//! `/etc/rustos/groups` (see `AGENTS.md` §5.1). Loading them is a
//! userland responsibility; this module provides the **in-memory builder
//! and verifier** the kernel uses to ingest already-loaded records and
//! turn them into a frozen [`IdentityTable`] it can consult on every
//! privileged operation.
//!
//! # Invariants enforced by the verifier
//!
//! The builder fails (returns [`rustos_abi::Errno`] and emits a single
//! [`AuditEvent::IdentityTableRejected`])
//! when:
//!
//! * Two user records share the same [`UserId`].
//! * Two group records share the same [`GroupId`].
//! * A user record's primary or supplementary group reference does not
//!   resolve to a known [`GroupRecord`].
//! * A user record's supplementary-group set exceeds
//!   [`MAX_SUPPLEMENTARY_GROUPS`] entries (a hostile or corrupted record
//!   must never force unbounded kernel allocation).
//!
//! # No ambient authority
//!
//! Per `AGENTS.md` §5.1, `uid == 0` is **not** privileged in this crate.
//! [`IdentityTable::user`] returns the requested record verbatim; powers
//! flow exclusively from the capability set attached to that record and
//! intersected with the binary's manifest request in
//! [`crate::TaskCapabilities`].

extern crate alloc;

use alloc::vec::Vec;

use rustos_abi::Errno;
use rustos_caps::CapabilitySet;
use rustos_log::{Field, Sink};

use crate::audit::{record, AuditEvent};

/// Maximum number of supplementary groups a single user record may carry.
///
/// Bounded so the table is `O(users × MAX_SUPPLEMENTARY_GROUPS)` and
/// never grows in response to a hostile on-disk record. Matches the
/// hard ceiling enforced by POSIX `NGROUPS_MAX` on long-standing Unix
/// kernels; if a deployment outgrows it, raising the bound is a
/// reviewed change here, not a per-call workaround.
pub const MAX_SUPPLEMENTARY_GROUPS: usize = 32;

/// Numeric user identifier.
///
/// `uid == 0` carries **no** special powers in `kernel/sec`; see the
/// module docs and `AGENTS.md` §5.1.
#[repr(transparent)]
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct UserId(pub u32);

/// Numeric group identifier.
#[repr(transparent)]
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct GroupId(pub u32);

/// In-memory user record.
///
/// Mirrors the on-disk `/etc/rustos/users` schema with the fields
/// `kernel/sec` actually consults: every privileged operation needs
/// `(uid, primary group, supplementary groups, capability grants)` and
/// nothing else. The textual fields (display name, home directory) live
/// purely in userland.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UserRecord {
    /// Numeric user id.
    pub uid: UserId,
    /// Primary group this user is a member of.
    pub primary_gid: GroupId,
    /// Supplementary groups the user is a member of. Bounded length;
    /// see [`MAX_SUPPLEMENTARY_GROUPS`].
    pub supplementary_gids: Vec<GroupId>,
    /// The maximum capability set this user may ever exercise. Per
    /// `AGENTS.md` §5.2, a task's effective set is the intersection of
    /// this grant with the binary's manifest request.
    pub capability_grants: CapabilitySet,
}

/// In-memory group record.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GroupRecord {
    /// Numeric group id.
    pub gid: GroupId,
}

/// Frozen identity table consulted by `kernel/sec` on every privileged
/// operation.
///
/// Build one with [`IdentityTableBuilder`]; the verifier turns a list of
/// candidate user/group records into an immutable view. The table is
/// `Send + Sync` and the kernel uses it through a shared reference; it
/// never mutates after construction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IdentityTable {
    users: Vec<UserRecord>,
    groups: Vec<GroupRecord>,
}

impl IdentityTable {
    /// Look up a user record by id.
    ///
    /// Returns [`Errno::NotFound`] if no record matches. The result is a
    /// borrow into the table so callers cannot accidentally mutate it.
    pub fn user(&self, uid: UserId) -> Result<&UserRecord, Errno> {
        self.users
            .iter()
            .find(|u| u.uid == uid)
            .ok_or(Errno::NotFound)
    }

    /// Look up a group record by id.
    ///
    /// Returns [`Errno::NotFound`] if no record matches.
    pub fn group(&self, gid: GroupId) -> Result<&GroupRecord, Errno> {
        self.groups
            .iter()
            .find(|g| g.gid == gid)
            .ok_or(Errno::NotFound)
    }

    /// `true` if `uid` is a member of `gid` either through its primary
    /// group or through its supplementary set. Returns `false` if the
    /// user is unknown — group membership is never granted to a missing
    /// principal.
    #[must_use]
    pub fn is_member_of(&self, uid: UserId, gid: GroupId) -> bool {
        let Ok(user) = self.user(uid) else {
            return false;
        };
        user.primary_gid == gid || user.supplementary_gids.contains(&gid)
    }

    /// Number of users in the table.
    #[must_use]
    pub fn user_count(&self) -> usize {
        self.users.len()
    }

    /// Number of groups in the table.
    #[must_use]
    pub fn group_count(&self) -> usize {
        self.groups.len()
    }
}

/// Builder for an [`IdentityTable`].
///
/// The builder owns the candidate records until [`Self::verify`] succeeds.
/// Verification is the single point where every invariant from the
/// module docs is enforced; once it returns `Ok`, no further mutation is
/// possible.
#[derive(Default, Debug)]
pub struct IdentityTableBuilder {
    users: Vec<UserRecord>,
    groups: Vec<GroupRecord>,
}

impl IdentityTableBuilder {
    /// Create an empty builder.
    #[must_use]
    pub fn new() -> Self {
        Self {
            users: Vec::new(),
            groups: Vec::new(),
        }
    }

    /// Append a candidate user record.
    ///
    /// Duplicates and dangling group references are not checked here;
    /// they are surfaced by [`Self::verify`] so a half-rejected table can
    /// never be observed.
    pub fn push_user(&mut self, user: UserRecord) {
        self.users.push(user);
    }

    /// Append a candidate group record.
    pub fn push_group(&mut self, group: GroupRecord) {
        self.groups.push(group);
    }

    /// Verify the builder and freeze it into an [`IdentityTable`].
    ///
    /// On success emits exactly one
    /// [`AuditEvent::IdentityTableLoaded`].
    /// On failure emits exactly one
    /// [`AuditEvent::IdentityTableRejected`]
    /// and returns the offending [`Errno`].
    ///
    /// # Errors
    ///
    /// * [`Errno::BadMagic`] — duplicate `uid` or `gid` in the input.
    /// * [`Errno::NotFound`] — a user references a `gid` that has no
    ///   matching [`GroupRecord`].
    /// * [`Errno::LengthOutOfRange`] — a user's supplementary-group set
    ///   exceeds [`MAX_SUPPLEMENTARY_GROUPS`].
    pub fn verify<S: Sink + ?Sized>(self, audit: &S) -> Result<IdentityTable, Errno> {
        if let Err(err) = self.check_invariants() {
            // One audit record per failed decision, with the failure
            // cause carried as a structured field (its numeric Errno).
            let mut buf = [0u8; 12];
            let cause = format_i32(err.as_i32(), &mut buf);
            record(
                audit,
                AuditEvent::IdentityTableRejected,
                &[Field {
                    key: "errno",
                    value: cause,
                }],
            );
            return Err(err);
        }

        let mut buf_u = [0u8; 12];
        let user_count = format_usize(self.users.len(), &mut buf_u);
        let mut buf_g = [0u8; 12];
        let group_count = format_usize(self.groups.len(), &mut buf_g);
        record(
            audit,
            AuditEvent::IdentityTableLoaded,
            &[
                Field {
                    key: "users",
                    value: user_count,
                },
                Field {
                    key: "groups",
                    value: group_count,
                },
            ],
        );
        Ok(IdentityTable {
            users: self.users,
            groups: self.groups,
        })
    }

    fn check_invariants(&self) -> Result<(), Errno> {
        // Duplicate uid.
        for (i, u) in self.users.iter().enumerate() {
            if self.users[i + 1..].iter().any(|v| v.uid == u.uid) {
                return Err(Errno::BadMagic);
            }
        }
        // Duplicate gid.
        for (i, g) in self.groups.iter().enumerate() {
            if self.groups[i + 1..].iter().any(|h| h.gid == g.gid) {
                return Err(Errno::BadMagic);
            }
        }
        for u in &self.users {
            // Supplementary cap.
            if u.supplementary_gids.len() > MAX_SUPPLEMENTARY_GROUPS {
                return Err(Errno::LengthOutOfRange);
            }
            // Primary group must exist.
            if !self.groups.iter().any(|g| g.gid == u.primary_gid) {
                return Err(Errno::NotFound);
            }
            // Supplementary groups must exist.
            for sup in &u.supplementary_gids {
                if !self.groups.iter().any(|g| g.gid == *sup) {
                    return Err(Errno::NotFound);
                }
            }
        }
        Ok(())
    }
}

// The audit-field formatters (`format_i32`, `format_usize`,
// `format_hex_u64`) used to live here. They were extracted into
// `lib/util::fmt` in Stage 2.5 once `kernel/ipc` became a second
// caller (`AGENTS.md` §2.2 / §6). Re-exported under their original
// crate-local names so the existing call sites and tests are not
// touched purely for the rename.
pub(crate) use rustos_util::fmt::{format_hex_u64, format_i32, format_usize};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit::AuditEvent;
    use crate::audit::RecordingSink;
    use rustos_abi::CapabilityId;

    fn sample_group(g: u32) -> GroupRecord {
        GroupRecord { gid: GroupId(g) }
    }

    fn sample_user(u: u32, primary: u32, sup: &[u32]) -> UserRecord {
        let mut caps = CapabilitySet::empty();
        caps.insert(CapabilityId::FS_MOUNT);
        UserRecord {
            uid: UserId(u),
            primary_gid: GroupId(primary),
            supplementary_gids: sup.iter().copied().map(GroupId).collect(),
            capability_grants: caps,
        }
    }

    #[test]
    fn builder_accepts_well_formed_input_and_emits_one_event() {
        let sink = RecordingSink::new();
        let mut b = IdentityTableBuilder::new();
        b.push_group(sample_group(0));
        b.push_group(sample_group(1));
        b.push_user(sample_user(1000, 0, &[1]));
        let table = b.verify(&sink).expect("valid");
        assert_eq!(table.user_count(), 1);
        assert_eq!(table.group_count(), 2);
        assert_eq!(sink.ids(), [AuditEvent::IdentityTableLoaded.id().0]);
    }

    #[test]
    fn duplicate_uid_is_rejected() {
        let sink = RecordingSink::new();
        let mut b = IdentityTableBuilder::new();
        b.push_group(sample_group(0));
        b.push_user(sample_user(7, 0, &[]));
        b.push_user(sample_user(7, 0, &[]));
        assert_eq!(b.verify(&sink), Err(Errno::BadMagic));
        assert_eq!(sink.ids(), [AuditEvent::IdentityTableRejected.id().0]);
    }

    #[test]
    fn duplicate_gid_is_rejected() {
        let sink = RecordingSink::new();
        let mut b = IdentityTableBuilder::new();
        b.push_group(sample_group(5));
        b.push_group(sample_group(5));
        assert_eq!(b.verify(&sink), Err(Errno::BadMagic));
        assert_eq!(sink.ids(), [AuditEvent::IdentityTableRejected.id().0]);
    }

    #[test]
    fn unknown_primary_group_is_rejected() {
        let sink = RecordingSink::new();
        let mut b = IdentityTableBuilder::new();
        b.push_group(sample_group(0));
        b.push_user(sample_user(1, 99, &[]));
        assert_eq!(b.verify(&sink), Err(Errno::NotFound));
        assert_eq!(sink.ids(), [AuditEvent::IdentityTableRejected.id().0]);
    }

    #[test]
    fn unknown_supplementary_group_is_rejected() {
        let sink = RecordingSink::new();
        let mut b = IdentityTableBuilder::new();
        b.push_group(sample_group(0));
        b.push_user(sample_user(1, 0, &[42]));
        assert_eq!(b.verify(&sink), Err(Errno::NotFound));
    }

    #[test]
    fn oversize_supplementary_set_is_rejected() {
        let sink = RecordingSink::new();
        let mut b = IdentityTableBuilder::new();
        b.push_group(sample_group(0));
        let upper = u32::try_from(MAX_SUPPLEMENTARY_GROUPS).expect("fits in u32") + 1;
        for g in 1..=upper {
            b.push_group(sample_group(g));
        }
        let sups: Vec<u32> = (1..=upper).collect();
        b.push_user(sample_user(1, 0, &sups));
        assert_eq!(b.verify(&sink), Err(Errno::LengthOutOfRange));
    }

    #[test]
    fn lookup_unknown_user_returns_not_found() {
        let sink = RecordingSink::new();
        let mut b = IdentityTableBuilder::new();
        b.push_group(sample_group(0));
        b.push_user(sample_user(1, 0, &[]));
        let table = b.verify(&sink).unwrap();
        assert_eq!(table.user(UserId(999)).err(), Some(Errno::NotFound));
        assert_eq!(table.group(GroupId(999)).err(), Some(Errno::NotFound));
    }

    #[test]
    fn membership_query_covers_primary_and_supplementary() {
        let sink = RecordingSink::new();
        let mut b = IdentityTableBuilder::new();
        b.push_group(sample_group(0));
        b.push_group(sample_group(1));
        b.push_group(sample_group(2));
        b.push_user(sample_user(10, 0, &[1, 2]));
        let table = b.verify(&sink).unwrap();
        assert!(table.is_member_of(UserId(10), GroupId(0))); // primary
        assert!(table.is_member_of(UserId(10), GroupId(1))); // supplementary
        assert!(table.is_member_of(UserId(10), GroupId(2)));
        assert!(!table.is_member_of(UserId(10), GroupId(3)));
        assert!(!table.is_member_of(UserId(11), GroupId(0))); // unknown user
    }

    #[test]
    fn uid_zero_is_not_ambient_root() {
        // A uid==0 record carries the same capability grant as any other
        // user: nothing more, nothing less. The kernel never confers
        // "root" powers based on the numeric uid.
        let sink = RecordingSink::new();
        let mut b = IdentityTableBuilder::new();
        b.push_group(sample_group(0));
        let mut zero_user = sample_user(0, 0, &[]);
        zero_user.capability_grants = CapabilitySet::empty();
        b.push_user(zero_user);
        let table = b.verify(&sink).unwrap();
        assert!(table.user(UserId(0)).unwrap().capability_grants.is_empty());
    }

    #[test]
    fn format_i32_examples() {
        let mut buf = [0u8; 12];
        assert_eq!(format_i32(0, &mut buf), "0");
        assert_eq!(format_i32(42, &mut buf), "42");
        assert_eq!(format_i32(-7, &mut buf), "-7");
        assert_eq!(format_i32(i32::MAX, &mut buf), "2147483647");
        assert_eq!(format_i32(i32::MIN + 1, &mut buf), "-2147483647");
    }
}
