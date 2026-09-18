//! The composed viewer: the controls, the one input entry point, and the
//! request/answer desk a picture arrives through.
//!
//! Nothing here reads, writes, or decodes anything. A command changes the
//! state it names and nothing else; a paint draws what has already arrived;
//! and the render the state calls for is *recorded* for `Run` to carry out on
//! its worker. That is what keeps the window answering while a slow store or
//! a large decode is in progress.

use alloc::string::String;
use alloc::vec::Vec;

use tairix_abi::Errno;
use tairix_controls::{
    ControlRole, IconButton, ScrollAction, ScrollBar, ScrollModel, ScrollOrientation, ScrollRange,
    Slider, SliderAction, Toolbar, ToolbarAction, ToolbarOutcome, REPEAT_DELAY_NS,
    REPEAT_INTERVAL_NS,
};
use tairix_font::BitmapFont;
use tairix_geometry::{Point, Rect, Region, Scale};
use tairix_icon::IconKind;
use tairix_input::{InputEvent, Key, Modifiers, NamedKey, PointerButton};
use tairix_raster::{Reorient, Surface};
use tairix_sandbox::imagerender::{ViewDocument, ViewFailure, ViewPage};
use tairix_theme::Theme;

use crate::layout::TOOL_COUNT;
use crate::{
    fitted_zoom, slider_at_zoom, window_in_page_space, zoom_at_slider, zoom_rung_above,
    zoom_rung_below, Answer, Document, Fit, Layout, Picture, Request, Viewport,
    ZOOM_ACTUAL_PER_MILLE, ZOOM_SLIDER_LINE_STEP, ZOOM_SLIDER_PAGE_STEP,
};

/// The toolbar's tools, in the order they are drawn.
///
/// One ordered list, so the glyph a tool draws, the tooltip it carries, the
/// command it runs, and the index a click reports are all the same position —
/// a toolbar whose glyphs and actions could be listed separately is one that
/// can be wired up wrong.
pub const TOOLS: [(IconKind, Command, &str); TOOL_COUNT] = [
    (IconKind::ZoomOut, Command::ZoomOut, "Zoom out"),
    (IconKind::ZoomIn, Command::ZoomIn, "Zoom in"),
    (IconKind::ZoomFit, Command::FitWindow, "Fit in window"),
    (IconKind::ZoomActual, Command::ActualSize, "Actual size"),
    (IconKind::NavBack, Command::PreviousPage, "Previous"),
    (IconKind::NavForward, Command::NextPage, "Next"),
    (IconKind::RotateLeft, Command::RotateLeft, "Rotate left"),
    (IconKind::RotateRight, Command::RotateRight, "Rotate right"),
    (IconKind::Mirror, Command::Mirror, "Mirror"),
    (IconKind::Resume, Command::TogglePlayback, "Play"),
    (IconKind::Info, Command::ToggleInfo, "Information"),
];

/// The position of the playback tool, whose glyph is the one that changes
/// with the state it reports.
const PLAYBACK_TOOL: usize = 9;

/// What the viewer can be asked to do, however it was asked.
///
/// The toolbar, the keyboard, the app-declared menu and the context menu all
/// resolve to one of these and run it through [`View::run`], so a command
/// cannot behave differently depending on which surface invoked it.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Command {
    /// Magnify to the next rung of the ladder.
    ZoomIn,
    /// Reduce to the previous rung of the ladder.
    ZoomOut,
    /// Fit the whole picture in the canvas.
    FitWindow,
    /// Fit the picture's width across the canvas.
    FitWidth,
    /// One picture pixel per screen pixel.
    ActualSize,
    /// Turn a quarter turn anticlockwise.
    RotateLeft,
    /// Turn a quarter turn clockwise.
    RotateRight,
    /// Mirror left to right.
    Mirror,
    /// Show the container's previous entry.
    PreviousPage,
    /// Show the container's next entry.
    NextPage,
    /// Show the container's first entry.
    FirstPage,
    /// Show the container's last entry.
    LastPage,
    /// Start or stop an animation.
    TogglePlayback,
    /// Show or hide the information panel.
    ToggleInfo,
    /// Pan by one step in each named direction.
    Pan {
        /// Steps toward the trailing edge.
        dx: i32,
        /// Steps downward.
        dy: i32,
    },
    /// Ask the session's trusted picker for a document.
    OpenDocument,
}

/// What the viewer wants its embedder to do about an input event, beyond
/// repainting what changed.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct Outcome {
    /// Whether anything drawn changed, so the embedder presents what the
    /// controls and the engine reported.
    ///
    /// A change that reshapes the bands reports the window's own rectangle,
    /// so there is nothing further for an embedder to decide: what was
    /// reported is always what to present.
    pub changed: bool,
    /// The viewer is asking for the session's trusted file picker.
    pub pick: bool,
    /// The viewer is asking for its context menu at this window position.
    pub menu: Option<Point>,
    /// The user asked to close this window.
    pub close: bool,
}

impl Outcome {
    /// An outcome that repaints what was reported.
    #[must_use]
    pub const fn changed(changed: bool) -> Self {
        Self {
            changed,
            pick: false,
            menu: None,
            close: false,
        }
    }

    /// An outcome asking for the session's trusted picker.
    #[must_use]
    pub const fn picking() -> Self {
        Self {
            changed: false,
            pick: true,
            menu: None,
            close: false,
        }
    }
}

