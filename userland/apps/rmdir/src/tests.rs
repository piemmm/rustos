//! Unit tests for the `rmdir` parse and removal engine, over in-memory
//! seams (no kernel).

use super::*;
use alloc::format;
use alloc::string::ToString;
use alloc::vec::Vec;
use core::cell::RefCell;
use tairix_help::SourceError;

extern crate std;
use std::collections::{BTreeMap, BTreeSet};

/// An in-memory [`Filesystem`]: each path answers with a scripted result,
/// and every successful removal is logged in order.
struct MockFs {
    /// Paths that exist as empty directories (removable).
    dirs: RefCell<BTreeSet<String>>,
    /// Paths whose removal fails with the given errno, regardless of state.
    refuse: BTreeMap<String, Errno>,
    removed: RefCell<Vec<String>>,
}

impl MockFs {
    fn new(dirs: &[&str]) -> Self {
        Self {
            dirs: RefCell::new(dirs.iter().map(|p| (*p).to_string()).collect()),
            refuse: BTreeMap::new(),
            removed: RefCell::new(Vec::new()),
        }
    }

    fn refusing(mut self, path: &str, errno: Errno) -> Self {
        self.refuse.insert(path.to_string(), errno);
        self
    }

    fn removed(&self) -> Vec<String> {
        self.removed.borrow().clone()
    }
}

impl Filesystem for MockFs {
    fn rmdir(&self, path: &str) -> Result<(), Errno> {
        if let Some(&errno) = self.refuse.get(path) {
            return Err(errno);
        }
        if !self.dirs.borrow_mut().remove(path) {
            return Err(Errno::NotFound);
        }
        self.removed.borrow_mut().push(path.to_string());
        Ok(())
    }
}

