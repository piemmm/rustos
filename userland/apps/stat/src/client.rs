//! The report engine: gather each operand's facts once, then render them.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use tairix_abi::fs::mode_string;
use tairix_abi::time::CivilTime;
use tairix_abi::time::Time64;
use tairix_abi::{Errno, FileKind, FileStat, FS_MODE_MASK, FS_NAME_MAX};
use tairix_help::{own_short_help, HelpSource};

use crate::command::{Command, Options, Piece, Subject, Trailer};
use crate::error::StatError;
use crate::io::{Filesystem, Mount, Mounts, Names, Output};

/// The usage banner a usage error is reported with, and the fallback the
/// short-help switches print when `stat`'s own Help tree is unavailable.
pub const USAGE: &str = "\
usage: stat [-Lft] [-c FORMAT | --printf=FORMAT] [--] file...

  -L, --dereference     describe what a symbolic link names
  -f, --file-system     describe the filesystem, not the file
  -c, --format=FORMAT   render FORMAT per operand, then a newline
      --printf=FORMAT   as -c, but interpret \\ escapes and add no newline
  -t, --terse           the one-line summary form
  -?, --help            show this message

At least one operand is required. `--` ends option parsing. Without -L a
symbolic link is described as itself, which is the whole point of the
tool: `stat -L` describes what it names instead.
";

/// `stat`'s own command word: the short-help switches render its own Help
/// document through the same engine as any other command's.
const OWN_WORD: &str = "stat";

/// The `-t` file form: GNU's fields in GNU's order, less the two
/// device-type columns (`%t`/`%T`) — TAIRiX has no device special files, so
/// printing them would mean fabricating a value rather than reporting one.
const TERSE_FILE: &str = "%n %s %b %f %u %g %D %i %h %X %Y %Z %W %o";
/// GNU's `-t -f` form, with the numeric type magic TAIRiX has none of
/// replaced by the type name the mount records.
const TERSE_FS: &str = "%n %i %l %T %b %f %a %c %d %s %S";

/// The full report `stat` writes with no format given.
const FULL_FILE: &str = "\
  File: %N\n\
  Size: %-10s\tBlocks: %-8b IO Block: %-6o %F\n\
Volume: %D\tInode: %-10i  Links: %h\n\
Access: (%a/%A)  Uid: (%u/%U)  Gid: (%g/-)\n\
Access: %x\nModify: %y\nChange: %z\n Birth: %w\n";

/// The full report `stat -f` writes with no format given.
const FULL_FS: &str = "\
  File: %n\n\
    ID: %-16i Namelen: %-7l Type: %T\n\
Block size: %-10s\n\
Blocks: Total: %-10b Free: %-10f Available: %a\n\
Inodes: Total: %-10c Free: %d\n";

/// The seams one `stat` run reports through.
///
/// The five arrive together and are used together on every operand, so they
/// travel as one value rather than as five parameters threaded through each
/// helper — the shape `du`'s `Reporter` and `cp`'s `Copier` take.
pub struct Reporter<'a> {
    /// The node facts each operand is described from.
    pub fs: &'a dyn Filesystem,
    /// The mount snapshot behind `%m`, `%o`, and every `-f` field.
    pub mounts: &'a dyn Mounts,
    /// The uid → account-name lookup behind `%U`.
    pub names: &'a dyn Names,
    /// The report (fd 1).
    pub out: &'a dyn Output,
    /// The per-operand diagnostics (fd 2).
    pub err: &'a dyn Output,
}

