//! Authenticate and decode the desktop session's commands on this
//! instance's per-instance mailbox
//! ([`tairix_abi::switchboard_ipc::command_endpoint_for`]).
//!
//! Every command is authenticated against the **kernel-attested** origin of
//! the sender, never a wire claim: the mailbox is unrestricted-sender at
//! bind (any process may send to it), so the one fact that actually gates a
//! command is whether the kernel itself vouches that the sender is the
//! desktop session this instance's publish reply named. A message from
//! anyone else — or one that does not even decode as an [`Origin`] — is
//! dropped before it ever reaches [`SwitchboardCommand::from_bytes`].

use tairix_abi::switchboard_ipc::SwitchboardCommand;
use tairix_abi::{Errno, Origin, ProcId, ORIGIN_WIRE_LEN};

/// `true` when `sender` decodes as an [`Origin`] whose attested
/// [`ProcId`](Origin::proc_id) is `session` — the one authentication check
/// every command on the per-instance mailbox must pass before it is
/// decoded, let alone applied.
#[must_use]
pub fn is_from_session(sender: &[u8; ORIGIN_WIRE_LEN], session: ProcId) -> bool {
    Origin::from_bytes(sender).is_ok_and(|origin| origin.proc_id() == session)
}

/// Authenticate and decode one command frame.
///
/// # Errors
///
/// [`Errno::PermissionDenied`] when `sender` is not the attested `session` —
/// the frame is never even looked at in that case, so a forged or malformed
/// sender record cannot smuggle a well-formed command past the identity
/// check. Otherwise, [`SwitchboardCommand::from_bytes`]'s own typed refusal
/// for a malformed frame.
pub fn authenticate_command(
    bytes: &[u8],
    sender: &[u8; ORIGIN_WIRE_LEN],
    session: ProcId,
) -> Result<SwitchboardCommand, Errno> {
    if !is_from_session(sender, session) {
        return Err(Errno::PermissionDenied);
    }
    SwitchboardCommand::from_bytes(bytes)
}

#[cfg(test)]
#[path = "command_tests.rs"]
mod tests;
