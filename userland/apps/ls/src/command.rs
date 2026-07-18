//! The parsed shape of an `ls` command line.

use alloc::string::String;
use alloc::vec::Vec;

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

/// The non-long output arrangement.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Format {
    /// One name per line (`-1`).
    OnePerLine,
    /// Multiple columns filled top-to-bottom, sized to the terminal width
    /// (`-C`; the GNU default when writing to a terminal).
    Columns,
    /// Multiple columns filled left-to-right, sized to the terminal width
    /// (`-x`).
    Across,
    /// Names separated by `, `, wrapped to the terminal width (`-m`).
    Commas,
}

/// The indicator suffix appended to a rendered name.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Indicator {
    /// No suffix (the default).
    None,
    /// `/` after directories (`-p`).
    Slash,
    /// `/` after directories and `*` after executables (`-F`).
    Classify,
}

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
    /// Long format (`-l`; also implied by `-n`, `-g`, and `-o`).
    pub long: bool,
    /// List directory operands themselves, not their contents (`-d`).
    pub directory: bool,
    /// Recurse into subdirectories (`-R`).
    pub recursive: bool,
    /// Reverse the sort order (`-r`).
    pub reverse: bool,
    /// Sort key (`-S`).
    pub sort: Sort,
    /// Non-long arrangement (`-1` / `-C` / `-x` / `-m`). `None` selects the
    /// GNU default: multiple columns (`-C`) when the output is a terminal,
    /// one name per line otherwise (decided against the attested console at
    /// render time, not here).
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
    /// Print each entry's allocated size in 1024-byte blocks (`-s`), plus
    /// a `total` line per directory block.
    pub size: bool,
    /// Human-readable sizes in the long format (`-h`).
    pub human_readable: bool,
    /// Omit the owner column from the long format (`-g`).
    pub hide_owner: bool,
    /// Omit the group column from the long format (`-o`).
    pub hide_group: bool,
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
}

impl Options {
    /// The defaults of a bare `ls`.
    pub const DEFAULT: Self = Self {
        hidden: Hidden::Skip,
        long: false,
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
        human_readable: false,
        hide_owner: false,
        hide_group: false,
        time_field: TimeField::Modified,
        time_style: TimeStyle::Locale,
        inode: false,
        group_directories_first: false,
    };
}

