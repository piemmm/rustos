//! The listing engine: inspect each operand, read each directory, and write
//! the sorted, formatted listing to the terminal.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write as _;

use tairix_abi::stdinfo::{Human, Severity, StdInfoKind, StdInfoRecord};
use tairix_help::{own_short_help, HelpSource};

use crate::command::{Command, Format, Hidden, Indicator, Options, Sort};
use crate::error::LsError;
use crate::io::{Entry, Listing, Metadata, Output};

/// The usage banner a usage error is reported with, and the fallback the
/// short-help switches print when `ls`'s own Help tree is unavailable.
pub const USAGE: &str = "usage: ls [-aAdFghlmnopQrRsS1] [--] [path...]";

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
///   [`Errno`](tairix_abi::Errno) (e.g. [`Errno::NotFound`]).
/// * [`LsError::Read`] — a directory could not be read.
/// * [`LsError::Output`] — writing the terminal failed.
///
/// [`Errno::NotFound`]: tairix_abi::Errno::NotFound
pub fn run(
    command: Command,
    locale: Option<&str>,
    fs: &dyn Listing,
    help: &dyn HelpSource,
    out: &dyn Output,
) -> Result<(), LsError> {
    match command {
        Command::Help => short_help(locale, help, out),
        Command::List { options, paths } => list(options, &paths, fs, out),
    }
}

/// Render `ls`'s own short help (`NAME` + `SYNOPSIS` + compact `OPTIONS`)
/// from its own Help tree through the one shared engine; when no document
/// can be served (a build without the bundle's documents) the usage banner
/// stands in — the tool's own text, not fabricated help content — so `-h`
/// never fails.
fn short_help(
    locale: Option<&str>,
    help: &dyn HelpSource,
    out: &dyn Output,
) -> Result<(), LsError> {
    let bytes =
        own_short_help(help, locale, OWN_WORD).unwrap_or_else(|| format!("{USAGE}\n").into_bytes());
    out.write_all(&bytes).map_err(LsError::Output)
}

/// Inspect every operand, then render and **write** the file block followed
/// by each directory block (depth-first under `-R`), one block at a time.
///
/// Output streams as the traversal proceeds — each block is written the
/// moment its directory has been read — so a recursive listing shows
/// progress immediately and the tool's memory stays bounded by the largest
/// single directory, never the whole tree (a filesystem can be arbitrarily
/// larger than RAM). A filesystem error mid-traversal therefore surfaces
/// after the blocks already listed, exactly as any streaming tool behaves.
/// Hidden entries filtered from the listing are counted and noted at the
/// end on the advisory stream — never in the listing itself.
fn list(
    options: Options,
    paths: &[String],
    fs: &dyn Listing,
    out: &dyn Output,
) -> Result<(), LsError> {
    let mut files: Vec<(String, Metadata)> = Vec::new();
    let mut dirs: Vec<String> = Vec::new();
    for path in paths {
        let meta = fs.stat(path).map_err(LsError::Stat)?;
        if meta.kind.is_dir() && !options.directory {
            dirs.push(path.clone());
        } else {
            files.push((path.clone(), meta));
        }
    }

    // Directory blocks carry a `path:` header with several operands, and
    // always when recursing (the reader must know which directory each
    // block describes).
    let headered = paths.len() > 1 || options.recursive;
    let mut buf = String::new();
    let mut first = true;
    let mut hidden_omitted: u64 = 0;

    if !files.is_empty() {
        sort_rows(&mut files, options);
        open_block(&mut buf, &mut first, None);
        // No `total` line: the GNU tool totals directory listings only,
        // never the loose file-operand block.
        render_rows(&mut buf, &files, options);
        write_block(out, &mut buf)?;
    }
    // A depth-first worklist: operands are pushed reversed so they pop in
    // command-line order, and a listed directory's children are pushed
    // reversed so they pop in rendered order.
    dirs.reverse();
    let mut pending = dirs;
    while let Some(path) = pending.pop() {
        let mut entries = fs.read_dir(&path).map_err(LsError::Read)?;
        match options.hidden {
            Hidden::Skip => {
                let before = entries.len();
                entries.retain(|entry| !entry.name.starts_with('.'));
                hidden_omitted += (before - entries.len()) as u64;
            }
            Hidden::AlmostAll => entries.retain(|entry| entry.name != "." && entry.name != ".."),
            Hidden::All => {}
        }
        let mut rows = rows_for(&path, &entries, options, fs)?;
        sort_rows(&mut rows, options);
        open_block(&mut buf, &mut first, headered.then_some(path.as_str()));
        if options.size || options.long {
            render_total(&mut buf, &rows, options);
        }
        render_rows(&mut buf, &rows, options);
        write_block(out, &mut buf)?;
        if options.recursive {
            // `.`/`..` never recurse — a listing must terminate even when
            // `-a` renders them.
            for (name, meta) in rows.iter().rev() {
                if meta.kind.is_dir() && name != "." && name != ".." {
                    pending.push(join(&path, name));
                }
            }
        }
    }

    if hidden_omitted > 0 {
        emit_omission_record(out, hidden_omitted);
    }
    Ok(())
}

