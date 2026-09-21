//! Host tests for the `groupdel` client.

use super::{run, Output, RunError};
use crate::command::Command;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::cell::RefCell;
use tairix_abi::users_admin::{UsersAdminOp, UsersAdminRequest};
use tairix_abi::Errno;
use tairix_useradmin::AdminChannel;

/// An in-memory `users_admin` endpoint recording every decoded request.
struct MemChannel {
    fail: Option<Errno>,
    seen: RefCell<Vec<(UsersAdminOp, String)>>,
}

impl MemChannel {
    const fn new() -> Self {
        Self {
            fail: None,
            seen: RefCell::new(Vec::new()),
        }
    }

    const fn failing(err: Errno) -> Self {
        Self {
            fail: Some(err),
            seen: RefCell::new(Vec::new()),
        }
    }
}

impl AdminChannel for MemChannel {
    fn call(&self, req: &[u8], _out: &mut [u8]) -> Result<usize, Errno> {
        let request = UsersAdminRequest::decode(req)?;
        let target = match request {
            UsersAdminRequest::DeleteUser { username } => username.to_string(),
            UsersAdminRequest::DeleteGroup { name } => name.to_string(),
            _ => String::new(),
        };
        self.seen.borrow_mut().push((request.op(), target));
        self.fail.map_or(Ok(0), Err)
    }
}

/// A help tree holding one document, so the short-help path renders the
/// bundle's own text rather than the usage fallback.
struct MemHelp(String);

impl tairix_help::HelpSource for MemHelp {
    fn locale_dirs(&self) -> Result<Vec<String>, tairix_help::SourceError> {
        Ok(alloc::vec![String::from("en-US")])
    }

    fn read(
        &self,
        locale_dir: &str,
        file_name: &str,
    ) -> Result<Option<Vec<u8>>, tairix_help::SourceError> {
        if locale_dir == "en-US" && file_name == "groupdel.md" {
            Ok(Some(self.0.clone().into_bytes()))
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

/// An output stream that refuses every write.
struct DeadOut;

impl Output for DeadOut {
    fn write_all(&self, _bytes: &[u8]) -> Result<(), Errno> {
        Err(Errno::NotImplemented)
    }
}

fn help_doc() -> MemHelp {
    MemHelp(String::from(
        "## NAME\n\ngroupdel — x\n\n## SYNOPSIS\n\n`groupdel NAME`\n\n## OPTIONS\n\n- `-h, -?`\n",
    ))
}

#[test]
fn a_deletion_submits_exactly_one_operation_naming_the_operand() {
    let channel = MemChannel::new();
    let out = MemOut(RefCell::new(Vec::new()));
    run(
        Command::Delete(String::from("target")),
        None,
        &channel,
        &help_doc(),
        &out,
    )
    .expect("the deletion is accepted");
    assert_eq!(
        channel.seen.borrow().as_slice(),
        &[(UsersAdminOp::DeleteGroup, String::from("target"))]
    );
    // A deletion prints nothing on success.
    assert!(out.0.borrow().is_empty());
}

#[test]
fn a_refusal_carries_the_kernel_errno_and_its_one_wording() {
    let channel = MemChannel::failing(Errno::PermissionDenied);
    let out = MemOut(RefCell::new(Vec::new()));
    let err = run(
        Command::Delete(String::from("target")),
        None,
        &channel,
        &help_doc(),
        &out,
    )
    .expect_err("the deletion is refused");
    assert_eq!(err, RunError::Refused(Errno::PermissionDenied));
    assert_eq!(alloc::format!("{err}"), "permission denied");
}

#[test]
fn the_short_help_renders_the_bundle_document_and_reports_a_dead_stream() {
    let channel = MemChannel::new();
    let out = MemOut(RefCell::new(Vec::new()));
    run(Command::Help, None, &channel, &help_doc(), &out).expect("the help renders");
    let text = String::from_utf8(out.0.borrow().clone()).expect("utf8");
    assert!(text.contains("groupdel"));
    // Rendering help never touches the registry.
    assert!(channel.seen.borrow().is_empty());

    assert_eq!(
        run(Command::Help, None, &channel, &help_doc(), &DeadOut),
        Err(RunError::Output(Errno::NotImplemented))
    );
}