/// Why the canvas is showing a reason instead of a picture.
///
/// A viewer never blanks and never fabricates: whatever went wrong, the user
/// is told what it was, in the window and on the standard error stream.
///
/// The decoder's own refusals arrive wrapped in [`Failed`](Self::Failed);
/// the rest are this side of the seam, because reading the file and holding
/// the pixels are the embedder's job and the worker knows nothing of either.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Refusal {
    /// The worker refused the document, or a page of it, or the sandbox
    /// itself failed.
    Failed(ViewFailure),
    /// The document could not be read at all: the descriptor refused, or the
    /// file is shorter than it measured.
    Unreadable(Errno),
    /// The document is longer than a viewer will hold resident.
    TooLong,
    /// The session would not open a file chooser at all, so there was never
    /// anything to choose from.
    PickRefused(Errno),
    /// The document's pixels arrived but could not be held.
    Unholdable,
}

impl core::fmt::Display for Refusal {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Failed(inner) => write!(f, "{inner}"),
            Self::Unreadable(err) => write!(f, "the document could not be read ({err})"),
            Self::TooLong => f.write_str("the document is larger than this viewer opens"),
            Self::PickRefused(err) => {
                write!(f, "the desktop offered no file chooser ({err})")
            }
            Self::Unholdable => f.write_str("the picture could not be held in memory"),
        }
    }
}

/// What a render was asked for, so an answer that a newer one has superseded
/// is recognised and dropped rather than drawn.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
struct Shape {
    page: u32,
    extent: (u32, u32),
    window: Rect,
}

/// The id of a viewer's first open. Zero names no open at all.
const FIRST_OPEN_ID: u64 = 1;

/// What the viewer is waiting for.
///
/// One value rather than an "opening" flag beside an in-flight render, so the
/// two can never both be set: a render is meaningless before the document is
/// open, and this makes that unrepresentable rather than merely avoided.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum Pending {
    /// The document with this open id has still to be opened.
    ///
    /// The id is echoed back in the answer, so an open the viewer has since
    /// abandoned — a window closed with a read in flight, then a document
    /// asked for in another — is recognised and dropped rather than landing
    /// in whichever window is waiting now. It is the same rule a render's
    /// echoed shape already keeps.
    Open(u64),
    /// Nothing is outstanding.
    Idle,
    /// This render has been asked for and not yet answered.
    Show(Shape),
}

/// The viewer.
pub struct View {
    /// The document open, once one is.
    document: Option<Document>,
    /// Where the viewer is looking.
    viewport: Viewport,
    /// Why there is no picture, when there is none.
    refusal: Option<Refusal>,
    /// The picture the canvas draws.
    picture: Picture,
    /// The pixel buffer lent to the worker and handed back, so a pan costs no
    /// allocation once the geometry has settled.
    scratch: Option<Vec<u8>>,
    /// What the viewer is waiting for.
    pending: Pending,
    /// How many opens this viewer has asked for; the next id is one above.
    minted_opens: u64,
    /// The toolbar's tools.
    toolbar: Toolbar,
    /// The zoom slider.
    zoom: Slider,
    /// The canvas's vertical scrollbar.
    vertical: ScrollBar,
    /// The canvas's horizontal scrollbar.
    horizontal: ScrollBar,
    /// Whether the information panel is open.
    info: bool,
    /// Whether an animation is playing.
    playing: bool,
    /// When the next animation frame is due, in monotonic nanoseconds.
    frame_ns: Option<u64>,
    /// When a held control's next auto-repeat step is due, in monotonic
    /// nanoseconds. The toolbar's overflow affordances and both scrollbars
    /// are driven by this one deadline, so two held controls in one window
    /// cannot step at different rates.
    repeat_ns: Option<u64>,
    /// The canvas the last layout resolved, so a command that refits has the
    /// geometry to refit against without being handed one.
    canvas: (u32, u32),
    /// Where a pan drag was last sampled, while one is in progress.
    drag: Option<Point>,
    /// The latest pointer position, for the context menu and hit-testing.
    pointer: Point,
}

impl View {
    /// A viewer with nothing open, waiting for its document.
    ///
    /// `opening` is whether a document is on its way — inherited at spawn or
    /// named on the command line. A viewer launched with none asks the
    /// session's picker instead, which is the embedder's call, so it starts
    /// with neither a document nor a refusal to state.
    #[must_use]
    pub fn new(opening: bool) -> Self {
        let toolbar = tools();
        Self {
            document: None,
            viewport: Viewport::new(),
            refusal: None,
            picture: Picture::default(),
            scratch: None,
            pending: if opening {
                Pending::Open(FIRST_OPEN_ID)
            } else {
                Pending::Idle
            },
            // A viewer handed a document at spawn has already minted its
            // first open; one that will ask for its own has minted none.
            minted_opens: u64::from(opening),
            toolbar,
            zoom: Slider::new(slider_at_zoom(crate::ZOOM_ACTUAL_PER_MILLE))
                .with_steps(ZOOM_SLIDER_LINE_STEP, ZOOM_SLIDER_PAGE_STEP),
            vertical: ScrollBar::new(ScrollOrientation::Vertical, empty_scroll()),
            horizontal: ScrollBar::new(ScrollOrientation::Horizontal, empty_scroll()),
            info: false,
            playing: false,
            frame_ns: None,
            repeat_ns: None,
            canvas: (0, 0),
            drag: None,
            pointer: Point::ORIGIN,
        }
    }

    /// Whether the viewer has nothing of its own to show yet.
    ///
    /// True while a window is still waiting for its document, however it was
    /// launched: until the document decodes or is refused the canvas is
    /// empty. A served window is shown by its first present, so an embedder
    /// asks this before presenting rather than putting an empty window on
    /// screen — which for a viewer waiting on the picker leaves it blank
    /// behind the chooser, and for one handed a document puts it on screen at
    /// a size the picture has not been measured against yet, so sizing the
    /// window to the picture reads as a flash. Either conclusion ends it: the
    /// document, or the reason there is none.
    #[must_use]
    pub const fn nothing_to_show(&self) -> bool {
        self.document.is_none() && self.refusal.is_none()
    }

