//! The window-manager furniture family (spec §11.17–§11.23, §11.31).
//!
//! These are the window-manager-owned controls around one client viewport:
//! the [`WindowFrame`] boundary, the [`TitleBar`] with its window-command
//! group, the compact [`WindowControl`] buttons (close, minimize,
//! put-to-back, size-toggle), the [`ResizeGrabber`], and the neutral
//! [`ScrollCorner`] at a two-scrollbar junction.
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

use tairix_font::BitmapFont;
use tairix_geometry::{Point, Rect, Scale};
use tairix_input::{InputEvent, Key, NamedKey, PointerButton};
use tairix_raster::{Color, Surface};
use tairix_theme::Theme;

use crate::paint::{
    draw_outline, heavy_contrast, inset, key_activation, paint_bead, paint_plate, plate_border,
    pointer_activation, resolve_bead, resolve_frame, surface_rect, to_i32, PlateStyle,
};
use crate::state::{
    ControlDisposition, ControlRole, ControlState, PointerState, SizeAction, WindowActivationState,
    WindowControlKind, WindowFurnitureState,
};

// --- command glyphs -------------------------------------------------------
//
// The furniture command glyphs are drawn here once (as `paint::paint_chevron`
// is), rather than added to the notification-icon set in `lib/icon`: they are
// window-control primitives, not the taskbar's status vocabulary. Each is a
// crisp geometric mark that reads without colour (spec §15): the close cross,
// the minimize bar, the maximize/restore squares, and the put-to-back stacked
// plates, so a monochrome theme still tells the four commands apart.

/// The design-grid side every window-command glyph and grip-tooth is authored
/// on; the glyph rasterises across whatever pixel box the control gives it.
const GLYPH_GRID: u32 = 100;

/// The floor of the square root of a small non-negative `n`, by Newton's
/// method. Used to normalise a diagonal stroke's perpendicular; a first-party
/// helper because the workspace minimum Rust version predates `i32::isqrt`.
fn isqrt(n: i32) -> i32 {
    if n <= 0 {
        return 0;
    }
    let mut x = n;
    let mut prev = 0;
    while x != prev {
        prev = x;
        x = i32::midpoint(x, n / x);
    }
    x
}

/// Draw a filled diagonal stroke from `a` to `b` on a `design`-grid
/// sub-surface, `thick` grid units wide, so a glyph scales with its box at any
/// density. The stroke is a quad: both endpoints offset by half the thickness
/// along the line's true perpendicular, so a rising *and* a falling diagonal
/// both draw (a fixed offset would collapse one of them to a zero-area line).
fn diagonal(
    glyph: &mut Surface,
    design: u32,
    a: (i32, i32),
    b: (i32, i32),
    thick: i32,
    color: Color,
) {
    let (ax, ay) = a;
    let (bx, by) = b;
    let dx = bx - ax;
    let dy = by - ay;
    let len = isqrt(dx * dx + dy * dy);
    if len == 0 {
        return;
    }
    let h = (thick / 2).max(1);
    // Perpendicular to (dx, dy) is (-dy, dx), scaled to the half-thickness.
    let ox = -dy * h / len;
    let oy = dx * h / len;
    let points = [
        (ax + ox, ay + oy),
        (ax - ox, ay - oy),
        (bx - ox, by - oy),
        (bx + ox, by + oy),
    ];
    glyph.fill_polygon(&points, design, color);
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
    let Some(mut glyph) = Surface::new(side, side) else {
        return;
    };
    let thick = if heavy { 20 } else { 12 };
    match kind {
        WindowControlKind::Close => {
            diagonal(&mut glyph, GLYPH_GRID, (22, 22), (78, 78), thick, color);
            diagonal(&mut glyph, GLYPH_GRID, (78, 22), (22, 78), thick, color);
        }
        WindowControlKind::Minimize => {
            stroke_bar(&mut glyph, GLYPH_GRID, 20, 80, 62, thick, color);
        }
        WindowControlKind::SizeToggle => match next {
            SizeAction::Maximize => {
                square_outline(&mut glyph, GLYPH_GRID, 22, 22, 56, thick, color);
            }
            SizeAction::Restore => {
                // Two overlapping square outlines: a back square up-right and
                // a front square down-left — the classic restore mark.
                square_outline(&mut glyph, GLYPH_GRID, 34, 18, 44, thick, color);
                square_outline(&mut glyph, GLYPH_GRID, 18, 34, 44, thick, color);
            }
        },
        WindowControlKind::PutToBack => {
            // A filled front plate low-left going behind an outlined back
            // plate high-right — a "send to back / down" cue distinct from
            // the restore mark's two outlines.
            square_outline(&mut glyph, GLYPH_GRID, 40, 16, 42, thick, color);
            filled_square(&mut glyph, GLYPH_GRID, 16, 40, 42, color);
        }
    }
    surface.blit(to_i32(x), to_i32(y), &glyph);
}

