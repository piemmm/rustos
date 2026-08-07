//! Decision-surface controls: [`Dialog`], [`Tooltip`], and [`HelpTip`]
//! (spec §11.24, §11.32).
//!
//! These are the desktop's *decision* surfaces — the modal choice, the
//! immediate-affordance hint, and the explanation of why an action is
//! unavailable or recommended. Each is drawn over the shared `crate::paint`
//! core (the one elevated-plate recipe) and the shared `lib/theme` tokens, so
//! nothing here restates a visual recipe. A dialog and a
//! help tip render state and emit typed actions; the owning service enforces
//! authority, and a capability denial reads distinctly from a disabled control
//! — never collapsed into a generic inactive look (spec §13).

use alloc::string::String;
use alloc::vec::Vec;

use tairix_font::BitmapFont;
use tairix_geometry::{Rect, Scale};
use tairix_input::{InputEvent, Key};
use tairix_raster::{Color, Surface};
use tairix_theme::{TextRole, Theme};

use crate::button::{Button, ButtonAction, ButtonContent};
use crate::paint::{
    foreground, inset, paint_plate, plate_border, role_font, surface_rect, text_plate_height,
    to_i32, PlateStyle,
};
use crate::state::ControlRole;

/// The natural width one action button needs: a labelled button fits its text
/// plus horizontal padding; a glyph button is a square of the control height.
/// One definition shared by the decision surfaces so their action rows lay out
/// identically.
fn button_width(button: &Button, scale: Scale, theme: &Theme, font: BitmapFont) -> u32 {
    let pad = scale.scale_length(theme.metrics().control_inset).max(1);
    match button.content() {
        ButtonContent::Label(text) => font.text_width(text).saturating_add(pad.saturating_mul(4)),
        _ => scale.scale_length(theme.metrics().control_height).max(1),
    }
}

/// Lay a row of action buttons right-aligned along the bottom of `inner`,
/// returning one rect per button in index order (so the trailing button — by
/// convention the recommended/primary action — sits on the right edge). Shared
/// by the dialog and help-tip renderers and their pointer routing so the two
/// never disagree.
fn action_row_rects(
    buttons: &[Button],
    inner: (u32, u32, u32, u32),
    scale: Scale,
    theme: &Theme,
    font: BitmapFont,
) -> Vec<Rect> {
    let mut rects = Vec::new();
    if buttons.is_empty() {
        return rects;
    }
    let (ix, iy, iw, ih) = inner;
    let pad = scale.scale_length(theme.metrics().control_inset).max(1);
    let gap = scale.scale_length(theme.metrics().control_gap).max(1);
    let bh = text_plate_height(theme, scale, TextRole::Body);
    if ih <= bh.saturating_add(pad) {
        return rects;
    }
    let top = iy + ih - bh;
    let left_bound = ix.saturating_add(pad);
    let widths: Vec<u32> = buttons
        .iter()
        .map(|b| button_width(b, scale, theme, font))
        .collect();
    // Place from the right edge leftwards, then reverse into index order.
    let mut right = ix.saturating_add(iw).saturating_sub(pad);
    let mut placed: Vec<Rect> = Vec::new();
    for width in widths.iter().rev() {
        let w = (*width).min(right.saturating_sub(left_bound));
        if w == 0 || right <= left_bound {
            break;
        }
        let bx = right.saturating_sub(w);
        placed.push(Rect::new(to_i32(bx), to_i32(top), w, bh));
        right = bx.saturating_sub(gap);
    }
    placed.reverse();
    // `placed` holds the buttons that fit, from the last index toward the
    // first; align it back to the leading indices so index 0 maps to rect 0.
    let start = buttons.len().saturating_sub(placed.len());
    for _ in 0..start {
        rects.push(Rect::new(0, 0, 0, 0));
    }
    rects.extend(placed);
    rects
}

// --- Dialog ------------------------------------------------------------

/// The outcome of feeding input to a [`Dialog`].
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum DialogAction {
    /// The dialog action at `index` was activated; the owner performs it and
    /// enforces authority.
    ActionActivated {
        /// The zero-based index of the activated action button.
        index: usize,
    },
}

