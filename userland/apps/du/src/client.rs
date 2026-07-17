//! The `du` engine: walk each operand, sum usage, and print the rows.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use tairix_abi::{Errno, FileKind};
use tairix_help::{own_short_help, HelpSource};
use tairix_util::size::{blocks_ceil, format_human, format_u128, SizeScale, SIZE_TEXT_MAX};

use crate::command::{Command, Options};
use crate::error::DuError;
use crate::io::{Entry, Metadata, Output, Walk};

/// The one-line usage banner, printed on a usage error and as the
/// fallback when the bundled help document is unavailable.
pub const USAGE: &str =
    "usage: du [-a | -s] [-cS0] [-h | -k | -m | -b | --si | -B <size>] [--apparent-size] [-d <n>] [--] [file...]";

/// The command word this bundle is named by, for the own-help lookup.
const OWN_WORD: &str = "du";

/// Run a parsed `du` command against the injected seams.
///
/// Returns `Ok(true)` when every operand was walked cleanly, `Ok(false)`
/// when at least one path was diagnosed on standard error (the GNU
/// behaviour: report, continue, exit `1`).
///
/// # Errors
///
/// [`DuError::Output`] when a row (or the short help) cannot be written.
pub fn run(
    command: Command,
    locale: Option<&str>,
    walk: &dyn Walk,
    help: &dyn HelpSource,
    out: &dyn Output,
    err: &dyn Output,
) -> Result<bool, DuError> {
    let options = match command {
        Command::Help => {
            let bytes = own_short_help(help, locale, OWN_WORD)
                .unwrap_or_else(|| format!("{USAGE}\n").into_bytes());
            out.write_all(&bytes).map_err(DuError::Output)?;
            return Ok(true);
        }
        Command::Report(options) => options,
    };
    let mut report = Reporter {
        options: &options,
        walk,
        out,
        err,
        clean: true,
        grand_total: 0,
    };
    if options.paths.is_empty() {
        report.operand(".")?;
    } else {
        for path in &options.paths {
            report.operand(path)?;
        }
    }
    if options.grand_total {
        let total = report.grand_total;
        report.row(total, "total")?;
    }
    Ok(report.clean)
}

/// One in-flight directory of the iterative post-order walk.
struct Frame {
    /// The directory's full path, as reported.
    path: String,
    /// Its entries, consumed front to back.
    entries: Vec<Entry>,
    /// Index of the next entry to visit.
    next: usize,
    /// Levels below the operand (the operand itself is depth `0`).
    depth: u64,
    /// The directory's own bytes plus its plain files' bytes.
    own_bytes: u128,
    /// `own_bytes` plus every subdirectory's tree total.
    tree_bytes: u128,
}

/// The walk-and-report state for one `du` run.
struct Reporter<'a> {
    options: &'a Options,
    walk: &'a dyn Walk,
    out: &'a dyn Output,
    err: &'a dyn Output,
    clean: bool,
    grand_total: u128,
}

