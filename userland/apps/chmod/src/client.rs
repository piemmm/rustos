//! The mode-changing engine: stat each operand, compute its new mode, apply
//! it, and — with `-R` — descend into directories applying the same mode.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use tairix_help::{own_short_help, HelpSource};
use tairix_path::join;

use crate::command::{Command, Mode, Options, Verbosity};
use crate::error::ChmodError;
use crate::io::{Entry, EntryKind, FileSystem, Output};

/// The usage banner a usage error is reported with, and the fallback the
/// short-help switches print when `chmod`'s own Help tree is unavailable.
pub const USAGE: &str = "\
usage: chmod [-cfRv] [--] MODE file...

  -R, --recursive       change files and directories recursively
  -c, --changes         report only files whose mode actually changed
  -v, --verbose         report every file processed
  -f, --silent, --quiet suppress most error messages
  -h, -?, --help        show this message

MODE is either an octal value (e.g. 644, 0755) or a comma-separated list of
symbolic clauses [ugoa]*[-+=][rwxXst]* (e.g. g+w, o-rx, a=rx, u+s). `--` ends
option parsing: every later argument is an operand. To set a mode beginning
with `-`, write it without the dash (a-w) or end options first (chmod -- -w f).
";

/// `chmod`'s own command word: the short-help switches render its own Help
/// document through the same engine as any other command's.
const OWN_WORD: &str = "chmod";

/// Run one [`Command`], changing the mode of its files through `fs`.
/// `locale` is the user's `LANG` preference, if set; `help` is the tool's
/// own `Help/` tree, read by the short-help switches.
///
/// Each file is inspected, its new mode computed from the parsed [`Mode`] and
/// the file's current mode/kind, and the result applied. With `-R` a directory
/// is changed and then its contents are changed recursively. `-v` reports
/// every file processed on `out` and `-c` only those whose mode actually
/// changed; otherwise `chmod` writes nothing on success beyond the
/// [`Command::Help`] banner.
///
/// The first failure stops the run before any later operand (fail closed) —
/// except under `-f`, which suppresses each failing operand's diagnostic,
/// continues with the remaining operands, and fails the whole run afterwards
/// as the message-less [`ChmodError::Silenced`].
///
/// # Errors
///
/// * [`ChmodError::Stat`] — an operand could not be inspected; carries the
///   underlying [`Errno`](tairix_abi::Errno).
/// * [`ChmodError::Apply`] — applying the new mode failed.
/// * [`ChmodError::Read`] — a directory's entries could not be read during a
///   recursive descent.
/// * [`ChmodError::Silenced`] — one or more operands failed under `-f`.
/// * [`ChmodError::Output`] — writing the usage banner or a report failed.
pub fn run(
    command: Command,
    locale: Option<&str>,
    fs: &dyn FileSystem,
    help: &dyn HelpSource,
    out: &dyn Output,
) -> Result<(), ChmodError> {
    match command {
        Command::Help => short_help(locale, help, out),
        Command::Change {
            options,
            mode,
            files,
        } => {
            let mut silenced = false;
            for file in &files {
                match change(file, &mode, options, fs, out) {
                    Ok(()) => {}
                    // A report that cannot be written is never suppressed:
                    // `-f` silences filesystem failures, not a dead terminal.
                    Err(ChmodError::Output(errno)) => return Err(ChmodError::Output(errno)),
                    Err(_) if options.quiet => silenced = true,
                    Err(err) => return Err(err),
                }
            }
            if silenced {
                return Err(ChmodError::Silenced);
            }
            Ok(())
        }
    }
}

/// Render `chmod`'s own short help (`NAME` + `SYNOPSIS` + compact `OPTIONS`)
/// from its own Help tree through the one shared engine; when no document
/// can be served (a build without the bundle's documents) the usage banner
/// stands in — the tool's own text, not fabricated help content — so `-h`
/// never fails.
fn short_help(
    locale: Option<&str>,
    help: &dyn HelpSource,
    out: &dyn Output,
) -> Result<(), ChmodError> {
    let bytes =
        own_short_help(help, locale, OWN_WORD).unwrap_or_else(|| String::from(USAGE).into_bytes());
    out.write_all(&bytes).map_err(ChmodError::Output)
}

