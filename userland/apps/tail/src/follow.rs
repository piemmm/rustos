//! The `tail -f`/`-F` follow engine.
//!
//! After the initial output, a follow keeps each file source open and
//! re-emits appended data as the file grows, blocking off-CPU on the
//! kernel's file-change wait source (the [`Watcher`] seam) between
//! emissions — never a busy poll. `-f` follows the open node by descriptor;
//! `-F` follows the *name*, watching the parent directory so a rotation
//! (the name replaced by a new file) reopens the new file, and `--retry`ing
//! a name that is not yet present.

use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use tairix_abi::FileId;

use crate::client::{
    diagnose, emit_omission_record, serve_stdin, write_header, Engine, READ_CHUNK,
};
use crate::command::{Count, Follow, HeaderMode, Job, Source};
use crate::error::TailError;
use crate::io::{Info, Input, Output, Watcher};

/// One followed file source's live state.
struct FollowSource {
    /// The operand path, as given (used for headers, re-open, and rotation
    /// re-stat).
    path: String,
    /// The open file handle, or [`None`] when the name is currently
    /// inaccessible and being retried (`--retry`/`-F`).
    handle: Option<u64>,
    /// The watched parent-directory handle (name follow only), so a create
    /// or rename in the directory wakes the engine at once.
    dir: Option<u64>,
    /// Identity of the currently open node, for rotation detection.
    id: FileId,
    /// Bytes already emitted (the next read offset).
    offset: u64,
    /// Consecutive re-check cycles with no growth, for the
    /// `--max-unchanged-stats` forced by-name reopen.
    unchanged: u64,
}

/// Run a follow (`-f`/`-F`): initial output for every source, then block on
/// the change wait source and re-emit appended data until interrupted (or,
/// with `--pid`, until the process dies).
#[allow(clippy::too_many_arguments)]
pub fn run_follow(
    job: &Job,
    mode: Follow,
    watcher: &dyn Watcher,
    stdin: &dyn Input,
    out: &dyn Output,
    err: &dyn Output,
    info: &dyn Info,
) -> Result<bool, TailError> {
    let show_headers = match job.headers {
        HeaderMode::Always => true,
        HeaderMode::Never => false,
        HeaderMode::MultipleFiles => job.sources.len() > 1,
    };
    let lines = matches!(job.count, Count::Lines(_));

    let mut all_ok = true;
    let mut header_written = false;
    let mut omitted_total: u64 = 0;
    // The source index whose header was last written, so multi-file follow
    // reprints a header only when the emitting file changes (the GNU rule).
    let mut active: Option<usize> = None;
    let mut sources: Vec<FollowSource> = Vec::new();

    for (index, source) in job.sources.iter().enumerate() {
        match source {
            // Standard input is not a watchable node: emit its initial tail
            // and do not follow it (as the GNU tool warns and does).
            Source::Stdin => {
                if show_headers {
                    write_header(out, "standard input", &mut header_written)
                        .map_err(TailError::Output)?;
                    active = None;
                }
                let (ok, omitted) = serve_stdin(job, stdin, out, err)?;
                all_ok &= ok;
                omitted_total = omitted_total.saturating_add(omitted);
            }
            Source::Path(path) => {
                let (ok, omitted) = open_and_emit_initial(
                    job,
                    mode,
                    watcher,
                    path,
                    out,
                    err,
                    show_headers,
                    &mut header_written,
                    &mut active,
                    index,
                    &mut sources,
                )?;
                all_ok &= ok;
                omitted_total = omitted_total.saturating_add(omitted);
            }
        }
    }

    if omitted_total > 0 {
        emit_omission_record(info, omitted_total, lines, job.from_start);
    }

    // Nothing left to follow (only standard input, or every named source is
    // gone and not being retried): the follow is complete.
    let followable = sources
        .iter()
        .any(|s| s.handle.is_some() || (job.retry && !s.path.is_empty()));
    if !followable {
        return Ok(all_ok);
    }

    follow_loop(
        job,
        mode,
        watcher,
        out,
        err,
        show_headers,
        &mut sources,
        &mut header_written,
        &mut active,
    )?;
    Ok(all_ok)
}

