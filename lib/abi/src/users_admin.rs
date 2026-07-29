//! Typed user/group administration requests for the `users_admin` syscall
//! (`plans/CAPABILITY_USE.md` CU4).
//!
//! One versioned, length-prefixed little-endian record per call. The
//! decoder here is **structural only** — it bounds the whole request,
//! validates lengths, UTF-8, and capability/gid list shapes, and fails
//! closed on anything else. The *semantic* field rules (username charset,
//! path shape, password-record grammar, record-count budgets) have exactly
//! one home, the `users-v1` format in `lib/users`, and are applied
//! kernel-side through its validating constructors — never duplicated
//! here.
//!
//! Password material crosses this boundary only as a ready salted PBKDF2
//! record string built by the caller; no operation ever returns stored
//! password material, so the list responses are secret-free.
//!
//! # Wire layout
//!
//! Every request starts with the header
//! `version: u16 == USERS_ADMIN_VERSION`, `op: u16`, followed by the
//! operation's payload. Field encodings:
//!
//! * string — `u16` byte length, then that many UTF-8 bytes;
//! * gid list — `u16` count, then `count` × `u32`;
//! * grant list — `u16` count, then `count` × `u16` [`CapabilityId`] raw
//!   values (each validated by [`CapabilityId::from_raw`]);
//! * state — one byte, `0` = active, `1` = locked.
//!
//! Trailing bytes after a payload are rejected (a request is exactly one
//! record). The list responses are described on
//! [`UserEntry`] / [`GroupEntry`].

use crate::{CapabilityId, Errno};

/// Version of the `users_admin` request/response encoding.
pub const USERS_ADMIN_VERSION: u16 = 1;

/// Largest request record, in bytes, the kernel will copy in (validation
/// bound — a defence, not a capacity). A request describes at most one
/// account record, whose `users-v1` fields are line-bounded far below
/// this.
pub const USERS_ADMIN_MAX_REQUEST: usize = 1024;

/// Structural bound on a grant list's entry count. The real vocabulary is
/// the closed [`CapabilityId`] set (each entry is validated individually);
/// this merely stops a hostile count before any scan.
pub const USERS_ADMIN_MAX_GRANTS: usize = 256;

/// Structural bound on a supplementary-gid list's entry count. The
/// semantic bound (`MAX_SUPPLEMENTARY_GIDS`) lives in `lib/users` and is
/// enforced kernel-side; this merely stops a hostile count before any
/// scan.
pub const USERS_ADMIN_MAX_GIDS: usize = 256;

/// The operation discriminants carried in the request header.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u16)]
pub enum UsersAdminOp {
    /// Create a new active account from a full record.
    CreateUser = 1,
    /// Replace an existing account's non-security identity fields
    /// (groups, display name, home, shell).
    ModifyUser = 2,
    /// Delete an account.
    DeleteUser = 3,
    /// Lock or unlock an account.
    SetAccountState = 4,
    /// Replace an account's capability grant ceiling.
    SetGrants = 5,
    /// Replace an account's stored password record.
    SetPassword = 6,
    /// Create a group.
    CreateGroup = 7,
    /// Delete a group.
    DeleteGroup = 8,
    /// List every account's non-secret fields.
    ListUsers = 9,
    /// List every group.
    ListGroups = 10,
}

impl UsersAdminOp {
    /// Decode a raw discriminant, failing closed on an unknown value.
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] for a discriminant outside the closed set.
    pub const fn from_raw(raw: u16) -> Result<Self, Errno> {
        Ok(match raw {
            1 => Self::CreateUser,
            2 => Self::ModifyUser,
            3 => Self::DeleteUser,
            4 => Self::SetAccountState,
            5 => Self::SetGrants,
            6 => Self::SetPassword,
            7 => Self::CreateGroup,
            8 => Self::DeleteGroup,
            9 => Self::ListUsers,
            10 => Self::ListGroups,
            _ => return Err(Errno::OutOfRange),
        })
    }

    /// Raw on-wire discriminant.
    #[must_use]
    pub const fn as_u16(self) -> u16 {
        self as u16
    }
}

/// A fail-closed little-endian reader over a request/response buffer.
///
/// Every accessor either yields a validated value or an [`Errno`]; nothing
/// is ever read past the buffer's end.
struct Cursor<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Cursor<'a> {
    const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, at: 0 }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], Errno> {
        let end = self.at.checked_add(n).ok_or(Errno::LengthOutOfRange)?;
        if end > self.bytes.len() {
            return Err(Errno::LengthOutOfRange);
        }
        let out = &self.bytes[self.at..end];
        self.at = end;
        Ok(out)
    }

    fn u8(&mut self) -> Result<u8, Errno> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, Errno> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    fn u32(&mut self) -> Result<u32, Errno> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    /// A `u16`-length-prefixed UTF-8 string.
    fn str(&mut self) -> Result<&'a str, Errno> {
        let len = self.u16()? as usize;
        let bytes = self.take(len)?;
        core::str::from_utf8(bytes).map_err(|_| Errno::OutOfRange)
    }

    /// Whether every byte has been consumed (a request is exactly one
    /// record; trailing bytes are rejected).
    const fn exhausted(&self) -> bool {
        self.at == self.bytes.len()
    }
}

