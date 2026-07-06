//! The orchestration engine: assemble the inputs, count each, and print
//! the aligned rows and the total.

use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use rustos_abi::Errno;
use rustos_help::{own_short_help, HelpSource};

use crate::command::{Command, Job, Operand, Selection, TotalMode};
use crate::counter::{Counter, Counts};
use crate::error::WcError;
use crate::io::{FileSource, Input, Output, SizeProbe};

/// The usage banner a usage error is reported with, and the fallback the
/// short-help switches print when `wc`'s own Help tree is unavailable.
pub const USAGE: &str = "usage: wc [-clmwL] [--total <when>] [--files0-from <file>] [--] [file...]";

/// `wc`'s own command word: the short-help switches render its own Help
/// document through the same engine as any other command's.
const OWN_WORD: &str = "wc";

/// Bytes pulled from a source per read call. A fixed chunk bounds the
/// per-call buffer so a source of any size streams through a constant
/// amount of memory.
const READ_CHUNK: usize = 4096;

/// The column minimum a non-regular input (standard input, a directory)
/// forces, exactly as in GNU `wc`.
const WIDE_MINIMUM: usize = 7;

/// One assembled input of the run.
enum Entry {
    /// The unlabelled default standard input (no operands were given).
    Stdin,
    /// A labelled input: `-` is standard input; anything else a path.
    Named(String),
    /// A `--files0-from` record that names nothing usable.
    Bad(BadRecord),
}

/// Why a `--files0-from` record is unusable.
enum BadRecord {
    /// A zero-length record.
    Empty,
    /// A record that is not UTF-8 text; RustOS path spellings are UTF-8,
    /// so such a record can name nothing (this case cannot arise from the
    /// UTF-8 argument vector).
    NotText,
}

/// Run one [`Command`], reading its inputs through `files`/`stdin` and
/// writing the count rows to `out`. `locale` is the user's `LANG`
/// preference, if set; `help` is the tool's own `Help/` tree, read by the
/// short-help switches; `err` receives the per-input diagnostics.
///
/// Returns `Ok(true)` when every input was counted, `Ok(false)` when at
/// least one input failed (it was diagnosed on `err` and the run moved to
/// the next input, as in the GNU tool).
///
/// # Errors
///
/// [`WcError::Output`] — writing `out` or `err` failed. Nothing else is
/// fatal.
pub fn run(
    command: Command,
    locale: Option<&str>,
    files: &dyn FileSource,
    stdin: &dyn Input,
    help: &dyn HelpSource,
    out: &dyn Output,
    err: &dyn Output,
) -> Result<bool, WcError> {
    let job = match command {
        Command::Help => {
            let bytes = own_short_help(help, locale, OWN_WORD)
                .unwrap_or_else(|| format!("{USAGE}\n").into_bytes());
            out.write_all(&bytes).map_err(WcError::Output)?;
            return Ok(true);
        }
        Command::Count(job) => job,
    };

    let Some(entries) = assemble(&job, files, stdin, err)? else {
        // The operand list itself could not be read: diagnosed, counted
        // nothing.
        return Ok(false);
    };

    let width = number_width(&job, &entries, files);
    let list_is_stdin = job.files0_from.as_deref() == Some("-");
    let show_rows = job.total != TotalMode::Only;
    let mut totals = Counts::default();
    let mut all_ok = true;

    for (position, entry) in entries.iter().enumerate() {
        let (label, outcome) = match entry {
            Entry::Stdin => (None, count_stream(&mut |buf| stdin.read(buf))),
            Entry::Named(name) if name == "-" => {
                if list_is_stdin {
                    // The names are already streaming in on stdin.
                    diagnose(
                        err,
                        "when reading file names from standard input, no file name of '-' allowed",
                    )?;
                    all_ok = false;
                    continue;
                }
                (
                    Some(name.as_str()),
                    count_stream(&mut |buf| stdin.read(buf)),
                )
            }
            Entry::Named(name) if name.is_empty() => {
                diagnose(
                    err,
                    &bad_record_message(&job, position, "invalid zero-length file name"),
                )?;
                all_ok = false;
                continue;
            }
            Entry::Named(name) => match count_file(files, name) {
                FileCount::OpenFailed(errno) => {
                    diagnose(err, &format!("{name}: {errno}"))?;
                    all_ok = false;
                    continue;
                }
                FileCount::Counted(counts, read_error) => {
                    (Some(name.as_str()), (counts, read_error))
                }
            },
            Entry::Bad(kind) => {
                let what = match kind {
                    BadRecord::Empty => "invalid zero-length file name",
                    BadRecord::NotText => "invalid file name",
                };
                diagnose(err, &bad_record_message(&job, position, what))?;
                all_ok = false;
                continue;
            }
        };

        let (counts, read_error) = outcome;
        if show_rows {
            out.write_all(format_row(&counts, job.select, width, label).as_bytes())
                .map_err(WcError::Output)?;
        }
        totals.add(&counts);
        if let Some(errno) = read_error {
            diagnose(
                err,
                &format!("{}: {errno}", label.unwrap_or("standard input")),
            )?;
            all_ok = false;
        }
    }

    let print_total = match job.total {
        TotalMode::Never => false,
        TotalMode::Always | TotalMode::Only => true,
        TotalMode::Auto => entries.len() > 1,
    };
    if print_total {
        let label = (job.total != TotalMode::Only).then_some("total");
        out.write_all(format_row(&totals, job.select, width, label).as_bytes())
            .map_err(WcError::Output)?;
    }
    Ok(all_ok)
}

