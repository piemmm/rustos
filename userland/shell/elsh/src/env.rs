//! The shell's mutable state and the variable-expansion phase.
//!
//! An [`Environment`] holds the shell variables, the working directory, and
//! the exit status of the most recently completed pipeline (`$?`). It is the
//! single owner of expansion: turning a parsed [`Word`] — still a list of
//! [`Segment`]s — into the final argument string.
//!
//! Expansion is deliberately *minimal and correct* rather than broad:
//!
//! * `$NAME` and `${NAME}` expand a variable; an unset variable expands to
//!   the empty string (POSIX default).
//! * `$?` and `${?}` expand to the last exit status.
//! * A `$` not followed by a name, `{`, or `?` is a literal `$`.
//! * Expansion never field-splits its result: each word stays one word. This
//!   is a documented simplification, not a defect — it keeps argument counts
//!   predictable and avoids re-quoting surprises.
//!
//! Only [`Segment::Expandable`] runs are scanned; [`Segment::Literal`] runs
//! (single quotes, backslash escapes) are emitted verbatim, so a quoted or
//! escaped `$` is never expanded.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::error::ParseError;
use crate::lexer::{Segment, Word};

/// Mutable shell state: variables, working directory, and `$?`.
#[derive(Clone, Debug)]
pub struct Environment {
    vars: BTreeMap<String, String>,
    exported: BTreeSet<String>,
    cwd: String,
    last_status: i32,
}

impl Default for Environment {
    fn default() -> Self {
        Self::new()
    }
}

impl Environment {
    /// A fresh environment with no variables and the working directory at the
    /// root (`/`).
    #[must_use]
    pub fn new() -> Self {
        Self {
            vars: BTreeMap::new(),
            exported: BTreeSet::new(),
            cwd: String::from("/"),
            last_status: 0,
        }
    }

    /// The current working directory.
    #[must_use]
    pub fn cwd(&self) -> &str {
        &self.cwd
    }

    /// Replace the working directory.
    pub fn set_cwd(&mut self, path: impl Into<String>) {
        self.cwd = path.into();
    }

    /// The exit status of the last completed pipeline.
    #[must_use]
    pub fn last_status(&self) -> i32 {
        self.last_status
    }

    /// Record the exit status of the just-completed pipeline.
    pub fn set_last_status(&mut self, status: i32) {
        self.last_status = status;
    }

