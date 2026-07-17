//! The parsed shape of a `stress` command line (`plans/STRESSTEST.md` §7.3).
//!
//! The option surface follows the established `stress`/`stress-ng`
//! conventions (`AGENTS.md` §16.7): `--cpu`/`--io`/`--vm`/`--vm-bytes`/
//! `--hdd`/`--hdd-bytes`/`--timeout`/`--quiet` keep their GNU `stress`
//! meaning and value grammar (binary byte suffixes, `s`/`m`/`h` time
//! suffixes). The TAIRiX-only options — `--cache`, `--all`, `--overcommit`,
//! `--monitor`, `--background`, `--temp-path` — are additive and spelled so
//! they cannot collide with the GNU set.

use alloc::string::{String, ToString};

use crate::error::StressError;
use crate::worker::WorkerSpec;

/// The usage banner a usage error is reported with, and the fallback the
/// short-help switches print when `stress`'s own Help tree is unavailable.
pub const USAGE: &str = "usage: stress [--cpu N] [--io N] [--vm N] [--vm-bytes B] [--hdd N] \
[--hdd-bytes B] [--cache N] [--all N] [--overcommit P] [--timeout T] [--temp-path DIR] \
[--monitor] [--quiet] [--background] [--help | --version]";

/// How many workers of each kind a run dispatches.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Workers {
    /// CPU-spinner workers (`--cpu`).
    pub cpu: u32,
    /// Memory allocate/touch workers (`--vm`).
    pub vm: u32,
    /// Small-buffer write/sync/read workers (`--io`).
    pub io: u32,
    /// Large sequential write/verify/delete workers (`--hdd`).
    pub hdd: u32,
    /// Directory-walk/re-read cache-churn workers (`--cache`).
    pub cache: u32,
}

impl Workers {
    /// Total worker processes the run dispatches. Saturating: five
    /// maximal counts cannot wrap the total back to "nothing requested".
    #[must_use]
    pub const fn total(&self) -> u32 {
        self.cpu
            .saturating_add(self.vm)
            .saturating_add(self.io)
            .saturating_add(self.hdd)
            .saturating_add(self.cache)
    }
}

/// A fully parsed load run: what to dispatch and how to behave.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RunSpec {
    /// Per-kind worker counts. At least one is non-zero.
    pub workers: Workers,
    /// `--vm-bytes`: each memory worker's allocation target; `None` means
    /// "size from discovered RAM" (`crate::sizing`).
    pub vm_bytes: Option<u64>,
    /// `--hdd-bytes`: each disk worker's file-size target; `None` means
    /// "size from discovered free space" (`crate::sizing`).
    pub hdd_bytes: Option<u64>,
    /// `--overcommit P`: scale the *discovered* vm/hdd targets to `P`
    /// percent of the resource; may exceed 100 (the refusals that
    /// produces are expected outcomes).
    pub overcommit: Option<u32>,
    /// `--timeout T` in seconds; `None` runs until a signal ends the run.
    pub timeout_secs: Option<u64>,
    /// `--monitor`: run `sysmon` in the foreground for the duration.
    pub monitor: bool,
    /// `--quiet`: suppress the summary and progress lines on stdout
    /// (stderr diagnostics are never silenced).
    pub quiet: bool,
    /// `--background`: re-spawn the run detached (implies `--quiet`),
    /// print the controller PID, and return the prompt.
    pub background: bool,
    /// `--temp-path DIR`: the scratch directory override for the
    /// disk-touching workers; `None` uses the app-scoped per-user cache
    /// directory (`$HOME/Library/stress`).
    pub temp_path: Option<String>,
}

/// One thing the `stress` tool can do.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Command {
    /// Dispatch the described load run (the controller role).
    Run(RunSpec),
    /// Serve one worker's load loop (the internal `--worker` re-entry the
    /// controller spawns via the kernel's `@self` token; never typed by a
    /// user, never documented in `Help/`).
    Worker(WorkerSpec),
    /// Render `stress`'s own short help (`-h`/`-?`/`--help`).
    Help,
    /// Print the tool's name and version (`--version`, per §16.7).
    Version,
}

