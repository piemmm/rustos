//! Tests for the move engine, driven against an in-memory filesystem and a
//! recording output.

use super::{run as engine_run, USAGE};
use crate::command::{Clobber, Command, Options, TargetMode};
use crate::error::MvError;
use crate::io::{Entry, EntryKind, FileSystem, Output, Prompt, RenameOutcome};
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::cell::RefCell;
use tairix_abi::Errno;
use tairix_help::{HelpSource, SourceError};

/// A prompt no non-interactive run may ever reach.
struct NeverAsked;

impl Prompt for NeverAsked {
    fn confirm(&self, question: &str) -> Result<bool, Errno> {
        panic!("unexpected prompt: {question}");
    }
}

/// A scripted prompt: answers in order, recording every question; an
/// exhausted script fails the read.
struct Answers {
    replies: RefCell<Vec<bool>>,
    asked: RefCell<Vec<String>>,
}

impl Answers {
    fn new(replies: &[bool]) -> Self {
        Self {
            replies: RefCell::new(replies.to_vec()),
            asked: RefCell::new(Vec::new()),
        }
    }

    fn asked(&self) -> Vec<String> {
        self.asked.borrow().clone()
    }
}

impl Prompt for Answers {
    fn confirm(&self, question: &str) -> Result<bool, Errno> {
        self.asked.borrow_mut().push(question.to_string());
        let mut replies = self.replies.borrow_mut();
        if replies.is_empty() {
            return Err(Errno::NotFound);
        }
        Ok(replies.remove(0))
    }
}

/// A Help tree with no documents at all: the short-help fallback path.
struct NoHelp;

impl HelpSource for NoHelp {
    fn locale_dirs(&self) -> Result<Vec<String>, SourceError> {
        Ok(Vec::new())
    }

    fn read(&self, _locale_dir: &str, _file_name: &str) -> Result<Option<Vec<u8>>, SourceError> {
        Ok(None)
    }
}

/// A Help tree holding one canonical `mv.md` document.
struct OneDoc;

const DOC: &str = "## NAME\n\nmv — move files and directories\n\n\
                   ## SYNOPSIS\n\n`mv [-f] [--] source... dest`\n\n\
                   ## DESCRIPTION\n\nMoves things.\n";

impl HelpSource for OneDoc {
    fn locale_dirs(&self) -> Result<Vec<String>, SourceError> {
        Ok(alloc::vec![String::from("en-US")])
    }

    fn read(&self, locale_dir: &str, file_name: &str) -> Result<Option<Vec<u8>>, SourceError> {
        if locale_dir == "en-US" && file_name == "mv.md" {
            Ok(Some(DOC.as_bytes().to_vec()))
        } else {
            Ok(None)
        }
    }
}

/// The engine under a prompt that must never be reached — the shape every
/// pre-existing non-interactive test uses.
fn run(command: Command, fs: &dyn FileSystem, out: &Recorder) -> Result<(), MvError> {
    engine_run(command, None, fs, &NeverAsked, &NoHelp, out)
}

/// An in-memory tree. Regular files carry their bytes; directories are named
/// so their children can be derived by parent path. Failure injection covers
/// the rename/read/create/write/remove fail-closed paths, and a cross-device
/// prefix forces the copy-then-remove relocation.
struct MemFs {
    state: RefCell<State>,
}

struct State {
    files: Vec<(String, Vec<u8>)>,
    dirs: Vec<String>,
    /// A `source` under this path prefix renames cross-device.
    cross_device: Option<String>,
    /// `rename` with this source fails with the errno.
    rename_fail: Option<(String, Errno)>,
    /// When set, `rename` fails with [`Errno::PermissionDenied`] while the
    /// destination still exists, modelling a destination that blocks an
    /// atomic rename until it is removed (the `-f` recovery path).
    block_if_dest_exists: bool,
    /// `read` of this path fails with the errno.
    read_fail: Option<(String, Errno)>,
    /// `write` to this path fails with the errno.
    write_fail: Option<(String, Errno)>,
    /// `remove_file`/`remove_dir` of this path fails with the errno.
    remove_fail: Option<(String, Errno)>,
}

