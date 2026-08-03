//! Shared drawing helpers for the Reactive Alloy control renderers.
//!
//! The button family and the boolean-selector family (and every later drawn
//! family) share the same low-level plate geometry: converting a logical
//! [`Rect`] to a surface rectangle, insetting by a border, resolving the
//! scaled plate border thickness, and asking the theme whether the
//! heavier-contrast treatment applies. Those helpers live here once rather
//! than being copied into each family's module, so the whole control set
//! rounds, insets, and thickens identically and a change to the recipe cannot
//! silently diverge between two controls.
//!
//! It is also the home of [`RenderInvariant`], the one marker that keeps a
//! control's derived equality honest about what it draws.

use core::ops::{Deref, DerefMut};

use tairix_font::BitmapFont;
use tairix_geometry::{Rect, Scale};
use tairix_icon::{builtin_icon, IconKind};
use tairix_input::{InputEvent, Key, NamedKey, PointerButton};
use tairix_raster::{Color, Surface};
use tairix_theme::{Contrast, Palette, Rgba, SignalRole, Theme};

use crate::state::{
    ActivityState, ControlDisposition, ControlRole, ControlState, PointerState, PressureKind,
    PressureState, RecoveryState, ValidationState,
};

/// A value the renderer never reads, wrapped so it cannot make two controls
/// that draw the same pixels compare unequal.
///
/// # The contract it buys
///
/// Every drawn control in this crate derives `PartialEq`, and a host uses that
/// equality as a *render gate*: it re-renders and re-presents only when the
/// composition it is about to draw differs from the one it last drew. That is
/// sound only if equality means "these two would draw the same pixels", so a
/// control may hold no field that changes without changing the picture. Raw
/// pointer coordinates, press latches, and drag anchors are exactly such
/// fields: they are hit-testing bookkeeping consumed by `on_pointer`, and the
/// *visible* consequence of a press or a hover is a separate, still-compared
/// `ControlState`. Left bare, one pointer sample over inert background would
/// make the whole composition compare as changed and pay a full repaint.
///
/// Wrapping such a field in `RenderInvariant` makes it compare equal to every
/// other value of its type, so it drops out of the surrounding `derive` while
/// every other field keeps its ordinary meaning.
///
/// # Why a wrapper rather than a hand-written `PartialEq`
///
/// The alternative — writing `impl PartialEq` per control and simply omitting
/// the excluded field — has to restate every *remaining* field. A field added
/// later is then silently absent from equality, and a visible change stops
/// forcing a repaint: stale pixels, the failure direction that a test is
/// unlikely to catch. Localising the exception in the *type of the excluded
/// field* inverts that: the struct keeps `#[derive(PartialEq)]`, a new field is
/// covered automatically, and exempting one takes a deliberate, greppable,
/// self-documenting change at the field itself.
///
/// # The obligation on the author
///
/// Wrap a field only with positive evidence that no render path reads it, and
/// prove it with a drift-guard test that renders two values differing only in
/// that field and compares the surfaces byte for byte. The cost of the two
/// mistakes is not symmetric: a field wrongly left bare only costs a needless
/// repaint, while one wrongly wrapped freezes the screen on stale pixels.
///
/// It deliberately implements no `Hash`: equality here is coarser than the
/// wrapped value, so any hash derived from that value would break the
/// `Hash`/`Eq` agreement.
#[derive(Copy, Clone, Debug, Default)]
pub(crate) struct RenderInvariant<T>(T);

impl<T> RenderInvariant<T> {
    /// Mark `value` as state the renderer never reads.
    pub(crate) const fn new(value: T) -> Self {
        Self(value)
    }
}

/// Two wrapped values are always equal — that is the whole point of the
/// wrapper, and the reason the surrounding control's derived equality means
/// "would draw the same pixels".
impl<T> PartialEq for RenderInvariant<T> {
    fn eq(&self, _other: &Self) -> bool {
        true
    }
}

impl<T> Eq for RenderInvariant<T> {}

impl<T> Deref for RenderInvariant<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.0
    }
}

impl<T> DerefMut for RenderInvariant<T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.0
    }
}

