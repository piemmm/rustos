//! The value-control family: [`Slider`] and [`Progress`] (spec §11.6–§11.7).
//!
//! Both are *measured* controls whose value is a validated fraction in permille
//! (`0..=1000`). A [`Slider`] is interactive — its thumb runs along a rail, its
//! value track fills from the start to the thumb, drag and keyboard update the
//! visual value immediately while the change commits through the owning model
//! (`AGENTS.md` §5.4) — while [`Progress`] is a read-only instrument trace of
//! known, working, indeterminate, complete, or failed work. Both resolve every
//! colour/metric/radius from the active [`Theme`] and [`Scale`] and round their
//! plates through the shared drawing core the button and selector families use,
//! so nothing here restates a recipe those families already own.

use alloc::format;
use alloc::string::String;

use tairix_font::BitmapFont;
use tairix_geometry::{Point, Rect, Scale};
use tairix_input::{InputEvent, Key, NamedKey, PointerButton};
use tairix_raster::{Color, Surface};
use tairix_theme::Theme;

use crate::paint::{
    inset, paint_bead, paint_plate, plate_border, resolve_bead, resolve_frame, resolve_mark,
    resolve_rail, surface_rect, to_i32, PlateStyle,
};
use crate::state::{
    ActivityState, ControlDisposition, ControlRole, ControlState, PointerState, RecoveryState,
};

/// The full-scale value of a measured control, in permille.
const FULL: u16 = 1000;

/// Clamp a permille value into `0..=1000` (fail closed on an out-of-range
/// request, `AGENTS.md` §2.9).
#[must_use]
const fn clamp_permille(v: u16) -> u16 {
    if v > FULL {
        FULL
    } else {
        v
    }
}

/// The outcome of interacting with a [`Slider`].
///
/// A slider updates its own displayed value immediately so a drag reads
/// smoothly, but the authoritative change still commits through the owning
/// model (`AGENTS.md` §5.4): the owner receives the requested value and applies
/// it (calling [`Slider::set_value`] to confirm, or a different value to
/// reject/clamp it).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum SliderAction {
    /// The slider requests its value become `permille` (`0..=1000`).
    SetValue {
        /// The requested new value, in permille.
        permille: u16,
    },
}

/// The resolved horizontal geometry of a slider within its bounds.
struct SliderLayout {
    /// The surface x of the thumb-centre travel origin (value `0`).
    track_x0: u32,
    /// The travel span in pixels the thumb centre moves across (value
    /// `0..=1000` maps onto `0..=travel`).
    travel: u32,
    /// The thumb diameter (also its plate side), in pixels.
    thumb_d: u32,
    /// The groove band's top y.
    groove_y: u32,
    /// The groove band's height.
    groove_h: u32,
    /// The whole control's surface-x origin.
    x: u32,
    /// The whole control's surface-y origin.
    y: u32,
    /// The whole control's width.
    w: u32,
    /// The whole control's height.
    h: u32,
}

impl SliderLayout {
    /// The thumb-centre x for a permille value.
    fn centre_for(&self, permille: u16) -> u32 {
        let v = u64::from(clamp_permille(permille));
        let along = u64::from(self.travel) * v / u64::from(FULL);
        self.track_x0 + u32::try_from(along).unwrap_or(self.travel)
    }

    /// The permille value a pointer at surface-x `px` implies, clamped.
    fn value_for(&self, px: i32) -> u16 {
        if self.travel == 0 {
            return 0;
        }
        let clamped = px.clamp(to_i32(self.track_x0), to_i32(self.track_x0 + self.travel));
        let along = u64::from(
            u32::try_from(clamped)
                .unwrap_or(0)
                .saturating_sub(self.track_x0),
        );
        let permille = along * u64::from(FULL) / u64::from(self.travel);
        clamp_permille(u16::try_from(permille).unwrap_or(FULL))
    }
}

