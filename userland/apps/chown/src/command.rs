//! The parsed shape of a `chown` command line, including the owner spec.

use alloc::string::String;
use alloc::vec::Vec;

use crate::error::ChownError;

/// One thing the `chown` tool can do.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Command {
    /// Apply `owner` to each of `files`, in operand order.
    Change {
        /// Descend into directories and apply the owner to their contents
        /// (`-R`/`--recursive`).
        recursive: bool,
        /// The owner (user and/or group) to apply.
        owner: Owner,
        /// The files to change, in order. Always at least one.
        files: Vec<String>,
    },
    /// Print the usage banner (`-h`/`--help`).
    Help,
}

/// A parsed owner operand: an optional new user id and an optional new group
/// id.
///
/// `chown` accepts three forms, all using **decimal** ids (RustOS has no
/// name-to-id seam in this tool, so a name would be interface creep,
/// `AGENTS.md` §2.4):
///
/// * `OWNER` — change the owning user, leave the group ([`uid`](Owner::uid)
///   set, [`gid`](Owner::gid) [`None`]).
/// * `OWNER:GROUP` — change both.
/// * `:GROUP` — change only the group ([`uid`](Owner::uid) [`None`]).
///
/// At least one field is always [`Some`]: an empty spec, a bare `:`, and a
/// trailing-colon `OWNER:` are all rejected by [`parse_owner`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Owner {
    /// The new owning user id, or [`None`] to leave it unchanged.
    pub uid: Option<u32>,
    /// The new owning group id, or [`None`] to leave it unchanged.
    pub gid: Option<u32>,
}

/// Parse `args` (the tool's arguments, excluding the program name) into a
/// [`Command`].
///
/// The grammar is the familiar `chown [-R] [--] OWNER[:GROUP] file...`:
///
/// * `-R` / `--recursive` — descend into directories.
/// * `-h` / `--help` — print the usage banner (wins immediately).
/// * `--` — end option parsing; every later argument is an operand.
/// * any other `-…` — a [`ChownError::Usage`] error (fail closed; never a
///   silently ignored token).
/// * anything else — an operand.
///
/// The first operand is the owner spec; the rest are files.
///
/// # Errors
///
/// [`ChownError::Usage`] for any unrecognised option before `--`, or when
/// fewer than two operands (an owner spec and at least one file) are given.
/// [`ChownError::BadOwner`] when the owner operand is not a valid
/// `OWNER`/`OWNER:GROUP`/`:GROUP` spec.
pub fn parse(args: &[&str]) -> Result<Command, ChownError> {
    let mut recursive = false;
    let mut operands = Vec::new();
    let mut options_done = false;
    for &arg in args {
        if !options_done {
            if arg == "--" {
                options_done = true;
                continue;
            }
            if let Some(name) = arg.strip_prefix("--") {
                match name {
                    "recursive" => recursive = true,
                    "help" => return Ok(Command::Help),
                    _ => return Err(ChownError::Usage),
                }
                continue;
            }
            if let Some(letters) = arg.strip_prefix('-').filter(|rest| !rest.is_empty()) {
                for letter in letters.chars() {
                    match letter {
                        'R' => recursive = true,
                        'h' => return Ok(Command::Help),
                        _ => return Err(ChownError::Usage),
                    }
                }
                continue;
            }
        }
        operands.push(String::from(arg));
    }
    if operands.is_empty() {
        return Err(ChownError::Usage);
    }
    let owner_spec = operands.remove(0);
    if operands.is_empty() {
        return Err(ChownError::Usage);
    }
    let owner = parse_owner(&owner_spec).ok_or(ChownError::BadOwner)?;
    Ok(Command::Change {
        recursive,
        owner,
        files: operands,
    })
}

/// Parse an owner operand into an [`Owner`].
///
/// Accepts `OWNER`, `OWNER:GROUP`, and `:GROUP`, where each present field is a
/// decimal [`u32`]. Returns [`None`] for an empty spec, a bare `:`, a
/// trailing-colon `OWNER:` (the field would be empty), more than one colon, or
/// a non-decimal / overflowing id — leaving at least one field [`Some`] in
/// every successful parse.
#[must_use]
pub fn parse_owner(spec: &str) -> Option<Owner> {
    match spec.split_once(':') {
        None => {
            let uid = parse_id(spec)?;
            Some(Owner {
                uid: Some(uid),
                gid: None,
            })
        }
        Some((user, group)) => {
            let uid = if user.is_empty() {
                None
            } else {
                Some(parse_id(user)?)
            };
            let gid = if group.is_empty() {
                None
            } else {
                Some(parse_id(group)?)
            };
            if uid.is_none() && gid.is_none() {
                return None;
            }
            // `OWNER:` (a named user with an empty group) has no meaning
            // without a name database to resolve the user's primary group, so
            // it is rejected rather than guessed (`AGENTS.md` §2.1).
            if uid.is_some() && gid.is_none() {
                return None;
            }
            Some(Owner { uid, gid })
        }
    }
}

/// Parse one non-empty run of decimal digits into a [`u32`]. Returns [`None`]
/// for an empty string, a non-digit, or a value that overflows.
fn parse_id(text: &str) -> Option<u32> {
    if text.is_empty() {
        return None;
    }
    let mut value: u32 = 0;
    for c in text.chars() {
        let digit = c.to_digit(10)?;
        value = value.checked_mul(10)?.checked_add(digit)?;
    }
    Some(value)
}

