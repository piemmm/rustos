//! The `files.app` bundle's `Run` entry point (`plans/APPWIN.md` AW3):
//! the windowed file browser, the first app served over the desktop
//! session's window channel.
//!
//! # What the program wires (and what stays in the libraries)
//!
//! Everything with behaviour worth testing lives in host-tested crates —
//! the shared browser engine and its validated path spelling
//! (`tairix_browse`), the themed listing renderer
//! (`tairix_browse::render`), and the window
//! channel's client half (`tairix_window`). This binary only composes
//! them over the live syscalls:
//!
//! * One `shm_create`d frame region, granted to the reserved window
//!   endpoint (the zero-copy surface the session maps once at create).
//! * One `port_bind`-bound event mailbox the app **parks** on through
//!   its wait-set — never a poll loop. Every received event carries its
//!   sender's kernel-attested origin, and the app accepts only events
//!   from the session identity the (squat-protected) create reply
//!   named: no other process can feed it forged input (fail closed).
//! * The `WindowClient` calls (create / present / close) over `ipc_call`
//!   and the `WindowEvents` typed wait over the parked source.
//!
//! Keyboard navigation drives the browser (`Down`/`Up` select, `Enter`
//! opens a directory, `Backspace` goes up); `F2` renames the selected
//! item through an inline `lib/controls` text field, committing over
//! `fs_rename` under the user's own identity (a refusal is stated in the
//! field, never a silent failure or a fabricated success); a
//! `CloseRequested` from the desktop closes the window and ends the
//! program cleanly. Every bring-up refusal exits fail-loud with a
//! reserved code and a stated reason on `stderr`.
//!
//! On the host it is an inert stub so `cargo build --workspace`, clippy,
//! and fmt still cover the file.

#![cfg_attr(freestanding, no_std)]
#![cfg_attr(freestanding, no_main)]
#![deny(missing_docs)]

#[cfg(freestanding)]
extern crate alloc;

// --- Pure-Rust program --------------------------------------------------
#[cfg(freestanding)]
mod program {

    use alloc::string::{String, ToString};

    use tairix_abi::driver::display::{DamageRect, DisplayFormat, DisplayMode};
    use tairix_abi::fs::{OpenFlags, FS_MODE_MASK, FS_NAME_MAX};
    use tairix_abi::input::{
        KeyInput, KeyValue, Modifiers as AbiModifiers, NamedKeyCode, PointerButtonCode,
    };
    use tairix_abi::window_ipc::{PointerAction, WindowEvent, WINDOW_ENDPOINT};
    use tairix_abi::{
        CapabilityId, Errno, Origin, ProcId, WaitSetOp, WaitSourceKind, ORIGIN_WIRE_LEN,
    };
    use tairix_browse::render::{
        draw_owner_control, draw_properties_editable, manager_tool_at, owner_field_at,
        permission_cell_at, render, OwnerField,
    };
    use tairix_browse::{
        suggest_new_dir_name, validate_new_name, Browser, DirectorySource, EntryKind, ManagerTool,
        OwnerChange, Properties, RenameError, ToolbarCommand, VfsDirectorySource, MANAGER_TOOLS,
        WIN_HEIGHT, WIN_WIDTH,
    };
    use tairix_controls::text::{TextAction, TextField};
    use tairix_font::BitmapFont;
    use tairix_geometry::{Point, Rect, Scale};
    use tairix_input::{Key, Modifiers, NamedKey};
    use tairix_theme::{Theme, ThemeRegistry};
    use tairix_window::{EventSource, WindowClient, WindowEvents, WindowTransport};

    /// Exit code when the initial directory listing was refused (no
    /// filesystem reach, or a corrupt stream). A reserved, fail-closed
    /// value: the browser never shows a fabricated listing.
    const EXIT_NO_LISTING: i32 = 80;

    /// Exit code when the shared frame region could not be created or
    /// granted to the window endpoint. A reserved, fail-closed value.
    const EXIT_NO_FRAMES: i32 = 81;

    /// Exit code when the event mailbox could not be bound or observed
    /// through the wait-set. A reserved, fail-closed value: the app
    /// exits rather than degrade into a busy re-poll.
    const EXIT_NO_EVENTS: i32 = 82;

    /// Exit code when the desktop session refused the window create (no
    /// graphical session, or the channel refused the geometry). A
    /// reserved, fail-closed value.
    const EXIT_NO_WINDOW: i32 = 83;

    /// Exit code when a present was refused or the event channel died
    /// (the session went away). A reserved, fail-closed value.
    const EXIT_CHANNEL_LOST: i32 = 84;

    /// Frames in the shared region. The window protocol serialises a
    /// present (the app is parked in the call while the session reads),
    /// so a single frame is race-free; the constant names the choice.
    const FRAME_COUNT: u32 = 1;

    /// The event mailbox's bounded capacity: input-rate events, drained
    /// after every wake, so a small queue is ample and a stalled app
    /// costs the kernel a bounded mailbox — never unbounded memory.
    const EVENT_CAPACITY: usize = 32;

    /// The wait-set token of the event-mailbox member.
    const EVENT_TOKEN: u64 = 1;

    /// The maximum digit count the owner/group id editor accepts — a `u32` id
    /// is at most ten decimal digits, so a longer entry cannot be a valid id.
    const OWNER_ID_MAX_DIGITS: usize = 10;

    /// The in-field hint shown when a typed owner/group id is not a
    /// well-formed, assignable `u32` (non-numeric, empty, out of range, or the
    /// reserved "unchanged" sentinel).
    const OWNER_ID_HINT: &str = "Enter a valid numeric id.";

