//! The tab strip: [`Tab`] and [`Tabs`] (spec §11.12).
//!
//! A tab strip selects one of several views. In its default
//! [`TabsOrientation::Horizontal`] it is a row of equal-width items sharing
//! the strip's width, each carrying a strong lower seam when selected and
//! reading on the content surface. Laid out [`TabsOrientation::Vertical`] it
//! is a *sidebar list*: items stacked top-down, each at its own content
//! height, carrying a strong leading seam and a quiet selected plate.
//!
//! A vertical item is a list entry rather than a tab shape, so it takes the
//! anatomy a sidebar needs: its label leads, an optional live
//! [`reading`](Tab::with_reading) trails on that same line, and an optional
//! bounded [`trend`](Tab::with_trend) draws beneath — which makes the strip a
//! live summary of everything it selects between. Items may be grouped:
//! [`with_group`](Tab::with_group) puts a quiet heading above the item that
//! starts a group. A vertical strip stacks rather than splits, so a list
//! longer than its box shows the entries it can seat whole and the owner
//! scrolls it — a squeezed entry that cannot draw its own label would be a
//! list that truncates instead of one that scrolls.
//!
//! A horizontal strip has one row and no room for either, so it draws neither;
//! a reading belongs in its label there (see [`Tab::set_label`]).
//!
//! A loading tab shows a Heat Seam on the same edge as the selection seam; a
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
use tairix_geometry::{Point, Rect, Region, Scale};
use tairix_input::{InputEvent, Key, NamedKey, PointerButton};
use tairix_raster::{Color, Surface};
use tairix_theme::{TextRole, Theme};

use crate::chart::Chart;
use crate::damage;
use crate::paint::{
    draw_outline, heavy_contrast, paint_bead, plate_border, role_font, seam_thickness, seam_width,
    surface_rect, text_plate_height, to_i32, BeadShape,
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
/// share the one selection, hit-testing, keyboard, and action model — the axis
/// items stack along, which edge carries the selection seam, and whether an
/// item is a tab shape or a sidebar list entry differ.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum TabsOrientation {
    /// A strip across the top: equal-width tabs, each carrying a strong lower
    /// seam when selected.
    Horizontal,
    /// A column down the side: a sidebar list, each entry at its own content
    /// height, carrying a strong leading seam and a quiet selected plate.
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
    reading: Option<String>,
    trend: Option<Chart>,
    group: Option<String>,
    modified: bool,
    state: ControlState,
}

impl Tab {
    /// A neutral, enabled tab with the given label.
    #[must_use]
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            reading: None,
            trend: None,
            group: None,
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

    /// This entry with a current reading, drawn trailing its label on the same
    /// line — sidebar anatomy, so a horizontal strip draws it nowhere.
    #[must_use]
    pub fn with_reading(mut self, reading: impl Into<String>) -> Self {
        self.reading = Some(reading.into());
        self
    }

    /// This entry with a bounded trend drawn beneath its label — sidebar
    /// anatomy, so a horizontal strip draws it nowhere.
    ///
    /// An entry that carries no trend claims no room for one, so the absence of
    /// an instrument is what says a reading is a fact rather than a rate.
    #[must_use]
    pub fn with_trend(mut self, trend: Chart) -> Self {
        self.trend = Some(trend);
        self
    }