#[cfg(test)]
mod tests {
    use super::{parse, parse_owner, Command, Owner};
    use crate::error::ChownError;
    use alloc::string::String;
    use alloc::vec::Vec;

    fn change(recursive: bool, owner: Owner, files: &[&str]) -> Command {
        Command::Change {
            recursive,
            owner,
            files: files.iter().map(|p| String::from(*p)).collect::<Vec<_>>(),
        }
    }

    fn owner(uid: Option<u32>, gid: Option<u32>) -> Owner {
        Owner { uid, gid }
    }

    // ----- command-line parsing -------------------------------------------

    #[test]
    fn an_owner_and_one_file_parses() {
        assert_eq!(
            parse(&["1000", "a.txt"]),
            Ok(change(false, owner(Some(1000), None), &["a.txt"]))
        );
    }

    #[test]
    fn an_owner_group_pair_parses() {
        assert_eq!(
            parse(&["1000:100", "f"]),
            Ok(change(false, owner(Some(1000), Some(100)), &["f"]))
        );
    }

    #[test]
    fn a_group_only_spec_parses() {
        assert_eq!(
            parse(&[":100", "f"]),
            Ok(change(false, owner(None, Some(100)), &["f"]))
        );
    }

    #[test]
    fn fewer_than_two_operands_is_usage() {
        assert_eq!(parse(&[]), Err(ChownError::Usage));
        assert_eq!(parse(&["1000"]), Err(ChownError::Usage));
        assert_eq!(parse(&["-R"]), Err(ChownError::Usage));
        assert_eq!(parse(&["-R", "1000"]), Err(ChownError::Usage));
    }

    #[test]
    fn help_flags_win() {
        assert_eq!(parse(&["-h"]), Ok(Command::Help));
        assert_eq!(parse(&["--help"]), Ok(Command::Help));
        assert_eq!(parse(&["-Rh", "1000", "f"]), Ok(Command::Help));
    }

    #[test]
    fn recursive_flag_sets_its_field() {
        assert_eq!(
            parse(&["-R", "1000:100", "d"]),
            Ok(change(true, owner(Some(1000), Some(100)), &["d"]))
        );
        assert_eq!(
            parse(&["--recursive", "1000:100", "d"]),
            Ok(change(true, owner(Some(1000), Some(100)), &["d"]))
        );
    }

    #[test]
    fn several_files_after_the_owner_are_all_collected() {
        assert_eq!(
            parse(&["0:0", "a", "b", "c"]),
            Ok(change(false, owner(Some(0), Some(0)), &["a", "b", "c"]))
        );
    }

    #[test]
    fn lowercase_r_is_not_recursive_and_is_usage() {
        // POSIX `chown` spells recursive `-R`; a bare `-r` is not an option.
        assert_eq!(parse(&["-r", "1000", "f"]), Err(ChownError::Usage));
    }

    #[test]
    fn unknown_option_is_usage() {
        assert_eq!(parse(&["-x", "1000", "f"]), Err(ChownError::Usage));
        assert_eq!(parse(&["--frob", "1000", "f"]), Err(ChownError::Usage));
        assert_eq!(parse(&["-Rx", "1000", "f"]), Err(ChownError::Usage));
    }

    #[test]
    fn double_dash_ends_options_so_a_dash_named_file_is_an_operand() {
        assert_eq!(
            parse(&["--", "1000", "-weird"]),
            Ok(change(false, owner(Some(1000), None), &["-weird"]))
        );
    }

    #[test]
    fn an_unparseable_owner_is_bad_owner() {
        assert_eq!(parse(&["", "f"]), Err(ChownError::BadOwner));
        assert_eq!(parse(&[":", "f"]), Err(ChownError::BadOwner));
        assert_eq!(parse(&["1000:", "f"]), Err(ChownError::BadOwner));
        assert_eq!(parse(&["alice", "f"]), Err(ChownError::BadOwner));
        assert_eq!(parse(&["1000:wheel", "f"]), Err(ChownError::BadOwner));
        assert_eq!(parse(&["1000:100:5", "f"]), Err(ChownError::BadOwner));
    }

    // ----- owner-spec parsing ---------------------------------------------

    #[test]
    fn owner_forms_parse_their_fields() {
        assert_eq!(parse_owner("0"), Some(owner(Some(0), None)));
        assert_eq!(parse_owner("4294967295"), Some(owner(Some(u32::MAX), None)));
        assert_eq!(parse_owner("1000:100"), Some(owner(Some(1000), Some(100))));
        assert_eq!(parse_owner(":100"), Some(owner(None, Some(100))));
    }

    #[test]
    fn owner_rejects_empty_colon_and_trailing_colon() {
        assert_eq!(parse_owner(""), None);
        assert_eq!(parse_owner(":"), None);
        assert_eq!(parse_owner("1000:"), None);
    }

    #[test]
    fn owner_rejects_non_decimal_and_overflow() {
        assert_eq!(parse_owner("alice"), None);
        assert_eq!(parse_owner("0x10"), None);
        assert_eq!(parse_owner("10:bob"), None);
        // u32::MAX + 1 overflows.
        assert_eq!(parse_owner("4294967296"), None);
        // More than one colon is not a valid spec.
        assert_eq!(parse_owner("1:2:3"), None);
    }
}
