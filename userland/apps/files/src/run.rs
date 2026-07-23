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
//! activates the selection — it descends into a directory or launches a
//! selected `<Name>.app` bundle by spawning the bundle's own `Run` through
//! the ordinary signed app-load gate (asynchronously, with the launched
//! child reaped on the wait-set's any-child member so it is never left a
//! zombie), and a launch refusal is stated fail-loud on `stderr`;
//! `Backspace` goes up); `F2` renames the selected
//! item through an inline `lib/controls` text field, committing over
//! `fs_rename` under the user's own identity (a refusal is stated in the
//! field, never a silent failure or a fabricated success); `Ctrl+X`/`Ctrl+C`
//! capture the selection onto a cut/copy clipboard and `Ctrl+V` pastes it
//! into the current directory — a same-volume move is one `fs_rename`, a
//! cross-volume move is copy-then-delete, and a copy streams the bytes in
//! bounded chunks, all under the user's own identity and stopping fail-loud
//! on `stderr` at the first refusal; `Delete` opens a modal confirmation
//! `Dialog` and, on confirm, removes the selection (recursively for a folder)
//! over the user's own `fs_unlink`s, stopping and stating the reason on
//! `stderr` on the first refusal; a `CloseRequested` from the desktop closes
//! the window and ends the program cleanly. Every bring-up refusal exits
//! fail-loud with a reserved code and a stated reason on `stderr`.
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

    use alloc::collections::BTreeMap;
    use alloc::string::{String, ToString};
    use core::cell::RefCell;

    use tairix_abi::driver::display::{DamageRect, DisplayFormat, DisplayMode};
    use tairix_abi::fs::{FileKind, OpenFlags, FS_IO_MAX, FS_MODE_MASK, FS_NAME_MAX};
    use tairix_abi::input::{
        KeyInput, KeyValue, Modifiers as AbiModifiers, NamedKeyCode, PointerButtonCode,
    };
    use tairix_abi::window_ipc::{PointerAction, WindowEvent, WINDOW_ENDPOINT};
    use tairix_abi::{
        load_failure_reason, CapabilityId, Errno, Origin, ProcId, UnlinkFlags, WaitFlags,
        WaitSetOp, WaitSourceKind, WaitStatus, ORIGIN_WIRE_LEN, WAITSET_CHILD_ANY, WAIT_PID_ANY,
    };
    use tairix_browse::render::{
        build_context_menu, build_delete_dialog, context_menu_command_at, delete_dialog_action_at,
        draw_context_menu, draw_delete_dialog, draw_owner_control, draw_properties_editable,
        manager_tool_at, owner_field_at, permission_cell_at, render, OwnerField,
        DELETE_CANCEL_INDEX, DELETE_CONFIRM_INDEX,
    };
    use tairix_browse::{
        paste_strategy, plan_paste, suggest_new_dir_name, validate_new_name, Activation, Browser,
        Clipboard, ClipboardOp, ContextCommand, ContextMenuModel, CopyAction, CopyCursor, CopyWalk,
        DeleteAction, DeletePlan, DeleteWalk, DirectorySource, EntryKind, ManagerTool, OwnerChange,
        PasteItem, PasteStrategy, Properties, RenameError, ToolbarCommand, VfsDirectorySource,
        VolumeId, MANAGER_TOOLS, WIN_HEIGHT, WIN_WIDTH,
    };
    use tairix_controls::decision::Dialog;
    use tairix_controls::text::{TextAction, TextField};
    use tairix_controls::Menu;
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

    /// The wait-set token of the any-child member: a bundle the file manager
    /// launched has exited, so it is reaped promptly (never left a zombie,
    /// and never a busy-poll — the member is drained the instant it wakes).
    const CHILD_TOKEN: u64 = 2;

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

    /// The file manager's launched children: the application bundles it
    /// spawned when the user activated a `<Name>.app`, tracked by PID so each
    /// is reaped when it exits (never left a zombie) and a load refusal is
    /// named in the fail-loud diagnosis.
    ///
    /// Launching is asynchronous: [`spawn`](tairix_rt::spawn) admits the child
    /// and returns its PID before the bundle's image is loaded, so a load
    /// refusal (a missing, unverified, or malformed bundle) surfaces later as
    /// the child's reserved `LOAD_*` exit status, reported by
    /// [`reap`](Self::reap) under the bundle name. The launched app is the
    /// user's own — it runs under the launching user's identity and receives
    /// only the manager's manifest set intersected with that user's grants, so
    /// launching grants no ambient authority.
    struct Launcher {
        /// PID → bundle leaf name, for every launched child not yet reaped.
        /// Bounded by the children in flight; an entry is removed when its
        /// child is reaped, so it never grows beyond that.
        in_flight: BTreeMap<u64, String>,
    }

    impl Launcher {
        /// An idle launcher with no children in flight.
        fn new() -> Self {
            Self {
                in_flight: BTreeMap::new(),
            }
        }

        /// Launch the application bundle whose directory is `bundle_path` (the
        /// validated absolute path the activation named, e.g. `/Apps/Notes.app`)
        /// by spawning its own `Run` binary through the ordinary signed
        /// app-load gate — never a private path.
        ///
        /// The spawn is admitted immediately and the child loads on its own
        /// task, so the event loop never blocks behind a load. A synchronous
        /// refusal (a stripped spawn capability or a malformed path, decided
        /// before any child exists) is stated fail-loud on `stderr` at once; a
        /// load refusal that only shows once the image is read surfaces later
        /// through [`reap`](Self::reap). Either way the file manager carries on
        /// — a refused launch is an answer, not a crash (§2.24).
        fn launch(&mut self, bundle_path: &str) {
            let label = bundle_leaf(bundle_path);
            let mut run_path = String::from(bundle_path);
            run_path.push_str("/Run");
            let ret = tairix_rt::spawn(run_path.as_bytes());
            if ret < 0 {
                report_error(&alloc::format!("could not launch {label}"));
                return;
            }
            #[allow(clippy::cast_sign_loss)] // `ret >= 0` in this branch; it is a PID.
            self.in_flight.insert(ret as u64, label);
        }

        /// Reap every child that has exited, naming a load refusal in the
        /// fail-loud diagnosis.
        ///
        /// Non-blocking and drained fully: the loop ends the instant no zombie
        /// remains (the non-blocking `wait` yields nothing once none is left),
        /// so a burst of exits is handled in one wake and nothing spins. A
        /// clean exit, or an ordinary non-zero exit outside the reserved
        /// `LOAD_*` band, is the launched app's own business and is not
        /// reported as a launch failure.
        fn reap(&mut self) {
            loop {
                let mut status = WaitStatus::Exited(0);
                let pid = tairix_rt::wait(WAIT_PID_ANY, &mut status, WaitFlags::NONBLOCK);
                if pid <= 0 {
                    // No child ready (WouldBlock) or none left: stop draining.
                    return;
                }
                #[allow(clippy::cast_sign_loss)] // guarded by `pid > 0`.
                let label = self.in_flight.remove(&(pid as u64));
                if let WaitStatus::Exited(code) = status {
                    if let Some(reason) = load_failure_reason(code) {
                        let name = label.as_deref().unwrap_or("an application");
                        report_error(&alloc::format!("{name} failed to launch: {reason}"));
                    }
                }
            }
        }
    }

    /// The bundle directory's leaf name (`/Apps/Notes.app` → `Notes.app`) — the
    /// label the fail-loud launch diagnosis names, carrying no path prefix
    /// beyond the bundle name the user already sees. An empty or `/`-only path
    /// (which the validated activation path never produces) falls back to the
    /// whole string rather than an empty name.
    fn bundle_leaf(bundle_path: &str) -> String {
        let leaf = bundle_path.rsplit('/').find(|part| !part.is_empty());
        String::from(leaf.unwrap_or(bundle_path))
    }

    /// The production [`EventSource`]: drain the app's own event
    /// mailbox, parking on the wait-set whenever it is empty, and accept
    /// only events whose kernel-attested sender is the desktop session
    /// named by the create reply — anything else is dropped (fail
    /// closed), so no other process can feed the app forged input.
    ///
    /// The same wait-set carries the any-child member ([`CHILD_TOKEN`]): a
    /// launched bundle exiting wakes the park, and the source reaps it in place
    /// before re-parking, so a child is never left a zombie and the wake never
    /// degrades into a busy-poll.
    struct RtEventSource<'a> {
        /// The app's event-mailbox endpoint id.
        endpoint: u64,
        /// The wait-set handle the app parks on.
        set: u64,
        /// The only sender whose events are accepted.
        server: ProcId,
        /// The launched-bundle bookkeeping reaped on a [`CHILD_TOKEN`] wake,
        /// shared with the activation path that spawns.
        launcher: &'a RefCell<Launcher>,
    }

    impl EventSource for RtEventSource<'_> {
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
                        // delivery — or a launched bundle's exit — wakes the
                        // wait-set, never a spin.
                        let mut token = 0u64;
                        if tairix_rt::waitset_wait(self.set, u64::MAX, &mut token) != 0 {
                            return Err(Errno::NotFound);
                        }
                        // A child-exit wake reaps the exited bundle(s) in
                        // place before re-parking, so a launched app is never
                        // left a zombie and the ready child member cannot spin
                        // the park (it is drained the instant it fires).
                        if token == CHILD_TOKEN {
                            self.launcher.borrow_mut().reap();
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

    /// The open delete-confirmation dialog and the plan it would carry out.
    ///
    /// `None` unless the user pressed `Delete` on a selection; the event loop
    /// threads it so the painted modal dialog and the pending removal stay in
    /// step. It owns the window while open: keys and clicks either confirm or
    /// cancel and nothing navigates the view behind it. The plan is captured
    /// when the dialog opens, so a listing change while it is up cannot move
    /// what the confirmed delete targets.
    struct DeleteConfirm {
        /// The modal confirmation dialog (Delete / Cancel).
        dialog: Dialog,
        /// What the confirmed delete would remove, captured at open time.
        plan: DeletePlan,
    }

    /// The open right-click context menu: the built [`Menu`] and the
    /// window-local point it is anchored at.
    ///
    /// `None` unless the user right-clicked; the event loop threads it so the
    /// painted menu and the anchor its hit-test mirrors stay in step. It opens
    /// only in navigation mode (no other overlay is up), owns input while
    /// open, and performs nothing itself — a chosen command runs the user's
    /// own verb, exactly the paths the toolbar and keyboard drive.
    struct ContextMenu {
        /// The window-local right-click point the menu is placed at and
        /// hit-tested against (one anchor, so paint and click agree).
        anchor: Point,
        /// The drawn menu, built from the shared context-menu model.
        menu: Menu,
    }

    /// The transient overlay state layered over the browser view, threaded
    /// through the event loop so the painted overlays and the state they
    /// reflect stay in step. At most one of `rename`/`properties`/`delete` is
    /// open at a time; `owner` is nested inside `properties`.
    struct Overlays {
        /// The in-place rename editor, when open (`F2`).
        rename: Option<TextField>,
        /// The Properties overlay, when open (`Alt+Enter`).
        properties: Option<Properties>,
        /// The inline owner/group id editor on the Properties overlay.
        owner: Option<OwnerEditor>,
        /// The delete-confirmation dialog, when open (`Delete`).
        delete: Option<DeleteConfirm>,
        /// The right-click context menu, when open (secondary-button press).
        menu: Option<ContextMenu>,
        /// The held cut/copy clipboard, captured by `Ctrl+X`/`Ctrl+C` and
        /// consumed by `Ctrl+V`. It lives in the app (not the browser), so it
        /// survives navigating to the paste target; a `Cut` is cleared once
        /// pasted (its sources have moved), a `Copy` is kept so it can be
        /// pasted again elsewhere.
        clipboard: Option<Clipboard>,
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
        // The delete-confirmation dialog is modal: drawn last, on top of the
        // view, and never open together with the rename/Properties overlays.
        if let Some(confirm) = overlays.delete.as_ref() {
            draw_delete_dialog(&mut surface, &confirm.dialog, theme, font, viewport);
        }
        // The right-click context menu draws last, on top of the view. It opens
        // only in navigation mode, so it never overlaps the modal overlays
        // above; drawing it last keeps it topmost regardless.
        if let Some(ctx) = overlays.menu.as_ref() {
            draw_context_menu(&mut surface, &ctx.menu, ctx.anchor, theme, font, viewport);
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
        launcher: &RefCell<Launcher>,
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

        // The right-click context menu owns input while it is open (it opens
        // only in navigation mode, so no other overlay is up); it needs the
        // launcher for a context-menu Open, so it is handled here rather than
        // in the launcher-less modal router.
        if overlays.menu.is_some() {
            return apply_menu_event(browser, overlays, launcher, font, theme, viewport, event);
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
                // Alt+Enter opens the Properties overlay, a plain Enter
                // activates the selection (needs the launcher for a bundle
                // launch), Delete opens the delete confirmation, and
                // Ctrl+X/C/V drive the clipboard verbs (all need the
                // overlay/clipboard/launcher state); every other navigation-
                // mode key is handled by the shared `apply_nav_key`.
                if matches!(key, KeyValue::Named(NamedKeyCode::Enter)) && modifiers.alt {
                    begin_properties(browser, &mut overlays.properties)
                } else if matches!(key, KeyValue::Named(NamedKeyCode::Enter)) {
                    activate(browser, launcher, font, theme, viewport)
                } else if matches!(key, KeyValue::Named(NamedKeyCode::Delete)) {
                    begin_delete(browser, &mut overlays.delete)
                } else if let Some(verb) = clipboard_verb(*key, *modifiers) {
                    apply_clipboard_verb(browser, &mut overlays.clipboard, verb)
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
                // A secondary-button (right-click) press opens the context
                // menu on the item under the pointer; its commands are the
                // user's own verbs, so it needs the overlay/launcher state.
                if let Some(point) = secondary_press_point(*action, *x, *y) {
                    return open_context_menu(browser, overlays, font, theme, viewport, point);
                }
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
        // The delete-confirmation dialog is the topmost modal: while it is up
        // it owns the window, so it is handled before anything else.
        if overlays.delete.is_some() {
            return Some(apply_delete_event(
                browser, overlays, font, theme, viewport, event,
            ));
        }
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
    /// (Properties) and a plain Enter (activation, which needs the launcher)
    /// are handled by the caller, which owns the overlay and launcher state.
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
            KeyValue::Named(NamedKeyCode::Backspace) => (browser.go_up().unwrap_or(false), false),
            // F2 begins an in-place rename of the selected item; with nothing
            // selected (an empty directory) it is a no-op.
            KeyValue::Named(NamedKeyCode::F2) => {
                begin_rename(browser, rename, font, theme, viewport)
            }
            _ => (false, false),
        }
    }

    /// Activate the selected entry — the one dispatch-by-kind decision `Enter`
    /// drives, over the shared [`Browser::activate_selected`] so the file
    /// manager and the trusted picker act on the same [`Activation`]. The
    /// engine decides *what* the target is; the launch stays here, in the app's
    /// own capability-checked tail under the user's identity.
    ///
    /// * [`Activation::Descended`] — the engine descended into a directory (its
    ///   own transactional, fail-closed navigation); the selection is revealed
    ///   and the view repainted, exactly as a breadcrumb-click navigation is.
    /// * [`Activation::LaunchBundle`] — the entry is a `<Name>.app` bundle,
    ///   launched through the ordinary signed app-load gate ([`Launcher`]),
    ///   asynchronously so the event loop never blocks behind the load.
    ///   Launching changes nothing on screen, so nothing repaints.
    /// * [`Activation::OpenFile`] — opening a data file in its associated
    ///   viewer is a later stage (the file hand-off / "Open With…"); until it
    ///   is wired, activating a file leaves the listing exactly as it was,
    ///   never a fabricated action.
    ///
    /// A refused activation (an unreadable directory the engine could not
    /// descend into) leaves the browser where it was and repaints nothing
    /// (fail closed).
    fn activate<S: DirectorySource>(
        browser: &mut Browser<S>,
        launcher: &RefCell<Launcher>,
        font: BitmapFont,
        theme: &Theme,
        viewport: Rect,
    ) -> (bool, bool) {
        match browser.activate_selected() {
            Ok(Activation::Descended) => {
                tairix_browse::render::reveal_selection(browser, font, theme, viewport);
                (true, false)
            }
            Ok(Activation::LaunchBundle { path }) => {
                launcher.borrow_mut().launch(&path);
                (false, false)
            }
            Ok(Activation::OpenFile { .. }) | Err(_) => (false, false),
        }
    }

    /// Open the delete-confirmation dialog for the current selection, reporting
    /// a repaint. With nothing selected (an empty directory, or a cleared
    /// selection) [`Browser::plan_delete`] yields no plan and this is a no-op —
    /// the Delete verb is simply unavailable rather than a catastrophic empty
    /// or root removal (fail closed, §5.4). The plan is captured now, so a
    /// listing change while the dialog is up cannot move what a confirmed
    /// delete removes.
    fn begin_delete<S: DirectorySource>(
        browser: &Browser<S>,
        delete: &mut Option<DeleteConfirm>,
    ) -> (bool, bool) {
        let Some(plan) = browser.plan_delete() else {
            return (false, false);
        };
        let dialog = build_delete_dialog(&plan);
        *delete = Some(DeleteConfirm { dialog, plan });
        (true, false)
    }

    /// Handle one event while the delete-confirmation dialog owns the window.
    ///
    /// `Escape` (or a click on Cancel) dismisses the dialog and changes
    /// nothing; `Enter` (or a click on Delete) carries out the captured plan
    /// under the user's own identity and re-lists. A click that lands on
    /// neither button, and every non-decision event, leaves the dialog open
    /// (fail closed). The removal is the user's own capability-checked
    /// `fs_unlink`s — no new capability, no ambient authority (§4, §5.4).
    fn apply_delete_event<S: DirectorySource>(
        browser: &mut Browser<S>,
        overlays: &mut Overlays,
        font: BitmapFont,
        theme: &Theme,
        viewport: Rect,
        event: &WindowEvent,
    ) -> (bool, bool) {
        // Resolve the decision: `Some(true)` confirms, `Some(false)` cancels,
        // `None` leaves the dialog open.
        let decision = match event {
            WindowEvent::Key {
                key: KeyInput::Pressed { key, .. },
                ..
            } => match key {
                KeyValue::Named(NamedKeyCode::Escape) => Some(false),
                KeyValue::Named(NamedKeyCode::Enter) => Some(true),
                _ => None,
            },
            WindowEvent::Pointer { x, y, action, .. } => {
                press_point(*action, *x, *y).and_then(|point| {
                    let confirm = overlays.delete.as_ref()?;
                    let index =
                        delete_dialog_action_at(&confirm.dialog, viewport, font, theme, point);
                    if index == Some(DELETE_CONFIRM_INDEX) {
                        Some(true)
                    } else if index == Some(DELETE_CANCEL_INDEX) {
                        Some(false)
                    } else {
                        None
                    }
                })
            }
            _ => None,
        };
        match decision {
            None => (false, false),
            Some(false) => {
                overlays.delete = None;
                (true, false)
            }
            Some(true) => {
                let Some(confirm) = overlays.delete.take() else {
                    return (false, false);
                };
                run_delete(&confirm.plan);
                // Re-list so the view reflects what actually remains — a partial
                // removal left by a refusal is shown honestly (§2.24). A failed
                // re-list leaves the browser where it was (fail closed).
                let _ = browser.refresh();
                (true, false)
            }
        }
    }

    /// Carry out `plan` by driving a [`DeleteWalk`] to completion: read each
    /// directory (`fs_readdir`) and unlink each node (`fs_unlink`,
    /// depth-first, so contents go before their container) under the user's
    /// own identity.
    ///
    /// It is bounded and fail closed: the walk caps its recursion depth, and
    /// the first refused read or unlink stops the removal, states the reason on
    /// `stderr` (fail loud, §2.24), and returns — leaving the walk's own
    /// position untouched and whatever was already removed removed, never a
    /// fabricated success (§5.4). No new capability is involved: every syscall
    /// is the ordinary §5.3-checked write the user could perform themselves.
    fn run_delete(plan: &DeletePlan) {
        let mut walk = DeleteWalk::from_plan(plan);
        loop {
            // Copy the current step out so the walk is free to be mutated.
            let step = walk.next_action().map(|action| match action {
                DeleteAction::List(path) => (true, path.to_vec(), false),
                DeleteAction::Remove { path, is_directory } => (false, path.to_vec(), is_directory),
            });
            let Some((is_list, path, is_directory)) = step else {
                return;
            };
            if is_list {
                let Ok(children) = read_children(&path) else {
                    report_delete_refused(&path);
                    return;
                };
                if walk.expand(&children).is_err() {
                    report_error("delete stopped: a folder is nested too deep");
                    return;
                }
            } else {
                let Ok(spelled) = tairix_browse::vfs::absolute_path(&path) else {
                    report_error("delete stopped: a path could not be spelled");
                    return;
                };
                let flags = if is_directory {
                    UnlinkFlags::DIRECTORY
                } else {
                    UnlinkFlags::empty()
                };
                if tairix_rt::fs_unlink(spelled.as_bytes(), flags) != 0 {
                    report_delete_refused(&path);
                    return;
                }
                if walk.complete_removal().is_err() {
                    report_error("delete stopped: internal walk error");
                    return;
                }
            }
        }
    }

    /// Read the children of the directory at `path` for a [`DeleteWalk`]
    /// expansion: each child's leaf name and whether it is directory-backed,
    /// through the same capability-checked listing call and shared decode the
    /// browser navigates with, so the delete sees exactly what the browser
    /// would.
    fn read_children(path: &[String]) -> Result<alloc::vec::Vec<(String, bool)>, Errno> {
        let spelled = tairix_browse::vfs::absolute_path(path)?;
        let stream = tairix_rt::read_dir_all(spelled.as_bytes()).map_err(errno_from)?;
        let entries = tairix_browse::vfs::entries_from_dir_stream(&stream)?;
        Ok(entries
            .into_iter()
            .map(|entry| (entry.name().to_string(), entry.is_directory_backed()))
            .collect())
    }

    /// State on `stderr` that the item at `path` could not be removed — an
    /// honest, fail-loud diagnosis naming the item, never a silent failure or
    /// a fabricated success (§2.24). Carries no path prefix or token beyond the
    /// leaf name the user already sees.
    fn report_delete_refused(path: &[String]) {
        let name = path.last().map_or("", String::as_str);
        let _ = tairix_rt::stderr(b"files: could not delete ");
        let _ = tairix_rt::stderr(name.as_bytes());
        let _ = tairix_rt::stderr(b"\n");
    }

    /// State a `files:`-prefixed diagnosis on `stderr` — the one fail-loud
    /// reporting path a whole-operation refusal (a too-deep tree, an
    /// unspellable path, a rejected paste plan, an internal step error) states
    /// its reason through, shared by the delete and paste drives (§2.2).
    fn report_error(reason: &str) {
        let _ = tairix_rt::stderr(b"files: ");
        let _ = tairix_rt::stderr(reason.as_bytes());
        let _ = tairix_rt::stderr(b"\n");
    }

    /// One clipboard verb the keyboard invokes in navigation mode.
    #[derive(Copy, Clone, Debug, Eq, PartialEq)]
    enum ClipboardVerb {
        /// `Ctrl+X`: capture the selection onto a move clipboard.
        Cut,
        /// `Ctrl+C`: capture the selection onto a copy clipboard.
        Copy,
        /// `Ctrl+V`: paste the held clipboard into the current directory.
        Paste,
    }

    /// Classify a navigation-mode key press as a clipboard verb, or `None`.
    ///
    /// `Ctrl+X`/`Ctrl+C`/`Ctrl+V` are the verbs; `Shift` must not be held (so
    /// `Ctrl+Shift+N`'s New Folder is never shadowed). The keyboard delivers
    /// the letter itself even with `Ctrl` held (the `Ctrl+Shift+N` precedent),
    /// and either case is accepted.
    fn clipboard_verb(key: KeyValue, modifiers: AbiModifiers) -> Option<ClipboardVerb> {
        let KeyValue::Char(ch) = key else {
            return None;
        };
        if !modifiers.ctrl || modifiers.shift || modifiers.alt {
            return None;
        }
        if ch.eq_ignore_ascii_case(&'x') {
            Some(ClipboardVerb::Cut)
        } else if ch.eq_ignore_ascii_case(&'c') {
            Some(ClipboardVerb::Copy)
        } else if ch.eq_ignore_ascii_case(&'v') {
            Some(ClipboardVerb::Paste)
        } else {
            None
        }
    }

    /// Apply a clipboard `verb`.
    ///
    /// `Cut`/`Copy` capture the current selection onto `clipboard` (a no-op
    /// with nothing selected — the verb is simply unavailable, fail closed);
    /// neither changes the visible view, so both repaint nothing. `Paste`
    /// carries the held clipboard into the current directory (`run_paste`),
    /// which re-lists and repaints.
    fn apply_clipboard_verb<S: DirectorySource>(
        browser: &mut Browser<S>,
        clipboard: &mut Option<Clipboard>,
        verb: ClipboardVerb,
    ) -> (bool, bool) {
        match verb {
            ClipboardVerb::Cut => {
                if let Some(clip) = browser.clipboard(ClipboardOp::Cut) {
                    *clipboard = Some(clip);
                }
                (false, false)
            }
            ClipboardVerb::Copy => {
                if let Some(clip) = browser.clipboard(ClipboardOp::Copy) {
                    *clipboard = Some(clip);
                }
                (false, false)
            }
            ClipboardVerb::Paste => run_paste(browser, clipboard),
        }
    }

    /// Carry out a paste of the held `clipboard` into the current directory,
    /// under the user's own identity (no new capability, no ambient authority
    /// — every operation is the ordinary §5.3-checked write the user could
    /// perform themselves).
    ///
    /// The plan is validated first ([`plan_paste`]): pasting a folder into
    /// itself is refused outright (`WouldRecurse`) and nothing is touched. Each
    /// item's move-vs-copy is then decided by [`paste_strategy`] from the two
    /// nodes' volume ids — a same-volume move is one `fs_rename`, a cross-volume
    /// move is copy-then-delete, a copy always streams. It is bounded and fail
    /// closed: the first refused operation stops the paste, states the reason on
    /// `stderr` (fail loud, §2.24), and leaves whatever already landed in place
    /// rather than a fabricated success (§5.4). A completed `Cut` clears the
    /// clipboard (its sources have moved); a `Copy` keeps it for another paste.
    fn run_paste<S: DirectorySource>(
        browser: &mut Browser<S>,
        clipboard: &mut Option<Clipboard>,
    ) -> (bool, bool) {
        let Some(clip) = clipboard.as_ref() else {
            return (false, false);
        };
        let target = browser.components().to_vec();
        let plan = match plan_paste(clip, &target) {
            Ok(plan) => plan,
            Err(err) => {
                report_error(err.to_string().as_str());
                return (false, false);
            }
        };
        // The destination directory's volume decides same- vs cross-volume for
        // every item (a move within a volume is one rename).
        let Some(dest_stat) = stat_node(&target) else {
            report_error("paste stopped: the destination folder could not be read");
            return (false, false);
        };
        let dest_vol = VolumeId::new(dest_stat.id.volume);
        let op = plan.op();
        // One reused, fixed-size copy buffer (never a per-file allocation, and
        // never a buffer sized to a file's length): a copy of any size streams
        // through it a chunk at a time, so memory stays bounded (§2.23, §26.6).
        let mut buf = alloc::vec![0u8; FS_IO_MAX];
        for item in plan.items() {
            if let Err(reason) = run_paste_item(op, item, dest_vol, &mut buf) {
                report_paste_item_error(item.source(), reason);
                break;
            }
        }
        // A cut is consumed by the paste; a copy can be pasted again elsewhere.
        if op == ClipboardOp::Cut {
            *clipboard = None;
        }
        // Re-list so the view shows what actually landed — a partial paste left
        // by a refusal is shown honestly (§2.24); a failed re-list leaves the
        // browser where it was (fail closed).
        let _ = browser.refresh();
        (true, false)
    }

    /// Carry out one planned paste item, returning a terse reason on the first
    /// refusal (the paste stops; nothing after this item runs).
    ///
    /// An item whose destination equals its source ([`PasteItem::overwrites_source`])
    /// is a paste back into the item's own directory: a `Cut` is a no-op (the
    /// item is already where it would land) and a `Copy` is refused rather than
    /// silently duplicating a file onto itself (§2.24). Otherwise the source is
    /// stat'd for its kind and volume, [`paste_strategy`] chooses the mechanism,
    /// and a pre-existing destination of a *different* name is refused by the
    /// exclusive create in [`copy_file`] rather than clobbered — a deliberate
    /// v1 scope boundary (overwrite/merge confirmation is future work), not a
    /// silent overwrite.
    fn run_paste_item(
        op: ClipboardOp,
        item: &PasteItem,
        dest_vol: VolumeId,
        buf: &mut [u8],
    ) -> Result<(), &'static str> {
        if item.overwrites_source() {
            return match op {
                ClipboardOp::Cut => Ok(()),
                ClipboardOp::Copy => Err("an item cannot be copied onto itself"),
            };
        }
        let source = item.source();
        let dest = item.dest();
        let Some(stat) = stat_node(source) else {
            return Err("a source item could not be read");
        };
        let source_vol = VolumeId::new(stat.id.volume);
        let is_directory = matches!(stat.kind, FileKind::Directory);
        match paste_strategy(op, source_vol, dest_vol) {
            PasteStrategy::Rename => rename_item(source, dest),
            PasteStrategy::Copy => copy_tree(source, dest, is_directory, buf),
            PasteStrategy::CopyThenDelete => {
                copy_tree(source, dest, is_directory, buf)?;
                delete_source(source, is_directory)
            }
        }
    }

    /// Move a same-volume item with a single `fs_rename` from its source to its
    /// destination path, under the user's own identity.
    fn rename_item(source: &[String], dest: &[String]) -> Result<(), &'static str> {
        let from = spell_path(source)?;
        let to = spell_path(dest)?;
        if tairix_rt::fs_rename(from.as_bytes(), to.as_bytes()) != 0 {
            return Err("a source item could not be moved");
        }
        Ok(())
    }

    /// Copy a source tree to its destination: a single [`copy_file`] for a
    /// regular file, or a depth-first [`CopyWalk`] for a directory (or sealed
    /// `.app` bundle), under the user's own identity.
    fn copy_tree(
        source: &[String],
        dest: &[String],
        is_directory: bool,
        buf: &mut [u8],
    ) -> Result<(), &'static str> {
        if is_directory {
            copy_dir(source, dest, buf)
        } else {
            copy_file(source, dest, buf)
        }
    }

    /// Copy one regular file from `source` to `dest` by driving a [`CopyCursor`]
    /// in fixed `FS_IO_MAX`-sized chunks through the reused `buf`, so the copy
    /// stays bounded and interruptible (§2.23).
    ///
    /// The destination is created exclusively: a pre-existing file of that name
    /// is refused rather than clobbered (§2.24). A source that ends before its
    /// stat'd length, or grows past it, fails closed rather than looping or
    /// wrapping.
    fn copy_file(source: &[String], dest: &[String], buf: &mut [u8]) -> Result<(), &'static str> {
        let source_spelled = spell_path(source)?;
        let dest_spelled = spell_path(dest)?;
        let reader = tairix_rt::File::open(source_spelled.as_bytes(), OpenFlags::READ)
            .map_err(|_| "a source file could not be opened")?;
        let stat = reader
            .stat()
            .map_err(|_| "a source file could not be read")?;
        let create = OpenFlags::WRITE
            .union(OpenFlags::CREATE)
            .union(OpenFlags::EXCLUSIVE);
        let writer = tairix_rt::File::open(dest_spelled.as_bytes(), create)
            .map_err(|_| "a destination of that name already exists")?;
        let mut cursor = CopyCursor::new(stat.size);
        while let Some(chunk) = cursor.next_chunk() {
            let want = usize::try_from(chunk.len()).map_err(|_| "a copy chunk was too large")?;
            let read = reader
                .read_at(chunk.offset(), &mut buf[..want])
                .map_err(|_| "a source file could not be read")?;
            if read == 0 {
                return Err("a source file ended early");
            }
            let wrote = writer
                .write_at(chunk.offset(), &buf[..read])
                .map_err(|_| "a destination file could not be written")?;
            if wrote != read {
                return Err("a destination file could not be fully written");
            }
            let carried = u64::try_from(read).map_err(|_| "a copy transfer was too large")?;
            cursor
                .advance(carried)
                .map_err(|_| "a source file changed during the copy")?;
        }
        Ok(())
    }

    /// Copy a directory tree by driving a [`CopyWalk`] to completion: create
    /// each destination directory before its contents (`fs_mkdir`), read each
    /// source directory (`fs_readdir`, the same shared decode the browser and
    /// the delete walk use), and stream each leaf file — depth-first, bounded,
    /// under the user's own identity.
    fn copy_dir(source: &[String], dest: &[String], buf: &mut [u8]) -> Result<(), &'static str> {
        let Some(mut walk) =
            CopyWalk::from_items(alloc::vec![(source.to_vec(), dest.to_vec(), true)])
        else {
            return Err("nothing to copy");
        };
        loop {
            // Copy the current step out so the walk is free to be mutated.
            enum Step {
                MakeDir(alloc::vec::Vec<String>),
                List(alloc::vec::Vec<String>),
                CopyFile(alloc::vec::Vec<String>, alloc::vec::Vec<String>),
            }
            let step = match walk.next_action() {
                None => return Ok(()),
                Some(CopyAction::MakeDir { dest }) => Step::MakeDir(dest.to_vec()),
                Some(CopyAction::List { source }) => Step::List(source.to_vec()),
                Some(CopyAction::CopyFile { source, dest }) => {
                    Step::CopyFile(source.to_vec(), dest.to_vec())
                }
            };
            match step {
                Step::MakeDir(dest) => {
                    let spelled = spell_path(&dest)?;
                    if tairix_rt::fs_mkdir(spelled.as_bytes()) != 0 {
                        return Err("a destination folder could not be created");
                    }
                    walk.created().map_err(|_| "internal copy step error")?;
                }
                Step::List(source) => {
                    let children =
                        read_children(&source).map_err(|_| "a folder could not be read")?;
                    walk.expand(&children)
                        .map_err(|_| "a folder is nested too deep")?;
                }
                Step::CopyFile(source, dest) => {
                    copy_file(&source, &dest, buf)?;
                    walk.copied_file().map_err(|_| "internal copy step error")?;
                }
            }
        }
    }

    /// Remove a cross-volume move's source once its copy has fully succeeded,
    /// by driving the shared delete path (`run_delete`) over a one-item
    /// [`DeletePlan`] under the user's own identity. Any refusal is stated on
    /// `stderr` by `run_delete` itself, so a copied-but-not-removed source is
    /// reported, never silently left (§2.24).
    fn delete_source(source: &[String], is_directory: bool) -> Result<(), &'static str> {
        let Some(plan) = DeletePlan::new(alloc::vec![(source.to_vec(), is_directory)]) else {
            return Err("a moved source could not be removed");
        };
        run_delete(&plan);
        Ok(())
    }

    /// Read a node's structural metadata (kind + size + volume id) through a
    /// resolve-only handle — opened with neither read nor write — so a path of
    /// unknown kind (file or directory) can be stat'd without guessing a flag,
    /// under the user's own identity. `None` when the path cannot be spelled or
    /// the node cannot be stat'd (the caller reports, fail closed).
    fn stat_node(path: &[String]) -> Option<tairix_abi::fs::FileStat> {
        let spelled = tairix_browse::vfs::absolute_path(path).ok()?;
        tairix_rt::File::open(spelled.as_bytes(), OpenFlags::empty())
            .and_then(|file| file.stat())
            .ok()
    }

    /// Spell a root-first component path to its validated absolute string, the
    /// one shared spelling the browser navigates with, or a terse reason.
    fn spell_path(path: &[String]) -> Result<String, &'static str> {
        tairix_browse::vfs::absolute_path(path).map_err(|_| "a path could not be spelled")
    }

    /// State on `stderr` that the paste stopped while handling `source` — an
    /// honest, fail-loud diagnosis naming the item and the reason, never a
    /// silent failure or a fabricated success (§2.24). Carries no path prefix
    /// beyond the leaf name the user already sees.
    fn report_paste_item_error(source: &[String], reason: &str) {
        let name = source.last().map_or("", String::as_str);
        let _ = tairix_rt::stderr(b"files: could not paste ");
        let _ = tairix_rt::stderr(name.as_bytes());
        let _ = tairix_rt::stderr(b": ");
        let _ = tairix_rt::stderr(reason.as_bytes());
        let _ = tairix_rt::stderr(b"\n");
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

    /// The window-local [`Point`] of a **secondary**-button (right-click)
    /// press, or `None` for any other pointer action — the mirror of
    /// [`press_point`] for the button that opens the right-click context menu.
    fn secondary_press_point(action: PointerAction, x: u32, y: u32) -> Option<Point> {
        if action != PointerAction::Pressed(PointerButtonCode::Secondary) {
            return None;
        }
        Some(Point::new(
            i32::try_from(x).unwrap_or(i32::MAX),
            i32::try_from(y).unwrap_or(i32::MAX),
        ))
    }

    /// Open the right-click context menu at window-local `point`.
    ///
    /// The item under the pointer is selected first so the menu's commands act
    /// on what was clicked; a right-click on empty space (or the chrome) clears
    /// the selection so the menu offers only the directory-scoped Paste. The
    /// menu is built from the shared [`ContextMenuModel`] with the app's own
    /// held-clipboard state, so an inapplicable command renders disabled. The
    /// menu itself performs nothing — a chosen command runs the user's own
    /// permission-checked verb in [`dispatch_context_command`], no new
    /// authority.
    fn open_context_menu<S: DirectorySource>(
        browser: &mut Browser<S>,
        overlays: &mut Overlays,
        font: BitmapFont,
        theme: &Theme,
        viewport: Rect,
        point: Point,
    ) -> (bool, bool) {
        if let Some(index) =
            tairix_browse::render::entry_index_at(browser, font, theme, viewport, point)
        {
            let _ = browser.select(index);
        } else {
            browser.clear_selection();
        }
        let model = ContextMenuModel::for_browser(browser, overlays.clipboard.is_some());
        overlays.menu = Some(ContextMenu {
            anchor: point,
            menu: build_context_menu(model),
        });
        (true, false)
    }

    /// Handle one event while the right-click context menu owns the window.
    ///
    /// `Escape` dismisses it. A primary-button press on an enabled command runs
    /// that command's verb (and closes the menu); a press off the menu, or on
    /// a disabled row, simply dismisses it (fail closed — a disabled row never
    /// acts, §5.4). Every other event leaves the menu open.
    fn apply_menu_event<S: DirectorySource>(
        browser: &mut Browser<S>,
        overlays: &mut Overlays,
        launcher: &RefCell<Launcher>,
        font: BitmapFont,
        theme: &Theme,
        viewport: Rect,
        event: &WindowEvent,
    ) -> (bool, bool) {
        match event {
            WindowEvent::Key {
                key:
                    KeyInput::Pressed {
                        key: KeyValue::Named(NamedKeyCode::Escape),
                        ..
                    },
                ..
            } => {
                overlays.menu = None;
                (true, false)
            }
            WindowEvent::Pointer { x, y, action, .. } => {
                let Some(point) = press_point(*action, *x, *y) else {
                    return (false, false);
                };
                // Take the menu out (closing it) so the dispatch can borrow the
                // overlays mutably; a press off an enabled row is a plain
                // dismiss.
                let Some(ctx) = overlays.menu.take() else {
                    return (false, false);
                };
                match context_menu_command_at(&ctx.menu, ctx.anchor, viewport, font, theme, point) {
                    Some(command) => dispatch_context_command(
                        browser, overlays, launcher, font, theme, viewport, command,
                    ),
                    None => (true, false),
                }
            }
            _ => (false, false),
        }
    }

    /// Run the verb a chosen [`ContextCommand`] names, over the exact same app
    /// paths the toolbar and keyboard drive, so the right-click menu can never
    /// diverge from them (§2.2). Every verb is the user's own permission-
    /// checked action under their identity — the menu adds no authority.
    fn dispatch_context_command<S: DirectorySource>(
        browser: &mut Browser<S>,
        overlays: &mut Overlays,
        launcher: &RefCell<Launcher>,
        font: BitmapFont,
        theme: &Theme,
        viewport: Rect,
        command: ContextCommand,
    ) -> (bool, bool) {
        match command {
            ContextCommand::Open => activate(browser, launcher, font, theme, viewport),
            ContextCommand::Rename => {
                begin_rename(browser, &mut overlays.rename, font, theme, viewport)
            }
            ContextCommand::Cut => {
                apply_clipboard_verb(browser, &mut overlays.clipboard, ClipboardVerb::Cut)
            }
            ContextCommand::Copy => {
                apply_clipboard_verb(browser, &mut overlays.clipboard, ClipboardVerb::Copy)
            }
            ContextCommand::Paste => {
                apply_clipboard_verb(browser, &mut overlays.clipboard, ClipboardVerb::Paste)
            }
            ContextCommand::Properties => begin_properties(browser, &mut overlays.properties),
        }
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
        // The any-child member: a bundle the file manager launched exiting
        // wakes the park so it is reaped promptly (never left a zombie). Adding
        // it needs no capability — a process may always wait on its own
        // children — so a refusal here is a genuine bring-up failure.
        if tairix_rt::waitset_ctl(
            set,
            WaitSetOp::Add,
            WaitSourceKind::Child,
            WAITSET_CHILD_ANY,
            CHILD_TOKEN,
        ) != 0
        {
            return Err(fail(EXIT_NO_EVENTS, "child wait refused"));
        }
        Ok((event_endpoint, set))
    }

    /// The transient overlay/clipboard state the event loop threads, all
    /// closed at start-up.
    ///
    /// `can_chown` is whether the launching user holds `CAP_FS_CHOWN` — read
    /// once from the kernel-attested self-origin (a refused query fails closed
    /// to "not held") — so the ownership control is offered only where it can
    /// be used (§2.24).
    fn initial_overlays() -> Overlays {
        Overlays {
            rename: None,
            properties: None,
            owner: None,
            delete: None,
            menu: None,
            clipboard: None,
            can_chown: tairix_rt::self_origin()
                .is_ok_and(|origin| origin.capabilities().holds_cap(CapabilityId::FS_CHOWN)),
        }
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
        let mut overlays = initial_overlays();
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

        // --- The launched-bundle bookkeeping: shared between the event
        // source (which reaps an exited bundle on a child-exit wake) and the
        // activation path below (which spawns one), so a launch and its reap
        // agree on the same in-flight set.
        let launcher = RefCell::new(Launcher::new());

        // --- The event loop: park, apply, repaint. A dead channel ends
        // the app fail-loud; a clean close ends it at zero.
        let mut events = WindowEvents::new(RtEventSource {
            endpoint: event_endpoint,
            set,
            server,
            launcher: &launcher,
        });
        loop {
            let event = match events.wait() {
                Ok(event) => event,
                // A malformed frame from the authenticated session is
                // refused and the app keeps waiting (never guessed at).
                Err(Errno::OutOfRange | Errno::BadMagic | Errno::BufferTooSmall) => continue,
                Err(_) => return fail(EXIT_CHANNEL_LOST, "event channel lost"),
            };
            let (changed, close) =
                apply_event(&mut browser, &mut overlays, &launcher, theme, &mode, &event);
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
