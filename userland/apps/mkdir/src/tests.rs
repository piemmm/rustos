//! Unit tests for the `mkdir` parse and creation engine, over in-memory
//! seams (no kernel).

use super::*;
use alloc::format;
use alloc::string::ToString;
use alloc::vec::Vec;
use core::cell::RefCell;
use tairix_help::SourceError;

extern crate std;
use std::collections::BTreeMap;

/// An in-memory [`Filesystem`]: a path → kind map plus the creation log.
struct MockFs {
    nodes: RefCell<BTreeMap<String, FileKind>>,
    /// Paths whose creation fails with the given errno, regardless of state.
    refuse: BTreeMap<String, Errno>,
}

impl MockFs {
    fn new(existing: &[(&str, FileKind)]) -> Self {
        Self {
            nodes: RefCell::new(
                existing
                    .iter()
                    .map(|(p, k)| ((*p).to_string(), *k))
                    .collect(),
            ),
            refuse: BTreeMap::new(),
        }
    }

    fn refusing(mut self, path: &str, errno: Errno) -> Self {
        self.refuse.insert(path.to_string(), errno);
        self
    }

    fn created(&self, path: &str) -> bool {
        self.nodes.borrow().get(path) == Some(&FileKind::Directory)
    }
}

impl Filesystem for MockFs {
    fn mkdir(&self, path: &str) -> Result<(), Errno> {
        if let Some(&errno) = self.refuse.get(path) {
            return Err(errno);
        }
        let mut nodes = self.nodes.borrow_mut();
        if nodes.contains_key(path) {
            return Err(Errno::AlreadyExists);
        }
        nodes.insert(path.to_string(), FileKind::Directory);
        Ok(())
    }

