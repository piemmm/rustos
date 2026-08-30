//! The `Run` entry-point binary of the Switchboard monitor service,
//! installed at `/System/Services/switchboard.app/Run`
//! (`plans/NEW-TASKBAR.md` T10/T11) — spawned by the desktop session as the
//! logged-in user (never PID 1), so the tray-overview authority
//! (`CAP_SYSINFO_GLOBAL`/`CAP_SYSINFO_KERNEL`) never has to grow the
//! session's own manifest.
//!
//! This is a **pure-Rust** program: TAIRiX is Rust-only, so it links the
//! Rust userland runtime `tairix-rt` — never the C ABI. `tairix-rt`
//! provides `_start`, the panic handler, the `#[global_allocator]`,
//! `ipc_call`, `ipc_recv`, `port_bind`, the shared-memory and wait-set
//! syscalls, `clock_get`, `cap_query`, `signal`, `signal_intake`, and
//! `stderr`; `tairix-procinfo`'s `IpcTransport` (enabled through its own
//! `program` feature) is the production `tairix_procinfo::Transport` the
//! sampler queries through.
//!
//! # What this service does
//!
//! Everything with behaviour worth testing lives in the host-tested
//! `tairix_switchboard` library: the sampler, the tray-summary derivation,
//! the publish gate, the live overview model, and the panel's window
//! lifecycle. This binary is the wiring the host cannot run:
//!
//! * it learns its own kernel-attested identity (`self_origin`) and binds
//!   the session's per-instance command mailbox under it;
//! * it learns the desktop session's own identity from the reply to its
//!   first publish, and authenticates every later command against that
//!   attested identity rather than any claim on the wire;
//! * it parks in **one** `waitset_wait` per iteration covering its
//!   termination signal, that command mailbox, and — only while a window is
//!   open — the window's event mailbox, with a timeout equal to the time
//!   remaining until the next sample is due. It never polls and never
//!   sleeps in a loop;
//! * it creates, paints, resizes, and destroys the overview window, and
//!   translates the window channel's wire input into the shared desktop
//!   input vocabulary the composition consumes.
//!
//! The tray summary is sampled and published on every cycle whether or not
//! a window is open: the window is a view onto a monitor that never stops
//! monitoring.
//!
//! A refusal from the session (`NotFound` — no session bound the endpoint,
//! or it exited; `PermissionDenied` — the session refused this instance's
//! identity, e.g. after a session restart left it orphaned) is a **clean**
//! exit: the service has no purpose without a session to report to. Any
//! other publish failure is retried on the next cycle, up to a small
//! bounded count in a row, after which the service gives up rather than
//! retrying forever. Every abnormal exit states its reason on `stderr`
//! first, and every refused *optional* action is stated on `stderr` without
//! ending the program.
//!
//! On the host it is an inert stub so `cargo build --workspace`, clippy,
//! and fmt still cover the file.

#![cfg_attr(all(freestanding, feature = "program"), no_std)]
#![cfg_attr(all(freestanding, feature = "program"), no_main)]
#![deny(missing_docs)]

// --- Pure-Rust program --------------------------------------------------
// Compiled only for the freestanding service binary, which links the
// optional `tairix-rt` runtime through the default `program` feature. The
// host tooling builds only this crate's *library*, so this module (and
// `tairix-rt`) never enter those builds.
#[cfg(all(freestanding, feature = "program"))]
mod program {
    use tairix_abi::driver::display::{DisplayFormat, DisplayMode};
    use tairix_abi::input::{KeyInput, KeyValue, NamedKeyCode, PointerButtonCode};
    use tairix_abi::reply::decode_status_reply;
    use tairix_abi::switchboard_ipc::{
        command_endpoint_for, decode_publish_reply, SwitchboardCommand, SwitchboardRequest,
        TraySummary, SWITCHBOARD_ENDPOINT, SWITCHBOARD_PUBLISH_REPLY_LEN,
    };
    use tairix_abi::window_ipc::{PointerAction, WindowEvent, WINDOW_ENDPOINT};
    use tairix_abi::{
        CapabilityId, CapabilityQuery, Errno, Origin, PowerAction, ProcId, SchedPriority, Signal,
        SignalIntakeOp, WaitSetOp, WaitSourceKind, ORIGIN_WIRE_LEN,
    };
    use tairix_display::{winframe, SERIAL};
    use tairix_font::BitmapFont;
    use tairix_geometry::{Rect, Region, Scale};
    use tairix_input::{InputEvent, Key, NamedKey, PointerButton};
    use tairix_log::{
        log, Event as LogEvent, Field as LogField, FieldValue as LogFieldValue, Level as LogLevel,
    };
    use tairix_procinfo::IpcTransport;
    use tairix_raster::Surface;
    use tairix_rt::io::{self, Stderr, Write};
    use tairix_switchboard::{
        authenticate_command, probe_scopes, refusal_notice, CycleOutcome, DegradedField,
        RenderInputs, Service, ServiceHost, Switchboard, SwitchboardAction, WaitToken, PANEL_TITLE,
        SESSION_REFUSED, WIN_HEIGHT, WIN_SIZING, WIN_WIDTH,
    };
    use tairix_theme::{TextRole, Theme, ThemeRegistry};
    use tairix_window::{
        pointer_point, present_damage, Desktop, Repaint, WindowClient, WindowFrames,
        WindowTransport,
    };

