//! The `files.app` bundle's `Run` entry point (`plans/APPWIN.md` AW3):
//! the windowed file browser, the first app served over the desktop
//! session's window channel.
//!
//! # One binary, two roles
//!
//! Launched plainly — from a shell, or by the desktop opening a folder — this
//! is an ordinary application: one window at the location it was given, and
//! the shared icon-bar menu convention on its slot. Closing every window
//! puts it away rather than ending it: the slot stays, a click there opens a
//! window at the user's home, and *Quit* is what ends the process.
//!
//! Launched by the desktop session with `tairix_window::DESKTOP_ROLE_SWITCH`
//! it is instead a **component of the desktop**
//! ([`Role::Desktop`](command::Role::Desktop)): it comes up with the session,
//! holds a permanent icon-bar slot offering the user's places and whatever is
//! mounted, opens a window per place the user chooses (bounded by the
//! session's per-client frame budget), and cannot be quit at all — its menu
//! carries neither an information row nor *Quit*. Only the session passes the
//! switch, so a second component can never appear.
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
//! * One `shm_create`d frame region **per window**, granted to the reserved
//!   window endpoint (the zero-copy surface the session maps once at create).
//! * One `port_bind`-bound event mailbox the app **parks** on through
//!   its wait-set — never a poll loop, and one mailbox for every window and
//!   the icon-bar slot alike. Every received event carries its sender's
//!   kernel-attested origin, and the app accepts only events from the session
//!   identity the (squat-protected) desktop-query reply named: no other
//!   process can feed it forged input (fail closed). Learning that identity
//!   from the *query* rather than from a create reply is what lets a
//!   component answer its slot before it owns any window.
//! * The `WindowClient` calls (create / present / close) over `ipc_call`
//!   and the `WindowEvents` typed wait over the parked source.
//! * The grid's icon artwork: the shared reclaim-governed decode cache
//!   (`tairix_icon::artwork`) bound to a bounded VFS read and to the
//!   sandboxed icon rasteriser. The artwork is untrusted input, so it is
//!   decoded by a capability-empty worker this binary re-enters itself as
//!   — never in this process — and a missing, over-long, or disbelieved
//!   asset falls back to the built-in glyph rather than a blank tile. The
//!   decode happens between turns of the event loop, never inside a paint
//!   ([`crate::icons`]), so a folder of picture-bearing bundles neither
//!   freezes the first frame nor stalls a scroll. The same wait-set carries
//!   a memory-pressure member, so the retained pixels are trimmed when the
//!   machine's band changes and released when the app ends.
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
//! that window, which ends an ordinary file manager cleanly once it was its
//! last. Every bring-up refusal exits fail-loud with a reserved code and a
//! stated reason on `stderr`.
//!
//! On the host it is an inert stub so `cargo build --workspace`, clippy,
//! and fmt still cover the file.

#![cfg_attr(freestanding, no_std)]
#![cfg_attr(freestanding, no_main)]
#![deny(missing_docs)]

extern crate alloc;

pub mod appbar;
pub mod chrome;
pub mod command;
pub mod deferred;
pub mod gesture;
pub mod icons;
pub mod listing;
pub mod location;
pub mod operation;
pub mod sidebar;

#[cfg(test)]
mod test_fs;

// --- Pure-Rust program --------------------------------------------------
#[cfg(freestanding)]
mod program {

    use alloc::collections::BTreeMap;
    use alloc::string::{String, ToString};
    use alloc::vec::Vec;
    use core::cell::RefCell;

    use tairix_abi::driver::display::{DamageRect, DisplayFormat, DisplayMode};
    use tairix_abi::fs::{FileKind, OpenFlags, FS_IO_MAX, FS_MODE_MASK, FS_NAME_MAX};
    use tairix_abi::input::{
        KeyInput, KeyValue, Modifiers as AbiModifiers, NamedKeyCode, PointerButtonCode,
    };
    use tairix_abi::seat::SEAT_PRIMARY;
    use tairix_abi::window_ipc::{
        MenuAnchor, MenuOutcome, PointerAction, WindowEvent, WINDOW_ENDPOINT,
    };
    use tairix_abi::{
        load_failure_reason, CapabilityId, Errno, FdWire, Origin, ProcId, SpawnAttach, UnlinkFlags,
        WaitFlags, WaitSetOp, WaitSourceKind, WaitStatus, BUNDLE_SUFFIX, DOCUMENT_ROLE_ARG,
        INSTALLED_APP_STORE, ORIGIN_WIRE_LEN, STDIN, STD_STREAM_COUNT, SYSTEM_APPLICATION_STORE,
        SYSTEM_COMMAND_STORE, WAITSET_CHILD_ANY, WAIT_PID_ANY,
    };
    use tairix_browse::render::{
        build_delete_dialog, delete_dialog_action_at, draw_delete_dialog, draw_open_with_chooser,
        draw_owner_control, draw_progress_dialog, draw_properties_editable, manager_tool_at,
        open_with_row_at, open_with_scroll_pointer, open_with_visible_rows, owner_editor_rect,
        owner_field_at, permission_cell_at, render_into, scroll_pointer, OwnerField,
        DELETE_CANCEL_INDEX, DELETE_CONFIRM_INDEX,
    };
    use tairix_browse::{
        applications_for, association_from_appinfo, context_command_from_item, context_menu,
        empty_trash_plan, paste_strategy, plan_paste, suggest_new_dir_name, trash_dest_path,
        trash_dir, trash_strategy, validate_new_name, Activation, AppAssociation, Browser,
        BundleIntent, BundleSource, Clipboard, ClipboardOp, ContextCommand, ContextMenuModel,
        CopyAction, CopyCursor, CopyKind, CopyWalk, DeleteAction, DeleteDisposition, DeletePlan,
        DeleteWalk, DirectorySource, DoubleClickTracker, Entry, EntryKind, Listing, ListingDesk,
        ManagerChrome, ManagerTool, ManagerToolModel, OpenWithChooser, OwnerChange, PasteItem,
        PasteStrategy, Places, Probe, ProgressModel, ProgressOp, Properties, RenameError,
        RtLinkReader, ToolbarBand, ToolbarCommand, TrashStrategy, VfsDirectorySource, ViewMode,
        Volume, VolumeId, MANAGER_TOOLS, WIN_HEIGHT, WIN_SIZING, WIN_WIDTH,
    };
    use tairix_controls::damage;
    use tairix_controls::decision::Dialog;
    use tairix_controls::text::{TextAction, TextField};
    use tairix_display::{winframe, SERIAL};
    use tairix_geometry::{Point, Rect, Region, Scale};
    use tairix_help::{own_short_help, BundleHelp};
    use tairix_icon::{artwork_cache, ArtworkRasteriser, ArtworkReader, MAX_ARTWORK_BYTES};
    use tairix_input::{Key, Modifiers, NamedKey};
    use tairix_procinfo::{IpcTransport, WalkStep};
    use tairix_raster::Surface;
    use tairix_rt::io::{self, Stderr, Stdout, Write};
    use tairix_sandbox::imagerender::{rasterise_icon, ImageRenderService};
    use tairix_sandbox::rt::{serve_stdio, worker_role, RtLauncher};
    use tairix_sandbox::{ParserSandbox, ServeEnd};
    use tairix_theme::{Theme, ThemeRegistry};
    use tairix_window::{
        pointer_input_events, pointer_point, present_damage, Desktop, EventSource, Parked, Repaint,
        WindowClient, WindowEvents, WindowFrames, WindowTransport,
    };

    use crate::appbar;
    use crate::chrome::Chrome;
    use crate::command::{self, unlistable_reason, Command, Role, UsageError, USAGE};
    use crate::deferred::{FilesClient, Probes};
    use crate::gesture::{self, bundle_intent, AfterHandoff, PrimaryPress};
    use crate::listing::{self, ViewMark};
    use crate::location::{leave_directory, location_title, retitle, Leave};
    use crate::operation::{operation_control, OperationControl};
    use crate::sidebar::{self, press_point};

    /// The application's own name, as its context menu's plate is titled.
    ///
    /// A per-window menu's title is the application's, bounded and sanitised
    /// exactly as a row label is: a name, not a credential.
    const APP_NAME: &str = "Files";

    /// Exit code when the initial directory listing was refused (no
    /// filesystem reach, or a corrupt stream). A reserved, fail-closed
    /// value: the browser never shows a fabricated listing.
    const EXIT_NO_LISTING: i32 = 80;

    /// Exit code when the memory a window's pixels need could not be had —
    /// the shared frame region could not be created or granted to the window
    /// endpoint, or the surface each frame is drawn in could not be
    /// allocated. A reserved, fail-closed value.
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

    /// Exit code for a command line the program cannot act on: an unrecognised
    /// option, a second operand, or an argument vector that is not UTF-8. The
    /// conventional usage status the other command apps return, so a script
    /// sees the familiar value rather than one reserved to this app.
    const EXIT_USAGE: i32 = 2;

    /// Frames in the shared region. The window protocol serialises a
    /// present (the app is parked in the call while the session reads),
    /// so a single frame is race-free; the constant names the choice.
    const FRAME_COUNT: u32 = 1;

    /// The wait-set token of the event-mailbox member.
    const EVENT_TOKEN: u64 = 1;

    /// The wait-set token of the any-child member: a bundle the file manager
    /// launched has exited, so it is reaped promptly (never left a zombie,
    /// and never a busy-poll — the member is drained the instant it wakes).
    const CHILD_TOKEN: u64 = 2;

    /// The wait-set token of the memory-pressure member: the kernel wakes the
    /// park when the machine's pressure band changes, so the decoded grid
    /// artwork is handed back as memory tightens instead of being held until
    /// something else is starved. The wake is the notification — nothing here
    /// polls or times the band.
    const PRESSURE_TOKEN: u64 = 3;

    /// The wait-set token of the reader's wake pipe: readable exactly when a
    /// directory listing, a batch of folder cues, or a bundle scan has come
    /// back, so the answer is adopted through the park the loop is already in
    /// rather than by polling for it.
    const READS_TOKEN: u64 = 4;

    /// The maximum digit count the owner/group id editor accepts — a `u32` id
    /// is at most ten decimal digits, so a longer entry cannot be a valid id.
    const OWNER_ID_MAX_DIGITS: usize = 10;

    /// The in-field hint shown when a typed owner/group id is not a
    /// well-formed, assignable `u32` (non-numeric, empty, out of range, or the
    /// reserved "unchanged" sentinel).
    const OWNER_ID_HINT: &str = "Enter a valid numeric id.";

    /// The RGBA8888 window surface `width_px` × `height_px`, its stride the
    /// tightly-packed four-bytes-per-pixel row. One definition so the initial
    /// window and every resize build the surface identically.
    fn mode_for(width_px: u32, height_px: u32) -> DisplayMode {
        DisplayMode {
            width_px,
            height_px,
            stride_bytes: width_px.saturating_mul(4),
            format: DisplayFormat::Rgba8888,
        }
    }

    /// Re-map the window `window` onto a fresh frame region shaped as
    /// `new_mode`, fail-closed. Returns the adopted region's `(base, len)` on
    /// success — the old region (`old_base` / `old_len`) already unmapped — or
    /// `None` when the region could not be allocated or the session refused the
    /// re-map, in which case the old region is left intact and still mapped so
    /// the current surface stays valid (never a crash or a blank window).
    ///
    /// The ordering is fail-closed by ownership: the fresh region is created
    /// and granted first and returned only once [`WindowClient::resize`] has
    /// accepted it, so the caller's old region is dropped — and unmapped — by
    /// adopting the new one, while every refusal drops the fresh region here
    /// instead and leaves the window on the geometry it had.
    fn resize_frames(
        client: &mut WindowClient<RtWindowTransport>,
        window: u64,
        new_mode: &DisplayMode,
    ) -> Option<(WindowFrames, Surface)> {
        let new_len = (new_mode.stride_bytes as usize)
            .checked_mul(new_mode.height_px as usize)?
            .checked_mul(FRAME_COUNT as usize)?;
        let frames = WindowFrames::create(new_len)?;
        let surface = Surface::new(new_mode.width_px, new_mode.height_px)?;
        client
            .resize(window, frames.grant()?, FRAME_COUNT, new_mode)
            .ok()?;
        Some((frames, surface))
    }

    /// What a router that only answers "did anything change" concludes: it
    /// moved pixels it cannot name, so the window is drawn whole.
    ///
    /// Every path that replaces the listing, opens or closes an overlay, or
    /// re-derives the model is one of these — correct, and cheap enough at one
    /// window's size that describing it would buy nothing.
    const fn whole_if(changed: bool) -> Repaint {
        if changed {
            Repaint::Whole
        } else {
            Repaint::Nothing
        }
    }

    /// What a round that reported its own rectangles concludes.
    const fn reported_if(changed: bool) -> Repaint {
        if changed {
            Repaint::Reported
        } else {
            Repaint::Nothing
        }
    }

    /// The stronger of two conclusions about one round, so a round that both
    /// reported a rectangle and moved something no report describes still
    /// covers the window.
    const fn merge(a: Repaint, b: Repaint) -> Repaint {
        match (a, b) {
            (Repaint::Whole, _) | (_, Repaint::Whole) => Repaint::Whole,
            (Repaint::Reported, _) | (_, Repaint::Reported) => Repaint::Reported,
            (Repaint::Nothing, Repaint::Nothing) => Repaint::Nothing,
        }
    }

    /// State the abnormal-exit reason on `stderr` (fail loud: an exit
    /// code alone is not a diagnosis) and hand back `code` for `main`.
    fn fail(code: i32, reason: &str) -> i32 {
        let _ = writeln!(Stderr, "files: {reason}");
        code
    }

    /// Declare (or re-declare) this process's presence on the desktop's icon
    /// bar, as its `role` decides.
    ///
    /// An ordinary file manager makes the shared declaration — the
    /// session-drawn information row and *Quit*, with the session raising a
    /// window it has and asking for one when it has none. A component offers
    /// its places instead and neither of those rows ([`appbar`]).
    ///
    /// A refused declaration is an answer, not a death: the application says
    /// so and carries on with no slot of its own — a window it owns is still
    /// reachable through the slot the session derives from it. A component so
    /// refused has no slot and no window, which is a desktop without its file
    /// manager: stated loudly, and still not fatal.
    fn declare_app_bar(
        client: &mut WindowClient<RtWindowTransport>,
        endpoint: u64,
        role: Role,
        places: &Places,
    ) {
        let declared = match role {
            Role::Window => tairix_window::info_and_quit(endpoint, appbar::WINDOW_SLOT_CLICK)
                .map(|bar| (bar, 0)),
            Role::Desktop => appbar::component_declaration(endpoint, places),
        };
        match declared {
            Ok((bar, skipped)) => {
                if skipped > 0 {
                    report_error(&alloc::format!(
                        "{skipped} place(s) do not fit the icon-bar menu and are not shown"
                    ));
                }
                if let Err(err) = client.set_app_bar(&bar) {
                    report_error(&alloc::format!(
                        "the desktop refused this application's icon-bar presence ({err}); \
                         carrying on without one"
                    ));
                }
            }
            Err(err) => report_error(&alloc::format!(
                "this application's icon-bar menu is invalid ({err:?}); carrying on without one"
            )),
        }
    }

    /// The production [`WindowTransport`]: one synchronous `ipc_call` to
    /// the reserved window endpoint per request. The session attests the
    /// caller kernel-side on every request, so the transport carries no
    /// claimed authority.
    struct RtWindowTransport;

    impl WindowTransport for RtWindowTransport {
        fn call(&mut self, request: &[u8], reply: &mut [u8]) -> Result<usize, Errno> {
            tairix_rt::ipc_call(WINDOW_ENDPOINT, request, reply).map_err(Errno::from_syscall)
        }
    }

