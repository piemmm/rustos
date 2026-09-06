//! The graphical-login session protocol (`session-v1`,
//! `plans/NEW-DESKTOP-LOGIN.md` G4): the reserved rendezvous the **session
//! authority** (`login`) binds, and the two questions the graphical login
//! screen is allowed to ask it.
//!
//! The greeter draws and types; the authority verifies and starts. Neither
//! does the other's job, because the greeter links a whole image-decoding
//! and drawing stack over untrusted bytes and the authority holds the two
//! most dangerous grants on the machine. This protocol is the narrow seam
//! between them:
//!
//! * [`SessionRequest::Accounts`] pages the machine's login-able accounts.
//!   A record carries only what a tile draws — a display name, a login
//!   name, and whether that account already has a live session — never a
//!   password hash, a uid, a capability ceiling, or a home path.
//! * [`SessionRequest::Authenticate`] offers one account's secret and gets
//!   back a verdict. It starts nothing: the authority chooses what runs, on
//!   its own loop, so a compromised greeter cannot pick the program that
//!   runs as the authenticated user.
//! * [`SessionRequest::Background`] is the *desktop session's* one request,
//!   not the greeter's: "I am giving up the screen, put the login screen
//!   back up". It is the switch-away half of fast user switching, and the
//!   authority honours it only from the session it currently records as the
//!   foreground one.
//!
//! # Security posture
//!
//! The endpoint is reserved ([`crate::ipc::is_reserved_endpoint`]), so only
//! a `CAP_IPC_BIND_PRIVILEGED` holder can serve it and a squatter cannot
//! impersonate the authority. Callers are not trusted: the authority
//! attests every caller's uid and console from the kernel
//! ([`crate::Origin`], `call_peer_origin`) and refuses anyone but the
//! greeter service account on its own console, exactly as the elevation
//! broker does.
//!
//! Every refusal is **one** answer. [`SessionVerdict::Refused`] carries no
//! reason, so an unknown account, a wrong password, a locked account, and
//! an authority that cannot read its database are indistinguishable to the
//! caller; why a login was refused belongs in the authority's audit trail,
//! not in a reply an attacker can read. The refusal does carry
//! `retry_after`, the remaining per-account cooldown, because a screen that
//! cannot say "wait 30 seconds" would leave the user pressing a key that
//! silently does nothing.
//!
//! The request carries the offered secret across the kernel-copied IPC
//! buffer — the same trust boundary as typing it at the text prompt — and
//! both ends zeroise their copies as soon as the exchange resolves.

use crate::bounded_text::BoundedText;
use crate::le::{put_u16, put_u32, read_u16, read_u32};
use crate::time::Duration64;
use crate::Errno;

/// Reserved well-known call-endpoint id of the session authority's
/// `session-v1` service (`"SE"` ASCII hex-spelled prefix, the sibling of
/// [`crate::switchboard_ipc::SWITCHBOARD_ENDPOINT`] and
/// [`crate::seat::SEATMGR_ENDPOINT`]).
///
/// Bound by `login`, which holds `CAP_IPC_BIND_PRIVILEGED`. It is reserved
/// rather than seat-scoped ([`crate::ipc::is_seat_scoped_endpoint`]): the
/// authority owns no seat, and a process that merely held the seat lease
/// must never be able to serve the rendezvous that hands out account names
/// and adjudicates passwords.
pub const SESSION_ENDPOINT: u64 = 0x5345_1001;

/// Protocol version carried by every frame; any other value is refused at
/// decode rather than guessed.
pub const SESSION_VERSION: u16 = 1;

/// Magic word identifying a `session-v1` request (`"SES1"` little-endian).
pub const SESSION_REQUEST_MAGIC: u32 = u32::from_le_bytes(*b"SES1");

/// Magic word identifying an account-page reply (`"SEA1"` little-endian).
///
/// Deliberately distinct from [`SESSION_VERDICT_MAGIC`]: a client knows
/// which frame it asked for, so a reply of the wrong shape is a protocol
/// error caught at the first four bytes rather than misread as the other.
pub const SESSION_ACCOUNTS_MAGIC: u32 = u32::from_le_bytes(*b"SEA1");

