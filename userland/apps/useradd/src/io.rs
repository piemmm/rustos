//! The seams through which `useradd` touches the outside world, and the
//! record they carry.
//!
//! Keeping the user database and the terminal behind object-safe traits is
//! what lets the account-creation logic in [`crate::client`] run against
//! in-memory fixtures with no kernel, mirroring the seam design of the other
//! userland crates (`init`'s `Spawner`/`Reaper`, `login`'s `Authenticator`,
//! `setcap`'s `FileSystem`).

use rustos_abi::Errno;

/// The fully-parsed account record handed to [`UserDb::create`].
///
/// Every field is borrowed from the parsed [`Command`](crate::Command), so the
/// record allocates nothing of its own. A `None` `uid` asks the database to
/// allocate one; `None` `comment`/`home` leave those fields to the database's
/// documented defaults (the `/Users/<name>` layout for the home
/// directory) — `useradd` never guesses them.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UserSpec<'a> {
    /// The login name. Already validated to match `[a-z_][a-z0-9_-]*` within
    /// the length bound (see [`validate_name`](crate::validate_name)).
    pub name: &'a str,
    /// The requested numeric user id, or [`None`] to let the database
    /// allocate the next free one.
    pub uid: Option<u32>,
    /// The primary group id. Always present — `useradd` requires `-g` rather
    /// than guessing a default group.
    pub primary_gid: u32,
    /// The supplementary group ids, in operand order. Empty when `-G` was not
    /// given.
    pub supplementary_gids: &'a [u32],
    /// The optional account comment / full name (`-c`).
    pub comment: Option<&'a str>,
    /// The optional home directory (`-d`).
    pub home: Option<&'a str>,
}

/// Reads and writes the user database that persists under
/// `/System/Security/Users`.
///
/// `useradd` first asks [`name_in_use`](UserDb::name_in_use) so it can report
/// a precise "already exists" before attempting a write, then calls
/// [`create`](UserDb::create). The database — not this tool — is the policy
/// point: it enforces `CAP_USER_ADMIN`, uid uniqueness,
/// group existence, and the supplementary-group bound, and returns the
/// matching [`Errno`] on refusal.
pub trait UserDb {
    /// Return whether a user record with login name `name` already exists.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the database raises — e.g. [`Errno::PermissionDenied`]
    /// when the caller may not read the user database.
    fn name_in_use(&self, name: &str) -> Result<bool, Errno>;

    /// Create the account described by `spec`.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the database raises — e.g. [`Errno::PermissionDenied`]
    /// when the caller lacks `CAP_USER_ADMIN`, [`Errno::NotFound`] when a
    /// referenced group does not exist, or [`Errno::LengthOutOfRange`] when
    /// the supplementary set exceeds the database's bound.
    fn create(&self, spec: &UserSpec<'_>) -> Result<(), Errno>;
}

/// Writes rendered bytes to the terminal.
///
/// `useradd` is silent on success; this seam carries only the usage banner.
pub trait Output {
    /// Write every byte of `bytes` to the terminal.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the console raises (e.g. a closed terminal).
    fn write_all(&self, bytes: &[u8]) -> Result<(), Errno>;
}
