//! Parse an `mdadm` command line into a [`Command`].
//!
//! `mdadm` is a mode tool: exactly one mode flag (`--create`, `--detail`,
//! `--examine`, `--add`, `--remove`, `--stop`) selects what to do, and the
//! operands and value options are interpreted in that mode's grammar. The
//! spelling tracks Linux `mdadm` — the same short and long flags, the same
//! `--long=value` / `-x value` grammar, and `--` for end-of-options — so a
//! user who knows `mdadm` finds this familiar. Where TAIRiX genuinely differs
//! (no `/dev`: a device is `node:<id>` and an array is a hexadecimal identity),
//! the difference is in the *operands*, not the flags.
//!
//! Parsing is pure and fails closed: an unknown flag, a missing value, a level
//! the composer does not offer, a value option in a mode that does not use it,
//! or the wrong operand count is a typed [`ParseError`], never a guess.

use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::fmt;

use tairix_abi::raid::RaidLevel;

/// A fully parsed `mdadm` invocation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Command {
    /// Show the command's own help and exit (`-h`, `-?`, `--help`).
    Help,
    /// Print the tool version and exit (`-V`, `--version`).
    Version,
    /// Report per-array detail. `array` is the identity operand, or [`None`]
    /// for every array (`-D`, `--detail [<array>]`).
    Detail {
        /// The array identity operand, or [`None`] for all arrays.
        array: Option<String>,
    },
    /// List every device the composer holds (`-E`, `--examine`).
    Examine,
    /// Create an array over the named devices (`-C`, `--create`).
    Create(CreateArgs),
    /// Admit a candidate device into an array's absent slot (`-a`, `--add`).
    Add {
        /// The array to admit the device into.
        array: String,
        /// The device to admit (`node:<id>`).
        device: String,
    },
    /// Retire a member device from an array (`-r`, `--remove`).
    Remove {
        /// The array to retire the member from.
        array: String,
        /// The device to retire (`node:<id>`).
        device: String,
    },
    /// Stop a live array (`-S`, `--stop`).
    Stop {
        /// The array to stop.
        array: String,
    },
}

/// The parts of a `--create` request that survive parsing: the level, the
/// requested slot count, the optional stripe unit, and the device operands
/// (each `node:<id>`, resolved later).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CreateArgs {
    /// The level to compose.
    pub level: RaidLevel,
    /// The number of member slots requested (`--raid-devices`).
    pub raid_devices: u16,
    /// The stripe unit in logical blocks, or [`None`] to let the composer
    /// choose; only meaningful for a striped level.
    pub chunk_blocks: Option<u32>,
    /// The device operands, in slot order (each `node:<id>`).
    pub devices: Vec<String>,
}

/// The mode a command line selected before its operands are validated.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum Mode {
    Create,
    Detail,
    Examine,
    Add,
    Remove,
    Stop,
}

impl Mode {
    /// The user-facing flag naming this mode, for a diagnostic.
    const fn flag(self) -> &'static str {
        match self {
            Self::Create => "--create",
            Self::Detail => "--detail",
            Self::Examine => "--examine",
            Self::Add => "--add",
            Self::Remove => "--remove",
            Self::Stop => "--stop",
        }
    }
}

