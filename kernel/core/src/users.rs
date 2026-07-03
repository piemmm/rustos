//! Boot-time load of the `/System/Security/Users` database off the
//! mounted root volume (`plans/PI.md` P11).
//!
//! [`load_users_db`] is the kernel's root-volume read path for the user
//! database: given the live [`FilesystemRead`] +
//! [`FilesystemSecurity`] driver of the mounted root volume (rustfs on a
//! real installation), it resolves [`USERS_DB_PATH`] through the VFS's
//! -checked per-inode delegation ([`crate::fs::Vfs::read_via_secured`]), bounds
//! the file against [`rustos_users::MAX_DB_LEN`] *before* reading it,
//! and parses the bytes through the fail-closed
//! [`rustos_users::UsersDb`] parser. The parsed database is what the
//! login path holds to authenticate sessions.
//!
//! Every outcome is audited with a stable event id
//! ([`AuditEvent::UsersDbLoaded`] / [`AuditEvent::UsersDbRejected`]), and every failure yields **no** database — a
//! system whose user database cannot be read refuses every login rather
//! than inventing accounts (fail closed).
//!
//! # Credentials of the boot read
//!
//! The read runs under the kernel's bootstrap identity — `uid 0`,
//! `gid 0`, **no** capabilities. `uid 0` carries no ambient power: the read succeeds because the database's stored
//! record makes it owner-readable, not because the kernel bypasses
//! the check. A capability-gated database is therefore refused — the
//! installer authors the record, and gating the file against the
//! boot reader is a configuration defect surfaced loudly, never silently
//! bypassed.

use alloc::vec::Vec;

use core::ops::Deref;
use core::sync::atomic::{AtomicBool, Ordering};

use rustos_abi::driver::filesystem::{FilesystemRead, FilesystemSecurity};
use rustos_abi::Errno;
use rustos_log::{Field, Level, Sink};
use rustos_sync::RwLock;
use rustos_users::{ParseError, UsersDb, MAX_DB_LEN};
use rustos_util::fmt::format_usize;

use crate::audit::{emit, AuditEvent};
use crate::fs::{read_bootstrap_file, BootstrapReadError, VfsError};

/// Absolute path of the user database on the root volume.
pub const USERS_DB_PATH: &str = "/System/Security/Users";

/// The kernel-held user database the `users_db_read` syscall serves
/// (`plans/PI.md` P11).
///
/// The boot path that mounts the root volume and runs [`load_users_db`]
/// installs an implementation holding the validated `users-v1` text;
/// the syscall handler copies that exact text out to the (capability-
/// gated, `CAP_USERS_READ`) caller, which re-parses it with the same
/// fail-closed `rustos-users` parser. Serving the *text* rather than a
/// re-serialisation keeps one canonical byte representation end to end.
///
/// `Sync` because the single installed source is shared by the per-CPU
/// syscall handlers, exactly like [`crate::ConsoleWrite`].
pub trait UsersDbSource: Sync {
    /// The held database's exact `users-v1` text, as an owned,
    /// zero-on-drop snapshot.
    ///
    /// A snapshot rather than a borrow, because the held database is
    /// replaceable at runtime through the audited `CAP_USER_ADMIN` admin
    /// path (`plans/CAPABILITY_USE.md` CU4): a borrow could outlive the
    /// text it points at across a replacement. The copy is cheap on this
    /// low-volume path (once per login / admin call) and is zeroed on
    /// drop, so no credential bytes outlive their use.
    ///
    /// # Errors
    ///
    /// [`Errno::NotFound`] when no database is held — the root volume
    /// is not mounted, or the boot read refused the record — so a
    /// system without accounts refuses every login rather than
    /// inventing one. The default
    /// [`NullUsersDbSource`] returns [`Errno::NotImplemented`] to mark
    /// an inert interface.
    fn text(&self) -> Result<UsersDbText, Errno>;

