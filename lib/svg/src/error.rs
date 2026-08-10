//! The closed set of reasons an SVG asset is rejected.
//!
//! SVG is untrusted input: every structural or content problem resolves to
//! one of these variants so the caller can fail closed to a built-in fallback
//! rather than crash the compositor. The decoder never panics for any byte
//! string.

/// Why [`decode`](crate::decode) rejected an SVG document.
///
/// A closed enum: each variant names a specific, recoverable rejection, so a
/// caller can log the precise reason and substitute its fallback artwork.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum SvgError {
    /// The bytes were not valid UTF-8, so they are not an SVG document.
    NotUtf8,
    /// The document's root element is not `<svg>`, or it holds no element at
    /// all.
    MissingRoot,
    /// The `<svg>` element declared neither a `viewBox` nor a usable
    /// `width`/`height`, so it has no coordinate system to draw in.
    MissingViewBox,
    /// A `viewBox`, `preserveAspectRatio`, or element extent was malformed or
    /// had no positive area.
    InvalidViewBox,
    /// A coordinate, length, transform, or other numeric value was outside
    /// the grammar.
    InvalidNumber,
    /// A colour or paint value was outside the accepted syntax.
    InvalidColor,
    /// A `path` command was unknown, or its parameters did not parse.
    UnsupportedPath,
    /// The XML was structurally malformed (an unterminated tag, quote, or
    /// comment, or a close tag that does not match what it ends).
    Malformed,
    /// The document exceeded a decode resource limit — element, nesting,
    /// layer, vertex, stop, or reference depth — so it is refused rather than
    /// allowed to exhaust memory or draw time.
    TooComplex,
}
