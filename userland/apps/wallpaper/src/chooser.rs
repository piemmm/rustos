//! The chooser model: what is selected, what the pointer is doing, and what
//! the desktop is being asked for.
//!
//! Every interactive thing in the window is a shared `lib/controls` control
//! held here for the life of the window — the four settings drop-downs, the
//! Apply and Close buttons, and the gallery's scrollbar — so each one owns
//! its own hover, press, drag and focus state exactly as it does everywhere
//! else in the desktop. The gallery's tiles are the one exception the design
//! language names: a tile renders state and never dispatches, so the gallery
//! hit-tests the pointer against the very geometry it painted.
//!
//! The model performs no I/O. Pixels for the preview and for each gallery
//! thumbnail are asked for ([`Chooser::next_preview`],
//! [`Chooser::next_thumbnail`]) and handed back by the caller, which is the
//! only part of the program that may speak to the parser sandbox.

use alloc::string::String;
use alloc::vec::Vec;

use tairix_controls::button::{Button, ButtonAction, ButtonContent};
use tairix_controls::collection::IconTile;
use tairix_controls::combo::{ComboAction, ComboBox};
use tairix_controls::scroll::{ScrollModel, ScrollOrientation, ScrollRange};
use tairix_controls::scrollbar::{ScrollAction, ScrollBar};
use tairix_controls::state::ControlRole;
use tairix_geometry::{Point, Rect};
use tairix_input::{InputEvent, Key, Modifiers, NamedKey, PointerButton};
use tairix_raster::Surface;
use tairix_wallpaper::{
    Backdrop, IconFlow, IconSort, PinboardSettings, WallpaperChoice, WallpaperFit, WallpaperPath,
};

use crate::{
    backdrop_options, leaf_name, to_i32, to_u32, ApplyOutcome, BackdropOption, Candidate,
    ChooserAction, Focus, Layout, OptionGroup, Style, Thumbnail, APPLY_LABEL, CLOSE_LABEL, FIT_ALL,
    FIT_LABELS, ICON_FLOW_ALL, ICON_FLOW_LABELS, MIN_WIN_HEIGHT, MIN_WIN_WIDTH, OPTION_GROUP_COUNT,
    SORT_ALL, SORT_LABELS, WIN_HEIGHT, WIN_WIDTH,
};

/// A wallpaper the caller must render for the preview panel.
///
/// Carries everything the render needs and nothing else, and compares by
/// value: the model holds the request its current pixels came from, so a
/// preview is re-rendered exactly when the selection, the fit, the screen
/// extent, or the model box's size changes, and never otherwise. Keying the
/// held pixels to this whole request — screen included — is what makes a
/// preview rendered for one screen unrepresentable as a preview of another:
/// a screen-extent change can never leave a stale preview on show.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreviewRequest {
    /// The wallpaper to render.
    pub path: WallpaperPath,
    /// The fit to place it with.
    pub fit: WallpaperFit,
    /// The screen extent, in physical pixels, the render must model (the
    /// target-only `Run` binary renders it through the sandbox's
    /// screen-aware wallpaper render, keyed to this same extent).
    pub screen: (u32, u32),
    /// The true-scale model box's width in pixels.
    pub width: u32,
    /// The true-scale model box's height in pixels.
    pub height: u32,
}

/// A wallpaper the caller must render for one gallery tile.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ThumbnailRequest {
    /// The candidate the pixels belong to.
    pub index: usize,
    /// The wallpaper to render.
    pub path: WallpaperPath,
    /// The square side, in pixels, the tile will draw the picture at.
    pub side: u32,
}

/// What the preview panel currently holds.
#[derive(Debug)]
enum PreviewState {
    /// Nothing has been rendered for the held request yet.
    Pending,
    /// The pixels the held request produced.
    Ready(Surface),
    /// The sandbox refused the held request's source.
    Refused,
}

/// The preview panel's pixels and the request they came from.
///
/// Pairing them is what makes a stale preview unrepresentable: the painter
/// asks for the pixels *of a particular request*, so a selection or fit
/// change simply has no pixels yet rather than showing the previous
/// wallpaper.
#[derive(Debug)]
struct PreviewSlot {
    request: Option<PreviewRequest>,
    state: PreviewState,
}

