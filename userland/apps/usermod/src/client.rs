//! Running a parsed [`crate::Command`]: the short help, or the
//! sequence of `users_admin` operations the change set implies.

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

use tairix_abi::users_admin::{gid_list_into, grant_list_into, ModifyUser, UsersAdminRequest};
use tairix_abi::{CapabilityId, Errno};
use tairix_help::HelpSource;
use tairix_useradmin::{account, parse_grants, refusal, submit, Account, AdminChannel};

use crate::command::{Changes, Command, Lock};

/// Writes rendered bytes to the terminal.
pub trait Output {
    /// Write every byte of `bytes`.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the stream raises (e.g. a closed terminal).
    fn write_all(&self, bytes: &[u8]) -> Result<(), Errno>;
}

/// Which operation a run was on when it stopped.
///
/// A command line may ask for several, and the kernel applies one at a
/// time; naming the step is what lets a reader tell "nothing happened"
/// from "the fields landed and the lock did not".
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Step {
    /// Reading the account's current record, before anything was sent.
    Read,
    /// Replacing the account's identity fields.
    Identity,
    /// Replacing the account's capability grant ceiling.
    Grants,
    /// Setting the account's lock state.
    LockState,
}

impl Step {
    /// The word the diagnostic names this step by.
    const fn word(self) -> &'static str {
        match self {
            Self::Read => "read the account",
            Self::Identity => "change the fields",
            Self::Grants => "set the grants",
            Self::LockState => "set the lock state",
        }
    }

    /// Whether a refusal at this step may have left an earlier one
    /// applied, which is what the diagnostic has to warn about.
    const fn follows_a_write(self) -> bool {
        matches!(self, Self::Grants | Self::LockState)
    }
}

/// Why a `usermod` run did not complete.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RunError {
    /// The registry refused an operation, at this step.
    Refused(Step, Errno),
    /// The listing holds no such account, so nothing was sent.
    NoSuchAccount,
    /// A `--grants` word is not a capability this build knows.
    UnknownGrant(String),
    /// The change set does not fit one request.
    TooLarge,
    /// The short help could not be written to the terminal.
    Output(Errno),
}

impl fmt::Display for RunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Refused(step, err) => {
                write!(f, "could not {}: {}", step.word(), refusal(*err))?;
                if step.follows_a_write() {
                    // Each switch is its own kernel operation, so an
                    // earlier one may already be durable. Saying so is the
                    // difference between a diagnosis and a mystery.
                    f.write_str(" (an earlier change in this command may already be in effect)")?;
                }
                Ok(())
            }
            Self::NoSuchAccount => f.write_str("no such account"),
            Self::UnknownGrant(name) => write!(f, "unknown capability `{name}`"),
            Self::TooLarge => f.write_str("the change does not fit one request"),
            Self::Output(_) => f.write_str("could not write to the terminal"),
        }
    }
}

/// Run `command` against the injected seams.
///
/// # Errors
///
/// [`RunError`] naming the step that refused, an account the listing does
/// not hold, an unreadable grant name, an over-large change, or a dead
/// output stream.
pub fn run(
    command: Command,
    locale: Option<&str>,
    channel: &dyn AdminChannel,
    help: &dyn HelpSource,
    out: &dyn Output,
) -> Result<(), RunError> {
    let (name, changes) = match command {
        Command::Help => {
            let text = tairix_help::own_short_help(help, locale, "usermod")
                .unwrap_or_else(|| crate::USAGE.as_bytes().to_vec());
            return out.write_all(&text).map_err(RunError::Output);
        }
        Command::Modify { name, changes } => (name, changes),
    };
    // Read first: `ModifyUser` carries a whole record, and an account the
    // listing does not hold is refused before a byte is sent.
    let current = account(channel, &name)
        .map_err(|err| RunError::Refused(Step::Read, err))?
        .ok_or(RunError::NoSuchAccount)?;
    if changes.touches_identity() {
        modify_identity(channel, &current, &changes)?;
    }
    if let Some(list) = changes.grants.as_deref() {
        set_grants(channel, &name, list)?;
    }
    if let Some(lock) = changes.lock {
        submit(
            channel,
            &UsersAdminRequest::SetAccountState {
                username: &name,
                locked: lock == Lock::Locked,
            },
        )
        .map_err(|err| RunError::Refused(Step::LockState, err))?;
    }
    Ok(())
}

/// Resend the account's whole non-security field set with `changes`
/// applied, leaving every field nobody named exactly as it is held.
fn modify_identity(
    channel: &dyn AdminChannel,
    held: &Account,
    changes: &Changes,
) -> Result<(), RunError> {
    let gids: Vec<u32> = changes
        .supplementary_gids
        .clone()
        .unwrap_or_else(|| held.supplementary_gids.clone());
    let mut gid_backing = alloc::vec![0u8; 4 * gids.len()];
    let supplementary_gids =
        gid_list_into(&gids, &mut gid_backing).map_err(|_| RunError::TooLarge)?;
    let request = UsersAdminRequest::ModifyUser(ModifyUser {
        username: &held.username,
        primary_gid: changes.primary_gid.unwrap_or(held.primary_gid),
        supplementary_gids,
        display_name: changes.comment.as_deref().unwrap_or(&held.display_name),
        home: changes.home.as_deref().unwrap_or(&held.home),
        shell: changes.shell.as_deref().unwrap_or(&held.shell),
    });
    submit(channel, &request).map_err(|err| RunError::Refused(Step::Identity, err))
}

/// Replace the account's capability grant ceiling.
fn set_grants(channel: &dyn AdminChannel, name: &str, list: &str) -> Result<(), RunError> {
    let grants: Vec<CapabilityId> =
        parse_grants(list).map_err(|word| RunError::UnknownGrant(String::from(word)))?;
    let mut backing = alloc::vec![0u8; 2 * grants.len()];
    let grants = grant_list_into(&grants, &mut backing).map_err(|_| RunError::TooLarge)?;
    submit(
        channel,
        &UsersAdminRequest::SetGrants {
            username: name,
            grants,
        },
    )
    .map_err(|err| RunError::Refused(Step::Grants, err))
}

#[cfg(test)]
#[path = "client_tests.rs"]
mod tests;
