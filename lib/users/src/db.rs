//! The `/System/Security/Users` database: parse, serialise, authenticate.
//!
//! The on-disk text is **untrusted input** (`AGENTS.md` §19.5/§19.6): the
//! parser bounds the whole file, every line, and the record count before
//! reading anything, validates every field through [`UserRecord`], enforces
//! username and uid uniqueness, and fails closed on the first defect — a
//! database the parser cannot fully understand yields **no** [`UsersDb`]
//! (`AGENTS.md` §2.9, §5.4).
//!
//! # Format (`rustos-users-v1`)
//!
//! Line one is exactly [`FORMAT_HEADER`]. Every other line is blank, a `#`
//! comment, or one [`UserRecord`] line:
//!
//! ```text
//! rustos-users-v1
//! # username:uid:gid:supplementary:display name:home:shell:caps:state:password
//! root:0:0::System Administrator:/Users/root:/Apps/Shell.app/Run:CAP_USER_ADMIN:active:pbkdf2-sha256$600000$…$…
//! ```

use core::num::NonZeroU32;

use alloc::string::String;
use alloc::vec::Vec;

use rustos_crypto::{pbkdf2_sha256_verify, PASSWORD_HASH_LEN};

use crate::password::{DEFAULT_ITERATIONS, MAX_PASSWORD_LEN, SALT_LEN};
use crate::record::{AccountState, Uid, UserRecord};
use crate::{AuthError, ParseError};

/// The exact first line of every `users-v1` database.
pub const FORMAT_HEADER: &str = "rustos-users-v1";

/// Largest database file, in bytes, the parser will consider (§24.4
/// validation bound — a defence, not a capacity).
pub const MAX_DB_LEN: usize = 64 * 1024;

/// Longest single line, in bytes.
pub const MAX_LINE_LEN: usize = 512;

/// Most records one database may hold.
pub const MAX_USERS: usize = 512;

/// A parsed, validated user database.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UsersDb {
    records: Vec<UserRecord>,
}

impl UsersDb {
    /// Build a database from validated records, enforcing the whole-database
    /// invariants.
    ///
    /// # Errors
    ///
    /// [`ParseError::TooManyUsers`] past [`MAX_USERS`];
    /// [`ParseError::DuplicateUsername`] / [`ParseError::DuplicateUserId`]
    /// when two records collide.
    pub fn new(records: Vec<UserRecord>) -> Result<Self, ParseError> {
        if records.len() > MAX_USERS {
            return Err(ParseError::TooManyUsers);
        }
        for (index, record) in records.iter().enumerate() {
            for earlier in &records[..index] {
                if earlier.username() == record.username() {
                    return Err(ParseError::DuplicateUsername);
                }
                if earlier.uid() == record.uid() {
                    return Err(ParseError::DuplicateUserId);
                }
            }
        }
        Ok(Self { records })
    }

    /// Parse and validate a whole database text.
    ///
    /// # Errors
    ///
    /// The matching [`ParseError`], failing closed on the first defect.
    pub fn parse(text: &str) -> Result<Self, ParseError> {
        if text.len() > MAX_DB_LEN {
            return Err(ParseError::TooLong);
        }
        let mut lines = text.lines();
        if lines.next() != Some(FORMAT_HEADER) {
            return Err(ParseError::Header);
        }

        let mut records = Vec::new();
        for line in lines {
            if line.len() > MAX_LINE_LEN {
                return Err(ParseError::LineTooLong);
            }
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            if records.len() == MAX_USERS {
                return Err(ParseError::TooManyUsers);
            }
            records.push(UserRecord::decode_line(trimmed)?);
        }
        Self::new(records)
    }

    /// Serialise the database into the text form [`Self::parse`] accepts.
    #[must_use]
    pub fn serialise(&self) -> String {
        let mut out = String::from(FORMAT_HEADER);
        out.push('\n');
        for record in &self.records {
            out.push_str(&record.encode_line());
            out.push('\n');
        }
        out
    }