/// A horizontal bar `thick` grid units tall, centred vertically at `cy`,
/// spanning `[x0, x1]` on a `design`-grid sub-surface.
fn stroke_bar(
    glyph: &mut Surface,
    design: u32,
    x0: i32,
    x1: i32,
    cy: i32,
    thick: i32,
    color: Color,
) {
    let t = thick.max(1);
    let points = [
        (x0, cy - t / 2),
        (x1, cy - t / 2),
        (x1, cy + t / 2),
        (x0, cy + t / 2),
    ];
    glyph.fill_polygon(&points, design, color);
}

/// A hollow square outline of side `s` at `(ox, oy)` with `thick`-unit edges,
/// on a `design`-grid sub-surface (four bars, so the interior stays open).
fn square_outline(
    glyph: &mut Surface,
    design: u32,
    ox: i32,
    oy: i32,
    s: i32,
    thick: i32,
    color: Color,
) {
    let t = thick.max(1);
    // top, bottom, left, right bars.
    let top = [(ox, oy), (ox + s, oy), (ox + s, oy + t), (ox, oy + t)];
    let bottom = [
        (ox, oy + s - t),
        (ox + s, oy + s - t),
        (ox + s, oy + s),
        (ox, oy + s),
    ];
    let left = [(ox, oy), (ox + t, oy), (ox + t, oy + s), (ox, oy + s)];
    let right = [
        (ox + s - t, oy),
        (ox + s, oy),
        (ox + s, oy + s),
        (ox + s - t, oy + s),
    ];
    glyph.fill_polygon(&top, design, color);
    glyph.fill_polygon(&bottom, design, color);
    glyph.fill_polygon(&left, design, color);
    glyph.fill_polygon(&right, design, color);
}

/// A filled square of side `s` at `(ox, oy)` on a `design`-grid sub-surface.
fn filled_square(glyph: &mut Surface, design: u32, ox: i32, oy: i32, s: i32, color: Color) {
    let points = [(ox, oy), (ox + s, oy), (ox + s, oy + s), (ox, oy + s)];
    glyph.fill_polygon(&points, design, color);
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
}