/// Why a command line did not parse.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ParseError {
    /// No mode flag was given.
    NoMode,
    /// Two different mode flags were given.
    ConflictingModes,
    /// An unrecognised option.
    UnknownOption(String),
    /// A value option that was given no value.
    MissingValue(&'static str),
    /// A `--level` value that names nothing.
    BadLevel(String),
    /// A level TAIRiX does not compose (e.g. RAID4).
    LevelNotSupported(String),
    /// A `--raid-devices` value that is not a positive count.
    BadRaidDevices(String),
    /// A `--chunk` value that is not a block count.
    BadChunk(String),
    /// `--create` without `--level`.
    MissingLevel,
    /// `--create` without `--raid-devices`.
    MissingRaidDevices,
    /// `--chunk` given for a level that stores no stripe.
    ChunkNotAllowed,
    /// A value option (`--level`, `--raid-devices`, `--chunk`) in a mode that
    /// does not use it.
    OptionNotAllowed {
        /// The offending option.
        option: &'static str,
        /// The mode it was given in.
        mode: &'static str,
    },
    /// `--create` was given `n` devices but `--raid-devices=m`.
    DeviceCountMismatch {
        /// The count `--raid-devices` requested.
        expected: u16,
        /// The number of device operands given.
        got: usize,
    },
    /// A required operand was absent.
    MissingOperand(&'static str),
    /// An operand was given where none (or no more) was expected.
    UnexpectedOperand(String),
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoMode => f.write_str(
                "no mode given: use one of --create, --detail, --examine, --add, --remove, --stop",
            ),
            Self::ConflictingModes => f.write_str("only one mode may be given"),
            Self::UnknownOption(opt) => write!(f, "unknown option '{opt}'"),
            Self::MissingValue(opt) => write!(f, "option '{opt}' requires a value"),
            Self::BadLevel(value) => write!(f, "'{value}' is not a RAID level"),
            Self::LevelNotSupported(value) => {
                write!(f, "RAID level '{value}' is not supported")
            }
            Self::BadRaidDevices(value) => {
                write!(f, "'{value}' is not a positive device count")
            }
            Self::BadChunk(value) => write!(f, "'{value}' is not a block count"),
            Self::MissingLevel => f.write_str("--create requires --level"),
            Self::MissingRaidDevices => f.write_str("--create requires --raid-devices"),
            Self::ChunkNotAllowed => {
                f.write_str("--chunk applies only to a striped level (raid0/5/6/10/tp)")
            }
            Self::OptionNotAllowed { option, mode } => {
                write!(f, "option '{option}' is not valid with {mode}")
            }
            Self::DeviceCountMismatch { expected, got } => write!(
                f,
                "--raid-devices={expected} but {got} device(s) were given"
            ),
            Self::MissingOperand(what) => write!(f, "missing {what}"),
            Self::UnexpectedOperand(value) => write!(f, "unexpected operand '{value}'"),
        }
    }
}

/// Parse the argument vector (excluding `argv[0]`).
///
/// # Errors
///
/// A [`ParseError`] describing the first thing that did not parse.
pub fn parse<S: AsRef<str>>(args: &[S]) -> Result<Command, ParseError> {
    let mut state = State::default();
    let mut iter = args.iter().map(AsRef::as_ref);
    let mut options_ended = false;

    while let Some(arg) = iter.next() {
        if options_ended || arg == "-" || !arg.starts_with('-') {
            state.operands.push(arg.to_string());
            continue;
        }
        if arg == "--" {
            options_ended = true;
            continue;
        }
        if let Some(long) = arg.strip_prefix("--") {
            state.long_option(long, &mut iter)?;
        } else {
            // A `-xyz` cluster: `arg[1..]`.
            state.short_cluster(&arg[1..], &mut iter)?;
        }
    }

    state.finish()
}

/// The accumulator a single parse pass fills.
#[derive(Default)]
struct State {
    mode: Option<Mode>,
    level: Option<RaidLevel>,
    raid_devices: Option<u16>,
    chunk_blocks: Option<u32>,
    operands: Vec<String>,
    saw_help: bool,
    saw_version: bool,
}

impl State {
    /// Record a mode, rejecting a second, different one.
    fn set_mode(&mut self, mode: Mode) -> Result<(), ParseError> {
        match self.mode {
            Some(existing) if existing != mode => Err(ParseError::ConflictingModes),
            _ => {
                self.mode = Some(mode);
                Ok(())
            }
        }
    }

    /// Handle one `--name` or `--name=value` option.
    fn long_option<'a, I>(&mut self, long: &str, iter: &mut I) -> Result<(), ParseError>
    where
        I: Iterator<Item = &'a str>,
    {
        let (name, inline) = match long.split_once('=') {
            Some((name, value)) => (name, Some(value)),
            None => (long, None),
        };
        // Every option but the three value options rejects an `=value`.
        match name {
            "level" | "raid-devices" | "chunk" => {}
            "help" | "version" | "create" | "detail" | "examine" | "add" | "remove" | "stop"
                if inline.is_none() => {}
            "help" | "version" | "create" | "detail" | "examine" | "add" | "remove" | "stop" => {
                return Err(ParseError::UnknownOption(alloc::format!("--{long}")));
            }
            _ => return Err(ParseError::UnknownOption(alloc::format!("--{long}"))),
        }
        match name {
            "help" => self.saw_help = true,
            "version" => self.saw_version = true,
            "create" => self.set_mode(Mode::Create)?,
            "detail" => self.set_mode(Mode::Detail)?,
            "examine" => self.set_mode(Mode::Examine)?,
            "add" => self.set_mode(Mode::Add)?,
            "remove" => self.set_mode(Mode::Remove)?,
            "stop" => self.set_mode(Mode::Stop)?,
            "level" => {
                let value = value_of(inline, iter, "--level")?;
                self.set_level(&value)?;
            }
            "raid-devices" => {
                let value = value_of(inline, iter, "--raid-devices")?;
                self.set_raid_devices(&value)?;
            }
            "chunk" => {
                let value = value_of(inline, iter, "--chunk")?;
                self.set_chunk(&value)?;
            }
            _ => {}
        }
        Ok(())
    }

