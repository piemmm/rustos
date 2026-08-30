//! The window-manager furniture family (spec §11.17–§11.23, §11.31).
//!
//! These are the window-manager-owned controls around one client viewport:
//! the [`WindowFrame`] boundary, the [`TitleBar`] with its window commands,
//! the compact [`WindowControl`] buttons (close, minimize, put-to-back,
//! size-toggle), the [`ResizeGrabber`], and the neutral [`ScrollCorner`] at a
//! two-scrollbar junction.
//!
//! The window manager owns frame rendering, hit testing, pointer capture,
//! move and resize, stacking, minimization, and size-state transitions;
//! applications provide typed metadata and receive typed events. The client
//! surface can never receive furniture input: the frame's hit map classifies
//! every point as either client or a specific furniture part, and the resize
//! corner never overlaps a scrollbar thumb.
//!
//! Every visible property resolves from the active [`Theme`] and [`Scale`]
//! through the shared `crate::paint` core (the same plate, rim, focus-ring,
//! press-latch, and keyboard-activation recipe the button family uses); the
//! command glyphs are drawn here once, as `paint::paint_chevron` is. The
//! controls render state and emit typed actions; the window manager enforces
//! authority (a denial reads distinctly from a plain disabled control, spec
//! §13). Nothing here animates, so it is reduced-motion correct by
//! construction — a minimize/size transition is a window-manager animation of
//! geometry, not a property of these controls.

use alloc::string::String;

use tairix_font::ELLIPSIS;
use tairix_geometry::{Point, Rect, Region, Scale};
use tairix_icon::{IconKind, IconPicture};
use tairix_input::{InputEvent, Key, NamedKey, PointerButton};
use tairix_raster::{div255, round_rect_coverage, Color, Surface, SUBPIXEL};
use tairix_theme::{Palette, Rgba, TextRole, Theme};

use crate::damage;
use crate::paint::{
    draw_outline, heavy_contrast, icon_slot_side, inset, key_activation, paint_bead,
    paint_flush_plate, paint_icon_slot, plate_border, pointer_activation, resolve_bead,
    resolve_tinted_frame, role_font, surface_rect, to_i32, PlateBleed, PlateStyle,
};
use crate::state::{
    ControlDisposition, ControlState, PlateSeating, PointerState, RenderInvariant, SizeAction,
    WindowActivationState, WindowControlKind, WindowFurnitureState, WindowSizeState,
};

// --- command glyphs -------------------------------------------------------
//
// The furniture command glyphs are drawn here once (as `paint::paint_chevron`
// is), rather than added to the notification-icon set in `lib/icon`: they are
// window-control primitives, not the taskbar's status vocabulary. Each is a
// crisp geometric mark that reads without colour (spec §15): the close cross,
// the minimize bar, the maximize/restore squares, and the put-to-back stacked
// plates, so a monochrome theme still tells the four commands apart.

/// The design-grid side every window-command glyph and grip tooth is authored
/// on. A glyph is authored once against this grid and then grid-fitted to the
/// device pixels of whatever box it is drawn in (see [`Glyph`]).
const GLYPH_GRID: i32 = 100;

/// `a / b` rounded to the nearest whole number, for a non-negative `a` and a
/// positive `b`.
///
/// Grid fitting must round, never truncate: truncation drags every fitted
/// coordinate toward the box's leading edge, so a mark authored symmetrically
/// in the design grid comes out off-centre and a stroke authored just under
/// half a pixel disappears rather than becoming the thinnest line that can be
/// drawn.
fn round_div(a: i32, b: i32) -> i32 {
    a.saturating_add(b / 2) / b
}

/// `v` as an unsigned surface coordinate; a negative value clamps to zero.
fn to_u32(v: i32) -> u32 {
    u32::try_from(v).unwrap_or(0)
}

/// A window-furniture glyph being drawn: the square device box it occupies and
/// the whole-pixel weight its strokes are drawn at.
///
/// Furniture marks are only a handful of pixels across, and at that size a
/// stroke whose width works out fractional has no crisp rendering at all: area
/// coverage spreads a 1.4-pixel line over two columns at partial alpha and it
/// reads as a grey smear. Each authored coordinate is therefore rounded to the
/// whole device pixel it lands nearest and the stroke weight to a whole pixel
/// too — the same grid fitting a font hinter performs — which keeps the mark
/// scaling with its box while landing it on the pixel grid.
///
/// Axis-aligned marks then need no anti-aliasing whatever and are drawn as
/// plain span fills. Only a true diagonal, which by definition cannot lie on
/// pixel boundaries, goes through the scan converter, and it is emitted in
/// [`SUBPIXEL`] units so its coverage is symmetric about the line.
struct Glyph {
    /// The box's left edge on the destination surface, in whole pixels.
    left: i32,
    /// The box's top edge on the destination surface, in whole pixels.
    top: i32,
    /// The box's side, in whole pixels.
    side: i32,
    /// The stroke weight, in whole pixels, never below one.
    weight: i32,
}

impl Glyph {
    /// The glyph box with its top-left corner at `origin`, `side` device pixels
    /// square, whose strokes are `weight` design-grid units thick.
    fn new(origin: (u32, u32), side: u32, weight: i32) -> Self {
        let side = to_i32(side);
        Self {
            left: to_i32(origin.0),
            top: to_i32(origin.1),
            side,
            weight: round_div(weight.saturating_mul(side), GLYPH_GRID).max(1),
        }
    }

    /// The whole device pixel design-grid coordinate `d` lands nearest, as an
    /// offset from the box's own origin.
    fn px(&self, d: i32) -> i32 {
        round_div(d.saturating_mul(self.side), GLYPH_GRID)
    }

    /// Fill the box-relative pixel rectangle `[x.0, x.1) × [y.0, y.1)`.
    ///
    /// Every edge is a whole pixel, so this is a plain span fill: crisp by
    /// construction, and with no scan conversion to pay for.
    fn fill(&self, surface: &mut Surface, x: (i32, i32), y: (i32, i32), color: Color) {
        let (x0, x1) = (x.0.max(0), x.1.max(0));
        let (y0, y1) = (y.0.max(0), y.1.max(0));
        if x1 <= x0 || y1 <= y0 {
            return;
        }
        surface.fill_rect(
            to_u32(self.left.saturating_add(x0)),
            to_u32(self.top.saturating_add(y0)),
            to_u32(x1 - x0),
            to_u32(y1 - y0),
            color,
        );
    }

    /// A horizontal bar one stroke weight tall spanning design columns
    /// `[x0, x1]`, sitting as near design row `cy` as whole pixels allow.
    fn bar(&self, surface: &mut Surface, x0: i32, x1: i32, cy: i32, color: Color) {
        let top = self.px(cy) - self.weight / 2;
        self.fill(
            surface,
            (self.px(x0), self.px(x1)),
            (top, top + self.weight),
            color,
        );
    }

    /// A hollow square outline between design corners `lo` and `hi`, its four
    /// edges one stroke weight thick so the interior stays open.
    fn square(&self, surface: &mut Surface, lo: (i32, i32), hi: (i32, i32), color: Color) {
        let (x0, y0) = (self.px(lo.0), self.px(lo.1));
        let (x1, y1) = (self.px(hi.0), self.px(hi.1));
        let t = self.weight;
        self.fill(surface, (x0, x1), (y0, y0 + t), color);
        self.fill(surface, (x0, x1), (y1 - t, y1), color);
        self.fill(surface, (x0, x0 + t), (y0, y1), color);
        self.fill(surface, (x1 - t, x1), (y0, y1), color);
    }

    /// A filled square between design corners `lo` and `hi`.
    fn plate(&self, surface: &mut Surface, lo: (i32, i32), hi: (i32, i32), color: Color) {
        self.fill(
            surface,
            (self.px(lo.0), self.px(hi.0)),
            (self.px(lo.1), self.px(hi.1)),
            color,
        );
    }

    /// A diagonal stroke from design point `a` to design point `b`, one stroke
    /// weight wide measured perpendicular to the line.
    ///
    /// A diagonal is the one furniture mark grid fitting cannot make crisp — a
    /// line at an angle does not lie on pixel boundaries, which is what makes
    /// it a diagonal — so it goes through the shared stroke path, with its
    /// endpoints still fitted and its weight still a whole pixel so the
    /// coverage falls symmetrically about the line.
    fn diagonal(&self, surface: &mut Surface, a: (i32, i32), b: (i32, i32), color: Color) {
        let point = |d: (i32, i32)| {
            (
                self.left
                    .saturating_add(self.px(d.0))
                    .saturating_mul(SUBPIXEL),
                self.top
                    .saturating_add(self.px(d.1))
                    .saturating_mul(SUBPIXEL),
            )
        };
        surface.stroke_polyline(
            &[point(a), point(b)],
            self.weight.saturating_mul(SUBPIXEL),
            color,
        );
    }
}

/// Paint the command glyph for `kind` centred in the content rectangle
/// `(x, y, w, h)`, in `color`. A [`WindowControlKind::SizeToggle`] draws the
/// glyph for the action it will perform *next* (`next` — maximize while
/// restored, restore while maximized, spec §11.22). Under heavy contrast the
/// strokes thicken so the mark stays legible without colour (spec §15).
fn paint_command_glyph(
    surface: &mut Surface,
    rect: (u32, u32, u32, u32),
    kind: WindowControlKind,
    next: SizeAction,
    color: Color,
    heavy: bool,
) {
    let (x, y, w, h) = rect;
    let side = w.min(h);
    if side == 0 {
        return;
    }
    // The mark is square; centre its box in a content rectangle that need not
    // be, so an off-square control does not push its glyph into one corner.
    let origin = (x + (w - side) / 2, y + (h - side) / 2);
    let glyph = Glyph::new(origin, side, if heavy { 20 } else { 12 });
    match kind {
        WindowControlKind::Close => {
            glyph.diagonal(surface, (22, 22), (78, 78), color);
            glyph.diagonal(surface, (78, 22), (22, 78), color);
        }
        WindowControlKind::Minimize => glyph.bar(surface, 20, 80, 62, color),
        WindowControlKind::SizeToggle => match next {
            SizeAction::Maximize => glyph.square(surface, (22, 22), (78, 78), color),
            SizeAction::Restore => {
                // Two overlapping square outlines: a back square up-right and
                // a front square down-left — the classic restore mark.
                glyph.square(surface, (34, 18), (78, 62), color);
                glyph.square(surface, (18, 34), (62, 78), color);
            }
        },
        WindowControlKind::PutToBack => {
            // A filled front plate low-left going behind an outlined back
            // plate high-right — a "send to back / down" cue distinct from
            // the restore mark's two outlines.
            glyph.square(surface, (40, 16), (82, 58), color);
            glyph.plate(surface, (16, 40), (58, 82), color);
        }
    }
}

// --- WindowControl --------------------------------------------------------

