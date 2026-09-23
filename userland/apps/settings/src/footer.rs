//! The pane's action band: what a pane offers beneath its column, and what
//! the last attempt came to.
//!
//! An immediate pane has none — its effect is its feedback, and a stale
//! Apply button is a trap. Every other pane has one of exactly two bands: a
//! staged pane's Revert and Apply over its working copy, and the single
//! named command a pane offers when it has no working copy at all — the
//! application that owns its subject, or the authenticated reading its rows
//! cannot exist without. A band offers Revert only where there is something
//! to revert.

use alloc::borrow::Cow;
use alloc::string::String;
use alloc::vec::Vec;

use tairix_controls::{paint_run, Button, ButtonAction, ControlRole, ControlState, FocusState};
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
    /// The band's one command is there to be used and nothing has been
    /// attempted, so there is no change to count and nothing to say.
    Offered,
    /// Nothing differs from what is in effect.
    Unchanged,
    /// Rows differ and have not been applied.
    Changed(usize),
    /// Rows hold values their store would refuse, so there is nothing to
    /// apply until they are corrected or reverted.
    Refusing(usize),
    /// The last apply was made.
    Applied,
    /// The last attempt was refused, and why.
    Refused(String),
}

impl Standing {
    /// The line the band shows, allocated only where it carries a count.
    fn line(&self) -> Cow<'_, str> {
        match self {
            Self::Offered => Cow::Borrowed(""),
            Self::Unchanged => Cow::Borrowed("No changes"),
            Self::Changed(1) => Cow::Borrowed("1 change not applied"),
            Self::Changed(n) => Cow::Owned(alloc::format!("{n} changes not applied")),
            Self::Refusing(1) => Cow::Borrowed("1 value this cannot be saved with"),
            Self::Refusing(n) => Cow::Owned(alloc::format!("{n} values this cannot be saved with")),
            Self::Applied => Cow::Borrowed("Applied"),
            Self::Refused(reason) => Cow::Borrowed(reason),
        }
    }

    /// Whether the band's acting command can act on anything.
    ///
    /// A refused value is not applied around: the change goes whole or not
    /// at all, so the acting command waits until the rows agree.
    const fn can_act(&self) -> bool {
        matches!(self, Self::Offered | Self::Changed(_) | Self::Refused(_))
    }

    /// Whether the band's reverting command can act on anything, which a
    /// refused value is exactly the case for.
    const fn can_revert(&self) -> bool {
        matches!(
            self,
            Self::Changed(_) | Self::Refusing(_) | Self::Refused(_)
        )
    }

    /// Whether the line is a refusal, which is drawn in the danger role.
    const fn is_refusal(&self) -> bool {
        matches!(self, Self::Refused(_) | Self::Refusing(_))
    }
}

/// The label every staged pane's acting command carries.
const APPLY_LABEL: &str = "Apply";

/// The action band beneath a pane's column.
pub(crate) struct Footer {
    buttons: Vec<Button>,
    standing: Standing,
    /// Whether the leading command reverts a working copy, which a band
    /// offering one command alone has none of.
    reverts: bool,
    /// Which command holds the keyboard, or `None` when the band does not.
    focus: Option<usize>,
    /// Where the pointer last was, so a press resolves against the command
    /// it was actually over.
    pointer: Point,
}

impl Footer {
    /// A staged pane's band: Revert, then Apply.
    pub(crate) fn staged() -> Self {
        Self::of(
            alloc::vec![
                Button::labelled("Revert"),
                Button::new(
                    tairix_controls::ButtonContent::Label(String::from(APPLY_LABEL)),
                    ControlRole::Recommended,
                ),
            ],
            true,
            Standing::Unchanged,
        )
    }

    /// A band offering the one named command a pane has instead of a
    /// working copy.
    pub(crate) fn command(label: &'static str) -> Self {
        Self::of(
            alloc::vec![Button::new(
                tairix_controls::ButtonContent::Label(String::from(label)),
                ControlRole::Recommended,
            )],
            false,
            Standing::Offered,
        )
    }

    /// A band over `buttons`, stating `standing`.
    fn of(buttons: Vec<Button>, reverts: bool, standing: Standing) -> Self {
        let mut band = Self {
            buttons,
            standing: Standing::Unchanged,
            reverts,
            focus: None,
            pointer: Point::ORIGIN,
        };
        band.state(standing);
        band
    }

