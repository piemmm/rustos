//! The sequence engine: generate and write the numbers a [`Job`] names.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use tairix_help::{own_short_help, HelpSource};

use crate::command::{Command, Job};
use crate::error::SeqError;
use crate::format::Format;
use crate::number::{parse_number, scan_arg, Operand, NOT_FIXED_POINT};

/// The usage banner a usage error is reported with, and the fallback the
/// short-help switches print when `seq`'s own Help tree is unavailable.
pub const USAGE: &str = "usage: seq [-f format] [-s string] [-w] [first [increment]] last";

/// `seq`'s own command word: the short-help switches render its own Help
/// document through the same engine as any other command's.
const OWN_WORD: &str = "seq";

/// The output sink the sequence is pumped into. A failed write is fatal.
pub trait Output {
    /// Write all of `bytes`, or report that the sink no longer accepts
    /// output.
    ///
    /// # Errors
    ///
    /// [`SeqError::Output`] when the sink stopped accepting bytes.
    fn write_all(&self, bytes: &[u8]) -> Result<(), SeqError>;
}

/// Flush threshold for the output buffer: one write call delivers many
/// numbers instead of one, without unbounded buffering.
const FLUSH_LEN: usize = 8192;

/// Run one [`Command`], writing the sequence to `out`. `locale` is the
/// user's `LANG` preference, if set; `help` is the tool's own `Help/`
/// tree, read by the short-help switches.
///
/// # Errors
///
/// [`SeqError::Output`] when a write failed, and the number-scanning
/// usage errors ([`SeqError::InvalidNumber`], [`SeqError::NotANumber`],
/// [`SeqError::ZeroIncrement`]) the operand scan raises.
pub fn run(
    command: Command,
    locale: Option<&str>,
    help: &dyn HelpSource,
    out: &dyn Output,
) -> Result<(), SeqError> {
    let job = match command {
        Command::Help => {
            let bytes = own_short_help(help, locale, OWN_WORD)
                .unwrap_or_else(|| format!("{USAGE}\n").into_bytes());
            return out.write_all(&bytes);
        }
        Command::Print(job) => job,
    };
    print_job(&job, out)
}

/// The largest integer increment the exact decimal path takes (the GNU
/// `SEQ_FAST_STEP_LIMIT`, chosen there by measurement).
const FAST_STEP_LIMIT: f64 = 200.0;

/// True when `s` consists of at least one digit and nothing else.
fn all_digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

/// Scan the operands and print the sequence, choosing the exact decimal
/// path where GNU does and the floating-point path otherwise.
fn print_job(job: &Job, out: &dyn Output) -> Result<(), SeqError> {
    let n = job.operands.len();
    let plain = !job.equal_width && job.format.is_none() && job.separator.len() == 1;
    let separator = job.separator.as_bytes().first().copied().unwrap_or(b'\n');
    let user_start = if n == 1 { "1" } else { &job.operands[0] };

    // First exact attempt: every operand is already a plain digit string
    // (and a third operand is a usable integer step).
    let fast_step = if n == 3 {
        let text = &job.operands[1];
        if all_digits(text) {
            // The filter proves 0 < v <= 200 and a digits-only spelling
            // proves integrality, so the conversion is exact.
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            parse_number(text)
                .filter(|&v| 0.0 < v && v <= FAST_STEP_LIMIT)
                .map(|v| v as u64)
        } else {
            None
        }
    } else {
        Some(1)
    };
    if all_digits(&job.operands[0])
        && (n == 1 || all_digits(&job.operands[n - 1]))
        && (n < 3 || (fast_step.is_some() && all_digits(&job.operands[2])))
        && plain
    {
        if let Some(step) = fast_step {
            return seq_fast(user_start, &job.operands[n - 1], step, separator, out);
        }
    }

    // The general scan, in GNU's operand order (left to right, with the
    // zero-increment check before the third operand is scanned).
    let mut first = Operand::ONE;
    let mut step = Operand::ONE;
    let mut last = scan_arg(&job.operands[0])?;
    if n > 1 {
        first = last;
        last = scan_arg(&job.operands[1])?;
        if n > 2 {
            step = last;
            if step.value == 0.0 {
                return Err(SeqError::ZeroIncrement(job.operands[1].clone()));
            }
            last = scan_arg(&job.operands[2])?;
        }
    }

    // Second exact attempt, for integers spelled another way (`1e1`,
    // `0x14`) or an `inf` end value.
    if first.precision == 0
        && step.precision == 0
        && last.precision == 0
        && first.value.is_finite()
        && 0.0 <= first.value
        && 0.0 <= last.value
        && 0.0 < step.value
        && step.value <= FAST_STEP_LIMIT
        && plain
    {
        let s1 = if all_digits(user_start) {
            String::from(user_start)
        } else {
            Format::fixed(0).render(first.value)
        };
        let s2 = if last.value.is_finite() {
            Format::fixed(0).render(last.value)
        } else {
            String::from("inf")
        };
        if !s1.starts_with('-') && !s2.starts_with('-') {
            // A precision-0 operand is integral and the guard proves
            // 0 < step <= 200, so the conversion is exact.
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            return seq_fast(&s1, &s2, step.value as u64, separator, out);
        }
    }

    let format = match &job.format {
        Some(format) => format.clone(),
        None => default_format(first, step, last, job.equal_width),
    };
    print_numbers(
        &format,
        job.separator.as_bytes(),
        first.value,
        step.value,
        last.value,
        out,
    )
}