/// Map a resource pressure to its theme signal role, in one place so no
/// renderer restates the mapping.
#[must_use]
pub(crate) const fn pressure_role(kind: PressureKind) -> SignalRole {
    match kind {
        PressureKind::Cpu => SignalRole::Cpu,
        PressureKind::Memory => SignalRole::Memory,
        PressureKind::Disk => SignalRole::Disk,
        PressureKind::Network => SignalRole::Network,
        PressureKind::Power => SignalRole::Power,
        PressureKind::Thermal => SignalRole::Thermal,
    }
}

/// The theme's signal colour for a resource `kind` — the Pressure Rail hue
/// every family shares, whether the rail is conditional (a row or card shows
/// it only while genuinely under pressure, see [`resolve_rail`]) or a
/// control's own fixed identity (a resource meter, which always reads as
/// that resource regardless of how loaded it is).
#[must_use]
pub(crate) fn signal_color(theme: &Theme, kind: PressureKind) -> Color {
    Color::from(theme.palette().signal(pressure_role(kind)))
}

/// The full-scale value of a measured control, in permille.
pub(crate) const FULL: u16 = 1000;

/// Clamp a permille value into `0..=1000` (fail closed on an out-of-range
/// request).
#[must_use]
pub(crate) const fn clamp_permille(v: u16) -> u16 {
    if v > FULL {
        FULL
    } else {
        v
    }
}

/// Whether the theme asks for the heavier-contrast treatment (thicker rim,
/// stronger marks) — high-contrast or monochrome-safe.
#[must_use]
pub(crate) fn heavy_contrast(theme: &Theme) -> bool {
    !matches!(theme.contrast(), Contrast::Normal)
}

/// Clamp a rectangle's origin into non-negative surface coordinates, returning
/// the `(x, y, w, h)` in surface pixels, or `None` if it lies fully off the
/// top-left. A control is laid out within a client surface, so its origin is
/// expected to be non-negative; anything off-surface simply does not paint.
#[must_use]
pub(crate) fn surface_rect(bounds: Rect) -> Option<(u32, u32, u32, u32)> {
    let x = u32::try_from(bounds.left()).ok()?;
    let y = u32::try_from(bounds.top()).ok()?;
    Some((x, y, bounds.width, bounds.height))
}

/// Inset a surface rectangle by `by` on every side, or `None` if it collapses.
#[must_use]
pub(crate) fn inset(x: u32, y: u32, w: u32, h: u32, by: u32) -> Option<(u32, u32, u32, u32)> {
    let iw = w.checked_sub(by.saturating_mul(2))?;
    let ih = h.checked_sub(by.saturating_mul(2))?;
    if iw == 0 || ih == 0 {
        return None;
    }
    Some((x + by, y + by, iw, ih))
}

/// The scaled plate border/rim thickness, doubled under heavy contrast so a
/// high-contrast theme strengthens the rim before adding any glow.
#[must_use]
pub(crate) fn plate_border(theme: &Theme, scale: Scale) -> u32 {
    scale
        .scale_length(theme.metrics().border_thickness)
        .max(1)
        .saturating_mul(if heavy_contrast(theme) { 2 } else { 1 })
}

/// The physical breadth of the *measured* track a user drives — a slider's
/// groove — from the theme metric, never thinner than a hairline.
///
/// A measured track is an instrument line rather than a plate, so the slider
/// resolves it here instead of deriving a thickness from its row height.
#[must_use]
pub(crate) fn measured_thickness(theme: &Theme, scale: Scale) -> u32 {
    track_thickness(theme, scale, theme.metrics().measured_thickness)
}

/// The physical breadth of a progress trace's bar from the theme metric.
///
/// A progress bar is read rather than dragged, so the theme gives it a little
/// more breadth than a slider's groove; it stays an instrument line resolved
/// from theme data, never from the row it sits in.
#[must_use]
pub(crate) fn progress_thickness(theme: &Theme, scale: Scale) -> u32 {
    track_thickness(theme, scale, theme.metrics().progress_thickness)
}

/// One logical track breadth in physical pixels: at least a hairline, and one
/// pixel broader under heavy contrast so the line stays visible.
#[must_use]
fn track_thickness(theme: &Theme, scale: Scale, logical: u32) -> u32 {
    scale
        .scale_length(logical)
        .max(1)
        .saturating_add(u32::from(heavy_contrast(theme)))
}