/// A fail-closed little-endian writer over a caller-supplied buffer.
///
/// Every append either fits or reports [`Errno::BufferTooSmall`]; nothing
/// is ever written past the buffer's end.
struct Writer<'a> {
    out: &'a mut [u8],
    at: usize,
}

impl<'a> Writer<'a> {
    fn new(out: &'a mut [u8]) -> Self {
        Self { out, at: 0 }
    }

    fn bytes(&mut self, bytes: &[u8]) -> Result<(), Errno> {
        let end = self
            .at
            .checked_add(bytes.len())
            .ok_or(Errno::BufferTooSmall)?;
        if end > self.out.len() {
            return Err(Errno::BufferTooSmall);
        }
        self.out[self.at..end].copy_from_slice(bytes);
        self.at = end;
        Ok(())
    }

    fn u8(&mut self, v: u8) -> Result<(), Errno> {
        self.bytes(&[v])
    }

    fn u16(&mut self, v: u16) -> Result<(), Errno> {
        self.bytes(&v.to_le_bytes())
    }

    fn u32(&mut self, v: u32) -> Result<(), Errno> {
        self.bytes(&v.to_le_bytes())
    }

    /// A `u16`-length-prefixed UTF-8 string; a string longer than a `u16`
    /// can carry is refused.
    fn str(&mut self, s: &str) -> Result<(), Errno> {
        let len = u16::try_from(s.len()).map_err(|_| Errno::LengthOutOfRange)?;
        self.u16(len)?;
        self.bytes(s.as_bytes())
    }

    const fn written(&self) -> usize {
        self.at
    }
}

/// A validated, borrowed capability-grant list.
///
/// Constructed only by a successful decode (every raw value passed
/// [`CapabilityId::from_raw`]) or from a caller's slice at encode time, so
/// iterating never yields an invalid id.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct GrantList<'a> {
    /// `2 * count` bytes of little-endian raw ids, each already validated.
    raw: &'a [u8],
}

impl<'a> GrantList<'a> {
    /// Number of entries.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.raw.len() / 2
    }

    /// Whether the list is empty.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.raw.is_empty()
    }

    /// Iterate the validated capability ids.
    ///
    /// Construction validated every entry, so the inner `from_raw` cannot
    /// refuse; `filter_map` expresses that without a panic path.
    pub fn iter(&self) -> impl Iterator<Item = CapabilityId> + 'a {
        self.raw
            .as_chunks::<2>()
            .0
            .iter()
            .filter_map(|b| CapabilityId::from_raw(u16::from_le_bytes([b[0], b[1]])).ok())
    }

    /// Decode a grant list at the cursor, validating the count bound and
    /// every id.
    fn decode(cur: &mut Cursor<'a>) -> Result<Self, Errno> {
        let count = cur.u16()? as usize;
        if count > USERS_ADMIN_MAX_GRANTS {
            return Err(Errno::LengthOutOfRange);
        }
        let raw = cur.take(count * 2)?;
        for b in raw.as_chunks::<2>().0 {
            CapabilityId::from_raw(u16::from_le_bytes([b[0], b[1]]))?;
        }
        Ok(Self { raw })
    }
}

/// A validated, borrowed supplementary-gid list.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct GidList<'a> {
    /// `4 * count` bytes of little-endian gids.
    raw: &'a [u8],
}

impl<'a> GidList<'a> {
    /// Number of entries.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.raw.len() / 4
    }

    /// Whether the list is empty.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.raw.is_empty()
    }

    /// Iterate the gids.
    pub fn iter(&self) -> impl Iterator<Item = u32> + 'a {
        self.raw
            .as_chunks::<4>()
            .0
            .iter()
            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    /// Decode a gid list at the cursor, validating the count bound.
    fn decode(cur: &mut Cursor<'a>) -> Result<Self, Errno> {
        let count = cur.u16()? as usize;
        if count > USERS_ADMIN_MAX_GIDS {
            return Err(Errno::LengthOutOfRange);
        }
        let raw = cur.take(count * 4)?;
        Ok(Self { raw })
    }
}