/// Resolve a slider's horizontal geometry, or `None` if the control collapses.
///
/// The thumb is one row tall (a grabbable knob); the track runs between the
/// thumb's extreme centres so the thumb never overhangs the control edge, and
/// the groove is a thin band centred vertically. Every extent is proportional
/// to the bounds, so the slider scales with density with no hard-coded pixel.
fn slider_layout(bounds: Rect) -> Option<SliderLayout> {
    let (x, y, w, h) = surface_rect(bounds)?;
    if w == 0 || h == 0 {
        return None;
    }
    let thumb_d = h.min(w).max(1);
    let radius = thumb_d / 2;
    let travel = w.saturating_sub(thumb_d);
    let track_x0 = x + radius;
    let groove_h = (h / 4).max(2).min(h);
    let groove_y = y + (h.saturating_sub(groove_h)) / 2;
    Some(SliderLayout {
        track_x0,
        travel,
        thumb_d,
        groove_y,
        groove_h,
        x,
        y,
        w,
        h,
    })
}

/// A measured value control: a rail, a value track that fills to the thumb, a
/// draggable thumb, and an optional bounded-cap marker (spec §11.6).
///
/// The active range uses the theme accent, or the semantic pressure colour for
/// a resource slider (a slider under a [`PressureState`](crate::PressureState)).
/// A denied slider keeps its value and shows an Authority Mark rather than
/// looking merely disabled (spec §13); a bounded slider shows a cap marker at
/// the constrained edge and cannot be dragged past it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Slider {
    role: ControlRole,
    state: ControlState,
    value: u16,
    line_step: u16,
    page_step: u16,
    cap: Option<u16>,
    pointer: Point,
    dragging: bool,
}

impl Slider {
    /// A neutral slider at `value` permille, with a 1% line step and a 10%
    /// page step (both settable).
    #[must_use]
    pub fn new(value: u16) -> Self {
        Self {
            role: ControlRole::Neutral,
            state: ControlState::idle(),
            value: clamp_permille(value),
            line_step: 10,
            page_step: 100,
            cap: None,
            pointer: Point::ORIGIN,
            dragging: false,
        }
    }

    /// This slider with a non-default role (e.g. destructive or recovery).
    #[must_use]
    pub fn with_role(mut self, role: ControlRole) -> Self {
        self.role = role;
        self
    }

    /// This slider with the given line and page steps (permille), each clamped
    /// into `0..=1000`. A zero step moves nothing (fail closed, no guessed
    /// distance).
    #[must_use]
    pub fn with_steps(mut self, line_step: u16, page_step: u16) -> Self {
        self.line_step = clamp_permille(line_step);
        self.page_step = clamp_permille(page_step);
        self
    }

    /// This slider bounded to a maximum settable value (permille), shown as a
    /// cap marker; the value can neither be dragged nor stepped past it.
    #[must_use]
    pub fn with_cap(mut self, cap: u16) -> Self {
        let cap = clamp_permille(cap);
        self.cap = Some(cap);
        self.value = self.value.min(cap);
        self
    }

    /// The slider's current value, in permille.
    #[must_use]
    pub fn value(&self) -> u16 {
        self.value
    }

    /// Set the slider's value (e.g. after the owner commits a change), clamped
    /// into range and to any cap.
    pub fn set_value(&mut self, value: u16) {
        self.value = self.ceiling().min(clamp_permille(value));
    }

    /// The slider's role.
    #[must_use]
    pub fn role(&self) -> ControlRole {
        self.role
    }

    /// The slider's composed state.
    #[must_use]
    pub fn state(&self) -> ControlState {
        self.state
    }

    /// Replace the slider's composed state (e.g. from a model update).
    pub fn set_state(&mut self, state: ControlState) {
        self.state = state;
    }

    /// Set the slider's keyboard focus.
    pub fn set_focused(&mut self, focused: bool) {
        self.state.focus.focused = focused;
    }

