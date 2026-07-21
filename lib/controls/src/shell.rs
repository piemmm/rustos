//! Shell-surface controls: [`Notification`], [`TaskbarItem`], and
//! [`TraySignal`] (spec §11.25–§11.27).
//!
//! These are the desktop's *shell* surfaces — the transient message, the
//! taskbar entry, and the notification-area status capsule. Each is a
//! first-class Reactive Alloy control drawn over the shared `crate::paint`
//! core (plate, rail, Heat Seam, Signal Bead) and the shared `lib/theme`
//! tokens, so nothing here restates a visual recipe (`AGENTS.md` §2.2). A
//! control renders state and emits a typed userland action; the owning
//! service enforces authority, and a denied action reads distinctly from a
//! disabled one (spec §13).

use alloc::string::String;
use alloc::vec::Vec;

use tairix_font::BitmapFont;
use tairix_geometry::{Point, Rect, Scale};
use tairix_icon::{builtin_icon, IconKind};
use tairix_input::{InputEvent, Key};
use tairix_raster::{Color, Surface};
use tairix_theme::Theme;

use crate::button::{Button, ButtonAction};
use crate::collection::{Card, CardAction};
use crate::paint::{
    foreground, inset, key_activation, paint_bead, paint_plate, plate_border, pointer_activation,
    rail_thickness, resolve_bead, resolve_frame, resolve_rail, seam_thickness, seam_width,
    surface_rect, to_i32, BeadShape, PlateStyle,
};
use crate::state::{
    ControlDisposition, ControlRole, ControlState, PointerState, RecoveryState, ValidationState,
};

// --- Notification ------------------------------------------------------

/// The outcome of feeding input to a [`Notification`].
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum NotificationAction {
    /// The notification action at `index` was activated (e.g. a "Clear" or
    /// "Recover" button); the owner performs it and enforces authority.
    ActionActivated {
        /// The zero-based index of the activated action button.
        index: usize,
    },
}

/// A compact, actionable transient message (spec §11.25).
///
/// A notification *is* a [`Card`] carrying semantic beads, plus an optional
/// *source* attribution (the application or service that raised it). The card's
/// composed [`ControlState`] drives the reading: an informational notice keeps
/// the quiet rim, a background job shows a Heat Seam (its `activity`), a warning
/// shows the warning rail (its `validation`), a recoverable object shows the
/// recovery bead (its `recovery`), and a refused action shows the Authority Mark
/// (its `authority`) beside the source name — never a generic disabled look
/// (spec §13). Its actions are footer [`Button`]s; the notification routes input
/// to them and reports [`NotificationAction`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Notification {
    card: Card,
    source: Option<String>,
}