    /// Recover the [`Errno`] a syscall encoded as a negative register
    /// (`-ret`); an unrecognised code fails closed as
    /// [`Errno::NotImplemented`] rather than being guessed.
    fn errno_from(ret: i64) -> Errno {
        i32::try_from(-ret)
            .ok()
            .and_then(Errno::from_i32)
            .unwrap_or(Errno::NotImplemented)
    }

    /// State the abnormal-exit reason on `stderr` (fail loud: an exit
    /// code alone is not a diagnosis) and hand back `code` for `main`.
    fn fail(code: i32, reason: &str) -> i32 {
        let _ = tairix_rt::stderr(b"files: ");
        let _ = tairix_rt::stderr(reason.as_bytes());
        let _ = tairix_rt::stderr(b"\n");
        code
    }

    /// The production [`WindowTransport`]: one synchronous `ipc_call` to
    /// the reserved window endpoint per request. The session attests the
    /// caller kernel-side on every request, so the transport carries no
    /// claimed authority.
    struct RtWindowTransport;

    impl WindowTransport for RtWindowTransport {
        fn call(&mut self, request: &[u8], reply: &mut [u8]) -> Result<usize, Errno> {
            tairix_rt::ipc_call(WINDOW_ENDPOINT, request, reply).map_err(errno_from)
        }
    }

    /// The production [`EventSource`]: drain the app's own event
    /// mailbox, parking on the wait-set whenever it is empty, and accept
    /// only events whose kernel-attested sender is the desktop session
    /// named by the create reply — anything else is dropped (fail
    /// closed), so no other process can feed the app forged input.
    struct RtEventSource {
        /// The app's event-mailbox endpoint id.
        endpoint: u64,
        /// The wait-set handle the app parks on.
        set: u64,
        /// The only sender whose events are accepted.
        server: ProcId,
    }

