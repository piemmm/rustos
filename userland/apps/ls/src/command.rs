//! The parsed shape of an `ls` command line.

use alloc::string::String;
use alloc::vec::Vec;

use tairix_glob::Pattern;
use tairix_termcap::ColorChoice;

use crate::error::LsError;

/// Which entries a directory listing includes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Hidden {
    /// Hide entries whose name begins with `.` (the default).
    Skip,
    /// Show every entry except `.` and `..` (`-A`).
    AlmostAll,
    /// Show every entry (`-a`).
    All,
}

/// The output arrangement of one listing.
///
/// GNU `ls` keeps a *single* format state: the last of `-l` / `-1` / `-C` /
/// `-x` / `-m` (and the `--format=WORD` words) wins, with one exception —
/// `-1` has no effect after `-l` (so `-l -1` stays long). This one enum,
/// rather than a separate `long` flag beside a column arrangement, is what
/// makes that last-wins precedence expressible.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Format {
    /// The long format: mode, owner/group, size, timestamp, name — one
    /// entry per line (`-l` / `-n` / `-g` / `-o` / `--format=long`).
    Long,
    /// One name per line (`-1` / `--format=single-column`).
    OnePerLine,
    /// Multiple columns filled top-to-bottom, sized to the terminal width
    /// (`-C` / `--format=vertical`; the GNU default when writing to a
    /// terminal).
    Columns,
    /// Multiple columns filled left-to-right, sized to the terminal width
    /// (`-x` / `--format=across` / `--format=horizontal`).
    Across,
    /// Names separated by `, `, wrapped to the terminal width (`-m` /
    /// `--format=commas`).
    Commas,
}

/// The indicator suffix appended to a rendered name.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Indicator {
    /// No suffix (the default).
    None,
    /// `/` after directories (`-p` / `--indicator-style=slash`).
    Slash,
    /// A type indicator after every classifiable entry *except* executables
    /// (`--file-type` / `--indicator-style=file-type`). With the VFS's two
    /// kinds (regular file, directory) this appends `/` to directories and
    /// nothing else; it differs from [`Classify`](Self::Classify) only in
    /// never appending the executable `*`.
    FileType,
    /// `/` after directories and `*` after executables (`-F` / `--classify`
    /// / `--indicator-style=classify`).
    Classify,
}

/// Which symbolic links a listing resolves — the GNU `ls` dereference
/// posture, whose four states are what make `ls linkdir`, `ls -l linkdir`,
/// `ls -H linkdir`, and `ls -L linkdir` four different listings.
///
/// The posture chosen here is not itself the per-path reading: it *selects*
/// one ([`FinalLink`](crate::io::FinalLink)) for each path, differently for a
/// command-line operand and for an entry inside a listed directory.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Dereference {
    /// Describe every link itself, wherever it appears. Selected by the
    /// formats that show a link's own metadata — `-l`, `-d`, and `-F` — and
    /// by nothing else.
    Never,
    /// Resolve a command-line operand that is a link **to a directory**, so
    /// `ls linkdir` lists the directory's contents; describe every other
    /// link itself. The GNU default when no format flag forces
    /// [`Never`](Self::Never).
    CommandLineDirectory,
    /// Resolve every command-line operand (`-H` / `--dereference-command-line`);
    /// links inside a listed directory describe themselves.
    CommandLine,
    /// Resolve every link, wherever it appears (`-L` / `--dereference`).
    Always,
}

/// How a byte count is scaled and rendered as a size.
///
/// GNU `ls` keeps *two* of these independently: one for the long-format
/// file-size column (`file_size`, default plain bytes) and one for the `-s`
/// allocation cells and the `total` line (`block_size`, default 1024-byte
/// units). `-h` / `--si` / `--block-size` set both; `-k` touches only the
/// block scaling.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SizeFormat {
    /// Scale each byte count by `unit` (rounded up to whole units) and, when
    /// `suffix` is present, print that unit suffix after the number. Plain
    /// bytes are `unit == 1` with no suffix (the long-format default and
    /// `--block-size=1`); the `-s` default is `unit == 1024`, no suffix.
    Scaled {
        /// The block size in bytes (never zero).
        unit: u64,
        /// The unit suffix to print, when the SIZE was a bare unit; GNU
        /// prints none when a numeric coefficient was given.
        suffix: Option<UnitSuffix>,
    },
    /// Autoscaling human-readable sizes: base 1024 (`-h`) or base 1000
    /// (`--si`), rounded up, one decimal below ten.
    Human {
        /// Powers of 1000 (`--si`) rather than 1024 (`-h`).
        si: bool,
    },
}

/// A unit suffix printed after a [`SizeFormat::Scaled`] count.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UnitSuffix {
    /// The power letter, uppercase: `K`, `M`, `G`, `T`, `P`, or `E`.
    letter: u8,
    /// Which spelling of the suffix to print.
    form: SuffixForm,
}

/// The spelling of a [`UnitSuffix`], mirroring the three `--block-size`
/// unit forms GNU accepts and echoes back.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SuffixForm {
    /// The bare power letter: `K`, `M`, … (a base-1024 unit like `K`).
    Iec,
    /// The IEC binary spelling: `KiB`, `MiB`, … (a `KiB`-form unit).
    IecLong,
    /// The SI decimal spelling: kilo `kB`, else `MB`, `GB`, … (a `kB`-form
    /// unit, base 1000).
    Si,
}

impl UnitSuffix {
    /// The exact text GNU prints for this suffix.
    #[must_use]
    pub fn text(self) -> &'static str {
        match (self.letter, self.form) {
            (b'K', SuffixForm::Iec) => "K",
            (b'K', SuffixForm::IecLong) => "KiB",
            (b'K', SuffixForm::Si) => "kB",
            (b'M', SuffixForm::Iec) => "M",
            (b'M', SuffixForm::IecLong) => "MiB",
            (b'M', SuffixForm::Si) => "MB",
            (b'G', SuffixForm::Iec) => "G",
            (b'G', SuffixForm::IecLong) => "GiB",
            (b'G', SuffixForm::Si) => "GB",
            (b'T', SuffixForm::Iec) => "T",
            (b'T', SuffixForm::IecLong) => "TiB",
            (b'T', SuffixForm::Si) => "TB",
            (b'P', SuffixForm::Iec) => "P",
            (b'P', SuffixForm::IecLong) => "PiB",
            (b'P', SuffixForm::Si) => "PB",
            (b'E', SuffixForm::Iec) => "E",
            (b'E', SuffixForm::IecLong) => "EiB",
            (b'E', SuffixForm::Si) => "EB",
            // Every `letter` is set from the closed power table in
            // `parse_block_size`, so no other pair is constructible.
            _ => "",
        }
    }
}

impl SizeFormat {
    /// Plain bytes: no scaling, no suffix. The long-format file-size default
    /// and `--block-size=1`.
    pub const BYTES: Self = Self::Scaled {
        unit: 1,
        suffix: None,
    };

    /// 1024-byte units with no suffix: the `-s` allocation / `total` default
    /// (and `--block-size=1024` / `-k`).
    pub const KIBI_BLOCKS: Self = Self::Scaled {
        unit: 1024,
        suffix: None,
    };
}

/// The GNU default column tab stop (`-T` / `--tabsize`).
pub const DEFAULT_TABSIZE: usize = 8;

/// The key rows are ordered by.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Sort {
    /// Lexicographic name order (the default).
    Name,
    /// No sort: the directory (read) order the filesystem returns (`-U`,
    /// `-f`, `--sort=none`). `-r` still reverses that order.
    None,
    /// Largest size first, ties by name (`-S`).
    Size,
    /// Newest [selected timestamp](TimeField) first, ties by name (`-t`, and
    /// the GNU `-c`/`-u`-without-`-l` default).
    Time,
    /// By file-name extension (the text from the last `.`), ties by name
    /// (`-X`, `--sort=extension`).
    Extension,
    /// Natural "version" order (`-v`, `--sort=version`): the GNU `filevercmp`
    /// ordering, so `f2` sorts before `f10`; ties by name.
    Version,
}

/// Which of a node's four timestamps the long format shows and a time sort
/// (`-t`) orders by.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TimeField {
    /// Last contents-modification time (mtime) — the default.
    Modified,
    /// Last access time (atime) — `-u` / `--time=atime`.
    Accessed,
    /// Last metadata-change time (ctime) — `-c` / `--time=ctime`.
    Changed,
    /// Creation time — `--time=birth`.
    Created,
}

/// How each rendered name is quoted (`-N` / `-Q` / `-b` / `--quoting-style`).
///
/// These are the GNU `ls` quoting styles. The `locale` and `clocale` styles
/// are a documented divergence: they select quotation marks from the active
/// locale, which TAIRiX has no infrastructure for, so they are refused (fail
/// closed) rather than faked — the same stance the tool already takes on a
/// custom `--time-style=+FORMAT`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QuotingStyle {
    /// Names verbatim, no quoting (`-N` / `--literal` / `--quoting-style=literal`).
    /// With `-q` (hide control characters) nongraphic bytes become `?`.
    Literal,
    /// Quote for the shell only when the name contains a shell-special or
    /// nongraphic character (`--quoting-style=shell`). Nongraphic characters
    /// are left literal (or, with `-q`, shown as `?`).
    Shell,
    /// Like [`Shell`](Self::Shell) but always quote, even a plain name
    /// (`--quoting-style=shell-always`).
    ShellAlways,
    /// Like [`Shell`](Self::Shell) but render nongraphic characters with
    /// `$'…'` ANSI-C escapes (`--quoting-style=shell-escape`).
    ShellEscape,
    /// Like [`ShellEscape`](Self::ShellEscape) but always quote
    /// (`--quoting-style=shell-escape-always`).
    ShellEscapeAlways,
    /// C string quoting: always double-quoted with C backslash escapes
    /// (`-Q` / `--quote-name` / `--quoting-style=c`).
    C,
    /// Like [`C`](Self::C) but without the surrounding double quotes and with
    /// spaces escaped (`-b` / `--escape` / `--quoting-style=escape`).
    Escape,
}

/// How a timestamp is rendered in the long format.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TimeStyle {
    /// The GNU default: `Mon DD HH:MM` for a stamp within the last six
    /// months, `Mon DD  YYYY` for an older or future one.
    Locale,
    /// `YYYY-MM-DD HH:MM` (`--time-style=long-iso`).
    LongIso,
    /// `YYYY-MM-DD HH:MM:SS.NNNNNNNNN +0000` (`--time-style=full-iso`, and
    /// `--full-time`).
    FullIso,
    /// `MM-DD HH:MM` for a recent stamp, `YYYY-MM-DD` otherwise
    /// (`--time-style=iso`).
    Iso,
}