/// A `u32` extent as an `i32` coordinate, saturating rather than wrapping.
#[must_use]
pub(crate) fn to_i32(v: u32) -> i32 {
    i32::try_from(v).unwrap_or(i32::MAX)
}

/// Paint a content icon at `(x, y)` into a `side`-pixel square slot: the
/// owner-supplied `artwork` when present, else the built-in class glyph for
/// `kind` tinted `tint`.
///
/// This is the one place every collection control — a taskbar item, a card, a
/// list row — applies the "blit centred artwork, else rasterise the built-in
/// glyph" rule, so the three can never draw it differently. The artwork is
/// blitted centred in the slot: a stale cache entry from mid-scale-change, or
/// a surface sized differently from `side`, is placed in the middle rather
/// than overflowing the slot from its corner. The built-in glyph rasterises to
/// fill the slot exactly. Artwork is decoded and rasterised by the owner long
/// before it reaches here — a control never parses image bytes.
pub(crate) fn paint_icon_slot(
    surface: &mut Surface,
    x: u32,
    y: u32,
    side: u32,
    kind: IconKind,
    tint: Color,
    artwork: Option<&Surface>,
) {
    if let Some(art) = artwork {
        let ax = to_i32(x) + (to_i32(side) - to_i32(art.width())) / 2;
        let ay = to_i32(y) + (to_i32(side) - to_i32(art.height())) / 2;
        surface.blit(ax, ay, art);
    } else if let Some(image) = builtin_icon(kind, tint).rasterise(side) {
        surface.blit(to_i32(x), to_i32(y), &image);
    }
}

/// Update `state`/`armed` from one pointer event and return whether the
/// control was activated (a primary press-and-release over it).
///
/// The press captures a latch on primary-button down over an actionable
/// control; releasing over it activates, releasing away cancels — the
/// standard press model shared by every clickable control (button, toggle,
/// checkbox, radio). `inside` is whether the pointer is over the control's
/// bounds (the caller's hit-test).
pub(crate) fn pointer_activation(
    state: &mut ControlState,
    armed: &mut bool,
    event: &InputEvent,
    inside: bool,
) -> bool {
    match event {
        InputEvent::PointerMoved { .. } => {
            if !*armed {
                state.pointer = if inside {
                    PointerState::Hover
                } else {
                    PointerState::None
                };
            }
            false
        }
        InputEvent::PointerPressed {
            button: PointerButton::Primary,
        } => {
            if inside && state.is_actionable() {
                *armed = true;
                state.pointer = PointerState::Pressed;
            }
            false
        }
        InputEvent::PointerReleased {
            button: PointerButton::Primary,
        } => {
            let activated = *armed && inside && state.is_actionable();
            *armed = false;
            state.pointer = if inside {
                PointerState::Hover
            } else {
                PointerState::None
            };
            activated
        }
        _ => false,
    }
}

/// Whether a key activates a focused, actionable control (Space or Enter).
#[must_use]
pub(crate) fn key_activation(state: ControlState, key: Key) -> bool {
    state.focus.focused
        && state.is_actionable()
        && matches!(key, Key::Char(' ') | Key::Named(NamedKey::Enter))
}

/// The non-colour shape a Signal Bead draws, so an alert is legible without
/// relying on hue.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) enum BeadShape {
    /// A completion check mark.
    Check,
    /// A recovery diamond.
    Diamond,
    /// An authority lock (a small keyhole square).
    Lock,
}

/// The plate, rim, and label colours a control frame paints, resolved from
/// theme and state.
///
/// This is the one place the control set maps its typed [`ControlState`] and
/// [`ControlRole`] to the plate/rim/label colours, so every family — button,
/// toggle, checkbox, radio — reads the same way and an authority denial never
/// collapses into a plain disabled look.
pub(crate) struct FrameColors {
    /// The inner Alloy Plate fill.
    pub plate: Color,
    /// The Signal Rim perimeter.
    pub rim: Color,
    /// The label / foreground colour.
    pub label: Color,
    /// Whether the control draws its focus ring.
    pub focused: bool,
}

