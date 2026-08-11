//! The choice-entry control: [`ComboBox`] (spec §11.9).
//!
//! A combo box is a field plus a disclosure action: a quiet Alloy Plate showing
//! the currently selected choice with a trailing down chevron, expanding to a
//! [`Menu`] of choices. It composes the text-field focus model (a plate, a
//! focus ring, the spec §13 authority treatment) and the [`Menu`] model for the
//! expanded list rather than re-deriving either: the popup
//! *is* a [`Menu`] built from the choices, so the menu's keyboard navigation,
//! hover, and rendering are reused unchanged. Selection belongs to the choice
//! list, never to string parsing inside the control. Every activation is a
//! typed [`ComboAction`]; the control enforces no authority.

use alloc::string::String;
use alloc::vec::Vec;

use tairix_geometry::{Point, Rect, Region, Scale};
use tairix_input::{InputEvent, Key, NamedKey, PointerButton};
use tairix_raster::{Color, Surface};
use tairix_theme::{TextRole, Theme};

use crate::damage;
use crate::menu::{Menu, MenuAction, MenuItem};
use crate::paint::{
    paint_bead, paint_chevron, paint_plate, plate_border, resolve_bead, resolve_frame, role_font,
    surface_rect, to_i32, ChevronDir, PlateStyle,
};
use crate::state::{ControlRole, ControlState, RenderInvariant, SelectionState};

/// The outcome of feeding input to a [`ComboBox`].
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ComboAction {
    /// The choice at `index` was selected; the list has collapsed.
    Selected {
        /// The zero-based index of the selected choice.
        index: usize,
    },
    /// The choice list has expanded and the owner should show the popup.
    Opened,
    /// The choice list has collapsed without a new selection.
    Closed,
}

/// A field plus a disclosure over a choice list (spec §11.9).
///
/// The collapsed control shows the selected choice (or a placeholder) with a
/// disclosure chevron; expanding shows a [`Menu`] of the choices with the
/// selected one highlighted. The owner renders the collapsed field with
/// [`ComboBox::render`] and, while [`ComboBox::is_expanded`] is true, renders
/// the popup with [`ComboBox::render_popup`] sized by [`ComboBox::popup_size`].
///
/// Equal combo boxes draw the same pixels, so a host may use `==` as its
/// repaint gate: the choices, selection, placeholder, expanded flag, popup
/// menu, role, and visible state compare. The pointer coordinate and the press
/// latch do not — no render path reads either.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ComboBox {
    choices: Vec<String>,
    selected: Option<usize>,
    placeholder: String,
    expanded: bool,
    menu: Menu,
    role: ControlRole,
    state: ControlState,
    /// The last pointer position — hit-testing input, never drawn.
    pointer: RenderInvariant<Point>,
    /// Whether a primary press landed on the field and has not yet been
    /// released; the press *look* lives in `state.pointer`.
    armed: RenderInvariant<bool>,
}

impl ComboBox {
    /// A combo box over the given choices, with nothing selected.
    #[must_use]
    pub fn new(choices: Vec<String>) -> Self {
        let menu = Menu::new(choices.iter().map(MenuItem::new).collect());
        Self {
            choices,
            selected: None,
            placeholder: String::new(),
            expanded: false,
            menu,
            role: ControlRole::Neutral,
            state: ControlState::idle(),
            pointer: RenderInvariant::new(Point::ORIGIN),
            armed: RenderInvariant::new(false),
        }
    }

    /// This combo box with the choice at `index` initially selected.
    #[must_use]
    pub fn with_selected(mut self, index: usize) -> Self {
        self.select_internal(index);
        self
    }

    /// This combo box with placeholder text shown when nothing is selected.
    #[must_use]
    pub fn with_placeholder(mut self, placeholder: impl Into<String>) -> Self {
        self.placeholder = placeholder.into();
        self
    }

    /// This combo box with a non-default role.
    #[must_use]
    pub fn with_role(mut self, role: ControlRole) -> Self {
        self.role = role;
        self
    }

    /// The choices.
    #[must_use]
    pub fn choices(&self) -> &[String] {
        &self.choices
    }

    /// The index of the selected choice, if any.
    #[must_use]
    pub fn selected(&self) -> Option<usize> {
        self.selected
    }

    /// The text of the selected choice, if any.
    #[must_use]
    pub fn selected_text(&self) -> Option<&str> {
        self.selected
            .and_then(|i| self.choices.get(i))
            .map(String::as_str)
    }

