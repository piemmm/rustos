//! The chooser model: what is selected, what the pointer is doing, and what
//! the desktop is being asked for.
//!
//! Every interactive thing in the window is a shared `lib/controls` control
//! held here for the life of the window — the four settings drop-downs, the
//! category rail, the Apply and Close buttons, and the gallery's scrollbar —
//! so each one owns
//! its own hover, press, drag and focus state exactly as it does everywhere
//! else in the desktop. The gallery's tiles are the one exception the design
//! language names: a tile renders state and never dispatches, so the gallery
//! hit-tests the pointer against the very geometry it painted.
//!
//! The rail narrows the gallery rather than replacing its contents: the model
//! holds every candidate once, and [`Chooser::visible`] is the window into it
//! the active rail entry implies. The grid, the scroll range, the hit-test and
//! the painter all work in that window's own positions, so a click can never
//! land on a candidate the active category was not showing.
//!
//! The model performs no I/O. Pixels for the preview and for each gallery
//! thumbnail are asked for ([`Chooser::next_preview`],
//! [`Chooser::next_thumbnail`]) and handed back by the caller, which is the
//! only part of the program that may speak to the parser sandbox.

use alloc::collections::BTreeSet;
use alloc::string::String;
use alloc::vec::Vec;

use tairix_controls::button::{Button, ButtonAction, ButtonContent};
use tairix_controls::collection::IconTile;
use tairix_controls::combo::{ComboAction, ComboBox};
use tairix_controls::damage;
use tairix_controls::scroll::{ScrollModel, ScrollOrientation, ScrollRange};
use tairix_controls::scrollbar::ScrollBar;
use tairix_controls::state::ControlRole;
use tairix_controls::tabs::{Tab, Tabs, TabsAction, TabsOrientation};
use tairix_geometry::{Point, Rect, Region};
use tairix_input::{InputEvent, Key, Modifiers, NamedKey, PointerButton};
use tairix_raster::Surface;
use tairix_wallpaper::{
    catalog_categories, Backdrop, IconFlow, IconSort, PinboardSettings, WallpaperChoice,
    WallpaperFit, WallpaperPath,
};