impl Notification {
    /// A neutral, informational notification with the given title.
    #[must_use]
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            card: Card::new(title),
            source: None,
        }
    }

    /// This notification with a message line below the title.
    #[must_use]
    pub fn with_message(mut self, message: impl Into<String>) -> Self {
        self.card = self.card.with_body(message);
        self
    }

    /// This notification with a source application/service attribution.
    #[must_use]
    pub fn with_source(mut self, source: impl Into<String>) -> Self {
        self.source = Some(source.into());
        self
    }

    /// This notification with a non-default role.
    #[must_use]
    pub fn with_role(mut self, role: ControlRole) -> Self {
        self.card = self.card.with_role(role);
        self
    }

    /// This notification with the given composed state (drives its semantics).
    #[must_use]
    pub fn with_state(mut self, state: ControlState) -> Self {
        self.card = self.card.with_state(state);
        self
    }

    /// This notification with a top-trailing count badge (grouped count).
    #[must_use]
    pub fn with_count(mut self, count: u32) -> Self {
        self.card = self.card.with_count(count);
        self
    }

    /// This notification with the given action buttons.
    #[must_use]
    pub fn with_actions(mut self, actions: Vec<Button>) -> Self {
        self.card = self.card.with_footer(actions);
        self
    }

    /// The notification's source attribution, if any.
    #[must_use]
    pub fn source(&self) -> Option<&str> {
        self.source.as_deref()
    }

    /// The notification's composed state.
    #[must_use]
    pub fn state(&self) -> ControlState {
        self.card.state()
    }

    /// Replace the notification's composed state.
    pub fn set_state(&mut self, state: ControlState) {
        self.card.set_state(state);
    }

    /// The notification's action buttons.
    #[must_use]
    pub fn actions(&self) -> &[Button] {
        self.card.footer()
    }

    /// Mutable access to the action buttons (e.g. to update their state).
    pub fn actions_mut(&mut self) -> &mut [Button] {
        self.card.footer_mut()
    }

    /// The height of the source caption strip, in surface pixels.
    fn caption_height(scale: Scale, theme: &Theme, font: BitmapFont) -> u32 {
        font.line_height()
            .saturating_add(scale.scale_length(theme.metrics().control_inset).max(1))
    }

    /// The card sub-rectangle, below the source caption when a source is shown.
    fn card_bounds(&self, bounds: Rect, scale: Scale, theme: &Theme, font: BitmapFont) -> Rect {
        if self.source.is_none() {
            return bounds;
        }
        let caption = Self::caption_height(scale, theme, font);
        let h = bounds.height.saturating_sub(caption);
        Rect::new(
            bounds.left(),
            bounds.top().saturating_add(to_i32(caption)),
            bounds.width,
            h,
        )
    }

    /// Paint the notification into `surface` at `bounds` for the active theme.
    pub fn render(
        &self,
        surface: &mut Surface,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
        font: BitmapFont,
    ) {
        if let Some(source) = &self.source {
            if let Some((x, y, w, _)) = surface_rect(bounds) {
                let pad = scale.scale_length(theme.metrics().control_inset).max(1);
                if w > pad.saturating_mul(2) {
                    let fitted = font.truncate_to_width(source, w - pad.saturating_mul(2));
                    font.draw_text(
                        surface,
                        to_i32(x + pad),
                        to_i32(y + pad / 2),
                        fitted,
                        foreground(theme, ControlDisposition::DisabledByState),
                    );
                }
            }
        }
        self.card.render(
            surface,
            self.card_bounds(bounds, scale, theme, font),
            scale,
            theme,
            font,
        );
    }

    /// Feed a pointer event; a footer action that completes a click reports
    /// [`NotificationAction::ActionActivated`].
    pub fn on_pointer(
        &mut self,
        event: &InputEvent,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
        font: BitmapFont,
    ) -> Option<NotificationAction> {
        let card_bounds = self.card_bounds(bounds, scale, theme, font);
        self.card.on_pointer(event, card_bounds, scale, theme).map(
            |CardAction::FooterActivated { index }| NotificationAction::ActionActivated { index },
        )
    }

    /// Feed a key event; a focused footer action activated with Space/Enter
    /// reports [`NotificationAction::ActionActivated`].
    pub fn on_key(&mut self, key: Key) -> Option<NotificationAction> {
        self.card
            .on_key(key)
            .map(
                |CardAction::FooterActivated { index }| NotificationAction::ActionActivated {
                    index,
                },
            )
    }
}

// --- TaskbarItem -------------------------------------------------------

/// The outcome of feeding input to a [`TaskbarItem`].
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum TaskbarItemAction {
    /// The item was activated (clicked, or Space/Enter while focused); the
    /// owner decides whether to focus, restore, or minimize the window.
    Activated,
}

/// A taskbar item's window-visibility state (spec §11.26).
///
/// These are mutually exclusive — a window cannot be both the active window and
/// minimized at once — so they are one enum rather than independent flags
/// (illegal states unrepresentable).
#[derive(Copy, Clone, Debug, Eq, PartialEq, Default)]
pub enum TaskVisibility {
    /// Running and visible, but not the active window.
    #[default]
    Running,
    /// The active (focused) window — shown with a lower accent seam.
    Active,
    /// Minimized — a recessed plate and a non-colour mark, still restorable.
    Minimized,
}

