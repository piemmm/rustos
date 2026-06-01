//! The removal engine: inspect each operand, descend each directory `-r`
//! must remove, and unlink every reachable object — depth-first, contents
//! before the directory that holds them.

use alloc::string::String;
use alloc::vec::Vec;

use crate::command::Command;
use crate::error::RmError;
use crate::io::{Entry, EntryKind, Output, Removal};

/// The usage banner printed by [`Command::Help`].
pub const USAGE: &str = "\
usage: rm [-r] [-f] [--] file...

  -r, -R, --recursive  remove directories and their contents
  -f, --force          ignore operands that do not exist; never prompt
  -h, --help           show this message

At least one file operand is required unless -f is given. `--` ends option
parsing: every later argument is a path.
";

/// Run one [`Command`], removing its operands through `fs`.
///
/// Operands are removed in order. A non-directory operand is unlinked; a
/// directory operand is removed only with `-r`, which removes its contents
/// depth-first and then the directory itself. With `-f` an operand that does
/// not exist is skipped silently. `rm` writes nothing on success; `out`
/// carries only the [`Command::Help`] banner.
///
/// The first failure stops the run before any later operand (fail closed,
/// `AGENTS.md` §2.9).
///
/// # Errors
///
/// * [`RmError::IsDirectory`] — a directory was named without `-r`.
/// * [`RmError::Stat`] — an operand could not be inspected; carries the
///   underlying [`Errno`](rustos_abi::Errno) (suppressed for
///   [`NotFound`](rustos_abi::Errno::NotFound) when `-f` is set).
/// * [`RmError::Read`] — a directory's entries could not be read.
/// * [`RmError::Remove`] — unlinking a file or directory failed.
/// * [`RmError::Output`] — writing the usage banner failed.
pub fn run(command: Command, fs: &dyn Removal, out: &dyn Output) -> Result<(), RmError> {
    match command {
        Command::Help => out.write_all(USAGE.as_bytes()).map_err(RmError::Output),
        Command::Remove {
            recursive,
            force,
            paths,
        } => {
            for path in &paths {
                remove_operand(path, recursive, force, fs)?;
            }
            Ok(())
        }
    }
}

/// Remove one named operand, honouring `-f` for a missing path.
fn remove_operand(
    path: &str,
    recursive: bool,
    force: bool,
    fs: &dyn Removal,
) -> Result<(), RmError> {
    let kind = match fs.kind(path) {
        Ok(kind) => kind,
        Err(rustos_abi::Errno::NotFound) if force => return Ok(()),
        Err(errno) => return Err(RmError::Stat(errno)),
    };
    remove_known(path, kind, recursive, fs)
}

/// Remove an object whose [`EntryKind`] is already known (from the parent's
/// directory entry, or from the operand's stat), recursing into directories.
fn remove_known(
    path: &str,
    kind: EntryKind,
    recursive: bool,
    fs: &dyn Removal,
) -> Result<(), RmError> {
    match kind {
        EntryKind::Other => fs.remove_file(path).map_err(RmError::Remove),
        EntryKind::Directory => {
            if !recursive {
                return Err(RmError::IsDirectory);
            }
            for entry in read_children(path, fs)? {
                let child = join(path, &entry.name);
                remove_known(&child, entry.kind, recursive, fs)?;
            }
            fs.remove_dir(path).map_err(RmError::Remove)
        }
    }
}

