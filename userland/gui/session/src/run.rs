//! The `Run` entry-point binary of the `desktop` command app, installed
//! as a signed bundle in the system app store (`/System/Apps/`) and
//! started two ways through the one bundle: a graphical login
//! (`os.loginType graphical`) spawns it as the authenticated user's
//! session, and a shell user starts it on demand by typing `desktop`
//! (`plans/DISPLAY.md` D7c, `plans/APPS.md`). The reserved `-h`/`-?`
//! switches serve the command's own short help; the grammar is otherwise
//! closed (see [`tairix_desktop_session::cli`]).
//!
//! This is the client half of the zero-copy, lease-gated present path: the
//! session acquires the boot seat's exclusive, revocable lease, brings the
//! display client up over the reserved `DISPLAY_ENDPOINT` (query the mode →
//! create the shared double-buffered frame region → grant it to the display
//! service → configure), and then runs the desktop from its wait-set: it
//! parks on a `SeatInput` member (woken by input delivery *and* by lease
//! loss), drains the owned seat's pointer and keyboard channels through the
//! session crate's fail-closed record path, pumps each decoded event
//! through the `DesktopShell`, and presents the composited damage by
//! frame index — no frame bytes ever cross the IPC.
//!
//! It is a **pure-Rust** program: TAIRiX is Rust-only, so it links the Rust
//! userland runtime `tairix-rt`, never the C ABI (which exists solely for
//! non-Rust programs). `tairix-rt` provides `_start`, the per-process stack
//! canary, the panic handler, the allocator, and the syscall wrappers;
//! `tairix_rt::entry!` names this program's `main`.
//!
//! `main` wires the real seams the shared engines drive:
//!
//! * `display_acquire(SEAT_PRIMARY)`: the kernel binds this task as the
//!   seat's owner and mints the revocable lease. Every later drain and
//!   present is owner-gated kernel-side against that live lease — the
//!   session holds no oracle and asserts nothing.
//! * `DisplayClient` over `ipc_call` to the reserved `DISPLAY_ENDPOINT`:
//!   the display service re-checks the caller's live lease per request via
//!   `call_peer_seat`, so a stale session cannot scribble on a switched
//!   seat.
//! * `shm_create` + `shm_grant`: the frame region is the session's own
//!   kernel-zeroed mapping, granted *to the serving task of the display
//!   endpoint* — never to a raw, recyclable PID.
//! * `SeatEventReader` over the seat-addressed `pointer_read` /
//!   `keyboard_read`: each drain is `CAP_INPUT_READ`- and owner-gated
//!   kernel-side; a truncated or malformed record fails closed in the
//!   session crate's one validation path, never decoding as a spurious
//!   event.
//! * The `SeatInput` wait-set member: the session parks between events —
//!   never a poll loop — and is woken by input *or* by losing the seat, so
//!   a revoked session observes the typed refusal on its very next drain
//!   and tears down fail-loud instead of parking forever or repainting
//!   blind.
//!
//! Loss of the seat (`SeatRevoked` / `SeatNotOwner` on any drain or
//! present) ends the session with its reason on `stderr` and a reserved
//! exit code; the spawning supervisor decides whether a fresh session (with
//! a fresh acquire and a fresh configure) replaces it. Every other fault —
//! a dead display service, a refused wait-set, a malformed input record —
//! is equally fail-loud: the session never spins, never guesses a mode, and
//! never repaints without a live lease.
//!
//! On the host it is an inert stub so `cargo build --workspace`, clippy,
//! and fmt still cover the file.

#![cfg_attr(freestanding, no_std)]
#![cfg_attr(freestanding, no_main)]
#![deny(missing_docs)]

// --- Pure-Rust program --------------------------------------------------
#[cfg(freestanding)]
mod program {
    use alloc::collections::BTreeMap;

    use tairix_abi::display_ipc::DISPLAY_ENDPOINT;
    use tairix_abi::input::KeyInput;
    use tairix_abi::seat::SEAT_PRIMARY;
    use tairix_abi::window_ipc::{PointerAction, WindowEvent, WINDOW_ENDPOINT, WINDOW_MAX_REQUEST};
    use tairix_abi::{
        DriverError, Errno, OpenFlags, Origin, ProcId, WaitFlags, WaitSetOp, WaitSourceKind,
        WaitStatus, CONSOLE_INHERIT, ORIGIN_WIRE_LEN, SPAWN_UID_INHERIT, WAIT_PID_ANY,
    };
    use tairix_browse::{DirectorySource, VfsDirectorySource};
    use tairix_caps::CapabilitySet;
    use tairix_desktop_session::{
        parse, reap_launched, window_control_event, CliError, Command, ConcludedPick, DesktopShell,
        DeviceInputSource, KeyboardInputSource, PickConclusion, SeatEventReader, SeatInputChannel,
        SessionPicker, SessionWindows, ShellWindowHost, APPEARANCE_LABEL, FILES_LABEL,
        FILES_LAUNCHER, FILES_RUN_PATH, TERMINAL_LABEL, TERMINAL_LAUNCHER, TERMINAL_RUN_PATH,
        USAGE, VIEWER_LABEL, VIEWER_LAUNCHER, VIEWER_RUN_PATH,
    };
    use tairix_display::{DisplayClient, DisplayTransport, RemoteDisplay, RtShmMapper};
    use tairix_help::{own_short_help, BundleHelp};
    use tairix_rt::io::{write_stderr_line, Stdout, Write};
    use tairix_taskbar::{MenuAction, TaskbarConfig, TaskbarResponse};
    use tairix_window::{CallerIdentity, EventSink, WindowServer, WINDOW_REPLY_MAX};
    use tairix_wm::{Compositor, InputResponse, Rect};

    extern crate alloc;

    /// Exit code when the boot seat's lease could not be acquired (held by
    /// another session, or the manifest lacks `CAP_DISPLAY`). A reserved,
    /// fail-closed value.
    const EXIT_NO_SEAT: i32 = 90;