/// The chooser's model.
pub struct Chooser {
    candidates: Vec<Candidate>,
    selected: usize,
    backdrops: Vec<BackdropOption>,
    fields: [ComboBox; OPTION_GROUP_COUNT],
    apply: Button,
    close: Button,
    scroll: ScrollBar,
    focus: Focus,
    outcome: Option<ApplyOutcome>,
    pointer: Point,
    hovered: Option<usize>,
    armed: Option<usize>,
    width: u32,
    height: u32,
    preview: PreviewSlot,
}

impl Chooser {
    /// Build a chooser offering `images` (the wallpaper store's listing,
    /// through [`candidates_from_catalog`](crate::candidates_from_catalog))
    /// with `settings` — the document currently in effect — selected.
    ///
    /// The "no wallpaper" entry is always the first candidate, so a user can
    /// always get back to a plain backdrop. A settings document naming a
    /// wallpaper the store does not list (a file removed since it was set)
    /// still appears, appended under its own leaf name, so the chooser never
    /// silently drops the choice that is actually in effect.
    #[must_use]
    pub fn new(images: Vec<Candidate>, settings: &PinboardSettings) -> Self {
        let mut candidates = Vec::with_capacity(images.len().saturating_add(1));
        candidates.push(Candidate::none_entry());
        candidates.extend(images);
        if let WallpaperChoice::Image(path) = &settings.wallpaper {
            if !candidates
                .iter()
                .any(|candidate| candidate.choice == settings.wallpaper)
            {
                candidates.push(Candidate::image(path.clone(), leaf_name(path)));
            }
        }
        let selected = candidates
            .iter()
            .position(|candidate| candidate.choice == settings.wallpaper)
            .unwrap_or(0);

        let backdrops = backdrop_options(settings.backdrop);
        let backdrop_index = backdrops
            .iter()
            .position(|option| option.backdrop == settings.backdrop)
            .unwrap_or(0);
        let fit_index = FIT_ALL
            .iter()
            .position(|fit| *fit == settings.fit)
            .unwrap_or(0);
        let icons_index = ICON_FLOW_ALL
            .iter()
            .position(|flow| *flow == settings.icons)
            .unwrap_or(0);
        let sort_index = SORT_ALL
            .iter()
            .position(|sort| *sort == settings.sort)
            .unwrap_or(0);

        Self {
            candidates,
            selected,
            fields: [
                field(&FIT_LABELS, fit_index),
                field_from(&backdrops, backdrop_index),
                field(&ICON_FLOW_LABELS, icons_index),
                field(&SORT_LABELS, sort_index),
            ],
            backdrops,
            apply: Button::new(
                ButtonContent::Label(String::from(APPLY_LABEL)),
                ControlRole::Primary,
            ),
            close: Button::labelled(CLOSE_LABEL),
            scroll: ScrollBar::new(
                ScrollOrientation::Vertical,
                ScrollModel::new(ScrollRange::new(0, 0, 0), 1, 1),
            ),
            focus: Focus::Gallery,
            outcome: None,
            pointer: Point::new(-1, -1),
            hovered: None,
            armed: None,
            width: WIN_WIDTH,
            height: WIN_HEIGHT,
            preview: PreviewSlot {
                request: None,
                state: PreviewState::Pending,
            },
        }
    }

    /// Adopt a new client size, never below the window's own floor.
    ///
    /// Only the size is stored: every derived extent — the layout, the
    /// gallery's grid, the scroll range — is resolved afresh from it on the
    /// next paint or event, so a resize can leave nothing stale behind.
    pub fn relayout(&mut self, width: u32, height: u32) {
        self.width = width.max(MIN_WIN_WIDTH);
        self.height = height.max(MIN_WIN_HEIGHT);
    }

    /// The candidates the gallery offers, in gallery order.
    #[must_use]
    pub fn candidates(&self) -> &[Candidate] {
        &self.candidates
    }

