//! Boot-time load of the `/System/Security/Users` database off the
//! mounted root volume (`AGENTS.md` §5.1, `plans/PI.md` P11).
//!
//! [`load_users_db`] is the kernel's root-volume read path for the user
//! database: given the live [`FilesystemRead`] +
//! [`FilesystemSecurity`] driver of the mounted root volume (rustfs on a
//! real installation), it resolves [`USERS_DB_PATH`] through the VFS's
//! §5.3-checked per-inode delegation ([`Vfs::read_via_secured`]), bounds
//! the file against [`rustos_users::MAX_DB_LEN`] *before* reading it,
//! and parses the bytes through the fail-closed
//! [`rustos_users::UsersDb`] parser. The parsed database is what the
//! login path holds to authenticate sessions.
//!
//! Every outcome is audited with a stable event id
//! ([`AuditEvent::UsersDbLoaded`] / [`AuditEvent::UsersDbRejected`],
//! `AGENTS.md` §5.4.4), and every failure yields **no** database — a
//! system whose user database cannot be read refuses every login rather
//! than inventing accounts (`AGENTS.md` §5.4.5 — fail closed).
//!
//! # Credentials of the boot read
//!
//! The read runs under the kernel's bootstrap identity — `uid 0`,
//! `gid 0`, **no** capabilities. `uid 0` carries no ambient power
//! (`AGENTS.md` §5.1): the read succeeds because the database's stored
//! §5.3 record makes it owner-readable, not because the kernel bypasses
//! the check. A capability-gated database is therefore refused — the
//! §11 installer authors the record, and gating the file against the
//! boot reader is a configuration defect surfaced loudly, never silently
//! bypassed.

use alloc::vec;

use rustos_abi::driver::filesystem::{FilesystemRead, FilesystemSecurity, NodeKind};
use rustos_abi::driver::DriverHandle;
use rustos_abi::Errno;
use rustos_caps::CapabilitySet;
use rustos_kernel_sec::{GroupId, UserId};
use rustos_log::{Field, Level, Sink};
use rustos_users::{ParseError, UsersDb, MAX_DB_LEN};
use rustos_util::fmt::format_usize;

use crate::audit::{emit, AuditEvent};
use crate::fs::{Credentials, Metadata, Mode, Path, Vfs, VfsError};

/// Absolute path of the user database on the root volume
/// (`AGENTS.md` §16.2).
pub const USERS_DB_PATH: &str = "/System/Security/Users";

/// The kernel-held user database the `users_db_read` syscall serves
/// (`plans/PI.md` P11).
///
/// The boot path that mounts the root volume and runs [`load_users_db`]
/// installs an implementation holding the validated `users-v1` text;
/// the syscall handler copies that exact text out to the (capability-
/// gated, `CAP_USERS_READ`) caller, which re-parses it with the same
/// fail-closed `rustos-users` parser. Serving the *text* rather than a
/// re-serialisation keeps one canonical byte representation end to end
/// (`AGENTS.md` §2.2).
///
/// `Sync` because the single installed source is shared by the per-CPU
/// syscall handlers, exactly like [`crate::ConsoleWrite`].
pub trait UsersDbSource: Sync {
    /// The held database's exact `users-v1` text.
    ///
    /// # Errors
    ///
    /// [`Errno::NotFound`] when no database is held — the root volume
    /// is not mounted, or the boot read refused the record — so a
    /// system without accounts refuses every login rather than
    /// inventing one (`AGENTS.md` §5.4.5). The default
    /// [`NullUsersDbSource`] returns [`Errno::NotImplemented`] to mark
    /// an inert interface (`AGENTS.md` §2.9).
    fn text(&self) -> Result<&[u8], Errno>;
}

/// The users-database source installed before any real holder exists.
///
/// Every read fails closed with [`Errno::NotImplemented`] — a kernel
/// build with no users-database service wired never fabricates
/// accounts (`AGENTS.md` §2.9 / §5.4).
#[derive(Debug, Default, Copy, Clone)]
pub struct NullUsersDbSource;

impl UsersDbSource for NullUsersDbSource {
    fn text(&self) -> Result<&[u8], Errno> {
        Err(Errno::NotImplemented)
    }
}

/// The shared [`NullUsersDbSource`] instance the syscall handler
/// defaults to until a boot path installs a real holder through
/// `KernelSyscallHandlers::with_users_db` (mirrors
/// [`crate::console::NULL_CONSOLE`]).
pub static NULL_USERS_DB: NullUsersDbSource = NullUsersDbSource;

/// `DriverHandle` the loader's private root mount carries. The loader
/// maps the handle to the caller's borrowed driver itself, so the value
/// only needs to be non-zero; it spells `root` for log legibility.
const ROOT_VOLUME_HANDLE: u64 = 0x726F_6F74;