    /// Whether the choice list is expanded.
    #[must_use]
    pub fn is_expanded(&self) -> bool {
        self.expanded
    }

    /// The control's composed state.
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

    /// The popup [`Menu`], for the owner to inspect (never to mutate its
    /// selection directly — use [`ComboBox::set_selected`]).
    #[must_use]
    pub fn menu(&self) -> &Menu {
        &self.menu
    }

    /// Set the selected choice (e.g. after the owner commits a change); an
    /// out-of-range index selects nothing (fail closed).
    pub fn set_selected(&mut self, index: usize) {
        self.select_internal(index);
    }

    /// Record a selection and mirror it into the menu (highlight + selection
    /// state), so the popup opens on the current choice.
    ///
    /// Answers whether the selection moved, which is what decides whether the
    /// label the field shows — and so the field — changed. Re-selecting the
    /// current choice moves nothing: the menu's mirror is a function of this
    /// selection and is already in that state.
    fn select_internal(&mut self, index: usize) -> bool {
        if index >= self.choices.len() || self.selected == Some(index) {
            return false;
        }
        self.selected = Some(index);
        for (i, item) in self.menu.items_mut().iter_mut().enumerate() {
            let mut s = item.state();
            s.selection = if i == index {
                SelectionState::Selected
            } else {
                SelectionState::Unselected
            };
            item.set_state(s);
        }
        self.menu.set_current(Some(index));
        true
    }

    /// Expand the list, highlighting the selected (or first) choice, reporting
    /// the `popup` rectangle that appears.
    fn open(&mut self, popup: Rect, damage: &mut Region) -> Option<ComboAction> {
        if self.expanded || self.choices.is_empty() {
            return None;
        }
        damage::set(&mut self.expanded, true, popup, damage);
        self.menu.set_current(Some(self.selected.unwrap_or(0)));
        Some(ComboAction::Opened)
    }

    /// Collapse the list, reporting the `popup` rectangle it vacates — what the
    /// popup covered is drawn again by whatever lies beneath it.
    fn close(&mut self, popup: Rect, damage: &mut Region) -> Option<ComboAction> {
        if !self.expanded {
            return None;
        }
        damage::set(&mut self.expanded, false, popup, damage);
        *self.armed = false;
        Some(ComboAction::Closed)
    }

    /// Take the choice at `index` and collapse, reporting the `field` whose
    /// label it changes and the `popup` it vacates.
    ///
    /// The collapse is [`close`](Self::close)'s, so there is one definition of
    /// what collapsing does; the action it would have reported is superseded by
    /// the selection.
    fn take_choice(
        &mut self,
        index: usize,
        field: Rect,
        popup: Rect,
        damage: &mut Region,
    ) -> ComboAction {
        if self.select_internal(index) {
            damage.add(field);
        }
        self.close(popup, damage);
        ComboAction::Selected { index }
    }

    /// The popup surface size for the active theme: as wide as the widest
    /// choice row (and never narrower than `field_width`) and tall enough for
    /// every row, so the owner can allocate the popup exactly.
    #[must_use]
    pub fn popup_size(&self, field_width: u32, scale: Scale, theme: &Theme) -> (u32, u32) {
        let w = self.menu.preferred_width(scale, theme).max(field_width);
        let h = self.menu.preferred_height(scale, theme);
        (w, h)
    }

    /// The chevron square width for a field of the given inner height.
    fn chevron_side(inner_h: u32) -> u32 {
        inner_h
    }

    /// Paint the collapsed field into `surface` at `bounds` for the theme.
    pub fn render(&self, surface: &mut Surface, bounds: Rect, scale: Scale, theme: &Theme) {
        let font = role_font(theme, scale, TextRole::Body);
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
        let frame = resolve_frame(theme, self.role, self.state);

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

        let pad = scale.scale_length(metrics.control_inset).max(1);
        let inner_h = h.saturating_sub(border.saturating_mul(2));
        let chevron_w = Self::chevron_side(inner_h);
        let left = x.saturating_add(border).saturating_add(pad);
        let right = x
            .saturating_add(w)
            .saturating_sub(border)
            .saturating_sub(chevron_w);

        // The selected choice, or the placeholder when nothing is selected.
        if right > left {
            let (text, color) = match self.selected_text() {
                Some(sel) => (sel, frame.label),
                None => (
                    self.placeholder.as_str(),
                    Color::from(palette.on_surface_muted),
                ),
            };
            let budget = right - left;
            let fitted = font.truncate_to_width(text, budget);
            let glyph_h = font.glyph_height();
            let text_y = to_i32(y) + (to_i32(h) - to_i32(glyph_h)).max(0) / 2;
            font.draw_text(surface, to_i32(left), text_y, fitted, color);
        }

        // The disclosure chevron at the trailing edge.
        let cx = x
            .saturating_add(w)
            .saturating_sub(border)
            .saturating_sub(chevron_w);
        paint_chevron(
            surface,
            Rect::new(to_i32(cx), to_i32(y + border), chevron_w, inner_h),
            ChevronDir::Down,
            frame.label,
        );

        // The Signal Bead (denied lock / recovery / complete), if any, over the
        // chevron corner so an authority denial is never hidden.
        if let Some((color, shape)) = resolve_bead(theme, self.state) {
            let size = scale.scale_length(metrics.bead_size).max(3).min(inner_h);
            paint_bead(
                surface,
                x + w - border - size,
                y + border,
                size,
                color,
                shape,
            );
        }
    }

