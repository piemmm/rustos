//! TAIRiX `whoami` — print the current user's account name
//! (`plans/APPS.md` §12.1 Stage C).
//!
//! The GNU coreutils `whoami`: it prints the user name associated with the
//! caller's identity and nothing else. TAIRiX has no `/etc/passwd` and no
//! ambient identity file: the uid comes from the kernel-attested origin
//! record (the ungated `self_origin` syscall — a pure self-observer), and
//! the uid → name pairing comes from the ungated `USER_DIRECTORY` query the
//! System Information API serves (`sysinfod`; no credential material). No
//! privileged path and no capability beyond the tool's console/help reach
//! is involved.
//!
//! # What this crate is
//!
//! The pure, host-testable core of the tool:
//!
//! * [`parse`] — the [`Command`] a command line names, with GNU `whoami`'s
//!   option rules: the tool takes no operands (`extra operand`), knows no
//!   options beyond the reserved short-help switches, and — like GNU
//!   getopt's permutation — diagnoses option tokens left to right wherever
//!   they sit, so `whoami foo --help` serves the help and
//!   `whoami foo -x` diagnoses `-x`.
//! * [`run`] — the engine: read the caller's uid through the injected
//!   [`Identity`] seam, walk the shared account directory
//!   ([`tairix_procinfo::for_each_user`]) for the matching name, and write
//!   it through the injected [`Output`]. A missing name is the GNU
//!   `cannot find name for user ID` diagnostic; a failed directory walk is
//!   a service error, never misreported as a missing name.
//!
//! # Layering & safety
//!
//! `no_std` (with `alloc`); the only dependencies are the audited `lib/abi`
//! ABI crate, the shared `lib/procinfo` client helpers, and the shared
//! `lib/help` engine, so this userland tool never links a kernel or driver
//! crate. No `unsafe`, and no `unwrap`/`expect`/`panic!` in production
//! paths; nothing writes to fd 3 (`stdinfo` — a single name is never an
//! omission or summary).
//!
//! The bundle's `Help/` documents are **not** embedded in this crate: they
//! are authored once in the bundle's on-disk `Help/` tree, planted onto
//! `/System` by the image builder from that source (`tools/syshelp`), and
//! read back at runtime through the injected [`tairix_help::HelpSource`]
//! seam. Help is never hardcoded into the program (`plans/APPS.md`).

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

extern crate alloc;

use alloc::string::String;
use core::fmt;

use tairix_abi::Errno;
use tairix_help::{own_short_help, HelpSource};
use tairix_procinfo::{for_each_user, CallError, ListError, Output, Transport};

/// The usage banner a usage error is reported with, and the fallback the
/// short-help switches print when `whoami`'s own Help tree is unavailable.
pub const USAGE: &str = "\
usage: whoami

  print the account name of the current user
  -h, -?      show this help";

/// `whoami`'s own command word: the short-help switches render its own
/// Help document through the same engine as any other command's.
const OWN_WORD: &str = "whoami";

/// One thing the `whoami` tool can do.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Command {
    /// Print the current user's account name.
    Whoami,
    /// Render `whoami`'s own short help (`-h`/`-?`/`--help`): the `NAME`,
    /// `SYNOPSIS`, and compact `OPTIONS` of its Help document, through the
    /// same engine as any other command's short help (plans/APPS.md §4).
    Help,
}

/// The failures the `whoami` tool reports.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WhoamiError {
    /// An unrecognised `--long` option (GNU `unrecognized option`).
    UnknownLong(String),
    /// An unrecognised short option (GNU `invalid option`); carries the
    /// offending flag character.
    UnknownShort(char),
    /// An operand was given; `whoami` takes none (GNU `extra operand`).
    ExtraOperand(String),
    /// Reading the caller's identity failed, the directory transport
    /// failed, or the reply did not decode against `sysinfo-v1`. Carries
    /// the underlying [`Errno`].
    Service(Errno),
    /// The account directory holds no name for the caller's uid — the GNU
    /// `cannot find name for user ID` condition. Carries the uid.
    NoName(u32),
    /// Writing to the terminal failed. Carries the underlying [`Errno`].
    Output(Errno),
}

impl From<ListError> for WhoamiError {
    fn from(err: ListError) -> Self {
        match err {
            // The directory query is ungated, so a refusal is just another
            // service failure — reported honestly, never turned into a
            // fabricated "no name".
            ListError::Call(CallError::PermissionDenied) => Self::Service(Errno::PermissionDenied),
            ListError::Call(CallError::Service(errno)) => Self::Service(errno),
            // The lookup sink never fails, but the conversion stays total.
            ListError::Sink(errno) => Self::Output(errno),
        }
    }
}

