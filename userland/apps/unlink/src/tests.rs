//! Unit tests for the `unlink` parse and removal engine, over in-memory
//! seams (no kernel).

use super::*;
use alloc::format;
use alloc::string::ToString;
use alloc::vec::Vec;
use core::cell::RefCell;
use tairix_help::SourceError;

extern crate std;
use std::collections::{BTreeMap, BTreeSet};

/// An in-memory [`Filesystem`]: a name present is removable, a scripted
/// name answers its own errno, and every removal is logged in order.
struct MockFs {
    names: RefCell<BTreeSet<String>>,
    refuse: BTreeMap<String, Errno>,
    removed: RefCell<Vec<String>>,
}

impl MockFs {
    fn new(names: &[&str]) -> Self {
        Self {
            names: RefCell::new(names.iter().map(|p| (*p).to_string()).collect()),
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
    fn unlink(&self, path: &str) -> Result<(), Errno> {
        if let Some(&errno) = self.refuse.get(path) {
            return Err(errno);
        }
        if !self.names.borrow_mut().remove(path) {
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

fn remove(path: &str) -> Command {
    Command::Remove(path.to_string())
}

// --- parse ---------------------------------------------------------------

#[test]
fn exactly_one_operand_parses() {
    assert_eq!(parse(&["gone"]), Ok(remove("gone")));
}

#[test]
fn no_operand_is_usage() {
    assert_eq!(parse(&[]), Err(UnlinkError::Usage));
}

#[test]
fn a_second_operand_is_usage_and_nothing_is_removed() {
    // GNU `unlink` takes exactly one name; a second is far likelier a
    // mistake than an intention, so the run refuses before touching either.
    let fs = MockFs::new(&["a", "b"]);
    assert_eq!(parse(&["a", "b"]), Err(UnlinkError::Usage));
    assert!(fs.removed().is_empty());
}

#[test]
fn help_switches_are_the_reserved_pair() {
    assert_eq!(parse(&["-?"]), Ok(Command::Help));
    assert_eq!(parse(&["--help"]), Ok(Command::Help));
}

#[test]
fn an_unknown_option_is_usage() {
    // The tool has no options beyond short help — not even the GNU-family
    // `-f`/`-v` — so every other dash argument is refused rather than
    // silently treated as a name.
    assert_eq!(parse(&["-f", "x"]), Err(UnlinkError::Usage));
    assert_eq!(parse(&["--force", "x"]), Err(UnlinkError::Usage));
    assert_eq!(parse(&["-r"]), Err(UnlinkError::Usage));
}

#[test]
fn double_dash_makes_a_dashed_name_removable() {
    assert_eq!(parse(&["--", "-f"]), Ok(remove("-f")));
    // And a second operand after `--` is still one too many.
    assert_eq!(parse(&["--", "-f", "-g"]), Err(UnlinkError::Usage));
}

#[test]
fn a_bare_dash_is_a_name() {
    assert_eq!(parse(&["-"]), Ok(remove("-")));
}

// --- run -----------------------------------------------------------------

#[test]
fn the_one_operand_is_removed() {
    let fs = MockFs::new(&["gone"]);
    assert_eq!(
        run(remove("gone"), None, &fs, &NoHelp, &MockOut::default()),
        Ok(())
    );
    assert_eq!(fs.removed(), Vec::from(["gone".to_string()]));
}

#[test]
fn a_refused_removal_carries_the_kernels_errno() {
    // A directory is the kernel's refusal, not this tool's guess: the empty
    // flag word asks for a non-directory removal, so the refusal is decided
    // in the same locked walk that would have removed the entry.
    let fs = MockFs::new(&["dir"]).refusing("dir", Errno::IsADirectory);
    assert_eq!(
        run(remove("dir"), None, &fs, &NoHelp, &MockOut::default()),
        Err(UnlinkError::Remove {
            path: "dir".to_string(),
            errno: Errno::IsADirectory,
        })
    );
    assert!(fs.removed().is_empty());
}

#[test]
fn a_missing_name_is_reported_not_ignored() {
    // There is no `-f`, so an absent name is an error: the tool that removes
    // exactly one name reports when it removed none.
    let fs = MockFs::new(&[]);
    assert_eq!(
        run(remove("absent"), None, &fs, &NoHelp, &MockOut::default()),
        Err(UnlinkError::Remove {
            path: "absent".to_string(),
            errno: Errno::NotFound,
        })
    );
}

#[test]
fn help_falls_back_to_the_usage_banner() {
    let out = MockOut::default();
    assert_eq!(
        run(Command::Help, None, &MockFs::new(&[]), &NoHelp, &out),
        Ok(())
    );
    assert_eq!(out.text.borrow().as_str(), USAGE);
}

#[test]
fn a_failed_help_write_is_reported() {
    let out = MockOut {
        text: RefCell::new(String::new()),
        fail: true,
    };
    assert_eq!(
        run(Command::Help, None, &MockFs::new(&[]), &NoHelp, &out),
        Err(UnlinkError::Output(Errno::NotImplemented))
    );
}

#[test]
fn errors_render_the_gnu_wording() {
    assert_eq!(
        format!(
            "{}",
            UnlinkError::Remove {
                path: "d".to_string(),
                errno: Errno::IsADirectory,
            }
        ),
        "cannot unlink 'd': is a directory"
    );
}

// --- the bundled help documents -----------------------------------------

/// Every required locale's help document exists and names the one switch
/// the parser accepts, so the two cannot drift apart.
#[test]
fn help_documents_the_parser_switches() {
    use std::fs;

    let help_root = format!("{}/Help", env!("CARGO_MANIFEST_DIR"));
    for locale in tairix_help::REQUIRED_LOCALES {
        let path = format!("{help_root}/{locale}/unlink.md");
        let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
        assert!(
            text.contains("`-?, --help`"),
            "{locale}/unlink.md must document `-?, --help`"
        );
    }
}
