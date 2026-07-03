//! The RustOS user-account database.
//!
//! `rustos-users` owns the single definition of a user account and of the
//! versioned text format persisted at `/System/Security/Users`: the
//! installer and the image builder (`tools/mkimage`)
//! *author* it, and the login path (`userland/session/login`) *reads* it —
//! one format, defined once.
//!
//! * [`UserRecord`] — one account: username, [`Uid`], primary [`Gid`],
//!   supplementary groups, display name, home directory, the user's shell
//!   of choice, the capability grant ceiling, the
//!   [`AccountState`], and the stored [`PasswordRecord`].
//! * [`UsersDb`] — the whole database: fail-closed [`UsersDb::parse`],
//!   exact-round-trip [`UsersDb::serialise`], and the timing-equalised
//!   [`UsersDb::authenticate`].
//! * [`PasswordRecord`] — the salted PBKDF2-HMAC-SHA256 password hash
//!   (`lib/crypto`), verified in constant time with
//!   respect to the stored hash.
//!
//! The database text is untrusted input: every
//! bound and field shape is validated and the first defect rejects the
//! whole file ([`ParseError`]). Authentication exposes exactly one refusal
//! ([`AuthError::InvalidCredentials`]) whether the account is unknown,
//! locked, or the password is wrong, so the interface cannot be used to
//! probe for valid usernames.

#![no_std]
#![deny(missing_docs)]

extern crate alloc;

mod db;
mod grants;
mod groups;
mod password;
mod record;

pub use db::{UsersDb, FORMAT_HEADER, MAX_DB_LEN, MAX_LINE_LEN, MAX_USERS};
pub use grants::{administrator_ceiling, session_baseline, ADMINISTRATIVE_SET, SESSION_BASELINE};
pub use groups::{
    GroupRecord, GroupsDb, GROUPS_FORMAT_HEADER, MAX_GROUPNAME_LEN, MAX_GROUPS, MAX_GROUPS_DB_LEN,
    MAX_GROUP_LINE_LEN,
};
pub use password::{
    PasswordRecord, Salt, DEFAULT_ITERATIONS, MAX_ITERATIONS, MAX_PASSWORD_LEN, MIN_ITERATIONS,
    PASSWORD_SCHEME, SALT_LEN,
};
pub use record::{
    AccountState, Gid, Identity, Uid, UserRecord, MAX_DISPLAY_NAME_LEN, MAX_PATH_LEN,
    MAX_SUPPLEMENTARY_GIDS, MAX_USERNAME_LEN,
};

use core::fmt;

/// Why a database text, record line, or field was refused.
///
/// Every variant is a fail-closed rejection: the
/// parser returns it and the caller holds no database, rather than a
/// partially-applied or guessed-at one.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ParseError {
    /// The text exceeds [`MAX_DB_LEN`].
    TooLong,
    /// A line exceeds [`MAX_LINE_LEN`].
    LineTooLong,
    /// The first line is not exactly [`FORMAT_HEADER`].
    Header,
    /// A record line has the wrong number of `:`-separated fields.
    FieldCount,
    /// A username violates its length or charset rules.
    Username,
    /// A uid field is not a canonically spelled `u32`.
    UserId,
    /// A gid field is not a canonically spelled `u32`.
    GroupId,
    /// The supplementary-gid list is too long or carries a duplicate.
    SupplementaryGids,
    /// A display name violates its length or charset rules.
    DisplayName,
    /// A home or shell path is not a bounded absolute path.
    Path,
    /// A capability grant names no `abi-v1` capability, or repeats one.
    Capability,
    /// An account state is neither `active` nor `locked`.
    AccountState,
    /// A stored password record violates its scheme, cost, or encoding.
    PasswordRecord,
    /// Two records share a username.
    DuplicateUsername,
    /// Two records share a uid.
    DuplicateUserId,
    /// The database exceeds [`MAX_USERS`] records.
    TooManyUsers,
    /// A group record carries an invalid group name.
    GroupName,
    /// Two group records share a name.
    DuplicateGroupName,
    /// Two group records share a gid.
    DuplicateGroupId,
    /// The group database exceeds [`MAX_GROUPS`] records.
    TooManyGroups,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::TooLong => "users database exceeds the maximum length",
            Self::LineTooLong => "users database line exceeds the maximum length",
            Self::Header => "users database does not begin with the format header",
            Self::FieldCount => "user record has the wrong number of fields",
            Self::Username => "user record carries an invalid username",
            Self::UserId => "user record carries an invalid uid",
            Self::GroupId => "user record carries an invalid gid",
            Self::SupplementaryGids => "user record carries an invalid supplementary-gid list",
            Self::DisplayName => "user record carries an invalid display name",
            Self::Path => "user record carries an invalid home or shell path",
            Self::Capability => "user record carries an invalid capability grant",
            Self::AccountState => "user record carries an unknown account state",
            Self::PasswordRecord => "user record carries an invalid password record",
            Self::DuplicateUsername => "users database repeats a username",
            Self::DuplicateUserId => "users database repeats a uid",
            Self::TooManyUsers => "users database exceeds the record budget",
            Self::GroupName => "group record carries an invalid group name",
            Self::DuplicateGroupName => "groups database repeats a group name",
            Self::DuplicateGroupId => "groups database repeats a gid",
            Self::TooManyGroups => "groups database exceeds the record budget",
        };
        f.write_str(message)
    }
}

/// Why an authentication attempt was refused.
///
/// Deliberately a single variant: the caller (and therefore the person at
/// the prompt) learns only that the pair was rejected, never which part
/// (no information leak).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum AuthError {
    /// The username/password pair was rejected.
    InvalidCredentials,
}

impl fmt::Display for AuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("invalid credentials")
    }
}

#[cfg(test)]
mod tests {
    use super::{AuthError, ParseError};

    extern crate std;
    use std::string::ToString;

    #[test]
    fn error_displays_are_stable() {
        assert_eq!(
            ParseError::Header.to_string(),
            "users database does not begin with the format header"
        );
        assert_eq!(
            AuthError::InvalidCredentials.to_string(),
            "invalid credentials"
        );
    }
}
