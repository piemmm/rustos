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

use alloc::vec::Vec;

use tairix_font::BitmapFont;
use tairix_geometry::{Point, Rect, Scale};
use tairix_input::{InputEvent, Key, NamedKey};
use tairix_raster::{Color, Surface};
use tairix_theme::Theme;

use crate::button::{IconButton, SplitAction, SplitButton};
use crate::paint::{heavy_contrast, plate_border, surface_rect, to_i32};
use crate::state::RenderInvariant;

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
/// Equal toolbars draw the same pixels, so a host may use `==` as its repaint
/// gate: the tools with their own hover/press/active state, their group ids,
/// and the keyboard focus index all compare. The pointer coordinate does not
/// — the strip only forwards it to the tool it lands on, and no render path
/// reads it.
#[derive(Clone, Debug, Eq, PartialEq, Default)]
pub struct Toolbar {
    entries: Vec<Entry>,
    focus: Option<usize>,
    /// The last pointer position, forwarded to the tool it falls on —
    /// hit-testing input, never drawn.
    pointer: RenderInvariant<Point>,
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
    pub fn set_focus(&mut self, index: Option<usize>) {
        let index = index.filter(|&i| i < self.entries.len());
        self.focus = index;
        for (i, entry) in self.entries.iter_mut().enumerate() {
            entry.tool.set_focused(Some(i) == index);
        }
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

    /// The layout of the tools: one surface [`Rect`] per tool, plus the
    /// surface-x of each group divider. Tools of height `control_height` are
    /// centred vertically in the strip; a gap separates adjacent tools and a
    /// wider gutter (with a divider) separates groups.
    fn layout(&self, bounds: Rect, scale: Scale, theme: &Theme) -> (Vec<Rect>, Vec<i32>) {
        let metrics = theme.metrics();
        let ch = scale.scale_length(metrics.control_height).max(1);
        let gap = scale.scale_length(metrics.control_gap).max(1);
        let mut rects = Vec::with_capacity(self.entries.len());
        let mut dividers = Vec::new();
        let y = bounds.top();
        let h = bounds.height;
        let tool_y = y + to_i32(h.saturating_sub(ch)) / 2;
        let mut x = bounds.left() + to_i32(gap);
        let mut prev_group: Option<u16> = None;
        for entry in &self.entries {
            let w = match entry.tool {
                Tool::Icon(_) => ch,
                Tool::Split(_) => ch.saturating_mul(2),
            };
            if let Some(pg) = prev_group {
                if pg != entry.group {
                    dividers.push(x + to_i32(gap) / 2);
                    x += to_i32(gap);
                }
            }
            rects.push(Rect::new(x, tool_y, w, ch));
            x += to_i32(w) + to_i32(gap);
            prev_group = Some(entry.group);
        }
        (rects, dividers)
    }

    /// The tool index under `point`, if any, for the given bounds.
    #[must_use]
    pub fn tool_at(
        &self,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
        point: Point,
    ) -> Option<usize> {
        let (rects, _) = self.layout(bounds, scale, theme);
        rects.iter().position(|r| r.contains(point))
    }

    /// The surface [`Rect`] tool `index` occupies for the given bounds, or
    /// `None` when `index` is out of range (fail closed). The forward mirror
    /// of [`tool_at`](Self::tool_at) over the one shared layout, so a caller
    /// that must aim *at* a tool (a test that clicks it) reads the same
    /// geometry paint and hit-test use, never a hand-copied position.
    #[must_use]
    pub fn tool_rect(
        &self,
        index: usize,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
    ) -> Option<Rect> {
        let (rects, _) = self.layout(bounds, scale, theme);
        rects.get(index).copied()
    }

    /// Paint the toolbar into `surface` at `bounds` for the active theme.
    pub fn render(
        &self,
        surface: &mut Surface,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
        font: BitmapFont,
    ) {
        let palette = theme.palette();
        if let Some((x, y, w, h)) = surface_rect(bounds) {
            if w > 0 && h > 0 {
                surface.fill_rect(x, y, w, h, Color::from(palette.surface_raised));
            }
        }

        let (rects, dividers) = self.layout(bounds, scale, theme);
        let border = plate_border(theme, scale);

        // Group gutters: a quiet vertical divider between groups.
        if let Some((_, y, _, h)) = surface_rect(bounds) {
            let pad = scale.scale_length(theme.metrics().control_inset).max(1);
            let inner_h = h.saturating_sub(pad.saturating_mul(2));
            for &dx in &dividers {
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

        for (entry, rect) in self.entries.iter().zip(rects.iter()) {
            match &entry.tool {
                Tool::Icon(b) => b.render(surface, *rect, scale, theme, font, None),
                Tool::Split(b) => b.render(surface, *rect, scale, theme, font),
            }
            if entry.active {
                Self::paint_active_seam(surface, *rect, scale, theme);
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

    /// Feed a pointer event to every tool (so hover state stays current) and
    /// report the activation, if any, with the tool it came from.
    pub fn on_pointer(
        &mut self,
        event: &InputEvent,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
    ) -> Option<ToolbarAction> {
        if let InputEvent::PointerMoved { to } = event {
            *self.pointer = *to;
        }
        let (rects, _) = self.layout(bounds, scale, theme);
        let mut fired = None;
        for (i, (entry, rect)) in self.entries.iter_mut().zip(rects.iter()).enumerate() {
            match &mut entry.tool {
                Tool::Icon(b) => {
                    if b.on_pointer(event, *rect).is_some() {
                        fired = Some(ToolbarAction {
                            index: i,
                            part: ToolActivation::Primary,
                        });
                    }
                }
                Tool::Split(b) => {
                    if let Some(part) = b.on_pointer(event, *rect, scale, theme) {
                        fired = Some(ToolbarAction {
                            index: i,
                            part: match part {
                                SplitAction::Primary => ToolActivation::Primary,
                                SplitAction::Disclosure => ToolActivation::Disclosure,
                            },
                        });
                    }
                }
            }
        }
        fired
    }

    /// Feed a key event: Left/Right move focus between tools (wrapping),
    /// Home/End jump to the ends, and Enter/Space activate the focused tool.
    pub fn on_key(&mut self, key: Key) -> Option<ToolbarAction> {
        if self.entries.is_empty() {
            return None;
        }
        let last = self.entries.len() - 1;
        match key {
            Key::Named(NamedKey::Right) => {
                let next = match self.focus {
                    Some(i) if i < last => i + 1,
                    _ => 0,
                };
                self.set_focus(Some(next));
                None
            }
            Key::Named(NamedKey::Left) => {
                let prev = match self.focus {
                    Some(0) | None => last,
                    Some(i) => i - 1,
                };
                self.set_focus(Some(prev));
                None
            }
            Key::Named(NamedKey::Home) => {
                self.set_focus(Some(0));
                None
            }
            Key::Named(NamedKey::End) => {
                self.set_focus(Some(last));
                None
            }
            _ => {
                let i = self.focus?;
                match &mut self.entries.get_mut(i)?.tool {
                    Tool::Icon(b) => b.on_key(key).map(|_| ToolbarAction {
                        index: i,
                        part: ToolActivation::Primary,
                    }),
                    Tool::Split(b) => b.on_key(key).map(|part| ToolbarAction {
                        index: i,
                        part: match part {
                            SplitAction::Primary => ToolActivation::Primary,
                            SplitAction::Disclosure => ToolActivation::Disclosure,
                        },
                    }),
                }
            }
        }
    }
}

impl Tool {
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
