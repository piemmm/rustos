//! The parsed shape of a `groupadd` command line, the group it describes,
//! and the group-name validator.

use alloc::string::String;
use alloc::vec::Vec;

use crate::error::GroupaddError;

/// Maximum length of a group name, in bytes.
///
/// Matches the long-standing Unix `login.defs` ceiling shared with login
/// names. The bound keeps a hostile command line from forcing an unbounded
/// name into the database; raising it is a reviewed change here, not a
/// per-call workaround.
pub const MAX_NAME_LEN: usize = 32;

/// One thing the `groupadd` tool can do.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Command {
    /// Create the described group.
    Create(NewGroup),
    /// Print the usage banner (`-h`/`--help`).
    Help,
}

/// A parsed `groupadd` group specification.
///
/// The gid is **decimal** (RustOS has no name-to-id seam in this tool, so a
/// name would be interface creep). A missing `-g` is left to
/// the database to allocate rather than guessed here.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NewGroup {
    /// The group name, validated against [`validate_name`].
    pub name: String,
    /// The requested numeric group id (`-g`), or [`None`] to let the database
    /// allocate the next free one.
    pub gid: Option<u32>,
}

/// Parse `args` (the tool's arguments, excluding the program name) into a
/// [`Command`].
///
/// The grammar is `groupadd [-g GID] [--] NAME`:
///
/// * `-g` / `--gid` — numeric group id (auto-allocated when omitted).
/// * `-h` / `-?` / `--help` — show the command's own short help (wins
///   immediately).
/// * `--` — end option parsing; every later argument is an operand.
/// * any other `-…` — a [`GroupaddError::Usage`] error (fail closed).
///
/// `-g` accepts its value attached (`-g0`, `--gid=0`) or as the following
/// argument (`-g 0`). Exactly one operand — the group name — is required.
///
/// # Errors
///
/// [`GroupaddError::Usage`] for an unrecognised option, a missing value, or a
/// number of operands other than one. [`GroupaddError::BadId`] when the `-g`
/// value is not a decimal id. [`GroupaddError::BadName`] when the operand is
/// not a valid group name.
pub fn parse(args: &[&str]) -> Result<Command, GroupaddError> {
    let mut gid: Option<u32> = None;
    let mut operands: Vec<String> = Vec::new();
    let mut options_done = false;

    let mut index = 0;
    while index < args.len() {
        let arg = args[index];
        index += 1;
        if !options_done {
            if arg == "--" {
                options_done = true;
                continue;
            }
            if let Some(long) = arg.strip_prefix("--") {
                let (name, inline) = match long.split_once('=') {
                    Some((n, v)) => (n, Some(v)),
                    None => (long, None),
                };
                match name {
                    "help" => return Ok(Command::Help),
                    "gid" => gid = Some(parse_id(value(inline, args, &mut index)?)?),
                    _ => return Err(GroupaddError::Usage),
                }
                continue;
            }
            if let Some(rest) = arg.strip_prefix('-').filter(|rest| !rest.is_empty()) {
                let mut chars = rest.chars();
                let letter = chars.next().unwrap_or('-');
                let attached = chars.as_str();
                let inline = if attached.is_empty() {
                    None
                } else {
                    Some(attached)
                };
                match letter {
                    'h' | '?' => return Ok(Command::Help),
                    'g' => gid = Some(parse_id(value(inline, args, &mut index)?)?),
                    _ => return Err(GroupaddError::Usage),
                }
                continue;
            }
        }
        operands.push(String::from(arg));
    }

    if operands.len() != 1 {
        return Err(GroupaddError::Usage);
    }
    let name = operands.remove(0);
    if !validate_name(&name) {
        return Err(GroupaddError::BadName);
    }
    Ok(Command::Create(NewGroup { name, gid }))
}

/// Resolve the value of a value-taking option: the attached `inline` text when
/// present, otherwise the following argument (which the caller's `index` is
/// advanced past).
///
/// # Errors
///
/// [`GroupaddError::Usage`] when neither an attached value nor a following
/// argument is available.
fn value<'a>(
    inline: Option<&'a str>,
    args: &[&'a str],
    index: &mut usize,
) -> Result<&'a str, GroupaddError> {
    if let Some(v) = inline {
        return Ok(v);
    }
    let v = args.get(*index).copied().ok_or(GroupaddError::Usage)?;
    *index += 1;
    Ok(v)
}