/// How strongly a control's role is stated on its surface.
///
/// The design boards give a control exactly three treatments, and the
/// difference between them is *where* the role colour lands: nowhere, on the
/// edge and the label, or across the whole plate. Naming the three makes the
/// recipe one decision instead of a per-family colour choice.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum Emphasis {
    /// No role colour: a neutral plate with a quiet rim and a plain label.
    Quiet,
    /// The role colour on the rim and the label, over the resting plate.
    Outlined(Rgba),
    /// The role colour across the plate, with the rim the same colour and a
    /// contrasting label.
    Filled(Rgba),
}

/// The emphasis a role carries on an interactive control.
///
/// The main action of a surface is filled, the action the model recommends and
/// a destructive action are outlined in their own colour (so a hard-to-undo
/// action reads as coloured intent without shouting like the primary), and
/// everything else stays quiet.
#[must_use]
fn role_emphasis(palette: &Palette, role: ControlRole) -> Emphasis {
    match role {
        ControlRole::Primary => Emphasis::Filled(palette.accent),
        ControlRole::Recovery => Emphasis::Filled(palette.recovery),
        ControlRole::Recommended => Emphasis::Outlined(palette.accent),
        ControlRole::Destructive => Emphasis::Outlined(palette.danger),
        ControlRole::Neutral | ControlRole::Navigation | ControlRole::System => Emphasis::Quiet,
    }
}

/// How far a filled plate is darkened while pressed, in permille.
const PRESS_DARKEN: u16 = 220;
/// How far a filled plate is lightened while hovered, in permille.
const HOVER_LIGHTEN: u16 = 90;
/// How far a Focus Field member's rim is carried toward the active rim, in
/// permille — a partial lift, so a member reads as *related to* the focused
/// control without competing with the control that actually holds the ring.
const FIELD_LIFT: u16 = 550;

/// Black and white, the two ends a filled role colour is mixed toward to
/// derive its pressed and hovered neighbours.
const BLACK: Rgba = Rgba::rgb(0, 0, 0);
const WHITE: Rgba = Rgba::rgb(255, 255, 255);

/// A filled role colour under one pointer state: darker while pressed,
/// brighter while hovered, the plain role colour at rest.
#[must_use]
fn filled_plate(color: Rgba, pointer: PointerState) -> Rgba {
    match pointer {
        PointerState::Pressed => color.mix(BLACK, PRESS_DARKEN),
        PointerState::Hover => color.mix(WHITE, HOVER_LIGHTEN),
        PointerState::None | PointerState::DragSource | PointerState::DragTarget => color,
    }
}

/// The rim an ordinary interactive Focus Field *member* draws: the resting
/// rim carried part-way toward the active rim, so a set of related controls
/// reads as one group while the member that actually holds keyboard focus
/// keeps the only ring.
///
/// Only the caller's interactive dispositions reach here; a disabled, denied,
/// failed-closed, or pending control keeps the rim its disposition gave it.
///
/// A filled plate is left alone. Its rim is its plate colour by construction,
/// and tinting one without the other would put a foreign edge on a coloured
/// control — the invariant [`resolve_frame`] documents. A filled member states
/// its membership through the group's other members instead.
///
/// Under a heavier-contrast theme the lift goes all the way to the active rim:
/// contrast comes before glow, so the field must survive a palette a partial
/// blend would wash out.
#[must_use]
fn field_rim(theme: &Theme, plate: Rgba, rim: Rgba) -> Rgba {
    if rim == plate {
        return rim;
    }
    let active = theme.palette().rim_active;
    if heavy_contrast(theme) {
        active
    } else {
        rim.mix(active, FIELD_LIFT)
    }
}