impl MemFs {
    fn new() -> Self {
        Self {
            state: RefCell::new(State {
                files: Vec::new(),
                dirs: Vec::new(),
                cross_device: None,
                rename_fail: None,
                block_if_dest_exists: false,
                read_fail: None,
                write_fail: None,
                remove_fail: None,
            }),
        }
    }

    fn file(self, path: &str, contents: &[u8]) -> Self {
        self.state
            .borrow_mut()
            .files
            .push((path.to_string(), contents.to_vec()));
        self
    }

    fn dir(self, path: &str) -> Self {
        self.state.borrow_mut().dirs.push(path.to_string());
        self
    }

    fn cross_device(self, prefix: &str) -> Self {
        self.state.borrow_mut().cross_device = Some(prefix.to_string());
        self
    }

    fn rename_fails(self, source: &str, errno: Errno) -> Self {
        self.state.borrow_mut().rename_fail = Some((source.to_string(), errno));
        self
    }

    fn blocks_until_dest_removed(self) -> Self {
        self.state.borrow_mut().block_if_dest_exists = true;
        self
    }

    fn read_fails(self, path: &str, errno: Errno) -> Self {
        self.state.borrow_mut().read_fail = Some((path.to_string(), errno));
        self
    }

    fn write_fails(self, path: &str, errno: Errno) -> Self {
        self.state.borrow_mut().write_fail = Some((path.to_string(), errno));
        self
    }

    fn remove_fails(self, path: &str, errno: Errno) -> Self {
        self.state.borrow_mut().remove_fail = Some((path.to_string(), errno));
        self
    }

    fn contents(&self, path: &str) -> Option<Vec<u8>> {
        self.state
            .borrow()
            .files
            .iter()
            .find(|(p, _)| p == canon(path))
            .map(|(_, bytes)| bytes.clone())
    }

    fn has_dir(&self, path: &str) -> bool {
        self.state.borrow().dirs.iter().any(|p| p == canon(path))
    }

    fn exists(&self, path: &str) -> bool {
        self.contents(path).is_some() || self.has_dir(path)
    }
}

/// Canonicalise a path: a trailing slash on anything but the root names the
/// same object (`/d/` is `/d`).
fn canon(path: &str) -> &str {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        "/"
    } else {
        trimmed
    }
}

/// The immediate parent of a path: `/a/b` maps to `/a`, and a top-level `/a`
/// maps to the empty string (its children sit at the root level).
fn parent_of(path: &str) -> &str {
    match path.rfind('/') {
        Some(0) => "/",
        Some(slash) => &path[..slash],
        None => "",
    }
}

/// The final component of a path.
fn name_of(path: &str) -> &str {
    match path.rfind('/') {
        Some(slash) => &path[slash + 1..],
        None => path,
    }
}

impl FileSystem for MemFs {
    fn kind(&self, path: &str) -> Result<EntryKind, Errno> {
        let path = canon(path);
        let state = self.state.borrow();
        if state.dirs.iter().any(|p| p == path) {
            return Ok(EntryKind::Directory);
        }
        if state.files.iter().any(|(p, _)| p == path) {
            return Ok(EntryKind::File);
        }
        Err(Errno::NotFound)
    }

    fn rename(&self, source: &str, dest: &str) -> Result<RenameOutcome, Errno> {
        let source = canon(source);
        let dest = canon(dest);
        let mut state = self.state.borrow_mut();
        if let Some((p, errno)) = &state.rename_fail {
            if p == source {
                return Err(*errno);
            }
        }
        if let Some(prefix) = &state.cross_device {
            if source.starts_with(prefix.as_str()) {
                return Ok(RenameOutcome::CrossDevice);
            }
        }
        let dest_exists =
            state.files.iter().any(|(p, _)| p == dest) || state.dirs.iter().any(|p| p == dest);
        if state.block_if_dest_exists && dest_exists {
            return Err(Errno::PermissionDenied);
        }
        // Re-point every file and directory whose path is `source` or sits
        // beneath it, onto `dest`.
        let prefix = alloc::format!("{source}/");
        let repath = |p: &str| -> Option<String> {
            if p == source {
                Some(dest.to_string())
            } else {
                p.strip_prefix(&prefix)
                    .map(|rest| alloc::format!("{dest}/{rest}"))
            }
        };
        // Replace an existing destination of the same shape first.
        state.files.retain(|(p, _)| p != dest);
        state.dirs.retain(|p| p != dest);
        for (p, _) in &mut state.files {
            if let Some(np) = repath(p) {
                *p = np;
            }
        }
        for p in &mut state.dirs {
            if let Some(np) = repath(p) {
                *p = np;
            }
        }
        Ok(RenameOutcome::Renamed)
    }