/// The outcome of interacting with a [`WindowControl`].
///
/// The control never performs the window command itself: it reports the typed
/// [`WindowControlKind`] and the window manager cooperatively dispatches it
/// (close is an application-directed request, not force termination, spec
/// §11.19).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum WindowControlAction {
    /// The control was activated and its command should be dispatched.
    Invoked(WindowControlKind),
    /// A secondary press landed on the control: the alternate gesture, a
    /// different request from the control's own command.
    ///
    /// The control neither arms nor washes for it, so it draws exactly as
    /// it did before the press and can never also fire
    /// [`Invoked`](Self::Invoked) from the same gesture. What the alternate
    /// gesture *means* is the window manager's to decide; the control only
    /// reports that it happened over this kind.
    AlternateInvoked(WindowControlKind),
}

/// Which corner of a command cell follows the window's rim, and by how far.
///
/// A cell is seated flush: it fills the band's height and touches its
/// neighbour, and the outermost one in each cluster is hard against the band's
/// end — where the window's rim curves through. That corner has to curve with
/// it or the cell would square off the shape the rim traces; the other three
/// stay square so the row still reads as part of the bar.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub enum BandCorner {
    /// Seated between other commands: every corner square.
    #[default]
    Square,
    /// Hard against the band's leading end, the top-leading corner curving by
    /// this radius.
    Leading(u32),
    /// Hard against its trailing end, the top-trailing corner curving by this
    /// radius.
    Trailing(u32),
}

impl BandCorner {
    /// The radius this corner draws with, and how far the plate bleeds past its
    /// cell to leave the other three square.
    pub(crate) fn plate(self) -> (u32, PlateBleed) {
        match self {
            Self::Square => (0, PlateBleed::NONE),
            Self::Leading(radius) => (
                radius,
                PlateBleed {
                    right: radius,
                    bottom: radius,
                    ..PlateBleed::NONE
                },
            ),
            Self::Trailing(radius) => (
                radius,
                PlateBleed {
                    left: radius,
                    bottom: radius,
                    ..PlateBleed::NONE
                },
            ),
        }
    }
}

/// The identity hue `kind` highlights with.
///
/// The four commands are a closed vocabulary and the palette names one wash
/// each, so the mapping lives here alone. A size toggle keeps its hue in both
/// directions: maximizing and restoring are the same command, and swapping its
/// colour with its glyph would read as a different button appearing.
fn command_tint(palette: &Palette, kind: WindowControlKind) -> Rgba {
    match kind {
        WindowControlKind::Close => palette.window_close,
        WindowControlKind::Minimize => palette.window_minimize,
        WindowControlKind::SizeToggle => palette.window_maximize,
        WindowControlKind::PutToBack => palette.window_put_to_back,
    }
}

/// One compact window-command furniture button (spec §11.19–§11.22).
///
/// Built from the shared button behaviour (the `crate::paint` plate,
/// focus-ring, press-latch, and keyboard-activation core the [`crate::Button`]
/// family uses), it draws the command's glyph on a quiet plate that brightens
/// on hover/focus/press and mutes on an inactive frame, and carries the spec §13
/// authority treatment (a denied control keeps its slot and shows the lock
/// bead rather than looking merely disabled). A [`WindowControlKind::SizeToggle`]
/// shows the glyph and accessible name of the action it will perform *next*.
///
/// Equal controls draw the same pixels, so a host may use `==` as its repaint
/// gate: the command, the next size action, whether the frame is active, and
/// the visible state all compare. The pointer coordinate and press latch do
/// not — no render path reads either.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WindowControl {
    kind: WindowControlKind,
    state: ControlState,
    next_size: SizeAction,
    active_frame: bool,
    /// The last pointer position — hit-testing input, never drawn.
    pointer: RenderInvariant<Point>,
    /// The press latch, kept while the pointer slides off so the control still
    /// fires on release over it; the press *look* lives in `state.pointer`.
    armed: RenderInvariant<bool>,
}

impl WindowControl {
    /// A window control for `kind`, in the resting state on an active frame.
    #[must_use]
    pub fn new(kind: WindowControlKind) -> Self {
        Self {
            kind,
            state: ControlState::idle(),
            next_size: SizeAction::Maximize,
            active_frame: true,
            pointer: RenderInvariant::new(Point::ORIGIN),
            armed: RenderInvariant::new(false),
        }
    }

    /// The command this control represents.
    #[must_use]
    pub fn kind(&self) -> WindowControlKind {
        self.kind
    }

    /// The control's current composed state.
    #[must_use]
    pub fn state(&self) -> ControlState {
        self.state
    }

    /// Replace the control's composed state (e.g. from a model update).
    pub fn set_state(&mut self, state: ControlState) {
        self.state = state;
    }

    /// Set the control's keyboard focus.
    pub fn set_focused(&mut self, focused: bool) {
        self.state.focus.focused = focused;
    }

    /// Set the size action a [`WindowControlKind::SizeToggle`] should show.
    ///
    /// Ignored by the other kinds. The window manager sets this from the
    /// window's [`WindowSizeState`] so the glyph and
    /// accessible name describe the *next* action (spec §11.22).
    pub fn set_size_action(&mut self, next: SizeAction) {
        self.next_size = next;
    }

    /// Whether the owning frame is active. An inactive frame's controls stay
    /// complete but quieter (spec §11.17), so this only lowers idle contrast.
    pub fn set_active_frame(&mut self, active: bool) {
        self.active_frame = active;
    }

    /// The accessible name for this control's current command.
    ///
    /// A size toggle names its *next* action, so accessibility tools announce
    /// what activation will do (spec §11.22).
    #[must_use]
    pub fn accessible_name(&self) -> &'static str {
        match self.kind {
            WindowControlKind::Close => "Close",
            WindowControlKind::Minimize => "Minimize",
            WindowControlKind::PutToBack => "Put window to back",
            WindowControlKind::SizeToggle => match self.next_size {
                SizeAction::Maximize => "Maximize",
                SizeAction::Restore => "Restore",
            },
        }
    }

    /// Paint the control into `surface` at `bounds` for the active theme,
    /// rounding `corner` where the cell meets the end of its band.
    ///
    /// `bounds` is the whole cell, margins and all — a command carries none, so
    /// a hover lights every pixel of it and a press lands anywhere in it.
    pub fn render(
        &self,
        surface: &mut Surface,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
        corner: BandCorner,
    ) {
        let Some((x, y, w, h)) = surface_rect(bounds) else {
            return;
        };
        if w == 0 || h == 0 {
            return;
        }
        let palette = theme.palette();
        let disposition = self.state.disposition();
        let interactive = matches!(
            disposition,
            ControlDisposition::Interactive | ControlDisposition::NeedsConfirmation
        );
        let hovered = self.state.pointer == PointerState::Hover;
        let pressed = self.state.pointer == PointerState::Pressed;
        let focused = self.state.focus.focused;
        let awake = hovered || pressed || focused;
        // The command's own hue, composed over the window body the title bar
        // is painted on. A plate is laid down rather than blended, so the
        // authored translucency has to be resolved against that ground here:
        // laying it down as-is would leave a hole in the window's furniture
        // strip and show the desktop through the button.
        let tint = command_tint(palette, self.kind).over(palette.surface);
        let frame = resolve_tinted_frame(theme, tint, self.state);

        // A flush cell is square except where the window's rim curves through
        // it, so the radius is the band's, not the control family's: a cell
        // rounded on its own terms inside a rounder window shows a sliver of
        // the bar at the corner.
        let (radius, bleed) = corner.plate();
        let border = plate_border(theme, scale);
        // A command is seated in the title bar, so it states hover, press, and
        // focus on its plate alone and wears nothing at all while it rests: an
        // edge of its own would read as a line drawn round the window's corner
        // rather than as feedback on a button.
        if let Some((plate, rim)) = frame.face(PlateSeating::Bar) {
            paint_flush_plate(
                surface,
                (x, y, w, h),
                bleed,
                &PlateStyle {
                    radius,
                    border,
                    plate,
                    rim,
                    focused,
                    ring: Color::from(palette.rim_active),
                },
            );
        }

        // Glyph colour: muted when disabled, or when an interactive control
        // rests on an inactive frame; otherwise full contrast.
        let muted = disposition == ControlDisposition::DisabledByState
            || (!self.active_frame && interactive && !awake);
        let glyph_color = if muted {
            Color::from(palette.on_surface_muted)
        } else {
            Color::from(palette.on_surface)
        };

        // Inset the glyph by a fraction of the extent (not the full control
        // inset, which would collapse a compact furniture button).
        let pad = (w.min(h) / 5).max(1);
        if let Some(content) = inset(x, y, w, h, pad) {
            paint_command_glyph(
                surface,
                content,
                self.kind,
                self.next_size,
                glyph_color,
                heavy_contrast(theme),
            );
        }

        // The spec §13 Authority Mark / recovery / complete bead, top-trailing.
        if let Some((color, shape)) = resolve_bead(theme, self.state) {
            let size = scale
                .scale_length(theme.metrics().bead_size)
                .max(3)
                .min(w)
                .min(h);
            paint_bead(surface, x + w - size, y, size, color, shape);
        }
    }

    /// Feed a pointer event, given the control's current `bounds`, returning
    /// [`WindowControlAction::Invoked`] on a completed primary click, or
    /// [`WindowControlAction::AlternateInvoked`] on a secondary press over
    /// an actionable control. The press is never forwarded to the client
    /// surface (spec §11.18).
    ///
    /// A secondary press takes no latch and leaves `state` alone, so the
    /// control's appearance is bit-identical across it and the two gestures
    /// can never both fire.
    ///
    /// On the release that completes a click the control returns to rest — its
    /// hover/press highlight and any keyboard focus ring are cleared — so a
    /// furniture button loses its border once its command fires, the way a
    /// desktop title-bar control does. A genuine hover is re-established by the
    /// next pointer move if the pointer still lies over the control, so this
    /// only drops the *stale* highlight left when activation relocates the
    /// control (a size toggle) or takes the frame away (close/minimise/back).
    pub fn on_pointer(
        &mut self,
        event: &InputEvent,
        bounds: Rect,
        damage: &mut Region,
    ) -> Option<WindowControlAction> {
        if let InputEvent::PointerMoved { to } = event {
            *self.pointer = *to;
        }
        let inside = bounds.contains(*self.pointer);
        if let InputEvent::PointerPressed {
            button: PointerButton::Secondary,
        } = event
        {
            return (inside && self.state.is_actionable())
                .then_some(WindowControlAction::AlternateInvoked(self.kind));
        }
        if pointer_activation(
            &mut self.state,
            &mut self.armed,
            event,
            inside,
            bounds,
            damage,
        ) {
            self.rest(bounds, damage);
            Some(WindowControlAction::Invoked(self.kind))
        } else {
            None
        }
    }

    /// The pointer has left this control, with no position that would prove
    /// it: drop the hover highlight, reporting `bounds` only if it was lit.
    ///
    /// [`on_pointer`](Self::on_pointer) ends a hover when a *motion* lands
    /// outside the control, which is the ordinary way one ends — and not the
    /// only way. A window raised over this one, a grab taking the pointer, or
    /// the pointer crossing onto another surface each end the hover with the
    /// pointer still at the very same coordinates, and re-testing those
    /// coordinates would answer "still inside" and leave the control lit under
    /// whatever is now in front of it. Occlusion is the seat's fact, so the
    /// control is told rather than left to infer it.
    ///
    /// Any press latch is left alone: a latch is held only while a button is
    /// down, and a button held down holds the pointer with it, so a control
    /// cannot be both armed and left.
    pub fn pointer_left(&mut self, bounds: Rect, damage: &mut Region) {
        damage::set(&mut self.state.pointer, PointerState::None, bounds, damage);
    }

    /// Feed a key event, given the control's current `bounds`, returning
    /// [`WindowControlAction::Invoked`] when a focused, actionable control is
    /// activated with Space or Enter.
    ///
    /// Activation clears the control's focus ring, so the border shows only
    /// while the group is being navigated with the keyboard, not after the
    /// command has fired.
    pub fn on_key(
        &mut self,
        key: Key,
        bounds: Rect,
        damage: &mut Region,
    ) -> Option<WindowControlAction> {
        if key_activation(self.state, key) {
            self.rest(bounds, damage);
            Some(WindowControlAction::Invoked(self.kind))
        } else {
            None
        }
    }

    /// Return the control to rest after its command fires: drop the pointer
    /// hover/press highlight and the keyboard focus ring so no border lingers
    /// once activation completes.
    ///
    /// Both cleared fields are drawn, so each goes through the guarded write:
    /// a control activated by the keyboard reports the ring it just dropped,
    /// and one activated by a press that was already resting reports nothing.
    fn rest(&mut self, bounds: Rect, damage: &mut Region) {
        damage::set(&mut self.state.pointer, PointerState::None, bounds, damage);
        damage::set(&mut self.state.focus.focused, false, bounds, damage);
        *self.armed = false;
    }

    /// Whether the control currently holds a captured press latch.
    #[must_use]
    fn armed(&self) -> bool {
        *self.armed
    }
}

