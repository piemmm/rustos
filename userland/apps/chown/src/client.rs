//! The owner-changing engine: stat each operand to learn its kind, apply the
//! new owner, and — with `-R` — descend into directories applying the same
//! owner.

use alloc::format;
use alloc::vec::Vec;

use crate::command::{Command, Options, Owner, Verbosity};
use crate::error::ChownError;
use crate::io::{Entry, EntryKind, FileSystem, Output};
use tairix_path::join;

/// The usage banner printed by [`Command::Help`].
pub const USAGE: &str = "\
usage: chown [-cfRv] [--] OWNER[:GROUP] file...

  -R, --recursive       change files and directories recursively
  -c, --changes         report only files whose ownership actually changed
  -v, --verbose         report every file processed
  -f, --silent, --quiet suppress most error messages
  -h, --help            show this message

OWNER and GROUP are decimal ids. Forms: OWNER (user only), OWNER:GROUP (both),
or :GROUP (group only). `--` ends option parsing: every later argument is an
operand.
";

/// Run one [`Command`], changing the owner of its files through `fs`.
///
/// Each file is inspected to learn its kind, the new owner is applied, and
/// with `-R` a directory is changed and then its contents are changed
/// recursively. `-v` reports every file processed on `out` and `-c` only
/// those whose ownership actually changed; otherwise `chown` writes nothing
/// on success beyond the [`Command::Help`] banner.
///
/// The first failure stops the run before any later operand (fail closed) —
/// except under `-f`, which suppresses each failing operand's diagnostic,
/// continues with the remaining operands, and fails the whole run afterwards
/// as the message-less [`ChownError::Silenced`].
///
/// # Errors
///
/// * [`ChownError::Stat`] — an operand could not be inspected; carries the
///   underlying [`Errno`](tairix_abi::Errno).
/// * [`ChownError::Apply`] — applying the new owner failed.
/// * [`ChownError::Read`] — a directory's entries could not be read during a
///   recursive descent.
/// * [`ChownError::Silenced`] — one or more operands failed under `-f`.
/// * [`ChownError::Output`] — writing the usage banner or a report failed.
pub fn run(command: Command, fs: &dyn FileSystem, out: &dyn Output) -> Result<(), ChownError> {
    match command {
        Command::Help => out.write_all(USAGE.as_bytes()).map_err(ChownError::Output),
        Command::Change {
            options,
            owner,
            files,
        } => {
            let mut silenced = false;
            for file in &files {
                match change(file, None, &owner, options, fs, out) {
                    Ok(()) => {}
                    // A report that cannot be written is never suppressed:
                    // `-f` silences filesystem failures, not a dead terminal.
                    Err(ChownError::Output(errno)) => return Err(ChownError::Output(errno)),
                    Err(_) if options.quiet => silenced = true,
                    Err(err) => return Err(err),
                }
            }
            if silenced {
                return Err(ChownError::Silenced);
            }
            Ok(())
        }
    }
}

/// Apply `owner` to `path`, then — when `-R` and `path` is a directory — to
/// each of its entries in turn, changing the directory before its contents.
///
/// `kind_hint` is the kind the parent's directory entry already reported;
/// it is trusted only when no report is wanted, because the `-c`/`-v`
/// reports also need the node's current ownership from a fresh stat.
fn change(
    path: &str,
    kind_hint: Option<EntryKind>,
    owner: &Owner,
    options: Options,
    fs: &dyn FileSystem,
    out: &dyn Output,
) -> Result<(), ChownError> {
    let (kind, old) = if let (Some(kind), Verbosity::None) = (kind_hint, options.verbosity) {
        (kind, None)
    } else {
        let meta = fs.stat(path).map_err(ChownError::Stat)?;
        (meta.kind, Some((meta.uid, meta.gid)))
    };
    fs.set_owner(path, owner.uid, owner.gid)
        .map_err(ChownError::Apply)?;
    if let Some((user_before, group_before)) = old {
        report(path, user_before, group_before, owner, options, out)?;
    }
    if options.recursive && kind == EntryKind::Directory {
        for entry in read_children(path, fs)? {
            let child = join(path, &entry.name);
            change(&child, Some(entry.kind), owner, options, fs, out)?;
        }
    }
    Ok(())
}