    /// Whether the database is still in its *pending* state: a real
    /// holder is being unlocked but has not yet been published or given
    /// up on, so [`text`](Self::text) returns the live-but-not-ready
    /// [`Errno::WouldBlock`] signal (only [`LateUsersDb`] is ever
    /// pending).
    ///
    /// This is the condition the `users_db_wait` syscall parks on: a
    /// caller blocks while it is `true` and is released the instant a
    /// terminal unlock outcome flips it `false` (park,
    /// never busy-poll). The default decodes it from [`text`](Self::text)
    /// so an inert source ([`NullUsersDbSource`], [`HeldUsersDbSource`])
    /// is never pending without overriding anything.
    fn is_pending(&self) -> bool {
        matches!(self.text(), Err(Errno::WouldBlock))
    }
}

/// The users-database source installed before any real holder exists.
///
/// Every read fails closed with [`Errno::NotImplemented`] — a kernel
/// build with no users-database service wired never fabricates
/// accounts.
#[derive(Debug, Default, Copy, Clone)]
pub struct NullUsersDbSource;

impl UsersDbSource for NullUsersDbSource {
    fn text(&self) -> Result<UsersDbText, Errno> {
        Err(Errno::NotImplemented)
    }
}

/// An owned copy of the `users-v1` database text, zeroed on drop.
///
/// The database carries salted credential records, so every copy handed
/// out of a [`UsersDbSource`] scrubs itself when released — a consumer
/// cannot leak the bytes by forgetting to.
pub struct UsersDbText(Vec<u8>);

impl UsersDbText {
    /// Wrap `text` in a zero-on-drop snapshot.
    ///
    /// Public so a [`UsersDbSource`] outside this crate (a test double, a
    /// future alternate holder) can serve its text under the same
    /// scrub-on-release guarantee; wrapping bytes grants no authority.
    #[must_use]
    pub fn new(text: Vec<u8>) -> Self {
        Self(text)
    }
}

impl Deref for UsersDbText {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        &self.0
    }
}

impl PartialEq for UsersDbText {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl Eq for UsersDbText {}

impl PartialEq<[u8]> for UsersDbText {
    fn eq(&self, other: &[u8]) -> bool {
        self.0 == other
    }
}

impl PartialEq<&[u8]> for UsersDbText {
    fn eq(&self, other: &&[u8]) -> bool {
        self.0 == *other
    }
}

impl Drop for UsersDbText {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}

impl core::fmt::Debug for UsersDbText {
    /// Redacted: only the length is printed, which carries no secret.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("UsersDbText")
            .field("len", &self.0.len())
            .finish_non_exhaustive()
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
/// end to end, exactly as [`UsersDbSource::text`]
/// requires.
///
/// The held bytes are the salted credential records of the user database;
/// they are zeroed when the source is dropped (zero-on-free for credential-bearing memory). A production boot
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
    /// Crate-private: the only constructors are the audited
    /// [`load_users_db_source`] read and the `CAP_USER_ADMIN` admin
    /// engine's re-serialisation of an already-validated [`UsersDb`], so
    /// a holder cannot exist without the bytes having passed the
    /// fail-closed `users-v1` validation.
    pub(crate) fn new(text: Vec<u8>) -> Self {
        Self { text }
    }
}

impl UsersDbSource for HeldUsersDbSource {
    fn text(&self) -> Result<UsersDbText, Errno> {
        // A holder only exists for a successfully loaded database, so the
        // text is always present — unlike [`NullUsersDbSource`], which
        // marks the inert "no database" interface.
        Ok(UsersDbText(self.text.clone()))
    }
}

impl core::fmt::Debug for HeldUsersDbSource {
    /// Redacted: the held bytes are salted credential records and must
    /// never reach a log or panic message. Only
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
        // bytes before the backing allocation is released. Filling with zero keeps the buffer valid for the `Vec`'s
        // own deallocation.
        self.text.fill(0);
    }
}

/// A database install was refused because one is already installed.
///
/// Returned by [`LateUsersDb::install`] when a database has already been
/// published into the cell. The cell is immutable after the first
/// successful install, so the live credential database cannot be replaced
/// by any later code path.
#[derive(Debug, Default, Copy, Clone, Eq, PartialEq)]
pub struct UsersDbAlreadyInstalled;