/// Resolve the shared plate/rim/label colours for one theme, role, and state.
///
/// The rim carries the spec §13 disposition: a disabled control shows a quiet
/// border, a denial the denied role, a failed-closed attempt the recovery
/// role, a pending check the active rim; only an interactive control takes its
/// role's emphasis.
///
/// Two invariants come from the design boards and hold for every family. A
/// coloured plate always has its rim in the *same* colour, so a filled control
/// never shows a foreign edge; and a control states its role on the edge and
/// the label before it states it on the plate — pressing a quiet or outlined
/// control colours it rather than merely darkening it, which is what makes a
/// click visible without motion.
///
/// An ordinary interactive control that belongs to a highlighted Focus Field
/// but does not itself hold focus takes the lifted [`field_rim`]: the design
/// language draws either a focus ring *or* a Focus Field, never both on one
/// control, so membership is stated on the edge and the ring stays the
/// property of the one control the keyboard is actually on. A disposition
/// that owns the rim outranks membership entirely — see below.
#[must_use]
pub(crate) fn resolve_frame(theme: &Theme, role: ControlRole, state: ControlState) -> FrameColors {
    let palette = theme.palette();
    let disposition = state.disposition();
    let pointer = state.pointer;

    let emphasis = match disposition {
        ControlDisposition::DisabledByState => Emphasis::Quiet,
        ControlDisposition::DeniedByAuthority => Emphasis::Outlined(palette.denied),
        ControlDisposition::FailedClosed => Emphasis::Outlined(palette.recovery),
        ControlDisposition::PendingCheck => Emphasis::Outlined(palette.rim_active),
        ControlDisposition::Interactive | ControlDisposition::NeedsConfirmation => {
            role_emphasis(palette, role)
        }
    };

    let (plate, rim, label) = match emphasis {
        Emphasis::Filled(color) => {
            let fill = filled_plate(color, pointer);
            (fill, fill, palette.on_accent)
        }
        // A press promotes an outlined control to a filled one: the colour it
        // was stating on its edge takes the plate, edge included.
        Emphasis::Outlined(color) if pointer == PointerState::Pressed => {
            let fill = filled_plate(color, pointer);
            (fill, fill, palette.on_accent)
        }
        Emphasis::Outlined(color) => (palette.surface_raised, color, color),
        Emphasis::Quiet if disposition == ControlDisposition::DisabledByState => {
            (palette.surface, palette.border, palette.on_surface_muted)
        }
        // A quiet control has no colour of its own, so a press borrows the
        // active rim for both its edge and its label.
        Emphasis::Quiet if pointer == PointerState::Pressed => (
            palette.surface_pressed,
            palette.rim_active,
            palette.rim_active,
        ),
        Emphasis::Quiet if pointer == PointerState::Hover || state.focus.focused => (
            palette.surface_raised,
            palette.rim_active,
            palette.on_surface,
        ),
        Emphasis::Quiet => (palette.surface_raised, palette.rim, palette.on_surface),
    };

    // A disposition that owns the rim outranks the Focus Field. A disabled,
    // denied, failed-closed, or pending control is stating something the user
    // needs far more than which group it belongs to, and lifting its edge
    // toward the active rim would both soften that statement and make a
    // control that cannot be actioned look livelier than a resting one that
    // can. Membership is cosmetic; those four are not.
    let interactive = matches!(
        disposition,
        ControlDisposition::Interactive | ControlDisposition::NeedsConfirmation
    );
    let rim = if state.focus.in_focus_field && !state.focus.focused && interactive {
        field_rim(theme, plate, rim)
    } else {
        rim
    };

    FrameColors {
        plate: Color::from(plate),
        rim: Color::from(rim),
        label: Color::from(label),
        focused: state.focus.focused,
    }
}

/// The accent colour a control fills its *value mark* with — a selector's
/// check/bead/toggle contact, or a slider's value track and thumb accent.
///
/// It carries the spec §13 disposition exactly like the rim does: a disabled
/// control mutes it, a denial takes the denied role, a failed-closed attempt
/// the recovery role, and an interactive control takes its role's accent
/// (destructive danger, recovery, otherwise the theme accent). Sharing this
/// with the selector family keeps the mark recipe defined once, so a
/// selector's tick and a slider's track can never diverge.
#[must_use]
pub(crate) fn resolve_mark(theme: &Theme, role: ControlRole, state: ControlState) -> Color {
    let palette = theme.palette();
    let rgba = match state.disposition() {
        ControlDisposition::DisabledByState => palette.on_surface_muted,
        ControlDisposition::DeniedByAuthority => palette.denied,
        ControlDisposition::FailedClosed => palette.recovery,
        _ => match role {
            ControlRole::Destructive => palette.danger,
            ControlRole::Recovery => palette.recovery,
            _ => palette.accent,
        },
    };
    Color::from(rgba)
}

/// The Pressure Rail colour a control shows, if it is under a resource
/// pressure — one mapping shared by every family.
#[must_use]
pub(crate) fn resolve_rail(theme: &Theme, state: ControlState) -> Option<Color> {
    match state.pressure {
        PressureState::Under(kind) => Some(signal_color(theme, kind)),
        PressureState::None => None,
    }
}

