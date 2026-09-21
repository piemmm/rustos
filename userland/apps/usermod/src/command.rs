//! The parsed shape of a `usermod` command line.

use alloc::string::String;
use alloc::vec::Vec;

use tairix_util::argv::option_value;

/// The command line was not understood: an unknown option, a missing
/// value, a non-decimal id, a contradictory lock request, or an operand
/// count other than one. Nothing is modified.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UsageError;

/// Which lock state a command line asked for.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Lock {
    /// `-L`: bar the account from logging in.
    Locked,
    /// `-U`: let it log in again.
    Unlocked,
}

/// What a `usermod` command line asks to change.
///
/// Every field is optional, and an absent one is left exactly as the
/// account holds it — the tool never rewrites a field nobody named.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Changes {
    /// `-c`: the account comment / display name.
    pub comment: Option<String>,
    /// `-d`: the home directory.
    pub home: Option<String>,
    /// `-s`: the login shell.
    pub shell: Option<String>,
    /// `-g`: the primary group id.
    pub primary_gid: Option<u32>,
    /// `-G`: the whole supplementary group set, replacing the current one.
    pub supplementary_gids: Option<Vec<u32>>,
    /// `--grants`: the whole capability grant ceiling, replacing the
    /// current one.
    pub grants: Option<String>,
    /// `-L` / `-U`: the lock state.
    pub lock: Option<Lock>,
}

impl Changes {
    /// Whether any identity field was named, which is what decides
    /// whether a whole-record replacement is sent at all.
    #[must_use]
    pub const fn touches_identity(&self) -> bool {
        self.comment.is_some()
            || self.home.is_some()
            || self.shell.is_some()
            || self.primary_gid.is_some()
            || self.supplementary_gids.is_some()
    }

    /// Whether the line asked for nothing at all.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        !self.touches_identity() && self.grants.is_none() && self.lock.is_none()
    }
}

/// One thing the `usermod` tool can do.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Command {
    /// Apply `changes` to the named account.
    Modify {
        /// The account to modify.
        name: String,
        /// What to change about it.
        changes: Changes,
    },
    /// Render this command's own short help (`-h`/`-?`/`--help`).
    Help,
}

