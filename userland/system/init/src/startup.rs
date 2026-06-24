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
//! The config text is **untrusted input**: it is the
//! first thing a freshly spawned program parses, so the parser is
//! allocation-free, borrows from the caller's text rather than copying, and
//! **fails closed** ([`ConfigError`]) on anything it does not understand — an
//! unknown directive, a duplicate, a missing or malformed argument, or a
//! config that omits a required directive. A surprising or partial startup
//! configuration never boots a surprising system.
//!
//! # Grammar
//!
//! The text is a sequence of lines. A `#` begins a comment that runs to the
//! end of the line; blank lines and comment-only lines are ignored. Every
//! other line is a directive: a keyword optionally followed by whitespace and
//! a single argument. Three directives are defined:
//!
//! * `console` — open the system console so the banner and later output have
//!   somewhere to go. Takes no argument.
//! * `session <path>` — the absolute path of the program `init` launches as
//!   the user's session (the shell, today). The argument must be an absolute
//!   path (bundle layout). Required exactly once.
//! * `service <path>` — the absolute path of a long-running system service
//!   `init` launches once at startup and supervises for the life of the
//!   system (the device manager, today). The
//!   argument must be an absolute path. Optional and **repeatable**, up to
//!   [`MAX_SERVICES`]; the directives' order is the launch order.

use core::fmt;

/// Maximum length, in bytes, of a startup config text [`StartupConfig::parse`]
/// will consider. A larger input is refused outright ([`ConfigError::TooLong`])
/// rather than scanned — the config `init` carries is tiny, and an
/// unboundedly large one is a defect, not a workload.
pub const MAX_CONFIG_LEN: usize = 4096;

/// Maximum number of `service` directives a startup config may declare.
///
/// A fixed, allocation-free bound for the no-heap PID 1 (`plans/SPAWN.md`
/// `SP5b` — the userland heap producer is still staged): the parsed service
/// paths live in a stack array borrowing the config text. The compiled-in
/// [`DEFAULT_CONFIG`] declares one (`devmgr`); the small headroom leaves
/// room for the session/login-adjacent services later stages add without a
/// heap. A config that declares more fails closed
/// ([`ConfigError::TooManyServices`]) rather than overrunning the array.
pub const MAX_SERVICES: usize = 4;

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
# Open the system console, launch the device-manager service, and start the
# login service as the session.
console
service /System/Services/devmgr
session /System/Services/login
";

/// The first line `init` writes to the console once it reaches user mode.
///
/// A fixed, terse banner (no aimless waffle) that proves the
/// kernel reached EL0 and `init`'s console write path works end to end.
pub const BANNER: &str = "RustOS init: reached user mode\n";

/// Why a startup config text was refused.
///
/// Every variant is a fail-closed refusal: [`StartupConfig::parse`] returns it
/// and the caller starts nothing, rather than guess at a malformed intent.
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
    /// A `session` or `service` path was not absolute (did not begin with `/`).
    NotAbsolutePath,
    /// The required `console` directive was absent.
    ConsoleRequired,
    /// The required `session` directive was absent.
    SessionRequired,
    /// More than [`MAX_SERVICES`] `service` directives were declared.
    TooManyServices,
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
            Self::TooManyServices => "startup config declares too many `service` directives",
        };
        f.write_str(message)
    }
}

/// A parsed, validated startup configuration borrowing from its source text.
///
/// Construct one with [`StartupConfig::parse`]. The borrow keeps the parser
/// allocation-free: the [`session`](Self::session) path and every
/// [`services`](Self::services) path point into the config text the caller
/// supplied and are valid for as long as that text lives.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct StartupConfig<'a> {
    session: &'a str,
    /// The declared `service` paths in declaration (launch) order; only the
    /// first [`service_count`](Self::service_count) entries are populated.
    services: [&'a str; MAX_SERVICES],
    /// How many of [`services`](Self::services) are populated.
    service_count: usize,
}