/// Why [`load_users_db`] yielded no database.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum UsersLoadError {
    /// Resolving or reading [`USERS_DB_PATH`] failed (missing file,
    /// permission refusal, driver fault, …).
    Vfs(VfsError),
    /// The path names a directory, not a regular file.
    NotAFile,
    /// The file exceeds [`MAX_DB_LEN`]; it is refused before any byte
    /// is read (`AGENTS.md` §5.4.3).
    TooLarge,
    /// The driver returned fewer bytes than the file's reported size; a
    /// truncated database is never parsed (`AGENTS.md` §2.9).
    ShortRead,
    /// The file is not valid UTF-8.
    NotUtf8,
    /// The text failed the `users-v1` validation.
    Parse(ParseError),
}

impl UsersLoadError {
    /// Short stable cause string carried by the audit record.
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

impl From<VfsError> for UsersLoadError {
    fn from(err: VfsError) -> Self {
        Self::Vfs(err)
    }
}

/// Read and parse `/System/Security/Users` from the mounted root
/// volume's filesystem driver.
///
/// On success the [`AuditEvent::UsersDbLoaded`] record carries the
/// account count; on failure the [`AuditEvent::UsersDbRejected`] record
/// carries the [`cause`](UsersLoadError::cause) and no database exists
/// (`AGENTS.md` §5.4.5). The intermediate read buffer is zeroed before
/// release — the database carries credential records (`AGENTS.md` §4).
///
/// # Errors
///
/// The [`UsersLoadError`] naming the first check that refused.
pub fn load_users_db<F>(fs: &mut F, audit: &dyn Sink) -> Result<UsersDb, UsersLoadError>
where
    F: FilesystemRead + FilesystemSecurity + ?Sized,
{
    match load_inner(fs) {
        Ok(db) => {
            let mut count_buf = [0u8; 12];
            let records = format_usize(db.records().len(), &mut count_buf);
            emit(
                audit,
                Level::Info,
                AuditEvent::UsersDbLoaded,
                &[
                    Field {
                        key: "path",
                        value: USERS_DB_PATH,
                    },
                    Field {
                        key: "records",
                        value: records,
                    },
                ],
            );
            Ok(db)
        }
        Err(err) => {
            emit(
                audit,
                Level::Error,
                AuditEvent::UsersDbRejected,
                &[
                    Field {
                        key: "path",
                        value: USERS_DB_PATH,
                    },
                    Field {
                        key: "cause",
                        value: err.cause(),
                    },
                ],
            );
            Err(err)
        }
    }
}

/// The unaudited body of [`load_users_db`]: every `?` is reported by
/// the caller's single rejection record.
fn load_inner<F>(fs: &mut F) -> Result<UsersDb, UsersLoadError>
where
    F: FilesystemRead + FilesystemSecurity + ?Sized,
{
    // A minimal VFS whose root mount is backed by the caller's driver —
    // the shape of the real root volume, which carries the whole §16
    // tree from its own root directory.
    let mut vfs = Vfs::new(Metadata::new(UserId(0), GroupId(0), Mode::from_bits(0o755)));
    let handle = DriverHandle::from_raw(ROOT_VOLUME_HANDLE).map_err(|_| VfsError::Io)?;
    vfs.mounts_mut().back_root(handle)?;

    let caps = CapabilitySet::empty();
    let cred = Credentials {
        uid: UserId(0),
        gid: GroupId(0),
        supplementary_gids: &[],
        caps: &caps,
    };
    let path = Path::parse(USERS_DB_PATH)?;

    // Bound the file against the format's own maximum before reading a
    // single byte (`AGENTS.md` §5.4.3).
    let info = vfs.stat_via_secured(&cred, &path, fs)?;
    if info.kind != NodeKind::RegularFile {
        return Err(UsersLoadError::NotAFile);
    }
    if info.size > MAX_DB_LEN as u64 {
        return Err(UsersLoadError::TooLarge);
    }
    let size = usize::try_from(info.size).map_err(|_| UsersLoadError::TooLarge)?;

    let mut buf = vec![0u8; size];
    let read = vfs.read_via_secured(&cred, &path, fs, 0, &mut buf)?;
    let result = if read == size {
        match core::str::from_utf8(&buf) {
            Ok(text) => UsersDb::parse(text).map_err(UsersLoadError::Parse),
            Err(_) => Err(UsersLoadError::NotUtf8),
        }
    } else {
        Err(UsersLoadError::ShortRead)
    };
    // The buffer held credential records; zero it before release
    // (`AGENTS.md` §4).
    buf.fill(0);
    result
}

#[cfg(test)]
#[path = "users_tests.rs"]
mod tests;
