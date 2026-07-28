//! PID 1's session supervision: one login session per installed text
//! console (`plans/PI.md` P11).
//!
//! The kernel installs one stream backing per text console it discovers —
//! the video console when a display is active (the UART then carries only
//! the debug log, with no session), else the discovered UART — and reports
//! how many exist through the `console_count` syscall. PID 1 launches the
//! configured session program once per console with `spawn`'s explicit
//! console selector and supervises the whole set with wait-any: whichever
//! session exits is reaped and relaunched on **its own** console, within a
//! per-console crash-loop budget (never an unbounded
//! `spawn` loop).
//!
//! The long-running **services** (the System Information, network-stack,
//! device-manager, and seat-manager services, today) are **not** supervised
//! here: they are the [`Init`](crate::Init) service-manager engine's job
//! (`plans/NEW-SERVICEMANAGER.md` SVC-A). PID 1 registers them with the
//! engine, brings them up in dependency order, and drives the engine's
//! readiness-gated admission and restart-policy state machine — the model
//! that lets discovery/registration and on-demand activation follow. This
//! module owns only the per-console **session** half, layered over that
//! engine through the [`Services`] seam: the one wait-any loop routes each
//! reaped pid to a session relaunch or to the engine (which reaps a known
//! service — applying its restart policy — or an inherited orphan).
//!
//! The logic is pure and parameterised over the [`Sessions`] and [`Services`]
//! seams so every decision — session fan-out, pid→console accounting,
//! per-console budgets, exhaustion, error propagation, and service routing —
//! is host-tested without a kernel, mirroring the `startup` config parser's
//! split. The freestanding `run` binary backs [`Sessions`] with the real
//! `tairix-rt` syscall wrappers and [`Services`] with the live
//! [`Init`](crate::Init) engine; the per-console session table is a fixed
//! stack array, so the session half itself allocates nothing.

/// Most consoles PID 1 supervises in the bootstrap (allocation-free)
/// supervisor.
///
/// A stack-array sizing for the no-heap PID 1, not a system policy: the
/// kernel's console list is discovered hardware (today a single display
/// or UART console), and the slot table must live on `main`'s stack until the
/// userland heap lands (`plans/SPAWN.md` `SP5b`), after which the table can
/// size itself from `console_count` alone (grow when
/// growth is possible). Consoles beyond the bound are left without a
/// session rather than overrunning the table (fail closed).
pub const MAX_SUPERVISED_CONSOLES: usize = 8;

/// How many times PID 1 will (re)launch one console's session before
/// declaring that console's session unable to stay up and abandoning it.
///
/// On a console with a working input stream the session blocks on `stdin`
/// rather than exiting, so its supervisor slot never approaches this
/// bound — the budget exists only as a **crash-loop guard**: a session that exits the instant it starts would otherwise be
/// relaunched forever. The budget is per console, so one crash-looping
/// console cannot starve the others' relaunches.
pub const SESSION_SPAWN_BUDGET: u32 = 3;

/// The service-manager engine, as a seam the session supervisor routes
/// non-session child exits into.
///
/// PID 1's long-running services are owned by the [`Init`](crate::Init)
/// engine, not by this module. The one wait-any loop cannot tell a service's
/// pid from an inherited orphan's by itself, so every reaped pid that is not
/// one of its own login sessions is handed to [`on_child_exit`](Self::on_child_exit),
/// which the engine classifies (a known service exit — applying its restart
/// policy — or an orphan). [`any_running`](Self::any_running) lets the loop
/// keep waiting while any service still holds a live process (a perpetual
/// service such as `devmgr` keeps this `true` for the life of the system),
/// so PID 1 does not declare exhaustion while it is legitimately holding a
/// service up.
///
/// The seam keeps the session-supervision policy host-testable without
/// constructing a full engine; the freestanding binary backs it with the
/// live [`Init`](crate::Init) engine over the real syscall seams.
pub trait Services {
    /// Route a reaped child that is **not** one of PID 1's login sessions to
    /// the service engine. The engine reaps a service it started (moving its
    /// lifecycle to a terminal state and scheduling any policy-driven
    /// restart) or logs an inherited orphan; either way the child is reaped
    /// exactly once. `pid` is the kernel-reported non-negative pid and
    /// `exit_code` its exit status.
    fn on_child_exit(&mut self, pid: u64, exit_code: i32);