    /// Handle a `-xyz` short-option cluster.
    fn short_cluster<'a, I>(&mut self, cluster: &str, iter: &mut I) -> Result<(), ParseError>
    where
        I: Iterator<Item = &'a str>,
    {
        for (offset, flag) in cluster.char_indices() {
            match flag {
                'C' => self.set_mode(Mode::Create)?,
                'D' => self.set_mode(Mode::Detail)?,
                'E' => self.set_mode(Mode::Examine)?,
                'a' => self.set_mode(Mode::Add)?,
                'r' => self.set_mode(Mode::Remove)?,
                'S' => self.set_mode(Mode::Stop)?,
                'h' | '?' => self.saw_help = true,
                'V' => self.saw_version = true,
                'l' | 'n' | 'c' => {
                    // The value is the rest of the cluster, else the next arg.
                    let rest = &cluster[offset + flag.len_utf8()..];
                    let long = short_value_long(flag);
                    let value = if rest.is_empty() {
                        next_value(iter, long)?
                    } else {
                        rest.to_string()
                    };
                    match flag {
                        'l' => self.set_level(&value)?,
                        'n' => self.set_raid_devices(&value)?,
                        'c' => self.set_chunk(&value)?,
                        _ => {}
                    }
                    return Ok(());
                }
                other => {
                    return Err(ParseError::UnknownOption(alloc::format!("-{other}")));
                }
            }
        }
        Ok(())
    }

    /// Parse and record a `--level` value.
    fn set_level(&mut self, value: &str) -> Result<(), ParseError> {
        self.level = Some(parse_level(value)?);
        Ok(())
    }

    /// Parse and record a `--raid-devices` value.
    fn set_raid_devices(&mut self, value: &str) -> Result<(), ParseError> {
        let count = value
            .parse::<u16>()
            .map_err(|_| ParseError::BadRaidDevices(value.to_string()))?;
        if count == 0 {
            return Err(ParseError::BadRaidDevices(value.to_string()));
        }
        self.raid_devices = Some(count);
        Ok(())
    }

    /// Parse and record a `--chunk` value.
    fn set_chunk(&mut self, value: &str) -> Result<(), ParseError> {
        let blocks = value
            .parse::<u32>()
            .map_err(|_| ParseError::BadChunk(value.to_string()))?;
        self.chunk_blocks = Some(blocks);
        Ok(())
    }

    /// Turn the accumulated state into a [`Command`], validating operands.
    fn finish(self) -> Result<Command, ParseError> {
        // Help and version win over any mode, and over each other help wins.
        if self.saw_help {
            return Ok(Command::Help);
        }
        if self.saw_version {
            return Ok(Command::Version);
        }
        let mode = self.mode.ok_or(ParseError::NoMode)?;
        match mode {
            Mode::Create => self.finish_create(),
            Mode::Detail => {
                self.reject_value_options(Mode::Detail)?;
                let mut operands = self.operands.into_iter();
                let array = operands.next();
                if let Some(extra) = operands.next() {
                    return Err(ParseError::UnexpectedOperand(extra));
                }
                Ok(Command::Detail { array })
            }
            Mode::Examine => {
                self.reject_value_options(Mode::Examine)?;
                if let Some(extra) = self.operands.into_iter().next() {
                    return Err(ParseError::UnexpectedOperand(extra));
                }
                Ok(Command::Examine)
            }
            Mode::Add | Mode::Remove => {
                self.reject_value_options(mode)?;
                let mut operands = self.operands.iter();
                let array = operands
                    .next()
                    .ok_or(ParseError::MissingOperand("array"))?
                    .clone();
                let device = operands
                    .next()
                    .ok_or(ParseError::MissingOperand("device"))?
                    .clone();
                if let Some(extra) = operands.next() {
                    return Err(ParseError::UnexpectedOperand(extra.clone()));
                }
                if mode == Mode::Add {
                    Ok(Command::Add { array, device })
                } else {
                    Ok(Command::Remove { array, device })
                }
            }
            Mode::Stop => {
                self.reject_value_options(Mode::Stop)?;
                let mut operands = self.operands.into_iter();
                let array = operands.next().ok_or(ParseError::MissingOperand("array"))?;
                if let Some(extra) = operands.next() {
                    return Err(ParseError::UnexpectedOperand(extra));
                }
                Ok(Command::Stop { array })
            }
        }
    }

