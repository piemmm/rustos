//! The tab strip: [`Tab`] and [`Tabs`] (spec §11.12).
//!
//! A tab strip is a row (or, in [`TabsOrientation::Vertical`], a column) of
//! equal-extent items that select one of several views. In its default
//! [`TabsOrientation::Horizontal`] the selected tab carries a strong lower
//! seam and reads on the content surface; laid out [`TabsOrientation::Vertical`]
//! it instead carries a strong leading seam and a quiet selected plate. A
//! loading tab shows a Heat Seam on the same edge as the selection seam; a
//! modified tab shows a small Signal Bead; and an error tab shows a warning or
//! recovery bead so its state is legible without colour (spec §11.12, §15).
//! One definition drives both orientations: the strip owns keyboard navigation
//! (Left/Right move the current tab in a horizontal strip, Up/Down in a
//! vertical one, Home/End jump to the ends in either, Enter/Space select it)
//! and pointer hover/click, emitting a typed [`TabsAction`]; it enforces no
//! authority. Every colour, metric, and radius resolves from the active
//! [`Theme`] and [`Scale`].

use alloc::string::String;
use alloc::vec::Vec;

use tairix_font::BitmapFont;
use tairix_geometry::{Point, Rect, Scale};
use tairix_input::{InputEvent, Key, NamedKey, PointerButton};
use tairix_raster::{Color, Surface};
use tairix_theme::{TextRole, Theme};