/// The default format (GNU `get_default_format`): fixed point at the
/// operands' precision when every operand is fixed point — zero-padded to
/// a common width under `-w` — and `%g` otherwise.
fn default_format(first: Operand, step: Operand, last: Operand, equal_width: bool) -> Format {
    let prec = first.precision.max(step.precision);
    if prec != NOT_FIXED_POINT && last.precision != NOT_FIXED_POINT {
        let prec_usize = usize::try_from(prec).unwrap_or(0);
        if equal_width {
            // Adjust each operand's spelled width to the shared precision.
            let mut first_width = first.width + (prec - first.precision);
            let mut last_width = last.width + (prec - last.precision);
            if last.precision != 0 && prec == 0 {
                last_width -= 1; // no space for the '.'
            }
            if last.precision == 0 && prec != 0 {
                last_width += 1; // space for the '.'
            }
            if first.precision == 0 && prec != 0 {
                first_width += 1; // space for the '.'
            }
            let width = first_width.max(last_width);
            if (0..=i64::from(i32::MAX)).contains(&width) {
                return Format::fixed_padded(usize::try_from(width).unwrap_or(0), prec_usize);
            }
        } else {
            return Format::fixed(prec_usize);
        }
    }
    Format::shortest()
}

/// Print all whole numbers from `a` to `b` inclusive (`b` may be `inf`),
/// stepping by `step`, in exact decimal string arithmetic — no float can
/// misrepresent a large integer here (GNU `seq_fast`).
fn seq_fast(a: &str, b: &str, step: u64, separator: u8, out: &dyn Output) -> Result<(), SeqError> {
    let a = trim_leading_zeros(a);
    let b = trim_leading_zeros(b);
    let inf = b == "inf";

    // The current number, most significant digit first.
    let mut digits: Vec<u8> = a.bytes().collect();
    let b_digits: &[u8] = b.as_bytes();

    let mut buf: Vec<u8> = Vec::with_capacity(FLUSH_LEN + digits.len() + 1);
    let mut wrote_any = false;
    while inf || cmp_digits(&digits, b_digits).is_le() {
        buf.extend_from_slice(&digits);
        buf.push(separator);
        wrote_any = true;
        if buf.len() >= FLUSH_LEN {
            // Hold the final byte back: the last separator of the whole
            // run must become the terminator, and only the next
            // comparison knows whether this number was the last.
            out.write_all(&buf[..buf.len() - 1])?;
            let last = buf[buf.len() - 1];
            buf.clear();
            buf.push(last);
        }
        add_to_digits(&mut digits, step);
    }

    if wrote_any {
        // Swap the trailing separator for the newline terminator.
        buf.pop();
        buf.push(b'\n');
        out.write_all(&buf)?;
    }
    Ok(())
}

