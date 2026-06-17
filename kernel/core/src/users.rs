//! Boot-time load of the `/System/Security/Users` database off the
//! mounted root volume (`AGENTS.md` §5.1, `plans/PI.md` P11).
//!
//! [`load_users_db`] is the kernel's root-volume read path for the user
//! database: given the live [`FilesystemRead`] +
//! [`FilesystemSecurity`] driver of the mounted root volume (rustfs on a
//! real installation), it resolves [`USERS_DB_PATH`] through the VFS's
//! §5.3-checked per-inode delegation ([`crate::fs::Vfs::read_via_secured`]), bounds
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
use alloc::vec::Vec;

use rustos_abi::driver::filesystem::{FilesystemRead, FilesystemSecurity, NodeKind};
use rustos_abi::Errno;
use rustos_caps::CapabilitySet;
use rustos_kernel_sec::{GroupId, UserId};
use rustos_log::{Field, Level, Sink};
use rustos_sync::OnceCell;
use rustos_users::{ParseError, UsersDb, MAX_DB_LEN};
use rustos_util::fmt::format_usize;

use crate::audit::{emit, AuditEvent};
use crate::fs::{Credentials, Path, VfsError};

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

/// A [`UsersDbSource`] that owns the validated `users-v1` text read off
/// the mounted root volume (`plans/PI.md` P11).
///
/// Built by [`load_users_db_source`] on a successful boot read, then
/// installed into the dispatch hook through
/// `KernelSyscallHandlers::with_users_db` so the `users_db_read` syscall
/// serves its bytes to a `CAP_USERS_READ` caller. Holding the *text*
/// rather than a re-serialisation keeps one canonical byte representation
/// end to end (`AGENTS.md` §2.2), exactly as [`UsersDbSource::text`]
/// requires.
///
/// The held bytes are the salted credential records of the user database;
/// they are zeroed when the source is dropped (`AGENTS.md` §4 —
/// zero-on-free for credential-bearing memory). A production boot
/// `Box::leak`s the holder so it lives for the kernel's lifetime and the
/// `Drop` never fires; the guarantee still holds for any non-leaked use
/// (tests, a future re-load path).
pub struct HeldUsersDbSource {
    /// Validated `users-v1` text, exactly as read off the root volume.
    text: Vec<u8>,
}

impl HeldUsersDbSource {
    /// Wrap already-validated `users-v1` `text`.
    ///
    /// Private to this module: the only constructor is the audited
    /// [`load_users_db_source`] read, so a holder cannot exist without
    /// the bytes having passed the §5.3 permission check and the
    /// fail-closed `users-v1` parse.
    fn new(text: Vec<u8>) -> Self {
        Self { text }
    }
}

impl UsersDbSource for HeldUsersDbSource {
    fn text(&self) -> Result<&[u8], Errno> {
        // A holder only exists for a successfully loaded database, so the
        // text is always present — unlike [`NullUsersDbSource`], which
        // marks the inert "no database" interface.
        Ok(&self.text)
    }
}

impl core::fmt::Debug for HeldUsersDbSource {
    /// Redacted: the held bytes are salted credential records and must
    /// never reach a log or panic message (`AGENTS.md` §4 / §19.4). Only
    /// the length is printed, which carries no secret.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("HeldUsersDbSource")
            .field("len", &self.text.len())
            .finish_non_exhaustive()
    }
}

impl Drop for HeldUsersDbSource {
    fn drop(&mut self) {
        // The database carries salted credential records; zero the held
        // bytes before the backing allocation is released (`AGENTS.md`
        // §4). Filling with zero keeps the buffer valid for the `Vec`'s
        // own deallocation.
        self.text.fill(0);
    }
}

/// A database install was refused because one is already installed.
///
/// Returned by [`LateUsersDb::install`] when a database has already been
/// published into the cell. The cell is immutable after the first
/// successful install, so the live credential database cannot be replaced
/// by any later code path (`AGENTS.md` §5.4).
#[derive(Debug, Default, Copy, Clone, Eq, PartialEq)]
pub struct UsersDbAlreadyInstalled;