    /// The selected candidate's index.
    #[must_use]
    pub fn selected(&self) -> usize {
        self.selected
    }

    /// Which region holds keyboard focus.
    #[must_use]
    pub fn focus(&self) -> Focus {
        self.focus
    }

    /// The gallery's scroll offset, in whole tile rows.
    #[must_use]
    pub fn scroll_offset(&self) -> u64 {
        self.scroll.model().offset()
    }

    /// The fit currently chosen.
    #[must_use]
    pub fn fit(&self) -> WallpaperFit {
        FIT_ALL
            .get(self.choice_index(OptionGroup::Fit))
            .copied()
            .unwrap_or(WallpaperFit::Fill)
    }

    /// The backdrop currently chosen.
    #[must_use]
    pub fn backdrop(&self) -> Backdrop {
        self.backdrops
            .get(self.choice_index(OptionGroup::Backdrop))
            .map_or(Backdrop::Theme, |option| option.backdrop)
    }

    /// The icon arrangement currently chosen.
    #[must_use]
    pub fn icons(&self) -> IconFlow {
        ICON_FLOW_ALL
            .get(self.choice_index(OptionGroup::Icons))
            .copied()
            .unwrap_or(IconFlow::Leading)
    }

    /// The icon sort order currently chosen.
    #[must_use]
    pub fn sort(&self) -> IconSort {
        SORT_ALL
            .get(self.choice_index(OptionGroup::Sort))
            .copied()
            .unwrap_or(IconSort::Name)
    }

    /// The outcome of the last apply, or `None` before the first attempt.
    #[must_use]
    pub fn apply_outcome(&self) -> Option<&ApplyOutcome> {
        self.outcome.as_ref()
    }

    /// Record what the desktop session answered to the last apply.
    pub fn set_apply_outcome(&mut self, outcome: ApplyOutcome) {
        self.outcome = Some(outcome);
    }

    /// The settings document the current state means.
    #[must_use]
    pub fn to_settings(&self) -> PinboardSettings {
        PinboardSettings {
            wallpaper: self
                .candidates
                .get(self.selected)
                .map_or(WallpaperChoice::None, |candidate| candidate.choice.clone()),
            fit: self.fit(),
            backdrop: self.backdrop(),
            icons: self.icons(),
            sort: self.sort(),
        }
    }

    /// Render [`Self::to_settings`] as the canonical document text, ready to
    /// post to the desktop session (`plans/PINBOARD.md` §6).
    #[must_use]
    pub fn settings_document(&self) -> String {
        tairix_wallpaper::settings::render(&self.to_settings())
    }

    /// The wallpaper the preview panel is showing, whether or not its pixels
    /// exist yet — `None` only when the true-scale model box has no room, or
    /// when the selection is the plain backdrop, which decodes nothing.
    #[must_use]
    pub fn wanted_preview(&self, style: Style<'_>) -> Option<PreviewRequest> {
        let model = self.layout(style).preview_model();
        if model.is_empty() {
            return None;
        }
        let WallpaperChoice::Image(path) = self.candidates.get(self.selected)?.choice.clone()
        else {
            return None;
        };
        Some(PreviewRequest {
            path,
            fit: self.fit(),
            screen: style.screen(),
            width: model.width,
            height: model.height,
        })
    }

    /// The wallpaper the preview panel needs rendering, or `None` when the
    /// held pixels (or a remembered refusal) already answer the current
    /// selection, fit and panel size.
    #[must_use]
    pub fn next_preview(&self, style: Style<'_>) -> Option<PreviewRequest> {
        let want = self.wanted_preview(style)?;
        (self.preview.request.as_ref() != Some(&want)).then_some(want)
    }

    /// Adopt `surface` as the pixels `request` asked for.
    pub fn set_preview(&mut self, request: PreviewRequest, surface: Surface) {
        self.preview = PreviewSlot {
            request: Some(request),
            state: PreviewState::Ready(surface),
        };
    }

    /// Record that the sandbox refused `request`, so the panel says so
    /// instead of asking again on every paint.
    pub fn mark_preview_refused(&mut self, request: PreviewRequest) {
        self.preview = PreviewSlot {
            request: Some(request),
            state: PreviewState::Refused,
        };
    }