/// Write one rendered block to the terminal and reset the buffer for the
/// next, so the buffer's capacity is reused across blocks and the listing
/// streams as the traversal proceeds.
fn write_block(out: &dyn Output, buf: &mut String) -> Result<(), LsError> {
    out.write_all(buf.as_bytes()).map_err(LsError::Output)?;
    buf.clear();
    Ok(())
}

/// Whether rendering under `options` needs each entry's full metadata.
///
/// The long format renders mode/owner/group/size, `-s` renders allocated
/// blocks, a size sort compares sizes, and `-F` needs the mode's execute
/// bits for `*` — everything else renders names straight off the one
/// `read_dir`, so the per-entry `stat` is paid only when asked for.
fn needs_stat(options: Options) -> bool {
    options.long
        || options.size
        || options.sort == Sort::Size
        || options.indicator == Indicator::Classify
}

/// The rendered rows of one directory block: each entry's name, with its
/// metadata attached.
fn rows_for(
    dir: &str,
    entries: &[Entry],
    options: Options,
    fs: &dyn Listing,
) -> Result<Vec<(String, Metadata)>, LsError> {
    let mut rows = Vec::with_capacity(entries.len());
    for entry in entries {
        let meta = if needs_stat(options) {
            fs.stat(&join(dir, &entry.name)).map_err(LsError::Stat)?
        } else {
            // The kind from the directory stream stands in and the zeroed
            // fields are unread.
            Metadata {
                kind: entry.kind,
                mode: 0,
                size: 0,
                allocated: 0,
                uid: 0,
                gid: 0,
            }
        };
        rows.push((entry.name.clone(), meta));
    }
    Ok(rows)
}

