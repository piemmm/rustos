//! Boot-time load of the `/System/Security/Groups` registry off the mounted
//! root volume, and the construction of the kernel's authoritative
//! [`IdentityTable`] from the user and group databases together.
//!
//! Groups are first-class objects (their own registry, persisted separately
//! from users). [`load_groups_db`] is the kernel's root-volume read path for
//! that registry: given the live [`FilesystemRead`] + [`FilesystemSecurity`]
//! driver of the mounted root volume, it resolves [`GROUPS_DB_PATH`] through
//! the permission-checked per-inode delegation, bounds the file against
//! [`rustos_users::MAX_GROUPS_DB_LEN`] *before* reading it, and parses the
//! bytes through the fail-closed [`rustos_users::GroupsDb`] parser. The
//! bounded `uid 0` read is the one shared with [`crate::users`]
//! (`crate::fs`'s bootstrap-file reader), so neither file copies it.
//!
//! [`build_identity_table`] is the bridge from the on-disk databases to the
//! in-kernel [`IdentityTable`]: it pushes one
//! [`rustos_kernel_sec::GroupRecord`] per group and one
//! [`rustos_kernel_sec::UserRecord`] per user — carrying only the
//! `(uid, primary gid, supplementary gids, capability grants)` the kernel
//! consults, never any password material — and runs the verifying
//! [`IdentityTableBuilder`], which fails closed if a user references a group
//! that has no registry record (referential integrity) or any other
//! invariant is violated. The verifier emits the single
//! [`rustos_kernel_sec::AuditEvent`] outcome record.
//!
//! Every load outcome is audited with a stable event id
//! ([`AuditEvent::GroupsDbLoaded`] / [`AuditEvent::GroupsDbRejected`]); every
//! failure yields **no** registry, so a system whose group registry cannot be
//! read installs no identity table and the filesystem path resolves no caller
//! groups rather than inventing them (fail closed).

use rustos_abi::driver::filesystem::{FilesystemRead, FilesystemSecurity};
use rustos_abi::Errno;
use rustos_kernel_sec::{
    GroupId, GroupRecord, IdentityTable, IdentityTableBuilder, UserId, UserRecord,
};
use rustos_log::{Field, Level, Sink};
use rustos_users::{GroupsDb, ParseError, UsersDb, MAX_GROUPS_DB_LEN};
use rustos_util::fmt::format_usize;

use crate::audit::{emit, AuditEvent};
use crate::fs::{read_bootstrap_file, BootstrapReadError, VfsError};

/// Absolute path of the group registry on the root volume.
pub const GROUPS_DB_PATH: &str = "/System/Security/Groups";

/// Why [`load_groups_db`] yielded no group registry.
///
/// Each variant carries the underlying refusal; the load stops at the first
/// failure and returns it (fail closed, never a partial registry).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum GroupsLoadError {
    /// Resolving or reading [`GROUPS_DB_PATH`] failed (missing file,
    /// permission refusal, driver fault, …).
    Vfs(VfsError),
    /// The path names a directory, not a regular file.
    NotAFile,
    /// The file exceeds [`MAX_GROUPS_DB_LEN`]; it is refused before any byte
    /// is read.
    TooLarge,
    /// The driver returned fewer bytes than the file's reported size; a
    /// truncated registry is never parsed.
    ShortRead,
    /// The file is not valid UTF-8.
    NotUtf8,
    /// The text failed the `groups-v1` validation.
    Parse(ParseError),
}

impl GroupsLoadError {
    /// Short, stable, secret-free cause string carried by the audit record.
    #[must_use]
    pub fn cause(self) -> &'static str {
        match self {
            Self::Vfs(VfsError::NotFound) => "not_found",
            Self::Vfs(VfsError::PermissionDenied) => "permission_denied",
            Self::Vfs(_) => "vfs_error",
            Self::NotAFile => "not_a_file",
            Self::TooLarge => "too_large",
            Self::ShortRead => "short_read",
            Self::NotUtf8 => "not_utf8",
            Self::Parse(_) => "parse_rejected",
        }
    }
}

impl From<BootstrapReadError> for GroupsLoadError {
    fn from(err: BootstrapReadError) -> Self {
        match err {
            BootstrapReadError::Vfs(err) => Self::Vfs(err),
            BootstrapReadError::NotAFile => Self::NotAFile,
            BootstrapReadError::TooLarge => Self::TooLarge,
            BootstrapReadError::ShortRead => Self::ShortRead,
        }
    }
}

