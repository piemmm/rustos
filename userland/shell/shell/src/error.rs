//! The error raised when a command line cannot be turned into something to
//! run.
//!
//! [`ParseError`] is the shell's only *line-aborting* error: a lexical fault
//! (an unterminated quote, a dangling escape) or a grammatical one (an empty
//! command, a redirection with no target, an unterminated `${...}`). When a
//! line fails to parse or expand, the shell runs **nothing** from it.
//!
//! Everything that can go wrong *after* a line is understood — a program that
//! is not found, a permission denial, a redirection target that cannot be
//! opened — is not a `ParseError`. It is an ordinary non-zero exit status
//! (carried back from the [`ProcessHost`](crate::ProcessHost) as an
//! [`Errno`](rustos_abi::Errno)), so that `;`, `&&`, and `||` keep working
//! across a failed command exactly as a POSIX shell requires. Neither path
//! ever panics.

use core::fmt;

/// A command line that could not be turned into an executable form.
///
/// Every variant names *what* is wrong without quoting the offending bytes
/// back, so the type stays allocation-free and `Copy`.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ParseError {
    /// A single- or double-quoted string was opened but never closed.
    UnterminatedQuote,
    /// A line ended on a trailing backslash, so the escaped character is
    /// missing.
    DanglingEscape,
    /// A file redirection operator (`<`, `>`, `>>`, `<>`, `&>`, …) was not
    /// followed by a target filename.
    MissingRedirectionTarget,
    /// A redirection operator the shell recognises but does not yet implement:
    /// a here-document (`<<`, `<<-`) or here-string (`<<<`). Failing closed is
    /// deliberate — the alternative would be to misread the body as commands.
    UnsupportedRedirection,
    /// A redirection whose meaning is not well defined: a descriptor-duplication
    /// form with neither a source descriptor nor a `-` close (`<&`, `2>&x`), or
    /// a descriptor number too large to represent.
    AmbiguousRedirection,
    /// A pipe (`|`) or sequence/logical operator (`&&`, `||`, `;`, `&`) had
    /// no command on one of its sides.
    MissingCommand,
    /// A `$` introducing a `${...}` expansion was not closed by `}`.
    UnterminatedExpansion,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::UnterminatedQuote => "unterminated quote",
            Self::DanglingEscape => "trailing backslash with nothing to escape",
            Self::MissingRedirectionTarget => "redirection is missing its target",
            Self::UnsupportedRedirection => "unsupported redirection operator",
            Self::AmbiguousRedirection => "ambiguous redirection",
            Self::MissingCommand => "expected a command",
            Self::UnterminatedExpansion => "unterminated ${...} expansion",
        };
        f.write_str(message)
    }
}

#[cfg(test)]
mod tests {
    use super::ParseError;

    #[test]
    fn parse_error_displays() {
        assert_eq!(
            alloc::format!("{}", ParseError::UnterminatedQuote),
            "unterminated quote"
        );
        assert_eq!(
            alloc::format!("{}", ParseError::MissingRedirectionTarget),
            "redirection is missing its target"
        );
    }

    extern crate alloc;
}