/// A set-once [`UsersDbSource`] the boot path installs the mounted root
/// volume's database into after it has unlocked and read it (option A,
/// in-kernel unlock — `plans/PI.md` P11 Chunk B-2).
///
/// The encrypted root is unlocked only *after* the console keyboard is
/// live (the operator types the passphrase), which is past the point
/// where [`BootInfo::with_users_db`](crate::BootInfo::with_users_db) is
/// consumed. The dispatch hook therefore holds a `&'static LateUsersDb`
/// from boot and reads [`text`](UsersDbSource::text) on every
/// `users_db_read`; the trusted unlock step publishes the loaded database
/// into the same cell once it exists, and the next `users_db_read` serves
/// it.
///
/// # Three states, two errors
///
/// The cell distinguishes *"the unlock has not finished yet"* from *"the
/// unlock finished and there is no database"*, because `login` must treat
/// them differently. Under design B (`plans/PI.md` P11) `login` is spawned
/// **before** the in-kernel unlock kthread mounts the root and prompts for
/// the passphrase on the same console; if `login` prompted `Username:`
/// straight away it would draw over the `Root passphrase:` prompt and the
/// two would compete for the one keyboard. So:
///
/// * **Pending** (not installed, not [`resolve`](Self::resolve)d):
///   [`text`](UsersDbSource::text) returns [`Errno::WouldBlock`] — the
///   live-but-not-ready signal. `login` waits (yielding) and does **not**
///   prompt, so the unlock owns the console until it finishes.
/// * **Installed** ([`install`](Self::install) succeeded): the held text
///   is served; `login` authenticates against it.
/// * **Resolved-empty** ([`resolve`](Self::resolve)d with nothing
///   installed — the unlock gave up, or there was no root to unlock):
///   [`text`](UsersDbSource::text) returns [`Errno::NotImplemented`],
///   identical to [`NullUsersDbSource`]. `login` then runs its fail-closed
///   deny-all prompt — an installer image stays
///   usable.
///
/// Every boot path that hands `&LATE_USERS_DB` to
/// [`with_users_db`](crate::BootInfo::with_users_db) reaches exactly one
/// terminal step — [`install`](Self::install) on a successful unlock, or
/// [`resolve`](Self::resolve) on every fail-closed / no-disk path (paired
/// with releasing the console to `login`) — so a `login` parked on the
/// pending state always makes progress and never waits forever.
///
/// Security properties:
///
/// * **Fail closed by default.** Until [`install`](Self::install) or
///   [`resolve`](Self::resolve), a read returns [`Errno::WouldBlock`]
///   (wait, do not authenticate); once resolved without a database it
///   returns [`Errno::NotImplemented`], identical to [`NullUsersDbSource`].
///   Neither path ever invents an account.
/// * **Install is set-once; replacement is a separate, audited kernel
///   path.** [`install`](Self::install) publishes the boot database and
///   every later call is refused. The only way the held database changes
///   afterwards is [`replace`](Self::replace), which is called
///   exclusively by the `CAP_USER_ADMIN` admin engine
///   (`plans/CAPABILITY_USE.md` CU4) after it has validated, verified,
///   and persisted the edited database — and which refuses to run before
///   the boot install has happened, so it can never *create* the first
///   database.
/// * **No user-reachable surface.** [`install`](Self::install),
///   [`resolve`](Self::resolve), and [`replace`](Self::replace) are
///   internal kernel code (the unlock step and the audited admin
///   engine); none is exposed as a syscall, so they add no attack
///   surface to the ABI.
///
/// `Sync` (through the lock and the resolved flag) so the single
/// `&'static` instance is shared by the per-CPU syscall handlers, exactly
/// like [`NullUsersDbSource`].
pub struct LateUsersDb {
    held: RwLock<Option<HeldUsersDbSource>>,
    /// Set once the unlock reaches a terminal outcome with **no** database
    /// installed (gave up, or there was no root). It turns a pending read
    /// ([`Errno::WouldBlock`]) into the inert
    /// [`Errno::NotImplemented`] so `login` stops waiting and runs its
    /// fail-closed deny-all prompt. A successful unlock installs into
    /// `held` instead, which wins in [`text`](UsersDbSource::text)
    /// regardless of this flag.
    resolved: AtomicBool,
}

impl LateUsersDb {
    /// Construct an empty cell. `const` so a boot path can place it in a
    /// `static` and hand `&LATE_USERS_DB` to
    /// [`BootInfo::with_users_db`](crate::BootInfo::with_users_db).
    #[must_use]
    pub const fn new() -> Self {
        Self {
            held: RwLock::new(None),
            resolved: AtomicBool::new(false),
        }
    }