// --- TitleBar -------------------------------------------------------------

/// The window commands in the fixed left-to-right order they are seated in:
/// the leading cluster in the bar's left corner, then the trailing cluster in
/// its right. It is also the keyboard traversal order, so an arrow key moves
/// the focus ring the way the eye reads.
pub(crate) const CONTROL_ORDER: [WindowControlKind; 4] = [
    WindowControlKind::PutToBack,
    WindowControlKind::Close,
    WindowControlKind::Minimize,
    WindowControlKind::SizeToggle,
];

/// How many of [`CONTROL_ORDER`] each corner cluster seats.
///
/// Both clusters hold this many equally sized controls, so the span they leave
/// between them is the same whichever end of the bar it is measured from.
const CLUSTER_COUNT: u32 = 2;

/// Which commands a title band seats.
///
/// A window's band seats the four window commands; a menu plate's seats
/// none. Two
/// properties of a plate's band follow from that emptiness rather than from
/// knobs of their own: with no clusters the drag span is the whole band, and
/// with no leading cluster to justify against the title centres.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub enum TitleBarCommands {
    /// The four window commands, in their two corner clusters.
    #[default]
    Window,
    /// None: a handle and an identification, nothing to press.
    Empty,
}

impl TitleBarCommands {
    /// How many of the stored controls this set actually seats.
    const fn seated(self) -> usize {
        match self {
            Self::Window => CONTROL_ORDER.len(),
            Self::Empty => 0,
        }
    }

    /// How many controls each corner cluster holds.
    const fn per_cluster(self) -> u32 {
        match self {
            Self::Window => CLUSTER_COUNT,
            Self::Empty => 0,
        }
    }
}

/// The scaled lengths a title bar's command clusters are built from, so
/// laying a bar out and asking for the narrowest band it fits in read the
/// same arithmetic.
///
/// A command *cell* carries no margin of its own: it is as tall as the band,
/// it touches the cell beside it, and the outermost one is hard against the
/// band's end. A hover therefore lights every pixel between one command and
/// the next, and a press lands wherever the pointer is over the cell rather
/// than only over the glyph's own square. The two spacings that remain are not
/// button margins — they hold the identity group off the commands and its
/// title off its icon — so they keep their own names here.
struct ClusterMetrics {
    /// One command cell's side. A cell fills the band's height and is square,
    /// so the band's height is the only thing that sets it — a second metric
    /// for the width could only ever disagree with it.
    extent: u32,
    /// The clear space between a cluster and the identity span beside it.
    span_gap: u32,
    /// The gap inside the identity group, between the icon slot and the title.
    identity_gap: u32,
    /// One cluster's total width: [`CLUSTER_COUNT`] cells, butted together.
    cluster_w: u32,
}

impl ClusterMetrics {
    /// The cluster lengths for a band `side` pixels tall seating `commands`.
    ///
    /// A band that seats none has no cluster width at all, which is what
    /// leaves its whole span draggable.
    fn of(scale: Scale, theme: &Theme, side: u32, commands: TitleBarCommands) -> Self {
        let metrics = theme.metrics();
        let extent = side.max(1);
        Self {
            extent,
            span_gap: scale.scale_length(metrics.control_gap),
            identity_gap: scale.scale_length(metrics.control_inset),
            cluster_w: extent.saturating_mul(commands.per_cluster()),
        }
    }
}

/// The corner the command in layout `slot` rounds: the first cell is hard
/// against the band's leading end and the last against its trailing one, both
/// following the window's own `radius`; the two between them are square.
fn band_corner(slot: usize, radius: u32) -> BandCorner {
    match slot {
        0 => BandCorner::Leading(radius),
        s if s == CONTROL_ORDER.len() - 1 => BandCorner::Trailing(radius),
        _ => BandCorner::Square,
    }
}

/// The canonical index of a command in [`CONTROL_ORDER`].
fn control_index(kind: WindowControlKind) -> usize {
    match kind {
        WindowControlKind::PutToBack => 0,
        WindowControlKind::Close => 1,
        WindowControlKind::Minimize => 2,
        WindowControlKind::SizeToggle => 3,
    }
}

/// The outcome of interacting with a [`TitleBar`].
///
/// A title bar reports intent; the window manager performs the activation,
/// move, and command dispatch (a title-bar drag is a cooperative move, spec
/// §11.18).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum TitleBarEvent {
    /// The title bar was pressed and the window should be activated.
    Activate,
    /// A move gesture began (movement passed the drag threshold); the window
    /// manager should capture the pointer.
    DragBegin,
    /// The move gesture continued to `to` (screen coordinates).
    DragMoved {
        /// The new pointer position.
        to: Point,
    },
    /// The move gesture ended (pointer released).
    DragEnd,
    /// A window control was invoked.
    Control(WindowControlKind),
    /// A secondary press landed on a window control — the alternate
    /// gesture beside its command, leaving the control's drawn state and
    /// the window untouched.
    AlternateControl(WindowControlKind),
}

/// Where a point falls within a title bar.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum TitleHit {
    /// Over one of the window-command controls.
    Control(WindowControlKind),
    /// Over the draggable title region (identity/title/drag area).
    Drag,
}

/// How much of the identity artwork's colour a title bar keeps while its
/// window is active: nearly all of it, so the icon still reads as itself while
/// sitting in the chrome rather than shouting out of it.
pub(crate) const IDENTITY_SATURATION_ACTIVE: u8 = 230;

/// And while it is not: none. An unfocused window's icon goes grey along with
/// its muted title, so a glance across the desktop finds the window in hand by
/// looking for the one coloured icon.
pub(crate) const IDENTITY_SATURATION_INACTIVE: u8 = 0;

/// How much of its colour the title-bar *hue* keeps while its window is active:
/// the icon's own, since the two are the same colour seen at two strengths.
pub(crate) const HUE_SATURATION_ACTIVE: u8 = IDENTITY_SATURATION_ACTIVE;

/// And while it is not: better than half. The hue does not follow the icon all
/// the way to grey. It is already faint enough not to compete for attention,
/// and it is the only thing on an unfocused window still saying which
/// application owns it — a whole desktop of identically grey bars is harder to
/// read at a glance, not calmer.
pub(crate) const HUE_SATURATION_INACTIVE: u8 = 150;

/// The laid-out rectangles of a title bar's parts, for painting and hit
/// testing over one shared geometry (so they cannot diverge).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TitleBarLayout {
    /// Storage for the control rects; only the first
    /// [`seated`](Self::seated) of them are laid out.
    slots: [(WindowControlKind, Rect); CONTROL_ORDER.len()],
    /// How many of `slots` this band seats.
    seated: usize,
    /// The identity icon's square slot, leading the identity group.
    /// [`Rect::EMPTY`] when the bar carries no identity, or when the span
    /// between the clusters leaves no room for the slot.
    pub icon: Rect,
    /// The box the title text occupies, following the identity slot: exactly
    /// as wide as the text draws, or as wide as the span leaves once the text
    /// has to elide.
    ///
    /// This is the *text* box, not the drag region. Everything in the band
    /// that is not a control drags the window — the identity slot and the
    /// text included — so a bar drags from anywhere but its commands.
    pub title: Rect,
    /// The span between the two command clusters, which the identity group
    /// starts at: the contiguous stretch of band a press can always move the
    /// window by.
    ///
    /// Narrower than everything that in fact drags (the insets outboard of
    /// the clusters do too), and deliberately so — a window manager keeping a
    /// grabbable patch of the bar on screen wants the part it can be sure of.
    /// Empty when the band is too narrow to leave a span at all.
    pub drag: Rect,
}

impl TitleBarLayout {
    /// The laid-out control rects in canonical command order, each paired
    /// with its command: the leading cluster first, then the trailing one.
    /// Empty for a band that seats no commands.
    #[must_use]
    pub fn controls(&self) -> &[(WindowControlKind, Rect)] {
        &self.slots[..self.seated]
    }
}

/// Bound an untrusted window title/identity string: cap its length and replace
/// control characters with a space, so it renders as plain text and cannot
/// smuggle escape sequences or break the layout (spec §11.18). The text
/// engine's own directional handling applies when it is drawn.
fn sanitize_label(text: &str) -> String {
    const MAX: usize = 512;
    let mut out = String::new();
    for ch in text.chars().take(MAX) {
        if ch.is_control() {
            out.push(' ');
        } else {
            out.push(ch);
        }
    }
    out
}

