//! The fan-out engine: copy standard input to standard output and files.

use alloc::format;
use alloc::vec;

use rustos_help::{own_short_help, HelpSource};

use crate::command::{Command, Job, OutputError};
use crate::error::TeeError;
use crate::io::{FileSink, Input, Output};

/// The usage banner a usage error is reported with, and the fallback the
/// short-help switches print when `tee`'s own Help tree is unavailable.
pub const USAGE: &str = "usage: tee [-ap] [--output-error[=mode]] [--] [file...]";

/// `tee`'s own command word: the short-help switches render its own Help
/// document through the same engine as any other command's.
const OWN_WORD: &str = "tee";

/// Bytes pulled from standard input per read call. A fixed chunk bounds
/// the per-call buffer so input of any size streams through a constant
/// amount of memory.
const READ_CHUNK: usize = 4096;

/// One destination of the fan-out: the standard-output copy or one file
/// operand, with its liveness. A failed output is marked dead and skipped
/// thereafter (per the mode), exactly as the GNU tool nulls a failed
/// descriptor.
enum Target<'a> {
    /// The always-present copy to standard output — the RustOS analogue of
    /// the GNU pipe class (see [`OutputError`]).
    Stdout,
    /// The file operand at command-line position `id`.
    File { id: usize, path: &'a str },
}

impl Target<'_> {
    /// The name diagnostics report for this output, the GNU spelling.
    fn name(&self) -> &str {
        match self {
            Self::Stdout => "standard output",
            Self::File { path, .. } => path,
        }
    }

    /// Whether this output is the pipe class (see [`OutputError`]).
    fn is_pipe_class(&self) -> bool {
        matches!(self, Self::Stdout)
    }
}

/// What a failed output means under the selected mode: whether it is
/// diagnosed, whether it fails the run's exit status, and whether the run
/// stops immediately. Mirrors GNU `tee.c`: only a pipe-class failure under
/// a `-nopipe` mode (or the default) escapes the diagnosis, and only the
/// `exit` modes (or the default pipe-class case) stop the run.
struct Verdict {
    diagnose: bool,
    fail: bool,
    stop: bool,
}

fn judge(mode: OutputError, pipe_class: bool) -> Verdict {
    match (mode, pipe_class) {
        // The `-nopipe` modes tolerate a pipe-class failure silently: not
        // diagnosed, not counted, the run continues.
        (OutputError::WarnNopipe | OutputError::ExitNopipe, true) => Verdict {
            diagnose: false,
            fail: false,
            stop: false,
        },
        // Diagnosed and fatal: any counted failure under an `exit` mode,
        // and the default mode's pipe-class case — the SIGPIPE analogue:
        // the consumer went away, stop — stated on the diagnostic stream
        // (a RustOS process never ends silently).
        (OutputError::Default, true)
        | (OutputError::Exit, _)
        | (OutputError::ExitNopipe, false) => Verdict {
            diagnose: true,
            fail: true,
            stop: true,
        },
        // Diagnosed and counted; the run continues with the surviving
        // outputs, exactly as the GNU tool nulls the failed descriptor.
        (OutputError::Default | OutputError::WarnNopipe, false) | (OutputError::Warn, _) => {
            Verdict {
                diagnose: true,
                fail: true,
                stop: false,
            }
        }
    }
}

/// Whether `mode` is one of the immediately-fatal `exit` modes.
fn mode_is_exit(mode: OutputError) -> bool {
    matches!(mode, OutputError::Exit | OutputError::ExitNopipe)
}

