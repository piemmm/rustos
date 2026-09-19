//! The [`Shell`]: the whole client content of the settings window.
//!
//! It owns the navigation and nothing else. Every pixel is a shared
//! `lib/controls` control — the vertical `Tabs` strip, the `SearchField`, the
//! `Breadcrumb`, the `ScrollBar`, the `Menu` the shed strip becomes — and the
//! pane on show is drawn by the one statement renderer. The shell holds no
//! capability, performs no I/O, and reads nothing but the registry table and
//! the desktop it was handed.
//!
//! Input updates state and asks for a paint; the paint is produced from that
//! state afterwards, so a burst of pointer motion costs one frame rather than
//! one per sample.

use alloc::string::String;
use alloc::vec::Vec;

use tairix_controls::{
    plate_rect, Breadcrumb, BreadcrumbAction, Crumb, Menu, MenuAction, MenuItem, PlatePlacement,
    PlateSide, ScrollAction, ScrollBar, ScrollModel, ScrollOrientation, ScrollRange, SearchField,
    Tab, Tabs, TabsAction, TabsOrientation, TextAction,
};
use tairix_geometry::{to_i32, Point, Rect, Region, Scale};
use tairix_icon::{IconArtwork, IconKind};
use tairix_input::{InputEvent, Key, Modifiers, NamedKey};
use tairix_raster::{Color, Surface};
use tairix_theme::{CursorSetId, Theme};
use tairix_wallpaper::{CatalogItem, DesktopSettings};

use crate::appearance::{Form, FormOutcome, FormPlace};
use crate::frame::{resolve_frame, Overflow, ShellFrame};
use crate::gallery::{Gallery, GalleryOutcome, PictureWanted};
use crate::registry::{strip_rows, CategoryRow, Location, StripRow, CATEGORIES};
use crate::statement;

/// The trail's leading crumb: the surface itself, and — once the strip is
/// shed — the way back to the category list.
const ROOT_CRUMB: &str = "Settings";

/// How far one line-scroll moves the pane column, in physical pixels.
///
/// A fixed distance rather than a measured text line: the column holds prose
/// at three type roles, so no one line height is the column's, and a reader
/// turning a wheel wants a consistent step.
const LINE_STEP: u64 = 24;

/// How far one page-scroll moves the pane column, in physical pixels.
const PAGE_STEP: u64 = 240;

/// Which region of the shell holds the keyboard cursor.
///
/// A region the frame did not seat is not on the ring, so `Tab` never lands
/// somewhere the reader cannot see.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum Focus {
    /// The search field above the strip.
    Search,
    /// The location trail.
    Trail,
    /// The category and pane strip.
    Strip,
    /// The pane column, whose keyboard is its scrollbar's while the pane
    /// composes no controls of its own.
    Content,
}

/// What one routed event concluded, for a caller that must act outside the
/// window.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ShellOutcome {
    /// Nothing on screen changed.
    Idle,
    /// The shell changed and must be re-presented.
    Changed,
    /// The reader chose a setting: the rendered document the caller posts to
    /// the desktop session, which decides whether to adopt it.
    ///
    /// The shell holds no capability and performs no I/O: it never writes
    /// the desktop's settings, it says what was asked for.
    Apply(String),
}

impl ShellOutcome {
    /// `Changed` when `acted`, else `Idle`.
    const fn of(acted: bool) -> Self {
        if acted {
            Self::Changed
        } else {
            Self::Idle
        }
    }

    /// Whether the shell must be re-presented.
    #[must_use]
    pub const fn changed(&self) -> bool {
        !matches!(self, Self::Idle)
    }

    /// The document this outcome asks the session to adopt, if any.
    #[must_use]
    pub fn document(&self) -> Option<&str> {
        match self {
            Self::Apply(document) => Some(document),
            Self::Idle | Self::Changed => None,
        }
    }
}

/// The settings window's client content.
pub struct Shell {
    /// Where the surface is.
    location: Location,
    /// The strip's rows, in the order they are drawn — the one list the
    /// paint, the hit test and the cursor read.
    rows: Vec<StripRow>,
    strip: Tabs,
    search: SearchField,
    trail: Breadcrumb,
    /// The strip's own scroll: the list of categories is longer than a short
    /// window's column, and a row the reader cannot reach is a category they
    /// cannot open.
    strip_scroll: ScrollBar,
    /// The pane column's scroll.
    scroll: ScrollBar,
    /// The category list the shed strip becomes, while it is open.
    categories: Option<Menu>,
    focus: Focus,
    /// The last pointer position, for routing a press to the region under it.
    pointer: Point,
    /// The desktop settings every composed pane's rows are built from.
    settings: DesktopSettings,
    /// The form the pane on show composes, or `None` for one that states an
    /// absence instead.
    form: Option<Form>,
    /// The shipped pictures the desktop answered, empty until it has.
    catalog: Vec<CatalogItem>,
    /// The cursor sets the desktop offers besides the built-in one, empty
    /// until it has answered.
    cursor_sets: Vec<CursorSetId>,
    /// The picture gallery the pane on show draws beneath its form, for the
    /// one pane that has one.
    gallery: Option<Gallery>,
}

impl Shell {
    /// The shell as the window opens on `settings`: the first category, its
    /// first pane, and the cursor on the strip.
    ///
    /// `settings` is the desktop's own published document, read by the
    /// caller — the shell performs no I/O. An empty registry would leave
    /// nothing to show, so it answers `None` rather than inventing a
    /// location; the registry's own test rules it out.
    #[must_use]
    pub fn new(settings: DesktopSettings) -> Option<Self> {
        let location = Location::opening()?;
        let rows = strip_rows(location.category, "");
        let mut shell = Self {
            location,
            strip: strip_of(&rows, location),
            rows,
            search: SearchField::new().with_placeholder("Search settings"),
            trail: Breadcrumb::new(Vec::new()),
            strip_scroll: ScrollBar::new(ScrollOrientation::Vertical, empty_scroll()),
            scroll: ScrollBar::new(ScrollOrientation::Vertical, empty_scroll()),
            categories: None,
            focus: Focus::Strip,
            pointer: Point::ORIGIN,
            settings,
            form: None,
            catalog: Vec::new(),
            cursor_sets: Vec::new(),
            gallery: None,
        };
        shell.restate_trail();
        shell.restate_form();
        Some(shell)
    }