/// The window-manager-owned title bar: application identity, a title, a stable
/// drag region, and the window commands (spec §11.18).
///
/// It owns the four [`WindowControl`]s and seats them in two corner clusters —
/// put-to-back and close at the leading edge, minimize and size-toggle at the
/// trailing one — leaving the span between them for the identity group, which
/// is left-justified in it, hard against the leading commands. Pressing
/// anywhere but a control activates the window and, past the drag threshold,
/// begins a cooperative move; a press over a control routes to that control
/// instead and never starts a drag. The title text is untrusted application
/// data, so it is length-bounded, control characters are replaced, and it
/// elides on the right once the span cannot show it whole.
///
/// The owning application's identity icon leads that group, before the text. It
/// is inert: it drags the window like the rest of the band and is never a
/// control.
///
/// Equal title bars draw the same pixels, so a host may use `==` as its
/// repaint gate: the furniture state, the four commands with their own visible
/// states, the identity class, and both texts all compare. The whole drag
/// gesture behind them does not — the pointer coordinate, the pending-press
/// and dragging latches, and the press origin the threshold is measured from
/// are hit-testing bookkeeping no render path reads. What a drag *shows* is
/// the window moving, which is the owner's geometry rather than this bar's
/// pixels.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TitleBar {
    furniture: WindowFurnitureState,
    commands: TitleBarCommands,
    controls: [WindowControl; CONTROL_ORDER.len()],
    identity: Option<IconKind>,
    /// The hue the band washes with, taken from the identity artwork's dominant
    /// colour by whoever installed it. `None` leaves the band plain.
    identity_hue: Option<Color>,
    app_name: String,
    title: String,
    /// The last pointer position — hit-testing input, never drawn.
    pointer: RenderInvariant<Point>,
    /// Whether a press landed on the drag region and has not been released.
    press_pending: bool,
    /// Whether that press has passed the drag threshold and become a move.
    dragging: bool,
    /// Where the pending press started, so the threshold measures the whole
    /// gesture rather than the last sample.
    press_origin: RenderInvariant<Point>,
}

impl TitleBar {
    /// A title bar for a window in the given furniture state, with its four
    /// commands seated in the two corner clusters.
    #[must_use]
    pub fn new(furniture: WindowFurnitureState) -> Self {
        Self::seating(furniture, TitleBarCommands::Window)
    }

    /// A title band that seats no commands: a centred title over a band that
    /// drags end to end.
    ///
    /// A menu plate's band. It is this same bar rather than a second control,
    /// so the drag gesture and the untrusted-label bounding have one
    /// implementation. A plate is never inactive, never maximized, and is
    /// moved only by its own band, which is what its furniture state says.
    #[must_use]
    pub fn plate() -> Self {
        Self::seating(
            WindowFurnitureState {
                activation: WindowActivationState::Active,
                size: WindowSizeState::Restored,
                movable: true,
                resizable: false,
            },
            TitleBarCommands::Empty,
        )
    }

    /// A title bar in `furniture` seating `commands`.
    fn seating(furniture: WindowFurnitureState, commands: TitleBarCommands) -> Self {
        let controls = [
            WindowControl::new(CONTROL_ORDER[0]),
            WindowControl::new(CONTROL_ORDER[1]),
            WindowControl::new(CONTROL_ORDER[2]),
            WindowControl::new(CONTROL_ORDER[3]),
        ];
        let mut bar = Self {
            furniture,
            commands,
            controls,
            identity: None,
            identity_hue: None,
            app_name: String::new(),
            title: String::new(),
            pointer: RenderInvariant::new(Point::ORIGIN),
            press_pending: false,
            dragging: false,
            press_origin: RenderInvariant::new(Point::ORIGIN),
        };
        bar.apply_furniture();
        bar
    }

    /// Propagate the furniture state to the controls: the size toggle shows
    /// the next action and is enabled only on a resizable window; the frame
    /// activation lowers idle contrast on an inactive frame.
    ///
    /// The size toggle's enablement is set from `resizable` both ways, so a
    /// window that becomes resizable again gets its toggle back; only the
    /// enabled flag moves, leaving the control's hover and focus alone.
    fn apply_furniture(&mut self) {
        let active = self.furniture.activation != WindowActivationState::Inactive;
        let next = self.furniture.size_action();
        let seated = self.commands.seated();
        for control in &mut self.controls[..seated] {
            control.set_active_frame(active);
            if control.kind() == WindowControlKind::SizeToggle {
                control.set_size_action(next);
                control.set_state(control.state().with_enabled(self.furniture.resizable));
            }
        }
    }

    /// The window's furniture state.
    #[must_use]
    pub fn furniture(&self) -> WindowFurnitureState {
        self.furniture
    }

    /// Replace the furniture state, updating the controls (spec §11.22).
    pub fn set_furniture(&mut self, furniture: WindowFurnitureState) {
        self.furniture = furniture;
        self.apply_furniture();
    }

    /// Set the application-identity name (untrusted; sanitised).
    pub fn set_app_name(&mut self, name: &str) {
        self.app_name = sanitize_label(name);
    }

    /// Set the owning application's identity icon, or clear it with `None`.
    ///
    /// This is the *class* of the icon, never the artwork: it is small and
    /// cheaply compared, so a host may keep using `==` on the whole bar as its
    /// repaint gate. The artwork itself is the owner's, passed to
    /// [`render`](Self::render) at draw time. A bar with no identity reserves
    /// no slot and its title text takes the whole draggable region.
    ///
    /// The identity is the window manager's own attestation of who owns the
    /// window, not anything the application said about itself.
    pub fn set_identity(&mut self, identity: Option<IconKind>) {
        self.identity = identity;
    }

    /// Set the hue the band washes with, taken from the identity artwork's
    /// dominant colour ([`Surface::dominant_color`]), or `None` for a plain
    /// band.
    ///
    /// The caller resolves it once when it installs the artwork rather than the
    /// bar deriving it per repaint: a hover over a command re-renders the
    /// chrome, and re-reading an icon's pixels to answer a question whose
    /// answer cannot have changed is work for nothing. Artwork with no
    /// discernible hue — a greyscale glyph — yields `None` there, so the band
    /// stays plain rather than washing with a grey nobody asked for.
    pub fn set_identity_hue(&mut self, hue: Option<Color>) {
        self.identity_hue = hue;
    }

    /// The hue the band washes with, if any.
    #[must_use]
    pub fn identity_hue(&self) -> Option<Color> {
        self.identity_hue
    }

    /// The owning application's identity icon class, if it has one.
    #[must_use]
    pub fn identity(&self) -> Option<IconKind> {
        self.identity
    }

    /// The pixel side the identity icon paints at inside the title band
    /// `bounds`.
    ///
    /// The pixel side the identity icon draws at inside a title band of
    /// `bounds`.
    ///
    /// `bounds` is the title band itself — the same rectangle passed to
    /// [`layout`](Self::layout) and [`render`](Self::render), never the whole
    /// window's bounds. This is the render geometry, exposed so an owner
    /// rasterising the window's identity artwork produces it at exactly the size
    /// [`render`](Self::render) will place. The side is the same whether or not
    /// the bar carries an identity, so a caller can size artwork before deciding
    /// to supply it — which is why it takes no bar at all.
    #[must_use]
    pub fn icon_side(bounds: Rect, scale: Scale, theme: &Theme) -> u32 {
        icon_slot_side(
            role_font(theme, scale, TextRole::WindowTitle),
            bounds.height,
        )
    }

    /// Set the window title (untrusted; sanitised).
    pub fn set_title(&mut self, title: &str) {
        self.title = sanitize_label(title);
    }

    /// The current window title (sanitised).
    #[must_use]
    pub fn title(&self) -> &str {
        &self.title
    }

    /// The application-identity name (sanitised).
    #[must_use]
    pub fn app_name(&self) -> &str {
        &self.app_name
    }

    /// A title band's height at this scale and theme, in physical pixels.
    ///
    /// The one reading of the metric, so a band drawn by the window frame and
    /// one drawn as a menu plate's are the same depth.
    #[must_use]
    pub fn band_height(scale: Scale, theme: &Theme) -> u32 {
        scale.scale_length(theme.metrics().title_bar_height)
    }

    /// The narrowest band that seats both command clusters and still leaves a
    /// drag surface between them, in physical pixels.
    ///
    /// Below it [`layout`](Self::layout) abuts the clusters and the window
    /// has nothing left to be dragged by, so a window manager sizes a
    /// decorated window against this rather than a constant of its own. The
    /// span it reserves between the clusters is one whole cell wide: a drag
    /// surface thinner than the square buttons beside it would be one the
    /// pointer keeps missing.
    ///
    /// The cell side is the band height [`WindowFrame::layout`] will hand
    /// [`layout`](Self::layout), taken from the same frame metrics rather than
    /// re-scaled here, so the floor and the layout at it cannot disagree.
    #[must_use]
    pub fn min_band_width(commands: TitleBarCommands, scale: Scale, theme: &Theme) -> u32 {
        let (_, band_h, _) = WindowFrame::edges(scale, theme);
        let m = ClusterMetrics::of(scale, theme, band_h, commands);
        m.cluster_w
            .saturating_mul(2)
            .saturating_add(m.span_gap.saturating_mul(2))
            .saturating_add(m.extent)
    }

    /// Which commands this band seats.
    #[must_use]
    pub const fn commands(&self) -> TitleBarCommands {
        self.commands
    }

    /// A shared reference to the control for `kind`, or `None` on a band that
    /// seats no commands — where there is no such control to report a state
    /// for, and answering with an unseated one would be a lie.
    #[must_use]
    pub fn control(&self, kind: WindowControlKind) -> Option<&WindowControl> {
        self.seated().get(control_index(kind))
    }

    /// A mutable reference to the control for `kind`, so the window manager can
    /// set its authority/enabled/recovery state. `None` on a band that seats
    /// no commands.
    pub fn control_mut(&mut self, kind: WindowControlKind) -> Option<&mut WindowControl> {
        let seated = self.commands.seated();
        self.controls[..seated].get_mut(control_index(kind))
    }

    /// The controls this band actually seats, in canonical command order.
    fn seated(&self) -> &[WindowControl] {
        &self.controls[..self.commands.seated()]
    }

    /// The stored control for `kind`, seated or not.
    ///
    /// For the band's own laid-out slots, which name only seated commands.
    fn stored_mut(&mut self, kind: WindowControlKind) -> &mut WindowControl {
        &mut self.controls[control_index(kind)]
    }

    /// Lay the title bar out within `bounds` for the active theme: a command
    /// cluster inset into each corner, and the identity group at the leading
    /// edge of the span they leave between them.
    #[must_use]
    pub fn layout(&self, bounds: Rect, scale: Scale, theme: &Theme) -> TitleBarLayout {
        let ClusterMetrics {
            extent: e,
            span_gap: g,
            identity_gap,
            cluster_w,
        } = ClusterMetrics::of(scale, theme, bounds.height, self.commands);

        let leading_left = bounds.left();
        // A band too narrow for both clusters abuts them instead of stacking
        // one over the other: a control drawn under another cannot be hit
        // where it is seen.
        let trailing_left =
            (bounds.right() - to_i32(cluster_w)).max(leading_left + to_i32(cluster_w));

        let cell = Rect::new(0, 0, e, bounds.height);
        let mut slots = [
            (CONTROL_ORDER[0], cell),
            (CONTROL_ORDER[1], cell),
            (CONTROL_ORDER[2], cell),
            (CONTROL_ORDER[3], cell),
        ];
        for (i, slot) in slots.iter_mut().enumerate() {
            let i = u32::try_from(i).unwrap_or(0);
            let (cluster_left, within) = if i < CLUSTER_COUNT {
                (leading_left, i)
            } else {
                (trailing_left, i - CLUSTER_COUNT)
            };
            let x = cluster_left + to_i32(within.saturating_mul(e));
            slot.1 = Rect::new(x, bounds.top(), e, bounds.height);
        }

        // With no clusters there is nothing to hold the span off, so it is the
        // whole band and the identity group centres in it.
        let bare = cluster_w == 0;
        let inset = if bare { 0 } else { to_i32(g) };
        let span_left = leading_left + to_i32(cluster_w) + inset;
        let span = Rect::new(
            span_left,
            bounds.top(),
            to_u32(trailing_left - inset - span_left),
            bounds.height,
        );
        let (icon, title) = self.seat_identity(span, scale, theme, identity_gap, bare);

        TitleBarLayout {
            slots,
            seated: self.commands.seated(),
            icon,
            title,
            drag: span,
        }
    }