    fn read(&self, path: &str, offset: u64, buf: &mut [u8]) -> Result<usize, Errno> {
        let path = canon(path);
        let state = self.state.borrow();
        if let Some((p, errno)) = &state.read_fail {
            if p == path {
                return Err(*errno);
            }
        }
        let (_, bytes) = state
            .files
            .iter()
            .find(|(p, _)| p == path)
            .ok_or(Errno::NotFound)?;
        let start = usize::try_from(offset).map_err(|_| Errno::LengthOutOfRange)?;
        if start >= bytes.len() {
            return Ok(0);
        }
        let take = (bytes.len() - start).min(buf.len());
        buf[..take].copy_from_slice(&bytes[start..start + take]);
        Ok(take)
    }

    fn read_dir(&self, path: &str, index: u64) -> Result<Option<Entry>, Errno> {
        let path = canon(path);
        let state = self.state.borrow();
        if !state.dirs.iter().any(|p| p == path) {
            return Err(Errno::NotFound);
        }
        let mut entries = Vec::new();
        for (p, _) in &state.files {
            if parent_of(p) == path {
                entries.push(Entry {
                    name: name_of(p).to_string(),
                    kind: EntryKind::File,
                });
            }
        }
        for p in &state.dirs {
            if parent_of(p) == path {
                entries.push(Entry {
                    name: name_of(p).to_string(),
                    kind: EntryKind::Directory,
                });
            }
        }
        let idx = usize::try_from(index).map_err(|_| Errno::LengthOutOfRange)?;
        Ok(entries.into_iter().nth(idx))
    }

    fn mkdir(&self, path: &str) -> Result<(), Errno> {
        self.state.borrow_mut().dirs.push(canon(path).to_string());
        Ok(())
    }

    fn create(&self, path: &str) -> Result<(), Errno> {
        let path = canon(path);
        let mut state = self.state.borrow_mut();
        if let Some((_, bytes)) = state.files.iter_mut().find(|(p, _)| p == path) {
            bytes.clear();
        } else {
            state.files.push((path.to_string(), Vec::new()));
        }
        Ok(())
    }

    fn write(&self, path: &str, offset: u64, bytes: &[u8]) -> Result<(), Errno> {
        let path = canon(path);
        let mut state = self.state.borrow_mut();
        if let Some((p, errno)) = &state.write_fail {
            if p == path {
                return Err(*errno);
            }
        }
        let start = usize::try_from(offset).map_err(|_| Errno::LengthOutOfRange)?;
        let (_, file) = state
            .files
            .iter_mut()
            .find(|(p, _)| p == path)
            .ok_or(Errno::NotFound)?;
        let end = start + bytes.len();
        if file.len() < end {
            file.resize(end, 0);
        }
        file[start..end].copy_from_slice(bytes);
        Ok(())
    }

    fn remove_file(&self, path: &str) -> Result<(), Errno> {
        let path = canon(path);
        let mut state = self.state.borrow_mut();
        if let Some((p, errno)) = &state.remove_fail {
            if p == path {
                return Err(*errno);
            }
        }
        state.files.retain(|(p, _)| p != path);
        Ok(())
    }

    fn remove_dir(&self, path: &str) -> Result<(), Errno> {
        let path = canon(path);
        let mut state = self.state.borrow_mut();
        if let Some((p, errno)) = &state.remove_fail {
            if p == path {
                return Err(*errno);
            }
        }
        state.dirs.retain(|p| p != path);
        Ok(())
    }
}

/// Captures every byte written; optionally fails on the first write.
struct Recorder {
    bytes: RefCell<Vec<u8>>,
    fail: bool,
}

impl Recorder {
    fn new() -> Self {
        Self {
            bytes: RefCell::new(Vec::new()),
            fail: false,
        }
    }

    fn failing() -> Self {
        Self {
            bytes: RefCell::new(Vec::new()),
            fail: true,
        }
    }

    fn text(&self) -> String {
        String::from_utf8(self.bytes.borrow().clone()).expect("utf8 output")
    }
}

