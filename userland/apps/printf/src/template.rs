//! The FORMAT template engine: backslash escapes and `%` directives.
//!
//! [`render_pass`] walks the FORMAT once, appending output bytes and
//! consuming arguments, exactly as one pass of GNU `printf`'s
//! `print_formatted` does; the client repeats it while it consumes
//! arguments and any remain. Escapes are shared between the FORMAT and a
//! `%b` argument (the latter reads octal as `\0NNN`, per POSIX). `\c`
//! halts all output.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use tairix_util::cfloat::{FloatConversion, FloatDirective};

use crate::error::PrintfError;
use crate::number::{to_float, to_signed, to_unsigned, Note};
use crate::quote::shell_quote;

/// How serious a conversion diagnostic is: an error sets the run's exit
/// status to `1`; a warning leaves it untouched (both GNU behaviours).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Severity {
    /// Sets the exit status to `1`; the run continues.
    Error,
    /// Reported only; the exit status is untouched.
    Warning,
}

/// One diagnostic a pass wants reported on standard error, already in
/// GNU's wording (without the `printf: ` program prefix).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Diagnostic {
    /// Whether the exit status becomes `1`.
    pub severity: Severity,
    /// The message text after the program prefix.
    pub message: String,
}

/// The outcome of one pass over the FORMAT.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PassResult {
    /// How many arguments the pass consumed.
    pub consumed: usize,
    /// `true` when a `\c` ended all output.
    pub halted: bool,
}

/// Walk `format` once — literals, escapes, directives — appending output
/// bytes to `out`, consuming `arguments` from the front, and collecting
/// conversion diagnostics. The client repeats the pass over the
/// remaining arguments while a pass consumes any and some remain, as GNU
/// `printf` reuses its format.
///
/// # Errors
///
/// The fatal [`PrintfError`]s: an invalid conversion specification or a
/// malformed escape. Output already appended to `out` stands (the client
/// writes it before reporting, exactly as GNU's exit flushes its stream).
pub fn render_pass(
    format: &str,
    arguments: &[&str],
    out: &mut Vec<u8>,
    diagnostics: &mut Vec<Diagnostic>,
) -> Result<PassResult, PrintfError> {
    let bytes = format.as_bytes();
    let mut i = 0;
    let mut cursor = Cursor {
        arguments,
        next: 0,
        diagnostics,
    };
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => {
                let (next, halt) = push_escape(bytes, i + 1, OctalStyle::Format, out)?;
                if halt {
                    return Ok(PassResult {
                        consumed: cursor.next,
                        halted: true,
                    });
                }
                i = next;
            }
            b'%' if bytes.get(i + 1) == Some(&b'%') => {
                out.push(b'%');
                i += 2;
            }
            b'%' => {
                let (next, halt) = render_directive(format, i, &mut cursor, out)?;
                if halt {
                    return Ok(PassResult {
                        consumed: cursor.next,
                        halted: true,
                    });
                }
                i = next;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    Ok(PassResult {
        consumed: cursor.next,
        halted: false,
    })
}

/// The argument cursor a pass consumes from, reporting each converted
/// argument's diagnostic as it goes.
struct Cursor<'a, 'd> {
    arguments: &'a [&'a str],
    next: usize,
    diagnostics: &'d mut Vec<Diagnostic>,
}

impl<'a> Cursor<'a, '_> {
    /// The next argument, or the empty string once they are exhausted
    /// (an exhausted take does not count as consumption, so a format
    /// with no arguments still terminates the reuse loop).
    fn take(&mut self) -> &'a str {
        match self.arguments.get(self.next) {
            Some(arg) => {
                self.next += 1;
                arg
            }
            None => "",
        }
    }

    /// Report a conversion note against `arg` in GNU's wording.
    fn report(&mut self, arg: &str, note: Note) {
        let (severity, message) = match note {
            Note::ExpectedNumeric => (
                Severity::Error,
                format!("'{arg}': expected a numeric value"),
            ),
            Note::NotCompletelyConverted => (
                Severity::Error,
                format!("'{arg}': value not completely converted"),
            ),
            Note::OutOfRange => (
                Severity::Error,
                format!("'{arg}': Numerical result out of range"),
            ),
            Note::TrailingCharacters(tail) => (
                Severity::Warning,
                format!(
                    "warning: {tail}: character(s) following character constant have been ignored"
                ),
            ),
        };
        self.diagnostics.push(Diagnostic { severity, message });
    }
}