    /// Whether any supervised service still has a live process. The loop
    /// keeps waiting while this is `true` even after every session has been
    /// abandoned, so a perpetual service holds PID 1 up rather than the
    /// supervisor declaring [`Outcome::Exhausted`].
    fn any_running(&self) -> bool;
}

/// The syscalls the supervisor drives, as a seam so the policy is
/// host-testable (`plans/PI.md` P11; the `Spawner`/`Reaper` split's
/// shape). The freestanding binary backs it with `tairix-rt`.
pub trait Sessions {
    /// `console_count`: how many text consoles are installed
    /// (non-negative), or `-errno`.
    fn console_count(&mut self) -> i64;
    /// `spawn` with an explicit console selector and target user: launch
    /// `path` attached to installed console `console`, switched onto the
    /// concrete `uid` (a compiled-in service account resolved at config
    /// parse time — the kernel gates the switch on `CAP_SPAWN_AS_USER`
    /// and resolves the credential from its boot-installed identity
    /// table), returning the PID or `-errno`.
    fn spawn_at(&mut self, path: &[u8], console: u32, uid: u32) -> i64;
    /// `wait` with `WAIT_PID_ANY`: block until any child exits, reap it, and
    /// return its PID (writing the exit code to `status`), or `-errno`.
    fn wait_any(&mut self, status: &mut i32) -> i64;
    /// State the reason a launch was refused: `spawn_at` for `path` on
    /// `console` returned the negative `err` (`-errno`). PID 1 abandons the
    /// entry and boots on, so the refusal must land somewhere a user can
    /// find it — the production backing writes one terse line to `stderr`
    /// (fd 2, the inherited diagnostic stream); a silent skip would hide a
    /// dead service behind a working-looking boot.
    fn report_launch_failure(&mut self, path: &[u8], console: u32, err: i64);
}

/// Why [`supervise`] returned (PID 1 never returns while a
/// session is still supervisable).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Outcome {
    /// `console_count` failed or reported zero consoles: there is nothing
    /// a session could attach to, so PID 1 reports the system unusable
    /// rather than spawning sessions with no streams.
    NoConsoles,
    /// `wait` failed (`-errno`): the supervisor cannot reap its own
    /// children — a kernel-state inconsistency it surfaces rather than
    /// continuing blindly.
    WaitFailed,
    /// Every console's session exhausted its relaunch budget: the system
    /// cannot keep a session up anywhere, and PID 1 declares that
    /// honestly instead of busy-looping on `spawn`.
    Exhausted,
}

/// One launch entry the caller configures: the program path and the
/// concrete uid of the compiled-in service account it runs as (resolved
/// by the startup-config parser at parse time).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Launch<'a> {
    /// The program path to launch.
    pub path: &'a [u8],
    /// The concrete `target_uid` the entry is spawned with.
    pub uid: u32,
}

/// One supervised **session**'s bookkeeping: the program to (re)launch, the
/// account it runs as, the console it attaches to, its live PID, and how
/// many launches it has consumed. One slot per text console.
///
/// The long-running services are **not** slots here — the [`Init`](crate::Init)
/// engine owns them (`plans/NEW-SERVICEMANAGER.md` SVC-A); this table is only
/// the per-console login sessions.
#[derive(Copy, Clone)]
struct Slot<'a> {
    /// The program path to (re)launch this slot with.
    path: &'a [u8],
    /// The concrete `target_uid` every (re)launch of this slot switches
    /// the child onto.
    uid: u32,
    /// The console index the program's standard streams attach to.
    console: u32,
    /// The live child's PID, or `-1` when the slot is abandoned.
    pid: i64,
    /// Launches consumed (counts the initial launch).
    launches: u32,
}