    /// Every record, in file order.
    #[must_use]
    pub fn records(&self) -> &[UserRecord] {
        &self.records
    }

    /// The record named `username`, if any.
    #[must_use]
    pub fn lookup(&self, username: &str) -> Option<&UserRecord> {
        self.records
            .iter()
            .find(|record| record.username() == username)
    }

    /// Verify a `(username, password)` pair, returning the matched record.
    ///
    /// Refusals are indistinguishable: an unknown username, a locked
    /// account, and a wrong password all cost one PBKDF2 derivation and all
    /// return the same [`AuthError::InvalidCredentials`], so a caller cannot
    /// probe for valid usernames or locked accounts (`AGENTS.md` §5.4,
    /// §19.1).
    ///
    /// # Errors
    ///
    /// [`AuthError::InvalidCredentials`] — the only refusal this method can
    /// express, by design.
    pub fn authenticate(&self, username: &str, password: &[u8]) -> Result<&UserRecord, AuthError> {
        match self.lookup(username) {
            Some(record) if record.state() == AccountState::Active => {
                if record.password().verify(password) {
                    Ok(record)
                } else {
                    Err(AuthError::InvalidCredentials)
                }
            }
            _ => {
                self.burn_dummy_derivation(password);
                Err(AuthError::InvalidCredentials)
            }
        }
    }

    /// The record owning `uid`, if any.
    #[must_use]
    pub fn lookup_uid(&self, uid: Uid) -> Option<&UserRecord> {
        self.records.iter().find(|record| record.uid() == uid)
    }

