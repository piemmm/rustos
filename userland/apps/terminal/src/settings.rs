//! The in-window settings sheet: a modal panel the terminal draws over its
//! own screen, built from the shared Reactive Alloy controls
//! (`plans/GUI-CONTROLS-DESIGN.md`).
//!
//! The sheet edits a copy of the [`Profile`] it opened on and never touches
//! the caller's own copy: [`Settings::profile`] hands back the edited,
//! always-clamped result, and the caller re-resolves colours, re-derives the
//! font, repaints, and persists once it sees [`SheetOutcome::Edited`].
//!
//! # Layout
//!
//! The body of each tab is an ordered list of rows (a scheme choice, the text
//! size, the custom-scheme swatch grid, a channel slider, an effect slider),
//! laid out top to bottom at the theme's control height and gap through
//! [`Scale`]. One private `laid_out_rows` is the *one* function that computes
//! each row's rectangle for the current scroll offset; both
//! [`Settings::render`] and [`Settings::on_pointer`] read it, so drawing and
//! hit-testing can never disagree. A row that would only be partly inside the
//! scrollable body is left out of that list entirely — it neither draws nor
//! accepts a pointer — so a small window degrades to "scroll to reach it"
//! rather than to a half-drawn control or a panic; the keyboard path never
//! depends on a row's rectangle at all, so every setting stays reachable from
//! the keyboard however small the window is.
//!
//! # Keyboard model
//!
//! Tab/Shift-Tab moves focus between rows (including the tab strip itself,
//! the scrollbar, and the footer buttons); the keys a focused control's own
//! `on_key` understands — arrows, Space/Enter, Page Up/Down, Home/End — drive
//! that control. Escape and the *Done* button dismiss the sheet; a primary
//! press outside the panel also dismisses it, since the sheet is modal and
//! nothing outside it is reachable while it is open.

use alloc::format;
use alloc::string::ToString;
use alloc::vec::Vec;

use tairix_font::BitmapFont;
use tairix_geometry::{to_i32, Point, Rect, Region, Scale};
use tairix_input::{InputEvent, Key, Modifiers, NamedKey, PointerButton};
use tairix_raster::{Color, Surface};
use tairix_theme::{TextRole, Theme};

use tairix_controls::{
    damage, Button, ButtonAction, ButtonContent, ControlRole, Panel, Radio, ScrollBar, ScrollModel,
    ScrollOrientation, ScrollRange, SelectorAction, Slider, SliderAction, Tab, Tabs, TabsAction,
};

use crate::effects::{FULL, MIN_OPACITY};
use crate::profile::{Profile, MAX_FONT_SIZE_PX, MIN_FONT_SIZE_PX};
use crate::scheme::Scheme;
use crate::swatch::{SwatchAction, SwatchGrid};

/// The tab that edits the colour scheme and text size.
const APPEARANCE_TAB: usize = 0;

/// The tab that edits the screen effects.
const EFFECTS_TAB: usize = 1;

/// How many effect sliders the Effects tab lists, and the fixed order they
/// index [`Settings::effect_sliders`] and a [`Profile`]'s effect fields in:
/// Opacity, Blur, Scan lines, Fuzz, Phosphor, Wobble.
const EFFECT_COUNT: usize = 6;

/// The label a channel slider carries, in [`Settings::channel_sliders`] order.
const CHANNEL_LABELS: [&str; 3] = ["Red", "Green", "Blue"];

/// The label an effect slider carries, in [`Settings::effect_sliders`] order.
const EFFECT_LABELS: [&str; EFFECT_COUNT] = [
    "Opacity",
    "Backdrop blur",
    "Scan lines",
    "Fuzz",
    "Phosphor",
    "Wobble",
];

/// The closed range each effect slider spans, in [`EFFECT_LABELS`] order.
///
/// Opacity starts at [`MIN_OPACITY`] rather than zero because a fully
/// transparent screen is unreadable; mapping the slider onto that range keeps
/// the whole travel live instead of leaving its first third snapping back.
const EFFECT_BOUNDS: [(u16, u16); EFFECT_COUNT] = [
    (MIN_OPACITY, FULL),
    (0, FULL),
    (0, FULL),
    (0, FULL),
    (0, FULL),
    (0, FULL),
];

/// The logical width of a slider row's leading label column.
const LABEL_WIDTH_PX: u32 = 150;

/// The logical gap between a slider row's label and its slider.
const LABEL_GAP_PX: u32 = 8;

/// The logical gap between the custom-editor caption and its swatch grid.
const CAPTION_GAP_PX: u32 = 4;

/// The largest logical width the sheet's panel grows to; a viewport smaller
/// than this simply gives the panel the whole viewport instead.
const MAX_PANEL_WIDTH_PX: u32 = 520;

/// The largest logical height the sheet's panel grows to.
const MAX_PANEL_HEIGHT_PX: u32 = 420;

/// The one row a [`Settings`] sheet lays its scrollable body out from.
///
/// Every variant but the footer/scrollbar/tab-strip picks are a *content*
/// row belonging to whichever tab is current; [`Settings::content_rows`]
/// lists exactly the rows the active tab owns, in display order.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum Focus {
    /// The tab strip itself.
    Tabs,
    /// The scheme radio at [`Scheme::ALL`] index `usize`.
    Scheme(usize),
    /// The text-size slider.
    TextSize,
    /// The custom-scheme swatch grid.
    Swatches,
    /// A colour channel of the selected swatch well: `0` red, `1` green,
    /// `2` blue.
    Channel(usize),
    /// An effect slider, indexed as [`EFFECT_LABELS`].
    Effect(usize),
    /// The body scrollbar.
    Scroll,
    /// The *Restore defaults* footer button.
    Restore,
    /// The *Done* footer button.
    Done,
}

