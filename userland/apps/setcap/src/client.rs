//! The gate-setting engine: stat each operand to learn its kind, apply the
//! new capability gate, and — with `-R` — descend into directories applying
//! the same gate.

use alloc::vec::Vec;

use tairix_abi::CapabilityId;
use tairix_path::join;

use crate::command::Command;
use crate::error::SetcapError;
use crate::io::{Entry, EntryKind, FileSystem, Output};

/// The usage banner printed by [`Command::Help`].
pub const USAGE: &str = "\
usage: setcap [-R] [--] CAP file...

  -R, --recursive  change files and directories recursively
  -h, --help       show this message

CAP is a canonical capability name (e.g. CAP_AUDIT_READ) to install as the
file's gate, or `-` to clear the gate. `--` ends option parsing: every later
argument is an operand.
";

/// Run one [`Command`], setting the capability gate of its files through
/// `fs`.
///
/// Each file is inspected to learn its kind, the new gate is applied, and
/// with `-R` a directory is changed and then its contents are changed
/// recursively. `setcap` writes nothing on success; `out` carries only the
/// [`Command::Help`] banner.
///
/// The first failure stops the run before any later operand (fail closed).
///
/// # Errors
///
/// * [`SetcapError::Stat`] — an operand could not be inspected; carries the
///   underlying [`Errno`](tairix_abi::Errno).
/// * [`SetcapError::Apply`] — applying the new gate failed.
/// * [`SetcapError::Read`] — a directory's entries could not be read during a
///   recursive descent.
/// * [`SetcapError::Output`] — writing the usage banner failed.
pub fn run(command: Command, fs: &dyn FileSystem, out: &dyn Output) -> Result<(), SetcapError> {
    match command {
        Command::Help => out.write_all(USAGE.as_bytes()).map_err(SetcapError::Output),
        Command::Set {
            recursive,
            cap,
            files,
        } => {
            for file in &files {
                let kind = fs.stat(file).map_err(SetcapError::Stat)?;
                change(file, kind, cap, recursive, fs)?;
            }
            Ok(())
        }
    }
}

/// Apply `cap` to `path` (already known to be `kind`), then — when
/// `recursive` and `path` is a directory — to each of its entries in turn,
/// changing the directory before its contents.
fn change(
    path: &str,
    kind: EntryKind,
    cap: Option<CapabilityId>,
    recursive: bool,
    fs: &dyn FileSystem,
) -> Result<(), SetcapError> {
    fs.set_cap(path, cap).map_err(SetcapError::Apply)?;
    if recursive && kind == EntryKind::Directory {
        for entry in read_children(path, fs)? {
            let child = join(path, &entry.name);
            change(&child, entry.kind, cap, recursive, fs)?;
        }
    }
    Ok(())
}