    /// The document open, if one is.
    #[must_use]
    pub const fn document(&self) -> Option<&Document> {
        self.document.as_ref()
    }

    /// Where the viewer is looking.
    #[must_use]
    pub const fn viewport(&self) -> &Viewport {
        &self.viewport
    }

    /// Why the canvas is showing a reason instead of a picture.
    #[must_use]
    pub const fn refusal(&self) -> Option<&Refusal> {
        self.refusal.as_ref()
    }

    /// A document is on its way: ask for it, and show that until it lands.
    ///
    /// What the embedder calls once it holds a source — a descriptor
    /// inherited at spawn, or one the user just chose in the session's
    /// picker. No render is asked for until the open is answered, so a
    /// document being replaced costs no draw of the one it replaces.
    ///
    /// Answers the **open id** the embedder carries on the request and back
    /// on the answer. Minted per ask and never reused, so an answer to an
    /// open this viewer has abandoned is dropped rather than adopted.
    pub fn expect_document(&mut self) -> u64 {
        self.minted_opens = self.minted_opens.saturating_add(1);
        self.pending = Pending::Open(self.minted_opens);
        self.refusal = None;
        self.minted_opens
    }

    /// Whether the information panel is open.
    #[must_use]
    pub const fn info_open(&self) -> bool {
        self.info
    }

    /// Whether an animation is playing.
    #[must_use]
    pub const fn playing(&self) -> bool {
        self.playing
    }

    /// When this viewer next needs waking, in monotonic nanoseconds, or
    /// `None` when nothing is timed: the nearer of the next animation frame
    /// and the next auto-repeat step of a held control.
    ///
    /// What the embedder's park deadline is set from, so a paused viewer with
    /// nothing held arms no timer at all and one with either arms exactly
    /// one.
    #[must_use]
    pub fn deadline_ns(&self) -> Option<u64> {
        match (self.frame_ns, self.repeat_ns) {
            (Some(frame), Some(repeat)) => Some(frame.min(repeat)),
            (due, None) | (None, due) => due,
        }
    }

    /// The zoom slider, for the painter.
    #[must_use]
    pub(crate) const fn zoom_control(&self) -> &Slider {
        &self.zoom
    }

    /// The toolbar, for the painter.
    #[must_use]
    pub(crate) const fn toolbar_control(&self) -> &Toolbar {
        &self.toolbar
    }

    /// The canvas's two scrollbars, for the painter.
    #[must_use]
    pub(crate) const fn bars(&self) -> (&ScrollBar, &ScrollBar) {
        (&self.vertical, &self.horizontal)
    }

    /// The picture the canvas draws, or `None` before one has arrived.
    #[must_use]
    pub(crate) fn picture(&self) -> Option<&Surface> {
        self.picture.surface()
    }

    /// Resolve the layout for a `width`×`height` client area and adopt the
    /// canvas it gives, refitting a fitted zoom and reclamping the pan.
    ///
    /// The one place the layout is resolved, so what was painted and what a
    /// click is tested against are the same geometry.
    pub fn layout(
        &mut self,
        width: u32,
        height: u32,
        theme: &Theme,
        scale: Scale,
        font: BitmapFont,
    ) -> Layout {
        let layout = Layout::for_window(
            width,
            height,
            theme,
            scale,
            font,
            self.toolbar.natural_width(scale, theme),
            self.info,
        );
        let canvas = (layout.canvas().width, layout.canvas().height);
        if canvas != self.canvas {
            self.canvas = canvas;
            if let Some(natural) = self.natural() {
                self.viewport.refit(natural, canvas);
                self.viewport.cap_zoom(natural);
                self.viewport.clamp_pan(natural, canvas);
            }
            self.sync_controls();
        }
        layout
    }

    /// The client size whose canvas is exactly the selected page's own pixels,
    /// or `None` where the window should keep the size it has.
    ///
    /// Only a picture the window can already hold has a preference: one
    /// zoomed out to fit keeps its window, because hugging one axis of it
    /// would only leave the fitted picture smaller. **It therefore only ever
    /// shrinks** — a picture that fits is no larger than the canvas, and the
    /// canvas-to-client mapping only grows with its argument, so what this
    /// asks for is never wider or taller than the window already is. Floored
    /// at the smallest client the viewer is laid out for, which the window
    /// manager already holds every window to, so the floor cannot grow one
    /// either.
    #[must_use]
    pub fn preferred_client_size(
        &self,
        theme: &Theme,
        scale: Scale,
        font: BitmapFont,
    ) -> Option<(u32, u32)> {
        if !self.fits_canvas() {
            return None;
        }
        let natural = self.natural()?;
        let (want_w, want_h) = Layout::client_for_canvas(natural, theme, scale, font, self.info);
        let (floor_w, floor_h) =
            Layout::min_client(theme, scale, font, self.toolbar.min_width(scale, theme));
        Some((want_w.max(floor_w), want_h.max(floor_h)))
    }

    /// Whether the picture fits the canvas at its own size.
    ///
    /// Asked through the fitted zoom the engine already computes rather than
    /// a second comparison of extents, so what "fits" means and what a fit
    /// resolves to cannot disagree: a picture needing no shrinking fits at or
    /// above actual size. A page container reports its *largest* page until
    /// one decodes, so deciding on that covers every page it holds.
    fn fits_canvas(&self) -> bool {
        self.natural()
            .and_then(|natural| fitted_zoom(Fit::Window, natural, self.canvas))
            .is_some_and(|fitted| fitted >= ZOOM_ACTUAL_PER_MILLE)
    }

    /// The selected page's natural pixel size, or `None` with nothing open.
    fn natural(&self) -> Option<(u32, u32)> {
        self.document.as_ref().map(Document::natural)
    }