/// Assemble the run's inputs: the `--files0-from` records, the argv
/// operands, or the unlabelled standard-input default. `Ok(None)` means
/// the operand list itself could not be read (already diagnosed); `Err`
/// is a fatal output failure.
fn assemble(
    job: &Job,
    files: &dyn FileSource,
    stdin: &dyn Input,
    err: &dyn Output,
) -> Result<Option<Vec<Entry>>, WcError> {
    if let Some(list) = &job.files0_from {
        let raw = match read_list(list, files, stdin) {
            Ok(raw) => raw,
            Err(message) => {
                diagnose(err, &message)?;
                return Ok(None);
            }
        };
        return Ok(Some(records(&raw)));
    }
    if job.operands.is_empty() {
        return Ok(Some(vec![Entry::Stdin]));
    }
    Ok(Some(
        job.operands
            .iter()
            .map(|operand| match operand {
                Operand::Dash => Entry::Named(String::from("-")),
                Operand::Path(path) => Entry::Named(path.clone()),
            })
            .collect(),
    ))
}

/// Read the whole `--files0-from` list: a named file through the
/// filesystem seam, or standard input for `-`. The error is the finished
/// diagnostic message.
fn read_list(list: &str, files: &dyn FileSource, stdin: &dyn Input) -> Result<Vec<u8>, String> {
    let mut data = Vec::new();
    let mut buf = vec![0u8; READ_CHUNK];
    if list == "-" {
        loop {
            match stdin.read(&mut buf) {
                Ok(0) => return Ok(data),
                Ok(n) => data.extend_from_slice(&buf[..n]),
                Err(errno) => return Err(format!("-: read error: {errno}")),
            }
        }
    }
    let mut offset: u64 = 0;
    loop {
        match files.read(list, offset, &mut buf) {
            Ok(0) => return Ok(data),
            Ok(n) => {
                data.extend_from_slice(&buf[..n]);
                offset += n as u64;
            }
            Err(errno) if offset == 0 => {
                return Err(format!("cannot open '{list}' for reading: {errno}"))
            }
            Err(errno) => return Err(format!("{list}: read error: {errno}")),
        }
    }
}

