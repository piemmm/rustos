//! The `Run` entry-point binary of the login service, installed at
//! `/System/Services/login.app/Run` (`plans/PI.md` P11) — the
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
//!   record's **shell of choice** is spawned and supervised; the session's
//!   exit code is reported back to the login loop.
//! * [`rustos_login::handle_elevate_request`] over this console's reserved
//!   elevation call endpoint (`plans/CAPABILITY_USE.md` CU5): while the
//!   session runs, the supervision wait multiplexes the shell child with the
//!   endpoint, so an `elevate <user> <program>` request from the session's
//!   shell is re-authenticated (same authenticator as the prompt,
//!   timing-equalised, refusals indistinguishable) and its command run as
//!   the target account while the shell blocks in its `ipc_call`. Binding
//!   the reserved id requires login's `CAP_IPC_BIND_PRIVILEGED`; when no
//!   rendezvous can be bound the failure is audited and sessions simply run
//!   without a broker (requests fail closed at the missing endpoint).
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
    // The session-launch path builds the child environment with the heap,
    // which is live by then (a successful authentication already parsed the
    // user database). `rustos-rt` registers the process global allocator.
    extern crate alloc;

    use rustos_abi::elevate::{elevate_endpoint, ELEVATE_MAX_REQUEST, ELEVATE_REPLY_LEN};
    use rustos_abi::{
        Errno, Origin, WaitSetOp, WaitSourceKind, CONSOLE_INHERIT, ORIGIN_CONSOLE_NONE,
        ORIGIN_WIRE_LEN,
    };
    use rustos_caps::CapabilitySet;
    use rustos_login::elevate::ElevateLauncher;
    use rustos_login::{
        events, handle_elevate_request, session_environment, supervise, AuthenticatedUser,
        Authenticator, DbLoad, Login, LoginConfig, LoginError, Prompt, SessionKind,
        SessionLauncher, SessionOutcome,
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
    /// runs the **buffer** half ([`rustos_vt::line::LineEditor`]) while the
    /// kernel console runs the matching **echo** half, both keyed off the one
    /// `lib/vt` erase definition. So a Backspace — or the Delete key's
    /// `CSI 3 ~` sequence — rubs out the last character both on screen and
    /// in `buf`; CR and LF both terminate (UART terminals commonly send CR);
    /// a line longer than `buf` is refused, never truncated.
    ///
    /// **Allocation-free by design**: every byte lands in the caller's
    /// stack buffer, because the `mem_map`-backed userland heap is not
    /// available until its production producer lands (`plans/SPAWN.md`
    /// SP5b) — a heap allocation here would abort the process on the
    /// first keystroke. The stream *backing* owns blocking: each `stream_read` parks until input arrives, so a
    /// zero-length read means the stream failed or closed — reported as
    /// a console failure the login loop fails closed on, never spun on.
    fn read_line_raw(buf: &mut [u8]) -> Result<usize, Errno> {
        let mut editor = rustos_vt::line::LineEditor::new();
        let mut len = 0;
        let mut byte = [0u8; 1];
        loop {
            let read = rustos_rt::stdin(&mut byte);
            if read == 0 {
                return Err(Errno::NotFound);
            }
            match editor.push(buf, &mut len, byte[0]) {
                rustos_vt::line::LineFeed::Pending => {}
                rustos_vt::line::LineFeed::Complete => return Ok(len),
                rustos_vt::line::LineFeed::TooLong => return Err(Errno::LengthOutOfRange),
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
            // ourselves to match the un-suppressed prompts — a plain line
            // feed, which the console line discipline cooks to CR-LF so the
            // cursor returns to column zero.
            let _ = rustos_rt::set_echo(true);
            let _ = Stdout.write_all(b"\n");
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

    /// The waitset token naming this console's elevation call endpoint.
    const TOKEN_ELEVATE: u64 = 1;
    /// The waitset token naming the running session's shell child.
    const TOKEN_CHILD: u64 = 2;

    /// This console's bound elevation rendezvous: the call endpoint id the
    /// session's shell posts `elevate` requests to, and the wait-set that
    /// multiplexes those requests with the shell child's exit.
    ///
    /// Bound **once** at startup ([`ElevationContext::bind`]): a call
    /// endpoint lives until its owning task exits (there is no destroy
    /// syscall, exactly as `sysinfod`/`journald` hold theirs), and the one
    /// wait-set is reused across sessions — only the per-session child
    /// member is added and removed — so supervision allocates no kernel
    /// object per round.
    struct ElevationContext {
        /// This console's `elevate_endpoint` id, already bound.
        endpoint: u64,
        /// The reusable wait-set holding the endpoint member.
        waitset: u64,
        /// Login's own kernel-attested console index: the placement every
        /// requester's attested console is checked against.
        own_console: u64,
    }

    impl ElevationContext {
        /// Derive this console's rendezvous from login's **own** attested
        /// origin and bind it, wiring the reusable wait-set.
        ///
        /// Every failure returns `None` — sessions then run without an
        /// elevation broker and a shell's request fails closed at the
        /// missing endpoint — and is audited as
        /// [`events::ELEVATE_UNAVAILABLE`] by the caller: a process with no
        /// console-backed streams has no rendezvous to serve, and a bind
        /// refusal (id squatted ahead of us, no registry) must never be
        /// "recovered" by serving elsewhere.
        fn bind() -> Option<Self> {
            let own_console = rustos_rt::self_origin().ok()?.console();
            if own_console == ORIGIN_CONSOLE_NONE {
                return None;
            }
            let endpoint = elevate_endpoint(own_console).ok()?;
            let empty = CapabilitySet::empty();
            // Unrestricted senders: any process may post — placement and
            // re-authentication are enforced per request by the broker.
            // Capacity 1: elevation is serialised per console by design, so
            // a second concurrent post fails closed instead of queueing.
            if rustos_rt::call_create(
                endpoint,
                &empty,
                &empty,
                ELEVATE_MAX_REQUEST,
                ELEVATE_REPLY_LEN,
                1,
            ) != 0
            {
                return None;
            }
            let waitset = rustos_rt::waitset_create();
            if waitset < 0 {
                return None;
            }
            #[allow(clippy::cast_sign_loss)] // `waitset >= 0` is the handle encoding.
            let waitset = waitset as u64;
            if rustos_rt::waitset_ctl(
                waitset,
                WaitSetOp::Add,
                WaitSourceKind::Endpoint,
                endpoint,
                TOKEN_ELEVATE,
            ) != 0
            {
                return None;
            }
            Some(Self {
                endpoint,
                waitset,
                own_console,
            })
        }

        /// Receive, decide, and answer one posted elevation request.
        ///
        /// The request buffer carries an offered password, so it is zeroed
        /// before this returns on every path. A recv failure is dropped
        /// (the poster's `ipc_call` observes its error); a reply failure is
        /// dropped likewise — the decision and its audit record already
        /// stand.
        fn serve_one(&self, authenticator: &dyn Authenticator, sink: &LogSink) {
            let mut request = [0u8; ELEVATE_MAX_REQUEST];
            let mut ticket = 0u64;
            let Ok(len) = rustos_rt::call_recv(self.endpoint, &mut request, &mut ticket) else {
                request.fill(0);
                return;
            };
            // Attest the caller's placement. A failure to read the peer
            // origin fails closed as "no console", which the broker refuses.
            let mut origin_buf = [0u8; ORIGIN_WIRE_LEN];
            let peer_console =
                match rustos_rt::call_peer_origin(self.endpoint, ticket, &mut origin_buf) {
                    Ok(n) => match Origin::from_bytes(&origin_buf[..n]) {
                        Ok(origin) => origin.console(),
                        Err(_) => ORIGIN_CONSOLE_NONE,
                    },
                    Err(_) => ORIGIN_CONSOLE_NONE,
                };
            let reply = handle_elevate_request(
                &request[..len],
                peer_console,
                self.own_console,
                authenticator,
                &RtElevateLauncher,
                sink,
            );
            request.fill(0);
            let mut reply_buf = [0u8; ELEVATE_REPLY_LEN];
            if let Ok(total) = reply.encode(&mut reply_buf) {
                let _ = rustos_rt::call_reply(self.endpoint, ticket, &reply_buf[..total]);
            }
        }
    }

    /// Runs one re-authenticated elevated command: `spawn_as` the target
    /// account on this console, then a targeted `wait` for exactly that
    /// child. The session's shell is blocked in its `ipc_call` for the
    /// duration (a foreground elevated command, serialised per console), so
    /// the only child that can exit here is the elevated one.
    struct RtElevateLauncher;

    impl ElevateLauncher for RtElevateLauncher {
        fn run_as(&self, program: &str, uid: u32) -> Result<i32, Errno> {
            let ret = rustos_rt::spawn_as(program.as_bytes(), CONSOLE_INHERIT, uid);
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
            Ok(status)
        }
    }

    /// Launches the authenticated record's shell of choice **as the
    /// authenticated user** through the `spawn` syscall and supervises the
    /// session until it ends (`plans/SPAWN.md` SP3/SP6; `PREREQUISITES.md`
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
    ///
    /// With an [`ElevationContext`] bound, the wait multiplexes the shell
    /// child with this console's elevation endpoint (`plans/CAPABILITY_USE.md`
    /// CU5): a posted request is re-authenticated and served while the shell
    /// blocks in its `ipc_call`, and the shell's own exit ends the session
    /// exactly as before. Without one (no console-backed streams, bind
    /// refused) the launcher degrades to the plain blocking `wait` — the
    /// session is unaffected; only elevation is unavailable, audited at
    /// startup.
    struct RtLauncher<'a> {
        elevation: Option<&'a ElevationContext>,
        authenticator: &'a dyn Authenticator,
        sink: &'a LogSink,
    }

    impl RtLauncher<'_> {
        /// Supervise the running shell `pid`: serve elevation requests as
        /// they arrive and return the shell's exit status when it ends.
        ///
        /// Falls back to the plain blocking `wait` when the wait-set cannot
        /// observe the child (member add or wait failure): supervision
        /// degrades to exactly the pre-elevation behaviour rather than
        /// spinning or abandoning the session.
        fn supervise_session(&self, context: &ElevationContext, pid: i32) -> Result<i32, Errno> {
            let child_id = u64::from(pid.unsigned_abs());
            if rustos_rt::waitset_ctl(
                context.waitset,
                WaitSetOp::Add,
                WaitSourceKind::Child,
                child_id,
                TOKEN_CHILD,
            ) != 0
            {
                return self.plain_wait(pid);
            }
            let status = loop {
                let mut token = 0u64;
                let ret = rustos_rt::waitset_wait(context.waitset, u64::MAX, &mut token);
                if ret != 0 {
                    // An unexpected wait failure must not wedge the session:
                    // fall back to the plain blocking wait.
                    break self.plain_wait(pid);
                }
                if token == TOKEN_CHILD {
                    break self.plain_wait(pid);
                }
                context.serve_one(self.authenticator, self.sink);
            };
            // Remove the reaped child's member so the reusable set never
            // carries a stale PID into the next session.
            let _ = rustos_rt::waitset_ctl(
                context.waitset,
                WaitSetOp::Del,
                WaitSourceKind::Child,
                child_id,
                TOKEN_CHILD,
            );
            status
        }

        /// The targeted blocking reap of the session's shell.
        fn plain_wait(&self, pid: i32) -> Result<i32, Errno> {
            let mut status = 0i32;
            let wret = rustos_rt::wait(pid, &mut status);
            if wret < 0 {
                return Err(errno_from(wret));
            }
            Ok(status)
        }
    }

    impl SessionLauncher for RtLauncher<'_> {
        fn launch(
            &self,
            user: &AuthenticatedUser,
            kind: SessionKind,
        ) -> Result<SessionOutcome, Errno> {
            // Hand the shell the session environment (USER, LOGNAME, HOME,
            // SHELL, PWD, PATH, TERM, LANG) built from the authenticated
            // account, so its prompt and `$USER`/`$HOME`/… reflect the real
            // user. `spawn_with` carries both the environment and the uid
            // switch; the env strings are data and grant no authority (every
            // capability stays kernel-side). Allocating here is safe: a
            // successful authentication already parsed the database, so the
            // heap is live well before this launch.
            let env_owned = session_environment(user);
            let env: alloc::vec::Vec<&[u8]> = env_owned
                .iter()
                .map(alloc::string::String::as_bytes)
                .collect();
            let ret = rustos_rt::spawn_with(
                user.shell.as_bytes(),
                CONSOLE_INHERIT,
                user.uid.0,
                &[],
                &env,
            );
            if ret < 0 {
                return Err(errno_from(ret));
            }
            // `ret >= 0` here, so the cast preserves the PID value; PIDs
            // fit an `i32` on this ABI.
            #[allow(clippy::cast_possible_truncation)]
            let pid = ret as i32;
            let exit_code = match self.elevation {
                Some(context) => self.supervise_session(context, pid)?,
                None => self.plain_wait(pid)?,
            };
            Ok(SessionOutcome { kind, exit_code })
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
    /// (PID 1 relaunches it). The session launcher serves this console's
    /// elevation endpoint (when bound) with the **same** authenticator the
    /// prompt used, so an elevation re-authenticates against exactly the
    /// database this round authenticated against.
    fn login_round(
        elevation: Option<&ElevationContext>,
        authenticator: &dyn Authenticator,
        sink: &LogSink,
    ) -> bool {
        let prompt = RtPrompt;
        let launcher = RtLauncher {
            elevation,
            authenticator,
            sink,
        };
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
        // Bind this console's elevation rendezvous once for the process's
        // lifetime (the endpoint dies with the task). Failure is audited and
        // sessions run without a broker — an `elevate` request then fails
        // closed at the missing endpoint, never served unattested.
        let elevation = ElevationContext::bind();
        if elevation.is_none() {
            rustos_log::log(
                &sink,
                &rustos_log::Event {
                    level: rustos_log::Level::Warn,
                    id: events::ELEVATE_UNAVAILABLE,
                    message: "elevation endpoint unavailable; sessions run without a broker",
                    fields: &[],
                },
            );
        }
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
            |authenticator| login_round(elevation.as_ref(), authenticator, &sink),
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