/// A taskbar entry for one application/window (spec §11.26).
///
/// A taskbar item combines application identity (an icon and a label), live
/// activity, attention, and window-visibility state on one Alloy Plate. A
/// running item shows its plate; the *active* window's item shows a lower accent
/// seam; a minimized window's item recesses its plate and shows a distinct
/// non-colour mark while remaining restorable; background work shows a Heat
/// Seam; an attention request or a recovery/denied state shows a shape-coded
/// Signal Bead (spec §13, §15). It renders state and reports
/// [`TaskbarItemAction`]; the owner performs the window operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskbarItem {
    label: String,
    icon: IconKind,
    state: ControlState,
    visibility: TaskVisibility,
    attention: bool,
    pointer: Point,
    armed: bool,
}

impl TaskbarItem {
    /// A running, inactive taskbar item with the given label and icon.
    #[must_use]
    pub fn new(label: impl Into<String>, icon: IconKind) -> Self {
        Self {
            label: label.into(),
            icon,
            state: ControlState::idle(),
            visibility: TaskVisibility::Running,
            attention: false,
            pointer: Point::ORIGIN,
            armed: false,
        }
    }

    /// This item with the given composed state.
    #[must_use]
    pub fn with_state(mut self, state: ControlState) -> Self {
        self.state = state;
        self
    }

    /// This item with the given window-visibility state.
    #[must_use]
    pub fn with_visibility(mut self, visibility: TaskVisibility) -> Self {
        self.visibility = visibility;
        self
    }

    /// This item marked as requesting attention (Signal Bead).
    #[must_use]
    pub fn with_attention(mut self, attention: bool) -> Self {
        self.attention = attention;
        self
    }

    /// The item's label.
    #[must_use]
    pub fn label(&self) -> &str {
        &self.label
    }

    /// The item's composed state.
    #[must_use]
    pub fn state(&self) -> ControlState {
        self.state
    }

    /// Replace the item's composed state.
    pub fn set_state(&mut self, state: ControlState) {
        self.state = state;
    }

    /// Set the item's keyboard focus.
    pub fn set_focused(&mut self, focused: bool) {
        self.state.focus.focused = focused;
    }

    /// Set the item's window-visibility state.
    pub fn set_visibility(&mut self, visibility: TaskVisibility) {
        self.visibility = visibility;
    }

    /// The item's window-visibility state.
    #[must_use]
    pub fn visibility(&self) -> TaskVisibility {
        self.visibility
    }

    /// Set whether this item is requesting attention.
    pub fn set_attention(&mut self, attention: bool) {
        self.attention = attention;
    }

