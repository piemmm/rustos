//! The copy engine: resolve each source's destination, stream regular files,
//! and reproduce each directory `-r` must copy — creating the destination
//! directory before its contents.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use tairix_help::{own_short_help, HelpSource};

use crate::command::{Clobber, Command, Options, TargetMode};
use crate::error::CpError;
use crate::io::{Entry, EntryKind, FileSystem, Output, Prompt};

/// The fixed-size chunk used to stream a regular file from source to
/// destination. Matches `cat`'s `READ_CHUNK` so the userland tools share one
/// streaming granularity.
const READ_CHUNK: usize = 4096;

/// The usage banner a usage error is reported with, and the fallback the
/// short-help switches print when `cp`'s own Help tree is unavailable.
pub const USAGE: &str = "\
usage: cp [-finrRvT] [-t dir] [--] source... dest

  -r, -R, --recursive        copy directories and their contents
  -f, --force                remove an unwritable destination and retry
  -i, --interactive          ask before overwriting an existing file
  -n, --no-clobber           never overwrite an existing file
  -v, --verbose              report each copy
  -t dir, --target-directory=dir
                             copy every source into dir
  -T, --no-target-directory  treat dest as a normal file (one source)
  -h, -?, --help             show this message

With one source and a non-directory dest, the source is copied to dest. When
dest is an existing directory (always, with more than one source) each source
is copied into it under its base name. `--` ends option parsing: every later
argument is a path.
";

/// `cp`'s own command word: the short-help switches render its own Help
/// document through the same engine as any other command's.
const OWN_WORD: &str = "cp";

/// Run one [`Command`], copying its sources through `fs`. `locale` is the
/// user's `LANG` preference, if set; `help` is the tool's own `Help/` tree,
/// read by the short-help switches.
///
/// A non-directory source is streamed to its destination; a directory source
/// is reproduced only with `-r`. When the destination is an existing
/// directory — and always with more than one source or `-t` — each source is
/// copied into it under its base name; `-T` uses the destination exactly as
/// given. An existing destination file is overwritten by default, skipped
/// under `-n`, and asked about through `prompt` under `-i` (a declined
/// question skips that copy without error). `-v` reports each copy on `out`;
/// otherwise `cp` writes nothing on success beyond the [`Command::Help`]
/// banner.
///
/// The first failure stops the run before any later operand (fail closed).
///
/// # Errors
///
/// * [`CpError::Usage`] — more than one source aimed at a non-directory
///   destination (or at any destination under `-T`).
/// * [`CpError::IsDirectory`] — a directory source was named without `-r`.
/// * [`CpError::NotADirectory`] — a directory source's destination already
///   exists as a non-directory, or the `-t` operand is not an existing
///   directory.
/// * [`CpError::Stat`] — an operand could not be inspected; carries the
///   underlying [`Errno`](tairix_abi::Errno).
/// * [`CpError::Read`] — a source file or directory could not be read.
/// * [`CpError::Create`] — a destination file or directory could not be made.
/// * [`CpError::Write`] — writing a destination file's bytes failed.
/// * [`CpError::Prompt`] — a confirmation could not be read (never treated
///   as consent).
/// * [`CpError::Output`] — writing the usage banner or a `-v` report failed.
pub fn run(
    command: Command,
    locale: Option<&str>,
    fs: &dyn FileSystem,
    prompt: &dyn Prompt,
    help: &dyn HelpSource,
    out: &dyn Output,
) -> Result<(), CpError> {
    match command {
        Command::Help => short_help(locale, help, out),
        Command::Copy {
            options,
            sources,
            dest,
        } => copy_all(&sources, &dest, options, fs, prompt, out),
    }
}

/// Render `cp`'s own short help (`NAME` + `SYNOPSIS` + compact `OPTIONS`)
/// from its own Help tree through the one shared engine; when no document
/// can be served (a build without the bundle's documents) the usage banner
/// stands in — the tool's own text, not fabricated help content — so `-h`
/// never fails.
fn short_help(
    locale: Option<&str>,
    help: &dyn HelpSource,
    out: &dyn Output,
) -> Result<(), CpError> {
    let bytes =
        own_short_help(help, locale, OWN_WORD).unwrap_or_else(|| String::from(USAGE).into_bytes());
    out.write_all(&bytes).map_err(CpError::Output)
}

