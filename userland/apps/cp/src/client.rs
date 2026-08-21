//! The copy engine: resolve each source's destination, stream regular files,
//! and reproduce each directory `-r` must copy — creating the destination
//! directory before its contents.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use tairix_help::{own_short_help, HelpSource};
use tairix_path::{join, leaf_name};

use crate::command::{Clobber, Command, Contents, Options, TargetMode};
use crate::error::CpError;
use crate::io::{Entry, EntryKind, FileSystem, Follow, Output, Probe, Prompt};

/// The fixed-size chunk used to stream a regular file from source to
/// destination. Matches `cat`'s `READ_CHUNK` so the userland tools share one
/// streaming granularity.
const READ_CHUNK: usize = 4096;

/// The usage banner a usage error is reported with, and the fallback the
/// short-help switches print when `cp`'s own Help tree is unavailable.
pub const USAGE: &str = "\
usage: cp [-dfilnPrRsvT] [-t dir] [--] source... dest

  -r, -R, --recursive        copy directories and their contents
  -f, --force                remove an unwritable destination and retry
  -i, --interactive          ask before overwriting an existing file
  -n, --no-clobber           never overwrite an existing file
  -l, --link                 give the destination a second name for the
                             source's node instead of copying its bytes
  -s, --symbolic-link        make a symbolic link naming the source
  -P, --no-dereference       reproduce a symbolic-link source as a link
                             rather than copying what it names
  --preserve=links           two sources naming one node get two names at
                             the destination, not two copies
  -d                         -P and --preserve=links together
  -v, --verbose              report each copy
  -t dir, --target-directory=dir
                             copy every source into dir
  -T, --no-target-directory  treat dest as a normal file (one source)
  -h, -?, --help             show this message

With one source and a non-directory dest, the source is copied to dest. When
dest is an existing directory (always, with more than one source) each source
is copied into it under its base name. `--` ends option parsing: every later
argument is a path.

-a/--archive and the other --preserve members are refused: --preserve=all
includes timestamps, which no call can set yet, so they are not honoured in
part. Use -dR for the rest of -a.
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
        } => Copier {
            options,
            linked: Linked::default(),
            fs,
            prompt,
            out,
        }
        .copy_all(&sources, &dest),
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

/// One `cp` run: its options, its seams, and the sharing it has preserved so
/// far.
///
/// Bundling them is what keeps every step a two- or three-argument method
/// rather than an eight-argument function — the shape `du`'s `Reporter` uses
/// for the same reason.
struct Copier<'a> {
    options: Options,
    /// The destinations already written for each multiply-named source node
    /// (`--preserve=links`).
    linked: Linked,
    fs: &'a dyn FileSystem,
    prompt: &'a dyn Prompt,
    out: &'a dyn Output,
}

/// The destinations already written for each multiply-named source node, so
/// `--preserve=links` gives a second source naming one node a second *name*
/// rather than a second copy.
///
/// Only a node whose name count exceeds one is remembered — a node named once
/// cannot be met twice — so the map holds the hard links a copy actually
/// meets rather than one entry per node on the volume. It grows on demand
/// from no fixed ceiling; a heap that refuses to grow it makes the copy fall
/// back to copying the bytes, which is correct output rather than a failure,
/// and the destinations simply are not linked.
#[derive(Default)]
struct Linked {
    /// `(source node identity, the destination first written for it)`,
    /// sorted by identity so a lookup is a binary search.
    seen: Vec<(tairix_abi::FileId, String)>,
}

impl Linked {
    /// The destination already written for `probe`'s node, if any.
    fn destination_of(&self, probe: &Probe) -> Option<&str> {
        if probe.nlink <= 1 || probe.id.is_none() {
            return None;
        }
        let at = self
            .seen
            .binary_search_by_key(&probe.id, |(id, _)| *id)
            .ok()?;
        self.seen.get(at).map(|(_, target)| target.as_str())
    }