/// What routing an input event into the sheet concluded.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum SheetOutcome {
    /// Not claimed by the sheet.
    Ignored,
    /// Claimed; only the sheet's own pixels changed.
    Changed,
    /// Claimed; the profile was edited (the caller re-resolves colours,
    /// re-derives the font, repaints, and persists).
    Edited,
    /// The sheet asked to close.
    Dismissed,
}

/// The in-window settings sheet (module documentation above).
pub struct Settings {
    profile: Profile,
    panel: Panel,
    tabs: Tabs,
    scheme_radios: Vec<Radio>,
    text_size: Slider,
    swatches: SwatchGrid,
    channel_sliders: [Slider; 3],
    effect_sliders: [Slider; EFFECT_COUNT],
    restore: Button,
    done: Button,
    scroll: ScrollBar,
    focus: Focus,
    /// The last pointer position, tracked from [`InputEvent::PointerMoved`]
    /// since a press/release event carries no position of its own.
    last_pointer: Point,
}

impl Settings {
    /// A sheet opened on a copy of `profile`.
    #[must_use]
    pub fn new(profile: &Profile) -> Self {
        let mut profile = *profile;
        profile.clamp();

        let mut tabs = Tabs::new(Vec::from([Tab::new("Appearance"), Tab::new("Effects")]));
        tabs.adopt_selected(APPEARANCE_TAB);

        let scheme_radios = Scheme::ALL
            .iter()
            .map(|scheme| Radio::new(scheme.label(), *scheme == profile.scheme))
            .collect();

        let text_size = Slider::new(permille_from_bounded(
            profile.font_size_px,
            MIN_FONT_SIZE_PX,
            MAX_FONT_SIZE_PX,
        ))
        .with_steps(font_size_step_permille(), font_size_step_permille() * 4);

        let swatches = SwatchGrid::from_scheme(&profile.custom);

        let effect_sliders = [
            effect_slider(0, profile.effects.opacity),
            effect_slider(1, profile.effects.blur),
            effect_slider(2, profile.effects.scanlines),
            effect_slider(3, profile.effects.fuzz),
            effect_slider(4, profile.effects.phosphor),
            effect_slider(5, profile.effects.wobble),
        ];

        let mut sheet = Self {
            profile,
            panel: Panel::new("Terminal Settings"),
            tabs,
            scheme_radios,
            text_size,
            swatches,
            channel_sliders: [
                Slider::new(0).with_steps(10, 100),
                Slider::new(0).with_steps(10, 100),
                Slider::new(0).with_steps(10, 100),
            ],
            effect_sliders,
            restore: Button::new(
                ButtonContent::Label("Restore defaults".to_string()),
                ControlRole::Neutral,
            ),
            done: Button::new(
                ButtonContent::Label("Done".to_string()),
                ControlRole::Neutral,
            ),
            // The steps and extents are density-dependent, so the bar starts
            // inert and is sized from the theme by `scrolled_model` before any
            // frame is drawn or any event routed.
            scroll: ScrollBar::new(
                ScrollOrientation::Vertical,
                ScrollModel::new(ScrollRange::EMPTY, 0, 0),
            ),
            focus: Focus::Tabs,
            last_pointer: Point::ORIGIN,
        };
        sheet.sync_channel_sliders();
        sheet.sync_focus(Rect::EMPTY, &mut damage::sink());
        sheet
    }

    /// The profile as edited so far (always clamped/valid).
    #[must_use]
    pub fn profile(&self) -> &Profile {
        &self.profile
    }

    /// Draw the sheet over the terminal screen already in `surface`.
    pub fn render(&self, surface: &mut Surface, viewport: Rect, scale: Scale, theme: &Theme) {
        let font = BitmapFont::for_role(theme.fonts(), TextRole::Body, scale);

        let bounds = panel_bounds(viewport, scale);
        self.panel.render(surface, bounds, scale, theme);
        let Some(content) = self.panel.content_rect(bounds, scale, theme) else {
            return;
        };
        let (tabs_rect, body_rect, scrollbar_rect, footer_rect) = self.bands(content, scale, theme);
        // Drawing cannot mutate the held bar, so the bar is drawn from the
        // same freshly-sized model the rows are laid out at.
        let model = self.scrolled_model(body_rect, scale, theme, font);

        if let Some(rect) = tabs_rect {
            self.tabs.render(surface, rect, scale, theme);
        }
        if let Some(rect) = body_rect {
            if let Some((bx, by, bw, bh)) = surface_rect(rect) {
                surface.with_clip(bx, by, bw, bh, |clipped| {
                    self.render_rows(clipped, rect, model.offset(), scale, theme, font);
                });
            }
        }
        if let Some(rect) = scrollbar_rect {
            let mut bar = self.scroll;
            bar.set_model(model);
            bar.render(surface, rect, scale, theme);
        }
        if let Some(rect) = footer_rect {
            self.render_footer(surface, rect, scale, theme);
        }
    }

