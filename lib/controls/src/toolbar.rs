//! The toolbar / toolstrip: [`Toolbar`] (spec §11.11).
//!
//! A toolbar is a horizontal container for tool controls — [`IconButton`]s and
//! [`SplitButton`]s — arranged in groups. It draws the toolstrip background and
//! the quiet vertical gutter between groups, positions each tool, marks the
//! *active* tool with a persistent lower accent seam, and routes pointer and
//! keyboard input to the tool controls it owns (each tool's own Heat Seam,
//! Signal Bead, and pressure rail come from that tool's [`crate::button`]
//! state, so background work shows on the tool, not across the whole strip).
//! Activation is reported as a typed [`ToolbarAction`]; the toolbar enforces no
//! authority. Every metric resolves from the active
//! [`Theme`] and [`Scale`].
//!
//! A strip too narrow for its tools **scrolls** rather than running off its
//! own edge: it seats whole tools only, reserves one slot at each end for the
//! overflow affordances, and offsets in whole tools through the shared
//! [`crate::scroll`] engine. Nothing is ever painted or hit-tested outside the
//! bounds the owner gave.

use alloc::vec::Vec;

use tairix_geometry::{Point, Rect, Region, Scale};
use tairix_icon::{IconArtwork, IconRequest};
use tairix_input::{InputEvent, Key, NamedKey, PointerButton};
use tairix_raster::{Color, Surface};
use tairix_theme::Theme;

use crate::button::{IconButton, SplitAction, SplitButton};
use crate::damage;
use crate::paint::{
    grab_after, heavy_contrast, paint_chevron, plate_border, route_pointer, surface_rect, to_i32,
    withheld, ChevronDir,
};
use crate::scroll::{ScrollModel, ScrollRange};
use crate::state::{ControlState, RenderInvariant};

/// Which region of a tool an activation came from.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ToolActivation {
    /// The tool's primary action (an icon button, or a split button's primary
    /// region).
    Primary,
    /// A split-button tool's disclosure region.
    Disclosure,
}

/// The outcome of feeding input to a [`Toolbar`].
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct ToolbarAction {
    /// The zero-based index of the activated tool.
    pub index: usize,
    /// Which region of the tool fired.
    pub part: ToolActivation,
}

/// What feeding input to a [`Toolbar`] did.
///
/// A scroll is reported apart from an activation because the owner owes a
/// repaint for it while running no command: the tools the strip shows have
/// moved, and nothing was chosen. [`Redraw`](Self::Redraw) covers every other
/// drawn change too — a hover arriving or leaving, a press latching — so an
/// owner that presents on this alone never leaves a lit tool unpainted.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ToolbarOutcome {
    /// Nothing drawn changed and nothing fired.
    Idle,
    /// Something drawn changed — a hover, a press, or a scroll — and nothing
    /// fired.
    Redraw,
    /// The tool the action names fired.
    Activated(ToolbarAction),
}

/// A drawn-change flag as an outcome, so the two spellings of "nothing
/// fired" stay one decision.
fn outcome(redrew: bool) -> ToolbarOutcome {
    if redrew {
        ToolbarOutcome::Redraw
    } else {
        ToolbarOutcome::Idle
    }
}

/// Which way an overflow affordance scrolls the strip.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum Overflow {
    /// Toward the leading edge — the tools before the first seated one.
    Back,
    /// Toward the trailing edge.
    Forward,
}

/// One tool control hosted by a toolbar.
#[derive(Clone, Debug, Eq, PartialEq)]
enum Tool {
    /// A single-glyph action.
    Icon(IconButton),
    /// A primary action plus a disclosure.
    Split(SplitButton),
}

/// One entry in a toolbar: a tool, the group it belongs to, and whether it is
/// the currently active tool (persistent accent seam).
#[derive(Clone, Debug, Eq, PartialEq)]
struct Entry {
    tool: Tool,
    group: u16,
    active: bool,
}