/// The full-record payload of a [`UsersAdminOp::CreateUser`] request.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct CreateUser<'a> {
    /// Account name (semantically validated kernel-side by `lib/users`).
    pub username: &'a str,
    /// Account uid.
    pub uid: u32,
    /// Primary group.
    pub primary_gid: u32,
    /// Supplementary groups.
    pub supplementary_gids: GidList<'a>,
    /// Human-readable display name (may be empty).
    pub display_name: &'a str,
    /// Absolute home directory path.
    pub home: &'a str,
    /// Absolute shell path.
    pub shell: &'a str,
    /// The account's capability grant ceiling.
    pub grants: GrantList<'a>,
    /// The ready salted PBKDF2 password record
    /// (`pbkdf2-sha256$<iterations>$<salt>$<hash>`), built by the caller.
    pub password_record: &'a str,
}

/// The identity-field payload of a [`UsersAdminOp::ModifyUser`] request:
/// a full replacement of the account's non-security fields. The uid,
/// grants, password, and state are deliberately absent — each has its own
/// operation (or, for the uid, none: an account's uid is its identity and
/// is never rewritten).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct ModifyUser<'a> {
    /// Account to modify.
    pub username: &'a str,
    /// New primary group.
    pub primary_gid: u32,
    /// New supplementary groups.
    pub supplementary_gids: GidList<'a>,
    /// New display name (may be empty).
    pub display_name: &'a str,
    /// New absolute home directory path.
    pub home: &'a str,
    /// New absolute shell path.
    pub shell: &'a str,
}

