//! Bookkeeping for the desktop's launched children and operator-facing
//! diagnosis for a launch that failed to load.
//!
//! Asynchronous process launch (`plans/FIX-DESKTOP.md`) admits a child and
//! returns its PID to the desktop immediately; the load — the VFS read of the
//! bundle, the signature/hash verification, and the address-space build —
//! then runs on the *child's own* task, so the compositor loop never freezes
//! behind it. A refusal therefore no longer comes back as the `spawn` return
//! value; it surfaces as the child's **exit status**, a reserved `LOAD_*`
//! code that `lib/abi` places in a high, loader-only band a normal program's
//! own `exit(code)` never lands in.
//!
//! [`LaunchTable`] remembers every launched child still running: its PID, the
//! display name the desktop reports it by, and the `Run` path it was spawned
//! from. The name feeds the fail-loud load-refusal diagnosis below; the path
//! is the child's *bundle identity*, which the file manager's idempotent open
//! resolves against — a press on its strip slot raises the running file
//! manager's window instead of spawning a second copy. Identity by recorded
//! spawn path is attested by construction: the desktop spawned the child
//! itself, so no window title or other app-controlled data is ever trusted
//! for it.
//!
//! The desktop reaps every exited child on its `CHILD_TOKEN` wait-set member.
//! [`launch_failure_report`] turns a reaped status into the terse line the
//! session prints on `stderr`, so a refused launch is a loud, non-fatal
//! diagnosis (the charter's fail-loud rule) instead of a window that silently
//! never appears. The reason wording is `lib/abi`'s shared mapping, so the
//! desktop and any other launcher describe the same cause identically.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

use tairix_abi::{load_failure_reason, WaitStatus};

use crate::apps::BUNDLE_RUN_SUFFIX;

/// How the desktop asks a live instance to open, once a launch has resolved
/// to reuse it rather than start a second process.
///
/// Attempted in this order, because each step is the honest answer for one
/// less capable instance than the last: the target is the specific thing the
/// user asked for, the icon-bar default is what the application itself says a
/// bare launch means, and raising its most recent window is all a desktop can
/// do for an application that says nothing.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Handover {
    /// Queue the launch's named path for the instance and wake it.
    OpenTarget,
    /// Ask the instance for its icon-bar default action — a new window.
    Default,
    /// Raise the instance's most recent window: it declared no icon-bar
    /// presence, so it has no default action to be asked for.
    Raise,
}

/// What a launch of a bundle resolved to.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Launch {
    /// Start a fresh process from the bundle's `Run` binary.
    Spawn,
    /// The live instance `pid` took the request through `by`.
    Reused {
        /// The instance that took it.
        pid: u64,
        /// How it was reached.
        by: Handover,
    },
}

/// The effects a launch needs, so the rule below is one host-testable
/// decision and the syscalls stay in the `Run` binary.
///
/// Each method answers whether the *engine* accepted the hand-over, which is
/// load-bearing rather than advisory: the desktop re-routes a launch only
/// when something actually took it, and falls back to spawning when nothing
/// did. Every one of them is a message to a live process; none waits on it.
pub trait LaunchHost {
    /// Queue `path` for the live instance `pid` to open and wake it.
    fn queue_open_target(&mut self, pid: u64, path: &str) -> bool;

    /// Ask the live instance `pid` for its icon-bar default action.
    /// `false` when it declared no icon-bar presence to ask.
    fn ask_default(&mut self, pid: u64) -> bool;

    /// Raise the live instance `pid`'s most recent window. `false` when it
    /// owns none.
    fn raise_recent_window(&mut self, pid: u64) -> bool;
}

/// Resolve a launch of the bundle whose entry binary is `run_path`.
///
/// `running` is the live instance the [`LaunchTable`] holds for that path, and
/// `one_instance` what the bundle's signed manifest attests. `target` is the
/// document or folder the launch named, if any.
///
/// Singleton is the default, so this is what a user means by clicking a
/// program they already have open: the running instance is asked to open,
/// rather than a second process starting. A bundle that declares itself
/// multi-instance, or has no live instance, spawns.
///
/// **Fails closed to the old behaviour.** An instance that cannot be reached
/// at all — no window, no icon-bar presence, a mailbox that has gone — is
/// spawned instead, so a launch never silently does nothing.
pub fn resolve_launch<H: LaunchHost>(
    host: &mut H,
    running: Option<u64>,
    one_instance: bool,
    target: Option<&str>,
) -> Launch {
    let Some(pid) = running.filter(|_| one_instance) else {
        return Launch::Spawn;
    };
    if let Some(path) = target {
        if host.queue_open_target(pid, path) {
            return Launch::Reused {
                pid,
                by: Handover::OpenTarget,
            };
        }
    }
    if host.ask_default(pid) {
        return Launch::Reused {
            pid,
            by: Handover::Default,
        };
    }
    if host.raise_recent_window(pid) {
        return Launch::Reused {
            pid,
            by: Handover::Raise,
        };
    }
    Launch::Spawn
}