/// Magic word identifying an authentication-verdict reply (`"SEV1"`
/// little-endian).
pub const SESSION_VERDICT_MAGIC: u32 = u32::from_le_bytes(*b"SEV1");

/// Longest login name `session-v1` carries, in bytes.
///
/// A fail-closed wire bound. The user database's own username limit
/// (`tairix_users::MAX_USERNAME_LEN`) must not exceed it, or an account
/// with a legal name could not be offered a graphical login; the authority
/// asserts that relation at compile time, since only it depends on both.
pub const SESSION_LOGIN_NAME_MAX: usize = 32;

/// Longest display name `session-v1` carries, in bytes.
pub const SESSION_DISPLAY_NAME_MAX: usize = 64;

/// Longest secret one [`SessionRequest::Authenticate`] carries, in bytes.
///
/// A fail-closed memory bound, not a password policy: it is what the login
/// screen's own pre-reserved field holds, so a secret that reached the
/// field always fits the wire.
pub const SESSION_SECRET_MAX: usize = 256;

/// Accounts carried by one [`SessionRequest::Accounts`] page.
///
/// The list is paged rather than sent whole because a machine may have far
/// more accounts than any single reply could hold; a client walks the pages
/// until it has [`AccountPage::total`] of them.
pub const SESSION_ACCOUNTS_PER_PAGE: usize = 16;

/// Encoded length of one account record: two fixed-width validated names,
/// a flags byte, and one reserved byte.
pub const SESSION_ACCOUNT_RECORD_LEN: usize =
    1 + SESSION_DISPLAY_NAME_MAX + 1 + SESSION_LOGIN_NAME_MAX + 2;

/// Encoded length of an account page's fixed header.
pub const SESSION_ACCOUNTS_HEADER_LEN: usize = 16;

/// Largest encoded account page: the header plus a full page of records.
/// Also the endpoint's maximum reply size, since it is the longer of the
/// two reply frames.
pub const SESSION_MAX_REPLY: usize =
    SESSION_ACCOUNTS_HEADER_LEN + SESSION_ACCOUNTS_PER_PAGE * SESSION_ACCOUNT_RECORD_LEN;

/// Exact encoded length of an authentication verdict.
pub const SESSION_VERDICT_LEN: usize = 8 + Duration64::WIRE_LEN;

/// Largest encoded request — also the endpoint's maximum request size: the
/// fixed header, a login name, and a secret, each length-prefixed.
pub const SESSION_MAX_REQUEST: usize = 8 + 2 + SESSION_LOGIN_NAME_MAX + 2 + SESSION_SECRET_MAX;

/// Wire opcode naming a [`SessionRequest::Accounts`] request.
const OPCODE_ACCOUNTS: u8 = 0;
/// Wire opcode naming a [`SessionRequest::Authenticate`] request.
const OPCODE_AUTHENTICATE: u8 = 1;
/// Wire opcode naming a [`SessionRequest::Background`] request.
const OPCODE_BACKGROUND: u8 = 2;

/// Wire status naming an [`SessionVerdict::Accepted`] verdict.
const STATUS_ACCEPTED: u8 = 0;
/// Wire status naming a [`SessionVerdict::Refused`] verdict.
const STATUS_REFUSED: u8 = 1;

/// Record flag: the account already has a live desktop session, so
/// authenticating returns to it rather than starting a second one.
const FLAG_LIVE_SESSION: u8 = 1 << 0;

/// A display name as it crosses the wire: validated, bounded, control-free.
type DisplayName = BoundedText<0, SESSION_DISPLAY_NAME_MAX>;

/// A login name as it crosses the wire.
type LoginName = BoundedText<0, SESSION_LOGIN_NAME_MAX>;

/// One question the login screen asks the session authority.
///
/// The strings are shape-checked here — well-formed UTF-8, non-empty,
/// within their bounds. Whether the account exists, whether the secret is
/// its secret, and whether this caller may ask at all are the authority's
/// to decide, and it refuses every failure indistinguishably.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum SessionRequest<'a> {
    /// Page the machine's login-able accounts, starting at `offset`.
    Accounts {
        /// Index of the first account wanted, counting from zero.
        offset: u32,
    },
    /// Whether `password` authenticates `username`. Starts nothing.
    Authenticate {
        /// The account whose secret is offered.
        username: &'a str,
        /// The offered secret. Never logged, never echoed in a reply.
        password: &'a str,
    },
    /// Step the calling desktop session aside so the login screen can come
    /// back up, leaving it running in the background to be resumed.
    ///
    /// Sent by a desktop session, never by the greeter. The authority
    /// identifies the caller from the kernel and honours this only from the
    /// session it records as the foreground one, so no other process can
    /// take the screen away from the person using it. Answered with
    /// [`SessionVerdict::Accepted`] once the session is recorded as
    /// background — the caller releases the seat and parks on its wake
    /// mailbox — or [`SessionVerdict::Refused`] if it is not the session
    /// holding the screen.
    Background,
}