/// The full option set of one `ls` listing.
///
/// The flags are the GNU tool's independent boolean switches; a
/// two-variant enum per flag would only restate `bool` with extra noise,
/// so the field count mirrors the command surface deliberately.
#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Options {
    /// Which dot-entries the listing includes (`-a` / `-A`).
    pub hidden: Hidden,
    /// List directory operands themselves, not their contents (`-d`).
    pub directory: bool,
    /// Recurse into subdirectories (`-R`).
    pub recursive: bool,
    /// Reverse the sort order (`-r`).
    pub reverse: bool,
    /// Sort key (`-S`).
    pub sort: Sort,
    /// The output arrangement (`-l` / `-1` / `-C` / `-x` / `-m` /
    /// `--format`). `None` selects the GNU default, resolved against the
    /// attested console at render time: multiple columns when the output is
    /// a terminal (single column under `--zero`), one name per line
    /// otherwise. The long format ([`Format::Long`]) is one of the values,
    /// so its last-wins precedence against a column arrangement is captured
    /// here rather than in a separate flag.
    pub format: Option<Format>,
    /// Explicit output width in columns (`-w` / `--width`); `Some(0)` means
    /// an unlimited line length. `None` takes the attested terminal width,
    /// falling back to 80.
    pub width: Option<usize>,
    /// Name indicator suffixes (`-p` / `-F`).
    pub indicator: Indicator,
    /// How each name is quoted (`-N` / `-Q` / `-b` / `--quoting-style`).
    /// `None` selects the GNU default, resolved against the attested console
    /// at render time: `shell-escape` when the output is a terminal, `literal`
    /// otherwise.
    pub quoting: Option<QuotingStyle>,
    /// Whether nongraphic characters are shown as `?` in the non-escaping
    /// quoting styles (`-q` / `--hide-control-chars` vs `--show-control-chars`).
    /// `None` selects the GNU default: hide when the output is a terminal,
    /// show otherwise.
    pub hide_control_chars: Option<bool>,
    /// Print each entry's allocated size in blocks (`-s`), plus a `total`
    /// line per directory block. The block scaling is [`block_size`].
    ///
    /// [`block_size`]: Self::block_size
    pub size: bool,
    /// How the long-format file-size column is scaled (`-h` / `--si` /
    /// `--block-size`). Default: plain bytes.
    pub file_size: SizeFormat,
    /// How the `-s` allocation cells and the `total` line are scaled (`-h`
    /// / `--si` / `--block-size` / `-k`). Default: 1024-byte units.
    pub block_size: SizeFormat,
    /// Omit the owner column from the long format (`-g`).
    pub hide_owner: bool,
    /// Omit the group column from the long format (`-o` / `-G` /
    /// `--no-group`). Note `-o` also selects the long format, whereas `-G`
    /// only drops the column and has no effect outside the long format.
    pub hide_group: bool,
    /// Print the author column in the long format (`--author`). TAIRiX has
    /// no separate author, so — as on any normal system the GNU tool runs
    /// on — the author is the owning user; the column repeats the numeric
    /// owner, placed after the owner and before the group.
    pub author: bool,
    /// Which timestamp the long format shows and `-t` sorts by (`-c` / `-u`
    /// / `--time`).
    pub time_field: TimeField,
    /// How the long-format timestamp is rendered (`--time-style` /
    /// `--full-time`).
    pub time_style: TimeStyle,
    /// Print each entry's stable node number (`-i` / `--inode`).
    pub inode: bool,
    /// List directories before other entries, keeping the chosen sort within
    /// each group (`--group-directories-first`). Directories come first
    /// regardless of `-r` — the GNU behaviour.
    pub group_directories_first: bool,
    /// Column-grid tab stop (`-T` / `--tabsize`); `0` pads columns with
    /// spaces only. Default: 8. Only the `-C` / `-x` arrangements use it,
    /// exactly as the GNU tool advances between columns with tabs.
    pub tabsize: usize,
    /// Terminate each output line with NUL instead of newline (`--zero`).
    /// Also selects, as overridable defaults, the single-column
    /// arrangement, literal quoting, and shown control characters — the GNU
    /// `--zero` posture.
    pub zero: bool,
    /// Which symbolic links the listing resolves (`-L` / `-H` /
    /// `--dereference-command-line-symlink-to-dir`). `None` selects the GNU
    /// default, resolved once against the chosen format:
    /// [`Dereference::Never`] when `-l`, `-d`, or `-F` shows a link's own
    /// metadata, and [`Dereference::CommandLineDirectory`] otherwise.
    pub dereference: Option<Dereference>,
    /// When to colourise names by kind (`--color[=WHEN]`). The default is
    /// [`ColorChoice::Auto`]: colour at an attested colour terminal, plain
    /// when piped, redirected, or on a console the kernel cannot attest. The
    /// concrete colour depth is resolved against `TERM` at render time; this
    /// field only records *when*.
    pub color: ColorChoice,
}

impl Options {
    /// Whether the long format ([`Format::Long`]) is selected.
    #[must_use]
    pub fn is_long(self) -> bool {
        matches!(self.format, Some(Format::Long))
    }

    /// The effective dereference posture: the explicit `-L` / `-H` choice,
    /// else the GNU default resolved against the chosen format.
    ///
    /// GNU makes the undefined posture [`Dereference::Never`] exactly when a
    /// format that shows a link's *own* metadata is in force — `-l`, `-d`
    /// (list the operand itself), or `-F` (whose indicator is the kind) —
    /// and [`Dereference::CommandLineDirectory`] otherwise, which is what
    /// makes a bare `ls linkdir` list the directory the link names.
    #[must_use]
    pub fn dereference(self) -> Dereference {
        self.dereference.unwrap_or({
            if self.is_long() || self.directory || self.indicator == Indicator::Classify {
                Dereference::Never
            } else {
                Dereference::CommandLineDirectory
            }
        })
    }
}

/// The name filters of one `ls` listing: the `-B` / `-I` / `--hide`
/// switches that suppress directory entries by name.
///
/// These are kept apart from [`Options`] on purpose. [`Options`] is a small
/// `Copy` value threaded through the whole render/sort/format pipeline;
/// these filters own a `Vec` of compiled patterns and are consulted in
/// exactly one place — the per-directory filtering step in the listing
/// engine. Folding the owned patterns into [`Options`] would strip its
/// `Copy`-ness and force every render helper to borrow a `Vec` it never
/// reads, so the filters travel beside [`Options`], not inside it.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Filters {
    /// Do not list entries whose name ends with `~` (`-B` /
    /// `--ignore-backups`), in every dotfile mode — a backup is filtered
    /// even when `-a` shows hidden entries. A direct suffix test, not a
    /// glob, so it needs no leading-`.` special case.
    pub ignore_backups: bool,
    /// Do not list entries whose name matches any of these glob patterns
    /// (`-I PATTERN` / `--ignore=PATTERN`, repeatable). Applied in every
    /// dotfile mode.
    pub ignore: Vec<Pattern>,
    /// Do not list entries whose name matches any of these glob patterns
    /// (`--hide=PATTERN`, repeatable), **unless** `-a` or `-A` is given —
    /// the GNU rule that `--hide` yields to an explicit "show hidden".
    pub hide: Vec<Pattern>,
}

impl Filters {
    /// Whether `name` is suppressed by these filters, given whether the
    /// listing is showing hidden entries (`show_hidden` is true under `-a`
    /// or `-A`). Mirrors GNU's `file_ignored`: `-B` and `-I` patterns apply
    /// always; `--hide` patterns apply only when hidden entries are
    /// otherwise shown by default (i.e. not under `-a` / `-A`).
    #[must_use]
    pub fn suppresses(&self, name: &str, show_hidden: bool) -> bool {
        if self.ignore_backups && name.ends_with('~') {
            return true;
        }
        if self.ignore.iter().any(|pattern| pattern.matches(name)) {
            return true;
        }
        !show_hidden && self.hide.iter().any(|pattern| pattern.matches(name))
    }
}

impl Options {
    /// The defaults of a bare `ls`.
    pub const DEFAULT: Self = Self {
        hidden: Hidden::Skip,
        directory: false,
        recursive: false,
        reverse: false,
        sort: Sort::Name,
        format: None,
        width: None,
        indicator: Indicator::None,
        quoting: None,
        hide_control_chars: None,
        size: false,
        file_size: SizeFormat::BYTES,
        block_size: SizeFormat::KIBI_BLOCKS,
        hide_owner: false,
        hide_group: false,
        author: false,
        time_field: TimeField::Modified,
        time_style: TimeStyle::Locale,
        inode: false,
        group_directories_first: false,
        tabsize: DEFAULT_TABSIZE,
        zero: false,
        dereference: None,
        color: ColorChoice::Auto,
    };
}

/// One thing the `ls` tool can do.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Command {
    /// List `paths`, in operand order.
    List {
        /// The listing options.
        options: Options,
        /// The name filters (`-B` / `-I` / `--hide`) suppressing entries.
        filters: Filters,
        /// The paths to list, in order. Never empty: an empty command line
        /// yields the single path `.`.
        paths: Vec<String>,
    },
    /// Render `ls`'s own short help (`-?`/`--help`): the `NAME`,
    /// `SYNOPSIS`, and compact `OPTIONS` of its Help document, through the
    /// same engine as any other command's short help (plans/APPS.md §4).
    Help,
}