    /// Run one command, reporting what changed and what it is worth
    /// repainting.
    ///
    /// Each command touches the state its own name covers and nothing else: a
    /// zoom does not drop the page, a page change does not reset the turn, and
    /// a panel toggle re-derives only the layout. Re-deriving state a change
    /// does not name is not merely wasted work — it is wrong behaviour.
    ///
    /// The damage a command owes is where it was drawn, which only the layout
    /// knows, so the layout is passed in rather than a scope being guessed
    /// here.
    pub fn run(&mut self, command: Command, layout: &Layout, damage: &mut Region) -> Outcome {
        match command {
            Command::OpenDocument => return Outcome::picking(),
            // A panel opening or closing reshapes every band, so the scope
            // genuinely is the window.
            Command::ToggleInfo => {
                self.info = !self.info;
                damage.add(layout.window());
                return Outcome::changed(true);
            }
            _ => {}
        }
        let Some(natural) = self.natural() else {
            return Outcome::changed(false);
        };
        let canvas = self.canvas;
        let changed = match command {
            // Handled above, before the picture was needed.
            Command::OpenDocument | Command::ToggleInfo => false,
            Command::ZoomIn => self.rezoom(zoom_rung_above(self.viewport.zoom()), natural, canvas),
            Command::ZoomOut => self.rezoom(zoom_rung_below(self.viewport.zoom()), natural, canvas),
            Command::FitWindow => self.refit(Fit::Window, natural, canvas),
            Command::FitWidth => self.refit(Fit::Width, natural, canvas),
            Command::ActualSize => self.refit(Fit::Actual, natural, canvas),
            Command::RotateLeft => self.turn(Reorient::QuarterTurnLeft, natural, canvas),
            Command::RotateRight => self.turn(Reorient::QuarterTurnRight, natural, canvas),
            Command::Mirror => self.turn(Reorient::FlipHorizontal, natural, canvas),
            Command::PreviousPage => self.step_page(-1),
            Command::NextPage => self.step_page(1),
            Command::FirstPage => self.go_to_page(0),
            Command::LastPage => self.go_to_page(self.entries().saturating_sub(1)),
            Command::TogglePlayback => self.toggle_playback(),
            Command::Pan { dx, dy } => {
                let before = self.viewport.pan();
                self.viewport.pan_steps(dx, dy, natural, canvas);
                let moved = self.viewport.pan() != before;
                if moved {
                    self.sync_controls();
                }
                moved
            }
        };
        if changed {
            self.report(command, layout, damage);
        }
        Outcome::changed(changed)
    }

    /// Report where a command's change was drawn.
    ///
    /// Scoped to what the command actually invalidates: a pan redraws the
    /// canvas and the chrome that reports where it is, a zoom additionally
    /// redraws the slider, and playback redraws the tool whose glyph is its
    /// own state. Nothing here reports the whole window, because none of
    /// these changes reshapes it.
    fn report(&self, command: Command, layout: &Layout, damage: &mut Region) {
        damage.add(layout.canvas());
        damage.add(layout.status());
        damage.add(layout.vertical_bar());
        damage.add(layout.horizontal_bar());
        match command {
            Command::ZoomIn
            | Command::ZoomOut
            | Command::FitWindow
            | Command::FitWidth
            | Command::ActualSize => damage.add(layout.zoom_slider()),
            Command::TogglePlayback => damage.add(layout.tools()),
            _ => {}
        }
        if self.info {
            damage.add(layout.info());
        }
    }

    /// How many entries the open container holds.
    fn entries(&self) -> u32 {
        self.document
            .as_ref()
            .map_or(0, |document| document.info.count)
    }

    /// Adopt a zoom the user asked for.
    fn rezoom(&mut self, per_mille: u32, natural: (u32, u32), canvas: (u32, u32)) -> bool {
        let before = self.viewport.zoom();
        self.viewport.set_zoom(per_mille);
        self.viewport.cap_zoom(natural);
        if self.viewport.zoom() == before {
            return false;
        }
        self.viewport.clamp_pan(natural, canvas);
        self.sync_controls();
        true
    }

    /// Adopt a fit mode.
    fn refit(&mut self, fit: Fit, natural: (u32, u32), canvas: (u32, u32)) -> bool {
        let before = (self.viewport.fit, self.viewport.zoom());
        self.viewport.set_fit(fit, natural, canvas);
        self.viewport.cap_zoom(natural);
        self.viewport.clamp_pan(natural, canvas);
        let moved = (self.viewport.fit, self.viewport.zoom()) != before;
        if moved {
            self.sync_controls();
        }
        moved
    }

    /// Turn the picture, composing onto the turn already in effect.
    fn turn(&mut self, by: Reorient, natural: (u32, u32), canvas: (u32, u32)) -> bool {
        self.viewport.turn(by, natural, canvas);
        self.viewport.cap_zoom(natural);
        self.sync_controls();
        true
    }

    /// Move `by` entries through the container, stopping at either end.
    fn step_page(&mut self, by: i32) -> bool {
        let Some(document) = self.document.as_ref() else {
            return false;
        };
        let last = document.info.count.saturating_sub(1);
        let at = i64::from(document.index()).saturating_add(i64::from(by));
        let next = u32::try_from(at.clamp(0, i64::from(last))).unwrap_or(0);
        self.go_to_page(next)
    }

    /// Show the entry at `index`.
    ///
    /// The turn, the fit and the zoom are the user's and survive a page
    /// change; the pan does not, because an offset into one picture names
    /// nothing in another. What is on the canvas stays drawn until the new
    /// entry arrives — a viewer never blanks — and the render this makes the
    /// state call for is what replaces it.
    fn go_to_page(&mut self, index: u32) -> bool {
        let Some(document) = self.document.as_mut() else {
            return false;
        };
        if !document.select(index) {
            return false;
        }
        self.viewport.reset_pan();
        self.sync_controls();
        true
    }

