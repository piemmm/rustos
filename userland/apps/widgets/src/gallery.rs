//! The [`Gallery`]: the client-content composition the `widgets.app` bundle's
//! `Run` binary presents.
//!
//! The window's furniture (frame, title bar, command buttons) is drawn
//! server-side by the compositor, so the gallery renders only client content:
//! a [`Tabs`] strip selecting one control family and a panel of captioned demo
//! widgets for the selected family. Each family is one [`GalleryTab`]; each
//! panel is a column of [`DemoItem`]s laid out top-to-bottom, a caption on the
//! left and the live [`DemoWidget`] on the right. Pointer and key events are
//! routed to the tab strip or to the demo widget under focus; nothing here
//! performs privileged work.

use alloc::string::String;
use alloc::vec::Vec;

use tairix_controls::{Tab, Tabs, TabsAction};
use tairix_font::BitmapFont;
use tairix_geometry::{Point, Rect, Scale};
use tairix_input::{InputEvent, Key, Modifiers, PointerButton};
use tairix_raster::{Color, Surface};
use tairix_theme::Theme;

use crate::panels;
use crate::widget::DemoWidget;

/// One control family, shown on its own tab.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum GalleryTab {
    /// Push buttons: [`Button`](tairix_controls::Button),
    /// [`IconButton`](tairix_controls::IconButton),
    /// [`SplitButton`](tairix_controls::SplitButton).
    Buttons,
    /// Boolean selectors: toggle, checkbox, radio.
    Selectors,
    /// Value controls: slider, progress.
    Values,
    /// Text entries: text field, search field.
    Text,
    /// Choice controls: combo box, menu.
    Choice,
    /// Collection surfaces: list row, table row, card, panel.
    Collections,
    /// Bars: toolbar and scroll bars.
    Bars,
    /// Feedback surfaces: dialog, tooltip, help tip.
    Feedback,
    /// Window-manager furniture: the command buttons.
    Window,
}

impl GalleryTab {
    /// Every tab, in strip order.
    pub const ALL: [GalleryTab; 9] = [
        GalleryTab::Buttons,
        GalleryTab::Selectors,
        GalleryTab::Values,
        GalleryTab::Text,
        GalleryTab::Choice,
        GalleryTab::Collections,
        GalleryTab::Bars,
        GalleryTab::Feedback,
        GalleryTab::Window,
    ];

    /// This tab's zero-based index in [`Self::ALL`].
    #[must_use]
    pub fn index(self) -> usize {
        Self::ALL.iter().position(|&t| t == self).unwrap_or(0)
    }

    /// The tab at `index`, if in range.
    #[must_use]
    pub fn from_index(index: usize) -> Option<GalleryTab> {
        Self::ALL.get(index).copied()
    }

    /// The tab's strip label.
    #[must_use]
    pub fn title(self) -> &'static str {
        match self {
            GalleryTab::Buttons => "Buttons",
            GalleryTab::Selectors => "Selectors",
            GalleryTab::Values => "Values",
            GalleryTab::Text => "Text",
            GalleryTab::Choice => "Choice",
            GalleryTab::Collections => "Collections",
            GalleryTab::Bars => "Bars",
            GalleryTab::Feedback => "Feedback",
            GalleryTab::Window => "Window",
        }
    }
}

/// One demonstrated control in a panel: a caption, the live widget, and its
/// row height and optional fixed widget width in *logical* pixels (scaled at
/// layout time).
#[derive(Clone, Debug)]
pub struct DemoItem {
    /// The left-column caption naming the variation.
    pub caption: String,
    /// The live shared control.
    pub widget: DemoWidget,
    /// The row height in logical pixels.
    pub height: u32,
    /// A fixed widget width in logical pixels, or `None` to fill the row.
    pub width: Option<u32>,
}