    /// The first gallery thumbnail still to be rendered, or `None` when every
    /// candidate is resolved (or the window is too small to show a tile).
    ///
    /// A thumbnail already held at a different square side is stale and is
    /// asked for again: the tile's side follows the desktop's UI scale, and a
    /// tile painter centres the artwork it is given rather than stretching
    /// it, so keeping pixels rendered for the old scale would draw the
    /// picture at the wrong size. A refusal stays a refusal at every side,
    /// since a file the worker could not decode will not decode smaller.
    #[must_use]
    pub fn next_thumbnail(&self, style: Style<'_>) -> Option<ThumbnailRequest> {
        let (width, height) = self.layout(style).tile_size();
        let side =
            IconTile::icon_side(Rect::new(0, 0, width, height), style.scale(), style.theme());
        if side == 0 {
            return None;
        }
        self.candidates
            .iter()
            .enumerate()
            .find_map(
                |(index, candidate)| match (&candidate.thumbnail, &candidate.choice) {
                    (Thumbnail::Pending, WallpaperChoice::Image(path)) => Some(ThumbnailRequest {
                        index,
                        path: path.clone(),
                        side,
                    }),
                    (Thumbnail::Ready(held), WallpaperChoice::Image(path))
                        if held.width() != side =>
                    {
                        Some(ThumbnailRequest {
                            index,
                            path: path.clone(),
                            side,
                        })
                    }
                    _ => None,
                },
            )
    }

    /// Adopt `surface` as the thumbnail of the candidate at `index`.
    pub fn set_thumbnail(&mut self, index: usize, surface: Surface) {
        if let Some(candidate) = self.candidates.get_mut(index) {
            candidate.thumbnail = Thumbnail::Ready(surface);
        }
    }

    /// Record that the sandbox refused the candidate at `index`, so it is
    /// asked for once and never again this session.
    pub fn mark_thumbnail_refused(&mut self, index: usize) {
        if let Some(candidate) = self.candidates.get_mut(index) {
            candidate.thumbnail = Thumbnail::Refused;
        }
    }

    /// Feed one pointer event, reporting what the user asked for.
    ///
    /// Events are routed in the order the surfaces overlap on screen: an
    /// expanded drop-down owns the pointer until it closes, then the
    /// collapsed fields, the buttons, the gallery's scrollbar, and finally
    /// the gallery itself. Each control decides for itself whether the
    /// pointer is inside it, so a press that lands on one control and a
    /// release that lands on another activate nothing at all.
    pub fn on_pointer(&mut self, event: &InputEvent, style: Style<'_>) -> ChooserAction {
        let layout = self.sync(style);
        if let InputEvent::PointerMoved { to } = event {
            self.pointer = *to;
        }

        if let Some(group) = self.expanded() {
            let changed = self.field_pointer(group, event, &layout, style);
            return ChooserAction::changed(changed);
        }

        let mut changed = false;
        for group in OptionGroup::ALL {
            changed |= self.field_pointer(group, event, &layout, style);
        }
        if self.expanded().is_some() {
            // A field just opened: the click is spent, and nothing beneath
            // the popup may also act on it.
            return ChooserAction::Changed;
        }

        let (apply_changed, applied) = button_pointer(&mut self.apply, event, layout.apply());
        if applied {
            self.focus = Focus::Apply;
            return ChooserAction::Apply;
        }
        let (close_changed, closed) = button_pointer(&mut self.close, event, layout.close());
        if closed {
            return ChooserAction::Close;
        }
        changed |= apply_changed | close_changed;

        changed |= self.scroll_pointer(event, &layout, style);
        changed |= self.gallery_pointer(event, &layout);
        ChooserAction::changed(changed)
    }

