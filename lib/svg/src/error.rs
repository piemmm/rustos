//! The closed set of reasons an SVG asset is rejected.
//!
//! SVG is untrusted input: every structural or
//! content problem resolves to one of these variants so the caller can fail
//! closed to a built-in fallback rather than crash the compositor. The decoder never panics for any byte string.

/// Why [`decode`](crate::decode) rejected an SVG document.
///
/// A closed enum: each variant names a specific, recoverable rejection, so a
/// caller can log the precise reason and substitute its fallback artwork.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum SvgError {
    /// The bytes were not valid UTF-8, so they are not an SVG document.
    NotUtf8,
    /// No `<svg>` root element was found.
    MissingRoot,
    /// The `<svg>` element declared no usable `viewBox` (or `width`/`height`).
    MissingViewBox,
    /// The `viewBox` / size was syntactically malformed.
    InvalidViewBox,
    /// The design box is not square; the desktop's vector forms are authored
    /// on a square design grid (`lib/cursor`, `lib/icon`).
    NonSquareViewBox,
    /// A coordinate, length, or opacity was not a representable integer in the
    /// supported subset.
    InvalidNumber,
    /// A `fill` value was outside the supported colour subset.
    InvalidColor,
    /// A `path` used a command this rasteriser cannot turn into a polygon
    /// (curves, arcs); the asset must be authored with straight segments.
    UnsupportedPath,
    /// The XML was structurally malformed (an unterminated tag, quote, or
    /// comment).
    Malformed,
    /// The document exceeded a decode resource limit (layer or vertex count),
    /// so it is refused rather than allowed to exhaust memory.
    TooComplex,
}