    /// Route one pointer event; `viewport` is the whole window client rect.
    ///
    /// The sheet is modal, so it claims every event a drawable panel can see;
    /// only a viewport too small for the panel to have a content rectangle at
    /// all leaves an event [`SheetOutcome::Ignored`] for the caller, and even
    /// then a press still dismisses.
    pub fn on_pointer(
        &mut self,
        event: &InputEvent,
        viewport: Rect,
        scale: Scale,
        theme: &Theme,
        damage: &mut Region,
    ) -> SheetOutcome {
        let font = BitmapFont::for_role(theme.fonts(), TextRole::Body, scale);

        if let InputEvent::PointerMoved { to } = event {
            self.last_pointer = *to;
        }
        let bounds = panel_bounds(viewport, scale);

        // A primary press outside the whole panel dismisses the sheet: it is
        // modal, so nothing outside it is reachable while it is open. This is
        // tested before the panel's own geometry so a viewport too small to
        // draw the sheet in can still be clicked out of.
        if matches!(
            event,
            InputEvent::PointerPressed {
                button: PointerButton::Primary
            }
        ) && !bounds.contains(self.last_pointer)
        {
            return SheetOutcome::Dismissed;
        }

        let Some(content) = self.panel.content_rect(bounds, scale, theme) else {
            return SheetOutcome::Ignored;
        };
        let (tabs_rect, body_rect, scrollbar_rect, footer_rect) = self.bands(content, scale, theme);
        self.scroll
            .set_model(self.scrolled_model(body_rect, scale, theme, font));

        if let Some(rect) = tabs_rect {
            if let Some(TabsAction::Selected { index }) = self.tabs.on_pointer(event, rect, damage)
            {
                self.tabs.set_selected(index, rect, damage);
                self.focus = Focus::Tabs;
                self.sync_focus(rect, damage);
                return SheetOutcome::Changed;
            }
        }

        if let Some(rect) = body_rect {
            if let outcome @ (SheetOutcome::Changed | SheetOutcome::Edited) =
                self.route_body_pointer(event, rect, scale, theme, font, damage)
            {
                return outcome;
            }
        }

        if let Some(rect) = scrollbar_rect {
            // The bar applies the offset to the model it holds, and that model
            // is the sheet's only scroll position, so there is nothing further
            // to write back.
            if self
                .scroll
                .on_pointer(event, rect, scale, theme, damage)
                .is_some()
            {
                self.focus = Focus::Scroll;
                self.sync_focus(tabs_rect.unwrap_or(Rect::EMPTY), damage);
                return SheetOutcome::Changed;
            }
        }

        if let Some(rect) = footer_rect {
            if let Some(outcome) = self.route_footer_pointer(event, rect, scale, damage) {
                return outcome;
            }
        }

        // A press outside the panel's content but still inside the panel
        // (the header, or a gap between bands) is claimed and otherwise
        // inert: the sheet stays open with nothing else changed.
        SheetOutcome::Changed
    }

    /// Route one key press.
    ///
    /// Never [`SheetOutcome::Ignored`]: the keyboard path does not depend on
    /// the sheet being drawable, so every setting stays reachable however
    /// small the window is.
    pub fn on_key(
        &mut self,
        key: Key,
        modifiers: Modifiers,
        viewport: Rect,
        scale: Scale,
        theme: &Theme,
        damage: &mut Region,
    ) -> SheetOutcome {
        let font = BitmapFont::for_role(theme.fonts(), TextRole::Body, scale);

        // Keyboard reach never depends on a row's rectangle, so a viewport too
        // small to lay the body out still dismisses, moves focus, and edits.
        // Only scrolling needs the geometry, and an unsizeable body simply
        // leaves the bar with nothing to move.
        let bounds = panel_bounds(viewport, scale);
        let (tabs_rect, body_rect, scrollbar_rect, _) = self
            .panel
            .content_rect(bounds, scale, theme)
            .map_or((None, None, None, None), |content| {
                self.bands(content, scale, theme)
            });
        self.scroll
            .set_model(self.scrolled_model(body_rect, scale, theme, font));
        if key == Key::Named(NamedKey::Escape) {
            return SheetOutcome::Dismissed;
        }
        if key == Key::Named(NamedKey::Tab) {
            self.move_focus(!modifiers.shift);
            self.sync_focus(tabs_rect.unwrap_or(Rect::EMPTY), damage);
            return SheetOutcome::Changed;
        }
        let slider_rect = self.focused_slider_rect(body_rect, scale, theme, font);
        self.dispatch_key(
            key,
            tabs_rect.unwrap_or(Rect::EMPTY),
            slider_rect.unwrap_or(Rect::EMPTY),
            scrollbar_rect.unwrap_or(Rect::EMPTY),
            damage,
        )
    }
}

// --- Construction helpers --------------------------------------------------

/// The permille line step that moves the text-size slider by one logical
/// pixel.
fn font_size_step_permille() -> u16 {
    permille_from_bounded(
        MIN_FONT_SIZE_PX.saturating_add(1),
        MIN_FONT_SIZE_PX,
        MAX_FONT_SIZE_PX,
    )
    .max(1)
}

/// Map a value within `min..=max` onto a slider's `0..=1000` permille scale.
fn permille_from_bounded(value: u16, min: u16, max: u16) -> u16 {
    let span = u32::from(max.saturating_sub(min)).max(1);
    let numerator = u32::from(value.clamp(min, max).saturating_sub(min)) * u32::from(FULL);
    u16::try_from(numerator / span).unwrap_or(FULL).min(FULL)
}

/// The inverse of [`permille_from_bounded`].
fn bounded_from_permille(permille: u16, min: u16, max: u16) -> u16 {
    let span = u32::from(max.saturating_sub(min));
    let value = u32::from(min)
        + (u32::from(permille.min(FULL)) * span + u32::from(FULL) / 2) / u32::from(FULL);
    u16::try_from(value).unwrap_or(max).min(max)
}

/// An 8-bit channel mapped onto permille, for a channel slider.
fn permille_from_channel(value: u8) -> u16 {
    permille_from_bounded(u16::from(value), 0, 255)
}

/// The inverse of [`permille_from_channel`].
fn channel_from_permille(permille: u16) -> u8 {
    u8::try_from(bounded_from_permille(permille, 0, 255)).unwrap_or(u8::MAX)
}

/// A permille value as a whole percentage, rounded to nearest.
fn permille_as_percent(permille: u16) -> u32 {
    (u32::from(permille) + 5) / 10
}

/// The closed range the effect at `index` spans.
fn effect_bounds(index: usize) -> (u16, u16) {
    EFFECT_BOUNDS.get(index).copied().unwrap_or((0, FULL))
}

/// An effect value mapped onto its slider's permille travel.
fn effect_permille(index: usize, value: u16) -> u16 {
    let (min, max) = effect_bounds(index);
    permille_from_bounded(value, min, max)
}

