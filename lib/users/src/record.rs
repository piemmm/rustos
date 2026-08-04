//! One user account: identity, grants, state, and the stored password.
//!
//! A [`UserRecord`] is one validated line of the `users-v1` database. Every
//! field is checked at construction *and* at decode, so an in-memory record
//! and a parsed record obey the same invariants — there is no way to hold a
//! `UserRecord` whose fields the database format could not carry
//! (illegal states unrepresentable).

use alloc::string::String;
use alloc::vec::Vec;

use tairix_abi::CapabilityId;
use tairix_caps::CapabilitySet;

use crate::password::{PasswordRecord, Salt, StoredPassword};
use crate::ParseError;

/// Numeric user identifier. `uid == 0` carries **no**
/// ambient power; powers come from capabilities.
#[repr(transparent)]
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct Uid(pub u32);

/// Numeric group identifier.
#[repr(transparent)]
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct Gid(pub u32);

/// Longest username, in bytes.
pub const MAX_USERNAME_LEN: usize = 32;

/// Longest display name, in bytes.
pub const MAX_DISPLAY_NAME_LEN: usize = 64;

/// Longest home or shell path, in bytes.
pub const MAX_PATH_LEN: usize = 128;

/// Most supplementary groups one account may carry.
pub const MAX_SUPPLEMENTARY_GIDS: usize = 16;

/// The explicit stored spelling of an absent home or shell — a
/// non-interactive account states "none", never a fake path. It can
/// never collide with a real value: every stored path begins with `/`.
pub const NO_PATH_MARKER: &str = "none";

/// Whether an account may start sessions.
///
/// A locked account keeps its full record (identity, grants, password hash)
/// but authentication refuses it — indistinguishably from a wrong password,
/// so an attacker learns nothing from the lock. A no-login account states
/// the *intent* that it never starts a session: it carries no home, no
/// shell, and no password ([`StoredPassword::NeverAuthenticates`]), so it
/// is structurally incapable of one — fail closed by construction, not by
/// configuration.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum AccountState {
    /// The account may log in.
    Active,
    /// The account is administratively barred from logging in.
    Locked,
    /// A system or service identity that never starts a session.
    NoLogin,
}

impl AccountState {
    /// The stable on-disk spelling.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Locked => "locked",
            Self::NoLogin => "nologin",
        }
    }

    fn from_label(label: &str) -> Result<Self, ParseError> {
        match label {
            "active" => Ok(Self::Active),
            "locked" => Ok(Self::Locked),
            "nologin" => Ok(Self::NoLogin),
            _ => Err(ParseError::AccountState),
        }
    }
}

/// One validated user account.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UserRecord {
    username: String,
    uid: Uid,
    primary_gid: Gid,
    supplementary_gids: Vec<Gid>,
    display_name: String,
    home: Option<String>,
    shell: Option<String>,
    capabilities: CapabilitySet,
    state: AccountState,
    password: StoredPassword,
}

/// The non-secret identity fields of a [`UserRecord`], grouped so
/// constructors stay within clippy's argument budget and call sites read as
/// named fields.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Identity<'a> {
    /// Account name; see [`MAX_USERNAME_LEN`] and the charset rules.
    pub username: &'a str,
    /// Numeric user id, unique within a database.
    pub uid: Uid,
    /// Primary group.
    pub primary_gid: Gid,
    /// Supplementary groups, at most [`MAX_SUPPLEMENTARY_GIDS`].
    pub supplementary_gids: &'a [Gid],
    /// Human-readable name; may be empty.
    pub display_name: &'a str,
    /// Absolute home directory path; [`None`] only on a
    /// [`AccountState::NoLogin`] account.
    pub home: Option<&'a str>,
    /// Absolute path of the user's shell of choice; [`None`] only on a
    /// [`AccountState::NoLogin`] account.
    pub shell: Option<&'a str>,
    /// Capability grant ceiling.
    pub capabilities: CapabilitySet,
    /// Whether the account may start sessions.
    pub state: AccountState,
}

