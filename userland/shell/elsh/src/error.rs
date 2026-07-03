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
    /// A here-document (`<<`, `<<-`) whose body was never terminated by its
    /// delimiter line before the input ended. Failing closed is deliberate:
    /// the alternative would be to run the command with a partial body.
    UnterminatedHereDoc,
    /// A here-document whose body exceeded the fixed collection bound
    /// ([`MAX_HERE_DOC_BYTES`](crate::parser::MAX_HERE_DOC_BYTES)) or lost a
    /// body line to the reader's line-length limit. The body is discarded and
    /// the line runs nothing, so a truncated body never reaches a command.
    HereDocTooLarge,
    /// A redirection whose meaning is not well defined: a descriptor-duplication
    /// form with neither a source descriptor nor a `-` close (`<&`, `2>&x`), or
    /// a descriptor number too large to represent.
    AmbiguousRedirection,
    /// A redirection target whose spelling names a registered resource
    /// namespace (`sys:null`) but is not a well-formed resource reference
    /// (`sys:null@`). Failing the whole line closed is deliberate: the shell
    /// never falls back to opening it as a file, so a typo cannot silently
    /// create junk on disk.
    InvalidResourceTarget,
    /// A pipe (`|`) or sequence/logical operator (`&&`, `||`, `;`, `&`) had
    /// no command on one of its sides.
    MissingCommand,
    /// A `$` introducing a `${...}` expansion was not closed by `}`.
    UnterminatedExpansion,
    /// A compound-command form the shell does not (yet) support: `( list )`
    /// subshells, `{ list; }` brace groups, or a `function` definition.
    /// Failing closed is deliberate: silently treating `(` or `{` as an
    /// ordinary word would run a different command than the user wrote.
    UnsupportedCompound,
    /// A process substitution (`<(...)`, `>(...)`, `=(...)`). `=(...)` is
    /// permanently unsupported (RustOS has no scratch filesystem to back it);
    /// the stream forms await the launch plumbing. All three fail closed so
    /// the parenthesised command is never misread as a filename.
    UnsupportedProcessSubstitution,
    /// A `{var}` dynamic-descriptor redirection whose variable does not hold
    /// a previously allocated descriptor number (for the `{var}>&-` /
    /// `{var}>&m` forms that reuse one).
    BadDynamicFd,
    /// A token in a position the grammar gives no meaning — e.g. a `!`
    /// negation word after the pipeline has begun. Failing closed is
    /// deliberate: silently dropping the token would run a different command
    /// than the user wrote.
    UnexpectedToken,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::UnterminatedQuote => "unterminated quote",
            Self::DanglingEscape => "trailing backslash with nothing to escape",
            Self::MissingRedirectionTarget => "redirection is missing its target",
            Self::UnterminatedHereDoc => "here-document is missing its terminator",
            Self::HereDocTooLarge => "here-document too large",
            Self::AmbiguousRedirection => "ambiguous redirection",
            Self::InvalidResourceTarget => "malformed resource-reference redirection target",
            Self::MissingCommand => "expected a command",
            Self::UnterminatedExpansion => "unterminated ${...} expansion",
            Self::UnsupportedCompound => "compound commands are not supported",
            Self::UnsupportedProcessSubstitution => "process substitution is not supported",
            Self::BadDynamicFd => "{var} does not name an allocated descriptor",
            Self::UnexpectedToken => "unexpected token",
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