/// The Signal Bead colour and shape a control shows, if any — one priority
/// shared by every family: authority mark first, then recovery, then
/// completion.
#[must_use]
pub(crate) fn resolve_bead(theme: &Theme, state: ControlState) -> Option<(Color, BeadShape)> {
    let palette = theme.palette();
    let bead = match state.disposition() {
        ControlDisposition::DeniedByAuthority => (palette.denied, BeadShape::Lock),
        ControlDisposition::FailedClosed => (palette.recovery, BeadShape::Diamond),
        _ => match state.recovery {
            RecoveryState::None => match state.activity {
                ActivityState::Complete => (palette.success, BeadShape::Check),
                _ => return None,
            },
            _ => (palette.recovery, BeadShape::Diamond),
        },
    };
    Some((Color::from(bead.0), bead.1))
}

/// The colours and geometry of one Alloy Plate, grouped so the shared
/// plate-drawing routine takes a single style rather than a long argument
/// list.
pub(crate) struct PlateStyle {
    /// Outer corner radius (physical px).
    pub radius: u32,
    /// Rim/border thickness the inner plate is inset by (physical px).
    pub border: u32,
    /// Inner Alloy Plate fill.
    pub plate: Color,
    /// Signal Rim perimeter.
    pub rim: Color,
    /// Whether to draw the double-rim focus ring.
    pub focused: bool,
    /// The focus-ring colour.
    pub ring: Color,
}

/// Paint an Alloy Plate: the Signal Rim as a rounded rect, the inner plate
/// inset by the border, and — when focused — a double-rim focus ring, so a
/// focused control is distinct from a hovered one without relying on colour.
///
/// This is the one plate-drawing definition every rounded control frame uses,
/// so the rim, inner plate, and focus ring can never diverge between families.
pub(crate) fn paint_plate(surface: &mut Surface, rect: (u32, u32, u32, u32), style: &PlateStyle) {
    let (x, y, w, h) = rect;
    if w == 0 || h == 0 {
        return;
    }
    surface.fill_round_rect(x, y, w, h, style.radius, style.rim);
    let Some((ix, iy, iw, ih)) = inset(x, y, w, h, style.border) else {
        return;
    };
    let inner_radius = style.radius.saturating_sub(style.border);
    surface.fill_round_rect(ix, iy, iw, ih, inner_radius, style.plate);

    if style.focused {
        let gap = style.border;
        if let Some((fx, fy, fw, fh)) = inset(ix, iy, iw, ih, gap) {
            surface.fill_round_rect(fx, fy, fw, fh, inner_radius.saturating_sub(gap), style.ring);
            if let Some((px, py, pw, ph)) = inset(fx, fy, fw, fh, style.border) {
                surface.fill_round_rect(
                    px,
                    py,
                    pw,
                    ph,
                    inner_radius.saturating_sub(gap + style.border),
                    style.plate,
                );
            }
        }
    }
}

/// A directional disclosure/anchor/step chevron.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) enum ChevronDir {
    /// Points up (a vertical scrollbar's decrement button).
    Up,
    /// Points down (a disclosure that expands below, e.g. a split button or a
    /// combo box; a vertical scrollbar's increment button).
    Down,
    /// Points toward the logical start (a horizontal scrollbar's decrement
    /// button).
    Left,
    /// Points right (a submenu anchor; a horizontal scrollbar's increment
    /// button).
    Right,
}

/// Draw a filled chevron of the given direction centred in `rect`.
///
/// One definition shared by the split button's disclosure, the combo box's
/// disclosure, a menu's submenu anchor, and a scrollbar's end-button steps, so
/// no family carries its own triangle recipe.
pub(crate) fn paint_chevron(surface: &mut Surface, rect: Rect, dir: ChevronDir, color: Color) {
    let Some((x, y, w, h)) = surface_rect(rect) else {
        return;
    };
    if w == 0 || h == 0 {
        return;
    }
    let Some(mut glyph) = Surface::new(w, h) else {
        return;
    };
    // Triangles authored on a 100×100 grid mapped across the region, so they
    // scale with the region at any density.
    let points: [(i32, i32); 3] = match dir {
        ChevronDir::Up => [(32, 58), (68, 58), (50, 36)],
        ChevronDir::Down => [(32, 42), (68, 42), (50, 64)],
        ChevronDir::Left => [(58, 32), (58, 68), (36, 50)],
        ChevronDir::Right => [(40, 32), (40, 68), (64, 50)],
    };
    glyph.fill_polygon(&points, 100, color);
    surface.blit(to_i32(x), to_i32(y), &glyph);
}

