//! The listing engine: inspect each operand, read each directory, and write
//! the sorted, formatted listing to the terminal.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write as _;

use rustos_abi::stdinfo::{Human, Severity, StdInfoKind, StdInfoRecord};
use rustos_help::{load, render_short, DocumentName, HelpSource, Locale};
use rustos_vt::encode_all;

use crate::command::Command;
use crate::error::LsError;
use crate::io::{Entry, Listing, Metadata, Output};

/// The usage banner a usage error is reported with, and the fallback the
/// short-help switches print when `ls`'s own Help tree is unavailable.
pub const USAGE: &str = "usage: ls [-a] [-l] [--] [path...]";

/// `ls`'s own command word: the short-help switches render its own Help
/// document through the same engine as any other command's.
const OWN_WORD: &str = "ls";

/// Run one [`Command`], inspecting its paths through `fs` and writing the
/// rendered listing to `out`. `locale` is the user's `LANG` preference, if
/// set; `help` is the tool's own `Help/` tree, read by the short-help
/// switches.
///
/// Non-directory operands are listed first (by name), then each directory
/// operand has its entries listed, sorted by name. When more than one operand
/// is given, each directory's listing is preceded by a `path:` header and
/// blocks are separated by a blank line — the POSIX model.
///
/// # Errors
///
/// * [`LsError::Stat`] — an operand (or, under `-l`, a directory entry)
///   could not be inspected; carries the underlying
///   [`Errno`](rustos_abi::Errno) (e.g. [`Errno::NotFound`]).
/// * [`LsError::Read`] — a directory could not be read.
/// * [`LsError::Output`] — writing the terminal failed.
///
/// [`Errno::NotFound`]: rustos_abi::Errno::NotFound
pub fn run(
    command: Command,
    locale: Option<&str>,
    fs: &dyn Listing,
    help: &dyn HelpSource,
    out: &dyn Output,
) -> Result<(), LsError> {
    match command {
        Command::Help => short_help(locale, help, out),
        Command::List { all, long, paths } => list(all, long, &paths, fs, out),
    }
}

/// Render `ls`'s own short help (`NAME` + `SYNOPSIS` + compact `OPTIONS`)
/// from its own Help tree; when that tree is absent (a build without the
/// bundle's documents) the usage banner stands in, so `-h` never fails.
fn short_help(
    locale: Option<&str>,
    help: &dyn HelpSource,
    out: &dyn Output,
) -> Result<(), LsError> {
    let loaded = DocumentName::parse(OWN_WORD)
        .ok()
        .and_then(|name| load(help, &active_locale(locale), &name).ok());
    let bytes = match &loaded {
        Some(loaded) => encode_all(&render_short(&loaded.doc)),
        // The tool's own page being missing must not make `-h` fail: the
        // usage banner is the tool's own text, not fabricated help content.
        None => format!("{USAGE}\n").into_bytes(),
    };
    out.write_all(&bytes).map_err(LsError::Output)
}

/// The locale the engine is asked for: the user's preference when it is a
/// well-formed tag, the canonical `default/` otherwise. A malformed
/// preference degrades to the canonical documents rather than making the
/// short help unreadable — the fallback chain itself stays the engine's.
fn active_locale(tag: Option<&str>) -> Locale {
    tag.and_then(|tag| Locale::parse(tag).ok())
        .unwrap_or_default()
}

/// Inspect every operand, then render the file block followed by each
/// directory block into one buffer and write it once. Hidden entries
/// filtered from the listing are counted and noted afterwards on the
/// advisory stream — never in the listing itself.
fn list(
    all: bool,
    long: bool,
    paths: &[String],
    fs: &dyn Listing,
    out: &dyn Output,
) -> Result<(), LsError> {
    let mut files: Vec<(String, Metadata)> = Vec::new();
    let mut dirs: Vec<&String> = Vec::new();
    for path in paths {
        let meta = fs.stat(path).map_err(LsError::Stat)?;
        if meta.kind.is_dir() {
            dirs.push(path);
        } else {
            files.push((path.clone(), meta));
        }
    }

    let multi = paths.len() > 1;
    let mut buf = String::new();
    let mut first = true;
    let mut hidden_omitted: u64 = 0;

    if !files.is_empty() {
        files.sort_by(|a, b| a.0.cmp(&b.0));
        open_block(&mut buf, &mut first, None);
        render_rows(&mut buf, &files, long);
    }
    for path in dirs {
        let mut entries = fs.read_dir(path).map_err(LsError::Read)?;
        if !all {
            let before = entries.len();
            entries.retain(|entry| !entry.name.starts_with('.'));
            hidden_omitted += (before - entries.len()) as u64;
        }
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        let rows = rows_for(path, &entries, long, fs)?;
        open_block(&mut buf, &mut first, multi.then_some(path.as_str()));
        render_rows(&mut buf, &rows, long);
    }

    out.write_all(buf.as_bytes()).map_err(LsError::Output)?;
    if hidden_omitted > 0 {
        emit_omission_record(out, hidden_omitted);
    }
    Ok(())
}