    /// Start or stop playback, arming or disarming the frame deadline.
    fn toggle_playback(&mut self) -> bool {
        let Some(document) = self.document.as_ref() else {
            return false;
        };
        if !document.info.animated {
            return false;
        }
        self.playing = !self.playing;
        if !self.playing {
            self.frame_ns = None;
        }
        if let Some(button) = self.toolbar.icon_mut(PLAYBACK_TOOL) {
            *button = IconButton::new(
                if self.playing {
                    IconKind::Pause
                } else {
                    IconKind::Resume
                },
                ControlRole::Neutral,
            );
        }
        true
    }

    /// Bring the zoom slider and the two scrollbars into line with the
    /// viewport.
    ///
    /// The controls are a *view* of the viewport, never a second copy of it:
    /// this is the only place they are written, so a slider can never report
    /// a zoom the viewer is not at.
    fn sync_controls(&mut self) {
        self.zoom.set_value(slider_at_zoom(self.viewport.zoom()));
        let Some(natural) = self.natural() else {
            self.vertical.set_model(empty_scroll());
            self.horizontal.set_model(empty_scroll());
            return;
        };
        let scaled = self.viewport.scaled(natural);
        let pan = self.viewport.pan();
        self.vertical
            .set_model(scroll_for(scaled.1, self.canvas.1, pan.1));
        self.horizontal
            .set_model(scroll_for(scaled.0, self.canvas.0, pan.0));
    }

    /// Advance whatever is due at `now_ns` — an animation frame, and one
    /// auto-repeat step of any held control — reporting whether anything
    /// drawn changed.
    ///
    /// Tickless by construction: the deadline is a single instant the
    /// embedder parks until, and a paused viewer with nothing held has none
    /// at all.
    pub fn tick(
        &mut self,
        now_ns: u64,
        layout: &Layout,
        scale: Scale,
        theme: &Theme,
        damage: &mut Region,
    ) -> bool {
        self.repeat(now_ns, layout, scale, theme, damage) | self.next_frame(now_ns)
    }

    /// Step the animation if its frame is due.
    fn next_frame(&mut self, now_ns: u64) -> bool {
        if !self.playing {
            return false;
        }
        let Some(deadline) = self.frame_ns else {
            return false;
        };
        if now_ns < deadline {
            return false;
        }
        self.frame_ns = None;
        let Some(document) = self.document.as_ref() else {
            return false;
        };
        let next = document.index().saturating_add(1) % document.info.count.max(1);
        // A one-frame animation stays where it is rather than re-selecting
        // itself, so a container declaring one frame arms no further work.
        self.go_to_page(next)
    }

    /// Step every held control once if their shared repeat is due, and re-arm
    /// it at the interval while anything is still held.
    fn repeat(
        &mut self,
        now_ns: u64,
        layout: &Layout,
        scale: Scale,
        theme: &Theme,
        damage: &mut Region,
    ) -> bool {
        let Some(due) = self.repeat_ns else {
            return false;
        };
        if now_ns < due {
            return false;
        }
        self.repeat_ns = None;
        let mut changed = self.toolbar.repeat(layout.tools(), scale, theme, damage);
        if let Some(ScrollAction::ScrollTo { offset }) =
            self.vertical.repeat(layout.vertical_bar(), damage)
        {
            changed |= self.pan_to(None, Some(offset));
        }
        if let Some(ScrollAction::ScrollTo { offset }) =
            self.horizontal.repeat(layout.horizontal_bar(), damage)
        {
            changed |= self.pan_to(Some(offset), None);
        }
        if changed {
            damage.add(layout.canvas());
            damage.add(layout.status());
        }
        self.repeat_ns = self
            .holding()
            .then(|| now_ns.saturating_add(REPEAT_INTERVAL_NS));
        changed
    }

    /// Whether any control is holding a press that wants repeat wake-ups.
    fn holding(&self) -> bool {
        self.toolbar.is_repeating()
            || self.vertical.is_repeating()
            || self.horizontal.is_repeating()
    }

    /// The render the current state calls for, or `None` when what is held is
    /// already it.
    ///
    /// Handing the buffer over is what makes an interactive re-render cost no
    /// allocation: it comes back in the answer, whether the draw succeeded or
    /// not.
    pub fn next_request(&mut self) -> Option<Request> {
        match self.pending {
            // Left outstanding rather than handed over once: only the
            // embedder knows whether it holds a document to open yet, and
            // until the open is answered no render describes anything the
            // user is looking at.
            Pending::Open(open_id) => return Some(Request::Open { open_id }),
            Pending::Show(_) => return None,
            Pending::Idle => {}
        }
        let shape = self.wanted_shape()?;
        if self.holds(shape) {
            return None;
        }
        self.pending = Pending::Show(shape);
        Some(Request::Show {
            page: shape.page,
            extent: shape.extent,
            window: shape.window,
            pixels: self.scratch.take().unwrap_or_default(),
        })
    }

    /// The render the current state calls for, or `None` when the state calls
    /// for none — nothing open, or a canvas with no pixels.
    ///
    /// The one derivation, so what is asked for and what an answer is checked
    /// against cannot be computed two ways.
    fn wanted_shape(&self) -> Option<Shape> {
        let natural = self.natural()?;
        let page = self.document.as_ref()?.index();
        let visible = self.viewport.visible(natural, self.canvas);
        if visible.is_empty() {
            return None;
        }
        Some(Shape {
            page,
            extent: self.viewport.page_extent(natural),
            window: window_in_page_space(
                visible,
                self.viewport.scaled(natural),
                self.viewport.reorient,
            ),
        })
    }

    /// Whether the picture held is already the render `shape` describes.
    fn holds(&self, shape: Shape) -> bool {
        self.document.as_ref().is_some_and(Document::decoded)
            && self.picture.surface().is_some()
            && self.picture.window == shape.window
            && self.picture.extent == shape.extent
            && self.picture.reorient == self.viewport.reorient
    }