    /// Adopt the cursor sets the desktop answered.
    ///
    /// Which sets exist is what the desktop's read-only store holds, and
    /// this application may not read it: the session lists it once and
    /// answers, so the pointer row's choice space arrives here. Until it
    /// does the row offers the built-in set alone, plus whatever the
    /// document already names.
    pub fn adopt_cursor_sets(&mut self, sets: Vec<CursorSetId>) {
        self.cursor_sets = sets;
        self.restate_form();
    }

    /// Adopt the shipped picture catalog the desktop answered.
    ///
    /// Requested, never awaited: the Wallpaper pane opens on whatever has
    /// arrived — nothing at all, at first — and rebuilds when it lands.
    pub fn adopt_catalog(&mut self, catalog: Vec<CatalogItem>) {
        self.catalog = catalog;
        if self.gallery.is_some() {
            self.gallery = Some(Gallery::new(&self.catalog, &self.settings));
        }
    }

    /// The next picture the caller should ask the desktop to render for the
    /// gallery, or `None` when there is nothing to ask for.
    #[must_use]
    pub fn next_picture_wanted(
        &self,
        viewport: Rect,
        scale: Scale,
        theme: &Theme,
    ) -> Option<PictureWanted> {
        let frame = self.frame(viewport, scale, theme);
        let band = self.gallery_band(&frame, scale, theme)?;
        self.gallery.as_ref()?.next_wanted(band, scale, theme)
    }

    /// Adopt the pixels the desktop rendered for catalog position `index`,
    /// answering whether anything on screen changed.
    pub fn set_picture(&mut self, index: u16, side: u16, pixels: &[u8]) -> bool {
        self.gallery
            .as_mut()
            .is_some_and(|gallery| gallery.set_picture(index, side, pixels))
    }

    /// Record that the desktop refused catalog position `index`, answering
    /// whether anything on screen changed.
    pub fn mark_picture_refused(&mut self, index: u16) -> bool {
        self.gallery
            .as_mut()
            .is_some_and(|gallery| gallery.mark_refused(index))
    }

    /// Ask for every rendered picture again, because a picture is square
    /// at one side only and the desktop's scale has moved.
    pub fn invalidate_pictures(&mut self) {
        if let Some(gallery) = &mut self.gallery {
            gallery.invalidate_pictures();
        }
    }

    /// Show the pane `name` identifies, answering whether it named one.
    ///
    /// The desktop hands this over when it sends the reader here to change
    /// a particular setting. It confers nothing and names nothing outside
    /// the closed registry, so an unknown name leaves the window on the
    /// pane it already showed.
    pub fn go_to_pane(
        &mut self,
        name: &str,
        viewport: Rect,
        scale: Scale,
        theme: &Theme,
        damage: &mut Region,
    ) -> bool {
        let Some(location) = Location::named(name) else {
            return false;
        };
        if location == self.location {
            return true;
        }
        self.go_to(location, viewport, scale, theme, damage);
        true
    }

    /// Adopt the desktop settings the session now holds.
    ///
    /// What the window does when an apply is answered, and on regaining
    /// focus: every composed row shows what the store actually holds, so a
    /// refused apply reverts rather than leaving a value on screen the next
    /// login would not restore.
    pub fn adopt_settings(&mut self, settings: DesktopSettings) {
        // Against what the *form* shows, not against the last answer: a
        // choice the reader made moved the rows on screen without moving
        // this, so an answer equal to the last one is exactly the refusal
        // that has to put them back.
        let shown = match &self.form {
            Some(form) => form.settings() == &settings,
            None => self.settings == settings,
        };
        self.settings = settings;
        if shown {
            return;
        }
        match &mut self.form {
            Some(form) => form.adopt(&self.settings),
            None => self.restate_form(),
        }
        if let Some(gallery) = &mut self.gallery {
            gallery.adopt(&self.settings);
        }
    }

    /// Build the form the pane on show composes, and the gallery it draws
    /// beneath it, if it has either.
    fn restate_form(&mut self) {
        self.form = self
            .location
            .rows()
            .and_then(|(_, pane)| pane.composition())
            .map(|composition| Form::new(composition, &self.settings, &self.cursor_sets));
        self.gallery = self
            .location
            .rows()
            .is_some_and(|(_, pane)| pane.has_gallery())
            .then(|| Gallery::new(&self.catalog, &self.settings));
    }

    /// The band the gallery is drawn in: what `frame.content` has left
    /// below the form that sits fixed at its top.
    ///
    /// The rows are the part the reader came for, so they keep their place
    /// and the pictures are what scrolls — and a window too short for both
    /// still shows the rows.
    fn gallery_band(&self, frame: &ShellFrame, scale: Scale, theme: &Theme) -> Option<Rect> {
        self.gallery.as_ref()?;
        let header = self
            .form
            .as_ref()
            .map_or(0, |form| form.measured_height(scale, theme));
        let height = frame.content.height.checked_sub(header)?;
        Some(Rect::new(
            frame.content.left(),
            frame.content.top().saturating_add(to_i32(header)),
            frame.content.width,
            height,
        ))
    }

    /// Where the surface is.
    #[must_use]
    pub fn location(&self) -> Location {
        self.location
    }

    /// The strip's rows, in drawn order.
    #[must_use]
    pub fn rows(&self) -> &[StripRow] {
        &self.rows
    }