impl<'a> StartupConfig<'a> {
    /// Parse and validate a startup config `text`.
    ///
    /// # Errors
    ///
    /// Returns the matching [`ConfigError`] if `text` exceeds
    /// [`MAX_CONFIG_LEN`], contains an unknown or duplicated directive, gives a
    /// directive the wrong arguments, carries a non-absolute `session` /
    /// `service` path, declares more than [`MAX_SERVICES`] services, or omits a
    /// required directive. The parser fails closed: a config it cannot fully
    /// understand yields no [`StartupConfig`].
    pub fn parse(text: &'a str) -> Result<Self, ConfigError> {
        if text.len() > MAX_CONFIG_LEN {
            return Err(ConfigError::TooLong);
        }

        let mut console = false;
        let mut session: Option<&'a str> = None;
        let mut services: [&'a str; MAX_SERVICES] = [""; MAX_SERVICES];
        let mut service_count = 0usize;

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
                "service" => {
                    let path = argument.ok_or(ConfigError::MissingArgument)?;
                    if !path.starts_with('/') {
                        return Err(ConfigError::NotAbsolutePath);
                    }
                    // `service` is repeatable; overflow fails closed rather
                    // than overrunning the fixed array.
                    if service_count >= MAX_SERVICES {
                        return Err(ConfigError::TooManyServices);
                    }
                    services[service_count] = path;
                    service_count += 1;
                }
                _ => return Err(ConfigError::UnknownDirective),
            }
        }

        if !console {
            return Err(ConfigError::ConsoleRequired);
        }
        let session = session.ok_or(ConfigError::SessionRequired)?;
        Ok(Self {
            session,
            services,
            service_count,
        })
    }

    /// The absolute path of the program to launch as the user's session.
    #[must_use]
    pub fn session(&self) -> &'a str {
        self.session
    }

    /// The absolute paths of the long-running services `init` launches once
    /// at startup and supervises, in declaration (launch) order. Empty when
    /// the config declares none.
    #[must_use]
    pub fn services(&self) -> &[&'a str] {
        &self.services[..self.service_count]
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
    use super::{ConfigError, StartupConfig, DEFAULT_CONFIG, MAX_CONFIG_LEN, MAX_SERVICES};

    extern crate alloc;
    use alloc::format;
    use alloc::string::String;
    use core::fmt::Write as _;

    #[test]
    fn default_config_parses_to_console_login_session_and_the_devmgr_service() {
        let config = StartupConfig::parse(DEFAULT_CONFIG).expect("default config parses");
        assert_eq!(config.session(), "/System/Services/login");
        assert_eq!(config.services(), &["/System/Services/devmgr"]);
    }

    #[test]
    fn a_config_without_a_service_directive_has_no_services() {
        let config = StartupConfig::parse("console\nsession /Apps/Shell.app/Run\n").unwrap();
        assert!(config.services().is_empty());
    }

    #[test]
    fn service_directives_are_collected_in_declaration_order() {
        let config = StartupConfig::parse(
            "console\nservice /System/Services/devmgr\nservice /System/Services/netd\nsession /x\n",
        )
        .expect("config parses");
        assert_eq!(
            config.services(),
            &["/System/Services/devmgr", "/System/Services/netd"],
        );
    }

    #[test]
    fn a_service_path_must_be_absolute() {
        assert_eq!(
            StartupConfig::parse("console\nservice devmgr\nsession /x\n"),
            Err(ConfigError::NotAbsolutePath),
        );
        assert_eq!(
            StartupConfig::parse("console\nservice\nsession /x\n"),
            Err(ConfigError::MissingArgument),
        );
    }

    #[test]
    fn more_than_max_services_fails_closed() {
        let mut text = String::from("console\nsession /x\n");
        for n in 0..=MAX_SERVICES {
            let _ = writeln!(text, "service /System/Services/s{n}");
        }
        assert_eq!(
            StartupConfig::parse(&text),
            Err(ConfigError::TooManyServices),
        );
    }

    #[test]
    fn exactly_max_services_is_accepted() {
        let mut text = String::from("console\nsession /x\n");
        for n in 0..MAX_SERVICES {
            let _ = writeln!(text, "service /System/Services/s{n}");
        }
        let config = StartupConfig::parse(&text).expect("exactly the bound parses");
        assert_eq!(config.services().len(), MAX_SERVICES);
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