/// Parse `args` (the tool's arguments, excluding the program name) into a
/// [`Command`].
///
/// The grammar is the GNU `ls` surface,
/// `ls [-aABCcdFfGgHhikILlmnopQrRsSTtUuvXx1] [-w cols] [-I PATTERN]`
/// `[--block-size=SIZE] [--si] [--format=WORD] [--indicator-style=WORD]`
/// `[--hide=PATTERN] [--time=WORD] [--time-style=STYLE] [--sort=WORD]`
/// `[--full-time] [--author] [--file-type] [--group-directories-first]`
/// `[--zero] [--] [path...]`:
///
/// * `-a` / `--all` — do not hide entries whose name begins with `.`.
/// * `-A` / `--almost-all` — like `-a`, but never list `.` or `..`.
/// * `-C` — list entries in columns, filled top-to-bottom.
/// * `-d` / `--directory` — list directory operands themselves.
/// * `-F` / `--classify` — append `/` to directories and `*` to
///   executables.
/// * `--file-type` / `--indicator-style=file-type` — append `/` to
///   directories (and, on richer filesystems, a type indicator to other
///   non-executables); like `-F` but never the executable `*`.
/// * `--indicator-style=WORD` — the indicator suffix by name: `none`,
///   `slash` (`-p`), `file-type` (`--file-type`), or `classify` (`-F`).
/// * `-g` — long format without the owner column; implies `-l`.
/// * `-G` / `--no-group` — omit the group column from the long format.
///   Unlike `-o` it does *not* select the long format on its own.
/// * `--author` — with the long format, print the author (the owning user)
///   after the owner and before the group.
/// * `-h` / `--human-readable` — human-readable sizes (base 1024, e.g.
///   `4.9K`), for both the long-format file sizes and the `-s` blocks (as
///   in the GNU tool; short help is `-?` / `--help`).
/// * `--si` — like `-h` but powers of 1000 (e.g. `5.0k`).
/// * `-k` / `--kibibytes` — use 1024-byte blocks for the `-s` cells and the
///   directory `total` (TAIRiX's default, so this confirms rather than
///   changes the output; a size option overrides it).
/// * `--block-size=SIZE` — scale both the file sizes and the `-s` blocks by
///   SIZE: a plain integer (bytes), or a unit `K`/`M`/`G`/`T`/`P`/`E`
///   (1024-based), a `KiB`-form (1024-based), or a `KB`-form (1000-based),
///   optionally with an integer coefficient. A bare unit prints its
///   suffix; a coefficient suppresses it. A malformed SIZE is a usage
///   error (fail closed).
/// * `-l` — long format.
/// * `--format=WORD` — the arrangement by name: `long` (`-l`) / `verbose`,
///   `single-column` (`-1`), `vertical` (`-C`), `across` / `horizontal`
///   (`-x`), or `commas` (`-m`).
/// * `-m` — comma-separated names, wrapped to the output width.
/// * `-n` / `--numeric-uid-gid` — long format with numeric owner and
///   group; implies `-l` (owner and group are always numeric here — see
///   the Help document).
/// * `-o` — long format without the group column; implies `-l`.
/// * `-p` — append `/` to directories.
/// * `-N` / `--literal` / `--quoting-style=literal` — print names verbatim,
///   no quoting.
/// * `-Q` / `--quote-name` / `--quoting-style=c` — C-string quoting: each
///   name double-quoted with C backslash escapes.
/// * `-b` / `--escape` / `--quoting-style=escape` — like `-Q` but without
///   the surrounding quotes and with spaces escaped.
/// * `--quoting-style=WORD` — the quoting style by name: `literal` (`-N`),
///   `shell`, `shell-always`, `shell-escape`, `shell-escape-always`, `c`
///   (`-Q`), or `escape` (`-b`). The last quoting flag wins. The GNU
///   default is `shell-escape` at a terminal and `literal` otherwise. The
///   `locale` / `clocale` styles are refused (fail closed; a documented
///   divergence).
/// * `-q` / `--hide-control-chars` — show nongraphic characters as `?` (the
///   default at a terminal); `--show-control-chars` prints them as-is (the
///   default otherwise). Only affects the non-escaping styles.
/// * `-r` / `--reverse` — reverse the sort order.
/// * `-R` / `--recursive` — list subdirectories recursively.
/// * `-L` / `--dereference` — show information about the file each
///   symbolic link names, rather than the link itself, wherever a link
///   appears. A link whose target cannot be reached (a dangling link) is
///   reported on standard error and the listing continues, with a non-zero
///   exit status.
/// * `-H` / `--dereference-command-line` — dereference only the symbolic
///   links named on the command line; links inside a listed directory show
///   themselves. The later of `-L` / `-H` wins.
/// * `--dereference-command-line-symlink-to-dir` — the GNU default when no
///   format flag forces otherwise: a command-line link *to a directory* is
///   dereferenced (so `ls linkdir` lists the directory) and every other
///   link shows itself. `-l`, `-d`, and `-F` instead default to showing
///   every link itself.
/// * `-s` / `--size` — print each entry's allocated size in blocks (scaled
///   by `-h` / `--si` / `--block-size` / `-k`), with a `total` line per
///   directory block.
/// * `-S` — sort by size, largest first.
/// * `-t` — sort by the selected timestamp, newest first.
/// * `-U` — do not sort; list entries in directory order.
/// * `-X` — sort by file-name extension, ties by name.
/// * `-v` — natural "version" sort (`filevercmp`), ties by name.
/// * `-f` — do not sort and show all entries: enables `-a` and `-U` and
///   disables `-l` and `-s` (applied where it appears, so a later
///   `-l`/`-s`/sort flag overrides it — the GNU order semantics).
/// * `--sort=WORD` — the sort key by name: `none` (`-U`), `size` (`-S`),
///   `time` (`-t`), `version` (`-v`), `extension` (`-X`), or `name`.
/// * `--group-directories-first` — list directories before other entries;
///   directories come first even under `-r`.
/// * `-c` — use the metadata-change time (ctime): with `-l` show it, and
///   with `-t` sort by it; without `-l` sort by it.
/// * `-u` — like `-c` but for the access time (atime).
/// * `--time=WORD` — which timestamp to show and sort by: `atime`
///   (`access` / `use`), `ctime` (`status`), `mtime` (`modification`), or
///   `birth` (`creation`).
/// * `-i` / `--inode` — print each entry's stable node number.
/// * `-B` / `--ignore-backups` — do not list entries whose name ends with
///   `~`, in every dotfile mode (a backup is hidden even under `-a`).
/// * `-I PATTERN` / `--ignore=PATTERN` — do not list entries matching the
///   glob `PATTERN` (repeatable); applied in every dotfile mode.
/// * `--hide=PATTERN` — like `--ignore`, but has no effect when `-a` or
///   `-A` is given (the GNU rule that an explicit "show hidden" wins).
///   `-I` and `--hide` patterns are the shared `tairix_glob` matcher, in
///   which `*` / `?` also match a leading `.` (a documented divergence
///   from GNU's `fnmatch(FNM_PERIOD)`); a malformed pattern is a usage
///   error (fail closed).
/// * `--time-style=STYLE` — how the long-format timestamp is rendered:
///   `locale` (the default), `long-iso`, `full-iso`, or `iso`. A custom
///   `+FORMAT` style is refused (fail closed; a documented divergence).
/// * `--full-time` — like `-l --time-style=full-iso`.
/// * `-T` / `--tabsize <cols>` — set the column-grid tab stop (default 8;
///   `0` pads with spaces only).
/// * `-w` / `--width <cols>` — set the output width; `0` means unlimited.
/// * `-x` — list entries in columns, filled left-to-right.
/// * `-1` — one name per line.
/// * `--zero` — end each output line with NUL, not newline; also selects
///   (overridably) the single-column arrangement, literal quoting, and
///   shown control characters.
/// * `-?` / `--help` — the reserved short-help switches (plans/APPS.md §4;
///   they win immediately).
/// * `--` — end option parsing; every later argument is a path.
/// * any other `-…` — an [`LsError::Usage`] error (fail closed; never a
///   silently ignored token).
/// * anything else (including the bare `-`) — a path.
///
/// Short options may be combined into one argument (e.g. `-la` is
/// `-l -a`); an unrecognised letter anywhere in such a cluster is a usage
/// error. `-w`, `-T`, and `-I` each take the rest of their cluster as their
/// value (`-w80`), or the next argument (`-w 80`) when they end the
/// cluster. The last of `-l` / `-1` / `-C` / `-x` / `-m` (and the
/// `--format` words) wins, with one GNU exception: `-1` has no effect
/// after `-l` (so `-l -1` stays long). The later of `-p` / `-F` /
/// `--file-type` / `--indicator-style` wins.
///
/// With no path operand the single path is the current directory (`.`).
///
/// # Errors
///
/// [`LsError::Usage`] for any unrecognised option before `--`, a `-w` /
/// `--width` without a valid non-negative integer value, an unknown
/// `--time` / `--time-style` / `--quoting-style` word (including a custom
/// `--time-style=+FORMAT` and the `locale` / `clocale` quoting styles), or
/// a value given to a long option that takes none.
pub fn parse(args: &[&str]) -> Result<Command, LsError> {
    let mut state = ParseState::default();
    let mut paths = Vec::new();
    let mut options_done = false;
    let mut args = args.iter();
    while let Some(&arg) = args.next() {
        if options_done {
            paths.push(String::from(arg));
            continue;
        }
        if arg == "--" {
            options_done = true;
            continue;
        }
        if let Some(name) = arg.strip_prefix("--") {
            let (key, inline) = match name.split_once('=') {
                Some((key, value)) => (key, Some(value)),
                None => (name, None),
            };
            if apply_long(key, inline, &mut state, &mut args)? {
                return Ok(Command::Help);
            }
            continue;
        }
        // A short-option cluster is a leading `-` followed by at least one
        // letter (the bare `-` is a path, not an option).
        if let Some(letters) = arg.strip_prefix('-').filter(|rest| !rest.is_empty()) {
            if apply_short(letters, &mut state, &mut args)? {
                return Ok(Command::Help);
            }
            continue;
        }
        paths.push(String::from(arg));
    }
    state.resolve_sort();
    state.resolve_sizes();
    if paths.is_empty() {
        paths.push(String::from("."));
    }
    Ok(Command::List {
        options: state.options,
        filters: state.filters,
        paths,
    })
}

/// The mutable accumulator threaded through option parsing.
///
/// Sort selection is resolved after the whole line is parsed: an explicit
/// `-t` / `-S` wins; otherwise a `-c` / `-u` / `--time` time selection sorts
/// by that time *unless* `-l` is present — the GNU rule, which depends on
/// flags that may appear in any order.
struct ParseState {
    options: Options,
    filters: Filters,
    explicit_sort: Option<Sort>,
    time_selected: bool,
    /// `-k` / `--kibibytes` was seen: force 1024-byte blocks for the `-s` /
    /// `total` scaling unless a size option already set it.
    kibibytes: bool,
    /// A size option (`-h` / `--si` / `--block-size`) set the scaling, so
    /// `-k` yields to it — the GNU precedence (`-k` never overrides an
    /// explicit size choice).
    size_explicit: bool,
}

impl Default for ParseState {
    fn default() -> Self {
        Self {
            options: Options::DEFAULT,
            filters: Filters::default(),
            explicit_sort: None,
            time_selected: false,
            kibibytes: false,
            size_explicit: false,
        }
    }
}

