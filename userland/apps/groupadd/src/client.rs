//! The group-creation engine: refuse a name that is already taken, then hand
//! the new record to the database.

use crate::command::Command;
use crate::error::GroupaddError;
use crate::io::{GroupDb, GroupSpec, Output};

/// The usage banner printed by [`Command::Help`].
pub const USAGE: &str = "\
usage: groupadd [-g GID] [--] NAME

  -g, --gid GID   numeric group id (auto-allocated if omitted)
  -h, --help      show this message

GID is a decimal id. NAME matches [a-z_][a-z0-9_-]*.
`--` ends option parsing: every later argument is an operand.
";

/// Run one [`Command`], creating the group through `db`.
///
/// For a [`Command::Create`] the name is first checked against the database so
/// a duplicate can be reported precisely, then the new record is written.
/// `groupadd` writes nothing on success; `out` carries only the
/// [`Command::Help`] banner.
///
/// # Errors
///
/// * [`GroupaddError::Exists`] — a group with the requested name already
///   exists.
/// * [`GroupaddError::Lookup`] — the database could not be consulted for the
///   name; carries the underlying [`Errno`](rustos_abi::Errno).
/// * [`GroupaddError::Create`] — the database refused or failed the creation
///   (e.g. a missing `CAP_USER_ADMIN` or a duplicate gid).
/// * [`GroupaddError::Output`] — writing the usage banner failed.
pub fn run(command: Command, db: &dyn GroupDb, out: &dyn Output) -> Result<(), GroupaddError> {
    match command {
        Command::Help => out
            .write_all(USAGE.as_bytes())
            .map_err(GroupaddError::Output),
        Command::Create(group) => {
            if db.name_in_use(&group.name).map_err(GroupaddError::Lookup)? {
                return Err(GroupaddError::Exists);
            }
            let spec = GroupSpec {
                name: &group.name,
                gid: group.gid,
            };
            db.create(&spec).map_err(GroupaddError::Create)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{run, USAGE};
    use crate::command::parse;
    use crate::error::GroupaddError;
    use crate::io::{GroupDb, GroupSpec, Output};
    use alloc::string::{String, ToString};
    use alloc::vec::Vec;
    use core::cell::RefCell;
    use rustos_abi::Errno;

    /// An in-memory group database. Holds the existing names, records every
    /// created group, and supports injecting a failure into either seam.
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
        gid: Option<u32>,
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

        fn with_group(self, name: &str) -> Self {
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

    impl GroupDb for MemDb {
        fn name_in_use(&self, name: &str) -> Result<bool, Errno> {
            let state = self.state.borrow();
            if let Some(errno) = state.lookup_fail {
                return Err(errno);
            }
            Ok(state.existing.iter().any(|n| n == name))
        }

        fn create(&self, spec: &GroupSpec<'_>) -> Result<(), Errno> {
            let mut state = self.state.borrow_mut();
            if let Some(errno) = state.create_fail {
                return Err(errno);
            }
            state.created.push(Created {
                name: spec.name.to_string(),
                gid: spec.gid,
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

    fn run_args(args: &[&str], db: &MemDb, out: &Recorder) -> Result<(), GroupaddError> {
        run(parse(args).expect("valid command"), db, out)
    }

    #[test]
    fn help_prints_the_usage_banner() {
        let db = MemDb::new();
        let out = Recorder::new();
        assert_eq!(run_args(&["--help"], &db, &out), Ok(()));
        assert_eq!(out.written(), USAGE.as_bytes());
    }

    #[test]
    fn a_minimal_group_is_created_with_no_gid() {
        let db = MemDb::new();
        let out = Recorder::new();
        assert_eq!(run_args(&["staff"], &db, &out), Ok(()));
        let created = db.created();
        assert_eq!(created.len(), 1);
        assert_eq!(created[0].name, "staff");
        assert_eq!(created[0].gid, None);
        // Silent on success.
        assert!(out.written().is_empty());
    }

    #[test]
    fn a_requested_gid_reaches_the_database() {
        let db = MemDb::new();
        let out = Recorder::new();
        assert_eq!(run_args(&["-g", "100", "staff"], &db, &out), Ok(()));
        let created = db.created();
        assert_eq!(created[0].gid, Some(100));
    }

    #[test]
    fn an_existing_name_is_refused_without_writing() {
        let db = MemDb::new().with_group("staff");
        let out = Recorder::new();
        assert_eq!(run_args(&["staff"], &db, &out), Err(GroupaddError::Exists));
        assert!(db.created().is_empty());
    }

    #[test]
    fn a_lookup_error_surfaces_and_nothing_is_created() {
        let db = MemDb::new().lookup_fails(Errno::PermissionDenied);
        let out = Recorder::new();
        assert_eq!(
            run_args(&["staff"], &db, &out),
            Err(GroupaddError::Lookup(Errno::PermissionDenied))
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
            run_args(&["staff"], &db, &out),
            Err(GroupaddError::Create(Errno::PermissionDenied))
        );
        assert!(db.created().is_empty());
    }

    #[test]
    fn a_taken_gid_surfaces_as_create_out_of_range() {
        let db = MemDb::new().create_fails(Errno::OutOfRange);
        let out = Recorder::new();
        assert_eq!(
            run_args(&["-g", "0", "staff"], &db, &out),
            Err(GroupaddError::Create(Errno::OutOfRange))
        );
    }

    #[test]
    fn a_help_write_failure_surfaces() {
        let db = MemDb::new();
        let out = FailingOutput;
        assert_eq!(
            run(parse(&["--help"]).expect("valid"), &db, &out),
            Err(GroupaddError::Output(Errno::PermissionDenied))
        );
    }
}