/// A modal decision surface (spec §11.24).
///
/// A dialog is an elevated plate carrying a title, a message, and a right-
/// aligned row of action [`Button`]s (the trailing one being, by convention,
/// the recommended action). Action Warmth is honest: an action is warm only
/// when its [`ControlRole`] is [`Recommended`](ControlRole::Recommended) or
/// [`Primary`](ControlRole::Primary); a destructive action carries
/// [`Destructive`](ControlRole::Destructive) and the caller sets its
/// confirmation posture, and a blocked action shows the Authority Mark rather
/// than a generic disabled look (spec §13). An optional inline reason explains
/// why an action is unavailable. The dialog routes input to its actions and
/// reports [`DialogAction`]; it performs no privileged work.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Dialog {
    title: String,
    message: Option<String>,
    reason: Option<String>,
    actions: Vec<Button>,
}

impl Dialog {
    /// A dialog with the given title and no message or actions.
    #[must_use]
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            message: None,
            reason: None,
            actions: Vec::new(),
        }
    }

    /// This dialog with a message body.
    #[must_use]
    pub fn with_message(mut self, message: impl Into<String>) -> Self {
        self.message = Some(message.into());
        self
    }

    /// This dialog with an inline reason explaining why an action is blocked or
    /// recommended (concise, never a secret or capability token, spec §13).
    #[must_use]
    pub fn with_reason(mut self, reason: impl Into<String>) -> Self {
        self.reason = Some(reason.into());
        self
    }

    /// This dialog with the given action buttons (trailing = recommended).
    #[must_use]
    pub fn with_actions(mut self, actions: Vec<Button>) -> Self {
        self.actions = actions;
        self
    }

    /// The dialog's title.
    #[must_use]
    pub fn title(&self) -> &str {
        &self.title
    }

    /// The dialog's message body, if any.
    #[must_use]
    pub fn message(&self) -> Option<&str> {
        self.message.as_deref()
    }

    /// The dialog's inline reason, if any.
    #[must_use]
    pub fn reason(&self) -> Option<&str> {
        self.reason.as_deref()
    }

    /// The dialog's action buttons.
    #[must_use]
    pub fn actions(&self) -> &[Button] {
        &self.actions
    }

    /// Mutable access to the action buttons (e.g. to update their state).
    pub fn actions_mut(&mut self) -> &mut [Button] {
        &mut self.actions
    }

    /// The inner content rectangle (inside the rim) as surface pixels.
    fn inner(bounds: Rect, scale: Scale, theme: &Theme) -> Option<(u32, u32, u32, u32)> {
        let (x, y, w, h) = surface_rect(bounds)?;
        inset(x, y, w, h, plate_border(theme, scale))
    }

    /// Paint the dialog into `surface` at `bounds` for the active theme.
    pub fn render(&self, surface: &mut Surface, bounds: Rect, scale: Scale, theme: &Theme) {
        let font = role_font(theme, scale, TextRole::Body);
        let Some((x, y, w, h)) = surface_rect(bounds) else {
            return;
        };
        if w == 0 || h == 0 {
            return;
        }
        let palette = theme.palette();
        let border = plate_border(theme, scale);
        let radius = scale
            .scale_length(theme.metrics().popup_corner_radius)
            .min(w / 2)
            .min(h / 2);
        paint_plate(
            surface,
            (x, y, w, h),
            &PlateStyle {
                radius,
                border,
                plate: Color::from(palette.surface_raised),
                rim: Color::from(palette.rim),
                focused: false,
                ring: Color::from(palette.rim_active),
            },
        );
        let Some((ix, iy, iw, ih)) = Self::inner(bounds, scale, theme) else {
            return;
        };
        let pad = scale.scale_length(theme.metrics().control_inset).max(1);
        let content_left = ix.saturating_add(pad);
        let content_w = iw.saturating_sub(pad.saturating_mul(2));
        if content_w == 0 {
            return;
        }

        // Title, then message.
        let title_y = to_i32(iy) + to_i32(pad);
        let fitted = font.truncate_to_width(&self.title, content_w);
        font.draw_text(
            surface,
            to_i32(content_left),
            title_y,
            fitted,
            foreground(theme, crate::state::ControlDisposition::Interactive),
        );
        if let Some(message) = &self.message {
            let message_y = title_y + to_i32(font.line_height()) + to_i32(pad) / 2;
            let fitted = font.truncate_to_width(message, content_w);
            font.draw_text(
                surface,
                to_i32(content_left),
                message_y,
                fitted,
                Color::from(palette.on_surface_muted),
            );
        }

        // The action row, and the inline reason just above it.
        let rects = action_row_rects(&self.actions, (ix, iy, iw, ih), scale, theme, font);
        let action_top = rects.iter().filter(|r| r.height > 0).map(Rect::top).min();
        if let Some(reason) = &self.reason {
            let reason_bottom = action_top.unwrap_or(to_i32(iy + ih));
            let reason_y = reason_bottom - to_i32(font.line_height()) - to_i32(pad) / 2;
            if reason_y > title_y {
                let fitted = font.truncate_to_width(reason, content_w);
                font.draw_text(
                    surface,
                    to_i32(content_left),
                    reason_y,
                    fitted,
                    Color::from(palette.warning),
                );
            }
        }
        for (button, rect) in self.actions.iter().zip(rects) {
            if rect.width > 0 {
                button.render(surface, rect, scale, theme);
            }
        }
    }

    /// The surface-pixel rectangles of the action buttons for `bounds`, in
    /// action order (index `0` first).
    ///
    /// An empty vector when the plate has no drawable interior, and a
    /// zero-width rect for any trailing button that did not fit the action
    /// band. One definition so [`on_pointer`](Self::on_pointer) and an owner
    /// that routes clicks through its own press-point hit-test resolve the
    /// exact same button geometry rather than each re-deriving it.
    #[must_use]
    pub fn action_rects(&self, bounds: Rect, scale: Scale, theme: &Theme) -> Vec<Rect> {
        let font = role_font(theme, scale, TextRole::Body);
        match Self::inner(bounds, scale, theme) {
            Some(inner) => action_row_rects(&self.actions, inner, scale, theme, font),
            None => Vec::new(),
        }
    }

    /// Feed a pointer event; an action that completes a click reports
    /// [`DialogAction::ActionActivated`].
    pub fn on_pointer(
        &mut self,
        event: &InputEvent,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
    ) -> Option<DialogAction> {
        let rects = self.action_rects(bounds, scale, theme);
        let mut action = None;
        for (i, button) in self.actions.iter_mut().enumerate() {
            if let Some(rect) = rects.get(i) {
                if button.on_pointer(event, *rect) == Some(ButtonAction::Activated)
                    && action.is_none()
                {
                    action = Some(DialogAction::ActionActivated { index: i });
                }
            }
        }
        action
    }

    /// Feed a key event; a focused action activated with Space/Enter reports
    /// [`DialogAction::ActionActivated`].
    pub fn on_key(&mut self, key: Key) -> Option<DialogAction> {
        let mut action = None;
        for (i, button) in self.actions.iter_mut().enumerate() {
            if button.on_key(key) == Some(ButtonAction::Activated) && action.is_none() {
                action = Some(DialogAction::ActionActivated { index: i });
            }
        }
        action
    }
}