    /// Frames in the shared region. The window protocol serialises a
    /// present (the app is parked in the call while the session reads), so
    /// a single frame is race-free.
    const FRAME_COUNT: u32 = 1;

    /// The command mailbox's bounded capacity: the session sends a panel
    /// open on a click and a seat report when the seat's health changes, so
    /// a small queue is ample.
    const COMMAND_CAPACITY: usize = 8;

    /// Exit code when the process cannot learn its own identity, enable
    /// signal observation, or build and arm its wait-set: with no parking
    /// source the service cannot run its tickless loop at all.
    const EXIT_NO_WAIT_SOURCE: i32 = 1;

    /// Exit code after too many consecutive publish failures.
    const EXIT_PUBLISH_FAILURES: i32 = 2;

    /// Exit code when `waitset_wait` itself fails for a reason other than
    /// the ordinary sample-due timeout — continuing would either busy-loop
    /// (no real park occurred) or hang forever, so the service exits.
    const EXIT_WAIT_FAILED: i32 = 3;

    /// Exit code when the session's per-instance command mailbox cannot be
    /// bound: without it the panel could never be opened, and a silently
    /// deaf monitor is worse than one that says why it stopped.
    const EXIT_NO_COMMANDS: i32 = 4;

    /// Exit code when the desktop session refuses this instance's identity.
    ///
    /// A monitor the session itself launched cannot legitimately be an
    /// impostor, so a refusal is a fault in the pair rather than a reason
    /// to stop quietly: exiting `0` here would leave the panel vanishing
    /// mid-use with nothing anywhere to say why.
    const EXIT_SESSION_REFUSED: i32 = 5;

    /// The system log this service records its own abnormal end through.
    ///
    /// The desktop launches it with no terminal behind `stderr`, so the log
    /// is the only channel a user can still read the reason on.
    static LOG_SINK: tairix_rt::LogSink = tairix_rt::LogSink;

    /// State the abnormal-exit reason on `stderr` (fail loud: an exit code
    /// alone is not a diagnosis) and hand back `code` for `main`.
    fn fail(code: i32, reason: &str) -> i32 {
        let _ = writeln!(Stderr, "switchboard: {reason}");
        code
    }

    /// State a clean-exit reason on `stderr` and return `0`: the service
    /// has no purpose without a session to report to, so this is not a
    /// failure, merely a stated reason for stopping.
    fn clean_exit(reason: &str) -> i32 {
        let _ = writeln!(Stderr, "switchboard: {reason}");
        0
    }

    /// The code one cycle outcome ends the service with, or `None` to keep
    /// running.
    ///
    /// A session that is not there at all leaves the monitor with nothing to
    /// report to, which is a reason to stop rather than a fault. Being
    /// refused by a session that *is* there is a fault: the desktop launched
    /// this instance, so it cannot be the impostor it has been called, and
    /// ending quietly would leave the panel disappearing mid-use with
    /// nothing anywhere to say why. The log carries that one because a
    /// desktop-launched service has no terminal behind `stderr`.
    fn stop_code(outcome: CycleOutcome, pid: u64) -> Option<i32> {
        match outcome {
            CycleOutcome::Continue => None,
            CycleOutcome::SessionUnbound => Some(clean_exit(
                "the desktop session's Switchboard endpoint is not bound; exiting",
            )),
            CycleOutcome::SessionRefused => {
                let reason = "the desktop session refused this instance's identity; exiting";
                log(
                    &LOG_SINK,
                    &LogEvent {
                        level: LogLevel::Error,
                        id: SESSION_REFUSED,
                        message: reason,
                        fields: &[LogField {
                            key: "instance",
                            value: LogFieldValue::UnsignedInt(pid),
                        }],
                    },
                );
                Some(fail(EXIT_SESSION_REFUSED, reason))
            }
            CycleOutcome::PublishFailed => Some(fail(
                EXIT_PUBLISH_FAILURES,
                "too many consecutive publish failures",
            )),
        }
    }

    /// A display mode for a client area of `width_px` × `height_px`.
    fn mode_for(width_px: u32, height_px: u32) -> DisplayMode {
        DisplayMode {
            width_px,
            height_px,
            stride_bytes: width_px.saturating_mul(4),
            format: DisplayFormat::Rgba8888,
        }
    }

    /// Total bytes a `FRAME_COUNT`-frame region shaped as `mode` needs.
    fn region_bytes(mode: &DisplayMode) -> usize {
        (mode.stride_bytes as usize)
            .saturating_mul(mode.height_px as usize)
            .saturating_mul(FRAME_COUNT as usize)
    }

    /// The panel's text font: the theme's ordinary interface-text role
    /// resolved through the one shared role-to-font conversion.
    ///
    /// The window's extents are authored in unscaled pixels, so the role
    /// resolves at the desktop's `scale` to keep the text and the box it must
    /// fit in on one density. It is the one place the render and hit-test
    /// paths agrees on a font.
    fn panel_font(theme: &Theme, scale: Scale) -> BitmapFont {
        BitmapFont::for_role(theme.fonts(), TextRole::Body, scale)
    }

