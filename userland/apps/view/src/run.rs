//! The `view.app` bundle's `Run` entry point: the picture and document
//! viewer.
//!
//! # The capability story
//!
//! The manifest requests **no filesystem capability**: on its own this
//! program can open, list, and stat nothing. A document reaches it one of two
//! ways, and both are the user's own act — a read-only descriptor the file
//! manager had the kernel clone in at spawn, or a one-shot `fd_grant` the
//! session's trusted picker delegated after the user chose a file in the
//! *session's* UI under the *session's* authority.
//!
//! The document is then untrusted input, so it is never decoded here: the
//! bytes are streamed to a capability-empty worker this same binary is
//! re-entered as, which holds no filesystem reach at all and answers pixels.
//! A malformed or hostile file crashes that worker and nothing else.
//!
//! # What runs where
//!
//! Everything with behaviour worth testing is in the host-tested
//! [`tairix_view`] engine: the document model, the viewport, the one layout,
//! the command set, the input routing and the renderers. This binary composes
//! them over the live syscalls, and keeps two things off the loop that owes
//! the user a frame — reading the file, and driving the sandbox — because
//! both wait on something. They run on the shared worker desk; the loop
//! submits and carries on drawing.
//!
//! On the host it is an inert stub so `cargo build --workspace`, clippy, and
//! fmt still cover the file.

#![cfg_attr(freestanding, no_std)]
#![cfg_attr(freestanding, no_main)]
#![deny(missing_docs)]

#[cfg(freestanding)]
mod program {
    extern crate alloc;

    use alloc::string::String;
    use alloc::sync::Arc;
    use alloc::vec::Vec;
    use core::cell::Cell;

    use tairix_abi::fs::{FileKind, FileStat};
    use tairix_abi::input::KeyInput;
    use tairix_abi::latency::DEFAULT_FRAME_BUDGET_NS;
    use tairix_abi::window_ipc::{
        AppBarClick, AppMenu, AppMenuItem, AppMenuItemId, AppMenuLabel, AppMenuMark, AppMenuRow,
        AppMenuShortcut, MenuOutcome, TooltipText, WindowEvent, WindowRegion,
    };
    use tairix_abi::{Errno, WaitSetOp, WaitSourceKind, DOCUMENT_ROLE_ARG, STDIN};
    use tairix_controls::damage;
    use tairix_font::BitmapFont;
    use tairix_geometry::{Point, Rect, Region, Scale};
    use tairix_help::{own_short_help, BundleHelp};
    use tairix_icon::NoArtwork;
    use tairix_input::InputEvent;
    use tairix_rt::io::{Stderr, Stdout, Write};
    use tairix_sandbox::imagerender::{
        begin_document, close_view, open_view, push_document, render_page, select_page,
        ImageRenderService, ViewDocument, ViewFailure, ViewFormat, ViewPage, ViewRefusal,
        MAX_DOCUMENT_CHUNK,
    };
    use tairix_sandbox::rt::{serve_stdio, worker_role, RtLauncher};
    use tairix_sandbox::{ParserSandbox, ServeEnd};
    use tairix_theme::{TextRole, Theme};
    use tairix_view::view::{Command, Outcome, View};
    use tairix_view::{
        Answer, Layout, Refusal, Request, MAX_DOCUMENT_BYTES, MIN_WIN_HEIGHT, MIN_WIN_WIDTH,
        WIN_HEIGHT, WIN_WIDTH,
    };
    use tairix_window::app::{self, Wake};
    use tairix_window::{
        key_input_event, pointer_input_events, pointer_point, present_damage, EventDrain,
        EventError, EventMailbox, EventSource, Parked, Repaint, WindowEvents, WindowSizing,
    };

    /// The name this program states its own refusals under.
    const APP_NAME: &str = "view";

    /// The wait-set token of the sandbox worker's answer wake.
    const WORKER_TOKEN: u64 = app::FIRST_APP_TOKEN;

    /// State the abnormal-exit reason on `stderr` and hand `code` back for
    /// `main`: an exit code alone is not a diagnosis.
    fn fail(code: i32, reason: &str) -> i32 {
        let _ = writeln!(Stderr, "{APP_NAME}: {reason}");
        code
    }

    /// State a bring-up refusal the shared shell reported.
    fn fail_shell(err: app::ShellError) -> i32 {
        fail(err.code(), &alloc::format!("{err}"))
    }

    /// Report something the user should know that is not fatal.
    fn report(reason: &str) {
        let _ = writeln!(Stderr, "{APP_NAME}: {reason}");
    }

    // ---- the sandbox session -------------------------------------------

    /// The authorised read-only descriptor a document is read from.
    ///
    /// Two cases, because their lifetimes differ: a descriptor inherited at
    /// spawn belongs to the process and is reclaimed by the runtime, while a
    /// redeemed delegation is this program's own and closes on every path out
    /// of the value that holds it.
    enum Handle {
        /// Cloned in at spawn by the launcher.
        Inherited(u32),
        /// Redeemed from the picker's one-shot grant.
        Delegated(tairix_rt::File),
    }