/// The inverse of [`effect_permille`].
fn effect_from_permille(index: usize, permille: u16) -> u16 {
    let (min, max) = effect_bounds(index);
    bounded_from_permille(permille, min, max)
}

/// The slider for the effect at `index`, showing `value` on that effect's own
/// travel.
fn effect_slider(index: usize, value: u16) -> Slider {
    Slider::new(effect_permille(index, value)).with_steps(10, 100)
}

/// The surface rectangle of a logical `Rect`, or `None` if it lies off the
/// top-left or collapses.
fn surface_rect(rect: Rect) -> Option<(u32, u32, u32, u32)> {
    let x = u32::try_from(rect.left()).ok()?;
    let y = u32::try_from(rect.top()).ok()?;
    if rect.width == 0 || rect.height == 0 {
        return None;
    }
    Some((x, y, rect.width, rect.height))
}

impl Settings {
    /// Every content row the active tab owns, in display order.
    fn content_rows(&self) -> Vec<Focus> {
        let mut rows = Vec::new();
        if self.tabs.selected() == Some(EFFECTS_TAB) {
            for index in 0..EFFECT_COUNT {
                rows.push(Focus::Effect(index));
            }
        } else {
            for index in 0..self.scheme_radios.len() {
                rows.push(Focus::Scheme(index));
            }
            rows.push(Focus::TextSize);
            rows.push(Focus::Swatches);
            for index in 0..self.channel_sliders.len() {
                rows.push(Focus::Channel(index));
            }
        }
        rows
    }

    /// Every focusable element in Tab order: the tab strip, every content
    /// row of the active tab, the scrollbar, then the footer buttons.
    fn focus_order(&self) -> Vec<Focus> {
        let mut order = Vec::from([Focus::Tabs]);
        order.extend(self.content_rows());
        order.push(Focus::Scroll);
        order.push(Focus::Restore);
        order.push(Focus::Done);
        order
    }

    /// Move focus to the next (`forward`) or previous element, wrapping.
    fn move_focus(&mut self, forward: bool) {
        let order = self.focus_order();
        if order.is_empty() {
            return;
        }
        let current = order.iter().position(|&f| f == self.focus).unwrap_or(0);
        let next = if forward {
            (current + 1) % order.len()
        } else {
            (current + order.len() - 1) % order.len()
        };
        self.focus = order[next];
    }

    /// Set exactly the focused control's own focus flag, clearing every
    /// other one — the one place that maps [`Focus`] onto every control's
    /// composed keyboard-focus state.
    ///
    /// `tabs` is the strip's own rectangle as the sheet last laid it out, empty
    /// where the strip is drawn nowhere to report against: a window too small to
    /// seat it, or a sheet still being composed and presented whole.
    fn sync_focus(&mut self, tabs: Rect, damage: &mut Region) {
        for (index, radio) in self.scheme_radios.iter_mut().enumerate() {
            radio.set_focused(self.focus == Focus::Scheme(index));
        }
        self.text_size.set_focused(self.focus == Focus::TextSize);
        for (index, slider) in self.channel_sliders.iter_mut().enumerate() {
            slider.set_focused(self.focus == Focus::Channel(index));
        }
        for (index, slider) in self.effect_sliders.iter_mut().enumerate() {
            slider.set_focused(self.focus == Focus::Effect(index));
        }
        self.scroll.set_focused(self.focus == Focus::Scroll);
        self.restore.set_focused(self.focus == Focus::Restore);
        self.done.set_focused(self.focus == Focus::Done);
        if self.focus == Focus::Tabs {
            let index = self.tabs.current().or(self.tabs.selected()).unwrap_or(0);
            self.tabs.set_current(Some(index), tabs, damage);
        } else {
            self.tabs.set_current(None, tabs, damage);
        }
    }

    /// Copy the currently selected swatch well's channels into the three
    /// channel sliders, so they always show the well they edit.
    fn sync_channel_sliders(&mut self) {
        let color = self
            .swatches
            .color(self.swatches.selected())
            .unwrap_or_default();
        let channels = [color.r, color.g, color.b];
        for (slider, channel) in self.channel_sliders.iter_mut().zip(channels) {
            slider.set_value(permille_from_channel(channel));
        }
    }

    /// Mark exactly the radio matching the profile's current scheme as
    /// selected.
    fn sync_scheme_radios(&mut self) {
        for (index, radio) in self.scheme_radios.iter_mut().enumerate() {
            let scheme = Scheme::ALL.get(index).copied().unwrap_or(Scheme::System);
            radio.set_selected(scheme == self.profile.scheme);
        }
    }

    /// The height a `row` needs at `scale` under `theme`.
    fn row_height(&self, row: Focus, scale: Scale, theme: &Theme, font: BitmapFont) -> u32 {
        match row {
            Focus::Swatches => {
                let caption = font.glyph_height().max(1);
                let gap = scale.scale_length(CAPTION_GAP_PX).max(1);
                self.swatches
                    .preferred_height(scale, theme)
                    .saturating_add(caption)
                    .saturating_add(gap)
            }
            _ => scale.scale_length(theme.metrics().control_height).max(1),
        }
    }

    /// The total scrollable height the active tab's rows need, ignoring the
    /// current scroll offset.
    fn content_extent(&self, scale: Scale, theme: &Theme, font: BitmapFont) -> u32 {
        let gap = scale.scale_length(theme.metrics().control_gap).max(1);
        let rows = self.content_rows();
        let mut total: u32 = 0;
        for (index, row) in rows.iter().enumerate() {
            if index > 0 {
                total = total.saturating_add(gap);
            }
            total = total.saturating_add(self.row_height(*row, scale, theme, font));
        }
        total
    }