/// Read all entries of the directory `path` into a vector, so the recursive
/// descent does not depend on entry indices staying stable as gates change.
fn read_children(path: &str, fs: &dyn FileSystem) -> Result<Vec<Entry>, SetcapError> {
    let mut entries = Vec::new();
    let mut index: u64 = 0;
    while let Some(entry) = fs.read_dir(path, index).map_err(SetcapError::Read)? {
        index = index.saturating_add(1);
        entries.push(entry);
    }
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::{run, USAGE};
    use crate::command::parse;
    use crate::error::SetcapError;
    use crate::io::{Entry, EntryKind, FileSystem, Output};
    use alloc::string::{String, ToString};
    use alloc::vec::Vec;
    use core::cell::RefCell;
    use tairix_abi::{CapabilityId, Errno};

    /// An in-memory tree. Each node carries its kind and current gate; a
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
        applied: Vec<(String, Option<CapabilityId>)>,
    }

    struct Node {
        path: String,
        kind: EntryKind,
        cap: Option<CapabilityId>,
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

        fn node(self, path: &str, kind: EntryKind, cap: Option<CapabilityId>) -> Self {
            self.state.borrow_mut().nodes.push(Node {
                path: path.to_string(),
                kind,
                cap,
            });
            self
        }

        fn file(self, path: &str, cap: Option<CapabilityId>) -> Self {
            self.node(path, EntryKind::File, cap)
        }

        fn dir(self, path: &str, cap: Option<CapabilityId>) -> Self {
            self.node(path, EntryKind::Directory, cap)
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

        /// The gate stored at `path`. Panics if the test fixture has no such
        /// node (a test-only invariant, so `expect` is appropriate here).
        fn cap_of(&self, path: &str) -> Option<CapabilityId> {
            self.state
                .borrow()
                .nodes
                .iter()
                .find(|n| n.path == path)
                .map(|n| n.cap)
                .expect("node exists in fixture")
        }

        fn applied(&self) -> Vec<(String, Option<CapabilityId>)> {
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
        fn stat(&self, path: &str) -> Result<EntryKind, Errno> {
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
                .map(|n| n.kind)
                .ok_or(Errno::NotFound)
        }

        fn set_cap(&self, path: &str, cap: Option<CapabilityId>) -> Result<(), Errno> {
            let mut state = self.state.borrow_mut();
            if let Some((p, errno)) = &state.set_fail {
                if p == path {
                    return Err(*errno);
                }
            }
            match state.nodes.iter_mut().find(|n| n.path == path) {
                Some(node) => node.cap = cap,
                None => return Err(Errno::NotFound),
            }
            state.applied.push((path.to_string(), cap));
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

    fn run_args(args: &[&str], fs: &MemFs, out: &Recorder) -> Result<(), SetcapError> {
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
    fn a_named_capability_installs_the_gate() {
        let fs = MemFs::new().file("/f", None);
        let out = Recorder::new();
        assert_eq!(run_args(&["CAP_AUDIT_READ", "/f"], &fs, &out), Ok(()));
        assert_eq!(fs.cap_of("/f"), Some(CapabilityId::AUDIT_READ));
        assert_eq!(
            fs.applied(),
            [("/f".to_string(), Some(CapabilityId::AUDIT_READ))]
        );
        assert!(out.written().is_empty());
    }

    #[test]
    fn the_dash_clears_an_existing_gate() {
        let fs = MemFs::new().file("/f", Some(CapabilityId::FS_MOUNT));
        let out = Recorder::new();
        assert_eq!(run_args(&["-", "/f"], &fs, &out), Ok(()));
        assert_eq!(fs.cap_of("/f"), None);
    }

    #[test]
    fn several_files_are_all_changed() {
        let fs = MemFs::new()
            .file("/a", None)
            .file("/b", None)
            .file("/c", None);
        let out = Recorder::new();
        assert_eq!(
            run_args(&["CAP_NET_RAW", "/a", "/b", "/c"], &fs, &out),
            Ok(())
        );
        assert_eq!(fs.cap_of("/a"), Some(CapabilityId::NET_RAW));
        assert_eq!(fs.cap_of("/b"), Some(CapabilityId::NET_RAW));
        assert_eq!(fs.cap_of("/c"), Some(CapabilityId::NET_RAW));
    }

    #[test]
    fn without_recursive_only_the_named_directory_changes() {
        let fs = MemFs::new()
            .dir("/d", None)
            .file("/d/f", Some(CapabilityId::FS_MOUNT));
        let out = Recorder::new();
        assert_eq!(run_args(&["CAP_NET_RAW", "/d"], &fs, &out), Ok(()));
        assert_eq!(fs.cap_of("/d"), Some(CapabilityId::NET_RAW));
        // The child is untouched.
        assert_eq!(fs.cap_of("/d/f"), Some(CapabilityId::FS_MOUNT));
    }

    #[test]
    fn recursive_changes_the_directory_then_its_contents() {
        let fs = MemFs::new()
            .dir("/d", None)
            .file("/d/f", None)
            .dir("/d/sub", None)
            .file("/d/sub/g", None);
        let out = Recorder::new();
        assert_eq!(
            run_args(&["-R", "CAP_AUDIT_WRITE", "/d"], &fs, &out),
            Ok(())
        );
        assert_eq!(fs.cap_of("/d"), Some(CapabilityId::AUDIT_WRITE));
        assert_eq!(fs.cap_of("/d/f"), Some(CapabilityId::AUDIT_WRITE));
        assert_eq!(fs.cap_of("/d/sub"), Some(CapabilityId::AUDIT_WRITE));
        assert_eq!(fs.cap_of("/d/sub/g"), Some(CapabilityId::AUDIT_WRITE));
        // The directory itself is changed before its contents.
        let applied = fs.applied();
        assert_eq!(applied[0].0, "/d");
    }

    #[test]
    fn a_missing_operand_is_stat_and_stops_the_run() {
        let fs = MemFs::new().file("/b", None);
        let out = Recorder::new();
        // `/a` does not exist: the run fails before reaching `/b`.
        assert_eq!(
            run_args(&["CAP_NET_RAW", "/a", "/b"], &fs, &out),
            Err(SetcapError::Stat(Errno::NotFound))
        );
        assert_eq!(fs.cap_of("/b"), None);
    }

    #[test]
    fn a_stat_permission_error_surfaces() {
        let fs = MemFs::new()
            .file("/f", None)
            .stat_fails("/f", Errno::PermissionDenied);
        let out = Recorder::new();
        assert_eq!(
            run_args(&["CAP_NET_RAW", "/f"], &fs, &out),
            Err(SetcapError::Stat(Errno::PermissionDenied))
        );
    }

    #[test]
    fn an_apply_error_surfaces() {
        let fs = MemFs::new()
            .file("/f", None)
            .set_fails("/f", Errno::PermissionDenied);
        let out = Recorder::new();
        assert_eq!(
            run_args(&["CAP_NET_RAW", "/f"], &fs, &out),
            Err(SetcapError::Apply(Errno::PermissionDenied))
        );
        // The gate is unchanged.
        assert_eq!(fs.cap_of("/f"), None);
    }

    #[test]
    fn a_read_dir_error_during_recursion_surfaces() {
        let fs = MemFs::new()
            .dir("/d", None)
            .file("/d/f", None)
            .read_fails("/d", Errno::PermissionDenied);
        let out = Recorder::new();
        assert_eq!(
            run_args(&["-R", "CAP_NET_RAW", "/d"], &fs, &out),
            Err(SetcapError::Read(Errno::PermissionDenied))
        );
        // The directory itself was changed before the read failed.
        assert_eq!(fs.cap_of("/d"), Some(CapabilityId::NET_RAW));
    }
}