/// A set-once [`UsersDbSource`] the boot path installs the mounted root
/// volume's database into after it has unlocked and read it (option A,
/// in-kernel unlock — `plans/PI.md` P11 Chunk B-2).
///
/// The encrypted root is unlocked only *after* the console keyboard is
/// live (the operator types the passphrase, §11), which is past the point
/// where [`BootInfo::with_users_db`](crate::BootInfo::with_users_db) is
/// consumed. The dispatch hook therefore holds a `&'static LateUsersDb`
/// from boot and reads [`text`](UsersDbSource::text) on every
/// `users_db_read`; the trusted unlock step publishes the loaded database
/// into the same cell once it exists, and the next `users_db_read` serves
/// it.
///
/// Security properties (`AGENTS.md` §5.4):
///
/// * **Fail closed by default.** Until [`install`](Self::install)
///   succeeds the cell is empty and every read returns
///   [`Errno::NotImplemented`], identical to [`NullUsersDbSource`] — a
///   build that never mounts a root, or one whose unlock fails, refuses
///   every login rather than inventing accounts (`AGENTS.md` §5.4.5).
/// * **Immutable after install.** [`install`](Self::install) is
///   set-once: the first call publishes the database and every later
///   call is refused, so no code path that runs after the trusted boot
///   unlock can swap the live credential database.
/// * **No user-reachable surface.** [`install`](Self::install) is
///   internal kernel code the unlock step calls; it is never exposed as a
///   syscall, so it adds no attack surface to the ABI.
///
/// `Sync` (through [`OnceCell`]) so the single `&'static` instance is
/// shared by the per-CPU syscall handlers, exactly like
/// [`NullUsersDbSource`].
pub struct LateUsersDb {
    held: OnceCell<HeldUsersDbSource>,
}

impl LateUsersDb {
    /// Construct an empty cell. `const` so a boot path can place it in a
    /// `static` and hand `&LATE_USERS_DB` to
    /// [`BootInfo::with_users_db`](crate::BootInfo::with_users_db).
    #[must_use]
    pub const fn new() -> Self {
        Self {
            held: OnceCell::new(),
        }
    }

    /// Publish the mounted database exactly once.
    ///
    /// On success the cell serves `source`'s text for the rest of the
    /// kernel's lifetime. The rejected `source` of a refused install is
    /// dropped here (zeroing its credential bytes, `AGENTS.md` §4) rather
    /// than handed back, so a caller cannot leak it.
    ///
    /// # Errors
    ///
    /// [`UsersDbAlreadyInstalled`] if a database is already installed —
    /// the cell is immutable after the first successful install
    /// (`AGENTS.md` §5.4).
    pub fn install(&self, source: HeldUsersDbSource) -> Result<(), UsersDbAlreadyInstalled> {
        // `OnceCell::set` hands the rejected value back in its error; drop
        // it (zeroing the duplicate credential bytes) instead of
        // surfacing it.
        self.held.set(source).map_err(|_| UsersDbAlreadyInstalled)
    }

    /// Whether a database has been installed.
    #[must_use]
    pub fn is_installed(&self) -> bool {
        self.held.is_initialised()
    }
}

impl Default for LateUsersDb {
    fn default() -> Self {
        Self::new()
    }
}

impl UsersDbSource for LateUsersDb {
    fn text(&self) -> Result<&[u8], Errno> {
        // Empty (not yet unlocked) or poisoned both fail closed exactly
        // as [`NullUsersDbSource`] does (`AGENTS.md` §2.9 / §5.4.5); only
        // a published database serves its bytes.
        match self.held.get() {
            Ok(Some(held)) => held.text(),
            _ => Err(Errno::NotImplemented),
        }
    }
}

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
/// (`AGENTS.md` §5.4.5). The read buffer is zeroed before release — the
/// database carries credential records (`AGENTS.md` §4).
///
/// Returns the parsed [`UsersDb`]. A boot path that must *serve* the
/// database to the `users_db_read` syscall holds the canonical text
/// instead — see [`load_users_db_source`], which shares this read and
/// audit path (`AGENTS.md` §2.2).
///
/// # Errors
///
/// The [`UsersLoadError`] naming the first check that refused.
pub fn load_users_db<F>(fs: &mut F, audit: &dyn Sink) -> Result<UsersDb, UsersLoadError>
where
    F: FilesystemRead + FilesystemSecurity + ?Sized,
{
    let parsed = match read_users_bytes(fs) {
        // This caller does not retain the bytes, so the buffer is zeroed
        // after the parse — it held credential records (`AGENTS.md` §4).
        Ok(mut buf) => {
            let parsed = parse_users_text(&buf);
            buf.fill(0);
            parsed
        }
        Err(err) => Err(err),
    };
    match &parsed {
        Ok(db) => audit_load(audit, Some(db.records().len()), None),
        Err(err) => audit_load(audit, None, Some(*err)),
    }
    parsed
}