    /// The category list drawn over the content while the shed strip is open.
    #[must_use]
    pub fn category_list_open(&self) -> bool {
        self.categories.is_some()
    }

    /// The frame this shell is drawn with in `viewport`.
    ///
    /// Cheap enough for the input path: whether the pane scrolls is the
    /// scroll range's own answer, laid in by [`lay_out`](Self::lay_out),
    /// rather than a statement re-measured per event.
    #[must_use]
    pub fn frame(&self, viewport: Rect, scale: Scale, theme: &Theme) -> ShellFrame {
        resolve_frame(
            viewport,
            scale,
            theme,
            Overflow {
                strip: self.strip_scroll.model().range().is_scrollable(),
                pane: self.scroll.model().range().is_scrollable(),
            },
        )
    }

    /// Measure the pane for `viewport` and adopt the scroll range it implies.
    ///
    /// Called whenever what the column holds or how wide it is can have
    /// changed — the window opening, a navigation, a resize, a desktop change
    /// — and never from the input path: measuring a wrapped statement is a
    /// pass over its every word, which a pointer sample did not ask for.
    ///
    /// The column's width decides how the statement wraps and the wrap decides
    /// its height, while a column that needs a scrollbar is the narrower for
    /// it. So it measures without one, and again at the narrowed width when
    /// one turns out to be needed.
    pub fn lay_out(&mut self, viewport: Rect, scale: Scale, theme: &Theme) {
        let bare = resolve_frame(viewport, scale, theme, Overflow::default());
        let overflow = Overflow {
            strip: bare
                .sidebar
                .is_some_and(|rect| self.strip.seated(rect, scale, theme) < self.rows.len()),
            pane: self.gallery.as_ref().map_or_else(
                || self.content_height(bare.content.width, scale, theme) > bare.content.height,
                |gallery| {
                    self.gallery_band(&bare, scale, theme).is_some_and(|band| {
                        gallery.scroll_range(band, scale, theme, 0).is_scrollable()
                    })
                },
            ),
        };
        // A column that needs a bar is the narrower for it, and a narrower
        // pane column wraps its statement into more lines — so the columns
        // the ranges are set from are the ones a bar has already been taken
        // out of.
        let frame = resolve_frame(viewport, scale, theme, overflow);
        // Tile lines for a gallery, groups for a form — each is the unit
        // the thing that scrolls actually moves in — and pixels for a
        // statement, which is drawn at any offset.
        let (extent, seen) = match (&self.gallery, &self.form) {
            (Some(gallery), _) => {
                let band = self
                    .gallery_band(&frame, scale, theme)
                    .unwrap_or(frame.content);
                let range = gallery.scroll_range(band, scale, theme, self.scroll.model().offset());
                (range.content_extent(), range.viewport_extent())
            }
            (None, Some(form)) => (
                groups_as_extent(form.groups_len()),
                groups_as_extent(form.seated(place(frame.content, viewport, scale, theme))),
            ),
            (None, None) => (
                u64::from(self.content_height(frame.content.width, scale, theme)),
                u64::from(frame.content.height),
            ),
        };
        self.scroll
            .set_model(self.scroll.model().resize(extent, seen));
        // The strip's scroll is counted in *rows*, because that is the unit a
        // strip drawing from an entry of its own moves in.
        let seats = frame
            .sidebar
            .map_or(0, |rect| self.strip.seated(rect, scale, theme));
        self.strip_scroll.set_model(
            self.strip_scroll
                .model()
                .resize(rows_as_extent(self.rows.len()), rows_as_extent(seats)),
        );
    }

    /// Scroll the strip so row `index` is one the column shows.
    ///
    /// A strip draws from an entry of its own, so the scroll is in *rows*: a
    /// row above the window becomes the first drawn, and a row below it
    /// becomes the last. That is also why the window is the strip's own
    /// answer ([`Tabs::seated`]) rather than arithmetic here — entries stack
    /// at their own content height.
    fn reveal_row(
        &mut self,
        index: usize,
        frame: &ShellFrame,
        scale: Scale,
        theme: &Theme,
        damage: &mut Region,
    ) {
        let Some(sidebar) = frame.sidebar else {
            return;
        };
        let first = self.strip.first();
        if index < first {
            self.adopt_first(index, sidebar, damage);
            return;
        }
        let seats = self.strip.seated(sidebar, scale, theme).max(1);
        if index >= first.saturating_add(seats) {
            // Walk the first-drawn row forward until the wanted one is the
            // last seated: each step may seat a different number of rows, so
            // the count is re-asked rather than assumed uniform.
            let mut want = first;
            while want < index
                && index >= want.saturating_add(self.seats_from(want, sidebar, scale, theme).max(1))
            {
                want = want.saturating_add(1);
            }
            self.adopt_first(want, sidebar, damage);
        }
    }

    /// How many rows the column seats from row `index`, without disturbing
    /// where the strip is actually scrolled to.
    fn seats_from(&self, index: usize, sidebar: Rect, scale: Scale, theme: &Theme) -> usize {
        let mut probe = self.strip.clone();
        probe.set_first(index);
        probe.seated(sidebar, scale, theme)
    }

    /// Draw the strip from row `first`, and report the column it redraws.
    fn adopt_first(&mut self, first: usize, sidebar: Rect, damage: &mut Region) {
        if self.strip.first() == first {
            return;
        }
        self.strip.set_first(first);
        self.strip_scroll
            .set_model(self.strip_scroll.model().scroll_to(rows_as_extent(first)));
        damage.add(sidebar);
    }

    /// The pane's own height in a column `width` pixels wide.
    fn content_height(&self, width: u32, scale: Scale, theme: &Theme) -> u32 {
        if let Some(form) = &self.form {
            return form.measured_height(scale, theme);
        }
        self.location.rows().map_or(0, |(_, pane)| {
            statement::measured_height(pane, width, scale, theme)
        })
    }

