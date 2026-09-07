//! TAIRiX `flock` — run a command holding an advisory file lock
//! (`plans/FILELOCK.md`).
//!
//! The canonical way a shell script gets mutual exclusion: `flock` takes a
//! whole-file advisory lock, runs the command while holding it, and releases
//! it when the command finishes. Two runs of the same script naming the same
//! lock file therefore never overlap, and neither has to invent a lockfile
//! protocol with a stale-lock problem — the kernel releases the lock when the
//! holding process ends, however it ends.
//!
//! The option surface follows util-linux `flock(1)` (`-s`, `-x`, `-n`,
//! `-w`, `-E`, `-v`), so a script written for it works here. Three of its
//! options are deliberately absent rather than faked, because TAIRiX has no
//! honest implementation of them:
//!
//! * `-u`/`--unlock` and `-o`/`--close` exist for util-linux's
//!   *file-descriptor* form (`flock -u 9`), where a shell has already opened
//!   the descriptor with `exec 9>file`. TAIRiX locks are owned by the open
//!   file description and released when it closes, so with no descriptor
//!   form there is nothing for either option to act on.
//! * `-c` runs its argument through a shell. It needs a shell-invocation
//!   seam this tool does not have; `flock <file> elsh -c '…'` is the
//!   explicit spelling and does not hide which shell runs.
//!
//! # What this crate is
//!
//! A **parse-and-run engine**. Everything touching the outside world is an
//! injected seam, so the whole decision surface — including every exit code
//! — is host-testable without a kernel:
//!
//! * [`Session`] — take the lock, run the command, release the lock.
//! * [`Output`] — the short help (standard output) and the `-v` notes
//!   (standard error).
//! * [`tairix_help::HelpSource`] — the tool's own `Help/` tree.
//!
//! # Fail closed
//!
//! An unrecognised option, a missing operand, or a malformed `-w`/`-E`
//! value is [`FlockError::Usage`] and **nothing is locked and nothing is
//! run**. A lock that cannot be taken under `-n` or `-w` exits with the
//! conflict code rather than running the command, so a script can never
//! mistake "I could not get the lock" for "the command succeeded".

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

use tairix_abi::Errno;
use tairix_help::{own_short_help, HelpSource};

/// The usage banner a usage error is reported with, and the fallback the
/// short-help switches print when `flock`'s own Help tree is unavailable.
pub const USAGE: &str = "\
usage: flock [options] file command [argument...]

  -s, --shared             take a shared (read) lock
  -x, --exclusive          take an exclusive (write) lock (default)
  -n, --nonblock           fail rather than wait for the lock
  -w, --timeout <seconds>  wait at most <seconds> for the lock
  -E, --conflict-code <n>  exit with <n> when -n or -w gives up (default 1)
  -v, --verbose            report what was locked, on standard error
  -?, --help               show this message

The lock covers the whole file and is held for as long as the command runs.
`--` ends option parsing. Exit status is the command's own, or the conflict
code when the lock could not be taken.
";

/// Exit status for a usage error: nothing was locked and nothing ran.
pub const EXIT_USAGE: i32 = 2;

/// Exit status for a command word that resolved to nothing, matching the
/// shell's own spelling for it.
pub const EXIT_NOT_FOUND: i32 = 127;

/// Exit status for a command that resolved but could not be executed,
/// matching the shell's own spelling for it.
pub const EXIT_NOT_EXECUTABLE: i32 = 126;

/// The conflict status `-E` defaults to.
pub const DEFAULT_CONFLICT_CODE: i32 = 1;

/// Nanoseconds in one second, for the `-w` operand's conversion.
const NANOS_PER_SECOND: u64 = 1_000_000_000;

/// The largest `-w` operand accepted, in whole seconds.
///
/// A wait is expressed to the kernel in nanoseconds, so a longer span would
/// not survive the conversion. A century is past any legitimate script's
/// patience, and refusing rather than saturating keeps a typo from reading
/// as "wait forever".
const MAX_TIMEOUT_SECONDS: u64 = u64::MAX / NANOS_PER_SECOND;

/// Which lock a run asks for.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Mode {
    /// Shared: coexists with other shared holders, excludes every writer.
    Shared,
    /// Exclusive: excludes every other holder. The default.
    Exclusive,
}

/// How long a run will wait for the lock.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Wait {
    /// Wait indefinitely (the default).
    Forever,
    /// Do not wait at all (`-n`).
    Never,
    /// Wait at most this many nanoseconds (`-w`).
    Until(u64),
}

/// One parsed command line.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Command<'a> {
    /// Render the short help and do nothing else.
    pub help: bool,
    /// The lock to take.
    pub mode: Mode,
    /// How long to wait for it.
    pub wait: Wait,
    /// The status to exit with when the lock could not be taken.
    pub conflict_code: i32,
    /// Report the acquisition on standard error.
    pub verbose: bool,
    /// The file whose lock serialises the runs.
    pub file: &'a str,
    /// The command word to run, and its arguments.
    pub argv: Vec<&'a str>,
}