/// The parsed prefix of one `%` directive: flags, width, precision.
#[derive(Default)]
// The five booleans are C's five independent printf flags; the directive
// grammar defines them as free combinations, so an enum would misstate it.
#[allow(clippy::struct_excessive_bools)]
struct Directive {
    left: bool,
    plus: bool,
    space: bool,
    alternate: bool,
    zero: bool,
    grouping: bool,
    saw_width: bool,
    width: usize,
    saw_precision: bool,
    precision: Option<usize>,
}

/// Render the directive starting at `format[start]` (the `%`), returning
/// the index after its conversion letter and whether output halted.
fn render_directive(
    format: &str,
    start: usize,
    cursor: &mut Cursor<'_, '_>,
    out: &mut Vec<u8>,
) -> Result<(usize, bool), PrintfError> {
    let bytes = format.as_bytes();
    let (d, i) = parse_spec(bytes, start + 1, cursor);
    let invalid = |end: usize| {
        Err(PrintfError::InvalidConversion(String::from(
            &format[start..end],
        )))
    };
    let Some(&conversion) = bytes.get(i) else {
        return invalid(i);
    };
    let end = i + 1;
    if !flags_permitted(&d, conversion) {
        return invalid(end);
    }
    let halt = match conversion {
        b'd' | b'i' | b'u' | b'o' | b'x' | b'X' => {
            render_integer(conversion, cursor, &d, out);
            false
        }
        b'e' | b'E' | b'f' | b'F' | b'g' | b'G' | b'a' | b'A' => {
            render_float(conversion, cursor, &d, out);
            false
        }
        b'c' => {
            // C's `%c` of the argument's first byte; an empty (or
            // missing) argument prints the NUL terminator, as GNU does.
            let byte = cursor.take().as_bytes().first().copied().unwrap_or(0);
            push_padded(out, &[byte], &d);
            false
        }
        b's' => {
            let arg = cursor.take().as_bytes();
            // A byte truncation, exactly as C performs it: the output
            // stream carries bytes, not text.
            let content = match d.precision {
                Some(p) => &arg[..p.min(arg.len())],
                None => arg,
            };
            push_padded(out, content, &d);
            false
        }
        b'b' => push_b_argument(cursor.take(), out)?,
        b'q' => {
            out.extend_from_slice(shell_quote(cursor.take()).as_bytes());
            false
        }
        // Only an immediate `%%` is the literal (`%5%` is invalid), and
        // any unknown letter is invalid.
        _ => return invalid(end),
    };
    Ok((end, halt))
}

