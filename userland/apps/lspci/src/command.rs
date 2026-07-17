//! The `lspci` command shape and its parser.
//!
//! The option surface follows `pciutils` `lspci`: `-n` lists numeric ids,
//! `-nn` lists names *and* ids, `-v` adds each function's declared
//! resources, `-t` renders the bus topology as a tree, and `-d
//! [<vendor>]:[<device>]` / `-s <node>` filter the listing. `-?` and
//! `--help` answer with the bundle's own short help. `lspci` takes no
//! positional operands; one is a usage error, never silently ignored.
//!
//! TAIRiX's hardware model has no PCI bus/device/function address — a
//! discovered function is a hardware-tree node with a stable node id — so
//! the `-s` selector names that node id (the same deliberate divergence
//! `lsusb`'s bus/device numbers make; the Help document states it).

use alloc::format;
use alloc::string::{String, ToString};
use core::fmt;

/// What a parsed argument vector asks `lspci` to do.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Command {
    /// `-?` / `--help`: render the bundle's own short help and exit
    /// successfully. Wins over every other option, as in the GNU tools.
    Help,
    /// List the discovered PCI functions with the given options.
    List(Options),
}

/// How device identities are rendered.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum NameMode {
    /// Human-readable names, the numeric form only where the database has
    /// no entry (the default).
    #[default]
    Names,
    /// `-n`: numeric ids only.
    Numeric,
    /// `-nn`: names followed by the numeric ids in brackets.
    Both,
}

/// A `-d [<vendor>]:[<device>]` filter: an omitted half is a wildcard,
/// exactly as in `pciutils`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeviceFilter {
    /// Match only this vendor id, when present.
    pub vendor: Option<u16>,
    /// Match only this device id, when present.
    pub device: Option<u16>,
}

/// The parsed listing options.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Options {
    /// How identities are rendered (`-n` / `-nn`).
    pub names: NameMode,
    /// `-v`: append each function's declared resources.
    pub verbose: bool,
    /// `-t`: render the bus topology as a tree.
    pub tree: bool,
    /// `-d`: keep only functions matching the vendor/device filter.
    pub device: Option<DeviceFilter>,
    /// `-s`: keep only the function with this hardware-tree node id.
    pub slot: Option<u32>,
}

/// Why an argument vector did not parse.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ParseError {
    /// An option this tool does not implement.
    UnknownOption(String),
    /// A value-carrying option (`-d`, `-s`) with no value.
    MissingValue(&'static str),
    /// A `-d` value that is not `[<vendor>]:[<device>]` in hex.
    BadDeviceFilter(String),
    /// A `-s` value that is not a decimal hardware-tree node id.
    BadSlot(String),
    /// A positional operand; `lspci` takes none.
    UnexpectedOperand(String),
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownOption(opt) => write!(f, "unrecognized option '{opt}'"),
            Self::MissingValue(opt) => write!(f, "option '{opt}' requires an argument"),
            Self::BadDeviceFilter(value) => {
                write!(f, "-d expects [<vendor>]:[<device>] in hex, got '{value}'")
            }
            Self::BadSlot(value) => {
                write!(
                    f,
                    "-s expects a decimal hardware-tree node id, got '{value}'"
                )
            }
            Self::UnexpectedOperand(arg) => write!(f, "unexpected operand '{arg}'"),
        }
    }
}

/// Parse an argument vector (without the program name).
///
/// `--` ends option parsing (anything after it is an operand, and
/// therefore an error — `lspci` takes none). Short options cluster; a
/// second `n` in the same invocation upgrades `-n` to `-nn`, exactly as
/// `pciutils` treats `-nn`.
///
/// # Errors
///
/// A [`ParseError`] naming the offending argument; the caller reports it
/// with the usage banner and exits `2`.
pub fn parse(args: &[&str]) -> Result<Command, ParseError> {
    let mut options = Options::default();
    let mut index = 0;
    let mut options_ended = false;
    while index < args.len() {
        let arg = args[index];
        index += 1;
        if options_ended || arg == "-" || !arg.starts_with('-') {
            return Err(ParseError::UnexpectedOperand(arg.to_string()));
        }
        if arg == "--" {
            options_ended = true;
            continue;
        }
        if let Some(long) = arg.strip_prefix("--") {
            match long {
                "help" => return Ok(Command::Help),
                _ => return Err(ParseError::UnknownOption(arg.to_string())),
            }
        }
        // A short-option cluster; `-d` and `-s` consume the rest of the
        // cluster (or the next argument) as their value.
        let mut cluster = arg[1..].chars();
        while let Some(flag) = cluster.next() {
            match flag {
                '?' => return Ok(Command::Help),
                'n' => {
                    options.names = match options.names {
                        NameMode::Names => NameMode::Numeric,
                        NameMode::Numeric | NameMode::Both => NameMode::Both,
                    };
                }
                'v' => options.verbose = true,
                't' => options.tree = true,
                'd' => {
                    let value = take_cluster_value(cluster.as_str(), args, &mut index, "-d")?;
                    options.device = Some(parse_device_filter(value)?);
                    break;
                }
                's' => {
                    let value = take_cluster_value(cluster.as_str(), args, &mut index, "-s")?;
                    options.slot = Some(
                        value
                            .parse::<u32>()
                            .map_err(|_| ParseError::BadSlot(value.to_string()))?,
                    );
                    break;
                }
                other => return Err(ParseError::UnknownOption(format!("-{other}"))),
            }
        }
    }
    Ok(Command::List(options))
}