    /// Exit code when the display service could not be reached or refused
    /// the bring-up handshake (no bound endpoint, a refused query, a
    /// refused configure). A reserved, fail-closed value: the session never
    /// renders against a guessed mode.
    const EXIT_NO_DISPLAY: i32 = 91;

    /// Exit code when the queried mode is unusable (zero-sized, or its
    /// frame arithmetic overflows the address width). A reserved,
    /// fail-closed value.
    const EXIT_BAD_MODE: i32 = 92;

    /// Exit code when the shared frame region could not be created or
    /// granted to the display service. A reserved, fail-closed value.
    const EXIT_NO_FRAMES: i32 = 93;

    /// Exit code when the wait-set the session parks on could not be
    /// created, populated, or waited on. A reserved, fail-closed value: the
    /// session exits rather than degrade into a busy re-poll.
    const EXIT_WAIT_FAILED: i32 = 94;

    /// Exit code when the seat lease was lost (revoked by the seat manager
    /// or released from under the session): the typed `SeatRevoked` /
    /// `SeatNotOwner` observed on a drain or present. The session's normal
    /// fail-loud teardown, never an error in the session itself.
    const EXIT_SEAT_LOST: i32 = 95;

    /// Exit code when a seat input drain faulted for a reason other than
    /// losing the lease (a malformed record surfaced by the fail-closed
    /// decode path). A reserved value: an untrustworthy input stream ends
    /// the session rather than being skipped over.
    const EXIT_INPUT_FAULT: i32 = 96;

    /// Exit code when a present was refused for a reason other than losing
    /// the lease (a dead display service, a device fault). A reserved,
    /// fail-closed value.
    const EXIT_PRESENT_FAILED: i32 = 97;

    /// Exit code when the reserved `WINDOW_ENDPOINT` could not be bound.
    /// The kernel authorises the bind by this session's live seat lease
    /// (no privileged-bind capability), so a refusal means the lease is
    /// gone or another server already claimed the rendezvous — exit
    /// fail-loud, never serve a desktop apps cannot reach.
    const EXIT_NO_WINDOW_ENDPOINT: i32 = 98;

    /// Frames in the shared region: a double buffer, so the session renders
    /// into one frame while the service scans out the other.
    const FRAME_COUNT: u32 = 2;

    /// The wait-set token of the session's `SeatInput` member.
    const SEAT_TOKEN: u64 = 1;

    /// The wait-set token of the served `WINDOW_ENDPOINT` member.
    const WINDOW_TOKEN: u64 = 2;

    /// The wait-set token of the any-child member: a spawned app exiting
    /// wakes the loop so its windows are torn down promptly.
    const CHILD_TOKEN: u64 = 3;

    /// Outstanding-call capacity of the window endpoint (a fail-closed
    /// memory bound): every app calls synchronously, so a small queue
    /// covers several concurrent clients.
    const WINDOW_CAPACITY: usize = 8;

    /// Recover the [`Errno`] a syscall encoded as a negative register
    /// (`-ret`); an unrecognised code fails closed as
    /// [`Errno::NotImplemented`] rather than being guessed.
    fn errno_from(ret: i64) -> Errno {
        i32::try_from(-ret)
            .ok()
            .and_then(Errno::from_i32)
            .unwrap_or(Errno::NotImplemented)
    }

    /// State the abnormal-exit reason on `stderr` (fail loud: an exit code
    /// alone is not a diagnosis) and hand back `code` for `main` to return.
    fn fail(code: i32, reason: &str) -> i32 {
        let _ = tairix_rt::stderr(b"desktop: ");
        let _ = tairix_rt::stderr(reason.as_bytes());
        let _ = tairix_rt::stderr(b"\n");
        code
    }

    /// The production [`DisplayTransport`]: one synchronous `ipc_call` to
    /// the reserved display endpoint per request. The display service
    /// re-checks the caller's live seat lease kernel-side on every request,
    /// so the transport carries no claimed authority.
    struct RtDisplayTransport;

    impl DisplayTransport for RtDisplayTransport {
        fn call(&mut self, request: &[u8], reply: &mut [u8]) -> Result<usize, Errno> {
            tairix_rt::ipc_call(DISPLAY_ENDPOINT, request, reply).map_err(errno_from)
        }
    }

    /// The production pointer [`SeatEventReader`]: the seat-addressed
    /// `pointer_read` drain of the boot seat's pointer channel, owner-gated
    /// kernel-side against the live lease on every call.
    struct PointerReader;

    impl SeatEventReader for PointerReader {
        fn read(&mut self, buf: &mut [u8]) -> Result<usize, Errno> {
            let ret = tairix_rt::pointer_read(SEAT_PRIMARY, buf);
            if ret < 0 {
                return Err(errno_from(ret));
            }
            // A count the address width cannot hold is refused, never
            // truncated into a shorter, decodable-looking record.
            usize::try_from(ret).map_err(|_| Errno::LengthOutOfRange)
        }
    }

    /// The production keyboard [`SeatEventReader`]: the seat-addressed
    /// `keyboard_read` drain of the boot seat's keyboard channel,
    /// owner-gated kernel-side against the live lease on every call.
    struct KeyboardReader;

    impl SeatEventReader for KeyboardReader {
        fn read(&mut self, buf: &mut [u8]) -> Result<usize, Errno> {
            let ret = tairix_rt::keyboard_read(SEAT_PRIMARY, buf);
            if ret < 0 {
                return Err(errno_from(ret));
            }
            // A count the address width cannot hold is refused, never
            // truncated into a shorter, decodable-looking record.
            usize::try_from(ret).map_err(|_| Errno::LengthOutOfRange)
        }
    }

    /// The production [`CallerIdentity`]: the kernel's `call_peer_origin`
    /// on the served window endpoint, so every request is attributed to
    /// the kernel-attested in-flight caller — never a claim the request
    /// carried. Each attested caller's `(pid → ProcId)` pair is retained
    /// so a reaped child pid resolves back to the client whose windows
    /// must be torn down.
    struct RtWindowIdentity {
        peers: BTreeMap<u64, ProcId>,
    }