/// Parse the directive's flags, width, and precision starting at
/// `bytes[i]` (just past the `%`), consuming `*` width/precision
/// arguments from the cursor. Returns the parsed prefix and the index of
/// the conversion letter.
fn parse_spec(bytes: &[u8], mut i: usize, cursor: &mut Cursor<'_, '_>) -> (Directive, usize) {
    let mut d = Directive::default();
    loop {
        match bytes.get(i) {
            Some(b'-') => d.left = true,
            Some(b'+') => d.plus = true,
            Some(b' ') => d.space = true,
            Some(b'#') => d.alternate = true,
            Some(b'0') => d.zero = true,
            Some(b'\'') => d.grouping = true,
            _ => break,
        }
        i += 1;
    }
    if bytes.get(i) == Some(&b'*') {
        i += 1;
        d.saw_width = true;
        let arg = cursor.take();
        let converted = to_signed(arg);
        if let Some(note) = converted.note {
            cursor.report(arg, note);
        }
        if converted.value < 0 {
            d.left = true;
        }
        d.width = usize::try_from(converted.value.unsigned_abs()).unwrap_or(usize::MAX);
    } else {
        while let Some(&b) = bytes.get(i) {
            if !b.is_ascii_digit() {
                break;
            }
            d.saw_width = true;
            d.width = d
                .width
                .saturating_mul(10)
                .saturating_add(usize::from(b - b'0'));
            i += 1;
        }
    }
    if bytes.get(i) == Some(&b'.') {
        i += 1;
        d.saw_precision = true;
        if bytes.get(i) == Some(&b'*') {
            i += 1;
            let arg = cursor.take();
            let converted = to_signed(arg);
            if let Some(note) = converted.note {
                cursor.report(arg, note);
            }
            // A negative `*` precision acts as no precision, as in C.
            d.precision = usize::try_from(converted.value).ok();
        } else {
            let mut precision = 0_usize;
            while let Some(&b) = bytes.get(i) {
                if !b.is_ascii_digit() {
                    break;
                }
                precision = precision
                    .saturating_mul(10)
                    .saturating_add(usize::from(b - b'0'));
                i += 1;
            }
            d.precision = Some(precision);
        }
    }
    (d, i)
}

/// Convert and render one integer conversion (`diouxX`).
fn render_integer(conversion: u8, cursor: &mut Cursor<'_, '_>, d: &Directive, out: &mut Vec<u8>) {
    let arg = cursor.take();
    let value = if matches!(conversion, b'd' | b'i') {
        let converted = to_signed(arg);
        if let Some(note) = converted.note {
            cursor.report(arg, note);
        }
        Int {
            negative: converted.value < 0,
            magnitude: converted.value.unsigned_abs(),
            base: 10,
            uppercase: false,
            signed: true,
        }
    } else {
        let converted = to_unsigned(arg);
        if let Some(note) = converted.note {
            cursor.report(arg, note);
        }
        Int {
            negative: false,
            magnitude: converted.value,
            base: match conversion {
                b'o' => 8,
                b'u' => 10,
                _ => 16,
            },
            uppercase: conversion == b'X',
            signed: false,
        }
    };
    push_integer(out, d, value);
}

/// Convert and render one floating-point conversion (`eEfFgGaA`).
fn render_float(conversion: u8, cursor: &mut Cursor<'_, '_>, d: &Directive, out: &mut Vec<u8>) {
    let arg = cursor.take();
    let converted = to_float(arg);
    if let Some(note) = converted.note {
        cursor.report(arg, note);
    }
    let directive = FloatDirective {
        left: d.left,
        plus: d.plus,
        space: d.space,
        alternate: d.alternate,
        zero: d.zero,
        width: d.saw_width.then_some(d.width),
        precision: d.precision,
        conversion: match conversion.to_ascii_lowercase() {
            b'e' => FloatConversion::Scientific,
            b'f' => FloatConversion::Fixed,
            b'g' => FloatConversion::Shortest,
            _ => FloatConversion::Hex,
        },
        uppercase: conversion.is_ascii_uppercase(),
    };
    out.extend_from_slice(directive.render(converted.value).as_bytes());
}

