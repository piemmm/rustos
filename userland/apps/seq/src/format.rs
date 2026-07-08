//! The `seq` format grammar behind `-f` and the default formats.
//!
//! GNU `seq` hands its format to C `printf(3)`; RustOS is Rust-only, so
//! the rendering itself is the one shared C-locale engine
//! (`rustos_util::cfloat`, also consumed by `printf`). This module owns
//! only what is `seq`'s: the validation of the one directive `seq`
//! permits — flags `-+#0 '`, optional width and precision, and a
//! conversion in `efgaEFGA` — exactly as GNU `long_double_format`
//! validates it, with the same diagnostics, plus the literal prefix and
//! suffix around the directive (`%%` collapsed).

use alloc::string::String;
use alloc::vec::Vec;

use rustos_util::cfloat::{FloatConversion, FloatDirective};

use crate::error::SeqError;

/// A validated `seq` format: literal prefix, one directive, literal
/// suffix. `%%` in the literals is already collapsed to `%`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Format {
    /// The literal text before the directive (unescaped).
    pub prefix: String,
    /// The one floating-point directive, rendered by the shared engine.
    pub directive: FloatDirective,
    /// The literal text after the directive (unescaped).
    pub suffix: String,
}

/// Validate `fmt` exactly as GNU `seq` does and split it into a
/// [`Format`].
///
/// # Errors
///
/// The four GNU diagnostics: no `%` directive, ends in `%`, an unknown
/// conversion, or more than one directive.
pub fn parse_format(fmt: &str) -> Result<Format, SeqError> {
    let bytes = fmt.as_bytes();
    let err = |make: fn(String) -> SeqError| Err(make(String::from(fmt)));

    // The literal prefix: everything before the first `%` not followed by
    // another `%`; `%%` collapses.
    let mut prefix = Vec::new();
    let mut i = 0;
    loop {
        match bytes.get(i) {
            None => return err(SeqError::FormatNoDirective),
            Some(b'%') if bytes.get(i + 1) != Some(&b'%') => break,
            Some(b'%') => {
                prefix.push(b'%');
                i += 2;
            }
            Some(&b) => {
                prefix.push(b);
                i += 1;
            }
        }
    }
    i += 1;

    let mut directive = FloatDirective::plain(FloatConversion::Fixed);
    while let Some(&b) = bytes.get(i) {
        match b {
            b'-' => directive.left = true,
            b'+' => directive.plus = true,
            b' ' => directive.space = true,
            b'#' => directive.alternate = true,
            b'0' => directive.zero = true,
            // `'` asks for locale grouping; the C locale groups nothing.
            b'\'' => {}
            _ => break,
        }
        i += 1;
    }
    let (width, next) = take_number(bytes, i);
    directive.width = width;
    i = next;
    if bytes.get(i) == Some(&b'.') {
        let (precision, next) = take_number(bytes, i + 1);
        directive.precision = Some(precision.unwrap_or(0));
        i = next;
    }

    // GNU accepts (and replaces) a caller-written `L` length modifier.
    if bytes.get(i) == Some(&b'L') {
        i += 1;
    }
    let Some(&conv) = bytes.get(i) else {
        return err(SeqError::FormatEndsInPercent);
    };
    directive.conversion = match conv {
        b'e' | b'E' => FloatConversion::Scientific,
        b'f' | b'F' => FloatConversion::Fixed,
        b'g' | b'G' => FloatConversion::Shortest,
        b'a' | b'A' => FloatConversion::Hex,
        other => {
            return Err(SeqError::FormatUnknownDirective(
                String::from(fmt),
                char::from(other),
            ))
        }
    };
    directive.uppercase = conv.is_ascii_uppercase();
    i += 1;

    // The literal suffix: `%%` collapses; a second directive is an error.
    let mut suffix = Vec::new();
    loop {
        match bytes.get(i) {
            None => break,
            Some(b'%') if bytes.get(i + 1) != Some(&b'%') => {
                return err(SeqError::FormatTooManyDirectives);
            }
            Some(b'%') => {
                suffix.push(b'%');
                i += 2;
            }
            Some(&b) => {
                suffix.push(b);
                i += 1;
            }
        }
    }
    Ok(Format {
        prefix: string_from_bytes(prefix),
        directive,
        suffix: string_from_bytes(suffix),
    })
}

/// Take a run of decimal digits starting at `i`, returning the parsed
/// value (saturating: an absurd width still validates, it just cannot be
/// satisfied) and the index after the run.
fn take_number(bytes: &[u8], mut i: usize) -> (Option<usize>, usize) {
    let start = i;
    let mut value: usize = 0;
    while let Some(&b) = bytes.get(i) {
        if !b.is_ascii_digit() {
            break;
        }
        value = value
            .saturating_mul(10)
            .saturating_add(usize::from(b - b'0'));
        i += 1;
    }
    (if i > start { Some(value) } else { None }, i)
}

