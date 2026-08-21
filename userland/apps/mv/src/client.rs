//! The move engine: resolve each source's destination, rename it, and — when
//! the rename would cross a filesystem boundary — copy the source to the
//! destination and then remove the source.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use tairix_help::{own_short_help, HelpSource};
use tairix_path::{join, leaf_name};

use crate::command::{Clobber, Command, Options, TargetMode};
use crate::error::MvError;
use crate::io::{Entry, EntryKind, FileSystem, Output, Prompt, RenameOutcome};

/// The fixed-size chunk used to stream a regular file during the cross-device
/// fallback. Matches `cat`'s and `cp`'s `READ_CHUNK` so the userland tools
/// share one streaming granularity.
const READ_CHUNK: usize = 4096;

/// The usage banner a usage error is reported with, and the fallback the
/// short-help switches print when `mv`'s own Help tree is unavailable.
pub const USAGE: &str = "\
usage: mv [-finvT] [-t dir] [--] source... dest

  -f, --force                remove a blocking destination and retry the
                             rename; never prompt
  -i, --interactive          ask before overwriting an existing destination
  -n, --no-clobber           never overwrite an existing destination
  -v, --verbose              report each move
  -t dir, --target-directory=dir
                             move every source into dir
  -T, --no-target-directory  treat dest as a normal file (one source)
  -h, -?, --help             show this message

With one source and a non-directory dest, the source is moved to dest. When
dest is an existing directory (always, with more than one source) each source
is moved into it under its base name. `--` ends option parsing: every later
argument is a path.
";

/// `mv`'s own command word: the short-help switches render its own Help
/// document through the same engine as any other command's.
const OWN_WORD: &str = "mv";

/// Run one [`Command`], moving its sources through `fs`. `locale` is the
/// user's `LANG` preference, if set; `help` is the tool's own `Help/` tree,
/// read by the short-help switches.
///
/// Each source is renamed onto its destination; a rename that would cross a
/// filesystem boundary falls back to a copy followed by removal of the source.
/// When the destination is an existing directory — and always with more than
/// one source or `-t` — each source is moved into it under its base name;
/// `-T` uses the destination exactly as given. An existing destination is
/// overwritten by default, skipped under `-n`, and asked about through
/// `prompt` under `-i` (a declined question skips that move without error).
/// `-v` reports each move on `out`; otherwise `mv` writes nothing on success
/// beyond the [`Command::Help`] banner.
///
/// The first failure stops the run before any later operand (fail closed).
///
/// # Errors
///
/// * [`MvError::Usage`] — more than one source aimed at a non-directory
///   destination (or at any destination under `-T`), or a `-t` operand
///   that is not an existing directory.
/// * [`MvError::Stat`] — an operand could not be inspected; carries the
///   underlying [`Errno`](tairix_abi::Errno).
/// * [`MvError::Rename`] — a rename failed for a non-boundary reason.
/// * [`MvError::Read`] / [`MvError::Create`] / [`MvError::Write`] — a
///   cross-device copy failed.
/// * [`MvError::Remove`] — the source could not be removed after a copy.
/// * [`MvError::Prompt`] — a confirmation could not be read (never treated
///   as consent).
/// * [`MvError::Output`] — writing the usage banner or a `-v` report
///   failed.
pub fn run(
    command: Command,
    locale: Option<&str>,
    fs: &dyn FileSystem,
    prompt: &dyn Prompt,
    help: &dyn HelpSource,
    out: &dyn Output,
) -> Result<(), MvError> {
    match command {
        Command::Help => short_help(locale, help, out),
        Command::Move {
            options,
            sources,
            dest,
        } => move_all(&sources, &dest, options, fs, prompt, out),
    }
}

/// Render `mv`'s own short help (`NAME` + `SYNOPSIS` + compact `OPTIONS`)
/// from its own Help tree through the one shared engine; when no document
/// can be served (a build without the bundle's documents) the usage banner
/// stands in — the tool's own text, not fabricated help content — so `-h`
/// never fails.
fn short_help(
    locale: Option<&str>,
    help: &dyn HelpSource,
    out: &dyn Output,
) -> Result<(), MvError> {
    let bytes =
        own_short_help(help, locale, OWN_WORD).unwrap_or_else(|| String::from(USAGE).into_bytes());
    out.write_all(&bytes).map_err(MvError::Output)
}

/// Move every source to `dest`, deciding per source whether the destination is
/// `dest` itself or a child of `dest` named after the source.
fn move_all(
    sources: &[String],
    dest: &str,
    options: Options,
    fs: &dyn FileSystem,
    prompt: &dyn Prompt,
    out: &dyn Output,
) -> Result<(), MvError> {
    let dest_is_dir = match options.target_mode {
        // `-t`: the destination must be an existing directory.
        TargetMode::Directory => match stat(dest, fs)? {
            Some(EntryKind::Directory) => true,
            Some(EntryKind::File) => return Err(MvError::Usage),
            None => return Err(MvError::Stat(tairix_abi::Errno::NotFound)),
        },
        // `-T`: the destination is a normal file for exactly one source.
        TargetMode::NoDirectory => {
            if sources.len() > 1 {
                return Err(MvError::Usage);
            }
            false
        }
        TargetMode::Inferred => matches!(stat(dest, fs)?, Some(EntryKind::Directory)),
    };
    // More than one source can only land inside a directory.
    if sources.len() > 1 && !dest_is_dir {
        return Err(MvError::Usage);
    }
    for source in sources {
        let target = if dest_is_dir {
            join(dest, leaf_name(source))
        } else {
            String::from(dest)
        };
        move_operand(source, &target, options, fs, prompt)?;
        if options.verbose {
            out.write_all(format!("renamed '{source}' -> '{target}'\n").as_bytes())
                .map_err(MvError::Output)?;
        }
    }
    Ok(())
}