impl UserRecord {
    /// Build a record from validated parts.
    ///
    /// # Errors
    ///
    /// The matching [`ParseError`] if any identity field violates its
    /// bounds or charset, if `capabilities` contains an id `abi-v1` has
    /// not named (an unnamed grant could not be re-read from disk), or
    /// [`ParseError::AccountShape`] when the state and the home/shell/
    /// password presence disagree: a login-capable ([`AccountState::Active`]
    /// or [`AccountState::Locked`]) account requires all three; a
    /// [`AccountState::NoLogin`] account carries none of them.
    pub fn new(identity: Identity<'_>, password: StoredPassword) -> Result<Self, ParseError> {
        check_username(identity.username)?;
        check_display_name(identity.display_name)?;
        if let Some(home) = identity.home {
            check_path(home)?;
        }
        if let Some(shell) = identity.shell {
            check_path(shell)?;
        }
        let login_shaped = identity.home.is_some()
            && identity.shell.is_some()
            && matches!(password, StoredPassword::Password(_));
        let nologin_shaped = identity.home.is_none()
            && identity.shell.is_none()
            && password == StoredPassword::NeverAuthenticates;
        let well_shaped = match identity.state {
            AccountState::Active | AccountState::Locked => login_shaped,
            AccountState::NoLogin => nologin_shaped,
        };
        if !well_shaped {
            return Err(ParseError::AccountShape);
        }
        if identity.supplementary_gids.len() > MAX_SUPPLEMENTARY_GIDS {
            return Err(ParseError::SupplementaryGids);
        }
        if identity.capabilities.iter().any(|cap| cap.name().is_none()) {
            return Err(ParseError::Capability);
        }
        Ok(Self {
            username: String::from(identity.username),
            uid: identity.uid,
            primary_gid: identity.primary_gid,
            supplementary_gids: identity.supplementary_gids.to_vec(),
            display_name: String::from(identity.display_name),
            home: identity.home.map(String::from),
            shell: identity.shell.map(String::from),
            capabilities: identity.capabilities,
            state: identity.state,
            password,
        })
    }

    /// Build a record by hashing a fresh `password` under `salt` at
    /// `iterations` cost (see [`PasswordRecord::new`]).
    ///
    /// # Errors
    ///
    /// As [`Self::new`], plus [`ParseError::PasswordRecord`] for an invalid
    /// password length or cost.
    pub fn with_password(
        identity: Identity<'_>,
        password: &[u8],
        salt: Salt,
        iterations: u32,
    ) -> Result<Self, ParseError> {
        Self::new(
            identity,
            StoredPassword::Password(PasswordRecord::new(password, salt, iterations)?),
        )
    }

    /// Decode one database line.
    ///
    /// # Errors
    ///
    /// The matching [`ParseError`] for a wrong field count or any field
    /// that fails its validation.
    pub fn decode_line(line: &str) -> Result<Self, ParseError> {
        let mut fields = line.split(':');
        let mut next = || fields.next().ok_or(ParseError::FieldCount);
        let username = next()?;
        let uid = parse_u32(next()?).ok_or(ParseError::UserId)?;
        let primary_gid = parse_u32(next()?).ok_or(ParseError::GroupId)?;
        let supplementary = next()?;
        let display_name = next()?;
        let home = next()?;
        let shell = next()?;
        let caps = next()?;
        let state = AccountState::from_label(next()?)?;
        let password = StoredPassword::decode(next()?)?;
        if fields.next().is_some() {
            return Err(ParseError::FieldCount);
        }

        let supplementary_gids = parse_gid_list(supplementary)?;
        let capabilities = parse_capabilities(caps)?;
        Self::new(
            Identity {
                username,
                uid: Uid(uid),
                primary_gid: Gid(primary_gid),
                supplementary_gids: &supplementary_gids,
                display_name,
                home: parse_optional_path(home),
                shell: parse_optional_path(shell),
                capabilities,
                state,
            },
            password,
        )
    }

