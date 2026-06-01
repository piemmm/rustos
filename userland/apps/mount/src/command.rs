//! The parsed shape of a `mount` command line.

use alloc::string::String;
use alloc::vec::Vec;

use rustos_abi::driver::filesystem::MountFlags;

use crate::error::MountError;

/// One thing the `mount` tool can do.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Command {
    /// List the current mount table (no operands). Issues the ungated
    /// `sysinfo-v1` `MOUNT_LIST` query (`AGENTS.md` §16.6).
    List,
    /// Attach the described filesystem (two operands).
    Mount(MountRequest),
    /// Print the usage banner (`-h`/`--help`).
    Help,
}

/// A parsed `mount` attach request.
///
/// The privileged *act* of mounting is the kernel's decision, gated on
/// `CAP_FS_MOUNT` (`AGENTS.md` §5.2); this struct only describes what the
/// user asked for. The driver `fstype` is optional — a kernel that probes
/// the superblock can identify it — and `flags` is the parsed mount policy.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MountRequest {
    /// The backing source (a `/Storage` volume or device identifier).
    pub source: String,
    /// The mount-point path.
    pub target: String,
    /// The driver filesystem type (`-t`), or [`None`] to let the kernel
    /// probe.
    pub fstype: Option<String>,
    /// The parsed mount-policy flags (`-r` and `-o`).
    pub flags: MountFlags,
}

/// Parse `args` (the tool's arguments, excluding the program name) into a
/// [`Command`].
///
/// The grammar is `mount [-r] [-t TYPE] [-o OPTIONS] [--] [SOURCE TARGET]`:
///
/// * no operands — list the current mount table.
/// * `SOURCE TARGET` — attach `SOURCE` at `TARGET`.
/// * `-r` / `--read-only` — mount read-only (shorthand for `-o ro`).
/// * `-t` / `--types` TYPE — the driver filesystem type.
/// * `-o` / `--options` LIST — a comma-separated subset of
///   `ro,rw,nosuid,nodev,noexec`.
/// * `-h` / `--help` — print the usage banner (wins immediately).
/// * `--` — end option parsing; every later argument is an operand.
///
/// `-r` may cluster with other toggles (`-ro …` is `-r -o …` only at the end
/// of a cluster, since `-o` takes a value); `-t`/`-o` accept their value
/// attached (`-text4`, `--types=ext4`) or as the following argument.
///
/// # Errors
///
/// [`MountError::Usage`] for an unrecognised option, a missing option value,
/// or a number of operands other than zero or two (mount options given
/// without operands are also a usage error — there is nothing to mount).
/// [`MountError::BadOption`] for an unknown or empty `-o`/`-t` value.
pub fn parse(args: &[&str]) -> Result<Command, MountError> {
    let mut flags = MountFlags::default();
    let mut fstype: Option<String> = None;
    let mut operands: Vec<String> = Vec::new();
    let mut options_done = false;
    let mut saw_option = false;

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
                    "read-only" => {
                        if inline.is_some() {
                            return Err(MountError::Usage);
                        }
                        flags = flags.union(MountFlags::READ_ONLY);
                    }
                    "types" => fstype = Some(String::from(value(inline, args, &mut index)?)),
                    "options" => {
                        flags = flags.union(parse_options(value(inline, args, &mut index)?)?);
                    }
                    _ => return Err(MountError::Usage),
                }
                saw_option = true;
                continue;
            }
            if let Some(rest) = arg.strip_prefix('-').filter(|rest| !rest.is_empty()) {
                let mut chars = rest.chars();
                while let Some(letter) = chars.next() {
                    match letter {
                        'h' => return Ok(Command::Help),
                        'r' => flags = flags.union(MountFlags::READ_ONLY),
                        't' => {
                            let attached = chars.as_str();
                            let inline = (!attached.is_empty()).then_some(attached);
                            fstype = Some(String::from(value(inline, args, &mut index)?));
                            break;
                        }
                        'o' => {
                            let attached = chars.as_str();
                            let inline = (!attached.is_empty()).then_some(attached);
                            flags = flags.union(parse_options(value(inline, args, &mut index)?)?);
                            break;
                        }
                        _ => return Err(MountError::Usage),
                    }
                }
                saw_option = true;
                continue;
            }
        }
        operands.push(String::from(arg));
    }

    match operands.len() {
        0 if saw_option => Err(MountError::Usage),
        0 => Ok(Command::List),
        2 => {
            let mut operands = operands.into_iter();
            let source = operands.next().unwrap_or_default();
            let target = operands.next().unwrap_or_default();
            Ok(Command::Mount(MountRequest {
                source,
                target,
                fstype,
                flags,
            }))
        }
        _ => Err(MountError::Usage),
    }
}

/// Resolve the value of a value-taking option: the attached `inline` text
/// when present, otherwise the following argument (which the caller's `index`
/// is advanced past).
///
/// # Errors
///
/// [`MountError::Usage`] when neither an attached value nor a following
/// argument is available.
fn value<'a>(
    inline: Option<&'a str>,
    args: &[&'a str],
    index: &mut usize,
) -> Result<&'a str, MountError> {
    if let Some(v) = inline {
        return Ok(v);
    }
    let v = args.get(*index).copied().ok_or(MountError::Usage)?;
    *index += 1;
    Ok(v)
}