/// Split a `--files0-from` byte list into entries: records are
/// NUL-separated, a trailing NUL closes the last record rather than
/// opening an empty one, and an unusable record is kept as [`Entry::Bad`]
/// so it is diagnosed with its record number.
fn records(raw: &[u8]) -> Vec<Entry> {
    if raw.is_empty() {
        return Vec::new();
    }
    let body = raw.strip_suffix(&[0]).unwrap_or(raw);
    body.split(|&byte| byte == 0)
        .map(|record| {
            if record.is_empty() {
                Entry::Bad(BadRecord::Empty)
            } else {
                match core::str::from_utf8(record) {
                    Ok(name) => Entry::Named(String::from(name)),
                    Err(_) => Entry::Bad(BadRecord::NotText),
                }
            }
        })
        .collect()
}

/// The diagnostic for an unusable operand: plain for an argv operand,
/// `list:record:` prefixed for a `--files0-from` record, as in GNU `wc`.
fn bad_record_message(job: &Job, position: usize, what: &str) -> String {
    match &job.files0_from {
        Some(list) => format!("{list}:{}: {what}", position + 1),
        None => String::from(what),
    }
}

/// The outcome of counting one named file.
enum FileCount {
    /// The very first read failed: the file could not be opened. No row is
    /// printed.
    OpenFailed(Errno),
    /// The file was counted; a mid-stream read error (if any) is reported
    /// after its partial row, exactly as in the GNU tool.
    Counted(Counts, Option<Errno>),
}

/// Count one named file through the filesystem seam.
fn count_file(files: &dyn FileSource, path: &str) -> FileCount {
    let mut counter = Counter::new();
    let mut buf = vec![0u8; READ_CHUNK];
    let mut offset: u64 = 0;
    loop {
        match files.read(path, offset, &mut buf) {
            Ok(0) => return FileCount::Counted(counter.finish(), None),
            Ok(n) => {
                counter.feed(&buf[..n]);
                offset += n as u64;
            }
            Err(errno) if offset == 0 => return FileCount::OpenFailed(errno),
            Err(errno) => return FileCount::Counted(counter.finish(), Some(errno)),
        }
    }
}

/// Count one byte stream (standard input) to exhaustion; a read error
/// ends the stream with the counts gathered so far.
fn count_stream(
    read: &mut dyn FnMut(&mut [u8]) -> Result<usize, Errno>,
) -> (Counts, Option<Errno>) {
    let mut counter = Counter::new();
    let mut buf = vec![0u8; READ_CHUNK];
    loop {
        match read(&mut buf) {
            Ok(0) => return (counter.finish(), None),
            Ok(n) => counter.feed(&buf[..n]),
            Err(errno) => return (counter.finish(), Some(errno)),
        }
    }
}

/// One output row: the selected counts in the fixed GNU order (lines,
/// words, chars, bytes, max line width), each right-aligned to `width`,
/// space-separated, then the label.
fn format_row(counts: &Counts, select: Selection, width: usize, name: Option<&str>) -> String {
    let mut row = String::new();
    let values = [
        (select.lines, counts.lines),
        (select.words, counts.words),
        (select.chars, counts.chars),
        (select.bytes, counts.bytes),
        (select.max_line, counts.max_line),
    ];
    for (selected, value) in values {
        if selected {
            if !row.is_empty() {
                row.push(' ');
            }
            let _ = core::fmt::Write::write_fmt(&mut row, format_args!("{value:>width$}"));
        }
    }
    if let Some(name) = name {
        row.push(' ');
        row.push_str(name);
    }
    row.push('\n');
    row
}