// --- Tooltip -----------------------------------------------------------

/// A short, anchored affordance hint (spec §11.32).
///
/// A tooltip is a small elevated plate carrying one short line that explains
/// the immediate affordance of the control it is anchored to. It is
/// non-interactive: the owner shows and hides it, positioning it beside its
/// anchor. Its text must stay concise and must never carry secrets or
/// capability tokens (spec §13); that is the caller's responsibility, the
/// tooltip simply draws the string it is given.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Tooltip {
    text: String,
}

impl Tooltip {
    /// A tooltip carrying the given short text.
    #[must_use]
    pub fn new(text: impl Into<String>) -> Self {
        Self { text: text.into() }
    }

    /// The tooltip's text.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// The tooltip's preferred `(width, height)` in surface pixels, so the
    /// owner can size the popup surface it anchors the tooltip in.
    #[must_use]
    pub fn preferred_size(&self, scale: Scale, theme: &Theme) -> (u32, u32) {
        let font = role_font(theme, scale, TextRole::Body);
        let border = plate_border(theme, scale);
        let pad = scale.scale_length(theme.metrics().control_inset).max(1);
        let margin = border.saturating_add(pad);
        let w = font
            .text_width(&self.text)
            .saturating_add(margin.saturating_mul(2))
            .max(1);
        let h = font
            .line_height()
            .saturating_add(margin.saturating_mul(2))
            .max(1);
        (w, h)
    }

