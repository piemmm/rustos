//! The listing engine: inspect each operand, read each directory, and write
//! the sorted, formatted listing to the terminal.

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write as _;

use crate::command::Command;
use crate::error::LsError;
use crate::io::{Entry, EntryKind, Listing, Metadata, Output};

/// The usage banner printed by [`Command::Help`].
pub const USAGE: &str = "\
usage: ls [-a] [-l] [--] [path...]

  -a, --all    do not hide entries whose name begins with `.`
  -l, --long   long format: type and permission bits, size, then name
  -h, --help   show this message

With no path operand ls lists the current directory. Short options may be
combined (e.g. `-la`). `--` ends option parsing: every later argument is a
path.
";

/// Run one [`Command`], inspecting its paths through `fs` and writing the
/// rendered listing to `out`.
///
/// Non-directory operands are listed first (by name), then each directory
/// operand has its entries listed, sorted by name. When more than one operand
/// is given, each directory's listing is preceded by a `path:` header and
/// blocks are separated by a blank line — the POSIX model.
///
/// # Errors
///
/// * [`LsError::Stat`] — an operand could not be inspected; carries the
///   underlying [`Errno`](rustos_abi::Errno) (e.g. [`Errno::NotFound`]).
/// * [`LsError::Read`] — a directory could not be read.
/// * [`LsError::Output`] — writing the terminal failed.
///
/// [`Errno::NotFound`]: rustos_abi::Errno::NotFound
pub fn run(command: Command, fs: &dyn Listing, out: &dyn Output) -> Result<(), LsError> {
    match command {
        Command::Help => out.write_all(USAGE.as_bytes()).map_err(LsError::Output),
        Command::List { all, long, paths } => list(all, long, &paths, fs, out),
    }
}

/// Inspect every operand, then render the file block followed by each
/// directory block into one buffer and write it once.
fn list(
    all: bool,
    long: bool,
    paths: &[String],
    fs: &dyn Listing,
    out: &dyn Output,
) -> Result<(), LsError> {
    let mut files: Vec<Entry> = Vec::new();
    let mut dirs: Vec<&String> = Vec::new();
    for path in paths {
        let meta = fs.stat(path).map_err(LsError::Stat)?;
        if meta.kind == EntryKind::Directory {
            dirs.push(path);
        } else {
            files.push(Entry {
                name: path.clone(),
                meta,
            });
        }
    }

    let multi = paths.len() > 1;
    let mut buf = String::new();
    let mut first = true;

    if !files.is_empty() {
        files.sort_by(|a, b| a.name.cmp(&b.name));
        append_block(&mut buf, &mut first, None, &files, long);
    }
    for path in dirs {
        let mut entries = read_directory(path, all, fs)?;
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        let header = if multi { Some(path.as_str()) } else { None };
        append_block(&mut buf, &mut first, header, &entries, long);
    }

    out.write_all(buf.as_bytes()).map_err(LsError::Output)
}

/// Read every entry of `path`, dropping dot-prefixed names unless `all`.
fn read_directory(path: &str, all: bool, fs: &dyn Listing) -> Result<Vec<Entry>, LsError> {
    let mut entries = Vec::new();
    let mut index: u64 = 0;
    while let Some(entry) = fs.read_dir(path, index).map_err(LsError::Read)? {
        index = index.saturating_add(1);
        if all || !entry.name.starts_with('.') {
            entries.push(entry);
        }
    }
    Ok(entries)
}

/// Append one rendered block — an optional `header:` line followed by the
/// formatted entries — to `buf`, separating it from a previous block with a
/// blank line.
fn append_block(
    buf: &mut String,
    first: &mut bool,
    header: Option<&str>,
    entries: &[Entry],
    long: bool,
) {
    if !*first {
        buf.push('\n');
    }
    *first = false;
    if let Some(name) = header {
        buf.push_str(name);
        buf.push_str(":\n");
    }
    render_into(buf, entries, long);
}

/// Render `entries` into `buf`: one name per line, or the long format when
/// `long` is set.
fn render_into(buf: &mut String, entries: &[Entry], long: bool) {
    if long {
        let width = entries
            .iter()
            .map(|e| decimal_width(e.meta.size))
            .max()
            .unwrap_or(1);
        for entry in entries {
            // Writing into a `String` is infallible, so the `fmt::Result` is
            // discarded deliberately.
            let _ = writeln!(
                buf,
                "{} {:>width$} {}",
                mode_string(entry.meta),
                entry.meta.size,
                entry.name,
            );
        }
    } else {
        for entry in entries {
            buf.push_str(&entry.name);
            buf.push('\n');
        }
    }
}

/// The ten-character long-format mode string, e.g. `drwxr-xr-x`.
fn mode_string(meta: Metadata) -> String {
    const PERMISSIONS: [(u32, char); 9] = [
        (0o400, 'r'),
        (0o200, 'w'),
        (0o100, 'x'),
        (0o040, 'r'),
        (0o020, 'w'),
        (0o010, 'x'),
        (0o004, 'r'),
        (0o002, 'w'),
        (0o001, 'x'),
    ];
    let mut s = String::with_capacity(10);
    s.push(type_char(meta.kind));
    for (bit, ch) in PERMISSIONS {
        s.push(if meta.mode & bit != 0 { ch } else { '-' });
    }
    s
}