impl<'a> SessionRequest<'a> {
    /// Encode the request little-endian into `out`, returning the encoded
    /// length.
    ///
    /// # Errors
    ///
    /// [`Errno::LengthOutOfRange`] when a string field is empty or longer
    /// than its bound; [`Errno::BufferTooSmall`] when `out` cannot hold the
    /// frame.
    pub fn encode(&self, out: &mut [u8]) -> Result<usize, Errno> {
        let body = match *self {
            Self::Accounts { .. } => 4,
            Self::Background => 0,
            Self::Authenticate { username, password } => {
                check_field(username, SESSION_LOGIN_NAME_MAX)?;
                check_field(password, SESSION_SECRET_MAX)?;
                2 + username.len() + 2 + password.len()
            }
        };
        let total = 8 + body;
        if out.len() < total {
            return Err(Errno::BufferTooSmall);
        }
        put_u32(out, 0, SESSION_REQUEST_MAGIC);
        put_u16(out, 4, SESSION_VERSION);
        out[7] = 0;
        match *self {
            Self::Accounts { offset } => {
                out[6] = OPCODE_ACCOUNTS;
                put_u32(out, 8, offset);
            }
            Self::Authenticate { username, password } => {
                out[6] = OPCODE_AUTHENTICATE;
                let at = put_str(out, 8, username)?;
                put_str(out, at, password)?;
            }
            Self::Background => out[6] = OPCODE_BACKGROUND,
        }
        Ok(total)
    }

    /// Decode a request, failing closed on any malformation: a wrong magic
    /// or version, a dirty reserved byte, an unknown opcode, an over-long
    /// buffer, a field running past the end, non-UTF-8 bytes, an empty or
    /// over-long field, or trailing bytes.
    ///
    /// # Errors
    ///
    /// [`Errno::BadMagic`] for a wrong magic or a dirty reserved byte,
    /// [`Errno::OutOfRange`] for a wrong version, an unknown opcode, or
    /// invalid UTF-8, [`Errno::LengthOutOfRange`] for every length
    /// violation.
    pub fn decode(bytes: &'a [u8]) -> Result<Self, Errno> {
        if bytes.len() > SESSION_MAX_REQUEST || bytes.len() < 8 {
            return Err(Errno::LengthOutOfRange);
        }
        if read_u32(bytes, 0) != SESSION_REQUEST_MAGIC || bytes[7] != 0 {
            return Err(Errno::BadMagic);
        }
        if read_u16(bytes, 4) != SESSION_VERSION {
            return Err(Errno::OutOfRange);
        }
        let mut cur = Cursor::new(bytes, 8);
        let request = match bytes[6] {
            OPCODE_ACCOUNTS => Self::Accounts { offset: cur.u32()? },
            OPCODE_AUTHENTICATE => {
                let username = cur.str(SESSION_LOGIN_NAME_MAX)?;
                let password = cur.str(SESSION_SECRET_MAX)?;
                Self::Authenticate { username, password }
            }
            OPCODE_BACKGROUND => Self::Background,
            _ => return Err(Errno::OutOfRange),
        };
        if !cur.exhausted() {
            return Err(Errno::LengthOutOfRange);
        }
        Ok(request)
    }
}

/// One account as the login screen draws it.
///
/// Exactly what a tile shows and nothing else. A uid, a capability
/// ceiling, a home path, or anything derived from the stored password
/// would be an enumeration aid handed to a process that only needs to
/// paint a name.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct SessionAccount {
    display_name: DisplayName,
    login_name: LoginName,
    live: bool,
}