    /// Draw the shell into `surface` filling `viewport`.
    pub fn render(
        &self,
        surface: &mut Surface,
        viewport: Rect,
        scale: Scale,
        theme: &Theme,
        artwork: &mut dyn IconArtwork,
    ) {
        surface.fill_rect(
            0,
            0,
            viewport.width,
            viewport.height,
            Color::from(theme.palette().surface),
        );
        let frame = self.frame(viewport, scale, theme);
        self.trail.render(surface, frame.breadcrumb, scale, theme);
        if let Some(rect) = frame.search {
            self.search.render(surface, rect, scale, theme);
        }
        if let Some(rect) = frame.sidebar {
            self.strip.render(surface, rect, scale, theme, artwork);
        }
        if let Some(rect) = frame.strip_scrollbar {
            self.strip_scroll.render(surface, rect, scale, theme);
        }
        if let Some((_, pane)) = self.location.rows() {
            let column = self.pane_column(&frame);
            surface.with_clip(
                u32::try_from(frame.content.left()).unwrap_or(0),
                u32::try_from(frame.content.top()).unwrap_or(0),
                frame.content.width,
                frame.content.height,
                |clipped| match (&self.gallery, &self.form) {
                    (Some(gallery), form) => {
                        if let Some(form) = form {
                            form.render(clipped, place(frame.content, viewport, scale, theme));
                        }
                        if let Some(band) = self.gallery_band(&frame, scale, theme) {
                            gallery.render(
                                clipped,
                                band,
                                self.scroll.model().offset(),
                                scale,
                                theme,
                            );
                        }
                    }
                    (None, Some(form)) => {
                        form.render(clipped, place(column, viewport, scale, theme));
                    }
                    (None, None) => statement::render(clipped, pane, column, scale, theme),
                },
            );
        }
        if let Some(rect) = frame.scrollbar {
            self.scroll.render(surface, rect, scale, theme);
        }
        // The category list stands over everything it was opened from.
        if let Some(menu) = &self.categories {
            menu.render(
                surface,
                Self::list_rect(menu, &frame, viewport, scale, theme),
                scale,
                theme,
            );
        }
    }

    /// Scroll the form's focused row into view.
    ///
    /// A row the cursor reached but the column does not show is a control
    /// the reader cannot use, which is the same correctness property the
    /// strip's own gutter exists for.
    fn reveal_focused_group(
        &mut self,
        frame: &ShellFrame,
        viewport: Rect,
        scale: Scale,
        theme: &Theme,
        damage: &mut Region,
    ) {
        let column = self.pane_column(frame);
        let spot = place(column, viewport, scale, theme);
        let Some(form) = &self.form else {
            return;
        };
        let want = form.reveal_from(form.focused_group(), spot);
        if want == form.first() {
            return;
        }
        if let Some(form) = &mut self.form {
            form.set_first(want);
        }
        self.scroll
            .set_model(self.scroll.model().scroll_to(groups_as_extent(want)));
        damage.add(frame.content);
    }

    /// The pane's own rectangle, scrolled: the column the form or the
    /// statement is drawn in and hit-tested against.
    ///
    /// The top rides above the frame while the column is scrolled, so a
    /// press lands on the row the reader can actually see. One definition,
    /// read by the paint and the hit test alike.
    fn pane_column(&self, frame: &ShellFrame) -> Rect {
        if self.form.is_some() {
            // A form is *placed* on the surface rather than clipped to it,
            // so it never rides above the column's own top; it scrolls by
            // whole groups instead, exactly as the strip scrolls by rows.
            return frame.content;
        }
        let offset = u32::try_from(self.scroll.model().offset()).unwrap_or(u32::MAX);
        Rect::new(
            frame.content.left(),
            frame.content.top().saturating_sub(to_i32(offset)),
            frame.content.width,
            frame.content.height.saturating_add(offset),
        )
    }

