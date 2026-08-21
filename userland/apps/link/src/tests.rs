//! Unit tests for the `link` parse and linking engine, over in-memory seams
//! (no kernel).

use super::*;
use alloc::format;
use alloc::string::ToString;
use core::cell::RefCell;
use tairix_help::SourceError;

extern crate std;
use std::collections::BTreeMap;

/// An in-memory [`Filesystem`]: a scripted `(existing, new)` pair answers
/// its own errno, every other pair succeeds, and each created link is
/// logged in order.
struct MockFs {
    refuse: BTreeMap<(String, String), Errno>,
    created: RefCell<Vec<(String, String)>>,
}

impl MockFs {
    fn new() -> Self {
        Self {
            refuse: BTreeMap::new(),
            created: RefCell::new(Vec::new()),
        }
    }

    fn refusing(mut self, existing: &str, new: &str, errno: Errno) -> Self {
        self.refuse
            .insert((existing.to_string(), new.to_string()), errno);
        self
    }

    fn created(&self) -> Vec<(String, String)> {
        self.created.borrow().clone()
    }
}

impl Filesystem for MockFs {
    fn link(&self, existing: &str, new: &str) -> Result<(), Errno> {
        if let Some(&errno) = self.refuse.get(&(existing.to_string(), new.to_string())) {
            return Err(errno);
        }
        self.created
            .borrow_mut()
            .push((existing.to_string(), new.to_string()));
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

fn create(existing: &str, new: &str) -> Command {
    Command::Create {
        existing: existing.to_string(),
        new: new.to_string(),
    }
}

// --- parse ---------------------------------------------------------------

#[test]
fn exactly_two_operands_parse_in_order() {
    // The first operand is the node that gains a name, the second the name
    // created — the GNU order, and the one the seam takes.
    assert_eq!(parse(&["kept", "second"]), Ok(create("kept", "second")));
}

#[test]
fn fewer_than_two_operands_is_usage() {
    assert_eq!(parse(&[]), Err(LinkError::Usage));
    assert_eq!(parse(&["only"]), Err(LinkError::Usage));
}

#[test]
fn a_third_operand_is_usage_and_nothing_is_linked() {
    let fs = MockFs::new();
    assert_eq!(parse(&["a", "b", "c"]), Err(LinkError::Usage));
    assert!(fs.created().is_empty());
}

#[test]
fn help_switches_are_the_reserved_pair() {
    assert_eq!(parse(&["-?"]), Ok(Command::Help));
    assert_eq!(parse(&["--help"]), Ok(Command::Help));
}

#[test]
fn the_ln_option_surface_is_refused_rather_than_ignored() {
    // `link` is deliberately optionless: silently accepting `-s` would make
    // it create a *symbolic* link when the caller asked for a hard one.
    for arg in ["-s", "-f", "-i", "-v", "-L", "-P", "--symbolic", "--force"] {
        assert_eq!(parse(&[arg, "a", "b"]), Err(LinkError::Usage), "{arg}");
    }
}

#[test]
fn double_dash_makes_dashed_names_linkable() {
    assert_eq!(parse(&["--", "-a", "-b"]), Ok(create("-a", "-b")));
}

#[test]
fn a_bare_dash_is_a_name() {
    assert_eq!(parse(&["-", "second"]), Ok(create("-", "second")));
}

// --- run -----------------------------------------------------------------

#[test]
fn the_link_is_created_from_the_operands_as_typed() {
    let fs = MockFs::new();
    assert_eq!(
        run(
            create("kept", "second"),
            None,
            &fs,
            &NoHelp,
            &MockOut::default()
        ),
        Ok(())
    );
    assert_eq!(
        fs.created(),
        Vec::from([("kept".to_string(), "second".to_string())])
    );
}

#[test]
fn each_kernel_refusal_reaches_the_caller_unchanged() {
    // The five refusals say five different things, so none may be collapsed
    // into another on the way out: an occupied name is not a format limit,
    // and a cross-volume pair is not a permission problem.
    for errno in [
        Errno::AlreadyExists,
        Errno::IsADirectory,
        Errno::CrossVolume,
        Errno::TooManyLinks,
        Errno::NotSupported,
    ] {
        let fs = MockFs::new().refusing("kept", "second", errno);
        assert_eq!(
            run(
                create("kept", "second"),
                None,
                &fs,
                &NoHelp,
                &MockOut::default()
            ),
            Err(LinkError::Create {
                existing: "kept".to_string(),
                new: "second".to_string(),
                errno,
            })
        );
        assert!(fs.created().is_empty());
    }
}

#[test]
fn help_falls_back_to_the_usage_banner() {
    let out = MockOut::default();
    assert_eq!(
        run(Command::Help, None, &MockFs::new(), &NoHelp, &out),
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
        run(Command::Help, None, &MockFs::new(), &NoHelp, &out),
        Err(LinkError::Output(Errno::NotImplemented))
    );
}

#[test]
fn errors_name_both_operands() {
    assert_eq!(
        format!(
            "{}",
            LinkError::Create {
                existing: "a".to_string(),
                new: "b".to_string(),
                errno: Errno::CrossVolume,
            }
        ),
        "cannot create link 'b' to 'a': paths on different volumes"
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
        let path = format!("{help_root}/{locale}/link.md");
        let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
        assert!(
            text.contains("`-?, --help`"),
            "{locale}/link.md must document `-?, --help`"
        );
    }
}
