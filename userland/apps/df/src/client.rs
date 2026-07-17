//! The `df` engine: fetch the mount table, select and filter the
//! filesystems, and render the usage report.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use tairix_abi::driver::filesystem::VolumeStats;
use tairix_abi::stdinfo::{Human, Severity, StdInfoKind, StdInfoRecord};
use tairix_help::{own_short_help, HelpSource};
use tairix_procinfo::{for_each_mount, Transport};
use tairix_util::size::{blocks_ceil, format_human, format_u128, SizeScale, SIZE_TEXT_MAX};

use crate::command::{Command, Options};
use crate::error::DfError;
use crate::io::{Output, PathProbe};

/// The one-line usage banner, printed on a usage error and as the
/// fallback when the bundled help document is unavailable.
pub const USAGE: &str =
    "usage: df [-aikPTl] [-h | -H | --si | -B <size>] [-t <type>] [-x <type>] [--total] [--] [file...]";

/// The command word this bundle is named by, for the own-help lookup.
const OWN_WORD: &str = "df";

/// One mounted filesystem, decoded from its wire record.
#[derive(Clone, Debug)]
struct Fs {
    source: String,
    target: String,
    fstype: String,
    usage: VolumeStats,
}

/// Run a parsed `df` command against the injected seams.
///
/// Returns `Ok(true)` when the report covered everything asked for,
/// `Ok(false)` when an operand was diagnosed on standard error (the GNU
/// behaviour: report what remains, exit `1`).
///
/// # Errors
///
/// * [`DfError::Service`] — the mount-table query failed.
/// * [`DfError::Output`] — a row (or the short help) could not be
///   written.
/// * [`DfError::NothingProcessed`] — the type filters left nothing
///   to report.
pub fn run(
    command: Command,
    locale: Option<&str>,
    transport: &dyn Transport,
    probe: &dyn PathProbe,
    help: &dyn HelpSource,
    out: &dyn Output,
    err: &dyn Output,
) -> Result<bool, DfError> {
    let options = match command {
        Command::Help => {
            let bytes = own_short_help(help, locale, OWN_WORD)
                .unwrap_or_else(|| format!("{USAGE}\n").into_bytes());
            out.write_all(&bytes).map_err(DfError::Output)?;
            return Ok(true);
        }
        Command::Report(options) => options,
    };

    // The live mount table, in service order (the permanent root first).
    let mut mounts: Vec<Fs> = Vec::new();
    for_each_mount(transport, |record| {
        mounts.push(Fs {
            source: String::from_utf8_lossy(record.source_bytes()).into_owned(),
            target: String::from_utf8_lossy(record.target_bytes()).into_owned(),
            fstype: String::from_utf8_lossy(record.fstype_bytes()).into_owned(),
            usage: record.usage(),
        });
        Ok(())
    })
    .map_err(DfError::from)?;

    let mut clean = true;
    let (selected, hidden) = if options.paths.is_empty() {
        select_all(&options, &mounts)
    } else {
        let selected = select_operands(&options, &mounts, probe, err, &mut clean)?;
        (selected, 0)
    };

    // The type filters apply to whatever was selected; leaving nothing is
    // the GNU `no file systems processed` outcome.
    let rows: Vec<&Fs> = selected
        .into_iter()
        .filter(|fs| type_selected(&options, fs))
        .collect();
    if rows.is_empty() {
        if clean {
            return Err(DfError::NothingProcessed);
        }
        // Every operand was already diagnosed; there is nothing to table.
        return Ok(false);
    }

    render(&options, &rows, out)?;
    if hidden > 0 {
        emit_omission_record(out, hidden);
    }
    Ok(clean)
}

/// The default whole-table selection: hide capacity-less mounts (the
/// in-RAM layout bindings) and further mounts of an already-listed
/// volume, unless `-a` asks for everything. Returns the kept rows and
/// how many were hidden.
fn select_all<'a>(options: &Options, mounts: &'a [Fs]) -> (Vec<&'a Fs>, u64) {
    if options.all {
        return (mounts.iter().collect(), 0);
    }
    let mut seen_sources: Vec<&str> = Vec::new();
    let mut kept = Vec::new();
    let mut hidden = 0u64;
    for fs in mounts {
        let pseudo = fs.usage.total_blocks == 0;
        let duplicate = !fs.source.is_empty() && seen_sources.contains(&fs.source.as_str());
        if pseudo || duplicate {
            hidden += 1;
            continue;
        }
        if !fs.source.is_empty() {
            seen_sources.push(&fs.source);
        }
        kept.push(fs);
    }
    (kept, hidden)
}