/// Read every entry of `path` into a vector, so the directory can be walked
/// without depending on entry indices staying stable as removals proceed.
fn read_children(path: &str, fs: &dyn Removal) -> Result<Vec<Entry>, RmError> {
    let mut entries = Vec::new();
    let mut index: u64 = 0;
    while let Some(entry) = fs.read_dir(path, index).map_err(RmError::Read)? {
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
    use crate::command::Command;
    use crate::error::RmError;
    use crate::io::{Entry, EntryKind, Output, Removal};
    use alloc::string::{String, ToString};
    use alloc::vec::Vec;
    use core::cell::RefCell;
    use rustos_abi::Errno;

    /// An in-memory tree: a kind table keyed by path plus, for directories,
    /// the entries that path's `read_dir` returns. Removals are recorded in
    /// call order; an optional `(path, errno)` makes one removal fail.
    struct TreeFs {
        kinds: Vec<(String, EntryKind)>,
        children: Vec<(String, Vec<Entry>)>,
        removed: RefCell<Vec<String>>,
        fail: Option<(String, Errno)>,
    }

    impl TreeFs {
        fn new() -> Self {
            Self {
                kinds: Vec::new(),
                children: Vec::new(),
                removed: RefCell::new(Vec::new()),
                fail: None,
            }
        }

        fn file(mut self, path: &str) -> Self {
            self.kinds.push((path.to_string(), EntryKind::Other));
            self
        }

        fn dir(mut self, path: &str) -> Self {
            self.kinds.push((path.to_string(), EntryKind::Directory));
            self.children.push((path.to_string(), Vec::new()));
            self
        }

        fn child(mut self, dir: &str, name: &str, kind: EntryKind) -> Self {
            let entries = self
                .children
                .iter_mut()
                .find(|(d, _)| d == dir)
                .map(|(_, c)| c)
                .expect("directory must be declared before its entries");
            entries.push(Entry {
                name: name.to_string(),
                kind,
            });
            self
        }

        fn failing(mut self, path: &str, errno: Errno) -> Self {
            self.fail = Some((path.to_string(), errno));
            self
        }

        fn record(&self, path: &str) -> Result<(), Errno> {
            if let Some((target, errno)) = &self.fail {
                if target == path {
                    return Err(*errno);
                }
            }
            self.removed.borrow_mut().push(path.to_string());
            Ok(())
        }

        fn removed(&self) -> Vec<String> {
            self.removed.borrow().clone()
        }
    }

    impl Removal for TreeFs {
        fn kind(&self, path: &str) -> Result<EntryKind, Errno> {
            self.kinds
                .iter()
                .find(|(p, _)| p == path)
                .map(|(_, k)| *k)
                .ok_or(Errno::NotFound)
        }

        fn read_dir(&self, path: &str, index: u64) -> Result<Option<Entry>, Errno> {
            let entries = self
                .children
                .iter()
                .find(|(d, _)| d == path)
                .map(|(_, c)| c)
                .ok_or(Errno::NotFound)?;
            let idx = usize::try_from(index).map_err(|_| Errno::LengthOutOfRange)?;
            Ok(entries.get(idx).cloned())
        }

        fn remove_file(&self, path: &str) -> Result<(), Errno> {
            self.record(path)
        }

        fn remove_dir(&self, path: &str) -> Result<(), Errno> {
            self.record(path)
        }
    }

    /// A directory whose `read_dir` always fails — to exercise the read
    /// fail-closed path.
    struct FailingDir;

    impl Removal for FailingDir {
        fn kind(&self, _path: &str) -> Result<EntryKind, Errno> {
            Ok(EntryKind::Directory)
        }

        fn read_dir(&self, _path: &str, _index: u64) -> Result<Option<Entry>, Errno> {
            Err(Errno::PermissionDenied)
        }

        fn remove_file(&self, _path: &str) -> Result<(), Errno> {
            Ok(())
        }

        fn remove_dir(&self, _path: &str) -> Result<(), Errno> {
            Ok(())
        }
    }

    /// Captures every byte written; optionally fails on the first write.
    struct Recorder {
        bytes: RefCell<Vec<u8>>,
        fail: bool,
    }

    impl Recorder {
        fn new() -> Self {
            Self {
                bytes: RefCell::new(Vec::new()),
                fail: false,
            }
        }

        fn failing() -> Self {
            Self {
                bytes: RefCell::new(Vec::new()),
                fail: true,
            }
        }

        fn text(&self) -> String {
            String::from_utf8(self.bytes.borrow().clone()).expect("utf8 output")
        }
    }

    impl Output for Recorder {
        fn write_all(&self, bytes: &[u8]) -> Result<(), Errno> {
            if self.fail {
                return Err(Errno::NotFound);
            }
            self.bytes.borrow_mut().extend_from_slice(bytes);
            Ok(())
        }
    }

    fn remove(recursive: bool, force: bool, paths: &[&str]) -> Command {
        Command::Remove {
            recursive,
            force,
            paths: paths.iter().map(|p| (*p).to_string()).collect::<Vec<_>>(),
        }
    }

    #[test]
    fn help_writes_usage() {
        let fs = TreeFs::new();
        let out = Recorder::new();
        assert_eq!(run(Command::Help, &fs, &out), Ok(()));
        assert_eq!(out.text(), USAGE);
    }

    #[test]
    fn removes_a_single_file() {
        let fs = TreeFs::new().file("/a.txt");
        let out = Recorder::new();
        assert_eq!(run(remove(false, false, &["/a.txt"]), &fs, &out), Ok(()));
        assert_eq!(fs.removed(), ["/a.txt"]);
        // `rm` is silent on success.
        assert_eq!(out.text(), "");
    }

    #[test]
    fn removes_several_files_in_order() {
        let fs = TreeFs::new().file("/a").file("/b").file("/c");
        let out = Recorder::new();
        assert_eq!(
            run(remove(false, false, &["/a", "/b", "/c"]), &fs, &out),
            Ok(())
        );
        assert_eq!(fs.removed(), ["/a", "/b", "/c"]);
    }

    #[test]
    fn directory_without_recursive_fails_closed() {
        let fs = TreeFs::new().dir("/d");
        let out = Recorder::new();
        assert_eq!(
            run(remove(false, false, &["/d"]), &fs, &out),
            Err(RmError::IsDirectory)
        );
        // Nothing was removed.
        assert!(fs.removed().is_empty());
    }

    #[test]
    fn recursive_removes_contents_before_the_directory() {
        // /d holds a file and a nested directory with its own file.
        let fs = TreeFs::new()
            .dir("/d")
            .child("/d", "f", EntryKind::Other)
            .child("/d", "sub", EntryKind::Directory)
            .dir("/d/sub")
            .child("/d/sub", "g", EntryKind::Other);
        let out = Recorder::new();
        assert_eq!(run(remove(true, false, &["/d"]), &fs, &out), Ok(()));
        // Depth-first: the file, then the nested file, then the nested dir,
        // then the top dir last. Every parent is removed after its contents.
        assert_eq!(fs.removed(), ["/d/f", "/d/sub/g", "/d/sub", "/d"]);
    }

    #[test]
    fn recursive_removes_an_empty_directory() {
        let fs = TreeFs::new().dir("/empty");
        let out = Recorder::new();
        assert_eq!(run(remove(true, false, &["/empty"]), &fs, &out), Ok(()));
        assert_eq!(fs.removed(), ["/empty"]);
    }

    #[test]
    fn missing_operand_fails_closed_without_force() {
        let fs = TreeFs::new();
        let out = Recorder::new();
        assert_eq!(
            run(remove(false, false, &["/absent"]), &fs, &out),
            Err(RmError::Stat(Errno::NotFound))
        );
    }

    #[test]
    fn force_skips_a_missing_operand() {
        let fs = TreeFs::new().file("/present");
        let out = Recorder::new();
        // The missing operand is skipped; the present one is still removed.
        assert_eq!(
            run(remove(false, true, &["/absent", "/present"]), &fs, &out),
            Ok(())
        );
        assert_eq!(fs.removed(), ["/present"]);
    }

    #[test]
    fn force_does_not_mask_a_permission_error() {
        // `-f` suppresses only NotFound; a permission denial still surfaces.
        struct Denied;
        impl Removal for Denied {
            fn kind(&self, _path: &str) -> Result<EntryKind, Errno> {
                Err(Errno::PermissionDenied)
            }
            fn read_dir(&self, _path: &str, _index: u64) -> Result<Option<Entry>, Errno> {
                Ok(None)
            }
            fn remove_file(&self, _path: &str) -> Result<(), Errno> {
                Ok(())
            }
            fn remove_dir(&self, _path: &str) -> Result<(), Errno> {
                Ok(())
            }
        }
        let out = Recorder::new();
        assert_eq!(
            run(remove(false, true, &["/x"]), &Denied, &out),
            Err(RmError::Stat(Errno::PermissionDenied))
        );
    }

    #[test]
    fn a_failure_stops_before_a_later_operand() {
        let fs = TreeFs::new().file("/a").file("/b");
        let out = Recorder::new();
        // The first operand is missing, so the second is never touched.
        assert_eq!(
            run(remove(false, false, &["/absent", "/a"]), &fs, &out),
            Err(RmError::Stat(Errno::NotFound))
        );
        assert!(fs.removed().is_empty());
    }

    #[test]
    fn an_unreadable_directory_fails_closed() {
        let out = Recorder::new();
        assert_eq!(
            run(remove(true, false, &["/d"]), &FailingDir, &out),
            Err(RmError::Read(Errno::PermissionDenied))
        );
    }

    #[test]
    fn a_failed_unlink_surfaces_the_errno() {
        let fs = TreeFs::new()
            .file("/a")
            .failing("/a", Errno::PermissionDenied);
        let out = Recorder::new();
        assert_eq!(
            run(remove(false, false, &["/a"]), &fs, &out),
            Err(RmError::Remove(Errno::PermissionDenied))
        );
    }

    #[test]
    fn a_failed_directory_unlink_stops_the_recursion() {
        // The child unlinks, then removing the directory itself fails.
        let fs = TreeFs::new()
            .dir("/d")
            .child("/d", "f", EntryKind::Other)
            .failing("/d", Errno::PermissionDenied);
        let out = Recorder::new();
        assert_eq!(
            run(remove(true, false, &["/d"]), &fs, &out),
            Err(RmError::Remove(Errno::PermissionDenied))
        );
        // The child was removed before the directory's removal failed.
        assert_eq!(fs.removed(), ["/d/f"]);
    }

    #[test]
    fn empty_force_run_removes_nothing() {
        let fs = TreeFs::new();
        let out = Recorder::new();
        assert_eq!(run(remove(false, true, &[]), &fs, &out), Ok(()));
        assert!(fs.removed().is_empty());
    }

    #[test]
    fn help_output_failure_propagates() {
        let fs = TreeFs::new();
        let out = Recorder::failing();
        assert_eq!(
            run(Command::Help, &fs, &out),
            Err(RmError::Output(Errno::NotFound))
        );
    }

    #[test]
    fn a_directory_under_a_trailing_slash_parent_joins_cleanly() {
        // A mount-root style parent ending in `/` must not double the slash.
        let fs = TreeFs::new().dir("/").child("/", "f", EntryKind::Other);
        let out = Recorder::new();
        assert_eq!(run(remove(true, false, &["/"]), &fs, &out), Ok(()));
        assert_eq!(fs.removed(), ["/f", "/"]);
    }
}
