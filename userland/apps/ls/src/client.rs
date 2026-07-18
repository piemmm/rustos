//! The listing engine: inspect each operand, read each directory, and write
//! the sorted, formatted listing to the terminal.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::cmp::Reverse;
use core::fmt::Write as _;

use tairix_abi::stdinfo::{Human, Severity, StdInfoKind, StdInfoRecord};
use tairix_abi::time::Time64;
use tairix_abi::NodeTimes;
use tairix_fsmeta::calendar::CivilTime;
use tairix_help::{own_short_help, HelpSource};
use tairix_vt::str_width;

use crate::command::{
    Command, Filters, Format, Hidden, Indicator, Options, QuotingStyle, Sort, TimeField, TimeStyle,
};
use crate::error::LsError;
use crate::io::{Entry, Listing, Metadata, Output};

/// The usage banner a usage error is reported with, and the fallback the
/// short-help switches print when `ls`'s own Help tree is unavailable.
pub const USAGE: &str = "usage: ls [-aACdFghlmnopQrRsSx1] [-w cols] [--] [path...]";

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
    now: Time64,
    fs: &dyn Listing,
    help: &dyn HelpSource,
    out: &dyn Output,
) -> Result<(), LsError> {
    match command {
        Command::Help => short_help(locale, help, out),
        Command::List {
            options,
            filters,
            paths,
        } => list(options, &filters, &paths, now, fs, out),
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
    filters: &Filters,
    paths: &[String],
    now: Time64,
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
    // The GNU arrangement rule: multiple columns when writing to a
    // terminal, one name per line otherwise — decided against the attested
    // console, never guessed. The column budget is the given width, the
    // attested width, or the conventional 80.
    let terminal = out.terminal_width();
    let format = resolve_format(options, terminal);
    let width = resolve_width(options, terminal);
    let quoting = Quoting::resolve(options, terminal);
    let mut buf = String::new();
    let mut first = true;
    let mut hidden_omitted: u64 = 0;

    if !files.is_empty() {
        sort_rows(&mut files, options);
        open_block(&mut buf, &mut first, None);
        // No `total` line: the GNU tool totals directory listings only,
        // never the loose file-operand block.
        render_rows(&mut buf, &files, options, format, width, now, quoting);
        write_block(out, &mut buf)?;
    }
    // A depth-first worklist: operands are pushed reversed so they pop in
    // command-line order, and a listed directory's children are pushed
    // reversed so they pop in rendered order.
    dirs.reverse();
    let mut pending = dirs;
    while let Some(path) = pending.pop() {
        let mut entries = fs.read_dir(&path).map_err(LsError::Read)?;
        // Two filtering stages, in GNU's order. First the default dotfile
        // rule, whose omissions are counted for the advisory record (a
        // surprising, non-requested hiding). Then the explicit `-B`/`-I`/
        // `--hide` name filters, which the user asked for and so are applied
        // silently — never advertised as an omission.
        let before_dotfiles = entries.len();
        match options.hidden {
            Hidden::Skip => entries.retain(|entry| !entry.name.starts_with('.')),
            Hidden::AlmostAll => entries.retain(|entry| entry.name != "." && entry.name != ".."),
            Hidden::All => {}
        }
        if options.hidden == Hidden::Skip {
            hidden_omitted += (before_dotfiles - entries.len()) as u64;
        }
        let show_hidden = options.hidden != Hidden::Skip;
        entries.retain(|entry| !filters.suppresses(&entry.name, show_hidden));
        let mut rows = rows_for(&path, &entries, options, fs)?;
        sort_rows(&mut rows, options);
        open_block(&mut buf, &mut first, headered.then_some(path.as_str()));
        if options.size || options.long {
            render_total(&mut buf, &rows, options);
        }
        render_rows(&mut buf, &rows, options, format, width, now, quoting);
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
/// The long format renders mode/owner/group/size/date, `-s` renders
/// allocated blocks, `-i` renders the node number, a size or time sort
/// compares those fields, and `-F` needs the mode's execute bits for `*` —
/// everything else renders names straight off the one `read_dir`, so the
/// per-entry `stat` is paid only when asked for.
fn needs_stat(options: Options) -> bool {
    options.long
        || options.size
        || options.inode
        || options.sort == Sort::Size
        || options.sort == Sort::Time
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
                inode: 0,
                times: NodeTimes::default(),
            }
        };
        rows.push((entry.name.clone(), meta));
    }
    Ok(rows)
}

/// The timestamp `options` selects for the long-format date column and the
/// `-t` sort: modified (the default), accessed (`-u`), changed (`-c`), or
/// created (`--time=birth`).
fn selected_time(meta: &Metadata, field: TimeField) -> Time64 {
    match field {
        TimeField::Modified => meta.times.modified,
        TimeField::Accessed => meta.times.accessed,
        TimeField::Changed => meta.times.changed,
        TimeField::Created => meta.times.created,
    }
}

/// The file-name extension `-X` sorts on: the text from the last `.`
/// (inclusive), or the empty string when the name has none — exactly what
/// GNU compares (`strrchr(name, '.')`). A leading-dot name like `.bashrc`
/// therefore has extension `.bashrc`, matching the GNU tool.
fn extension(name: &str) -> &str {
    match name.rfind('.') {
        Some(dot) => &name[dot..],
        None => "",
    }
}