    /// The highest value the slider may take: the cap if bounded, else full.
    fn ceiling(&self) -> u16 {
        self.cap.unwrap_or(FULL)
    }

    /// Request a new value, clamped to `0..=ceiling`; returns the action if the
    /// value actually changed, updating the displayed value immediately.
    fn request(&mut self, value: u16) -> Option<SliderAction> {
        let next = self.ceiling().min(clamp_permille(value));
        if next == self.value {
            return None;
        }
        self.value = next;
        Some(SliderAction::SetValue { permille: next })
    }

    /// Paint the slider into `surface` at `bounds` for the active theme.
    pub fn render(&self, surface: &mut Surface, bounds: Rect, scale: Scale, theme: &Theme) {
        let Some(layout) = slider_layout(bounds) else {
            return;
        };
        let palette = theme.palette();
        let border = plate_border(theme, scale);

        // The quiet groove the thumb runs along.
        surface.fill_round_rect(
            layout.track_x0,
            layout.groove_y,
            layout.travel + layout.thumb_d,
            layout.groove_h,
            layout.groove_h / 2,
            Color::from(palette.scroll_track),
        );

        // The value track, filled from the start to the thumb centre.
        let centre = layout.centre_for(self.value);
        let active = resolve_rail(theme, self.state)
            .unwrap_or_else(|| resolve_mark(theme, self.role, self.state));
        let active_w = centre.saturating_sub(layout.x).max(layout.groove_h);
        surface.fill_round_rect(
            layout.x,
            layout.groove_y,
            active_w,
            layout.groove_h,
            layout.groove_h / 2,
            active,
        );

        // The bounded-cap marker at the constrained edge, if any.
        if let Some(cap) = self.cap {
            if cap < FULL {
                let cap_x = layout.centre_for(cap);
                let tick_w = border.max(2).min(layout.thumb_d);
                surface.fill_rect(
                    cap_x.saturating_sub(tick_w / 2),
                    layout.y,
                    tick_w,
                    layout.h,
                    Color::from(palette.warning),
                );
            }
        }

        // The thumb: a small raised plate carrying the rim and focus ring.
        let frame = resolve_frame(theme, self.role, self.state);
        let thumb_x = centre.saturating_sub(layout.thumb_d / 2);
        paint_plate(
            surface,
            (thumb_x, layout.y, layout.thumb_d, layout.thumb_d),
            &PlateStyle {
                radius: layout.thumb_d / 2,
                border,
                plate: frame.plate,
                rim: frame.rim,
                focused: frame.focused,
                ring: Color::from(palette.rim_active),
            },
        );

        // The Signal Bead (denied lock / recovery / complete) at the top-right.
        if let Some((color, shape)) = resolve_bead(theme, self.state) {
            let size = scale
                .scale_length(theme.metrics().bead_size)
                .max(3)
                .min(layout.w)
                .min(layout.h);
            paint_bead(
                surface,
                layout.x + layout.w - size,
                layout.y,
                size,
                color,
                shape,
            );
        }
    }

    /// Feed a pointer event; a press/drag over an actionable slider updates the
    /// value and reports the requested change. A denied, disabled, pending, or
    /// failed-closed slider ignores pointer input (fail closed).
    pub fn on_pointer(&mut self, event: &InputEvent, bounds: Rect) -> Option<SliderAction> {
        if let InputEvent::PointerMoved { to } = event {
            self.pointer = *to;
        }
        let layout = slider_layout(bounds)?;
        let inside = bounds.contains(self.pointer);
        match event {
            InputEvent::PointerPressed {
                button: PointerButton::Primary,
            } => {
                if inside && self.state.is_actionable() {
                    self.dragging = true;
                    self.state.pointer = PointerState::Pressed;
                    return self.request(layout.value_for(self.pointer.x));
                }
                None
            }
            InputEvent::PointerMoved { .. } => {
                if self.dragging {
                    self.request(layout.value_for(self.pointer.x))
                } else {
                    self.state.pointer = if inside {
                        PointerState::Hover
                    } else {
                        PointerState::None
                    };
                    None
                }
            }
            InputEvent::PointerReleased {
                button: PointerButton::Primary,
            } => {
                self.dragging = false;
                self.state.pointer = if inside {
                    PointerState::Hover
                } else {
                    PointerState::None
                };
                None
            }
            _ => None,
        }
    }