impl ParseState {
    /// Apply the GNU sort rule once the whole line has been parsed.
    fn resolve_sort(&mut self) {
        self.options.sort = self.explicit_sort.unwrap_or({
            if self.time_selected && !self.options.is_long() {
                Sort::Time
            } else {
                Sort::Name
            }
        });
    }

    /// Apply the GNU `-k` rule once the whole line has been parsed: force
    /// the block (`-s` / `total`) scaling to 1024-byte units, but only when
    /// no `-h` / `--si` / `--block-size` already set it (an explicit size
    /// choice always wins over `-k`). TAIRiX has no `BLOCK_SIZE` environment
    /// default, so `-k` confirms the existing default rather than changing
    /// it — the compatible, honest behaviour.
    fn resolve_sizes(&mut self) {
        if self.kibibytes && !self.size_explicit {
            self.options.block_size = SizeFormat::KIBI_BLOCKS;
        }
    }
}

/// Read a long option's value, taking it from the inline `--opt=value` form
/// or the following argument. A missing value is a usage error (fail closed).
fn long_value<'a>(
    inline: Option<&'a str>,
    args: &mut core::slice::Iter<'a, &'a str>,
) -> Result<&'a str, LsError> {
    match inline {
        Some(value) => Ok(value),
        None => Ok(*args.next().ok_or(LsError::Usage)?),
    }
}

/// Apply one `--long` option. Returns `Ok(true)` when the option requests
/// short help (`--help`), so the caller returns [`Command::Help`].
// One flat long-option dispatch match; splitting it would scatter the option
// table across helpers for no gain.
#[allow(clippy::too_many_lines)]
fn apply_long<'a>(
    key: &str,
    inline: Option<&'a str>,
    state: &mut ParseState,
    args: &mut core::slice::Iter<'a, &'a str>,
) -> Result<bool, LsError> {
    let options = &mut state.options;
    match key {
        "all" => options.hidden = Hidden::All,
        "almost-all" => options.hidden = Hidden::AlmostAll,
        "directory" => options.directory = true,
        "classify" => options.indicator = Indicator::Classify,
        "file-type" => options.indicator = Indicator::FileType,
        "human-readable" => {
            options.file_size = SizeFormat::Human { si: false };
            options.block_size = SizeFormat::Human { si: false };
            state.size_explicit = true;
        }
        "si" => {
            options.file_size = SizeFormat::Human { si: true };
            options.block_size = SizeFormat::Human { si: true };
            state.size_explicit = true;
        }
        "kibibytes" => state.kibibytes = true,
        "numeric-uid-gid" => options.format = Some(Format::Long),
        "no-group" => options.hide_group = true,
        "author" => options.author = true,
        "literal" => options.quoting = Some(QuotingStyle::Literal),
        "quote-name" => options.quoting = Some(QuotingStyle::C),
        "escape" => options.quoting = Some(QuotingStyle::Escape),
        "hide-control-chars" => options.hide_control_chars = Some(true),
        "show-control-chars" => options.hide_control_chars = Some(false),
        "size" => options.size = true,
        "group-directories-first" => options.group_directories_first = true,
        "reverse" => options.reverse = true,
        "recursive" => options.recursive = true,
        "inode" => options.inode = true,
        "zero" => options.zero = true,
        "dereference" => options.dereference = Some(Dereference::Always),
        "dereference-command-line" => options.dereference = Some(Dereference::CommandLine),
        "dereference-command-line-symlink-to-dir" => {
            options.dereference = Some(Dereference::CommandLineDirectory);
        }
        "ignore-backups" => state.filters.ignore_backups = true,
        "ignore" => {
            state
                .filters
                .ignore
                .push(Pattern::new(long_value(inline, args)?).map_err(|_| LsError::Usage)?);
            return Ok(false);
        }
        "hide" => {
            state
                .filters
                .hide
                .push(Pattern::new(long_value(inline, args)?).map_err(|_| LsError::Usage)?);
            return Ok(false);
        }
        "full-time" => {
            options.format = Some(Format::Long);
            options.time_style = TimeStyle::FullIso;
        }
        "help" => return Ok(true),
        "block-size" => {
            let size = parse_block_size(long_value(inline, args)?)?;
            options.file_size = size;
            options.block_size = size;
            state.size_explicit = true;
            return Ok(false);
        }
        "format" => {
            options.format = Some(parse_format(long_value(inline, args)?)?);
            return Ok(false);
        }
        "indicator-style" => {
            options.indicator = parse_indicator_style(long_value(inline, args)?)?;
            return Ok(false);
        }
        "tabsize" => {
            options.tabsize = parse_width(long_value(inline, args)?)?;
            return Ok(false);
        }
        "time" => {
            options.time_field = parse_time_field(long_value(inline, args)?)?;
            state.time_selected = true;
            return Ok(false);
        }
        "time-style" => {
            options.time_style = parse_time_style(long_value(inline, args)?)?;
            return Ok(false);
        }
        "sort" => {
            state.explicit_sort = Some(parse_sort(long_value(inline, args)?)?);
            return Ok(false);
        }
        "quoting-style" => {
            options.quoting = Some(parse_quoting_style(long_value(inline, args)?)?);
            return Ok(false);
        }
        "width" => {
            options.width = Some(parse_width(long_value(inline, args)?)?);
            return Ok(false);
        }
        "color" | "colour" => {
            options.color = parse_color_when(inline)?;
            return Ok(false);
        }
        _ => return Err(LsError::Usage),
    }
    // A value on a long option that takes none (`--all=x`) is a usage error,
    // not a silently ignored token.
    if inline.is_some() {
        return Err(LsError::Usage);
    }
    Ok(false)
}

/// Apply one short-option cluster (`-la`, `-w80`, …). Returns `Ok(true)`
/// when the cluster requests short help (`-?`), so the caller returns
/// [`Command::Help`].
// One flat short-option dispatch match, like `apply_long`; splitting it would
// scatter the option table across helpers for no gain.
#[allow(clippy::too_many_lines)]
fn apply_short<'a>(
    letters: &'a str,
    state: &mut ParseState,
    args: &mut core::slice::Iter<'a, &'a str>,
) -> Result<bool, LsError> {
    let options = &mut state.options;
    let mut rest = letters;
    while let Some(letter) = rest.chars().next() {
        rest = &rest[letter.len_utf8()..];
        match letter {
            'a' => options.hidden = Hidden::All,
            'A' => options.hidden = Hidden::AlmostAll,
            'C' => options.format = Some(Format::Columns),
            'd' => options.directory = true,
            'F' => options.indicator = Indicator::Classify,
            'g' => {
                options.format = Some(Format::Long);
                options.hide_owner = true;
            }
            'G' => options.hide_group = true,
            'L' => options.dereference = Some(Dereference::Always),
            'H' => options.dereference = Some(Dereference::CommandLine),
            'h' => {
                options.file_size = SizeFormat::Human { si: false };
                options.block_size = SizeFormat::Human { si: false };
                state.size_explicit = true;
            }
            'k' => state.kibibytes = true,
            // `-n` selects the long format like `-l`; its numeric
            // owner/group is already the only output.
            'l' | 'n' => options.format = Some(Format::Long),
            'm' => options.format = Some(Format::Commas),
            'o' => {
                options.format = Some(Format::Long);
                options.hide_group = true;
            }
            'p' => options.indicator = Indicator::Slash,
            'N' => options.quoting = Some(QuotingStyle::Literal),
            'Q' => options.quoting = Some(QuotingStyle::C),
            'b' => options.quoting = Some(QuotingStyle::Escape),
            'q' => options.hide_control_chars = Some(true),
            'r' => options.reverse = true,
            'R' => options.recursive = true,
            's' => options.size = true,
            'S' => state.explicit_sort = Some(Sort::Size),
            't' => state.explicit_sort = Some(Sort::Time),
            'U' => state.explicit_sort = Some(Sort::None),
            'X' => state.explicit_sort = Some(Sort::Extension),
            'v' => state.explicit_sort = Some(Sort::Version),
            // `-f` enables `-a` and `-U` and turns off `-l`/`-s`, in order:
            // a later long/size/sort flag in the same line overrides it.
            'f' => {
                options.hidden = Hidden::All;
                if options.is_long() {
                    options.format = None;
                }
                options.size = false;
                state.explicit_sort = Some(Sort::None);
            }
            'c' => {
                options.time_field = TimeField::Changed;
                state.time_selected = true;
            }
            'u' => {
                options.time_field = TimeField::Accessed;
                state.time_selected = true;
            }
            'i' => options.inode = true,
            'B' => state.filters.ignore_backups = true,
            // `-w` takes a value: the rest of the cluster, or the next
            // argument when it ends the cluster.
            'w' => {
                let raw = if rest.is_empty() {
                    *args.next().ok_or(LsError::Usage)?
                } else {
                    rest
                };
                options.width = Some(parse_width(raw)?);
                break;
            }
            // `-I` takes a value like `-w`: the rest of the cluster, or the
            // next argument when it ends the cluster. A malformed pattern is
            // a usage error, never a silently ignored filter (fail closed).
            'I' => {
                let raw = if rest.is_empty() {
                    *args.next().ok_or(LsError::Usage)?
                } else {
                    rest
                };
                state
                    .filters
                    .ignore
                    .push(Pattern::new(raw).map_err(|_| LsError::Usage)?);
                break;
            }
            // `-T` takes a value like `-w`: the rest of the cluster, or the
            // next argument when it ends the cluster.
            'T' => {
                let raw = if rest.is_empty() {
                    *args.next().ok_or(LsError::Usage)?
                } else {
                    rest
                };
                options.tabsize = parse_width(raw)?;
                break;
            }
            'x' => options.format = Some(Format::Across),
            // `-1` has no effect after `-l`: the long format wins (the GNU
            // rule), so a column arrangement is only selected when the long
            // format is not already chosen.
            '1' => {
                if !options.is_long() {
                    options.format = Some(Format::OnePerLine);
                }
            }
            '?' => return Ok(true),
            _ => return Err(LsError::Usage),
        }
    }
    Ok(false)
}

/// Parse a `-w` / `--width` value: a non-negative decimal integer, where
/// `0` selects an unlimited line length (the GNU meaning). Anything else —
/// an empty string, a sign, a non-digit, or an overflowing value — is a
/// usage error, never a guessed-at width (fail closed).
fn parse_width(raw: &str) -> Result<usize, LsError> {
    raw.parse::<usize>().map_err(|_| LsError::Usage)
}