    /// Route one pointer event.
    pub fn on_pointer(
        &mut self,
        event: &InputEvent,
        viewport: Rect,
        scale: Scale,
        theme: &Theme,
        damage: &mut Region,
    ) -> ShellOutcome {
        if let InputEvent::PointerMoved { to } = event {
            self.pointer = *to;
        }
        let frame = self.frame(viewport, scale, theme);

        if let Some(menu) = &mut self.categories {
            let rect = Self::list_rect(menu, &frame, viewport, scale, theme);
            let acted = menu.on_pointer(event, rect, scale, theme, damage);
            return self.list_acted(acted, viewport, scale, theme, damage);
        }

        if let Some(rect) = frame.scrollbar {
            if rect.contains(self.pointer) || self.scroll.is_pressing() {
                let acted = self.scroll.on_pointer(event, rect, scale, theme, damage);
                return ShellOutcome::of(self.scrolled(frame.content, acted, damage));
            }
        }
        if frame.content.contains(self.pointer) || self.form_is_listing() {
            if let InputEvent::PointerScrolled { dx, dy } = event {
                let acted = self.scroll.wheel(*dx, *dy, frame.content, damage);
                return ShellOutcome::of(self.scrolled(frame.content, acted, damage));
            }
            let column = self.pane_column(&frame);
            if let Some(form) = &mut self.form {
                let acted = form.on_pointer(event, place(column, viewport, scale, theme), damage);
                if !matches!(acted, FormOutcome::Idle) {
                    self.focus_on(Focus::Content, viewport, scale, theme, damage);
                    return outcome_of(acted);
                }
            }
            // Under the form, never over it: an open choice list hangs over
            // the gallery and keeps the pointer until it resolves, which
            // the form has already answered for above.
            if let Some(band) = self.gallery_band(&frame, scale, theme) {
                let offset = self.scroll.model().offset();
                if let Some(gallery) = &mut self.gallery {
                    let acted = gallery.on_pointer(event, band, offset, scale, theme, damage);
                    if acted.changed() {
                        self.focus_on(Focus::Content, viewport, scale, theme, damage);
                    }
                    return match acted {
                        GalleryOutcome::Idle => ShellOutcome::Idle,
                        GalleryOutcome::Changed => ShellOutcome::Changed,
                        GalleryOutcome::Chose(settings) => {
                            self.settings = settings;
                            if let Some(form) = &mut self.form {
                                form.adopt(&self.settings);
                                ShellOutcome::Apply(form.applied())
                            } else {
                                ShellOutcome::Changed
                            }
                        }
                    };
                }
            }
        }
        if let Some(rect) = frame.search {
            if rect.contains(self.pointer) {
                let acted = self.search.on_pointer(event, rect, scale, theme, damage);
                self.focus_on(Focus::Search, viewport, scale, theme, damage);
                return self.searched(acted, viewport, scale, theme, damage);
            }
        }
        if let Some(rect) = frame.strip_scrollbar {
            if rect.contains(self.pointer) || self.strip_scroll.is_pressing() {
                let acted = self
                    .strip_scroll
                    .on_pointer(event, rect, scale, theme, damage);
                return ShellOutcome::of(self.strip_scrolled(&frame, acted, damage));
            }
        }
        if let Some(rect) = frame.sidebar {
            if rect.contains(self.pointer) {
                if let InputEvent::PointerScrolled { dx, dy } = event {
                    let acted = self.strip_scroll.wheel(*dx, *dy, rect, damage);
                    return ShellOutcome::of(self.strip_scrolled(&frame, acted, damage));
                }
                let acted = self.strip.on_pointer(event, rect, scale, theme, damage);
                self.focus_on(Focus::Strip, viewport, scale, theme, damage);
                if let Some(TabsAction::Selected { index }) = acted {
                    return ShellOutcome::of(self.choose(index, viewport, scale, theme, damage));
                }
                return ShellOutcome::Idle;
            }
        }
        if frame.breadcrumb.contains(self.pointer) {
            let acted = self
                .trail
                .on_pointer(event, frame.breadcrumb, scale, theme, damage);
            self.focus_on(Focus::Trail, viewport, scale, theme, damage);
            return ShellOutcome::of(self.navigated(acted, viewport, scale, theme, damage));
        }
        ShellOutcome::Idle
    }

    /// Route one key press.
    pub fn on_key(
        &mut self,
        key: Key,
        modifiers: Modifiers,
        viewport: Rect,
        scale: Scale,
        theme: &Theme,
        damage: &mut Region,
    ) -> ShellOutcome {
        let frame = self.frame(viewport, scale, theme);

        if let Some(menu) = &mut self.categories {
            let rect = Self::list_rect(menu, &frame, viewport, scale, theme);
            let acted = menu.on_key(key, rect, scale, theme, damage);
            return self.list_acted(acted, viewport, scale, theme, damage);
        }
        if key == Key::Named(NamedKey::Tab) {
            self.step_focus(!modifiers.shift, viewport, scale, theme, damage);
            return ShellOutcome::Changed;
        }
        match self.focus {
            Focus::Search => {
                let Some(rect) = frame.search else {
                    return ShellOutcome::Idle;
                };
                let acted = self.search.on_key(key, modifiers, rect, damage);
                self.searched(acted, viewport, scale, theme, damage)
            }
            Focus::Trail => {
                let acted = self
                    .trail
                    .on_key(key, frame.breadcrumb, scale, theme, damage);
                ShellOutcome::of(self.navigated(acted, viewport, scale, theme, damage))
            }
            Focus::Strip => {
                let Some(rect) = frame.sidebar else {
                    return ShellOutcome::Idle;
                };
                let acted = self.strip.on_key(key, rect, scale, theme, damage);
                if let Some(cursor) = self.strip.current() {
                    self.reveal_row(cursor, &frame, scale, theme, damage);
                }
                match acted {
                    Some(TabsAction::Selected { index }) => {
                        ShellOutcome::of(self.choose(index, viewport, scale, theme, damage))
                    }
                    // The cursor moved, which the strip reported itself.
                    None => ShellOutcome::of(self.strip.current().is_some()),
                }
            }
            Focus::Content => {
                let column = self.pane_column(&frame);
                if let Some(form) = &mut self.form {
                    let acted = form.on_key(
                        key,
                        modifiers,
                        place(column, viewport, scale, theme),
                        damage,
                    );
                    if !matches!(acted, FormOutcome::Idle) {
                        self.reveal_focused_group(&frame, viewport, scale, theme, damage);
                        return outcome_of(acted);
                    }
                }
                let Some(rect) = frame.scrollbar else {
                    return ShellOutcome::Idle;
                };
                let acted = self.scroll.on_key(key, rect, damage);
                ShellOutcome::of(self.scrolled(frame.content, acted, damage))
            }
        }
    }

    /// Adopt a scroll request, answering whether the `column` moved.
    fn scrolled(&mut self, column: Rect, acted: Option<ScrollAction>, damage: &mut Region) -> bool {
        match acted {
            Some(ScrollAction::ScrollTo { offset }) => {
                self.scroll.set_model(self.scroll.model().scroll_to(offset));
                // A form's scroll is counted in groups, because that is the
                // unit a placed plate can move in — but on a pane whose
                // gallery is what scrolls, the form is the fixed header and
                // the offset is in tile lines, not groups.
                if let (None, Some(form)) = (&self.gallery, &mut self.form) {
                    form.set_first(usize::try_from(offset).unwrap_or(usize::MAX));
                }
                damage.add(column);
                true
            }
            None => false,
        }
    }

    /// Adopt a strip-scroll request, answering whether the strip moved.
    fn strip_scrolled(
        &mut self,
        frame: &ShellFrame,
        acted: Option<ScrollAction>,
        damage: &mut Region,
    ) -> bool {
        match acted {
            Some(ScrollAction::ScrollTo { offset }) => {
                self.strip_scroll
                    .set_model(self.strip_scroll.model().scroll_to(offset));
                self.strip
                    .set_first(usize::try_from(offset).unwrap_or(usize::MAX));
                if let Some(rect) = frame.sidebar {
                    damage.add(rect);
                }
                true
            }
            None => false,
        }
    }

