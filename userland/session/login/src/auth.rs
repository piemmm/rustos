//! The production [`Authenticator`]: verify credentials against the
//! `/System/Security/Users` database.
//!
//! [`UsersAuthenticator`] is the seam implementation the login binary wires
//! once the database text has been read from `/System/Security/Users`. All verification lives in `lib/users`:
//! PBKDF2-HMAC-SHA256 through `lib/crypto`, constant-time hash comparison,
//! and a timing-equalised refusal for unknown or locked accounts. This type only adapts the database's answer to the
//! [`Authenticator`] contract — every refusal becomes the same
//! [`Errno::PermissionDenied`], so the prompt cannot probe for valid
//! usernames.

use alloc::string::ToString;

use tairix_abi::Errno;
use tairix_users::{Uid, UserRecord, UsersDb};

use crate::session::{AuthenticatedUser, Authenticator, Credentials};

/// An [`Authenticator`] backed by a parsed, validated [`UsersDb`].
pub struct UsersAuthenticator<'a> {
    db: &'a UsersDb,
}

impl<'a> UsersAuthenticator<'a> {
    /// Wrap a parsed user database.
    #[must_use]
    pub fn new(db: &'a UsersDb) -> Self {
        Self { db }
    }
}

/// Adapt a matched, verified [`UserRecord`] to the [`Authenticator`]
/// contract shared by [`UsersAuthenticator::authenticate`] and
/// [`UsersAuthenticator::authenticate_uid`].
///
/// Only an active account authenticates, and the record format guarantees
/// an active account carries both a home and a shell — but a session
/// without them is refused rather than fabricated (fail closed).
fn authenticated_user(record: &UserRecord) -> Result<AuthenticatedUser, Errno> {
    let (Some(home), Some(shell)) = (record.home(), record.shell()) else {
        return Err(Errno::PermissionDenied);
    };
    Ok(AuthenticatedUser {
        username: record.username().to_string(),
        uid: record.uid(),
        primary_gid: record.primary_gid(),
        supplementary_gids: record.supplementary_gids().to_vec(),
        capabilities: record.capabilities(),
        home: home.to_string(),
        shell: shell.to_string(),
    })
}

impl Authenticator for UsersAuthenticator<'_> {
    fn authenticate(&self, credentials: &Credentials<'_>) -> Result<AuthenticatedUser, Errno> {
        let record = self
            .db
            .authenticate(credentials.username, credentials.password.as_bytes())
            .map_err(|_| Errno::PermissionDenied)?;
        authenticated_user(record)
    }

    fn authenticate_uid(&self, uid: u32, password: &str) -> Result<AuthenticatedUser, Errno> {
        let record = self
            .db
            .authenticate_uid(Uid(uid), password.as_bytes())
            .map_err(|_| Errno::PermissionDenied)?;
        authenticated_user(record)
    }
}

/// An [`Authenticator`] wired when no user database is held: every attempt
/// is refused with the same error, so an installer image — or a boot that
/// has not yet unlocked the encrypted root that carries the database — sits
/// at a prompt that grants nothing (fail closed, never
/// invent an account). Paired with [`UsersAuthenticator`] by
/// [`supervise`](crate::supervise::supervise), which wires this whenever a
/// round's database reload returns nothing.
pub struct DenyAll;

impl Authenticator for DenyAll {
    fn authenticate(&self, _credentials: &Credentials<'_>) -> Result<AuthenticatedUser, Errno> {
        Err(Errno::PermissionDenied)
    }

    fn authenticate_uid(&self, _uid: u32, _password: &str) -> Result<AuthenticatedUser, Errno> {
        Err(Errno::PermissionDenied)
    }
}

#[cfg(test)]
mod tests {
    use super::UsersAuthenticator;
    use crate::session::{Authenticator, Credentials};

    use alloc::vec;
    use tairix_abi::{CapabilityId, Errno};
    use tairix_caps::CapabilitySet;
    use tairix_users::{
        AccountState, Gid, Identity, StoredPassword, Uid, UserRecord, UsersDb, MIN_ITERATIONS,
    };