/// The long-format type character for a [`EntryKind`].
fn type_char(kind: EntryKind) -> char {
    match kind {
        EntryKind::Directory => 'd',
        EntryKind::RegularFile => '-',
        EntryKind::Symlink => 'l',
        EntryKind::Other => '?',
    }
}

/// The number of decimal digits in `value` (at least 1, for `0`).
fn decimal_width(value: u64) -> usize {
    let mut width = 1;
    let mut n = value;
    while n >= 10 {
        n /= 10;
        width += 1;
    }
    width
}

#[cfg(test)]
mod tests {
    use super::{run, USAGE};
    use crate::command::Command;
    use crate::error::LsError;
    use crate::io::{Entry, EntryKind, Listing, Metadata, Output};
    use alloc::string::{String, ToString};
    use alloc::vec::Vec;
    use core::cell::RefCell;
    use rustos_abi::Errno;

    /// An in-memory tree: a stat table keyed by path plus, for directories,
    /// the entries that path's `read_dir` returns.
    struct TreeFs {
        stat: Vec<(String, Metadata)>,
        dirs: Vec<(String, Vec<Entry>)>,
    }

    impl TreeFs {
        fn new() -> Self {
            Self {
                stat: Vec::new(),
                dirs: Vec::new(),
            }
        }

        fn file(mut self, path: &str, mode: u32, size: u64) -> Self {
            self.stat.push((
                path.to_string(),
                Metadata {
                    kind: EntryKind::RegularFile,
                    mode,
                    size,
                },
            ));
            self
        }

        fn dir(mut self, path: &str) -> Self {
            self.stat.push((
                path.to_string(),
                Metadata {
                    kind: EntryKind::Directory,
                    mode: 0o755,
                    size: 0,
                },
            ));
            self.dirs.push((path.to_string(), Vec::new()));
            self
        }

        fn entry(mut self, dir: &str, name: &str, kind: EntryKind, mode: u32, size: u64) -> Self {
            let children = self
                .dirs
                .iter_mut()
                .find(|(d, _)| d == dir)
                .map(|(_, c)| c)
                .expect("directory must be declared before its entries");
            children.push(Entry {
                name: name.to_string(),
                meta: Metadata { kind, mode, size },
            });
            self
        }
    }

    impl Listing for TreeFs {
        fn stat(&self, path: &str) -> Result<Metadata, Errno> {
            self.stat
                .iter()
                .find(|(p, _)| p == path)
                .map(|(_, m)| *m)
                .ok_or(Errno::NotFound)
        }

        fn read_dir(&self, path: &str, index: u64) -> Result<Option<Entry>, Errno> {
            let children = self
                .dirs
                .iter()
                .find(|(d, _)| d == path)
                .map(|(_, c)| c)
                .ok_or(Errno::NotFound)?;
            let idx = usize::try_from(index).map_err(|_| Errno::LengthOutOfRange)?;
            Ok(children.get(idx).cloned())
        }
    }

    /// A directory whose `read_dir` always fails — to exercise the read
    /// fail-closed path.
    struct FailingDir;

    impl Listing for FailingDir {
        fn stat(&self, _path: &str) -> Result<Metadata, Errno> {
            Ok(Metadata {
                kind: EntryKind::Directory,
                mode: 0o755,
                size: 0,
            })
        }