/// Read and parse `/System/Security/Groups` from the mounted root volume's
/// filesystem driver.
///
/// On success the [`AuditEvent::GroupsDbLoaded`] record carries the group
/// count; on failure the [`AuditEvent::GroupsDbRejected`] record carries the
/// [`cause`](GroupsLoadError::cause) and no registry exists. The registry
/// text carries no credential bytes (it is the public name↔gid map), so the
/// read buffer needs no zeroisation.
///
/// # Errors
///
/// The [`GroupsLoadError`] naming the first check that refused.
pub fn load_groups_db<F>(fs: &mut F, audit: &dyn Sink) -> Result<GroupsDb, GroupsLoadError>
where
    F: FilesystemRead + FilesystemSecurity + ?Sized,
{
    let parsed = match read_bootstrap_file(fs, GROUPS_DB_PATH, MAX_GROUPS_DB_LEN) {
        Ok(buf) => parse_groups_text(&buf),
        Err(err) => Err(GroupsLoadError::from(err)),
    };
    match &parsed {
        Ok(db) => audit_load(audit, Some(db.records().len()), None),
        Err(err) => audit_load(audit, None, Some(*err)),
    }
    parsed
}

/// Build the kernel's authoritative [`IdentityTable`] from the on-disk user
/// and group databases.
///
/// Pushes one [`GroupRecord`] per group and one [`UserRecord`] per user
/// (carrying only the kernel-consulted `(uid, primary gid, supplementary
/// gids, capability grants)`), then runs the verifying [`IdentityTableBuilder`].
/// The verifier enforces referential integrity — a user referencing a group
/// with no registry record fails the build — and uniqueness, and emits the
/// single [`rustos_kernel_sec::AuditEvent`] outcome record.
///
/// # Errors
///
/// The [`Errno`] the verifier raises ([`Errno::BadMagic`] for a duplicate id,
/// [`Errno::NotFound`] for a dangling group reference,
/// [`Errno::LengthOutOfRange`] for an over-large supplementary set), failing
/// closed with no table.
pub fn build_identity_table(
    users: &UsersDb,
    groups: &GroupsDb,
    audit: &dyn Sink,
) -> Result<IdentityTable, Errno> {
    let mut builder = IdentityTableBuilder::new();
    for group in groups.records() {
        builder.push_group(GroupRecord {
            gid: GroupId(group.gid().0),
        });
    }
    for user in users.records() {
        builder.push_user(UserRecord {
            uid: UserId(user.uid().0),
            primary_gid: GroupId(user.primary_gid().0),
            supplementary_gids: user
                .supplementary_gids()
                .iter()
                .map(|gid| GroupId(gid.0))
                .collect(),
            capability_grants: user.capabilities(),
        });
    }
    builder.verify(audit)
}

/// Emit the single shared load outcome record: [`AuditEvent::GroupsDbLoaded`]
/// with the group count on success, else [`AuditEvent::GroupsDbRejected`]
/// with the refusal cause. Exactly one of `records` (loaded) or `err`
/// (rejected) is `Some`; a `records` of `Some` wins.
fn audit_load(audit: &dyn Sink, records: Option<usize>, err: Option<GroupsLoadError>) {
    if let Some(records) = records {
        let mut count_buf = [0u8; 12];
        let records = format_usize(records, &mut count_buf);
        emit(
            audit,
            Level::Info,
            AuditEvent::GroupsDbLoaded,
            &[
                Field {
                    key: "path",
                    value: GROUPS_DB_PATH,
                },
                Field {
                    key: "records",
                    value: records,
                },
            ],
        );
    } else if let Some(err) = err {
        emit(
            audit,
            Level::Error,
            AuditEvent::GroupsDbRejected,
            &[
                Field {
                    key: "path",
                    value: GROUPS_DB_PATH,
                },
                Field {
                    key: "cause",
                    value: err.cause(),
                },
            ],
        );
    }
}

/// Validate `buf` as UTF-8 and parse it with the fail-closed `groups-v1`
/// parser.
fn parse_groups_text(buf: &[u8]) -> Result<GroupsDb, GroupsLoadError> {
    match core::str::from_utf8(buf) {
        Ok(text) => GroupsDb::parse(text).map_err(GroupsLoadError::Parse),
        Err(_) => Err(GroupsLoadError::NotUtf8),
    }
}

#[cfg(test)]
#[path = "groups_tests.rs"]
mod tests;