    /// Feed a key event; arrows step by the line step, PageUp/PageDown by the
    /// page step, Home/End jump to the ends, on a focused, actionable slider.
    pub fn on_key(&mut self, key: Key) -> Option<SliderAction> {
        if !self.state.focus.focused || !self.state.is_actionable() {
            return None;
        }
        match key {
            Key::Named(NamedKey::Right | NamedKey::Up) => {
                self.request(self.value.saturating_add(self.line_step))
            }
            Key::Named(NamedKey::Left | NamedKey::Down) => {
                self.request(self.value.saturating_sub(self.line_step))
            }
            Key::Named(NamedKey::PageUp) => self.request(self.value.saturating_add(self.page_step)),
            Key::Named(NamedKey::PageDown) => {
                self.request(self.value.saturating_sub(self.page_step))
            }
            Key::Named(NamedKey::Home) => self.request(0),
            Key::Named(NamedKey::End) => self.request(FULL),
            _ => None,
        }
    }
}

/// A read-only instrument trace of known, working, indeterminate, complete, or
/// failed work (spec §11.7).
///
/// Progress is *not* decoration and runs no idle loop: its appearance is
/// driven entirely by the [`ControlState::activity`] its owner sets and, for an
/// indeterminate trace, by a [`phase`](Progress::set_phase) the owner advances
/// on job-progress events. Known progress shows a stable percentage; a failed
/// job shows a recovery rim and a concise reason; an indeterminate trace is a
/// bounded moving segment that renders statically under reduced motion.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Progress {
    role: ControlRole,
    state: ControlState,
    phase: u16,
    label: Option<String>,
}

impl Default for Progress {
    fn default() -> Self {
        Self::new()
    }
}

impl Progress {
    /// An idle neutral progress trace.
    #[must_use]
    pub fn new() -> Self {
        Self {
            role: ControlRole::Neutral,
            state: ControlState::idle(),
            phase: 0,
            label: None,
        }
    }

    /// This trace with a non-default role.
    #[must_use]
    pub fn with_role(mut self, role: ControlRole) -> Self {
        self.role = role;
        self
    }

    /// This trace with a caption (a value/throughput note, or a failure
    /// reason). The reason is concise user-facing text, never a secret.
    #[must_use]
    pub fn with_label(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }

    /// The trace's composed state.
    #[must_use]
    pub fn state(&self) -> ControlState {
        self.state
    }

    /// Replace the trace's composed state (e.g. from a model update).
    pub fn set_state(&mut self, state: ControlState) {
        self.state = state;
    }

    /// Advance the indeterminate animation phase (permille around the track).
    /// The owner calls this from a job-progress event, never an idle loop.
    pub fn set_phase(&mut self, phase: u16) {
        self.phase = clamp_permille(phase);
    }

    /// Whether the trace's linked object has failed or needs recovery.
    fn is_failed(&self) -> bool {
        self.state.disposition() == ControlDisposition::FailedClosed
            || self.state.recovery != RecoveryState::None
    }

    /// Paint the trace into `surface` at `bounds` for the active theme.
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
        let border = plate_border(theme, scale);
        let radius = scale.scale_length(metrics.control_corner_radius).min(h / 2);
        let failed = self.is_failed();
        let frame = resolve_frame(theme, self.role, self.state);
        let rim = if failed {
            Color::from(palette.recovery)
        } else {
            frame.rim
        };

        paint_plate(
            surface,
            (x, y, w, h),
            &PlateStyle {
                radius,
                border,
                plate: Color::from(palette.scroll_track),
                rim,
                focused: false,
                ring: Color::from(palette.rim_active),
            },
        );

