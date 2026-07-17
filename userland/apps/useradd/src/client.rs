//! The account-creation engine: refuse a name that is already taken, then
//! hand the new record to the database.

use alloc::string::String;

use tairix_help::{own_short_help, HelpSource};

use crate::command::Command;
use crate::error::UseraddError;
use crate::io::{Output, UserDb, UserSpec};

/// The usage banner a usage error is reported with, and the fallback the
/// short-help switches print when `useradd`'s own Help tree is unavailable.
pub const USAGE: &str = "\
usage: useradd [-u UID] -g GID [-G GID[,GID...]] [-c COMMENT] [-d HOME] [--] NAME

  -u, --uid UID       numeric user id (auto-allocated if omitted)
  -g, --gid GID       numeric primary group id (required)
  -G, --groups LIST   comma-separated numeric supplementary group ids
  -c, --comment TEXT  account comment / full name
  -d, --home PATH     home directory
  -h, -?, --help      show this command's own short help

UID, GID, and the LIST entries are decimal ids. NAME matches [a-z_][a-z0-9_-]*.
`--` ends option parsing: every later argument is an operand.
";

/// `useradd`'s own command word: the short-help switches render its own
/// Help document through the same engine as any other command's.
const OWN_WORD: &str = "useradd";

/// Run one [`Command`], creating the account through `db`. `locale` is the
/// user's `LANG` preference, if set; `help` is the tool's own `Help/` tree,
/// read by the short-help switches.
///
/// For a [`Command::Create`] the name is first checked against the database so
/// a duplicate can be reported precisely, then the new record is written.
/// `useradd` writes nothing on success; `out` carries only the
/// [`Command::Help`] render.
///
/// # Errors
///
/// * [`UseraddError::Exists`] — a user with the requested name already exists.
/// * [`UseraddError::Lookup`] — the database could not be consulted for the
///   name; carries the underlying [`Errno`](tairix_abi::Errno).
/// * [`UseraddError::Create`] — the database refused or failed the creation
///   (e.g. a missing `CAP_USER_ADMIN`, a duplicate uid, or an unknown group).
/// * [`UseraddError::Output`] — writing the short help failed.
pub fn run(
    command: Command,
    locale: Option<&str>,
    db: &dyn UserDb,
    help: &dyn HelpSource,
    out: &dyn Output,
) -> Result<(), UseraddError> {
    match command {
        Command::Help => short_help(locale, help, out),
        Command::Create(user) => {
            if db.name_in_use(&user.name).map_err(UseraddError::Lookup)? {
                return Err(UseraddError::Exists);
            }
            let spec = UserSpec {
                name: &user.name,
                uid: user.uid,
                primary_gid: user.primary_gid,
                supplementary_gids: &user.supplementary_gids,
                comment: user.comment.as_deref(),
                home: user.home.as_deref(),
            };
            db.create(&spec).map_err(UseraddError::Create)
        }
    }
}

/// Render `useradd`'s own short help (`NAME` + `SYNOPSIS` + compact
/// `OPTIONS`) from its own Help tree through the one shared engine; when no
/// document can be served (a build without the bundle's documents) the usage
/// banner stands in — the tool's own text, not fabricated help content — so
/// `-h` never fails.
fn short_help(
    locale: Option<&str>,
    help: &dyn HelpSource,
    out: &dyn Output,
) -> Result<(), UseraddError> {
    let bytes =
        own_short_help(help, locale, OWN_WORD).unwrap_or_else(|| String::from(USAGE).into_bytes());
    out.write_all(&bytes).map_err(UseraddError::Output)
}

#[cfg(test)]
mod tests {
    use super::{run, USAGE};
    use crate::command::parse;
    use crate::error::UseraddError;
    use crate::io::{Output, UserDb, UserSpec};
    use alloc::string::{String, ToString};
    use alloc::vec::Vec;
    use core::cell::RefCell;
    use tairix_abi::Errno;
    use tairix_help::{HelpSource, SourceError};

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

    /// A Help tree holding one canonical `useradd.md` document.
    struct OneDoc;

