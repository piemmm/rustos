//! The production [`UserDb`]: the `users_admin` client that lists the
//! database and submits the new account record.
//!
//! [`UsersAdminDb`] holds the whole client policy — the request encoding,
//! the reply decoding, the uid auto-allocation, the account defaults, and
//! the created account's password posture — behind two small injected
//! seams, so every decision is host-tested and the freestanding `Run`
//! binary adds only the raw syscall and the `sys:random` draw.
//!
//! # The created account has no usable password
//!
//! GNU `useradd` creates an account that cannot authenticate until an
//! administrator sets a password. The database requires a well-formed
//! password record on creation, so this client submits one derived from a
//! throwaway random secret it immediately discards: no password matches
//! it (recovering one is a PBKDF2 preimage search), which is the honest
//! TAIRiX equivalent of the `!` field. The administrator then sets a real
//! password with the `users` tool's `passwd` command.
//!
//! # Defaults are the shared account policy
//!
//! The auto-allocated uid ([`tairix_users::next_id`], interactive-user
//! range), the home layout
//! ([`tairix_users::default_home`]), the login shell
//! ([`tairix_users::DEFAULT_SHELL`]), and the created ceiling
//! ([`tairix_users::SESSION_BASELINE`] — an administrator widens it
//! afterwards with the `users` tool's `grant`, bounded by their own
//! effective set) all come from the one `lib/users` policy definition,
//! never a private copy.

use alloc::string::String;
use alloc::vec::Vec;

use tairix_abi::users_admin::{
    decode_user_list, gid_list_into, grant_list_into, CreateUser, UsersAdminRequest,
    USERS_ADMIN_MAX_REQUEST,
};
use tairix_abi::Errno;
use tairix_users::{
    default_home, next_id, IdRange, PasswordRecord, Salt, DEFAULT_SHELL, MAX_DB_LEN,
    MIN_ITERATIONS, SALT_LEN, SESSION_BASELINE,
};

use crate::io::{UserDb, UserSpec};

/// Byte capacity for a `ListUsers` reply: comfortably above the largest
/// database the kernel serialises (the same headroom the `users` tool's
/// session uses).
const RESPONSE_CAPACITY: usize = 2 * MAX_DB_LEN;

/// Length of the throwaway random secret behind the created account's
/// unusable password record: 256 bits, so no guess can match it.
const THROWAWAY_SECRET_LEN: usize = 32;

/// The transport that carries one encoded `users_admin` request and
/// returns the response bytes written. On a running system this is the
/// `users_admin` syscall; in tests an in-memory database. Every
/// authorisation decision stays on the far side of this seam.
pub trait AdminChannel {
    /// Submit `req`, writing any response into `out`.
    ///
    /// # Errors
    ///
    /// The [`Errno`] the database raises — e.g.
    /// [`Errno::PermissionDenied`] for a caller without `CAP_USER_ADMIN`.
    fn call(&self, req: &[u8], out: &mut [u8]) -> Result<usize, Errno>;
}

/// A cryptographic randomness source (the kernel CSPRNG through
/// `sys:random` in production). Refuses — never guesses — when the draw
/// fails, so a weak record is never built from a partial fill.
pub trait Entropy {
    /// Fill `buf` entirely with random bytes.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the source raises; the caller fails closed.
    fn fill(&self, buf: &mut [u8]) -> Result<(), Errno>;
}

/// The production [`UserDb`] over an [`AdminChannel`] and an [`Entropy`]
/// source.
pub struct UsersAdminDb<'a> {
    channel: &'a dyn AdminChannel,
    entropy: &'a dyn Entropy,
}

impl<'a> UsersAdminDb<'a> {
    /// A client over `channel` and `entropy`.
    #[must_use]
    pub fn new(channel: &'a dyn AdminChannel, entropy: &'a dyn Entropy) -> Self {
        Self { channel, entropy }
    }

    /// Every account's `(name, uid)`, from a `ListUsers` round trip.
    fn list(&self) -> Result<Vec<(String, u32)>, Errno> {
        let mut req = [0u8; USERS_ADMIN_MAX_REQUEST];
        let len = UsersAdminRequest::ListUsers.encode_into(&mut req)?;
        let mut out = alloc::vec![0u8; RESPONSE_CAPACITY];
        let used = self.channel.call(&req[..len], &mut out)?;
        let bytes = out.get(..used).ok_or(Errno::LengthOutOfRange)?;
        let mut users = Vec::new();
        for entry in decode_user_list(bytes)? {
            let entry = entry?;
            users.push((String::from(entry.username), entry.uid));
        }
        Ok(users)
    }

