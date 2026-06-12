//! The PID 1 startup configuration: the minimal, fail-closed description of
//! what `init` does the instant it reaches user mode (`plans/PI.md` P6b).
//!
//! When the kernel spawns PID 1 into EL0 (`plans/PI.md` P6c) the `init`
//! program's first act is to learn what it should do. That intent is data, not
//! code: a small text [`StartupConfig`] that today says only "open the console
//! and start the user in a shell" ([`DEFAULT_CONFIG`]). Keeping it as parsed
//! configuration — rather than hard-coding the two actions — is what lets the
//! later stages (`plans/PI.md` P6e and beyond) grow the boot sequence without
//! editing `init`'s control flow.
//!
//! The config text is **untrusted input** (`AGENTS.md` §19.5/§19.6): it is the
//! first thing a freshly spawned program parses, so the parser is
//! allocation-free, borrows from the caller's text rather than copying, and
//! **fails closed** ([`ConfigError`]) on anything it does not understand — an
//! unknown directive, a duplicate, a missing or malformed argument, or a
//! config that omits a required directive. A surprising or partial startup
//! configuration never boots a surprising system (`AGENTS.md` §2.9, §5.4.5).
//!
//! # Grammar
//!
//! The text is a sequence of lines. A `#` begins a comment that runs to the
//! end of the line; blank lines and comment-only lines are ignored. Every
//! other line is a directive: a keyword optionally followed by whitespace and
//! a single argument. Exactly two directives are defined, and both are
//! required exactly once:
//!
//! * `console` — open the system console so the banner and later output have
//!   somewhere to go. Takes no argument.
//! * `session <path>` — the absolute path of the program `init` launches as
//!   the user's session (the shell, today). The argument must be an absolute
//!   path (`AGENTS.md` §16.5 bundle layout).

use core::fmt;

/// Maximum length, in bytes, of a startup config text [`StartupConfig::parse`]
/// will consider. A larger input is refused outright ([`ConfigError::TooLong`])
/// rather than scanned — the config `init` carries is tiny, and an
/// unboundedly large one is a defect, not a workload (`AGENTS.md` §2.9).
pub const MAX_CONFIG_LEN: usize = 4096;

/// The startup configuration compiled into the `init` `Run` binary.
///
/// The session is the login service (`plans/PI.md` P11): every text console
/// sits at a `login:` prompt, and the authenticated account's shell of
/// choice is started by login, never directly by `init`. Later stages
/// replace the compiled-in default with a config read from
/// `/System/Settings` once a filesystem is mounted; the parser does not
/// change.
pub const DEFAULT_CONFIG: &str = "\
# RustOS PID 1 startup configuration (plans/PI.md P6b / P11).
# Open the system console and start the login service as the session.
console
session /System/Services/login
";

/// The first line `init` writes to the console once it reaches user mode.
///
/// A fixed, terse banner (`AGENTS.md` §13 — no aimless waffle) that proves the
/// kernel reached EL0 and `init`'s console write path works end to end.
pub const BANNER: &str = "RustOS init: reached user mode\n";

/// Why a startup config text was refused.
///
/// Every variant is a fail-closed refusal: [`StartupConfig::parse`] returns it
/// and the caller starts nothing, rather than guess at a malformed intent
/// (`AGENTS.md` §2.9, §5.4.5).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ConfigError {
    /// The config text is longer than [`MAX_CONFIG_LEN`].
    TooLong,
    /// A line names a directive keyword the parser does not define.
    UnknownDirective,
    /// A directive that may appear at most once appeared twice.
    DuplicateDirective,
    /// A directive that takes an argument was given none.
    MissingArgument,
    /// A directive that takes no argument was given one.
    UnexpectedArgument,
    /// A `session` path was not absolute (did not begin with `/`).
    NotAbsolutePath,
    /// The required `console` directive was absent.
    ConsoleRequired,
    /// The required `session` directive was absent.
    SessionRequired,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::TooLong => "startup config exceeds the maximum length",
            Self::UnknownDirective => "startup config names an unknown directive",
            Self::DuplicateDirective => "startup config repeats a single-use directive",
            Self::MissingArgument => "a startup directive is missing its argument",
            Self::UnexpectedArgument => "a startup directive was given an unexpected argument",
            Self::NotAbsolutePath => "a startup path argument is not absolute",
            Self::ConsoleRequired => "startup config omits the required `console` directive",
            Self::SessionRequired => "startup config omits the required `session` directive",
        };
        f.write_str(message)
    }
}

/// A parsed, validated startup configuration borrowing from its source text.
///
/// Construct one with [`StartupConfig::parse`]. The borrow keeps the parser
/// allocation-free: the [`session`](Self::session) path points into the config
/// text the caller supplied and is valid for as long as that text lives.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct StartupConfig<'a> {
    session: &'a str,
}

