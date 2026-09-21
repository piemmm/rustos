//! Running a parsed [`crate::Command`]: the short help, or the
//! one `users_admin` deletion.

use core::fmt;

use tairix_abi::users_admin::UsersAdminRequest;
use tairix_abi::Errno;
use tairix_help::HelpSource;
use tairix_useradmin::{refusal, submit, AdminChannel};

use crate::command::Command;

/// Writes rendered bytes to the terminal.
pub trait Output {
    /// Write every byte of `bytes`.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the stream raises (e.g. a closed terminal).
    fn write_all(&self, bytes: &[u8]) -> Result<(), Errno>;
}

/// Why a `userdel` run did not complete.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RunError {
    /// The registry refused or failed the deletion.
    Refused(Errno),
    /// The short help could not be written to the terminal.
    Output(Errno),
}

impl fmt::Display for RunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Refused(err) => f.write_str(refusal(*err)),
            Self::Output(_) => f.write_str("could not write to the terminal"),
        }
    }
}

/// Run `command` against the injected seams.
///
/// # Errors
///
/// [`RunError::Refused`] when the registry refused the deletion, and
/// [`RunError::Output`] when the short help could not be written.
pub fn run(
    command: Command,
    locale: Option<&str>,
    channel: &dyn AdminChannel,
    help: &dyn HelpSource,
    out: &dyn Output,
) -> Result<(), RunError> {
    match command {
        Command::Help => {
            let text = tairix_help::own_short_help(help, locale, "userdel")
                .unwrap_or_else(|| crate::USAGE.as_bytes().to_vec());
            out.write_all(&text).map_err(RunError::Output)
        }
        Command::Delete(name) => {
            let name = name.as_str();
            submit(channel, &UsersAdminRequest::DeleteUser { username: name })
                .map_err(RunError::Refused)
        }
    }
}

#[cfg(test)]
#[path = "client_tests.rs"]
mod tests;