    impl RtWindowIdentity {
        const fn new() -> Self {
            Self {
                peers: BTreeMap::new(),
            }
        }

        /// Resolve (and forget) the client that ran as child `pid`.
        fn take_by_pid(&mut self, pid: u64) -> Option<ProcId> {
            self.peers.remove(&pid)
        }

        /// The kernel task id the attested client `id` called as, if it
        /// has called this session. The delegation target of a concluded
        /// pick: the pid came from `call_peer_origin`, never a wire claim,
        /// and task ids are never reused, so `fd_grant` lands on exactly
        /// the process whose window asked.
        fn pid_of(&self, id: ProcId) -> Option<u64> {
            self.peers
                .iter()
                .find(|(_, proc_id)| **proc_id == id)
                .map(|(pid, _)| *pid)
        }
    }

    impl CallerIdentity for RtWindowIdentity {
        fn caller(&mut self, ticket: u64) -> Result<ProcId, Errno> {
            let mut buf = [0u8; ORIGIN_WIRE_LEN];
            let len = tairix_rt::call_peer_origin(WINDOW_ENDPOINT, ticket, &mut buf)
                .map_err(errno_from)?;
            let origin = Origin::from_bytes(&buf[..len])?;
            self.peers.insert(origin.pid(), origin.proc_id());
            Ok(origin.proc_id())
        }
    }

    /// The production [`EventSink`]: one non-blocking `ipc_send` to the
    /// owning app's event port per event. The send never parks this
    /// session (a full mailbox or a dead port is a typed refusal), so a
    /// wedged app can never wedge the desktop.
    struct RtEventSink;

    impl EventSink for RtEventSink {
        fn deliver(
            &mut self,
            endpoint: u64,
            event: &[u8; WindowEvent::WIRE_LEN],
        ) -> Result<(), Errno> {
            let ret = tairix_rt::ipc_send(endpoint, event);
            if ret == 0 {
                Ok(())
            } else {
                Err(errno_from(ret))
            }
        }
    }

    /// Classify one drain fault: losing the seat is the session's normal
    /// fail-loud teardown; anything else is an untrustworthy input stream.
    fn drain_fault(err: Errno) -> i32 {
        match err {
            Errno::SeatRevoked | Errno::SeatNotOwner => {
                fail(EXIT_SEAT_LOST, "seat lease lost; tearing the session down")
            }
            _ => fail(EXIT_INPUT_FAULT, "seat input drain faulted"),
        }
    }

    /// Present the composited damage through the remote display, mapping a
    /// refusal onto the session's exit codes. The service refuses a caller
    /// whose lease is no longer live (`SeatRevoked` from the kernel's
    /// per-request check; a stale owner surfaces as a permission refusal),
    /// so a lost seat is observed here exactly as on a drain.
    fn present(
        compositor: &mut Compositor,
        display: &mut RemoteDisplay<'_, RtDisplayTransport>,
    ) -> Result<(), i32> {
        match compositor.present(display) {
            Ok(()) => Ok(()),
            Err(DriverError::SeatRevoked | DriverError::PermissionDenied) => Err(fail(
                EXIT_SEAT_LOST,
                "seat lease lost; tearing the session down",
            )),
            Err(_) => Err(fail(EXIT_PRESENT_FAILED, "display present refused")),
        }
    }

