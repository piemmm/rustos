//! The `widgets.app` bundle's `Run` entry point: the windowed Reactive Alloy
//! widget gallery (`plans/GUI-CONTROLS-DESIGN.md`).
//!
//! Everything with behaviour worth testing lives in the host-tested gallery
//! model (`tairix_widgets`); this binary only composes it over the live window
//! channel, exactly as `userland/apps/files` composes `lib/browse`:
//!
//! * one `shm_create`d frame region granted to the reserved window endpoint
//!   (the zero-copy surface the session maps once at create);
//! * one `port_bind`-bound event mailbox the app **parks** on through its
//!   wait-set — never a poll loop. Every received event carries its sender's
//!   kernel-attested origin, and the app accepts only events from the session
//!   identity the create reply named, so no other process can feed it forged
//!   input (fail closed);
//! * the `WindowClient` calls (create / present / close) and the
//!   `WindowEvents` typed wait over the parked source.
//!
//! Delivered pointer and key events are mapped onto the shared desktop input
//! vocabulary and routed into the gallery, which draws the tab strip and the
//! selected family's panel of demo widgets and reflects each control's own
//! action back into it. A `CloseRequested` from the desktop closes the window
//! and ends the program cleanly; every bring-up refusal exits fail-loud with a
//! reserved code and a stated reason on `stderr`.
//!
//! On the host it is an inert stub so `cargo build --workspace`, clippy, and
//! fmt still cover the file.

#![cfg_attr(freestanding, no_std)]
#![cfg_attr(freestanding, no_main)]
#![deny(missing_docs)]

// --- Pure-Rust program --------------------------------------------------
#[cfg(freestanding)]
mod program {
    use tairix_abi::driver::display::{DamageRect, DisplayFormat, DisplayMode};
    use tairix_abi::input::{
        KeyInput, KeyValue, Modifiers as AbiModifiers, NamedKeyCode, PointerButtonCode,
    };
    use tairix_abi::window_ipc::{PointerAction, WindowEvent, WINDOW_ENDPOINT};
    use tairix_abi::{Errno, Origin, ProcId, ORIGIN_WIRE_LEN};
    use tairix_font::BitmapFont;
    use tairix_geometry::{Point, Rect, Scale};
    use tairix_input::{InputEvent, Key, Modifiers, NamedKey, PointerButton};
    use tairix_theme::{TextRole, Theme, ThemeRegistry};
    use tairix_widgets::Gallery;
    use tairix_window::{EventSource, WindowClient, WindowEvents, WindowTransport};

    /// The gallery window's logical width in physical pixels.
    const WIN_WIDTH: u32 = 820;
    /// The gallery window's logical height in physical pixels.
    const WIN_HEIGHT: u32 = 620;

    /// Frames in the shared region. The window protocol serialises a present
    /// (the app is parked in the call while the session reads), so a single
    /// frame is race-free.
    const FRAME_COUNT: u32 = 1;

    /// The event mailbox's bounded capacity: input-rate events, drained after
    /// every wake, so a small queue is ample.
    const EVENT_CAPACITY: usize = 32;

    /// The wait-set token of the event-mailbox member.
    const EVENT_TOKEN: u64 = 1;

    /// Exit code when the shared frame region could not be created or granted.
    const EXIT_NO_FRAMES: i32 = 81;
    /// Exit code when the event mailbox could not be bound or observed.
    const EXIT_NO_EVENTS: i32 = 82;
    /// Exit code when the desktop session refused the window create.
    const EXIT_NO_WINDOW: i32 = 83;
    /// Exit code when a present was refused or the event channel died.
    const EXIT_CHANNEL_LOST: i32 = 84;

    /// Recover the [`Errno`] a syscall encoded as a negative register.
    fn errno_from(ret: i64) -> Errno {
        i32::try_from(-ret)
            .ok()
            .and_then(Errno::from_i32)
            .unwrap_or(Errno::NotImplemented)
    }

    /// State the abnormal-exit reason on `stderr` (fail loud) and hand back
    /// `code` for `main`.
    fn fail(code: i32, reason: &str) -> i32 {
        let _ = tairix_rt::stderr(b"widgets: ");
        let _ = tairix_rt::stderr(reason.as_bytes());
        let _ = tairix_rt::stderr(b"\n");
        code
    }

