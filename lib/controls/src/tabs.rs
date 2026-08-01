//! The tab strip: [`Tab`] and [`Tabs`] (spec §11.12).
//!
//! A tab strip is a row of equal-width items that select one of several views.
//! The selected tab carries a strong lower seam and reads on the content
//! surface; a loading tab shows a Heat Seam on its lower edge; a modified tab
//! shows a small Signal Bead; and an error tab shows a warning or recovery
//! bead so its state is legible without colour (spec §11.12, §15). The strip
//! owns keyboard navigation (Left/Right move the current tab, Home/End jump to
//! the ends, Enter/Space select it) and pointer hover/click, emitting a typed
//! [`TabsAction`]; it enforces no authority. Every colour,
//! metric, and radius resolves from the active [`Theme`] and [`Scale`].

use alloc::string::String;
use alloc::vec::Vec;

use tairix_font::BitmapFont;
use tairix_geometry::{Point, Rect, Scale};
use tairix_input::{InputEvent, Key, NamedKey, PointerButton};
use tairix_raster::{Color, Surface};
use tairix_theme::Theme;

use crate::paint::{
    draw_outline, heavy_contrast, paint_bead, plate_border, surface_rect, to_i32, BeadShape,
    RenderInvariant,
};
use crate::state::{
    ActivityState, ControlDisposition, ControlState, SelectionState, ValidationState,
};

/// The outcome of feeding input to a [`Tabs`] strip.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum TabsAction {
    /// The tab at `index` was chosen and its view should become active.
    Selected {
        /// The zero-based index of the chosen tab.
        index: usize,
    },
}

/// One tab in a [`Tabs`] strip (spec §11.12).
///
/// A tab's selected/loading/error state is read from its composed
/// [`ControlState`] (selection, activity, validation); the modified flag is a
/// small explicit marker for unsaved work. The tab renders state and never
/// dispatches — selection commits through the owner.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Tab {
    label: String,
    modified: bool,
    state: ControlState,
}

impl Tab {
    /// A neutral, enabled tab with the given label.
    #[must_use]
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            modified: false,
            state: ControlState::idle(),
        }
    }

    /// This tab flagged as having unsaved modifications (draws a Signal Bead).
    #[must_use]
    pub fn with_modified(mut self, modified: bool) -> Self {
        self.modified = modified;
        self
    }

    /// This tab with the given composed state (selection/activity/validation).
    #[must_use]
    pub fn with_state(mut self, state: ControlState) -> Self {
        self.state = state;
        self
    }

    /// The tab's label.
    #[must_use]
    pub fn label(&self) -> &str {
        &self.label
    }

    /// The tab's composed state.
    #[must_use]
    pub fn state(&self) -> ControlState {
        self.state
    }

    /// Replace the tab's composed state (e.g. from a model update).
    pub fn set_state(&mut self, state: ControlState) {
        self.state = state;
    }

    /// Whether the tab is the selected one.
    #[must_use]
    pub fn is_selected(&self) -> bool {
        self.state.selection == SelectionState::Selected
    }

    /// Whether the tab's view is loading (any in-progress activity).
    fn is_loading(&self) -> bool {
        matches!(
            self.state.activity,
            ActivityState::Working | ActivityState::Indeterminate | ActivityState::Progress(_)
        )
    }
}

/// A row of equal-width items selecting one of several views (spec §11.12).
///
/// The strip tracks a *current* tab for keyboard focus (distinct from the
/// *selected* tab, which is the one whose view is shown). Selection commits
/// through the owner via [`TabsAction::Selected`]; the owner then updates the
/// items' [`SelectionState`] (helper [`Tabs::set_selected`]).
///
/// Equal strips draw the same pixels, so a host may use `==` as its repaint
/// gate: the items, the current tab, and whether that focus came from the
/// keyboard all compare. The pointer coordinate and the pressed-tab latch do
/// not — no render path reads either, and the *visible* consequence of a press
/// is the `current` tab the same event sets.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Tabs {
    items: Vec<Tab>,
    current: Option<usize>,
    keyboard_focus: bool,
    /// The last pointer position, mapped to a tab on the next press or
    /// release — hit-testing input, never drawn.
    pointer: RenderInvariant<Point>,
    /// The tab a primary press landed on, held until release so a click that
    /// slides onto another tab does not select it; the pressed tab's *look* is
    /// `current`.
    armed: RenderInvariant<Option<usize>>,
}

impl Tabs {
    /// A tab strip over the given items.
    #[must_use]
    pub fn new(tabs: Vec<Tab>) -> Self {
        Self {
            items: tabs,
            current: None,
            keyboard_focus: false,
            pointer: RenderInvariant::new(Point::ORIGIN),
            armed: RenderInvariant::new(None),
        }
    }