/// The value of a `-x value` / `-xvalue` short option.
fn take_cluster_value<'a>(
    rest: &'a str,
    args: &[&'a str],
    index: &mut usize,
    option: &'static str,
) -> Result<&'a str, ParseError> {
    if !rest.is_empty() {
        return Ok(rest);
    }
    let Some(value) = args.get(*index) else {
        return Err(ParseError::MissingValue(option));
    };
    *index += 1;
    Ok(value)
}

/// Parse a `-d` value: `[<vendor>]:[<device>]`, each half up to four hex
/// digits, an empty half a wildcard. Anything else fails closed.
fn parse_device_filter(value: &str) -> Result<DeviceFilter, ParseError> {
    let bad = || ParseError::BadDeviceFilter(value.to_string());
    let (vendor, device) = value.split_once(':').ok_or_else(bad)?;
    let half = |text: &str| -> Result<Option<u16>, ParseError> {
        if text.is_empty() {
            return Ok(None);
        }
        if text.len() > 4 || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(bad());
        }
        u16::from_str_radix(text, 16).map(Some).map_err(|_| bad())
    };
    Ok(DeviceFilter {
        vendor: half(vendor)?,
        device: half(device)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(args: &[&str]) -> Options {
        match parse(args).expect("parses") {
            Command::List(options) => options,
            Command::Help => panic!("expected a listing"),
        }
    }

    #[test]
    fn defaults_render_names_only() {
        let options = list(&[]);
        assert_eq!(options, Options::default());
        assert_eq!(options.names, NameMode::Names);
    }

    #[test]
    fn help_switches_win() {
        assert_eq!(parse(&["--help"]), Ok(Command::Help));
        assert_eq!(parse(&["-?"]), Ok(Command::Help));
        assert_eq!(parse(&["-v", "-?"]), Ok(Command::Help));
    }

    #[test]
    fn n_and_nn_select_the_pciutils_modes() {
        assert_eq!(list(&["-n"]).names, NameMode::Numeric);
        assert_eq!(list(&["-nn"]).names, NameMode::Both);
        // Split across arguments, as `pciutils` also accepts.
        assert_eq!(list(&["-n", "-n"]).names, NameMode::Both);
    }

    #[test]
    fn flags_cluster_and_values_attach() {
        let options = list(&["-vt", "-d8086:10d3", "-s", "7"]);
        assert!(options.verbose);
        assert!(options.tree);
        assert_eq!(
            options.device,
            Some(DeviceFilter {
                vendor: Some(0x8086),
                device: Some(0x10d3),
            })
        );
        assert_eq!(options.slot, Some(7));
    }

    #[test]
    fn device_filter_halves_are_wildcards() {
        assert_eq!(
            list(&["-d", "8086:"]).device,
            Some(DeviceFilter {
                vendor: Some(0x8086),
                device: None,
            })
        );
        assert_eq!(
            list(&["-d", ":10d3"]).device,
            Some(DeviceFilter {
                vendor: None,
                device: Some(0x10d3),
            })
        );
    }

    #[test]
    fn malformed_values_fail_closed() {
        assert!(matches!(
            parse(&["-d", "8086"]),
            Err(ParseError::BadDeviceFilter(_))
        ));
        assert!(matches!(
            parse(&["-d", "80867:1"]),
            Err(ParseError::BadDeviceFilter(_))
        ));
        assert!(matches!(
            parse(&["-d", "80g6:1"]),
            Err(ParseError::BadDeviceFilter(_))
        ));
        assert!(matches!(parse(&["-s", "0x7"]), Err(ParseError::BadSlot(_))));
        assert!(matches!(
            parse(&["-s"]),
            Err(ParseError::MissingValue("-s"))
        ));
        assert!(matches!(parse(&["-z"]), Err(ParseError::UnknownOption(_))));
        assert!(matches!(
            parse(&["--verbose"]),
            Err(ParseError::UnknownOption(_))
        ));
    }

    #[test]
    fn operands_are_refused() {
        assert!(matches!(
            parse(&["bus0"]),
            Err(ParseError::UnexpectedOperand(_))
        ));
        assert!(matches!(
            parse(&["--", "-v"]),
            Err(ParseError::UnexpectedOperand(_))
        ));
    }
}