    const DOC: &str = "## NAME\n\nuseradd — create a user account\n\n\
                       ## SYNOPSIS\n\n`useradd -g GID [--] NAME`\n\n\
                       ## DESCRIPTION\n\nCreates an account.\n";

    impl HelpSource for OneDoc {
        fn locale_dirs(&self) -> Result<Vec<String>, SourceError> {
            Ok(alloc::vec![String::from("en-US")])
        }

        fn read(&self, locale_dir: &str, file_name: &str) -> Result<Option<Vec<u8>>, SourceError> {
            if locale_dir == "en-US" && file_name == "useradd.md" {
                Ok(Some(DOC.as_bytes().to_vec()))
            } else {
                Ok(None)
            }
        }
    }

    /// An in-memory user database. Holds the existing names, records every
    /// created account, and supports injecting a failure into either seam.
    struct MemDb {
        state: RefCell<State>,
    }

    struct State {
        existing: Vec<String>,
        lookup_fail: Option<Errno>,
        create_fail: Option<Errno>,
        created: Vec<Created>,
    }

    #[derive(Clone, Debug, Eq, PartialEq)]
    struct Created {
        name: String,
        uid: Option<u32>,
        primary_gid: u32,
        supplementary_gids: Vec<u32>,
        comment: Option<String>,
        home: Option<String>,
    }

    impl MemDb {
        fn new() -> Self {
            Self {
                state: RefCell::new(State {
                    existing: Vec::new(),
                    lookup_fail: None,
                    create_fail: None,
                    created: Vec::new(),
                }),
            }
        }

        fn with_user(self, name: &str) -> Self {
            self.state.borrow_mut().existing.push(name.to_string());
            self
        }

        fn lookup_fails(self, errno: Errno) -> Self {
            self.state.borrow_mut().lookup_fail = Some(errno);
            self
        }

        fn create_fails(self, errno: Errno) -> Self {
            self.state.borrow_mut().create_fail = Some(errno);
            self
        }

        fn created(&self) -> Vec<Created> {
            self.state.borrow().created.clone()
        }
    }

    impl UserDb for MemDb {
        fn name_in_use(&self, name: &str) -> Result<bool, Errno> {
            let state = self.state.borrow();
            if let Some(errno) = state.lookup_fail {
                return Err(errno);
            }
            Ok(state.existing.iter().any(|n| n == name))
        }

        fn create(&self, spec: &UserSpec<'_>) -> Result<(), Errno> {
            let mut state = self.state.borrow_mut();
            if let Some(errno) = state.create_fail {
                return Err(errno);
            }
            state.created.push(Created {
                name: spec.name.to_string(),
                uid: spec.uid,
                primary_gid: spec.primary_gid,
                supplementary_gids: spec.supplementary_gids.to_vec(),
                comment: spec.comment.map(String::from),
                home: spec.home.map(String::from),
            });
            state.existing.push(spec.name.to_string());
            Ok(())
        }
    }

    /// A terminal that records every byte written to it.
    struct Recorder {
        bytes: RefCell<Vec<u8>>,
    }

    impl Recorder {
        fn new() -> Self {
            Self {
                bytes: RefCell::new(Vec::new()),
            }
        }

        fn written(&self) -> Vec<u8> {
            self.bytes.borrow().clone()
        }
    }

    impl Output for Recorder {
        fn write_all(&self, bytes: &[u8]) -> Result<(), Errno> {
            self.bytes.borrow_mut().extend_from_slice(bytes);
            Ok(())
        }
    }

    /// An output seam that always fails, to drive the help-write failure path.
    struct FailingOutput;

    impl Output for FailingOutput {
        fn write_all(&self, _bytes: &[u8]) -> Result<(), Errno> {
            Err(Errno::PermissionDenied)
        }
    }

    fn run_args(args: &[&str], db: &MemDb, out: &Recorder) -> Result<(), UseraddError> {
        run(parse(args).expect("valid command"), None, db, &NoHelp, out)
    }