/// The GNU column width: `1` for `--total=only`, for the `--files0-from`
/// form, and for a single input printing a single count; otherwise the
/// decimal width of the summed regular-file operand sizes, with any
/// standard-input or non-regular operand forcing the 7-column minimum
/// (an unprobeable operand contributes nothing).
fn number_width(job: &Job, entries: &[Entry], files: &dyn FileSource) -> usize {
    if job.total == TotalMode::Only || job.files0_from.is_some() {
        return 1;
    }
    if entries.len() == 1 && job.select.selected() == 1 {
        return 1;
    }
    let mut minimum = 1usize;
    let mut sum: u128 = 0;
    for entry in entries {
        match entry {
            Entry::Stdin => minimum = WIDE_MINIMUM,
            Entry::Named(name) if name == "-" => minimum = WIDE_MINIMUM,
            Entry::Named(name) => match files.size(name) {
                SizeProbe::Regular(bytes) => sum += u128::from(bytes),
                SizeProbe::NotRegular => minimum = WIDE_MINIMUM,
                SizeProbe::Unavailable => {}
            },
            Entry::Bad(_) => {}
        }
    }
    digits(sum).max(minimum)
}

/// The decimal digit count of `value` (`1` for zero).
fn digits(mut value: u128) -> usize {
    let mut width = 1;
    while value >= 10 {
        value /= 10;
        width += 1;
    }
    width
}

/// One `wc: …` diagnostic line on the error stream. A failure to report
/// is itself fatal — the tool never continues silently.
fn diagnose(err: &dyn Output, message: &str) -> Result<(), WcError> {
    err.write_all(format!("wc: {message}\n").as_bytes())
        .map_err(WcError::Output)
}

#[cfg(test)]
mod tests {
    extern crate std;

    use alloc::collections::BTreeMap;
    use alloc::string::String;
    use alloc::vec::Vec;
    use core::cell::RefCell;

    use rustos_abi::Errno;
    use rustos_help::{HelpSource, SourceError};

    use super::{run, USAGE};
    use crate::command::{parse, Command};
    use crate::error::WcError;
    use crate::io::{FileSource, Input, Output, SizeProbe};

    /// One fixture filesystem node.
    enum Node {
        File(Vec<u8>),
        Directory,
    }

    /// An in-memory filesystem serving reads in bounded slices so a
    /// counted file crosses several read calls.
    struct MemFiles {
        nodes: BTreeMap<String, Node>,
        /// Largest slice one read call returns.
        chunk: usize,
        /// Fail reads of this path at or beyond this offset, to model a
        /// mid-stream I/O error.
        fail_at: Option<(String, u64)>,
    }

    impl MemFiles {
        fn new(entries: &[(&str, &[u8])]) -> Self {
            Self {
                nodes: entries
                    .iter()
                    .map(|(name, data)| (String::from(*name), Node::File(data.to_vec())))
                    .collect(),
                chunk: usize::MAX,
                fail_at: None,
            }
        }

        fn with_directory(mut self, name: &str) -> Self {
            self.nodes.insert(String::from(name), Node::Directory);
            self
        }
    }

    impl FileSource for MemFiles {
        fn read(&self, path: &str, offset: u64, buf: &mut [u8]) -> Result<usize, Errno> {
            if let Some((name, at)) = &self.fail_at {
                if name == path && offset >= *at {
                    return Err(Errno::NotImplemented);
                }
            }
            let data = match self.nodes.get(path) {
                Some(Node::File(data)) => data,
                Some(Node::Directory) => return Err(Errno::OutOfRange),
                None => return Err(Errno::NotFound),
            };
            let offset = usize::try_from(offset).unwrap_or(usize::MAX);
            if offset >= data.len() {
                return Ok(0);
            }
            let take = buf.len().min(self.chunk).min(data.len() - offset);
            buf[..take].copy_from_slice(&data[offset..offset + take]);
            Ok(take)
        }

        fn size(&self, path: &str) -> SizeProbe {
            match self.nodes.get(path) {
                Some(Node::File(data)) => SizeProbe::Regular(data.len() as u64),
                Some(Node::Directory) => SizeProbe::NotRegular,
                None => SizeProbe::Unavailable,
            }
        }
    }

    /// In-memory standard input.
    struct MemStdin {
        data: RefCell<Vec<u8>>,
    }

    impl MemStdin {
        fn new(data: &[u8]) -> Self {
            Self {
                data: RefCell::new(data.to_vec()),
            }
        }
    }