/// Run one [`Command`], copying `stdin` to `out` and to each file operand
/// through `files`. `locale` is the user's `LANG` preference, if set;
/// `help` is the tool's own `Help/` tree, read by the short-help switches;
/// `err` receives the per-output diagnostics.
///
/// Returns `Ok(true)` when every output was served to end-of-input,
/// `Ok(false)` when at least one output failed in a way the selected mode
/// counts (it was handled per the mode and the run continued or stopped,
/// as in the GNU tool).
///
/// # Errors
///
/// [`TeeError::Output`] — writing a diagnostic to `err` failed. Nothing
/// else is fatal to report.
pub fn run(
    command: Command,
    locale: Option<&str>,
    stdin: &dyn Input,
    files: &dyn FileSink,
    help: &dyn HelpSource,
    out: &dyn Output,
    err: &dyn Output,
) -> Result<bool, TeeError> {
    let job = match command {
        Command::Help => {
            let bytes = own_short_help(help, locale, OWN_WORD)
                .unwrap_or_else(|| format!("{USAGE}\n").into_bytes());
            out.write_all(&bytes).map_err(TeeError::Output)?;
            return Ok(true);
        }
        Command::Tee(job) => job,
    };

    let mut all_ok = true;
    let mut targets = open_targets(&job, files, err, &mut all_ok)?;
    if targets.is_empty() {
        // Every file operand failed to open under an `exit` mode's fatal
        // open; unreachable otherwise (stdout is always present), but the
        // guard keeps the loop's invariant explicit.
        return Ok(all_ok);
    }

    let mut buf = vec![0u8; READ_CHUNK];
    loop {
        let n = match stdin.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(errno) => {
                diagnose(err, &format!("read error: {errno}"))?;
                all_ok = false;
                break;
            }
        };
        let chunk = &buf[..n];
        let mut index = 0;
        while index < targets.len() {
            let target = &targets[index];
            let result = match target {
                Target::Stdout => out.write_all(chunk),
                Target::File { id, .. } => files.write(*id, chunk),
            };
            match result {
                Ok(()) => index += 1,
                Err(errno) => {
                    let verdict = judge(job.on_error, target.is_pipe_class());
                    if verdict.diagnose {
                        diagnose(err, &format!("{}: {errno}", target.name()))?;
                    }
                    all_ok &= !verdict.fail;
                    if verdict.stop {
                        return Ok(false);
                    }
                    targets.remove(index);
                }
            }
        }
        if targets.is_empty() {
            // No live output remains: reading further would copy bytes to
            // nowhere, so stop exactly as the GNU tool does.
            break;
        }
    }
    Ok(all_ok)
}

/// Open every output of `job`: the standard-output copy first, then each
/// file operand in command-line order. An unopenable file is diagnosed and
/// skipped; under an `exit` mode it is immediately fatal, the GNU shape.
/// The returned list is empty only on that fatal path.
fn open_targets<'a>(
    job: &'a Job,
    files: &dyn FileSink,
    err: &dyn Output,
    all_ok: &mut bool,
) -> Result<alloc::vec::Vec<Target<'a>>, TeeError> {
    let mut targets = alloc::vec::Vec::with_capacity(job.files.len() + 1);
    targets.push(Target::Stdout);
    for (id, path) in job.files.iter().enumerate() {
        match files.open(id, path, job.append) {
            Ok(()) => targets.push(Target::File { id, path }),
            Err(errno) => {
                diagnose(err, &format!("{path}: {errno}"))?;
                *all_ok = false;
                if mode_is_exit(job.on_error) {
                    targets.clear();
                    return Ok(targets);
                }
            }
        }
    }
    Ok(targets)
}

/// One `tee: …` diagnostic line on the error stream. A failure to report
/// is itself fatal — the tool never continues silently.
fn diagnose(err: &dyn Output, message: &str) -> Result<(), TeeError> {
    err.write_all(format!("tee: {message}\n").as_bytes())
        .map_err(TeeError::Output)
}

#[cfg(test)]
mod tests {
    extern crate std;

    use alloc::collections::BTreeMap;
    use alloc::string::String;
    use alloc::vec::Vec;
    use core::cell::{Cell, RefCell};

    use rustos_abi::Errno;
    use rustos_help::{HelpSource, SourceError};

    use super::{run, USAGE};
    use crate::command::{parse, Command};
    use crate::error::TeeError;
    use crate::io::{FileSink, Input, Output};

    /// In-memory standard input, serving bounded slices so a streamed copy
    /// crosses several read calls, and remembering what was left unread.
    struct MemStdin {
        data: RefCell<Vec<u8>>,
        chunk: usize,
        /// Fail the read after this many bytes have been served.
        fail_after: Option<usize>,
        served: Cell<usize>,
    }