    impl Handle {
        /// The descriptor to read.
        fn fd(&self) -> u32 {
            match self {
                Self::Inherited(fd) => *fd,
                Self::Delegated(file) => file.fd(),
            }
        }

        /// How long the document is, in bytes.
        ///
        /// One `fs_stat`, which is ungated at the dispatcher precisely for
        /// this case: the descriptor's own backing decides the authority, so
        /// a holder with no filesystem capability of its own can describe the
        /// file it was handed. Measuring by reading to the end instead would
        /// read the whole document twice — once to size it and once to send
        /// it — for a figure the kernel already knows.
        fn length(&self) -> Result<usize, Refusal> {
            let mut record = [0u8; FileStat::WIRE_LEN];
            let read = tairix_rt::fs_stat_raw(self.fd(), &mut record)
                .map_err(|raw| Refusal::Unreadable(Errno::from_syscall(raw)))?;
            if read < FileStat::WIRE_LEN {
                return Err(Refusal::Unreadable(Errno::BufferTooSmall));
            }
            let stat = FileStat::decode(&record).map_err(Refusal::Unreadable)?;
            if stat.kind != FileKind::Regular {
                // A directory or a device is not a document, and its declared
                // length says nothing about what a read would give.
                return Err(Refusal::Unreadable(Errno::OutOfRange));
            }
            let length = usize::try_from(stat.size).map_err(|_| Refusal::TooLong)?;
            if length > MAX_DOCUMENT_BYTES {
                return Err(Refusal::TooLong);
            }
            Ok(length)
        }
    }

    /// What only this binary knows about a document: the descriptor it is
    /// read from, what to call it, and the format to read it as.
    struct Source {
        handle: Handle,
        /// The document's own file name, empty when the hand-off did not
        /// carry one.
        name: String,
        /// The format to name in place of reading the document's signature,
        /// for the one format that carries none.
        format: Option<ViewFormat>,
    }

    /// One job the worker carries out.
    ///
    /// The engine's [`Request`] says *what* is wanted; this adds what only
    /// this binary holds — the descriptor a document is read from. An open
    /// therefore cannot be asked for without a source to open.
    enum Job {
        /// Stream `source` into the worker and open it.
        Open(Source),
        /// The engine's render request, carried through unchanged.
        Show {
            /// The entry to hold decoded.
            page: u32,
            /// The extent the page is scaled to, in page space.
            extent: (u32, u32),
            /// The rectangle of that scaling to draw, in page space.
            window: Rect,
            /// The buffer to draw into, handed back in the answer.
            pixels: Vec<u8>,
        },
    }

    /// What the worker keeps between jobs: the sandbox, and so the open
    /// document and the page it holds decoded.
    ///
    /// A view is a *session* — open once, then draw from the page held — so
    /// the sandbox must outlive one job. It lives here, reachable only from
    /// the thread carrying work out, so the loop can never touch it.
    struct Session {
        sandbox: ParserSandbox<RtLauncher, tairix_rt::LogSink>,
        /// Whether a document is open, so a replacement releases the one
        /// before it rather than leaving the worker holding both.
        open: bool,
    }

    /// The worker desk: one job in flight, latest-wins, answers arriving as a
    /// wake on the loop's own wait-set.
    type Worker = tairix_rt::work::Worker<Session, Job, Answer>;

    /// Carry out one job against the session.
    ///
    /// The job is taken by exclusive reference so the buffer a render was lent
    /// is drawn into and handed straight back in the answer, rather than a
    /// window's worth of pixels being allocated per pointer sample.
    fn serve_job(session: &mut Session, job: &mut Job) -> Answer {
        match job {
            Job::Open(source) => Answer::Opened {
                opened: open_source(session, source),
            },
            Job::Show {
                page,
                extent,
                window,
                pixels,
            } => {
                let (decoded, outcome) = draw(session, *page, *extent, *window, pixels);
                Answer::Shown {
                    page: *page,
                    extent: *extent,
                    window: *window,
                    decoded,
                    pixels: core::mem::take(pixels),
                    outcome,
                }
            }
        }
    }

    /// Stream `source` into the worker and open it, answering what the
    /// container declares.
    fn open_source(
        session: &mut Session,
        source: &Source,
    ) -> Result<(ViewDocument, String, u64), Refusal> {
        if session.open {
            // The worker holds a document and everything decoded from it;
            // dropping that first is what keeps a replacement from paying for
            // both at once.
            let _ = close_view(&mut session.sandbox);
            session.open = false;
        }
        let length = upload(session, &source.handle)?;
        let declared = open_view(&mut session.sandbox, source.format).map_err(Refusal::Failed)?;
        session.open = true;
        Ok((declared, source.name.clone(), length))
    }