    #[test]
    fn help_without_a_document_falls_back_to_the_usage_banner() {
        let db = MemDb::new();
        let out = Recorder::new();
        assert_eq!(run_args(&["--help"], &db, &out), Ok(()));
        assert_eq!(out.written(), USAGE.as_bytes());
    }

    #[test]
    fn help_renders_the_own_document_when_served() {
        let db = MemDb::new();
        let out = Recorder::new();
        assert_eq!(
            run(parse(&["-h"]).expect("valid"), None, &db, &OneDoc, &out),
            Ok(())
        );
        let text = String::from_utf8(out.written()).expect("utf-8 render");
        assert!(text.contains("useradd"));
        assert!(text.contains("create a user account"));
        assert!(
            !text.contains("Creates an account."),
            "short help omits DESCRIPTION"
        );
    }

    #[test]
    fn a_minimal_account_is_created() {
        let db = MemDb::new();
        let out = Recorder::new();
        assert_eq!(run_args(&["-g", "100", "alice"], &db, &out), Ok(()));
        let created = db.created();
        assert_eq!(created.len(), 1);
        assert_eq!(created[0].name, "alice");
        assert_eq!(created[0].uid, None);
        assert_eq!(created[0].primary_gid, 100);
        assert!(created[0].supplementary_gids.is_empty());
        assert_eq!(created[0].comment, None);
        assert_eq!(created[0].home, None);
        // Silent on success.
        assert!(out.written().is_empty());
    }

    #[test]
    fn every_field_reaches_the_database() {
        let db = MemDb::new();
        let out = Recorder::new();
        assert_eq!(
            run_args(
                &[
                    "-u",
                    "1000",
                    "-g",
                    "100",
                    "-G",
                    "10,20",
                    "-c",
                    "Alice A",
                    "-d",
                    "/Users/alice",
                    "alice",
                ],
                &db,
                &out,
            ),
            Ok(())
        );
        let created = db.created();
        assert_eq!(created[0].uid, Some(1000));
        assert_eq!(created[0].primary_gid, 100);
        assert_eq!(created[0].supplementary_gids, [10, 20]);
        assert_eq!(created[0].comment.as_deref(), Some("Alice A"));
        assert_eq!(created[0].home.as_deref(), Some("/Users/alice"));
    }

    #[test]
    fn an_existing_name_is_refused_without_writing() {
        let db = MemDb::new().with_user("alice");
        let out = Recorder::new();
        assert_eq!(
            run_args(&["-g", "100", "alice"], &db, &out),
            Err(UseraddError::Exists)
        );
        assert!(db.created().is_empty());
    }

    #[test]
    fn a_lookup_error_surfaces_and_nothing_is_created() {
        let db = MemDb::new().lookup_fails(Errno::PermissionDenied);
        let out = Recorder::new();
        assert_eq!(
            run_args(&["-g", "100", "alice"], &db, &out),
            Err(UseraddError::Lookup(Errno::PermissionDenied))
        );
        assert!(db.created().is_empty());
    }

    #[test]
    fn a_create_error_surfaces() {
        // The database is the policy point: a missing CAP_USER_ADMIN is its
        // call to make, surfaced here as Create(PermissionDenied).
        let db = MemDb::new().create_fails(Errno::PermissionDenied);
        let out = Recorder::new();
        assert_eq!(
            run_args(&["-g", "100", "alice"], &db, &out),
            Err(UseraddError::Create(Errno::PermissionDenied))
        );
        assert!(db.created().is_empty());
    }

    #[test]
    fn an_unknown_group_surfaces_as_create_not_found() {
        let db = MemDb::new().create_fails(Errno::NotFound);
        let out = Recorder::new();
        assert_eq!(
            run_args(&["-g", "999", "alice"], &db, &out),
            Err(UseraddError::Create(Errno::NotFound))
        );
    }

    #[test]
    fn a_help_write_failure_surfaces() {
        let db = MemDb::new();
        let out = FailingOutput;
        assert_eq!(
            run(parse(&["--help"]).expect("valid"), None, &db, &NoHelp, &out),
            Err(UseraddError::Output(Errno::PermissionDenied))
        );
    }
}
