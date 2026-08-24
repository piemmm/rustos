//! The interactive account-administration session: command grammar,
//! typed `users_admin` request encoding, and response rendering, behind
//! host-testable seams.

use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use tairix_abi::users_admin::{
    decode_group_list, decode_user_list, gid_list_into, grant_list_into, AccountStateCode,
    CreateUser, UsersAdminRequest, USERS_ADMIN_MAX_REQUEST,
};
use tairix_abi::{CapabilityId, Errno};
use tairix_users::{
    default_home, PasswordRecord, Salt, DEFAULT_ITERATIONS, DEFAULT_SHELL, MAX_DB_LEN,
    SESSION_BASELINE,
};

/// The terminal the session runs over — the inherited standard streams,
/// never a device.
pub trait ToolIo {
    /// Write one line to standard output.
    fn write_line(&mut self, line: &str);
    /// Write one line to standard error.
    fn error_line(&mut self, line: &str);
    /// Print `prompt` (no newline) and read one echoed line; `None` on
    /// end-of-input.
    fn read_line(&mut self, prompt: &str) -> Option<String>;
    /// Print `prompt` (no newline) and read one line with terminal echo
    /// off; `None` on end-of-input. Returned as raw bytes so the caller
    /// can zeroise the secret after use.
    fn read_secret(&mut self, prompt: &str) -> Option<Vec<u8>>;
}

/// The `users_admin` syscall seam (the `tairix_rt::users_admin` shape).
pub trait AdminChannel {
    /// Submit one encoded request; a list response is written into `out`
    /// and its byte length returned. Errors are the raw negative kernel
    /// result (`-errno`).
    ///
    /// # Errors
    ///
    /// The raw negative kernel result.
    fn call(&mut self, req: &[u8], out: &mut [u8]) -> Result<usize, i64>;
}

/// The salt source for client-side password hashing (the kernel CSPRNG
/// in production; fixed bytes in tests).
pub trait SaltSource {
    /// Draw one fresh random salt; `None` when no randomness is
    /// available (the operation is refused — never a guessed salt).
    fn salt(&mut self) -> Option<Salt>;
}

/// Session tuning.
#[derive(Copy, Clone, Debug)]
pub struct SessionConfig {
    /// PBKDF2 iteration count for newly set passwords. The production
    /// default; tests use the format's minimum to stay fast.
    pub iterations: u32,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            iterations: DEFAULT_ITERATIONS,
        }
    }
}

/// The response buffer capacity every call hands the kernel: the
/// kernel's own list-response bound (twice the on-disk database
/// maximum), so a full listing always fits.
const RESPONSE_CAPACITY: usize = 2 * MAX_DB_LEN;

/// The interactive prompt.
const PROMPT: &str = "users> ";

/// One terse usage summary, printed by `help` and on an unknown command.
const HELP: &[&str] = &[
    "commands:",
    "  list                       list accounts",
    "  groups                     list groups",
    "  create <name> <uid> <gid>  create an account (prompts for details)",
    "  passwd <name>              replace an account's password",
    "  lock <name> | unlock <name>",
    "  grant <name> <CAP_...>     add one capability to an account's ceiling",
    "  revoke <name> <CAP_...>    remove one capability from it",
    "  deluser <name>             delete an account",
    "  addgroup <name> <gid> | delgroup <name>",
    "  help | exit",
];

