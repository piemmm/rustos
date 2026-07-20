//! The button family: [`Button`], [`IconButton`], and [`SplitButton`].
//!
//! All three are Alloy Plates with a Signal Rim and the optional edge signals
//! (Heat Seam, Pressure Rail, Signal Bead, focus ring) the design language
//! gives a button. They share one visual core (`resolve` + `paint_frame`) and
//! one interaction core (`pointer_activation`/`key_activation`) rather
//! than three copies, so the whole family rounds, tints, and responds
//! identically. Every visible property resolves from the active
//! [`Theme`] and [`Scale`]; the
//! control holds no hard-coded colour, radius, or timing.

use alloc::string::String;

use tairix_font::BitmapFont;
use tairix_geometry::{Point, Rect, Scale};
use tairix_icon::{builtin_icon, IconKind};
use tairix_input::{InputEvent, Key};
use tairix_raster::{Color, Surface};
use tairix_theme::Theme;

use crate::paint::{
    key_activation, paint_bead, paint_plate, plate_border, pointer_activation, resolve_bead,
    resolve_frame, resolve_rail, surface_rect, to_i32, BeadShape, PlateStyle,
};
use crate::state::{ActivityState, ControlDisposition, ControlRole, ControlState, PointerState};

/// What a button displays: a label, an icon, or an icon with a label.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ButtonContent {
    /// A text label.
    Label(String),
    /// A single themed icon glyph.
    Icon(IconKind),
    /// A leading icon and a trailing label.
    IconLabel {
        /// The leading glyph.
        icon: IconKind,
        /// The label after it.
        label: String,
    },
}

/// The outcome of feeding input to a [`Button`] or [`IconButton`].
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ButtonAction {
    /// The button was activated (released over it, or Space/Enter while
    /// focused) and its action should be dispatched.
    Activated,
}

/// The region of a [`SplitButton`] an action came from.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum SplitAction {
    /// The primary action region was activated.
    Primary,
    /// The disclosure region was activated.
    Disclosure,
}

/// How much of the lower edge a Heat Seam covers.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum SeamExtent {
    /// The full width (working / indeterminate / pending).
    Full,
    /// A known fraction in permille (measured progress).
    Fraction(u16),
}

/// The colours and edge signals a button paints, resolved from theme and state.
///
/// This is the one place the button family maps its typed [`ControlState`] and
/// [`ControlRole`] to concrete theme colours, so every button — labelled,
/// icon, or split — reads the same way and an authority denial never collapses
/// into a plain disabled look.
struct Resolved {
    plate: Color,
    rim: Color,
    label: Color,
    seam: Option<(Color, SeamExtent)>,
    rail: Option<Color>,
    bead: Option<(Color, BeadShape)>,
    focused: bool,
}

/// Resolve the button's colours and edge signals for one theme and state.
///
/// The plate/rim/label/rail/bead come from the shared control resolvers so
/// every family reads identically; only the button-specific Heat Seam (the
/// activity/progress trace) is resolved here.
fn resolve(theme: &Theme, role: ControlRole, state: ControlState) -> Resolved {
    let palette = theme.palette();
    let disposition = state.disposition();
    let frame = resolve_frame(theme, role, state);

    let seam = match state.activity {
        ActivityState::Working | ActivityState::Indeterminate => {
            Some((palette.accent, SeamExtent::Full))
        }
        ActivityState::Progress(v) => Some((palette.accent, SeamExtent::Fraction(v.permille()))),
        ActivityState::Complete | ActivityState::Idle => {
            if disposition == ControlDisposition::PendingCheck {
                Some((palette.rim_active, SeamExtent::Full))
            } else {
                None
            }
        }
    };

    Resolved {
        plate: frame.plate,
        rim: frame.rim,
        label: frame.label,
        seam: seam.map(|(c, e)| (Color::from(c), e)),
        rail: resolve_rail(theme, state),
        bead: resolve_bead(theme, state),
        focused: frame.focused,
    }
}