/// A horizontal container for tool controls arranged in groups (spec §11.11).
///
/// Tools are added with [`Toolbar::with_icon`] / [`Toolbar::with_split`], each
/// tagged with a `u16` group id; adjacent tools with different group ids are
/// separated by a quiet gutter and divider. The active tool (set with
/// [`Toolbar::set_active`]) carries a persistent lower accent seam. Keyboard
/// focus moves between tools with Left/Right (Home/End to the ends) and
/// Enter/Space activates the focused tool.
///
/// A strip with no room for every tool scrolls in whole tools: the offset is
/// held here and clamped through the shared [`ScrollModel`], and the two
/// overflow affordances step it (a held press auto-repeats through
/// [`Toolbar::repeat`], driven by the owner's one-shot timer).
///
/// Equal toolbars draw the same pixels, so a host may use `==` as its repaint
/// gate: the tools with their own hover/press/active state, their group ids,
/// the keyboard focus index, and the scroll offset all compare. The pointer
/// coordinate does not — the strip only forwards it to the tool it lands on,
/// and no render path reads it.
#[derive(Clone, Debug, Eq, PartialEq, Default)]
pub struct Toolbar {
    entries: Vec<Entry>,
    focus: Option<usize>,
    /// The first tool the strip shows — drawn, so it compares. Clamped on
    /// every layout against the tools the current bounds can seat, so a
    /// narrowed strip never holds an offset past its own end.
    offset: u64,
    /// The last pointer position, forwarded to the tool it falls on —
    /// hit-testing input, never drawn.
    pointer: RenderInvariant<Point>,
    /// The tool the pointer was last over, so a motion sample reaches the
    /// tool it left and the one it entered rather than the whole strip. It
    /// mirrors the hover the tools themselves carry, so nothing is drawn from
    /// it.
    hovered: RenderInvariant<Option<usize>>,
    /// The tool holding a press, which keeps receiving the stream wherever
    /// the pointer goes — bookkeeping, never drawn.
    armed: RenderInvariant<Option<usize>>,
    /// The overflow affordance holding a press, so the owner's timer can step
    /// it again. An affordance draws the same pressed or not, so this is
    /// bookkeeping rather than a drawn field.
    held: RenderInvariant<Option<Overflow>>,
}

impl Toolbar {
    /// An empty toolbar.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// This toolbar with an icon-button tool appended to `group`.
    #[must_use]
    pub fn with_icon(mut self, button: IconButton, group: u16) -> Self {
        self.entries.push(Entry {
            tool: Tool::Icon(button),
            group,
            active: false,
        });
        self
    }

    /// This toolbar with a split-button tool appended to `group`.
    #[must_use]
    pub fn with_split(mut self, button: SplitButton, group: u16) -> Self {
        self.entries.push(Entry {
            tool: Tool::Split(button),
            group,
            active: false,
        });
        self
    }

    /// The number of tools.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the toolbar has no tools.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Whether tool `index` is the active tool.
    #[must_use]
    pub fn is_active(&self, index: usize) -> bool {
        self.entries.get(index).is_some_and(|e| e.active)
    }

    /// Mark tool `index` as the active tool and clear the flag from the
    /// others; an out-of-range index clears every active mark (fail closed).
    pub fn set_active(&mut self, index: usize) {
        for (i, entry) in self.entries.iter_mut().enumerate() {
            entry.active = i == index;
        }
    }

    /// The focused tool, if any.
    #[must_use]
    pub fn focused(&self) -> Option<usize> {
        self.focus
    }

    /// Focus tool `index` (or clear focus with `None`), updating each tool's
    /// own focus flag so its focus ring draws; an out-of-range index clears
    /// focus (fail closed).
    ///
    /// A focus move reports the whole bar: the ring leaves one tool and
    /// arrives at another, and covering the strip over-covers by the tools
    /// that did not change, which repaints correctly. A strip that had no
    /// room to seat the newly focused tool scrolls it into view, so the ring
    /// is never left on a tool nothing drew.
    pub fn set_focus(
        &mut self,
        index: Option<usize>,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
        damage: &mut Region,
    ) {
        let index = index.filter(|&i| i < self.entries.len());
        damage::set(&mut self.focus, index, bounds, damage);
        for (i, entry) in self.entries.iter_mut().enumerate() {
            entry.tool.set_focused(Some(i) == index);
        }
        if let Some(target) = index {
            self.reveal(target, bounds, scale, theme, damage);
        }
    }

    /// Scroll tool `target` into view, if the strip had no room to seat it.
    ///
    /// The nearest offset that shows it: its own index when it sits before
    /// the band, and the first offset forward whose band reaches it
    /// otherwise. The seating depends on the widths in between, so the walk
    /// asks the seating rather than subtracting a tool count that is not
    /// constant. A tool the strip already shows moves nothing.
    fn reveal(
        &mut self,
        target: usize,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
        damage: &mut Region,
    ) {
        let measured = self.measure(bounds, scale, theme);
        let offset = usize::try_from(measured.model.offset()).unwrap_or(0);
        if measured.shows(offset, target) {
            return;
        }
        let wanted = if target < offset {
            target
        } else {
            let last = usize::try_from(measured.model.range().max_offset()).unwrap_or(0);
            (offset..=target.min(last))
                .find(|&candidate| measured.shows(candidate, target))
                .unwrap_or(last)
        };
        self.scroll_to(measured.model.scroll_to(wanted as u64), bounds, damage);
    }

