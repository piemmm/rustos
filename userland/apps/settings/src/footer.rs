//! The pane's action band: what a staged pane offers beneath its column,
//! and what the last attempt came to.
//!
//! An immediate pane has none — its effect is its feedback, and a stale
//! Apply button is a trap. A staged pane has exactly this one: a line
//! saying where the change stands, and the two commands that resolve it.

use alloc::string::String;
use alloc::vec::Vec;

use tairix_controls::{Button, ButtonAction, ControlRole, ControlState, FocusState};
use tairix_font::BitmapFont;
use tairix_geometry::{to_i32, Point, Rect, Region, Scale};
use tairix_input::{InputEvent, Key, NamedKey};
use tairix_raster::{Color, Surface};
use tairix_theme::{TextRole, Theme};

/// Which command the band reported.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) enum FooterAction {
    /// Make the staged change durable.
    Apply,
    /// Put the working copy back to what is in effect.
    Revert,
}

/// What the band says about the change it is offering.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Standing {
    /// Nothing differs from what is in effect.
    Unchanged,
    /// Rows differ and have not been applied.
    Changed(usize),
    /// The last apply was made.
    Applied,
    /// The last apply was refused, and why.
    Refused(String),
}

impl Standing {
    /// The line the band shows.
    fn line(&self) -> String {
        match self {
            Self::Unchanged => String::from("No changes"),
            Self::Changed(1) => String::from("1 change not applied"),
            Self::Changed(n) => alloc::format!("{n} changes not applied"),
            Self::Applied => String::from("Applied"),
            Self::Refused(reason) => reason.clone(),
        }
    }

    /// Whether the band's commands can act on anything.
    const fn actionable(&self) -> bool {
        matches!(self, Self::Changed(_) | Self::Refused(_))
    }
}

/// Index of the reverting command; leading, so the recommended one is
/// trailing exactly as it is in a dialog's action band.
const REVERT: usize = 0;
/// Index of the applying command.
const APPLY: usize = 1;

/// The action band beneath a staged pane's column.
pub(crate) struct Footer {
    buttons: Vec<Button>,
    standing: Standing,
    /// Which command holds the keyboard, or `None` when the band does not.
    focus: Option<usize>,
    /// Where the pointer last was, so a press resolves against the command
    /// it was actually over.
    pointer: Point,
}

impl Footer {
    /// A band offering `apply` under its own label, with nothing staged.
    pub(crate) fn new(apply: &str) -> Self {
        Self {
            buttons: alloc::vec![
                Button::labelled("Revert"),
                Button::new(
                    tairix_controls::ButtonContent::Label(String::from(apply)),
                    ControlRole::Recommended,
                ),
            ],
            standing: Standing::Unchanged,
            focus: None,
            pointer: Point::ORIGIN,
        }
    }

    /// Say `standing`, and enable or disable the commands to match.
    pub(crate) fn state(&mut self, standing: Standing) {
        let enabled = standing.actionable();
        self.standing = standing;
        for (index, button) in self.buttons.iter_mut().enumerate() {
            let mut state = ControlState {
                enabled,
                ..button.state()
            };
            state.focus = if self.focus == Some(index) {
                FocusState::FOCUSED
            } else {
                FocusState::default()
            };
            button.set_state(state);
        }
    }

    /// Put the keyboard on the applying command, or take it off the band.
    pub(crate) fn set_focused(&mut self, focused: bool) {
        self.focus = focused.then_some(APPLY);
        self.state(self.standing.clone());
    }

    /// The height the band needs.
    pub(crate) fn measured_height(scale: Scale, theme: &Theme) -> u32 {
        let gap = scale.scale_length(theme.metrics().control_gap).max(1);
        Button::height(scale, theme).saturating_add(gap.saturating_mul(2))
    }

