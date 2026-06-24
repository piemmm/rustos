//! The move engine: resolve each source's destination, rename it, and — when
//! the rename would cross a filesystem boundary — copy the source to the
//! destination and then remove the source.

use alloc::string::String;
use alloc::vec::Vec;

use crate::command::Command;
use crate::error::MvError;
use crate::io::{Entry, EntryKind, FileSystem, Output, RenameOutcome};

/// The fixed-size chunk used to stream a regular file during the cross-device
/// fallback. Matches `cat`'s and `cp`'s `READ_CHUNK` so the userland tools
/// share one streaming granularity.
const READ_CHUNK: usize = 4096;

/// The usage banner printed by [`Command::Help`].
pub const USAGE: &str = "\
usage: mv [-f] [-n] [--] source... dest

  -f, --force        remove a blocking destination and retry the rename
  -n, --no-clobber   never overwrite an existing destination
  -h, --help         show this message

With one source and a non-directory dest, the source is moved to dest. When
dest is an existing directory (always, with more than one source) each source
is moved into it under its base name. `--` ends option parsing: every later
argument is a path.
";

/// Run one [`Command`], moving its sources through `fs`.
///
/// Each source is renamed onto its destination; a rename that would cross a
/// filesystem boundary falls back to a copy followed by removal of the source.
/// When the destination is an existing directory — and always with more than
/// one source — each source is moved into it under its base name. `mv` writes
/// nothing on success; `out` carries only the [`Command::Help`] banner.
///
/// The first failure stops the run before any later operand (fail closed).
///
/// # Errors
///
/// * [`MvError::Usage`] — more than one source aimed at a non-directory
///   destination.
/// * [`MvError::Stat`] — an operand could not be inspected; carries the
///   underlying [`Errno`](rustos_abi::Errno).
/// * [`MvError::Rename`] — a rename failed for a non-boundary reason.
/// * [`MvError::Read`] / [`MvError::Create`] / [`MvError::Write`] — a
///   cross-device copy failed.
/// * [`MvError::Remove`] — the source could not be removed after a copy.
/// * [`MvError::Output`] — writing the usage banner failed.
pub fn run(command: Command, fs: &dyn FileSystem, out: &dyn Output) -> Result<(), MvError> {
    match command {
        Command::Help => out.write_all(USAGE.as_bytes()).map_err(MvError::Output),
        Command::Move {
            force,
            no_clobber,
            sources,
            dest,
        } => move_all(&sources, &dest, force, no_clobber, fs),
    }
}

/// Move every source to `dest`, deciding per source whether the destination is
/// `dest` itself or a child of `dest` named after the source.
fn move_all(
    sources: &[String],
    dest: &str,
    force: bool,
    no_clobber: bool,
    fs: &dyn FileSystem,
) -> Result<(), MvError> {
    let dest_is_dir = matches!(stat(dest, fs)?, Some(EntryKind::Directory));
    // More than one source can only land inside a directory.
    if sources.len() > 1 && !dest_is_dir {
        return Err(MvError::Usage);
    }
    for source in sources {
        let target = if dest_is_dir {
            join(dest, basename(source))
        } else {
            String::from(dest)
        };
        move_operand(source, &target, force, no_clobber, fs)?;
    }
    Ok(())
}

/// Move one named source operand to `target`.
fn move_operand(
    source: &str,
    target: &str,
    force: bool,
    no_clobber: bool,
    fs: &dyn FileSystem,
) -> Result<(), MvError> {
    let kind = fs.kind(source).map_err(MvError::Stat)?;
    // `-n`: an existing destination is left untouched and the source skipped.
    if no_clobber && stat(target, fs)?.is_some() {
        return Ok(());
    }
    match fs.rename(source, target) {
        Ok(RenameOutcome::Renamed) => Ok(()),
        Ok(RenameOutcome::CrossDevice) => relocate(source, kind, target, fs),
        Err(_) if force => force_retry(source, kind, target, fs),
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
            return Err(MvError::Read(rustos_abi::Errno::LengthOutOfRange));
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
        Err(rustos_abi::Errno::NotFound) => Ok(None),
        Err(errno) => Err(MvError::Stat(errno)),
    }
}

/// The final path component of `path`, ignoring any trailing slashes. The base
/// name of `/a/b/` is `b`; of `/` (or the empty string) it is `/`.
fn basename(path: &str) -> &str {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return "/";
    }
    match trimmed.rfind('/') {
        Some(slash) => &trimmed[slash + 1..],
        None => trimmed,
    }
}

/// Join a directory `parent` and a child `name` into a path, inserting a
/// single `/` unless `parent` already ends with one.
fn join(parent: &str, name: &str) -> String {
    let mut path = String::with_capacity(parent.len() + 1 + name.len());
    path.push_str(parent);
    if !parent.ends_with('/') {
        path.push('/');
    }
    path.push_str(name);
    path
}

#[cfg(test)]
mod tests;