/// Resolve each `file` operand to the mount that contains it (the
/// longest mount-point prefix). A missing, unreachable, or relative
/// operand is diagnosed and skipped; the report continues.
fn select_operands<'a>(
    options: &Options,
    mounts: &'a [Fs],
    probe: &dyn PathProbe,
    err: &dyn Output,
    clean: &mut bool,
) -> Result<Vec<&'a Fs>, DfError> {
    let mut rows: Vec<&Fs> = Vec::new();
    for path in &options.paths {
        if let Err(errno) = probe.probe(path) {
            diagnose(err, &format!("{path}: {errno}"))?;
            *clean = false;
            continue;
        }
        if !path.starts_with('/') {
            // Mount points are absolute; without a resolved absolute form
            // a relative operand cannot be matched honestly.
            diagnose(
                err,
                &format!("{path}: cannot resolve a relative path to its mount point"),
            )?;
            *clean = false;
            continue;
        }
        let Some(covering) = covering_mount(mounts, path) else {
            // A live table always carries the root mount, so this is a
            // service anomaly worth diagnosing, never a silent skip.
            diagnose(err, &format!("{path}: no mount covers this path"))?;
            *clean = false;
            continue;
        };
        // One row per filesystem, however many operands it covers.
        if !rows.iter().any(|fs| core::ptr::eq::<Fs>(*fs, covering)) {
            rows.push(covering);
        }
    }
    Ok(rows)
}

/// The mount with the longest mount-point path that is a prefix of
/// `path` (`/` covers everything).
fn covering_mount<'a>(mounts: &'a [Fs], path: &str) -> Option<&'a Fs> {
    mounts
        .iter()
        .filter(|fs| {
            fs.target == "/"
                || path == fs.target
                || (path.starts_with(&fs.target)
                    && path.as_bytes().get(fs.target.len()) == Some(&b'/'))
        })
        .max_by_key(|fs| fs.target.len())
}

/// Whether the `-t`/`-x` filters keep `fs` in the report.
fn type_selected(options: &Options, fs: &Fs) -> bool {
    if options.exclude_types.contains(&fs.fstype) {
        return false;
    }
    options.types.is_empty() || options.types.contains(&fs.fstype)
}

/// A table cell: its text and whether it right-aligns (numbers do).
struct Cell {
    text: String,
    numeric: bool,
}

impl Cell {
    fn text(value: impl Into<String>) -> Self {
        Self {
            text: value.into(),
            numeric: false,
        }
    }

    fn number(value: impl Into<String>) -> Self {
        Self {
            text: value.into(),
            numeric: true,
        }
    }
}

/// Render the selected rows as the GNU-shaped table.
fn render(options: &Options, rows: &[&Fs], out: &dyn Output) -> Result<(), DfError> {
    let mut table: Vec<Vec<Cell>> = Vec::with_capacity(rows.len() + 2);
    table.push(header(options));
    for fs in rows {
        table.push(row(options, fs));
    }
    if options.grand_total {
        table.push(total_row(options, rows));
    }

    // Column widths from the widest cell of each column; the final
    // column is never padded, so a long mount point cannot smear the
    // line with trailing spaces.
    let columns = table.iter().map(Vec::len).max().unwrap_or(0);
    let mut widths = alloc::vec![0usize; columns];
    for line in &table {
        for (index, cell) in line.iter().enumerate() {
            if let Some(width) = widths.get_mut(index) {
                *width = (*width).max(cell.text.chars().count());
            }
        }
    }
    for line in &table {
        let mut rendered = String::new();
        let last = line.len().saturating_sub(1);
        for (index, cell) in line.iter().enumerate() {
            if index > 0 {
                rendered.push(' ');
            }
            let width = widths.get(index).copied().unwrap_or(0);
            let pad = width.saturating_sub(cell.text.chars().count());
            if index == last {
                rendered.push_str(&cell.text);
            } else if cell.numeric {
                for _ in 0..pad {
                    rendered.push(' ');
                }
                rendered.push_str(&cell.text);
            } else {
                rendered.push_str(&cell.text);
                for _ in 0..pad {
                    rendered.push(' ');
                }
            }
        }
        rendered.push('\n');
        out.write_all(rendered.as_bytes())
            .map_err(DfError::Output)?;
    }
    Ok(())
}