/// Parse a `--sort=WORD` value into the [`Sort`] key it selects. The words
/// are the GNU set (`none`/`size`/`time`/`version`/`extension`/`name`);
/// anything else is a usage error (fail closed), never a silently ignored
/// token.
fn parse_sort(raw: &str) -> Result<Sort, LsError> {
    match raw {
        "none" => Ok(Sort::None),
        "size" => Ok(Sort::Size),
        "time" => Ok(Sort::Time),
        "version" => Ok(Sort::Version),
        "extension" => Ok(Sort::Extension),
        "name" => Ok(Sort::Name),
        _ => Err(LsError::Usage),
    }
}

/// Parse a `--time=WORD` value into the timestamp it selects. The words are
/// the GNU set; anything else is a usage error (fail closed), never a
/// silently ignored token.
fn parse_time_field(raw: &str) -> Result<TimeField, LsError> {
    match raw {
        "atime" | "access" | "use" => Ok(TimeField::Accessed),
        "ctime" | "status" => Ok(TimeField::Changed),
        "mtime" | "modification" => Ok(TimeField::Modified),
        "birth" | "creation" => Ok(TimeField::Created),
        _ => Err(LsError::Usage),
    }
}

/// Parse a `--time-style=WORD` value into a [`TimeStyle`].
///
/// The named GNU styles are supported. A custom `+FORMAT` style is refused
/// (a documented divergence, fail closed): a `strftime`-style formatter is
/// deferred until the `date` command that will share it exists, rather than
/// shipped here for one consumer. Any other word is a usage error.
fn parse_time_style(raw: &str) -> Result<TimeStyle, LsError> {
    match raw {
        "full-iso" => Ok(TimeStyle::FullIso),
        "long-iso" => Ok(TimeStyle::LongIso),
        "iso" => Ok(TimeStyle::Iso),
        "locale" => Ok(TimeStyle::Locale),
        _ => Err(LsError::Usage),
    }
}

/// Parse a `--quoting-style=WORD` value into a [`QuotingStyle`].
///
/// The `locale` and `clocale` styles are refused (a documented divergence,
/// fail closed): they select quotation marks from the active locale, which
/// TAIRiX has no infrastructure for, so they are not faked. Any other word
/// is a usage error, never a silently ignored token.
fn parse_quoting_style(raw: &str) -> Result<QuotingStyle, LsError> {
    match raw {
        "literal" => Ok(QuotingStyle::Literal),
        "shell" => Ok(QuotingStyle::Shell),
        "shell-always" => Ok(QuotingStyle::ShellAlways),
        "shell-escape" => Ok(QuotingStyle::ShellEscape),
        "shell-escape-always" => Ok(QuotingStyle::ShellEscapeAlways),
        "c" => Ok(QuotingStyle::C),
        "escape" => Ok(QuotingStyle::Escape),
        _ => Err(LsError::Usage),
    }
}

/// Parse a `--color[=WHEN]` value into a [`ColorChoice`].
///
/// `WHEN` is optional: a bare `--color` (no inline value) means `always`, the
/// GNU rule — and, since the value is optional, `--color` never consumes the
/// following argument. The GNU synonym sets are accepted for an explicit
/// value: `never`/`no`/`none`, `always`/`yes`/`force`, and `auto`/`tty`/
/// `if-tty`. The British `--colour` spelling maps here too. Any other word is
/// a usage error (fail closed), never a silently ignored token.
fn parse_color_when(inline: Option<&str>) -> Result<ColorChoice, LsError> {
    let Some(word) = inline else {
        return Ok(ColorChoice::Always);
    };
    match word {
        "never" | "no" | "none" => Ok(ColorChoice::Never),
        "always" | "yes" | "force" => Ok(ColorChoice::Always),
        "auto" | "tty" | "if-tty" => Ok(ColorChoice::Auto),
        _ => Err(LsError::Usage),
    }
}

/// Parse a `--format=WORD` value into the [`Format`] it selects. The words
/// are the GNU set; anything else is a usage error (fail closed), never a
/// silently ignored token.
fn parse_format(raw: &str) -> Result<Format, LsError> {
    match raw {
        "long" | "verbose" => Ok(Format::Long),
        "single-column" => Ok(Format::OnePerLine),
        "vertical" => Ok(Format::Columns),
        "across" | "horizontal" => Ok(Format::Across),
        "commas" => Ok(Format::Commas),
        _ => Err(LsError::Usage),
    }
}

/// Parse an `--indicator-style=WORD` value into the [`Indicator`] it
/// selects. The words are the GNU set; anything else is a usage error (fail
/// closed), never a silently ignored token.
fn parse_indicator_style(raw: &str) -> Result<Indicator, LsError> {
    match raw {
        "none" => Ok(Indicator::None),
        "slash" => Ok(Indicator::Slash),
        "file-type" => Ok(Indicator::FileType),
        "classify" => Ok(Indicator::Classify),
        _ => Err(LsError::Usage),
    }
}

/// Parse a `--block-size=SIZE` value into a [`SizeFormat::Scaled`].
///
/// The grammar is GNU's: an optional decimal coefficient (at least `1`)
/// followed by an optional unit. The unit letter is `K`/`M`/`G`/`T`/`P`/`E`
/// (case-insensitive); it may be followed by `iB` (the binary spelling, e.g.
/// `KiB` = 1024) or `B` (the decimal spelling, e.g. `kB` = 1000). A bare
/// letter (no `iB`/`B`) is the binary unit (`K` = 1024). When a numeric
/// coefficient is given, GNU prints no unit suffix; a bare unit prints its
/// canonical suffix. A zero or missing coefficient, an unknown or malformed
/// suffix, an empty value, or an overflow is a usage error (fail closed).
fn parse_block_size(raw: &str) -> Result<SizeFormat, LsError> {
    let digits_end = raw.find(|c: char| !c.is_ascii_digit()).unwrap_or(raw.len());
    let (digits, unit) = raw.split_at(digits_end);
    let has_coefficient = !digits.is_empty();
    let coefficient: u64 = if has_coefficient {
        // A leading `0` (or an overflow) is rejected: a block size is at
        // least one byte.
        match digits.parse::<u64>() {
            Ok(value) if value >= 1 => value,
            _ => return Err(LsError::Usage),
        }
    } else {
        1
    };

    if unit.is_empty() {
        // A bare number scales by that many bytes with no suffix
        // (`--block-size=1024`); a completely empty SIZE is invalid.
        if !has_coefficient {
            return Err(LsError::Usage);
        }
        return Ok(SizeFormat::Scaled {
            unit: coefficient,
            suffix: None,
        });
    }

    // The unit is a power letter optionally followed by `iB` or `B`.
    let mut chars = unit.chars();
    let letter = chars.next().ok_or(LsError::Usage)?;
    let power = match letter.to_ascii_uppercase() {
        'K' => 1u32,
        'M' => 2,
        'G' => 3,
        'T' => 4,
        'P' => 5,
        'E' => 6,
        _ => return Err(LsError::Usage),
    };
    let (base, form) = match chars.as_str() {
        "" => (1024u64, SuffixForm::Iec),
        "iB" => (1024, SuffixForm::IecLong),
        "B" => (1000, SuffixForm::Si),
        _ => return Err(LsError::Usage),
    };
    let multiplier = base.checked_pow(power).ok_or(LsError::Usage)?;
    let unit = coefficient.checked_mul(multiplier).ok_or(LsError::Usage)?;
    // GNU prints the unit suffix only for a bare unit (no coefficient).
    let suffix = (!has_coefficient).then_some(UnitSuffix {
        letter: letter.to_ascii_uppercase() as u8,
        form,
    });
    Ok(SizeFormat::Scaled { unit, suffix })
}

#[cfg(test)]
mod tests {
    use super::{
        parse, ColorChoice, Command, Filters, Format, Hidden, Indicator, Options, QuotingStyle,
        SizeFormat, Sort, TimeField, TimeStyle,
    };
    use crate::error::LsError;
    use alloc::format;
    use alloc::string::String;
    use alloc::vec::Vec;
    use tairix_glob::Pattern;

    fn list(options: Options, paths: &[&str]) -> Command {
        listing(options, Filters::default(), paths)
    }

    fn listing(options: Options, filters: Filters, paths: &[&str]) -> Command {
        Command::List {
            options,
            filters,
            paths: paths.iter().map(|p| String::from(*p)).collect::<Vec<_>>(),
        }
    }

    #[test]
    fn no_arguments_lists_current_directory() {
        assert_eq!(parse(&[]), Ok(list(Options::DEFAULT, &["."])));
    }

    #[test]
    fn help_flags_win() {
        assert_eq!(parse(&["-?"]), Ok(Command::Help));
        assert_eq!(parse(&["--help"]), Ok(Command::Help));
        // `-?` wins even from inside a cluster and alongside other arguments.
        assert_eq!(parse(&["-l?", "dir"]), Ok(Command::Help));
    }

    #[test]
    fn h_is_human_readable_not_help() {
        // As in the GNU tool, `-h` scales sizes; it is not the help switch.
        assert_eq!(
            parse(&["-lh"]),
            Ok(list(
                Options {
                    format: Some(Format::Long),
                    file_size: SizeFormat::Human { si: false },
                    block_size: SizeFormat::Human { si: false },
                    ..Options::DEFAULT
                },
                &["."],
            ))
        );
        assert_eq!(
            parse(&["--human-readable"]),
            Ok(list(
                Options {
                    file_size: SizeFormat::Human { si: false },
                    block_size: SizeFormat::Human { si: false },
                    ..Options::DEFAULT
                },
                &["."],
            ))
        );
    }

    #[test]
    fn all_and_long_flags_set_their_fields() {
        assert_eq!(
            parse(&["-a"]),
            Ok(list(
                Options {
                    hidden: Hidden::All,
                    ..Options::DEFAULT
                },
                &["."],
            ))
        );
        assert_eq!(
            parse(&["-l"]),
            Ok(list(
                Options {
                    format: Some(Format::Long),
                    ..Options::DEFAULT
                },
                &["."],
            ))
        );
        assert_eq!(
            parse(&["--all", "-l"]),
            Ok(list(
                Options {
                    hidden: Hidden::All,
                    format: Some(Format::Long),
                    ..Options::DEFAULT
                },
                &["."],
            ))
        );
    }

    #[test]
    fn almost_all_and_directory_parse() {
        assert_eq!(
            parse(&["-A", "-d"]),
            Ok(list(
                Options {
                    hidden: Hidden::AlmostAll,
                    directory: true,
                    ..Options::DEFAULT
                },
                &["."],
            ))
        );
        assert_eq!(
            parse(&["--almost-all", "--directory"]),
            Ok(list(
                Options {
                    hidden: Hidden::AlmostAll,
                    directory: true,
                    ..Options::DEFAULT
                },
                &["."],
            ))
        );
    }

