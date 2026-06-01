//! In-memory test doubles for the [`Console`] and [`ProcessHost`] seams.
//!
//! These fixtures let every unit and integration test drive the real shell
//! logic without a kernel: the [`RecordingConsole`] captures output, and the
//! [`ScriptedHost`] answers `launch`/`wait`/`signal`/`change_directory` from a
//! programmable script while recording what it was asked to do.

use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::cell::RefCell;

use rustos_abi::Errno;

use crate::host::{Console, LaunchSpec, ProcessHost, ResolvedCommand};
use crate::job::{Pid, Signal, WaitOutcome};

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
    fail_launch: RefCell<Option<String>>,
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
        *self.fail_launch.borrow_mut() = Some(name.to_string());
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
        if let Some(name) = self.fail_launch.borrow().as_ref() {
            let first = spec.commands.first().and_then(|c| c.argv.first());
            if first == Some(name) {
                return Err(Errno::NotFound);
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