/// Draw a hollow rectangular outline of `thickness` inside `(x, y, w, h)`.
///
/// The one focus-ring / cell-outline primitive shared by the row/tab families
/// (a keyboard-focused row or tab draws this ring to read distinctly from a
/// pointer hover, spec §15).
pub(crate) fn draw_outline(
    surface: &mut Surface,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    thickness: u32,
    color: Color,
) {
    if w == 0 || h == 0 || thickness == 0 {
        return;
    }
    let edge = thickness.min(w).min(h);
    surface.fill_rect(x, y, w, edge, color);
    surface.fill_rect(x, y + h - edge, w, edge, color);
    surface.fill_rect(x, y, edge, h, color);
    surface.fill_rect(x + w - edge, y, edge, h, color);
}

/// The scaled thickness of a leading rail (selection or resource pressure),
/// doubled under heavy contrast so the rail strengthens before any tint.
///
/// One definition shared by the collection controls and the shell surfaces
/// (a card's leading dominant rail, a notification's warning rail, a tray
/// signal's pressure rail) so the rail breadth cannot diverge between them.
#[must_use]
pub(crate) fn rail_thickness(theme: &Theme, scale: Scale) -> u32 {
    scale
        .scale_length(theme.metrics().rail_thickness)
        .max(1)
        .saturating_mul(if heavy_contrast(theme) { 2 } else { 1 })
}

/// The scaled thickness of a Heat Seam (an activity/progress trace on an
/// edge), shared by every family that draws one so the seam breadth is one
/// value.
#[must_use]
pub(crate) fn seam_thickness(theme: &Theme, scale: Scale) -> u32 {
    scale.scale_length(theme.metrics().seam_thickness).max(1)
}

/// Paint an Edge Wake down the leading edge of `bounds`: a lit seam on the
/// side of an anchored control that displaced content sits against.
///
/// The design language uses the wake to answer a question the user would
/// otherwise have to answer by eye — *did this column move, or did the list
/// move under it?* The control never moves, so the confirmation has to come
/// from its edge, and the edge that carries it is the one facing the content;
/// the caller passes the anchored control's own bounds.
///
/// It is a state, not an animation: the seam is lit for exactly as long as
/// the adjacent content is displaced, so a reduced-motion theme needs no
/// separate path (there is nothing to animate) and a still frame carries the
/// same information as a moving one. It is drawn in the active rim colour, at
/// the shared seam breadth, doubled under heavy contrast so the wake
/// strengthens with the rest of the theme's edges rather than relying on a
/// glow a high-contrast palette would flatten.
pub(crate) fn paint_edge_wake(surface: &mut Surface, bounds: Rect, scale: Scale, theme: &Theme) {
    let Some((x, y, w, h)) = surface_rect(bounds) else {
        return;
    };
    let thickness = seam_thickness(theme, scale)
        .saturating_mul(if heavy_contrast(theme) { 2 } else { 1 })
        .min(w);
    if thickness == 0 || h == 0 {
        return;
    }
    surface.fill_rect(x, y, thickness, h, Color::from(theme.palette().rim_active));
}

/// The width a Heat Seam of the given `activity` covers across `w` pixels: a
/// known fraction fills proportionally, working/indeterminate fills fully, and
/// anything else draws nothing (fail-closed, no guessed extent).
#[must_use]
pub(crate) fn seam_width(activity: ActivityState, w: u32) -> u32 {
    match activity {
        ActivityState::Progress(value) => {
            u32::try_from(u64::from(w) * u64::from(value.permille()) / 1000).unwrap_or(w)
        }
        ActivityState::Working | ActivityState::Indeterminate => w,
        _ => 0,
    }
}

/// The foreground colour for a surface's body text: muted when the disposition
/// is disabled, the normal on-surface foreground otherwise. A denied surface
/// keeps full-contrast text and shows its Authority Mark instead of dimming.
#[must_use]
pub(crate) fn foreground(theme: &Theme, disposition: ControlDisposition) -> Color {
    let palette = theme.palette();
    Color::from(if disposition == ControlDisposition::DisabledByState {
        palette.on_surface_muted
    } else {
        palette.on_surface
    })
}

