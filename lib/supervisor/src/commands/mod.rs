//! The built-in Supervisor commands, grouped by purpose.
//!
//! * [`control`] — commands that change boot state: `continue`, `mount`,
//!   `reboot`, `poweroff`. Each is audited before it acts.
//! * [`info`] — read-only presentation of state the kernel already computes:
//!   `version`, `mem`, `cpu`, `hw`, `disk`, `partitions`, `arxfs`, `ls`,
//!   `uptime`, `date`, plus the REPL niceties `echo` and `clear`.
//! * [`diag`] — the heavier diagnostics: `log`, `panic-log`, `memtest`,
//!   `test disk`.
//!
//! Every handler has the signature the dispatch table expects
//! (`fn(&[&[u8]], &mut Session) -> Flow`) and returns [`Flow::Stay`] unless
//! it is a control command that leaves the REPL. None panics on any input.

pub mod control;
pub mod diag;
pub mod info;

#[cfg(test)]
pub mod test_support;

use crate::Report;

/// Interpret a byte token as UTF-8, or `None` if it is not valid UTF-8.
///
/// Command arguments are operator-typed words; a non-UTF-8 argument is
/// reported as invalid rather than guessed at.
pub(crate) fn arg_str(token: &[u8]) -> Option<&str> {
    core::str::from_utf8(token).ok()
}

/// Parse a token as an unsigned decimal count, or `None` if it is not one.
pub(crate) fn arg_u32(token: &[u8]) -> Option<u32> {
    let text = arg_str(token)?;
    text.parse().ok()
}

/// Report that a required argument was missing, naming the usage.
pub(crate) fn missing_arg(out: &mut dyn Report, usage: &str) {
    out.write_str("supervisor: missing argument; usage: ");
    out.line(usage);
}
