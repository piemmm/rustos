//! The `Run` entry-point binary of the `desktop` application, installed as
//! a signed bundle in the system application store (`/System/Applications/`)
//! and started two ways through the one bundle: a graphical login
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
    use alloc::vec::Vec;

    use tairix_abi::display_ipc::DISPLAY_ENDPOINT;
    use tairix_abi::elevate::{elevate_endpoint, ElevateReply, ElevateRequest};
    use tairix_abi::input::KeyInput;
    use tairix_abi::notify_ipc::{NotifyRequest, NOTIFY_ENDPOINT, NOTIFY_MAX_REQUEST};
    use tairix_abi::pinboard_ipc::{PINBOARD_ENDPOINT, PINBOARD_MAX_REQUEST};
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
    use tairix_browse::{
        association_from_appinfo, AppAssociation, DirectorySource, VfsDirectorySource,
    };
    use tairix_caps::CapabilitySet;
    use tairix_desktop_session::{
        build_pin_views, deliver_pending_open, desktop_info, load_library, maybe_send_seat_report,
        open_tray, parse, reap_launched, relay_power, serve_pinboard_apply,
        serve_switchboard_request, window_control_event, Answer, ArtworkFileReader, ArtworkSandbox,
        CliError, Command, ConcludedPick, ConfirmPrompt, Delivery, Desktop, DesktopAction,
        DesktopActivation, DesktopOutcome, DesktopShell, DeviceInputSource, DragOrigin,
        HangTracker, HoldBack, IconRasteriser, InputSource, KeyboardInputSource, LaunchTable,
        LockedDrain, OwnerWindow, PickConclusion, PinBridge, PinService, PinboardMenu,
        PinboardMenuOutcome, PinboardStore, PinboardStoreError, ResolvedPin, ScreenLock,
        SeatEventReader, SeatInputChannel, SessionFileReader, SessionFileWriter, SessionPicker,
        SessionPins, SessionWindows, ShellWindowHost, SwitchboardMailbox, SwitchboardOutcome,
        SwitchboardServe, FILES_LABEL, FILES_RUN_PATH, SWITCHBOARD_LABEL, SWITCHBOARD_RUN_PATH,
        USAGE, WALLPAPER_LABEL, WALLPAPER_RUN_PATH,
    };
    use tairix_display::{DisplayClient, DisplayTransport, RemoteDisplay, RtShmMapper};
    use tairix_greeter::{Verdict, Verifier};
    use tairix_help::{own_short_help, BundleHelp};
    use tairix_rt::io::{self, Stderr, Write};
    use tairix_sandbox::imagerender::{rasterise_icon, render_wallpaper, ImageRenderService};
    use tairix_sandbox::rt::{serve_stdio, worker_role, RtLauncher};
    use tairix_sandbox::{ParserSandbox, ServeEnd};
    use tairix_taskbar::{TaskId, TaskbarConfig, TaskbarResponse};
    use tairix_taskpins::PinTarget;
    use tairix_wallpaper::{PinboardSettings, WallpaperChoice, WallpaperFit, MAX_WALLPAPER_BYTES};
    use tairix_window::{
        event_endpoint_for, CallerIdentity, EventSink, PinDecision, WindowServer, WINDOW_REPLY_MAX,
    };
    use tairix_wm::{chrome_cache, Compositor, InputResponse, Rect, Surface};

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

    /// Exit code when the reserved `PINBOARD_ENDPOINT` could not be bound.
    /// It is authorised by this session's kernel-attested live seat lease —
    /// the same lease/rendezvous anomaly as the other three — so the session
    /// exits fail-loud rather than run a desktop whose wallpaper chooser can
    /// never apply anything.
    const EXIT_NO_PINBOARD_ENDPOINT: i32 = 101;

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

    /// The wait-set token of the memory-pressure member: the kernel wakes
    /// the loop when the machine's pressure band changes, so the desktop
    /// gives its cached pixels back as memory tightens instead of holding
    /// them until something else is starved.
    const PRESSURE_TOKEN: u64 = 6;

    /// The wait-set token of the served `PINBOARD_ENDPOINT` member: a tool
    /// the user ran (the wallpaper chooser) asking the session to adopt new
    /// pinboard settings wakes the loop to apply them.
    const PINBOARD_TOKEN: u64 = 7;

    /// The wait-set token every held-back destination's room member carries:
    /// an app draining its full event mailbox wakes the loop to send it what
    /// it is owed. One token for all of them — the flush offers every
    /// destination its events anyway, so which one drained is not worth
    /// distinguishing.
    const HOLDBACK_TOKEN: u64 = 8;

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

    /// Outstanding-call capacity of the pinboard endpoint: an apply is a
    /// deliberate, user-driven act and synchronous, so a tiny queue covers
    /// every real caller (a fail-closed memory bound).
    const PINBOARD_CAPACITY: usize = 4;

    /// The audit sink every cache in this session records through. The shared
    /// cache constructors take a `'static` borrow, and the runtime sink is a
    /// unit value that owns nothing.
    static LOG_SINK: tairix_rt::LogSink = tairix_rt::LogSink;

    /// State the abnormal-exit reason on `stderr` (fail loud: an exit code
    /// alone is not a diagnosis) and hand back `code` for `main` to return.
    fn fail(code: i32, reason: &str) -> i32 {
        let _ = writeln!(Stderr, "desktop: {reason}");
        code
    }

    /// The production [`DisplayTransport`]: one synchronous `ipc_call` to
    /// the reserved display endpoint per request. The display service
    /// re-checks the caller's live seat lease kernel-side on every request,
    /// so the transport carries no claimed authority.
    struct RtDisplayTransport;

    impl DisplayTransport for RtDisplayTransport {
        fn call(&mut self, request: &[u8], reply: &mut [u8]) -> Result<usize, Errno> {
            tairix_rt::ipc_call(DISPLAY_ENDPOINT, request, reply).map_err(Errno::from_syscall)
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
                return Err(Errno::from_syscall(ret));
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
                return Err(Errno::from_syscall(ret));
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

        /// Resolve (and forget) the client whose event mailbox is
        /// `endpoint` — the owner a held-back send has just proved gone.
        ///
        /// Matched *forward*, by deriving each attested peer's mailbox and
        /// comparing, never by inverting the endpoint value back into a pid:
        /// the answer rests on the kernel-attested pid, exactly as the seat
        /// report's owner naming does.
        fn take_by_event_endpoint(&mut self, endpoint: u64) -> Option<ProcId> {
            let pid = self
                .peers
                .keys()
                .copied()
                .find(|pid| event_endpoint_for(*pid) == endpoint)?;
            self.take_by_pid(pid)
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
                .map_err(Errno::from_syscall)?;
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
    /// A refused send is **held**, not dropped ([`HoldBack`]): the mailbox
    /// is a bounded resource and a merely slow app fills it, so dropping
    /// would cost the app a resize it cannot re-derive or a picker
    /// conclusion it is owed exactly once. The sink arms a room member on
    /// the destination's port, and the loop's [`Self::flush`] sends what is
    /// owed the moment the app drains — it never polls for capacity and
    /// never blocks on the app.
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
        /// The wait-set the room members are armed on.
        set: u64,
        held: HoldBack,
    }

    impl RtEventSink {
        /// A sink with no delivery evidence yet, arming its room members on
        /// `set`.
        const fn new(set: u64) -> Self {
            Self {
                vigil: HangTracker::new(),
                changed: false,
                set,
                held: HoldBack::new(),
            }
        }

        /// Watch `endpoint` for room, so the app draining its mailbox wakes
        /// the loop to send what it is owed.
        fn arm(&self, endpoint: u64) -> Result<(), Errno> {
            let ret = tairix_rt::waitset_ctl(
                self.set,
                WaitSetOp::Add,
                WaitSourceKind::PortRoom,
                endpoint,
                HOLDBACK_TOKEN,
            );
            if ret == 0 {
                Ok(())
            } else {
                Err(Errno::from_syscall(ret))
            }
        }

        /// Stop watching `endpoint` for room — it is owed nothing, or its
        /// owner is gone.
        fn disarm(&self, endpoint: u64) {
            let _ = tairix_rt::waitset_ctl(
                self.set,
                WaitSetOp::Del,
                WaitSourceKind::PortRoom,
                endpoint,
                HOLDBACK_TOKEN,
            );
        }

        /// One non-blocking app-ward send, folding its outcome into the
        /// responsiveness evidence.
        ///
        /// Free of `self` so the hold-back can be borrowed across it: both
        /// the first attempt and every later flush go through this one
        /// definition, so an event's send and its evidence can never differ
        /// by which path carried it.
        fn post(
            vigil: &mut HangTracker,
            changed: &mut bool,
            endpoint: u64,
            event: &WindowEvent,
        ) -> Result<(), Errno> {
            let ret = tairix_rt::ipc_send(endpoint, &event.to_le_bytes());
            if ret == 0 {
                *changed |= vigil.note_delivered(endpoint);
                return Ok(());
            }
            let error = Errno::from_syscall(ret);
            *changed |= vigil.note_refused(endpoint, error, tairix_rt::clock_get());
            Err(error)
        }

        /// Send what each destination is owed, as far as its mailbox now
        /// allows, and report the owners the sends proved gone so the loop
        /// can tear their windows down.
        fn flush(&mut self) -> Vec<u64> {
            let vigil = &mut self.vigil;
            let changed = &mut self.changed;
            let report = self
                .held
                .flush(|endpoint, event| Self::post(vigil, changed, endpoint, event));
            for endpoint in report.settled.iter().chain(&report.gone) {
                self.disarm(*endpoint);
            }
            report.gone
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

        /// Drop every verdict held against a reaped child's event mailbox,
        /// and everything still owed to it — a dead app is not a hung app,
        /// a recycled task id must start clean, and events owed to a corpse
        /// have nowhere to land.
        fn forget_owner(&mut self, pid: u64) {
            let endpoint = event_endpoint_for(pid);
            self.changed |= self.vigil.forget(endpoint);
            if self.held.forget(endpoint) {
                self.disarm(endpoint);
            }
        }
    }

    impl EventSink for RtEventSink {
        fn deliver(&mut self, endpoint: u64, event: &WindowEvent) -> Result<(), Errno> {
            let vigil = &mut self.vigil;
            let changed = &mut self.changed;
            // Back-pressure means the app is behind, not gone: the event is
            // held rather than dropped, and the destination watched for room.
            let outcome = self.held.deliver(endpoint, event, |event| {
                Self::post(vigil, changed, endpoint, event)
            })?;
            let Delivery::Owed { watch } = outcome else {
                return Ok(());
            };
            // The event could not be delivered, so the responsiveness
            // evidence stands whether the refusal came from this send or
            // from the debt this one joined: only a delivery the owner
            // accepts clears it.
            self.changed |=
                self.vigil
                    .note_refused(endpoint, Errno::WouldBlock, tairix_rt::clock_get());
            if watch {
                if let Err(error) = self.arm(endpoint) {
                    // Nothing held for a destination that cannot be watched
                    // could ever go out. Restore the invariant the flush
                    // relies on — a destination is watched exactly while it
                    // is owed something — and say so rather than stranding
                    // the events in silence. `NotFound` here is the owner's
                    // port already reclaimed, and answering with it is what
                    // tears its windows down.
                    let _ = self.held.forget(endpoint);
                    self.disarm(endpoint);
                    io::write_stderr_line("desktop: cannot watch an app's mailbox for room");
                    return Err(error);
                }
            }
            Ok(())
        }
    }

    /// The [`Verifier`] the running desktop uses: the per-console elevation
    /// broker served by the login supervisor that started this session.
    ///
    /// The request goes through the shared runtime client, which derives
    /// this process's console from its kernel-attested origin and erases the
    /// request buffer on every return path. The broker re-reads the caller's
    /// identity from the kernel rather than trusting anything sent to it, so
    /// no caller can ask it to check a password against another account.
    struct BrokerUnlocker;

    impl Verifier for BrokerUnlocker {
        fn verify(&mut self, password: &str) -> Verdict {
            match tairix_rt::elevate(&ElevateRequest::Verify { password }) {
                Ok(ElevateReply::Verified) => Verdict::Verified,
                Ok(ElevateReply::Refused(_)) => Verdict::Refused,
                // `Completed` answers a `Run` request, never a `Verify`. A
                // broker that sent it is not speaking this protocol, and a
                // lock does not open on a reply it did not understand.
                Ok(ElevateReply::Completed { .. }) | Err(_) => Verdict::Unreachable,
            }
        }
    }

    /// How a locked drain ended.
    #[derive(Copy, Clone, Debug, Eq, PartialEq)]
    enum Drained {
        /// Both channels are empty. The screen may or may not still be
        /// locked; a verified password takes the lock down mid-drain.
        Empty,
        /// A channel faulted. The seat is no longer trustworthy.
        Faulted,
    }

    /// Drain the seat's pointer and keyboard straight into the lock while
    /// the screen is secured, routing nothing anywhere else.
    ///
    /// This is what makes the lock a lock. The shell is never given the
    /// events, so no motion, click, or keystroke can reach the window
    /// manager, the taskbar, a served application, or the confirmation
    /// prompt while the screen is locked.
    ///
    /// Both channels are drained to empty even once a password has been
    /// verified mid-batch, and everything after that point is discarded by
    /// the shared [`LockedDrain`] rule, which states why.
    fn drain_locked(
        lock: &mut ScreenLock,
        pointer: &mut DeviceInputSource<SeatInputChannel<PointerReader>>,
        keyboard: &mut KeyboardInputSource<SeatInputChannel<KeyboardReader>>,
        unlocker: &mut dyn Verifier,
        shell: &mut DesktopShell,
        compositor: &mut Compositor,
    ) -> Drained {
        let mut drain = LockedDrain::new();
        loop {
            match pointer.poll() {
                Ok(None) => break,
                Ok(Some(event)) => drain.feed(lock, &event, unlocker, shell, compositor),
                Err(_) => return Drained::Faulted,
            }
        }
        loop {
            match keyboard.poll_record() {
                Ok(None) => break,
                Ok(Some((event, _))) => drain.feed(lock, &event, unlocker, shell, compositor),
                Err(_) => return Drained::Faulted,
            }
        }
        Drained::Empty
    }

    /// Classify one drain fault: losing the seat is the session's normal
    /// fail-loud teardown; anything else is an untrustworthy input stream.
    /// Either way the session is ending, so the shell's disposable-UI caches
    /// are wiped before the exit code is returned.
    fn drain_fault(shell: &mut DesktopShell, compositor: &mut Compositor, err: Errno) -> i32 {
        shell.teardown(compositor);
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
    /// so a lost seat is observed here exactly as on a drain. Any refusal
    /// ends the session, so the shell's disposable-UI caches are wiped
    /// before the exit code is returned.
    fn present(
        shell: &mut DesktopShell,
        compositor: &mut Compositor,
        display: &mut RemoteDisplay<'_, RtDisplayTransport>,
    ) -> Result<(), i32> {
        match compositor.present(display) {
            Ok(()) => Ok(()),
            Err(DriverError::SeatRevoked | DriverError::PermissionDenied) => {
                shell.teardown(compositor);
                Err(fail(
                    EXIT_SEAT_LOST,
                    "seat lease lost; tearing the session down",
                ))
            }
            Err(_) => {
                shell.teardown(compositor);
                Err(fail(EXIT_PRESENT_FAILED, "display present refused"))
            }
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
        let len = tairix_rt::call_peer_origin(NOTIFY_ENDPOINT, ticket, &mut buf)
            .map_err(Errno::from_syscall)?;
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
            .map_err(Errno::from_syscall)?;
        let origin = Origin::from_bytes(&buf[..len])?;
        serve_switchboard_request(serve, origin.pid(), request).map_err(|refusal| {
            let msg = refusal.reason();
            let _ = writeln!(Stderr, "desktop: {msg}");
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
    /// The send never parks the desktop and never retries in a loop: it
    /// makes one attempt and answers whether the mailbox took it, leaving
    /// the caller to decide whether the command is worth holding for the
    /// monitor's next publish.
    ///
    /// A refusal is stated on `stderr` with the kernel's own reason rather
    /// than a guess — `WouldBlock` is back-pressure from a mailbox the
    /// monitor has not drained, while `NotFound` is an instance that has
    /// exited or has not bound its mailbox yet, and calling the second one
    /// "full" would send a reader looking for a problem that is not there.
    struct RtSwitchboardMailbox;

    impl SwitchboardMailbox for RtSwitchboardMailbox {
        fn send(&mut self, pid: u64, command: SwitchboardCommand) -> bool {
            let ret = tairix_rt::ipc_send(command_endpoint_for(pid), &command.to_le_bytes());
            if ret == 0 {
                return true;
            }
            let _ = writeln!(
                Stderr,
                "desktop: switchboard command dropped: {}",
                Errno::from_syscall(ret)
            );
            false
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
            spawn_app(SWITCHBOARD_RUN_PATH.as_bytes(), &[]),
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

    /// Withdraws this process's reported cache rows on every way out of
    /// [`session`] once the desktop's caches are registered, so the
    /// system's cache monitor never keeps showing memory the ended session
    /// no longer holds.
    struct CacheReportGuard;

    impl Drop for CacheReportGuard {
        fn drop(&mut self) {
            tairix_rt::cachereport::withdraw();
        }
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
        // The seat's rasterised-asset caches are budgeted from one frame of
        // this very output, so the desktop is allowed more cached pixels on
        // a large display than a small one and no ceiling is guessed. They
        // are governed by the process pressure gauge, which the wait loop
        // below keeps current from the kernel's band.
        //
        // Publish the band *before* those caches exist: the gauge starts in
        // its fail-closed unknown state, where every cache admits nothing, so
        // a desktop that waited for its wait-set member would draw the whole
        // bring-up with no cached cursor, glyph, or icon artwork.
        let _ = tairix_procinfo::pressure::refresh();
        let mut shell = DesktopShell::new(
            TaskbarConfig::bottom_bar(mode.width_px, mode.height_px),
            SEAT_PRIMARY,
            frame_len,
            tairix_rt::pressure::gauge(),
            &LOG_SINK,
        );
        // The shell registered the desktop's own cache rows, so from here
        // every way out — a bring-up refusal below or the serve loop's
        // fail-loud exit — has to take them back out of the monitor's
        // registry; a dropped guard does that once, unconditionally.
        let _cache_report = CacheReportGuard;
        // The decorated windows' furniture is the output's own cache, so it
        // is built here from the same seat, output size, gauge, and sink and
        // handed to the compositor that draws from it. The compositor takes
        // the gauge itself as well: a window's *content* is not a keyed
        // cache but a release policy over the same band, so it reads the
        // pressure directly rather than through the furniture cache.
        //
        // It is this process's memory like the shell's three caches, so it
        // joins them in the report before the compositor takes it: a ledger
        // is a shared handle to the figures, not the cache itself.
        let chrome = chrome_cache(
            SEAT_PRIMARY,
            frame_len,
            tairix_rt::pressure::gauge(),
            &LOG_SINK,
        );
        if let Some(ledger) = chrome.ledger() {
            tairix_rt::cachereport::register(ledger);
        }
        let Some(mut compositor) = Compositor::new(
            mode,
            shell.desktop_background(),
            chrome,
            tairix_rt::pressure::gauge(),
        ) else {
            return fail(EXIT_BAD_MODE, "compositor rejected the queried mode");
        };
        let screen = Rect::new(0, 0, mode.width_px, mode.height_px);
        let Ok(mut pointer) = DeviceInputSource::new(SeatInputChannel::new(PointerReader), screen)
        else {
            return fail(EXIT_BAD_MODE, "queried mode has no pointer surface");
        };
        let mut keyboard = KeyboardInputSource::new(SeatInputChannel::new(KeyboardReader));

        // One parser-sandbox worker for every untrusted image this session
        // decodes: the desktop's icon artwork and the user's wallpaper
        // alike. Both are attacker-influenced files, both are decoded in a
        // capability-empty re-entry of this binary rather than in this
        // address space, and sharing the one worker keeps a second decode
        // path from existing.
        let sandbox: SharedSandbox = alloc::rc::Rc::new(core::cell::RefCell::new(
            ParserSandbox::new(RtLauncher::own_binary(), tairix_rt::LogSink),
        ));

        // The desktop's icon artwork — the shipped `/System/Graphics`
        // masters and each bundle's own icon — is read through the
        // session's own VFS identity and decoded in that worker. Until this
        // call the shell draws every icon from its built-in glyphs, and it
        // falls back to them again whenever either seam refuses.
        shell.set_artwork_source(
            alloc::boxed::Box::new(ArtworkFileReader(VfsFileReader)),
            alloc::boxed::Box::new(ArtworkSandbox(SandboxRasteriser {
                sandbox: alloc::rc::Rc::clone(&sandbox),
            })),
        );

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

        // The desktop's own icon column: the logged-in user's `Desktop`
        // folder, listed through the same capability-checked directory call
        // the trusted picker uses, under the session's own identity. An
        // unset or malformed `HOME` leaves the folder at the storage root's
        // `Desktop`, which simply lists nothing if it is not there.
        let mut desktop_folder = tairix_rt::env_var(b"HOME")
            .and_then(|home| core::str::from_utf8(home).ok())
            .and_then(|home| tairix_browse::vfs::components_from_absolute_path(home).ok())
            .unwrap_or_default();
        desktop_folder.push(alloc::string::String::from("Desktop"));
        let mut desktop = Desktop::new(
            VfsDirectorySource::new(|path: &str| {
                tairix_rt::read_dir_all(path.as_bytes()).map_err(Errno::from_syscall)
            }),
            desktop_folder,
        );
        // The user's pinboard settings, with the same fail-closed posture as
        // the pin store: absent → the defaults, silently (a fresh account);
        // unusable → the defaults plus one loud reason. They are applied
        // *before* the first listing, so the very first frame already has
        // the user's own sort order and icon arrangement rather than
        // re-sorting a frame later.
        let mut pinboard = load_pinboard(&mut desktop, sandbox);
        desktop.relist(tairix_rt::clock_get());
        // Which installed application opens which file, read once from the
        // bundles the catalog already names and refreshed only when the
        // catalog is. Resolving it per gesture would re-read every manifest
        // on every click.
        let mut associations = desktop_associations(&shell);

        // The wallpaper the desktop layer is painted over: read under the
        // session's own identity and fitted to this screen in the sandbox
        // worker, once. A wallpaper that cannot be read or rendered leaves
        // the backdrop colour showing and states why — the desktop never
        // fails over a picture.
        prepare_wallpaper(&mut pinboard, &mut shell, &desktop, &compositor);

        // First frame: place the bar, paint the desktop's icons beneath
        // every window, install the pointer cursor at the seat's initial
        // pointer position, and push the whole surface once;
        // every later present carries only the composited damage. The cursor
        // is then kept live by the shell as each seat event is pumped.
        shell.present(&mut compositor);
        shell.present_desktop(&mut compositor, &desktop);
        shell.refresh_cursor(&mut compositor);
        if let Err(code) = present(&mut shell, &mut compositor, &mut display) {
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

        // Bind the pinboard rendezvous the same way: the fourth seat-scoped
        // reserved id, authorised by the same live seat lease,
        // unrestricted-sender — the serve arm compares every caller's
        // kernel-attested origin uid against this session's own and refuses
        // anything else, so an unentitled sender only ever reaches a typed
        // refusal.
        if tairix_rt::call_create(
            PINBOARD_ENDPOINT,
            &empty,
            &empty,
            PINBOARD_MAX_REQUEST,
            STATUS_REPLY_LEN,
            PINBOARD_CAPACITY,
        ) != 0
        {
            return fail(EXIT_NO_PINBOARD_ENDPOINT, "pinboard endpoint bind refused");
        }

        // Park on the wait-set: the seat member wakes on input delivery
        // and on lease loss, the endpoint member on a posted window
        // request, the any-child member when a spawned app exits (so its
        // windows are torn down promptly), and the memory-pressure member
        // when the machine's pressure band moves. Every member is
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
            WaitSourceKind::Endpoint,
            PINBOARD_ENDPOINT,
            PINBOARD_TOKEN,
        ) != 0
        {
            return fail(EXIT_WAIT_FAILED, "pinboard endpoint wait refused");
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
        // `watch` re-reads the band as it registers the member, closing the
        // race between the bring-up read above and this registration — a
        // move in between would otherwise never be seen.
        if !tairix_procinfo::pressure::watch(set, PRESSURE_TOKEN) {
            return fail(EXIT_WAIT_FAILED, "memory-pressure wait refused");
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
        let mut sink = RtEventSink::new(set);
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
                tairix_rt::read_dir_all(path.as_bytes()).map_err(Errno::from_syscall)
            })
        })
        .starting_at(picker_start);
        // The trusted confirmation prompt for a power transition. It is the
        // session's own window, so the question the user answers is asked by
        // the desktop itself rather than by the bar, which holds no
        // authority; an unanswered prompt relays nothing.
        let mut confirm = ConfirmPrompt::new();
        // The screen lock, and the account it re-verifies. `USER` is what
        // login exported for this session; it names whose password the
        // prompt is asking for and nothing more — the broker reads the
        // identity it actually checks against from the kernel, so a wrong
        // or missing name here cannot unlock anybody's session. An unset or
        // malformed value simply leaves the prompt unnamed.
        let mut lock = ScreenLock::new();
        let account = tairix_rt::env_var(b"USER")
            .and_then(|raw| core::str::from_utf8(raw).ok())
            .unwrap_or_default();
        // Offer the Lock row only where this session really has a password
        // prompt to unlock with: a console the login supervisor brokers
        // re-verification on. Without one, locking would strand the user.
        shell.set_lock_available(
            &mut compositor,
            elevate_endpoint(self_origin.console()).is_ok(),
        );

        let mut token = 0u64;
        loop {
            // The park stays indefinite: a cache-report change the rate
            // limiter is holding back only ever *tightens* the wait to the
            // moment it may be sent, and folds back to indefinite once it
            // has gone out. The desktop never polls for anything.
            let timeout_ns = tairix_rt::cachereport::fold_wait_deadline_ns(u64::MAX);
            let waited = tairix_rt::waitset_wait(set, timeout_ns, &mut token);
            if waited != 0 {
                if Errno::from_syscall(waited) != Errno::TimedOut {
                    // A dead wait-set would degrade the loop into a busy poll;
                    // exit fail-loud instead and let the supervisor decide.
                    return fail(EXIT_WAIT_FAILED, "seat wait failed");
                }
                // No member woke, so `token` still names the *previous*
                // wake's source and dispatching on it would block in a
                // `call_recv` with nothing to receive. The held-back report
                // is the only bounded wait this loop arms: send it and park
                // again.
                tairix_rt::cachereport::publish_if_due();
                continue;
            }
            // Dispatch on the woken member's token and handle only that
            // source: `call_recv` *blocks* when nothing is pending, so a
            // seat-input wake must never touch the window endpoint (and
            // vice versa). Readiness is a non-consuming peek, so a member
            // left pending re-reports on the very next wait, and the
            // wait-set hands ready members out in turn — which is what
            // makes one source per wake safe. Were it fixed priority by
            // registration order instead, a hand on the mouse would hold
            // the seat member ready for as long as it moved and every
            // application blocked in a window call would hang until it
            // stopped.
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
                                        spawn_app(run_path.as_bytes(), &[]),
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
            } else if token == PINBOARD_TOKEN {
                // Serve a pending pinboard apply: attest that the caller
                // runs as this session's own user from the kernel (never
                // the wire), parse the carried document with the one
                // settings engine, and put it through the very same
                // persist-then-adopt path the backdrop menu uses, so the
                // two routes cannot diverge. A foreign caller, a malformed
                // frame, an unusable document, or a refused write is a
                // typed refusal stated on `stderr`, so no caller is left
                // parked and the desktop keeps the settings it had.
                let mut request = [0u8; PINBOARD_MAX_REQUEST];
                let mut ticket = 0u64;
                if let Ok(len) = tairix_rt::call_recv(PINBOARD_ENDPOINT, &mut request, &mut ticket)
                {
                    let result = serve_pinboard(
                        &mut pinboard,
                        &mut desktop,
                        &mut shell,
                        &mut compositor,
                        self_origin.uid(),
                        ticket,
                        &request[..len],
                    );
                    let reply = encode_status_reply(result);
                    let _ = tairix_rt::call_reply(PINBOARD_ENDPOINT, ticket, &reply);
                }
            } else if token == HOLDBACK_TOKEN {
                // An app drained its full event mailbox, so what the session
                // owes it can go out. The room member is armed exactly while
                // a destination is owed something, so this never runs on a
                // wake nobody asked for, and it is the only path that sends
                // a held event — the desktop never polls for capacity.
                for endpoint in sink.flush() {
                    // The send proved this owner gone before its exit was
                    // reaped. Its windows go with it, exactly as a refused
                    // direct send tears them down.
                    if let Some(client) = identity.take_by_event_endpoint(endpoint) {
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
                }
            } else if token == PRESSURE_TOKEN {
                // The machine's memory-pressure band moved. Read it and, if
                // it really changed, give back whatever the new band says a
                // disposable-UI cache may keep — at the moment pressure
                // rises, not at whatever later frame happens to touch a
                // cache. The desktop's rasterised pixels are among the first
                // memory the system reclaims, ahead of clean file data and
                // well ahead of compressing anyone's anonymous pages.
                //
                // The cursor, glyphs, and window furniture remain correct
                // throughout: a dropped entry is simply rendered again on
                // demand, so this costs rendering work and never a wrong
                // pixel. Nothing is repainted here, and a band that demands
                // nothing releases nothing, so a wake the desktop has
                // already acted on is almost free.
                //
                // Window *content* is the one thing the desktop cannot
                // re-render itself, so every window whose pixels the same
                // trim released is asked to present again straight away.
                if tairix_procinfo::pressure::refresh() {
                    let _ = shell.trim_caches(&mut compositor);
                    tairix_font::trim_glyph_cache();
                    deliver_pending_redraws(
                        &mut server,
                        &mut sink,
                        &mut shell,
                        &mut compositor,
                        &mut windows,
                        &mut picker,
                        &mut pins.service,
                    );
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
                        let _ = write!(Stderr, "{line}");
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
                // A program the desktop started has finished, and it may
                // have written to the folder the icons come from. This
                // system has no filesystem-change notification, so an exit
                // the session itself observes is one of the few honest
                // moments to look again — and it is an event, never a poll.
                if desktop.relist(tairix_rt::clock_get()) {
                    shell.present_desktop(&mut compositor, &desktop);
                }
            } else if token == SEAT_TOKEN && lock.is_locked() {
                // Locked: the seat's events belong to the lock and to
                // nothing else. They are drained straight out of the
                // channels here — not through the shell — so no pointer
                // motion, click, or keystroke can reach the window manager,
                // the taskbar, or a served application while the screen is
                // secured. This is the routing half of the lock; the
                // full-screen surface only hides the session.
                if drain_locked(
                    &mut lock,
                    &mut pointer,
                    &mut keyboard,
                    &mut BrokerUnlocker,
                    &mut shell,
                    &mut compositor,
                ) == Drained::Faulted
                {
                    return drain_fault(&mut shell, &mut compositor, Errno::DeviceFault);
                }
            } else if token == SEAT_TOKEN && pinboard.menu.is_open() {
                // The backdrop menu is up, so it is modal: the seat's
                // events are drained straight into it here — not through
                // the shell — so no press or keystroke behind the open
                // plate reaches a window, the bar, or the icon column.
                // This is the routing half of the menu's modality, exactly
                // as the lock's drain above is the routing half of the
                // lock.
                let now_ns = tairix_rt::clock_get();
                if drain_pinboard_menu(
                    &mut pinboard,
                    &mut pointer,
                    &mut keyboard,
                    &mut desktop,
                    &mut shell,
                    &mut compositor,
                    &mut launched,
                    &mut associations,
                    now_ns,
                ) == Drained::Faulted
                {
                    return drain_fault(&mut shell, &mut compositor, Errno::DeviceFault);
                }
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
                    Err(err) => return drain_fault(&mut shell, &mut compositor, err),
                };
                for outcome in outcomes {
                    route_desktop(
                        &outcome,
                        &mut pinboard,
                        &mut desktop,
                        &mut shell,
                        &mut compositor,
                        &mut launched,
                        &mut associations,
                        now_ns,
                    );
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
                        &mut lock,
                        account,
                        &mut launched,
                        &mut pins,
                        &mut switchboard_pid,
                        &mut pending_open,
                        &mut associations,
                    ) == Routed::EndSession
                    {
                        shell.teardown(&mut compositor);
                        return EXIT_LOGGED_OUT;
                    }
                }
                loop {
                    match keyboard.poll_record() {
                        Ok(None) => break,
                        Ok(Some((event, record))) => {
                            let outcome = shell.handle(event, &mut compositor, now_ns);
                            route_desktop(
                                &outcome,
                                &mut pinboard,
                                &mut desktop,
                                &mut shell,
                                &mut compositor,
                                &mut launched,
                                &mut associations,
                                now_ns,
                            );
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
                                &mut lock,
                                account,
                                &mut launched,
                                &mut pins,
                                &mut switchboard_pid,
                                &mut pending_open,
                                &mut associations,
                            ) == Routed::EndSession
                            {
                                shell.teardown(&mut compositor);
                                return EXIT_LOGGED_OUT;
                            }
                        }
                        Err(err) => return drain_fault(&mut shell, &mut compositor, err),
                    }
                }
                // A window restored from the taskbar (or otherwise shown
                // again) whose content was released while it was hidden
                // has nothing to draw until its app presents, so ask now
                // rather than leaving it blank until the next wake.
                deliver_pending_redraws(
                    &mut server,
                    &mut sink,
                    &mut shell,
                    &mut compositor,
                    &mut windows,
                    &mut picker,
                    &mut pins.service,
                );
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
            // Glue each open popup back onto the window that owns it, so
            // nothing raised anywhere else this wake can land between a
            // parent and its menu or sheet. Idle when no popup is open, and
            // before the lock so the lock still ends up above everything.
            windows.keep_popups_stacked(&mut compositor);
            // Nothing an application does may surface over a locked
            // screen: whatever opened, raised, or resized behind the lock
            // this wake, the lock goes back on top before the frame is
            // shown. Idle when the screen is not locked.
            lock.keep_topmost(&mut compositor);
            // One present per wake: the compositor tracks the damage the
            // pumped events and served presents produced and the ring
            // copies only that region.
            if let Err(code) = present(&mut shell, &mut compositor, &mut display) {
                return code;
            }
            // The wake is fully handled and its frame is on screen: report
            // what the desktop's caches hold now, before parking again. A
            // change made this turn would otherwise wait for the next wake,
            // which on an idle desktop may be a very long time. Silent
            // unless a figure actually moved.
            tairix_rt::cachereport::publish_if_due();
        }
    }

    /// The session's pin state: the store-owning service plus the resolved
    /// pins and their live running-window matches, kept beside the loop so a
    /// press resolves against exactly what the strip shows.
    ///
    /// The pins' icons are not here: the shell owns the one artwork cache
    /// and the seams it reads and decodes through, so the strip's icons and
    /// the rest of the desktop's cannot be cached twice.
    struct PinPanel {
        service: PinService<VfsFileReader, VfsFileWriter>,
        resolved: alloc::vec::Vec<ResolvedPin>,
        matches: alloc::vec::Vec<Option<TaskId>>,
    }

    /// The one parser-sandbox worker the icon artwork and the wallpaper
    /// preparation both decode through: this binary re-entered as a single
    /// capability-empty worker, never a second one spawned for wallpaper
    /// alone. Shared because the shell owns the icon rasteriser behind a
    /// boxed trait object while the wallpaper preparation below needs the
    /// very same live worker from the serve loop's own state.
    type SharedSandbox =
        alloc::rc::Rc<core::cell::RefCell<ParserSandbox<RtLauncher, tairix_rt::LogSink>>>;

    /// The production [`IconRasteriser`]: untrusted icon bytes go to the
    /// parser-sandbox icon service — this binary re-entered as a
    /// capability-empty worker — and only a verified pixel block comes
    /// back. Any refusal (malformed image, crashed worker, unavailable
    /// spawn) is `None`: the pin falls back to its class glyph.
    struct SandboxRasteriser {
        sandbox: SharedSandbox,
    }

    impl IconRasteriser for SandboxRasteriser {
        fn rasterise(&mut self, side: u32, icon: &[u8]) -> Option<alloc::vec::Vec<u8>> {
            rasterise_icon(&mut self.sandbox.borrow_mut(), side, icon).ok()
        }
    }

    /// The session's pinboard state, kept beside the loop: the backdrop's
    /// context menu, the per-user settings store changes are persisted
    /// through, the sandbox worker the wallpaper is decoded in, and what the
    /// wallpaper surface now on screen was prepared from.
    ///
    /// The settings themselves are *not* here: the desktop model owns them,
    /// so there is exactly one copy of what is in force.
    struct PinboardPanel {
        menu: PinboardMenu,
        store: PinboardStore,
        sandbox: SharedSandbox,
        prepared: Option<WallpaperSource>,
    }

    /// What the wallpaper surface currently installed in the shell was
    /// prepared from.
    ///
    /// Preparing a wallpaper reads a file and runs a sandboxed decode, so it
    /// happens only when one of these inputs really changed — never on a
    /// frame path. It is recorded *before* the attempt is made, so a file
    /// that cannot be read or rendered costs one refusal rather than one per
    /// frame.
    #[derive(Clone, Eq, PartialEq)]
    struct WallpaperSource {
        choice: WallpaperChoice,
        fit: WallpaperFit,
        width: u32,
        height: u32,
    }

    /// Load the user's pinboard settings into `desktop` (reporting an
    /// unusable store loudly) and answer with the session's pinboard state
    /// over the production file seams and `sandbox`.
    ///
    /// The settings are applied to the model here rather than returned, so
    /// only the desktop ever holds what is in force.
    fn load_pinboard<S: DirectorySource>(
        desktop: &mut Desktop<S>,
        sandbox: SharedSandbox,
    ) -> PinboardPanel {
        let home = tairix_rt::env_var(b"HOME").and_then(|raw| core::str::from_utf8(raw).ok());
        let loaded = PinboardStore::load(&mut VfsFileReader, home);
        if let Some(warning) = loaded.warning {
            let _ = write!(Stderr, "{warning}");
        }
        desktop.apply_settings(loaded.settings);
        PinboardPanel {
            menu: PinboardMenu::new(),
            store: loaded.store,
            sandbox,
            prepared: None,
        }
    }

    /// Prepare the wallpaper the desktop layer is painted over and install it
    /// in the shell, doing nothing at all when the surface already on screen
    /// was prepared from exactly these settings at exactly this screen size.
    ///
    /// The chosen file is read under the session's own identity, bounded by
    /// the shared wallpaper cap, and fitted to the whole screen in the
    /// sandbox worker — so untrusted image bytes are never decoded in this
    /// address space. A wallpaper that cannot be read or rendered installs
    /// no surface, leaving the backdrop colour as the whole base, and states
    /// why once.
    fn prepare_wallpaper<S: DirectorySource>(
        pinboard: &mut PinboardPanel,
        shell: &mut DesktopShell,
        desktop: &Desktop<S>,
        compositor: &Compositor,
    ) {
        let settings = desktop.settings();
        let screen = compositor.screen_rect();
        let wanted = WallpaperSource {
            choice: settings.wallpaper.clone(),
            fit: settings.fit,
            width: screen.width,
            height: screen.height,
        };
        if pinboard.prepared.as_ref() == Some(&wanted) {
            return;
        }
        let WallpaperChoice::Image(path) = &wanted.choice else {
            pinboard.prepared = Some(wanted);
            shell.set_wallpaper(None);
            return;
        };
        let surface = render_wallpaper_surface(
            &pinboard.sandbox,
            path.as_str(),
            wanted.fit,
            wanted.width,
            wanted.height,
        );
        pinboard.prepared = Some(wanted);
        shell.set_wallpaper(surface);
    }

    /// Read the image at `path`, place it over a `width` × `height` screen
    /// under `fit` in the sandbox worker, and rebuild the result as the
    /// surface the compositor blits.
    ///
    /// Every refusal — a file that cannot be read, one larger than any
    /// wallpaper, a malformed image, a crashed worker, or a reply whose
    /// pixels do not fill the screen — answers `None` with its reason on
    /// `stderr`, so the desktop falls back to the backdrop colour instead of
    /// failing over a picture.
    fn render_wallpaper_surface(
        sandbox: &SharedSandbox,
        path: &str,
        fit: WallpaperFit,
        width: u32,
        height: u32,
    ) -> Option<Surface> {
        let bytes = match read_file(path, MAX_WALLPAPER_BYTES) {
            Ok(bytes) if bytes.len() > MAX_WALLPAPER_BYTES => {
                let _ = writeln!(
                    Stderr,
                    "desktop: wallpaper {path} is larger than any wallpaper the desktop renders; \
                     using the backdrop colour"
                );
                return None;
            }
            Ok(bytes) => bytes,
            Err(err) => {
                let _ = writeln!(
                    Stderr,
                    "desktop: wallpaper {path} could not be read ({err}); \
                     using the backdrop colour"
                );
                return None;
            }
        };
        let placed = match render_wallpaper(&mut sandbox.borrow_mut(), width, height, fit, &bytes) {
            Ok(placed) => placed,
            Err(err) => {
                let _ = writeln!(
                    Stderr,
                    "desktop: wallpaper {path} could not be rendered ({err}); \
                     using the backdrop colour"
                );
                return None;
            }
        };
        let surface = Surface::from_rgba8(width, height, &placed);
        if surface.is_none() {
            let _ = writeln!(
                Stderr,
                "desktop: wallpaper {path} did not fill the screen; using the backdrop colour"
            );
        }
        surface
    }

    /// Drain the seat's pointer and keyboard straight into the open backdrop
    /// menu, routing nothing anywhere else.
    ///
    /// This is what makes the menu modal: the shell is never given the
    /// events, so no press or keystroke behind the plate reaches a window,
    /// the bar, or the icon column while the menu is up. A chosen row is
    /// resolved by the desktop model — the same resolution a gesture goes
    /// through — and carried out by the same one action path, so a command
    /// and a double-click can never disagree. A press away and Escape both
    /// dismiss, which takes the menu's window down.
    #[allow(clippy::too_many_arguments)] // The desktop's whole mutable state, threaded explicitly.
    fn drain_pinboard_menu<S: DirectorySource>(
        pinboard: &mut PinboardPanel,
        pointer: &mut DeviceInputSource<SeatInputChannel<PointerReader>>,
        keyboard: &mut KeyboardInputSource<SeatInputChannel<KeyboardReader>>,
        desktop: &mut Desktop<S>,
        shell: &mut DesktopShell,
        compositor: &mut Compositor,
        launched: &mut LaunchTable,
        associations: &mut alloc::vec::Vec<AppAssociation>,
        now_ns: u64,
    ) -> Drained {
        loop {
            match pointer.poll() {
                Ok(None) => break,
                Ok(Some(event)) => {
                    // Motion alone still goes to the shell, so the tracked
                    // pointer and the on-screen cursor stay in step for the
                    // moment the plate closes; its outcome is discarded and
                    // no press ever reaches it, which is what keeps the
                    // windows beneath the plate untouched.
                    if matches!(event, tairix_wm::InputEvent::PointerMoved { .. }) {
                        let _ = shell.handle(event, compositor, now_ns);
                    }
                    let acted = if let Some(bounds) =
                        shell.pinboard_menu_bounds(compositor, &pinboard.menu)
                    {
                        pinboard.menu.on_pointer(
                            &event,
                            shell.router().pointer(),
                            bounds,
                            compositor.scale(),
                            shell.session().active_theme(),
                        )
                    } else {
                        // An open menu the shell cannot place has no plate to
                        // route against; dismiss rather than guess at one.
                        pinboard.menu.close();
                        PinboardMenuOutcome::Dismissed
                    };
                    settle_pinboard_menu(
                        acted,
                        pinboard,
                        desktop,
                        shell,
                        compositor,
                        launched,
                        associations,
                        now_ns,
                    );
                }
                Err(_) => return Drained::Faulted,
            }
        }
        loop {
            match keyboard.poll_record() {
                Ok(None) => break,
                // Every key belongs to the open plate: the arrows move the
                // highlight, Enter chooses, Escape dismisses, and a key it
                // has no meaning for is dropped rather than reaching a
                // window behind it.
                Ok(Some((tairix_wm::InputEvent::KeyPressed { key, .. }, _))) => {
                    let acted = pinboard.menu.on_key(key);
                    settle_pinboard_menu(
                        acted,
                        pinboard,
                        desktop,
                        shell,
                        compositor,
                        launched,
                        associations,
                        now_ns,
                    );
                }
                Ok(Some(_)) => {}
                Err(_) => return Drained::Faulted,
            }
        }
        Drained::Empty
    }

    /// Apply one backdrop-menu outcome: repaint a moved highlight, take the
    /// menu's window down when it closed, and put a chosen command through
    /// the desktop model and the one action path.
    #[allow(clippy::too_many_arguments)] // The desktop's whole mutable state, threaded explicitly.
    fn settle_pinboard_menu<S: DirectorySource>(
        acted: PinboardMenuOutcome,
        pinboard: &mut PinboardPanel,
        desktop: &mut Desktop<S>,
        shell: &mut DesktopShell,
        compositor: &mut Compositor,
        launched: &mut LaunchTable,
        associations: &mut alloc::vec::Vec<AppAssociation>,
        now_ns: u64,
    ) {
        match acted {
            PinboardMenuOutcome::Ignored => {}
            // The one presentation call both repaints an open plate and takes
            // a closed one's window down, so a dismissal needs no second
            // path.
            PinboardMenuOutcome::Changed | PinboardMenuOutcome::Dismissed => {
                shell.present_pinboard_menu(compositor, &pinboard.menu);
            }
            PinboardMenuOutcome::Chose(command) => {
                shell.present_pinboard_menu(compositor, &pinboard.menu);
                // The model resolves the command against its own state, so a
                // row and the equivalent gesture produce the very same
                // action; the session merely carries it out.
                let acted = desktop.command(command, associations, now_ns);
                let redraw = acted.redraw
                    | apply_desktop_action(
                        acted.action,
                        pinboard,
                        desktop,
                        shell,
                        compositor,
                        launched,
                        now_ns,
                    );
                if acted.relisted {
                    refresh_library(shell, compositor);
                    *associations = desktop_associations(shell);
                }
                if redraw {
                    shell.present_desktop(compositor, desktop);
                }
            }
        }
    }

    /// Attest the caller of a pending pinboard call from the kernel, decode
    /// the settings it carries through the shared, host-tested policy, and
    /// adopt them through the session's one persist-then-adopt path.
    ///
    /// Only a caller running as this session's own user may rewrite this
    /// session's desktop: the uid compared is the kernel-attested
    /// `call_peer_origin` uid, never a wire claim. Anything else — another
    /// user's process, a malformed frame, an unusable document, a refused
    /// store write — is a typed refusal stated on `stderr` that adopts
    /// nothing (fail closed). The document merely *names* a wallpaper path;
    /// the session reads it under its own identity, so this channel reaches
    /// no file the session could not already read.
    #[allow(clippy::too_many_arguments)] // The desktop's whole mutable state, threaded explicitly.
    fn serve_pinboard<S: DirectorySource>(
        pinboard: &mut PinboardPanel,
        desktop: &mut Desktop<S>,
        shell: &mut DesktopShell,
        compositor: &mut Compositor,
        session_uid: u32,
        ticket: u64,
        request: &[u8],
    ) -> Result<(), Errno> {
        let mut buf = [0u8; ORIGIN_WIRE_LEN];
        let len = tairix_rt::call_peer_origin(PINBOARD_ENDPOINT, ticket, &mut buf)
            .map_err(Errno::from_syscall)?;
        let origin = Origin::from_bytes(&buf[..len])?;
        let settings =
            serve_pinboard_apply(session_uid, origin.uid(), request).map_err(|refusal| {
                let msg = refusal.reason();
                let _ = writeln!(Stderr, "desktop: {msg}");
                refusal.errno()
            })?;
        adopt_pinboard_settings(
            settings,
            pinboard,
            desktop,
            shell,
            compositor,
            tairix_rt::clock_get(),
        )
        .map_err(PinboardStoreError::errno)?;
        shell.present_desktop(compositor, desktop);
        Ok(())
    }

    /// Load the user's pin store (reporting an unusable one loudly) into a
    /// service over the production file seams.
    fn load_pin_service() -> PinPanel {
        let home = tairix_rt::env_var(b"HOME").and_then(|raw| core::str::from_utf8(raw).ok());
        let (store, warning) = SessionPins::load(&mut VfsFileReader, home);
        if let Some(warning) = warning {
            let _ = write!(Stderr, "{warning}");
        }
        PinPanel {
            service: PinService::new(VfsFileReader, VfsFileWriter, store),
            resolved: alloc::vec::Vec::new(),
            matches: alloc::vec::Vec::new(),
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
    /// strip's own icon geometry through the shell's sandboxed pipeline,
    /// served from its one cache on every later push.
    fn push_pin_views(pins: &mut PinPanel, shell: &mut DesktopShell, compositor: &mut Compositor) {
        let side = shell.session().taskbar().pin_icon_side(compositor.scale());
        let (cache, reader, rasteriser) = shell.artwork_parts();
        let views = build_pin_views(
            &pins.resolved,
            &pins.matches,
            reader,
            rasteriser,
            cache,
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
        lock: &mut ScreenLock,
        account: &str,
        launched: &mut LaunchTable,
        pins: &mut PinPanel,
        switchboard: &mut Option<u64>,
        pending_open: &mut Option<CommandSection>,
        associations: &mut alloc::vec::Vec<AppAssociation>,
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
                // A press on the backdrop, primary or secondary, means the
                // desktop holds the keyboard: the window that had it learns
                // it lost it. The secondary press additionally opens the
                // backdrop menu, which `route_desktop` has already applied.
                InputResponse::DesktopPressed | InputResponse::DesktopSecondaryPressed => {
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
                    resolve_drop(
                        windows.ipc_id(window).map(DragOrigin::Window),
                        pins,
                        shell,
                        compositor,
                    );
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
                // A pointer motion or key that reached no window belongs to
                // the desktop's icon column, which `route_desktop` has
                // already applied; nothing is forwarded app-ward.
                | InputResponse::DesktopPointerMoved
                | InputResponse::DesktopKey { .. }
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
                // always current. What each installed application opens is
                // read from the same bundles, so it is refreshed here and
                // nowhere else.
                refresh_library(shell, compositor);
                *associations = desktop_associations(shell);
            }
            ShellOutcome::Taskbar(TaskbarResponse::PinDragOffered { entry }) => {
                // Every popup row is a catalogued entry, so the offer names
                // that identity directly — the store records what the
                // catalog vouches for, never a path guessed from a label.
                pins.service
                    .offer_drag(DragOrigin::Library, PinTarget::Entry(entry));
            }
            ShellOutcome::Taskbar(TaskbarResponse::PinDragDropped) => {
                resolve_drop(Some(DragOrigin::Library), pins, shell, compositor);
            }
            ShellOutcome::Taskbar(TaskbarResponse::PinDragWithdrawn) => {
                pins.service.withdraw_drag(DragOrigin::Library);
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
                    let _ = writeln!(Stderr, "desktop: unpin refused: {err}");
                }
            }
            ShellOutcome::Taskbar(TaskbarResponse::PinEntry { entry }) => {
                // Pin a program-library entry from its context menu and
                // persist; a refused edit changes nothing and says why.
                if let Err(err) = pins.service.pin_entry(entry) {
                    let _ = writeln!(Stderr, "desktop: pin refused: {err}");
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
                    section,
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
                // Served application windows are the apps' own pixels, so
                // the session cannot re-colour them: it tells every app
                // instead, and each repaints itself. Without this the
                // desktop would switch and every open window would sit
                // there in the appearance the user just left.
                announce_desktop(
                    server,
                    sink,
                    shell,
                    compositor,
                    windows,
                    picker,
                    &mut pins.service,
                );
            }
            ShellOutcome::Taskbar(TaskbarResponse::LockSession) => {
                // Secure the screen. The prompt goes down first: an
                // unanswered question must not sit behind a lock where the
                // user cannot see what they are agreeing to. A lock that
                // could not be put up says so rather than leaving the user
                // believing the screen is secured.
                confirm.abandon(shell, compositor);
                if !lock.engage(account, shell, compositor) {
                    io::write_stderr_line("desktop: could not lock the screen; it is still open");
                }
            }
            ShellOutcome::Taskbar(TaskbarResponse::LogOut) => {
                // The user asked for the session to end: take the prompt down
                // unanswered (so nothing irreversible follows a log-out) and
                // unwind through the one owner-checked release.
                confirm.abandon(shell, compositor);
                lock.abandon(compositor);
                return Routed::EndSession;
            }
            ShellOutcome::Taskbar(TaskbarResponse::ConfirmSystemPower { action }) => {
                // Never on the strength of the click alone: put the
                // consequence to the user first. A prompt that cannot be
                // shown asks nothing and relays nothing.
                if !confirm.ask(action, shell, compositor) {
                    io::write_stderr_line(
                        "desktop: could not ask for confirmation; nothing was done",
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
            ShellOutcome::Taskbar(TaskbarResponse::LibraryDismissed) => {
                // The popup is the pin drag source, so a drag it had offered
                // dies with it rather than staying armed for a later click.
                pins.service.withdraw_drag(DragOrigin::Library);
            }
            ShellOutcome::Ignored
            | ShellOutcome::Taskbar(
                TaskbarResponse::Ignored
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
            let _ = writeln!(Stderr, "desktop: {reason}");
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
            let name = &pin.label;
            let _ = writeln!(
                Stderr,
                "desktop: pin '{name}' no longer resolves to an application"
            );
            return;
        };
        activate_bundle(
            shell, compositor, server, windows, identity, launched, run_path, &pin.label,
        );
    }

    /// Resolve a drop of the armed app-reference drag: a primary release
    /// from the origin that offered it, landing on the bar's pin band, pins
    /// the offered application at the drop index; anywhere else, the gesture
    /// simply ends (the shared, host-tested `resolve_pin_drop` policy). A
    /// refused admission is reported loudly; the desktop carries on.
    ///
    /// One drop path for both origins — an application window offering one
    /// of its own bundles, and a program dragged out of the library popup —
    /// so the two can never admit a pin on different terms.
    fn resolve_drop(
        origin: Option<DragOrigin>,
        pins: &mut PinPanel,
        shell: &DesktopShell,
        compositor: &Compositor,
    ) {
        let layout = shell.session().taskbar().layout(compositor.scale());
        let decision = tairix_desktop_session::resolve_pin_drop(
            &mut pins.service,
            origin,
            shell.session().taskbar().library().catalog(),
            &layout,
            shell.router().pointer(),
        );
        match decision {
            None | Some(PinDecision::Pinned) => {}
            Some(PinDecision::AlreadyPinned) => {
                io::write_stderr_line("desktop: pin drop: already pinned");
            }
            Some(PinDecision::Full) => {
                io::write_stderr_line("desktop: pin drop: the pin strip is full");
            }
            Some(PinDecision::Refused) => {
                io::write_stderr_line("desktop: pin drop refused");
            }
        }
    }

    /// The file-type associations of every catalogued application: each
    /// entry's bundle manifest, decoded through the shared, fail-closed
    /// [`association_from_appinfo`].
    ///
    /// The program-library catalog is the session's existing record of what
    /// is installed, so the desktop reads no directory of its own to find
    /// out what can open a file. A bundle whose manifest is missing or will
    /// not decode simply claims nothing.
    fn desktop_associations(shell: &DesktopShell) -> alloc::vec::Vec<AppAssociation> {
        shell
            .session()
            .taskbar()
            .library()
            .catalog()
            .entries()
            .filter_map(|entry| {
                let bundle = entry.bundle().as_str();
                let manifest = alloc::format!("{bundle}/AppInfo");
                let bytes = VfsFileReader.read(&manifest).ok()?;
                association_from_appinfo(bundle, &bytes)
            })
            .collect()
    }

    /// Apply one shell outcome to the desktop's icon column and carry out
    /// whatever it asks for.
    ///
    /// The window manager reports a pointer or key event that reached no
    /// window as one of the desktop outcomes, and those drive the column's
    /// hover, selection, keyboard, and activation. Every other outcome means
    /// the gesture went somewhere else; when the pointer is over a window or
    /// the bar that is a departure, which clears the hover and arms the next
    /// arrival's re-listing.
    ///
    /// A refusal — a file no installed application opens — is written to
    /// `stderr` and changes nothing else.
    #[allow(clippy::too_many_arguments)] // The desktop's whole mutable state, threaded explicitly.
    fn route_desktop<S: DirectorySource>(
        outcome: &tairix_desktop_session::ShellOutcome,
        pinboard: &mut PinboardPanel,
        desktop: &mut Desktop<S>,
        shell: &mut DesktopShell,
        compositor: &mut Compositor,
        launched: &mut LaunchTable,
        associations: &mut alloc::vec::Vec<AppAssociation>,
        now_ns: u64,
    ) {
        // The desktop holds the keyboard exactly when no window does, so the
        // focus ring follows the window manager's one notion of focus rather
        // than a second one kept here.
        let mut redraw = desktop.set_focused(shell.router().focused().is_none());
        let pointer = shell.router().pointer();
        let layout = shell.desktop_layout(compositor, desktop);
        let acted = match outcome {
            tairix_desktop_session::ShellOutcome::WindowManager(response) => match response {
                InputResponse::DesktopPointerMoved => {
                    desktop.pointer_moved(pointer, &layout, now_ns)
                }
                InputResponse::DesktopPressed => {
                    desktop.press(pointer, &layout, now_ns, associations)
                }
                InputResponse::DesktopSecondaryPressed => desktop.context_press(pointer, &layout),
                InputResponse::DesktopKey { key, pressed, .. } => {
                    desktop.key(*key, *pressed, &layout, associations)
                }
                _ => departed(desktop, compositor, pointer),
            },
            _ => departed(desktop, compositor, pointer),
        };
        redraw |= acted.redraw;
        redraw |= apply_desktop_action(
            acted.action,
            pinboard,
            desktop,
            shell,
            compositor,
            launched,
            now_ns,
        );
        if acted.relisted {
            // The user's own files demonstrably changed under the desktop, so
            // this is the honest moment to re-read what is installed as well:
            // a program installed since bring-up can open a document from
            // here without waiting for the library popup to be opened. A
            // re-list that found nothing changed costs none of this.
            refresh_library(shell, compositor);
            *associations = desktop_associations(shell);
        }
        if redraw {
            shell.present_desktop(compositor, desktop);
        }
    }

    /// Carry out one desktop action, whether a gesture on the icon column
    /// named it or a chosen backdrop-menu row did, and answer whether the
    /// desktop layer must be repainted.
    ///
    /// The single place every [`DesktopAction`] is honoured, so a
    /// double-click and the equivalent menu row can never disagree about
    /// what happens. Every failure — a refused launch, a folder the
    /// filesystem would not create, settings that could not be saved — is
    /// stated on `stderr` and leaves the desktop running.
    #[allow(clippy::too_many_arguments)] // The desktop's whole mutable state, threaded explicitly.
    fn apply_desktop_action<S: DirectorySource>(
        action: Option<DesktopAction>,
        pinboard: &mut PinboardPanel,
        desktop: &mut Desktop<S>,
        shell: &mut DesktopShell,
        compositor: &mut Compositor,
        launched: &mut LaunchTable,
        now_ns: u64,
    ) -> bool {
        match action {
            Some(DesktopAction::Activate(DesktopActivation::OpenFolder { path })) => {
                record_launch(
                    launched,
                    spawn_app(FILES_RUN_PATH.as_bytes(), &[path.as_bytes()]),
                    FILES_LABEL,
                    FILES_RUN_PATH,
                );
                false
            }
            Some(DesktopAction::Activate(DesktopActivation::Launch {
                run_path,
                label,
                argument,
            })) => {
                let args: alloc::vec::Vec<&[u8]> = argument
                    .iter()
                    .map(alloc::string::String::as_bytes)
                    .collect();
                record_launch(
                    launched,
                    spawn_app(run_path.as_bytes(), &args),
                    &label,
                    &run_path,
                );
                false
            }
            Some(DesktopAction::OpenMenu { at, on_icon }) => {
                pinboard.menu.open(at, on_icon, desktop.settings());
                shell.present_pinboard_menu(compositor, &pinboard.menu);
                false
            }
            Some(DesktopAction::CreateFolder { path }) => {
                create_desktop_folder(&path, desktop, now_ns)
            }
            Some(DesktopAction::AdoptSettings(settings)) => {
                adopt_pinboard_settings(settings, pinboard, desktop, shell, compositor, now_ns)
                    .is_ok()
            }
            Some(DesktopAction::ChangeBackground) => {
                record_launch(
                    launched,
                    spawn_app(WALLPAPER_RUN_PATH.as_bytes(), &[]),
                    WALLPAPER_LABEL,
                    WALLPAPER_RUN_PATH,
                );
                false
            }
            Some(DesktopAction::Refuse(reason)) => {
                let _ = write!(Stderr, "{reason}");
                false
            }
            None => false,
        }
    }

    /// Create `path` under the session's own identity and show it, answering
    /// whether the icon column changed.
    ///
    /// A refusal — the folder already exists, the desktop folder is not
    /// writable, the volume is full — is stated on `stderr` with the
    /// kernel's own reason and leaves the desktop exactly as it was.
    fn create_desktop_folder<S: DirectorySource>(
        path: &str,
        desktop: &mut Desktop<S>,
        now_ns: u64,
    ) -> bool {
        let ret = tairix_rt::fs_mkdir(path.as_bytes());
        if ret < 0 {
            let _ = writeln!(
                Stderr,
                "desktop: {path} could not be created ({})",
                Errno::from_syscall(ret)
            );
            return false;
        }
        desktop.relist(now_ns)
    }

    /// Persist `settings` to the user's own store and, once that write has
    /// succeeded, adopt them and do exactly the work the resulting change
    /// names: re-lay-out, re-list, and re-prepare the wallpaper.
    ///
    /// The write comes first, so memory and disk can never diverge: a
    /// refused write states why on `stderr` and leaves the desktop showing
    /// the settings the next login would restore. Both routes into the
    /// settings — a chosen menu row and an apply from the wallpaper chooser
    /// — come through here, so neither can adopt something the other would
    /// not have.
    ///
    /// # Errors
    ///
    /// The [`PinboardStoreError`] the store refused with; nothing was
    /// adopted.
    fn adopt_pinboard_settings<S: DirectorySource>(
        settings: PinboardSettings,
        pinboard: &mut PinboardPanel,
        desktop: &mut Desktop<S>,
        shell: &mut DesktopShell,
        compositor: &mut Compositor,
        now_ns: u64,
    ) -> Result<(), PinboardStoreError> {
        if let Err(err) = pinboard.store.persist(&mut VfsFileWriter, &settings) {
            let _ = writeln!(Stderr, "desktop: {err}");
            return Err(err);
        }
        let Some(change) = desktop.apply_settings(settings) else {
            return Ok(());
        };
        if change.relist {
            desktop.relist(now_ns);
        }
        if change.wallpaper {
            prepare_wallpaper(pinboard, shell, desktop, compositor);
        }
        // A re-layout, a re-list, and a new wallpaper all show as the same
        // repaint of the desktop layer, so one present covers whichever of
        // them the change asked for.
        Ok(())
    }

    /// The desktop's answer to an outcome that was not its own: a pointer
    /// resting over a window or the bar has left the desktop, and anything
    /// else leaves it exactly as it is.
    ///
    /// Asking the compositor what is under the pointer is total, so window
    /// furniture and the bar's own surfaces count as a departure just like a
    /// window's content does.
    fn departed<S: DirectorySource>(
        desktop: &mut Desktop<S>,
        compositor: &Compositor,
        pointer: tairix_wm::Point,
    ) -> DesktopOutcome {
        if compositor.window_at(pointer).is_some() {
            return desktop.pointer_left();
        }
        DesktopOutcome::ignored()
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
        record_launch(
            launched,
            spawn_app(run_path.as_bytes(), &[]),
            label,
            run_path,
        );
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
            let _ = writeln!(
                Stderr,
                "desktop: library entry {entry} is no longer catalogued"
            );
            return;
        };
        let run_path = alloc::format!("{}/Run", chosen.bundle().as_str());
        let label = chosen.name().as_str();
        record_launch(
            launched,
            spawn_app(run_path.as_bytes(), &[]),
            label,
            &run_path,
        );
    }

    /// (Re)load the program library from its on-disk stores and hand the
    /// resolved catalog to the taskbar's popup, reporting each unusable
    /// store loudly on `stderr`.
    fn refresh_library(shell: &mut DesktopShell, compositor: &mut Compositor) {
        let home = tairix_rt::env_var(b"HOME").and_then(|raw| core::str::from_utf8(raw).ok());
        let loaded = load_library(&mut VfsFileReader, home);
        for warning in &loaded.warnings {
            let _ = write!(Stderr, "{warning}");
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
            read_file(path, tairix_proglib::MAX_CATALOG_LEN)
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

    /// Read the whole file at `path` through the kernel VFS under the
    /// session's own kernel-attested identity, stopping one chunk past `cap`
    /// so no file can make the desktop slurp an arbitrary number of bytes.
    ///
    /// The one read path every document the session reads goes through — the
    /// program-library and pin stores at the catalog cap, the user's
    /// wallpaper at the wallpaper cap — so a second, differently-bounded
    /// reader cannot exist. An answer longer than `cap` is the caller's
    /// whole-document refusal to state.
    fn read_file(path: &str, cap: usize) -> Result<alloc::vec::Vec<u8>, Errno> {
        let ret = tairix_rt::fs_open(path.as_bytes(), OpenFlags::READ);
        if ret < 0 {
            return Err(Errno::from_syscall(ret));
        }
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        // `ret >= 0` checked above; it is a descriptor number.
        let fd = ret as u32;
        let outcome = read_to_end(fd, cap);
        let _ = tairix_rt::fs_close(fd);
        outcome
    }

    /// Read `fd` from the start until end-of-file, stopping one chunk past
    /// `cap` (the caller treats the oversize as the whole-document refusal it
    /// is).
    fn read_to_end(fd: u32, cap: usize) -> Result<alloc::vec::Vec<u8>, Errno> {
        let mut bytes = alloc::vec::Vec::new();
        let mut chunk = [0u8; 1024];
        while bytes.len() <= cap {
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
    fn spawn_app(path: &[u8], args: &[&[u8]]) -> i64 {
        let count = tairix_rt::env_count();
        let mut env: alloc::vec::Vec<&[u8]> = alloc::vec::Vec::with_capacity(count as usize);
        for index in 0..count {
            if let Some(entry) = tairix_rt::env(index) {
                env.push(entry);
            }
        }
        tairix_rt::spawn_with(path, CONSOLE_INHERIT, SPAWN_UID_INHERIT, args, &env)
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
            let _ = writeln!(Stderr, "desktop: {label} launch refused");
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
                    io::write_stderr_line("desktop: picker delegation refused");
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

    /// Deliver a redraw request to the owning app of every window whose
    /// content the compositor gave back (or found missing when the window
    /// was shown again).
    ///
    /// The window manager queues window-manager ids and knows nothing of
    /// the window protocol; the session maps each to its client window
    /// through the one table it already keeps and sends the protocol's
    /// redraw event. A window with no served client — the taskbar, a
    /// session-owned popup — has nothing to ask and is skipped: the
    /// session paints those itself.
    #[allow(clippy::too_many_arguments)] // The serve loop's whole mutable state, threaded explicitly.
    fn deliver_pending_redraws<S: DirectorySource, F: FnMut() -> S>(
        server: &mut WindowServer<RtShmMapper>,
        sink: &mut RtEventSink,
        shell: &mut DesktopShell,
        compositor: &mut Compositor,
        windows: &mut SessionWindows,
        picker: &mut SessionPicker<S, F>,
        pins: &mut dyn PinBridge,
    ) {
        for wm in compositor.pending_redraws() {
            let Some(window_id) = windows.ipc_id(wm) else {
                continue;
            };
            deliver(
                server,
                sink,
                shell,
                compositor,
                windows,
                picker,
                pins,
                &WindowEvent::RedrawRequested { window_id },
            );
        }
    }

    /// Tell every live window that the desktop they share has changed.
    ///
    /// The screen extent, the UI scale, and the active appearance are
    /// properties of the seat, and an application only learns them by
    /// asking or by being told: it holds its own pixels, so nothing the
    /// session does to its own surfaces can bring an app's window into
    /// step. Each window is told through the ordinary delivery path, so a
    /// client that has died is torn down here exactly as it would be for
    /// any other event.
    ///
    /// A desktop the record cannot describe is reported and nothing is
    /// sent: an application keeps the last state it was given rather than
    /// being handed a guess.
    #[allow(clippy::too_many_arguments)] // The delivery path's whole mutable state, threaded explicitly.
    fn announce_desktop<S: DirectorySource, F: FnMut() -> S>(
        server: &mut WindowServer<RtShmMapper>,
        sink: &mut RtEventSink,
        shell: &mut DesktopShell,
        compositor: &mut Compositor,
        windows: &mut SessionWindows,
        picker: &mut SessionPicker<S, F>,
        pins: &mut dyn PinBridge,
    ) {
        let desktop = match desktop_info(compositor) {
            Ok(desktop) => desktop,
            Err(err) => {
                let _ = writeln!(
                    Stderr,
                    "desktop: cannot describe the desktop to apps: {err}"
                );
                return;
            }
        };
        for window_id in server.window_ids() {
            deliver(
                server,
                sink,
                shell,
                compositor,
                windows,
                picker,
                pins,
                &WindowEvent::DesktopChanged { window_id, desktop },
            );
        }
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
            // reclaimed it at exit — and its windows go with it. A merely
            // full mailbox never reaches here: the sink holds that event
            // and answers for it.
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
        match io::Stdout.write_all(&bytes) {
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
            let mut service = ImageRenderService::default();
            return match serve_stdio(&mut service) {
                ServeEnd::Finished => 0,
                ServeEnd::Failed(_) => 1,
            };
        }
        // The command surface next: a malformed (non-UTF-8) argument
        // vector is a usage error, reported rather than guessed at, and
        // the reserved short-help switches never touch the seat.
        let Some(arguments) = tairix_rt::args() else {
            io::write_stderr_line(USAGE);
            return 2;
        };
        match parse(&arguments) {
            Ok(Command::Run) => {}
            Ok(Command::Help) => return short_help(),
            Err(CliError::Usage) => {
                io::write_stderr_line(USAGE);
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