    /// Build a `--create` command, validating the level/count/chunk/devices.
    fn finish_create(self) -> Result<Command, ParseError> {
        let level = self.level.ok_or(ParseError::MissingLevel)?;
        let raid_devices = self.raid_devices.ok_or(ParseError::MissingRaidDevices)?;
        if self.chunk_blocks.is_some() && !level.is_striped() {
            return Err(ParseError::ChunkNotAllowed);
        }
        if self.operands.is_empty() {
            return Err(ParseError::MissingOperand("device"));
        }
        if self.operands.len() != usize::from(raid_devices) {
            return Err(ParseError::DeviceCountMismatch {
                expected: raid_devices,
                got: self.operands.len(),
            });
        }
        Ok(Command::Create(CreateArgs {
            level,
            raid_devices,
            chunk_blocks: self.chunk_blocks,
            devices: self.operands,
        }))
    }

    /// Reject a value option in a mode that does not consume it.
    fn reject_value_options(&self, mode: Mode) -> Result<(), ParseError> {
        let flag = mode.flag();
        if self.level.is_some() {
            return Err(ParseError::OptionNotAllowed {
                option: "--level",
                mode: flag,
            });
        }
        if self.raid_devices.is_some() {
            return Err(ParseError::OptionNotAllowed {
                option: "--raid-devices",
                mode: flag,
            });
        }
        if self.chunk_blocks.is_some() {
            return Err(ParseError::OptionNotAllowed {
                option: "--chunk",
                mode: flag,
            });
        }
        Ok(())
    }
}

/// The long spelling a short value flag corresponds to, for diagnostics.
const fn short_value_long(flag: char) -> &'static str {
    match flag {
        'l' => "--level",
        'n' => "--raid-devices",
        'c' => "--chunk",
        // Only the three value flags reach here.
        _ => "",
    }
}

/// The value of a value option: the inline `=value`, else the next argument.
fn value_of<'a, I>(
    inline: Option<&str>,
    iter: &mut I,
    long: &'static str,
) -> Result<String, ParseError>
where
    I: Iterator<Item = &'a str>,
{
    match inline {
        Some(value) => Ok(value.to_string()),
        None => next_value(iter, long),
    }
}

/// Consume the next argument as a value, or report the option needs one.
fn next_value<'a, I>(iter: &mut I, long: &'static str) -> Result<String, ParseError>
where
    I: Iterator<Item = &'a str>,
{
    iter.next()
        .map(ToString::to_string)
        .ok_or(ParseError::MissingValue(long))
}

/// Map a `--level` value to a [`RaidLevel`].
///
/// Accepts the numeric forms, the `raidN` forms, the descriptive words
/// `mirror`/`stripe`, and `tp` for triple parity — the spellings Linux
/// `mdadm` accepts, plus `tp`. RAID4 is named explicitly as unsupported
/// (TAIRiX has no dedicated-parity level), separately from an unknown level.
fn parse_level(value: &str) -> Result<RaidLevel, ParseError> {
    let lowered = value.to_ascii_lowercase();
    let level = match lowered.as_str() {
        "0" | "raid0" | "stripe" => RaidLevel::Stripe,
        "1" | "raid1" | "mirror" => RaidLevel::Mirror,
        "5" | "raid5" => RaidLevel::Parity,
        "6" | "raid6" => RaidLevel::DualParity,
        "10" | "raid10" => RaidLevel::Raid10,
        "tp" | "raid-tp" | "triple" | "tripleparity" => RaidLevel::TripleParity,
        "4" | "raid4" => return Err(ParseError::LevelNotSupported(value.to_string())),
        _ => return Err(ParseError::BadLevel(value.to_string())),
    };
    Ok(level)
}

#[cfg(test)]
mod tests;