/// Parse `args` (the tool's arguments, excluding the program name).
///
/// The grammar is
/// `usermod [-c COMMENT] [-d HOME] [-s SHELL] [-g GID] [-G LIST] [-L | -U]
/// [--grants LIST] [--] NAME`:
///
/// * `-c` / `--comment` — the account comment / display name.
/// * `-d` / `--home` — the home directory.
/// * `-s` / `--shell` — the login shell.
/// * `-g` / `--gid` — the numeric primary group id.
/// * `-G` / `--groups` — the comma-separated numeric supplementary set,
///   which **replaces** the current one (GNU's `-a` append is deliberately
///   absent: the syscall takes whole sets, and an append that silently read
///   a stale listing would be worse than an explicit replacement).
/// * `-L` / `--lock`, `-U` / `--unlock` — the lock state; naming both is a
///   usage error rather than a guess.
/// * `--grants` — the comma-separated capability ceiling, a TAIRiX concept
///   with no coreutils counterpart, so it is spelled long-only and cannot
///   collide with a shadow-utils switch.
/// * `-h` / `-?` / `--help` — this command's own short help (wins
///   immediately).
/// * `--` — end option parsing; every later argument is an operand.
///
/// Each value-taking option accepts its value attached (`-g0`,
/// `--gid=0`) or as the following argument. Exactly one operand — the
/// account name — is required, and a line that asks for no change at all
/// is a usage error rather than a no-op run.
///
/// # Errors
///
/// [`UsageError`] for an unrecognised option, a missing value, a
/// non-decimal id, both lock switches, an operand count other than one, or
/// an empty change set.
pub fn parse(args: &[&str]) -> Result<Command, UsageError> {
    let mut changes = Changes::default();
    let mut operands: Vec<&str> = Vec::new();
    let mut options_done = false;
    let mut index = 0;
    while index < args.len() {
        let arg = args[index];
        index += 1;
        if options_done {
            operands.push(arg);
            continue;
        }
        if arg == "--" {
            options_done = true;
            continue;
        }
        if let Some(long) = arg.strip_prefix("--") {
            let (name, inline) = match long.split_once('=') {
                Some((name, value)) => (name, Some(value)),
                None => (long, None),
            };
            match name {
                "help" => return Ok(Command::Help),
                "comment" => changes.comment = Some(text(inline, args, &mut index)?),
                "home" => changes.home = Some(text(inline, args, &mut index)?),
                "shell" => changes.shell = Some(text(inline, args, &mut index)?),
                "gid" => changes.primary_gid = Some(id(inline, args, &mut index)?),
                "groups" => {
                    changes.supplementary_gids = Some(id_list(inline, args, &mut index)?);
                }
                "grants" => changes.grants = Some(text(inline, args, &mut index)?),
                "lock" => set_lock(&mut changes, Lock::Locked)?,
                "unlock" => set_lock(&mut changes, Lock::Unlocked)?,
                _ => return Err(UsageError),
            }
            continue;
        }
        if let Some(rest) = arg.strip_prefix('-').filter(|rest| !rest.is_empty()) {
            let mut chars = rest.chars();
            let letter = chars.next().ok_or(UsageError)?;
            let attached = chars.as_str();
            let inline = (!attached.is_empty()).then_some(attached);
            match letter {
                'h' | '?' => return Ok(Command::Help),
                'c' => changes.comment = Some(text(inline, args, &mut index)?),
                'd' => changes.home = Some(text(inline, args, &mut index)?),
                's' => changes.shell = Some(text(inline, args, &mut index)?),
                'g' => changes.primary_gid = Some(id(inline, args, &mut index)?),
                'G' => changes.supplementary_gids = Some(id_list(inline, args, &mut index)?),
                'L' => set_lock(&mut changes, Lock::Locked)?,
                'U' => set_lock(&mut changes, Lock::Unlocked)?,
                _ => return Err(UsageError),
            }
            continue;
        }
        operands.push(arg);
    }
    let [name] = operands.as_slice() else {
        return Err(UsageError);
    };
    if changes.is_empty() {
        return Err(UsageError);
    }
    Ok(Command::Modify {
        name: String::from(*name),
        changes,
    })
}

/// Record a lock request, refusing a line that asks for both states.
fn set_lock(changes: &mut Changes, lock: Lock) -> Result<(), UsageError> {
    match changes.lock {
        Some(held) if held != lock => Err(UsageError),
        _ => {
            changes.lock = Some(lock);
            Ok(())
        }
    }
}

/// An option's value as owned text.
fn text(inline: Option<&str>, args: &[&str], index: &mut usize) -> Result<String, UsageError> {
    option_value(inline, args, index)
        .map(String::from)
        .ok_or(UsageError)
}

/// An option's value as a decimal id.
fn id(inline: Option<&str>, args: &[&str], index: &mut usize) -> Result<u32, UsageError> {
    option_value(inline, args, index)
        .and_then(|value| value.parse::<u32>().ok())
        .ok_or(UsageError)
}

/// An option's value as a comma-separated decimal id list.
///
/// An empty string is the empty set — how a caller clears a supplementary
/// membership — while an empty *element* (`1,,2`) is malformed.
fn id_list(inline: Option<&str>, args: &[&str], index: &mut usize) -> Result<Vec<u32>, UsageError> {
    let raw = option_value(inline, args, index).ok_or(UsageError)?;
    if raw.is_empty() {
        return Ok(Vec::new());
    }
    raw.split(',')
        .map(|part| part.parse::<u32>().map_err(|_| UsageError))
        .collect()
}

#[cfg(test)]
#[path = "command_tests.rs"]
mod tests;