    /// Mutable access to an icon-button tool, if the tool at `index` is one
    /// (e.g. to update its activity/pressure/authority state from a model).
    pub fn icon_mut(&mut self, index: usize) -> Option<&mut IconButton> {
        match self.entries.get_mut(index).map(|e| &mut e.tool) {
            Some(Tool::Icon(b)) => Some(b),
            _ => None,
        }
    }

    /// Mutable access to a split-button tool, if the tool at `index` is one.
    pub fn split_mut(&mut self, index: usize) -> Option<&mut SplitButton> {
        match self.entries.get_mut(index).map(|e| &mut e.tool) {
            Some(Tool::Split(b)) => Some(b),
            _ => None,
        }
    }

    /// Each tool's own width and the gutter charged before it when it follows
    /// a tool of another group.
    ///
    /// Every width the strip reasons about — what it needs, what one band can
    /// seat, where each seated tool sits — is summed from this one list, so
    /// the natural width and the seating can never measure a strip
    /// differently.
    fn spans(&self, slot: u32, gap: u32) -> Vec<Span> {
        let mut spans = Vec::with_capacity(self.entries.len());
        let mut prev: Option<u16> = None;
        for entry in &self.entries {
            spans.push(Span {
                width: match entry.tool {
                    Tool::Icon(_) => slot,
                    Tool::Split(_) => slot.saturating_mul(2),
                },
                gutter: if prev.is_some_and(|group| group != entry.group) {
                    gap
                } else {
                    0
                },
            });
            prev = Some(entry.group);
        }
        spans
    }

    /// The width the strip needs to seat **every** tool: the leading gap, each
    /// tool with the gap that follows it, and a gutter at each group boundary.
    ///
    /// What an owner floors a window on when its strip must never scroll, so
    /// the room the tools need is derived rather than hand-picked.
    #[must_use]
    pub fn natural_width(&self, scale: Scale, theme: &Theme) -> u32 {
        let (slot, gap) = slot_metrics(scale, theme);
        let spans = self.spans(slot, gap);
        span_of(&spans, 0, spans.len(), gap)
    }

    /// The narrowest strip that can still show something: the two reserved
    /// overflow slots plus the widest single tool in its own band.
    ///
    /// What an owner floors a window on when its strip is allowed to scroll —
    /// below this the affordances leave no room for a tool and the strip shows
    /// nothing at all.
    #[must_use]
    pub fn min_width(&self, scale: Scale, theme: &Theme) -> u32 {
        let (slot, gap) = slot_metrics(scale, theme);
        let widest = self
            .spans(slot, gap)
            .iter()
            .map(|span| span.width)
            .max()
            .unwrap_or(slot);
        slot.saturating_mul(2)
            .saturating_add(gap)
            .saturating_add(widest)
    }

    /// The scroll model over the tools for a strip drawn at `bounds`: how many
    /// tools there are, how many that band seats, and which is first.
    ///
    /// The unit is a tool, because that is what the strip scrolls in. Exposed
    /// so an owner (or a test) can reason about the offset without
    /// re-deriving the seating.
    #[must_use]
    pub fn scroll_model(&self, bounds: Rect, scale: Scale, theme: &Theme) -> ScrollModel {
        self.strip(bounds, scale, theme).model
    }

