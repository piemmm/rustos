//! The `Run` entry-point binary of the login service, installed at
//! `/System/Services/login` (`plans/PI.md` P11) — the
//! program PID 1 `init` launches as the per-console session supervisor.
//!
//! This is a **pure-Rust** program: RustOS is Rust-only, so
//! it links the Rust userland runtime `rustos-rt` — never the C ABI, which
//! exists solely for programs **not** written in Rust.
//! `rustos-rt` provides `_start`, the per-process stack canary, the panic handler, the `mem_map`-backed global allocator, and the
//! syscall wrappers; `rustos_rt::entry!` names this program's `main`.
//!
//! `main` wires the real seams the [`rustos_login::Login`] state machine
//! drives and supervises sessions on this console:
//!
//! * [`rustos_login::Prompt`] over the **inherited standard streams**:
//!   prompts go to fd 1, input lines come from fd 0. The console stream
//!   backing performs terminal local echo in the kernel's read line
//!   discipline (`plans/PI.md` P11), on by default, so a typed username is
//!   visible. The password read suppresses echo through the `stream_echo`
//!   syscall (`rustos_rt::set_echo`) before reading and restores it after,
//!   so the secret is never rendered (never echo a
//!   credential); if echo cannot be disabled the read fails closed rather
//!   than echoing the password.
//! * [`rustos_login::UsersAuthenticator`] over the user database obtained
//!   through the capability-gated `users_db_read` syscall (`CAP_USERS_READ`) and re-parsed with the fail-closed `rustos-users`
//!   parser. When no database is held — an installer image, or the boot
//!   read refused the record — a deny-all authenticator is wired instead,
//!   so the prompt stays up and **every** login is refused (fail closed, never invent an account).
//! * [`rustos_login::SessionLauncher`] through the `spawn` syscall: the
//!   authenticated
//!   record's **shell of choice** is spawned and `wait`ed on; the session's
//!   exit code is reported back to the login loop.
//!
//! Each completed session (or exhausted attempt budget) loops back to a
//! fresh `login:` prompt — login supervises this console's sessions. A dead
//! console (a failed read) exits instead; PID 1 `init` supervises login
//! itself and relaunches it (`plans/SPAWN.md` SP6).
//!
//! On the host it is an inert stub so `cargo build --workspace`, clippy, and
//! fmt still cover the file.

#![cfg_attr(freestanding, no_std)]
#![cfg_attr(freestanding, no_main)]
#![deny(missing_docs)]

// --- Pure-Rust program --------------------------------------------------
#[cfg(freestanding)]
mod program {
    use rustos_abi::{Errno, CONSOLE_INHERIT};
    use rustos_login::{
        supervise, AuthenticatedUser, Authenticator, DbLoad, Login, LoginConfig, LoginError,
        Prompt, SessionKind, SessionLauncher, SessionOutcome,
    };
    use rustos_rt::io::{Stdout, Write};
    use rustos_rt::LogSink;
    use rustos_users::{UsersDb, MAX_DB_LEN};

    /// Authentication attempts per login round before the round fails
    /// closed and the loop opens a fresh one. The
    /// bound exists so a wedged automation cannot hold one round open
    /// forever; the supervising loop itself is the retry path.
    const MAX_ATTEMPTS: u32 = 3;

    /// Bound on a single `users_db_wait` park while the database is
    /// [`DbLoad::Pending`] (the encrypted root is still being unlocked).
    ///
    /// The kernel wakes the wait the instant the unlock reaches a terminal
    /// outcome, so this is only a safety net: should a wake ever be missed,
    /// the wait returns `TimedOut` and [`supervise`] re-reads and re-parks
    /// rather than blocking forever. Five seconds is far longer than an
    /// unlock takes yet short enough to recover promptly; the wait parks the
    /// CPU throughout, so a long bound costs nothing.
    const DB_WAIT_TIMEOUT_NS: u64 = 5_000_000_000;

    /// Read one edited input line (without its terminator) from standard
    /// input into `buf`, returning the number of bytes filled.
    ///
    /// The read line discipline is shared with the kernel console echo: this
    /// runs the **buffer** half ([`rustos_login::push_line_byte`]) while the
    /// kernel console runs the matching **echo** half, both keyed off the one
    /// `lib/vt` erase definition. So a Backspace rubs out
    /// the last character both on screen and in `buf`; CR and LF both
    /// terminate (UART terminals commonly send CR); a line longer than `buf`
    /// is refused, never truncated.
    ///
    /// **Allocation-free by design**: every byte lands in the caller's
    /// stack buffer, because the `mem_map`-backed userland heap is not
    /// available until its production producer lands (`plans/SPAWN.md`
    /// SP5b) — a heap allocation here would abort the process on the
    /// first keystroke. The stream *backing* owns blocking: each `stream_read` parks until input arrives, so a
    /// zero-length read means the stream failed or closed — reported as
    /// a console failure the login loop fails closed on, never spun on.
    fn read_line_raw(buf: &mut [u8]) -> Result<usize, Errno> {
        let mut len = 0;
        let mut byte = [0u8; 1];
        loop {
            let read = rustos_rt::stdin(&mut byte);
            if read == 0 {
                return Err(Errno::NotFound);
            }
            match rustos_login::push_line_byte(buf, &mut len, byte[0]) {
                rustos_login::LineFeed::Pending => {}
                rustos_login::LineFeed::Complete => return Ok(len),
                rustos_login::LineFeed::TooLong => return Err(Errno::LengthOutOfRange),
            }
        }
    }