impl DemoItem {
    /// A demo item filling the row width at the given logical height.
    #[must_use]
    pub fn new(caption: impl Into<String>, widget: DemoWidget, height: u32) -> Self {
        Self {
            caption: caption.into(),
            widget,
            height,
            width: None,
        }
    }

    /// This item with a fixed logical widget width instead of filling the row.
    #[must_use]
    pub fn with_width(mut self, width: u32) -> Self {
        self.width = Some(width);
        self
    }
}

/// The logical width of a panel's left caption column, in reference pixels.
const CAPTION_WIDTH: u32 = 168;

/// Which region of the gallery currently holds keyboard focus.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum Focus {
    /// The tab strip.
    Tabs,
    /// The demo item at this index in the current panel.
    Item(usize),
}

/// The widget-gallery client content: a tab strip plus the current family's
/// panel of demo widgets.
///
/// The gallery owns one panel of [`DemoItem`]s per [`GalleryTab`], built once
/// at construction. Rendering draws the tab strip and the selected panel;
/// input is routed to the tab strip or to the focused demo widget, and a demo
/// widget's value change is reflected straight back into it (the gallery is
/// the control's owner). Pointer position is tracked so a press/release routes
/// to the region under the pointer and a drag stays captured by the widget it
/// began on.
#[derive(Clone, Debug)]
pub struct Gallery {
    tabs: Tabs,
    panels: Vec<Vec<DemoItem>>,
    current: GalleryTab,
    focus: Focus,
    pointer: Point,
}

impl Default for Gallery {
    fn default() -> Self {
        Self::new()
    }
}

impl Gallery {
    /// Build the gallery with every family's panel populated and the first tab
    /// selected.
    #[must_use]
    pub fn new() -> Self {
        let mut tabs = Tabs::new(
            GalleryTab::ALL
                .iter()
                .map(|t| Tab::new(t.title()))
                .collect::<Vec<_>>(),
        );
        tabs.set_selected(0);
        let panels = GalleryTab::ALL.iter().map(|t| panels::build(*t)).collect();
        Self {
            tabs,
            panels,
            current: GalleryTab::Buttons,
            focus: Focus::Tabs,
            pointer: Point::ORIGIN,
        }
    }

    /// The currently selected tab.
    #[must_use]
    pub fn current_tab(&self) -> GalleryTab {
        self.current
    }

    /// The demo items of the currently selected panel.
    #[must_use]
    pub fn current_panel(&self) -> &[DemoItem] {
        &self.panels[self.current.index()]
    }

    /// The tab strip and content rectangles within `viewport`.
    fn layout(viewport: Rect, scale: Scale, theme: &Theme) -> (Rect, Rect) {
        let tab_h = scale
            .scale_length(theme.metrics().control_height)
            .max(1)
            .min(viewport.height);
        let tabs = Rect::new(viewport.left(), viewport.top(), viewport.width, tab_h);
        let content = Rect::new(
            viewport.left(),
            viewport.top() + i32::try_from(tab_h).unwrap_or(0),
            viewport.width,
            viewport.height.saturating_sub(tab_h),
        );
        (tabs, content)
    }

    /// The widget rectangle of each demo item in the current panel, laid out
    /// as a column within `content`. The caption occupies a fixed left column
    /// and the widget fills (or takes its fixed width in) the remainder.
    fn item_rects(&self, content: Rect, scale: Scale, theme: &Theme) -> Vec<Rect> {
        let pad = scale.scale_length(theme.metrics().control_inset).max(2);
        let gap = scale.scale_length(theme.metrics().control_gap).max(2);
        let caption_w = scale
            .scale_length(CAPTION_WIDTH)
            .min(content.width.saturating_sub(pad.saturating_mul(2)) / 2);
        let x0 = content.left() + i32::try_from(pad).unwrap_or(0);
        let wx = x0 + i32::try_from(caption_w + gap).unwrap_or(0);
        let right = content.right() - i32::try_from(pad).unwrap_or(0);
        let fill_w = u32::try_from((right - wx).max(0)).unwrap_or(0);
        let mut y = content.top() + i32::try_from(pad).unwrap_or(0);
        let mut rects = Vec::new();
        for item in &self.panels[self.current.index()] {
            let ih = scale.scale_length(item.height).max(1);
            let ww = item
                .width
                .map_or(fill_w, |w| scale.scale_length(w).min(fill_w));
            rects.push(Rect::new(wx, y, ww, ih));
            y += i32::try_from(ih + gap).unwrap_or(0);
        }
        rects
    }