    /// The seating inputs for a strip drawn at `bounds`.
    ///
    /// A strip with room for all its tools takes it from the leading edge and
    /// reserves nothing. One without reserves a slot at each end — whether or
    /// not an affordance is currently drawn there — so scrolling moves the
    /// tools without also moving the band they sit in.
    fn measure(&self, bounds: Rect, scale: Scale, theme: &Theme) -> Measure {
        let (slot, gap) = slot_metrics(scale, theme);
        let spans = self.spans(slot, gap);
        let count = spans.len();
        let tool_y = bounds.top() + to_i32(bounds.height.saturating_sub(slot)) / 2;
        if span_of(&spans, 0, count, gap) <= bounds.width {
            return Measure {
                spans,
                band: Band {
                    x: bounds.left(),
                    width: bounds.width,
                    tool_y,
                    slot,
                    gap,
                },
                model: fixed_model(count),
                reserved: false,
            };
        }
        let band = Band {
            x: bounds.left().saturating_add(to_i32(slot)),
            width: bounds.width.saturating_sub(slot.saturating_mul(2)),
            tool_y,
            slot,
            gap,
        };
        // The least offset whose window still reaches the last tool. Each
        // tool added at the front costs its own width and leading gap plus
        // the gutter the tool it now precedes no longer starts a run with.
        let mut first = count;
        let mut span = 0u32;
        while let Some(added) = first.checked_sub(1).and_then(|i| {
            let grown = span
                .saturating_add(gap)
                .saturating_add(spans.get(i)?.width)
                .saturating_add(spans.get(first).map_or(0, |next| next.gutter));
            (grown <= band.width).then_some((i, grown))
        }) {
            (first, span) = added;
        }
        let shown = (count - first) as u64;
        Measure {
            spans,
            band,
            model: ScrollModel::new(ScrollRange::new(count as u64, shown, self.offset), 1, shown),
            reserved: true,
        }
    }

    /// Where each *drawn* tool sits, the group dividers between them, the two
    /// overflow affordances, and the scroll model over the tools.
    ///
    /// Every rectangle lies inside `bounds`. A tool the band has no room to
    /// seat whole gets no rectangle at all, so paint and hit-test agree by
    /// construction and neither reaches outside the strip.
    fn strip(&self, bounds: Rect, scale: Scale, theme: &Theme) -> Strip {
        let measured = self.measure(bounds, scale, theme);
        let offset = usize::try_from(measured.model.offset()).unwrap_or(0);
        let (seats, dividers) = measured.seats(offset);

        // Each affordance is drawn, and pressable, only where there is
        // something that way; a strip whose band seats nothing at all offers
        // neither, because stepping it could not help (fail closed).
        let count = measured.spans.len();
        let scrollable = measured.reserved && measured.model.range().is_scrollable();
        let last_seated = seats.iter().rposition(Option::is_some);
        let back = (scrollable && offset > 0).then(|| measured.band.leading_slot());
        let forward = (scrollable && last_seated.is_none_or(|last| last + 1 < count))
            .then(|| measured.band.trailing_slot());
        Strip {
            seats,
            dividers,
            back,
            forward,
            model: measured.model,
        }
    }

    /// The tool index under `point`, if any, for the given bounds. A point on
    /// an overflow affordance, or over a tool the strip had no room to seat,
    /// is on no tool.
    #[must_use]
    pub fn tool_at(
        &self,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
        point: Point,
    ) -> Option<usize> {
        self.strip(bounds, scale, theme).tool_at(point)
    }

    /// The surface [`Rect`] tool `index` occupies for the given bounds, or
    /// `None` when `index` is out of range or the strip had no room to seat it
    /// (fail closed). The forward mirror of [`tool_at`](Self::tool_at) over
    /// the one shared layout, so a caller that must aim *at* a tool (a test
    /// that clicks it) reads the same geometry paint and hit-test use, never a
    /// hand-copied position.
    #[must_use]
    pub fn tool_rect(
        &self,
        index: usize,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
    ) -> Option<Rect> {
        self.strip(bounds, scale, theme)
            .seats
            .get(index)
            .copied()
            .flatten()
    }

    /// Paint the toolbar into `surface` at `bounds` for the active theme.
    ///
    /// Each icon tool's picture is resolved through `artwork` at that tool's
    /// own icon side, so a toolbar of glyphs costs a cache lookup per tool
    /// rather than re-resolving vector coverage every frame. The lookup is
    /// taken inside the loop because a cache serves one borrow at a time.
    pub fn render(
        &self,
        surface: &mut Surface,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
        artwork: &mut dyn IconArtwork,
    ) {
        if withheld(surface, bounds) {
            return;
        }
        let palette = theme.palette();
        if let Some((x, y, w, h)) = surface_rect(bounds) {
            if w > 0 && h > 0 {
                surface.fill_rect(x, y, w, h, Color::from(palette.surface_raised));
            }
        }

        let strip = self.strip(bounds, scale, theme);
        let border = plate_border(theme, scale);

        // Group gutters: a quiet vertical divider between groups.
        if let Some((_, y, _, h)) = surface_rect(bounds) {
            let pad = scale.scale_length(theme.metrics().control_inset).max(1);
            let inner_h = h.saturating_sub(pad.saturating_mul(2));
            for &dx in &strip.dividers {
                if let Ok(dxu) = u32::try_from(dx) {
                    if inner_h > 0 {
                        surface.fill_rect(
                            dxu,
                            y + pad,
                            border.max(1),
                            inner_h,
                            Color::from(palette.border),
                        );
                    }
                }
            }
        }

        for (entry, rect) in self
            .entries
            .iter()
            .zip(strip.seats.iter())
            .filter_map(|(entry, seat)| seat.map(|rect| (entry, rect)))
        {
            match &entry.tool {
                Tool::Icon(b) => {
                    let side = b.icon_side(rect, scale, theme);
                    let picture = artwork.artwork(IconRequest::kind(b.icon()), side);
                    b.render(surface, rect, scale, theme, picture);
                }
                Tool::Split(b) => b.render(surface, rect, scale, theme),
            }
            if entry.active {
                Self::paint_active_seam(surface, rect, scale, theme);
            }
        }

        // The overflow chevrons — the same glyph the scrollbar's end buttons
        // draw, never a second icon — in the reserved slot each occupies.
        let chevron = Color::from(palette.on_surface_muted);
        for (rect, dir) in [
            (strip.back, ChevronDir::Left),
            (strip.forward, ChevronDir::Right),
        ] {
            if let Some(rect) = rect {
                paint_chevron(surface, rect, dir, chevron);
            }
        }
    }