/// Apply `mode` to `path`, then — when `-R` and `path` is a directory —
/// to each of its entries in turn, reporting per the verbosity policy.
fn change(
    path: &str,
    mode: &Mode,
    options: Options,
    fs: &dyn FileSystem,
    out: &dyn Output,
) -> Result<(), ChmodError> {
    let meta = fs.stat(path).map_err(ChmodError::Stat)?;
    let is_dir = meta.kind == EntryKind::Directory;
    let old_mode = meta.mode & 0o7777;
    let new_mode = mode.resolve(meta.mode, is_dir);
    fs.set_mode(path, new_mode).map_err(ChmodError::Apply)?;
    report(path, old_mode, new_mode, options, out)?;
    if options.recursive && is_dir {
        for entry in read_children(path, fs)? {
            let child = join(path, &entry.name);
            change(&child, mode, options, fs, out)?;
        }
    }
    Ok(())
}

/// Report one processed file in the GNU wording, per the verbosity policy:
/// `-v` reports every file, `-c` only an actual change.
fn report(
    path: &str,
    old_mode: u32,
    new_mode: u32,
    options: Options,
    out: &dyn Output,
) -> Result<(), ChmodError> {
    let changed = old_mode != new_mode;
    let wanted = match options.verbosity {
        Verbosity::None => false,
        Verbosity::Changes => changed,
        Verbosity::All => true,
    };
    if !wanted {
        return Ok(());
    }
    let line = if changed {
        format!(
            "mode of '{path}' changed from {old_mode:04o} ({}) to {new_mode:04o} ({})\n",
            perm_string(old_mode),
            perm_string(new_mode)
        )
    } else {
        format!(
            "mode of '{path}' retained as {old_mode:04o} ({})\n",
            perm_string(old_mode)
        )
    };
    out.write_all(line.as_bytes()).map_err(ChmodError::Output)
}

/// The nine-character `rwx` rendering of `mode`'s permission bits.
fn perm_string(mode: u32) -> String {
    const PERMISSIONS: [(u32, char); 9] = [
        (0o400, 'r'),
        (0o200, 'w'),
        (0o100, 'x'),
        (0o040, 'r'),
        (0o020, 'w'),
        (0o010, 'x'),
        (0o004, 'r'),
        (0o002, 'w'),
        (0o001, 'x'),
    ];
    let mut s = String::with_capacity(9);
    for (bit, ch) in PERMISSIONS {
        s.push(if mode & bit != 0 { ch } else { '-' });
    }
    s
}