    /// This entry as the start of a group, introduced by the quiet heading
    /// `heading` above it — sidebar anatomy, so a horizontal strip draws it
    /// nowhere.
    #[must_use]
    pub fn with_group(mut self, heading: impl Into<String>) -> Self {
        self.group = Some(heading.into());
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

    /// The entry's current reading, if any.
    #[must_use]
    pub fn reading(&self) -> Option<&str> {
        self.reading.as_deref()
    }

    /// Replace the entry's reading, leaving the rest of the entry alone — the
    /// live-sample counterpart of [`set_label`](Self::set_label).
    pub fn set_reading(&mut self, reading: Option<String>) {
        self.reading = reading;
    }

    /// The entry's trend, if any.
    #[must_use]
    pub fn trend(&self) -> Option<&Chart> {
        self.trend.as_ref()
    }

    /// Replace the entry's trend, leaving the rest of the entry alone — the
    /// live-sample counterpart of [`set_label`](Self::set_label).
    pub fn set_trend(&mut self, trend: Option<Chart>) {
        self.trend = trend;
    }

    /// The heading introducing this entry's group, if it starts one.
    #[must_use]
    pub fn group(&self) -> Option<&str> {
        self.group.as_deref()
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
/// The horizontal strip's whole geometry: equal tabs sharing the strip's
/// width. A vertical strip stacks at content height instead, so it does not
/// come through here.
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

/// What one band of a strip's stack is.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum BandKind {
    /// The quiet heading introducing the group the item at this index starts.
    Heading(usize),
    /// The item at this index.
    Item(usize),
}

/// One band of a strip's stack and the rectangle it occupies.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
struct Band {
    kind: BandKind,
    rect: Rect,
}

/// Item `index`'s rectangle within `bands`, or `None` when it was not seated —
/// the one rule every damage report and hit test applies.
fn item_area(bands: &[Band], index: usize) -> Option<Rect> {
    bands
        .iter()
        .find(|band| band.kind == BandKind::Item(index))
        .map(|band| band.rect)
}

/// A row of equal-width tabs, or — laid out [`TabsOrientation::Vertical`] — a
/// sidebar list of grouped entries, selecting one of several views
/// (spec §11.12).
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

    /// The extent the strip needs *across* its own axis — the height a
    /// horizontal strip occupies, or the width a vertical one does — from the
    /// font's line height and the theme's control padding, floored at the
    /// theme's standard control height so a strip never reads shorter than an
    /// ordinary control.
    ///
    /// A horizontal strip's height is the same for every tab regardless of
    /// how many share it, but a vertical strip's *width* is fixed regardless
    /// of how many entries stack down it; that fixed width has to comfortably
    /// hold the modified/error Signal Bead beside the label without the two
    /// competing for space, so the vertical extent additionally reserves the
    /// bead's own footprint.
    #[must_use]
    pub fn measured_extent(&self, scale: Scale, theme: &Theme) -> u32 {
        let base = text_plate_height(theme, scale, TextRole::Body);
        match self.orientation {
            TabsOrientation::Horizontal => base,
            TabsOrientation::Vertical => {
                let pad = scale.scale_length(theme.metrics().control_inset).max(1);
                let bead = scale.scale_length(theme.metrics().bead_size).max(3);
                base.saturating_add(bead).saturating_add(pad)
            }
        }
    }

    /// The height the strip needs to draw whole.
    ///
    /// A horizontal strip is one row, so this is its
    /// [`measured_extent`](Self::measured_extent). A vertical strip stacks, so
    /// this is every group heading plus every entry at its own content height
    /// — which is what an owner whose entry list is *discovered* rather than
    /// fixed reserves and scrolls, instead of squeezing entries into whatever
    /// column it happens to have.
    #[must_use]
    pub fn measured_height(&self, scale: Scale, theme: &Theme) -> u32 {
        match self.orientation {
            TabsOrientation::Horizontal => self.measured_extent(scale, theme),
            TabsOrientation::Vertical => {
                let heading = heading_height(scale, theme);
                self.items.iter().fold(0, |total, tab| {
                    total
                        .saturating_add(if tab.group.is_some() { heading } else { 0 })
                        .saturating_add(entry_height(tab, scale, theme))
                })
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
    ///
    /// Every tab whose selection actually changes reports its own rectangle, so
    /// moving the selection costs the tab it left and the tab it arrives on. The
    /// sweep is over all of them rather than those two, because the owner sets
    /// each tab's initial selection and nothing here may assume only one was
    /// ever lit.
    pub fn set_selected(
        &mut self,
        index: usize,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
        damage: &mut Region,
    ) {
        let bands = self.layout(bounds, scale, theme);
        for i in 0..self.items.len() {
            let rect = item_area(&bands, i).unwrap_or(Rect::EMPTY);
            let selection = if i == index {
                SelectionState::Selected
            } else {
                SelectionState::Unselected
            };
            if let Some(tab) = self.items.get_mut(i) {
                damage::set(&mut tab.state.selection, selection, rect, damage);
            }
        }
    }

    /// Adopt `index` as the selected tab without reporting, for a caller that is
    /// composing or rebuilding this strip and presents it whole.
    ///
    /// [`set_selected`](Self::set_selected) is the interactive move and reports
    /// the tabs whose plates change. A rebuild has no layout to resolve a tab
    /// against and nothing to report against either, so it says so here rather
    /// than passing a rectangle it does not have.
    pub fn adopt_selected(&mut self, index: usize) {
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
    /// keyboard focus as often as its model refreshes; a re-state that lands the
    /// cursor where it already was reports nothing.
    pub fn set_current(
        &mut self,
        index: Option<usize>,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
        damage: &mut Region,
    ) {
        self.move_current(self.on_strip(index), bounds, scale, theme, damage);
    }

    /// Adopt `index` as the keyboard cursor without reporting, for a caller that
    /// is composing or rebuilding this strip and presents it whole.
    ///
    /// [`set_current`](Self::set_current) is the interactive move and reports the
    /// two tabs the cursor moves between. A rebuild has no layout to resolve a
    /// tab against and nothing to report against either, so it says so here
    /// rather than passing a rectangle it does not have.
    pub fn adopt_current(&mut self, index: Option<usize>) {
        self.current = self.on_strip(index);
    }

    /// `index` if it names a tab of this strip, else `None` — the one admission
    /// rule every cursor entry point applies.
    fn on_strip(&self, index: Option<usize>) -> Option<usize> {
        index.filter(|&i| i < self.items.len())
    }

    /// Every band of the strip, in order, with the rectangle it occupies.
    ///
    /// The one layout [`render`](Self::render), the hit test and every damage
    /// report read, so a press can never select a tab drawn at a different
    /// span. A horizontal strip is items alone, sharing the strip's width
    /// equally. A vertical strip stacks top-down — a group's heading, then its
    /// entries, each at its own content height — and omits any band it cannot
    /// seat whole, along with everything after it: a half-drawn entry is worse
    /// than one the owner scrolls to.
    fn layout(&self, bounds: Rect, scale: Scale, theme: &Theme) -> Vec<Band> {
        let Some((x, y, w, h)) = surface_rect(bounds) else {
            return Vec::new();
        };
        if w == 0 || h == 0 {
            return Vec::new();
        }
        match self.orientation {
            TabsOrientation::Horizontal => (0..self.items.len())
                .filter_map(|index| {
                    let (offset, extent) = axis_span(index, self.items.len(), w)?;
                    Some(Band {
                        kind: BandKind::Item(index),
                        rect: Rect::new(to_i32(x + offset), to_i32(y), extent, h),
                    })
                })
                .collect(),
            TabsOrientation::Vertical => {
                let heading_h = heading_height(scale, theme);
                let mut bands = Vec::with_capacity(self.items.len());
                let mut top = 0u32;
                for (index, tab) in self.items.iter().enumerate() {
                    let own_heading = if tab.group.is_some() { heading_h } else { 0 };
                    let entry_h = entry_height(tab, scale, theme);
                    // A heading never appears without at least its own first
                    // entry beneath it.
                    if top
                        .saturating_add(own_heading)
                        .saturating_add(entry_h)
                        .saturating_add(1)
                        > h.saturating_add(1)
                    {
                        break;
                    }
                    if own_heading > 0 {
                        bands.push(Band {
                            kind: BandKind::Heading(index),
                            rect: Rect::new(
                                to_i32(x),
                                to_i32(y).saturating_add(to_i32(top)),
                                w,
                                own_heading,
                            ),
                        });
                        top = top.saturating_add(own_heading);
                    }
                    bands.push(Band {
                        kind: BandKind::Item(index),
                        rect: Rect::new(
                            to_i32(x),
                            to_i32(y).saturating_add(to_i32(top)),
                            w,
                            entry_h,
                        ),
                    });
                    top = top.saturating_add(entry_h);
                }
                bands
            }
        }
    }

    /// Tab `index`'s area within `bounds`, or `None` if it was not seated.
    #[must_use]
    pub fn tab_area(
        &self,
        index: usize,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
    ) -> Option<Rect> {
        item_area(&self.layout(bounds, scale, theme), index)
    }

    /// The tab index under `point`, if any, for the given bounds.
    ///
    /// A point over a group heading, or over an entry the strip could not seat,
    /// answers `None`: a heading selects nothing and an entry that was not
    /// drawn cannot be pressed (fail closed).
    #[must_use]
    pub fn tab_at(&self, bounds: Rect, scale: Scale, theme: &Theme, point: Point) -> Option<usize> {
        self.layout(bounds, scale, theme)
            .iter()
            .find_map(|band| match band.kind {
                BandKind::Item(index) if band.rect.contains(point) => Some(index),
                _ => None,
            })
    }

    /// Paint the strip into `surface` at `bounds` for the active theme.
    pub fn render(&self, surface: &mut Surface, bounds: Rect, scale: Scale, theme: &Theme) {
        let font = role_font(theme, scale, TextRole::Body);
        for band in self.layout(bounds, scale, theme) {
            let Some(rect) = surface_rect(band.rect) else {
                continue;
            };
            match band.kind {
                BandKind::Heading(index) => {
                    self.paint_heading(surface, index, rect, scale, theme, font);
                }
                BandKind::Item(index) => {
                    self.paint_tab(surface, index, rect, scale, theme, font);
                }
            }
        }
    }

    /// Paint the quiet group heading above the entry at `index`: its own text
    /// on the surface behind it, with no plate, so it reads as a break in the
    /// list rather than as another entry.
    fn paint_heading(
        &self,
        surface: &mut Surface,
        index: usize,
        rect: (u32, u32, u32, u32),
        scale: Scale,
        theme: &Theme,
        font: BitmapFont,
    ) {
        let Some(heading) = self.items.get(index).and_then(|tab| tab.group.as_deref()) else {
            return;
        };
        let (x, y, w, h) = rect;
        let pad = scale.scale_length(theme.metrics().control_inset).max(1);
        let gap = scale.scale_length(theme.metrics().control_gap).max(1);
        let Some(avail) = w.checked_sub(pad.saturating_mul(2)) else {
            return;
        };
        if avail == 0 || h < font.line_height() {
            return;
        }
        let fitted = font.truncate_to_width(heading, avail);
        font.draw_text(
            surface,
            to_i32(x.saturating_add(pad)),
            to_i32(y.saturating_add(gap)),
            fitted,
            Color::from(theme.palette().on_surface_muted),
        );
    }

    /// Paint the tab at `index` into the `rect` [`Self::layout`] gave it:
    /// its plate, the seam its orientation carries, the keyboard focus ring,
    /// then its own content.
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

        match self.orientation {
            TabsOrientation::Horizontal => {
                Self::paint_centred_label(surface, rect, scale, theme, font, tab);
            }
            TabsOrientation::Vertical => {
                Self::paint_entry(surface, rect, scale, theme, font, tab);
            }
        }
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

    /// The colour `tab`'s label reads in: muted when its state rules it out,
    /// the accent when selected, the plain foreground otherwise.
    fn label_color(theme: &Theme, tab: &Tab) -> Color {
        let palette = theme.palette();
        Color::from(match tab.state.disposition() {
            ControlDisposition::DisabledByState => palette.on_surface_muted,
            _ if tab.is_selected() => palette.accent,
            _ => palette.on_surface,
        })
    }

    /// Paint `tab`'s label centred in `rect`, and its Signal Bead at the
    /// top-trailing corner — the horizontal tab shape.
    ///
    /// The bead's footprint is carved out of the label's own budget before the
    /// label is laid out, so a long label is truncated rather than running
    /// under the bead.
    fn paint_centred_label(
        surface: &mut Surface,
        rect: (u32, u32, u32, u32),
        scale: Scale,
        theme: &Theme,
        font: BitmapFont,
        tab: &Tab,
    ) {
        let (x, y, w, h) = rect;
        let border = plate_border(theme, scale);
        let pad = scale.scale_length(theme.metrics().control_inset).max(1);
        let bead_w = Self::bead_gutter(scale, theme, rect, tab);
        let avail = w
            .saturating_sub(border.saturating_add(pad).saturating_mul(2))
            .saturating_sub(bead_w);
        if avail > 0 {
            let fitted = font.truncate_to_width(tab.label(), avail);
            let tw = font.text_width(fitted);
            let cx = to_i32(x) + to_i32(w.saturating_sub(bead_w)) / 2;
            let glyph_h = font.glyph_height();
            let text_y = to_i32(y) + (to_i32(h) - to_i32(glyph_h)).max(0) / 2;
            font.draw_text(
                surface,
                cx - to_i32(tw) / 2,
                text_y,
                fitted,
                Self::label_color(theme, tab),
            );
        }
        Self::paint_tab_bead(surface, rect, scale, theme, tab);
    }

    /// Paint `tab` as a sidebar list entry: its label leading, its reading
    /// trailing on that same line, and its trend beneath.
    ///
    /// Room is claimed in the order a reader needs it: the Signal Bead first
    /// (it is a state, not a reading), then the reading, then the label, which
    /// is what truncates — the reading is what the reader came for. The trend
    /// draws only where a whole one still fits beneath the label line.
    fn paint_entry(
        surface: &mut Surface,
        rect: (u32, u32, u32, u32),
        scale: Scale,
        theme: &Theme,
        font: BitmapFont,
        tab: &Tab,
    ) {
        let (x, y, w, h) = rect;
        let pad = scale.scale_length(theme.metrics().control_inset).max(1);
        let gap = scale.scale_length(theme.metrics().control_gap).max(1);
        let seam = seam_thickness(theme, scale);
        let lead = seam.saturating_add(pad);
        let Some(inner_w) = w.checked_sub(lead.saturating_add(pad)) else {
            Self::paint_tab_bead(surface, rect, scale, theme, tab);
            return;
        };
        let inner_x = x.saturating_add(lead);
        let glyph_h = font.glyph_height();
        let label_row = text_plate_height(theme, scale, TextRole::Body);
        let text_y = y.saturating_add(label_row.saturating_sub(glyph_h) / 2);

        let bead_w = Self::bead_gutter(scale, theme, rect, tab);
        let mut avail = inner_w.saturating_sub(bead_w);

        if let Some(reading) = tab.reading() {
            let fitted = font.truncate_to_width(reading, avail);
            let reading_w = font.text_width(fitted).min(avail);
            let reading_x = inner_x
                .saturating_add(inner_w)
                .saturating_sub(bead_w)
                .saturating_sub(reading_w);
            font.draw_text(
                surface,
                to_i32(reading_x),
                to_i32(text_y),
                fitted,
                Color::from(theme.palette().on_surface_muted),
            );
            avail = avail.saturating_sub(reading_w.saturating_add(gap));
        }
        if avail > 0 {
            let fitted = font.truncate_to_width(tab.label(), avail);
            font.draw_text(
                surface,
                to_i32(inner_x),
                to_i32(text_y),
                fitted,
                Self::label_color(theme, tab),
            );
        }

        if let Some(trend) = tab.trend() {
            let trend_h = chart_height(scale, theme);
            let trend_y = y.saturating_add(label_row);
            if trend_y.saturating_add(trend_h) <= y.saturating_add(h) && inner_w > 0 {
                trend.render(
                    surface,
                    Rect::new(to_i32(inner_x), to_i32(trend_y), inner_w, trend_h),
                    scale,
                    theme,
                );
            }
        }

        Self::paint_tab_bead(surface, rect, scale, theme, tab);
    }

    /// The width `tab`'s Signal Bead claims at the trailing end of its label
    /// line, including the gap that keeps it off the text — zero when it shows
    /// none, or when the bead could not sit inside the tab at all.
    fn bead_gutter(scale: Scale, theme: &Theme, rect: (u32, u32, u32, u32), tab: &Tab) -> u32 {
        let (_, _, w, h) = rect;
        if Self::tab_bead(theme, tab).is_none() {
            return 0;
        }
        let pad = scale.scale_length(theme.metrics().control_inset).max(1);
        scale
            .scale_length(theme.metrics().bead_size)
            .max(3)
            .min(w)
            .min(h)
            .saturating_add(pad)
    }

    /// Paint `tab`'s Signal Bead at the top-trailing corner of `rect`, if it
    /// shows one.
    fn paint_tab_bead(
        surface: &mut Surface,
        rect: (u32, u32, u32, u32),
        scale: Scale,
        theme: &Theme,
        tab: &Tab,
    ) {
        let Some((color, shape)) = Self::tab_bead(theme, tab) else {
            return;
        };
        let (x, y, w, h) = rect;
        let border = plate_border(theme, scale);
        let size = scale
            .scale_length(theme.metrics().bead_size)
            .max(3)
            .min(w)
            .min(h);
        // A bead that cannot sit inside its own tab past the plate border is
        // omitted: a cramped strip loses the marker rather than stamping it
        // over the neighbouring tab.
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

    /// Put the keyboard cursor on `next`, or take it off the strip with `None`,
    /// reporting the tab it left and the tab it arrives on rather than the whole
    /// strip.
    fn move_current(
        &mut self,
        next: Option<usize>,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
        damage: &mut Region,
    ) {
        let bands = self.layout(bounds, scale, theme);
        if damage::move_mark(self.current, next, |index| item_area(&bands, index), damage) {
            self.current = next;
        }
    }

    /// Feed a pointer event; the tab under the pointer lifts and a completed
    /// primary click selects it.
    ///
    /// A press moves no pointer, so it states nothing new about where the
    /// pointer is; it only arms the tab it landed on.
    ///
    /// A lift that moves reports the two tabs it moved between; a sample that
    /// stays on one tab reports nothing.
    pub fn on_pointer(
        &mut self,
        event: &InputEvent,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
        damage: &mut Region,
    ) -> Option<TabsAction> {
        if let InputEvent::PointerMoved { to } = event {
            *self.pointer = *to;
        }
        let bands = self.layout(bounds, scale, theme);
        let over = bands.iter().find_map(|band| match band.kind {
            BandKind::Item(index) if band.rect.contains(*self.pointer) => Some(index),
            _ => None,
        });
        match event {
            InputEvent::PointerMoved { .. } => {
                if self.armed.is_none()
                    && damage::move_mark(
                        self.hovered,
                        over,
                        |index| item_area(&bands, index),
                        damage,
                    )
                {
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
    /// to the ends in either, and Enter/Space select the current tab. A moved
    /// cursor reports the two tabs it moved between; selecting reports nothing,
    /// because the strip draws the selection its owner sets.
    ///
    /// The two arrow pairs are deliberately exclusive to their own axis: a
    /// vertical strip ignores Left/Right and a horizontal one ignores Up/Down,
    /// so a reader is never misled into thinking the wrong arrows move it.
    pub fn on_key(
        &mut self,
        key: Key,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
        damage: &mut Region,
    ) -> Option<TabsAction> {
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
                self.move_current(Some(next), bounds, scale, theme, damage);
                None
            }
            Key::Named(named) if named == backward => {
                let prev = match self.current {
                    Some(0) | None => last,
                    Some(i) => i - 1,
                };
                self.move_current(Some(prev), bounds, scale, theme, damage);
                None
            }
            Key::Named(NamedKey::Home) => {
                self.move_current(Some(0), bounds, scale, theme, damage);
                None
            }
            Key::Named(NamedKey::End) => {
                self.move_current(Some(last), bounds, scale, theme, damage);
                None
            }
            Key::Named(NamedKey::Enter) | Key::Char(' ') => self.choose(self.current?),
            _ => None,
        }
    }
}

/// The height one vertical entry claims: the shared one-line plate height,
/// plus its own trend where it carries one.
///
/// The reading shares the label's line, so it costs no height; the trend is
/// what an entry pays for, which is why an entry with no rate behind it is
/// visibly shorter than one with a trace.
fn entry_height(tab: &Tab, scale: Scale, theme: &Theme) -> u32 {
    let row = text_plate_height(theme, scale, TextRole::Body);
    match tab.trend {
        Some(_) => row.saturating_add(chart_height(scale, theme)),
        None => row,
    }
}

/// The height one group heading claims: a quiet label with breathing room
/// above and below, so a heading separates its group rather than reading as
/// another entry.
fn heading_height(scale: Scale, theme: &Theme) -> u32 {
    let font = role_font(theme, scale, TextRole::Body);
    let gap = scale.scale_length(theme.metrics().control_gap).max(1);
    font.line_height().saturating_add(gap.saturating_mul(2))
}

/// The height an entry's trend claims, from the theme's own chart metric.
fn chart_height(scale: Scale, theme: &Theme) -> u32 {
    scale.scale_length(theme.metrics().chart_height).max(1)
}