impl Output for Recorder {
    fn write_all(&self, bytes: &[u8]) -> Result<(), Errno> {
        if self.fail {
            return Err(Errno::NotFound);
        }
        self.bytes.borrow_mut().extend_from_slice(bytes);
        Ok(())
    }
}

fn mv_with(options: Options, sources: &[&str], dest: &str) -> Command {
    Command::Move {
        options,
        sources: sources.iter().map(|p| (*p).to_string()).collect::<Vec<_>>(),
        dest: dest.to_string(),
    }
}

fn mv(force: bool, no_clobber: bool, sources: &[&str], dest: &str) -> Command {
    mv_with(
        Options {
            force,
            clobber: if no_clobber {
                Clobber::Skip
            } else {
                Clobber::Overwrite
            },
            ..Options::DEFAULT
        },
        sources,
        dest,
    )
}

#[test]
fn help_writes_usage() {
    let fs = MemFs::new();
    let out = Recorder::new();
    assert_eq!(run(Command::Help, &fs, &out), Ok(()));
    assert_eq!(out.text(), USAGE);
}

#[test]
fn help_renders_the_short_help_from_the_document() {
    let fs = MemFs::new();
    let out = Recorder::new();
    assert_eq!(
        engine_run(Command::Help, None, &fs, &NeverAsked, &OneDoc, &out),
        Ok(())
    );
    let text = out.text();
    assert!(text.contains("mv — move files and directories"), "{text}");
    assert!(text.contains("mv [-f] [--] source... dest"), "{text}");
}

#[test]
fn renames_a_file_to_a_new_path() {
    let fs = MemFs::new().file("/a.txt", b"hello");
    let out = Recorder::new();
    assert_eq!(
        run(mv(false, false, &["/a.txt"], "/b.txt"), &fs, &out),
        Ok(())
    );
    assert_eq!(fs.contents("/b.txt").as_deref(), Some(&b"hello"[..]));
    // The source no longer exists; `mv` is silent on success.
    assert!(!fs.exists("/a.txt"));
    assert_eq!(out.text(), "");
}

#[test]
fn renames_a_directory() {
    let fs = MemFs::new().dir("/d").file("/d/f", b"x");
    let out = Recorder::new();
    assert_eq!(run(mv(false, false, &["/d"], "/e"), &fs, &out), Ok(()));
    assert!(fs.has_dir("/e"));
    assert!(!fs.has_dir("/d"));
    assert_eq!(fs.contents("/e/f").as_deref(), Some(&b"x"[..]));
}

#[test]
fn moves_a_file_into_an_existing_directory_under_its_basename() {
    let fs = MemFs::new().file("/src/a.txt", b"data").dir("/dst");
    let out = Recorder::new();
    assert_eq!(
        run(mv(false, false, &["/src/a.txt"], "/dst"), &fs, &out),
        Ok(())
    );
    assert_eq!(fs.contents("/dst/a.txt").as_deref(), Some(&b"data"[..]));
    assert!(!fs.exists("/src/a.txt"));
}

#[test]
fn moves_several_files_into_a_directory() {
    let fs = MemFs::new()
        .file("/a", b"one")
        .file("/b", b"two")
        .dir("/dst");
    let out = Recorder::new();
    assert_eq!(
        run(mv(false, false, &["/a", "/b"], "/dst"), &fs, &out),
        Ok(())
    );
    assert_eq!(fs.contents("/dst/a").as_deref(), Some(&b"one"[..]));
    assert_eq!(fs.contents("/dst/b").as_deref(), Some(&b"two"[..]));
}

#[test]
fn several_sources_to_a_non_directory_dest_is_usage() {
    let fs = MemFs::new().file("/a", b"x").file("/b", b"y");
    let out = Recorder::new();
    assert_eq!(
        run(mv(false, false, &["/a", "/b"], "/c"), &fs, &out),
        Err(MvError::Usage)
    );
    assert!(!fs.exists("/c"));
}

#[test]
fn a_missing_source_fails_closed() {
    let fs = MemFs::new();
    let out = Recorder::new();
    assert_eq!(
        run(mv(false, false, &["/absent"], "/dst"), &fs, &out),
        Err(MvError::Stat(Errno::NotFound))
    );
}