    /// Bring the desktop up and run it until the seat is lost or a fault
    /// ends it. Split from `main` so every exit path after the acquire
    /// flows back through the one owner-checked `display_release`.
    fn session() -> i32 {
        // --- Display bring-up: query → shared frames → grant → configure.
        let mut client = DisplayClient::new(RtDisplayTransport, SEAT_PRIMARY);
        let Ok(mode) = client.query() else {
            return fail(
                EXIT_NO_DISPLAY,
                "display service unreachable or refused the mode query",
            );
        };
        // The region holds FRAME_COUNT frames back to back, each shaped
        // exactly as the queried mode; the arithmetic is checked so a
        // hostile or corrupt mode can never size a short region.
        let Some(frame_len) = u64::from(mode.stride_bytes)
            .checked_mul(u64::from(mode.height_px))
            .and_then(|bytes| usize::try_from(bytes).ok())
        else {
            return fail(EXIT_BAD_MODE, "frame geometry overflows");
        };
        let Some(total) = frame_len.checked_mul(FRAME_COUNT as usize) else {
            return fail(EXIT_BAD_MODE, "frame geometry overflows");
        };
        if frame_len == 0 {
            return fail(EXIT_BAD_MODE, "queried mode is zero-sized");
        }
        let mut region_id: u64 = 0;
        let base = tairix_rt::shm_create(total, &mut region_id);
        if base < 0 {
            return fail(EXIT_NO_FRAMES, "shared frame region refused");
        }
        let grant = tairix_rt::shm_grant(region_id, DISPLAY_ENDPOINT);
        if grant < 1 {
            return fail(EXIT_NO_FRAMES, "frame region grant refused");
        }
        #[allow(clippy::cast_sign_loss)] // `grant >= 1` checked above; it is a kernel handle.
        if client.configure(grant as u64, FRAME_COUNT, &mode).is_err() {
            return fail(EXIT_NO_DISPLAY, "display service refused the configure");
        }
        let Ok(base) = usize::try_from(base) else {
            return fail(
                EXIT_NO_FRAMES,
                "frame region base outside the address width",
            );
        };
        // SAFETY: the kernel mapped at least `total` zeroed bytes read/write
        // into this process at `base` (`shm_create` maps the exact length it
        // was asked for) and the mapping stays live for the life of the
        // process — nothing below unmaps or aliases it. The display service
        // maps the same frames read-only for its blit, and the protocol
        // serialises access: this session is parked in its present call
        // while the service reads, so the two never race on the presented
        // bytes.
        let frames = unsafe { core::slice::from_raw_parts_mut(base as *mut u8, total) };
        let Ok(mut display) = RemoteDisplay::new(client, mode, frames, FRAME_COUNT) else {
            return fail(EXIT_BAD_MODE, "queried mode rejected by the frame ring");
        };

        // --- Desktop bring-up: the shell, the compositor over the active
        // theme's desktop colour, and the two live seat input sources with
        // the queried mode as the pointer's screen rectangle.
        let mut shell = DesktopShell::new(
            TaskbarConfig::bottom_bar(mode.width_px, mode.height_px),
            APPEARANCE_LABEL,
        );
        let Some(mut compositor) = Compositor::new(mode, shell.desktop_background()) else {
            return fail(EXIT_BAD_MODE, "compositor rejected the queried mode");
        };
        let screen = Rect::new(0, 0, mode.width_px, mode.height_px);
        let Ok(mut pointer) = DeviceInputSource::new(SeatInputChannel::new(PointerReader), screen)
        else {
            return fail(EXIT_BAD_MODE, "queried mode has no pointer surface");
        };
        let mut keyboard = KeyboardInputSource::new(SeatInputChannel::new(KeyboardReader));

        // The start menu's launcher entries for the file browser and the
        // terminal: selecting one is forwarded by the shell and spawns
        // the matching bundle below.
        let _ = shell
            .session_mut()
            .taskbar_mut()
            .start_menu_mut()
            .add_launcher(FILES_LAUNCHER, FILES_LABEL);
        let _ = shell
            .session_mut()
            .taskbar_mut()
            .start_menu_mut()
            .add_launcher(TERMINAL_LAUNCHER, TERMINAL_LABEL);
        let _ = shell
            .session_mut()
            .taskbar_mut()
            .start_menu_mut()
            .add_launcher(VIEWER_LAUNCHER, VIEWER_LABEL);

        // First frame: place the bar, install the pointer cursor at the
        // seat's initial pointer position, and push the whole surface once;
        // every later present carries only the composited damage. The cursor
        // is then kept live by the shell as each seat event is pumped.
        shell.present(&mut compositor);
        shell.refresh_cursor(&mut compositor);
        if let Err(code) = present(&mut compositor, &mut display) {
            return code;
        }

        // Bind the reserved window rendezvous. The kernel authorises the
        // bind by this session's kernel-attested live seat lease (the one
        // seat-scoped reserved id); the endpoint is unrestricted-sender —
        // the engine attests every caller per request and keys each window
        // to its creator, so an unentitled sender only ever reaches typed
        // refusals.
        let empty = CapabilitySet::empty();
        if tairix_rt::call_create(
            WINDOW_ENDPOINT,
            &empty,
            &empty,
            WINDOW_MAX_REQUEST,
            WINDOW_REPLY_MAX,
            WINDOW_CAPACITY,
        ) != 0
        {
            return fail(EXIT_NO_WINDOW_ENDPOINT, "window endpoint bind refused");
        }

        // Park on the wait-set: the seat member wakes on input delivery
        // and on lease loss, the endpoint member on a posted window
        // request, and the any-child member when a spawned app exits (so
        // its windows are torn down promptly). Every member is
        // owner-checked at add; the session never polls and never sleeps
        // through its own revocation.
        let set = tairix_rt::waitset_create();
        if set < 0 {
            return fail(EXIT_WAIT_FAILED, "wait-set refused");
        }
        #[allow(clippy::cast_sign_loss)] // `set >= 0` checked above; it is a kernel handle.
        let set = set as u64;
        if tairix_rt::waitset_ctl(
            set,
            WaitSetOp::Add,
            WaitSourceKind::SeatInput,
            SEAT_PRIMARY,
            SEAT_TOKEN,
        ) != 0
        {
            return fail(EXIT_WAIT_FAILED, "seat wait refused");
        }
        if tairix_rt::waitset_ctl(
            set,
            WaitSetOp::Add,
            WaitSourceKind::Endpoint,
            WINDOW_ENDPOINT,
            WINDOW_TOKEN,
        ) != 0
        {
            return fail(EXIT_WAIT_FAILED, "window endpoint wait refused");
        }
        if tairix_rt::waitset_ctl(
            set,
            WaitSetOp::Add,
            WaitSourceKind::Child,
            tairix_abi::WAITSET_CHILD_ANY,
            CHILD_TOKEN,
        ) != 0
        {
            return fail(EXIT_WAIT_FAILED, "child wait refused");
        }

        // The window channel's server state: the engine, the session-side
        // window table, the kernel-attested caller identity, the app-ward
        // event sink, and the focused served window the routing mirrors.
        // The engine stamps this session's own kernel-attested identity
        // into every create reply, so apps can authenticate the sender of
        // each later event; a session that cannot learn its own identity
        // must not serve windows apps cannot authenticate (fail closed).
        let Ok(self_origin) = tairix_rt::self_origin() else {
            return fail(EXIT_NO_WINDOW_ENDPOINT, "session identity unavailable");
        };
        let mut server = WindowServer::new(RtShmMapper, self_origin.proc_id());
        let mut windows = SessionWindows::new();
        let mut identity = RtWindowIdentity::new();
        let mut sink = RtEventSink;
        let mut focused: Option<u64> = None;
        // Every app in_flight from the desktop is admitted immediately and
        // loads on its own task (asynchronous launch); a load refusal now
        // surfaces as the child's reserved-`LOAD_*` exit status, not the
        // `spawn` return. This table remembers each in_flight child's
        // start-menu label so the `CHILD_TOKEN` reap below can name the app
        // in the fail-loud diagnosis. An entry is removed when its child is
        // reaped, so it never grows beyond the apps currently in flight.
        let mut in_flight: BTreeMap<u64, &'static str> = BTreeMap::new();
        // The trusted file picker (AW5/CU6): the one shared browser engine
        // over the session's own capability-checked listing call. Every
        // pick starts from a fresh listing under the session's authority;
        // the app never lists anything itself. The picker opens at the
        // logged-in user's home (`HOME`, exported by login) so the user
        // lands among their own files rather than at the storage-forest
        // root; an unset or malformed `HOME` parses to no components (the
        // root), and a home that cannot be listed when a pick begins falls
        // back to the root there (fail closed, never a guessed path).
        let picker_start = tairix_rt::env_var(b"HOME")
            .and_then(|home| core::str::from_utf8(home).ok())
            .and_then(|home| tairix_browse::vfs::components_from_absolute_path(home).ok())
            .unwrap_or_default();
        let mut picker = SessionPicker::new(|| {
            VfsDirectorySource::new(|path: &str| {
                tairix_rt::read_dir_all(path.as_bytes()).map_err(errno_from)
            })
        })
        .starting_at(picker_start);

        let mut token = 0u64;
        loop {
            if tairix_rt::waitset_wait(set, u64::MAX, &mut token) != 0 {
                // A dead wait-set would degrade the loop into a busy poll;
                // exit fail-loud instead and let the supervisor decide.
                return fail(EXIT_WAIT_FAILED, "seat wait failed");
            }
            // Dispatch on the woken member's token and handle only that
            // source: `call_recv` *blocks* when nothing is pending, so a
            // seat-input wake must never touch the window endpoint (and
            // vice versa). Readiness is a non-consuming peek, so a member
            // left pending re-reports on the very next wait — handling one
            // source per wake starves nothing.
            if token == WINDOW_TOKEN {
                // Serve the pending window request: the wait-set peeked a
                // queued call and only this task ever dequeues, so the
                // recv returns promptly. Every outcome — including a
                // malformed request — is a well-formed typed reply, so no
                // caller is ever left parked; a transient recv error drops
                // the wake and re-parks.
                let mut request = [0u8; WINDOW_MAX_REQUEST];
                let mut ticket = 0u64;
                if let Ok(len) = tairix_rt::call_recv(WINDOW_ENDPOINT, &mut request, &mut ticket) {
                    let mut reply = [0u8; WINDOW_REPLY_MAX];
                    let n = {
                        let mut bridge = ShellWindowHost {
                            shell: &mut shell,
                            compositor: &mut compositor,
                            windows: &mut windows,
                            picker: &mut picker,
                        };
                        server.serve(
                            &mut bridge,
                            &mut identity,
                            ticket,
                            &request[..len],
                            &mut reply,
                        )
                    };
                    let _ = tairix_rt::call_reply(WINDOW_ENDPOINT, ticket, &reply[..n]);
                }
            } else if token == CHILD_TOKEN {
                // Reap every exited child in one wake and act on each: a child
                // whose asynchronous load was refused exits with a reserved
                // `LOAD_*` status (the load ran on the child's own task, so
                // the refusal arrives here, not at `spawn`), which is reported
                // fail-loud on `stderr` named by its launcher label; and every
                // reaped child — refused or clean — has its windows torn down
                // (the kernel already reclaimed its port and shm). Draining
                // fully is safe and never busy-waits: the non-blocking `wait`
                // yields nothing once no zombie remains. The whole
                // reap/report/teardown flow is the shared, host-tested
                // `reap_launched`.
                reap_launched(
                    &mut in_flight,
                    || {
                        // Placeholder the kernel overwrites on a successful
                        // reap; only the pid and status are needed.
                        let mut status = WaitStatus::Exited(0);
                        let pid = tairix_rt::wait(WAIT_PID_ANY, &mut status, WaitFlags::NONBLOCK);
                        if pid > 0 {
                            #[allow(clippy::cast_sign_loss)] // guarded by `pid > 0`.
                            Some((pid as u64, status))
                        } else {
                            None
                        }
                    },
                    |line| {
                        let _ = tairix_rt::stderr(line.as_bytes());
                    },
                    |pid| {
                        if let Some(client) = identity.take_by_pid(pid) {
                            let mut bridge = ShellWindowHost {
                                shell: &mut shell,
                                compositor: &mut compositor,
                                windows: &mut windows,
                                picker: &mut picker,
                            };
                            server.client_exited(&mut bridge, client);
                            if focused.is_some_and(|id| server.owner_of(id).is_none()) {
                                focused = None;
                            }
                        }
                    },
                );
            } else if token == SEAT_TOKEN {
                // Drain both input channels through the shell, routing
                // every outcome onward (to the focused app window, or the
                // launcher spawn); the events already applied stay
                // applied, and a faulting drain ends the session. The
                // drains are genuinely non-blocking (`pointer_read` /
                // `keyboard_read` return 0 when empty).
                let outcomes = match shell.pump(&mut pointer, &mut compositor) {
                    Ok(outcomes) => outcomes,
                    Err(err) => return drain_fault(err),
                };
                for outcome in outcomes {
                    route_outcome(
                        outcome,
                        None,
                        &mut focused,
                        &mut shell,
                        &mut compositor,
                        &mut windows,
                        &mut server,
                        &mut sink,
                        &mut identity,
                        &mut picker,
                        &mut in_flight,
                    );
                }
                loop {
                    match keyboard.poll_record() {
                        Ok(None) => break,
                        Ok(Some((event, record))) => {
                            let outcome = shell.handle(event, &mut compositor);
                            route_outcome(
                                outcome,
                                Some(record),
                                &mut focused,
                                &mut shell,
                                &mut compositor,
                                &mut windows,
                                &mut server,
                                &mut sink,
                                &mut identity,
                                &mut picker,
                                &mut in_flight,
                            );
                        }
                        Err(err) => return drain_fault(err),
                    }
                }
            }
            // One present per wake: the compositor tracks the damage the
            // pumped events and served presents produced and the ring
            // copies only that region.
            if let Err(code) = present(&mut compositor, &mut display) {
                return code;
            }
        }
    }