/// Why a run could not proceed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FlockError {
    /// The command line was malformed; nothing was locked and nothing ran.
    Usage,
    /// The lock could not be taken, for a reason waiting would not fix.
    Lock(Errno),
    /// The command word resolved to nothing.
    NotFound(String),
    /// The command resolved but could not be executed.
    NotExecutable(String),
    /// The command could not be run, or its status could not be collected.
    Run(Errno),
    /// A stream stopped accepting bytes.
    Output,
}

impl fmt::Display for FlockError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Usage => f.write_str("bad usage"),
            Self::Lock(err) => write!(f, "cannot lock: {err}"),
            Self::NotFound(word) => write!(f, "{word}: command not found"),
            Self::NotExecutable(word) => write!(f, "{word}: command not executable"),
            Self::Run(err) => write!(f, "cannot run: {err}"),
            Self::Output => f.write_str("cannot write output"),
        }
    }
}

impl FlockError {
    /// The exit status this failure reports.
    #[must_use]
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Usage => EXIT_USAGE,
            Self::NotFound(_) => EXIT_NOT_FOUND,
            Self::NotExecutable(_) => EXIT_NOT_EXECUTABLE,
            Self::Lock(_) | Self::Run(_) | Self::Output => 1,
        }
    }
}

/// The outcome of running the command word.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Ran {
    /// The command ran and left this status.
    Exited(i32),
    /// The command word resolved to nothing.
    NotFound,
    /// The command resolved but could not be executed.
    NotExecutable,
}

/// Taking the lock, running the command, and releasing the lock.
///
/// One seam rather than three, because the three are one transaction: the
/// lock must be held for exactly the span the command runs in, and splitting
/// them across seams would let a caller wire an order that does not hold it.
pub trait Session {
    /// Open `path` (creating it if absent) and take `mode` over the whole of
    /// it, waiting as `wait` says.
    ///
    /// # Errors
    ///
    /// [`Errno::WouldBlock`] when a `Never` wait found the lock held,
    /// [`Errno::TimedOut`] when an `Until` wait ran out, or whatever refusal
    /// the open or the lock produced.
    fn lock(&self, path: &str, mode: Mode, wait: Wait) -> Result<(), Errno>;

    /// Run `word` with `args`, waiting for it to finish.
    ///
    /// # Errors
    ///
    /// Whatever refusal the launch or the status collection produced.
    fn run(&self, word: &str, args: &[&str]) -> Result<Ran, Errno>;
}

/// A byte sink the engine writes through.
pub trait Output {
    /// Write every byte of `bytes`.
    ///
    /// # Errors
    ///
    /// [`Errno`] when the stream stopped accepting bytes.
    fn write_all(&self, bytes: &[u8]) -> Result<(), Errno>;
}

/// Parse an argument vector into a [`Command`].
///
/// The vector is the inherited one, so element 0 is the program name and is
/// skipped. Option parsing follows the GNU conventions the sibling tools
/// use: clustered short flags (`-sn`), a `-w5` or `-w 5` value, `--long` and
/// `--long=value` forms, and `--` ending option parsing so a file or command
/// beginning with a dash stays reachable.
///
/// # Errors
///
/// [`FlockError::Usage`] for an unrecognised option, a missing or malformed
/// value, a missing file, or a missing command word.
pub fn parse<'a>(argv: &[&'a str]) -> Result<Command<'a>, FlockError> {
    let mut command = Command {
        help: false,
        mode: Mode::Exclusive,
        wait: Wait::Forever,
        conflict_code: DEFAULT_CONFLICT_CODE,
        verbose: false,
        file: "",
        argv: Vec::new(),
    };
    let mut operands: Vec<&'a str> = Vec::new();
    let mut rest = argv.iter().skip(1);
    let mut only_operands = false;

    while let Some(&arg) = rest.next() {
        // Everything after the first operand is the command and its own
        // arguments, which must reach the command verbatim — `flock lock ls
        // -l` passes `-l` to `ls`, it does not mean anything to `flock`.
        if only_operands || !operands.is_empty() {
            operands.push(arg);
            continue;
        }
        match arg {
            "--" => only_operands = true,
            "-?" | "-h" | "--help" => command.help = true,
            "-s" | "--shared" => command.mode = Mode::Shared,
            "-x" | "--exclusive" => command.mode = Mode::Exclusive,
            "-n" | "--nonblock" | "--nb" => command.wait = Wait::Never,
            "-v" | "--verbose" => command.verbose = true,
            "-w" | "--timeout" | "--wait" => {
                let value = rest.next().ok_or(FlockError::Usage)?;
                command.wait = Wait::Until(parse_seconds(value)?);
            }
            "-E" | "--conflict-code" => {
                let value = rest.next().ok_or(FlockError::Usage)?;
                command.conflict_code = parse_code(value)?;
            }
            _ => {
                if let Some(value) = arg.strip_prefix("--timeout=") {
                    command.wait = Wait::Until(parse_seconds(value)?);
                } else if let Some(value) = arg.strip_prefix("--conflict-code=") {
                    command.conflict_code = parse_code(value)?;
                } else if let Some(value) = arg.strip_prefix("-w") {
                    command.wait = Wait::Until(parse_seconds(value)?);
                } else if let Some(value) = arg.strip_prefix("-E") {
                    command.conflict_code = parse_code(value)?;
                } else if arg.starts_with("--") {
                    // An unrecognised long option is refused, never taken as
                    // the file name: `flock --nonblok lock cmd` would
                    // otherwise lock a file called `--nonblok` and run
                    // `lock cmd`, silently doing something else entirely.
                    return Err(FlockError::Usage);
                } else if let Some(cluster) = short_cluster(arg) {
                    apply_cluster(cluster, &mut command)?;
                } else {
                    operands.push(arg);
                }
            }
        }
    }

    if command.help {
        return Ok(command);
    }
    let mut operands = operands.into_iter();
    command.file = operands.next().ok_or(FlockError::Usage)?;
    command.argv = operands.collect();
    if command.argv.is_empty() {
        return Err(FlockError::Usage);
    }
    Ok(command)
}