    /// The value of a variable, if set.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&str> {
        self.vars.get(name).map(String::as_str)
    }

    /// Set a shell variable. Does not change its exported state.
    pub fn set(&mut self, name: impl Into<String>, value: impl Into<String>) {
        self.vars.insert(name.into(), value.into());
    }

    /// Set a variable and mark it for export to child processes.
    pub fn export(&mut self, name: impl Into<String>, value: impl Into<String>) {
        let name = name.into();
        self.vars.insert(name.clone(), value.into());
        self.exported.insert(name);
    }

    /// Mark an already-set variable as exported. Returns `false` if the
    /// variable does not exist.
    pub fn mark_exported(&mut self, name: &str) -> bool {
        if self.vars.contains_key(name) {
            self.exported.insert(name.to_string());
            true
        } else {
            false
        }
    }

    /// Remove a variable (and its exported mark). Returns `true` if it was set.
    pub fn unset(&mut self, name: &str) -> bool {
        self.exported.remove(name);
        self.vars.remove(name).is_some()
    }

    /// The exported variables, sorted by name — the environment a child
    /// process inherits.
    #[must_use]
    pub fn exported_vars(&self) -> Vec<(&str, &str)> {
        self.exported
            .iter()
            .filter_map(|name| self.vars.get(name).map(|v| (name.as_str(), v.as_str())))
            .collect()
    }

    /// Expand a parsed word into its final string.
    ///
    /// # Errors
    ///
    /// Returns [`ParseError::UnterminatedExpansion`] if a `${` is never
    /// closed by `}`.
    pub fn expand_word(&self, word: &Word) -> Result<String, ParseError> {
        let mut out = String::new();
        for segment in word {
            match segment {
                Segment::Literal(s) => out.push_str(s),
                Segment::Expandable(s) => self.expand_into(s, &mut out)?,
            }
        }
        Ok(out)
    }

    fn expand_into(&self, source: &str, out: &mut String) -> Result<(), ParseError> {
        let chars: Vec<char> = source.chars().collect();
        let mut i = 0;
        while i < chars.len() {
            if chars[i] != '$' {
                out.push(chars[i]);
                i += 1;
                continue;
            }
            i += 1;
            match chars.get(i) {
                Some('{') => {
                    i += 1;
                    let start = i;
                    while i < chars.len() && chars[i] != '}' {
                        i += 1;
                    }
                    if i >= chars.len() {
                        return Err(ParseError::UnterminatedExpansion);
                    }
                    let name: String = chars[start..i].iter().collect();
                    i += 1; // consume '}'
                    out.push_str(&self.lookup(&name));
                }
                Some('?') => {
                    i += 1;
                    out.push_str(&self.last_status.to_string());
                }
                Some(&c) if is_name_start(c) => {
                    let start = i;
                    while i < chars.len() && is_name_char(chars[i]) {
                        i += 1;
                    }
                    let name: String = chars[start..i].iter().collect();
                    out.push_str(&self.lookup(&name));
                }
                _ => out.push('$'),
            }
        }
        Ok(())
    }

    fn lookup(&self, name: &str) -> String {
        if name == "?" {
            return self.last_status.to_string();
        }
        self.get(name).unwrap_or("").to_string()
    }
}

/// Split a word of the form `NAME=VALUE` into its name and its (still
/// unexpanded) value word.
///
/// Returns `None` unless the word begins with a valid variable name followed
/// by `=` in unquoted text — so `'FOO'=bar` (a quoted name) and `1=x` (a name
/// starting with a digit) are *not* assignments. The value retains its
/// segments so it is expanded exactly like any other word: `FOO=$BAR` and
/// `FOO="a b"` work as expected.
#[must_use]
pub fn assignment_split(word: &Word) -> Option<(String, Word)> {
    let Segment::Expandable(first) = word.first()? else {
        return None;
    };
    let eq = first.find('=')?;
    let name = &first[..eq];
    if !is_valid_name(name) {
        return None;
    }
    let mut value: Word = Vec::new();
    let rest = &first[eq + 1..];
    if !rest.is_empty() {
        value.push(Segment::Expandable(rest.to_string()));
    }
    value.extend_from_slice(&word[1..]);
    Some((name.to_string(), value))
}

/// `true` if `name` is a valid shell variable name (`[A-Za-z_][A-Za-z0-9_]*`).
#[must_use]
pub fn is_valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if is_name_start(c) => chars.all(is_name_char),
        _ => false,
    }
}

fn is_name_start(c: char) -> bool {
    c == '_' || c.is_ascii_alphabetic()
}

fn is_name_char(c: char) -> bool {
    c == '_' || c.is_ascii_alphanumeric()
}

#[cfg(test)]
mod tests {
    use super::{assignment_split, is_valid_name, Environment};
    use crate::error::ParseError;
    use crate::lexer::{Segment, Word};
    use alloc::string::{String, ToString};
    use alloc::vec;

    fn expandable(s: &str) -> Word {
        vec![Segment::Expandable(s.to_string())]
    }

    fn flat(word: &Word) -> String {
        let mut out = String::new();
        for seg in word {
            match seg {
                Segment::Literal(s) | Segment::Expandable(s) => out.push_str(s),
            }
        }
        out
    }