/// The blocking follow loop: park on the change wait source, then re-poll
/// every source for appended data. With `--pid`, a bounded timeout re-checks
/// the process and the loop ends once it dies; otherwise it blocks until the
/// process is interrupted (the shell's `^C`).
#[allow(clippy::too_many_arguments)]
fn follow_loop(
    job: &Job,
    mode: Follow,
    watcher: &dyn Watcher,
    out: &dyn Output,
    err: &dyn Output,
    show_headers: bool,
    sources: &mut [FollowSource],
    header_written: &mut bool,
    active: &mut Option<usize>,
) -> Result<(), TailError> {
    loop {
        watcher.block(follow_timeout(job, mode, sources));
        for i in 0..sources.len() {
            poll_source(
                job,
                mode,
                watcher,
                out,
                err,
                show_headers,
                sources,
                i,
                header_written,
                active,
            )?;
        }
        if let Some(pid) = job.pid {
            if !watcher.pid_alive(pid) {
                // The process is gone: drain any last-moment appended data
                // once more, then stop following.
                for i in 0..sources.len() {
                    poll_source(
                        job,
                        mode,
                        watcher,
                        out,
                        err,
                        show_headers,
                        sources,
                        i,
                        header_written,
                        active,
                    )?;
                }
                return Ok(());
            }
        }
    }
}

/// The wait timeout for one follow cycle: the `--sleep-interval` when a
/// bounded re-check is needed (a `--pid` to poll, a currently-missing name
/// to retry, or a name follow with no directory-watch wake source),
/// otherwise no timeout — the change wait source alone wakes the loop.
fn follow_timeout(job: &Job, mode: Follow, sources: &[FollowSource]) -> u64 {
    let needs_poll = job.pid.is_some()
        || sources.iter().any(|s| s.handle.is_none())
        || (mode == Follow::Name && sources.iter().any(|s| s.dir.is_none()));
    if needs_poll {
        job.sleep_ns
    } else {
        u64::MAX
    }
}

/// Poll one source once: retry a missing name, detect rotation (name
/// follow), detect truncation, and emit any appended bytes.
#[allow(clippy::too_many_arguments)]
fn poll_source(
    job: &Job,
    mode: Follow,
    watcher: &dyn Watcher,
    out: &dyn Output,
    err: &dyn Output,
    show_headers: bool,
    sources: &mut [FollowSource],
    i: usize,
    header_written: &mut bool,
    active: &mut Option<usize>,
) -> Result<(), TailError> {
    // A currently-missing name: reopen it if it has appeared (name follow,
    // or `--retry`). A descriptor follow without retry has nothing to reopen.
    if sources[i].handle.is_none() {
        if !(job.retry || mode == Follow::Name) {
            return Ok(());
        }
        match watcher.open(&sources[i].path) {
            Ok(handle) => {
                let id = watcher.meta(handle).map_or(FileId::NONE, |m| m.id);
                let _ = watcher.watch(handle);
                sources[i].handle = Some(handle);
                sources[i].id = id;
                sources[i].offset = 0;
                sources[i].unchanged = 0;
                let path = sources[i].path.clone();
                diagnose(err, &format!("'{path}' has appeared;  following new file"))?;
            }
            Err(_) => return Ok(()),
        }
    }

    // Name follow: if the name now resolves to a different node, the file was
    // rotated — reopen the new file from its start.
    if mode == Follow::Name {
        if let Ok(meta) = watcher.meta_path(&sources[i].path) {
            if !meta.id.is_none() && meta.id != sources[i].id {
                reopen_rotated(watcher, sources, i, err)?;
            }
        }
    }

    let Some(handle) = sources[i].handle else {
        return Ok(());
    };
    // The handle became invalid (the node vanished under a descriptor
    // follow): drop it. Name follow will retry the name.
    let Ok(meta) = watcher.meta(handle) else {
        watcher.unwatch(handle);
        watcher.close(handle);
        sources[i].handle = None;
        return Ok(());
    };

    if meta.size < sources[i].offset {
        let path = sources[i].path.clone();
        diagnose(err, &format!("{path}: file truncated"))?;
        sources[i].offset = 0;
    }

    if meta.size > sources[i].offset {
        if show_headers && *active != Some(i) {
            let path = sources[i].path.clone();
            write_header(out, &path, header_written).map_err(TailError::Output)?;
            *active = Some(i);
        }
        let new_offset = emit_appended(
            watcher,
            handle,
            sources[i].offset,
            out,
            err,
            &sources[i].path,
        )?;
        sources[i].offset = new_offset;
        sources[i].unchanged = 0;
    } else {
        sources[i].unchanged = sources[i].unchanged.saturating_add(1);
        // `--max-unchanged-stats`: after this many quiet cycles, force a
        // by-name rotation re-check even absent a directory-watch wake (a
        // safety net for a name whose replacement the per-cycle check
        // missed). Only meaningful for name follow.
        if mode == Follow::Name
            && job.max_unchanged > 0
            && sources[i].unchanged >= job.max_unchanged
        {
            sources[i].unchanged = 0;
            if let Ok(meta) = watcher.meta_path(&sources[i].path) {
                if !meta.id.is_none() && meta.id != sources[i].id {
                    reopen_rotated(watcher, sources, i, err)?;
                }
            }
        }
    }
    Ok(())
}