    /// Remember `target` as the destination written for `probe`'s node.
    ///
    /// A node that cannot recur, or one the backing offers no identity for,
    /// is not remembered; nor is anything, if the heap cannot grow the map.
    fn remember(&mut self, probe: &Probe, target: &str) {
        if probe.nlink <= 1 || probe.id.is_none() {
            return;
        }
        let Err(at) = self.seen.binary_search_by_key(&probe.id, |(id, _)| *id) else {
            return;
        };
        if self.seen.try_reserve_exact(1).is_err() {
            return;
        }
        self.seen.insert(at, (probe.id, String::from(target)));
    }
}

impl Copier<'_> {
    /// Copy every source to `dest`, deciding per source whether the
    /// destination is `dest` itself or a child of `dest` named after the
    /// source.
    fn copy_all(&mut self, sources: &[String], dest: &str) -> Result<(), CpError> {
        // A destination is always probed through a final link: a directory
        // reached by a link is still the directory the copies land in, and
        // `-P`/`-d` are about how *sources* are read, never where output goes.
        let dest_is_dir = match self.options.target_mode {
            // `-t`: the destination must be an existing directory.
            TargetMode::Directory => match self.stat(dest, Follow::Target)?.map(|p| p.kind) {
                Some(EntryKind::Directory) => true,
                Some(EntryKind::File | EntryKind::Symlink) => return Err(CpError::NotADirectory),
                None => return Err(CpError::Stat(tairix_abi::Errno::NotFound)),
            },
            // `-T`: the destination is a normal file for exactly one source.
            TargetMode::NoDirectory => {
                if sources.len() > 1 {
                    return Err(CpError::Usage);
                }
                false
            }
            TargetMode::Inferred => matches!(
                self.stat(dest, Follow::Target)?.map(|p| p.kind),
                Some(EntryKind::Directory)
            ),
        };
        // More than one source can only land inside a directory.
        if sources.len() > 1 && !dest_is_dir {
            return Err(CpError::Usage);
        }
        for source in sources {
            let target = if dest_is_dir {
                join(dest, leaf_name(source))
            } else {
                String::from(dest)
            };
            let probe = self
                .fs
                .probe(source, self.source_follow())
                .map_err(CpError::Stat)?;
            self.copy_known(source, probe, &target)?;
        }
        Ok(())
    }

    /// Copy an object a probe already described (from the parent's directory
    /// entry, or from the operand's own probe), recursing into directories.
    fn copy_known(&mut self, source: &str, probe: Probe, target: &str) -> Result<(), CpError> {
        match probe.kind {
            EntryKind::File => self.reproduce_file(source, &probe, target),
            // Only a `Follow::Keep` probe reports a link, i.e. only under
            // `-P`/`-d`: the link itself is reproduced by storing the same
            // target, verbatim, so a relative or dangling one survives.
            EntryKind::Symlink => {
                if !self.replaceable(target)? {
                    return Ok(());
                }
                let stored = self.fs.read_link(source).map_err(CpError::Read)?;
                self.fs.symlink(&stored, target).map_err(CpError::Create)?;
                self.report(source, target)
            }
            EntryKind::Directory => self.reproduce_directory(source, target),
        }
    }

    /// Reproduce a directory source under `target` and recurse into it.
    fn reproduce_directory(&mut self, source: &str, target: &str) -> Result<(), CpError> {
        if !self.options.recursive {
            return Err(CpError::IsDirectory);
        }
        match self.stat(target, Follow::Target)?.map(|p| p.kind) {
            Some(EntryKind::Directory) => {}
            Some(EntryKind::File | EntryKind::Symlink) => return Err(CpError::NotADirectory),
            None => {
                self.fs.mkdir(target).map_err(CpError::Create)?;
                self.report(source, target)?;
            }
        }
        for entry in self.read_children(source)? {
            let child_source = join(source, &entry.name);
            let child_target = join(target, &entry.name);
            // A listing describes each child itself, so a child link needs a
            // following probe unless `-P`/`-d` keeps it.
            let child = if entry.probe.kind == EntryKind::Symlink && !self.options.no_dereference {
                self.fs
                    .probe(&child_source, Follow::Target)
                    .map_err(CpError::Stat)?
            } else {
                entry.probe
            };
            self.copy_known(&child_source, child, &child_target)?;
        }
        Ok(())
    }

    /// Put a non-directory source at `target`: its bytes, a second name for
    /// its node (`-l`, or `--preserve=links` meeting the node again), or a
    /// symbolic link naming it (`-s`).
    fn reproduce_file(&mut self, source: &str, probe: &Probe, target: &str) -> Result<(), CpError> {
        // A node already written under another of its names becomes a second
        // name there, whatever the contents mode: `--preserve=links` is about
        // keeping the *sources'* sharing, so it outranks copying the bytes.
        if let Some(first) = self.linked.destination_of(probe) {
            // The borrow of `self.linked` ends with this copy of the name.
            let first = String::from(first);
            if !self.replaceable(target)? {
                return Ok(());
            }
            self.fs.link(&first, target).map_err(CpError::Create)?;
            return self.report(source, target);
        }
        match self.options.contents {
            Contents::Bytes => self.copy_file(source, target)?,
            Contents::HardLink => {
                if !self.replaceable(target)? {
                    return Ok(());
                }
                self.fs.link(source, target).map_err(CpError::Create)?;
                self.report(source, target)?;
            }
            Contents::SymbolicLink => {
                if !self.replaceable(target)? {
                    return Ok(());
                }
                self.fs.symlink(source, target).map_err(CpError::Create)?;
                self.report(source, target)?;
            }
        }
        if self.options.preserve_links {
            self.linked.remember(probe, target);
        }
        Ok(())
    }

    /// Whether the destination may be written: the existing-destination
    /// policy (`-n` skips, `-i` asks), plus the removal `-f` needs before a
    /// *create* that cannot overwrite.
    ///
    /// A link or a second name is created, and a create never replaces a
    /// name, so an occupied destination must be removed first — unlike the
    /// byte copy, which truncates through its own create. Without `-f` the
    /// create fails and the kernel says why.
    fn replaceable(&self, target: &str) -> Result<bool, CpError> {
        if self.stat(target, Follow::Keep)?.is_none() {
            return Ok(true);
        }
        if !self.consented(target)? {
            return Ok(false);
        }
        if self.options.force {
            // A removal error is irrelevant if the create then succeeds, and
            // is reported by the create if it does not.
            let _ = self.fs.remove_file(target);
        }
        Ok(true)
    }

    /// The existing-destination policy for a destination that is already
    /// there: `-n` skips it, `-i` asks, and the default overwrites.
    fn consented(&self, target: &str) -> Result<bool, CpError> {
        match self.options.clobber {
            Clobber::Overwrite => Ok(true),
            Clobber::Skip => Ok(false),
            Clobber::Prompt => self
                .prompt
                .confirm(&format!("overwrite '{target}'?"))
                .map_err(CpError::Prompt),
        }
    }

    /// Stream a regular file from `source` to `target`, honouring the
    /// existing-destination policy and `-f` (remove a destination that
    /// cannot be created and retry the create once).
    fn copy_file(&self, source: &str, target: &str) -> Result<(), CpError> {
        if self.stat(target, Follow::Keep)?.is_some() && !self.consented(target)? {
            return Ok(());
        }
        self.create_destination(target)?;
        let mut offset: u64 = 0;
        let mut buf = [0_u8; READ_CHUNK];
        loop {
            let read = self
                .fs
                .read(source, offset, &mut buf)
                .map_err(CpError::Read)?;
            if read == 0 {
                return self.report(source, target);
            }
            // A seam reporting more than the buffer holds would index out of
            // bounds; refuse it rather than trust the count.
            if read > buf.len() {
                return Err(CpError::Read(tairix_abi::Errno::LengthOutOfRange));
            }
            self.fs
                .write(target, offset, &buf[..read])
                .map_err(CpError::Write)?;
            offset = offset.saturating_add(read as u64);
        }
    }

    /// Create `target`, or — with `-f` — remove it and retry the create once.
    fn create_destination(&self, target: &str) -> Result<(), CpError> {
        match self.fs.create(target) {
            Ok(()) => Ok(()),
            Err(_) if self.options.force => {
                // The destination could not be created (e.g. it exists and is
                // not writable). `-f` removes it and retries exactly once; a
                // removal error is irrelevant if the retried create succeeds.
                let _ = self.fs.remove_file(target);
                self.fs.create(target).map_err(CpError::Create)
            }
            Err(errno) => Err(CpError::Create(errno)),
        }
    }

    /// Read every entry of `path` into a vector, so the directory can be
    /// walked without depending on entry indices staying stable as the copy
    /// proceeds.
    fn read_children(&self, path: &str) -> Result<Vec<Entry>, CpError> {
        let mut entries = Vec::new();
        let mut index: u64 = 0;
        while let Some(entry) = self.fs.read_dir(path, index).map_err(CpError::Read)? {
            index = index.saturating_add(1);
            entries.push(entry);
        }
        Ok(entries)
    }

    /// Inspect `path`, mapping a missing path to [`None`] so a destination
    /// can be probed for existence without treating absence as a failure.
    fn stat(&self, path: &str, follow: Follow) -> Result<Option<Probe>, CpError> {
        match self.fs.probe(path, follow) {
            Ok(probe) => Ok(Some(probe)),
            Err(tairix_abi::Errno::NotFound) => Ok(None),
            Err(errno) => Err(CpError::Stat(errno)),
        }
    }

    /// The follow posture this run's source probes use: `-P`/`-d` describe a
    /// link source itself, everything else describes what it names.
    const fn source_follow(&self) -> Follow {
        if self.options.no_dereference {
            Follow::Keep
        } else {
            Follow::Target
        }
    }

    /// Report one copy under `-v`, in the GNU wording.
    fn report(&self, source: &str, target: &str) -> Result<(), CpError> {
        if !self.options.verbose {
            return Ok(());
        }
        self.out
            .write_all(format!("'{source}' -> '{target}'\n").as_bytes())
            .map_err(CpError::Output)
    }
}
#[cfg(test)]
mod tests {
    use super::{run as engine_run, USAGE};
    use crate::command::{Clobber, Command, Contents, Options, TargetMode};
    use crate::error::CpError;
    use crate::io::{Entry, EntryKind, FileSystem, Follow, Output, Probe, Prompt};
    use alloc::string::{String, ToString};
    use alloc::vec::Vec;
    use core::cell::RefCell;
    use tairix_abi::{Errno, FileId};
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
        /// Symbolic links: `(path, stored target)`, the target verbatim.
        symlinks: Vec<(String, String)>,
        /// Second names: `(path, the path whose node it also names)`, so a
        /// hard-linked pair shares one identity and one set of contents.
        hard: Vec<(String, String)>,
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
                    symlinks: Vec::new(),
                    hard: Vec::new(),
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