use crate::paint::{
    draw_outline, heavy_contrast, paint_bead, plate_border, role_font, seam_thickness, seam_width,
    surface_rect, to_i32, BeadShape,
};
use crate::state::{
    ActivityState, ControlDisposition, ControlState, RenderInvariant, SelectionState,
    ValidationState,
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

/// Which axis a [`Tabs`] strip stacks its items along (spec §11.12).
///
/// [`Tabs::new`] always produces [`TabsOrientation::Horizontal`]; a sidebar
/// selects [`TabsOrientation::Vertical`] with [`Tabs::with_orientation`]. Both
/// share the one selection, hit-testing, keyboard, and action model — only the
/// axis tabs stack along, and which edge carries the selection seam, differ.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum TabsOrientation {
    /// A strip across the top: the selected tab carries a strong lower seam.
    Horizontal,
    /// A column down the side: the selected tab carries a strong leading seam
    /// and a quiet selected plate.
    Vertical,
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

    /// Replace the tab's label, leaving the rest of the tab alone.
    ///
    /// For a strip whose labels carry a live reading — a count beside the
    /// name — so the owner can re-label in place and keep the strip that
    /// holds where the pointer and the keyboard cursor are.
    pub fn set_label(&mut self, label: impl Into<String>) {
        self.label = label.into();
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

/// The `(offset, extent)` span of item `index` when `count` equal items share
/// `total` pixels along one axis, the last absorbing the rounding remainder.
///
/// [`Tabs::tab_rect`] calls this once for whichever axis the strip's
/// [`TabsOrientation`] stacks along — the width for a horizontal strip, the
/// height for a vertical one — so both orientations share the exact same
/// equal-split arithmetic and can never diverge into two recipes.
fn axis_span(index: usize, count: usize, total: u32) -> Option<(u32, u32)> {
    let count_u32 = u32::try_from(count).ok()?;
    if count_u32 == 0 || total == 0 || index >= count {
        return None;
    }
    let idx = u32::try_from(index).ok()?;
    let each = total / count_u32;
    if each == 0 {
        return None;
    }
    let offset = idx * each;
    // The last item absorbs the rounding remainder so the strip fills the
    // whole axis.
    let extent = if index + 1 == count {
        total - idx * each
    } else {
        each
    };
    Some((offset, extent))
}

/// The `(x, y, w, h)` a seam `thickness` deep and `extent` long occupies on a
/// tab's own `rect`: the lower edge of a horizontal tab, the leading (left)
/// edge of a vertical one.
///
/// This is the one definition of which edge carries a seam, so a tab's
/// selection seam and its Heat Seam can never end up on different edges of the
/// same tab. The extent is capped by the tab's own span along that edge, so a
/// seam never runs past the tab it belongs to.
#[must_use]
fn seam_rect(
    orientation: TabsOrientation,
    rect: (u32, u32, u32, u32),
    thickness: u32,
    extent: u32,
) -> (u32, u32, u32, u32) {
    let (x, y, w, h) = rect;
    match orientation {
        TabsOrientation::Horizontal => (
            x,
            y.saturating_add(h).saturating_sub(thickness),
            extent.min(w),
            thickness,
        ),
        TabsOrientation::Vertical => (x, y, thickness, extent.min(h)),
    }
}

/// A row (or, laid out [`TabsOrientation::Vertical`], a column) of
/// equal-extent items selecting one of several views (spec §11.12).
///
/// The strip keeps where the pointer rests (the *hovered* tab) apart from
/// where the keyboard cursor is (the *current* tab): both lift their plate,
/// only the current tab is ringed. One record for both would blink a resting
/// pointer's highlight off every time a host re-stated its keyboard focus,
/// which hosts do whenever their model refreshes. Neither is the *selected*
/// tab, whose view is the one on show: selection commits through the owner via
/// [`TabsAction::Selected`], which then updates the items' [`SelectionState`]
/// (helper [`Tabs::set_selected`]).
///
/// Equal strips draw the same pixels, so a host may use `==` as its repaint
/// gate: the items, the orientation, and both records of attention compare.
/// The pointer coordinate and the pressed-tab latch do not — no render path
/// reads either, and a press's visible consequence is the lift the pointer's
/// own motion already stated.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Tabs {
    items: Vec<Tab>,
    orientation: TabsOrientation,
    /// The tab the pointer rests on, or the one holding a press while the
    /// pointer slides off it.
    hovered: Option<usize>,
    /// The tab the keyboard cursor is on.
    current: Option<usize>,
    /// The last pointer position, mapped to a tab on the next press or
    /// release — hit-testing input, never drawn.
    pointer: RenderInvariant<Point>,
    /// The tab a primary press landed on, held until release so a click that
    /// slides onto another tab does not select it; the pressed tab keeps its
    /// lift meanwhile.
    armed: RenderInvariant<Option<usize>>,
}

impl Tabs {
    /// A horizontal tab strip over the given items.
    #[must_use]
    pub fn new(tabs: Vec<Tab>) -> Self {
        Self {
            items: tabs,
            orientation: TabsOrientation::Horizontal,
            hovered: None,
            current: None,
            pointer: RenderInvariant::new(Point::ORIGIN),
            armed: RenderInvariant::new(None),
        }
    }

    /// This strip laid out along `orientation`.
    #[must_use]
    pub fn with_orientation(mut self, orientation: TabsOrientation) -> Self {
        self.orientation = orientation;
        self
    }

    /// The strip's orientation.
    #[must_use]
    pub fn orientation(&self) -> TabsOrientation {
        self.orientation
    }

    /// The extent the strip needs along its own axis — the height a
    /// horizontal strip occupies, or the width a vertical one does — from the
    /// font's line height and the theme's control padding, floored at the
    /// theme's standard control height so a strip never reads shorter than an
    /// ordinary control.
    ///
    /// A horizontal strip's height is the same for every tab regardless of
    /// how many share it, but a vertical strip's *width* is fixed regardless
    /// of how many tabs stack down it; that fixed width has to comfortably
    /// hold the modified/error Signal Bead beside the label without the two
    /// competing for space, so the vertical extent additionally reserves the
    /// bead's own footprint.
    #[must_use]
    pub fn measured_extent(&self, scale: Scale, theme: &Theme) -> u32 {
        let font = role_font(theme, scale, TextRole::Body);
        let pad = scale.scale_length(theme.metrics().control_inset).max(1);
        let text_band = font.glyph_height().saturating_add(pad.saturating_mul(2));
        let base = text_band.max(scale.scale_length(theme.metrics().control_height).max(1));
        match self.orientation {
            TabsOrientation::Horizontal => base,
            TabsOrientation::Vertical => {
                let bead = scale.scale_length(theme.metrics().bead_size).max(3);
                base.saturating_add(bead).saturating_add(pad)
            }
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

    /// The tab the keyboard cursor is on, if any.
    #[must_use]
    pub fn current(&self) -> Option<usize> {
        self.current
    }

    /// Put the keyboard cursor on `index`, or take it off the strip with
    /// `None`; an out-of-range index takes it off (fail closed).
    ///
    /// Where the pointer rests is untouched, so a host may re-state its
    /// keyboard focus as often as its model refreshes.
    pub fn set_current(&mut self, index: Option<usize>) {
        self.current = index.filter(|&i| i < self.items.len());
    }

    /// The surface rectangle of tab `index` within `bounds`, or `None` if it
    /// collapses. Tabs share the strip's own axis equally — width for a
    /// horizontal strip, height for a vertical one — through the shared
    /// [`axis_span`] helper, so a press can never select a tab that render
    /// did not draw at that same span.
    fn tab_rect(&self, index: usize, bounds: Rect) -> Option<(u32, u32, u32, u32)> {
        let (x, y, w, h) = surface_rect(bounds)?;
        if w == 0 || h == 0 || index >= self.items.len() {
            return None;
        }
        match self.orientation {
            TabsOrientation::Horizontal => {
                let (ox, ow) = axis_span(index, self.items.len(), w)?;
                Some((x + ox, y, ow, h))
            }
            TabsOrientation::Vertical => {
                let (oy, oh) = axis_span(index, self.items.len(), h)?;
                Some((x, y + oy, w, oh))
            }
        }
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
    pub fn render(&self, surface: &mut Surface, bounds: Rect, scale: Scale, theme: &Theme) {
        let font = role_font(theme, scale, TextRole::Body);
        for index in 0..self.items.len() {
            if let Some(rect) = self.tab_rect(index, bounds) {
                self.paint_tab(surface, index, rect, scale, theme, font);
            }
        }
    }

    /// Paint the tab at `index` into the `rect` [`Self::tab_rect`] gave it:
    /// its plate, the seam its orientation carries, the keyboard focus ring,
    /// then its label and Signal Bead.
    fn paint_tab(
        &self,
        surface: &mut Surface,
        index: usize,
        rect: (u32, u32, u32, u32),
        scale: Scale,
        theme: &Theme,
        font: BitmapFont,
    ) {
        let Some(tab) = self.items.get(index) else {
            return;
        };
        let (x, y, w, h) = rect;
        if w == 0 || h == 0 {
            return;
        }
        let palette = theme.palette();
        let current = self.current == Some(index);
        let lifted = current || self.hovered == Some(index);

        // Tab plate: the selected tab reads as a quiet selected plate on the
        // content surface in either orientation; an unselected tab is
        // quieter; a tab either the pointer or the keyboard cursor is on
        // lifts. Disabled stays muted.
        let plate = if tab.is_selected() {
            palette.surface
        } else if lifted {
            palette.surface_raised
        } else {
            palette.surface_pressed
        };
        surface.fill_rect(x, y, w, h, Color::from(plate));

        Self::paint_seam(surface, self.orientation, rect, scale, theme, tab);

        // The keyboard focus ring, distinct from a hover lift.
        if current {
            draw_outline(
                surface,
                x,
                y,
                w,
                h,
                plate_border(theme, scale).max(1),
                Color::from(palette.rim_active),
            );
        }

        Self::paint_label(surface, rect, scale, theme, font, tab);
    }

    /// Paint `tab`'s seam onto the edge its `orientation` carries it on: a
    /// strong accent seam for the selected tab, else a Heat Seam while its
    /// view loads (proportional when the fraction is known).
    ///
    /// A selected tab shows selection rather than progress, so its seam wins
    /// over a Heat Seam it would otherwise draw on the very same edge.
    fn paint_seam(
        surface: &mut Surface,
        orientation: TabsOrientation,
        rect: (u32, u32, u32, u32),
        scale: Scale,
        theme: &Theme,
        tab: &Tab,
    ) {
        let (_, _, w, h) = rect;
        let (along, cross) = match orientation {
            TabsOrientation::Horizontal => (w, h),
            TabsOrientation::Vertical => (h, w),
        };
        let base = seam_thickness(theme, scale);
        let (thickness, extent) = if tab.is_selected() {
            // Heavier contrast doubles the selected seam, so selection still
            // carries where a hue shift alone would not.
            let heavy = if heavy_contrast(theme) { 2 } else { 1 };
            (base.saturating_mul(heavy).min(cross), along)
        } else if tab.is_loading() {
            (base.min(cross), seam_width(tab.state.activity, along))
        } else {
            return;
        };
        if thickness == 0 || extent == 0 {
            return;
        }
        let (sx, sy, sw, sh) = seam_rect(orientation, rect, thickness, extent);
        surface.fill_rect(sx, sy, sw, sh, Color::from(theme.palette().accent));
    }

    /// Paint `tab`'s label centred in `rect`, and its modified/error Signal
    /// Bead at the top-trailing corner.
    ///
    /// The bead's footprint is carved out of the label's own budget before the
    /// label is laid out, so a long label is truncated rather than running
    /// under the bead.
    fn paint_label(
        surface: &mut Surface,
        rect: (u32, u32, u32, u32),
        scale: Scale,
        theme: &Theme,
        font: BitmapFont,
        tab: &Tab,
    ) {
        let (x, y, w, h) = rect;
        let palette = theme.palette();
        let metrics = theme.metrics();
        let border = plate_border(theme, scale);

        // The label, centred, with weight conveyed by colour (accent when
        // selected) since the bitmap font has one weight.
        let label_color = match tab.state.disposition() {
            ControlDisposition::DisabledByState => palette.on_surface_muted,
            _ if tab.is_selected() => palette.accent,
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

        if let Some((color, shape)) = bead {
            let size = scale.scale_length(metrics.bead_size).max(3).min(w).min(h);
            // A bead that cannot sit inside its own tab past the plate border
            // is omitted: a cramped strip loses the marker rather than
            // stamping it over the neighbouring tab.
            let corner = border.saturating_add(size);
            if corner <= w.min(h) {
                paint_bead(
                    surface,
                    x.saturating_add(w - corner),
                    y.saturating_add(border),
                    size,
                    color,
                    shape,
                );
            }
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

    /// Feed a pointer event; the tab under the pointer lifts and a completed
    /// primary click selects it.
    ///
    /// A press moves no pointer, so it states nothing new about where the
    /// pointer is; it only arms the tab it landed on.
    pub fn on_pointer(&mut self, event: &InputEvent, bounds: Rect) -> Option<TabsAction> {
        if let InputEvent::PointerMoved { to } = event {
            *self.pointer = *to;
        }
        let over = self.tab_at(bounds, *self.pointer);
        match event {
            InputEvent::PointerMoved { .. } => {
                if self.armed.is_none() {
                    self.hovered = over;
                }
                None
            }
            InputEvent::PointerPressed {
                button: PointerButton::Primary,
            } => {
                *self.armed = over;
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

    /// Feed a key event: Left/Right move the current tab (wrapping) in a
    /// horizontal strip, Up/Down do the same in a vertical one, Home/End jump
    /// to the ends in either, and Enter/Space select the current tab.
    ///
    /// The two arrow pairs are deliberately exclusive to their own axis: a
    /// vertical strip ignores Left/Right and a horizontal one ignores Up/Down,
    /// so a reader is never misled into thinking the wrong arrows move it.
    pub fn on_key(&mut self, key: Key) -> Option<TabsAction> {
        if self.items.is_empty() {
            return None;
        }
        let last = self.items.len() - 1;
        let (forward, backward) = match self.orientation {
            TabsOrientation::Horizontal => (NamedKey::Right, NamedKey::Left),
            TabsOrientation::Vertical => (NamedKey::Down, NamedKey::Up),
        };
        match key {
            Key::Named(named) if named == forward => {
                let next = match self.current {
                    Some(i) if i < last => i + 1,
                    _ => 0,
                };
                self.current = Some(next);
                None
            }
            Key::Named(named) if named == backward => {
                let prev = match self.current {
                    Some(0) | None => last,
                    Some(i) => i - 1,
                };
                self.current = Some(prev);
                None
            }
            Key::Named(NamedKey::Home) => {
                self.current = Some(0);
                None
            }
            Key::Named(NamedKey::End) => {
                self.current = Some(last);
                None
            }
            Key::Named(NamedKey::Enter) | Key::Char(' ') => self.choose(self.current?),
            _ => None,
        }
    }
}
