//! The line grammar TAIRiX's `#`-commented configuration stores share.
//!
//! Every line-oriented store in the tree — the boot-time system
//! configuration (`lib/sysconfig`), the network configuration
//! (`lib/netconfig`), and the service registry and startup list
//! (`userland/system/init`) — reads the same shape of document: a `#`
//! begins a comment that runs to the end of the line, and blank lines
//! carry no setting. That tokenisation lives here once, so a change to how
//! a comment is recognised cannot apply to some stores and not others.
//!
//! No store's keys or values may contain `#`; each store's own validators
//! enforce that, which is what makes cutting at the first `#`
//! unambiguous.

/// What a configuration key accepts.
///
/// Shared because both closed registries state it and both a command-line
/// tool and a settings surface read it: a tool refusing a value names the
/// choices, and a surface offering the choices builds its list from the same
/// definition rather than a second copy that would drift.
///
/// Most keys carry a closed set of canonical spellings; a key whose value is
/// inherently open — an address, a list of host names — describes its
/// accepted form instead, and its own parser is what admits or refuses a
/// spelling.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ValueShape {
    /// One of these canonical spellings, and nothing else.
    Closed(&'static [&'static str]),
    /// Free-form text of this described form.
    Free(&'static str),
}

/// The portion of `line` before its first `#`, dropping an inline or
/// whole-line comment.
///
/// The returned slice keeps its surrounding whitespace: a caller trims it
/// with `str::trim` and treats an empty result as a line carrying no
/// setting.
#[must_use]
pub fn strip_comment(line: &str) -> &str {
    match line.find('#') {
        Some(index) => &line[..index],
        None => line,
    }
}

#[cfg(test)]
mod tests {
    use super::strip_comment;

    #[test]
    fn a_line_without_a_comment_is_returned_whole() {
        assert_eq!(strip_comment("os.loginType text"), "os.loginType text");
    }

    #[test]
    fn an_inline_comment_is_cut_at_the_first_marker() {
        assert_eq!(strip_comment("key value # why # again"), "key value ");
    }

    #[test]
    fn a_whole_line_comment_leaves_nothing() {
        assert!(strip_comment("# a comment").trim().is_empty());
    }

    #[test]
    fn an_empty_line_stays_empty() {
        assert_eq!(strip_comment(""), "");
    }

    #[test]
    fn surrounding_whitespace_is_left_for_the_caller_to_trim() {
        assert_eq!(strip_comment("  key value  ").trim(), "key value");
    }
}