impl Reporter<'_> {
    /// Walk one operand and report its rows.
    fn operand(&mut self, path: &str) -> Result<(), DuError> {
        let meta = match self.walk.stat(path) {
            Ok(meta) => meta,
            Err(errno) => return self.diagnose(path, errno),
        };
        if meta.kind != FileKind::Directory {
            let bytes = self.measure(&meta);
            self.grand_total += bytes;
            return self.row(bytes, path);
        }
        self.directory(path, &meta)
    }

    /// Walk a directory operand iteratively (an explicit frame stack, so
    /// a deep tree can never exhaust the call stack), reporting each
    /// subdirectory post-order exactly as GNU `du` does.
    fn directory(&mut self, path: &str, meta: &Metadata) -> Result<(), DuError> {
        let mut stack = Vec::new();
        let root = self.open_frame(path, meta, 0)?;
        let Some(root) = root else {
            return Ok(());
        };
        stack.push(root);
        loop {
            // Take the top frame's next entry, releasing the frame borrow
            // before any diagnostics or recursion bookkeeping.
            let next_child = match stack.last_mut() {
                None => break,
                Some(frame) => match frame.entries.get(frame.next).cloned() {
                    Some(entry) => {
                        frame.next += 1;
                        Some((join(&frame.path, &entry.name), entry.meta, frame.depth + 1))
                    }
                    None => None,
                },
            };
            let Some((child_path, child_meta, child_depth)) = next_child else {
                // The top frame is exhausted: report it post-order and fold
                // its tree total into its parent (or the grand total).
                let Some(frame) = stack.pop() else { break };
                let reported = if self.options.separate_dirs {
                    frame.own_bytes
                } else {
                    frame.tree_bytes
                };
                if self.within_depth(frame.depth) {
                    self.row(reported, &frame.path)?;
                }
                match stack.last_mut() {
                    Some(parent) => parent.tree_bytes += frame.tree_bytes,
                    None => self.grand_total += frame.tree_bytes,
                }
                continue;
            };
            // The child's metadata came with its directory entry: the one
            // listing already reported it, so the walk never re-resolves a
            // child by path (each such stat is a fresh full walk on an
            // uncached, authenticated volume).
            if child_meta.kind == FileKind::Directory {
                if let Some(child) = self.open_frame(&child_path, &child_meta, child_depth)? {
                    stack.push(child);
                }
                continue;
            }
            let bytes = self.measure(&child_meta);
            if let Some(frame) = stack.last_mut() {
                frame.own_bytes += bytes;
                frame.tree_bytes += bytes;
            }
            if self.options.all && self.within_depth(child_depth) {
                self.row(bytes, &child_path)?;
            }
        }
        Ok(())
    }

    /// Open a directory as a walk frame; an unreadable directory is
    /// diagnosed and skipped (its own bytes are not reported — nothing
    /// below it could be counted, so a partial number would be a guess).
    fn open_frame(
        &mut self,
        path: &str,
        meta: &Metadata,
        depth: u64,
    ) -> Result<Option<Frame>, DuError> {
        let entries = match self.walk.read_dir(path) {
            Ok(entries) => entries,
            Err(errno) => {
                self.diagnose(path, errno)?;
                return Ok(None);
            }
        };
        let own = self.measure(meta);
        Ok(Some(Frame {
            path: String::from(path),
            entries,
            next: 0,
            depth,
            own_bytes: own,
            tree_bytes: own,
        }))
    }

    /// The bytes a node contributes under the selected measure.
    fn measure(&self, meta: &Metadata) -> u128 {
        if self.options.apparent_size {
            u128::from(meta.size)
        } else {
            u128::from(meta.allocated)
        }
    }

    /// Whether a node this many levels below its operand is reported.
    fn within_depth(&self, depth: u64) -> bool {
        match self.options.max_depth {
            Some(max_depth) => depth <= max_depth,
            None => true,
        }
    }

    /// Write one `size<TAB>path` row in the selected scale.
    fn row(&self, bytes: u128, label: &str) -> Result<(), DuError> {
        let mut buf = [0u8; SIZE_TEXT_MAX];
        let size = match self.options.scale {
            SizeScale::Blocks(unit) => {
                // The unit came from the parser, which refuses zero.
                let blocks = blocks_ceil(bytes, unit).unwrap_or(0);
                format_u128(blocks, &mut buf)
            }
            SizeScale::HumanBinary => format_human(bytes, 1024, &mut buf),
            SizeScale::HumanDecimal => format_human(bytes, 1000, &mut buf),
        };
        let terminator = if self.options.null_terminated {
            '\0'
        } else {
            '\n'
        };
        let line = format!("{size}\t{label}{terminator}");
        self.out.write_all(line.as_bytes()).map_err(DuError::Output)
    }

    /// Report an unreachable path on standard error and mark the run
    /// unclean; the walk continues (GNU behaviour), but a diagnostics
    /// stream that itself fails is fatal.
    fn diagnose(&mut self, path: &str, errno: Errno) -> Result<(), DuError> {
        self.clean = false;
        let line = format!("du: cannot access '{path}': {errno}\n");
        self.err.write_all(line.as_bytes()).map_err(DuError::Output)
    }
}

