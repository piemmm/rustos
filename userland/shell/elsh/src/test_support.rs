//! In-memory test doubles for the [`Console`] and [`ProcessHost`] seams.
//!
//! These fixtures let every unit and integration test drive the real shell
//! logic without a kernel: the [`RecordingConsole`] captures output, the
//! [`ScriptedHost`] answers `launch`/`wait`/`signal`/`change_directory` from a
//! programmable script while recording what it was asked to do, and the
//! [`Fixture`] bundles them behind a real [`BuiltinContext`] dispatch — the
//! one scaffolding every builtin's test module shares.

use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::cell::RefCell;

use tairix_abi::{Errno, LimitKind, ResourceLimit};

use crate::builtin::{dispatch, BuiltinContext};
use crate::env::Environment;
use crate::host::{
    Console, Elevator, LaunchSpec, LimitStore, ProcessHost, ResolvedCommand, NULL_ELEVATOR,
};
use crate::job::{JobTable, Pid, Signal, WaitOutcome};

/// A [`Console`] that accumulates everything written to each stream.
#[derive(Default)]
pub(crate) struct RecordingConsole {
    out: RefCell<String>,
    err: RefCell<String>,
}

impl RecordingConsole {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn stdout(&self) -> String {
        self.out.borrow().clone()
    }

    pub(crate) fn stderr(&self) -> String {
        self.err.borrow().clone()
    }

    pub(crate) fn clear(&self) {
        self.out.borrow_mut().clear();
        self.err.borrow_mut().clear();
    }
}

impl Console for RecordingConsole {
    fn write_stdout(&self, text: &str) {
        self.out.borrow_mut().push_str(text);
    }

    fn write_stderr(&self, text: &str) {
        self.err.borrow_mut().push_str(text);
    }
}

/// One thing the [`ScriptedHost`] was asked to launch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct LaunchRecord {
    pub commands: Vec<ResolvedCommand>,
    pub env: Vec<(String, String)>,
    pub background: bool,
}

/// A programmable [`ProcessHost`].
pub(crate) struct ScriptedHost {
    next_pid: RefCell<u64>,
    launches: RefCell<Vec<LaunchRecord>>,
    fail_launch: RefCell<Option<(String, Errno)>>,
    waits: RefCell<BTreeMap<u64, WaitOutcome>>,
    signals: RefCell<Vec<(Pid, Signal)>>,
    directories: RefCell<Vec<String>>,
    poll_queue: RefCell<Vec<(Pid, WaitOutcome)>>,
}

impl ScriptedHost {
    pub(crate) fn new() -> Self {
        Self {
            next_pid: RefCell::new(100),
            launches: RefCell::new(Vec::new()),
            fail_launch: RefCell::new(None),
            waits: RefCell::new(BTreeMap::new()),
            signals: RefCell::new(Vec::new()),
            directories: RefCell::new(Vec::new()),
            poll_queue: RefCell::new(Vec::new()),
        }
    }

    /// Make the next `launch` whose `argv[0]` equals `name` fail with
    /// [`Errno::NotFound`].
    pub(crate) fn fail_launch_of(&self, name: &str) {
        self.fail_launch_with(name, Errno::NotFound);
    }

    /// Make the next `launch` whose `argv[0]` equals `name` fail with
    /// `errno` (e.g. [`Errno::PermissionDenied`] for a command that
    /// resolved but is refused).
    pub(crate) fn fail_launch_with(&self, name: &str, errno: Errno) {
        *self.fail_launch.borrow_mut() = Some((name.to_string(), errno));
    }

    /// Register the outcome `wait` returns for a given pid.
    pub(crate) fn set_wait(&self, pid: Pid, outcome: WaitOutcome) {
        self.waits.borrow_mut().insert(pid.as_u64(), outcome);
    }

    /// Mark a directory the host will accept in `change_directory`.
    pub(crate) fn set_directory(&self, path: &str) {
        self.directories.borrow_mut().push(path.to_string());
    }

    /// Queue a background state change for `poll` to report.
    pub(crate) fn queue_poll(&self, pid: Pid, outcome: WaitOutcome) {
        self.poll_queue.borrow_mut().push((pid, outcome));
    }

    pub(crate) fn launches(&self) -> Vec<LaunchRecord> {
        self.launches.borrow().clone()
    }

    pub(crate) fn last_signal(&self) -> Option<(Pid, Signal)> {
        self.signals.borrow().last().copied()
    }
}

impl ProcessHost for ScriptedHost {
    fn launch(&self, spec: &LaunchSpec<'_>) -> Result<Pid, Errno> {
        if let Some((name, errno)) = self.fail_launch.borrow().as_ref() {
            let first = spec.commands.first().and_then(|c| c.argv.first());
            if first == Some(name) {
                return Err(*errno);
            }
        }
        self.launches.borrow_mut().push(LaunchRecord {
            commands: spec.commands.to_vec(),
            env: spec
                .env
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect(),
            background: spec.background,
        });
        let mut next = self.next_pid.borrow_mut();
        let pid = Pid::new(*next);
        *next += 1;
        Ok(pid)
    }

    fn wait(&self, pid: Pid) -> Result<WaitOutcome, Errno> {
        self.waits
            .borrow()
            .get(&pid.as_u64())
            .copied()
            .map_or(Ok(WaitOutcome::Exited(0)), Ok)
    }

