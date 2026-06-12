//! PID 1's session supervision: one login session per discovered text
//! console (`plans/PI.md` P11, `AGENTS.md` §20).
//!
//! The kernel installs one stream backing per text console — the video
//! console and the UART are **separate session contexts** — and reports
//! how many exist through the `console_count` syscall. PID 1 launches the
//! configured session program once per console with `spawn`'s explicit
//! console selector and supervises the whole set with wait-any: whichever
//! session exits is reaped and relaunched on **its own** console, within a
//! per-console crash-loop budget (`AGENTS.md` §2.1 — never an unbounded
//! `spawn` loop).
//!
//! The logic is pure and parameterised over the [`Sessions`] seam so every
//! decision — launch fan-out, pid→console accounting, per-console budgets,
//! exhaustion, error propagation — is host-tested without a kernel,
//! mirroring the `startup` config parser's split. The freestanding `run`
//! binary backs the seam with the real `rustos-rt` syscall wrappers; PID 1
//! performs **no allocation** here (fixed slot array), because the
//! userland heap's production producer is staged (`plans/SPAWN.md` `SP5b`).

/// Most consoles PID 1 supervises in the bootstrap (allocation-free)
/// supervisor.
///
/// A stack-array sizing for the no-heap PID 1, not a system policy: the
/// kernel's console list is discovered hardware (today at most a display
/// plus a UART), and the slot table must live on `main`'s stack until the
/// userland heap lands (`plans/SPAWN.md` `SP5b`), after which the table can
/// size itself from `console_count` alone (`AGENTS.md` §24.1 — grow when
/// growth is possible). Consoles beyond the bound are left without a
/// session rather than overrunning the table (fail closed, §2.9).
pub const MAX_SUPERVISED_CONSOLES: usize = 8;

/// How many times PID 1 will (re)launch one console's session before
/// declaring that console's session unable to stay up and abandoning it.
///
/// On a console with a working input stream the session blocks on `stdin`
/// rather than exiting, so its supervisor slot never approaches this
/// bound — the budget exists only as a **crash-loop guard** (`AGENTS.md`
/// §2.1): a session that exits the instant it starts would otherwise be
/// relaunched forever. The budget is per console, so one crash-looping
/// console cannot starve the others' relaunches.
pub const SESSION_SPAWN_BUDGET: u32 = 3;

/// The syscalls the supervisor drives, as a seam so the policy is
/// host-testable (`plans/PI.md` P11; the `Spawner`/`Reaper` split's
/// shape). The freestanding binary backs it with `rustos-rt`.
pub trait Sessions {
    /// `console_count`: how many text consoles are installed
    /// (non-negative), or `-errno`.
    fn console_count(&mut self) -> i64;
    /// `spawn` with an explicit console selector: launch `path` attached
    /// to installed console `console`, returning the PID or `-errno`.
    fn spawn_at(&mut self, path: &[u8], console: u32) -> i64;
    /// `wait` with `WAIT_ANY`: block until any child exits, reap it, and
    /// return its PID (writing the exit code to `status`), or `-errno`.
    fn wait_any(&mut self, status: &mut i32) -> i64;
}

/// Why [`supervise_sessions`] returned (PID 1 never returns while a
/// session is still supervisable).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Outcome {
    /// `console_count` failed or reported zero consoles: there is nothing
    /// a session could attach to, so PID 1 reports the system unusable
    /// rather than spawning sessions with no streams (`AGENTS.md` §2.9).
    NoConsoles,
    /// A `spawn` failed (`-errno`): an unknown path, an unwired spawn
    /// subsystem, or an invalid console index — fail-loud, never ignored.
    SpawnFailed,
    /// `wait` failed (`-errno`): the supervisor cannot reap its own
    /// children — a kernel-state inconsistency it surfaces rather than
    /// continuing blindly.
    WaitFailed,
    /// Every console's session exhausted its relaunch budget: the system
    /// cannot keep a session up anywhere, and PID 1 declares that
    /// honestly instead of busy-looping on `spawn` (`AGENTS.md` §2.1).
    Exhausted,
}