    /// Adopt a search edit: the strip is rebuilt from the query, and the
    /// first row a query reaches is shown, so a search always lands
    /// somewhere.
    fn searched(
        &mut self,
        acted: Option<TextAction>,
        viewport: Rect,
        scale: Scale,
        theme: &Theme,
        damage: &mut Region,
    ) -> ShellOutcome {
        let Some(action) = acted else {
            return ShellOutcome::Idle;
        };
        match action {
            TextAction::Cancelled => self.search.set_text(""),
            TextAction::Edited | TextAction::Submitted => {}
        }
        self.restate_strip(viewport, scale, theme, damage);
        if matches!(action, TextAction::Submitted) {
            if let Some(location) = self.rows.first().and_then(|row| row.location()) {
                self.go_to(location, viewport, scale, theme, damage);
            }
        }
        ShellOutcome::Changed
    }

    /// Adopt a trail activation: a crumb other than the trailing one goes
    /// back, and the leading crumb opens the category list once the strip has
    /// been shed and there is no strip to walk.
    fn navigated(
        &mut self,
        acted: Option<BreadcrumbAction>,
        viewport: Rect,
        scale: Scale,
        theme: &Theme,
        damage: &mut Region,
    ) -> bool {
        let frame = self.frame(viewport, scale, theme);
        let Some(BreadcrumbAction::Activate { index }) = acted else {
            return false;
        };
        if index == 0 {
            if frame.sidebar.is_some() {
                // The strip is on screen, so the category list would be a
                // second way to the same rows.
                self.focus_on(Focus::Strip, viewport, scale, theme, damage);
                return true;
            }
            self.open_category_list(frame, damage);
            return true;
        }
        // The middle crumb is the open category: going to it shows that
        // category's first pane.
        let Some(location) = self
            .location
            .category
            .row()
            .and_then(CategoryRow::first_pane)
            .map(|pane| Location {
                category: self.location.category,
                pane: pane.pane,
            })
        else {
            return false;
        };
        self.go_to(location, viewport, scale, theme, damage);
        true
    }

    /// Adopt the strip row at `index`.
    fn choose(
        &mut self,
        index: usize,
        viewport: Rect,
        scale: Scale,
        theme: &Theme,
        damage: &mut Region,
    ) -> bool {
        let Some(location) = self.rows.get(index).copied().and_then(StripRow::location) else {
            return false;
        };
        self.go_to(location, viewport, scale, theme, damage);
        true
    }

    /// Show `location`: the strip is restated (its disclosure may have moved),
    /// the trail rewritten, the pane re-measured, and its scroll reset to the
    /// top.
    ///
    /// The whole pane band is reported rather than the column alone, because a
    /// pane of a different height may have gained or lost the scrollbar
    /// beside it.
    fn go_to(
        &mut self,
        location: Location,
        viewport: Rect,
        scale: Scale,
        theme: &Theme,
        damage: &mut Region,
    ) {
        let frame = self.frame(viewport, scale, theme);
        self.location = location;
        self.scroll.set_model(self.scroll.model().scroll_to(0));
        self.restate_form();
        self.restate_trail();
        self.restate_strip(viewport, scale, theme, damage);
        self.lay_out(viewport, scale, theme);
        damage.add(frame.breadcrumb);
        damage.add(pane_band(&frame, viewport));
    }

    /// Rebuild the strip from the registry for the open category and the
    /// current query, keeping the cursor on the row that is on show.
    fn restate_strip(&mut self, viewport: Rect, scale: Scale, theme: &Theme, damage: &mut Region) {
        self.rows = strip_rows(self.location.category, self.search.text());
        self.strip.restate(strip_of(&self.rows, self.location));
        // The row count changed, so what the strip wants and what it can show
        // did too.
        self.lay_out(viewport, scale, theme);
        let frame = self.frame(viewport, scale, theme);
        if let (Focus::Strip, Some(rect)) = (self.focus, frame.sidebar) {
            let cursor = self.selected_row();
            self.strip.set_current(cursor, rect, scale, theme, damage);
        }
        if let Some(index) = self.selected_row() {
            self.reveal_row(index, &frame, scale, theme, damage);
        }
        if let Some(rect) = frame.sidebar {
            damage.add(rect);
        }
        if let Some(rect) = frame.strip_scrollbar {
            damage.add(rect);
        }
    }

    /// Rewrite the location trail for where the surface is.
    fn restate_trail(&mut self) {
        let mut crumbs = Vec::with_capacity(3);
        crumbs.push(Crumb::new(ROOT_CRUMB));
        if let Some((category, pane)) = self.location.rows() {
            crumbs.push(Crumb::new(category.label));
            // A category holding one pane shares its name, and a trail that
            // said it twice would read as two places.
            if category.discloses() {
                crumbs.push(Crumb::new(pane.title));
            }
        }
        self.trail = Breadcrumb::new(crumbs);
    }

    /// Which strip row is the one on show, if the strip is drawing it.
    fn selected_row(&self) -> Option<usize> {
        row_on_show(&self.rows, self.location)
    }

    /// Open the category list the shed strip becomes.
    fn open_category_list(&mut self, frame: ShellFrame, damage: &mut Region) {
        let mut menu = Menu::new(
            CATEGORIES
                .iter()
                .map(|row| MenuItem::new(row.label))
                .collect(),
        );
        menu.adopt_current(
            CATEGORIES
                .iter()
                .position(|row| row.category == self.location.category),
        );
        self.categories = Some(menu);
        damage.add(frame.content);
    }

