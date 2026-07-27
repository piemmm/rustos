//! The scrollbar renderer: the one orientation-parameterized [`ScrollBar`]
//! control (spec §11.28–§11.30). The `ScrollCorner` at a two-bar junction
//! (§11.31) is drawn by the window-manager furniture family.
//!
//! Vertical and horizontal scrollbars are a single behavioural component; the
//! [`ScrollOrientation`] only decides how the shared one-dimensional thumb math
//! ([`crate::scroll`]) maps onto the screen. This module is the *drawn* form of
//! that component: it lays a bar out into a decrement button, a track (before
//! thumb / thumb / after thumb), and an increment button; paints the quiet
//! Scroll Channel, the thumb, and the end-button chevrons from the active
//! [`Theme`] and [`Scale`]; classifies a pointer position; and maps pointer
//! drag, track paging, end-button line steps, wheel ticks, and arrow/page/home/
//! end keys back to a clamped offset through the shared [`ScrollModel`].
//!
//! The control does **not** keep a private offset beside the owning viewport:
//! it holds the viewport's own [`ScrollModel`] (refreshed by the owner through
//! [`ScrollBar::set_model`]), updates it immediately so a drag reads smoothly,
//! and reports the requested offset as a typed [`ScrollAction`] the owner
//! applies. Every result is clamped and every division guarded, so an empty,
//! non-scrollable, or degenerate bar simply does not move (fail closed).

use tairix_geometry::{Point, Rect, Scale};
use tairix_input::{InputEvent, Key, NamedKey, PointerButton};
use tairix_raster::{Color, Surface};
use tairix_theme::Theme;

use crate::paint::{draw_outline, heavy_contrast, paint_chevron, surface_rect, to_i32, ChevronDir};
use crate::scroll::{ScrollGeometry, ScrollModel, ScrollOrientation, ThumbSpan, TrackHit};
use crate::state::{ControlDisposition, ControlState, PointerState};

/// The outcome of interacting with a [`ScrollBar`].
///
/// A scrollbar updates its own held model immediately so a drag reads smoothly,
/// but the authoritative offset still lives in the owning viewport: the owner
/// receives the requested offset and applies it to the real content, then
/// refreshes the bar with [`ScrollBar::set_model`].
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ScrollAction {
    /// The bar requests its offset become `offset`, in the model's logical
    /// scroll unit.
    ScrollTo {
        /// The requested new offset (already clamped to the valid range).
        offset: u64,
    },
}

/// Which part of a scrollbar a pointer position falls in.
///
/// The two end buttons perform one line step each; the track regions before
/// and after the thumb perform one page step each; the thumb starts a drag.
/// This is the one classification paint, hit testing, and input all share, so
/// the mapping cannot diverge between them (spec §11.28).
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum ScrollPart {
    /// The decrement end button — one line step toward the start.
    Decrement,
    /// The increment end button — one line step toward the end.
    Increment,
    /// The track before the thumb — one page step toward the start.
    TrackBefore,
    /// The thumb itself — the start of a drag.
    Thumb,
    /// The track after the thumb — one page step toward the end.
    TrackAfter,
    /// Not over the bar at all.
    Outside,
}

/// The resolved on-screen anatomy of one scrollbar: the two end buttons, the
/// track between them, and the thumb geometry over that track.
///
/// Every rectangle is in surface coordinates. The `track_origin` is the track's
/// near edge along the bar's long axis, so a pointer's along-axis distance is a
/// single subtraction regardless of orientation.
struct BarLayout {
    orientation: ScrollOrientation,
    bar: Rect,
    decrement: Rect,
    increment: Rect,
    track: Rect,
    track_origin: i32,
    breadth: u32,
    geometry: ScrollGeometry,
}

impl BarLayout {
    /// The pointer's distance along the track's long axis from the track start.
    fn along(&self, point: Point) -> i32 {
        let coord = match self.orientation {
            ScrollOrientation::Vertical => point.y,
            ScrollOrientation::Horizontal => point.x,
        };
        coord - self.track_origin
    }

    /// Classify a surface `point` into a [`ScrollPart`].
    fn part_at(&self, point: Point) -> ScrollPart {
        if self.decrement.contains(point) {
            return ScrollPart::Decrement;
        }
        if self.increment.contains(point) {
            return ScrollPart::Increment;
        }
        if self.track.contains(point) {
            let along = u32::try_from(self.along(point)).unwrap_or(0);
            return match self.geometry.hit(along) {
                TrackHit::BeforeThumb => ScrollPart::TrackBefore,
                TrackHit::Thumb => ScrollPart::Thumb,
                TrackHit::AfterThumb => ScrollPart::TrackAfter,
            };
        }
        ScrollPart::Outside
    }