/// Paint one button plate frame — Alloy Plate, Signal Rim, focus ring, and the
/// edge signals (Pressure Rail, Heat Seam, Signal Bead) — into `surface` at
/// `bounds`, without content.
///
/// Content is drawn separately by [`paint_content`], so a [`SplitButton`] can
/// paint one shared frame and then place its primary content and disclosure
/// mark in their own regions. Geometry is derived from the theme metrics
/// through the shared [`Scale`]; nothing is hard-coded.
fn paint_frame(surface: &mut Surface, bounds: Rect, scale: Scale, theme: &Theme, res: &Resolved) {
    let Some((x, y, w, h)) = surface_rect(bounds) else {
        return;
    };
    if w == 0 || h == 0 {
        return;
    }
    let metrics = theme.metrics();
    let radius = scale.scale_length(metrics.control_corner_radius);
    let border = plate_border(theme, scale);

    let ring = Color::from(theme.palette().rim_active);
    paint_plate(
        surface,
        (x, y, w, h),
        &PlateStyle {
            radius,
            border,
            plate: res.plate,
            rim: res.rim,
            focused: res.focused,
            ring,
        },
    );

    paint_signals(surface, (x, y, w, h), scale, theme, res);
}

/// Paint the Pressure Rail, Heat Seam, and Signal Bead inside the plate.
fn paint_signals(
    surface: &mut Surface,
    rect: (u32, u32, u32, u32),
    scale: Scale,
    theme: &Theme,
    res: &Resolved,
) {
    let (x, y, w, h) = rect;
    let border = plate_border(theme, scale);
    let metrics = theme.metrics();
    let inner_x = x + border;
    let inner_y = y + border;
    let inner_w = w.saturating_sub(border.saturating_mul(2));
    let inner_h = h.saturating_sub(border.saturating_mul(2));
    if inner_w == 0 || inner_h == 0 {
        return;
    }

    if let Some(color) = res.rail {
        let rail_w = scale
            .scale_length(metrics.rail_thickness)
            .max(1)
            .min(inner_w);
        surface.fill_rect(inner_x, inner_y, rail_w, inner_h, color);
    }

    if let Some((color, extent)) = res.seam {
        let seam_h = scale
            .scale_length(metrics.seam_thickness)
            .max(1)
            .min(inner_h);
        let seam_y = y + h - border - seam_h;
        let seam_w = match extent {
            SeamExtent::Full => inner_w,
            SeamExtent::Fraction(permille) => {
                let scaled = u64::from(inner_w) * u64::from(permille) / 1000;
                u32::try_from(scaled).unwrap_or(inner_w)
            }
        };
        if seam_w > 0 {
            surface.fill_rect(inner_x, seam_y, seam_w, seam_h, color);
        }
    }

    if let Some((color, shape)) = res.bead {
        let size = scale
            .scale_length(metrics.bead_size)
            .max(3)
            .min(inner_w)
            .min(inner_h);
        let bx = x + w - border - size;
        let by = y + border;
        paint_bead(surface, bx, by, size, color, shape);
    }
}

/// Paint the content group (icon and/or label) centred within the plate.
fn paint_content(
    surface: &mut Surface,
    rect: (u32, u32, u32, u32),
    scale: Scale,
    theme: &Theme,
    res: &Resolved,
    content: &ButtonContent,
    font: BitmapFont,
) {
    let (x, y, w, h) = rect;
    let border = plate_border(theme, scale);
    let pad = scale.scale_length(theme.metrics().control_inset);
    let edge = border.saturating_add(pad);
    let avail_w = w.saturating_sub(edge.saturating_mul(2));
    let avail_h = h.saturating_sub(edge.saturating_mul(2));
    if avail_w == 0 || avail_h == 0 {
        return;
    }
    let cx = to_i32(x) + to_i32(w) / 2;
    let glyph_h = font.glyph_height();
    let text_y = to_i32(y) + (to_i32(h) - to_i32(glyph_h)).max(0) / 2;

    match content {
        ButtonContent::Label(text) => {
            let fitted = font.truncate_to_width(text, avail_w);
            let width = font.text_width(fitted);
            font.draw_text(surface, cx - to_i32(width) / 2, text_y, fitted, res.label);
        }
        ButtonContent::Icon(kind) => {
            let side = glyph_h.min(avail_w).min(avail_h);
            if side > 0 {
                if let Some(image) = builtin_icon(*kind, res.label).rasterise(side) {
                    let ix = cx - to_i32(side) / 2;
                    let iy = to_i32(y) + (to_i32(h) - to_i32(side)).max(0) / 2;
                    surface.blit(ix, iy, &image);
                }
            }
        }
        ButtonContent::IconLabel { icon, label } => {
            let side = glyph_h.min(avail_h);
            let gap = scale.scale_length(theme.metrics().control_gap);
            let label_budget = avail_w.saturating_sub(side.saturating_add(gap));
            let fitted = font.truncate_to_width(label, label_budget);
            let width = font.text_width(fitted);
            let total = side.saturating_add(gap).saturating_add(width);
            let start = cx - to_i32(total) / 2;
            if side > 0 {
                if let Some(image) = builtin_icon(*icon, res.label).rasterise(side) {
                    let iy = to_i32(y) + (to_i32(h) - to_i32(side)).max(0) / 2;
                    surface.blit(start, iy, &image);
                }
            }
            let text_x = start + to_i32(side.saturating_add(gap));
            font.draw_text(surface, text_x, text_y, fitted, res.label);
        }
    }
}