/// Report one processed file in the GNU wording, per the verbosity policy:
/// `-v` reports every file, `-c` only an actual change. The spelling
/// follows the owner spec's shape — a group-only spec reports the group, a
/// user-only spec the user, and a full spec `uid:gid`.
fn report(
    path: &str,
    user_before: u32,
    group_before: u32,
    owner: &Owner,
    options: Options,
    out: &dyn Output,
) -> Result<(), ChownError> {
    let changed = owner.uid.is_some_and(|uid| uid != user_before)
        || owner.gid.is_some_and(|gid| gid != group_before);
    let wanted = match options.verbosity {
        Verbosity::None => false,
        Verbosity::Changes => changed,
        Verbosity::All => true,
    };
    if !wanted {
        return Ok(());
    }
    let line = match (owner.uid, owner.gid) {
        (Some(uid), Some(gid)) => {
            if changed {
                format!(
                    "changed ownership of '{path}' from {user_before}:{group_before} to {uid}:{gid}\n"
                )
            } else {
                format!("ownership of '{path}' retained as {user_before}:{group_before}\n")
            }
        }
        (Some(uid), None) => {
            if changed {
                format!("changed ownership of '{path}' from {user_before} to {uid}\n")
            } else {
                format!("ownership of '{path}' retained as {user_before}\n")
            }
        }
        (None, Some(gid)) => {
            if changed {
                format!("changed group of '{path}' from {group_before} to {gid}\n")
            } else {
                format!("group of '{path}' retained as {group_before}\n")
            }
        }
        // At least one field is always set in a parsed owner spec.
        (None, None) => return Ok(()),
    };
    out.write_all(line.as_bytes()).map_err(ChownError::Output)
}