    /// A well-formed password record no password matches, from a
    /// throwaway random secret discarded (and zeroed) before returning.
    ///
    /// The minimum iteration count is deliberate: PBKDF2 cost exists to
    /// slow the brute-forcing of *guessable* passwords, and a discarded
    /// 256-bit random secret is unguessable at any cost — so paying the
    /// interactive-password cost here would be pure waste. A real password
    /// set later replaces the whole record, cost included.
    fn unusable_password_record(&self) -> Result<String, Errno> {
        let mut salt: Salt = [0u8; SALT_LEN];
        self.entropy.fill(&mut salt)?;
        let mut secret = [0u8; THROWAWAY_SECRET_LEN];
        self.entropy.fill(&mut secret)?;
        let record = PasswordRecord::new(&secret, salt, MIN_ITERATIONS);
        secret.fill(0);
        // Unreachable by construction (the length and iteration count are
        // in bounds), but fail closed rather than panic.
        let record = record.map_err(|_| Errno::OutOfRange)?;
        Ok(record.encode())
    }
}

impl UserDb for UsersAdminDb<'_> {
    fn name_in_use(&self, name: &str) -> Result<bool, Errno> {
        Ok(self.list()?.iter().any(|(taken, _)| taken == name))
    }

    fn create(&self, spec: &UserSpec<'_>) -> Result<(), Errno> {
        let uid = match spec.uid {
            Some(uid) => uid,
            None => next_id(IdRange::User, self.list()?.into_iter().map(|(_, uid)| uid))
                .ok_or(Errno::OutOfRange)?,
        };
        let password_record = self.unusable_password_record()?;
        let mut grant_backing = [0u8; 2 * SESSION_BASELINE.len()];
        let grants = grant_list_into(SESSION_BASELINE, &mut grant_backing)?;
        let mut gid_backing = alloc::vec![0u8; 4 * spec.supplementary_gids.len()];
        let gids = gid_list_into(spec.supplementary_gids, &mut gid_backing)?;
        let home = spec
            .home
            .map_or_else(|| default_home(spec.name), String::from);
        let request = UsersAdminRequest::CreateUser(CreateUser {
            username: spec.name,
            uid,
            primary_gid: spec.primary_gid,
            supplementary_gids: gids,
            display_name: spec.comment.unwrap_or(""),
            home: &home,
            shell: DEFAULT_SHELL,
            grants,
            password_record: &password_record,
        });
        let mut req = [0u8; USERS_ADMIN_MAX_REQUEST];
        let len = request.encode_into(&mut req)?;
        self.channel.call(&req[..len], &mut [])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{AdminChannel, Entropy, UsersAdminDb, RESPONSE_CAPACITY};
    use crate::io::{UserDb, UserSpec};
    use alloc::string::{String, ToString};
    use alloc::vec::Vec;
    use core::cell::RefCell;
    use tairix_abi::users_admin::{
        ListResponseBuilder, UserEntry, UsersAdminRequest, USERS_ADMIN_VERSION,
    };
    use tairix_abi::Errno;
    use tairix_users::{PasswordRecord, DEFAULT_SHELL, SESSION_BASELINE};

    /// An in-memory `users_admin` endpoint: serves `ListUsers` from a
    /// `(name, uid)` table and records every decoded `CreateUser`.
    struct MemChannel {
        existing: Vec<(String, u32)>,
        fail: Option<Errno>,
        created: RefCell<Vec<CreatedRecord>>,
    }

    #[derive(Clone, Debug, Eq, PartialEq)]
    struct CreatedRecord {
        username: String,
        uid: u32,
        primary_gid: u32,
        supplementary_gids: Vec<u32>,
        display_name: String,
        home: String,
        shell: String,
        grant_count: usize,
        password_record: String,
    }

    impl MemChannel {
        fn new(existing: &[(&str, u32)]) -> Self {
            Self {
                existing: existing
                    .iter()
                    .map(|(n, uid)| ((*n).to_string(), *uid))
                    .collect(),
                fail: None,
                created: RefCell::new(Vec::new()),
            }
        }

        fn failing(mut self, errno: Errno) -> Self {
            self.fail = Some(errno);
            self
        }

        fn created(&self) -> Vec<CreatedRecord> {
            self.created.borrow().clone()
        }
    }

    impl AdminChannel for MemChannel {
        fn call(&self, req: &[u8], out: &mut [u8]) -> Result<usize, Errno> {
            if let Some(errno) = self.fail {
                return Err(errno);
            }
            match UsersAdminRequest::decode(req)? {
                UsersAdminRequest::ListUsers => {
                    let mut builder = ListResponseBuilder::new(out)?;
                    for (name, uid) in &self.existing {
                        builder.push_user(&UserEntry {
                            username: name,
                            uid: *uid,
                            primary_gid: 100,
                            supplementary_gids: tairix_abi::users_admin::gid_list_into(
                                &[],
                                &mut [],
                            )?,
                            display_name: "",
                            home: "/Users/x",
                            shell: DEFAULT_SHELL,
                            grants: tairix_abi::users_admin::grant_list_into(&[], &mut [])?,
                            state: tairix_abi::users_admin::AccountStateCode::Active,
                        })?;
                    }
                    Ok(builder.finish())
                }
                UsersAdminRequest::CreateUser(create) => {
                    self.created.borrow_mut().push(CreatedRecord {
                        username: create.username.to_string(),
                        uid: create.uid,
                        primary_gid: create.primary_gid,
                        supplementary_gids: create.supplementary_gids.iter().collect(),
                        display_name: create.display_name.to_string(),
                        home: create.home.to_string(),
                        shell: create.shell.to_string(),
                        grant_count: create.grants.len(),
                        password_record: create.password_record.to_string(),
                    });
                    Ok(0)
                }
                _ => Err(Errno::NotImplemented),
            }
        }
    }

    /// A deterministic entropy source (a fixed byte), or a refusing one.
    struct FixedEntropy {
        fail: bool,
    }

    impl Entropy for FixedEntropy {
        fn fill(&self, buf: &mut [u8]) -> Result<(), Errno> {
            if self.fail {
                return Err(Errno::NotImplemented);
            }
            buf.fill(0x5a);
            Ok(())
        }
    }

    const ENTROPY: FixedEntropy = FixedEntropy { fail: false };

    fn spec(name: &str, uid: Option<u32>) -> UserSpec<'_> {
        UserSpec {
            name,
            uid,
            primary_gid: 100,
            supplementary_gids: &[],
            comment: None,
            home: None,
        }
    }

    #[test]
    fn name_in_use_reflects_the_listing() {
        let channel = MemChannel::new(&[("root", 0), ("alice", 1)]);
        let db = UsersAdminDb::new(&channel, &ENTROPY);
        assert_eq!(db.name_in_use("alice"), Ok(true));
        assert_eq!(db.name_in_use("bob"), Ok(false));
    }

    #[test]
    fn a_channel_failure_surfaces_from_the_lookup() {
        let channel = MemChannel::new(&[]).failing(Errno::PermissionDenied);
        let db = UsersAdminDb::new(&channel, &ENTROPY);
        assert_eq!(db.name_in_use("alice"), Err(Errno::PermissionDenied));
    }

    #[test]
    fn an_omitted_uid_is_allocated_in_the_user_band() {
        // System-band uids never steer the allocation: a database holding
        // only reserved uids yields the band's first id…
        let channel = MemChannel::new(&[("root", 0), ("devmgr", 7)]);
        let db = UsersAdminDb::new(&channel, &ENTROPY);
        assert_eq!(db.create(&spec("bob", None)), Ok(()));
        let created = channel.created();
        assert_eq!(created.len(), 1);
        assert_eq!(created[0].uid, 1000);

        // …and an existing user-band uid is allocated above.
        let channel = MemChannel::new(&[("root", 0), ("alice", 1004)]);
        let db = UsersAdminDb::new(&channel, &ENTROPY);
        assert_eq!(db.create(&spec("bob", None)), Ok(()));
        assert_eq!(channel.created()[0].uid, 1005);
    }

    #[test]
    fn a_requested_uid_is_passed_through_verbatim() {
        let channel = MemChannel::new(&[("root", 0)]);
        let db = UsersAdminDb::new(&channel, &ENTROPY);
        assert_eq!(db.create(&spec("bob", Some(4321))), Ok(()));
        assert_eq!(channel.created()[0].uid, 4321);
    }

    #[test]
    fn an_exhausted_uid_space_fails_closed() {
        let channel = MemChannel::new(&[("root", 0), ("max", u32::MAX)]);
        let db = UsersAdminDb::new(&channel, &ENTROPY);
        assert_eq!(db.create(&spec("bob", None)), Err(Errno::OutOfRange));
        assert!(channel.created().is_empty());
    }

    #[test]
    fn the_defaults_are_the_shared_account_policy() {
        let channel = MemChannel::new(&[("root", 0)]);
        let db = UsersAdminDb::new(&channel, &ENTROPY);
        assert_eq!(db.create(&spec("bob", None)), Ok(()));
        let created = channel.created();
        assert_eq!(created[0].home, "/Users/bob");
        assert_eq!(created[0].shell, DEFAULT_SHELL);
        assert_eq!(created[0].display_name, "");
        assert_eq!(created[0].grant_count, SESSION_BASELINE.len());
    }

    #[test]
    fn explicit_fields_reach_the_record() {
        let channel = MemChannel::new(&[("root", 0)]);
        let db = UsersAdminDb::new(&channel, &ENTROPY);
        let spec = UserSpec {
            name: "bob",
            uid: Some(9),
            primary_gid: 200,
            supplementary_gids: &[10, 20],
            comment: Some("Bob B"),
            home: Some("/Users/elsewhere"),
        };
        assert_eq!(db.create(&spec), Ok(()));
        let created = channel.created();
        assert_eq!(created[0].primary_gid, 200);
        assert_eq!(created[0].supplementary_gids, [10, 20]);
        assert_eq!(created[0].display_name, "Bob B");
        assert_eq!(created[0].home, "/Users/elsewhere");
    }

    #[test]
    fn the_created_record_is_well_formed_and_matches_no_password() {
        let channel = MemChannel::new(&[("root", 0)]);
        let db = UsersAdminDb::new(&channel, &ENTROPY);
        assert_eq!(db.create(&spec("bob", None)), Ok(()));
        let record = channel.created()[0].password_record.clone();
        let decoded = PasswordRecord::decode(&record).expect("well-formed record");
        // The throwaway secret is 0x5a repeated under this test's entropy;
        // real candidates — including the empty password — do not match.
        assert!(!decoded.verify(b""));
        assert!(!decoded.verify(b"password"));
        assert!(!decoded.verify(b"bob"));
    }

    #[test]
    fn a_refused_entropy_draw_creates_nothing() {
        let channel = MemChannel::new(&[("root", 0)]);
        let entropy = FixedEntropy { fail: true };
        let db = UsersAdminDb::new(&channel, &entropy);
        assert_eq!(db.create(&spec("bob", None)), Err(Errno::NotImplemented));
        assert!(channel.created().is_empty());
    }

    #[test]
    fn a_hostile_reply_fails_closed() {
        /// A channel answering with a version the client does not speak.
        struct BadVersion;
        impl AdminChannel for BadVersion {
            fn call(&self, _req: &[u8], out: &mut [u8]) -> Result<usize, Errno> {
                let bad = (USERS_ADMIN_VERSION + 1).to_le_bytes();
                out[..2].copy_from_slice(&bad);
                out[2..4].copy_from_slice(&0u16.to_le_bytes());
                Ok(4)
            }
        }
        let db = UsersAdminDb::new(&BadVersion, &ENTROPY);
        assert_eq!(db.name_in_use("alice"), Err(Errno::AbiVersionUnsupported));
    }

    #[test]
    fn a_reply_longer_than_the_buffer_fails_closed() {
        /// A channel claiming to have written more than the buffer holds.
        struct Overlong;
        impl AdminChannel for Overlong {
            fn call(&self, _req: &[u8], _out: &mut [u8]) -> Result<usize, Errno> {
                Ok(RESPONSE_CAPACITY + 1)
            }
        }
        let db = UsersAdminDb::new(&Overlong, &ENTROPY);
        assert_eq!(db.name_in_use("alice"), Err(Errno::LengthOutOfRange));
    }
}