    /// Publish the mounted database exactly once.
    ///
    /// On success the cell serves `source`'s text for the rest of the
    /// kernel's lifetime. The rejected `source` of a refused install is
    /// dropped here (zeroing its credential bytes) rather
    /// than handed back, so a caller cannot leak it.
    ///
    /// # Errors
    ///
    /// [`UsersDbAlreadyInstalled`] if a database is already installed —
    /// the cell is immutable after the first successful install.
    pub fn install(&self, source: HeldUsersDbSource) -> Result<(), UsersDbAlreadyInstalled> {
        {
            let mut held = self.held.write();
            if held.is_some() {
                // The rejected `source` is dropped here, zeroing its
                // duplicate credential bytes, instead of being surfaced.
                return Err(UsersDbAlreadyInstalled);
            }
            *held = Some(source);
        }
        // Release any task parked in `users_db_wait`: the database left its
        // pending state, so a blocked `login` re-reads and authenticates
        // (the wake that closes the park). A no-op
        // before the wait-queue arch hook is installed (host tests), so it
        // is always safe to call here.
        crate::waitq::users_db_wake();
        Ok(())
    }

    /// Replace the installed database with an edited one
    /// (`plans/CAPABILITY_USE.md` CU4).
    ///
    /// Called exclusively by the `CAP_USER_ADMIN` admin engine after it
    /// has validated the edit, re-verified the identity table, and
    /// persisted the new text to the root volume — replacement is never a
    /// user-reachable install path. The displaced holder is dropped here,
    /// zeroing its credential bytes. A change binds at the next
    /// `users_db_read` (the next login); running sessions keep the
    /// credentials they authenticated with.
    ///
    /// # Errors
    ///
    /// [`Errno::NotImplemented`] when no database has been installed yet:
    /// the boot unlock is the only path that may publish the *first*
    /// database, so a replacement can never create one (fail closed).
    pub fn replace(&self, source: HeldUsersDbSource) -> Result<(), Errno> {
        let mut held = self.held.write();
        if held.is_none() {
            return Err(Errno::NotImplemented);
        }
        *held = Some(source);
        Ok(())
    }

    /// Whether a database has been installed.
    #[must_use]
    pub fn is_installed(&self) -> bool {
        self.held.read().is_some()
    }

    /// Mark the unlock terminated with no database to serve.
    ///
    /// Called on every fail-closed / no-disk unlock outcome (the unlock
    /// gave up, or there was no root block device to unlock), paired with
    /// releasing the console to `login`. It flips a pending read
    /// ([`Errno::WouldBlock`]) into the inert [`Errno::NotImplemented`] so
    /// `login` stops waiting and runs its fail-closed deny-all prompt. Idempotent, and harmless after a successful
    /// [`install`](Self::install): an installed database is served from
    /// `held` regardless of this flag.
    pub fn resolve(&self) {
        self.resolved.store(true, Ordering::Release);
        // Release any task parked in `users_db_wait`: the unlock gave up
        // with no database, so a blocked `login` re-reads, sees the inert
        // `NotImplemented`, and runs its fail-closed deny-all prompt. A no-op before the wait-queue arch hook is
        // installed (host tests).
        crate::waitq::users_db_wake();
    }

    /// Whether the unlock has reached a terminal outcome — either a
    /// database was [`install`](Self::install)ed or the cell was
    /// [`resolve`](Self::resolve)d with none.
    #[must_use]
    pub fn is_resolved(&self) -> bool {
        self.is_installed() || self.resolved.load(Ordering::Acquire)
    }
}

impl Default for LateUsersDb {
    fn default() -> Self {
        Self::new()
    }
}