/// Trim leading zeros, leaving one digit when the string is all zeros.
fn trim_leading_zeros(s: &str) -> &str {
    let trimmed = s.trim_start_matches('0');
    if trimmed.is_empty() {
        &s[s.len() - 1..]
    } else {
        trimmed
    }
}

/// Compare two decimal digit strings without redundant leading zeros.
fn cmp_digits(a: &[u8], b: &[u8]) -> core::cmp::Ordering {
    a.len().cmp(&b.len()).then_with(|| a.cmp(b))
}

/// Add `step` to the digit string in place (most significant digit
/// first), growing on the left when the sum carries out.
fn add_to_digits(digits: &mut Vec<u8>, step: u64) {
    let mut carry = step;
    let mut index = digits.len();
    while index > 0 && carry > 0 {
        index -= 1;
        let sum = u64::from(digits[index] - b'0') + carry % 10;
        digits[index] = b'0' + (sum % 10) as u8;
        carry = carry / 10 + sum / 10;
    }
    while carry > 0 {
        digits.insert(0, b'0' + (carry % 10) as u8);
        carry /= 10;
    }
}

/// Print the floating-point sequence through `format` (GNU
/// `print_numbers`), including the rule that a value one step past LAST
/// still prints when it renders equal to LAST but differently from its
/// predecessor — the x86 `seq 0 0.000001 0.000003` rounding case.
fn print_numbers(
    format: &Format,
    separator: &[u8],
    first: f64,
    step: f64,
    last: f64,
    out: &dyn Output,
) -> Result<(), SeqError> {
    let past = |x: f64| if step < 0.0 { x < last } else { last < x };
    if past(first) {
        return Ok(());
    }

    let mut buf: Vec<u8> = Vec::with_capacity(FLUSH_LEN + 64);
    let mut x = first;
    let mut out_of_range = false;
    let mut i = 1.0_f64;
    loop {
        let x0 = x;
        buf.extend_from_slice(format.render(x).as_bytes());
        if buf.len() >= FLUSH_LEN {
            out.write_all(&buf)?;
            buf.clear();
        }
        if out_of_range {
            break;
        }

        // `first + i * step` accumulates less rounding error than a
        // running `x += step`.
        x = first + i * step;
        i += 1.0;
        if past(x) {
            if !prints_as_last(format, x, x0, last) {
                break;
            }
            // Print the extra number on the next pass, then stop.
            out_of_range = true;
        }
        buf.extend_from_slice(separator);
    }
    buf.push(b'\n');
    out.write_all(&buf)
}

