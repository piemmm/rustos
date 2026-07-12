//! Host tests for the interactive session: scripted terminals drive the
//! command grammar, and a recording channel asserts the exact typed
//! requests the tool submits (and how refusals render).

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use rustos_abi::users_admin::{
    gid_list_into, grant_list_into, GroupEntry, ListResponseBuilder, UserEntry, UsersAdminRequest,
};
use rustos_abi::{CapabilityId, Errno};
use rustos_users::{PasswordRecord, SESSION_BASELINE};

use super::{run_session, AdminChannel, SaltSource, SessionConfig, ToolIo};

/// A scripted terminal: queued input lines/secrets, captured output.
struct ScriptedIo {
    lines: Vec<String>,
    secrets: Vec<Vec<u8>>,
    out: Vec<String>,
    errors: Vec<String>,
}

impl ScriptedIo {
    fn new(lines: &[&str], secrets: &[&[u8]]) -> Self {
        Self {
            lines: lines.iter().rev().map(|s| (*s).to_string()).collect(),
            secrets: secrets.iter().rev().map(|s| s.to_vec()).collect(),
            out: Vec::new(),
            errors: Vec::new(),
        }
    }
}

impl ToolIo for ScriptedIo {
    fn write_line(&mut self, line: &str) {
        self.out.push(line.to_string());
    }

    fn error_line(&mut self, line: &str) {
        self.errors.push(line.to_string());
    }

    fn read_line(&mut self, _prompt: &str) -> Option<String> {
        self.lines.pop()
    }

    fn read_secret(&mut self, _prompt: &str) -> Option<Vec<u8>> {
        self.secrets.pop()
    }
}

/// The scripted result of one channel call.
enum Reply {
    Ok(Vec<u8>),
    Err(i64),
}

/// A recording channel: captures every encoded request and answers from
/// a script.
struct RecordingChannel {
    requests: Vec<Vec<u8>>,
    replies: Vec<Reply>,
}

impl RecordingChannel {
    fn new(replies: Vec<Reply>) -> Self {
        let mut replies = replies;
        replies.reverse();
        Self {
            requests: Vec::new(),
            replies,
        }
    }

    fn ok() -> Self {
        Self::new(alloc::vec![Reply::Ok(Vec::new())])
    }
}

impl AdminChannel for RecordingChannel {
    fn call(&mut self, req: &[u8], out: &mut [u8]) -> Result<usize, i64> {
        self.requests.push(req.to_vec());
        match self.replies.pop() {
            Some(Reply::Ok(bytes)) => {
                if bytes.len() > out.len() {
                    return Err(-i64::from(Errno::BufferTooSmall.as_i32()));
                }
                out[..bytes.len()].copy_from_slice(&bytes);
                Ok(bytes.len())
            }
            Some(Reply::Err(err)) => Err(err),
            None => Err(-i64::from(Errno::NotImplemented.as_i32())),
        }
    }
}

struct FixedSalt;

impl SaltSource for FixedSalt {
    fn salt(&mut self) -> Option<rustos_users::Salt> {
        Some([0x42; rustos_users::SALT_LEN])
    }
}

fn config() -> SessionConfig {
    SessionConfig {
        iterations: rustos_users::MIN_ITERATIONS,
    }
}

/// Build a one-account list response for the grant-editing tests.
fn user_list_response() -> Vec<u8> {
    let mut grant_backing = [0u8; 2];
    let grants = grant_list_into(&[CapabilityId::FS_ACCESS], &mut grant_backing).expect("fits");
    let mut gid_backing = [0u8; 0];
    let gids = gid_list_into(&[], &mut gid_backing).expect("fits");
    let mut out = alloc::vec![0u8; 512];
    let mut builder = ListResponseBuilder::new(&mut out).expect("header fits");
    builder
        .push_user(&UserEntry {
            username: "ada",
            uid: 1000,
            primary_gid: 0,
            supplementary_gids: gids,
            display_name: "Ada",
            home: "/Users/ada",
            shell: "/System/Apps/elsh.app/Run",
            grants,
            state: rustos_abi::users_admin::AccountStateCode::Active,
        })
        .expect("entry fits");
    let len = builder.finish();
    out.truncate(len);
    out
}

/// Build a two-group list response for the rendering test.
fn group_list_response() -> Vec<u8> {
    let mut out = alloc::vec![0u8; 128];
    let mut builder = ListResponseBuilder::new(&mut out).expect("header fits");
    builder
        .push_group(&GroupEntry {
            name: "system",
            gid: 0,
        })
        .expect("fits");
    let len = builder.finish();
    out.truncate(len);
    out
}

#[test]
fn lock_and_unlock_encode_the_typed_state_request() {
    let mut io = ScriptedIo::new(&["lock ada", "unlock ada", "exit"], &[]);
    let mut channel =
        RecordingChannel::new(alloc::vec![Reply::Ok(Vec::new()), Reply::Ok(Vec::new()),]);
    assert_eq!(
        run_session(&mut io, &mut channel, &mut FixedSalt, config()),
        0
    );
    assert_eq!(channel.requests.len(), 2);
    assert_eq!(
        UsersAdminRequest::decode(&channel.requests[0]),
        Ok(UsersAdminRequest::SetAccountState {
            username: "ada",
            locked: true,
        })
    );
    assert_eq!(
        UsersAdminRequest::decode(&channel.requests[1]),
        Ok(UsersAdminRequest::SetAccountState {
            username: "ada",
            locked: false,
        })
    );
    assert_eq!(io.out.iter().filter(|line| *line == "ok").count(), 2);
}