    /// The production [`WindowTransport`]: one synchronous `ipc_call` to the
    /// reserved window endpoint per request.
    struct RtWindowTransport;

    impl WindowTransport for RtWindowTransport {
        fn call(&mut self, request: &[u8], reply: &mut [u8]) -> Result<usize, Errno> {
            tairix_rt::ipc_call(WINDOW_ENDPOINT, request, reply).map_err(errno_from)
        }
    }

    /// The production [`EventSource`]: drain the app's own event mailbox,
    /// parking on the wait-set whenever it is empty, and accept only events
    /// whose kernel-attested sender is the desktop session named by the create
    /// reply — anything else is dropped (fail closed).
    struct RtEventSource {
        endpoint: u64,
        set: u64,
        server: ProcId,
    }

    /// Whether a received mailbox frame is a genuine event from the desktop
    /// session: exactly one [`WindowEvent`] wide and from the attested origin.
    fn accept_frame(len: usize, sender: &[u8; ORIGIN_WIRE_LEN], server: ProcId) -> bool {
        len == WindowEvent::WIRE_LEN
            && Origin::from_bytes(sender).is_ok_and(|origin| origin.proc_id() == server)
    }

    impl EventSource for RtEventSource {
        fn next(&mut self, event: &mut [u8; WindowEvent::WIRE_LEN]) -> Result<(), Errno> {
            loop {
                let mut sender = [0u8; ORIGIN_WIRE_LEN];
                match tairix_rt::ipc_recv(self.endpoint, event, &mut sender) {
                    Ok(len) => {
                        if accept_frame(len, &sender, self.server) {
                            return Ok(());
                        }
                    }
                    Err(err) if errno_from(err) == Errno::WouldBlock => {
                        let mut token = 0u64;
                        if tairix_rt::waitset_wait(self.set, u64::MAX, &mut token) != 0 {
                            return Err(Errno::NotFound);
                        }
                    }
                    Err(err) => return Err(errno_from(err)),
                }
            }
        }
    }

    /// Render the gallery into `frame` (the shared window surface) and present
    /// the whole window.
    fn present_frame<T: WindowTransport>(
        gallery: &Gallery,
        theme: &Theme,
        client: &mut WindowClient<T>,
        window: u64,
        frame: &mut [u8],
        mode: &DisplayMode,
    ) -> Result<(), Errno> {
        let viewport = Rect::new(0, 0, mode.width_px, mode.height_px);
        let font = gallery_font(theme);
        let mut surface = tairix_raster::Surface::new(mode.width_px, mode.height_px)
            .ok_or(Errno::LengthOutOfRange)?;
        gallery.render(&mut surface, viewport, Scale::ONE, theme, font);
        for (i, pixel) in surface.pixels().iter().enumerate() {
            let color = pixel.unpremultiply();
            let at = i * 4;
            let Some(slot) = frame.get_mut(at..at + 4) else {
                return Err(Errno::LengthOutOfRange);
            };
            slot.copy_from_slice(&[color.r, color.g, color.b, color.a]);
        }
        client.present(window, 0, DamageRect::full(mode))
    }

    /// Apply one delivered event to the gallery, reporting whether the view
    /// changed (and must re-present) and whether the app should end.
    fn apply_event(
        gallery: &mut Gallery,
        theme: &Theme,
        mode: &DisplayMode,
        event: &WindowEvent,
    ) -> (bool, bool) {
        let viewport = Rect::new(0, 0, mode.width_px, mode.height_px);
        let font = gallery_font(theme);
        match event {
            WindowEvent::CloseRequested { .. } => (false, true),
            WindowEvent::Key {
                key: KeyInput::Pressed { key, modifiers },
                ..
            } => {
                let (key, mods) = to_editor_key(*key, *modifiers);
                (gallery.on_key(key, mods), false)
            }
            WindowEvent::Pointer { x, y, action, .. } => (
                apply_pointer(gallery, *x, *y, *action, viewport, theme, font),
                false,
            ),
            WindowEvent::Scrolled { dx, dy, .. } => {
                let scroll = InputEvent::PointerScrolled { dx: *dx, dy: *dy };
                (
                    gallery.on_pointer(&scroll, viewport, Scale::ONE, theme, font),
                    false,
                )
            }
            WindowEvent::Key { .. }
            | WindowEvent::Focus { .. }
            | WindowEvent::Minimized { .. }
            | WindowEvent::Resized { .. }
            | WindowEvent::FilePicked { .. }
            | WindowEvent::PickCancelled { .. } => (false, false),
        }
    }