/// Read all entries of the directory `path` into a vector, so the recursive
/// descent does not depend on entry indices staying stable as modes change.
fn read_children(path: &str, fs: &dyn FileSystem) -> Result<Vec<Entry>, ChmodError> {
    let mut entries = Vec::new();
    let mut index: u64 = 0;
    while let Some(entry) = fs.read_dir(path, index).map_err(ChmodError::Read)? {
        index = index.saturating_add(1);
        entries.push(entry);
    }
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::{run as engine_run, USAGE};
    use crate::command::parse;
    use crate::error::ChmodError;
    use crate::io::{Entry, EntryKind, FileSystem, Metadata, Output};
    use alloc::string::{String, ToString};
    use alloc::vec::Vec;
    use core::cell::RefCell;
    use tairix_abi::Errno;
    use tairix_help::{HelpSource, SourceError};

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

    /// A Help tree holding one canonical `chmod.md` document.
    struct OneDoc;

    const DOC: &str = "## NAME\n\nchmod — change file mode bits\n\n\
                       ## SYNOPSIS\n\n`chmod [-cfRv] [--] MODE file...`\n\n\
                       ## DESCRIPTION\n\nChanges modes.\n";

    impl HelpSource for OneDoc {
        fn locale_dirs(&self) -> Result<Vec<String>, SourceError> {
            Ok(alloc::vec![String::from("en-US")])
        }

        fn read(&self, locale_dir: &str, file_name: &str) -> Result<Option<Vec<u8>>, SourceError> {
            if locale_dir == "en-US" && file_name == "chmod.md" {
                Ok(Some(DOC.as_bytes().to_vec()))
            } else {
                Ok(None)
            }
        }
    }

    /// An in-memory tree. Each node carries its kind and current mode; a
    /// directory's children are derived by parent path. Failure injection
    /// covers the stat/apply/read fail-closed paths.
    struct MemFs {
        state: RefCell<State>,
    }

    struct State {
        nodes: Vec<(String, EntryKind, u32)>,
        stat_fail: Option<(String, Errno)>,
        set_fail: Option<(String, Errno)>,
        read_fail: Option<(String, Errno)>,
        applied: Vec<(String, u32)>,
    }

    impl MemFs {
        fn new() -> Self {
            Self {
                state: RefCell::new(State {
                    nodes: Vec::new(),
                    stat_fail: None,
                    set_fail: None,
                    read_fail: None,
                    applied: Vec::new(),
                }),
            }
        }

        fn file(self, path: &str, mode: u32) -> Self {
            self.state
                .borrow_mut()
                .nodes
                .push((path.to_string(), EntryKind::File, mode));
            self
        }

        fn dir(self, path: &str, mode: u32) -> Self {
            self.state
                .borrow_mut()
                .nodes
                .push((path.to_string(), EntryKind::Directory, mode));
            self
        }

        fn stat_fails(self, path: &str, errno: Errno) -> Self {
            self.state.borrow_mut().stat_fail = Some((path.to_string(), errno));
            self
        }

        fn set_fails(self, path: &str, errno: Errno) -> Self {
            self.state.borrow_mut().set_fail = Some((path.to_string(), errno));
            self
        }

        fn read_fails(self, path: &str, errno: Errno) -> Self {
            self.state.borrow_mut().read_fail = Some((path.to_string(), errno));
            self
        }

        fn mode_of(&self, path: &str) -> Option<u32> {
            self.state
                .borrow()
                .nodes
                .iter()
                .find(|(p, _, _)| p == path)
                .map(|(_, _, m)| *m)
        }

        fn applied(&self) -> Vec<(String, u32)> {
            self.state.borrow().applied.clone()
        }
    }

    /// The immediate parent of an absolute path: `/a/b` maps to `/a`, and a
    /// top-level `/a` maps to the empty string (its children sit at the root).
    fn parent_of(path: &str) -> &str {
        match path.rfind('/') {
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
        fn stat(&self, path: &str) -> Result<Metadata, Errno> {
            let state = self.state.borrow();
            if let Some((p, errno)) = &state.stat_fail {
                if p == path {
                    return Err(*errno);
                }
            }
            state
                .nodes
                .iter()
                .find(|(p, _, _)| p == path)
                .map(|(_, kind, mode)| Metadata {
                    kind: *kind,
                    mode: *mode,
                })
                .ok_or(Errno::NotFound)
        }

        fn set_mode(&self, path: &str, mode: u32) -> Result<(), Errno> {
            let mut state = self.state.borrow_mut();
            if let Some((p, errno)) = &state.set_fail {
                if p == path {
                    return Err(*errno);
                }
            }
            match state.nodes.iter_mut().find(|(p, _, _)| p == path) {
                Some((_, _, stored)) => *stored = mode,
                None => return Err(Errno::NotFound),
            }
            state.applied.push((path.to_string(), mode));
            Ok(())
        }

        fn read_dir(&self, path: &str, index: u64) -> Result<Option<Entry>, Errno> {
            let state = self.state.borrow();
            if let Some((p, errno)) = &state.read_fail {
                if p == path {
                    return Err(*errno);
                }
            }
            let mut children: Vec<Entry> = state
                .nodes
                .iter()
                .filter(|(p, _, _)| parent_of(p) == path && p.as_str() != path)
                .map(|(p, kind, _)| Entry {
                    name: name_of(p).to_string(),
                    kind: *kind,
                })
                .collect();
            children.sort_by(|a, b| a.name.cmp(&b.name));
            Ok(children
                .into_iter()
                .nth(usize::try_from(index).unwrap_or(usize::MAX)))
        }
    }

    /// A terminal that records every byte written to it.
    struct Recorder {
        bytes: RefCell<Vec<u8>>,
    }

    impl Recorder {
        fn new() -> Self {
            Self {
                bytes: RefCell::new(Vec::new()),
            }
        }

        fn written(&self) -> Vec<u8> {
            self.bytes.borrow().clone()
        }
    }

    impl Output for Recorder {
        fn write_all(&self, bytes: &[u8]) -> Result<(), Errno> {
            self.bytes.borrow_mut().extend_from_slice(bytes);
            Ok(())
        }
    }

    fn run_args(args: &[&str], fs: &MemFs, out: &Recorder) -> Result<(), ChmodError> {
        engine_run(parse(args).expect("valid command"), None, fs, &NoHelp, out)
    }

    #[test]
    fn help_prints_the_usage_banner() {
        // With no Help documents available the tool's own usage banner
        // stands in — never fabricated help content.
        let fs = MemFs::new();
        let out = Recorder::new();
        assert_eq!(run_args(&["--help"], &fs, &out), Ok(()));
        assert_eq!(out.written(), USAGE.as_bytes());
    }

    #[test]
    fn help_renders_the_short_help_from_the_document() {
        let fs = MemFs::new();
        let out = Recorder::new();
        assert_eq!(
            engine_run(
                parse(&["-h"]).expect("valid command"),
                None,
                &fs,
                &OneDoc,
                &out
            ),
            Ok(())
        );
        let written = String::from_utf8(out.written()).expect("utf-8 help");
        assert!(
            written.contains("chmod — change file mode bits"),
            "{written}"
        );
        assert!(
            written.contains("chmod [-cfRv] [--] MODE file..."),
            "{written}"
        );
    }

    #[test]
    fn an_octal_mode_replaces_a_file_mode() {
        let fs = MemFs::new().file("/f", 0o600);
        let out = Recorder::new();
        assert_eq!(run_args(&["644", "/f"], &fs, &out), Ok(()));
        assert_eq!(fs.mode_of("/f"), Some(0o644));
        assert_eq!(fs.applied(), [("/f".to_string(), 0o644)]);
        assert!(out.written().is_empty());
    }

    #[test]
    fn a_symbolic_mode_transforms_the_current_mode() {
        let fs = MemFs::new().file("/f", 0o644);
        let out = Recorder::new();
        assert_eq!(run_args(&["g+w", "/f"], &fs, &out), Ok(()));
        assert_eq!(fs.mode_of("/f"), Some(0o664));
    }

    #[test]
    fn several_files_are_all_changed() {
        let fs = MemFs::new()
            .file("/a", 0o600)
            .file("/b", 0o600)
            .file("/c", 0o600);
        let out = Recorder::new();
        assert_eq!(run_args(&["640", "/a", "/b", "/c"], &fs, &out), Ok(()));
        assert_eq!(fs.mode_of("/a"), Some(0o640));
        assert_eq!(fs.mode_of("/b"), Some(0o640));
        assert_eq!(fs.mode_of("/c"), Some(0o640));
    }

    #[test]
    fn without_recursive_only_the_named_directory_changes() {
        let fs = MemFs::new().dir("/d", 0o755).file("/d/f", 0o644);
        let out = Recorder::new();
        assert_eq!(run_args(&["700", "/d"], &fs, &out), Ok(()));
        assert_eq!(fs.mode_of("/d"), Some(0o700));
        // The child is untouched.
        assert_eq!(fs.mode_of("/d/f"), Some(0o644));
    }

    #[test]
    fn recursive_changes_the_directory_then_its_contents() {
        let fs = MemFs::new()
            .dir("/d", 0o700)
            .file("/d/f", 0o600)
            .dir("/d/sub", 0o700)
            .file("/d/sub/g", 0o600);
        let out = Recorder::new();
        assert_eq!(run_args(&["-R", "go+r", "/d"], &fs, &out), Ok(()));
        assert_eq!(fs.mode_of("/d"), Some(0o744));
        assert_eq!(fs.mode_of("/d/f"), Some(0o644));
        assert_eq!(fs.mode_of("/d/sub"), Some(0o744));
        assert_eq!(fs.mode_of("/d/sub/g"), Some(0o644));
        // The directory itself is changed before its contents.
        let applied = fs.applied();
        assert_eq!(applied[0].0, "/d");
    }

    #[test]
    fn recursive_conditional_execute_uses_each_node_kind() {
        // `a+X` gives a directory execute but leaves a non-executable file.
        let fs = MemFs::new().dir("/d", 0o644).file("/d/f", 0o644);
        let out = Recorder::new();
        assert_eq!(run_args(&["-R", "a+X", "/d"], &fs, &out), Ok(()));
        assert_eq!(fs.mode_of("/d"), Some(0o755));
        assert_eq!(fs.mode_of("/d/f"), Some(0o644));
    }

    #[test]
    fn a_missing_operand_is_stat_and_stops_the_run() {
        let fs = MemFs::new().file("/b", 0o600);
        let out = Recorder::new();
        // `/a` does not exist: the run fails before reaching `/b`.
        assert_eq!(
            run_args(&["644", "/a", "/b"], &fs, &out),
            Err(ChmodError::Stat(Errno::NotFound))
        );
        assert_eq!(fs.mode_of("/b"), Some(0o600));
    }

    #[test]
    fn a_stat_permission_error_surfaces() {
        let fs = MemFs::new()
            .file("/f", 0o600)
            .stat_fails("/f", Errno::PermissionDenied);
        let out = Recorder::new();
        assert_eq!(
            run_args(&["644", "/f"], &fs, &out),
            Err(ChmodError::Stat(Errno::PermissionDenied))
        );
    }

    #[test]
    fn an_apply_error_surfaces() {
        let fs = MemFs::new()
            .file("/f", 0o600)
            .set_fails("/f", Errno::PermissionDenied);
        let out = Recorder::new();
        assert_eq!(
            run_args(&["644", "/f"], &fs, &out),
            Err(ChmodError::Apply(Errno::PermissionDenied))
        );
        // The mode is unchanged.
        assert_eq!(fs.mode_of("/f"), Some(0o600));
    }

    #[test]
    fn a_read_dir_error_during_recursion_surfaces() {
        let fs = MemFs::new()
            .dir("/d", 0o700)
            .file("/d/f", 0o600)
            .read_fails("/d", Errno::PermissionDenied);
        let out = Recorder::new();
        assert_eq!(
            run_args(&["-R", "700", "/d"], &fs, &out),
            Err(ChmodError::Read(Errno::PermissionDenied))
        );
        // The directory itself was changed before the read failed.
        assert_eq!(fs.mode_of("/d"), Some(0o700));
    }

    #[test]
    fn verbose_reports_changed_and_retained_in_gnu_wording() {
        let fs = MemFs::new().file("/f", 0o644);
        let out = Recorder::new();
        assert_eq!(run_args(&["-v", "644", "/f"], &fs, &out), Ok(()));
        assert_eq!(
            out.written(),
            b"mode of '/f' retained as 0644 (rw-r--r--)\n"
        );
        let out = Recorder::new();
        assert_eq!(run_args(&["-v", "664", "/f"], &fs, &out), Ok(()));
        assert_eq!(
            out.written(),
            b"mode of '/f' changed from 0644 (rw-r--r--) to 0664 (rw-rw-r--)\n"
        );
    }

    #[test]
    fn changes_reports_only_an_actual_change() {
        let fs = MemFs::new().file("/f", 0o644);
        let out = Recorder::new();
        // The mode is already 644: `-c` reports nothing.
        assert_eq!(run_args(&["-c", "644", "/f"], &fs, &out), Ok(()));
        assert!(out.written().is_empty());
        let out = Recorder::new();
        assert_eq!(run_args(&["-c", "600", "/f"], &fs, &out), Ok(()));
        assert_eq!(
            out.written(),
            b"mode of '/f' changed from 0644 (rw-r--r--) to 0600 (rw-------)\n"
        );
    }

    #[test]
    fn quiet_suppresses_the_failure_but_still_fails_the_run() {
        let fs = MemFs::new().file("/b", 0o600);
        let out = Recorder::new();
        // `/a` is missing: `-f` keeps going (so `/b` is still changed) and
        // the run fails afterwards with the message-less error.
        assert_eq!(
            run_args(&["-f", "644", "/a", "/b"], &fs, &out),
            Err(ChmodError::Silenced)
        );
        assert_eq!(fs.mode_of("/b"), Some(0o644));
        assert!(out.written().is_empty());
    }
}