    fn db() -> UsersDb {
        let mut capabilities = CapabilitySet::empty();
        capabilities.insert(CapabilityId::PROC_SPAWN);
        let ada = UserRecord::with_password(
            Identity {
                username: "ada",
                uid: Uid(1000),
                primary_gid: Gid(1000),
                supplementary_gids: &[Gid(4)],
                display_name: "Ada Lovelace",
                home: Some("/Users/ada"),
                shell: Some("/System/Commands/elsh.app/Run"),
                capabilities,
                state: AccountState::Active,
            },
            b"byron",
            [0x42; 16],
            MIN_ITERATIONS,
        )
        .expect("valid record");
        let locked = UserRecord::with_password(
            Identity {
                username: "mallory",
                uid: Uid(1001),
                primary_gid: Gid(1001),
                supplementary_gids: &[],
                display_name: "",
                home: Some("/Users/mallory"),
                shell: Some("/System/Commands/elsh.app/Run"),
                capabilities: CapabilitySet::empty(),
                state: AccountState::Locked,
            },
            b"evil",
            [0x43; 16],
            MIN_ITERATIONS,
        )
        .expect("valid record");
        let service = UserRecord::new(
            Identity {
                username: "devmgr",
                uid: Uid(10),
                primary_gid: Gid(101),
                supplementary_gids: &[],
                display_name: "",
                home: None,
                shell: None,
                capabilities: CapabilitySet::empty(),
                state: AccountState::NoLogin,
            },
            StoredPassword::NeverAuthenticates,
        )
        .expect("valid record");
        UsersDb::new(vec![ada, locked, service]).expect("valid db")
    }

    fn credentials<'a>(username: &'a str, password: &'a str) -> Credentials<'a> {
        Credentials { username, password }
    }

    #[test]
    fn valid_credentials_yield_the_full_identity() {
        let db = db();
        let auth = UsersAuthenticator::new(&db);
        let user = auth
            .authenticate(&credentials("ada", "byron"))
            .expect("authenticates");
        assert_eq!(user.uid, Uid(1000));
        assert_eq!(user.primary_gid, Gid(1000));
        assert_eq!(user.supplementary_gids, vec![Gid(4)]);
        assert!(user.capabilities.contains(CapabilityId::PROC_SPAWN));
        assert!(!user.capabilities.contains(CapabilityId::USER_ADMIN));
        assert_eq!(user.shell, "/System/Commands/elsh.app/Run");
    }

    #[test]
    fn every_refusal_is_the_same_error() {
        let db = db();
        let auth = UsersAuthenticator::new(&db);
        for (username, password) in [
            ("ada", "wrong"),
            ("nobody", "byron"),
            ("mallory", "evil"),
            ("devmgr", ""),
            ("devmgr", "*"),
            ("", ""),
        ] {
            assert_eq!(
                auth.authenticate(&credentials(username, password)),
                Err(Errno::PermissionDenied),
                "credentials {username:?}/{password:?}"
            );
        }
    }

    #[test]
    fn uid_authentication_yields_the_same_identity_as_the_username_path() {
        let db = db();
        let auth = UsersAuthenticator::new(&db);
        let by_uid = auth.authenticate_uid(1000, "byron").expect("authenticates");
        let by_name = auth
            .authenticate(&credentials("ada", "byron"))
            .expect("authenticates");
        assert_eq!(by_uid, by_name);
    }

    #[test]
    fn uid_authentication_refusals_are_the_same_error() {
        let db = db();
        let auth = UsersAuthenticator::new(&db);
        for (uid, password) in [
            (1000, "wrong"), // right account, wrong password
            (9999, "byron"), // no account owns this uid
            (1001, "evil"),  // a locked account's own uid
            (10, ""),        // a no-login service account
        ] {
            assert_eq!(
                auth.authenticate_uid(uid, password),
                Err(Errno::PermissionDenied),
                "uid {uid:?}/password {password:?}"
            );
        }
    }
}
