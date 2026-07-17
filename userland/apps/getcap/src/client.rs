//! The reporting engine: stat each operand to learn its kind, read its
//! capability gate, render the gated files, and — with `-R` — descend into
//! directories reporting their contents.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use tairix_abi::CapabilityId;

use crate::command::Command;
use crate::error::GetcapError;
use crate::io::{Entry, EntryKind, FileSystem, Output};

/// The usage banner printed by [`Command::Help`].
pub const USAGE: &str = "\
usage: getcap [-R] [--] file...

  -R, --recursive  report files and directories recursively
  -h, --help       show this message

For each file that carries a capability gate, getcap prints `path CAP_NAME`.
A file with no gate produces no output. `--` ends option parsing: every later
argument is an operand.
";

/// Run one [`Command`], reporting the capability gate of its files through
/// `fs`.
///
/// Each file is inspected to learn its kind, its gate is read, and a gated
/// file is rendered as `path CAP_NAME`; with `-R` a directory is reported and
/// then its contents recursively. A file with no gate produces no output.
///
/// The first failure stops the run before any later operand (fail closed).
///
/// # Errors
///
/// * [`GetcapError::Stat`] — an operand could not be inspected; carries the
///   underlying [`Errno`](tairix_abi::Errno).
/// * [`GetcapError::Query`] — a node's capability gate could not be read.
/// * [`GetcapError::Read`] — a directory's entries could not be read during a
///   recursive descent.
/// * [`GetcapError::Output`] — writing the report or the usage banner failed.
pub fn run(command: Command, fs: &dyn FileSystem, out: &dyn Output) -> Result<(), GetcapError> {
    match command {
        Command::Help => out.write_all(USAGE.as_bytes()).map_err(GetcapError::Output),
        Command::Report { recursive, files } => {
            for file in &files {
                let kind = fs.stat(file).map_err(GetcapError::Stat)?;
                report(file, kind, recursive, fs, out)?;
            }
            Ok(())
        }
    }
}

/// Report the gate of `path` (already known to be `kind`), then — when
/// `recursive` and `path` is a directory — each of its entries in turn,
/// reporting the directory before its contents.
fn report(
    path: &str,
    kind: EntryKind,
    recursive: bool,
    fs: &dyn FileSystem,
    out: &dyn Output,
) -> Result<(), GetcapError> {
    if let Some(cap) = fs.capability(path).map_err(GetcapError::Query)? {
        out.write_all(render(path, cap).as_bytes())
            .map_err(GetcapError::Output)?;
    }
    if recursive && kind == EntryKind::Directory {
        for entry in read_children(path, fs)? {
            let child = join(path, &entry.name);
            report(&child, entry.kind, recursive, fs, out)?;
        }
    }
    Ok(())
}

/// Render one report line: `path CAP_NAME\n`.
///
/// A capability assigned a canonical name in `abi-v1` renders by that name;
/// an in-range identifier `abi-v1` has not yet named (so a node stored a
/// gate the running ABI does not name) renders as `CAP_<id>` rather than
/// being dropped, so the gate is never silently hidden.
fn render(path: &str, cap: CapabilityId) -> String {
    match cap.name() {
        Some(name) => format!("{path} {name}\n"),
        None => format!("{path} CAP_{}\n", cap.as_u16()),
    }
}

/// Read all entries of the directory `path` into a vector, so the recursive
/// descent does not depend on entry indices staying stable.
fn read_children(path: &str, fs: &dyn FileSystem) -> Result<Vec<Entry>, GetcapError> {
    let mut entries = Vec::new();
    let mut index: u64 = 0;
    while let Some(entry) = fs.read_dir(path, index).map_err(GetcapError::Read)? {
        index = index.saturating_add(1);
        entries.push(entry);
    }
    Ok(entries)
}

/// Join a directory `parent` and a child `name` into a path, inserting a
/// single `/` unless `parent` already ends with one.
fn join(parent: &str, name: &str) -> String {
    let mut path = String::with_capacity(parent.len() + 1 + name.len());
    path.push_str(parent);
    if !parent.ends_with('/') {
        path.push('/');
    }
    path.push_str(name);
    path
}

#[cfg(test)]
mod tests {
    use super::{run, USAGE};
    use crate::command::parse;
    use crate::error::GetcapError;
    use crate::io::{Entry, EntryKind, FileSystem, Output};
    use alloc::string::{String, ToString};
    use alloc::vec::Vec;
    use core::cell::RefCell;
    use tairix_abi::{CapabilityId, Errno};

    /// An in-memory tree. Each node carries its kind and optional capability
    /// gate; a directory's children are derived by parent path. Failure
    /// injection covers the stat/query/read fail-closed paths.
    struct MemFs {
        state: RefCell<State>,
    }

    struct State {
        nodes: Vec<Node>,
        stat_fail: Option<(String, Errno)>,
        query_fail: Option<(String, Errno)>,
        read_fail: Option<(String, Errno)>,
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
                    query_fail: None,
                    read_fail: None,
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

        fn query_fails(self, path: &str, errno: Errno) -> Self {
            self.state.borrow_mut().query_fail = Some((path.to_string(), errno));
            self
        }