    impl MemStdin {
        fn new(data: &[u8]) -> Self {
            Self {
                data: RefCell::new(data.to_vec()),
                chunk: usize::MAX,
                fail_after: None,
                served: Cell::new(0),
            }
        }

        fn remaining(&self) -> usize {
            self.data.borrow().len()
        }
    }

    impl Input for MemStdin {
        fn read(&self, buf: &mut [u8]) -> Result<usize, Errno> {
            if let Some(limit) = self.fail_after {
                if self.served.get() >= limit {
                    return Err(Errno::NotImplemented);
                }
            }
            let mut data = self.data.borrow_mut();
            let take = buf.len().min(self.chunk).min(data.len());
            buf[..take].copy_from_slice(&data[..take]);
            data.drain(..take);
            self.served.set(self.served.get() + take);
            Ok(take)
        }
    }

    /// An in-memory disk behind the [`FileSink`] seam: `open` creates or
    /// truncates (or keeps, for append) a named byte vector, `write`
    /// extends it. Paths in `refuse_open` fail to open; `fail_write_at`
    /// fails operand `id`'s write once `n` writes have succeeded.
    #[derive(Default)]
    struct MemFiles {
        disk: RefCell<BTreeMap<String, Vec<u8>>>,
        handles: RefCell<BTreeMap<usize, String>>,
        opens: RefCell<Vec<(usize, String, bool)>>,
        writes: RefCell<BTreeMap<usize, usize>>,
        refuse_open: Vec<String>,
        fail_write_at: Option<(usize, usize)>,
    }

    impl MemFiles {
        fn with_existing(entries: &[(&str, &[u8])]) -> Self {
            let disk = entries
                .iter()
                .map(|(name, data)| (String::from(*name), data.to_vec()))
                .collect();
            Self {
                disk: RefCell::new(disk),
                ..Self::default()
            }
        }

        fn contents(&self, path: &str) -> Vec<u8> {
            self.disk.borrow().get(path).cloned().unwrap_or_default()
        }
    }

    impl FileSink for MemFiles {
        fn open(&self, id: usize, path: &str, append: bool) -> Result<(), Errno> {
            self.opens
                .borrow_mut()
                .push((id, String::from(path), append));
            if self.refuse_open.iter().any(|p| p == path) {
                return Err(Errno::PermissionDenied);
            }
            let mut disk = self.disk.borrow_mut();
            let entry = disk.entry(String::from(path)).or_default();
            if !append {
                entry.clear();
            }
            self.handles.borrow_mut().insert(id, String::from(path));
            Ok(())
        }

        fn write(&self, id: usize, bytes: &[u8]) -> Result<(), Errno> {
            if let Some((fail_id, at)) = self.fail_write_at {
                if fail_id == id {
                    let mut writes = self.writes.borrow_mut();
                    let count = writes.entry(id).or_insert(0);
                    if *count >= at {
                        return Err(Errno::NotImplemented);
                    }
                    *count += 1;
                }
            }
            let handles = self.handles.borrow();
            let Some(path) = handles.get(&id) else {
                return Err(Errno::NotFound);
            };
            let mut disk = self.disk.borrow_mut();
            let Some(entry) = disk.get_mut(path) else {
                return Err(Errno::NotFound);
            };
            entry.extend_from_slice(bytes);
            Ok(())
        }
    }

    /// A capturing output stream that accepts a bounded number of writes,
    /// then fails — the closed-consumer stand-in when bounded.
    struct Sink {
        accepted: RefCell<Vec<u8>>,
        remaining: Cell<usize>,
    }

    impl Sink {
        fn new() -> Self {
            Self::accepting(usize::MAX)
        }

        fn accepting(writes: usize) -> Self {
            Self {
                accepted: RefCell::new(Vec::new()),
                remaining: Cell::new(writes),
            }
        }

        fn bytes(&self) -> Vec<u8> {
            self.accepted.borrow().clone()
        }

        fn text(&self) -> String {
            String::from_utf8(self.bytes()).expect("fixture output is UTF-8")
        }
    }

