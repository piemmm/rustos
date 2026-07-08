//! The outcomes of parsing and running a `printf` command.

use alloc::string::String;
use core::fmt;

/// Why a `printf` invocation stopped instead of completing.
///
/// These are GNU `printf`'s *fatal* failures, with its diagnostics: a
/// missing FORMAT operand, a malformed conversion specification, a
/// malformed escape, or a dead output stream. Argument-conversion
/// problems (a non-numeric argument, a partially converted one, an
/// out-of-range value) are deliberately **not** variants here: the GNU
/// tool diagnoses them on standard error, converts as far as it can, and
/// carries on with exit status `1` — the client tracks those as a status,
/// not an abort.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PrintfError {
    /// No FORMAT operand was given.
    MissingOperand,
    /// An unknown conversion letter, a `%` with nothing after it, or a
    /// flag/width/precision on a conversion that does not accept it;
    /// carries the offending directive text (`%r`, `%5%`, `%10b`, …).
    InvalidConversion(String),
    /// A `\x` with no hex digit, or a `\u`/`\U` with too few.
    MissingHexEscape,
    /// A `\u`/`\U` naming a surrogate or an out-of-range code point;
    /// carries the escape as written (`\ud800`).
    InvalidUniversal(String),
    /// Writing to standard output (or the diagnostic stream) failed.
    Output,
}

impl fmt::Display for PrintfError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingOperand => write!(f, "missing operand"),
            Self::InvalidConversion(directive) => {
                write!(f, "{directive}: invalid conversion specification")
            }
            Self::MissingHexEscape => write!(f, "missing hexadecimal number in escape"),
            Self::InvalidUniversal(escape) => {
                write!(f, "invalid universal character name {escape}")
            }
            Self::Output => write!(f, "write error"),
        }
    }
}
