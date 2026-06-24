//! The parsed shape of a `useradd` command line, the account it describes,
//! and the login-name validator.

use alloc::string::String;
use alloc::vec::Vec;

use crate::error::UseraddError;

/// Maximum length of a login name, in bytes.
///
/// Matches the long-standing Unix `LOGIN_NAME_MAX`/`login.defs` ceiling. The
/// bound keeps a hostile command line from forcing an unbounded name into the
/// database; raising it is a reviewed change here, not a per-call workaround.
pub const MAX_NAME_LEN: usize = 32;

/// One thing the `useradd` tool can do.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Command {
    /// Create the described account.
    Create(NewUser),
    /// Print the usage banner (`-h`/`--help`).
    Help,
}

/// A parsed `useradd` account specification.
///
/// All ids are **decimal** (RustOS has no name-to-id seam in this tool, so a
/// name would be interface creep). The primary group is
/// always present — `useradd` requires `-g` rather than guessing a default
/// group.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NewUser {
    /// The login name, validated against [`validate_name`].
    pub name: String,
    /// The requested numeric user id (`-u`), or [`None`] to let the database
    /// allocate the next free one.
    pub uid: Option<u32>,
    /// The numeric primary group id (`-g`).
    pub primary_gid: u32,
    /// The numeric supplementary group ids (`-G`), in operand order. Empty
    /// when `-G` was not given.
    pub supplementary_gids: Vec<u32>,
    /// The optional account comment / full name (`-c`).
    pub comment: Option<String>,
    /// The optional home directory (`-d`).
    pub home: Option<String>,
}

/// Parse `args` (the tool's arguments, excluding the program name) into a
/// [`Command`].
///
/// The grammar is
/// `useradd [-u UID] -g GID [-G LIST] [-c COMMENT] [-d HOME] [--] NAME`:
///
/// * `-u` / `--uid` — numeric user id (auto-allocated when omitted).
/// * `-g` / `--gid` — numeric primary group id (**required**).
/// * `-G` / `--groups` — comma-separated numeric supplementary group ids.
/// * `-c` / `--comment` — account comment / full name.
/// * `-d` / `--home` — home directory.
/// * `-h` / `--help` — print the usage banner (wins immediately).
/// * `--` — end option parsing; every later argument is an operand.
/// * any other `-…` — a [`UseraddError::Usage`] error (fail closed).
///
/// Each value-taking option accepts its value attached (`-u0`, `--uid=0`) or
/// as the following argument (`-u 0`, `--uid 0`). Exactly one operand — the
/// login name — is required.
///
/// # Errors
///
/// [`UseraddError::Usage`] for an unrecognised option, a missing value, a
/// missing `-g`, or a number of operands other than one. [`UseraddError::BadId`]
/// when a `-u`/`-g`/`-G` value is not a decimal id (or a `-G` element is
/// empty). [`UseraddError::BadName`] when the operand is not a valid login
/// name.
pub fn parse(args: &[&str]) -> Result<Command, UseraddError> {
    let mut uid: Option<u32> = None;
    let mut primary_gid: Option<u32> = None;
    let mut supplementary_gids: Vec<u32> = Vec::new();
    let mut comment: Option<String> = None;
    let mut home: Option<String> = None;
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
                    "uid" => uid = Some(parse_id(value(inline, args, &mut index)?)?),
                    "gid" => primary_gid = Some(parse_id(value(inline, args, &mut index)?)?),
                    "groups" => {
                        supplementary_gids = parse_id_list(value(inline, args, &mut index)?)?;
                    }
                    "comment" => comment = Some(String::from(value(inline, args, &mut index)?)),
                    "home" => home = Some(String::from(value(inline, args, &mut index)?)),
                    _ => return Err(UseraddError::Usage),
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
                    'h' => return Ok(Command::Help),
                    'u' => uid = Some(parse_id(value(inline, args, &mut index)?)?),
                    'g' => primary_gid = Some(parse_id(value(inline, args, &mut index)?)?),
                    'G' => {
                        supplementary_gids = parse_id_list(value(inline, args, &mut index)?)?;
                    }
                    'c' => comment = Some(String::from(value(inline, args, &mut index)?)),
                    'd' => home = Some(String::from(value(inline, args, &mut index)?)),
                    _ => return Err(UseraddError::Usage),
                }
                continue;
            }
        }
        operands.push(String::from(arg));
    }

    if operands.len() != 1 {
        return Err(UseraddError::Usage);
    }
    let name = operands.remove(0);
    if !validate_name(&name) {
        return Err(UseraddError::BadName);
    }
    let primary_gid = primary_gid.ok_or(UseraddError::Usage)?;
    Ok(Command::Create(NewUser {
        name,
        uid,
        primary_gid,
        supplementary_gids,
        comment,
        home,
    }))
}

