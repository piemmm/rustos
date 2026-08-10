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
use tairix_geometry::{Point, Rect, Region, Scale};
use tairix_icon::{builtin_icon, IconKind};
use tairix_input::{InputEvent, Key};
use tairix_raster::{Color, Surface};
use tairix_theme::{TextRole, Theme};

use crate::paint::{
    key_activation, paint_bead, paint_chevron, paint_icon_slot, paint_plate, plate_border,
    pointer_activation, resolve_bead, resolve_frame, resolve_rail, role_font, surface_rect, to_i32,
    BeadShape, ChevronDir, PlateStyle,
};
use crate::state::{
    ActivityState, ControlDisposition, ControlRole, ControlState, PlateSeating, PointerState,
    RenderInvariant,
};

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

/// Where a [`Button`] seats its content group within its plate.
///
/// A button standing on its own centres its content; one in a stack of
/// commands aligns to the leading edge so the icons and labels of the whole
/// stack line up and the stack reads as a list of commands rather than a
/// column of centred captions.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub enum ContentAlign {
    /// Centred within the plate — the standalone default.
    #[default]
    Center,
    /// Against the plate's leading inset, so sibling buttons align.
    Leading,
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
    /// The plate and rim the button wears, or `None` when its seating leaves it
    /// bare and the surface behind it shows through.
    face: Option<(Color, Color)>,
    label: Color,
    seam: Option<(Color, SeamExtent)>,
    rail: Option<Color>,
    bead: Option<(Color, BeadShape)>,
    focused: bool,
}

/// Resolve the button's colours and edge signals for one theme, state, and
/// seating.
///
/// The plate/rim/label/rail/bead come from the shared control resolvers so
/// every family reads identically; only the button-specific Heat Seam (the
/// activity/progress trace) is resolved here.
fn resolve(
    theme: &Theme,
    role: ControlRole,
    state: ControlState,
    seating: PlateSeating,
) -> Resolved {
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
        face: frame.face(seating),
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

    if let Some((plate, rim)) = res.face {
        paint_plate(
            surface,
            (x, y, w, h),
            &PlateStyle {
                radius,
                border,
                plate,
                rim,
                focused: res.focused,
                ring: Color::from(theme.palette().rim_active),
            },
        );
    }

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

/// The side length of the square glyph an icon-only plate `w`×`h` (with border
/// thickness `border`) should draw: the smaller plate dimension inside its
/// frame, reduced by a small margin proportional to the plate so the glyph
/// fills the button yet stays clear of the frame.
///
/// Unlike a labelled plate, an icon-only button gives its whole face to the
/// glyph, so it is sized off the plate rather than the text inset (which is
/// tuned for a line of type and would shrink the glyph to a few pixels).
pub(crate) fn icon_content_side(w: u32, h: u32, border: u32) -> u32 {
    let plate = w.min(h);
    // An eighth of the plate on each side keeps the glyph clear of the frame
    // (about a 75% fill on the default 28px plate) while scaling with size.
    let margin = border.saturating_add(plate / 8);
    plate.saturating_sub(margin.saturating_mul(2))
}

/// What a plate draws inside itself: the content group and where it sits.
struct ContentGroup<'a> {
    content: &'a ButtonContent,
    align: ContentAlign,
}

impl<'a> ContentGroup<'a> {
    /// A centred group — what a control with no seating choice of its own
    /// draws.
    const fn centred(content: &'a ButtonContent) -> Self {
        Self {
            content,
            align: ContentAlign::Center,
        }
    }
}