/// Parse a comma-separated `-o` option list into the [`MountFlags`] it sets.
///
/// Accepts the policy names RustOS recognises: `ro`/`rw` (read-only vs the
/// read-write default) and the `nosuid`/`nodev`/`noexec` restrictions
/// (`AGENTS.md` §5.3). `rw` clears nothing on its own — it is the default —
/// but is accepted so a user can write it explicitly.
///
/// # Errors
///
/// [`MountError::BadOption`] for an empty list, an empty element, or an
/// unrecognised option name.
fn parse_options(text: &str) -> Result<MountFlags, MountError> {
    if text.is_empty() {
        return Err(MountError::BadOption);
    }
    let mut flags = MountFlags::default();
    for element in text.split(',') {
        let flag = match element {
            "rw" => MountFlags::default(),
            "ro" => MountFlags::READ_ONLY,
            "nosuid" => MountFlags::NOSUID,
            "nodev" => MountFlags::NODEV,
            "noexec" => MountFlags::NOEXEC,
            _ => return Err(MountError::BadOption),
        };
        flags = flags.union(flag);
    }
    Ok(flags)
}

#[cfg(test)]
mod tests {
    use super::{parse, Command, MountRequest};
    use crate::error::MountError;
    use alloc::string::String;
    use rustos_abi::driver::filesystem::MountFlags;

    fn mount(source: &str, target: &str, fstype: Option<&str>, flags: MountFlags) -> Command {
        Command::Mount(MountRequest {
            source: String::from(source),
            target: String::from(target),
            fstype: fstype.map(String::from),
            flags,
        })
    }

    #[test]
    fn no_arguments_lists_the_mount_table() {
        assert_eq!(parse(&[]), Ok(Command::List));
        assert_eq!(parse(&["--"]), Ok(Command::List));
    }

    #[test]
    fn two_operands_describe_a_mount() {
        assert_eq!(
            parse(&["/Storage/data", "/Storage/data"]),
            Ok(mount(
                "/Storage/data",
                "/Storage/data",
                None,
                MountFlags::default()
            ))
        );
    }

    #[test]
    fn every_option_parses() {
        assert_eq!(
            parse(&[
                "-r",
                "-t",
                "rustfs",
                "-o",
                "nosuid,nodev",
                "vol",
                "/Storage/vol",
            ]),
            Ok(mount(
                "vol",
                "/Storage/vol",
                Some("rustfs"),
                MountFlags::READ_ONLY
                    .union(MountFlags::NOSUID)
                    .union(MountFlags::NODEV),
            ))
        );
    }

    #[test]
    fn long_options_with_space_or_equals() {
        assert_eq!(
            parse(&["--types=ext4", "--options", "ro", "dev", "/Storage/d"]),
            Ok(mount(
                "dev",
                "/Storage/d",
                Some("ext4"),
                MountFlags::READ_ONLY
            ))
        );
    }

    #[test]
    fn attached_short_values_parse() {
        assert_eq!(
            parse(&["-trustfs", "-onodev", "v", "/Storage/v"]),
            Ok(mount("v", "/Storage/v", Some("rustfs"), MountFlags::NODEV))
        );
    }

    #[test]
    fn read_only_shorthand_sets_the_flag() {
        let Command::Mount(req) = parse(&["-r", "v", "/Storage/v"]).expect("valid") else {
            panic!("expected mount");
        };
        assert!(req.flags.contains(MountFlags::READ_ONLY));
    }

    #[test]
    fn rw_option_is_the_read_write_default() {
        assert_eq!(
            parse(&["-o", "rw", "v", "/Storage/v"]),
            Ok(mount("v", "/Storage/v", None, MountFlags::default()))
        );
    }

    #[test]
    fn help_wins_immediately() {
        assert_eq!(parse(&["-h"]), Ok(Command::Help));
        assert_eq!(parse(&["--help"]), Ok(Command::Help));
        assert_eq!(parse(&["-r", "--help", "a", "b"]), Ok(Command::Help));
    }

    #[test]
    fn options_without_operands_is_usage() {
        // There is nothing to mount, and options never apply to a listing.
        assert_eq!(parse(&["-r"]), Err(MountError::Usage));
        assert_eq!(parse(&["-t", "ext4"]), Err(MountError::Usage));
    }

    #[test]
    fn wrong_operand_count_is_usage() {
        assert_eq!(parse(&["only-one"]), Err(MountError::Usage));
        assert_eq!(parse(&["a", "b", "c"]), Err(MountError::Usage));
    }

    #[test]
    fn unknown_option_is_usage() {
        assert_eq!(parse(&["-x", "a", "b"]), Err(MountError::Usage));
        assert_eq!(parse(&["--frob", "a", "b"]), Err(MountError::Usage));
    }

    #[test]
    fn missing_option_value_is_usage() {
        assert_eq!(parse(&["-t"]), Err(MountError::Usage));
        assert_eq!(parse(&["a", "b", "-o"]), Err(MountError::Usage));
    }

    #[test]
    fn bad_option_value_is_bad_option() {
        assert_eq!(
            parse(&["-o", "weird", "a", "b"]),
            Err(MountError::BadOption)
        );
        assert_eq!(parse(&["-o", "", "a", "b"]), Err(MountError::BadOption));
        assert_eq!(
            parse(&["-o", "ro,,nodev", "a", "b"]),
            Err(MountError::BadOption)
        );
    }

    #[test]
    fn double_dash_protects_a_dash_named_operand() {
        assert_eq!(
            parse(&["--", "-weird", "/Storage/x"]),
            Ok(mount("-weird", "/Storage/x", None, MountFlags::default()))
        );
    }
}
