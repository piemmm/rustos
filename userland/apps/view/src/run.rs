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
//! # One instance, many windows, resident
//!
//! The viewer is a *single* instance with a window per document: a second
//! document opens a second window in this process rather than a second
//! process, which is the rule for every application with an icon-bar slot.
//! It stays on that slot with no window open — launched by the user it shows
//! nothing at all until it has something to display — and only the slot's
//! *Quit* row ends it.
//!
//! Each window holds its own engine, its own sandbox in the decode worker,
//! and its own open and render state, so a malformed file refuses in its own
//! window and disturbs no other.
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
    use tairix_abi::{Errno, ProcId, WaitSetOp, WaitSourceKind, DOCUMENT_ROLE_ARG, STDIN};
    use tairix_controls::damage;
    use tairix_font::BitmapFont;
    use tairix_geometry::{Point, Rect, Region, Scale};
    use tairix_help::{own_short_help, BundleHelp};
    use tairix_icon::NoArtwork;
    use tairix_input::InputEvent;
    use tairix_raster::Surface;
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
        Answer, Layout, Refusal, Request, MAX_DOCUMENT_BYTES, WIN_HEIGHT, WIN_WIDTH,
    };
    use tairix_window::app::{self, Wake, WindowPane};
    use tairix_window::{
        key_input_event, pointer_input_events, pointer_point, present_damage, Desktop, EventDrain,
        EventError, EventMailbox, EventSource, Parked, Repaint, Target, WindowClient, WindowEvents,
        WindowSizing,
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

    /// One job the worker carries out, and the window it belongs to.
    ///
    /// The engine's [`Request`] says *what* is wanted; this adds what only
    /// this binary holds — the descriptor a document is read from, and which
    /// window asked. An open therefore cannot be asked for without a source
    /// to open, and an answer cannot land in a window that did not ask.
    struct Job {
        /// The window this is for.
        window: u64,
        /// What to carry out.
        work: Work,
    }

    /// The work half of a [`Job`].
    enum Work {
        /// Stream `source` into the window's sandbox and open it.
        Open {
            /// The open this answers, echoed so one the window has abandoned
            /// is dropped rather than adopted.
            open_id: u64,
            /// The document to read.
            source: Source,
        },
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
        /// End the decoders of these closed windows.
        ///
        /// A job rather than a call from the loop, because the sandboxes are
        /// the worker's and the loop may not reach them — which is the point
        /// of the worker owning its state.
        Forget(Vec<u64>),
    }

    /// What came back from one job.
    enum Reply {
        /// An answer for the window that asked.
        Window {
            /// The window that asked.
            window: u64,
            /// What came back, for that window's own engine.
            answer: Answer,
        },
        /// The decoders of closed windows have been ended.
        Forgotten,
    }

    /// What the worker keeps between jobs: **one sandbox per window**, and so
    /// each window's open document and the page it holds decoded.
    ///
    /// A view is a *session* — open once, then draw from the page held — so a
    /// sandbox must outlive one job; and a window's decode must be its own, or
    /// one document would land in another window and a malformed file would
    /// take every window's picture down with it. It lives here, reachable only
    /// from the thread carrying work out, so the loop can never touch it.
    #[derive(Default)]
    struct Session {
        sandboxes: alloc::collections::BTreeMap<u64, ParserSandbox<RtLauncher, tairix_rt::LogSink>>,
    }

    impl Session {
        /// The sandbox serving `window`, started on first use.
        ///
        /// A window that never opens a document never spawns a decoder, which
        /// is what keeps a viewer sitting on the icon bar costing nothing.
        fn sandbox(&mut self, window: u64) -> &mut ParserSandbox<RtLauncher, tairix_rt::LogSink> {
            self.sandboxes
                .entry(window)
                .or_insert_with(|| ParserSandbox::new(RtLauncher::own_binary(), tairix_rt::LogSink))
        }

        /// Drop `window`'s sandbox, ending its decoder.
        fn forget(&mut self, window: u64) {
            self.sandboxes.remove(&window);
        }
    }

    /// The worker desk: one job in flight, latest-wins, answers arriving as a
    /// wake on the loop's own wait-set.
    type Worker = tairix_rt::work::Worker<Session, Job, Reply>;

    /// Carry out one job against the window's own sandbox.
    ///
    /// The job is taken by exclusive reference so the buffer a render was lent
    /// is drawn into and handed straight back in the answer, rather than a
    /// window's worth of pixels being allocated per pointer sample.
    fn serve_job(session: &mut Session, job: &mut Job) -> Reply {
        let window = job.window;
        let answer = match &mut job.work {
            Work::Open { open_id, source } => Answer::Opened {
                open_id: *open_id,
                opened: open_source(session.sandbox(window), source),
            },
            Work::Show {
                page,
                extent,
                window: rect,
                pixels,
            } => {
                let (decoded, outcome) =
                    draw(session.sandbox(window), *page, *extent, *rect, pixels);
                Answer::Shown {
                    page: *page,
                    extent: *extent,
                    window: *rect,
                    decoded,
                    pixels: core::mem::take(pixels),
                    outcome,
                }
            }
            Work::Forget(closed) => {
                for window in closed.drain(..) {
                    session.forget(window);
                }
                return Reply::Forgotten;
            }
        };
        Reply::Window { window, answer }
    }

    /// Stream `source` into the worker and open it, answering what the
    /// container declares.
    fn open_source(
        sandbox: &mut ParserSandbox<RtLauncher, tairix_rt::LogSink>,
        source: &Source,
    ) -> Result<(ViewDocument, String, u64), Refusal> {
        // The sandbox may hold a document and everything decoded from it;
        // dropping that first is what keeps a replacement from paying for
        // both at once. A sandbox with nothing open refuses the close, which
        // is nothing to act on.
        let _ = close_view(sandbox);
        let length = upload(sandbox, &source.handle)?;
        let declared = open_view(sandbox, source.format).map_err(Refusal::Failed)?;
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
    fn upload(
        sandbox: &mut ParserSandbox<RtLauncher, tairix_rt::LogSink>,
        handle: &Handle,
    ) -> Result<u64, Refusal> {
        let fd = handle.fd();
        let length = handle.length()?;
        begin_document(sandbox, length)
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
            push_document(sandbox, &chunk[..got])
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
        sandbox: &mut ParserSandbox<RtLauncher, tairix_rt::LogSink>,
        page: u32,
        extent: (u32, u32),
        window: Rect,
        pixels: &mut Vec<u8>,
    ) -> (Option<ViewPage>, Result<(), Refusal>) {
        let decoded = match select_page(sandbox, page) {
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
            sandbox,
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

    /// A document delegated to this process: a one-shot `fd_grant` redeemed
    /// into a read-only descriptor whose reads the kernel authorises under
    /// the identity of whoever opened it.
    ///
    /// `name` is what to call it, and is empty where the hand-off did not
    /// carry one — the session's own picker, whose conclusion carries the
    /// authority and nothing else. The viewer then states what it knows and
    /// invents nothing, and a RISC OS sprite area (which no signature can
    /// identify) cannot be reached by an unnamed hand-off at all.
    fn delegated(handle: u64, name: String) -> Option<Source> {
        Some(Source {
            handle: Handle::Delegated(tairix_rt::File::from_delegation(handle).ok()?),
            format: format_for(&name),
            name,
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

    // ---- one window per document ---------------------------------------

    /// One of the viewer's windows: its channel-side state, its retained
    /// picture, and the engine that draws into it.
    ///
    /// Every window is independent of its siblings — its own document, its
    /// own decode sandbox, its own menu and title — so a malformed file
    /// refuses in the window that opened it and disturbs no other.
    struct Window {
        /// Its channel-side state: the id its events arrive under, its shared
        /// frame region, and the geometry both are shaped as.
        pane: WindowPane,
        /// The surface every frame of it is drawn into, held for the window's
        /// life so a clipped repaint leaves the pixels outside the clip alone.
        surface: Surface,
        /// The viewer looking at this window's document.
        view: View,
        /// The document this window is waiting for, once the embedder holds
        /// one: submitted when the engine asks for its open.
        pending: Option<Source>,
        /// The menu open over the window, so an outcome is matched to the
        /// gesture that asked for it rather than to whichever was last.
        menu: Option<u64>,
        /// The tooltip region last declared, so it is only sent again when it
        /// moves.
        tip: Option<Rect>,
        /// The title the session was last told, so it is only set again when
        /// the document changes.
        title: String,
        /// Whether any frame of this window has reached the session.
        presented: bool,
    }

    impl Window {
        /// Resolve the layout for the window's current extent.
        fn layout(&mut self, theme: &Theme, scale: Scale) -> Layout {
            let mode = *self.pane.mode();
            self.view.layout(
                mode.width_px,
                mode.height_px,
                theme,
                scale,
                face(theme, scale),
            )
        }

        /// Whether presenting now would put an empty window on screen.
        ///
        /// The session shows a served window on its first present, so
        /// withholding that present is withholding the window — which is what
        /// a window waiting on a pick must do, or it sits blank behind the
        /// chooser for as long as the choice takes. Once anything of the
        /// window has been on screen it is never withheld again, whatever the
        /// viewer goes on to show.
        fn withholding(&self) -> bool {
            !self.presented && self.view.nothing_to_show()
        }

        /// Paint what `repaint` owes and present it.
        ///
        /// The whole viewer is re-derived under the clip narrowed to, so a
        /// partial repaint lands the pixels a whole one would have ��� there is
        /// no second "paint just this part" recipe.
        fn present(
            &mut self,
            client: &mut WindowClient<app::RtWindowTransport>,
            repaint: Repaint,
            reported: &Region,
            theme: &Theme,
            scale: Scale,
        ) -> Result<(), Errno> {
            if self.withholding() {
                return Ok(());
            }
            let mode = *self.pane.mode();
            // Nothing of what was on screen survives where the session gave
            // its copy of the region back, or where the window has never been
            // on screen at all.
            let repaint = if self.pane.content_released() || !self.presented {
                Repaint::Whole
            } else {
                repaint
            };
            let Some(area) = present_damage(&mode, repaint, reported) else {
                return Ok(());
            };
            let layout = self.layout(theme, scale);
            let view = &self.view;
            let surface = &mut self.surface;
            surface.with_clip(area.x, area.y, area.width_px, area.height_px, |clipped| {
                tairix_view::paint::render_into(
                    clipped,
                    view,
                    &layout,
                    theme,
                    scale,
                    face(theme, scale),
                    &mut NoArtwork,
                );
            });
            let landed = self.pane.present(client, surface, area);
            if landed.is_ok() {
                self.presented = true;
            }
            landed
        }

        /// Close this window, its menu going with it.
        fn close(self, client: &mut WindowClient<app::RtWindowTransport>) {
            let _ = self.pane.close(client);
        }
    }

    /// Open a window for `source`, or for the user to pick into when there is
    /// none, answering it and the session identity it is served by.
    ///
    /// A refusal is stated and answers `None`: the viewer carries on with the
    /// windows it has, which for a resident application is the honest outcome
    /// — it is still on the icon bar and still clickable.
    fn open_window(
        client: &mut WindowClient<app::RtWindowTransport>,
        event_endpoint: u64,
        server: ProcId,
        desktop: &Desktop,
        theme: &Theme,
        scale: Scale,
        source: Option<Source>,
    ) -> Option<Window> {
        let (width, height) = desktop.window_size(WIN_WIDTH, WIN_HEIGHT);
        let mode = app::mode_for(width, height);
        let Some(surface) = Surface::new(mode.width_px, mode.height_px) else {
            report("no drawing surface; no window opened");
            return None;
        };
        // Declared in *physical* pixels, derived from the theme's metrics at
        // the desktop's own density: what the toolbar needs to keep a tool and
        // both its overflow affordances reachable, and the chrome plus a
        // strip of canvas down.
        let least = tairix_view::min_client_size(theme, scale, face(theme, scale));
        let sizing = WindowSizing::Resizable {
            min_width_px: least.0,
            min_height_px: least.1,
        };
        let (pane, replied) =
            match WindowPane::open(client, event_endpoint, &mode, APP_TITLE, sizing) {
                Ok(opened) => opened,
                Err(err) => {
                    report(&alloc::format!("{err}; no window opened"));
                    return None;
                }
            };
        // A reply from any other sender is something else answering for the
        // window endpoint: the window it named is closed rather than drawn
        // into, exactly as a popup's reply is checked.
        if replied != server {
            let _ = pane.close(client);
            report("a window reply came from another sender; no window opened");
            return None;
        }
        Some(Window {
            pane,
            surface,
            // A window handed a document asks for its open from the start; one
            // the user will pick into asks for nothing until they have chosen.
            view: View::new(source.is_some()),
            pending: source,
            menu: None,
            tip: None,
            title: String::from(APP_TITLE),
            presented: false,
        })
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
    fn set_tip(
        window: &mut Window,
        client: &mut WindowClient<app::RtWindowTransport>,
        layout: &Layout,
        at: Point,
    ) {
        let wanted = tool_tip(layout, at);
        let region = wanted.map(|(rect, _)| rect);
        if region == window.tip {
            return;
        }
        window.tip = region;
        let (rect, text) = wanted.unwrap_or((Rect::EMPTY, ""));
        let (Ok(anchor), Ok(text)) = (
            WindowRegion::new(rect.left(), rect.top(), rect.width, rect.height),
            TooltipText::new(text),
        ) else {
            return;
        };
        if client.set_tooltip(window.pane.id(), anchor, text).is_err() {
            window.tip = None;
        }
    }

    /// Tell the session what the window is showing, once, when it changes.
    fn retitle(window: &mut Window, client: &mut WindowClient<app::RtWindowTransport>) {
        let wanted = window
            .view
            .document()
            .map(|document| document.name.clone())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| String::from(APP_TITLE));
        if wanted == window.title {
            return;
        }
        if client.set_title(window.pane.id(), &wanted).is_ok() {
            window.title = wanted;
        }
    }

    /// Declare this viewer's presence on the desktop's icon bar.
    ///
    /// A refused declaration is an answer, not a death: the viewer says so
    /// and carries on with no slot of its own — its windows are still
    /// reachable, though nothing can then reach it with none open.
    fn declare_app_bar(client: &mut WindowClient<app::RtWindowTransport>, endpoint: u64) {
        match tairix_window::info_and_quit(endpoint, AppBarClick::RaiseOrOpen) {
            Ok(bar) => {
                if let Err(err) = client.set_app_bar(&bar) {
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

    /// Ask the picker for a document for `window`, reporting a session that
    /// has none.
    fn ask_for_document(window: &mut Window, client: &mut WindowClient<app::RtWindowTransport>) {
        // A refused ask is the one thing that leaves the window with nothing
        // to show and nothing coming, so it is recorded as the reason there is
        // no document: the window then appears stating it, where the stderr
        // line alone would leave a graphical launch silent.
        if let Err(err) = client.pick_file(window.pane.id()) {
            report(&alloc::format!(
                "the desktop offered no file chooser ({err}); \
                 open a document from the files app"
            ));
            window.view.no_document(Refusal::PickRefused(err));
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
        // document at spawn it opens a window on it, and launched by the user
        // it opens nothing at all and sits on the icon bar.
        let inherited_source = tairix_rt::arg(1)
            .is_some_and(|arg| arg == DOCUMENT_ROLE_ARG)
            .then(inherited);

        let mut client = WindowClient::new(app::RtWindowTransport);
        let (desktop, themes) = match app::bring_up_desktop(&mut client) {
            Ok(pair) => pair,
            Err(err) => return fail_shell(err),
        };
        let theme = themes.active();
        let scale = desktop.scale();
        // The serving session's own identity, learned by the desktop query.
        // An application that may own no window needs it from there: it is
        // what authenticates the icon-bar events it still receives, and
        // without it there is nothing to accept them against (fail closed).
        let Some(server) = client.session() else {
            return fail(app::EXIT_NO_WINDOW, "the desktop did not identify itself");
        };

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
            Session::default(),
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

        declare_app_bar(&mut client, event_endpoint);

        // The windows open, in the order they were opened. A viewer launched
        // by the user starts with none: it is resident on the icon bar, and
        // shows nothing until it has something to show.
        let mut windows: Vec<Window> = Vec::new();
        if inherited_source.is_some() {
            if let Some(opened) = open_window(
                &mut client,
                event_endpoint,
                server,
                &desktop,
                theme,
                scale,
                inherited_source,
            ) {
                windows.push(opened);
            }
        }
        // The first frame of each window is the whole of it: nothing is on
        // screen yet. A window with nothing to show withholds it, and the
        // window with it.
        for window in &mut windows {
            if window
                .present(&mut client, Repaint::Whole, &damage::sink(), theme, scale)
                .is_err()
            {
                return fail(app::EXIT_CHANNEL_LOST, "first present refused");
            }
        }

        let mut desk = Desk::default();
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
            // One that names a window this viewer has closed is dropped: the
            // window it described is gone.
            if let Some(reply) = worker.collect() {
                desk.outstanding = false;
                if let Reply::Window { window, answer } = reply {
                    if let Some(index) = index_of(&windows, window) {
                        // A document that has just opened is the one moment
                        // the window sizes itself to the picture; a render, a
                        // zoom, or a resize never does, so it cannot fight
                        // the user's own drag.
                        let just_opened = matches!(&answer, Answer::Opened { opened: Ok(_), .. });
                        let layout = windows[index].layout(theme, scale);
                        let changed = windows[index]
                            .view
                            .deliver(answer, &layout, &mut reported)
                            .changed;
                        retitle(&mut windows[index], &mut client);
                        let resized = just_opened
                            && hug_picture(
                                &mut windows[index],
                                &mut client,
                                &desktop,
                                theme,
                                scale,
                            );
                        let repaint = if resized {
                            Repaint::Whole
                        } else {
                            Repaint::Reported
                        };
                        if (changed || resized)
                            && windows[index]
                                .present(&mut client, repaint, &reported, theme, scale)
                                .is_err()
                        {
                            return fail(app::EXIT_CHANNEL_LOST, "present refused");
                        }
                    }
                }
                continue;
            }

            // Then queued input, drained before anything is painted so a
            // burst of pointer motion costs one frame rather than one each;
            // and with nothing queued, whatever the park wakes on. Both reach
            // the one routing below: a park *consumes* the event it woke on,
            // so nothing else would ever see it again.
            let delivered = match events.try_wait(&mut client) {
                Ok(None) => {
                    // Nothing queued: submit whatever any window now calls
                    // for. The answer is collected on the next turn either
                    // way — a deferred job wakes the park, and one carried
                    // out inline for want of a worker thread is already on
                    // the desk — so what `submit` reports about where it ran
                    // changes nothing here.
                    if submit_next(&mut windows, &worker, &mut desk, theme, scale) {
                        continue;
                    }

                    // Nothing to submit: arm the animation deadline — one
                    // shot, to the next frame any window actually asks for —
                    // and park. With nothing animating, nothing is armed.
                    let now = tairix_rt::clock_get();
                    deadline.set(next_deadline(&mut windows, now));
                    let woken = events.wait(&mut client);
                    // A frame may be due whether the park ended on the
                    // deadline or on an event; the render it calls for is
                    // asked for on the next turn.
                    let now = tairix_rt::clock_get();
                    for window in &mut windows {
                        // Each window's own sink: one window's rectangles are
                        // no description of another's surface.
                        let mut owed = damage::sink();
                        let layout = window.layout(theme, scale);
                        if window.view.tick(now, &layout, scale, theme, &mut owed) {
                            // A held control's step, or an animation frame,
                            // moved pixels the park was woken for.
                            let _ =
                                window.present(&mut client, Repaint::Reported, &owed, theme, scale);
                        }
                    }
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
            match route(
                &mut App {
                    windows: &mut windows,
                    client: &mut client,
                    desk: &mut desk,
                    event_endpoint,
                    server,
                    desktop: &desktop,
                },
                &event,
                theme,
                scale,
                &mut reported,
            ) {
                Routed::Quit => return 0,
                Routed::Lost => return fail(app::EXIT_CHANNEL_LOST, "present refused"),
                Routed::Served => {}
            }
        }
    }

    /// Everything routing one event may reach.
    ///
    /// Bundled because a viewer with several windows threads all of it to
    /// every arm; each field is the one copy the process holds.
    struct App<'a> {
        windows: &'a mut Vec<Window>,
        client: &'a mut WindowClient<app::RtWindowTransport>,
        desk: &'a mut Desk,
        event_endpoint: u64,
        server: ProcId,
        desktop: &'a Desktop,
    }

    /// What the loop knows about the decode desk.
    ///
    /// The desk takes one job at a time latest-wins, so submitting while one
    /// is in flight would displace a job whose answer a window is waiting
    /// for and leave that window's render outstanding for ever. Exactly one
    /// is therefore submitted at a time, and the window it is asked *for*
    /// rotates, so an animating window cannot starve another's open.
    #[derive(Default)]
    struct Desk {
        /// Whether a job is in flight.
        outstanding: bool,
        /// The window after the one last served — where the next scan starts.
        next: usize,
        /// Closed windows whose decoders have still to be ended.
        closed: Vec<u64>,
    }

    /// The index of the window `id` names, or `None` for one already closed.
    fn index_of(windows: &[Window], id: u64) -> Option<usize> {
        windows.iter().position(|open| open.pane.id() == id)
    }

    /// Submit the first request any window is asking for, answering whether
    /// one was submitted.
    ///
    /// The desk takes one job at a time latest-wins, so exactly one is
    /// submitted per turn and the next turn collects its answer and asks
    /// again — which is what keeps several windows decoding in turn rather
    /// than one starving the rest.
    fn submit_next(
        windows: &mut [Window],
        worker: &Worker,
        desk: &mut Desk,
        theme: &Theme,
        scale: Scale,
    ) -> bool {
        if desk.outstanding {
            return false;
        }
        // A closed window's decoder first: it is a whole process holding
        // memory for a window nobody can see any more.
        if !desk.closed.is_empty() {
            worker.submit(Job {
                window: 0,
                work: Work::Forget(core::mem::take(&mut desk.closed)),
            });
            desk.outstanding = true;
            return true;
        }
        let count = windows.len();
        for turn in 0..count {
            let index = (desk.next + turn) % count;
            let id = windows[index].pane.id();
            // The layout the request is derived from, at this window's own
            // current extent.
            let _ = windows[index].layout(theme, scale);
            let work = match windows[index].view.next_request() {
                // The open stays outstanding until it is answered, so this
                // arm repeats while the worker is reading. Only a source is
                // worth submitting; without one the picker's conclusion is
                // what brings it, and that arrives as an event.
                Some(Request::Open { open_id }) => windows[index]
                    .pending
                    .take()
                    .map(|source| Work::Open { open_id, source }),
                Some(Request::Show {
                    page,
                    extent,
                    window,
                    pixels,
                }) => Some(Work::Show {
                    page,
                    extent,
                    window,
                    pixels,
                }),
                None => None,
            };
            if let Some(work) = work {
                worker.submit(Job { window: id, work });
                desk.outstanding = true;
                desk.next = (index + 1) % count;
                return true;
            }
        }
        false
    }

    /// The nearest animation frame any window owes, or `None` when none is
    /// timed — in which case the park arms no timer at all.
    fn next_deadline(windows: &mut [Window], now: u64) -> Option<u64> {
        let mut nearest = None;
        for window in windows.iter_mut() {
            window.view.arm_deadline(now);
            if let Some(due) = window.view.deadline_ns() {
                nearest = Some(nearest.map_or(due, |held: u64| held.min(due)));
            }
        }
        nearest
    }

    /// What routing one event decided for the process.
    enum Routed {
        /// Carry on serving.
        Served,
        /// *Quit* was chosen: close every window and end.
        Quit,
        /// A present was refused: the channel is gone.
        Lost,
    }

    /// What routing one event decided for the window it named.
    enum Acted {
        /// Something drawn changed, at this scope.
        Changed(Repaint),
        /// This window closed. The viewer keeps its slot and carries on.
        Close,
        /// Nothing to do.
        Idle,
    }

    /// Route one delivered window event.
    ///
    /// An event naming a window this viewer no longer has is dropped: the
    /// window it addressed is gone, and there is nothing left to apply it to.
    fn route(
        app: &mut App<'_>,
        event: &WindowEvent,
        theme: &Theme,
        scale: Scale,
        reported: &mut Region,
    ) -> Routed {
        // The application-scoped events first: they name no window, and two of
        // them are the whole of a resident viewer's own lifecycle.
        match event {
            // The slot's primary click, delivered only while the viewer owns
            // no window: open one and ask what to put in it.
            WindowEvent::AppBarDefault => {
                open_and_pick(app, theme, scale);
                return Routed::Served;
            }
            WindowEvent::AppBarMenu { item } if tairix_window::is_quit(*item) => {
                for window in app.windows.drain(..) {
                    window.close(app.client);
                }
                return Routed::Quit;
            }
            // At least one document has been handed to this instance: drain
            // them, opening a window at each.
            WindowEvent::OpenRequested => {
                drain_open_targets(app, theme, scale);
                return Routed::Served;
            }
            _ => {}
        }

        let Some(id) = event.window_id() else {
            return Routed::Served;
        };
        let Some(index) = index_of(app.windows, id) else {
            return Routed::Served;
        };
        match act(app, index, event, theme, scale, reported) {
            Acted::Changed(repaint) => {
                if app.windows[index]
                    .present(app.client, repaint, reported, theme, scale)
                    .is_err()
                {
                    return Routed::Lost;
                }
            }
            Acted::Close => {
                // The viewer is not its windows: it keeps its icon-bar slot
                // with none open, and a click there opens the next. Only
                // *Quit* ends it.
                let window = app.windows.remove(index);
                app.desk.closed.push(window.pane.id());
                window.close(app.client);
            }
            Acted::Idle => {}
        }
        Routed::Served
    }

    /// Route one event to the window at `index`.
    #[allow(
        clippy::too_many_lines,
        reason = "one dispatch over the window-scoped event vocabulary; splitting it would hide the ordering"
    )]
    fn act(
        app: &mut App<'_>,
        index: usize,
        event: &WindowEvent,
        theme: &Theme,
        scale: Scale,
        reported: &mut Region,
    ) -> Acted {
        let layout = app.windows[index].layout(theme, scale);
        match event {
            WindowEvent::CloseRequested { .. } | WindowEvent::AlternateCloseRequested { .. } => {
                Acted::Close
            }
            WindowEvent::Resized {
                width_px,
                height_px,
                ..
            } => {
                let mode = app::mode_for(*width_px, *height_px);
                if !resize(&mut app.windows[index], app.client, &mode) {
                    // A refused resize leaves the old geometry standing, so
                    // the window is still drawable at the size it had.
                    report("the desktop refused a resize; the window keeps its size");
                }
                // The reported client size is what the layout follows either
                // way, so the whole window is redrawn regardless.
                Acted::Changed(Repaint::Whole)
            }
            WindowEvent::RedrawRequested { .. } => Acted::Changed(Repaint::Whole),
            WindowEvent::ContentReleased { .. } => {
                app.windows[index].pane.release_frames();
                Acted::Idle
            }
            WindowEvent::FilePicked { handle, .. } => {
                let Some(source) = delegated(*handle, String::new()) else {
                    report("the delegated document could not be redeemed");
                    return Acted::Idle;
                };
                app.windows[index].pending = Some(source);
                app.windows[index].view.expect_document();
                reported.add(layout.window());
                Acted::Changed(Repaint::Reported)
            }
            // The user chose nothing, so there is nothing to display: the
            // window they were choosing into closes rather than appearing to
            // state a refusal they already know about. A window that already
            // holds a document keeps it.
            WindowEvent::PickCancelled { .. } => {
                if app.windows[index].view.document().is_some() {
                    Acted::Idle
                } else {
                    Acted::Close
                }
            }
            WindowEvent::Key {
                key: pressed @ KeyInput::Pressed { .. },
                ..
            } => {
                let InputEvent::KeyPressed { key, modifiers } = key_input_event(*pressed) else {
                    return Acted::Idle;
                };
                let outcome = app.windows[index]
                    .view
                    .on_key(key, modifiers, &layout, reported);
                apply(app, index, outcome)
            }
            WindowEvent::Pointer { x, y, action, .. } => {
                let at = pointer_point(*x, *y);
                let mut changed = false;
                let mut asked = None;
                for input in pointer_input_events(*action, at) {
                    let outcome = app.windows[index]
                        .view
                        .on_pointer(&input, &layout, scale, theme, reported);
                    changed |= outcome.changed;
                    if outcome.pick || outcome.menu.is_some() || outcome.close {
                        asked = Some(outcome);
                    }
                }
                set_tip(&mut app.windows[index], app.client, &layout, at);
                match asked {
                    Some(outcome) => apply(app, index, outcome),
                    None if changed => Acted::Changed(Repaint::Reported),
                    None => Acted::Idle,
                }
            }
            WindowEvent::MenuClosed {
                open_id, outcome, ..
            } => {
                if app.windows[index].menu != Some(*open_id) {
                    // An answer to a gesture another open has superseded.
                    return Acted::Idle;
                }
                app.windows[index].menu = None;
                let MenuOutcome::Chosen(item) = outcome else {
                    return Acted::Idle;
                };
                let Some(command) = menu_command(*item) else {
                    return Acted::Idle;
                };
                let outcome = app.windows[index].view.run(command, &layout, reported);
                apply(app, index, outcome)
            }
            _ => Acted::Idle,
        }
    }

    /// Shrink `window` to hug the picture it has just opened, answering
    /// whether its geometry moved.
    ///
    /// Shrink-only and once per document: the viewer asks for the client
    /// whose canvas is exactly the picture's own pixels, capped at the window
    /// it opens at and floored at the smallest client it lays out for, so a
    /// small picture is shown at 100% without a frame of empty canvas round
    /// it and a photograph keeps the default window. A refused re-map leaves
    /// the window at the size it had.
    fn hug_picture(
        window: &mut Window,
        client: &mut WindowClient<app::RtWindowTransport>,
        desktop: &Desktop,
        theme: &Theme,
        scale: Scale,
    ) -> bool {
        let Some((want_w, want_h)) =
            window
                .view
                .preferred_client_size(theme, scale, face(theme, scale))
        else {
            return false;
        };
        // Never larger than the display the window has to appear on.
        let screen = desktop.screen();
        let mode = app::mode_for(want_w.min(screen.width), want_h.min(screen.height));
        let held = window.pane.mode();
        if held.width_px == mode.width_px && held.height_px == mode.height_px {
            return false;
        }
        resize(window, client, &mode)
    }

    /// Re-map the window's frame region and its retained surface onto `mode`,
    /// answering whether the new geometry was adopted.
    ///
    /// The fresh surface is allocated before the session is asked and adopted
    /// only once it has accepted, so every refusal leaves the window at the
    /// size it had and still drawable.
    fn resize(
        window: &mut Window,
        client: &mut WindowClient<app::RtWindowTransport>,
        mode: &tairix_abi::driver::display::DisplayMode,
    ) -> bool {
        let Some(surface) = Surface::new(mode.width_px, mode.height_px) else {
            return false;
        };
        if !window.pane.resize(client, mode) {
            return false;
        }
        window.surface = surface;
        true
    }

    /// Carry out whatever an engine outcome asked the embedder for.
    fn apply(app: &mut App<'_>, index: usize, outcome: Outcome) -> Acted {
        if outcome.close {
            return Acted::Close;
        }
        if outcome.pick {
            ask_for_document(&mut app.windows[index], app.client);
        }
        if let Some(at) = outcome.menu {
            open_menu(&mut app.windows[index], app.client, at);
        }
        if outcome.changed {
            Acted::Changed(Repaint::Reported)
        } else {
            Acted::Idle
        }
    }

    /// Open a window with nothing in it and ask the picker what to put there.
    ///
    /// What the icon-bar slot's primary click means. Its present is withheld
    /// until there is something to show, so the window appears with the
    /// document in it rather than sitting blank behind the chooser.
    fn open_and_pick(app: &mut App<'_>, theme: &Theme, scale: Scale) {
        let Some(mut opened) = open_window(
            app.client,
            app.event_endpoint,
            app.server,
            app.desktop,
            theme,
            scale,
            None,
        ) else {
            return;
        };
        ask_for_document(&mut opened, app.client);
        let _ = opened.present(app.client, Repaint::Whole, &damage::sink(), theme, scale);
        app.windows.push(opened);
    }

    /// Drain every document the desktop has handed to this instance, opening
    /// a window at each.
    ///
    /// How a launch that names a document reaches an instance already
    /// running: the desktop relays the authority to this process and wakes
    /// it, rather than starting a second viewer.
    fn drain_open_targets(app: &mut App<'_>, theme: &Theme, scale: Scale) {
        loop {
            let target = match app.client.take_open_target() {
                Ok(Some(target)) => target,
                Ok(None) => return,
                Err(err) => {
                    report(&alloc::format!("cannot take an open target ({err})"));
                    return;
                }
            };
            let source = match target {
                Target::Document { name, grant } => {
                    if let Some(source) = delegated(grant, name) {
                        source
                    } else {
                        report("a handed-over document could not be redeemed");
                        continue;
                    }
                }
                // Honest, and unreachable from the desktop, which hands this
                // viewer a descriptor precisely because it holds no
                // filesystem authority to open a name with.
                Target::Path(path) => {
                    report(&alloc::format!(
                        "{path} was handed over as a path; this viewer holds no filesystem \
                         authority and can only be given an open document"
                    ));
                    continue;
                }
            };
            let Some(mut opened) = open_window(
                app.client,
                app.event_endpoint,
                app.server,
                app.desktop,
                theme,
                scale,
                Some(source),
            ) else {
                continue;
            };
            let _ = opened.present(app.client, Repaint::Whole, &damage::sink(), theme, scale);
            app.windows.push(opened);
        }
    }

    /// Ask the session to open the app's own menu at `at`.
    ///
    /// The plate is the session's — the app draws no menu pixel — and a
    /// session that composes none is reported and carried on from.
    fn open_menu(
        window: &mut Window,
        client: &mut WindowClient<app::RtWindowTransport>,
        at: Point,
    ) {
        let (menu, skipped) = build_menu(&window.view);
        if skipped > 0 {
            report(&alloc::format!(
                "{skipped} menu row(s) do not fit and are not shown"
            ));
        }
        let Ok(anchor) = WindowRegion::new(at.x, at.y, 0, 0) else {
            return;
        };
        match client.open_menu(window.pane.id(), anchor, &menu) {
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
