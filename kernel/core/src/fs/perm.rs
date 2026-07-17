//! Per-inode permission model: POSIX mode bits **plus** ACLs **plus** a
//! capability gate.
//!
//! [`Metadata::authorize`] is the single decision point every VFS
//! operation routes through. It fails closed and never branches on
//! `uid == 0`: authority comes from capability grants, ACL entries, and
//! mode bits, never from a magic user id.
//!
//! The three layers compose in a fixed, documented order:
//!
//! 1. **Capability gate.** If the inode declares a required capability and
//!    the caller does not hold it, access is denied *regardless* of mode
//!    bits — a file marked `CAP_AUDIT_READ` is unreadable at mode `0644`
//!    by a caller without that capability.
//! 2. **ACL.** An explicit deny for the requested access wins; otherwise an
//!    explicit allow grants it. With no matching entry, the decision falls
//!    through to the mode bits.
//! 3. **Mode bits.** The owner / owning-group / other triad is selected by
//!    the caller's identity and the requested `rwx` bit is checked.

use alloc::vec::Vec;

use tairix_abi::driver::filesystem::{NodeSecurity, SecuritySubject};
use tairix_abi::CapabilityId;
use tairix_abi::CapabilityQuery;
use tairix_kernel_sec::{GroupId, UserId};

use super::VfsError;

/// A single access right requested against an inode.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum Access {
    /// Read the file's contents or list a directory.
    Read,
    /// Modify the file's contents or the directory's entries.
    Write,
    /// Execute the file or search (traverse) the directory.
    Execute,
}

impl Access {
    /// The POSIX `rwx` bit for this access within a single mode triad.
    const fn bit(self) -> u16 {
        match self {
            Self::Read => 0b100,
            Self::Write => 0b010,
            Self::Execute => 0b001,
        }
    }
}

/// POSIX permission bits (the low 12 bits: `setuid`, `setgid`, sticky, and
/// the owner/group/other `rwx` triads).
#[repr(transparent)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Default)]
pub struct Mode(u16);

impl Mode {
    /// `setuid` bit (`0o4000`).
    pub const SETUID: u16 = 0o4000;
    /// `setgid` bit (`0o2000`).
    pub const SETGID: u16 = 0o2000;
    /// Sticky bit (`0o1000`).
    pub const STICKY: u16 = 0o1000;

    /// Construct a [`Mode`] from its raw permission bits, masking off
    /// anything above the low 12.
    #[must_use]
    pub const fn from_bits(bits: u16) -> Self {
        Self(bits & 0o7777)
    }

    /// The raw permission bits.
    #[must_use]
    pub const fn bits(self) -> u16 {
        self.0
    }

    /// `true` if any of `flags` (e.g. [`Mode::SETUID`]) are set.
    #[must_use]
    pub const fn has(self, flags: u16) -> bool {
        self.0 & flags == flags
    }

    /// The owner triad's `rwx` bits (`0..=7`).
    const fn owner_rwx(self) -> u16 {
        (self.0 >> 6) & 0b111
    }

    /// The owning-group triad's `rwx` bits (`0..=7`).
    const fn group_rwx(self) -> u16 {
        (self.0 >> 3) & 0b111
    }

    /// The other triad's `rwx` bits (`0..=7`).
    const fn other_rwx(self) -> u16 {
        self.0 & 0b111
    }
}

/// Which principal an [`AclEntry`] applies to.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum AclWho {
    /// A specific user.
    User(UserId),
    /// A specific group (matched against the caller's primary and
    /// supplementary groups).
    Group(GroupId),
}

/// A single access-control list entry: an explicit allow or deny of one
/// [`Access`] for one [`AclWho`].
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub struct AclEntry {
    /// The principal the entry applies to.
    pub who: AclWho,
    /// The access right the entry governs.
    pub access: Access,
    /// `true` to allow, `false` to deny.
    pub allow: bool,
}

/// The caller's security identity, as seen by the VFS.
///
/// Borrows the capability query rather than owning it: the live set lives
/// in the task's [`tairix_kernel_sec::TaskCapabilities`], which the VFS
/// never copies.
#[derive(Copy, Clone)]
pub struct Credentials<'a> {
    /// Numeric user id.
    pub uid: UserId,
    /// Primary group id.
    pub gid: GroupId,
    /// Supplementary group ids.
    pub supplementary_gids: &'a [GroupId],
    /// The caller's granted capabilities.
    pub caps: &'a dyn CapabilityQuery,
}

impl Credentials<'_> {
    /// `true` if the caller is a member of `gid` through its primary or
    /// supplementary groups.
    #[must_use]
    pub fn is_in_group(&self, gid: GroupId) -> bool {
        self.gid == gid || self.supplementary_gids.contains(&gid)
    }
}

/// Everything the VFS stores about an inode for the purpose of access
/// control. Mirrors the on-disk inode header describes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Metadata {
    /// Owning user.
    pub owner: UserId,
    /// Owning group.
    pub group: GroupId,
    /// POSIX mode bits.
    pub mode: Mode,
    /// An optional capability the caller must hold to access the inode at
    /// all, on top of the mode/ACL checks.
    pub required_cap: Option<CapabilityId>,
    /// Explicit allow/deny entries, consulted before the mode bits.
    pub acl: Vec<AclEntry>,
}