    /// Encode the record into the line form [`Self::decode_line`] accepts.
    #[must_use]
    pub fn encode_line(&self) -> String {
        let mut out = String::new();
        out.push_str(&self.username);
        push_field(&mut out, &decimal(self.uid.0));
        push_field(&mut out, &decimal(self.primary_gid.0));
        out.push(':');
        for (i, gid) in self.supplementary_gids.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str(&decimal(gid.0));
        }
        push_field(&mut out, &self.display_name);
        push_field(&mut out, self.home.as_deref().unwrap_or(NO_PATH_MARKER));
        push_field(&mut out, self.shell.as_deref().unwrap_or(NO_PATH_MARKER));
        out.push(':');
        let mut first = true;
        for cap in &self.capabilities {
            if let Some(name) = cap.name() {
                if !first {
                    out.push(',');
                }
                out.push_str(name);
                first = false;
            }
        }
        push_field(&mut out, self.state.label());
        push_field(&mut out, &self.password.encode());
        out
    }

    /// The account name.
    #[must_use]
    pub fn username(&self) -> &str {
        &self.username
    }

    /// The numeric user id.
    #[must_use]
    pub fn uid(&self) -> Uid {
        self.uid
    }

    /// The primary group.
    #[must_use]
    pub fn primary_gid(&self) -> Gid {
        self.primary_gid
    }

    /// The supplementary groups.
    #[must_use]
    pub fn supplementary_gids(&self) -> &[Gid] {
        &self.supplementary_gids
    }

    /// The human-readable name (may be empty).
    #[must_use]
    pub fn display_name(&self) -> &str {
        &self.display_name
    }

    /// The absolute home directory path; [`None`] only on a
    /// [`AccountState::NoLogin`] account.
    #[must_use]
    pub fn home(&self) -> Option<&str> {
        self.home.as_deref()
    }

    /// The absolute path of the user's shell of choice; [`None`] only on
    /// a [`AccountState::NoLogin`] account.
    #[must_use]
    pub fn shell(&self) -> Option<&str> {
        self.shell.as_deref()
    }

    /// The capability grant ceiling.
    #[must_use]
    pub fn capabilities(&self) -> CapabilitySet {
        self.capabilities
    }

    /// Whether the account may start sessions.
    #[must_use]
    pub fn state(&self) -> AccountState {
        self.state
    }

    /// The stored password: a real record, or the typed
    /// never-authenticates marker.
    #[must_use]
    pub fn password(&self) -> &StoredPassword {
        &self.password
    }
}

/// Whether `name` is a well-formed account/group identifier:
/// 1..=`max_len` bytes, first byte `[a-z_]`, the rest `[a-z0-9_-]`.
///
/// The single charset definition shared by the username check below and
/// the group-name check in `groups.rs`, so a user and a group obey the
/// one identifier grammar rather than two copies drifting apart. The
/// charset keeps an identifier unambiguous in paths, logs, and the
/// colon-separated database formats.
pub(crate) fn name_charset_ok(name: &str, max_len: usize) -> bool {
    let bytes = name.as_bytes();
    if bytes.is_empty() || bytes.len() > max_len {
        return false;
    }
    if !(bytes[0].is_ascii_lowercase() || bytes[0] == b'_') {
        return false;
    }
    bytes[1..]
        .iter()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'-'))
}

/// Validate a username against the shared identifier grammar
/// ([`name_charset_ok`]) within [`MAX_USERNAME_LEN`].
fn check_username(name: &str) -> Result<(), ParseError> {
    if name_charset_ok(name, MAX_USERNAME_LEN) {
        Ok(())
    } else {
        Err(ParseError::Username)
    }
}

/// Parse a canonically spelled `u32` group/user id: no sign, no leading
/// `+`, and no leading zeros (other than `"0"`), so each value has exactly
/// one accepted spelling. Shared with `groups.rs`.
pub(crate) fn parse_canonical_u32(text: &str) -> Option<u32> {
    parse_u32(text)
}

