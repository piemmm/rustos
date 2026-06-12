//! The `Run` entry-point binary of the login service, installed at
//! `/System/Services/login` (`AGENTS.md` §16.2, `plans/PI.md` P11) — the
//! program PID 1 `init` launches as the per-console session supervisor.
//!
//! This is a **pure-Rust** program: RustOS is Rust-only (`AGENTS.md` §1), so
//! it links the Rust userland runtime `rustos-rt` — never the C ABI, which
//! exists solely for programs **not** written in Rust (`AGENTS.md` §16.4).
//! `rustos-rt` provides `_start`, the per-process stack canary (`AGENTS.md`
//! §19.2), the panic handler, the `mem_map`-backed global allocator, and the
//! syscall wrappers; `rustos_rt::entry!` names this program's `main`.
//!
//! `main` wires the real seams the [`rustos_login::Login`] state machine
//! drives and supervises sessions on this console:
//!
//! * [`rustos_login::Prompt`] over the **inherited standard streams**
//!   (`AGENTS.md` §20):
//!   prompts go to fd 1, input lines come from fd 0. The console stream
//!   backing performs terminal local echo in the kernel's read line
//!   discipline (`plans/PI.md` P11), on by default, so a typed username is
//!   visible. The password read suppresses echo through the `stream_echo`
//!   syscall (`rustos_rt::set_echo`) before reading and restores it after,
//!   so the secret is never rendered (`AGENTS.md` §5.4 — never echo a
//!   credential); if echo cannot be disabled the read fails closed rather
//!   than echoing the password.
//! * [`rustos_login::UsersAuthenticator`] over the user database obtained
//!   through the capability-gated `users_db_read` syscall (`CAP_USERS_READ`,
//!   `AGENTS.md` §5.1) and re-parsed with the fail-closed `rustos-users`
//!   parser. When no database is held — an installer image, or the boot
//!   read refused the record — a deny-all authenticator is wired instead,
//!   so the prompt stays up and **every** login is refused (`AGENTS.md`
//!   §5.4.5 — fail closed, never invent an account).
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
extern crate alloc;

#[cfg(freestanding)]
mod program {
    use rustos_abi::Errno;
    use rustos_log::{Event, Sink};
    use rustos_login::{
        AuthenticatedUser, Authenticator, Credentials, Login, LoginConfig, LoginError, Prompt,
        SessionKind, SessionLauncher, SessionOutcome, UsersAuthenticator,
    };
    use rustos_users::{UsersDb, MAX_DB_LEN};

    /// Authentication attempts per login round before the round fails
    /// closed and the loop opens a fresh one (`AGENTS.md` §5.4.5). The
    /// bound exists so a wedged automation cannot hold one round open
    /// forever; the supervising loop itself is the retry path.
    const MAX_ATTEMPTS: u32 = 3;

    /// Write all of `bytes` to standard output, looping over short writes.
    ///
    /// A write that accepts zero bytes means the stream will accept no more
    /// (a closed or full backing); the loop stops rather than spinning
    /// (`AGENTS.md` §2.1). Output is best-effort: a dropped tail does not
    /// abort the prompt.
    fn write_all_stdout(mut bytes: &[u8]) {
        while !bytes.is_empty() {
            let written = rustos_rt::stdout(bytes);
            if written == 0 {
                break;
            }
            bytes = &bytes[written.min(bytes.len())..];
        }
    }

    /// Write all of `bytes` to standard error, looping over short writes
    /// (see [`write_all_stdout`]).
    fn write_all_stderr(mut bytes: &[u8]) {
        while !bytes.is_empty() {
            let written = rustos_rt::stderr(bytes);
            if written == 0 {
                break;
            }
            bytes = &bytes[written.min(bytes.len())..];
        }
    }

    /// Read one edited input line (without its terminator) from standard
    /// input into `buf`, returning the number of bytes filled.
    ///
    /// The read line discipline is shared with the kernel console echo: this
    /// runs the **buffer** half ([`rustos_login::push_line_byte`]) while the
    /// kernel console runs the matching **echo** half, both keyed off the one
    /// `lib/vt` erase definition (`AGENTS.md` §2.2). So a Backspace rubs out
    /// the last character both on screen and in `buf`; CR and LF both
    /// terminate (UART terminals commonly send CR); a line longer than `buf`
    /// is refused, never truncated (`AGENTS.md` §2.9).
    ///
    /// **Allocation-free by design**: every byte lands in the caller's
    /// stack buffer, because the `mem_map`-backed userland heap is not
    /// available until its production producer lands (`plans/SPAWN.md`
    /// SP5b) — a heap allocation here would abort the process on the
    /// first keystroke. The stream *backing* owns blocking (`AGENTS.md`
    /// §20): each `stream_read` parks until input arrives, so a
    /// zero-length read means the stream failed or closed — reported as
    /// a console failure the login loop fails closed on, never spun on
    /// (`AGENTS.md` §2.1).
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