impl Metadata {
    /// Construct metadata with the given owner, group, and mode and no
    /// capability gate or ACL entries.
    #[must_use]
    pub fn new(owner: UserId, group: GroupId, mode: Mode) -> Self {
        Self {
            owner,
            group,
            mode,
            required_cap: None,
            acl: Vec::new(),
        }
    }

    /// Translate a filesystem driver's stored security record
    /// ([`NodeSecurity`]) into the VFS policy [`Metadata`].
    ///
    /// This is the bridge that lets a driver such as `arxfs`, which stores
    /// full per-inode ownership, mode bits, an ACL, and an optional
    /// capability gate, drive the decision instead of a uniform
    /// mount-point template. Each grant-only driver ACL entry expands into
    /// one *allow* [`AclEntry`] per `rwx` bit it grants; the driver surface
    /// carries no explicit deny.
    #[must_use]
    pub fn from_node_security(sec: &NodeSecurity) -> Self {
        let mode = Mode::from_bits(u16::try_from(sec.mode & 0o7777).unwrap_or(0));
        let mut meta = Self::new(UserId(sec.uid), GroupId(sec.gid), mode);
        meta.required_cap = sec.required_cap;
        for entry in sec.acl() {
            let who = match entry.subject {
                SecuritySubject::User(id) => AclWho::User(UserId(id)),
                SecuritySubject::Group(id) => AclWho::Group(GroupId(id)),
            };
            for access in [Access::Read, Access::Write, Access::Execute] {
                if u16::from(entry.perms) & access.bit() != 0 {
                    meta.acl.push(AclEntry {
                        who,
                        access,
                        allow: true,
                    });
                }
            }
        }
        meta
    }

    /// Decide whether `cred` may perform `access` on this inode.
    ///
    /// # Errors
    ///
    /// Returns [`VfsError::PermissionDenied`] if the capability gate, an
    /// ACL deny, or the mode bits forbid the access.
    pub fn authorize(&self, cred: &Credentials<'_>, access: Access) -> Result<(), VfsError> {
        if let Some(cap) = self.required_cap {
            if !cred.caps.holds(cap) {
                return Err(VfsError::PermissionDenied);
            }
        }

        match self.acl_decision(cred, access) {
            Some(true) => return Ok(()),
            Some(false) => return Err(VfsError::PermissionDenied),
            None => {}
        }

        if self.mode_triad(cred) & access.bit() != 0 {
            Ok(())
        } else {
            Err(VfsError::PermissionDenied)
        }
    }

    /// Resolve the ACL for `access`: `Some(false)` if any matching entry
    /// denies, `Some(true)` if one allows (and none deny), `None` if no
    /// entry matches.
    fn acl_decision(&self, cred: &Credentials<'_>, access: Access) -> Option<bool> {
        let mut allowed = None;
        for entry in &self.acl {
            if entry.access != access || !Self::acl_matches(cred, entry.who) {
                continue;
            }
            if !entry.allow {
                return Some(false);
            }
            allowed = Some(true);
        }
        allowed
    }

    /// `true` if `who` names a principal the caller acts as.
    fn acl_matches(cred: &Credentials<'_>, who: AclWho) -> bool {
        match who {
            AclWho::User(uid) => cred.uid == uid,
            AclWho::Group(gid) => cred.is_in_group(gid),
        }
    }

    /// Select the owner / group / other mode triad for `cred`.
    fn mode_triad(&self, cred: &Credentials<'_>) -> u16 {
        if cred.uid == self.owner {
            self.mode.owner_rwx()
        } else if cred.is_in_group(self.group) {
            self.mode.group_rwx()
        } else {
            self.mode.other_rwx()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tairix_caps::CapabilitySet;

    fn creds<'a>(
        uid: u32,
        gid: u32,
        sup: &'a [GroupId],
        caps: &'a CapabilitySet,
    ) -> Credentials<'a> {
        Credentials {
            uid: UserId(uid),
            gid: GroupId(gid),
            supplementary_gids: sup,
            caps,
        }
    }

    #[test]
    fn mode_triads_decode_correctly() {
        let m = Mode::from_bits(0o751);
        assert_eq!(m.owner_rwx(), 0b111);
        assert_eq!(m.group_rwx(), 0b101);
        assert_eq!(m.other_rwx(), 0b001);
        assert!(Mode::from_bits(0o4755).has(Mode::SETUID));
    }

    #[test]
    fn owner_uses_owner_triad() {
        let caps = CapabilitySet::empty();
        let meta = Metadata::new(UserId(7), GroupId(3), Mode::from_bits(0o700));
        let cred = creds(7, 3, &[], &caps);
        assert!(meta.authorize(&cred, Access::Read).is_ok());
        assert!(meta.authorize(&cred, Access::Write).is_ok());
        // A non-owner, non-group caller gets the (empty) other triad.
        let other = creds(8, 9, &[], &caps);
        assert_eq!(
            meta.authorize(&other, Access::Read),
            Err(VfsError::PermissionDenied)
        );
    }