/// The probe-pinned GNU flag/precision validity table: `%b`/`%q` take no
/// flags, width, or precision; `#` is invalid for `d`/`i`/`u`/`c`/`s`;
/// `0` for `c`/`s`; `'` for `c`/`s`/`a`/`A`; a precision for `c`.
fn flags_permitted(d: &Directive, conversion: u8) -> bool {
    match conversion {
        b'b' | b'q' => {
            !(d.left
                || d.plus
                || d.space
                || d.alternate
                || d.zero
                || d.grouping
                || d.saw_width
                || d.saw_precision)
        }
        b'd' | b'i' | b'u' => !d.alternate,
        b'c' => !(d.alternate || d.zero || d.grouping || d.saw_precision),
        b's' => !(d.alternate || d.zero || d.grouping),
        b'a' | b'A' => !d.grouping,
        b'o' | b'x' | b'X' | b'e' | b'E' | b'f' | b'F' | b'g' | b'G' => true,
        _ => false,
    }
}

/// One integer value ready to render: its sign, magnitude, base, digit
/// case, and whether the conversion is signed (`+`/space print a sign
/// only for `%d`/`%i`; GNU ignores them for the unsigned conversions).
#[derive(Clone, Copy)]
struct Int {
    negative: bool,
    magnitude: u64,
    base: u64,
    uppercase: bool,
    signed: bool,
}

/// Append one rendered integer: sign, alternate-form prefix, zero or
/// space padding, and the digits, per C's rules (a precision defeats the
/// `0` flag; `-` defeats it too).
fn push_integer(out: &mut Vec<u8>, d: &Directive, value: Int) {
    let mut digits = [0_u8; 64];
    let mut len = 0_usize;
    let mut rest = value.magnitude;
    while rest > 0 {
        // A base-8/10/16 digit is below 16: the cast is lossless.
        #[allow(clippy::cast_possible_truncation)]
        let digit = (rest % value.base) as u8;
        digits[63 - len] = match digit {
            0..=9 => b'0' + digit,
            hex if value.uppercase => b'A' + (hex - 10),
            hex => b'a' + (hex - 10),
        };
        len += 1;
        rest /= value.base;
    }
    let mut body = Vec::with_capacity(24);
    // The precision is the minimum digit count; zero with precision
    // zero prints no digits at all.
    let min_digits = d.precision.unwrap_or(1);
    let zeros = min_digits.saturating_sub(len);
    let mut prefix: &[u8] = b"";
    if d.alternate {
        match value.base {
            // Alternate octal guarantees a leading zero digit.
            8 if zeros == 0 && (len == 0 || digits[64 - len] != b'0') => prefix = b"0",
            16 if value.magnitude != 0 => prefix = if value.uppercase { b"0X" } else { b"0x" },
            _ => {}
        }
    }
    let sign: &[u8] = if value.negative {
        b"-"
    } else if d.plus && value.signed {
        b"+"
    } else if d.space && value.signed {
        b" "
    } else {
        b""
    };
    body.extend_from_slice(prefix);
    body.resize(body.len() + zeros, b'0');
    body.extend_from_slice(&digits[64 - len..]);

    let printed = sign.len() + body.len();
    let width = if d.saw_width { d.width } else { 0 };
    let padding = width.saturating_sub(printed);
    if d.left {
        out.extend_from_slice(sign);
        out.extend_from_slice(&body);
        out.extend(core::iter::repeat(b' ').take(padding));
    } else if d.zero && d.precision.is_none() {
        out.extend_from_slice(sign);
        out.extend(core::iter::repeat(b'0').take(padding));
        out.extend_from_slice(&body);
    } else {
        out.extend(core::iter::repeat(b' ').take(padding));
        out.extend_from_slice(sign);
        out.extend_from_slice(&body);
    }
}

/// Append `content` space-padded to the directive's width (`%c`/`%s`).
fn push_padded(out: &mut Vec<u8>, content: &[u8], d: &Directive) {
    let width = if d.saw_width { d.width } else { 0 };
    let padding = width.saturating_sub(content.len());
    if d.left {
        out.extend_from_slice(content);
        out.extend(core::iter::repeat(b' ').take(padding));
    } else {
        out.extend(core::iter::repeat(b' ').take(padding));
        out.extend_from_slice(content);
    }
}

