//! Host tests for the `passwd` client.

use super::{run, Output, RunError, SaltSource, Terminal};
use crate::command::{Command, Secret};
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::cell::RefCell;
use tairix_abi::users_admin::UsersAdminRequest;
use tairix_abi::Errno;
use tairix_help::{HelpSource, SourceError};
use tairix_useradmin::AdminChannel;
use tairix_users::{PasswordRecord, Salt, MIN_ITERATIONS, SALT_LEN};

/// An in-memory `users_admin` endpoint recording the record it was sent.
struct MemChannel {
    fail: Option<Errno>,
    set: RefCell<Vec<(String, String)>>,
}

impl MemChannel {
    const fn new() -> Self {
        Self {
            fail: None,
            set: RefCell::new(Vec::new()),
        }
    }

    const fn failing(err: Errno) -> Self {
        Self {
            fail: Some(err),
            set: RefCell::new(Vec::new()),
        }
    }
}

impl AdminChannel for MemChannel {
    fn call(&self, req: &[u8], _out: &mut [u8]) -> Result<usize, Errno> {
        if let UsersAdminRequest::SetPassword {
            username,
            password_record,
        } = UsersAdminRequest::decode(req)?
        {
            self.set
                .borrow_mut()
                .push((username.to_string(), password_record.to_string()));
        }
        self.fail.map_or(Ok(0), Err)
    }
}

/// A terminal answering a fixed script of secrets.
struct ScriptedTerminal(RefCell<Vec<Option<Vec<u8>>>>);

impl ScriptedTerminal {
    fn new(lines: &[Option<&[u8]>]) -> Self {
        Self(RefCell::new(
            lines
                .iter()
                .map(|line| line.map(<[u8]>::to_vec))
                .rev()
                .collect(),
        ))
    }
}

impl Terminal for ScriptedTerminal {
    fn read_secret(&self, _prompt: &str) -> Option<Vec<u8>> {
        self.0.borrow_mut().pop().flatten()
    }
}

/// A fixed salt, so a test asserts a value rather than a shape.
struct FixedSalt;

impl SaltSource for FixedSalt {
    fn salt(&self) -> Option<Salt> {
        Some([7u8; SALT_LEN])
    }
}

/// A machine with no randomness at all.
struct NoSalt;

impl SaltSource for NoSalt {
    fn salt(&self) -> Option<Salt> {
        None
    }
}

/// A help tree holding the tool's own canonical document.
struct OneDoc;

impl HelpSource for OneDoc {
    fn locale_dirs(&self) -> Result<Vec<String>, SourceError> {
        Ok(alloc::vec![String::from("en-US")])
    }

    fn read(&self, locale_dir: &str, file_name: &str) -> Result<Option<Vec<u8>>, SourceError> {
        if locale_dir == "en-US" && file_name == "passwd.md" {
            Ok(Some(
                b"## NAME\n\npasswd \xe2\x80\x94 set a password\n\n\
                  ## SYNOPSIS\n\n`passwd NAME`\n"
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

fn set(secret: Secret) -> Command {
    Command::Set {
        name: String::from("ada"),
        secret,
    }
}

fn go(
    channel: &MemChannel,
    secret: Secret,
    terminal: &dyn Terminal,
    entropy: &dyn SaltSource,
) -> Result<(), RunError> {
    run(
        set(secret),
        None,
        channel,
        &OneDoc,
        terminal,
        entropy,
        &MemOut(RefCell::new(Vec::new())),
    )
}

#[test]
fn two_matching_prompts_become_one_salted_record_the_syscall_carries() {
    let channel = MemChannel::new();
    let terminal = ScriptedTerminal::new(&[Some(b"correct horse"), Some(b"correct horse")]);
    go(&channel, Secret::Prompt, &terminal, &FixedSalt).expect("the password is set");
    let set = channel.set.borrow();
    let (name, record) = set.first().expect("one request");
    assert_eq!(name, "ada");
    // What crossed the syscall is a record, never the password.
    assert!(record.starts_with("pbkdf2-sha256$"));
    assert!(!record.contains("correct horse"));
    // And it is one the format's own decoder takes back.
    PasswordRecord::decode(record).expect("a well-formed record");
}

#[test]
fn a_mismatch_absent_input_and_absent_randomness_each_send_nothing() {
    let channel = MemChannel::new();
    assert_eq!(
        go(
            &channel,
            Secret::Prompt,
            &ScriptedTerminal::new(&[Some(b"one"), Some(b"two")]),
            &FixedSalt
        ),
        Err(RunError::Mismatch)
    );
    assert_eq!(
        go(
            &channel,
            Secret::Prompt,
            &ScriptedTerminal::new(&[None]),
            &FixedSalt
        ),
        Err(RunError::NoInput)
    );
    assert_eq!(
        go(
            &channel,
            Secret::Prompt,
            &ScriptedTerminal::new(&[Some(b"x"), None]),
            &FixedSalt
        ),
        Err(RunError::NoInput)
    );
    assert_eq!(
        go(
            &channel,
            Secret::Prompt,
            &ScriptedTerminal::new(&[Some(b"x"), Some(b"x")]),
            &NoSalt
        ),
        Err(RunError::NoEntropy)
    );
    assert!(channel.set.borrow().is_empty());
}

#[test]
fn a_ready_record_is_validated_before_it_is_stored() {
    let channel = MemChannel::new();
    let record = PasswordRecord::new(b"secret", [3u8; SALT_LEN], MIN_ITERATIONS)
        .expect("builds")
        .encode();
    go(
        &channel,
        Secret::Record(record.clone()),
        &ScriptedTerminal::new(&[]),
        &NoSalt,
    )
    .expect("the record is accepted");
    assert_eq!(
        channel.set.borrow().first().map(|(_, r)| r.clone()),
        Some(record)
    );

    // A word the format's own decoder will not take is refused rather
    // than becoming a credential nothing can ever match. The prompting
    // path is not reached, so no terminal or randomness is needed.
    assert_eq!(
        go(
            &channel,
            Secret::Record(String::from("not-a-record")),
            &ScriptedTerminal::new(&[]),
            &NoSalt
        ),
        Err(RunError::BadRecord)
    );
    assert_eq!(channel.set.borrow().len(), 1);
}

#[test]
fn a_refusal_carries_the_kernel_errno_and_its_one_wording() {
    let channel = MemChannel::failing(Errno::PermissionDenied);
    let err = go(
        &channel,
        Secret::Prompt,
        &ScriptedTerminal::new(&[Some(b"x"), Some(b"x")]),
        &FixedSalt,
    )
    .expect_err("the replacement is refused");
    assert_eq!(err, RunError::Refused(Errno::PermissionDenied));
    assert_eq!(alloc::format!("{err}"), "permission denied");
}

#[test]
fn the_short_help_renders_the_bundle_document() {
    let channel = MemChannel::new();
    let out = MemOut(RefCell::new(Vec::new()));
    run(
        Command::Help,
        None,
        &channel,
        &OneDoc,
        &ScriptedTerminal::new(&[]),
        &NoSalt,
        &out,
    )
    .expect("the help renders");
    let text = String::from_utf8(out.0.borrow().clone()).expect("utf8");
    assert!(text.contains("passwd"));
    assert!(channel.set.borrow().is_empty());
}