impl UsersDbSource for LateUsersDb {
    fn text(&self) -> Result<UsersDbText, Errno> {
        // A published database always serves its bytes. Otherwise the
        // result depends on whether the unlock has *finished*: while it is
        // still pending return [`Errno::WouldBlock`] so `login` waits
        // without prompting (the unlock owns the console); once it has
        // resolved with no database, fail closed with
        // [`Errno::NotImplemented`] exactly as [`NullUsersDbSource`] does so `login` runs its deny-all prompt.
        match &*self.held.read() {
            Some(held) => held.text(),
            None if self.resolved.load(Ordering::Acquire) => Err(Errno::NotImplemented),
            None => Err(Errno::WouldBlock),
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
    /// is read.
    TooLarge,
    /// The driver returned fewer bytes than the file's reported size; a
    /// truncated database is never parsed.
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

impl From<BootstrapReadError> for UsersLoadError {
    fn from(err: BootstrapReadError) -> Self {
        match err {
            BootstrapReadError::Vfs(err) => Self::Vfs(err),
            BootstrapReadError::NotAFile => Self::NotAFile,
            BootstrapReadError::TooLarge => Self::TooLarge,
            BootstrapReadError::ShortRead => Self::ShortRead,
        }
    }
}

/// Read and parse `/System/Security/Users` from the mounted root
/// volume's filesystem driver.
///
/// On success the [`AuditEvent::UsersDbLoaded`] record carries the
/// account count; on failure the [`AuditEvent::UsersDbRejected`] record
/// carries the [`cause`](UsersLoadError::cause) and no database exists. The read buffer is zeroed before release — the
/// database carries credential records.
///
/// Returns the parsed [`UsersDb`]. A boot path that must *serve* the
/// database to the `users_db_read` syscall holds the canonical text
/// instead — see [`load_users_db_source`], which shares this read and
/// audit path.
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
        // after the parse — it held credential records.
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
/// Shares the-checked read, the fail-closed parse, and the audit
/// records with [`load_users_db`]. On success the
/// returned holder owns the exact `users-v1` bytes (zeroed when dropped); the boot path `Box::leak`s it and installs it through
/// `KernelSyscallHandlers::with_users_db`. On any refusal **no** holder
/// is produced and the read buffer is zeroed, so a system whose database
/// cannot be read serves none rather than inventing accounts.
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
        // A refused parse retains nothing; zero the buffer before release.
        Err(err) => {
            buf.fill(0);
            audit_load(audit, None, Some(err));
            Err(err)
        }
    }
}

/// Emit the single shared load outcome record: [`AuditEvent::UsersDbLoaded`]
/// with the account count on success, else [`AuditEvent::UsersDbRejected`]
/// with the refusal cause. Exactly one of `records`
/// (loaded) or `err` (rejected) is `Some`; a `records` of `Some` wins so
/// the two read entry points emit byte-identical records.
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
                    value: rustos_log::FieldValue::Str(USERS_DB_PATH),
                },
                Field {
                    key: "records",
                    value: rustos_log::FieldValue::Str(records),
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
                    value: rustos_log::FieldValue::Str(USERS_DB_PATH),
                },
                Field {
                    key: "cause",
                    value: rustos_log::FieldValue::Str(err.cause()),
                },
            ],
        );
    }
}

/// Read the exact-size, fully-read bytes of `/System/Security/Users` off
/// the mounted root volume under the kernel's capability-less `uid 0`
/// bootstrap identity, applying the permission check and the
/// size bound *before* a single byte is read.
///
/// The returned buffer carries credential records; the caller either zeros
/// it after use ([`load_users_db`]) or hands it to a holder that owns and
/// zeroes it ([`load_users_db_source`]). The bytes are not parsed here —
/// [`parse_users_text`] is the shared validation step.
fn read_users_bytes<F>(fs: &mut F) -> Result<Vec<u8>, UsersLoadError>
where
    F: FilesystemRead + FilesystemSecurity + ?Sized,
{
    // The bounded, fail-closed `uid 0` read shared with the group registry
    // reader; the database is bounded by the `users-v1` format maximum.
    Ok(read_bootstrap_file(fs, USERS_DB_PATH, MAX_DB_LEN)?)
}

/// Validate `buf` as UTF-8 and parse it with the fail-closed `users-v1`
/// parser. The single validation step shared by [`load_users_db`] and
/// [`load_users_db_source`].
fn parse_users_text(buf: &[u8]) -> Result<UsersDb, UsersLoadError> {
    match core::str::from_utf8(buf) {
        Ok(text) => UsersDb::parse(text).map_err(UsersLoadError::Parse),
        Err(_) => Err(UsersLoadError::NotUtf8),
    }
}

#[cfg(test)]
#[path = "users_tests.rs"]
mod tests;
