//! The listing engine: inspect each operand, read each directory, and write
//! the sorted, formatted listing to the terminal.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::cmp::Reverse;
use core::fmt::Write as _;

use tairix_abi::fs::{FileId, FileKind};
use tairix_abi::stdinfo::{Human, Severity, StdInfoKind, StdInfoRecord};
use tairix_abi::time::CivilTime;
use tairix_abi::time::Time64;
use tairix_abi::Errno;
use tairix_curses::downgrade;
use tairix_help::{own_short_help, HelpSource};
use tairix_path::join;
use tairix_termcap::{ColorChoice, ColorDepth};
use tairix_vt::{encode_into, str_width, Op, Role, Sgr};

use crate::command::{
    Command, Dereference, Filters, Format, Hidden, Indicator, Options, QuotingStyle, SizeFormat,
    Sort, TimeField, TimeStyle,
};
use crate::error::LsError;
use crate::io::{Entry, FinalLink, Listing, Metadata, Output};

/// The usage banner a usage error is reported with, and the fallback the
/// short-help switches print when `ls`'s own Help tree is unavailable.
pub const USAGE: &str =
    "usage: ls [-aACdFgGhklmnopQrRsSTtx1] [-w cols] [--block-size=SIZE] [--] [path...]";

/// `ls`'s own command word: the short-help switches render its own Help
/// document through the same engine as any other command's.
const OWN_WORD: &str = "ls";

/// Run one [`Command`], inspecting its paths through `fs` and writing the
/// rendered listing to `out`. `locale` is the user's `LANG` preference, if
/// set; `term` is the user's `TERM` value, if set, which decides the colour
/// depth `--color` output renders at (`None`, or a mono `TERM`, yields plain
/// output under `auto`); `help` is the tool's own `Help/` tree, read by the
/// short-help switches.
///
/// Non-directory operands are listed first (by name), then each directory
/// operand has its entries listed, sorted by name. When more than one operand
/// is given, each directory's listing is preceded by a `path:` header and
/// blocks are separated by a blank line — the POSIX model.
///
/// A path that cannot be inspected or read does **not** end the listing: the
/// reason is written to the error stream, that path is skipped, and the
/// [`Outcome`] records how serious it was so the caller can exit non-zero —
/// the GNU behaviour, and the only way `ls -L` over a directory holding a
/// dangling link can list the rest of it.
///
/// # Errors
///
/// * [`LsError::Output`] — writing the listing to the terminal failed. This
///   is the one failure that stops the run: with nowhere to write, there is
///   nothing left to do.
pub fn run(
    command: Command,
    locale: Option<&str>,
    now: Time64,
    term: Option<&str>,
    fs: &dyn Listing,
    help: &dyn HelpSource,
    out: &dyn Output,
) -> Result<Outcome, LsError> {
    match command {
        Command::Help => short_help(locale, help, out).map(|()| Outcome::Complete),
        Command::List {
            options,
            filters,
            paths,
        } => list(options, &filters, &paths, now, term, fs, out),
    }
}

/// How completely a listing that reached the end of its operands ran.
///
/// GNU `ls` grades its exit status: everything listed is `0`, a *minor*
/// problem — an entry or subdirectory inside a listing that could not be
/// reached — is `1`, and *serious trouble* — a command-line operand that
/// could not be reached at all — is `2`. The reason is always reported on
/// the error stream as it happens; this is only the grade the caller turns
/// into that status.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum Outcome {
    /// Every operand and entry was listed.
    Complete,
    /// Something inside a listing could not be reached; it was reported and
    /// the listing continued. Exit `1`.
    MinorProblem,
    /// A command-line operand could not be reached; it was reported and
    /// skipped. Exit `2`.
    SeriousProblem,
}

impl Outcome {
    /// The exit status a caller reports for this outcome.
    #[must_use]
    pub const fn exit_status(self) -> i32 {
        match self {
            Self::Complete => 0,
            Self::MinorProblem => 1,
            Self::SeriousProblem => 2,
        }
    }

    /// Keep the more serious of two outcomes, so one bad entry in a long
    /// listing cannot be forgotten by a later good one.
    fn worse(self, other: Self) -> Self {
        if other > self {
            other
        } else {
            self
        }
    }
}

/// The `-> target` a long-format row shows for a symbolic link.
///
/// `resolved` is filled only when colour is active, which is the only thing
/// that needs it: GNU paints the target text in the role of what the target
/// *is*. A target that cannot be reached carries `None` and is painted
/// plain — the shared scheme names no orphan-link role, and inventing a
/// second colour vocabulary here would be worse than an uncoloured target.
struct LinkText {
    target: String,
    resolved: Option<Metadata>,
}

/// One row of a listing: the name, the kind the listing shows it as, the
/// metadata behind it, and — for a link — the target the long format prints.
///
/// `kind` is never unknown, because the directory stream reports every
/// child's own kind even when the per-entry stat is refused; that is what
/// lets an unstattable row still render its type letter rather than a `?`.
/// `stat` is absent when the listing needs no stat field at all *or* when
/// the stat was refused (a dangling link under `-L`); every cell it would
/// have filled then renders GNU's `?` rather than a fabricated zero. The one
/// constructor takes `kind` from the stat whenever there is one, so the two
/// cannot disagree.
struct Row {
    name: String,
    kind: FileKind,
    stat: Option<Metadata>,
    link: Option<LinkText>,
}

impl Row {
    /// A row for `name` whose stat succeeded.
    fn stated(name: String, meta: Metadata) -> Self {
        Self {
            name,
            kind: meta.kind,
            stat: Some(meta),
            link: None,
        }
    }

    /// A row for `name` with no stat behind it: either none was needed
    /// (`kind` then comes from the directory stream) or the stat was refused.
    fn unstated(name: String, kind: FileKind) -> Self {
        Self {
            name,
            kind,
            stat: None,
            link: None,
        }
    }

    /// This row with its long-format link target attached.
    fn with_link(mut self, link: Option<LinkText>) -> Self {
        self.link = link;
        self
    }

    /// The size a sort compares and the long format prints; `0` when no stat
    /// stands behind the row, which is what GNU compares for one it could
    /// not inspect.
    fn size(&self) -> u64 {
        self.stat.map_or(0, |meta| meta.size)
    }