    /// Where each command is drawn in `bounds`, trailing-aligned in band
    /// order.
    pub(crate) fn command_rects(&self, bounds: Rect, scale: Scale, theme: &Theme) -> Vec<Rect> {
        let gap = scale.scale_length(theme.metrics().control_gap).max(1);
        let height = Button::height(scale, theme);
        let top = bounds
            .top()
            .saturating_add(to_i32(bounds.height.saturating_sub(height) / 2));
        let mut widths: Vec<u32> = self
            .buttons
            .iter()
            .map(|button| button.measured_width(scale, theme))
            .collect();
        // Trailing to leading, then reversed, so the recommended command
        // keeps the trailing edge however wide the others turn out to be.
        let mut right = bounds.right().saturating_sub(to_i32(gap));
        let mut rects = Vec::with_capacity(widths.len());
        while let Some(width) = widths.pop() {
            let left = right.saturating_sub(to_i32(width));
            rects.push(Rect::new(left, top, width, height));
            right = left.saturating_sub(to_i32(gap));
        }
        rects.reverse();
        rects
    }

    /// Paint the band.
    pub(crate) fn render(&self, surface: &mut Surface, bounds: Rect, scale: Scale, theme: &Theme) {
        let palette = theme.palette();
        surface.fill_rect(
            u32::try_from(bounds.left()).unwrap_or(0),
            u32::try_from(bounds.top()).unwrap_or(0),
            bounds.width,
            bounds.height,
            Color::from(palette.surface),
        );
        let rects = self.command_rects(bounds, scale, theme);
        for (button, rect) in self.buttons.iter().zip(&rects) {
            button.render(surface, *rect, scale, theme);
        }
        let gap = scale.scale_length(theme.metrics().control_gap).max(1);
        let font = BitmapFont::for_role(theme.fonts(), TextRole::Caption, scale);
        let limit = rects.first().map_or(bounds.right(), |rect| {
            rect.left().saturating_sub(to_i32(gap))
        });
        let avail = u32::try_from(limit.saturating_sub(bounds.left().saturating_add(to_i32(gap))))
            .unwrap_or(0);
        let line = self.standing.line();
        let fitted = font.truncate_to_width(&line, avail);
        let baseline = bounds
            .top()
            .saturating_add(to_i32(bounds.height.saturating_sub(font.line_height()) / 2));
        font.draw_text(
            surface,
            bounds.left().saturating_add(to_i32(gap)),
            baseline,
            fitted,
            Color::from(match self.standing {
                Standing::Refused(_) => palette.danger,
                _ => palette.on_surface_muted,
            }),
        );
    }

    /// Route one pointer event.
    pub(crate) fn on_pointer(
        &mut self,
        event: &InputEvent,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
        damage: &mut Region,
    ) -> Option<FooterAction> {
        if let InputEvent::PointerMoved { to } = event {
            self.pointer = *to;
        }
        let rects = self.command_rects(bounds, scale, theme);
        let mut acted = None;
        for (index, (button, rect)) in self.buttons.iter_mut().zip(&rects).enumerate() {
            if button.on_pointer(event, *rect, damage) == Some(ButtonAction::Activated) {
                acted = action_of(index);
            }
        }
        acted
    }

    /// Route one key press.
    pub(crate) fn on_key(&mut self, key: Key) -> Option<FooterAction> {
        if key == Key::Named(NamedKey::Left) || key == Key::Named(NamedKey::Right) {
            let step = usize::from(key == Key::Named(NamedKey::Right));
            self.focus = Some(if step == 1 { APPLY } else { REVERT });
            self.state(self.standing.clone());
            return None;
        }
        let mut acted = None;
        for (index, button) in self.buttons.iter_mut().enumerate() {
            if button.on_key(key) == Some(ButtonAction::Activated) {
                acted = action_of(index);
            }
        }
        acted
    }
}

/// The command at `index`, or `None` for an index this band did not build.
const fn action_of(index: usize) -> Option<FooterAction> {
    match index {
        REVERT => Some(FooterAction::Revert),
        APPLY => Some(FooterAction::Apply),
        _ => None,
    }
}
