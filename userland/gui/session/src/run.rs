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
//! The session also binds the three seat-scoped rendezvous — the window
//! channel, the notification channel, and the Switchboard tray-summary
//! channel — and spawns the desktop's Switchboard monitor service as the
//! logged-in user at bring-up. The monitor's change-driven summaries feed
//! the taskbar capsule (each publish attested against the launch table);
//! the session's own delivery evidence (the `HangTracker` behind the event
//! sink) feeds the capsule's "not responding" count; and a monitor that
//! dies or was never there simply leaves the capsule calm
//! (`plans/NEW-TASKBAR.md` T9/T10).
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
    use tairix_abi::notify_ipc::{NotifyRequest, NOTIFY_ENDPOINT, NOTIFY_MAX_REQUEST};
    use tairix_abi::reply::{encode_status_reply, STATUS_REPLY_LEN};
    use tairix_abi::seat::SEAT_PRIMARY;
    use tairix_abi::switchboard_ipc::{
        command_endpoint_for, encode_publish_reply, CommandSection, SwitchboardCommand,
        SEAT_REPORT_OWNERS_MAX, SWITCHBOARD_ENDPOINT, SWITCHBOARD_MAX_REQUEST,
        SWITCHBOARD_PUBLISH_REPLY_LEN,
    };
    use tairix_abi::window_ipc::{PointerAction, WindowEvent, WINDOW_ENDPOINT, WINDOW_MAX_REQUEST};
    use tairix_abi::{
        DriverError, Errno, OpenFlags, Origin, ProcId, WaitFlags, WaitSetOp, WaitSourceKind,
        WaitStatus, CONSOLE_INHERIT, ORIGIN_WIRE_LEN, SPAWN_UID_INHERIT, WAIT_PID_ANY,
    };
    use tairix_browse::{DirectorySource, VfsDirectorySource};
    use tairix_caps::CapabilitySet;
    use tairix_desktop_session::{
        build_pin_views, command_section, deliver_pending_open, load_library,
        maybe_send_seat_report, open_tray, parse, reap_launched, relay_power,
        serve_switchboard_request, window_control_event, Answer, CliError, Command, ConcludedPick,
        ConfirmPrompt, DesktopShell, DeviceInputSource, HangTracker, IconCache, IconRasteriser,
        KeyboardInputSource, LaunchTable, OwnerWindow, PickConclusion, PinBridge, PinService,
        ResolvedPin, SeatEventReader, SeatInputChannel, SessionFileReader, SessionFileWriter,
        SessionPicker, SessionPins, SessionWindows, ShellWindowHost, SwitchboardMailbox,
        SwitchboardOutcome, SwitchboardServe, FILES_LABEL, FILES_RUN_PATH, SWITCHBOARD_LABEL,
        SWITCHBOARD_RUN_PATH, USAGE,
    };
    use tairix_display::{DisplayClient, DisplayTransport, RemoteDisplay, RtShmMapper};
    use tairix_help::{own_short_help, BundleHelp};
    use tairix_rt::io::{write_stderr_line, Stdout, Write};
    use tairix_sandbox::iconraster::{rasterise_icon, IconRasterService};
    use tairix_sandbox::rt::{serve_stdio, worker_role, RtLauncher};
    use tairix_sandbox::{ParserSandbox, ServeEnd};
    use tairix_taskbar::{TaskId, TaskbarConfig, TaskbarResponse};
    use tairix_window::{
        event_endpoint_for, CallerIdentity, EventSink, PinDecision, WindowServer, WINDOW_REPLY_MAX,
    };
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

    /// Exit code when the reserved `NOTIFY_ENDPOINT` could not be bound. It
    /// is another seat-scoped reserved id, authorised by the same live
    /// seat lease as the window endpoint, so a refusal here is the same
    /// lease/rendezvous anomaly — exit fail-loud rather than run a desktop
    /// whose services cannot post notifications.
    const EXIT_NO_NOTIFY_ENDPOINT: i32 = 99;

    /// Exit code when the reserved `SWITCHBOARD_ENDPOINT` could not be
    /// bound. The third seat-scoped reserved id, authorised by the same
    /// live seat lease — the same lease/rendezvous anomaly as the other
    /// two, so the session exits fail-loud rather than run a desktop whose
    /// monitor cannot publish.
    const EXIT_NO_SWITCHBOARD_ENDPOINT: i32 = 100;

    /// Exit code when the user chose *Log Out*. The session ended because it
    /// was asked to, so it is a success: nothing failed, and the login
    /// supervisor that started the desktop prompts again.
    const EXIT_LOGGED_OUT: i32 = 0;

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

    /// The wait-set token of the served `NOTIFY_ENDPOINT` member: a producer
    /// posting or clearing a notification wakes the loop to relay it.
    const NOTIFY_TOKEN: u64 = 4;

    /// The wait-set token of the served `SWITCHBOARD_ENDPOINT` member: the
    /// Switchboard service publishing a tray summary wakes the loop to
    /// relay it to the capsule.
    const SWITCHBOARD_TOKEN: u64 = 5;

    /// Outstanding-call capacity of the window endpoint (a fail-closed
    /// memory bound): every app calls synchronously, so a small queue
    /// covers several concurrent clients.
    const WINDOW_CAPACITY: usize = 8;

    /// Outstanding-call capacity of the notification endpoint: notifications
    /// are infrequent and synchronous, so a small queue covers several
    /// producers posting at once (a fail-closed memory bound).
    const NOTIFY_CAPACITY: usize = 8;

    /// Outstanding-call capacity of the Switchboard endpoint: exactly one
    /// attested publisher posts, change-driven and synchronous, so the
    /// queue stays tiny (a fail-closed memory bound).
    const SWITCHBOARD_CAPACITY: usize = 4;

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
    /// owning app's event port per event. To avoid flooding an app with
    /// samples it can only act on the newest of, the shell coalesces
    /// adjacent motions naming the same window; every remaining outcome is
    /// a non-blocking send. The send never parks this session (a full
    /// mailbox or a dead port is a typed refusal), so a wedged app can
    /// never wedge the desktop.
    ///
    /// Every send outcome doubles as responsiveness evidence: the wrapped
    /// [`HangTracker`] folds each `WouldBlock` back-pressure refusal and
    /// each accepted delivery into per-owner "not responding" verdicts
    /// (keyed by the event-mailbox endpoint, which embeds the owning task
    /// id), and the loop drains [`take_changed`](Self::take_changed) once
    /// per wake to bring the taskbar capsule in step. Time is stamped only
    /// on the delivery paths, so an idle desktop reads no clock.
    struct RtEventSink {
        vigil: HangTracker,
        changed: bool,
    }

    impl RtEventSink {
        /// A sink with no delivery evidence yet.
        const fn new() -> Self {
            Self {
                vigil: HangTracker::new(),
                changed: false,
            }
        }

        /// Whether the unresponsive set changed since the last drain,
        /// clearing the latch.
        fn take_changed(&mut self) -> bool {
            core::mem::take(&mut self.changed)
        }

        /// How many window owners are currently flagged unresponsive.
        fn unresponsive_count(&self) -> u16 {
            self.vigil.unresponsive_count()
        }

        /// The flagged owners' event-mailbox endpoints, walked without
        /// allocating so the seat report can name a bounded few of them.
        fn unresponsive_endpoints(&self) -> impl Iterator<Item = u64> + '_ {
            self.vigil.unresponsive_owners()
        }

        /// Drop every verdict held against a reaped child's event mailbox
        /// — a dead app is not a hung app, and a recycled task id must
        /// start clean.
        fn forget_owner(&mut self, pid: u64) {
            self.changed |= self.vigil.forget(event_endpoint_for(pid));
        }
    }

    impl EventSink for RtEventSink {
        fn deliver(
            &mut self,
            endpoint: u64,
            event: &[u8; WindowEvent::WIRE_LEN],
        ) -> Result<(), Errno> {
            let ret = tairix_rt::ipc_send(endpoint, event);
            if ret == 0 {
                self.changed |= self.vigil.note_delivered(endpoint);
                Ok(())
            } else {
                let error = errno_from(ret);
                self.changed |= self
                    .vigil
                    .note_refused(endpoint, error, tairix_rt::clock_get());
                Err(error)
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

    /// Attest the producer of a pending notification call, decode the request
    /// fail-closed, and relay it to the taskbar model, returning the status
    /// the producer receives.
    ///
    /// The producer's identity is the kernel-attested `call_peer_origin` on
    /// the notification endpoint, never a wire claim, so a notification is
    /// always keyed to the service that actually posted it. An unattestable
    /// caller or a malformed request is a typed refusal (fail closed) and
    /// never mutates the model.
    fn serve_notify(
        shell: &mut DesktopShell,
        compositor: &mut Compositor,
        ticket: u64,
        request: &[u8],
    ) -> Result<(), Errno> {
        let mut buf = [0u8; ORIGIN_WIRE_LEN];
        let len =
            tairix_rt::call_peer_origin(NOTIFY_ENDPOINT, ticket, &mut buf).map_err(errno_from)?;
        let origin = Origin::from_bytes(&buf[..len])?;
        let request = NotifyRequest::from_bytes(request)?;
        shell.apply_notify(compositor, origin.pid(), request);
        Ok(())
    }

    /// Attest the caller of a pending Switchboard call from the kernel and
    /// serve it through the shared, host-tested policy, returning what the
    /// caller is answered with.
    ///
    /// Only the Switchboard child this session spawned may call: the
    /// caller's kernel-attested `call_peer_origin` pid must match the
    /// launch table's live entry for the service's bundle path. Anything
    /// else — a foreign process, an orphan of an earlier session, a copy
    /// launched by hand — is a typed refusal, stated on `stderr`, and
    /// never mutates the model (fail closed). A malformed frame, and an
    /// owner-directed operation naming an owner this session cannot act
    /// on, refuse the same way.
    fn serve_switchboard(
        serve: SwitchboardServe<'_>,
        ticket: u64,
        request: &[u8],
    ) -> Result<SwitchboardOutcome, Errno> {
        let mut buf = [0u8; ORIGIN_WIRE_LEN];
        let len = tairix_rt::call_peer_origin(SWITCHBOARD_ENDPOINT, ticket, &mut buf)
            .map_err(errno_from)?;
        let origin = Origin::from_bytes(&buf[..len])?;
        serve_switchboard_request(serve, origin.pid(), request).map_err(|refusal| {
            let _ = tairix_rt::stderr(alloc::format!("desktop: {}\n", refusal.reason()).as_bytes());
            refusal.errno()
        })
    }

    /// The live window ownership an `ActivateOwner` is validated against:
    /// the window engine's own attested owner records, resolved through the
    /// one `window_of_pid` every other owner lookup in this session uses.
    struct SessionOwnerWindows<'a> {
        server: &'a WindowServer<RtShmMapper>,
        windows: &'a SessionWindows,
        identity: &'a RtWindowIdentity,
    }

    impl OwnerWindow for SessionOwnerWindows<'_> {
        fn window_of(&self, owner: u64) -> Option<tairix_wm::WindowId> {
            window_of_pid(owner, self.server, self.windows, self.identity)
        }
    }

    /// The production [`SwitchboardMailbox`]: one non-blocking `ipc_send`
    /// to the live monitor's own per-instance command mailbox.
    ///
    /// The send never parks the desktop. A refusal — `WouldBlock`
    /// back-pressure because the monitor has not drained its mailbox, or
    /// `NotFound` because the instance exited — is stated on `stderr` and
    /// the command dropped: the panel missing an advisory open or seat
    /// report is not worth stalling the session for, and a retry loop here
    /// would be the busy-wait the desktop must never run.
    struct RtSwitchboardMailbox;

    impl SwitchboardMailbox for RtSwitchboardMailbox {
        fn send(&mut self, pid: u64, command: SwitchboardCommand) {
            if tairix_rt::ipc_send(command_endpoint_for(pid), &command.to_le_bytes()) != 0 {
                let _ = tairix_rt::stderr(b"desktop: switchboard command dropped: mailbox full\n");
            }
        }
    }

    /// Start the desktop's Switchboard monitor as this logged-in user and
    /// record it in the launch table like any other desktop child,
    /// answering with the pid of the instance now live.
    ///
    /// The kernel intersects the monitor's manifest with the user's
    /// ceiling, so its view follows the seat user's authority. A refused
    /// spawn answers `None` and leaves the capsule calm: the desktop runs
    /// without its monitor rather than failing over it.
    fn spawn_switchboard(launched: &mut LaunchTable) -> Option<u64> {
        record_launch(
            launched,
            spawn_app(SWITCHBOARD_RUN_PATH.as_bytes()),
            SWITCHBOARD_LABEL,
            SWITCHBOARD_RUN_PATH,
        );
        launched.running_from(SWITCHBOARD_RUN_PATH)
    }

    /// Name up to [`SEAT_REPORT_OWNERS_MAX`] of the currently-unresponsive
    /// window owners into `owners`, answering how many were named.
    ///
    /// The tracker keys its verdicts by each owner's event-mailbox
    /// endpoint, so an owner is named by matching that endpoint forward
    /// against `event_endpoint_for` of every live window owner — the same
    /// attested ownership records the rest of the session resolves owners
    /// through, never a claimed id or an inverse guessed from the endpoint
    /// number. A flagged owner whose windows have since gone simply goes
    /// unnamed; the report's total still counts it, so the monitor is told
    /// the truth either way.
    fn seat_report_owners(
        sink: &RtEventSink,
        server: &WindowServer<RtShmMapper>,
        windows: &SessionWindows,
        identity: &RtWindowIdentity,
        owners: &mut [u64; SEAT_REPORT_OWNERS_MAX],
    ) -> usize {
        let mut named = 0;
        for endpoint in sink.unresponsive_endpoints() {
            if named == owners.len() {
                break;
            }
            let owner = windows.served().find_map(|(ipc, _)| {
                let client = server.owner_of(ipc)?;
                let pid = identity.pid_of(client)?;
                (event_endpoint_for(pid) == endpoint).then_some(pid)
            });
            if let Some(pid) = owner {
                owners[named] = pid;
                named += 1;
            }
        }
        named
    }

    /// Bring the desktop up and run it until the seat is lost or a fault
    /// ends it. Split from `main` so every exit path after the acquire
    /// flows back through the one owner-checked `display_release`.
    #[allow(clippy::too_many_lines)] // One linear bring-up + serve loop; splitting it would scatter the lease lifecycle.
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
        let mut shell = DesktopShell::new(TaskbarConfig::bottom_bar(mode.width_px, mode.height_px));
        let Some(mut compositor) = Compositor::new(mode, shell.desktop_background()) else {
            return fail(EXIT_BAD_MODE, "compositor rejected the queried mode");
        };
        let screen = Rect::new(0, 0, mode.width_px, mode.height_px);
        let Ok(mut pointer) = DeviceInputSource::new(SeatInputChannel::new(PointerReader), screen)
        else {
            return fail(EXIT_BAD_MODE, "queried mode has no pointer surface");
        };
        let mut keyboard = KeyboardInputSource::new(SeatInputChannel::new(KeyboardReader));

        // The program library: read the machine store and the logged-in
        // user's overlay under the session's own identity, merge them, and
        // hand the resolved catalog to the taskbar's popup. A store that
        // cannot be used is reported loudly and contributes an empty
        // catalog, so the desktop comes up with a calm empty library rather
        // than dying over a settings file.
        refresh_library(&mut shell, &mut compositor);

        // The user's taskbar pins: load the per-user store with the same
        // fail-closed posture (absent → empty; unusable → empty plus a
        // loud reason), then resolve each pin against the catalog just
        // loaded. Edits arriving later (a context-menu unpin, a pin request
        // or drop from an app) mark the service dirty and the loop
        // re-resolves before its next present.
        let mut pins = load_pin_service();

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

        // Bind the notification rendezvous the same way: the same live seat
        // lease authorises it (it is the other seat-scoped reserved id), and
        // it is unrestricted-sender — a producer's identity is attested per
        // request and each notification keyed to it, so an unentitled sender
        // only ever reaches a typed refusal. A refusal here is the same
        // lease/rendezvous anomaly the window bind would hit; fail loud.
        if tairix_rt::call_create(
            NOTIFY_ENDPOINT,
            &empty,
            &empty,
            NOTIFY_MAX_REQUEST,
            STATUS_REPLY_LEN,
            NOTIFY_CAPACITY,
        ) != 0
        {
            return fail(
                EXIT_NO_NOTIFY_ENDPOINT,
                "notification endpoint bind refused",
            );
        }

        // Bind the Switchboard rendezvous the same way: the third
        // seat-scoped reserved id, authorised by the same live seat lease,
        // unrestricted-sender — the serve arm attests the one legitimate
        // publisher (the Switchboard child this session spawns) per
        // request and refuses everyone else, so an unentitled sender only
        // ever reaches a typed refusal.
        if tairix_rt::call_create(
            SWITCHBOARD_ENDPOINT,
            &empty,
            &empty,
            SWITCHBOARD_MAX_REQUEST,
            SWITCHBOARD_PUBLISH_REPLY_LEN,
            SWITCHBOARD_CAPACITY,
        ) != 0
        {
            return fail(
                EXIT_NO_SWITCHBOARD_ENDPOINT,
                "switchboard endpoint bind refused",
            );
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
            WaitSourceKind::Endpoint,
            NOTIFY_ENDPOINT,
            NOTIFY_TOKEN,
        ) != 0
        {
            return fail(EXIT_WAIT_FAILED, "notification endpoint wait refused");
        }
        if tairix_rt::waitset_ctl(
            set,
            WaitSetOp::Add,
            WaitSourceKind::Endpoint,
            SWITCHBOARD_ENDPOINT,
            SWITCHBOARD_TOKEN,
        ) != 0
        {
            return fail(EXIT_WAIT_FAILED, "switchboard endpoint wait refused");
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
        let mut sink = RtEventSink::new();
        let mut focused: Option<u64> = None;
        // Every app launched from the desktop is admitted immediately and
        // loads on its own task (asynchronous launch); a load refusal now
        // surfaces as the child's reserved-`LOAD_*` exit status, not the
        // `spawn` return. This table remembers each running child's label
        // (so the `CHILD_TOKEN` reap below can name the app in the
        // fail-loud diagnosis) and its spawn path (the attested bundle
        // identity the Files button's idempotent open resolves against). An
        // entry is removed when its child is reaped, so it never grows
        // beyond the apps currently alive.
        let mut launched = LaunchTable::new();
        // Start the desktop's Switchboard monitor. It is recorded in the
        // launch table like any desktop child — the reap arm names a load
        // refusal, and the serve arm attests its calls against this entry
        // — and the very same bring-up serves a later tray press that
        // finds no instance live.
        let mut switchboard_pid = spawn_switchboard(&mut launched);
        // A tray press with no live monitor to receive it: the section the
        // bar asked to open on, held until that instance's first publish
        // proves it is up. One pending open, replaced by a later press
        // rather than queued, so a user pressing repeatedly opens the
        // section they last asked for and no more.
        let mut pending_open: Option<CommandSection> = None;
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
        // The trusted confirmation prompt for a power transition. It is the
        // session's own window, so the question the user answers is asked by
        // the desktop itself rather than by the bar, which holds no
        // authority; an unanswered prompt relays nothing.
        let mut confirm = ConfirmPrompt::new();

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
                            pins: &mut pins.service,
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
            } else if token == NOTIFY_TOKEN {
                // Serve a pending notification request: attest the producer
                // from the kernel (never the wire), decode fail-closed, relay
                // the raise/clear to the taskbar model, and answer with the
                // shared status reply. A malformed request or an unattestable
                // caller is a typed refusal, so no producer is left parked.
                let mut request = [0u8; NOTIFY_MAX_REQUEST];
                let mut ticket = 0u64;
                if let Ok(len) = tairix_rt::call_recv(NOTIFY_ENDPOINT, &mut request, &mut ticket) {
                    let result = serve_notify(&mut shell, &mut compositor, ticket, &request[..len]);
                    let reply = encode_status_reply(result);
                    let _ = tairix_rt::call_reply(NOTIFY_ENDPOINT, ticket, &reply);
                }
            } else if token == SWITCHBOARD_TOKEN {
                // Serve a pending monitor call: attest that the caller is
                // the Switchboard child this session spawned (never the
                // wire), decode fail-closed, apply it, and answer. A
                // foreign caller, a malformed frame, or an owner this
                // session cannot act on is a typed refusal, so no caller
                // is left parked.
                let mut request = [0u8; SWITCHBOARD_MAX_REQUEST];
                let mut ticket = 0u64;
                if let Ok(len) =
                    tairix_rt::call_recv(SWITCHBOARD_ENDPOINT, &mut request, &mut ticket)
                {
                    let result = serve_switchboard(
                        SwitchboardServe {
                            shell: &mut shell,
                            compositor: &mut compositor,
                            launched: &mut launched,
                            owner_windows: &SessionOwnerWindows {
                                server: &server,
                                windows: &windows,
                                identity: &identity,
                            },
                            relaunch:
                                &mut |launched: &mut LaunchTable, run_path: &str, label: &str| {
                                    record_launch(
                                        launched,
                                        spawn_app(run_path.as_bytes()),
                                        label,
                                        run_path,
                                    );
                                },
                            self_proc_id: self_origin.proc_id(),
                        },
                        ticket,
                        &request[..len],
                    );
                    // A publish is the proof an instance is up and
                    // draining: a press that arrived before it was has an
                    // instance to open on now.
                    if matches!(result, Ok(SwitchboardOutcome::Published(_))) {
                        switchboard_pid = launched.running_from(SWITCHBOARD_RUN_PATH);
                        if let Some(pid) = switchboard_pid {
                            deliver_pending_open(&mut pending_open, pid, &mut RtSwitchboardMailbox);
                        }
                    }
                    // A successful publish answers with this session's own
                    // kernel-attested identity, so the monitor can
                    // authenticate the commands the session later sends
                    // it; every other outcome, refusals included, answers
                    // with the shared status frame.
                    let mut reply = [0u8; SWITCHBOARD_PUBLISH_REPLY_LEN];
                    let len = if let Ok(SwitchboardOutcome::Published(session)) = result {
                        let frame = encode_publish_reply(session);
                        reply[..frame.len()].copy_from_slice(&frame);
                        frame.len()
                    } else {
                        let frame = encode_status_reply(result.map(|_| ()));
                        reply[..frame.len()].copy_from_slice(&frame);
                        frame.len()
                    };
                    let _ = tairix_rt::call_reply(SWITCHBOARD_ENDPOINT, ticket, &reply[..len]);
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
                    &mut launched,
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
                                pins: &mut pins.service,
                            };
                            server.client_exited(&mut bridge, client);
                            if focused.is_some_and(|id| server.owner_of(id).is_none()) {
                                focused = None;
                            }
                        }
                        // A launched app that raised notifications and then
                        // exited can no longer clear them; drop them here so a
                        // dead producer leaves no stuck notification — the
                        // notification counterpart of the window teardown
                        // above, run for every reaped child, windowed or not.
                        shell.clear_producer_notifications(&mut compositor, pid);
                        // A reaped child is gone, not hung: drop its delivery
                        // evidence so a recycled task id starts clean. And a
                        // reaped Switchboard can publish nothing more — clear
                        // the tray feed so the capsule falls back to calm
                        // rather than freezing a dead service's last summary.
                        sink.forget_owner(pid);
                        if switchboard_pid == Some(pid) {
                            switchboard_pid = None;
                            shell.set_tray_summary(&mut compositor, None);
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
                // One wake is one instant: the whole drained batch resolves
                // its time-driven gestures (a held capsule press) against
                // the clock read here, and an idle desktop reads none.
                let now_ns = tairix_rt::clock_get();
                let outcomes = match shell.pump(&mut pointer, &mut compositor, now_ns) {
                    Ok(outcomes) => outcomes,
                    Err(err) => return drain_fault(err),
                };
                for outcome in outcomes {
                    if route_outcome(
                        outcome,
                        None,
                        &mut focused,
                        &mut shell,
                        &mut compositor,
                        &mut windows,
                        &mut server,
                        &mut sink,
                        &identity,
                        &mut picker,
                        &mut confirm,
                        &mut launched,
                        &mut pins,
                        &mut switchboard_pid,
                        &mut pending_open,
                    ) == Routed::EndSession
                    {
                        return EXIT_LOGGED_OUT;
                    }
                }
                loop {
                    match keyboard.poll_record() {
                        Ok(None) => break,
                        Ok(Some((event, record))) => {
                            let outcome = shell.handle(event, &mut compositor, now_ns);
                            if route_outcome(
                                outcome,
                                Some(record),
                                &mut focused,
                                &mut shell,
                                &mut compositor,
                                &mut windows,
                                &mut server,
                                &mut sink,
                                &identity,
                                &mut picker,
                                &mut confirm,
                                &mut launched,
                                &mut pins,
                                &mut switchboard_pid,
                                &mut pending_open,
                            ) == Routed::EndSession
                            {
                                return EXIT_LOGGED_OUT;
                            }
                        }
                        Err(err) => return drain_fault(err),
                    }
                }
            }
            // Fold this wake's delivery evidence into the capsule and the
            // monitor's seat view exactly once: the sink latched whether
            // any window owner crossed into or out of the unresponsive set
            // while events were delivered, so both move on a real change
            // and neither is recomputed on a quiet wake.
            let vigil_changed = sink.take_changed();
            let mut unresponsive = 0;
            let mut owners = [0u64; SEAT_REPORT_OWNERS_MAX];
            let mut named = 0;
            if vigil_changed {
                unresponsive = sink.unresponsive_count();
                shell.set_tray_unresponsive(&mut compositor, unresponsive);
                named = seat_report_owners(&sink, &server, &windows, &identity, &mut owners);
            }
            maybe_send_seat_report(
                vigil_changed,
                switchboard_pid,
                unresponsive,
                &owners[..named],
                &mut RtSwitchboardMailbox,
            );
            // Bring the pin strip up to date before presenting: an edit
            // (pin, unpin, an accepted drop or app request) re-resolves the
            // store; otherwise only the cheap running-window matches are
            // recomputed, and the strip is re-pushed exactly when a match
            // changed (a pinned app launched, gained its window, or exited).
            if pins.service.take_dirty() {
                refresh_pins(
                    &mut pins,
                    &mut shell,
                    &mut compositor,
                    &server,
                    &windows,
                    &identity,
                    &launched,
                );
            } else {
                sync_pin_windows(
                    &mut pins,
                    &mut shell,
                    &mut compositor,
                    &server,
                    &windows,
                    &identity,
                    &launched,
                );
            }
            // One present per wake: the compositor tracks the damage the
            // pumped events and served presents produced and the ring
            // copies only that region.
            if let Err(code) = present(&mut compositor, &mut display) {
                return code;
            }
        }
    }

    /// The session's pin state: the store-owning service plus the resolved
    /// pins, their live running-window matches, and the sandboxed icon
    /// pipeline (rasteriser + artwork cache), kept beside the loop so a
    /// press resolves against exactly what the strip shows.
    struct PinPanel {
        service: PinService<VfsFileReader, VfsFileWriter>,
        resolved: alloc::vec::Vec<ResolvedPin>,
        matches: alloc::vec::Vec<Option<TaskId>>,
        rasteriser: SandboxRasteriser,
        icons: IconCache,
    }

    /// The production [`IconRasteriser`]: untrusted icon bytes go to the
    /// parser-sandbox icon service — this binary re-entered as a
    /// capability-empty worker — and only a verified pixel block comes
    /// back. Any refusal (malformed image, crashed worker, unavailable
    /// spawn) is `None`: the pin falls back to its class glyph.
    struct SandboxRasteriser {
        sandbox: ParserSandbox<RtLauncher, tairix_rt::LogSink>,
    }

    impl IconRasteriser for SandboxRasteriser {
        fn rasterise(&mut self, side: u32, icon: &[u8]) -> Option<alloc::vec::Vec<u8>> {
            rasterise_icon(&mut self.sandbox, side, icon).ok()
        }
    }

    /// Load the user's pin store (reporting an unusable one loudly) into a
    /// service over the production file seams, alongside the sandboxed
    /// icon pipeline.
    fn load_pin_service() -> PinPanel {
        let home = tairix_rt::env_var(b"HOME").and_then(|raw| core::str::from_utf8(raw).ok());
        let (store, warning) = SessionPins::load(&mut VfsFileReader, home);
        if let Some(warning) = warning {
            let _ = tairix_rt::stderr(warning.as_bytes());
        }
        PinPanel {
            service: PinService::new(VfsFileReader, VfsFileWriter, store),
            resolved: alloc::vec::Vec::new(),
            matches: alloc::vec::Vec::new(),
            rasteriser: SandboxRasteriser {
                sandbox: ParserSandbox::new(RtLauncher::own_binary(), tairix_rt::LogSink),
            },
            icons: IconCache::new(),
        }
    }

    /// Re-resolve every pin (the store or the catalog changed), recompute
    /// the running-window matches, and push fresh views to the strip.
    fn refresh_pins(
        pins: &mut PinPanel,
        shell: &mut DesktopShell,
        compositor: &mut Compositor,
        server: &WindowServer<RtShmMapper>,
        windows: &SessionWindows,
        identity: &RtWindowIdentity,
        launched: &LaunchTable,
    ) {
        pins.resolved = {
            let catalog = shell.session().taskbar().library().catalog();
            pins.service.resolve(catalog)
        };
        pins.matches = pin_matches(&pins.resolved, shell, server, windows, identity, launched);
        push_pin_views(pins, shell, compositor);
    }

    /// Recompute only the pins' running-window matches, re-pushing the
    /// strip's views exactly when a match changed. Pure in-memory
    /// bookkeeping — no store or manifest is re-read — so it is cheap
    /// enough to run once per wake.
    fn sync_pin_windows(
        pins: &mut PinPanel,
        shell: &mut DesktopShell,
        compositor: &mut Compositor,
        server: &WindowServer<RtShmMapper>,
        windows: &SessionWindows,
        identity: &RtWindowIdentity,
        launched: &LaunchTable,
    ) {
        let matches = pin_matches(&pins.resolved, shell, server, windows, identity, launched);
        if matches != pins.matches {
            pins.matches = matches;
            push_pin_views(pins, shell, compositor);
        }
    }

    /// The running desktop task behind each resolved pin: its bundle's
    /// desktop-launched process, when that process has a served window the
    /// bar tracks as a task. Matching is by the launch table's attested
    /// spawn path and the window engine's attested ownership — never a
    /// window title or any other app-controlled data.
    fn pin_matches(
        resolved: &[ResolvedPin],
        shell: &DesktopShell,
        server: &WindowServer<RtShmMapper>,
        windows: &SessionWindows,
        identity: &RtWindowIdentity,
        launched: &LaunchTable,
    ) -> alloc::vec::Vec<Option<TaskId>> {
        resolved
            .iter()
            .map(|pin| {
                let run_path = pin.run_path.as_deref()?;
                let pid = launched.running_from(run_path)?;
                let wm = window_of_pid(pid, server, windows, identity)?;
                shell.tasks().task_for(wm)
            })
            .collect()
    }

    /// Push the resolved pins (with their live matches and artwork) into
    /// the taskbar's strip and re-present. Artwork is rasterised at the
    /// strip's own icon geometry through the sandboxed pipeline, served
    /// from the cache on every later push.
    fn push_pin_views(pins: &mut PinPanel, shell: &mut DesktopShell, compositor: &mut Compositor) {
        let side = shell.session().taskbar().pin_icon_side(compositor.scale());
        let views = build_pin_views(
            &pins.resolved,
            &pins.matches,
            &mut VfsFileReader,
            &mut pins.rasteriser,
            &mut pins.icons,
            side,
        );
        shell.set_pins(compositor, views);
    }

    /// Whether the serve loop carries on after an outcome was routed.
    #[derive(Copy, Clone, Debug, Eq, PartialEq)]
    enum Routed {
        /// Keep serving.
        Continue,
        /// The user asked to log out. The loop unwinds so the one
        /// owner-checked release runs and the login supervisor prompts again.
        EndSession,
    }

    /// Route one shell outcome onward: mirror focus changes and pointer
    /// presses to the owning app over the window channel, hand the raw
    /// key record to the focused served window (or the showing picker,
    /// or the showing confirmation prompt), and spawn the launcher
    /// selection. Everything else is complete inside the shell.
    #[allow(clippy::too_many_arguments)] // The serve loop's whole mutable state, threaded explicitly.
    #[allow(clippy::too_many_lines)] // One linear match over every outcome; splitting it would hide the routing policy.
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
        confirm: &mut ConfirmPrompt,
        launched: &mut LaunchTable,
        pins: &mut PinPanel,
        switchboard: &mut Option<u64>,
        pending_open: &mut Option<CommandSection>,
    ) -> Routed {
        use tairix_desktop_session::ShellOutcome;
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
                                &mut pins.service,
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
                                &mut pins.service,
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
                            &mut pins.service,
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
                                picker, pins,
                            );
                        }
                    }
                    // A press on the showing confirmation prompt answers it:
                    // only the confirming button relays the transition, and
                    // the prompt window is already gone by then.
                    if confirm.wm_id() == Some(window) {
                        if let Some(answer) = confirm.handle_click(local, shell, compositor) {
                            report_power_relay(answer, *switchboard);
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
                                &mut pins.service,
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
                                &mut pins.service,
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
                            &mut pins.service,
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
                            &mut pins.service,
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
                                    picker, pins,
                                );
                            }
                        }
                    } else if confirm.wm_id() == Some(window) {
                        // The focused prompt consumes its own keys the same
                        // way, so `Escape` declines and no key reaches an app
                        // while the question is unanswered.
                        if let Some(record) = key {
                            if let Some(answer) = confirm.handle_key(&record, shell, compositor) {
                                report_power_relay(answer, *switchboard);
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
                            &mut pins.service,
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
                                &mut pins.service,
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
                        deliver(
                            server,
                            sink,
                            shell,
                            compositor,
                            windows,
                            picker,
                            &mut pins.service,
                            &event,
                        );
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
                            &mut pins.service,
                            &WindowEvent::Resized {
                                window_id,
                                width_px: client.width,
                                height_px: client.height,
                            },
                        );
                    }
                }
                // A client-area pointer motion the window manager consumed no
                // furniture for: forward it to the owning app as a window-local
                // move so its in-content controls track hover and thumb drags.
                // A negative coordinate cannot occur (the router clamps into the
                // client); refuse rather than wrap if it ever did.
                InputResponse::ClientPointerMoved { window, local } => {
                    if let (Some(id), Ok(x), Ok(y)) = (
                        windows.ipc_id(window),
                        u32::try_from(local.x),
                        u32::try_from(local.y),
                    ) {
                        deliver(
                            server,
                            sink,
                            shell,
                            compositor,
                            windows,
                            picker,
                            &mut pins.service,
                            &WindowEvent::Pointer {
                                window_id: id,
                                x,
                                y,
                                action: PointerAction::Moved,
                            },
                        );
                    }
                }
                // A primary release that ended a client pointer grab: forward it
                // so an in-content click or drag completes (a tab or combo
                // selection, a released scrollbar thumb). If the releasing
                // window has an app-reference drag armed, the release is also
                // the drop: landing on the bar's pin band pins the offered
                // bundle at the drop index. The release still reaches the app
                // either way, so its own gesture state always unwinds.
                InputResponse::ClientPointerReleased { window, local } => {
                    resolve_drop(window, pins, shell, compositor, windows);
                    if let (Some(id), Ok(x), Ok(y)) = (
                        windows.ipc_id(window),
                        u32::try_from(local.x),
                        u32::try_from(local.y),
                    ) {
                        deliver(
                            server,
                            sink,
                            shell,
                            compositor,
                            windows,
                            picker,
                            &mut pins.service,
                            &WindowEvent::Pointer {
                                window_id: id,
                                x,
                                y,
                                action: PointerAction::Released(
                                    tairix_abi::input::PointerButtonCode::Primary,
                                ),
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
            ShellOutcome::Taskbar(TaskbarResponse::OpenFiles) => {
                // The permanent Files button is idempotent: raise the
                // desktop-launched file manager's window when one is up,
                // let an in-flight launch finish undisturbed, and only
                // spawn when no desktop-launched copy is running.
                activate_bundle(
                    shell,
                    compositor,
                    server,
                    windows,
                    identity,
                    launched,
                    FILES_RUN_PATH,
                    FILES_LABEL,
                );
            }
            ShellOutcome::Taskbar(TaskbarResponse::LibraryLaunch { entry }) => {
                // Resolve the chosen entry's bundle through the catalog the
                // popup was handed and spawn its `Run` binary: admitted
                // immediately, loaded on its own task, refusal reported
                // (synchronously here or by the reap), desktop carries on.
                launch_library_entry(shell, &entry, launched);
            }
            ShellOutcome::Taskbar(TaskbarResponse::OpenLibrary) => {
                // Re-read the stores each time the popup opens, so an edit
                // made through `applib` (or a fresh install) shows without
                // restarting the session. Two small documents: cheap, and
                // always current.
                refresh_library(shell, compositor);
            }
            ShellOutcome::Taskbar(TaskbarResponse::ActivatePin { index }) => {
                // A pinned application with no live window: launch it (or
                // raise it, if a desktop-launched copy exists that the
                // strip's view had not caught up with yet), exactly like
                // the Files button's idempotent open.
                activate_pin(
                    index, pins, shell, compositor, server, windows, identity, launched,
                );
            }
            ShellOutcome::Taskbar(TaskbarResponse::Unpin { index }) => {
                // Remove the pin and persist; a refused edit changes
                // nothing and says why. The dirty latch re-resolves the
                // strip before the next present.
                if let Err(err) = pins.service.unpin(index) {
                    let _ = tairix_rt::stderr(
                        alloc::format!("desktop: unpin refused: {err}\n").as_bytes(),
                    );
                }
            }
            ShellOutcome::Taskbar(TaskbarResponse::PinEntry { entry }) => {
                // Pin a program-library entry from its context menu and
                // persist; a refused edit changes nothing and says why.
                if let Err(err) = pins.service.pin_entry(entry) {
                    let _ = tairix_rt::stderr(
                        alloc::format!("desktop: pin refused: {err}\n").as_bytes(),
                    );
                }
            }
            ShellOutcome::Taskbar(TaskbarResponse::OpenSwitchboard { section }) => {
                // The bar already decided which section the gesture asks
                // for (a quick press its overview, a hold its recovery
                // list); the session only relays it to the live monitor.
                // With none live the press is itself the demand for one:
                // bring an instance up and hold the section until its
                // first publish proves it is listening.
                if let Some(revived) = open_tray(
                    pending_open,
                    command_section(section),
                    *switchboard,
                    &mut RtSwitchboardMailbox,
                    || spawn_switchboard(launched),
                ) {
                    *switchboard = Some(revived);
                }
            }
            ShellOutcome::Taskbar(TaskbarResponse::SetAppearance { appearance }) => {
                // The desktop's own appearance: re-theme the taskbar model,
                // bring the desktop background in step, and repaint. A prompt
                // showing behind the menu is redrawn too, so nothing on
                // screen is left in the appearance just left behind.
                shell.session_mut().set_appearance(appearance);
                shell.sync_background(compositor);
                shell.present(compositor);
                confirm.repaint(shell, compositor);
            }
            ShellOutcome::Taskbar(TaskbarResponse::LogOut) => {
                // The user asked for the session to end: take the prompt down
                // unanswered (so nothing irreversible follows a log-out) and
                // unwind through the one owner-checked release.
                confirm.abandon(shell, compositor);
                return Routed::EndSession;
            }
            ShellOutcome::Taskbar(TaskbarResponse::ConfirmSystemPower { action }) => {
                // Never on the strength of the click alone: put the
                // consequence to the user first. A prompt that cannot be
                // shown asks nothing and relays nothing.
                if !confirm.ask(action, shell, compositor) {
                    let _ = tairix_rt::stderr(
                        b"desktop: could not ask for confirmation; nothing was done\n",
                    );
                }
            }
            // An event no router acted on, and outcomes the shell has
            // already fully applied with its own state: the
            // click-to-activate/minimise rule, clearing a dismissed
            // notification from the model, and the popup's own open/close.
            // Nothing here needs a capability the shell lacks, so the
            // session adds nothing. Listed rather than caught by a wildcard
            // so a new outcome fails the build instead of being dropped in
            // silence.
            ShellOutcome::Ignored
            | ShellOutcome::Taskbar(
                TaskbarResponse::Ignored
                | TaskbarResponse::LibraryDismissed
                | TaskbarResponse::TaskActivated { .. }
                | TaskbarResponse::DismissNotification { .. }
                | TaskbarResponse::ClockPressed,
            ) => {}
        }
        Routed::Continue
    }

    /// Relay a confirmed power transition over the production mailbox and
    /// state loudly why nothing happened when it could not be relayed.
    fn report_power_relay(answer: Answer, switchboard: Option<u64>) {
        if let Some(reason) = relay_power(answer, switchboard, &mut RtSwitchboardMailbox) {
            let _ = tairix_rt::stderr(alloc::format!("desktop: {reason}\n").as_bytes());
        }
    }

    /// Resolve a press on the pin at `index`: launch its bundle, or raise
    /// the running copy's window. A pin whose target no longer resolves
    /// refuses loudly rather than spawning a guessed path.
    #[allow(clippy::too_many_arguments)] // The serve loop's whole mutable state, threaded explicitly.
    fn activate_pin(
        index: usize,
        pins: &PinPanel,
        shell: &mut DesktopShell,
        compositor: &mut Compositor,
        server: &WindowServer<RtShmMapper>,
        windows: &SessionWindows,
        identity: &RtWindowIdentity,
        launched: &mut LaunchTable,
    ) {
        let Some(pin) = pins.resolved.get(index) else {
            return;
        };
        let Some(run_path) = pin.run_path.as_deref() else {
            let _ = tairix_rt::stderr(
                alloc::format!(
                    "desktop: pin '{}' no longer resolves to an application\n",
                    pin.label
                )
                .as_bytes(),
            );
            return;
        };
        activate_bundle(
            shell, compositor, server, windows, identity, launched, run_path, &pin.label,
        );
    }

    /// Resolve a drop of the armed app-reference drag: a primary release
    /// from the offering window that lands on the bar's pin band pins the
    /// offered bundle at the drop index; anywhere else, the gesture simply
    /// ends (the shared, host-tested `resolve_pin_drop` policy). A refused
    /// admission is reported loudly; the desktop carries on.
    fn resolve_drop(
        window: tairix_wm::WindowId,
        pins: &mut PinPanel,
        shell: &DesktopShell,
        compositor: &Compositor,
        windows: &SessionWindows,
    ) {
        let layout = shell.session().taskbar().layout(compositor.scale());
        let decision = tairix_desktop_session::resolve_pin_drop(
            &mut pins.service,
            windows.ipc_id(window),
            &layout,
            shell.router().pointer(),
        );
        match decision {
            None | Some(PinDecision::Pinned) => {}
            Some(PinDecision::AlreadyPinned) => {
                let _ = tairix_rt::stderr(b"desktop: pin drop: already pinned\n");
            }
            Some(PinDecision::Full) => {
                let _ = tairix_rt::stderr(b"desktop: pin drop: the pin strip is full\n");
            }
            Some(PinDecision::Refused) => {
                let _ = tairix_rt::stderr(b"desktop: pin drop refused\n");
            }
        }
    }

    /// An idempotent bundle activation: raise the running copy's window
    /// when the desktop launched one and it has a window up; do nothing
    /// while its launch is still in flight (no window yet — the press is
    /// already satisfied); spawn only when no desktop-launched copy is
    /// alive. The one rule behind the Files button and every pin press.
    #[allow(clippy::too_many_arguments)] // The serve loop's whole mutable state, threaded explicitly.
    fn activate_bundle(
        shell: &mut DesktopShell,
        compositor: &mut Compositor,
        server: &WindowServer<RtShmMapper>,
        windows: &SessionWindows,
        identity: &RtWindowIdentity,
        launched: &mut LaunchTable,
        run_path: &str,
        label: &str,
    ) {
        if let Some(pid) = launched.running_from(run_path) {
            if let Some(wm) = window_of_pid(pid, server, windows, identity) {
                let _ = shell.raise_window(compositor, wm);
            }
            return;
        }
        record_launch(launched, spawn_app(run_path.as_bytes()), label, run_path);
    }

    /// The compositor window of the first served window owned by `pid`,
    /// resolved through the window engine's attested ownership records —
    /// never a window title or any other app-controlled data.
    fn window_of_pid(
        pid: u64,
        server: &WindowServer<RtShmMapper>,
        windows: &SessionWindows,
        identity: &RtWindowIdentity,
    ) -> Option<tairix_wm::WindowId> {
        windows.served().find_map(|(ipc, wm)| {
            let owner = server.owner_of(ipc)?;
            (identity.pid_of(owner)? == pid).then_some(wm)
        })
    }

    /// Resolve a program-library launch: the chosen entry's bundle names its
    /// `Run` binary; spawn it and record the launch under the entry's
    /// display name.
    fn launch_library_entry(
        shell: &DesktopShell,
        entry: &tairix_proglib::EntryId,
        launched: &mut LaunchTable,
    ) {
        let library = shell.session().taskbar().library();
        let Some(chosen) = library.catalog().entry(entry) else {
            // The popup only reports entries from the catalog it was
            // handed, so a miss means the catalog changed underneath the
            // click; refuse loudly rather than spawning a guessed path.
            let _ = tairix_rt::stderr(
                alloc::format!(
                    "desktop: library entry {} is no longer catalogued\n",
                    entry.as_str()
                )
                .as_bytes(),
            );
            return;
        };
        let run_path = alloc::format!("{}/Run", chosen.bundle().as_str());
        let label = chosen.name().as_str();
        record_launch(launched, spawn_app(run_path.as_bytes()), label, &run_path);
    }

    /// (Re)load the program library from its on-disk stores and hand the
    /// resolved catalog to the taskbar's popup, reporting each unusable
    /// store loudly on `stderr`.
    fn refresh_library(shell: &mut DesktopShell, compositor: &mut Compositor) {
        let home = tairix_rt::env_var(b"HOME").and_then(|raw| core::str::from_utf8(raw).ok());
        let loaded = load_library(&mut VfsFileReader, home);
        for warning in &loaded.warnings {
            let _ = tairix_rt::stderr(warning.as_bytes());
        }
        shell.set_library(compositor, loaded.catalog);
    }

    /// The session's live file-reading seam: whole-file reads through the
    /// kernel VFS under the session's own kernel-attested identity, bounded
    /// just past the program-library document cap — the largest document the
    /// session reads through this seam — so no store can make the desktop
    /// slurp an arbitrarily large file (the loader then refuses the
    /// oversize).
    struct VfsFileReader;

    impl SessionFileReader for VfsFileReader {
        fn read(&mut self, path: &str) -> Result<alloc::vec::Vec<u8>, Errno> {
            let ret = tairix_rt::fs_open(path.as_bytes(), OpenFlags::READ);
            if ret < 0 {
                return Err(Errno::from_syscall(ret));
            }
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            // `ret >= 0` checked above; it is a descriptor number.
            let fd = ret as u32;
            let outcome = read_to_end(fd);
            let _ = tairix_rt::fs_close(fd);
            outcome
        }
    }

    /// The session's live file-writing seam: whole-document replacement
    /// through the kernel VFS under the session's own kernel-attested
    /// identity — the write-side twin of [`VfsFileReader`], used for the
    /// user's own desktop configuration (the taskbar pin store). The
    /// parent directory is created first (`~/Settings/Taskbar` does not
    /// exist until the first pin), an existing directory being the
    /// ordinary case; every permission decision is the kernel's.
    struct VfsFileWriter;

    impl SessionFileWriter for VfsFileWriter {
        fn write(&mut self, path: &str, bytes: &[u8]) -> Result<(), Errno> {
            if let Some((parent, _)) = path.rsplit_once('/') {
                if !parent.is_empty() {
                    let ret = tairix_rt::fs_mkdir(parent.as_bytes());
                    if ret < 0 && Errno::from_syscall(ret) != Errno::AlreadyExists {
                        return Err(Errno::from_syscall(ret));
                    }
                }
            }
            let file = tairix_rt::create(path.as_bytes()).map_err(Errno::from_syscall)?;
            let written = file.write_at(0, bytes).map_err(Errno::from_syscall)?;
            if written != bytes.len() {
                // The backing stopped accepting bytes: report the stall as
                // the out-of-space refusal it is rather than leaving a
                // silently truncated store.
                return Err(Errno::NoSpace);
            }
            Ok(())
        }
    }

    /// Read `fd` from the start until end-of-file, stopping one chunk past
    /// the catalog cap (the caller treats the oversize as the whole-document
    /// refusal it is).
    fn read_to_end(fd: u32) -> Result<alloc::vec::Vec<u8>, Errno> {
        let mut bytes = alloc::vec::Vec::new();
        let mut chunk = [0u8; 1024];
        while bytes.len() <= tairix_proglib::MAX_CATALOG_LEN {
            let read = tairix_rt::fs_read(fd, bytes.len() as u64, &mut chunk)
                .map_err(Errno::from_syscall)?;
            if read == 0 {
                break;
            }
            bytes.extend_from_slice(&chunk[..read]);
        }
        Ok(bytes)
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
    /// display label (so the `CHILD_TOKEN` reap can name the app if its load
    /// is later refused via the child's reserved-`LOAD_*` exit status) and
    /// its spawn path (its attested bundle identity). A synchronous refusal
    /// (`ret < 0` — a stripped spawn capability or a malformed path, decided
    /// before any child exists) is reported fail-loud at once. Either way a
    /// denied optional launch never ends the session.
    fn record_launch(launched: &mut LaunchTable, ret: i64, label: &str, run_path: &str) {
        if ret < 0 {
            let _ =
                tairix_rt::stderr(alloc::format!("desktop: {label} launch refused\n").as_bytes());
        } else {
            #[allow(clippy::cast_sign_loss)] // `ret >= 0` in this branch; it is a PID.
            launched.record(ret as u64, label, run_path);
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
        pins: &mut PinPanel,
    ) {
        let window_id = concluded.for_window;
        let event = match concluded.conclusion {
            PickConclusion::Cancelled => WindowEvent::PickCancelled { window_id },
            PickConclusion::Chosen(path) => {
                if let Some(handle) = delegate(&path, window_id, server, identity) {
                    WindowEvent::FilePicked { window_id, handle }
                } else {
                    let _ = tairix_rt::stderr(b"desktop: picker delegation refused\n");
                    WindowEvent::PickCancelled { window_id }
                }
            }
        };
        deliver(
            server,
            sink,
            shell,
            compositor,
            windows,
            picker,
            &mut pins.service,
            &event,
        );
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
    #[allow(clippy::too_many_arguments)] // The serve loop's whole mutable state, threaded explicitly.
    fn deliver<S: DirectorySource, F: FnMut() -> S>(
        server: &mut WindowServer<RtShmMapper>,
        sink: &mut RtEventSink,
        shell: &mut DesktopShell,
        compositor: &mut Compositor,
        windows: &mut SessionWindows,
        picker: &mut SessionPicker<S, F>,
        pins: &mut dyn PinBridge,
        event: &WindowEvent,
    ) {
        let Some(owner) = server.owner_of(event.window_id()) else {
            return;
        };
        if let Err(Errno::NotFound) = server.deliver_event(sink, event) {
            // `owner_of` proved the window exists, so the `NotFound` is
            // the sink's: the owner's event port is gone — the kernel
            // reclaimed it at exit — and its windows go with it. Any
            // other refusal (the `WouldBlock` back-pressure signal) drops
            // the event only.
            let mut bridge = ShellWindowHost {
                shell,
                compositor,
                windows,
                picker,
                pins,
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
        // The sandbox worker role first, before any argument parsing or
        // seat work: when the session spawns its icon worker it re-enters
        // this same binary with the reserved role argument, and that
        // capability-empty child must serve parses and nothing else.
        if worker_role() {
            let mut service = IconRasterService;
            return match serve_stdio(&mut service) {
                ServeEnd::Finished => 0,
                ServeEnd::Failed(_) => 1,
            };
        }
        // The command surface next: a malformed (non-UTF-8) argument
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