    impl EventSource for RtEventSource {
        fn next(&mut self, event: &mut [u8; WindowEvent::WIRE_LEN]) -> Result<(), Errno> {
            loop {
                let mut sender = [0u8; ORIGIN_WIRE_LEN];
                match tairix_rt::ipc_recv(self.endpoint, event, &mut sender) {
                    Ok(len) => {
                        // A short frame or a foreign sender is dropped,
                        // never delivered: the mailbox is open to any
                        // capable sender, so the kernel-attested origin
                        // is the authentication.
                        if len != WindowEvent::WIRE_LEN {
                            continue;
                        }
                        let Ok(origin) = Origin::from_bytes(&sender) else {
                            continue;
                        };
                        if origin.proc_id() != self.server {
                            continue;
                        }
                        return Ok(());
                    }
                    Err(err) if errno_from(err) == Errno::WouldBlock => {
                        // Nothing queued: park until the session's next
                        // delivery wakes the wait-set — never a spin.
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

    /// The open owner-id editor on the Properties overlay: which owning id is
    /// being edited and the inline [`TextField`] carrying the typed value.
    ///
    /// `None` unless the user (who must hold `CAP_FS_CHOWN`) clicked the uid or
    /// gid value to reassign it; the event loop threads it so the editor state
    /// and the painted control stay in step, exactly as `rename`/`properties`
    /// are threaded.
    struct OwnerEditor {
        /// Which owning id the editor commits.
        field: OwnerField,
        /// The inline numeric editor, pre-filled with the current id.
        editor: TextField,
    }

    /// The transient overlay state layered over the browser view, threaded
    /// through the event loop so the painted overlays and the state they
    /// reflect stay in step. At most one of `rename`/`properties` is open at a
    /// time; `owner` is nested inside `properties`.
    struct Overlays {
        /// The in-place rename editor, when open (`F2`).
        rename: Option<TextField>,
        /// The Properties overlay, when open (`Alt+Enter`).
        properties: Option<Properties>,
        /// The inline owner/group id editor on the Properties overlay.
        owner: Option<OwnerEditor>,
        /// Whether the launching user holds `CAP_FS_CHOWN` — the one gate on
        /// offering the ownership control (read once at start-up).
        can_chown: bool,
    }

    /// Render the browser into `frame` (the shared window surface) and
    /// present the whole window.
    ///
    /// The full-window damage is deliberate: a listing change repaints
    /// the path bar, the rows, and the selection highlight together, and
    /// the surface is one window — not a screen — so the copy is small.
    ///
    /// The ownership control is drawn on the Properties overlay only where the
    /// launching user holds `CAP_FS_CHOWN` (`overlays.can_chown`), so a session
    /// that cannot use it is never shown it (§2.24).
    fn present_frame<S, T>(
        browser: &Browser<S>,
        overlays: &Overlays,
        theme: &Theme,
        client: &mut WindowClient<T>,
        window: u64,
        frame: &mut [u8],
        mode: &DisplayMode,
    ) -> Result<(), Errno>
    where
        S: DirectorySource,
        T: WindowTransport,
    {
        let rename = overlays.rename.as_ref();
        let properties = overlays.properties.as_ref();
        let owner = overlays.owner.as_ref();
        let can_chown = overlays.can_chown;
        let viewport = Rect::new(0, 0, mode.width_px, mode.height_px);
        // Render the listing at the theme's logical UI font size (the browser
        // window is not DPI-scaled today, so the logical size is the physical
        // size), rather than a size hard-coded here.
        let font = BitmapFont::with_pixel_height(u32::from(theme.fonts().ui.size_px));
        let mut surface =
            render(browser, theme, font, viewport, MANAGER_TOOLS).ok_or(Errno::LengthOutOfRange)?;
        // In rename mode, overlay the inline editor exactly over the selected
        // item's row through the shared selection geometry, so the field sits
        // on the item the user is renaming (§2.2).
        if let Some(field) = rename {
            if let Some(bounds) =
                tairix_browse::render::selection_rect(browser, font, theme, viewport)
            {
                field.render(&mut surface, bounds, Scale::ONE, theme, font);
            }
        }
        // With the Properties overlay open, draw it centered on top of the
        // view (the shared drawn panel painting the already-authorised
        // metadata). Rename and Properties are never open together.
        if let Some(props) = properties {
            draw_properties_editable(&mut surface, props, theme, font, viewport);
            // Reassigning an owner is privileged, so the ownership control is
            // drawn only where the launching user holds `CAP_FS_CHOWN` — never
            // shown to a session that cannot use it (§2.24).
            if can_chown {
                let active = owner.map(|ed| (ed.field, &ed.editor));
                draw_owner_control(&mut surface, props, theme, font, viewport, active);
            }
        }
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

    /// Apply one delivered event to the browser, reporting whether the
    /// listing changed (and must re-present) and whether the app should
    /// end (the desktop asked the window to close).
    ///
    /// `theme` and `mode` give the reveal/scroll helpers the same font and
    /// content viewport the renderer uses, so the drawn view, the selection
    /// reveal, and the wheel scroll all agree on the geometry.
    fn apply_event<S: DirectorySource>(
        browser: &mut Browser<S>,
        overlays: &mut Overlays,
        theme: &Theme,
        mode: &DisplayMode,
        event: &WindowEvent,
    ) -> (bool, bool) {
        let font = BitmapFont::with_pixel_height(u32::from(theme.fonts().ui.size_px));
        let viewport = Rect::new(0, 0, mode.width_px, mode.height_px);

        // A close request ends the app whatever mode it is in; an open rename
        // edit or properties overlay is simply abandoned (nothing was written).
        if let WindowEvent::CloseRequested { .. } = event {
            return (false, true);
        }

        // A modal overlay (the Properties overlay, or the owner-id editor
        // nested in it) owns the window while it is open; handle it and return.
        if let Some(result) = apply_modal_event(browser, overlays, font, theme, viewport, event) {
            return result;
        }

        // Rename mode: the inline editor owns the keyboard. Its keys never
        // navigate the listing, and non-key events leave the edit untouched.
        if overlays.rename.is_some() {
            return match event {
                WindowEvent::Key {
                    key: KeyInput::Pressed { key, modifiers },
                    ..
                } => apply_rename_key(
                    browser,
                    &mut overlays.rename,
                    font,
                    theme,
                    viewport,
                    *key,
                    *modifiers,
                ),
                _ => (false, false),
            };
        }

        match event {
            WindowEvent::Key {
                key: KeyInput::Pressed { key, modifiers },
                ..
            } => {
                // Alt+Enter opens the Properties overlay (it needs the overlay
                // state); every other navigation-mode key is handled by the
                // shared `apply_nav_key`.
                if matches!(key, KeyValue::Named(NamedKeyCode::Enter)) && modifiers.alt {
                    begin_properties(browser, &mut overlays.properties)
                } else {
                    apply_nav_key(
                        browser,
                        &mut overlays.rename,
                        font,
                        theme,
                        viewport,
                        *key,
                        *modifiers,
                    )
                }
            }
            // A wheel gesture the desktop forwarded (this window owns its own
            // content scrolling): scroll the view one line per tick through
            // the shared scroll model, which clamps at both ends so a large or
            // hostile tick count cannot run past the content or spin. The
            // selection is untouched; repaint only when the offset moved.
            WindowEvent::Scrolled { dy, .. } => {
                let moved = tairix_browse::render::scroll_lines(
                    browser,
                    font,
                    theme,
                    viewport,
                    i64::from(*dy),
                );
                (moved, false)
            }
            // A pointer event the desktop routed into this window's local
            // coordinates: a primary-button press navigates (a path-bar crumb)
            // or selects (an item); every other pointer action is a no-op.
            WindowEvent::Pointer { x, y, action, .. } => {
                // A primary click on a manager write tool (New Folder) is
                // dispatched here because the write path needs the rename
                // state; all read-only pointer routing (toolbar commands,
                // path-bar crumbs, item selection) stays in `apply_pointer`,
                // the same read-only router the trusted picker uses.
                if let Some(point) = press_point(*action, *x, *y) {
                    if let Some(tool) =
                        manager_tool_at(browser, theme, viewport, point, MANAGER_TOOLS)
                    {
                        return apply_manager_tool(
                            browser,
                            &mut overlays.rename,
                            font,
                            theme,
                            viewport,
                            tool,
                        );
                    }
                }
                apply_pointer(browser, font, theme, viewport, *x, *y, *action)
            }
            // Focus changes and key releases repaint nothing. The browser
            // never requests a pick, so a pick conclusion is a session bug and
            // is ignored rather than acted on (an unredeemed delegation is
            // reclaimed by the kernel at exit).
            //
            // Minimized needs no action: the window manager hides the
            // window and keeps its taskbar entry; the browser renders on
            // demand, so there is nothing to pause. Resized cannot reach
            // this window: the browser presents a single fixed-size window
            // and does not request resizable decoration, so the window
            // manager offers it neither a maximize nor a resize grabber
            // (the size controls render disabled) and never sends it a new
            // client size. Both are honest no-ops, not deferred work.
            WindowEvent::Key { .. }
            | WindowEvent::CloseRequested { .. }
            | WindowEvent::Focus { .. }
            | WindowEvent::Minimized { .. }
            | WindowEvent::Resized { .. }
            | WindowEvent::FilePicked { .. }
            | WindowEvent::PickCancelled { .. } => (false, false),
        }
    }

    /// Handle one event while a modal overlay owns the window, returning
    /// `Some(result)` when it consumed the event and `None` when no modal
    /// overlay is open (so the caller falls through to rename / navigation).
    ///
    /// The owner-id editor is nested inside the Properties overlay, so it is
    /// checked first: while it is open its keys commit or cancel the ownership
    /// change. Otherwise the Properties overlay owns the window — `Escape`
    /// dismisses it and a primary-button press routes to a permission toggle
    /// or (for a `CAP_FS_CHOWN` holder) the owner control. Every other event
    /// is swallowed so a keystroke never navigates the view behind the overlay.
    fn apply_modal_event<S: DirectorySource>(
        browser: &mut Browser<S>,
        overlays: &mut Overlays,
        font: BitmapFont,
        theme: &Theme,
        viewport: Rect,
        event: &WindowEvent,
    ) -> Option<(bool, bool)> {
        if overlays.owner.is_some() {
            return Some(match event {
                WindowEvent::Key {
                    key: KeyInput::Pressed { key, modifiers },
                    ..
                } => apply_owner_edit_key(
                    browser,
                    &mut overlays.properties,
                    &mut overlays.owner,
                    *key,
                    *modifiers,
                ),
                _ => (false, false),
            });
        }
        if overlays.properties.is_some() {
            return Some(match event {
                WindowEvent::Key {
                    key:
                        KeyInput::Pressed {
                            key: KeyValue::Named(NamedKeyCode::Escape),
                            ..
                        },
                    ..
                } => {
                    overlays.properties = None;
                    (true, false)
                }
                WindowEvent::Pointer { x, y, action, .. } => match press_point(*action, *x, *y) {
                    Some(point) => {
                        apply_properties_pointer(browser, overlays, font, theme, viewport, point)
                    }
                    None => (false, false),
                },
                _ => (false, false),
            });
        }
        None
    }

    /// Handle one key press in navigation mode (not renaming, not showing the
    /// Properties overlay), reporting whether the view changed (it never asks
    /// the app to close). Mirrors [`apply_rename_key`]'s shape; Alt+Enter
    /// (Properties) is handled by the caller, which owns the overlay state.
    fn apply_nav_key<S: DirectorySource>(
        browser: &mut Browser<S>,
        rename: &mut Option<TextField>,
        font: BitmapFont,
        theme: &Theme,
        viewport: Rect,
        key: KeyValue,
        modifiers: AbiModifiers,
    ) -> (bool, bool) {
        match key {
            // Toolbar-command accelerators: Alt+←/→/↑ drive the history and
            // climb commands, F5 refreshes — the same shared dispatch a toolbar
            // click uses, so the keyboard and the toolbar cannot disagree (§2.2).
            KeyValue::Named(NamedKeyCode::Left) if modifiers.alt => {
                apply_toolbar_command(browser, font, theme, viewport, ToolbarCommand::Back)
            }
            KeyValue::Named(NamedKeyCode::Right) if modifiers.alt => {
                apply_toolbar_command(browser, font, theme, viewport, ToolbarCommand::Forward)
            }
            KeyValue::Named(NamedKeyCode::Up) if modifiers.alt => {
                apply_toolbar_command(browser, font, theme, viewport, ToolbarCommand::Up)
            }
            KeyValue::Named(NamedKeyCode::F5) => {
                apply_toolbar_command(browser, font, theme, viewport, ToolbarCommand::Refresh)
            }
            // Ctrl+Shift+N: the keyboard equivalent of the New Folder tool.
            // Shift may deliver 'n' upper- or lower-case, so match either.
            KeyValue::Char(ch)
                if modifiers.ctrl && modifiers.shift && ch.eq_ignore_ascii_case(&'n') =>
            {
                begin_new_folder(browser, rename, font, theme, viewport)
            }
            KeyValue::Named(NamedKeyCode::Down) => {
                browser.select_next();
                tairix_browse::render::reveal_selection(browser, font, theme, viewport);
                (true, false)
            }
            KeyValue::Named(NamedKeyCode::Up) => {
                browser.select_previous();
                tairix_browse::render::reveal_selection(browser, font, theme, viewport);
                (true, false)
            }
            KeyValue::Named(NamedKeyCode::Enter) => {
                // Opening a file (or an unreadable directory) is a refused
                // no-op today: the browser lists, it does not launch. The
                // listing stays as it was.
                (browser.open_selected().is_ok(), false)
            }
            KeyValue::Named(NamedKeyCode::Backspace) => (browser.go_up().unwrap_or(false), false),
            // F2 begins an in-place rename of the selected item; with nothing
            // selected (an empty directory) it is a no-op.
            KeyValue::Named(NamedKeyCode::F2) => {
                begin_rename(browser, rename, font, theme, viewport)
            }
            _ => (false, false),
        }
    }

    /// The window-local [`Point`] of a primary-button press, or `None` for any
    /// other pointer action. The one place the primary-press gate and the
    /// wire-coordinate conversion live, shared by the write-tool dispatch and
    /// the read-only [`apply_pointer`] routing so they cannot disagree (§2.2).
    fn press_point(action: PointerAction, x: u32, y: u32) -> Option<Point> {
        if action != PointerAction::Pressed(PointerButtonCode::Primary) {
            return None;
        }
        Some(Point::new(
            i32::try_from(x).unwrap_or(i32::MAX),
            i32::try_from(y).unwrap_or(i32::MAX),
        ))
    }

    /// Apply one routed pointer event in navigation mode, reporting whether the
    /// view changed (and must re-present).
    ///
    /// Only a primary-button press acts, and it is the spelling of one user
    /// intent, never an escalation: a click on a path-bar crumb climbs to that
    /// ancestor through the same transactional [`Browser::navigate_to_depth`]
    /// the keyboard uses (a refused re-listing leaves the browser exactly where
    /// it was); a click on an item selects it. A click on the inert current
    /// crumb, a separator gap, or empty space resolves to nothing and repaints
    /// nothing. Opening an item stays keyboard-driven until the launch/open
    /// stage wires the spawn path.
    fn apply_pointer<S: DirectorySource>(
        browser: &mut Browser<S>,
        font: BitmapFont,
        theme: &Theme,
        viewport: Rect,
        x: u32,
        y: u32,
        action: PointerAction,
    ) -> (bool, bool) {
        let Some(point) = press_point(action, x, y) else {
            return (false, false);
        };
        // A click on a toolbar command runs it through the same shared dispatch
        // the keyboard accelerators use; a disabled command resolves to nothing
        // (`toolbar_command_at` fails closed) and repaints nothing.
        if let Some(command) =
            tairix_browse::render::toolbar_command_at(browser, theme, viewport, point)
        {
            return apply_toolbar_command(browser, font, theme, viewport, command);
        }
        if let Some(depth) = tairix_browse::render::crumb_at(browser, font, theme, viewport, point)
        {
            let moved = browser.navigate_to_depth(depth).unwrap_or(false);
            if moved {
                tairix_browse::render::reveal_selection(browser, font, theme, viewport);
            }
            return (moved, false);
        }
        if let Some(index) =
            tairix_browse::render::entry_index_at(browser, font, theme, viewport, point)
        {
            return (browser.select(index).is_ok(), false);
        }
        (false, false)
    }

    /// Run a toolbar `command` against the browser through the one shared
    /// [`tairix_browse::apply_command`] dispatch (so a toolbar click and its
    /// keyboard accelerator can never diverge), revealing the selection and
    /// reporting a repaint when the view changed. A navigation refused by the
    /// VFS leaves the browser exactly where it was (fail closed) and repaints
    /// nothing.
    fn apply_toolbar_command<S: DirectorySource>(
        browser: &mut Browser<S>,
        font: BitmapFont,
        theme: &Theme,
        viewport: Rect,
        command: ToolbarCommand,
    ) -> (bool, bool) {
        match tairix_browse::apply_command(browser, command) {
            Ok(true) => {
                tairix_browse::render::reveal_selection(browser, font, theme, viewport);
                (true, false)
            }
            Ok(false) | Err(_) => (false, false),
        }
    }

    /// Dispatch a manager write `tool` (a toolbar click or its keyboard
    /// equivalent). New Folder is the only write tool today.
    fn apply_manager_tool<S: DirectorySource>(
        browser: &mut Browser<S>,
        rename: &mut Option<TextField>,
        font: BitmapFont,
        theme: &Theme,
        viewport: Rect,
        tool: ManagerTool,
    ) -> (bool, bool) {
        match tool {
            ManagerTool::NewFolder => begin_new_folder(browser, rename, font, theme, viewport),
        }
    }

    /// Create a new folder in the current directory and open the inline rename
    /// on it, so the user names it immediately (the standard new-folder flow).
    ///
    /// The placeholder name is disambiguated against the current listing
    /// ([`suggest_new_dir_name`]) and the create is an ordinary
    /// permission-checked `fs_mkdir` under the user's own identity — no new
    /// capability; the per-inode owner/mode/ACL model gates it. The engine
    /// validates before the syscall and is transactional: a refused create
    /// leaves the listing exactly as it was and states its reason on `stderr`
    /// (an honest answer, never a crash or a fabricated folder). On success the
    /// engine has selected the new folder, so the rename editor opens on it.
    fn begin_new_folder<S: DirectorySource>(
        browser: &mut Browser<S>,
        rename: &mut Option<TextField>,
        font: BitmapFont,
        theme: &Theme,
        viewport: Rect,
    ) -> (bool, bool) {
        let name = suggest_new_dir_name(browser.entries());
        match browser.create_directory(&name, |path| {
            let ret = tairix_rt::fs_mkdir(path.as_bytes());
            if ret == 0 {
                Ok(())
            } else {
                Err(errno_from(ret))
            }
        }) {
            Ok(()) => begin_rename(browser, rename, font, theme, viewport),
            Err(err) => {
                let _ = tairix_rt::stderr(b"files: ");
                let _ = tairix_rt::stderr(err.message().as_bytes());
                let _ = tairix_rt::stderr(b"\n");
                (false, false)
            }
        }
    }

    /// Begin an in-place rename of the selected item: reveal the row so the
    /// editor is on screen, then open a focused [`TextField`] pre-filled with
    /// the current name (bounded by the kernel's own `FS_NAME_MAX`). With
    /// nothing selected it is a no-op.
    fn begin_rename<S: DirectorySource>(
        browser: &mut Browser<S>,
        rename: &mut Option<TextField>,
        font: BitmapFont,
        theme: &Theme,
        viewport: Rect,
    ) -> (bool, bool) {
        let Some(name) = browser.selected_name().map(ToString::to_string) else {
            return (false, false);
        };
        tairix_browse::render::reveal_selection(browser, font, theme, viewport);
        let mut field = TextField::new().with_text(&name).with_max_len(FS_NAME_MAX);
        field.set_focused(true);
        *rename = Some(field);
        (true, false)
    }

    /// Open the Properties overlay for the selected item: name its path
    /// through the shared spelling, read its metadata with one
    /// capability-checked `fs_stat` under the user's own identity, and store
    /// the display-ready [`Properties`] the overlay paints. With nothing
    /// selected (an empty directory) it is a no-op.
    ///
    /// Showing properties is an incidental, refusable action: if the item can
    /// no longer be named or its metadata cannot be read (it vanished, or is
    /// unreadable), the refusal is stated on `stderr` and the overlay simply
    /// stays closed — an answer, not a crash, and never a fabricated summary
    /// (§2.24, §5.4). A directory or sealed bundle is opened with the
    /// directory flag, a regular file read-only; `stat` needs only a live
    /// handle either way.
    fn begin_properties<S: DirectorySource>(
        browser: &Browser<S>,
        properties: &mut Option<Properties>,
    ) -> (bool, bool) {
        // With nothing selected (an empty directory) it is a silent no-op.
        if browser.selected_entry().is_none() {
            return (false, false);
        }
        if let Some(props) = stat_selected_properties(browser) {
            *properties = Some(props);
            (true, false)
        } else {
            let _ = tairix_rt::stderr(b"files: properties unavailable for that item\n");
            (false, false)
        }
    }

    /// Read the selected item's metadata into a display-ready [`Properties`]:
    /// name its path through the shared spelling and `fs_stat` it with one
    /// capability-checked handle under the user's own identity (no new
    /// capability). Returns `None` when there is no selection, the item can no
    /// longer be named, or its metadata cannot be read — the caller decides
    /// whether that is a silent no-op or a stated refusal (§2.24, §5.4). A
    /// directory or sealed bundle is opened with the directory flag, a regular
    /// file read-only; `stat` needs only a live handle either way.
    ///
    /// One definition shared by opening the overlay ([`begin_properties`]) and
    /// refreshing it after a permission change, so the two cannot drift (§2.2).
    fn stat_selected_properties<S: DirectorySource>(browser: &Browser<S>) -> Option<Properties> {
        let entry = browser.selected_entry()?;
        let kind = entry.kind();
        let name = entry.name().to_string();
        let Some(Ok(path)) = browser.selected_target_path() else {
            return None;
        };
        let flags = if matches!(kind, EntryKind::File) {
            OpenFlags::READ
        } else {
            OpenFlags::DIRECTORY
        };
        let stat = tairix_rt::File::open(path.as_bytes(), flags)
            .and_then(|file| file.stat())
            .ok()?;
        Some(Properties::from_stat(name, kind, &stat))
    }

    /// Apply a primary-button press inside the open Properties overlay: if it
    /// landed on one of the nine permission toggles, flip that `rwx` bit and
    /// commit the new mode through the browser's own capability-checked
    /// [`Browser::set_mode_selected`] over `fs_set_mode` under the user's own
    /// identity (no new capability — the per-inode owner/mode/ACL model gates
    /// it). A press elsewhere in the overlay changes nothing.
    ///
    /// The toggle flips only its own `rwx` bit and preserves the current
    /// setuid/setgid/sticky bits (the settable word masked by [`FS_MODE_MASK`],
    /// dropping the non-settable file-type bits `fs_stat` also reports). On
    /// success the overlay is re-stat'd so it reflects the applied mode; a VFS
    /// refusal leaves the node's mode exactly as it was and states its reason
    /// on `stderr` — an honest answer, never a crash or a fabricated success
    /// (§2.24, §5.4).
    fn apply_permission_toggle<S: DirectorySource>(
        browser: &mut Browser<S>,
        properties: &mut Option<Properties>,
        font: BitmapFont,
        theme: &Theme,
        viewport: Rect,
        point: Point,
    ) -> (bool, bool) {
        let Some(bit) = permission_cell_at(viewport, font, theme, point) else {
            return (false, false);
        };
        let Some(props) = properties.as_ref() else {
            return (false, false);
        };
        let new_mode = (props.mode() & FS_MODE_MASK) ^ bit;
        match browser.set_mode_selected(new_mode, |path, mode| {
            let ret = tairix_rt::fs_set_mode(path.as_bytes(), mode);
            if ret == 0 {
                Ok(())
            } else {
                Err(errno_from(ret))
            }
        }) {
            Ok(()) => {
                // Re-read the node so the overlay shows the applied mode; if the
                // re-stat fails the commit still succeeded, so keep the panel.
                if let Some(updated) = stat_selected_properties(browser) {
                    *properties = Some(updated);
                }
                (true, false)
            }
            Err(err) => {
                let _ = tairix_rt::stderr(b"files: ");
                let _ = tairix_rt::stderr(err.message().as_bytes());
                let _ = tairix_rt::stderr(b"\n");
                (false, false)
            }
        }
    }

    /// Route a primary-button press inside the open Properties overlay: a
    /// press on one of the nine permission toggles commits a mode change
    /// (`apply_permission_toggle`); otherwise, where the user holds
    /// `CAP_FS_CHOWN`, a press on the owner or group value opens the inline id
    /// editor for that field. A press elsewhere changes nothing (fail closed,
    /// §5.4).
    ///
    /// The permission and owner cells sit on different rows, so a press
    /// resolves to at most one of them; the permission row is checked first so
    /// its (capability-free) toggles are never shadowed by the owner control.
    fn apply_properties_pointer<S: DirectorySource>(
        browser: &mut Browser<S>,
        overlays: &mut Overlays,
        font: BitmapFont,
        theme: &Theme,
        viewport: Rect,
        point: Point,
    ) -> (bool, bool) {
        if permission_cell_at(viewport, font, theme, point).is_some() {
            return apply_permission_toggle(
                browser,
                &mut overlays.properties,
                font,
                theme,
                viewport,
                point,
            );
        }
        if overlays.can_chown {
            if let Some(props) = overlays.properties.as_ref() {
                if let Some(field) = owner_field_at(props, viewport, font, theme, point) {
                    return begin_owner_edit(props, &mut overlays.owner, field);
                }
            }
        }
        (false, false)
    }

    /// Open the inline id editor over the clicked owner or group value,
    /// pre-filled with the current id and bounded to a `u32`'s ten digits.
    ///
    /// The caller has already confirmed the user holds `CAP_FS_CHOWN` and that
    /// the press landed on `field`'s value; the kernel still authorises the
    /// eventual commit under the user's own identity (the editor holds no
    /// authority).
    fn begin_owner_edit(
        props: &Properties,
        owner: &mut Option<OwnerEditor>,
        field: OwnerField,
    ) -> (bool, bool) {
        let current = match field {
            OwnerField::Uid => props.uid(),
            OwnerField::Gid => props.gid(),
        };
        let mut editor = TextField::new()
            .with_text(current.to_string())
            .with_max_len(OWNER_ID_MAX_DIGITS);
        editor.set_focused(true);
        *owner = Some(OwnerEditor { field, editor });
        (true, false)
    }

    /// Feed one key to the open owner-id editor. A submit commits the reassigned
    /// id through `fs_set_owner` (closing the editor and refreshing the panel on
    /// success, or stating the refusal reason in the field and staying open); a
    /// cancel abandons the edit; an edit repaints and live-validates the typed
    /// id so a non-numeric or out-of-range value is flagged as the user types.
    fn apply_owner_edit_key<S: DirectorySource>(
        browser: &mut Browser<S>,
        properties: &mut Option<Properties>,
        owner: &mut Option<OwnerEditor>,
        key: KeyValue,
        modifiers: AbiModifiers,
    ) -> (bool, bool) {
        let (editor_key, mods) = to_editor_key(key, modifiers);
        let action = match owner.as_mut() {
            Some(ed) => ed.editor.on_key(editor_key, mods),
            None => return (false, false),
        };
        match action {
            Some(TextAction::Submitted) => commit_owner_edit(browser, properties, owner),
            Some(TextAction::Cancelled) => {
                *owner = None;
                (true, false)
            }
            Some(TextAction::Edited) => {
                if let Some(ed) = owner.as_mut() {
                    let text = ed.editor.text().to_string();
                    ed.editor.set_message(owner_id_message(&text));
                }
                (true, false)
            }
            None => (false, false),
        }
    }

    /// Commit the open owner-id editor: parse the typed value as a `u32` id and
    /// apply it to the selected node through the browser's own
    /// capability-checked [`Browser::set_owner_selected`] over `fs_set_owner`,
    /// under the user's own identity. On success the editor closes and the
    /// panel is re-stat'd to reflect the new owner; a non-numeric/out-of-range
    /// value or a VFS refusal (including the missing-`CAP_FS_CHOWN` denial)
    /// states its reason in the field and keeps the editor open — an honest
    /// answer, never a silent or fabricated result (§2.24, §5.4).
    fn commit_owner_edit<S: DirectorySource>(
        browser: &mut Browser<S>,
        properties: &mut Option<Properties>,
        owner: &mut Option<OwnerEditor>,
    ) -> (bool, bool) {
        let Some(ed) = owner.as_ref() else {
            return (false, false);
        };
        let field = ed.field;
        let text = ed.editor.text().to_string();
        let Ok(id) = text.parse::<u32>() else {
            if let Some(ed) = owner.as_mut() {
                ed.editor.set_message(Some(String::from(OWNER_ID_HINT)));
            }
            return (true, false);
        };
        let change = match field {
            OwnerField::Uid => OwnerChange::user(id),
            OwnerField::Gid => OwnerChange::group(id),
        };
        match browser.set_owner_selected(change, |path, uid, gid| {
            let ret = tairix_rt::fs_set_owner(path.as_bytes(), uid, gid);
            if ret == 0 {
                Ok(())
            } else {
                Err(errno_from(ret))
            }
        }) {
            Ok(()) => {
                *owner = None;
                if let Some(updated) = stat_selected_properties(browser) {
                    *properties = Some(updated);
                }
                (true, false)
            }
            Err(err) => {
                if let Some(ed) = owner.as_mut() {
                    ed.editor.set_message(Some(String::from(err.message())));
                }
                (true, false)
            }
        }
    }

    /// The live-validation message for a typed owner/group id, or `None` when
    /// the text is a well-formed, assignable `u32` id. It never blocks typing;
    /// it only flags a value the commit would reject.
    fn owner_id_message(text: &str) -> Option<String> {
        match text.parse::<u32>() {
            Ok(id) if id != tairix_abi::fs::FS_OWNER_UNCHANGED => None,
            _ => Some(String::from(OWNER_ID_HINT)),
        }
    }

    /// Feed one key to the open rename editor. A submit commits the new name
    /// through `fs_rename` (closing the editor and following the selection on
    /// success, or stating the refusal reason in the field and staying open);
    /// a cancel abandons the edit; an edit repaints and live-validates the
    /// typed name so a clash or bad character is flagged as the user types.
    fn apply_rename_key<S: DirectorySource>(
        browser: &mut Browser<S>,
        rename: &mut Option<TextField>,
        font: BitmapFont,
        theme: &Theme,
        viewport: Rect,
        key: KeyValue,
        modifiers: AbiModifiers,
    ) -> (bool, bool) {
        let (editor_key, mods) = to_editor_key(key, modifiers);
        let action = match rename.as_mut() {
            Some(field) => field.on_key(editor_key, mods),
            None => return (false, false),
        };
        match action {
            Some(TextAction::Submitted) => {
                let Some(new_name) = rename.as_ref().map(|f| f.text().to_string()) else {
                    return (false, false);
                };
                match browser.rename_selected(&new_name, |from, to| {
                    let ret = tairix_rt::fs_rename(from.as_bytes(), to.as_bytes());
                    if ret == 0 {
                        Ok(())
                    } else {
                        Err(errno_from(ret))
                    }
                }) {
                    // A committed rename (or a no-op rename to the same name)
                    // closes the editor; the selection follows the entry.
                    Ok(()) | Err(RenameError::Unchanged) => {
                        *rename = None;
                        tairix_browse::render::reveal_selection(browser, font, theme, viewport);
                        (true, false)
                    }
                    // A refused rename stays open with the honest reason shown
                    // in the field (§2.24 — never a silent or fabricated
                    // result); the listing is untouched.
                    Err(err) => {
                        if let Some(field) = rename.as_mut() {
                            field.set_message(Some(String::from(err.message())));
                        }
                        (true, false)
                    }
                }
            }
            Some(TextAction::Cancelled) => {
                *rename = None;
                (true, false)
            }
            Some(TextAction::Edited) => {
                let current = browser.selected_name().map(ToString::to_string);
                if let (Some(field), Some(current)) = (rename.as_mut(), current) {
                    let text = field.text().to_string();
                    let message = match validate_new_name(&text, &current, browser.entries()) {
                        Ok(()) | Err(RenameError::Unchanged) => None,
                        Err(err) => Some(String::from(err.message())),
                    };
                    field.set_message(message);
                }
                (true, false)
            }
            None => (false, false),
        }
    }

    /// Map the window channel's wire key event onto the desktop control
    /// vocabulary the shared [`TextField`] consumes.
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

    /// Map a wire [`NamedKeyCode`] onto the desktop [`NamedKey`]. The two sets
    /// are the producer/consumer halves of one keyboard vocabulary, so this is
    /// a total mapping with no guessing.
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
    /// event loop parks on, returning `(endpoint, set)`.
    ///
    /// The endpoint id is unique by construction (the shared
    /// `event_endpoint_for` naming rule: this task's never-reused kernel id
    /// under a fixed tag) and never a reserved endpoint; the bind is refused
    /// otherwise. On any refusal it states the reason on `stderr` and returns
    /// the reserved fail-closed [`EXIT_NO_EVENTS`] code for `main` to exit
    /// with, so the app exits rather than degrade into a busy re-poll.
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
            WaitSetOp::Add,
            WaitSourceKind::Port,
            event_endpoint,
            EVENT_TOKEN,
        ) != 0
        {
            return Err(fail(EXIT_NO_EVENTS, "event mailbox wait refused"));
        }
        Ok((event_endpoint, set))
    }

    /// Program entry point. `tairix-rt`'s `_start` calls it once the
    /// runtime is set up and routes its return value through the `exit`
    /// syscall.
    fn main() -> i32 {
        // --- The browser over the live, capability-checked listing call.
        let source = VfsDirectorySource::new(|path: &str| {
            tairix_rt::read_dir_all(path.as_bytes()).map_err(errno_from)
        });
        let Ok(mut browser) = Browser::open_root(source) else {
            return fail(EXIT_NO_LISTING, "root directory listing refused");
        };

        // --- The shared window surface: FRAME_COUNT frames shaped as the
        // window mode, created here and granted to the session.
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
        // SAFETY: the kernel mapped at least `total` zeroed bytes
        // read/write into this process at `base` (`shm_create` maps the
        // exact length it was asked for) and the mapping stays live for
        // the life of the process — nothing below unmaps or aliases it.
        // The session maps the same frames read-only for its blit, and
        // the protocol serialises access: this app is parked in its
        // present call while the session reads.
        let frames = unsafe { core::slice::from_raw_parts_mut(base as *mut u8, total) };

        // --- The event mailbox the app parks on, bound and added to a
        // fresh wait-set (a bring-up refusal exits fail-loud with its code).
        let (event_endpoint, set) = match bind_event_mailbox() {
            Ok(pair) => pair,
            Err(code) => return code,
        };

        // --- Open the window and paint the first listing.
        let mut client = WindowClient::new(RtWindowTransport);
        #[allow(clippy::cast_sign_loss)] // `grant >= 1` checked above; it is a kernel handle.
        let Ok((window, server)) = client.create(
            grant as u64,
            event_endpoint,
            FRAME_COUNT,
            &mode,
            "Files",
            false,
        ) else {
            return fail(EXIT_NO_WINDOW, "desktop session refused the window");
        };
        let themes = ThemeRegistry::with_builtins();
        let theme = themes.active();
        // The transient overlay state (rename / Properties / owner editor),
        // threaded through the event loop so the painted overlays and the
        // state they reflect stay in step. `can_chown` is whether the
        // launching user holds `CAP_FS_CHOWN` — read once from the
        // kernel-attested self-origin (a refused query fails closed to "not
        // held") — so the ownership control is offered only where it can be
        // used (§2.24).
        let mut overlays = Overlays {
            rename: None,
            properties: None,
            owner: None,
            can_chown: tairix_rt::self_origin()
                .is_ok_and(|origin| origin.capabilities().holds_cap(CapabilityId::FS_CHOWN)),
        };
        if present_frame(
            &browser,
            &overlays,
            theme,
            &mut client,
            window,
            frames,
            &mode,
        )
        .is_err()
        {
            return fail(EXIT_CHANNEL_LOST, "first present refused");
        }

        // --- The event loop: park, apply, repaint. A dead channel ends
        // the app fail-loud; a clean close ends it at zero.
        let mut events = WindowEvents::new(RtEventSource {
            endpoint: event_endpoint,
            set,
            server,
        });
        loop {
            let event = match events.wait() {
                Ok(event) => event,
                // A malformed frame from the authenticated session is
                // refused and the app keeps waiting (never guessed at).
                Err(Errno::OutOfRange | Errno::BadMagic | Errno::BufferTooSmall) => continue,
                Err(_) => return fail(EXIT_CHANNEL_LOST, "event channel lost"),
            };
            let (changed, close) = apply_event(&mut browser, &mut overlays, theme, &mode, &event);
            if close {
                // The desktop asked; close the window and end cleanly.
                let _ = client.close(window);
                return 0;
            }
            if changed
                && present_frame(
                    &browser,
                    &overlays,
                    theme,
                    &mut client,
                    window,
                    frames,
                    &mode,
                )
                .is_err()
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