/// The label used for a reaped child the launcher did not record (it should
/// not happen — every desktop child is recorded at launch — but a diagnosis
/// must never be dropped for want of a name).
const UNKNOWN_LABEL: &str = "an application";

/// One launched child still running: how the desktop names it and the `Run`
/// path it was spawned from.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LaunchedApp {
    /// The display name launch diagnostics report the child by.
    pub label: String,
    /// The bundle entry-point path the child was spawned from — its
    /// attested bundle identity.
    pub run_path: String,
}

/// Every child the desktop has launched and not yet reaped, keyed by PID.
///
/// An entry is recorded when a launch is admitted and removed when the child
/// is reaped, so the table never grows beyond the children currently alive.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct LaunchTable {
    children: BTreeMap<u64, LaunchedApp>,
}

impl LaunchTable {
    /// An empty table.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record the admitted child `pid`, launched from `run_path` and
    /// reported as `label`.
    pub fn record(&mut self, pid: u64, label: &str, run_path: &str) {
        self.children.insert(
            pid,
            LaunchedApp {
                label: String::from(label),
                run_path: String::from(run_path),
            },
        );
    }

    /// Forget (and return) the record for a reaped `pid`.
    pub fn remove(&mut self, pid: u64) -> Option<LaunchedApp> {
        self.children.remove(&pid)
    }

    /// The lowest-numbered running child spawned from `run_path`, if any —
    /// how the file manager's strip slot finds its already-running instance.
    #[must_use]
    pub fn running_from(&self, run_path: &str) -> Option<u64> {
        self.children
            .iter()
            .find(|(_, app)| app.run_path == run_path)
            .map(|(&pid, _)| pid)
    }

    /// The recorded launch of running child `pid`, if this table launched
    /// it — how the Switchboard panel's restart action resolves an owner's
    /// kernel task id back to the bundle it was launched from.
    #[must_use]
    pub fn get(&self, pid: u64) -> Option<&LaunchedApp> {
        self.children.get(&pid)
    }

    /// The number of children still recorded.
    #[must_use]
    pub fn len(&self) -> usize {
        self.children.len()
    }

    /// `true` when no launched child is recorded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.children.is_empty()
    }

    /// The bundle directory of every live child launched from one, in pid
    /// order.
    ///
    /// A child spawned from a bare program rather than a bundle contributes
    /// nothing: its `run_path` has no `Run` leaf to strip, so there is no
    /// bundle whose manifest could name an icon.
    pub fn bundles(&self) -> impl Iterator<Item = &str> {
        self.children
            .values()
            .filter_map(|app| app.run_path.strip_suffix(BUNDLE_RUN_SUFFIX))
    }
}

/// The `stderr` line for a reaped child `label` that exited with a reserved
/// loader-failure status, or `None` when the exit is not a load refusal.
///
/// A clean exit (`Exited(0)`), any ordinary non-zero exit outside the
/// reserved `LOAD_*` band, and a stop all return `None` — they are the
/// launched app's own business, not a launch failure the desktop reports.
/// The returned line is newline-terminated and ready to hand to `stderr`.
#[must_use]
pub fn launch_failure_report(label: &str, status: WaitStatus) -> Option<String> {
    let WaitStatus::Exited(code) = status else {
        return None;
    };
    let reason = load_failure_reason(code)?;
    Some(alloc::format!(
        "desktop: {label} failed to launch: {reason}\n"
    ))
}

/// The kernel task id a spawn returned, or `None` when the result is not
/// one.
///
/// Task ids start at 1, so a non-positive result is the kernel's refusal
/// rather than a child: recording `0` as a pid would leave the launch
/// table holding an entry no process can ever answer for or reap.
#[must_use]
pub fn admitted_pid(ret: i64) -> Option<u64> {
    u64::try_from(ret).ok().filter(|pid| *pid != 0)
}