/// Order `rows` by the selected sort key, reversed under `-r`, then — under
/// `--group-directories-first` — float directories to the front.
fn sort_rows(rows: &mut [(String, Metadata)], options: Options) {
    match options.sort {
        // No sort: keep the directory (read) order the filesystem returned.
        Sort::None => {}
        Sort::Name => rows.sort_by(|a, b| a.0.cmp(&b.0)),
        // Largest first, ties by name — the GNU `-S` order.
        Sort::Size => rows.sort_by(|a, b| b.1.size.cmp(&a.1.size).then_with(|| a.0.cmp(&b.0))),
        // Newest first, ties by name — the GNU `-t` order.
        Sort::Time => rows.sort_by(|a, b| {
            selected_time(&b.1, options.time_field)
                .cmp(&selected_time(&a.1, options.time_field))
                .then_with(|| a.0.cmp(&b.0))
        }),
        // By extension, ties by name — the GNU `-X` order.
        Sort::Extension => rows.sort_by(|a, b| {
            extension(&a.0)
                .cmp(extension(&b.0))
                .then_with(|| a.0.cmp(&b.0))
        }),
        // Natural version order, ties by name — the GNU `-v` order.
        Sort::Version => rows.sort_by(|a, b| {
            version::filevercmp(a.0.as_bytes(), b.0.as_bytes()).then_with(|| a.0.cmp(&b.0))
        }),
    }
    if options.reverse {
        rows.reverse();
    }
    // Directories first is applied *after* the sort and the reverse, as a
    // stable partition: it keeps the sorted order within each group and puts
    // directories first regardless of `-r` — the GNU behaviour.
    if options.group_directories_first {
        rows.sort_by_key(|row| Reverse(row.1.kind.is_dir()));
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

/// The conventional fallback output width when no width is given and the
/// console cannot be attested (the GNU default).
const DEFAULT_WIDTH: usize = 80;

/// The blank columns between two grid columns (the GNU column gap).
const COLUMN_GAP: usize = 2;

/// Resolve the effective arrangement for one listing. An explicit
/// `-1`/`-C`/`-x`/`-m` wins; otherwise the GNU default is multiple columns
/// when standard output is an attested terminal and one name per line when
/// it is not (a pipe, a file, or an unattested console).
fn resolve_format(options: Options, terminal: Option<usize>) -> Format {
    options.format.unwrap_or({
        if terminal.is_some() {
            Format::Columns
        } else {
            Format::OnePerLine
        }
    })
}

/// Resolve the column budget: the explicit `-w`/`--width` value, else the
/// attested terminal width, else the conventional 80. A width of `0` is the
/// GNU "unlimited line length" value, represented here as the maximum so a
/// column arrangement lays every entry on one line.
fn resolve_width(options: Options, terminal: Option<usize>) -> usize {
    match options.width.or(terminal) {
        Some(0) => usize::MAX,
        Some(cols) => cols,
        None => DEFAULT_WIDTH,
    }
}

/// Render `rows` into `buf` in the resolved `format`, wrapping column and
/// comma arrangements to `width`. The long format (`-l`) ignores `format`
/// and `width` — it is always one entry per line.
fn render_rows(
    buf: &mut String,
    rows: &[(String, Metadata)],
    options: Options,
    format: Format,
    width: usize,
    now: Time64,
    quoting: Quoting,
) {
    if options.long {
        render_long(buf, rows, options, now, quoting);
        return;
    }
    match format {
        Format::OnePerLine => render_one_per_line(buf, rows, options, quoting),
        Format::Columns => render_grid(buf, rows, options, width, Fill::TopToBottom, quoting),
        Format::Across => render_grid(buf, rows, options, width, Fill::LeftToRight, quoting),
        Format::Commas => render_commas(buf, rows, options, width, quoting),
    }
}

/// The rendered form of one entry as it appears in a listing cell: its
/// `-i` inode prefix (right-aligned to `inode_width`) then its `-s`
/// allocated-blocks prefix (right-aligned to `blocks_width`), each `0` for
/// no prefix, followed by the decorated name. The inode precedes the
/// blocks, as in the GNU tool.
fn entry_cell(
    name: &str,
    meta: Metadata,
    options: Options,
    inode_width: usize,
    blocks_width: usize,
    quoting: Quoting,
) -> String {
    let mut cell = String::new();
    // Writing into a `String` is infallible, so the `fmt::Result`s are
    // discarded deliberately.
    if options.inode {
        let _ = write!(cell, "{:>inode_width$} ", meta.inode);
    }
    if options.size {
        let _ = write!(cell, "{:>blocks_width$} ", blocks_cell(meta, options));
    }
    cell.push_str(&decorate(name, meta, options, quoting));
    cell
}

/// Render one entry per line — the `-1` arrangement and the non-terminal
/// default. Each line carries its `-s` blocks cell, right-aligned to the
/// block's width, when `-s` is set.
fn render_one_per_line(
    buf: &mut String,
    rows: &[(String, Metadata)],
    options: Options,
    quoting: Quoting,
) {
    let inode_width = inode_column_width(rows, options);
    let blocks_width = size_column_width(rows, options);
    for (name, meta) in rows {
        buf.push_str(&entry_cell(
            name,
            *meta,
            options,
            inode_width,
            blocks_width,
            quoting,
        ));
        buf.push('\n');
    }
}

/// The `-m` comma arrangement: names separated by `, `, wrapped so no line
/// exceeds `width`. The comma stays at the end of a full line and the next
/// line begins with the name, no leading space — the GNU `-m` layout.
fn render_commas(
    buf: &mut String,
    rows: &[(String, Metadata)],
    options: Options,
    width: usize,
    quoting: Quoting,
) {
    if rows.is_empty() {
        return;
    }
    // `-m` does not pad the `-i` inode or `-s` blocks cell (GNU prints them
    // inline), so the cell is built with zero prefix widths.
    let mut pos = 0usize;
    for (index, (name, meta)) in rows.iter().enumerate() {
        let cell = entry_cell(name, *meta, options, 0, 0, quoting);
        let len = str_width(&cell);
        if index > 0 {
            // Keep the entry on the current line only if it and the `, `
            // separator still fit; otherwise break after the comma.
            if pos.saturating_add(len).saturating_add(COLUMN_GAP) < width {
                pos += COLUMN_GAP;
                buf.push_str(", ");
            } else {
                pos = 0;
                buf.push_str(",\n");
            }
        }
        buf.push_str(&cell);
        pos += len;
    }
    buf.push('\n');
}

/// The direction a column grid is filled.
#[derive(Clone, Copy)]
enum Fill {
    /// Down each column, then across (`-C`).
    TopToBottom,
    /// Across each row, then down (`-x`).
    LeftToRight,
}

/// Render `rows` as a multi-column grid wrapped to `width`, filled in the
/// `fill` direction — the GNU `-C` / `-x` arrangements.
///
/// The grid uses the greatest number of columns whose padded widths plus
/// the inter-column gaps fit `width`, exactly as the GNU tool sizes its
/// columns; a single column is the always-valid fallback. Cells are
/// left-justified and padded to their column's width, and the last cell on
/// each line carries no trailing padding.
fn render_grid(
    buf: &mut String,
    rows: &[(String, Metadata)],
    options: Options,
    width: usize,
    fill: Fill,
    quoting: Quoting,
) {
    let count = rows.len();
    if count == 0 {
        return;
    }
    let inode_width = inode_column_width(rows, options);
    let blocks_width = size_column_width(rows, options);
    let cells: Vec<String> = rows
        .iter()
        .map(|(name, meta)| entry_cell(name, *meta, options, inode_width, blocks_width, quoting))
        .collect();
    let widths: Vec<usize> = cells.iter().map(|cell| str_width(cell)).collect();

    let layout = grid_layout(&widths, width, fill);
    for row in 0..layout.rows {
        // The last present column index in this row decides which cell ends
        // the line (and so carries no trailing pad).
        let last_col = (0..layout.cols)
            .rev()
            .find(|&col| cell_index(fill, row, col, layout.rows, layout.cols) < count);
        let Some(last_col) = last_col else { continue };
        // Present columns in a row are contiguous from 0 to `last_col` in
        // both fill directions, so every index below is in range.
        for col in 0..=last_col {
            let index = cell_index(fill, row, col, layout.rows, layout.cols);
            buf.push_str(&cells[index]);
            if col != last_col {
                for _ in 0..layout.col_widths[col] + COLUMN_GAP - widths[index] {
                    buf.push(' ');
                }
            }
        }
        buf.push('\n');
    }
}

/// The flattened index of the cell at (`row`, `col`) in a `rows`×`cols`
/// grid filled in the `fill` direction.
fn cell_index(fill: Fill, row: usize, col: usize, rows: usize, cols: usize) -> usize {
    match fill {
        Fill::TopToBottom => col * rows + row,
        Fill::LeftToRight => row * cols + col,
    }
}

/// A chosen grid arrangement: its row and column counts and each column's
/// content width (the widest cell in that column).
struct GridLayout {
    rows: usize,
    cols: usize,
    col_widths: Vec<usize>,
}

/// Choose the grid arrangement for cells of the given display `widths`: the
/// greatest column count whose padded columns fit `width`, filled in the
/// `fill` direction. A single column always fits and is the fallback.
fn grid_layout(widths: &[usize], width: usize, fill: Fill) -> GridLayout {
    let count = widths.len();
    // A column needs at least one character plus the gap, so no more than
    // this many columns can ever fit; `saturating_add` guards the unlimited
    // (`usize::MAX`) width.
    let max_cols = width
        .saturating_add(COLUMN_GAP)
        .checked_div(1 + COLUMN_GAP)
        .unwrap_or(count)
        .clamp(1, count);
    for cols in (1..=max_cols).rev() {
        let rows = count.div_ceil(cols);
        // The candidate column count can leave the last row short; the real
        // column count is what `rows` rows actually hold.
        let real_cols = count.div_ceil(rows);
        let mut col_widths = alloc::vec![0usize; real_cols];
        for (index, &cell_width) in widths.iter().enumerate() {
            let col = match fill {
                Fill::TopToBottom => index / rows,
                Fill::LeftToRight => index % real_cols,
            };
            col_widths[col] = col_widths[col].max(cell_width);
        }
        let total: usize = col_widths.iter().sum::<usize>() + COLUMN_GAP * (real_cols - 1);
        if real_cols == 1 || total <= width {
            return GridLayout {
                rows,
                cols: real_cols,
                col_widths,
            };
        }
    }
    // Unreachable in practice — `cols == 1` always satisfies the test above
    // — but a single column is the correct total fallback.
    GridLayout {
        rows: count,
        cols: 1,
        col_widths: alloc::vec![widths.iter().copied().max().unwrap_or(0)],
    }
}

/// Width of the widest `-i` inode cell in `rows` (0 when `-i` is off).
fn inode_column_width(rows: &[(String, Metadata)], options: Options) -> usize {
    if !options.inode {
        return 0;
    }
    rows.iter()
        .map(|(_, meta)| decimal_len(meta.inode))
        .max()
        .unwrap_or(1)
}

/// Digits in the decimal rendering of `value` (at least one, for `0`).
fn decimal_len(value: u64) -> usize {
    let mut digits = 1;
    let mut rest = value;
    while rest >= 10 {
        rest /= 10;
        digits += 1;
    }
    digits
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

/// Render the long format: the optional `-i` inode and `-s` blocks columns,
/// then mode, numeric owner and group (unless hidden by `-g` / `-o`), size,
/// the selected timestamp, and finally the decorated name, with each
/// numeric column right-aligned and the date column padded so the names
/// align.
///
/// Owner and group are numeric ids: resolving names needs the
/// capability-gated user database, which a listing must not demand — the
/// GNU tool falls back to numbers for exactly this case (`-n` renders the
/// same). There is **no link-count column**: the VFS has no hard links yet,
/// so a fabricated count would be a lie — a documented divergence from GNU
/// (see the Help document). The timestamp column shows the time selected by
/// `-c` / `-u` / `--time` (modified by default), rendered in the style
/// chosen by `--time-style` / `--full-time`.
fn render_long(
    buf: &mut String,
    rows: &[(String, Metadata)],
    options: Options,
    now: Time64,
    quoting: Quoting,
) {
    let size_cell = |meta: &Metadata| {
        if options.human_readable {
            human_size(meta.size)
        } else {
            format!("{}", meta.size)
        }
    };
    let date_cell = |meta: &Metadata| {
        render_time(
            selected_time(meta, options.time_field),
            options.time_style,
            now,
        )
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
    // The date column is padded to its widest rendering so the names align;
    // within one style every row is the same width except `iso`, whose
    // recent and old forms differ.
    let date_width = rows
        .iter()
        .map(|(_, meta)| date_cell(meta).len())
        .max()
        .unwrap_or(0);
    let inode_width = inode_column_width(rows, options);
    let blocks_width = size_column_width(rows, options);
    for (name, meta) in rows {
        // Writing into a `String` is infallible, so the `fmt::Result` is
        // discarded deliberately.
        if options.inode {
            let _ = write!(buf, "{:>inode_width$} ", meta.inode);
        }
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
            " {:>size_width$} {:<date_width$} {}",
            size_cell(meta),
            date_cell(meta),
            decorate(name, *meta, options, quoting)
        );
    }
}

/// Month abbreviations for the `locale` time style (the C-locale English
/// names GNU `ls` prints without a locale).
const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// The GNU "recent" window, in seconds: half of the mean Gregorian year
/// (`31_556_952 / 2`). A stamp within the last six months renders with a
/// time-of-day; an older — or future — one renders with a year.
const RECENT_WINDOW_SECS: i64 = 31_556_952 / 2;

/// Whether `stamp` falls in the GNU "recent" window relative to `now`: at or
/// before `now` and no more than six months earlier. A future stamp is not
/// recent. When the clock is unset (`now` is the epoch) essentially every
/// real stamp is "old", so the long form (with a year) is shown — never a
/// guessed time-of-day.
fn is_recent(stamp: Time64, now: Time64) -> bool {
    let stamp_secs = stamp.secs();
    let now_secs = now.secs();
    stamp_secs <= now_secs && now_secs.saturating_sub(stamp_secs) <= RECENT_WINDOW_SECS
}

/// Render `stamp` in the requested [`TimeStyle`]. `now` decides the
/// recent/old split of the `locale` and `iso` styles. All fields are UTC;
/// the `full-iso` zone is therefore always `+0000`.
fn render_time(stamp: Time64, style: TimeStyle, now: Time64) -> String {
    let civil = CivilTime::from_time64(stamp);
    let month_name = MONTHS[civil.month as usize - 1];
    match style {
        TimeStyle::Locale => {
            if is_recent(stamp, now) {
                format!(
                    "{month_name} {:>2} {:02}:{:02}",
                    civil.day, civil.hour, civil.minute
                )
            } else {
                format!("{month_name} {:>2}  {:04}", civil.day, civil.year)
            }
        }
        TimeStyle::LongIso => civil.iso_minute(),
        TimeStyle::FullIso => format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:09} +0000",
            civil.year,
            civil.month,
            civil.day,
            civil.hour,
            civil.minute,
            civil.second,
            stamp.subsec_nanos(),
        ),
        TimeStyle::Iso => {
            if is_recent(stamp, now) {
                format!(
                    "{:02}-{:02} {:02}:{:02}",
                    civil.month, civil.day, civil.hour, civil.minute
                )
            } else {
                format!("{:04}-{:02}-{:02}", civil.year, civil.month, civil.day)
            }
        }
    }
}

/// The rendered form of one name: quoted in the resolved [`Quoting`] style,
/// with the `-p` / `-F` indicator suffix appended after the closing quote,
/// as in the GNU tool.
fn decorate(name: &str, meta: Metadata, options: Options, quoting: Quoting) -> String {
    let mut rendered = quoting.render(name);
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

/// A resolved name-quoting decision: the concrete [`QuotingStyle`] and
/// whether nongraphic characters are shown as `?` (the resolved `-q` /
/// `--show-control-chars` axis). Built once per listing from the options and
/// the attested console, then threaded through rendering so the arithmetic is
/// done in one place.
#[derive(Clone, Copy)]
struct Quoting {
    style: QuotingStyle,
    hide_control: bool,
}

impl Quoting {
    /// Resolve the GNU defaults against the attested console: `shell-escape`
    /// quoting with hidden control characters at a terminal, `literal` quoting
    /// with shown control characters otherwise. An explicit flag overrides
    /// either axis.
    fn resolve(options: Options, terminal: Option<usize>) -> Self {
        let at_terminal = terminal.is_some();
        Self {
            style: options.quoting.unwrap_or(if at_terminal {
                QuotingStyle::ShellEscape
            } else {
                QuotingStyle::Literal
            }),
            hide_control: options.hide_control_chars.unwrap_or(at_terminal),
        }
    }

    /// Render `name` in the resolved style.
    fn render(self, name: &str) -> String {
        match self.style {
            QuotingStyle::Literal => literal_name(name, self.hide_control),
            QuotingStyle::C => c_name(name),
            QuotingStyle::Escape => escape_name(name),
            QuotingStyle::Shell => shell_name(name, false, false, self.hide_control),
            QuotingStyle::ShellAlways => shell_name(name, false, true, self.hide_control),
            QuotingStyle::ShellEscape => shell_name(name, true, false, self.hide_control),
            QuotingStyle::ShellEscapeAlways => shell_name(name, true, true, self.hide_control),
        }
    }
}

/// Whether `ch` is a nongraphic (control) character: an ASCII control or
/// DEL. Valid multibyte UTF-8 characters are graphic and pass through, as
/// they do for GNU `ls` in a UTF-8 locale.
fn is_control(ch: char) -> bool {
    let c = ch as u32;
    c < 0x20 || c == 0x7f
}

/// The C named backslash escape letter for a control character, where one
/// exists (`\a \b \t \n \v \f \r`); otherwise `None`, and the caller uses
/// three-digit octal.
fn named_escape(ch: char) -> Option<char> {
    Some(match ch {
        '\u{07}' => 'a',
        '\u{08}' => 'b',
        '\t' => 't',
        '\n' => 'n',
        '\u{0b}' => 'v',
        '\u{0c}' => 'f',
        '\r' => 'r',
        _ => return None,
    })
}

/// Append the C backslash escape for the control character `ch`: its named
/// escape where one exists, else three-digit octal (`is_control` guarantees
/// a value below `0o200`, so three digits always suffice).
fn push_control_escape(out: &mut String, ch: char) {
    out.push('\\');
    match named_escape(ch) {
        Some(esc) => out.push(esc),
        None => {
            let _ = write!(out, "{:03o}", ch as u32);
        }
    }
}

/// `literal` quoting: the name verbatim, with nongraphic characters shown as
/// `?` when control characters are hidden (`-q`).
fn literal_name(name: &str, hide_control: bool) -> String {
    if !hide_control {
        return String::from(name);
    }
    name.chars()
        .map(|ch| if is_control(ch) { '?' } else { ch })
        .collect()
}

/// `c` quoting: the name as a C string literal — always double-quoted, with
/// `\"` and `\\` for the quote and backslash and C escapes (named or octal)
/// for control characters.
fn c_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 2);
    out.push('"');
    for ch in name.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            _ if is_control(ch) => push_control_escape(&mut out, ch),
            _ => out.push(ch),
        }
    }
    out.push('"');
    out
}

