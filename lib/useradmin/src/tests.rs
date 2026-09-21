//! Host tests for the shared `users_admin` client.

use super::{
    account, list_groups, list_users, parse_grants, refusal, render_grants, state_word, submit,
    AdminChannel,
};
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::cell::RefCell;
use tairix_abi::users_admin::{
    gid_list_into, grant_list_into, AccountStateCode, GroupEntry, ListResponseBuilder, UserEntry,
    UsersAdminOp, UsersAdminRequest,
};
use tairix_abi::{CapabilityId, Errno};

/// An in-memory `users_admin` endpoint: serves listings from fixed rows
/// and records the operation of every mutation it is handed.
struct MemChannel {
    users: Vec<(String, u32, Vec<CapabilityId>, AccountStateCode)>,
    groups: Vec<(String, u32)>,
    fail: Option<Errno>,
    seen: RefCell<Vec<UsersAdminOp>>,
    /// Every request byte the channel was handed, so a test can assert the
    /// encode buffer carried what it should.
    last_request: RefCell<Vec<u8>>,
}

impl MemChannel {
    fn new() -> Self {
        Self {
            users: alloc::vec![
                (
                    "root".to_string(),
                    0,
                    alloc::vec![CapabilityId::USER_ADMIN],
                    AccountStateCode::Active,
                ),
                (
                    "alice".to_string(),
                    1000,
                    Vec::new(),
                    AccountStateCode::Locked,
                ),
            ],
            groups: alloc::vec![("system".to_string(), 0), ("staff".to_string(), 1000)],
            fail: None,
            seen: RefCell::new(Vec::new()),
            last_request: RefCell::new(Vec::new()),
        }
    }

    fn failing(err: Errno) -> Self {
        Self {
            fail: Some(err),
            ..Self::new()
        }
    }
}

impl AdminChannel for MemChannel {
    fn call(&self, req: &[u8], out: &mut [u8]) -> Result<usize, Errno> {
        *self.last_request.borrow_mut() = req.to_vec();
        let request = UsersAdminRequest::decode(req)?;
        self.seen.borrow_mut().push(request.op());
        if let Some(err) = self.fail {
            return Err(err);
        }
        let mut builder = match request {
            UsersAdminRequest::ListUsers | UsersAdminRequest::ListGroups => {
                ListResponseBuilder::new(out)?
            }
            // A mutation answers no payload, so it never touches `out` —
            // which the caller legitimately passes empty.
            _ => return Ok(0),
        };
        match request {
            UsersAdminRequest::ListUsers => {
                for (name, uid, grants, state) in &self.users {
                    let mut grant_backing = alloc::vec![0u8; 2 * grants.len()];
                    let grants = grant_list_into(grants, &mut grant_backing)?;
                    let gids = gid_list_into(&[], &mut [])?;
                    builder.push_user(&UserEntry {
                        username: name,
                        uid: *uid,
                        primary_gid: *uid,
                        supplementary_gids: gids,
                        display_name: "",
                        home: "/Users/x",
                        shell: "/System/Commands/elsh.app/Run",
                        grants,
                        state: *state,
                    })?;
                }
            }
            UsersAdminRequest::ListGroups => {
                for (name, gid) in &self.groups {
                    builder.push_group(&GroupEntry { name, gid: *gid })?;
                }
            }
            _ => return Ok(0),
        }
        Ok(builder.finish())
    }
}

#[test]
fn a_listing_decodes_every_field_into_owned_rows() {
    let channel = MemChannel::new();
    let accounts = list_users(&channel).expect("the listing decodes");
    assert_eq!(accounts.len(), 2);
    assert_eq!(accounts[0].username, "root");
    assert_eq!(accounts[0].grants, alloc::vec![CapabilityId::USER_ADMIN]);
    assert_eq!(accounts[0].state, AccountStateCode::Active);
    assert_eq!(accounts[1].state, AccountStateCode::Locked);
    assert_eq!(accounts[1].home, "/Users/x");

    let groups = list_groups(&channel).expect("the listing decodes");
    assert_eq!(groups.len(), 2);
    assert_eq!(groups[1].name, "staff");
    assert_eq!(groups[1].gid, 1000);
}

#[test]
fn account_finds_a_named_row_and_reports_an_absent_one() {
    let channel = MemChannel::new();
    let found = account(&channel, "alice")
        .expect("the listing decodes")
        .expect("alice is listed");
    assert_eq!(found.uid, 1000);
    assert_eq!(account(&channel, "nobody").expect("decodes"), None);
}

#[test]
fn a_refused_call_surfaces_the_kernel_errno_unchanged() {
    let channel = MemChannel::failing(Errno::PermissionDenied);
    assert_eq!(list_users(&channel), Err(Errno::PermissionDenied));
    assert_eq!(
        submit(&channel, &UsersAdminRequest::DeleteGroup { name: "staff" }),
        Err(Errno::PermissionDenied)
    );
}

#[test]
fn submit_hands_the_channel_exactly_the_encoded_operation() {
    let channel = MemChannel::new();
    let request = UsersAdminRequest::SetPassword {
        username: "alice",
        password_record: "pbkdf2-sha256$1000$AAAA$BBBB",
    };
    submit(&channel, &request).expect("the mutation is accepted");
    assert_eq!(
        channel.seen.borrow().as_slice(),
        &[UsersAdminOp::SetPassword]
    );
    // Byte-exact: the submitted slice is the encoding and nothing past it,
    // so no tail of a previous request can ride along.
    let mut expected = [0u8; tairix_abi::users_admin::USERS_ADMIN_MAX_REQUEST];
    let len = request.encode_into(&mut expected).expect("encodes");
    assert_eq!(channel.last_request.borrow().as_slice(), &expected[..len]);
}