    /// Seat the identity slot and the title text as one group within `span`,
    /// the two separated by `gap` pixels — left-justified against the leading
    /// cluster, or `centred` in the span when there is no cluster to justify
    /// against.
    ///
    /// Left-justification is what makes a window's title start in the same
    /// place whatever it says and however wide the window is: the eye finds it
    /// without hunting, and a squeezed bar elides the tail on the right rather
    /// than sliding the whole line out from under the reader. A band with
    /// nothing at either end has no such edge to find, so its title reads from
    /// the middle. A bar with no identity, or a span too narrow to seat the
    /// slot, reserves nothing — a window without an identifiable owner reads
    /// exactly as it did before one existed.
    ///
    /// The text box is exactly as wide as the line drawn in it, so it measures
    /// the very line that is drawn: the font layer memoises a measurement by
    /// whole string, so laying out and then painting the same title measure it
    /// once between them, and no piecewise total can disagree with what
    /// appears.
    fn seat_identity(
        &self,
        span: Rect,
        scale: Scale,
        theme: &Theme,
        gap: u32,
        centred: bool,
    ) -> (Rect, Rect) {
        let font = role_font(theme, scale, TextRole::WindowTitle);
        let side = icon_slot_side(font, span.height);
        let show_icon = self.identity.is_some() && side > 0 && span.width >= side;
        let reserved = if show_icon {
            side.saturating_add(gap)
        } else {
            0
        };
        let text = font
            .text_width(&self.display_text())
            .min(span.width.saturating_sub(reserved));
        let group = reserved.saturating_add(text);
        let left = if centred {
            span.left() + (to_i32(span.width) - to_i32(group)).max(0) / 2
        } else {
            span.left()
        };
        let icon = if show_icon {
            let iy = span.top() + (to_i32(span.height) - to_i32(side)).max(0) / 2;
            Rect::new(left, iy, side, side)
        } else {
            Rect::EMPTY
        };
        (
            icon,
            Rect::new(left + to_i32(reserved), span.top(), text, span.height),
        )
    }

    /// Paint the title bar into `surface` at `bounds` for the active theme.
    ///
    /// `bounds` is the title band, not the whole window. The band's ground is
    /// the frame's own plate, which the frame has already laid down *rounded*
    /// ([`WindowFrame::render`]) — painting it again here would square off the
    /// very corners the rim curves around, in the colour that is already there.
    /// So this draws the bar's own marks alone: the identity icon in its
    /// laid-out slot, the identity/title text elided to the box laid out for
    /// it, and the controls in their laid-out slots.
    ///
    /// `artwork` is the owning application's identity icon, pre-rasterised by
    /// the owner at [`icon_side`](Self::icon_side); `None` falls back to the
    /// built-in glyph for the identity class, so a bar with an identity always
    /// draws something. It is ignored by a bar with no identity. The artwork
    /// is decoded and rasterised long before it reaches this call — a control
    /// never parses image bytes.
    pub fn render(
        &self,
        surface: &mut Surface,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
        artwork: Option<IconPicture<'_>>,
    ) {
        // Window furniture is titling text, not interface body text.
        let font = role_font(theme, scale, TextRole::WindowTitle);
        let palette = theme.palette();
        let layout = self.layout(bounds, scale, theme);

        let active = self.furniture.activation != WindowActivationState::Inactive;
        let text_color = if active {
            Color::from(palette.on_surface)
        } else {
            Color::from(palette.on_surface_muted)
        };
        let saturation = if active {
            IDENTITY_SATURATION_ACTIVE
        } else {
            IDENTITY_SATURATION_INACTIVE
        };

        // The band's own wash goes down first, so everything else — the icon,
        // the title, a lit command — reads on top of it rather than through it.
        self.wash_band(surface, bounds, scale, theme, &layout, active);

        if let Some(kind) = self.identity {
            if let Some((ix, iy, side, _)) = surface_rect(layout.icon) {
                paint_icon_slot(
                    surface,
                    (ix, iy, side),
                    kind,
                    text_color,
                    artwork,
                    saturation,
                );
            }
        }

        if layout.title.width > 0 {
            let glyph_h = font.glyph_height();
            let ty =
                layout.title.top() + (to_i32(layout.title.height) - to_i32(glyph_h)).max(0) / 2;
            let tx = layout.title.left();
            let combined = self.display_text();
            // Titles carry paths, so a tail that does not fit ends in the
            // shared mark: a reader can tell a hidden remainder from a name
            // that simply ends there.
            let (fitted, marked) = font.elide_to_width(&combined, layout.title.width);
            let pen = font.draw_text(surface, tx, ty, fitted, text_color);
            if marked {
                font.draw_text(surface, pen, ty, ELLIPSIS, text_color);
            }
        }

        let plate_radius = FrameRim::of(scale, theme).plate().1;
        for (slot, (kind, rect)) in layout.controls().iter().copied().enumerate() {
            self.controls[control_index(kind)].render(
                surface,
                rect,
                scale,
                theme,
                band_corner(slot, plate_radius),
            );
        }
    }

    /// Wash the band with the window's identity hue, fading out from the icon
    /// in both directions.
    ///
    /// The colour is the application's, not the theme's: an icon lends the
    /// chrome the hue that identifies it, so a glance at a bar says which
    /// program owns the window before its title is read. The theme sets only
    /// how strong it is where it starts
    /// ([`title_hue_alpha`](tairix_theme::Palette::title_hue_alpha)) and how
    /// far it travels before it is gone
    /// ([`title_hue_reach`](tairix_theme::Metrics::title_hue_reach)).
    ///
    /// A *reach* rather than a width: the ramp runs out from the icon at the
    /// same rate whatever the window's size, and the band's ends cut it. A wide
    /// bar therefore keeps its far reaches plain instead of stretching one ramp
    /// ever thinner, and a short bar is washed end to end — including behind
    /// the commands, which is where a narrow window needs it most.
    ///
    /// The wash is confined to the shape the frame's rim curves through, so a
    /// band drawn corner to corner cannot square off the window's silhouette.
    /// The mask is the ramp multiplied by that arc, both handed to `lib/raster`
    /// rather than derived here — the arc from the one shared
    /// [`round_rect_coverage`], the band's radius from the one shared
    /// [`FrameRim`].
    fn wash_band(
        &self,
        surface: &mut Surface,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
        layout: &TitleBarLayout,
        active: bool,
    ) {
        let palette = theme.palette();
        let Some(hue) = self.identity_hue else {
            return;
        };
        let alpha = palette.title_hue_alpha;
        if alpha == 0 {
            return;
        }
        let Some((x, y, w, h)) = surface_rect(bounds) else {
            return;
        };
        let reach = scale.scale_length(theme.metrics().title_hue_reach);
        if reach == 0 || w == 0 || h == 0 {
            return;
        }
        // A bar with no icon slot still has an identity group; the hue starts
        // where that group does, so the colour and the thing it came from sit
        // together.
        let source = if layout.icon.width > 0 {
            i32::midpoint(layout.icon.left(), layout.icon.right())
        } else {
            layout.title.left()
        };
        let origin = u32::try_from(source.saturating_sub(bounds.left())).unwrap_or(0);

        let saturation = if active {
            HUE_SATURATION_ACTIVE
        } else {
            HUE_SATURATION_INACTIVE
        };
        // Desaturating is a pixel operation and a hue is a colour, so it is
        // toned while opaque — where premultiplying is exactly the identity —
        // and given its opacity afterwards.
        let toned = Color::rgb(hue.r, hue.g, hue.b)
            .premultiply()
            .desaturate(saturation)
            .unpremultiply();
        let wash = Color::rgba(toned.r, toned.g, toned.b, alpha);

        // The band's top corners are the frame plate's; its bottom edge is
        // interior to the window and square. Rounding a rectangle `radius`
        // taller than the band puts the bottom arcs below it, so only the top
        // two land — concentric with the arc the rim already drew.
        let radius = FrameRim::of(scale, theme).plate().1;
        let shape_h = h.saturating_add(radius);
        surface.wash_region(x, y, w, h, wash, |lx, ly| {
            let distance = lx.abs_diff(origin);
            if distance >= reach {
                return 0;
            }
            let ramp = u8::try_from(255 - (255 * distance / reach)).unwrap_or(255);
            div255(u32::from(ramp) * u32::from(round_rect_coverage(lx, ly, w, shape_h, radius)))
        });
    }

    /// The identity+title string drawn in the identity group.
    fn display_text(&self) -> String {
        if self.app_name.is_empty() {
            self.title.clone()
        } else if self.title.is_empty() {
            self.app_name.clone()
        } else {
            let mut s = self.app_name.clone();
            s.push_str(" — ");
            s.push_str(&self.title);
            s
        }
    }

    /// Classify a point (surface coordinates) within the title bar.
    #[must_use]
    pub fn hit(&self, bounds: Rect, scale: Scale, theme: &Theme, point: Point) -> TitleHit {
        let layout = self.layout(bounds, scale, theme);
        for (kind, rect) in layout.controls().iter().copied() {
            if rect.contains(point) {
                return TitleHit::Control(kind);
            }
        }
        TitleHit::Drag
    }

    /// Feed a pointer event within the title bar `bounds`, returning the typed
    /// [`TitleBarEvent`] it produced.
    ///
    /// A press over a control routes to that control (and can never start a
    /// drag); a press over the drag region activates the window and, once the
    /// pointer moves past the drag threshold, begins and then continues a move
    /// until release.
    pub fn on_pointer(
        &mut self,
        event: &InputEvent,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
        damage: &mut Region,
    ) -> Option<TitleBarEvent> {
        if let InputEvent::PointerMoved { to } = event {
            *self.pointer = *to;
        }
        let layout = self.layout(bounds, scale, theme);

        // Route to every control so hover stays current and an armed control
        // keeps its latch even as the pointer moves off it.
        let mut fired = None;
        for (kind, rect) in layout.controls().iter().copied() {
            if let Some(action) = self.stored_mut(kind).on_pointer(event, rect, damage) {
                fired = Some(action);
            }
        }
        if let Some(action) = fired {
            return Some(match action {
                WindowControlAction::Invoked(kind) => TitleBarEvent::Control(kind),
                WindowControlAction::AlternateInvoked(kind) => {
                    TitleBarEvent::AlternateControl(kind)
                }
            });
        }

        let over_control = layout
            .controls()
            .iter()
            .any(|(_, r)| r.contains(*self.pointer));
        let any_armed = self.seated().iter().any(WindowControl::armed);

        match event {
            InputEvent::PointerPressed {
                button: PointerButton::Primary,
            } => {
                if !over_control && !any_armed && bounds.contains(*self.pointer) {
                    self.press_pending = true;
                    self.dragging = false;
                    *self.press_origin = *self.pointer;
                    return Some(TitleBarEvent::Activate);
                }
                None
            }
            InputEvent::PointerMoved { to } => {
                if self.press_pending {
                    if self.dragging {
                        return Some(TitleBarEvent::DragMoved { to: *to });
                    }
                    let threshold =
                        to_i32(scale.scale_length(theme.metrics().control_inset).max(2));
                    let dx = (to.x - self.press_origin.x).abs();
                    let dy = (to.y - self.press_origin.y).abs();
                    if dx.max(dy) >= threshold {
                        self.dragging = true;
                        return Some(TitleBarEvent::DragBegin);
                    }
                }
                None
            }
            InputEvent::PointerReleased {
                button: PointerButton::Primary,
            } => {
                let was_dragging = self.dragging;
                self.press_pending = false;
                self.dragging = false;
                was_dragging.then_some(TitleBarEvent::DragEnd)
            }
            _ => None,
        }
    }