impl SessionAccount {
    /// The zero record an undecoded page slot holds.
    const EMPTY: Self = Self {
        display_name: DisplayName::EMPTY,
        login_name: LoginName::EMPTY,
        live: false,
    };

    /// Build a record for `login_name`, shown as `display_name`.
    ///
    /// # Errors
    ///
    /// [`Errno::LengthOutOfRange`] when either name is empty or longer than
    /// its bound; [`Errno::OutOfRange`] when either contains a control
    /// character.
    pub fn new(display_name: &str, login_name: &str, live: bool) -> Result<Self, Errno> {
        if display_name.is_empty() || login_name.is_empty() {
            return Err(Errno::LengthOutOfRange);
        }
        Ok(Self {
            display_name: DisplayName::new(display_name)?,
            login_name: LoginName::new(login_name)?,
            live,
        })
    }

    /// The name shown on the tile.
    #[must_use]
    pub fn display_name(&self) -> &str {
        self.display_name.as_str()
    }

    /// The name an [`SessionRequest::Authenticate`] carries for this
    /// account.
    #[must_use]
    pub fn login_name(&self) -> &str {
        self.login_name.as_str()
    }

    /// Whether the account already has a live desktop session.
    #[must_use]
    pub const fn has_live_session(&self) -> bool {
        self.live
    }

    /// Encode the record into the [`SESSION_ACCOUNT_RECORD_LEN`] bytes at
    /// `out`.
    fn encode(&self, out: &mut [u8; SESSION_ACCOUNT_RECORD_LEN]) {
        out[0] = self.display_name.len_byte();
        out[1..=SESSION_DISPLAY_NAME_MAX].copy_from_slice(self.display_name.raw_bytes());
        let login_at = 1 + SESSION_DISPLAY_NAME_MAX;
        out[login_at] = self.login_name.len_byte();
        out[login_at + 1..login_at + 1 + SESSION_LOGIN_NAME_MAX]
            .copy_from_slice(self.login_name.raw_bytes());
        out[SESSION_ACCOUNT_RECORD_LEN - 2] = u8::from(self.live) * FLAG_LIVE_SESSION;
        out[SESSION_ACCOUNT_RECORD_LEN - 1] = 0;
    }

    /// Decode one record, refusing an empty name, an undefined flag bit, or
    /// a dirty reserved byte.
    fn decode(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() != SESSION_ACCOUNT_RECORD_LEN {
            return Err(Errno::LengthOutOfRange);
        }
        let flags = bytes[SESSION_ACCOUNT_RECORD_LEN - 2];
        if flags & !FLAG_LIVE_SESSION != 0 || bytes[SESSION_ACCOUNT_RECORD_LEN - 1] != 0 {
            return Err(Errno::BadMagic);
        }
        let mut display = [0u8; SESSION_DISPLAY_NAME_MAX];
        display.copy_from_slice(&bytes[1..=SESSION_DISPLAY_NAME_MAX]);
        let login_at = 1 + SESSION_DISPLAY_NAME_MAX;
        let mut login = [0u8; SESSION_LOGIN_NAME_MAX];
        login.copy_from_slice(&bytes[login_at + 1..login_at + 1 + SESSION_LOGIN_NAME_MAX]);
        if bytes[0] == 0 || bytes[login_at] == 0 {
            return Err(Errno::LengthOutOfRange);
        }
        Ok(Self {
            display_name: DisplayName::from_wire(bytes[0], &display)?,
            login_name: LoginName::from_wire(bytes[login_at], &login)?,
            live: flags & FLAG_LIVE_SESSION != 0,
        })
    }
}

/// One page of the machine's login-able accounts.
///
/// `total` is the whole list's length, so a client knows whether to ask for
/// another page; `offset` is where this page starts in it. A page is
/// validated whole at decode — a single malformed record refuses the page
/// rather than yielding a partly-trusted list.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AccountPage {
    total: u32,
    offset: u32,
    count: u8,
    accounts: [SessionAccount; SESSION_ACCOUNTS_PER_PAGE],
}

impl AccountPage {
    /// How many accounts the machine has in total, of which this page
    /// carries [`accounts`](Self::accounts).
    #[must_use]
    pub const fn total(&self) -> u32 {
        self.total
    }

    /// Index of this page's first account in the whole list.
    #[must_use]
    pub const fn offset(&self) -> u32 {
        self.offset
    }