    #[test]
    fn group_membership_uses_group_triad() {
        let caps = CapabilitySet::empty();
        let meta = Metadata::new(UserId(7), GroupId(3), Mode::from_bits(0o070));
        let sup = [GroupId(3)];
        let member = creds(8, 1, &sup, &caps);
        assert!(meta.authorize(&member, Access::Read).is_ok());
        let nonmember = creds(8, 1, &[], &caps);
        assert_eq!(
            meta.authorize(&nonmember, Access::Read),
            Err(VfsError::PermissionDenied)
        );
    }

    #[test]
    fn capability_gate_overrides_open_mode_bits() {
        // mode 0644 would allow the read, but the capability gate refuses.
        let mut meta = Metadata::new(UserId(7), GroupId(3), Mode::from_bits(0o644));
        meta.required_cap = Some(CapabilityId::AUDIT_READ);

        let without = CapabilitySet::empty();
        let cred = creds(7, 3, &[], &without);
        assert_eq!(
            meta.authorize(&cred, Access::Read),
            Err(VfsError::PermissionDenied)
        );

        let mut with = CapabilitySet::empty();
        with.insert(CapabilityId::AUDIT_READ);
        let cred = creds(7, 3, &[], &with);
        assert!(meta.authorize(&cred, Access::Read).is_ok());
    }

    #[test]
    fn acl_deny_beats_permissive_mode() {
        let caps = CapabilitySet::empty();
        let mut meta = Metadata::new(UserId(7), GroupId(3), Mode::from_bits(0o777));
        meta.acl.push(AclEntry {
            who: AclWho::User(UserId(8)),
            access: Access::Write,
            allow: false,
        });
        let cred = creds(8, 9, &[], &caps);
        assert!(meta.authorize(&cred, Access::Read).is_ok());
        assert_eq!(
            meta.authorize(&cred, Access::Write),
            Err(VfsError::PermissionDenied)
        );
    }

    #[test]
    fn acl_allow_grants_where_mode_would_deny() {
        let caps = CapabilitySet::empty();
        let mut meta = Metadata::new(UserId(7), GroupId(3), Mode::from_bits(0o600));
        meta.acl.push(AclEntry {
            who: AclWho::Group(GroupId(42)),
            access: Access::Read,
            allow: true,
        });
        let sup = [GroupId(42)];
        let cred = creds(8, 1, &sup, &caps);
        assert!(meta.authorize(&cred, Access::Read).is_ok());
        // No allow entry for Write: falls through to the (denying) mode bits.
        assert_eq!(
            meta.authorize(&cred, Access::Write),
            Err(VfsError::PermissionDenied)
        );
    }

    #[test]
    fn uid_zero_gets_no_special_treatment() {
        let caps = CapabilitySet::empty();
        let meta = Metadata::new(UserId(7), GroupId(3), Mode::from_bits(0o000));
        let root = creds(0, 0, &[], &caps);
        assert_eq!(
            meta.authorize(&root, Access::Read),
            Err(VfsError::PermissionDenied)
        );
    }

    #[test]
    fn from_node_security_carries_owner_mode_cap_and_acl() {
        use tairix_abi::driver::filesystem::{NodeSecurity, SecurityAcl, SecuritySubject};

        let mut sec = NodeSecurity::new(0o600, 7, 3);
        sec.required_cap = Some(CapabilityId::AUDIT_READ);
        // A group ACL grant of read+write (0b110) for gid 42.
        sec.push_acl(SecurityAcl {
            subject: SecuritySubject::Group(42),
            perms: 0b110,
        })
        .expect("acl");

        let meta = Metadata::from_node_security(&sec);
        assert_eq!(meta.owner, UserId(7));
        assert_eq!(meta.group, GroupId(3));
        assert_eq!(meta.mode, Mode::from_bits(0o600));
        assert_eq!(meta.required_cap, Some(CapabilityId::AUDIT_READ));
        // The single rw grant expands into one allow entry per bit.
        assert_eq!(meta.acl.len(), 2);

        // A gid-42 member, without the capability, is still gated out.
        let without = CapabilitySet::empty();
        let denied = creds(8, 1, &[GroupId(42)], &without);
        assert_eq!(
            meta.authorize(&denied, Access::Read),
            Err(VfsError::PermissionDenied)
        );

        // With the capability the ACL grant lets the group member read and
        // write, where the owner-only mode `0o600` would otherwise deny.
        let mut with = CapabilitySet::empty();
        with.insert(CapabilityId::AUDIT_READ);
        let member = creds(8, 1, &[GroupId(42)], &with);
        assert!(meta.authorize(&member, Access::Read).is_ok());
        assert!(meta.authorize(&member, Access::Write).is_ok());
        // No grant for execute: falls through to the denying mode bits.
        assert_eq!(
            meta.authorize(&member, Access::Execute),
            Err(VfsError::PermissionDenied)
        );
    }
}