    /// Read the descriptor and push it to the worker in protocol-bounded
    /// chunks, answering how many bytes it holds.
    ///
    /// The document's length is declared before any of it is sent, so the
    /// descriptor is measured first and then streamed: at no point does this
    /// process hold more than one chunk of an untrusted file, and the fixed
    /// ceiling bounds what is *resident* rather than what is addressable,
    /// because the reads are positional.
    fn upload(session: &mut Session, handle: &Handle) -> Result<u64, Refusal> {
        let fd = handle.fd();
        let length = handle.length()?;
        begin_document(&mut session.sandbox, length)
            .map_err(|err| Refusal::Failed(ViewFailure::Document(err)))?;
        let length = length as u64;
        let mut chunk =
            tairix_util::fallible::filled(MAX_DOCUMENT_CHUNK, 0u8).ok_or(Refusal::Unholdable)?;
        let mut sent = 0u64;
        while sent < length {
            let want = usize::try_from(length - sent)
                .unwrap_or(MAX_DOCUMENT_CHUNK)
                .min(MAX_DOCUMENT_CHUNK);
            let got = read_at(fd, sent, &mut chunk[..want])?;
            if got == 0 {
                // The file is shorter than it measured, so the declaration
                // the worker holds can no longer be satisfied: fail closed
                // rather than pad the document with anything.
                return Err(Refusal::Unreadable(Errno::OutOfRange));
            }
            push_document(&mut session.sandbox, &chunk[..got])
                .map_err(|err| Refusal::Failed(ViewFailure::Document(err)))?;
            sent = sent.saturating_add(got as u64);
        }
        Ok(length)
    }

    /// Read from `fd` at `offset`, reporting the kernel's own refusal.
    fn read_at(fd: u32, offset: u64, into: &mut [u8]) -> Result<usize, Refusal> {
        tairix_rt::fs_read(fd, offset, into)
            .map_err(|raw| Refusal::Unreadable(Errno::from_syscall(raw)))
    }

    /// Bring the session to `page` and draw `window` of it scaled to
    /// `extent`, into `pixels`.
    fn draw(
        session: &mut Session,
        page: u32,
        extent: (u32, u32),
        window: Rect,
        pixels: &mut Vec<u8>,
    ) -> (Option<ViewPage>, Result<(), Refusal>) {
        if !session.open {
            return (
                None,
                Err(Refusal::Failed(ViewFailure::Refused(ViewRefusal::NotOpen))),
            );
        }
        let decoded = match select_page(&mut session.sandbox, page) {
            Ok(decoded) => decoded,
            Err(err) => return (None, Err(Refusal::Failed(err))),
        };
        let Some(wanted) = pixel_len(window) else {
            return (
                Some(decoded),
                Err(Refusal::Failed(ViewFailure::Refused(
                    ViewRefusal::MalformedRequest,
                ))),
            );
        };
        if pixels.len() != wanted && !tairix_util::fallible::grow_to(pixels, wanted, 0) {
            return (Some(decoded), Err(Refusal::Unholdable));
        }
        pixels.truncate(wanted);
        let outcome = render_page(
            &mut session.sandbox,
            extent,
            tairix_raster::Region {
                x: u32::try_from(window.left()).unwrap_or(0),
                y: u32::try_from(window.top()).unwrap_or(0),
                width: window.width,
                height: window.height,
            },
            pixels,
        )
        .map_err(Refusal::Failed);
        (Some(decoded), outcome)
    }

    /// The straight-alpha byte count `window` holds, or `None` for a window
    /// no buffer could describe.
    fn pixel_len(window: Rect) -> Option<usize> {
        usize::try_from(window.width)
            .ok()?
            .checked_mul(usize::try_from(window.height).ok()?)?
            .checked_mul(4)
    }

    // ---- the document a launch was given -------------------------------

    /// The document this program was handed on [`STDIN`] by its launcher: a
    /// read-only descriptor the kernel cloned in at spawn, so it is read with
    /// no filesystem capability of this program's own.
    fn inherited() -> Source {
        let name = tairix_rt::arg(2)
            .and_then(|raw| core::str::from_utf8(raw).ok())
            .map(leaf_of)
            .unwrap_or_default();
        Source {
            handle: Handle::Inherited(STDIN),
            format: format_for(&name),
            name,
        }
    }

    /// The document the session's picker delegated: a one-shot `fd_grant`
    /// redeemed into a read-only descriptor whose reads the kernel authorises
    /// under the session's captured identity.
    ///
    /// The pick conclusion carries the authority and nothing else, so the
    /// document arrives **unnamed**: the viewer states what it knows and
    /// invents nothing. A picked sprite area therefore cannot be reached by
    /// being named either, which is a limitation of the pick conclusion
    /// rather than of the decoder (`plans/VIEW.md`).
    fn delegated(handle: u64) -> Option<Source> {
        Some(Source {
            handle: Handle::Delegated(tairix_rt::File::from_delegation(handle).ok()?),
            name: String::new(),
            format: None,
        })
    }