#[test]
fn a_request_past_the_frozen_bound_never_reaches_the_channel() {
    let channel = MemChannel::new();
    let name = alloc::string::String::from_utf8(alloc::vec![
        b'x';
        tairix_abi::users_admin::USERS_ADMIN_MAX_REQUEST
    ])
    .expect("ascii");
    assert_eq!(
        submit(&channel, &UsersAdminRequest::DeleteUser { username: &name }),
        Err(Errno::BufferTooSmall)
    );
    assert!(channel.seen.borrow().is_empty());
}

#[test]
fn grants_render_and_parse_through_one_vocabulary() {
    let grants = alloc::vec![CapabilityId::USER_ADMIN, CapabilityId::FS_MOUNT];
    let rendered = render_grants(&grants);
    assert_eq!(parse_grants(&rendered), Ok(grants));
    assert!(render_grants(&[]).is_empty());
    assert_eq!(parse_grants(""), Ok(Vec::new()));
    // A duplicate names the same authority once.
    assert_eq!(
        parse_grants("CAP_USER_ADMIN,CAP_USER_ADMIN"),
        Ok(alloc::vec![CapabilityId::USER_ADMIN])
    );
    // An unreadable name is refused whole, never silently dropped: a
    // ceiling applied approximately is a ceiling nobody can reason about.
    assert_eq!(parse_grants("CAP_USER_ADMIN,CAP_NOPE"), Err("CAP_NOPE"));
}

#[test]
fn every_refusal_and_state_has_one_wording() {
    assert_eq!(refusal(Errno::PermissionDenied), "permission denied");
    assert_eq!(refusal(Errno::NotFound), "no such account or group");
    assert_eq!(refusal(Errno::AlreadyExists), "already exists");
    assert_eq!(refusal(Errno::NoSpace), "database full");
    assert_eq!(refusal(Errno::LengthOutOfRange), "malformed field");
    assert_eq!(refusal(Errno::BadAddress), "operation failed");
    assert_eq!(state_word(AccountStateCode::Active), "active");
    assert_eq!(state_word(AccountStateCode::Locked), "locked");
    assert_eq!(state_word(AccountStateCode::NoLogin), "nologin");
}

#[test]
fn a_relayed_listing_round_trips_through_one_definition() {
    use super::listing;
    let accounts = alloc::vec![
        super::Account {
            username: "ada".to_string(),
            uid: 1000,
            primary_gid: 1000,
            supplementary_gids: alloc::vec![10, 20],
            display_name: "Ada Lovelace".to_string(),
            home: "/Users/ada".to_string(),
            shell: "/System/Commands/elsh.app/Run".to_string(),
            grants: alloc::vec![CapabilityId::USER_ADMIN],
            state: AccountStateCode::Locked,
        },
        super::Account {
            username: "svc".to_string(),
            uid: 3,
            primary_gid: 1,
            supplementary_gids: Vec::new(),
            display_name: String::new(),
            home: "none".to_string(),
            shell: "none".to_string(),
            grants: Vec::new(),
            state: AccountStateCode::NoLogin,
        },
    ];
    let groups = alloc::vec![super::Group {
        name: "staff".to_string(),
        gid: 1000,
    }];
    let rendered = listing::render(&accounts, &groups);
    let read = listing::parse(rendered.as_bytes()).expect("the listing parses");
    assert_eq!(read.accounts, accounts);
    assert_eq!(read.groups, groups);
}

#[test]
fn an_unreadable_listing_line_is_skipped_rather_than_hiding_the_rest() {
    use super::listing;
    let good = listing::render(
        &[super::Account {
            username: "ada".to_string(),
            uid: 1000,
            primary_gid: 1000,
            supplementary_gids: Vec::new(),
            display_name: String::new(),
            home: "/Users/ada".to_string(),
            shell: "/bin/sh".to_string(),
            grants: Vec::new(),
            state: AccountStateCode::Active,
        }],
        &[],
    );
    // A line of the wrong arity, an unknown state, an unreadable gid, and
    // a word that is not a capability each drop their own record only.
    let mixed = alloc::format!(
        "{good}u:short:1\nu:bad:1:1::::::z\nu:gid:1:1:x::::::a\ng:staff:1000\ng:bad:x\n"
    );
    let read = listing::parse(mixed.as_bytes()).expect("the listing parses");
    assert_eq!(read.accounts.len(), 1);
    assert_eq!(read.accounts[0].username, "ada");
    assert_eq!(read.groups.len(), 1);
    assert_eq!(read.groups[0].name, "staff");

    // Non-UTF-8 is no listing at all, distinguishable from an empty one.
    assert_eq!(listing::parse(&[0xff, 0xfe]), None);
    assert_eq!(listing::parse(b""), Some(listing::Listing::default()));
    assert!(listing::known_capability("CAP_USER_ADMIN"));
    assert!(!listing::known_capability("CAP_NOPE"));
}

#[test]
fn a_reply_longer_than_the_buffer_is_refused_rather_than_clamped() {
    /// A channel claiming to have written more than the buffer holds.
    struct Overlong;
    impl AdminChannel for Overlong {
        fn call(&self, _req: &[u8], _out: &mut [u8]) -> Result<usize, Errno> {
            Ok(usize::MAX)
        }
    }
    // Clamping would decode a truncated listing as if it were whole, so
    // both walks refuse instead.
    assert_eq!(list_users(&Overlong), Err(Errno::LengthOutOfRange));
    assert_eq!(list_groups(&Overlong), Err(Errno::LengthOutOfRange));
}
