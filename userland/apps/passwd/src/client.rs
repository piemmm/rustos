//! Running a parsed [`crate::Command`]: the short help, or the
//! one `users_admin` password replacement.

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

use tairix_abi::users_admin::UsersAdminRequest;
use tairix_abi::Errno;
use tairix_help::HelpSource;
use tairix_useradmin::{refusal, submit, AdminChannel};
use tairix_users::{PasswordRecord, Salt, DEFAULT_ITERATIONS};

use crate::command::{Command, Secret};

/// The terminal the prompts run over — the inherited standard streams,
/// never a device.
pub trait Terminal {
    /// Print `prompt` (no newline) and read one line with terminal echo
    /// off; `None` on end-of-input.
    ///
    /// Returned as raw bytes so the caller can zeroise the secret the
    /// moment it has been hashed.
    fn read_secret(&self, prompt: &str) -> Option<Vec<u8>>;
}

/// The randomness the salt is drawn from (the kernel CSPRNG in
/// production).
pub trait SaltSource {
    /// Draw one fresh random salt; `None` when no randomness is
    /// available, which refuses the operation rather than guessing one.
    fn salt(&self) -> Option<Salt>;
}

/// Writes rendered bytes to the terminal.
pub trait Output {
    /// Write every byte of `bytes`.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the stream raises (e.g. a closed terminal).
    fn write_all(&self, bytes: &[u8]) -> Result<(), Errno>;
}

/// Why a `passwd` run did not complete.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RunError {
    /// The registry refused or failed the replacement.
    Refused(Errno),
    /// The two prompts did not agree.
    Mismatch,
    /// Input ended before both prompts were answered.
    NoInput,
    /// No randomness was available, so no salt could be drawn.
    NoEntropy,
    /// The password is outside the record format's length bounds.
    BadPassword,
    /// The `--record` word is not a well-formed password record.
    BadRecord,
    /// The short help could not be written to the terminal.
    Output(Errno),
}

impl fmt::Display for RunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Refused(err) => f.write_str(refusal(*err)),
            Self::Mismatch => f.write_str("passwords do not match"),
            Self::NoInput => f.write_str("no password was entered"),
            Self::NoEntropy => f.write_str("no randomness available"),
            Self::BadPassword => f.write_str("password rejected (length bounds)"),
            Self::BadRecord => f.write_str("malformed password record"),
            Self::Output(_) => f.write_str("could not write to the terminal"),
        }
    }
}

/// The prompts the interactive form shows.
const FIRST_PROMPT: &str = "New password: ";
/// The confirmation prompt.
const REPEAT_PROMPT: &str = "Repeat password: ";

/// Run `command` against the injected seams.
///
/// # Errors
///
/// [`RunError`] for a refused replacement, a mismatched or absent
/// password, an unavailable salt, a malformed record, or a dead output
/// stream.
pub fn run(
    command: Command,
    locale: Option<&str>,
    channel: &dyn AdminChannel,
    help: &dyn HelpSource,
    terminal: &dyn Terminal,
    entropy: &dyn SaltSource,
    out: &dyn Output,
) -> Result<(), RunError> {
    let (name, secret) = match command {
        Command::Help => {
            let text = tairix_help::own_short_help(help, locale, "passwd")
                .unwrap_or_else(|| crate::USAGE.as_bytes().to_vec());
            return out.write_all(&text).map_err(RunError::Output);
        }
        Command::Set { name, secret } => (name, secret),
    };
    let record = match secret {
        Secret::Record(record) => {
            // Validated before it is stored: a word the format's own
            // decoder will not take is refused here rather than becoming
            // a credential nothing can ever match.
            PasswordRecord::decode(&record).map_err(|_| RunError::BadRecord)?;
            record
        }
        Secret::Prompt => prompted_record(terminal, entropy)?,
    };
    submit(
        channel,
        &UsersAdminRequest::SetPassword {
            username: &name,
            password_record: &record,
        },
    )
    .map_err(RunError::Refused)
}

/// Prompt twice with echo off and hash the confirmed password into its
/// salted record, zeroising both plaintext buffers before returning.
fn prompted_record(terminal: &dyn Terminal, entropy: &dyn SaltSource) -> Result<String, RunError> {
    let mut first = terminal
        .read_secret(FIRST_PROMPT)
        .ok_or(RunError::NoInput)?;
    let second = terminal.read_secret(REPEAT_PROMPT);
    let built = second
        .as_deref()
        .ok_or(RunError::NoInput)
        .and_then(|second| hash(&first, second, entropy));
    first.fill(0);
    if let Some(mut second) = second {
        second.fill(0);
    }
    built
}

/// Hash a confirmed password into its salted PBKDF2 record.
fn hash(first: &[u8], second: &[u8], entropy: &dyn SaltSource) -> Result<String, RunError> {
    if first != second {
        return Err(RunError::Mismatch);
    }
    let salt = entropy.salt().ok_or(RunError::NoEntropy)?;
    PasswordRecord::new(first, salt, DEFAULT_ITERATIONS)
        .map(|record| record.encode())
        .map_err(|_| RunError::BadPassword)
}

#[cfg(test)]
#[path = "client_tests.rs"]
mod tests;
