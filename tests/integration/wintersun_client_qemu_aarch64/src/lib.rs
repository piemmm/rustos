//! The `WinterSun` client vertical's shared contract (`plans/WINTERSUN.md`
//! WS23).
//!
//! The freestanding guest kernel (`src/main.rs`) and the host runner's
//! enrolment (`tools/xtask/src/commands/qemu_tests.rs`) both read these
//! definitions, so what the host types and what the guest latches cannot
//! drift apart.
//!
//! # Who states what
//!
//! - The **host** reads the serial transcript, so it gates each keystroke,
//!   click and screendump on the desktop session's own announcements: the
//!   game's window first on screen, and each size state on screen at the
//!   extent it gave.
//! - The **guest** kernel's audit sink sees kernel audit records only, so it
//!   gates on those: the bundles the shell loads and the window channel's
//!   create reply.
//!
//! Neither side infers the other's facts.

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

/// Bare name of the game's bundle in the system application store.
pub const GAME_APP_NAME: &str = "wintersun";

/// Bare name of the command the shell runs once the game has left.
///
/// [`COMMAND_LINE`] joins the two with `&&`, so the shell loads it only after
/// the game exited with status zero: its load is the guest's last witness,
/// and nothing in the run can cause it before the host closes the game's
/// window.
pub const THEN_COMMAND: &str = "true";

/// The line typed at the terminal's shell: the game holding its reference
/// scene, then [`THEN_COMMAND`] once it has left cleanly.
pub const COMMAND_LINE: &str = "wintersun --reference-scene && true\n";

/// Whether `bundle` is exactly `<store>/<name><suffix>`.
///
/// The store and suffix are the shared `lib/abi` spellings, handed in by the
/// caller, so the path is composed from its own definitions on both sides
/// rather than written out here.
#[must_use]
pub fn is_bundle(bundle: &str, store: &str, name: &str, suffix: &str) -> bool {
    bundle
        .strip_prefix(store)
        .and_then(|rest| rest.strip_prefix('/'))
        .and_then(|rest| rest.strip_suffix(suffix))
        .is_some_and(|bare| bare == name)
}

#[cfg(test)]
mod tests {
    use super::{is_bundle, COMMAND_LINE, GAME_APP_NAME, THEN_COMMAND};

    /// Only the named bundle in the named store matches: not a longer name,
    /// not the same name in another store, not a file inside the bundle.
    #[test]
    fn only_the_named_bundle_matches() {
        let store = "/System/Applications";
        assert!(is_bundle(
            "/System/Applications/wintersun.app",
            store,
            GAME_APP_NAME,
            ".app"
        ));
        for other in [
            "/System/Applications/wintersund.app",
            "/System/Commands/wintersun.app",
            "/System/Applications/wintersun.app/Run",
            "/System/Applicationswintersun.app",
            "wintersun.app",
        ] {
            assert!(!is_bundle(other, store, GAME_APP_NAME, ".app"), "{other}");
        }
    }

    /// The line runs the game first and the witness command only after it,
    /// and only on a clean exit.
    #[test]
    fn the_witness_command_runs_only_after_the_game_left_cleanly() {
        let (game, then) = COMMAND_LINE
            .trim_end_matches('\n')
            .split_once(" && ")
            .expect("the line joins two commands with &&");
        assert!(game.starts_with(GAME_APP_NAME));
        assert_eq!(then, THEN_COMMAND);
        assert!(COMMAND_LINE.ends_with('\n'), "the line is entered");
    }
}