/// A labelled or icon-labelled action plate (spec §11.1).
///
/// A `Button` owns its typed [`ControlState`] and its [`ControlRole`]; it
/// renders itself into a [`Surface`] and consumes pointer/keyboard input,
/// emitting a [`ButtonAction`] when activated. It performs no privileged work
/// — activation is a signal to the owning container, which enforces authority
/// (`AGENTS.md` §5.4).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Button {
    content: ButtonContent,
    role: ControlRole,
    state: ControlState,
    pointer: Point,
    armed: bool,
}

impl Button {
    /// A button with the given content and role, in the resting state.
    #[must_use]
    pub fn new(content: ButtonContent, role: ControlRole) -> Self {
        Self {
            content,
            role,
            state: ControlState::idle(),
            pointer: Point::ORIGIN,
            armed: false,
        }
    }

    /// A neutral labelled button — the common case.
    #[must_use]
    pub fn labelled(label: impl Into<String>) -> Self {
        Self::new(ButtonContent::Label(label.into()), ControlRole::Neutral)
    }

    /// The button's content.
    #[must_use]
    pub fn content(&self) -> &ButtonContent {
        &self.content
    }

    /// The button's role.
    #[must_use]
    pub fn role(&self) -> ControlRole {
        self.role
    }

    /// The button's current composed state.
    #[must_use]
    pub fn state(&self) -> ControlState {
        self.state
    }

    /// Replace the button's composed state (e.g. from a model update).
    pub fn set_state(&mut self, state: ControlState) {
        self.state = state;
    }

    /// Set the button's keyboard focus.
    pub fn set_focused(&mut self, focused: bool) {
        self.state.focus.focused = focused;
    }

    /// Paint the button into `surface` at `bounds` for the active theme.
    pub fn render(
        &self,
        surface: &mut Surface,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
        font: BitmapFont,
    ) {
        let res = resolve(theme, self.role, self.state);
        paint_frame(surface, bounds, scale, theme, &res);
        if let Some(rect) = surface_rect(bounds) {
            paint_content(surface, rect, scale, theme, &res, &self.content, font);
        }
    }

    /// Feed a pointer event, given the button's current `bounds`, updating its
    /// pointer state and returning [`ButtonAction::Activated`] on a completed
    /// primary click.
    pub fn on_pointer(&mut self, event: &InputEvent, bounds: Rect) -> Option<ButtonAction> {
        if let InputEvent::PointerMoved { to } = event {
            self.pointer = *to;
        }
        let inside = bounds.contains(self.pointer);
        if pointer_activation(&mut self.state, &mut self.armed, event, inside) {
            Some(ButtonAction::Activated)
        } else {
            None
        }
    }

    /// Feed a key event, returning [`ButtonAction::Activated`] when a focused,
    /// actionable button is activated with Space or Enter.
    pub fn on_key(&mut self, key: Key) -> Option<ButtonAction> {
        key_activation(self.state, key).then_some(ButtonAction::Activated)
    }
}

/// An action plate whose content is a single themed icon glyph (spec §11.2).
///
/// It shares the button state model, rendering, and interaction; only its
/// content differs (an [`IconKind`] rather than a label).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IconButton {
    icon: IconKind,
    role: ControlRole,
    state: ControlState,
    pointer: Point,
    armed: bool,
}

