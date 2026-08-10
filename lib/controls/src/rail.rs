//! The action rail: [`ActionRail`].
//!
//! An action rail is the vertical column of full-width commands a detail
//! surface offers about the thing it is showing — the counterpart to the
//! horizontal [`Toolbar`](crate::toolbar::Toolbar). Each item *is* a
//! [`Button`], so the rail restates none of a button's plate painting, press
//! feedback, role emphasis, disabled look, or Authority Mark for a denied
//! action; it owns only the stacking geometry, which item the pointer is
//! over, which item holds keyboard focus, routing input to the item under
//! the pointer or in focus, and translating a completed
//! [`ButtonAction`](crate::button::ButtonAction) into a typed [`RailAction`].
//!
//! Items stack top to bottom, each spanning the rail's full width so their
//! labels align, separated by the theme's control gap; an item's height is
//! the button family's own standard control height, never a number the rail
//! invents. When the given bounds are too short for every item, the rail
//! draws as many whole items as fit from the top and omits the rest —
//! hit-testing, rendering, and measurement all walk the one shared layout so
//! a press can never land on an item that was not drawn.
//!
//! A rail anchored beside a scrollable list — its content beside it moving
//! while the rail itself stays put — can light an Edge Wake down its own
//! leading edge with [`ActionRail::with_edge_wake`], so the reader can tell
//! the list has moved even though the anchored column of commands never
//! does.

use alloc::vec::Vec;

use tairix_geometry::{Point, Rect, Region, Scale};
use tairix_input::{InputEvent, Key, NamedKey};
use tairix_raster::Surface;
use tairix_theme::{TextRole, Theme};

use crate::button::{icon_content_side, Button, ButtonContent, ContentAlign};
use crate::damage;
use crate::paint::{
    grab_after, paint_edge_wake, plate_border, role_font, route_pointer, surface_rect,
    text_plate_height, to_i32,
};
use crate::state::RenderInvariant;

/// The outcome of feeding input to an [`ActionRail`].
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum RailAction {
    /// The item at `index` was activated and its command should be
    /// dispatched.
    Activate {
        /// The zero-based index of the activated item.
        index: usize,
    },
}

/// A vertical column of full-width command [`Button`]s.
///
/// Every item is seated [`ContentAlign::Leading`], so the icons and labels of
/// the whole column line up and the rail reads as a list of commands rather
/// than a stack of centred captions. The rail imposes that on the items it is
/// given, so no caller has to remember it and two rails can never disagree.
///
/// The rail owns keyboard focus (Up/Down move it, clamping at the ends;
/// Home/End jump to the ends) and pointer routing; every event is forwarded
/// to whichever item it lands on or, for the keyboard, to the item that
/// holds focus, and the item itself decides whether it may activate — a
/// disabled or denied item keeps its slot and consumes an activation key
/// without acting, exactly as [`Menu`](crate::menu::Menu) treats a
/// non-actionable row. Activation is reported as a typed [`RailAction`]; the
/// rail enforces no authority.
///
/// Equal rails draw the same pixels, so a host may use `==` as its repaint
/// gate: the items (each with its own hover/press/focus/authority state),
/// which one holds focus, and whether the Edge Wake is lit all compare.
/// Neither item stores anything that is not drawn — see [`Button`]'s own
/// equality note — so there is nothing here for a rail-level
/// `RenderInvariant` to exempt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActionRail {
    items: Vec<Button>,
    focus: Option<usize>,
    /// Whether the rail's own leading-edge Edge Wake is lit (see
    /// [`with_edge_wake`](Self::with_edge_wake)).
    edge_wake: bool,
    /// The last pointer position, resolved against the item rects —
    /// hit-testing input, never drawn.
    pointer: RenderInvariant<Point>,
    /// The item the pointer was last over, so a motion sample reaches the
    /// item it left and the one it entered rather than the whole column. It
    /// mirrors the hover the items themselves carry.
    hovered: RenderInvariant<Option<usize>>,
    /// The item holding a press, which keeps receiving the stream wherever
    /// the pointer goes.
    armed: RenderInvariant<Option<usize>>,
}

impl ActionRail {
    /// A rail over the given items, with no item focused and its Edge Wake
    /// unlit.
    ///
    /// Each item is re-seated [`ContentAlign::Leading`] as it is taken in.
    #[must_use]
    pub fn new(items: Vec<Button>) -> Self {
        Self {
            items: items
                .into_iter()
                .map(|item| item.aligned(ContentAlign::Leading))
                .collect(),
            focus: None,
            pointer: RenderInvariant::new(Point::ORIGIN),
            hovered: RenderInvariant::new(None),
            armed: RenderInvariant::new(None),
            edge_wake: false,
        }
    }