    /// Whether this item is the active window's entry.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.visibility == TaskVisibility::Active
    }

    /// The Signal Bead the item shows, if any: an authority/recovery/complete
    /// bead (shared priority) wins, then an attention request draws an accent
    /// bead — so a denial is never hidden behind an attention notice.
    fn bead(&self, theme: &Theme) -> Option<(Color, BeadShape)> {
        if let Some(bead) = resolve_bead(theme, self.state) {
            return Some(bead);
        }
        if self.attention {
            return Some((Color::from(theme.palette().accent), BeadShape::Check));
        }
        None
    }

    /// Paint the item into `surface` at `bounds` for the active theme.
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
        let radius = scale.scale_length(metrics.control_corner_radius);
        let frame = resolve_frame(theme, ControlRole::Neutral, self.state);

        // A minimized item recesses its plate (a flatter fill than the raised
        // resting plate) so it reads as put-away without relying on colour.
        let plate = if self.visibility == TaskVisibility::Minimized
            && self.state.disposition() != ControlDisposition::DisabledByState
        {
            Color::from(palette.surface)
        } else {
            frame.plate
        };
        paint_plate(
            surface,
            (x, y, w, h),
            &PlateStyle {
                radius,
                border,
                plate,
                rim: frame.rim,
                focused: frame.focused,
                ring: Color::from(palette.rim_active),
            },
        );

        let inner_x = x + border;
        let inner_y = y + border;
        let inner_w = w.saturating_sub(border.saturating_mul(2));
        let inner_h = h.saturating_sub(border.saturating_mul(2));
        if inner_w == 0 || inner_h == 0 {
            return;
        }
        let pad = scale.scale_length(metrics.control_inset).max(1);

        // The application identity: leading icon then label.
        let side = font.glyph_height().min(inner_h);
        let mut content_x = inner_x.saturating_add(pad);
        if side > 0 && content_x.saturating_add(side) < inner_x + inner_w {
            if let Some(image) = builtin_icon(self.icon, frame.label).rasterise(side) {
                let iy = inner_y + (inner_h.saturating_sub(side)) / 2;
                surface.blit(to_i32(content_x), to_i32(iy), &image);
            }
            content_x = content_x.saturating_add(side).saturating_add(pad);
        }
        let bead_size = self.bead(theme).map_or(0, |_| {
            scale
                .scale_length(metrics.bead_size)
                .max(3)
                .min(inner_w)
                .min(inner_h)
        });
        let label_right = (inner_x + inner_w)
            .saturating_sub(pad)
            .saturating_sub(if bead_size > 0 { bead_size + pad } else { 0 });
        if label_right > content_x {
            let fitted = font.truncate_to_width(&self.label, label_right - content_x);
            let text_y =
                to_i32(inner_y) + (to_i32(inner_h) - to_i32(font.glyph_height())).max(0) / 2;
            font.draw_text(surface, to_i32(content_x), text_y, fitted, frame.label);
        }

        self.paint_status(
            surface,
            (inner_x, inner_y, inner_w, inner_h),
            border,
            bead_size,
            scale,
            theme,
        );
    }

    /// Paint the item's status edges and marks: the active-window accent seam,
    /// the activity Heat Seam above it, the minimized non-colour tick, and the
    /// top-trailing Signal Bead.
    fn paint_status(
        &self,
        surface: &mut Surface,
        inner: (u32, u32, u32, u32),
        border: u32,
        bead_size: u32,
        scale: Scale,
        theme: &Theme,
    ) {
        let (inner_x, inner_y, inner_w, inner_h) = inner;
        let palette = theme.palette();
        // Bottom edge: the active-window accent seam sits on the very bottom;
        // the activity Heat Seam sits just above it so both read at once.
        let seam_h = seam_thickness(theme, scale).min(inner_h);
        let mut seam_floor = inner_y + inner_h;
        if self.visibility == TaskVisibility::Active {
            surface.fill_rect(
                inner_x,
                seam_floor - seam_h,
                inner_w,
                seam_h,
                Color::from(palette.accent),
            );
            seam_floor = seam_floor.saturating_sub(seam_h);
        }
        let activity_w = seam_width(self.state.activity, inner_w);
        if activity_w > 0 && seam_floor > inner_y + seam_h {
            surface.fill_rect(
                inner_x,
                seam_floor - seam_h,
                activity_w,
                seam_h,
                Color::from(palette.accent),
            );
        }

        // A minimized item's distinct non-colour mark: a short muted tick on
        // the leading edge (present regardless of hue).
        if self.visibility == TaskVisibility::Minimized {
            let tick_h = inner_h / 3;
            if tick_h > 0 {
                let ty = inner_y + (inner_h.saturating_sub(tick_h)) / 2;
                surface.fill_rect(
                    inner_x,
                    ty,
                    border.max(1),
                    tick_h,
                    Color::from(palette.on_surface_muted),
                );
            }
        }

        // The top-trailing Signal Bead.
        if let Some((color, shape)) = self.bead(theme) {
            if bead_size > 0 {
                let bx = inner_x + inner_w - bead_size;
                paint_bead(surface, bx, inner_y, bead_size, color, shape);
            }
        }
    }

    /// Feed a pointer event; a completed primary click reports
    /// [`TaskbarItemAction::Activated`].
    pub fn on_pointer(&mut self, event: &InputEvent, bounds: Rect) -> Option<TaskbarItemAction> {
        if let InputEvent::PointerMoved { to } = event {
            self.pointer = *to;
        }
        let inside = bounds.contains(self.pointer);
        pointer_activation(&mut self.state, &mut self.armed, event, inside)
            .then_some(TaskbarItemAction::Activated)
    }

    /// Feed a key event; Space/Enter while focused reports
    /// [`TaskbarItemAction::Activated`].
    pub fn on_key(&mut self, key: Key) -> Option<TaskbarItemAction> {
        key_activation(self.state, key).then_some(TaskbarItemAction::Activated)
    }
}