    /// Route one wire pointer event: a move to `(x, y)` to sync the pointer,
    /// then the press/release the action names. Returns whether the view
    /// changed.
    /// The gallery's text font: the theme's ordinary interface-text role
    /// resolved through the one shared role-to-font conversion, so the gallery
    /// reads like every other list of interface text.
    ///
    /// The window's extents are authored in unscaled pixels ([`WIN_WIDTH`],
    /// [`WIN_HEIGHT`]), so the role resolves at [`Scale::ONE`] to keep the text
    /// and the box it must fit in on one density. It is the one place the
    /// render and hit-test paths agree on a font.
    fn gallery_font(theme: &Theme) -> BitmapFont {
        BitmapFont::for_role(theme.fonts(), TextRole::Body, Scale::ONE)
    }

    fn apply_pointer(
        gallery: &mut Gallery,
        x: u32,
        y: u32,
        action: PointerAction,
        viewport: Rect,
        theme: &Theme,
        font: BitmapFont,
    ) -> bool {
        let point = Point::new(
            i32::try_from(x).unwrap_or(i32::MAX),
            i32::try_from(y).unwrap_or(i32::MAX),
        );
        let moved = gallery.on_pointer(
            &InputEvent::PointerMoved { to: point },
            viewport,
            Scale::ONE,
            theme,
            font,
        );
        let acted = match action {
            PointerAction::Moved => false,
            PointerAction::Pressed(code) => gallery.on_pointer(
                &InputEvent::PointerPressed {
                    button: to_button(code),
                },
                viewport,
                Scale::ONE,
                theme,
                font,
            ),
            PointerAction::Released(code) => gallery.on_pointer(
                &InputEvent::PointerReleased {
                    button: to_button(code),
                },
                viewport,
                Scale::ONE,
                theme,
                font,
            ),
        };
        moved || acted
    }

    /// Map a wire [`PointerButtonCode`] onto the desktop [`PointerButton`].
    fn to_button(code: PointerButtonCode) -> PointerButton {
        match code {
            PointerButtonCode::Primary => PointerButton::Primary,
            PointerButtonCode::Secondary => PointerButton::Secondary,
            PointerButtonCode::Middle => PointerButton::Middle,
        }
    }

    /// Map the window channel's wire key event onto the desktop control
    /// vocabulary the gallery consumes.
    fn to_editor_key(key: KeyValue, mods: AbiModifiers) -> (Key, Modifiers) {
        let modifiers = Modifiers {
            shift: mods.shift,
            ctrl: mods.ctrl,
            alt: mods.alt,
            meta: mods.meta,
        };
        let key = match key {
            KeyValue::Char(ch) => Key::Char(ch),
            KeyValue::Named(named) => Key::Named(named_to_editor(named)),
        };
        (key, modifiers)
    }