    /// The controlling terminal over the inherited standard streams
    /// (`AGENTS.md` §20). The program binds only fd 0/1, never a device.
    struct RtPrompt;

    impl Prompt for RtPrompt {
        fn write(&self, text: &str) {
            write_all_stdout(text.as_bytes());
        }

        fn read_line(&self, buf: &mut [u8]) -> Result<usize, Errno> {
            read_line_raw(buf)
        }

        fn read_secret(&self, buf: &mut [u8]) -> Result<usize, Errno> {
            // Console echo is on by default, so suppress it for the
            // duration of the password read — a credential must never be
            // rendered (`AGENTS.md` §5.4). If echo cannot be disabled (the
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
            write_all_stdout(b"\r\n");
            result
        }
    }

    /// An [`Authenticator`] wired when no user database is held: every
    /// attempt is refused with the same error, so an installer image (or a
    /// boot whose database read was refused) sits at a prompt that grants
    /// nothing (`AGENTS.md` §5.4.5 — fail closed, never invent accounts).
    struct DenyAll;

    impl Authenticator for DenyAll {
        fn authenticate(&self, _credentials: &Credentials<'_>) -> Result<AuthenticatedUser, Errno> {
            Err(Errno::PermissionDenied)
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

    /// Launches the authenticated record's shell of choice through the
    /// `spawn` syscall and blocks in `wait` until the session ends
    /// (`plans/SPAWN.md` SP3/SP6). The child receives only its registered
    /// program grant — spawning here never widens authority (`AGENTS.md`
    /// §4, §5.2).
    struct RtLauncher;

    impl SessionLauncher for RtLauncher {
        fn launch(
            &self,
            user: &AuthenticatedUser,
            kind: SessionKind,
        ) -> Result<SessionOutcome, Errno> {
            let ret = rustos_rt::spawn(user.shell.as_bytes());
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

    /// Audit sink for the login process: each security-relevant decision is
    /// written as one terse line on standard error (fd 2 — diagnostics,
    /// `AGENTS.md` §20). The hash-chained system audit log (§19.4) is
    /// kernel-side; a userland audit transport is future work, and silently
    /// discarding the records until then would hide security decisions.
    struct StderrSink;

    impl Sink for StderrSink {
        fn write_event(&self, event: &Event<'_>) {
            write_all_stderr(b"login: ");
            write_all_stderr(event.message.as_bytes());
            write_all_stderr(b"\n");
        }
    }

    /// Read the user database through the capability-gated `users_db_read`
    /// syscall and parse it fail-closed.
    ///
    /// Any failure — the syscall refused (no database held, no capability)
    /// or the text failed the `users-v1` validation — yields [`None`] and
    /// the caller wires the deny-all authenticator (`AGENTS.md` §5.4.5).
    /// The intermediate buffer holds credential records, so it is zeroed
    /// before release (`AGENTS.md` §4).
    ///
    /// The buffer is a stack array, **not** a heap allocation: everything
    /// on the path to the first prompt must be allocation-free, because
    /// the userland heap is backed by the `mem_map` syscall whose
    /// production producer is still staged (`plans/SPAWN.md` SP5b) — a
    /// pre-prompt allocation would fail and the console would never reach
    /// `login:`. Parsing a *successfully delivered* database allocates;
    /// that path is only reachable once a root volume holds a database,
    /// which arrives with the same staged work (`plans/PI.md` P11).
    fn load_users_db() -> Option<UsersDb> {
        let mut buf = [0u8; MAX_DB_LEN];
        let db = match rustos_rt::users_db_read(&mut buf) {
            Ok(len) => core::str::from_utf8(&buf[..len])
                .ok()
                .and_then(|text| UsersDb::parse(text).ok()),
            Err(_) => None,
        };
        buf.fill(0);
        db
    }

    /// Run one login round: prompt → authenticate → run the session to
    /// completion. Returns `true` to open another round, `false` when the
    /// console is dead and the process should exit (PID 1 relaunches it).
    fn login_round(authenticator: &dyn Authenticator, sink: &StderrSink) -> bool {
        let prompt = RtPrompt;
        let launcher = RtLauncher;
        let login = Login::new(LoginConfig {
            max_attempts: MAX_ATTEMPTS,
            // The graphical session rides the P10 WM work; until a display
            // session exists the option is hidden, never errored
            // (`AGENTS.md` §10).
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
            // A dead console cannot prompt again: exit rather than spin
            // (`AGENTS.md` §2.1); PID 1 supervises and relaunches login.
            Err(LoginError::Console(_)) => false,
        }
    }

    /// Program entry point. `rustos-rt`'s `_start` calls it once the runtime
    /// is set up and routes its return value through the `exit` syscall.
    fn main() -> i32 {
        let sink = StderrSink;
        let db = load_users_db();
        loop {
            let alive = match db.as_ref() {
                Some(db) => login_round(&UsersAuthenticator::new(db), &sink),
                None => login_round(&DenyAll, &sink),
            };
            if !alive {
                return 1;
            }
        }
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