impl Slot<'_> {
    const ABANDONED: i64 = -1;

    const fn vacant() -> Self {
        Self {
            path: &[],
            uid: 0,
            console: 0,
            pid: Self::ABANDONED,
            launches: 0,
        }
    }

    fn alive(&self) -> bool {
        self.pid >= 0
    }
}

/// Launch and supervise one `session` (login) instance per installed
/// console, routing every other reaped child to the service engine.
///
/// The long-running services are already registered with, and brought up
/// by, the [`Init`](crate::Init) engine before this is called
/// (`plans/NEW-SERVICEMANAGER.md` SVC-A); `services` here is that engine
/// behind the [`Services`] seam, not a launch list.
///
/// 1. Read the console count; zero (or an error) is [`Outcome::NoConsoles`].
/// 2. Launch the `session` program once on each console (up to
///    [`MAX_SUPERVISED_CONSOLES`]) through [`Sessions::spawn_at`]. A refused
///    session is reported through [`Sessions::report_launch_failure`] and
///    its slot abandoned; the remaining sessions still launch — one console
///    that cannot host a session must not take the others down.
/// 3. Wait-any in a loop. A reaped pid that matches a live session slot is
///    relaunched on **its own** console until that slot's
///    [`SESSION_SPAWN_BUDGET`] is consumed (the slot is then abandoned, never
///    busy-looped on `spawn`). Every other reaped pid — a supervised service
///    or an inherited orphan — is handed to [`Services::on_child_exit`], which
///    the engine classifies and reaps. The loop returns [`Outcome::Exhausted`]
///    only when no session is alive **and** the engine holds no running
///    service; a perpetual service (e.g. `devmgr`) therefore keeps PID 1
///    waiting for the life of the system even after every session is gone.
pub fn supervise<E: Services, S: Sessions>(
    services: &mut E,
    sys: &mut S,
    session: Launch<'_>,
) -> Outcome {
    let count = sys.console_count();
    if count <= 0 {
        return Outcome::NoConsoles;
    }
    // Clamp to the allocation-free session table; consoles past the bound
    // are left without a session rather than overrunning it (fail closed).
    // `count` is positive here, so the conversion only fails on a width the
    // clamp would saturate anyway.
    let consoles =
        usize::try_from(count).map_or(MAX_SUPERVISED_CONSOLES, |n| n.min(MAX_SUPERVISED_CONSOLES));

    let mut slots = [Slot::vacant(); MAX_SUPERVISED_CONSOLES];
    for (console, slot) in slots[..consoles].iter_mut().enumerate() {
        slot.path = session.path;
        slot.uid = session.uid;
        // Console indices fit `u32`: the table is bounded far below it.
        #[allow(clippy::cast_possible_truncation)]
        {
            slot.console = console as u32;
        }
    }
    for slot in &mut slots[..consoles] {
        let pid = sys.spawn_at(slot.path, slot.console, slot.uid);
        slot.launches = 1;
        if pid < 0 {
            // Fail loud, degrade gracefully: state the refusal and boot
            // on with the surviving sessions. A launch refusal is
            // deterministic (the kernel's load gate said no), so a retry
            // loop would only repeat the answer.
            sys.report_launch_failure(slot.path, slot.console, pid);
            continue;
        }
        slot.pid = pid;
    }

    loop {
        let any_session = slots[..consoles].iter().any(Slot::alive);
        if !any_session && !services.any_running() {
            // No session can stay up anywhere and no service holds a live
            // process: there is nothing left to wait on, so PID 1 declares
            // exhaustion honestly instead of blocking on an empty wait set.
            return Outcome::Exhausted;
        }

        let mut status = 0i32;
        let reaped = sys.wait_any(&mut status);
        if reaped < 0 {
            return Outcome::WaitFailed;
        }

        if let Some(index) = slots[..consoles]
            .iter()
            .position(|slot| slot.alive() && slot.pid == reaped)
        {
            let slot = &mut slots[index];
            if slot.launches >= SESSION_SPAWN_BUDGET {
                // This console's session cannot stay up; abandon the slot
                // rather than busy-looping on `spawn`. The rest keep running.
                slot.pid = Slot::ABANDONED;
                continue;
            }
            let path = slot.path;
            let console = slot.console;
            let pid = sys.spawn_at(path, console, slot.uid);
            if pid < 0 {
                // Same policy as the initial fan-out: report, abandon this
                // slot, keep the rest of the system up.
                slot.pid = Slot::ABANDONED;
                sys.report_launch_failure(path, console, pid);
                continue;
            }
            slot.pid = pid;
            slot.launches += 1;
        } else {
            // Not a login session: a service the engine started, or an
            // inherited orphan. Either way it is the engine's to reap and
            // classify (a service exit applies its restart policy; an
            // orphan is logged). `reaped` is non-negative here (the `< 0`
            // check above returned), so `unsigned_abs` is its exact value.
            services.on_child_exit(reaped.unsigned_abs(), status);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The session's fixture uid (the `login` service account's band).
    const LOGIN_UID: u32 = 13;

    /// A session [`Launch`] over `path` under [`LOGIN_UID`].
    fn session(path: &'static [u8]) -> Launch<'static> {
        Launch {
            path,
            uid: LOGIN_UID,
        }
    }

    /// A [`Services`] double: records every routed non-session child exit and
    /// reports a settable "a service is still running" answer. `idle()` is a
    /// service manager with nothing running (so the loop can reach
    /// exhaustion); `perpetual()` is one holding a live service forever (so it
    /// never does).
    struct MockServices {
        running: bool,
        exits: Vec<(u64, i32)>,
    }

    impl MockServices {
        fn idle() -> Self {
            Self {
                running: false,
                exits: Vec::new(),
            }
        }
        fn perpetual() -> Self {
            Self {
                running: true,
                exits: Vec::new(),
            }
        }
    }

    impl Services for MockServices {
        fn on_child_exit(&mut self, pid: u64, exit_code: i32) {
            self.exits.push((pid, exit_code));
        }
        fn any_running(&self) -> bool {
            self.running
        }
    }

    /// Scripted [`Sessions`] double: hands out PIDs, records every spawn's
    /// `console` and `path`, and replays a scripted sequence of wait-any
    /// results (with an optional parallel exit-status script).
    struct ScriptedSessions {
        count: i64,
        spawn_results: Vec<i64>,
        spawns: Vec<u32>,
        spawn_paths: Vec<Vec<u8>>,
        spawn_uids: Vec<u32>,
        waits: Vec<i64>,
        statuses: Vec<i32>,
        reports: Vec<(Vec<u8>, u32, i64)>,
        next_spawn: usize,
        next_wait: usize,
    }

    impl ScriptedSessions {
        fn new(count: i64, spawn_results: Vec<i64>, waits: Vec<i64>) -> Self {
            Self {
                count,
                spawn_results,
                spawns: Vec::new(),
                spawn_paths: Vec::new(),
                spawn_uids: Vec::new(),
                waits,
                statuses: Vec::new(),
                reports: Vec::new(),
                next_spawn: 0,
                next_wait: 0,
            }
        }

        /// Attach a parallel exit-status script, written into `status` on each
        /// `wait_any` (index-aligned with `waits`; absent entries stay `0`).
        fn with_statuses(mut self, statuses: Vec<i32>) -> Self {
            self.statuses = statuses;
            self
        }
    }

    impl Sessions for ScriptedSessions {
        fn console_count(&mut self) -> i64 {
            self.count
        }
        fn spawn_at(&mut self, path: &[u8], console: u32, uid: u32) -> i64 {
            self.spawns.push(console);
            self.spawn_paths.push(path.to_vec());
            self.spawn_uids.push(uid);
            let result = self.spawn_results[self.next_spawn];
            self.next_spawn += 1;
            result
        }
        fn wait_any(&mut self, status: &mut i32) -> i64 {
            if let Some(s) = self.statuses.get(self.next_wait) {
                *status = *s;
            }
            let result = self.waits[self.next_wait];
            self.next_wait += 1;
            result
        }
        fn report_launch_failure(&mut self, path: &[u8], console: u32, err: i64) {
            self.reports.push((path.to_vec(), console, err));
        }
    }

    #[test]
    fn zero_or_failed_console_count_is_no_consoles() {
        let mut none = ScriptedSessions::new(0, vec![], vec![]);
        assert_eq!(
            supervise(&mut MockServices::idle(), &mut none, session(b"login")),
            Outcome::NoConsoles
        );
        assert!(none.spawns.is_empty());

        let mut err = ScriptedSessions::new(-7, vec![], vec![]);
        assert_eq!(
            supervise(&mut MockServices::idle(), &mut err, session(b"login")),
            Outcome::NoConsoles
        );
        assert!(err.spawns.is_empty());
    }

    #[test]
    fn one_session_is_launched_per_console() {
        // Two consoles; both sessions then crash-loop to exhaustion. The
        // launch fan-out attaches one session to console 0 and one to
        // console 1 — the supervisor covers every console the kernel
        // reports, whatever its backing (`plans/PI.md` P11).
        let mut sys = ScriptedSessions::new(
            2,
            // budget=3 per console: 2 initial + 2×2 relaunches.
            vec![10, 20, 11, 21, 12, 22],
            vec![10, 20, 11, 21, 12, 22],
        );
        assert_eq!(
            supervise(&mut MockServices::idle(), &mut sys, session(b"login")),
            Outcome::Exhausted
        );
        assert_eq!(sys.spawns, vec![0, 1, 0, 1, 0, 1]);
    }

    #[test]
    fn a_reaped_session_relaunches_on_its_own_console() {
        // Console 1's session exits twice; console 0's session never
        // exits. Every relaunch lands back on console 1, then the budget
        // (3 launches) abandons that console and the next wait fails the
        // run out so the test terminates deterministically.
        let mut sys = ScriptedSessions::new(2, vec![10, 20, 21, 22], vec![20, 21, 22, -7]);
        assert_eq!(
            supervise(&mut MockServices::idle(), &mut sys, session(b"login")),
            Outcome::WaitFailed
        );
        assert_eq!(sys.spawns, vec![0, 1, 1, 1]);
    }

    #[test]
    fn a_reaped_non_session_pid_is_routed_to_the_engine() {
        // PID 99 is no supervised session (a service the engine started, or
        // a reparented grandchild): it consumes no console's budget and no
        // session spawn — it is handed to the engine to reap and classify.
        // A non-zero exit status is forwarded verbatim.
        let mut services = MockServices::perpetual();
        let mut sys = ScriptedSessions::new(1, vec![10], vec![99, -7]).with_statuses(vec![7, 0]);
        assert_eq!(
            supervise(&mut services, &mut sys, session(b"login")),
            Outcome::WaitFailed
        );
        // Only the one session spawn; PID 99 never triggered a session spawn.
        assert_eq!(sys.spawns, vec![0]);
        assert_eq!(services.exits, vec![(99, 7)]);
    }

    #[test]
    fn a_failed_session_launch_is_reported_and_the_rest_of_the_boot_survives() {
        // Two consoles; console 1's session is refused at the fan-out. The
        // refusal is reported, console 0's session keeps running (the next
        // wait error ends the run deterministically) — one refused session
        // never aborts PID 1.
        let mut at_start = ScriptedSessions::new(2, vec![10, -3], vec![-7]);
        assert_eq!(
            supervise(&mut MockServices::idle(), &mut at_start, session(b"login")),
            Outcome::WaitFailed
        );
        assert_eq!(at_start.reports, vec![(b"login".to_vec(), 1, -3)]);

        // A refused *relaunch* abandons only that slot: with no other live
        // session and no running service, the supervisor then reports honest
        // exhaustion.
        let mut at_relaunch = ScriptedSessions::new(1, vec![10, -3], vec![10]);
        assert_eq!(
            supervise(
                &mut MockServices::idle(),
                &mut at_relaunch,
                session(b"login")
            ),
            Outcome::Exhausted
        );
        assert_eq!(at_relaunch.reports, vec![(b"login".to_vec(), 0, -3)]);
    }

    #[test]
    fn every_session_refused_with_no_service_is_honest_exhaustion() {
        // No session could be launched and the engine holds no running
        // service: the refusal is reported and the supervisor returns
        // `Exhausted` instead of waiting on children it never had.
        let mut sys = ScriptedSessions::new(1, vec![-10], vec![]);
        assert_eq!(
            supervise(&mut MockServices::idle(), &mut sys, session(b"login")),
            Outcome::Exhausted
        );
        assert_eq!(sys.reports.len(), 1);
    }

    #[test]
    fn a_perpetual_service_keeps_pid1_up_after_every_session_is_abandoned() {
        // The device-manager case: every session crash-loops to its budget
        // and is abandoned, but a service is still running, so the
        // supervisor does **not** declare exhaustion — it keeps waiting for
        // the life of the system. The scripted wait error only ends the
        // *test*, proving the loop was still waiting rather than exhausted.
        let mut services = MockServices::perpetual();
        let mut sys = ScriptedSessions::new(1, vec![10, 11, 12], vec![10, 11, 12, -7]);
        assert_eq!(
            supervise(&mut services, &mut sys, session(b"login")),
            Outcome::WaitFailed
        );
        // Three session launches (initial + 2 relaunches to the budget),
        // then the slot is abandoned; no fourth session spawn.
        assert_eq!(sys.spawns, vec![0, 0, 0]);
        // No non-session pid was reaped, so nothing was routed to the engine.
        assert!(services.exits.is_empty());
    }

    #[test]
    fn exhaustion_requires_every_console_to_consume_its_budget() {
        // One console, budget 3, no running service: three launches (PIDs
        // 10, 11, 12), three exits, then exhaustion — exactly the
        // single-console session behaviour the pre-P11 supervisor had.
        let mut sys = ScriptedSessions::new(1, vec![10, 11, 12], vec![10, 11, 12]);
        assert_eq!(
            supervise(&mut MockServices::idle(), &mut sys, session(b"login")),
            Outcome::Exhausted
        );
        assert_eq!(sys.spawns, vec![0, 0, 0]);
    }

    #[test]
    fn console_count_is_clamped_to_the_slot_table() {
        // More consoles than slots: only the first
        // `MAX_SUPERVISED_CONSOLES` get sessions, and the run still
        // terminates through the budget.
        let launches = MAX_SUPERVISED_CONSOLES * SESSION_SPAWN_BUDGET as usize;
        let bound = i64::try_from(launches).expect("small test constant");
        let spawn_results: Vec<i64> = (0..bound).map(|n| 100 + n).collect();
        let waits: Vec<i64> = spawn_results.clone();
        let mut sys = ScriptedSessions::new(64, spawn_results, waits);
        assert_eq!(
            supervise(&mut MockServices::idle(), &mut sys, session(b"login")),
            Outcome::Exhausted
        );
        assert_eq!(sys.spawns.len(), launches);
        assert!(sys
            .spawns
            .iter()
            .all(|&console| (console as usize) < MAX_SUPERVISED_CONSOLES));
    }

    #[test]
    fn a_reaped_service_and_a_reaped_session_are_routed_to_their_owners() {
        // One console session (PID 10, perpetual) and a service (PID 50)
        // that exits: the session is not the reaped pid, so 50 is routed to
        // the engine; the session keeps running. The wait error ends the
        // run. Proves the loop distinguishes a session pid from every other
        // reaped pid.
        let mut services = MockServices::perpetual();
        let mut sys = ScriptedSessions::new(1, vec![10], vec![50, -7]).with_statuses(vec![0, 0]);
        assert_eq!(
            supervise(&mut services, &mut sys, session(b"login")),
            Outcome::WaitFailed
        );
        // The session spawned once and was never relaunched (it did not
        // exit); the service exit was routed to the engine.
        assert_eq!(sys.spawns, vec![0]);
        assert_eq!(services.exits, vec![(50, 0)]);
    }
}