    /// The one open overview window: its session-side id, the identity that
    /// serves it, the surface it is painted through, and the shared frames
    /// the session blits from.
    struct Window {
        id: u64,
        server: ProcId,
        mode: DisplayMode,
        surface: Surface,
        frames: WindowFrames,
    }

    /// The production [`WindowTransport`]: one synchronous `ipc_call` to the
    /// reserved window endpoint per request.
    struct RtWindowTransport;

    impl WindowTransport for RtWindowTransport {
        fn call(&mut self, request: &[u8], reply: &mut [u8]) -> Result<usize, Errno> {
            tairix_rt::ipc_call(WINDOW_ENDPOINT, request, reply).map_err(Errno::from_syscall)
        }
    }

    /// The process's own effective capability set, read straight from the
    /// kernel. An action whose authority is absent renders refused and is
    /// never attempted.
    struct RtAuthority;

    impl CapabilityQuery for RtAuthority {
        fn holds(&self, cap: CapabilityId) -> bool {
            tairix_rt::cap_query(cap)
        }
    }

    /// The production [`ServiceHost`]: the window channel, the session's
    /// Switchboard endpoint, the `signal` syscall, and `stderr`.
    struct RtHost {
        set: u64,
        event_endpoint: u64,
        command_endpoint: u64,
        client: WindowClient<RtWindowTransport>,
        desktop: Desktop,
        themes: ThemeRegistry,
        window: Option<Window>,
        session: Option<ProcId>,
    }

    impl RtHost {
        /// A host with no window open, whose mailboxes are already bound
        /// (the window's not yet armed in `set`) and whose session identity
        /// is not yet known.
        fn new(set: u64, event_endpoint: u64, command_endpoint: u64, desktop: Desktop) -> Self {
            Self {
                set,
                event_endpoint,
                command_endpoint,
                client: WindowClient::new(RtWindowTransport),
                desktop,
                themes: ThemeRegistry::with_builtins(),
                window: None,
                session: None,
            }
        }

        /// The desktop session's kernel-attested identity, learned from the
        /// reply to this instance's first accepted publish. `None` until
        /// then, and every command is dropped while it is `None`: an
        /// unauthenticated command is never applied.
        fn session(&self) -> Option<ProcId> {
            self.session
        }

        /// The identity serving the open window, for authenticating its
        /// event mailbox.
        fn window_server(&self) -> Option<ProcId> {
            self.window.as_ref().map(|window| window.server)
        }

        /// The open window's client bounds.
        fn bounds(&self) -> Option<Rect> {
            self.window
                .as_ref()
                .map(|window| Rect::new(0, 0, window.mode.width_px, window.mode.height_px))
        }

        /// Re-map the window's frame region onto `width_px` × `height_px`
        /// and adopt it.
        ///
        /// The ordering is fail-closed: a fresh region is created and
        /// granted first and adopted only if the session accepts the
        /// re-map. On success the *old* region is unmapped (never before,
        /// so a refused resize leaves the current surface intact); on
        /// refusal the freshly allocated region is unmapped so nothing
        /// leaks. A region that cannot be allocated at all keeps the
        /// current size rather than tearing the window down.
        fn resize(&mut self, width_px: u32, height_px: u32) {
            let Some(window) = self.window.as_mut() else {
                return;
            };
            let mode = mode_for(width_px, height_px);
            if mode.width_px == window.mode.width_px && mode.height_px == window.mode.height_px {
                return;
            }
            let Some(spare) = WindowFrames::create(region_bytes(&mode)) else {
                return;
            };
            let Some(grant) = spare.grant() else {
                return;
            };
            let Some(surface) = Surface::new(mode.width_px, mode.height_px) else {
                return;
            };
            if self
                .client
                .resize(window.id, grant, FRAME_COUNT, &mode)
                .is_err()
            {
                return;
            }
            // Adopting drops the old region, which unmaps it; every early
            // return above drops the spare instead, so no path can leave a
            // region pinned or the surface half-replaced.
            window.frames = spare;
            window.surface = surface;
            window.mode = mode;
        }
    }

    impl ServiceHost for RtHost {
        fn open_window(&mut self) -> Result<(), Errno> {
            let (initial_w, initial_h) = self.desktop.window_size(WIN_WIDTH, WIN_HEIGHT);
            let mode = mode_for(initial_w, initial_h);
            let frames = WindowFrames::create(region_bytes(&mode)).ok_or(Errno::OutOfMemory)?;
            let grant = frames.grant().ok_or(Errno::OutOfMemory)?;
            let surface =
                Surface::new(mode.width_px, mode.height_px).ok_or(Errno::LengthOutOfRange)?;
            // The window manager decorates and resizes the window server-side;
            // the app draws no chrome and only re-maps its region when a
            // `WindowEvent::Resized` arrives.
            let created = self.client.create(
                grant,
                self.event_endpoint,
                FRAME_COUNT,
                &mode,
                PANEL_TITLE,
                WIN_SIZING,
            );
            let (id, server) = created?;
            if tairix_rt::waitset_ctl(
                self.set,
                WaitSetOp::Add,
                WaitSourceKind::Port,
                self.event_endpoint,
                WaitToken::WindowEvent.as_u64(),
            ) != 0
            {
                let _ = self.client.close(id);
                return Err(Errno::NotFound);
            }
            self.window = Some(Window {
                id,
                server,
                mode,
                surface,
                frames,
            });
            Ok(())
        }