/// Run one [`Command`], writing its report through the reporter's streams.
///
/// Returns `Ok(true)` when every operand was described, `Ok(false)` when at
/// least one was refused — the GNU behaviour: report the reason on standard
/// error, continue to the remaining operands, exit non-zero.
///
/// # Errors
///
/// [`StatError::Output`] when a rendering or a diagnostic cannot be written
/// — the only fatal condition.
pub fn run(
    command: Command,
    locale: Option<&str>,
    reporter: &Reporter<'_>,
    help: &dyn HelpSource,
) -> Result<bool, StatError> {
    let (options, paths) = match command {
        Command::Help => {
            let bytes = own_short_help(help, locale, OWN_WORD)
                .unwrap_or_else(|| String::from(USAGE).into_bytes());
            reporter.out.write_all(&bytes).map_err(StatError::Output)?;
            return Ok(true);
        }
        Command::Describe { options, paths } => (options, paths),
    };
    let (pieces, trailer) = form(&options)?;

    let mut clean = true;
    for path in &paths {
        match reporter.describe(path, &options) {
            Ok(facts) => {
                let mut rendered = render(&pieces, &facts);
                if trailer == Trailer::Newline {
                    rendered.push('\n');
                }
                reporter
                    .out
                    .write_all(rendered.as_bytes())
                    .map_err(StatError::Output)?;
            }
            Err(errno) => {
                clean = false;
                reporter
                    .err
                    .write_all(format!("stat: cannot stat '{path}': {errno}\n").as_bytes())
                    .map_err(StatError::Output)?;
            }
        }
    }
    Ok(clean)
}

/// The pieces and trailer one run renders with.
///
/// The default and `-t` forms are themselves formats, so there is one
/// renderer rather than a hand-written report per form.
///
/// # Errors
///
/// As [`crate::command::parse_format`]; a built-in form's specifiers are
/// this crate's own text, so a failure there would be a defect in the
/// constants above rather than in the caller's command line.
fn form(options: &Options) -> Result<(Vec<Piece>, Trailer), StatError> {
    if let Some((pieces, trailer)) = &options.format {
        return Ok((pieces.clone(), *trailer));
    }
    let text = match (options.subject, options.terse) {
        (Subject::File, false) => FULL_FILE,
        (Subject::File, true) => TERSE_FILE,
        (Subject::Filesystem, false) => FULL_FS,
        (Subject::Filesystem, true) => TERSE_FS,
    };
    let trailer = if options.terse {
        Trailer::Newline
    } else {
        Trailer::None
    };
    Ok((
        crate::command::parse_format(text, trailer, options.subject)?,
        trailer,
    ))
}

/// Everything one operand's report can name, gathered before any of it is
/// rendered.
///
/// Gathering first is what keeps a multi-specifier format from asking the
/// kernel the same question twice: `%N` and `%F` both need the kind, `%m`
/// and `%o` both need the covering mount.
struct Facts {
    /// The operand as the caller spelled it — what `%n` prints.
    name: String,
    /// The node's own report. Gathered under both readings: `-f` describes
    /// the volume, and the volume's identity is the one every node on it
    /// carries, so the two vocabularies name the same volume in the same
    /// spelling.
    stat: FileStat,
    /// Which vocabulary renders.
    subject: Subject,
    /// The stored target, for a symbolic link described as itself.
    target: Option<String>,
    /// The account name owning the node, if the directory holds one.
    user: Option<String>,
    /// The covering mount, if the mount table names one.
    mount: Option<Mount>,
}

impl Reporter<'_> {
    /// Gather one operand's facts.
    fn describe(&self, path: &str, options: &Options) -> Result<Facts, Errno> {
        // Every reading confirms the operand exists first, so `-f` on an
        // absent path is the same refusal `stat` gives, not a volume report
        // about a path that is not there.
        let stat = self.fs.stat(path, options.dereference)?;
        let target = if stat.kind == FileKind::Symlink {
            self.fs.read_link(path).ok()
        } else {
            None
        };
        // The mount holding a path is the longest mount prefix of its
        // *canonical* spelling: a link into another volume must report the
        // volume it lands on. A mount table the caller cannot read leaves
        // the mount-derived fields unknown rather than failing the report.
        let mount = self
            .fs
            .canonicalize(path)
            .ok()
            .zip(self.mounts.list().ok())
            .and_then(|(canonical, table)| covering_mount(&canonical, table));
        Ok(Facts {
            name: String::from(path),
            user: self.names.user(stat.uid),
            stat,
            subject: options.subject,
            target,
            mount,
        })
    }
}