/// Resolve the value of a value-taking option: the attached `inline` text when
/// present, otherwise the following argument (which the caller's `index` is
/// advanced past).
///
/// # Errors
///
/// [`UseraddError::Usage`] when neither an attached value nor a following
/// argument is available.
fn value<'a>(
    inline: Option<&'a str>,
    args: &[&'a str],
    index: &mut usize,
) -> Result<&'a str, UseraddError> {
    if let Some(v) = inline {
        return Ok(v);
    }
    let v = args.get(*index).copied().ok_or(UseraddError::Usage)?;
    *index += 1;
    Ok(v)
}

/// Validate a login name against `[a-z_][a-z0-9_-]*` within
/// [`MAX_NAME_LEN`].
///
/// The first character must be a lowercase ASCII letter or `_`; the rest may
/// also be ASCII digits or `-`. An empty or over-long name, an upper-case
/// letter, a leading digit or `-`, or any other byte is rejected. This is the
/// portable Unix login-name shape; it deliberately admits no name that could
/// be confused for a numeric id or an option.
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

/// Parse a comma-separated list of decimal ids into a vector.
///
/// # Errors
///
/// [`UseraddError::BadId`] for an empty list, an empty element, or an element
/// that is not a decimal [`u32`].
fn parse_id_list(text: &str) -> Result<Vec<u32>, UseraddError> {
    if text.is_empty() {
        return Err(UseraddError::BadId);
    }
    let mut ids = Vec::new();
    for element in text.split(',') {
        ids.push(parse_id(element)?);
    }
    Ok(ids)
}