/// One decoded `users_admin` request.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum UsersAdminRequest<'a> {
    /// Create a new active account.
    CreateUser(CreateUser<'a>),
    /// Replace an account's non-security identity fields.
    ModifyUser(ModifyUser<'a>),
    /// Delete the named account.
    DeleteUser {
        /// Account to delete.
        username: &'a str,
    },
    /// Lock (`locked == true`) or unlock the named account.
    SetAccountState {
        /// Account to lock or unlock.
        username: &'a str,
        /// `true` to lock, `false` to reactivate.
        locked: bool,
    },
    /// Replace the named account's capability grant ceiling.
    SetGrants {
        /// Account whose ceiling is replaced.
        username: &'a str,
        /// The new ceiling.
        grants: GrantList<'a>,
    },
    /// Replace the named account's stored password record.
    SetPassword {
        /// Account whose password record is replaced.
        username: &'a str,
        /// The ready salted PBKDF2 record built by the caller.
        password_record: &'a str,
    },
    /// Create a group.
    CreateGroup {
        /// Group name (semantically validated kernel-side).
        name: &'a str,
        /// Group id.
        gid: u32,
    },
    /// Delete a group.
    DeleteGroup {
        /// Group to delete.
        name: &'a str,
    },
    /// List every account's non-secret fields.
    ListUsers,
    /// List every group.
    ListGroups,
}

impl<'a> UsersAdminRequest<'a> {
    /// Decode one request record, failing closed on any structural defect:
    /// a wrong version, an unknown operation, an out-of-bounds length, an
    /// invalid capability id, non-UTF-8 text, or trailing bytes.
    ///
    /// # Errors
    ///
    /// * [`Errno::LengthOutOfRange`] — the whole record exceeds
    ///   [`USERS_ADMIN_MAX_REQUEST`], a field runs past the buffer, a list
    ///   exceeds its structural count bound, or trailing bytes follow the
    ///   payload.
    /// * [`Errno::AbiVersionUnsupported`] — the version is not
    ///   [`USERS_ADMIN_VERSION`].
    /// * [`Errno::OutOfRange`] — an unknown operation, an invalid
    ///   capability id, a state byte outside the closed set, or non-UTF-8
    ///   text.
    pub fn decode(bytes: &'a [u8]) -> Result<Self, Errno> {
        if bytes.len() > USERS_ADMIN_MAX_REQUEST {
            return Err(Errno::LengthOutOfRange);
        }
        let mut cur = Cursor::new(bytes);
        if cur.u16()? != USERS_ADMIN_VERSION {
            return Err(Errno::AbiVersionUnsupported);
        }
        let op = UsersAdminOp::from_raw(cur.u16()?)?;
        let decoded = match op {
            UsersAdminOp::CreateUser => Self::CreateUser(CreateUser {
                username: cur.str()?,
                uid: cur.u32()?,
                primary_gid: cur.u32()?,
                supplementary_gids: GidList::decode(&mut cur)?,
                display_name: cur.str()?,
                home: cur.str()?,
                shell: cur.str()?,
                grants: GrantList::decode(&mut cur)?,
                password_record: cur.str()?,
            }),
            UsersAdminOp::ModifyUser => Self::ModifyUser(ModifyUser {
                username: cur.str()?,
                primary_gid: cur.u32()?,
                supplementary_gids: GidList::decode(&mut cur)?,
                display_name: cur.str()?,
                home: cur.str()?,
                shell: cur.str()?,
            }),
            UsersAdminOp::DeleteUser => Self::DeleteUser {
                username: cur.str()?,
            },
            UsersAdminOp::SetAccountState => Self::SetAccountState {
                username: cur.str()?,
                locked: match cur.u8()? {
                    0 => false,
                    1 => true,
                    _ => return Err(Errno::OutOfRange),
                },
            },
            UsersAdminOp::SetGrants => Self::SetGrants {
                username: cur.str()?,
                grants: GrantList::decode(&mut cur)?,
            },
            UsersAdminOp::SetPassword => Self::SetPassword {
                username: cur.str()?,
                password_record: cur.str()?,
            },
            UsersAdminOp::CreateGroup => Self::CreateGroup {
                name: cur.str()?,
                gid: cur.u32()?,
            },
            UsersAdminOp::DeleteGroup => Self::DeleteGroup { name: cur.str()? },
            UsersAdminOp::ListUsers => Self::ListUsers,
            UsersAdminOp::ListGroups => Self::ListGroups,
        };
        if !cur.exhausted() {
            return Err(Errno::LengthOutOfRange);
        }
        Ok(decoded)
    }

    /// The request's operation discriminant.
    #[must_use]
    pub const fn op(&self) -> UsersAdminOp {
        match self {
            Self::CreateUser(_) => UsersAdminOp::CreateUser,
            Self::ModifyUser(_) => UsersAdminOp::ModifyUser,
            Self::DeleteUser { .. } => UsersAdminOp::DeleteUser,
            Self::SetAccountState { .. } => UsersAdminOp::SetAccountState,
            Self::SetGrants { .. } => UsersAdminOp::SetGrants,
            Self::SetPassword { .. } => UsersAdminOp::SetPassword,
            Self::CreateGroup { .. } => UsersAdminOp::CreateGroup,
            Self::DeleteGroup { .. } => UsersAdminOp::DeleteGroup,
            Self::ListUsers => UsersAdminOp::ListUsers,
            Self::ListGroups => UsersAdminOp::ListGroups,
        }
    }

    /// Encode this request into `out`, returning the encoded length.
    ///
    /// The inverse of [`decode`](Self::decode); the caller (the `users`
    /// tool through `lib/rt`) builds its buffer with this so the two sides
    /// share one layout definition.
    ///
    /// # Errors
    ///
    /// [`Errno::BufferTooSmall`] when `out` cannot hold the record;
    /// [`Errno::LengthOutOfRange`] when a field exceeds what the wire
    /// format can carry.
    pub fn encode_into(&self, out: &mut [u8]) -> Result<usize, Errno> {
        let mut w = Writer::new(out);
        w.u16(USERS_ADMIN_VERSION)?;
        w.u16(self.op().as_u16())?;
        match self {
            Self::CreateUser(req) => {
                w.str(req.username)?;
                w.u32(req.uid)?;
                w.u32(req.primary_gid)?;
                encode_gids(&mut w, &req.supplementary_gids)?;
                w.str(req.display_name)?;
                w.str(req.home)?;
                w.str(req.shell)?;
                encode_grants(&mut w, &req.grants)?;
                w.str(req.password_record)?;
            }
            Self::ModifyUser(req) => {
                w.str(req.username)?;
                w.u32(req.primary_gid)?;
                encode_gids(&mut w, &req.supplementary_gids)?;
                w.str(req.display_name)?;
                w.str(req.home)?;
                w.str(req.shell)?;
            }
            Self::DeleteUser { username } => w.str(username)?,
            Self::SetAccountState { username, locked } => {
                w.str(username)?;
                w.u8(u8::from(*locked))?;
            }
            Self::SetGrants { username, grants } => {
                w.str(username)?;
                encode_grants(&mut w, grants)?;
            }
            Self::SetPassword {
                username,
                password_record,
            } => {
                w.str(username)?;
                w.str(password_record)?;
            }
            Self::CreateGroup { name, gid } => {
                w.str(name)?;
                w.u32(*gid)?;
            }
            Self::DeleteGroup { name } => w.str(name)?,
            Self::ListUsers | Self::ListGroups => {}
        }
        Ok(w.written())
    }
}

/// Build a [`GrantList`] from already-validated ids serialised into
/// `backing` (little-endian, two bytes per id).
///
/// The borrowed-wire shape cannot be built from a `&[CapabilityId]`
/// directly without allocating, so an encoding caller lays the ids into
/// its own `backing` buffer first.
///
/// # Errors
///
/// [`Errno::LengthOutOfRange`] when `ids` exceeds
/// [`USERS_ADMIN_MAX_GRANTS`] or `backing` is too short.
pub fn grant_list_into<'a>(
    ids: &[CapabilityId],
    backing: &'a mut [u8],
) -> Result<GrantList<'a>, Errno> {
    if ids.len() > USERS_ADMIN_MAX_GRANTS || backing.len() < ids.len() * 2 {
        return Err(Errno::LengthOutOfRange);
    }
    for (slot, id) in backing.as_chunks_mut::<2>().0.iter_mut().zip(ids.iter()) {
        slot.copy_from_slice(&id.as_u16().to_le_bytes());
    }
    Ok(GrantList {
        raw: &backing[..ids.len() * 2],
    })
}