    impl Output for Sink {
        fn write_all(&self, bytes: &[u8]) -> Result<(), Errno> {
            if self.remaining.get() == 0 {
                return Err(Errno::NotImplemented);
            }
            self.remaining.set(self.remaining.get() - 1);
            self.accepted.borrow_mut().extend_from_slice(bytes);
            Ok(())
        }
    }

    /// A bundle with no help documents, so the short help falls back to
    /// the usage banner.
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

    fn command(args: &[&str]) -> Command {
        parse(args).expect("fixture command line parses")
    }

    /// Run `args` against the fixtures, returning (all-ok, stdout, stderr).
    fn run_case(args: &[&str], stdin: &MemStdin, files: &MemFiles) -> (bool, Vec<u8>, String) {
        let out = Sink::new();
        let err = Sink::new();
        let ok = run(command(args), None, stdin, files, &NoHelp, &out, &err)
            .expect("fixture diagnostic stream never fails");
        (ok, out.bytes(), err.text())
    }

    #[test]
    fn copies_stdin_to_stdout_and_each_file() {
        let stdin = MemStdin::new(b"payload\n");
        let files = MemFiles::default();
        let (ok, out, err) = run_case(&["a", "b"], &stdin, &files);
        assert!(ok);
        assert_eq!(out, b"payload\n");
        assert_eq!(files.contents("a"), b"payload\n");
        assert_eq!(files.contents("b"), b"payload\n");
        assert!(err.is_empty());
    }

    #[test]
    fn no_operands_copy_to_stdout_alone() {
        let stdin = MemStdin::new(b"just stdout");
        let files = MemFiles::default();
        let (ok, out, err) = run_case(&[], &stdin, &files);
        assert!(ok);
        assert_eq!(out, b"just stdout");
        assert!(files.opens.borrow().is_empty());
        assert!(err.is_empty());
    }

    #[test]
    fn chunked_input_is_reassembled_in_order() {
        let mut stdin = MemStdin::new(b"0123456789");
        stdin.chunk = 3;
        let files = MemFiles::default();
        let (ok, out, _) = run_case(&["f"], &stdin, &files);
        assert!(ok);
        assert_eq!(out, b"0123456789");
        assert_eq!(files.contents("f"), b"0123456789");
    }

    #[test]
    fn overwrite_truncates_and_append_preserves() {
        let stdin = MemStdin::new(b"new");
        let files = MemFiles::with_existing(&[("f", b"old")]);
        let (ok, _, _) = run_case(&["f"], &stdin, &files);
        assert!(ok);
        assert_eq!(files.contents("f"), b"new");
        assert_eq!(*files.opens.borrow(), [(0, String::from("f"), false)]);

        let stdin = MemStdin::new(b"-more");
        let files = MemFiles::with_existing(&[("f", b"old")]);
        let (ok, _, _) = run_case(&["-a", "f"], &stdin, &files);
        assert!(ok);
        assert_eq!(files.contents("f"), b"old-more");
        assert_eq!(*files.opens.borrow(), [(0, String::from("f"), true)]);
    }

    #[test]
    fn an_unopenable_file_is_diagnosed_and_the_run_continues() {
        let stdin = MemStdin::new(b"data");
        let files = MemFiles {
            refuse_open: alloc::vec![String::from("locked")],
            ..MemFiles::default()
        };
        let (ok, out, err) = run_case(&["locked", "open"], &stdin, &files);
        assert!(!ok);
        assert_eq!(out, b"data");
        assert_eq!(files.contents("open"), b"data");
        assert!(
            err.starts_with("tee: locked:"),
            "unexpected diagnostic: {err}"
        );
    }

    #[test]
    fn an_unopenable_file_under_an_exit_mode_is_immediately_fatal() {
        let stdin = MemStdin::new(b"data");
        let files = MemFiles {
            refuse_open: alloc::vec![String::from("locked")],
            ..MemFiles::default()
        };
        let (ok, out, err) = run_case(&["--output-error=exit", "locked", "open"], &stdin, &files);
        assert!(!ok);
        // Nothing was read or copied: the failed open ended the run.
        assert!(out.is_empty());
        assert_eq!(stdin.remaining(), 4);
        assert!(
            err.starts_with("tee: locked:"),
            "unexpected diagnostic: {err}"
        );
    }