        /// A symbolic link at `path` storing `target` verbatim.
        fn symlink_at(self, path: &str, target: &str) -> Self {
            self.state
                .borrow_mut()
                .symlinks
                .push((path.to_string(), target.to_string()));
            self
        }

        /// A second name at `path` for the node `object` already names.
        fn second_name(self, path: &str, object: &str) -> Self {
            self.state
                .borrow_mut()
                .hard
                .push((path.to_string(), object.to_string()));
            self
        }

        fn target_of(&self, path: &str) -> Option<String> {
            self.state
                .borrow()
                .symlinks
                .iter()
                .find(|(p, _)| p == path)
                .map(|(_, target)| target.clone())
        }

        fn names_of(&self, object: &str) -> usize {
            1 + self
                .state
                .borrow()
                .hard
                .iter()
                .filter(|(_, o)| o == object)
                .count()
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

    impl MemFs {
        /// The path whose file object `path` names: itself, or the object a
        /// second name shares. A symbolic link is **not** followed — the
        /// posture `link` and a `Follow::Keep` probe need.
        fn object_of(&self, path: &str) -> Option<String> {
            let state = self.state.borrow();
            if state.files.iter().any(|(p, _)| p == path) {
                return Some(path.to_string());
            }
            state
                .hard
                .iter()
                .find(|(p, _)| p == path)
                .map(|(_, object)| object.clone())
        }

        /// As [`MemFs::object_of`], but resolving a final symbolic link, the
        /// way a read through a following descriptor reaches its target.
        /// Bounded, so a cycle in the fixture cannot spin.
        fn followed_object_of(&self, path: &str) -> Option<String> {
            let mut name = path.to_string();
            for _ in 0..8 {
                if let Some(object) = self.object_of(&name) {
                    return Some(object);
                }
                name = self.target_of(&name)?;
            }
            None
        }

        /// The identity and name count of the object at `object`: a distinct
        /// non-zero node number per object, plus one name per second name.
        fn identity(&self, object: &str) -> (FileId, u32) {
            let index = self
                .state
                .borrow()
                .files
                .iter()
                .position(|(p, _)| p == object)
                .expect("the object is in the fixture");
            (
                FileId {
                    volume: [2u8; 16],
                    node: u64::try_from(index).expect("fixture index") + 1,
                },
                u32::try_from(self.names_of(object)).expect("fixture names"),
            )
        }
    }

    impl FileSystem for MemFs {
        fn probe(&self, path: &str, follow: Follow) -> Result<Probe, Errno> {
            let path = canon(path);
            if let Some(target) = self.target_of(path) {
                return match follow {
                    // A link describes itself: no second name can exist for
                    // one in this fixture, so one name is the whole truth.
                    Follow::Keep => Ok(Probe {
                        kind: EntryKind::Symlink,
                        id: FileId::NONE,
                        nlink: 1,
                    }),
                    // Following resolves to what the link names; a dangling
                    // one is the kernel's `NotFound`.
                    Follow::Target => self.probe(&target, follow),
                };
            }
            if self.state.borrow().dirs.iter().any(|p| p == path) {
                return Ok(Probe {
                    kind: EntryKind::Directory,
                    id: FileId::NONE,
                    nlink: 2,
                });
            }
            let object = self.object_of(path).ok_or(Errno::NotFound)?;
            let (id, nlink) = self.identity(&object);
            Ok(Probe {
                kind: EntryKind::File,
                id,
                nlink,
            })
        }

        fn read_link(&self, path: &str) -> Result<String, Errno> {
            self.target_of(canon(path)).ok_or(Errno::OutOfRange)
        }

        fn symlink(&self, target: &str, link: &str) -> Result<(), Errno> {
            let link = canon(link).to_string();
            let mut state = self.state.borrow_mut();
            if state.symlinks.iter().any(|(p, _)| *p == link)
                || state.files.iter().any(|(p, _)| *p == link)
                || state.dirs.contains(&link)
            {
                return Err(Errno::AlreadyExists);
            }
            state.symlinks.push((link, target.to_string()));
            Ok(())
        }

        fn link(&self, existing: &str, new: &str) -> Result<(), Errno> {
            let object = self.object_of(canon(existing)).ok_or(Errno::NotFound)?;
            let new = canon(new).to_string();
            let mut state = self.state.borrow_mut();
            if state.files.iter().any(|(p, _)| *p == new)
                || state.hard.iter().any(|(p, _)| *p == new)
                || state.symlinks.iter().any(|(p, _)| *p == new)
            {
                return Err(Errno::AlreadyExists);
            }
            state.hard.push((new, object));
            Ok(())
        }

        fn read(&self, path: &str, offset: u64, buf: &mut [u8]) -> Result<usize, Errno> {
            let named = canon(path);
            let object = self.followed_object_of(named).ok_or(Errno::NotFound)?;
            let path = object.as_str();
            let state = self.state.borrow();
            if let Some((p, errno)) = &state.read_fail {
                if p == named {
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
            let mut names = Vec::new();
            for (p, _) in &state.files {
                if parent_of(p) == path {
                    names.push((p.clone(), EntryKind::File));
                }
            }
            for (p, _) in &state.hard {
                if parent_of(p) == path {
                    names.push((p.clone(), EntryKind::File));
                }
            }
            for (p, _) in &state.symlinks {
                if parent_of(p) == path {
                    names.push((p.clone(), EntryKind::Symlink));
                }
            }
            for p in &state.dirs {
                if parent_of(p) == path {
                    names.push((p.clone(), EntryKind::Directory));
                }
            }
            drop(state);
            // A listing describes each child itself, carrying the identity
            // and name count the real `fs_readdir` record now reports.
            let mut entries = Vec::new();
            for (p, kind) in names {
                let (id, nlink) = match kind {
                    EntryKind::File => self
                        .object_of(&p)
                        .map_or((FileId::NONE, 1), |object| self.identity(&object)),
                    EntryKind::Directory => (FileId::NONE, 2),
                    EntryKind::Symlink => (FileId::NONE, 1),
                };
                entries.push(Entry {
                    name: name_of(&p).to_string(),
                    probe: Probe { kind, id, nlink },
                });
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

    /// Assertions about the fixture state a link-making run produced.
    impl MemFs {
        fn is_second_name_of(&self, path: &str, object: &str) -> bool {
            self.state
                .borrow()
                .hard
                .iter()
                .any(|(p, o)| p == path && o == object)
        }
    }

    // --- -l: a second name instead of a copy ------------------------------

    #[test]
    fn link_gives_the_destination_a_second_name_rather_than_copying() {
        // The point of `-l`: no second copy of the bytes exists, so the
        // destination cannot diverge from the source on the next write.
        let fs = MemFs::new().file("/a.txt", b"hello");
        let out = Recorder::new();
        assert_eq!(
            run(
                copy_with(
                    Options {
                        contents: Contents::HardLink,
                        ..Options::DEFAULT
                    },
                    &["/a.txt"],
                    "/b.txt"
                ),
                &fs,
                &out
            ),
            Ok(())
        );
        assert!(fs.is_second_name_of("/b.txt", "/a.txt"));
        assert!(
            fs.contents("/b.txt").is_none(),
            "no second copy of the bytes"
        );
        assert_eq!(fs.names_of("/a.txt"), 2);
    }

    #[test]
    fn link_onto_a_taken_name_is_refused_without_force() {
        // A create never replaces a name, so the kernel's `AlreadyExists`
        // reaches the caller rather than a silent overwrite.
        let fs = MemFs::new().file("/a.txt", b"hello").file("/b.txt", b"old");
        let out = Recorder::new();
        assert_eq!(
            run(
                copy_with(
                    Options {
                        contents: Contents::HardLink,
                        ..Options::DEFAULT
                    },
                    &["/a.txt"],
                    "/b.txt"
                ),
                &fs,
                &out
            ),
            Err(CpError::Create(Errno::AlreadyExists))
        );
        assert_eq!(fs.contents("/b.txt"), Some(b"old".to_vec()));
    }

    #[test]
    fn force_removes_the_taken_name_before_linking() {
        let fs = MemFs::new().file("/a.txt", b"hello").file("/b.txt", b"old");
        let out = Recorder::new();
        assert_eq!(
            run(
                copy_with(
                    Options {
                        contents: Contents::HardLink,
                        force: true,
                        ..Options::DEFAULT
                    },
                    &["/a.txt"],
                    "/b.txt"
                ),
                &fs,
                &out
            ),
            Ok(())
        );
        assert_eq!(fs.removed(), Vec::from(["/b.txt".to_string()]));
        assert!(fs.is_second_name_of("/b.txt", "/a.txt"));
    }

    // --- -s: a symbolic link naming the source ---------------------------

    #[test]
    fn symbolic_link_names_the_source_rather_than_copying_it() {
        let fs = MemFs::new().file("/a.txt", b"hello");
        let out = Recorder::new();
        assert_eq!(
            run(
                copy_with(
                    Options {
                        contents: Contents::SymbolicLink,
                        ..Options::DEFAULT
                    },
                    &["/a.txt"],
                    "/b.txt"
                ),
                &fs,
                &out
            ),
            Ok(())
        );
        assert_eq!(fs.target_of("/b.txt").as_deref(), Some("/a.txt"));
        assert!(fs.contents("/b.txt").is_none());
    }

    // --- -P/-d: reproduce a link rather than following it ----------------

    #[test]
    fn without_no_dereference_a_link_source_is_followed_and_its_target_copied() {
        // The default: a copy of a link to a file is a copy of the file.
        let fs = MemFs::new()
            .file("/a.txt", b"hello")
            .symlink_at("/alias", "/a.txt");
        let out = Recorder::new();
        assert_eq!(
            run(copy(false, false, &["/alias"], "/b.txt"), &fs, &out),
            Ok(())
        );
        assert_eq!(fs.contents("/b.txt"), Some(b"hello".to_vec()));
        assert!(fs.target_of("/b.txt").is_none(), "not reproduced as a link");
    }

    #[test]
    fn no_dereference_reproduces_the_link_with_the_same_stored_target() {
        // Verbatim: a relative target stays relative rather than being
        // resolved against the source's directory.
        let fs = MemFs::new()
            .file("/a.txt", b"hello")
            .symlink_at("/alias", "../elsewhere/a.txt");
        let out = Recorder::new();
        assert_eq!(
            run(
                copy_with(
                    Options {
                        no_dereference: true,
                        ..Options::DEFAULT
                    },
                    &["/alias"],
                    "/b"
                ),
                &fs,
                &out
            ),
            Ok(())
        );
        assert_eq!(fs.target_of("/b").as_deref(), Some("../elsewhere/a.txt"));
        assert!(fs.contents("/b").is_none());
    }

    #[test]
    fn no_dereference_reproduces_a_dangling_link_too() {
        // A link's target is data, so a link naming nothing is copyable —
        // following it would be `NotFound`.
        let fs = MemFs::new().symlink_at("/alias", "nowhere");
        let out = Recorder::new();
        assert_eq!(
            run(
                copy_with(
                    Options {
                        no_dereference: true,
                        ..Options::DEFAULT
                    },
                    &["/alias"],
                    "/b"
                ),
                &fs,
                &out
            ),
            Ok(())
        );
        assert_eq!(fs.target_of("/b").as_deref(), Some("nowhere"));
        // And the default posture cannot copy it at all.
        let fs = MemFs::new().symlink_at("/alias", "nowhere");
        assert_eq!(
            run(copy(false, false, &["/alias"], "/c"), &fs, &Recorder::new()),
            Err(CpError::Stat(Errno::NotFound))
        );
    }

    #[test]
    fn a_recursive_copy_reproduces_interior_links_under_no_dereference() {
        let fs = MemFs::new()
            .dir("/src")
            .file("/src/real", b"bytes")
            .symlink_at("/src/alias", "real");
        let out = Recorder::new();
        assert_eq!(
            run(
                copy_with(
                    Options {
                        recursive: true,
                        no_dereference: true,
                        ..Options::DEFAULT
                    },
                    &["/src"],
                    "/dst"
                ),
                &fs,
                &out
            ),
            Ok(())
        );
        assert_eq!(fs.contents("/dst/real"), Some(b"bytes".to_vec()));
        assert_eq!(fs.target_of("/dst/alias").as_deref(), Some("real"));
    }

    #[test]
    fn a_recursive_copy_follows_interior_links_by_default() {
        let fs = MemFs::new()
            .dir("/src")
            .file("/src/real", b"bytes")
            .symlink_at("/src/alias", "/src/real");
        let out = Recorder::new();
        assert_eq!(run(copy(true, false, &["/src"], "/dst"), &fs, &out), Ok(()));
        assert_eq!(fs.contents("/dst/alias"), Some(b"bytes".to_vec()));
        assert!(fs.target_of("/dst/alias").is_none());
    }

    // --- --preserve=links: keep the sources' sharing ----------------------

    #[test]
    fn preserve_links_gives_a_second_source_naming_one_node_a_second_name() {
        // Two sources, one node: the destinations share a node too, so the
        // copy does not silently double the storage.
        let fs = MemFs::new()
            .dir("/dst")
            .file("/one", b"shared")
            .second_name("/two", "/one");
        let out = Recorder::new();
        assert_eq!(
            run(
                copy_with(
                    Options {
                        preserve_links: true,
                        ..Options::DEFAULT
                    },
                    &["/one", "/two"],
                    "/dst"
                ),
                &fs,
                &out
            ),
            Ok(())
        );
        assert_eq!(fs.contents("/dst/one"), Some(b"shared".to_vec()));
        assert!(fs.is_second_name_of("/dst/two", "/dst/one"));
        assert!(
            fs.contents("/dst/two").is_none(),
            "the second name is not a second copy"
        );
    }

    #[test]
    fn without_preserve_links_each_name_is_copied_separately() {
        let fs = MemFs::new()
            .dir("/dst")
            .file("/one", b"shared")
            .second_name("/two", "/one");
        let out = Recorder::new();
        assert_eq!(
            run(copy(false, false, &["/one", "/two"], "/dst"), &fs, &out),
            Ok(())
        );
        assert_eq!(fs.contents("/dst/one"), Some(b"shared".to_vec()));
        assert_eq!(fs.contents("/dst/two"), Some(b"shared".to_vec()));
        assert!(!fs.is_second_name_of("/dst/two", "/dst/one"));
    }

    #[test]
    fn preserve_links_leaves_singly_named_sources_alone() {
        // A node named once cannot be met twice, so nothing is remembered
        // for it and two distinct files stay two files.
        let fs = MemFs::new()
            .dir("/dst")
            .file("/one", b"a")
            .file("/two", b"b");
        let out = Recorder::new();
        assert_eq!(
            run(
                copy_with(
                    Options {
                        preserve_links: true,
                        ..Options::DEFAULT
                    },
                    &["/one", "/two"],
                    "/dst"
                ),
                &fs,
                &out
            ),
            Ok(())
        );
        assert_eq!(fs.contents("/dst/one"), Some(b"a".to_vec()));
        assert_eq!(fs.contents("/dst/two"), Some(b"b".to_vec()));
        assert!(!fs.is_second_name_of("/dst/two", "/dst/one"));
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
    fn an_alias_root_source_lands_under_its_own_root_spelling() {
        // Behaviour moved with the sweep onto the shared `leaf_name`: the
        // private base name this tool carried was not alias-aware and named
        // `Home:/` as `Home:`, dropping the separator that makes it a root.
        assert_eq!(tairix_path::leaf_name("Home:/"), "Home:/");
        // And an ordinary path is unchanged by the move.
        assert_eq!(tairix_path::leaf_name("/a/b/"), "b");
    }

    #[test]
    fn an_empty_source_operand_names_nothing_and_fails_closed() {
        // The other moved answer: the private base name reported `/` for the
        // empty spelling, fabricating a root component the caller never
        // typed. `leaf_name` names nothing, and the copy still fails on the
        // unreadable source rather than acting on a guessed path.
        assert_eq!(tairix_path::leaf_name(""), "");
        let fs = MemFs::new().dir("/dst");
        let out = Recorder::new();
        assert!(run(copy(false, false, &[""], "/dst"), &fs, &out).is_err());
        assert!(!fs.has_dir("/dst/"));
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