    /// The pointer has left this title bar: drop every command control's
    /// hover highlight, reporting only the controls that were lit.
    ///
    /// The bar lays itself out here for the same reason
    /// [`on_pointer`](Self::on_pointer) does — a control's own rect is what it
    /// reports repainting — so a leave costs the one control that was under the
    /// pointer and nothing when none was. See
    /// [`WindowControl::pointer_left`] for why a leave cannot be expressed as
    /// a motion somewhere else.
    ///
    /// The title bar's own drag latch is untouched: a drag is a held button,
    /// and a held button holds the pointer.
    pub fn pointer_left(&mut self, bounds: Rect, scale: Scale, theme: &Theme, damage: &mut Region) {
        let layout = self.layout(bounds, scale, theme);
        for (kind, rect) in layout.controls().iter().copied() {
            self.stored_mut(kind).pointer_left(rect, damage);
        }
    }

    /// Feed a key event within the title bar `bounds`. A focused control is
    /// activated with Space/Enter; the left/right arrows move focus between the
    /// enabled controls so the group is fully keyboard-navigable without a
    /// pointer (spec §11.18 furniture keyboard focus).
    ///
    /// The bar lays itself out here for the same reason
    /// [`on_pointer`](Self::on_pointer) does: a control's own rect is what it
    /// reports repainting, so an activation or a focus move costs the two
    /// controls that changed and never the whole strip.
    pub fn on_key(
        &mut self,
        key: Key,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
        damage: &mut Region,
    ) -> Option<TitleBarEvent> {
        let layout = self.layout(bounds, scale, theme);
        for (kind, rect) in layout.controls().iter().copied() {
            if let Some(WindowControlAction::Invoked(invoked)) =
                self.stored_mut(kind).on_key(key, rect, damage)
            {
                return Some(TitleBarEvent::Control(invoked));
            }
        }
        match key {
            Key::Named(NamedKey::Right) => {
                self.move_focus(true, &layout, damage);
                None
            }
            Key::Named(NamedKey::Left) => {
                self.move_focus(false, &layout, damage);
                None
            }
            _ => None,
        }
    }

    /// Move keyboard focus among the controls one slot `forward` (or backward),
    /// skipping disabled controls and wrapping. If no control is focused, the
    /// first step lands on the first (forward) or last (backward) control.
    ///
    /// The ring is then written to every control through the guarded write, so
    /// exactly the controls whose ring changed are reported — the one it left
    /// and the one it reached, never the strip between them, and nothing at all
    /// when it stays put. Writing all four rather than the two this bar's own
    /// invariant predicts costs a comparison each and cannot under-report if a
    /// caller had lit two of them.
    fn move_focus(&mut self, forward: bool, layout: &TitleBarLayout, damage: &mut Region) {
        let count = self.commands.seated();
        if count == 0 {
            return;
        }
        let current = self.seated().iter().position(|c| c.state().focus.focused);
        let mut idx = match current {
            Some(i) => i,
            None if forward => count - 1,
            None => 0,
        };
        let mut landed = None;
        for _ in 0..count {
            idx = if forward {
                (idx + 1) % count
            } else {
                (idx + count - 1) % count
            };
            if self.controls[idx].state().is_actionable() {
                landed = Some(idx);
                break;
            }
        }
        for slot in 0..count {
            let rect = Self::control_rect(layout, slot);
            damage::set(
                &mut self.controls[slot].state.focus.focused,
                landed == Some(slot),
                rect,
                damage,
            );
        }
    }

    /// The laid-out rect of the control stored at `slot`, or [`Rect::EMPTY`]
    /// when the layout has no entry for it (fail closed: an empty rectangle
    /// covers nothing).
    fn control_rect(layout: &TitleBarLayout, slot: usize) -> Rect {
        layout
            .controls()
            .iter()
            .find(|(kind, _)| control_index(*kind) == slot)
            .map_or(Rect::EMPTY, |(_, rect)| *rect)
    }
}

// --- WindowFrame ----------------------------------------------------------

/// A resize edge of the window frame.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ResizeEdge {
    /// The left edge.
    Left,
    /// The right edge.
    Right,
    /// The bottom edge.
    Bottom,
    /// The bottom-left corner.
    BottomLeft,
    /// The bottom-right corner.
    BottomRight,
}

/// How far a resizable window's resize zones reach in from its outer edges,
/// in physical pixels at a given scale and theme.
///
/// Two reaches, not one: an *edge* zone only has to be wide enough to grab,
/// while a *corner* zone is a square where two edges meet and would otherwise
/// shrink to that width at its tip — the hardest thing on the frame to hit,
/// and the one that resizes both axes at once. The corner is therefore the
/// wider of the two, and is clamped never to fall below the edge reach so the
/// very corner can never classify as a plain edge.
///
/// Measured from the **outer** rectangle, so the thin drawn furniture band and
/// the client pixels the zone reaches over are one continuous region rather
/// than two zones that could disagree about where a corner ends.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub struct GrabReach {
    /// The left/right/bottom edge reach, at least one pixel so a resizable
    /// window always has a grabbable edge.
    pub edge: u32,
    /// The bottom-left/bottom-right corner reach along each axis.
    pub corner: u32,
}

impl GrabReach {
    /// The reaches a resizable window's frame grabs by at `scale` under
    /// `theme`.
    #[must_use]
    pub fn of(scale: Scale, theme: &Theme) -> Self {
        let metrics = theme.metrics();
        let edge = scale.scale_length(metrics.resize_edge_grab).max(1);
        Self {
            edge,
            corner: scale.scale_length(metrics.resize_corner_grab).max(edge),
        }
    }

    /// The resize edge (or corner) `point` falls in within the outer
    /// rectangle `bounds`, or `None` when it is clear of every edge.
    ///
    /// A corner wins over the two edges that form it, and the top edge is
    /// never a resize edge — the title bar lives there, and it is resolved
    /// before this is reached.
    fn edge_at(self, bounds: Rect, point: Point) -> Option<ResizeEdge> {
        let near_left = |reach: u32| point.x < bounds.left().saturating_add(to_i32(reach));
        let near_right = |reach: u32| point.x >= bounds.right().saturating_sub(to_i32(reach));
        let near_bottom = |reach: u32| point.y >= bounds.bottom().saturating_sub(to_i32(reach));
        if near_bottom(self.corner) {
            if near_left(self.corner) {
                return Some(ResizeEdge::BottomLeft);
            }
            if near_right(self.corner) {
                return Some(ResizeEdge::BottomRight);
            }
        }
        if near_bottom(self.edge) {
            return Some(ResizeEdge::Bottom);
        }
        if near_left(self.edge) {
            return Some(ResizeEdge::Left);
        }
        if near_right(self.edge) {
            return Some(ResizeEdge::Right);
        }
        None
    }
}

/// Where a point falls within a window frame.
///
/// This is the frame's furniture hit map: it classifies every point as either
/// the client viewport or a specific furniture part, so an application-drawn
/// lookalike inside the client area can never receive input meant for the
/// frame, and the client can never receive furniture input (spec §11.17). On a
/// resizable window the client's own outermost pixels double as part of this
/// map: [`ResizeEdge`] deliberately overlaps them (see [`WindowFrame::hit`]),
/// so the app still draws every client pixel but no longer receives presses
/// on the few it traded away for a grabbable edge.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum FurniturePart {
    /// Outside the whole window.
    Outside,
    /// Over the client viewport (the only part delivered to the application).
    Client,
    /// Over the draggable title region.
    TitleBar,
    /// Over one of the window-command controls.
    WindowControl(WindowControlKind),
    /// Over a resizable frame edge (only when the window is resizable).
    ResizeEdge(ResizeEdge),
    /// Over the inert frame border.
    Frame,
}

/// The laid-out rectangles of a window frame.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct FrameLayout {
    /// The full outer bounds.
    pub outer: Rect,
    /// The title-bar region (inside the frame rim).
    pub title_bar: Rect,
    /// The client viewport (application-owned; never receives furniture input).
    pub client: Rect,
}

/// The per-edge thickness of a window frame's furniture band around its client
/// viewport, in physical pixels at a given scale and theme.
///
/// This is the single definition [`WindowFrame::layout`] and
/// [`WindowFrame::outer_for_client`] both derive from: an outer rectangle grown
/// from a client rectangle by these insets satisfies
/// `layout(outer).client == client`, so a window manager can size a decorated
/// window's outer bounds from its client-sized content surface without
/// re-deriving the frame metrics.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub struct FrameInsets {
    /// Top band: the frame border plus the title bar.
    pub top: u32,
    /// Left band: the frame inset.
    pub left: u32,
    /// Right band: the frame inset.
    pub right: u32,
    /// Bottom band: the frame inset.
    pub bottom: u32,
}

/// The line a window frame draws its shape with, in physical pixels at a given
/// scale and theme: how far the corners round and how thick the rim is.
///
/// A window's *silhouette* is this radius, and a rectangular client cannot be
/// drawn over the arc it curves around, so a window manager reads it to cut the
/// client to the [`plate`](Self::plate) the frame fills inside the rim — the
/// only region client pixels belong in.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub struct FrameRim {
    /// The outer corner radius; `0` for square corners.
    pub radius: u32,
    /// The rim line's thickness, never less than one pixel.
    pub thickness: u32,
}

impl FrameRim {
    /// The rim a frame draws its shape with at `scale` under `theme`.
    ///
    /// An associated function rather than a method because nothing about it
    /// depends on a particular frame: it is the house window shape. That is
    /// what lets the [`TitleBar`] seat a command hard against the band's end
    /// and round it by the very arc [`WindowFrame::render`] laid, instead of
    /// re-deriving the radius from the metrics and drifting from it.
    #[must_use]
    pub fn of(scale: Scale, theme: &Theme) -> Self {
        let (thickness, _, _) = WindowFrame::edges(scale, theme);
        Self {
            radius: scale.scale_length(theme.metrics().window_corner_radius),
            thickness,
        }
    }

