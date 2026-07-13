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
//! * `session <path> <account>` — the absolute path of the program `init`
//!   launches as the user's session (the login service, today) and the
//!   compiled-in system account it runs as. Required exactly once.
//! * `service <path> <account>` — the absolute path of a long-running system
//!   service `init` launches once at startup and supervises for the life of
//!   the system, and the compiled-in system account it runs as. Optional and
//!   **repeatable**, up to [`MAX_SERVICES`]; the directives' order is the
//!   launch order.
//!
//! Every `session`/`service` directive names its account, and the parser
//! resolves the name onto its uid **at parse time** through the compiled-in
//! system identity (`rustos_users::system_account_uid`) — no volume, no
//! syscall, no waiting. A directive naming an unknown account rejects the
//! whole config ([`ConfigError::UnknownAccount`]): nothing is spawned from a
//! config whose identities cannot all be resolved (fail closed). PID 1 then
//! spawns each entry with that concrete `target_uid`, so every service runs
//! as its own unprivileged service account from its first instruction
//! (`plans/USERS.md`).

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
# Open the system console, launch the System Information, device-manager,
# and seat-manager services, and start the login service as the session —
# each under its own compiled-in service account (plans/USERS.md).
# `sysinfod` starts first so the introspection endpoint (`AGENTS.md` §16.6)
# is published before any client queries it; `seatmgr` (plans/DISPLAY.md D3)
# holds the seat-multiplexing authority.
console
service /System/Services/sysinfod.app/Run sysinfod
service /System/Services/devmgr.app/Run devmgr
service /System/Services/seatmgr.app/Run seatmgr
session /System/Services/login.app/Run login
";

/// The version prefix of the banner `init` writes once it reaches user mode.
///
/// The version is the workspace crate version stamped in at compile time
/// (`env!("CARGO_PKG_VERSION")`). No wall-clock build date or timestamp is
/// embedded: the images must stay bit-reproducible, and a build clock would
/// defeat that — the source-fixed version is the honest, reproducible build
/// identity. The machine facts appended by [`render_banner`] come from the
/// kernel-attested `boot_facts_get` answer, never a compiled-in guess.
const BANNER_PREFIX: &str = concat!("RustOS ", env!("CARGO_PKG_VERSION"));

/// Byte capacity of the [`render_banner`] output buffer.
///
/// The banner is bounded by construction: the fixed prefix and layout text,
/// a `u64` memory figure (at most 20 digits), a CPU name of at most
/// [`rustos_abi::CPU_NAME_LEN`] (48) bytes — or the fallback's
/// `Unknown `/` processor` text around the longest arch name — and a `u32`
/// core count (at most 10 digits) fit comfortably; 160 bytes leaves honest
/// headroom without a heap. A `Write` overflow is impossible for
/// well-formed inputs and fails closed (truncation refused) rather than
/// corrupting the text.
pub const BANNER_MAX: usize = 160;

/// One binary mebibyte, the banner's default memory unit.
const MIB: u64 = 1024 * 1024;

/// One binary gibibyte; the banner switches to GiB only above
/// [`GIB_THRESHOLD`].
const GIB: u64 = 1024 * MIB;

/// Installed-memory sizes strictly above this render in GiB; everything at
/// or below it renders in MiB (`8192MiB`, not `8GiB`).
const GIB_THRESHOLD: u64 = 100 * GIB;

/// A bounded, allocation-free `core::fmt::Write` sink over a caller-owned
/// byte buffer. Refuses (fails closed) any write past the buffer's end
/// rather than truncating mid-text.
struct BufWriter<'a> {
    buf: &'a mut [u8],
    len: usize,
}

impl core::fmt::Write for BufWriter<'_> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let bytes = s.as_bytes();
        let end = self.len.checked_add(bytes.len()).ok_or(core::fmt::Error)?;
        if end > self.buf.len() {
            return Err(core::fmt::Error);
        }
        self.buf[self.len..end].copy_from_slice(bytes);
        self.len = end;
        Ok(())
    }
}