    /// The rectangles of every row that is *fully* inside `body` at the
    /// current scroll offset — the one layout [`Settings::render`] and
    /// [`Settings::route_body_pointer`] both read.
    ///
    /// A row only partly inside `body` is omitted rather than clipped: a
    /// half-drawn slider or radio would be both visually confusing and, for
    /// a pointer, ambiguous to hit-test, so it simply is not laid out until
    /// scrolled fully into view.
    fn laid_out_rows(
        &self,
        body: Rect,
        offset: u64,
        scale: Scale,
        theme: &Theme,
        font: BitmapFont,
    ) -> Vec<(Focus, Rect)> {
        let gap = to_i32(scale.scale_length(theme.metrics().control_gap).max(1));
        let offset = u32::try_from(offset.min(u64::from(u32::MAX))).unwrap_or(0);
        let mut y = body.top() - to_i32(offset);
        let mut out = Vec::new();
        for row in self.content_rows() {
            let height = self.row_height(row, scale, theme, font);
            let rect = Rect::new(body.left(), y, body.width, height);
            if rect.top() >= body.top() && rect.bottom() <= body.bottom() {
                out.push((row, rect));
            }
            y = y.saturating_add(to_i32(height)).saturating_add(gap);
        }
        out
    }

    /// The panel content split into the tab strip, the scrollable body, the
    /// scrollbar, and the footer, in that top-to-bottom order.
    ///
    /// Each band claims what it needs, in order, and hands the remainder on;
    /// a viewport too small for a band simply gives it zero height rather
    /// than overlapping the next one or panicking.
    fn bands(
        &self,
        content: Rect,
        scale: Scale,
        theme: &Theme,
    ) -> (Option<Rect>, Option<Rect>, Option<Rect>, Option<Rect>) {
        let total_h = content.height;
        let tabs_h = self.tabs.measured_extent(scale, theme).min(total_h);
        let after_tabs = total_h.saturating_sub(tabs_h);
        let footer_h = scale
            .scale_length(theme.metrics().control_height)
            .max(1)
            .min(after_tabs);
        let body_h = after_tabs.saturating_sub(footer_h);

        let tabs_rect =
            (tabs_h > 0).then(|| Rect::new(content.left(), content.top(), content.width, tabs_h));
        let body_top = content.top() + to_i32(tabs_h);
        let scrollbar_w = scale
            .scale_length(theme.metrics().scrollbar_breadth)
            .max(1)
            .min(content.width);
        let rows_w = content.width.saturating_sub(scrollbar_w);
        let body_rect =
            (body_h > 0 && rows_w > 0).then(|| Rect::new(content.left(), body_top, rows_w, body_h));
        let scrollbar_rect = (body_h > 0 && scrollbar_w > 0).then(|| {
            Rect::new(
                content.left() + to_i32(rows_w),
                body_top,
                scrollbar_w,
                body_h,
            )
        });
        let footer_top = body_top + to_i32(body_h);
        let footer_rect =
            (footer_h > 0).then(|| Rect::new(content.left(), footer_top, content.width, footer_h));

        (tabs_rect, body_rect, scrollbar_rect, footer_rect)
    }

    /// The scroll model the active tab's content implies for `body_rect`: the
    /// held offset re-clamped against that content, with a line step of one
    /// row pitch and a page step of one bodyful, both taken from the theme
    /// through [`Scale`] so they track the display density.
    ///
    /// Rendering and both input paths read this one function, so the offset a
    /// row is laid out at can never disagree with the offset the bar draws.
    ///
    /// The sheet keeps one scroll position shared by both tabs: switching
    /// tabs re-clamps it against the new tab's own content extent rather than
    /// remembering a per-tab position.
    fn scrolled_model(
        &self,
        body_rect: Option<Rect>,
        scale: Scale,
        theme: &Theme,
        font: BitmapFont,
    ) -> ScrollModel {
        let viewport_extent = u64::from(body_rect.map_or(0, |rect| rect.height));
        let content_extent = u64::from(self.content_extent(scale, theme, font));
        let metrics = theme.metrics();
        let line_step = u64::from(
            scale
                .scale_length(metrics.control_height.saturating_add(metrics.control_gap))
                .max(1),
        );
        ScrollModel::new(
            self.scroll
                .model()
                .range()
                .resize(content_extent, viewport_extent),
            line_step,
            viewport_extent.max(line_step),
        )
    }

    /// Paint the scrollable body's rows into `surface`, already clipped to
    /// `body`.
    fn render_rows(
        &self,
        surface: &mut Surface,
        body: Rect,
        offset: u64,
        scale: Scale,
        theme: &Theme,
        font: BitmapFont,
    ) {
        for (row, rect) in self.laid_out_rows(body, offset, scale, theme, font) {
            self.render_row(surface, row, rect, scale, theme, font);
        }
    }

    /// Paint one row at its already-laid-out `rect`.
    fn render_row(
        &self,
        surface: &mut Surface,
        row: Focus,
        rect: Rect,
        scale: Scale,
        theme: &Theme,
        font: BitmapFont,
    ) {
        match row {
            Focus::Scheme(index) => {
                if let Some(radio) = self.scheme_radios.get(index) {
                    radio.render(surface, rect, scale, theme);
                }
            }
            Focus::TextSize => {
                let (label, control) = split_row(rect, scale);
                let text = format!("Text size {}px", self.profile.font_size_px);
                draw_row_label(surface, label, theme, font, &text);
                self.text_size.render(surface, control, scale, theme);
            }
            Focus::Swatches => self.render_swatches(surface, rect, scale, theme, font),
            Focus::Channel(index) => {
                let Some(slider) = self.channel_sliders.get(index) else {
                    return;
                };
                let Some(&label_text) = CHANNEL_LABELS.get(index) else {
                    return;
                };
                let value = self.channel_value(index);
                let (label, control) = split_row(rect, scale);
                draw_row_label(
                    surface,
                    label,
                    theme,
                    font,
                    &format!("{label_text} {value}"),
                );
                slider.render(surface, control, scale, theme);
            }
            Focus::Effect(index) => {
                let Some(slider) = self.effect_sliders.get(index) else {
                    return;
                };
                let Some(&label_text) = EFFECT_LABELS.get(index) else {
                    return;
                };
                let percent = permille_as_percent(self.effect_value(index));
                let (label, control) = split_row(rect, scale);
                draw_row_label(
                    surface,
                    label,
                    theme,
                    font,
                    &format!("{label_text} {percent}%"),
                );
                slider.render(surface, control, scale, theme);
            }
            Focus::Tabs | Focus::Scroll | Focus::Restore | Focus::Done => {}
        }
    }