/// Move one named source operand to `target`.
fn move_operand(
    source: &str,
    target: &str,
    options: Options,
    fs: &dyn FileSystem,
    prompt: &dyn Prompt,
) -> Result<(), MvError> {
    let kind = fs.kind(source).map_err(MvError::Stat)?;
    if stat(target, fs)?.is_some() {
        match options.clobber {
            Clobber::Overwrite => {}
            // `-n`: an existing destination is left untouched and the
            // source skipped.
            Clobber::Skip => return Ok(()),
            Clobber::Prompt => {
                let question = format!("overwrite '{target}'?");
                if !prompt.confirm(&question).map_err(MvError::Prompt)? {
                    return Ok(());
                }
            }
        }
    }
    match fs.rename(source, target) {
        Ok(RenameOutcome::Renamed) => Ok(()),
        Ok(RenameOutcome::CrossDevice) => relocate(source, kind, target, fs),
        Err(_) if options.force => force_retry(source, kind, target, fs),
        Err(errno) => Err(MvError::Rename(errno)),
    }
}

/// `-f` recovery: remove a destination that blocks the rename, then retry the
/// rename exactly once, still honouring a cross-device result.
fn force_retry(
    source: &str,
    kind: EntryKind,
    target: &str,
    fs: &dyn FileSystem,
) -> Result<(), MvError> {
    // The destination blocked the rename (e.g. it exists and is not writable).
    // Remove it and retry once; a removal error is irrelevant if the retried
    // rename then succeeds.
    let _ = fs.remove_file(target);
    match fs.rename(source, target) {
        Ok(RenameOutcome::Renamed) => Ok(()),
        Ok(RenameOutcome::CrossDevice) => relocate(source, kind, target, fs),
        Err(errno) => Err(MvError::Rename(errno)),
    }
}

/// Relocate `source` to `target` across a filesystem boundary: copy the whole
/// object (recursively, for a directory) and then remove the source.
fn relocate(
    source: &str,
    kind: EntryKind,
    target: &str,
    fs: &dyn FileSystem,
) -> Result<(), MvError> {
    copy_tree(source, kind, target, fs)?;
    remove_tree(source, kind, fs)
}

/// Copy an object whose [`EntryKind`] is known to `target`, recursing into
/// directories.
fn copy_tree(
    source: &str,
    kind: EntryKind,
    target: &str,
    fs: &dyn FileSystem,
) -> Result<(), MvError> {
    match kind {
        EntryKind::File => copy_file(source, target, fs),
        EntryKind::Directory => {
            match stat(target, fs)? {
                Some(EntryKind::Directory) => {}
                _ => fs.mkdir(target).map_err(MvError::Create)?,
            }
            for entry in read_children(source, fs)? {
                let child_source = join(source, &entry.name);
                let child_target = join(target, &entry.name);
                copy_tree(&child_source, entry.kind, &child_target, fs)?;
            }
            Ok(())
        }
    }
}

/// Stream a regular file from `source` to `target`.
fn copy_file(source: &str, target: &str, fs: &dyn FileSystem) -> Result<(), MvError> {
    fs.create(target).map_err(MvError::Create)?;
    let mut offset: u64 = 0;
    let mut buf = [0_u8; READ_CHUNK];
    loop {
        let read = fs.read(source, offset, &mut buf).map_err(MvError::Read)?;
        if read == 0 {
            return Ok(());
        }
        // A seam reporting more than the buffer holds would index out of
        // bounds; refuse it rather than trust the count.
        if read > buf.len() {
            return Err(MvError::Read(tairix_abi::Errno::LengthOutOfRange));
        }
        fs.write(target, offset, &buf[..read])
            .map_err(MvError::Write)?;
        offset = offset.saturating_add(read as u64);
    }
}

/// Remove an already-copied object whose [`EntryKind`] is known, depth-first
/// for a directory so a parent is removed only after its contents.
fn remove_tree(source: &str, kind: EntryKind, fs: &dyn FileSystem) -> Result<(), MvError> {
    match kind {
        EntryKind::File => fs.remove_file(source).map_err(MvError::Remove),
        EntryKind::Directory => {
            for entry in read_children(source, fs)? {
                let child = join(source, &entry.name);
                remove_tree(&child, entry.kind, fs)?;
            }
            fs.remove_dir(source).map_err(MvError::Remove)
        }
    }
}

/// Read every entry of `path` into a vector, so the directory can be walked
/// without depending on entry indices staying stable as the move proceeds.
fn read_children(path: &str, fs: &dyn FileSystem) -> Result<Vec<Entry>, MvError> {
    let mut entries = Vec::new();
    let mut index: u64 = 0;
    while let Some(entry) = fs.read_dir(path, index).map_err(MvError::Read)? {
        index = index.saturating_add(1);
        entries.push(entry);
    }
    Ok(entries)
}

/// Inspect `path`, mapping a missing path to [`None`] so a destination can be
/// probed for existence without treating absence as a failure.
fn stat(path: &str, fs: &dyn FileSystem) -> Result<Option<EntryKind>, MvError> {
    match fs.kind(path) {
        Ok(kind) => Ok(Some(kind)),
        Err(tairix_abi::Errno::NotFound) => Ok(None),
        Err(errno) => Err(MvError::Stat(errno)),
    }
}
#[cfg(test)]
mod tests;