    /// Route one shell outcome onward: mirror focus changes and pointer
    /// presses to the owning app over the window channel, hand the raw
    /// key record to the focused served window (or the showing picker),
    /// and spawn the launcher selection. Everything else is complete
    /// inside the shell.
    #[allow(clippy::too_many_arguments)] // The serve loop's whole mutable state, threaded explicitly.
    fn route_outcome<S: DirectorySource, F: FnMut() -> S>(
        outcome: tairix_desktop_session::ShellOutcome,
        key: Option<KeyInput>,
        focused: &mut Option<u64>,
        shell: &mut DesktopShell,
        compositor: &mut Compositor,
        windows: &mut SessionWindows,
        server: &mut WindowServer<RtShmMapper>,
        sink: &mut RtEventSink,
        identity: &RtWindowIdentity,
        picker: &mut SessionPicker<S, F>,
        in_flight: &mut alloc::collections::BTreeMap<u64, &'static str>,
    ) {
        use tairix_desktop_session::{SessionEvent, ShellOutcome};
        match outcome {
            ShellOutcome::WindowManager(response) => match response {
                InputResponse::Activated { window, local } => {
                    let target = windows.ipc_id(window);
                    // Mirror the focus change app-ward: the window that
                    // lost focus (if served) learns first, then the
                    // newly focused one.
                    if *focused != target {
                        if let Some(old) = focused.take() {
                            deliver(
                                server,
                                sink,
                                shell,
                                compositor,
                                windows,
                                picker,
                                &WindowEvent::Focus {
                                    window_id: old,
                                    focused: false,
                                },
                            );
                        }
                        if let Some(id) = target {
                            deliver(
                                server,
                                sink,
                                shell,
                                compositor,
                                windows,
                                picker,
                                &WindowEvent::Focus {
                                    window_id: id,
                                    focused: true,
                                },
                            );
                        }
                        *focused = target;
                    }
                    // The activating press itself, window-local. A
                    // negative coordinate cannot occur for an in-window
                    // press; refuse rather than wrap if it ever did.
                    if let (Some(id), Ok(x), Ok(y)) =
                        (target, u32::try_from(local.x), u32::try_from(local.y))
                    {
                        deliver(
                            server,
                            sink,
                            shell,
                            compositor,
                            windows,
                            picker,
                            &WindowEvent::Pointer {
                                window_id: id,
                                x,
                                y,
                                action: PointerAction::Pressed(
                                    tairix_abi::input::PointerButtonCode::Primary,
                                ),
                            },
                        );
                    }
                    // A press on the showing picker window navigates it:
                    // the row hit-test, descent, and choose rules are the
                    // shared engine's, and a concluded pick delegates (or
                    // cancels) below.
                    if picker.wm_id() == Some(window) {
                        if let Some(concluded) = picker.handle_click(local, shell, compositor) {
                            conclude_pick(
                                concluded, server, sink, shell, compositor, windows, identity,
                                picker,
                            );
                        }
                    }
                }
                InputResponse::SecondaryActivated { window, local } => {
                    // A right-click raises+focuses the window like a primary
                    // press, then delivers a secondary-button press so the
                    // client can open its context menu. Mirror the focus change
                    // app-ward first (the old window unfocuses, the new one
                    // focuses), then deliver the press. The trusted picker is a
                    // read-only browser with no context menu, so a right-click
                    // on it delivers focus only and opens nothing.
                    let target = windows.ipc_id(window);
                    if *focused != target {
                        if let Some(old) = focused.take() {
                            deliver(
                                server,
                                sink,
                                shell,
                                compositor,
                                windows,
                                picker,
                                &WindowEvent::Focus {
                                    window_id: old,
                                    focused: false,
                                },
                            );
                        }
                        if let Some(id) = target {
                            deliver(
                                server,
                                sink,
                                shell,
                                compositor,
                                windows,
                                picker,
                                &WindowEvent::Focus {
                                    window_id: id,
                                    focused: true,
                                },
                            );
                        }
                        *focused = target;
                    }
                    if let (Some(id), Ok(x), Ok(y)) =
                        (target, u32::try_from(local.x), u32::try_from(local.y))
                    {
                        deliver(
                            server,
                            sink,
                            shell,
                            compositor,
                            windows,
                            picker,
                            &WindowEvent::Pointer {
                                window_id: id,
                                x,
                                y,
                                action: PointerAction::Pressed(
                                    tairix_abi::input::PointerButtonCode::Secondary,
                                ),
                            },
                        );
                    }
                }
                InputResponse::DesktopPressed => {
                    if let Some(old) = focused.take() {
                        deliver(
                            server,
                            sink,
                            shell,
                            compositor,
                            windows,
                            picker,
                            &WindowEvent::Focus {
                                window_id: old,
                                focused: false,
                            },
                        );
                    }
                }
                InputResponse::Key { window, .. } => {
                    if picker.wm_id() == Some(window) {
                        // The focused picker consumes its own keys; a
                        // concluded pick delegates (or cancels) below and
                        // the key never reaches a served window.
                        if let Some(record) = key {
                            if let Some(concluded) = picker.handle_key(&record, shell, compositor) {
                                conclude_pick(
                                    concluded, server, sink, shell, compositor, windows, identity,
                                    picker,
                                );
                            }
                        }
                    } else if let (Some(id), Some(record)) = (windows.ipc_id(window), key) {
                        deliver(
                            server,
                            sink,
                            shell,
                            compositor,
                            windows,
                            picker,
                            &WindowEvent::Key {
                                window_id: id,
                                key: record,
                            },
                        );
                    }
                }
                // A wheel gesture over a window that owns its own content
                // scrolling (no window-manager root viewport): the ticks
                // belong to the application, so forward them to that window's
                // owner over the window channel. The picker window scrolls its
                // own list in-process, so a wheel over it is consumed by the
                // shell, not forwarded as an app event.
                InputResponse::AppScroll { window, dx, dy } => {
                    if picker.wm_id() != Some(window) {
                        if let Some(window_id) = windows.ipc_id(window) {
                            deliver(
                                server,
                                sink,
                                shell,
                                compositor,
                                windows,
                                picker,
                                &WindowEvent::Scrolled { window_id, dx, dy },
                            );
                        }
                    }
                }
                // A title-bar command control was activated: map it to the
                // window's lifecycle in the one shared place
                // (`window_control_event`) — Close/Minimize/PutToBack/
                // SizeToggle — and deliver the app-ward event it yields
                // (Close→CloseRequested, Minimize→Minimized, SizeToggle→
                // Resized; PutToBack is window-manager-local and yields none)
                // over the existing window path.
                InputResponse::WindowControl { window, control } => {
                    let work_area = shell.work_area(compositor);
                    if let Some(event) =
                        window_control_event(control, window, work_area, shell, compositor, windows)
                    {
                        deliver(server, sink, shell, compositor, windows, picker, &event);
                    }
                }
                // An interactive edge resize-grab settled: tell the owning app
                // its new client content size once, at the end of the drag, so
                // it re-lays-out and re-maps its frame region. The per-frame
                // `Resized` ticks during the drag are the window manager's own
                // live geometry and are not forwarded (the app is told once,
                // here).
                InputResponse::ResizeEnded { window } => {
                    if let (Some(window_id), Some(client)) = (
                        windows.ipc_id(window),
                        compositor.window_client_rect(window),
                    ) {
                        deliver(
                            server,
                            sink,
                            shell,
                            compositor,
                            windows,
                            picker,
                            &WindowEvent::Resized {
                                window_id,
                                width_px: client.width,
                                height_px: client.height,
                            },
                        );
                    }
                }
                // Window-manager-local outcomes the session does not forward
                // app-ward: a scrollbar press, a move-grab, and the per-frame
                // resize ticks (the app is told once, at `ResizeEnded`, above).
                InputResponse::Scrolled { .. }
                | InputResponse::FurniturePressed { .. }
                | InputResponse::Moved { .. }
                | InputResponse::MoveEnded { .. }
                | InputResponse::Resized { .. }
                | InputResponse::Ignored => {}
            },
            ShellOutcome::Session(SessionEvent::Forward(TaskbarResponse::MenuEntrySelected {
                action: MenuAction::Launch(launcher),
                ..
            })) if launcher == FILES_LAUNCHER => {
                // Spawn the file browser under the session's own identity
                // and ceiling; the child is admitted and returns its PID at
                // once (asynchronous launch), loading on its own task. A
                // synchronous refusal (stripped spawn capability, malformed
                // path) is reported here; a load refusal surfaces later as
                // the child's exit status, reported by the reap. Either way
                // a denied optional action never ends the session.
                record_launch(in_flight, spawn_app(FILES_RUN_PATH), FILES_LABEL);
            }
            ShellOutcome::Session(SessionEvent::Forward(TaskbarResponse::MenuEntrySelected {
                action: MenuAction::Launch(launcher),
                ..
            })) if launcher == TERMINAL_LAUNCHER => {
                // The terminal, exactly as the file browser above: admitted
                // immediately, loaded on its own task, refusal reported
                // (synchronously here or by the reap), desktop carries on.
                record_launch(in_flight, spawn_app(TERMINAL_RUN_PATH), TERMINAL_LABEL);
            }
            ShellOutcome::Session(SessionEvent::Forward(TaskbarResponse::MenuEntrySelected {
                action: MenuAction::Launch(launcher),
                ..
            })) if launcher == VIEWER_LAUNCHER => {
                // The file viewer, exactly as the apps above: admitted
                // immediately, loaded on its own task, refusal reported
                // (synchronously here or by the reap), desktop carries on.
                record_launch(in_flight, spawn_app(VIEWER_RUN_PATH), VIEWER_LABEL);
            }
            _ => {}
        }
    }