/// The argument vector a desktop launch is spawned with: the program's own
/// path first, then `args`.
///
/// A program reads its arguments from index 1, because index 0 is the
/// program name its spawner chose (`tairix_rt::args`). An argument passed
/// first is read as that name and never seen at all, so a launch that omits
/// the program silently loses its leading operand — the file manager's
/// component-role switch, the folder a desktop icon opens, the document an
/// icon's launch names.
#[must_use]
pub fn launch_argv<'a>(program: &'a [u8], args: &[&'a [u8]]) -> Vec<&'a [u8]> {
    core::iter::once(program)
        .chain(args.iter().copied())
        .collect()
}

/// Drain every currently-exited child, reporting each load refusal and
/// handing every reaped PID back to the caller for its own teardown.
///
/// This is the whole of the desktop's `CHILD_TOKEN` handling, factored out of
/// the freestanding `Run` loop so it is exercised on the host by real tests
/// rather than only in a booted image. The three seams are injected so the
/// pure control flow — drain until empty, drop the table entry, report a
/// reserved-status refusal, forward the PID — is identical in production and
/// under test:
///
/// * `reap` performs one non-blocking reap, returning `Some((pid, status))`
///   for a reaped zombie or `None` when none remains. In production it wraps
///   the kernel `wait`; in tests it replays a scripted sequence.
/// * `report` receives each `stderr` diagnosis line (already newline-
///   terminated). In production it writes `stderr`; in tests it records the
///   lines.
/// * `teardown` receives every reaped PID so the caller can tear that child's
///   windows down. It runs for *every* reaped child, load-failure or not — a
///   child that launched successfully and later exited is torn down here too.
///
/// Only an exit reaps. A `Stopped` child is still alive and still holds
/// its windows, so dropping its record or tearing its windows down would
/// destroy a running program's screen and orphan it from the table.
///
/// Reaping drains fully: a non-blocking reap that returns `None` ends the
/// drain, so a burst of exits is handled in one wake and nothing is left for
/// a later poll — never a busy-wait.
pub fn reap_launched<R, P, T>(
    launched: &mut LaunchTable,
    mut reap: R,
    mut report: P,
    mut teardown: T,
) where
    R: FnMut() -> Option<(u64, WaitStatus)>,
    P: FnMut(&str),
    T: FnMut(u64),
{
    while let Some((pid, status)) = reap() {
        let WaitStatus::Exited(_) = status else {
            continue;
        };
        let label = launched.remove(pid);
        let label = label
            .as_ref()
            .map_or(UNKNOWN_LABEL, |app| app.label.as_str());
        if let Some(line) = launch_failure_report(label, status) {
            report(&line);
        }
        teardown(pid);
    }
}

#[cfg(test)]
mod tests {
    use super::{
        admitted_pid, launch_argv, launch_failure_report, reap_launched, resolve_launch, Handover,
        Launch, LaunchHost, LaunchTable, UNKNOWN_LABEL,
    };
    use alloc::string::String;
    use alloc::vec::Vec;
    use tairix_abi::{
        Signal, WaitStatus, LOAD_MALFORMED, LOAD_NOT_FOUND, LOAD_OOM, LOAD_UNVERIFIED,
    };

    /// A host recording every hand-over attempt, each of which may be
    /// configured to refuse — which is what an unreachable instance is.
    #[derive(Default)]
    struct FakeHost {
        queued: Vec<(u64, String)>,
        defaults: Vec<u64>,
        raises: Vec<u64>,
        takes_target: bool,
        takes_default: bool,
        has_window: bool,
    }

    impl FakeHost {
        /// A host whose instance takes everything asked of it.
        fn reachable() -> Self {
            Self {
                takes_target: true,
                takes_default: true,
                has_window: true,
                ..Self::default()
            }
        }
    }

    impl LaunchHost for FakeHost {
        fn queue_open_target(&mut self, pid: u64, path: &str) -> bool {
            self.queued.push((pid, String::from(path)));
            self.takes_target
        }

        fn ask_default(&mut self, pid: u64) -> bool {
            self.defaults.push(pid);
            self.takes_default
        }

        fn raise_recent_window(&mut self, pid: u64) -> bool {
            self.raises.push(pid);
            self.has_window
        }
    }

    #[test]
    fn a_singleton_relaunch_reaches_the_running_instance_rather_than_spawning() {
        let mut host = FakeHost::reachable();
        assert_eq!(
            resolve_launch(&mut host, Some(42), true, None),
            Launch::Reused {
                pid: 42,
                by: Handover::Default
            },
            "an argument-less relaunch asks the instance for a new window"
        );
        assert_eq!(host.defaults, [42]);
        assert!(host.queued.is_empty(), "no document was named");
        assert!(
            host.raises.is_empty(),
            "the default answered, so nothing else"
        );
    }

    #[test]
    fn a_document_relaunch_queues_the_target_rather_than_asking_for_a_window() {
        let mut host = FakeHost::reachable();
        assert_eq!(
            resolve_launch(&mut host, Some(42), true, Some("Users:/ada/report")),
            Launch::Reused {
                pid: 42,
                by: Handover::OpenTarget
            }
        );
        assert_eq!(host.queued, [(42, String::from("Users:/ada/report"))]);
        assert!(
            host.defaults.is_empty(),
            "the target is the specific thing asked for; a new window is not"
        );
    }

    #[test]
    fn an_instance_with_no_icon_bar_presence_has_its_window_raised() {
        let mut host = FakeHost {
            has_window: true,
            ..FakeHost::default()
        };
        assert_eq!(
            resolve_launch(&mut host, Some(7), true, None),
            Launch::Reused {
                pid: 7,
                by: Handover::Raise
            }
        );
        assert_eq!(host.defaults, [7], "the default is tried first");
        assert_eq!(host.raises, [7]);
    }

    #[test]
    fn a_multi_instance_bundle_spawns_again_however_many_are_running() {
        let mut host = FakeHost::reachable();
        assert_eq!(
            resolve_launch(&mut host, Some(42), false, None),
            Launch::Spawn
        );
        assert_eq!(
            resolve_launch(&mut host, Some(42), false, Some("Users:/ada/report")),
            Launch::Spawn
        );
        assert!(
            host.queued.is_empty() && host.defaults.is_empty() && host.raises.is_empty(),
            "a bundle that declares independent instances is never asked"
        );
    }

    #[test]
    fn a_bundle_with_no_live_instance_spawns_without_asking_anything() {
        let mut host = FakeHost::reachable();
        assert_eq!(resolve_launch(&mut host, None, true, None), Launch::Spawn);
        assert_eq!(
            resolve_launch(&mut host, None, true, Some("Users:/ada/report")),
            Launch::Spawn
        );
        assert!(
            host.queued.is_empty() && host.defaults.is_empty(),
            "there is no instance to ask, so the manifest is never even needed"
        );
    }

    #[test]
    fn an_unreachable_instance_spawns_rather_than_silently_doing_nothing() {
        // No window, no icon-bar presence, and a queue that will not take a
        // target: every route refused, so the launch falls back to what it
        // did before the channel existed.
        let mut host = FakeHost::default();
        assert_eq!(
            resolve_launch(&mut host, Some(42), true, Some("Users:/ada/report")),
            Launch::Spawn
        );
        assert_eq!(host.queued.len(), 1, "each route is tried once");
        assert_eq!(host.defaults, [42]);
        assert_eq!(host.raises, [42]);
    }

    #[test]
    fn a_document_launch_falls_back_through_the_same_ladder() {
        // The queue refused but the instance is otherwise live: the user's
        // launch still reaches it, as a new window rather than as nothing.
        let mut host = FakeHost {
            takes_default: true,
            ..FakeHost::default()
        };
        assert_eq!(
            resolve_launch(&mut host, Some(42), true, Some("Users:/ada/report")),
            Launch::Reused {
                pid: 42,
                by: Handover::Default
            }
        );
    }

    #[test]
    fn every_reserved_status_reports_its_reason_named_by_label() {
        for code in [LOAD_NOT_FOUND, LOAD_UNVERIFIED, LOAD_MALFORMED, LOAD_OOM] {
            let line = launch_failure_report("Files", WaitStatus::Exited(code))
                .expect("a reserved load status must be reported");
            assert!(line.starts_with("desktop: Files failed to launch: "));
            assert!(line.ends_with('\n'));
            // The reason is the shared `lib/abi` wording, never fabricated.
            let reason = tairix_abi::load_failure_reason(code).unwrap();
            assert!(line.contains(reason));
        }
    }

    #[test]
    fn a_clean_exit_is_not_a_launch_failure() {
        assert_eq!(launch_failure_report("Files", WaitStatus::Exited(0)), None);
    }

    #[test]
    fn an_ordinary_nonzero_exit_is_not_a_launch_failure() {
        // A program that ran and chose its own non-zero code is not a load
        // refusal; the desktop must not misreport it as one.
        assert_eq!(
            launch_failure_report("Terminal", WaitStatus::Exited(1)),
            None
        );
        assert_eq!(
            launch_failure_report("Terminal", WaitStatus::Exited(90)),
            None
        );
    }

    #[test]
    fn a_stop_is_not_a_launch_failure() {
        assert_eq!(
            launch_failure_report("Viewer", WaitStatus::Stopped(Signal::Stop)),
            None
        );
    }

    /// The program is named first and every argument follows it, so nothing
    /// a launch passes is read as the program's own name and dropped.
    #[test]
    fn a_launch_names_the_program_before_its_arguments() {
        let files = &b"/System/Applications/files.app/Run"[..];
        assert_eq!(
            launch_argv(files, &[&b"--desktop"[..]]),
            alloc::vec![files, &b"--desktop"[..]],
        );
        assert_eq!(
            launch_argv(files, &[&b"/Users/ada/Documents"[..]]),
            alloc::vec![files, &b"/Users/ada/Documents"[..]],
        );
        assert_eq!(launch_argv(files, &[]), alloc::vec![files]);
    }

    #[test]
    fn the_table_resolves_a_running_child_by_its_run_path() {
        let mut launched = LaunchTable::new();
        assert!(launched.is_empty());
        launched.record(10, "Files", "/System/Applications/files.app/Run");
        launched.record(11, "Chess", "/Apps/chess.app/Run");
        assert_eq!(launched.len(), 2);

        assert_eq!(
            launched.running_from("/System/Applications/files.app/Run"),
            Some(10)
        );
        assert_eq!(launched.running_from("/Apps/editor.app/Run"), None);

        let reaped = launched.remove(10).expect("recorded");
        assert_eq!(reaped.label, "Files");
        assert_eq!(
            launched.running_from("/System/Applications/files.app/Run"),
            None
        );
        assert_eq!(launched.remove(10), None, "a reaped pid is forgotten");
    }

    /// `get` resolves a running child's own recorded launch by its kernel
    /// task id — the restart action's bundle lookup — and reports `None`
    /// for a pid this table never launched or has already reaped.
    #[test]
    fn get_resolves_a_running_child_by_its_pid() {
        let mut launched = LaunchTable::new();
        launched.record(11, "Chess", "/Apps/chess.app/Run");

        let app = launched.get(11).expect("recorded");
        assert_eq!(app.label, "Chess");
        assert_eq!(app.run_path, "/Apps/chess.app/Run");
        assert_eq!(
            launched.get(99),
            None,
            "an unrecorded pid resolves to nothing"
        );

        launched.remove(11);
        assert_eq!(launched.get(11), None, "a reaped pid is forgotten");
    }

    /// A scripted drain: two children exit in one wake — a `Files` launch
    /// that was refused (reserved `LOAD_UNVERIFIED`) and a `Terminal` that
    /// ran and exited cleanly. The refusal is reported (named), the clean
    /// exit is not, both table entries are dropped, and *both* PIDs are
    /// handed to teardown. The drain stops when the reap yields `None`.
    #[test]
    fn reap_drains_and_reports_only_the_refused_launch() {
        let mut launched = LaunchTable::new();
        launched.record(10, "Files", "/System/Applications/files.app/Run");
        launched.record(11, "Terminal", "/System/Applications/terminal.app/Run");

        let mut script = alloc::vec![
            (10u64, WaitStatus::Exited(LOAD_UNVERIFIED)),
            (11u64, WaitStatus::Exited(0)),
        ]
        .into_iter();
        let mut reported: Vec<String> = Vec::new();
        let mut torn_down: Vec<u64> = Vec::new();

        reap_launched(
            &mut launched,
            || script.next(),
            |line| reported.push(String::from(line)),
            |pid| torn_down.push(pid),
        );

        assert!(launched.is_empty(), "every reaped child is dropped");
        assert_eq!(
            torn_down,
            alloc::vec![10, 11],
            "every reaped pid is torn down"
        );
        assert_eq!(reported.len(), 1, "only the refused launch is reported");
        assert!(reported[0].starts_with("desktop: Files failed to launch: "));
        assert!(reported[0].contains(tairix_abi::load_failure_reason(LOAD_UNVERIFIED).unwrap()));
    }

    /// A refused child the launcher never recorded (an impossible-in-practice
    /// case the desktop still must not drop silently) is reported under the
    /// fallback label rather than not at all.
    #[test]
    fn an_unrecorded_refused_child_is_still_reported() {
        let mut launched = LaunchTable::new();
        let mut script = alloc::vec![(7u64, WaitStatus::Exited(LOAD_OOM))].into_iter();
        let mut reported: Vec<String> = Vec::new();
        let mut torn_down: Vec<u64> = Vec::new();

        reap_launched(
            &mut launched,
            || script.next(),
            |line| reported.push(String::from(line)),
            |pid| torn_down.push(pid),
        );

        assert_eq!(torn_down, alloc::vec![7]);
        assert_eq!(reported.len(), 1);
        assert!(reported[0].contains(UNKNOWN_LABEL));
    }

    /// A stopped child is alive and unreaped: its launch record and its
    /// windows both survive, and the drain moves on.
    #[test]
    fn a_stopped_child_keeps_its_record_and_its_windows() {
        let mut launched = LaunchTable::new();
        launched.record(10, "Viewer", "/Apps/viewer.app/Run");

        let mut script = alloc::vec![(10u64, WaitStatus::Stopped(Signal::Stop))].into_iter();
        let mut reported: Vec<String> = Vec::new();
        let mut torn_down: Vec<u64> = Vec::new();

        reap_launched(
            &mut launched,
            || script.next(),
            |line| reported.push(String::from(line)),
            |pid| torn_down.push(pid),
        );

        assert_eq!(launched.len(), 1, "a stopped child is still launched");
        assert!(
            torn_down.is_empty(),
            "a stopped child keeps the windows it is still holding"
        );
        assert!(reported.is_empty());
    }

    /// A stop does not stall the drain: the child that exited behind it is
    /// still reaped and torn down.
    #[test]
    fn a_stop_does_not_stop_the_drain() {
        let mut launched = LaunchTable::new();
        launched.record(10, "Viewer", "/Apps/viewer.app/Run");
        launched.record(11, "Terminal", "/System/Applications/terminal.app/Run");

        let mut script = alloc::vec![
            (10u64, WaitStatus::Stopped(Signal::Stop)),
            (11u64, WaitStatus::Exited(0)),
        ]
        .into_iter();
        let mut torn_down: Vec<u64> = Vec::new();

        reap_launched(
            &mut launched,
            || script.next(),
            |_| {},
            |pid| {
                torn_down.push(pid);
            },
        );

        assert_eq!(
            torn_down,
            alloc::vec![11],
            "only the exited child is torn down"
        );
        assert_eq!(launched.len(), 1);
        assert!(
            launched.get(10).is_some(),
            "the stopped child is still recorded"
        );
    }

    /// A kernel task id starts at 1, so only a positive spawn result names
    /// a child; `0` and every refusal answer `None`.
    #[test]
    fn only_a_positive_spawn_result_is_a_pid() {
        assert_eq!(admitted_pid(1), Some(1));
        assert_eq!(admitted_pid(4096), Some(4096));
        assert_eq!(
            admitted_pid(i64::MAX),
            Some(u64::try_from(i64::MAX).unwrap())
        );
        assert_eq!(admitted_pid(0), None, "0 is not a task id");
        assert_eq!(admitted_pid(-1), None);
        assert_eq!(admitted_pid(i64::MIN), None);
    }

    /// No exited child: nothing is reaped, reported, or torn down.
    #[test]
    fn reap_with_no_zombies_does_nothing() {
        let mut launched = LaunchTable::new();
        let path = "/System/Applications/files.app/Run";
        launched.record(3, "Files", path);
        let mut reported: Vec<String> = Vec::new();
        let mut torn_down: Vec<u64> = Vec::new();

        reap_launched(
            &mut launched,
            || None,
            |line| reported.push(String::from(line)),
            |pid| torn_down.push(pid),
        );

        assert_eq!(launched.len(), 1, "an unexited child stays in flight");
        assert!(reported.is_empty());
        assert!(torn_down.is_empty());
    }
}