    /// Paint the custom-scheme editor's active/inactive caption above its
    /// swatch grid.
    fn render_swatches(
        &self,
        surface: &mut Surface,
        rect: Rect,
        scale: Scale,
        theme: &Theme,
        font: BitmapFont,
    ) {
        let (caption, grid) = swatch_caption_split(rect, scale, font);
        let text = if self.profile.scheme == Scheme::Custom {
            "Custom scheme (active)"
        } else {
            "Custom scheme (not active — select it above to use these colours)"
        };
        draw_row_label(surface, caption, theme, font, text);
        self.swatches.render(surface, grid, scale, theme);
    }

    /// Paint the *Restore defaults* / *Done* footer buttons.
    fn render_footer(&self, surface: &mut Surface, rect: Rect, scale: Scale, theme: &Theme) {
        let (restore, done) = footer_split(rect, scale);
        if let Some(rect) = restore {
            self.restore.render(surface, rect, scale, theme);
        }
        if let Some(rect) = done {
            self.done.render(surface, rect, scale, theme);
        }
    }

    /// Route one pointer event into the scrollable body's rows.
    fn route_body_pointer(
        &mut self,
        event: &InputEvent,
        body: Rect,
        scale: Scale,
        theme: &Theme,
        font: BitmapFont,
        damage: &mut Region,
    ) -> SheetOutcome {
        let offset = self.scroll.model().offset();
        for (row, rect) in self.laid_out_rows(body, offset, scale, theme, font) {
            let outcome = self.route_row_pointer(event, row, rect, scale, font, damage);
            if outcome != SheetOutcome::Ignored {
                return outcome;
            }
        }
        SheetOutcome::Ignored
    }

    /// Route one pointer event into a single already-laid-out row.
    fn route_row_pointer(
        &mut self,
        event: &InputEvent,
        row: Focus,
        rect: Rect,
        scale: Scale,
        font: BitmapFont,
        damage: &mut Region,
    ) -> SheetOutcome {
        match row {
            Focus::Scheme(index) => {
                let Some(radio) = self.scheme_radios.get_mut(index) else {
                    return SheetOutcome::Ignored;
                };
                match radio.on_pointer(event, rect, damage) {
                    Some(SelectorAction::Set { on: true }) => {
                        self.focus = row;
                        self.set_scheme(index);
                        SheetOutcome::Edited
                    }
                    _ => SheetOutcome::Ignored,
                }
            }
            Focus::TextSize => {
                let (_, control) = split_row(rect, scale);
                match self.text_size.on_pointer(event, control, damage) {
                    Some(SliderAction::SetValue { permille }) => {
                        self.focus = row;
                        self.set_font_size_permille(permille);
                        SheetOutcome::Edited
                    }
                    None => SheetOutcome::Ignored,
                }
            }
            Focus::Swatches => self.route_swatches_pointer(event, rect, scale, font),
            Focus::Channel(index) => {
                let (_, control) = split_row(rect, scale);
                let Some(slider) = self.channel_sliders.get_mut(index) else {
                    return SheetOutcome::Ignored;
                };
                match slider.on_pointer(event, control, damage) {
                    Some(SliderAction::SetValue { permille }) => {
                        self.focus = row;
                        self.set_channel_permille(index, permille);
                        SheetOutcome::Edited
                    }
                    None => SheetOutcome::Ignored,
                }
            }
            Focus::Effect(index) => {
                let (_, control) = split_row(rect, scale);
                let Some(slider) = self.effect_sliders.get_mut(index) else {
                    return SheetOutcome::Ignored;
                };
                match slider.on_pointer(event, control, damage) {
                    Some(SliderAction::SetValue { permille }) => {
                        self.focus = row;
                        self.set_effect_permille(index, permille);
                        SheetOutcome::Edited
                    }
                    None => SheetOutcome::Ignored,
                }
            }
            Focus::Tabs | Focus::Scroll | Focus::Restore | Focus::Done => SheetOutcome::Ignored,
        }
    }

    /// Route one pointer event into the custom-editor swatch grid row.
    fn route_swatches_pointer(
        &mut self,
        event: &InputEvent,
        rect: Rect,
        scale: Scale,
        font: BitmapFont,
    ) -> SheetOutcome {
        let (_, grid_rect) = swatch_caption_split(rect, scale, font);
        match self.swatches.on_pointer(event, grid_rect) {
            Some(SwatchAction::Selected { .. }) => {
                self.focus = Focus::Swatches;
                self.sync_channel_sliders();
                SheetOutcome::Changed
            }
            None => SheetOutcome::Ignored,
        }
    }

    /// Route one pointer event into the *Restore defaults* / *Done* buttons.
    fn route_footer_pointer(
        &mut self,
        event: &InputEvent,
        rect: Rect,
        scale: Scale,
        damage: &mut Region,
    ) -> Option<SheetOutcome> {
        let (restore, done) = footer_split(rect, scale);
        if let Some(rect) = restore {
            if let Some(ButtonAction::Activated) = self.restore.on_pointer(event, rect, damage) {
                self.focus = Focus::Restore;
                self.restore_defaults();
                return Some(SheetOutcome::Edited);
            }
        }
        if let Some(rect) = done {
            if let Some(ButtonAction::Activated) = self.done.on_pointer(event, rect, damage) {
                self.focus = Focus::Done;
                return Some(SheetOutcome::Dismissed);
            }
        }
        None
    }