/// The header line for the selected format.
fn header(options: &Options) -> Vec<Cell> {
    let mut cells = Vec::new();
    cells.push(Cell::text("Filesystem"));
    if options.print_type {
        cells.push(Cell::text("Type"));
    }
    if options.inodes {
        cells.push(Cell::number("Inodes"));
        cells.push(Cell::number("IUsed"));
        cells.push(Cell::number("IFree"));
        cells.push(Cell::number(if options.portability {
            "Capacity"
        } else {
            "IUse%"
        }));
    } else {
        cells.push(Cell::number(blocks_header(options)));
        cells.push(Cell::number("Used"));
        cells.push(Cell::number(match options.scale {
            SizeScale::Blocks(_) => "Available",
            SizeScale::HumanBinary | SizeScale::HumanDecimal => "Avail",
        }));
        cells.push(Cell::number(if options.portability {
            "Capacity"
        } else {
            "Use%"
        }));
    }
    cells.push(Cell::text("Mounted on"));
    cells
}

/// The size column's header: `Size` for the human formats, the GNU
/// `1K-blocks`-style unit name otherwise (`1024-blocks` under `-P`).
fn blocks_header(options: &Options) -> String {
    match options.scale {
        SizeScale::HumanBinary | SizeScale::HumanDecimal => String::from("Size"),
        SizeScale::Blocks(unit) => {
            if options.portability {
                return format!("{unit}-blocks");
            }
            format!("{}-blocks", block_unit_name(unit))
        }
    }
}

/// A block size as GNU spells it in the header: an exact power-of-1024
/// multiple as `1K`/`4M`, an exact power-of-1000 multiple as `1kB`/`2MB`,
/// anything else as raw bytes (`512B`).
fn block_unit_name(unit: u64) -> String {
    const BINARY: [(u64, char); 6] = [
        (1 << 60, 'E'),
        (1 << 50, 'P'),
        (1 << 40, 'T'),
        (1 << 30, 'G'),
        (1 << 20, 'M'),
        (1 << 10, 'K'),
    ];
    const DECIMAL: [(u64, &str); 6] = [
        (1_000_000_000_000_000_000, "EB"),
        (1_000_000_000_000_000, "PB"),
        (1_000_000_000_000, "TB"),
        (1_000_000_000, "GB"),
        (1_000_000, "MB"),
        (1_000, "kB"),
    ];
    for (value, letter) in BINARY {
        if unit >= value && unit % value == 0 {
            return format!("{}{letter}", unit / value);
        }
    }
    for (value, letters) in DECIMAL {
        if unit >= value && unit % value == 0 {
            return format!("{}{letters}", unit / value);
        }
    }
    format!("{unit}B")
}

/// The byte totals a volume's usage reduces to: `(total, used, avail)`.
fn byte_figures(usage: &VolumeStats) -> (u128, u128, u128) {
    let block = u128::from(usage.block_size);
    let total = u128::from(usage.total_blocks) * block;
    let free = u128::from(usage.free_blocks) * block;
    let avail = u128::from(usage.avail_blocks) * block;
    (total, total.saturating_sub(free), avail)
}

/// One data row for `fs` in the selected format.
fn row(options: &Options, fs: &Fs) -> Vec<Cell> {
    let mut cells = Vec::new();
    cells.push(Cell::text(if fs.source.is_empty() {
        "-"
    } else {
        fs.source.as_str()
    }));
    if options.print_type {
        cells.push(Cell::text(if fs.fstype.is_empty() {
            "-"
        } else {
            fs.fstype.as_str()
        }));
    }
    if options.inodes {
        push_inode_cells(&mut cells, fs.usage.files, fs.usage.files_free);
    } else {
        let (total, used, avail) = byte_figures(&fs.usage);
        push_size_cells(&mut cells, options, total, used, avail);
    }
    cells.push(Cell::text(fs.target.as_str()));
    cells
}