/// The extra-number rule: `x` (one step past LAST) is printed when its
/// rendering parses back to exactly LAST and differs from the previous
/// number's rendering.
fn prints_as_last(format: &Format, x: f64, x0: f64, last: f64) -> bool {
    let x_str = format.render(x);
    let numeric = &x_str[format.prefix.len()..x_str.len() - format.suffix.len()];
    // Exact equality is the GNU rule: the re-parsed rendering must be
    // LAST itself, not merely near it.
    #[allow(clippy::float_cmp)]
    match parse_number(numeric) {
        Some(value) if value == last => format.render(x0) != x_str,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use alloc::format;
    use alloc::string::String;
    use alloc::vec::Vec;
    use core::cell::RefCell;

    use tairix_help::{HelpSource, SourceError};

    use super::{run, Output, USAGE};
    use crate::command::parse;
    use crate::error::SeqError;

    /// An [`Output`] recording everything written.
    #[derive(Default)]
    struct Sink {
        written: RefCell<Vec<u8>>,
    }

    impl Output for Sink {
        fn write_all(&self, bytes: &[u8]) -> Result<(), SeqError> {
            self.written.borrow_mut().extend_from_slice(bytes);
            Ok(())
        }
    }

    /// A help source with no documents, so the short help falls back to
    /// the usage banner.
    struct NoHelp;

    impl HelpSource for NoHelp {
        fn locale_dirs(&self) -> Result<Vec<String>, SourceError> {
            Ok(Vec::new())
        }
        fn read(&self, _locale: &str, _document: &str) -> Result<Option<Vec<u8>>, SourceError> {
            Ok(None)
        }
    }

    fn seq(args: &[&str]) -> Result<String, SeqError> {
        let sink = Sink::default();
        run(parse(args)?, None, &NoHelp, &sink)?;
        Ok(String::from_utf8(sink.written.into_inner()).expect("utf-8 output"))
    }

    #[test]
    fn counts_from_one_by_one() {
        assert_eq!(seq(&["5"]).expect("runs"), "1\n2\n3\n4\n5\n");
        assert_eq!(seq(&["1"]).expect("runs"), "1\n");
        assert_eq!(seq(&["2", "5"]).expect("runs"), "2\n3\n4\n5\n");
        assert_eq!(seq(&["1", "2", "10"]).expect("runs"), "1\n3\n5\n7\n9\n");
    }

    #[test]
    fn an_empty_range_prints_nothing() {
        assert_eq!(seq(&["5", "1"]).expect("runs"), "");
        assert_eq!(seq(&["0"]).expect("runs"), "");
        assert_eq!(seq(&["1", "-1", "5"]).expect("runs"), "");
    }

    #[test]
    fn descending_sequences() {
        assert_eq!(seq(&["5", "-1", "1"]).expect("runs"), "5\n4\n3\n2\n1\n");
        assert_eq!(seq(&["3", "-1.5", "0"]).expect("runs"), "3.0\n1.5\n0.0\n");
        assert_eq!(seq(&["-1", "-3"]).expect("runs"), "");
        assert_eq!(seq(&["-1", "-1", "-3"]).expect("runs"), "-1\n-2\n-3\n");
    }

    #[test]
    fn separators_join_numbers() {
        assert_eq!(seq(&["-s", ",", "3"]).expect("runs"), "1,2,3\n");
        // A multi-byte separator leaves the exact path but prints the same
        // integers.
        assert_eq!(seq(&["-s", "::", "3"]).expect("runs"), "1::2::3\n");
        assert_eq!(seq(&["-s", "", "3"]).expect("runs"), "123\n");
    }

    #[test]
    fn fractional_steps_infer_the_precision() {
        assert_eq!(
            seq(&["0.5", "0.5", "2"]).expect("runs"),
            "0.5\n1.0\n1.5\n2.0\n"
        );
        assert_eq!(seq(&["1", "0.5", "2"]).expect("runs"), "1.0\n1.5\n2.0\n");
        assert_eq!(
            seq(&["1", "0.25", "1.5"]).expect("runs"),
            "1.00\n1.25\n1.50\n"
        );
        // The GNU rounding rule: the value one step past LAST prints when
        // it renders as LAST.
        assert_eq!(
            seq(&["0", "0.000001", "0.000003"]).expect("runs"),
            "0.000000\n0.000001\n0.000002\n0.000003\n"
        );
    }

    #[test]
    fn equal_width_pads_with_zeros() {
        assert_eq!(seq(&["-w", "8", "10"]).expect("runs"), "08\n09\n10\n");
        assert_eq!(
            seq(&["-w", "1", "3", "10"]).expect("runs"),
            "01\n04\n07\n10\n"
        );
        assert_eq!(
            seq(&["-w", "0.5", "0.5", "2"]).expect("runs"),
            "0.5\n1.0\n1.5\n2.0\n"
        );
        assert_eq!(
            seq(&["-w", "-2", "2"]).expect("runs"),
            "-2\n-1\n00\n01\n02\n"
        );
    }

    #[test]
    fn formats_shape_the_output() {
        assert_eq!(
            seq(&["-f", "%.2f", "3"]).expect("runs"),
            "1.00\n2.00\n3.00\n"
        );
        assert_eq!(seq(&["-f", "%g", "3"]).expect("runs"), "1\n2\n3\n");
        assert_eq!(
            seq(&["-f", "%.2e", "100", "100", "300"]).expect("runs"),
            "1.00e+02\n2.00e+02\n3.00e+02\n"
        );
        assert_eq!(seq(&["-f", "[%.0f]", "2"]).expect("runs"), "[1]\n[2]\n");
    }

    #[test]
    fn exact_integers_beyond_the_float_mantissa() {
        // 2^64 + 1 and neighbours: exact only through the decimal path.
        assert_eq!(
            seq(&["18446744073709551615", "18446744073709551617"]).expect("runs"),
            "18446744073709551615\n18446744073709551616\n18446744073709551617\n"
        );
        // A carry across every digit.
        assert_eq!(
            seq(&[
                "999999999999999999999999999999",
                "1000000000000000000000000000000"
            ])
            .expect("runs"),
            "999999999999999999999999999999\n1000000000000000000000000000000\n"
        );
        // Steps up to 200 stay exact.
        assert_eq!(
            seq(&["9999999999999999999999", "200", "10000000000000000000399"]).expect("runs"),
            "9999999999999999999999\n10000000000000000000199\n10000000000000000000399\n"
        );
    }

    #[test]
    fn integer_spellings_reach_the_exact_path() {
        assert_eq!(seq(&["1e1"]).expect("runs").lines().count(), 10);
        assert_eq!(seq(&["0x14"]).expect("runs").lines().count(), 20);
        assert_eq!(seq(&["8", "1e1"]).expect("runs"), "8\n9\n10\n");
        assert_eq!(seq(&["007"]).expect("runs"), "1\n2\n3\n4\n5\n6\n7\n");
    }

    #[test]
    fn long_runs_cross_the_flush_boundary_intact() {
        // > 8 KiB of output exercises the buffered flush with the
        // held-back separator/terminator byte.
        use core::fmt::Write;

        let text = seq(&["3000"]).expect("runs");
        let expected: String = (1..=3000).fold(String::new(), |mut acc, n| {
            let _ = writeln!(acc, "{n}");
            acc
        });
        assert_eq!(text, expected);
    }

    #[test]
    fn scan_errors_surface_in_operand_order() {
        assert_eq!(
            seq(&["x", "0", "y"]),
            Err(SeqError::InvalidNumber(String::from("x")))
        );
        // The zero increment is diagnosed before the LAST operand is
        // scanned, as in the GNU tool.
        assert_eq!(
            seq(&["1", "0", "y"]),
            Err(SeqError::ZeroIncrement(String::from("0")))
        );
        assert_eq!(
            seq(&["1", "0.0", "5"]),
            Err(SeqError::ZeroIncrement(String::from("0.0")))
        );
        assert_eq!(
            seq(&["1", "-0", "5"]),
            Err(SeqError::ZeroIncrement(String::from("-0")))
        );
        assert_eq!(
            seq(&["nan"]),
            Err(SeqError::NotANumber(String::from("nan")))
        );
    }

    #[test]
    fn short_help_falls_back_to_the_usage_banner() {
        assert_eq!(seq(&["-h"]).expect("runs"), format!("{USAGE}\n"));
    }

    /// Every locale's `OPTIONS` section documents exactly the switches
    /// this parser accepts (`plans/APPS.md` §3.1): the flag tokens are
    /// language-neutral, so each translated document must carry the same
    /// keys as the canonical one. The documents are read from the
    /// bundle's own on-disk `Help/` tree — the single source the image
    /// builder plants — never a copy embedded in this crate.
    #[test]
    fn help_documents_the_parser_switches() {
        use std::fs;

        let help_root = format!("{}/Help", env!("CARGO_MANIFEST_DIR"));
        let locales = tairix_help::REQUIRED_LOCALES;
        for locale in locales {
            let path = format!("{help_root}/{locale}/seq.md");
            let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
            for switch in [
                "`-f, --format <format>`",
                "`-s, --separator <string>`",
                "`-w, --equal-width`",
                "`-h, -?`",
            ] {
                assert!(
                    text.contains(switch),
                    "{locale}/seq.md must document {switch}"
                );
            }
        }
    }
}
