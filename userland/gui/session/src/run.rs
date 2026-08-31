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
    use tairix_abi::driver::display::DisplayMode;
    use tairix_abi::elevate::{elevate_endpoint, ElevateReply, ElevateRequest};
    use tairix_abi::input::{KeyInput, Modifiers as AbiModifiers};
    use tairix_abi::notify_ipc::{NotifyRequest, NOTIFY_ENDPOINT, NOTIFY_MAX_REQUEST};
    use tairix_abi::pinboard_ipc::{PINBOARD_ENDPOINT, PINBOARD_MAX_REQUEST};
    use tairix_abi::reply::{encode_status_reply, STATUS_REPLY_LEN};
    use tairix_abi::seat::ReleaseSurface;
    use tairix_abi::seat::SEAT_PRIMARY;
    use tairix_abi::session_ipc::{
        session_wake_endpoint, SessionRequest, SessionVerdict, SessionWake, SESSION_ENDPOINT,
        SESSION_MAX_REQUEST, SESSION_VERDICT_LEN, SESSION_WAKE_LEN,
    };
    use tairix_abi::switchboard_ipc::{
        command_endpoint_for, encode_publish_reply, CommandSection, SwitchboardCommand,
        SEAT_REPORT_OWNERS_MAX, SWITCHBOARD_ENDPOINT, SWITCHBOARD_MAX_REQUEST,
        SWITCHBOARD_PUBLISH_REPLY_LEN,
    };
    use tairix_abi::sysinfo::{decode_reply, SYSINFO_ENDPOINT, SYSINFO_REPLY_STATUS_LEN};
    use tairix_abi::window_ipc::{
        MenuOutcome, PointerAction, WindowEvent, WINDOW_ENDPOINT, WINDOW_MAX_REQUEST,
    };
    use tairix_abi::{
        DriverError, Errno, OpenFlags, Origin, ProcId, WaitFlags, WaitSetOp, WaitSourceKind,
        WaitStatus, CONSOLE_INHERIT, ORIGIN_WIRE_LEN, SPAWN_UID_INHERIT, WAIT_PID_ANY,
    };
    use tairix_appdata::RtHost;
    use tairix_browse::{
        association_from_appinfo, AppAssociation, DirectorySource, Entry, GridView, Listing,
    };
    use tairix_caps::CapabilitySet;
    use tairix_desktop_session::menu::{
        open_desktop_menu, ChainAction, ChainOutcome, ChainOwner, MenuChain,
    };
    use tairix_desktop_session::pinboard::{self, PinboardCommand};
    use tairix_desktop_session::switchuser::{
        SeatPresentation, SessionAuthority, SwitchUser, NO_DEADLINE_NS,
    };
    use tairix_desktop_session::windows::window_menu_placement;
    use tairix_desktop_session::{
        admitted_pid, catalogued, chain_geometry, deliver_pending_open, desktop_info,
        drop_is_noteworthy, ensure_switchboard, launch_argv, load_library,
        load_pinboard as read_pinboard_store, maybe_send_seat_report, open_tray, parse,
        persist_pinboard, reap_launched, relay_power, resolve_window_identities,
        serve_pinboard_apply, serve_switchboard_request, window_control_alternate_event,
        window_control_event, Answer, AppBarBridge, AppBarService, ArtworkDesk, ArtworkFileReader,
        ArtworkSandbox, CliError, Command, ConcludedPick, ConfirmPrompt, Delivery, Desktop,
        DesktopAction, DesktopActivation, DesktopOutcome, DesktopShell, DeviceInputSource,
        ElevatePrompt, Elevator, FrameContent, FramePacer, FrameReportGate, FrameStatsPublisher,
        FrameStatsSink, HangTracker, HoldBack, IconRasteriser, InputSource, KeyboardInputSource,
        LaunchTable, ListingClient, ListingDesk, LockedDrain, OwnerWindow, PickConclusion,
        Prepared, PresentedOwners, PromptOutcome, ScreenFade, ScreenLock, SeatEventReader,
        SeatInputChannel, SessionClock, SessionFileReader, SessionPicker, SessionWindows,
        ShellWindowHost, SwitchboardMailbox, SwitchboardOutcome, SwitchboardServe, WallpaperDesk,
        WallpaperSource, BUNDLE_RUN_SUFFIX, CONTENT_RELEASED, CONTENT_RELEASED_MESSAGE,
        DATETIME_RUN_PATH, ELEVATE_PROMPT_SHOWN, ELEVATE_PROMPT_SHOWN_MESSAGE, FILES_LABEL,
        FILES_RUN_PATH, MENU_SHOWN, MENU_SHOWN_MESSAGE, SWITCHBOARD_CALL_REFUSED,
        SWITCHBOARD_LABEL, SWITCHBOARD_RUN_PATH, USAGE, WALLPAPER_LABEL, WALLPAPER_RUN_PATH,
        WINDOW_SHOWN, WINDOW_SHOWN_MESSAGE,
    };
    use tairix_display::{DisplayClient, DisplayTransport, RemoteDisplay, RtShmMapper};
    use tairix_greeter::{Verdict, Verifier};
    use tairix_help::{own_short_help, BundleHelp};
    use tairix_icon::{ArtworkKey, ArtworkResolver, InlineArtwork, Resolved};
    use tairix_keymap::modifiers_to_abi;
    use tairix_log::{
        log, Event as LogEvent, Field as LogField, FieldValue as LogFieldValue, Level as LogLevel,
    };
    use tairix_parallel::Pool;
    use tairix_procinfo::IpcTransport;
    use tairix_rt::io::{self, Stderr, Write};
    use tairix_sandbox::imagerender::{rasterise_icon, render_wallpaper, ImageRenderService};
    use tairix_sandbox::rt::{serve_stdio, worker_role, RtLauncher};
    use tairix_sandbox::{ParserSandbox, ServeEnd};
    use tairix_taskbar::{MenuRequest, MenuSubject, TaskId, TaskbarConfig, TaskbarResponse};
    use tairix_wallpaper::{PinboardSettings, MAX_WALLPAPER_BYTES};
    use tairix_window::{
        event_endpoint_for, CallerIdentity, EventSink, WindowServer, WINDOW_REPLY_MAX,
    };
    use tairix_wm::{
        chrome_cache, frost_cache, Compositor, InputResponse, Point, Rect, Region, Surface,
    };

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

    /// Exit code when a resumed session could not take its screen back: the
    /// seat, the mode, the frame region, or the compositor's adoption of the
    /// new mode refused. Reserved, so the supervisor can tell a desktop that
    /// came back blind from one that was logged out of.
    const EXIT_RESUME_FAILED: i32 = 102;

    /// Exit code when the session authority went away while this session was
    /// parked in the background: nothing can resume it, so it ends cleanly
    /// rather than being stranded invisible.
    const EXIT_AUTHORITY_GONE: i32 = 103;

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

    /// The wait-set token of this session's fast-user-switching wake
    /// mailbox: the session authority telling a background desktop it is the
    /// foreground one again, or that it must end.
    const WAKE_TOKEN: u64 = 9;

    /// The wait-set token of the session's worker wake pipe: a directory read or
    /// a wallpaper preparation the session asked for has finished, so whichever
    /// consumer was waiting can adopt it and repaint.
    const WORKER_TOKEN: u64 = 10;

    /// Bytes drained from the worker wake pipe per wake.
    ///
    /// A worker writes one byte per delivered answer, and there are three
    /// consumers between the two workers, so this is comfortably more than can
    /// ever be outstanding — and the member is a level-triggered peek, so
    /// anything a short drain left behind re-reports on the very next wait rather
    /// than being lost.
    const WORKER_NUDGE_DRAIN: usize = 8;

    /// Queued-wake capacity of the mailbox. The authority sends one wake per
    /// switch and the loop drains it on the very next turn, so a handful of
    /// slots outlasts any legitimate burst and bounds what an unattested
    /// sender can queue before the kernel refuses it.
    const WAKE_CAPACITY: usize = 4;

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

    /// The sink this session records through — every cache's audit trail and
    /// the desktop's one-shot reveal witness. The shared cache constructors
    /// take a `'static` borrow, and the runtime sink is a unit value that
    /// owns nothing.
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
        /// The account name the surface offers is ignored: the broker
        /// re-reads the caller's identity from the kernel and checks the
        /// password against that uid, so naming an account here could only
        /// ever ask for one this process is not.
        fn verify(&mut self, _account: &str, password: &str) -> Verdict {
            match tairix_rt::elevate(&ElevateRequest::Verify { password }) {
                Ok(ElevateReply::Verified) => Verdict::Verified,
                Ok(ElevateReply::Refused(_)) => Verdict::Refused,
                // `Completed` answers a `Run` request and `Launched` a
                // `Launch` one, never a `Verify`. A broker that sent either
                // is not speaking this protocol, and a lock does not open
                // on a reply it did not understand.
                Ok(ElevateReply::Completed { .. } | ElevateReply::Launched { .. }) | Err(_) => {
                    Verdict::Unreachable
                }
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
        now_ns: u64,
        pointer: &mut DeviceInputSource<SeatInputChannel<PointerReader>>,
        keyboard: &mut KeyboardInputSource<SeatInputChannel<KeyboardReader>>,
        unlocker: &mut dyn Verifier,
        shell: &mut DesktopShell,
        compositor: &mut Compositor,
    ) -> Drained {
        // The lock is taking the stream, so the shell will not see the release
        // that ends any gesture in flight: it gives the pointer up rather than
        // holding a grab for a button it can never be told about, and drops the
        // hover anything was drawing behind the plate.
        shell.yield_pointer(compositor);
        let mut drain = LockedDrain::new();
        loop {
            match pointer.poll() {
                Ok(None) => break,
                Ok(Some(event)) => drain.feed(lock, &event, now_ns, unlocker, shell, compositor),
                Err(_) => return Drained::Faulted,
            }
        }
        loop {
            match keyboard.poll_record() {
                Ok(None) => break,
                Ok(Some((event, _))) => {
                    drain.feed(lock, &event, now_ns, unlocker, shell, compositor);
                }
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

    /// Step everything the session animates to `now_ns`, so the frame
    /// presented next carries it: the desktop's screen fade, the locked
    /// screen's own surface, and the backdrop dissolving into another. All of
    /// them are idle once nothing is in flight, which is what leaves an idle
    /// desktop's park indefinite.
    fn animate<S: DirectorySource>(
        fade: &mut ScreenFade,
        lock: &mut ScreenLock,
        clock: &mut SessionClock,
        shell: &mut DesktopShell,
        desktop: &Desktop<S>,
        compositor: &mut Compositor,
        now_ns: u64,
    ) {
        fade.advance(now_ns, compositor);
        lock.advance(now_ns, shell, compositor);
        tick_clock(clock, shell, compositor, now_ns);
        // A backdrop dissolving into another is a repaint of the desktop
        // layer rather than a compositor state change, so each frame of it is
        // the layer drawn again. Only a frame that changed the ground costs
        // one, and only a running fade can have changed it — which is why the
        // reveal witness may be answered from in here: the wallpaper is on
        // screen but still arriving, and a user cannot yet see the desktop
        // they configured.
        if shell.advance_backdrop(now_ns) {
            shell.present_desktop(compositor, desktop);
            fade.set_awaiting_backdrop(!shell.backdrop_settled());
        }
    }

    /// Read the wall clock when the label it produced has gone stale, and put
    /// the new one on the bar.
    ///
    /// This runs on every wake, but the clock owns the cadence: it is read
    /// only once the minute its label was right for has turned, which is the
    /// same deadline the park is shortened to. An idle desktop therefore
    /// reads it once a minute however often something else wakes the loop —
    /// and something else does, roughly every couple of seconds, so reading
    /// unconditionally here would put a syscall on a path that had nothing to
    /// ask about.
    ///
    /// The cost is that a wall clock *stepped* while the desktop is up (an
    /// NTP correction) reaches the bar at the next minute rather than the
    /// next wake. There is no step notification to subscribe to — `wall_time`
    /// is a plain read — and the bar shows whole minutes, so waiting for the
    /// boundary it already wakes on beats polling for a correction that
    /// almost never comes.
    ///
    /// A refused read (a machine with no wall clock wired at all) leaves the
    /// bar exactly as it was rather than blanking it: the label already shown
    /// is the last thing that was true.
    fn tick_clock(
        clock: &mut SessionClock,
        shell: &mut DesktopShell,
        compositor: &mut Compositor,
        now_ns: u64,
    ) {
        if !clock.is_due(now_ns) {
            return;
        }
        let Ok(reading) = tairix_rt::wall_time() else {
            return;
        };
        if clock.adopt(reading, now_ns) {
            shell.set_clock_label(compositor, clock.label());
        }
    }

    /// Dissolve the session's screen to black, presenting every frame, and
    /// return once it is dark.
    ///
    /// The last thing a session draws before it hands the seat on cleared,
    /// so the desktop dims into the black the login screen appears out of
    /// rather than vanishing mid-frame. Bounded by the fade's own span, and
    /// a no-op under a reduced-motion theme, which is dark from its first
    /// frame.
    ///
    /// Paced on the runtime's timed park, not the session wait-set: the
    /// sources this loop is not serving would report ready on every re-park
    /// and spin a core through the whole fade. A refused present stops the
    /// dim where it got to — nothing here can act on it, and the seat is
    /// handed on cleared regardless, so the screen still ends black.
    fn fade_to_black(
        fade: &mut ScreenFade,
        compositor: &mut Compositor,
        display: &mut Option<RemoteDisplay<'_, RtDisplayTransport>>,
    ) {
        let Some(display) = display.as_mut() else {
            return;
        };
        fade.depart(tairix_rt::clock_get(), compositor);
        while compositor.present(display).is_ok() {
            if fade.settled() {
                return;
            }
            tairix_rt::park_ns(fade.park_deadline_ns(tairix_rt::clock_get(), NO_DEADLINE_NS));
            fade.advance(tairix_rt::clock_get(), compositor);
        }
    }

    /// Present the composited damage through the remote display, mapping a
    /// refusal onto the session's exit codes. The service refuses a caller
    /// whose lease is no longer live (`SeatRevoked` from the kernel's
    /// per-request check; a stale owner surfaces as a permission refusal),
    /// so a lost seat is observed here exactly as on a drain. Any refusal
    /// ends the session, so the shell's disposable-UI caches are wiped
    /// before the exit code is returned.
    ///
    /// A background session owns no frame ring, presents nothing, and
    /// answers `Ok`: it has given the screen to somebody else, which is not
    /// a failure.
    ///
    /// A frame that did reach the display is where the desktop's one-shot
    /// reveal witness is announced, so the record can only follow pixels
    /// this session actually put on the screen. Each served window whose
    /// first painted frame this one carried is announced there too, and the
    /// menu chain this one first carried, for the same reason: until the frame
    /// lands, nobody has seen either.
    fn present(
        shell: &mut DesktopShell,
        compositor: &mut Compositor,
        display: &mut Option<RemoteDisplay<'_, RtDisplayTransport>>,
        fade: &mut ScreenFade,
        windows: &mut SessionWindows,
        menu: &mut MenuChain,
    ) -> Result<(), i32> {
        let Some(display) = display.as_mut() else {
            return Ok(());
        };
        match compositor.present(display) {
            Ok(()) => {
                fade.presented(&LOG_SINK);
                windows.report_newly_shown(|window| {
                    log(
                        &LOG_SINK,
                        &LogEvent {
                            level: LogLevel::Info,
                            id: WINDOW_SHOWN,
                            message: WINDOW_SHOWN_MESSAGE,
                            fields: &[LogField {
                                key: "window",
                                value: LogFieldValue::UnsignedInt(window),
                            }],
                        },
                    );
                });
                menu.report_newly_shown(|owner| {
                    let owner = match owner {
                        ChainOwner::Window { .. } => "window",
                        ChainOwner::Backdrop => "backdrop",
                        ChainOwner::Bar(_) => "bar",
                    };
                    log(
                        &LOG_SINK,
                        &LogEvent {
                            level: LogLevel::Info,
                            id: MENU_SHOWN,
                            message: MENU_SHOWN_MESSAGE,
                            fields: &[LogField {
                                key: "owner",
                                value: LogFieldValue::Str(owner),
                            }],
                        },
                    );
                });
                Ok(())
            }
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
    /// Only a Switchboard child this session spawned may call: the
    /// caller's kernel-attested `call_peer_origin` pid must hold a launch
    /// record of this session's own naming the service's bundle path.
    /// Anything else — a foreign process, an orphan of an earlier session,
    /// a copy launched by hand — is a typed refusal, stated on `stderr`
    /// and on the audit trail, and never mutates the model (fail closed).
    /// A malformed frame, and an owner-directed operation naming an owner
    /// this session cannot act on, refuse the same way.
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
            log(
                &LOG_SINK,
                &LogEvent {
                    level: LogLevel::Warn,
                    id: SWITCHBOARD_CALL_REFUSED,
                    message: msg,
                    fields: &[LogField {
                        key: "caller",
                        value: LogFieldValue::UnsignedInt(origin.pid()),
                    }],
                },
            );
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
    /// A refusal worth stating is stated on `stderr` with the kernel's own
    /// reason rather than a guess — `WouldBlock` is back-pressure from a
    /// mailbox the monitor has not drained, while `NotFound` is an instance
    /// that has exited or has not bound its mailbox yet, and calling the
    /// second one "full" would send a reader looking for a problem that is
    /// not there.
    struct RtSwitchboardMailbox;

    impl SwitchboardMailbox for RtSwitchboardMailbox {
        fn send(&mut self, pid: u64, command: SwitchboardCommand) -> bool {
            let ret = tairix_rt::ipc_send(command_endpoint_for(pid), &command.to_le_bytes());
            if ret == 0 {
                return true;
            }
            if drop_is_noteworthy(command) {
                let _ = writeln!(
                    Stderr,
                    "desktop: switchboard command dropped: {}",
                    Errno::from_syscall(ret)
                );
            }
            false
        }
    }

    /// The production [`FrameStatsSink`]: one `ipc_call` to the System
    /// Information API, whose reply is the framed status word every
    /// `sysinfo` answer carries.
    ///
    /// A refused submission is surfaced on `stderr` with the service's own
    /// reason rather than a guess, and dropped: the accounting it carried is
    /// cumulative, so the next attempt states a superset of it and nothing is
    /// lost by not retrying here.
    struct RtFrameStatsSink;

    impl FrameStatsSink for RtFrameStatsSink {
        fn submit(&mut self, request: &[u8]) -> Result<(), Errno> {
            let mut reply = [0u8; SYSINFO_REPLY_STATUS_LEN];
            let outcome = tairix_rt::ipc_call(SYSINFO_ENDPOINT, request, &mut reply)
                .map_err(Errno::from_syscall)
                .and_then(|len| decode_reply(&reply[..len]).map(|_| ()));
            if let Err(err) = outcome {
                let _ = writeln!(Stderr, "desktop: frame accounting not published: {err}");
                return Err(err);
            }
            Ok(())
        }
    }

    /// Start the desktop's Switchboard monitor as this logged-in user and
    /// record it in the launch table like any other desktop child,
    /// answering with the pid of the instance now live.
    ///
    /// An instance already recorded is that instance: one monitor per
    /// session, so a second is never started. The kernel intersects the
    /// monitor's manifest with the user's ceiling, so its view follows the
    /// seat user's authority. A refused spawn answers `None` and leaves
    /// the capsule calm: the desktop runs without its monitor rather than
    /// failing over it.
    fn spawn_switchboard(launched: &mut LaunchTable) -> Option<u64> {
        ensure_switchboard(launched, |launched| {
            record_launch(
                launched,
                spawn_app(SWITCHBOARD_RUN_PATH.as_bytes(), &[]),
                SWITCHBOARD_LABEL,
                SWITCHBOARD_RUN_PATH,
            )
        })
    }

    /// Start the desktop's file manager in its **core** role as this
    /// logged-in user and record it in the launch table like any other
    /// desktop child.
    ///
    /// The file manager is a component of the desktop rather than an
    /// application the user starts: it holds a permanent icon-bar slot from
    /// bring-up, offers the user's places and the mounted volumes on that
    /// slot's menu, and cannot be quit. The shared
    /// [`DESKTOP_ROLE_SWITCH`](tairix_window::DESKTOP_ROLE_SWITCH) is what
    /// says so — the same binary launched without it (from a shell, or by
    /// opening a folder on the desktop) is the ordinary, quittable file
    /// manager, so a second component can never appear.
    ///
    /// It is spawned before anything else so it takes the leading application
    /// slot: the strip keeps the order the session first saw each process in,
    /// which puts the desktop's own component ahead of whatever the user
    /// starts. A refused spawn is reported by the reap like any other and
    /// leaves the desktop running without it.
    fn spawn_files(launched: &mut LaunchTable) {
        let _ = record_launch(
            launched,
            spawn_app(
                FILES_RUN_PATH.as_bytes(),
                &[tairix_window::DESKTOP_ROLE_SWITCH.as_bytes()],
            ),
            FILES_LABEL,
            FILES_RUN_PATH,
        );
    }

    /// Classify the served presents drained since the last report decision:
    /// Switchboard-only content is what must not re-excite a frame report.
    fn frame_content(
        windows: &mut SessionWindows,
        server: &WindowServer<RtShmMapper>,
        identity: &RtWindowIdentity,
        switchboard_pid: Option<u64>,
    ) -> FrameContent {
        let mut owners = PresentedOwners::default();
        for ipc in windows.take_presented() {
            let pid = server
                .owner_of(ipc)
                .and_then(|client| identity.pid_of(client));
            owners.note(pid, switchboard_pid);
        }
        owners.content()
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

    /// Stops the session's worker threads on every way out of [`session`], so
    /// none is left reading a directory or decoding a picture for a desktop that
    /// has ended.
    ///
    /// The handles are *detached* rather than joined: a worker mid-read of a slow
    /// disk would otherwise hold the teardown for as long as that disk takes, and
    /// it has nothing left to write to — each leaves at its next turn round its
    /// loop, and the desk it shares is kept alive by its own handle until then.
    struct WorkerGuard {
        listings: alloc::sync::Arc<Listings>,
        wallpapers: alloc::sync::Arc<Wallpapers>,
        artworks: alloc::sync::Arc<Artworks>,
    }

    impl Drop for WorkerGuard {
        fn drop(&mut self) {
            self.listings.stop();
            self.wallpapers.stop();
            self.artworks.stop();
        }
    }

    /// Spawn one named session worker, stating a refusal once.
    ///
    /// A kernel that will not grant the thread is not a failure: the work moves
    /// back onto the serve loop, which is exactly where it used to be.
    fn spawn_worker(
        what: &str,
        body: impl FnOnce() + Send + 'static,
    ) -> Option<tairix_rt::thread::JoinHandle<()>> {
        match tairix_rt::thread::Thread::spawn(body) {
            Ok(handle) => Some(handle),
            Err(err) => {
                let _ = writeln!(
                    Stderr,
                    "desktop: no {what} thread ({err:?}); that work runs on the serve loop"
                );
                None
            }
        }
    }

    /// The shared frame region the display service scans out of, kept so a
    /// session that steps aside can give it back and a resumed one can be
    /// handed a region shaped for the mode now in force.
    struct FrameRegion {
        base: usize,
        total: usize,
    }

    impl FrameRegion {
        /// One frame's bytes, which is what the desktop's cache budgets are
        /// derived from.
        const fn frame_len(&self) -> usize {
            self.total / FRAME_COUNT as usize
        }

        /// Give the mapping back to the kernel.
        ///
        /// The caller must already have dropped the [`RemoteDisplay`] that
        /// borrowed it, so no ring can name these bytes afterwards.
        fn unmap(self) {
            let _ = tairix_rt::shm_unmap(self.base as u64, self.total);
        }
    }

    /// The taskbar layout for an output of this mode.
    ///
    /// One definition, read by the first bring-up and by a resume onto a
    /// screen the next account re-moded, so the bar cannot come back laid
    /// out differently from how it started.
    fn bar_config(mode: &DisplayMode) -> TaskbarConfig {
        TaskbarConfig::bottom_bar(mode.width_px, mode.height_px)
    }

    /// Create the shared frame region for `mode`, grant it to the display
    /// service, configure the service over it, and answer the ring the
    /// session presents through together with the region to give back.
    ///
    /// The one place frames are established: the first bring-up and every
    /// resume come here, so neither can size, grant, or configure a region
    /// the other would not. A refusal after the region exists unmaps it
    /// before returning, so a failed attempt leaves nothing mapped.
    fn establish_frames(
        mode: &DisplayMode,
    ) -> Result<(RemoteDisplay<'static, RtDisplayTransport>, FrameRegion), (i32, &'static str)>
    {
        let mut client = DisplayClient::new(RtDisplayTransport, SEAT_PRIMARY);
        // The region holds FRAME_COUNT frames back to back, each shaped
        // exactly as the queried mode; the arithmetic is checked so a
        // hostile or corrupt mode can never size a short region.
        let Some(frame_len) = u64::from(mode.stride_bytes)
            .checked_mul(u64::from(mode.height_px))
            .and_then(|bytes| usize::try_from(bytes).ok())
        else {
            return Err((EXIT_BAD_MODE, "frame geometry overflows"));
        };
        let Some(total) = frame_len.checked_mul(FRAME_COUNT as usize) else {
            return Err((EXIT_BAD_MODE, "frame geometry overflows"));
        };
        if frame_len == 0 {
            return Err((EXIT_BAD_MODE, "queried mode is zero-sized"));
        }
        let mut region_id: u64 = 0;
        let base = tairix_rt::shm_create(total, &mut region_id);
        if base < 0 {
            return Err((EXIT_NO_FRAMES, "shared frame region refused"));
        }
        let Ok(base) = usize::try_from(base) else {
            return Err((
                EXIT_NO_FRAMES,
                "frame region base outside the address width",
            ));
        };
        let region = FrameRegion { base, total };
        let grant = tairix_rt::shm_grant(region_id, DISPLAY_ENDPOINT);
        if grant < 1 {
            region.unmap();
            return Err((EXIT_NO_FRAMES, "frame region grant refused"));
        }
        #[allow(clippy::cast_sign_loss)] // `grant >= 1` checked above; it is a kernel handle.
        if client.configure(grant as u64, FRAME_COUNT, mode).is_err() {
            region.unmap();
            return Err((EXIT_NO_DISPLAY, "display service refused the configure"));
        }
        // SAFETY: the kernel mapped exactly `total` zeroed bytes read/write
        // into this process at `base` (`shm_create` maps the length it was
        // asked for), and nothing aliases them. The mapping outlives every
        // use of this slice: the only `shm_unmap` is `FrameRegion::unmap`,
        // whose contract is that the `RemoteDisplay` borrowing these bytes
        // has already been dropped, which is why the borrow may be `'static`
        // here. The display service maps the same frames read-only for its
        // blit, and the protocol serialises access: this session is parked
        // in its present call while the service reads, so the two never race
        // on the presented bytes.
        let frames = unsafe { core::slice::from_raw_parts_mut(base as *mut u8, total) };
        let Ok(display) = RemoteDisplay::new(client, *mode, frames, FRAME_COUNT) else {
            region.unmap();
            return Err((EXIT_BAD_MODE, "queried mode rejected by the frame ring"));
        };
        Ok((display, region))
    }

    /// The step-aside request over the reserved session rendezvous.
    ///
    /// The frame is bodyless and carries no identity: the authority attests
    /// the caller from the kernel and honours the request only from the
    /// session it records as the foreground one.
    struct RtSessionAuthority;

    impl SessionAuthority for RtSessionAuthority {
        fn request_background(&mut self) -> Result<SessionVerdict, Errno> {
            let mut request = [0u8; SESSION_MAX_REQUEST];
            let len = SessionRequest::Background.encode(&mut request)?;
            let mut reply = [0u8; SESSION_VERDICT_LEN];
            let got = tairix_rt::ipc_call(SESSION_ENDPOINT, &request[..len], &mut reply)
                .map_err(Errno::from_syscall)?;
            SessionVerdict::decode(&reply[..got])
        }
    }

    /// The session's ownership of the screen, as the switch drives it: the
    /// frame ring and its region, the compositor and the surfaces laid out
    /// over the mode, the screen fade the hand-over is dressed with, and the
    /// wait-set the seat member belongs to.
    struct SessionScreen<'a, S: DirectorySource> {
        display: &'a mut Option<RemoteDisplay<'static, RtDisplayTransport>>,
        region: &'a mut Option<FrameRegion>,
        compositor: &'a mut Compositor,
        shell: &'a mut DesktopShell,
        desktop: &'a Desktop<S>,
        pinboard: &'a mut PinboardPanel,
        wallpapers: &'a Wallpapers,
        fade: &'a mut ScreenFade,
        set: u64,
    }

    impl<S: DirectorySource> SeatPresentation for SessionScreen<'_, S> {
        fn fade_out(&mut self) {
            fade_to_black(self.fade, self.compositor, self.display);
        }

        fn fade_in(&mut self) {
            self.fade.arrive(tairix_rt::clock_get(), self.compositor);
        }

        fn suspend(&mut self) {
            // The ring goes before the region it borrows, and the seat's
            // wait-set member goes with them: a parked session must not be
            // woken by the next account's typing, and ignoring such a wake
            // instead would spin on a member that stays ready.
            *self.display = None;
            if let Some(region) = self.region.take() {
                region.unmap();
            }
            let _ = tairix_rt::waitset_ctl(
                self.set,
                WaitSetOp::Del,
                WaitSourceKind::SeatInput,
                SEAT_PRIMARY,
                SEAT_TOKEN,
            );
        }

        fn release_seat(&mut self) {
            // The login screen is what comes up next, so the seat is handed
            // on cleared: this account's last frame must not linger on the
            // screen for the next person, and no text console belongs in the
            // gap either.
            let _ = tairix_rt::display_release(SEAT_PRIMARY, ReleaseSurface::Handover);
        }

        fn acquire_seat(&mut self) -> Result<(), Errno> {
            let taken = tairix_rt::display_acquire(SEAT_PRIMARY);
            if taken < 1 {
                return Err(Errno::from_syscall(taken));
            }
            if tairix_rt::waitset_ctl(
                self.set,
                WaitSetOp::Add,
                WaitSourceKind::SeatInput,
                SEAT_PRIMARY,
                SEAT_TOKEN,
            ) != 0
            {
                let _ = tairix_rt::display_release(SEAT_PRIMARY, ReleaseSurface::Text);
                return Err(Errno::SeatRevoked);
            }
            Ok(())
        }

        fn query_mode(&mut self) -> Result<DisplayMode, Errno> {
            DisplayClient::new(RtDisplayTransport, SEAT_PRIMARY).query()
        }

        fn reconfigure(&mut self, mode: DisplayMode) -> Result<(), Errno> {
            let (ring, region) = establish_frames(&mode).map_err(|_| Errno::DeviceFault)?;
            *self.display = Some(ring);
            *self.region = Some(region);
            // The compositor adopts the mode before anything is laid out
            // against it: the bar and the icons are placed on the extent it
            // reports. A mode it cannot take leaves it untouched, and the
            // session ends rather than showing a screen it cannot draw.
            if !self.compositor.set_mode(mode) {
                return Err(Errno::NotSupported);
            }
            self.shell
                .set_output_layout(bar_config(&mode), self.compositor);
            prepare_wallpaper(
                self.pinboard,
                self.wallpapers,
                self.shell,
                self.desktop,
                self.compositor,
                tairix_rt::clock_get(),
            );
            self.shell.present_desktop(self.compositor, self.desktop);
            Ok(())
        }

        fn repaint_all(&mut self, _mode: DisplayMode) -> Result<(), Errno> {
            let Some(display) = self.display.as_mut() else {
                return Err(Errno::NotConnected);
            };
            self.compositor
                .present(display)
                .map_err(|_| Errno::DeviceFault)
        }
    }

    /// Bring the desktop up and run it until the seat is lost or a fault
    /// ends it. Split from `main` so every exit path after the acquire
    /// flows back through the one owner-checked `display_release`.
    #[allow(clippy::too_many_lines)] // One linear bring-up + serve loop; splitting it would scatter the lease lifecycle.
    fn session() -> i32 {
        // The session's own kernel-attested identity, read before anything
        // else: the window engine stamps it into every create reply, the
        // wake mailbox below is addressed by its pid, and a session that
        // cannot learn who it is must not serve windows apps cannot
        // authenticate (fail closed).
        let Ok(self_origin) = tairix_rt::self_origin() else {
            return fail(EXIT_NO_WINDOW_ENDPOINT, "session identity unavailable");
        };
        // Bind the fast-user-switching wake mailbox before the first frame,
        // so a session is resumable from the moment it can be switched away
        // from. The id is derived from this session's own pid and is
        // unreserved, so anyone may send to it — every message is attested
        // against the authority when it is drained. A refused bind is not
        // fatal: the desktop runs as a session that simply cannot be
        // switched away from, and says so by leaving the row out.
        let wake = session_wake_endpoint(self_origin.pid());
        let bound = !tairix_abi::ipc::is_reserved_endpoint(wake)
            && tairix_rt::port_bind(wake, SESSION_WAKE_LEN, WAKE_CAPACITY) == 0;
        if !bound {
            io::write_stderr_line(
                "desktop: session wake mailbox refused; this session cannot switch user",
            );
        }
        let mut switch = SwitchUser::new(bound.then_some(wake), self_origin.console());

        // --- Display bring-up: query → shared frames → grant → configure.
        let Ok(mode) = DisplayClient::new(RtDisplayTransport, SEAT_PRIMARY).query() else {
            return fail(
                EXIT_NO_DISPLAY,
                "display service unreachable or refused the mode query",
            );
        };
        let (display, region) = match establish_frames(&mode) {
            Ok(established) => established,
            Err((code, reason)) => return fail(code, reason),
        };
        let frame_len = region.frame_len();
        // Held as options so a resume can drop the ring, give the region
        // back, and build both again for the mode then in force — there is
        // no second bring-up path.
        let mut display = Some(display);
        let mut region = Some(region);

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
            bar_config(&mode),
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
        // The decorated windows' furniture and the backdrop-blurred windows'
        // frosted backdrops are the output's own caches, so they are built
        // here from the same seat, output size, gauge, and sink and handed to
        // the compositor that draws from them. The compositor takes the gauge
        // itself as well: a window's *content* is not a keyed cache but a
        // release policy over the same band, so it reads the pressure directly
        // rather than through a cache.
        //
        // They are this process's memory like the shell's three caches, so
        // they join them in the report before the compositor takes them: a
        // ledger is a shared handle to the figures, not the cache itself.
        let chrome = chrome_cache(
            SEAT_PRIMARY,
            frame_len,
            tairix_rt::pressure::gauge(),
            &LOG_SINK,
        );
        let frost = frost_cache(
            SEAT_PRIMARY,
            frame_len,
            tairix_rt::pressure::gauge(),
            &LOG_SINK,
        );
        for ledger in [chrome.ledger(), frost.ledger()].into_iter().flatten() {
            tairix_rt::cachereport::register(ledger);
        }
        let Some(mut compositor) = Compositor::new(
            mode,
            shell.desktop_background(),
            chrome,
            frost,
            tairix_rt::pressure::gauge(),
        ) else {
            return fail(EXIT_BAD_MODE, "compositor rejected the queried mode");
        };
        compositor.set_job_runner(composite_pool());
        let screen = Rect::new(0, 0, mode.width_px, mode.height_px);
        let Ok(mut pointer) = DeviceInputSource::new(SeatInputChannel::new(PointerReader), screen)
        else {
            return fail(EXIT_BAD_MODE, "queried mode has no pointer surface");
        };
        let mut keyboard = KeyboardInputSource::new(SeatInputChannel::new(KeyboardReader));

        // The serve loop's own parser-sandbox worker: this binary re-entered as
        // a capability-empty child, where untrusted images are decoded rather
        // than in this address space. The wallpaper and artwork threads own one
        // each of their own, so no sandbox handle crosses a thread; this one is
        // what both fall back to when the kernel grants no thread to hold them.
        let sandbox: SharedSandbox = alloc::rc::Rc::new(core::cell::RefCell::new(
            ParserSandbox::new(RtLauncher::own_binary(), tairix_rt::LogSink),
        ));

        // The session's workers, and the one pipe they all nudge the serve loop
        // through: a directory read, an icon decode, and a wallpaper
        // preparation are each a disk and a sandbox away, so each costs a
        // repaint's delay rather than a frozen desktop. A pipe the kernel
        // refuses, or a thread it will not grant, leaves that work on the serve
        // loop's own task: slower under load, never wrong, and stated once.
        let (worker_wake_read, worker_wake) = if let Ok((read, write)) = tairix_rt::pipe_create() {
            (Some(read), WorkerWake { fd: Some(write) })
        } else {
            io::write_stderr_line(
                "desktop: no worker wake pipe; directory listings, icon artwork, and the \
                 wallpaper are prepared on the serve loop",
            );
            (None, WorkerWake { fd: None })
        };
        let worker_wake = alloc::sync::Arc::new(worker_wake);
        let listings = alloc::sync::Arc::new(Listings::new(alloc::sync::Arc::clone(&worker_wake)));
        let wallpapers =
            alloc::sync::Arc::new(Wallpapers::new(alloc::sync::Arc::clone(&worker_wake)));
        let artworks = alloc::sync::Arc::new(Artworks::new(alloc::sync::Arc::clone(&worker_wake)));
        // One worker per kind of work, spawned only where there is a wake to
        // deliver through. Each handle is held for the session's life; the
        // worker's own `Arc` keeps its desk alive either way.
        let (listing_worker, wallpaper_worker, artwork_worker) =
            worker_wake_read.map_or((None, None, None), |_| {
                let listing = {
                    let served = alloc::sync::Arc::clone(&listings);
                    spawn_worker("listing", move || served.serve())
                };
                let wallpaper = {
                    let served = alloc::sync::Arc::clone(&wallpapers);
                    spawn_worker("wallpaper", move || served.serve())
                };
                let artwork = {
                    let served = alloc::sync::Arc::clone(&artworks);
                    spawn_worker("icon", move || served.serve())
                };
                (listing, wallpaper, artwork)
            });
        // With no worker there is nobody to answer a recorded request, so the
        // desk is stopped and that work happens on this task instead.
        if listing_worker.is_none() {
            listings.stop();
        }
        if wallpaper_worker.is_none() {
            wallpapers.stop();
        }
        if artwork_worker.is_none() {
            artworks.stop();
        }
        // Every way out of this function stops every worker. The guard is
        // declared after the handles, so it runs first: the desks stop, then the
        // handles detach.
        let _worker_guard = WorkerGuard {
            listings: alloc::sync::Arc::clone(&listings),
            wallpapers: alloc::sync::Arc::clone(&wallpapers),
            artworks: alloc::sync::Arc::clone(&artworks),
        };

        // The desktop's icon artwork — the shipped `/System/Graphics` masters
        // and each bundle's own icon — is read through the session's own VFS
        // identity and decoded in a sandbox worker. With a decoder thread that
        // happens off this task and a paint that misses draws the built-in
        // glyph until the pixels land; without one the read and the round trip
        // happen here, exactly as they used to. Until this call the shell draws
        // every icon from its built-in glyphs, and it falls back to them again
        // whenever either seam refuses.
        if artwork_worker.is_some() {
            shell.set_artwork_resolver(alloc::boxed::Box::new(DeferredArtwork(
                alloc::sync::Arc::clone(&artworks),
            )));
        } else {
            shell.set_artwork_resolver(alloc::boxed::Box::new(InlineArtwork::new(
                ArtworkFileReader(VfsFileReader),
                ArtworkSandbox(SandboxRasteriser {
                    sandbox: alloc::rc::Rc::clone(&sandbox),
                }),
            )));
        }

        // The program library: read the machine store and the logged-in
        // user's overlay under the session's own identity, merge them, and
        // hand the resolved catalog to the taskbar's popup. A store that
        // cannot be used is reported loudly and contributes an empty
        // catalog, so the desktop comes up with a calm empty library rather
        // than dying over a settings file.
        refresh_library(&mut shell, &mut compositor);

        // The icon bar's application strip: one slot per running
        // application, resolved from the bundle the kernel attested owns
        // each process. Nothing is loaded from disk — the strip is derived
        // from live state, never stored — so it starts empty and fills as
        // applications declare a presence or open a window.
        let mut apps = AppBarPanel::new();

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
            AsyncDirectorySource {
                listings: alloc::sync::Arc::clone(&listings),
                client: ListingClient::Pinboard,
            },
            desktop_folder,
        );
        // The user's pinboard settings, with the same fail-closed posture as
        // the program library: absent → the defaults, silently (a fresh
        // account); unusable → the defaults plus one loud reason. Applied
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
        let backdrop_ready = prepare_wallpaper(
            &mut pinboard,
            &wallpapers,
            &mut shell,
            &desktop,
            &compositor,
            tairix_rt::clock_get(),
        );

        // The session-side served-window table. Declared before the first
        // present because every present reports which served windows that
        // frame has just put on screen, and bring-up presents before any
        // application can have opened one.
        let mut windows = SessionWindows::new();
        // The seat's one menu chain. Every menu on the desktop — an
        // application's and the desktop's own — is this one service's.
        let mut menu = MenuChain::new();
        // What a chosen row of one of the **icon bar's** own chains resolved
        // to. The bar's menus answer with the very same typed responses a
        // click on the bar produces, so they are routed where those are
        // rather than acted on a second way; the seat branch drains this in
        // the wake the row was chosen in.
        let mut answered: Vec<tairix_desktop_session::ShellOutcome> = Vec::new();
        // First frame: place the bar, paint the desktop's icons beneath
        // every window, install the pointer cursor at the seat's initial
        // pointer position, and push the whole surface once;
        // every later present carries only the composited damage. The cursor
        // is then kept live by the shell as each seat event is pumped.
        shell.present(&mut compositor);
        shell.present_desktop(&mut compositor, &desktop);
        shell.refresh_cursor(&mut compositor);
        // The login screen faded to black before it exited, so the desktop
        // comes up over a dark screen and reveals itself rather than
        // snapping on. Begun here, with the first frame composed and about
        // to be shown: begun any earlier, the fade would spend itself on
        // bring-up with nothing on screen yet.
        let mut fade = ScreenFade::begin(tairix_rt::clock_get(), &mut compositor);
        // The reveal witness says a user can see the desktop, so it waits for the
        // backdrop they chose rather than announcing a frame that carries the
        // fallback colour in its place — and, once that backdrop lands, for it to
        // finish dissolving in over that colour.
        fade.set_awaiting_backdrop(!backdrop_ready || !shell.backdrop_settled());
        if let Err(code) = present(
            &mut shell,
            &mut compositor,
            &mut display,
            &mut fade,
            &mut windows,
            &mut menu,
        ) {
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
        // The workers' wake: readable exactly when a directory read or a
        // wallpaper preparation has finished. A refused add is fatal rather than
        // tolerated — a worker whose answers nobody collects would leave the
        // desktop listing forever, and the session must not park on a set that
        // cannot report it.
        if let Some(read) = worker_wake_read {
            if tairix_rt::waitset_ctl(
                set,
                WaitSetOp::Add,
                WaitSourceKind::Stream,
                u64::from(read),
                WORKER_TOKEN,
            ) != 0
            {
                return fail(EXIT_WAIT_FAILED, "listing wake wait refused");
            }
        }
        // `watch` re-reads the band as it registers the member, closing the
        // race between the bring-up read above and this registration — a
        // move in between would otherwise never be seen.
        if !tairix_procinfo::pressure::watch(set, PRESSURE_TOKEN) {
            return fail(EXIT_WAIT_FAILED, "memory-pressure wait refused");
        }
        // The wake mailbox joins for the session's whole life, foreground or
        // background: it is the only member a switched-away desktop waits
        // on, so a bind that succeeded and a member that did not join would
        // park it forever. A session that never bound has no member, and
        // offers no switch.
        if let Some(wake) = switch.wake_endpoint() {
            if tairix_rt::waitset_ctl(set, WaitSetOp::Add, WaitSourceKind::Port, wake, WAKE_TOKEN)
                != 0
            {
                return fail(EXIT_WAIT_FAILED, "session wake mailbox wait refused");
            }
        }

        // The window channel's server state: the engine, the kernel-attested
        // caller identity, the app-ward event sink, and the focused served
        // window the routing mirrors (the window table itself is older than
        // this block — every present announces what it put on screen, so it
        // exists before the first frame). The engine stamps this session's
        // own kernel-attested identity into every create reply, so apps can
        // authenticate the sender of each later event.
        // What one client may hold mapped here, from the machine's own RAM
        // total and one frame of this session's output — never a window count,
        // which says nothing about the bytes a window actually costs.
        let mut server = WindowServer::new(
            RtShmMapper,
            self_origin.proc_id(),
            tairix_window::client_frame_budget_bytes(
                tairix_procinfo::memory_total_bytes(&tairix_procinfo::IpcTransport).unwrap_or(0),
                frame_len,
            ),
        );
        let mut identity = RtWindowIdentity::new();
        let mut sink = RtEventSink::new(set);
        let mut focused: Option<u64> = None;
        // Every app launched from the desktop is admitted immediately and
        // loads on its own task (asynchronous launch); a load refusal now
        // surfaces as the child's reserved-`LOAD_*` exit status, not the
        // `spawn` return. This table remembers each running child's label
        // (so the `CHILD_TOKEN` reap below can name the app in the
        // fail-loud diagnosis) and its spawn path (the attested bundle
        // identity a single-instance rule resolves against). An entry is
        // removed when its child is reaped, so it never grows beyond the
        // apps currently alive.
        let mut launched = LaunchTable::new();
        // Start the desktop's Switchboard monitor. It is recorded in the
        // launch table like any desktop child — the reap arm names a load
        // refusal, and the serve arm attests its calls against this entry
        // — and the very same bring-up serves a later tray press that
        // finds no instance live.
        let mut switchboard_pid = spawn_switchboard(&mut launched);
        // Start the desktop's file manager in its core role. It is a
        // component of the desktop, not an application the user starts, so it
        // comes up with the session and holds its icon-bar slot from here on.
        spawn_files(&mut launched);
        // A tray press with no live monitor to receive it: the section the
        // bar asked to open on, held until that instance's first publish
        // proves it is up. One pending open, replaced by a later press
        // rather than queued, so a user pressing repeatedly opens the
        // section they last asked for and no more.
        let mut pending_open: Option<CommandSection> = None;
        // What the monitor's Resources page already shows about the last
        // frame, so a frame whose cost is unchanged sends nothing — and, past
        // that, so a desktop whose cost moves on *every* frame (a pointer
        // crossing the wallpaper redamages the cursor) still reports at a
        // rate a reader can follow rather than at frame rate.
        let mut frames = FrameReportGate::new();
        let mut frame_stats = FrameStatsPublisher::new();
        // The frame deadline. Wakes arrive as fast as a hand can move a
        // mouse, which is several times faster than any screen shows a
        // frame, so damage accumulates in the compositor between deadlines
        // and is composited once when one arrives. A held frame shortens the
        // park below to the moment it comes due and nothing else; a desktop
        // with nothing held arms nothing.
        let mut pacer = FramePacer::new();
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
        let picker_listings = alloc::sync::Arc::clone(&listings);
        let mut picker = SessionPicker::new(move || AsyncDirectorySource {
            listings: alloc::sync::Arc::clone(&picker_listings),
            client: ListingClient::Picker,
        })
        .starting_at(picker_start);
        // The trusted confirmation prompt for a power transition. It is the
        // session's own window, so the question the user answers is asked by
        // the desktop itself rather than by the bar, which holds no
        // authority; an unanswered prompt relays nothing.
        let mut confirm = ConfirmPrompt::new();
        // The trusted credential prompt for a command this session may not
        // perform — setting the clock. It too is the session's own window, so
        // a password is typed into desktop chrome and offered to the
        // console's broker, never to an application; an unanswered prompt
        // offers nothing.
        let mut elevate = ElevatePrompt::new();
        // The screen lock, and the account it re-verifies. `USER` is what
        // login exported for this session; it names whose password the
        // prompt is asking for and nothing more — the broker reads the
        // identity it actually checks against from the kernel, so a wrong
        // or missing name here cannot unlock anybody's session. An unset or
        // malformed value simply leaves the prompt unnamed.
        let mut lock = ScreenLock::new();
        // The taskbar clock. It is read here so the bar carries the time from
        // the first frame rather than blank until the minute turns, and its
        // tick is folded into the park below — one wake a minute, the fewest a
        // minute-granular clock can be right with.
        let mut clock = SessionClock::new();
        let account = tairix_rt::env_var(b"USER")
            .and_then(|raw| core::str::from_utf8(raw).ok())
            .unwrap_or_default();
        // Offer the rows that need re-authentication only where this session
        // really has a broker for it: the Lock row (which would otherwise
        // strand the user behind a screen with no way back) and the clock's
        // set-time row (which needs an account holding CAP_TIME_SET
        // authenticated). One console fact, read once.
        shell.set_elevation_available(
            &mut compositor,
            elevate_endpoint(self_origin.console()).is_ok(),
        );
        // Offer the Switch User row only where this session really could be
        // resumed: the wake mailbox bound. Without it the row is absent
        // rather than refused — there is no authority to explain.
        shell.set_switch_user_available(&mut compositor, switch.is_available());
        // The clock's first reading, so the bar carries the time from the
        // frame the user first sees rather than from the next minute.
        tick_clock(
            &mut clock,
            &mut shell,
            &mut compositor,
            tairix_rt::clock_get(),
        );

        let mut token = 0u64;
        // Held for the life of the loop rather than taken per request: it is
        // sized to the widest operation the channel has, so a per-request
        // array would cost every present — the hottest and one of the
        // shortest — the whole of the widest one's clearing.
        let mut request = [0u8; WINDOW_MAX_REQUEST];
        loop {
            // The park stays indefinite: a cache-report change the runtime's
            // rate limiter is holding back, a frame report this session's own
            // one is holding back, a composited frame the pacer is holding
            // for its deadline, an animation frame the session owes, a bar
            // gesture the clock owes an answer to, and a window thumbnail a
            // hover picker is waiting on, only ever *tighten* the wait to the
            // moment the work is due, and fold back to indefinite once it is
            // done. The desktop never polls for anything.
            //
            // A background session has no deadline at all, not even those:
            // it draws nothing, so a held-back report has nothing to report,
            // no held frame can reach a screen it does not own, nothing it
            // animates is on screen, and a timer would wake a core for no
            // work.
            let timeout_ns = {
                let now_ns = tairix_rt::clock_get();
                // A thumbnail slice is owed *now*: the wait still reports a
                // ready member first, so slicing never starves input.
                let owed = if shell.window_thumbnails_owed() {
                    0
                } else {
                    u64::MAX
                };
                switch.park_deadline_ns(lock.park_deadline_ns(
                    now_ns,
                    clock.park_deadline_ns(
                        now_ns,
                        fade.park_deadline_ns(
                            now_ns,
                            shell.backdrop_park_deadline_ns(
                                now_ns,
                                shell.taskbar_park_deadline_ns(
                                    now_ns,
                                    frames.park_deadline_ns(
                                        now_ns,
                                        frame_stats.park_deadline_ns(
                                            now_ns,
                                            pacer.park_deadline_ns(
                                                now_ns,
                                                tairix_rt::cachereport::fold_wait_deadline_ns(owed),
                                            ),
                                        ),
                                    ),
                                ),
                            ),
                        ),
                    ),
                ))
            };
            let waited = tairix_rt::waitset_wait(set, timeout_ns, &mut token);
            if waited != 0 {
                if Errno::from_syscall(waited) != Errno::TimedOut {
                    // A dead wait-set would degrade the loop into a busy poll;
                    // exit fail-loud instead and let the supervisor decide.
                    return fail(EXIT_WAIT_FAILED, "seat wait failed");
                }
                // No member woke, so `token` still names the *previous*
                // wake's source and dispatching on it would block in a
                // `call_recv` with nothing to receive. Only what this loop
                // armed the deadline for is owed: the next frame of whatever
                // is animating, the bar gesture the clock owes an answer to,
                // the next window thumbnail a hover picker is waiting on, and
                // the held-back report.
                //
                // One clock reading serves the whole frame: what the
                // animation steps to and what the report's rate limit is
                // timed against are the same instant, and the frame path
                // takes one syscall for both rather than two.
                let now_ns = tairix_rt::clock_get();
                // The pointer resting still produces no events at all, so
                // this is what opens a picker whose dwell has elapsed and
                // takes down one whose grace has.
                shell.tick_taskbar(&mut compositor, now_ns);
                shell.advance_window_thumbnails(&mut compositor);
                animate(
                    &mut fade,
                    &mut lock,
                    &mut clock,
                    &mut shell,
                    &desktop,
                    &mut compositor,
                    now_ns,
                );
                if pacer.admit(now_ns, compositor.has_damage()) {
                    if let Err(code) = present(
                        &mut shell,
                        &mut compositor,
                        &mut display,
                        &mut fade,
                        &mut windows,
                        &mut menu,
                    ) {
                        return code;
                    }
                }
                frames.maybe_send(
                    &compositor,
                    switchboard_pid,
                    frame_content(&mut windows, &server, &identity, switchboard_pid),
                    now_ns,
                    &mut RtSwitchboardMailbox,
                );
                frame_stats.maybe_publish(&compositor, now_ns, &mut RtFrameStatsSink);
                tairix_rt::cachereport::publish_if_due();
                continue;
            }
            // Any wake but a worker's nudge is the desktop *acting*, so the
            // icon decoder opens a fresh round: what is on screen may have
            // changed, and every key it answered for the last round may be
            // asked for again. A nudge does not, which is what stops a decode
            // the cache declines to retain being asked for by the very repaint
            // its landing drove — the desktop would otherwise repaint itself
            // for ever over a cache it cannot fill.
            if token != WORKER_TOKEN {
                artworks.begin_round();
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
                let mut ticket = 0u64;
                if let Ok(len) = tairix_rt::call_recv(WINDOW_ENDPOINT, &mut request, &mut ticket) {
                    let mut reply = [0u8; WINDOW_REPLY_MAX];
                    // Read before the bridge borrows the picker: a menu may
                    // not be drawn over a lock screen or the trusted picker,
                    // and an accepted open is answered `SeatBusy` instead.
                    let seat_held = seat_held(&lock, &picker);
                    let n = {
                        let mut bridge = ShellWindowHost {
                            shell: &mut shell,
                            compositor: &mut compositor,
                            windows: &mut windows,
                            picker: &mut picker,
                            apps: &mut apps.service,
                            menu: &mut menu,
                            seat_held,
                        };
                        server.serve(
                            &mut bridge,
                            &mut identity,
                            ticket,
                            &request[..len],
                            &mut reply,
                        )
                    };
                    // A window opened by this pass wears the icon of the
                    // application the kernel says opened it. It runs here,
                    // not in the bridge, because the attested-caller table
                    // and the launch records are both borrowed while a
                    // request is served.
                    resolve_window_identities(
                        &mut shell,
                        &mut compositor,
                        &mut windows,
                        &launched,
                        |owner| identity.pid_of(owner),
                    );
                    let _ = tairix_rt::call_reply(WINDOW_ENDPOINT, ticket, &reply[..n]);
                    // A chain this pass brought up has to reach the screen,
                    // and one it displaced has to be answered. Both run here
                    // rather than in the bridge, for the reason the identity
                    // pass above does: the engine holds the borrow the
                    // delivery needs while a request is being served.
                    // A chain displaced here is dismissed, never chosen — a
                    // row is chosen only where a seat event reaches the chain,
                    // which is the seat's own drain — so nothing lands in the
                    // bar's answer sink for this branch to route.
                    answer_menu_chain(
                        &mut menu,
                        &mut shell,
                        &mut compositor,
                        &mut windows,
                        &mut server,
                        &mut sink,
                        &mut picker,
                        &mut apps.service,
                        &mut DesktopMenuDesk {
                            pinboard: &mut pinboard,
                            wallpapers: &wallpapers,
                            desktop: &mut desktop,
                            launched: &mut launched,
                            associations: &mut associations,
                            answered: &mut answered,
                        },
                        tairix_rt::clock_get(),
                    );
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
                                    let _ = record_launch(
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
                    // draining, and names which one: the session tracks
                    // the attested publisher from here, so a press that
                    // arrived before it was up has an instance to open on
                    // now and every later command goes to the instance
                    // that answered rather than to a guess.
                    if let Ok(SwitchboardOutcome::Published { publisher, .. }) = result {
                        switchboard_pid = Some(publisher);
                        deliver_pending_open(
                            &mut pending_open,
                            publisher,
                            &mut RtSwitchboardMailbox,
                        );
                    }
                    // A successful publish answers with this session's own
                    // kernel-attested identity, so the monitor can
                    // authenticate the commands the session later sends
                    // it; every other outcome, refusals included, answers
                    // with the shared status frame.
                    let mut reply = [0u8; SWITCHBOARD_PUBLISH_REPLY_LEN];
                    let len = if let Ok(SwitchboardOutcome::Published { session, .. }) = result {
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
                        &wallpapers,
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
                            apps: &mut apps.service,
                            menu: &mut menu,
                            seat_held: true,
                        };
                        server.client_exited(&mut bridge, client);
                        if focused.is_some_and(|id| server.owner_of(id).is_none()) {
                            focused = None;
                        }
                    }
                }
            } else if token == WORKER_TOKEN {
                // A worker finished something. Drain the nudge bytes (the member
                // is a level-triggered peek, so anything left re-reports on the
                // next wait and nothing is lost), then offer every consumer the
                // chance to adopt what arrived. Each is a no-op unless it was the
                // one waiting, so one wake serves whichever it was.
                let mut nudge = [0u8; WORKER_NUDGE_DRAIN];
                if let Some(read) = worker_wake_read {
                    let _ = tairix_rt::fs_read(read, 0, &mut nudge);
                }
                let relisted = desktop.relist(tairix_rt::clock_get());
                let papered = prepare_wallpaper(
                    &mut pinboard,
                    &wallpapers,
                    &mut shell,
                    &desktop,
                    &compositor,
                    tairix_rt::clock_get(),
                );
                if papered {
                    fade.set_awaiting_backdrop(!shell.backdrop_settled());
                }
                // Icon artwork that landed is drawn by asking for it again:
                // every icon surface resolves through the one cache the
                // decoder's answers go into, so a repaint is the whole of
                // adopting them. A window's title-bar and taskbar identity are
                // the exception — those *store* the picture, so the windows
                // still waiting for one are offered it here.
                let arted = artworks.take_landed();
                if arted {
                    // A slot's picture and a window's identity are *stored*
                    // on the model rather than resolved as the surface
                    // paints, so both are offered the artwork again before
                    // the present; every other icon surface simply asks the
                    // cache once more.
                    refresh_app_strip(
                        &mut apps,
                        &mut shell,
                        &mut compositor,
                        &server,
                        &windows,
                        &identity,
                        &launched,
                    );
                    resolve_window_identities(
                        &mut shell,
                        &mut compositor,
                        &mut windows,
                        &launched,
                        |owner| identity.pid_of(owner),
                    );
                    shell.present_icon_artwork(&mut compositor);
                }
                if relisted || papered || arted {
                    shell.present_desktop(&mut compositor, &desktop);
                }
                if let Some(concluded) = picker.resume(&mut shell, &mut compositor) {
                    conclude_pick(
                        concluded,
                        &mut server,
                        &mut sink,
                        &mut shell,
                        &mut compositor,
                        &mut windows,
                        &identity,
                        &mut picker,
                        &mut apps,
                        &mut menu,
                    );
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
                // re-render itself, so a *visible* window whose pixels the
                // same trim released is asked to present again straight away.
                // A hidden one is told it may let go of its own copies
                // instead, and asked when it is next shown: presenting it now
                // would spend the memory the release recovered on pixels
                // nobody can see.
                if tairix_procinfo::pressure::refresh() {
                    let _ = shell.trim_caches(&mut compositor);
                    tairix_font::trim_glyph_cache();
                    deliver_released_notices(
                        &mut server,
                        &mut sink,
                        &mut shell,
                        &mut compositor,
                        &mut windows,
                        &mut picker,
                        &mut apps.service,
                        &mut menu,
                    );
                    // A band that refused to keep a decode may now allow it,
                    // and a band that has just tightened will refuse it once
                    // more and be recorded again. Either way the decision is
                    // remade here, on the band's own wake, rather than by
                    // every repaint in between.
                    artworks.retry_declined();
                    deliver_pending_redraws(
                        &mut server,
                        &mut sink,
                        &mut shell,
                        &mut compositor,
                        &mut windows,
                        &mut picker,
                        &mut apps.service,
                        &mut menu,
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
                                apps: &mut apps.service,
                                menu: &mut menu,
                                seat_held: true,
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
            } else if token == WAKE_TOKEN {
                // The session authority speaking to this desktop: it is the
                // foreground session again, or the authority is going away
                // and a session it can no longer reach must not be left
                // stranded. The sender is the kernel's account of who sent
                // it, never a claim on the wire; a message from anyone else,
                // or one that does not decode, is dropped with its reason
                // stated and acted on by nothing.
                let Some(wake) = switch.wake_endpoint() else {
                    continue;
                };
                let mut message = [0u8; SESSION_WAKE_LEN];
                let mut sender = [0u8; ORIGIN_WIRE_LEN];
                let Ok(len) = tairix_rt::ipc_recv(wake, &mut message, &mut sender) else {
                    continue;
                };
                let Ok(origin) = Origin::from_bytes(&sender) else {
                    io::write_stderr_line("desktop: dropped an unattested session wake");
                    continue;
                };
                match switch.classify(&message[..len], &origin) {
                    Ok(SessionWake::Foreground) => {
                        let mut screen = SessionScreen {
                            display: &mut display,
                            region: &mut region,
                            compositor: &mut compositor,
                            shell: &mut shell,
                            desktop: &desktop,
                            pinboard: &mut pinboard,
                            wallpapers: &wallpapers,
                            fade: &mut fade,
                            set,
                        };
                        let mode = match switch.resume(&mut screen) {
                            Ok(mode) => mode,
                            Err(failure) => {
                                let _ = writeln!(
                                    Stderr,
                                    "desktop: {} ({:?})",
                                    failure.reason(),
                                    failure.errno()
                                );
                                shell.teardown(&mut compositor);
                                return EXIT_RESUME_FAILED;
                            }
                        };
                        // The pointer is clamped to the screen, so it is
                        // rebuilt for the mode now in force rather than left
                        // on the one this session came up with.
                        let screen_rect = Rect::new(0, 0, mode.width_px, mode.height_px);
                        let Ok(rebuilt) =
                            DeviceInputSource::new(pointer.into_channel(), screen_rect)
                        else {
                            shell.teardown(&mut compositor);
                            return fail(
                                EXIT_RESUME_FAILED,
                                "the resumed mode has no pointer surface",
                            );
                        };
                        pointer = rebuilt;
                    }
                    Ok(SessionWake::End) => {
                        io::write_stderr_line(
                            "desktop: the login service is going away; ending this session",
                        );
                        shell.teardown(&mut compositor);
                        return EXIT_AUTHORITY_GONE;
                    }
                    Err(refusal) => {
                        let _ = writeln!(Stderr, "desktop: {}", refusal.reason());
                    }
                }
            } else if token == SEAT_TOKEN && lock.is_locked() {
                // Locked: the seat's events belong to the lock and to
                // nothing else. They are drained straight out of the
                // channels here — not through the shell — so no pointer
                // motion, click, or keystroke can reach the window manager,
                // the taskbar, or a served application while the screen is
                // secured. This is the routing half of the lock; the
                // full-screen surface only hides the session.
                // One wake is one instant here too: the whole drained batch
                // reaches the surface against the clock read once, so the
                // motion it times cannot step mid-batch.
                if drain_locked(
                    &mut lock,
                    tairix_rt::clock_get(),
                    &mut pointer,
                    &mut keyboard,
                    &mut BrokerUnlocker,
                    &mut shell,
                    &mut compositor,
                ) == Drained::Faulted
                {
                    return drain_fault(&mut shell, &mut compositor, Errno::DeviceFault);
                }
            } else if token == SEAT_TOKEN {
                // Drain both input channels, routing every outcome onward (to
                // the focused app window, or the launcher spawn); the events
                // already applied stay applied, and a faulting drain ends the
                // session. The drains are genuinely non-blocking
                // (`pointer_read` / `keyboard_read` return 0 when empty).
                // One wake is one instant: the whole drained batch resolves
                // its time-driven gestures (a held capsule press) against
                // the clock read here, and an idle desktop reads none.
                let now_ns = tairix_rt::clock_get();
                // A chain holds the seat: every pointer and key event routes
                // into it and none reaches what is behind, which is what makes
                // a press outside a dismissal rather than a click on the window
                // the menu was covering. Its own answers then route through the
                // very same path a click on the bar's takes, so a *Log Out* row
                // and a *Log Out* click are honoured in one place.
                let chain_held = menu.is_open();
                let outcomes = if chain_held {
                    if drain_menu_chain(
                        &mut menu,
                        &mut pointer,
                        &mut keyboard,
                        &mut shell,
                        &mut compositor,
                        &mut windows,
                        &mut server,
                        &mut sink,
                        &mut picker,
                        &mut apps.service,
                        &mut DesktopMenuDesk {
                            pinboard: &mut pinboard,
                            wallpapers: &wallpapers,
                            desktop: &mut desktop,
                            launched: &mut launched,
                            associations: &mut associations,
                            answered: &mut answered,
                        },
                        now_ns,
                    ) == Drained::Faulted
                    {
                        return drain_fault(&mut shell, &mut compositor, Errno::DeviceFault);
                    }
                    core::mem::take(&mut answered)
                } else {
                    match shell.pump(&mut pointer, &mut compositor, now_ns) {
                        Ok(outcomes) => outcomes,
                        Err(err) => return drain_fault(&mut shell, &mut compositor, err),
                    }
                };
                // Set once the session has given the screen up: what is left
                // of this batch, and everything still queued behind it, is
                // input for a seat this desktop no longer owns.
                let mut stepped_aside = false;
                for outcome in outcomes {
                    route_desktop(
                        &outcome,
                        &mut pinboard,
                        &wallpapers,
                        &mut desktop,
                        &mut shell,
                        &mut compositor,
                        &windows,
                        &mut menu,
                        seat_held(&lock, &picker),
                        &mut launched,
                        &mut associations,
                        now_ns,
                    );
                    match route_outcome(
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
                        &mut elevate,
                        &mut lock,
                        &mut menu,
                        account,
                        &mut launched,
                        &mut apps,
                        &mut switchboard_pid,
                        &mut pending_open,
                        &mut associations,
                    ) {
                        Routed::Continue => {}
                        Routed::EndSession => {
                            fade_to_black(&mut fade, &mut compositor, &mut display);
                            shell.teardown(&mut compositor);
                            return EXIT_LOGGED_OUT;
                        }
                        Routed::SwitchUser => {
                            stepped_aside = step_aside(
                                &mut switch,
                                SessionScreen {
                                    display: &mut display,
                                    region: &mut region,
                                    compositor: &mut compositor,
                                    shell: &mut shell,
                                    desktop: &desktop,
                                    pinboard: &mut pinboard,
                                    wallpapers: &wallpapers,
                                    fade: &mut fade,
                                    set,
                                },
                            );
                            if stepped_aside {
                                break;
                            }
                        }
                    }
                }
                // Every keystroke is applied in order, and the screen is
                // settled once for the whole batch below: a held key
                // repeating costs one taskbar present, active-frame sync and
                // cursor refresh rather than one of each per repeat. A chain
                // that held this batch drained the keyboard into itself, so
                // there is nothing here for the shell.
                let mut typed = false;
                while !stepped_aside && !chain_held {
                    match keyboard.poll_record() {
                        Ok(None) => break,
                        Ok(Some((event, record))) => {
                            let outcome = shell.apply(event, &mut compositor, now_ns);
                            typed = true;
                            route_desktop(
                                &outcome,
                                &mut pinboard,
                                &wallpapers,
                                &mut desktop,
                                &mut shell,
                                &mut compositor,
                                &windows,
                                &mut menu,
                                seat_held(&lock, &picker),
                                &mut launched,
                                &mut associations,
                                now_ns,
                            );
                            match route_outcome(
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
                                &mut elevate,
                                &mut lock,
                                &mut menu,
                                account,
                                &mut launched,
                                &mut apps,
                                &mut switchboard_pid,
                                &mut pending_open,
                                &mut associations,
                            ) {
                                Routed::Continue => {}
                                Routed::EndSession => {
                                    fade_to_black(&mut fade, &mut compositor, &mut display);
                                    shell.teardown(&mut compositor);
                                    return EXIT_LOGGED_OUT;
                                }
                                Routed::SwitchUser => {
                                    stepped_aside = step_aside(
                                        &mut switch,
                                        SessionScreen {
                                            display: &mut display,
                                            region: &mut region,
                                            compositor: &mut compositor,
                                            shell: &mut shell,
                                            desktop: &desktop,
                                            pinboard: &mut pinboard,
                                            wallpapers: &wallpapers,
                                            fade: &mut fade,
                                            set,
                                        },
                                    );
                                }
                            }
                        }
                        Err(err) => return drain_fault(&mut shell, &mut compositor, err),
                    }
                }
                if stepped_aside {
                    // The screen belongs to somebody else now: nothing this
                    // wake read is applied any further, and nothing is drawn.
                    continue;
                }
                if typed {
                    shell.settle(&mut compositor);
                }
                // A window minimised while the machine was already short of
                // memory had its content released by the gesture itself, so
                // its client is told here rather than waiting for a band
                // change that may never come.
                deliver_released_notices(
                    &mut server,
                    &mut sink,
                    &mut shell,
                    &mut compositor,
                    &mut windows,
                    &mut picker,
                    &mut apps.service,
                    &mut menu,
                );
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
                    &mut apps.service,
                    &mut menu,
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
            // A launch recorded this wake has a window coming — a spawn, a
            // load, and the app's own bring-up away. Starting its icon now has
            // the picture ready by the time there is a window to put it on,
            // rather than the window opening on the shared application glyph
            // and swapping it a decode later.
            shell.warm_launched_artwork(&compositor, launched.bundles());
            // Bring the application strip up to date before presenting: a
            // declaration that arrived or was withdrawn latches the service
            // dirty, and otherwise the cheap live-window comparison decides
            // — so the strip is re-resolved exactly when an application
            // joined the bar, left it, re-declared, or opened or closed a
            // window.
            if apps.service.take_dirty() || app_strip_is_stale(&apps, &shell, &server, &windows) {
                refresh_app_strip(
                    &mut apps,
                    &mut shell,
                    &mut compositor,
                    &server,
                    &windows,
                    &identity,
                    &launched,
                );
            }
            // Nothing an application does may surface over a locked
            // screen: whatever opened, raised, or resized behind the lock
            // this wake, the lock goes back on top before the frame is
            // shown. Idle when the screen is not locked.
            lock.keep_topmost(&mut compositor);
            // One window thumbnail per wake, so a hover picker over a
            // screenful of windows fills in across the turns the loop was
            // making anyway instead of scaling every frame in one of them.
            shell.advance_window_thumbnails(&mut compositor);
            // Whatever is animating steps to the instant this frame is
            // actually shown at, not to when the wake arrived, so the work
            // this wake did does not age the frame. One clock reading serves
            // the whole frame, as on the deadline path above: the animation
            // and the frame report's rate limit share it.
            let now_ns = tairix_rt::clock_get();
            animate(
                &mut fade,
                &mut lock,
                &mut clock,
                &mut shell,
                &desktop,
                &mut compositor,
                now_ns,
            );
            // One present per frame deadline: the compositor accumulates the
            // damage the pumped events and served presents produced, and the
            // ring copies only that region once the pacer admits the frame.
            if pacer.admit(now_ns, compositor.has_damage()) {
                if let Err(code) = present(
                    &mut shell,
                    &mut compositor,
                    &mut display,
                    &mut fade,
                    &mut windows,
                    &mut menu,
                ) {
                    return code;
                }
            }
            // What that frame cost, for the monitor's Resources page. After
            // the present, so the counts describe pixels already on screen,
            // and silent unless they moved, unless the rate limiter is still
            // holding the last change back, or unless the only content was
            // the Switchboard painting the number itself.
            frames.maybe_send(
                &compositor,
                switchboard_pid,
                frame_content(&mut windows, &server, &identity, switchboard_pid),
                now_ns,
                &mut RtSwitchboardMailbox,
            );
            // And the same counts to the System Information API, where a
            // reader outside this process — a monitor, a shell, a regression
            // gate — asks for them instead of being pushed them.
            frame_stats.maybe_publish(&compositor, now_ns, &mut RtFrameStatsSink);
            // The wake is fully handled and its frame is on screen: report
            // what the desktop's caches hold now, before parking again. A
            // change made this turn would otherwise wait for the next wake,
            // which on an idle desktop may be a very long time. Silent
            // unless a figure actually moved.
            tairix_rt::cachereport::publish_if_due();
        }
    }

    /// The session's icon-bar state: the declaration-holding service plus
    /// the strip it last resolved, kept beside the loop so a click resolves
    /// against exactly what the bar shows.
    ///
    /// The slots' icons are not here: the shell owns the one artwork cache
    /// and the seams it reads and decodes through, so the strip's icons and
    /// the rest of the desktop's cannot be cached twice.
    struct AppBarPanel {
        service: AppBarService,
        strip: alloc::vec::Vec<tairix_desktop_session::AppGroup>,
    }

    impl AppBarPanel {
        fn new() -> Self {
            Self {
                service: AppBarService::new(),
                strip: alloc::vec::Vec::new(),
            }
        }
    }

    /// The one nudge the session's worker threads wake its serve loop with: a
    /// byte on a pipe whose read end is a wait-set member.
    ///
    /// Shared by every worker rather than one pipe each. The serve loop's arm
    /// offers each consumer the chance to adopt whatever arrived, and a consumer
    /// with nothing waiting costs a branch — so a second token, a second drain,
    /// and a second descriptor would buy nothing.
    struct WorkerWake {
        /// `None` when the pipe could not be created, in which case the session
        /// does the work on its own task: slower under load, never wrong.
        fd: Option<u32>,
    }

    impl WorkerWake {
        /// Nudge the serve loop.
        ///
        /// A refused write is dropped rather than retried: the only thing it can
        /// mean is that the session is not draining, and a worker that spun on
        /// it would be the busy-wait the charter forbids. The answer stays on its
        /// desk, so the next wake for any reason still delivers it.
        fn nudge(&self) {
            if let Some(fd) = self.fd {
                let _ = tairix_rt::fs_write(fd, 0, &[1u8]);
            }
        }
    }

    /// The desktop's wallpaper, prepared on a worker thread that owns its **own**
    /// sandbox worker.
    ///
    /// The icon rasteriser keeps the shared sandbox handle on the session's own
    /// task, untouched; this thread creates a second capability-empty worker
    /// inside itself, so no sandbox handle ever crosses a thread boundary. The
    /// policy is the host-tested [`WallpaperDesk`].
    struct Wallpapers {
        desk: tairix_rt::sync::Mutex<WallpaperDesk>,
        work: tairix_rt::sync::Condvar,
        wake: alloc::sync::Arc<WorkerWake>,
    }

    impl Wallpapers {
        fn new(wake: alloc::sync::Arc<WorkerWake>) -> Self {
            Self {
                desk: tairix_rt::sync::Mutex::new(WallpaperDesk::new()),
                work: tairix_rt::sync::Condvar::new(),
                wake,
            }
        }

        /// One preparer's whole life: park until a picture is wanted, read it,
        /// decode it in this thread's own sandbox, and deliver the surface.
        ///
        /// The sandbox is built here, once, and reused for every later
        /// preparation — the same lifetime the session's own handle has, and the
        /// reason this thread rather than the session owns it.
        fn serve(&self) {
            let mut sandbox = ParserSandbox::new(RtLauncher::own_binary(), tairix_rt::LogSink);
            loop {
                let job = {
                    let mut desk = self.desk.lock();
                    loop {
                        if desk.stopping() {
                            return;
                        }
                        if let Some(job) = desk.next_job() {
                            break job;
                        }
                        desk = self.work.wait(desk);
                    }
                };
                // The read and the sandbox round trip, with no lock held: these
                // are the calls that used to stall the desktop.
                let outcome = prepare_wallpaper_surface(&mut sandbox, &job);
                if self.desk.lock().deliver(job, outcome) {
                    self.wake.nudge();
                }
            }
        }

        /// Record `source` as wanted and wake a preparer.
        ///
        /// With no preparer to answer it the picture is prepared on the calling
        /// thread instead, exactly as the session did before it had one: a
        /// recorded request nobody will serve would leave the backdrop bare
        /// forever.
        fn request(&self, source: &WallpaperSource, own: &SharedSandbox) -> Prepared {
            let deferred = {
                let mut desk = self.desk.lock();
                if desk.stopping() {
                    None
                } else {
                    Some(desk.take(source))
                }
            };
            let Some(prepared) = deferred else {
                if source.image_path().is_none() {
                    return Prepared::Ready {
                        surface: None,
                        refusal: None,
                    };
                }
                return match prepare_wallpaper_surface(&mut own.borrow_mut(), source) {
                    Ok(surface) => Prepared::Ready {
                        surface: Some(surface),
                        refusal: None,
                    },
                    Err(refusal) => Prepared::Ready {
                        surface: None,
                        refusal: Some(refusal),
                    },
                };
            };
            if matches!(prepared, Prepared::Pending) {
                self.work.notify_one();
            }
            prepared
        }

        /// Ask the preparer to leave.
        fn stop(&self) {
            self.desk.lock().stop();
            self.work.notify_all();
        }
    }

    /// Read the image `source` names, place it over its screen in `sandbox`, and
    /// rebuild the result as the surface the compositor blits.
    ///
    /// Every refusal — a file that cannot be read, one larger than any wallpaper,
    /// a malformed image, a crashed worker, or a reply whose pixels do not fill
    /// the screen — *answers* the reason rather than writing it, because this runs
    /// on a worker thread and `stderr` is one descriptor a formatted line reaches
    /// in several writes. The session states it, once, on its own thread; the
    /// desktop falls back to the backdrop colour instead of failing over a
    /// picture.
    fn prepare_wallpaper_surface<L: tairix_sandbox::Launcher, S: tairix_log::Sink>(
        sandbox: &mut ParserSandbox<L, S>,
        source: &WallpaperSource,
    ) -> Result<Surface, alloc::string::String> {
        let Some(path) = source.image_path() else {
            return Err(alloc::string::String::from(
                "desktop: no wallpaper image to prepare; using the backdrop colour",
            ));
        };
        let bytes = match read_file(path, MAX_WALLPAPER_BYTES) {
            Ok(bytes) if bytes.len() > MAX_WALLPAPER_BYTES => {
                return Err(alloc::format!(
                    "desktop: wallpaper {path} is larger than any wallpaper the desktop renders; \
                     using the backdrop colour"
                ));
            }
            Ok(bytes) => bytes,
            Err(err) => {
                return Err(alloc::format!(
                    "desktop: wallpaper {path} could not be read ({err}); using the backdrop \
                     colour"
                ));
            }
        };
        let placed = render_wallpaper(sandbox, source.width, source.height, source.fit, &bytes)
            .map_err(|err| {
                alloc::format!(
                    "desktop: wallpaper {path} could not be rendered ({err}); using the backdrop \
                     colour"
                )
            })?;
        Surface::from_rgba8(source.width, source.height, &placed).ok_or_else(|| {
            alloc::format!(
                "desktop: wallpaper {path} did not fill the screen; using the backdrop colour"
            )
        })
    }

    /// The desktop's icon artwork, decoded on a worker thread that owns its
    /// **own** sandbox worker.
    ///
    /// Every icon the taskbar, the launcher popup, and the desktop's column
    /// draw costs a bounded read plus a sandbox round trip the first time it is
    /// asked for at a given pixel side. Run on the serve loop that was a visible
    /// freeze — a launcher opening on thirty applications paid it thirty times
    /// before its first pixel. Here it costs the frame that icon spends on its
    /// built-in glyph instead. The policy is the host-tested [`ArtworkDesk`];
    /// this adds the runtime's futex mutex for exclusion, a condition variable
    /// the worker parks on with nothing to do (never a spin), and the shared
    /// wake pipe the wait-set already watches.
    struct Artworks {
        desk: tairix_rt::sync::Mutex<ArtworkDesk>,
        /// Signalled when a decode is recorded, and on teardown.
        work: tairix_rt::sync::Condvar,
        wake: alloc::sync::Arc<WorkerWake>,
    }

    impl Artworks {
        fn new(wake: alloc::sync::Arc<WorkerWake>) -> Self {
            Self {
                desk: tairix_rt::sync::Mutex::new(ArtworkDesk::new()),
                work: tairix_rt::sync::Condvar::new(),
                wake,
            }
        }

        /// One decoder's whole life: park until an icon is wanted, read it,
        /// decode it in this thread's own sandbox, and deliver the pixels.
        ///
        /// The sandbox is built here, once, and reused for every later decode —
        /// the same lifetime the session's own handle has, and the reason this
        /// thread rather than the session owns it.
        fn serve(&self) {
            let mut reader = ArtworkFileReader(VfsFileReader);
            let mut rasteriser = ArtworkSandbox(OwnedSandbox(ParserSandbox::new(
                RtLauncher::own_binary(),
                tairix_rt::LogSink,
            )));
            // Whether a delivery since the last nudge still owes the session
            // one, so a batch drained without waking it cannot be stranded by a
            // final job the desk no longer wants.
            let mut owed = false;
            loop {
                let job = {
                    let mut desk = self.desk.lock();
                    loop {
                        if desk.stopping() {
                            return;
                        }
                        if let Some(job) = desk.next_job() {
                            break job;
                        }
                        desk = self.work.wait(desk);
                    }
                };
                // The read and the sandbox round trip, with no lock held: these
                // are the calls that used to stall the desktop. The decode is
                // the shared one, so what a worker produces is exactly what the
                // calling thread would have.
                let artwork =
                    tairix_icon::render_artwork(&mut reader, &mut rasteriser, &job.key, job.side);
                let (delivered, more) = {
                    let mut desk = self.desk.lock();
                    (desk.deliver(&job, artwork), desk.has_work())
                };
                owed |= delivered;
                // Wake the session when the batch is drained rather than after
                // every icon: a bring-up that wants thirty of them costs one
                // repaint instead of thirty, and they appear together. A lone
                // icon empties the queue immediately, so it still lands the
                // moment it is ready.
                if owed && !more {
                    owed = false;
                    self.wake.nudge();
                }
            }
        }

        /// Answer a paint's miss on `key` at `side`, waking a decoder if there
        /// is anything for one to do.
        ///
        /// A notify with no decode outstanding wakes nobody and a worker already
        /// running is not waiting to be told, so the signal is unconditional
        /// rather than a second reading of the desk's own state.
        fn resolve(&self, key: &ArtworkKey, side: u32) -> Resolved {
            let (answer, wanted) = {
                let mut desk = self.desk.lock();
                let answer = desk.collect(key, side);
                (answer, desk.has_work())
            };
            if wanted {
                self.work.notify_one();
            }
            answer
        }

        /// Record `key` at `side` as wanted and wake a decoder, without waiting
        /// for or collecting an answer.
        ///
        /// This is what a warm-up drives: the surface that will draw the icon is
        /// not painting yet, so there is nothing to answer — only work to start.
        fn want(&self, key: &ArtworkKey, side: u32) {
            let wanted = {
                let mut desk = self.desk.lock();
                desk.want(key, side);
                desk.has_work()
            };
            if wanted {
                self.work.notify_one();
            }
        }

        /// Whether a decode has landed since this was last asked.
        fn take_landed(&self) -> bool {
            self.desk.lock().take_landed()
        }

        /// Open a fresh round, because the desktop acted and what it draws may
        /// have changed.
        fn begin_round(&self) {
            self.desk.lock().begin_round();
        }

        /// Note that the cache refused to keep this decode, so no round asks
        /// for it again until the band moves.
        fn decline(&self, key: &ArtworkKey, side: u32) {
            self.desk.lock().decline(key, side);
        }

        /// The band moved: offer the refused decodes again.
        fn retry_declined(&self) {
            self.desk.lock().retry_declined();
        }

        /// Ask the decoder to leave.
        fn stop(&self) {
            self.desk.lock().stop();
            self.work.notify_all();
        }
    }

    /// The serve loop's [`ArtworkResolver`]: whatever the decoder has already
    /// produced, and otherwise a recorded decode and the built-in glyph for this
    /// frame.
    ///
    /// Held by the shell behind a boxed trait object, which is why it owns its
    /// handle to the desk rather than borrowing one.
    struct DeferredArtwork(alloc::sync::Arc<Artworks>);

    impl ArtworkResolver for DeferredArtwork {
        fn resolve(&mut self, key: &ArtworkKey, side: u32) -> Resolved {
            self.0.resolve(key, side)
        }

        fn prefetch(&mut self, key: &ArtworkKey, side: u32) {
            self.0.want(key, side);
        }

        fn declined(&mut self, key: &ArtworkKey, side: u32) {
            self.0.decline(key, side);
        }
    }

    /// The desktop's directory listings, read on a worker thread so a slow or
    /// contended disk cannot stall the compositor, the seat drain, or an
    /// application blocked in a window call.
    ///
    /// The policy — who asked for what, which answer is stale, whose turn it is
    /// — is the host-tested [`ListingDesk`]; this adds only the three things a
    /// real program brings: the runtime's futex mutex for exclusion, a
    /// condition variable the worker parks on with nothing to do (never a
    /// spin), and the write end of the pipe whose read end is a wait-set
    /// member, so the session learns an answer landed through the very loop it
    /// already parks in — no new ABI and no second wake mechanism.
    struct Listings {
        desk: tairix_rt::sync::Mutex<ListingDesk>,
        /// Signalled when a request is recorded, and on teardown.
        work: tairix_rt::sync::Condvar,
        wake: alloc::sync::Arc<WorkerWake>,
    }

    impl Listings {
        /// A desk with no worker yet.
        fn new(wake: alloc::sync::Arc<WorkerWake>) -> Self {
            Self {
                desk: tairix_rt::sync::Mutex::new(ListingDesk::new()),
                work: tairix_rt::sync::Condvar::new(),
                wake,
            }
        }

        /// One worker's whole life: park until there is a directory to read,
        /// read it, deliver it, wake the session.
        ///
        /// Leaves when the desk stops. A read that nobody wants any more is
        /// delivered all the same and reports itself unwanted, so no wake is
        /// owed for it — a user clicking through directories does not make the
        /// session repaint once per abandoned read.
        fn serve(&self) {
            loop {
                let job = {
                    let mut desk = self.desk.lock();
                    loop {
                        if desk.stopping() {
                            return;
                        }
                        if let Some(job) = desk.next_job() {
                            break job;
                        }
                        desk = self.work.wait(desk);
                    }
                };
                let (client, target) = job;
                // The read itself, with no lock held: this is the call that can
                // take as long as the disk takes.
                let result = read_directory(&target);
                if self.desk.lock().deliver(client, target, result) {
                    self.wake.nudge();
                }
            }
        }

        /// Record `components` as `client`'s request and wake a worker.
        ///
        /// With no worker to answer it — the kernel granted no thread, or the
        /// session is tearing down — the read happens on the calling thread
        /// instead, which is exactly what the session did before it had one. A
        /// recorded request nobody will ever serve would leave the desktop
        /// listing forever, so the degradation is a real read, not a wait.
        fn request(
            &self,
            client: ListingClient,
            components: &[alloc::string::String],
        ) -> Result<Listing, Errno> {
            let deferred = {
                let mut desk = self.desk.lock();
                if desk.stopping() {
                    None
                } else {
                    Some(desk.take(client, components))
                }
            };
            let Some(listing) = deferred else {
                return read_directory(components).map(Listing::Ready);
            };
            if matches!(listing, Ok(Listing::Pending)) {
                self.work.notify_one();
            }
            listing
        }

        /// Ask the workers to leave and wake every one of them.
        fn stop(&self) {
            self.desk.lock().stop();
            self.work.notify_all();
        }
    }

    /// Read the directory named by root-first `components` under this session's
    /// own identity, through the same validated path spelling and stream decode
    /// the synchronous source uses.
    fn read_directory(components: &[alloc::string::String]) -> Result<Vec<Entry>, Errno> {
        let path = tairix_browse::vfs::absolute_path(components)?;
        let stream = tairix_rt::read_dir_all(path.as_bytes()).map_err(Errno::from_syscall)?;
        tairix_browse::vfs::entries_from_dir_stream(
            &path,
            &stream,
            &mut tairix_browse::RtLinkReader,
        )
    }

    /// One consumer's view of [`Listings`]: a [`DirectorySource`] that records a
    /// request and answers with whatever has come back.
    ///
    /// Cheap to clone, because the picker builds a fresh browser per pick and
    /// both consumers must reach the one worker rather than each starting their
    /// own.
    #[derive(Clone)]
    struct AsyncDirectorySource {
        listings: alloc::sync::Arc<Listings>,
        client: ListingClient,
    }

    impl DirectorySource for AsyncDirectorySource {
        fn list(&mut self, components: &[alloc::string::String]) -> Result<Listing, Errno> {
            self.listings.request(self.client, components)
        }
    }

    /// The pool each composite's per-pixel work is spread across: one
    /// participant per online CPU, of which the serve loop's own thread is one.
    ///
    /// The count is *discovered* through the System Information API — the only
    /// interface live machine facts come from — never a constant, so the same
    /// binary uses a four-core Pi's cores and a server's without a rebuild. A
    /// machine that reports one CPU, and a session that cannot reach the service
    /// or is refused a thread, all compose on the serve loop's own thread and pay
    /// nothing for the machinery: fewer cores is slower, never wrong.
    ///
    /// The pool lives as long as the session does, so it is created once here and
    /// leaked deliberately — its workers are process-lifetime threads, and a
    /// pool torn down at some arbitrary point would only mean joining them again
    /// at exit.
    fn composite_pool() -> &'static Pool {
        let online = tairix_procinfo::cpu_info(&IpcTransport).map_or(1, |cpus| cpus.len());
        let pool: &'static Pool =
            alloc::boxed::Box::leak(alloc::boxed::Box::new(Pool::for_cpus(online)));
        // Fewer workers than the machine has cores is a refusal worth stating:
        // the desktop still draws, more slowly than the hardware allows.
        let wanted = online.saturating_sub(1);
        if pool.worker_count() < wanted {
            let _ = writeln!(
                Stderr,
                "desktop: composing on {} of {online} cores (the kernel granted \
                 {} of {wanted} compositing threads)",
                pool.worker_count().saturating_add(1),
                pool.worker_count(),
            );
        }
        pool
    }

    /// The serve loop's own parser-sandbox worker: this binary re-entered as a
    /// capability-empty child, which an untrusted image is decoded in.
    ///
    /// Shared behind an `Rc` because the loop's pinboard state and, where no
    /// decoder thread was granted, the shell's boxed resolver both need the very
    /// same live worker — and *not* `Send`, deliberately: each worker thread
    /// creates its own rather than borrowing this one, so a sandbox handle never
    /// crosses a thread. On a desktop that got its threads this is the fallback
    /// path only, and its child is never even spawned.
    type SharedSandbox =
        alloc::rc::Rc<core::cell::RefCell<ParserSandbox<RtLauncher, tairix_rt::LogSink>>>;

    /// The production [`IconRasteriser`]: untrusted icon bytes go to the
    /// parser-sandbox icon service — this binary re-entered as a
    /// capability-empty worker — and only a verified pixel block comes
    /// back. Any refusal (malformed image, crashed worker, unavailable
    /// spawn) is `None`: the slot falls back to its class glyph.
    struct SandboxRasteriser {
        sandbox: SharedSandbox,
    }

    impl IconRasteriser for SandboxRasteriser {
        fn rasterise(&mut self, side: u32, icon: &[u8]) -> Option<alloc::vec::Vec<u8>> {
            rasterise_icon(&mut self.sandbox.borrow_mut(), side, icon).ok()
        }
    }

    /// The same decode over a sandbox worker the *calling thread* owns
    /// outright.
    ///
    /// [`SandboxRasteriser`] shares the serve loop's handle behind an `Rc`,
    /// which a thread cannot take and must not: the artwork worker builds one
    /// of these instead, exactly as the wallpaper worker builds its own.
    struct OwnedSandbox(ParserSandbox<RtLauncher, tairix_rt::LogSink>);

    impl IconRasteriser for OwnedSandbox {
        fn rasterise(&mut self, side: u32, icon: &[u8]) -> Option<alloc::vec::Vec<u8>> {
            rasterise_icon(&mut self.0, side, icon).ok()
        }
    }

    /// The session's pinboard state, kept beside the loop: the loop's own
    /// sandbox worker (the wallpaper's fallback when no thread was granted),
    /// and what the wallpaper surface now on screen was prepared from.
    ///
    /// The backdrop menu is *not* here: it is the seat's one menu chain like
    /// every other menu on the desktop, so the pinboard hands over a model and
    /// keeps no shell of its own.
    ///
    /// The settings themselves are *not* here: the desktop model owns them,
    /// so there is exactly one copy of what is in force. Nor is the store —
    /// it is the application's own published app-data scope, opened for the
    /// one round trip a read or a publish costs and never held between them,
    /// so there is no handle here that could go stale against what the
    /// service holds.
    struct PinboardPanel {
        sandbox: SharedSandbox,
        prepared: Option<WallpaperSource>,
    }

    /// What a chosen row of one of the **desktop's own** menus acts on,
    /// bundled so it reaches the chain's one delivery point without a dozen
    /// more parameters.
    ///
    /// Only a desktop-owned chain reads it. An application's chain is answered
    /// over the window channel and touches none of this.
    ///
    /// The backdrop's rows act on the desktop model directly. The icon bar's
    /// resolve to the same typed [`TaskbarResponse`] a click on the bar
    /// produces, so they leave through `answered` and are routed exactly where
    /// every other bar outcome is — there is no second place a *Log Out* row
    /// and a *Log Out* click are honoured.
    struct DesktopMenuDesk<'a, S: DirectorySource> {
        pinboard: &'a mut PinboardPanel,
        wallpapers: &'a Wallpapers,
        desktop: &'a mut Desktop<S>,
        launched: &'a mut LaunchTable,
        associations: &'a mut alloc::vec::Vec<AppAssociation>,
        /// The outcomes the bar's own chains resolved to, in the order they
        /// were chosen.
        answered: &'a mut Vec<tairix_desktop_session::ShellOutcome>,
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
        let loaded = read_pinboard_store(&mut RtHost);
        for warning in &loaded.warnings {
            let _ = write!(Stderr, "{warning}");
        }
        desktop.apply_settings(loaded.settings);
        PinboardPanel {
            sandbox,
            prepared: None,
        }
    }

    /// Ask for the wallpaper the desktop layer should be painted over, and
    /// install it when it is ready.
    ///
    /// Called both when something changed (bring-up, a settings apply, a resume
    /// at a new mode) and on the wake that says a preparation finished, because
    /// the two are the same question: *is what the desktop wants what is
    /// installed?* Answering it costs a comparison when nothing changed, so a
    /// wake the desktop has already acted on is almost free.
    ///
    /// Reads a file and runs a sandboxed decode, so it happens on a worker
    /// thread; the desktop keeps painting whatever it has until the answer
    /// lands, and a wallpaper that cannot be read or rendered installs no
    /// surface and leaves the backdrop colour showing (stated once, by the
    /// worker that observed it). Answers whether the desktop layer needs
    /// repainting.
    fn prepare_wallpaper<S: DirectorySource>(
        pinboard: &mut PinboardPanel,
        wallpapers: &Wallpapers,
        shell: &mut DesktopShell,
        desktop: &Desktop<S>,
        compositor: &Compositor,
        now_ns: u64,
    ) -> bool {
        let wanted = WallpaperSource::wanted(desktop.settings(), compositor.screen_rect());
        if pinboard.prepared.as_ref() == Some(&wanted) {
            return false;
        }
        match wallpapers.request(&wanted, &pinboard.sandbox) {
            Prepared::Pending => false,
            Prepared::Ready { surface, refusal } => {
                // Stated here, on the serve loop's own thread, so a worker's
                // diagnosis cannot interleave with anything else reaching
                // `stderr`.
                if let Some(reason) = refusal {
                    let _ = writeln!(Stderr, "{reason}");
                }
                pinboard.prepared = Some(wanted);
                shell.set_wallpaper(surface, desktop.settings().backdrop, now_ns);
                true
            }
        }
    }

    /// Drain the seat straight into the open menu chain, which holds it.
    ///
    /// The routing half of the grab: nothing behind a chain is reachable
    /// while it is up, a press with none of the chain under it dismisses and
    /// is consumed, and every answer the chain settles on leaves through the
    /// one delivery point below.
    #[allow(clippy::too_many_arguments)] // The chain's whole mutable surround, threaded explicitly.
    fn drain_menu_chain<S: DirectorySource, F: FnMut() -> S>(
        menu: &mut MenuChain,
        pointer: &mut DeviceInputSource<SeatInputChannel<PointerReader>>,
        keyboard: &mut KeyboardInputSource<SeatInputChannel<KeyboardReader>>,
        shell: &mut DesktopShell,
        compositor: &mut Compositor,
        windows: &mut SessionWindows,
        server: &mut WindowServer<RtShmMapper>,
        sink: &mut RtEventSink,
        picker: &mut SessionPicker<S, F>,
        apps: &mut dyn AppBarBridge,
        desk: &mut DesktopMenuDesk<'_, S>,
        now_ns: u64,
    ) -> Drained {
        // The chain is taking the stream, so the shell gives the pointer up
        // for the reason the lock's drain does: no gesture of its own can be
        // completed from here, and nothing behind the plates may be left
        // showing a hover.
        shell.yield_pointer(compositor);
        let mut moved = false;
        loop {
            match pointer.poll() {
                Ok(None) => break,
                Ok(Some(event)) => {
                    // Motion alone still reaches the shell so the tracked
                    // pointer and the on-screen cursor stay in step; its
                    // outcome is discarded and no press ever reaches it.
                    if matches!(event, tairix_wm::InputEvent::PointerMoved { .. }) {
                        let _ = shell.apply(event, compositor, tairix_rt::clock_get());
                        moved = true;
                    }
                    let at = shell.router().pointer();
                    let acted = {
                        let geom = chain_geometry(shell, compositor);
                        menu.handle(&event, at, &geom)
                    };
                    settle_menu_chain(
                        &acted, menu, shell, compositor, windows, server, sink, picker, apps, desk,
                        now_ns,
                    );
                    if !menu.is_open() {
                        break;
                    }
                }
                Err(_) => return Drained::Faulted,
            }
        }
        if moved {
            shell.settle(compositor);
        }
        loop {
            match keyboard.poll_record() {
                Ok(None) => break,
                Ok(Some((event @ tairix_wm::InputEvent::KeyPressed { .. }, _))) => {
                    let at = shell.router().pointer();
                    let acted = {
                        let geom = chain_geometry(shell, compositor);
                        menu.handle(&event, at, &geom)
                    };
                    settle_menu_chain(
                        &acted, menu, shell, compositor, windows, server, sink, picker, apps, desk,
                        now_ns,
                    );
                    if !menu.is_open() {
                        break;
                    }
                }
                Ok(Some(_)) => {}
                Err(_) => return Drained::Faulted,
            }
        }
        Drained::Empty
    }

    /// Apply one chain outcome: repaint what moved, ask an owner for a
    /// surface, or take the chain's own surfaces down and answer it.
    #[allow(clippy::too_many_arguments)] // The chain's whole mutable surround, threaded explicitly.
    fn settle_menu_chain<S: DirectorySource, F: FnMut() -> S>(
        acted: &ChainAction,
        menu: &mut MenuChain,
        shell: &mut DesktopShell,
        compositor: &mut Compositor,
        windows: &mut SessionWindows,
        server: &mut WindowServer<RtShmMapper>,
        sink: &mut RtEventSink,
        picker: &mut SessionPicker<S, F>,
        apps: &mut dyn AppBarBridge,
        desk: &mut DesktopMenuDesk<'_, S>,
        now_ns: u64,
    ) {
        match acted {
            ChainAction::Consumed => {}
            ChainAction::Redraw | ChainAction::Closed => {
                present_menu_chain(menu, shell, compositor, windows);
            }
        }
        answer_menu_chain(
            menu, shell, compositor, windows, server, sink, picker, apps, desk, now_ns,
        );
    }

    /// Deliver every answer the chain owes, and bring the screen into line
    /// with what it now has.
    ///
    /// The session's **one** delivery point. Every close — a chosen row, a
    /// dismissal, a chain displaced by the next open, an owner's death, a
    /// mode change — queues its answer here rather than sending its own, so
    /// no chain can be answered twice and none can be left unanswered.
    #[allow(clippy::too_many_arguments)] // The chain's whole mutable surround, threaded explicitly.
    fn answer_menu_chain<S: DirectorySource, F: FnMut() -> S>(
        menu: &mut MenuChain,
        shell: &mut DesktopShell,
        compositor: &mut Compositor,
        windows: &mut SessionWindows,
        server: &mut WindowServer<RtShmMapper>,
        sink: &mut RtEventSink,
        picker: &mut SessionPicker<S, F>,
        apps: &mut dyn AppBarBridge,
        desk: &mut DesktopMenuDesk<'_, S>,
        now_ns: u64,
    ) {
        // Before the drain, not after: a chain the mode has moved under owes
        // an answer of its own, and settling once the queue is empty would
        // leave it sitting there until the next event.
        {
            let geom = chain_geometry(shell, compositor);
            menu.settle_mode(&geom);
        }
        for (owner, outcome) in menu.take_answers() {
            // A total match, so a third kind of owner cannot silently be
            // answered as the desktop's own.
            let (window_id, open_id) = match owner {
                ChainOwner::Window { window_id, open_id } => (window_id, open_id),
                ChainOwner::Backdrop => {
                    answer_backdrop_menu(outcome, shell, compositor, desk, now_ns);
                    continue;
                }
                ChainOwner::Bar(subject) => {
                    answer_bar_menu(&subject, outcome, shell, desk);
                    continue;
                }
            };
            let outcome = match outcome {
                ChainOutcome::Chosen(item) => MenuOutcome::Chosen(item),
                ChainOutcome::Dismissed => MenuOutcome::Dismissed,
                ChainOutcome::Refused(reason) => MenuOutcome::Refused(reason),
            };
            deliver(
                server,
                sink,
                shell,
                compositor,
                windows,
                picker,
                apps,
                menu,
                &WindowEvent::MenuClosed {
                    window_id,
                    open_id,
                    outcome,
                },
            );
        }
        present_menu_chain(menu, shell, compositor, windows);
    }

    /// Answer one of the **icon bar's** own chains in process: read the chosen
    /// row back through the bar's own subject and queue what it asks for.
    ///
    /// The bar keeps the vocabulary, so this asks it rather than interpreting
    /// an id here, and the answer joins the outcomes a click on the bar
    /// produces. A row id the menu never declared names nothing and is dropped
    /// (fail closed — never guessed at); a refusal is stated and the bar
    /// carries on.
    fn answer_bar_menu<S: DirectorySource>(
        subject: &MenuSubject,
        outcome: ChainOutcome,
        shell: &mut DesktopShell,
        desk: &mut DesktopMenuDesk<'_, S>,
    ) {
        let item = match outcome {
            ChainOutcome::Chosen(item) => item,
            ChainOutcome::Dismissed => return,
            ChainOutcome::Refused(reason) => {
                let _ = writeln!(Stderr, "desktop: no bar menu ({reason:?})");
                return;
            }
        };
        if let Some(response) = shell.session_mut().taskbar_mut().menu_chosen(subject, item) {
            desk.answered
                .push(tairix_desktop_session::ShellOutcome::Taskbar(response));
        }
    }

    /// Open one of the icon bar's own menus as the seat's one chain.
    ///
    /// The bar hands over a model, an anchor and which menu it is; everything
    /// after that — titling, placement, drawing, the grab, traversal,
    /// dismissal, and the one answer — is the chain's, exactly as for an
    /// application's `OpenMenu`. A refused menu is an answer stated on
    /// `stderr`, never a reason to draw one on the bar.
    fn open_bar_menu(
        request: MenuRequest,
        seat_held: bool,
        menu: &mut MenuChain,
        shell: &mut DesktopShell,
        compositor: &mut Compositor,
        windows: &SessionWindows,
    ) {
        let geom = chain_geometry(shell, compositor);
        match open_desktop_menu(
            menu,
            ChainOwner::Bar(request.subject),
            request.model,
            request.placement,
            seat_held,
            &geom,
        ) {
            Ok(()) => present_menu_chain(menu, shell, compositor, windows),
            Err(refused) => {
                let _ = writeln!(Stderr, "desktop: no bar menu ({refused:?})");
            }
        }
    }

    /// Answer the desktop's own chain in process: put a chosen row's command
    /// through the desktop model and the one action path.
    ///
    /// The model resolves the command against its own state, so a row and the
    /// equivalent gesture on the icon column produce the very same action; the
    /// session merely carries it out. A row id the menu never declared names
    /// no command and is dropped (fail closed — never guessed at).
    fn answer_backdrop_menu<S: DirectorySource>(
        outcome: ChainOutcome,
        shell: &mut DesktopShell,
        compositor: &mut Compositor,
        desk: &mut DesktopMenuDesk<'_, S>,
        now_ns: u64,
    ) {
        let command = match outcome {
            ChainOutcome::Chosen(item) => PinboardCommand::from_item(item),
            ChainOutcome::Dismissed => None,
            ChainOutcome::Refused(reason) => {
                let _ = writeln!(Stderr, "desktop: no backdrop menu ({reason:?})");
                None
            }
        };
        let Some(command) = command else {
            return;
        };
        let acted = desk.desktop.command(command, desk.associations, now_ns);
        let whole = acted.relisted
            | apply_desktop_action(
                acted.action,
                desk.pinboard,
                desk.wallpapers,
                desk.desktop,
                shell,
                compositor,
                desk.launched,
                now_ns,
            );
        if acted.relisted {
            refresh_library(shell, compositor);
            *desk.associations = desktop_associations(shell);
        }
        if whole {
            shell.present_desktop(compositor, desk.desktop);
        }
    }

    /// Reconcile the compositor against the chain, resolving the owner's and
    /// the attached window's compositor windows the session holds.
    fn present_menu_chain(
        menu: &mut MenuChain,
        shell: &mut DesktopShell,
        compositor: &mut Compositor,
        windows: &SessionWindows,
    ) {
        let owner = menu.owner_window().and_then(|id| windows.wm_id(id));
        if !shell.present_menu_chain(compositor, menu, owner) && menu.exhausted() {
            // The chain could not be drawn, so it is refused rather than left
            // half on the screen; taking it down needs the reconcile to run
            // once more, now over an empty list.
            let _ = shell.present_menu_chain(compositor, menu, owner);
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
        wallpapers: &Wallpapers,
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
            wallpapers,
            desktop,
            shell,
            compositor,
            tairix_rt::clock_get(),
        )?;
        shell.present_desktop(compositor, desktop);
        Ok(())
    }

    /// Re-resolve the icon bar's application strip from live state and push
    /// it to the bar.
    ///
    /// The strip is derived, never stored: every live served window is
    /// grouped under the process the window engine attested owns it, each
    /// process's bundle comes from the desktop's own launch records (never
    /// anything an application sent), and every application that declared a
    /// presence keeps a slot whether it owns a window or not. Slot icons are
    /// rasterised at the strip's own geometry through the shell's sandboxed
    /// pipeline, served from its one cache on every later push.
    fn refresh_app_strip(
        apps: &mut AppBarPanel,
        shell: &mut DesktopShell,
        compositor: &mut Compositor,
        server: &WindowServer<RtShmMapper>,
        windows: &SessionWindows,
        identity: &RtWindowIdentity,
        launched: &LaunchTable,
    ) {
        let owners = window_owners(shell, server, windows);
        apps.strip = apps.service.strip(&owners, |owner| {
            identity
                .pid_of(owner)
                .and_then(|pid| launched.get(pid))
                .and_then(|app| app.run_path.strip_suffix(BUNDLE_RUN_SUFFIX))
                .map(alloc::string::String::from)
        });
        let side = shell.session().taskbar().app_icon_side(compositor.scale());
        let slots = {
            let strip = core::mem::take(&mut apps.strip);
            let (cache, resolver) = shell.artwork_parts();
            let slots = apps
                .service
                .slots(&strip, &mut VfsFileReader, (resolver, cache, side));
            apps.strip = strip;
            slots
        };
        shell.set_apps(compositor, slots);
        // The strip it now holds names the applications whose icons it will
        // draw, so anything not decoded yet is asked for before the next paint
        // rather than by it.
        shell.warm_icon_artwork(compositor);
    }

    /// Every live served window as `(attested owner, task)`, in the order
    /// the windows opened.
    ///
    /// The window-channel ids the engine mints rise with each open, so
    /// walking them in id order is walking the windows in the order they
    /// opened. A window the taskbar does not track as a task (a popup) has
    /// nothing to group.
    fn window_owners(
        shell: &DesktopShell,
        server: &WindowServer<RtShmMapper>,
        windows: &SessionWindows,
    ) -> alloc::vec::Vec<(tairix_abi::ProcId, TaskId)> {
        windows
            .served()
            .filter_map(|(ipc, wm)| Some((server.owner_of(ipc)?, shell.tasks().task_for(wm)?)))
            .collect()
    }

    /// Whether the strip the bar shows still describes the live windows.
    ///
    /// Pure in-memory bookkeeping — no manifest and no icon is re-read — so
    /// it is cheap enough to run once per wake, and the strip is re-pushed
    /// only when a window actually opened, closed, or changed hands.
    fn app_strip_is_stale(
        apps: &AppBarPanel,
        shell: &DesktopShell,
        server: &WindowServer<RtShmMapper>,
        windows: &SessionWindows,
    ) -> bool {
        let owners = window_owners(shell, server, windows);
        let held: alloc::vec::Vec<(tairix_abi::ProcId, TaskId)> = apps
            .strip
            .iter()
            .flat_map(|group| group.windows.iter().map(move |&task| (group.owner, task)))
            .collect();
        let mut sorted = owners;
        sorted.sort_unstable();
        let mut held = held;
        held.sort_unstable();
        sorted != held
    }

    /// Whether the serve loop carries on after an outcome was routed.
    #[derive(Copy, Clone, Debug, Eq, PartialEq)]
    enum Routed {
        /// Keep serving.
        Continue,
        /// The user asked to log out. The loop unwinds so the one
        /// owner-checked release runs and the login supervisor prompts again.
        EndSession,
        /// The user asked to switch to another account. The loop asks the
        /// session authority and, only if it accepts, gives up the screen —
        /// the session itself keeps running, so nothing unwinds here.
        SwitchUser,
    }

    /// Ask the authority to record this session as background and, only on
    /// its acceptance, give the screen up through `screen`.
    ///
    /// Answers whether the session is now background. A refusal — the
    /// authority said no, could not be reached, or answered something that
    /// is not a verdict — is stated to the user and changes nothing: the
    /// desktop keeps the seat and keeps drawing.
    fn step_aside<S: DirectorySource>(
        switch: &mut SwitchUser,
        screen: SessionScreen<'_, S>,
    ) -> bool {
        let mut screen = screen;
        match switch.step_aside(&mut RtSessionAuthority, &mut screen) {
            Ok(()) => true,
            Err(refusal) => {
                let _ = writeln!(Stderr, "desktop: {}", refusal.reason());
                false
            }
        }
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
        elevate: &mut ElevatePrompt,
        lock: &mut ScreenLock,
        menu: &mut MenuChain,
        account: &str,
        launched: &mut LaunchTable,
        apps: &mut AppBarPanel,
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
                                &mut apps.service,
                                menu,
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
                                &mut apps.service,
                                menu,
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
                            &mut apps.service,
                            menu,
                            &WindowEvent::Pointer {
                                window_id: id,
                                x,
                                y,
                                action: PointerAction::Pressed(
                                    tairix_abi::input::PointerButtonCode::Primary,
                                ),
                                modifiers: pointer_modifiers(shell),
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
                                picker, apps, menu,
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
                    // A press on the showing credential prompt answers it the
                    // same way: only the continuing button offers what was
                    // typed, and the broker decides.
                    if elevate.wm_id() == Some(window) {
                        let outcome =
                            elevate.handle_click(local, &mut RtElevator, shell, compositor);
                        report_elevation(outcome);
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
                                &mut apps.service,
                                menu,
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
                                &mut apps.service,
                                menu,
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
                            &mut apps.service,
                            menu,
                            &WindowEvent::Pointer {
                                window_id: id,
                                x,
                                y,
                                action: PointerAction::Pressed(
                                    tairix_abi::input::PointerButtonCode::Secondary,
                                ),
                                modifiers: pointer_modifiers(shell),
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
                            &mut apps.service,
                            menu,
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
                                    picker, apps, menu,
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
                    } else if elevate.wm_id() == Some(window) {
                        // The credential prompt consumes its own keys, so a
                        // password is typed into it and never into whatever
                        // held focus behind it.
                        if let Some(record) = key {
                            let outcome =
                                elevate.handle_key(&record, &mut RtElevator, shell, compositor);
                            report_elevation(outcome);
                        }
                    } else if let (Some(id), Some(record)) = (windows.ipc_id(window), key) {
                        deliver(
                            server,
                            sink,
                            shell,
                            compositor,
                            windows,
                            picker,
                            &mut apps.service,
                            menu,
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
                                &mut apps.service,
                                menu,
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
                            &mut apps.service,
                            menu,
                            &event,
                        );
                    }
                }
                // A secondary press landed on a title-bar control: the window
                // manager changed nothing, so the only outcome is the app-ward
                // event the one shared rule yields (Close→AlternateCloseRequested;
                // every other control, and a window the session itself owns,
                // yields none and the press does nothing at all).
                InputResponse::WindowControlAlternate { window, control } => {
                    if let Some(event) = window_control_alternate_event(control, window, windows) {
                        deliver(
                            server,
                            sink,
                            shell,
                            compositor,
                            windows,
                            picker,
                            &mut apps.service,
                            menu,
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
                            &mut apps.service,
                            menu,
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
                            &mut apps.service,
                            menu,
                            &WindowEvent::Pointer {
                                window_id: id,
                                x,
                                y,
                                action: PointerAction::Moved,
                                modifiers: pointer_modifiers(shell),
                            },
                        );
                    }
                }
                // A primary release that ended a client pointer grab: forward
                // it so an in-content click or drag completes (a tab or combo
                // selection, a released scrollbar thumb).
                InputResponse::ClientPointerReleased { window, local } => {
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
                            &mut apps.service,
                            menu,
                            &WindowEvent::Pointer {
                                window_id: id,
                                x,
                                y,
                                action: PointerAction::Released(
                                    tairix_abi::input::PointerButtonCode::Primary,
                                ),
                                modifiers: pointer_modifiers(shell),
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
            ShellOutcome::Taskbar(TaskbarResponse::LibraryLaunch { entry }) => {
                // Resolve the chosen entry's bundle through the catalog the
                // popup was handed and spawn its `Run` binary: admitted
                // immediately, loaded on its own task, refusal reported
                // (synchronously here or by the reap), desktop carries on.
                launch_library_entry(shell, &entry, launched);
            }
            ShellOutcome::Taskbar(TaskbarResponse::OpenMenu(request)) => {
                // The bar draws no menu: it hands over a model and an anchor,
                // and the seat's one chain places, draws, grabs and answers it
                // — the same service an application's `OpenMenu` reaches.
                open_bar_menu(
                    request,
                    seat_held(lock, picker),
                    menu,
                    shell,
                    compositor,
                    windows,
                );
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
            ShellOutcome::Taskbar(TaskbarResponse::AppDefault { app }) => {
                // The application declared that it handles the primary click
                // itself, so the click is relayed to it and the session does
                // nothing else — one click, one actor.
                relay_app_bar(
                    apps,
                    app,
                    shell,
                    compositor,
                    server,
                    sink,
                    windows,
                    picker,
                    menu,
                    &WindowEvent::AppBarDefault,
                );
            }
            ShellOutcome::Taskbar(TaskbarResponse::AppRaise { app }) => {
                // No declared default action: raise the application's most
                // recently used window. The bar already refused to report
                // this for an application with none, so there is always one
                // to raise; a window the bridge has since lost changes
                // nothing (fail closed).
                if let Some(window) = mru_window(apps, app, shell) {
                    shell.raise_window(compositor, window);
                }
            }
            ShellOutcome::Taskbar(TaskbarResponse::AppMenuChosen { app, item }) => {
                // The row id is the application's own and the session never
                // interprets one: it is relayed straight back to the process
                // that declared the menu.
                relay_app_bar(
                    apps,
                    app,
                    shell,
                    compositor,
                    server,
                    sink,
                    windows,
                    picker,
                    menu,
                    &WindowEvent::AppBarMenu { item },
                );
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
                elevate.repaint(shell, compositor);
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
                    &mut apps.service,
                    menu,
                );
            }
            ShellOutcome::Taskbar(TaskbarResponse::LockSession) => {
                // Secure the screen. The prompt goes down first: an
                // unanswered question must not sit behind a lock where the
                // user cannot see what they are agreeing to. A lock that
                // could not be put up says so rather than leaving the user
                // believing the screen is secured.
                confirm.abandon(shell, compositor);
                elevate.abandon(shell, compositor);
                if !lock.engage(account, shell, compositor) {
                    io::write_stderr_line("desktop: could not lock the screen; it is still open");
                }
            }
            ShellOutcome::Taskbar(TaskbarResponse::SwitchUser) => {
                // Step aside for another account. The prompt goes down
                // first: an unanswered question must not be left on a screen
                // that is about to belong to somebody else. The switch
                // itself is the loop's, which owns the frame region the
                // session gives back.
                confirm.abandon(shell, compositor);
                elevate.abandon(shell, compositor);
                return Routed::SwitchUser;
            }
            ShellOutcome::Taskbar(TaskbarResponse::LogOut) => {
                // The user asked for the session to end: take the prompt down
                // unanswered (so nothing irreversible follows a log-out) and
                // unwind through the one owner-checked release.
                confirm.abandon(shell, compositor);
                elevate.abandon(shell, compositor);
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
            ShellOutcome::Taskbar(TaskbarResponse::SetDateTime) => {
                // The session holds no authority to set a clock and never
                // will: it asks for an account that does, and the console's
                // broker re-authenticates it and starts the application
                // itself. A prompt that cannot be shown asks nothing and
                // sets nothing.
                if elevate.ask(DATETIME_RUN_PATH, SET_TIME_PURPOSE, shell, compositor) {
                    // The prompt is focused and on screen: announce it so a
                    // host that must type into the fields waits on a real
                    // surface rather than racing the click that asked for it.
                    log(
                        &LOG_SINK,
                        &LogEvent {
                            level: LogLevel::Info,
                            id: ELEVATE_PROMPT_SHOWN,
                            message: ELEVATE_PROMPT_SHOWN_MESSAGE,
                            fields: &[],
                        },
                    );
                } else {
                    io::write_stderr_line(
                        "desktop: could not ask for an account; the clock was not changed",
                    );
                }
            }
            // An event no router acted on; outcomes the shell has already
            // fully applied with its own state (the click-to-activate/minimise
            // rule, clearing a dismissed notification from the model, the
            // popup's own open/close, opening the hover picker out of the
            // thumbnails it prepared); and the desktop shortcut, which
            // `route_desktop` — the owner of that folder, its icons, and its
            // one creation path — has already made and shown. Nothing here
            // needs a capability this side of the routing holds, so the
            // session adds nothing. Listed rather than caught by a wildcard
            // so a new outcome fails the build instead of being dropped in
            // silence.
            ShellOutcome::Ignored
            | ShellOutcome::Taskbar(
                TaskbarResponse::Ignored
                | TaskbarResponse::LibraryDismissed
                | TaskbarResponse::WindowChosen { .. }
                | TaskbarResponse::DismissNotification { .. }
                | TaskbarResponse::ShowWindowPicker { .. }
                | TaskbarResponse::CreateDesktopShortcut { .. },
            ) => {}
        }
        Routed::Continue
    }

    /// What the credential prompt says the account is wanted for.
    const SET_TIME_PURPOSE: &str = "Setting the date and time needs an account that may.";

    /// The production elevation seam: post the offered credentials to this
    /// console's broker and let it re-authenticate and start the program.
    ///
    /// The desktop never authenticates anybody and never spawns the elevated
    /// program itself; it carries the offer and reads the verdict.
    struct RtElevator;

    impl Elevator for RtElevator {
        fn launch(&mut self, username: &str, password: &str, program: &str) -> Result<i32, Errno> {
            match tairix_rt::elevate(&ElevateRequest::Launch {
                username,
                password,
                program,
            })? {
                ElevateReply::Launched { pid } => Ok(pid),
                ElevateReply::Refused(err) => Err(err),
                // `Completed` answers a `Run` request and `Verified` a
                // `Verify` one, never a `Launch`. A broker that sent either
                // is not speaking this protocol, and nothing was started on
                // a reply the session did not understand.
                ElevateReply::Completed { .. } | ElevateReply::Verified => Err(Errno::OutOfRange),
            }
        }
    }

    /// State a concluded elevation on `stderr`.
    ///
    /// The started program is deliberately **not** entered in the launch
    /// table. That table maps a child the session started to the bundle it
    /// came from, and an entry leaves it only when the session *reaps* that
    /// child — but an elevated program is login's child, so the reap never
    /// comes and the entry would outlive the program for the life of the
    /// session, claiming a bundle still runs. Its window therefore resolves
    /// its identity exactly as any other window the session did not launch
    /// does (a program started from a shell, say): one uniform behaviour,
    /// rather than a special case that leaks.
    ///
    /// A refusal is already stated in the prompt, which stays up for another
    /// attempt, so nothing is reported for one here. A cancellation is
    /// reported, since a user who asked to set the clock and saw nothing
    /// happen deserves to be told nothing was set.
    fn report_elevation(outcome: PromptOutcome) {
        match outcome {
            PromptOutcome::Started { .. } | PromptOutcome::Pending => {}
            PromptOutcome::Cancelled => {
                io::write_stderr_line("desktop: the clock was not changed");
            }
        }
    }

    /// Relay a confirmed power transition over the production mailbox and
    /// state loudly why nothing happened when it could not be relayed.
    fn report_power_relay(answer: Answer, switchboard: Option<u64>) {
        if let Some(reason) = relay_power(answer, switchboard, &mut RtSwitchboardMailbox) {
            let _ = writeln!(Stderr, "desktop: {reason}");
        }
    }

    /// Relay one icon-bar event to the application whose slot it landed on.
    ///
    /// The destination is the declaration the window engine recorded for the
    /// slot's attested owner, never anything the event carries, so a bar
    /// event can only ever reach the process that asked to be on the bar. An
    /// application with no declaration — a slot the session derived from its
    /// windows alone — has nothing to relay to, and a refused delivery tears
    /// its windows down exactly as any other refused send does.
    #[allow(clippy::too_many_arguments)] // The delivery path's whole mutable state, threaded explicitly.
    fn relay_app_bar<S: DirectorySource, F: FnMut() -> S>(
        apps: &mut AppBarPanel,
        app: usize,
        shell: &mut DesktopShell,
        compositor: &mut Compositor,
        server: &mut WindowServer<RtShmMapper>,
        sink: &mut RtEventSink,
        windows: &mut SessionWindows,
        picker: &mut SessionPicker<S, F>,
        menu: &mut MenuChain,
        event: &WindowEvent,
    ) {
        let Some(owner) = apps.strip.get(app).map(|group| group.owner) else {
            return;
        };
        if let Err(Errno::NotFound) = server.deliver_app_event(sink, owner, event) {
            // The declaration is gone with the process: its windows go too,
            // exactly as a refused window-scoped send tears them down.
            let mut bridge = ShellWindowHost {
                shell,
                compositor,
                windows,
                picker,
                apps: &mut apps.service,
                menu,
                // This bridge tears windows down and never serves an
                // `OpenMenu`, so it cannot vouch for the seat and says so.
                seat_held: true,
            };
            server.client_exited(&mut bridge, owner);
        }
    }

    /// The window the icon-bar slot at `app` raises: its application's most
    /// recently used one.
    ///
    /// "Most recently used" is the window list's own answer — the focused
    /// window when this application owns it, else the one it handed focus to
    /// last, else the newest it opened — so the bar and the Switchboard
    /// capsule agree on what "the last window you were in" means.
    fn mru_window(
        apps: &AppBarPanel,
        app: usize,
        shell: &DesktopShell,
    ) -> Option<tairix_wm::WindowId> {
        let group = apps.strip.get(app)?;
        let tasks = shell.session().taskbar().tasks();
        let owned = |task: Option<TaskId>| task.filter(|task| group.windows.contains(task));
        let task = owned(tasks.focused())
            .or_else(|| owned(tasks.previous()))
            .or_else(|| group.windows.last().copied())?;
        shell.tasks().window_for(task)
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
        wallpapers: &Wallpapers,
        desktop: &mut Desktop<S>,
        shell: &mut DesktopShell,
        compositor: &mut Compositor,
        windows: &SessionWindows,
        menu: &mut MenuChain,
        seat_held: bool,
        launched: &mut LaunchTable,
        associations: &mut alloc::vec::Vec<AppAssociation>,
        now_ns: u64,
    ) {
        let pointer = shell.router().pointer();
        let layout = shell.desktop_layout(compositor, desktop);
        // Every gesture reports the icon cells it changed here, and only
        // those are repainted: a click that moves focus between a window and
        // the desktop must cost a focus ring, not a screen.
        let mut damage = Region::new();
        // The desktop holds the keyboard exactly when no window does, so the
        // focus ring follows the window manager's one notion of focus rather
        // than a second one kept here.
        desktop.set_focused(shell.router().focused().is_none(), &layout, &mut damage);
        let acted = match outcome {
            tairix_desktop_session::ShellOutcome::WindowManager(response) => match response {
                InputResponse::DesktopPointerMoved => {
                    desktop.pointer_moved(pointer, &layout, now_ns, &mut damage)
                }
                InputResponse::DesktopPressed => {
                    desktop.press(pointer, &layout, now_ns, associations, &mut damage)
                }
                InputResponse::DesktopSecondaryPressed => {
                    // The backdrop menu is the seat's one chain, so this asks
                    // for it directly rather than naming an action: it is the
                    // desktop's own model handed to the one service, exactly
                    // as an application's `OpenMenu` is.
                    let on_icon = desktop.context_press(pointer, &layout, &mut damage);
                    open_backdrop_menu(
                        pointer, on_icon, seat_held, desktop, menu, shell, compositor, windows,
                    );
                    DesktopOutcome::ignored()
                }
                InputResponse::DesktopKey { key, pressed, .. } => {
                    desktop.key(*key, *pressed, &layout, associations, &mut damage)
                }
                _ => departed(desktop, compositor, pointer, &layout, &mut damage),
            },
            _ => departed(desktop, compositor, pointer, &layout, &mut damage),
        };
        // A shortcut the program library's row menu asked for is a change to
        // *this* folder, so it is honoured beside the desktop's own gestures
        // rather than through a second creation path. The two sources are
        // exclusive — a taskbar outcome is never also a desktop gesture — and
        // `or` says so without discarding either.
        let action = shortcut_asked(outcome, shell, desktop).or(acted.action);
        // A re-list moved the icons themselves, so no cell of the layout the
        // gesture reported against describes the new column: that, and the
        // settings and folder edits `apply_desktop_action` performs, are the
        // changes that genuinely repaint the whole layer.
        let whole = acted.relisted
            | apply_desktop_action(
                action, pinboard, wallpapers, desktop, shell, compositor, launched, now_ns,
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
        if whole {
            shell.present_desktop(compositor, desktop);
        } else if !damage.is_empty() {
            shell.present_desktop_area(compositor, desktop, &damage);
        }
    }

    /// Open the backdrop menu as the seat's one chain, anchored at the press
    /// that asked for it.
    ///
    /// The desktop's own menu is a client of the menu service exactly as an
    /// application's is; the only difference is that its model is built here
    /// rather than decoded from the wire, so its rows may state things
    /// (a command the *system* lacks the authority for) that an application
    /// structurally cannot. The chain places, draws, grabs, traverses and
    /// dismisses it, and its one answer arrives at the session's single
    /// delivery point like every other chain's.
    ///
    /// A model the chain will not show is reported and opens nothing: a
    /// refused menu is an answer, never a reason to draw one here.
    ///
    /// Whether a surface a menu may not displace holds the seat: the screen
    /// lock, or the trusted picker. One definition, because every direction a
    /// chain arrives from consults it — an application's `OpenMenu` over the
    /// window channel, the desktop's own backdrop press, and a press on the
    /// icon bar.
    fn seat_held<S: DirectorySource, F: FnMut() -> S>(
        lock: &ScreenLock,
        picker: &SessionPicker<S, F>,
    ) -> bool {
        lock.is_locked() || picker.wm_id().is_some()
    }

    #[allow(clippy::too_many_arguments)] // The chain's whole mutable surround, threaded explicitly.
    fn open_backdrop_menu<S: DirectorySource>(
        at: Point,
        on_icon: bool,
        seat_held: bool,
        desktop: &Desktop<S>,
        menu: &mut MenuChain,
        shell: &mut DesktopShell,
        compositor: &mut Compositor,
        windows: &SessionWindows,
    ) {
        let model = pinboard::model(on_icon, desktop.settings());
        let geom = chain_geometry(shell, compositor);
        match open_desktop_menu(
            menu,
            ChainOwner::Backdrop,
            model,
            window_menu_placement(Rect::new(at.x, at.y, 0, 0)),
            seat_held,
            &geom,
        ) {
            Ok(()) => present_menu_chain(menu, shell, compositor, windows),
            Err(refused) => {
                let _ = writeln!(Stderr, "desktop: no backdrop menu ({refused:?})");
            }
        }
    }

    /// The action a program-library *Create Desktop Shortcut* row asks for,
    /// or `None` for every other outcome.
    ///
    /// The catalog the popup was handed is the one the shortcut is resolved
    /// against, so a row can never launch one bundle and link another.
    fn shortcut_asked<S: DirectorySource>(
        outcome: &tairix_desktop_session::ShellOutcome,
        shell: &DesktopShell,
        desktop: &Desktop<S>,
    ) -> Option<DesktopAction> {
        let tairix_desktop_session::ShellOutcome::Taskbar(TaskbarResponse::CreateDesktopShortcut {
            entry,
        }) = outcome
        else {
            return None;
        };
        Some(desktop.shortcut_to(shell.session().taskbar().library().catalog(), entry))
    }

    /// Carry out one desktop action, whether a gesture on the icon column
    /// named it or a chosen backdrop-menu row did, and answer whether the
    /// whole desktop layer must be repainted.
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
        wallpapers: &Wallpapers,
        desktop: &mut Desktop<S>,
        shell: &mut DesktopShell,
        compositor: &mut Compositor,
        launched: &mut LaunchTable,
        now_ns: u64,
    ) -> bool {
        match action {
            Some(DesktopAction::Activate(DesktopActivation::OpenFolder { path })) => {
                let _ = record_launch(
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
                let _ = record_launch(
                    launched,
                    spawn_app(run_path.as_bytes(), &args),
                    &label,
                    &run_path,
                );
                false
            }
            Some(DesktopAction::CreateFolder { path }) => {
                let ret = tairix_rt::fs_mkdir(path.as_bytes());
                settle_desktop_create(&path, ret, desktop, now_ns)
            }
            // Target first, then link: the stored target is data the kernel
            // never resolves here, and a name already taken is the kernel's
            // own refusal — this never replaces one.
            Some(DesktopAction::CreateShortcut { link, target }) => {
                let ret = tairix_rt::fs_symlink(target.as_bytes(), link.as_bytes());
                settle_desktop_create(&link, ret, desktop, now_ns)
            }
            Some(DesktopAction::AdoptSettings(settings)) => adopt_pinboard_settings(
                settings, pinboard, wallpapers, desktop, shell, compositor, now_ns,
            )
            .is_ok(),
            Some(DesktopAction::ChangeBackground) => {
                let _ = record_launch(
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

    /// Settle a name the session asked the filesystem to create at `path`,
    /// whose call answered `ret`, and show the result — answering whether the
    /// icon column changed.
    ///
    /// Both names the desktop creates — a folder and a shortcut — end here,
    /// so a refusal reads the same whichever asked for it and the fresh name
    /// appears the same way. A refusal (the name is already taken, the
    /// desktop folder is not writable, the volume is full) is stated on
    /// `stderr` with the kernel's own reason and leaves the desktop exactly
    /// as it was.
    fn settle_desktop_create<S: DirectorySource>(
        path: &str,
        ret: i64,
        desktop: &mut Desktop<S>,
        now_ns: u64,
    ) -> bool {
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
    /// The [`Errno`] the app-data service refused the publish with; nothing
    /// was adopted.
    #[allow(clippy::too_many_arguments)] // The desktop's whole settings state, threaded explicitly.
    fn adopt_pinboard_settings<S: DirectorySource>(
        settings: PinboardSettings,
        pinboard: &mut PinboardPanel,
        wallpapers: &Wallpapers,
        desktop: &mut Desktop<S>,
        shell: &mut DesktopShell,
        compositor: &mut Compositor,
        now_ns: u64,
    ) -> Result<(), Errno> {
        if let Err(err) = persist_pinboard(&mut RtHost, &settings) {
            let _ = writeln!(
                Stderr,
                "desktop: the desktop settings could not be published ({err:?})"
            );
            return Err(err);
        }
        let Some(change) = desktop.apply_settings(settings) else {
            return Ok(());
        };
        if change.relist {
            desktop.relist(now_ns);
        }
        if change.wallpaper {
            prepare_wallpaper(pinboard, wallpapers, shell, desktop, compositor, now_ns);
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
        layout: &GridView,
        damage: &mut Region,
    ) -> DesktopOutcome {
        if compositor.window_at(pointer).is_some() {
            return desktop.pointer_left(layout, damage);
        }
        DesktopOutcome::ignored()
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
        let catalog = shell.session().taskbar().library().catalog();
        let chosen = match catalogued(catalog, entry) {
            Ok(chosen) => chosen,
            Err(reason) => {
                let _ = write!(Stderr, "{reason}");
                return;
            }
        };
        let run_path = alloc::format!("{}/Run", chosen.bundle().as_str());
        let label = chosen.name().as_str();
        let _ = record_launch(
            launched,
            spawn_app(run_path.as_bytes(), &[]),
            label,
            &run_path,
        );
    }

    /// (Re)load the program library from its two layers — the machine-wide
    /// store on the volume and the account's overlay as the library-admin
    /// command publishes it — and hand the resolved catalog to the taskbar's
    /// popup, reporting each unusable layer loudly on `stderr`.
    fn refresh_library(shell: &mut DesktopShell, compositor: &mut Compositor) {
        let loaded = load_library(&mut VfsFileReader, &mut RtHost);
        for warning in &loaded.warnings {
            let _ = write!(Stderr, "{warning}");
        }
        shell.set_library(compositor, loaded.catalog);
        shell.warm_icon_artwork(compositor);
    }

    /// The session's live file-reading seam: whole-file reads through the
    /// kernel VFS under the session's own kernel-attested identity, bounded
    /// just past the configuration-document cap — the largest document the
    /// session reads through this seam is the machine-wide program library,
    /// which is one of those — so no store can make the desktop slurp an
    /// arbitrarily large file (the loader then refuses the oversize).
    struct VfsFileReader;

    impl SessionFileReader for VfsFileReader {
        fn read(&mut self, path: &str) -> Result<alloc::vec::Vec<u8>, Errno> {
            read_file(path, tairix_appconf::MAX_DOCUMENT_LEN)
        }
    }
    /// Read the whole file at `path` through the kernel VFS under the
    /// session's own kernel-attested identity, stopping one chunk past `cap`
    /// so no file can make the desktop slurp an arbitrary number of bytes.
    ///
    /// The one read path every file the session reads goes through — the
    /// machine-wide program-library store at the configuration-document cap,
    /// the user's wallpaper at the wallpaper cap — so a second,
    /// differently-bounded reader cannot exist. An answer longer than `cap` is the caller's
    /// whole-document refusal to state.
    ///
    /// The streaming is the runtime's one whole-file policy
    /// ([`tairix_rt::read_fd_to_end`]), so the desktop cannot drift to a chunk
    /// size of its own: a wallpaper master is megabytes, and reading one a
    /// kilobyte per syscall spent thousands of traps — seconds of them on real
    /// storage — before the decoder saw a byte.
    fn read_file(path: &str, cap: usize) -> Result<alloc::vec::Vec<u8>, Errno> {
        let ret = tairix_rt::fs_open(path.as_bytes(), OpenFlags::READ);
        if ret < 0 {
            return Err(Errno::from_syscall(ret));
        }
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        // `ret >= 0` checked above; it is a descriptor number.
        let fd = ret as u32;
        let outcome = tairix_rt::read_fd_to_end(fd, cap).map_err(Errno::from_syscall);
        let _ = tairix_rt::fs_close(fd);
        outcome
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
    /// carries no authority.
    ///
    /// `args` are the app's arguments alone; the program itself is named by
    /// [`launch_argv`], which every launch goes through so no argument is
    /// read as the program's own name and lost.
    fn spawn_app(path: &[u8], args: &[&[u8]]) -> i64 {
        let count = tairix_rt::env_count();
        let mut env: alloc::vec::Vec<&[u8]> = alloc::vec::Vec::with_capacity(count as usize);
        for index in 0..count {
            if let Some(entry) = tairix_rt::env(index) {
                env.push(entry);
            }
        }
        tairix_rt::spawn_with(
            path,
            CONSOLE_INHERIT,
            SPAWN_UID_INHERIT,
            &launch_argv(path, args),
            &env,
        )
    }

    /// Record a just-issued launch, answering with the pid recorded.
    ///
    /// Asynchronous launch admits the child and returns its PID before the
    /// image is loaded, so a successful admit only *starts* the launch:
    /// remember the PID under its display label (so the `CHILD_TOKEN` reap
    /// can name the app if its load is later refused via the child's
    /// reserved-`LOAD_*` exit status) and its spawn path (its attested
    /// bundle identity). Answering with that pid is what lets a caller act
    /// on the child it just started instead of searching the table for it.
    ///
    /// A result that is not a task id — a stripped spawn capability, a
    /// malformed path, any refusal decided before a child exists — records
    /// nothing, is reported fail-loud at once, and answers `None`. A
    /// denied optional launch never ends the session.
    fn record_launch(
        launched: &mut LaunchTable,
        ret: i64,
        label: &str,
        run_path: &str,
    ) -> Option<u64> {
        let Some(pid) = admitted_pid(ret) else {
            let _ = writeln!(Stderr, "desktop: {label} launch refused");
            return None;
        };
        launched.record(pid, label, run_path);
        Some(pid)
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
        apps: &mut AppBarPanel,
        menu: &mut MenuChain,
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
            &mut apps.service,
            menu,
            &event,
        );
    }

    /// Open `path` read-only under the session's own authority and mint a
    /// one-shot delegation to the attested owner of window `window_id`,
    /// returning the `fd_redeem` handle. The session's descriptor is
    /// closed either way — the delegation record is self-contained — and
    /// every refusal answers `None` (fail closed, nothing delegated).
    ///
    /// The delegation carries no write extent: what the user chose is a
    /// document to read, and a read-only grant has no length to bound.
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
        let handle = tairix_rt::fd_grant(fd, pid, 0);
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
        apps: &mut dyn AppBarBridge,
        menu: &mut MenuChain,
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
                apps,
                menu,
                &WindowEvent::RedrawRequested { window_id },
            );
        }
    }

    /// Tell every client whose window content was released while nobody could
    /// see it that the session has let go of its frames, unmapping this side
    /// first.
    ///
    /// Both sides have to let go for the pages to be freed: the compositor's
    /// copy of a window's pixels is one of three, and the other two — the
    /// app's render target and the frame region it presents from — are the
    /// client's. Unmapping here before the event goes out means any present
    /// that crosses it is refused typed rather than writing into a mapping
    /// that is about to vanish; the client library re-attaches on the paint
    /// that follows the redraw request the window's next showing sends.
    #[allow(clippy::too_many_arguments)] // The delivery path's whole mutable state, threaded explicitly.
    fn deliver_released_notices<S: DirectorySource, F: FnMut() -> S>(
        server: &mut WindowServer<RtShmMapper>,
        sink: &mut RtEventSink,
        shell: &mut DesktopShell,
        compositor: &mut Compositor,
        windows: &mut SessionWindows,
        picker: &mut SessionPicker<S, F>,
        apps: &mut dyn AppBarBridge,
        menu: &mut MenuChain,
    ) {
        for wm in compositor.take_released_notices() {
            let Some(window_id) = windows.ipc_id(wm) else {
                continue;
            };
            let bytes = server.release_frames(window_id);
            // Its pixels are gone, so the window is awaiting them again and
            // the shown announcement must be re-earned by the frame that
            // brings them back.
            windows.content_released(window_id);
            log(
                &LOG_SINK,
                &LogEvent {
                    level: LogLevel::Info,
                    id: CONTENT_RELEASED,
                    message: CONTENT_RELEASED_MESSAGE,
                    fields: &[
                        LogField {
                            key: "window",
                            value: LogFieldValue::UnsignedInt(window_id),
                        },
                        LogField {
                            key: "bytes",
                            value: LogFieldValue::UnsignedInt(bytes),
                        },
                    ],
                },
            );
            deliver(
                server,
                sink,
                shell,
                compositor,
                windows,
                picker,
                apps,
                menu,
                &WindowEvent::ContentReleased { window_id },
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
        apps: &mut dyn AppBarBridge,
        menu: &mut MenuChain,
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
                apps,
                menu,
                &WindowEvent::DesktopChanged { window_id, desktop },
            );
        }
    }

    /// The wire modifiers a pointer event delivered right now carries: the
    /// seat's held set, in the ABI vocabulary.
    ///
    /// A modifier key reaches no surface as a key, so an application cannot
    /// track the seat's state itself; stamping it here is what lets one
    /// qualify a click (a shift-click) by what is held.
    fn pointer_modifiers(shell: &DesktopShell) -> AbiModifiers {
        modifiers_to_abi(shell.modifiers())
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
        apps: &mut dyn AppBarBridge,
        menu: &mut MenuChain,
        event: &WindowEvent,
    ) {
        // Window-scoped only: an icon-bar event names no window and goes
        // out through `relay_app_bar`, which addresses the declaration
        // instead.
        let Some(owner) = event.window_id().and_then(|id| server.owner_of(id)) else {
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
                apps,
                menu,
                // A teardown serves no `OpenMenu`, so this bridge cannot
                // vouch for the seat and says so rather than claiming it
                // free.
                seat_held: true,
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
        //
        // A clean exit is a session the user ended, and the authority brings
        // the login screen back on this seat, so the screen is handed on
        // cleared. Any other exit is a failure whose reason belongs on the
        // console, which therefore takes the screen back.
        let next = if code == 0 {
            ReleaseSurface::Handover
        } else {
            ReleaseSurface::Text
        };
        let _ = tairix_rt::display_release(SEAT_PRIMARY, next);
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