/// Validate a group name against `[a-z_][a-z0-9_-]*` within [`MAX_NAME_LEN`].
///
/// The first character must be a lowercase ASCII letter or `_`; the rest may
/// also be ASCII digits or `-`. An empty or over-long name, an upper-case
/// letter, a leading digit or `-`, or any other byte is rejected. This is the
/// portable Unix name shape, shared with login names; it deliberately admits
/// no name that could be confused for a numeric id or an option.
#[must_use]
pub fn validate_name(name: &str) -> bool {
    if name.is_empty() || name.len() > MAX_NAME_LEN {
        return false;
    }
    let mut chars = name.chars();
    let first = chars.next().unwrap_or('\0');
    if !(first.is_ascii_lowercase() || first == '_') {
        return false;
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

/// Parse one non-empty run of decimal digits into a [`u32`].
///
/// # Errors
///
/// [`GroupaddError::BadId`] for an empty string, a non-digit, or an overflow.
fn parse_id(text: &str) -> Result<u32, GroupaddError> {
    if text.is_empty() {
        return Err(GroupaddError::BadId);
    }
    let mut value: u32 = 0;
    for c in text.chars() {
        let digit = c.to_digit(10).ok_or(GroupaddError::BadId)?;
        value = value
            .checked_mul(10)
            .and_then(|v| v.checked_add(digit))
            .ok_or(GroupaddError::BadId)?;
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::{parse, validate_name, Command, NewGroup, MAX_NAME_LEN};
    use crate::error::GroupaddError;
    use alloc::string::ToString;

    fn create(name: &str, gid: Option<u32>) -> Command {
        Command::Create(NewGroup {
            name: name.to_string(),
            gid,
        })
    }

    // ----- command-line parsing -------------------------------------------

    #[test]
    fn a_bare_name_parses_with_no_gid() {
        assert_eq!(parse(&["staff"]), Ok(create("staff", None)));
    }

    #[test]
    fn a_name_and_a_gid_parses() {
        assert_eq!(
            parse(&["-g", "100", "staff"]),
            Ok(create("staff", Some(100)))
        );
    }

    #[test]
    fn long_option_parses_with_space_or_equals() {
        assert_eq!(
            parse(&["--gid", "7", "wheel"]),
            Ok(create("wheel", Some(7)))
        );
        assert_eq!(parse(&["--gid=7", "wheel"]), Ok(create("wheel", Some(7))));
    }

    #[test]
    fn attached_short_value_parses() {
        assert_eq!(parse(&["-g0", "root_svc"]), Ok(create("root_svc", Some(0))));
    }

    #[test]
    fn question_mark_is_short_help() {
        assert_eq!(parse(&["-?"]), Ok(Command::Help));
    }

    #[test]
    fn help_wins_immediately() {
        assert_eq!(parse(&["-h"]), Ok(Command::Help));
        assert_eq!(parse(&["--help"]), Ok(Command::Help));
        assert_eq!(parse(&["-g", "100", "--help", "staff"]), Ok(Command::Help));
    }

    #[test]
    fn wrong_operand_count_is_usage() {
        assert_eq!(parse(&[]), Err(GroupaddError::Usage));
        assert_eq!(parse(&["-g", "100"]), Err(GroupaddError::Usage));
        assert_eq!(parse(&["staff", "wheel"]), Err(GroupaddError::Usage));
    }

    #[test]
    fn unknown_option_is_usage() {
        assert_eq!(parse(&["-x", "staff"]), Err(GroupaddError::Usage));
        assert_eq!(parse(&["--frob", "staff"]), Err(GroupaddError::Usage));
        // A `-u`-style option borrowed from `useradd` is not part of the
        // `groupadd` grammar.
        assert_eq!(parse(&["-u", "1", "staff"]), Err(GroupaddError::Usage));
    }

    #[test]
    fn a_missing_option_value_is_usage() {
        assert_eq!(parse(&["staff", "-g"]), Err(GroupaddError::Usage));
        assert_eq!(parse(&["-g"]), Err(GroupaddError::Usage));
    }

    #[test]
    fn double_dash_ends_options_so_a_dash_named_operand_is_taken() {
        // `-weird` after `--` is the (invalid) name operand, not an option.
        assert_eq!(parse(&["--", "-weird"]), Err(GroupaddError::BadName));
    }

    #[test]
    fn a_non_decimal_id_is_bad_id() {
        assert_eq!(parse(&["-g", "wheel", "staff"]), Err(GroupaddError::BadId));
        assert_eq!(parse(&["-g", "0x10", "staff"]), Err(GroupaddError::BadId));
        assert_eq!(parse(&["-g", "", "staff"]), Err(GroupaddError::BadId));
        // u32::MAX + 1 overflows.
        assert_eq!(
            parse(&["-g", "4294967296", "staff"]),
            Err(GroupaddError::BadId)
        );
    }

    #[test]
    fn an_invalid_name_is_bad_name() {
        assert_eq!(parse(&["Staff"]), Err(GroupaddError::BadName));
        assert_eq!(parse(&["1abc"]), Err(GroupaddError::BadName));
        assert_eq!(parse(&["a$b"]), Err(GroupaddError::BadName));
    }

    /// Every locale's `OPTIONS` section documents exactly the switches this
    /// parser accepts (`plans/APPS.md` §3.1): the flag tokens are
    /// language-neutral, so each translated document must carry the same
    /// keys as the canonical one. The documents are read from the bundle's
    /// own on-disk `Help/` tree — the single source the image builder plants
    /// — never a copy embedded in this crate.
    #[test]
    fn help_documents_the_parser_switches() {
        extern crate std;
        use alloc::format;
        use std::fs;

        let help_root = format!("{}/Help", env!("CARGO_MANIFEST_DIR"));
        let locales = ["en-US", "fr-FR", "de-DE", "es-ES", "uk-UA", "it-IT"];
        for locale in locales {
            let path = format!("{help_root}/{locale}/groupadd.md");
            let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
            for switch in ["`-g, --gid GID`", "`-h, -?, --help`"] {
                assert!(
                    text.contains(switch),
                    "{locale}/groupadd.md must document {switch}"
                );
            }
        }
    }

    // ----- group-name validation -----------------------------------------

    #[test]
    fn valid_names_are_accepted() {
        assert!(validate_name("staff"));
        assert!(validate_name("_svc"));
        assert!(validate_name("group-1"));
        assert!(validate_name("a"));
        let max = "a".repeat(MAX_NAME_LEN);
        assert!(validate_name(&max));
    }

    #[test]
    fn invalid_names_are_rejected() {
        assert!(!validate_name(""));
        assert!(!validate_name("Staff")); // upper-case
        assert!(!validate_name("1abc")); // leading digit
        assert!(!validate_name("-abc")); // leading dash
        assert!(!validate_name("a b")); // space
        assert!(!validate_name("café")); // non-ASCII
        let too_long = "a".repeat(MAX_NAME_LEN + 1);
        assert!(!validate_name(&too_long));
    }
}