    /// The caption rectangle (left column) aligned with widget `rect`.
    fn caption_rect(rect: Rect, content: Rect, scale: Scale, theme: &Theme) -> Rect {
        let pad = scale.scale_length(theme.metrics().control_inset).max(2);
        let caption_w = scale
            .scale_length(CAPTION_WIDTH)
            .min(content.width.saturating_sub(pad.saturating_mul(2)) / 2);
        Rect::new(
            content.left() + i32::try_from(pad).unwrap_or(0),
            rect.top(),
            caption_w,
            rect.height,
        )
    }

    /// Draw the gallery client content into `surface` filling `viewport`.
    pub fn render(
        &self,
        surface: &mut Surface,
        viewport: Rect,
        scale: Scale,
        theme: &Theme,
        font: BitmapFont,
    ) {
        let palette = theme.palette();
        surface.fill_rect(
            0,
            0,
            viewport.width,
            viewport.height,
            Color::from(palette.surface),
        );
        let (tabs_rect, content) = Self::layout(viewport, scale, theme);
        self.tabs.render(surface, tabs_rect, scale, theme);

        let rects = self.item_rects(content, scale, theme);
        let glyph_h = font.glyph_height();
        for (item, rect) in self.panels[self.current.index()].iter().zip(&rects) {
            let caption = Self::caption_rect(*rect, content, scale, theme);
            let text = font.truncate_to_width(&item.caption, caption.width);
            let ty = caption.top()
                + (i32::try_from(caption.height).unwrap_or(0)
                    - i32::try_from(glyph_h).unwrap_or(0))
                .max(0)
                    / 2;
            font.draw_text(
                surface,
                caption.left(),
                ty,
                text,
                Color::from(palette.on_surface),
            );
            item.widget.render(surface, *rect, scale, theme);
        }
    }

    /// Route one pointer event, returning whether the view should repaint.
    pub fn on_pointer(
        &mut self,
        event: &InputEvent,
        viewport: Rect,
        scale: Scale,
        theme: &Theme,
    ) -> bool {
        if let InputEvent::PointerMoved { to } = event {
            self.pointer = *to;
        }
        let (tabs_rect, content) = Self::layout(viewport, scale, theme);

        if tabs_rect.contains(self.pointer) {
            if let Some(TabsAction::Selected { index }) = self.tabs.on_pointer(event, tabs_rect) {
                return self.select_index(index);
            }
            return false;
        }

        let rects = self.item_rects(content, scale, theme);
        let hovered = rects.iter().position(|r| r.contains(self.pointer));
        if let InputEvent::PointerPressed {
            button: PointerButton::Primary,
        } = event
        {
            if let Some(idx) = hovered {
                self.set_focus(Focus::Item(idx));
            }
        }
        // The focused widget captures every event once a press has focused it
        // (so a drag that leaves its rect still reaches it); otherwise the
        // event — including the hover-sync move before a press — goes to the
        // widget under the pointer.
        let target = match self.focus {
            Focus::Item(idx) => Some(idx),
            Focus::Tabs => hovered,
        };
        if let Some(idx) = target {
            if let (Some(item), Some(rect)) = (
                self.panels[self.current.index()].get_mut(idx),
                rects.get(idx),
            ) {
                let changed = item.widget.on_pointer(event, *rect, scale, theme);
                if changed {
                    self.enforce_radio_group(idx);
                }
                return changed;
            }
        }
        false
    }