    /// This rail with its leading-edge Edge Wake lit (`lit`) or unlit.
    ///
    /// A rail anchored beside a list lights an Edge Wake down its own
    /// leading edge while the content beside it is scrolled away from its
    /// start, so the reader can see the list has moved even though the
    /// anchored rail itself never does.
    #[must_use]
    pub fn with_edge_wake(mut self, lit: bool) -> Self {
        self.edge_wake = lit;
        self
    }

    /// Whether the rail's Edge Wake is currently lit.
    #[must_use]
    pub fn edge_wake(&self) -> bool {
        self.edge_wake
    }

    /// The rail's items.
    #[must_use]
    pub fn items(&self) -> &[Button] {
        &self.items
    }

    /// Mutable access to the rail's items (e.g. to update one's state from a
    /// model).
    pub fn items_mut(&mut self) -> &mut [Button] {
        &mut self.items
    }

    /// The number of items.
    #[must_use]
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Whether the rail has no items.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// The focused item, if any.
    #[must_use]
    pub fn focus(&self) -> Option<usize> {
        self.focus
    }

    /// Focus item `index` (or clear focus with `None`), updating each item's
    /// own focus flag so its focus ring draws; an out-of-range index clears
    /// focus (fail closed).
    ///
    /// A focus move reports the whole rail: the ring leaves one item and
    /// arrives at another, and without a scale and theme the rail cannot say
    /// where either item sits. Covering the column over-covers by the items
    /// that did not change, which repaints correctly.
    pub fn set_focus(&mut self, index: Option<usize>, bounds: Rect, damage: &mut Region) {
        let index = index.filter(|&i| i < self.items.len());
        damage::set(&mut self.focus, index, bounds, damage);
        for (i, item) in self.items.iter_mut().enumerate() {
            item.set_focused(Some(i) == index);
        }
    }

    /// The scaled height of the visible plate every item draws at (the
    /// button family's own standard control height).
    fn content_height(scale: Scale, theme: &Theme) -> u32 {
        text_plate_height(theme, scale, TextRole::Body)
    }

    /// The scaled gap that separates one item's plate from the next.
    fn gap(scale: Scale, theme: &Theme) -> u32 {
        scale.scale_length(theme.metrics().control_gap).max(1)
    }

    /// Height one item occupies, including the gap that follows it.
    ///
    /// An item's height is the button family's own control height, never a
    /// text metric.
    #[must_use]
    pub fn item_height(scale: Scale, theme: &Theme) -> u32 {
        Self::content_height(scale, theme).saturating_add(Self::gap(scale, theme))
    }

    /// The minimum height the whole rail needs: every item's slot, stacked.
    #[must_use]
    pub fn measured_height(&self, scale: Scale, theme: &Theme) -> u32 {
        let per_item = Self::item_height(scale, theme);
        let count = u32::try_from(self.items.len()).unwrap_or(u32::MAX);
        per_item.saturating_mul(count)
    }

    /// The minimum width the widest item needs before its content is
    /// truncated: the button family's own frame border and content inset
    /// around whichever item's label, icon, or icon-and-label group is
    /// widest.
    #[must_use]
    pub fn measured_width(&self, scale: Scale, theme: &Theme) -> u32 {
        let font = role_font(theme, scale, TextRole::Body);
        let border = plate_border(theme, scale);
        let pad = scale.scale_length(theme.metrics().control_inset).max(1);
        let ch = Self::content_height(scale, theme);
        let gap = Self::gap(scale, theme);
        let mut widest = 0;
        for item in &self.items {
            let content = match item.content() {
                ButtonContent::Label(text) => font.text_width(text),
                ButtonContent::Icon(_) => icon_content_side(ch, ch, border),
                ButtonContent::IconLabel { label, .. } => {
                    let side = font.glyph_height().min(ch);
                    side.saturating_add(gap)
                        .saturating_add(font.text_width(label))
                }
            };
            let w = border
                .saturating_add(pad)
                .saturating_add(content)
                .saturating_add(pad)
                .saturating_add(border);
            widest = widest.max(w);
        }
        widest
    }