/// How octal escapes are read: the FORMAT takes `\NNN`; a `%b` argument
/// takes `\0NNN` (and GNU also accepts `\NNN` there).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OctalStyle {
    Format,
    BArgument,
}

/// Append the escape starting at `bytes[i]` (the byte after the `\`),
/// returning the next index and whether `\c` halted output.
///
/// # Errors
///
/// [`PrintfError::MissingHexEscape`] for `\x`/`\u`/`\U` with too few
/// digits, and [`PrintfError::InvalidUniversal`] for a surrogate or
/// out-of-range `\u`/`\U` code point — both fatal, as in GNU.
fn push_escape(
    bytes: &[u8],
    i: usize,
    style: OctalStyle,
    out: &mut Vec<u8>,
) -> Result<(usize, bool), PrintfError> {
    let Some(&b) = bytes.get(i) else {
        // A trailing lone backslash passes through, as in GNU.
        out.push(b'\\');
        return Ok((i, false));
    };
    match b {
        b'a' => out.push(0x07),
        b'b' => out.push(0x08),
        b'c' => return Ok((i + 1, true)),
        b'e' => out.push(0x1B),
        b'f' => out.push(0x0C),
        b'n' => out.push(b'\n'),
        b'r' => out.push(b'\r'),
        b't' => out.push(b'\t'),
        b'v' => out.push(0x0B),
        b'\\' => out.push(b'\\'),
        b'"' => out.push(b'"'),
        b'x' => {
            let (value, len) = hex_digits(bytes, i + 1, 2);
            if len == 0 {
                return Err(PrintfError::MissingHexEscape);
            }
            // At most two hex digits: always a byte.
            #[allow(clippy::cast_possible_truncation)]
            out.push(value as u8);
            return Ok((i + 1 + len, false));
        }
        b'u' | b'U' => return push_universal(bytes, i, out),
        b'0'..=b'7' if style == OctalStyle::Format => {
            let (value, len) = octal_digits(bytes, i, 3);
            // At most three octal digits: 0..=0o777, truncated to a byte
            // exactly as C's printf truncates it.
            #[allow(clippy::cast_possible_truncation)]
            out.push(value as u8);
            return Ok((i + len, false));
        }
        b'0'..=b'7' if style == OctalStyle::BArgument => {
            // POSIX %b octal is \0NNN; GNU also reads \NNN.
            let (start, max) = if b == b'0' { (i + 1, 3) } else { (i, 3) };
            let (value, len) = octal_digits(bytes, start, max);
            #[allow(clippy::cast_possible_truncation)]
            out.push(value as u8);
            return Ok((start + len, false));
        }
        other => {
            // An unrecognised escape passes through backslash and all.
            out.push(b'\\');
            out.push(other);
        }
    }
    Ok((i + 1, false))
}

/// Read up to `max` octal digits at `bytes[start..]`, returning the value
/// and the digit count (possibly zero).
fn octal_digits(bytes: &[u8], start: usize, max: usize) -> (u32, usize) {
    let mut value = 0_u32;
    let mut len = 0;
    while len < max {
        match bytes.get(start + len) {
            Some(&b @ b'0'..=b'7') => {
                value = value * 8 + u32::from(b - b'0');
                len += 1;
            }
            _ => break,
        }
    }
    (value, len)
}

/// Read up to `max` hex digits at `bytes[start..]`, returning the value
/// and the digit count (possibly zero).
fn hex_digits(bytes: &[u8], start: usize, max: usize) -> (u32, usize) {
    let mut value = 0_u32;
    let mut len = 0;
    while len < max {
        match bytes.get(start + len) {
            Some(&b) if b.is_ascii_hexdigit() => {
                value = value * 16 + hex_value(b);
                len += 1;
            }
            _ => break,
        }
    }
    (value, len)
}

/// The value of one hexadecimal digit byte.
fn hex_value(b: u8) -> u32 {
    match b {
        b'0'..=b'9' => u32::from(b - b'0'),
        b'a'..=b'f' => u32::from(b - b'a' + 10),
        _ => u32::from(b - b'A' + 10),
    }
}