/// The rendered rows of one directory block: each entry's name, with its
/// metadata attached. The per-entry `stat` behind the long format's mode
/// and size columns is paid only when `-l` asks for them; the short format
/// renders names straight off the one `read_dir`.
fn rows_for(
    dir: &str,
    entries: &[Entry],
    long: bool,
    fs: &dyn Listing,
) -> Result<Vec<(String, Metadata)>, LsError> {
    let mut rows = Vec::with_capacity(entries.len());
    for entry in entries {
        let meta = if long {
            fs.stat(&join(dir, &entry.name)).map_err(LsError::Stat)?
        } else {
            // The short format renders only the name; the kind from the
            // directory stream stands in and the zeroed fields are unread.
            Metadata {
                kind: entry.kind,
                mode: 0,
                size: 0,
            }
        };
        rows.push((entry.name.clone(), meta));
    }
    Ok(rows)
}

/// `name` under the directory `dir`, without doubling a trailing slash.
fn join(dir: &str, name: &str) -> String {
    if dir.ends_with('/') {
        format!("{dir}{name}")
    } else {
        format!("{dir}/{name}")
    }
}

/// Start one rendered block: a blank separator line after a previous block,
/// then the optional `header:` line.
fn open_block(buf: &mut String, first: &mut bool, header: Option<&str>) {
    if !*first {
        buf.push('\n');
    }
    *first = false;
    if let Some(name) = header {
        buf.push_str(name);
        buf.push_str(":\n");
    }
}

