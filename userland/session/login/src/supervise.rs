//! Console login supervision: reload the user database before each round,
//! and wait — without prompting — while the encrypted root is still being
//! unlocked.
//!
//! A console's `login` process runs an unbounded sequence of login *rounds*
//! ([`Login::run`](crate::login::Login::run)); between rounds it loops back
//! to a fresh prompt. The user database it authenticates
//! against is **not** owned by `login` — it lives on the encrypted root and
//! is read through the capability-gated `users_db_read` syscall once that
//! root is unlocked.
//!
//! Under design B (`plans/PI.md` P11), `init` spawns `login` *before* the
//! in-kernel root-unlock kthread has mounted the encrypted root: the unlock
//! prompts for its passphrase on the **same** console `login` would prompt
//! on. Two problems follow, both fixed here:
//!
//! 1. **Prompt contention.** If `login` printed `Username:` straight away it
//!    would draw over the kthread's `Root passphrase:` prompt and the two
//!    would fight over the one keyboard. So while the unlock is still
//!    running, `users_db_read` reports [`DbLoad::Pending`] (the kernel's
//!    `Errno::WouldBlock`); `login` then **waits** ([`supervise`] calls the
//!    injected `wait`, e.g. `tairix_rt::users_db_wait`, which parks the task
//!    off the run queue until the unlock resolves) and does not prompt,
//!    leaving the console to the unlock until it resolves.
//! 2. **Stale "no database".** A `login` that read the database **once** at
//!    startup would cache the pre-unlock answer and refuse every credential
//!    for the life of the process — even after the unlock installs the
//!    database. So [`supervise`] reloads it **before each round**.
//!
//! Once the unlock resolves, the read returns either [`DbLoad::Present`]
//! (wire the [`UsersAuthenticator`] and authenticate against it) or
//! [`DbLoad::Absent`] (an installer image, or an unlock that gave up — wire
//! the fail-closed [`DenyAll`]).

use crate::auth::{DenyAll, UsersAuthenticator};
use crate::session::Authenticator;

use tairix_users::UsersDb;

/// The state of the user database as seen by a single `load_db` call.
///
/// Distinguishes "the unlock has not finished yet" ([`Pending`](Self::Pending))
/// from "the unlock finished and there is no database"
/// ([`Absent`](Self::Absent)), because [`supervise`] must wait on the
/// former and prompt fail-closed on the latter (`plans/PI.md` P11).
pub enum DbLoad {
    /// A validated database is held; authenticate against it this round.
    Present(UsersDb),
    /// The encrypted root is still being unlocked (`Errno::WouldBlock`):
    /// wait without prompting, then re-check, so the unlock keeps the
    /// console.
    Pending,
    /// No database will arrive — an installer image, or an unlock that gave
    /// up. Run the fail-closed deny-all prompt.
    Absent,
}