    /// Spawn a desktop app under the session's own identity and console,
    /// forwarding the **user's environment** to it (`HOME`, `LANG`, …). Plain
    /// [`tairix_rt::spawn`] hands a child an *empty* environment; the desktop is
    /// the logged-in user's session, so an app it launches must inherit the
    /// same environment login exported and the session itself runs under —
    /// exactly as a login shell's children do. The file manager reads `HOME` to
    /// locate the user's Trash (`plans/NEW-FILEMANAGER.md` FM10), and apps read
    /// `LANG` for help localisation; forwarding the whole environment keeps the
    /// session from having to know which variables an app cares about. The
    /// child still runs under the session's attested credential and console
    /// ([`CONSOLE_INHERIT`]/[`SPAWN_UID_INHERIT`]) — the environment is data and
    /// carries no authority (§4, §5.4).
    fn spawn_app(path: &[u8]) -> i64 {
        let count = tairix_rt::env_count();
        let mut env: alloc::vec::Vec<&[u8]> = alloc::vec::Vec::with_capacity(count as usize);
        for index in 0..count {
            if let Some(entry) = tairix_rt::env(index) {
                env.push(entry);
            }
        }
        tairix_rt::spawn_with(path, CONSOLE_INHERIT, SPAWN_UID_INHERIT, &[], &env)
    }

