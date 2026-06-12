//! The production [`Authenticator`]: verify credentials against the
//! `/System/Security/Users` database.
//!
//! [`UsersAuthenticator`] is the seam implementation the login binary wires
//! once the database text has been read from `/System/Security/Users`
//! (`AGENTS.md` §5.1, §16.2). All verification lives in `lib/users`:
//! PBKDF2-HMAC-SHA256 through `lib/crypto`, constant-time hash comparison,
//! and a timing-equalised refusal for unknown or locked accounts
//! (`AGENTS.md` §19.1). This type only adapts the database's answer to the
//! [`Authenticator`] contract — every refusal becomes the same
//! [`Errno::PermissionDenied`], so the prompt cannot probe for valid
//! usernames (`AGENTS.md` §5.4).

use rustos_abi::Errno;
use rustos_users::UsersDb;

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

impl Authenticator for UsersAuthenticator<'_> {
    fn authenticate(&self, credentials: &Credentials) -> Result<AuthenticatedUser, Errno> {
        let record = self
            .db
            .authenticate(&credentials.username, credentials.password.as_bytes())
            .map_err(|_| Errno::PermissionDenied)?;
        Ok(AuthenticatedUser {
            uid: record.uid(),
            primary_gid: record.primary_gid(),
            supplementary_gids: record.supplementary_gids().to_vec(),
            capabilities: record.capabilities(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::UsersAuthenticator;
    use crate::session::{Authenticator, Credentials};

    use alloc::string::ToString;
    use alloc::vec;
    use rustos_abi::{CapabilityId, Errno};
    use rustos_caps::CapabilitySet;
    use rustos_users::{AccountState, Gid, Identity, Uid, UserRecord, UsersDb, MIN_ITERATIONS};

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
                home: "/Users/ada",
                shell: "/Apps/Shell.app/Run",
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
                home: "/Users/mallory",
                shell: "/Apps/Shell.app/Run",
                capabilities: CapabilitySet::empty(),
                state: AccountState::Locked,
            },
            b"evil",
            [0x43; 16],
            MIN_ITERATIONS,
        )
        .expect("valid record");
        UsersDb::new(vec![ada, locked]).expect("valid db")
    }

    fn credentials(username: &str, password: &str) -> Credentials {
        Credentials {
            username: username.to_string(),
            password: password.to_string(),
        }
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
    }

    #[test]
    fn every_refusal_is_the_same_error() {
        let db = db();
        let auth = UsersAuthenticator::new(&db);
        for (username, password) in [
            ("ada", "wrong"),
            ("nobody", "byron"),
            ("mallory", "evil"),
            ("", ""),
        ] {
            assert_eq!(
                auth.authenticate(&credentials(username, password)),
                Err(Errno::PermissionDenied),
                "credentials {username:?}/{password:?}"
            );
        }
    }
}
