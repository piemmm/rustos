//! Host tests for the `flock` parse-and-run engine.

use alloc::string::String;
use alloc::vec::Vec;
use core::cell::RefCell;

use tairix_abi::Errno;
use tairix_help::{HelpSource, SourceError};

use super::{
    parse, run, Command, FlockError, Mode, Output, Ran, Session, Wait, DEFAULT_CONFLICT_CODE,
    EXIT_NOT_EXECUTABLE, EXIT_NOT_FOUND, EXIT_USAGE, USAGE,
};

/// A session that records what it was asked and answers as scripted.
struct FakeSession {
    lock_result: RefCell<Result<(), Errno>>,
    run_result: RefCell<Result<Ran, Errno>>,
    log: RefCell<Vec<String>>,
}

impl FakeSession {
    fn new() -> Self {
        Self {
            lock_result: RefCell::new(Ok(())),
            run_result: RefCell::new(Ok(Ran::Exited(0))),
            log: RefCell::new(Vec::new()),
        }
    }

    fn refusing(err: Errno) -> Self {
        let session = Self::new();
        *session.lock_result.borrow_mut() = Err(err);
        session
    }

    fn log(&self) -> Vec<String> {
        self.log.borrow().clone()
    }
}

impl Session for FakeSession {
    fn lock(&self, path: &str, mode: Mode, wait: Wait) -> Result<(), Errno> {
        self.log
            .borrow_mut()
            .push(alloc::format!("lock {path} {mode:?} {wait:?}"));
        *self.lock_result.borrow()
    }

    fn run(&self, word: &str, args: &[&str]) -> Result<Ran, Errno> {
        self.log
            .borrow_mut()
            .push(alloc::format!("run {word} {args:?}"));
        *self.run_result.borrow()
    }
}

/// A sink that keeps what it was handed.
struct Sink(RefCell<Vec<u8>>);

impl Sink {
    fn new() -> Self {
        Self(RefCell::new(Vec::new()))
    }

    fn text(&self) -> String {
        String::from_utf8(self.0.borrow().clone()).expect("utf-8")
    }
}

impl Output for Sink {
    fn write_all(&self, bytes: &[u8]) -> Result<(), Errno> {
        self.0.borrow_mut().extend_from_slice(bytes);
        Ok(())
    }
}

/// A help source with no documents, so the short help falls back.
struct NoHelp;

impl HelpSource for NoHelp {
    fn locale_dirs(&self) -> Result<Vec<String>, SourceError> {
        Ok(Vec::new())
    }

    fn read(&self, _locale_dir: &str, _file_name: &str) -> Result<Option<Vec<u8>>, SourceError> {
        Ok(None)
    }
}

fn parsed<'a>(argv: &[&'a str]) -> Command<'a> {
    parse(argv).expect("a well-formed command line")
}

#[test]
fn the_default_is_an_exclusive_indefinite_wait() {
    let command = parsed(&["flock", "/tmp.lock", "backup"]);
    assert_eq!(command.mode, Mode::Exclusive);
    assert_eq!(command.wait, Wait::Forever);
    assert_eq!(command.conflict_code, DEFAULT_CONFLICT_CODE);
    assert!(!command.verbose);
    assert_eq!(command.file, "/tmp.lock");
    assert_eq!(command.argv, alloc::vec!["backup"]);
}

#[test]
fn the_long_and_short_spellings_agree() {
    for argv in [
        alloc::vec!["flock", "-s", "-n", "f", "c"],
        alloc::vec!["flock", "--shared", "--nonblock", "f", "c"],
        alloc::vec!["flock", "-sn", "f", "c"],
    ] {
        let command = parsed(&argv);
        assert_eq!(command.mode, Mode::Shared, "{argv:?}");
        assert_eq!(command.wait, Wait::Never, "{argv:?}");
    }
    // The later mode flag wins, as it does for the GNU tools.
    assert_eq!(
        parsed(&["flock", "-s", "-x", "f", "c"]).mode,
        Mode::Exclusive
    );
    assert_eq!(parsed(&["flock", "-x", "-s", "f", "c"]).mode, Mode::Shared);
}

#[test]
fn a_timeout_is_read_in_seconds_in_every_accepted_spelling() {
    for argv in [
        alloc::vec!["flock", "-w", "5", "f", "c"],
        alloc::vec!["flock", "-w5", "f", "c"],
        alloc::vec!["flock", "--timeout", "5", "f", "c"],
        alloc::vec!["flock", "--timeout=5", "f", "c"],
    ] {
        assert_eq!(parsed(&argv).wait, Wait::Until(5_000_000_000), "{argv:?}");
    }
    assert_eq!(parsed(&["flock", "-w", "0", "f", "c"]).wait, Wait::Until(0));
}