    fn kind(&self, path: &str) -> Result<FileKind, Errno> {
        self.nodes
            .borrow()
            .get(path)
            .copied()
            .ok_or(Errno::NotFound)
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

fn make(options: Options, paths: &[&str]) -> Command {
    Command::Make {
        options,
        paths: paths.iter().map(|p| (*p).to_string()).collect(),
    }
}

// --- parse ---------------------------------------------------------------

#[test]
fn a_single_operand_parses() {
    assert_eq!(parse(&["d"]), Ok(make(Options::default(), &["d"])));
}

#[test]
fn no_operand_is_usage() {
    assert_eq!(parse(&[]), Err(MkdirError::Usage));
    assert_eq!(parse(&["-p"]), Err(MkdirError::Usage));
}

#[test]
fn switches_and_clusters_parse() {
    let options = Options {
        parents: true,
        verbose: true,
    };
    assert_eq!(parse(&["-p", "-v", "d"]), Ok(make(options, &["d"])));
    assert_eq!(parse(&["-pv", "d"]), Ok(make(options, &["d"])));
    assert_eq!(
        parse(&["--parents", "--verbose", "d"]),
        Ok(make(options, &["d"]))
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
    assert_eq!(parse(&["-m", "755", "d"]), Err(MkdirError::Usage));
    assert_eq!(parse(&["--mode=755", "d"]), Err(MkdirError::Usage));
    assert_eq!(parse(&["-x", "d"]), Err(MkdirError::Usage));
}

#[test]
fn double_dash_ends_options_and_dash_is_an_operand() {
    assert_eq!(parse(&["--", "-p"]), Ok(make(Options::default(), &["-p"])));
    assert_eq!(parse(&["-"]), Ok(make(Options::default(), &["-"])));
}

// --- engine --------------------------------------------------------------

fn run_make(fs: &MockFs, options: Options, paths: &[&str]) -> Result<(), MkdirError> {
    run(make(options, paths), None, fs, &NoHelp, &MockOut::default())
}

#[test]
fn a_plain_creation_creates_exactly_the_operand() {
    let fs = MockFs::new(&[]);
    run_make(&fs, Options::default(), &["/Users/me/new"]).expect("create");
    assert!(fs.created("/Users/me/new"));
}

#[test]
fn an_existing_name_is_already_exists() {
    let fs = MockFs::new(&[("/d", FileKind::Directory)]);
    assert_eq!(
        run_make(&fs, Options::default(), &["/d"]),
        Err(MkdirError::Create {
            path: "/d".to_string(),
            errno: Errno::AlreadyExists,
        })
    );
}

#[test]
fn the_first_failure_stops_the_run() {
    let fs = MockFs::new(&[]).refusing("/b", Errno::PermissionDenied);
    assert_eq!(
        run_make(&fs, Options::default(), &["/a", "/b", "/c"]),
        Err(MkdirError::Create {
            path: "/b".to_string(),
            errno: Errno::PermissionDenied,
        })
    );
    assert!(fs.created("/a"));
    assert!(!fs.created("/c"));
}

#[test]
fn parents_creates_every_missing_ancestor() {
    let fs = MockFs::new(&[]);
    run_make(
        &fs,
        Options {
            parents: true,
            verbose: false,
        },
        &["/a/b/c"],
    )
    .expect("create chain");
    assert!(fs.created("/a"));
    assert!(fs.created("/a/b"));
    assert!(fs.created("/a/b/c"));
}

#[test]
fn parents_tolerates_existing_directories() {
    let fs = MockFs::new(&[("/a", FileKind::Directory), ("/a/b", FileKind::Directory)]);
    run_make(
        &fs,
        Options {
            parents: true,
            verbose: false,
        },
        &["/a/b/c"],
    )
    .expect("only the leaf is new");
    assert!(fs.created("/a/b/c"));
}

#[test]
fn parents_tolerates_an_operand_that_is_already_a_directory() {
    let fs = MockFs::new(&[("/a", FileKind::Directory)]);
    run_make(
        &fs,
        Options {
            parents: true,
            verbose: false,
        },
        &["/a"],
    )
    .expect("existing directory is not an error under -p");
}

#[test]
fn parents_fails_when_a_prefix_exists_as_a_file() {
    let fs = MockFs::new(&[("/a", FileKind::Regular)]);
    assert_eq!(
        run_make(
            &fs,
            Options {
                parents: true,
                verbose: false,
            },
            &["/a/b"],
        ),
        Err(MkdirError::Create {
            path: "/a".to_string(),
            errno: Errno::AlreadyExists,
        })
    );
}

#[test]
fn parents_walks_an_alias_rooted_operand() {
    let fs = MockFs::new(&[]);
    run_make(
        &fs,
        Options {
            parents: true,
            verbose: false,
        },
        &["Home:/tools/bin"],
    )
    .expect("create alias chain");
    assert!(fs.created("Home:/tools"));
    assert!(fs.created("Home:/tools/bin"));
}

#[test]
fn parents_on_a_bare_root_is_a_silent_success() {
    let fs = MockFs::new(&[]);
    run_make(
        &fs,
        Options {
            parents: true,
            verbose: false,
        },
        &["/"],
    )
    .expect("a bare root always exists");
}

#[test]
fn verbose_reports_each_created_directory_in_gnu_wording() {
    let fs = MockFs::new(&[("/a", FileKind::Directory)]);
    let out = MockOut::default();
    run(
        make(
            Options {
                parents: true,
                verbose: true,
            },
            &["/a/b/c"],
        ),
        None,
        &fs,
        &NoHelp,
        &out,
    )
    .expect("create chain");
    // Only the directories actually created are reported, outermost first.
    assert_eq!(
        out.text.borrow().as_str(),
        "mkdir: created directory '/a/b'\nmkdir: created directory '/a/b/c'\n"
    );
}

#[test]
fn a_failed_verbose_write_is_an_output_error() {
    let fs = MockFs::new(&[]);
    let out = MockOut {
        fail: true,
        ..MockOut::default()
    };
    assert_eq!(
        run(
            make(
                Options {
                    parents: false,
                    verbose: true,
                },
                &["/d"],
            ),
            None,
            &fs,
            &NoHelp,
            &out,
        ),
        Err(MkdirError::Output(Errno::NotImplemented))
    );
}

#[test]
fn help_falls_back_to_the_usage_banner() {
    let out = MockOut::default();
    run(Command::Help, None, &MockFs::new(&[]), &NoHelp, &out).expect("help");
    assert_eq!(out.text.borrow().as_str(), USAGE);
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
        let path = format!("{help_root}/{locale}/mkdir.md");
        let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
        for switch in ["`-p, --parents`", "`-v, --verbose`", "`-h, -?`"] {
            assert!(
                text.contains(switch),
                "{locale}/mkdir.md must document {switch}"
            );
        }
    }
}
