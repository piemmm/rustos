//! The shared `users_admin` client: the one place a command app encodes a
//! request, submits it, decodes a listing, and words a refusal.
//!
//! Seven tools administer accounts — `useradd`, `usermod`, `userdel`,
//! `passwd`, `groupadd`, `groupdel`, and the interactive `users` session —
//! and every one of them needs the same four things. Without this crate
//! each would carry its own copy of the encode buffer, the reply capacity,
//! the list walk and the error wording, and they would drift: two tools
//! printing different words for the same kernel refusal is the duplication
//! the charter forbids.
//!
//! # Not a policy point
//!
//! The client adds no authority of its own. Every decision — the
//! `CAP_USER_ADMIN` dispatch gate, the never-widen grant rule, the
//! last-administrator guard, uid and name validity — is made kernel-side
//! under the caller's attested identity, and a refusal arrives here as the
//! [`Errno`] the kernel chose. This crate builds and decodes; it never
//! judges.
//!
//! # Layering & safety
//!
//! `no_std` (with `alloc`); the dependencies are the audited `lib/abi`
//! vocabulary and the shared `lib/users` account policy, so a userland tool
//! linking it never links a kernel or driver crate. No `unsafe`, and no
//! `unwrap`/`expect`/`panic!` in production paths.

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use tairix_abi::users_admin::{
    decode_group_list, decode_user_list, AccountStateCode, UsersAdminRequest,
    USERS_ADMIN_MAX_REQUEST,
};
use tairix_abi::{CapabilityId, Errno};
use tairix_users::{MAX_DB_LEN, MAX_GROUPS_DB_LEN};

/// Byte capacity every listing reply is read into: twice the on-disk
/// database maximum, which is the kernel's own response bound, so a full
/// listing always fits and a hostile capacity can never be asked for.
const RESPONSE_CAPACITY: usize = 2 * MAX_DB_LEN;

/// Byte capacity a group listing is read into. The registry is far smaller
/// than the account database, so it gets its own bound rather than paying
/// the account one.
const GROUP_RESPONSE_CAPACITY: usize = 2 * MAX_GROUPS_DB_LEN;

/// The transport that carries one encoded `users_admin` request and
/// returns the response bytes written.
///
/// On a running system this is the `users_admin` syscall; in tests an
/// in-memory database. Every authorisation decision stays on the far side
/// of this seam.
pub trait AdminChannel {
    /// Submit `req`, writing any response into `out` and answering how
    /// many bytes it holds.
    ///
    /// # Errors
    ///
    /// The [`Errno`] the kernel raised — e.g. [`Errno::PermissionDenied`]
    /// for a caller without `CAP_USER_ADMIN`.
    fn call(&self, req: &[u8], out: &mut [u8]) -> Result<usize, Errno>;
}

/// One account as a listing answers it, owned so the reply buffer it was
/// decoded from can be released.
///
/// Carries no password material: the kernel's `ListUsers` response omits
/// it entirely, not even the record's cost parameters.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Account {
    /// Account name.
    pub username: String,
    /// Account uid.
    pub uid: u32,
    /// Primary group.
    pub primary_gid: u32,
    /// Supplementary groups.
    pub supplementary_gids: Vec<u32>,
    /// Display name, empty where the account declares none.
    pub display_name: String,
    /// Home path, or the `users-v1` absent-path marker.
    pub home: String,
    /// Shell path, or the `users-v1` absent-path marker.
    pub shell: String,
    /// The account's capability grant ceiling.
    pub grants: Vec<CapabilityId>,
    /// Whether the account may log in.
    pub state: AccountStateCode,
}

/// One group as a listing answers it, owned for the same reason
/// [`Account`] is.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Group {
    /// Group name.
    pub name: String,
    /// Group id.
    pub gid: u32,
}