/// Run the interactive session until `exit` or end-of-input. Returns the
/// process exit code (`0`; refusals are reported per command and the
/// session continues).
pub fn run_session(
    io: &mut dyn ToolIo,
    channel: &mut dyn AdminChannel,
    salt: &mut dyn SaltSource,
    config: SessionConfig,
) -> i32 {
    io.write_line("users: account administration (help for commands)");
    loop {
        let Some(line) = io.read_line(PROMPT) else {
            return 0;
        };
        let mut words = line.split_whitespace();
        let Some(command) = words.next() else {
            continue;
        };
        let args: Vec<&str> = words.collect();
        match command {
            "exit" | "quit" => return 0,
            "help" => {
                for line in HELP {
                    io.write_line(line);
                }
            }
            "list" => list_users(io, channel),
            "groups" => list_groups(io, channel),
            "create" => create_user(io, channel, salt, config, &args),
            "passwd" => set_password(io, channel, salt, config, &args),
            "lock" => set_state(io, channel, &args, true),
            "unlock" => set_state(io, channel, &args, false),
            "grant" => edit_grants(io, channel, &args, true),
            "revoke" => edit_grants(io, channel, &args, false),
            "deluser" => delete_user(io, channel, &args),
            "addgroup" => add_group(io, channel, &args),
            "delgroup" => delete_group(io, channel, &args),
            _ => {
                io.error_line("users: unknown command (help for commands)");
            }
        }
    }
}

/// Encode `request` and submit it, reporting a refusal as a terse error
/// line; a mutating operation's success prints `ok`.
fn submit_mutation(
    io: &mut dyn ToolIo,
    channel: &mut dyn AdminChannel,
    request: &UsersAdminRequest<'_>,
) {
    let mut req_buf = [0u8; USERS_ADMIN_MAX_REQUEST];
    let Ok(encoded) = request.encode_into(&mut req_buf) else {
        io.error_line("users: request too large");
        return;
    };
    match channel.call(&req_buf[..encoded], &mut []) {
        Ok(_) => io.write_line("ok"),
        Err(err) => io.error_line(&format!("users: {}", errno_message(err))),
    }
    // The request may carry a password record (a salted hash, but still
    // credential material): scrub the encode buffer.
    req_buf.fill(0);
}

/// Submit a list request, returning the response bytes.
fn submit_list(
    io: &mut dyn ToolIo,
    channel: &mut dyn AdminChannel,
    request: &UsersAdminRequest<'_>,
) -> Option<Vec<u8>> {
    let mut req_buf = [0u8; USERS_ADMIN_MAX_REQUEST];
    let Ok(encoded) = request.encode_into(&mut req_buf) else {
        io.error_line("users: request too large");
        return None;
    };
    let mut out = vec![0u8; RESPONSE_CAPACITY];
    match channel.call(&req_buf[..encoded], &mut out) {
        Ok(len) => {
            out.truncate(len.min(RESPONSE_CAPACITY));
            Some(out)
        }
        Err(err) => {
            io.error_line(&format!("users: {}", errno_message(err)));
            None
        }
    }
}

fn list_users(io: &mut dyn ToolIo, channel: &mut dyn AdminChannel) {
    let Some(response) = submit_list(io, channel, &UsersAdminRequest::ListUsers) else {
        return;
    };
    let Ok(entries) = decode_user_list(&response) else {
        io.error_line("users: malformed response");
        return;
    };
    io.write_line("NAME                             UID        GID        STATE   GRANTS");
    for entry in entries {
        let Ok(entry) = entry else {
            io.error_line("users: malformed response");
            return;
        };
        let mut grants = String::new();
        for cap in entry.grants.iter() {
            if !grants.is_empty() {
                grants.push(',');
            }
            grants.push_str(cap.name().unwrap_or("CAP_?"));
        }
        io.write_line(&format!(
            "{:<32} {:<10} {:<10} {:<7} {}",
            entry.username,
            entry.uid,
            entry.primary_gid,
            match entry.state {
                AccountStateCode::Active => "active",
                AccountStateCode::Locked => "locked",
                AccountStateCode::NoLogin => "nologin",
            },
            grants,
        ));
    }
}

fn list_groups(io: &mut dyn ToolIo, channel: &mut dyn AdminChannel) {
    let Some(response) = submit_list(io, channel, &UsersAdminRequest::ListGroups) else {
        return;
    };
    let Ok(entries) = decode_group_list(&response) else {
        io.error_line("users: malformed response");
        return;
    };
    io.write_line("NAME                             GID");
    for entry in entries {
        let Ok(entry) = entry else {
            io.error_line("users: malformed response");
            return;
        };
        io.write_line(&format!("{:<32} {}", entry.name, entry.gid));
    }
}

