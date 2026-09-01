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
//!   launch order. These are the irreducible **bootstrap floor**: services
//!   the machine needs before the registration store can mean anything, so
//!   enrolment does not govern them.
//! * `enrolled <path> <account>` — the same, for a service the **enrolment
//!   record** governs. It is registered only if the effective enrolment (the
//!   image's layer with the administrator's overrides applied) enables it, so
//!   an administrator can turn it off durably. Optional and **repeatable**,
//!   up to [`MAX_ENROLLED_SERVICES`].
//!
//! Every `session`/`service` directive names its account, and the parser
//! resolves the name onto its uid **at parse time** through the compiled-in
//! system identity (`tairix_users::system_account_uid`) — no volume, no
//! syscall, no waiting. A directive naming an unknown account rejects the
//! whole config ([`ConfigError::UnknownAccount`]): nothing is spawned from a
//! config whose identities cannot all be resolved (fail closed). PID 1 then
//! spawns each entry with that concrete `target_uid`, so every service runs
//! as its own unprivileged service account from its first instruction
//! (`plans/USERS.md`).

use core::fmt;

use tairix_util::conf::strip_comment;

/// Maximum length, in bytes, of a startup config text [`StartupConfig::parse`]
/// will consider. A larger input is refused outright ([`ConfigError::TooLong`])
/// rather than scanned — the config `init` carries is tiny, and an
/// unboundedly large one is a defect, not a workload.
pub const MAX_CONFIG_LEN: usize = 4096;

/// The boot service floor: how many `service` directives the compiled-in
/// [`DEFAULT_CONFIG`] declares, and thus the size of the allocation-free
/// array [`StartupConfig::parse`] borrows the parsed service paths into for
/// the no-heap PID 1 (`plans/SPAWN.md` `SP5b` — the userland heap producer is
/// still staged).
///
/// This is a **bound dictated by the floor**, not a hand-picked cap: it is
/// derived from the floor text itself by [`count_keyword_directives`], so
/// adding a `service` directive to [`DEFAULT_CONFIG`] grows the bound with it
/// — the floor can never overrun the array nor be silently truncated by a
/// stale magic number. A config that declares *more* `service` directives
/// than the floor does fails closed ([`ConfigError::TooManyServices`]) rather
/// than overrunning the array; the growable, discovery-registered tier past
/// the floor lands with the userland heap (`plans/NEW-SERVICEMANAGER.md`
/// §3.10).
pub const MAX_SERVICES: usize = count_keyword_directives(DEFAULT_CONFIG, b"service");

/// The enrolment-governed tier's size: how many `enrolled` directives the
/// compiled-in [`DEFAULT_CONFIG`] declares.
///
/// Derived from the floor text exactly as [`MAX_SERVICES`] is, so adding an
/// `enrolled` directive grows the bound with it.
pub const MAX_ENROLLED_SERVICES: usize = count_keyword_directives(DEFAULT_CONFIG, b"enrolled");

/// The startup configuration compiled into the `init` `Run` binary.
///
/// The session is the login service (`plans/PI.md` P11): every text console
/// sits at a `login:` prompt, and the authenticated account's shell of
/// choice is started by login, never directly by `init`. Later stages
/// replace the compiled-in default with a config read from
/// `/System/Settings` once a filesystem is mounted; the parser does not
/// change.
pub const DEFAULT_CONFIG: &str = "\
# TAIRiX PID 1 startup configuration (plans/PI.md P6b / P11).
# Open the system console, launch the System Information, network-stack,
# device-manager, seat-manager, app-data, and time services, and start the
# login service as the session — each under its own compiled-in service
# account (plans/USERS.md). `sysinfod` starts first so the introspection
# endpoint (`AGENTS.md` §16.6) is published before any client queries it;
# `netstack` (plans/NETWORK.md) owns the network interfaces and is launched
# before `devmgr` so it is ready to receive the NIC device channels `devmgr`
# binds to it; `seatmgr` (plans/DISPLAY.md D3) holds the seat-multiplexing
# authority; `confd` (plans/APPDATA.md) owns every application's settings
# store and is a boot-floor service because a headless machine needs it as
# much as a desktop does — it binds its endpoint straight away and answers a
# typed refusal until the encrypted root is unlocked. `timed`
# (plans/TIMESYNC.md) is last, and is the one entry the enrolment record
# governs rather than the bootstrap floor: a machine with no RTC boots knowing
# nothing about the time, and every audit-log hash chain, filesystem timestamp,
# and certificate lifetime rests on the clock it establishes, but automatic
# time-setting is a thing a user may turn off, so it is `enrolled` and an
# administrator's `disable` survives a reboot. It starts after `netstack` so
# the interfaces exist by the time its first query is due, and needs no
# readiness gate of its own — a query it cannot send simply fails and its
# bounded backoff paces the retry.
console
service /System/Services/sysinfod.app/Run sysinfod
service /System/Services/netstack.app/Run netstack
service /System/Services/devmgr.app/Run devmgr
service /System/Services/seatmgr.app/Run seatmgr
service /System/Services/confd.app/Run confd
enrolled /System/Services/timed.app/Run timed
session /System/Services/login.app/Run login
";