/// One thing the `ls` tool can do.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Command {
    /// List `paths`, in operand order.
    List {
        /// The listing options.
        options: Options,
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
/// `ls [-aACcdFfghilmnopQrRsStUuvXx1] [-w cols] [--time=WORD]`
/// `[--time-style=STYLE] [--sort=WORD] [--full-time]`
/// `[--group-directories-first] [--] [path...]`:
///
/// * `-a` / `--all` — do not hide entries whose name begins with `.`.
/// * `-A` / `--almost-all` — like `-a`, but never list `.` or `..`.
/// * `-C` — list entries in columns, filled top-to-bottom.
/// * `-d` / `--directory` — list directory operands themselves.
/// * `-F` / `--classify` — append `/` to directories and `*` to
///   executables.
/// * `-g` — long format without the owner column; implies `-l`.
/// * `-h` / `--human-readable` — human-readable sizes in the long format
///   (as in the GNU tool; short help is `-?` / `--help`).
/// * `-l` — long format.
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
/// * `-s` / `--size` — print each entry's allocated size in 1024-byte
///   blocks (scaled by `-h`), with a `total` line per directory block.
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
/// * `--time-style=STYLE` — how the long-format timestamp is rendered:
///   `locale` (the default), `long-iso`, `full-iso`, or `iso`. A custom
///   `+FORMAT` style is refused (fail closed; a documented divergence).
/// * `--full-time` — like `-l --time-style=full-iso`.
/// * `-w` / `--width <cols>` — set the output width; `0` means unlimited.
/// * `-x` — list entries in columns, filled left-to-right.
/// * `-1` — one name per line.
/// * `-?` / `--help` — the reserved short-help switches (plans/APPS.md §4;
///   they win immediately).
/// * `--` — end option parsing; every later argument is a path.
/// * any other `-…` — an [`LsError::Usage`] error (fail closed; never a
///   silently ignored token).
/// * anything else (including the bare `-`) — a path.
///
/// Short options may be combined into one argument (e.g. `-la` is
/// `-l -a`); an unrecognised letter anywhere in such a cluster is a usage
/// error. `-w` takes the rest of its cluster as its value (`-w80`), or the
/// next argument (`-w 80`) when it ends the cluster. The last of
/// `-1` / `-C` / `-x` / `-m` wins; any of `-l` / `-n` / `-g` / `-o`
/// selects the long format regardless of order; the later of `-p` / `-F`
/// wins.
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
    if paths.is_empty() {
        paths.push(String::from("."));
    }
    Ok(Command::List {
        options: state.options,
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
    explicit_sort: Option<Sort>,
    time_selected: bool,
}

impl Default for ParseState {
    fn default() -> Self {
        Self {
            options: Options::DEFAULT,
            explicit_sort: None,
            time_selected: false,
        }
    }
}

impl ParseState {
    /// Apply the GNU sort rule once the whole line has been parsed.
    fn resolve_sort(&mut self) {
        self.options.sort = self.explicit_sort.unwrap_or({
            if self.time_selected && !self.options.long {
                Sort::Time
            } else {
                Sort::Name
            }
        });
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
        "human-readable" => options.human_readable = true,
        "numeric-uid-gid" => options.long = true,
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
        "full-time" => {
            options.long = true;
            options.time_style = TimeStyle::FullIso;
        }
        "help" => return Ok(true),
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
                options.long = true;
                options.hide_owner = true;
            }
            'h' => options.human_readable = true,
            // `-n` selects the long format like `-l`; its numeric
            // owner/group is already the only output.
            'l' | 'n' => options.long = true,
            'm' => options.format = Some(Format::Commas),
            'o' => {
                options.long = true;
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
                options.long = false;
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
            'x' => options.format = Some(Format::Across),
            '1' => options.format = Some(Format::OnePerLine),
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

#[cfg(test)]
mod tests {
    use super::{
        parse, Command, Format, Hidden, Indicator, Options, QuotingStyle, Sort, TimeField,
        TimeStyle,
    };
    use crate::error::LsError;
    use alloc::string::String;
    use alloc::vec::Vec;

    fn list(options: Options, paths: &[&str]) -> Command {
        Command::List {
            options,
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
                    long: true,
                    human_readable: true,
                    ..Options::DEFAULT
                },
                &["."],
            ))
        );
        assert_eq!(
            parse(&["--human-readable"]),
            Ok(list(
                Options {
                    human_readable: true,
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
                    long: true,
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
                    long: true,
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
                    long: true,
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
                    long: true,
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
                        long: true,
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
                    long: true,
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
                    long: true,
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
                    long: true,
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
                    long: true,
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
                    long: true,
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
                    long: true,
                    indicator: Indicator::Classify,
                    ..Options::DEFAULT
                },
                &["dir"],
            ))
        );
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

    /// Every locale's `OPTIONS` section documents exactly the switches this
    /// parser accepts (`plans/APPS.md` §3.1): the flag tokens are
    /// language-neutral, so each translated document must carry the same
    /// keys as the canonical one. The documents are read from the bundle's
    /// own on-disk `Help/` tree — the single source the image builder plants
    /// — never a copy embedded in this crate.
    #[test]
    fn help_documents_the_parser_switches() {
        extern crate std;
        use alloc::format;
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
                "`-g`",
                "`-h, --human-readable`",
                "`-l`",
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
                "`--time=WORD`",
                "`--time-style=STYLE`",
                "`--full-time`",
                "`-C`",
                "`-x`",
                "`-w, --width <cols>`",
                "`-1`",
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