/// Prompt twice for a new password (echo off) and build its salted
/// PBKDF2 record; `None` when the prompts disagree, input ends, no salt
/// is available, or the password violates the record bounds. Both
/// plaintext buffers are zeroised before returning.
fn build_password_record(
    io: &mut dyn ToolIo,
    salt: &mut dyn SaltSource,
    config: SessionConfig,
) -> Option<String> {
    let mut first = io.read_secret("New password: ")?;
    let Some(mut second) = io.read_secret("Repeat password: ") else {
        first.fill(0);
        return None;
    };
    let record = encode_password_record(io, salt, config, &first, &second);
    // Zeroise both plaintext buffers before they are released.
    first.fill(0);
    second.fill(0);
    record
}

/// Hash a confirmed password into its salted PBKDF2 record; every refusal
/// is reported and yields `None` (the caller scrubs the plaintext).
fn encode_password_record(
    io: &mut dyn ToolIo,
    salt: &mut dyn SaltSource,
    config: SessionConfig,
    first: &[u8],
    second: &[u8],
) -> Option<String> {
    if first != second {
        io.error_line("users: passwords do not match");
        return None;
    }
    let Some(salt) = salt.salt() else {
        io.error_line("users: no randomness available");
        return None;
    };
    if let Ok(record) = PasswordRecord::new(first, salt, config.iterations) {
        Some(record.encode())
    } else {
        io.error_line("users: password rejected (length bounds)");
        None
    }
}

fn create_user(
    io: &mut dyn ToolIo,
    channel: &mut dyn AdminChannel,
    salt: &mut dyn SaltSource,
    config: SessionConfig,
    args: &[&str],
) {
    let [name, uid, gid] = args else {
        io.error_line("usage: create <name> <uid> <gid>");
        return;
    };
    let (Ok(uid), Ok(gid)) = (uid.parse::<u32>(), gid.parse::<u32>()) else {
        io.error_line("users: uid and gid must be decimal integers");
        return;
    };
    let Some(display_name) = io.read_line("Display name (may be empty): ") else {
        return;
    };
    let Some(password_record) = build_password_record(io, salt, config) else {
        return;
    };

    // A created account starts from the shared session baseline; an
    // administrator widens it afterwards with `grant`, bounded by their
    // own effective set (the kernel enforces never-widen).
    let mut grant_backing = [0u8; 2 * SESSION_BASELINE.len()];
    let Ok(grants) = grant_list_into(SESSION_BASELINE, &mut grant_backing) else {
        io.error_line("users: request too large");
        return;
    };
    let Ok(gids) = gid_list_into(&[], &mut []) else {
        io.error_line("users: request too large");
        return;
    };
    let home = default_home(name);
    submit_mutation(
        io,
        channel,
        &UsersAdminRequest::CreateUser(CreateUser {
            username: name,
            uid,
            primary_gid: gid,
            supplementary_gids: gids,
            display_name: display_name.trim(),
            home: &home,
            shell: DEFAULT_SHELL,
            grants,
            password_record: &password_record,
        }),
    );
}

fn set_password(
    io: &mut dyn ToolIo,
    channel: &mut dyn AdminChannel,
    salt: &mut dyn SaltSource,
    config: SessionConfig,
    args: &[&str],
) {
    let [name] = args else {
        io.error_line("usage: passwd <name>");
        return;
    };
    let Some(password_record) = build_password_record(io, salt, config) else {
        return;
    };
    submit_mutation(
        io,
        channel,
        &UsersAdminRequest::SetPassword {
            username: name,
            password_record: &password_record,
        },
    );
}

