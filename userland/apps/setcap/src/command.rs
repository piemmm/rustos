//! The parsed shape of a `setcap` command line, including the capability spec.

use alloc::string::String;
use alloc::vec::Vec;

use tairix_abi::CapabilityId;

use crate::error::SetcapError;

/// One thing the `setcap` tool can do.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Command {
    /// Apply `cap` to each of `files`, in operand order. `cap` is [`Some`]
    /// capability to install a gate, or [`None`] to clear any existing gate.
    Set {
        /// Descend into directories and apply the gate to their contents
        /// (`-R`/`--recursive`).
        recursive: bool,
        /// The capability gate to install, or [`None`] to clear the gate.
        cap: Option<CapabilityId>,
        /// The files to change, in order. Always at least one.
        files: Vec<String>,
    },
    /// Print the usage banner (`-h`/`--help`).
    Help,
}

/// Parse `args` (the tool's arguments, excluding the program name) into a
/// [`Command`].
///
/// The grammar is `setcap [-R] [--] CAP file...`:
///
/// * `-R` / `--recursive` — descend into directories.
/// * `-h` / `--help` — print the usage banner (wins immediately).
/// * `--` — end option parsing; every later argument is an operand.
/// * any other `-…` — a [`SetcapError::Usage`] error (fail closed; never a
///   silently ignored token). The bare `-` (clear the gate) is an operand,
///   not an option.
/// * anything else — an operand.
///
/// The first operand is the capability spec; the rest are files.
///
/// # Errors
///
/// [`SetcapError::Usage`] for any unrecognised option before `--`, or when
/// fewer than two operands (a capability spec and at least one file) are
/// given. [`SetcapError::BadCapability`] when the capability operand is
/// neither a known `CAP_*` name nor `-`.
pub fn parse(args: &[&str]) -> Result<Command, SetcapError> {
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
                    _ => return Err(SetcapError::Usage),
                }
                continue;
            }
            if let Some(letters) = arg.strip_prefix('-').filter(|rest| !rest.is_empty()) {
                for letter in letters.chars() {
                    match letter {
                        'R' => recursive = true,
                        'h' => return Ok(Command::Help),
                        _ => return Err(SetcapError::Usage),
                    }
                }
                continue;
            }
        }
        operands.push(String::from(arg));
    }
    if operands.is_empty() {
        return Err(SetcapError::Usage);
    }
    let cap_spec = operands.remove(0);
    if operands.is_empty() {
        return Err(SetcapError::Usage);
    }
    let cap = parse_capability(&cap_spec).ok_or(SetcapError::BadCapability)?;
    Ok(Command::Set {
        recursive,
        cap,
        files: operands,
    })
}

/// Parse a capability operand into the gate it requests.
///
/// Returns:
///
/// * `Some(Some(cap))` for a canonical `CAP_*` name (e.g. `CAP_AUDIT_READ`) —
///   install that gate.
/// * `Some(None)` for the literal `-` — clear the gate.
/// * `None` for anything else — not a valid capability spec.
///
/// The name match is exact (no guessing): an unknown,
/// mis-cased, or bare numeric value is rejected rather than coerced.
#[must_use]
pub fn parse_capability(spec: &str) -> Option<Option<CapabilityId>> {
    if spec == "-" {
        return Some(None);
    }
    CapabilityId::from_name(spec).map(Some)
}

#[cfg(test)]
mod tests {
    use super::{parse, parse_capability, Command};
    use crate::error::SetcapError;
    use alloc::string::String;
    use alloc::vec::Vec;
    use tairix_abi::CapabilityId;

    fn set(recursive: bool, cap: Option<CapabilityId>, files: &[&str]) -> Command {
        Command::Set {
            recursive,
            cap,
            files: files.iter().map(|p| String::from(*p)).collect::<Vec<_>>(),
        }
    }

    // ----- command-line parsing -------------------------------------------

    #[test]
    fn a_capability_and_one_file_parses() {
        assert_eq!(
            parse(&["CAP_AUDIT_READ", "/f"]),
            Ok(set(false, Some(CapabilityId::AUDIT_READ), &["/f"]))
        );
    }