    /// The controlling terminal over the inherited standard streams. The program binds only fd 0/1, never a device.
    struct RtPrompt;

    impl Prompt for RtPrompt {
        fn write(&self, text: &str) {
            // The shared `rustos_rt::io` short-write loop — no login-private
            // copy (the charter forbids that duplication). Prompt output is
            // best-effort (a dropped tail does not abort the prompt), so the
            // fail-closed result is discarded; `write_all` never spins.
            let _ = Stdout.write_all(text.as_bytes());
        }

        fn read_line(&self, buf: &mut [u8]) -> Result<usize, Errno> {
            read_line_raw(buf)
        }

        fn read_secret(&self, buf: &mut [u8]) -> Result<usize, Errno> {
            // Console echo is on by default, so suppress it for the
            // duration of the password read — a credential must never be
            // rendered. If echo cannot be disabled (the
            // toggle failed), fail closed rather than reading a secret that
            // would echo.
            let toggled = rustos_rt::set_echo(false);
            if toggled < 0 {
                return Err(errno_from(toggled));
            }
            let result = read_line_raw(buf);
            // Restore echo for the subsequent interactive prompts. A
            // failure to re-enable cannot compromise the secret already
            // read, so it is best-effort. The Return key the user pressed
            // was not echoed (echo was off), so advance the display a line
            // ourselves to match the un-suppressed prompts.
            let _ = rustos_rt::set_echo(true);
            let _ = Stdout.write_all(b"\r\n");
            result
        }
    }

    /// Recover the [`Errno`] a syscall encoded as a negative register
    /// (`-errno`, the standard `abi-v1` convention). An unrecognised code
    /// fails closed as [`Errno::NotImplemented`] rather than being guessed.
    fn errno_from(ret: i64) -> Errno {
        i32::try_from(-ret)
            .ok()
            .and_then(Errno::from_i32)
            .unwrap_or(Errno::NotImplemented)
    }

    /// Launches the authenticated record's shell of choice **as the
    /// authenticated user** through the `spawn` syscall and blocks in `wait`
    /// until the session ends (`plans/SPAWN.md` SP3/SP6; `PREREQUISITES.md`
    /// P-C spawn-as-user). Login authenticated the account, so it drops the
    /// shell into that user's kernel-attested credential (uid, primary gid,
    /// supplementary groups) via `spawn_as` — privilege only ever switches
    /// user at process creation, never by a running process mutating its own
    /// identity (no setuid-self). The kernel resolves the full credential
    /// from the authoritative identity table, so login chooses *which* user
    /// but never fabricates the identity; it holds `CAP_SPAWN_AS_USER`, and
    /// the shell still receives only its own registered program grant
    /// intersected with that user's ceiling. The child stays on login's own
    /// console (`CONSOLE_INHERIT`).
    struct RtLauncher;

    impl SessionLauncher for RtLauncher {
        fn launch(
            &self,
            user: &AuthenticatedUser,
            kind: SessionKind,
        ) -> Result<SessionOutcome, Errno> {
            let ret = rustos_rt::spawn_as(user.shell.as_bytes(), CONSOLE_INHERIT, user.uid.0);
            if ret < 0 {
                return Err(errno_from(ret));
            }
            let mut status = 0i32;
            // `ret >= 0` here, so the cast preserves the PID value; PIDs
            // fit an `i32` on this ABI.
            #[allow(clippy::cast_possible_truncation)]
            let wret = rustos_rt::wait(ret as i32, &mut status);
            if wret < 0 {
                return Err(errno_from(wret));
            }
            Ok(SessionOutcome {
                kind,
                exit_code: status,
            })
        }
    }

