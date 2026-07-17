//! The removal engine: inspect each operand, descend each directory `-r`
//! must remove, and unlink every reachable object — depth-first, contents
//! before the directory that holds them.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use tairix_help::{own_short_help, HelpSource};

use crate::command::{Command, Interactive, Options};
use crate::error::RmError;
use crate::io::{Entry, EntryKind, Output, Prompt, Removal};

/// The usage banner a usage error is reported with, and the fallback the
/// short-help switches print when `rm`'s own Help tree is unavailable.
pub const USAGE: &str = "\
usage: rm [-dfiIrRv] [--] file...

  -r, -R, --recursive  remove directories and their contents
  -f, --force          ignore operands that do not exist; never prompt
  -d, --dir            remove empty directories
  -i, --interactive    prompt before every removal
  -I                   prompt once before removing more than three
                       operands, or before a recursive removal
  -v, --verbose        report each removal
  --preserve-root      refuse to remove '/' (the default)
  --no-preserve-root   allow removing '/'
  -h, -?, --help       show this message

At least one file operand is required unless -f is given. `--` ends option
parsing: every later argument is a path.
";

/// `rm`'s own command word: the short-help switches render its own Help
/// document through the same engine as any other command's.
const OWN_WORD: &str = "rm";

/// Run one [`Command`], removing its operands through `fs`. `locale` is the
/// user's `LANG` preference, if set; `help` is the tool's own `Help/` tree,
/// read by the short-help switches.
///
/// Operands are removed in order. A non-directory operand is unlinked; a
/// directory operand is removed only with `-r` (which removes its contents
/// depth-first and then the directory itself) or, when empty, with `-d`.
/// With `-f` an operand that does not exist is skipped silently. `-i` asks
/// through `prompt` before every removal and before descending into a
/// directory; `-I` asks once up front for a large or recursive removal; a
/// declined question skips the object (or the whole run for `-I`) without
/// error. `-v` reports each removal on `out`; otherwise `rm` writes nothing
/// on success beyond the [`Command::Help`] banner. `--preserve-root` (the
/// default) refuses the operand `/` outright.
///
/// The first failure stops the run before any later operand (fail closed).
///
/// # Errors
///
/// * [`RmError::IsDirectory`] — a directory was named without `-r` (or
///   `-d`).
/// * [`RmError::PreserveRoot`] — the operand `/` under `--preserve-root`.
/// * [`RmError::Stat`] — an operand could not be inspected; carries the
///   underlying [`Errno`](tairix_abi::Errno) (suppressed for
///   [`NotFound`](tairix_abi::Errno::NotFound) when `-f` is set).
/// * [`RmError::Read`] — a directory's entries could not be read.
/// * [`RmError::Remove`] — unlinking a file or directory failed.
/// * [`RmError::Prompt`] — a confirmation could not be read (never treated
///   as consent).
/// * [`RmError::Output`] — writing the usage banner or a `-v` report
///   failed.
pub fn run(
    command: Command,
    locale: Option<&str>,
    fs: &dyn Removal,
    prompt: &dyn Prompt,
    help: &dyn HelpSource,
    out: &dyn Output,
) -> Result<(), RmError> {
    match command {
        Command::Help => short_help(locale, help, out),
        Command::Remove { options, paths } => {
            if options.interactive == Interactive::Once && (paths.len() > 3 || options.recursive) {
                let plural = if paths.len() == 1 { "" } else { "s" };
                let recursively = if options.recursive {
                    " recursively"
                } else {
                    ""
                };
                let question = format!("remove {} argument{plural}{recursively}?", paths.len());
                if !prompt.confirm(&question).map_err(RmError::Prompt)? {
                    return Ok(());
                }
            }
            for path in &paths {
                remove_operand(path, options, fs, prompt, out)?;
            }
            Ok(())
        }
    }
}

/// Render `rm`'s own short help (`NAME` + `SYNOPSIS` + compact `OPTIONS`)
/// from its own Help tree through the one shared engine; when no document
/// can be served (a build without the bundle's documents) the usage banner
/// stands in — the tool's own text, not fabricated help content — so `-h`
/// never fails.
fn short_help(
    locale: Option<&str>,
    help: &dyn HelpSource,
    out: &dyn Output,
) -> Result<(), RmError> {
    let bytes =
        own_short_help(help, locale, OWN_WORD).unwrap_or_else(|| String::from(USAGE).into_bytes());
    out.write_all(&bytes).map_err(RmError::Output)
}