/// The `--total` summary row: the displayed rows' figures summed.
fn total_row(options: &Options, rows: &[&Fs]) -> Vec<Cell> {
    let mut cells = Vec::new();
    cells.push(Cell::text("total"));
    if options.print_type {
        cells.push(Cell::text("-"));
    }
    if options.inodes {
        let files: u64 = rows.iter().map(|fs| fs.usage.files).sum();
        let files_free: u64 = rows.iter().map(|fs| fs.usage.files_free).sum();
        push_inode_cells(&mut cells, files, files_free);
    } else {
        let mut total = 0u128;
        let mut used = 0u128;
        let mut avail = 0u128;
        for fs in rows {
            let (row_total, row_used, row_avail) = byte_figures(&fs.usage);
            total += row_total;
            used += row_used;
            avail += row_avail;
        }
        push_size_cells(&mut cells, options, total, used, avail);
    }
    cells.push(Cell::text("-"));
    cells
}

/// Append the four block-usage cells in the selected scale.
fn push_size_cells(cells: &mut Vec<Cell>, options: &Options, total: u128, used: u128, avail: u128) {
    let mut buf = [0u8; SIZE_TEXT_MAX];
    for bytes in [total, used, avail] {
        let text = match options.scale {
            SizeScale::Blocks(unit) => {
                // The unit came from the parser, which refuses zero.
                format_u128(blocks_ceil(bytes, unit).unwrap_or(0), &mut buf).to_string()
            }
            SizeScale::HumanBinary => format_human(bytes, 1024, &mut buf).to_string(),
            SizeScale::HumanDecimal => format_human(bytes, 1000, &mut buf).to_string(),
        };
        cells.push(Cell::number(text));
    }
    cells.push(Cell::number(percentage(used, used + avail)));
}

/// Append the four inode cells; a volume with no fixed inode table
/// (`files == 0`) reports zeros and `-`, never a fabricated capacity.
fn push_inode_cells(cells: &mut Vec<Cell>, files: u64, files_free: u64) {
    let used = files.saturating_sub(files_free);
    cells.push(Cell::number(format!("{files}")));
    cells.push(Cell::number(format!("{used}")));
    cells.push(Cell::number(format!("{files_free}")));
    cells.push(Cell::number(percentage(
        u128::from(used),
        u128::from(files),
    )));
}

/// `used` over `capacity` as the GNU ceiling percentage, or `-` when
/// there is no capacity to be a fraction of.
fn percentage(used: u128, capacity: u128) -> String {
    if capacity == 0 {
        return String::from("-");
    }
    format!("{}%", (used * 100).div_ceil(capacity))
}

/// Report an operand problem on standard error; the report continues,
/// but a diagnostics stream that itself fails is fatal.
fn diagnose(err: &dyn Output, message: &str) -> Result<(), DfError> {
    let line = format!("df: {message}\n");
    err.write_all(line.as_bytes()).map_err(DfError::Output)
}