/// Build a [`GidList`] from gids serialised into `backing` (little-endian,
/// four bytes per gid). The gid analogue of [`grant_list_into`].
///
/// # Errors
///
/// [`Errno::LengthOutOfRange`] when `gids` exceeds [`USERS_ADMIN_MAX_GIDS`]
/// or `backing` is too short.
pub fn gid_list_into<'a>(gids: &[u32], backing: &'a mut [u8]) -> Result<GidList<'a>, Errno> {
    if gids.len() > USERS_ADMIN_MAX_GIDS || backing.len() < gids.len() * 4 {
        return Err(Errno::LengthOutOfRange);
    }
    for (slot, gid) in backing.as_chunks_mut::<4>().0.iter_mut().zip(gids.iter()) {
        slot.copy_from_slice(&gid.to_le_bytes());
    }
    Ok(GidList {
        raw: &backing[..gids.len() * 4],
    })
}

fn encode_grants(w: &mut Writer<'_>, grants: &GrantList<'_>) -> Result<(), Errno> {
    let count = u16::try_from(grants.len()).map_err(|_| Errno::LengthOutOfRange)?;
    w.u16(count)?;
    w.bytes(grants.raw)
}

fn encode_gids(w: &mut Writer<'_>, gids: &GidList<'_>) -> Result<(), Errno> {
    let count = u16::try_from(gids.len()).map_err(|_| Errno::LengthOutOfRange)?;
    w.u16(count)?;
    w.bytes(gids.raw)
}

/// The account state a [`UserEntry`] reports, mirroring the `users-v1`
/// states (`lib/users`): a no-login system/service account is reported
/// truthfully, never disguised as merely locked or unlocked.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum AccountStateCode {
    /// The account may log in.
    Active = 0,
    /// The account is administratively barred from logging in.
    Locked = 1,
    /// A system/service identity that never starts a session.
    NoLogin = 2,
}

impl AccountStateCode {
    /// Decode the one-byte wire form, failing closed on anything unknown.
    fn decode(byte: u8) -> Result<Self, Errno> {
        match byte {
            0 => Ok(Self::Active),
            1 => Ok(Self::Locked),
            2 => Ok(Self::NoLogin),
            _ => Err(Errno::OutOfRange),
        }
    }
}

/// One account's non-secret fields in a [`UsersAdminOp::ListUsers`]
/// response.
///
/// The response is `version: u16`, `count: u16`, then `count` entries in
/// database order, each encoded with the request field primitives
/// (strings, gid list, grant list) plus the one-byte state. No password
/// material — not even the record's cost parameters — is ever included.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct UserEntry<'a> {
    /// Account name.
    pub username: &'a str,
    /// Account uid.
    pub uid: u32,
    /// Primary group.
    pub primary_gid: u32,
    /// Supplementary groups.
    pub supplementary_gids: GidList<'a>,
    /// Display name (may be empty).
    pub display_name: &'a str,
    /// Absolute home directory path, or the `users-v1` `none` marker on a
    /// no-login account.
    pub home: &'a str,
    /// Absolute shell path, or the `users-v1` `none` marker on a no-login
    /// account.
    pub shell: &'a str,
    /// The account's capability grant ceiling.
    pub grants: GrantList<'a>,
    /// The account's state.
    pub state: AccountStateCode,
}

impl<'a> UserEntry<'a> {
    /// Append this entry to a response being built in `w`.
    fn encode(&self, w: &mut Writer<'_>) -> Result<(), Errno> {
        w.str(self.username)?;
        w.u32(self.uid)?;
        w.u32(self.primary_gid)?;
        encode_gids(w, &self.supplementary_gids)?;
        w.str(self.display_name)?;
        w.str(self.home)?;
        w.str(self.shell)?;
        encode_grants(w, &self.grants)?;
        w.u8(self.state as u8)
    }

    /// Decode one entry at the cursor.
    fn decode(cur: &mut Cursor<'a>) -> Result<Self, Errno> {
        Ok(Self {
            username: cur.str()?,
            uid: cur.u32()?,
            primary_gid: cur.u32()?,
            supplementary_gids: GidList::decode(cur)?,
            display_name: cur.str()?,
            home: cur.str()?,
            shell: cur.str()?,
            grants: GrantList::decode(cur)?,
            state: AccountStateCode::decode(cur.u8()?)?,
        })
    }
}