    /// Adopt what the open category list reported.
    fn list_acted(
        &mut self,
        acted: Option<MenuAction>,
        viewport: Rect,
        scale: Scale,
        theme: &Theme,
        damage: &mut Region,
    ) -> ShellOutcome {
        // The list's own rectangle, resolved while it is still open, so
        // closing it reports the pixels it actually covered.
        let frame = self.frame(viewport, scale, theme);
        let rect = self.categories.as_ref().map_or(Rect::EMPTY, |menu| {
            Self::list_rect(menu, &frame, viewport, scale, theme)
        });
        match acted {
            Some(MenuAction::Activated { index }) => {
                self.categories = None;
                damage.add(rect);
                let chosen = CATEGORIES
                    .get(index)
                    .and_then(|row| StripRow::Category(row.category).location());
                match chosen {
                    Some(location) => {
                        self.go_to(location, viewport, scale, theme, damage);
                        ShellOutcome::Changed
                    }
                    None => ShellOutcome::Changed,
                }
            }
            Some(MenuAction::Dismissed) => {
                self.categories = None;
                damage.add(rect);
                ShellOutcome::Changed
            }
            Some(MenuAction::OpenSubmenu { .. }) | None => ShellOutcome::Idle,
        }
    }

    /// Where the category list is drawn: hanging off the trail's leading
    /// crumb, placed by the one shared plate rule so it never leaves the
    /// client.
    fn list_rect(
        menu: &Menu,
        frame: &ShellFrame,
        viewport: Rect,
        scale: Scale,
        theme: &Theme,
    ) -> Rect {
        let anchor = Rect::new(
            frame.breadcrumb.left(),
            frame.breadcrumb.top(),
            frame.breadcrumb.height,
            frame.breadcrumb.height,
        );
        plate_rect(
            menu.preferred_width(scale, theme),
            menu.preferred_height(scale, theme),
            PlatePlacement {
                anchor,
                side: PlateSide::Below,
                gap: 0,
            },
            viewport,
        )
    }

    /// The focus ring for `frame`: every region the frame actually seated, in
    /// Tab order.
    fn ring(&self, frame: &ShellFrame) -> Vec<Focus> {
        let mut ring = Vec::with_capacity(4);
        if frame.search.is_some() {
            ring.push(Focus::Search);
        }
        ring.push(Focus::Trail);
        if frame.sidebar.is_some() {
            ring.push(Focus::Strip);
        }
        // A pane composing controls is reachable whether or not it is long
        // enough to scroll; one that only scrolls is reachable only when
        // there is something to scroll.
        if self.form.is_some() || frame.scrollbar.is_some() {
            ring.push(Focus::Content);
        }
        ring
    }

    /// Whether the form has a choice list open, which is modal: the list
    /// keeps the pointer even when it hangs outside the pane's own column.
    fn form_is_listing(&self) -> bool {
        self.form.as_ref().is_some_and(Form::is_listing)
    }

    /// Move the cursor one step round the ring.
    fn step_focus(
        &mut self,
        forward: bool,
        viewport: Rect,
        scale: Scale,
        theme: &Theme,
        damage: &mut Region,
    ) {
        let ring = self.ring(&self.frame(viewport, scale, theme));
        if ring.is_empty() {
            return;
        }
        let at = ring.iter().position(|f| *f == self.focus).unwrap_or(0);
        let next = if forward {
            (at + 1) % ring.len()
        } else {
            (at + ring.len() - 1) % ring.len()
        };
        let Some(&focus) = ring.get(next) else {
            return;
        };
        self.focus_on(focus, viewport, scale, theme, damage);
    }

    /// Put the cursor on `focus`, so exactly one region reads as focused.
    fn focus_on(
        &mut self,
        focus: Focus,
        viewport: Rect,
        scale: Scale,
        theme: &Theme,
        damage: &mut Region,
    ) {
        let frame = self.frame(viewport, scale, theme);
        if !self.ring(&frame).contains(&focus) || self.focus == focus {
            return;
        }
        self.focus = focus;
        self.search.set_focused(focus == Focus::Search);
        self.trail.adopt_focus((focus == Focus::Trail).then_some(0));
        self.scroll
            .set_focused(focus == Focus::Content && self.form.is_none());
        if let Some(form) = &mut self.form {
            form.set_focused(focus == Focus::Content);
            damage.add(frame.content);
        }
        if let Some(rect) = frame.sidebar {
            let cursor = (focus == Focus::Strip)
                .then(|| self.selected_row())
                .flatten();
            self.strip.set_current(cursor, rect, scale, theme, damage);
        }
        if let Some(rect) = frame.search {
            damage.add(rect);
        }
        damage.add(frame.breadcrumb);
        if let Some(rect) = frame.scrollbar {
            damage.add(rect);
        }
    }

    /// The strip rectangle of row `index` within a sidebar at `bounds`, so a
    /// test aims at the row the strip actually drew rather than at arithmetic
    /// of its own.
    #[cfg(test)]
    pub(crate) fn strip_row_rect(
        &self,
        index: usize,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
    ) -> Option<Rect> {
        self.strip.tab_area(index, bounds, scale, theme)
    }

    /// The location trail's labels, in order.
    #[cfg(test)]
    pub(crate) fn trail_labels(&self) -> Vec<&str> {
        self.trail.crumbs().iter().map(Crumb::label).collect()
    }

    /// The pane's own height in a column `width` pixels wide.
    #[cfg(test)]
    pub(crate) fn pane_height(&self, width: u32, scale: Scale, theme: &Theme) -> u32 {
        self.content_height(width, scale, theme)
    }

    /// How far the pane column is scrolled, in physical pixels.
    #[cfg(test)]
    pub(crate) fn scroll_offset(&self) -> u64 {
        self.scroll.model().offset()
    }

    /// Which row the strip is drawing from.
    #[cfg(test)]
    pub(crate) fn strip_first_for_test(&self) -> usize {
        self.strip.first()
    }