/// Copy every source to `dest`, deciding per source whether the destination is
/// `dest` itself or a child of `dest` named after the source.
fn copy_all(
    sources: &[String],
    dest: &str,
    options: Options,
    fs: &dyn FileSystem,
    prompt: &dyn Prompt,
    out: &dyn Output,
) -> Result<(), CpError> {
    let dest_is_dir = match options.target_mode {
        // `-t`: the destination must be an existing directory.
        TargetMode::Directory => match stat(dest, fs)? {
            Some(EntryKind::Directory) => true,
            Some(EntryKind::File) => return Err(CpError::NotADirectory),
            None => return Err(CpError::Stat(tairix_abi::Errno::NotFound)),
        },
        // `-T`: the destination is a normal file for exactly one source.
        TargetMode::NoDirectory => {
            if sources.len() > 1 {
                return Err(CpError::Usage);
            }
            false
        }
        TargetMode::Inferred => matches!(stat(dest, fs)?, Some(EntryKind::Directory)),
    };
    // More than one source can only land inside a directory.
    if sources.len() > 1 && !dest_is_dir {
        return Err(CpError::Usage);
    }
    for source in sources {
        let target = if dest_is_dir {
            join(dest, basename(source))
        } else {
            String::from(dest)
        };
        let kind = fs.kind(source).map_err(CpError::Stat)?;
        copy_known(source, kind, &target, options, fs, prompt, out)?;
    }
    Ok(())
}

/// Report one copy under `-v`, in the GNU wording.
fn report(options: Options, out: &dyn Output, source: &str, target: &str) -> Result<(), CpError> {
    if !options.verbose {
        return Ok(());
    }
    out.write_all(format!("'{source}' -> '{target}'\n").as_bytes())
        .map_err(CpError::Output)
}

/// Copy an object whose [`EntryKind`] is already known (from the parent's
/// directory entry, or from the operand's stat), recursing into directories.
fn copy_known(
    source: &str,
    kind: EntryKind,
    target: &str,
    options: Options,
    fs: &dyn FileSystem,
    prompt: &dyn Prompt,
    out: &dyn Output,
) -> Result<(), CpError> {
    match kind {
        EntryKind::File => copy_file(source, target, options, fs, prompt, out),
        EntryKind::Directory => {
            if !options.recursive {
                return Err(CpError::IsDirectory);
            }
            match stat(target, fs)? {
                Some(EntryKind::Directory) => {}
                Some(EntryKind::File) => return Err(CpError::NotADirectory),
                None => {
                    fs.mkdir(target).map_err(CpError::Create)?;
                    report(options, out, source, target)?;
                }
            }
            for entry in read_children(source, fs)? {
                let child_source = join(source, &entry.name);
                let child_target = join(target, &entry.name);
                copy_known(
                    &child_source,
                    entry.kind,
                    &child_target,
                    options,
                    fs,
                    prompt,
                    out,
                )?;
            }
            Ok(())
        }
    }
}

/// Stream a regular file from `source` to `target`, honouring the
/// existing-destination policy (`-n` skips, `-i` asks) and `-f` (remove a
/// destination that cannot be created and retry the create once).
fn copy_file(
    source: &str,
    target: &str,
    options: Options,
    fs: &dyn FileSystem,
    prompt: &dyn Prompt,
    out: &dyn Output,
) -> Result<(), CpError> {
    if stat(target, fs)?.is_some() {
        match options.clobber {
            Clobber::Overwrite => {}
            Clobber::Skip => return Ok(()),
            Clobber::Prompt => {
                let question = format!("overwrite '{target}'?");
                if !prompt.confirm(&question).map_err(CpError::Prompt)? {
                    return Ok(());
                }
            }
        }
    }
    create_destination(target, options.force, fs)?;
    let mut offset: u64 = 0;
    let mut buf = [0_u8; READ_CHUNK];
    loop {
        let read = fs.read(source, offset, &mut buf).map_err(CpError::Read)?;
        if read == 0 {
            return report(options, out, source, target);
        }
        // A seam reporting more than the buffer holds would index out of
        // bounds; refuse it rather than trust the count.
        if read > buf.len() {
            return Err(CpError::Read(tairix_abi::Errno::LengthOutOfRange));
        }
        fs.write(target, offset, &buf[..read])
            .map_err(CpError::Write)?;
        offset = offset.saturating_add(read as u64);
    }
}