    #[test]
    fn g_and_o_imply_long_and_hide_a_column() {
        assert_eq!(
            parse(&["-g"]),
            Ok(list(
                Options {
                    format: Some(Format::Long),
                    hide_owner: true,
                    ..Options::DEFAULT
                },
                &["."],
            ))
        );
        assert_eq!(
            parse(&["-o"]),
            Ok(list(
                Options {
                    format: Some(Format::Long),
                    hide_group: true,
                    ..Options::DEFAULT
                },
                &["."],
            ))
        );
    }

    #[test]
    fn numeric_uid_gid_implies_long() {
        for args in [["-n"], ["--numeric-uid-gid"]] {
            assert_eq!(
                parse(&args),
                Ok(list(
                    Options {
                        format: Some(Format::Long),
                        ..Options::DEFAULT
                    },
                    &["."],
                ))
            );
        }
    }

    #[test]
    fn format_flags_last_one_wins() {
        assert_eq!(
            parse(&["-m"]),
            Ok(list(
                Options {
                    format: Some(Format::Commas),
                    ..Options::DEFAULT
                },
                &["."],
            ))
        );
        // The later format flag wins; an explicit `-1` is not the same as
        // the unset default (which decides against the terminal at render
        // time).
        assert_eq!(
            parse(&["-m", "-1"]),
            Ok(list(
                Options {
                    format: Some(Format::OnePerLine),
                    ..Options::DEFAULT
                },
                &["."],
            ))
        );
        assert_eq!(
            parse(&["-1", "-C"]),
            Ok(list(
                Options {
                    format: Some(Format::Columns),
                    ..Options::DEFAULT
                },
                &["."],
            ))
        );
        assert_eq!(
            parse(&["-Cx"]),
            Ok(list(
                Options {
                    format: Some(Format::Across),
                    ..Options::DEFAULT
                },
                &["."],
            ))
        );
    }

    #[test]
    fn width_parses_in_every_spelling() {
        // `-w 80` (separate argument), `-w80` (rest of the cluster),
        // `-lw80` (rest of a longer cluster), `--width 80`, `--width=80`.
        for args in [
            &["-w", "80"][..],
            &["-w80"][..],
            &["--width", "80"][..],
            &["--width=80"][..],
        ] {
            assert_eq!(
                parse(args),
                Ok(list(
                    Options {
                        width: Some(80),
                        ..Options::DEFAULT
                    },
                    &["."],
                ))
            );
        }
        assert_eq!(
            parse(&["-lw80"]),
            Ok(list(
                Options {
                    format: Some(Format::Long),
                    width: Some(80),
                    ..Options::DEFAULT
                },
                &["."],
            ))
        );
        // `0` is the GNU "unlimited line length" value.
        assert_eq!(
            parse(&["-w0"]),
            Ok(list(
                Options {
                    width: Some(0),
                    ..Options::DEFAULT
                },
                &["."],
            ))
        );
    }

    #[test]
    fn a_bad_or_missing_width_is_a_usage_error() {
        assert_eq!(parse(&["-w"]), Err(LsError::Usage));
        assert_eq!(parse(&["--width"]), Err(LsError::Usage));
        assert_eq!(parse(&["-w", "wide"]), Err(LsError::Usage));
        assert_eq!(parse(&["--width=-5"]), Err(LsError::Usage));
        assert_eq!(parse(&["-wx"]), Err(LsError::Usage));
    }

    #[test]
    fn a_value_on_a_valueless_long_option_is_a_usage_error() {
        assert_eq!(parse(&["--all=yes"]), Err(LsError::Usage));
    }

    #[test]
    fn indicator_flags_last_one_wins() {
        assert_eq!(
            parse(&["-p"]),
            Ok(list(
                Options {
                    indicator: Indicator::Slash,
                    ..Options::DEFAULT
                },
                &["."],
            ))
        );
        assert_eq!(
            parse(&["-p", "-F"]),
            Ok(list(
                Options {
                    indicator: Indicator::Classify,
                    ..Options::DEFAULT
                },
                &["."],
            ))
        );
    }

    #[test]
    fn size_flag_parses_in_both_spellings() {
        for args in [["-s"], ["--size"]] {
            assert_eq!(
                parse(&args),
                Ok(list(
                    Options {
                        size: true,
                        ..Options::DEFAULT
                    },
                    &["."],
                ))
            );
        }
        // `-s` (allocated blocks) and `-S` (sort by size) stay distinct.
        assert_eq!(
            parse(&["-sS"]),
            Ok(list(
                Options {
                    size: true,
                    sort: Sort::Size,
                    ..Options::DEFAULT
                },
                &["."],
            ))
        );
    }

    #[test]
    fn sort_reverse_recursive_and_quote_parse() {
        assert_eq!(
            parse(&["-S", "-r", "-R", "-Q"]),
            Ok(list(
                Options {
                    sort: Sort::Size,
                    reverse: true,
                    recursive: true,
                    quoting: Some(QuotingStyle::C),
                    ..Options::DEFAULT
                },
                &["."],
            ))
        );
        assert_eq!(
            parse(&["--reverse", "--recursive", "--quote-name"]),
            Ok(list(
                Options {
                    reverse: true,
                    recursive: true,
                    quoting: Some(QuotingStyle::C),
                    ..Options::DEFAULT
                },
                &["."],
            ))
        );
    }

    #[test]
    fn quoting_flags_select_their_style_and_last_wins() {
        // Each short/long quoting flag selects its style.
        let cases: &[(&[&str], QuotingStyle)] = &[
            (&["-N"], QuotingStyle::Literal),
            (&["--literal"], QuotingStyle::Literal),
            (&["-Q"], QuotingStyle::C),
            (&["--quote-name"], QuotingStyle::C),
            (&["-b"], QuotingStyle::Escape),
            (&["--escape"], QuotingStyle::Escape),
        ];
        for &(args, style) in cases {
            assert_eq!(
                parse(args),
                Ok(list(
                    Options {
                        quoting: Some(style),
                        ..Options::DEFAULT
                    },
                    &["."],
                ))
            );
        }
        // The last quoting flag wins, whatever the spelling.
        assert_eq!(
            parse(&["-Q", "-b", "-N"]),
            Ok(list(
                Options {
                    quoting: Some(QuotingStyle::Literal),
                    ..Options::DEFAULT
                },
                &["."],
            ))
        );
        assert_eq!(
            parse(&["-N", "--quote-name"]),
            Ok(list(
                Options {
                    quoting: Some(QuotingStyle::C),
                    ..Options::DEFAULT
                },
                &["."],
            ))
        );
    }

    #[test]
    fn quoting_style_word_selects_the_style() {
        for (word, style) in [
            ("literal", QuotingStyle::Literal),
            ("shell", QuotingStyle::Shell),
            ("shell-always", QuotingStyle::ShellAlways),
            ("shell-escape", QuotingStyle::ShellEscape),
            ("shell-escape-always", QuotingStyle::ShellEscapeAlways),
            ("c", QuotingStyle::C),
            ("escape", QuotingStyle::Escape),
        ] {
            let expected = Ok(list(
                Options {
                    quoting: Some(style),
                    ..Options::DEFAULT
                },
                &["."],
            ));
            assert_eq!(parse(&["--quoting-style", word]), expected.clone());
            assert_eq!(
                parse(&[&alloc::format!("--quoting-style={word}")]),
                expected
            );
        }
        // The locale-dependent styles are refused (fail closed), as is any
        // unknown word.
        assert_eq!(parse(&["--quoting-style=locale"]), Err(LsError::Usage));
        assert_eq!(parse(&["--quoting-style=clocale"]), Err(LsError::Usage));
        assert_eq!(parse(&["--quoting-style=bogus"]), Err(LsError::Usage));
    }

    #[test]
    fn hide_and_show_control_chars_parse() {
        for args in [["-q"], ["--hide-control-chars"]] {
            assert_eq!(
                parse(&args),
                Ok(list(
                    Options {
                        hide_control_chars: Some(true),
                        ..Options::DEFAULT
                    },
                    &["."],
                ))
            );
        }
        assert_eq!(
            parse(&["--show-control-chars"]),
            Ok(list(
                Options {
                    hide_control_chars: Some(false),
                    ..Options::DEFAULT
                },
                &["."],
            ))
        );
    }

    #[test]
    fn inode_flag_parses_in_both_spellings() {
        for args in [["-i"], ["--inode"]] {
            assert_eq!(
                parse(&args),
                Ok(list(
                    Options {
                        inode: true,
                        ..Options::DEFAULT
                    },
                    &["."],
                ))
            );
        }
    }

    #[test]
    fn time_sort_and_time_fields_parse() {
        // `-t` sorts by time; `-u` / `-c` pick the access / change time and,
        // without `-l`, also make the sort a time sort (the GNU rule).
        assert_eq!(
            parse(&["-t"]),
            Ok(list(
                Options {
                    sort: Sort::Time,
                    ..Options::DEFAULT
                },
                &["."],
            ))
        );
        assert_eq!(
            parse(&["-u"]),
            Ok(list(
                Options {
                    sort: Sort::Time,
                    time_field: TimeField::Accessed,
                    ..Options::DEFAULT
                },
                &["."],
            ))
        );
        assert_eq!(
            parse(&["-c"]),
            Ok(list(
                Options {
                    sort: Sort::Time,
                    time_field: TimeField::Changed,
                    ..Options::DEFAULT
                },
                &["."],
            ))
        );
    }

    #[test]
    fn the_long_format_holds_the_c_and_u_sort_at_name() {
        // With `-l`, `-c` / `-u` choose the shown time but leave the sort by
        // name; only an explicit `-t` makes it a time sort.
        assert_eq!(
            parse(&["-lc"]),
            Ok(list(
                Options {
                    format: Some(Format::Long),
                    sort: Sort::Name,
                    time_field: TimeField::Changed,
                    ..Options::DEFAULT
                },
                &["."],
            ))
        );
        assert_eq!(
            parse(&["-ltc"]),
            Ok(list(
                Options {
                    format: Some(Format::Long),
                    sort: Sort::Time,
                    time_field: TimeField::Changed,
                    ..Options::DEFAULT
                },
                &["."],
            ))
        );
    }