/// Validate a display name: at most [`MAX_DISPLAY_NAME_LEN`] bytes of
/// printable ASCII (space allowed), excluding the `:` field separator.
fn check_display_name(name: &str) -> Result<(), ParseError> {
    if name.len() > MAX_DISPLAY_NAME_LEN {
        return Err(ParseError::DisplayName);
    }
    if name
        .bytes()
        .all(|b| (0x20..=0x7e).contains(&b) && b != b':')
    {
        Ok(())
    } else {
        Err(ParseError::DisplayName)
    }
}

/// Validate a home/shell path: absolute, 2..=[`MAX_PATH_LEN`] bytes of
/// printable non-space ASCII, excluding the `:` field separator.
fn check_path(path: &str) -> Result<(), ParseError> {
    let bytes = path.as_bytes();
    if bytes.len() < 2 || bytes.len() > MAX_PATH_LEN || bytes[0] != b'/' {
        return Err(ParseError::Path);
    }
    if bytes
        .iter()
        .all(|b| (0x21..=0x7e).contains(b) && *b != b':')
    {
        Ok(())
    } else {
        Err(ParseError::Path)
    }
}

/// Decode a stored home/shell field: the explicit [`NO_PATH_MARKER`], or a
/// path the constructor then validates. Anything else is also handed to
/// path validation, which rejects it (a path must begin with `/`).
fn parse_optional_path(field: &str) -> Option<&str> {
    if field == NO_PATH_MARKER {
        None
    } else {
        Some(field)
    }
}