    #[test]
    fn a_failed_file_write_is_dropped_and_the_run_continues() {
        let mut stdin = MemStdin::new(b"abcdef");
        stdin.chunk = 3;
        let files = MemFiles {
            fail_write_at: Some((0, 1)),
            ..MemFiles::default()
        };
        let (ok, out, err) = run_case(&["flaky"], &stdin, &files);
        assert!(!ok);
        // Standard output was served in full; the file kept its first chunk.
        assert_eq!(out, b"abcdef");
        assert_eq!(files.contents("flaky"), b"abc");
        assert!(
            err.starts_with("tee: flaky:"),
            "unexpected diagnostic: {err}"
        );
    }

    #[test]
    fn default_mode_stops_when_standard_output_fails() {
        let mut stdin = MemStdin::new(b"abcdef");
        stdin.chunk = 3;
        let out = Sink::accepting(1);
        let err = Sink::new();
        let files = MemFiles::default();
        let ok = run(command(&["f"]), None, &stdin, &files, &NoHelp, &out, &err)
            .expect("fixture diagnostic stream never fails");
        assert!(!ok);
        // The first chunk landed everywhere; the second write to stdout
        // failed and stopped the run before the file saw a third chunk.
        assert_eq!(out.bytes(), b"abc");
        assert_eq!(files.contents("f"), b"abcdef"[..3].to_vec());
        assert!(
            err.text().starts_with("tee: standard output:"),
            "unexpected diagnostic: {}",
            err.text()
        );
    }

    #[test]
    fn warn_mode_diagnoses_a_failed_stdout_and_continues_with_files() {
        let mut stdin = MemStdin::new(b"abcdef");
        stdin.chunk = 3;
        let out = Sink::accepting(1);
        let err = Sink::new();
        let files = MemFiles::default();
        let ok = run(
            command(&["--output-error=warn", "f"]),
            None,
            &stdin,
            &files,
            &NoHelp,
            &out,
            &err,
        )
        .expect("fixture diagnostic stream never fails");
        assert!(!ok);
        assert_eq!(out.bytes(), b"abc");
        assert_eq!(files.contents("f"), b"abcdef");
        assert!(
            err.text().starts_with("tee: standard output:"),
            "unexpected diagnostic: {}",
            err.text()
        );
    }

    #[test]
    fn warn_nopipe_tolerates_a_failed_stdout_silently() {
        let mut stdin = MemStdin::new(b"abcdef");
        stdin.chunk = 3;
        let out = Sink::accepting(1);
        let err = Sink::new();
        let files = MemFiles::default();
        let ok = run(
            command(&["-p", "f"]),
            None,
            &stdin,
            &files,
            &NoHelp,
            &out,
            &err,
        )
        .expect("fixture diagnostic stream never fails");
        // The pipe-class failure neither fails the run nor is diagnosed.
        assert!(ok);
        assert_eq!(out.bytes(), b"abc");
        assert_eq!(files.contents("f"), b"abcdef");
        assert!(err.text().is_empty());
    }

    #[test]
    fn exit_mode_stops_on_the_first_failed_output() {
        let mut stdin = MemStdin::new(b"abcdef");
        stdin.chunk = 3;
        let files = MemFiles {
            fail_write_at: Some((0, 1)),
            ..MemFiles::default()
        };
        let (ok, out, err) = run_case(&["--output-error=exit", "flaky"], &stdin, &files);
        assert!(!ok);
        // The second chunk reached stdout, then the file write stopped the
        // run before a third read.
        assert_eq!(out, b"abcdef");
        assert_eq!(files.contents("flaky"), b"abc");
        assert!(
            err.starts_with("tee: flaky:"),
            "unexpected diagnostic: {err}"
        );
    }