    /// The accounts this page carries.
    #[must_use]
    pub fn accounts(&self) -> &[SessionAccount] {
        &self.accounts[..self.count as usize]
    }

    /// Whether the whole list has been walked once this page is consumed.
    #[must_use]
    pub fn is_last(&self) -> bool {
        u64::from(self.offset) + u64::from(self.count) >= u64::from(self.total)
    }
}

/// Encode an account page into `out`, returning the encoded length.
///
/// # Errors
///
/// [`Errno::LengthOutOfRange`] when `accounts` is longer than
/// [`SESSION_ACCOUNTS_PER_PAGE`], or when `offset` plus the page length
/// would run past `total`; [`Errno::BufferTooSmall`] when `out` cannot hold
/// the frame.
pub fn encode_account_page(
    out: &mut [u8],
    total: u32,
    offset: u32,
    accounts: &[SessionAccount],
) -> Result<usize, Errno> {
    let count = u8::try_from(accounts.len()).map_err(|_| Errno::LengthOutOfRange)?;
    if accounts.len() > SESSION_ACCOUNTS_PER_PAGE {
        return Err(Errno::LengthOutOfRange);
    }
    if u64::from(offset) + u64::from(count) > u64::from(total) {
        return Err(Errno::LengthOutOfRange);
    }
    let len = SESSION_ACCOUNTS_HEADER_LEN + accounts.len() * SESSION_ACCOUNT_RECORD_LEN;
    if out.len() < len {
        return Err(Errno::BufferTooSmall);
    }
    put_u32(out, 0, SESSION_ACCOUNTS_MAGIC);
    put_u16(out, 4, SESSION_VERSION);
    out[6] = count;
    out[7] = 0;
    put_u32(out, 8, total);
    put_u32(out, 12, offset);
    for (index, account) in accounts.iter().enumerate() {
        let at = SESSION_ACCOUNTS_HEADER_LEN + index * SESSION_ACCOUNT_RECORD_LEN;
        let mut record = [0u8; SESSION_ACCOUNT_RECORD_LEN];
        account.encode(&mut record);
        out[at..at + SESSION_ACCOUNT_RECORD_LEN].copy_from_slice(&record);
    }
    Ok(len)
}

/// Decode an account page, failing closed on a wrong magic or version, a
/// dirty reserved byte, a count past the page bound, a length that does not
/// match the count exactly, a page that claims to start past `total`, or
/// any malformed record.
///
/// # Errors
///
/// [`Errno::BadMagic`] for a wrong magic or a dirty reserved byte,
/// [`Errno::OutOfRange`] for a wrong version, [`Errno::LengthOutOfRange`]
/// for every length or range violation.
pub fn decode_account_page(bytes: &[u8]) -> Result<AccountPage, Errno> {
    if bytes.len() < SESSION_ACCOUNTS_HEADER_LEN || bytes.len() > SESSION_MAX_REPLY {
        return Err(Errno::LengthOutOfRange);
    }
    if read_u32(bytes, 0) != SESSION_ACCOUNTS_MAGIC || bytes[7] != 0 {
        return Err(Errno::BadMagic);
    }
    if read_u16(bytes, 4) != SESSION_VERSION {
        return Err(Errno::OutOfRange);
    }
    let count = bytes[6];
    if count as usize > SESSION_ACCOUNTS_PER_PAGE {
        return Err(Errno::LengthOutOfRange);
    }
    let expected = SESSION_ACCOUNTS_HEADER_LEN + count as usize * SESSION_ACCOUNT_RECORD_LEN;
    if bytes.len() != expected {
        return Err(Errno::LengthOutOfRange);
    }
    let total = read_u32(bytes, 8);
    let offset = read_u32(bytes, 12);
    if u64::from(offset) + u64::from(count) > u64::from(total) {
        return Err(Errno::LengthOutOfRange);
    }
    let mut accounts = [SessionAccount::EMPTY; SESSION_ACCOUNTS_PER_PAGE];
    for (index, slot) in accounts.iter_mut().take(count as usize).enumerate() {
        let at = SESSION_ACCOUNTS_HEADER_LEN + index * SESSION_ACCOUNT_RECORD_LEN;
        *slot = SessionAccount::decode(&bytes[at..at + SESSION_ACCOUNT_RECORD_LEN])?;
    }
    Ok(AccountPage {
        total,
        offset,
        count,
        accounts,
    })
}