    /// The plate the frame fills inside the rim, as `(inset, radius)`: inset
    /// from the window's outer rectangle by the rim's thickness, with a
    /// concentric radius, so the rim keeps its weight around the whole arc.
    #[must_use]
    pub const fn plate(self) -> (u32, u32) {
        (self.thickness, self.radius.saturating_sub(self.thickness))
    }
}

/// The window-manager-owned boundary around one client viewport (spec §11.17).
///
/// It draws the Frame Rim — one quiet neutral at every activation, with a
/// bounded attention dot on an attention request and never an indefinite
/// pulse — owns the [`TitleBar`], and exposes the client rectangle the
/// compositor clips the application into. Drawing stays strictly separate:
/// the frame never paints a client pixel and the app never paints a furniture
/// one. The **hit** map is deliberately looser on a resizable window — its
/// resize edges reach into the client's outer pixels rather than reserving a
/// visible band for them, so grabbing an edge costs no space the content
/// could otherwise fill (see [`Self::hit`]). Focus is the title bar's to
/// show (its text brightens), joined under heavy contrast by a doubled inner
/// rim line so the distinction is a difference in shape too. Activation,
/// theme, and hover never change the client origin or the outer dimensions.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WindowFrame {
    furniture: WindowFurnitureState,
    title_bar: TitleBar,
}

impl WindowFrame {
    /// A window frame for the given furniture state.
    #[must_use]
    pub fn new(furniture: WindowFurnitureState) -> Self {
        Self {
            furniture,
            title_bar: TitleBar::new(furniture),
        }
    }

    /// The window's furniture state.
    #[must_use]
    pub fn furniture(&self) -> WindowFurnitureState {
        self.furniture
    }

    /// Replace the furniture state, updating the title bar and its controls.
    pub fn set_furniture(&mut self, furniture: WindowFurnitureState) {
        self.furniture = furniture;
        self.title_bar.set_furniture(furniture);
    }

    /// A shared reference to the frame's title bar.
    #[must_use]
    pub fn title_bar(&self) -> &TitleBar {
        &self.title_bar
    }

    /// A mutable reference to the frame's title bar (title, controls, input).
    pub fn title_bar_mut(&mut self) -> &mut TitleBar {
        &mut self.title_bar
    }

    /// The pixel side a decorated window's title-bar identity icon draws at.
    ///
    /// The title band's height and the font that sizes the slot inside it both
    /// come from `theme` at `scale`, never from a particular window, so this is
    /// the answer for *every* decorated window. That is what lets an owner have
    /// an application's artwork decoded before the window that will wear it
    /// exists, and it is the one definition of the side, so a frame already on
    /// screen and one not yet created cannot disagree about it.
    #[must_use]
    pub fn identity_icon_side(scale: Scale, theme: &Theme) -> u32 {
        let (_, title_h, _) = Self::edges(scale, theme);
        TitleBar::icon_side(Rect::new(0, 0, 0, title_h), scale, theme)
    }

    /// The three scaled frame metrics —
    /// `(border, title_bar_height, frame_inset)` in physical pixels — every
    /// frame rectangle is built from. The border is at least one physical pixel
    /// and the side inset is never thinner than the border, so a rim always
    /// draws.
    fn edges(scale: Scale, theme: &Theme) -> (u32, u32, u32) {
        let metrics = theme.metrics();
        let border = scale.scale_length(metrics.border_thickness).max(1);
        let inset_amt = scale.scale_length(metrics.frame_inset).max(border);
        (border, TitleBar::band_height(scale, theme), inset_amt)
    }

    /// The left/right/bottom furniture-band thickness around the client.
    ///
    /// This is the plain frame inset for every window, resizable or not: a
    /// visible band wide enough to grab would waste space an app's content
    /// could otherwise fill (the resize hit zone reaches over the client's
    /// outer pixels instead — see [`GrabReach`]). Never thinner than the
    /// frame border, so a rim always draws.
    fn band_inset(scale: Scale, theme: &Theme) -> u32 {
        let (_, _, inset_amt) = Self::edges(scale, theme);
        inset_amt
    }

    /// The rim this frame draws its shape with, at the active scale and theme.
    ///
    /// [`Self::render`] draws from it, and a window manager reads it to cut the
    /// application's rectangular client to the plate inside the rim, so the
    /// content can never square off the corners the rim curves around.
    #[must_use]
    pub fn rim(&self, scale: Scale, theme: &Theme) -> FrameRim {
        FrameRim::of(scale, theme)
    }

    /// The per-edge furniture-band thickness around the client, at the active
    /// scale and theme.
    ///
    /// The top band carries the frame border and the title bar; the other
    /// three carry the plain frame inset, the same for a resizable and a
    /// fixed-size window — a resizable window's extra grab room lives in the
    /// hit map ([`Self::hit`]), never in this drawn geometry. This is the one
    /// definition [`Self::layout`] and [`Self::outer_for_client`] share (they
    /// never restate the metric math).
    #[must_use]
    pub fn insets(&self, scale: Scale, theme: &Theme) -> FrameInsets {
        let (border, title_h, _) = Self::edges(scale, theme);
        let band = Self::band_inset(scale, theme);
        FrameInsets {
            top: border.saturating_add(title_h),
            left: band,
            right: band,
            bottom: band,
        }
    }

    /// The smallest outer rectangle this frame may be given, in physical
    /// pixels: `(width, height)`.
    ///
    /// The width is what the title bar needs to seat its commands and keep a
    /// drag surface between them ([`TitleBar::min_band_width`]) plus the rim
    /// either side of the band; the height is the furniture bands plus one
    /// standard control of client, the theme's own minimum interactive
    /// target. Both axes leave at least that much client, so a window at the
    /// floor is still a window rather than a strip of chrome.
    ///
    /// This is the frame's own floor. It bounds a *user* resize; an
    /// application's declared minimum is separate and larger, and a window
    /// manager honours whichever is greater.
    #[must_use]
    pub fn min_outer_size(&self, scale: Scale, theme: &Theme) -> (u32, u32) {
        let (border, _, _) = Self::edges(scale, theme);
        let insets = self.insets(scale, theme);
        let client = scale.scale_length(theme.metrics().control_height).max(1);
        let sides = insets.left.saturating_add(insets.right);
        let band = TitleBar::min_band_width(TitleBarCommands::Window, scale, theme)
            .saturating_add(border.saturating_mul(2));
        (
            band.max(sides.saturating_add(client)),
            insets
                .top
                .saturating_add(insets.bottom)
                .saturating_add(client),
        )
    }

    /// The outer window rectangle whose client viewport is exactly `client`:
    /// `client` grown by the furniture band ([`Self::insets`]) on every edge.
    ///
    /// This is the window manager's inverse of [`Self::layout`] — it sizes a
    /// decorated window's outer bounds from its client-sized content surface —
    /// and it round-trips:
    /// `self.layout(self.outer_for_client(client, ..), ..).client == client`.
    #[must_use]
    pub fn outer_for_client(&self, client: Rect, scale: Scale, theme: &Theme) -> Rect {
        let insets = self.insets(scale, theme);
        Rect::new(
            client.left() - to_i32(insets.left),
            client.top() - to_i32(insets.top),
            client
                .width
                .saturating_add(insets.left)
                .saturating_add(insets.right),
            client
                .height
                .saturating_add(insets.top)
                .saturating_add(insets.bottom),
        )
    }

    /// Lay the frame out within `bounds` for the active theme.
    ///
    /// The rim thickness and client inset are independent of activation, so a
    /// window never shifts its client when it gains or loses focus.
    #[must_use]
    pub fn layout(&self, bounds: Rect, scale: Scale, theme: &Theme) -> FrameLayout {
        let (b, title_h, _) = Self::edges(scale, theme);
        let band = Self::band_inset(scale, theme);

        let title_bar = Rect::new(
            bounds.left() + to_i32(b),
            bounds.top() + to_i32(b),
            bounds.width.saturating_sub(b.saturating_mul(2)),
            title_h,
        );
        let client = Rect::new(
            bounds.left() + to_i32(band),
            bounds.top() + to_i32(b) + to_i32(title_h),
            bounds.width.saturating_sub(band.saturating_mul(2)),
            bounds
                .height
                .saturating_sub(b)
                .saturating_sub(title_h)
                .saturating_sub(band),
        );

        FrameLayout {
            outer: bounds,
            title_bar,
            client,
        }
    }

    /// Classify a point (surface coordinates) against the frame's hit map.
    ///
    /// The client *viewport* stays exactly [`Self::layout`]'s `client` rect —
    /// an app's content is never inset further than the plain frame band. The
    /// resize **hit** zone is deliberately wider: on a resizable window it
    /// reaches in from the outer edge far enough to grab
    /// ([`GrabReach`]), which over a thin band means it overlaps the client's
    /// outermost pixels. A press landing there is reported as
    /// [`FurniturePart::ResizeEdge`], not [`FurniturePart::Client`] — those
    /// outermost app pixels are still drawn by the app but no longer deliver
    /// pointer input to it, the accepted trade-off for an invisible border.
    ///
    /// The title bar is resolved first and keeps its whole band: a window is
    /// dragged from its title bar far more often than it is resized from the
    /// sliver of edge beside one, so the resize zones start below it.
    #[must_use]
    pub fn hit(&self, bounds: Rect, scale: Scale, theme: &Theme, point: Point) -> FurniturePart {
        if !bounds.contains(point) {
            return FurniturePart::Outside;
        }
        let layout = self.layout(bounds, scale, theme);
        if layout.title_bar.contains(point) {
            return match self.title_bar.hit(layout.title_bar, scale, theme, point) {
                TitleHit::Control(kind) => FurniturePart::WindowControl(kind),
                TitleHit::Drag => FurniturePart::TitleBar,
            };
        }
        let inside = if layout.client.contains(point) {
            FurniturePart::Client
        } else {
            FurniturePart::Frame
        };
        if !self.furniture.resizable {
            return inside;
        }
        GrabReach::of(scale, theme)
            .edge_at(bounds, point)
            .map_or(inside, FurniturePart::ResizeEdge)
    }