/// Submit a mutating request, whose success carries no payload.
///
/// The encode buffer is scrubbed before returning: a `SetPassword` or
/// `CreateUser` request carries a salted password record, which is
/// credential material even though it is not a password.
///
/// # Errors
///
/// [`Errno::BufferTooSmall`] when the encoding runs past the frozen
/// request bound, else the [`Errno`] the kernel raised.
pub fn submit(channel: &dyn AdminChannel, request: &UsersAdminRequest<'_>) -> Result<(), Errno> {
    let mut buf = [0u8; USERS_ADMIN_MAX_REQUEST];
    let outcome = request
        .encode_into(&mut buf)
        .and_then(|len| channel.call(&buf[..len], &mut []));
    buf.fill(0);
    outcome.map(|_| ())
}

/// Every account the caller may list.
///
/// # Errors
///
/// The [`Errno`] the kernel raised, or a structural defect in its reply
/// (fail closed — never a partially decoded listing).
pub fn list_users(channel: &dyn AdminChannel) -> Result<Vec<Account>, Errno> {
    decode_accounts(&submit_list(
        channel,
        &UsersAdminRequest::ListUsers,
        RESPONSE_CAPACITY,
    )?)
}

/// Decode a `ListUsers` reply a caller already holds.
///
/// The one walk of that response, so a tool that submitted the request
/// itself does not re-derive the decode.
///
/// # Errors
///
/// A structural defect in the reply (fail closed — never a partially
/// decoded listing).
pub fn decode_accounts(bytes: &[u8]) -> Result<Vec<Account>, Errno> {
    let mut accounts = Vec::new();
    for entry in decode_user_list(bytes)? {
        let entry = entry?;
        accounts.push(Account {
            username: String::from(entry.username),
            uid: entry.uid,
            primary_gid: entry.primary_gid,
            supplementary_gids: entry.supplementary_gids.iter().collect(),
            display_name: String::from(entry.display_name),
            home: String::from(entry.home),
            shell: String::from(entry.shell),
            grants: entry.grants.iter().collect(),
            state: entry.state,
        });
    }
    Ok(accounts)
}

/// Every group the caller may list.
///
/// # Errors
///
/// The [`Errno`] the kernel raised, or a structural defect in its reply.
pub fn list_groups(channel: &dyn AdminChannel) -> Result<Vec<Group>, Errno> {
    decode_groups(&submit_list(
        channel,
        &UsersAdminRequest::ListGroups,
        GROUP_RESPONSE_CAPACITY,
    )?)
}

/// Decode a `ListGroups` reply a caller already holds.
///
/// # Errors
///
/// A structural defect in the reply.
pub fn decode_groups(bytes: &[u8]) -> Result<Vec<Group>, Errno> {
    let mut groups = Vec::new();
    for entry in decode_group_list(bytes)? {
        let entry = entry?;
        groups.push(Group {
            name: String::from(entry.name),
            gid: entry.gid,
        });
    }
    Ok(groups)
}

/// The account `username` names, or `None` where the listing holds none.
///
/// The one definition of "read this account's current record so an edit
/// can be expressed as a full replacement", which is what every
/// operation that replaces a field set needs: the syscall takes whole
/// records, so a tool changing one field must first know the rest.
///
/// # Errors
///
/// The [`Errno`] the kernel raised while listing.
pub fn account(channel: &dyn AdminChannel, username: &str) -> Result<Option<Account>, Errno> {
    Ok(list_users(channel)?
        .into_iter()
        .find(|account| account.username == username))
}

/// Submit a listing request and return the reply bytes.
fn submit_list(
    channel: &dyn AdminChannel,
    request: &UsersAdminRequest<'_>,
    capacity: usize,
) -> Result<Vec<u8>, Errno> {
    let mut req = [0u8; USERS_ADMIN_MAX_REQUEST];
    let len = request.encode_into(&mut req)?;
    let mut out = alloc::vec![0u8; capacity];
    let used = channel.call(&req[..len], &mut out)?;
    // A channel claiming to have written more than the buffer holds is
    // refused, never clamped: clamping would decode a truncated listing
    // as if it were whole.
    if used > out.len() {
        return Err(Errno::LengthOutOfRange);
    }
    out.truncate(used);
    Ok(out)
}