    /// Route one key press, returning whether the view should repaint. `Tab`
    /// and `Shift+Tab` move focus between the tab strip and the interactive
    /// demo widgets; every other key goes to the focused region.
    pub fn on_key(&mut self, key: Key, modifiers: Modifiers) -> bool {
        if key == Key::Named(tairix_input::NamedKey::Tab) {
            if modifiers.shift {
                self.focus_step(false);
            } else {
                self.focus_step(true);
            }
            return true;
        }
        match self.focus {
            Focus::Tabs => {
                if let Some(TabsAction::Selected { index }) = self.tabs.on_key(key) {
                    return self.select_index(index);
                }
                false
            }
            Focus::Item(idx) => {
                if let Some(item) = self.panels[self.current.index()].get_mut(idx) {
                    let changed = item.widget.on_key(key, modifiers);
                    if changed {
                        self.enforce_radio_group(idx);
                    }
                    return changed;
                }
                false
            }
        }
    }

    /// Select the tab at `index`, returning whether it changed.
    fn select_index(&mut self, index: usize) -> bool {
        let Some(tab) = GalleryTab::from_index(index) else {
            return false;
        };
        if tab == self.current {
            return false;
        }
        self.current = tab;
        self.tabs.set_selected(index);
        self.set_focus(Focus::Tabs);
        true
    }

    /// Move focus to `focus`, updating the widgets' and tab strip's focus
    /// marks so exactly one region reads as focused.
    fn set_focus(&mut self, focus: Focus) {
        for item in &mut self.panels[self.current.index()] {
            item.widget.set_focused(false);
        }
        self.tabs.set_current(None);
        self.focus = focus;
        match focus {
            Focus::Tabs => self.tabs.set_current(Some(self.current.index())),
            Focus::Item(idx) => {
                if let Some(item) = self.panels[self.current.index()].get_mut(idx) {
                    item.widget.set_focused(true);
                }
            }
        }
    }

    /// Advance keyboard focus forward (`true`) or backward (`false`) through
    /// the tab strip and the panel's interactive widgets, wrapping around.
    fn focus_step(&mut self, forward: bool) {
        let interactive: Vec<usize> = self.panels[self.current.index()]
            .iter()
            .enumerate()
            .filter(|(_, item)| item.widget.is_interactive())
            .map(|(i, _)| i)
            .collect();
        // The focus ring: the tab strip, then each interactive item in order.
        let current_pos = match self.focus {
            Focus::Tabs => 0,
            Focus::Item(idx) => interactive
                .iter()
                .position(|&i| i == idx)
                .map_or(0, |p| p + 1),
        };
        let ring_len = interactive.len() + 1;
        let next_pos = if forward {
            (current_pos + 1) % ring_len
        } else {
            (current_pos + ring_len - 1) % ring_len
        };
        let next = if next_pos == 0 {
            Focus::Tabs
        } else {
            Focus::Item(interactive[next_pos - 1])
        };
        self.set_focus(next);
    }

    /// The on-screen widget rectangle of demo item `index` in the current
    /// panel, for pointer-routing tests.
    #[cfg(test)]
    pub(crate) fn widget_rect_for_test(
        &self,
        index: usize,
        viewport: Rect,
        scale: Scale,
        theme: &Theme,
    ) -> Option<Rect> {
        let (_, content) = Self::layout(viewport, scale, theme);
        self.item_rects(content, scale, theme).get(index).copied()
    }

    /// Keep a radio group single-selection: if the item just actuated is a now
    /// selected radio, clear every other radio in the panel.
    fn enforce_radio_group(&mut self, idx: usize) {
        let panel = &mut self.panels[self.current.index()];
        if panel
            .get(idx)
            .is_some_and(|it| it.widget.is_selected_radio())
        {
            for (i, item) in panel.iter_mut().enumerate() {
                if i != idx {
                    item.widget.clear_radio();
                }
            }
        }
    }
}
