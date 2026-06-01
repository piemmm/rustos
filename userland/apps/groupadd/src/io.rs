//! The seams through which `groupadd` touches the outside world, and the
//! record they carry.
//!
//! Keeping the group database and the terminal behind object-safe traits is
//! what lets the group-creation logic in [`crate::client`] run against
//! in-memory fixtures with no kernel, mirroring the seam design of the other
//! userland crates (`useradd`'s `UserDb`, `init`'s `Spawner`/`Reaper`,
//! `login`'s `Authenticator`, `setcap`'s `FileSystem`).

use rustos_abi::Errno;

/// The fully-parsed group record handed to [`GroupDb::create`].
///
/// Both fields are borrowed from the parsed [`Command`](crate::Command), so the
/// record allocates nothing of its own. A `None` `gid` asks the database to
/// allocate the next free one; `groupadd` never guesses one (`AGENTS.md`
/// §2.1).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GroupSpec<'a> {
    /// The group name. Already validated to match `[a-z_][a-z0-9_-]*` within
    /// the length bound (see [`validate_name`](crate::validate_name)).
    pub name: &'a str,
    /// The requested numeric group id, or [`None`] to let the database
    /// allocate the next free one.
    pub gid: Option<u32>,
}

/// Reads and writes the group database that persists under
/// `/System/Security/Groups` (`AGENTS.md` §5.1).
///
/// `groupadd` first asks [`name_in_use`](GroupDb::name_in_use) so it can
/// report a precise "already exists" before attempting a write, then calls
/// [`create`](GroupDb::create). The database — not this tool — is the policy
/// point (`AGENTS.md` §5.4): it enforces `CAP_USER_ADMIN` and gid uniqueness,
/// and returns the matching [`Errno`] on refusal.
pub trait GroupDb {
    /// Return whether a group record with name `name` already exists.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the database raises — e.g. [`Errno::PermissionDenied`]
    /// when the caller may not read the group database.
    fn name_in_use(&self, name: &str) -> Result<bool, Errno>;

    /// Create the group described by `spec`.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the database raises — e.g. [`Errno::PermissionDenied`]
    /// when the caller lacks `CAP_USER_ADMIN`, or [`Errno::OutOfRange`] when
    /// the requested gid is already taken.
    fn create(&self, spec: &GroupSpec<'_>) -> Result<(), Errno>;
}

/// Writes rendered bytes to the terminal.
///
/// `groupadd` is silent on success; this seam carries only the usage banner.
pub trait Output {
    /// Write every byte of `bytes` to the terminal.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the console raises (e.g. a closed terminal).
    fn write_all(&self, bytes: &[u8]) -> Result<(), Errno>;
}