        fn read_dir(&self, _path: &str, _index: u64) -> Result<Option<Entry>, Errno> {
            Err(Errno::PermissionDenied)
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

    fn list(all: bool, long: bool, paths: &[&str]) -> Command {
        Command::List {
            all,
            long,
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
    fn directory_entries_are_sorted_by_name() {
        let fs = TreeFs::new()
            .dir(".")
            .entry(".", "b", EntryKind::RegularFile, 0o644, 0)
            .entry(".", "a", EntryKind::RegularFile, 0o644, 0)
            .entry(".", "c", EntryKind::RegularFile, 0o644, 0);
        let out = Recorder::new();
        assert_eq!(run(list(false, false, &["."]), &fs, &out), Ok(()));
        assert_eq!(out.text(), "a\nb\nc\n");
    }

    #[test]
    fn hidden_entries_are_filtered_without_all() {
        let fs = TreeFs::new()
            .dir(".")
            .entry(".", ".hidden", EntryKind::RegularFile, 0o644, 0)
            .entry(".", "visible", EntryKind::RegularFile, 0o644, 0);
        let out = Recorder::new();
        assert_eq!(run(list(false, false, &["."]), &fs, &out), Ok(()));
        assert_eq!(out.text(), "visible\n");
    }

    #[test]
    fn all_includes_hidden_entries() {
        let fs = TreeFs::new()
            .dir(".")
            .entry(".", ".hidden", EntryKind::RegularFile, 0o644, 0)
            .entry(".", "visible", EntryKind::RegularFile, 0o644, 0);
        let out = Recorder::new();
        assert_eq!(run(list(true, false, &["."]), &fs, &out), Ok(()));
        assert_eq!(out.text(), ".hidden\nvisible\n");
    }

    #[test]
    fn non_directory_operand_prints_its_name() {
        let fs = TreeFs::new().file("a.txt", 0o644, 12);
        let out = Recorder::new();
        assert_eq!(run(list(false, false, &["a.txt"]), &fs, &out), Ok(()));
        assert_eq!(out.text(), "a.txt\n");
    }

    #[test]
    fn long_format_renders_mode_size_and_aligns() {
        let fs = TreeFs::new()
            .dir(".")
            .entry(".", "d", EntryKind::Directory, 0o755, 4096)
            .entry(".", "f", EntryKind::RegularFile, 0o644, 7);
        let out = Recorder::new();
        assert_eq!(run(list(false, true, &["."]), &fs, &out), Ok(()));
        assert_eq!(out.text(), "drwxr-xr-x 4096 d\n-rw-r--r--    7 f\n");
    }

    #[test]
    fn long_format_type_chars_cover_symlink_and_other() {
        let fs = TreeFs::new()
            .dir(".")
            .entry(".", "link", EntryKind::Symlink, 0o777, 0)
            .entry(".", "sock", EntryKind::Other, 0o600, 0);
        let out = Recorder::new();
        assert_eq!(run(list(false, true, &["."]), &fs, &out), Ok(()));
        assert_eq!(out.text(), "lrwxrwxrwx 0 link\n?rw------- 0 sock\n");
    }

    #[test]
    fn single_directory_operand_has_no_header() {
        let fs = TreeFs::new()
            .dir("dir")
            .entry("dir", "x", EntryKind::RegularFile, 0o644, 0);
        let out = Recorder::new();
        assert_eq!(run(list(false, false, &["dir"]), &fs, &out), Ok(()));
        assert_eq!(out.text(), "x\n");
    }

    #[test]
    fn multiple_operands_list_files_first_then_directories() {
        let fs = TreeFs::new()
            .file("z.txt", 0o644, 0)
            .dir("dir")
            .entry("dir", "y", EntryKind::RegularFile, 0o644, 0)
            .entry("dir", "x", EntryKind::RegularFile, 0o644, 0);
        let out = Recorder::new();
        assert_eq!(
            run(list(false, false, &["z.txt", "dir"]), &fs, &out),
            Ok(())
        );
        assert_eq!(out.text(), "z.txt\n\ndir:\nx\ny\n");
    }

    #[test]
    fn two_directory_operands_each_get_a_header() {
        let fs = TreeFs::new()
            .dir("dir1")
            .entry("dir1", "a", EntryKind::RegularFile, 0o644, 0)
            .dir("dir2")
            .entry("dir2", "b", EntryKind::RegularFile, 0o644, 0);
        let out = Recorder::new();
        assert_eq!(
            run(list(false, false, &["dir1", "dir2"]), &fs, &out),
            Ok(())
        );
        assert_eq!(out.text(), "dir1:\na\n\ndir2:\nb\n");
    }

    #[test]
    fn empty_directory_emits_nothing() {
        let fs = TreeFs::new().dir(".");
        let out = Recorder::new();
        assert_eq!(run(list(false, false, &["."]), &fs, &out), Ok(()));
        assert_eq!(out.text(), "");
    }

    #[test]
    fn missing_operand_fails_closed() {
        let fs = TreeFs::new();
        let out = Recorder::new();
        assert_eq!(
            run(list(false, false, &["absent"]), &fs, &out),
            Err(LsError::Stat(Errno::NotFound))
        );
        assert_eq!(out.text(), "");
    }

    #[test]
    fn a_stat_error_stops_before_listing_anything() {
        // The present directory is never listed because the missing operand
        // aborts first (operands are stat'd in order).
        let fs =
            TreeFs::new()
                .dir("present")
                .entry("present", "x", EntryKind::RegularFile, 0o644, 0);
        let out = Recorder::new();
        assert_eq!(
            run(list(false, false, &["absent", "present"]), &fs, &out),
            Err(LsError::Stat(Errno::NotFound))
        );
        assert_eq!(out.text(), "");
    }

    #[test]
    fn a_read_dir_error_fails_closed() {
        let out = Recorder::new();
        assert_eq!(
            run(list(false, false, &["dir"]), &FailingDir, &out),
            Err(LsError::Read(Errno::PermissionDenied))
        );
    }

    #[test]
    fn output_failure_propagates() {
        let fs = TreeFs::new()
            .dir(".")
            .entry(".", "x", EntryKind::RegularFile, 0o644, 0);
        let out = Recorder::failing();
        assert_eq!(
            run(list(false, false, &["."]), &fs, &out),
            Err(LsError::Output(Errno::NotFound))
        );
    }
}