    /// Record a just-issued launch. Asynchronous launch admits the child and
    /// returns its PID (`ret > 0`) before the image is loaded, so a
    /// successful admit only *starts* the launch: remember the PID under its
    /// start-menu label so the `CHILD_TOKEN` reap can name the app if its
    /// load is later refused (via the child's reserved-`LOAD_*` exit status).
    /// A synchronous refusal (`ret < 0` — a stripped spawn capability or a
    /// malformed path, decided before any child exists) is reported fail-loud
    /// at once. Either way a denied optional launch never ends the session.
    fn record_launch(
        in_flight: &mut alloc::collections::BTreeMap<u64, &'static str>,
        ret: i64,
        label: &'static str,
    ) {
        if ret < 0 {
            let _ =
                tairix_rt::stderr(alloc::format!("desktop: {label} launch refused\n").as_bytes());
        } else {
            #[allow(clippy::cast_sign_loss)] // `ret >= 0` in this branch; it is a PID.
            in_flight.insert(ret as u64, label);
        }
    }

    /// Conclude a pick: delegate the chosen file one-shot to the
    /// requesting window's attested owner and deliver `FilePicked`, or
    /// deliver `PickCancelled` when the user dismissed — or when any step
    /// of the delegation refuses (a vanished owner, a refused open or
    /// grant): nothing was delegated, so the cancellation is the honest,
    /// fail-closed conclusion, stated on `stderr` for the operator.
    #[allow(clippy::too_many_arguments)] // The serve loop's whole mutable state, threaded explicitly.
    fn conclude_pick<S: DirectorySource, F: FnMut() -> S>(
        concluded: ConcludedPick,
        server: &mut WindowServer<RtShmMapper>,
        sink: &mut RtEventSink,
        shell: &mut DesktopShell,
        compositor: &mut Compositor,
        windows: &mut SessionWindows,
        identity: &RtWindowIdentity,
        picker: &mut SessionPicker<S, F>,
    ) {
        let window_id = concluded.for_window;
        let event = match concluded.conclusion {
            PickConclusion::Cancelled => WindowEvent::PickCancelled { window_id },
            PickConclusion::Chosen(path) => match delegate(&path, window_id, server, identity) {
                Some(handle) => WindowEvent::FilePicked { window_id, handle },
                None => {
                    let _ = tairix_rt::stderr(b"desktop: picker delegation refused\n");
                    WindowEvent::PickCancelled { window_id }
                }
            },
        };
        deliver(server, sink, shell, compositor, windows, picker, &event);
    }