#[test]
fn a_failure_stops_before_a_later_source() {
    let fs = MemFs::new().file("/b", b"present").dir("/dst");
    let out = Recorder::new();
    assert_eq!(
        run(mv(false, false, &["/absent", "/b"], "/dst"), &fs, &out),
        Err(MvError::Stat(Errno::NotFound))
    );
    // The second source was never moved.
    assert_eq!(fs.contents("/b").as_deref(), Some(&b"present"[..]));
    assert!(!fs.exists("/dst/b"));
}

#[test]
fn no_clobber_skips_an_existing_destination() {
    let fs = MemFs::new().file("/a", b"new").file("/b", b"old");
    let out = Recorder::new();
    assert_eq!(run(mv(false, true, &["/a"], "/b"), &fs, &out), Ok(()));
    // The destination is untouched and the source remains.
    assert_eq!(fs.contents("/b").as_deref(), Some(&b"old"[..]));
    assert_eq!(fs.contents("/a").as_deref(), Some(&b"new"[..]));
}

#[test]
fn overwrites_an_existing_destination_by_default() {
    let fs = MemFs::new().file("/a", b"new").file("/b", b"old");
    let out = Recorder::new();
    assert_eq!(run(mv(false, false, &["/a"], "/b"), &fs, &out), Ok(()));
    assert_eq!(fs.contents("/b").as_deref(), Some(&b"new"[..]));
    assert!(!fs.exists("/a"));
}

#[test]
fn a_failed_rename_surfaces_the_errno() {
    let fs = MemFs::new()
        .file("/a", b"x")
        .rename_fails("/a", Errno::PermissionDenied);
    let out = Recorder::new();
    assert_eq!(
        run(mv(false, false, &["/a"], "/b"), &fs, &out),
        Err(MvError::Rename(Errno::PermissionDenied))
    );
}

#[test]
fn a_blocking_destination_without_force_fails_closed() {
    let fs = MemFs::new()
        .file("/a", b"fresh")
        .file("/b", b"stale")
        .blocks_until_dest_removed();
    let out = Recorder::new();
    assert_eq!(
        run(mv(false, false, &["/a"], "/b"), &fs, &out),
        Err(MvError::Rename(Errno::PermissionDenied))
    );
    // Nothing moved: the source and the stale destination both remain.
    assert_eq!(fs.contents("/a").as_deref(), Some(&b"fresh"[..]));
    assert_eq!(fs.contents("/b").as_deref(), Some(&b"stale"[..]));
}

#[test]
fn force_removes_a_blocking_destination_and_retries() {
    let fs = MemFs::new()
        .file("/a", b"fresh")
        .file("/b", b"stale")
        .blocks_until_dest_removed();
    let out = Recorder::new();
    // Without -f this is a Rename error; -f removes /b so the retried rename
    // is no longer blocked and succeeds.
    assert_eq!(run(mv(true, false, &["/a"], "/b"), &fs, &out), Ok(()));
    assert_eq!(fs.contents("/b").as_deref(), Some(&b"fresh"[..]));
    assert!(!fs.exists("/a"));
}

#[test]
fn cross_device_file_is_copied_then_removed() {
    let fs = MemFs::new().file("/src/a", b"payload").cross_device("/src");
    let out = Recorder::new();
    assert_eq!(
        run(mv(false, false, &["/src/a"], "/dst"), &fs, &out),
        Ok(())
    );
    assert_eq!(fs.contents("/dst").as_deref(), Some(&b"payload"[..]));
    assert!(!fs.exists("/src/a"));
}

#[test]
fn cross_device_large_file_streams_across_the_chunk_boundary() {
    let payload: Vec<u8> = (0..(super::READ_CHUNK * 2 + 7))
        .map(|i| u8::try_from(i % 251).unwrap_or_default())
        .collect();
    let fs = MemFs::new().file("/src/big", &payload).cross_device("/src");
    let out = Recorder::new();
    assert_eq!(
        run(mv(false, false, &["/src/big"], "/copy"), &fs, &out),
        Ok(())
    );
    assert_eq!(fs.contents("/copy"), Some(payload));
    assert!(!fs.exists("/src/big"));
}