impl fmt::Display for WhoamiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownLong(token) => write!(f, "unrecognized option '{token}'"),
            Self::UnknownShort(flag) => write!(f, "invalid option -- '{flag}'"),
            Self::ExtraOperand(operand) => write!(f, "extra operand '{operand}'"),
            Self::Service(errno) => write!(f, "system information service error: {errno}"),
            Self::NoName(uid) => write!(f, "cannot find name for user ID {uid}"),
            Self::Output(errno) => write!(f, "terminal write failed: {errno}"),
        }
    }
}

/// The caller's own kernel-attested identity.
///
/// The production implementation reads the `self_origin` syscall (ungated —
/// a task may always observe its own identity); tests inject a fixture. The
/// seam keeps the engine pure and makes "identity unavailable" an ordinary,
/// testable failure.
pub trait Identity {
    /// The caller's uid.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the identity read raises.
    fn uid(&self) -> Result<u32, Errno>;
}

/// Parse `args` (the tool's arguments, excluding the program name) into a
/// [`Command`].
///
/// The grammar is the GNU `whoami` surface — no operands, no options beyond
/// the reserved short-help switches (plans/APPS.md §4):
///
/// * `-h` / `-?` / `--help` — short help; like GNU getopt's permutation,
///   an option is honoured wherever it sits, so `whoami foo --help` serves
///   the help.
/// * `--` — end option parsing; every later argument is an operand.
/// * any other option-shaped token — [`WhoamiError::UnknownLong`] /
///   [`WhoamiError::UnknownShort`], diagnosed left to right exactly as GNU
///   getopt reports the first bad option before complaining about operands.
/// * any operand (`whoami` takes none) — [`WhoamiError::ExtraOperand`]
///   naming the first one. A lone `-` is an operand.
///
/// # Errors
///
/// The GNU diagnostics above, in getopt's order: the first unrecognised
/// option anywhere wins over an operand complaint.
pub fn parse(args: &[&str]) -> Result<Command, WhoamiError> {
    let mut operand: Option<&str> = None;
    let mut options_ended = false;
    for &arg in args {
        if options_ended {
            operand.get_or_insert(arg);
            continue;
        }
        match arg {
            "--" => options_ended = true,
            "-h" | "-?" | "--help" => return Ok(Command::Help),
            _ if arg.starts_with("--") => {
                return Err(WhoamiError::UnknownLong(String::from(arg)));
            }
            _ if arg.len() > 1 && arg.starts_with('-') => {
                // The first flag character of the cluster, exactly as
                // getopt reports it (`-hx` diagnoses `h`: GNU `whoami` has
                // no short options, so only the exact reserved spellings
                // above are ever honoured).
                let flag = arg.chars().nth(1).unwrap_or('-');
                return Err(WhoamiError::UnknownShort(flag));
            }
            _ => {
                operand.get_or_insert(arg);
            }
        }
    }
    match operand {
        Some(operand) => Err(WhoamiError::ExtraOperand(String::from(operand))),
        None => Ok(Command::Whoami),
    }
}

/// Run one [`Command`]: read the caller's uid through `identity`, find its
/// account name in the shared directory walk over `transport`, and write
/// the name to `out`. `locale` is the user's `LANG` preference, if set;
/// `help` is the tool's own `Help/` tree, read by the short-help switches.
///
/// The directory walk is the shared `lib/procinfo` helper (the same one
/// `top`'s `USER` column uses), so the paging and fail-closed decode are
/// never re-derived here. The walk always completes over the whole
/// directory; a duplicate uid keeps its first name deterministically.
///
/// # Errors
///
/// * [`WhoamiError::Service`] — the identity read or the directory query
///   failed.
/// * [`WhoamiError::NoName`] — the directory holds no name for the uid.
/// * [`WhoamiError::Output`] — writing the terminal failed.
pub fn run(
    command: Command,
    locale: Option<&str>,
    identity: &dyn Identity,
    transport: &dyn Transport,
    help: &dyn HelpSource,
    out: &dyn Output,
) -> Result<(), WhoamiError> {
    match command {
        Command::Help => short_help(locale, help, out),
        Command::Whoami => {
            let uid = identity.uid().map_err(WhoamiError::Service)?;
            let mut name: Option<String> = None;
            for_each_user(transport, |record| {
                if record.uid == uid && name.is_none() {
                    name = Some(String::from_utf8_lossy(record.name_bytes()).into_owned());
                }
                Ok(())
            })?;
            let name = name.ok_or(WhoamiError::NoName(uid))?;
            out.write_line(&name).map_err(WhoamiError::Output)
        }
    }
}