/// The authority's answer to one [`SessionRequest::Authenticate`] or
/// [`SessionRequest::Background`].
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum SessionVerdict {
    /// The request is granted: the secret authenticates the account, or the
    /// calling session is now recorded as background. Nothing has been
    /// started — the authority acts on its own loop.
    Accepted,
    /// The request is refused.
    ///
    /// Deliberately reasonless — an unknown account, a wrong password, a
    /// locked account, and an authority that cannot read its database all
    /// answer identically. `retry_after` is the remaining per-account
    /// cooldown, zero when another attempt may be made now.
    Refused {
        /// How long until this account may be offered a secret again.
        retry_after: Duration64,
    },
}

impl SessionVerdict {
    /// Encode the verdict into `out`, returning [`SESSION_VERDICT_LEN`].
    ///
    /// # Errors
    ///
    /// [`Errno::BufferTooSmall`] when `out` is shorter than
    /// [`SESSION_VERDICT_LEN`].
    pub fn encode(&self, out: &mut [u8]) -> Result<usize, Errno> {
        if out.len() < SESSION_VERDICT_LEN {
            return Err(Errno::BufferTooSmall);
        }
        put_u32(out, 0, SESSION_VERDICT_MAGIC);
        put_u16(out, 4, SESSION_VERSION);
        out[7] = 0;
        let retry_after = match *self {
            Self::Accepted => {
                out[6] = STATUS_ACCEPTED;
                Duration64::ZERO
            }
            Self::Refused { retry_after } => {
                out[6] = STATUS_REFUSED;
                retry_after
            }
        };
        out[8..SESSION_VERDICT_LEN].copy_from_slice(&retry_after.to_le_bytes());
        Ok(SESSION_VERDICT_LEN)
    }

    /// Decode a verdict, failing closed on a wrong length, magic, version,
    /// or status, a dirty reserved byte, a non-canonical duration, or a
    /// cooldown attached to an acceptance.
    ///
    /// # Errors
    ///
    /// [`Errno::LengthOutOfRange`] for a wrong length, [`Errno::BadMagic`]
    /// for a wrong magic or a dirty reserved byte, [`Errno::OutOfRange`]
    /// for a wrong version, an unknown status, or a cooldown on an
    /// acceptance.
    pub fn decode(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() != SESSION_VERDICT_LEN {
            return Err(Errno::LengthOutOfRange);
        }
        if read_u32(bytes, 0) != SESSION_VERDICT_MAGIC || bytes[7] != 0 {
            return Err(Errno::BadMagic);
        }
        if read_u16(bytes, 4) != SESSION_VERSION {
            return Err(Errno::OutOfRange);
        }
        let retry_after = Duration64::from_bytes(&bytes[8..SESSION_VERDICT_LEN])?;
        match bytes[6] {
            STATUS_ACCEPTED if retry_after == Duration64::ZERO => Ok(Self::Accepted),
            STATUS_REFUSED if retry_after.secs() >= 0 => Ok(Self::Refused { retry_after }),
            _ => Err(Errno::OutOfRange),
        }
    }
}

// --- The authority -> session wake mailbox ---------------------------------

/// High tag of a desktop session's wake-mailbox endpoint id (see
/// [`session_wake_endpoint`]).
const WAKE_ENDPOINT_TAG: u64 = 0x5345_0000_0000_0000;

/// Magic word identifying a session wake message (`"SEW1"` little-endian).
pub const SESSION_WAKE_MAGIC: u32 = u32::from_le_bytes(*b"SEW1");

/// Exact encoded length of a [`SessionWake`] message.
pub const SESSION_WAKE_LEN: usize = 8;