    /// Paint the persistent active-tool lower accent seam under `rect`.
    fn paint_active_seam(surface: &mut Surface, rect: Rect, scale: Scale, theme: &Theme) {
        let Some((x, y, w, h)) = surface_rect(rect) else {
            return;
        };
        if w == 0 || h == 0 {
            return;
        }
        let seam_h = scale
            .scale_length(theme.metrics().seam_thickness)
            .max(1)
            .saturating_mul(if heavy_contrast(theme) { 2 } else { 1 })
            .min(h);
        surface.fill_rect(
            x,
            y + h - seam_h,
            w,
            seam_h,
            Color::from(theme.palette().accent),
        );
    }

    /// Route a pointer event to the tools and affordances it concerns, and
    /// report what it did.
    ///
    /// One hit test decides where the pointer is; the event then reaches only
    /// the tool it left, the tool it entered, and any tool holding a press.
    /// Every other tool is already at rest and would be written back the
    /// state it has. A press on an overflow affordance steps the strip by one
    /// tool and latches, so the owner's timer can repeat it
    /// ([`repeat`](Self::repeat)); the affordances sit in reserved room no
    /// tool occupies, so the two can never both claim one sample.
    pub fn on_pointer(
        &mut self,
        event: &InputEvent,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
        damage: &mut Region,
    ) -> ToolbarOutcome {
        if let InputEvent::PointerMoved { to } = event {
            *self.pointer = *to;
        }
        let strip = self.strip(bounds, scale, theme);
        let at = *self.pointer;
        match event {
            InputEvent::PointerPressed {
                button: PointerButton::Primary,
            } => {
                if let Some(end) = strip.overflow_at(at) {
                    *self.held = Some(end);
                    return outcome(self.step(end, &strip.model, bounds, damage));
                }
            }
            InputEvent::PointerReleased {
                button: PointerButton::Primary,
            } => *self.held = None,
            _ => {}
        }

        self.route_tools(event, &strip, scale, theme, damage)
    }

    /// Deliver `event` to the tools it concerns: the one the pointer left, the
    /// one it entered, and any holding a press.
    fn route_tools(
        &mut self,
        event: &InputEvent,
        strip: &Strip,
        scale: Scale,
        theme: &Theme,
        damage: &mut Region,
    ) -> ToolbarOutcome {
        let over = strip.tool_at(*self.pointer);
        let route = route_pointer(&mut self.hovered, *self.armed, over);
        *self.armed = grab_after(*self.armed, event, over);

        let mut fired = None;
        let mut redrew = false;
        for i in route.into_iter().flatten() {
            let Some(entry) = self.entries.get_mut(i) else {
                continue;
            };
            let before = entry.tool.drawn_state();
            // A tool the strip has scrolled away from is not under the
            // pointer and cannot be holding a press: a release off its
            // absent rectangle puts it back at rest and activates nothing, so
            // it neither stays lit nor misfires if it comes back.
            let (delivered, at) = match strip.seats.get(i).copied().flatten() {
                Some(rect) => (event, rect),
                None => (&RELEASE_OFF, Rect::EMPTY),
            };
            let part = entry.tool.deliver(delivered, at, scale, theme, damage);
            redrew |= entry.tool.drawn_state() != before;
            if let Some(part) = part {
                fired = Some(ToolbarAction { index: i, part });
            }
        }
        match fired {
            Some(action) => ToolbarOutcome::Activated(action),
            None => outcome(redrew),
        }
    }