    /// The control rectangle of the focused slider row, as the body last laid
    /// it out — the very rectangle the pointer path hit-tests and the renderer
    /// draws into. `None` where the body is unsizeable or the focus is not a
    /// slider row.
    fn focused_slider_rect(
        &self,
        body: Option<Rect>,
        scale: Scale,
        theme: &Theme,
        font: BitmapFont,
    ) -> Option<Rect> {
        let offset = self.scroll.model().offset();
        self.laid_out_rows(body?, offset, scale, theme, font)
            .into_iter()
            .find(|(row, _)| *row == self.focus)
            .map(|(_, rect)| split_row(rect, scale).1)
    }

    /// Dispatch one key press to whichever element currently holds focus.
    ///
    /// The rectangles are the tab strip, the focused slider, and the scrollbar
    /// as the sheet last laid them out, and are empty where a window too small
    /// to draw that element leaves it no pixels to report.
    fn dispatch_key(
        &mut self,
        key: Key,
        tabs: Rect,
        slider: Rect,
        scrollbar: Rect,
        damage: &mut Region,
    ) -> SheetOutcome {
        match self.focus {
            Focus::Tabs => {
                if let Some(TabsAction::Selected { index }) = self.tabs.on_key(key, tabs, damage) {
                    self.tabs.set_selected(index, tabs, damage);
                }
                SheetOutcome::Changed
            }
            Focus::Scheme(index) => match self
                .scheme_radios
                .get_mut(index)
                .and_then(|r| r.on_key(key))
            {
                Some(SelectorAction::Set { on: true }) => {
                    self.set_scheme(index);
                    SheetOutcome::Edited
                }
                _ => SheetOutcome::Changed,
            },
            Focus::TextSize => match self.text_size.on_key(key, slider, damage) {
                Some(SliderAction::SetValue { permille }) => {
                    self.set_font_size_permille(permille);
                    SheetOutcome::Edited
                }
                None => SheetOutcome::Changed,
            },
            Focus::Swatches => match self.swatches.on_key(key) {
                Some(SwatchAction::Selected { .. }) => {
                    self.sync_channel_sliders();
                    SheetOutcome::Changed
                }
                None => SheetOutcome::Changed,
            },
            Focus::Channel(index) => match self
                .channel_sliders
                .get_mut(index)
                .and_then(|s| s.on_key(key, slider, damage))
            {
                Some(SliderAction::SetValue { permille }) => {
                    self.set_channel_permille(index, permille);
                    SheetOutcome::Edited
                }
                None => SheetOutcome::Changed,
            },
            Focus::Effect(index) => match self
                .effect_sliders
                .get_mut(index)
                .and_then(|s| s.on_key(key, slider, damage))
            {
                Some(SliderAction::SetValue { permille }) => {
                    self.set_effect_permille(index, permille);
                    SheetOutcome::Edited
                }
                None => SheetOutcome::Changed,
            },
            // The bar holds the sheet's only scroll position and has already
            // moved it, so the action needs no write-back.
            Focus::Scroll => {
                let _ = self.scroll.on_key(key, scrollbar, damage);
                SheetOutcome::Changed
            }
            Focus::Restore => match self.restore.on_key(key) {
                Some(ButtonAction::Activated) => {
                    self.restore_defaults();
                    SheetOutcome::Edited
                }
                None => SheetOutcome::Changed,
            },
            Focus::Done => match self.done.on_key(key) {
                Some(ButtonAction::Activated) => SheetOutcome::Dismissed,
                None => SheetOutcome::Changed,
            },
        }
    }

    /// Commit a scheme choice at `index`, clamp, and re-sync every radio.
    fn set_scheme(&mut self, index: usize) {
        if let Some(scheme) = Scheme::ALL.get(index).copied() {
            self.profile.scheme = scheme;
            self.profile.clamp();
        }
        self.sync_scheme_radios();
    }

    /// Commit a text-size request, clamp, and reflect the clamped value.
    fn set_font_size_permille(&mut self, permille: u16) {
        self.profile.font_size_px =
            bounded_from_permille(permille, MIN_FONT_SIZE_PX, MAX_FONT_SIZE_PX);
        self.profile.clamp();
        self.text_size.set_value(permille_from_bounded(
            self.profile.font_size_px,
            MIN_FONT_SIZE_PX,
            MAX_FONT_SIZE_PX,
        ));
    }

    /// Commit a channel request for the selected well, apply it onto the
    /// custom scheme, and reflect the (never-clamped, channels have no
    /// tighter bound than their own type) value back into the slider.
    fn set_channel_permille(&mut self, channel: usize, permille: u16) {
        let selected = self.swatches.selected();
        let mut color = self.swatches.color(selected).unwrap_or_default();
        let value = channel_from_permille(permille);
        match channel {
            0 => color.r = value,
            1 => color.g = value,
            2 => color.b = value,
            _ => return,
        }
        self.swatches.set_color(selected, color);
        self.swatches.apply_to(&mut self.profile.custom);
        self.profile.clamp();
        if let Some(slider) = self.channel_sliders.get_mut(channel) {
            slider.set_value(permille_from_channel(value));
        }
    }

    /// The current channel value (`0..=255`) of the selected well.
    fn channel_value(&self, channel: usize) -> u8 {
        let color = self
            .swatches
            .color(self.swatches.selected())
            .unwrap_or_default();
        match channel {
            0 => color.r,
            1 => color.g,
            _ => color.b,
        }
    }

    /// Commit an effect request at `index`, clamp, and reflect the clamped
    /// value back onto its slider's own travel.
    fn set_effect_permille(&mut self, index: usize, permille: u16) {
        let value = effect_from_permille(index, permille);
        {
            let field = match index {
                0 => &mut self.profile.effects.opacity,
                1 => &mut self.profile.effects.blur,
                2 => &mut self.profile.effects.scanlines,
                3 => &mut self.profile.effects.fuzz,
                4 => &mut self.profile.effects.phosphor,
                _ => &mut self.profile.effects.wobble,
            };
            *field = value;
        }
        self.profile.clamp();
        let clamped = self.effect_value(index);
        if let Some(slider) = self.effect_sliders.get_mut(index) {
            slider.set_value(effect_permille(index, clamped));
        }
    }