    impl Input for MemStdin {
        fn read(&self, buf: &mut [u8]) -> Result<usize, Errno> {
            let mut data = self.data.borrow_mut();
            let take = buf.len().min(data.len());
            buf[..take].copy_from_slice(&data[..take]);
            data.drain(..take);
            Ok(take)
        }
    }

    /// A capturing output stream.
    #[derive(Default)]
    struct Sink(RefCell<Vec<u8>>);

    impl Sink {
        fn text(&self) -> String {
            String::from_utf8(self.0.borrow().clone()).expect("fixture output is UTF-8")
        }
    }

    impl Output for Sink {
        fn write_all(&self, bytes: &[u8]) -> Result<(), Errno> {
            self.0.borrow_mut().extend_from_slice(bytes);
            Ok(())
        }
    }

    /// An output stream whose every write fails.
    struct BrokenSink;

    impl Output for BrokenSink {
        fn write_all(&self, _bytes: &[u8]) -> Result<(), Errno> {
            Err(Errno::NotImplemented)
        }
    }

    /// A bundle with no help documents, so the short help falls back to
    /// the usage banner.
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

    fn command(args: &[&str]) -> Command {
        parse(args).expect("fixture command line parses")
    }

    /// Run `args` against the fixtures, returning (all-ok, stdout, stderr).
    fn run_case(args: &[&str], files: &MemFiles, stdin: &MemStdin) -> (bool, String, String) {
        let out = Sink::default();
        let err = Sink::default();
        let ok = run(command(args), None, files, stdin, &NoHelp, &out, &err)
            .expect("fixture output streams never fail");
        (ok, out.text(), err.text())
    }

    #[test]
    fn default_stdin_row_is_unlabelled_and_seven_wide() {
        // Standard input is not a regular file, so the three-count default
        // pads to the GNU seven-column minimum and carries no name.
        let files = MemFiles::new(&[]);
        let stdin = MemStdin::new(b"hello world\n");
        let (ok, out, err) = run_case(&[], &files, &stdin);
        assert!(ok);
        assert_eq!(out, "      1       2      12\n");
        assert!(err.is_empty());
    }

    #[test]
    fn a_single_count_of_a_single_input_is_unpadded() {
        let files = MemFiles::new(&[("f", b"a\nb\nc\n")]);
        let stdin = MemStdin::new(b"x\n");
        let (_, out, _) = run_case(&["-l"], &files, &MemStdin::new(b"x\n"));
        assert_eq!(out, "1\n");
        let (_, out, _) = run_case(&["-l", "f"], &files, &stdin);
        assert_eq!(out, "3 f\n");
    }

    #[test]
    fn multi_file_width_comes_from_the_summed_sizes() {
        // 8 + 6 = 14 bytes: two-digit columns, plus the auto total row.
        let files = MemFiles::new(&[("a", b"one\ntwo\n"), ("b", b"three\n")]);
        let stdin = MemStdin::new(b"");
        let (ok, out, _) = run_case(&["-l", "a", "b"], &files, &stdin);
        assert!(ok);
        assert_eq!(out, " 2 a\n 1 b\n 3 total\n");
    }

    #[test]
    fn a_dash_operand_forces_the_wide_minimum() {
        let files = MemFiles::new(&[("a", b"x\n")]);
        let stdin = MemStdin::new(b"y\nz\n");
        let (ok, out, _) = run_case(&["-l", "a", "-"], &files, &stdin);
        assert!(ok);
        assert_eq!(out, "      1 a\n      2 -\n      3 total\n");
    }

    #[test]
    fn the_full_default_columns_order_lines_words_bytes() {
        let files = MemFiles::new(&[("a", b"one two\nthree\n"), ("b", b"4\n")]);
        let stdin = MemStdin::new(b"");
        let (_, out, _) = run_case(&["a", "b"], &files, &stdin);
        // Sizes sum to 16: two-digit columns.
        assert_eq!(out, " 2  3 14 a\n 1  1  2 b\n 3  4 16 total\n");
    }