#[test]
fn create_builds_a_baseline_account_with_a_verifiable_password_record() {
    let mut io = ScriptedIo::new(
        &["create grace 1001 100", "Grace Hopper", "exit"],
        &[b"lovelace", b"lovelace"],
    );
    let mut channel = RecordingChannel::ok();
    run_session(&mut io, &mut channel, &mut FixedSalt, config());

    assert_eq!(channel.requests.len(), 1);
    let decoded = UsersAdminRequest::decode(&channel.requests[0]).expect("decodes");
    let UsersAdminRequest::CreateUser(create) = decoded else {
        unreachable!("create submits a CreateUser request");
    };
    assert_eq!(create.username, "grace");
    assert_eq!(create.uid, 1001);
    assert_eq!(create.primary_gid, 100);
    assert_eq!(create.display_name, "Grace Hopper");
    assert_eq!(create.home, "/Users/grace");
    assert_eq!(create.shell, "/System/Apps/elsh.app/Run");
    // The new account starts from exactly the shared session baseline.
    let grants: Vec<CapabilityId> = create.grants.iter().collect();
    assert_eq!(grants, SESSION_BASELINE);
    // The submitted record is a real salted PBKDF2 record for the typed
    // password — never plaintext.
    assert!(!create
        .password_record
        .as_bytes()
        .windows(8)
        .any(|w| w == b"lovelace"));
    let record = PasswordRecord::decode(create.password_record).expect("valid record");
    assert!(record.verify(b"lovelace"));
    assert!(!record.verify(b"wrong"));
}

#[test]
fn mismatched_passwords_submit_nothing() {
    let mut io = ScriptedIo::new(&["passwd ada", "exit"], &[b"first", b"second"]);
    let mut channel = RecordingChannel::ok();
    run_session(&mut io, &mut channel, &mut FixedSalt, config());
    assert!(channel.requests.is_empty());
    assert!(io
        .errors
        .iter()
        .any(|line| line.contains("passwords do not match")));
}

#[test]
fn grant_merges_with_the_accounts_current_ceiling() {
    let mut io = ScriptedIo::new(&["grant ada CAP_PROC_SPAWN", "exit"], &[]);
    let mut channel = RecordingChannel::new(alloc::vec![
        Reply::Ok(user_list_response()),
        Reply::Ok(Vec::new()),
    ]);
    run_session(&mut io, &mut channel, &mut FixedSalt, config());

    assert_eq!(channel.requests.len(), 2);
    let decoded = UsersAdminRequest::decode(&channel.requests[1]).expect("decodes");
    let UsersAdminRequest::SetGrants { username, grants } = decoded else {
        unreachable!("grant submits a SetGrants request");
    };
    assert_eq!(username, "ada");
    let grants: Vec<CapabilityId> = grants.iter().collect();
    assert_eq!(
        grants,
        alloc::vec![CapabilityId::FS_ACCESS, CapabilityId::PROC_SPAWN]
    );
}

#[test]
fn revoke_removes_one_capability() {
    let mut io = ScriptedIo::new(&["revoke ada CAP_FS_ACCESS", "exit"], &[]);
    let mut channel = RecordingChannel::new(alloc::vec![
        Reply::Ok(user_list_response()),
        Reply::Ok(Vec::new()),
    ]);
    run_session(&mut io, &mut channel, &mut FixedSalt, config());
    let decoded = UsersAdminRequest::decode(&channel.requests[1]).expect("decodes");
    let UsersAdminRequest::SetGrants { grants, .. } = decoded else {
        unreachable!("revoke submits a SetGrants request");
    };
    assert!(grants.is_empty());
}

#[test]
fn listings_render_and_refusals_report_tersely() {
    let mut io = ScriptedIo::new(&["list", "groups", "deluser root", "exit"], &[]);
    let mut channel = RecordingChannel::new(alloc::vec![
        Reply::Ok(user_list_response()),
        Reply::Ok(group_list_response()),
        Reply::Err(-i64::from(Errno::PermissionDenied.as_i32())),
    ]);
    run_session(&mut io, &mut channel, &mut FixedSalt, config());

    assert!(io.out.iter().any(|line| line.contains("ada")
        && line.contains("active")
        && line.contains("CAP_FS_ACCESS")));
    assert!(io.out.iter().any(|line| line.contains("system")));
    assert!(io
        .errors
        .iter()
        .any(|line| line.contains("permission denied")));
}

#[test]
fn unknown_commands_and_bad_usage_report_without_calling() {
    let mut io = ScriptedIo::new(
        &["frobnicate", "lock", "grant ada CAP_NOT_A_THING", "exit"],
        &[],
    );
    let mut channel = RecordingChannel::new(Vec::new());
    run_session(&mut io, &mut channel, &mut FixedSalt, config());
    assert!(channel.requests.is_empty());
    assert_eq!(io.errors.len(), 3);
}
