//! The TAIRiX user-account database.
//!
//! `tairix-users` owns the single definition of a user account and of the
//! versioned text format persisted at `/System/Security/Users`: the
//! installer and the image builder (`tools/mkimage`)
//! *author* it, and the login path (`userland/session/login`) *reads* it —
//! one format, defined once.
//!
//! * [`UserRecord`] — one account: username, [`Uid`], primary [`Gid`],
//!   supplementary groups, display name, home directory, the user's shell
//!   of choice, the capability grant ceiling, the
//!   [`AccountState`], and the stored password. An interactive account
//!   carries all of home, shell, and a real [`PasswordRecord`]; a
//!   [`AccountState::NoLogin`] system/service account carries none of
//!   them — the explicit [`NO_PATH_MARKER`] / [`NO_PASSWORD_MARKER`]
//!   spellings, never a fake path or a throwaway hash — and the
//!   constructor and parser both enforce that pairing
//!   ([`ParseError::AccountShape`]).
//! * [`UsersDb`] — the whole database: fail-closed [`UsersDb::parse`],
//!   exact-round-trip [`UsersDb::serialise`], and the timing-equalised
//!   [`UsersDb::authenticate`].
//! * [`StoredPassword`] — what the password field stores: a
//!   [`PasswordRecord`] (the salted PBKDF2-HMAC-SHA256 hash via
//!   `lib/crypto`, verified in constant time with respect to the stored
//!   hash), or the typed never-authenticates marker.
//! * The compiled-in system identity ([`system_accounts`],
//!   [`system_groups`], [`system_account_uid`]): the OS-owned accounts
//!   and groups are kernel policy, defined once here, compiled into the
//!   kernel's identity table, and never written to disk — the on-disk
//!   databases hold only human accounts, and the kernel's identity merge
//!   refuses any on-disk record colliding with this set.
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
mod policy;
mod provision;
mod record;

pub use db::{UsersDb, FORMAT_HEADER, MAX_DB_LEN, MAX_LINE_LEN, MAX_USERS};
pub use grants::{
    administrator_ceiling, capability_set, session_baseline, ADMINISTRATIVE_SET, CONFD_CEILING,
    DEVMGR_CEILING, FONTD_CEILING, LOGIN_CEILING, NETSTACK_CEILING, SEATMGR_CEILING,
    SESSION_BASELINE, SYSINFOD_CEILING,
};
pub use groups::{
    GroupRecord, GroupsDb, GROUPS_FORMAT_HEADER, MAX_GROUPNAME_LEN, MAX_GROUPS, MAX_GROUPS_DB_LEN,
    MAX_GROUP_LINE_LEN, STORAGE_GID, STORAGE_GROUP,
};
pub use password::{
    PasswordRecord, Salt, StoredPassword, DEFAULT_ITERATIONS, MAX_ITERATIONS, MAX_PASSWORD_LEN,
    MIN_ITERATIONS, NO_PASSWORD_MARKER, PASSWORD_SCHEME, SALT_LEN,
};
pub use policy::{
    appdata_root_security, appdata_transit_security, default_home, next_id, IdRange, APPDATA_ROOT,
    APPDATA_ROOT_PARENTS, DEFAULT_SHELL, FIRST_USER_GID, FIRST_USER_UID, HOME_MODE, HOME_SUBDIRS,
};
pub use provision::{
    is_system_account_name, is_system_group_name, system_account_directory, system_account_uid,
    system_accounts, system_groups, CONFD_UID, CONFD_USERNAME, DEVMGR_UID, DEVMGR_USERNAME,
    FONTD_UID, FONTD_USERNAME, GREETER_UID, GREETER_USERNAME, LOGIN_UID, LOGIN_USERNAME,
    NETSTACK_UID, NETSTACK_USERNAME, SEATMGR_UID, SEATMGR_USERNAME, SERVICES_GID, SERVICES_GROUP,
    SYSINFOD_UID, SYSINFOD_USERNAME, SYSTEM_GID, SYSTEM_GROUP, SYSTEM_UID, SYSTEM_USERNAME,
};
pub use record::{
    AccountState, Gid, Identity, Uid, UserRecord, MAX_DISPLAY_NAME_LEN, MAX_PATH_LEN,
    MAX_SUPPLEMENTARY_GIDS, MAX_USERNAME_LEN, NO_PATH_MARKER,
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
    /// An account state is not `active`, `locked`, or `nologin`.
    AccountState,
    /// The account state and the home/shell/password presence disagree: a
    /// login-capable account requires all three; a no-login account
    /// carries none of them.
    AccountShape,
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
            Self::AccountShape => {
                "user record pairs its state with the wrong home/shell/password shape"
            }
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