/// Paint the content group (icon and/or label) within the plate, seated as
/// the group asks.
fn paint_content(
    surface: &mut Surface,
    rect: (u32, u32, u32, u32),
    scale: Scale,
    theme: &Theme,
    res: &Resolved,
    group: &ContentGroup<'_>,
    font: BitmapFont,
) {
    let (x, y, w, h) = rect;
    let (content, align) = (group.content, group.align);
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
    // Where a content group `total` wide begins, for the requested seating.
    let group_start = |total: u32| match align {
        ContentAlign::Center => cx - to_i32(total) / 2,
        ContentAlign::Leading => to_i32(x) + to_i32(edge),
    };

    match content {
        ButtonContent::Label(text) => {
            let fitted = font.truncate_to_width(text, avail_w);
            let width = font.text_width(fitted);
            font.draw_text(surface, group_start(width), text_y, fitted, res.label);
        }
        ButtonContent::Icon(kind) => {
            // An icon-only button has no label competing for the plate, so the
            // glyph fills the plate rather than shrinking to the text inset.
            // The text inset (`control_inset`) is sized to keep a line of type
            // clear of the frame; applied to an icon-only plate it leaves only
            // a few pixels of glyph adrift in a large button, which reads as
            // broken. The icon instead uses a small margin proportional to the
            // plate, so it fills the button while staying clear of the frame.
            let side = icon_content_side(w, h, border);
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
            let start = group_start(total);
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
/// — activation is a signal to the owning container, which enforces authority.
///
/// # Equality is render equivalence
///
/// Equal buttons draw the same pixels for the same bounds, scale, theme, and
/// font, so a host may use `==` to decide whether a surface holding one needs
/// repainting. The content, role, content seating, and every visible part of
/// the composed [`ControlState`] — hover, press, focus, validation, authority
/// — take part. The last pointer coordinate and the press latch do not: they
/// are hit-testing bookkeeping no render path reads, and the *visible* result
/// of a press is the state's [`PointerState`], which is compared.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Button {
    content: ButtonContent,
    role: ControlRole,
    align: ContentAlign,
    state: ControlState,
    /// The last pointer position, resolved against `bounds` on the next press
    /// or release — hit-testing input, never a drawn property.
    pointer: RenderInvariant<Point>,
    /// Whether a primary press landed on this button and has not yet been
    /// released; the press *look* lives in `state.pointer`.
    armed: RenderInvariant<bool>,
}

impl Button {
    /// A button with the given content and role, in the resting state.
    #[must_use]
    pub fn new(content: ButtonContent, role: ControlRole) -> Self {
        Self {
            content,
            role,
            align: ContentAlign::Center,
            state: ControlState::idle(),
            pointer: RenderInvariant::new(Point::ORIGIN),
            armed: RenderInvariant::new(false),
        }
    }

    /// A neutral labelled button — the common case.
    #[must_use]
    pub fn labelled(label: impl Into<String>) -> Self {
        Self::new(ButtonContent::Label(label.into()), ControlRole::Neutral)
    }

    /// The same button with its content group seated as given.
    ///
    /// A container stacking buttons into a list of commands asks for
    /// [`ContentAlign::Leading`] so their icons and labels line up down the
    /// stack; a standalone button keeps the centred default.
    #[must_use]
    pub fn aligned(mut self, align: ContentAlign) -> Self {
        self.align = align;
        self
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

    /// Where the button seats its content group.
    #[must_use]
    pub fn align(&self) -> ContentAlign {
        self.align
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

    /// Set whether the button belongs to the highlighted Focus Field — the
    /// group of related controls around whichever one holds keyboard focus.
    ///
    /// Orthogonal to [`set_focused`](Self::set_focused): the sibling actions
    /// of a focused row are members without holding the ring themselves.
    pub fn set_in_focus_field(&mut self, member: bool) {
        self.state.focus.in_focus_field = member;
    }

    /// Paint the button into `surface` at `bounds` for the active theme.
    pub fn render(&self, surface: &mut Surface, bounds: Rect, scale: Scale, theme: &Theme) {
        let font = role_font(theme, scale, TextRole::Body);
        let res = resolve(theme, self.role, self.state, PlateSeating::Panel);
        paint_frame(surface, bounds, scale, theme, &res);
        if let Some(rect) = surface_rect(bounds) {
            paint_content(
                surface,
                rect,
                scale,
                theme,
                &res,
                &ContentGroup {
                    content: &self.content,
                    align: self.align,
                },
                font,
            );
        }
    }

    /// Feed a pointer event, given the button's current `bounds`, updating its
    /// pointer state and returning [`ButtonAction::Activated`] on a completed
    /// primary click. The button reports `bounds` into `damage` when the event
    /// changed how it draws.
    pub fn on_pointer(
        &mut self,
        event: &InputEvent,
        bounds: Rect,
        damage: &mut Region,
    ) -> Option<ButtonAction> {
        if let InputEvent::PointerMoved { to } = event {
            *self.pointer = *to;
        }
        let inside = bounds.contains(*self.pointer);
        if pointer_activation(
            &mut self.state,
            &mut self.armed,
            event,
            inside,
            bounds,
            damage,
        ) {
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
/// content differs (an [`IconKind`] rather than a label) and it carries a
/// [`PlateSeating`], because an icon button is the one control that appears
/// both on a panel and in the taskbar's icon strip ([`Self::seated`]).
///
/// Its equality is the render-equivalence relation [`Button`] documents: the
/// icon, role, seating, and visible state are compared; the pointer coordinate
/// and press latch behind it are not.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IconButton {
    icon: IconKind,
    role: ControlRole,
    seating: PlateSeating,
    state: ControlState,
    /// The last pointer position — hit-testing input, never drawn.
    pointer: RenderInvariant<Point>,
    /// The press latch; the press *look* lives in `state.pointer`.
    armed: RenderInvariant<bool>,
}

impl IconButton {
    /// An icon button with the given glyph and role, in the resting state,
    /// seated on a panel.
    #[must_use]
    pub fn new(icon: IconKind, role: ControlRole) -> Self {
        Self {
            icon,
            role,
            seating: PlateSeating::Panel,
            state: ControlState::idle(),
            pointer: RenderInvariant::new(Point::ORIGIN),
            armed: RenderInvariant::new(false),
        }
    }

    /// The same button seated as given.
    ///
    /// The icon button is the one family that appears on both kinds of surface
    /// — a window toolbar and the taskbar's icon strip — so it is the one that
    /// carries the choice. It changes only how the plate is worn
    /// ([`PlateSeating`]); the state model, hit testing, and every signal the
    /// button reports are identical either way.
    #[must_use]
    pub fn seated(mut self, seating: PlateSeating) -> Self {
        self.seating = seating;
        self
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

    /// Where the button is seated.
    #[must_use]
    pub fn seating(&self) -> PlateSeating {
        self.seating
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

    /// The pixel side the button's glyph paints at inside `bounds`.
    ///
    /// This is the render geometry itself, exposed so an owner rasterising
    /// per-button artwork produces it at exactly the size [`Self::render`]
    /// will place — the two can never disagree. An icon-only plate sizes the
    /// glyph off the plate (never the text inset), so it fills the button;
    /// `0` when the bounds are off-surface or too small for a glyph.
    #[must_use]
    pub fn icon_side(&self, bounds: Rect, scale: Scale, theme: &Theme) -> u32 {
        let Some((_, _, w, h)) = surface_rect(bounds) else {
            return 0;
        };
        icon_content_side(w, h, plate_border(theme, scale))
    }

    /// Paint the icon button into `surface` at `bounds` for the active theme.
    ///
    /// `artwork` is the owner's own icon, pre-rasterised at [`Self::icon_side`]
    /// (through its cache); `None` falls back to the button's built-in class
    /// glyph. The artwork is decoded and rasterised long before it reaches
    /// this call — a control never parses image bytes. Both go through the one
    /// shared "blit centred artwork, else rasterise the glyph" slot the other
    /// collection controls draw their icon with.
    pub fn render(
        &self,
        surface: &mut Surface,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
        artwork: Option<&Surface>,
    ) {
        let res = resolve(theme, self.role, self.state, self.seating);
        paint_frame(surface, bounds, scale, theme, &res);
        if let Some((x, y, w, h)) = surface_rect(bounds) {
            let side = icon_content_side(w, h, plate_border(theme, scale));
            if side > 0 {
                let ix = x + (w.saturating_sub(side)) / 2;
                let iy = y + (h.saturating_sub(side)) / 2;
                paint_icon_slot(surface, ix, iy, side, self.icon, res.label, artwork);
            }
        }
    }

    /// Feed a pointer event; see [`Button::on_pointer`].
    pub fn on_pointer(
        &mut self,
        event: &InputEvent,
        bounds: Rect,
        damage: &mut Region,
    ) -> Option<ButtonAction> {
        if let InputEvent::PointerMoved { to } = event {
            *self.pointer = *to;
        }
        let inside = bounds.contains(*self.pointer);
        pointer_activation(
            &mut self.state,
            &mut self.armed,
            event,
            inside,
            bounds,
            damage,
        )
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

/// A primary action region plus a disclosure region sharing one plate
/// (spec §11.3).
///
/// The two regions expose *separate* focus and pointer states over one shared
/// Signal Rim; the Heat Seam and Signal Bead belong to the primary action (its
/// job). Activation reports which region fired via [`SplitAction`].
///
/// Its equality is the render-equivalence relation [`Button`] documents: the
/// content, role, and both regions' visible states are compared; the shared
/// pointer coordinate and the two press latches behind them are not.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SplitButton {
    content: ButtonContent,
    role: ControlRole,
    primary: ControlState,
    disclosure: ControlState,
    /// The last pointer position, resolved against whichever region it fell
    /// in — hit-testing input, never drawn.
    pointer: RenderInvariant<Point>,
    /// The primary region's press latch; its press *look* lives in
    /// `primary.pointer`.
    primary_armed: RenderInvariant<bool>,
    /// The disclosure region's press latch; its press *look* lives in
    /// `disclosure.pointer`.
    disclosure_armed: RenderInvariant<bool>,
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
            pointer: RenderInvariant::new(Point::ORIGIN),
            primary_armed: RenderInvariant::new(false),
            disclosure_armed: RenderInvariant::new(false),
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
    pub fn render(&self, surface: &mut Surface, bounds: Rect, scale: Scale, theme: &Theme) {
        let font = role_font(theme, scale, TextRole::Body);
        let res = resolve(
            theme,
            self.role,
            combined_state(self.primary, self.disclosure),
            PlateSeating::Panel,
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
            paint_content(
                surface,
                rect,
                scale,
                theme,
                &res,
                &ContentGroup::centred(&self.content),
                font,
            );
        }
        paint_chevron(surface, disclosure_rect, ChevronDir::Down, res.label);
    }

    /// Feed a pointer event, given the button's current `bounds`, returning the
    /// [`SplitAction`] of whichever region completed a primary click.
    ///
    /// Either region reports the whole `bounds`: the two share one plate whose
    /// frame resolves from both region states, so a change in one repaints the
    /// pair.
    pub fn on_pointer(
        &mut self,
        event: &InputEvent,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
        damage: &mut Region,
    ) -> Option<SplitAction> {
        if let InputEvent::PointerMoved { to } = event {
            *self.pointer = *to;
        }
        let (primary_rect, disclosure_rect) = split_regions(bounds, scale, theme);
        let in_primary = primary_rect.contains(*self.pointer);
        let in_disclosure = disclosure_rect.contains(*self.pointer);
        let primary_fired = pointer_activation(
            &mut self.primary,
            &mut self.primary_armed,
            event,
            in_primary,
            bounds,
            damage,
        );
        let disclosure_fired = pointer_activation(
            &mut self.disclosure,
            &mut self.disclosure_armed,
            event,
            in_disclosure,
            bounds,
            damage,
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