    /// Adopt an answer, reporting what changed and what is worth repainting.
    ///
    /// A document that has just opened reshapes the window's title, its facts
    /// and its chrome all at once, so that one is the whole surface; a drawn
    /// window is the canvas and the chrome that describes it.
    pub fn deliver(&mut self, answer: Answer, layout: &Layout, damage: &mut Region) -> Outcome {
        match answer {
            Answer::Opened { open_id, opened } => {
                if !self.opened(open_id, opened) {
                    return Outcome::changed(false);
                }
                // A document that has just opened, or refused to, changes the
                // title, the facts, the chrome and the canvas together.
                damage.add(layout.window());
                Outcome::changed(true)
            }
            Answer::Shown {
                page,
                extent,
                window,
                decoded,
                pixels,
                outcome,
            } => {
                let changed = self.shown(page, extent, window, decoded, pixels, outcome);
                if changed {
                    damage.add(layout.canvas());
                    damage.add(layout.status());
                    damage.add(layout.vertical_bar());
                    damage.add(layout.horizontal_bar());
                    damage.add(layout.zoom_slider());
                    if self.info {
                        damage.add(layout.info());
                    }
                }
                Outcome::changed(changed)
            }
        }
    }

    /// Adopt an open, or the reason there is not one.
    fn opened(
        &mut self,
        open_id: u64,
        opened: Result<(ViewDocument, String, u64), Refusal>,
    ) -> bool {
        // An answer to an open this viewer has abandoned describes a document
        // nobody is waiting for; adopting it would put one window's document
        // in another.
        if self.pending != Pending::Open(open_id) {
            return false;
        }
        self.pending = Pending::Idle;
        match opened {
            Ok((info, name, bytes)) => {
                self.document = Some(Document::new(info, name, bytes));
                self.refusal = None;
                self.viewport = Viewport::new();
                if let Some(natural) = self.natural() {
                    // A picture the window can hold opens at the size it was
                    // authored at; one too big for it is zoomed out to fit,
                    // because opening part-shown hides the picture behind its
                    // own corner.
                    let fit = if self.fits_canvas() {
                        Fit::Actual
                    } else {
                        Fit::Window
                    };
                    self.viewport.set_fit(fit, natural, self.canvas);
                    self.viewport.cap_zoom(natural);
                }
                self.sync_controls();
            }
            Err(refusal) => {
                self.document = None;
                self.refusal = Some(refusal);
                self.picture = Picture::default();
                self.sync_controls();
            }
        }
        true
    }