    #[test]
    fn the_dash_clears_the_gate() {
        assert_eq!(parse(&["-", "/f"]), Ok(set(false, None, &["/f"])));
    }

    #[test]
    fn fewer_than_two_operands_is_usage() {
        assert_eq!(parse(&[]), Err(SetcapError::Usage));
        assert_eq!(parse(&["CAP_AUDIT_READ"]), Err(SetcapError::Usage));
        assert_eq!(parse(&["-R"]), Err(SetcapError::Usage));
        assert_eq!(parse(&["-R", "CAP_AUDIT_READ"]), Err(SetcapError::Usage));
    }

    #[test]
    fn help_flags_win() {
        assert_eq!(parse(&["-h"]), Ok(Command::Help));
        assert_eq!(parse(&["--help"]), Ok(Command::Help));
        assert_eq!(parse(&["-Rh", "CAP_NET_RAW", "/f"]), Ok(Command::Help));
    }

    #[test]
    fn recursive_flag_sets_its_field() {
        assert_eq!(
            parse(&["-R", "CAP_FS_MOUNT", "/d"]),
            Ok(set(true, Some(CapabilityId::FS_MOUNT), &["/d"]))
        );
        assert_eq!(
            parse(&["--recursive", "-", "/d"]),
            Ok(set(true, None, &["/d"]))
        );
    }

    #[test]
    fn several_files_after_the_capability_are_all_collected() {
        assert_eq!(
            parse(&["CAP_NET_RAW", "/a", "/b", "/c"]),
            Ok(set(false, Some(CapabilityId::NET_RAW), &["/a", "/b", "/c"]))
        );
    }

    #[test]
    fn lowercase_r_is_not_recursive_and_is_usage() {
        // `setcap` spells recursive `-R`; a bare `-r` is not an option.
        assert_eq!(parse(&["-r", "CAP_NET_RAW", "/f"]), Err(SetcapError::Usage));
    }

    #[test]
    fn unknown_option_is_usage() {
        assert_eq!(parse(&["-x", "CAP_NET_RAW", "/f"]), Err(SetcapError::Usage));
        assert_eq!(
            parse(&["--frob", "CAP_NET_RAW", "/f"]),
            Err(SetcapError::Usage)
        );
        assert_eq!(
            parse(&["-Rx", "CAP_NET_RAW", "/f"]),
            Err(SetcapError::Usage)
        );
    }

    #[test]
    fn double_dash_ends_options_so_a_dash_named_file_is_an_operand() {
        assert_eq!(
            parse(&["--", "CAP_NET_RAW", "-weird"]),
            Ok(set(false, Some(CapabilityId::NET_RAW), &["-weird"]))
        );
    }

    #[test]
    fn an_unparseable_capability_is_bad_capability() {
        assert_eq!(parse(&["", "/f"]), Err(SetcapError::BadCapability));
        assert_eq!(parse(&["CAP_NOPE", "/f"]), Err(SetcapError::BadCapability));
        assert_eq!(
            parse(&["cap_net_raw", "/f"]),
            Err(SetcapError::BadCapability)
        );
        assert_eq!(parse(&["2", "/f"]), Err(SetcapError::BadCapability));
    }

    // ----- capability-spec parsing ----------------------------------------

    #[test]
    fn capability_spec_forms_parse() {
        assert_eq!(
            parse_capability("CAP_FS_MOUNT"),
            Some(Some(CapabilityId::FS_MOUNT))
        );
        assert_eq!(
            parse_capability("CAP_SYSINFO_HW"),
            Some(Some(CapabilityId::SYSINFO_HW))
        );
        assert_eq!(parse_capability("-"), Some(None));
    }

    #[test]
    fn capability_spec_rejects_unknown_and_mis_cased_and_numeric() {
        assert_eq!(parse_capability(""), None);
        assert_eq!(parse_capability("CAP_NOPE"), None);
        assert_eq!(parse_capability("FS_MOUNT"), None);
        assert_eq!(parse_capability("cap_fs_mount"), None);
        assert_eq!(parse_capability("1"), None);
        assert_eq!(parse_capability("--"), None);
    }
}