    /// The thumb's surface rectangle, inset from the channel walls by `margin`
    /// on the short axis so it reads as floating in the Scroll Channel.
    fn thumb_rect(&self, margin: u32) -> Rect {
        let ThumbSpan { start, length } = self.geometry.thumb();
        let short = self.breadth.saturating_sub(margin.saturating_mul(2)).max(1);
        let off = self.breadth.saturating_sub(short) / 2;
        match self.orientation {
            ScrollOrientation::Vertical => Rect::new(
                self.track.left() + to_i32(off),
                self.track_origin + to_i32(start),
                short,
                length,
            ),
            ScrollOrientation::Horizontal => Rect::new(
                self.track_origin + to_i32(start),
                self.track.top() + to_i32(off),
                length,
                short,
            ),
        }
    }
}

/// A window-manager or application scrollbar in either orientation (spec
/// §11.28–§11.30).
///
/// The bar is laid out into a decrement button, a track, and an increment
/// button; its thumb math comes from the shared [`crate::scroll`] engine so the
/// vertical and horizontal forms are one behaviour parameterised by
/// [`ScrollOrientation`]. It holds the owning viewport's [`ScrollModel`] (never
/// a private offset beside it) and reports every requested change as a typed
/// [`ScrollAction`]; a denied or disabled bar keeps its position and ignores
/// input rather than looking merely inert (spec §13).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct ScrollBar {
    orientation: ScrollOrientation,
    model: ScrollModel,
    state: ControlState,
    pointer: Point,
    dragging: bool,
    anchor: i32,
    held: Option<ScrollPart>,
}

impl ScrollBar {
    /// A scrollbar of the given orientation driven by `model`.
    #[must_use]
    pub fn new(orientation: ScrollOrientation, model: ScrollModel) -> Self {
        Self {
            orientation,
            model,
            state: ControlState::idle(),
            pointer: Point::ORIGIN,
            dragging: false,
            anchor: 0,
            held: None,
        }
    }

    /// The bar's orientation.
    #[must_use]
    pub fn orientation(&self) -> ScrollOrientation {
        self.orientation
    }

    /// The bar's current scroll model (the owning viewport's model).
    #[must_use]
    pub fn model(&self) -> ScrollModel {
        self.model
    }

    /// Replace the scroll model — what the owner calls after it applies a
    /// [`ScrollAction`] or when its content or client size changes, so the bar
    /// never keeps an offset separate from the viewport.
    pub fn set_model(&mut self, model: ScrollModel) {
        self.model = model;
    }

    /// The bar's composed control state.
    #[must_use]
    pub fn state(&self) -> ControlState {
        self.state
    }

    /// Replace the bar's composed state (e.g. an authority or pressure update).
    pub fn set_state(&mut self, state: ControlState) {
        self.state = state;
    }

    /// Set the bar's keyboard focus.
    pub fn set_focused(&mut self, focused: bool) {
        self.state.focus.focused = focused;
    }

    /// The part currently held for press-and-hold auto-repeat, if any.
    #[must_use]
    pub fn held(&self) -> Option<ScrollPart> {
        self.held
    }

    /// Whether a primary press is currently captured on the bar — a thumb
    /// drag in progress or an end/track button held down. An owner uses this
    /// to decide that a subsequent move or release belongs to the bar (the
    /// interaction the press started) rather than to the content beneath it.
    #[must_use]
    pub fn is_pressing(&self) -> bool {
        self.dragging || self.held.is_some()
    }

    /// Classify a surface `point` into a [`ScrollPart`] for the bar drawn at
    /// `bounds`, using the same layout as paint and input (spec §11.28). Off a
    /// present bar, or when the bar is degenerate, this is [`ScrollPart::Outside`].
    #[must_use]
    pub fn part_at(&self, bounds: Rect, point: Point, scale: Scale, theme: &Theme) -> ScrollPart {
        match self.layout(bounds, scale, theme) {
            Some(layout) => layout.part_at(point),
            None => ScrollPart::Outside,
        }
    }

    /// The scroll geometry for the bar drawn at `bounds`, or `None` when the
    /// bar is degenerate. Exposed so an owner (or a test) can reason about the
    /// thumb without re-deriving the layout.
    #[must_use]
    pub fn geometry(&self, bounds: Rect, scale: Scale, theme: &Theme) -> Option<ScrollGeometry> {
        self.layout(bounds, scale, theme).map(|l| l.geometry)
    }

