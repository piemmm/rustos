//! The parsed shape of an `unmount` command line.

use alloc::string::String;

use crate::error::UnmountError;

/// One thing the `unmount` tool can do.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Command {
    /// Detach the volume mounted under `name`. `force` selects the
    /// audited force-unmount that discards retained uncommitted data
    /// when a clean commit is impossible.
    Unmount {
        /// The volume's catalog name (`usb1`) or its mount-point path
        /// (`/Storage/usb1`), matched against the mount listing.
        name: String,
        /// `true` for `-f`/`--force`.
        force: bool,
    },
    /// Render the tool's own short help (`-?`/`--help`).
    Help,
}

/// Parse `args` (the tool's arguments, excluding the program name) into a
/// [`Command`].
///
/// The grammar is `unmount [-f] [--] NAME`, following the established
/// `umount` surface:
///
/// * `NAME` — the volume to detach, by catalog name or mount-point path.
/// * `-f` / `--force` — force-unmount: retract the volume even when its
///   uncommitted data cannot be committed, deliberately discarding the
///   retained set (an audited, capability-gated kernel decision).
/// * `-?` / `--help` — the tool's own short help (wins immediately).
/// * `--` — end option parsing; every later argument is an operand.
///
/// Short toggles cluster (`-f` alone today, so clustering is trivial but
/// consistent with the sibling tools).
///
/// # Errors
///
/// [`UnmountError::Usage`] for an unrecognised option or a number of
/// operands other than exactly one.
pub fn parse(args: &[&str]) -> Result<Command, UnmountError> {
    let mut force = false;
    let mut name: Option<String> = None;
    let mut options_done = false;

    for arg in args {
        if !options_done {
            match *arg {
                "--" => {
                    options_done = true;
                    continue;
                }
                "--help" => return Ok(Command::Help),
                "--force" => {
                    force = true;
                    continue;
                }
                _ => {}
            }
            if let Some(rest) = arg.strip_prefix('-').filter(|rest| !rest.is_empty()) {
                for letter in rest.chars() {
                    match letter {
                        '?' => return Ok(Command::Help),
                        'f' => force = true,
                        _ => return Err(UnmountError::Usage),
                    }
                }
                continue;
            }
        }
        if name.is_some() {
            return Err(UnmountError::Usage);
        }
        name = Some(String::from(*arg));
    }

    match name {
        Some(name) => Ok(Command::Unmount { name, force }),
        None => Err(UnmountError::Usage),
    }
}

#[cfg(test)]
mod tests {
    use super::{parse, Command};
    use crate::error::UnmountError;
    use alloc::string::String;

    fn unmount(name: &str, force: bool) -> Command {
        Command::Unmount {
            name: String::from(name),
            force,
        }
    }

    #[test]
    fn one_operand_is_a_plain_unmount() {
        assert_eq!(parse(&["usb1"]), Ok(unmount("usb1", false)));
        assert_eq!(
            parse(&["/Storage/usb1"]),
            Ok(unmount("/Storage/usb1", false))
        );
    }

    #[test]
    fn force_parses_short_and_long() {
        assert_eq!(parse(&["-f", "usb1"]), Ok(unmount("usb1", true)));
        assert_eq!(parse(&["--force", "usb1"]), Ok(unmount("usb1", true)));
        assert_eq!(parse(&["usb1", "-f"]), Ok(unmount("usb1", true)));
    }

    #[test]
    fn help_switches_win_immediately() {
        assert_eq!(parse(&["-?"]), Ok(Command::Help));
        assert_eq!(parse(&["--help"]), Ok(Command::Help));
        assert_eq!(parse(&["-f", "--help", "usb1"]), Ok(Command::Help));
    }

    #[test]
    fn double_dash_protects_a_dash_named_operand() {
        assert_eq!(parse(&["--", "-f"]), Ok(unmount("-f", false)));
        assert_eq!(
            parse(&["-f", "--", "--force"]),
            Ok(unmount("--force", true))
        );
    }

    #[test]
    fn wrong_operand_counts_are_usage() {
        assert_eq!(parse(&[]), Err(UnmountError::Usage));
        assert_eq!(parse(&["-f"]), Err(UnmountError::Usage));
        assert_eq!(parse(&["a", "b"]), Err(UnmountError::Usage));
    }

    #[test]
    fn unknown_options_are_usage() {
        assert_eq!(parse(&["-x", "usb1"]), Err(UnmountError::Usage));
        assert_eq!(parse(&["--frob", "usb1"]), Err(UnmountError::Usage));
        assert_eq!(parse(&["-fx", "usb1"]), Err(UnmountError::Usage));
    }
}