    /// The last component of a path.
    fn leaf_of(path: &str) -> String {
        String::from(path.rsplit('/').next().unwrap_or(path))
    }

    /// The format to read a document as in place of its own signature, or
    /// `None` to let the decoder sniff it.
    ///
    /// Only the one format that carries no signature is named: a RISC OS
    /// sprite area's first word is its sprite count, so nothing in the file
    /// can identify it and the name is the only door. Every other format is
    /// recognised from its bytes, which is stronger than trusting a file
    /// name — a document named `.png` that is a JPEG opens as the JPEG it is.
    fn format_for(name: &str) -> Option<ViewFormat> {
        let extension = name.rsplit_once('.')?.1;
        extension
            .eq_ignore_ascii_case("spr")
            .then_some(ViewFormat::Sprite)
    }

    // ---- the app-declared menu -----------------------------------------

    /// The commands the app's own menu offers, in the order they are shown.
    ///
    /// One ordered list, so a row's label, its accelerator caption and the
    /// command it runs are the same position — a menu whose rows and actions
    /// could be listed separately is one that can be wired up wrong.
    const MENU: [(&str, &str, Command); 9] = [
        ("Open…", "O", Command::OpenDocument),
        ("Zoom in", "+", Command::ZoomIn),
        ("Zoom out", "-", Command::ZoomOut),
        ("Fit in window", "0", Command::FitWindow),
        ("Fit width", "2", Command::FitWidth),
        ("Actual size", "1", Command::ActualSize),
        ("Rotate right", "]", Command::RotateRight),
        ("Mirror", "M", Command::Mirror),
        ("Information", "I", Command::ToggleInfo),
    ];

    /// Build the app's menu, marking the rows whose state is a toggle.
    ///
    /// A refused row is dropped rather than the whole menu being abandoned:
    /// the menu is incidental to the viewer's purpose, so the user gets the
    /// rows that fit and is told how many did not.
    fn build_menu(view: &View) -> (AppMenu, usize) {
        let mut menu = AppMenu::EMPTY;
        let mut skipped = 0;
        for (index, (label, shortcut, command)) in MENU.iter().enumerate() {
            let (Some(id), Ok(label)) = (AppMenuItemId::for_index(index), AppMenuLabel::new(label))
            else {
                skipped += 1;
                continue;
            };
            let mut item = AppMenuItem::new(id, label);
            if let Ok(caption) = AppMenuShortcut::new(shortcut) {
                item = item.with_shortcut(caption);
            }
            if matches!(command, Command::ToggleInfo) && view.info_open() {
                item = item.with_mark(AppMenuMark::Check);
            }
            if menu.push(AppMenuRow::Item(item)).is_err() {
                skipped += 1;
            }
        }
        (menu, skipped)
    }

    /// The command a chosen menu row names.
    fn menu_command(id: AppMenuItemId) -> Option<Command> {
        MENU.get(id.index()).map(|(_, _, command)| *command)
    }

    // ---- the one window --------------------------------------------

    /// The viewer's window: the shell's single-window channel state and the
    /// engine that draws into its retained surface.
    ///
    /// One window per process, because the bundle declares multiple
    /// *instances*: opening a second document starts a second viewer, which
    /// is what puts two pictures side by side without either being able to
    /// disturb the other's decode.
    struct Window {
        shell: app::AppWindow,
        view: View,
        /// The menu open over the window, so an outcome is matched to the
        /// gesture that asked for it rather than to whichever was last.
        menu: Option<u64>,
        /// The tooltip region last declared, so it is only sent again when it
        /// moves.
        tip: Option<Rect>,
        /// The title the session was last told, so it is only set again when
        /// the document changes.
        title: String,
    }

    impl Window {
        /// Resolve the layout for the window's current extent.
        ///
        /// Answers the layout of a zero-sized window when there is none open,
        /// which every painter and hit-test reads as absent.
        fn layout(&mut self, theme: &Theme, scale: Scale) -> Layout {
            let (width, height) = self
                .shell
                .mode()
                .map_or((0, 0), |mode| (mode.width_px, mode.height_px));
            self.view
                .layout(width, height, theme, scale, face(theme, scale))
        }