/// Read all entries of the directory `path` into a vector, so the recursive
/// descent does not depend on entry indices staying stable as ownership
/// changes.
fn read_children(path: &str, fs: &dyn FileSystem) -> Result<Vec<Entry>, ChownError> {
    let mut entries = Vec::new();
    let mut index: u64 = 0;
    while let Some(entry) = fs.read_dir(path, index).map_err(ChownError::Read)? {
        index = index.saturating_add(1);
        entries.push(entry);
    }
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::{run, USAGE};
    use crate::command::parse;
    use crate::error::ChownError;
    use crate::io::{Entry, EntryKind, FileSystem, Metadata, Output};
    use alloc::string::{String, ToString};
    use alloc::vec::Vec;
    use core::cell::RefCell;
    use tairix_abi::Errno;

    /// An in-memory tree. Each node carries its kind and current owner; a
    /// directory's children are derived by parent path. Failure injection
    /// covers the stat/apply/read fail-closed paths.
    struct MemFs {
        state: RefCell<State>,
    }

    struct State {
        nodes: Vec<Node>,
        stat_fail: Option<(String, Errno)>,
        set_fail: Option<(String, Errno)>,
        read_fail: Option<(String, Errno)>,
        applied: Vec<(String, Option<u32>, Option<u32>)>,
    }

    struct Node {
        path: String,
        kind: EntryKind,
        uid: u32,
        gid: u32,
    }

    impl MemFs {
        fn new() -> Self {
            Self {
                state: RefCell::new(State {
                    nodes: Vec::new(),
                    stat_fail: None,
                    set_fail: None,
                    read_fail: None,
                    applied: Vec::new(),
                }),
            }
        }

        fn node(self, path: &str, kind: EntryKind, uid: u32, gid: u32) -> Self {
            self.state.borrow_mut().nodes.push(Node {
                path: path.to_string(),
                kind,
                uid,
                gid,
            });
            self
        }

        fn file(self, path: &str, uid: u32, gid: u32) -> Self {
            self.node(path, EntryKind::File, uid, gid)
        }

        fn dir(self, path: &str, uid: u32, gid: u32) -> Self {
            self.node(path, EntryKind::Directory, uid, gid)
        }

        fn stat_fails(self, path: &str, errno: Errno) -> Self {
            self.state.borrow_mut().stat_fail = Some((path.to_string(), errno));
            self
        }

        fn set_fails(self, path: &str, errno: Errno) -> Self {
            self.state.borrow_mut().set_fail = Some((path.to_string(), errno));
            self
        }

        fn read_fails(self, path: &str, errno: Errno) -> Self {
            self.state.borrow_mut().read_fail = Some((path.to_string(), errno));
            self
        }

        fn owner_of(&self, path: &str) -> Option<(u32, u32)> {
            self.state
                .borrow()
                .nodes
                .iter()
                .find(|n| n.path == path)
                .map(|n| (n.uid, n.gid))
        }

        fn applied(&self) -> Vec<(String, Option<u32>, Option<u32>)> {
            self.state.borrow().applied.clone()
        }
    }

    /// The immediate parent of an absolute path: `/a/b` maps to `/a`, and a
    /// top-level `/a` maps to the empty string (its children sit at the root).
    fn parent_of(path: &str) -> &str {
        match path.rfind('/') {
            Some(slash) => &path[..slash],
            None => "",
        }
    }

    /// The final component of a path.
    fn name_of(path: &str) -> &str {
        match path.rfind('/') {
            Some(slash) => &path[slash + 1..],
            None => path,
        }
    }

    impl FileSystem for MemFs {
        fn stat(&self, path: &str) -> Result<Metadata, Errno> {
            let state = self.state.borrow();
            if let Some((p, errno)) = &state.stat_fail {
                if p == path {
                    return Err(*errno);
                }
            }
            state
                .nodes
                .iter()
                .find(|n| n.path == path)
                .map(|n| Metadata {
                    kind: n.kind,
                    uid: n.uid,
                    gid: n.gid,
                })
                .ok_or(Errno::NotFound)
        }

        fn set_owner(&self, path: &str, uid: Option<u32>, gid: Option<u32>) -> Result<(), Errno> {
            let mut state = self.state.borrow_mut();
            if let Some((p, errno)) = &state.set_fail {
                if p == path {
                    return Err(*errno);
                }
            }
            match state.nodes.iter_mut().find(|n| n.path == path) {
                Some(node) => {
                    if let Some(uid) = uid {
                        node.uid = uid;
                    }
                    if let Some(gid) = gid {
                        node.gid = gid;
                    }
                }
                None => return Err(Errno::NotFound),
            }
            state.applied.push((path.to_string(), uid, gid));
            Ok(())
        }

        fn read_dir(&self, path: &str, index: u64) -> Result<Option<Entry>, Errno> {
            let state = self.state.borrow();
            if let Some((p, errno)) = &state.read_fail {
                if p == path {
                    return Err(*errno);
                }
            }
            let mut children: Vec<Entry> = state
                .nodes
                .iter()
                .filter(|n| parent_of(&n.path) == path && n.path.as_str() != path)
                .map(|n| Entry {
                    name: name_of(&n.path).to_string(),
                    kind: n.kind,
                })
                .collect();
            children.sort_by(|a, b| a.name.cmp(&b.name));
            Ok(children
                .into_iter()
                .nth(usize::try_from(index).unwrap_or(usize::MAX)))
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

    fn run_args(args: &[&str], fs: &MemFs, out: &Recorder) -> Result<(), ChownError> {
        run(parse(args).expect("valid command"), fs, out)
    }

    #[test]
    fn help_prints_the_usage_banner() {
        let fs = MemFs::new();
        let out = Recorder::new();
        assert_eq!(run_args(&["--help"], &fs, &out), Ok(()));
        assert_eq!(out.written(), USAGE.as_bytes());
    }

    #[test]
    fn an_owner_only_spec_changes_the_user_and_leaves_the_group() {
        let fs = MemFs::new().file("/f", 1, 2);
        let out = Recorder::new();
        assert_eq!(run_args(&["1000", "/f"], &fs, &out), Ok(()));
        assert_eq!(fs.owner_of("/f"), Some((1000, 2)));
        assert_eq!(fs.applied(), [("/f".to_string(), Some(1000), None)]);
        assert!(out.written().is_empty());
    }

    #[test]
    fn an_owner_group_pair_changes_both() {
        let fs = MemFs::new().file("/f", 1, 2);
        let out = Recorder::new();
        assert_eq!(run_args(&["1000:100", "/f"], &fs, &out), Ok(()));
        assert_eq!(fs.owner_of("/f"), Some((1000, 100)));
    }

    #[test]
    fn a_group_only_spec_leaves_the_user() {
        let fs = MemFs::new().file("/f", 7, 2);
        let out = Recorder::new();
        assert_eq!(run_args(&[":100", "/f"], &fs, &out), Ok(()));
        assert_eq!(fs.owner_of("/f"), Some((7, 100)));
    }

    #[test]
    fn several_files_are_all_changed() {
        let fs = MemFs::new()
            .file("/a", 0, 0)
            .file("/b", 0, 0)
            .file("/c", 0, 0);
        let out = Recorder::new();
        assert_eq!(run_args(&["5:6", "/a", "/b", "/c"], &fs, &out), Ok(()));
        assert_eq!(fs.owner_of("/a"), Some((5, 6)));
        assert_eq!(fs.owner_of("/b"), Some((5, 6)));
        assert_eq!(fs.owner_of("/c"), Some((5, 6)));
    }

    #[test]
    fn without_recursive_only_the_named_directory_changes() {
        let fs = MemFs::new().dir("/d", 0, 0).file("/d/f", 1, 1);
        let out = Recorder::new();
        assert_eq!(run_args(&["9:9", "/d"], &fs, &out), Ok(()));
        assert_eq!(fs.owner_of("/d"), Some((9, 9)));
        // The child is untouched.
        assert_eq!(fs.owner_of("/d/f"), Some((1, 1)));
    }

    #[test]
    fn recursive_changes_the_directory_then_its_contents() {
        let fs = MemFs::new()
            .dir("/d", 0, 0)
            .file("/d/f", 1, 1)
            .dir("/d/sub", 0, 0)
            .file("/d/sub/g", 2, 2);
        let out = Recorder::new();
        assert_eq!(run_args(&["-R", "100:100", "/d"], &fs, &out), Ok(()));
        assert_eq!(fs.owner_of("/d"), Some((100, 100)));
        assert_eq!(fs.owner_of("/d/f"), Some((100, 100)));
        assert_eq!(fs.owner_of("/d/sub"), Some((100, 100)));
        assert_eq!(fs.owner_of("/d/sub/g"), Some((100, 100)));
        // The directory itself is changed before its contents.
        let applied = fs.applied();
        assert_eq!(applied[0].0, "/d");
    }

    #[test]
    fn a_missing_operand_is_stat_and_stops_the_run() {
        let fs = MemFs::new().file("/b", 0, 0);
        let out = Recorder::new();
        // `/a` does not exist: the run fails before reaching `/b`.
        assert_eq!(
            run_args(&["5:5", "/a", "/b"], &fs, &out),
            Err(ChownError::Stat(Errno::NotFound))
        );
        assert_eq!(fs.owner_of("/b"), Some((0, 0)));
    }

    #[test]
    fn a_stat_permission_error_surfaces() {
        let fs = MemFs::new()
            .file("/f", 0, 0)
            .stat_fails("/f", Errno::PermissionDenied);
        let out = Recorder::new();
        assert_eq!(
            run_args(&["5", "/f"], &fs, &out),
            Err(ChownError::Stat(Errno::PermissionDenied))
        );
    }

    #[test]
    fn an_apply_error_surfaces() {
        let fs = MemFs::new()
            .file("/f", 0, 0)
            .set_fails("/f", Errno::PermissionDenied);
        let out = Recorder::new();
        assert_eq!(
            run_args(&["5", "/f"], &fs, &out),
            Err(ChownError::Apply(Errno::PermissionDenied))
        );
        // The owner is unchanged.
        assert_eq!(fs.owner_of("/f"), Some((0, 0)));
    }

    #[test]
    fn a_read_dir_error_during_recursion_surfaces() {
        let fs = MemFs::new()
            .dir("/d", 0, 0)
            .file("/d/f", 0, 0)
            .read_fails("/d", Errno::PermissionDenied);
        let out = Recorder::new();
        assert_eq!(
            run_args(&["-R", "5:5", "/d"], &fs, &out),
            Err(ChownError::Read(Errno::PermissionDenied))
        );
        // The directory itself was changed before the read failed.
        assert_eq!(fs.owner_of("/d"), Some((5, 5)));
    }

    #[test]
    fn verbose_reports_changed_and_retained_in_gnu_wording() {
        let fs = MemFs::new().file("/f", 1000, 100);
        let out = Recorder::new();
        assert_eq!(run_args(&["-v", "0:0", "/f"], &fs, &out), Ok(()));
        assert_eq!(
            out.written(),
            b"changed ownership of '/f' from 1000:100 to 0:0\n"
        );
        let out = Recorder::new();
        assert_eq!(run_args(&["-v", "0:0", "/f"], &fs, &out), Ok(()));
        assert_eq!(out.written(), b"ownership of '/f' retained as 0:0\n");
    }

    #[test]
    fn changes_reports_only_an_actual_change_per_spec_shape() {
        let fs = MemFs::new().file("/f", 1000, 100);
        let out = Recorder::new();
        // The owner is already 1000: a user-only `-c` reports nothing.
        assert_eq!(run_args(&["-c", "1000", "/f"], &fs, &out), Ok(()));
        assert!(out.written().is_empty());
        let out = Recorder::new();
        assert_eq!(run_args(&["-c", "0", "/f"], &fs, &out), Ok(()));
        assert_eq!(out.written(), b"changed ownership of '/f' from 1000 to 0\n");
        // A group-only spec reports the group in the GNU wording.
        let out = Recorder::new();
        assert_eq!(run_args(&["-v", ":100", "/f"], &fs, &out), Ok(()));
        assert_eq!(out.written(), b"group of '/f' retained as 100\n");
    }

    #[test]
    fn quiet_suppresses_the_failure_but_still_fails_the_run() {
        let fs = MemFs::new().file("/b", 1000, 100);
        let out = Recorder::new();
        // `/a` is missing: `-f` keeps going (so `/b` is still changed) and
        // the run fails afterwards with the message-less error.
        assert_eq!(
            run_args(&["-f", "0:0", "/a", "/b"], &fs, &out),
            Err(ChownError::Silenced)
        );
        assert_eq!(fs.owner_of("/b"), Some((0, 0)));
        assert!(out.written().is_empty());
    }
}