    #[test]
    fn expands_set_and_unset_variables() {
        let mut env = Environment::new();
        env.set("NAME", "rustos");
        assert_eq!(env.expand_word(&expandable("$NAME")).unwrap(), "rustos");
        assert_eq!(env.expand_word(&expandable("${NAME}!")).unwrap(), "rustos!");
        // Unset expands to empty.
        assert_eq!(env.expand_word(&expandable("[$MISSING]")).unwrap(), "[]");
    }

    #[test]
    fn expands_last_status() {
        let mut env = Environment::new();
        env.set_last_status(42);
        assert_eq!(env.expand_word(&expandable("$?")).unwrap(), "42");
        assert_eq!(env.expand_word(&expandable("${?}")).unwrap(), "42");
    }

    #[test]
    fn literal_segments_are_never_expanded() {
        let env = Environment::new();
        let word = vec![Segment::Literal("$NAME".to_string())];
        assert_eq!(env.expand_word(&word).unwrap(), "$NAME");
    }

    #[test]
    fn lone_dollar_is_literal() {
        let env = Environment::new();
        assert_eq!(env.expand_word(&expandable("a$ b")).unwrap(), "a$ b");
        assert_eq!(env.expand_word(&expandable("cost$")).unwrap(), "cost$");
    }

    #[test]
    fn unterminated_brace_expansion_fails_closed() {
        let env = Environment::new();
        assert_eq!(
            env.expand_word(&expandable("${oops")),
            Err(ParseError::UnterminatedExpansion)
        );
    }

    #[test]
    fn concatenates_adjacent_expansions() {
        let mut env = Environment::new();
        env.set("A", "x");
        env.set("B", "y");
        assert_eq!(env.expand_word(&expandable("$A$B")).unwrap(), "xy");
    }

    #[test]
    fn export_controls_child_environment() {
        let mut env = Environment::new();
        env.set("SECRET", "1");
        env.export("PATH", "/Apps");
        assert_eq!(env.exported_vars(), [("PATH", "/Apps")]);
        // A plain `set` variable is visible to the shell but not exported.
        assert_eq!(env.get("SECRET"), Some("1"));
        assert!(env.mark_exported("SECRET"));
        assert_eq!(env.exported_vars(), [("PATH", "/Apps"), ("SECRET", "1")]);
        assert!(!env.mark_exported("GHOST"));
    }

    #[test]
    fn unset_removes_value_and_export() {
        let mut env = Environment::new();
        env.export("X", "1");
        assert!(env.unset("X"));
        assert_eq!(env.get("X"), None);
        assert!(env.exported_vars().is_empty());
        assert!(!env.unset("X"));
    }

    #[test]
    fn assignment_split_recognises_valid_forms() {
        let (name, value) = assignment_split(&expandable("FOO=bar")).unwrap();
        assert_eq!(name, "FOO");
        assert_eq!(flat(&value), "bar");

        // Value keeps its segments for later expansion.
        let word = vec![
            Segment::Expandable("FOO=".to_string()),
            Segment::Literal("a b".to_string()),
        ];
        let (name, value) = assignment_split(&word).unwrap();
        assert_eq!(name, "FOO");
        assert_eq!(flat(&value), "a b");

        // Empty value is allowed.
        let (name, value) = assignment_split(&expandable("EMPTY=")).unwrap();
        assert_eq!(name, "EMPTY");
        assert_eq!(flat(&value), "");
    }

    #[test]
    fn assignment_split_rejects_non_assignments() {
        assert!(assignment_split(&expandable("notanassignment")).is_none());
        assert!(assignment_split(&expandable("1BAD=x")).is_none());
        // A quoted name is not an assignment.
        let quoted = vec![
            Segment::Literal("FOO".to_string()),
            Segment::Expandable("=bar".to_string()),
        ];
        assert!(assignment_split(&quoted).is_none());
    }

    #[test]
    fn valid_name_rules() {
        assert!(is_valid_name("FOO_bar9"));
        assert!(is_valid_name("_x"));
        assert!(!is_valid_name(""));
        assert!(!is_valid_name("9x"));
        assert!(!is_valid_name("a-b"));
    }
}