/// Render `rows` into `buf`: one name per line, or the long format when
/// `long` is set.
fn render_rows(buf: &mut String, rows: &[(String, Metadata)], long: bool) {
    if long {
        let width = rows
            .iter()
            .map(|(_, meta)| decimal_width(meta.size))
            .max()
            .unwrap_or(1);
        for (name, meta) in rows {
            // Writing into a `String` is infallible, so the `fmt::Result` is
            // discarded deliberately.
            let _ = writeln!(buf, "{} {:>width$} {}", mode_string(*meta), meta.size, name);
        }
    } else {
        for (name, _) in rows {
            buf.push_str(name);
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
    s.push(if meta.kind.is_dir() { 'd' } else { '-' });
    for (bit, ch) in PERMISSIONS {
        s.push(if meta.mode & bit != 0 { ch } else { '-' });
    }
    s
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

/// Emit the `fs.hidden_entries_omitted` advisory (fd 3) when the default
/// dotfile filter hid entries from the listing: a tool or user then knows
/// the listing is not exhaustive and how to see the rest. Advisory only —
/// never affects the listing, the exit status, or ordering.
fn emit_omission_record(out: &dyn Output, omitted: u64) {
    let message = if omitted == 1 {
        String::from("1 hidden file not shown.")
    } else {
        format!("{omitted} hidden files not shown.")
    };
    let ai = format!(
        "{{\"subject\":\"directory_listing\",\
         \"omission\":{{\"reason\":\"hidden_by_default\",\
         \"entry_class\":\"dotfile\",\"omitted_count\":{omitted},\
         \"stdout_is_exhaustive\":false}},\
         \"suggestion\":{{\"argv\":[\"ls\",\"-a\"],\
         \"safe_to_autorun\":false,\"requires_confirmation\":true}}}}"
    );
    let record = StdInfoRecord::new(
        OWN_WORD,
        StdInfoKind::Omission,
        "fs.hidden_entries_omitted",
        Severity::Info,
        Human::with_suggestion(&message, "Use `ls -a` to show them."),
    )
    .with_ai(&ai);
    let mut buf = [0u8; 512];
    if let Ok(len) = record.write_jsonl(&mut buf) {
        out.info(&buf[..len]);
    }
}

#[cfg(test)]
mod tests {
    use super::{run, USAGE};
    use crate::command::Command;
    use crate::error::LsError;
    use crate::io::{Entry, Listing, Metadata, Output};
    use alloc::string::{String, ToString};
    use alloc::vec::Vec;
    use core::cell::RefCell;
    use rustos_abi::fs::FileKind;
    use rustos_abi::Errno;
    use rustos_help::{HelpSource, SourceError};

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
                    kind: FileKind::Regular,
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
                    kind: FileKind::Directory,
                    mode: 0o755,
                    size: 0,
                },
            ));
            self.dirs.push((path.to_string(), Vec::new()));
            self
        }

        /// Declare `name` inside `dir` and give the joined path a stat row,
        /// so the long format's per-entry stat finds it.
        fn entry(mut self, dir: &str, name: &str, kind: FileKind, mode: u32, size: u64) -> Self {
            let children = self
                .dirs
                .iter_mut()
                .find(|(d, _)| d == dir)
                .map(|(_, c)| c)
                .expect("directory must be declared before its entries");
            children.push(Entry {
                name: name.to_string(),
                kind,
            });
            self.stat
                .push((super::join(dir, name), Metadata { kind, mode, size }));
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

        fn read_dir(&self, path: &str) -> Result<Vec<Entry>, Errno> {
            self.dirs
                .iter()
                .find(|(d, _)| d == path)
                .map(|(_, c)| c.clone())
                .ok_or(Errno::NotFound)
        }
    }

    /// A directory whose `read_dir` always fails — to exercise the read
    /// fail-closed path.
    struct FailingDir;

    impl Listing for FailingDir {
        fn stat(&self, _path: &str) -> Result<Metadata, Errno> {
            Ok(Metadata {
                kind: FileKind::Directory,
                mode: 0o755,
                size: 0,
            })
        }

        fn read_dir(&self, _path: &str) -> Result<Vec<Entry>, Errno> {
            Err(Errno::PermissionDenied)
        }
    }

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

    /// A Help tree holding one canonical `ls.md` document.
    struct OneDoc;

    const DOC: &str = "## NAME\n\nls — list directory contents\n\n\
                       ## SYNOPSIS\n\n`ls [-a] [-l] [--] [path...]`\n\n\
                       ## DESCRIPTION\n\nLists things.\n";

    impl HelpSource for OneDoc {
        fn locale_dirs(&self) -> Result<Vec<String>, SourceError> {
            Ok(alloc::vec![String::from("default")])
        }

        fn read(&self, locale_dir: &str, file_name: &str) -> Result<Option<Vec<u8>>, SourceError> {
            if locale_dir == "default" && file_name == "ls.md" {
                Ok(Some(DOC.as_bytes().to_vec()))
            } else {
                Ok(None)
            }
        }
    }

    /// Captures every stdout byte and every fd 3 record; optionally fails on
    /// the first stdout write.
    struct Recorder {
        bytes: RefCell<Vec<u8>>,
        records: RefCell<Vec<Vec<u8>>>,
        fail: bool,
    }

    impl Recorder {
        fn new() -> Self {
            Self {
                bytes: RefCell::new(Vec::new()),
                records: RefCell::new(Vec::new()),
                fail: false,
            }
        }

        fn failing() -> Self {
            Self {
                bytes: RefCell::new(Vec::new()),
                records: RefCell::new(Vec::new()),
                fail: true,
            }
        }

        fn text(&self) -> String {
            String::from_utf8(self.bytes.borrow().clone()).expect("utf8 output")
        }

        fn records(&self) -> Vec<String> {
            self.records
                .borrow()
                .iter()
                .map(|r| String::from_utf8(r.clone()).expect("utf8 record"))
                .collect()
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

        fn info(&self, record: &[u8]) {
            self.records.borrow_mut().push(record.to_vec());
        }
    }

    fn list(all: bool, long: bool, paths: &[&str]) -> Command {
        Command::List {
            all,
            long,
            paths: paths.iter().map(|p| (*p).to_string()).collect::<Vec<_>>(),
        }
    }

    fn run_ls(command: Command, fs: &dyn Listing, out: &Recorder) -> Result<(), LsError> {
        run(command, None, fs, &NoHelp, out)
    }

    #[test]
    fn help_renders_the_short_help_from_the_document() {
        let fs = TreeFs::new();
        let out = Recorder::new();
        assert_eq!(run(Command::Help, None, &fs, &OneDoc, &out), Ok(()));
        let text = out.text();
        assert!(text.contains("ls — list directory contents"), "{text}");
        assert!(text.contains("ls [-a] [-l] [--] [path...]"), "{text}");
    }

    #[test]
    fn help_falls_back_to_the_usage_banner_without_a_tree() {
        let fs = TreeFs::new();
        let out = Recorder::new();
        assert_eq!(run(Command::Help, None, &fs, &NoHelp, &out), Ok(()));
        let mut expected = String::from(USAGE);
        expected.push('\n');
        assert_eq!(out.text(), expected);
    }

    #[test]
    fn directory_entries_are_sorted_by_name() {
        let fs = TreeFs::new()
            .dir(".")
            .entry(".", "b", FileKind::Regular, 0o644, 0)
            .entry(".", "a", FileKind::Regular, 0o644, 0)
            .entry(".", "c", FileKind::Regular, 0o644, 0);
        let out = Recorder::new();
        assert_eq!(run_ls(list(false, false, &["."]), &fs, &out), Ok(()));
        assert_eq!(out.text(), "a\nb\nc\n");
    }

    #[test]
    fn hidden_entries_are_filtered_and_noted_on_the_advisory_stream() {
        let fs = TreeFs::new()
            .dir(".")
            .entry(".", ".hidden", FileKind::Regular, 0o644, 0)
            .entry(".", "visible", FileKind::Regular, 0o644, 0);
        let out = Recorder::new();
        assert_eq!(run_ls(list(false, false, &["."]), &fs, &out), Ok(()));
        assert_eq!(out.text(), "visible\n");
        let records = out.records();
        assert_eq!(records.len(), 1);
        assert!(
            records[0].contains("\"code\":\"fs.hidden_entries_omitted\""),
            "{}",
            records[0]
        );
        assert!(
            records[0].contains("1 hidden file not shown."),
            "{}",
            records[0]
        );
        assert!(records[0].contains("\"omitted_count\":1"), "{}", records[0]);
        assert!(records[0].ends_with('\n'), "framed JSONL record");
    }

    #[test]
    fn all_includes_hidden_entries_and_emits_no_record() {
        let fs = TreeFs::new()
            .dir(".")
            .entry(".", ".hidden", FileKind::Regular, 0o644, 0)
            .entry(".", "visible", FileKind::Regular, 0o644, 0);
        let out = Recorder::new();
        assert_eq!(run_ls(list(true, false, &["."]), &fs, &out), Ok(()));
        assert_eq!(out.text(), ".hidden\nvisible\n");
        assert!(out.records().is_empty());
    }

    #[test]
    fn a_listing_without_hidden_entries_emits_no_record() {
        let fs = TreeFs::new()
            .dir(".")
            .entry(".", "visible", FileKind::Regular, 0o644, 0);
        let out = Recorder::new();
        assert_eq!(run_ls(list(false, false, &["."]), &fs, &out), Ok(()));
        assert!(out.records().is_empty());
    }

    #[test]
    fn hidden_entries_are_counted_across_directories() {
        let fs = TreeFs::new()
            .dir("dir1")
            .entry("dir1", ".one", FileKind::Regular, 0o644, 0)
            .dir("dir2")
            .entry("dir2", ".two", FileKind::Regular, 0o644, 0)
            .entry("dir2", ".three", FileKind::Regular, 0o644, 0);
        let out = Recorder::new();
        assert_eq!(
            run_ls(list(false, false, &["dir1", "dir2"]), &fs, &out),
            Ok(())
        );
        let records = out.records();
        assert_eq!(records.len(), 1);
        assert!(
            records[0].contains("3 hidden files not shown."),
            "{}",
            records[0]
        );
    }

    #[test]
    fn non_directory_operand_prints_its_name() {
        let fs = TreeFs::new().file("a.txt", 0o644, 12);
        let out = Recorder::new();
        assert_eq!(run_ls(list(false, false, &["a.txt"]), &fs, &out), Ok(()));
        assert_eq!(out.text(), "a.txt\n");
    }

    #[test]
    fn long_format_renders_mode_size_and_aligns() {
        let fs = TreeFs::new()
            .dir(".")
            .entry(".", "d", FileKind::Directory, 0o755, 4096)
            .entry(".", "f", FileKind::Regular, 0o644, 7);
        let out = Recorder::new();
        assert_eq!(run_ls(list(false, true, &["."]), &fs, &out), Ok(()));
        assert_eq!(out.text(), "drwxr-xr-x 4096 d\n-rw-r--r--    7 f\n");
    }

    #[test]
    fn long_format_stats_entries_under_a_slash_terminated_operand() {
        // The joined per-entry path must not double the trailing slash.
        let fs = TreeFs::new()
            .dir("dir/")
            .entry("dir/", "f", FileKind::Regular, 0o600, 3);
        let out = Recorder::new();
        assert_eq!(run_ls(list(false, true, &["dir/"]), &fs, &out), Ok(()));
        assert_eq!(out.text(), "-rw------- 3 f\n");
    }

    #[test]
    fn single_directory_operand_has_no_header() {
        let fs = TreeFs::new()
            .dir("dir")
            .entry("dir", "x", FileKind::Regular, 0o644, 0);
        let out = Recorder::new();
        assert_eq!(run_ls(list(false, false, &["dir"]), &fs, &out), Ok(()));
        assert_eq!(out.text(), "x\n");
    }

    #[test]
    fn multiple_operands_list_files_first_then_directories() {
        let fs = TreeFs::new()
            .file("z.txt", 0o644, 0)
            .dir("dir")
            .entry("dir", "y", FileKind::Regular, 0o644, 0)
            .entry("dir", "x", FileKind::Regular, 0o644, 0);
        let out = Recorder::new();
        assert_eq!(
            run_ls(list(false, false, &["z.txt", "dir"]), &fs, &out),
            Ok(())
        );
        assert_eq!(out.text(), "z.txt\n\ndir:\nx\ny\n");
    }

    #[test]
    fn two_directory_operands_each_get_a_header() {
        let fs = TreeFs::new()
            .dir("dir1")
            .entry("dir1", "a", FileKind::Regular, 0o644, 0)
            .dir("dir2")
            .entry("dir2", "b", FileKind::Regular, 0o644, 0);
        let out = Recorder::new();
        assert_eq!(
            run_ls(list(false, false, &["dir1", "dir2"]), &fs, &out),
            Ok(())
        );
        assert_eq!(out.text(), "dir1:\na\n\ndir2:\nb\n");
    }

    #[test]
    fn empty_directory_emits_nothing() {
        let fs = TreeFs::new().dir(".");
        let out = Recorder::new();
        assert_eq!(run_ls(list(false, false, &["."]), &fs, &out), Ok(()));
        assert_eq!(out.text(), "");
    }

    #[test]
    fn missing_operand_fails_closed() {
        let fs = TreeFs::new();
        let out = Recorder::new();
        assert_eq!(
            run_ls(list(false, false, &["absent"]), &fs, &out),
            Err(LsError::Stat(Errno::NotFound))
        );
        assert_eq!(out.text(), "");
    }

    #[test]
    fn a_stat_error_stops_before_listing_anything() {
        // The present directory is never listed because the missing operand
        // aborts first (operands are stat'd in order).
        let fs = TreeFs::new()
            .dir("present")
            .entry("present", "x", FileKind::Regular, 0o644, 0);
        let out = Recorder::new();
        assert_eq!(
            run_ls(list(false, false, &["absent", "present"]), &fs, &out),
            Err(LsError::Stat(Errno::NotFound))
        );
        assert_eq!(out.text(), "");
    }

    #[test]
    fn a_read_dir_error_fails_closed() {
        let out = Recorder::new();
        assert_eq!(
            run_ls(list(false, false, &["dir"]), &FailingDir, &out),
            Err(LsError::Read(Errno::PermissionDenied))
        );
    }

    #[test]
    fn output_failure_propagates() {
        let fs = TreeFs::new()
            .dir(".")
            .entry(".", "x", FileKind::Regular, 0o644, 0);
        let out = Recorder::failing();
        assert_eq!(
            run_ls(list(false, false, &["."]), &fs, &out),
            Err(LsError::Output(Errno::NotFound))
        );
    }
}