/// One group in a [`UsersAdminOp::ListGroups`] response.
///
/// The response is `version: u16`, `count: u16`, then `count` entries in
/// database order.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct GroupEntry<'a> {
    /// Group name.
    pub name: &'a str,
    /// Group id.
    pub gid: u32,
}

impl<'a> GroupEntry<'a> {
    /// Append this entry to a response being built in `w`.
    fn encode(&self, w: &mut Writer<'_>) -> Result<(), Errno> {
        w.str(self.name)?;
        w.u32(self.gid)
    }

    /// Decode one entry at the cursor.
    fn decode(cur: &mut Cursor<'a>) -> Result<Self, Errno> {
        Ok(Self {
            name: cur.str()?,
            gid: cur.u32()?,
        })
    }
}

/// Incrementally encode a list response (`version`, `count`, entries)
/// into a caller-supplied buffer without allocating.
///
/// The kernel builds each entry from its held database and appends it;
/// [`finish`](Self::finish) back-patches the count and yields the byte
/// length. Whole-or-nothing: any overflow of `out` fails closed with
/// [`Errno::BufferTooSmall`] and nothing partial is served.
pub struct ListResponseBuilder<'a> {
    w: Writer<'a>,
    count: u16,
}

impl<'a> ListResponseBuilder<'a> {
    /// Start a response in `out`.
    ///
    /// # Errors
    ///
    /// [`Errno::BufferTooSmall`] when `out` cannot hold even the header.
    pub fn new(out: &'a mut [u8]) -> Result<Self, Errno> {
        let mut w = Writer::new(out);
        w.u16(USERS_ADMIN_VERSION)?;
        // Placeholder count, back-patched by `finish`.
        w.u16(0)?;
        Ok(Self { w, count: 0 })
    }

    /// Append one account entry.
    ///
    /// # Errors
    ///
    /// [`Errno::BufferTooSmall`] when the entry does not fit;
    /// [`Errno::LengthOutOfRange`] past `u16::MAX` entries.
    pub fn push_user(&mut self, entry: &UserEntry<'_>) -> Result<(), Errno> {
        entry.encode(&mut self.w)?;
        self.count = self.count.checked_add(1).ok_or(Errno::LengthOutOfRange)?;
        Ok(())
    }

    /// Append one group entry.
    ///
    /// # Errors
    ///
    /// [`Errno::BufferTooSmall`] when the entry does not fit;
    /// [`Errno::LengthOutOfRange`] past `u16::MAX` entries.
    pub fn push_group(&mut self, entry: &GroupEntry<'_>) -> Result<(), Errno> {
        entry.encode(&mut self.w)?;
        self.count = self.count.checked_add(1).ok_or(Errno::LengthOutOfRange)?;
        Ok(())
    }

    /// Back-patch the entry count and return the encoded byte length.
    #[must_use]
    pub fn finish(self) -> usize {
        let len = self.w.written();
        // The header was written by `new`, so slots 2..4 exist.
        self.w.out[2..4].copy_from_slice(&self.count.to_le_bytes());
        len
    }
}

/// Iterate the entries of a [`UsersAdminOp::ListUsers`] response.
///
/// # Errors
///
/// [`Errno::AbiVersionUnsupported`] on a version mismatch; the per-entry
/// structural errors of [`UserEntry`] surface from the iterator items
/// (fail closed at the first defect).
pub fn decode_user_list(bytes: &[u8]) -> Result<UserListIter<'_>, Errno> {
    let (cur, remaining) = decode_list_header(bytes)?;
    Ok(UserListIter { cur, remaining })
}

/// Iterate the entries of a [`UsersAdminOp::ListGroups`] response.
///
/// # Errors
///
/// [`Errno::AbiVersionUnsupported`] on a version mismatch; per-entry
/// structural errors surface from the iterator items.
pub fn decode_group_list(bytes: &[u8]) -> Result<GroupListIter<'_>, Errno> {
    let (cur, remaining) = decode_list_header(bytes)?;
    Ok(GroupListIter { cur, remaining })
}

fn decode_list_header(bytes: &[u8]) -> Result<(Cursor<'_>, u16), Errno> {
    let mut cur = Cursor::new(bytes);
    if cur.u16()? != USERS_ADMIN_VERSION {
        return Err(Errno::AbiVersionUnsupported);
    }
    let count = cur.u16()?;
    Ok((cur, count))
}

/// Iterator over decoded [`UserEntry`] records; yields an [`Errno`] and
/// then stops at the first structural defect (fail closed, never a
/// partial record).
pub struct UserListIter<'a> {
    cur: Cursor<'a>,
    remaining: u16,
}