/// The wake-mailbox endpoint id a desktop session binds for the authority's
/// messages: the session's own kernel task id under a fixed high tag, so
/// every session binds a distinct, collision-free, unreserved id — the same
/// naming rule the Switchboard command mailbox and the window channel's
/// event mailboxes follow. A pid is bounded to [`crate::PID_MAX`] precisely
/// so it fits beneath the tag, which is what makes the derivation lossless
/// and the four tagged namespaces mutually disjoint.
///
/// The session parks on this mailbox as a member of the wait-set it already
/// drains, so a switched-away session is woken by an event and never polls.
/// The id needs no secrecy: the mailbox is owner-only to receive, the
/// authority derives it from the task id it got when it spawned the session,
/// and the session honours a message only from the authority's
/// kernel-attested identity. The wake carries no authority of its own —
/// which task may actually present is the kernel's seat exclusivity, not
/// this message.
#[must_use]
pub const fn session_wake_endpoint(pid: u64) -> u64 {
    WAKE_ENDPOINT_TAG | (pid & crate::PID_MAX)
}

/// What the session authority tells a desktop session to do.
#[repr(u8)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum SessionWake {
    /// This session is the foreground one again: re-acquire the seat,
    /// re-read the display mode, and repaint in full.
    Foreground = 1,
    /// End cleanly: the machine is going down, or this account is being
    /// logged out from elsewhere.
    End = 2,
}

impl SessionWake {
    /// Encode the message into `out`, returning [`SESSION_WAKE_LEN`].
    ///
    /// # Errors
    ///
    /// [`Errno::BufferTooSmall`] when `out` is shorter than
    /// [`SESSION_WAKE_LEN`].
    pub fn encode(self, out: &mut [u8]) -> Result<usize, Errno> {
        if out.len() < SESSION_WAKE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        put_u32(out, 0, SESSION_WAKE_MAGIC);
        put_u16(out, 4, SESSION_VERSION);
        out[6] = self as u8;
        out[7] = 0;
        Ok(SESSION_WAKE_LEN)
    }

    /// Decode a message, failing closed on a wrong length, magic, version,
    /// or opcode, or a dirty reserved byte.
    ///
    /// # Errors
    ///
    /// [`Errno::LengthOutOfRange`] for a wrong length, [`Errno::BadMagic`]
    /// for a wrong magic or a dirty reserved byte, [`Errno::OutOfRange`]
    /// for a wrong version or an unknown opcode.
    pub fn decode(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() != SESSION_WAKE_LEN {
            return Err(Errno::LengthOutOfRange);
        }
        if read_u32(bytes, 0) != SESSION_WAKE_MAGIC || bytes[7] != 0 {
            return Err(Errno::BadMagic);
        }
        if read_u16(bytes, 4) != SESSION_VERSION {
            return Err(Errno::OutOfRange);
        }
        match bytes[6] {
            1 => Ok(Self::Foreground),
            2 => Ok(Self::End),
            _ => Err(Errno::OutOfRange),
        }
    }
}

/// Shape-check one request string field before it is encoded.
fn check_field(field: &str, max: usize) -> Result<(), Errno> {
    if field.is_empty() || field.len() > max {
        return Err(Errno::LengthOutOfRange);
    }
    Ok(())
}

/// Write a `u16`-length-prefixed string at `at`, returning the next offset.
/// The caller has already sized `out` for the whole frame.
fn put_str(out: &mut [u8], at: usize, s: &str) -> Result<usize, Errno> {
    let len = u16::try_from(s.len()).map_err(|_| Errno::LengthOutOfRange)?;
    put_u16(out, at, len);
    let end = at + 2 + s.len();
    out[at + 2..end].copy_from_slice(s.as_bytes());
    Ok(end)
}

/// A fail-closed little-endian reader over a request buffer.
struct Cursor<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Cursor<'a> {
    const fn new(bytes: &'a [u8], at: usize) -> Self {
        Self { bytes, at }
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

    fn u32(&mut self) -> Result<u32, Errno> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    /// A `u16`-length-prefixed UTF-8 string, refused when empty or longer
    /// than `max`.
    fn str(&mut self, max: usize) -> Result<&'a str, Errno> {
        let b = self.take(2)?;
        let len = usize::from(u16::from_le_bytes([b[0], b[1]]));
        if len == 0 || len > max {
            return Err(Errno::LengthOutOfRange);
        }
        let bytes = self.take(len)?;
        core::str::from_utf8(bytes).map_err(|_| Errno::OutOfRange)
    }

    const fn exhausted(&self) -> bool {
        self.at == self.bytes.len()
    }
}

#[cfg(test)]
#[path = "session_ipc_tests.rs"]
mod tests;