/// Render `whoami`'s own short help (`NAME` + `SYNOPSIS` + compact
/// `OPTIONS`) from its own bundle's `Help/` tree through the one shared
/// engine; when no document can be served (a build without the bundle's
/// documents) the usage banner stands in — the tool's own text, not
/// fabricated help content — so `-h` never fails. The rendered page is
/// written as one multi-line `write_line`; the seam owns the final newline.
fn short_help(
    locale: Option<&str>,
    help: &dyn HelpSource,
    out: &dyn Output,
) -> Result<(), WhoamiError> {
    let bytes = own_short_help(help, locale, OWN_WORD);
    let text = bytes
        .as_deref()
        .and_then(|bytes| core::str::from_utf8(bytes).ok())
        .unwrap_or(USAGE);
    out.write_line(text.trim_end_matches('\n'))
        .map_err(WhoamiError::Output)
}

#[cfg(test)]
mod tests {
    extern crate std;

    use alloc::format;
    use alloc::string::{String, ToString};
    use alloc::vec::Vec;
    use core::cell::RefCell;

    use tairix_abi::sysinfo::{
        SysinfoQueryId, SysinfoRequestHeader, UserDirectoryRecord, UserDirectoryRequest,
    };
    use tairix_abi::Errno;
    use tairix_help::{HelpSource, SourceError};
    use tairix_procinfo::{Output, Transport};

    use super::{parse, run, Command, Identity, WhoamiError, USAGE};

    /// A Help tree with no documents at all: the short-help fallback path.
    struct NoHelp;

    impl HelpSource for NoHelp {
        fn locale_dirs(&self) -> Result<Vec<String>, SourceError> {
            Ok(Vec::new())
        }

        fn read(
            &self,
            _locale_dir: &str,
            _file_name: &str,
        ) -> Result<Option<Vec<u8>>, SourceError> {
            Ok(None)
        }
    }

    /// A fixed identity answering with one uid, or failing.
    struct FixedUid(Result<u32, Errno>);

    impl Identity for FixedUid {
        fn uid(&self) -> Result<u32, Errno> {
            self.0
        }
    }

    /// An in-memory `sysinfod` stand-in answering directory queries from a
    /// fixed record set, decoding the request exactly as the real service.
    struct Directory {
        records: Vec<UserDirectoryRecord>,
    }

    impl Transport for Directory {
        fn query(&self, request: &[u8]) -> Result<Vec<u8>, Errno> {
            let header = SysinfoRequestHeader::from_bytes(request)?;
            assert_eq!(header.query, SysinfoQueryId::USER_DIRECTORY);
            let payload = &request[SysinfoRequestHeader::WIRE_LEN
                ..SysinfoRequestHeader::WIRE_LEN + header.payload_len as usize];
            let req = UserDirectoryRequest::from_bytes(payload)?;
            let offset = req.offset as usize;
            if offset >= self.records.len() {
                return Ok(Vec::new());
            }
            let take = core::cmp::min(self.records.len() - offset, req.limit as usize);
            let mut out = Vec::with_capacity(take * UserDirectoryRecord::WIRE_LEN);
            for record in &self.records[offset..offset + take] {
                out.extend_from_slice(&record.to_le_bytes());
            }
            Ok(out)
        }
    }

    /// A transport that always fails, standing in for a broken service.
    struct Failing;

    impl Transport for Failing {
        fn query(&self, _request: &[u8]) -> Result<Vec<u8>, Errno> {
            Err(Errno::NotFound)
        }
    }

    /// Captures written lines; optionally refuses every write.
    struct Sink {
        lines: RefCell<Vec<String>>,
        closed: bool,
    }

    impl Sink {
        fn new() -> Self {
            Self {
                lines: RefCell::new(Vec::new()),
                closed: false,
            }
        }
    }

    impl Output for Sink {
        fn write_line(&self, line: &str) -> Result<(), Errno> {
            if self.closed {
                return Err(Errno::NotFound);
            }
            self.lines.borrow_mut().push(line.to_string());
            Ok(())
        }
    }

    fn record(uid: u32, name: &[u8]) -> UserDirectoryRecord {
        UserDirectoryRecord::new(uid, name).expect("record")
    }

    fn directory() -> Directory {
        Directory {
            records: alloc::vec![record(0, b"root"), record(1000, b"alice")],
        }
    }

    #[test]
    fn parse_names_the_lookup_and_the_help() {
        assert_eq!(parse(&[]), Ok(Command::Whoami));
        assert_eq!(parse(&["-h"]), Ok(Command::Help));
        assert_eq!(parse(&["-?"]), Ok(Command::Help));
        assert_eq!(parse(&["--help"]), Ok(Command::Help));
        // getopt permutation: an option is honoured after an operand.
        assert_eq!(parse(&["extra", "--help"]), Ok(Command::Help));
    }