    #[test]
    fn explicit_sort_wins_over_the_time_field_default() {
        // `-S` is an explicit sort, so `-c` only picks the time field.
        assert_eq!(
            parse(&["-cS"]),
            Ok(list(
                Options {
                    sort: Sort::Size,
                    time_field: TimeField::Changed,
                    ..Options::DEFAULT
                },
                &["."],
            ))
        );
    }

    #[test]
    fn new_sort_flags_set_the_key() {
        for (arg, sort) in [
            ("-U", Sort::None),
            ("-X", Sort::Extension),
            ("-v", Sort::Version),
        ] {
            assert_eq!(
                parse(&[arg]),
                Ok(list(
                    Options {
                        sort,
                        ..Options::DEFAULT
                    },
                    &["."],
                )),
                "{arg}"
            );
        }
    }

    #[test]
    fn sort_word_selects_the_key_in_both_forms() {
        for (word, sort) in [
            ("none", Sort::None),
            ("size", Sort::Size),
            ("time", Sort::Time),
            ("version", Sort::Version),
            ("extension", Sort::Extension),
            ("name", Sort::Name),
        ] {
            let inline = alloc::format!("--sort={word}");
            assert_eq!(
                parse(&[inline.as_str()]),
                Ok(list(
                    Options {
                        sort,
                        ..Options::DEFAULT
                    },
                    &["."],
                )),
                "{word}"
            );
            assert_eq!(
                parse(&["--sort", word]),
                Ok(list(
                    Options {
                        sort,
                        ..Options::DEFAULT
                    },
                    &["."],
                )),
                "{word}"
            );
        }
        assert_eq!(parse(&["--sort=bogus"]), Err(LsError::Usage));
        assert_eq!(parse(&["--sort"]), Err(LsError::Usage));
    }

    #[test]
    fn group_directories_first_parses() {
        assert_eq!(
            parse(&["--group-directories-first"]),
            Ok(list(
                Options {
                    group_directories_first: true,
                    ..Options::DEFAULT
                },
                &["."],
            ))
        );
    }

    #[test]
    fn f_shows_all_disables_long_and_size_and_stops_sorting() {
        assert_eq!(
            parse(&["-f"]),
            Ok(list(
                Options {
                    hidden: Hidden::All,
                    sort: Sort::None,
                    ..Options::DEFAULT
                },
                &["."],
            ))
        );
        // `-f` clears an earlier `-l`/`-s` and an earlier sort selection.
        assert_eq!(
            parse(&["-lsSf"]),
            Ok(list(
                Options {
                    hidden: Hidden::All,
                    sort: Sort::None,
                    ..Options::DEFAULT
                },
                &["."],
            ))
        );
    }

    #[test]
    fn a_later_flag_overrides_f() {
        // Order matters: `-l` and `-t` after `-f` win (the GNU semantics).
        assert_eq!(
            parse(&["-flt"]),
            Ok(list(
                Options {
                    hidden: Hidden::All,
                    format: Some(Format::Long),
                    sort: Sort::Time,
                    ..Options::DEFAULT
                },
                &["."],
            ))
        );
    }

    #[test]
    fn time_word_selects_the_field_in_both_forms() {
        for (args, field) in [
            (&["--time=atime"][..], TimeField::Accessed),
            (&["--time", "access"][..], TimeField::Accessed),
            (&["--time=ctime"][..], TimeField::Changed),
            (&["--time=status"][..], TimeField::Changed),
            (&["--time=mtime"][..], TimeField::Modified),
            (&["--time=birth"][..], TimeField::Created),
        ] {
            // Without `-l`, selecting a time makes the sort a time sort.
            assert_eq!(
                parse(args),
                Ok(list(
                    Options {
                        sort: Sort::Time,
                        time_field: field,
                        ..Options::DEFAULT
                    },
                    &["."],
                )),
                "{args:?}"
            );
        }
        assert_eq!(parse(&["--time=bogus"]), Err(LsError::Usage));
        assert_eq!(parse(&["--time"]), Err(LsError::Usage));
    }

    #[test]
    fn time_style_and_full_time_parse() {
        for (word, style) in [
            ("locale", TimeStyle::Locale),
            ("long-iso", TimeStyle::LongIso),
            ("full-iso", TimeStyle::FullIso),
            ("iso", TimeStyle::Iso),
        ] {
            let arg = alloc::format!("--time-style={word}");
            assert_eq!(
                parse(&[arg.as_str()]),
                Ok(list(
                    Options {
                        time_style: style,
                        ..Options::DEFAULT
                    },
                    &["."],
                )),
                "{word}"
            );
        }
        // `--full-time` is `-l --time-style=full-iso`.
        assert_eq!(
            parse(&["--full-time"]),
            Ok(list(
                Options {
                    format: Some(Format::Long),
                    time_style: TimeStyle::FullIso,
                    ..Options::DEFAULT
                },
                &["."],
            ))
        );
        // A custom `+FORMAT` and an unknown word both fail closed.
        assert_eq!(parse(&["--time-style=+%Y"]), Err(LsError::Usage));
        assert_eq!(parse(&["--time-style=bogus"]), Err(LsError::Usage));
    }

    #[test]
    fn clustered_short_options_set_all_their_fields() {
        assert_eq!(
            parse(&["-laF", "dir"]),
            Ok(list(
                Options {
                    hidden: Hidden::All,
                    format: Some(Format::Long),
                    indicator: Indicator::Classify,
                    ..Options::DEFAULT
                },
                &["dir"],
            ))
        );
    }

    #[test]
    fn ignore_backups_parses_in_both_spellings() {
        let filters = Filters {
            ignore_backups: true,
            ..Filters::default()
        };
        assert_eq!(
            parse(&["-B"]),
            Ok(listing(Options::DEFAULT, filters.clone(), &["."]))
        );
        assert_eq!(
            parse(&["--ignore-backups"]),
            Ok(listing(Options::DEFAULT, filters, &["."]))
        );
    }

    #[test]
    fn ignore_patterns_collect_in_order_from_both_spellings() {
        let filters = Filters {
            ignore: alloc::vec![
                Pattern::new("*.o").expect("valid glob"),
                Pattern::new("tmp*").expect("valid glob"),
            ],
            ..Filters::default()
        };
        assert_eq!(
            parse(&["-I", "*.o", "--ignore=tmp*"]),
            Ok(listing(Options::DEFAULT, filters, &["."]))
        );
    }

    #[test]
    fn ignore_takes_its_value_from_the_cluster_tail() {
        let filters = Filters {
            ignore: alloc::vec![Pattern::new("*.tmp").expect("valid glob")],
            ..Filters::default()
        };
        assert_eq!(
            parse(&["-I*.tmp"]),
            Ok(listing(Options::DEFAULT, filters, &["."]))
        );
    }

    #[test]
    fn hide_patterns_are_separate_from_ignore() {
        let filters = Filters {
            hide: alloc::vec![Pattern::new("*~").expect("valid glob")],
            ..Filters::default()
        };
        assert_eq!(
            parse(&["--hide=*~"]),
            Ok(listing(Options::DEFAULT, filters, &["."]))
        );
    }

    #[test]
    fn a_malformed_or_missing_ignore_pattern_is_a_usage_error() {
        // A malformed glob fails closed rather than becoming a filter that
        // silently matches nothing.
        assert_eq!(parse(&["-I", "["]), Err(LsError::Usage));
        assert_eq!(parse(&["--hide=[a-"]), Err(LsError::Usage));
        // A value-taking option with no value fails closed.
        assert_eq!(parse(&["-I"]), Err(LsError::Usage));
        assert_eq!(parse(&["--ignore"]), Err(LsError::Usage));
    }

    #[test]
    fn paths_preserve_order() {
        assert_eq!(
            parse(&["a", "b", "c"]),
            Ok(list(Options::DEFAULT, &["a", "b", "c"]))
        );
    }

    #[test]
    fn bare_dash_is_a_path() {
        assert_eq!(parse(&["-"]), Ok(list(Options::DEFAULT, &["-"])));
    }

    #[test]
    fn unknown_option_is_usage() {
        assert_eq!(parse(&["-Z"]), Err(LsError::Usage));
        assert_eq!(parse(&["--frobnicate"]), Err(LsError::Usage));
        // The retired invented synonym for `-l` stays retired.
        assert_eq!(parse(&["--long"]), Err(LsError::Usage));
        // An unknown letter inside an otherwise-valid cluster still fails.
        assert_eq!(parse(&["-lZ"]), Err(LsError::Usage));
    }

    #[test]
    fn double_dash_ends_options() {
        // After `--`, a leading-dash argument is a path, not an option.
        assert_eq!(parse(&["--", "-l"]), Ok(list(Options::DEFAULT, &["-l"])));
    }

    fn size_of(args: &[&str]) -> (SizeFormat, SizeFormat) {
        match parse(args) {
            Ok(Command::List { options, .. }) => (options.file_size, options.block_size),
            other => panic!("expected a listing, got {other:?}"),
        }
    }

    #[test]
    fn human_readable_and_si_set_both_scalings() {
        for args in [["-h"], ["--human-readable"]] {
            assert_eq!(
                size_of(&args),
                (
                    SizeFormat::Human { si: false },
                    SizeFormat::Human { si: false }
                )
            );
        }
        assert_eq!(
            size_of(&["--si"]),
            (
                SizeFormat::Human { si: true },
                SizeFormat::Human { si: true }
            )
        );
        // The last size flag wins for both scalings.
        assert_eq!(
            size_of(&["-h", "--si"]),
            (
                SizeFormat::Human { si: true },
                SizeFormat::Human { si: true }
            )
        );
    }

    #[test]
    fn kibibytes_confirms_the_default_and_yields_to_a_size_option() {
        // `-k` alone leaves the defaults (bytes file / 1024-block).
        assert_eq!(
            size_of(&["-k"]),
            (SizeFormat::BYTES, SizeFormat::KIBI_BLOCKS)
        );
        // `-k` never overrides an explicit `-h`, in either order.
        for args in [["-h", "-k"], ["-k", "-h"]] {
            assert_eq!(
                size_of(&args),
                (
                    SizeFormat::Human { si: false },
                    SizeFormat::Human { si: false }
                )
            );
        }
    }

    #[test]
    fn block_size_scales_both_columns() {
        // A plain integer scales by bytes with no suffix.
        let scaled = SizeFormat::Scaled {
            unit: 1024,
            suffix: None,
        };
        assert_eq!(size_of(&["--block-size=1024"]), (scaled, scaled));
        // `--block-size=1` is plain bytes.
        assert_eq!(
            size_of(&["--block-size=1"]),
            (SizeFormat::BYTES, SizeFormat::BYTES)
        );
    }