    /// Whether the bar is drawn in its awake (brightened) form — being
    /// hovered, focused, dragged, or repeating an end/track press.
    fn is_active(&self) -> bool {
        self.dragging
            || self.held.is_some()
            || self.state.focus.focused
            || matches!(
                self.state.pointer,
                PointerState::Hover | PointerState::Pressed
            )
    }

    /// Apply `change` to the held model, returning the requested offset when it
    /// actually moved (fail closed: no movement means no action).
    fn apply(&mut self, change: impl FnOnce(ScrollModel) -> ScrollModel) -> Option<ScrollAction> {
        let before = self.model.offset();
        self.model = change(self.model);
        let after = self.model.offset();
        (after != before).then_some(ScrollAction::ScrollTo { offset: after })
    }

    /// Feed a pointer event for the bar drawn at `bounds`.
    ///
    /// An end-button press steps one line, a track press pages one step, and a
    /// thumb press captures a drag whose pointer-to-thumb anchor is preserved so
    /// the content does not jump; a subsequent move re-maps and re-clamps the
    /// offset (so a mid-drag range change stays valid). A denied, disabled,
    /// pending, or failed-closed bar ignores presses (fail closed).
    pub fn on_pointer(
        &mut self,
        event: &InputEvent,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
    ) -> Option<ScrollAction> {
        if let InputEvent::PointerMoved { to } = event {
            self.pointer = *to;
        }
        let layout = self.layout(bounds, scale, theme)?;
        match event {
            InputEvent::PointerMoved { .. } => {
                if self.dragging {
                    let offset = layout
                        .geometry
                        .offset_for_drag(layout.along(self.pointer), self.anchor);
                    self.apply(|m| m.scroll_to(offset))
                } else {
                    self.state.pointer = if layout.bar.contains(self.pointer) {
                        PointerState::Hover
                    } else {
                        PointerState::None
                    };
                    None
                }
            }
            InputEvent::PointerPressed {
                button: PointerButton::Primary,
            } => {
                if !self.state.is_actionable() {
                    return None;
                }
                match layout.part_at(self.pointer) {
                    ScrollPart::Decrement => {
                        self.held = Some(ScrollPart::Decrement);
                        self.state.pointer = PointerState::Pressed;
                        self.apply(ScrollModel::line_backward)
                    }
                    ScrollPart::Increment => {
                        self.held = Some(ScrollPart::Increment);
                        self.state.pointer = PointerState::Pressed;
                        self.apply(ScrollModel::line_forward)
                    }
                    ScrollPart::TrackBefore => {
                        self.held = Some(ScrollPart::TrackBefore);
                        self.state.pointer = PointerState::Pressed;
                        self.apply(ScrollModel::page_backward)
                    }
                    ScrollPart::TrackAfter => {
                        self.held = Some(ScrollPart::TrackAfter);
                        self.state.pointer = PointerState::Pressed;
                        self.apply(ScrollModel::page_forward)
                    }
                    ScrollPart::Thumb => {
                        if layout.geometry.draggable() {
                            self.dragging = true;
                            let thumb_start = to_i32(layout.geometry.thumb().start);
                            self.anchor = layout.along(self.pointer) - thumb_start;
                            self.state.pointer = PointerState::Pressed;
                        }
                        None
                    }
                    ScrollPart::Outside => None,
                }
            }
            InputEvent::PointerReleased {
                button: PointerButton::Primary,
            } => {
                self.dragging = false;
                self.held = None;
                self.state.pointer = if layout.bar.contains(self.pointer) {
                    PointerState::Hover
                } else {
                    PointerState::None
                };
                None
            }
            _ => None,
        }
    }

    /// Perform one more step of a held end-button or track press.
    ///
    /// Press-and-hold auto-repeat is driven by the owner's one-shot timer and
    /// event-driven wakeups (never a polling loop, spec §11.28): the owner arms
    /// a timer on the press and calls this on each wake while the button is
    /// held. It stops contributing when the offset reaches a bound (fail
    /// closed) or the press is released.
    pub fn repeat(&mut self) -> Option<ScrollAction> {
        if !self.state.is_actionable() {
            return None;
        }
        match self.held {
            Some(ScrollPart::Decrement) => self.apply(ScrollModel::line_backward),
            Some(ScrollPart::Increment) => self.apply(ScrollModel::line_forward),
            Some(ScrollPart::TrackBefore) => self.apply(ScrollModel::page_backward),
            Some(ScrollPart::TrackAfter) => self.apply(ScrollModel::page_forward),
            _ => None,
        }
    }