        fn close_window(&mut self) -> Result<(), Errno> {
            let Some(window) = self.window.take() else {
                return Ok(());
            };
            let disarmed = tairix_rt::waitset_ctl(
                self.set,
                WaitSetOp::Del,
                WaitSourceKind::Port,
                self.event_endpoint,
                WaitToken::WindowEvent.as_u64(),
            );
            let closed = self.client.close(window.id);
            if disarmed != 0 {
                return Err(Errno::NotFound);
            }
            closed
        }

        fn present(
            &mut self,
            panel: &mut Switchboard,
            repaint: Repaint,
            damage: &Region,
        ) -> Result<(), Errno> {
            let bounds = self.bounds().ok_or(Errno::NotFound)?;
            let Self {
                client,
                desktop,
                themes,
                window,
                ..
            } = self;
            let window = window.as_mut().ok_or(Errno::NotFound)?;
            // A region the session released holds none of the pixels a partial
            // present would leave standing, so it is re-attached and drawn
            // whole.
            let repaint = if window.frames.is_released() {
                Repaint::Whole
            } else {
                repaint
            };
            let Some(rect) = present_damage(&window.mode, repaint, damage) else {
                return Ok(());
            };
            themes.set_appearance(desktop.appearance());
            let theme = themes.active();
            window
                .surface
                .with_clip(rect.x, rect.y, rect.width_px, rect.height_px, |surface| {
                    panel.render(
                        surface,
                        bounds,
                        desktop.scale(),
                        theme,
                        panel_font(theme, desktop.scale()),
                    );
                });
            let pixels = client
                .frame_pixels(&mut window.frames, window.id, FRAME_COUNT, &window.mode)
                .ok_or(Errno::NotAttached)?;
            winframe::encode(&window.surface, pixels, &window.mode, rect, &SERIAL)?;
            client.present(window.id, 0, rect)
        }

        fn render_inputs(&self) -> Option<RenderInputs> {
            let bounds = self.bounds()?;
            let theme = self.themes.active();
            Some(RenderInputs {
                bounds_left: bounds.origin.x,
                bounds_top: bounds.origin.y,
                bounds_width: bounds.width,
                bounds_height: bounds.height,
                theme_id: theme.id().0,
                scale_percent: self.desktop.scale().percent(),
            })
        }

        fn request(&mut self, request: SwitchboardRequest) -> Result<(), Errno> {
            let mut reply = [0u8; tairix_abi::reply::STATUS_REPLY_LEN];
            match tairix_rt::ipc_call(SWITCHBOARD_ENDPOINT, &request.to_le_bytes(), &mut reply) {
                Ok(len) => decode_status_reply(&reply[..len]),
                Err(ret) => Err(Errno::from_syscall(ret)),
            }
        }

        fn publish(&mut self, summary: TraySummary) -> Result<(), Errno> {
            let request = SwitchboardRequest::PublishSummary { summary }.to_le_bytes();
            let mut reply = [0u8; SWITCHBOARD_PUBLISH_REPLY_LEN];
            let session = match tairix_rt::ipc_call(SWITCHBOARD_ENDPOINT, &request, &mut reply) {
                Ok(len) => decode_publish_reply(&reply[..len])?,
                Err(ret) => return Err(Errno::from_syscall(ret)),
            };
            // The only process the kernel lets bind the seat-scoped
            // rendezvous is the one that answered this call, so the identity
            // it attested here is the one every later command must match.
            self.session = Some(session);
            Ok(())
        }

        fn signal(&mut self, pid: i32, signal: Signal) -> Result<(), Errno> {
            let ret = tairix_rt::signal(pid, signal);
            if ret == 0 {
                Ok(())
            } else {
                Err(Errno::from_syscall(ret))
            }
        }

        fn lower_priority(&mut self, pid: i32) -> Result<(), Errno> {
            let ret = tairix_rt::sched_set_priority(pid, SchedPriority::Low);
            if ret == 0 {
                Ok(())
            } else {
                Err(Errno::from_syscall(ret))
            }
        }

        fn power(&mut self, action: PowerAction) -> Result<(), Errno> {
            // A granted transition flushes every volume and stops the
            // machine, so this call does not come back; a return at all
            // means the kernel refused, and the reason is passed up to be
            // stated.
            let ret = tairix_rt::system_power(action);
            if ret == 0 {
                Ok(())
            } else {
                Err(Errno::from_syscall(ret))
            }
        }

        fn report_refusal(&mut self, action: &str, refusal: Errno) {
            io::write_stderr_line(&refusal_notice(action, refusal));
        }