use crate::{
    backdrop_options, leaf_name, to_i32, to_u32, ApplyOutcome, BackdropOption, Candidate,
    ChooserAction, Focus, Layout, OptionGroup, Style, Thumbnail, ALL_CATEGORIES_LABEL, APPLY_LABEL,
    CLOSE_LABEL, FIT_ALL, FIT_LABELS, ICON_FLOW_ALL, ICON_FLOW_LABELS, MIN_WIN_HEIGHT,
    MIN_WIN_WIDTH, OPTION_GROUP_COUNT, SORT_ALL, SORT_LABELS, WIN_HEIGHT, WIN_WIDTH,
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
    categories: Vec<String>,
    rail: Tabs,
    active: usize,
    visible: Vec<usize>,
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
    /// still appears, appended under its own leaf name and under no category,
    /// so the chooser never silently drops the choice that is actually in
    /// effect.
    ///
    /// The category rail is *derived* from the categories `images` are filed
    /// under, so it offers exactly what the store holds and this app carries
    /// no list of its own. It opens on the category holding the selection, so
    /// the wallpaper in effect is shown in the company it was chosen from.
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
                candidates.push(Candidate::image(path.clone(), leaf_name(path), None));
            }
        }
        let selected = candidates
            .iter()
            .position(|candidate| candidate.choice == settings.wallpaper)
            .unwrap_or(0);

        let categories = discovered_categories(&candidates);
        let active = candidates
            .get(selected)
            .and_then(|candidate| candidate.category.as_deref())
            .and_then(|name| categories.iter().position(|owned| owned == name))
            .map_or(0, |slot| slot.saturating_add(1));
        let mut rail = category_rail(&categories);
        rail.adopt_selected(active);
        let visible = visible_indices(&candidates, category_at(&categories, active));

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
            categories,
            rail,
            active,
            visible,
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

    /// Every candidate the chooser holds, whichever category is being
    /// browsed.
    #[must_use]
    pub fn candidates(&self) -> &[Candidate] {
        &self.candidates
    }

    /// The categories the rail offers beneath its leading "all" entry, in
    /// rail order.
    #[must_use]
    pub fn categories(&self) -> &[String] {
        &self.categories
    }

    /// The category currently narrowing the gallery, or `None` when the rail
    /// is on its "all" entry.
    #[must_use]
    pub fn active_category(&self) -> Option<&str> {
        category_at(&self.categories, self.active)
    }

    /// The candidates the gallery is showing, as indices into
    /// [`Self::candidates`], in gallery order.
    #[must_use]
    pub fn visible(&self) -> &[usize] {
        &self.visible
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

    /// Record what the desktop session answered to the last apply, reporting
    /// the status line that states it.
    ///
    /// The owner commits this value, so the owner reports it: it holds the
    /// layout at exactly the moment it writes.
    pub fn set_apply_outcome(
        &mut self,
        outcome: ApplyOutcome,
        style: Style<'_>,
        damage: &mut Region,
    ) {
        self.outcome = Some(outcome);
        damage.add(self.layout(style).status());
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
    ///
    /// The chooser posts a document rather than writing one: an application
    /// publishes only its *own* app-data scope, so this program cannot write
    /// the desktop's settings at all — it asks, and the session decides
    /// (`plans/APPDATA.md` §3.11).
    #[must_use]
    pub fn settings_document(&self) -> String {
        self.to_settings().document().render()
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
    pub fn set_preview(
        &mut self,
        request: PreviewRequest,
        surface: Surface,
        style: Style<'_>,
        damage: &mut Region,
    ) {
        self.preview = PreviewSlot {
            request: Some(request),
            state: PreviewState::Ready(surface),
        };
        self.mark_preview(style, damage);
    }

    /// Record that the sandbox refused `request`, so the panel says so
    /// instead of asking again on every paint.
    pub fn mark_preview_refused(
        &mut self,
        request: PreviewRequest,
        style: Style<'_>,
        damage: &mut Region,
    ) {
        self.preview = PreviewSlot {
            request: Some(request),
            state: PreviewState::Refused,
        };
        self.mark_preview(style, damage);
    }

    /// Report the model box a delivered or refused preview redraws.
    fn mark_preview(&self, style: Style<'_>, damage: &mut Region) {
        damage.add(self.layout(style).preview_model());
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
    pub fn set_thumbnail(
        &mut self,
        index: usize,
        surface: Surface,
        style: Style<'_>,
        damage: &mut Region,
    ) {
        if let Some(candidate) = self.candidates.get_mut(index) {
            candidate.thumbnail = Thumbnail::Ready(surface);
        }
        self.mark_thumbnail(index, style, damage);
    }

    /// Record that the sandbox refused the candidate at `index`, so it is
    /// asked for once and never again this session.
    pub fn mark_thumbnail_refused(&mut self, index: usize, style: Style<'_>, damage: &mut Region) {
        if let Some(candidate) = self.candidates.get_mut(index) {
            candidate.thumbnail = Thumbnail::Refused;
        }
        self.mark_thumbnail(index, style, damage);
    }

    /// Report the one tile a delivered or refused thumbnail redraws. A
    /// candidate the active category is not showing, or one scrolled out of
    /// view, has no tile on screen and so reports nothing.
    fn mark_thumbnail(&self, index: usize, style: Style<'_>, damage: &mut Region) {
        if let Some(rect) = self.candidate_rect(index, &self.layout(style)) {
            damage.add(rect);
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
    pub fn on_pointer(
        &mut self,
        event: &InputEvent,
        style: Style<'_>,
        damage: &mut Region,
    ) -> ChooserAction {
        let layout = self.sync(style);
        if let InputEvent::PointerMoved { to } = event {
            self.pointer = *to;
        }

        if let Some(group) = self.expanded() {
            let changed = self.field_pointer(group, event, &layout, style, damage);
            return ChooserAction::changed(changed);
        }

        let mut changed = false;
        for group in OptionGroup::ALL {
            changed |= self.field_pointer(group, event, &layout, style, damage);
        }
        if self.expanded().is_some() {
            // A field just opened: the click is spent, and nothing beneath
            // the popup may also act on it.
            return ChooserAction::Changed;
        }

        let (apply_changed, applied) =
            button_pointer(&mut self.apply, event, layout.apply(), damage);
        if applied {
            damage::move_mark(
                Some(self.focus),
                Some(Focus::Apply),
                |region| self.focus_rect(region, &layout),
                damage,
            );
            self.focus = Focus::Apply;
            return ChooserAction::Apply;
        }
        let (close_changed, closed) =
            button_pointer(&mut self.close, event, layout.close(), damage);
        if closed {
            return ChooserAction::Close;
        }
        changed |= apply_changed | close_changed;

        changed |= self.rail_pointer(event, &layout, damage);
        changed |= self.scroll_pointer(event, &layout, style, damage);
        changed |= self.gallery_pointer(event, &layout, damage);
        ChooserAction::changed(changed)
    }

    /// Feed one key press, reporting what the user asked for.
    ///
    /// The keyboard is the secondary path: it reaches everything the pointer
    /// does, through the focus order Tab walks, and an expanded drop-down
    /// owns the keyboard exactly as it owns the pointer.
    pub fn on_key(
        &mut self,
        key: Key,
        modifiers: Modifiers,
        style: Style<'_>,
        damage: &mut Region,
    ) -> ChooserAction {
        let layout = self.sync(style);

        if let Some(group) = self.expanded() {
            let action = self.field_key(group, key, &layout, style, damage);
            return ChooserAction::changed(action.is_some());
        }

        match key {
            Key::Named(NamedKey::Tab) => {
                let next = if modifiers.shift {
                    self.focus.prev()
                } else {
                    self.focus.next()
                };
                // The ring is the window's own mark, drawn on one region at a
                // time, so the two that change are the one it leaves and the
                // one it lands on.
                let moved = damage::move_mark(
                    Some(self.focus),
                    Some(next),
                    |region| self.focus_rect(region, &layout),
                    damage,
                );
                self.focus = next;
                ChooserAction::changed(moved)
            }
            Key::Named(NamedKey::Escape) => ChooserAction::Close,
            _ => match self.focus {
                Focus::Categories => ChooserAction::changed(self.rail_key(key, &layout, damage)),
                Focus::Gallery => self.gallery_key(key, &layout, damage),
                Focus::Setting(group) => ChooserAction::changed(
                    self.field_key(group, key, &layout, style, damage).is_some(),
                ),
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

    /// Where the keyboard ring is drawn for `region` in the current layout.
    ///
    /// The gallery's ring is on the selected tile, so a selection scrolled out
    /// of view lays out nowhere and reports nothing — which is right: no ring
    /// is on screen to erase or draw.
    fn focus_rect(&self, region: Focus, layout: &Layout) -> Option<Rect> {
        let rect = match region {
            Focus::Categories => layout.categories(),
            Focus::Gallery => return self.candidate_rect(self.selected, layout),
            Focus::Setting(group) => layout.option_field(group),
            Focus::Apply => layout.apply(),
            Focus::Close => layout.close(),
        };
        (!rect.is_empty()).then_some(rect)
    }

    /// Paint the chooser into the caller's retained window `surface`.
    ///
    /// Painting takes `&mut self` because it refreshes exactly the derived
    /// geometry the hit-test uses — the layout and the gallery's scroll range
    /// — through the one shared path both go through, so what was drawn and
    /// what a click is tested against cannot drift apart.
    ///
    /// The surface is the host's, held for the life of the window, so a caller
    /// that has narrowed its clip to the rectangle a round reported redraws
    /// only that band: every pixel outside it is the one already on screen.
    pub fn render_into(&mut self, surface: &mut Surface, style: Style<'_>) {
        let layout = self.sync(style);
        crate::paint::render_into(surface, self, &layout, style);
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

    /// The category rail.
    #[must_use]
    pub(crate) fn rail(&self) -> &Tabs {
        &self.rail
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
            self.rail_width(style),
        )
    }

    /// The width the category rail asks for: the strip's own measurement,
    /// widened to hold its longest label in the face the strip draws it in,
    /// and zero when there are no categories to offer.
    ///
    /// The strip's own measurement is a fixed cross-axis extent that reserves
    /// its bead but knows nothing of the labels stacked down it, so a rail of
    /// names has to be measured here — by its owner, which is what holds
    /// them.
    fn rail_width(&self, style: Style<'_>) -> u32 {
        if self.rail.is_empty() {
            return 0;
        }
        let font = style.body_font();
        let inset = style
            .scale()
            .scale_length(style.theme().metrics().control_inset)
            .max(1);
        let widest = self
            .rail
            .tabs()
            .iter()
            .map(|tab| font.text_width(tab.label()))
            .max()
            .unwrap_or(0);
        self.rail
            .measured_extent(style.scale(), style.theme())
            .max(widest.saturating_add(inset.saturating_mul(2)))
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
        let grid = layout.grid(self.visible.len());
        let range = grid.scroll_range(self.scroll.model().offset());
        let page = u64::try_from(grid.visible_lines()).unwrap_or(1).max(1);
        self.scroll.set_model(ScrollModel::new(range, 1, page));

        for group in OptionGroup::ALL {
            self.fields[group.index()].set_focused(self.focus == Focus::Setting(group));
        }
        self.apply.set_focused(self.focus == Focus::Apply);
        self.close.set_focused(self.focus == Focus::Close);
        // The rail draws its focus ring around the keyboard cursor, so the
        // cursor is put on the strip when the rail holds focus and taken off
        // it when it does not — never moved, or the arrow keys would be
        // undone on the next paint.
        match (self.focus == Focus::Categories, self.rail.current()) {
            (true, None) => self.rail.adopt_current(Some(self.active)),
            (false, Some(_)) => self.rail.adopt_current(None),
            _ => {}
        }
        layout
    }

    /// Route a pointer event to the category rail, reporting whether anything
    /// it draws changed.
    ///
    /// The strip reports its own repainted pixels rather than exposing its
    /// hover, so its damage is collected here and forwarded: a hover that
    /// moved between two entries is a change, an idle sample over the one it
    /// is already on is not.
    fn rail_pointer(&mut self, event: &InputEvent, layout: &Layout, damage: &mut Region) -> bool {
        let bounds = layout.categories();
        if bounds.is_empty() {
            return false;
        }
        let mut reported = damage::sink();
        let action = self.rail.on_pointer(event, bounds, &mut reported);
        let mut changed = !reported.is_empty();
        for rect in reported.rects() {
            damage.add(*rect);
        }
        if let Some(TabsAction::Selected { index }) = action {
            self.focus = Focus::Categories;
            changed |= self.select_category(index, layout, damage);
        }
        changed
    }

    /// Route a key press to the category rail: the arrows move its cursor,
    /// Enter or Space narrows the gallery to the entry the cursor is on.
    fn rail_key(&mut self, key: Key, layout: &Layout, damage: &mut Region) -> bool {
        let bounds = layout.categories();
        if bounds.is_empty() {
            return false;
        }
        let before = damage.rects().len();
        let action = self.rail.on_key(key, bounds, damage);
        let mut changed = damage.rects().len() != before;
        if let Some(TabsAction::Selected { index }) = action {
            changed |= self.select_category(index, layout, damage);
        }
        changed
    }

    /// Narrow the gallery to rail entry `index`, reporting whether it was on
    /// a different one before.
    ///
    /// The selection is deliberately left where it is: it is the wallpaper
    /// that will be applied, and the preview and caption go on showing it
    /// even while a category that does not hold it is being browsed. The
    /// gallery returns to its top, because the rows it was scrolled to belong
    /// to the entry being left.
    fn select_category(&mut self, index: usize, layout: &Layout, damage: &mut Region) -> bool {
        if index >= self.rail.len() || index == self.active {
            return false;
        }
        self.active = index;
        self.rail.set_selected(index, layout.categories(), damage);
        let category = category_at(&self.categories, index);
        self.visible = visible_indices(&self.candidates, category);
        self.scroll_to(0, layout, damage);
        // Reported whether or not that moved: a gallery already at its top
        // still shows a different candidate in every tile.
        mark_gallery(layout, damage);
        true
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
        damage: &mut Region,
    ) -> bool {
        let field = layout.option_field(group);
        let popup = self.field_popup(group, layout, style);
        let combo = &mut self.fields[group.index()];
        let before = (combo.state(), combo.is_expanded(), combo.selected());
        let action = combo.on_pointer(event, field, popup, style.scale(), style.theme(), damage);
        let after = (combo.state(), combo.is_expanded(), combo.selected());
        if matches!(action, Some(ComboAction::Opened)) {
            self.focus = Focus::Setting(group);
        }
        before != after
    }

    /// Route a key press to one settings drop-down, at the same two
    /// rectangles the pointer path resolves.
    fn field_key(
        &mut self,
        group: OptionGroup,
        key: Key,
        layout: &Layout,
        style: Style<'_>,
        damage: &mut Region,
    ) -> Option<ComboAction> {
        let field = layout.option_field(group);
        let popup = self.field_popup(group, layout, style);
        self.fields[group.index()].on_key(key, field, popup, style.scale(), style.theme(), damage)
    }

    /// The popup rectangle of `group`'s drop-down.
    ///
    /// A collapsed field never reads one, and measuring it costs a pass over
    /// every choice's text, so it is resolved only for the field that is
    /// actually showing a list.
    fn field_popup(&self, group: OptionGroup, layout: &Layout, style: Style<'_>) -> Rect {
        if self.fields[group.index()].is_expanded() {
            self.popup_rect(group, layout, style)
        } else {
            Rect::new(0, 0, 0, 0)
        }
    }

    /// Route a pointer event to the gallery's scrollbar and to the wheel,
    /// reporting whether the gallery moved or the bar's own paint changed.
    ///
    /// The bar carries the gallery's scroll offset, and a wheel tick or a
    /// thumb drag moves it as part of handling the event. Whether anything
    /// changed is therefore decided by what the offset (and the bar's own
    /// hover, press and held part) were *before* the event, never by
    /// comparing a request against a model that has already followed it.
    ///
    /// The model the bar moved *is* the gallery's offset, so its request has
    /// nothing left to adopt and means only that the tiles it scrolled need
    /// reporting — a bar reports its own pixels alone.
    fn scroll_pointer(
        &mut self,
        event: &InputEvent,
        layout: &Layout,
        style: Style<'_>,
        damage: &mut Region,
    ) -> bool {
        let before = (
            self.scroll.model().offset(),
            self.scroll.state(),
            self.scroll.held(),
        );
        let request = match event {
            InputEvent::PointerScrolled { dx, dy } => {
                self.scroll.wheel(*dx, *dy, layout.scrollbar(), damage)
            }
            _ => self.scroll.on_pointer(
                event,
                layout.scrollbar(),
                style.scale(),
                style.theme(),
                damage,
            ),
        };
        if request.is_some() {
            mark_gallery(layout, damage);
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
    ///
    /// A move reports the viewport here rather than in each caller, because a
    /// bar reports its own pixels alone.
    fn scroll_to(&mut self, offset: u64, layout: &Layout, damage: &mut Region) -> bool {
        let model = self.scroll.model();
        if model.offset() == offset {
            return false;
        }
        self.scroll.set_model(model.scroll_to(offset));
        mark_gallery(layout, damage);
        true
    }

    /// Route a pointer event to the gallery's tiles, reporting whether the
    /// hover, the press latch, or the selection changed.
    ///
    /// The gallery owns this hit-test rather than the tiles: an icon tile
    /// paints state and holds no pointer of its own, so the view resolves the
    /// pointer against the very grid it painted.
    fn gallery_pointer(
        &mut self,
        event: &InputEvent,
        layout: &Layout,
        damage: &mut Region,
    ) -> bool {
        let grid = layout.grid(self.visible.len());
        let at = grid
            .index_at(
                self.scroll.model().offset(),
                to_u32(self.pointer.x),
                to_u32(self.pointer.y),
            )
            .and_then(|position| self.visible.get(position).copied());
        match event {
            InputEvent::PointerMoved { .. } => {
                let changed = self.mark_tile(self.hovered, at, layout, damage);
                self.hovered = at;
                changed
            }
            InputEvent::PointerPressed {
                button: PointerButton::Primary,
            } => {
                let mut changed = self.mark_tile(self.armed, at, layout, damage);
                self.armed = at;
                if at.is_some() {
                    changed |= damage::move_mark(
                        Some(self.focus),
                        Some(Focus::Gallery),
                        |region| self.focus_rect(region, layout),
                        damage,
                    );
                    self.focus = Focus::Gallery;
                }
                changed
            }
            InputEvent::PointerReleased {
                button: PointerButton::Primary,
            } => {
                let armed = self.armed.take();
                // The tile that was armed loses its pressed look whether or
                // not the release lands on it, so it is reported either way.
                self.mark_tile(armed, None, layout, damage);
                let selected = match armed {
                    Some(index) if Some(index) == at => self.select(index, layout, damage),
                    _ => false,
                };
                selected || armed.is_some()
            }
            _ => false,
        }
    }

    /// The screen rectangle candidate `index` occupies, or `None` when the
    /// active category is not showing it or it is scrolled out of the
    /// gallery's viewport — either way nothing of it is on screen to repaint.
    fn candidate_rect(&self, index: usize, layout: &Layout) -> Option<Rect> {
        let position = self.position_of(index)?;
        let grid = layout.grid(self.visible.len());
        let cell = grid.cell_rect(self.scroll.model().offset(), position)?;
        let shown = cell.intersection(&layout.tiles());
        (!shown.is_empty()).then_some(shown)
    }

    /// Report the two tiles a gallery mark — the hover, the press latch —
    /// moves between, answering whether it moved at all.
    fn mark_tile(
        &self,
        mark: Option<usize>,
        next: Option<usize>,
        layout: &Layout,
        damage: &mut Region,
    ) -> bool {
        damage::move_mark(
            mark,
            next,
            |index| self.candidate_rect(index, layout),
            damage,
        )
    }

    /// Move the gallery's selection with the arrow keys, revealing the tile
    /// that gains it.
    ///
    /// The arrows walk the gallery's *visible* positions, so they move
    /// through what the active category is showing rather than through
    /// candidates it is not. A selection the active category does not hold
    /// has no position to move from, so forward keys enter at its first tile
    /// and backward keys have nowhere to go.
    fn gallery_key(&mut self, key: Key, layout: &Layout, damage: &mut Region) -> ChooserAction {
        let grid = layout.grid(self.visible.len());
        let per_line = grid.cells_per_line().max(1);
        let last = self.visible.len().saturating_sub(1);
        let here = self.position_of(self.selected);
        let target = match key {
            Key::Named(NamedKey::Left) => here.and_then(|at| at.checked_sub(1)),
            Key::Named(NamedKey::Right) => {
                Some(here.map_or(0, |at| at.saturating_add(1).min(last)))
            }
            Key::Named(NamedKey::Up) => here.and_then(|at| at.checked_sub(per_line)),
            Key::Named(NamedKey::Down) => {
                Some(here.map_or(0, |at| at.saturating_add(per_line).min(last)))
            }
            Key::Named(NamedKey::Home) => Some(0),
            Key::Named(NamedKey::End) => Some(last),
            Key::Named(NamedKey::Enter) => return ChooserAction::Apply,
            _ => return ChooserAction::None,
        };
        let Some(index) = target.and_then(|at| self.visible.get(at).copied()) else {
            return ChooserAction::None;
        };
        let changed = self.select(index, layout, damage);
        let revealed = grid.reveal(
            self.scroll.model().offset(),
            self.position_of(self.selected),
        );
        let moved = self.scroll_to(revealed, layout, damage);
        ChooserAction::changed(changed || moved)
    }

    /// Where the candidate at `index` sits among the gallery's visible
    /// positions, or `None` when the active category is not showing it.
    fn position_of(&self, index: usize) -> Option<usize> {
        self.visible.iter().position(|shown| *shown == index)
    }

    /// Select the candidate at `index`, reporting what that redrew and
    /// whether the selection actually moved.
    ///
    /// A selection moves the marked tile and re-models the preview panel and
    /// its caption, which name the chosen wallpaper — the tiles alone would
    /// leave a stale preview of the wallpaper that was selected before.
    fn select(&mut self, index: usize, layout: &Layout, damage: &mut Region) -> bool {
        if index >= self.candidates.len() || index == self.selected {
            return false;
        }
        let was = self.selected;
        self.selected = index;
        self.mark_tile(Some(was), Some(index), layout, damage);
        damage.add(layout.preview_model());
        damage.add(layout.caption());
        true
    }
}

/// The categories a candidate list is filed under, in rail order.
///
/// Derived from the candidates themselves rather than taken as a second
/// input, so the rail can only ever offer a category the gallery actually
/// holds something for. The shared catalog decides which names may be
/// offered and how many, exactly as it does for the store's own listing.
fn discovered_categories(candidates: &[Candidate]) -> Vec<String> {
    let names: BTreeSet<&str> = candidates
        .iter()
        .filter_map(|candidate| candidate.category.as_deref())
        .collect();
    catalog_categories(names)
}

/// The rail over `categories`, with the "all" entry leading it.
///
/// A store offering no categories has nothing to choose between, so the rail
/// is empty and the layout gives its width to the tiles rather than drawing
/// a column with one entry that does nothing.
fn category_rail(categories: &[String]) -> Tabs {
    let mut entries = Vec::new();
    if !categories.is_empty() {
        entries.push(Tab::new(ALL_CATEGORIES_LABEL));
        entries.extend(categories.iter().map(|name| Tab::new(name.clone())));
    }
    Tabs::new(entries).with_orientation(TabsOrientation::Vertical)
}

/// The category rail entry `index` names, or `None` for the leading "all"
/// entry and for an index the rail does not have.
fn category_at(categories: &[String], index: usize) -> Option<&str> {
    index
        .checked_sub(1)
        .and_then(|slot| categories.get(slot))
        .map(String::as_str)
}

/// The candidates a rail entry shows, as indices into the whole list: those
/// filed under `category`, plus every candidate filed under none, which
/// belongs to every entry.
fn visible_indices(candidates: &[Candidate], category: Option<&str>) -> Vec<usize> {
    candidates
        .iter()
        .enumerate()
        .filter(
            |(_, candidate)| match (candidate.category.as_deref(), category) {
                (None, _) | (Some(_), None) => true,
                (Some(owner), Some(wanted)) => owner == wanted,
            },
        )
        .map(|(index, _)| index)
        .collect()
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

/// Report the gallery's viewport and its bar, which is what a scroll or a
/// change of the shown set redraws: every tile shifts or becomes a different
/// candidate, so no smaller rectangle covers it.
fn mark_gallery(layout: &Layout, damage: &mut Region) {
    damage.add(layout.tiles());
    damage.add(layout.scrollbar());
}

/// Route a pointer event to a button: `(whether its paint changed, whether it
/// was activated)`.
///
/// The button reports only its activation, but a hover or a press changes
/// what it draws, so the state either side of the event is compared: a
/// repaint happens exactly when the user would see a difference.
fn button_pointer(
    button: &mut Button,
    event: &InputEvent,
    bounds: Rect,
    damage: &mut Region,
) -> (bool, bool) {
    let before = button.state();
    let fired = matches!(
        button.on_pointer(event, bounds, damage),
        Some(ButtonAction::Activated)
    );
    (button.state() != before, fired)
}