// --- TraySignal --------------------------------------------------------

/// The outcome of feeding input to a [`TraySignal`].
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum TraySignalAction {
    /// The readout's primary safe action was activated; the owner performs it.
    Activated,
}

/// A compact live status capsule in the notification area (spec §11.27).
///
/// A tray signal is a small glyph capsule with a calm rim: background work adds
/// a lower Heat Seam, a resource pressure adds a leading semantic rail, and one
/// or more alert states stack as severity-ordered mini Signal Beads on the
/// top-trailing corner (so several states read at once without colour, spec
/// §15). On hover or keyboard focus it expands to a short instrument readout —
/// the state name, a count or value, and one primary safe action — which the
/// owner positions as a popup. It renders state and reports
/// [`TraySignalAction`]; the owner enforces authority (spec §13).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TraySignal {
    icon: IconKind,
    label: String,
    value: Option<String>,
    state: ControlState,
    action: Option<Button>,
    pointer: Point,
}

impl TraySignal {
    /// A calm tray signal with the given glyph and state-name label.
    #[must_use]
    pub fn new(icon: IconKind, label: impl Into<String>) -> Self {
        Self {
            icon,
            label: label.into(),
            value: None,
            state: ControlState::idle(),
            action: None,
            pointer: Point::ORIGIN,
        }
    }

    /// This signal with a readout count/value (e.g. a throughput or a count).
    #[must_use]
    pub fn with_value(mut self, value: impl Into<String>) -> Self {
        self.value = Some(value.into());
        self
    }

    /// This signal with the given composed state (drives seam/rail/beads).
    #[must_use]
    pub fn with_state(mut self, state: ControlState) -> Self {
        self.state = state;
        self
    }

    /// This signal with a primary safe action shown in its readout.
    #[must_use]
    pub fn with_action(mut self, action: Button) -> Self {
        self.action = Some(action);
        self
    }

    /// The signal's state-name label.
    #[must_use]
    pub fn label(&self) -> &str {
        &self.label
    }

    /// The signal's composed state.
    #[must_use]
    pub fn state(&self) -> ControlState {
        self.state
    }

    /// Replace the signal's composed state.
    pub fn set_state(&mut self, state: ControlState) {
        self.state = state;
    }

    /// Set the signal's keyboard focus (focus also expands the readout).
    pub fn set_focused(&mut self, focused: bool) {
        self.state.focus.focused = focused;
    }

    /// Whether the readout is expanded — on hover or keyboard focus.
    #[must_use]
    pub fn is_expanded(&self) -> bool {
        self.state.pointer == PointerState::Hover || self.state.focus.focused
    }

    /// The severity-ordered alert beads the capsule stacks, highest severity
    /// first: an authority denial, then a recovery/failed-closed state, then a
    /// validation warning, then a completion. Several states stack; none hides
    /// another.
    fn beads(&self, theme: &Theme) -> Vec<(Color, BeadShape)> {
        let palette = theme.palette();
        let mut beads = Vec::new();
        match self.state.disposition() {
            ControlDisposition::DeniedByAuthority => {
                beads.push((Color::from(palette.denied), BeadShape::Lock));
            }
            ControlDisposition::FailedClosed => {
                beads.push((Color::from(palette.recovery), BeadShape::Diamond));
            }
            _ => {}
        }
        if self.state.recovery != RecoveryState::None
            && self.state.disposition() != ControlDisposition::FailedClosed
        {
            beads.push((Color::from(palette.recovery), BeadShape::Diamond));
        }
        if self.state.validation == ValidationState::Warning {
            beads.push((Color::from(palette.warning), BeadShape::Diamond));
        }
        if matches!(self.state.activity, crate::state::ActivityState::Complete) {
            beads.push((Color::from(palette.success), BeadShape::Check));
        }
        beads
    }