/// `escape` quoting: like [`c_name`] but without the surrounding quotes, so
/// spaces are escaped (`\ `) and the double quote needs no escaping.
fn escape_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for ch in name.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            ' ' => out.push_str("\\ "),
            _ if is_control(ch) => push_control_escape(&mut out, ch),
            _ => out.push(ch),
        }
    }
    out
}

/// Whether `ch` at position `index` in a name of `len` characters forces
/// shell quoting on its own — a shell metacharacter or a nongraphic
/// character. `#` and `~` are special only leading, and `{`/`}` only as the
/// whole name; the space and the single quote are handled by the caller.
fn is_shell_special(ch: char, index: usize, len: usize) -> bool {
    match ch {
        '!' | '"' | '$' | '&' | '(' | ')' | '*' | ';' | '<' | '=' | '>' | '?' | '[' | '\\'
        | '^' | '`' | '|' => true,
        '#' | '~' => index == 0,
        '{' | '}' => len == 1,
        _ => is_control(ch),
    }
}

/// The shell quoting styles. `escape` splices nongraphic characters as `$'…'`
/// ANSI-C escapes; otherwise they stay literal (or become `?` under `-q`).
/// `always` quotes even a name that needs no quoting.
///
/// A name whose only awkward characters are single quotes (with otherwise
/// safe characters or spaces) takes the more concise C double-quoted form
/// (`"it's"`); every other quoted name takes the single-quoted form, with a
/// literal single quote written `'\''`.
fn shell_name(name: &str, escape: bool, always: bool, hide_control: bool) -> String {
    // The non-escaping shell styles show control characters as `?` under
    // `-q`; the escaping styles render them with `$'…'`, so `-q` is moot.
    let mapped: String = if !escape && hide_control {
        name.chars()
            .map(|ch| if is_control(ch) { '?' } else { ch })
            .collect()
    } else {
        String::from(name)
    };

    let chars: Vec<char> = mapped.chars().collect();
    let len = chars.len();
    let mut needs_quote = always || len == 0;
    let mut has_single_quote = false;
    // Any character other than a space or single quote that forces quoting;
    // such a character rules out the concise double-quote form.
    let mut hard = false;
    for (index, &ch) in chars.iter().enumerate() {
        if ch == '\'' {
            has_single_quote = true;
            needs_quote = true;
        } else if ch == ' ' {
            needs_quote = true;
        } else if is_shell_special(ch, index, len) {
            needs_quote = true;
            hard = true;
        }
    }

    if !needs_quote {
        return mapped;
    }

    if has_single_quote && !hard {
        // Nothing inside needs escaping: `hard` being false guarantees no
        // `"`, `$`, backtick, backslash, or control character.
        return format!("\"{mapped}\"");
    }

    let mut out = String::with_capacity(mapped.len() + 2);
    out.push('\'');
    let mut open = true;
    for &ch in &chars {
        if ch == '\'' {
            if open {
                out.push('\'');
                open = false;
            }
            out.push_str("\\'");
        } else if escape && is_control(ch) {
            if open {
                out.push('\'');
                open = false;
            }
            out.push_str("$'");
            push_control_escape(&mut out, ch);
            out.push('\'');
        } else {
            if !open {
                out.push('\'');
                open = true;
            }
            out.push(ch);
        }
    }
    if open {
        out.push('\'');
    }
    out
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

/// Natural "version" ordering for the `-v` / `--sort=version` sort.
///
/// This is a faithful port of GNU gnulib's `filevercmp` (the ordering GNU
/// `ls -v` and `sort -V` use), so a run of digits compares numerically —
/// `f2` before `f10` — while surrounding text compares by the Debian
/// version algorithm, with file suffixes (`.c`, `.tar.gz`) cut off for a
/// first pass and restored for a tie-break. Operating on bytes in the C
/// locale (ASCII classification) matches the reference exactly; a `~`
/// sorts before everything, including the empty string.
///
/// The comparator is total and never panics: it only indexes within the
/// slices it is given.
mod version {
    use core::cmp::Ordering;

    /// The length of the longest suffix of `s` matching the GNU C-locale
    /// regex `(\.[A-Za-z~][A-Za-z0-9~]*)*$`, but never all of a non-empty
    /// `s`. This is the run of file extensions cut before the first pass.
    fn file_prefixlen(s: &[u8]) -> usize {
        let n = s.len();
        let mut prefixlen = 0;
        let mut i = 0;
        loop {
            if i == n {
                return prefixlen;
            }
            i += 1;
            prefixlen = i;
            while i + 1 < n && s[i] == b'.' && (s[i + 1].is_ascii_alphabetic() || s[i + 1] == b'~')
            {
                i += 2;
                while i < n && (s[i].is_ascii_alphanumeric() || s[i] == b'~') {
                    i += 1;
                }
            }
        }
    }

    /// The version-sort weight of `s`'s byte at `pos` (of length `len`): the
    /// empty position sorts before every non-`~` byte, digits share a rank,
    /// letters keep their byte value, `~` sorts before everything, and any
    /// other byte sorts after all letters.
    fn order(s: &[u8], pos: usize, len: usize) -> i32 {
        if pos == len {
            return -1;
        }
        let c = s[pos];
        if c.is_ascii_digit() {
            0
        } else if c.is_ascii_alphabetic() {
            i32::from(c)
        } else if c == b'~' {
            -2
        } else {
            // Non-alphanumeric, non-`~`: after every letter. `UCHAR_MAX` is
            // 255, so this is `c + 256`, matching the reference weight.
            i32::from(c) + 256
        }
    }

    /// The Debian version comparison over the byte ranges `s1[..s1_len]` and
    /// `s2[..s2_len]` (a port of gnulib's `verrevcmp`): non-digit runs
    /// compare by [`order`], digit runs compare numerically ignoring leading
    /// zeros.
    fn verrevcmp(s1: &[u8], s1_len: usize, s2: &[u8], s2_len: usize) -> i32 {
        let (mut p1, mut p2) = (0usize, 0usize);
        while p1 < s1_len || p2 < s2_len {
            let mut first_diff = 0i32;
            while (p1 < s1_len && !s1[p1].is_ascii_digit())
                || (p2 < s2_len && !s2[p2].is_ascii_digit())
            {
                let c1 = order(s1, p1, s1_len);
                let c2 = order(s2, p2, s2_len);
                if c1 != c2 {
                    return c1 - c2;
                }
                p1 += 1;
                p2 += 1;
            }
            while p1 < s1_len && s1[p1] == b'0' {
                p1 += 1;
            }
            while p2 < s2_len && s2[p2] == b'0' {
                p2 += 1;
            }
            while p1 < s1_len && p2 < s2_len && s1[p1].is_ascii_digit() && s2[p2].is_ascii_digit() {
                if first_diff == 0 {
                    first_diff = i32::from(s1[p1]) - i32::from(s2[p2]);
                }
                p1 += 1;
                p2 += 1;
            }
            if p1 < s1_len && s1[p1].is_ascii_digit() {
                return 1;
            }
            if p2 < s2_len && s2[p2].is_ascii_digit() {
                return -1;
            }
            if first_diff != 0 {
                return first_diff;
            }
        }
        0
    }

    /// Compare version strings `a` and `b` (byte slices), returning their
    /// [`Ordering`]. A faithful port of gnulib `filenvercmp`.
    pub fn filevercmp(a: &[u8], b: &[u8]) -> Ordering {
        raw(a, b).cmp(&0)
    }

    /// The signed comparison value, so the port reads like the reference.
    fn raw(a: &[u8], b: &[u8]) -> i32 {
        // Empty versions sort first.
        if a.is_empty() {
            return if b.is_empty() { 0 } else { -1 };
        }
        if b.is_empty() {
            return 1;
        }
        // Leading ".": "." sorts first, then "..", then other dot-names,
        // then the rest.
        if a[0] == b'.' {
            if b[0] != b'.' {
                return -1;
            }
            let adot = a.len() == 1;
            let bdot = b.len() == 1;
            if adot {
                return if bdot { 0 } else { -1 };
            }
            if bdot {
                return 1;
            }
            let adotdot = a[1] == b'.' && a.len() == 2;
            let bdotdot = b[1] == b'.' && b.len() == 2;
            if adotdot {
                return if bdotdot { 0 } else { -1 };
            }
            if bdotdot {
                return 1;
            }
        } else if b[0] == b'.' {
            return 1;
        }
        // Cut file suffixes for the first pass; restore them on a tie.
        let apre = file_prefixlen(a);
        let bpre = file_prefixlen(b);
        let one_pass_only = apre == a.len() && bpre == b.len();
        let result = verrevcmp(a, apre, b, bpre);
        if result != 0 || one_pass_only {
            result
        } else {
            verrevcmp(a, a.len(), b, b.len())
        }
    }

    #[cfg(test)]
    mod tests {
        use super::filevercmp;
        use core::cmp::Ordering;

        fn cmp(a: &str, b: &str) -> Ordering {
            filevercmp(a.as_bytes(), b.as_bytes())
        }

        #[test]
        fn digit_runs_compare_numerically() {
            assert_eq!(cmp("f2", "f10"), Ordering::Less);
            assert_eq!(cmp("f10", "f2"), Ordering::Greater);
            assert_eq!(cmp("file", "file"), Ordering::Equal);
        }

        #[test]
        fn leading_zeros_are_ignored_in_a_numeric_run() {
            assert_eq!(cmp("f008", "f8"), Ordering::Equal);
            assert_eq!(cmp("f09", "f10"), Ordering::Less);
        }

        #[test]
        fn tilde_sorts_before_everything() {
            assert_eq!(cmp("f~", "f"), Ordering::Less);
            assert_eq!(cmp("1.0~beta", "1.0"), Ordering::Less);
        }

        #[test]
        fn dot_names_sort_first_in_order() {
            assert_eq!(cmp(".", ".."), Ordering::Less);
            assert_eq!(cmp("..", ".bashrc"), Ordering::Less);
            assert_eq!(cmp(".bashrc", "bashrc"), Ordering::Less);
        }

        #[test]
        fn suffixes_are_cut_then_restored_for_a_tie() {
            // Same base, different extension: the suffix breaks the tie.
            assert_eq!(cmp("hello.c", "hello.o"), Ordering::Less);
            // Different version in the base wins over the suffix.
            assert_eq!(cmp("hello-2.c", "hello-10.c"), Ordering::Less);
        }

        #[test]
        fn the_empty_string_sorts_first() {
            assert_eq!(cmp("", "a"), Ordering::Less);
            assert_eq!(cmp("a", ""), Ordering::Greater);
            assert_eq!(cmp("", ""), Ordering::Equal);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{run, USAGE};
    use crate::command::{
        Command, Filters, Format, Hidden, Indicator, Options, QuotingStyle, Sort, TimeField,
        TimeStyle,
    };
    use crate::error::LsError;
    use crate::io::{Entry, Listing, Metadata, Output};
    use alloc::string::{String, ToString};
    use alloc::vec::Vec;
    use core::cell::RefCell;
    use tairix_abi::fs::FileKind;
    use tairix_abi::time::Time64;
    use tairix_abi::{Errno, NodeTimes};
    use tairix_glob::Pattern;
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
                    inode: 0,
                    times: NodeTimes::default(),
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
                    inode: 0,
                    times: NodeTimes::default(),
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
                    inode: 0,
                    times: NodeTimes::default(),
                },
            ));
            self
        }

        /// Declare a regular-file entry with an explicit node number and
        /// timestamps, for the `-i`, `-t`, and long-format date tests.
        fn timed_entry(mut self, dir: &str, name: &str, inode: u64, times: NodeTimes) -> Self {
            let children = self
                .dirs
                .iter_mut()
                .find(|(d, _)| d == dir)
                .map(|(_, c)| c)
                .expect("directory must be declared before its entries");
            children.push(Entry {
                name: name.to_string(),
                kind: FileKind::Regular,
            });
            self.stat.push((
                super::join(dir, name),
                Metadata {
                    kind: FileKind::Regular,
                    mode: 0o644,
                    size: 0,
                    allocated: 0,
                    uid: UID,
                    gid: GID,
                    inode,
                    times,
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
                inode: 0,
                times: NodeTimes::default(),
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
        /// The width [`Output::terminal_width`] reports; `None` models a
        /// non-terminal stdout (a pipe/file), which is the default the
        /// non-column tests rely on.
        width: Option<usize>,
    }

    impl Recorder {
        fn new() -> Self {
            Self {
                chunks: RefCell::new(Vec::new()),
                records: RefCell::new(Vec::new()),
                fail: false,
                width: None,
            }
        }

        /// A recorder that reports an attested terminal of `cols` columns,
        /// so the listing takes the GNU multi-column default and wraps to
        /// that width.
        fn terminal(cols: usize) -> Self {
            Self {
                chunks: RefCell::new(Vec::new()),
                records: RefCell::new(Vec::new()),
                fail: false,
                width: Some(cols),
            }
        }

        fn failing() -> Self {
            Self {
                chunks: RefCell::new(Vec::new()),
                records: RefCell::new(Vec::new()),
                fail: true,
                width: None,
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

        fn terminal_width(&self) -> Option<usize> {
            self.width
        }
    }

    /// The owning ids every fixture row carries, so long-format
    /// expectations are explicit about the owner and group columns.
    const UID: u32 = 1000;
    const GID: u32 = 100;

    /// A fixed "now" for the date tests: 2024-06-15T12:00:00Z. Stamps are
    /// chosen relative to it so the recent/old split is deterministic.
    const NOW: Time64 = Time64::from_secs(1_718_452_800);

    /// A [`NodeTimes`] whose four stamps are all `secs` seconds since the
    /// epoch — the common case where a test cares only about one instant.
    fn stamps(secs: i64) -> NodeTimes {
        let t = Time64::from_secs(secs);
        NodeTimes {
            created: t,
            modified: t,
            accessed: t,
            changed: t,
        }
    }

    fn list_with(options: Options, paths: &[&str]) -> Command {
        listing_with(options, Filters::default(), paths)
    }

    fn listing_with(options: Options, filters: Filters, paths: &[&str]) -> Command {
        Command::List {
            options,
            filters,
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
        run(command, None, NOW, fs, &NoHelp, out)
    }

    #[test]
    fn help_renders_the_short_help_from_the_document() {
        let fs = TreeFs::new();
        let out = Recorder::new();
        assert_eq!(run(Command::Help, None, NOW, &fs, &OneDoc, &out), Ok(()));
        let text = out.text();
        assert!(text.contains("ls — list directory contents"), "{text}");
        assert!(text.contains("ls [-a] [-l] [--] [path...]"), "{text}");
    }

    #[test]
    fn help_falls_back_to_the_usage_banner_without_a_tree() {
        let fs = TreeFs::new();
        let out = Recorder::new();
        assert_eq!(run(Command::Help, None, NOW, &fs, &NoHelp, &out), Ok(()));
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
    fn ignore_backups_hides_tilde_names_and_emits_no_record() {
        let fs = TreeFs::new()
            .dir(".")
            .entry(".", "a", FileKind::Regular, 0o644, 0)
            .entry(".", "b~", FileKind::Regular, 0o644, 0)
            .entry(".", "c", FileKind::Regular, 0o644, 0);
        let out = Recorder::new();
        let filters = Filters {
            ignore_backups: true,
            ..Filters::default()
        };
        assert_eq!(
            run_ls(listing_with(Options::DEFAULT, filters, &["."]), &fs, &out),
            Ok(())
        );
        assert_eq!(out.text(), "a\nc\n");
        // An explicitly requested filter is not advertised as an omission.
        assert!(out.records().is_empty());
    }

    #[test]
    fn ignore_backups_hides_backups_even_among_hidden_entries() {
        let fs = TreeFs::new()
            .dir(".")
            .entry(".", "a", FileKind::Regular, 0o644, 0)
            .entry(".", "b~", FileKind::Regular, 0o644, 0)
            .entry(".", ".x~", FileKind::Regular, 0o644, 0)
            .entry(".", ".y", FileKind::Regular, 0o644, 0);
        let out = Recorder::new();
        let options = Options {
            hidden: Hidden::All,
            ..Options::DEFAULT
        };
        let filters = Filters {
            ignore_backups: true,
            ..Filters::default()
        };
        assert_eq!(
            run_ls(listing_with(options, filters, &["."]), &fs, &out),
            Ok(())
        );
        // `-a` shows `.y`, but both backups (`b~` and the hidden `.x~`) are
        // gone: `-B` applies in every dotfile mode.
        assert_eq!(out.text(), ".y\na\n");
    }

    #[test]
    fn ignore_pattern_suppresses_matches_in_every_mode() {
        let fs = TreeFs::new()
            .dir(".")
            .entry(".", ".hidden", FileKind::Regular, 0o644, 0)
            .entry(".", "keep.txt", FileKind::Regular, 0o644, 0)
            .entry(".", "drop.o", FileKind::Regular, 0o644, 0)
            .entry(".", "also.o", FileKind::Regular, 0o644, 0);
        let options = Options {
            hidden: Hidden::All,
            ..Options::DEFAULT
        };
        let filters = Filters {
            ignore: alloc::vec![Pattern::new("*.o").expect("valid glob")],
            ..Filters::default()
        };
        let out = Recorder::new();
        assert_eq!(
            run_ls(listing_with(options, filters, &["."]), &fs, &out),
            Ok(())
        );
        // `-I` applies under `-a` too: the object files are gone, the
        // dotfile stays.
        assert_eq!(out.text(), ".hidden\nkeep.txt\n");
    }

    #[test]
    fn hide_pattern_yields_to_show_hidden() {
        let fs = TreeFs::new()
            .dir(".")
            .entry(".", "keep", FileKind::Regular, 0o644, 0)
            .entry(".", "skip.tmp", FileKind::Regular, 0o644, 0);
        let filters = Filters {
            hide: alloc::vec![Pattern::new("*.tmp").expect("valid glob")],
            ..Filters::default()
        };
        // Without `-a`, `--hide` suppresses the match.
        let out = Recorder::new();
        assert_eq!(
            run_ls(
                listing_with(Options::DEFAULT, filters.clone(), &["."]),
                &fs,
                &out
            ),
            Ok(())
        );
        assert_eq!(out.text(), "keep\n");

        // With `-a`, `--hide` has no effect (the GNU rule).
        let out = Recorder::new();
        let options = Options {
            hidden: Hidden::All,
            ..Options::DEFAULT
        };
        assert_eq!(
            run_ls(listing_with(options, filters, &["."]), &fs, &out),
            Ok(())
        );
        assert_eq!(out.text(), "keep\nskip.tmp\n");
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
            "total 5\ndrwxr-xr-x 1000 100 4096 Jan  1  1970 d\n\
             -rw-r--r-- 1000 100    7 Jan  1  1970 f\n"
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
        assert_eq!(
            out.text(),
            "total 1\n-rw------- 1000 100 3 Jan  1  1970 f\n"
        );
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
    fn no_sort_keeps_directory_order() {
        // `-U` lists entries in the order the directory stream returned them.
        let fs = TreeFs::new()
            .dir(".")
            .entry(".", "c", FileKind::Regular, 0o644, 0)
            .entry(".", "a", FileKind::Regular, 0o644, 0)
            .entry(".", "b", FileKind::Regular, 0o644, 0);
        let out = Recorder::new();
        assert_eq!(
            run_ls(
                list_with(
                    Options {
                        sort: Sort::None,
                        ..Options::DEFAULT
                    },
                    &["."],
                ),
                &fs,
                &out,
            ),
            Ok(())
        );
        assert_eq!(out.text(), "c\na\nb\n");
    }

    #[test]
    fn extension_sort_orders_by_suffix_then_name() {
        // No extension sorts first (empty suffix), then by suffix, ties by
        // name — the GNU `-X` order.
        let fs = TreeFs::new()
            .dir(".")
            .entry(".", "b.txt", FileKind::Regular, 0o644, 0)
            .entry(".", "a.txt", FileKind::Regular, 0o644, 0)
            .entry(".", "c.c", FileKind::Regular, 0o644, 0)
            .entry(".", "readme", FileKind::Regular, 0o644, 0);
        let out = Recorder::new();
        assert_eq!(
            run_ls(
                list_with(
                    Options {
                        sort: Sort::Extension,
                        ..Options::DEFAULT
                    },
                    &["."],
                ),
                &fs,
                &out,
            ),
            Ok(())
        );
        assert_eq!(out.text(), "readme\nc.c\na.txt\nb.txt\n");
    }

    #[test]
    fn version_sort_orders_numerically() {
        // `-v` compares digit runs numerically, so `f2` precedes `f10`.
        let fs = TreeFs::new()
            .dir(".")
            .entry(".", "f10", FileKind::Regular, 0o644, 0)
            .entry(".", "f2", FileKind::Regular, 0o644, 0)
            .entry(".", "f1", FileKind::Regular, 0o644, 0);
        let out = Recorder::new();
        assert_eq!(
            run_ls(
                list_with(
                    Options {
                        sort: Sort::Version,
                        ..Options::DEFAULT
                    },
                    &["."],
                ),
                &fs,
                &out,
            ),
            Ok(())
        );
        assert_eq!(out.text(), "f1\nf2\nf10\n");
    }

    #[test]
    fn group_directories_first_floats_directories() {
        let fs = TreeFs::new()
            .dir(".")
            .entry(".", "afile", FileKind::Regular, 0o644, 0)
            .entry(".", "zdir", FileKind::Directory, 0o755, 0)
            .entry(".", "bfile", FileKind::Regular, 0o644, 0);
        let out = Recorder::new();
        assert_eq!(
            run_ls(
                list_with(
                    Options {
                        group_directories_first: true,
                        ..Options::DEFAULT
                    },
                    &["."],
                ),
                &fs,
                &out,
            ),
            Ok(())
        );
        // Directories first, then the name sort within each group.
        assert_eq!(out.text(), "zdir\nafile\nbfile\n");
    }

    #[test]
    fn group_directories_first_keeps_directories_first_under_reverse() {
        let fs = TreeFs::new()
            .dir(".")
            .entry(".", "adir", FileKind::Directory, 0o755, 0)
            .entry(".", "zdir", FileKind::Directory, 0o755, 0)
            .entry(".", "afile", FileKind::Regular, 0o644, 0)
            .entry(".", "zfile", FileKind::Regular, 0o644, 0);
        let out = Recorder::new();
        assert_eq!(
            run_ls(
                list_with(
                    Options {
                        group_directories_first: true,
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
        // `-r` reverses within each group, but directories stay first.
        assert_eq!(out.text(), "zdir\nadir\nzfile\nafile\n");
    }

    #[test]
    fn f_shows_all_entries_in_directory_order() {
        let fs = TreeFs::new()
            .dir(".")
            .entry(".", ".hidden", FileKind::Regular, 0o644, 0)
            .entry(".", "b", FileKind::Regular, 0o644, 0)
            .entry(".", "a", FileKind::Regular, 0o644, 0);
        let out = Recorder::new();
        assert_eq!(
            run_ls(
                list_with(
                    Options {
                        hidden: Hidden::All,
                        sort: Sort::None,
                        ..Options::DEFAULT
                    },
                    &["."],
                ),
                &fs,
                &out,
            ),
            Ok(())
        );
        assert_eq!(out.text(), ".hidden\nb\na\n");
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
                        format: Some(Format::Commas),
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
    fn a_terminal_defaults_to_vertical_columns() {
        // The GNU default on a terminal: multiple columns filled
        // top-to-bottom, sized to the attested width.
        let fs = TreeFs::new()
            .dir(".")
            .entry(".", "a", FileKind::Regular, 0o644, 0)
            .entry(".", "b", FileKind::Regular, 0o644, 0)
            .entry(".", "c", FileKind::Regular, 0o644, 0)
            .entry(".", "d", FileKind::Regular, 0o644, 0)
            .entry(".", "e", FileKind::Regular, 0o644, 0);
        let out = Recorder::terminal(6);
        assert_eq!(
            run_ls(list_with(Options::DEFAULT, &["."]), &fs, &out),
            Ok(())
        );
        assert_eq!(out.text(), "a  d\nb  e\nc\n");
    }

    #[test]
    fn a_wide_terminal_lays_a_small_listing_on_one_row() {
        let fs = TreeFs::new()
            .dir(".")
            .entry(".", "a", FileKind::Regular, 0o644, 0)
            .entry(".", "b", FileKind::Regular, 0o644, 0)
            .entry(".", "c", FileKind::Regular, 0o644, 0);
        let out = Recorder::terminal(80);
        assert_eq!(
            run_ls(list_with(Options::DEFAULT, &["."]), &fs, &out),
            Ok(())
        );
        assert_eq!(out.text(), "a  b  c\n");
    }

    #[test]
    fn columns_align_to_the_widest_entry_in_each_column() {
        // Vertical fill: column 0 holds `alpha`/`c`, column 1 holds
        // `bb`/`dd`, each column padded to its own widest entry.
        let fs = TreeFs::new()
            .dir(".")
            .entry(".", "alpha", FileKind::Regular, 0o644, 0)
            .entry(".", "bb", FileKind::Regular, 0o644, 0)
            .entry(".", "c", FileKind::Regular, 0o644, 0)
            .entry(".", "dd", FileKind::Regular, 0o644, 0);
        let out = Recorder::terminal(12);
        assert_eq!(
            run_ls(
                list_with(
                    Options {
                        format: Some(Format::Columns),
                        ..Options::DEFAULT
                    },
                    &["."],
                ),
                &fs,
                &out,
            ),
            Ok(())
        );
        // rows = 2, cols = 2; column 0 width 5 (`alpha`), column 1 the last
        // on each line and so unpadded.
        assert_eq!(out.text(), "alpha  c\nbb     dd\n");
    }

    #[test]
    fn across_fills_left_to_right() {
        let fs = TreeFs::new()
            .dir(".")
            .entry(".", "a", FileKind::Regular, 0o644, 0)
            .entry(".", "b", FileKind::Regular, 0o644, 0)
            .entry(".", "c", FileKind::Regular, 0o644, 0)
            .entry(".", "d", FileKind::Regular, 0o644, 0)
            .entry(".", "e", FileKind::Regular, 0o644, 0);
        let out = Recorder::new();
        assert_eq!(
            run_ls(
                list_with(
                    Options {
                        format: Some(Format::Across),
                        width: Some(6),
                        ..Options::DEFAULT
                    },
                    &["."],
                ),
                &fs,
                &out,
            ),
            Ok(())
        );
        assert_eq!(out.text(), "a  b\nc  d\ne\n");
    }

    #[test]
    fn explicit_one_per_line_overrides_the_terminal_default() {
        let fs = TreeFs::new()
            .dir(".")
            .entry(".", "a", FileKind::Regular, 0o644, 0)
            .entry(".", "b", FileKind::Regular, 0o644, 0);
        let out = Recorder::terminal(80);
        assert_eq!(
            run_ls(
                list_with(
                    Options {
                        format: Some(Format::OnePerLine),
                        ..Options::DEFAULT
                    },
                    &["."],
                ),
                &fs,
                &out,
            ),
            Ok(())
        );
        assert_eq!(out.text(), "a\nb\n");
    }

    #[test]
    fn a_narrow_width_falls_back_to_a_single_column() {
        // No column pair fits, so the grid degrades to one entry per line —
        // never a crash or an overlong line.
        let fs = TreeFs::new()
            .dir(".")
            .entry(".", "longname", FileKind::Regular, 0o644, 0)
            .entry(".", "another", FileKind::Regular, 0o644, 0);
        let out = Recorder::new();
        assert_eq!(
            run_ls(
                list_with(
                    Options {
                        format: Some(Format::Columns),
                        width: Some(3),
                        ..Options::DEFAULT
                    },
                    &["."],
                ),
                &fs,
                &out,
            ),
            Ok(())
        );
        assert_eq!(out.text(), "another\nlongname\n");
    }

    #[test]
    fn commas_wrap_to_the_width() {
        let fs = TreeFs::new()
            .dir(".")
            .entry(".", "a", FileKind::Regular, 0o644, 0)
            .entry(".", "b", FileKind::Regular, 0o644, 0)
            .entry(".", "c", FileKind::Regular, 0o644, 0)
            .entry(".", "d", FileKind::Regular, 0o644, 0);
        let out = Recorder::new();
        assert_eq!(
            run_ls(
                list_with(
                    Options {
                        format: Some(Format::Commas),
                        width: Some(5),
                        ..Options::DEFAULT
                    },
                    &["."],
                ),
                &fs,
                &out,
            ),
            Ok(())
        );
        assert_eq!(out.text(), "a, b,\nc, d\n");
    }

    #[test]
    fn an_unlimited_width_lays_columns_on_one_row() {
        // `-w 0` is the GNU "unlimited line length": one row across.
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
                        format: Some(Format::Columns),
                        width: Some(0),
                        ..Options::DEFAULT
                    },
                    &["."],
                ),
                &fs,
                &out,
            ),
            Ok(())
        );
        assert_eq!(out.text(), "a  b  c\n");
    }

    /// Render the single name `name` under `options` and return the one
    /// produced line (its trailing newline stripped). The verbatim expected
    /// strings below were pinned against GNU coreutils `ls`.
    fn quoted(name: &str, options: Options) -> String {
        let fs = TreeFs::new()
            .dir(".")
            .entry(".", name, FileKind::Regular, 0o644, 0);
        let out = Recorder::new();
        run_ls(list_with(options, &["."]), &fs, &out).expect("listing succeeds");
        let text = out.text();
        String::from(text.strip_suffix('\n').unwrap_or(&text))
    }

    /// Options selecting one concrete quoting style with control characters
    /// shown, so a test pins exactly the style under test.
    fn styled(style: QuotingStyle) -> Options {
        Options {
            quoting: Some(style),
            hide_control_chars: Some(false),
            ..Options::DEFAULT
        }
    }

    #[test]
    fn c_style_quotes_like_a_c_string_literal() {
        let opts = styled(QuotingStyle::C);
        assert_eq!(quoted("a\"b", opts), "\"a\\\"b\"");
        assert_eq!(quoted("a\\b", opts), "\"a\\\\b\"");
        assert_eq!(quoted("a\tb", opts), "\"a\\tb\"");
        assert_eq!(quoted("a\u{1b}b", opts), "\"a\\033b\"");
        assert_eq!(quoted("a\u{7f}b", opts), "\"a\\177b\"");
        // `$`, `#`, `'`, `*`, and space stay literal inside the quotes.
        assert_eq!(quoted("a$ '*b", opts), "\"a$ '*b\"");
    }

    #[test]
    fn escape_style_drops_the_quotes_and_escapes_spaces() {
        let opts = styled(QuotingStyle::Escape);
        assert_eq!(quoted("a b", opts), "a\\ b");
        assert_eq!(quoted("a\\b", opts), "a\\\\b");
        assert_eq!(quoted("a\tb", opts), "a\\tb");
        // Unlike C style, the double quote is left literal.
        assert_eq!(quoted("a\"b", opts), "a\"b");
        // Shell metacharacters other than space and backslash are literal.
        assert_eq!(quoted("a&;|<(b", opts), "a&;|<(b");
    }

    #[test]
    fn literal_style_prints_the_name_verbatim() {
        assert_eq!(quoted("a b\"$*", styled(QuotingStyle::Literal)), "a b\"$*");
        // With control characters hidden, nongraphic bytes become `?`.
        let hide = Options {
            quoting: Some(QuotingStyle::Literal),
            hide_control_chars: Some(true),
            ..Options::DEFAULT
        };
        assert_eq!(quoted("a\tb", hide), "a?b");
    }

    #[test]
    fn shell_style_quotes_only_when_necessary() {
        let opts = styled(QuotingStyle::Shell);
        // A plain name, and a name whose only special characters are safe,
        // are left unquoted.
        assert_eq!(quoted("plain", opts), "plain");
        assert_eq!(quoted("a@%+,-.:]_b", opts), "a@%+,-.:]_b");
        assert_eq!(quoted("mid#x", opts), "mid#x");
        // Shell metacharacters and spaces force single quotes.
        assert_eq!(quoted("a b", opts), "'a b'");
        assert_eq!(quoted("a$b", opts), "'a$b'");
        assert_eq!(quoted("a*b", opts), "'a*b'");
        assert_eq!(quoted("#lead", opts), "'#lead'");
        // A lone single quote (nothing else hard) takes the concise
        // double-quoted form; a single quote alongside a hard character
        // takes the single-quoted `'\''` form.
        assert_eq!(quoted("it's", opts), "\"it's\"");
        assert_eq!(quoted("a' b", opts), "\"a' b\"");
        assert_eq!(quoted("a'&b", opts), "'a'\\''&b'");
        // A control character stays literal inside the single quotes.
        assert_eq!(quoted("a\tb", opts), "'a\tb'");
    }

    #[test]
    fn shell_always_style_quotes_even_plain_names() {
        let opts = styled(QuotingStyle::ShellAlways);
        assert_eq!(quoted("plain", opts), "'plain'");
        assert_eq!(quoted("it's", opts), "\"it's\"");
    }

    #[test]
    fn shell_escape_style_splices_control_characters() {
        let opts = styled(QuotingStyle::ShellEscape);
        assert_eq!(quoted("plain", opts), "plain");
        assert_eq!(quoted("a b", opts), "'a b'");
        // Nongraphic characters are spliced in as `$'…'`, leaving the
        // surrounding single-quoted runs.
        assert_eq!(quoted("a\tb", opts), "'a'$'\\t''b'");
        assert_eq!(quoted("\tlead", opts), "''$'\\t''lead'");
        assert_eq!(quoted("trail\t", opts), "'trail'$'\\t'");
        assert_eq!(quoted("a\u{1b}b", opts), "'a'$'\\033''b'");
    }

    #[test]
    fn quoting_defaults_follow_the_attested_console() {
        // At a terminal the default is shell-escape, so a name with a space
        // is quoted.
        let fs = TreeFs::new()
            .dir(".")
            .entry(".", "a b", FileKind::Regular, 0o644, 0);
        let out = Recorder::terminal(80);
        run_ls(list_with(Options::DEFAULT, &["."]), &fs, &out).expect("listing succeeds");
        assert_eq!(out.text(), "'a b'\n");
        // Piped (no attested terminal) the default is literal.
        let piped = Recorder::new();
        run_ls(list_with(Options::DEFAULT, &["."]), &fs, &piped).expect("listing succeeds");
        assert_eq!(piped.text(), "a b\n");
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
             -rw-r--r-- 1000 100  500 Jan  1  1970 a\n\
             -rw-r--r-- 1000 100 1.1K Jan  1  1970 b\n\
             -rw-r--r-- 1000 100  10M Jan  1  1970 c\n"
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
        assert_eq!(
            hidden_owner.text(),
            "total 1\n-rw-r--r-- 100 7 Jan  1  1970 f\n"
        );
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
        assert_eq!(
            hidden_group.text(),
            "total 1\n-rw-r--r-- 1000 7 Jan  1  1970 f\n"
        );
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
            "total 5\n4 drwxr-xr-x 1000 100 4096 Jan  1  1970 d\n\
             1 -rw-r--r-- 1000 100    7 Jan  1  1970 f\n"
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
                        format: Some(Format::Commas),
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

    // Stamps used across the time tests, relative to `NOW` (2024-06-15):
    // `RECENT` is within six months (renders a time-of-day), `OLD` is not
    // (renders a year).
    const RECENT: i64 = 1_715_000_000; // 2024-05-06 12:53:20 UTC
    const OLD: i64 = 1_700_000_000; // 2023-11-14 22:13:20 UTC
    const MID: i64 = 1_709_214_367; // 2024-02-29 13:46:07 UTC

    #[test]
    fn inode_column_prefixes_names_right_aligned() {
        let fs = TreeFs::new()
            .dir(".")
            .timed_entry(".", "a", 42, stamps(0))
            .timed_entry(".", "b", 7, stamps(0));
        let out = Recorder::new();
        assert_eq!(
            run_ls(
                list_with(
                    Options {
                        inode: true,
                        format: Some(Format::OnePerLine),
                        ..Options::DEFAULT
                    },
                    &["."],
                ),
                &fs,
                &out,
            ),
            Ok(())
        );
        // The inode column is right-aligned to the widest number (`42`).
        assert_eq!(out.text(), "42 a\n 7 b\n");
    }

    #[test]
    fn inode_prefixes_the_long_format_before_the_mode() {
        let fs = TreeFs::new()
            .dir(".")
            .timed_entry(".", "f", 1234, stamps(RECENT));
        let out = Recorder::new();
        assert_eq!(
            run_ls(
                list_with(
                    Options {
                        long: true,
                        inode: true,
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
            "total 0\n1234 -rw-r--r-- 1000 100 0 May  6 12:53 f\n"
        );
    }

    #[test]
    fn time_sort_orders_newest_first_ties_by_name() {
        let fs = TreeFs::new()
            .dir(".")
            .timed_entry(".", "old", 1, stamps(OLD))
            .timed_entry(".", "new", 2, stamps(RECENT))
            .timed_entry(".", "mid", 3, stamps(MID));
        let out = Recorder::new();
        assert_eq!(
            run_ls(
                list_with(
                    Options {
                        sort: Sort::Time,
                        format: Some(Format::OnePerLine),
                        ..Options::DEFAULT
                    },
                    &["."],
                ),
                &fs,
                &out,
            ),
            Ok(())
        );
        assert_eq!(out.text(), "new\nmid\nold\n");
    }

    #[test]
    fn reverse_time_sort_orders_oldest_first() {
        let fs = TreeFs::new()
            .dir(".")
            .timed_entry(".", "old", 1, stamps(OLD))
            .timed_entry(".", "new", 2, stamps(RECENT));
        let out = Recorder::new();
        assert_eq!(
            run_ls(
                list_with(
                    Options {
                        sort: Sort::Time,
                        reverse: true,
                        format: Some(Format::OnePerLine),
                        ..Options::DEFAULT
                    },
                    &["."],
                ),
                &fs,
                &out,
            ),
            Ok(())
        );
        assert_eq!(out.text(), "old\nnew\n");
    }

    #[test]
    fn the_long_format_shows_the_selected_timestamp() {
        // Distinct stamps per field so the column proves which one is shown.
        let times = NodeTimes {
            created: Time64::from_secs(0),
            modified: Time64::from_secs(OLD),
            accessed: Time64::from_secs(RECENT),
            changed: Time64::from_secs(MID),
        };
        let fs = TreeFs::new().dir(".").timed_entry(".", "f", 0, times);
        let long = |field| {
            let out = Recorder::new();
            assert_eq!(
                run_ls(
                    list_with(
                        Options {
                            long: true,
                            time_field: field,
                            ..Options::DEFAULT
                        },
                        &["."],
                    ),
                    &fs,
                    &out,
                ),
                Ok(())
            );
            out.text()
        };
        // Modified (the default) is old → a year; accessed and changed are
        // recent → a time-of-day.
        assert_eq!(
            long(TimeField::Modified),
            "total 0\n-rw-r--r-- 1000 100 0 Nov 14  2023 f\n"
        );
        assert_eq!(
            long(TimeField::Accessed),
            "total 0\n-rw-r--r-- 1000 100 0 May  6 12:53 f\n"
        );
        assert_eq!(
            long(TimeField::Changed),
            "total 0\n-rw-r--r-- 1000 100 0 Feb 29 13:46 f\n"
        );
    }

    #[test]
    fn time_styles_render_the_recent_stamp_each_way() {
        let fs = TreeFs::new()
            .dir(".")
            .timed_entry(".", "f", 0, stamps(RECENT));
        let styled = |style| {
            let out = Recorder::new();
            assert_eq!(
                run_ls(
                    list_with(
                        Options {
                            long: true,
                            time_style: style,
                            ..Options::DEFAULT
                        },
                        &["."],
                    ),
                    &fs,
                    &out,
                ),
                Ok(())
            );
            out.text()
        };
        assert_eq!(
            styled(TimeStyle::Locale),
            "total 0\n-rw-r--r-- 1000 100 0 May  6 12:53 f\n"
        );
        assert_eq!(
            styled(TimeStyle::LongIso),
            "total 0\n-rw-r--r-- 1000 100 0 2024-05-06 12:53 f\n"
        );
        assert_eq!(
            styled(TimeStyle::FullIso),
            "total 0\n-rw-r--r-- 1000 100 0 2024-05-06 12:53:20.000000000 +0000 f\n"
        );
        assert_eq!(
            styled(TimeStyle::Iso),
            "total 0\n-rw-r--r-- 1000 100 0 05-06 12:53 f\n"
        );
    }

    #[test]
    fn the_iso_style_shows_a_year_for_an_old_stamp() {
        let fs = TreeFs::new().dir(".").timed_entry(".", "f", 0, stamps(OLD));
        let out = Recorder::new();
        assert_eq!(
            run_ls(
                list_with(
                    Options {
                        long: true,
                        time_style: TimeStyle::Iso,
                        ..Options::DEFAULT
                    },
                    &["."],
                ),
                &fs,
                &out,
            ),
            Ok(())
        );
        assert_eq!(out.text(), "total 0\n-rw-r--r-- 1000 100 0 2023-11-14 f\n");
    }
}