/// One supervised console's bookkeeping.
#[derive(Copy, Clone)]
struct Slot {
    /// The live session's PID, or `-1` when the slot is abandoned.
    pid: i64,
    /// Launches consumed (counts the initial launch).
    launches: u32,
}

impl Slot {
    const ABANDONED: i64 = -1;

    const fn vacant() -> Self {
        Self {
            pid: Self::ABANDONED,
            launches: 0,
        }
    }

    fn alive(&self) -> bool {
        self.pid >= 0
    }
}

/// Launch and supervise one `session` instance per installed console.
///
/// 1. Read the console count; zero (or an error) is [`Outcome::NoConsoles`].
/// 2. Launch `session` on each console (up to
///    [`MAX_SUPERVISED_CONSOLES`]) through [`Sessions::spawn_at`]; any
///    launch failure is [`Outcome::SpawnFailed`].
/// 3. Wait-any in a loop: map each reaped PID back to its console and
///    relaunch the session **on that console**, until the console's
///    [`SESSION_SPAWN_BUDGET`] is consumed (the slot is then abandoned).
///    A reaped PID belonging to no slot (a session's leaked child) is
///    reaped and ignored. When every slot is abandoned the supervisor
///    returns [`Outcome::Exhausted`].
///
/// The reaped exit status is read but not yet acted on; a policy that
/// distinguishes a clean logout from a crash (and resets the budget on a
/// session that ran long enough) awaits a clock/session-state ABI.
pub fn supervise_sessions<S: Sessions>(sys: &mut S, session: &[u8]) -> Outcome {
    let count = sys.console_count();
    if count <= 0 {
        return Outcome::NoConsoles;
    }
    // Clamp to the allocation-free slot table; consoles past the bound
    // get no session rather than overrunning it (fail closed, §2.9).
    // `count` is positive here, so the conversion only fails on a width
    // the clamp would saturate anyway.
    let consoles =
        usize::try_from(count).map_or(MAX_SUPERVISED_CONSOLES, |n| n.min(MAX_SUPERVISED_CONSOLES));

    let mut slots = [Slot::vacant(); MAX_SUPERVISED_CONSOLES];
    for (console, slot) in slots.iter_mut().enumerate().take(consoles) {
        // Console indices fit `u32`: the table is bounded far below it.
        #[allow(clippy::cast_possible_truncation)]
        let pid = sys.spawn_at(session, console as u32);
        if pid < 0 {
            return Outcome::SpawnFailed;
        }
        slot.pid = pid;
        slot.launches = 1;
    }

    loop {
        if !slots[..consoles].iter().any(Slot::alive) {
            return Outcome::Exhausted;
        }

        let mut status = 0i32;
        let reaped = sys.wait_any(&mut status);
        if reaped < 0 {
            return Outcome::WaitFailed;
        }

        let Some(console) = slots[..consoles]
            .iter()
            .position(|slot| slot.alive() && slot.pid == reaped)
        else {
            // Not one of the supervised sessions (a reparented grandchild):
            // it is reaped, nothing to relaunch.
            continue;
        };

        let slot = &mut slots[console];
        if slot.launches >= SESSION_SPAWN_BUDGET {
            // This console's session cannot stay up; abandon the slot
            // rather than busy-looping on `spawn` (`AGENTS.md` §2.1). The
            // remaining consoles keep their sessions.
            slot.pid = Slot::ABANDONED;
            continue;
        }
        #[allow(clippy::cast_possible_truncation)]
        let pid = sys.spawn_at(session, console as u32);
        if pid < 0 {
            return Outcome::SpawnFailed;
        }
        slot.pid = pid;
        slot.launches += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Scripted [`Sessions`] double: hands out PIDs, records every spawn
    /// `(path-ok, console)`, and replays a scripted sequence of wait-any
    /// results.
    struct ScriptedSessions {
        count: i64,
        spawn_results: Vec<i64>,
        spawns: Vec<u32>,
        waits: Vec<i64>,
        next_spawn: usize,
        next_wait: usize,
    }

    impl ScriptedSessions {
        fn new(count: i64, spawn_results: Vec<i64>, waits: Vec<i64>) -> Self {
            Self {
                count,
                spawn_results,
                spawns: Vec::new(),
                waits,
                next_spawn: 0,
                next_wait: 0,
            }
        }
    }

    impl Sessions for ScriptedSessions {
        fn console_count(&mut self) -> i64 {
            self.count
        }
        fn spawn_at(&mut self, path: &[u8], console: u32) -> i64 {
            assert_eq!(path, b"login");
            self.spawns.push(console);
            let result = self.spawn_results[self.next_spawn];
            self.next_spawn += 1;
            result
        }
        fn wait_any(&mut self, _status: &mut i32) -> i64 {
            let result = self.waits[self.next_wait];
            self.next_wait += 1;
            result
        }
    }

    #[test]
    fn zero_or_failed_console_count_is_no_consoles() {
        let mut none = ScriptedSessions::new(0, vec![], vec![]);
        assert_eq!(supervise_sessions(&mut none, b"login"), Outcome::NoConsoles);
        assert!(none.spawns.is_empty());

        let mut err = ScriptedSessions::new(-7, vec![], vec![]);
        assert_eq!(supervise_sessions(&mut err, b"login"), Outcome::NoConsoles);
        assert!(err.spawns.is_empty());
    }

    #[test]
    fn one_session_is_launched_per_console() {
        // Two consoles; both sessions then crash-loop to exhaustion. The
        // launch fan-out attaches one session to console 0 and one to
        // console 1 — the video console and the UART are separate session
        // contexts (`plans/PI.md` P11).
        let mut sys = ScriptedSessions::new(
            2,
            // budget=3 per console: 2 initial + 2×2 relaunches.
            vec![10, 20, 11, 21, 12, 22],
            vec![10, 20, 11, 21, 12, 22],
        );
        assert_eq!(supervise_sessions(&mut sys, b"login"), Outcome::Exhausted);
        assert_eq!(sys.spawns, vec![0, 1, 0, 1, 0, 1]);
    }

    #[test]
    fn a_reaped_session_relaunches_on_its_own_console() {
        // Console 1's session exits twice; console 0's session never
        // exits. Every relaunch lands back on console 1, then the budget
        // (3 launches) abandons that console and the next wait fails the
        // run out so the test terminates deterministically.
        let mut sys = ScriptedSessions::new(2, vec![10, 20, 21, 22], vec![20, 21, 22, -7]);
        assert_eq!(supervise_sessions(&mut sys, b"login"), Outcome::WaitFailed);
        assert_eq!(sys.spawns, vec![0, 1, 1, 1]);
    }

    #[test]
    fn an_unknown_reaped_pid_is_ignored() {
        // PID 99 is no supervised session (a reparented grandchild): it
        // is reaped without consuming any console's budget or spawning.
        let mut sys = ScriptedSessions::new(1, vec![10], vec![99, -7]);
        assert_eq!(supervise_sessions(&mut sys, b"login"), Outcome::WaitFailed);
        assert_eq!(sys.spawns, vec![0]);
    }

    #[test]
    fn a_failed_launch_is_spawn_failed() {
        let mut at_start = ScriptedSessions::new(2, vec![10, -3], vec![]);
        assert_eq!(
            supervise_sessions(&mut at_start, b"login"),
            Outcome::SpawnFailed
        );

        let mut at_relaunch = ScriptedSessions::new(1, vec![10, -3], vec![10]);
        assert_eq!(
            supervise_sessions(&mut at_relaunch, b"login"),
            Outcome::SpawnFailed
        );
    }

    #[test]
    fn exhaustion_requires_every_console_to_consume_its_budget() {
        // One console, budget 3: three launches (PIDs 10, 11, 12), three
        // exits, then exhaustion — exactly the single-console behaviour
        // the pre-P11 supervisor had.
        let mut sys = ScriptedSessions::new(1, vec![10, 11, 12], vec![10, 11, 12]);
        assert_eq!(supervise_sessions(&mut sys, b"login"), Outcome::Exhausted);
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
        assert_eq!(supervise_sessions(&mut sys, b"login"), Outcome::Exhausted);
        assert_eq!(sys.spawns.len(), launches);
        assert!(sys
            .spawns
            .iter()
            .all(|&console| (console as usize) < MAX_SUPERVISED_CONSOLES));
    }
}