/// Parse `args` (the tool's arguments, excluding the program name) into a
/// [`Command`].
///
/// # Errors
///
/// [`StressError::Usage`] for anything outside the closed grammar: an
/// unknown option, a missing or malformed value, a run with no workers
/// requested, the `--monitor`/`--background` contradiction, or a malformed
/// internal `--worker` block.
pub fn parse(args: &[&str]) -> Result<Command, StressError> {
    // The internal worker re-entry is an exact, closed spelling decoded by
    // the worker codec; it never mixes with user options.
    if args.first() == Some(&"--worker") {
        return WorkerSpec::decode_argv(&args[1..])
            .map(Command::Worker)
            .ok_or(StressError::Usage);
    }

    let mut spec = RunSpec::default();
    let mut rest = args;
    while let Some((&arg, tail)) = rest.split_first() {
        rest = tail;
        match arg {
            "-h" | "-?" | "--help" => return Ok(Command::Help),
            "--version" => return Ok(Command::Version),
            "-q" | "--quiet" => spec.quiet = true,
            "--monitor" => spec.monitor = true,
            "--background" => spec.background = true,
            _ => {
                let (name, value) = split_value(arg, &mut rest)?;
                match name {
                    "--cpu" => spec.workers.cpu = parse_count(value)?,
                    "--vm" => spec.workers.vm = parse_count(value)?,
                    "--io" => spec.workers.io = parse_count(value)?,
                    "--hdd" => spec.workers.hdd = parse_count(value)?,
                    "--cache" => spec.workers.cache = parse_count(value)?,
                    "--all" => {
                        let n = parse_count(value)?;
                        spec.workers = Workers {
                            cpu: n,
                            vm: n,
                            io: n,
                            hdd: n,
                            cache: n,
                        };
                    }
                    "--vm-bytes" => spec.vm_bytes = Some(parse_bytes(value)?),
                    "--hdd-bytes" => spec.hdd_bytes = Some(parse_bytes(value)?),
                    "--overcommit" => spec.overcommit = Some(parse_percent(value)?),
                    "--timeout" => spec.timeout_secs = Some(parse_timeout(value)?),
                    "--temp-path" => {
                        if value.is_empty() {
                            return Err(StressError::Usage);
                        }
                        spec.temp_path = Some(value.to_string());
                    }
                    _ => return Err(StressError::Usage),
                }
            }
        }
    }

    // `--background` detaches from the terminal; a foreground monitor on a
    // detached run is a contradiction, refused typed rather than guessed at.
    if spec.monitor && spec.background {
        return Err(StressError::Usage);
    }
    // A run that dispatches nothing is a mistyped command, not a no-op.
    if spec.workers.total() == 0 {
        return Err(StressError::Usage);
    }
    // A detached run has no terminal to summarise to.
    if spec.background {
        spec.quiet = true;
    }
    Ok(Command::Run(spec))
}

/// Split `--name value` / `--name=value` into its name and value, consuming
/// the following token from `rest` for the spaced form.
fn split_value<'a>(arg: &'a str, rest: &mut &[&'a str]) -> Result<(&'a str, &'a str), StressError> {
    if let Some(eq) = arg.find('=') {
        return Ok((&arg[..eq], &arg[eq + 1..]));
    }
    let (&value, tail) = rest.split_first().ok_or(StressError::Usage)?;
    *rest = tail;
    Ok((arg, value))
}

/// Parse a worker count: a plain decimal `u32`.
fn parse_count(value: &str) -> Result<u32, StressError> {
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
        return Err(StressError::Usage);
    }
    value.parse::<u32>().map_err(|_| StressError::Usage)
}

/// Parse an overcommit percentage: a plain decimal `u32`, optionally with a
/// trailing `%`. Zero is refused (a zero-byte target dispatches nothing).
fn parse_percent(value: &str) -> Result<u32, StressError> {
    let digits = value.strip_suffix('%').unwrap_or(value);
    let percent = parse_count(digits)?;
    if percent == 0 {
        return Err(StressError::Usage);
    }
    Ok(percent)
}

/// Parse a byte size: decimal digits with GNU `stress`'s optional binary
/// suffix (`b`, `k`, `m`, `g`, `t`, upper- or lower-case). Zero is refused.
///
/// Byte counts are `u64` everywhere; an overflowing size is a usage error,
/// never a silent wrap.
fn parse_bytes(value: &str) -> Result<u64, StressError> {
    let (digits, shift) = match value.as_bytes().last() {
        Some(b'b' | b'B') => (&value[..value.len() - 1], 0u32),
        Some(b'k' | b'K') => (&value[..value.len() - 1], 10),
        Some(b'm' | b'M') => (&value[..value.len() - 1], 20),
        Some(b'g' | b'G') => (&value[..value.len() - 1], 30),
        Some(b't' | b'T') => (&value[..value.len() - 1], 40),
        Some(_) => (value, 0),
        None => return Err(StressError::Usage),
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(StressError::Usage);
    }
    let base: u64 = digits.parse().map_err(|_| StressError::Usage)?;
    let bytes = base.checked_shl(shift).filter(|scaled| {
        // `checked_shl` only refuses shifts >= 64; recover the overflow by
        // shifting back and comparing.
        scaled >> shift == base
    });
    match bytes {
        Some(0) | None => Err(StressError::Usage),
        Some(scaled) => Ok(scaled),
    }
}

/// Parse a run timeout: decimal seconds with GNU `stress`'s optional
/// `s`/`m`/`h`/`d`/`y` suffix. Zero is refused (a zero-length run is a
/// mistyped command).
fn parse_timeout(value: &str) -> Result<u64, StressError> {
    let (digits, scale) = match value.as_bytes().last() {
        Some(b's') => (&value[..value.len() - 1], 1u64),
        Some(b'm') => (&value[..value.len() - 1], 60),
        Some(b'h') => (&value[..value.len() - 1], 3600),
        Some(b'd') => (&value[..value.len() - 1], 86_400),
        Some(b'y') => (&value[..value.len() - 1], 31_536_000),
        Some(_) => (value, 1),
        None => return Err(StressError::Usage),
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(StressError::Usage);
    }
    let base: u64 = digits.parse().map_err(|_| StressError::Usage)?;
    match base.checked_mul(scale) {
        Some(0) | None => Err(StressError::Usage),
        Some(seconds) => Ok(seconds),
    }
}