/// A terminal fixture capturing everything written.
#[derive(Default)]
struct MockOut {
    text: RefCell<String>,
    fail: bool,
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

fn remove(options: Options, paths: &[&str]) -> Command {
    Command::Remove {
        options,
        paths: paths.iter().map(|p| (*p).to_string()).collect(),
    }
}

fn run_remove(fs: &MockFs, options: Options, paths: &[&str]) -> Result<(), RmdirError> {
    run(
        remove(options, paths),
        None,
        fs,
        &NoHelp,
        &MockOut::default(),
    )
}

// --- parse ---------------------------------------------------------------

#[test]
fn a_single_operand_parses() {
    assert_eq!(parse(&["d"]), Ok(remove(Options::default(), &["d"])));
}

#[test]
fn no_operand_is_usage() {
    assert_eq!(parse(&[]), Err(RmdirError::Usage));
    assert_eq!(parse(&["-p"]), Err(RmdirError::Usage));
}

#[test]
fn switches_and_clusters_parse() {
    let options = Options {
        parents: true,
        verbose: true,
        ignore_non_empty: true,
    };
    assert_eq!(
        parse(&["-p", "-v", "--ignore-fail-on-non-empty", "d"]),
        Ok(remove(options, &["d"]))
    );
    assert_eq!(
        parse(&["-pv", "--ignore-fail-on-non-empty", "d"]),
        Ok(remove(options, &["d"]))
    );
    assert_eq!(
        parse(&["--parents", "--verbose", "--ignore-fail-on-non-empty", "d"]),
        Ok(remove(options, &["d"]))
    );
}

#[test]
fn help_spellings_parse() {
    assert_eq!(parse(&["-h"]), Ok(Command::Help));
    assert_eq!(parse(&["-?"]), Ok(Command::Help));
    assert_eq!(parse(&["--help"]), Ok(Command::Help));
}

#[test]
fn unknown_switches_are_usage() {
    assert_eq!(parse(&["-r", "d"]), Err(RmdirError::Usage));
    assert_eq!(parse(&["--recursive", "d"]), Err(RmdirError::Usage));
}

#[test]
fn double_dash_ends_options_and_dash_is_an_operand() {
    assert_eq!(
        parse(&["--", "-p"]),
        Ok(remove(Options::default(), &["-p"]))
    );
    assert_eq!(parse(&["-"]), Ok(remove(Options::default(), &["-"])));
}

// --- engine --------------------------------------------------------------

#[test]
fn a_plain_removal_removes_exactly_the_operand() {
    let fs = MockFs::new(&["/a", "/a/b"]);
    run_remove(&fs, Options::default(), &["/a/b"]).expect("remove");
    assert_eq!(fs.removed(), ["/a/b"]);
}

#[test]
fn the_kernel_refusals_surface_with_their_path() {
    let fs = MockFs::new(&[]).refusing("/plain", Errno::NotADirectory);
    assert_eq!(
        run_remove(&fs, Options::default(), &["/plain"]),
        Err(RmdirError::Remove {
            path: "/plain".to_string(),
            errno: Errno::NotADirectory,
        })
    );
    let fs = MockFs::new(&[]);
    assert_eq!(
        run_remove(&fs, Options::default(), &["/missing"]),
        Err(RmdirError::Remove {
            path: "/missing".to_string(),
            errno: Errno::NotFound,
        })
    );
}

#[test]
fn the_first_failure_stops_the_run() {
    let fs = MockFs::new(&["/a", "/c"]).refusing("/b", Errno::PermissionDenied);
    assert_eq!(
        run_remove(&fs, Options::default(), &["/a", "/b", "/c"]),
        Err(RmdirError::Remove {
            path: "/b".to_string(),
            errno: Errno::PermissionDenied,
        })
    );
    assert_eq!(fs.removed(), ["/a"]);
}

#[test]
fn parents_removes_each_ancestor_innermost_first() {
    let fs = MockFs::new(&["/a", "/a/b", "/a/b/c"]);
    run_remove(
        &fs,
        Options {
            parents: true,
            ..Options::default()
        },
        &["/a/b/c"],
    )
    .expect("remove chain");
    assert_eq!(fs.removed(), ["/a/b/c", "/a/b", "/a"]);
}

#[test]
fn parents_never_asks_to_remove_the_bare_root() {
    let fs = MockFs::new(&["/a"]);
    run_remove(
        &fs,
        Options {
            parents: true,
            ..Options::default()
        },
        &["/a"],
    )
    .expect("remove");
    assert_eq!(fs.removed(), ["/a"]);
}

#[test]
fn parents_walks_an_alias_rooted_operand() {
    let fs = MockFs::new(&["Home:/tools", "Home:/tools/bin"]);
    run_remove(
        &fs,
        Options {
            parents: true,
            ..Options::default()
        },
        &["Home:/tools/bin"],
    )
    .expect("remove chain");
    assert_eq!(fs.removed(), ["Home:/tools/bin", "Home:/tools"]);
}

#[test]
fn a_non_empty_ancestor_fails_the_parents_walk() {
    let fs = MockFs::new(&["/a/b"]).refusing("/a", Errno::NotEmpty);
    assert_eq!(
        run_remove(
            &fs,
            Options {
                parents: true,
                ..Options::default()
            },
            &["/a/b"],
        ),
        Err(RmdirError::Remove {
            path: "/a".to_string(),
            errno: Errno::NotEmpty,
        })
    );
    assert_eq!(fs.removed(), ["/a/b"]);
}

#[test]
fn ignore_fail_on_non_empty_tolerates_exactly_that_refusal() {
    // The tolerated refusal ends the operand's walk without error…
    let fs = MockFs::new(&["/a/b"]).refusing("/a", Errno::NotEmpty);
    run_remove(
        &fs,
        Options {
            parents: true,
            ignore_non_empty: true,
            ..Options::default()
        },
        &["/a/b"],
    )
    .expect("a populated ancestor is tolerated");
    assert_eq!(fs.removed(), ["/a/b"]);
    // …and it also covers a plain (non -p) operand…
    let fs = MockFs::new(&[]).refusing("/full", Errno::NotEmpty);
    run_remove(
        &fs,
        Options {
            ignore_non_empty: true,
            ..Options::default()
        },
        &["/full"],
    )
    .expect("a populated operand is tolerated");
    // …but no other refusal is.
    let fs = MockFs::new(&[]).refusing("/plain", Errno::NotADirectory);
    assert_eq!(
        run_remove(
            &fs,
            Options {
                ignore_non_empty: true,
                ..Options::default()
            },
            &["/plain"],
        ),
        Err(RmdirError::Remove {
            path: "/plain".to_string(),
            errno: Errno::NotADirectory,
        })
    );
}

#[test]
fn verbose_reports_each_attempt_in_gnu_wording() {
    let fs = MockFs::new(&["/a", "/a/b"]);
    let out = MockOut::default();
    run(
        remove(
            Options {
                parents: true,
                verbose: true,
                ..Options::default()
            },
            &["/a/b"],
        ),
        None,
        &fs,
        &NoHelp,
        &out,
    )
    .expect("remove chain");
    assert_eq!(
        out.text.borrow().as_str(),
        "rmdir: removing directory, '/a/b'\nrmdir: removing directory, '/a'\n"
    );
}

#[test]
fn a_failed_verbose_write_is_an_output_error() {
    let fs = MockFs::new(&["/d"]);
    let out = MockOut {
        fail: true,
        ..MockOut::default()
    };
    assert_eq!(
        run(
            remove(
                Options {
                    verbose: true,
                    ..Options::default()
                },
                &["/d"],
            ),
            None,
            &fs,
            &NoHelp,
            &out,
        ),
        Err(RmdirError::Output(Errno::NotImplemented))
    );
    // The failed report stopped the run before the removal.
    assert_eq!(fs.removed(), Vec::<String>::new());
}

#[test]
fn help_falls_back_to_the_usage_banner() {
    let out = MockOut::default();
    run(Command::Help, None, &MockFs::new(&[]), &NoHelp, &out).expect("help");
    assert_eq!(out.text.borrow().as_str(), USAGE);
}

/// The banner is complete as written, so a consumer emits it verbatim.
///
/// The usage-error path used to hand it to the line-appending stderr writer,
/// which followed it with a blank line GNU `rmdir` never prints while the
/// help path above wrote the same constant unchanged.
#[test]
fn the_usage_banner_ends_in_exactly_one_newline() {
    assert!(USAGE.ends_with('\n'));
    assert!(!USAGE.ends_with("\n\n"));
}

/// Every locale's `OPTIONS` section documents exactly the switches this
/// parser accepts (`plans/APPS.md` §3.1): the flag tokens are
/// language-neutral, so each translated document must carry the same keys
/// as the canonical one. The documents are read from the bundle's own
/// on-disk `Help/` tree — the single source the image builder plants —
/// never a copy embedded in this crate.
#[test]
fn help_documents_the_parser_switches() {
    use std::fs;

    let help_root = format!("{}/Help", env!("CARGO_MANIFEST_DIR"));
    let locales = tairix_help::REQUIRED_LOCALES;
    for locale in locales {
        let path = format!("{help_root}/{locale}/rmdir.md");
        let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
        for switch in [
            "`-p, --parents`",
            "`-v, --verbose`",
            "`--ignore-fail-on-non-empty`",
            "`-h, -?`",
        ] {
            assert!(
                text.contains(switch),
                "{locale}/rmdir.md must document {switch}"
            );
        }
    }
}