    /// The on-disk allocation the `-s` cell and the `total` line sum; `0`
    /// without a stat, so an unstattable entry adds nothing to the total
    /// rather than a guess.
    fn allocated(&self) -> u64 {
        self.stat.map_or(0, |meta| meta.allocated)
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
    term: Option<&str>,
    fs: &dyn Listing,
    out: &dyn Output,
) -> Result<Outcome, LsError> {
    // The attested console width is the one signal that decides the GNU
    // arrangement, the quoting default, and (with `--color` and `TERM`) the
    // colour. It is read once here and reused for all three.
    let terminal = out.terminal_width();
    // The colour decision is made once, up front. When colour is active it
    // forces a per-entry `stat` (the kind and execute bit decide the
    // colour), exactly as the GNU tool stats when colouring.
    let painter = Painter::resolve(options.color, terminal, term);
    // The dereference posture is resolved once from the whole command line,
    // then applied differently to an operand and to an entry inside a
    // listing — the distinction `-H` exists to draw.
    let deref = options.dereference();
    let mut outcome = Outcome::Complete;
    let mut files: Vec<Row> = Vec::new();
    let mut dirs: Vec<(String, FileId)> = Vec::new();
    for path in paths {
        let meta = match operand_meta(path, deref, fs) {
            Ok(meta) => meta,
            // A command-line operand that cannot be reached is GNU's
            // "serious trouble": the reason is reported, the operand is
            // skipped, and the remaining operands are still listed.
            Err(errno) => {
                report(out, path, errno);
                outcome = outcome.worse(Outcome::SeriousProblem);
                continue;
            }
        };
        // Whether the operand's *contents* are what gets listed. Only a
        // directory has contents, and `-d` asks for the operand itself; a
        // link resolved to a directory arrives here already reporting
        // `Directory`, which is what makes `ls linkdir` list it.
        let contents = match meta.kind {
            FileKind::Directory => !options.directory,
            FileKind::Regular | FileKind::Symlink => false,
        };
        if contents {
            dirs.push((path.clone(), meta.id));
        } else {
            let row = Row::stated(path.clone(), meta);
            let link = link_text(path, &row, options, painter, fs);
            files.push(row.with_link(link));
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
    let format = resolve_format(options, terminal);
    let width = resolve_width(options, terminal);
    let quoting = Quoting::resolve(options, terminal);
    // The wall clock, quoting style, and colour painter are resolved once and
    // threaded together through rendering.
    let ctx = RenderCtx {
        now,
        quoting,
        painter,
    };
    let mut buf = String::new();
    let mut first = true;
    let mut hidden_omitted: u64 = 0;

    if !files.is_empty() {
        sort_rows(&mut files, options);
        open_block(&mut buf, &mut first, None);
        // No `total` line: the GNU tool totals directory listings only,
        // never the loose file-operand block.
        render_rows(&mut buf, &files, options, format, width, ctx);
        write_block(out, &mut buf)?;
    }
    let walk = Walk {
        options,
        filters,
        deref,
        format,
        width,
        headered,
        ctx,
    };
    let (walked, hidden) = list_directories(dirs, &walk, fs, out, &mut buf, &mut first)?;
    outcome = outcome.worse(walked);
    hidden_omitted += hidden;

    if hidden_omitted > 0 {
        emit_omission_record(out, hidden_omitted);
    }
    Ok(outcome)
}

/// Everything the directory walk needs that does not change between blocks.
struct Walk<'a> {
    options: Options,
    filters: &'a Filters,
    deref: Dereference,
    format: Format,
    width: usize,
    headered: bool,
    ctx: RenderCtx,
}

/// Walk `dirs` depth-first, rendering and **writing** one block per
/// directory, and report how completely it ran plus how many entries the
/// default dotfile filter hid.
///
/// Each block is written the moment its directory has been read, so a
/// recursive listing shows progress immediately and memory stays bounded by
/// the largest single directory rather than the whole tree.
fn list_directories(
    dirs: Vec<(String, FileId)>,
    walk: &Walk<'_>,
    fs: &dyn Listing,
    out: &dyn Output,
    buf: &mut String,
    first: &mut bool,
) -> Result<(Outcome, u64), LsError> {
    let Walk {
        options,
        filters,
        deref,
        format,
        width,
        headered,
        ctx,
    } = *walk;
    let mut outcome = Outcome::Complete;
    let mut hidden_omitted: u64 = 0;
    // A cycle needs a link, so the ancestor chain is only tracked when `-R`
    // recursion and a dereferencing posture can actually produce one.
    let track_cycles = options.recursive && deref == Dereference::Always;
    // A depth-first worklist: operands are pushed reversed so they pop in
    // command-line order, and a listed directory's children are pushed
    // reversed so they pop in rendered order.
    let mut dirs = dirs;
    dirs.reverse();
    let mut pending: Vec<Pending> = dirs
        .into_iter()
        .map(|(path, id)| Pending {
            path,
            chain: if track_cycles {
                alloc::vec![id]
            } else {
                Vec::new()
            },
            operand: true,
        })
        .collect();
    while let Some(item) = pending.pop() {
        let Pending {
            path,
            chain,
            operand,
        } = item;
        let mut entries = match fs.read_dir(&path) {
            Ok(entries) => entries,
            // An unreadable directory is reported and skipped: a listing of
            // twenty operands must not be lost to the one the caller may not
            // open. A command-line operand is serious trouble; a directory
            // reached by recursing is a minor problem.
            Err(errno) => {
                report_unreadable(out, &path, errno);
                outcome = outcome.worse(if operand {
                    Outcome::SeriousProblem
                } else {
                    Outcome::MinorProblem
                });
                continue;
            }
        };
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
        let listed = rows_for(&path, &entries, options, deref, ctx.painter, fs, out);
        let mut rows = listed.rows;
        outcome = outcome.worse(listed.outcome);
        sort_rows(&mut rows, options);
        open_block(buf, first, headered.then_some(path.as_str()));
        if options.size || options.is_long() {
            render_total(buf, &rows, options);
        }
        render_rows(buf, &rows, options, format, width, ctx);
        write_block(out, buf)?;
        if options.recursive {
            for row in rows.iter().rev() {
                // `.`/`..` never recurse — a listing must terminate even
                // when `-a` renders them — and a row shown *as* a link is
                // not descended: only a posture that already resolved it
                // reports `Directory` here.
                let descend = match row.kind {
                    FileKind::Directory => row.name != "." && row.name != "..",
                    FileKind::Regular | FileKind::Symlink => false,
                };
                if !descend {
                    continue;
                }
                let child = join(&path, &row.name);
                let id = row.stat.map_or(FileId::NONE, |meta| meta.id);
                // A directory already on the chain that led here was reached
                // through a link pointing back at it; report it once and do
                // not descend, exactly as GNU does, rather than walking the
                // loop until the path outgrows the kernel's bound.
                if track_cycles && !id.is_none() && chain.contains(&id) {
                    report_cycle(out, &child);
                    outcome = outcome.worse(Outcome::MinorProblem);
                    continue;
                }
                let mut child_chain = chain.clone();
                if track_cycles {
                    child_chain.push(id);
                }
                pending.push(Pending {
                    path: child,
                    chain: child_chain,
                    operand: false,
                });
            }
        }
    }
    Ok((outcome, hidden_omitted))
}

/// One directory still to be listed.
struct Pending {
    path: String,
    /// Node identities of the directories this one was reached through,
    /// starting at the operand — empty unless a cycle is possible at all
    /// (`-R` with `-L`).
    ///
    /// A cycle requires a symbolic link, and every format that stores one
    /// reports node identities, so a tracked chain is never made of
    /// unusable [`FileId::NONE`]s in practice; an identity-less entry is
    /// simply not compared.
    chain: Vec<FileId>,
    /// Whether the directory was named on the command line, which decides
    /// how serious a failure to read it is.
    operand: bool,
}

/// The [`Metadata`] a *command-line operand* is described by, applying the
/// GNU dereference rule for operands.
///
/// [`Dereference::Always`] and [`Dereference::CommandLine`] resolve the
/// operand outright, so a dangling one is the error `stat(2)` reports.
/// [`Dereference::CommandLineDirectory`] resolves it *only* if that yields a
/// directory — which is what makes `ls linkdir` list the directory while
/// `ls dangling` and `ls linkfile` still describe the link — and
/// [`Dereference::Never`] always describes the operand itself.
fn operand_meta(path: &str, deref: Dereference, fs: &dyn Listing) -> Result<Metadata, Errno> {
    match deref {
        Dereference::Never => fs.stat(path, FinalLink::Keep),
        Dereference::Always | Dereference::CommandLine => fs.stat(path, FinalLink::Follow),
        Dereference::CommandLineDirectory => match fs.stat(path, FinalLink::Follow) {
            Ok(meta) if meta.kind.is_dir() => Ok(meta),
            // Resolved to a non-directory, or dangled: describe the operand
            // itself. Any *other* refusal is the operand's real answer and
            // is reported as it stands.
            Ok(_) | Err(Errno::NotFound) => fs.stat(path, FinalLink::Keep),
            Err(errno) => Err(errno),
        },
    }
}

/// Report that a path could not be inspected, in the GNU shape.
fn report(out: &dyn Output, path: &str, errno: Errno) {
    out.error(&format!("{OWN_WORD}: cannot access '{path}': {errno}"));
}

/// Report that a directory could not be read, in the GNU shape.
fn report_unreadable(out: &dyn Output, path: &str, errno: Errno) {
    out.error(&format!(
        "{OWN_WORD}: cannot open directory '{path}': {errno}"
    ));
}

/// Report a directory `-R` reached again through a link back into its own
/// chain, in the GNU shape.
fn report_cycle(out: &dyn Output, path: &str) {
    out.error(&format!(
        "{OWN_WORD}: not listing already-listed directory: '{path}'"
    ));
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
/// compares those fields, `-F` needs the mode's execute bits for `*`, active
/// colour needs them for the executable role, and a dereferencing posture
/// needs one because the directory stream reports a link's *own* kind —
/// everything else renders names straight off the one `read_dir`, so the
/// per-entry `stat` is paid only when asked for.
fn needs_stat(options: Options, entry_links: FinalLink, color_active: bool) -> bool {
    options.is_long()
        || options.size
        || options.inode
        || options.sort == Sort::Size
        || options.sort == Sort::Time
        || options.indicator == Indicator::Classify
        || color_active
        || entry_links == FinalLink::Follow
}

/// The reading a listing's *entries* take: only `-L` resolves them; `-H`
/// deliberately stops at the command line.
fn entry_links(deref: Dereference) -> FinalLink {
    match deref {
        Dereference::Always => FinalLink::Follow,
        Dereference::Never | Dereference::CommandLine | Dereference::CommandLineDirectory => {
            FinalLink::Keep
        }
    }
}

/// One directory block's rows, and how completely they could be inspected.
struct Listed {
    rows: Vec<Row>,
    outcome: Outcome,
}

/// The rendered rows of one directory block: each entry's name, with its
/// metadata and — for a link the long format prints — its target attached.
///
/// An entry that cannot be inspected is *kept*: the reason is reported, the
/// row renders its type letter from the directory stream and `?` for every
/// stat-derived cell, and the outcome records the minor problem. That is what
/// `ls -L` over a directory holding a dangling link must do — report it and
/// list the rest.
fn rows_for(
    dir: &str,
    entries: &[Entry],
    options: Options,
    deref: Dereference,
    painter: Painter,
    fs: &dyn Listing,
    out: &dyn Output,
) -> Listed {
    let links = entry_links(deref);
    let wanted = needs_stat(options, links, painter.is_active());
    let mut rows = Vec::with_capacity(entries.len());
    let mut outcome = Outcome::Complete;
    for entry in entries {
        let path = join(dir, &entry.name);
        let row = if wanted {
            match fs.stat(&path, links) {
                Ok(meta) => Row::stated(entry.name.clone(), meta),
                Err(errno) => {
                    report(out, &path, errno);
                    outcome = outcome.worse(Outcome::MinorProblem);
                    Row::unstated(entry.name.clone(), entry.kind)
                }
            }
        } else {
            // No cell in this listing reads a stat field, so none is paid
            // for; the kind from the directory stream is all it needs.
            Row::unstated(entry.name.clone(), entry.kind)
        };
        let link = link_text(&path, &row, options, painter, fs);
        rows.push(row.with_link(link));
    }
    Listed { rows, outcome }
}

/// The `-> target` a long-format row shows, for a row the listing shows *as*
/// a symbolic link.
///
/// Reading a link's target is a separate call because a link's content is a
/// path, never bytes; it is paid only for the format that prints it. When
/// colour is active the target is additionally resolved so its text is
/// painted in the role of what it names — GNU's behaviour — and a target
/// that cannot be reached simply carries no role.
fn link_text(
    path: &str,
    row: &Row,
    options: Options,
    painter: Painter,
    fs: &dyn Listing,
) -> Option<LinkText> {
    if !options.is_long() || row.kind != FileKind::Symlink {
        return None;
    }
    let target = fs.read_link(path).ok()?;
    let resolved = painter
        .is_active()
        .then(|| fs.stat(path, FinalLink::Follow).ok())
        .flatten();
    Some(LinkText { target, resolved })
}

/// The timestamp `options` selects for the long-format date column and the
/// `-t` sort: modified (the default), accessed (`-u`), changed (`-c`), or
/// created (`--time=birth`).
///
/// A row with no stat behind it has no timestamp at all, and reports the
/// epoch — what GNU compares for an entry it could not inspect — rather than
/// a fabricated one; the long format still renders it as `?`.
fn selected_time(row: &Row, field: TimeField) -> Time64 {
    let Some(meta) = row.stat else {
        return Time64::UNIX_EPOCH;
    };
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
fn sort_rows(rows: &mut [Row], options: Options) {
    match options.sort {
        // No sort: keep the directory (read) order the filesystem returned.
        Sort::None => {}
        Sort::Name => rows.sort_by(|a, b| a.name.cmp(&b.name)),
        // Largest first, ties by name — the GNU `-S` order.
        Sort::Size => {
            rows.sort_by(|a, b| b.size().cmp(&a.size()).then_with(|| a.name.cmp(&b.name)));
        }
        // Newest first, ties by name — the GNU `-t` order.
        Sort::Time => rows.sort_by(|a, b| {
            selected_time(b, options.time_field)
                .cmp(&selected_time(a, options.time_field))
                .then_with(|| a.name.cmp(&b.name))
        }),
        // By extension, ties by name — the GNU `-X` order.
        Sort::Extension => rows.sort_by(|a, b| {
            extension(&a.name)
                .cmp(extension(&b.name))
                .then_with(|| a.name.cmp(&b.name))
        }),
        // Natural version order, ties by name — the GNU `-v` order.
        Sort::Version => rows.sort_by(|a, b| {
            version::filevercmp(a.name.as_bytes(), b.name.as_bytes())
                .then_with(|| a.name.cmp(&b.name))
        }),
    }
    if options.reverse {
        rows.reverse();
    }
    // Directories first is applied *after* the sort and the reverse, as a
    // stable partition: it keeps the sorted order within each group and puts
    // directories first regardless of `-r` — the GNU behaviour. A row shown
    // as a link is not a directory here however its target resolves; only a
    // posture that resolved it already reports `Directory`.
    if options.group_directories_first {
        rows.sort_by_key(|row| Reverse(row.kind.is_dir()));
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

/// The character that ends each output line: NUL under `--zero`, else the
/// newline. Headers and the inter-block separator always use the newline;
/// only entry lines and the `total` line follow this, matching the GNU
/// `--zero` layout.
fn line_terminator(options: Options) -> char {
    if options.zero {
        '\0'
    } else {
        '\n'
    }
}

/// Render `bytes` in the given [`SizeFormat`]: the scaled, rounded-up count
/// with any unit suffix, or the human autoscaled form. The one place a byte
/// count becomes a displayed size, so the `-s` cells, the `total` line, and
/// the long-format file-size column never diverge.
fn render_size(format: SizeFormat, bytes: u64) -> String {
    match format {
        SizeFormat::Scaled { unit, suffix } => {
            // `unit` is never zero (rejected at parse time); guard anyway so
            // a future caller can never divide by zero.
            let count = bytes.div_ceil(unit.max(1));
            match suffix {
                Some(suffix) => format!("{count}{}", suffix.text()),
                None => format!("{count}"),
            }
        }
        SizeFormat::Human { si } => human_size(bytes, if si { 1000 } else { 1024 }),
    }
}

/// The `-s` blocks cell for one row: its allocated storage rendered in the
/// [`block_size`](Options::block_size) scaling, or `?` when no stat stands
/// behind the row.
fn blocks_cell(row: &Row, options: Options) -> String {
    match row.stat {
        Some(meta) => render_size(options.block_size, meta.allocated),
        None => String::from(UNKNOWN_CELL),
    }
}

/// What every stat-derived cell renders when the stat was refused: GNU's
/// single `?`, never a fabricated zero.
const UNKNOWN_CELL: &str = "?";

/// The `total` line of one directory block, printed for every directory
/// listing under `-l` or `-s` as in the GNU tool: the summed allocated bytes
/// of the block's entries rendered once in the
/// [`block_size`](Options::block_size) scaling (GNU sums the raw block
/// counts, then scales, so the total is the scaling of the sum — not the sum
/// of the per-entry cells).
fn render_total(buf: &mut String, rows: &[Row], options: Options) {
    let total: u64 = rows.iter().map(Row::allocated).sum();
    let _ = write!(buf, "total {}", render_size(options.block_size, total));
    buf.push(line_terminator(options));
}

/// The conventional fallback output width when no width is given and the
/// console cannot be attested (the GNU default).
const DEFAULT_WIDTH: usize = 80;

/// The blank columns between two grid columns (the GNU column gap).
const COLUMN_GAP: usize = 2;

/// Resolve the effective arrangement for one listing. An explicit
/// `-l`/`-1`/`-C`/`-x`/`-m` (or `--format`) wins; `--zero` defaults to a
/// single column; otherwise the GNU default is multiple columns when
/// standard output is an attested terminal and one name per line when it is
/// not (a pipe, a file, or an unattested console).
fn resolve_format(options: Options, terminal: Option<usize>) -> Format {
    options.format.unwrap_or({
        if options.zero || terminal.is_none() {
            Format::OnePerLine
        } else {
            Format::Columns
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

/// The presentation decisions resolved once per listing and threaded through
/// rendering together: the wall clock (for the relative-date window), the
/// name-quoting style, and the colour painter. Bundling them keeps the render
/// entry point's argument list small and guarantees every arrangement sees the
/// same decisions.
#[derive(Clone, Copy)]
struct RenderCtx {
    now: Time64,
    quoting: Quoting,
    painter: Painter,
}

/// Render `rows` into `buf` in the resolved `format`, wrapping column and
/// comma arrangements to `width`. The long format (`-l`) ignores `format`
/// and `width` — it is always one entry per line.
fn render_rows(
    buf: &mut String,
    rows: &[Row],
    options: Options,
    format: Format,
    width: usize,
    ctx: RenderCtx,
) {
    let RenderCtx {
        now,
        quoting,
        painter,
    } = ctx;
    match format {
        Format::Long => render_long(buf, rows, options, now, quoting, painter),
        Format::OnePerLine => render_one_per_line(buf, rows, options, quoting, painter),
        Format::Columns => {
            render_grid(
                buf,
                rows,
                options,
                width,
                Fill::TopToBottom,
                quoting,
                painter,
            );
        }
        Format::Across => {
            render_grid(
                buf,
                rows,
                options,
                width,
                Fill::LeftToRight,
                quoting,
                painter,
            );
        }
        Format::Commas => render_commas(buf, rows, options, width, quoting, painter),
    }
}

/// A rendered listing cell: the bytes to emit (which may carry colour SGR
/// sequences) and the cell's *plain* display width — the width the terminal
/// actually shows, with any zero-width escape sequences excluded.
///
/// Keeping the width beside the text is what lets colour be byte-identical to
/// the plain render apart from the SGR sequences: every column-layout
/// calculation reads [`RenderedCell::width`], never `str_width` over the
/// possibly-coloured [`RenderedCell::text`], so escape bytes never shift a
/// column.
struct RenderedCell {
    text: String,
    width: usize,
}

/// The rendered form of one entry as it appears in a listing cell: its
/// `-i` inode prefix (right-aligned to `inode_width`) then its `-s`
/// allocated-blocks prefix (right-aligned to `blocks_width`), each `0` for
/// no prefix, followed by the decorated (and, under colour, painted) name.
/// The inode precedes the blocks, as in the GNU tool.
fn entry_cell(
    row: &Row,
    options: Options,
    inode_width: usize,
    blocks_width: usize,
    quoting: Quoting,
    painter: Painter,
) -> RenderedCell {
    let mut text = String::new();
    // Writing into a `String` is infallible, so the `fmt::Result`s are
    // discarded deliberately.
    if options.inode {
        let _ = write!(text, "{:>inode_width$} ", inode_cell(row));
    }
    if options.size {
        let _ = write!(text, "{:>blocks_width$} ", blocks_cell(row, options));
    }
    // The prefixes are plain ASCII, so their display width is their length.
    let prefix_width = str_width(&text);
    let name_cell = decorate(row, options, quoting, painter);
    text.push_str(&name_cell.text);
    RenderedCell {
        text,
        width: prefix_width + name_cell.width,
    }
}

/// The `-i` node-number cell for one row, or `?` when no stat stands behind
/// it.
fn inode_cell(row: &Row) -> String {
    match row.stat {
        Some(meta) => format!("{}", meta.id.node),
        None => String::from(UNKNOWN_CELL),
    }
}

/// Render one entry per line — the `-1` arrangement and the non-terminal
/// default. Each line carries its `-s` blocks cell, right-aligned to the
/// block's width, when `-s` is set.
fn render_one_per_line(
    buf: &mut String,
    rows: &[Row],
    options: Options,
    quoting: Quoting,
    painter: Painter,
) {
    let inode_width = inode_column_width(rows, options);
    let blocks_width = size_column_width(rows, options);
    for row in rows {
        // The name is last on the line, so its (possibly coloured) text is
        // appended verbatim — no column depends on its width here.
        buf.push_str(&entry_cell(row, options, inode_width, blocks_width, quoting, painter).text);
        buf.push(line_terminator(options));
    }
}

/// The `-m` comma arrangement: names separated by `, `, wrapped so no line
/// exceeds `width`. The comma stays at the end of a full line and the next
/// line begins with the name, no leading space — the GNU `-m` layout.
fn render_commas(
    buf: &mut String,
    rows: &[Row],
    options: Options,
    width: usize,
    quoting: Quoting,
    painter: Painter,
) {
    if rows.is_empty() {
        return;
    }
    // `-m` does not pad the `-i` inode or `-s` blocks cell (GNU prints them
    // inline), so the cell is built with zero prefix widths.
    let mut pos = 0usize;
    for (index, row) in rows.iter().enumerate() {
        let cell = entry_cell(row, options, 0, 0, quoting, painter);
        // Wrap on the cell's plain display width, never the escape bytes.
        let len = cell.width;
        if index > 0 {
            // Keep the entry on the current line only if it and the `, `
            // separator still fit; otherwise break after the comma.
            if pos.saturating_add(len).saturating_add(COLUMN_GAP) < width {
                pos += COLUMN_GAP;
                buf.push_str(", ");
            } else {
                pos = 0;
                buf.push(',');
                buf.push(line_terminator(options));
            }
        }
        buf.push_str(&cell.text);
        pos += len;
    }
    buf.push(line_terminator(options));
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
    rows: &[Row],
    options: Options,
    width: usize,
    fill: Fill,
    quoting: Quoting,
    painter: Painter,
) {
    let count = rows.len();
    if count == 0 {
        return;
    }
    let inode_width = inode_column_width(rows, options);
    let blocks_width = size_column_width(rows, options);
    let cells: Vec<RenderedCell> = rows
        .iter()
        .map(|row| entry_cell(row, options, inode_width, blocks_width, quoting, painter))
        .collect();
    // The grid is laid out on each cell's *plain* display width, so a
    // coloured cell occupies exactly the columns its uncoloured form would —
    // the escape bytes are zero-width padding the layout never sees.
    let widths: Vec<usize> = cells.iter().map(|cell| cell.width).collect();

    let layout = grid_layout(&widths, width, fill);
    for row in 0..layout.rows {
        // The last present column index in this row decides which cell ends
        // the line (and so carries no trailing pad).
        let last_col = (0..layout.cols)
            .rev()
            .find(|&col| cell_index(fill, row, col, layout.rows, layout.cols) < count);
        let Some(last_col) = last_col else { continue };
        // Present columns in a row are contiguous from 0 to `last_col` in
        // both fill directions, so every index below is in range. `pos`
        // tracks the current column position so the gap to the next column
        // is advanced with tabs (up to `-T` tab stops) exactly as GNU does.
        let mut pos = 0usize;
        for col in 0..=last_col {
            let index = cell_index(fill, row, col, layout.rows, layout.cols);
            buf.push_str(&cells[index].text);
            pos += widths[index];
            if col != last_col {
                let target = pos - widths[index] + layout.col_widths[col] + COLUMN_GAP;
                indent(buf, pos, target, options.tabsize);
                pos = target;
            }
        }
        buf.push(line_terminator(options));
    }
}

/// Advance from column `from` to column `to`, emitting a tab whenever it
/// lands the position at or past the next tab stop and a space otherwise — a
/// direct port of the GNU `ls` `indent` routine. A `tabsize` of `0` pads
/// with spaces only (`-T0`).
fn indent(buf: &mut String, mut from: usize, to: usize, tabsize: usize) {
    while from < to {
        if tabsize != 0 && to / tabsize > (from + 1) / tabsize {
            buf.push('\t');
            from += tabsize - from % tabsize;
        } else {
            buf.push(' ');
            from += 1;
        }
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
fn inode_column_width(rows: &[Row], options: Options) -> usize {
    if !options.inode {
        return 0;
    }
    rows.iter()
        .map(|row| inode_cell(row).len())
        .max()
        .unwrap_or(1)
}

/// Width of the widest `-s` blocks cell in `rows` (0 when `-s` is off).
fn size_column_width(rows: &[Row], options: Options) -> usize {
    if !options.size {
        return 0;
    }
    rows.iter()
        .map(|row| blocks_cell(row, options).len())
        .max()
        .unwrap_or(1)
}

/// Render the long format: the optional `-i` inode and `-s` blocks columns,
/// then mode, link count, numeric owner and group (unless hidden by `-g` /
/// `-o`), size, the selected timestamp, and finally the decorated name —
/// followed by ` -> target` for a row shown as a symbolic link — with each
/// numeric column right-aligned and the date column padded so the names
/// align.
///
/// Owner and group are numeric ids: resolving names needs the
/// capability-gated user database, which a listing must not demand — the
/// GNU tool falls back to numbers for exactly this case (`-n` renders the
/// same). The link count is the filesystem's own record, reported by the
/// driver and never derived here. The timestamp column shows the time
/// selected by `-c` / `-u` / `--time` (modified by default), rendered in the
/// style chosen by `--time-style` / `--full-time`.
///
/// A row whose stat was refused renders every stat-derived cell as `?` and
/// keeps the type letter the directory stream gave it, exactly as GNU does —
/// which is how `ls -lL` shows a dangling link it has already reported.
fn render_long(
    buf: &mut String,
    rows: &[Row],
    options: Options,
    now: Time64,
    quoting: Quoting,
    painter: Painter,
) {
    let size_cell = |row: &Row| match row.stat {
        Some(meta) => render_size(options.file_size, meta.size),
        None => String::from(UNKNOWN_CELL),
    };
    let date_cell = |row: &Row| match row.stat {
        Some(_) => render_time(
            selected_time(row, options.time_field),
            options.time_style,
            now,
        ),
        None => String::from(UNKNOWN_CELL),
    };
    let links_cell = |row: &Row| match row.stat {
        Some(meta) => format!("{}", meta.nlink),
        None => String::from(UNKNOWN_CELL),
    };
    let owner_cell = |row: &Row| match row.stat {
        Some(meta) => format!("{}", meta.uid),
        None => String::from(UNKNOWN_CELL),
    };
    let group_cell = |row: &Row| match row.stat {
        Some(meta) => format!("{}", meta.gid),
        None => String::from(UNKNOWN_CELL),
    };
    let width_of =
        |cell: &dyn Fn(&Row) -> String| rows.iter().map(|row| cell(row).len()).max().unwrap_or(1);
    let links_width = width_of(&links_cell);
    let uid_width = width_of(&owner_cell);
    let gid_width = width_of(&group_cell);
    // The `--author` column repeats the owning user (TAIRiX has no separate
    // author), so it is the same values and width as the owner column.
    let author_width = uid_width;
    let size_width = width_of(&size_cell);
    // The date column is padded to its widest rendering so the names align;
    // within one style every row is the same width except `iso`, whose
    // recent and old forms differ.
    let date_width = rows
        .iter()
        .map(|row| date_cell(row).len())
        .max()
        .unwrap_or(0);
    let inode_width = inode_column_width(rows, options);
    let blocks_width = size_column_width(rows, options);
    for row in rows {
        // Writing into a `String` is infallible, so the `fmt::Result` is
        // discarded deliberately.
        if options.inode {
            let _ = write!(buf, "{:>inode_width$} ", inode_cell(row));
        }
        if options.size {
            let _ = write!(buf, "{:>blocks_width$} ", blocks_cell(row, options));
        }
        let _ = write!(buf, "{}", mode_string(row));
        let _ = write!(buf, " {:>links_width$}", links_cell(row));
        if !options.hide_owner {
            let _ = write!(buf, " {:>uid_width$}", owner_cell(row));
        }
        if options.author {
            let _ = write!(buf, " {:>author_width$}", owner_cell(row));
        }
        if !options.hide_group {
            let _ = write!(buf, " {:>gid_width$}", group_cell(row));
        }
        // The name is the last field on the line, so its (possibly coloured)
        // text is emitted verbatim — no column follows it to be shifted.
        let _ = write!(
            buf,
            " {:>size_width$} {:<date_width$} {}",
            size_cell(row),
            date_cell(row),
            decorate(row, options, quoting, painter).text
        );
        // `name -> target` for a link: the target is quoted in the same style
        // as the name and painted in the role of what it names, and the
        // indicator suffix (already appended above) stays on the *link*, as
        // in the GNU tool.
        if let Some(link) = &row.link {
            let target = quoting.render(&link.target);
            let role = link.resolved.as_ref().and_then(role_of);
            let _ = write!(buf, " -> {}", painter.paint(role, &target));
        }
        buf.push(line_terminator(options));
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
        TimeStyle::LongIso => tairix_fsmeta::calendar::iso_minute(&civil),
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

/// The rendered form of one row's name: quoted in the resolved [`Quoting`]
/// style and, when colour is active, painted in its kind's scheme role; the
/// `-p` / `-F` / `--file-type` indicator suffix is appended *after* the
/// closing quote (and after the colour reset), uncoloured, exactly as in the
/// GNU tool. A directory carries `/`, a symbolic link carries `@` under
/// `--file-type` and `-F`, and `-F` additionally marks executables with `*`.
///
/// The returned [`RenderedCell`] carries the plain display width — the
/// quoted name plus any suffix — so colour never shifts a column.
fn decorate(row: &Row, options: Options, quoting: Quoting, painter: Painter) -> RenderedCell {
    let quoted = quoting.render(&row.name);
    let mut width = str_width(&quoted);
    // Colour wraps only the name text; the indicator suffix is appended
    // afterwards, outside the colour, as the GNU tool does by default.
    let mut text = painter.paint(role_for(row), &quoted);
    let suffix = match options.indicator {
        Indicator::None => None,
        // `-p` marks directories only; `--file-type` marks every kind it can
        // name, which here adds the link's `@`.
        Indicator::Slash => match row.kind {
            FileKind::Directory => Some('/'),
            FileKind::Regular | FileKind::Symlink => None,
        },
        Indicator::FileType => match row.kind {
            FileKind::Directory => Some('/'),
            FileKind::Symlink => Some('@'),
            FileKind::Regular => None,
        },
        Indicator::Classify => match row.kind {
            FileKind::Directory => Some('/'),
            FileKind::Symlink => Some('@'),
            // The execute bit needs a stat; a row without one is left
            // unmarked rather than guessed at.
            FileKind::Regular => row.stat.filter(|meta| meta.mode & 0o111 != 0).map(|_| '*'),
        },
    };
    if let Some(suffix) = suffix {
        text.push(suffix);
        width += 1;
    }
    RenderedCell { text, width }
}

/// The scheme [`Role`] a row's name is painted in, or [`None`] for a plain
/// (uncoloured) regular file.
///
/// A row shown as a link takes the link role whatever its target is — the
/// name on the line *is* the link. A row with no stat behind it can only be
/// coloured by its kind, which is exactly what a dangling link under `-L`
/// leaves to work with.
fn role_for(row: &Row) -> Option<Role> {
    match row.kind {
        FileKind::Directory => Some(Role::Directory),
        FileKind::Symlink => Some(Role::Link),
        FileKind::Regular => row.stat.as_ref().and_then(role_of),
    }
}

/// The scheme [`Role`] a stat'd node's own kind and mode name — the role the
/// long format paints a link's *target* text in, where the node behind the
/// name is all there is to go on.
fn role_of(meta: &Metadata) -> Option<Role> {
    match meta.kind {
        FileKind::Directory => Some(Role::Directory),
        FileKind::Symlink => Some(Role::Link),
        FileKind::Regular => (meta.mode & 0o111 != 0).then_some(Role::Executable),
    }
}

/// The resolved colour decision for one listing: the depth to render at, or
/// [`None`] for plain output. Built once from `--color`, the attested
/// console, and `TERM`, then threaded through rendering so the policy lives
/// in one place and every cell decides identically.
#[derive(Clone, Copy)]
struct Painter {
    depth: Option<ColorDepth>,
}

impl Painter {
    /// Resolve the colour decision through the one shared policy: the
    /// `--color` choice, whether standard output is an attested terminal, and
    /// the `TERM` value. A [`None`] depth means plain output.
    fn resolve(choice: ColorChoice, terminal: Option<usize>, term: Option<&str>) -> Self {
        Self {
            depth: choice.resolve(terminal.is_some(), term),
        }
    }

    /// Whether colour is active, so the caller forces the per-entry `stat`
    /// the executable role needs.
    fn is_active(self) -> bool {
        self.depth.is_some()
    }

    /// Wrap `text` in the SGR sequences for `role`, or return it unchanged
    /// when colour is off, the role is plain, or the role names no colour.
    ///
    /// The role's ideal colour is degraded to the terminal's depth through
    /// the one shared `downgrade`, so a truecolour scheme entry still renders
    /// on a 16-colour terminal and none is emitted a terminal cannot show.
    fn paint(self, role: Option<Role>, text: &str) -> String {
        let (Some(depth), Some(role)) = (self.depth, role) else {
            return String::from(text);
        };
        let mut style = role.style();
        style.foreground = downgrade(style.foreground, depth);
        if style.is_plain() {
            return String::from(text);
        }
        // The SGR bytes are ASCII, so the assembled buffer is valid UTF-8.
        let mut bytes = Vec::new();
        let (sgrs, count) = style.open();
        for &sgr in &sgrs[..count] {
            encode_into(&Op::Sgr(sgr), &mut bytes);
        }
        bytes.extend_from_slice(text.as_bytes());
        encode_into(&Op::Sgr(Sgr::Reset), &mut bytes);
        String::from_utf8(bytes).unwrap_or_else(|_| String::from(text))
    }
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
    /// either axis. `--zero` forces the `literal` / show-control defaults (the
    /// GNU `--zero` posture), still overridable by an explicit quoting flag.
    fn resolve(options: Options, terminal: Option<usize>) -> Self {
        let at_terminal = terminal.is_some() && !options.zero;
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

/// `size` in the GNU human-readable form for the given `base`: plain bytes
/// below one unit, then powers of `base` rounded up — one decimal place
/// below ten, whole numbers from ten. `base` is 1024 for `-h` (units `K`,
/// `M`, …) or 1000 for `--si` (units `k`, `M`, …); only the kilo letter
/// differs between the two, matching GNU.
fn human_size(size: u64, base: u64) -> String {
    const UNITS_1024: [char; 6] = ['K', 'M', 'G', 'T', 'P', 'E'];
    const UNITS_1000: [char; 6] = ['k', 'M', 'G', 'T', 'P', 'E'];
    let units = if base == 1000 { UNITS_1000 } else { UNITS_1024 };
    let base = u128::from(base);
    let size = u128::from(size);
    if size < base {
        return format!("{size}");
    }
    let mut unit = 0;
    let mut scale: u128 = base;
    while unit + 1 < units.len() && size >= scale * base {
        scale *= base;
        unit += 1;
    }
    // Tenths of a unit, rounded up (the GNU ceiling), e.g. 1025 -> `1.1K`.
    let tenths = (size * 10).div_ceil(scale);
    if tenths < 100 {
        format!("{}.{}{}", tenths / 10, tenths % 10, units[unit])
    } else {
        format!("{}{}", size.div_ceil(scale), units[unit])
    }
}

/// The ten-character long-format mode string, e.g. `drwxr-xr-x`.
///
/// The permission-bit spelling is the one shared `tairix_abi::fs::mode_string`
/// definition, so `ls -l` and the file manager's properties view never
/// disagree on what a mode means; the bytes it returns are always ASCII.
///
/// A row with no stat behind it keeps its type letter — which the directory
/// stream gave it — and renders the nine permission characters as `?`, the
/// GNU spelling for a mode it could not read.
fn mode_string(row: &Row) -> String {
    let mode = row.stat.map_or(0, |meta| meta.mode);
    let spelling = tairix_abi::fs::mode_string(row.kind, mode);
    spelling
        .iter()
        .enumerate()
        .map(|(index, &byte)| {
            if index == 0 || row.stat.is_some() {
                byte as char
            } else {
                '?'
            }
        })
        .collect()
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
    use super::{run, Outcome, USAGE};
    use crate::command::{
        Command, Dereference, Filters, Format, Hidden, Indicator, Options, QuotingStyle,
        SizeFormat, Sort, TimeField, TimeStyle,
    };
    use crate::error::LsError;
    use crate::io::{Entry, FinalLink, Listing, Metadata, Output};
    use alloc::format;
    use alloc::string::{String, ToString};
    use alloc::vec::Vec;
    use core::cell::RefCell;
    use tairix_abi::fs::FileKind;
    use tairix_abi::time::Time64;
    use tairix_abi::{Errno, NodeTimes};
    use tairix_glob::Pattern;
    use tairix_help::{HelpSource, SourceError};
    use tairix_termcap::ColorChoice;

    /// An in-memory tree: a table of each path's **own** (`lstat`) metadata,
    /// the stored target of every path that is a link, and — for directories
    /// — the entries that path's `read_dir` returns.
    ///
    /// Following a link is done here, by walking the stored target under a
    /// hop bound, so the fixture answers a `Follow` stat exactly as the VFS
    /// would: the target's metadata, or `NotFound` when it dangles.
    struct TreeFs {
        stat: Vec<(String, Metadata)>,
        links: Vec<(String, String)>,
        dirs: Vec<(String, Vec<Entry>)>,
        /// The next node number a declared path is given, so two nodes of
        /// the fixture are as distinguishable as two nodes of a real volume
        /// — which is what the `-R` chain check compares.
        next_node: u64,
    }

    /// The fixture's hop bound, standing in for the VFS `SYMLINK_HOP_MAX`: a
    /// cycle is refused rather than walked.
    const FIXTURE_HOPS: u32 = 8;

    impl TreeFs {
        fn new() -> Self {
            Self {
                stat: Vec::new(),
                links: Vec::new(),
                dirs: Vec::new(),
                next_node: 1,
            }
        }

        /// The next distinct node identity.
        fn fresh_id(&mut self) -> tairix_abi::fs::FileId {
            let node = self.next_node;
            self.next_node += 1;
            node_id(node)
        }

        /// A link *entry* named `name` inside the declared directory `dir`,
        /// storing `target` verbatim (absolute, or relative to `dir`).
        fn link_entry(mut self, dir: &str, name: &str, target: &str) -> Self {
            let children = self
                .dirs
                .iter_mut()
                .find(|(d, _)| d == dir)
                .map(|(_, c)| c)
                .expect("directory must be declared before its entries");
            children.push(Entry {
                name: name.to_string(),
                kind: FileKind::Symlink,
            });
            self.link(&super::join(dir, name), target)
        }

        /// A link at `path` storing `target`, without listing it anywhere —
        /// the operand form.
        fn link(mut self, path: &str, target: &str) -> Self {
            let size = target.len() as u64;
            let id = self.fresh_id();
            self.stat.push((
                path.to_string(),
                Metadata {
                    // A link's mode is POSIX's `lrwxrwxrwx`.
                    kind: FileKind::Symlink,
                    nlink: 1,
                    mode: 0o777,
                    size,
                    allocated: size,
                    uid: UID,
                    gid: GID,
                    id,
                    times: NodeTimes::default(),
                },
            ));
            self.links.push((path.to_string(), target.to_string()));
            self
        }

        /// The metadata stored under the exact spelling `path`, if any.
        fn exact(&self, path: &str) -> Option<Metadata> {
            self.stat.iter().find(|(p, _)| p == path).map(|(_, m)| *m)
        }

        /// The target stored under the exact spelling `path`, if it is a
        /// link.
        fn exact_target(&self, path: &str) -> Option<String> {
            self.links
                .iter()
                .find(|(p, _)| p == path)
                .map(|(_, t)| t.clone())
        }

        /// `path` with every *interior* link resolved — the spelling the VFS
        /// reaches the final component through, which is why `to-dir/inner`
        /// finds `/sub/inner`. The final component is left as typed.
        fn interior(&self, path: &str, hops: u32) -> String {
            if hops == 0 || self.exact(path).is_some() {
                return String::from(path);
            }
            let parent = parent_of(path);
            let leaf = path.rsplit('/').next().unwrap_or(path);
            if parent == path || leaf.is_empty() {
                return String::from(path);
            }
            let real_parent = self.follow(parent, hops - 1);
            if real_parent == parent {
                return String::from(path);
            }
            self.interior(&super::join(&real_parent, leaf), hops - 1)
        }

        /// `path` with every link resolved, the final one included.
        fn follow(&self, path: &str, hops: u32) -> String {
            let here = self.interior(path, hops);
            if hops == 0 {
                return here;
            }
            match self.exact(&here) {
                Some(meta) if meta.kind == FileKind::Symlink => match self.exact_target(&here) {
                    Some(target) => {
                        let next = if target.starts_with('/') {
                            target
                        } else {
                            super::join(parent_of(&here), &target)
                        };
                        self.follow(&next, hops - 1)
                    }
                    None => here,
                },
                _ => here,
            }
        }

        /// The metadata of `path` itself, following no final link — the
        /// `Keep` reading.
        fn own(&self, path: &str) -> Result<Metadata, Errno> {
            self.exact(&self.interior(path, FIXTURE_HOPS))
                .ok_or(Errno::NotFound)
        }

        /// The metadata of what `path` finally names — the `Follow` reading.
        /// A chain that outlasts the hop bound is a cycle, refused rather
        /// than walked.
        fn resolve(&self, path: &str) -> Result<Metadata, Errno> {
            let target = self.follow(path, FIXTURE_HOPS);
            match self.exact(&target) {
                Some(meta) if meta.kind == FileKind::Symlink => Err(Errno::LinkLoop),
                Some(meta) => Ok(meta),
                None => Err(Errno::NotFound),
            }
        }

        /// The stored target of the link at `path`.
        fn target_of(&self, path: &str) -> Result<String, Errno> {
            self.exact_target(&self.interior(path, FIXTURE_HOPS))
                .ok_or(Errno::OutOfRange)
        }

        fn file(mut self, path: &str, mode: u32, size: u64) -> Self {
            let id = self.fresh_id();
            self.stat.push((
                path.to_string(),
                Metadata {
                    kind: FileKind::Regular,
                    nlink: 1,
                    mode,
                    size,
                    allocated: size,
                    uid: UID,
                    gid: GID,
                    id,
                    times: NodeTimes::default(),
                },
            ));
            self
        }

        fn dir(mut self, path: &str) -> Self {
            let id = self.fresh_id();
            self.stat.push((
                path.to_string(),
                Metadata {
                    kind: FileKind::Directory,
                    nlink: 1,
                    mode: 0o755,
                    size: 0,
                    allocated: 0,
                    uid: UID,
                    gid: GID,
                    id,
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

        /// [`entry`](Self::entry) whose stat reports `nlink` names, so the
        /// long format's link-count column can be driven with a real value
        /// rather than every row reading `1`.
        fn entry_links(
            mut self,
            dir: &str,
            name: &str,
            kind: FileKind,
            mode: u32,
            size: u64,
            nlink: u32,
        ) -> Self {
            self = self.entry(dir, name, kind, mode, size);
            let path = super::join(dir, name);
            let row = self
                .stat
                .iter_mut()
                .find(|(p, _)| *p == path)
                .expect("the entry just declared");
            row.1.nlink = nlink;
            self
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
            let id = self.fresh_id();
            self.stat.push((
                super::join(dir, name),
                Metadata {
                    kind,
                    nlink: 1,
                    mode,
                    size,
                    allocated,
                    uid: UID,
                    gid: GID,
                    id,
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
                    nlink: 1,
                    mode: 0o644,
                    size: 0,
                    allocated: 0,
                    uid: UID,
                    gid: GID,
                    id: node_id(inode),
                    times,
                },
            ));
            self
        }
    }

    impl Listing for TreeFs {
        fn stat(&self, path: &str, links: FinalLink) -> Result<Metadata, Errno> {
            match links {
                FinalLink::Keep => self.own(path),
                FinalLink::Follow => self.resolve(path),
            }
        }

        fn read_link(&self, path: &str) -> Result<String, Errno> {
            // A non-link has no target to read, and neither has an absent
            // path: the same domain refusal the kernel gives.
            self.own(path)?;
            self.target_of(path)
        }

        fn read_dir(&self, path: &str) -> Result<Vec<Entry>, Errno> {
            // A directory handle opened without `NO_FOLLOW` lists what the
            // final link names, so the fixture resolves before looking up.
            let real = self.follow(path, FIXTURE_HOPS);
            self.dirs
                .iter()
                .find(|(d, _)| *d == real)
                .map(|(_, c)| c.clone())
                .ok_or(Errno::NotFound)
        }
    }

    /// A directory whose `read_dir` always fails — to exercise the read
    /// fail-closed path.
    struct FailingDir;

    impl Listing for FailingDir {
        fn stat(&self, _path: &str, _links: FinalLink) -> Result<Metadata, Errno> {
            Ok(Metadata {
                kind: FileKind::Directory,
                nlink: 1,
                mode: 0o755,
                size: 0,
                allocated: 0,
                uid: UID,
                gid: GID,
                id: node_id(0),
                times: NodeTimes::default(),
            })
        }

        fn read_link(&self, _path: &str) -> Result<String, Errno> {
            Err(Errno::OutOfRange)
        }

        fn read_dir(&self, _path: &str) -> Result<Vec<Entry>, Errno> {
            Err(Errno::PermissionDenied)
        }
    }

    /// The fixture's node identity for node number `node`: one volume, so
    /// two rows differ exactly when their node numbers do.
    fn node_id(node: u64) -> tairix_abi::fs::FileId {
        tairix_abi::fs::FileId {
            volume: [1u8; 16],
            node,
        }
    }

    /// The directory part of `path`, for resolving a relative link target.
    fn parent_of(path: &str) -> &str {
        match path.rfind('/') {
            Some(0) => "/",
            Some(slash) => &path[..slash],
            None => ".",
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
        /// The diagnostics the listing wrote to the error stream, in order.
        errors: RefCell<Vec<String>>,
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
                errors: RefCell::new(Vec::new()),
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
                errors: RefCell::new(Vec::new()),
                fail: false,
                width: Some(cols),
            }
        }

        fn failing() -> Self {
            Self {
                chunks: RefCell::new(Vec::new()),
                records: RefCell::new(Vec::new()),
                errors: RefCell::new(Vec::new()),
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

        fn errors(&self) -> Vec<String> {
            self.errors.borrow().clone()
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

        fn error(&self, message: &str) {
            self.errors.borrow_mut().push(String::from(message));
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
                format: long.then_some(Format::Long),
                ..Options::DEFAULT
            },
            paths,
        )
    }

    fn run_ls(command: Command, fs: &dyn Listing, out: &Recorder) -> Result<Outcome, LsError> {
        run(command, None, NOW, None, fs, &NoHelp, out)
    }

    /// Run a listing with an explicit `TERM`, for the colour tests. The
    /// attestation still comes from the `Recorder`'s `terminal_width`.
    fn run_ls_term(
        command: Command,
        term: Option<&str>,
        fs: &dyn Listing,
        out: &Recorder,
    ) -> Result<Outcome, LsError> {
        run(command, None, NOW, term, fs, &NoHelp, out)
    }

    #[test]
    fn help_renders_the_short_help_from_the_document() {
        let fs = TreeFs::new();
        let out = Recorder::new();
        assert_eq!(
            run(Command::Help, None, NOW, None, &fs, &OneDoc, &out),
            Ok(Outcome::Complete)
        );
        let text = out.text();
        assert!(text.contains("ls — list directory contents"), "{text}");
        assert!(text.contains("ls [-a] [-l] [--] [path...]"), "{text}");
    }

    #[test]
    fn help_falls_back_to_the_usage_banner_without_a_tree() {
        let fs = TreeFs::new();
        let out = Recorder::new();
        assert_eq!(
            run(Command::Help, None, NOW, None, &fs, &NoHelp, &out),
            Ok(Outcome::Complete)
        );
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
        assert_eq!(
            run_ls(list(false, false, &["."]), &fs, &out),
            Ok(Outcome::Complete)
        );
        assert_eq!(out.text(), "a\nb\nc\n");
    }

    #[test]
    fn hidden_entries_are_filtered_and_noted_on_the_advisory_stream() {
        let fs = TreeFs::new()
            .dir(".")
            .entry(".", ".hidden", FileKind::Regular, 0o644, 0)
            .entry(".", "visible", FileKind::Regular, 0o644, 0);
        let out = Recorder::new();
        assert_eq!(
            run_ls(list(false, false, &["."]), &fs, &out),
            Ok(Outcome::Complete)
        );
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
        assert_eq!(
            run_ls(list(true, false, &["."]), &fs, &out),
            Ok(Outcome::Complete)
        );
        assert_eq!(out.text(), ".hidden\nvisible\n");
        assert!(out.records().is_empty());
    }

    #[test]
    fn a_listing_without_hidden_entries_emits_no_record() {
        let fs = TreeFs::new()
            .dir(".")
            .entry(".", "visible", FileKind::Regular, 0o644, 0);
        let out = Recorder::new();
        assert_eq!(
            run_ls(list(false, false, &["."]), &fs, &out),
            Ok(Outcome::Complete)
        );
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
            Ok(Outcome::Complete)
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
        assert_eq!(
            run_ls(list(false, false, &["a.txt"]), &fs, &out),
            Ok(Outcome::Complete)
        );
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
            Ok(Outcome::Complete)
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
            Ok(Outcome::Complete)
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
            Ok(Outcome::Complete)
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
            Ok(Outcome::Complete)
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
            Ok(Outcome::Complete)
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
        assert_eq!(
            run_ls(list(false, true, &["."]), &fs, &out),
            Ok(Outcome::Complete)
        );
        assert_eq!(
            out.text(),
            "total 5\ndrwxr-xr-x 1 1000 100 4096 Jan  1  1970 d\n\
             -rw-r--r-- 1 1000 100    7 Jan  1  1970 f\n"
        );
    }

    #[test]
    fn the_long_format_reports_the_filesystems_own_link_count() {
        // The second column is the count the filesystem records, not a
        // constant: a twice-named file shows `2`, and the column is padded
        // to its widest value so the owner column still aligns.
        let fs = TreeFs::new()
            .dir(".")
            .entry_links(".", "many", FileKind::Regular, 0o644, 7, 12)
            .entry_links(".", "one", FileKind::Regular, 0o644, 7, 1);
        let out = Recorder::new();
        assert_eq!(
            run_ls(list(false, true, &["."]), &fs, &out),
            Ok(Outcome::Complete)
        );
        assert_eq!(
            out.text(),
            "total 1\n-rw-r--r-- 12 1000 100 7 Jan  1  1970 many\n\
             -rw-r--r--  1 1000 100 7 Jan  1  1970 one\n"
        );
    }

    #[test]
    fn long_format_stats_entries_under_a_slash_terminated_operand() {
        // The joined per-entry path must not double the trailing slash.
        let fs = TreeFs::new()
            .dir("dir/")
            .entry("dir/", "f", FileKind::Regular, 0o600, 3);
        let out = Recorder::new();
        assert_eq!(
            run_ls(list(false, true, &["dir/"]), &fs, &out),
            Ok(Outcome::Complete)
        );
        assert_eq!(
            out.text(),
            "total 1\n-rw------- 1 1000 100 3 Jan  1  1970 f\n"
        );
    }

    #[test]
    fn single_directory_operand_has_no_header() {
        let fs = TreeFs::new()
            .dir("dir")
            .entry("dir", "x", FileKind::Regular, 0o644, 0);
        let out = Recorder::new();
        assert_eq!(
            run_ls(list(false, false, &["dir"]), &fs, &out),
            Ok(Outcome::Complete)
        );
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
            Ok(Outcome::Complete)
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
            Ok(Outcome::Complete)
        );
        assert_eq!(out.text(), "dir1:\na\n\ndir2:\nb\n");
    }

    #[test]
    fn empty_directory_emits_nothing() {
        let fs = TreeFs::new().dir(".");
        let out = Recorder::new();
        assert_eq!(
            run_ls(list(false, false, &["."]), &fs, &out),
            Ok(Outcome::Complete)
        );
        assert_eq!(out.text(), "");
    }

    #[test]
    fn a_missing_operand_is_reported_and_graded_serious() {
        let fs = TreeFs::new();
        let out = Recorder::new();
        assert_eq!(
            run_ls(list(false, false, &["absent"]), &fs, &out),
            Ok(Outcome::SeriousProblem)
        );
        assert_eq!(out.text(), "");
        assert_eq!(out.errors(), ["ls: cannot access 'absent': not found"]);
        assert_eq!(Outcome::SeriousProblem.exit_status(), 2);
    }

    #[test]
    fn a_stat_error_reports_and_the_remaining_operands_still_list() {
        // The GNU behaviour: the missing operand is reported on the error
        // stream and skipped, and the operand after it is still listed.
        let fs = TreeFs::new()
            .dir("present")
            .entry("present", "x", FileKind::Regular, 0o644, 0);
        let out = Recorder::new();
        assert_eq!(
            run_ls(list(false, false, &["absent", "present"]), &fs, &out),
            Ok(Outcome::SeriousProblem)
        );
        assert_eq!(out.text(), "present:\nx\n");
        assert_eq!(out.errors(), ["ls: cannot access 'absent': not found"]);
    }

    #[test]
    fn an_unreadable_operand_directory_is_reported_and_graded_serious() {
        let out = Recorder::new();
        assert_eq!(
            run_ls(list(false, false, &["dir"]), &FailingDir, &out),
            Ok(Outcome::SeriousProblem)
        );
        assert_eq!(
            out.errors(),
            ["ls: cannot open directory 'dir': permission denied"]
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
            Ok(Outcome::Complete)
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
            Ok(Outcome::Complete)
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
            Ok(Outcome::Complete)
        );
        assert_eq!(out.chunks(), ["top:\nsub\nz\n", "\ntop/sub:\nx\n"]);
    }

    /// A filesystem error mid-recursion surfaces after the blocks already
    /// listed: the traversal streamed them out when their directories were
    /// read, exactly as any streaming tool behaves.
    #[test]
    fn a_read_dir_error_mid_recursion_is_reported_and_the_walk_continues() {
        // `sub` is announced by `top` but has no readable node, so its
        // `read_dir` fails after `top`'s block has been written. A
        // subdirectory is a *minor* problem: it is reported and the rest of
        // the traversal still runs.
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
            Ok(Outcome::MinorProblem)
        );
        assert_eq!(out.text(), "top:\nsub\n");
        assert_eq!(
            out.errors(),
            ["ls: cannot open directory 'top/sub': not found"]
        );
        assert_eq!(Outcome::MinorProblem.exit_status(), 1);
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
            Ok(Outcome::Complete)
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
            Ok(Outcome::Complete)
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
            Ok(Outcome::Complete)
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
            Ok(Outcome::Complete)
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
            Ok(Outcome::Complete)
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
            Ok(Outcome::Complete)
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
            Ok(Outcome::Complete)
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
            Ok(Outcome::Complete)
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
            Ok(Outcome::Complete)
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
            Ok(Outcome::Complete)
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
            Ok(Outcome::Complete)
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
            Ok(Outcome::Complete)
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
            Ok(Outcome::Complete)
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
            Ok(Outcome::Complete)
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
            Ok(Outcome::Complete)
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
            Ok(Outcome::Complete)
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
            Ok(Outcome::Complete)
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
            Ok(Outcome::Complete)
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
            Ok(Outcome::Complete)
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
                        format: Some(Format::Long),
                        file_size: SizeFormat::Human { si: false },
                        block_size: SizeFormat::Human { si: false },
                        ..Options::DEFAULT
                    },
                    &["."],
                ),
                &fs,
                &out,
            ),
            Ok(Outcome::Complete)
        );
        assert_eq!(
            out.text(),
            "total 11M\n\
             -rw-r--r-- 1 1000 100  500 Jan  1  1970 a\n\
             -rw-r--r-- 1 1000 100 1.1K Jan  1  1970 b\n\
             -rw-r--r-- 1 1000 100  10M Jan  1  1970 c\n"
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
                        format: Some(Format::Long),
                        hide_owner: true,
                        ..Options::DEFAULT
                    },
                    &["."],
                ),
                &fs,
                &hidden_owner,
            ),
            Ok(Outcome::Complete)
        );
        assert_eq!(
            hidden_owner.text(),
            "total 1\n-rw-r--r-- 1 100 7 Jan  1  1970 f\n"
        );
        let hidden_group = Recorder::new();
        assert_eq!(
            run_ls(
                list_with(
                    Options {
                        format: Some(Format::Long),
                        hide_group: true,
                        ..Options::DEFAULT
                    },
                    &["."],
                ),
                &fs,
                &hidden_group,
            ),
            Ok(Outcome::Complete)
        );
        assert_eq!(
            hidden_group.text(),
            "total 1\n-rw-r--r-- 1 1000 7 Jan  1  1970 f\n"
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
            Ok(Outcome::Complete)
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
            Ok(Outcome::Complete)
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
                        format: Some(Format::Long),
                        size: true,
                        ..Options::DEFAULT
                    },
                    &["."],
                ),
                &fs,
                &out,
            ),
            Ok(Outcome::Complete)
        );
        assert_eq!(
            out.text(),
            "total 5\n4 drwxr-xr-x 1 1000 100 4096 Jan  1  1970 d\n\
             1 -rw-r--r-- 1 1000 100    7 Jan  1  1970 f\n"
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
                        file_size: SizeFormat::Human { si: false },
                        block_size: SizeFormat::Human { si: false },
                        ..Options::DEFAULT
                    },
                    &["."],
                ),
                &fs,
                &out,
            ),
            Ok(Outcome::Complete)
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
            Ok(Outcome::Complete)
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
            Ok(Outcome::Complete)
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
            assert_eq!(super::human_size(size, 1024), expected, "{size}");
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
            Ok(Outcome::Complete)
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
                        format: Some(Format::Long),
                        inode: true,
                        ..Options::DEFAULT
                    },
                    &["."],
                ),
                &fs,
                &out,
            ),
            Ok(Outcome::Complete)
        );
        assert_eq!(
            out.text(),
            "total 0\n1234 -rw-r--r-- 1 1000 100 0 May  6 12:53 f\n"
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
            Ok(Outcome::Complete)
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
            Ok(Outcome::Complete)
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
                            format: Some(Format::Long),
                            time_field: field,
                            ..Options::DEFAULT
                        },
                        &["."],
                    ),
                    &fs,
                    &out,
                ),
                Ok(Outcome::Complete)
            );
            out.text()
        };
        // Modified (the default) is old → a year; accessed and changed are
        // recent → a time-of-day.
        assert_eq!(
            long(TimeField::Modified),
            "total 0\n-rw-r--r-- 1 1000 100 0 Nov 14  2023 f\n"
        );
        assert_eq!(
            long(TimeField::Accessed),
            "total 0\n-rw-r--r-- 1 1000 100 0 May  6 12:53 f\n"
        );
        assert_eq!(
            long(TimeField::Changed),
            "total 0\n-rw-r--r-- 1 1000 100 0 Feb 29 13:46 f\n"
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
                            format: Some(Format::Long),
                            time_style: style,
                            ..Options::DEFAULT
                        },
                        &["."],
                    ),
                    &fs,
                    &out,
                ),
                Ok(Outcome::Complete)
            );
            out.text()
        };
        assert_eq!(
            styled(TimeStyle::Locale),
            "total 0\n-rw-r--r-- 1 1000 100 0 May  6 12:53 f\n"
        );
        assert_eq!(
            styled(TimeStyle::LongIso),
            "total 0\n-rw-r--r-- 1 1000 100 0 2024-05-06 12:53 f\n"
        );
        assert_eq!(
            styled(TimeStyle::FullIso),
            "total 0\n-rw-r--r-- 1 1000 100 0 2024-05-06 12:53:20.000000000 +0000 f\n"
        );
        assert_eq!(
            styled(TimeStyle::Iso),
            "total 0\n-rw-r--r-- 1 1000 100 0 05-06 12:53 f\n"
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
                        format: Some(Format::Long),
                        time_style: TimeStyle::Iso,
                        ..Options::DEFAULT
                    },
                    &["."],
                ),
                &fs,
                &out,
            ),
            Ok(Outcome::Complete)
        );
        assert_eq!(
            out.text(),
            "total 0\n-rw-r--r-- 1 1000 100 0 2023-11-14 f\n"
        );
    }

    #[test]
    fn si_scales_the_long_size_in_powers_of_1000() {
        // `--si` renders the long-format file size in base-1000 units, with
        // the lowercase kilo letter GNU uses.
        let fs = TreeFs::new()
            .dir(".")
            .entry(".", "f", FileKind::Regular, 0o644, 5000);
        let out = Recorder::new();
        assert_eq!(
            run_ls(
                list_with(
                    Options {
                        format: Some(Format::Long),
                        file_size: SizeFormat::Human { si: true },
                        block_size: SizeFormat::Human { si: true },
                        ..Options::DEFAULT
                    },
                    &["."],
                ),
                &fs,
                &out,
            ),
            Ok(Outcome::Complete)
        );
        assert_eq!(
            out.text(),
            "total 5.0k\n-rw-r--r-- 1 1000 100 5.0k Jan  1  1970 f\n"
        );
    }

    #[test]
    fn block_size_scales_the_allocation_cells() {
        // A 512-byte block size doubles the default 1024-block counts; the
        // `-s` cell and `total` both use it.
        let fs = TreeFs::new()
            .dir(".")
            .entry_alloc(".", "f", FileKind::Regular, 0o644, 5, 8192);
        let out = Recorder::new();
        assert_eq!(
            run_ls(
                list_with(
                    Options {
                        size: true,
                        block_size: SizeFormat::Scaled {
                            unit: 512,
                            suffix: None,
                        },
                        ..Options::DEFAULT
                    },
                    &["."],
                ),
                &fs,
                &out,
            ),
            Ok(Outcome::Complete)
        );
        assert_eq!(out.text(), "total 16\n16 f\n");
    }

    #[test]
    fn the_total_scales_the_summed_allocation_not_the_rounded_cells() {
        // Two half-block files each round up to one 1024-block cell, but the
        // total scales the *summed* allocation (1024 bytes -> 1 block), not
        // the sum of the rounded cells (which would be 2) — the GNU rule.
        let fs = TreeFs::new()
            .dir(".")
            .entry_alloc(".", "a", FileKind::Regular, 0o644, 1, 512)
            .entry_alloc(".", "b", FileKind::Regular, 0o644, 1, 512);
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
            Ok(Outcome::Complete)
        );
        assert_eq!(out.text(), "total 1\n1 a\n1 b\n");
    }

    #[test]
    fn author_repeats_the_owner_column() {
        // `--author` prints the owning user again, after the owner and
        // before the group.
        let fs = TreeFs::new()
            .dir(".")
            .entry(".", "f", FileKind::Regular, 0o644, 7);
        let out = Recorder::new();
        assert_eq!(
            run_ls(
                list_with(
                    Options {
                        format: Some(Format::Long),
                        author: true,
                        ..Options::DEFAULT
                    },
                    &["."],
                ),
                &fs,
                &out,
            ),
            Ok(Outcome::Complete)
        );
        assert_eq!(
            out.text(),
            "total 1\n-rw-r--r-- 1 1000 1000 100 7 Jan  1  1970 f\n"
        );
    }

    #[test]
    fn file_type_marks_directories_but_never_executables() {
        // `--file-type` appends `/` to directories and nothing to an
        // executable, unlike `-F` which would append `*`.
        let fs = TreeFs::new()
            .dir(".")
            .entry(".", "d", FileKind::Directory, 0o755, 0)
            .entry(".", "exe", FileKind::Regular, 0o755, 0);
        let file_type = Recorder::new();
        assert_eq!(
            run_ls(
                list_with(
                    Options {
                        indicator: Indicator::FileType,
                        ..Options::DEFAULT
                    },
                    &["."],
                ),
                &fs,
                &file_type,
            ),
            Ok(Outcome::Complete)
        );
        assert_eq!(file_type.text(), "d/\nexe\n");
        // `-F` (classify) additionally stars the executable.
        let classify = Recorder::new();
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
                &classify,
            ),
            Ok(Outcome::Complete)
        );
        assert_eq!(classify.text(), "d/\nexe*\n");
    }

    #[test]
    fn zero_terminates_every_line_with_nul() {
        // `--zero` ends each entry line and the `total` with NUL; the header
        // and inter-block separator keep the newline, matching GNU.
        let fs = TreeFs::new()
            .dir(".")
            .entry(".", "a", FileKind::Regular, 0o644, 0)
            .entry(".", "b", FileKind::Regular, 0o644, 0);
        let out = Recorder::new();
        assert_eq!(
            run_ls(
                list_with(
                    Options {
                        zero: true,
                        ..Options::DEFAULT
                    },
                    &["."],
                ),
                &fs,
                &out,
            ),
            Ok(Outcome::Complete)
        );
        assert_eq!(out.text(), "a\0b\0");
    }

    #[test]
    fn tabsize_advances_columns_with_tabs() {
        // A column whose gap crosses a tab stop is padded with a tab under
        // the default tab size and with spaces under `-T0` — a byte-for-byte
        // match with GNU `ls -C -T8` / `-T0`.
        let fs = TreeFs::new()
            .dir(".")
            .entry(".", "abcdef", FileKind::Regular, 0o644, 0)
            .entry(".", "bb", FileKind::Regular, 0o644, 0)
            .entry(".", "cc", FileKind::Regular, 0o644, 0)
            .entry(".", "dd", FileKind::Regular, 0o644, 0);
        let tabbed = Recorder::new();
        assert_eq!(
            run_ls(
                list_with(
                    Options {
                        format: Some(Format::Columns),
                        width: Some(12),
                        ..Options::DEFAULT
                    },
                    &["."],
                ),
                &fs,
                &tabbed,
            ),
            Ok(Outcome::Complete)
        );
        assert_eq!(tabbed.text(), "abcdef\tcc\nbb\tdd\n");
        let spaced = Recorder::new();
        assert_eq!(
            run_ls(
                list_with(
                    Options {
                        format: Some(Format::Columns),
                        width: Some(12),
                        tabsize: 0,
                        ..Options::DEFAULT
                    },
                    &["."],
                ),
                &fs,
                &spaced,
            ),
            Ok(Outcome::Complete)
        );
        assert_eq!(spaced.text(), "abcdef  cc\nbb      dd\n");
    }

    /// A small fixture with the three colour cases: a directory, an
    /// executable regular file, and a plain regular file.
    fn colour_fixture() -> TreeFs {
        TreeFs::new()
            .dir(".")
            .entry(".", "file.txt", FileKind::Regular, 0o644, 0)
            .entry(".", "prog", FileKind::Regular, 0o755, 0)
            .entry(".", "sub", FileKind::Directory, 0o755, 0)
    }

    // The standard-scheme SGR runs for the two coloured `ls` roles: a
    // directory (bold + bright blue) and an executable (bold + green), each
    // closed by a reset. Written literally here so the test pins the exact
    // bytes a terminal receives, independent of the scheme's builders.
    const DIR_ON: &str = "\u{1b}[1m\u{1b}[94m";
    const EXE_ON: &str = "\u{1b}[1m\u{1b}[32m";
    const OFF: &str = "\u{1b}[0m";

    /// Strip every `CSI … m` SGR sequence from `text`, leaving the plain
    /// bytes a mono terminal (or a pipe) would see.
    fn strip_sgr(text: &str) -> String {
        let mut out = String::new();
        let mut chars = text.chars();
        while let Some(ch) = chars.next() {
            if ch == '\u{1b}' {
                // Skip `[` … `m` (the only escapes `ls` emits are SGR).
                for esc in chars.by_ref() {
                    if esc == 'm' {
                        break;
                    }
                }
            } else {
                out.push(ch);
            }
        }
        out
    }

    #[test]
    fn color_always_paints_each_kind_by_role() {
        let out = Recorder::new();
        run_ls_term(
            list_with(
                Options {
                    color: ColorChoice::Always,
                    format: Some(Format::OnePerLine),
                    ..Options::DEFAULT
                },
                &["."],
            ),
            Some("xterm-256color"),
            &colour_fixture(),
            &out,
        )
        .expect("listing succeeds");
        // Plain regular file uncoloured; executable green; directory blue.
        assert_eq!(
            out.text(),
            format!("file.txt\n{EXE_ON}prog{OFF}\n{DIR_ON}sub{OFF}\n")
        );
    }

    #[test]
    fn color_never_stays_plain_even_at_a_colour_terminal() {
        let out = Recorder::terminal(80);
        run_ls_term(
            list_with(
                Options {
                    color: ColorChoice::Never,
                    format: Some(Format::OnePerLine),
                    ..Options::DEFAULT
                },
                &["."],
            ),
            Some("xterm-256color"),
            &colour_fixture(),
            &out,
        )
        .expect("listing succeeds");
        assert_eq!(out.text(), "file.txt\nprog\nsub\n");
    }

    #[test]
    fn color_auto_colours_only_an_attested_colour_terminal() {
        let coloured = format!("file.txt\n{EXE_ON}prog{OFF}\n{DIR_ON}sub{OFF}\n");
        let plain = "file.txt\nprog\nsub\n";
        let opts = Options {
            color: ColorChoice::Auto,
            format: Some(Format::OnePerLine),
            ..Options::DEFAULT
        };
        // Attested + colour TERM → coloured.
        let attested = Recorder::terminal(80);
        run_ls_term(
            list_with(opts, &["."]),
            Some("xterm-256color"),
            &colour_fixture(),
            &attested,
        )
        .expect("listing succeeds");
        assert_eq!(attested.text(), coloured);
        // Attested but no/unknown TERM → plain (never guessed).
        let no_term = Recorder::terminal(80);
        run_ls_term(list_with(opts, &["."]), None, &colour_fixture(), &no_term)
            .expect("listing succeeds");
        assert_eq!(no_term.text(), plain);
        // Not attested (piped) → plain regardless of TERM.
        let piped = Recorder::new();
        run_ls_term(
            list_with(opts, &["."]),
            Some("xterm-256color"),
            &colour_fixture(),
            &piped,
        )
        .expect("listing succeeds");
        assert_eq!(piped.text(), plain);
    }

    #[test]
    fn colour_never_changes_the_column_layout() {
        // A coloured grid, stripped of its SGR sequences, is byte-identical
        // to the plain grid: colour shifts no column.
        let opts = |color| Options {
            color,
            format: Some(Format::Columns),
            width: Some(40),
            ..Options::DEFAULT
        };
        let coloured = Recorder::terminal(40);
        run_ls_term(
            list_with(opts(ColorChoice::Always), &["."]),
            Some("xterm-256color"),
            &colour_fixture(),
            &coloured,
        )
        .expect("listing succeeds");
        let plain = Recorder::terminal(40);
        run_ls_term(
            list_with(opts(ColorChoice::Never), &["."]),
            Some("xterm-256color"),
            &colour_fixture(),
            &plain,
        )
        .expect("listing succeeds");
        assert!(coloured.text().contains(DIR_ON), "colour was emitted");
        assert_eq!(strip_sgr(&coloured.text()), plain.text());
    }

    #[test]
    fn colour_paints_the_name_but_not_the_indicator_suffix() {
        // Under `-F` the `/` and `*` suffixes are appended outside the colour
        // (after the reset), as the GNU tool does by default.
        let out = Recorder::new();
        run_ls_term(
            list_with(
                Options {
                    color: ColorChoice::Always,
                    format: Some(Format::OnePerLine),
                    indicator: Indicator::Classify,
                    ..Options::DEFAULT
                },
                &["."],
            ),
            Some("xterm-256color"),
            &colour_fixture(),
            &out,
        )
        .expect("listing succeeds");
        assert_eq!(
            out.text(),
            format!("file.txt\n{EXE_ON}prog{OFF}*\n{DIR_ON}sub{OFF}/\n")
        );
    }

    // --- Symbolic links -------------------------------------------------

    /// A tree with the four link shapes the postures have to tell apart: a
    /// link to a file, a link to a directory, a dangling link, and the
    /// targets themselves.
    fn link_fixture() -> TreeFs {
        TreeFs::new()
            .dir(".")
            .entry(".", "target.txt", FileKind::Regular, 0o644, 7)
            .link_entry(".", "to-file", "/target.txt")
            .link_entry(".", "dangling", "/nowhere")
            .dir("/sub")
            .entry("/sub", "inner", FileKind::Regular, 0o644, 3)
            .link_entry(".", "to-dir", "/sub")
            .file("/target.txt", 0o644, 7)
            // `x` and `./x` name the same node on a real volume; the fixture
            // is keyed by spelling, so the bare forms an operand uses are
            // declared too.
            .link("to-dir", "/sub")
            .link("to-file", "/target.txt")
            .link("dangling", "/nowhere")
    }

    fn long_lines(out: &Recorder) -> Vec<String> {
        out.text().lines().map(String::from).collect::<Vec<_>>()
    }

    #[test]
    fn the_long_format_shows_a_link_and_its_target() {
        // `-l` alone keeps every link: the type letter is `l`, the mode is
        // the link's own, and the stored target follows the name.
        let out = Recorder::new();
        assert_eq!(
            run_ls(list(false, true, &["."]), &link_fixture(), &out),
            Ok(Outcome::Complete)
        );
        let lines = long_lines(&out);
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("lrwxrwxrwx") && l.ends_with("to-file -> /target.txt")),
            "{lines:?}"
        );
        // A dangling link is shown exactly like a live one: `-l` never
        // resolves it, so there is nothing to fail.
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("lrwxrwxrwx") && l.ends_with("dangling -> /nowhere")),
            "{lines:?}"
        );
        assert!(out.errors().is_empty(), "{:?}", out.errors());
    }

    #[test]
    fn dereference_everywhere_shows_the_targets_metadata() {
        // `-L` resolves each entry, so the link to a file reads as a regular
        // file of the target's size and the link to a directory as a
        // directory — and neither shows a `-> target`, because neither row is
        // a link any more.
        let out = Recorder::new();
        let command = list_with(
            Options {
                format: Some(Format::Long),
                dereference: Some(Dereference::Always),
                ..Options::DEFAULT
            },
            &["."],
        );
        assert_eq!(
            run_ls(command, &link_fixture(), &out),
            Ok(Outcome::MinorProblem)
        );
        let lines = long_lines(&out);
        assert!(
            lines.iter().any(|l| l.starts_with("-rw-r--r--")
                && l.contains(" 7 ")
                && l.ends_with("to-file")),
            "{lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("drwxr-xr-x") && l.ends_with("to-dir")),
            "{lines:?}"
        );
        // A resolved row is no longer a link, so it shows no target; the
        // dangling row keeps its `-> target`, which is all GNU has left to
        // show for it.
        for line in &lines {
            if line.ends_with("to-file") || line.ends_with("to-dir") {
                assert!(!line.contains(" -> "), "{line}");
            }
        }
        assert!(
            out.text().contains("dangling -> /nowhere"),
            "{}",
            out.text()
        );
    }

    #[test]
    fn a_dangling_link_under_dereference_is_reported_and_the_listing_continues() {
        // The GNU shape: the reason on the error stream, a `?` row for what
        // could not be inspected, the rest of the listing on standard
        // output, and a non-zero grade.
        let out = Recorder::new();
        let command = list_with(
            Options {
                format: Some(Format::Long),
                dereference: Some(Dereference::Always),
                ..Options::DEFAULT
            },
            &["."],
        );
        assert_eq!(
            run_ls(command, &link_fixture(), &out),
            Ok(Outcome::MinorProblem)
        );
        assert_eq!(out.errors(), ["ls: cannot access './dangling': not found"]);
        let lines = long_lines(&out);
        // The type letter survives (the directory stream reported it); every
        // stat-derived cell is `?`, never a fabricated zero.
        let dangling = lines
            .iter()
            .find(|l| l.contains("dangling"))
            .expect("the row is still listed");
        assert!(dangling.starts_with("l????????? ?"), "{dangling}");
        assert!(dangling.contains(" ? "), "{dangling}");
        // The other entries listed normally.
        assert!(lines.iter().any(|l| l.ends_with("target.txt")), "{lines:?}");
    }

    #[test]
    fn a_dangling_link_operand_is_described_by_default_and_refused_under_dereference() {
        // The bare posture describes the operand itself, so a dangling link
        // lists fine; `-L` resolves it and reports the refusal.
        let out = Recorder::new();
        assert_eq!(
            run_ls(
                list(false, true, &["/nolink"]),
                &link_fixture().link("/nolink", "/gone"),
                &out
            ),
            Ok(Outcome::Complete)
        );
        assert!(out.text().ends_with("/nolink -> /gone\n"), "{}", out.text());

        let out = Recorder::new();
        let command = list_with(
            Options {
                format: Some(Format::Long),
                dereference: Some(Dereference::Always),
                ..Options::DEFAULT
            },
            &["/nolink"],
        );
        assert_eq!(
            run_ls(command, &link_fixture().link("/nolink", "/gone"), &out),
            Ok(Outcome::SeriousProblem)
        );
        assert_eq!(out.text(), "");
        assert_eq!(out.errors(), ["ls: cannot access '/nolink': not found"]);
    }

    #[test]
    fn the_default_posture_lists_a_command_line_link_to_a_directory() {
        // The GNU default: a command-line link *to a directory* is resolved,
        // so `ls to-dir` lists the directory's contents.
        let out = Recorder::new();
        assert_eq!(
            run_ls(list(false, false, &["to-dir"]), &link_fixture(), &out),
            Ok(Outcome::Complete)
        );
        assert_eq!(out.text(), "inner\n");
    }

    #[test]
    fn the_long_format_lists_a_command_line_link_as_itself() {
        // `-l` forces the `Never` posture, so the same operand is the link.
        let out = Recorder::new();
        assert_eq!(
            run_ls(list(false, true, &["to-dir"]), &link_fixture(), &out),
            Ok(Outcome::Complete)
        );
        assert!(out.text().starts_with("lrwxrwxrwx"), "{}", out.text());
        assert!(out.text().ends_with("to-dir -> /sub\n"), "{}", out.text());
    }

    #[test]
    fn dereference_command_line_resolves_only_the_operands() {
        // `-H` resolves the operand — so `to-dir` becomes the directory and
        // is listed — while the links *inside* a listing still show
        // themselves.
        let out = Recorder::new();
        let command = list_with(
            Options {
                format: Some(Format::Long),
                dereference: Some(Dereference::CommandLine),
                ..Options::DEFAULT
            },
            &["to-dir", "."],
        );
        let outcome = run_ls(command, &link_fixture(), &out);
        let text = out.text();
        assert_eq!(outcome, Ok(Outcome::Complete), "{text} {:?}", out.errors());
        // The operand's own block lists the directory behind the link.
        assert!(text.contains("to-dir:\n"), "{text}");
        assert!(text.contains("inner"), "{text}");
        // Inside the `.` listing the links are still links.
        assert!(text.contains("to-file -> /target.txt"), "{text}");
    }

    #[test]
    fn the_file_type_indicator_marks_a_link_and_the_classify_one_does_too() {
        for indicator in [Indicator::FileType, Indicator::Classify] {
            let out = Recorder::new();
            let command = list_with(
                Options {
                    format: Some(Format::OnePerLine),
                    indicator,
                    ..Options::DEFAULT
                },
                &["."],
            );
            assert_eq!(
                run_ls(command, &link_fixture(), &out),
                Ok(Outcome::Complete)
            );
            let text = out.text();
            assert!(text.contains("to-file@\n"), "{indicator:?}: {text}");
            assert!(text.contains("to-dir@\n"), "{indicator:?}: {text}");
            assert!(text.contains("target.txt\n"), "{indicator:?}: {text}");
        }
        // `-p` marks directories only, so a link keeps its bare name.
        let out = Recorder::new();
        let command = list_with(
            Options {
                format: Some(Format::OnePerLine),
                indicator: Indicator::Slash,
                ..Options::DEFAULT
            },
            &["."],
        );
        assert_eq!(
            run_ls(command, &link_fixture(), &out),
            Ok(Outcome::Complete)
        );
        assert!(out.text().contains("to-file\n"), "{}", out.text());
    }

    #[test]
    fn recursion_never_descends_a_link_unless_it_was_resolved() {
        // `-R` alone: `to-dir` is a link, so its target is not walked.
        let out = Recorder::new();
        let command = list_with(
            Options {
                recursive: true,
                ..Options::DEFAULT
            },
            &["."],
        );
        assert_eq!(
            run_ls(command, &link_fixture(), &out),
            Ok(Outcome::Complete)
        );
        assert!(!out.text().contains("inner"), "{}", out.text());

        // `-RL` resolves each entry, so the directory behind the link *is*
        // walked.
        let out = Recorder::new();
        let command = list_with(
            Options {
                recursive: true,
                dereference: Some(Dereference::Always),
                ..Options::DEFAULT
            },
            &["."],
        );
        assert_eq!(
            run_ls(command, &link_fixture(), &out),
            Ok(Outcome::MinorProblem)
        );
        assert!(out.text().contains("inner"), "{}", out.text());
    }

    #[test]
    fn a_cycle_under_recursive_dereference_is_reported_once_and_not_walked() {
        // `loop` points back at the directory that holds it, so `-RL` would
        // descend for ever; the already-listed directory is reported and
        // skipped instead.
        let fs = TreeFs::new()
            .dir("/top")
            .entry("/top", "f", FileKind::Regular, 0o644, 0)
            .link_entry("/top", "loop", "/top");
        let out = Recorder::new();
        let command = list_with(
            Options {
                recursive: true,
                dereference: Some(Dereference::Always),
                ..Options::DEFAULT
            },
            &["/top"],
        );
        assert_eq!(run_ls(command, &fs, &out), Ok(Outcome::MinorProblem));
        assert_eq!(
            out.errors(),
            ["ls: not listing already-listed directory: '/top/loop'"]
        );
        // The listing itself is finite and complete.
        assert_eq!(out.text(), "/top:\nf\nloop\n");
    }

    #[test]
    fn colour_paints_a_link_and_its_target_by_what_each_is() {
        // The link name takes the link role; the target text takes the role
        // of what it names — a directory here.
        let out = Recorder::new();
        let command = list_with(
            Options {
                color: ColorChoice::Always,
                format: Some(Format::Long),
                ..Options::DEFAULT
            },
            &["to-dir"],
        );
        run_ls_term(command, Some("xterm-256color"), &link_fixture(), &out)
            .expect("listing succeeds");
        let text = out.text();
        assert!(
            text.contains(&format!("{LINK_ON}to-dir{OFF} -> {DIR_ON}/sub{OFF}")),
            "{text}"
        );
    }

    #[test]
    fn a_link_target_that_cannot_be_reached_is_left_uncoloured() {
        let out = Recorder::new();
        let command = list_with(
            Options {
                color: ColorChoice::Always,
                format: Some(Format::Long),
                ..Options::DEFAULT
            },
            &["dangling"],
        );
        run_ls_term(command, Some("xterm-256color"), &link_fixture(), &out)
            .expect("listing succeeds");
        let text = out.text();
        assert!(
            text.contains(&format!("{LINK_ON}dangling{OFF} -> /nowhere")),
            "{text}"
        );
    }

    #[test]
    fn read_link_is_not_paid_outside_the_long_format() {
        // Only the format that prints `-> target` reads it, so a listing that
        // never shows one asks for nothing.
        let fs = CountingLinks::new(link_fixture());
        let out = Recorder::new();
        assert_eq!(
            run_ls(list(false, false, &["."]), &fs, &out),
            Ok(Outcome::Complete)
        );
        assert_eq!(fs.read_links(), 0);
        let out = Recorder::new();
        assert_eq!(
            run_ls(list(false, true, &["."]), &fs, &out),
            Ok(Outcome::Complete)
        );
        // One per link row, and only for the link rows.
        assert_eq!(fs.read_links(), 3);
    }

    /// A [`Listing`] that counts its `read_link` calls, so a test can prove
    /// the target read is paid only where it is printed.
    struct CountingLinks {
        inner: TreeFs,
        reads: RefCell<usize>,
    }

    impl CountingLinks {
        fn new(inner: TreeFs) -> Self {
            Self {
                inner,
                reads: RefCell::new(0),
            }
        }

        fn read_links(&self) -> usize {
            *self.reads.borrow()
        }
    }

    impl Listing for CountingLinks {
        fn stat(&self, path: &str, links: FinalLink) -> Result<Metadata, Errno> {
            self.inner.stat(path, links)
        }

        fn read_link(&self, path: &str) -> Result<String, Errno> {
            *self.reads.borrow_mut() += 1;
            self.inner.read_link(path)
        }

        fn read_dir(&self, path: &str) -> Result<Vec<Entry>, Errno> {
            self.inner.read_dir(path)
        }
    }

    /// The standard-scheme SGR run for the link role (bold + cyan).
    const LINK_ON: &str = "\u{1b}[1m\u{1b}[36m";
}