        fn read_fails(self, path: &str, errno: Errno) -> Self {
            self.state.borrow_mut().read_fail = Some((path.to_string(), errno));
            self
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

        fn capability(&self, path: &str) -> Result<Option<CapabilityId>, Errno> {
            let state = self.state.borrow();
            if let Some((p, errno)) = &state.query_fail {
                if p == path {
                    return Err(*errno);
                }
            }
            state
                .nodes
                .iter()
                .find(|n| n.path == path)
                .map(|n| n.cap)
                .ok_or(Errno::NotFound)
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

        fn text(&self) -> String {
            String::from_utf8(self.bytes.borrow().clone()).expect("utf-8")
        }
    }

    impl Output for Recorder {
        fn write_all(&self, bytes: &[u8]) -> Result<(), Errno> {
            self.bytes.borrow_mut().extend_from_slice(bytes);
            Ok(())
        }
    }

    fn run_args(args: &[&str], fs: &MemFs, out: &Recorder) -> Result<(), GetcapError> {
        run(parse(args).expect("valid command"), fs, out)
    }

    #[test]
    fn help_prints_the_usage_banner() {
        let fs = MemFs::new();
        let out = Recorder::new();
        assert_eq!(run_args(&["--help"], &fs, &out), Ok(()));
        assert_eq!(out.text(), USAGE);
    }

    #[test]
    fn a_gated_file_is_reported_by_name() {
        let fs = MemFs::new().file("/f", Some(CapabilityId::AUDIT_READ));
        let out = Recorder::new();
        assert_eq!(run_args(&["/f"], &fs, &out), Ok(()));
        assert_eq!(out.text(), "/f CAP_AUDIT_READ\n");
    }

    #[test]
    fn an_ungated_file_produces_no_output() {
        let fs = MemFs::new().file("/f", None);
        let out = Recorder::new();
        assert_eq!(run_args(&["/f"], &fs, &out), Ok(()));
        assert!(out.text().is_empty());
    }

    #[test]
    fn an_unnamed_in_range_gate_renders_numerically() {
        let cap = CapabilityId::from_raw(200).expect("in range");
        let fs = MemFs::new().file("/f", Some(cap));
        let out = Recorder::new();
        assert_eq!(run_args(&["/f"], &fs, &out), Ok(()));
        assert_eq!(out.text(), "/f CAP_200\n");
    }

    #[test]
    fn several_files_report_only_the_gated_ones_in_order() {
        let fs = MemFs::new()
            .file("/a", Some(CapabilityId::FS_MOUNT))
            .file("/b", None)
            .file("/c", Some(CapabilityId::NET_RAW));
        let out = Recorder::new();
        assert_eq!(run_args(&["/a", "/b", "/c"], &fs, &out), Ok(()));
        assert_eq!(out.text(), "/a CAP_FS_MOUNT\n/c CAP_NET_RAW\n");
    }

    #[test]
    fn without_recursive_only_the_named_directory_is_reported() {
        let fs = MemFs::new()
            .dir("/d", Some(CapabilityId::FS_MOUNT))
            .file("/d/f", Some(CapabilityId::NET_RAW));
        let out = Recorder::new();
        assert_eq!(run_args(&["/d"], &fs, &out), Ok(()));
        // The child is not visited.
        assert_eq!(out.text(), "/d CAP_FS_MOUNT\n");
    }

    #[test]
    fn recursive_reports_the_directory_then_its_contents() {
        let fs = MemFs::new()
            .dir("/d", Some(CapabilityId::FS_MOUNT))
            .file("/d/f", None)
            .dir("/d/sub", None)
            .file("/d/sub/g", Some(CapabilityId::AUDIT_WRITE));
        let out = Recorder::new();
        assert_eq!(run_args(&["-R", "/d"], &fs, &out), Ok(()));
        // The directory's own gate first, then the deep gated file; the
        // ungated nodes contribute nothing.
        assert_eq!(out.text(), "/d CAP_FS_MOUNT\n/d/sub/g CAP_AUDIT_WRITE\n");
    }

    #[test]
    fn a_missing_operand_is_stat_and_stops_the_run() {
        let fs = MemFs::new().file("/b", Some(CapabilityId::FS_MOUNT));
        let out = Recorder::new();
        assert_eq!(
            run_args(&["/a", "/b"], &fs, &out),
            Err(GetcapError::Stat(Errno::NotFound))
        );
        assert!(out.text().is_empty());
    }

    #[test]
    fn a_stat_permission_error_surfaces() {
        let fs = MemFs::new()
            .file("/f", None)
            .stat_fails("/f", Errno::PermissionDenied);
        let out = Recorder::new();
        assert_eq!(
            run_args(&["/f"], &fs, &out),
            Err(GetcapError::Stat(Errno::PermissionDenied))
        );
    }

    #[test]
    fn a_query_error_surfaces() {
        let fs = MemFs::new()
            .file("/f", Some(CapabilityId::FS_MOUNT))
            .query_fails("/f", Errno::PermissionDenied);
        let out = Recorder::new();
        assert_eq!(
            run_args(&["/f"], &fs, &out),
            Err(GetcapError::Query(Errno::PermissionDenied))
        );
    }

    #[test]
    fn a_read_dir_error_during_recursion_surfaces() {
        let fs = MemFs::new()
            .dir("/d", None)
            .file("/d/f", None)
            .read_fails("/d", Errno::PermissionDenied);
        let out = Recorder::new();
        assert_eq!(
            run_args(&["-R", "/d"], &fs, &out),
            Err(GetcapError::Read(Errno::PermissionDenied))
        );
    }
}