/// `\uHHHH` / `\UHHHHHHHH`: exactly 4 or 8 hex digits naming a Unicode
/// scalar, emitted as UTF-8. `bytes[i]` is the `u`/`U`.
fn push_universal(bytes: &[u8], i: usize, out: &mut Vec<u8>) -> Result<(usize, bool), PrintfError> {
    let want = if bytes[i] == b'u' { 4 } else { 8 };
    let (value, len) = hex_digits(bytes, i + 1, want);
    if len < want {
        return Err(PrintfError::MissingHexEscape);
    }
    let Some(c) = char::from_u32(value) else {
        // A surrogate or out-of-range code point, spelled as written.
        let mut escape = String::from("\\");
        for &b in &bytes[i..i + 1 + want] {
            escape.push(char::from(b.to_ascii_lowercase()));
        }
        return Err(PrintfError::InvalidUniversal(escape));
    };
    let mut buf = [0_u8; 4];
    out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
    Ok((i + 1 + want, false))
}

/// Append `text` with its escapes interpreted as a `%b` argument's,
/// returning `true` when `\c` halted output.
fn push_b_argument(text: &str, out: &mut Vec<u8>) -> Result<bool, PrintfError> {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' {
            let (next, halt) = push_escape(bytes, i + 1, OctalStyle::BArgument, out)?;
            if halt {
                return Ok(true);
            }
            i = next;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::{render_pass, Diagnostic, PassResult, Severity};
    use crate::error::PrintfError;
    use alloc::string::String;
    use alloc::vec::Vec;

    /// One pass, returning its output, diagnostics, and result. Every
    /// expectation in this module is the observed behaviour of GNU
    /// coreutils `printf` for the same format and arguments.
    fn pass(format: &str, args: &[&str]) -> (String, Vec<Diagnostic>, PassResult) {
        let mut out = Vec::new();
        let mut diagnostics = Vec::new();
        let result = render_pass(format, args, &mut out, &mut diagnostics).expect("pass renders");
        (
            String::from_utf8(out).expect("test outputs are UTF-8"),
            diagnostics,
            result,
        )
    }

    fn output(format: &str, args: &[&str]) -> String {
        pass(format, args).0
    }

    fn fatal(format: &str, args: &[&str]) -> PrintfError {
        let mut out = Vec::new();
        let mut diagnostics = Vec::new();
        render_pass(format, args, &mut out, &mut diagnostics).expect_err("pass fails")
    }

    #[test]
    fn literals_and_escapes_render() {
        assert_eq!(output("hi", &[]), "hi");
        assert_eq!(output("a\\tb\\n", &[]), "a\tb\n");
        assert_eq!(output("\\x41\\102", &[]), "AB");
        assert_eq!(output("\\u0024ok", &[]), "$ok");
        assert_eq!(output("\\u0041", &[]), "A");
        assert_eq!(output("\\z", &[]), "\\z", "unknown escapes pass through");
        assert_eq!(output("\\'", &[]), "\\'");
        assert_eq!(output("\\\"", &[]), "\"");
        assert_eq!(output("%%", &[]), "%");
    }

    #[test]
    fn hex_escapes_take_one_or_two_digits() {
        // `\xfff` is the byte 0xFF then a literal `f` — not valid UTF-8,
        // so this pin checks the raw bytes.
        let mut out = Vec::new();
        let mut diagnostics = Vec::new();
        let result = render_pass("\\xfff", &[], &mut out, &mut diagnostics).expect("pass renders");
        assert_eq!(out, [0xFF, b'f']);
        assert!(!result.halted);
    }

    #[test]
    fn slash_c_halts_the_pass() {
        let (text, _, result) = pass("a\\cb", &[]);
        assert_eq!(text, "a");
        assert!(result.halted);
    }

    #[test]
    fn integer_conversions_match_gnu() {
        assert_eq!(output("%d|", &["12"]), "12|");
        assert_eq!(output("%d|", &["-0x10"]), "-16|");
        assert_eq!(output("%i|", &["010"]), "8|");
        assert_eq!(output("%u|", &["-1"]), "18446744073709551615|");
        assert_eq!(output("%x|", &["255"]), "ff|");
        assert_eq!(output("%X|", &["255"]), "FF|");
        assert_eq!(output("%o|", &["8"]), "10|");
        assert_eq!(output("%-5d|", &["42"]), "42   |");
        assert_eq!(output("%05d|", &["-42"]), "-0042|");
        assert_eq!(
            output("%05.3d|", &["42"]),
            "  042|",
            "precision defeats zero"
        );
        assert_eq!(
            output("%#o %#x %#X|", &["8", "255", "255"]),
            "010 0xff 0XFF|"
        );
        assert_eq!(output("%#o|", &["0"]), "0|");
        assert_eq!(output("%#.5o|", &["8"]), "00010|");
        assert_eq!(output("%#x|", &["0"]), "0|");
        assert_eq!(output("%#.0x|", &["0"]), "|");
        assert_eq!(output("%+d % d|", &["5", "5"]), "+5  5|");
        assert_eq!(
            output("%+u % x|", &["5", "5"]),
            "5 5|",
            "sign flags are signed-only"
        );
        assert_eq!(output("%.0d|", &["0"]), "|");
        assert_eq!(
            output("%'d|", &["5000"]),
            "5000|",
            "C locale groups nothing"
        );
        assert_eq!(
            output("%d|", &["9223372036854775807"]),
            "9223372036854775807|"
        );
    }

    #[test]
    fn float_conversions_match_gnu() {
        assert_eq!(output("%f|", &["1"]), "1.000000|");
        assert_eq!(output("%.2e|", &["12345"]), "1.23e+04|");
        assert_eq!(output("%g|", &["0.00001"]), "1e-05|");
        assert_eq!(output("%a|", &["3"]), "0x1.8p+1|");
        assert_eq!(output("%F|", &["inf"]), "INF|");
        assert_eq!(output("%f|", &["0x1p-1"]), "0.500000|");
        assert_eq!(
            output("%.*f|", &["-1", "1.25"]),
            "1.250000|",
            "negative * precision is none"
        );
    }

    #[test]
    fn character_and_string_conversions_match_gnu() {
        assert_eq!(output("%c|", &["abc"]), "a|");
        assert_eq!(output("%c|", &[""]), "\0|", "an empty %c prints NUL");
        assert_eq!(output("%c|", &[]), "\0|");
        assert_eq!(output("%5c|", &["x"]), "    x|");
        assert_eq!(output("%.2s|", &["abcdef"]), "ab|");
        assert_eq!(output("%5.2s|", &["abcdef"]), "   ab|");
        assert_eq!(output("%-5s|", &["ab"]), "ab   |");
        assert_eq!(output("%s|", &[]), "|", "a missing %s is empty");
        assert_eq!(output("%08d|", &["1"]), "00000001|");
    }

    #[test]
    fn b_and_q_conversions_match_gnu() {
        assert_eq!(output("%b|", &["\\0101"]), "A|");
        assert_eq!(output("%b|", &["\\101"]), "A|");
        assert_eq!(output("%b|", &["\\x41"]), "A|");
        let (text, _, result) = pass("%b|", &["x\\c y"]);
        assert_eq!(text, "x");
        assert!(result.halted, "\\c in a %b argument halts everything");
        assert_eq!(output("%q|", &["a b"]), "'a b'|");
        assert_eq!(output("%q|", &[""]), "''|");
    }

    #[test]
    fn star_width_reads_an_argument() {
        assert_eq!(output("%*d|", &["6", "42"]), "    42|");
        assert_eq!(
            output("%*d|", &["-6", "42"]),
            "42    |",
            "negative * width left-justifies"
        );
    }

    #[test]
    fn conversion_diagnostics_match_gnu() {
        let (text, diagnostics, _) = pass("%d", &["12abc"]);
        assert_eq!(text, "12");
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].severity, Severity::Error);
        assert_eq!(
            diagnostics[0].message,
            "'12abc': value not completely converted"
        );

        let (text, diagnostics, _) = pass("%d", &["abc"]);
        assert_eq!(text, "0");
        assert_eq!(diagnostics[0].message, "'abc': expected a numeric value");

        let (text, diagnostics, _) = pass("%d", &["99999999999999999999999"]);
        assert_eq!(text, "9223372036854775807");
        assert_eq!(
            diagnostics[0].message,
            "'99999999999999999999999': Numerical result out of range"
        );

        let (text, diagnostics, _) = pass("%d", &["'ABC"]);
        assert_eq!(text, "65");
        assert_eq!(diagnostics[0].severity, Severity::Warning);
        assert_eq!(
            diagnostics[0].message,
            "warning: BC: character(s) following character constant have been ignored"
        );

        let (text, diagnostics, _) = pass("%d", &[""]);
        assert_eq!(text, "0");
        assert!(diagnostics.is_empty(), "an empty argument is silent");
    }

    #[test]
    fn invalid_specifications_are_fatal() {
        assert_eq!(
            fatal("%", &[]),
            PrintfError::InvalidConversion(String::from("%"))
        );
        assert_eq!(
            fatal("%r", &[]),
            PrintfError::InvalidConversion(String::from("%r"))
        );
        assert_eq!(
            fatal("%5%", &[]),
            PrintfError::InvalidConversion(String::from("%5%"))
        );
        assert_eq!(
            fatal("%10b", &["x"]),
            PrintfError::InvalidConversion(String::from("%10b"))
        );
        assert_eq!(
            fatal("%.1q", &["x"]),
            PrintfError::InvalidConversion(String::from("%.1q"))
        );
        assert_eq!(
            fatal("%#d", &["5"]),
            PrintfError::InvalidConversion(String::from("%#d"))
        );
        assert_eq!(
            fatal("%08s", &["a"]),
            PrintfError::InvalidConversion(String::from("%08s"))
        );
        assert_eq!(
            fatal("%.2c", &["a"]),
            PrintfError::InvalidConversion(String::from("%.2c"))
        );
        assert_eq!(
            fatal("%'a", &["1"]),
            PrintfError::InvalidConversion(String::from("%'a"))
        );
    }

    #[test]
    fn malformed_escapes_are_fatal() {
        assert_eq!(fatal("\\x", &[]), PrintfError::MissingHexEscape);
        assert_eq!(fatal("\\u041", &[]), PrintfError::MissingHexEscape);
        assert_eq!(
            fatal("\\uD800", &[]),
            PrintfError::InvalidUniversal(String::from("\\ud800"))
        );
    }

    #[test]
    fn output_before_a_fatal_error_is_kept() {
        let mut out = Vec::new();
        let mut diagnostics = Vec::new();
        let err = render_pass("a%r", &[], &mut out, &mut diagnostics).expect_err("fails");
        assert_eq!(err, PrintfError::InvalidConversion(String::from("%r")));
        assert_eq!(out, b"a", "GNU prints the literal before the error");
    }

    #[test]
    fn consumption_counts_only_real_arguments() {
        let (_, _, result) = pass("%s %s", &["a", "b", "c"]);
        assert_eq!(result.consumed, 2);
        let (_, _, result) = pass("%s %s", &["a"]);
        assert_eq!(result.consumed, 1, "an exhausted take is not consumption");
        let (_, _, result) = pass("plain", &["a"]);
        assert_eq!(result.consumed, 0);
    }
}