impl<'a> Iterator for UserListIter<'a> {
    type Item = Result<UserEntry<'a>, Errno>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        self.remaining -= 1;
        let entry = UserEntry::decode(&mut self.cur);
        if entry.is_err() {
            // Fail closed: stop after surfacing the first defect.
            self.remaining = 0;
        }
        Some(entry)
    }
}

/// Iterator over decoded [`GroupEntry`] records; the group analogue of
/// [`UserListIter`].
pub struct GroupListIter<'a> {
    cur: Cursor<'a>,
    remaining: u16,
}

impl<'a> Iterator for GroupListIter<'a> {
    type Item = Result<GroupEntry<'a>, Errno>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        self.remaining -= 1;
        let entry = GroupEntry::decode(&mut self.cur);
        if entry.is_err() {
            self.remaining = 0;
        }
        Some(entry)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(req: &UsersAdminRequest<'_>) {
        let mut buf = [0u8; USERS_ADMIN_MAX_REQUEST];
        let len = req.encode_into(&mut buf).expect("encodes");
        assert_eq!(UsersAdminRequest::decode(&buf[..len]).as_ref(), Ok(req));
    }

    #[test]
    fn every_operation_round_trips() {
        let mut grant_backing = [0u8; 8];
        let grants = grant_list_into(
            &[CapabilityId::FS_ACCESS, CapabilityId::PROC_SPAWN],
            &mut grant_backing,
        )
        .expect("grants fit");
        let mut gid_backing = [0u8; 8];
        let gids = gid_list_into(&[100, 4321], &mut gid_backing).expect("gids fit");

        round_trip(&UsersAdminRequest::CreateUser(CreateUser {
            username: "ada",
            uid: 1000,
            primary_gid: 1000,
            supplementary_gids: gids,
            display_name: "Ada Lovelace",
            home: "/Users/ada",
            shell: "/System/Apps/elsh.app/Run",
            grants,
            password_record: "pbkdf2-sha256$600000$00$11",
        }));
        round_trip(&UsersAdminRequest::ModifyUser(ModifyUser {
            username: "ada",
            primary_gid: 1001,
            supplementary_gids: gids,
            display_name: "",
            home: "/Users/ada",
            shell: "/System/Apps/elsh.app/Run",
        }));
        round_trip(&UsersAdminRequest::DeleteUser { username: "ada" });
        round_trip(&UsersAdminRequest::SetAccountState {
            username: "ada",
            locked: true,
        });
        round_trip(&UsersAdminRequest::SetGrants {
            username: "ada",
            grants,
        });
        round_trip(&UsersAdminRequest::SetPassword {
            username: "ada",
            password_record: "pbkdf2-sha256$600000$00$11",
        });
        round_trip(&UsersAdminRequest::CreateGroup {
            name: "staff",
            gid: 100,
        });
        round_trip(&UsersAdminRequest::DeleteGroup { name: "staff" });
        round_trip(&UsersAdminRequest::ListUsers);
        round_trip(&UsersAdminRequest::ListGroups);
    }

    #[test]
    fn wrong_version_unknown_op_and_trailing_bytes_are_rejected() {
        // Version 2 does not exist.
        assert_eq!(
            UsersAdminRequest::decode(&[2, 0, 9, 0]),
            Err(Errno::AbiVersionUnsupported)
        );
        // Op 0 and op 11 are outside the closed set.
        assert_eq!(
            UsersAdminRequest::decode(&[1, 0, 0, 0]),
            Err(Errno::OutOfRange)
        );
        assert_eq!(
            UsersAdminRequest::decode(&[1, 0, 11, 0]),
            Err(Errno::OutOfRange)
        );
        // A trailing byte after a complete payload is refused.
        assert_eq!(
            UsersAdminRequest::decode(&[1, 0, 9, 0, 0]),
            Err(Errno::LengthOutOfRange)
        );
        // A truncated header is refused.
        assert_eq!(
            UsersAdminRequest::decode(&[1, 0, 9]),
            Err(Errno::LengthOutOfRange)
        );
    }

    #[test]
    fn oversized_requests_and_hostile_lengths_are_rejected() {
        let oversized = [0u8; USERS_ADMIN_MAX_REQUEST + 1];
        assert_eq!(
            UsersAdminRequest::decode(&oversized),
            Err(Errno::LengthOutOfRange)
        );
        // DeleteUser whose string length runs past the buffer.
        let mut short = [0u8; 8];
        short[..4].copy_from_slice(&[1, 0, 3, 0]);
        short[4..6].copy_from_slice(&u16::MAX.to_le_bytes());
        assert_eq!(
            UsersAdminRequest::decode(&short),
            Err(Errno::LengthOutOfRange)
        );
        // Non-UTF-8 text is refused.
        let mut bad = [0u8; 7];
        bad[..4].copy_from_slice(&[1, 0, 3, 0]);
        bad[4..6].copy_from_slice(&1u16.to_le_bytes());
        bad[6] = 0xFF;
        assert_eq!(UsersAdminRequest::decode(&bad), Err(Errno::OutOfRange));
    }