        fn note_degradation(&mut self, field: DegradedField) {
            let reason = match field {
                DegradedField::ProcessList => {
                    "notice: the process list is unavailable; the task list and recovery rows are degraded"
                }
                DegradedField::CpuTime => {
                    "notice: CPU-time totals are unavailable; overall CPU load is degraded"
                }
                DegradedField::MemoryPressure => {
                    "notice: the memory-pressure gauge is unavailable; memory pressure is degraded"
                }
                DegradedField::Identity => {
                    "notice: the system identity is unavailable; the host name and version are degraded"
                }
                DegradedField::Uptime => "notice: uptime is unavailable; it is not shown",
                DegradedField::LoadAverage => {
                    "notice: the load average is unavailable; it is not shown"
                }
                DegradedField::CpuInfo => {
                    "notice: the CPU inventory is unavailable; core models and frequencies are degraded"
                }
                DegradedField::CpuLoad => {
                    "notice: per-CPU load is unavailable; only the overall CPU figure is shown"
                }
                DegradedField::KernelMemory => {
                    "notice: kernel memory accounting is unavailable; it is not shown"
                }
                DegradedField::MemoryTotal => {
                    "notice: the installed-memory total is unavailable; memory figures are degraded"
                }
                DegradedField::Mounts => {
                    "notice: the mount table is unavailable; volume capacities are not shown"
                }
                DegradedField::VolumeHealth => {
                    "notice: volume I/O health is unavailable; a failing disk cannot be reported"
                }
                DegradedField::NetInterfaceFacts => {
                    "notice: the network interface inventory is unavailable; interfaces are not named"
                }
                DegradedField::NetInterfaceState => {
                    "notice: network interface state is unavailable; link and address state are not shown"
                }
                DegradedField::NetInterfaceRates => {
                    "notice: network throughput is unavailable; rates are not shown"
                }
                DegradedField::Seats => {
                    "notice: the seat list is unavailable; seats are not shown"
                }
                DegradedField::ResourceLimits => {
                    "notice: resource limits are unavailable; they are not shown"
                }
                DegradedField::CrashRecords => {
                    "notice: crash records are unavailable; recent faults are not shown"
                }
            };
            let _ = writeln!(Stderr, "switchboard: {reason}");
        }
    }

    /// Map a wire [`PointerButtonCode`] onto the desktop [`PointerButton`].
    fn to_button(code: PointerButtonCode) -> PointerButton {
        match code {
            PointerButtonCode::Primary => PointerButton::Primary,
            PointerButtonCode::Secondary => PointerButton::Secondary,
            PointerButtonCode::Middle => PointerButton::Middle,
        }
    }

    /// Map a wire [`NamedKeyCode`] onto the desktop [`NamedKey`] (a total
    /// map).
    fn to_named_key(named: NamedKeyCode) -> NamedKey {
        match named {
            NamedKeyCode::Enter => NamedKey::Enter,
            NamedKeyCode::Escape => NamedKey::Escape,
            NamedKeyCode::Backspace => NamedKey::Backspace,
            NamedKeyCode::Tab => NamedKey::Tab,
            NamedKeyCode::Delete => NamedKey::Delete,
            NamedKeyCode::Insert => NamedKey::Insert,
            NamedKeyCode::Home => NamedKey::Home,
            NamedKeyCode::End => NamedKey::End,
            NamedKeyCode::PageUp => NamedKey::PageUp,
            NamedKeyCode::PageDown => NamedKey::PageDown,
            NamedKeyCode::Left => NamedKey::Left,
            NamedKeyCode::Right => NamedKey::Right,
            NamedKeyCode::Up => NamedKey::Up,
            NamedKeyCode::Down => NamedKey::Down,
            NamedKeyCode::F1 => NamedKey::Function { number: 1 },
            NamedKeyCode::F2 => NamedKey::Function { number: 2 },
            NamedKeyCode::F3 => NamedKey::Function { number: 3 },
            NamedKeyCode::F4 => NamedKey::Function { number: 4 },
            NamedKeyCode::F5 => NamedKey::Function { number: 5 },
            NamedKeyCode::F6 => NamedKey::Function { number: 6 },
            NamedKeyCode::F7 => NamedKey::Function { number: 7 },
            NamedKeyCode::F8 => NamedKey::Function { number: 8 },
            NamedKeyCode::F9 => NamedKey::Function { number: 9 },
            NamedKeyCode::F10 => NamedKey::Function { number: 10 },
            NamedKeyCode::F11 => NamedKey::Function { number: 11 },
            NamedKeyCode::F12 => NamedKey::Function { number: 12 },
        }
    }

    /// Feed one pointer position and its press/release to the composition,
    /// returning whichever action it reported last.
    fn route_pointer(
        service: &mut Service,
        host: &RtHost,
        x: u32,
        y: u32,
        action: PointerAction,
    ) -> Option<SwitchboardAction> {
        let bounds = host.bounds()?;
        let theme = host.themes.active();
        let font = panel_font(theme, host.desktop.scale());
        let panel = service.panel_mut();
        let at = pointer_point(x, y);
        let moved = panel.on_pointer(
            &InputEvent::PointerMoved { to: at },
            bounds,
            host.desktop.scale(),
            theme,
            font,
        );
        let acted = match action {
            PointerAction::Moved => None,
            PointerAction::Pressed(code) => panel.on_pointer(
                &InputEvent::PointerPressed {
                    button: to_button(code),
                },
                bounds,
                host.desktop.scale(),
                theme,
                font,
            ),
            PointerAction::Released(code) => panel.on_pointer(
                &InputEvent::PointerReleased {
                    button: to_button(code),
                },
                bounds,
                host.desktop.scale(),
                theme,
                font,
            ),
        };
        acted.or(moved)
    }

