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

use tairix_geometry::{Rect, Scale};
use tairix_input::{InputEvent, Key, NamedKey, PointerButton};
use tairix_raster::{Color, Surface};
use tairix_theme::{Contrast, SignalRole, Theme};

use crate::state::{
    ActivityState, ControlDisposition, ControlRole, ControlState, PointerState, PressureKind,
    PressureState, RecoveryState,
};

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

/// A `u32` extent as an `i32` coordinate, saturating rather than wrapping.
#[must_use]
pub(crate) fn to_i32(v: u32) -> i32 {
    i32::try_from(v).unwrap_or(i32::MAX)
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

/// Resolve the shared plate/rim/label colours for one theme, role, and state.
///
/// The rim carries the spec §13 disposition: a disabled control shows a quiet
/// border, a denial the denied role, a failed-closed attempt the recovery
/// role, a pending check the active rim; only an interactive control takes its
/// role's emphasis (destructive danger, recovery, primary accent) or lifts to
/// the active rim on hover/press/focus.
#[must_use]
pub(crate) fn resolve_frame(theme: &Theme, role: ControlRole, state: ControlState) -> FrameColors {
    let palette = theme.palette();
    let disposition = state.disposition();

    let plate = match disposition {
        ControlDisposition::DisabledByState => palette.surface,
        _ if state.pointer == PointerState::Pressed => palette.surface_pressed,
        _ => palette.surface_raised,
    };

    let rim = match disposition {
        ControlDisposition::DisabledByState => palette.border,
        ControlDisposition::DeniedByAuthority => palette.denied,
        ControlDisposition::FailedClosed => palette.recovery,
        ControlDisposition::PendingCheck => palette.rim_active,
        ControlDisposition::Interactive | ControlDisposition::NeedsConfirmation => match role {
            ControlRole::Destructive => palette.danger,
            ControlRole::Recovery => palette.recovery,
            ControlRole::Primary | ControlRole::Recommended => palette.accent,
            _ if state.pointer == PointerState::Hover
                || state.pointer == PointerState::Pressed
                || state.focus.focused =>
            {
                palette.rim_active
            }
            _ => palette.rim,
        },
    };

    let label = if disposition == ControlDisposition::DisabledByState {
        palette.on_surface_muted
    } else {
        palette.on_surface
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
/// with the selector family keeps the mark recipe defined once (`AGENTS.md`
/// §2.2), so a selector's tick and a slider's track can never diverge.
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
        PressureState::Under(kind) => {
            Some(Color::from(theme.palette().signal(pressure_role(kind))))
        }
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
        BeadShape::Check => surface.fill_round_rect(bx, by, size, size, size / 2, color),
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