    /// Paint the tooltip into `surface` at `bounds` for the active theme.
    pub fn render(&self, surface: &mut Surface, bounds: Rect, scale: Scale, theme: &Theme) {
        let font = role_font(theme, scale, TextRole::Body);
        let Some((x, y, w, h)) = surface_rect(bounds) else {
            return;
        };
        if w == 0 || h == 0 {
            return;
        }
        let palette = theme.palette();
        let border = plate_border(theme, scale);
        let radius = scale
            .scale_length(theme.metrics().popup_corner_radius)
            .min(w / 2)
            .min(h / 2);
        paint_plate(
            surface,
            (x, y, w, h),
            &PlateStyle {
                radius,
                border,
                plate: Color::from(palette.surface_raised),
                rim: Color::from(palette.rim),
                focused: false,
                ring: Color::from(palette.rim_active),
            },
        );
        let pad = scale.scale_length(theme.metrics().control_inset).max(1);
        let margin = border.saturating_add(pad);
        let text_w = w.saturating_sub(margin.saturating_mul(2));
        if text_w > 0 {
            let fitted = font.truncate_to_width(&self.text, text_w);
            font.draw_text(
                surface,
                to_i32(x + margin),
                to_i32(y + margin),
                fitted,
                foreground(theme, crate::state::ControlDisposition::Interactive),
            );
        }
    }
}

// --- HelpTip -----------------------------------------------------------

/// The outcome of feeding input to a [`HelpTip`].
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum HelpTipAction {
    /// The one safe next-step action was activated; the owner performs it.
    NextStep,
}

/// An explanation of why an action is unavailable or recommended (spec §11.32).
///
/// A help tip is an elevated plate carrying one reason line and, optionally, one
/// safe next-step [`Button`]. It is the surface that explains a capability
/// denial or a recommendation in concise, user-facing terms — never a secret or
/// a capability token (spec §13). Its reason takes a warning tone; its role
/// tints the reason toward recommendation or denial. It routes input to the
/// next-step action and reports [`HelpTipAction`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HelpTip {
    reason: String,
    role: ControlRole,
    step: Option<Button>,
}