fn set_state(io: &mut dyn ToolIo, channel: &mut dyn AdminChannel, args: &[&str], locked: bool) {
    let [name] = args else {
        io.error_line(if locked {
            "usage: lock <name>"
        } else {
            "usage: unlock <name>"
        });
        return;
    };
    submit_mutation(
        io,
        channel,
        &UsersAdminRequest::SetAccountState {
            username: name,
            locked,
        },
    );
}

/// Add (`grant`) or remove (`revoke`) one capability on an account's
/// ceiling: read the current grants from the listing, apply the edit,
/// and submit the full replacement set (the kernel bounds any addition
/// by the caller's own effective set).
fn edit_grants(io: &mut dyn ToolIo, channel: &mut dyn AdminChannel, args: &[&str], add: bool) {
    let [name, cap_name] = args else {
        io.error_line(if add {
            "usage: grant <name> <CAP_...>"
        } else {
            "usage: revoke <name> <CAP_...>"
        });
        return;
    };
    let Some(cap) = CapabilityId::from_name(cap_name) else {
        io.error_line("users: unknown capability name");
        return;
    };
    let Some(response) = submit_list(io, channel, &UsersAdminRequest::ListUsers) else {
        return;
    };
    let Ok(entries) = decode_user_list(&response) else {
        io.error_line("users: malformed response");
        return;
    };
    let mut grants: Option<Vec<CapabilityId>> = None;
    for entry in entries {
        let Ok(entry) = entry else {
            io.error_line("users: malformed response");
            return;
        };
        if entry.username == *name {
            grants = Some(entry.grants.iter().collect());
            break;
        }
    }
    let Some(mut grants) = grants else {
        io.error_line("users: no such account");
        return;
    };
    if add {
        if !grants.contains(&cap) {
            grants.push(cap);
        }
    } else {
        grants.retain(|held| *held != cap);
    }
    let mut grant_backing = vec![0u8; 2 * grants.len()];
    let Ok(grants) = grant_list_into(&grants, &mut grant_backing) else {
        io.error_line("users: request too large");
        return;
    };
    submit_mutation(
        io,
        channel,
        &UsersAdminRequest::SetGrants {
            username: name,
            grants,
        },
    );
}

fn delete_user(io: &mut dyn ToolIo, channel: &mut dyn AdminChannel, args: &[&str]) {
    let [name] = args else {
        io.error_line("usage: deluser <name>");
        return;
    };
    submit_mutation(
        io,
        channel,
        &UsersAdminRequest::DeleteUser { username: name },
    );
}

fn add_group(io: &mut dyn ToolIo, channel: &mut dyn AdminChannel, args: &[&str]) {
    let [name, gid] = args else {
        io.error_line("usage: addgroup <name> <gid>");
        return;
    };
    let Ok(gid) = gid.parse::<u32>() else {
        io.error_line("users: gid must be a decimal integer");
        return;
    };
    submit_mutation(io, channel, &UsersAdminRequest::CreateGroup { name, gid });
}

fn delete_group(io: &mut dyn ToolIo, channel: &mut dyn AdminChannel, args: &[&str]) {
    let [name] = args else {
        io.error_line("usage: delgroup <name>");
        return;
    };
    submit_mutation(io, channel, &UsersAdminRequest::DeleteGroup { name });
}

/// Render a raw negative kernel result (`-errno`) as a terse, stable
/// message.
fn errno_message(err: i64) -> &'static str {
    match Errno::try_from_syscall(err) {
        Some(Errno::PermissionDenied) => "permission denied",
        Some(Errno::NotFound) => "no such account or group",
        Some(Errno::AlreadyExists) => "already exists",
        Some(Errno::NoSpace) => "database full",
        Some(Errno::NotImplemented) => "account administration unavailable",
        Some(Errno::LengthOutOfRange | Errno::OutOfRange) => "malformed field",
        _ => "operation failed",
    }
}

#[cfg(test)]
#[path = "session_tests.rs"]
mod tests;