    /// The strip's items.
    #[must_use]
    pub fn tabs(&self) -> &[Tab] {
        &self.items
    }

    /// Mutable access to the strip's items.
    pub fn tabs_mut(&mut self) -> &mut [Tab] {
        &mut self.items
    }

    /// The number of items.
    #[must_use]
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Whether the strip has no items.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// The index of the selected tab, if any.
    #[must_use]
    pub fn selected(&self) -> Option<usize> {
        self.items.iter().position(Tab::is_selected)
    }

    /// Mark `index` as the selected tab and clear the selection from the
    /// others; an out-of-range index selects nothing (fail closed).
    pub fn set_selected(&mut self, index: usize) {
        for (i, tab) in self.items.iter_mut().enumerate() {
            tab.state.selection = if i == index {
                SelectionState::Selected
            } else {
                SelectionState::Unselected
            };
        }
    }

    /// The currently focused tab, if any.
    #[must_use]
    pub fn current(&self) -> Option<usize> {
        self.current
    }

    /// Focus `index` from the keyboard (or clear focus with `None`); an
    /// out-of-range index clears focus (fail closed).
    pub fn set_current(&mut self, index: Option<usize>) {
        self.current = index.filter(|&i| i < self.items.len());
        self.keyboard_focus = self.current.is_some();
    }

    /// The surface rectangle of tab `index` within `bounds`, or `None` if it
    /// collapses. Tabs share the width equally.
    fn tab_rect(&self, index: usize, bounds: Rect) -> Option<(u32, u32, u32, u32)> {
        let (x, y, w, h) = surface_rect(bounds)?;
        let count = u32::try_from(self.items.len()).ok()?;
        if count == 0 || w == 0 || h == 0 || index >= self.items.len() {
            return None;
        }
        let idx = u32::try_from(index).ok()?;
        let each = w / count;
        if each == 0 {
            return None;
        }
        let tx = x + idx * each;
        // The last tab absorbs the rounding remainder so the strip fills width.
        let tw = if index + 1 == self.items.len() {
            w - idx * each
        } else {
            each
        };
        Some((tx, y, tw, h))
    }

    /// The tab index under `point`, if any, for the given bounds.
    #[must_use]
    pub fn tab_at(&self, bounds: Rect, point: Point) -> Option<usize> {
        (0..self.items.len()).find(|&i| {
            self.tab_rect(i, bounds).is_some_and(|(x, y, w, h)| {
                point.x >= to_i32(x)
                    && point.x < to_i32(x + w)
                    && point.y >= to_i32(y)
                    && point.y < to_i32(y + h)
            })
        })
    }

    /// Paint the strip into `surface` at `bounds` for the active theme.
    pub fn render(
        &self,
        surface: &mut Surface,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
        font: BitmapFont,
    ) {
        for (i, tab) in self.items.iter().enumerate() {
            if let Some(rect) = self.tab_rect(i, bounds) {
                let current = self.current == Some(i);
                let focused = current && self.keyboard_focus;
                Self::paint_tab(surface, rect, scale, theme, font, tab, current, focused);
            }
        }
    }