/// Run console login rounds, reloading the user database before each round
/// and waiting while it is still pending.
///
/// `load_db` is invoked before each round to obtain the current state of
/// the `/System/Security/Users` database ([`DbLoad`]). `wait` is called
/// when the database is [`DbLoad::Pending`] (the encrypted root is still
/// being unlocked); it should **block** until the database becomes
/// available (e.g. `tairix_rt::users_db_wait`, which parks the task off the
/// run queue rather than busy-yielding) so the in-kernel
/// unlock kthread runs — `login` neither prompts nor reads the console
/// while pending, so it cannot steal the passphrase bytes.
/// `run_round` runs one prompt → authenticate → launch round against the
/// supplied authenticator and returns `true` to open another round or
/// `false` when the console is dead and the supervisor should return
/// (PID 1 relaunches `login`).
///
/// A [`DbLoad::Present`] round wires a [`UsersAuthenticator`]; a
/// [`DbLoad::Absent`] round wires the fail-closed [`DenyAll`]. Reloading per round — rather than once at process start — is
/// what lets a `login` spawned before the encrypted root is unlocked pick
/// up the database the instant it becomes available, instead of caching a
/// stale answer for its whole lifetime (`plans/PI.md` P11).
pub fn supervise<L, W, R>(mut load_db: L, mut wait: W, mut run_round: R)
where
    L: FnMut() -> DbLoad,
    W: FnMut(),
    R: FnMut(&dyn Authenticator) -> bool,
{
    let deny = DenyAll;
    loop {
        match load_db() {
            // The unlock has not finished: do not prompt (it would race the
            // `Root passphrase:` prompt for the console). Block until it
            // resolves, then re-check — never a busy spin, `wait` parks the
            // task off the run queue.
            DbLoad::Pending => wait(),
            DbLoad::Present(db) => {
                if !run_round(&UsersAuthenticator::new(&db)) {
                    return;
                }
            }
            DbLoad::Absent => {
                if !run_round(&deny) {
                    return;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{supervise, DbLoad};
    use crate::session::Credentials;

    use alloc::vec;
    use core::cell::{Cell, RefCell};
    use tairix_caps::CapabilitySet;
    use tairix_users::{AccountState, Gid, Identity, Uid, UserRecord, UsersDb, MIN_ITERATIONS};

    /// A one-record database whose `root`/`root` account authenticates — the
    /// debug-image seed shape (`tools/mkimage`).
    fn db() -> UsersDb {
        let record = UserRecord::with_password(
            Identity {
                username: "root",
                uid: Uid(0),
                primary_gid: Gid(0),
                supplementary_gids: &[],
                display_name: "System Administrator",
                home: Some("/Users/root"),
                shell: Some("/System/Commands/elsh.app/Run"),
                capabilities: CapabilitySet::empty(),
                state: AccountState::Active,
            },
            b"root",
            [0x11; 16],
            MIN_ITERATIONS,
        )
        .expect("valid record");
        UsersDb::new(vec![record]).expect("valid db")
    }

    /// The metal regression: under design B `login` is spawned before the
    /// encrypted root is unlocked, so the early reads are `Pending` (the
    /// unlock owns the console). `login` must **wait** — not prompt — on
    /// those, then pick up the database the moment the unlock installs it
    /// and authenticate `root`/`root` against it (`plans/PI.md` P11). The
    /// pending rounds run no prompt; only the resolved round does.
    #[test]
    fn login_waits_while_pending_then_authenticates_once_installed() {
        let loads = Cell::new(0u32);
        let waits = Cell::new(0u32);
        let outcomes = RefCell::new(vec![]);
        supervise(
            || {
                let n = loads.get();
                loads.set(n + 1);
                // Pending for the first two reads (unlock still running),
                // then the database is installed.
                if n >= 2 {
                    DbLoad::Present(db())
                } else {
                    DbLoad::Pending
                }
            },
            || waits.set(waits.get() + 1),
            |authenticator| {
                let accepted = authenticator
                    .authenticate(&Credentials {
                        username: "root",
                        password: "root",
                    })
                    .is_ok();
                outcomes.borrow_mut().push(accepted);
                // Stop after the first (and only) prompted round.
                false
            },
        );
        // Two pending reads each waited and ran no round; the third read
        // was `Present` and ran exactly one round, which authenticated.
        assert_eq!(waits.get(), 2);
        assert_eq!(outcomes.into_inner(), vec![true]);
    }

    /// An installer image (or an unlock that gave up) reports `Absent`:
    /// `login` runs its fail-closed deny-all prompt straight away and never
    /// waits.
    #[test]
    fn an_absent_database_prompts_deny_all_without_waiting() {
        let waits = Cell::new(0u32);
        let denied = Cell::new(false);
        supervise(
            || DbLoad::Absent,
            || waits.set(waits.get() + 1),
            |authenticator| {
                denied.set(
                    authenticator
                        .authenticate(&Credentials {
                            username: "root",
                            password: "root",
                        })
                        .is_err(),
                );
                false
            },
        );
        assert_eq!(waits.get(), 0);
        assert!(
            denied.get(),
            "the deny-all authenticator refuses every credential"
        );
    }

    /// A database installed after `login` started is picked up by a later
    /// round rather than a stale early answer being cached for the
    /// process's lifetime: the `Absent` round refuses `root`/`root`, the
    /// later `Present` round accepts it (`plans/PI.md` P11).
    #[test]
    fn the_users_database_is_reloaded_before_each_round() {
        let calls = Cell::new(0u32);
        let outcomes = RefCell::new(vec![]);
        supervise(
            || {
                if calls.get() >= 1 {
                    DbLoad::Present(db())
                } else {
                    DbLoad::Absent
                }
            },
            || {},
            |authenticator| {
                let round = calls.get();
                calls.set(round + 1);
                let accepted = authenticator
                    .authenticate(&Credentials {
                        username: "root",
                        password: "root",
                    })
                    .is_ok();
                outcomes.borrow_mut().push(accepted);
                round < 1
            },
        );
        assert_eq!(outcomes.into_inner(), vec![false, true]);
    }

    /// A console that dies on its first round returns immediately without
    /// reloading again — `init` is the relaunch path, not a spin here.
    #[test]
    fn a_dead_console_returns_after_one_round() {
        let loads = Cell::new(0u32);
        let rounds = Cell::new(0u32);
        supervise(
            || {
                loads.set(loads.get() + 1);
                DbLoad::Absent
            },
            || {},
            |_authenticator| {
                rounds.set(rounds.get() + 1);
                false
            },
        );
        assert_eq!(loads.get(), 1);
        assert_eq!(rounds.get(), 1);
    }
}