    fn signal(&self, pid: Pid, signal: Signal) -> Result<(), Errno> {
        self.signals.borrow_mut().push((pid, signal));
        Ok(())
    }

    fn poll(&self) -> Option<(Pid, WaitOutcome)> {
        let mut queue = self.poll_queue.borrow_mut();
        if queue.is_empty() {
            None
        } else {
            Some(queue.remove(0))
        }
    }

    fn change_directory(&self, path: &str) -> Result<String, Errno> {
        if self.directories.borrow().iter().any(|d| d == path) {
            Ok(path.to_string())
        } else {
            Err(Errno::NotFound)
        }
    }
}

/// An in-memory [`LimitStore`]: a per-resource [`ResourceLimit`] map that lets
/// `ulimit` tests drive the real builtin logic without a kernel.
///
/// An unset resource reads back [`ResourceLimit::UNLIMITED`], matching the
/// kernel's default-unlimited starting point. [`put`] and
/// [`snapshot`] are test-only direct accessors that bypass the gating
/// [`set`](LimitStore::set) applies, so a test can arrange or inspect state;
/// [`deny_set`] makes the next [`set`](LimitStore::set) fail with a chosen
/// [`Errno`] to exercise the kernel-side raise gate (`CAP_RLIMIT_RAISE`).
///
/// [`put`]: MemoryLimitStore::put
/// [`snapshot`]: MemoryLimitStore::snapshot
/// [`deny_set`]: MemoryLimitStore::deny_set
pub(crate) struct MemoryLimitStore {
    limits: RefCell<BTreeMap<u32, ResourceLimit>>,
    deny: RefCell<Option<Errno>>,
}

impl MemoryLimitStore {
    pub(crate) fn new() -> Self {
        Self {
            limits: RefCell::new(BTreeMap::new()),
            deny: RefCell::new(None),
        }
    }

    /// Seed the stored limit for `kind` directly (test setup; bypasses the
    /// raise gate `set` applies).
    pub(crate) fn put(&self, kind: LimitKind, limit: ResourceLimit) {
        self.limits.borrow_mut().insert(kind.as_u32(), limit);
    }

    /// The currently-stored limit for `kind`, or [`ResourceLimit::UNLIMITED`]
    /// if none was set.
    pub(crate) fn snapshot(&self, kind: LimitKind) -> ResourceLimit {
        self.limits
            .borrow()
            .get(&kind.as_u32())
            .copied()
            .unwrap_or(ResourceLimit::UNLIMITED)
    }

    /// Make the next [`set`](LimitStore::set) fail closed with `errno`,
    /// modelling the kernel refusing to raise a hard bound.
    pub(crate) fn deny_set(&self, errno: Errno) {
        *self.deny.borrow_mut() = Some(errno);
    }
}

impl LimitStore for MemoryLimitStore {
    fn get(&self, kind: LimitKind) -> Result<ResourceLimit, Errno> {
        Ok(self.snapshot(kind))
    }

    fn set(&self, kind: LimitKind, value: ResourceLimit) -> Result<(), Errno> {
        if let Some(errno) = *self.deny.borrow() {
            return Err(errno);
        }
        self.put(kind, value);
        Ok(())
    }
}

/// The one builtin test fixture: real [`Environment`] and
/// [`JobTable`] state over the in-memory seams, dispatching
/// through the production [`BuiltinContext`] path.
pub(crate) struct Fixture<'a> {
    pub env: Environment,
    pub jobs: JobTable,
    pub host: ScriptedHost,
    pub console: RecordingConsole,
    pub limits: MemoryLimitStore,
    pub elevator: &'a dyn Elevator,
    pub exit: Option<i32>,
}

impl Fixture<'static> {
    /// A fixture with the fail-closed default [`Elevator`].
    pub(crate) fn new() -> Self {
        Self::with_elevator(&NULL_ELEVATOR)
    }
}

impl<'a> Fixture<'a> {
    /// A fixture whose `elevate` builtin drives `elevator`.
    pub(crate) fn with_elevator(elevator: &'a dyn Elevator) -> Self {
        Self {
            env: Environment::new(),
            jobs: JobTable::new(),
            host: ScriptedHost::new(),
            console: RecordingConsole::new(),
            limits: MemoryLimitStore::new(),
            elevator,
            exit: None,
        }
    }

    /// Dispatch `words` (the full argv, name first) as a builtin line,
    /// returning its status — `None` when `words[0]` is not a builtin.
    pub(crate) fn run(&mut self, words: &[&str]) -> Option<i32> {
        let argv: Vec<String> = words.iter().map(|w| (*w).to_string()).collect();
        let mut ctx = BuiltinContext {
            env: &mut self.env,
            jobs: &mut self.jobs,
            host: &self.host,
            console: &self.console,
            limits: &self.limits,
            elevator: self.elevator,
            exit: &mut self.exit,
        };
        dispatch(&mut ctx, &argv)
    }
}

/// A [`crate::complete::DirLister`] with no filesystem: every listing is
/// refused, so completion degrades to the non-filesystem candidates. The
/// default lister for tests that do not exercise completion.
pub(crate) struct EmptyLister;

impl crate::complete::DirLister for EmptyLister {
    fn list_dir(&self, _dir: &str) -> Result<Vec<crate::complete::DirEntryInfo>, Errno> {
        Err(Errno::NotImplemented)
    }
}