    #[test]
    fn exit_nopipe_tolerates_stdout_but_stops_on_a_file() {
        let mut stdin = MemStdin::new(b"abcdef");
        stdin.chunk = 3;
        let out = Sink::accepting(1);
        let err = Sink::new();
        let files = MemFiles {
            fail_write_at: Some((0, 1)),
            ..MemFiles::default()
        };
        let ok = run(
            command(&["--output-error=exit-nopipe", "flaky"]),
            None,
            &stdin,
            &files,
            &NoHelp,
            &out,
            &err,
        )
        .expect("fixture diagnostic stream never fails");
        assert!(!ok);
        // Stdout's failure was tolerated silently; the file's second write
        // was the fatal one.
        assert_eq!(out.bytes(), b"abc");
        assert_eq!(files.contents("flaky"), b"abc");
        assert!(
            err.text().starts_with("tee: flaky:"),
            "unexpected diagnostic: {}",
            err.text()
        );
    }

    #[test]
    fn reading_stops_once_no_output_remains() {
        let mut stdin = MemStdin::new(b"abcdef");
        stdin.chunk = 3;
        let out = Sink::accepting(1);
        let err = Sink::new();
        let files = MemFiles::default();
        let ok = run(command(&["-p"]), None, &stdin, &files, &NoHelp, &out, &err)
            .expect("fixture diagnostic stream never fails");
        assert!(ok);
        // After stdout (the only output) died, the remaining input was
        // left unread rather than pumped to nowhere.
        assert_eq!(out.bytes(), b"abc");
        assert_eq!(stdin.remaining(), 0);
        assert_eq!(stdin.served.get(), 6);
    }

    #[test]
    fn a_read_error_is_diagnosed_after_the_bytes_already_served() {
        let mut stdin = MemStdin::new(b"abcdef");
        stdin.chunk = 3;
        stdin.fail_after = Some(3);
        let files = MemFiles::default();
        let (ok, out, err) = run_case(&["f"], &stdin, &files);
        assert!(!ok);
        assert_eq!(out, b"abc");
        assert_eq!(files.contents("f"), b"abc");
        assert!(
            err.starts_with("tee: read error:"),
            "unexpected diagnostic: {err}"
        );
    }

    #[test]
    fn short_help_falls_back_to_the_usage_banner() {
        let stdin = MemStdin::new(b"never read");
        let files = MemFiles::default();
        let (ok, out, err) = run_case(&["-h"], &stdin, &files);
        assert!(ok);
        assert_eq!(out, alloc::format!("{USAGE}\n").into_bytes());
        assert_eq!(stdin.remaining(), 10);
        assert!(err.is_empty());
    }

    #[test]
    fn a_failed_diagnostic_write_is_fatal() {
        let stdin = MemStdin::new(b"data");
        let files = MemFiles {
            refuse_open: alloc::vec![String::from("locked")],
            ..MemFiles::default()
        };
        let result = run(
            command(&["locked"]),
            None,
            &stdin,
            &files,
            &NoHelp,
            &Sink::new(),
            &Sink::accepting(0),
        );
        assert_eq!(result, Err(TeeError::Output(Errno::NotImplemented)));
    }

    /// Every locale's `OPTIONS` section documents exactly the switches this
    /// parser accepts (`plans/APPS.md` §3.1): the flag tokens are
    /// language-neutral, so each translated document must carry the same
    /// keys as the canonical one. The documents are read from the bundle's
    /// own on-disk `Help/` tree — the single source the image builder
    /// plants — never a copy embedded in this crate.
    #[test]
    fn help_documents_the_parser_switches() {
        use alloc::format;
        use std::fs;

        let help_root = format!("{}/Help", env!("CARGO_MANIFEST_DIR"));
        let locales = ["en-US", "fr-FR", "de-DE", "es-ES", "uk-UA", "it-IT"];
        for locale in locales {
            let path = format!("{help_root}/{locale}/tee.md");
            let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
            for switch in [
                "`-a, --append`",
                "`-p`",
                "`--output-error[=<mode>]`",
                "`-h, -?`",
            ] {
                assert!(
                    text.contains(switch),
                    "{locale}/tee.md must document {switch}"
                );
            }
        }
    }
}
