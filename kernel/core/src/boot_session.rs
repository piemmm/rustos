//! The operator's one-boot login choice ([`BootSession`]).
//!
//! The pre-boot Supervisor runs before any volume is mounted, so its
//! `continue text` / `continue gui` cannot be written to the configuration
//! store — and a one-boot override is not persistent policy in any case. The
//! root-unlock boot path records the choice in [`LATE_BOOT_SESSION`] instead,
//! and the ungated `boot_session_get` syscall serves it to `login`
//! (`plans/NEW-DESKTOP-LOGIN.md` G1).
//!
//! The value is public boot state: it names no account, grants no authority,
//! and reveals no secret, so any task may read it. It is nevertheless
//! **set-once**, so a later userland process cannot rewrite what the operator
//! chose at the physical console.

use tairix_abi::BootSession;
use tairix_sync::OnceCell;

/// The set-once cell holding the operator's one-boot login choice.
///
/// Installed by the root-unlock boot path from the Supervisor's exit; read by
/// the `boot_session_get` syscall handler. A boot that never entered the
/// Supervisor leaves it empty and reports [`BootSession::Unset`], which leaves
/// the stored `os.loginType` default in charge.
pub struct LateBootSession {
    cell: OnceCell<BootSession>,
}

impl LateBootSession {
    /// An empty cell; [`get`](Self::get) is [`BootSession::Unset`] until an
    /// install.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            cell: OnceCell::new(),
        }
    }

    /// Record the operator's choice. First-wins: a second install is ignored
    /// rather than replacing a choice already made at the console.
    ///
    /// [`BootSession::Unset`] is the *absence* of a choice, not a choice, so
    /// installing it is a no-op: recording it would burn the one install and
    /// silently discard a real choice made later in the same boot (a bare
    /// `continue` at the boot screen, then `continue gui` at the passphrase
    /// prompt).
    pub fn install(&self, session: BootSession) {
        if matches!(session, BootSession::Unset) {
            return;
        }
        let _ = self.cell.set(session);
    }

    /// The recorded choice, or [`BootSession::Unset`] before any install.
    #[must_use]
    pub fn get(&self) -> BootSession {
        match self.cell.get() {
            Ok(Some(session)) => *session,
            _ => BootSession::Unset,
        }
    }
}

impl Default for LateBootSession {
    fn default() -> Self {
        Self::new()
    }
}

/// The one production cell: the root-unlock boot path installs into it and the
/// `boot_session_get` syscall handler reads it.
pub static LATE_BOOT_SESSION: LateBootSession = LateBootSession::new();

#[cfg(test)]
mod tests {
    use super::LateBootSession;
    use tairix_abi::BootSession;

    #[test]
    fn an_uninstalled_cell_reports_unset() {
        assert_eq!(LateBootSession::new().get(), BootSession::Unset);
    }

    #[test]
    fn the_first_choice_wins_and_is_never_overwritten() {
        let cell = LateBootSession::new();
        cell.install(BootSession::Graphical);
        assert_eq!(cell.get(), BootSession::Graphical);
        cell.install(BootSession::Text);
        assert_eq!(cell.get(), BootSession::Graphical);
    }

    #[test]
    fn installing_unset_is_a_no_op_that_leaves_a_later_choice_room() {
        let cell = LateBootSession::new();
        cell.install(BootSession::Unset);
        assert_eq!(cell.get(), BootSession::Unset);
        // The bare `continue` above made no choice, so the real one still
        // lands.
        cell.install(BootSession::Text);
        assert_eq!(cell.get(), BootSession::Text);
    }
}
