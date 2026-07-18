//! The one "should output be coloured, and at what depth?" decision.
//!
//! Every colour-capable command app faces the same question — the `--color`
//! switch says *when*, the kernel's console attestation says *whether stdout is
//! a terminal at all*, and the `TERM` value says *how much colour that terminal
//! renders*. [`resolve_color`] folds those three inputs into one answer so no
//! tool re-implements the policy (a piped `ls` and a piped `grep` must decide
//! identically). It is pure: the caller supplies the attestation and `TERM`; it
//! reads no environment and touches no syscall.

use crate::capabilities::ColorDepth;
use crate::term_type::from_term;

/// The `--color[=WHEN]` choice a tool parses from its command line.
///
/// The spellings are GNU's: `always`, `never`, and `auto` (the TAIRiX default,
/// which colours only an attested terminal).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ColorChoice {
    /// Colour only when standard output is an attested terminal (`auto`).
    Auto,
    /// Always colour, even on a console the kernel cannot attest — the honest
    /// escape hatch for a serial or remote session (`always`).
    Always,
    /// Never colour; emit plain bytes (`never`).
    Never,
}

impl ColorChoice {
    /// The colour depth to render at, or [`None`] for plain (uncoloured)
    /// output — see [`resolve_color`].
    #[must_use]
    pub fn resolve(self, attested: bool, term: Option<&str>) -> Option<ColorDepth> {
        resolve_color(self, attested, term)
    }
}

/// Decide the colour depth to render at.
///
/// * [`ColorChoice::Never`] is always plain.
/// * [`ColorChoice::Auto`] colours only an attested terminal, at the depth its
///   `TERM` advertises; an unattested console, or one whose `TERM` renders no
///   colour ([`ColorDepth::None`], e.g. `dumb`/`vt100`/unset), stays plain —
///   colour never guesses (fail closed).
/// * [`ColorChoice::Always`] colours regardless of attestation, at the `TERM`
///   depth but never below [`ColorDepth::Ansi16`]: an explicit `--color=always`
///   on a `dumb`/unset console still emits the universally-safe 16 ANSI
///   colours rather than nothing.
///
/// A [`Some`] depth is always a real colour depth ([`ColorDepth::Ansi16`] or
/// richer); [`None`] means "emit no escape sequences at all".
#[must_use]
pub fn resolve_color(
    choice: ColorChoice,
    attested: bool,
    term: Option<&str>,
) -> Option<ColorDepth> {
    let depth = from_term(term.unwrap_or("")).capabilities().color;
    match choice {
        ColorChoice::Never => None,
        ColorChoice::Auto => {
            if attested && depth != ColorDepth::None {
                Some(depth)
            } else {
                None
            }
        }
        ColorChoice::Always => Some(at_least_ansi16(depth)),
    }
}

/// `depth`, raised to at least [`ColorDepth::Ansi16`] — the floor `--color=always`
/// renders at when `TERM` advertises no colour.
fn at_least_ansi16(depth: ColorDepth) -> ColorDepth {
    match depth {
        ColorDepth::None => ColorDepth::Ansi16,
        other => other,
    }
}