    #[test]
    fn chars_and_max_line_width_columns() {
        let files = MemFiles::new(&[("f", "héllo\n日本\n".as_bytes())]);
        let stdin = MemStdin::new(b"");
        let (_, out, _) = run_case(&["-m", "f"], &files, &stdin);
        assert_eq!(out, "9 f\n");
        let (_, out, _) = run_case(&["-L", "f"], &files, &stdin);
        assert_eq!(out, "5 f\n");
    }

    #[test]
    fn a_missing_file_is_diagnosed_and_the_run_continues() {
        let files = MemFiles::new(&[("b", b"B\n")]);
        let stdin = MemStdin::new(b"");
        let (ok, out, err) = run_case(&["-l", "missing", "b"], &files, &stdin);
        assert!(!ok);
        // The missing operand contributes nothing to the width (2 bytes).
        assert_eq!(out, "1 b\n1 total\n");
        assert!(
            err.starts_with("wc: missing:"),
            "unexpected diagnostic: {err}"
        );
    }

    #[test]
    fn a_directory_operand_widens_columns_and_is_diagnosed() {
        let files = MemFiles::new(&[("a", b"x\n")]).with_directory("d");
        let stdin = MemStdin::new(b"");
        let (ok, out, err) = run_case(&["-l", "d", "a"], &files, &stdin);
        assert!(!ok);
        assert_eq!(out, "      1 a\n      1 total\n");
        assert!(err.starts_with("wc: d:"), "unexpected diagnostic: {err}");
    }

    #[test]
    fn a_mid_stream_read_error_keeps_the_partial_row() {
        let mut files = MemFiles::new(&[("f", b"a\nb\nc\n"), ("g", b"x\n")]);
        files.chunk = 2;
        files.fail_at = Some((String::from("f"), 4));
        let stdin = MemStdin::new(b"");
        let (ok, out, err) = run_case(&["-l", "f", "g"], &files, &stdin);
        assert!(!ok);
        // The partial counts row still prints and still joins the total,
        // exactly as in the GNU tool.
        assert_eq!(out, "2 f\n1 g\n3 total\n");
        assert!(err.starts_with("wc: f:"), "unexpected diagnostic: {err}");
    }

    #[test]
    fn an_empty_argv_operand_is_diagnosed() {
        let files = MemFiles::new(&[]);
        let stdin = MemStdin::new(b"");
        let (ok, out, err) = run_case(&["-l", ""], &files, &stdin);
        assert!(!ok);
        assert_eq!(out, "");
        assert_eq!(err, "wc: invalid zero-length file name\n");
    }

    #[test]
    fn total_only_prints_one_unlabelled_unpadded_row() {
        let files = MemFiles::new(&[("a", b"one\ntwo\n"), ("b", b"three\n")]);
        let stdin = MemStdin::new(b"");
        let (ok, out, _) = run_case(&["-l", "--total=only", "a", "b"], &files, &stdin);
        assert!(ok);
        assert_eq!(out, "3\n");
    }

    #[test]
    fn total_never_and_always() {
        let files = MemFiles::new(&[("a", b"x\n"), ("b", b"y\n")]);
        let stdin = MemStdin::new(b"");
        let (_, out, _) = run_case(&["-l", "--total=never", "a", "b"], &files, &stdin);
        assert_eq!(out, "1 a\n1 b\n");
        let (_, out, _) = run_case(&["-l", "--total=always", "a"], &files, &stdin);
        assert_eq!(out, "1 a\n1 total\n");
    }

    #[test]
    fn files0_from_reads_nul_separated_names_unpadded() {
        let files = MemFiles::new(&[("a", b"one\ntwo\n"), ("b", b"three\n"), ("list", b"a\0b\0")]);
        let stdin = MemStdin::new(b"");
        let (ok, out, err) = run_case(&["-l", "--files0-from=list"], &files, &stdin);
        assert!(ok, "unexpected diagnostic: {err}");
        assert_eq!(out, "2 a\n1 b\n3 total\n");
    }