impl IconButton {
    /// An icon button with the given glyph and role, in the resting state.
    #[must_use]
    pub fn new(icon: IconKind, role: ControlRole) -> Self {
        Self {
            icon,
            role,
            state: ControlState::idle(),
            pointer: Point::ORIGIN,
            armed: false,
        }
    }

    /// The button's icon.
    #[must_use]
    pub fn icon(&self) -> IconKind {
        self.icon
    }

    /// The button's role.
    #[must_use]
    pub fn role(&self) -> ControlRole {
        self.role
    }

    /// The button's current composed state.
    #[must_use]
    pub fn state(&self) -> ControlState {
        self.state
    }

    /// Replace the button's composed state.
    pub fn set_state(&mut self, state: ControlState) {
        self.state = state;
    }

    /// Set the button's keyboard focus.
    pub fn set_focused(&mut self, focused: bool) {
        self.state.focus.focused = focused;
    }

    /// Paint the icon button into `surface` at `bounds` for the active theme.
    pub fn render(
        &self,
        surface: &mut Surface,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
        font: BitmapFont,
    ) {
        let res = resolve(theme, self.role, self.state);
        paint_frame(surface, bounds, scale, theme, &res);
        if let Some(rect) = surface_rect(bounds) {
            let content = ButtonContent::Icon(self.icon);
            paint_content(surface, rect, scale, theme, &res, &content, font);
        }
    }

    /// Feed a pointer event; see [`Button::on_pointer`].
    pub fn on_pointer(&mut self, event: &InputEvent, bounds: Rect) -> Option<ButtonAction> {
        if let InputEvent::PointerMoved { to } = event {
            self.pointer = *to;
        }
        let inside = bounds.contains(self.pointer);
        pointer_activation(&mut self.state, &mut self.armed, event, inside)
            .then_some(ButtonAction::Activated)
    }

    /// Feed a key event; see [`Button::on_key`].
    pub fn on_key(&mut self, key: Key) -> Option<ButtonAction> {
        key_activation(self.state, key).then_some(ButtonAction::Activated)
    }
}

/// The strongest of two pointer relationships, so a shared plate reads as
/// pressed if either region is pressed and hovered if either is hovered.
fn strongest_pointer(a: PointerState, b: PointerState) -> PointerState {
    match (a, b) {
        (PointerState::Pressed, _) | (_, PointerState::Pressed) => PointerState::Pressed,
        (PointerState::Hover, _) | (_, PointerState::Hover) => PointerState::Hover,
        _ => PointerState::None,
    }
}

/// The combined state that drives a split button's shared plate frame: the
/// liveliest pointer and focus of the two regions, over the primary region's
/// activity/pressure/recovery (the job the primary action owns).
fn combined_state(primary: ControlState, disclosure: ControlState) -> ControlState {
    let mut s = primary;
    s.enabled = primary.enabled || disclosure.enabled;
    s.pointer = strongest_pointer(primary.pointer, disclosure.pointer);
    s.focus.focused = primary.focus.focused || disclosure.focus.focused;
    s.focus.in_focus_field = primary.focus.in_focus_field || disclosure.focus.in_focus_field;
    s
}

/// The primary and disclosure sub-rectangles of a split button's `bounds`. The
/// disclosure region is a square at the trailing edge, at most half the width.
fn split_regions(bounds: Rect, scale: Scale, theme: &Theme) -> (Rect, Rect) {
    let disclosure_w = scale
        .scale_length(theme.metrics().control_height)
        .min(bounds.width / 2);
    let primary_w = bounds.width.saturating_sub(disclosure_w);
    let primary = Rect::new(bounds.left(), bounds.top(), primary_w, bounds.height);
    let disclosure = Rect::new(
        bounds.left().saturating_add(to_i32(primary_w)),
        bounds.top(),
        disclosure_w,
        bounds.height,
    );
    (primary, disclosure)
}

/// Draw a downward disclosure chevron centred in `rect`.
fn paint_chevron(surface: &mut Surface, rect: Rect, color: Color) {
    let Some((x, y, w, h)) = surface_rect(rect) else {
        return;
    };
    if w == 0 || h == 0 {
        return;
    }
    if let Some(mut glyph) = Surface::new(w, h) {
        // A downward triangle authored on a 100×100 grid mapped across the
        // region, so it scales with the region at any density.
        let points = [(32, 42), (68, 42), (50, 64)];
        glyph.fill_polygon(&points, 100, color);
        surface.blit(to_i32(x), to_i32(y), &glyph);
    }
}