    /// Feed one key press, reporting what the user asked for.
    ///
    /// The keyboard is the secondary path: it reaches everything the pointer
    /// does, through the focus order Tab walks, and an expanded drop-down
    /// owns the keyboard exactly as it owns the pointer.
    pub fn on_key(&mut self, key: Key, modifiers: Modifiers, style: Style<'_>) -> ChooserAction {
        let layout = self.sync(style);

        if let Some(group) = self.expanded() {
            let action = self.fields[group.index()].on_key(key);
            return ChooserAction::changed(action.is_some());
        }

        match key {
            Key::Named(NamedKey::Tab) => {
                self.focus = if modifiers.shift {
                    self.focus.prev()
                } else {
                    self.focus.next()
                };
                ChooserAction::Changed
            }
            Key::Named(NamedKey::Escape) => ChooserAction::Close,
            _ => match self.focus {
                Focus::Gallery => self.gallery_key(key, &layout),
                Focus::Setting(group) => {
                    ChooserAction::changed(self.fields[group.index()].on_key(key).is_some())
                }
                Focus::Apply => match self.apply.on_key(key) {
                    Some(ButtonAction::Activated) => ChooserAction::Apply,
                    None => ChooserAction::None,
                },
                Focus::Close => match self.close.on_key(key) {
                    Some(ButtonAction::Activated) => ChooserAction::Close,
                    None => ChooserAction::None,
                },
            },
        }
    }

    /// Paint the chooser into a `width` x `height` surface, returning `None`
    /// only when the surface cannot be allocated.
    ///
    /// Painting takes `&mut self` because it refreshes exactly the derived
    /// geometry the hit-test uses — the layout and the gallery's scroll range
    /// — through the one shared path both go through, so what was drawn and
    /// what a click is tested against cannot drift apart.
    #[must_use]
    pub fn render(&mut self, style: Style<'_>) -> Option<Surface> {
        let layout = self.sync(style);
        crate::paint::render(self, &layout, style)
    }

    /// The window's current client size.
    #[must_use]
    pub(crate) fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// The candidate the pointer is over, if any.
    #[must_use]
    pub(crate) fn hovered(&self) -> Option<usize> {
        self.hovered
    }

    /// The candidate a primary press is currently latched on, if any.
    #[must_use]
    pub(crate) fn armed(&self) -> Option<usize> {
        self.armed
    }

    /// One settings drop-down.
    #[must_use]
    pub(crate) fn field(&self, group: OptionGroup) -> &ComboBox {
        &self.fields[group.index()]
    }

    /// The group whose drop-down is expanded, if any.
    #[must_use]
    pub(crate) fn expanded(&self) -> Option<OptionGroup> {
        OptionGroup::ALL
            .into_iter()
            .find(|group| self.fields[group.index()].is_expanded())
    }

    /// The Apply button.
    #[must_use]
    pub(crate) fn apply_button(&self) -> &Button {
        &self.apply
    }

    /// The Close button.
    #[must_use]
    pub(crate) fn close_button(&self) -> &Button {
        &self.close
    }

    /// The gallery's scrollbar.
    #[must_use]
    pub(crate) fn scrollbar(&self) -> &ScrollBar {
        &self.scroll
    }

    /// The pixels the preview panel should draw for `want`, or `None` when
    /// they have not been rendered yet.
    #[must_use]
    pub(crate) fn preview_surface(&self, want: &PreviewRequest) -> Option<&Surface> {
        match (&self.preview.request, &self.preview.state) {
            (Some(held), PreviewState::Ready(surface)) if held == want => Some(surface),
            _ => None,
        }
    }

    /// Whether the sandbox refused the wallpaper `want` names.
    #[must_use]
    pub(crate) fn preview_refused(&self, want: &PreviewRequest) -> bool {
        match (&self.preview.request, &self.preview.state) {
            (Some(held), PreviewState::Refused) => held.path == want.path,
            _ => false,
        }
    }

    /// Where the expanded drop-down of `group` is drawn: directly below its
    /// field where the window has room, flipped above it where it does not,
    /// and never past either side of the window.
    #[must_use]
    pub(crate) fn popup_rect(&self, group: OptionGroup, layout: &Layout, style: Style<'_>) -> Rect {
        let field = layout.option_field(group);
        let (width, height) =
            self.fields[group.index()].popup_size(field.width, style.scale(), style.theme());
        let below = field.bottom();
        let y = if to_u32(below).saturating_add(height) <= self.height {
            below
        } else {
            field.top().saturating_sub(to_i32(height))
        };
        let right_limit = to_i32(self.width.saturating_sub(width.min(self.width)));
        Rect::new(field.left().min(right_limit).max(0), y, width, height)
    }