    /// Adopt a drawn window, or the reason there is not one.
    #[allow(
        clippy::too_many_arguments,
        reason = "one echoed render request plus its pixels and its outcome; bundling them into a struct would be a second spelling of `Answer::Shown`"
    )]
    fn shown(
        &mut self,
        page: u32,
        extent: (u32, u32),
        window: Rect,
        decoded: Option<ViewPage>,
        pixels: Vec<u8>,
        outcome: Result<(), Refusal>,
    ) -> bool {
        self.pending = Pending::Idle;
        // The buffer comes back whatever happened, so the next render neither
        // allocates nor waits for one.
        self.scratch = Some(pixels);
        // Which entry the worker holds decoded is a fact about the session
        // whatever became of this draw, so it is recorded even when the user
        // has since moved to another entry.
        if let (Some(document), Some(decoded)) = (self.document.as_mut(), decoded) {
            document.page = Some(decoded);
        }
        // A page's own size may differ from the container's declared canvas,
        // so a fitted zoom is only right once the entry itself is known.
        if let Some(natural) = self.natural() {
            self.viewport.refit(natural, self.canvas);
            self.viewport.cap_zoom(natural);
            self.viewport.clamp_pan(natural, self.canvas);
            self.sync_controls();
        }
        if let Err(refusal) = outcome {
            self.refusal = Some(refusal);
            return true;
        }
        // Staleness is about the state *now*, not about what was asked: a
        // rectangle the user has since panned or zoomed away from is a real
        // picture of the wrong place, and drawing it at the current placement
        // would put those pixels somewhere they do not belong.
        if self.wanted_shape()
            != Some(Shape {
                page,
                extent,
                window,
            })
        {
            return false;
        }
        let Some(pixels) = self.scratch.as_ref() else {
            return false;
        };
        let adopted = {
            let reorient = self.viewport.reorient;
            let mut picture = core::mem::take(&mut self.picture);
            let adopted = picture.adopt(window, extent, reorient, pixels);
            self.picture = picture;
            adopted
        };
        self.refusal = if adopted {
            None
        } else {
            Some(Refusal::Unholdable)
        };
        true
    }

    /// State `why` no document will arrive.
    ///
    /// So a window asked for one states the reason instead of staying empty,
    /// or never appearing at all — the session refusing a chooser
    /// ([`Refusal::PickRefused`]) especially, because nothing comes after it.
    /// A refused optional action is an answer, not a death.
    ///
    /// A pick the user *cancelled* is not one of these: they chose nothing, so
    /// the embedder closes that window rather than showing them a reason they
    /// already know.
    ///
    /// Answers `false`, changing nothing, when a document is already open: the
    /// picture on screen is still what the user is looking at.
    pub fn no_document(&mut self, why: Refusal) -> bool {
        if self.document.is_some() {
            return false;
        }
        self.refusal = Some(why);
        true
    }

    /// Feed one pointer event, reporting what the user asked for.
    ///
    /// Routed in the order the surfaces sit on screen: the toolbar and the
    /// zoom slider first, then the two scrollbars, then the canvas — where a
    /// primary drag pans and a secondary press asks for the context menu.
    pub fn on_pointer(
        &mut self,
        event: &InputEvent,
        layout: &Layout,
        scale: Scale,
        theme: &Theme,
        damage: &mut Region,
    ) -> Outcome {
        if let InputEvent::PointerMoved { to } = event {
            self.pointer = *to;
        }
        // The wheel over the tools scrolls the strip rather than reaching the
        // canvas beneath it, so a strip too narrow for its tools is reachable
        // with the same gesture the rest of the window uses.
        if let InputEvent::PointerScrolled { dx, dy } = event {
            if layout.tools().contains(self.pointer) {
                let moved = self
                    .toolbar
                    .wheel(*dx, *dy, layout.tools(), scale, theme, damage);
                return Outcome::changed(moved);
            }
        }
        match self
            .toolbar
            .on_pointer(event, layout.tools(), scale, theme, damage)
        {
            ToolbarOutcome::Activated(action) => return self.tool(action, layout, damage),
            // A hover arriving or leaving, a press latching, or a scrolled
            // strip: the toolbar reported its own pixels and owes a present.
            ToolbarOutcome::Redraw => return Outcome::changed(true),
            ToolbarOutcome::Idle => {}
        }
        if let Some(action) = self.zoom.on_pointer(event, layout.zoom_slider(), damage) {
            return Outcome::changed(self.slid(action));
        }
        if let Some(changed) = self.bar_pointer(event, layout, scale, theme, damage) {
            return Outcome::changed(changed);
        }
        self.canvas_pointer(event, layout, damage)
    }

    /// Route a pointer event to whichever scrollbar wants it, or `None` when
    /// neither did.
    fn bar_pointer(
        &mut self,
        event: &InputEvent,
        layout: &Layout,
        scale: Scale,
        theme: &Theme,
        damage: &mut Region,
    ) -> Option<bool> {
        if let Some(ScrollAction::ScrollTo { offset }) =
            self.vertical
                .on_pointer(event, layout.vertical_bar(), scale, theme, damage)
        {
            return Some(self.pan_to(None, Some(offset)));
        }
        if let Some(ScrollAction::ScrollTo { offset }) =
            self.horizontal
                .on_pointer(event, layout.horizontal_bar(), scale, theme, damage)
        {
            return Some(self.pan_to(Some(offset), None));
        }
        None
    }

    /// Route a pointer event over the canvas: a primary drag pans, the wheel
    /// pans or zooms, and a secondary press asks for the context menu.
    fn canvas_pointer(
        &mut self,
        event: &InputEvent,
        layout: &Layout,
        damage: &mut Region,
    ) -> Outcome {
        let over = layout.canvas().contains(self.pointer);
        match event {
            InputEvent::PointerPressed {
                button: PointerButton::Primary,
            } if over => {
                self.drag = Some(self.pointer);
                Outcome::changed(false)
            }
            InputEvent::PointerPressed {
                button: PointerButton::Secondary,
            } if over => Outcome {
                changed: false,
                pick: false,
                menu: Some(self.pointer),
                close: false,
            },
            InputEvent::PointerReleased {
                button: PointerButton::Primary,
            } => {
                // The pan happened as the pointer moved, so releasing it
                // draws nothing new: it only ends the drag.
                self.drag = None;
                Outcome::changed(false)
            }
            InputEvent::PointerMoved { to } => {
                let Some(from) = self.drag else {
                    return Outcome::changed(false);
                };
                self.drag = Some(*to);
                let Some(natural) = self.natural() else {
                    return Outcome::changed(false);
                };
                let before = self.viewport.pan();
                // Dragging the picture moves it *with* the pointer, so the
                // view moves the other way.
                self.viewport.pan_by(
                    i64::from(from.x) - i64::from(to.x),
                    i64::from(from.y) - i64::from(to.y),
                    natural,
                    self.canvas,
                );
                if self.viewport.pan() == before {
                    return Outcome::changed(false);
                }
                self.sync_controls();
                damage.add(layout.canvas());
                damage.add(layout.status());
                Outcome::changed(true)
            }
            InputEvent::PointerScrolled { dx, dy } if over => {
                self.run(Command::Pan { dx: *dx, dy: *dy }, layout, damage)
            }
            _ => Outcome::changed(false),
        }
    }

    /// Run the command a toolbar activation names.
    fn tool(&mut self, action: ToolbarAction, layout: &Layout, damage: &mut Region) -> Outcome {
        let Some((_, command, _)) = TOOLS.get(action.index) else {
            return Outcome::changed(false);
        };
        self.run(*command, layout, damage)
    }

    /// Adopt a zoom the slider reports.
    ///
    /// Both of its actions do the same thing here, because a viewer's zoom is
    /// not persisted: there is nothing durable for a settle to commit, so the
    /// live value *is* the outcome and the drag stays smooth.
    fn slid(&mut self, action: SliderAction) -> bool {
        let permille = match action {
            SliderAction::SetValue { permille } | SliderAction::Settled { permille } => permille,
        };
        let Some(natural) = self.natural() else {
            return false;
        };
        let before = self.viewport.zoom();
        self.viewport.set_zoom(zoom_at_slider(permille));
        self.viewport.cap_zoom(natural);
        if self.viewport.zoom() == before {
            return false;
        }
        self.viewport.clamp_pan(natural, self.canvas);
        // The slider's own value is not resynced from the zoom here: the
        // control is mid-drag and holds the position the user is pointing at,
        // and a capped zoom writing the thumb back under their finger would
        // fight them.
        let scaled = self.viewport.scaled(natural);
        let pan = self.viewport.pan();
        self.vertical
            .set_model(scroll_for(scaled.1, self.canvas.1, pan.1));
        self.horizontal
            .set_model(scroll_for(scaled.0, self.canvas.0, pan.0));
        true
    }

    /// Move the pan to an offset a scrollbar reports.
    fn pan_to(&mut self, x: Option<u64>, y: Option<u64>) -> bool {
        let Some(natural) = self.natural() else {
            return false;
        };
        let before = self.viewport.pan();
        let to = |offset: Option<u64>, from: u32| -> i64 {
            offset.map_or(0, |offset| {
                i64::try_from(offset).unwrap_or(i64::MAX) - i64::from(from)
            })
        };
        self.viewport
            .pan_by(to(x, before.0), to(y, before.1), natural, self.canvas);
        if self.viewport.pan() == before {
            return false;
        }
        self.sync_controls();
        true
    }

    /// Feed one key press, reporting what the user asked for.
    ///
    /// The keyboard reaches every command the toolbar and the menus do, with
    /// the spellings a viewer's user already knows.
    pub fn on_key(
        &mut self,
        key: Key,
        modifiers: Modifiers,
        layout: &Layout,
        damage: &mut Region,
    ) -> Outcome {
        let command = match key {
            Key::Char('+' | '=') => Command::ZoomIn,
            Key::Char('-' | '_') => Command::ZoomOut,
            Key::Char('0') => Command::FitWindow,
            Key::Char('1') => Command::ActualSize,
            Key::Char('2') => Command::FitWidth,
            Key::Char('[') => Command::RotateLeft,
            Key::Char(']') => Command::RotateRight,
            Key::Char('m' | 'M') => Command::Mirror,
            Key::Char('i' | 'I') => Command::ToggleInfo,
            Key::Char(' ') => Command::TogglePlayback,
            Key::Named(NamedKey::Left) if modifiers.shift => Command::PreviousPage,
            Key::Named(NamedKey::Right) if modifiers.shift => Command::NextPage,
            Key::Named(NamedKey::PageUp) => Command::PreviousPage,
            Key::Named(NamedKey::PageDown) => Command::NextPage,
            Key::Named(NamedKey::Home) => Command::FirstPage,
            Key::Named(NamedKey::End) => Command::LastPage,
            Key::Named(NamedKey::Left) => Command::Pan { dx: -1, dy: 0 },
            Key::Named(NamedKey::Right) => Command::Pan { dx: 1, dy: 0 },
            Key::Named(NamedKey::Up) => Command::Pan { dx: 0, dy: -1 },
            Key::Named(NamedKey::Down) => Command::Pan { dx: 0, dy: 1 },
            Key::Char('o' | 'O') | Key::Named(NamedKey::Enter) => Command::OpenDocument,
            Key::Named(NamedKey::Escape) => {
                return Outcome {
                    changed: false,
                    pick: false,
                    menu: None,
                    close: true,
                }
            }
            _ => return Outcome::changed(false),
        };
        self.run(command, layout, damage)
    }

    /// Arm the deadlines the current state calls for: the next animation
    /// frame, and the first auto-repeat step of a freshly held control.
    ///
    /// Called by the embedder once it knows the clock, because the engine
    /// holds no clock of its own: a frame's delay is the container's and the
    /// instant it lands on is the machine's. A press released before its
    /// first step disarms rather than firing once more.
    pub fn arm_deadline(&mut self, now_ns: u64) {
        self.repeat_ns = match (self.holding(), self.repeat_ns) {
            (true, None) => Some(now_ns.saturating_add(REPEAT_DELAY_NS)),
            (true, armed) => armed,
            (false, _) => None,
        };
        if !self.playing || self.frame_ns.is_some() {
            return;
        }
        let Some(delay) = self
            .document
            .as_ref()
            .and_then(|document| document.page)
            .map(|page| page.delay_ns)
        else {
            return;
        };
        self.frame_ns = Some(now_ns.saturating_add(delay.max(MIN_FRAME_DELAY_NS)));
    }
}