    /// Feed one key to the composition, laid out exactly as a present would
    /// lay it out, so every control the key reaches reports the rectangle it
    /// is drawn in.
    fn route_key(service: &mut Service, host: &RtHost, key: Key) -> Option<SwitchboardAction> {
        let bounds = host.bounds()?;
        let theme = host.themes.active();
        let font = panel_font(theme, host.desktop.scale());
        service
            .panel_mut()
            .on_key(key, bounds, host.desktop.scale(), theme, font)
    }

    /// Feed one wheel gesture to the composition.
    fn route_scroll(
        service: &mut Service,
        host: &RtHost,
        dx: i32,
        dy: i32,
    ) -> Option<SwitchboardAction> {
        let bounds = host.bounds()?;
        let theme = host.themes.active();
        let font = panel_font(theme, host.desktop.scale());
        service.panel_mut().on_pointer(
            &InputEvent::PointerScrolled { dx, dy },
            bounds,
            host.desktop.scale(),
            theme,
            font,
        )
    }

    /// Apply one delivered window event.
    ///
    /// Nothing here decides whether to re-present: the main loop's single
    /// end-of-wake [`Panel::flush`](tairix_switchboard::Panel::flush) call
    /// compares what the composition would now draw against what is
    /// already on screen and presents only on an actual difference, so a
    /// dense batch of events that changed nothing costs no present and one
    /// that did costs exactly one.
    fn apply_window_event(
        service: &mut Service,
        host: &mut RtHost,
        authority: &dyn CapabilityQuery,
        event: &WindowEvent,
    ) {
        let action = match *event {
            WindowEvent::CloseRequested { .. } => {
                service.panel_mut().close(host);
                return;
            }
            WindowEvent::Resized {
                width_px,
                height_px,
                ..
            } => {
                host.resize(width_px, height_px);
                // The re-mapped region and the fresh drawing surface hold none
                // of the last frame's pixels, so nothing partial can stand.
                service.panel_mut().repaint_whole();
                return;
            }
            // Nobody can see the window, so the session gave its copy of the
            // pixels back and unmapped the region. Let go of this side too —
            // the pages go only when both do; the redraw request that follows
            // the window being shown again re-attaches a fresh region.
            WindowEvent::ContentReleased { .. } => {
                if let Some(window) = host.window.as_mut() {
                    window.frames.release();
                }
                service.panel_mut().repaint_whole();
                return;
            }
            WindowEvent::Key {
                key: KeyInput::Pressed { key, .. },
                ..
            } => {
                let key = match key {
                    KeyValue::Char(ch) => Key::Char(ch),
                    KeyValue::Named(named) => Key::Named(to_named_key(named)),
                };
                route_key(service, host, key)
            }
            WindowEvent::Pointer { x, y, action, .. } => route_pointer(service, host, x, y, action),
            WindowEvent::Scrolled { dx, dy, .. } => route_scroll(service, host, dx, dy),
            // The session reclaimed the retained pixels. The composition is
            // unchanged, so the end-of-wake difference test would suppress
            // the present the blank window now needs: forget what was
            // presented and let that one path draw it.
            WindowEvent::RedrawRequested { .. } => {
                service.panel_mut().invalidate_presented();
                return;
            }
            // The desktop switched appearance, density, or screen. Bringing
            // the theme registry into step is all this needs: the panel is
            // composed from the active theme at the desktop's scale, so the
            // end-of-wake difference test sees the new composition and
            // presents it. A refused change states its reason and leaves the
            // last good desktop standing.
            WindowEvent::DesktopChanged { .. } => {
                match host.desktop.apply(event) {
                    Ok(true) => {
                        let appearance = host.desktop.appearance();
                        host.themes.set_appearance(appearance);
                        // A new appearance, density, or screen re-draws every
                        // pixel; no control round could have described it.
                        service.panel_mut().repaint_whole();
                    }
                    Ok(false) => {}
                    Err(err) => {
                        let _ = writeln!(Stderr, "switchboard: desktop change refused: {err}");
                    }
                }
                return;
            }
            // A secondary press on Close asks to leave what the window is
            // showing; the overview has nothing to leave but itself, and a
            // primary press already closes it. The monitor declares no
            // icon-bar presence — it is a service whose window the bar's own
            // capsule opens — so a bar click or menu row names nothing of its.
            // Nor does a chain outcome: it answers an open the overview never
            // asks for.
            WindowEvent::AlternateCloseRequested { .. }
            | WindowEvent::AppBarDefault
            | WindowEvent::AppBarMenu { .. }
            | WindowEvent::MenuClosed { .. }
            | WindowEvent::MenuPanelRequested { .. }
            | WindowEvent::Key { .. }
            | WindowEvent::Focus { .. }
            | WindowEvent::Minimized { .. }
            | WindowEvent::FilePicked { .. }
            | WindowEvent::PickCancelled { .. } => return,
        };
        if let Some(action) = action {
            if let Some(outcome) = service.panel_mut().act(host, action, authority) {
                service.apply_grouping(host, outcome, authority);
            }
        }
    }