    /// Paint the frame chrome (rim, body background, title bar) into `surface`
    /// at `bounds`. The client viewport is left for the compositor to clip the
    /// application into; the frame never paints client pixels.
    ///
    /// `bounds` is the whole decorated window's outer rectangle, not the title
    /// band. `artwork` is the owning application's identity icon, pre-rasterised
    /// at [`TitleBar::icon_side`] of the *laid-out title band*, and is handed
    /// straight to [`TitleBar::render`].
    pub fn render(
        &self,
        surface: &mut Surface,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
        artwork: Option<IconPicture<'_>>,
    ) {
        let Some((x, y, w, h)) = surface_rect(bounds) else {
            return;
        };
        if w == 0 || h == 0 {
            return;
        }
        let palette = theme.palette();
        let metrics = theme.metrics();
        let rim = self.rim(scale, theme);
        let border = rim.thickness;
        let active = self.furniture.activation != WindowActivationState::Inactive;

        // Frame Rim: one quiet neutral, whatever the activation. The rim is
        // the line the eye reads a window's shape by, so brightening it on
        // focus made the boundary the loudest mark on the desktop and left
        // every other window looking switched off. Focus is the title bar's
        // to carry.
        surface.fill_round_rect(x, y, w, h, rim.radius, Color::from(palette.frame));

        // Window body behind the title bar and client viewport. It is the plate
        // the window manager cuts the client to, so both read one definition of
        // where the arc leaves room for content.
        let (plate_inset, plate_radius) = rim.plate();
        if let Some((ix, iy, iw, ih)) = inset(x, y, w, h, plate_inset) {
            surface.fill_round_rect(ix, iy, iw, ih, plate_radius, Color::from(palette.surface));
            // In high contrast the active frame adds a doubled inner rim line
            // in the muted foreground, so focus reads as a difference in shape
            // and not only as the title tone; it never changes frame
            // measurements. Outside high contrast the frame stays a single flat
            // line and the title bar carries the distinction alone.
            if active && heavy_contrast(theme) {
                draw_outline(
                    surface,
                    ix,
                    iy,
                    iw,
                    ih,
                    border,
                    Color::from(palette.on_surface_muted),
                );
            }
        }

        let layout = self.layout(bounds, scale, theme);
        self.title_bar
            .render(surface, layout.title_bar, scale, theme, artwork);

        // A bounded attention dot on the trailing edge of the title bar — never
        // an indefinite pulse (spec §11.17). Static, so it is reduced-motion
        // correct.
        if self.furniture.activation == WindowActivationState::AttentionRequested {
            let size = scale.scale_length(metrics.bead_size).max(4);
            if let Some((tx, ty, tw, _)) = surface_rect(layout.title_bar) {
                let bx = tx + tw.saturating_sub(size).saturating_sub(border);
                let by = ty + border;
                surface.fill_round_rect(bx, by, size, size, size / 2, Color::from(palette.accent));
            }
        }
    }
}

// --- ResizeGrabber --------------------------------------------------------

/// The outcome of interacting with a [`ResizeGrabber`].
///
/// The grabber reports the resize gesture; the window manager enforces the
/// typed minimum/maximum/aspect/work-area constraints before presenting each
/// new rectangle (spec §11.23). The gesture can be cancelled (Escape while
/// dragging), which leaves the window at its pre-drag geometry.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ResizeEvent {
    /// A resize gesture began and the pointer should be captured.
    Begin,
    /// The gesture continued to `to` (screen coordinates).
    Moved {
        /// The new pointer position.
        to: Point,
    },
    /// The gesture ended (pointer released).
    End,
    /// The gesture was cancelled (Escape); restore the pre-drag geometry.
    Cancel,
}

/// The corner resize drag gesture for resizable windows (spec §11.23).
///
/// It captures a pointer drag from its hit region and can paint Grip Teeth —
/// a shape mark that reads without colour — into a caller-supplied affordance
/// rectangle. A resizable window's furniture band is now the plain frame
/// inset, too thin to hold that mark without painting into the client, so the
/// window manager's own chrome no longer calls [`Self::render`]: the corner's
/// grab zone is invisible, carried entirely by [`WindowFrame::hit`]'s client
/// overlap. A host with room for a visible affordance may still call
/// [`Self::render`]. A non-resizable or maximized window
/// disables the grabber; a disabled grabber ignores input (fail closed).
/// Geometry follows the pointer with no easing, so it is reduced-motion
/// correct.
///
/// Equal grabbers draw the same pixels, so a host may use `==` as its repaint
/// gate: the visible state, whether the frame is active, and — unlike the
/// other furniture — the `dragging` flag all compare, because a grabber in a
/// drag draws its teeth in the pressed treatment. Only the pointer coordinate
/// is excluded; no render path reads it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResizeGrabber {
    state: ControlState,
    active_frame: bool,
    /// The last pointer position — hit-testing input, never drawn.
    pointer: RenderInvariant<Point>,
    dragging: bool,
}

impl Default for ResizeGrabber {
    fn default() -> Self {
        Self::new()
    }
}

impl ResizeGrabber {
    /// A resize grabber in the resting, enabled state.
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: ControlState::idle(),
            active_frame: true,
            pointer: RenderInvariant::new(Point::ORIGIN),
            dragging: false,
        }
    }

    /// The grabber's composed state.
    #[must_use]
    pub fn state(&self) -> ControlState {
        self.state
    }

    /// Replace the grabber's composed state.
    pub fn set_state(&mut self, state: ControlState) {
        self.state = state;
        if !self.state.is_actionable() {
            self.dragging = false;
        }
    }

    /// Enable or disable the grabber (a maximized or non-resizable window
    /// disables it, spec §11.23).
    pub fn set_enabled(&mut self, enabled: bool) {
        self.state.enabled = enabled;
        if !enabled {
            self.dragging = false;
        }
    }

    /// Whether the frame is active (lowers idle contrast when inactive).
    pub fn set_active_frame(&mut self, active: bool) {
        self.active_frame = active;
    }

    /// Whether a resize drag is currently in progress.
    #[must_use]
    pub fn is_dragging(&self) -> bool {
        self.dragging
    }

    /// Paint the Grip Teeth into `surface` within the visible affordance
    /// `bounds` for the active theme. The teeth are authored on the shared
    /// glyph design grid and grid-fitted to the affordance the window manager
    /// already sized through the shared [`Scale`], so they scale with it while
    /// keeping a whole-pixel stroke weight.
    pub fn render(&self, surface: &mut Surface, bounds: Rect, _scale: Scale, theme: &Theme) {
        let Some((x, y, w, h)) = surface_rect(bounds) else {
            return;
        };
        let side = w.min(h);
        if side == 0 {
            return;
        }
        let palette = theme.palette();
        let color = match self.state.disposition() {
            ControlDisposition::DisabledByState => Color::from(palette.on_surface_muted),
            ControlDisposition::DeniedByAuthority => Color::from(palette.denied),
            _ if self.dragging || self.state.pointer == PointerState::Pressed => {
                Color::from(palette.rim_active)
            }
            _ if self.state.pointer == PointerState::Hover || !self.active_frame => {
                Color::from(if self.active_frame {
                    palette.on_surface
                } else {
                    palette.on_surface_muted
                })
            }
            _ => Color::from(palette.on_surface),
        };
        let weight = if heavy_contrast(theme) { 16 } else { 10 };
        // The square glyph box sits at the bottom-right of the affordance.
        let origin = (x + w.saturating_sub(side), y + h.saturating_sub(side));
        let glyph = Glyph::new(origin, side, weight);
        // Three diagonal teeth parallel to the bottom-right corner.
        glyph.diagonal(surface, (55, 95), (95, 55), color);
        glyph.diagonal(surface, (72, 95), (95, 72), color);
        glyph.diagonal(surface, (38, 95), (95, 38), color);
    }

    /// Feed a pointer event, given the grabber's current hit region
    /// `hit_bounds`, returning the resize gesture it produced. A press begins
    /// and captures the drag; motion continues it; release ends it. A disabled
    /// or denied grabber ignores input (fail closed).
    ///
    /// Both the pointer look and the `dragging` flag are drawn in the teeth, so
    /// each is a guarded write against `hit_bounds`: motion that only carries a
    /// drag forward paints the same teeth and reports nothing.
    pub fn on_pointer(
        &mut self,
        event: &InputEvent,
        hit_bounds: Rect,
        damage: &mut Region,
    ) -> Option<ResizeEvent> {
        if let InputEvent::PointerMoved { to } = event {
            *self.pointer = *to;
        }
        let inside = hit_bounds.contains(*self.pointer);
        let hover_or_none = if inside {
            PointerState::Hover
        } else {
            PointerState::None
        };
        match event {
            InputEvent::PointerMoved { to } => {
                if self.dragging {
                    Some(ResizeEvent::Moved { to: *to })
                } else {
                    damage::set(&mut self.state.pointer, hover_or_none, hit_bounds, damage);
                    None
                }
            }
            InputEvent::PointerPressed {
                button: PointerButton::Primary,
            } => {
                if inside && self.state.is_actionable() {
                    damage::set(&mut self.dragging, true, hit_bounds, damage);
                    damage::set(
                        &mut self.state.pointer,
                        PointerState::Pressed,
                        hit_bounds,
                        damage,
                    );
                    Some(ResizeEvent::Begin)
                } else {
                    None
                }
            }
            InputEvent::PointerReleased {
                button: PointerButton::Primary,
            } => {
                let was = self.dragging;
                damage::set(&mut self.dragging, false, hit_bounds, damage);
                damage::set(&mut self.state.pointer, hover_or_none, hit_bounds, damage);
                was.then_some(ResizeEvent::End)
            }
            _ => None,
        }
    }

    /// Feed a key event: Escape cancels an in-flight resize (restoring the
    /// pre-drag geometry), so a keyboard escape works exactly like a pointer
    /// cancel.
    ///
    /// The cancel drops the drag *and* the pressed look, and the drag is drawn
    /// in the teeth in its own right — so it is guarded too. Cancelling with
    /// the pointer already away from the corner changes only the drag, and that
    /// still has to be reported.
    pub fn on_key(
        &mut self,
        key: Key,
        hit_bounds: Rect,
        damage: &mut Region,
    ) -> Option<ResizeEvent> {
        if self.dragging && key == Key::Named(NamedKey::Escape) {
            damage::set(&mut self.dragging, false, hit_bounds, damage);
            damage::set(
                &mut self.state.pointer,
                PointerState::None,
                hit_bounds,
                damage,
            );
            Some(ResizeEvent::Cancel)
        } else {
            None
        }
    }
}

// --- ScrollCorner ---------------------------------------------------------

/// The neutral plate at the junction of two visible scrollbars (spec §11.31).
///
/// On a non-resizable window the junction cell is a quiet Alloy Plate with no
/// hidden scroll or resize action; a resizable window uses a [`ResizeGrabber`]
/// there instead. It never receives line-step or page-step input meant for a
/// scrollbar track — it is inert by construction (it has no input method).
#[derive(Copy, Clone, Debug, Eq, PartialEq, Default)]
pub struct ScrollCorner {
    active_frame: bool,
}

impl ScrollCorner {
    /// A neutral scroll corner on an active frame.
    #[must_use]
    pub fn new() -> Self {
        Self { active_frame: true }
    }

    /// Whether the frame is active (only affects the quiet fill contrast).
    pub fn set_active_frame(&mut self, active: bool) {
        self.active_frame = active;
    }

    /// Paint the neutral corner plate into `surface` at `bounds`.
    pub fn render(&self, surface: &mut Surface, bounds: Rect, _scale: Scale, theme: &Theme) {
        let Some((x, y, w, h)) = surface_rect(bounds) else {
            return;
        };
        if w == 0 || h == 0 {
            return;
        }
        let palette = theme.palette();
        let fill = if self.active_frame {
            palette.surface
        } else {
            palette.scroll_track
        };
        surface.fill_rect(x, y, w, h, Color::from(fill));
    }
}