/// Join a directory path and an entry name without doubling separators.
fn join(dir: &str, name: &str) -> String {
    if dir.ends_with('/') {
        format!("{dir}{name}")
    } else {
        format!("{dir}/{name}")
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::{run, USAGE};
    use crate::command::{parse, Command};
    use crate::error::DuError;
    use crate::io::{Entry, Metadata, Output, Walk};
    use alloc::collections::BTreeMap;
    use alloc::string::{String, ToString};
    use alloc::vec::Vec;
    use core::cell::RefCell;
    use tairix_abi::{Errno, FileKind};
    use tairix_help::{HelpSource, SourceError};

    /// One node of the in-memory tree fixture.
    enum Node {
        /// A regular file: `(apparent size, allocated bytes)`.
        File(u64, u64),
        /// A directory: `(allocated bytes, entry names)`.
        Dir(u64, Vec<String>),
        /// A path whose stat is refused with this errno.
        Denied(Errno),
        /// A directory that stats fine but refuses to list.
        Unlistable(u64),
    }

    /// An in-memory `Walk` over a fixed path → node map.
    struct MemFs {
        nodes: BTreeMap<String, Node>,
    }

    impl MemFs {
        fn new(nodes: &[(&str, Node)]) -> Self {
            Self {
                nodes: nodes
                    .iter()
                    .map(|(path, node)| {
                        (
                            (*path).to_string(),
                            match node {
                                Node::File(size, allocated) => Node::File(*size, *allocated),
                                Node::Dir(own, names) => Node::Dir(*own, names.clone()),
                                Node::Denied(errno) => Node::Denied(*errno),
                                Node::Unlistable(own) => Node::Unlistable(*own),
                            },
                        )
                    })
                    .collect(),
            }
        }
    }

    impl Walk for MemFs {
        fn stat(&self, path: &str) -> Result<Metadata, Errno> {
            match self.nodes.get(path) {
                Some(Node::File(size, allocated)) => Ok(Metadata {
                    kind: FileKind::Regular,
                    size: *size,
                    allocated: *allocated,
                }),
                Some(Node::Dir(own, _) | Node::Unlistable(own)) => Ok(Metadata {
                    kind: FileKind::Directory,
                    size: 0,
                    allocated: *own,
                }),
                Some(Node::Denied(errno)) => Err(*errno),
                None => Err(Errno::NotFound),
            }
        }

        fn read_dir(&self, path: &str) -> Result<Vec<Entry>, Errno> {
            match self.nodes.get(path) {
                Some(Node::Dir(_, names)) => names
                    .iter()
                    .map(|name| {
                        let child = if path.ends_with('/') {
                            alloc::format!("{path}{name}")
                        } else {
                            alloc::format!("{path}/{name}")
                        };
                        // The listing reports each child's metadata, as the
                        // production `fs_readdir` stream does.
                        let meta = self.stat(&child)?;
                        Ok(Entry {
                            name: name.clone(),
                            meta,
                        })
                    })
                    .collect(),
                Some(Node::Unlistable(_)) => Err(Errno::PermissionDenied),
                _ => Err(Errno::NotFound),
            }
        }
    }

    /// Captures everything written to one stream.
    #[derive(Default)]
    struct MemOut {
        bytes: RefCell<Vec<u8>>,
    }

    impl MemOut {
        fn text(&self) -> String {
            String::from_utf8(self.bytes.borrow().clone()).expect("utf-8 output")
        }
    }

    impl Output for MemOut {
        fn write_all(&self, bytes: &[u8]) -> Result<(), Errno> {
            self.bytes.borrow_mut().extend_from_slice(bytes);
            Ok(())
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

    /// A three-level tree: 512-byte directory metadata, two files under
    /// the root, one file in a subdirectory.
    fn tree() -> MemFs {
        MemFs::new(&[
            (
                "docs",
                Node::Dir(512, alloc::vec!["a".to_string(), "sub".to_string()]),
            ),
            ("docs/a", Node::File(100, 1024)),
            ("docs/sub", Node::Dir(512, alloc::vec!["b".to_string()])),
            ("docs/sub/b", Node::File(3000, 4096)),
        ])
    }

    fn run_case(args: &[&str], fs: &MemFs) -> (bool, String, String) {
        let command = parse(args).expect("parse");
        let out = MemOut::default();
        let err = MemOut::default();
        let clean = run(command, None, fs, &NoHelp, &out, &err).expect("run");
        (clean, out.text(), err.text())
    }

    #[test]
    fn reports_directories_post_order_in_kibibytes() {
        // sub: 512 + 4096 = 4608 B → 5 blocks; docs: 512 + 1024 + 4608 =
        // 6144 B → 6 blocks. Post-order: the subdirectory first.
        let (clean, out, err) = run_case(&["docs"], &tree());
        assert!(clean);
        assert_eq!(out, "5\tdocs/sub\n6\tdocs\n");
        assert!(err.is_empty());
    }

    #[test]
    fn all_reports_files_and_null_terminates() {
        let (clean, out, _) = run_case(&["-a0", "docs"], &tree());
        assert!(clean);
        // docs/a: 1024 B → 1 block; docs/sub/b: 4096 B → 4 blocks.
        assert_eq!(
            out,
            "1\tdocs/a\x004\tdocs/sub/b\x005\tdocs/sub\x006\tdocs\x00"
        );
    }

    #[test]
    fn summarize_and_total_report_the_operand_totals() {
        let (clean, out, _) = run_case(&["-sc", "docs", "docs/a"], &tree());
        assert!(clean);
        // The grand total covers both operands: 6144 + 1024 = 7168 B.
        assert_eq!(out, "6\tdocs\n1\tdocs/a\n7\ttotal\n");
    }

    #[test]
    fn max_depth_bounds_the_rows_not_the_sums() {
        let (clean, out, _) = run_case(&["-d0", "docs"], &tree());
        assert!(clean);
        assert_eq!(out, "6\tdocs\n");
    }

    #[test]
    fn separate_dirs_excludes_subdirectories_from_a_row() {
        let (clean, out, _) = run_case(&["-S", "docs"], &tree());
        assert!(clean);
        // docs' own row excludes sub's 4608 B: 512 + 1024 = 1536 B → 2.
        assert_eq!(out, "5\tdocs/sub\n2\tdocs\n");
    }

    #[test]
    fn apparent_size_and_bytes_measure_lengths() {
        let (clean, out, _) = run_case(&["-b", "docs"], &tree());
        assert!(clean);
        // Apparent bytes: sub = 0 + 3000; docs = 0 + 100 + 3000.
        assert_eq!(out, "3000\tdocs/sub\n3100\tdocs\n");
    }

    #[test]
    fn human_readable_scales_the_rows() {
        let (clean, out, _) = run_case(&["-h", "docs"], &tree());
        assert!(clean);
        assert_eq!(out, "4.5K\tdocs/sub\n6.0K\tdocs\n");
    }

    #[test]
    fn an_unreachable_operand_is_diagnosed_and_the_walk_continues() {
        let fs = MemFs::new(&[
            ("docs", Node::Dir(512, alloc::vec!["a".to_string()])),
            ("docs/a", Node::File(100, 1024)),
            ("gone", Node::Denied(Errno::PermissionDenied)),
        ]);
        let (clean, out, err) = run_case(&["gone", "docs"], &fs);
        assert!(!clean, "a diagnosed path makes the run unclean");
        // The reachable operand is still counted: 512 + 1024 = 1536 B → 2.
        assert_eq!(out, "2\tdocs\n");
        assert!(err.contains("gone"));
        assert!(err.contains("cannot access"));
    }

    #[test]
    fn an_unlistable_directory_is_diagnosed_and_skipped() {
        let fs = MemFs::new(&[
            ("docs", Node::Dir(512, alloc::vec!["locked".to_string()])),
            ("docs/locked", Node::Unlistable(512)),
        ]);
        let (clean, out, err) = run_case(&["docs"], &fs);
        assert!(!clean);
        // Only docs' own 512 B are countable → 1 block.
        assert_eq!(out, "1\tdocs\n");
        assert!(err.contains("docs/locked"));
    }

    #[test]
    fn a_missing_operand_is_diagnosed_without_a_row() {
        let (clean, out, err) = run_case(&["absent"], &tree());
        assert!(!clean);
        assert!(out.is_empty());
        assert!(err.contains("absent"));
    }

    #[test]
    fn no_operand_walks_the_current_directory() {
        let fs = MemFs::new(&[(".", Node::Dir(512, Vec::new()))]);
        let (clean, out, _) = run_case(&[], &fs);
        assert!(clean);
        assert_eq!(out, "1\t.\n");
    }

    #[test]
    fn a_file_operand_reports_one_row() {
        let (clean, out, _) = run_case(&["docs/a"], &tree());
        assert!(clean);
        assert_eq!(out, "1\tdocs/a\n");
    }

    #[test]
    fn help_prints_the_usage_fallback() {
        let out = MemOut::default();
        let err = MemOut::default();
        let clean = run(Command::Help, None, &tree(), &NoHelp, &out, &err).expect("run");
        assert!(clean);
        assert_eq!(out.text(), alloc::format!("{USAGE}\n"));
    }

    #[test]
    fn a_failed_output_write_is_fatal() {
        let command = parse(&["docs"]).expect("parse");
        let err = MemOut::default();
        let result = run(command, None, &tree(), &NoHelp, &BrokenSink, &err);
        assert_eq!(result, Err(DuError::Output(Errno::NotImplemented)));
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
        let locales = tairix_help::REQUIRED_LOCALES;
        for locale in locales {
            let path = format!("{help_root}/{locale}/du.md");
            let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
            for switch in [
                "`-a, --all`",
                "`-s, --summarize`",
                "`-c, --total`",
                "`-h, --human-readable`",
                "`--si`",
                "`-k`",
                "`-m`",
                "`-b, --bytes`",
                "`-B, --block-size <size>`",
                "`--apparent-size`",
                "`-d, --max-depth <n>`",
                "`-S, --separate-dirs`",
                "`-0, --null`",
                "`-?, --help`",
            ] {
                assert!(
                    text.contains(switch),
                    "{locale}/du.md must document {switch}"
                );
            }
        }
    }
}