/// Remove one named operand, honouring `-f` for a missing path and the
/// `--preserve-root` guard.
fn remove_operand(
    path: &str,
    options: Options,
    fs: &dyn Removal,
    prompt: &dyn Prompt,
    out: &dyn Output,
) -> Result<(), RmError> {
    if options.preserve_root && path == "/" {
        return Err(RmError::PreserveRoot);
    }
    let kind = match fs.kind(path) {
        Ok(kind) => kind,
        Err(tairix_abi::Errno::NotFound) if options.force => return Ok(()),
        Err(errno) => return Err(RmError::Stat(errno)),
    };
    remove_known(path, kind, options, fs, prompt, out)
}

/// Ask a per-object `-i` question; `Ok(true)` means proceed.
fn confirmed(options: Options, prompt: &dyn Prompt, question: &str) -> Result<bool, RmError> {
    if options.interactive != Interactive::Always {
        return Ok(true);
    }
    prompt.confirm(question).map_err(RmError::Prompt)
}

/// Report one removal under `-v`, in the GNU wording.
fn report(options: Options, out: &dyn Output, path: &str, directory: bool) -> Result<(), RmError> {
    if !options.verbose {
        return Ok(());
    }
    let line = if directory {
        format!("removed directory '{path}'\n")
    } else {
        format!("removed '{path}'\n")
    };
    out.write_all(line.as_bytes()).map_err(RmError::Output)
}

/// Remove an object whose [`EntryKind`] is already known (from the parent's
/// directory entry, or from the operand's stat), recursing into directories.
fn remove_known(
    path: &str,
    kind: EntryKind,
    options: Options,
    fs: &dyn Removal,
    prompt: &dyn Prompt,
    out: &dyn Output,
) -> Result<(), RmError> {
    match kind {
        EntryKind::Other => {
            if !confirmed(options, prompt, &format!("remove file '{path}'?"))? {
                return Ok(());
            }
            fs.remove_file(path).map_err(RmError::Remove)?;
            report(options, out, path, false)
        }
        EntryKind::Directory => {
            if !options.recursive {
                if !options.dir {
                    return Err(RmError::IsDirectory);
                }
                // `-d`: remove the (empty) directory itself; a non-empty
                // one surfaces the filesystem's own refusal.
                if !confirmed(options, prompt, &format!("remove directory '{path}'?"))? {
                    return Ok(());
                }
                fs.remove_dir(path).map_err(RmError::Remove)?;
                return report(options, out, path, true);
            }
            if !confirmed(
                options,
                prompt,
                &format!("descend into directory '{path}'?"),
            )? {
                return Ok(());
            }
            for entry in read_children(path, fs)? {
                let child = join(path, &entry.name);
                remove_known(&child, entry.kind, options, fs, prompt, out)?;
            }
            if !confirmed(options, prompt, &format!("remove directory '{path}'?"))? {
                return Ok(());
            }
            fs.remove_dir(path).map_err(RmError::Remove)?;
            report(options, out, path, true)
        }
    }
}