/// Reopen a rotated name: close the old node, open the new one at the same
/// name, reset the offset, and re-register the watch.
fn reopen_rotated(
    watcher: &dyn Watcher,
    sources: &mut [FollowSource],
    i: usize,
    err: &dyn Output,
) -> Result<(), TailError> {
    match watcher.open(&sources[i].path) {
        Ok(handle) => {
            if let Some(old) = sources[i].handle.take() {
                watcher.unwatch(old);
                watcher.close(old);
            }
            let id = watcher.meta(handle).map_or(FileId::NONE, |m| m.id);
            let _ = watcher.watch(handle);
            sources[i].handle = Some(handle);
            sources[i].id = id;
            sources[i].offset = 0;
            sources[i].unchanged = 0;
            let path = sources[i].path.clone();
            diagnose(
                err,
                &format!("'{path}' has been replaced;  following new file"),
            )
        }
        // The new name is not openable yet; keep the old handle and let the
        // next cycle retry.
        Err(_) => Ok(()),
    }
}

/// Emit every byte of `handle` from `from` to end of file, returning the new
/// offset. A read error is diagnosed and stops this source's emission for
/// the cycle (the next cycle retries).
fn emit_appended(
    watcher: &dyn Watcher,
    handle: u64,
    from: u64,
    out: &dyn Output,
    err: &dyn Output,
    path: &str,
) -> Result<u64, TailError> {
    let mut buf = vec![0u8; READ_CHUNK];
    let mut offset = from;
    loop {
        match watcher.read_at(handle, offset, &mut buf) {
            Ok(0) => break,
            Ok(n) => {
                out.write_all(&buf[..n]).map_err(TailError::Output)?;
                offset += n as u64;
            }
            Err(errno) => {
                diagnose(err, &format!("error reading '{path}': {errno}"))?;
                break;
            }
        }
    }
    Ok(offset)
}

/// The parent directory of a path, for the name-follow directory watch.
/// `.`/empty for a bare filename.
fn parent_dir(path: &str) -> &str {
    let trimmed = path.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(0) => "/",
        Some(idx) => &trimmed[..idx],
        None => ".",
    }
}