/// The colour a grouped surface's dominant edge uses for its overall state:
/// a resource-pressure rail wins, then an authority/recovery/failed state,
/// then a validation warning, then the control role's emphasis, falling back
/// to the quiet rim for a plain neutral surface.
///
/// One definition shared by the card's leading rail, the panel's header, and
/// the shell surfaces (a notification's semantic rail, a taskbar item's / tray
/// signal's dominant state) so the priority order cannot diverge between them.
#[must_use]
pub(crate) fn dominant_color(theme: &Theme, role: ControlRole, state: ControlState) -> Color {
    if let Some(color) = resolve_rail(theme, state) {
        return color;
    }
    let palette = theme.palette();
    let rgba = match state.disposition() {
        ControlDisposition::DeniedByAuthority => palette.denied,
        ControlDisposition::FailedClosed => palette.recovery,
        _ if state.recovery != RecoveryState::None => palette.recovery,
        _ if state.validation == ValidationState::Warning => palette.warning,
        _ => match role {
            ControlRole::Destructive => palette.danger,
            ControlRole::Recovery => palette.recovery,
            ControlRole::Primary | ControlRole::Recommended => palette.accent,
            _ => palette.rim,
        },
    };
    Color::from(rgba)
}

/// Paint one filled circle of `diameter` at `(x, y)`.
///
/// This is the one circle-fill primitive the desktop shares: a Signal
/// Bead's completion mark and a secret [`TextField`](crate::TextField)'s
/// masking beads round through here rather than each hand-rolling its own
/// circle, over the same [`Surface::fill_round_rect`] every other rounded
/// fill in this crate already uses.
pub(crate) fn paint_filled_circle(
    surface: &mut Surface,
    x: u32,
    y: u32,
    diameter: u32,
    color: Color,
) {
    surface.fill_round_rect(x, y, diameter, diameter, diameter / 2, color);
}

/// Draw one Signal Bead of `size` at `(bx, by)` in the given shape, so the
/// alert role reads by shape as well as colour.
pub(crate) fn paint_bead(
    surface: &mut Surface,
    bx: u32,
    by: u32,
    size: u32,
    color: Color,
    shape: BeadShape,
) {
    match shape {
        BeadShape::Check => paint_filled_circle(surface, bx, by, size, color),
        BeadShape::Lock => surface.fill_round_rect(bx, by, size, size, size / 4, color),
        BeadShape::Diamond => {
            if let Some(mut glyph) = Surface::new(size, size) {
                let s = to_i32(size);
                let points = [(s / 2, 0), (s, s / 2), (s / 2, s), (0, s / 2)];
                glyph.fill_polygon(&points, size, color);
                surface.blit(to_i32(bx), to_i32(by), &glyph);
            }
        }
    }
}

/// Paint a filled count/alert badge in `rect` (`(x, y, w, h)`, like
/// [`paint_plate`]) — a circle when `text` fits within the height, else a
/// pill — with `text` centred inside it.
///
/// One definition shared by a [`Card`](crate::collection::Card)'s grouped
/// top-trailing count/alert badge and a
/// [`TraySignal`](crate::shell::TraySignal)'s live-state badge, so the badge
/// recipe cannot diverge between the two families. The caller resolves the
/// badge's rectangle and colours from its own available space and state;
/// this paints exactly the resolved geometry and draws nothing for a
/// degenerate (zero-sized) rectangle.
pub(crate) fn paint_count_badge(
    surface: &mut Surface,
    rect: (u32, u32, u32, u32),
    fill: Color,
    text_color: Color,
    font: BitmapFont,
    text: &str,
) {
    let (x, y, w, h) = rect;
    if w == 0 || h == 0 {
        return;
    }
    surface.fill_round_rect(x, y, w, h, h / 2, fill);
    let tw = font.text_width(text);
    let tx = to_i32(x) + (to_i32(w) - to_i32(tw)).max(0) / 2;
    let ty = to_i32(y) + (to_i32(h) - to_i32(font.glyph_height())).max(0) / 2;
    font.draw_text(surface, tx, ty, text, text_color);
}