    /// Apply wheel `dx`/`dy` ticks over the strip, one tool per tick along its
    /// own axis, answering whether the tools it shows moved.
    ///
    /// A strip that shows every tool it has ignores the wheel (fail closed: no
    /// movement, no repaint).
    pub fn wheel(
        &mut self,
        dx: i32,
        dy: i32,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
        damage: &mut Region,
    ) -> bool {
        // A horizontal strip answers a horizontal wheel first and a vertical
        // one where the pointer has no sideways axis to offer.
        let ticks = if dx != 0 { dx } else { dy };
        if ticks == 0 {
            return false;
        }
        let model = self.strip(bounds, scale, theme).model;
        if !self.scroll_to(model.scroll_by(i64::from(ticks)), bounds, damage) {
            return false;
        }
        // Different tools now sit under the pointer, so the hover is
        // re-derived from where it actually is rather than left lit on the
        // tool that scrolled away.
        let settled = self.strip(bounds, scale, theme);
        let at = *self.pointer;
        self.route_tools(
            &InputEvent::PointerMoved { to: at },
            &settled,
            scale,
            theme,
            damage,
        );
        true
    }

    /// Perform one more step of a held overflow press, answering whether the
    /// tools the strip shows moved.
    ///
    /// Press-and-hold auto-repeat is driven by the owner's one-shot timer and
    /// event-driven wakeups, never a polling loop: the owner arms a timer on
    /// the press ([`crate::scroll::REPEAT_DELAY_NS`], then
    /// [`crate::scroll::REPEAT_INTERVAL_NS`]) and calls this on each wake
    /// while [`is_repeating`](Self::is_repeating) holds. It stops contributing
    /// when the offset reaches a bound.
    pub fn repeat(
        &mut self,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
        damage: &mut Region,
    ) -> bool {
        let Some(end) = *self.held else {
            return false;
        };
        let model = self.strip(bounds, scale, theme).model;
        self.step(end, &model, bounds, damage)
    }

    /// Whether an overflow affordance is held down, so the owner owes this
    /// strip a repeat wake-up.
    #[must_use]
    pub fn is_repeating(&self) -> bool {
        self.held.is_some()
    }

    /// Step the offset one tool toward `end`.
    fn step(
        &mut self,
        end: Overflow,
        model: &ScrollModel,
        bounds: Rect,
        damage: &mut Region,
    ) -> bool {
        let moved = match end {
            Overflow::Back => model.line_backward(),
            Overflow::Forward => model.line_forward(),
        };
        self.scroll_to(moved, bounds, damage)
    }

    /// Adopt `model`'s offset, reporting the strip when it moved: every tool
    /// shifts, so the change is the strip's and not one tool's.
    fn scroll_to(&mut self, model: ScrollModel, bounds: Rect, damage: &mut Region) -> bool {
        damage::set(&mut self.offset, model.offset(), bounds, damage)
    }

    /// The fan-to-all delivery [`on_pointer`](Self::on_pointer) replaced: every
    /// tool receives every event and hit-tests it itself.
    ///
    /// Kept as the oracle the routed path is measured against — a scripted
    /// pointer path must leave the two indistinguishable, which is what makes
    /// routing an optimisation rather than a behaviour change.
    #[cfg(test)]
    pub(crate) fn fan_pointer(
        &mut self,
        event: &InputEvent,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
        damage: &mut Region,
    ) -> ToolbarOutcome {
        if let InputEvent::PointerMoved { to } = event {
            *self.pointer = *to;
        }
        let strip = self.strip(bounds, scale, theme);
        let mut fired = None;
        let mut redrew = false;
        for (i, (entry, rect)) in self
            .entries
            .iter_mut()
            .zip(strip.seats.iter())
            .enumerate()
            .filter_map(|(i, (entry, seat))| seat.map(|rect| (i, (entry, rect))))
        {
            let before = entry.tool.drawn_state();
            let part = entry.tool.deliver(event, rect, scale, theme, damage);
            redrew |= entry.tool.drawn_state() != before;
            if let Some(part) = part {
                fired = Some(ToolbarAction { index: i, part });
            }
        }
        match fired {
            Some(action) => ToolbarOutcome::Activated(action),
            None => outcome(redrew),
        }
    }