/// The mount whose point is the longest prefix of `canonical`.
///
/// Longest-prefix, not first-match, because mounts nest: `/System/Logs`
/// covers a path under it even though `/System` also prefixes it.
fn covering_mount(canonical: &str, table: Vec<Mount>) -> Option<Mount> {
    table
        .into_iter()
        .filter(|mount| covers(&mount.target, canonical))
        .max_by_key(|mount| mount.target.len())
}

/// Whether the mount point `target` covers the canonical path `path`.
///
/// A mount point covers a path when the path is the point itself or lies
/// under it — matched by whole components, so `/Storage/vol` does not cover
/// `/Storage/volume`.
fn covers(target: &str, path: &str) -> bool {
    if target == "/" {
        return path.starts_with('/');
    }
    path == target || (path.starts_with(target) && path.as_bytes().get(target.len()) == Some(&b'/'))
}

/// Render `pieces` for one operand's `facts`.
fn render(pieces: &[Piece], facts: &Facts) -> String {
    let mut out = String::new();
    for piece in pieces {
        match piece {
            Piece::Text(text) => out.push_str(text),
            Piece::Field(letter, pad) => out.push_str(&pad.apply(&field(*letter, facts))),
        }
    }
    out
}

/// The value of one specifier for `facts`.
///
/// A field whose fact is unavailable renders as `?`, never as a guess: a
/// report that cannot be made is said so in the field rather than by
/// substituting a plausible number.
fn field(letter: char, facts: &Facts) -> String {
    match facts.subject {
        Subject::File => file_field(letter, &facts.stat, facts, facts.mount.as_ref()),
        // A volume the mount table does not name has no usage to report, so
        // every field but the operand's own name says so.
        Subject::Filesystem => match &facts.mount {
            Some(mount) => fs_field(letter, mount, facts),
            None if letter == 'n' => facts.name.clone(),
            None if letter == 'i' => volume_hex(facts.stat.id.volume),
            None => String::from("?"),
        },
    }
}

/// One field of the file vocabulary.
fn file_field(letter: char, stat: &FileStat, facts: &Facts, mount: Option<&Mount>) -> String {
    match letter {
        'a' => format!("{:o}", stat.mode & FS_MODE_MASK),
        'A' => {
            let bytes = mode_string(stat.kind, stat.mode);
            // The renderer's ten bytes are ASCII by construction, so the
            // conversion cannot fail; fall back to the raw octal rather than
            // to a fabricated string if it ever did.
            core::str::from_utf8(&bytes)
                .map_or_else(|_| format!("{:o}", stat.mode & FS_MODE_MASK), String::from)
        }
        // GNU's `%b` counts 512-byte units and `%B` states the unit, so the
        // pair is exact even though the format tracks bytes.
        'b' => (stat.allocated / BLOCK_UNIT).to_string(),
        'B' => BLOCK_UNIT.to_string(),
        // A TAIRiX volume is identified by a 16-byte id rather than a
        // device number, so the pair renders that id: `%d` as the decimal
        // of the 128-bit value, `%D` as its hex. A script comparing two
        // files' `%d` still learns exactly what it asked.
        'd' => volume_decimal(stat.id.volume),
        'D' => volume_hex(stat.id.volume),
        'f' => format!("{:x}", stat.mode),
        'F' => String::from(kind_name(stat.kind)),
        'g' => stat.gid.to_string(),
        'h' => stat.nlink.to_string(),
        'i' => stat.id.node.to_string(),
        'm' => mount.map_or_else(|| String::from("?"), |m| m.target.clone()),
        'n' => facts.name.clone(),
        'N' => match &facts.target {
            Some(target) => format!("'{}' -> '{target}'", facts.name),
            None => format!("'{}'", facts.name),
        },
        // `st_blksize`'s role — the transfer size the backing prefers — is
        // the mounted format's own block size.
        'o' => mount.map_or_else(|| String::from("?"), |m| m.usage.block_size.to_string()),
        's' => stat.size.to_string(),
        'u' => stat.uid.to_string(),
        'U' => facts
            .user
            .clone()
            .unwrap_or_else(|| String::from("UNKNOWN")),
        'w' => human_stamp(stat.times.created),
        'W' => epoch_stamp(stat.times.created),
        'x' => human_stamp(stat.times.accessed),
        'X' => epoch_stamp(stat.times.accessed),
        'y' => human_stamp(stat.times.modified),
        'Y' => epoch_stamp(stat.times.modified),
        'z' => human_stamp(stat.times.changed),
        'Z' => epoch_stamp(stat.times.changed),
        // The parser admits only the letters above, so nothing else reaches
        // here; answer `?` rather than invent a value.
        _ => String::from("?"),
    }
}