    /// Drain every window event the session has delivered, applying each in
    /// turn. Only events whose kernel-attested sender is the identity that
    /// serves this window are accepted; anything else is dropped with a
    /// stated reason (fail closed — no forged input ever reaches the panel).
    fn drain_window_events(
        service: &mut Service,
        host: &mut RtHost,
        authority: &dyn CapabilityQuery,
    ) {
        let mut frame = [0u8; WindowEvent::WIRE_LEN];
        let mut sender = [0u8; ORIGIN_WIRE_LEN];
        loop {
            let Some(len) = next_message(
                host.event_endpoint,
                &mut frame,
                &mut sender,
                "read the window event mailbox",
            ) else {
                return;
            };
            // Every rejection below still takes its message with it. A drain
            // that returned with the mailbox non-empty would be woken for it
            // again at once, and the loop would spin instead of parking.
            let Some(server) = host.window_server() else {
                io::write_stderr_line("switchboard: dropped a window event for a closed window");
                continue;
            };
            if !from_identity(&sender, server) {
                io::write_stderr_line(
                    "switchboard: dropped a window event from an unattested sender",
                );
                continue;
            }
            let Ok(event) = WindowEvent::from_bytes(&frame[..len]) else {
                io::write_stderr_line("switchboard: dropped a malformed window event");
                continue;
            };
            apply_window_event(service, host, authority, &event);
        }
    }

    /// Drain every command the session has delivered, applying each in
    /// turn.
    ///
    /// Authentication comes first and is the kernel's word, never the
    /// wire's: a frame whose attested sender is not the session that
    /// answered this instance's publish is dropped before it is even
    /// decoded, and so is a frame that does not decode.
    fn drain_commands(service: &mut Service, host: &mut RtHost, authority: &dyn CapabilityQuery) {
        let mut frame = [0u8; SwitchboardCommand::WIRE_LEN];
        let mut sender = [0u8; ORIGIN_WIRE_LEN];
        loop {
            let Some(len) = next_message(
                host.command_endpoint,
                &mut frame,
                &mut sender,
                "read the command mailbox",
            ) else {
                return;
            };
            let Some(session) = host.session() else {
                io::write_stderr_line(
                    "switchboard: dropped a command received before any session was attested",
                );
                continue;
            };
            match authenticate_command(&frame[..len], &sender, session) {
                Ok(command) => service.command(host, command, authority),
                Err(Errno::PermissionDenied) => {
                    io::write_stderr_line(
                        "switchboard: dropped a command from a sender that is not the session",
                    );
                }
                Err(_) => io::write_stderr_line("switchboard: dropped a malformed command"),
            }
        }
    }

    /// `true` when `sender` decodes as an [`Origin`] the kernel attested to
    /// `expected`.
    fn from_identity(sender: &[u8; ORIGIN_WIRE_LEN], expected: ProcId) -> bool {
        Origin::from_bytes(sender).is_ok_and(|origin| origin.proc_id() == expected)
    }

    /// Take the next message waiting on `endpoint`, or [`None`] when the
    /// mailbox is drained.
    ///
    /// An empty mailbox is the ordinary end of a drain and says nothing; any
    /// other refusal means messages are being lost, so it is stated once per
    /// drain and the drain ends rather than spinning on the same failure.
    fn next_message(
        endpoint: u64,
        frame: &mut [u8],
        sender: &mut [u8; ORIGIN_WIRE_LEN],
        action: &str,
    ) -> Option<usize> {
        match tairix_rt::ipc_recv(endpoint, frame, sender) {
            Ok(len) => Some(len),
            Err(ret) if Errno::from_syscall(ret) == Errno::WouldBlock => None,
            Err(ret) => {
                io::write_stderr_line(&refusal_notice(action, Errno::from_syscall(ret)));
                None
            }
        }
    }