#[test]
fn cross_device_directory_is_reproduced_then_removed() {
    let fs = MemFs::new()
        .dir("/src/d")
        .file("/src/d/f", b"top")
        .dir("/src/d/sub")
        .file("/src/d/sub/g", b"nested")
        .cross_device("/src");
    let out = Recorder::new();
    assert_eq!(run(mv(false, false, &["/src/d"], "/e"), &fs, &out), Ok(()));
    assert!(fs.has_dir("/e"));
    assert!(fs.has_dir("/e/sub"));
    assert_eq!(fs.contents("/e/f").as_deref(), Some(&b"top"[..]));
    assert_eq!(fs.contents("/e/sub/g").as_deref(), Some(&b"nested"[..]));
    // The whole source subtree is gone.
    assert!(!fs.has_dir("/src/d"));
    assert!(!fs.has_dir("/src/d/sub"));
    assert!(!fs.exists("/src/d/f"));
    assert!(!fs.exists("/src/d/sub/g"));
}

#[test]
fn cross_device_read_failure_surfaces_the_errno() {
    let fs = MemFs::new()
        .file("/src/a", b"x")
        .cross_device("/src")
        .read_fails("/src/a", Errno::PermissionDenied);
    let out = Recorder::new();
    assert_eq!(
        run(mv(false, false, &["/src/a"], "/dst"), &fs, &out),
        Err(MvError::Read(Errno::PermissionDenied))
    );
    // The source remains, because the copy failed before removal.
    assert!(fs.exists("/src/a"));
}

#[test]
fn cross_device_write_failure_surfaces_the_errno() {
    let fs = MemFs::new()
        .file("/src/a", b"x")
        .cross_device("/src")
        .write_fails("/dst", Errno::PermissionDenied);
    let out = Recorder::new();
    assert_eq!(
        run(mv(false, false, &["/src/a"], "/dst"), &fs, &out),
        Err(MvError::Write(Errno::PermissionDenied))
    );
}

#[test]
fn cross_device_remove_failure_surfaces_the_errno() {
    let fs = MemFs::new()
        .file("/src/a", b"x")
        .cross_device("/src")
        .remove_fails("/src/a", Errno::PermissionDenied);
    let out = Recorder::new();
    assert_eq!(
        run(mv(false, false, &["/src/a"], "/dst"), &fs, &out),
        Err(MvError::Remove(Errno::PermissionDenied))
    );
    // The destination already holds the copied data.
    assert_eq!(fs.contents("/dst").as_deref(), Some(&b"x"[..]));
}

#[test]
fn help_output_failure_propagates() {
    let fs = MemFs::new();
    let out = Recorder::failing();
    assert_eq!(
        run(Command::Help, &fs, &out),
        Err(MvError::Output(Errno::NotFound))
    );
}

#[test]
fn a_trailing_slash_source_moves_under_its_basename() {
    let fs = MemFs::new().dir("/d").file("/d/f", b"x").dir("/dst");
    let out = Recorder::new();
    assert_eq!(run(mv(false, false, &["/d/"], "/dst"), &fs, &out), Ok(()));
    assert!(fs.has_dir("/dst/d"));
    assert_eq!(fs.contents("/dst/d/f").as_deref(), Some(&b"x"[..]));
}

#[test]
fn an_alias_root_source_lands_under_its_own_root_spelling() {
    // Behaviour moved with the sweep onto the shared `leaf_name`: the private
    // base name this tool carried was not alias-aware and named `Home:/` as
    // `Home:`, dropping the separator that makes it a root.
    assert_eq!(tairix_path::leaf_name("Home:/"), "Home:/");
    // And an ordinary path is unchanged by the move.
    assert_eq!(tairix_path::leaf_name("/a/b/"), "b");
}

#[test]
fn an_empty_source_operand_names_nothing_and_fails_closed() {
    // The other moved answer: the private base name reported `/` for the
    // empty spelling, fabricating a root component the caller never typed.
    // `leaf_name` names nothing, and the move still fails on the unreachable
    // source rather than acting on a guessed path.
    assert_eq!(tairix_path::leaf_name(""), "");
    let fs = MemFs::new().dir("/dst");
    let out = Recorder::new();
    assert!(run(mv(false, false, &[""], "/dst"), &fs, &out).is_err());
}