    /// Say that whatever the band last asked for has settled.
    ///
    /// A staged band has nothing left to apply and says so; a band whose
    /// one command is a reading or an application stays offered, because
    /// having used it once is no reason it cannot be used again.
    pub(crate) fn settled(&mut self) {
        self.state(if self.reverts {
            Standing::Applied
        } else {
            Standing::Offered
        });
    }

    /// Say `standing`, and enable or disable the commands to match.
    pub(crate) fn state(&mut self, standing: Standing) {
        let acts = standing.can_act();
        let reverts = self.reverts && standing.can_revert();
        self.standing = standing;
        let last = self.buttons.len().saturating_sub(1);
        for (index, button) in self.buttons.iter_mut().enumerate() {
            let mut state = ControlState {
                enabled: if index == last { acts } else { reverts },
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

    /// Put the keyboard on the acting command, or take it off the band.
    pub(crate) fn set_focused(&mut self, focused: bool) {
        self.focus = focused.then_some(self.acting());
        self.state(self.standing.clone());
    }

    /// Which command acts: always the trailing one, so the recommended
    /// command keeps the trailing edge exactly as it does in a dialog.
    const fn acting(&self) -> usize {
        self.buttons.len().saturating_sub(1)
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
        let baseline = bounds
            .top()
            .saturating_add(to_i32(bounds.height.saturating_sub(font.line_height()) / 2));
        paint_run(
            surface,
            font,
            font.elide_to_width(&line, avail),
            (bounds.left().saturating_add(to_i32(gap)), baseline),
            Color::from(if self.standing.is_refusal() {
                palette.danger
            } else {
                palette.on_surface_muted
            }),
            None,
        );
    }

    /// What the band is saying, for a test that asks what a reader would
    /// read there.
    #[cfg(test)]
    pub(crate) fn line(&self) -> Cow<'_, str> {
        self.standing.line()
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
        let acting = self.acting();
        let mut acted = None;
        for (index, (button, rect)) in self.buttons.iter_mut().zip(&rects).enumerate() {
            if button.on_pointer(event, *rect, damage) == Some(ButtonAction::Activated) {
                acted = Some(action_of(index, acting));
            }
        }
        acted
    }

    /// Route one key press.
    pub(crate) fn on_key(&mut self, key: Key) -> Option<FooterAction> {
        if key == Key::Named(NamedKey::Left) || key == Key::Named(NamedKey::Right) {
            self.focus = Some(if key == Key::Named(NamedKey::Right) {
                self.acting()
            } else {
                0
            });
            self.state(self.standing.clone());
            return None;
        }
        let acting = self.acting();
        let mut acted = None;
        for (index, button) in self.buttons.iter_mut().enumerate() {
            if button.on_key(key) == Some(ButtonAction::Activated) {
                acted = Some(action_of(index, acting));
            }
        }
        acted
    }
}

/// The command at `index`: the trailing one acts, and a leading one is the
/// revert a staged band carries.
const fn action_of(index: usize, acting: usize) -> FooterAction {
    if index == acting {
        FooterAction::Apply
    } else {
        FooterAction::Revert
    }
}

#[cfg(test)]
mod tests {
    use alloc::string::String;

    use tairix_controls::testkit::marks_elision;
    use tairix_geometry::{Rect, Scale};
    use tairix_raster::Surface;

    use super::{Footer, Standing};

    /// A refusal too long for the room beside the band's commands is elided
    /// with the shared mark rather than cut where the room ran out: it is the
    /// store's text, and a reader must be told there is more of it.
    #[test]
    fn a_refusal_too_long_for_the_band_is_elided_with_the_mark() {
        let theme = crate::test_support::theme();
        let bounds = Rect::new(0, 0, 420, Footer::measured_height(Scale::ONE, &theme));
        assert!(marks_elision(|reason| {
            let mut footer = Footer::staged();
            footer.state(Standing::Refused(String::from(reason)));
            let mut surface = Surface::new(bounds.width, bounds.height).expect("surface");
            footer.render(&mut surface, bounds, Scale::ONE, &theme);
            surface
        }));
    }
}