/// Emit the `fs.mounts_omitted` advisory (fd 3) when the default view
/// hid capacity-less or duplicate mounts: a tool or user then knows the
/// table is not exhaustive and how to see the rest. Advisory only —
/// never affects the table, the exit status, or ordering.
fn emit_omission_record(out: &dyn Output, omitted: u64) {
    let message = if omitted == 1 {
        String::from("1 pseudo or duplicate mount not shown.")
    } else {
        format!("{omitted} pseudo or duplicate mounts not shown.")
    };
    let ai = format!(
        "{{\"subject\":\"filesystem_report\",\
         \"omission\":{{\"reason\":\"hidden_by_default\",\
         \"entry_class\":\"pseudo_or_duplicate_mount\",\"omitted_count\":{omitted},\
         \"stdout_is_exhaustive\":false}},\
         \"suggestion\":{{\"argv\":[\"df\",\"-a\"],\
         \"safe_to_autorun\":false,\"requires_confirmation\":true}}}}"
    );
    let record = StdInfoRecord::new(
        OWN_WORD,
        StdInfoKind::Omission,
        "fs.mounts_omitted",
        Severity::Info,
        Human::with_suggestion(&message, "Use `df -a` to show them."),
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
    use crate::command::{parse, Command};
    use crate::error::DfError;
    use crate::io::{Output, PathProbe};
    use alloc::string::{String, ToString};
    use alloc::vec::Vec;
    use core::cell::RefCell;
    use tairix_abi::driver::filesystem::{MountFlags, VolumeStats};
    use tairix_abi::sysinfo::{
        MountAvailability, MountListRequest, MountRecord, SysinfoRequestHeader,
    };
    use tairix_abi::Errno;
    use tairix_help::{HelpSource, SourceError};
    use tairix_procinfo::Transport;

    /// An in-memory `sysinfod` stand-in answering mount-list queries from
    /// a fixture, decoding the request the same way the real service does.
    struct Fixture {
        records: Vec<MountRecord>,
        fail: Option<Errno>,
    }

    impl Fixture {
        fn new(records: Vec<MountRecord>) -> Self {
            Self {
                records,
                fail: None,
            }
        }
    }

    impl Transport for Fixture {
        fn query(&self, request: &[u8]) -> Result<Vec<u8>, Errno> {
            let header = SysinfoRequestHeader::from_bytes(request)?;
            if let Some(errno) = self.fail {
                return Err(errno);
            }
            let payload = &request[SysinfoRequestHeader::WIRE_LEN
                ..SysinfoRequestHeader::WIRE_LEN + header.payload_len as usize];
            let req = MountListRequest::from_bytes(payload)?;
            let offset = req.offset as usize;
            if offset >= self.records.len() {
                return Ok(Vec::new());
            }
            let take = core::cmp::min(self.records.len() - offset, req.limit as usize);
            let mut out = Vec::with_capacity(take * MountRecord::WIRE_LEN);
            for record in &self.records[offset..offset + take] {
                out.extend_from_slice(&record.to_le_bytes());
            }
            Ok(out)
        }
    }

    /// A probe over a fixed list of existing paths.
    struct MemProbe {
        existing: Vec<String>,
    }

    impl MemProbe {
        fn new(existing: &[&str]) -> Self {
            Self {
                existing: existing.iter().map(|p| (*p).to_string()).collect(),
            }
        }
    }

    impl PathProbe for MemProbe {
        fn probe(&self, path: &str) -> Result<(), Errno> {
            if self.existing.iter().any(|p| p == path) {
                Ok(())
            } else {
                Err(Errno::NotFound)
            }
        }
    }

    /// Captures everything written to one stream, plus fd-3 records.
    #[derive(Default)]
    struct MemOut {
        bytes: RefCell<Vec<u8>>,
        records: RefCell<Vec<String>>,
    }

    impl MemOut {
        fn text(&self) -> String {
            String::from_utf8(self.bytes.borrow().clone()).expect("utf-8 output")
        }

        fn records(&self) -> Vec<String> {
            self.records.borrow().clone()
        }
    }

    impl Output for MemOut {
        fn write_all(&self, bytes: &[u8]) -> Result<(), Errno> {
            self.bytes.borrow_mut().extend_from_slice(bytes);
            Ok(())
        }

        fn info(&self, record: &[u8]) {
            self.records
                .borrow_mut()
                .push(String::from_utf8_lossy(record).into_owned());
        }
    }

    /// A stream that refuses every write.
    struct BrokenSink;

    impl Output for BrokenSink {
        fn write_all(&self, _bytes: &[u8]) -> Result<(), Errno> {
            Err(Errno::NotImplemented)
        }
    }

    /// A help tree with no documents, so the usage banner is the fallback.
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

    fn usage(block_size: u32, total: u64, free: u64, avail: u64) -> VolumeStats {
        VolumeStats {
            block_size,
            total_blocks: total,
            free_blocks: free,
            avail_blocks: avail,
            files: 0,
            files_free: 0,
        }
    }

    fn record(source: &str, target: &str, fstype: &str, stats: VolumeStats) -> MountRecord {
        MountRecord::new(
            source.as_bytes(),
            target.as_bytes(),
            fstype.as_bytes(),
            MountFlags::default(),
            stats,
            MountAvailability::Available,
            [0u8; 16],
        )
        .expect("record")
    }

    /// The production shape: a backed root volume, a read-only system
    /// volume, rebased sub-mounts of the root volume, and an unbacked
    /// in-RAM binding.
    fn table() -> Fixture {
        Fixture::new(alloc::vec![
            record("ARXFSRoot", "/", "arxfs", usage(512, 4096, 2048, 2032)),
            record(
                "ARXFSSystem",
                "/System",
                "arxfs",
                usage(512, 1024, 128, 112),
            ),
            record("ARXFSRoot", "/Users", "arxfs", usage(512, 4096, 2048, 2032)),
            record("", "/System/Logs", "", VolumeStats::default()),
        ])
    }

    fn run_case(
        args: &[&str],
        fixture: &Fixture,
        probe: &MemProbe,
    ) -> (bool, String, String, Vec<String>) {
        let command = parse(args).expect("parse");
        let out = MemOut::default();
        let err = MemOut::default();
        let clean = run(command, None, fixture, probe, &NoHelp, &out, &err).expect("run");
        (clean, out.text(), err.text(), out.records())
    }

    #[test]
    fn default_report_hides_pseudo_and_duplicate_mounts_and_notes_it() {
        let (clean, out, err, records) = run_case(&[], &table(), &MemProbe::new(&[]));
        assert!(clean);
        // Root: 2048 KiB total, 1024 used, 1016 available, 50% of
        // (1024 + 1016 = 2040). System: 512 total, 448 used, 56 avail.
        assert_eq!(
            out,
            "Filesystem  1K-blocks Used Available Use% Mounted on\n\
             ARXFSRoot        2048 1024      1016  51% /\n\
             ARXFSSystem       512  448        56  89% /System\n"
        );
        assert!(err.is_empty());
        // The duplicate /Users mount and the unbacked binding were hidden
        // and noted on fd 3.
        assert_eq!(records.len(), 1);
        assert!(
            records[0].contains("\"code\":\"fs.mounts_omitted\""),
            "{}",
            records[0]
        );
        assert!(records[0].contains("2 pseudo or duplicate mounts not shown."));
    }

    #[test]
    fn all_shows_every_mount_without_a_record() {
        let (clean, out, _, records) = run_case(&["-a"], &table(), &MemProbe::new(&[]));
        assert!(clean);
        assert!(out.contains("/Users"));
        assert!(out.contains("/System/Logs"));
        // The unbacked binding reports the honest unknowns.
        assert!(out.contains("- "), "empty source renders as '-': {out}");
        assert!(records.is_empty());
    }

    #[test]
    fn print_type_and_filters_select_by_fstype() {
        let (clean, out, _, _) = run_case(&["-T"], &table(), &MemProbe::new(&[]));
        assert!(clean);
        assert!(out.contains("Type"));
        assert!(out.contains("arxfs"));
        let (clean, out, _, _) = run_case(&["-a", "-x", "arxfs"], &table(), &MemProbe::new(&[]));
        assert!(clean);
        assert!(!out.contains("arxfs"));
        assert!(out.contains("/System/Logs"));
        let result = {
            let command = parse(&["-t", "ext4"]).expect("parse");
            run(
                command,
                None,
                &table(),
                &MemProbe::new(&[]),
                &NoHelp,
                &MemOut::default(),
                &MemOut::default(),
            )
        };
        assert_eq!(result, Err(DfError::NothingProcessed));
    }

    #[test]
    fn human_readable_scales_and_renames_the_columns() {
        let (clean, out, _, _) = run_case(&["-h"], &table(), &MemProbe::new(&[]));
        assert!(clean);
        assert!(out.contains("Size"));
        assert!(out.contains("Avail"));
        // Root total: 4096 × 512 B = 2.0M.
        assert!(out.contains("2.0M"), "{out}");
    }

    #[test]
    fn portability_uses_the_posix_header_wording() {
        let (clean, out, _, _) = run_case(&["-P"], &table(), &MemProbe::new(&[]));
        assert!(clean);
        assert!(out.contains("1024-blocks"));
        assert!(out.contains("Capacity"));
    }

    #[test]
    fn block_size_names_the_header_unit() {
        let (_, out, _, _) = run_case(&["-B", "4K"], &table(), &MemProbe::new(&[]));
        assert!(out.contains("4K-blocks"));
        let (_, out, _, _) = run_case(&["-B", "512"], &table(), &MemProbe::new(&[]));
        assert!(out.contains("512B-blocks"));
        let (_, out, _, _) = run_case(&["--block-size=1kB"], &table(), &MemProbe::new(&[]));
        assert!(out.contains("1kB-blocks"));
    }

    #[test]
    fn inodes_report_the_honest_untracked_zeros() {
        let (clean, out, _, _) = run_case(&["-i"], &table(), &MemProbe::new(&[]));
        assert!(clean);
        assert!(out.contains("Inodes"));
        assert!(out.contains("IUse%"));
        // A dynamic-inode volume reports zeros and `-`, like GNU df on
        // btrfs — never a fabricated capacity.
        assert!(out.contains(" 0 "), "{out}");
        assert!(out.contains(" - "), "{out}");
    }

    #[test]
    fn total_appends_the_summed_row() {
        let (clean, out, _, _) = run_case(&["--total"], &table(), &MemProbe::new(&[]));
        assert!(clean);
        // 2048 + 512 total KiB; 1024 + 448 used; 1016 + 56 available.
        assert!(out.contains("total"), "{out}");
        assert!(out.contains("2560"), "{out}");
        assert!(out.contains("1472"), "{out}");
        assert!(out.contains("1072"), "{out}");
    }

    #[test]
    fn an_operand_selects_its_covering_mount_once() {
        let probe = MemProbe::new(&["/Users/jo/notes.txt", "/Users/mo"]);
        let (clean, out, err, _) =
            run_case(&["/Users/jo/notes.txt", "/Users/mo"], &table(), &probe);
        assert!(clean);
        assert!(err.is_empty());
        // Both operands live on the /Users mount: one row, not two.
        assert_eq!(out.matches("/Users").count(), 1, "{out}");
        assert!(!out.contains("/System"), "{out}");
    }

    #[test]
    fn a_missing_or_relative_operand_is_diagnosed() {
        let probe = MemProbe::new(&["notes.txt"]);
        let (clean, out, err, _) = run_case(&["/absent"], &table(), &probe);
        assert!(!clean);
        assert!(out.is_empty());
        assert!(err.contains("/absent"));
        // A relative operand exists but cannot be matched to a mount
        // point honestly; it is diagnosed, never guessed.
        let (clean, out, err, _) = run_case(&["notes.txt"], &table(), &probe);
        assert!(!clean);
        assert!(out.is_empty());
        assert!(err.contains("relative path"));
    }

    #[test]
    fn a_service_failure_is_fatal() {
        let mut fixture = table();
        fixture.fail = Some(Errno::PermissionDenied);
        let command = parse(&[]).expect("parse");
        let result = run(
            command,
            None,
            &fixture,
            &MemProbe::new(&[]),
            &NoHelp,
            &MemOut::default(),
            &MemOut::default(),
        );
        assert_eq!(result, Err(DfError::Service(Errno::PermissionDenied)));
    }

    #[test]
    fn a_failed_output_write_is_fatal() {
        let command = parse(&[]).expect("parse");
        let result = run(
            command,
            None,
            &table(),
            &MemProbe::new(&[]),
            &NoHelp,
            &BrokenSink,
            &MemOut::default(),
        );
        assert_eq!(result, Err(DfError::Output(Errno::NotImplemented)));
    }

    #[test]
    fn help_prints_the_usage_fallback() {
        let out = MemOut::default();
        let err = MemOut::default();
        let clean = run(
            Command::Help,
            None,
            &table(),
            &MemProbe::new(&[]),
            &NoHelp,
            &out,
            &err,
        )
        .expect("run");
        assert!(clean);
        assert_eq!(out.text(), alloc::format!("{USAGE}\n"));
    }

    /// Every locale's `OPTIONS` section documents exactly the switches this
    /// parser accepts (`plans/APPS.md` §3.1): the flag tokens are
    /// language-neutral, so each translated document must carry the same
    /// keys as the canonical one. The documents are read from the bundle's
    /// own on-disk `Help/` tree — the single source the image builder
    /// plants — never a copy embedded in this crate.
    #[test]
    fn help_documents_the_parser_switches() {
        extern crate std;
        use alloc::format;
        use std::fs;

        let help_root = format!("{}/Help", env!("CARGO_MANIFEST_DIR"));
        let locales = tairix_help::REQUIRED_LOCALES;
        for locale in locales {
            let path = format!("{help_root}/{locale}/df.md");
            let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
            for switch in [
                "`-a, --all`",
                "`-h, --human-readable`",
                "`-H, --si`",
                "`-k`",
                "`-B, --block-size <size>`",
                "`-i, --inodes`",
                "`-T, --print-type`",
                "`-t, --type <type>`",
                "`-x, --exclude-type <type>`",
                "`-P, --portability`",
                "`-l, --local`",
                "`--total`",
                "`-?, --help`",
            ] {
                assert!(
                    text.contains(switch),
                    "{locale}/df.md must document {switch}"
                );
            }
        }
    }
}