/// Read `/System/Security/Users` and retain its validated text as a
/// [`HeldUsersDbSource`] the `users_db_read` syscall can serve
/// (`plans/PI.md` P11).
///
/// Shares the §5.3-checked read, the fail-closed parse, and the audit
/// records with [`load_users_db`] (`AGENTS.md` §2.2). On success the
/// returned holder owns the exact `users-v1` bytes (zeroed when dropped,
/// `AGENTS.md` §4); the boot path `Box::leak`s it and installs it through
/// `KernelSyscallHandlers::with_users_db`. On any refusal **no** holder
/// is produced and the read buffer is zeroed, so a system whose database
/// cannot be read serves none rather than inventing accounts (`AGENTS.md`
/// §5.4.5).
///
/// # Errors
///
/// The [`UsersLoadError`] naming the first check that refused.
pub fn load_users_db_source<F>(
    fs: &mut F,
    audit: &dyn Sink,
) -> Result<HeldUsersDbSource, UsersLoadError>
where
    F: FilesystemRead + FilesystemSecurity + ?Sized,
{
    let mut buf = match read_users_bytes(fs) {
        Ok(buf) => buf,
        Err(err) => {
            audit_load(audit, None, Some(err));
            return Err(err);
        }
    };
    match parse_users_text(&buf) {
        // The bytes validated: hand them to the holder (which owns and
        // serves them), so the text is *not* zeroed here.
        Ok(db) => {
            let records = db.records().len();
            audit_load(audit, Some(records), None);
            Ok(HeldUsersDbSource::new(buf))
        }
        // A refused parse retains nothing; zero the buffer before release
        // (`AGENTS.md` §4).
        Err(err) => {
            buf.fill(0);
            audit_load(audit, None, Some(err));
            Err(err)
        }
    }
}

/// Emit the single shared load outcome record: [`AuditEvent::UsersDbLoaded`]
/// with the account count on success, else [`AuditEvent::UsersDbRejected`]
/// with the refusal cause (`AGENTS.md` §5.4.4). Exactly one of `records`
/// (loaded) or `err` (rejected) is `Some`; a `records` of `Some` wins so
/// the two read entry points emit byte-identical records (`AGENTS.md`
/// §2.2).
fn audit_load(audit: &dyn Sink, records: Option<usize>, err: Option<UsersLoadError>) {
    if let Some(records) = records {
        let mut count_buf = [0u8; 12];
        let records = format_usize(records, &mut count_buf);
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
    } else if let Some(err) = err {
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
    }
}

/// Read the exact-size, fully-read bytes of `/System/Security/Users` off
/// the mounted root volume under the kernel's capability-less `uid 0`
/// bootstrap identity, applying the §5.3 permission check and the
/// `AGENTS.md` §5.4.3 size bound *before* a single byte is read.
///
/// The returned buffer carries credential records; the caller either zeros
/// it after use ([`load_users_db`]) or hands it to a holder that owns and
/// zeroes it ([`load_users_db_source`]). The bytes are not parsed here —
/// [`parse_users_text`] is the shared validation step (`AGENTS.md` §2.2).
fn read_users_bytes<F>(fs: &mut F) -> Result<Vec<u8>, UsersLoadError>
where
    F: FilesystemRead + FilesystemSecurity + ?Sized,
{
    // A minimal VFS whose root mount is backed by the caller's driver —
    // the shape of the real root volume, which carries the whole §16
    // tree from its own root directory (`AGENTS.md` §2.2: shared builder).
    let vfs = crate::fs::root_backed_vfs()?;

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
    if read != size {
        // A truncated database is never parsed (`AGENTS.md` §2.9); zero the
        // partial read before release (`AGENTS.md` §4).
        buf.fill(0);
        return Err(UsersLoadError::ShortRead);
    }
    Ok(buf)
}

/// Validate `buf` as UTF-8 and parse it with the fail-closed `users-v1`
/// parser. The single validation step shared by [`load_users_db`] and
/// [`load_users_db_source`] (`AGENTS.md` §2.2).
fn parse_users_text(buf: &[u8]) -> Result<UsersDb, UsersLoadError> {
    match core::str::from_utf8(buf) {
        Ok(text) => UsersDb::parse(text).map_err(UsersLoadError::Parse),
        Err(_) => Err(UsersLoadError::NotUtf8),
    }
}

#[cfg(test)]
#[path = "users_tests.rs"]
mod tests;