    /// Feed a key event to a focused, actionable bar: the orientation's arrow
    /// keys step one line, Page Up/Down step one page, and Home/End jump to the
    /// range bounds (spec §11.28).
    pub fn on_key(&mut self, key: Key) -> Option<ScrollAction> {
        if !self.state.focus.focused || !self.state.is_actionable() {
            return None;
        }
        let vertical = self.orientation == ScrollOrientation::Vertical;
        match key {
            Key::Named(NamedKey::PageUp) => self.apply(ScrollModel::page_backward),
            Key::Named(NamedKey::PageDown) => self.apply(ScrollModel::page_forward),
            Key::Named(NamedKey::Home) => self.apply(ScrollModel::to_start),
            Key::Named(NamedKey::End) => self.apply(ScrollModel::to_end),
            Key::Named(NamedKey::Up) if vertical => self.apply(ScrollModel::line_backward),
            Key::Named(NamedKey::Down) if vertical => self.apply(ScrollModel::line_forward),
            Key::Named(NamedKey::Left) if !vertical => self.apply(ScrollModel::line_backward),
            Key::Named(NamedKey::Right) if !vertical => self.apply(ScrollModel::line_forward),
            _ => None,
        }
    }

    /// Apply wheel `dx`/`dy` ticks to the bar (one line step per tick along its
    /// own axis), returning the requested offset when it moved. A denied or
    /// disabled bar ignores the wheel (fail closed).
    pub fn wheel(&mut self, dx: i32, dy: i32) -> Option<ScrollAction> {
        let ticks = match self.orientation {
            ScrollOrientation::Vertical => dy,
            ScrollOrientation::Horizontal => dx,
        };
        if ticks == 0 || !self.state.is_actionable() {
            return None;
        }
        self.apply(|m| {
            let step = i64::try_from(m.line_step()).unwrap_or(i64::MAX);
            m.scroll_by(i64::from(ticks).saturating_mul(step))
        })
    }

    /// Resolve the bar's on-screen anatomy for `bounds`, or `None` when it
    /// collapses. The end buttons are breadth-square (bounded so the track
    /// keeps at least a third of the long axis); the minimum thumb length is the
    /// theme metric scaled to physical pixels.
    fn layout(&self, bounds: Rect, scale: Scale, theme: &Theme) -> Option<BarLayout> {
        let (_, _, sw, sh) = surface_rect(bounds)?;
        if sw == 0 || sh == 0 {
            return None;
        }
        let min_thumb = scale.scale_length(theme.metrics().min_thumb_length).max(1);
        let (breadth, long) = match self.orientation {
            ScrollOrientation::Vertical => (sw, sh),
            ScrollOrientation::Horizontal => (sh, sw),
        };
        let button_len = breadth.min(long / 3);
        let track_len = long.saturating_sub(button_len.saturating_mul(2));
        let (decrement, increment, track, track_origin) = match self.orientation {
            ScrollOrientation::Vertical => (
                Rect::new(bounds.left(), bounds.top(), sw, button_len),
                Rect::new(
                    bounds.left(),
                    bounds.top() + to_i32(long - button_len),
                    sw,
                    button_len,
                ),
                Rect::new(
                    bounds.left(),
                    bounds.top() + to_i32(button_len),
                    sw,
                    track_len,
                ),
                bounds.top() + to_i32(button_len),
            ),
            ScrollOrientation::Horizontal => (
                Rect::new(bounds.left(), bounds.top(), button_len, sh),
                Rect::new(
                    bounds.left() + to_i32(long - button_len),
                    bounds.top(),
                    button_len,
                    sh,
                ),
                Rect::new(
                    bounds.left() + to_i32(button_len),
                    bounds.top(),
                    track_len,
                    sh,
                ),
                bounds.left() + to_i32(button_len),
            ),
        };
        Some(BarLayout {
            orientation: self.orientation,
            bar: Rect::new(bounds.left(), bounds.top(), sw, sh),
            decrement,
            increment,
            track,
            track_origin,
            breadth,
            geometry: ScrollGeometry::new(self.model.range(), track_len, min_thumb),
        })
    }