    /// The window geometry for the current size, with no side effect.
    #[must_use]
    pub(crate) fn layout(&self, style: Style<'_>) -> Layout {
        Layout::compute(
            self.width,
            self.height,
            style.scale(),
            style.theme(),
            style.font(),
            style.screen(),
        )
    }

    /// Resolve the layout and bring every control's derived state in line
    /// with it: the scroll range the gallery's grid implies, and the focus
    /// flag each control paints its Focus Ring from.
    ///
    /// Every entry point starts here, so the geometry a click is tested
    /// against is the geometry that was painted, and the keyboard's focus is
    /// never on a control that does not know it has it.
    fn sync(&mut self, style: Style<'_>) -> Layout {
        let layout = self.layout(style);
        let grid = layout.grid(self.candidates.len());
        let range = grid.scroll_range(self.scroll.model().offset());
        let page = u64::try_from(grid.visible_lines()).unwrap_or(1).max(1);
        self.scroll.set_model(ScrollModel::new(range, 1, page));

        for group in OptionGroup::ALL {
            self.fields[group.index()].set_focused(self.focus == Focus::Setting(group));
        }
        self.apply.set_focused(self.focus == Focus::Apply);
        self.close.set_focused(self.focus == Focus::Close);
        layout
    }

    /// The index the drop-down of `group` has selected.
    fn choice_index(&self, group: OptionGroup) -> usize {
        self.fields[group.index()].selected().unwrap_or(0)
    }

    /// Route a pointer event to one settings drop-down, reporting whether
    /// anything it draws changed.
    fn field_pointer(
        &mut self,
        group: OptionGroup,
        event: &InputEvent,
        layout: &Layout,
        style: Style<'_>,
    ) -> bool {
        let field = layout.option_field(group);
        // A collapsed field never reads its popup rectangle, and measuring
        // one costs a pass over every choice's text, so it is resolved only
        // for the field that is actually showing a list.
        let popup = if self.fields[group.index()].is_expanded() {
            self.popup_rect(group, layout, style)
        } else {
            Rect::new(0, 0, 0, 0)
        };
        let combo = &mut self.fields[group.index()];
        let before = (combo.state(), combo.is_expanded(), combo.selected());
        let action = combo.on_pointer(event, field, popup, style.scale(), style.theme());
        let after = (combo.state(), combo.is_expanded(), combo.selected());
        if matches!(action, Some(ComboAction::Opened)) {
            self.focus = Focus::Setting(group);
        }
        before != after
    }

    /// Route a pointer event to the gallery's scrollbar and to the wheel,
    /// reporting whether the gallery moved or the bar's own paint changed.
    ///
    /// The bar carries the gallery's scroll offset, and a wheel tick or a
    /// thumb drag moves it as part of handling the event — the returned
    /// request is the bar telling its owner where it has gone. Whether
    /// anything changed is therefore decided by what the offset (and the
    /// bar's own hover, press and held part) were *before* the event, never
    /// by comparing the request against a model that has already followed
    /// it.
    fn scroll_pointer(&mut self, event: &InputEvent, layout: &Layout, style: Style<'_>) -> bool {
        let before = (
            self.scroll.model().offset(),
            self.scroll.state(),
            self.scroll.held(),
        );
        let action = match event {
            InputEvent::PointerScrolled { dx, dy } => self.scroll.wheel(*dx, *dy),
            _ => self
                .scroll
                .on_pointer(event, layout.scrollbar(), style.scale(), style.theme()),
        };
        if let Some(ScrollAction::ScrollTo { offset }) = action {
            self.scroll_to(offset);
        }
        before
            != (
                self.scroll.model().offset(),
                self.scroll.state(),
                self.scroll.held(),
            )
    }