    #[test]
    fn block_size_units_carry_their_suffix() {
        // A bare unit keeps its canonical printed suffix; `KiB` and `kB`
        // differ from `K` in spelling and base.
        let cases: &[(&str, u64, &str)] = &[
            ("K", 1024, "K"),
            ("KiB", 1024, "KiB"),
            ("kB", 1000, "kB"),
            ("M", 1024 * 1024, "M"),
            ("MiB", 1024 * 1024, "MiB"),
            ("MB", 1_000_000, "MB"),
            ("G", 1024 * 1024 * 1024, "G"),
        ];
        for &(word, unit, text) in cases {
            let (file, block) = size_of(&[&format!("--block-size={word}")]);
            match file {
                SizeFormat::Scaled {
                    unit: got,
                    suffix: Some(suffix),
                } => {
                    assert_eq!(got, unit, "{word} unit");
                    assert_eq!(suffix.text(), text, "{word} suffix");
                }
                other => panic!("{word} -> {other:?}"),
            }
            assert_eq!(file, block, "{word} scales both columns");
        }
    }

    #[test]
    fn block_size_coefficient_suppresses_the_suffix() {
        // A coefficient multiplies the unit and drops the printed suffix.
        assert_eq!(
            size_of(&["--block-size=2K"]),
            (
                SizeFormat::Scaled {
                    unit: 2048,
                    suffix: None,
                },
                SizeFormat::Scaled {
                    unit: 2048,
                    suffix: None,
                }
            )
        );
    }

    #[test]
    fn a_malformed_block_size_is_a_usage_error() {
        for bad in ["", "0", "-1", "1.5K", "1x", "foo", "b", "Ki", "KIB", "Kb"] {
            assert_eq!(
                parse(&[&format!("--block-size={bad}")]),
                Err(LsError::Usage),
                "--block-size={bad}"
            );
        }
        // Missing value fails closed too.
        assert_eq!(parse(&["--block-size"]), Err(LsError::Usage));
    }

    #[test]
    fn format_word_selects_the_arrangement() {
        for (word, format) in [
            ("long", Format::Long),
            ("verbose", Format::Long),
            ("single-column", Format::OnePerLine),
            ("vertical", Format::Columns),
            ("across", Format::Across),
            ("horizontal", Format::Across),
            ("commas", Format::Commas),
        ] {
            assert_eq!(
                parse(&[&format!("--format={word}")]),
                Ok(list(
                    Options {
                        format: Some(format),
                        ..Options::DEFAULT
                    },
                    &["."],
                )),
                "{word}"
            );
        }
        assert_eq!(parse(&["--format=bogus"]), Err(LsError::Usage));
    }

    #[test]
    fn one_per_line_has_no_effect_after_long() {
        // The GNU rule: `-l -1` stays long, but `-l -C` becomes columns and
        // `--format=single-column` overrides long unconditionally.
        assert_eq!(
            parse(&["-l", "-1"]),
            Ok(list(
                Options {
                    format: Some(Format::Long),
                    ..Options::DEFAULT
                },
                &["."],
            ))
        );
        assert_eq!(
            parse(&["-l", "-C"]),
            Ok(list(
                Options {
                    format: Some(Format::Columns),
                    ..Options::DEFAULT
                },
                &["."],
            ))
        );
        assert_eq!(
            parse(&["-l", "--format=single-column"]),
            Ok(list(
                Options {
                    format: Some(Format::OnePerLine),
                    ..Options::DEFAULT
                },
                &["."],
            ))
        );
    }

    #[test]
    fn indicator_style_and_file_type_select_the_suffix() {
        for (word, indicator) in [
            ("none", Indicator::None),
            ("slash", Indicator::Slash),
            ("file-type", Indicator::FileType),
            ("classify", Indicator::Classify),
        ] {
            assert_eq!(
                parse(&[&format!("--indicator-style={word}")]),
                Ok(list(
                    Options {
                        indicator,
                        ..Options::DEFAULT
                    },
                    &["."],
                )),
                "{word}"
            );
        }
        assert_eq!(
            parse(&["--file-type"]),
            Ok(list(
                Options {
                    indicator: Indicator::FileType,
                    ..Options::DEFAULT
                },
                &["."],
            ))
        );
        // The later indicator flag wins.
        assert_eq!(
            parse(&["-F", "--file-type"]),
            Ok(list(
                Options {
                    indicator: Indicator::FileType,
                    ..Options::DEFAULT
                },
                &["."],
            ))
        );
        assert_eq!(parse(&["--indicator-style=bogus"]), Err(LsError::Usage));
    }

    #[test]
    fn no_group_drops_the_column_without_selecting_long() {
        for args in [["-G"], ["--no-group"]] {
            assert_eq!(
                parse(&args),
                Ok(list(
                    Options {
                        hide_group: true,
                        ..Options::DEFAULT
                    },
                    &["."],
                ))
            );
        }
    }

    #[test]
    fn author_sets_its_flag() {
        assert_eq!(
            parse(&["-l", "--author"]),
            Ok(list(
                Options {
                    format: Some(Format::Long),
                    author: true,
                    ..Options::DEFAULT
                },
                &["."],
            ))
        );
    }

    #[test]
    fn tabsize_parses_in_every_spelling() {
        for args in [
            &["-T", "4"][..],
            &["-T4"][..],
            &["--tabsize", "4"][..],
            &["--tabsize=4"][..],
        ] {
            assert_eq!(
                parse(args),
                Ok(list(
                    Options {
                        tabsize: 4,
                        ..Options::DEFAULT
                    },
                    &["."],
                )),
                "{args:?}"
            );
        }
        // `-T0` disables tabs; a bad value fails closed.
        assert_eq!(
            parse(&["-T0"]),
            Ok(list(
                Options {
                    tabsize: 0,
                    ..Options::DEFAULT
                },
                &["."],
            ))
        );
        assert_eq!(parse(&["-T", "wide"]), Err(LsError::Usage));
        assert_eq!(parse(&["--tabsize"]), Err(LsError::Usage));
    }

    #[test]
    fn zero_sets_its_flag() {
        assert_eq!(
            parse(&["--zero"]),
            Ok(list(
                Options {
                    zero: true,
                    ..Options::DEFAULT
                },
                &["."],
            ))
        );
    }

    #[test]
    fn color_when_parses_in_every_spelling_and_bare_means_always() {
        // The default is `auto`.
        assert_eq!(parse(&["."]).unwrap(), list(Options::DEFAULT, &["."]));
        let cases = [
            (&["--color"][..], ColorChoice::Always),
            (&["--colour"][..], ColorChoice::Always),
            (&["--color=always"][..], ColorChoice::Always),
            (&["--color=yes"][..], ColorChoice::Always),
            (&["--color=force"][..], ColorChoice::Always),
            (&["--color=never"][..], ColorChoice::Never),
            (&["--color=no"][..], ColorChoice::Never),
            (&["--color=none"][..], ColorChoice::Never),
            (&["--color=auto"][..], ColorChoice::Auto),
            (&["--color=tty"][..], ColorChoice::Auto),
            (&["--color=if-tty"][..], ColorChoice::Auto),
        ];
        for (args, expected) in cases {
            assert_eq!(
                parse(args),
                Ok(list(
                    Options {
                        color: expected,
                        ..Options::DEFAULT
                    },
                    &["."],
                )),
                "{args:?}"
            );
        }
    }

    #[test]
    fn a_bare_color_does_not_consume_the_next_argument() {
        // `--color` takes an optional value, so a following path is a path,
        // never swallowed as the WHEN word.
        assert_eq!(
            parse(&["--color", "dir"]),
            Ok(list(
                Options {
                    color: ColorChoice::Always,
                    ..Options::DEFAULT
                },
                &["dir"],
            ))
        );
    }

    #[test]
    fn a_bad_color_when_is_a_usage_error() {
        assert_eq!(parse(&["--color=maybe"]), Err(LsError::Usage));
    }

    /// Every locale's `OPTIONS` section documents exactly the switches this
    /// parser accepts (`plans/APPS.md` §3.1): the flag tokens are
    /// language-neutral, so each translated document must carry the same
    /// keys as the canonical one. The documents are read from the bundle's
    /// own on-disk `Help/` tree — the single source the image builder plants
    /// — never a copy embedded in this crate.
    #[test]
    fn help_documents_the_parser_switches() {
        extern crate std;
        use std::fs;

        let help_root = format!("{}/Help", env!("CARGO_MANIFEST_DIR"));
        let locales = tairix_help::REQUIRED_LOCALES;
        for locale in locales {
            let path = format!("{help_root}/{locale}/ls.md");
            let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
            for switch in [
                "`-a, --all`",
                "`-A, --almost-all`",
                "`-d, --directory`",
                "`-F, --classify`",
                "`--file-type`",
                "`--indicator-style=WORD`",
                "`-g`",
                "`-G, --no-group`",
                "`--author`",
                "`-h, --human-readable`",
                "`--si`",
                "`-k, --kibibytes`",
                "`--block-size=SIZE`",
                "`-l`",
                "`--format=WORD`",
                "`-m`",
                "`-n, --numeric-uid-gid`",
                "`-o`",
                "`-p`",
                "`-N, --literal`",
                "`-Q, --quote-name`",
                "`-b, --escape`",
                "`-q, --hide-control-chars`",
                "`--show-control-chars`",
                "`--quoting-style=WORD`",
                "`-r, --reverse`",
                "`-R, --recursive`",
                "`-L, --dereference`",
                "`-H, --dereference-command-line`",
                "`--dereference-command-line-symlink-to-dir`",
                "`-s, --size`",
                "`-S`",
                "`-t`",
                "`-U`",
                "`-X`",
                "`-v`",
                "`-f`",
                "`--sort=WORD`",
                "`--group-directories-first`",
                "`-c`",
                "`-u`",
                "`-i, --inode`",
                "`-B, --ignore-backups`",
                "`-I, --ignore=PATTERN`",
                "`--hide=PATTERN`",
                "`--time=WORD`",
                "`--time-style=STYLE`",
                "`--full-time`",
                "`-C`",
                "`-x`",
                "`-T, --tabsize <cols>`",
                "`-w, --width <cols>`",
                "`-1`",
                "`--zero`",
                "`--color[=WHEN]`",
                "`-?`",
            ] {
                assert!(
                    text.contains(switch),
                    "{locale}/ls.md must document {switch}"
                );
            }
        }
    }
}