    /// Paint one tab cell.
    #[allow(clippy::too_many_arguments)]
    fn paint_tab(
        surface: &mut Surface,
        rect: (u32, u32, u32, u32),
        scale: Scale,
        theme: &Theme,
        font: BitmapFont,
        tab: &Tab,
        current: bool,
        focused: bool,
    ) {
        let (x, y, w, h) = rect;
        if w == 0 || h == 0 {
            return;
        }
        let palette = theme.palette();
        let metrics = theme.metrics();
        let border = plate_border(theme, scale);
        let selected = tab.is_selected();
        let disposition = tab.state.disposition();

        // Tab plate: the selected tab reads on the content surface; an
        // unselected tab is quieter; a hovered tab lifts. Disabled stays muted.
        let plate = if selected {
            palette.surface
        } else if current {
            palette.surface_raised
        } else {
            palette.surface_pressed
        };
        surface.fill_rect(x, y, w, h, Color::from(plate));

        // The lower seam: a strong accent seam for the selected tab, a Heat
        // Seam for a loading tab (proportional if the fraction is known).
        let seam_h = scale.scale_length(metrics.seam_thickness).max(1).min(h);
        if selected {
            let thick = seam_h
                .saturating_mul(if heavy_contrast(theme) { 2 } else { 1 })
                .min(h);
            surface.fill_rect(x, y + h - thick, w, thick, Color::from(palette.accent));
        } else if tab.is_loading() {
            let seam_w = match tab.state.activity {
                ActivityState::Progress(value) => {
                    u32::try_from(u64::from(w) * u64::from(value.permille()) / 1000).unwrap_or(w)
                }
                _ => w,
            };
            if seam_w > 0 {
                surface.fill_rect(
                    x,
                    y + h - seam_h,
                    seam_w,
                    seam_h,
                    Color::from(palette.accent),
                );
            }
        }

        // The keyboard focus ring, distinct from a hover lift.
        if focused {
            draw_outline(
                surface,
                x,
                y,
                w,
                h,
                border.max(1),
                Color::from(palette.rim_active),
            );
        }

        // The label, centred, with weight conveyed by colour (accent when
        // selected) since the bitmap font has one weight.
        let label_color = match disposition {
            ControlDisposition::DisabledByState => palette.on_surface_muted,
            _ if selected => palette.accent,
            _ => palette.on_surface,
        };
        let pad = scale.scale_length(metrics.control_inset).max(1);
        let bead = Self::tab_bead(theme, tab);
        let bead_w = bead.map_or(0, |_| {
            scale.scale_length(metrics.bead_size).max(3).min(w).min(h) + pad
        });
        let avail = w
            .saturating_sub(border.saturating_add(pad).saturating_mul(2))
            .saturating_sub(bead_w);
        if avail > 0 {
            let fitted = font.truncate_to_width(&tab.label, avail);
            let tw = font.text_width(fitted);
            let cx = to_i32(x) + to_i32(w.saturating_sub(bead_w)) / 2;
            let glyph_h = font.glyph_height();
            let text_y = to_i32(y) + (to_i32(h) - to_i32(glyph_h)).max(0) / 2;
            font.draw_text(
                surface,
                cx - to_i32(tw) / 2,
                text_y,
                fitted,
                Color::from(label_color),
            );
        }

        // The modified / error Signal Bead at the top-trailing corner.
        if let Some((color, shape)) = bead {
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
    }

    /// The Signal Bead a tab shows, if any: an error bead (recovery diamond for
    /// invalid, warning diamond for a caution) takes priority over the modified
    /// dot, so an error is never hidden by an unsaved-work marker.
    fn tab_bead(theme: &Theme, tab: &Tab) -> Option<(Color, BeadShape)> {
        let palette = theme.palette();
        match tab.state.validation {
            ValidationState::Invalid => Some((Color::from(palette.recovery), BeadShape::Diamond)),
            ValidationState::Warning => Some((Color::from(palette.warning), BeadShape::Diamond)),
            _ if tab.modified => Some((Color::from(palette.accent), BeadShape::Check)),
            _ => None,
        }
    }

    /// Select the current tab if it is actionable, reporting the choice.
    fn choose(&self, index: usize) -> Option<TabsAction> {
        let tab = self.items.get(index)?;
        tab.state
            .is_actionable()
            .then_some(TabsAction::Selected { index })
    }

    /// Feed a pointer event; hover focuses a tab and a completed primary click
    /// selects it.
    pub fn on_pointer(&mut self, event: &InputEvent, bounds: Rect) -> Option<TabsAction> {
        if let InputEvent::PointerMoved { to } = event {
            *self.pointer = *to;
        }
        let over = self.tab_at(bounds, *self.pointer);
        match event {
            InputEvent::PointerMoved { .. } => {
                if self.armed.is_none() {
                    self.current = over;
                    self.keyboard_focus = false;
                }
                None
            }
            InputEvent::PointerPressed {
                button: PointerButton::Primary,
            } => {
                *self.armed = over;
                if let Some(i) = over {
                    self.current = Some(i);
                    self.keyboard_focus = false;
                }
                None
            }
            InputEvent::PointerReleased {
                button: PointerButton::Primary,
            } => {
                let armed = self.armed.take();
                match (armed, over) {
                    (Some(a), Some(o)) if a == o => self.choose(o),
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// Feed a key event: Left/Right move the current tab (wrapping), Home/End
    /// jump to the ends, and Enter/Space select the current tab.
    pub fn on_key(&mut self, key: Key) -> Option<TabsAction> {
        if self.items.is_empty() {
            return None;
        }
        let last = self.items.len() - 1;
        match key {
            Key::Named(NamedKey::Right) => {
                let next = match self.current {
                    Some(i) if i < last => i + 1,
                    _ => 0,
                };
                self.current = Some(next);
                self.keyboard_focus = true;
                None
            }
            Key::Named(NamedKey::Left) => {
                let prev = match self.current {
                    Some(0) | None => last,
                    Some(i) => i - 1,
                };
                self.current = Some(prev);
                self.keyboard_focus = true;
                None
            }
            Key::Named(NamedKey::Home) => {
                self.current = Some(0);
                self.keyboard_focus = true;
                None
            }
            Key::Named(NamedKey::End) => {
                self.current = Some(last);
                self.keyboard_focus = true;
                None
            }
            Key::Named(NamedKey::Enter) | Key::Char(' ') => self.choose(self.current?),
            _ => None,
        }
    }
}
