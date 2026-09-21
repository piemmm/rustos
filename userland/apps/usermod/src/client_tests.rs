//! Host tests for the `usermod` client.

use super::{run, Output, RunError, Step};
use crate::command::{Changes, Command, Lock};
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::cell::RefCell;
use tairix_abi::users_admin::{
    gid_list_into, grant_list_into, AccountStateCode, ListResponseBuilder, UserEntry, UsersAdminOp,
    UsersAdminRequest,
};
use tairix_abi::{CapabilityId, Errno};
use tairix_help::{HelpSource, SourceError};
use tairix_useradmin::AdminChannel;

/// What the fixture recorded about one submitted request.
#[derive(Clone, Debug, Eq, PartialEq)]
enum Seen {
    Modify {
        username: String,
        primary_gid: u32,
        gids: Vec<u32>,
        display_name: String,
        home: String,
        shell: String,
    },
    Grants(String, Vec<CapabilityId>),
    Lock(String, bool),
    List,
}

/// An in-memory `users_admin` endpoint serving one account and recording
/// every mutation, with an optional refusal for a named operation.
struct MemChannel {
    listed: bool,
    refuse: Option<(UsersAdminOp, Errno)>,
    seen: RefCell<Vec<Seen>>,
}

impl MemChannel {
    const fn new() -> Self {
        Self {
            listed: true,
            refuse: None,
            seen: RefCell::new(Vec::new()),
        }
    }

    const fn empty() -> Self {
        Self {
            listed: false,
            refuse: None,
            seen: RefCell::new(Vec::new()),
        }
    }

    const fn refusing(op: UsersAdminOp, err: Errno) -> Self {
        Self {
            listed: true,
            refuse: Some((op, err)),
            seen: RefCell::new(Vec::new()),
        }
    }
}

impl AdminChannel for MemChannel {
    fn call(&self, req: &[u8], out: &mut [u8]) -> Result<usize, Errno> {
        let request = UsersAdminRequest::decode(req)?;
        if let Some((op, err)) = self.refuse {
            if op == request.op() {
                return Err(err);
            }
        }
        match request {
            UsersAdminRequest::ListUsers => {
                self.seen.borrow_mut().push(Seen::List);
                let mut builder = ListResponseBuilder::new(out)?;
                if self.listed {
                    let mut grant_backing = [0u8; 2];
                    let grants = grant_list_into(&[CapabilityId::FS_ACCESS], &mut grant_backing)?;
                    let mut gid_backing = [0u8; 8];
                    let gids = gid_list_into(&[7, 8], &mut gid_backing)?;
                    builder.push_user(&UserEntry {
                        username: "ada",
                        uid: 1000,
                        primary_gid: 1000,
                        supplementary_gids: gids,
                        display_name: "Ada",
                        home: "/Users/ada",
                        shell: "/System/Commands/elsh.app/Run",
                        grants,
                        state: AccountStateCode::Active,
                    })?;
                }
                Ok(builder.finish())
            }
            UsersAdminRequest::ModifyUser(record) => {
                self.seen.borrow_mut().push(Seen::Modify {
                    username: record.username.to_string(),
                    primary_gid: record.primary_gid,
                    gids: record.supplementary_gids.iter().collect(),
                    display_name: record.display_name.to_string(),
                    home: record.home.to_string(),
                    shell: record.shell.to_string(),
                });
                Ok(0)
            }
            UsersAdminRequest::SetGrants { username, grants } => {
                self.seen
                    .borrow_mut()
                    .push(Seen::Grants(username.to_string(), grants.iter().collect()));
                Ok(0)
            }
            UsersAdminRequest::SetAccountState { username, locked } => {
                self.seen
                    .borrow_mut()
                    .push(Seen::Lock(username.to_string(), locked));
                Ok(0)
            }
            _ => Ok(0),
        }
    }
}

/// A help tree holding the tool's own canonical document.
struct OneDoc;

impl HelpSource for OneDoc {
    fn locale_dirs(&self) -> Result<Vec<String>, SourceError> {
        Ok(alloc::vec![String::from("en-US")])
    }

    fn read(&self, locale_dir: &str, file_name: &str) -> Result<Option<Vec<u8>>, SourceError> {
        if locale_dir == "en-US" && file_name == "usermod.md" {
            Ok(Some(
                b"## NAME\n\nusermod \xe2\x80\x94 modify a user account\n\n\
                  ## SYNOPSIS\n\n`usermod [-L] NAME`\n"
                    .to_vec(),
            ))
        } else {
            Ok(None)
        }
    }
}

/// Collects what the tool wrote.
struct MemOut(RefCell<Vec<u8>>);

impl Output for MemOut {
    fn write_all(&self, bytes: &[u8]) -> Result<(), Errno> {
        self.0.borrow_mut().extend_from_slice(bytes);
        Ok(())
    }
}

fn modify(changes: Changes) -> Command {
    Command::Modify {
        name: String::from("ada"),
        changes,
    }
}