        /// Paint what `repaint` owes and present it.
        ///
        /// The whole viewer is re-derived under the clip the shell narrows to,
        /// so a partial repaint lands the pixels a whole one would have —
        /// there is no second "paint just this part" recipe.
        fn present(
            &mut self,
            repaint: Repaint,
            reported: &Region,
            theme: &Theme,
            scale: Scale,
        ) -> Result<(), Errno> {
            let Some(mode) = self.shell.mode().copied() else {
                return Ok(());
            };
            // The session gave its copy of the region back, so nothing of
            // what was on screen survives a reported repaint.
            let repaint = if self.shell.content_released() {
                Repaint::Whole
            } else {
                repaint
            };
            let Some(area) = present_damage(&mode, repaint, reported) else {
                return Ok(());
            };
            let layout = self.layout(theme, scale);
            let view = &self.view;
            self.shell.present(area, |surface| {
                tairix_view::paint::render_into(
                    surface,
                    view,
                    &layout,
                    theme,
                    scale,
                    face(theme, scale),
                    &mut NoArtwork,
                );
            })
        }
    }

    /// The face the viewer sets its own text in.
    fn face(theme: &Theme, scale: Scale) -> BitmapFont {
        BitmapFont::for_role(theme.fonts(), TextRole::Body, scale)
    }

    // ---- the event source ----------------------------------------------