/// Byte capacity of the [`render_banner`] output buffer.
///
/// The banner is bounded by construction: the fixed layout text, a CPU name
/// of at most [`tairix_abi::CPU_NAME_LEN`] (48) bytes — or the fallback's
/// `Unknown `/` processor` text around the longest arch name — and a `u32`
/// core count (at most 10 digits) fit comfortably; 160 bytes leaves honest
/// headroom without a heap. A `Write` overflow is impossible for
/// well-formed inputs and fails closed (truncation refused) rather than
/// corrupting the text.
pub const BANNER_MAX: usize = 160;

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

/// Render the startup machine-summary line into `buf`, returning the text.
///
/// The kernel's early-boot RAM self-test already drew the identity line —
/// `TAIRiX <version> <RAM>MiB` — as it verified the installed memory (see
/// `tairix_kernel_core::memtest`), so `init` does **not** repeat the version
/// or the RAM figure. It adds only the processor line beneath it:
///
/// ```text
/// TAIRiX 0.0.0 8192MiB      (drawn by the kernel)
///
/// ARM Cortex-A72, 4 cores   (this line)
/// ```
///
/// A single core reads `1 core`; a kernel that discovered no CPU model
/// reports the honest fallback `Unknown <arch> processor, <n> cores`. Without
/// boot facts there is no machine shape to add, so the banner is empty — the
/// honest output, never a fabricated line. A formatting overflow (impossible
/// for well-formed inputs) equally degrades to empty, fail closed.
pub fn render_banner(facts: Option<tairix_abi::BootFacts>, buf: &mut [u8; BANNER_MAX]) -> &str {
    let mut w = BufWriter { buf, len: 0 };
    let ok = match facts {
        Some(facts) => write_facts_banner(&mut w, &facts).is_ok(),
        // No facts: the kernel already drew the identity line; there is
        // nothing to add.
        None => true,
    };
    if !ok {
        // Fail closed to no line rather than a partial one.
        w.len = 0;
    }
    let len = w.len;
    // The writer only ever copies whole `&str`s, so the text is UTF-8.
    core::str::from_utf8(&buf[..len]).unwrap_or("")
}