    /// Feed a key event: Left/Right move focus between tools (wrapping),
    /// Home/End jump to the ends, and Enter/Space activate the focused tool.
    ///
    /// A focus move scrolls the tool it lands on into view, so the keyboard
    /// reaches every tool however narrow the strip is.
    pub fn on_key(
        &mut self,
        key: Key,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
        damage: &mut Region,
    ) -> ToolbarOutcome {
        if self.entries.is_empty() {
            return ToolbarOutcome::Idle;
        }
        let last = self.entries.len() - 1;
        let moved_to = match key {
            Key::Named(NamedKey::Right) => Some(match self.focus {
                Some(i) if i < last => i + 1,
                _ => 0,
            }),
            Key::Named(NamedKey::Left) => Some(match self.focus {
                Some(0) | None => last,
                Some(i) => i - 1,
            }),
            Key::Named(NamedKey::Home) => Some(0),
            Key::Named(NamedKey::End) => Some(last),
            _ => None,
        };
        if let Some(next) = moved_to {
            self.set_focus(Some(next), bounds, scale, theme, damage);
            return ToolbarOutcome::Redraw;
        }
        let Some(i) = self.focus else {
            return ToolbarOutcome::Idle;
        };
        let Some(entry) = self.entries.get_mut(i) else {
            return ToolbarOutcome::Idle;
        };
        let before = entry.tool.drawn_state();
        let part = match &mut entry.tool {
            Tool::Icon(b) => b.on_key(key).map(|_| ToolActivation::Primary),
            Tool::Split(b) => b.on_key(key).map(|part| match part {
                SplitAction::Primary => ToolActivation::Primary,
                SplitAction::Disclosure => ToolActivation::Disclosure,
            }),
        };
        let redrew = entry.tool.drawn_state() != before;
        match part {
            Some(part) => ToolbarOutcome::Activated(ToolbarAction { index: i, part }),
            None => outcome(redrew),
        }
    }
}

/// One tool's contribution to the strip's width.
#[derive(Copy, Clone)]
struct Span {
    /// The tool's own width: one slot for an icon, two for a split.
    width: u32,
    /// The extra gap charged before it when it follows a tool of another
    /// group, and where a divider is drawn.
    gutter: u32,
}

/// A run of the strip tools are seated into, and the placement they share.
struct Band {
    x: i32,
    width: u32,
    tool_y: i32,
    slot: u32,
    gap: u32,
}

impl Band {
    /// The reserved slot before the band — where the leading affordance is
    /// drawn and pressed.
    fn leading_slot(&self) -> Rect {
        Rect::new(
            self.x.saturating_sub(to_i32(self.slot)),
            self.tool_y,
            self.slot,
            self.slot,
        )
    }

    /// The reserved slot after the band.
    fn trailing_slot(&self) -> Rect {
        Rect::new(
            self.x.saturating_add(to_i32(self.width)),
            self.tool_y,
            self.slot,
            self.slot,
        )
    }
}

/// The seating inputs for one strip at one bounds: each tool's span, the band
/// the tools are seated into, the offset clamped against what that band can
/// show, and whether the overflow slots are reserved.
struct Measure {
    spans: Vec<Span>,
    band: Band,
    model: ScrollModel,
    reserved: bool,
}

impl Measure {
    /// Seat the tools from `start`, answering where each drawn one sits and
    /// the dividers between them.
    fn seats(&self, start: usize) -> (Vec<Option<Rect>>, Vec<i32>) {
        let mut seats = alloc::vec![None; self.spans.len()];
        let dividers = seat(&self.spans, &self.band, start, &mut seats);
        (seats, dividers)
    }

    /// Whether seating from `start` shows tool `target` whole.
    fn shows(&self, start: usize, target: usize) -> bool {
        self.seats(start).0.get(target).copied().flatten().is_some()
    }
}

/// A seated strip: where each drawn tool sits, the dividers between them, the
/// affordances actually offered, and the clamped scroll model over the tools.
struct Strip {
    /// One entry per tool, `None` for a tool the strip had no room to seat.
    seats: Vec<Option<Rect>>,
    dividers: Vec<i32>,
    back: Option<Rect>,
    forward: Option<Rect>,
    model: ScrollModel,
}

impl Strip {
    /// The tool `point` falls on, if any.
    fn tool_at(&self, point: Point) -> Option<usize> {
        self.seats
            .iter()
            .position(|seat| seat.is_some_and(|rect| rect.contains(point)))
    }

    /// The affordance `point` falls on, if any.
    fn overflow_at(&self, point: Point) -> Option<Overflow> {
        if self.back.is_some_and(|rect| rect.contains(point)) {
            return Some(Overflow::Back);
        }
        self.forward
            .is_some_and(|rect| rect.contains(point))
            .then_some(Overflow::Forward)
    }
}