/// Open a named source, emit its initial tail, register the watch(es), and
/// record its follow state. A source that cannot be opened is diagnosed; it
/// is still recorded (for `--retry`) so the follow loop can reopen it.
/// Returns `(served-cleanly, leading-units-omitted)`.
#[allow(clippy::too_many_arguments)]
fn open_and_emit_initial(
    job: &Job,
    mode: Follow,
    watcher: &dyn Watcher,
    path: &str,
    out: &dyn Output,
    err: &dyn Output,
    show_headers: bool,
    header_written: &mut bool,
    active: &mut Option<usize>,
    index: usize,
    sources: &mut Vec<FollowSource>,
) -> Result<(bool, u64), TailError> {
    // The parent-directory watch (name follow) makes a rotation wake the
    // engine immediately; a failure to open it is non-fatal (the loop still
    // re-checks on its bounded timeout).
    let dir = if mode == Follow::Name {
        match watcher.open_dir(parent_dir(path)) {
            Ok(d) => {
                let _ = watcher.watch(d);
                Some(d)
            }
            Err(_) => None,
        }
    } else {
        None
    };

    match watcher.open(path) {
        Ok(handle) => {
            if show_headers {
                write_header(out, path, header_written).map_err(TailError::Output)?;
                *active = Some(index);
            }
            let mut engine = Engine::new(job);
            let mut buf = vec![0u8; READ_CHUNK];
            let mut offset = 0u64;
            loop {
                match watcher.read_at(handle, offset, &mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        engine.feed(&buf[..n], out).map_err(TailError::Output)?;
                        offset += n as u64;
                    }
                    Err(errno) => {
                        diagnose(err, &format!("error reading '{path}': {errno}"))?;
                        break;
                    }
                }
            }
            engine.finish(out).map_err(TailError::Output)?;
            let id = watcher.meta(handle).map_or(FileId::NONE, |m| m.id);
            let _ = watcher.watch(handle);
            sources.push(FollowSource {
                path: String::from(path),
                handle: Some(handle),
                dir,
                id,
                offset,
                unchanged: 0,
            });
            Ok((true, engine.omitted))
        }
        Err(errno) => {
            diagnose(err, &format!("cannot open '{path}' for reading: {errno}"))?;
            sources.push(FollowSource {
                path: String::from(path),
                handle: None,
                dir,
                id: FileId::NONE,
                offset: 0,
                unchanged: 0,
            });
            Ok((false, 0))
        }
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use alloc::collections::BTreeMap;
    use alloc::string::{String, ToString};
    use alloc::vec::Vec;
    use core::cell::RefCell;

    use tairix_abi::{Errno, FileId};

    use super::run_follow;
    use crate::command::{parse, Command, Follow, Job};
    use crate::io::{Info, Input, Meta, Output, Watcher};

    fn fid(node: u64) -> FileId {
        FileId {
            volume: [1u8; 16],
            node,
        }
    }

    /// One mutation the fake applies on a `block()` cycle.
    enum Mutation {
        /// Append bytes to the node currently at `path`.
        Append(&'static str, &'static [u8]),
        /// Set the length of the node currently at `path`.
        Truncate(&'static str, u64),
        /// Replace `path` with a brand-new node carrying `data` (rotation).
        Rotate(&'static str, u64, &'static [u8]),
        /// Create a new node at a previously-absent `path`.
        Create(&'static str, u64, &'static [u8]),
    }

    /// An in-memory fake modelling node identity: nodes live in `nodes`
    /// keyed by `FileId`; `names` maps a path to the node currently at it.
    /// A handle captures the node it opened (descriptor semantics), so a
    /// rotation of the name leaves an existing handle on the old node.
    struct FakeWatcher {
        nodes: RefCell<BTreeMap<u64, Vec<u8>>>,
        names: RefCell<BTreeMap<String, u64>>,
        handles: RefCell<BTreeMap<u64, u64>>,
        next_handle: RefCell<u64>,
        script: RefCell<Vec<Mutation>>,
        step: RefCell<usize>,
    }

    impl FakeWatcher {
        fn new(initial: &[(&str, u64, &[u8])], script: Vec<Mutation>) -> Self {
            let mut nodes = BTreeMap::new();
            let mut names = BTreeMap::new();
            for (path, node, data) in initial {
                nodes.insert(*node, data.to_vec());
                names.insert((*path).to_string(), *node);
            }
            Self {
                nodes: RefCell::new(nodes),
                names: RefCell::new(names),
                handles: RefCell::new(BTreeMap::new()),
                next_handle: RefCell::new(1),
                script: RefCell::new(script),
                step: RefCell::new(0),
            }
        }

        fn apply(&self, m: &Mutation) {
            match *m {
                Mutation::Append(path, bytes) => {
                    if let Some(&node) = self.names.borrow().get(path) {
                        self.nodes
                            .borrow_mut()
                            .entry(node)
                            .or_default()
                            .extend_from_slice(bytes);
                    }
                }
                Mutation::Truncate(path, len) => {
                    if let Some(&node) = self.names.borrow().get(path) {
                        if let Some(data) = self.nodes.borrow_mut().get_mut(&node) {
                            data.truncate(usize::try_from(len).unwrap_or(usize::MAX));
                        }
                    }
                }
                // Rotation and creation are the same operation on the fake:
                // install a fresh node and point the name at it.
                Mutation::Rotate(path, node, data) | Mutation::Create(path, node, data) => {
                    self.nodes.borrow_mut().insert(node, data.to_vec());
                    self.names.borrow_mut().insert(path.to_string(), node);
                }
            }
        }
    }

    impl Watcher for FakeWatcher {
        fn open(&self, path: &str) -> Result<u64, Errno> {
            let node = *self.names.borrow().get(path).ok_or(Errno::NotFound)?;
            let h = *self.next_handle.borrow();
            *self.next_handle.borrow_mut() += 1;
            self.handles.borrow_mut().insert(h, node);
            Ok(h)
        }

        fn open_dir(&self, _path: &str) -> Result<u64, Errno> {
            // The tests drive rotation through the bounded timeout re-check,
            // so the directory watch is not needed; report it absent.
            Err(Errno::NotFound)
        }

        fn close(&self, handle: u64) {
            self.handles.borrow_mut().remove(&handle);
        }

        fn read_at(&self, handle: u64, offset: u64, buf: &mut [u8]) -> Result<usize, Errno> {
            let node = *self.handles.borrow().get(&handle).ok_or(Errno::NotFound)?;
            let nodes = self.nodes.borrow();
            let data = nodes.get(&node).ok_or(Errno::NotFound)?;
            let start = usize::try_from(offset).unwrap_or(usize::MAX);
            if start >= data.len() {
                return Ok(0);
            }
            let n = buf.len().min(data.len() - start);
            buf[..n].copy_from_slice(&data[start..start + n]);
            Ok(n)
        }

        fn meta(&self, handle: u64) -> Result<Meta, Errno> {
            let node = *self.handles.borrow().get(&handle).ok_or(Errno::NotFound)?;
            let nodes = self.nodes.borrow();
            let data = nodes.get(&node).ok_or(Errno::NotFound)?;
            Ok(Meta {
                id: fid(node),
                size: data.len() as u64,
            })
        }

        fn meta_path(&self, path: &str) -> Result<Meta, Errno> {
            let node = *self.names.borrow().get(path).ok_or(Errno::NotFound)?;
            let nodes = self.nodes.borrow();
            let data = nodes.get(&node).ok_or(Errno::NotFound)?;
            Ok(Meta {
                id: fid(node),
                size: data.len() as u64,
            })
        }

        fn watch(&self, _handle: u64) -> Result<(), Errno> {
            Ok(())
        }

        fn unwatch(&self, _handle: u64) {}

        fn block(&self, _timeout_ns: u64) {
            let step = *self.step.borrow();
            if let Some(m) = self.script.borrow().get(step) {
                self.apply(m);
            }
            *self.step.borrow_mut() = step + 1;
        }

        fn pid_alive(&self, _pid: u64) -> bool {
            // Alive while scripted mutations remain; once exhausted the loop
            // performs its final drain and exits.
            *self.step.borrow() < self.script.borrow().len()
        }
    }

    #[derive(Default)]
    struct Sink(RefCell<Vec<u8>>);

    impl Output for Sink {
        fn write_all(&self, bytes: &[u8]) -> Result<(), Errno> {
            self.0.borrow_mut().extend_from_slice(bytes);
            Ok(())
        }
    }

    struct NoStdin;

    impl Input for NoStdin {
        fn read(&self, _buf: &mut [u8]) -> Result<usize, Errno> {
            Ok(0)
        }
    }

    #[derive(Default)]
    struct NoInfo;

    impl Info for NoInfo {
        fn emit(&self, _record: &[u8]) {}
    }

    fn job(args: &[&str]) -> (Job, Follow) {
        match parse(args).expect("parses") {
            Command::Tail(job) => {
                let mode = job.follow.expect("a follow mode");
                (job, mode)
            }
            Command::Help => panic!("expected a tail job"),
        }
    }

    /// Run a follow to completion (the fake's `--pid` ends it) and return
    /// (all-ok, stdout, stderr).
    fn run_case(
        args: &[&str],
        initial: &[(&str, u64, &[u8])],
        script: Vec<Mutation>,
    ) -> (bool, Vec<u8>, String) {
        let (job, mode) = job(args);
        let watcher = FakeWatcher::new(initial, script);
        let out = Sink::default();
        let err = Sink::default();
        let ok = run_follow(&job, mode, &watcher, &NoStdin, &out, &err, &NoInfo)
            .expect("fixture streams never fail");
        let out_bytes = out.0.borrow().clone();
        let err_bytes = err.0.borrow().clone();
        (
            ok,
            out_bytes,
            String::from_utf8(err_bytes).expect("stderr utf8"),
        )
    }

    #[test]
    fn appended_bytes_are_emitted_as_the_file_grows() {
        let (ok, out, _) = run_case(
            &["-f", "-n", "1", "--pid=1", "f"],
            &[("f", 10, b"a\nb\n")],
            std::vec![Mutation::Append("f", b"c\n"), Mutation::Append("f", b"d\n")],
        );
        assert!(ok);
        // Initial last line `b\n`, then each appended line as it arrives.
        assert_eq!(out, b"b\nc\nd\n");
    }

    #[test]
    fn truncation_is_reported_and_re_followed_from_zero() {
        let (ok, out, err) = run_case(
            &["-f", "-c", "1", "--pid=1", "f"],
            &[("f", 10, b"abcdef")],
            std::vec![Mutation::Truncate("f", 0), Mutation::Append("f", b"XY"),],
        );
        assert!(ok);
        // Initial last byte `f`; after truncation the new content `XY`.
        assert_eq!(out, b"fXY");
        assert!(err.contains("f: file truncated"), "stderr: {err}");
    }

    #[test]
    fn name_follow_reopens_a_rotated_file() {
        let (ok, out, err) = run_case(
            &["-F", "--pid=1", "f"],
            &[("f", 10, b"old\n")],
            // Rotate `f` to a new node with fresh content, then append to it.
            std::vec![
                Mutation::Rotate("f", 20, b"new\n"),
                Mutation::Append("f", b"more\n"),
            ],
        );
        assert!(ok);
        // Initial `old\n` (last 10 lines = whole file), then the rotated
        // file's `new\n` from its start, then its appended `more\n`.
        assert_eq!(out, b"old\nnew\nmore\n");
        assert!(err.contains("has been replaced"), "stderr: {err}");
    }

    #[test]
    fn retry_opens_a_name_that_appears() {
        let (ok, out, err) = run_case(
            &["-F", "--pid=1", "later"],
            // `later` is absent initially.
            &[],
            std::vec![
                Mutation::Create("later", 30, b"hi\n"),
                Mutation::Append("later", b"there\n"),
            ],
        );
        // The initial open failed (all-ok is false), but retry then follows.
        assert!(!ok);
        assert_eq!(out, b"hi\nthere\n");
        assert!(
            err.contains("cannot open 'later'") && err.contains("has appeared"),
            "stderr: {err}"
        );
    }

    #[test]
    fn pid_death_ends_the_follow() {
        // An empty script: the process is already gone, so the follow does
        // one drain of the initial state and returns.
        let (ok, out, _) = run_case(
            &["-f", "-n", "1", "--pid=1", "f"],
            &[("f", 10, b"x\ny\n")],
            std::vec![],
        );
        assert!(ok);
        assert_eq!(out, b"y\n");
    }
}
