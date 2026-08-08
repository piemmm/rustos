//! The `Run` entry-point binary of the login service, installed at
//! `/System/Services/login.app/Run` (`plans/PI.md` P11) — the
//! program PID 1 `init` launches as the per-console session supervisor.
//!
//! This is a **pure-Rust** program: TAIRiX is Rust-only, so
//! it links the Rust userland runtime `tairix-rt` — never the C ABI, which
//! exists solely for programs **not** written in Rust.
//! `tairix-rt` provides `_start`, the per-process stack canary, the panic handler, the `mem_map`-backed global allocator, and the
//! syscall wrappers; `tairix_rt::entry!` names this program's `main`.
//!
//! `main` wires the real seams the [`tairix_login::Login`] state machine
//! drives and supervises sessions on this console:
//!
//! * [`tairix_login::LoginView`] as the full-screen curses view
//!   ([`tairix_login::CursesView`]) over the **inherited standard
//!   streams**: rendered bytes go to fd 1, keystrokes come from fd 0. The
//!   view selects the raw (echo-off) discipline through the
//!   `stream_input_mode` syscall for the whole page — it echoes the
//!   username itself and renders nothing for the password — and refuses a
//!   password read outright if raw mode cannot be selected (never echo a
//!   credential). Its status bars query `sysinfod` (identity, memory,
//!   load/task/user censuses) and the kernel wall clock; a refused or
//!   unavailable figure renders as a placeholder and never blocks a login.
//! * [`tairix_login::UsersAuthenticator`] over the user database obtained
//!   through the capability-gated `users_db_read` syscall (`CAP_USERS_READ`) and re-parsed with the fail-closed `tairix-users`
//!   parser. When no database is held — an installer image, or the boot
//!   read refused the record — a deny-all authenticator is wired instead,
//!   so the prompt stays up and **every** login is refused (fail closed, never invent an account).
//! * [`tairix_login::SessionLauncher`] through the `spawn` syscall: the
//!   authenticated
//!   record's **shell of choice** is spawned and supervised; the session's
//!   exit code is reported back to the login loop.
//! * [`tairix_login::handle_elevate_request`] over this console's reserved
//!   elevation call endpoint (`plans/CAPABILITY_USE.md` CU5): while the
//!   session runs, the supervision wait multiplexes the shell child with the
//!   endpoint, so an `elevate <user> <program>` request from the session's
//!   shell is re-authenticated (same authenticator as the prompt,
//!   timing-equalised, refusals indistinguishable) and its command run as
//!   the target account while the shell blocks in its `ipc_call`. The same
//!   endpoint also answers a caller's own `Verify`-only request — no
//!   program runs, and the account checked is the caller's kernel-attested
//!   uid (read off the same `call_peer_origin` result as the console),
//!   never a name the request supplies. Binding the reserved id requires
//!   login's `CAP_IPC_BIND_PRIVILEGED`; when no rendezvous can be bound the
//!   failure is audited and sessions simply run without a broker (requests
//!   fail closed at the missing endpoint).
//! * [`tairix_login::handle_session_request`] over the reserved `session-v1`
//!   endpoint (`plans/NEW-DESKTOP-LOGIN.md` G4), multiplexed on that same
//!   wait-set. A round that can be graphical starts the login screen as the
//!   unprivileged `greeter` service account, serves it until a verdict is
//!   accepted, then starts — or resumes — that account's desktop. A session
//!   that asks to step aside keeps its processes and its place in the
//!   session table while the login screen comes back up.
//!
//! Each completed session (or exhausted attempt budget) loops back to a
//! fresh `login:` prompt — login supervises this console's sessions. A dead
//! console (a failed read) exits instead, first telling every session still
//! on the table to end, since nothing could wake one once this process is
//! gone; PID 1 `init` supervises login itself and relaunches it
//! (`plans/SPAWN.md` SP6).
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
    // user database). `tairix-rt` registers the process global allocator.
    extern crate alloc;

    use alloc::string::String;
    use alloc::vec::Vec;

    use core::cell::RefCell;
    use core::sync::atomic::{AtomicBool, Ordering};
    use tairix_abi::display_ipc::{DisplayRequest, DISPLAY_ENDPOINT, DISPLAY_MODE_REPLY_LEN};
    use tairix_abi::elevate::{elevate_endpoint, ELEVATE_MAX_REQUEST, ELEVATE_REPLY_LEN};
    use tairix_abi::seat::SEAT_PRIMARY;
    use tairix_abi::session_ipc::{
        SessionWake, SESSION_ENDPOINT, SESSION_MAX_REPLY, SESSION_MAX_REQUEST, SESSION_WAKE_LEN,
    };
    use tairix_abi::sysinfo::{KernelMemoryStats, LoadAverage, SysinfoQueryId, SystemIdentity};
    use tairix_abi::time::Duration64;
    use tairix_abi::{
        Errno, InputMode, OpenFlags, Origin, Time64, WaitSetOp, WaitSourceKind, WaitStatus,
        CONSOLE_INHERIT, ORIGIN_CONSOLE_NONE, ORIGIN_WIRE_LEN,
    };
    use tairix_caps::CapabilitySet;
    use tairix_curses::{Screen, Size, StreamTty};
    use tairix_login::elevate::ElevateLauncher;

    use tairix_log::{log, Event, EventId, Field, FieldValue, Level};
    use tairix_login::{
        configured_session_kind, effective_session_kind, end_live_sessions, events,
        handle_elevate_request, handle_session_request, session_environment, session_program,
        supervise, AttemptBudget, AuthenticatedUser, Authenticator, ConfigStore, ConsoleMode,
        Credentials, CursesView, DbAccounts, DbLoad, LiveSessions, Login, LoginConfig, LoginError,
        LoginStatus, LoginView, SessionDirectory, SessionKind, SessionLauncher, SessionOutcome,
        SessionWaker, StatusSource, DESKTOP_SESSION_PATH, FONTD_SERVICE_PATH, GREETER_SERVICE_PATH,
    };
    use tairix_procinfo::{call, IpcTransport};
    use tairix_rt::io::write_stderr_line;
    use tairix_rt::LogSink;
    use tairix_termcap::TermType;
    use tairix_users::{UsersDb, FONTD_UID, GREETER_UID, MAX_DB_LEN};
    use tairix_util::secret::wipe;

    /// Set once the sandboxed OS font service (`fontd`) has been started, so
    /// login launches it at most once per process (`plans/FONT-SERVICE.md`).
    /// The graphical desktop draws text through `fontd`, so login — the one
    /// holder of `CAP_SPAWN_AS_USER` on this path — starts it as the `fontd`
    /// service account the first round a display is available, and never on a
    /// headless boot (`AGENTS.md` §17.3). A duplicate start would in any case
    /// fail closed on the reserved `FONT_ENDPOINT` bind, so this guard only
    /// avoids a redundant spawn on a later round.
    static FONTD_STARTED: AtomicBool = AtomicBool::new(false);

    /// Start the sandboxed font service once, as the `fontd` service account.
    ///
    /// Called when this machine is display-capable (a graphical session may
    /// run), whether the desktop is launched by a graphical login or on demand
    /// by the shell's `desktop` command. `fontd` is a graphics-only OS
    /// resource, so it is **not** a boot-floor service (a headless machine
    /// never runs it, `AGENTS.md` §17.3); login brings it up here instead.
    /// login holds `CAP_SPAWN_AS_USER`, so it drops `fontd` onto its own
    /// service account (uid resolved from the kernel identity table, never
    /// fabricated) exactly as it drops a session onto the authenticated user.
    /// The service is detached — it outlives any one session and is not this
    /// login's child to reap — and needs no console. A refused spawn is
    /// audited loudly and login proceeds (fail loud, degrade gracefully,
    /// `AGENTS.md` §2.24): desktop text simply will not render until a font
    /// service is up.
    fn ensure_fontd(sink: &LogSink) {
        if FONTD_STARTED.swap(true, Ordering::SeqCst) {
            return;
        }
        let ret = tairix_rt::spawn_as(FONTD_SERVICE_PATH.as_bytes(), CONSOLE_INHERIT, FONTD_UID.0);
        let id = if ret < 0 {
            events::FONTD_UNAVAILABLE
        } else {
            events::FONTD_STARTED
        };
        let (level, message) = if ret < 0 {
            (
                tairix_log::Level::Warn,
                "font service could not be started; desktop text will not render until one is",
            )
        } else {
            (
                tairix_log::Level::Info,
                "font service started for the graphical session",
            )
        };
        tairix_log::log(
            sink,
            &tairix_log::Event {
                level,
                id,
                message,
                fields: &[],
            },
        );
    }

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

    /// The console line discipline behind the view: the `stream_input_mode`
    /// syscall. Raw (echo off, per-keystroke) while the view owns the
    /// screen; cooked restored for the launched session. A failed raw
    /// toggle reports `false`, and the view then refuses to read a password
    /// that would echo (fail closed).
    struct RtConsoleMode;

    impl ConsoleMode for RtConsoleMode {
        fn raw(&self) -> bool {
            tairix_rt::set_input_mode(InputMode::Raw) >= 0
        }

        fn cooked(&self) {
            let _ = tairix_rt::set_input_mode(InputMode::Cooked);
        }
    }

    /// The view's status figures, queried live from `sysinfod` and the
    /// kernel wall clock. Every figure is best-effort: a refused or failed
    /// query leaves its field `None` and the view renders a placeholder —
    /// a denied optional query never blocks a login.
    struct RtStatusSource;

    impl RtStatusSource {
        /// One payload-free query, `None` on any refusal or failure.
        fn query(query: SysinfoQueryId) -> Option<Vec<u8>> {
            call(&IpcTransport, query, &[]).ok()
        }
    }

    impl StatusSource for RtStatusSource {
        fn status(&self) -> LoginStatus {
            let mut status = LoginStatus::default();
            if let Some(identity) = Self::query(SysinfoQueryId::SYSTEM_IDENTITY)
                .and_then(|bytes| SystemIdentity::from_bytes(&bytes).ok())
            {
                // An unprovisioned machine has an empty hostname; the view
                // shows the honest placeholder rather than an invented name.
                if let Ok(name) = core::str::from_utf8(identity.hostname_bytes()) {
                    if !name.is_empty() {
                        status.hostname = Some(String::from(name));
                    }
                }
                status.version = Some((
                    identity.version_major,
                    identity.version_minor,
                    identity.version_patch,
                ));
            }
            if let Some(memory) = Self::query(SysinfoQueryId::KERNEL_MEMORY_STATS)
                .and_then(|bytes| KernelMemoryStats::from_bytes(&bytes).ok())
            {
                status.memory = Some((
                    memory.total_bytes.saturating_sub(memory.free_bytes),
                    memory.total_bytes,
                ));
            }
            if let Some(load) = Self::query(SysinfoQueryId::LOAD_AVERAGE)
                .and_then(|bytes| LoadAverage::from_bytes(&bytes).ok())
            {
                status.load = Some([load.load1, load.load5, load.load15]);
                status.tasks = Some(load.total_tasks);
                status.users = Some(load.users);
            }
            status
        }

        fn now(&self) -> Option<Time64> {
            let reading = tairix_rt::wall_time().ok()?;
            reading.state().is_set().then(|| reading.time())
        }

        fn monotonic_ns(&self) -> u64 {
            tairix_rt::clock_get()
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
    /// The waitset token naming the running child: the session, or the
    /// login screen while a graphical round is up.
    const TOKEN_CHILD: u64 = 2;
    /// The waitset token naming the `session-v1` endpoint the graphical
    /// login screen calls.
    const TOKEN_SESSION: u64 = 3;

    /// Consecutive greeters a graphical round starts before it gives up and
    /// runs the text login instead.
    ///
    /// A login screen that cannot reach an accepted verdict — it will not
    /// start, it dies, it is dismissed — must never leave the machine
    /// impossible to log in to, so the round degrades after three tries
    /// rather than restarting for ever.
    const GREETER_ATTEMPTS: u32 = 3;

    /// Bound on one wait-set park while supervising a child the set could
    /// **not** be made to watch.
    ///
    /// Normally the child is a member of the set and its exit wakes an
    /// unbounded park, so nothing periodic runs. This is the fallback for
    /// the one case with no wake source at all — a child the kernel refused
    /// to add, including one that had already gone before the add — where a
    /// bounded park plus a non-blocking reap is the only way to notice its
    /// exit. The CPU sleeps in between; nothing spins.
    const UNWATCHED_CHILD_POLL_NS: u64 = 5_000_000_000;

    /// Whether supervision of a child carries on after a served request.
    enum Watch {
        /// Keep parking until the child exits.
        Keep,
        /// Stop supervising, leaving the child running: the desktop session
        /// gave up the screen and is now a background one.
        Release,
    }

    /// How supervising a child ended.
    enum Watched {
        /// It exited, with this code.
        Exited(i32),
        /// It runs on, released by [`Watch::Release`], and keeps its place in
        /// the session table.
        Released,
    }

    /// This console's bound rendezvous — the elevation endpoint a running
    /// session's shell posts to, and the reserved `session-v1` endpoint the
    /// graphical login screen calls — plus the one wait-set that
    /// multiplexes both with the running child.
    ///
    /// Bound **once** at startup ([`ConsoleServer::bind`]): a call endpoint
    /// lives until its owning task exits (there is no destroy syscall,
    /// exactly as `sysinfod`/`journald` hold theirs), and the one wait-set
    /// is reused across rounds — only the per-round child member is added
    /// and removed — so supervision allocates no kernel object per round.
    ///
    /// The two endpoints are independently optional: a refused bind takes
    /// away only its own facility, audited, and never the other.
    struct ConsoleServer {
        /// The reusable wait-set holding both endpoint members.
        waitset: u64,
        /// Login's own kernel-attested console index: the placement every
        /// requester's attested console is checked against.
        own_console: u64,
        /// This console's `elevate_endpoint` id, when it could be bound.
        elevate: Option<u64>,
        /// The reserved `session-v1` endpoint, when it could be bound.
        session: Option<u64>,
    }

    impl ConsoleServer {
        /// Bind this console's rendezvous from login's **own** attested
        /// origin, wiring the reusable wait-set.
        ///
        /// `None` means neither facility can be served: a process with no
        /// console-backed streams has no rendezvous to serve, and no
        /// wait-set means nothing to multiplex on. A bind refusal for one
        /// endpoint (its id squatted ahead of us, no registry) leaves that
        /// one `None` and must never be "recovered" by serving elsewhere;
        /// the caller audits each absence.
        fn bind() -> Option<Self> {
            let own_console = tairix_rt::self_origin().ok()?.console();
            if own_console == ORIGIN_CONSOLE_NONE {
                return None;
            }
            let waitset = tairix_rt::waitset_create();
            if waitset < 0 {
                return None;
            }
            #[allow(clippy::cast_sign_loss)] // `waitset >= 0` is the handle encoding.
            let waitset = waitset as u64;
            let mut server = Self {
                waitset,
                own_console,
                elevate: None,
                session: None,
            };
            // Unrestricted senders on both: any process may post —
            // placement and identity are enforced per request by the
            // brokers. Capacity 1 on both: each exchange is serialised by
            // design (one elevation per console, one question at a time
            // from the login screen), so a second concurrent post fails
            // closed instead of queueing.
            server.elevate = elevate_endpoint(own_console).ok().filter(|endpoint| {
                server.bind_endpoint(
                    *endpoint,
                    ELEVATE_MAX_REQUEST,
                    ELEVATE_REPLY_LEN,
                    TOKEN_ELEVATE,
                )
            });
            if server.bind_endpoint(
                SESSION_ENDPOINT,
                SESSION_MAX_REQUEST,
                SESSION_MAX_REPLY,
                TOKEN_SESSION,
            ) {
                server.session = Some(SESSION_ENDPOINT);
            }
            Some(server)
        }

        /// Create one call endpoint and join it to the wait-set under
        /// `token`, reporting whether both steps succeeded.
        fn bind_endpoint(
            &self,
            endpoint: u64,
            max_request: usize,
            max_reply: usize,
            token: u64,
        ) -> bool {
            let empty = CapabilitySet::empty();
            if tairix_rt::call_create(endpoint, &empty, &empty, max_request, max_reply, 1) != 0 {
                return false;
            }
            tairix_rt::waitset_ctl(
                self.waitset,
                WaitSetOp::Add,
                WaitSourceKind::Endpoint,
                endpoint,
                token,
            ) == 0
        }

        /// Receive, decide, and answer one posted elevation request.
        ///
        /// The receive is non-blocking: this wait-set also supervises the
        /// session's shell child, and the queued request the wake reported
        /// may have been cancelled (its poster exited), so parking here
        /// would wedge session supervision on an endpoint with nothing to
        /// serve. An empty queue simply returns.
        ///
        /// The request buffer carries an offered password, so it is zeroed
        /// before this returns on every path. A recv failure is dropped
        /// (the poster's `ipc_call` observes its error); a reply failure is
        /// dropped likewise — the decision and its audit record already
        /// stand.
        fn serve_elevate(&self, endpoint: u64, authenticator: &dyn Authenticator, sink: &LogSink) {
            let mut request = [0u8; ELEVATE_MAX_REQUEST];
            let mut ticket = 0u64;
            let Ok(len) = tairix_rt::call_recv_nonblock(endpoint, &mut request, &mut ticket) else {
                wipe(&mut request);
                return;
            };
            let (peer_console, peer_uid) = attest(endpoint, ticket);
            let reply = handle_elevate_request(
                &request[..len],
                peer_console,
                peer_uid,
                self.own_console,
                authenticator,
                &RtElevateLauncher,
                sink,
            );
            wipe(&mut request);
            let mut reply_buf = [0u8; ELEVATE_REPLY_LEN];
            if let Ok(total) = reply.encode(&mut reply_buf) {
                let _ = tairix_rt::call_reply(endpoint, ticket, &reply_buf[..total]);
            }
        }

        /// Receive, decide, and answer one posted `session-v1` request from
        /// the graphical login screen.
        ///
        /// Non-blocking for the same reason the elevation receive is: this
        /// wait-set also supervises a child, so a wake whose queued call was
        /// cancelled must not park the loop.
        ///
        /// The request buffer carries an offered password, so it is zeroed
        /// before this returns on every path.
        ///
        /// Reports [`Watch::Release`] when the request was the presenting
        /// session giving up the screen, so the caller stops supervising it
        /// and puts the login screen back up.
        fn serve_session(
            &self,
            endpoint: u64,
            directory: &mut dyn SessionDirectory,
            authenticator: &dyn Authenticator,
            budget: &mut AttemptBudget,
            sink: &LogSink,
        ) -> Watch {
            let mut request = [0u8; SESSION_MAX_REQUEST];
            let mut ticket = 0u64;
            let Ok(len) = tairix_rt::call_recv_nonblock(endpoint, &mut request, &mut ticket) else {
                wipe(&mut request);
                return Watch::Keep;
            };
            let (peer_console, peer_uid) = attest(endpoint, ticket);
            let mut reply = [0u8; SESSION_MAX_REPLY];
            let answer = handle_session_request(
                &request[..len],
                peer_uid,
                peer_console,
                self.own_console,
                directory,
                authenticator,
                budget,
                monotonic_now(),
                sink,
                &mut reply,
            );
            wipe(&mut request);
            let delivered = answer.len > 0
                && tairix_rt::call_reply(endpoint, ticket, &reply[..answer.len]) == 0;
            // Supervision is handed back only once the session has its
            // verdict: a session that never heard "accepted" still holds the
            // seat, so releasing it would leave the login screen unable to
            // take the screen it was told was free.
            if answer.stepped_aside && delivered {
                Watch::Release
            } else {
                Watch::Keep
            }
        }

        /// Answer any request already queued on `endpoint` and discard it,
        /// so a round never inherits one posted while it was not listening.
        ///
        /// Both endpoints hold a single queued call, so one uncollected
        /// request from an unauthorised poster would otherwise block the
        /// next legitimate caller's post — an availability defect, not just
        /// a stale message.
        fn drain_session(
            &self,
            directory: &mut dyn SessionDirectory,
            authenticator: &dyn Authenticator,
            budget: &mut AttemptBudget,
            sink: &LogSink,
        ) {
            if let Some(endpoint) = self.session {
                // No child is under supervision here, so a step-aside
                // verdict has nothing to hand back to.
                let _ = self.serve_session(endpoint, directory, authenticator, budget, sink);
            }
        }

        /// Park on the wait-set until `pid` exits, handing every other wake
        /// to `serve`, and report how the watch ended.
        ///
        /// The child is joined to the set so its exit wakes the park
        /// directly and the park is unbounded. Only a child the kernel
        /// refused to add has no wake source, and that park is bounded
        /// instead, each expiry re-checking with a non-blocking reap.
        /// Nothing here spins: every iteration either parks or has just
        /// served a request.
        ///
        /// A `serve` that answers [`Watch::Release`] ends the watch with the
        /// child still running — a desktop session that stepped aside — so
        /// the caller must neither reap it nor forget it.
        fn supervise_child<F>(&self, pid: i32, mut serve: F) -> Result<Watched, Errno>
        where
            F: FnMut(u64) -> Watch,
        {
            let child_id = u64::from(pid.unsigned_abs());
            let observed = tairix_rt::waitset_ctl(
                self.waitset,
                WaitSetOp::Add,
                WaitSourceKind::Child,
                child_id,
                TOKEN_CHILD,
            ) == 0;
            let timeout = if observed {
                u64::MAX
            } else {
                UNWATCHED_CHILD_POLL_NS
            };
            let status = loop {
                let mut token = 0u64;
                let ret = tairix_rt::waitset_wait(self.waitset, timeout, &mut token);
                if ret == 0 {
                    if token == TOKEN_CHILD {
                        break plain_wait(pid).map(Watched::Exited);
                    }
                    if matches!(serve(token), Watch::Release) {
                        break Ok(Watched::Released);
                    }
                } else if errno_from(ret) != Errno::TimedOut {
                    // An unexpected wait failure must not wedge the round:
                    // reap directly rather than loop on a broken wait.
                    break plain_wait(pid).map(Watched::Exited);
                }
                if let Some(status) = reap_if_exited(pid) {
                    break Ok(Watched::Exited(status));
                }
            };
            // Drop the child's member so the reusable set never carries a
            // stale PID into the next round, whether it was reaped or
            // released still running.
            if observed {
                let _ = tairix_rt::waitset_ctl(
                    self.waitset,
                    WaitSetOp::Del,
                    WaitSourceKind::Child,
                    child_id,
                    TOKEN_CHILD,
                );
            }
            status
        }
    }

    /// The caller's kernel-attested `(console, uid)` for one received call,
    /// read from the per-call origin the kernel records — never from the
    /// message.
    ///
    /// A failed read fails closed to "no console" / "no attested uid", which
    /// every broker refuses, rather than a guessed or defaulted real
    /// identity.
    fn attest(endpoint: u64, ticket: u64) -> (u64, Option<u32>) {
        let mut origin_buf = [0u8; ORIGIN_WIRE_LEN];
        match tairix_rt::call_peer_origin(endpoint, ticket, &mut origin_buf) {
            Ok(n) => match Origin::from_bytes(&origin_buf[..n]) {
                Ok(origin) => (origin.console(), Some(origin.uid())),
                Err(_) => (ORIGIN_CONSOLE_NONE, None),
            },
            Err(_) => (ORIGIN_CONSOLE_NONE, None),
        }
    }

    /// Posts the authority's wake messages through the `ipc_send` syscall.
    ///
    /// The one delivery path for both messages a session can receive: the
    /// resume that brings it back to the screen, and the end the authority
    /// sends on its way out.
    struct RtWaker;

    impl SessionWaker for RtWaker {
        fn wake(&self, mailbox: u64, message: SessionWake) -> bool {
            let mut buf = [0u8; SESSION_WAKE_LEN];
            let Ok(len) = message.encode(&mut buf) else {
                return false;
            };
            tairix_rt::ipc_send(mailbox, &buf[..len]) == 0
        }
    }

    /// The monotonic reading the attempt budget meters against.
    ///
    /// The kernel monotonic clock, never the wall clock, so a cooldown
    /// cannot be shortened by moving the time of day. Its epoch is
    /// unspecified and only differences matter, which is exactly what the
    /// budget compares.
    fn monotonic_now() -> Duration64 {
        Duration64::from_nanos(tairix_rt::clock_get())
    }

    /// The targeted blocking reap of a child login started.
    fn plain_wait(pid: i32) -> Result<i32, Errno> {
        let mut status = 0i32;
        let wret = tairix_rt::wait_exit(pid, &mut status);
        if wret < 0 {
            return Err(errno_from(wret));
        }
        Ok(status)
    }

    /// Reap `pid` and report its exit code if it has already exited, without
    /// blocking. `None` means it is still running (or is not ours to reap).
    fn reap_if_exited(pid: i32) -> Option<i32> {
        let mut status = WaitStatus::Exited(0);
        if tairix_rt::try_wait(pid, &mut status) < 0 {
            return None;
        }
        match status {
            WaitStatus::Exited(code) => Some(code),
            // Unreachable without the stopped-report flag, and a stop is not
            // an exit: keep waiting rather than invent an exit code.
            WaitStatus::Stopped(_) => None,
        }
    }

    /// Start `user`'s session program for `kind` as that user, returning its
    /// PID.
    ///
    /// The one spawn both the text round and the graphical round use, so the
    /// environment they hand a session and the credential it starts under
    /// cannot diverge.
    ///
    /// Hands the program the session environment (USER, LOGNAME, HOME,
    /// SHELL, PWD, PATH, TERM, LANG) built from the authenticated account,
    /// so its prompt and `$USER`/`$HOME`/… reflect the real user.
    /// `spawn_with` carries both the environment and the uid switch; the env
    /// strings are data and grant no authority (every capability stays
    /// kernel-side). Privilege switches user only at process creation, never
    /// by a running process mutating its own identity, and the kernel
    /// resolves the full credential from the authoritative identity table —
    /// login chooses *which* user but never fabricates the identity. The
    /// child stays on login's own console.
    fn spawn_session(user: &AuthenticatedUser, kind: SessionKind) -> Result<i32, Errno> {
        // Allocating here is safe: a successful authentication already
        // parsed the database, so the heap is live well before this launch.
        let env_owned = session_environment(user);
        let env: Vec<&[u8]> = env_owned.iter().map(String::as_bytes).collect();
        let program = session_program(user, kind);
        let ret = tairix_rt::spawn_with(program.as_bytes(), CONSOLE_INHERIT, user.uid.0, &[], &env);
        if ret < 0 {
            return Err(errno_from(ret));
        }
        // `ret >= 0` here, so the cast preserves the PID value; PIDs fit an
        // `i32` on this ABI.
        #[allow(clippy::cast_possible_truncation)]
        Ok(ret as i32)
    }

    /// Runs one re-authenticated elevated command: `spawn_as` the target
    /// account on this console, then a targeted `wait` for exactly that
    /// child. The session's shell is blocked in its `ipc_call` for the
    /// duration (a foreground elevated command, serialised per console), so
    /// the only child that can exit here is the elevated one.
    struct RtElevateLauncher;

    impl ElevateLauncher for RtElevateLauncher {
        fn run_as(&self, program: &str, uid: u32) -> Result<i32, Errno> {
            let ret = tairix_rt::spawn_as(program.as_bytes(), CONSOLE_INHERIT, uid);
            if ret < 0 {
                return Err(errno_from(ret));
            }
            let mut status = 0i32;
            // `ret >= 0` here, so the cast preserves the PID value; PIDs
            // fit an `i32` on this ABI.
            #[allow(clippy::cast_possible_truncation)]
            let wret = tairix_rt::wait_exit(ret as i32, &mut status);
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
    /// With a [`ConsoleServer`] bound, the wait multiplexes the shell child
    /// with this console's elevation endpoint (`plans/CAPABILITY_USE.md`
    /// CU5): a posted request is re-authenticated and served while the shell
    /// blocks in its `ipc_call`, and the shell's own exit ends the session
    /// exactly as before. Without one (no console-backed streams, bind
    /// refused) the launcher degrades to the plain blocking `wait` — the
    /// session is unaffected; only elevation is unavailable, audited at
    /// startup.
    struct RtLauncher<'a> {
        server: Option<&'a ConsoleServer>,
        authenticator: &'a dyn Authenticator,
        sink: &'a LogSink,
    }

    impl SessionLauncher for RtLauncher<'_> {
        fn launch(
            &self,
            user: &AuthenticatedUser,
            kind: SessionKind,
        ) -> Result<SessionOutcome, Errno> {
            // Text runs the account's recorded shell; graphical runs the OS
            // desktop-session bundle (one mapping, defined in the library
            // beside `SessionKind`). Both are spawned **as the authenticated
            // user**: the desktop session's seat lease, input drains, and
            // frame region are that user's authority (its manifest ∩ the
            // account ceiling), never login's.
            let pid = spawn_session(user, kind)?;
            let watched = match self.server.filter(|server| server.elevate.is_some()) {
                Some(server) => server.supervise_child(pid, |token| {
                    if let (TOKEN_ELEVATE, Some(endpoint)) = (token, server.elevate) {
                        server.serve_elevate(endpoint, self.authenticator, self.sink);
                    }
                    Watch::Keep
                })?,
                None => Watched::Exited(plain_wait(pid)?),
            };
            let exit_code = match watched {
                Watched::Exited(code) => code,
                // The text round keeps no session-table entry, so no session
                // it starts can step aside; the wait can only have ended on
                // an exit, which this reap collects.
                Watched::Released => plain_wait(pid)?,
            };
            Ok(SessionOutcome { kind, exit_code })
        }
    }

    /// Whether a graphical round reached a session, or gave up on the login
    /// screen and must run the text login instead.
    enum GraphicalOutcome {
        /// A session ran and ended; open a fresh round.
        Completed,
        /// No login screen could reach an accepted verdict; run the text
        /// login this round.
        Degraded,
    }

    /// The authenticated account a `session-v1` `Accepted` verdict named,
    /// captured from the seam the broker verifies through.
    ///
    /// The broker answers a verdict and starts nothing — deliberately, so a
    /// compromised login screen cannot choose which program runs as the
    /// authenticated user. The authority therefore learns *who* was accepted
    /// from its own injected authenticator rather than from anything the
    /// login screen said.
    struct Accepted<'a> {
        inner: &'a dyn Authenticator,
        user: RefCell<Option<AuthenticatedUser>>,
    }

    impl<'a> Accepted<'a> {
        fn new(inner: &'a dyn Authenticator) -> Self {
            Self {
                inner,
                user: RefCell::new(None),
            }
        }

        /// The account accepted this round, consuming it.
        fn take(&self) -> Option<AuthenticatedUser> {
            self.user.borrow_mut().take()
        }
    }

    impl Authenticator for Accepted<'_> {
        fn authenticate(&self, credentials: &Credentials<'_>) -> Result<AuthenticatedUser, Errno> {
            let outcome = self.inner.authenticate(credentials);
            if let Ok(user) = &outcome {
                *self.user.borrow_mut() = Some(user.clone());
            }
            outcome
        }

        fn authenticate_uid(&self, uid: u32, password: &str) -> Result<AuthenticatedUser, Errno> {
            self.inner.authenticate_uid(uid, password)
        }
    }

    /// One graphical login round: put the login screen up, serve it, and
    /// bring the account it authenticated to the foreground
    /// (`plans/NEW-DESKTOP-LOGIN.md` G4).
    struct GraphicalRound<'a> {
        server: &'a ConsoleServer,
        session_endpoint: u64,
        authenticator: &'a dyn Authenticator,
        sink: &'a LogSink,
    }

    impl GraphicalRound<'_> {
        /// Run the round: restart a login screen that reaches no verdict,
        /// and degrade to the text login once the budget is spent.
        fn run(
            &self,
            db: Option<&UsersDb>,
            live: &mut LiveSessions,
            budget: &mut AttemptBudget,
        ) -> GraphicalOutcome {
            for _ in 0..GREETER_ATTEMPTS {
                let accepted = Accepted::new(self.authenticator);
                {
                    // The chooser's account list reads the session table for
                    // its live flags and a served request may write it, so
                    // this borrow ends before the table is used below.
                    let mut accounts = DbAccounts::new(db, &mut *live);
                    self.server
                        .drain_session(&mut accounts, &accepted, budget, self.sink);
                    self.serve_greeter(&mut accounts, &accepted, budget);
                }
                let Some(user) = accepted.take() else {
                    self.audit(
                        Level::Warn,
                        events::GREETER_FAILED,
                        "login screen ended with no login",
                        "-",
                    );
                    continue;
                };
                self.present(&user, live, db, budget);
                return GraphicalOutcome::Completed;
            }
            // A broken login screen must never leave the machine
            // impossible to log in to: say so where the person at the
            // console can read it, and on the audit trail.
            write_stderr_line(
                "login: the graphical login screen could not start; using the text login.",
            );
            self.audit(
                Level::Warn,
                events::GREETER_DEGRADED,
                "graphical login degraded to the text login",
                "-",
            );
            GraphicalOutcome::Degraded
        }

        /// Start one login screen and serve `session-v1` until it exits.
        fn serve_greeter(
            &self,
            directory: &mut dyn SessionDirectory,
            accepted: &Accepted<'_>,
            budget: &mut AttemptBudget,
        ) {
            // The login screen runs as its own service account: it draws and
            // types, and holds no authority to read the user database or
            // start a process.
            let ret = tairix_rt::spawn_as(
                GREETER_SERVICE_PATH.as_bytes(),
                CONSOLE_INHERIT,
                GREETER_UID.0,
            );
            if ret < 0 {
                return;
            }
            // `ret >= 0` here, so the cast preserves the PID value.
            #[allow(clippy::cast_possible_truncation)]
            let pid = ret as i32;
            let _ = self.server.supervise_child(pid, |token| {
                match token {
                    TOKEN_SESSION => {
                        // Nothing presents while the login screen is up, so a
                        // step-aside cannot be honoured here and the greeter
                        // is watched until it exits.
                        let _ = self.server.serve_session(
                            self.session_endpoint,
                            directory,
                            accepted,
                            budget,
                            self.sink,
                        );
                    }
                    TOKEN_ELEVATE => {
                        if let Some(endpoint) = self.server.elevate {
                            self.server
                                .serve_elevate(endpoint, self.authenticator, self.sink);
                        }
                    }
                    _ => {}
                }
                Watch::Keep
            });
        }

        /// Bring `user`'s desktop session to the foreground and supervise it
        /// until it ends or steps aside.
        ///
        /// An account that already has a live session is **resumed** through
        /// its wake mailbox; a second desktop is never started for one
        /// account. A wake that cannot be delivered means the session may be
        /// gone, so it is reaped non-blockingly to find out: a reaped
        /// session leaves the table and a fresh desktop starts, while one
        /// that is still running keeps its entry and the round returns to the
        /// login screen rather than duplicating it.
        ///
        /// The two ways supervision ends are kept apart. A session that
        /// **exited** loses its table entry and is audited as ended. One that
        /// **stepped aside** keeps both its processes and its entry, so the
        /// next round offers it as live and can resume it; the broker already
        /// audited that decision, so nothing is recorded twice.
        fn present(
            &self,
            user: &AuthenticatedUser,
            live: &mut LiveSessions,
            db: Option<&UsersDb>,
            budget: &mut AttemptBudget,
        ) {
            let mut resumed = None;
            if let Some((task, mailbox)) = live
                .get(&user.username)
                .map(|session| (session.pid(), session.wake_endpoint()))
            {
                let Ok(pid) = i32::try_from(task) else {
                    self.audit(
                        Level::Warn,
                        events::SESSION_LAUNCH_FAILED,
                        "live desktop session cannot be supervised",
                        &user.username,
                    );
                    return;
                };
                if RtWaker.wake(mailbox, SessionWake::Foreground) {
                    let _ = live.set_foreground(&user.username);
                    self.audit(
                        Level::Info,
                        events::SESSION_RESUMED,
                        "desktop session resumed",
                        &user.username,
                    );
                    resumed = Some(pid);
                } else if reap_if_exited(pid).is_some() {
                    // It was already gone, so its slot frees for a fresh one.
                    let _ = live.remove(&user.username);
                } else {
                    self.audit(
                        Level::Warn,
                        events::SESSION_LAUNCH_FAILED,
                        "live desktop session could not be resumed",
                        &user.username,
                    );
                    return;
                }
            }
            let pid = match resumed {
                Some(pid) => pid,
                None => {
                    let Ok(pid) = spawn_session(user, SessionKind::Graphical) else {
                        self.audit(
                            Level::Warn,
                            events::SESSION_LAUNCH_FAILED,
                            "desktop session could not be started",
                            &user.username,
                        );
                        return;
                    };
                    // The resume branch above owns the case where an entry
                    // already exists, so this insert cannot collide.
                    let _ = live.insert(&user.username, user.uid.0, u64::from(pid.unsigned_abs()));
                    self.audit(
                        Level::Info,
                        events::SESSION_STARTED,
                        "desktop session started",
                        &user.username,
                    );
                    pid
                }
            };
            let watched = {
                let mut accounts = DbAccounts::new(db, &mut *live);
                self.server.supervise_child(pid, |token| {
                    match token {
                        TOKEN_SESSION => {
                            return self.server.serve_session(
                                self.session_endpoint,
                                &mut accounts,
                                self.authenticator,
                                budget,
                                self.sink,
                            );
                        }
                        TOKEN_ELEVATE => {
                            if let Some(endpoint) = self.server.elevate {
                                self.server
                                    .serve_elevate(endpoint, self.authenticator, self.sink);
                            }
                        }
                        _ => {}
                    }
                    Watch::Keep
                })
            };
            if matches!(watched, Ok(Watched::Released)) {
                return;
            }
            let _ = live.remove(&user.username);
            self.audit(
                Level::Info,
                events::SESSION_ENDED,
                "desktop session ended",
                &user.username,
            );
        }

        /// Record one round decision, naming the account it concerns.
        fn audit(&self, level: Level, id: EventId, message: &str, user: &str) {
            log(
                self.sink,
                &Event {
                    level,
                    id,
                    message,
                    fields: &[Field {
                        key: "user",
                        value: FieldValue::Str(user),
                    }],
                },
            );
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
        let state = match tairix_rt::users_db_read(&mut buf) {
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
        wipe(&mut buf);
        state
    }

    /// The session type this round runs by default: the operator's one-boot
    /// Supervisor choice (`continue text` / `continue gui`) where they made
    /// one, otherwise the administrator-configured `os.loginType` store
    /// value. The precedence itself lives in the library beside
    /// [`SessionKind`], so the rule has one definition.
    ///
    /// Both inputs are re-read each round: a `configure os.loginType`
    /// change takes effect at the next prompt, and the kernel's record of
    /// the boot choice is immutable, so re-reading it costs a register
    /// read and can never go stale.
    fn session_default() -> SessionKind {
        effective_session_kind(tairix_rt::boot_session(), configured_session_default())
    }

    /// The administrator-configured boot-default session type
    /// (`os.loginType`): this performs the I/O and hands what it found to
    /// [`configured_session_kind`], the one definition of what it means.
    ///
    /// The store's own directory is probed first, because "this machine
    /// carries no configuration" and "the volume holding it is not up yet"
    /// must not read alike. A round before the root unlock mounts
    /// `/System/Settings` can reach neither, and treating that as an absent
    /// store would silently boot the compiled default over an
    /// administrator's opposite choice; a reachable directory with no
    /// document is a genuine "no configuration" and does take the default.
    /// A refused read, an oversized or malformed document is likewise a
    /// reachable store that teaches nothing, and never breaks the login.
    fn configured_session_default() -> SessionKind {
        let dir = tairix_rt::fs_open(
            tairix_sysconfig::CONFIG_DIR.as_bytes(),
            OpenFlags::DIRECTORY,
        );
        if dir < 0 {
            return configured_session_kind(ConfigStore::Unreachable);
        }
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        // `dir >= 0` checked above; descriptors are small kernel indices.
        let _ = tairix_rt::fs_close(dir as u32);

        let fd = tairix_rt::fs_open(tairix_sysconfig::CONFIG_PATH.as_bytes(), OpenFlags::READ);
        if fd < 0 {
            return configured_session_kind(ConfigStore::Reachable(None));
        }
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        // `fd >= 0` checked above; descriptors are small kernel indices.
        let fd = fd as u32;
        // One bounded read suffices: the engine refuses any document
        // longer than its own ceiling, so a store that does not fit this
        // buffer could never parse anyway.
        let mut buf = [0u8; tairix_sysconfig::MAX_CONFIG_LEN];
        let outcome = tairix_rt::fs_read(fd, 0, &mut buf);
        let _ = tairix_rt::fs_close(fd);
        configured_session_kind(ConfigStore::Reachable(
            outcome.ok().and_then(|len| buf.get(..len)),
        ))
    }

    /// Whether a graphical session can be launched on this round: the
    /// desktop bundle is installed **and** a display service is live.
    /// Both facts are re-checked per round — a service that came up (or a
    /// bundle installed) after boot arms a configured graphical default at
    /// the next prompt, and one that vanished degrades it to text again —
    /// and both fail closed: any refusal selects the text shell, never an
    /// errored login.
    ///
    /// * **Bundle presence** — a read-only `fs_open` of the desktop
    ///   bundle's `Run` path through the secured VFS (per-inode
    ///   authorisation under login's own attested identity; `/System` is
    ///   world-readable). The descriptor is closed immediately: the probe
    ///   wants existence, not bytes.
    /// * **Display service** — one `Query` call to the reserved
    ///   `DISPLAY_ENDPOINT`. Login holds no seat lease, so a live service
    ///   answers with a typed refusal — but *any* well-formed reply proves
    ///   a display service is serving the rendezvous (only a
    ///   `CAP_IPC_BIND_PRIVILEGED` holder can bind a reserved id), while an
    ///   unbound endpoint fails the call itself with `NotFound`. The probe
    ///   learns nothing about the seat and gains no authority.
    fn graphical_session_available() -> bool {
        // Both bundles: the desktop session a login starts, and the login
        // screen that collects the credential for it. A machine with a
        // display but no greeter bundle has no graphical login to offer, so
        // it degrades to text like any other absence.
        if !bundle_present(DESKTOP_SESSION_PATH) || !bundle_present(GREETER_SERVICE_PATH) {
            return false;
        }
        let request = DisplayRequest::Query {
            seat_id: SEAT_PRIMARY,
        }
        .to_le_bytes();
        let mut reply = [0u8; DISPLAY_MODE_REPLY_LEN];
        tairix_rt::ipc_call(DISPLAY_ENDPOINT, &request, &mut reply).is_ok()
    }

    /// Whether `path` can be opened read-only, closed again at once.
    ///
    /// An existence probe, not a load: it reads no bytes and keeps no
    /// descriptor, so it grants nothing and cannot leak one per round.
    fn bundle_present(path: &str) -> bool {
        let fd = tairix_rt::fs_open(path.as_bytes(), OpenFlags::READ);
        if fd < 0 {
            return false;
        }
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        // `fd >= 0` checked above; descriptors are small kernel indices.
        let _ = tairix_rt::fs_close(fd as u32);
        true
    }

    /// Run one login round: prompt → authenticate → run the session to
    /// completion against `authenticator`. Returns `true` to open another
    /// round, `false` when the console is dead and the process should exit
    /// (PID 1 relaunches it). The session launcher serves this console's
    /// elevation endpoint (when bound) with the **same** authenticator the
    /// prompt used, so an elevation re-authenticates against exactly the
    /// database this round authenticated against.
    ///
    /// A round where a graphical login is possible — the two bundles are
    /// installed, a display service answers, the `session-v1` endpoint is
    /// bound, and the configured default asks for it — puts the graphical
    /// login screen up instead of the text prompt
    /// (`plans/NEW-DESKTOP-LOGIN.md` G4). Every absence, and a login screen
    /// that repeatedly fails, degrades to that text prompt in the same
    /// round; nothing here can leave the console without a way in.
    fn login_round(
        view: &dyn LoginView,
        server: Option<&ConsoleServer>,
        authenticator: &dyn Authenticator,
        db: Option<&UsersDb>,
        live: &mut LiveSessions,
        budget: &mut AttemptBudget,
        sink: &LogSink,
    ) -> bool {
        let launcher = RtLauncher {
            server,
            authenticator,
            sink,
        };
        // Re-probed each round: whether a graphical session is possible this
        // round (both bundles are installed and a display service is live).
        // It both selects a configured graphical default (degrading to
        // text — never an error — when unavailable) and gates bringing up the
        // font service.
        let graphical_available = graphical_session_available();
        // The graphical desktop draws text through the sandboxed OS font
        // service, whether it is launched by a graphical login or on demand by
        // the shell's `desktop` command. So bring `fontd` up (once) as soon as
        // this machine is display-capable — not as a boot-floor service (a
        // headless machine, where this stays false, never runs it, `AGENTS.md`
        // §17.3) and not tied to one launch path. login holds
        // `CAP_SPAWN_AS_USER`, the authority the graphics-only service account
        // needs and neither the shell nor the desktop app has.
        if graphical_available {
            ensure_fontd(sink);
        }
        // The same rule the text state machine applies: a graphical session
        // only when this round can start one and the resolved default asks
        // for it.
        let session_default = session_default();
        if graphical_available && session_default == SessionKind::Graphical {
            if let Some((server, endpoint)) =
                server.and_then(|server| server.session.map(|endpoint| (server, endpoint)))
            {
                let round = GraphicalRound {
                    server,
                    session_endpoint: endpoint,
                    authenticator,
                    sink,
                };
                if matches!(round.run(db, live, budget), GraphicalOutcome::Completed) {
                    return true;
                }
            }
        }
        let login = Login::new(LoginConfig {
            max_attempts: MAX_ATTEMPTS,
            graphical_available,
            session_default,
            view,
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

    /// Program entry point. `tairix-rt`'s `_start` calls it once the runtime
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
        // Bind this console's two rendezvous once for the process's lifetime
        // (an endpoint dies with the task). Each absence is audited and
        // takes away only its own facility: an `elevate` request then fails
        // closed at the missing endpoint, and a round with no `session-v1`
        // endpoint runs the text login rather than claiming a rendezvous
        // nothing answers.
        let server = ConsoleServer::bind();
        if server.as_ref().and_then(|server| server.elevate).is_none() {
            unavailable(
                &sink,
                events::ELEVATE_UNAVAILABLE,
                "elevation endpoint unavailable; sessions run without a broker",
            );
        }
        if server.as_ref().and_then(|server| server.session).is_none() {
            unavailable(
                &sink,
                events::SESSION_ENDPOINT_UNAVAILABLE,
                "session endpoint unavailable; rounds use the text login",
            );
        }
        // The machine's live desktop sessions and the per-account guess
        // meter both outlive a round: switching between two logged-in
        // accounts, and a cooldown earned at the login screen, must survive
        // the round that created them.
        let mut live = LiveSessions::new();
        let mut budget = AttemptBudget::new();
        // While the database read is `Pending` (the encrypted root is still
        // being unlocked) `supervise` calls this to **block** until the
        // database becomes available, so the in-kernel unlock kthread runs
        // and `login` neither prompts nor reads the console. The kernel
        // parks the task off the run queue and wakes it the instant the
        // unlock resolves (`users_db_wait`) — never the busy yield loop that
        // flooded the boot log with one `users_db_read` rejection per poll. The wait's result is advisory: `supervise`
        // re-reads the database on the next round regardless of whether the
        // wait was woken or timed out.
        // One view for the process's lifetime: the failed-attempt counter
        // accumulates across rounds until a session launches, and the
        // screen is re-entered at each round. The terminal geometry comes
        // from the console stream; a console that cannot report one gets
        // the classic 80×25 rather than no login at all.
        let size = tairix_rt::terminal_size(1)
            .map(|s| Size::new(s.rows(), s.cols()))
            .unwrap_or(Size::new(25, 80));
        let screen = Screen::new(StreamTty, TermType::Xterm256Color, size);
        let view = CursesView::new(screen, RtStatusSource, RtConsoleMode);
        supervise(
            load_users_db,
            || {
                let _ = tairix_rt::users_db_wait(DB_WAIT_TIMEOUT_NS);
            },
            |authenticator, db| {
                login_round(
                    &view,
                    server.as_ref(),
                    authenticator,
                    db,
                    &mut live,
                    &mut budget,
                    &sink,
                )
            },
        );
        // The console is dead and this process is about to go, so nothing
        // would ever wake a session left recorded as background again.
        end_live_sessions(&mut live, &RtWaker, &sink);
        1
    }

    /// Audit a rendezvous this process could not bind.
    fn unavailable(sink: &LogSink, id: EventId, message: &str) {
        log(
            sink,
            &Event {
                level: Level::Warn,
                id,
                message,
                fields: &[],
            },
        );
    }

    tairix_rt::entry!(main);
}

// --- Host stub ----------------------------------------------------------
//
// On the host (`cargo build --workspace`, clippy, fmt) the program's real
// entry — the freestanding `tairix-rt` `_start` path — is not compiled, so
// this inert `main` keeps the crate building under the host tooling. It
// performs no I/O.
#[cfg(not(freestanding))]
fn main() {}