/// The flag letters of a clustered short option (`-sn`), or `None` when
/// `arg` is not one.
fn short_cluster(arg: &str) -> Option<&str> {
    let rest = arg.strip_prefix('-')?;
    if rest.is_empty() || arg.starts_with("--") {
        return None;
    }
    Some(rest)
}

/// Apply each letter of a clustered short option.
///
/// Only the value-free flags cluster: `-w` and `-E` take an operand, so a
/// cluster naming one is a usage error rather than a silent guess about
/// where the value went.
fn apply_cluster(cluster: &str, command: &mut Command<'_>) -> Result<(), FlockError> {
    for letter in cluster.chars() {
        match letter {
            's' => command.mode = Mode::Shared,
            'x' => command.mode = Mode::Exclusive,
            'n' => command.wait = Wait::Never,
            'v' => command.verbose = true,
            '?' | 'h' => command.help = true,
            _ => return Err(FlockError::Usage),
        }
    }
    Ok(())
}

/// Parse a `-w` operand: whole seconds, converted to the nanoseconds the
/// kernel waits in.
fn parse_seconds(value: &str) -> Result<u64, FlockError> {
    let seconds = parse_u64(value)?;
    if seconds > MAX_TIMEOUT_SECONDS {
        return Err(FlockError::Usage);
    }
    Ok(seconds * NANOS_PER_SECOND)
}

/// Parse an `-E` operand: an exit status, which the process ABI carries in
/// the low byte like every other.
fn parse_code(value: &str) -> Result<i32, FlockError> {
    let raw = parse_u64(value)?;
    i32::try_from(raw).map_err(|_| FlockError::Usage)
}

/// Parse an unsigned decimal operand, refusing an empty string, a sign, a
/// non-digit, or a value too large — never saturating one into a different
/// instruction.
fn parse_u64(value: &str) -> Result<u64, FlockError> {
    if value.is_empty() {
        return Err(FlockError::Usage);
    }
    let mut out: u64 = 0;
    for byte in value.bytes() {
        let digit = match byte {
            b'0'..=b'9' => u64::from(byte - b'0'),
            _ => return Err(FlockError::Usage),
        };
        out = out
            .checked_mul(10)
            .and_then(|scaled| scaled.checked_add(digit))
            .ok_or(FlockError::Usage)?;
    }
    Ok(out)
}

/// Take the lock, run the command, and report the status to exit with.
///
/// The lock is released by the process ending, so there is no unlock step to
/// forget on an error path: the kernel owns that guarantee.
///
/// # Errors
///
/// [`FlockError`] for a refusal the caller must report. A *conflict* — the
/// lock held when `-n` or `-w` gave up — is not an error here: it returns
/// the configured conflict code, because failing to get the lock is an
/// ordinary outcome a script branches on.
pub fn run<S: Session, O: Output, N: Output>(
    command: &Command<'_>,
    locale: Option<&str>,
    session: &S,
    help: &dyn HelpSource,
    out: &O,
    notices: &N,
) -> Result<i32, FlockError> {
    if command.help {
        let text =
            own_short_help(help, locale, "flock").unwrap_or_else(|| Vec::from(USAGE.as_bytes()));
        out.write_all(&text).map_err(|_| FlockError::Output)?;
        return Ok(0);
    }

    match session.lock(command.file, command.mode, command.wait) {
        Ok(()) => {}
        // Giving up on a contended lock is an answer, not a failure: the
        // script asked to be told rather than to wait.
        Err(Errno::WouldBlock | Errno::TimedOut) => {
            if command.verbose {
                let _ = notices.write_all(b"flock: lock already held\n");
            }
            return Ok(command.conflict_code);
        }
        Err(err) => return Err(FlockError::Lock(err)),
    }
    if command.verbose {
        let _ = notices.write_all(b"flock: lock held\n");
    }

    let (word, args) = command.argv.split_first().ok_or(FlockError::Usage)?;
    match session.run(word, args) {
        Ok(Ran::Exited(status)) => Ok(status),
        Ok(Ran::NotFound) => Err(FlockError::NotFound(String::from(*word))),
        Ok(Ran::NotExecutable) => Err(FlockError::NotExecutable(String::from(*word))),
        Err(err) => Err(FlockError::Run(err)),
    }
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