    /// Name the drained termination signal for the exit notice.
    fn signal_name(drained: i64) -> &'static str {
        if drained < 0 {
            return "unknown";
        }
        #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
        // A non-negative take result is the drained signal's u32 wire discriminant.
        let raw = drained as u32;
        match Signal::from_u32(raw) {
            Ok(Signal::Terminate) => "terminate",
            Ok(Signal::Interrupt) => "interrupt",
            Ok(Signal::Kill) => "kill",
            Ok(Signal::Continue) => "continue",
            Ok(Signal::Stop) => "stop",
            Err(_) => "unknown",
        }
    }

    /// Build and arm the wait-set this loop parks on: the process's own
    /// termination signal and the session's per-instance command mailbox.
    /// The open window's event mailbox joins and leaves the same set as the
    /// window itself opens and closes.
    fn arm_wait_set(command_endpoint: u64) -> Result<u64, i32> {
        let set = tairix_rt::waitset_create();
        if set < 0 {
            return Err(fail(EXIT_NO_WAIT_SOURCE, "cannot create the wait-set"));
        }
        #[allow(clippy::cast_sign_loss)] // `set >= 0` checked above; it is a kernel-minted handle.
        let set = set as u64;
        if tairix_rt::waitset_ctl(
            set,
            WaitSetOp::Add,
            WaitSourceKind::Signal,
            0,
            WaitToken::Signal.as_u64(),
        ) != 0
        {
            return Err(fail(
                EXIT_NO_WAIT_SOURCE,
                "cannot arm the termination signal wait-set member",
            ));
        }
        if tairix_rt::waitset_ctl(
            set,
            WaitSetOp::Add,
            WaitSourceKind::Port,
            command_endpoint,
            WaitToken::Command.as_u64(),
        ) != 0
        {
            return Err(fail(
                EXIT_NO_COMMANDS,
                "cannot arm the command mailbox wait-set member",
            ));
        }
        // Arms the band wake and reads the band in force now, so the glyph
        // cache starts from what the machine actually reports rather than the
        // fail-closed unknown that admits nothing.
        if !tairix_procinfo::pressure::watch(set, WaitToken::MemoryPressure.as_u64()) {
            return Err(fail(
                EXIT_NO_WAIT_SOURCE,
                "cannot arm the memory-pressure wait-set member",
            ));
        }
        Ok(set)
    }

    /// Program entry point. `tairix-rt`'s `_start` calls it once the runtime
    /// is set up and routes its return value through the `exit` syscall.
    fn main() -> i32 {
        if tairix_rt::signal_intake(SignalIntakeOp::Enable) != 0 {
            return fail(EXIT_NO_WAIT_SOURCE, "cannot enable signal observation");
        }
        let Ok(origin) = tairix_rt::self_origin() else {
            return fail(EXIT_NO_WAIT_SOURCE, "own identity unavailable");
        };
        let pid = origin.pid();

        let commands = command_endpoint_for(pid);
        if tairix_abi::ipc::is_reserved_endpoint(commands)
            || tairix_rt::port_bind(commands, SwitchboardCommand::WIRE_LEN, COMMAND_CAPACITY) != 0
        {
            return fail(EXIT_NO_COMMANDS, "command mailbox bind refused");
        }
        let events = tairix_window::event_endpoint_for(pid);
        if tairix_abi::ipc::is_reserved_endpoint(events)
            || tairix_rt::port_bind(
                events,
                WindowEvent::WIRE_LEN,
                tairix_window::EVENT_MAILBOX_CAPACITY,
            ) != 0
        {
            return fail(EXIT_NO_WAIT_SOURCE, "window event mailbox bind refused");
        }
        let set = match arm_wait_set(commands) {
            Ok(set) => set,
            Err(code) => return code,
        };

        let mut client = WindowClient::new(RtWindowTransport);
        // The desktop this window will be shown on: the screen, the density,
        // and the appearance, before anything is sized or painted, so the
        // first frame is right rather than a guess corrected once the user
        // has seen it.
        let info = match client.desktop() {
            Ok(info) => info,
            Err(err) => {
                let _ = writeln!(Stderr, "switchboard: desktop query refused: {err}");
                return EXIT_NO_WAIT_SOURCE;
            }
        };
        let desktop = match Desktop::new(info) {
            Ok(desktop) => desktop,
            Err(err) => {
                let _ = writeln!(Stderr, "switchboard: cannot draw this desktop: {err}");
                return EXIT_NO_WAIT_SOURCE;
            }
        };

        let transport = IpcTransport;
        let authority = RtAuthority;
        let mut host = RtHost::new(set, events, commands, desktop);
        host.client = client;
        let mut service = Service::new(pid, probe_scopes(&transport), &authority);

        loop {
            let cycled = service.cycle(&mut host, &transport, tairix_rt::clock_get(), &authority);
            if let Some(code) = stop_code(cycled, pid) {
                return code;
            }

            // One present per wake, immediately before parking: whatever the
            // cycle above and the previous wake's drained events marked is
            // shown in a single composition. Placing it here rather than
            // after the drain also covers the deadline-only path, which
            // continues straight back to the top of the loop.
            service.panel_mut().flush(&mut host);

            let timeout = service.wait_timeout_ns(tairix_rt::clock_get());
            let mut token = 0u64;
            let wait_ret = tairix_rt::waitset_wait(set, timeout, &mut token);
            if wait_ret != 0 {
                if Errno::from_syscall(wait_ret) == Errno::TimedOut {
                    continue;
                }
                // Any other wait failure means the loop is no longer
                // actually parking: continuing would spin rather than wait,
                // so exit fail-loud instead.
                return fail(EXIT_WAIT_FAILED, "the wait-set failed unexpectedly");
            }
            match WaitToken::from_u64(token) {
                Some(WaitToken::Signal) => {
                    let drained = tairix_rt::signal_intake(SignalIntakeOp::Take);
                    let name = signal_name(drained);
                    let _ = writeln!(Stderr, "switchboard: received a {name} signal; exiting");
                    return 0;
                }
                Some(WaitToken::Command) => drain_commands(&mut service, &mut host, &authority),
                Some(WaitToken::WindowEvent) => {
                    drain_window_events(&mut service, &mut host, &authority);
                }
                Some(WaitToken::MemoryPressure) if tairix_procinfo::pressure::refresh() => {
                    tairix_font::trim_glyph_cache();
                }
                // A band that did not move needs no trim, and a token the
                // loop never arms is a spurious wake: either way, re-sample
                // on the next iteration rather than acting on a guess.
                Some(WaitToken::MemoryPressure) | None => {}
            }
        }
    }

    tairix_rt::entry!(main);
}

// --- Host stub ------------------------------------------------------------
//
// Whenever the real freestanding `tairix-rt` `_start` path is not compiled —
// on the host (`cargo build --workspace`, clippy, fmt), or for a
// `program`-less build of this crate — this inert `main` keeps the crate
// building under the host tooling. It performs no I/O.
#[cfg(not(all(freestanding, feature = "program")))]
fn main() {}