    /// Open `path` read-only under the session's own authority and mint a
    /// one-shot delegation to the attested owner of window `window_id`,
    /// returning the `fd_redeem` handle. The session's descriptor is
    /// closed either way — the delegation record is self-contained — and
    /// every refusal answers `None` (fail closed, nothing delegated).
    fn delegate(
        path: &str,
        window_id: u64,
        server: &WindowServer<RtShmMapper>,
        identity: &RtWindowIdentity,
    ) -> Option<u64> {
        let owner = server.owner_of(window_id)?;
        let pid = identity.pid_of(owner)?;
        let fd = tairix_rt::fs_open(path.as_bytes(), OpenFlags::READ);
        let fd = u32::try_from(fd).ok()?;
        let handle = tairix_rt::fd_grant(fd, pid);
        let _ = tairix_rt::fs_close(fd);
        u64::try_from(handle).ok().filter(|&handle| handle != 0)
    }

    /// Deliver one app-ward event, tearing the owner's windows down when
    /// the kernel proves the owner is gone (its event port was reclaimed,
    /// so the send finds nothing).
    fn deliver<S: DirectorySource, F: FnMut() -> S>(
        server: &mut WindowServer<RtShmMapper>,
        sink: &mut RtEventSink,
        shell: &mut DesktopShell,
        compositor: &mut Compositor,
        windows: &mut SessionWindows,
        picker: &mut SessionPicker<S, F>,
        event: &WindowEvent,
    ) {
        let Some(owner) = server.owner_of(event.window_id()) else {
            return;
        };
        if let Err(Errno::NotFound) = server.deliver_event(sink, event) {
            // `owner_of` proved the window exists, so the `NotFound` is
            // the sink's: the owner's event port is gone — the kernel
            // reclaimed it at exit — and its windows go with it. Any
            // other refusal (a full mailbox) drops the event only.
            let mut bridge = ShellWindowHost {
                shell,
                compositor,
                windows,
                picker,
            };
            server.client_exited(&mut bridge, owner);
        }
    }

    /// Render the command's own short help (`NAME` + `SYNOPSIS` + compact
    /// `OPTIONS`) from its own bundle's `Help/` tree through the one shared
    /// engine; when no document can be served (a build without the
    /// bundle's documents) the usage banner stands in — the tool's own
    /// text, not fabricated help content — so `-h` never fails.
    fn short_help() -> i32 {
        let locale = tairix_rt::env_var(b"LANG").and_then(|raw| core::str::from_utf8(raw).ok());
        let bytes = own_short_help(&BundleHelp::new("desktop"), locale, "desktop")
            .unwrap_or_else(|| alloc::format!("{USAGE}\n").into_bytes());
        match Stdout.write_all(&bytes) {
            Ok(()) => 0,
            Err(_) => 1,
        }
    }

    /// Program entry point. `tairix-rt`'s `_start` calls it once the
    /// runtime is set up and routes its return value through the `exit`
    /// syscall.
    ///
    /// Exit codes: `0` for a served short help, `2` on a usage error, and
    /// otherwise the session's own codes — on success this never returns
    /// until the session ends: the loop runs until the seat is lost or a
    /// fault ends it.
    fn main() -> i32 {
        // The command surface first: a malformed (non-UTF-8) argument
        // vector is a usage error, reported rather than guessed at, and
        // the reserved short-help switches never touch the seat.
        let Some(arguments) = tairix_rt::args() else {
            write_stderr_line(USAGE);
            return 2;
        };
        match parse(&arguments) {
            Ok(Command::Run) => {}
            Ok(Command::Help) => return short_help(),
            Err(CliError::Usage) => {
                write_stderr_line(USAGE);
                return 2;
            }
        }

        // Acquire the boot seat's exclusive, revocable lease. The kernel
        // binds this task as the owner; a seat already held refuses with a
        // typed error rather than displacing its owner.
        if tairix_rt::display_acquire(SEAT_PRIMARY) < 1 {
            return fail(EXIT_NO_SEAT, "seat acquire refused");
        }
        let code = session();
        // Owner-checked release on every exit path: a lease already lost
        // refuses (typed, ignored) — heal, never widen.
        let _ = tairix_rt::display_release(SEAT_PRIMARY);
        code
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