/// Parse a `u32` with no sign, no leading `+`, and no leading zeros (other
/// than `"0"` itself), so every value has exactly one accepted spelling.
fn parse_u32(text: &str) -> Option<u32> {
    if text.is_empty() || (text.len() > 1 && text.starts_with('0')) {
        return None;
    }
    if !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

/// Parse the comma-separated supplementary-gid list (empty allowed).
fn parse_gid_list(text: &str) -> Result<Vec<Gid>, ParseError> {
    if text.is_empty() {
        return Ok(Vec::new());
    }
    let mut gids = Vec::new();
    for field in text.split(',') {
        if gids.len() == MAX_SUPPLEMENTARY_GIDS {
            return Err(ParseError::SupplementaryGids);
        }
        let gid = Gid(parse_u32(field).ok_or(ParseError::GroupId)?);
        if gids.contains(&gid) {
            return Err(ParseError::SupplementaryGids);
        }
        gids.push(gid);
    }
    Ok(gids)
}

/// Parse the comma-separated `CAP_*` grant list (empty allowed), failing
/// closed on any name `abi-v1` does not define.
fn parse_capabilities(text: &str) -> Result<CapabilitySet, ParseError> {
    let mut set = CapabilitySet::empty();
    if text.is_empty() {
        return Ok(set);
    }
    for name in text.split(',') {
        let cap = CapabilityId::from_name(name).ok_or(ParseError::Capability)?;
        if set.contains(cap) {
            return Err(ParseError::Capability);
        }
        set.insert(cap);
    }
    Ok(set)
}

/// Render a `u32` as decimal without an allocator-heavy `format!`.
fn decimal(value: u32) -> String {
    let mut out = String::new();
    // Writing into a `String` cannot fail; `fmt::Write` is total here.
    let _ = core::fmt::Write::write_fmt(&mut out, format_args!("{value}"));
    out
}

/// Append a `:`-prefixed field.
fn push_field(out: &mut String, field: &str) {
    out.push(':');
    out.push_str(field);
}

#[cfg(test)]
mod tests {
    use super::{AccountState, Gid, Identity, Uid, UserRecord, MAX_SUPPLEMENTARY_GIDS};
    use crate::password::{PasswordRecord, StoredPassword, MIN_ITERATIONS};
    use crate::ParseError;

    use alloc::string::String;
    use alloc::vec::Vec;
    use tairix_abi::CapabilityId;
    use tairix_caps::CapabilitySet;

    fn caps(list: &[CapabilityId]) -> CapabilitySet {
        let mut set = CapabilitySet::empty();
        for cap in list {
            set.insert(*cap);
        }
        set
    }

    fn identity(supplementary: &[Gid]) -> Identity<'_> {
        Identity {
            username: "ada",
            uid: Uid(1000),
            primary_gid: Gid(1000),
            supplementary_gids: supplementary,
            display_name: "Ada Lovelace",
            home: Some("/Users/ada"),
            shell: Some("/System/Commands/elsh.app/Run"),
            capabilities: caps(&[CapabilityId::FS_MOUNT, CapabilityId::PROC_SPAWN]),
            state: AccountState::Active,
        }
    }

    fn password() -> StoredPassword {
        StoredPassword::Password(
            PasswordRecord::new(b"byron", [0x5A; 16], MIN_ITERATIONS).expect("valid"),
        )
    }

    fn no_login_identity() -> Identity<'static> {
        Identity {
            username: "devmgr",
            uid: Uid(10),
            primary_gid: Gid(101),
            supplementary_gids: &[],
            display_name: "Device Manager",
            home: None,
            shell: None,
            capabilities: CapabilitySet::empty(),
            state: AccountState::NoLogin,
        }
    }

    fn record() -> UserRecord {
        UserRecord::new(identity(&[Gid(4), Gid(7)]), password()).expect("valid record")
    }

    #[test]
    fn encode_decode_round_trips() {
        let original = record();
        let line = original.encode_line();
        assert_eq!(UserRecord::decode_line(&line), Ok(original));
    }

    #[test]
    fn accessors_expose_the_identity() {
        let record = record();
        assert_eq!(record.username(), "ada");
        assert_eq!(record.uid(), Uid(1000));
        assert_eq!(record.primary_gid(), Gid(1000));
        assert_eq!(record.supplementary_gids(), &[Gid(4), Gid(7)]);
        assert_eq!(record.display_name(), "Ada Lovelace");
        assert_eq!(record.home(), Some("/Users/ada"));
        assert_eq!(record.shell(), Some("/System/Commands/elsh.app/Run"));
        assert!(record.capabilities().contains(CapabilityId::FS_MOUNT));
        assert_eq!(record.state(), AccountState::Active);
        assert!(record.password().verify(b"byron"));
    }

    #[test]
    fn a_no_login_record_round_trips_with_the_explicit_markers() {
        let record = UserRecord::new(no_login_identity(), StoredPassword::NeverAuthenticates)
            .expect("valid record");
        let line = record.encode_line();
        assert_eq!(line, "devmgr:10:101::Device Manager:none:none::nologin:*");
        assert_eq!(UserRecord::decode_line(&line), Ok(record.clone()));
        assert_eq!(record.home(), None);
        assert_eq!(record.shell(), None);
        assert_eq!(record.state(), AccountState::NoLogin);
        assert!(!record.password().verify(b""));
        assert!(!record.password().verify(b"*"));
    }

    #[test]
    fn mismatched_state_and_shape_are_rejected() {
        // A login-capable state must carry home, shell, and a password.
        for state in [AccountState::Active, AccountState::Locked] {
            let mut id = no_login_identity();
            id.state = state;
            assert_eq!(
                UserRecord::new(id, StoredPassword::NeverAuthenticates),
                Err(ParseError::AccountShape),
                "accepted a bare {state:?} record"
            );
        }
        // A no-login account carries none of them, in any combination.
        let mut id = identity(&[]);
        id.state = AccountState::NoLogin;
        assert_eq!(
            UserRecord::new(id, password()),
            Err(ParseError::AccountShape)
        );
        let mut home_only = no_login_identity();
        home_only.home = Some("/Users/devmgr");
        assert_eq!(
            UserRecord::new(home_only, StoredPassword::NeverAuthenticates),
            Err(ParseError::AccountShape)
        );
        let mut shell_only = no_login_identity();
        shell_only.shell = Some("/System/Commands/elsh.app/Run");
        assert_eq!(
            UserRecord::new(shell_only, StoredPassword::NeverAuthenticates),
            Err(ParseError::AccountShape)
        );
        assert_eq!(
            UserRecord::new(no_login_identity(), password()),
            Err(ParseError::AccountShape)
        );
        // An active account with a real password but no home/shell is
        // equally malformed.
        let mut pathless = identity(&[]);
        pathless.home = None;
        pathless.shell = None;
        assert_eq!(
            UserRecord::new(pathless, password()),
            Err(ParseError::AccountShape)
        );
    }

    #[test]
    fn empty_optional_fields_round_trip() {
        let mut id = identity(&[]);
        id.display_name = "";
        id.capabilities = CapabilitySet::empty();
        let record = UserRecord::new(id, password()).expect("valid");
        let line = record.encode_line();
        assert_eq!(UserRecord::decode_line(&line), Ok(record));
    }

    #[test]
    fn bad_usernames_are_rejected() {
        for name in [
            "",
            "Ada",
            "1ada",
            "-ada",
            "ada lovelace",
            "ada:b",
            "Ada\u{e9}",
        ] {
            let mut id = identity(&[]);
            id.username = name;
            assert_eq!(
                UserRecord::new(id, password()),
                Err(ParseError::Username),
                "accepted username {name:?}"
            );
        }
        let long = "a".repeat(33);
        let mut id = identity(&[]);
        id.username = &long;
        assert_eq!(UserRecord::new(id, password()), Err(ParseError::Username));
    }

    #[test]
    fn bad_paths_are_rejected() {
        for path in ["", "/", "Apps/Run", "/with space", "/with:colon"] {
            let mut id = identity(&[]);
            id.shell = Some(path);
            assert_eq!(
                UserRecord::new(id, password()),
                Err(ParseError::Path),
                "accepted path {path:?}"
            );
        }
    }

    #[test]
    fn bad_display_names_are_rejected() {
        for name in ["with:colon", "ctrl\u{7}", "caf\u{e9}"] {
            let mut id = identity(&[]);
            id.display_name = name;
            assert_eq!(
                UserRecord::new(id, password()),
                Err(ParseError::DisplayName)
            );
        }
    }

    #[test]
    fn supplementary_gid_bounds_and_duplicates_are_enforced() {
        let many: Vec<Gid> = (0..=u32::try_from(MAX_SUPPLEMENTARY_GIDS).expect("fits"))
            .map(Gid)
            .collect();
        assert_eq!(
            UserRecord::new(identity(&many), password()),
            Err(ParseError::SupplementaryGids)
        );

        let line = record().encode_line().replace("4,7", "4,4");
        assert_eq!(
            UserRecord::decode_line(&line),
            Err(ParseError::SupplementaryGids)
        );
    }

    #[test]
    fn malformed_lines_are_rejected() {
        let good = record().encode_line();
        for (bad, expected) in [
            (String::new(), ParseError::FieldCount),
            (good.replace(":1000:", ":-1:"), ParseError::UserId),
            (good.replace(":1000:", ":01:"), ParseError::UserId),
            (
                good.replace("CAP_FS_MOUNT", "CAP_BOGUS"),
                ParseError::Capability,
            ),
            (
                good.replace("CAP_FS_MOUNT", "CAP_PROC_SPAWN"),
                ParseError::Capability,
            ),
            (good.replace("active", "dormant"), ParseError::AccountState),
            (good.clone() + ":extra", ParseError::FieldCount),
        ] {
            assert_eq!(UserRecord::decode_line(&bad), Err(expected), "line: {bad}");
        }
        let mut truncated = good.clone();
        truncated.truncate(good.rfind(':').expect("has fields"));
        assert_eq!(
            UserRecord::decode_line(&truncated),
            Err(ParseError::FieldCount)
        );
    }
}