/// Render the startup banner into `buf`, returning the text.
///
/// With the kernel's boot facts available the banner is the machine
/// summary:
///
/// ```text
/// RustOS 0.0.0: 8192MiB
///
/// ARM Cortex-A72, 4 cores
/// ```
///
/// The installed memory renders in whole MiB (rounded to nearest) unless it
/// exceeds 100 GiB, where whole GiB keep the figure readable; a single core
/// reads `1 core`. A kernel that discovered no CPU model reports the honest
/// fallback `Unknown <arch> processor, <n> cores`. Without facts (a kernel
/// that installed none) the banner degrades to the version line alone —
/// the honest output, never a fabricated machine shape. A formatting
/// overflow (impossible for well-formed inputs) equally degrades to the
/// version line, fail closed.
pub fn render_banner(facts: Option<rustos_abi::BootFacts>, buf: &mut [u8; BANNER_MAX]) -> &str {
    use core::fmt::Write as _;

    let mut w = BufWriter { buf, len: 0 };
    let ok = match facts {
        Some(facts) => write_facts_banner(&mut w, &facts).is_ok(),
        None => writeln!(w, "{BANNER_PREFIX}").is_ok(),
    };
    if !ok {
        // Fail closed to the version line alone; it always fits.
        w.len = 0;
        if writeln!(w, "{BANNER_PREFIX}").is_err() {
            w.len = 0;
        }
    }
    let len = w.len;
    // The writer only ever copies whole `&str`s, so the prefix is UTF-8.
    core::str::from_utf8(&buf[..len]).unwrap_or("")
}

/// Write the full machine-summary banner (see [`render_banner`]).
fn write_facts_banner(w: &mut BufWriter<'_>, facts: &rustos_abi::BootFacts) -> core::fmt::Result {
    use core::fmt::Write as _;

    write!(w, "{BANNER_PREFIX}: ")?;
    if facts.memory_bytes > GIB_THRESHOLD {
        // Round to the nearest whole GiB; the saturating add cannot
        // overflow below u64::MAX - GIB/2, far past any real machine.
        let gib = facts.memory_bytes.saturating_add(GIB / 2) / GIB;
        write!(w, "{gib}GiB")?;
    } else {
        let mib = facts.memory_bytes.saturating_add(MIB / 2) / MIB;
        write!(w, "{mib}MiB")?;
    }
    let cores = facts.cpu_count;
    let noun = if cores == 1 { "core" } else { "cores" };
    match facts.cpu_name.as_str() {
        Some(name) => write!(w, "\n\n{name}, {cores} {noun}\n"),
        None => write!(
            w,
            "\n\nUnknown {} processor, {cores} {noun}\n",
            facts.arch.name()
        ),
    }
}

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
    /// A `session` or `service` directive named no account.
    MissingAccount,
    /// A `session` or `service` directive named an account outside the
    /// compiled-in system identity, so no uid can be resolved for it.
    UnknownAccount,
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
            Self::MissingAccount => "a startup directive names no account",
            Self::UnknownAccount => "a startup directive names an unknown system account",
            Self::ConsoleRequired => "startup config omits the required `console` directive",
            Self::SessionRequired => "startup config omits the required `session` directive",
            Self::TooManyServices => "startup config declares too many `service` directives",
        };
        f.write_str(message)
    }
}

/// One validated launch entry: the program path and the uid of the
/// compiled-in system account it runs as, resolved at parse time.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Launch<'a> {
    /// The absolute path of the program to launch.
    pub path: &'a str,
    /// The concrete `target_uid` the entry is spawned with — the
    /// compiled-in system account the directive named.
    pub uid: u32,
}