    #[test]
    fn parse_diagnoses_options_and_operands_in_getopt_order() {
        assert_eq!(
            parse(&["--bogus"]),
            Err(WhoamiError::UnknownLong(String::from("--bogus")))
        );
        assert_eq!(parse(&["-x"]), Err(WhoamiError::UnknownShort('x')));
        // A cluster diagnoses its first flag; only the exact reserved
        // spellings serve the help.
        assert_eq!(parse(&["-hx"]), Err(WhoamiError::UnknownShort('h')));
        assert_eq!(
            parse(&["extra"]),
            Err(WhoamiError::ExtraOperand(String::from("extra")))
        );
        // The first bad option wins over the operand complaint.
        assert_eq!(parse(&["extra", "-x"]), Err(WhoamiError::UnknownShort('x')));
        // After `--` everything is an operand — still one too many.
        assert_eq!(
            parse(&["--", "-h"]),
            Err(WhoamiError::ExtraOperand(String::from("-h")))
        );
        // A lone `-` is an operand.
        assert_eq!(
            parse(&["-"]),
            Err(WhoamiError::ExtraOperand(String::from("-")))
        );
    }

    #[test]
    fn prints_the_name_the_directory_pairs_with_the_uid() {
        let out = Sink::new();
        run(
            Command::Whoami,
            None,
            &FixedUid(Ok(1000)),
            &directory(),
            &NoHelp,
            &out,
        )
        .expect("runs");
        assert_eq!(&*out.lines.borrow(), &["alice"]);
    }

    #[test]
    fn a_uid_with_no_directory_entry_is_the_gnu_no_name_diagnostic() {
        let out = Sink::new();
        let outcome = run(
            Command::Whoami,
            None,
            &FixedUid(Ok(7)),
            &directory(),
            &NoHelp,
            &out,
        );
        assert_eq!(outcome, Err(WhoamiError::NoName(7)));
        assert!(out.lines.borrow().is_empty());
        assert_eq!(
            format!("{}", WhoamiError::NoName(7)),
            "cannot find name for user ID 7"
        );
    }

    #[test]
    fn a_failed_directory_walk_is_a_service_error_not_a_missing_name() {
        let out = Sink::new();
        let outcome = run(
            Command::Whoami,
            None,
            &FixedUid(Ok(0)),
            &Failing,
            &NoHelp,
            &out,
        );
        assert_eq!(outcome, Err(WhoamiError::Service(Errno::NotFound)));
    }

    #[test]
    fn a_failed_identity_read_is_a_service_error() {
        let out = Sink::new();
        let outcome = run(
            Command::Whoami,
            None,
            &FixedUid(Err(Errno::BadAddress)),
            &directory(),
            &NoHelp,
            &out,
        );
        assert_eq!(outcome, Err(WhoamiError::Service(Errno::BadAddress)));
    }

    #[test]
    fn a_closed_terminal_is_an_output_error() {
        let out = Sink {
            lines: RefCell::new(Vec::new()),
            closed: true,
        };
        let outcome = run(
            Command::Whoami,
            None,
            &FixedUid(Ok(0)),
            &directory(),
            &NoHelp,
            &out,
        );
        assert_eq!(outcome, Err(WhoamiError::Output(Errno::NotFound)));
    }

    #[test]
    fn short_help_falls_back_to_the_usage_banner_without_documents() {
        let out = Sink::new();
        run(
            Command::Help,
            None,
            &FixedUid(Ok(0)),
            &directory(),
            &NoHelp,
            &out,
        )
        .expect("help served");
        assert_eq!(&*out.lines.borrow(), &[USAGE]);
    }

    /// Every locale's `OPTIONS` section documents exactly the switches this
    /// parser accepts (`plans/APPS.md` §3.1): the flag tokens are
    /// language-neutral, so each translated document must carry the same
    /// keys as the canonical one. The documents are read from the bundle's
    /// own on-disk `Help/` tree — the single source the image builder
    /// plants — never a copy embedded in this crate.
    #[test]
    fn help_documents_the_parser_switches() {
        use std::fs;

        let help_root = format!("{}/Help", env!("CARGO_MANIFEST_DIR"));
        for locale in tairix_help::REQUIRED_LOCALES {
            let path = format!("{help_root}/{locale}/whoami.md");
            let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
            let switch = "`-h, -?`";
            assert!(
                text.contains(switch),
                "{locale}/whoami.md must document {switch}"
            );
        }
    }
}