/// The viewer's toolbar: one icon tool per command in [`TOOLS`], in order.
///
/// One definition, so the strip a window draws and the strip the declared
/// window floor is measured from are the same strip.
fn tools() -> Toolbar {
    let mut toolbar = Toolbar::new();
    for (icon, _, _) in TOOLS {
        toolbar = toolbar.with_icon(IconButton::new(icon, ControlRole::Neutral), 0);
    }
    toolbar
}

/// The smallest client the viewer is laid out for, in **physical** pixels at
/// `scale` — what a window declares to the window manager when it opens.
///
/// Derived from the theme's metrics and from the toolbar's own tools, so a
/// denser theme or a larger scale cannot leave the tools unreachable or the
/// canvas without a strip to draw in.
#[must_use]
pub fn min_client_size(theme: &Theme, scale: Scale, font: BitmapFont) -> (u32, u32) {
    Layout::min_client(theme, scale, font, tools().min_width(scale, theme))
}

/// The least a frame is shown for, in nanoseconds.
///
/// A container may declare no delay at all, or one so short no screen could
/// show it; playing such an animation as fast as the decode allows would peg
/// a core and show nothing. The floor is the shortest interval a display
/// could plausibly present, so a zero-delay animation plays smoothly rather
/// than as fast as possible.
const MIN_FRAME_DELAY_NS: u64 = 10_000_000;

/// A scroll model over nothing, for a viewer with no picture.
fn empty_scroll() -> ScrollModel {
    scroll_for(0, 0, 0)
}

/// The scroll model an axis of `scaled` pixels shown `canvas` wide at `offset`
/// implies.
///
/// The scrollbars are a view of the pan, so their model is derived here and
/// nowhere else.
fn scroll_for(scaled: u32, canvas: u32, offset: u32) -> ScrollModel {
    let range = ScrollRange::new(u64::from(scaled), u64::from(canvas), u64::from(offset));
    let page = u64::from(canvas).max(1);
    let line = (page / 8).max(1);
    ScrollModel::new(range, line, page)
}

#[cfg(test)]
#[path = "view_tests.rs"]
mod tests;