impl HelpTip {
    /// A help tip carrying the given reason and no next step.
    #[must_use]
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
            role: ControlRole::Neutral,
            step: None,
        }
    }

    /// This help tip with a non-default role (drives the reason's tone).
    #[must_use]
    pub fn with_role(mut self, role: ControlRole) -> Self {
        self.role = role;
        self
    }

    /// This help tip with one safe next-step action.
    #[must_use]
    pub fn with_step(mut self, step: Button) -> Self {
        self.step = Some(step);
        self
    }

    /// The help tip's reason text.
    #[must_use]
    pub fn reason(&self) -> &str {
        &self.reason
    }

    /// The help tip's next-step action, if any.
    #[must_use]
    pub fn step(&self) -> Option<&Button> {
        self.step.as_ref()
    }

    /// The reason's tint: a recommendation reads accent, a denial reads denied,
    /// otherwise a caution warning tone.
    fn reason_color(&self, theme: &Theme) -> Color {
        let palette = theme.palette();
        Color::from(match self.role {
            ControlRole::Recommended | ControlRole::Primary => palette.accent,
            ControlRole::Destructive => palette.danger,
            _ => palette.warning,
        })
    }

    /// The help tip's preferred `(width, height)` in surface pixels.
    #[must_use]
    pub fn preferred_size(&self, scale: Scale, theme: &Theme) -> (u32, u32) {
        let font = role_font(theme, scale, TextRole::Body);
        let border = plate_border(theme, scale);
        let pad = scale.scale_length(theme.metrics().control_inset).max(1);
        let margin = border.saturating_add(pad);
        let mut text_w = font.text_width(&self.reason);
        let step_h = self
            .step
            .as_ref()
            .map_or(0, |_| text_plate_height(theme, scale, TextRole::Body) + pad);
        if let Some(step) = &self.step {
            text_w = text_w.max(button_width(step, scale, theme, font));
        }
        let w = text_w.saturating_add(margin.saturating_mul(2)).max(1);
        let h = font
            .line_height()
            .saturating_add(step_h)
            .saturating_add(margin.saturating_mul(2))
            .max(1);
        (w, h)
    }

    /// The inner content rectangle (inside the rim) as surface pixels.
    fn inner(bounds: Rect, scale: Scale, theme: &Theme) -> Option<(u32, u32, u32, u32)> {
        let (x, y, w, h) = surface_rect(bounds)?;
        inset(x, y, w, h, plate_border(theme, scale))
    }

    /// The next-step button rectangle within `bounds`, shared by rendering and
    /// pointer routing so the two never disagree.
    fn step_rect(
        &self,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
        font: BitmapFont,
    ) -> Option<Rect> {
        let step = self.step.as_ref()?;
        let (ix, iy, iw, ih) = Self::inner(bounds, scale, theme)?;
        let pad = scale.scale_length(theme.metrics().control_inset).max(1);
        let bh = text_plate_height(theme, scale, TextRole::Body);
        if ih <= bh.saturating_add(pad) {
            return None;
        }
        let w = button_width(step, scale, theme, font)
            .min(iw.saturating_sub(pad.saturating_mul(2)))
            .max(1);
        Some(Rect::new(to_i32(ix + pad), to_i32(iy + ih - bh), w, bh))
    }

    /// Paint the help tip into `surface` at `bounds` for the active theme.
    pub fn render(&self, surface: &mut Surface, bounds: Rect, scale: Scale, theme: &Theme) {
        let font = role_font(theme, scale, TextRole::Body);
        let Some((x, y, w, h)) = surface_rect(bounds) else {
            return;
        };
        if w == 0 || h == 0 {
            return;
        }
        let palette = theme.palette();
        let border = plate_border(theme, scale);
        let radius = scale
            .scale_length(theme.metrics().popup_corner_radius)
            .min(w / 2)
            .min(h / 2);
        paint_plate(
            surface,
            (x, y, w, h),
            &PlateStyle {
                radius,
                border,
                plate: Color::from(palette.surface_raised),
                rim: Color::from(palette.rim),
                focused: false,
                ring: Color::from(palette.rim_active),
            },
        );
        let Some((ix, iy, iw, _)) = Self::inner(bounds, scale, theme) else {
            return;
        };
        let pad = scale.scale_length(theme.metrics().control_inset).max(1);
        let text_w = iw.saturating_sub(pad.saturating_mul(2));
        if text_w > 0 {
            let fitted = font.truncate_to_width(&self.reason, text_w);
            font.draw_text(
                surface,
                to_i32(ix + pad),
                to_i32(iy + pad),
                fitted,
                self.reason_color(theme),
            );
        }
        if let (Some(step), Some(rect)) = (&self.step, self.step_rect(bounds, scale, theme, font)) {
            step.render(surface, rect, scale, theme);
        }
    }

    /// Feed a pointer event; the next-step action completing a click reports
    /// [`HelpTipAction::NextStep`].
    pub fn on_pointer(
        &mut self,
        event: &InputEvent,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
    ) -> Option<HelpTipAction> {
        let font = role_font(theme, scale, TextRole::Body);
        let rect = self.step_rect(bounds, scale, theme, font)?;
        let step = self.step.as_mut()?;
        (step.on_pointer(event, rect) == Some(ButtonAction::Activated))
            .then_some(HelpTipAction::NextStep)
    }

    /// Feed a key event; a focused next-step action activated with Space/Enter
    /// reports [`HelpTipAction::NextStep`].
    pub fn on_key(&mut self, key: Key) -> Option<HelpTipAction> {
        let step = self.step.as_mut()?;
        (step.on_key(key) == Some(ButtonAction::Activated)).then_some(HelpTipAction::NextStep)
    }
}