    /// Read the user database through the capability-gated `users_db_read`
    /// syscall and classify it for [`supervise`] as a [`DbLoad`].
    ///
    /// * `WouldBlock` (the kernel's pending signal) → [`DbLoad::Pending`]:
    ///   the encrypted root is still being unlocked, so `login` waits
    ///   without prompting and leaves the console to the `Root passphrase:`
    ///   prompt (`plans/PI.md` P11).
    /// * A delivered, valid database → [`DbLoad::Present`].
    /// * Any other refusal (no database held once the unlock resolved, no
    ///   capability) or text that failed the `users-v1` validation →
    ///   [`DbLoad::Absent`]: the caller wires the deny-all authenticator.
    ///
    /// The intermediate buffer holds credential records, so it is zeroed
    /// before release. It is a stack array, **not** a heap
    /// allocation: everything on the path to the first prompt must be
    /// allocation-free, because the userland heap is backed by the
    /// `mem_map` syscall whose production producer is still staged
    /// (`plans/SPAWN.md` SP5b) — a pre-prompt allocation would fail and the
    /// console would never reach `login:`. The pending and absent paths
    /// allocate nothing; parsing a *successfully delivered* database
    /// allocates, which is only reachable once the encrypted root that
    /// holds it is unlocked (`plans/PI.md` P11). [`supervise`] calls this
    /// before each round, so a database installed after `login` started —
    /// the design-B order, where `login` is spawned before the in-kernel
    /// unlock kthread mounts the root — is picked up by the next round
    /// instead of a stale answer being cached for the process's lifetime.
    fn load_users_db() -> DbLoad {
        let mut buf = [0u8; MAX_DB_LEN];
        // The wrapper returns the raw `-errno` on failure; `WouldBlock` is the only one that means "retry", every
        // other refusal fails closed to the deny-all prompt.
        let pending = -i64::from(Errno::WouldBlock.as_i32());
        let state = match rustos_rt::users_db_read(&mut buf) {
            Ok(len) => match core::str::from_utf8(&buf[..len])
                .ok()
                .and_then(|text| UsersDb::parse(text).ok())
            {
                Some(db) => DbLoad::Present(db),
                None => DbLoad::Absent,
            },
            Err(code) if code == pending => DbLoad::Pending,
            Err(_) => DbLoad::Absent,
        };
        buf.fill(0);
        state
    }

    /// Run one login round: prompt → authenticate → run the session to
    /// completion against `authenticator`. Returns `true` to open another
    /// round, `false` when the console is dead and the process should exit
    /// (PID 1 relaunches it).
    fn login_round(authenticator: &dyn Authenticator, sink: &LogSink) -> bool {
        let prompt = RtPrompt;
        let launcher = RtLauncher;
        let login = Login::new(LoginConfig {
            max_attempts: MAX_ATTEMPTS,
            // The graphical session rides the P10 WM work; until a display
            // session exists the option is hidden, never errored.
            graphical_available: false,
            prompt: &prompt,
            authenticator,
            launcher: &launcher,
            sink,
        });
        match login.run() {
            // A finished session or an exhausted attempt budget both loop
            // back to a fresh prompt; the audit trail already records the
            // outcome.
            Ok(_) | Err(LoginError::TooManyAttempts | LoginError::SessionLaunch(_)) => true,
            // A dead console cannot prompt again: exit rather than spin; PID 1 supervises and relaunches login.
            Err(LoginError::Console(_)) => false,
        }
    }

    /// Program entry point. `rustos-rt`'s `_start` calls it once the runtime
    /// is set up and routes its return value through the `exit` syscall.
    ///
    /// [`supervise`] reloads the user database (and so re-wires the
    /// [`UsersAuthenticator`] or the fail-closed deny-all authenticator)
    /// before every round, so a database installed after this process
    /// started — design B spawns `login` before the encrypted root is
    /// unlocked — is picked up by the next round rather than a stale
    /// "no database" answer being cached for the process's lifetime
    /// (`plans/PI.md` P11). It returns only when a round reports the console
    /// dead; PID 1 relaunches `login`.
    fn main() -> i32 {
        // Route each security-relevant decision through the kernel
        // diagnostic log (the serial UART on a debug build) rather than fd 2. The hash-chained system audit log stays kernel-side; this is the diagnostic channel.
        let sink = LogSink;
        // While the database read is `Pending` (the encrypted root is still
        // being unlocked) `supervise` calls this to **block** until the
        // database becomes available, so the in-kernel unlock kthread runs
        // and `login` neither prompts nor reads the console. The kernel
        // parks the task off the run queue and wakes it the instant the
        // unlock resolves (`users_db_wait`) — never the busy yield loop that
        // flooded the boot log with one `users_db_read` rejection per poll. The wait's result is advisory: `supervise`
        // re-reads the database on the next round regardless of whether the
        // wait was woken or timed out.
        supervise(
            load_users_db,
            || {
                let _ = rustos_rt::users_db_wait(DB_WAIT_TIMEOUT_NS);
            },
            |authenticator| login_round(authenticator, &sink),
        );
        1
    }

    rustos_rt::entry!(main);
}

// --- Host stub ----------------------------------------------------------
//
// On the host (`cargo build --workspace`, clippy, fmt) the program's real
// entry — the freestanding `rustos-rt` `_start` path — is not compiled, so
// this inert `main` keeps the crate building under the host tooling. It
// performs no I/O.
#[cfg(not(freestanding))]
fn main() {}