/// Write the machine-summary line beneath the kernel's identity line (see
/// [`render_banner`]). The leading blank line separates it from the kernel's
/// RAM line above.
fn write_facts_banner(w: &mut BufWriter<'_>, facts: &tairix_abi::BootFacts) -> core::fmt::Result {
    use core::fmt::Write as _;

    let cores = facts.cpu_count;
    let noun = if cores == 1 { "core" } else { "cores" };
    match facts.cpu_name.as_str() {
        Some(name) => write!(w, "\n{name}, {cores} {noun}\n"),
        None => write!(
            w,
            "\nUnknown {} processor, {cores} {noun}\n",
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
    /// The declared `enrolled` entries in declaration order; only the first
    /// [`enrolled_count`](Self::enrolled_count) entries are populated.
    enrolled: [Launch<'a>; MAX_ENROLLED_SERVICES],
    /// How many of [`enrolled`](Self::enrolled) are populated.
    enrolled_count: usize,
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
        let mut enrolled: [Launch<'a>; MAX_ENROLLED_SERVICES] = [EMPTY; MAX_ENROLLED_SERVICES];
        let mut enrolled_count = 0usize;

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
                "enrolled" => {
                    let launch = parse_launch(argument.ok_or(ConfigError::MissingArgument)?)?;
                    if enrolled_count >= MAX_ENROLLED_SERVICES {
                        return Err(ConfigError::TooManyServices);
                    }
                    enrolled[enrolled_count] = launch;
                    enrolled_count += 1;
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
            enrolled,
            enrolled_count,
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

    /// The enrolment-governed services, in declaration order. Each is
    /// registered only if the effective enrolment enables it, so an
    /// administrator's `disable` survives a reboot. Empty when the config
    /// declares none.
    #[must_use]
    pub fn enrolled(&self) -> &[Launch<'a>] {
        &self.enrolled[..self.enrolled_count]
    }
}

/// Derive a service's short name from its bundle `Run` path.
///
/// A boot-floor service is named by its `.app` bundle directory:
/// `/System/Services/devmgr.app/Run` yields `devmgr`. That name is what the
/// service-manager engine uses to express dependencies and to label audit
/// records, so it must be stable and unique across the floor — which bundle
/// names are, since two bundles cannot share a directory.
///
/// The first path component ending in `.app` names the bundle; the text
/// before `.app` is the service name. A path with no non-empty `.app`
/// component falls back to the whole path, so a mis-shaped entry is still
/// named (and still unique, since paths are unique) rather than silently
/// empty — fail loud, never fabricate.
#[must_use]
pub fn service_name(path: &str) -> &str {
    for component in path.split('/') {
        if let Some(stem) = component.strip_suffix(".app") {
            if !stem.is_empty() {
                return stem;
            }
        }
    }
    path
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
    let uid = tairix_users::system_account_uid(account).ok_or(ConfigError::UnknownAccount)?;
    Ok(Launch { path, uid: uid.0 })
}

/// Count the `service` directives a startup config `text` declares.
///
/// This mirrors [`StartupConfig::parse`]'s tokenisation — each line has any
/// `#`-comment stripped, its surrounding whitespace trimmed, and its first
/// whitespace-delimited token taken as the keyword — closely enough for the
/// ASCII [`DEFAULT_CONFIG`] it is used on. It runs in `const` context so
/// [`MAX_SERVICES`] is the boot floor's own size rather than a hand-picked
/// number; a unit test asserts it agrees with the real parser on
/// [`DEFAULT_CONFIG`], so the two tokenisers can never silently drift for the
/// floor.
const fn count_keyword_directives(text: &str, keyword: &[u8]) -> usize {
    let bytes = text.as_bytes();
    let len = bytes.len();
    let mut count = 0;
    let mut line_start = 0;
    let mut i = 0;
    while i <= len {
        if i == len || bytes[i] == b'\n' {
            if line_keyword_is(bytes, line_start, i, keyword) {
                count += 1;
            }
            line_start = i + 1;
        }
        i += 1;
    }
    count
}

/// Whether the config line `bytes[start..end]`'s keyword is `keyword`: its
/// keyword — the first whitespace-delimited token, after a `#`-comment is
/// stripped and surrounding whitespace trimmed — is exactly `service`.
const fn line_keyword_is(bytes: &[u8], start: usize, end: usize, keyword: &[u8]) -> bool {
    let mut lo = start;
    let mut hi = end;
    // Strip an inline or whole-line comment beginning at the first `#`.
    let mut j = lo;
    while j < hi {
        if bytes[j] == b'#' {
            hi = j;
            break;
        }
        j += 1;
    }
    // Trim surrounding ASCII whitespace.
    while lo < hi && is_ascii_whitespace(bytes[lo]) {
        lo += 1;
    }
    while hi > lo && is_ascii_whitespace(bytes[hi - 1]) {
        hi -= 1;
    }
    // The keyword runs to the first whitespace (or the line's end).
    let mut keyword_end = lo;
    while keyword_end < hi && !is_ascii_whitespace(bytes[keyword_end]) {
        keyword_end += 1;
    }
    if keyword_end - lo != keyword.len() {
        return false;
    }
    let mut k = 0;
    while k < keyword.len() {
        if bytes[lo + k] != keyword[k] {
            return false;
        }
        k += 1;
    }
    true
}

/// The ASCII bytes `char::is_whitespace` matches, for the `const` floor-sizing
/// tokeniser (which cannot call the `str` methods the runtime parser uses).
const fn is_ascii_whitespace(b: u8) -> bool {
    matches!(b, b'\t' | b'\n' | 0x0b | 0x0c | b'\r' | b' ')
}

#[cfg(test)]
mod tests {
    use super::{
        service_name, ConfigError, Launch, StartupConfig, DEFAULT_CONFIG, MAX_CONFIG_LEN,
        MAX_ENROLLED_SERVICES, MAX_SERVICES,
    };

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
                uid: tairix_users::LOGIN_UID.0,
            }
        );
        // `sysinfod` is launched before `netstack`/`devmgr` so the
        // introspection endpoint is published before any client queries it;
        // `netstack` is launched before `devmgr` so it is ready when `devmgr`
        // binds discovered NIC device channels to it. `confd` needs nothing
        // from the others and nothing needs it before an application runs.
        // `timed` is not here: it is the enrolment-governed tier, asserted
        // separately below.
        assert_eq!(
            config.services(),
            &[
                Launch {
                    path: "/System/Services/sysinfod.app/Run",
                    uid: tairix_users::SYSINFOD_UID.0,
                },
                Launch {
                    path: "/System/Services/netstack.app/Run",
                    uid: tairix_users::NETSTACK_UID.0,
                },
                Launch {
                    path: "/System/Services/devmgr.app/Run",
                    uid: tairix_users::DEVMGR_UID.0,
                },
                Launch {
                    path: "/System/Services/seatmgr.app/Run",
                    uid: tairix_users::SEATMGR_UID.0,
                },
                Launch {
                    path: "/System/Services/confd.app/Run",
                    uid: tairix_users::CONFD_UID.0,
                },
            ],
        );
        // The enrolment-governed tier: `timed` alone, so a user who turns
        // automatic time-setting off keeps it off across a reboot.
        assert_eq!(
            config.enrolled(),
            &[Launch {
                path: "/System/Services/timed.app/Run",
                uid: tairix_users::TIMED_UID.0,
            }],
        );
    }

    #[test]
    fn service_name_is_the_bundle_stem_and_falls_back_to_the_whole_path() {
        // The `.app` bundle directory names the service.
        assert_eq!(service_name("/System/Services/devmgr.app/Run"), "devmgr");
        assert_eq!(
            service_name("/System/Services/sysinfod.app/Run"),
            "sysinfod"
        );
        // No `.app` component: fall back to the whole (unique) path rather
        // than an empty name.
        assert_eq!(
            service_name("/System/Services/netd"),
            "/System/Services/netd"
        );
        // A degenerate `.app` (empty stem) does not win the match; the whole
        // path is used.
        assert_eq!(service_name("/weird/.app/Run"), "/weird/.app/Run");
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
                    uid: tairix_users::DEVMGR_UID.0,
                },
                Launch {
                    path: "/System/Services/netd",
                    uid: tairix_users::SYSINFOD_UID.0,
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
    fn max_services_is_the_boot_floor_service_count() {
        // The bound is derived from the floor itself, so the compiled-in
        // floor exactly fills it: the `const` tokeniser and the real parser
        // agree on how many `service` directives the floor declares, and the
        // floor is never truncated by a stale magic cap.
        let floor = StartupConfig::parse(DEFAULT_CONFIG).expect("the boot floor parses");
        assert_eq!(floor.services().len(), MAX_SERVICES);
        // The current floor is sysinfod, netstack, devmgr, seatmgr, confd;
        // this pins the derived value so a change to the floor is a conscious
        // one.
        assert_eq!(MAX_SERVICES, 5);
        // The same derivation over the `enrolled` keyword: `timed` alone.
        assert_eq!(floor.enrolled().len(), MAX_ENROLLED_SERVICES);
        assert_eq!(MAX_ENROLLED_SERVICES, 1);
    }

    #[test]
    fn more_than_the_enrolled_bound_fails_closed() {
        // The enrolment-governed tier is bounded by its own derivation the
        // same way the floor is, and overflows fail closed rather than
        // dropping an entry.
        let mut text = String::from("console\nsession /x login\n");
        for n in 0..=MAX_ENROLLED_SERVICES {
            let _ = writeln!(text, "enrolled /System/Services/e{n} timed");
        }
        assert_eq!(
            StartupConfig::parse(&text),
            Err(ConfigError::TooManyServices),
        );
    }

    #[test]
    fn more_than_the_floor_bound_fails_closed() {
        // A config declaring more `service` directives than the floor sizes
        // for is refused outright rather than overrunning the array or
        // dropping entries (fail closed).
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
    fn exactly_the_floor_bound_is_accepted() {
        // A config filling the floor bound exactly parses; every declared
        // service is kept.
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
            StartupConfig::parse("console\nsession System/Commands/elsh.app/Run login\n"),
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
    use tairix_abi::{Arch, BootFacts, CpuName};

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
    fn banner_renders_only_the_cpu_name_and_cores() {
        // The kernel drew the identity + RAM line; `init` adds only the
        // processor line, on its own line beneath it.
        let facts = BootFacts {
            arch: Arch::X86_64,
            cpu_name: cpu_name("Intel(R) Xeon(R) CPU E5-2690 v4 @ 2.60GHz"),
            cpu_count: 36,
            memory_bytes: 8192 * 1024 * 1024,
        };
        assert_eq!(
            banner(Some(facts)),
            "\nIntel(R) Xeon(R) CPU E5-2690 v4 @ 2.60GHz, 36 cores\n",
        );
    }

    #[test]
    fn banner_does_not_repeat_the_ram_figure_the_kernel_showed() {
        let facts = BootFacts {
            arch: Arch::Aarch64,
            cpu_name: cpu_name("ARM Cortex-A72"),
            cpu_count: 4,
            memory_bytes: 8192 * 1024 * 1024,
        };
        let text = banner(Some(facts));
        assert!(!text.contains("MiB"), "{text}");
        assert!(!text.contains("GiB"), "{text}");
        assert!(!text.contains("TAIRiX"), "{text}");
        assert_eq!(text, "\nARM Cortex-A72, 4 cores\n");
    }

    #[test]
    fn banner_falls_back_to_unknown_processor_without_a_cpu_name() {
        let facts = BootFacts {
            arch: Arch::Riscv64,
            cpu_name: CpuName::UNKNOWN,
            cpu_count: 2,
            memory_bytes: 512 * 1024 * 1024,
        };
        assert_eq!(
            banner(Some(facts)),
            "\nUnknown riscv64 processor, 2 cores\n"
        );
    }

    #[test]
    fn banner_uses_the_singular_for_one_core() {
        let facts = BootFacts {
            arch: Arch::Aarch64,
            cpu_name: cpu_name("ARM Cortex-A53"),
            cpu_count: 1,
            memory_bytes: 128 * 1024 * 1024,
        };
        assert_eq!(banner(Some(facts)), "\nARM Cortex-A53, 1 core\n");
    }

    #[test]
    fn banner_without_facts_is_empty() {
        // The kernel already drew the identity line; with no machine facts
        // there is nothing for `init` to add — never a fabricated line.
        assert_eq!(banner(None), "");
    }

    #[test]
    fn banner_always_fits_its_buffer() {
        // The worst representable inputs — a maximum-length CPU name,
        // saturated counts — still fit BANNER_MAX, so the fail-closed
        // truncation refusal can never fire for real facts.
        let facts = BootFacts {
            arch: Arch::Riscv64,
            cpu_name: cpu_name(&"x".repeat(tairix_abi::CPU_NAME_LEN)),
            cpu_count: u32::MAX,
            memory_bytes: u64::MAX,
        };
        let text = banner(Some(facts));
        assert!(text.starts_with('\n'));
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