    /// Map a wire [`NamedKeyCode`] onto the desktop [`NamedKey`] (a total map).
    fn named_to_editor(named: NamedKeyCode) -> NamedKey {
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

    /// Bind the app's own event mailbox and add it to a fresh wait-set the
    /// event loop parks on, returning `(endpoint, set)`. On any refusal it
    /// states the reason on `stderr` and returns the reserved fail-closed
    /// [`EXIT_NO_EVENTS`] code for `main`.
    fn bind_event_mailbox() -> Result<(u64, u64), i32> {
        let Ok(origin) = tairix_rt::self_origin() else {
            return Err(fail(EXIT_NO_EVENTS, "own identity unavailable"));
        };
        let event_endpoint = tairix_window::event_endpoint_for(origin.pid());
        if tairix_abi::ipc::is_reserved_endpoint(event_endpoint)
            || tairix_rt::port_bind(event_endpoint, WindowEvent::WIRE_LEN, EVENT_CAPACITY) != 0
        {
            return Err(fail(EXIT_NO_EVENTS, "event mailbox bind refused"));
        }
        let set = tairix_rt::waitset_create();
        if set < 0 {
            return Err(fail(EXIT_NO_EVENTS, "wait-set refused"));
        }
        #[allow(clippy::cast_sign_loss)] // `set >= 0` checked above; it is a kernel handle.
        let set = set as u64;
        if tairix_rt::waitset_ctl(
            set,
            tairix_abi::WaitSetOp::Add,
            tairix_abi::WaitSourceKind::Port,
            event_endpoint,
            EVENT_TOKEN,
        ) != 0
        {
            return Err(fail(EXIT_NO_EVENTS, "event mailbox wait refused"));
        }
        Ok((event_endpoint, set))
    }

    /// Program entry point. `tairix-rt`'s `_start` calls it once the runtime is
    /// set up and routes its return value through the `exit` syscall.
    fn main() -> i32 {
        let mode = DisplayMode {
            width_px: WIN_WIDTH,
            height_px: WIN_HEIGHT,
            stride_bytes: WIN_WIDTH * 4,
            format: DisplayFormat::Rgba8888,
        };
        let frame_len = (mode.stride_bytes as usize) * (mode.height_px as usize);
        let total = frame_len * FRAME_COUNT as usize;
        let mut region_id: u64 = 0;
        let base = tairix_rt::shm_create(total, &mut region_id);
        if base < 0 {
            return fail(EXIT_NO_FRAMES, "shared frame region refused");
        }
        let grant = tairix_rt::shm_grant(region_id, WINDOW_ENDPOINT);
        if grant < 1 {
            return fail(EXIT_NO_FRAMES, "frame region grant refused");
        }
        let Ok(base) = usize::try_from(base) else {
            return fail(
                EXIT_NO_FRAMES,
                "frame region base outside the address width",
            );
        };
        // SAFETY: the kernel mapped exactly `total` zeroed bytes read/write
        // into this process at `base` (`shm_create` maps the length it was
        // asked for) and the mapping stays live for the life of the process —
        // nothing below unmaps or aliases it. The session maps the same frames
        // read-only for its blit, and the protocol serialises access: this app
        // is parked in its present call while the session reads.
        let frames = unsafe { core::slice::from_raw_parts_mut(base as *mut u8, total) };

        let (event_endpoint, set) = match bind_event_mailbox() {
            Ok(pair) => pair,
            Err(code) => return code,
        };

        let mut client = WindowClient::new(RtWindowTransport);
        #[allow(clippy::cast_sign_loss)] // `grant >= 1` checked above; it is a kernel handle.
        let Ok((window, server)) = client.create(
            grant as u64,
            event_endpoint,
            FRAME_COUNT,
            &mode,
            "widgets",
            false,
        ) else {
            return fail(EXIT_NO_WINDOW, "desktop session refused the window");
        };

        let themes = ThemeRegistry::with_builtins();
        let theme = themes.active();
        let mut gallery = Gallery::new();
        if present_frame(&gallery, theme, &mut client, window, frames, &mode).is_err() {
            return fail(EXIT_CHANNEL_LOST, "first present refused");
        }

        let mut events = WindowEvents::new(RtEventSource {
            endpoint: event_endpoint,
            set,
            server,
        });
        loop {
            let event = match events.wait() {
                Ok(event) => event,
                Err(Errno::OutOfRange | Errno::BadMagic | Errno::BufferTooSmall) => continue,
                Err(_) => return fail(EXIT_CHANNEL_LOST, "event channel lost"),
            };
            let (changed, close) = apply_event(&mut gallery, theme, &mode, &event);
            if close {
                let _ = client.close(window);
                return 0;
            }
            if changed
                && present_frame(&gallery, theme, &mut client, window, frames, &mode).is_err()
            {
                return fail(EXIT_CHANNEL_LOST, "present refused");
            }
        }
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