fn go(channel: &MemChannel, changes: Changes) -> Result<(), RunError> {
    run(
        modify(changes),
        None,
        channel,
        &OneDoc,
        &MemOut(RefCell::new(Vec::new())),
    )
}

#[test]
fn a_single_field_edit_resends_every_other_field_unchanged() {
    let channel = MemChannel::new();
    go(
        &channel,
        Changes {
            comment: Some(String::from("Ada Lovelace")),
            ..Changes::default()
        },
    )
    .expect("the edit is accepted");
    assert_eq!(
        channel.seen.borrow().as_slice(),
        &[
            Seen::List,
            Seen::Modify {
                username: String::from("ada"),
                primary_gid: 1000,
                gids: alloc::vec![7, 8],
                display_name: String::from("Ada Lovelace"),
                home: String::from("/Users/ada"),
                shell: String::from("/System/Commands/elsh.app/Run"),
            }
        ]
    );
}

#[test]
fn a_named_group_set_replaces_the_held_one_whole() {
    let channel = MemChannel::new();
    go(
        &channel,
        Changes {
            supplementary_gids: Some(alloc::vec![42]),
            ..Changes::default()
        },
    )
    .expect("the edit is accepted");
    let Some(Seen::Modify { gids, .. }) = channel.seen.borrow().get(1).cloned() else {
        panic!("expected a modify");
    };
    assert_eq!(gids, alloc::vec![42]);
}

#[test]
fn the_operations_are_issued_in_one_fixed_order() {
    let channel = MemChannel::new();
    go(
        &channel,
        Changes {
            comment: Some(String::from("Ada")),
            grants: Some(String::from("CAP_USER_ADMIN")),
            lock: Some(Lock::Locked),
            ..Changes::default()
        },
    )
    .expect("the edits are accepted");
    let seen = channel.seen.borrow();
    assert!(matches!(seen.first(), Some(Seen::List)));
    assert!(matches!(seen.get(1), Some(Seen::Modify { .. })));
    assert_eq!(
        seen.get(2),
        Some(&Seen::Grants(
            String::from("ada"),
            alloc::vec![CapabilityId::USER_ADMIN]
        ))
    );
    assert_eq!(seen.get(3), Some(&Seen::Lock(String::from("ada"), true)));
}

#[test]
fn a_lock_only_line_sends_no_identity_replacement() {
    let channel = MemChannel::new();
    go(
        &channel,
        Changes {
            lock: Some(Lock::Unlocked),
            ..Changes::default()
        },
    )
    .expect("the edit is accepted");
    assert_eq!(
        channel.seen.borrow().as_slice(),
        &[Seen::List, Seen::Lock(String::from("ada"), false)]
    );
}

#[test]
fn an_account_the_listing_does_not_hold_is_refused_before_anything_is_sent() {
    let channel = MemChannel::empty();
    assert_eq!(
        go(
            &channel,
            Changes {
                lock: Some(Lock::Locked),
                ..Changes::default()
            }
        ),
        Err(RunError::NoSuchAccount)
    );
    assert_eq!(channel.seen.borrow().as_slice(), &[Seen::List]);
}

#[test]
fn a_refusal_names_its_step_and_warns_when_an_earlier_one_may_stand() {
    let channel = MemChannel::refusing(UsersAdminOp::SetAccountState, Errno::PermissionDenied);
    let err = go(
        &channel,
        Changes {
            comment: Some(String::from("Ada")),
            lock: Some(Lock::Locked),
            ..Changes::default()
        },
    )
    .expect_err("the lock is refused");
    assert_eq!(
        err,
        RunError::Refused(Step::LockState, Errno::PermissionDenied)
    );
    let said = alloc::format!("{err}");
    assert!(said.contains("could not set the lock state: permission denied"));
    assert!(said.contains("may already be in effect"));

    // The first step refuses before any write, so no such warning is owed.
    let channel = MemChannel::refusing(UsersAdminOp::ModifyUser, Errno::PermissionDenied);
    let err = go(
        &channel,
        Changes {
            comment: Some(String::from("Ada")),
            ..Changes::default()
        },
    )
    .expect_err("the edit is refused");
    assert!(!alloc::format!("{err}").contains("may already be in effect"));
}

#[test]
fn an_unreadable_grant_name_is_refused_whole() {
    let channel = MemChannel::new();
    assert_eq!(
        go(
            &channel,
            Changes {
                grants: Some(String::from("CAP_USER_ADMIN,CAP_NOPE")),
                ..Changes::default()
            }
        ),
        Err(RunError::UnknownGrant(String::from("CAP_NOPE")))
    );
    // Nothing was set: a ceiling is not applied approximately.
    assert_eq!(channel.seen.borrow().as_slice(), &[Seen::List]);
}

#[test]
fn the_short_help_renders_the_bundle_document() {
    let channel = MemChannel::new();
    let out = MemOut(RefCell::new(Vec::new()));
    run(Command::Help, None, &channel, &OneDoc, &out).expect("the help renders");
    let text = String::from_utf8(out.0.borrow().clone()).expect("utf8");
    assert!(text.contains("usermod"));
    assert!(channel.seen.borrow().is_empty());
}