#[test]
fn a_conflict_code_is_read_in_every_accepted_spelling() {
    for argv in [
        alloc::vec!["flock", "-E", "7", "f", "c"],
        alloc::vec!["flock", "-E7", "f", "c"],
        alloc::vec!["flock", "--conflict-code", "7", "f", "c"],
        alloc::vec!["flock", "--conflict-code=7", "f", "c"],
    ] {
        assert_eq!(parsed(&argv).conflict_code, 7, "{argv:?}");
    }
}

#[test]
fn a_malformed_value_is_a_usage_error_rather_than_a_guess() {
    for argv in [
        alloc::vec!["flock", "-w", "f", "c"],
        alloc::vec!["flock", "-w", "-1", "f", "c"],
        alloc::vec!["flock", "-w", "5s", "f", "c"],
        alloc::vec!["flock", "-w", "", "f", "c"],
        alloc::vec!["flock", "-w", "99999999999999999999", "f", "c"],
        alloc::vec!["flock", "-E", "x", "f", "c"],
        alloc::vec!["flock", "-E", "5000000000", "f", "c"],
    ] {
        assert_eq!(parse(&argv), Err(FlockError::Usage), "{argv:?}");
    }
}

#[test]
fn a_timeout_too_large_to_express_in_nanoseconds_is_refused_not_saturated() {
    // Saturating would turn a typo into an unbounded wait, which is the one
    // thing a `-w` caller asked not to happen.
    let huge = alloc::format!("{}", u64::MAX / 1_000_000_000 + 1);
    assert_eq!(
        parse(&["flock", "-w", &huge, "f", "c"]),
        Err(FlockError::Usage)
    );
}

#[test]
fn an_unknown_option_is_refused_and_nothing_is_locked() {
    for argv in [
        alloc::vec!["flock", "-Z", "f", "c"],
        alloc::vec!["flock", "--nope", "f", "c"],
        // A near-miss long option must never be taken as the file name, or
        // the run would lock the wrong thing and run the wrong command.
        alloc::vec!["flock", "--nonblok", "lock", "cmd"],
        alloc::vec!["flock", "--shared=yes", "f", "c"],
        alloc::vec!["flock", "-sZ", "f", "c"],
        // `-u` and `-o` exist in util-linux for its descriptor form; they
        // are refused rather than silently ignored, so a script that needs
        // them is told so instead of running unlocked.
        alloc::vec!["flock", "-u", "f", "c"],
        alloc::vec!["flock", "-o", "f", "c"],
        alloc::vec!["flock", "-c", "echo hi"],
    ] {
        assert_eq!(parse(&argv), Err(FlockError::Usage), "{argv:?}");
    }
}

#[test]
fn a_missing_file_or_command_is_a_usage_error() {
    assert_eq!(parse(&["flock"]), Err(FlockError::Usage));
    assert_eq!(parse(&["flock", "-x"]), Err(FlockError::Usage));
    assert_eq!(
        parse(&["flock", "onlyfile"]),
        Err(FlockError::Usage),
        "a lock with nothing to run would take a lock and drop it again"
    );
}

#[test]
fn the_commands_own_options_reach_the_command_not_flock() {
    let command = parsed(&["flock", "lock", "ls", "-l", "-w", "--help"]);
    assert_eq!(command.file, "lock");
    assert_eq!(command.argv, alloc::vec!["ls", "-l", "-w", "--help"]);
    assert_eq!(
        command.wait,
        Wait::Forever,
        "a `-w` past the first operand belongs to the command"
    );
    assert!(!command.help, "and so does a `--help`");
}

#[test]
fn a_double_dash_ends_option_parsing_so_a_dashed_name_stays_reachable() {
    let command = parsed(&["flock", "--", "-weird-file", "-weird-command"]);
    assert_eq!(command.file, "-weird-file");
    assert_eq!(command.argv, alloc::vec!["-weird-command"]);
}

#[test]
fn help_is_rendered_and_nothing_is_locked_or_run() {
    for argv in [
        alloc::vec!["flock", "--help"],
        alloc::vec!["flock", "-?"],
        alloc::vec!["flock", "-h"],
    ] {
        let command = parsed(&argv);
        assert!(command.help, "{argv:?}");
        let session = FakeSession::new();
        let out = Sink::new();
        let notices = Sink::new();
        assert_eq!(
            run(&command, None, &session, &NoHelp, &out, &notices),
            Ok(0)
        );
        assert_eq!(out.text(), USAGE, "the fallback when no Help tree is read");
        assert!(
            session.log().is_empty(),
            "help takes no lock and runs nothing"
        );
    }
}

#[test]
fn the_lock_is_taken_before_the_command_runs() {
    let session = FakeSession::new();
    *session.run_result.borrow_mut() = Ok(Ran::Exited(0));
    let out = Sink::new();
    let notices = Sink::new();
    let command = parsed(&["flock", "-s", "-w", "3", "/db.lock", "report", "--full"]);
    assert_eq!(
        run(&command, None, &session, &NoHelp, &out, &notices),
        Ok(0)
    );
    assert_eq!(
        session.log(),
        alloc::vec![
            String::from("lock /db.lock Shared Until(3000000000)"),
            String::from("run report [\"--full\"]"),
        ],
        "the order is load-bearing: the command must never run unlocked"
    );
}