    #[test]
    fn files0_from_stdin_reads_the_names_from_standard_input() {
        let files = MemFiles::new(&[("a", b"one\ntwo\n")]);
        let stdin = MemStdin::new(b"a\0");
        let (ok, out, _) = run_case(&["-l", "--files0-from=-"], &files, &stdin);
        assert!(ok);
        assert_eq!(out, "2 a\n");
    }

    #[test]
    fn files0_records_are_validated_per_record() {
        // Record 2 is empty; record 3 is fine.
        let files = MemFiles::new(&[("a", b"x\n"), ("b", b"y\n"), ("list", b"a\0\0b\0")]);
        let stdin = MemStdin::new(b"");
        let (ok, out, err) = run_case(&["-l", "--files0-from=list"], &files, &stdin);
        assert!(!ok);
        assert_eq!(out, "1 a\n1 b\n2 total\n");
        assert_eq!(err, "wc: list:2: invalid zero-length file name\n");
    }

    #[test]
    fn files0_from_stdin_refuses_a_dash_record() {
        let files = MemFiles::new(&[("a", b"x\n")]);
        let stdin = MemStdin::new(b"a\0-\0");
        let (ok, out, err) = run_case(&["-l", "--files0-from=-"], &files, &stdin);
        assert!(!ok);
        assert_eq!(out, "1 a\n1 total\n");
        assert_eq!(
            err,
            "wc: when reading file names from standard input, no file name of '-' allowed\n"
        );
    }

    #[test]
    fn a_missing_files0_list_counts_nothing() {
        let files = MemFiles::new(&[]);
        let stdin = MemStdin::new(b"");
        let (ok, out, err) = run_case(&["--files0-from=list"], &files, &stdin);
        assert!(!ok);
        assert_eq!(out, "");
        assert!(
            err.starts_with("wc: cannot open 'list' for reading:"),
            "unexpected diagnostic: {err}"
        );
    }

    #[test]
    fn short_help_falls_back_to_the_usage_banner() {
        let files = MemFiles::new(&[]);
        let stdin = MemStdin::new(b"");
        let (ok, out, err) = run_case(&["-h"], &files, &stdin);
        assert!(ok);
        assert_eq!(out, alloc::format!("{USAGE}\n"));
        assert!(err.is_empty());
    }

    /// Every locale's `OPTIONS` section documents exactly the switches this
    /// parser accepts (`plans/APPS.md` §3.1): the flag tokens are
    /// language-neutral, so each translated document must carry the same
    /// keys as the canonical one. The documents are read from the bundle's
    /// own on-disk `Help/` tree — the single source the image builder
    /// plants — never a copy embedded in this crate.
    #[test]
    fn help_documents_the_parser_switches() {
        use alloc::format;
        use std::fs;

        let help_root = format!("{}/Help", env!("CARGO_MANIFEST_DIR"));
        let locales = ["default", "fr-FR", "de-DE", "es-ES", "uk-UA", "it-IT"];
        for locale in locales {
            let path = format!("{help_root}/{locale}/wc.md");
            let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
            for switch in [
                "`-c, --bytes`",
                "`-m, --chars`",
                "`-l, --lines`",
                "`-w, --words`",
                "`-L, --max-line-length`",
                "`--files0-from <file>`",
                "`--total <when>`",
                "`-h, -?`",
            ] {
                assert!(
                    text.contains(switch),
                    "{locale}/wc.md must document {switch}"
                );
            }
        }
    }

    #[test]
    fn a_failed_output_write_is_fatal() {
        let files = MemFiles::new(&[("f", b"data\n")]);
        let stdin = MemStdin::new(b"");
        let result = run(
            command(&["f"]),
            None,
            &files,
            &stdin,
            &NoHelp,
            &BrokenSink,
            &Sink::default(),
        );
        assert_eq!(result, Err(WcError::Output(Errno::NotImplemented)));
    }
}