/// The one terse wording for a kernel refusal, shared by every account
/// tool so two of them can never describe the same `Errno` differently.
///
/// Deliberately coarse on the authorisation path: `PermissionDenied`
/// covers a missing `CAP_USER_ADMIN`, a grant the caller does not hold,
/// and the last-administrator guard alike, and distinguishing them here
/// would tell an unauthorised caller which wall it hit.
#[must_use]
pub fn refusal(err: Errno) -> &'static str {
    match err {
        Errno::PermissionDenied => "permission denied",
        Errno::NotFound => "no such account or group",
        Errno::AlreadyExists => "already exists",
        Errno::NoSpace => "database full",
        Errno::NotImplemented => "account administration unavailable",
        Errno::LengthOutOfRange | Errno::OutOfRange => "malformed field",
        _ => "operation failed",
    }
}

/// How a listing renders an account's state.
#[must_use]
pub const fn state_word(state: AccountStateCode) -> &'static str {
    match state {
        AccountStateCode::Active => "active",
        AccountStateCode::Locked => "locked",
        AccountStateCode::NoLogin => "nologin",
    }
}

/// An account's grant ceiling as a comma-separated list of capability
/// names, empty for an account holding none.
///
/// A capability the running build does not name renders as `CAP_?` rather
/// than being dropped, so a listing never understates a ceiling.
#[must_use]
pub fn render_grants(grants: &[CapabilityId]) -> String {
    let mut text = String::new();
    for cap in grants {
        if !text.is_empty() {
            text.push(',');
        }
        text.push_str(cap.name().unwrap_or("CAP_?"));
    }
    text
}

/// Parse a comma-separated capability-name list into a grant set.
///
/// Fails closed on the first unrecognised name rather than silently
/// dropping it: a ceiling is not something to apply approximately. An
/// empty string is an empty ceiling, which is a legitimate request.
///
/// # Errors
///
/// The offending name, so the caller can say which word it could not read.
pub fn parse_grants(list: &str) -> Result<Vec<CapabilityId>, &str> {
    let mut grants = Vec::new();
    for name in list.split(',').filter(|name| !name.is_empty()) {
        let cap = CapabilityId::from_name(name).ok_or(name)?;
        if !grants.contains(&cap) {
            grants.push(cap);
        }
    }
    Ok(grants)
}

#[cfg(test)]
mod tests;

/// The `:`-delimited line form an administrator-authenticated listing is
/// relayed in, and the one parser that reads it back.
///
/// A graphical surface cannot call `users_admin` — the whole syscall is
/// gated on `CAP_USER_ADMIN` and a desktop application holds no
/// authority at all — so it asks an authenticated account to run the
/// listing tool and reads what it printed. Rendering and parsing
/// therefore live here together: the tool that writes the lines and the
/// surface that reads them share one definition, so they cannot disagree
/// about what a field means.
///
/// `:` is the delimiter because the `users-v1` and `groups-v1` databases
/// already forbid it in every field, so no value can ever contain one and
/// no escaping is needed. A line this parser does not recognise is
/// **skipped**, so a build that prints a field this one does not know
/// degrades to the records it does understand rather than to nothing.
pub mod listing {
    use super::{Account, Group};
    use alloc::string::String;
    use alloc::vec::Vec;
    use core::fmt::Write as _;
    use tairix_abi::users_admin::AccountStateCode;
    use tairix_abi::CapabilityId;

    /// Tag of an account line.
    const USER: &str = "u";
    /// Tag of a group line.
    const GROUP: &str = "g";
    /// Fields an account line carries, including its tag.
    const USER_FIELDS: usize = 10;
    /// Fields a group line carries, including its tag.
    const GROUP_FIELDS: usize = 3;

    /// What a relayed listing states.
    #[derive(Clone, Debug, Default, Eq, PartialEq)]
    pub struct Listing {
        /// Every account, in the order the listing stated them.
        pub accounts: Vec<Account>,
        /// Every group, in the order the listing stated them.
        pub groups: Vec<Group>,
    }

