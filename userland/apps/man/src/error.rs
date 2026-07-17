//! The typed outcomes of running `man`.

use core::fmt;

use alloc::string::String;

use tairix_abi::Errno;
use tairix_help::{LoadError, NameError};
use tairix_sandbox::helpdoc::{HelpRefusal, HelpRenderFailure};
use tairix_sandbox::host::SandboxError;

/// Why a `man` invocation failed.
///
/// Every failure is a value the `Run` binary reports on standard error and
/// maps to an exit status (`2` for [`ManError::Usage`], `1` otherwise) —
/// no path panics and none is silently swallowed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ManError {
    /// The command line is outside the `man [-h | -?] <command> [topic]`
    /// grammar.
    Usage,
    /// No candidate bundle exists for the command word: neither the system
    /// app store nor any `PATH` entry holds `<word>.app` (plans/APPS.md §8).
    CommandNotFound(String),
    /// The word spells an explicit path to a bare program, which names no
    /// bundle and therefore has no `Help/` tree to read.
    NotABundle(String),
    /// The command word or topic is not a well-formed help document name, so
    /// no document can be looked up for it (the spelling grammar is the
    /// `lib/help` one that makes path traversal unrepresentable).
    InvalidName(NameError),
    /// The owning bundle exists but no locale — not even `en-US/` — holds
    /// the document: an ordinary "no help" outcome, reported cleanly.
    NoHelp {
        /// The command word the bundle was resolved for.
        word: String,
        /// The document (command or topic) name that was looked up.
        name: String,
    },
    /// The bundle's `Help/` tree is present but unusable: the backing store
    /// failed, the tree lists too many locales, or the selected document
    /// does not parse under the engine's fail-closed bounds.
    Tree(LoadError),
    /// The sandboxed help renderer failed: its worker could not be
    /// started, crashed mid-render, or produced a reply that cannot be
    /// believed. The document itself may be fine; the *renderer* is the
    /// problem, and the page is withheld rather than rendered in-process
    /// (fail closed). Document-parse refusals are reported as
    /// [`ManError::Tree`], not here.
    Render(HelpRenderFailure),
    /// Probing a candidate bundle was refused outright (not "no such
    /// bundle"): the refusal is final, mirroring the shell's launch rule
    /// that only `NotFound` moves to the next candidate.
    Store(Errno),
    /// The recursive app-store walk hit its safety bound (directory budget)
    /// before the word could be resolved. Reported rather than silently
    /// treated as "not found", so a truncated search never masquerades as
    /// an exhaustive one.
    SearchTruncated {
        /// The store root whose walk exhausted the budget.
        root: String,
    },
    /// Writing the rendered page to standard output failed.
    Output(Errno),
}

impl fmt::Display for ManError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ManError::Usage => f.write_str("usage error"),
            ManError::CommandNotFound(word) => write!(f, "no such command: {word}"),
            ManError::NotABundle(word) => {
                write!(f, "{word}: not an application bundle (no help to read)")
            }
            ManError::InvalidName(err) => write!(f, "invalid help name: {err}"),
            ManError::NoHelp { word, name } => {
                if word == name {
                    write!(f, "no help for {word}")
                } else {
                    write!(f, "no help topic {name} for {word}")
                }
            }
            ManError::Tree(err) => write!(f, "help unavailable: {err}"),
            ManError::Render(failure) => match failure {
                HelpRenderFailure::Sandbox(SandboxError::WorkerUnavailable(errno)) => {
                    write!(f, "help renderer unavailable: {errno}")
                }
                HelpRenderFailure::Sandbox(SandboxError::WorkerFailed) => {
                    f.write_str("help renderer failed while rendering the page")
                }
                HelpRenderFailure::Sandbox(SandboxError::RequestTooLarge)
                | HelpRenderFailure::Refused(HelpRefusal::MalformedRequest) => {
                    f.write_str("help renderer refused the request")
                }
                HelpRenderFailure::Refused(HelpRefusal::Document(err)) => {
                    write!(f, "help unavailable: {err}")
                }
                HelpRenderFailure::ReplyMalformed => {
                    f.write_str("help renderer produced an invalid page")
                }
            },
            ManError::Store(err) => write!(f, "cannot read the app store: {err}"),
            ManError::SearchTruncated { root } => write!(
                f,
                "app search under {root} stopped at its safety bound; \
                 name the bundle's path directly"
            ),
            ManError::Output(err) => write!(f, "cannot write the page: {err}"),
        }
    }
}

impl From<NameError> for ManError {
    fn from(err: NameError) -> Self {
        ManError::InvalidName(err)
    }
}

#[cfg(test)]
mod tests {
    use alloc::format;
    use alloc::string::String;

    use super::ManError;

    #[test]
    fn messages_name_the_failing_input() {
        assert_eq!(
            format!("{}", ManError::CommandNotFound(String::from("nope"))),
            "no such command: nope"
        );
        assert_eq!(
            format!(
                "{}",
                ManError::NoHelp {
                    word: String::from("ps"),
                    name: String::from("ps"),
                }
            ),
            "no help for ps"
        );
        assert_eq!(
            format!(
                "{}",
                ManError::NoHelp {
                    word: String::from("top"),
                    name: String::from("keys"),
                }
            ),
            "no help topic keys for top"
        );
    }
}