    /// The window surface one present writes into, threaded through the
    /// present path as one value: the channel half and the window the frame is
    /// presented over, the mapped frame bytes, the pixel layout those bytes are
    /// shaped as, and the title the session was last told. Bundling them keeps
    /// a frame inseparable from the mode that describes it and the window it
    /// belongs to.
    struct FrameTarget<'a, T: WindowTransport> {
        /// The app half of the window channel the present goes out over.
        client: &'a mut WindowClient<T>,
        /// The window the frame belongs to — the one the present names.
        window: u64,
        /// The mapped shared-memory bytes of the frame being painted.
        frame: &'a mut [u8],
        /// The pixel layout `frame` is shaped as.
        mode: &'a DisplayMode,
        /// The title the window currently carries, owned by the run so it
        /// outlives one frame: the location is only sent again when it moves.
        title: &'a mut String,
        /// The window-lifetime surface the frame is drawn in and copied from.
        surface: &'a mut Surface,
        /// The rectangle this frame redraws and presents.
        damage: DamageRect,
    }

    /// One open browser window: everything a frame is drawn from, and nothing
    /// shared with another window.
    ///
    /// The process holds a list of these rather than one window's worth of
    /// locals, because a component's slot opens a window per place the user
    /// chooses and an empty list is a perfectly good state for it to be in
    /// (see [`Role`](command::Role)). What *is* shared sits outside: the
    /// artwork cache (one decode serves every window), the launched-bundle
    /// bookkeeping, and the desktop's own theme and density.
    struct OpenWindow {
        /// The session's id for this window.
        window: u64,
        /// The listing this window shows.
        browser: Browser<DeferredSource>,
        /// The overlays open over it.
        overlays: Overlays,
        /// This window's own copy of the places rail: the same shortcuts and
        /// volumes, but its own focus, cursor, and hover — one window's
        /// pointer must not highlight a row in another.
        places: Places,
        /// Which of its own chrome bands this window is showing. Per window
        /// like the rail above, so one window's chrome is not another's.
        chrome: Chrome,
        /// The pixel layout its frame region is shaped as.
        mode: DisplayMode,
        /// The live frame region this window presents from, released when the
        /// session releases its side and re-attached by the next paint.
        frames: WindowFrames,
        /// The title the session was last told. Kept so a frame retitles only
        /// when the location actually moves.
        title: String,
        /// The window-sized surface every frame is drawn into, held for the
        /// life of the window: allocating and zeroing one per present would be
        /// a whole-window pass of its own, and holding it is what makes a
        /// clipped repaint sound — every pixel outside the clip is the one
        /// already on screen.
        surface: Surface,
        /// The open id of this window's unanswered context menu, if one is up.
        ///
        /// The desktop mints one per gesture and never reuses it, so an answer
        /// that names anything else belongs to a gesture already settled and is
        /// not acted on.
        menu: Option<u64>,
    }

    /// Paint `win`'s current state and present it.
    ///
    /// The one place the window's mapped frame region becomes a slice, so the
    /// `unsafe` that reconstructs it is written once rather than at every
    /// present in the loop.
    ///
    /// # Errors
    ///
    /// Whatever the present refuses; the caller treats a refused present as a
    /// lost channel and exits fail-loud.
    fn present_window(
        win: &mut OpenWindow,
        client: &mut WindowClient<RtWindowTransport>,
        theme: &Theme,
        icons: &RefCell<Icons>,
        scale: Scale,
        repaint: Repaint,
        damage: &Region,
    ) -> Result<(), Errno> {
        if repaint == Repaint::Nothing {
            // Nothing moved, so a released window stays released rather than
            // being re-attached to redraw pixels nobody can see.
            return Ok(());
        }
        // A region the session released holds none of the pixels a partial
        // present would leave standing, so it is re-attached and drawn whole.
        let repaint = if win.frames.is_released() {
            Repaint::Whole
        } else {
            repaint
        };
        let Some(damage) = present_damage(&win.mode, repaint, damage) else {
            return Ok(());
        };
        // Re-attached first if the session released it while the window was
        // hidden, so a paint after a release paints into a live region.
        let frame = client
            .frame_pixels(&mut win.frames, win.window, FRAME_COUNT, &win.mode)
            .ok_or(Errno::NotAttached)?;
        present_frame(
            &mut win.browser,
            &win.overlays,
            &win.places,
            win.chrome,
            theme,
            &mut FrameTarget {
                client,
                window: win.window,
                frame,
                mode: &win.mode,
                title: &mut win.title,
                surface: &mut win.surface,
                damage,
            },
            icons,
            scale,
        )
    }

    /// Repaint and present the whole of `win`.
    ///
    /// The first frame, a resize onto a fresh surface, a re-theme, and a model
    /// refresh a round could not describe all cover the window, so they name
    /// the one conclusion here rather than each spelling it out.
    ///
    /// # Errors
    ///
    /// Whatever the present refuses.
    fn present_whole(
        win: &mut OpenWindow,
        client: &mut WindowClient<RtWindowTransport>,
        theme: &Theme,
        icons: &RefCell<Icons>,
        scale: Scale,
    ) -> Result<(), Errno> {
        present_window(
            win,
            client,
            theme,
            icons,
            scale,
            Repaint::Whole,
            &Region::new(),
        )
    }

    /// Open one browser window at `location`, mapping its own frame region and
    /// telling the session about it.
    ///
    /// The reason for a refusal is stated here, on the one fail-loud path, and
    /// the exit code it *would* warrant is handed back rather than taken: an
    /// ordinary file manager that cannot open its first window has nothing to
    /// be and exits with it, while a component simply has one fewer window and
    /// carries on. The codes stay distinct — a refused listing, a refused
    /// frame region, and a refused window are three different diagnoses and a
    /// supervisor reads them apart.
    ///
    /// `places` is the process's rail; the window takes its own copy so its
    /// focus and hover are its own.
    ///
    /// # Errors
    ///
    /// The exit code naming what refused.
    fn open_window(
        client: &mut WindowClient<RtWindowTransport>,
        event_endpoint: u64,
        desktop: &Desktop,
        places: &Places,
        reads: &alloc::sync::Arc<Reads>,
        location: Option<alloc::vec::Vec<String>>,
    ) -> Result<OpenWindow, i32> {
        let Some(browser) = open_browser(reads, location) else {
            report_error("root directory listing refused; no window opened");
            return Err(EXIT_NO_LISTING);
        };
        let (w, h) = desktop.window_size(WIN_WIDTH, WIN_HEIGHT);
        let mode = mode_for(w, h);
        let total = (mode.stride_bytes as usize) * (mode.height_px as usize) * FRAME_COUNT as usize;
        let Some(frames) = WindowFrames::create(total) else {
            report_error("shared frame region refused; no window opened");
            return Err(EXIT_NO_FRAMES);
        };
        let Some(grant) = frames.grant() else {
            report_error("frame region grant refused; no window opened");
            return Err(EXIT_NO_FRAMES);
        };
        let Some(surface) = Surface::new(mode.width_px, mode.height_px) else {
            report_error("window surface refused; no window opened");
            return Err(EXIT_NO_FRAMES);
        };
        // The window opens carrying the location it shows, rather than a name
        // the first frame would have to replace.
        let title = location_title(&browser);
        let Ok((window, _)) = client.create(
            grant,
            event_endpoint,
            FRAME_COUNT,
            &mode,
            &title,
            WIN_SIZING,
        ) else {
            report_error("the desktop session refused the window");
            return Err(EXIT_NO_WINDOW);
        };
        Ok(OpenWindow {
            window,
            browser,
            overlays: initial_overlays(),
            places: places.clone(),
            chrome: Chrome::HIDDEN,
            mode,
            frames,
            title,
            surface,
            menu: None,
        })
    }

    /// Close the window at `index`, which never ends the process.
    ///
    /// Neither role *is* its windows: both keep an icon-bar slot with none
    /// open, so closing every window is the user putting the file manager
    /// away rather than quitting it. An ordinary one is ended by the *Quit*
    /// row its slot carries and a component by nothing at all — the
    /// desktop's own parts are not the user's to close.
    fn close_window(
        windows: &mut alloc::vec::Vec<OpenWindow>,
        index: usize,
        client: &mut WindowClient<RtWindowTransport>,
    ) {
        if index >= windows.len() {
            return;
        }
        let closed = windows.remove(index);
        let _ = client.close(closed.window);
    }

    /// What routing an icon-bar event did.
    enum BarRouted {
        /// Not the icon bar's event at all; the caller resolves it against a
        /// window instead.
        NotMine,
        /// Handled; the process carries on.
        Handled,
        /// Handled, and the process owes this exit code.
        Ends(i32),
    }

    /// Route one icon-bar event — an event that names the *application* rather
    /// than a window.
    ///
    /// The slot outlives every window, and a component's slot is often all
    /// there is, so these are resolved before anything asks which window an
    /// event belongs to.
    #[allow(clippy::too_many_arguments)] // The run's whole mutable state, threaded explicitly.
    fn route_app_bar_event(
        windows: &mut alloc::vec::Vec<OpenWindow>,
        client: &mut WindowClient<RtWindowTransport>,
        desktop: &Desktop,
        places: &Places,
        theme: &Theme,
        icons: &RefCell<Icons>,
        reads: &alloc::sync::Arc<Reads>,
        event_endpoint: u64,
        role: Role,
        event: &WindowEvent,
    ) -> BarRouted {
        match *event {
            WindowEvent::AppBarDefault => {
                // A window at the user's home — the readiest thing a file
                // manager can offer. A component takes every click; an
                // ordinary one is told only when it has no window left, and
                // this is the way back to one.
                open_more(
                    windows,
                    client,
                    desktop,
                    places,
                    theme,
                    icons,
                    reads,
                    event_endpoint,
                    None,
                );
                BarRouted::Handled
            }
            // A component's rows are its places: open a window at the one
            // chosen. A stale row — the rail was re-read since the menu was
            // declared — names nothing and does nothing, rather than opening
            // somewhere the user did not point at.
            WindowEvent::AppBarMenu { item } if role == Role::Desktop => {
                let location = appbar::place_of(item)
                    .and_then(|index| places.rows().get(index))
                    .map(|place| place.components().to_vec());
                if location.is_some() {
                    open_more(
                        windows,
                        client,
                        desktop,
                        places,
                        theme,
                        icons,
                        reads,
                        event_endpoint,
                        location,
                    );
                }
                BarRouted::Handled
            }
            // An ordinary file manager declares the shared convention, whose
            // one command closes every window and ends the process. A row it
            // never declared names nothing.
            WindowEvent::AppBarMenu { item } => {
                if !tairix_window::is_quit(item) {
                    return BarRouted::Handled;
                }
                for win in windows.drain(..) {
                    let _ = client.close(win.window);
                }
                BarRouted::Ends(0)
            }
            _ => BarRouted::NotMine,
        }
    }

    /// Route one event to whatever it names, answering the exit code the
    /// process owes or `None` to carry on.
    ///
    /// The icon bar's own outcomes are resolved first
    /// ([`route_app_bar_event`]). Everything else is window-scoped: an id no
    /// live window carries is a window that has just closed, and the event has
    /// nowhere to land.
    #[allow(clippy::too_many_arguments)] // The run's whole mutable state, threaded explicitly.
    fn route_event(
        windows: &mut alloc::vec::Vec<OpenWindow>,
        client: &mut WindowClient<RtWindowTransport>,
        desktop: &mut Desktop,
        places: &mut Places,
        theme: &Theme,
        icons: &RefCell<Icons>,
        launcher: &RefCell<Launcher>,
        reads: &alloc::sync::Arc<Reads>,
        event_endpoint: u64,
        role: Role,
        event: &WindowEvent,
    ) -> Option<i32> {
        match route_app_bar_event(
            windows,
            client,
            desktop,
            places,
            theme,
            icons,
            reads,
            event_endpoint,
            role,
            event,
        ) {
            BarRouted::Handled => return None,
            BarRouted::Ends(code) => return Some(code),
            BarRouted::NotMine => {}
        }
        let index = event
            .window_id()
            .and_then(|id| windows.iter().position(|win| win.window == id))?;

        // The window manager resized (or maximized/restored) this window.
        // Re-map its frame region at the new client size and repaint so the
        // listing fills it; the browser lays out to the new viewport
        // automatically. A refused or unallocatable resize keeps the current
        // window rather than failing the app (fail closed).
        //
        // The reported size is adopted exactly: the declared minimum is the
        // window manager's to hold, and an app that pushed back here would
        // fight the drag frame by frame.
        if let WindowEvent::Resized {
            width_px,
            height_px,
            ..
        } = *event
        {
            let win = &mut windows[index];
            let new_mode = mode_for(width_px, height_px);
            if let Some((frames, surface)) = resize_frames(client, win.window, &new_mode) {
                // Adopting drops the old region, which unmaps it; the fresh
                // drawing surface holds none of the last frame's pixels, so
                // the repaint that follows can only be a whole one.
                win.frames = frames;
                win.mode = new_mode;
                win.surface = surface;
                if present_whole(win, client, theme, icons, desktop.scale()).is_err() {
                    return Some(fail(EXIT_CHANNEL_LOST, "present refused"));
                }
            }
            return None;
        }

        // Nobody can see the window, so the session gave its copy of the pixels
        // back and unmapped the region. Let go of this side too — the pages go
        // only when both do — and paint nothing: the redraw request that
        // follows the window being shown again is what re-attaches a fresh
        // region and fills it.
        if matches!(event, WindowEvent::ContentReleased { .. }) {
            windows[index].frames.release();
            return None;
        }

        let win = &mut windows[index];
        // The chrome toggle is a window-level gesture like the refresh below,
        // not a listing key: it changes what the frame is laid out from, so it
        // is applied to the record before this round's canvas is built and the
        // whole window is repainted at the new layout.
        let chrome_toggled = chrome_toggle(win, event);
        let canvas = Canvas {
            theme,
            mode: &win.mode,
            scale: desktop.scale(),
            chrome: win.chrome,
        };
        // The user asked this window to re-read what is there, so the rail
        // re-reads the mount table in the same gesture — and a component's
        // slot menu, which *is* that rail, is re-declared with it. The kernel
        // publishes no mount-change notification, so this gesture — not a poll
        // — is how a newly attached volume appears; nothing here spins waiting
        // for one. Read once and shared out, so the process's rail and the
        // window's can never disagree about what is mounted.
        if sidebar::is_refresh_request(
            &win.browser,
            canvas.scale,
            canvas.theme(),
            canvas.window(),
            canvas.chrome.toolbar,
            event,
        ) {
            let (home, volumes) = places_source();
            *places = Places::new(&home, &volumes);
            sidebar::refresh_places(&mut win.places, &home, &volumes);
            if role == Role::Desktop {
                declare_app_bar(client, event_endpoint, role, places);
            }
        }
        // One sink per round: every control the event reaches, and the rail
        // and listing for the marks they move themselves, report into this
        // one, which is what the present is clipped to.
        let mut damage = damage::sink();
        let window_id = win.window;
        let (repaint, close) = apply_event(
            &mut WindowState {
                browser: &mut win.browser,
                overlays: &mut win.overlays,
                places: &mut win.places,
            },
            &mut Acts {
                menu: MenuLink {
                    client,
                    window: window_id,
                    open: &mut win.menu,
                },
                launcher,
                reads,
            },
            canvas,
            event,
            &mut damage,
        );
        if close {
            close_window(windows, index, client);
            return None;
        }
        let repaint = merge(repaint, whole_if(chrome_toggled));
        if present_window(win, client, theme, icons, desktop.scale(), repaint, &damage).is_err() {
            return Some(fail(EXIT_CHANNEL_LOST, "present refused"));
        }
        None
    }

    /// Apply the chrome toggle `event` names to `win`, answering whether a
    /// band moved.
    ///
    /// `F9` shows or hides the places rail and `Ctrl+F9` the command toolbar.
    /// A window opens with neither, so these are what reach every command the
    /// toolbar carries — the view toggle, the sort cycle, the Trash tools —
    /// until the desktop settings application sets the same two fields from
    /// the user's stored preference.
    fn chrome_toggle(win: &mut OpenWindow, event: &WindowEvent) -> bool {
        let WindowEvent::Key {
            key: KeyInput::Pressed { key, modifiers },
            ..
        } = event
        else {
            return false;
        };
        match win.chrome.toggled_by(*key, *modifiers) {
            Some(chrome) => {
                win.chrome = chrome;
                true
            }
            None => false,
        }
    }

    /// Open one more window at `location` (the user's home when `None`),
    /// stating why when it cannot be opened.
    ///
    /// The window cap is this process's own resource bound, not a policy about
    /// how many folders a user may look at, so reaching it is stated rather
    /// than silently ignored.
    #[allow(clippy::too_many_arguments)] // The run's whole mutable state, threaded explicitly.
    fn open_more(
        windows: &mut alloc::vec::Vec<OpenWindow>,
        client: &mut WindowClient<RtWindowTransport>,
        desktop: &Desktop,
        places: &Places,
        theme: &Theme,
        icons: &RefCell<Icons>,
        reads: &alloc::sync::Arc<Reads>,
        event_endpoint: u64,
        location: Option<alloc::vec::Vec<String>>,
    ) {
        // No count of its own: a window's mapped frame region is bounded by
        // the session's per-client frame budget and by this process's own
        // address-space limit, both derived from the machine and both refusing
        // with a stated reason. A hand-picked ceiling in front of them would
        // only refuse windows the machine could have given.
        let Ok(mut win) = open_window(client, event_endpoint, desktop, places, reads, location)
        else {
            // Already stated by `open_window`; a component simply has one
            // fewer window and the desktop carries on.
            return;
        };
        // A window nobody can see is worse than none: a refused first present
        // takes it back down and says so, rather than leaving an empty frame
        // on the desktop.
        if present_whole(&mut win, client, theme, icons, desktop.scale()).is_err() {
            let _ = client.close(win.window);
            report_error("the new window could not be painted; it was closed again");
            return;
        }
        windows.push(win);
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
        /// — a refused launch is an answer, not a crash.
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

        /// Open the data file at the validated absolute path `file_path` in its
        /// associated viewer — the `OpenFile` half of activation.
        ///
        /// The associated application is resolved from the installed bundles'
        /// declared file-type associations ([`RtBundleSource`] +
        /// [`applications_for`], keyed off the file's leaf name), never a
        /// hard-coded viewer path. The first bundle that claims the file's type
        /// is launched with the file handed to it on `STDIN` (see
        /// [`launch_viewer`](Self::launch_viewer)). When no installed
        /// application claims the type the refusal is stated fail-loud on
        /// `stderr` and nothing is launched — an honest answer, never a
        /// fabricated open.
        fn open_file(&mut self, file_path: &str) {
            let name = path_leaf(file_path);
            let mut source = RtBundleSource;
            // A store that cannot be enumerated yields no candidate rather than
            // an error the user cannot act on; the honest "no application"
            // path below reports it.
            let bundles = source.installed_bundles().unwrap_or_default();
            // Copy the chosen bundle path out so the `bundles` borrow does not
            // outlive the launch call below.
            let chosen = applications_for(name, &bundles)
                .first()
                .map(|assoc| String::from(assoc.bundle_path()));
            match chosen {
                Some(bundle_path) => self.launch_viewer(&bundle_path, file_path, name),
                None => report_error(&alloc::format!("no application to open {name}")),
            }
        }

        /// Launch the viewer bundle at `bundle_path`, handing it the file at
        /// `file_path` on `STDIN` — the inherited-document hand-off (the TAIRiX
        /// spelling of `viewer < file`, `plans/NEW-FILEMANAGER.md` `FM6b`).
        ///
        /// The file manager opens the file **read-only in its own table** and
        /// wires that descriptor onto the child's `STDIN` slot
        /// ([`FdWire::Handle`]); the kernel clones the read-only open
        /// description into the child owner-checked, so the viewer reads the
        /// document with no filesystem capability of its own and there is no
        /// post-spawn channel or ordering race. The [`DOCUMENT_ROLE_ARG`] token
        /// tells the viewer it was handed a document (rather than to prompt),
        /// and the leaf name titles its window. The manager's own descriptor is
        /// closed regardless of the spawn outcome — the child holds its own
        /// counted clone. Launching is asynchronous and the child is reaped on
        /// the any-child wake exactly as [`launch`](Self::launch)'s children
        /// are; a refusal is stated fail-loud, never a fabricated open.
        fn launch_viewer(&mut self, bundle_path: &str, file_path: &str, display_name: &str) {
            // A negative (error) or out-of-range result is not a descriptor:
            // state the refusal and launch nothing (fail closed).
            let Ok(fd) = u32::try_from(tairix_rt::fs_open(file_path.as_bytes(), OpenFlags::READ))
            else {
                report_error(&alloc::format!("could not open {display_name}"));
                return;
            };
            let mut run_path = String::from(bundle_path);
            run_path.push_str("/Run");
            let label = bundle_leaf(bundle_path);
            let mut wires = [FdWire::Inherit; STD_STREAM_COUNT];
            wires[STDIN as usize] = FdWire::Handle(fd);
            let attach = SpawnAttach {
                wires,
                ..SpawnAttach::INHERIT
            };
            let pid = tairix_rt::spawn_attached(
                run_path.as_bytes(),
                &attach,
                &[
                    run_path.as_bytes(),
                    DOCUMENT_ROLE_ARG,
                    display_name.as_bytes(),
                ],
                &[],
            );
            // The child holds its own counted clone of the read-only open
            // description; drop the manager's copy either way so nothing leaks.
            let _ = tairix_rt::fs_close(fd);
            if pid < 0 {
                report_error(&alloc::format!("could not launch {label}"));
                return;
            }
            #[allow(clippy::cast_sign_loss)] // `pid >= 0` in this branch; it is a PID.
            self.in_flight.insert(pid as u64, label);
        }
    }

    /// The last non-empty component of a `/`-separated path
    /// (`/Apps/Notes.app` → `Notes.app`, `/Users/me/notes.txt` → `notes.txt`) —
    /// the leaf the user already sees, carrying no path prefix. An empty or
    /// `/`-only path (which the validated activation paths never produce) falls
    /// back to the whole string rather than an empty name.
    fn path_leaf(path: &str) -> &str {
        path.rsplit('/')
            .find(|part| !part.is_empty())
            .unwrap_or(path)
    }

    /// The bundle directory's leaf name (`/Apps/Notes.app` → `Notes.app`) — the
    /// label the fail-loud launch diagnosis names.
    fn bundle_leaf(bundle_path: &str) -> String {
        String::from(path_leaf(bundle_path))
    }

    /// The largest `AppInfo` manifest the bundle scan reads: a signed manifest
    /// is a small fixed header plus a bounded capability/MIME body, far under
    /// this, so a file larger than this at a bundle's `AppInfo` path is
    /// malformed and skipped (bounded read, never unbounded memory).
    const APPINFO_READ_MAX: usize = 64 * 1024;

    /// Depth bound on the app-store walk: bundles may be filed in nested plain
    /// subdirectories, but a pathological tree must not recurse without limit
    /// (fail closed). Ample for the store's real nesting.
    const MAX_BUNDLE_SCAN_DEPTH: usize = 8;

    /// The running-system [`BundleSource`]: the installed applications and the
    /// file types each declares, read from the on-disk app stores under the
    /// file manager's own `CAP_FS_ACCESS` (never a compiled-in list).
    ///
    /// It walks the machine-wide stores (`/System/Commands`, then
    /// `/System/Applications`, then `/Apps`), descending nested plain
    /// subdirectories and reading each `<Name>.app` bundle's `AppInfo` manifest
    /// for its declared MIME associations ([`association_from_appinfo`]). The
    /// MIME table is a display *hint* only: a bundle offered here is still
    /// launched through the ordinary signed load gate, which verifies its
    /// signature and capabilities. A store or manifest that cannot be read is
    /// skipped fail-closed, so a corrupt bundle is never offered on a guess.
    struct RtBundleSource;

    impl BundleSource for RtBundleSource {
        fn installed_bundles(&mut self) -> Result<alloc::vec::Vec<AppAssociation>, Errno> {
            let mut out = alloc::vec::Vec::new();
            for store in [
                SYSTEM_COMMAND_STORE,
                SYSTEM_APPLICATION_STORE,
                INSTALLED_APP_STORE,
            ] {
                collect_bundles(store, 0, &mut out);
            }
            Ok(out)
        }
    }

    /// Collect every `<Name>.app` bundle's declared associations under the
    /// directory `dir` into `out`, descending nested plain subdirectories to
    /// [`MAX_BUNDLE_SCAN_DEPTH`]. A directory that cannot be listed is skipped
    /// (an absent `/Apps` is not an error); a `.app` is a sealed unit and is
    /// never descended into.
    fn collect_bundles(dir: &str, depth: usize, out: &mut alloc::vec::Vec<AppAssociation>) {
        let Ok(stream) = tairix_rt::read_dir_all(dir.as_bytes()) else {
            return;
        };
        let Ok(entries) =
            tairix_browse::vfs::entries_from_dir_stream(dir, &stream, &mut RtLinkReader)
        else {
            return;
        };
        for entry in entries {
            let name = entry.name();
            let mut path = String::from(dir);
            path.push('/');
            path.push_str(name);
            if name.ends_with(BUNDLE_SUFFIX) {
                if let Some(assoc) = read_bundle_association(&path) {
                    out.push(assoc);
                }
            } else if entry.is_directory_backed() && depth < MAX_BUNDLE_SCAN_DEPTH {
                collect_bundles(&path, depth + 1, out);
            }
        }
    }

    /// What a paste must do with a node of `kind`.
    ///
    /// A symbolic link is *recreated*: streaming its bytes would leave a
    /// regular file holding the target's text, and following it would copy
    /// something the link only points at.
    fn copy_kind_of(kind: FileKind) -> CopyKind {
        match kind {
            FileKind::Directory => CopyKind::Directory,
            FileKind::Regular => CopyKind::File,
            FileKind::Symlink => CopyKind::Link,
        }
    }

    /// Recreate the symbolic link `source` at `dest` with the same stored
    /// target, or a terse reason.
    ///
    /// The target is copied verbatim and never resolved, so a relative one
    /// still reads relative to the *new* link's own directory — which is what
    /// duplicating a link means. Nothing is read or written through either
    /// link.
    fn recreate_link(source: &[String], dest: &[String]) -> Result<(), &'static str> {
        let from = spell_path(source)?;
        let to = spell_path(dest)?;
        let mut buf = alloc::vec![0u8; tairix_abi::fs::FS_SYMLINK_MAX];
        let read = tairix_rt::fs_readlink(from.as_bytes(), &mut buf);
        let len = usize::try_from(read).map_err(|_| "a shortcut's target could not be read")?;
        let target = buf
            .get(..len)
            .ok_or("a shortcut's target could not be read")?;
        if tairix_rt::fs_symlink(target, to.as_bytes()) != 0 {
            return Err("a shortcut could not be recreated");
        }
        Ok(())
    }

    /// Read the `AppInfo` manifest of the bundle at `bundle_path` and decode
    /// its declared associations, or `None` when the manifest cannot be read or
    /// does not parse (fail closed — the bundle is simply not offered).
    fn read_bundle_association(bundle_path: &str) -> Option<AppAssociation> {
        let mut manifest_path = String::from(bundle_path);
        manifest_path.push_str("/AppInfo");
        let bytes = read_bounded_file(manifest_path.as_bytes(), APPINFO_READ_MAX)?;
        association_from_appinfo(bundle_path, &bytes)
    }

    /// Read up to `max` bytes of the file at `path` (opened read-only), or
    /// `None` on any refusal. Bounded so a path that resolves to an
    /// unexpectedly huge file is refused rather than read without limit; the
    /// descriptor is closed either way.
    ///
    /// The one bounded read every consumer here shares — the bundle-manifest
    /// scan and the icon-artwork reader differ only in their ceiling, so
    /// neither carries its own copy of the open/read/close loop.
    fn read_bounded_file(path: &[u8], max: usize) -> Option<alloc::vec::Vec<u8>> {
        let fd = u32::try_from(tairix_rt::fs_open(path, OpenFlags::READ)).ok()?;
        let mut content = alloc::vec::Vec::new();
        // A modest heap read buffer: the files read here are small, and a stack
        // array of the full per-call I/O maximum would be a large-stack-array
        // defect.
        let mut chunk = alloc::vec![0u8; FS_IO_MAX];
        while content.len() < max {
            let want = chunk.len().min(max - content.len());
            let Ok(got) = tairix_rt::fs_read(fd, content.len() as u64, &mut chunk[..want]) else {
                let _ = tairix_rt::fs_close(fd);
                return None;
            };
            if got == 0 {
                break;
            }
            content.extend_from_slice(&chunk[..got]);
        }
        let _ = tairix_rt::fs_close(fd);
        Some(content)
    }

    /// Bound on one icon-artwork read: a single byte past the shared artwork
    /// ceiling, so an asset that exceeds it is *detected* as over-long rather
    /// than silently truncated into a decodable-looking one. The shared cache
    /// refuses anything longer before a byte of it reaches the decoder.
    const ARTWORK_READ_MAX: usize = MAX_ARTWORK_BYTES + 1;

    /// The grid's [`ArtworkReader`]: one shipped icon asset read through the
    /// app's own capability-checked filesystem access, under its own identity
    /// and with no authority beyond it.
    ///
    /// The read is bounded by [`ARTWORK_READ_MAX`], so an asset larger than the
    /// artwork ceiling comes back over-long and is refused before any decode;
    /// a missing or unreadable asset simply reads as `None`. Either way the
    /// tile falls back to its built-in glyph, so a tile is never blank.
    struct VfsArtworkReader;

    impl ArtworkReader for VfsArtworkReader {
        fn read(&mut self, path: &str) -> Option<alloc::vec::Vec<u8>> {
            read_bounded_file(path.as_bytes(), ARTWORK_READ_MAX)
        }
    }

    /// The grid's [`ArtworkRasteriser`]: the decode runs in a
    /// minimum-capability sandbox worker, never in this process.
    ///
    /// Icon artwork is a file on a volume — untrusted input — so its bytes go
    /// to the shared icon-rasterisation service running in a capability-empty
    /// child this binary re-enters itself as, and only validated pixels come
    /// back. A refusing, crashed, or replaced worker reports `None`, which the
    /// tile draws as its built-in glyph.
    struct SandboxRasteriser {
        /// The parser-sandbox seam: one worker, started on the first decode and
        /// replaced by the seam if it ever fails.
        sandbox: ParserSandbox<RtLauncher, tairix_rt::LogSink>,
    }

    impl ArtworkRasteriser for SandboxRasteriser {
        fn rasterise(&mut self, side: u32, bytes: &[u8]) -> Option<alloc::vec::Vec<u8>> {
            rasterise_icon(&mut self.sandbox, side, bytes).ok()
        }
    }

    /// Everything the file manager reads off its event loop, and the one worker
    /// that reads it.
    ///
    /// Three kinds of read used to happen on the loop that owes the window a
    /// frame: the directory the user navigated to, the folder cue every visible
    /// folder draws, and the three program stores the *Open With…* chooser is
    /// built from. Each is a read of somebody's disk, so each froze the window
    /// for as long as that disk took.
    ///
    /// They share one worker rather than taking one each. The app browses one
    /// place at a time, so these are never concurrent workloads — and a shared
    /// worker gives the order they are served in a single, stated answer:
    ///
    /// 1. **the listing**, because the user navigated and is waiting for it;
    /// 2. **the folder cues**, which decorate a listing already on screen;
    /// 3. **the bundle scan**, which the chooser waits on but which no frame
    ///    depends on.
    ///
    /// Nothing can starve: each request set is finite and is refilled only by
    /// the user asking again.
    struct Reads {
        work: tairix_rt::sync::Mutex<Work>,
        /// Signalled when a read is recorded, and on teardown.
        signal: tairix_rt::sync::Condvar,
        wake: tairix_rt::sync::WorkerWake,
    }

    /// The desks the worker serves, in one lock: they are read by the same
    /// thread and written by the same worker, so one lock is one ordering
    /// rather than three that could interleave.
    struct Work {
        listings: ListingDesk<FilesClient>,
        probes: Probes,
        bundles: tairix_util::defer::JobDesk<(), Vec<AppAssociation>>,
        /// Set once the app is tearing down, so a parked worker leaves instead
        /// of looking for work. Its own flag rather than one desk's, so the
        /// worker's exit test does not depend on which desk happens to carry
        /// one.
        stopping: bool,
    }

    /// One unit of work the reader took.
    enum Read {
        /// List this directory for the browser.
        List(Vec<String>),
        /// Probe these folders' occupancy as one batch.
        Probe(Vec<Vec<String>>),
        /// Walk the program stores for their declared file associations.
        Bundles,
    }

    impl Reads {
        /// A desk over `wake`, with no worker yet.
        fn new(wake: tairix_rt::sync::WorkerWake) -> Self {
            Self {
                work: tairix_rt::sync::Mutex::new(Work {
                    listings: ListingDesk::new(),
                    probes: Probes::new(),
                    bundles: tairix_util::defer::JobDesk::new(),
                    stopping: false,
                }),
                signal: tairix_rt::sync::Condvar::new(),
                wake,
            }
        }

        /// One worker's whole life: park until something is wanted, read it,
        /// deliver it, wake the loop.
        fn serve(&self) {
            loop {
                let job = {
                    let mut work = self.work.lock();
                    loop {
                        if work.stopping {
                            return;
                        }
                        if let Some(job) = Self::next_read(&mut work) {
                            break job;
                        }
                        work = self.signal.wait(work);
                    }
                };
                // The reads themselves, with no lock held: these are the calls
                // that used to stall the window.
                let owed = match job {
                    Read::List(target) => {
                        let listed = read_directory(&target);
                        self.work
                            .lock()
                            .listings
                            .deliver(FilesClient::Browser, target, listed)
                    }
                    Read::Probe(batch) => {
                        let answers = probe_batch(&batch);
                        self.work.lock().probes.deliver(answers)
                    }
                    Read::Bundles => {
                        let found = scan_bundles();
                        self.work.lock().bundles.deliver(found)
                    }
                };
                if owed {
                    self.wake.nudge();
                }
            }
        }

        /// The next unit of work, in the stated order.
        fn next_read(work: &mut Work) -> Option<Read> {
            if let Some((_, target)) = work.listings.next_job() {
                return Some(Read::List(target));
            }
            if let Some(batch) = work.probes.next_batch() {
                return Some(Read::Probe(batch));
            }
            work.bundles.next_job().map(|()| Read::Bundles)
        }

        /// Answer the browser's request for `components`, recording it if this
        /// desk does not already hold the answer.
        ///
        /// With no worker to answer it the directory is read on the calling
        /// thread instead, which is exactly what this app did before it had
        /// one: a recorded request nobody will serve would leave the window
        /// listing for ever, so the degradation is a real read, not a wait.
        fn list(&self, components: &[String]) -> Result<Listing, Errno> {
            let deferred = {
                let mut work = self.work.lock();
                if work.stopping {
                    None
                } else {
                    Some(work.listings.take(FilesClient::Browser, components))
                }
            };
            let Some(listing) = deferred else {
                return read_directory(components).map(Listing::Ready);
            };
            if matches!(listing, Ok(Listing::Pending)) {
                self.signal.notify_one();
            }
            listing
        }

        /// Answer the folder cue for `components`, recording the probe if this
        /// desk does not already hold the answer.
        ///
        /// This is called from *inside a paint*, so it must never read: an
        /// unknown cue is [`Probe::Pending`], the folder draws without it, and
        /// the answer is drawn a frame later. A desk with no worker records
        /// nothing and every folder simply draws plain — a cue is decoration,
        /// and exercising the user's directory-read authority on the calling
        /// thread to draw one is exactly what this avoids.
        fn probe(&self, components: &[String]) -> Probe {
            let (answer, recorded) = self.work.lock().probes.ask(components);
            // Only a folder this desk had not seen is worth a wake. Waking on
            // every ask would mean one `futex_wake` per folder per frame, for
            // work the worker has already been told about.
            if recorded {
                self.signal.notify_one();
            }
            answer
        }

        /// Ask for the program stores to be walked, answering with what they
        /// declare when there is no worker to walk them elsewhere.
        fn want_bundles(&self) -> Option<Vec<AppAssociation>> {
            let submitted = {
                let mut work = self.work.lock();
                if work.stopping {
                    drop(work);
                    return Some(scan_bundles());
                }
                work.bundles.submit(())
            };
            if submitted.wake {
                self.signal.notify_one();
            }
            None
        }

        /// Take a landed bundle scan, if one has.
        fn take_bundles(&self) -> Option<Vec<AppAssociation>> {
            self.work.lock().bundles.collect()
        }

        /// Ask the worker to leave and wake it.
        fn stop(&self) {
            let mut work = self.work.lock();
            work.stopping = true;
            work.listings.stop();
            work.probes.stop();
            work.bundles.stop();
            drop(work);
            self.signal.notify_all();
        }
    }

    /// Stops the reader on every way out, so it is not left reading a disk for
    /// a window that has gone.
    ///
    /// The thread is *detached* rather than joined: a worker mid-read of a slow
    /// disk would otherwise hold the teardown for as long as that disk takes,
    /// and it leaves at its next turn round its loop anyway.
    struct ReadsGuard(alloc::sync::Arc<Reads>);

    impl Drop for ReadsGuard {
        fn drop(&mut self) {
            self.0.stop();
        }
    }

    /// Start the reader, stating a refusal once.
    ///
    /// A kernel that will not grant the thread is not a failure: the reads move
    /// back onto the event loop, which is exactly where they used to be.
    fn spawn_reader(reads: &alloc::sync::Arc<Reads>) -> Option<tairix_rt::thread::JoinHandle<()>> {
        let served = alloc::sync::Arc::clone(reads);
        match tairix_rt::thread::Thread::spawn(move || served.serve()) {
            Ok(handle) => Some(handle),
            Err(err) => {
                report_error(&alloc::format!(
                    "no reader thread ({err:?}); directory listings happen on the event loop"
                ));
                None
            }
        }
    }

    /// The browser's directory seam: a [`DirectorySource`] that records a
    /// request and answers with whatever has come back.
    #[derive(Clone)]
    struct DeferredSource(alloc::sync::Arc<Reads>);

    impl DirectorySource for DeferredSource {
        fn list(&mut self, components: &[String]) -> Result<Listing, Errno> {
            self.0.list(components)
        }

        fn has_children(&mut self, components: &[String]) -> Result<Probe, Errno> {
            Ok(self.0.probe(components))
        }
    }

    /// Read the directory named by root-first `components` under this app's own
    /// identity, through the same validated path spelling and stream decode the
    /// synchronous source uses.
    fn read_directory(components: &[String]) -> Result<Vec<Entry>, Errno> {
        let path = tairix_browse::vfs::absolute_path(components)?;
        let stream = list_directory(&path)?;
        tairix_browse::vfs::entries_from_dir_stream(&path, &stream, &mut RtLinkReader)
    }

    /// Probe every folder in `batch`, answering only for those the probe
    /// decided.
    ///
    /// A folder the user may not read, or that has gone, contributes no answer
    /// rather than a guess: it draws plain, exactly as a refused probe always
    /// has.
    fn probe_batch(batch: &[Vec<String>]) -> Vec<(Vec<String>, bool)> {
        let mut answers = Vec::new();
        for folder in batch {
            let Ok(path) = tairix_browse::vfs::absolute_path(folder) else {
                continue;
            };
            let mut buf = [0u8; tairix_browse::vfs::PROBE_BUF_LEN];
            let occupied = match probe_directory(&path, &mut buf) {
                Ok(0) => false,
                Ok(_) | Err(Errno::BufferTooSmall) => true,
                Err(_) => continue,
            };
            answers.push((folder.clone(), occupied));
        }
        answers
    }

    /// Walk the machine-wide program stores for the file types their bundles
    /// declare.
    fn scan_bundles() -> Vec<AppAssociation> {
        let mut out = Vec::new();
        for store in [
            SYSTEM_COMMAND_STORE,
            SYSTEM_APPLICATION_STORE,
            INSTALLED_APP_STORE,
        ] {
            collect_bundles(store, 0, &mut out);
        }
        out
    }

    /// The grid's icon-artwork pipeline over this app's live seams.
    ///
    /// The policy — the reclaim-governed decode cache, the deferred-decode
    /// desk a paint resolves through, and the one decode per loop turn the
    /// pump runs — is the host-tested [`crate::icons::IconPipeline`]. This
    /// names the reader and rasteriser it runs over.
    type Icons = crate::icons::IconPipeline<VfsArtworkReader, SandboxRasteriser>;

    /// Build the grid's pipeline for a window whose frame is `frame_bytes`
    /// long, so the artwork it may retain scales with the surface it draws on.
    ///
    /// The cache is built through the one shared constructor with this app's
    /// real seat, frame size, pressure gauge, and audit sink, so it is
    /// classified and budgeted by the same desktop policy the session's caches
    /// obey rather than by numbers picked here.
    fn open_icons(frame_bytes: usize) -> Icons {
        // The reclaim bookkeeping's audit sink. The shared constructor takes a
        // `'static` borrow, and the runtime sink is a unit value that owns
        // nothing.
        static LOG_SINK: tairix_rt::LogSink = tairix_rt::LogSink;
        let cache = artwork_cache(
            "files.icon-artwork",
            SEAT_PRIMARY,
            frame_bytes,
            tairix_rt::pressure::gauge(),
            &LOG_SINK,
        );
        // Decoded artwork is memory only this process can see, so the app says
        // what it holds; nothing outside it can sample the counters. A cache
        // declared unclassifiable has no ledger and holds nothing, so there is
        // simply nothing to report.
        if let Some(ledger) = cache.ledger() {
            tairix_rt::cachereport::register(ledger);
        }
        Icons::new(
            cache,
            VfsArtworkReader,
            SandboxRasteriser {
                sandbox: ParserSandbox::new(RtLauncher::own_binary(), tairix_rt::LogSink),
            },
        )
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
    /// degrades into a busy-poll. It also carries the memory-pressure member
    /// ([`PRESSURE_TOKEN`]): a band change wakes the park and the retained
    /// artwork is trimmed there and then, so memory goes back when the machine
    /// asks for it rather than at whatever later moment the user next types.
    /// The reader's wake ([`READS_TOKEN`]) is the one member the source cannot
    /// serve itself: the answer is the loop's to adopt, so the wake is drained
    /// here and the wait ends without an event.
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
        /// The grid's artwork pipeline, trimmed on a [`PRESSURE_TOKEN`] wake,
        /// shared with the present path that draws through it.
        icons: &'a RefCell<Icons>,
        /// The reader's wake, drained on a [`READS_TOKEN`] wake. Its readiness
        /// is a level peek, so leaving it undrained would report ready for
        /// ever and turn the park into a spin.
        reads: &'a Reads,
    }

    /// Whether a received mailbox frame is a genuine event from the desktop
    /// session: exactly one [`WindowEvent`] wide and from the kernel-attested
    /// `server` origin. A short frame or a foreign sender is dropped — the
    /// mailbox is open to any capable sender, so the attested origin is the
    /// authentication (fail closed).
    fn accept_frame(len: usize, sender: &[u8; ORIGIN_WIRE_LEN], server: ProcId) -> bool {
        len == WindowEvent::WIRE_LEN
            && Origin::from_bytes(sender).is_ok_and(|origin| origin.proc_id() == server)
    }

    impl EventSource for RtEventSource<'_> {
        fn try_next(&mut self, event: &mut [u8; WindowEvent::WIRE_LEN]) -> Result<bool, Errno> {
            loop {
                let mut sender = [0u8; ORIGIN_WIRE_LEN];
                match tairix_rt::ipc_recv(self.endpoint, event, &mut sender) {
                    Ok(len) if accept_frame(len, &sender, self.server) => return Ok(true),
                    Ok(_) => {}
                    Err(err) if Errno::from_syscall(err) == Errno::WouldBlock => return Ok(false),
                    Err(err) => return Err(Errno::from_syscall(err)),
                }
            }
        }

        fn park(&mut self) -> Result<Parked, Errno> {
            // Park until the session's next delivery — or a launched bundle's
            // exit — wakes the wait-set, never a spin. A cache-report change
            // the rate limiter is holding back only ever *tightens* the park
            // to the moment it may be sent; with nothing pending the park
            // stays indefinite.
            let mut token = 0u64;
            let timeout_ns = tairix_rt::cachereport::fold_wait_deadline_ns(u64::MAX);
            let waited = tairix_rt::waitset_wait(self.set, timeout_ns, &mut token);
            if waited != 0 {
                if Errno::from_syscall(waited) != Errno::TimedOut {
                    return Err(Errno::NotFound);
                }
                // No member woke, so `token` names the *previous* wake's
                // source and acting on it would reap or trim for nothing. The
                // held-back report is the only bounded wait here.
                tairix_rt::cachereport::publish_if_due();
                return Ok(Parked::Served);
            }
            // The reader answered. Draining is the whole of noticing it, and
            // the answer is the loop's to adopt, so the wait ends here rather
            // than parking again on a source that is still ready.
            if token == READS_TOKEN {
                self.reads.wake.drain();
                return Ok(Parked::Interrupted);
            }
            // A child-exit wake reaps the exited bundle(s) in place, so a
            // launched app is never left a zombie and the ready child member
            // cannot spin the park (it is drained the instant it fires).
            if token == CHILD_TOKEN {
                self.launcher.borrow_mut().reap();
            } else if token == PRESSURE_TOKEN && tairix_procinfo::pressure::refresh() {
                // The machine's band moved: give back whatever the new band
                // says the decoded artwork may no longer keep, here at the
                // wake rather than at the next user input. A band that did not
                // really move costs one read and no eviction work. The pixels
                // the pump is holding are offered again, since the band that
                // refused them has changed. The glyph cache this window drew
                // through gives back the same way.
                self.icons.borrow_mut().trim();
                tairix_font::trim_glyph_cache();
            }
            Ok(Parked::Served)
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
        /// The recoverable move-to-Trash plan when the removal can be carried
        /// out as same-volume renames into the user's Trash
        /// (`plans/NEW-FILEMANAGER.md` `FM10`): the per-target
        /// source→destination renames, captured at open time so the dialog's
        /// "Move to Trash" wording and the confirmed action stay in step.
        /// `None` when Trash is unavailable or cross-volume, in which case the
        /// removal is the irreversible unlink and the dialog says so.
        trash_moves: Option<alloc::vec::Vec<(alloc::vec::Vec<String>, alloc::vec::Vec<String>)>>,
    }

    /// A running long file operation the event loop drives interleaved with
    /// input (`plans/NEW-FILEMANAGER.md` `FM7b`): the driving job plus its
    /// progress + latched-cancel display state.
    ///
    /// `None` unless a confirmed delete or a paste is in progress. It owns the
    /// window while it runs — no navigation happens behind it — and the event
    /// loop advances it a bounded slice at a time ([`advance_operation`]),
    /// repainting the progress panel and draining (non-blocking) a mid-run
    /// cancel, so a large recursive removal or copy never freezes the window
    /// and never busy-spins. A latched cancel stops the job at the next step
    /// boundary — between nodes, or between copy chunks, never mid-node.
    struct Operation {
        /// The long operation being driven — a recursive removal or a paste.
        /// Each holds its exact position between slices, so a cancel or a
        /// preemption loses no work.
        job: Job,
        /// The progress + latched-cancel display state the panel renders.
        progress: ProgressModel,
    }

    /// The long file operation an [`Operation`] drives, interleaved with the
    /// event loop.
    enum Job {
        /// A recursive removal, driven by a single [`DeleteWalk`]. The
        /// cross-volume-move source cleanup inside a [`Paste`] reuses the same
        /// walk definition, so a delete and a move's cleanup can never diverge.
        Delete(DeleteWalk),
        /// A move/copy paste of a captured plan, driven by the [`Paste`] state
        /// machine.
        Paste(Paste),
        /// A recoverable move to Trash (`plans/NEW-FILEMANAGER.md` `FM10`),
        /// driven by a [`TrashRun`]: one same-volume `fs_rename` per target
        /// into the user's Trash directory, in place of an irreversible unlink.
        Trash(TrashRun),
    }

    /// One window's link to the desktop's menu service: the channel a chain is
    /// asked for over, the window the ask is scoped to, and the open id that
    /// window is waiting on an answer for.
    ///
    /// One value rather than three parameters threaded separately, because the
    /// exactly-once rule reads all three together: an answer is acted on only
    /// when it names the open *this* window minted, and the desktop never
    /// reuses an id, so anything else answers a gesture already settled.
    struct MenuLink<'a> {
        /// The window channel the open is sent over.
        client: &'a mut WindowClient<RtWindowTransport>,
        /// The session's id for the window the chain belongs to.
        window: u64,
        /// The open id of this window's unanswered menu, if one is up.
        open: &'a mut Option<u64>,
    }

    /// The window's own mutable state one event round may change.
    ///
    /// One value rather than three parameters threaded separately, because
    /// every level of the router chain carries all three: what the window is
    /// showing (the browser), what is layered over it (the overlays), and the
    /// rail beside it (the places).
    struct WindowState<'a, S: DirectorySource> {
        browser: &'a mut Browser<S>,
        overlays: &'a mut Overlays,
        places: &'a mut Places,
    }

    /// What one event round acts *through*, as against what it acts *on*.
    ///
    /// The desktop's menu chain and the app launcher: neither is window state,
    /// and both are carried at every level of the router chain, so they travel
    /// as one value for the reason [`WindowState`] does.
    struct Acts<'a> {
        menu: MenuLink<'a>,
        launcher: &'a RefCell<Launcher>,
        /// The reader every deferred read is asked for through.
        reads: &'a Reads,
    }

    /// What an "Open With…" chooser needs once the bundle scan answers: the
    /// file it was asked about, by the same spelling every open uses.
    struct PendingChooser {
        /// The absolute path of the selected file.
        path: String,
        /// Its leaf name, which the candidate match is keyed off.
        name: String,
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
        /// The "Open With…" application chooser, when open (chosen from the
        /// context menu on a regular file).
        ///
        /// A chooser, not a menu: the candidates are as many as the
        /// applications a user has installed, which no menu plate can promise
        /// to hold, so this window draws a scrolled list of its own
        /// (`plans/NEW-MENUS.md` §6, decision 2). The right-click menu itself
        /// is the desktop's chain and appears nowhere here.
        open_with: Option<OpenWithChooser>,
        /// The file the "Open With…" chooser is being built for, while the
        /// installed bundles are still being read.
        ///
        /// The candidates are the declared associations of every bundle in
        /// three program stores, which is one manifest read per installed
        /// application — far more than a frame's worth, and it used to happen
        /// on the very click that asked. So the click records what it asked
        /// about here and the chooser opens when the scan lands.
        pending_open_with: Option<PendingChooser>,
        /// The running long file operation (a recursive delete), when one is in
        /// progress. While it is set the event loop drives it interleaved with
        /// input rather than parking, showing progress and honouring a cancel.
        operation: Option<Operation>,
        /// The held cut/copy clipboard, captured by `Ctrl+X`/`Ctrl+C` and
        /// consumed by `Ctrl+V`. It lives in the app (not the browser), so it
        /// survives navigating to the paste target; a `Cut` is cleared once
        /// pasted (its sources have moved), a `Copy` is kept so it can be
        /// pasted again elsewhere.
        clipboard: Option<Clipboard>,
        /// Whether the launching user holds `CAP_FS_CHOWN` — the one gate on
        /// offering the ownership control (read once at start-up).
        can_chown: bool,
        /// The pointer double-click detector: a second quick primary press on
        /// the same item activates it, exactly as `Enter` does. It lives in the
        /// app (the engine is pointer-agnostic) and is reset whenever a press
        /// lands on chrome rather than an item, so a click through the toolbar
        /// or the places rail never pairs across it.
        double_click: DoubleClickTracker,
    }

    /// How the window is drawn at this moment: the active theme, the
    /// surface a frame is laid out for, and the desktop's UI density.
    ///
    /// One value rather than three parameters, because every consumer
    /// resolves the same geometry from them, and threading them separately
    /// is how a painted frame and the hit-test that must match it come to
    /// disagree. All three change together when the desktop does.
    #[derive(Copy, Clone)]
    struct Canvas<'a> {
        /// The active theme, as the desktop reports its appearance.
        theme: &'a Theme,
        /// The surface the current frame is laid out for.
        mode: &'a DisplayMode,
        /// The desktop's UI density.
        scale: Scale,
        /// The chrome bands this window is showing, so every measurement of
        /// the listing and every hit-test that inverts one read the same
        /// answer as the paint did.
        chrome: Chrome,
    }

    impl<'a> Canvas<'a> {
        /// The active theme.
        fn theme(&self) -> &'a Theme {
            self.theme
        }

        /// The whole window, as a rectangle at its own origin.
        fn window(&self) -> Rect {
            Rect::new(0, 0, self.mode.width_px, self.mode.height_px)
        }
    }

    /// Render the browser into the window's retained surface, clipped to
    /// `target.damage`, and present that rectangle.
    ///
    /// Clipping is sound because the surface lives for the life of the window:
    /// every pixel outside the clip is the one already on screen, and the
    /// single shared frame holds the same. A round that could not describe
    /// what it changed asks for the whole window instead.
    ///
    /// The window's title is the location it is showing, so the frame begins by
    /// retitling when — and only when — the browser has moved since the last
    /// one. A repaint that did not move sends nothing.
    ///
    /// The ownership control is drawn on the Properties overlay only where the
    /// launching user holds `CAP_FS_CHOWN` (`overlays.can_chown`), so a session
    /// that cannot use it is never shown it.
    ///
    /// The frame also resolves the folder-occupancy of exactly the entries it
    /// is about to draw, so an empty folder and a full one are drawn apart.
    /// The probe is bounded by the visible range, so a listing of
    /// a hundred thousand entries costs one directory read per row on screen,
    /// not per entry, and each answer — including a refusal — is remembered
    /// until the next listing.
    #[allow(clippy::too_many_arguments)] // The window's state, its chrome, and the frame target.
    fn present_frame<S, T>(
        browser: &mut Browser<S>,
        overlays: &Overlays,
        places: &Places,
        chrome: Chrome,
        theme: &Theme,
        target: &mut FrameTarget<'_, T>,
        icons: &RefCell<Icons>,
        scale: Scale,
    ) -> Result<(), Errno>
    where
        S: DirectorySource,
        T: WindowTransport,
    {
        let rename = overlays.rename.as_ref();
        let properties = overlays.properties.as_ref();
        let owner = overlays.owner.as_ref();
        let can_chown = overlays.can_chown;
        let mode = target.mode;
        let window = Rect::new(0, 0, mode.width_px, mode.height_px);
        // The rail owns the window's leading edge, so every overlay drawn over
        // the view is placed within what is left — the one shared inset the
        // pointer hit-tests resolve through, so a dialog is never centred over
        // the rail it does not belong to.
        let rail = chrome.rail(places);
        let toolbar = chrome.toolbar;
        let viewport = tairix_browse::render::content_area(window, scale, theme, rail, toolbar);
        browser.resolve_occupancy(tairix_browse::render::visible_range(
            browser, scale, theme, viewport, toolbar,
        ));
        let browser = &*browser;
        // The title is the location, so it is sent only when the browser has
        // moved. A refused retitle leaves the remembered text alone rather than
        // claiming a title the session does not carry: the next frame retries.
        if let Some(title) = retitle(browser, target.title) {
            if target.client.set_title(target.window, &title).is_ok() {
                *target.title = title;
            }
        }
        // Each visible grid tile resolves its icon through the artwork
        // pipeline: the shipped raster master for the entry's content type,
        // read bounded and decoded in the sandbox once per (asset, pixel side)
        // and retained, falling back to the built-in glyph whenever no artwork
        // resolves. Only the tiles the grid actually draws are asked for, so
        // nothing scrolled out of view is ever decoded. The borrow is taken for
        // the render alone; the parked event source holds it only to trim.
        let damage = target.damage;
        let surface = &mut *target.surface;
        surface.with_clip(
            damage.x,
            damage.y,
            damage.width_px,
            damage.height_px,
            |surface| {
                {
                    let mut pipeline = icons.borrow_mut();
                    render_into(
                        surface,
                        browser,
                        scale,
                        theme,
                        window,
                        &ManagerChrome {
                            tools: MANAGER_TOOLS,
                            tool_model: manager_tool_model(browser),
                            sidebar: rail,
                            toolbar,
                        },
                        &mut pipeline.source(),
                    );
                }
                // In rename mode, overlay the inline editor exactly over the
                // selected item's row through the shared selection geometry, so
                // the field sits on the item the user is renaming.
                if let Some(field) = rename {
                    if let Some(bounds) = tairix_browse::render::selection_rect(
                        browser, scale, theme, viewport, toolbar,
                    ) {
                        field.render(surface, bounds, scale, theme);
                    }
                }
                // With the Properties overlay open, draw it centered on top of
                // the view (the shared drawn panel painting the
                // already-authorised metadata). Rename and Properties are never
                // open together.
                if let Some(props) = properties {
                    draw_properties_editable(surface, props, scale, theme, viewport);
                    // Reassigning an owner is privileged, so the ownership
                    // control is drawn only where the launching user holds
                    // `CAP_FS_CHOWN` — never shown to a session that cannot use
                    // it.
                    if can_chown {
                        let active = owner.map(|ed| (ed.field, &ed.editor));
                        draw_owner_control(surface, props, scale, theme, viewport, active);
                    }
                }
                // The delete-confirmation dialog is modal: drawn last, on top of
                // the view, and never open together with the rename/Properties
                // overlays.
                if let Some(confirm) = overlays.delete.as_ref() {
                    draw_delete_dialog(surface, &confirm.dialog, scale, theme, viewport);
                }
                // The "Open With…" chooser is modal and draws on top of the
                // view. The right-click menu is not drawn here at all — its
                // plates are the desktop's own surfaces, so this window never
                // paints a menu pixel.
                if let Some(chooser) = overlays.open_with.as_ref() {
                    draw_open_with_chooser(surface, chooser, scale, theme, viewport);
                }
                // A running long operation's progress + cancel panel is modal:
                // drawn last so it is topmost while the walk runs interleaved
                // with input.
                if let Some(operation) = overlays.operation.as_ref() {
                    draw_progress_dialog(surface, &operation.progress, scale, theme, viewport);
                }
            },
        );
        winframe::encode(surface, target.frame, mode, damage, &SERIAL)?;
        let window = target.window;
        target.client.present(window, 0, damage)
    }

    /// Apply one delivered event to the browser, reporting whether the
    /// listing changed (and must re-present) and whether the app should
    /// end (the desktop asked the window to close).
    ///
    /// `canvas` gives the reveal/scroll helpers the same scale and content
    /// viewport the renderer uses, so the drawn view, the selection reveal,
    /// and the wheel scroll all agree on the geometry; `launcher` is the
    /// launched-bundle bookkeeping an activation spawns through.
    fn apply_event<S: DirectorySource>(
        win: &mut WindowState<'_, S>,
        acts: &mut Acts<'_>,
        canvas: Canvas<'_>,
        event: &WindowEvent,
        damage: &mut Region,
    ) -> (Repaint, bool) {
        let WindowState {
            browser,
            overlays,
            places,
        } = win;
        let theme = canvas.theme();
        let scale = canvas.scale;
        let window = canvas.window();
        // Everything below the rail lays out in what the rail leaves, resolved
        // through the one shared inset the renderer paints with, so a click
        // lands on exactly the control the user saw.
        let toolbar = canvas.chrome.toolbar;
        let viewport = tairix_browse::render::content_area(
            window,
            scale,
            theme,
            canvas.chrome.rail(places),
            toolbar,
        );

        // A close request closes *this* window whatever mode it is in; an open
        // rename edit or properties overlay is simply abandoned (nothing was
        // written). Whether the process then ends is the caller's to decide —
        // an ordinary file manager is its window, a component is not.
        //
        // The icon bar's own outcomes never reach here: they name the
        // application rather than a window, so the run resolves them before it
        // resolves which window an event belongs to.
        if let WindowEvent::CloseRequested { .. } = event {
            return (Repaint::Nothing, true);
        }

        // The one answer the desktop owes an open. An id that names anything
        // else answers a gesture already settled, so acting on it would run a
        // stale command.
        if let WindowEvent::MenuClosed {
            open_id, outcome, ..
        } = *event
        {
            if *acts.menu.open != Some(open_id) {
                return (Repaint::Nothing, false);
            }
            *acts.menu.open = None;
            let (changed, close) = apply_menu_outcome(
                browser,
                overlays,
                acts.launcher,
                acts.reads,
                scale,
                theme,
                viewport,
                toolbar,
                outcome,
            );
            return (whole_if(changed), close);
        }

        // The "Open With…" chooser owns input while open (the context-menu row
        // that opens it has already concluded the chain) and needs the launcher
        // to hand the chosen application its file.
        if overlays.open_with.is_some() {
            let (changed, close) = apply_open_with_event(
                overlays,
                acts.launcher,
                scale,
                theme,
                viewport,
                event,
                damage,
            );
            return (whole_if(changed), close);
        }

        // A modal overlay (the Properties overlay, or the owner-id editor
        // nested in it) owns the window while it is open; handle it and return.
        if let Some((changed, close)) =
            apply_modal_event(browser, overlays, scale, theme, viewport, event)
        {
            return (whole_if(changed), close);
        }

        // Rename mode: the inline editor owns the keyboard. Its keys never
        // navigate the listing, and non-key events leave the edit untouched.
        if overlays.rename.is_some() {
            let (changed, close) = match event {
                WindowEvent::Key {
                    key: KeyInput::Pressed { key, modifiers },
                    ..
                } => apply_rename_key(
                    browser,
                    &mut overlays.rename,
                    scale,
                    theme,
                    viewport,
                    toolbar,
                    *key,
                    *modifiers,
                ),
                _ => (false, false),
            };
            return (whole_if(changed), close);
        }

        // The rail owns the window's leading edge: its hover highlight tracks
        // every motion that reaches here, and it consumes the presses and keys
        // that belong to it. Whatever it does not consume routes to the view,
        // carrying any repaint the highlight alone owed. A window showing no
        // rail routes nothing to one: there is no drawn row for a press to
        // land on, and hit-testing a rail that was never painted would take
        // presses from the listing beneath it.
        let hover = if canvas.chrome.rail {
            reported_if(sidebar::track_hover(
                places, scale, theme, window, toolbar, event, damage,
            ))
        } else {
            Repaint::Nothing
        };
        if canvas.chrome.rail {
            if let Some(outcome) = sidebar::apply_event(
                browser, places, scale, theme, window, toolbar, event, damage,
            ) {
                if let Some(reason) = &outcome.refused {
                    report_error(reason);
                }
                return (merge(outcome.repaint, hover), false);
            }
        }

        let (repaint, close) = apply_nav_event(win, acts, canvas, viewport, event, damage);
        (merge(repaint, hover), close)
    }

    /// Route one event in plain navigation mode — no overlay is open, and the
    /// places rail did not claim it — reporting whether the view changed and
    /// whether the window should close.
    ///
    /// `viewport` is the rail-inset content area the listing and the scrollbar
    /// occupy; the toolbar band spans the whole window, which the routers below
    /// read from `canvas`.
    fn apply_nav_event<S: DirectorySource>(
        win: &mut WindowState<'_, S>,
        acts: &mut Acts<'_>,
        canvas: Canvas<'_>,
        viewport: Rect,
        event: &WindowEvent,
        damage: &mut Region,
    ) -> (Repaint, bool) {
        let WindowState {
            browser, overlays, ..
        } = win;
        let theme = canvas.theme();
        let scale = canvas.scale;
        let toolbar = canvas.chrome.toolbar;
        match event {
            WindowEvent::Key {
                key: KeyInput::Pressed { key, modifiers },
                ..
            } => {
                // Alt+Enter opens the Properties overlay, a plain Enter
                // activates the selection and Shift+Enter lists a bundle
                // rather than running it — the keyboard spelling of the
                // pointer's shift-double-click, so the two cannot diverge —
                // Delete opens the delete confirmation, and Ctrl+X/C/V drive
                // the clipboard verbs (all need the overlay/clipboard/launcher
                // state); every other navigation-mode key is handled by the
                // shared `apply_nav_key`.
                if matches!(key, KeyValue::Named(NamedKeyCode::Enter)) && modifiers.alt {
                    whole(begin_properties(browser, &mut overlays.properties))
                } else if matches!(key, KeyValue::Named(NamedKeyCode::Enter)) {
                    whole(activate(
                        browser,
                        acts.launcher,
                        scale,
                        theme,
                        viewport,
                        toolbar,
                        bundle_intent(modifiers.shift),
                        AfterHandoff::Keep,
                    ))
                } else if matches!(key, KeyValue::Named(NamedKeyCode::Delete)) {
                    whole(begin_delete(browser, &mut overlays.delete))
                } else if let Some(verb) = clipboard_verb(*key, *modifiers) {
                    whole(apply_clipboard_verb(
                        browser,
                        &mut overlays.clipboard,
                        &mut overlays.operation,
                        verb,
                    ))
                } else {
                    apply_nav_key(
                        browser,
                        &mut overlays.rename,
                        scale,
                        theme,
                        viewport,
                        toolbar,
                        *key,
                        *modifiers,
                        damage,
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
                    scale,
                    theme,
                    viewport,
                    toolbar,
                    i64::from(*dy),
                );
                if moved {
                    listing::scrolled(scale, theme, viewport, toolbar, damage);
                }
                (reported_if(moved), false)
            }
            // A pointer event the desktop routed into this window's local
            // coordinates: routed by `apply_pointer`.
            WindowEvent::Pointer { .. } => {
                apply_pointer(win, acts, canvas, viewport, event, damage)
            }
            // A secondary press on the window's Close control asks to leave the
            // folder rather than the window: it climbs to the parent and closes
            // only at the top, where there is nothing left to leave. A parent
            // that cannot be listed keeps the window open and states which place
            // was refused.
            WindowEvent::AlternateCloseRequested { .. } => match leave_directory(browser) {
                Leave::Climbed => (Repaint::Whole, false),
                Leave::Closed => (Repaint::Nothing, true),
                Leave::Refused(reason) => {
                    report_error(&reason);
                    (Repaint::Nothing, false)
                }
            },
            // Focus changes and key releases repaint nothing. The browser
            // never requests a pick, so a pick conclusion is a session bug and
            // is ignored rather than acted on (an unredeemed delegation is
            // reclaimed by the kernel at exit).
            //
            // Minimized needs no action: the window manager hides the
            // window and keeps its taskbar entry; the browser renders on
            // demand, so there is nothing to pause. A `Resized` is handled by
            // the event loop itself (it re-maps the frame region before
            // `apply_event` is called), so it never reaches this match; it is
            // listed here only to keep the arm exhaustive. These are honest
            // no-ops, not deferred work.
            //
            // A redraw request is already answered by the typed wait, which
            // re-presents the last frame with full-window damage before
            // handing the event on. The browser's state has not changed, so
            // rendering it again would draw the same pixels at the cost of a
            // second present.
            //
            // A desktop change is adopted and, when anything actually moved,
            // repainted by the event loop itself (through `desktop.apply`)
            // before `apply_event` is called; nothing here needs to react to
            // it a second time.
            // Both icon-bar events were resolved before this dispatch, by
            // the routing that owns the whole application's windows rather
            // than by one of them.
            //
            // A `MenuClosed` was resolved before this dispatch too, against the
            // open id the window is waiting on; one reaching here named no such
            // open and is a stale answer, dropped rather than acted on.
            WindowEvent::Key { .. }
            | WindowEvent::AppBarDefault
            | WindowEvent::AppBarMenu { .. }
            | WindowEvent::MenuClosed { .. }
            | WindowEvent::CloseRequested { .. }
            | WindowEvent::Focus { .. }
            | WindowEvent::Minimized { .. }
            | WindowEvent::RedrawRequested { .. }
            | WindowEvent::ContentReleased { .. }
            | WindowEvent::Resized { .. }
            | WindowEvent::FilePicked { .. }
            | WindowEvent::PickCancelled { .. }
            | WindowEvent::DesktopChanged { .. } => (Repaint::Nothing, false),
        }
    }

    /// The conclusion of a router that answers `(changed, close)` and cannot
    /// name what it changed.
    const fn whole(outcome: (bool, bool)) -> (Repaint, bool) {
        (whole_if(outcome.0), outcome.1)
    }

    /// The mounted volumes offered to the places rail, read from the live
    /// mount table through the shared System Information client.
    ///
    /// Only volumes this session can actually navigate to are offered: a mount
    /// that is not serving I/O (a surprise-removed device) or whose target is
    /// not valid text is left out rather than shown as a row that would fail
    /// on the first click. A refused or failed query yields no volumes at all,
    /// so the rail falls back to the user's own places rather than guessing
    /// what is mounted. The rail itself then re-validates every row.
    fn mounted_volumes() -> alloc::vec::Vec<Volume> {
        let mut volumes = alloc::vec::Vec::new();
        let _ = tairix_procinfo::for_each_mount(&IpcTransport, |record| {
            if record.availability() != tairix_abi::sysinfo::MountAvailability::Available {
                return Ok(WalkStep::Continue);
            }
            let Ok(target) = core::str::from_utf8(record.target_bytes()) else {
                return Ok(WalkStep::Continue);
            };
            let Some(label) = target.rsplit('/').find(|part| !part.is_empty()) else {
                return Ok(WalkStep::Continue);
            };
            volumes.push(Volume {
                label: String::from(label),
                target: String::from(target),
                medium: record.medium(),
            });
            Ok(WalkStep::Continue)
        });
        volumes
    }

    /// Everything the places rail is built from, read from the live system:
    /// the logged-in user's home and whatever is mounted right now.
    ///
    /// The one place the rail's inputs are gathered, so the first build and
    /// every later refresh read exactly the same sources.
    fn places_source() -> (alloc::vec::Vec<String>, alloc::vec::Vec<Volume>) {
        (home_components().unwrap_or_default(), mounted_volumes())
    }

    /// Route one pointer event in navigation mode, reporting whether the view
    /// changed (and must re-present).
    ///
    /// The layers are resolved in the order they overlap on screen. The
    /// right-edge scrollbar owns its gutter, so it gets first refusal on a
    /// primary press/drag/release — a click on the bar scrolls the listing
    /// instead of selecting an item beneath it, and it consumes only events
    /// that belong to it. A secondary-button press asks the desktop to open
    /// this window's context menu on the item under the pointer, and a primary
    /// press is routed by [`apply_primary_press`]. Every other pointer action
    /// is a no-op.
    ///
    /// `viewport` is the rail-inset content area; the toolbar band spans the
    /// whole window, which [`apply_primary_press`] reads from `canvas`.
    fn apply_pointer<S: DirectorySource>(
        win: &mut WindowState<'_, S>,
        acts: &mut Acts<'_>,
        canvas: Canvas<'_>,
        viewport: Rect,
        event: &WindowEvent,
        damage: &mut Region,
    ) -> (Repaint, bool) {
        let toolbar = canvas.chrome.toolbar;
        let WindowState {
            browser, overlays, ..
        } = win;
        let theme = canvas.theme();
        let scale = canvas.scale;
        let WindowEvent::Pointer {
            x,
            y,
            action,
            modifiers,
            ..
        } = event
        else {
            return (Repaint::Nothing, false);
        };
        let point = pointer_point(*x, *y);
        let mut scrolled = None;
        let mark = ViewMark::of(browser);
        for input in pointer_input_events(*action, point) {
            if let Some(repaint) = scroll_pointer(
                browser, scale, theme, viewport, toolbar, point, &input, damage,
            ) {
                scrolled = Some(scrolled.unwrap_or(false) || repaint);
            }
        }
        if let Some(repaint) = scrolled {
            // The bar reported its own drawn state; an offset it actually
            // moved draws every entry somewhere new besides.
            mark.report(browser, scale, theme, viewport, toolbar, damage);
            return (reported_if(repaint), false);
        }
        if *action == PointerAction::Moved {
            return (Repaint::Nothing, false);
        }
        if let Some(point) = secondary_press_point(*action, *x, *y) {
            let hit = tairix_browse::render::entry_index_at(
                browser, scale, theme, viewport, toolbar, point,
            );
            return whole(open_context_menu(
                browser,
                overlays,
                &mut acts.menu,
                point,
                hit,
            ));
        }
        match press_point(*action, *x, *y) {
            Some(point) => apply_primary_press(
                browser,
                overlays,
                acts.launcher,
                canvas,
                viewport,
                point,
                *modifiers,
                damage,
            ),
            None => (Repaint::Nothing, false),
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
        scale: Scale,
        theme: &Theme,
        viewport: Rect,
        event: &WindowEvent,
    ) -> Option<(bool, bool)> {
        // The delete-confirmation dialog is the topmost modal: while it is up
        // it owns the window, so it is handled before anything else.
        if overlays.delete.is_some() {
            return Some(apply_delete_event(overlays, scale, theme, viewport, event));
        }
        if overlays.owner.is_some() {
            return Some(match event {
                WindowEvent::Key {
                    key: KeyInput::Pressed { key, modifiers },
                    ..
                } => apply_owner_edit_key(
                    browser, overlays, *key, *modifiers, scale, theme, viewport,
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
                        apply_properties_pointer(browser, overlays, scale, theme, viewport, point)
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
    #[allow(clippy::too_many_arguments)] // The key, its context, and the round's report.
    fn apply_nav_key<S: DirectorySource>(
        browser: &mut Browser<S>,
        rename: &mut Option<TextField>,
        scale: Scale,
        theme: &Theme,
        viewport: Rect,
        toolbar: ToolbarBand,
        key: KeyValue,
        modifiers: AbiModifiers,
        damage: &mut Region,
    ) -> (Repaint, bool) {
        match key {
            // Toolbar-command accelerators: Alt+←/→/↑ drive the history and
            // climb commands, F5 refreshes — the same shared dispatch a toolbar
            // click uses, so the keyboard and the toolbar cannot disagree.
            KeyValue::Named(NamedKeyCode::Left) if modifiers.alt => whole(apply_toolbar_command(
                browser,
                scale,
                theme,
                viewport,
                toolbar,
                ToolbarCommand::Back,
            )),
            KeyValue::Named(NamedKeyCode::Right) if modifiers.alt => whole(apply_toolbar_command(
                browser,
                scale,
                theme,
                viewport,
                toolbar,
                ToolbarCommand::Forward,
            )),
            KeyValue::Named(NamedKeyCode::Up) if modifiers.alt => whole(apply_toolbar_command(
                browser,
                scale,
                theme,
                viewport,
                toolbar,
                ToolbarCommand::Up,
            )),
            KeyValue::Named(NamedKeyCode::F5) => whole(apply_toolbar_command(
                browser,
                scale,
                theme,
                viewport,
                toolbar,
                ToolbarCommand::Refresh,
            )),
            // Ctrl+Shift+N: the keyboard equivalent of the New Folder tool.
            // Shift may deliver 'n' upper- or lower-case, so match either.
            KeyValue::Char(ch)
                if modifiers.ctrl && modifiers.shift && ch.eq_ignore_ascii_case(&'n') =>
            {
                whole(begin_new_folder(
                    browser, rename, scale, theme, viewport, toolbar,
                ))
            }
            KeyValue::Named(NamedKeyCode::Down) => walk_selection(
                browser,
                scale,
                theme,
                viewport,
                toolbar,
                damage,
                Browser::select_next,
            ),
            KeyValue::Named(NamedKeyCode::Up) => walk_selection(
                browser,
                scale,
                theme,
                viewport,
                toolbar,
                damage,
                Browser::select_previous,
            ),
            KeyValue::Named(NamedKeyCode::Backspace) => {
                (whole_if(browser.go_up().unwrap_or(false)), false)
            }
            // F2 begins an in-place rename of the selected item; with nothing
            // selected (an empty directory) it is a no-op.
            KeyValue::Named(NamedKeyCode::F2) => whole(begin_rename(
                browser, rename, scale, theme, viewport, toolbar,
            )),
            _ => (Repaint::Nothing, false),
        }
    }

    /// Move the listing's focus with `step`, keep it on screen, and report the
    /// entries the mark moved between — or the whole item area when keeping it
    /// on screen scrolled the view.
    fn walk_selection<S: DirectorySource>(
        browser: &mut Browser<S>,
        scale: Scale,
        theme: &Theme,
        viewport: Rect,
        toolbar: ToolbarBand,
        damage: &mut Region,
        step: fn(&mut Browser<S>),
    ) -> (Repaint, bool) {
        let mark = ViewMark::of(browser);
        step(browser);
        tairix_browse::render::reveal_selection(browser, scale, theme, viewport, toolbar);
        (
            reported_if(mark.report(browser, scale, theme, viewport, toolbar, damage)),
            false,
        )
    }

    /// Activate the selected entry — the one dispatch-by-kind decision `Enter`
    /// drives, over the shared [`Browser::activate_selected`] so the file
    /// manager and the trusted picker act on the same [`Activation`]. The
    /// engine decides *what* the target is; the launch stays here, in the app's
    /// own capability-checked tail under the user's identity. `intent` says
    /// what the gesture meant for a bundle (run it, or list what is inside it)
    /// and `handoff` whether the window's job ends with a successful launch or
    /// open.
    ///
    /// * [`Activation::Descended`] — the engine descended into a directory (its
    ///   own transactional, fail-closed navigation); the selection is revealed
    ///   and the view repainted, exactly as a rail-click navigation is.
    /// * [`Activation::LaunchBundle`] — the entry is a `<Name>.app` bundle,
    ///   launched through the ordinary signed app-load gate ([`Launcher`]),
    ///   asynchronously so the event loop never blocks behind the load.
    ///   Launching changes nothing on screen, so nothing repaints.
    /// * [`Activation::OpenFile`] — the entry is a data file, opened in its
    ///   associated viewer ([`Launcher::open_file`]): the file manager opens
    ///   it read-only and hands it to the resolved viewer on `STDIN` (the
    ///   inherited-document hand-off), asynchronously so the event loop never
    ///   blocks. A file no installed application claims leaves the listing
    ///   unchanged and states the refusal fail-loud, never a fabricated open.
    ///
    /// A refused activation (an unreadable directory the engine could not
    /// descend into) leaves the browser where it was and repaints nothing
    /// (fail closed).
    #[allow(clippy::too_many_arguments)] // The selection's context, the intent, and what follows a handoff.
    fn activate<S: DirectorySource>(
        browser: &mut Browser<S>,
        launcher: &RefCell<Launcher>,
        scale: Scale,
        theme: &Theme,
        viewport: Rect,
        toolbar: ToolbarBand,
        intent: BundleIntent,
        handoff: AfterHandoff,
    ) -> (bool, bool) {
        match browser.activate_selected(intent) {
            Ok(Activation::Descended) => {
                tairix_browse::render::reveal_selection(browser, scale, theme, viewport, toolbar);
                (true, false)
            }
            Ok(Activation::LaunchBundle { path }) => {
                launcher.borrow_mut().launch(&path);
                (false, handoff == AfterHandoff::CloseWindow)
            }
            Ok(Activation::OpenFile { path }) => {
                launcher.borrow_mut().open_file(&path);
                (false, handoff == AfterHandoff::CloseWindow)
            }
            Err(_) => (false, false),
        }
    }

    /// Open the delete-confirmation dialog for the current selection, reporting
    /// a repaint. With nothing selected (an empty directory, or a cleared
    /// selection) [`Browser::plan_delete`] yields no plan and this is a no-op —
    /// the Delete verb is simply unavailable rather than a catastrophic empty
    /// or root removal (fail closed). The plan is captured now, so a listing
    /// change while the dialog is up cannot move what a confirmed delete
    /// removes.
    fn begin_delete<S: DirectorySource>(
        browser: &Browser<S>,
        delete: &mut Option<DeleteConfirm>,
    ) -> (bool, bool) {
        let Some(plan) = browser.plan_delete() else {
            return (false, false);
        };
        // Decide now — before showing the dialog — whether the removal can be a
        // recoverable move to Trash (`plans/NEW-FILEMANAGER.md` `FM10`), so the
        // confirmation's wording matches exactly what a confirmed delete will
        // do. A resolvable, same-volume, ensured Trash gives the per-target
        // rename plan; anything else falls back to the irreversible unlink and
        // the dialog says "Delete Permanently".
        let trash_moves = plan_trash_moves(&plan);
        let disposition = if trash_moves.is_some() {
            DeleteDisposition::Trash
        } else {
            DeleteDisposition::Permanent
        };
        let dialog = build_delete_dialog(&plan, disposition);
        *delete = Some(DeleteConfirm {
            dialog,
            plan,
            trash_moves,
        });
        (true, false)
    }

    /// Resolve, for a confirmed removal of `plan`, whether it can be carried
    /// out as a recoverable move to the user's Trash and, if so, the per-target
    /// source→destination rename plan (`plans/NEW-FILEMANAGER.md` `FM10`).
    ///
    /// Returns `Some(moves)` only when the removal is wholly recoverable:
    /// `HOME` resolves to a real per-user home, the `Library/Trash` subtree is
    /// ensured, and **every** target shares Trash's volume (so a single
    /// `fs_rename` carries each intact). Any other outcome — an absent or root
    /// `HOME`, a Trash directory that cannot be created or stat'd, a
    /// cross-volume target (a mounted volume under the current directory), or a
    /// name that cannot be given a collision-free home in Trash — returns
    /// `None`, and the removal falls back to the irreversible unlink (fail
    /// closed). Every call here is the user's own permission-checked I/O — no
    /// new capability, no ambient authority.
    fn plan_trash_moves(
        plan: &DeletePlan,
    ) -> Option<alloc::vec::Vec<(alloc::vec::Vec<String>, alloc::vec::Vec<String>)>> {
        let home = home_components()?;
        // A root (empty) home has no per-user Trash; fall back to permanent.
        if home.is_empty() {
            return None;
        }
        let trash = trash_dir(&home);
        if !ensure_trash_dir(&trash) {
            return None;
        }
        let trash_vol = VolumeId::new(stat_node(&trash)?.id.volume);
        // The names already in Trash: a move must never overwrite a
        // previously-trashed item, and a second same-named target in this very
        // batch must not collide with the first, so each resolved leaf is
        // reserved as it is chosen.
        let mut taken: alloc::vec::Vec<String> = read_children(&trash)
            .ok()?
            .into_iter()
            .map(|(name, _kind)| name)
            .collect();
        let mut moves = alloc::vec::Vec::with_capacity(plan.len());
        for target in plan.targets() {
            let item_vol = VolumeId::new(stat_name(target.path())?.id.volume);
            if trash_strategy(item_vol, trash_vol) != TrashStrategy::Move {
                // A cross-volume target cannot be renamed into Trash; the whole
                // removal takes the permanent path so the dialog stays honest.
                return None;
            }
            let dest = trash_dest_path(&trash, target.name(), &taken).ok()?;
            if let Some(leaf) = dest.last() {
                taken.push(leaf.clone());
            }
            moves.push((target.path().to_vec(), dest));
        }
        Some(moves)
    }

    /// The logged-in user's home directory as root-first path components, read
    /// from the `HOME` the session exported (the same source the trusted picker
    /// starts at). `None` when `HOME` is unset or not a valid absolute path
    /// (fail closed — the caller falls back to the permanent delete rather than
    /// guessing a home).
    fn home_components() -> Option<alloc::vec::Vec<String>> {
        let home = tairix_rt::env_var(b"HOME")?;
        let home = core::str::from_utf8(home).ok()?;
        tairix_browse::vfs::components_from_absolute_path(home).ok()
    }

    /// Ensure the `Library/Trash` directory exists, creating the `Library`
    /// parent then `Trash` under the user's own identity (`fs_mkdir` is
    /// idempotent here — an already-present directory is success). `home`
    /// itself is assumed to exist (it came from `HOME`). Returns `false`,
    /// fail closed, if either directory cannot be created and is not already a
    /// directory, so the trash move degrades to the permanent path.
    fn ensure_trash_dir(trash: &[String]) -> bool {
        // Trash lives at `<home>/Library/Trash`; ensure the immediate `Library`
        // parent, then Trash itself.
        trash.len() >= 2 && ensure_dir(&trash[..trash.len() - 1]) && ensure_dir(trash)
    }

    /// Ensure a single directory at `components` exists, under the user's own
    /// identity. `true` when it was created, or already exists as a directory;
    /// `false` (fail closed) when the path cannot be spelled, the create is
    /// refused, or the name exists as a non-directory.
    fn ensure_dir(components: &[String]) -> bool {
        let Ok(spelled) = spell_path(components) else {
            return false;
        };
        if tairix_rt::fs_mkdir(spelled.as_bytes()) == 0 {
            return true;
        }
        matches!(stat_node(components), Some(stat) if stat.kind == FileKind::Directory)
    }

    /// Handle one event while the delete-confirmation dialog owns the window.
    ///
    /// `Escape` (or a click on Cancel) dismisses the dialog and changes
    /// nothing; `Enter` (or a click on Delete) hands the confirmed removal to
    /// the interleaved operation runner — the event loop then carries it out a
    /// bounded slice at a time, showing progress and honouring a cancel. A
    /// click that lands on neither button, and every non-decision event, leaves
    /// the dialog open (fail closed). The removal is the user's own
    /// capability-checked `fs_unlink`s — no new capability, no ambient
    /// authority.
    fn apply_delete_event(
        overlays: &mut Overlays,
        scale: Scale,
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
                        delete_dialog_action_at(&confirm.dialog, viewport, scale, theme, point);
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
                // Hand the removal to the interleaved operation runner: the
                // event loop drives it a bounded slice at a time, showing
                // progress and honouring a mid-run cancel, so a large recursive
                // delete never freezes the window. The plan was captured at
                // open time, so a listing change cannot move what it targets;
                // the view is re-listed when the operation finishes. A
                // recoverable move to Trash (captured at open time, matching
                // the dialog's wording) renames each target into Trash;
                // anything else is the irreversible recursive unlink (`FM10`).
                overlays.operation = Some(match confirm.trash_moves {
                    Some(moves) => Operation {
                        job: Job::Trash(TrashRun::new(moves)),
                        progress: ProgressModel::new(ProgressOp::Trash),
                    },
                    None => Operation {
                        job: Job::Delete(DeleteWalk::from_plan(&confirm.plan)),
                        progress: ProgressModel::new(ProgressOp::Delete),
                    },
                });
                (true, false)
            }
        }
    }

    /// The number of walk steps a running [`Operation`] advances between
    /// repaints and cancel polls.
    ///
    /// Large enough that the whole-window repaint is amortised over many nodes,
    /// small enough that a cancel is honoured promptly — a fixed tuning bound,
    /// not a hardware-scaled capacity.
    const OPERATION_STEP_BUDGET: u32 = 64;

    /// Advance the running `operation` by up to [`OPERATION_STEP_BUDGET`]
    /// bounded units of work, returning `true` once it has finished — completed,
    /// cancelled at a step boundary, or stopped fail-closed on a refusal.
    ///
    /// Dispatches on the job kind so the event loop drives a delete and a paste
    /// through one interleaving path: each holds its exact position between
    /// calls, so a bounded slice per turn keeps the window responsive without
    /// losing or repeating work.
    fn advance_operation(operation: &mut Operation) -> bool {
        match &mut operation.job {
            Job::Delete(walk) => advance_delete_walk(walk, &mut operation.progress),
            Job::Paste(paste) => advance_paste(paste, &mut operation.progress),
            Job::Trash(run) => advance_trash(run, &mut operation.progress),
        }
    }

    /// A recoverable move to Trash in progress (`plans/NEW-FILEMANAGER.md`
    /// `FM10`): the captured per-target source→destination renames, carried out
    /// one item per step so the window stays responsive and a cancel is
    /// honoured between items. Each rename is a single same-volume `fs_rename`
    /// — no tree is walked, since the item moves intact — so a move of any size
    /// is cheap; the interleaving matches the delete/paste runners only so a
    /// pathological count of targets can still be cancelled.
    struct TrashRun {
        /// The remaining source→destination renames, in listing order.
        moves: alloc::vec::Vec<(alloc::vec::Vec<String>, alloc::vec::Vec<String>)>,
        /// The next move to carry out — the honest count already done.
        index: usize,
    }

    impl TrashRun {
        /// A fresh run over the captured move plan.
        fn new(moves: alloc::vec::Vec<(alloc::vec::Vec<String>, alloc::vec::Vec<String>)>) -> Self {
            Self { moves, index: 0 }
        }

        /// The honest count of items moved to Trash so far (the progress
        /// figure).
        fn done(&self) -> usize {
            self.index
        }
    }

    /// Advance a recursive removal `walk` by up to [`OPERATION_STEP_BUDGET`]
    /// steps, returning `true` once it has finished.
    ///
    /// Each step reads a directory (`fs_readdir`) or unlinks a node
    /// (`fs_unlink`, depth-first so contents go before their container) under
    /// the user's own identity — the ordinary permission-checked writes the
    /// user could perform themselves, no new capability — and updates the
    /// honest progress count from the walk's own figure. A latched cancel stops
    /// at the next boundary (never mid-node); the first refused read or unlink
    /// stops the removal, states the reason on `stderr` (fail loud), and leaves
    /// whatever was already removed removed rather than a fabricated success.
    fn advance_delete_walk(walk: &mut DeleteWalk, progress: &mut ProgressModel) -> bool {
        for _ in 0..OPERATION_STEP_BUDGET {
            if progress.is_cancel_requested() {
                return true;
            }
            // Copy the current step out so the walk is free to be mutated.
            let step = walk.next_action().map(|action| match action {
                DeleteAction::List(path) => (true, path.to_vec(), false),
                DeleteAction::Remove { path, is_directory } => (false, path.to_vec(), is_directory),
            });
            let Some((is_list, path, is_directory)) = step else {
                return true;
            };
            if is_list {
                let Ok(children) = removal_children(&path) else {
                    report_delete_refused(&path);
                    return true;
                };
                if walk.expand(&children).is_err() {
                    report_error("delete stopped: a folder is nested too deep");
                    return true;
                }
            } else {
                let Ok(spelled) = tairix_browse::vfs::absolute_path(&path) else {
                    report_error("delete stopped: a path could not be spelled");
                    return true;
                };
                let flags = if is_directory {
                    UnlinkFlags::DIRECTORY
                } else {
                    UnlinkFlags::empty()
                };
                if tairix_rt::fs_unlink(spelled.as_bytes(), flags) != 0 {
                    report_delete_refused(&path);
                    return true;
                }
                if walk.complete_removal().is_err() {
                    report_error("delete stopped: internal walk error");
                    return true;
                }
            }
            progress.set_done(walk.removed());
        }
        false
    }

    /// Advance a move-to-Trash `run` by up to [`OPERATION_STEP_BUDGET`] items,
    /// returning `true` once it has finished.
    ///
    /// Each step renames one target into its captured Trash destination
    /// (`fs_rename`) under the user's own identity — the ordinary
    /// permission-checked move the user could perform themselves, no new
    /// capability — and updates the honest progress count. A latched cancel
    /// stops at the next item boundary (never mid-rename); the first refused
    /// move stops the run, states the reason on `stderr` (fail loud), and
    /// leaves whatever already moved in Trash rather than a fabricated success.
    /// The destinations were resolved collision-free at open time, so a rename
    /// never overwrites an existing trashed item.
    fn advance_trash(run: &mut TrashRun, progress: &mut ProgressModel) -> bool {
        for _ in 0..OPERATION_STEP_BUDGET {
            if progress.is_cancel_requested() {
                return true;
            }
            // Copy the current move out so `run` is free to be advanced.
            let Some((source, dest)) = run.moves.get(run.index).cloned() else {
                return true;
            };
            if let Err(reason) = rename_item(&source, &dest) {
                report_trash_item_error(&source, reason);
                return true;
            }
            run.index += 1;
            progress.set_done(run.done());
        }
        false
    }

    /// State on `stderr` that the move to Trash stopped while handling `source`
    /// — an honest, fail-loud diagnosis naming the item and the reason, never a
    /// silent failure or a fabricated success. Carries no path prefix beyond
    /// the leaf name the user already sees.
    fn report_trash_item_error(source: &[String], reason: &str) {
        let name = source.last().map_or("", String::as_str);
        let _ = writeln!(Stderr, "files: could not move {name} to Trash: {reason}");
    }

    /// Read the children of the directory at `path`: each child's leaf name
    /// and the browser's own classification of it, through the same
    /// capability-checked listing call and shared decode the browser
    /// navigates with, so a walk sees exactly what the browser would.
    ///
    /// One read serves both walks, each taking the reading it needs: the
    /// delete walk asks whether a child is directory-backed *on disk*
    /// ([`removal_children`]), while the copy walk asks what a copy must *do*
    /// with it ([`copy_children`]) — which is not the same question for a
    /// symbolic link.
    fn read_children(path: &[String]) -> Result<alloc::vec::Vec<(String, EntryKind)>, Errno> {
        let spelled = tairix_browse::vfs::absolute_path(path)?;
        let stream = tairix_rt::read_dir_all(spelled.as_bytes()).map_err(Errno::from_syscall)?;
        let entries =
            tairix_browse::vfs::entries_from_dir_stream(&spelled, &stream, &mut RtLinkReader)?;
        Ok(entries
            .into_iter()
            .map(|entry| (entry.name().to_string(), entry.kind()))
            .collect())
    }

    /// The children of `path` as a [`DeleteWalk`] expansion wants them:
    /// whether each is directory-backed on disk, so a link is unlinked as the
    /// leaf it is rather than recursed into.
    fn removal_children(path: &[String]) -> Result<alloc::vec::Vec<(String, bool)>, Errno> {
        Ok(read_children(path)?
            .into_iter()
            .map(|(name, kind)| (name, kind.is_directory_backed()))
            .collect())
    }

    /// The children of `path` as a [`CopyWalk`] expansion wants them: what a
    /// copy must do with each.
    fn copy_children(path: &[String]) -> Result<alloc::vec::Vec<(String, CopyKind)>, Errno> {
        Ok(read_children(path)?
            .into_iter()
            .map(|(name, kind)| {
                let copy = if kind.is_directory_backed() {
                    CopyKind::Directory
                } else if kind.is_link() {
                    CopyKind::Link
                } else {
                    CopyKind::File
                };
                (name, copy)
            })
            .collect())
    }

    /// State on `stderr` that the item at `path` could not be removed — an
    /// honest, fail-loud diagnosis naming the item, never a silent failure or a
    /// fabricated success. Carries no path prefix or token beyond the leaf name
    /// the user already sees.
    fn report_delete_refused(path: &[String]) {
        let name = path.last().map_or("", String::as_str);
        let _ = writeln!(Stderr, "files: could not delete {name}");
    }

    /// State a `files:`-prefixed diagnosis on `stderr` — the one fail-loud
    /// reporting path a whole-operation refusal (a too-deep tree, an
    /// unspellable path, a rejected paste plan, an internal step error) states
    /// its reason through, shared by the delete and paste drives.
    fn report_error(reason: &str) {
        let _ = writeln!(Stderr, "files: {reason}");
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
        operation: &mut Option<Operation>,
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
            ClipboardVerb::Paste => run_paste(browser, clipboard, operation),
        }
    }

    /// Begin a paste of the held `clipboard` into the current directory as an
    /// interleaved [`Operation`], under the user's own identity (no new
    /// capability, no ambient authority — every step is the ordinary
    /// permission-checked write the user could perform themselves).
    ///
    /// The plan is validated first ([`plan_paste`]): pasting a folder into
    /// itself is refused outright (`WouldRecurse`) and nothing is enqueued. The
    /// destination directory's volume is stat'd once (it decides same- vs
    /// cross-volume for every item), and the plan is handed to a [`Paste`]
    /// state machine the event loop then advances a bounded slice at a time —
    /// showing progress and honouring a mid-run cancel, so a large copy never
    /// freezes the window. Each item's move-vs-copy is decided by
    /// [`paste_strategy`] as it runs — a same-volume move is one `fs_rename`, a
    /// cross-volume move is copy-then-delete, a copy always streams — and the
    /// run is fail closed: the first refused operation stops the paste, states
    /// the reason on `stderr` (fail loud), and leaves whatever already landed
    /// in place rather than a fabricated success. A `Cut` is consumed by
    /// initiating the paste (its sources are being moved); a `Copy` keeps the
    /// clipboard for another paste.
    fn run_paste<S: DirectorySource>(
        browser: &mut Browser<S>,
        clipboard: &mut Option<Clipboard>,
        operation: &mut Option<Operation>,
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
        // Hand the plan to the interleaved operation runner: the event loop
        // carries it out a bounded chunk at a time, showing progress and
        // honouring a mid-run cancel, so a large copy never freezes the window.
        // The view is re-listed when the operation finishes.
        *operation = Some(Operation {
            job: Job::Paste(Paste::new(op, dest_vol, plan.items().to_vec())),
            progress: ProgressModel::new(ProgressOp::Copy),
        });
        // A cut is consumed by initiating the paste — its sources are being
        // moved, so re-pasting the same cut elsewhere would name items that are
        // gone; a copy is kept so it can be pasted again.
        if op == ClipboardOp::Cut {
            *clipboard = None;
        }
        (true, false)
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

    /// A paste in progress: a captured plan carried out one bounded unit of
    /// work at a time so the event loop stays responsive.
    ///
    /// The move-vs-copy decision for each item is made as it runs
    /// ([`paste_strategy`]) from the two nodes' volume ids; a same-volume move
    /// is a single `fs_rename`, a copy streams the bytes through the reused
    /// buffer a chunk at a time, and a cross-volume move copies then removes
    /// the source through the shared delete walk. Every step is the user's own
    /// permission-checked write — no new capability, no ambient authority — so
    /// the read-only picker never builds a [`Paste`].
    struct Paste {
        /// Whether the plan moves (`Cut`) or copies (`Copy`) its items.
        op: ClipboardOp,
        /// The paste target directory's volume — one side of every item's
        /// same- vs cross-volume decision.
        dest_vol: VolumeId,
        /// The resolved plan items, carried out in order.
        items: alloc::vec::Vec<PasteItem>,
        /// The next item to begin (once [`stage`](Self::stage) is idle).
        index: usize,
        /// The in-flight stage of the item currently being carried out.
        stage: PasteStage,
        /// The source of the item currently in flight, for fail-loud error
        /// reporting (its leaf names the item the user sees).
        current_source: alloc::vec::Vec<String>,
        /// The honest count of nodes moved/copied so far (the progress figure);
        /// cross-volume cleanup removals are not counted as copied.
        done: usize,
        /// One reused, fixed-size copy buffer — never a per-file allocation and
        /// never sized to a file's length, so a copy of any size stays bounded.
        buf: alloc::vec::Vec<u8>,
    }

    /// The stage of the item a [`Paste`] is currently carrying out.
    enum PasteStage {
        /// No item in flight — [`Paste::step`] begins `items[index]` next, or
        /// finishes the paste when the queue is drained.
        Idle,
        /// Copying the current item's tree (a [`CopyWalk`] over one item);
        /// `transfer` is the in-flight leaf-file stream when a
        /// [`CopyAction::CopyFile`] step is underway. `then_delete` names a
        /// cross-volume move's source to remove once the copy fully succeeds.
        Copying {
            /// The recursive-copy cursor for this item's tree.
            walk: CopyWalk,
            /// The leaf file being streamed, or `None` between files.
            transfer: Option<Transfer>,
            /// A cross-volume move's `(source, is_directory)` to remove after
            /// the copy completes; `None` for a plain copy.
            then_delete: Option<(alloc::vec::Vec<String>, bool)>,
        },
        /// Removing a cross-volume move's source after its copy fully
        /// succeeded, through the shared delete walk.
        Deleting(DeleteWalk),
    }

    /// One leaf-file transfer in flight: the open source and destination
    /// handles plus the resumable [`CopyCursor`] over them, stepped one bounded
    /// chunk at a time so a large file never blocks the event loop.
    struct Transfer {
        /// The source file opened read-only.
        reader: tairix_rt::File,
        /// The destination file, created exclusively (a pre-existing name is
        /// refused, never clobbered).
        writer: tairix_rt::File,
        /// The bounded, resumable copy cursor over the two handles.
        cursor: CopyCursor,
    }

    impl Transfer {
        /// Open the source (read-only) and create the destination (exclusively)
        /// for a leaf-file copy, or a terse reason on refusal.
        fn open(source: &[String], dest: &[String]) -> Result<Self, &'static str> {
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
            Ok(Self {
                reader,
                writer,
                cursor: CopyCursor::new(stat.size),
            })
        }

        /// Carry one bounded chunk of the transfer through `buf`, returning
        /// `Ok(true)` once the whole file has been copied and `Ok(false)` when
        /// more remains. A source that ends before, or grows past, its stat'd
        /// length fails closed rather than looping or wrapping.
        fn step(&mut self, buf: &mut [u8]) -> Result<bool, &'static str> {
            let Some(chunk) = self.cursor.next_chunk() else {
                return Ok(true);
            };
            let want = usize::try_from(chunk.len()).map_err(|_| "a copy chunk was too large")?;
            let read = self
                .reader
                .read_at(chunk.offset(), &mut buf[..want])
                .map_err(|_| "a source file could not be read")?;
            if read == 0 {
                return Err("a source file ended early");
            }
            let wrote = self
                .writer
                .write_at(chunk.offset(), &buf[..read])
                .map_err(|_| "a destination file could not be written")?;
            if wrote != read {
                return Err("a destination file could not be fully written");
            }
            let carried = u64::try_from(read).map_err(|_| "a copy transfer was too large")?;
            self.cursor
                .advance(carried)
                .map_err(|_| "a source file changed during the copy")?;
            Ok(self.cursor.is_complete())
        }
    }

    /// One resolved [`CopyWalk`] step, copied out of the walk's borrowed
    /// [`CopyAction`] so the walk is free to be mutated by the report call that
    /// follows it.
    enum CopyStep {
        /// Create this destination directory (`fs_mkdir`).
        MakeDir(alloc::vec::Vec<String>),
        /// List this source directory's children (`fs_readdir`).
        List(alloc::vec::Vec<String>),
        /// Stream this leaf file's bytes from source to destination.
        CopyFile(alloc::vec::Vec<String>, alloc::vec::Vec<String>),
        /// Recreate this symbolic link at the destination, with the target it
        /// stores — never a byte-wise copy, which would leave a regular file
        /// holding the target's text.
        CopyLink(alloc::vec::Vec<String>, alloc::vec::Vec<String>),
    }

    /// The outcome of one [`Paste::step`] unit of work.
    enum StepOutcome {
        /// One bounded unit was carried out; the paste has more to do.
        Working,
        /// Every item is done — the paste has finished.
        Done,
        /// The step was refused; the paste stops fail-closed with this reason
        /// (stated on `stderr` naming the current item).
        Failed(&'static str),
    }

    impl Paste {
        /// A paste of `items` (in order) into a target on `dest_vol`, nothing
        /// begun yet.
        fn new(op: ClipboardOp, dest_vol: VolumeId, items: alloc::vec::Vec<PasteItem>) -> Self {
            Self {
                op,
                dest_vol,
                items,
                index: 0,
                stage: PasteStage::Idle,
                current_source: alloc::vec::Vec::new(),
                done: 0,
                buf: alloc::vec![0u8; FS_IO_MAX],
            }
        }

        /// The honest count of nodes moved/copied so far — the rising progress
        /// figure.
        fn done(&self) -> usize {
            self.done
        }

        /// The source of the item currently in flight, for fail-loud reporting.
        fn current_source(&self) -> &[String] {
            &self.current_source
        }

        /// Carry out one bounded unit of work, dispatching on the current
        /// stage. The stage is taken out (leaving [`Idle`](PasteStage::Idle))
        /// so each handler owns it and installs the next stage, sidestepping a
        /// self-borrow across the open file handles a [`Transfer`] holds.
        fn step(&mut self) -> StepOutcome {
            match core::mem::replace(&mut self.stage, PasteStage::Idle) {
                PasteStage::Idle => self.begin_next_item(),
                PasteStage::Copying {
                    walk,
                    transfer,
                    then_delete,
                } => self.step_copy(walk, transfer, then_delete),
                PasteStage::Deleting(walk) => self.step_delete(walk),
            }
        }

        /// Begin the next planned item, or finish when the queue is drained.
        ///
        /// A `Cut` back into the item's own directory is a no-op (the item is
        /// already where it would land); a `Copy` onto itself is refused rather
        /// than duplicating a file onto itself. Otherwise the source is stat'd
        /// for its kind and volume and [`paste_strategy`] picks the mechanism:
        /// a same-volume move renames in one syscall, a copy (and a
        /// cross-volume move's copy phase) starts a [`CopyWalk`].
        fn begin_next_item(&mut self) -> StepOutcome {
            let Some(item) = self.items.get(self.index) else {
                return StepOutcome::Done;
            };
            self.index += 1;
            self.current_source = item.source().to_vec();
            if item.overwrites_source() {
                return match self.op {
                    ClipboardOp::Cut => StepOutcome::Working,
                    ClipboardOp::Copy => {
                        StepOutcome::Failed("an item cannot be copied onto itself")
                    }
                };
            }
            let source = item.source().to_vec();
            let dest = item.dest().to_vec();
            // The name as typed: a symbolic link is what gets copied or
            // moved, so its own kind decides *how* (recreate, never stream
            // its bytes) and its own volume decides rename-vs-copy.
            let Some(stat) = stat_name(&source) else {
                return StepOutcome::Failed("a source item could not be read");
            };
            let source_vol = VolumeId::new(stat.id.volume);
            let kind = copy_kind_of(stat.kind);
            // A link is a leaf on disk however its target resolves, so the
            // removal half of a cross-volume move unlinks the link itself.
            let is_directory = kind == CopyKind::Directory;
            match paste_strategy(self.op, source_vol, self.dest_vol) {
                PasteStrategy::Rename => match rename_item(&source, &dest) {
                    Ok(()) => {
                        self.done += 1;
                        StepOutcome::Working
                    }
                    Err(reason) => StepOutcome::Failed(reason),
                },
                PasteStrategy::Copy => self.start_copy(source, dest, kind, None),
                PasteStrategy::CopyThenDelete => {
                    self.start_copy(source.clone(), dest, kind, Some((source, is_directory)))
                }
            }
        }

        /// Start copying one item's tree, installing the [`Copying`] stage.
        fn start_copy(
            &mut self,
            source: alloc::vec::Vec<String>,
            dest: alloc::vec::Vec<String>,
            kind: CopyKind,
            then_delete: Option<(alloc::vec::Vec<String>, bool)>,
        ) -> StepOutcome {
            match CopyWalk::from_items(alloc::vec![(source, dest, kind)]) {
                Some(walk) => {
                    self.stage = PasteStage::Copying {
                        walk,
                        transfer: None,
                        then_delete,
                    };
                    StepOutcome::Working
                }
                None => StepOutcome::Failed("nothing to copy"),
            }
        }

        /// Advance the current item's tree copy by one unit: carry one chunk of
        /// an in-flight leaf file, or take the next [`CopyWalk`] step (create a
        /// directory, list one, or open the next file). When the tree is
        /// complete, either begin the cross-volume source removal or fall back
        /// to idle for the next item.
        fn step_copy(
            &mut self,
            mut walk: CopyWalk,
            transfer: Option<Transfer>,
            then_delete: Option<(alloc::vec::Vec<String>, bool)>,
        ) -> StepOutcome {
            if let Some(mut transfer) = transfer {
                return match transfer.step(&mut self.buf) {
                    Ok(true) => {
                        if walk.copied_file().is_err() {
                            return StepOutcome::Failed("internal copy step error");
                        }
                        self.done += 1;
                        self.stage = PasteStage::Copying {
                            walk,
                            transfer: None,
                            then_delete,
                        };
                        StepOutcome::Working
                    }
                    Ok(false) => {
                        self.stage = PasteStage::Copying {
                            walk,
                            transfer: Some(transfer),
                            then_delete,
                        };
                        StepOutcome::Working
                    }
                    Err(reason) => StepOutcome::Failed(reason),
                };
            }
            // Copy the current step out so the walk is free to be mutated.
            let next = match walk.next_action() {
                None => None,
                Some(CopyAction::MakeDir { dest }) => Some(CopyStep::MakeDir(dest.to_vec())),
                Some(CopyAction::List { source }) => Some(CopyStep::List(source.to_vec())),
                Some(CopyAction::CopyFile { source, dest }) => {
                    Some(CopyStep::CopyFile(source.to_vec(), dest.to_vec()))
                }
                Some(CopyAction::CopyLink { source, dest }) => {
                    Some(CopyStep::CopyLink(source.to_vec(), dest.to_vec()))
                }
            };
            let Some(step) = next else {
                // The tree is fully copied. A cross-volume move now removes the
                // source through the shared delete walk; a plain copy is done
                // with this item.
                if let Some((source, is_directory)) = then_delete {
                    let Some(plan) = DeletePlan::new(alloc::vec![(source, is_directory)]) else {
                        return StepOutcome::Failed("a moved source could not be removed");
                    };
                    self.stage = PasteStage::Deleting(DeleteWalk::from_plan(&plan));
                }
                return StepOutcome::Working;
            };
            match step {
                CopyStep::MakeDir(dest) => {
                    let spelled = match spell_path(&dest) {
                        Ok(spelled) => spelled,
                        Err(reason) => return StepOutcome::Failed(reason),
                    };
                    if tairix_rt::fs_mkdir(spelled.as_bytes()) != 0 {
                        return StepOutcome::Failed("a destination folder could not be created");
                    }
                    if walk.created().is_err() {
                        return StepOutcome::Failed("internal copy step error");
                    }
                    self.done += 1;
                    self.stage = PasteStage::Copying {
                        walk,
                        transfer: None,
                        then_delete,
                    };
                    StepOutcome::Working
                }
                CopyStep::List(source) => {
                    let Ok(children) = copy_children(&source) else {
                        return StepOutcome::Failed("a folder could not be read");
                    };
                    if walk.expand(&children).is_err() {
                        return StepOutcome::Failed("a folder is nested too deep");
                    }
                    self.stage = PasteStage::Copying {
                        walk,
                        transfer: None,
                        then_delete,
                    };
                    StepOutcome::Working
                }
                CopyStep::CopyFile(source, dest) => match Transfer::open(&source, &dest) {
                    Ok(transfer) => {
                        self.stage = PasteStage::Copying {
                            walk,
                            transfer: Some(transfer),
                            then_delete,
                        };
                        StepOutcome::Working
                    }
                    Err(reason) => StepOutcome::Failed(reason),
                },
                CopyStep::CopyLink(source, dest) => {
                    self.step_copy_link(walk, &source, &dest, then_delete)
                }
            }
        }

        /// Recreate one symbolic link at its destination and advance the walk
        /// past it.
        ///
        /// A link is copied by being *recreated* with the target it stores:
        /// streaming its bytes would leave a regular file holding the
        /// target's text, and following it would copy something the link only
        /// points at.
        fn step_copy_link(
            &mut self,
            mut walk: CopyWalk,
            source: &[String],
            dest: &[String],
            then_delete: Option<(alloc::vec::Vec<String>, bool)>,
        ) -> StepOutcome {
            if let Err(reason) = recreate_link(source, dest) {
                return StepOutcome::Failed(reason);
            }
            if walk.copied_link().is_err() {
                return StepOutcome::Failed("internal copy step error");
            }
            self.done += 1;
            self.stage = PasteStage::Copying {
                walk,
                transfer: None,
                then_delete,
            };
            StepOutcome::Working
        }

        /// Advance a cross-volume move's source removal by one step, over the
        /// shared delete walk. These removals are cleanup, so they are not
        /// counted toward the copied figure.
        fn step_delete(&mut self, mut walk: DeleteWalk) -> StepOutcome {
            // Copy the current step out so the walk is free to be mutated.
            let step = walk.next_action().map(|action| match action {
                DeleteAction::List(path) => (true, path.to_vec(), false),
                DeleteAction::Remove { path, is_directory } => (false, path.to_vec(), is_directory),
            });
            let Some((is_list, path, is_directory)) = step else {
                // The source is gone; this item is done.
                return StepOutcome::Working;
            };
            if is_list {
                let Ok(children) = removal_children(&path) else {
                    return StepOutcome::Failed("a moved source could not be removed");
                };
                if walk.expand(&children).is_err() {
                    return StepOutcome::Failed("a folder is nested too deep");
                }
            } else {
                let spelled = match spell_path(&path) {
                    Ok(spelled) => spelled,
                    Err(reason) => return StepOutcome::Failed(reason),
                };
                let flags = if is_directory {
                    UnlinkFlags::DIRECTORY
                } else {
                    UnlinkFlags::empty()
                };
                if tairix_rt::fs_unlink(spelled.as_bytes(), flags) != 0 {
                    return StepOutcome::Failed("a moved source could not be removed");
                }
                if walk.complete_removal().is_err() {
                    return StepOutcome::Failed("internal delete step error");
                }
            }
            self.stage = PasteStage::Deleting(walk);
            StepOutcome::Working
        }
    }

    /// Advance a running `paste` by up to [`OPERATION_STEP_BUDGET`] bounded
    /// units of work, returning `true` once it has finished — completed,
    /// cancelled at a step boundary, or stopped fail-closed on a refusal.
    ///
    /// A unit is one rename, one `fs_mkdir`, one directory listing, one copy
    /// chunk, or one source-removal step, so a single large file cannot block
    /// the event loop (each chunk is bounded). A latched cancel stops at the
    /// next unit boundary (never mid-chunk); the first refusal states its
    /// reason on `stderr` naming the item (fail loud) and leaves whatever
    /// already landed in place. The honest copied count is updated from the
    /// paste's own figure after each unit.
    fn advance_paste(paste: &mut Paste, progress: &mut ProgressModel) -> bool {
        for _ in 0..OPERATION_STEP_BUDGET {
            if progress.is_cancel_requested() {
                return true;
            }
            match paste.step() {
                StepOutcome::Working => {}
                StepOutcome::Done => return true,
                StepOutcome::Failed(reason) => {
                    report_paste_item_error(paste.current_source(), reason);
                    return true;
                }
            }
            progress.set_done(paste.done());
        }
        false
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

    /// Stat the node `path` **names**, without following a final symbolic
    /// link — the POSIX `lstat` reading.
    ///
    /// This is the question every verb that acts on a *name* must ask: a link
    /// is what gets copied, moved, or trashed, so its own kind and its own
    /// volume are what decide how. [`stat_node`] answers the other question —
    /// what the path leads to — which is what a *destination folder* is.
    fn stat_name(path: &[String]) -> Option<tairix_abi::fs::FileStat> {
        let spelled = tairix_browse::vfs::absolute_path(path).ok()?;
        tairix_rt::File::open(spelled.as_bytes(), OpenFlags::NO_FOLLOW)
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
    /// silent failure or a fabricated success. Carries no path prefix beyond
    /// the leaf name the user already sees.
    fn report_paste_item_error(source: &[String], reason: &str) {
        let name = source.last().map_or("", String::as_str);
        let _ = writeln!(Stderr, "files: could not paste {name}: {reason}");
    }

    /// Route one primary-button press at window-local `point` in navigation
    /// mode, reporting whether the view changed (and must re-present).
    ///
    /// The press is resolved in the order the layers overlap on screen, each
    /// action the spelling of one user intent, never an escalation:
    ///
    /// * a **manager write tool** (New Folder, Go to Trash, Empty Trash) is
    ///   dispatched here because the write path needs the overlay state; its
    ///   enable state gates the hit-test, so a click on a disabled Empty Trash
    ///   resolves to nothing (fail closed);
    /// * an **item** is activated on a quick second press on that same item —
    ///   descend / launch a bundle / open a file, through the very same
    ///   [`activate`] dispatch a keyboard `Enter` drives, so pointer and
    ///   keyboard can never diverge — and merely selected on a first or lone
    ///   press. Held shift lists a bundle instead of running it, the pointer
    ///   spelling of `Shift+Enter`. The monotonic [`tairix_rt::clock_get`]
    ///   needs no capability, and the pairing rule lives once in the shared
    ///   engine ([`DoubleClickTracker`]);
    /// * anything else lands on the read-only **chrome** and routes through
    ///   [`apply_chrome_press`].
    ///
    /// A tool or chrome press is not an item, so it resets the double-click
    /// tracker: a click *through* the chrome and back onto the same item is
    /// never mistaken for a double-click of that item.
    ///
    /// `viewport` is the rail-inset content area the items occupy; the write
    /// tools sit on the toolbar band, which spans the whole window
    /// (`canvas.window()`).
    #[allow(clippy::too_many_arguments)] // The press, its context, and the round's report.
    fn apply_primary_press<S: DirectorySource>(
        browser: &mut Browser<S>,
        overlays: &mut Overlays,
        launcher: &RefCell<Launcher>,
        canvas: Canvas<'_>,
        viewport: Rect,
        point: Point,
        modifiers: AbiModifiers,
        damage: &mut Region,
    ) -> (Repaint, bool) {
        let toolbar = canvas.chrome.toolbar;
        let theme = canvas.theme();
        let scale = canvas.scale;
        if let Some(tool) = manager_tool_at(
            browser,
            scale,
            theme,
            canvas.window(),
            toolbar,
            point,
            MANAGER_TOOLS,
            manager_tool_model(browser),
        ) {
            overlays.double_click.reset();
            return whole(apply_manager_tool(
                browser, overlays, scale, theme, viewport, toolbar, tool,
            ));
        }
        let hit =
            tairix_browse::render::entry_index_at(browser, scale, theme, viewport, toolbar, point);
        match gesture::primary_press(&mut overlays.double_click, tairix_rt::clock_get(), hit) {
            PrimaryPress::Activate { index } => {
                let _ = browser.select(index);
                whole(activate(
                    browser,
                    launcher,
                    scale,
                    theme,
                    viewport,
                    toolbar,
                    bundle_intent(modifiers.shift),
                    AfterHandoff::Keep,
                ))
            }
            // Selecting moves the highlight between two entries and nothing
            // else, so the round reports exactly those two.
            PrimaryPress::Select { index } => {
                let mark = ViewMark::of(browser);
                let selected = browser.select(index).is_ok();
                let moved = mark.report(browser, scale, theme, viewport, toolbar, damage);
                (reported_if(selected && moved), false)
            }
            PrimaryPress::Chrome => whole(apply_chrome_press(browser, canvas, viewport, point)),
        }
    }

    /// Apply one primary press that landed on the read-only **chrome** (the
    /// toolbar), reporting whether the view changed.
    ///
    /// The caller ([`apply_primary_press`]) resolves manager write tools and
    /// item clicks first, so by the time a press reaches here it is neither. A
    /// click on a toolbar command runs it through the same shared dispatch the
    /// keyboard accelerators use; a click on empty space resolves to nothing
    /// and repaints nothing.
    ///
    /// The toolbar band spans the whole window (`canvas.window()`), so it is
    /// hit-tested against that; `viewport` is the rail-inset content area the
    /// command it runs acts within.
    fn apply_chrome_press<S: DirectorySource>(
        browser: &mut Browser<S>,
        canvas: Canvas<'_>,
        viewport: Rect,
        point: Point,
    ) -> (bool, bool) {
        let toolbar = canvas.chrome.toolbar;
        let theme = canvas.theme();
        let scale = canvas.scale;
        // A click on a toolbar command runs it through the same shared dispatch
        // the keyboard accelerators use; a disabled command resolves to nothing
        // (`toolbar_command_at` fails closed) and repaints nothing.
        if let Some(command) = tairix_browse::render::toolbar_command_at(
            browser,
            scale,
            theme,
            canvas.window(),
            toolbar,
            point,
        ) {
            return apply_toolbar_command(browser, scale, theme, viewport, toolbar, command);
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
        Some(pointer_point(x, y))
    }

    /// Ask the desktop to open this window's context menu at window-local
    /// `point`, on the item `index` the caller's hit-test resolved (`None` for
    /// empty space or the chrome).
    ///
    /// The item is selected first so the menu's commands act on what was
    /// clicked; a right-click on nothing clears the selection so the menu
    /// offers only the directory-scoped Paste. The rows are the shared
    /// [`context_menu`] declaration over the [`ContextMenuModel`] with this
    /// app's own held-clipboard state, so an inapplicable command is declared
    /// disabled with its reason rather than left out.
    ///
    /// The anchor is the client-local point the press was reported at, which is
    /// the only space this window can speak truthfully: it is never told where
    /// it sits on screen. The desktop titles, places, draws, grabs and
    /// dismisses; the answer arrives later as one `MenuClosed` naming the id
    /// minted here. A refusal is an answer — it is reported and the window
    /// carries on with no menu, never drawing one of its own.
    ///
    /// The press also breaks any half-finished primary pair, so a click either
    /// side of a right-click cannot read as a double-click.
    fn open_context_menu<S: DirectorySource>(
        browser: &mut Browser<S>,
        overlays: &mut Overlays,
        menu: &mut MenuLink<'_>,
        point: Point,
        index: Option<usize>,
    ) -> (bool, bool) {
        overlays.double_click.reset();
        match index {
            Some(index) => {
                let _ = browser.select(index);
            }
            None => browser.clear_selection(),
        }
        let model = ContextMenuModel::for_browser(browser, overlays.clipboard.is_some());
        let rows = match context_menu(model, APP_NAME) {
            Ok(rows) => rows,
            Err(err) => {
                report_error(&alloc::format!("menu model refused ({err}); not shown"));
                return (true, false);
            }
        };
        let anchor = match MenuAnchor::new(point.x, point.y, 0, 0) {
            Ok(anchor) => anchor,
            Err(err) => {
                report_error(&alloc::format!("menu anchor refused ({err}); not shown"));
                return (true, false);
            }
        };
        match menu.client.open_menu(menu.window, anchor, &rows) {
            Ok(open_id) => *menu.open = Some(open_id),
            Err(err) => report_error(&alloc::format!("menu refused ({err}); not shown")),
        }
        // The selection moved whether or not a chain came up, so the listing is
        // repainted either way.
        (true, false)
    }

    /// Act on the one outcome the desktop owes this window's open.
    ///
    /// A chosen row runs its command's verb; a dismissal does nothing; a
    /// refusal is stated on `stderr` and the window carries on. A row id this
    /// window never declared names no command and is dropped (fail closed —
    /// an outcome is never guessed at).
    #[allow(clippy::too_many_arguments)] // The window's state, its geometry, and the outcome.
    fn apply_menu_outcome<S: DirectorySource>(
        browser: &mut Browser<S>,
        overlays: &mut Overlays,
        launcher: &RefCell<Launcher>,
        reads: &Reads,
        scale: Scale,
        theme: &Theme,
        viewport: Rect,
        toolbar: ToolbarBand,
        outcome: MenuOutcome,
    ) -> (bool, bool) {
        match outcome {
            MenuOutcome::Chosen(item) => match context_command_from_item(item) {
                Some(command) => dispatch_context_command(
                    browser, overlays, launcher, reads, scale, theme, viewport, toolbar, command,
                ),
                None => (false, false),
            },
            MenuOutcome::Dismissed => (false, false),
            MenuOutcome::Refused(reason) => {
                report_error(&alloc::format!("the desktop showed no menu ({reason:?})"));
                (false, false)
            }
        }
    }

    /// Run the verb a chosen [`ContextCommand`] names, over the exact same app
    /// paths the toolbar and keyboard drive, so the right-click menu can never
    /// diverge from them. Every verb is the user's own permission- checked
    /// action under their identity — the menu adds no authority.
    #[allow(clippy::too_many_arguments)] // The window's state, its geometry, and the command.
    fn dispatch_context_command<S: DirectorySource>(
        browser: &mut Browser<S>,
        overlays: &mut Overlays,
        launcher: &RefCell<Launcher>,
        reads: &Reads,
        scale: Scale,
        theme: &Theme,
        viewport: Rect,
        toolbar: ToolbarBand,
        command: ContextCommand,
    ) -> (bool, bool) {
        match command {
            ContextCommand::Open => activate(
                browser,
                launcher,
                scale,
                theme,
                viewport,
                toolbar,
                BundleIntent::Launch,
                AfterHandoff::Keep,
            ),
            // Opening and closing is the same activation, with the window
            // closed behind it once the entry has been handed over.
            ContextCommand::OpenAndClose => activate(
                browser,
                launcher,
                scale,
                theme,
                viewport,
                toolbar,
                BundleIntent::Launch,
                AfterHandoff::CloseWindow,
            ),
            ContextCommand::OpenWith => begin_open_with(browser, overlays, reads),
            ContextCommand::Rename => begin_rename(
                browser,
                &mut overlays.rename,
                scale,
                theme,
                viewport,
                toolbar,
            ),
            ContextCommand::Cut => apply_clipboard_verb(
                browser,
                &mut overlays.clipboard,
                &mut overlays.operation,
                ClipboardVerb::Cut,
            ),
            ContextCommand::Copy => apply_clipboard_verb(
                browser,
                &mut overlays.clipboard,
                &mut overlays.operation,
                ClipboardVerb::Copy,
            ),
            ContextCommand::Paste => apply_clipboard_verb(
                browser,
                &mut overlays.clipboard,
                &mut overlays.operation,
                ClipboardVerb::Paste,
            ),
            ContextCommand::Properties => begin_properties(browser, &mut overlays.properties),
            // The same modal-confirmed removal the `Delete` key opens: the menu
            // adds no authority — the confirmed walk is the user's own
            // permission-checked `fs_unlink`s.
            ContextCommand::Delete => begin_delete(browser, &mut overlays.delete),
        }
    }

    /// Open the "Open With…" application chooser for the selected regular file.
    ///
    /// The chooser is offered only for a regular file — a directory descends
    /// and a bundle launches itself, so neither has an application to pick (the
    /// context-menu model already disables the command otherwise; this guards
    /// it again, fail closed). The candidate applications are the installed
    /// bundles whose declared associations claim the file's type
    /// ([`RtBundleSource`] + [`applications_for`], keyed off the leaf name,
    /// never a hard-coded viewer). Enumerating them is a read of three program
    /// stores, so it happens *here* — when the user asks — rather than on every
    /// right-click, which is why the candidates are a chooser of this window's
    /// own and not rows of the desktop's menu.
    ///
    /// When no installed application claims the file the refusal is stated
    /// fail-loud on `stderr` and nothing is opened — an honest answer, never an
    /// empty chooser or a fabricated open. The chooser itself launches nothing:
    /// a chosen row runs the same capability-checked hand-off the default open
    /// uses ([`apply_open_with_event`]).
    fn begin_open_with<S: DirectorySource>(
        browser: &Browser<S>,
        overlays: &mut Overlays,
        reads: &Reads,
    ) -> (bool, bool) {
        let Some(entry) = browser.selected_entry() else {
            return (false, false);
        };
        if entry.kind().resolved() != Some(EntryKind::File) {
            return (false, false);
        }
        let name = entry.name().to_string();
        // The selection must still name a valid absolute path (the same
        // spelling every open/stat uses); a name that cannot be spelled is a
        // stated refusal, not a fabricated open.
        let Some(Ok(file_path)) = browser.selected_target_path() else {
            report_error(&alloc::format!("could not locate {name}"));
            return (false, false);
        };
        // Ask for the installed bundles rather than reading them here: three
        // program stores and one manifest per application is not a frame's
        // worth of work. The chooser opens when the scan lands, or at once when
        // the machine granted no reader to do it elsewhere.
        overlays.pending_open_with = Some(PendingChooser {
            path: file_path,
            name,
        });
        match reads.want_bundles() {
            Some(installed) => (settle_open_with(overlays, &installed), false),
            None => (false, false),
        }
    }

    /// Open the chooser the last "Open With…" asked for, now that the
    /// installed bundles are known, answering whether the window changed.
    ///
    /// A file with no installed application that claims its type is an honest
    /// refusal stated on `stderr` and opens nothing — never an empty chooser.
    /// A pending request that has since been dismissed (the user pressed
    /// Escape, or asked about something else) opens nothing either.
    fn settle_open_with(overlays: &mut Overlays, installed: &[AppAssociation]) -> bool {
        let Some(pending) = overlays.pending_open_with.take() else {
            return false;
        };
        let apps = applications_for(&pending.name, installed);
        let Some(chooser) = OpenWithChooser::new(&apps, &pending.path, &pending.name) else {
            report_error(&alloc::format!("no application to open {}", pending.name));
            return false;
        };
        overlays.open_with = Some(chooser);
        true
    }

    /// Handle one event while the "Open With…" chooser owns the window.
    ///
    /// `Escape` dismisses it. Up/Down/Home/End move the current candidate and
    /// scroll the least that keeps it in view; `Enter` hands the file to it. A
    /// primary press on a candidate row does the same, the scroll gutter owns a
    /// press that lands on it, and a press anywhere else dismisses the chooser
    /// (fail closed — a press off the rows never launches). A wheel scrolls the
    /// list. Every other event leaves the chooser open.
    ///
    /// The hand-off is the same [`Launcher::launch_viewer`] the default open
    /// uses: the file opened read-only in the manager's own table and wired
    /// onto the child's `STDIN`, so the application reads it with no filesystem
    /// capability of its own.
    fn apply_open_with_event(
        overlays: &mut Overlays,
        launcher: &RefCell<Launcher>,
        scale: Scale,
        theme: &Theme,
        viewport: Rect,
        event: &WindowEvent,
        damage: &mut Region,
    ) -> (bool, bool) {
        let visible = open_with_visible_rows(viewport, scale, theme);
        match event {
            WindowEvent::Key {
                key: KeyInput::Pressed { key, .. },
                ..
            } => {
                let Some(chooser) = overlays.open_with.as_mut() else {
                    return (false, false);
                };
                match key {
                    KeyValue::Named(NamedKeyCode::Escape) => {
                        overlays.open_with = None;
                        (true, false)
                    }
                    KeyValue::Named(NamedKeyCode::Enter) => {
                        launch_open_with(overlays, launcher);
                        (true, false)
                    }
                    KeyValue::Named(NamedKeyCode::Up) => {
                        let moved = chooser.step(-1);
                        (chooser.reveal(visible) || moved, false)
                    }
                    KeyValue::Named(NamedKeyCode::Down) => {
                        let moved = chooser.step(1);
                        (chooser.reveal(visible) || moved, false)
                    }
                    KeyValue::Named(NamedKeyCode::Home) => {
                        let moved = chooser.select(0);
                        (chooser.reveal(visible) || moved, false)
                    }
                    KeyValue::Named(NamedKeyCode::End) => {
                        let moved = chooser.select(usize::MAX);
                        (chooser.reveal(visible) || moved, false)
                    }
                    _ => (false, false),
                }
            }
            WindowEvent::Scrolled { dy, .. } => {
                let Some(chooser) = overlays.open_with.as_mut() else {
                    return (false, false);
                };
                (chooser.scroll_by(i64::from(*dy), visible), false)
            }
            WindowEvent::Pointer { x, y, action, .. } => {
                let point = pointer_point(*x, *y);
                // The gutter owns a press that lands on it, so dragging the
                // thumb scrolls the list instead of dismissing the chooser.
                if let Some(chooser) = overlays.open_with.as_mut() {
                    let mut scrolled = None;
                    for input in pointer_input_events(*action, point) {
                        if let Some(repaint) = open_with_scroll_pointer(
                            chooser, scale, theme, viewport, point, &input, damage,
                        ) {
                            scrolled = Some(scrolled.unwrap_or(false) || repaint);
                        }
                    }
                    if let Some(repaint) = scrolled {
                        return (repaint, false);
                    }
                }
                let Some(point) = press_point(*action, *x, *y) else {
                    return (false, false);
                };
                let Some(chooser) = overlays.open_with.as_mut() else {
                    return (false, false);
                };
                match open_with_row_at(chooser, viewport, scale, theme, point) {
                    Some(index) => {
                        chooser.select(index);
                        launch_open_with(overlays, launcher);
                    }
                    // A press off the rows closes the chooser and launches
                    // nothing.
                    None => overlays.open_with = None,
                }
                (true, false)
            }
            _ => (false, false),
        }
    }

    /// Hand the chooser's file to its current candidate and close the chooser.
    ///
    /// The chooser is taken out first, so the launch runs from owned state and
    /// a second activation cannot reach a chooser that is already gone.
    fn launch_open_with(overlays: &mut Overlays, launcher: &RefCell<Launcher>) {
        let Some(chooser) = overlays.open_with.take() else {
            return;
        };
        if let Some(candidate) = chooser.chosen() {
            launcher.borrow_mut().launch_viewer(
                candidate.bundle_path(),
                chooser.file_path(),
                chooser.display_name(),
            );
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
        scale: Scale,
        theme: &Theme,
        viewport: Rect,
        toolbar: ToolbarBand,
        command: ToolbarCommand,
    ) -> (bool, bool) {
        match tairix_browse::apply_command(browser, command) {
            Ok(true) => {
                tairix_browse::render::reveal_selection(browser, scale, theme, viewport, toolbar);
                (true, false)
            }
            Ok(false) | Err(_) => (false, false),
        }
    }

    /// Dispatch a manager write `tool` (a toolbar click). New Folder creates
    /// and inline-renames a folder; the Trash tool navigates to the user's
    /// Trash location; Empty Trash opens the permanent-removal confirmation
    /// (`plans/NEW-FILEMANAGER.md` `FM11`).
    fn apply_manager_tool<S: DirectorySource>(
        browser: &mut Browser<S>,
        overlays: &mut Overlays,
        scale: Scale,
        theme: &Theme,
        viewport: Rect,
        toolbar: ToolbarBand,
        tool: ManagerTool,
    ) -> (bool, bool) {
        match tool {
            ManagerTool::NewFolder => begin_new_folder(
                browser,
                &mut overlays.rename,
                scale,
                theme,
                viewport,
                toolbar,
            ),
            ManagerTool::Trash => go_to_trash(browser, scale, theme, viewport, toolbar),
            ManagerTool::EmptyTrash => begin_empty_trash(browser, &mut overlays.delete),
        }
    }

    /// The manager write-tools' enable state for `browser`: the Empty Trash
    /// verb ([`ManagerTool::EmptyTrash`]) is offered only when the current
    /// directory *is* the user's Trash and it is non-empty. Computed here
    /// rather than in the shared engine because it depends on the user's
    /// `HOME`, which the engine does not know (`plans/NEW-FILEMANAGER.md`
    /// `FM11`).
    fn manager_tool_model<S: DirectorySource>(browser: &Browser<S>) -> ManagerToolModel {
        ManagerToolModel::new(current_is_populated_trash(browser))
    }

    /// Whether the current directory is the user's Trash *and* it holds at
    /// least one item — the one gate on offering the Empty Trash verb. Fail
    /// closed: an absent/root `HOME`, or a current directory that is not the
    /// `Library/Trash` subtree, is not the Trash, so the verb is not offered.
    fn current_is_populated_trash<S: DirectorySource>(browser: &Browser<S>) -> bool {
        if browser.entries().is_empty() {
            return false;
        }
        let Some(home) = home_components() else {
            return false;
        };
        if home.is_empty() {
            return false;
        }
        browser.components() == trash_dir(&home).as_slice()
    }

    /// Navigate the browser to the user's Trash — the navigable Trash location
    /// (`plans/NEW-FILEMANAGER.md` `FM11`). Resolves the user's home from the
    /// exported `HOME`, ensures the fixed `Library/Trash` subtree exists (the
    /// user's own idempotent `fs_mkdir`), and lists it. Going to the Trash is
    /// an incidental, refusable action: an absent home or an unavailable Trash
    /// is stated on `stderr` and changes nothing — never a crash or a
    /// fabricated view.
    fn go_to_trash<S: DirectorySource>(
        browser: &mut Browser<S>,
        scale: Scale,
        theme: &Theme,
        viewport: Rect,
        toolbar: ToolbarBand,
    ) -> (bool, bool) {
        let Some(home) = home_components() else {
            io::write_stderr_line("files: no home directory, so no Trash");
            return (false, false);
        };
        if home.is_empty() {
            io::write_stderr_line("files: no home directory, so no Trash");
            return (false, false);
        }
        let trash = trash_dir(&home);
        if !ensure_trash_dir(&trash) {
            io::write_stderr_line("files: the Trash folder is unavailable");
            return (false, false);
        }
        match browser.navigate_to(trash) {
            Ok(true) => {
                tairix_browse::render::reveal_selection(browser, scale, theme, viewport, toolbar);
                (true, false)
            }
            // Already in the Trash, or (fail closed) it could not be listed.
            Ok(false) => (false, false),
            Err(_) => {
                io::write_stderr_line("files: the Trash folder could not be opened");
                (false, false)
            }
        }
    }

    /// Open the confirmation to **empty** the user's Trash — permanently remove
    /// its contents (`plans/NEW-FILEMANAGER.md` `FM11`).
    ///
    /// Recomputes the Trash location and re-reads its contents now, so a stale
    /// click can never empty the wrong directory (fail closed): if the current
    /// directory is not the user's Trash, or its listing cannot be read, or it
    /// is already empty, the verb is simply not offered (a silent no-op or a
    /// stated refusal, never a crash). Emptying is always permanent (there is
    /// no trash-of-the-trash), so the plan is confirmed with the
    /// [`DeleteDisposition::Permanent`] wording and carried out — on confirm —
    /// by the same interleaved [`DeleteWalk`] runner an ordinary permanent
    /// delete uses, under the user's own `fs_readdir`/`fs_unlink` (no new
    /// capability, no ambient authority).
    fn begin_empty_trash<S: DirectorySource>(
        browser: &Browser<S>,
        delete: &mut Option<DeleteConfirm>,
    ) -> (bool, bool) {
        let Some(home) = home_components() else {
            return (false, false);
        };
        if home.is_empty() {
            return (false, false);
        }
        let trash = trash_dir(&home);
        // Only the Trash may be emptied; a click anywhere else is a no-op.
        if browser.components() != trash.as_slice() {
            return (false, false);
        }
        let Ok(children) = removal_children(&trash) else {
            io::write_stderr_line("files: could not read the Trash folder");
            return (false, false);
        };
        match empty_trash_plan(&trash, &children) {
            // The plan removes the Trash's *contents* (never the Trash folder
            // itself), always permanently, so it is confirmed as irreversible.
            Ok(Some(plan)) => {
                let dialog = build_delete_dialog(&plan, DeleteDisposition::Permanent);
                *delete = Some(DeleteConfirm {
                    dialog,
                    plan,
                    trash_moves: None,
                });
                (true, false)
            }
            // An already-empty Trash: nothing to empty, so the verb just does
            // nothing (never an error).
            Ok(None) => (false, false),
            Err(err) => {
                let msg = err.message();
                let _ = writeln!(Stderr, "files: {msg}");
                (false, false)
            }
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
        scale: Scale,
        theme: &Theme,
        viewport: Rect,
        toolbar: ToolbarBand,
    ) -> (bool, bool) {
        let name = suggest_new_dir_name(browser.entries());
        match browser.create_directory(&name, |path| {
            let ret = tairix_rt::fs_mkdir(path.as_bytes());
            if ret == 0 {
                Ok(())
            } else {
                Err(Errno::from_syscall(ret))
            }
        }) {
            Ok(()) => begin_rename(browser, rename, scale, theme, viewport, toolbar),
            Err(err) => {
                let msg = err.message();
                let _ = writeln!(Stderr, "files: {msg}");
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
        scale: Scale,
        theme: &Theme,
        viewport: Rect,
        toolbar: ToolbarBand,
    ) -> (bool, bool) {
        let Some(name) = browser.selected_name().map(ToString::to_string) else {
            return (false, false);
        };
        tairix_browse::render::reveal_selection(browser, scale, theme, viewport, toolbar);
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
    /// stays closed — an answer, not a crash, and never a fabricated summary. A
    /// directory or sealed bundle is opened with the directory flag, a regular
    /// file read-only; `stat` needs only a live handle either way.
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
            io::write_stderr_line("files: properties unavailable for that item");
            (false, false)
        }
    }

    /// Read the selected item's metadata into a display-ready [`Properties`]:
    /// name its path through the shared spelling and `fs_stat` it with one
    /// capability-checked handle under the user's own identity (no new
    /// capability). Returns `None` when there is no selection, the item can no
    /// longer be named, or its metadata cannot be read — the caller decides
    /// whether that is a silent no-op or a stated refusal. A directory or
    /// sealed bundle is opened with the directory flag, a regular file
    /// read-only; `stat` needs only a live handle either way.
    ///
    /// One definition shared by opening the overlay ([`begin_properties`]) and
    /// refreshing it after a permission change, so the two cannot drift.
    fn stat_selected_properties<S: DirectorySource>(browser: &Browser<S>) -> Option<Properties> {
        let entry = browser.selected_entry()?;
        let kind = entry.kind();
        let name = entry.name().to_string();
        let Some(Ok(path)) = browser.selected_target_path() else {
            return None;
        };
        let flags = match kind {
            // A link is described as *itself*, through a resolve-only
            // `NO_FOLLOW` handle: that is the only reading under which a link
            // to a file can be described at all, and it is what makes the
            // permission string honestly read `l` rather than the target's
            // kind. `stat` needs only a live handle, not an access bit.
            EntryKind::Link(_) => OpenFlags::NO_FOLLOW,
            EntryKind::File => OpenFlags::READ,
            EntryKind::Directory | EntryKind::Bundle => OpenFlags::DIRECTORY,
        };
        let stat = tairix_rt::File::open(path.as_bytes(), flags)
            .and_then(|file| file.stat())
            .ok()?;
        let properties = Properties::from_stat(name, kind, &stat);
        // A link also shows where it points — the spelling it stores, which
        // is what explains a broken one.
        Some(match entry.target() {
            Some(target) => properties.with_target(target),
            None => properties,
        })
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
    /// on `stderr` — an honest answer, never a crash or a fabricated success.
    fn apply_permission_toggle<S: DirectorySource>(
        browser: &mut Browser<S>,
        properties: &mut Option<Properties>,
        scale: Scale,
        theme: &Theme,
        viewport: Rect,
        point: Point,
    ) -> (bool, bool) {
        let Some(bit) = permission_cell_at(viewport, scale, theme, point) else {
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
                Err(Errno::from_syscall(ret))
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
                let msg = err.message();
                let _ = writeln!(Stderr, "files: {msg}");
                (false, false)
            }
        }
    }

    /// Route a primary-button press inside the open Properties overlay: a press
    /// on one of the nine permission toggles commits a mode change
    /// (`apply_permission_toggle`); otherwise, where the user holds
    /// `CAP_FS_CHOWN`, a press on the owner or group value opens the inline id
    /// editor for that field. A press elsewhere changes nothing (fail closed).
    ///
    /// The permission and owner cells sit on different rows, so a press
    /// resolves to at most one of them; the permission row is checked first so
    /// its (capability-free) toggles are never shadowed by the owner control.
    fn apply_properties_pointer<S: DirectorySource>(
        browser: &mut Browser<S>,
        overlays: &mut Overlays,
        scale: Scale,
        theme: &Theme,
        viewport: Rect,
        point: Point,
    ) -> (bool, bool) {
        if permission_cell_at(viewport, scale, theme, point).is_some() {
            return apply_permission_toggle(
                browser,
                &mut overlays.properties,
                scale,
                theme,
                viewport,
                point,
            );
        }
        if overlays.can_chown {
            if let Some(props) = overlays.properties.as_ref() {
                if let Some(field) = owner_field_at(props, viewport, scale, theme, point) {
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
        overlays: &mut Overlays,
        key: KeyValue,
        modifiers: AbiModifiers,
        scale: Scale,
        theme: &Theme,
        viewport: Rect,
    ) -> (bool, bool) {
        let (editor_key, mods) = to_editor_key(key, modifiers);
        let Some(ed) = overlays.owner.as_mut() else {
            return (false, false);
        };
        // The rectangle the shared owner control draws this editor at; a row
        // that does not fit is drawn nowhere and so reports nothing.
        let bounds = overlays
            .properties
            .as_ref()
            .and_then(|props| owner_editor_rect(props, viewport, scale, theme, ed.field))
            .unwrap_or(Rect::EMPTY);
        let action = ed
            .editor
            .on_key(editor_key, mods, bounds, &mut damage::sink());
        match action {
            Some(TextAction::Submitted) => {
                commit_owner_edit(browser, &mut overlays.properties, &mut overlays.owner)
            }
            Some(TextAction::Cancelled) => {
                overlays.owner = None;
                (true, false)
            }
            Some(TextAction::Edited) => {
                let text = ed.editor.text().to_string();
                ed.editor.set_message(owner_id_message(&text));
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
    /// answer, never a silent or fabricated result.
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
                Err(Errno::from_syscall(ret))
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
    #[allow(clippy::too_many_arguments)] // The editor, its geometry, and the key.
    fn apply_rename_key<S: DirectorySource>(
        browser: &mut Browser<S>,
        rename: &mut Option<TextField>,
        scale: Scale,
        theme: &Theme,
        viewport: Rect,
        toolbar: ToolbarBand,
        key: KeyValue,
        modifiers: AbiModifiers,
    ) -> (bool, bool) {
        let (editor_key, mods) = to_editor_key(key, modifiers);
        let bounds =
            tairix_browse::render::selection_rect(browser, scale, theme, viewport, toolbar)
                .unwrap_or(Rect::EMPTY);
        let action = match rename.as_mut() {
            Some(field) => field.on_key(editor_key, mods, bounds, &mut damage::sink()),
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
                        Err(Errno::from_syscall(ret))
                    }
                }) {
                    // A committed rename (or a no-op rename to the same name)
                    // closes the editor; the selection follows the entry.
                    Ok(()) | Err(RenameError::Unchanged) => {
                        *rename = None;
                        tairix_browse::render::reveal_selection(
                            browser, scale, theme, viewport, toolbar,
                        );
                        (true, false)
                    }
                    // A refused rename stays open with the honest reason shown
                    // in the field (never a silent or fabricated result); the
                    // listing is untouched.
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
            || tairix_rt::port_bind(
                event_endpoint,
                WindowEvent::WIRE_LEN,
                tairix_window::EVENT_MAILBOX_CAPACITY,
            ) != 0
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
        // The memory-pressure member: the kernel wakes the park when the
        // machine's band changes, so the decoded grid artwork is handed back as
        // memory tightens instead of held until something else is starved. This
        // is the app's only pressure notification — it neither polls nor times
        // the band.
        if !tairix_procinfo::pressure::watch(set, PRESSURE_TOKEN) {
            return Err(fail(EXIT_NO_EVENTS, "memory-pressure wait refused"));
        }
        Ok((event_endpoint, set))
    }

    /// The transient overlay/clipboard state the event loop threads, all
    /// closed at start-up.
    ///
    /// `can_chown` is whether the launching user holds `CAP_FS_CHOWN` — read
    /// once from the kernel-attested self-origin (a refused query fails closed
    /// to "not held") — so the ownership control is offered only where it can
    /// be used.
    fn initial_overlays() -> Overlays {
        Overlays {
            rename: None,
            properties: None,
            owner: None,
            delete: None,
            open_with: None,
            pending_open_with: None,
            operation: None,
            clipboard: None,
            can_chown: tairix_rt::self_origin()
                .is_ok_and(|origin| origin.capabilities().holds_cap(CapabilityId::FS_CHOWN)),
            double_click: DoubleClickTracker::new(),
        }
    }

    /// Render `files`'s own short help (`NAME` + `SYNOPSIS` + compact
    /// `OPTIONS`) from its own bundle's `Help/` tree through the one shared
    /// engine; when no document can be served (a build without the bundle's
    /// documents) the usage banner stands in — the program's own text, not
    /// fabricated help content — so `-h` never fails.
    fn short_help() -> i32 {
        let locale = tairix_rt::env_var(b"LANG").and_then(|raw| core::str::from_utf8(raw).ok());
        let bytes = own_short_help(&BundleHelp::new("files"), locale, "files")
            .unwrap_or_else(|| alloc::format!("{USAGE}\n").into_bytes());
        match Stdout.write_all(&bytes) {
            Ok(()) => 0,
            Err(_) => 1,
        }
    }

    /// State a command line the program cannot act on — the reason, then the
    /// usage banner — and hand back the usage exit code for `main`.
    fn usage_error(reason: &str) -> i32 {
        report_error(reason);
        let _ = writeln!(Stderr, "{USAGE}");
        EXIT_USAGE
    }

    /// The live directory listing seam the browser reads through: one
    /// `read_dir_all` under the launching user's own identity, so every
    /// listing is an ordinary permission-checked read and the app holds no
    /// authority of its own.
    fn list_directory(path: &str) -> Result<alloc::vec::Vec<u8>, Errno> {
        tairix_rt::read_dir_all(path.as_bytes()).map_err(Errno::from_syscall)
    }

    /// The live folder-occupancy probe: open the directory, read at most one
    /// packed record, close it. The browser only asks "is there a first
    /// child?", so this never grows the buffer and never transfers a listing
    /// — a directory of a hundred thousand entries costs what an empty one
    /// does. It runs under the launching user's own identity, so a directory
    /// the user may not read simply refuses.
    fn probe_directory(path: &str, buf: &mut [u8]) -> Result<usize, Errno> {
        let dir = tairix_rt::open_dir(path.as_bytes()).map_err(Errno::from_syscall)?;
        dir.read(buf).map_err(Errno::from_syscall)
    }

    /// A directory source that reads on the calling thread. Named so a fresh
    /// one can be built per attempt: opening consumes its source, so a refused
    /// attempt cannot hand the same one to the next.
    ///
    /// Used only to *find* where to open (below), never by the running window:
    /// every listing a window asks for goes through [`DeferredSource`].
    type LiveSource = VfsDirectorySource<
        fn(&str) -> Result<alloc::vec::Vec<u8>, Errno>,
        RtLinkReader,
        fn(&str, &mut [u8]) -> Result<usize, Errno>,
    >;

    /// One live source over [`list_directory`], the shared production link
    /// reader, and [`probe_directory`].
    fn live_source() -> LiveSource {
        VfsDirectorySource::probing(list_directory, RtLinkReader, probe_directory)
    }

    /// Open the manager's browser at the first location that actually lists,
    /// showing its items as icons.
    ///
    /// The manager presents a desktop file view, so it opens on the icon grid
    /// and the toolbar's view toggle switches to the list; the engine's own
    /// default is the list the read-only picker wants, so the manager states
    /// its choice here — once, for whichever location [`first_listable`]
    /// opened.
    fn open_browser(
        reads: &alloc::sync::Arc<Reads>,
        location: Option<alloc::vec::Vec<String>>,
    ) -> Option<Browser<DeferredSource>> {
        let start = first_listable(location)?;
        // The window's own listings are read on the worker from here on; this
        // first one is asked for the same way and arrives with the first
        // resume, a frame or two later.
        let mut browser =
            Browser::open_at(DeferredSource(alloc::sync::Arc::clone(reads)), start).ok()?;
        browser.set_view_mode(ViewMode::Grid);
        Some(browser)
    }

    /// The first location that actually lists: the one the command line named,
    /// then the launching user's home, then the root view.
    ///
    /// Degrades rather than dies — a location that cannot be listed is stated
    /// on `stderr` and the next one tried, so a caller naming a folder that is
    /// gone, is not a directory, or that this user may not read still gets a
    /// usable window. `None` only when even the root view cannot be listed,
    /// which `main` exits fail-loud on.
    ///
    /// This reads on the calling thread, and is the one read that does: it runs
    /// before any window exists, so there is no frame to owe anyone — and the
    /// answer is *which location to open*, which a deferred source cannot give
    /// (its first answer is always "not yet", so every candidate would look
    /// listable and the ladder would never fall through).
    fn first_listable(
        location: Option<alloc::vec::Vec<String>>,
    ) -> Option<alloc::vec::Vec<String>> {
        if let Some(components) = location {
            match Browser::open_at(live_source(), components.clone()) {
                Ok(browser) => return Some(browser.components().to_vec()),
                Err(_) => report_error(&unlistable_reason(&components)),
            }
        }
        if let Some(home) = home_components() {
            match Browser::open_at(live_source(), home) {
                Ok(browser) => return Some(browser.components().to_vec()),
                Err(_) => report_error("could not list the home directory; opening the root view"),
            }
        }
        Browser::open_root(live_source())
            .ok()
            .map(|browser| browser.components().to_vec())
    }

    /// Program entry point. `tairix-rt`'s `_start` calls it once the
    /// runtime is set up and routes its return value through the `exit`
    /// syscall.
    #[allow(clippy::too_many_lines)] // One linear bring-up plus one event loop; splitting would obscure the flow.
    fn main() -> i32 {
        // --- The sandbox-worker role, before any other argument handling: the
        // grid's icon artwork is untrusted input, so it is decoded by a
        // capability-empty child this same binary is re-entered as with the
        // reserved role argument. That child serves rasterisation requests over
        // its wired standard streams and nothing else — it never becomes the
        // file manager.
        if worker_role() {
            let mut service = ImageRenderService::default();
            return match serve_stdio(&mut service) {
                ServeEnd::Finished => 0,
                ServeEnd::Failed(_) => 1,
            };
        }

        // --- The command line: an optional starting directory, the reserved
        // short-help switches, and nothing else. A location the program cannot
        // accept is stated and recovered from below; a command line it cannot
        // act on at all is refused here.
        let parsed = tairix_rt::args()
            .ok_or(UsageError::NotUtf8)
            .and_then(|arguments| command::parse(&arguments));
        let start = match parsed {
            Ok(Command::Open(start)) => start,
            Ok(Command::Help) => return short_help(),
            Err(err) => return usage_error(&alloc::format!("{err}")),
        };

        if let Some(reason) = &start.refused {
            report_error(reason);
        }

        // --- What the desktop is, before anything is sized or painted, so the
        // first frame is right rather than a guess corrected once the user has
        // seen it. The reply also tells this process who the session is, which
        // is the identity it requires of every event's attested sender — and
        // it does so without a window, which is what lets a component declare
        // its slot and start answering it before it opens one.
        let mut client = WindowClient::new(RtWindowTransport);
        let info = match client.desktop() {
            Ok(info) => info,
            Err(err) => {
                let _ = writeln!(Stderr, "files: desktop query refused: {err}");
                return EXIT_NO_WINDOW;
            }
        };
        let Some(server) = client.session() else {
            return fail(
                EXIT_NO_WINDOW,
                "the desktop session did not identify itself",
            );
        };
        let mut desktop = match Desktop::new(info) {
            Ok(desktop) => desktop,
            Err(err) => {
                let _ = writeln!(Stderr, "files: cannot draw this desktop: {err}");
                return EXIT_NO_WINDOW;
            }
        };

        // --- The event mailbox the app parks on, bound and added to a
        // fresh wait-set (a bring-up refusal exits fail-loud with its code).
        let (event_endpoint, set) = match bind_event_mailbox() {
            Ok(pair) => pair,
            Err(code) => return code,
        };

        let mut themes = ThemeRegistry::with_builtins();
        themes.set_appearance(desktop.appearance());
        let mut theme = themes.active();
        // The places rail: the user's own shortcuts plus whatever is mounted
        // right now, read once here and re-read whenever the user refreshes.
        // This is the process's copy — the one a component's slot menu is
        // declared from; each window takes its own so its focus and hover are
        // its own.
        let mut places = {
            let (home, volumes) = places_source();
            Places::new(&home, &volumes)
        };

        // --- The icon-bar presence, before this process owns any window. A
        // declared presence belongs to the *process*, so declaring it first is
        // what makes the slot carry this menu and this click from the moment
        // it appears; declare it after a window and the session derives a slot
        // from that window meanwhile — one that opens no menu and does nothing
        // when clicked. For a component that is the whole point: its slot is
        // all it has until the user asks for a window.
        declare_app_bar(&mut client, event_endpoint, start.role, &places);

        // --- The grid's icon artwork: the decode cache, the desk a paint
        // resolves through, and the read/rasterise seams the pump runs one
        // decode at a time over — budgeted from one window's frame size and
        // shared by every window this process opens, so one decode serves them
        // all. Shared, too, between the present path (which draws through it),
        // the parked event source (which trims it when the machine reports a
        // different memory-pressure band), and the loop's pump; dropping it
        // releases the retained pixels.
        // The cache-report rows go with this process on every way out.
        // `bind_event_mailbox` already primed the gauge with the band in
        // force now (`tairix_procinfo::pressure::watch`), so the cache never
        // runs on the fail-closed unknown state before the first draw.
        let _cache_report = tairix_rt::cachereport::ReportGuard;

        // --- The reader: every directory listing, folder cue, and program-store
        // walk this app makes, run on a worker so a slow or contended disk
        // cannot stall the window. A pipe the kernel refuses, or a thread it
        // will not grant, leaves those reads on this task — where they used to
        // be, and stated once.
        let reads = alloc::sync::Arc::new(Reads::new(tairix_rt::sync::WorkerWake::create()));
        let reader = if reads.wake.is_armed() {
            spawn_reader(&reads)
        } else {
            report_error("no reader wake pipe; directory listings happen on the event loop");
            None
        };
        if reader.is_none() {
            reads.stop();
        }
        // Declared after the handle, so it runs first: the desks stop, then the
        // handle detaches.
        let _reads_guard = ReadsGuard(alloc::sync::Arc::clone(&reads));
        if let Some(read) = reads.wake.read_end() {
            if tairix_rt::waitset_ctl(
                set,
                WaitSetOp::Add,
                WaitSourceKind::Stream,
                u64::from(read),
                READS_TOKEN,
            ) != 0
            {
                return fail(EXIT_NO_EVENTS, "reader wake wait refused");
            }
        }

        let icons = {
            let (w, h) = desktop.window_size(WIN_WIDTH, WIN_HEIGHT);
            let nominal = mode_for(w, h);
            RefCell::new(open_icons(
                (nominal.stride_bytes as usize) * (nominal.height_px as usize),
            ))
        };

        // --- The windows. An ordinary file manager was started to show a
        // location, so a first window that will not open leaves it nothing to
        // do and it says so; a component opens none until the user asks, and
        // an empty list is a perfectly good state for either to sit in
        // afterwards.
        let mut windows: alloc::vec::Vec<OpenWindow> = alloc::vec::Vec::new();
        if start.role == Role::Window {
            let mut win = match open_window(
                &mut client,
                event_endpoint,
                &desktop,
                &places,
                &reads,
                start.location,
            ) {
                Ok(win) => win,
                // Already stated, with the code naming what refused: an
                // ordinary file manager is its window, so there is nothing
                // left to be.
                Err(code) => return code,
            };
            if present_whole(&mut win, &mut client, theme, &icons, desktop.scale()).is_err() {
                return fail(EXIT_CHANNEL_LOST, "first present refused");
            }
            windows.push(win);
        }

        // --- The launched-bundle bookkeeping: shared between the event
        // source (which reaps an exited bundle on a child-exit wake) and the
        // activation path below (which spawns one), so a launch and its reap
        // agree on the same in-flight set.
        let launcher = RefCell::new(Launcher::new());

        // Whether an icon decode has landed and is waiting to be drawn. The
        // batch is drawn when the desk runs dry rather than after every icon,
        // so a folder's tiles cost one frame instead of one each.
        let mut artwork_landed = false;

        // --- The event loop: serve input, run one icon decode, repaint, and
        // park only when there is nothing of either left. A dead channel ends
        // the app fail-loud; a clean close ends it at zero.
        let mut events = WindowEvents::new(RtEventSource {
            endpoint: event_endpoint,
            set,
            server,
            launcher: &launcher,
            icons: &icons,
            reads: &reads,
        });
        loop {
            // Report what the icon cache holds at the head of the turn: this
            // is the one point every path through the body passes through
            // *before* anything can park (the park is inside `events.wait`
            // below, and the operation branch re-runs a slice without
            // parking at all), so a decode from the previous turn is never
            // left unreported while the app sits waiting. Silent unless a
            // figure actually moved.
            tairix_rt::cachereport::publish_if_due();
            // A running long operation (a recursive delete, or a copy/move
            // paste) owns its own window: drive it a bounded slice at a time,
            // repaint the progress, and drain (non-blocking) a mid-run
            // cancel or a close — never parking while there is genuine work to
            // do, and returning to the parked wait the instant the operation
            // finishes. Another window carries on: the drained event is routed
            // to whichever window it names, so only the operating window is
            // modal.
            if let Some(busy) = windows
                .iter()
                .position(|win| win.overlays.operation.is_some())
            {
                let finished = windows[busy]
                    .overlays
                    .operation
                    .as_mut()
                    .is_some_and(advance_operation);
                if present_whole(
                    &mut windows[busy],
                    &mut client,
                    theme,
                    &icons,
                    desktop.scale(),
                )
                .is_err()
                {
                    return fail(EXIT_CHANNEL_LOST, "present refused");
                }
                if finished {
                    // Re-list so the view reflects what actually remains — a
                    // partial removal (a refusal or a cancel) is shown
                    // honestly; a failed re-list leaves the browser put (fail
                    // closed).
                    windows[busy].overlays.operation = None;
                    let _ = windows[busy].browser.refresh();
                    // Reap any launched bundle that exited while the operation
                    // ran (the wait-set was not parked on during it).
                    launcher.borrow_mut().reap();
                    if present_whole(
                        &mut windows[busy],
                        &mut client,
                        theme,
                        &icons,
                        desktop.scale(),
                    )
                    .is_err()
                    {
                        return fail(EXIT_CHANNEL_LOST, "present refused");
                    }
                    continue;
                }
                // Poll (non-blocking) for a cancel or a close while the walk
                // runs. An event naming the operating window is the
                // operation's; one naming another window is that window's and
                // is applied as usual, so a long walk in one window never
                // freezes the rest.
                match events.try_wait(&mut client) {
                    Ok(Some(event)) => {
                        // A desktop change during a long operation is
                        // adopted here too: this loop re-presents the
                        // progress panel on every pass, so re-theming is
                        // all it takes for the change to reach the screen.
                        match desktop.apply(&event) {
                            Ok(true) => {
                                themes.set_appearance(desktop.appearance());
                                theme = themes.active();
                            }
                            Ok(false) => {}
                            Err(err) => {
                                let _ = writeln!(
                                    Stderr,
                                    "files: could not apply desktop change: {err}"
                                );
                            }
                        }
                        if event.window_id() == Some(windows[busy].window) {
                            let win = &mut windows[busy];
                            let canvas = Canvas {
                                theme,
                                mode: &win.mode,
                                scale: desktop.scale(),
                                chrome: win.chrome,
                            };
                            match operation_control(
                                canvas.chrome.rail(&win.places),
                                canvas.scale,
                                canvas.theme(),
                                canvas.window(),
                                canvas.chrome.toolbar,
                                &event,
                            ) {
                                OperationControl::Cancel => {
                                    if let Some(operation) = win.overlays.operation.as_mut() {
                                        operation.progress.request_cancel();
                                    }
                                }
                                OperationControl::Close => {
                                    close_window(&mut windows, busy, &mut client);
                                }
                                OperationControl::Ignore => {}
                            }
                        } else if let Some(code) = route_event(
                            &mut windows,
                            &mut client,
                            &mut desktop,
                            &mut places,
                            theme,
                            &icons,
                            &launcher,
                            &reads,
                            event_endpoint,
                            start.role,
                            &event,
                        ) {
                            return code;
                        }
                    }
                    // Nothing queued, or a malformed frame from the
                    // authenticated session: refused, and the operation
                    // carries on (never guessed at).
                    Ok(None) | Err(Errno::OutOfRange | Errno::BadMagic | Errno::BufferTooSmall) => {
                    }
                    Err(_) => return fail(EXIT_CHANNEL_LOST, "event channel lost"),
                }
                continue;
            }

            // Queued input first, then one recorded icon decode, and a park
            // only when neither has anything left. Serving input ahead of the
            // decode is what keeps a key or a click waiting at most one
            // decode, rather than the whole grid's worth the paint used to
            // perform inside itself.
            let delivered = match events.try_wait(&mut client) {
                Ok(Some(event)) => Ok(Some(event)),
                Ok(None) => {
                    // A bundle scan the reader has answered opens the
                    // chooser the click asked for. Collected before the
                    // listings so the chooser appears on the very frame the
                    // scan landed on.
                    let mut resumed = false;
                    if let Some(installed) = reads.take_bundles() {
                        for win in &mut windows {
                            resumed |= settle_open_with(&mut win.overlays, &installed);
                        }
                    }
                    // A listing the reader has answered is adopted here: the
                    // browser holds the navigation it could not complete, and
                    // resuming it is what turns the answer into entries. It
                    // costs a taken `Option` when nothing is pending, so
                    // asking every turn is free.
                    for win in &mut windows {
                        match win.browser.resume() {
                            Ok(committed) => resumed |= committed,
                            Err(err) => {
                                report_error(&alloc::format!("listing refused ({err})"));
                            }
                        }
                    }
                    if resumed {
                        // A listing is a whole new set of entries, so the
                        // window is repainted whole rather than by a mark: no
                        // reading of the old state describes where anything is
                        // now.
                        for win in &mut windows {
                            if present_whole(win, &mut client, theme, &icons, desktop.scale())
                                .is_err()
                            {
                                return fail(EXIT_CHANNEL_LOST, "present refused");
                            }
                        }
                        continue;
                    }
                    let ran = {
                        let mut pipeline = icons.borrow_mut();
                        let ran = pipeline.pump();
                        artwork_landed |= pipeline.take_landed();
                        ran
                    };
                    if ran {
                        continue;
                    }
                    // The desk is dry, so the batch that landed is drawn now:
                    // one whole-window pass for the whole batch, not one per
                    // icon, and the grid's tiles appear together — a present
                    // is a round trip through the compositor, which is far
                    // dearer than the decode that produced one tile.
                    if core::mem::take(&mut artwork_landed) {
                        for win in &mut windows {
                            if present_whole(win, &mut client, theme, &icons, desktop.scale())
                                .is_err()
                            {
                                return fail(EXIT_CHANNEL_LOST, "present refused");
                            }
                        }
                        continue;
                    }
                    events.wait(&mut client)
                }
                Err(err) => Err(err),
            };
            // A park the reader interrupted has no event; the collect at the
            // top of the next turn is what adopts what it woke for.
            let delivered = match delivered {
                Ok(None) => continue,
                Ok(Some(event)) => Ok(event),
                Err(err) => Err(err),
            };
            let event = match delivered {
                Ok(event) => event,
                // A malformed frame from the authenticated session is
                // refused and the app keeps waiting (never guessed at).
                Err(Errno::OutOfRange | Errno::BadMagic | Errno::BufferTooSmall) => continue,
                Err(_) => return fail(EXIT_CHANNEL_LOST, "event channel lost"),
            };

            // The desktop belongs to the seat, not to one window, so a change
            // is adopted once and every window is repainted in it.
            match desktop.apply(&event) {
                Ok(true) => {
                    themes.set_appearance(desktop.appearance());
                    theme = themes.active();
                    for win in &mut windows {
                        if present_whole(win, &mut client, theme, &icons, desktop.scale()).is_err()
                        {
                            return fail(EXIT_CHANNEL_LOST, "present refused");
                        }
                    }
                }
                Ok(false) => {}
                Err(err) => {
                    let _ = writeln!(Stderr, "files: could not apply desktop change: {err}");
                }
            }

            if let Some(code) = route_event(
                &mut windows,
                &mut client,
                &mut desktop,
                &mut places,
                theme,
                &icons,
                &launcher,
                &reads,
                event_endpoint,
                start.role,
                &event,
            ) {
                return code;
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