    /// Paint the expanded popup menu into `surface` at `bounds` for the theme.
    /// The owner only calls this while [`ComboBox::is_expanded`] is true.
    pub fn render_popup(&self, surface: &mut Surface, bounds: Rect, scale: Scale, theme: &Theme) {
        self.menu.render(surface, bounds, scale, theme);
    }

    /// Feed a pointer event. When collapsed, a primary click on the field
    /// toggles the list open. When expanded, the event is routed to the popup
    /// menu at `popup_bounds`: choosing a row selects it and collapses, and a
    /// primary press outside both the field and the popup collapses the list.
    ///
    /// The two rectangles are reported separately, because they change for
    /// different reasons: the popup when it appears or vacates (and the rows
    /// within it as the highlight moves), the field only when the label it
    /// shows changes.
    pub fn on_pointer(
        &mut self,
        event: &InputEvent,
        field_bounds: Rect,
        popup_bounds: Rect,
        scale: Scale,
        theme: &Theme,
        damage: &mut Region,
    ) -> Option<ComboAction> {
        if let InputEvent::PointerMoved { to } = event {
            *self.pointer = *to;
        }
        if self.expanded {
            match self
                .menu
                .on_pointer(event, popup_bounds, scale, theme, damage)
            {
                Some(MenuAction::Activated { index } | MenuAction::OpenSubmenu { index }) => {
                    return Some(self.take_choice(index, field_bounds, popup_bounds, damage));
                }
                Some(MenuAction::Dismissed) => return self.close(popup_bounds, damage),
                None => {}
            }
            if matches!(
                event,
                InputEvent::PointerPressed {
                    button: PointerButton::Primary
                }
            ) && !popup_bounds.contains(*self.pointer)
                && !field_bounds.contains(*self.pointer)
            {
                return self.close(popup_bounds, damage);
            }
            return None;
        }

        let inside = field_bounds.contains(*self.pointer);
        match event {
            InputEvent::PointerPressed {
                button: PointerButton::Primary,
            } => {
                *self.armed = inside && self.state.is_actionable();
                None
            }
            InputEvent::PointerReleased {
                button: PointerButton::Primary,
            } => {
                let fire = *self.armed && inside && self.state.is_actionable();
                *self.armed = false;
                if fire {
                    self.open(popup_bounds, damage)
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    /// Feed a key event. When expanded, keys drive the popup menu (Up/Down move,
    /// Enter/Space choose, Escape closes). When collapsed and focused, Down /
    /// Up / Enter / Space open the list on the current choice.
    pub fn on_key(
        &mut self,
        key: Key,
        field_bounds: Rect,
        popup_bounds: Rect,
        scale: Scale,
        theme: &Theme,
        damage: &mut Region,
    ) -> Option<ComboAction> {
        if self.expanded {
            return match self.menu.on_key(key, popup_bounds, scale, theme, damage) {
                Some(MenuAction::Activated { index } | MenuAction::OpenSubmenu { index }) => {
                    Some(self.take_choice(index, field_bounds, popup_bounds, damage))
                }
                Some(MenuAction::Dismissed) => self.close(popup_bounds, damage),
                None => None,
            };
        }
        if !self.state.focus.focused || !self.state.is_actionable() {
            return None;
        }
        match key {
            Key::Named(NamedKey::Down | NamedKey::Up | NamedKey::Enter) | Key::Char(' ') => {
                self.open(popup_bounds, damage)
            }
            _ => None,
        }
    }
}