impl<'a> StartupConfig<'a> {
    /// Parse and validate a startup config `text`.
    ///
    /// # Errors
    ///
    /// Returns the matching [`ConfigError`] if `text` exceeds
    /// [`MAX_CONFIG_LEN`], contains an unknown or duplicated directive, gives a
    /// directive the wrong arguments, carries a non-absolute `session` path, or
    /// omits a required directive. The parser fails closed: a config it cannot
    /// fully understand yields no [`StartupConfig`].
    pub fn parse(text: &'a str) -> Result<Self, ConfigError> {
        if text.len() > MAX_CONFIG_LEN {
            return Err(ConfigError::TooLong);
        }

        let mut console = false;
        let mut session: Option<&'a str> = None;

        for raw in text.lines() {
            let line = strip_comment(raw).trim();
            if line.is_empty() {
                continue;
            }

            let mut fields = line.splitn(2, char::is_whitespace);
            let keyword = fields.next().unwrap_or_default();
            let argument = fields.next().map(str::trim).filter(|a| !a.is_empty());

            match keyword {
                "console" => {
                    if argument.is_some() {
                        return Err(ConfigError::UnexpectedArgument);
                    }
                    if console {
                        return Err(ConfigError::DuplicateDirective);
                    }
                    console = true;
                }
                "session" => {
                    let path = argument.ok_or(ConfigError::MissingArgument)?;
                    if !path.starts_with('/') {
                        return Err(ConfigError::NotAbsolutePath);
                    }
                    if session.is_some() {
                        return Err(ConfigError::DuplicateDirective);
                    }
                    session = Some(path);
                }
                _ => return Err(ConfigError::UnknownDirective),
            }
        }

        if !console {
            return Err(ConfigError::ConsoleRequired);
        }
        let session = session.ok_or(ConfigError::SessionRequired)?;
        Ok(Self { session })
    }

    /// The absolute path of the program to launch as the user's session.
    #[must_use]
    pub fn session(&self) -> &'a str {
        self.session
    }
}

/// Return the portion of `line` before its first `#`, dropping an inline or
/// whole-line comment. A `session` path never contains `#`, so cutting at the
/// first one is unambiguous.
fn strip_comment(line: &str) -> &str {
    match line.find('#') {
        Some(index) => &line[..index],
        None => line,
    }
}

#[cfg(test)]
mod tests {
    use super::{ConfigError, StartupConfig, DEFAULT_CONFIG, MAX_CONFIG_LEN};

    extern crate alloc;
    use alloc::format;
    use alloc::string::String;

    #[test]
    fn default_config_parses_to_console_plus_login_session() {
        let config = StartupConfig::parse(DEFAULT_CONFIG).expect("default config parses");
        assert_eq!(config.session(), "/System/Services/login");
    }

    #[test]
    fn comments_blank_lines_and_inline_comments_are_ignored() {
        let text = "\
# a leading comment
\t
console   # open it
session /Apps/Shell.app/Run   # the shell
";
        let config = StartupConfig::parse(text).expect("config parses");
        assert_eq!(config.session(), "/Apps/Shell.app/Run");
    }

    #[test]
    fn surrounding_whitespace_is_tolerated() {
        let config =
            StartupConfig::parse("   console  \n   session    /Apps/Shell.app/Run   \n").unwrap();
        assert_eq!(config.session(), "/Apps/Shell.app/Run");
    }

    #[test]
    fn unknown_directive_fails_closed() {
        assert_eq!(
            StartupConfig::parse("console\nsession /x\nlaunch /y\n"),
            Err(ConfigError::UnknownDirective),
        );
    }

    #[test]
    fn duplicate_directive_is_rejected() {
        assert_eq!(
            StartupConfig::parse("console\nconsole\nsession /x\n"),
            Err(ConfigError::DuplicateDirective),
        );
        assert_eq!(
            StartupConfig::parse("console\nsession /x\nsession /y\n"),
            Err(ConfigError::DuplicateDirective),
        );
    }

    #[test]
    fn console_rejects_an_argument() {
        assert_eq!(
            StartupConfig::parse("console now\nsession /x\n"),
            Err(ConfigError::UnexpectedArgument),
        );
    }

    #[test]
    fn session_requires_an_absolute_path_argument() {
        assert_eq!(
            StartupConfig::parse("console\nsession\n"),
            Err(ConfigError::MissingArgument),
        );
        assert_eq!(
            StartupConfig::parse("console\nsession Apps/Shell.app/Run\n"),
            Err(ConfigError::NotAbsolutePath),
        );
    }

    #[test]
    fn a_required_directive_must_be_present() {
        assert_eq!(
            StartupConfig::parse("session /Apps/Shell.app/Run\n"),
            Err(ConfigError::ConsoleRequired),
        );
        assert_eq!(
            StartupConfig::parse("console\n"),
            Err(ConfigError::SessionRequired),
        );
        assert_eq!(StartupConfig::parse(""), Err(ConfigError::ConsoleRequired));
    }

    #[test]
    fn an_oversized_config_is_refused_before_scanning() {
        let mut text = String::from("console\nsession /Apps/Shell.app/Run\n");
        while text.len() <= MAX_CONFIG_LEN {
            text.push_str("# padding comment line\n");
        }
        assert_eq!(StartupConfig::parse(&text), Err(ConfigError::TooLong));
    }

    #[test]
    fn config_error_display_is_stable() {
        assert_eq!(
            format!("{}", ConfigError::UnknownDirective),
            "startup config names an unknown directive",
        );
    }
}