    /// Paint the bar into `surface` at `bounds` for the active theme.
    ///
    /// The Scroll Channel fills the whole bar; the thumb rounds through the
    /// shared raster fill and brightens to the reactive rim when the bar is
    /// awake; each end button draws its orientation-appropriate chevron,
    /// brightening when it is the pointer target or is held; a focused bar draws
    /// a reactive outline distinct from a hover. Content changes are never
    /// animated — the thumb follows the offset immediately — so the bar is
    /// already reduced-motion correct (spec §11.28).
    pub fn render(&self, surface: &mut Surface, bounds: Rect, scale: Scale, theme: &Theme) {
        let Some(layout) = self.layout(bounds, scale, theme) else {
            return;
        };
        let Some((bx, by, bw, bh)) = surface_rect(bounds) else {
            return;
        };
        let palette = theme.palette();
        surface.fill_rect(bx, by, bw, bh, Color::from(palette.scroll_track));

        let active = self.is_active();
        let disposition = self.state.disposition();
        let disabled = matches!(disposition, ControlDisposition::DisabledByState);
        let denied = matches!(disposition, ControlDisposition::DeniedByAuthority);

        // End-button chevrons.
        let chevron_idle = Color::from(if disabled {
            palette.border
        } else {
            palette.on_surface_muted
        });
        let chevron_hot = Color::from(palette.rim_active);
        let (dec_dir, inc_dir) = match self.orientation {
            ScrollOrientation::Vertical => (ChevronDir::Up, ChevronDir::Down),
            ScrollOrientation::Horizontal => (ChevronDir::Left, ChevronDir::Right),
        };
        let dec_hot = self.state.is_actionable()
            && (self.held == Some(ScrollPart::Decrement)
                || layout.decrement.contains(self.pointer));
        let inc_hot = self.state.is_actionable()
            && (self.held == Some(ScrollPart::Increment)
                || layout.increment.contains(self.pointer));
        paint_button(
            surface,
            layout.decrement,
            dec_dir,
            if dec_hot { chevron_hot } else { chevron_idle },
        );
        paint_button(
            surface,
            layout.increment,
            inc_dir,
            if inc_hot { chevron_hot } else { chevron_idle },
        );

        // Thumb.
        let draggable = layout.geometry.draggable();
        let thumb_color = if disabled {
            Color::from(palette.border)
        } else if denied {
            Color::from(palette.denied)
        } else if draggable && active {
            Color::from(palette.rim_active)
        } else {
            Color::from(palette.scroll_thumb)
        };
        let margin = (layout.breadth / 6).min(layout.breadth / 2);
        let thumb = layout.thumb_rect(margin);
        if let Some((tx, ty, tw, th)) = surface_rect(thumb) {
            if tw > 0 && th > 0 {
                let radius = (layout.breadth / 2).min(tw / 2).min(th / 2);
                surface.fill_round_rect(tx, ty, tw, th, radius, thumb_color);
                if heavy_contrast(theme) {
                    // A high-contrast theme rims the thumb so it reads without
                    // relying on the channel/thumb brightness difference alone.
                    draw_thumb_rim(surface, (tx, ty, tw, th), Color::from(palette.on_surface));
                }
            }
        }

        // Focus ring around the whole bar, distinct from a hover brighten.
        if self.state.focus.focused {
            let thickness = if heavy_contrast(theme) { 2 } else { 1 };
            draw_outline(
                surface,
                bx,
                by,
                bw,
                bh,
                thickness,
                Color::from(palette.rim_active),
            );
        }
    }
}

/// Paint one end button: its chevron centred in `rect`, inset so the glyph does
/// not touch the channel walls.
fn paint_button(surface: &mut Surface, rect: Rect, dir: ChevronDir, color: Color) {
    if rect.width == 0 || rect.height == 0 {
        return;
    }
    let pad = rect.width.min(rect.height) / 4;
    let inner = Rect::new(
        rect.left() + to_i32(pad),
        rect.top() + to_i32(pad),
        rect.width.saturating_sub(pad.saturating_mul(2)),
        rect.height.saturating_sub(pad.saturating_mul(2)),
    );
    paint_chevron(surface, inner, dir, color);
}

/// Draw a one-pixel rim around a thumb rectangle (high-contrast only).
fn draw_thumb_rim(surface: &mut Surface, rect: (u32, u32, u32, u32), color: Color) {
    let (x, y, w, h) = rect;
    if w == 0 || h == 0 {
        return;
    }
    surface.fill_rect(x, y, w, 1, color);
    surface.fill_rect(x, y + h - 1, w, 1, color);
    surface.fill_rect(x, y, 1, h, color);
    surface.fill_rect(x + w - 1, y, 1, h, color);
}