#[test]
fn the_commands_exit_status_is_the_tools_own() {
    for status in [0, 1, 42, 127] {
        let session = FakeSession::new();
        *session.run_result.borrow_mut() = Ok(Ran::Exited(status));
        let out = Sink::new();
        let notices = Sink::new();
        assert_eq!(
            run(
                &parsed(&["flock", "f", "c"]),
                None,
                &session,
                &NoHelp,
                &out,
                &notices
            ),
            Ok(status)
        );
    }
}

#[test]
fn a_contended_lock_exits_with_the_conflict_code_and_runs_nothing() {
    for refusal in [Errno::WouldBlock, Errno::TimedOut] {
        let session = FakeSession::refusing(refusal);
        let out = Sink::new();
        let notices = Sink::new();
        assert_eq!(
            run(
                &parsed(&["flock", "-n", "-E", "9", "f", "c"]),
                None,
                &session,
                &NoHelp,
                &out,
                &notices
            ),
            Ok(9),
            "{refusal:?}"
        );
        assert_eq!(
            session.log().len(),
            1,
            "the command must not run when the lock was not taken"
        );
    }
}

#[test]
fn the_conflict_code_defaults_to_one() {
    let session = FakeSession::refusing(Errno::WouldBlock);
    let out = Sink::new();
    let notices = Sink::new();
    assert_eq!(
        run(
            &parsed(&["flock", "-n", "f", "c"]),
            None,
            &session,
            &NoHelp,
            &out,
            &notices
        ),
        Ok(DEFAULT_CONFLICT_CODE)
    );
}

#[test]
fn a_refusal_waiting_would_not_fix_is_an_error_not_a_conflict() {
    for refusal in [
        Errno::PermissionDenied,
        Errno::Deadlock,
        Errno::NotSupported,
        Errno::LimitExceeded,
    ] {
        let session = FakeSession::refusing(refusal);
        let out = Sink::new();
        let notices = Sink::new();
        assert_eq!(
            run(
                &parsed(&["flock", "f", "c"]),
                None,
                &session,
                &NoHelp,
                &out,
                &notices
            ),
            Err(FlockError::Lock(refusal)),
            "{refusal:?}"
        );
    }
}

#[test]
fn an_unresolvable_command_reports_the_shells_own_status() {
    let session = FakeSession::new();
    *session.run_result.borrow_mut() = Ok(Ran::NotFound);
    let out = Sink::new();
    let notices = Sink::new();
    let failure = run(
        &parsed(&["flock", "f", "nosuch"]),
        None,
        &session,
        &NoHelp,
        &out,
        &notices,
    )
    .expect_err("not found");
    assert_eq!(failure, FlockError::NotFound(String::from("nosuch")));
    assert_eq!(failure.exit_code(), EXIT_NOT_FOUND);

    *session.run_result.borrow_mut() = Ok(Ran::NotExecutable);
    let failure = run(
        &parsed(&["flock", "f", "adir"]),
        None,
        &session,
        &NoHelp,
        &out,
        &notices,
    )
    .expect_err("not executable");
    assert_eq!(failure, FlockError::NotExecutable(String::from("adir")));
    assert_eq!(failure.exit_code(), EXIT_NOT_EXECUTABLE);
}

#[test]
fn verbose_notes_go_to_the_notice_stream_and_never_to_standard_output() {
    let session = FakeSession::new();
    let out = Sink::new();
    let notices = Sink::new();
    assert_eq!(
        run(
            &parsed(&["flock", "-v", "f", "c"]),
            None,
            &session,
            &NoHelp,
            &out,
            &notices
        ),
        Ok(0)
    );
    assert_eq!(notices.text(), "flock: lock held\n");
    assert!(
        out.text().is_empty(),
        "a note must never contaminate the command's own output stream"
    );

    let session = FakeSession::refusing(Errno::WouldBlock);
    let notices = Sink::new();
    let _ = run(
        &parsed(&["flock", "-v", "-n", "f", "c"]),
        None,
        &session,
        &NoHelp,
        &out,
        &notices,
    );
    assert_eq!(notices.text(), "flock: lock already held\n");
}

#[test]
fn a_usage_error_reports_the_status_the_tree_uses_for_one() {
    assert_eq!(FlockError::Usage.exit_code(), EXIT_USAGE);
}

#[test]
fn the_usage_banner_documents_every_switch_the_parser_accepts() {
    // The banner is the fallback a user sees when the Help tree cannot be
    // read, so a switch missing from it is undiscoverable.
    for switch in [
        "-s, --shared",
        "-x, --exclusive",
        "-n, --nonblock",
        "-w, --timeout",
        "-E, --conflict-code",
        "-v, --verbose",
        "-?, --help",
    ] {
        assert!(USAGE.contains(switch), "the banner omits {switch}");
    }
}