    /// The surface [`Rect`] each *drawn* item occupies, in order: as many
    /// whole items as fit from the top of `bounds`, each spanning its full
    /// width at the button family's control height.
    ///
    /// This is the one layout every measurement, hit test, and paint reads,
    /// so a press can never land on an item [`render`](Self::render) did not
    /// draw.
    fn layout(&self, bounds: Rect, scale: Scale, theme: &Theme) -> Vec<Rect> {
        let Some((x, y, w, h)) = surface_rect(bounds) else {
            return Vec::new();
        };
        if w == 0 || h == 0 {
            return Vec::new();
        }
        let content = Self::content_height(scale, theme);
        let slot = Self::item_height(scale, theme);
        let mut rects = Vec::with_capacity(self.items.len());
        let mut top = 0u32;
        for _ in &self.items {
            if top.saturating_add(content) > h {
                break;
            }
            rects.push(Rect::new(
                to_i32(x),
                to_i32(y).saturating_add(to_i32(top)),
                w,
                content,
            ));
            top = top.saturating_add(slot);
        }
        rects
    }

    /// The item index under `point`, if any, for the given bounds. A point
    /// over an item omitted for lack of room answers [`None`].
    #[must_use]
    pub fn item_at(
        &self,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
        point: Point,
    ) -> Option<usize> {
        self.layout(bounds, scale, theme)
            .iter()
            .position(|r| r.contains(point))
    }

    /// The surface [`Rect`] item `index` occupies for the given bounds, or
    /// `None` when the index is out of range or was omitted for lack of room
    /// (fail closed). The forward mirror of [`item_at`](Self::item_at) over
    /// the same shared layout, so a caller that must aim *at* an item reads
    /// the exact rectangle [`render`](Self::render) paints.
    #[must_use]
    pub fn item_rect(
        &self,
        bounds: Rect,
        index: usize,
        scale: Scale,
        theme: &Theme,
    ) -> Option<Rect> {
        self.layout(bounds, scale, theme).get(index).copied()
    }

    /// Paint the rail into `surface` at `bounds` for the active theme: as
    /// many whole items as fit, each rendering itself, then the leading-edge
    /// Edge Wake when lit — painted last so it reads over every item.
    pub fn render(&self, surface: &mut Surface, bounds: Rect, scale: Scale, theme: &Theme) {
        let rects = self.layout(bounds, scale, theme);
        for (item, rect) in self.items.iter().zip(rects.iter()) {
            item.render(surface, *rect, scale, theme);
        }
        if self.edge_wake {
            paint_edge_wake(surface, bounds, scale, theme);
        }
    }

    /// Route a pointer event to the items it concerns and report the
    /// activation, if any, with the item it came from.
    ///
    /// One hit test decides where the pointer is; the event then reaches only
    /// the item it left, the item it entered, and any item holding a press.
    pub fn on_pointer(
        &mut self,
        event: &InputEvent,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
        damage: &mut Region,
    ) -> Option<RailAction> {
        if let InputEvent::PointerMoved { to } = event {
            *self.pointer = *to;
        }
        let rects = self.layout(bounds, scale, theme);
        let over = rects.iter().position(|r| r.contains(*self.pointer));
        let route = route_pointer(&mut self.hovered, *self.armed, over);
        *self.armed = grab_after(*self.armed, event, over);

        let mut fired = None;
        for index in route.into_iter().flatten() {
            let (Some(item), Some(rect)) = (self.items.get_mut(index), rects.get(index)) else {
                continue;
            };
            if item.on_pointer(event, *rect, damage).is_some() {
                fired = Some(RailAction::Activate { index });
            }
        }
        fired
    }

    /// Feed a key event: Up/Down move focus between items, clamping at the
    /// first and last item rather than wrapping (a rail is a fixed column of
    /// commands, not a cycling ring), Home/End jump to the ends, and an
    /// activation key (Space or Enter) is forwarded to the focused item,
    /// which decides for itself whether it may activate — a disabled or
    /// denied focused item consumes the key without acting, so the column's
    /// shape never shifts under the reader.
    pub fn on_key(&mut self, key: Key, bounds: Rect, damage: &mut Region) -> Option<RailAction> {
        if self.items.is_empty() {
            return None;
        }
        let last = self.items.len() - 1;
        match key {
            Key::Named(NamedKey::Down) => {
                let next = match self.focus {
                    Some(i) => (i + 1).min(last),
                    None => 0,
                };
                self.set_focus(Some(next), bounds, damage);
                None
            }
            Key::Named(NamedKey::Up) => {
                let prev = match self.focus {
                    Some(i) => i.saturating_sub(1),
                    None => 0,
                };
                self.set_focus(Some(prev), bounds, damage);
                None
            }
            Key::Named(NamedKey::Home) => {
                self.set_focus(Some(0), bounds, damage);
                None
            }
            Key::Named(NamedKey::End) => {
                self.set_focus(Some(last), bounds, damage);
                None
            }
            _ => {
                let i = self.focus?;
                let item = self.items.get_mut(i)?;
                item.on_key(key).map(|_| RailAction::Activate { index: i })
            }
        }
    }
}