/// The slot side and gap every strip measurement resolves from.
fn slot_metrics(scale: Scale, theme: &Theme) -> (u32, u32) {
    let metrics = theme.metrics();
    (
        scale.scale_length(metrics.control_height).max(1),
        scale.scale_length(metrics.control_gap).max(1),
    )
}

/// The width the tools `start..end` claim from a band's leading edge: the
/// leading gap, each tool with the gap before the next, and a gutter wherever
/// the group changes inside the run. Zero for an empty run.
fn span_of(spans: &[Span], start: usize, end: usize, gap: u32) -> u32 {
    let Some(run) = spans.get(start..end) else {
        return 0;
    };
    let mut total = 0u32;
    for (offset, span) in run.iter().enumerate() {
        total = total.saturating_add(gap);
        if offset > 0 {
            total = total.saturating_add(span.gutter);
        }
        total = total.saturating_add(span.width);
    }
    total
}

/// Seat the tools from `start` into `band`, in order, stopping at the first
/// one that would not fit whole. Answers the surface-x of each divider drawn
/// between two seated tools of different groups.
fn seat(spans: &[Span], band: &Band, start: usize, seats: &mut [Option<Rect>]) -> Vec<i32> {
    let mut dividers = Vec::new();
    let Some(run) = spans.get(start..) else {
        return dividers;
    };
    let right = band.x.saturating_add(to_i32(band.width));
    let mut x = band.x.saturating_add(to_i32(band.gap));
    for (offset, span) in run.iter().enumerate() {
        // The first seated tool starts its own run, so it is charged no
        // gutter however its group compares with the tool scrolled past.
        let gutter = if offset == 0 { 0 } else { span.gutter };
        let at = x.saturating_add(to_i32(gutter));
        if at.saturating_add(to_i32(span.width)) > right {
            break;
        }
        if gutter > 0 {
            dividers.push(x.saturating_add(to_i32(band.gap) / 2));
        }
        if let Some(seat) = seats.get_mut(start.saturating_add(offset)) {
            *seat = Some(Rect::new(at, band.tool_y, span.width, band.slot));
        }
        x = at
            .saturating_add(to_i32(span.width))
            .saturating_add(to_i32(band.gap));
    }
    dividers
}

/// The model of a strip showing every tool it has: never scrollable, so no
/// offset can be held against it.
fn fixed_model(count: usize) -> ScrollModel {
    ScrollModel::new(
        ScrollRange::new(count as u64, count as u64, 0),
        1,
        count as u64,
    )
}

/// A primary release, used to put a tool the strip has scrolled away from
/// back at rest: delivered against an empty rectangle it cancels the press
/// and activates nothing.
const RELEASE_OFF: InputEvent = InputEvent::PointerReleased {
    button: PointerButton::Primary,
};

impl Tool {
    /// Feed one pointer event to the tool at `rect`, reporting which of its
    /// regions fired.
    ///
    /// One dispatch over the two tool kinds, so the routed path and the
    /// scrolled-away path cannot deliver differently.
    fn deliver(
        &mut self,
        event: &InputEvent,
        rect: Rect,
        scale: Scale,
        theme: &Theme,
        damage: &mut Region,
    ) -> Option<ToolActivation> {
        match self {
            Tool::Icon(b) => b
                .on_pointer(event, rect, damage)
                .map(|_| ToolActivation::Primary),
            Tool::Split(b) => {
                b.on_pointer(event, rect, scale, theme, damage)
                    .map(|part| match part {
                        SplitAction::Primary => ToolActivation::Primary,
                        SplitAction::Disclosure => ToolActivation::Disclosure,
                    })
            }
        }
    }

    /// The tool's own drawn state, so the strip can tell a sample that lit or
    /// unlit something from one that wrote back the state already there.
    fn drawn_state(&self) -> (ControlState, ControlState) {
        match self {
            Tool::Icon(b) => (b.state(), b.state()),
            Tool::Split(b) => (b.primary_state(), b.disclosure_state()),
        }
    }

    /// Set the tool's keyboard focus (on its primary region for a split tool).
    fn set_focused(&mut self, focused: bool) {
        match self {
            Tool::Icon(b) => b.set_focused(focused),
            Tool::Split(b) => {
                let mut s = b.primary_state();
                s.focus.focused = focused;
                b.set_primary_state(s);
            }
        }
    }
}