    /// Move the gallery to `offset`, reporting whether it was somewhere else
    /// before.
    fn scroll_to(&mut self, offset: u64) -> bool {
        let model = self.scroll.model();
        if model.offset() == offset {
            return false;
        }
        self.scroll.set_model(model.scroll_to(offset));
        true
    }

    /// Route a pointer event to the gallery's tiles, reporting whether the
    /// hover, the press latch, or the selection changed.
    ///
    /// The gallery owns this hit-test rather than the tiles: an icon tile
    /// paints state and holds no pointer of its own, so the view resolves the
    /// pointer against the very grid it painted.
    fn gallery_pointer(&mut self, event: &InputEvent, layout: &Layout) -> bool {
        let grid = layout.grid(self.candidates.len());
        let at = grid.index_at(
            self.scroll.model().offset(),
            to_u32(self.pointer.x),
            to_u32(self.pointer.y),
        );
        match event {
            InputEvent::PointerMoved { .. } => {
                let changed = self.hovered != at;
                self.hovered = at;
                changed
            }
            InputEvent::PointerPressed {
                button: PointerButton::Primary,
            } => {
                let changed = self.armed != at;
                self.armed = at;
                if at.is_some() {
                    self.focus = Focus::Gallery;
                }
                changed
            }
            InputEvent::PointerReleased {
                button: PointerButton::Primary,
            } => {
                let armed = self.armed.take();
                let selected = match armed {
                    Some(index) if Some(index) == at => self.select(index),
                    _ => false,
                };
                selected || armed.is_some()
            }
            _ => false,
        }
    }

    /// Move the gallery's selection with the arrow keys, revealing the tile
    /// that gains it.
    fn gallery_key(&mut self, key: Key, layout: &Layout) -> ChooserAction {
        let grid = layout.grid(self.candidates.len());
        let per_line = grid.cells_per_line().max(1);
        let last = self.candidates.len().saturating_sub(1);
        let target = match key {
            Key::Named(NamedKey::Left) => self.selected.checked_sub(1),
            Key::Named(NamedKey::Right) => Some(self.selected.saturating_add(1).min(last)),
            Key::Named(NamedKey::Up) => self.selected.checked_sub(per_line),
            Key::Named(NamedKey::Down) => Some(self.selected.saturating_add(per_line).min(last)),
            Key::Named(NamedKey::Home) => Some(0),
            Key::Named(NamedKey::End) => Some(last),
            Key::Named(NamedKey::Enter) => return ChooserAction::Apply,
            _ => return ChooserAction::None,
        };
        let Some(target) = target else {
            return ChooserAction::None;
        };
        let changed = self.select(target);
        let revealed = grid.reveal(self.scroll.model().offset(), Some(self.selected));
        let moved = self.scroll_to(revealed);
        ChooserAction::changed(changed || moved)
    }

    /// Select the candidate at `index`, reporting whether the selection
    /// actually moved.
    fn select(&mut self, index: usize) -> bool {
        if index >= self.candidates.len() || index == self.selected {
            return false;
        }
        self.selected = index;
        true
    }
}

/// One settings drop-down over a fixed set of human choice names.
fn field(labels: &[&str], selected: usize) -> ComboBox {
    ComboBox::new(labels.iter().map(|label| String::from(*label)).collect()).with_selected(selected)
}

/// The backdrop drop-down, whose choices are discovered rather than fixed
/// (the palette plus whatever colour is already in effect).
fn field_from(options: &[BackdropOption], selected: usize) -> ComboBox {
    ComboBox::new(options.iter().map(|option| option.label.clone()).collect())
        .with_selected(selected)
}

/// Route a pointer event to a button: `(whether its paint changed, whether it
/// was activated)`.
///
/// The button reports only its activation, but a hover or a press changes
/// what it draws, so the state either side of the event is compared: a
/// repaint happens exactly when the user would see a difference.
fn button_pointer(button: &mut Button, event: &InputEvent, bounds: Rect) -> (bool, bool) {
    let before = button.state();
    let fired = matches!(
        button.on_pointer(event, bounds),
        Some(ButtonAction::Activated)
    );
    (button.state() != before, fired)
}