    /// The profile's current value for the effect at `index`.
    fn effect_value(&self, index: usize) -> u16 {
        let effects = self.profile.effects;
        match index {
            0 => effects.opacity,
            1 => effects.blur,
            2 => effects.scanlines,
            3 => effects.fuzz,
            4 => effects.phosphor,
            _ => effects.wobble,
        }
    }

    /// Reset the whole profile to [`Profile::default`] and rebuild every
    /// control to reflect it.
    fn restore_defaults(&mut self) {
        self.profile = Profile::default();
        self.sync_scheme_radios();
        self.text_size.set_value(permille_from_bounded(
            self.profile.font_size_px,
            MIN_FONT_SIZE_PX,
            MAX_FONT_SIZE_PX,
        ));
        self.swatches = SwatchGrid::from_scheme(&self.profile.custom);
        self.sync_channel_sliders();
        let effects = self.profile.effects;
        let values = [
            effects.opacity,
            effects.blur,
            effects.scanlines,
            effects.fuzz,
            effects.phosphor,
            effects.wobble,
        ];
        for (index, (slider, value)) in self.effect_sliders.iter_mut().zip(values).enumerate() {
            slider.set_value(effect_permille(index, value));
        }
    }
}

/// The physical size the sheet wants, at `scale`.
///
/// A caller that owns the surface the sheet is drawn into — the terminal,
/// which gives each overlay its own popup window — asks for this and makes a
/// surface that large, so the sheet opens at its full size however small the
/// terminal window behind it happens to be. Drawing into a smaller surface
/// still works (the sheet takes whatever room there is and its body scrolls),
/// but the sheet is then cramped for no reason.
#[must_use]
pub fn preferred_extent(scale: Scale) -> (u32, u32) {
    (
        scale.scale_length(MAX_PANEL_WIDTH_PX),
        scale.scale_length(MAX_PANEL_HEIGHT_PX),
    )
}

/// The panel's own bounds within `viewport`: centred, grown up to
/// [`MAX_PANEL_WIDTH_PX`]/[`MAX_PANEL_HEIGHT_PX`], and never larger than the
/// viewport itself — so a small window simply gives the sheet the whole of it
/// rather than an unreachable margin.
fn panel_bounds(viewport: Rect, scale: Scale) -> Rect {
    let width = scale.scale_length(MAX_PANEL_WIDTH_PX).min(viewport.width);
    let height = scale.scale_length(MAX_PANEL_HEIGHT_PX).min(viewport.height);
    let x = viewport.left() + to_i32((viewport.width - width) / 2);
    let y = viewport.top() + to_i32((viewport.height - height) / 2);
    Rect::new(x, y, width, height)
}

/// The *Restore defaults* and *Done* button rectangles within the footer
/// band, shared by rendering and pointer routing.
fn footer_split(rect: Rect, scale: Scale) -> (Option<Rect>, Option<Rect>) {
    let Some((x, y, w, h)) = surface_rect(rect) else {
        return (None, None);
    };
    let gap = scale.scale_length(LABEL_GAP_PX).max(1);
    let each = w.saturating_sub(gap) / 2;
    if each == 0 {
        return (None, None);
    }
    let restore = Rect::new(to_i32(x), to_i32(y), each, h);
    let done = Rect::new(
        to_i32(x + each + gap),
        to_i32(y),
        w.saturating_sub(each + gap),
        h,
    );
    (Some(restore), Some(done))
}

/// Split a slider row into its leading label column and trailing control.
fn split_row(rect: Rect, scale: Scale) -> (Rect, Rect) {
    let label_w = scale.scale_length(LABEL_WIDTH_PX).min(rect.width / 2);
    let gap = scale.scale_length(LABEL_GAP_PX).max(1);
    let label = Rect::new(rect.left(), rect.top(), label_w, rect.height);
    let control_x = rect.left() + to_i32(label_w) + to_i32(gap);
    let control_w = rect.width.saturating_sub(label_w).saturating_sub(gap);
    (
        label,
        Rect::new(control_x, rect.top(), control_w, rect.height),
    )
}

/// Split the custom-editor row into its caption line and the swatch grid
/// beneath it — the one layout [`Settings::render_swatches`] and
/// [`Settings::route_swatches_pointer`] both read.
fn swatch_caption_split(rect: Rect, scale: Scale, font: BitmapFont) -> (Rect, Rect) {
    let caption_h = font.glyph_height().max(1).min(rect.height);
    let gap = scale.scale_length(CAPTION_GAP_PX).max(1);
    let caption = Rect::new(rect.left(), rect.top(), rect.width, caption_h);
    let grid_y = rect.top() + to_i32(caption_h) + to_i32(gap);
    let grid_h = rect.height.saturating_sub(caption_h).saturating_sub(gap);
    (caption, Rect::new(rect.left(), grid_y, rect.width, grid_h))
}

/// Draw one line of `text` vertically centred in `rect`.
fn draw_row_label(surface: &mut Surface, rect: Rect, theme: &Theme, font: BitmapFont, text: &str) {
    let Some((x, y, w, h)) = surface_rect(rect) else {
        return;
    };
    let fitted = font.truncate_to_width(text, w);
    let glyph_h = font.glyph_height();
    let text_y = to_i32(y) + (to_i32(h) - to_i32(glyph_h)).max(0) / 2;
    font.draw_text(
        surface,
        to_i32(x),
        text_y,
        fitted,
        Color::from(theme.palette().on_surface),
    );
}

#[cfg(test)]
#[path = "settings_tests.rs"]
mod tests;