/// Order `rows` by the selected sort key, reversed under `-r`.
fn sort_rows(rows: &mut [(String, Metadata)], options: Options) {
    match options.sort {
        Sort::Name => rows.sort_by(|a, b| a.0.cmp(&b.0)),
        // Largest first, ties by name — the GNU `-S` order.
        Sort::Size => rows.sort_by(|a, b| b.1.size.cmp(&a.1.size).then_with(|| a.0.cmp(&b.0))),
    }
    if options.reverse {
        rows.reverse();
    }
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

/// The `-s` blocks cell for one entry: its allocated storage in
/// 1024-byte units, rounded up (the GNU default block size), or the `-h`
/// human form of the allocated bytes.
fn blocks_cell(meta: Metadata, options: Options) -> String {
    if options.human_readable {
        human_size(meta.allocated)
    } else {
        format!("{}", meta.allocated.div_ceil(1024))
    }
}

/// The `total` line of one directory block, printed for every directory
/// listing under `-l` or `-s` as in the GNU tool. Plain form: the sum of
/// the entries' individual block counts (each rounded up, exactly as its
/// `-s` cell renders). `-h` form: the human rendering of the summed
/// allocated bytes.
fn render_total(buf: &mut String, rows: &[(String, Metadata)], options: Options) {
    let rendered = if options.human_readable {
        human_size(rows.iter().map(|(_, meta)| meta.allocated).sum())
    } else {
        let blocks: u64 = rows
            .iter()
            .map(|(_, meta)| meta.allocated.div_ceil(1024))
            .sum();
        format!("{blocks}")
    };
    let _ = writeln!(buf, "total {rendered}");
}

/// Render `rows` into `buf`: the long format when selected, otherwise one
/// name per line or the `-m` comma arrangement, each prefixed by its
/// blocks cell under `-s`.
fn render_rows(buf: &mut String, rows: &[(String, Metadata)], options: Options) {
    if options.long {
        render_long(buf, rows, options);
        return;
    }
    match options.format {
        Format::OnePerLine => {
            let blocks_width = size_column_width(rows, options);
            for (name, meta) in rows {
                if options.size {
                    let _ = write!(buf, "{:>blocks_width$} ", blocks_cell(*meta, options));
                }
                buf.push_str(&decorate(name, *meta, options));
                buf.push('\n');
            }
        }
        Format::Commas => {
            if rows.is_empty() {
                return;
            }
            for (index, (name, meta)) in rows.iter().enumerate() {
                if index > 0 {
                    buf.push_str(", ");
                }
                if options.size {
                    let _ = write!(buf, "{} ", blocks_cell(*meta, options));
                }
                buf.push_str(&decorate(name, *meta, options));
            }
            buf.push('\n');
        }
    }
}

/// Width of the widest `-s` blocks cell in `rows` (0 when `-s` is off).
fn size_column_width(rows: &[(String, Metadata)], options: Options) -> usize {
    if !options.size {
        return 0;
    }
    rows.iter()
        .map(|(_, meta)| blocks_cell(*meta, options).len())
        .max()
        .unwrap_or(1)
}

/// Render the long format: mode, numeric owner and group (unless hidden by
/// `-g` / `-o`), size, then the decorated name, with each numeric column
/// right-aligned to its block-wide width.
///
/// Owner and group are numeric ids: resolving names needs the
/// capability-gated user database, which a listing must not demand — the
/// GNU tool falls back to numbers for exactly this case (`-n` renders the
/// same). There is no link count or timestamp column because the
/// filesystem contract carries neither yet (see the Help document).
fn render_long(buf: &mut String, rows: &[(String, Metadata)], options: Options) {
    let size_cell = |meta: &Metadata| {
        if options.human_readable {
            human_size(meta.size)
        } else {
            format!("{}", meta.size)
        }
    };
    let width_of = |cell: fn(&Metadata) -> String| {
        rows.iter()
            .map(|(_, meta)| cell(meta).len())
            .max()
            .unwrap_or(1)
    };
    let uid_width = width_of(|meta| format!("{}", meta.uid));
    let gid_width = width_of(|meta| format!("{}", meta.gid));
    let size_width = rows
        .iter()
        .map(|(_, meta)| size_cell(meta).len())
        .max()
        .unwrap_or(1);
    let blocks_width = size_column_width(rows, options);
    for (name, meta) in rows {
        // Writing into a `String` is infallible, so the `fmt::Result` is
        // discarded deliberately.
        if options.size {
            let _ = write!(buf, "{:>blocks_width$} ", blocks_cell(*meta, options));
        }
        let _ = write!(buf, "{}", mode_string(*meta));
        if !options.hide_owner {
            let _ = write!(buf, " {:>uid_width$}", meta.uid);
        }
        if !options.hide_group {
            let _ = write!(buf, " {:>gid_width$}", meta.gid);
        }
        let _ = writeln!(
            buf,
            " {:>size_width$} {}",
            size_cell(meta),
            decorate(name, *meta, options)
        );
    }
}

/// The rendered form of one name: quoted under `-Q`, with the `-p` / `-F`
/// indicator suffix appended after the closing quote, as in the GNU tool.
fn decorate(name: &str, meta: Metadata, options: Options) -> String {
    let mut rendered = if options.quote {
        quote_name(name)
    } else {
        String::from(name)
    };
    match options.indicator {
        Indicator::None => {}
        Indicator::Slash => {
            if meta.kind.is_dir() {
                rendered.push('/');
            }
        }
        Indicator::Classify => {
            if meta.kind.is_dir() {
                rendered.push('/');
            } else if meta.mode & 0o111 != 0 {
                rendered.push('*');
            }
        }
    }
    rendered
}

/// `name` in the GNU C-style double-quoted form: `\\` and `\"` for the
/// backslash and quote, C escapes for the common control characters, and
/// three-digit octal for the rest.
fn quote_name(name: &str) -> String {
    let mut quoted = String::with_capacity(name.len() + 2);
    quoted.push('"');
    for ch in name.chars() {
        match ch {
            '"' => quoted.push_str("\\\""),
            '\\' => quoted.push_str("\\\\"),
            '\n' => quoted.push_str("\\n"),
            '\t' => quoted.push_str("\\t"),
            '\r' => quoted.push_str("\\r"),
            _ if (ch as u32) < 0x20 || ch as u32 == 0x7f => {
                let _ = write!(quoted, "\\{:03o}", ch as u32);
            }
            _ => quoted.push(ch),
        }
    }
    quoted.push('"');
    quoted
}

/// `size` in the GNU `-h` form: plain bytes below 1024, then powers of
/// 1024 as `K`, `M`, `G`, …, rounded up — one decimal place below 10, whole
/// numbers from 10.
fn human_size(size: u64) -> String {
    const UNITS: [char; 6] = ['K', 'M', 'G', 'T', 'P', 'E'];
    if size < 1024 {
        return format!("{size}");
    }
    let mut unit = 0;
    let mut base: u128 = 1024;
    let size = u128::from(size);
    while unit + 1 < UNITS.len() && size >= base * 1024 {
        base *= 1024;
        unit += 1;
    }
    // Tenths of a unit, rounded up (the GNU ceiling), e.g. 1025 -> `1.1K`.
    let tenths = (size * 10).div_ceil(base);
    if tenths < 100 {
        format!("{}.{}{}", tenths / 10, tenths % 10, UNITS[unit])
    } else {
        format!("{}{}", size.div_ceil(base), UNITS[unit])
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
    use crate::command::{Command, Format, Hidden, Indicator, Options, Sort};
    use crate::error::LsError;
    use crate::io::{Entry, Listing, Metadata, Output};
    use alloc::string::{String, ToString};
    use alloc::vec::Vec;
    use core::cell::RefCell;
    use tairix_abi::fs::FileKind;
    use tairix_abi::Errno;
    use tairix_help::{HelpSource, SourceError};

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
                    allocated: size,
                    uid: UID,
                    gid: GID,
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
                    allocated: 0,
                    uid: UID,
                    gid: GID,
                },
            ));
            self.dirs.push((path.to_string(), Vec::new()));
            self
        }

        /// Declare `name` inside `dir` and give the joined path a stat row,
        /// so the long format's per-entry stat finds it. Allocation equals
        /// the byte size; [`entry_alloc`](Self::entry_alloc) sets a
        /// divergent allocation.
        fn entry(self, dir: &str, name: &str, kind: FileKind, mode: u32, size: u64) -> Self {
            self.entry_alloc(dir, name, kind, mode, size, size)
        }

        /// [`entry`](Self::entry) with an allocation that differs from the
        /// byte size (a sparse or tail-padded file), so the `-s` tests can
        /// prove the blocks column renders allocation, never `size`.
        fn entry_alloc(
            mut self,
            dir: &str,
            name: &str,
            kind: FileKind,
            mode: u32,
            size: u64,
            allocated: u64,
        ) -> Self {
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
            self.stat.push((
                super::join(dir, name),
                Metadata {
                    kind,
                    mode,
                    size,
                    allocated,
                    uid: UID,
                    gid: GID,
                },
            ));
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
                allocated: 0,
                uid: UID,
                gid: GID,
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
            Ok(alloc::vec![String::from("en-US")])
        }

        fn read(&self, locale_dir: &str, file_name: &str) -> Result<Option<Vec<u8>>, SourceError> {
            if locale_dir == "en-US" && file_name == "ls.md" {
                Ok(Some(DOC.as_bytes().to_vec()))
            } else {
                Ok(None)
            }
        }
    }

    /// Captures every stdout write (chunk boundaries preserved, so a test
    /// can assert the listing streams block by block) and every fd 3
    /// record; optionally fails on the first stdout write.
    struct Recorder {
        chunks: RefCell<Vec<Vec<u8>>>,
        records: RefCell<Vec<Vec<u8>>>,
        fail: bool,
    }

    impl Recorder {
        fn new() -> Self {
            Self {
                chunks: RefCell::new(Vec::new()),
                records: RefCell::new(Vec::new()),
                fail: false,
            }
        }

        fn failing() -> Self {
            Self {
                chunks: RefCell::new(Vec::new()),
                records: RefCell::new(Vec::new()),
                fail: true,
            }
        }

        fn text(&self) -> String {
            let joined: Vec<u8> = self.chunks.borrow().concat();
            String::from_utf8(joined).expect("utf8 output")
        }

        fn chunks(&self) -> Vec<String> {
            self.chunks
                .borrow()
                .iter()
                .map(|c| String::from_utf8(c.clone()).expect("utf8 chunk"))
                .collect()
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
            self.chunks.borrow_mut().push(bytes.to_vec());
            Ok(())
        }

        fn info(&self, record: &[u8]) {
            self.records.borrow_mut().push(record.to_vec());
        }
    }

    /// The owning ids every fixture row carries, so long-format
    /// expectations are explicit about the owner and group columns.
    const UID: u32 = 1000;
    const GID: u32 = 100;

    fn list_with(options: Options, paths: &[&str]) -> Command {
        Command::List {
            options,
            paths: paths.iter().map(|p| (*p).to_string()).collect::<Vec<_>>(),
        }
    }

    fn list(all: bool, long: bool, paths: &[&str]) -> Command {
        list_with(
            Options {
                hidden: if all { Hidden::All } else { Hidden::Skip },
                long,
                ..Options::DEFAULT
            },
            paths,
        )
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
        assert_eq!(
            out.text(),
            "total 5\ndrwxr-xr-x 1000 100 4096 d\n-rw-r--r-- 1000 100    7 f\n"
        );
    }

    #[test]
    fn long_format_stats_entries_under_a_slash_terminated_operand() {
        // The joined per-entry path must not double the trailing slash.
        let fs = TreeFs::new()
            .dir("dir/")
            .entry("dir/", "f", FileKind::Regular, 0o600, 3);
        let out = Recorder::new();
        assert_eq!(run_ls(list(false, true, &["dir/"]), &fs, &out), Ok(()));
        assert_eq!(out.text(), "total 1\n-rw------- 1000 100 3 f\n");
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

    #[test]
    fn directory_option_lists_the_operand_itself() {
        let fs = TreeFs::new()
            .dir("dir")
            .entry("dir", "x", FileKind::Regular, 0o644, 0);
        let out = Recorder::new();
        assert_eq!(
            run_ls(
                list_with(
                    Options {
                        directory: true,
                        ..Options::DEFAULT
                    },
                    &["dir"],
                ),
                &fs,
                &out,
            ),
            Ok(())
        );
        assert_eq!(out.text(), "dir\n");
    }

    #[test]
    fn recursive_descends_depth_first_with_headers() {
        let fs = TreeFs::new()
            .dir("top")
            .entry("top", "z", FileKind::Regular, 0o644, 0)
            .dir("top/sub")
            .entry("top/sub", "x", FileKind::Regular, 0o644, 0);
        // Declare `sub` inside `top` after `top/sub` exists as a directory.
        let fs = fs.entry("top", "sub", FileKind::Directory, 0o755, 0);
        let out = Recorder::new();
        assert_eq!(
            run_ls(
                list_with(
                    Options {
                        recursive: true,
                        ..Options::DEFAULT
                    },
                    &["top"],
                ),
                &fs,
                &out,
            ),
            Ok(())
        );
        assert_eq!(out.text(), "top:\nsub\nz\n\ntop/sub:\nx\n");
    }

    /// The listing streams: each directory block is written the moment its
    /// directory has been read (one write per block), never accumulated
    /// into a single end-of-run write whose memory would grow with the
    /// whole tree.
    #[test]
    fn recursive_listing_writes_each_block_as_it_is_read() {
        let fs = TreeFs::new()
            .dir("top")
            .entry("top", "z", FileKind::Regular, 0o644, 0)
            .dir("top/sub")
            .entry("top/sub", "x", FileKind::Regular, 0o644, 0);
        let fs = fs.entry("top", "sub", FileKind::Directory, 0o755, 0);
        let out = Recorder::new();
        assert_eq!(
            run_ls(
                list_with(
                    Options {
                        recursive: true,
                        ..Options::DEFAULT
                    },
                    &["top"],
                ),
                &fs,
                &out,
            ),
            Ok(())
        );
        assert_eq!(out.chunks(), ["top:\nsub\nz\n", "\ntop/sub:\nx\n"]);
    }

    /// A filesystem error mid-recursion surfaces after the blocks already
    /// listed: the traversal streamed them out when their directories were
    /// read, exactly as any streaming tool behaves.
    #[test]
    fn a_read_dir_error_mid_recursion_keeps_the_blocks_already_written() {
        // `sub` is announced by `top` but has no readable node, so its
        // `read_dir` fails after `top`'s block has been written.
        let fs = TreeFs::new()
            .dir("top")
            .entry("top", "sub", FileKind::Directory, 0o755, 0);
        let out = Recorder::new();
        assert_eq!(
            run_ls(
                list_with(
                    Options {
                        recursive: true,
                        ..Options::DEFAULT
                    },
                    &["top"],
                ),
                &fs,
                &out,
            ),
            Err(LsError::Read(Errno::NotFound))
        );
        assert_eq!(out.text(), "top:\nsub\n");
    }

    #[test]
    fn reverse_reverses_the_name_order() {
        let fs = TreeFs::new()
            .dir(".")
            .entry(".", "a", FileKind::Regular, 0o644, 0)
            .entry(".", "b", FileKind::Regular, 0o644, 0)
            .entry(".", "c", FileKind::Regular, 0o644, 0);
        let out = Recorder::new();
        assert_eq!(
            run_ls(
                list_with(
                    Options {
                        reverse: true,
                        ..Options::DEFAULT
                    },
                    &["."],
                ),
                &fs,
                &out,
            ),
            Ok(())
        );
        assert_eq!(out.text(), "c\nb\na\n");
    }

    #[test]
    fn size_sort_lists_largest_first_ties_by_name() {
        let fs = TreeFs::new()
            .dir(".")
            .entry(".", "small", FileKind::Regular, 0o644, 1)
            .entry(".", "big", FileKind::Regular, 0o644, 100)
            .entry(".", "a-mid", FileKind::Regular, 0o644, 50)
            .entry(".", "b-mid", FileKind::Regular, 0o644, 50);
        let out = Recorder::new();
        assert_eq!(
            run_ls(
                list_with(
                    Options {
                        sort: Sort::Size,
                        ..Options::DEFAULT
                    },
                    &["."],
                ),
                &fs,
                &out,
            ),
            Ok(())
        );
        assert_eq!(out.text(), "big\na-mid\nb-mid\nsmall\n");
    }

    #[test]
    fn commas_format_joins_names_on_one_line() {
        let fs = TreeFs::new()
            .dir(".")
            .entry(".", "a", FileKind::Regular, 0o644, 0)
            .entry(".", "b", FileKind::Regular, 0o644, 0)
            .entry(".", "c", FileKind::Regular, 0o644, 0);
        let out = Recorder::new();
        assert_eq!(
            run_ls(
                list_with(
                    Options {
                        format: Format::Commas,
                        ..Options::DEFAULT
                    },
                    &["."],
                ),
                &fs,
                &out,
            ),
            Ok(())
        );
        assert_eq!(out.text(), "a, b, c\n");
    }

    #[test]
    fn quoted_names_escape_quotes_and_controls() {
        let fs = TreeFs::new()
            .dir(".")
            .entry(".", "a\"b", FileKind::Regular, 0o644, 0)
            .entry(".", "new\nline", FileKind::Regular, 0o644, 0);
        let out = Recorder::new();
        assert_eq!(
            run_ls(
                list_with(
                    Options {
                        quote: true,
                        ..Options::DEFAULT
                    },
                    &["."],
                ),
                &fs,
                &out,
            ),
            Ok(())
        );
        assert_eq!(out.text(), "\"a\\\"b\"\n\"new\\nline\"\n");
    }

    #[test]
    fn classify_marks_directories_and_executables() {
        let fs = TreeFs::new()
            .dir(".")
            .entry(".", "bin", FileKind::Regular, 0o755, 0)
            .entry(".", "dir", FileKind::Directory, 0o755, 0)
            .entry(".", "plain", FileKind::Regular, 0o644, 0);
        let out = Recorder::new();
        assert_eq!(
            run_ls(
                list_with(
                    Options {
                        indicator: Indicator::Classify,
                        ..Options::DEFAULT
                    },
                    &["."],
                ),
                &fs,
                &out,
            ),
            Ok(())
        );
        assert_eq!(out.text(), "bin*\ndir/\nplain\n");
    }

    #[test]
    fn slash_indicator_marks_directories_only() {
        let fs = TreeFs::new()
            .dir(".")
            .entry(".", "bin", FileKind::Regular, 0o755, 0)
            .entry(".", "dir", FileKind::Directory, 0o755, 0);
        let out = Recorder::new();
        assert_eq!(
            run_ls(
                list_with(
                    Options {
                        indicator: Indicator::Slash,
                        ..Options::DEFAULT
                    },
                    &["."],
                ),
                &fs,
                &out,
            ),
            Ok(())
        );
        assert_eq!(out.text(), "bin\ndir/\n");
    }

    #[test]
    fn human_readable_sizes_scale_in_the_long_format() {
        let fs = TreeFs::new()
            .dir(".")
            .entry(".", "a", FileKind::Regular, 0o644, 500)
            .entry(".", "b", FileKind::Regular, 0o644, 1025)
            .entry(".", "c", FileKind::Regular, 0o644, 10 * 1024 * 1024);
        let out = Recorder::new();
        assert_eq!(
            run_ls(
                list_with(
                    Options {
                        long: true,
                        human_readable: true,
                        ..Options::DEFAULT
                    },
                    &["."],
                ),
                &fs,
                &out,
            ),
            Ok(())
        );
        assert_eq!(
            out.text(),
            "total 11M\n\
             -rw-r--r-- 1000 100  500 a\n\
             -rw-r--r-- 1000 100 1.1K b\n\
             -rw-r--r-- 1000 100  10M c\n"
        );
    }

    #[test]
    fn hide_owner_and_hide_group_drop_their_columns() {
        let fs = TreeFs::new()
            .dir(".")
            .entry(".", "f", FileKind::Regular, 0o644, 7);
        let hidden_owner = Recorder::new();
        assert_eq!(
            run_ls(
                list_with(
                    Options {
                        long: true,
                        hide_owner: true,
                        ..Options::DEFAULT
                    },
                    &["."],
                ),
                &fs,
                &hidden_owner,
            ),
            Ok(())
        );
        assert_eq!(hidden_owner.text(), "total 1\n-rw-r--r-- 100 7 f\n");
        let hidden_group = Recorder::new();
        assert_eq!(
            run_ls(
                list_with(
                    Options {
                        long: true,
                        hide_group: true,
                        ..Options::DEFAULT
                    },
                    &["."],
                ),
                &fs,
                &hidden_group,
            ),
            Ok(())
        );
        assert_eq!(hidden_group.text(), "total 1\n-rw-r--r-- 1000 7 f\n");
    }

    #[test]
    fn almost_all_shows_dotfiles_but_never_dot_or_dot_dot() {
        let fs = TreeFs::new()
            .dir(".")
            .entry(".", ".", FileKind::Directory, 0o755, 0)
            .entry(".", "..", FileKind::Directory, 0o755, 0)
            .entry(".", ".hidden", FileKind::Regular, 0o644, 0)
            .entry(".", "vis", FileKind::Regular, 0o644, 0);
        let out = Recorder::new();
        assert_eq!(
            run_ls(
                list_with(
                    Options {
                        hidden: Hidden::AlmostAll,
                        ..Options::DEFAULT
                    },
                    &["."],
                ),
                &fs,
                &out,
            ),
            Ok(())
        );
        assert_eq!(out.text(), ".hidden\nvis\n");
        // Nothing was hidden *by default filtering*, so no advisory record.
        assert!(out.records().is_empty());
    }

    #[test]
    fn size_option_renders_allocation_not_byte_size() {
        // `sparse` stores fewer bytes than its length; `padded` more. The
        // blocks column must render the *allocation*, rounded up to
        // 1024-byte units and right-aligned, under a `total` of the same
        // units.
        let fs = TreeFs::new()
            .dir(".")
            .entry_alloc(".", "sparse", FileKind::Regular, 0o644, 1_000_000, 4096)
            .entry_alloc(".", "padded", FileKind::Regular, 0o644, 10, 12_288);
        let out = Recorder::new();
        assert_eq!(
            run_ls(
                list_with(
                    Options {
                        size: true,
                        ..Options::DEFAULT
                    },
                    &["."],
                ),
                &fs,
                &out,
            ),
            Ok(())
        );
        assert_eq!(out.text(), "total 16\n12 padded\n 4 sparse\n");
    }

    #[test]
    fn size_option_prefixes_the_long_format() {
        let fs = TreeFs::new()
            .dir(".")
            .entry(".", "d", FileKind::Directory, 0o755, 4096)
            .entry(".", "f", FileKind::Regular, 0o644, 7);
        let out = Recorder::new();
        assert_eq!(
            run_ls(
                list_with(
                    Options {
                        long: true,
                        size: true,
                        ..Options::DEFAULT
                    },
                    &["."],
                ),
                &fs,
                &out,
            ),
            Ok(())
        );
        assert_eq!(
            out.text(),
            "total 5\n4 drwxr-xr-x 1000 100 4096 d\n1 -rw-r--r-- 1000 100    7 f\n"
        );
    }

    #[test]
    fn size_option_scales_under_human_readable() {
        let fs = TreeFs::new()
            .dir(".")
            .entry_alloc(".", "a", FileKind::Regular, 0o644, 5, 4096)
            .entry_alloc(".", "b", FileKind::Regular, 0o644, 5, 1_048_576);
        let out = Recorder::new();
        assert_eq!(
            run_ls(
                list_with(
                    Options {
                        size: true,
                        human_readable: true,
                        ..Options::DEFAULT
                    },
                    &["."],
                ),
                &fs,
                &out,
            ),
            Ok(())
        );
        assert_eq!(out.text(), "total 1.1M\n4.0K a\n1.0M b\n");
    }

    #[test]
    fn size_option_joins_the_commas_format() {
        let fs = TreeFs::new()
            .dir(".")
            .entry(".", "a", FileKind::Regular, 0o644, 100)
            .entry(".", "b", FileKind::Regular, 0o644, 2048);
        let out = Recorder::new();
        assert_eq!(
            run_ls(
                list_with(
                    Options {
                        size: true,
                        format: Format::Commas,
                        ..Options::DEFAULT
                    },
                    &["."],
                ),
                &fs,
                &out,
            ),
            Ok(())
        );
        assert_eq!(out.text(), "total 3\n1 a, 2 b\n");
    }

    #[test]
    fn size_option_totals_directories_never_file_operands() {
        // A loose file operand gets its blocks cell but no `total` line —
        // the GNU tool totals directory listings only.
        let fs = TreeFs::new().file("a.txt", 0o644, 3000);
        let out = Recorder::new();
        assert_eq!(
            run_ls(
                list_with(
                    Options {
                        size: true,
                        ..Options::DEFAULT
                    },
                    &["a.txt"],
                ),
                &fs,
                &out,
            ),
            Ok(())
        );
        assert_eq!(out.text(), "3 a.txt\n");
    }

    #[test]
    fn human_size_rounds_up_like_the_gnu_tool() {
        for (size, expected) in [
            (0u64, "0"),
            (1023, "1023"),
            (1024, "1.0K"),
            (1025, "1.1K"),
            (10 * 1024, "10K"),
            (1024 * 1024, "1.0M"),
            (u64::MAX, "16E"),
        ] {
            assert_eq!(super::human_size(size), expected, "{size}");
        }
    }
}
