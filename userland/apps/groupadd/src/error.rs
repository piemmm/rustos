//! The outcomes of running a `groupadd` command.

use core::fmt;
use rustos_abi::Errno;

/// Why a `groupadd` invocation did not complete.
///
/// The variants are deliberately coarse: the CLI surfaces enough to print a
/// useful diagnostic and set a process exit status, while leaning on the
/// frozen [`Errno`] for the wire-level cause so it invents no parallel error
/// set.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GroupaddError {
    /// The command line carried an unrecognised option, a missing option
    /// value, or did not name exactly one group. The caller should print
    /// [`crate::USAGE`]. Nothing is created.
    Usage,
    /// The group name is not a valid `[a-z_][a-z0-9_-]*` within the length
    /// bound ([`MAX_NAME_LEN`](crate::MAX_NAME_LEN)). Nothing is created.
    BadName,
    /// A `-g` value was not a decimal id (empty, non-digit, or overflowing a
    /// [`u32`]). Nothing is created.
    BadId,
    /// The group name is already present in the database. Nothing is created.
    Exists,
    /// Consulting the database for the name failed. Carries the underlying
    /// [`Errno`] — e.g. [`Errno::PermissionDenied`] when the caller may not
    /// read the group database.
    Lookup(Errno),
    /// Creating the group failed. Carries the underlying [`Errno`] — e.g.
    /// [`Errno::PermissionDenied`] when the caller lacks `CAP_USER_ADMIN`, or
    /// [`Errno::OutOfRange`] when the requested gid is already taken.
    Create(Errno),
    /// Writing the usage banner to the terminal failed. Carries the underlying
    /// [`Errno`].
    Output(Errno),
}

impl fmt::Display for GroupaddError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Usage => f.write_str("invalid usage"),
            Self::BadName => f.write_str("invalid group name"),
            Self::BadId => f.write_str("invalid numeric id"),
            Self::Exists => f.write_str("group already exists"),
            Self::Lookup(errno) => write!(f, "cannot read group database: {errno}"),
            Self::Create(errno) => write!(f, "cannot create group: {errno}"),
            Self::Output(errno) => write!(f, "terminal write failed: {errno}"),
        }
    }
}