/// Read every entry of `path` into a vector, so the directory can be walked
/// without depending on entry indices staying stable as removals proceed.
fn read_children(path: &str, fs: &dyn Removal) -> Result<Vec<Entry>, RmError> {
    let mut entries = Vec::new();
    let mut index: u64 = 0;
    while let Some(entry) = fs.read_dir(path, index).map_err(RmError::Read)? {
        index = index.saturating_add(1);
        entries.push(entry);
    }
    Ok(entries)
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
    use crate::command::{Command, Interactive, Options};
    use crate::error::RmError;
    use crate::io::{Entry, EntryKind, Output, Prompt, Removal};
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

    /// A Help tree holding one canonical `rm.md` document.
    struct OneDoc;

    const DOC: &str = "## NAME\n\nrm — remove files and directories\n\n\
                       ## SYNOPSIS\n\n`rm [-r] [--] file...`\n\n\
                       ## DESCRIPTION\n\nRemoves things.\n";

    impl HelpSource for OneDoc {
        fn locale_dirs(&self) -> Result<Vec<String>, SourceError> {
            Ok(alloc::vec![String::from("en-US")])
        }

        fn read(&self, locale_dir: &str, file_name: &str) -> Result<Option<Vec<u8>>, SourceError> {
            if locale_dir == "en-US" && file_name == "rm.md" {
                Ok(Some(DOC.as_bytes().to_vec()))
            } else {
                Ok(None)
            }
        }
    }

    /// The engine under a prompt that must never be reached — the shape
    /// every pre-existing non-interactive test uses.
    fn run(command: Command, fs: &dyn Removal, out: &Recorder) -> Result<(), RmError> {
        engine_run(command, None, fs, &NeverAsked, &NoHelp, out)
    }

    /// An in-memory tree: a kind table keyed by path plus, for directories,
    /// the entries that path's `read_dir` returns. Removals are recorded in
    /// call order; an optional `(path, errno)` makes one removal fail.
    struct TreeFs {
        kinds: Vec<(String, EntryKind)>,
        children: Vec<(String, Vec<Entry>)>,
        removed: RefCell<Vec<String>>,
        fail: Option<(String, Errno)>,
    }

    impl TreeFs {
        fn new() -> Self {
            Self {
                kinds: Vec::new(),
                children: Vec::new(),
                removed: RefCell::new(Vec::new()),
                fail: None,
            }
        }

        fn file(mut self, path: &str) -> Self {
            self.kinds.push((path.to_string(), EntryKind::Other));
            self
        }

        fn dir(mut self, path: &str) -> Self {
            self.kinds.push((path.to_string(), EntryKind::Directory));
            self.children.push((path.to_string(), Vec::new()));
            self
        }

        fn child(mut self, dir: &str, name: &str, kind: EntryKind) -> Self {
            let entries = self
                .children
                .iter_mut()
                .find(|(d, _)| d == dir)
                .map(|(_, c)| c)
                .expect("directory must be declared before its entries");
            entries.push(Entry {
                name: name.to_string(),
                kind,
            });
            self
        }

        fn failing(mut self, path: &str, errno: Errno) -> Self {
            self.fail = Some((path.to_string(), errno));
            self
        }

        fn record(&self, path: &str) -> Result<(), Errno> {
            if let Some((target, errno)) = &self.fail {
                if target == path {
                    return Err(*errno);
                }
            }
            self.removed.borrow_mut().push(path.to_string());
            Ok(())
        }

        fn removed(&self) -> Vec<String> {
            self.removed.borrow().clone()
        }
    }

    impl Removal for TreeFs {
        fn kind(&self, path: &str) -> Result<EntryKind, Errno> {
            self.kinds
                .iter()
                .find(|(p, _)| p == path)
                .map(|(_, k)| *k)
                .ok_or(Errno::NotFound)
        }

        fn read_dir(&self, path: &str, index: u64) -> Result<Option<Entry>, Errno> {
            let entries = self
                .children
                .iter()
                .find(|(d, _)| d == path)
                .map(|(_, c)| c)
                .ok_or(Errno::NotFound)?;
            let idx = usize::try_from(index).map_err(|_| Errno::LengthOutOfRange)?;
            Ok(entries.get(idx).cloned())
        }

        fn remove_file(&self, path: &str) -> Result<(), Errno> {
            self.record(path)
        }

        fn remove_dir(&self, path: &str) -> Result<(), Errno> {
            self.record(path)
        }
    }

    /// A directory whose `read_dir` always fails — to exercise the read
    /// fail-closed path.
    struct FailingDir;

    impl Removal for FailingDir {
        fn kind(&self, _path: &str) -> Result<EntryKind, Errno> {
            Ok(EntryKind::Directory)
        }

        fn read_dir(&self, _path: &str, _index: u64) -> Result<Option<Entry>, Errno> {
            Err(Errno::PermissionDenied)
        }

        fn remove_file(&self, _path: &str) -> Result<(), Errno> {
            Ok(())
        }

        fn remove_dir(&self, _path: &str) -> Result<(), Errno> {
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

    fn remove_with(options: Options, paths: &[&str]) -> Command {
        Command::Remove {
            options,
            paths: paths.iter().map(|p| (*p).to_string()).collect::<Vec<_>>(),
        }
    }

    fn remove(recursive: bool, force: bool, paths: &[&str]) -> Command {
        remove_with(
            Options {
                recursive,
                force,
                ..Options::DEFAULT
            },
            paths,
        )
    }

    #[test]
    fn help_writes_usage() {
        let fs = TreeFs::new();
        let out = Recorder::new();
        assert_eq!(run(Command::Help, &fs, &out), Ok(()));
        assert_eq!(out.text(), USAGE);
    }

    #[test]
    fn help_renders_the_short_help_from_the_document() {
        let fs = TreeFs::new();
        let out = Recorder::new();
        assert_eq!(
            engine_run(Command::Help, None, &fs, &NeverAsked, &OneDoc, &out),
            Ok(())
        );
        let text = out.text();
        assert!(text.contains("rm — remove files and directories"), "{text}");
        assert!(text.contains("rm [-r] [--] file..."), "{text}");
    }

    #[test]
    fn removes_a_single_file() {
        let fs = TreeFs::new().file("/a.txt");
        let out = Recorder::new();
        assert_eq!(run(remove(false, false, &["/a.txt"]), &fs, &out), Ok(()));
        assert_eq!(fs.removed(), ["/a.txt"]);
        // `rm` is silent on success.
        assert_eq!(out.text(), "");
    }

    #[test]
    fn removes_several_files_in_order() {
        let fs = TreeFs::new().file("/a").file("/b").file("/c");
        let out = Recorder::new();
        assert_eq!(
            run(remove(false, false, &["/a", "/b", "/c"]), &fs, &out),
            Ok(())
        );
        assert_eq!(fs.removed(), ["/a", "/b", "/c"]);
    }

    #[test]
    fn directory_without_recursive_fails_closed() {
        let fs = TreeFs::new().dir("/d");
        let out = Recorder::new();
        assert_eq!(
            run(remove(false, false, &["/d"]), &fs, &out),
            Err(RmError::IsDirectory)
        );
        // Nothing was removed.
        assert!(fs.removed().is_empty());
    }

    #[test]
    fn recursive_removes_contents_before_the_directory() {
        // /d holds a file and a nested directory with its own file.
        let fs = TreeFs::new()
            .dir("/d")
            .child("/d", "f", EntryKind::Other)
            .child("/d", "sub", EntryKind::Directory)
            .dir("/d/sub")
            .child("/d/sub", "g", EntryKind::Other);
        let out = Recorder::new();
        assert_eq!(run(remove(true, false, &["/d"]), &fs, &out), Ok(()));
        // Depth-first: the file, then the nested file, then the nested dir,
        // then the top dir last. Every parent is removed after its contents.
        assert_eq!(fs.removed(), ["/d/f", "/d/sub/g", "/d/sub", "/d"]);
    }

    #[test]
    fn recursive_removes_an_empty_directory() {
        let fs = TreeFs::new().dir("/empty");
        let out = Recorder::new();
        assert_eq!(run(remove(true, false, &["/empty"]), &fs, &out), Ok(()));
        assert_eq!(fs.removed(), ["/empty"]);
    }

    #[test]
    fn missing_operand_fails_closed_without_force() {
        let fs = TreeFs::new();
        let out = Recorder::new();
        assert_eq!(
            run(remove(false, false, &["/absent"]), &fs, &out),
            Err(RmError::Stat(Errno::NotFound))
        );
    }

    #[test]
    fn force_skips_a_missing_operand() {
        let fs = TreeFs::new().file("/present");
        let out = Recorder::new();
        // The missing operand is skipped; the present one is still removed.
        assert_eq!(
            run(remove(false, true, &["/absent", "/present"]), &fs, &out),
            Ok(())
        );
        assert_eq!(fs.removed(), ["/present"]);
    }

    #[test]
    fn force_does_not_mask_a_permission_error() {
        // `-f` suppresses only NotFound; a permission denial still surfaces.
        struct Denied;
        impl Removal for Denied {
            fn kind(&self, _path: &str) -> Result<EntryKind, Errno> {
                Err(Errno::PermissionDenied)
            }
            fn read_dir(&self, _path: &str, _index: u64) -> Result<Option<Entry>, Errno> {
                Ok(None)
            }
            fn remove_file(&self, _path: &str) -> Result<(), Errno> {
                Ok(())
            }
            fn remove_dir(&self, _path: &str) -> Result<(), Errno> {
                Ok(())
            }
        }
        let out = Recorder::new();
        assert_eq!(
            run(remove(false, true, &["/x"]), &Denied, &out),
            Err(RmError::Stat(Errno::PermissionDenied))
        );
    }

    #[test]
    fn a_failure_stops_before_a_later_operand() {
        let fs = TreeFs::new().file("/a").file("/b");
        let out = Recorder::new();
        // The first operand is missing, so the second is never touched.
        assert_eq!(
            run(remove(false, false, &["/absent", "/a"]), &fs, &out),
            Err(RmError::Stat(Errno::NotFound))
        );
        assert!(fs.removed().is_empty());
    }

    #[test]
    fn an_unreadable_directory_fails_closed() {
        let out = Recorder::new();
        assert_eq!(
            run(remove(true, false, &["/d"]), &FailingDir, &out),
            Err(RmError::Read(Errno::PermissionDenied))
        );
    }

    #[test]
    fn a_failed_unlink_surfaces_the_errno() {
        let fs = TreeFs::new()
            .file("/a")
            .failing("/a", Errno::PermissionDenied);
        let out = Recorder::new();
        assert_eq!(
            run(remove(false, false, &["/a"]), &fs, &out),
            Err(RmError::Remove(Errno::PermissionDenied))
        );
    }

    #[test]
    fn a_failed_directory_unlink_stops_the_recursion() {
        // The child unlinks, then removing the directory itself fails.
        let fs = TreeFs::new()
            .dir("/d")
            .child("/d", "f", EntryKind::Other)
            .failing("/d", Errno::PermissionDenied);
        let out = Recorder::new();
        assert_eq!(
            run(remove(true, false, &["/d"]), &fs, &out),
            Err(RmError::Remove(Errno::PermissionDenied))
        );
        // The child was removed before the directory's removal failed.
        assert_eq!(fs.removed(), ["/d/f"]);
    }

    #[test]
    fn empty_force_run_removes_nothing() {
        let fs = TreeFs::new();
        let out = Recorder::new();
        assert_eq!(run(remove(false, true, &[]), &fs, &out), Ok(()));
        assert!(fs.removed().is_empty());
    }

    #[test]
    fn help_output_failure_propagates() {
        let fs = TreeFs::new();
        let out = Recorder::failing();
        assert_eq!(
            run(Command::Help, &fs, &out),
            Err(RmError::Output(Errno::NotFound))
        );
    }

    #[test]
    fn a_directory_under_a_trailing_slash_parent_joins_cleanly() {
        // A mount-root style parent ending in `/` must not double the
        // slash; removing `/` itself requires --no-preserve-root.
        let fs = TreeFs::new().dir("/").child("/", "f", EntryKind::Other);
        let out = Recorder::new();
        assert_eq!(
            run(
                remove_with(
                    Options {
                        recursive: true,
                        preserve_root: false,
                        ..Options::DEFAULT
                    },
                    &["/"],
                ),
                &fs,
                &out,
            ),
            Ok(())
        );
        assert_eq!(fs.removed(), ["/f", "/"]);
    }

    #[test]
    fn preserve_root_refuses_the_root_operand() {
        let fs = TreeFs::new().dir("/").child("/", "f", EntryKind::Other);
        let out = Recorder::new();
        assert_eq!(
            run(remove(true, false, &["/"]), &fs, &out),
            Err(RmError::PreserveRoot)
        );
        assert!(fs.removed().is_empty());
    }

    #[test]
    fn dir_option_removes_an_empty_directory_without_recursive() {
        let fs = TreeFs::new().dir("/empty");
        let out = Recorder::new();
        assert_eq!(
            run(
                remove_with(
                    Options {
                        dir: true,
                        ..Options::DEFAULT
                    },
                    &["/empty"],
                ),
                &fs,
                &out,
            ),
            Ok(())
        );
        assert_eq!(fs.removed(), ["/empty"]);
    }

    #[test]
    fn dir_option_surfaces_the_filesystems_refusal_of_a_full_directory() {
        let fs = TreeFs::new()
            .dir("/full")
            .child("/full", "f", EntryKind::Other)
            .failing("/full", Errno::PermissionDenied);
        let out = Recorder::new();
        assert_eq!(
            run(
                remove_with(
                    Options {
                        dir: true,
                        ..Options::DEFAULT
                    },
                    &["/full"],
                ),
                &fs,
                &out,
            ),
            Err(RmError::Remove(Errno::PermissionDenied))
        );
    }

    #[test]
    fn verbose_reports_each_removal_in_gnu_wording() {
        let fs = TreeFs::new().dir("/d").child("/d", "f", EntryKind::Other);
        let out = Recorder::new();
        assert_eq!(
            run(
                remove_with(
                    Options {
                        recursive: true,
                        verbose: true,
                        ..Options::DEFAULT
                    },
                    &["/d"],
                ),
                &fs,
                &out,
            ),
            Ok(())
        );
        assert_eq!(out.text(), "removed '/d/f'\nremoved directory '/d'\n");
    }

    #[test]
    fn interactive_always_asks_before_each_removal() {
        let fs = TreeFs::new().file("/a").file("/b");
        let out = Recorder::new();
        let prompt = Answers::new(&[true, false]);
        // `/a` is confirmed and removed; `/b` is declined and skipped, and
        // the run still succeeds.
        assert_eq!(
            engine_run(
                remove_with(
                    Options {
                        interactive: Interactive::Always,
                        ..Options::DEFAULT
                    },
                    &["/a", "/b"],
                ),
                None,
                &fs,
                &prompt,
                &NoHelp,
                &out,
            ),
            Ok(())
        );
        assert_eq!(fs.removed(), ["/a"]);
        assert_eq!(prompt.asked(), ["remove file '/a'?", "remove file '/b'?"]);
    }

    #[test]
    fn interactive_always_declining_the_descent_skips_the_directory() {
        let fs = TreeFs::new().dir("/d").child("/d", "f", EntryKind::Other);
        let out = Recorder::new();
        let prompt = Answers::new(&[false]);
        assert_eq!(
            engine_run(
                remove_with(
                    Options {
                        recursive: true,
                        interactive: Interactive::Always,
                        ..Options::DEFAULT
                    },
                    &["/d"],
                ),
                None,
                &fs,
                &prompt,
                &NoHelp,
                &out,
            ),
            Ok(())
        );
        assert!(fs.removed().is_empty());
        assert_eq!(prompt.asked(), ["descend into directory '/d'?"]);
    }

    #[test]
    fn interactive_once_asks_once_for_many_operands() {
        let fs = TreeFs::new().file("/a").file("/b").file("/c").file("/d");
        let out = Recorder::new();
        let declined = Answers::new(&[false]);
        assert_eq!(
            engine_run(
                remove_with(
                    Options {
                        interactive: Interactive::Once,
                        ..Options::DEFAULT
                    },
                    &["/a", "/b", "/c", "/d"],
                ),
                None,
                &fs,
                &declined,
                &NoHelp,
                &out,
            ),
            Ok(())
        );
        assert!(fs.removed().is_empty());
        assert_eq!(declined.asked(), ["remove 4 arguments?"]);

        let accepted = Answers::new(&[true]);
        assert_eq!(
            engine_run(
                remove_with(
                    Options {
                        interactive: Interactive::Once,
                        ..Options::DEFAULT
                    },
                    &["/a", "/b", "/c", "/d"],
                ),
                None,
                &fs,
                &accepted,
                &NoHelp,
                &out,
            ),
            Ok(())
        );
        assert_eq!(fs.removed(), ["/a", "/b", "/c", "/d"]);
    }

    #[test]
    fn interactive_once_asks_for_a_recursive_removal_and_not_for_few_files() {
        let fs = TreeFs::new().dir("/d");
        let out = Recorder::new();
        let prompt = Answers::new(&[true]);
        assert_eq!(
            engine_run(
                remove_with(
                    Options {
                        recursive: true,
                        interactive: Interactive::Once,
                        ..Options::DEFAULT
                    },
                    &["/d"],
                ),
                None,
                &fs,
                &prompt,
                &NoHelp,
                &out,
            ),
            Ok(())
        );
        assert_eq!(prompt.asked(), ["remove 1 argument recursively?"]);
        assert_eq!(fs.removed(), ["/d"]);

        // Three or fewer plain files ask nothing.
        let fs = TreeFs::new().file("/a");
        assert_eq!(
            run(
                remove_with(
                    Options {
                        interactive: Interactive::Once,
                        ..Options::DEFAULT
                    },
                    &["/a"],
                ),
                &fs,
                &out,
            ),
            Ok(())
        );
        assert_eq!(fs.removed(), ["/a"]);
    }

    #[test]
    fn an_unanswerable_prompt_fails_closed() {
        let fs = TreeFs::new().file("/a");
        let out = Recorder::new();
        let prompt = Answers::new(&[]);
        assert_eq!(
            engine_run(
                remove_with(
                    Options {
                        interactive: Interactive::Always,
                        ..Options::DEFAULT
                    },
                    &["/a"],
                ),
                None,
                &fs,
                &prompt,
                &NoHelp,
                &out,
            ),
            Err(RmError::Prompt(Errno::NotFound))
        );
        assert!(fs.removed().is_empty());
    }
}