/// One compact window-command furniture button (spec §11.19–§11.22).
///
/// Built from the shared button behaviour (the `crate::paint` plate,
/// focus-ring, press-latch, and keyboard-activation core the [`crate::Button`]
/// family uses), it draws the command's glyph on a quiet plate that brightens
/// on hover/focus/press and mutes on an inactive frame, and carries the §13
/// authority treatment (a denied control keeps its slot and shows the lock
/// bead rather than looking merely disabled). A [`WindowControlKind::SizeToggle`]
/// shows the glyph and accessible name of the action it will perform *next*.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WindowControl {
    kind: WindowControlKind,
    state: ControlState,
    next_size: SizeAction,
    active_frame: bool,
    pointer: Point,
    armed: bool,
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
            pointer: Point::ORIGIN,
            armed: false,
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
    /// window's [`WindowSizeState`](crate::WindowSizeState) so the glyph and
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

    /// Paint the control into `surface` at `bounds` for the active theme.
    pub fn render(&self, surface: &mut Surface, bounds: Rect, scale: Scale, theme: &Theme) {
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
        let frame = resolve_frame(theme, ControlRole::Neutral, self.state);

        // The plate is drawn only when the control is awake (hover/focus/press)
        // or non-interactive (disabled/denied/failed/pending); an idle control
        // on the title bar shows just its glyph on the bar's own surface.
        let show_plate = !interactive || awake;
        let radius = scale.scale_length(theme.metrics().control_corner_radius);
        let border = plate_border(theme, scale);
        if show_plate {
            paint_plate(
                surface,
                (x, y, w, h),
                &PlateStyle {
                    radius,
                    border,
                    plate: frame.plate,
                    rim: frame.rim,
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

        // The §13 Authority Mark / recovery / complete bead, top-trailing.
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
    /// [`WindowControlAction::Invoked`] on a completed primary click. The
    /// press is never forwarded to the client surface (spec §11.18).
    ///
    /// On the release that completes a click the control returns to rest — its
    /// hover/press highlight and any keyboard focus ring are cleared — so a
    /// furniture button loses its border once its command fires, the way a
    /// desktop title-bar control does. A genuine hover is re-established by the
    /// next pointer move if the pointer still lies over the control, so this
    /// only drops the *stale* highlight left when activation relocates the
    /// control (a size toggle) or takes the frame away (close/minimise/back).
    pub fn on_pointer(&mut self, event: &InputEvent, bounds: Rect) -> Option<WindowControlAction> {
        if let InputEvent::PointerMoved { to } = event {
            self.pointer = *to;
        }
        let inside = bounds.contains(self.pointer);
        if pointer_activation(&mut self.state, &mut self.armed, event, inside) {
            self.rest();
            Some(WindowControlAction::Invoked(self.kind))
        } else {
            None
        }
    }

    /// Feed a key event, returning [`WindowControlAction::Invoked`] when a
    /// focused, actionable control is activated with Space or Enter.
    ///
    /// Activation clears the control's focus ring, so the border shows only
    /// while the group is being navigated with the keyboard, not after the
    /// command has fired.
    pub fn on_key(&mut self, key: Key) -> Option<WindowControlAction> {
        if key_activation(self.state, key) {
            self.rest();
            Some(WindowControlAction::Invoked(self.kind))
        } else {
            None
        }
    }

    /// Return the control to rest after its command fires: drop the pointer
    /// hover/press highlight and the keyboard focus ring so no border lingers
    /// once activation completes.
    fn rest(&mut self) {
        self.state.pointer = PointerState::None;
        self.state.focus.focused = false;
        self.armed = false;
    }

    /// Whether the control currently holds a captured press latch.
    #[must_use]
    fn armed(&self) -> bool {
        self.armed
    }
}

// --- TitleBar -------------------------------------------------------------

/// The canonical window-command order, independent of which edge the group
/// sits on. A theme may place the group leading or trailing and mirror its
/// visual order, but never change command meaning (spec §10).
const CONTROL_ORDER: [WindowControlKind; 4] = [
    WindowControlKind::PutToBack,
    WindowControlKind::Minimize,
    WindowControlKind::SizeToggle,
    WindowControlKind::Close,
];

/// The number of window-command controls (the length of [`CONTROL_ORDER`]).
const CONTROL_COUNT: u32 = 4;

/// The canonical index of a command in [`CONTROL_ORDER`].
fn control_index(kind: WindowControlKind) -> usize {
    match kind {
        WindowControlKind::PutToBack => 0,
        WindowControlKind::Minimize => 1,
        WindowControlKind::SizeToggle => 2,
        WindowControlKind::Close => 3,
    }
}

/// Which edge of the title bar the window-command group sits on.
///
/// A session/theme policy chooses this; it changes placement and visual order
/// only, never command meaning (spec §10, §11.18).
#[derive(Copy, Clone, Debug, Eq, PartialEq, Default)]
pub enum ControlPlacement {
    /// The command group sits on the logical trailing (right) edge — the
    /// default — with the close control outermost.
    #[default]
    Trailing,
    /// The command group sits on the logical leading (left) edge, mirrored so
    /// the close control stays outermost.
    Leading,
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
}

/// Where a point falls within a title bar.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum TitleHit {
    /// Over one of the window-command controls.
    Control(WindowControlKind),
    /// Over the draggable title region (identity/title/drag area).
    Drag,
}

/// The laid-out rectangles of a title bar's parts, for painting and hit
/// testing over one shared geometry (so they cannot diverge).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TitleBarLayout {
    /// The control rects in the canonical command order, each paired with its
    /// command.
    pub controls: [(WindowControlKind, Rect); 4],
    /// The bounding rect of the whole command group.
    pub group: Rect,
    /// The draggable title/identity region (never overlaps a control).
    pub title: Rect,
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
/// drag region, and the window-command group (spec §11.18).
///
/// It owns the four [`WindowControl`]s and lays them out on the configured
/// edge; the remaining area is the drag/identity region. Pressing it activates
/// the window and, past the drag threshold, begins a cooperative move; a press
/// over a control routes to that control instead and never starts a drag. The
/// title text is untrusted application data, so it is length-bounded, control
/// characters are replaced, and it truncates with an ellipsis before it would
/// overlap the controls.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TitleBar {
    furniture: WindowFurnitureState,
    placement: ControlPlacement,
    controls: [WindowControl; 4],
    app_name: String,
    title: String,
    pointer: Point,
    press_pending: bool,
    dragging: bool,
    press_origin: Point,
}

impl TitleBar {
    /// A title bar for a window in the given furniture state, with the four
    /// commands on the trailing edge.
    #[must_use]
    pub fn new(furniture: WindowFurnitureState) -> Self {
        let controls = [
            WindowControl::new(CONTROL_ORDER[0]),
            WindowControl::new(CONTROL_ORDER[1]),
            WindowControl::new(CONTROL_ORDER[2]),
            WindowControl::new(CONTROL_ORDER[3]),
        ];
        let mut bar = Self {
            furniture,
            placement: ControlPlacement::Trailing,
            controls,
            app_name: String::new(),
            title: String::new(),
            pointer: Point::ORIGIN,
            press_pending: false,
            dragging: false,
            press_origin: Point::ORIGIN,
        };
        bar.apply_furniture();
        bar
    }

    /// Propagate the furniture state to the controls: the size toggle shows
    /// the next action and is disabled on a non-resizable window; the frame
    /// activation lowers idle contrast on an inactive frame.
    fn apply_furniture(&mut self) {
        let active = self.furniture.activation != WindowActivationState::Inactive;
        let next = self.furniture.size_action();
        for control in &mut self.controls {
            control.set_active_frame(active);
            if control.kind() == WindowControlKind::SizeToggle {
                control.set_size_action(next);
                if !self.furniture.resizable {
                    control.set_state(ControlState::disabled());
                }
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

    /// The command-group placement edge.
    #[must_use]
    pub fn placement(&self) -> ControlPlacement {
        self.placement
    }

    /// Set the command-group placement edge.
    pub fn set_placement(&mut self, placement: ControlPlacement) {
        self.placement = placement;
    }

    /// Set the application-identity name (untrusted; sanitised).
    pub fn set_app_name(&mut self, name: &str) {
        self.app_name = sanitize_label(name);
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

    /// A shared reference to the control for `kind`.
    #[must_use]
    pub fn control(&self, kind: WindowControlKind) -> &WindowControl {
        &self.controls[control_index(kind)]
    }

    /// A mutable reference to the control for `kind`, so the window manager can
    /// set its authority/enabled/recovery state.
    pub fn control_mut(&mut self, kind: WindowControlKind) -> &mut WindowControl {
        &mut self.controls[control_index(kind)]
    }

    /// Lay the title bar out within `bounds` for the active theme.
    #[must_use]
    pub fn layout(&self, bounds: Rect, scale: Scale, theme: &Theme) -> TitleBarLayout {
        let metrics = theme.metrics();
        let e = scale.scale_length(metrics.window_control_extent).max(1);
        let g = scale.scale_length(metrics.control_gap);
        let ins = scale.scale_length(metrics.control_inset);
        let group_w = e
            .saturating_mul(CONTROL_COUNT)
            .saturating_add(g.saturating_mul(CONTROL_COUNT.saturating_sub(1)));
        let cy = bounds.top() + (to_i32(bounds.height) - to_i32(e)).max(0) / 2;

        let group_left = match self.placement {
            ControlPlacement::Trailing => bounds.right() - to_i32(ins) - to_i32(group_w),
            ControlPlacement::Leading => bounds.left() + to_i32(ins),
        };

        let mut controls = [
            (CONTROL_ORDER[0], Rect::new(0, 0, e, e)),
            (CONTROL_ORDER[1], Rect::new(0, 0, e, e)),
            (CONTROL_ORDER[2], Rect::new(0, 0, e, e)),
            (CONTROL_ORDER[3], Rect::new(0, 0, e, e)),
        ];
        for (i, slot) in controls.iter_mut().enumerate() {
            let i = u32::try_from(i).unwrap_or(0);
            // Leading mirrors the visual order so the close control stays
            // outermost; trailing keeps the canonical left-to-right order.
            let pos = match self.placement {
                ControlPlacement::Trailing => i,
                ControlPlacement::Leading => (CONTROL_COUNT - 1) - i,
            };
            let x = group_left + to_i32(pos.saturating_mul(e.saturating_add(g)));
            slot.1 = Rect::new(x, cy, e, e);
        }

        let group = Rect::new(group_left.max(bounds.left()), cy, group_w, e);
        let title = match self.placement {
            ControlPlacement::Trailing => Rect::new(
                bounds.left() + to_i32(ins),
                bounds.top(),
                u32::try_from((group_left - to_i32(g) - (bounds.left() + to_i32(ins))).max(0))
                    .unwrap_or(0),
                bounds.height,
            ),
            ControlPlacement::Leading => {
                let start = group_left + to_i32(group_w) + to_i32(g);
                Rect::new(
                    start,
                    bounds.top(),
                    u32::try_from((bounds.right() - to_i32(ins) - start).max(0)).unwrap_or(0),
                    bounds.height,
                )
            }
        };

        TitleBarLayout {
            controls,
            group,
            title,
        }
    }

    /// Paint the title bar into `surface` at `bounds` for the active theme.
    ///
    /// The bar background is the window surface; the identity/title is drawn
    /// truncated within the drag region and the controls are painted in their
    /// laid-out slots.
    pub fn render(
        &self,
        surface: &mut Surface,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
        font: BitmapFont,
    ) {
        let palette = theme.palette();
        if let Some((x, y, w, h)) = surface_rect(bounds) {
            surface.fill_rect(x, y, w, h, Color::from(palette.surface));
        }
        let layout = self.layout(bounds, scale, theme);

        // The title/identity text, truncated so it never overlaps a control.
        let active = self.furniture.activation != WindowActivationState::Inactive;
        let text_color = if active {
            Color::from(palette.on_surface)
        } else {
            Color::from(palette.on_surface_muted)
        };
        if layout.title.width > 0 {
            let glyph_h = font.glyph_height();
            let ty =
                layout.title.top() + (to_i32(layout.title.height) - to_i32(glyph_h)).max(0) / 2;
            let tx = layout.title.left();
            let combined = self.display_text();
            let fitted = font.truncate_to_width(&combined, layout.title.width);
            font.draw_text(surface, tx, ty, fitted, text_color);
        }

        for (kind, rect) in layout.controls {
            self.control(kind).render(surface, rect, scale, theme);
        }
    }

    /// The identity+title string drawn in the drag region.
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
        for (kind, rect) in layout.controls {
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
    ) -> Option<TitleBarEvent> {
        if let InputEvent::PointerMoved { to } = event {
            self.pointer = *to;
        }
        let layout = self.layout(bounds, scale, theme);

        // Route to every control so hover stays current and an armed control
        // keeps its latch even as the pointer moves off it.
        let mut fired = None;
        for (kind, rect) in layout.controls {
            if let Some(action) = self.control_mut(kind).on_pointer(event, rect) {
                let WindowControlAction::Invoked(k) = action;
                fired = Some(k);
            }
        }
        if let Some(kind) = fired {
            return Some(TitleBarEvent::Control(kind));
        }

        let over_control = layout
            .controls
            .iter()
            .any(|(_, r)| r.contains(self.pointer));
        let any_armed = self.controls.iter().any(WindowControl::armed);

        match event {
            InputEvent::PointerPressed {
                button: PointerButton::Primary,
            } => {
                if !over_control && !any_armed && bounds.contains(self.pointer) {
                    self.press_pending = true;
                    self.dragging = false;
                    self.press_origin = self.pointer;
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

    /// Feed a key event. A focused control is activated with Space/Enter; the
    /// left/right arrows move focus between the enabled controls so the group
    /// is fully keyboard-navigable without a pointer (spec §11.18 furniture
    /// keyboard focus).
    pub fn on_key(&mut self, key: Key) -> Option<TitleBarEvent> {
        for control in &mut self.controls {
            if let Some(WindowControlAction::Invoked(kind)) = control.on_key(key) {
                return Some(TitleBarEvent::Control(kind));
            }
        }
        match key {
            Key::Named(NamedKey::Right) => {
                self.move_focus(true);
                None
            }
            Key::Named(NamedKey::Left) => {
                self.move_focus(false);
                None
            }
            _ => None,
        }
    }

    /// Move keyboard focus among the controls one slot `forward` (or backward),
    /// skipping disabled controls and wrapping. If no control is focused, the
    /// first step lands on the first (forward) or last (backward) control.
    fn move_focus(&mut self, forward: bool) {
        let count = self.controls.len();
        let current = self.controls.iter().position(|c| c.state().focus.focused);
        for control in &mut self.controls {
            control.set_focused(false);
        }
        let mut idx = match current {
            Some(i) => i,
            None if forward => count - 1,
            None => 0,
        };
        for _ in 0..count {
            idx = if forward {
                (idx + 1) % count
            } else {
                (idx + count - 1) % count
            };
            if self.controls[idx].state().is_actionable() {
                self.controls[idx].set_focused(true);
                return;
            }
        }
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

/// Where a point falls within a window frame.
///
/// This is the frame's furniture hit map: it classifies every point as either
/// the client viewport or a specific furniture part, so an application-drawn
/// lookalike inside the client area can never receive input meant for the
/// frame, and the client can never receive furniture input (spec §11.17).
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

/// The window-manager-owned boundary around one client viewport (spec §11.17).
///
/// It draws the Frame Rim (stronger and doubled when active, quieter when
/// inactive, with a bounded attention dot on an attention request — never an
/// indefinite pulse), owns the [`TitleBar`], and exposes the client rectangle
/// the compositor clips the application into and the furniture hit map that
/// keeps the client and the furniture strictly separate. Activation, theme,
/// and hover never change the client origin or the outer dimensions: the rim
/// distinction is colour and a doubled inner line, not a change of geometry.
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

    /// The three scaled frame metrics — `(border, title_bar_height,
    /// frame_inset)` in physical pixels — every frame rectangle is built from.
    /// The border is at least one physical pixel and the side inset is never
    /// thinner than the border, so a rim always draws.
    fn edges(scale: Scale, theme: &Theme) -> (u32, u32, u32) {
        let metrics = theme.metrics();
        let border = scale.scale_length(metrics.border_thickness).max(1);
        let inset_amt = scale.scale_length(metrics.frame_inset).max(border);
        let title_h = scale.scale_length(metrics.title_bar_height);
        (border, title_h, inset_amt)
    }

    /// The left/right/bottom furniture-band thickness around the client.
    ///
    /// A **resizable** window reserves a real grab border here — the theme's
    /// resize-grabber extent — so the resize edges are a usable pointer target
    /// and the corner grabber has furniture to live in (it never overlaps the
    /// client). A fixed-size window keeps the thin frame inset, so it is not
    /// widened by a border it can never use. Never thinner than the frame
    /// border, so a rim always draws.
    fn band_inset(&self, scale: Scale, theme: &Theme) -> u32 {
        let (_, _, inset_amt) = Self::edges(scale, theme);
        if self.furniture.resizable {
            scale
                .scale_length(theme.metrics().resize_grabber_extent)
                .max(inset_amt)
        } else {
            inset_amt
        }
    }

    /// The per-edge furniture-band thickness around the client, at the active
    /// scale and theme.
    ///
    /// The top band carries the frame border and the title bar; the other three
    /// carry the resize band (the theme's resize-grabber extent on a resizable
    /// window, the thin frame inset otherwise). This is the one definition
    /// [`Self::layout`] and [`Self::outer_for_client`] share (they never
    /// restate the metric math).
    #[must_use]
    pub fn insets(&self, scale: Scale, theme: &Theme) -> FrameInsets {
        let (border, title_h, _) = Self::edges(scale, theme);
        let band = self.band_inset(scale, theme);
        FrameInsets {
            top: border.saturating_add(title_h),
            left: band,
            right: band,
            bottom: band,
        }
    }

    /// The outer window rectangle whose client viewport is exactly `client`:
    /// `client` grown by the furniture band ([`Self::insets`]) on every edge.
    ///
    /// This is the window manager's inverse of [`Self::layout`] — it sizes a
    /// decorated window's outer bounds from its client-sized content surface —
    /// and it round-trips: `self.layout(self.outer_for_client(client, ..),
    /// ..).client == client`.
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
        let band = self.band_inset(scale, theme);

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
        if layout.client.contains(point) {
            return FurniturePart::Client;
        }
        if !self.furniture.resizable {
            return FurniturePart::Frame;
        }
        let left = point.x < layout.client.left();
        let right = point.x >= layout.client.right();
        let bottom = point.y >= layout.client.bottom();
        match (left, right, bottom) {
            (true, _, true) => FurniturePart::ResizeEdge(ResizeEdge::BottomLeft),
            (_, true, true) => FurniturePart::ResizeEdge(ResizeEdge::BottomRight),
            (true, _, false) => FurniturePart::ResizeEdge(ResizeEdge::Left),
            (_, true, false) => FurniturePart::ResizeEdge(ResizeEdge::Right),
            (false, false, true) => FurniturePart::ResizeEdge(ResizeEdge::Bottom),
            _ => FurniturePart::Frame,
        }
    }

    /// Paint the frame chrome (rim, body background, title bar) into `surface`
    /// at `bounds`. The client viewport is left for the compositor to clip the
    /// application into; the frame never paints client pixels.
    pub fn render(
        &self,
        surface: &mut Surface,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
        font: BitmapFont,
    ) {
        let Some((x, y, w, h)) = surface_rect(bounds) else {
            return;
        };
        if w == 0 || h == 0 {
            return;
        }
        let palette = theme.palette();
        let metrics = theme.metrics();
        let border = scale.scale_length(metrics.border_thickness).max(1);
        let radius = scale.scale_length(metrics.window_corner_radius);
        let active = self.furniture.activation != WindowActivationState::Inactive;

        // Frame Rim: brighter when active/attention, quieter when inactive.
        let rim = if active {
            Color::from(palette.frame_active)
        } else {
            Color::from(palette.frame_inactive)
        };
        surface.fill_round_rect(x, y, w, h, radius, rim);

        // Window body behind the title bar and client viewport.
        if let Some((ix, iy, iw, ih)) = inset(x, y, w, h, border) {
            surface.fill_round_rect(
                ix,
                iy,
                iw,
                ih,
                radius.saturating_sub(border),
                Color::from(palette.surface),
            );
            // The active frame adds a doubled inner rim line — a non-colour
            // focus distinction that does not change frame measurements.
            if active {
                draw_outline(
                    surface,
                    ix,
                    iy,
                    iw,
                    ih,
                    border,
                    Color::from(palette.rim_active),
                );
            }
        }

        let layout = self.layout(bounds, scale, theme);
        self.title_bar
            .render(surface, layout.title_bar, scale, theme, font);

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

/// The explicit corner resize affordance for resizable windows (spec §11.23).
///
/// It draws Grip Teeth — a shape mark that reads without colour — and captures
/// a pointer drag from its hit region. The visible affordance and the hit
/// region are supplied separately by the window manager, so the hit region can
/// extend into the frame without ever overlapping another control or a
/// scrollbar thumb (the caller keeps it clear of them). A non-resizable or
/// maximized window disables the grabber; a disabled grabber ignores input
/// (fail closed). Geometry follows the pointer with no easing, so it is
/// reduced-motion correct.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResizeGrabber {
    state: ControlState,
    active_frame: bool,
    pointer: Point,
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
            pointer: Point::ORIGIN,
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
    /// `bounds` for the active theme. The teeth are authored on a design grid
    /// mapped across `bounds`, so they scale with the affordance the window
    /// manager already sized through the shared [`Scale`].
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
        let Some(mut glyph) = Surface::new(side, side) else {
            return;
        };
        let thick = if heavy_contrast(theme) { 16 } else { 10 };
        // Three diagonal teeth parallel to the bottom-right corner.
        diagonal(&mut glyph, GLYPH_GRID, (55, 95), (95, 55), thick, color);
        diagonal(&mut glyph, GLYPH_GRID, (72, 95), (95, 72), thick, color);
        diagonal(&mut glyph, GLYPH_GRID, (38, 95), (95, 38), thick, color);
        // Place the square glyph at the bottom-right of the affordance.
        let gx = x + w - side;
        let gy = y + h - side;
        surface.blit(to_i32(gx), to_i32(gy), &glyph);
    }

    /// Feed a pointer event, given the grabber's current hit region
    /// `hit_bounds`, returning the resize gesture it produced. A press begins
    /// and captures the drag; motion continues it; release ends it. A disabled
    /// or denied grabber ignores input (fail closed).
    pub fn on_pointer(&mut self, event: &InputEvent, hit_bounds: Rect) -> Option<ResizeEvent> {
        if let InputEvent::PointerMoved { to } = event {
            self.pointer = *to;
        }
        let inside = hit_bounds.contains(self.pointer);
        match event {
            InputEvent::PointerMoved { to } => {
                if self.dragging {
                    Some(ResizeEvent::Moved { to: *to })
                } else {
                    self.state.pointer = if inside {
                        PointerState::Hover
                    } else {
                        PointerState::None
                    };
                    None
                }
            }
            InputEvent::PointerPressed {
                button: PointerButton::Primary,
            } => {
                if inside && self.state.is_actionable() {
                    self.dragging = true;
                    self.state.pointer = PointerState::Pressed;
                    Some(ResizeEvent::Begin)
                } else {
                    None
                }
            }
            InputEvent::PointerReleased {
                button: PointerButton::Primary,
            } => {
                let was = self.dragging;
                self.dragging = false;
                self.state.pointer = if inside {
                    PointerState::Hover
                } else {
                    PointerState::None
                };
                was.then_some(ResizeEvent::End)
            }
            _ => None,
        }
    }

    /// Feed a key event: Escape cancels an in-flight resize (restoring the
    /// pre-drag geometry), so a keyboard escape works exactly like a pointer
    /// cancel.
    pub fn on_key(&mut self, key: Key) -> Option<ResizeEvent> {
        if self.dragging && key == Key::Named(NamedKey::Escape) {
            self.dragging = false;
            self.state.pointer = PointerState::None;
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
