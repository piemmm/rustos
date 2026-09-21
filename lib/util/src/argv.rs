//! Resolving a value-taking option's value from a command line.
//!
//! Every GNU-shaped command app accepts an option's value attached
//! (`-u0`, `--uid=0`) or as the following argument (`-u 0`, `--uid 0`).
//! The rule is one line of logic and identical in each tool, so it is
//! defined here and imported rather than copied into each parser — a
//! private copy per tool is how two of them end up disagreeing about
//! whether `--uid` at the end of a line is a usage error.
//!
//! The caller owns its own error type, so the resolver reports the
//! *absence* and lets the parser name it.

/// The value of a value-taking option: the attached `inline` text when the
/// argument carried one, otherwise the following argument.
///
/// `index` is the parser's cursor, already advanced past the option
/// itself; taking the following argument advances it again. `None` is an
/// option at the end of the line with no value, which every caller reports
/// as its own usage error.
#[must_use]
pub fn option_value<'a>(
    inline: Option<&'a str>,
    args: &[&'a str],
    index: &mut usize,
) -> Option<&'a str> {
    if inline.is_some() {
        return inline;
    }
    let value = args.get(*index).copied()?;
    *index += 1;
    Some(value)
}

#[cfg(test)]
mod tests {
    use super::option_value;

    #[test]
    fn an_attached_value_wins_and_leaves_the_cursor_alone() {
        let args = ["--uid=7", "operand"];
        let mut index = 1;
        assert_eq!(option_value(Some("7"), &args, &mut index), Some("7"));
        assert_eq!(index, 1);
    }

    #[test]
    fn a_detached_value_is_taken_from_the_line_and_consumed() {
        let args = ["-u", "7", "operand"];
        let mut index = 1;
        assert_eq!(option_value(None, &args, &mut index), Some("7"));
        assert_eq!(index, 2);
        // The operand that follows is still the parser's to read.
        assert_eq!(args.get(index).copied(), Some("operand"));
    }

    #[test]
    fn an_option_at_the_end_of_the_line_has_no_value() {
        let args = ["-u"];
        let mut index = 1;
        assert_eq!(option_value(None, &args, &mut index), None);
        assert_eq!(index, 1);
    }

    #[test]
    fn an_attached_empty_value_is_a_value() {
        // `--comment=` clears the comment; it is not a missing value, and
        // must not silently swallow the next argument.
        let args = ["--comment=", "operand"];
        let mut index = 1;
        assert_eq!(option_value(Some(""), &args, &mut index), Some(""));
        assert_eq!(index, 1);
    }
}