    /// Pay the PBKDF2 cost a real verification would have paid, so a refusal
    /// for an unknown or locked account takes as long as a wrong password on
    /// a real one (`AGENTS.md` §19.1). The burn uses the database's highest
    /// record cost (the default cost when the database is empty) against an
    /// all-zero salt and hash; the discarded result is always `false`.
    fn burn_dummy_derivation(&self, password: &[u8]) {
        if password.len() > MAX_PASSWORD_LEN {
            return;
        }
        let cost = self
            .records
            .iter()
            .map(|record| record.password().iterations())
            .max()
            .unwrap_or(DEFAULT_ITERATIONS);
        if let Some(iterations) = NonZeroU32::new(cost) {
            let _ = pbkdf2_sha256_verify(
                password,
                &[0u8; SALT_LEN],
                iterations,
                &[0u8; PASSWORD_HASH_LEN],
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{UsersDb, FORMAT_HEADER, MAX_DB_LEN, MAX_USERS};
    use crate::password::MIN_ITERATIONS;
    use crate::record::{AccountState, Gid, Identity, Uid, UserRecord};
    use crate::{AuthError, ParseError};

    use alloc::string::String;
    use alloc::vec::Vec;
    use rustos_abi::CapabilityId;
    use rustos_caps::CapabilitySet;

    fn record(username: &str, uid: u32, state: AccountState, password: &[u8]) -> UserRecord {
        let mut capabilities = CapabilitySet::empty();
        capabilities.insert(CapabilityId::PROC_SPAWN);
        UserRecord::with_password(
            Identity {
                username,
                uid: Uid(uid),
                primary_gid: Gid(uid),
                supplementary_gids: &[],
                display_name: "",
                home: "/Users/test",
                shell: "/Apps/Shell.app/Run",
                capabilities,
                state,
            },
            password,
            [0x3C; 16],
            MIN_ITERATIONS,
        )
        .expect("valid record")
    }

    fn db() -> UsersDb {
        UsersDb::new(alloc::vec![
            record("root", 0, AccountState::Active, b"root"),
            record("ada", 1000, AccountState::Active, b"byron"),
            record("mallory", 1001, AccountState::Locked, b"evil"),
        ])
        .expect("valid db")
    }

    #[test]
    fn serialise_parse_round_trips() {
        let original = db();
        let text = original.serialise();
        assert!(text.starts_with("rustos-users-v1\n"));
        assert_eq!(UsersDb::parse(&text), Ok(original));
    }

    #[test]
    fn comments_and_blank_lines_are_ignored() {
        let mut text = String::from(FORMAT_HEADER);
        text.push_str("\n# a comment\n\n   \n");
        text.push_str(&record("ada", 1000, AccountState::Active, b"byron").encode_line());
        text.push('\n');
        let parsed = UsersDb::parse(&text).expect("parses");
        assert_eq!(parsed.records().len(), 1);
    }

    #[test]
    fn missing_or_wrong_header_is_rejected() {
        assert_eq!(UsersDb::parse(""), Err(ParseError::Header));
        assert_eq!(UsersDb::parse("rustos-users-v2\n"), Err(ParseError::Header));
        let body = record("ada", 1000, AccountState::Active, b"x").encode_line();
        assert_eq!(UsersDb::parse(&body), Err(ParseError::Header));
    }

    #[test]
    fn oversized_inputs_are_rejected_before_scanning() {
        let mut text = String::from(FORMAT_HEADER);
        text.push('\n');
        while text.len() <= MAX_DB_LEN {
            text.push_str("# padding\n");
        }
        assert_eq!(UsersDb::parse(&text), Err(ParseError::TooLong));

        let mut long_line = String::from(FORMAT_HEADER);
        long_line.push('\n');
        long_line.push('#');
        for _ in 0..super::MAX_LINE_LEN {
            long_line.push('x');
        }
        long_line.push('\n');
        assert_eq!(UsersDb::parse(&long_line), Err(ParseError::LineTooLong));
    }

    #[test]
    fn duplicates_are_rejected() {
        assert_eq!(
            UsersDb::new(alloc::vec![
                record("ada", 1000, AccountState::Active, b"x"),
                record("ada", 1001, AccountState::Active, b"x"),
            ]),
            Err(ParseError::DuplicateUsername)
        );
        assert_eq!(
            UsersDb::new(alloc::vec![
                record("ada", 1000, AccountState::Active, b"x"),
                record("bob", 1000, AccountState::Active, b"x"),
            ]),
            Err(ParseError::DuplicateUserId)
        );
    }

    #[test]
    fn the_record_budget_is_enforced() {
        let mut names = Vec::new();
        for i in 0..=MAX_USERS {
            let mut name = String::from("u");
            let _ = core::fmt::Write::write_fmt(&mut name, format_args!("{i}"));
            names.push(name);
        }
        let records: Vec<UserRecord> = names
            .iter()
            .enumerate()
            .map(|(i, name)| {
                record(
                    name,
                    u32::try_from(i).expect("fits"),
                    AccountState::Active,
                    b"x",
                )
            })
            .collect();
        assert_eq!(UsersDb::new(records), Err(ParseError::TooManyUsers));
    }

    #[test]
    fn authentication_accepts_only_the_right_password_on_an_active_account() {
        let db = db();
        assert_eq!(
            db.authenticate("ada", b"byron").map(UserRecord::uid),
            Ok(Uid(1000))
        );
        assert_eq!(
            db.authenticate("ada", b"wrong"),
            Err(AuthError::InvalidCredentials)
        );
    }

    #[test]
    fn unknown_and_locked_accounts_are_indistinguishable_refusals() {
        let db = db();
        assert_eq!(
            db.authenticate("nobody", b"anything"),
            Err(AuthError::InvalidCredentials)
        );
        assert_eq!(
            db.authenticate("mallory", b"evil"),
            Err(AuthError::InvalidCredentials)
        );
        let empty = UsersDb::new(Vec::new()).expect("empty db");
        assert_eq!(
            empty.authenticate("root", b""),
            Err(AuthError::InvalidCredentials)
        );
    }

    #[test]
    fn lookups_find_records_by_name_and_uid() {
        let db = db();
        assert_eq!(db.lookup("root").map(UserRecord::uid), Some(Uid(0)));
        assert!(db.lookup("missing").is_none());
        assert_eq!(
            db.lookup_uid(Uid(1001)).map(UserRecord::username),
            Some("mallory")
        );
        assert!(db.lookup_uid(Uid(42)).is_none());
    }
}