    /// Where the strip's keyboard cursor is.
    #[cfg(test)]
    pub(crate) fn strip_cursor_for_test(&self) -> Option<usize> {
        self.strip.current()
    }

    /// Scroll row `index` into view.
    #[cfg(test)]
    pub(crate) fn reveal_for_test(
        &mut self,
        index: usize,
        viewport: Rect,
        scale: Scale,
        theme: &Theme,
    ) {
        let frame = self.frame(viewport, scale, theme);
        self.reveal_row(
            index,
            &frame,
            scale,
            theme,
            &mut tairix_controls::damage::sink(),
        );
    }

    /// The strip, for a test that asks it where it seated a row.
    #[cfg(test)]
    pub(crate) fn strip_for_test(&self) -> &Tabs {
        &self.strip
    }

    /// Show `location`, as choosing its strip row would.
    #[cfg(test)]
    pub(crate) fn go_to_for_test(
        &mut self,
        location: Location,
        viewport: Rect,
        scale: Scale,
        theme: &Theme,
        damage: &mut Region,
    ) {
        self.go_to(location, viewport, scale, theme, damage);
    }

    /// The form the pane on show composes, for a test that asks what it
    /// composed.
    #[cfg(test)]
    pub(crate) fn form_for_test(&self) -> Option<&Form> {
        self.form.as_ref()
    }

    /// Which group the form is drawing from.
    #[cfg(test)]
    pub(crate) fn form_first_for_test(&self) -> Option<usize> {
        self.form.as_ref().map(Form::first)
    }

    /// Which group and row the form's keyboard cursor is on.
    #[cfg(test)]
    pub(crate) fn form_group_cursor_for_test(&self) -> Option<(usize, usize)> {
        self.form.as_ref().and_then(Form::cursor)
    }

    /// The settings the shell is showing.
    #[cfg(test)]
    pub(crate) fn settings_for_test(&self) -> &DesktopSettings {
        &self.settings
    }

    /// Put the keyboard cursor on the pane column.
    #[cfg(test)]
    pub(crate) fn focus_content_for_test(&mut self, viewport: Rect, scale: Scale, theme: &Theme) {
        self.focus_on(
            Focus::Content,
            viewport,
            scale,
            theme,
            &mut tairix_controls::damage::sink(),
        );
    }
}

/// Where a composed pane's form is drawn, gathered once for the call that
/// needs it.
fn place(bounds: Rect, viewport: Rect, scale: Scale, theme: &Theme) -> FormPlace<'_> {
    FormPlace {
        bounds,
        viewport,
        scale,
        theme,
    }
}

/// The shell outcome a form's answer implies.
fn outcome_of(acted: FormOutcome) -> ShellOutcome {
    match acted {
        FormOutcome::Idle => ShellOutcome::Idle,
        FormOutcome::Changed => ShellOutcome::Changed,
        FormOutcome::Apply(document) => ShellOutcome::Apply(document),
    }
}

/// A group count as a form scroll's extent, for the same reason a row count
/// is the strip's: a list of more entries than a `u64` can count is not one
/// this surface could draw.
fn groups_as_extent(groups: usize) -> u64 {
    u64::try_from(groups).unwrap_or(u64::MAX)
}

/// A row count as the strip scroll's extent.
///
/// The strip's scroll is counted in rows, and a list of more rows than a
/// `u64` can count is not one this surface could draw.
fn rows_as_extent(rows: usize) -> u64 {
    u64::try_from(rows).unwrap_or(u64::MAX)
}

/// The band a pane occupies: its column and whatever the scrollbar takes
/// beside it, so a change that moves the bar reports the strip it vacated.
fn pane_band(frame: &ShellFrame, viewport: Rect) -> Rect {
    let width = u32::try_from(viewport.right().saturating_sub(frame.content.left())).unwrap_or(0);
    Rect::new(
        frame.content.left(),
        frame.content.top(),
        width,
        frame.content.height,
    )
}

/// The scroll model an unmeasured column starts at: nothing to scroll, and
/// the step distances a settings pane scrolls by.
///
/// A line is one body line and a page a whole viewport, which the shell
/// re-derives from the column every time it is resized; at construction it has
/// no column yet, so the model starts empty and the first layout sizes it.
fn empty_scroll() -> ScrollModel {
    ScrollModel::new(ScrollRange::new(0, 0, 0), LINE_STEP, PAGE_STEP)
}

/// The strip a row list implies: a glyph and a disclosure chevron on each
/// category row, an indent on each disclosed pane row, and the row on show
/// selected.
fn strip_of(rows: &[StripRow], location: Location) -> Tabs {
    let mut strip = Tabs::new(
        rows.iter()
            .map(|row| match row {
                StripRow::Category(category) => {
                    let (label, icon, discloses) = category
                        .row()
                        .map_or(("", IconKind::Generic, false), |row| {
                            (row.label, row.icon, row.discloses())
                        });
                    let tab = Tab::new(label).with_icon(icon);
                    if discloses {
                        tab.with_disclosure(*category == location.category)
                    } else {
                        tab
                    }
                }
                StripRow::Pane(_, pane) => {
                    Tab::new(pane.locate().map_or("", |(_, row)| row.title)).nested()
                }
            })
            .collect(),
    )
    .with_orientation(TabsOrientation::Vertical);
    if let Some(index) = row_on_show(rows, location) {
        strip.adopt_selected(index);
    }
    strip
}

/// Which of `rows` is the pane on show: its own row where the category
/// discloses its panes, else the category row that stands for it.
///
/// One definition, read by the strip's selection and by the keyboard cursor,
/// so the row the reader sees selected is the row the cursor sits on.
fn row_on_show(rows: &[StripRow], location: Location) -> Option<usize> {
    rows.iter().position(|row| match row {
        StripRow::Pane(_, pane) => *pane == location.pane,
        StripRow::Category(category) => {
            *category == location.category && category.row().is_some_and(|row| !row.discloses())
        }
    })
}