    /// The app's park: its event mailbox, the memory-pressure band, the
    /// worker's answer wake, and the animation deadline.
    struct RtEventSource<'a> {
        mailbox: EventMailbox,
        set: u64,
        worker: &'a Worker,
        /// When the next animation frame is due, or `None` when nothing is
        /// timed — in which case the park has no deadline at all and the CPU
        /// is given up entirely.
        ///
        /// Shared with the loop through a cell because the loop owns the
        /// deadline and the source owns the park: one writes it just before
        /// the other reads it, on the one thread both run on.
        deadline_ns: &'a Cell<Option<u64>>,
    }

    impl EventDrain for RtEventSource<'_> {
        fn try_next(&mut self, event: &mut [u8; WindowEvent::WIRE_LEN]) -> Result<bool, Errno> {
            self.mailbox.try_next(event)
        }
    }

    impl EventSource for RtEventSource<'_> {
        fn park(&mut self) -> Result<Parked, Errno> {
            let woken = match self.deadline_ns.get() {
                // One-shot, to the next frame the animation actually needs:
                // no periodic tick, and no timer armed while it is paused.
                Some(deadline) => match app::park_until(self.set, deadline)? {
                    Some(woken) => woken,
                    None => return Ok(Parked::Interrupted),
                },
                None => app::park(self.set)?,
            };
            match woken {
                Wake::App(WORKER_TOKEN) => {
                    // The readiness is a level peek, so leaving it undrained
                    // would report ready for ever and turn the park into a
                    // spin.
                    self.worker.wake().drain();
                    Ok(Parked::Interrupted)
                }
                Wake::PressureChanged => {
                    tairix_font::trim_glyph_cache();
                    Ok(Parked::Served)
                }
                _ => Ok(Parked::Served),
            }
        }
    }

    // ---- the run -------------------------------------------------------

    /// The title a window opens under, replaced by the document's own name
    /// once one is open and named.
    const APP_TITLE: &str = "View";

    /// The tooltip for the tool the pointer is over, or none.
    fn tool_tip(layout: &Layout, at: Point) -> Option<(Rect, &'static str)> {
        let tools = layout.tools();
        let slot = tools.height;
        if slot == 0 || !tools.contains(at) {
            return None;
        }
        let across = u32::try_from(at.x.checked_sub(tools.left())?).ok()?;
        let index = usize::try_from(across / slot).ok()?;
        let (_, _, text) = tairix_view::view::TOOLS.get(index)?;
        let left = tools
            .left()
            .checked_add(tairix_geometry::to_i32(u32::try_from(index).ok()? * slot))?;
        Some((Rect::new(left, tools.top(), slot, slot), text))
    }

    /// Declare, or withdraw, the tooltip for whatever the pointer is over.
    ///
    /// A session that shows no tooltips refuses this; the tip is incidental
    /// to the viewer's purpose, so the refusal ends the asking and the viewer
    /// carries on rather than asking again on every pointer sample.
    fn set_tip(window: &mut Window, layout: &Layout, at: Point) {
        let wanted = tool_tip(layout, at);
        let region = wanted.map(|(rect, _)| rect);
        if region == window.tip {
            return;
        }
        window.tip = region;
        let Some(id) = window.shell.window_id() else {
            return;
        };
        let (rect, text) = wanted.unwrap_or((Rect::EMPTY, ""));
        let (Ok(anchor), Ok(text)) = (
            WindowRegion::new(rect.left(), rect.top(), rect.width, rect.height),
            TooltipText::new(text),
        ) else {
            return;
        };
        if window.shell.client().set_tooltip(id, anchor, text).is_err() {
            window.tip = None;
        }
    }

    /// Tell the session what the window is showing, once, when it changes.
    fn retitle(window: &mut Window) {
        let wanted = window
            .view
            .document()
            .map(|document| document.name.clone())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| String::from(APP_TITLE));
        if wanted == window.title {
            return;
        }
        if let Some(id) = window.shell.window_id() {
            if window.shell.client().set_title(id, &wanted).is_ok() {
                window.title = wanted;
            }
        }
    }

    /// Declare this viewer's presence on the desktop's icon bar.
    ///
    /// A refused declaration is an answer, not a death: the viewer says so
    /// and carries on with no slot of its own — its window is still reachable.
    fn declare_app_bar(window: &mut Window, endpoint: u64) {
        match tairix_window::info_and_quit(endpoint, AppBarClick::RaiseOrOpen) {
            Ok(bar) => {
                if let Err(err) = window.shell.client().set_app_bar(&bar) {
                    report(&alloc::format!(
                        "the desktop refused this application's icon-bar presence ({err}); \
                         carrying on without one"
                    ));
                }
            }
            Err(err) => report(&alloc::format!(
                "this application's icon-bar menu is invalid ({err:?}); carrying on without one"
            )),
        }
    }

    /// Print the bundle's own short help and answer the exit code.
    fn print_help() -> i32 {
        let locale = tairix_rt::env_var(b"LANG").and_then(|raw| core::str::from_utf8(raw).ok());
        let Some(bytes) = own_short_help(&BundleHelp::new(APP_NAME), locale, APP_NAME) else {
            return fail(1, "this bundle's help documents could not be read");
        };
        match Stdout.write_all(&bytes) {
            Ok(()) => 0,
            Err(_) => 1,
        }
    }

    /// Ask the picker for a document, reporting a session that has none.
    fn ask_for_document(window: &mut Window) {
        let Some(id) = window.shell.window_id() else {
            return;
        };
        if window.shell.client().pick_file(id).is_err() {
            report("the desktop offers no file picker; open a document from the files app");
        }
    }

    /// The viewer's whole life.
    #[allow(
        clippy::too_many_lines,
        reason = "one linear bring-up plus one event loop; splitting the loop would separate the park from the drain it must follow"
    )]
    fn main() -> i32 {
        // The sandbox-worker role, before anything else: a document is
        // untrusted input, so it is decoded by a capability-empty child this
        // same binary is re-entered as, serving over its wired standard
        // streams and nothing else. It never becomes the viewer.
        if worker_role() {
            let mut service = ImageRenderService::default();
            return match serve_stdio(&mut service) {
                ServeEnd::Finished => 0,
                ServeEnd::Failed(_) => 1,
            };
        }
        if tairix_rt::arg(1).is_some_and(|arg| matches!(arg, b"-h" | b"--help" | b"-?")) {
            return print_help();
        }

        // A user-facing loop, so declare the frame it owes. A debug image
        // reports any span that overruns; a shippable one arms nothing.
        let _ = tairix_rt::latency_watch(DEFAULT_FRAME_BUDGET_NS);

        // How the viewer starts depends on how it was launched: handed a
        // document at spawn it opens that, and launched on its own it asks
        // the session's trusted picker.
        let mut pending_source = tairix_rt::arg(1)
            .is_some_and(|arg| arg == DOCUMENT_ROLE_ARG)
            .then(inherited);

        let mut window = Window {
            shell: app::AppWindow::new(),
            view: View::new(pending_source.is_some()),
            menu: None,
            tip: None,
            title: String::from(APP_TITLE),
        };
        let (desktop, themes) = match app::bring_up_desktop(window.shell.client()) {
            Ok(pair) => pair,
            Err(err) => return fail_shell(err),
        };
        let theme = themes.active();
        let scale = desktop.scale();

        let binding = match app::bind_event_mailbox() {
            Ok(binding) => binding,
            Err(err) => return fail_shell(err),
        };
        let event_endpoint = binding.endpoint();
        let set = binding.set();

        // Reading the file and driving the sandbox both wait on something, so
        // both go on the worker; the loop submits and carries on drawing.
        let worker = Arc::new(Worker::new(
            serve_job,
            Session {
                sandbox: ParserSandbox::new(RtLauncher::own_binary(), tairix_rt::LogSink),
                open: false,
            },
            tairix_rt::sync::WorkerWake::create(),
        ));
        if let Err(reason) = Worker::start(&worker) {
            report(&alloc::format!(
                "no decode worker ({reason:?}); documents are read on the event loop"
            ));
        }
        let _worker_guard = tairix_rt::work::WorkerGuard::new(&worker);
        if let Some(read) = worker.wake().read_end() {
            if tairix_rt::waitset_ctl(
                set,
                WaitSetOp::Add,
                WaitSourceKind::Stream,
                u64::from(read),
                WORKER_TOKEN,
            ) != 0
            {
                return fail(app::EXIT_NO_EVENTS, "decode answer wake refused");
            }
        }

        let (initial_w, initial_h) = desktop.window_size(WIN_WIDTH, WIN_HEIGHT);
        let mode = app::mode_for(initial_w, initial_h);
        let sizing = WindowSizing::Resizable {
            min_width_px: MIN_WIN_WIDTH,
            min_height_px: MIN_WIN_HEIGHT,
        };
        let server = match window.shell.open(event_endpoint, &mode, APP_TITLE, sizing) {
            Ok(server) => server,
            Err(err) => return fail_shell(err),
        };
        declare_app_bar(&mut window, event_endpoint);
        if pending_source.is_none() {
            ask_for_document(&mut window);
        }
        // The first frame is the whole window: nothing of it is on screen yet.
        if window
            .present(Repaint::Whole, &damage::sink(), theme, scale)
            .is_err()
        {
            return fail(app::EXIT_CHANNEL_LOST, "first present refused");
        }

        let deadline = Cell::new(None);
        let mut events = WindowEvents::new(RtEventSource {
            mailbox: EventMailbox::new(event_endpoint, server),
            set,
            worker: &worker,
            deadline_ns: &deadline,
        });

        loop {
            let mut reported = damage::sink();

            // An answer the worker landed first, so a picture appears the
            // moment it is ready rather than at whatever later input arrives.
            if let Some(answer) = worker.collect() {
                let layout = window.layout(theme, scale);
                let changed = window.view.deliver(answer, &layout, &mut reported).changed;
                retitle(&mut window);
                if changed
                    && window
                        .present(Repaint::Reported, &reported, theme, scale)
                        .is_err()
                {
                    return fail(app::EXIT_CHANNEL_LOST, "present refused");
                }
                continue;
            }

            // Then queued input, drained before anything is painted so a
            // burst of pointer motion costs one frame rather than one each;
            // and with nothing queued, whatever the park wakes on. Both reach
            // the one routing below: a park *consumes* the event it woke on,
            // so nothing else would ever see it again.
            let delivered = match events.try_wait(window.shell.client()) {
                Ok(None) => {
                    // Nothing queued: submit whatever the state now calls
                    // for. The answer is collected on the next turn either
                    // way — a deferred job wakes the park, and one carried
                    // out inline for want of a worker thread is already on
                    // the desk — so what `submit` reports about where it ran
                    // changes nothing here.
                    match window.view.next_request() {
                        Some(Request::Open) => {
                            // The open stays outstanding until it is
                            // answered, so this arm repeats while the worker
                            // is reading. Only a source is worth submitting;
                            // without one the picker's conclusion is what
                            // brings it, and that arrives as an event. Either
                            // way park rather than re-ask a question whose
                            // answer cannot have changed yet.
                            if let Some(source) = pending_source.take() {
                                worker.submit(Job::Open(source));
                                continue;
                            }
                        }
                        Some(Request::Show {
                            page,
                            extent,
                            window: rect,
                            pixels,
                        }) => {
                            worker.submit(Job::Show {
                                page,
                                extent,
                                window: rect,
                                pixels,
                            });
                            continue;
                        }
                        None => {}
                    }

                    // Nothing to submit: arm the animation deadline — one
                    // shot, to the next frame the container actually asks
                    // for — and park. A paused viewer arms nothing at all.
                    let now = tairix_rt::clock_get();
                    window.view.arm_deadline(now);
                    deadline.set(window.view.deadline_ns());
                    let woken = events.wait(window.shell.client());
                    // A frame may be due whether the park ended on the
                    // deadline or on an event; the render it calls for is
                    // asked for on the next turn.
                    let _ = window.view.tick(tairix_rt::clock_get());
                    woken
                }
                other => other,
            };

            let event = match delivered {
                Ok(Some(event)) => event,
                // A park the worker's answer interrupted carries no event;
                // the collect at the top of the next turn adopts what it
                // woke for.
                Ok(None) => continue,
                Err(EventError::Mailbox(_)) => {
                    return fail(app::EXIT_CHANNEL_LOST, "the event channel died")
                }
                Err(EventError::Undecodable(_)) => {
                    report("a malformed window event was refused");
                    continue;
                }
            };
            let repaint = match route(
                &mut window,
                &mut pending_source,
                &event,
                theme,
                scale,
                &mut reported,
            ) {
                Routed::Changed(scope) => scope,
                Routed::Closed => return 0,
                Routed::Idle => continue,
            };
            if window.present(repaint, &reported, theme, scale).is_err() {
                return fail(app::EXIT_CHANNEL_LOST, "present refused");
            }
        }
    }

    /// What routing one event decided.
    enum Routed {
        /// Something drawn changed, at this scope.
        Changed(Repaint),
        /// The window closed, which ends the viewer.
        Closed,
        /// Nothing to do.
        Idle,
    }

    /// Route one delivered window event.
    fn route(
        window: &mut Window,
        pending_source: &mut Option<Source>,
        event: &WindowEvent,
        theme: &Theme,
        scale: Scale,
        reported: &mut Region,
    ) -> Routed {
        let layout = window.layout(theme, scale);
        match event {
            WindowEvent::CloseRequested { .. } | WindowEvent::AlternateCloseRequested { .. } => {
                let _ = window.shell.close();
                Routed::Closed
            }
            WindowEvent::Resized {
                width_px,
                height_px,
                ..
            } => {
                if !window.shell.resize(app::mode_for(*width_px, *height_px)) {
                    // A refused resize leaves the old geometry standing, so
                    // the window is still drawable at the size it had.
                    report("the desktop refused a resize; the window keeps its size");
                }
                // The reported client size is what the layout follows either
                // way, so the whole window is redrawn regardless.
                Routed::Changed(Repaint::Whole)
            }
            WindowEvent::RedrawRequested { .. } => Routed::Changed(Repaint::Whole),
            WindowEvent::FilePicked { handle, .. } => {
                let Some(source) = delegated(*handle) else {
                    report("the delegated document could not be redeemed");
                    return Routed::Idle;
                };
                *pending_source = Some(source);
                window.view.expect_document();
                reported.add(layout.window());
                Routed::Changed(Repaint::Reported)
            }
            WindowEvent::PickCancelled { .. } => {
                if window.view.cancelled() {
                    reported.add(layout.window());
                    return Routed::Changed(Repaint::Reported);
                }
                Routed::Idle
            }
            WindowEvent::Key {
                key: pressed @ KeyInput::Pressed { .. },
                ..
            } => {
                let InputEvent::KeyPressed { key, modifiers } = key_input_event(*pressed) else {
                    return Routed::Idle;
                };
                let outcome = window.view.on_key(key, modifiers, &layout, reported);
                act(window, outcome)
            }
            WindowEvent::Pointer { x, y, action, .. } => {
                let at = pointer_point(*x, *y);
                let mut changed = false;
                let mut asked = None;
                for input in pointer_input_events(*action, at) {
                    let outcome = window
                        .view
                        .on_pointer(&input, &layout, scale, theme, reported);
                    changed |= outcome.changed;
                    if outcome.pick || outcome.menu.is_some() || outcome.close {
                        asked = Some(outcome);
                    }
                }
                set_tip(window, &layout, at);
                match asked {
                    Some(outcome) => act(window, outcome),
                    None if changed => Routed::Changed(Repaint::Reported),
                    None => Routed::Idle,
                }
            }
            WindowEvent::MenuClosed {
                open_id, outcome, ..
            } => {
                if window.menu != Some(*open_id) {
                    // An answer to a gesture another open has superseded.
                    return Routed::Idle;
                }
                window.menu = None;
                let MenuOutcome::Chosen(item) = outcome else {
                    return Routed::Idle;
                };
                let Some(command) = menu_command(*item) else {
                    return Routed::Idle;
                };
                let outcome = window.view.run(command, &layout, reported);
                act(window, outcome)
            }
            // The slot's primary click with no window open asks for another
            // document, which is what the viewer's slot means.
            WindowEvent::AppBarDefault => {
                ask_for_document(window);
                Routed::Idle
            }
            WindowEvent::AppBarMenu { item } if tairix_window::is_quit(*item) => {
                let _ = window.shell.close();
                Routed::Closed
            }
            _ => Routed::Idle,
        }
    }

    /// Carry out whatever an engine outcome asked the embedder for.
    fn act(window: &mut Window, outcome: Outcome) -> Routed {
        if outcome.close {
            let _ = window.shell.close();
            return Routed::Closed;
        }
        if outcome.pick {
            ask_for_document(window);
        }
        if let Some(at) = outcome.menu {
            open_menu(window, at);
        }
        if outcome.changed {
            Routed::Changed(Repaint::Reported)
        } else {
            Routed::Idle
        }
    }

    /// Ask the session to open the app's own menu at `at`.
    ///
    /// The plate is the session's — the app draws no menu pixel — and a
    /// session that composes none is reported and carried on from.
    fn open_menu(window: &mut Window, at: Point) {
        let (menu, skipped) = build_menu(&window.view);
        if skipped > 0 {
            report(&alloc::format!(
                "{skipped} menu row(s) do not fit and are not shown"
            ));
        }
        let (Some(id), Ok(anchor)) = (
            window.shell.window_id(),
            WindowRegion::new(at.x, at.y, 0, 0),
        ) else {
            return;
        };
        match window.shell.client().open_menu(id, anchor, &menu) {
            Ok(open) => window.menu = Some(open),
            Err(_) => report("the desktop composes no menu service"),
        }
    }

    tairix_rt::entry!(main);
}

/// The host stub: this binary is a freestanding program on the Tier-1
/// targets, so on the host it exists only to keep the file covered by the
/// workspace build, clippy, and fmt.
#[cfg(not(freestanding))]
fn main() {}