/// A primary action region plus a disclosure region sharing one plate
/// (spec §11.3).
///
/// The two regions expose *separate* focus and pointer states over one shared
/// Signal Rim; the Heat Seam and Signal Bead belong to the primary action (its
/// job). Activation reports which region fired via [`SplitAction`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SplitButton {
    content: ButtonContent,
    role: ControlRole,
    primary: ControlState,
    disclosure: ControlState,
    pointer: Point,
    primary_armed: bool,
    disclosure_armed: bool,
}

impl SplitButton {
    /// A split button with the given primary content and role, in the resting
    /// state.
    #[must_use]
    pub fn new(content: ButtonContent, role: ControlRole) -> Self {
        Self {
            content,
            role,
            primary: ControlState::idle(),
            disclosure: ControlState::idle(),
            pointer: Point::ORIGIN,
            primary_armed: false,
            disclosure_armed: false,
        }
    }

    /// The primary region's composed state.
    #[must_use]
    pub fn primary_state(&self) -> ControlState {
        self.primary
    }

    /// The disclosure region's composed state.
    #[must_use]
    pub fn disclosure_state(&self) -> ControlState {
        self.disclosure
    }

    /// Replace the primary region's composed state.
    pub fn set_primary_state(&mut self, state: ControlState) {
        self.primary = state;
    }

    /// Replace the disclosure region's composed state.
    pub fn set_disclosure_state(&mut self, state: ControlState) {
        self.disclosure = state;
    }

    /// The button's role.
    #[must_use]
    pub fn role(&self) -> ControlRole {
        self.role
    }

    /// Paint the split button into `surface` at `bounds` for the active theme.
    pub fn render(
        &self,
        surface: &mut Surface,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
        font: BitmapFont,
    ) {
        let res = resolve(
            theme,
            self.role,
            combined_state(self.primary, self.disclosure),
        );
        paint_frame(surface, bounds, scale, theme, &res);
        let (primary_rect, disclosure_rect) = split_regions(bounds, scale, theme);

        // Divider between the regions, in the border role.
        let border = plate_border(theme, scale);
        if let Some((dx, dy, _, dh)) = surface_rect(disclosure_rect) {
            let top = dy + border;
            let height = dh.saturating_sub(border.saturating_mul(2));
            if height > 0 {
                let divider = Color::from(theme.palette().border);
                surface.fill_rect(dx, top, border.max(1), height, divider);
            }
        }

        if let Some(rect) = surface_rect(primary_rect) {
            paint_content(surface, rect, scale, theme, &res, &self.content, font);
        }
        paint_chevron(surface, disclosure_rect, res.label);
    }

    /// Feed a pointer event, given the button's current `bounds`, returning the
    /// [`SplitAction`] of whichever region completed a primary click.
    pub fn on_pointer(
        &mut self,
        event: &InputEvent,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
    ) -> Option<SplitAction> {
        if let InputEvent::PointerMoved { to } = event {
            self.pointer = *to;
        }
        let (primary_rect, disclosure_rect) = split_regions(bounds, scale, theme);
        let in_primary = primary_rect.contains(self.pointer);
        let in_disclosure = disclosure_rect.contains(self.pointer);
        let primary_fired = pointer_activation(
            &mut self.primary,
            &mut self.primary_armed,
            event,
            in_primary,
        );
        let disclosure_fired = pointer_activation(
            &mut self.disclosure,
            &mut self.disclosure_armed,
            event,
            in_disclosure,
        );
        if primary_fired {
            Some(SplitAction::Primary)
        } else if disclosure_fired {
            Some(SplitAction::Disclosure)
        } else {
            None
        }
    }

    /// Feed a key event: Space/Enter activates whichever region holds focus.
    pub fn on_key(&mut self, key: Key) -> Option<SplitAction> {
        if key_activation(self.primary, key) {
            Some(SplitAction::Primary)
        } else if key_activation(self.disclosure, key) {
            Some(SplitAction::Disclosure)
        } else {
            None
        }
    }
}
