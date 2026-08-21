//! Unit tests for the `readlink` parse and print engine, over in-memory
//! seams (no kernel).

use super::*;
use alloc::string::ToString;
use core::cell::RefCell;
use tairix_help::SourceError;

extern crate std;
use std::collections::BTreeMap;

/// An in-memory [`Filesystem`]: a path either stores a target or answers a
/// scripted errno, exactly as the kernel does.
struct MockFs {
    links: BTreeMap<String, Result<String, Errno>>,
}

impl MockFs {
    fn new(entries: &[(&str, Result<&str, Errno>)]) -> Self {
        Self {
            links: entries
                .iter()
                .map(|(path, answer)| ((*path).to_string(), answer.map(ToString::to_string)))
                .collect(),
        }
    }
}

impl Filesystem for MockFs {
    fn read_link(&self, path: &str) -> Result<String, Errno> {
        match self.links.get(path) {
            Some(answer) => answer.clone(),
            // An absent name is the kernel's `NotFound`, not a panic.
            None => Err(Errno::NotFound),
        }
    }
}

/// A stream fixture capturing everything written.
#[derive(Default)]
struct MockOut {
    text: RefCell<String>,
    fail: bool,
}

impl MockOut {
    fn text(&self) -> String {
        self.text.borrow().clone()
    }
}

impl Output for MockOut {
    fn write_all(&self, bytes: &[u8]) -> Result<(), Errno> {
        if self.fail {
            return Err(Errno::NotImplemented);
        }
        self.text
            .borrow_mut()
            .push_str(core::str::from_utf8(bytes).unwrap_or("<non-utf8>"));
        Ok(())
    }
}

/// A [`HelpSource`] with no documents, so `run` falls back to [`USAGE`].
struct NoHelp;

impl HelpSource for NoHelp {
    fn locale_dirs(&self) -> Result<Vec<String>, SourceError> {
        Ok(Vec::new())
    }

    fn read(&self, _locale_dir: &str, _file_name: &str) -> Result<Option<Vec<u8>>, SourceError> {
        Ok(None)
    }
}

fn tree() -> MockFs {
    MockFs::new(&[
        ("alias", Ok("Home:/Documents/report.txt")),
        ("relative", Ok("../sibling")),
        ("dangling", Ok("nowhere")),
        // A file and a directory have no target: the kernel's domain
        // refusal, the same code for both.
        ("plain", Err(Errno::OutOfRange)),
        ("dir", Err(Errno::OutOfRange)),
    ])
}

fn run_case(args: &[&str], fs: &MockFs) -> (bool, String, String) {
    let command = parse(args).expect("parse");
    let out = MockOut::default();
    let err = MockOut::default();
    let clean = run(command, None, fs, &NoHelp, &out, &err).expect("run");
    (clean, out.text(), err.text())
}

fn print(options: Options, paths: &[&str]) -> Command {
    Command::Print {
        options,
        paths: paths.iter().map(|p| (*p).to_string()).collect(),
    }
}

// --- parse ---------------------------------------------------------------

#[test]
fn one_operand_parses_with_the_gnu_defaults() {
    // Quiet is the GNU default: a refused read prints nothing unless `-v`.
    assert_eq!(parse(&["alias"]), Ok(print(Options::default(), &["alias"])));
    assert!(!Options::default().verbose);
}

#[test]
fn no_operand_is_usage() {
    assert_eq!(parse(&[]), Err(ReadlinkError::Usage));
    assert_eq!(parse(&["-n"]), Err(ReadlinkError::Usage));
}

#[test]
fn switches_and_clusters_parse() {
    let options = Options {
        no_newline: true,
        zero: true,
        verbose: true,
    };
    assert_eq!(parse(&["-nzv", "a"]), Ok(print(options, &["a"])));
    assert_eq!(
        parse(&["--no-newline", "--zero", "--verbose", "a"]),
        Ok(print(options, &["a"]))
    );
}

#[test]
fn the_last_of_quiet_and_verbose_wins() {
    assert!(
        !matches!(parse(&["-v", "-q", "a"]), Ok(Command::Print { options, .. }) if options.verbose)
    );
    assert!(
        matches!(parse(&["-q", "-v", "a"]), Ok(Command::Print { options, .. }) if options.verbose)
    );
    // `-s` is GNU's synonym for `-q`, not a separate posture.
    assert!(
        !matches!(parse(&["-v", "-s", "a"]), Ok(Command::Print { options, .. }) if options.verbose)
    );
}

#[test]
fn help_switches_are_the_reserved_pair() {
    assert_eq!(parse(&["-?"]), Ok(Command::Help));
    assert_eq!(parse(&["--help"]), Ok(Command::Help));
}

#[test]
fn the_canonicalisation_switches_fail_closed_naming_themselves() {
    // Refused, never approximated: path resolution lives in the VFS, and a
    // second implementation here could report a path the kernel resolves
    // differently.
    for (arg, spelled) in [
        ("-f", "-f"),
        ("-e", "-e"),
        ("-m", "-m"),
        ("--canonicalize", "--canonicalize"),
        ("--canonicalize-existing", "--canonicalize-existing"),
        ("--canonicalize-missing", "--canonicalize-missing"),
    ] {
        assert_eq!(
            parse(&[arg, "a"]),
            Err(ReadlinkError::Unsupported(spelled.to_string())),
            "{arg}"
        );
    }
}