        let inner_radius = radius.saturating_sub(border);
        if let Some((ix, iy, iw, ih)) = inset(x, y, w, h, border) {
            self.paint_fill(surface, (ix, iy, iw, ih), inner_radius, theme, failed);
        }

        if let Some((color, shape)) = resolve_bead(theme, self.state) {
            let size = scale.scale_length(metrics.bead_size).max(3).min(w).min(h);
            paint_bead(
                surface,
                x + w - border - size,
                y + border,
                size,
                color,
                shape,
            );
        }

        self.paint_caption(surface, (x, y, w, h), scale, theme, font, failed);
    }

    /// Paint the value fill for the current activity within the inner area.
    fn paint_fill(
        &self,
        surface: &mut Surface,
        inner: (u32, u32, u32, u32),
        inner_radius: u32,
        theme: &Theme,
        failed: bool,
    ) {
        let (ix, iy, iw, ih) = inner;
        if iw == 0 || ih == 0 || failed {
            return;
        }
        let palette = theme.palette();
        let accent = resolve_rail(theme, self.state)
            .unwrap_or_else(|| resolve_mark(theme, self.role, self.state));
        match self.state.activity {
            ActivityState::Progress(v) => {
                let fill_w =
                    u32::try_from(u64::from(iw) * u64::from(v.permille()) / u64::from(FULL))
                        .unwrap_or(iw);
                if fill_w > 0 {
                    surface.fill_round_rect(ix, iy, fill_w, ih, inner_radius, accent);
                }
            }
            ActivityState::Working | ActivityState::Indeterminate => {
                let seg_w = (iw / 4).max(1);
                let travel = iw.saturating_sub(seg_w);
                let pos = if theme.motion().reduced_motion() {
                    travel / 2
                } else {
                    u32::try_from(u64::from(travel) * u64::from(self.phase) / u64::from(FULL))
                        .unwrap_or(travel)
                };
                surface.fill_round_rect(ix + pos, iy, seg_w, ih, inner_radius, accent);
            }
            ActivityState::Complete => {
                surface.fill_round_rect(ix, iy, iw, ih, inner_radius, Color::from(palette.success));
            }
            ActivityState::Idle => {}
        }
    }

    /// Paint the caption: a percentage for known progress, else the reason /
    /// note label, vertically and horizontally centred within the plate.
    fn paint_caption(
        &self,
        surface: &mut Surface,
        rect: (u32, u32, u32, u32),
        scale: Scale,
        theme: &Theme,
        font: BitmapFont,
        failed: bool,
    ) {
        let (x, y, w, h) = rect;
        let border = plate_border(theme, scale);
        let pad = scale.scale_length(theme.metrics().control_inset);
        let edge = border.saturating_add(pad);
        let avail = w.saturating_sub(edge.saturating_mul(2));
        if avail == 0 {
            return;
        }
        let palette = theme.palette();
        let (text, color): (String, Color) = if failed {
            match &self.label {
                Some(reason) => (reason.clone(), Color::from(palette.recovery)),
                None => return,
            }
        } else if let ActivityState::Progress(v) = self.state.activity {
            let pct = ((u32::from(v.permille()) + 5) / 10).min(100);
            (format!("{pct}%"), Color::from(palette.on_surface))
        } else {
            match &self.label {
                Some(note) => (note.clone(), Color::from(palette.on_surface)),
                None => return,
            }
        };
        let fitted = font.truncate_to_width(&text, avail);
        let width = font.text_width(fitted);
        let glyph_h = font.glyph_height();
        let cx = to_i32(x) + to_i32(w) / 2;
        let text_y = to_i32(y) + (to_i32(h) - to_i32(glyph_h)).max(0) / 2;
        font.draw_text(surface, cx - to_i32(width) / 2, text_y, fitted, color);
    }
}