/// Parse one non-empty run of decimal digits into a [`u32`].
///
/// # Errors
///
/// [`UseraddError::BadId`] for an empty string, a non-digit, or an overflow.
fn parse_id(text: &str) -> Result<u32, UseraddError> {
    if text.is_empty() {
        return Err(UseraddError::BadId);
    }
    let mut value: u32 = 0;
    for c in text.chars() {
        let digit = c.to_digit(10).ok_or(UseraddError::BadId)?;
        value = value
            .checked_mul(10)
            .and_then(|v| v.checked_add(digit))
            .ok_or(UseraddError::BadId)?;
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::{parse, validate_name, Command, NewUser, MAX_NAME_LEN};
    use crate::error::UseraddError;
    use alloc::string::{String, ToString};
    use alloc::vec;
    use alloc::vec::Vec;

    fn create(
        name: &str,
        uid: Option<u32>,
        primary_gid: u32,
        supplementary_gids: &[u32],
        comment: Option<&str>,
        home: Option<&str>,
    ) -> Command {
        Command::Create(NewUser {
            name: name.to_string(),
            uid,
            primary_gid,
            supplementary_gids: supplementary_gids.to_vec(),
            comment: comment.map(String::from),
            home: home.map(String::from),
        })
    }

    // ----- command-line parsing -------------------------------------------

    #[test]
    fn a_name_and_a_group_parses() {
        assert_eq!(
            parse(&["-g", "100", "alice"]),
            Ok(create("alice", None, 100, &[], None, None))
        );
    }

    #[test]
    fn every_option_parses() {
        assert_eq!(
            parse(&[
                "-u",
                "1000",
                "-g",
                "100",
                "-G",
                "10,20,30",
                "-c",
                "Alice A",
                "-d",
                "/Users/alice",
                "alice",
            ]),
            Ok(create(
                "alice",
                Some(1000),
                100,
                &[10, 20, 30],
                Some("Alice A"),
                Some("/Users/alice"),
            ))
        );
    }

    #[test]
    fn long_options_parse_with_space_or_equals() {
        assert_eq!(
            parse(&["--uid", "7", "--gid=100", "--groups", "1,2", "bob"]),
            Ok(create("bob", Some(7), 100, &[1, 2], None, None))
        );
    }

    #[test]
    fn attached_short_values_parse() {
        assert_eq!(
            parse(&["-u0", "-g0", "root_svc"]),
            Ok(create("root_svc", Some(0), 0, &[], None, None))
        );
    }

    #[test]
    fn help_wins_immediately() {
        assert_eq!(parse(&["-h"]), Ok(Command::Help));
        assert_eq!(parse(&["--help"]), Ok(Command::Help));
        assert_eq!(parse(&["-g", "100", "--help", "alice"]), Ok(Command::Help));
    }

    #[test]
    fn missing_primary_group_is_usage() {
        assert_eq!(parse(&["alice"]), Err(UseraddError::Usage));
        assert_eq!(parse(&["-u", "1000", "alice"]), Err(UseraddError::Usage));
    }

    #[test]
    fn wrong_operand_count_is_usage() {
        assert_eq!(parse(&["-g", "100"]), Err(UseraddError::Usage));
        assert_eq!(
            parse(&["-g", "100", "alice", "bob"]),
            Err(UseraddError::Usage)
        );
    }

    #[test]
    fn unknown_option_is_usage() {
        assert_eq!(parse(&["-x", "-g", "100", "a"]), Err(UseraddError::Usage));
        assert_eq!(
            parse(&["--frob", "-g", "100", "a"]),
            Err(UseraddError::Usage)
        );
    }

    #[test]
    fn a_missing_option_value_is_usage() {
        assert_eq!(parse(&["-g", "100", "-u"]), Err(UseraddError::Usage));
        assert_eq!(parse(&["-g"]), Err(UseraddError::Usage));
    }

    #[test]
    fn double_dash_ends_options_so_a_dash_named_operand_is_taken() {
        // `-weird` after `--` is the (invalid) name operand, not an option.
        assert_eq!(
            parse(&["-g", "100", "--", "-weird"]),
            Err(UseraddError::BadName)
        );
    }

    #[test]
    fn a_non_decimal_id_is_bad_id() {
        assert_eq!(parse(&["-g", "wheel", "a"]), Err(UseraddError::BadId));
        assert_eq!(
            parse(&["-u", "0x10", "-g", "1", "a"]),
            Err(UseraddError::BadId)
        );
        assert_eq!(
            parse(&["-g", "1", "-G", "1,,2", "a"]),
            Err(UseraddError::BadId)
        );
        assert_eq!(parse(&["-g", "1", "-G", "", "a"]), Err(UseraddError::BadId));
        // u32::MAX + 1 overflows.
        assert_eq!(parse(&["-g", "4294967296", "a"]), Err(UseraddError::BadId));
    }

    #[test]
    fn an_invalid_name_is_bad_name() {
        assert_eq!(parse(&["-g", "1", "Alice"]), Err(UseraddError::BadName));
        assert_eq!(parse(&["-g", "1", "1abc"]), Err(UseraddError::BadName));
        assert_eq!(parse(&["-g", "1", "a$b"]), Err(UseraddError::BadName));
    }

    // ----- login-name validation -----------------------------------------

    #[test]
    fn valid_names_are_accepted() {
        assert!(validate_name("alice"));
        assert!(validate_name("_svc"));
        assert!(validate_name("user-1"));
        assert!(validate_name("a"));
        let max = "a".repeat(MAX_NAME_LEN);
        assert!(validate_name(&max));
    }

    #[test]
    fn invalid_names_are_rejected() {
        assert!(!validate_name(""));
        assert!(!validate_name("Alice")); // upper-case
        assert!(!validate_name("1abc")); // leading digit
        assert!(!validate_name("-abc")); // leading dash
        assert!(!validate_name("a b")); // space
        assert!(!validate_name("café")); // non-ASCII
        let too_long = "a".repeat(MAX_NAME_LEN + 1);
        assert!(!validate_name(&too_long));
    }

    #[test]
    fn the_empty_supplementary_default_is_an_empty_vec() {
        let parsed = parse(&["-g", "1", "alice"]).expect("valid");
        let Command::Create(user) = parsed else {
            panic!("expected create");
        };
        assert_eq!(user.supplementary_gids, Vec::<u32>::new());
    }

    #[test]
    fn a_single_supplementary_group_parses() {
        assert_eq!(
            parse(&["-g", "1", "-G", "42", "alice"]),
            Ok(create("alice", None, 1, &[42], None, None))
        );
        // And the vec form is what we expect.
        assert_eq!(
            parse(&["-g", "1", "-G", "42", "alice"]),
            Ok(create("alice", None, 1, &vec![42][..], None, None))
        );
    }
}