/// Rebuild a `String` from bytes sliced out of a `&str` (always valid:
/// the slices split only at ASCII `%`).
fn string_from_bytes(bytes: Vec<u8>) -> String {
    String::from_utf8(bytes).unwrap_or_default()
}

impl Format {
    /// The `%.PRECf` default format (GNU `get_default_format`'s
    /// fixed-point arm).
    #[must_use]
    pub fn fixed(precision: usize) -> Self {
        Self::plain(FloatConversion::Fixed, Some(precision), None)
    }

    /// The `%0WIDTH.PRECf` default format (`-w`).
    #[must_use]
    pub fn fixed_padded(width: usize, precision: usize) -> Self {
        let mut format = Self::plain(FloatConversion::Fixed, Some(precision), Some(width));
        format.directive.zero = true;
        format
    }

    /// The `%g` default format.
    #[must_use]
    pub fn shortest() -> Self {
        Self::plain(FloatConversion::Shortest, None, None)
    }

    fn plain(conversion: FloatConversion, precision: Option<usize>, width: Option<usize>) -> Self {
        let mut directive = FloatDirective::plain(conversion);
        directive.width = width;
        directive.precision = precision;
        Self {
            prefix: String::new(),
            directive,
            suffix: String::new(),
        }
    }

    /// Render one value through this format, exactly as C's `printf`
    /// renders its one directive in the C locale.
    #[must_use]
    pub fn render(&self, x: f64) -> String {
        let mut out = String::from(&*self.prefix);
        self.directive.render_into(x, &mut out);
        out.push_str(&self.suffix);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_format, FloatConversion, Format};
    use crate::error::SeqError;
    use alloc::string::String;

    fn render(fmt: &str, x: f64) -> String {
        parse_format(fmt).expect("format validates").render(x)
    }

    #[test]
    fn validation_matches_gnu() {
        assert!(parse_format("%f").is_ok());
        assert!(parse_format("%'0-+# 12.4Le").is_ok());
        assert!(parse_format("a%%b%gc").is_ok());
        assert_eq!(
            parse_format("abc"),
            Err(SeqError::FormatNoDirective(String::from("abc")))
        );
        assert_eq!(
            parse_format("%%"),
            Err(SeqError::FormatNoDirective(String::from("%%")))
        );
        assert_eq!(
            parse_format("10%"),
            Err(SeqError::FormatEndsInPercent(String::from("10%")))
        );
        assert_eq!(
            parse_format("%d"),
            Err(SeqError::FormatUnknownDirective(String::from("%d"), 'd'))
        );
        assert_eq!(
            parse_format("%f %f"),
            Err(SeqError::FormatTooManyDirectives(String::from("%f %f")))
        );
    }

    #[test]
    fn validation_splits_the_directive() {
        let format = parse_format("x%%y%+07.2fz%%").expect("validates");
        assert_eq!(format.prefix, "x%y");
        assert_eq!(format.suffix, "z%");
        assert!(format.directive.plus && format.directive.zero && !format.directive.left);
        assert_eq!(format.directive.width, Some(7));
        assert_eq!(format.directive.precision, Some(2));
        assert_eq!(format.directive.conversion, FloatConversion::Fixed);
        // An empty precision is zero, as in C.
        assert_eq!(
            parse_format("%.f").expect("validates").directive.precision,
            Some(0)
        );
    }

    #[test]
    fn rendering_goes_through_the_shared_engine() {
        // One spot check per conversion; the C-semantics coverage lives
        // with the shared engine in `lib/util`'s `cfloat` tests.
        assert_eq!(render("%.2f", 2.5), "2.50");
        assert_eq!(render("%.2e", 12345.0), "1.23e+04");
        assert_eq!(render("%g", 0.00001), "1e-05");
        assert_eq!(render("%a", 3.0), "0x1.8p+1");
        assert_eq!(render("%08.2f", -1.5), "-0001.50");
    }

    #[test]
    fn literals_wrap_the_number() {
        assert_eq!(render("a%%b%.0fc%%", 5.0), "a%b5c%");
        assert_eq!(render("--%.1f--", 1.5), "--1.5--");
    }

    #[test]
    fn default_format_constructors_render() {
        assert_eq!(Format::fixed(2).render(0.5), "0.50");
        assert_eq!(Format::fixed_padded(5, 1).render(-0.5), "-00.5");
        assert_eq!(Format::shortest().render(10_000_000.0), "1e+07");
        assert_eq!(Format::shortest().render(2.5), "2.5");
    }
}