    /// Paint the compact capsule into `surface` at `bounds`.
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
        let radius = scale.scale_length(metrics.control_corner_radius);
        let frame = resolve_frame(theme, ControlRole::Neutral, self.state);
        paint_plate(
            surface,
            (x, y, w, h),
            &PlateStyle {
                radius,
                border,
                plate: frame.plate,
                rim: frame.rim,
                focused: frame.focused,
                ring: Color::from(palette.rim_active),
            },
        );

        let inner_x = x + border;
        let inner_y = y + border;
        let inner_w = w.saturating_sub(border.saturating_mul(2));
        let inner_h = h.saturating_sub(border.saturating_mul(2));
        if inner_w == 0 || inner_h == 0 {
            return;
        }

        // Leading pressure rail.
        if let Some(color) = resolve_rail(theme, self.state) {
            let rail_w = rail_thickness(theme, scale).min(inner_w);
            surface.fill_rect(inner_x, inner_y, rail_w, inner_h, color);
        }

        // The calm glyph, centred.
        let side = font.glyph_height().min(inner_w).min(inner_h);
        if side > 0 {
            if let Some(image) = builtin_icon(self.icon, frame.label).rasterise(side) {
                let ix = inner_x + (inner_w.saturating_sub(side)) / 2;
                let iy = inner_y + (inner_h.saturating_sub(side)) / 2;
                surface.blit(to_i32(ix), to_i32(iy), &image);
            }
        }

        // Lower Heat Seam for background work.
        let seam_h = seam_thickness(theme, scale).min(inner_h);
        let seam_w = seam_width(self.state.activity, inner_w);
        if seam_w > 0 {
            surface.fill_rect(
                inner_x,
                inner_y + inner_h - seam_h,
                seam_w,
                seam_h,
                Color::from(palette.accent),
            );
        }