/// One field of the filesystem vocabulary.
fn fs_field(letter: char, mount: &Mount, facts: &Facts) -> String {
    let usage = &mount.usage;
    match letter {
        'a' => usage.avail_blocks.to_string(),
        'b' => usage.total_blocks.to_string(),
        'c' => usage.files.to_string(),
        'd' => usage.files_free.to_string(),
        'f' => usage.free_blocks.to_string(),
        'i' => volume_hex(facts.stat.id.volume),
        'l' => FS_NAME_MAX.to_string(),
        'n' => facts.name.clone(),
        // A TAIRiX volume declares one block size, which is both the
        // transfer size and the fundamental one, so the pair agrees rather
        // than one of them being invented.
        's' | 'S' => usage.block_size.to_string(),
        'T' => {
            if mount.fstype.is_empty() {
                String::from("?")
            } else {
                mount.fstype.clone()
            }
        }
        _ => String::from("?"),
    }
}

/// GNU's `%b` unit: 512-byte blocks, which `%B` states.
const BLOCK_UNIT: u64 = 512;

/// The human name of a node kind, as GNU's `%F` spells it.
const fn kind_name(kind: FileKind) -> &'static str {
    match kind {
        FileKind::Regular => "regular file",
        FileKind::Directory => "directory",
        FileKind::Symlink => "symbolic link",
    }
}

/// A 16-byte volume id as the decimal of the 128-bit value it is.
fn volume_decimal(volume: [u8; 16]) -> String {
    u128::from_be_bytes(volume).to_string()
}

/// A 16-byte volume id as 32 lowercase hex digits.
fn volume_hex(volume: [u8; 16]) -> String {
    use core::fmt::Write as _;
    let mut out = String::with_capacity(32);
    for byte in volume {
        // The buffer's `write_str` is infallible, so a formatting error
        // cannot arise; drop it rather than pretend to handle one.
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// A stamp as GNU renders `%x`/`%y`/`%z`/`%w`.
///
/// A stamp the backing format does not keep is the epoch, which GNU spells
/// `-` for a birth time it does not have; the same rule reads truthfully for
/// every stamp a format omits.
fn human_stamp(stamp: Time64) -> String {
    if stamp == Time64::UNIX_EPOCH {
        return String::from("-");
    }
    let civil = CivilTime::from_unix_secs(stamp.secs());
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:09} +0000",
        civil.year,
        civil.month,
        civil.day,
        civil.hour,
        civil.minute,
        civil.second,
        stamp.subsec_nanos(),
    )
}

/// A stamp as GNU renders `%X`/`%Y`/`%Z`/`%W`: seconds since the epoch, and
/// `0` for a stamp the format does not keep.
fn epoch_stamp(stamp: Time64) -> String {
    stamp.secs().to_string()
}

#[cfg(test)]
mod tests;