    #[test]
    fn an_invalid_capability_id_in_a_grant_list_is_rejected() {
        // SetGrants with one grant entry of raw id 0xFFFF.
        let mut buf = [0u8; 16];
        buf[..4].copy_from_slice(&[1, 0, 5, 0]);
        buf[4..6].copy_from_slice(&3u16.to_le_bytes());
        buf[6..9].copy_from_slice(b"ada");
        buf[9..11].copy_from_slice(&1u16.to_le_bytes());
        buf[11..13].copy_from_slice(&u16::MAX.to_le_bytes());
        assert!(UsersAdminRequest::decode(&buf[..13]).is_err());
    }

    #[test]
    fn a_hostile_lock_state_byte_is_rejected() {
        let mut buf = [0u8; 16];
        buf[..4].copy_from_slice(&[1, 0, 4, 0]);
        buf[4..6].copy_from_slice(&3u16.to_le_bytes());
        buf[6..9].copy_from_slice(b"ada");
        buf[9] = 2;
        assert_eq!(
            UsersAdminRequest::decode(&buf[..10]),
            Err(Errno::OutOfRange)
        );
    }

    #[test]
    fn list_responses_round_trip_and_fail_closed_on_truncation() {
        let mut grant_backing = [0u8; 4];
        let grants =
            grant_list_into(&[CapabilityId::USER_ADMIN], &mut grant_backing).expect("fits");
        let mut gid_backing = [0u8; 4];
        let gids = gid_list_into(&[7], &mut gid_backing).expect("fits");
        let entry = UserEntry {
            username: "root",
            uid: 0,
            primary_gid: 0,
            supplementary_gids: gids,
            display_name: "System Administrator",
            home: "/Users/root",
            shell: "/System/Apps/elsh.app/Run",
            grants,
            state: AccountStateCode::Locked,
        };

        let mut out = [0u8; 256];
        let mut b = ListResponseBuilder::new(&mut out).expect("header fits");
        b.push_user(&entry).expect("entry fits");
        let len = b.finish();

        let mut iter = decode_user_list(&out[..len]).expect("decodes");
        assert_eq!(iter.next(), Some(Ok(entry)));
        assert_eq!(iter.next(), None);

        // Every state code round-trips; an unknown byte is refused.
        for state in [
            AccountStateCode::Active,
            AccountStateCode::Locked,
            AccountStateCode::NoLogin,
        ] {
            let mut buf = [0u8; 256];
            let mut b = ListResponseBuilder::new(&mut buf).expect("header fits");
            b.push_user(&UserEntry { state, ..entry }).expect("fits");
            let n = b.finish();
            let mut iter = decode_user_list(&buf[..n]).expect("decodes");
            assert_eq!(iter.next(), Some(Ok(UserEntry { state, ..entry })));
        }
        let mut buf = [0u8; 256];
        let mut b = ListResponseBuilder::new(&mut buf).expect("header fits");
        b.push_user(&entry).expect("fits");
        let n = b.finish();
        buf[n - 1] = 3;
        let mut iter = decode_user_list(&buf[..n]).expect("header decodes");
        assert_eq!(iter.next(), Some(Err(Errno::OutOfRange)));

        // Truncating the encoded body surfaces an error, then stops.
        let mut truncated = decode_user_list(&out[..len - 1]).expect("header decodes");
        assert!(matches!(truncated.next(), Some(Err(_))));
        assert_eq!(truncated.next(), None);

        // A too-small output buffer fails closed at push time.
        let mut tiny = [0u8; 8];
        let mut b = ListResponseBuilder::new(&mut tiny).expect("header fits");
        assert_eq!(b.push_user(&entry), Err(Errno::BufferTooSmall));
    }

    #[test]
    fn group_list_responses_round_trip() {
        let mut out = [0u8; 64];
        let mut b = ListResponseBuilder::new(&mut out).expect("header fits");
        b.push_group(&GroupEntry {
            name: "system",
            gid: 0,
        })
        .expect("fits");
        b.push_group(&GroupEntry {
            name: "staff",
            gid: 100,
        })
        .expect("fits");
        let len = b.finish();

        let mut iter = decode_group_list(&out[..len]).expect("decodes");
        assert_eq!(
            iter.next(),
            Some(Ok(GroupEntry {
                name: "system",
                gid: 0
            }))
        );
        assert_eq!(
            iter.next(),
            Some(Ok(GroupEntry {
                name: "staff",
                gid: 100
            }))
        );
        assert_eq!(iter.next(), None);
    }
}