        // Severity-ordered mini beads stacked from the top-trailing corner.
        let beads = self.beads(theme);
        if !beads.is_empty() {
            let mini = (scale
                .scale_length(metrics.bead_size)
                .max(3)
                .saturating_mul(2)
                / 3)
            .max(2)
            .min(inner_w)
            .min(inner_h);
            let gap = (mini / 3).max(1);
            let mut bx = inner_x + inner_w;
            for (color, shape) in beads {
                if bx < inner_x + mini {
                    break;
                }
                bx = bx.saturating_sub(mini);
                paint_bead(surface, bx, inner_y, mini, color, shape);
                bx = bx.saturating_sub(gap);
            }
        }
    }

    /// The readout popup's preferred `(width, height)` in surface pixels — a
    /// state name, an optional value, and the primary action, so the owner can
    /// size the popup surface it hosts the readout in.
    #[must_use]
    pub fn readout_size(&self, scale: Scale, theme: &Theme, font: BitmapFont) -> (u32, u32) {
        let pad = scale.scale_length(theme.metrics().control_inset).max(1);
        let line = font.line_height();
        let mut text_w = font.text_width(&self.label);
        if let Some(value) = &self.value {
            text_w = text_w.max(font.text_width(value));
        }
        let action_h = self.action.as_ref().map_or(0, |_| {
            scale.scale_length(theme.metrics().control_height).max(1) + pad
        });
        let action_w = self.action.as_ref().map_or(0, |a| match a.content() {
            crate::button::ButtonContent::Label(t) => font.text_width(t) + pad.saturating_mul(4),
            _ => scale.scale_length(theme.metrics().control_height).max(1),
        });
        let value_h = self.value.as_ref().map_or(0, |_| line);
        let w = text_w.max(action_w).saturating_add(pad.saturating_mul(2));
        let h = line
            .saturating_add(value_h)
            .saturating_add(action_h)
            .saturating_add(pad.saturating_mul(2));
        (w.max(1), h.max(1))
    }

    /// The readout's primary-action button rectangle within `bounds`, shared by
    /// [`render_readout`](Self::render_readout) and pointer routing so the two
    /// never disagree.
    fn action_rect(&self, bounds: Rect, scale: Scale, theme: &Theme) -> Option<Rect> {
        self.action.as_ref()?;
        let (x, y, w, h) = surface_rect(bounds)?;
        let pad = scale.scale_length(theme.metrics().control_inset).max(1);
        let bh = scale.scale_length(theme.metrics().control_height).max(1);
        let (ix, iy, iw, ih) = inset(x, y, w, h, pad)?;
        if ih <= bh {
            return None;
        }
        Some(Rect::new(to_i32(ix), to_i32(iy + ih - bh), iw, bh))
    }

    /// Paint the expanded readout into `surface` at `bounds` — an elevated plate
    /// with the state name, the value, and the primary action. The owner calls
    /// this when [`is_expanded`](Self::is_expanded) is set, at the popup
    /// rectangle it sized from [`readout_size`](Self::readout_size).
    pub fn render_readout(
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
        let border = plate_border(theme, scale);
        let radius = scale.scale_length(theme.metrics().popup_corner_radius);
        surface.fill_round_rect(x, y, w, h, radius, Color::from(palette.rim));
        if let Some((ix, iy, iw, ih)) = inset(x, y, w, h, border) {
            surface.fill_round_rect(
                ix,
                iy,
                iw,
                ih,
                radius.saturating_sub(border),
                Color::from(palette.surface_raised),
            );
        }
        let pad = scale.scale_length(theme.metrics().control_inset).max(1);
        let text_x = to_i32(x + border + pad);
        let mut text_y = to_i32(y + border + pad);
        let text_w = w.saturating_sub((border + pad).saturating_mul(2));
        if text_w > 0 {
            let fitted = font.truncate_to_width(&self.label, text_w);
            font.draw_text(
                surface,
                text_x,
                text_y,
                fitted,
                foreground(theme, self.state.disposition()),
            );
            if let Some(value) = &self.value {
                text_y += to_i32(font.line_height());
                let fitted = font.truncate_to_width(value, text_w);
                font.draw_text(
                    surface,
                    text_x,
                    text_y,
                    fitted,
                    Color::from(palette.on_surface_muted),
                );
            }
        }
        if let (Some(action), Some(rect)) = (&self.action, self.action_rect(bounds, scale, theme)) {
            action.render(surface, rect, scale, theme, font);
        }
    }

    /// Feed a pointer event. `capsule_bounds` is the compact capsule; when the
    /// readout is expanded, `readout_bounds` is the popup rectangle. Hovering
    /// either keeps the readout open, and the readout's action reports
    /// [`TraySignalAction::Activated`].
    pub fn on_pointer(
        &mut self,
        event: &InputEvent,
        capsule_bounds: Rect,
        readout_bounds: Rect,
        scale: Scale,
        theme: &Theme,
    ) -> Option<TraySignalAction> {
        if let InputEvent::PointerMoved { to } = event {
            self.pointer = *to;
        }
        let expanded_before = self.is_expanded();
        let over_capsule = capsule_bounds.contains(self.pointer);
        let over_readout = expanded_before && readout_bounds.contains(self.pointer);
        self.state.pointer = if over_capsule || over_readout {
            PointerState::Hover
        } else {
            PointerState::None
        };
        if self.is_expanded() {
            if let Some(rect) = self.action_rect(readout_bounds, scale, theme) {
                if let Some(button) = self.action.as_mut() {
                    if button.on_pointer(event, rect) == Some(ButtonAction::Activated) {
                        return Some(TraySignalAction::Activated);
                    }
                }
            }
        }
        None
    }

    /// Feed a key event; when focused (readout expanded) Space/Enter activates
    /// the primary action, reporting [`TraySignalAction::Activated`].
    pub fn on_key(&mut self, key: Key) -> Option<TraySignalAction> {
        if self.action.is_some() && key_activation(self.state, key) {
            return Some(TraySignalAction::Activated);
        }
        None
    }
}