#[test]
fn an_unknown_option_is_usage() {
    assert_eq!(parse(&["-x", "a"]), Err(ReadlinkError::Usage));
    assert_eq!(parse(&["--nonsense", "a"]), Err(ReadlinkError::Usage));
}

#[test]
fn double_dash_ends_options_and_a_bare_dash_is_a_name() {
    assert_eq!(parse(&["--", "-n"]), Ok(print(Options::default(), &["-n"])));
    assert_eq!(parse(&["-"]), Ok(print(Options::default(), &["-"])));
}

// --- run -----------------------------------------------------------------

#[test]
fn a_target_is_printed_exactly_as_stored() {
    // Verbatim: an absolute target, a relative one with `..`, and one that
    // names nothing all print the spelling the link holds.
    for (path, target) in [
        ("alias", "Home:/Documents/report.txt\n"),
        ("relative", "../sibling\n"),
        ("dangling", "nowhere\n"),
    ] {
        let (clean, out, err) = run_case(&[path], &tree());
        assert!(clean, "{path}");
        assert_eq!(out, target, "{path}");
        assert!(err.is_empty(), "{path}");
    }
}

#[test]
fn a_non_link_is_refused_quietly_and_the_run_is_unclean() {
    let (clean, out, err) = run_case(&["plain"], &tree());
    assert!(!clean);
    assert!(out.is_empty());
    assert!(err.is_empty(), "quiet is the GNU default");
}

#[test]
fn verbose_diagnoses_the_refusal_with_the_kernels_own_reason() {
    let (clean, out, err) = run_case(&["-v", "plain"], &tree());
    assert!(!clean);
    assert!(out.is_empty());
    assert_eq!(err, "readlink: plain: value out of range\n");
}

#[test]
fn several_operands_each_get_a_line_and_a_refusal_does_not_stop_the_run() {
    let (clean, out, err) = run_case(&["-v", "alias", "plain", "relative"], &tree());
    assert!(!clean, "a refused read makes the run unclean");
    // The two readable targets are still printed, in operand order.
    assert_eq!(out, "Home:/Documents/report.txt\n../sibling\n");
    assert_eq!(err, "readlink: plain: value out of range\n");
}

#[test]
fn zero_ends_each_target_with_nul() {
    let (clean, out, _) = run_case(&["-z", "alias", "relative"], &tree());
    assert!(clean);
    assert_eq!(out, "Home:/Documents/report.txt\0../sibling\0");
}

#[test]
fn no_newline_drops_only_the_final_delimiter() {
    let (clean, out, err) = run_case(&["-n", "alias"], &tree());
    assert!(clean);
    assert_eq!(out, "Home:/Documents/report.txt");
    assert!(err.is_empty());
}

#[test]
fn no_newline_is_ignored_and_reported_for_several_operands() {
    // The delimiters are what separate the targets, so dropping them would
    // run two paths together; GNU says so on standard error rather than
    // producing an unparseable line.
    let (clean, out, err) = run_case(&["-n", "alias", "relative"], &tree());
    assert!(clean);
    assert_eq!(out, "Home:/Documents/report.txt\n../sibling\n");
    assert_eq!(
        err,
        "readlink: ignoring --no-newline with multiple arguments\n"
    );
}

#[test]
fn an_absent_name_is_the_kernels_not_found() {
    let (clean, out, err) = run_case(&["-v", "absent"], &tree());
    assert!(!clean);
    assert!(out.is_empty());
    assert_eq!(err, "readlink: absent: not found\n");
}

#[test]
fn help_falls_back_to_the_usage_banner() {
    let out = MockOut::default();
    let err = MockOut::default();
    assert_eq!(
        run(Command::Help, None, &tree(), &NoHelp, &out, &err),
        Ok(true)
    );
    assert_eq!(out.text(), USAGE);
}

#[test]
fn a_failed_write_is_fatal() {
    let out = MockOut {
        text: RefCell::new(String::new()),
        fail: true,
    };
    assert_eq!(
        run(
            print(Options::default(), &["alias"]),
            None,
            &tree(),
            &NoHelp,
            &out,
            &MockOut::default()
        ),
        Err(ReadlinkError::Output(Errno::NotImplemented))
    );
}

// --- the bundled help documents -----------------------------------------

/// Every required locale's help document exists and names each switch the
/// parser accepts, so the two cannot drift apart.
#[test]
fn help_documents_the_parser_switches() {
    use std::fs;

    let help_root = format!("{}/Help", env!("CARGO_MANIFEST_DIR"));
    for locale in tairix_help::REQUIRED_LOCALES {
        let path = format!("{help_root}/{locale}/readlink.md");
        let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
        for switch in [
            "`-n, --no-newline`",
            "`-z, --zero`",
            "`-q, -s`",
            "`-v, --verbose`",
            "`-?, --help`",
        ] {
            assert!(
                text.contains(switch),
                "{locale}/readlink.md must document {switch}"
            );
        }
    }
}