/// A parsed, validated startup configuration borrowing from its source text.
///
/// Construct one with [`StartupConfig::parse`]. The borrow keeps the parser
/// allocation-free: the [`session`](Self::session) path and every
/// [`services`](Self::services) path point into the config text the caller
/// supplied and are valid for as long as that text lives.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct StartupConfig<'a> {
    session: Launch<'a>,
    /// The declared `service` entries in declaration (launch) order; only
    /// the first [`service_count`](Self::service_count) entries are
    /// populated.
    services: [Launch<'a>; MAX_SERVICES],
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
    /// `service` path, omits or fails to resolve an account name, declares
    /// more than [`MAX_SERVICES`] services, or omits a required directive.
    /// The parser fails closed: a config it cannot fully understand — or
    /// whose identities it cannot fully resolve — yields no
    /// [`StartupConfig`].
    pub fn parse(text: &'a str) -> Result<Self, ConfigError> {
        const EMPTY: Launch<'_> = Launch { path: "", uid: 0 };
        if text.len() > MAX_CONFIG_LEN {
            return Err(ConfigError::TooLong);
        }

        let mut console = false;
        let mut session: Option<Launch<'a>> = None;
        let mut services: [Launch<'a>; MAX_SERVICES] = [EMPTY; MAX_SERVICES];
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
                    let launch = parse_launch(argument.ok_or(ConfigError::MissingArgument)?)?;
                    if session.is_some() {
                        return Err(ConfigError::DuplicateDirective);
                    }
                    session = Some(launch);
                }
                "service" => {
                    let launch = parse_launch(argument.ok_or(ConfigError::MissingArgument)?)?;
                    // `service` is repeatable; overflow fails closed rather
                    // than overrunning the fixed array.
                    if service_count >= MAX_SERVICES {
                        return Err(ConfigError::TooManyServices);
                    }
                    services[service_count] = launch;
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

    /// The user-session launch entry: the program path and the resolved
    /// uid of the account it runs as.
    #[must_use]
    pub fn session(&self) -> Launch<'a> {
        self.session
    }

    /// The long-running services `init` launches once at startup and
    /// supervises, in declaration (launch) order — each entry the program
    /// path plus its resolved account uid. Empty when the config declares
    /// none.
    #[must_use]
    pub fn services(&self) -> &[Launch<'a>] {
        &self.services[..self.service_count]
    }
}

/// Parse one `session`/`service` argument — `<path> <account>` — into a
/// validated [`Launch`]: the path must be absolute, the account name must
/// resolve against the compiled-in system identity, and nothing may
/// follow the account.
fn parse_launch(argument: &str) -> Result<Launch<'_>, ConfigError> {
    let mut fields = argument.split_whitespace();
    let path = fields.next().ok_or(ConfigError::MissingArgument)?;
    let account = fields.next().ok_or(ConfigError::MissingAccount)?;
    if fields.next().is_some() {
        return Err(ConfigError::UnexpectedArgument);
    }
    if !path.starts_with('/') {
        return Err(ConfigError::NotAbsolutePath);
    }
    let uid = rustos_users::system_account_uid(account).ok_or(ConfigError::UnknownAccount)?;
    Ok(Launch { path, uid: uid.0 })
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
    use super::{ConfigError, Launch, StartupConfig, DEFAULT_CONFIG, MAX_CONFIG_LEN, MAX_SERVICES};

    extern crate alloc;
    use alloc::format;
    use alloc::string::String;
    use core::fmt::Write as _;

    #[test]
    fn default_config_parses_to_console_login_session_and_the_startup_services() {
        let config = StartupConfig::parse(DEFAULT_CONFIG).expect("default config parses");
        // Every service is a `<name>.app` bundle in the service store (a
        // service is an app), so the config names each bundle's `Run`
        // binary — and each entry's account resolves to the compiled-in
        // service uid it runs as (`plans/USERS.md`).
        assert_eq!(
            config.session(),
            Launch {
                path: "/System/Services/login.app/Run",
                uid: rustos_users::LOGIN_UID.0,
            }
        );
        // `sysinfod` is launched before `devmgr` so the introspection endpoint
        // is published before any client queries it.
        assert_eq!(
            config.services(),
            &[
                Launch {
                    path: "/System/Services/sysinfod.app/Run",
                    uid: rustos_users::SYSINFOD_UID.0,
                },
                Launch {
                    path: "/System/Services/devmgr.app/Run",
                    uid: rustos_users::DEVMGR_UID.0,
                },
                Launch {
                    path: "/System/Services/seatmgr.app/Run",
                    uid: rustos_users::SEATMGR_UID.0,
                },
            ],
        );
    }

    #[test]
    fn a_config_without_a_service_directive_has_no_services() {
        let config = StartupConfig::parse("console\nsession /Apps/Shell.app/Run login\n").unwrap();
        assert!(config.services().is_empty());
    }

    #[test]
    fn service_directives_are_collected_in_declaration_order() {
        let config = StartupConfig::parse(
            "console\nservice /System/Services/devmgr.app/Run devmgr\nservice /System/Services/netd sysinfod\nsession /x login\n",
        )
        .expect("config parses");
        assert_eq!(
            config.services(),
            &[
                Launch {
                    path: "/System/Services/devmgr.app/Run",
                    uid: rustos_users::DEVMGR_UID.0,
                },
                Launch {
                    path: "/System/Services/netd",
                    uid: rustos_users::SYSINFOD_UID.0,
                },
            ],
        );
    }

    #[test]
    fn a_service_path_must_be_absolute() {
        assert_eq!(
            StartupConfig::parse("console\nservice devmgr devmgr\nsession /x login\n"),
            Err(ConfigError::NotAbsolutePath),
        );
        assert_eq!(
            StartupConfig::parse("console\nservice\nsession /x login\n"),
            Err(ConfigError::MissingArgument),
        );
    }

    #[test]
    fn a_directive_without_an_account_fails_closed() {
        assert_eq!(
            StartupConfig::parse("console\nsession /x\n"),
            Err(ConfigError::MissingAccount),
        );
        assert_eq!(
            StartupConfig::parse(
                "console\nservice /System/Services/devmgr.app/Run\nsession /x login\n"
            ),
            Err(ConfigError::MissingAccount),
        );
    }

    #[test]
    fn an_unknown_account_fails_closed() {
        // Neither an arbitrary name nor a *human* account resolves: only
        // the compiled-in system identity can run a boot service.
        assert_eq!(
            StartupConfig::parse("console\nsession /x nobody\n"),
            Err(ConfigError::UnknownAccount),
        );
        assert_eq!(
            StartupConfig::parse("console\nservice /svc root\nsession /x login\n"),
            Err(ConfigError::UnknownAccount),
        );
    }

    #[test]
    fn trailing_fields_after_the_account_fail_closed() {
        assert_eq!(
            StartupConfig::parse("console\nsession /x login extra\n"),
            Err(ConfigError::UnexpectedArgument),
        );
    }

    #[test]
    fn more_than_max_services_fails_closed() {
        let mut text = String::from("console\nsession /x login\n");
        for n in 0..=MAX_SERVICES {
            let _ = writeln!(text, "service /System/Services/s{n} devmgr");
        }
        assert_eq!(
            StartupConfig::parse(&text),
            Err(ConfigError::TooManyServices),
        );
    }

    #[test]
    fn exactly_max_services_is_accepted() {
        let mut text = String::from("console\nsession /x login\n");
        for n in 0..MAX_SERVICES {
            let _ = writeln!(text, "service /System/Services/s{n} devmgr");
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
session /Apps/Shell.app/Run login   # the login service
";
        let config = StartupConfig::parse(text).expect("config parses");
        assert_eq!(config.session().path, "/Apps/Shell.app/Run");
    }

    #[test]
    fn surrounding_whitespace_is_tolerated() {
        let config =
            StartupConfig::parse("   console  \n   session    /Apps/Shell.app/Run  login \n")
                .unwrap();
        assert_eq!(config.session().path, "/Apps/Shell.app/Run");
    }

    #[test]
    fn unknown_directive_fails_closed() {
        assert_eq!(
            StartupConfig::parse("console\nsession /x login\nlaunch /y\n"),
            Err(ConfigError::UnknownDirective),
        );
    }

    #[test]
    fn duplicate_directive_is_rejected() {
        assert_eq!(
            StartupConfig::parse("console\nconsole\nsession /x login\n"),
            Err(ConfigError::DuplicateDirective),
        );
        assert_eq!(
            StartupConfig::parse("console\nsession /x login\nsession /y login\n"),
            Err(ConfigError::DuplicateDirective),
        );
    }

    #[test]
    fn console_rejects_an_argument() {
        assert_eq!(
            StartupConfig::parse("console now\nsession /x login\n"),
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
            StartupConfig::parse("console\nsession System/Apps/elsh.app/Run login\n"),
            Err(ConfigError::NotAbsolutePath),
        );
    }

    #[test]
    fn a_required_directive_must_be_present() {
        assert_eq!(
            StartupConfig::parse("session /Apps/Shell.app/Run login\n"),
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
        let mut text = String::from("console\nsession /Apps/Shell.app/Run login\n");
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

    use super::{render_banner, BANNER_MAX};
    use rustos_abi::{Arch, BootFacts, CpuName};

    /// Render with the given facts into a fresh buffer.
    fn banner(facts: Option<BootFacts>) -> String {
        let mut buf = [0u8; BANNER_MAX];
        String::from(render_banner(facts, &mut buf))
    }

    /// A [`CpuName`] from a string the tests know is well formed.
    fn cpu_name(name: &str) -> CpuName {
        CpuName::new(name).expect("valid name")
    }

    #[test]
    fn banner_renders_memory_cpu_name_and_cores() {
        let facts = BootFacts {
            arch: Arch::X86_64,
            cpu_name: cpu_name("Intel(R) Xeon(R) CPU E5-2690 v4 @ 2.60GHz"),
            cpu_count: 36,
            memory_bytes: 8192 * 1024 * 1024,
        };
        assert_eq!(
            banner(Some(facts)),
            format!(
                "RustOS {}: 8192MiB\n\nIntel(R) Xeon(R) CPU E5-2690 v4 @ 2.60GHz, 36 cores\n",
                env!("CARGO_PKG_VERSION"),
            ),
        );
    }

    #[test]
    fn banner_falls_back_to_unknown_processor_without_a_cpu_name() {
        let facts = BootFacts {
            arch: Arch::Riscv64,
            cpu_name: CpuName::UNKNOWN,
            cpu_count: 2,
            memory_bytes: 512 * 1024 * 1024,
        };
        let text = banner(Some(facts));
        assert!(
            text.ends_with("Unknown riscv64 processor, 2 cores\n"),
            "{text}"
        );
    }

    #[test]
    fn banner_uses_mib_up_to_the_100_gib_threshold() {
        // Exactly 100 GiB still renders in MiB ("over 100G" is strict).
        let facts = BootFacts {
            arch: Arch::Aarch64,
            cpu_name: cpu_name("ARM Cortex-A72"),
            cpu_count: 4,
            memory_bytes: 100 * 1024 * 1024 * 1024,
        };
        assert!(banner(Some(facts)).contains(": 102400MiB\n"));
    }

    #[test]
    fn banner_switches_to_gib_above_the_threshold() {
        let facts = BootFacts {
            arch: Arch::Riscv64,
            cpu_name: cpu_name("SiFive U74-MC"),
            cpu_count: 128,
            memory_bytes: 256 * 1024 * 1024 * 1024,
        };
        let text = banner(Some(facts));
        assert!(text.contains(": 256GiB\n"), "{text}");
        assert!(text.ends_with("SiFive U74-MC, 128 cores\n"), "{text}");
    }

    #[test]
    fn banner_rounds_memory_to_the_nearest_unit() {
        // 8 GiB minus 100 KiB rounds back up to 8192 MiB — the figure a
        // user recognises as installed, not a truncated 8191.
        let facts = BootFacts {
            arch: Arch::Aarch64,
            cpu_name: cpu_name("ARM Cortex-A72"),
            cpu_count: 4,
            memory_bytes: 8192 * 1024 * 1024 - 100 * 1024,
        };
        assert!(banner(Some(facts)).contains(": 8192MiB\n"));
    }

    #[test]
    fn banner_uses_the_singular_for_one_core() {
        let facts = BootFacts {
            arch: Arch::Aarch64,
            cpu_name: cpu_name("ARM Cortex-A53"),
            cpu_count: 1,
            memory_bytes: 128 * 1024 * 1024,
        };
        let text = banner(Some(facts));
        assert!(text.ends_with("ARM Cortex-A53, 1 core\n"), "{text}");
    }

    #[test]
    fn banner_without_facts_degrades_to_the_version_line() {
        assert_eq!(
            banner(None),
            format!("RustOS {}\n", env!("CARGO_PKG_VERSION")),
        );
    }

    #[test]
    fn banner_always_fits_its_buffer() {
        // The worst representable inputs — a maximum-length CPU name,
        // saturated counts — still fit BANNER_MAX, so the fail-closed
        // truncation refusal can never fire for real facts.
        let facts = BootFacts {
            arch: Arch::Riscv64,
            cpu_name: cpu_name(&"x".repeat(rustos_abi::CPU_NAME_LEN)),
            cpu_count: u32::MAX,
            memory_bytes: u64::MAX,
        };
        let text = banner(Some(facts));
        assert!(text.starts_with("RustOS "));
        assert!(text.ends_with(" cores\n"), "{text}");
        // The unknown-name fallback's worst case fits too.
        let facts = BootFacts {
            cpu_name: CpuName::UNKNOWN,
            ..facts
        };
        let text = banner(Some(facts));
        assert!(text.ends_with(" cores\n"), "{text}");
    }
}