/// Create `target`, or — with `-f` — remove it and retry the create once.
fn create_destination(target: &str, force: bool, fs: &dyn FileSystem) -> Result<(), CpError> {
    match fs.create(target) {
        Ok(()) => Ok(()),
        Err(_) if force => {
            // The destination could not be created (e.g. it exists and is not
            // writable). `-f` removes it and retries exactly once; a removal
            // error is irrelevant if the retried create then succeeds.
            let _ = fs.remove_file(target);
            fs.create(target).map_err(CpError::Create)
        }
        Err(errno) => Err(CpError::Create(errno)),
    }
}

/// Read every entry of `path` into a vector, so the directory can be walked
/// without depending on entry indices staying stable as the copy proceeds.
fn read_children(path: &str, fs: &dyn FileSystem) -> Result<Vec<Entry>, CpError> {
    let mut entries = Vec::new();
    let mut index: u64 = 0;
    while let Some(entry) = fs.read_dir(path, index).map_err(CpError::Read)? {
        index = index.saturating_add(1);
        entries.push(entry);
    }
    Ok(entries)
}

/// Inspect `path`, mapping a missing path to [`None`] so a destination can be
/// probed for existence without treating absence as a failure.
fn stat(path: &str, fs: &dyn FileSystem) -> Result<Option<EntryKind>, CpError> {
    match fs.kind(path) {
        Ok(kind) => Ok(Some(kind)),
        Err(tairix_abi::Errno::NotFound) => Ok(None),
        Err(errno) => Err(CpError::Stat(errno)),
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
mod tests {
    use super::{run as engine_run, USAGE};
    use crate::command::{Clobber, Command, Options, TargetMode};
    use crate::error::CpError;
    use crate::io::{Entry, EntryKind, FileSystem, Output, Prompt};
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

        fn read(
            &self,
            _locale_dir: &str,
            _file_name: &str,
        ) -> Result<Option<Vec<u8>>, SourceError> {
            Ok(None)
        }
    }

    /// A Help tree holding one canonical `cp.md` document.
    struct OneDoc;

    const DOC: &str = "## NAME\n\ncp — copy files and directories\n\n\
                       ## SYNOPSIS\n\n`cp [-r] [--] source... dest`\n\n\
                       ## DESCRIPTION\n\nCopies things.\n";

    impl HelpSource for OneDoc {
        fn locale_dirs(&self) -> Result<Vec<String>, SourceError> {
            Ok(alloc::vec![String::from("en-US")])
        }

        fn read(&self, locale_dir: &str, file_name: &str) -> Result<Option<Vec<u8>>, SourceError> {
            if locale_dir == "en-US" && file_name == "cp.md" {
                Ok(Some(DOC.as_bytes().to_vec()))
            } else {
                Ok(None)
            }
        }
    }

    /// The engine under a prompt that must never be reached — the shape
    /// every pre-existing non-interactive test uses.
    fn run(command: Command, fs: &dyn FileSystem, out: &Recorder) -> Result<(), CpError> {
        engine_run(command, None, fs, &NeverAsked, &NoHelp, out)
    }

    /// An in-memory tree. Regular files carry their bytes; directories are
    /// named so their children can be derived by parent path. Failure
    /// injection covers the create/read/write fail-closed paths and the `-f`
    /// remove-then-recreate recovery.
    struct MemFs {
        state: RefCell<State>,
    }

    struct State {
        files: Vec<(String, Vec<u8>)>,
        dirs: Vec<String>,
        /// `create` of this path fails with the errno until it is removed.
        create_fail: Option<(String, Errno)>,
        /// `read` of this path fails with the errno.
        read_fail: Option<(String, Errno)>,
        /// `write` to this path fails with the errno.
        write_fail: Option<(String, Errno)>,
        /// Removals recorded in call order.
        removed: Vec<String>,
    }

    impl MemFs {
        fn new() -> Self {
            Self {
                state: RefCell::new(State {
                    files: Vec::new(),
                    dirs: Vec::new(),
                    create_fail: None,
                    read_fail: None,
                    write_fail: None,
                    removed: Vec::new(),
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

        fn create_fails(self, path: &str, errno: Errno) -> Self {
            self.state.borrow_mut().create_fail = Some((path.to_string(), errno));
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

        fn contents(&self, path: &str) -> Option<Vec<u8>> {
            self.state
                .borrow()
                .files
                .iter()
                .find(|(p, _)| p == path)
                .map(|(_, bytes)| bytes.clone())
        }

        fn has_dir(&self, path: &str) -> bool {
            self.state.borrow().dirs.iter().any(|p| p == path)
        }

        fn removed(&self) -> Vec<String> {
            self.state.borrow().removed.clone()
        }
    }

    /// Canonicalise a path the way a real filesystem would: a trailing slash
    /// on anything but the root names the same object (`/d/` is `/d`).
    fn canon(path: &str) -> &str {
        let trimmed = path.trim_end_matches('/');
        if trimmed.is_empty() {
            "/"
        } else {
            trimmed
        }
    }

    /// The immediate parent of a path: `/a/b` maps to `/a`, and a top-level
    /// `/a` maps to the empty string (its children sit at the root level).
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
            // Derive the directory's children from every file and directory
            // whose immediate parent is `path`, in a stable insertion order.
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
            if let Some((p, errno)) = &state.create_fail {
                if p == path {
                    return Err(*errno);
                }
            }
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
            state.removed.push(path.to_string());
            state.files.retain(|(p, _)| p != path);
            // A removed destination is no longer blocked from being created.
            if matches!(&state.create_fail, Some((p, _)) if p == path) {
                state.create_fail = None;
            }
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

    fn copy_with(options: Options, sources: &[&str], dest: &str) -> Command {
        Command::Copy {
            options,
            sources: sources.iter().map(|p| (*p).to_string()).collect::<Vec<_>>(),
            dest: dest.to_string(),
        }
    }

    fn copy(recursive: bool, force: bool, sources: &[&str], dest: &str) -> Command {
        copy_with(
            Options {
                recursive,
                force,
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
        assert!(text.contains("cp — copy files and directories"), "{text}");
        assert!(text.contains("cp [-r] [--] source... dest"), "{text}");
    }

    #[test]
    fn copies_a_single_file_to_a_new_path() {
        let fs = MemFs::new().file("/a.txt", b"hello world");
        let out = Recorder::new();
        assert_eq!(
            run(copy(false, false, &["/a.txt"], "/b.txt"), &fs, &out),
            Ok(())
        );
        assert_eq!(fs.contents("/b.txt").as_deref(), Some(&b"hello world"[..]));
        // The source is untouched, and `cp` is silent on success.
        assert_eq!(fs.contents("/a.txt").as_deref(), Some(&b"hello world"[..]));
        assert_eq!(out.text(), "");
    }

    #[test]
    fn copies_a_file_across_the_chunk_boundary() {
        // A payload larger than READ_CHUNK exercises the streaming loop.
        let payload: Vec<u8> = (0..(super::READ_CHUNK * 2 + 7))
            .map(|i| u8::try_from(i % 251).unwrap_or_default())
            .collect();
        let fs = MemFs::new().file("/big", &payload);
        let out = Recorder::new();
        assert_eq!(
            run(copy(false, false, &["/big"], "/copy"), &fs, &out),
            Ok(())
        );
        assert_eq!(fs.contents("/copy"), Some(payload));
    }

    #[test]
    fn copies_an_empty_file() {
        let fs = MemFs::new().file("/empty", b"");
        let out = Recorder::new();
        assert_eq!(
            run(copy(false, false, &["/empty"], "/dup"), &fs, &out),
            Ok(())
        );
        assert_eq!(fs.contents("/dup").as_deref(), Some(&b""[..]));
    }

    #[test]
    fn copies_a_file_into_an_existing_directory_under_its_basename() {
        let fs = MemFs::new().file("/src/a.txt", b"data").dir("/dst");
        let out = Recorder::new();
        assert_eq!(
            run(copy(false, false, &["/src/a.txt"], "/dst"), &fs, &out),
            Ok(())
        );
        assert_eq!(fs.contents("/dst/a.txt").as_deref(), Some(&b"data"[..]));
    }

    #[test]
    fn copies_several_files_into_a_directory() {
        let fs = MemFs::new()
            .file("/a", b"one")
            .file("/b", b"two")
            .dir("/dst");
        let out = Recorder::new();
        assert_eq!(
            run(copy(false, false, &["/a", "/b"], "/dst"), &fs, &out),
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
            run(copy(false, false, &["/a", "/b"], "/c"), &fs, &out),
            Err(CpError::Usage)
        );
        // Nothing was created.
        assert!(fs.contents("/c").is_none());
    }

    #[test]
    fn a_directory_source_without_recursive_fails_closed() {
        let fs = MemFs::new().dir("/d");
        let out = Recorder::new();
        assert_eq!(
            run(copy(false, false, &["/d"], "/e"), &fs, &out),
            Err(CpError::IsDirectory)
        );
        assert!(!fs.has_dir("/e"));
    }

    #[test]
    fn recursive_reproduces_a_subtree() {
        // /d holds a file and a nested directory with its own file.
        let fs = MemFs::new()
            .dir("/d")
            .file("/d/f", b"top")
            .dir("/d/sub")
            .file("/d/sub/g", b"nested");
        let out = Recorder::new();
        assert_eq!(run(copy(true, false, &["/d"], "/e"), &fs, &out), Ok(()));
        assert!(fs.has_dir("/e"));
        assert!(fs.has_dir("/e/sub"));
        assert_eq!(fs.contents("/e/f").as_deref(), Some(&b"top"[..]));
        assert_eq!(fs.contents("/e/sub/g").as_deref(), Some(&b"nested"[..]));
    }

    #[test]
    fn recursive_into_an_existing_directory_merges() {
        let fs = MemFs::new().dir("/d").file("/d/f", b"x").dir("/dst");
        let out = Recorder::new();
        // The destination already exists, so /d is reproduced as /dst/d.
        assert_eq!(run(copy(true, false, &["/d"], "/dst"), &fs, &out), Ok(()));
        assert!(fs.has_dir("/dst/d"));
        assert_eq!(fs.contents("/dst/d/f").as_deref(), Some(&b"x"[..]));
    }

    #[test]
    fn recursive_onto_an_existing_file_fails_closed() {
        let fs = MemFs::new()
            .dir("/d")
            .file("/d/f", b"x")
            .file("/e", b"blocker");
        let out = Recorder::new();
        assert_eq!(
            run(copy(true, false, &["/d"], "/e"), &fs, &out),
            Err(CpError::NotADirectory)
        );
    }

    #[test]
    fn a_missing_source_fails_closed() {
        let fs = MemFs::new();
        let out = Recorder::new();
        assert_eq!(
            run(copy(false, false, &["/absent"], "/dst"), &fs, &out),
            Err(CpError::Stat(Errno::NotFound))
        );
    }

    #[test]
    fn a_failure_stops_before_a_later_source() {
        let fs = MemFs::new().file("/b", b"present").dir("/dst");
        let out = Recorder::new();
        // The first source is missing, so the second is never copied.
        assert_eq!(
            run(copy(false, false, &["/absent", "/b"], "/dst"), &fs, &out),
            Err(CpError::Stat(Errno::NotFound))
        );
        assert!(fs.contents("/dst/b").is_none());
    }

    #[test]
    fn an_unreadable_source_surfaces_the_errno() {
        let fs = MemFs::new()
            .file("/a", b"data")
            .read_fails("/a", Errno::PermissionDenied);
        let out = Recorder::new();
        assert_eq!(
            run(copy(false, false, &["/a"], "/b"), &fs, &out),
            Err(CpError::Read(Errno::PermissionDenied))
        );
    }

    #[test]
    fn an_uncreatable_destination_surfaces_the_errno() {
        let fs = MemFs::new()
            .file("/a", b"data")
            .create_fails("/b", Errno::PermissionDenied);
        let out = Recorder::new();
        assert_eq!(
            run(copy(false, false, &["/a"], "/b"), &fs, &out),
            Err(CpError::Create(Errno::PermissionDenied))
        );
    }

    #[test]
    fn force_removes_an_unwritable_destination_and_retries() {
        let fs = MemFs::new()
            .file("/a", b"fresh")
            .file("/b", b"stale")
            .create_fails("/b", Errno::PermissionDenied);
        let out = Recorder::new();
        // Without -f this would be a Create error; -f removes /b and retries.
        assert_eq!(run(copy(false, true, &["/a"], "/b"), &fs, &out), Ok(()));
        assert_eq!(fs.removed(), ["/b"]);
        assert_eq!(fs.contents("/b").as_deref(), Some(&b"fresh"[..]));
    }

    #[test]
    fn a_failed_write_surfaces_the_errno() {
        let fs = MemFs::new()
            .file("/a", b"data")
            .write_fails("/b", Errno::PermissionDenied);
        let out = Recorder::new();
        assert_eq!(
            run(copy(false, false, &["/a"], "/b"), &fs, &out),
            Err(CpError::Write(Errno::PermissionDenied))
        );
    }

    #[test]
    fn help_output_failure_propagates() {
        let fs = MemFs::new();
        let out = Recorder::failing();
        assert_eq!(
            run(Command::Help, &fs, &out),
            Err(CpError::Output(Errno::NotFound))
        );
    }

    #[test]
    fn a_trailing_slash_source_copies_under_its_basename() {
        // `cp -r /d/ /dst` copies into /dst/d, not /dst//.
        let fs = MemFs::new().dir("/d").file("/d/f", b"x").dir("/dst");
        let out = Recorder::new();
        assert_eq!(run(copy(true, false, &["/d/"], "/dst"), &fs, &out), Ok(()));
        assert!(fs.has_dir("/dst/d"));
        assert_eq!(fs.contents("/dst/d/f").as_deref(), Some(&b"x"[..]));
    }

    #[test]
    fn no_clobber_skips_an_existing_destination() {
        let fs = MemFs::new().file("/a", b"fresh").file("/b", b"stale");
        let out = Recorder::new();
        assert_eq!(
            run(
                copy_with(
                    Options {
                        clobber: Clobber::Skip,
                        ..Options::DEFAULT
                    },
                    &["/a"],
                    "/b",
                ),
                &fs,
                &out,
            ),
            Ok(())
        );
        // The destination keeps its bytes; nothing was copied.
        assert_eq!(fs.contents("/b").as_deref(), Some(&b"stale"[..]));
    }

    #[test]
    fn no_clobber_still_copies_to_a_new_destination() {
        let fs = MemFs::new().file("/a", b"fresh");
        let out = Recorder::new();
        assert_eq!(
            run(
                copy_with(
                    Options {
                        clobber: Clobber::Skip,
                        ..Options::DEFAULT
                    },
                    &["/a"],
                    "/b",
                ),
                &fs,
                &out,
            ),
            Ok(())
        );
        assert_eq!(fs.contents("/b").as_deref(), Some(&b"fresh"[..]));
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
                copy_with(
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
        // `/dst/a` was confirmed and overwritten; `/dst/c` was declined and
        // kept, and the run still succeeds.
        assert_eq!(fs.contents("/dst/a").as_deref(), Some(&b"fresh"[..]));
        assert_eq!(fs.contents("/dst/c").as_deref(), Some(&b"blockC"[..]));
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
                copy_with(
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
            Err(CpError::Prompt(Errno::NotFound))
        );
        assert_eq!(fs.contents("/b").as_deref(), Some(&b"stale"[..]));
    }

    #[test]
    fn verbose_reports_each_copy_in_gnu_wording() {
        let fs = MemFs::new().dir("/d").file("/d/f", b"x").dir("/dst");
        let out = Recorder::new();
        assert_eq!(
            run(
                copy_with(
                    Options {
                        recursive: true,
                        verbose: true,
                        ..Options::DEFAULT
                    },
                    &["/d"],
                    "/dst",
                ),
                &fs,
                &out,
            ),
            Ok(())
        );
        assert_eq!(out.text(), "'/d' -> '/dst/d'\n'/d/f' -> '/dst/d/f'\n");
    }

    #[test]
    fn target_directory_copies_every_source_into_it() {
        let fs = MemFs::new()
            .file("/a", b"one")
            .file("/b", b"two")
            .dir("/dst");
        let out = Recorder::new();
        assert_eq!(
            run(
                copy_with(
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
                copy_with(
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
            Err(CpError::NotADirectory)
        );
        assert_eq!(
            run(
                copy_with(
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
            Err(CpError::Stat(Errno::NotFound))
        );
    }

    #[test]
    fn no_target_directory_refuses_several_sources() {
        let fs = MemFs::new().file("/a", b"x").file("/b", b"y").dir("/dst");
        let out = Recorder::new();
        assert_eq!(
            run(
                copy_with(
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
            Err(CpError::Usage)
        );
    }
}