    /// Render `accounts` and `groups` as the relayable line form.
    #[must_use]
    pub fn render(accounts: &[Account], groups: &[Group]) -> String {
        let mut out = String::new();
        for account in accounts {
            let _ = writeln!(
                out,
                "{USER}:{}:{}:{}:{}:{}:{}:{}:{}:{}",
                account.username,
                account.uid,
                account.primary_gid,
                join_ids(&account.supplementary_gids),
                account.display_name,
                account.home,
                account.shell,
                super::render_grants(&account.grants),
                state_code(account.state),
            );
        }
        for group in groups {
            let _ = writeln!(out, "{GROUP}:{}:{}", group.name, group.gid);
        }
        out
    }

    /// Read a relayed listing back.
    ///
    /// Non-UTF-8 input is no listing at all; within valid text, a line
    /// the grammar does not admit is skipped rather than failing the
    /// whole read, so one unreadable record never hides the rest.
    #[must_use]
    pub fn parse(output: &[u8]) -> Option<Listing> {
        let text = core::str::from_utf8(output).ok()?;
        let mut listing = Listing::default();
        for line in text.lines() {
            let fields: Vec<&str> = line.split(':').collect();
            match fields.first().copied() {
                Some(USER) if fields.len() == USER_FIELDS => {
                    if let Some(account) = account(&fields) {
                        listing.accounts.push(account);
                    }
                }
                Some(GROUP) if fields.len() == GROUP_FIELDS => {
                    if let Ok(gid) = fields[2].parse() {
                        listing.groups.push(Group {
                            name: String::from(fields[1]),
                            gid,
                        });
                    }
                }
                _ => {}
            }
        }
        Some(listing)
    }

    /// One account line's fields as an [`Account`], or `None` for a line
    /// whose numbers or state this build cannot read.
    fn account(fields: &[&str]) -> Option<Account> {
        Some(Account {
            username: String::from(fields[1]),
            uid: fields[2].parse().ok()?,
            primary_gid: fields[3].parse().ok()?,
            supplementary_gids: split_ids(fields[4])?,
            display_name: String::from(fields[5]),
            home: String::from(fields[6]),
            shell: String::from(fields[7]),
            grants: super::parse_grants(fields[8]).ok()?,
            state: state_of(fields[9])?,
        })
    }

    /// A gid list as its comma-separated spelling.
    fn join_ids(ids: &[u32]) -> String {
        let mut out = String::new();
        for id in ids {
            if !out.is_empty() {
                out.push(',');
            }
            let _ = write!(out, "{id}");
        }
        out
    }

    /// A comma-separated gid list, or `None` where an element is not a
    /// number (fail closed — a membership is not read approximately).
    fn split_ids(text: &str) -> Option<Vec<u32>> {
        text.split(',')
            .filter(|part| !part.is_empty())
            .map(|part| part.parse().ok())
            .collect()
    }

    /// The one-letter spelling of an account state.
    const fn state_code(state: AccountStateCode) -> &'static str {
        match state {
            AccountStateCode::Active => "a",
            AccountStateCode::Locked => "l",
            AccountStateCode::NoLogin => "n",
        }
    }

    /// The state a spelling names, or `None` for one this build does not
    /// know — which drops the record rather than guessing it is active.
    fn state_of(text: &str) -> Option<AccountStateCode> {
        match text {
            "a" => Some(AccountStateCode::Active),
            "l" => Some(AccountStateCode::Locked),
            "n" => Some(AccountStateCode::NoLogin),
            _ => None,
        }
    }

    /// Whether `grants` names a capability whose id this build knows.
    ///
    /// The one place the desktop asks "is this word a capability?", so a
    /// surface offering a ceiling edit and the tool applying it admit the
    /// same vocabulary.
    #[must_use]
    pub fn known_capability(name: &str) -> bool {
        CapabilityId::from_name(name).is_some()
    }
}
