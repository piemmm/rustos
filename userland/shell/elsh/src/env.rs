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
//! Only [`Segment::Expandable`] and [`Segment::QuotedExpandable`] runs are
//! scanned; [`Segment::Literal`] runs (single quotes, backslash escapes) are
//! emitted verbatim, so a quoted or escaped `$` is never expanded.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::error::ParseError;
use crate::lexer::{Segment, Word};

/// The default interactive prompt format, stored as `ELSH_PROMPT` when the
/// session exported none. It renders `user@host working-directory% `; the
/// backslash escapes are expanded by [`Environment::render_prompt`]:
/// `\u` → `USER`, `\h` → `HOSTNAME`, `\w` → the working directory with the
/// user's home abbreviated to `~`.
pub const DEFAULT_PROMPT: &str = "\\u@\\h \\w% ";

/// The hostname the prompt shows when the session exported no `HOSTNAME`
/// (the system hostname is still unprovisioned). A fixed, honest default —
/// the analogue of a POSIX system's `localhost` — not a guess at the real
/// name.
pub const DEFAULT_HOSTNAME: &str = "tairix";

/// The user name the prompt shows when the session exported no `USER`.
const DEFAULT_USER: &str = "user";

/// The shell's own fallback search path when the session exported no `PATH`
/// (the shell run outside a normal login). A defensive default the shell
/// owns, exactly as an interactive `bash` supplies its own when `PATH` is
/// unset; the authoritative value is the one login exports.
const DEFAULT_PATH: &str = "/System/Apps:/Apps";

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
                Segment::Expandable(s) | Segment::QuotedExpandable(s) => {
                    self.expand_into(s, &mut out)?;
                }
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

    /// Seed the standard interactive-session variables, exporting each so a
    /// child inherits it, and set the working directory to the user's home.
    ///
    /// `inherited(name)` returns the value the spawner (login) exported for
    /// `name`, or `None`. Identity and locale variables (`USER`, `LOGNAME`,
    /// `HOME`, `SHELL`, `PATH`, `TERM`, `LANG`) come from the inherited
    /// environment login built; the shell fills a defensive default only when
    /// one is absent (a shell run outside a normal login). The shell-owned
    /// variables — `HOSTNAME` (the prompt host, defaulting to
    /// [`DEFAULT_HOSTNAME`] until the system hostname is provisioned),
    /// `PWD`/`OLDPWD`, and the prompt format `ELSH_PROMPT`
    /// ([`DEFAULT_PROMPT`]) — are filled here. The lookup seam keeps this
    /// pure and host-testable: production passes a closure over the runtime's
    /// environment accessor, tests pass a map.
    pub fn seed_interactive(&mut self, inherited: impl Fn(&str) -> Option<String>) {
        let take = |name: &str, default: &str| -> String {
            inherited(name)
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| default.to_string())
        };
        let user = take("USER", DEFAULT_USER);
        let home = take("HOME", "/");
        let pwd = inherited("PWD")
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| home.clone());
        let logname = inherited("LOGNAME")
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| user.clone());

        self.export("USER", user);
        self.export("LOGNAME", logname);
        self.export("HOME", home);
        self.export("SHELL", take("SHELL", "/System/Apps/elsh.app/Run"));
        self.export("PATH", take("PATH", DEFAULT_PATH));
        self.export("TERM", take("TERM", "xterm-256color"));
        self.export("LANG", take("LANG", "en-US"));
        self.export("HOSTNAME", take("HOSTNAME", DEFAULT_HOSTNAME));
        self.export("PWD", pwd.clone());
        // OLDPWD is defined from the first prompt (equal to PWD until the
        // first `cd`), so `$OLDPWD` and `cd -` are never unset surprises.
        self.export("OLDPWD", pwd.clone());
        self.set("ELSH_PROMPT", take("ELSH_PROMPT", DEFAULT_PROMPT));
        self.set_cwd(pwd);
    }

    /// The `USER` value for the prompt, or [`DEFAULT_USER`] when unset.
    fn prompt_user(&self) -> &str {
        self.get("USER")
            .filter(|v| !v.is_empty())
            .unwrap_or(DEFAULT_USER)
    }

    /// The `HOSTNAME` value for the prompt, or [`DEFAULT_HOSTNAME`] when unset.
    fn prompt_host(&self) -> &str {
        self.get("HOSTNAME")
            .filter(|v| !v.is_empty())
            .unwrap_or(DEFAULT_HOSTNAME)
    }

    /// The working directory for the prompt, with the user's `HOME`
    /// abbreviated to `~` (exactly `~` at home, `~/rest` beneath it).
    fn prompt_cwd(&self) -> String {
        let cwd = self.cwd();
        if let Some(home) = self.get("HOME").filter(|h| !h.is_empty()) {
            if cwd == home {
                return "~".to_string();
            }
            if let Some(rest) = cwd.strip_prefix(home) {
                if rest.starts_with('/') {
                    return alloc::format!("~{rest}");
                }
            }
        }
        cwd.to_string()
    }

    /// Render the interactive prompt from the `ELSH_PROMPT` format (or
    /// [`DEFAULT_PROMPT`] when unset), expanding the escapes `\u` (`USER`),
    /// `\h` (`HOSTNAME`), and `\w` (the working directory with `HOME`
    /// abbreviated to `~`). `\\` is a literal backslash; any other `\x` is
    /// left verbatim so an unknown escape is shown, never dropped.
    #[must_use]
    pub fn render_prompt(&self) -> String {
        let format = self
            .get("ELSH_PROMPT")
            .map_or_else(|| DEFAULT_PROMPT.to_string(), ToString::to_string);
        let mut out = String::new();
        let mut chars = format.chars();
        while let Some(c) = chars.next() {
            if c != '\\' {
                out.push(c);
                continue;
            }
            match chars.next() {
                Some('u') => out.push_str(self.prompt_user()),
                Some('h') => out.push_str(self.prompt_host()),
                Some('w') => out.push_str(&self.prompt_cwd()),
                // A literal backslash, or a lone trailing backslash at the
                // end of the format, both render a single `\`.
                Some('\\') | None => out.push('\\'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
            }
        }
        out
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

/// Split a command's words into its leading `NAME=VALUE` prefix assignments
/// and the remaining words (the command proper).
///
/// The split follows POSIX: assignment words bind only while they *lead* the
/// command — the first non-assignment word ends the prefix, and a later
/// `NAME=VALUE` is an ordinary argument. Both halves may be empty (an
/// assignment-only command, or a command with no prefix). Values keep their
/// segments so the caller expands them exactly like any other word.
#[must_use]
pub fn split_prefix_assignments(words: &[Word]) -> (Vec<(String, Word)>, &[Word]) {
    let mut assignments = Vec::new();
    for (index, word) in words.iter().enumerate() {
        match assignment_split(word) {
            Some(assignment) => assignments.push(assignment),
            None => return (assignments, &words[index..]),
        }
    }
    (assignments, &[])
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
    use super::{assignment_split, is_valid_name, split_prefix_assignments, Environment};
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
                Segment::Literal(s) | Segment::Expandable(s) | Segment::QuotedExpandable(s) => {
                    out.push_str(s);
                }
            }
        }
        out
    }

    #[test]
    fn expands_set_and_unset_variables() {
        let mut env = Environment::new();
        env.set("NAME", "tairix");
        assert_eq!(env.expand_word(&expandable("$NAME")).unwrap(), "tairix");
        assert_eq!(env.expand_word(&expandable("${NAME}!")).unwrap(), "tairix!");
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
    fn seed_interactive_takes_inherited_values_and_exports_them() {
        let mut env = Environment::new();
        env.seed_interactive(|name| match name {
            "USER" => Some("ada".to_string()),
            "HOME" => Some("/Users/ada".to_string()),
            "SHELL" => Some("/System/Apps/elsh.app/Run".to_string()),
            "TERM" => Some("xterm-256color".to_string()),
            _ => None,
        });
        assert_eq!(env.get("USER"), Some("ada"));
        // LOGNAME defaults to USER when not separately inherited.
        assert_eq!(env.get("LOGNAME"), Some("ada"));
        assert_eq!(env.get("HOME"), Some("/Users/ada"));
        // PWD/OLDPWD and the working directory follow HOME.
        assert_eq!(env.get("PWD"), Some("/Users/ada"));
        assert_eq!(env.get("OLDPWD"), Some("/Users/ada"));
        assert_eq!(env.cwd(), "/Users/ada");
        // The standard variables are exported to children.
        let exported: alloc::vec::Vec<&str> =
            env.exported_vars().into_iter().map(|(n, _)| n).collect();
        for name in ["USER", "LOGNAME", "HOME", "SHELL", "PATH", "TERM", "LANG"] {
            assert!(exported.contains(&name), "{name} exported: {exported:?}");
        }
    }

    #[test]
    fn seed_interactive_fills_defaults_when_nothing_is_inherited() {
        let mut env = Environment::new();
        env.seed_interactive(|_| None);
        assert_eq!(env.get("USER"), Some(super::DEFAULT_USER));
        assert_eq!(env.get("HOSTNAME"), Some(super::DEFAULT_HOSTNAME));
        assert_eq!(env.get("LANG"), Some("en-US"));
        assert_eq!(env.get("PATH"), Some(super::DEFAULT_PATH));
        assert_eq!(env.get("ELSH_PROMPT"), Some(super::DEFAULT_PROMPT));
        // An empty inherited value is treated as absent (fills the default).
        let mut empty = Environment::new();
        empty.seed_interactive(|name| (name == "USER").then(String::new));
        assert_eq!(empty.get("USER"), Some(super::DEFAULT_USER));
    }

    #[test]
    fn render_prompt_shows_user_host_and_home_abbreviated_cwd() {
        let mut env = Environment::new();
        env.seed_interactive(|name| match name {
            "USER" => Some("ada".to_string()),
            "HOSTNAME" => Some("babbage".to_string()),
            "HOME" => Some("/Users/ada".to_string()),
            _ => None,
        });
        // At home the working directory renders as `~`.
        assert_eq!(env.render_prompt(), "ada@babbage ~% ");
        // Beneath home it is `~/rest`.
        env.set_cwd("/Users/ada/Documents");
        assert_eq!(env.render_prompt(), "ada@babbage ~/Documents% ");
        // Outside home the absolute path is shown.
        env.set_cwd("/Storage/disk");
        assert_eq!(env.render_prompt(), "ada@babbage /Storage/disk% ");
    }

    #[test]
    fn render_prompt_honours_a_custom_format_and_unknown_escapes() {
        let mut env = Environment::new();
        env.set("USER", "ada");
        env.set("HOSTNAME", "babbage");
        // A custom format with a literal and an unknown escape (kept verbatim).
        env.set("ELSH_PROMPT", "[\\u@\\h]\\z$ ");
        assert_eq!(env.render_prompt(), "[ada@babbage]\\z$ ");
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
        // A quoted name is not an assignment, whether single- or double-quoted.
        let quoted = vec![
            Segment::Literal("FOO".to_string()),
            Segment::Expandable("=bar".to_string()),
        ];
        assert!(assignment_split(&quoted).is_none());
        let double_quoted = vec![
            Segment::QuotedExpandable("FOO".to_string()),
            Segment::Expandable("=bar".to_string()),
        ];
        assert!(assignment_split(&double_quoted).is_none());
    }

    #[test]
    fn prefix_assignments_split_at_the_first_command_word() {
        let words = vec![
            expandable("A=1"),
            expandable("B=2"),
            expandable("cmd"),
            expandable("C=3"),
        ];
        let (assignments, rest) = split_prefix_assignments(&words);
        assert_eq!(assignments.len(), 2);
        assert_eq!(assignments[0].0, "A");
        assert_eq!(assignments[1].0, "B");
        // `C=3` follows the command word, so it is an ordinary argument.
        assert_eq!(rest.len(), 2);

        // Assignment-only: everything splits, nothing remains.
        let words = vec![expandable("A=1")];
        let (assignments, rest) = split_prefix_assignments(&words);
        assert_eq!(assignments.len(), 1);
        assert!(rest.is_empty());

        // No prefix: the words pass through untouched.
        let words = vec![expandable("cmd"), expandable("A=1")];
        let (assignments, rest) = split_prefix_assignments(&words);
        assert!(assignments.is_empty());
        assert_eq!(rest.len(), 2);
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