#[test]
fn interactive_asks_before_overwriting() {
    let fs = MemFs::new()
        .file("/a", b"fresh")
        .file("/b", b"stale")
        .file("/c", b"old")
        .dir("/dst")
        .file("/dst/a", b"blockA")
        .file("/dst/c", b"blockC");
    let out = Recorder::new();
    let prompt = Answers::new(&[true, false]);
    assert_eq!(
        engine_run(
            mv_with(
                Options {
                    clobber: Clobber::Prompt,
                    ..Options::DEFAULT
                },
                &["/a", "/c"],
                "/dst",
            ),
            None,
            &fs,
            &prompt,
            &NoHelp,
            &out,
        ),
        Ok(())
    );
    // `/dst/a` was confirmed and replaced; `/dst/c` was declined and kept
    // (its source stays put), and the run still succeeds.
    assert_eq!(fs.contents("/dst/a").as_deref(), Some(&b"fresh"[..]));
    assert_eq!(fs.contents("/dst/c").as_deref(), Some(&b"blockC"[..]));
    assert_eq!(fs.contents("/c").as_deref(), Some(&b"old"[..]));
    assert_eq!(
        prompt.asked(),
        ["overwrite '/dst/a'?", "overwrite '/dst/c'?"]
    );
}

#[test]
fn an_unanswerable_prompt_fails_closed() {
    let fs = MemFs::new().file("/a", b"fresh").file("/b", b"stale");
    let out = Recorder::new();
    let prompt = Answers::new(&[]);
    assert_eq!(
        engine_run(
            mv_with(
                Options {
                    clobber: Clobber::Prompt,
                    ..Options::DEFAULT
                },
                &["/a"],
                "/b",
            ),
            None,
            &fs,
            &prompt,
            &NoHelp,
            &out,
        ),
        Err(MvError::Prompt(Errno::NotFound))
    );
    assert_eq!(fs.contents("/b").as_deref(), Some(&b"stale"[..]));
}

#[test]
fn verbose_reports_each_move_in_gnu_wording() {
    let fs = MemFs::new().file("/a", b"x").dir("/dst");
    let out = Recorder::new();
    assert_eq!(
        run(
            mv_with(
                Options {
                    verbose: true,
                    ..Options::DEFAULT
                },
                &["/a"],
                "/dst",
            ),
            &fs,
            &out,
        ),
        Ok(())
    );
    assert_eq!(out.text(), "renamed '/a' -> '/dst/a'\n");
}

#[test]
fn target_directory_moves_every_source_into_it() {
    let fs = MemFs::new()
        .file("/a", b"one")
        .file("/b", b"two")
        .dir("/dst");
    let out = Recorder::new();
    assert_eq!(
        run(
            mv_with(
                Options {
                    target_mode: TargetMode::Directory,
                    ..Options::DEFAULT
                },
                &["/a", "/b"],
                "/dst",
            ),
            &fs,
            &out,
        ),
        Ok(())
    );
    assert_eq!(fs.contents("/dst/a").as_deref(), Some(&b"one"[..]));
    assert_eq!(fs.contents("/dst/b").as_deref(), Some(&b"two"[..]));
}

#[test]
fn target_directory_must_exist_and_be_a_directory() {
    let fs = MemFs::new().file("/a", b"one").file("/blocker", b"x");
    let out = Recorder::new();
    assert_eq!(
        run(
            mv_with(
                Options {
                    target_mode: TargetMode::Directory,
                    ..Options::DEFAULT
                },
                &["/a"],
                "/blocker",
            ),
            &fs,
            &out,
        ),
        Err(MvError::Usage)
    );
    assert_eq!(
        run(
            mv_with(
                Options {
                    target_mode: TargetMode::Directory,
                    ..Options::DEFAULT
                },
                &["/a"],
                "/absent",
            ),
            &fs,
            &out,
        ),
        Err(MvError::Stat(Errno::NotFound))
    );
}

#[test]
fn no_target_directory_refuses_several_sources() {
    let fs = MemFs::new().file("/a", b"x").file("/b", b"y").dir("/dst");
    let out = Recorder::new();
    assert_eq!(
        run(
            mv_with(
                Options {
                    target_mode: TargetMode::NoDirectory,
                    ..Options::DEFAULT
                },
                &["/a", "/b"],
                "/dst",
            ),
            &fs,
            &out,
        ),
        Err(MvError::Usage)
    );
}
