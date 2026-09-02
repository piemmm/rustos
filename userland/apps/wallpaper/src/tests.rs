//! Host unit tests for the wallpaper chooser engine.
//!
//! The pointer tests are written the way a user drives the app: a move to a
//! place the layout actually puts something, then a press, then a release.
//! No test hard-codes a coordinate — every one asks the layout where the
//! thing it is about to click is — so a change to the geometry moves the
//! tests with it instead of quietly making them click empty space.

use alloc::string::String;
use alloc::vec;

use tairix_controls::collection::IconTile;
use tairix_controls::damage;
use tairix_controls::scrollbar::ScrollPart;
use tairix_controls::tabs::Tab;
use tairix_geometry::{Point, Rect, Region};
use tairix_input::{InputEvent, Key, Modifiers, NamedKey, PointerButton};
use tairix_theme::{TextRole, ThemeRegistry};
use tairix_wallpaper::{CatalogEntry, PinboardSettings, WallpaperPath};

use super::*;

/// Settings with no wallpaper at all, everything else at the shared
/// default.
fn settings_without_a_wallpaper() -> PinboardSettings {
    PinboardSettings {
        wallpaper: WallpaperChoice::None,
        ..PinboardSettings::default()
    }
}

/// The screen every test's preview panel models: an ordinary 1920x1080
/// landscape desktop, matching the shape `Layout` used to hard-code before
/// it was taught to read the real screen, so the existing region-overlap
/// assertions below still hold unchanged.
const TEST_SCREEN: (u32, u32) = (1920, 1080);

/// The style every test paints and hit-tests through: the built-in dark
/// theme at the unscaled desktop density, in the interface face, modelling
/// [`TEST_SCREEN`].
fn style_for(theme: &Theme) -> Style<'_> {
    style_with_screen(theme, TEST_SCREEN)
}

/// [`style_for`], modelling `screen` instead of [`TEST_SCREEN`] — for the
/// tests that care what the preview panel's true-scale model looks like on
/// a particular screen.
fn style_with_screen(theme: &Theme, screen: (u32, u32)) -> Style<'_> {
    Style::new(
        theme,
        Scale::ONE,
        BitmapFont::for_role(theme.fonts(), TextRole::Body, Scale::ONE),
        screen,
    )
}

/// A settings document naming a wallpaper, otherwise at the shared crate
/// default.
fn settings_selecting(path: &str) -> PinboardSettings {
    PinboardSettings {
        wallpaper: WallpaperChoice::Image(WallpaperPath::new(path).expect("a valid test path")),
        ..PinboardSettings::default()
    }
}

/// The store category every single-category test files its images under.
const TEST_CATEGORY: &str = "Space";

/// The rail width the geometry tests hand [`Layout::compute`]: wide enough
/// that a rail is drawn at every window size they try, so the assertions see
/// a real column rather than the dropped one a narrow window would give.
const TEST_RAIL: u32 = 64;

/// `count` catalog entries under one store category, in listing order.
fn catalog_in(category: &str, count: usize) -> Vec<Candidate> {
    let entries: Vec<CatalogEntry> = (0..count)
        .map(|index| CatalogEntry {
            name: alloc::format!("image-{index:02}.png"),
            bytes: 10,
        })
        .collect();
    candidates_from_catalog(category, &entries)
}

/// `count` catalog entries under [`TEST_CATEGORY`], in listing order.
fn catalog(count: usize) -> Vec<Candidate> {
    catalog_in(TEST_CATEGORY, count)
}

/// The store's whole listing over several categories, in `(category, count)`
/// order.
fn catalog_over(categories: &[(&str, usize)]) -> Vec<Candidate> {
    categories
        .iter()
        .flat_map(|(category, count)| catalog_in(category, *count))
        .collect()
}

/// A chooser over three images in one category, opened on the first of them.
fn sample_chooser() -> Chooser {
    Chooser::new(
        catalog(3),
        &settings_selecting("/System/Graphics/Wallpapers/Space/image-00.png"),
    )
}

/// The middle of `rect`.
fn centre(rect: Rect) -> Point {
    Point::new(
        rect.left() + to_i32(rect.width / 2),
        rect.top() + to_i32(rect.height / 2),
    )
}

/// Move the pointer to `at`.
fn move_to(chooser: &mut Chooser, at: Point, style: Style<'_>) -> ChooserAction {
    chooser.on_pointer(
        &InputEvent::PointerMoved { to: at },
        style,
        &mut damage::sink(),
    )
}

/// Press the primary button where the pointer already is.
fn press(chooser: &mut Chooser, style: Style<'_>) -> ChooserAction {
    chooser.on_pointer(
        &InputEvent::PointerPressed {
            button: PointerButton::Primary,
        },
        style,
        &mut damage::sink(),
    )
}

/// Release the primary button where the pointer already is, reporting the
/// pixels it repainted into `damage` — for the tests that check what a click
/// asks the window to present, rather than only what it did.
fn release_reporting(
    chooser: &mut Chooser,
    style: Style<'_>,
    damage: &mut Region,
) -> ChooserAction {
    chooser.on_pointer(
        &InputEvent::PointerReleased {
            button: PointerButton::Primary,
        },
        style,
        damage,
    )
}

/// Release the primary button where the pointer already is.
fn release(chooser: &mut Chooser, style: Style<'_>) -> ChooserAction {
    release_reporting(chooser, style, &mut damage::sink())
}

/// A whole primary click at `at`: the move that positions the pointer, the
/// press, and the release, reporting what the release asked for.
fn click(chooser: &mut Chooser, at: Point, style: Style<'_>) -> ChooserAction {
    let _ = move_to(chooser, at, style);
    let _ = press(chooser, style);
    release(chooser, style)
}

/// Press a named key with no modifiers.
fn key(chooser: &mut Chooser, named: NamedKey, style: Style<'_>) -> ChooserAction {
    chooser.on_key(
        Key::Named(named),
        Modifiers::default(),
        style,
        &mut damage::sink(),
    )
}

/// Draw the whole window into a fresh surface at the size every test's
/// chooser lays out for, so a test can compare two pictures.
fn shot(chooser: &mut Chooser, style: Style<'_>) -> Surface {
    shot_at(chooser, style, WIN_WIDTH, WIN_HEIGHT)
}

/// [`shot`] onto a `width` × `height` window, for the tests that resize.
fn shot_at(chooser: &mut Chooser, style: Style<'_>, width: u32, height: u32) -> Surface {
    let mut surface = Surface::new(width, height).expect("a window-sized surface");
    chooser.render_into(&mut surface, style);
    surface
}

/// The rectangle of the gallery tile showing the candidate at `index`, which
/// the active category must be showing.
///
/// The grid works in the gallery's own visible positions, so the candidate is
/// resolved to its position first — exactly as the painter and the hit-test
/// do.
fn tile_rect(chooser: &Chooser, index: usize, style: Style<'_>) -> Rect {
    let position = chooser
        .visible()
        .iter()
        .position(|shown| *shown == index)
        .expect("the active category shows the candidate");
    chooser
        .layout(style)
        .grid(chooser.visible().len())
        .cell_rect(chooser.scroll_offset(), position)
        .expect("the tile is visible")
}

/// The rectangle of category rail entry `index`.
fn rail_rect(chooser: &Chooser, index: usize, style: Style<'_>) -> Rect {
    let bounds = chooser.layout(style).categories();
    assert!(!bounds.is_empty(), "the window draws a category rail");
    let entries = u32::try_from(chooser.rail().len()).unwrap_or(1).max(1);
    let height = bounds.height / entries;
    let down = height.saturating_mul(u32::try_from(index).unwrap_or(0));
    Rect::new(
        bounds.left(),
        bounds.top() + to_i32(down),
        bounds.width,
        height,
    )
}

/// A point inside row `row` of the open drop-down list of `group`.
///
/// The list's rows divide its height evenly, so the middle of a row is half
/// a row-height past its top edge.
fn popup_row(chooser: &Chooser, group: OptionGroup, row: usize, style: Style<'_>) -> Point {
    let choices = chooser.field(group).choices().len().max(1);
    let popup = chooser.popup_rect(group, &chooser.layout(style), style);
    let height = popup.height / u32::try_from(choices).unwrap_or(1).max(1);
    let down = height
        .saturating_mul(u32::try_from(row).unwrap_or(0))
        .saturating_add(height / 2);
    Point::new(
        popup.left() + to_i32(popup.width / 2),
        popup.top() + to_i32(down),
    )
}

/// A point on the gallery scrollbar's thumb, wherever the bar has drawn it.
fn thumb_point(chooser: &Chooser, style: Style<'_>) -> Point {
    let gutter = chooser.layout(style).scrollbar();
    let x = gutter.left() + to_i32(gutter.width / 2);
    let bar = chooser.scrollbar();
    (gutter.top()..gutter.bottom())
        .map(|y| Point::new(x, y))
        .find(|at| bar.part_at(gutter, *at, style.scale(), style.theme()) == ScrollPart::Thumb)
        .expect("a scrollable gallery draws a thumb")
}

/// Assert that every pixel `feed` repaints on a `width` × `height` window lies
/// inside what it reported, and that it repainted at all. `what` names the
/// input in the failure.
///
/// The window presents only what a round reports, into a surface it retains,
/// so a pixel changed outside the report is a stale pixel left on screen.
fn every_changed_pixel_is_reported(
    chooser: &mut Chooser,
    style: Style<'_>,
    width: u32,
    height: u32,
    what: &str,
    feed: impl FnOnce(&mut Chooser, &mut Region),
) {
    let (changed, reported) = round_at(chooser, style, width, height, feed);
    assert!(
        !changed.is_empty(),
        "{what} repainted nothing, so it proves nothing"
    );
    for point in changed {
        assert!(
            reported.contains(point),
            "{what} changed {point:?}, which it did not report"
        );
    }
}

#[test]
fn the_no_wallpaper_entry_is_always_first() {
    let chooser = sample_chooser();
    let first = &chooser.candidates()[0];
    assert_eq!(first.choice, WallpaperChoice::None);
    assert_eq!(first.label, NONE_LABEL);
    assert_eq!(first.thumbnail, Thumbnail::Backdrop);
}

#[test]
fn the_chooser_opens_on_the_settings_current_wallpaper() {
    let chooser = Chooser::new(
        catalog(3),
        &settings_selecting("/System/Graphics/Wallpapers/Space/image-01.png"),
    );
    assert_eq!(chooser.selected(), 2);
}

#[test]
fn no_wallpaper_settings_open_the_chooser_on_the_none_entry() {
    let chooser = Chooser::new(catalog(3), &settings_without_a_wallpaper());
    assert_eq!(chooser.selected(), 0);
    assert_eq!(chooser.to_settings().wallpaper, WallpaperChoice::None);
}

#[test]
fn a_current_wallpaper_outside_the_catalog_is_appended_and_selected() {
    let chooser = Chooser::new(
        catalog(2),
        &settings_selecting("/Users/ada/Pictures/holiday.png"),
    );
    let last = chooser.candidates().len() - 1;
    assert_eq!(chooser.selected(), last);
    assert_eq!(chooser.candidates()[last].label, "holiday.png");
}

#[test]
fn a_refused_thumbnail_is_remembered_and_never_retried() {
    let registry = ThemeRegistry::with_builtins();
    let style = style_for(registry.active());
    let mut chooser = sample_chooser();

    let first = chooser.next_thumbnail(style).expect("a pending thumbnail");
    chooser.mark_thumbnail_refused(first.index, style, &mut damage::sink());
    let next = chooser.next_thumbnail(style).expect("another pending one");
    assert_ne!(next.index, first.index);
    assert_eq!(
        chooser.candidates()[first.index].thumbnail,
        Thumbnail::Refused
    );
}

#[test]
fn every_thumbnail_is_asked_for_at_the_side_the_tile_will_draw_it() {
    let registry = ThemeRegistry::with_builtins();
    let style = style_for(registry.active());
    let chooser = sample_chooser();

    let (width, height) = chooser.layout(style).tile_size();
    let expected =
        IconTile::icon_side(Rect::new(0, 0, width, height), style.scale(), style.theme());
    let request = chooser.next_thumbnail(style).expect("a pending thumbnail");
    assert!(expected > 0);
    assert_eq!(request.side, expected);
}

#[test]
fn next_thumbnail_is_none_once_every_candidate_is_resolved() {
    let registry = ThemeRegistry::with_builtins();
    let style = style_for(registry.active());
    let mut chooser = sample_chooser();

    while let Some(request) = chooser.next_thumbnail(style) {
        chooser.mark_thumbnail_refused(request.index, style, &mut damage::sink());
    }
    assert!(chooser.next_thumbnail(style).is_none());
}

#[test]
fn a_ready_thumbnail_replaces_the_pending_state() {
    let registry = ThemeRegistry::with_builtins();
    let style = style_for(registry.active());
    let mut chooser = sample_chooser();

    let request = chooser.next_thumbnail(style).expect("a pending thumbnail");
    let pixels = Surface::new(request.side, request.side).expect("a test surface");
    chooser.set_thumbnail(request.index, pixels, style, &mut damage::sink());
    assert!(matches!(
        chooser.candidates()[request.index].thumbnail,
        Thumbnail::Ready(_)
    ));
}

#[test]
fn a_thumbnail_rendered_for_another_side_is_asked_for_again() {
    // A tile painter centres the artwork it is handed rather than stretching
    // it, so pixels rendered for a different square side would draw the
    // picture at the wrong size. Holding a thumbnail whose side no longer
    // matches the tile is therefore stale, not resolved.
    let registry = ThemeRegistry::with_builtins();
    let style = style_for(registry.active());
    let mut chooser = sample_chooser();

    let request = chooser.next_thumbnail(style).expect("a pending thumbnail");
    let index = request.index;
    let wrong_side = Surface::new(request.side / 2, request.side / 2).expect("a test surface");
    chooser.set_thumbnail(index, wrong_side, style, &mut damage::sink());

    let again = chooser
        .next_thumbnail(style)
        .expect("the wrong-sided thumbnail is asked for again");
    assert_eq!(again.index, index);
    assert_eq!(again.side, request.side);

    let fresh = Surface::new(request.side, request.side).expect("a test surface");
    chooser.set_thumbnail(index, fresh, style, &mut damage::sink());
    assert!(
        chooser
            .next_thumbnail(style)
            .is_none_or(|next| next.index != index),
        "a thumbnail at the wanted side is resolved"
    );
}

#[test]
fn a_refused_thumbnail_stays_refused_at_every_side() {
    // A file the worker could not decode will not decode smaller, so a
    // refusal costs one attempt however the tile is later sized.
    let registry = ThemeRegistry::with_builtins();
    let mut chooser = sample_chooser();
    let theme = registry.active();

    let mut resolved = 0;
    while let Some(request) = chooser.next_thumbnail(style_for(theme)) {
        chooser.mark_thumbnail_refused(request.index, style_for(theme), &mut damage::sink());
        resolved += 1;
        assert!(resolved <= chooser.candidates().len(), "no re-asking");
    }
    assert!(chooser
        .next_thumbnail(style_with_screen(theme, (800, 600)))
        .is_none());
}

/// The on-screen candidates, by index, for the gallery as it currently sits.
///
/// Read from the grid the painter itself lays the tiles out with, so a test
/// asserting "visible first" cannot drift from what is actually drawn.
fn on_screen(chooser: &Chooser, style: Style<'_>) -> Vec<usize> {
    let layout = chooser.layout(style);
    let range = layout
        .grid(chooser.visible().len())
        .visible_range(chooser.scroll_offset());
    chooser.visible()[range].to_vec()
}

/// Those of `indices` that need a rendered thumbnail. The "no wallpaper" entry
/// paints from the backdrop colour and is never rendered, so counting it would
/// make a test ask for one thumbnail more than the set holds.
fn pictures_among(chooser: &Chooser, indices: &[usize]) -> Vec<usize> {
    indices
        .iter()
        .copied()
        .filter(|index| {
            matches!(
                chooser.candidates()[*index].choice,
                WallpaperChoice::Image(_)
            )
        })
        .collect()
}

/// The chooser froze until it had read and decoded **every** master, because
/// the render order was the catalog's own and ignored both the scroll offset
/// and the category filter. With 4K masters that is tens of megabytes before
/// the first visible tile has a picture.
///
/// The scheduler must serve what is on screen first, and answer `None` for the
/// visible set once it is complete rather than reaching past it.
#[test]
fn a_scrolled_gallery_renders_what_is_on_screen_before_anything_else() {
    let registry = ThemeRegistry::with_builtins();
    let style = style_for(registry.active());
    let mut chooser = Chooser::new(catalog(24), &settings_without_a_wallpaper());
    chooser.relayout(MIN_WIN_WIDTH, MIN_WIN_HEIGHT);

    // Scroll well past the first row, so index order and screen order differ.
    for _ in 0..3 {
        let _ = chooser.on_pointer(
            &InputEvent::PointerScrolled { dx: 0, dy: 3 },
            style,
            &mut damage::sink(),
        );
    }
    assert!(chooser.scroll_offset() > 0, "the gallery scrolled");
    let shown = on_screen(&chooser, style);
    let wanted = pictures_among(&chooser, &shown);
    assert!(!wanted.is_empty(), "the window shows tiles to render");
    assert!(
        wanted.iter().any(|index| *index > wanted.len()),
        "the visible set must not be the catalog's leading run, or the \
         regression would pass on index order alone"
    );

    // Every request until the visible set is complete names a visible tile.
    let mut served = Vec::new();
    while served.len() < wanted.len() {
        let request = chooser
            .next_thumbnail(style)
            .expect("a visible tile still needs a thumbnail");
        assert!(
            shown.contains(&request.index),
            "an off-screen candidate was rendered before the visible ones"
        );
        let pixels = Surface::new(request.side, request.side).expect("a test surface");
        chooser.set_thumbnail(request.index, pixels, style, &mut damage::sink());
        served.push(request.index);
    }
    served.sort_unstable();
    let mut expected = wanted.clone();
    expected.sort_unstable();
    assert_eq!(served, expected, "the visible set is served exactly once");

    // Only now is the rest of the gallery filled in behind it.
    let behind = chooser
        .next_thumbnail(style)
        .expect("the off-screen candidates are still wanted");
    assert!(!shown.contains(&behind.index));
}

/// A category filter narrows the scheduler exactly as it narrows the painter:
/// a candidate the rail is hiding is never rendered ahead of one it shows.
#[test]
fn a_filtered_gallery_never_renders_a_hidden_candidate_first() {
    let registry = ThemeRegistry::with_builtins();
    let style = style_for(registry.active());
    let mut chooser = categorised_chooser();

    // Rail entry 2 is `Nature`, whose images sit *after* `Space`'s in the
    // catalog — so a scheduler walking index order would reach a hidden
    // candidate first, and this assertion means something.
    let nature = rail_rect(&chooser, 2, style);
    assert_eq!(
        click(&mut chooser, centre(nature), style),
        ChooserAction::Changed
    );
    assert_eq!(chooser.active_category(), Some("Nature"));
    let shown = chooser.visible().to_vec();
    assert!(
        shown.len() < chooser.candidates().len(),
        "the filter is hiding candidates"
    );
    let wanted = pictures_among(&chooser, &shown);
    assert!(!wanted.is_empty(), "the filter left pictures to render");
    assert!(
        wanted.iter().any(|index| *index > wanted.len()),
        "the shown set must not be the catalog's leading run"
    );

    for _ in 0..wanted.len() {
        let request = chooser
            .next_thumbnail(style)
            .expect("a shown tile still needs a thumbnail");
        assert!(
            shown.contains(&request.index),
            "a hidden candidate was rendered before a shown one"
        );
        let pixels = Surface::new(request.side, request.side).expect("a test surface");
        chooser.set_thumbnail(request.index, pixels, style, &mut damage::sink());
    }
}

/// A tile-side change — a window resize, or a change of UI scale — used to
/// re-render **every** master from its 4K source, repeating the whole pass. It
/// must re-render what is on screen and leave the rest to be served as it is
/// scrolled to.
///
/// The stale state is spelled directly, as thumbnails held at a smaller side
/// than the tile now wants: that is exactly what a scale change leaves behind,
/// and it leaves the geometry alone so the visible set is the one the painter
/// would use.
#[test]
fn a_tile_side_change_re_renders_the_visible_tiles_first() {
    let registry = ThemeRegistry::with_builtins();
    let style = style_for(registry.active());
    let mut chooser = Chooser::new(catalog(24), &settings_without_a_wallpaper());
    chooser.relayout(MIN_WIN_WIDTH, MIN_WIN_HEIGHT);

    // Fill the whole gallery at the side the tiles want, so nothing is
    // outstanding.
    while let Some(request) = chooser.next_thumbnail(style) {
        let pixels = Surface::new(request.side, request.side).expect("a test surface");
        chooser.set_thumbnail(request.index, pixels, style, &mut damage::sink());
    }

    // Scroll off the first row, so index order and screen order differ.
    for _ in 0..3 {
        let _ = chooser.on_pointer(
            &InputEvent::PointerScrolled { dx: 0, dy: 3 },
            style,
            &mut damage::sink(),
        );
    }
    assert!(chooser.scroll_offset() > 0, "the gallery scrolled");

    // Then make every held thumbnail the wrong side, installed by index rather
    // than through the scheduler that is the thing under test.
    let (width, height) = chooser.layout(style).tile_size();
    let stale_side =
        IconTile::icon_side(Rect::new(0, 0, width, height), style.scale(), style.theme()) / 2;
    assert!(stale_side > 0, "a stale thumbnail still has pixels");
    for index in chooser.visible().to_vec() {
        if matches!(chooser.candidates()[index].thumbnail, Thumbnail::Ready(_)) {
            let undersized = Surface::new(stale_side, stale_side).expect("a test surface");
            chooser.set_thumbnail(index, undersized, style, &mut damage::sink());
        }
    }

    let shown = on_screen(&chooser, style);
    let wanted = pictures_among(&chooser, &shown);
    assert!(!wanted.is_empty(), "the window shows tiles to re-render");
    assert!(
        shown.len() < chooser.visible().len(),
        "the assertion is only meaningful while some tiles are off screen"
    );
    assert!(
        wanted.iter().any(|index| *index > wanted.len()),
        "the on-screen set must not be the catalog's leading run"
    );

    for _ in 0..wanted.len() {
        let request = chooser
            .next_thumbnail(style)
            .expect("a visible tile is stale at the wanted side");
        assert!(
            shown.contains(&request.index),
            "an off-screen tile was re-rendered before a visible one"
        );
        let pixels = Surface::new(request.side, request.side).expect("a test surface");
        chooser.set_thumbnail(request.index, pixels, style, &mut damage::sink());
    }
}

#[test]
fn clicking_a_tile_selects_it_and_moves_the_keyboard_there_too() {
    let registry = ThemeRegistry::with_builtins();
    let style = style_for(registry.active());
    let mut chooser = sample_chooser();
    assert_eq!(chooser.selected(), 1);

    let target = tile_rect(&chooser, 2, style);
    assert_eq!(
        click(&mut chooser, centre(target), style),
        ChooserAction::Changed
    );
    assert_eq!(chooser.selected(), 2);
    assert_eq!(chooser.focus(), Focus::Gallery);
}

#[test]
fn a_press_released_away_from_the_tile_it_started_on_selects_nothing() {
    let registry = ThemeRegistry::with_builtins();
    let style = style_for(registry.active());
    let mut chooser = sample_chooser();
    let before = chooser.selected();

    let target = tile_rect(&chooser, 2, style);
    let elsewhere = tile_rect(&chooser, 0, style);
    let _ = move_to(&mut chooser, centre(target), style);
    let _ = press(&mut chooser, style);
    let _ = move_to(&mut chooser, centre(elsewhere), style);
    let _ = release(&mut chooser, style);

    assert_eq!(chooser.selected(), before);
}

#[test]
fn hovering_a_tile_changes_what_the_gallery_draws() {
    let registry = ThemeRegistry::with_builtins();
    let style = style_for(registry.active());
    let mut chooser = sample_chooser();

    let resting = shot(&mut chooser, style);
    let target = tile_rect(&chooser, 2, style);
    assert_eq!(
        move_to(&mut chooser, centre(target), style),
        ChooserAction::Changed
    );
    let hovered = shot(&mut chooser, style);
    assert_ne!(resting.pixels(), hovered.pixels());

    // ...and moving away puts it back exactly as it was.
    let _ = move_to(&mut chooser, Point::new(0, 0), style);
    let left = shot(&mut chooser, style);
    assert_eq!(resting.pixels(), left.pixels());
}

#[test]
fn pressing_a_tile_draws_it_pressed_before_the_release_decides() {
    let registry = ThemeRegistry::with_builtins();
    let style = style_for(registry.active());
    let mut chooser = sample_chooser();

    let target = tile_rect(&chooser, 2, style);
    let _ = move_to(&mut chooser, centre(target), style);
    let hovered = shot(&mut chooser, style);
    let _ = press(&mut chooser, style);
    assert_eq!(chooser.armed(), Some(2));
    let pressed = shot(&mut chooser, style);
    assert_ne!(hovered.pixels(), pressed.pixels());
}

#[test]
fn clicking_apply_asks_to_apply_and_clicking_close_asks_to_close() {
    let registry = ThemeRegistry::with_builtins();
    let style = style_for(registry.active());
    let mut chooser = sample_chooser();
    let layout = chooser.layout(style);

    assert_eq!(
        click(&mut chooser, centre(layout.apply()), style),
        ChooserAction::Apply
    );
    assert_eq!(
        click(&mut chooser, centre(layout.close()), style),
        ChooserAction::Close
    );
}

#[test]
fn a_press_on_apply_released_off_it_applies_nothing() {
    let registry = ThemeRegistry::with_builtins();
    let style = style_for(registry.active());
    let mut chooser = sample_chooser();
    let layout = chooser.layout(style);

    let _ = move_to(&mut chooser, centre(layout.apply()), style);
    let _ = press(&mut chooser, style);
    let _ = move_to(&mut chooser, centre(layout.status()), style);
    assert_ne!(release(&mut chooser, style), ChooserAction::Apply);
}

#[test]
fn hovering_apply_changes_what_the_footer_draws() {
    let registry = ThemeRegistry::with_builtins();
    let style = style_for(registry.active());
    let mut chooser = sample_chooser();
    let layout = chooser.layout(style);

    let resting = shot(&mut chooser, style);
    assert_eq!(
        move_to(&mut chooser, centre(layout.apply()), style),
        ChooserAction::Changed
    );
    let hovered = shot(&mut chooser, style);
    assert_ne!(resting.pixels(), hovered.pixels());
}

#[test]
fn clicking_a_field_opens_its_list_and_choosing_a_row_changes_the_setting() {
    let registry = ThemeRegistry::with_builtins();
    let style = style_for(registry.active());
    let mut chooser = sample_chooser();
    assert_eq!(chooser.fit(), WallpaperFit::Fill);

    let field = chooser.layout(style).option_field(OptionGroup::Fit);
    let _ = click(&mut chooser, centre(field), style);
    assert_eq!(chooser.expanded(), Some(OptionGroup::Fit));

    let second = popup_row(&chooser, OptionGroup::Fit, 1, style);
    let _ = click(&mut chooser, second, style);

    assert_eq!(chooser.expanded(), None);
    assert_eq!(chooser.fit(), WallpaperFit::Fit);
    assert_eq!(chooser.to_settings().fit, WallpaperFit::Fit);
}

#[test]
fn an_open_list_takes_the_click_that_dismisses_it_and_the_gallery_does_not() {
    let registry = ThemeRegistry::with_builtins();
    let style = style_for(registry.active());
    let mut chooser = sample_chooser();
    let selected = chooser.selected();

    let field = chooser.layout(style).option_field(OptionGroup::Sort);
    let _ = click(&mut chooser, centre(field), style);
    assert_eq!(chooser.expanded(), Some(OptionGroup::Sort));

    // A click on a tile while the list is open dismisses the list and
    // reaches nothing beneath it.
    let tile = tile_rect(&chooser, 2, style);
    let _ = click(&mut chooser, centre(tile), style);
    assert_eq!(chooser.expanded(), None);
    assert_eq!(chooser.selected(), selected);
}

#[test]
fn the_wheel_scrolls_the_gallery_and_stops_at_both_ends() {
    let registry = ThemeRegistry::with_builtins();
    let style = style_for(registry.active());
    let mut chooser = Chooser::new(catalog(24), &settings_without_a_wallpaper());
    chooser.relayout(MIN_WIN_WIDTH, MIN_WIN_HEIGHT);
    assert_eq!(chooser.scroll_offset(), 0);

    let scrolled = chooser.on_pointer(
        &InputEvent::PointerScrolled { dx: 0, dy: 3 },
        style,
        &mut damage::sink(),
    );
    assert_eq!(scrolled, ChooserAction::Changed);
    assert!(chooser.scroll_offset() > 0);

    for _ in 0..64 {
        let _ = chooser.on_pointer(
            &InputEvent::PointerScrolled { dx: 0, dy: 8 },
            style,
            &mut damage::sink(),
        );
    }
    let at_end = chooser.scroll_offset();
    assert_eq!(
        chooser.on_pointer(
            &InputEvent::PointerScrolled { dx: 0, dy: 8 },
            style,
            &mut damage::sink()
        ),
        ChooserAction::None
    );
    assert_eq!(chooser.scroll_offset(), at_end);

    for _ in 0..80 {
        let _ = chooser.on_pointer(
            &InputEvent::PointerScrolled { dx: 0, dy: -8 },
            style,
            &mut damage::sink(),
        );
    }
    assert_eq!(chooser.scroll_offset(), 0);
}

#[test]
fn the_wheel_repaints_the_tiles_it_scrolled() {
    let registry = ThemeRegistry::with_builtins();
    let style = style_for(registry.active());
    let mut chooser = Chooser::new(catalog(24), &settings_without_a_wallpaper());
    chooser.relayout(MIN_WIN_WIDTH, MIN_WIN_HEIGHT);
    let _ = shot_at(&mut chooser, style, MIN_WIN_WIDTH, MIN_WIN_HEIGHT);

    let tiles = chooser.layout(style).tiles();
    let mut damage = damage::sink();
    let scrolled = chooser.on_pointer(
        &InputEvent::PointerScrolled { dx: 0, dy: 3 },
        style,
        &mut damage,
    );
    assert_eq!(scrolled, ChooserAction::Changed);
    assert!(chooser.scroll_offset() > 0);
    assert_eq!(
        damage.bounds().intersection(&tiles),
        tiles,
        "a wheel tick moves every tile, so the whole viewport is reported: \
         the bar reports only its own pixels"
    );
}

#[test]
fn a_wheel_scroll_leaves_no_stale_tile_behind() {
    let registry = ThemeRegistry::with_builtins();
    let style = style_for(registry.active());
    let mut chooser = Chooser::new(catalog(24), &settings_without_a_wallpaper());
    chooser.relayout(MIN_WIN_WIDTH, MIN_WIN_HEIGHT);

    every_changed_pixel_is_reported(
        &mut chooser,
        style,
        MIN_WIN_WIDTH,
        MIN_WIN_HEIGHT,
        "a wheel tick",
        |chooser, reported| {
            let _ = chooser.on_pointer(
                &InputEvent::PointerScrolled { dx: 0, dy: 3 },
                style,
                reported,
            );
        },
    );
    assert!(chooser.scroll_offset() > 0, "the wheel moved the gallery");
}

#[test]
fn a_keyboard_reveal_leaves_no_stale_tile_behind() {
    let registry = ThemeRegistry::with_builtins();
    let style = style_for(registry.active());
    let mut chooser = Chooser::new(catalog(24), &settings_without_a_wallpaper());
    chooser.relayout(MIN_WIN_WIDTH, MIN_WIN_HEIGHT);
    let _ = shot_at(&mut chooser, style, MIN_WIN_WIDTH, MIN_WIN_HEIGHT);
    assert_eq!(
        chooser.focus(),
        Focus::Gallery,
        "a fresh chooser gives the gallery the keyboard"
    );

    // End selects the last tile, which is past the bottom of the viewport, so
    // revealing it scrolls the gallery.
    every_changed_pixel_is_reported(
        &mut chooser,
        style,
        MIN_WIN_WIDTH,
        MIN_WIN_HEIGHT,
        "End in the gallery",
        |chooser, reported| {
            let _ = chooser.on_key(
                Key::Named(NamedKey::End),
                Modifiers::default(),
                style,
                reported,
            );
        },
    );
    assert!(chooser.scroll_offset() > 0, "End revealed the last tile");
}

#[test]
fn dragging_the_thumb_leaves_no_stale_tile_behind() {
    let registry = ThemeRegistry::with_builtins();
    let style = style_for(registry.active());
    let mut chooser = Chooser::new(catalog(24), &settings_without_a_wallpaper());
    chooser.relayout(MIN_WIN_WIDTH, MIN_WIN_HEIGHT);
    let _ = shot_at(&mut chooser, style, MIN_WIN_WIDTH, MIN_WIN_HEIGHT);

    let gutter = chooser.layout(style).scrollbar();
    let grab = thumb_point(&chooser, style);
    let _ = move_to(&mut chooser, grab, style);
    let _ = press(&mut chooser, style);

    every_changed_pixel_is_reported(
        &mut chooser,
        style,
        MIN_WIN_WIDTH,
        MIN_WIN_HEIGHT,
        "a thumb drag",
        |chooser, reported| {
            let _ = chooser.on_pointer(
                &InputEvent::PointerMoved {
                    to: Point::new(grab.x, gutter.top() + to_i32(gutter.height)),
                },
                style,
                reported,
            );
        },
    );
    assert!(chooser.scroll_offset() > 0, "the drag moved the gallery");
}

#[test]
fn dragging_the_scrollbar_thumb_scrolls_the_gallery() {
    let registry = ThemeRegistry::with_builtins();
    let style = style_for(registry.active());
    let mut chooser = Chooser::new(catalog(24), &settings_without_a_wallpaper());
    chooser.relayout(MIN_WIN_WIDTH, MIN_WIN_HEIGHT);
    // One paint brings the bar's model in line with the resized gallery,
    // which is what gives it a thumb to grab.
    let _ = shot(&mut chooser, style);
    let gutter = chooser.layout(style).scrollbar();

    // Grab the thumb where it rests, then drag to the bottom of the track.
    let grab = thumb_point(&chooser, style);
    let _ = move_to(&mut chooser, grab, style);
    let _ = press(&mut chooser, style);
    let _ = move_to(
        &mut chooser,
        Point::new(grab.x, gutter.top() + to_i32(gutter.height)),
        style,
    );
    let dragged = chooser.scroll_offset();
    let _ = release(&mut chooser, style);

    assert!(dragged > 0);
    assert_eq!(chooser.scroll_offset(), dragged);
}

#[test]
fn the_preview_is_re_asked_for_when_the_selection_changes_and_never_shows_the_old_one() {
    let registry = ThemeRegistry::with_builtins();
    let style = style_for(registry.active());
    let mut chooser = sample_chooser();

    let first = chooser.next_preview(style).expect("a preview to render");
    let pixels = Surface::new(first.width, first.height).expect("a test surface");
    chooser.set_preview(first.clone(), pixels, style, &mut damage::sink());
    assert!(chooser.next_preview(style).is_none());

    let target = tile_rect(&chooser, 2, style);
    let _ = click(&mut chooser, centre(target), style);
    let second = chooser.next_preview(style).expect("a new preview");
    assert_ne!(second.path, first.path);
    // The pixels held are the previous selection's, so the panel has none.
    assert!(chooser.preview_surface(&second).is_none());
}

#[test]
fn changing_the_fit_re_asks_for_the_preview_but_leaves_the_thumbnails_alone() {
    let registry = ThemeRegistry::with_builtins();
    let style = style_for(registry.active());
    let mut chooser = sample_chooser();

    while let Some(request) = chooser.next_thumbnail(style) {
        let pixels = Surface::new(request.side, request.side).expect("a test surface");
        chooser.set_thumbnail(request.index, pixels, style, &mut damage::sink());
    }
    let held = chooser.next_preview(style).expect("a preview to render");
    let pixels = Surface::new(held.width, held.height).expect("a test surface");
    chooser.set_preview(held.clone(), pixels, style, &mut damage::sink());

    let field = chooser.layout(style).option_field(OptionGroup::Fit);
    let _ = click(&mut chooser, centre(field), style);
    let second = popup_row(&chooser, OptionGroup::Fit, 1, style);
    let _ = click(&mut chooser, second, style);

    let again = chooser.next_preview(style).expect("the fit changed");
    assert_eq!(again.path, held.path);
    assert_ne!(again.fit, held.fit);
    // A thumbnail is the wallpaper itself, not a fit preview, so none of
    // them is asked for again.
    assert!(chooser.next_thumbnail(style).is_none());
}

#[test]
fn a_refused_preview_is_remembered_rather_than_asked_for_on_every_paint() {
    let registry = ThemeRegistry::with_builtins();
    let style = style_for(registry.active());
    let mut chooser = sample_chooser();

    let request = chooser.next_preview(style).expect("a preview to render");
    chooser.mark_preview_refused(request.clone(), style, &mut damage::sink());
    assert!(chooser.next_preview(style).is_none());
    assert!(chooser.preview_refused(&request));
    assert!(chooser.preview_surface(&request).is_none());
}

#[test]
fn selecting_the_no_wallpaper_entry_asks_for_no_preview_at_all() {
    let registry = ThemeRegistry::with_builtins();
    let style = style_for(registry.active());
    let mut chooser = sample_chooser();

    let none_tile = tile_rect(&chooser, 0, style);
    let _ = click(&mut chooser, centre(none_tile), style);
    assert_eq!(chooser.selected(), 0);
    assert!(chooser.next_preview(style).is_none());
    assert!(chooser.wanted_preview(style).is_none());
}

#[test]
fn tab_cycles_focus_through_every_region_and_wraps() {
    let registry = ThemeRegistry::with_builtins();
    let style = style_for(registry.active());
    let mut chooser = sample_chooser();
    let opened_on = chooser.focus();

    let mut seen = vec![opened_on];
    for _ in 1..Focus::ORDER.len() {
        let _ = key(&mut chooser, NamedKey::Tab, style);
        seen.push(chooser.focus());
    }
    for region in Focus::ORDER {
        assert_eq!(
            seen.iter().filter(|visited| **visited == region).count(),
            1,
            "{region:?} is visited exactly once per cycle"
        );
    }
    let _ = key(&mut chooser, NamedKey::Tab, style);
    assert_eq!(chooser.focus(), opened_on, "the cycle wraps");
}

#[test]
fn shift_tab_cycles_focus_backward_and_wraps() {
    let registry = ThemeRegistry::with_builtins();
    let style = style_for(registry.active());
    let mut chooser = sample_chooser();
    let back = Modifiers {
        shift: true,
        ..Modifiers::default()
    };

    let opened_on = chooser.focus();
    let _ = chooser.on_key(Key::Named(NamedKey::Tab), back, style, &mut damage::sink());
    assert_ne!(chooser.focus(), opened_on);
    let _ = key(&mut chooser, NamedKey::Tab, style);
    assert_eq!(chooser.focus(), opened_on, "back then forward returns");

    let mut seen = vec![opened_on];
    for _ in 1..Focus::ORDER.len() {
        let _ = chooser.on_key(Key::Named(NamedKey::Tab), back, style, &mut damage::sink());
        seen.push(chooser.focus());
    }
    for region in Focus::ORDER {
        assert_eq!(
            seen.iter().filter(|visited| **visited == region).count(),
            1,
            "{region:?} is visited exactly once per backward cycle"
        );
    }
    let _ = chooser.on_key(Key::Named(NamedKey::Tab), back, style, &mut damage::sink());
    assert_eq!(chooser.focus(), opened_on, "the backward cycle wraps");
}

#[test]
fn the_keyboard_reaches_apply_and_close_and_escape_always_closes() {
    let registry = ThemeRegistry::with_builtins();
    let style = style_for(registry.active());
    let mut chooser = sample_chooser();

    while chooser.focus() != Focus::Apply {
        let _ = key(&mut chooser, NamedKey::Tab, style);
    }
    assert_eq!(
        key(&mut chooser, NamedKey::Enter, style),
        ChooserAction::Apply
    );
    let _ = key(&mut chooser, NamedKey::Tab, style);
    assert_eq!(chooser.focus(), Focus::Categories);
    assert_eq!(
        key(&mut chooser, NamedKey::Escape, style),
        ChooserAction::Close
    );

    while chooser.focus() != Focus::Close {
        let _ = key(&mut chooser, NamedKey::Tab, style);
    }
    assert_eq!(
        key(&mut chooser, NamedKey::Enter, style),
        ChooserAction::Close
    );
}

#[test]
fn the_arrow_keys_move_the_gallery_selection_and_stop_at_the_edges() {
    let registry = ThemeRegistry::with_builtins();
    let style = style_for(registry.active());
    let mut chooser = sample_chooser();
    assert_eq!(chooser.focus(), Focus::Gallery);
    let last = chooser.candidates().len() - 1;

    let _ = key(&mut chooser, NamedKey::Home, style);
    assert_eq!(chooser.selected(), 0);
    assert_eq!(
        key(&mut chooser, NamedKey::Left, style),
        ChooserAction::None
    );
    assert_eq!(chooser.selected(), 0);

    let _ = key(&mut chooser, NamedKey::Right, style);
    assert_eq!(chooser.selected(), 1);
    let _ = key(&mut chooser, NamedKey::End, style);
    assert_eq!(chooser.selected(), last);
    let _ = key(&mut chooser, NamedKey::Right, style);
    assert_eq!(chooser.selected(), last);
}

#[test]
fn the_keyboard_opens_a_field_and_chooses_from_it() {
    let registry = ThemeRegistry::with_builtins();
    let style = style_for(registry.active());
    let mut chooser = sample_chooser();
    assert_eq!(chooser.sort(), IconSort::Name);

    while chooser.focus() != Focus::Setting(OptionGroup::Sort) {
        let _ = key(&mut chooser, NamedKey::Tab, style);
    }
    let _ = key(&mut chooser, NamedKey::Enter, style);
    assert_eq!(chooser.expanded(), Some(OptionGroup::Sort));
    let _ = key(&mut chooser, NamedKey::Down, style);
    let _ = key(&mut chooser, NamedKey::Enter, style);
    assert_eq!(chooser.expanded(), None);
    assert_eq!(chooser.sort(), IconSort::Kind);
}

#[test]
fn backdrop_options_always_offer_the_current_backdrop() {
    let unlisted = Backdrop::Colour(Rgb::new(0x12, 0x34, 0x56));
    let options = backdrop_options(unlisted);
    assert_eq!(options.len(), BACKDROP_PALETTE.len() + 1);
    assert_eq!(options[options.len() - 1].label, "123456");
    assert!(options.iter().any(|option| option.backdrop == unlisted));

    let listed = backdrop_options(Backdrop::Theme);
    assert_eq!(listed.len(), BACKDROP_PALETTE.len());
}

#[test]
fn a_current_colour_outside_the_palette_is_offered_and_carried_through() {
    let unlisted = Backdrop::Colour(Rgb::new(0x12, 0x34, 0x56));
    let settings = PinboardSettings {
        backdrop: unlisted,
        ..PinboardSettings::default()
    };
    let chooser = Chooser::new(catalog(1), &settings);
    assert_eq!(chooser.backdrop(), unlisted);
    assert_eq!(chooser.to_settings().backdrop, unlisted);
}

#[test]
fn the_rendered_document_matches_the_state_the_controls_are_in() {
    let registry = ThemeRegistry::with_builtins();
    let style = style_for(registry.active());
    let mut chooser = sample_chooser();

    let tile = tile_rect(&chooser, 2, style);
    let _ = click(&mut chooser, centre(tile), style);
    let document = chooser.settings_document();
    // The document goes on the wire to the session, which reads it with the
    // registry's *strict* reading — so that is what this asserts against.
    let parsed = tairix_wallpaper::decode(&document).expect("a valid document");
    assert_eq!(parsed, chooser.to_settings());
    assert_eq!(
        parsed.wallpaper,
        chooser.candidates()[chooser.selected()].choice
    );
}

#[test]
fn an_apply_outcome_starts_absent_and_reports_exactly_what_was_set() {
    let registry = ThemeRegistry::with_builtins();
    let style = style_for(registry.active());
    let mut chooser = sample_chooser();
    assert!(chooser.apply_outcome().is_none());
    chooser.set_apply_outcome(
        ApplyOutcome::Refused(String::from("denied")),
        style,
        &mut damage::sink(),
    );
    assert_eq!(
        chooser.apply_outcome(),
        Some(&ApplyOutcome::Refused(String::from("denied")))
    );
}

#[test]
fn every_apply_outcome_draws_a_distinct_footer() {
    let registry = ThemeRegistry::with_builtins();
    let style = style_for(registry.active());
    let mut chooser = sample_chooser();

    let silent = shot(&mut chooser, style);
    let mut rendered = vec![silent.pixels().to_vec()];
    for outcome in [
        ApplyOutcome::Applied,
        ApplyOutcome::Refused(String::from("the session said no")),
        ApplyOutcome::NoDesktop,
    ] {
        chooser.set_apply_outcome(outcome, style, &mut damage::sink());
        let painted = shot(&mut chooser, style);
        let pixels = painted.pixels().to_vec();
        assert!(!rendered.contains(&pixels));
        rendered.push(pixels);
    }
}

#[test]
fn the_layout_keeps_every_region_inside_the_window_at_every_size() {
    let registry = ThemeRegistry::with_builtins();
    let style = style_for(registry.active());
    for (width, height) in [
        (MIN_WIN_WIDTH, MIN_WIN_HEIGHT),
        (WIN_WIDTH, WIN_HEIGHT),
        (640, 400),
        (1600, 1200),
    ] {
        let layout = Layout::compute(
            width,
            height,
            style.scale(),
            style.theme(),
            style.font(),
            style.screen(),
            TEST_RAIL,
        );
        let regions = [
            layout.preview(),
            layout.caption(),
            layout.heading(),
            layout.categories(),
            layout.tiles(),
            layout.scrollbar(),
            layout.status(),
            layout.apply(),
            layout.close(),
            layout.option_field(OptionGroup::Fit),
            layout.option_field(OptionGroup::Sort),
        ];
        for region in regions {
            assert!(region.left() >= 0, "{region:?} at {width}x{height}");
            assert!(region.top() >= 0, "{region:?} at {width}x{height}");
            assert!(
                to_u32(region.right()) <= width,
                "{region:?} spills past {width}"
            );
            assert!(
                to_u32(region.bottom()) <= height,
                "{region:?} spills past {height}"
            );
        }
        // The regions that share a column never overlap each other.
        assert!(layout.preview().right() <= layout.option_field(OptionGroup::Fit).left());
        if !layout.categories().is_empty() {
            assert!(
                layout.categories().right() <= layout.tiles().left(),
                "the rail and the tiles overlap at {width}x{height}"
            );
            assert_eq!(layout.categories().top(), layout.tiles().top());
            assert_eq!(layout.categories().height, layout.tiles().height);
        }
        assert!(layout.tiles().right() <= layout.scrollbar().left());
        assert!(layout.status().right() <= layout.close().left());
        assert!(layout.close().right() <= layout.apply().left());
        assert!(layout.heading().top() >= layout.preview().bottom());
        assert!(layout.tiles().top() >= layout.heading().bottom());
        assert!(layout.apply().top() >= layout.tiles().bottom());
    }
}

#[test]
fn the_chooser_paints_within_the_surface_it_is_handed_at_every_size() {
    let registry = ThemeRegistry::with_builtins();
    let style = style_for(registry.active());
    let mut chooser = sample_chooser();
    let mut painted = alloc::vec::Vec::new();
    for (width, height) in [(MIN_WIN_WIDTH, MIN_WIN_HEIGHT), (WIN_WIDTH, WIN_HEIGHT)] {
        chooser.relayout(width, height);
        let shot = shot_at(&mut chooser, style, width, height);
        assert_eq!((shot.width(), shot.height()), (width, height));
        // Something was drawn: the surface starts transparent and the window
        // background alone makes every pixel opaque.
        assert!(shot.pixels().iter().all(|pixel| pixel.a == 255));
        painted.push(shot);
    }
    assert_ne!(
        painted[0].width(),
        painted[1].width(),
        "the two sizes must differ for this to prove anything"
    );
}

#[test]
fn a_window_resize_re_flows_the_gallery_and_clamps_the_scroll() {
    let registry = ThemeRegistry::with_builtins();
    let style = style_for(registry.active());
    let mut chooser = Chooser::new(catalog(24), &settings_without_a_wallpaper());
    chooser.relayout(MIN_WIN_WIDTH, MIN_WIN_HEIGHT);

    for _ in 0..64 {
        let _ = chooser.on_pointer(
            &InputEvent::PointerScrolled { dx: 0, dy: 8 },
            style,
            &mut damage::sink(),
        );
    }
    let deep = chooser.scroll_offset();
    assert!(deep > 0);

    // A window big enough for every tile has nothing left to scroll to.
    chooser.relayout(1600, 1200);
    let _ = shot(&mut chooser, style);
    let grid = chooser.layout(style).grid(chooser.visible().len());
    assert!(grid.lines_total() <= grid.visible_lines());
    assert_eq!(chooser.scroll_offset(), 0);
}

#[test]
fn a_secondary_button_click_changes_nothing() {
    let registry = ThemeRegistry::with_builtins();
    let style = style_for(registry.active());
    let mut chooser = sample_chooser();
    let selected = chooser.selected();

    let tile = tile_rect(&chooser, 2, style);
    let _ = move_to(&mut chooser, centre(tile), style);
    let _ = chooser.on_pointer(
        &InputEvent::PointerPressed {
            button: PointerButton::Secondary,
        },
        style,
        &mut damage::sink(),
    );
    let action = chooser.on_pointer(
        &InputEvent::PointerReleased {
            button: PointerButton::Secondary,
        },
        style,
        &mut damage::sink(),
    );
    assert_eq!(action, ChooserAction::None);
    assert_eq!(chooser.selected(), selected);
}

// ---- the category rail ---------------------------------------------------

/// A chooser over three categories, opened on no wallpaper at all, laid out
/// large enough that the rail is drawn.
fn categorised_chooser() -> Chooser {
    let mut chooser = Chooser::new(
        catalog_over(&[("Space", 3), ("Nature", 2), ("Abstract", 1)]),
        &settings_without_a_wallpaper(),
    );
    chooser.relayout(WIN_WIDTH, WIN_HEIGHT);
    chooser
}

#[test]
fn the_rail_offers_the_all_entry_then_every_discovered_category_in_order() {
    let chooser = categorised_chooser();
    assert_eq!(chooser.categories(), ["Abstract", "Nature", "Space"]);
    let labels: Vec<&str> = chooser.rail().tabs().iter().map(Tab::label).collect();
    assert_eq!(
        labels,
        [ALL_CATEGORIES_LABEL, "Abstract", "Nature", "Space"],
        "the rail is derived from the store, and `All` leads it"
    );
    assert_eq!(chooser.active_category(), None);
}

#[test]
fn a_store_with_no_categories_offers_no_rail_and_gives_its_width_to_the_tiles() {
    let registry = ThemeRegistry::with_builtins();
    let style = style_for(registry.active());
    // Only the "no wallpaper" entry, which belongs to no category.
    let chooser = Chooser::new(Vec::new(), &settings_without_a_wallpaper());
    assert!(chooser.categories().is_empty());
    assert!(chooser.rail().is_empty());

    let layout = chooser.layout(style);
    assert!(layout.categories().is_empty());
    assert_eq!(layout.tiles().left(), layout.heading().left());
}

#[test]
fn the_chooser_opens_on_the_category_holding_the_selection() {
    let chooser = Chooser::new(
        catalog_over(&[("Space", 2), ("Nature", 2)]),
        &settings_selecting("/System/Graphics/Wallpapers/Nature/image-01.png"),
    );
    assert_eq!(chooser.active_category(), Some("Nature"));
    assert_eq!(
        chooser.candidates()[chooser.selected()].category.as_deref(),
        Some("Nature")
    );
    assert!(
        chooser.visible().contains(&chooser.selected()),
        "the wallpaper in effect is on show when the chooser opens"
    );
}

#[test]
fn clicking_a_category_narrows_the_gallery_to_it() {
    let registry = ThemeRegistry::with_builtins();
    let style = style_for(registry.active());
    let mut chooser = categorised_chooser();
    let everything = chooser.visible().len();

    // Rail entry 3 is `Space`, which holds three of the six images.
    let space = rail_rect(&chooser, 3, style);
    assert_eq!(
        click(&mut chooser, centre(space), style),
        ChooserAction::Changed
    );
    assert_eq!(chooser.active_category(), Some("Space"));
    assert_eq!(chooser.focus(), Focus::Categories);

    let shown: Vec<Option<&str>> = chooser
        .visible()
        .iter()
        .map(|index| chooser.candidates()[*index].category.as_deref())
        .collect();
    assert!(shown.len() < everything);
    for category in &shown {
        assert!(
            matches!(category, None | Some("Space")),
            "a narrowed gallery shows only its own category and the entries that belong to every one"
        );
    }
}

#[test]
fn narrowing_the_gallery_repaints_the_tiles_it_replaced() {
    let registry = ThemeRegistry::with_builtins();
    let style = style_for(registry.active());
    let mut chooser = categorised_chooser();

    // Rail entry 3 is `Space`; entry 0 is `All`. Both changes happen with the
    // gallery already at its top, which is the case a scroll-driven repaint
    // would miss: nothing moves, and every tile is still a different
    // candidate afterwards.
    for entry in [3, 0] {
        let _ = shot(&mut chooser, style);
        let tiles = chooser.layout(style).tiles();
        let at = centre(rail_rect(&chooser, entry, style));
        let _ = move_to(&mut chooser, at, style);
        let _ = press(&mut chooser, style);

        let mut damage = damage::sink();
        assert_eq!(
            release_reporting(&mut chooser, style, &mut damage),
            ChooserAction::Changed
        );
        assert_eq!(chooser.scroll_offset(), 0);
        assert_eq!(
            damage.bounds().intersection(&tiles),
            tiles,
            "rail entry {entry} shows a different set of candidates, so the \
             whole viewport is reported"
        );
    }
}

#[test]
fn a_category_change_leaves_no_stale_tile_behind() {
    let registry = ThemeRegistry::with_builtins();
    let style = style_for(registry.active());
    let mut chooser = categorised_chooser();

    // The release is the event that selects, so it is the one round whose
    // report has to cover the tiles the narrowing replaced.
    for entry in [3, 0] {
        let at = centre(rail_rect(&chooser, entry, style));
        let _ = move_to(&mut chooser, at, style);
        let _ = press(&mut chooser, style);
        every_changed_pixel_is_reported(
            &mut chooser,
            style,
            WIN_WIDTH,
            WIN_HEIGHT,
            "a rail click",
            |chooser, reported| {
                let _ = release_reporting(chooser, style, reported);
            },
        );
    }
}

#[test]
fn the_no_wallpaper_entry_stays_offered_in_every_category() {
    let registry = ThemeRegistry::with_builtins();
    let style = style_for(registry.active());
    let mut chooser = categorised_chooser();

    for entry in 0..chooser.rail().len() {
        let target = rail_rect(&chooser, entry, style);
        let _ = click(&mut chooser, centre(target), style);
        assert!(
            chooser.visible().contains(&0),
            "the plain-backdrop choice must be reachable from every rail entry"
        );
    }
}

#[test]
fn a_wallpaper_from_outside_the_store_stays_offered_in_every_category() {
    let registry = ThemeRegistry::with_builtins();
    let style = style_for(registry.active());
    let mut chooser = Chooser::new(
        catalog_over(&[("Space", 2), ("Nature", 2)]),
        &settings_selecting("/Users/ada/Pictures/holiday.png"),
    );
    chooser.relayout(WIN_WIDTH, WIN_HEIGHT);
    let outsider = chooser.selected();
    assert_eq!(chooser.candidates()[outsider].category, None);

    for entry in 0..chooser.rail().len() {
        let target = rail_rect(&chooser, entry, style);
        let _ = click(&mut chooser, centre(target), style);
        assert!(
            chooser.visible().contains(&outsider),
            "the wallpaper actually in effect is never the one thing hidden"
        );
    }
}

#[test]
fn narrowing_the_gallery_leaves_the_selection_and_its_preview_alone() {
    let registry = ThemeRegistry::with_builtins();
    let style = style_for(registry.active());
    let mut chooser = Chooser::new(
        catalog_over(&[("Space", 2), ("Nature", 2)]),
        &settings_selecting("/System/Graphics/Wallpapers/Space/image-00.png"),
    );
    chooser.relayout(WIN_WIDTH, WIN_HEIGHT);
    let chosen = chooser.selected();
    let wanted = chooser.wanted_preview(style).expect("a preview to render");

    // `Nature` does not hold the selection, so it drops out of the gallery —
    // but it is still what would be applied, and still what the preview
    // shows.
    let nature = rail_rect(&chooser, 1, style);
    let _ = click(&mut chooser, centre(nature), style);
    assert_eq!(chooser.active_category(), Some("Nature"));
    assert_eq!(chooser.selected(), chosen);
    assert!(!chooser.visible().contains(&chosen));
    assert_eq!(chooser.wanted_preview(style), Some(wanted));
    assert_eq!(
        chooser.to_settings().wallpaper,
        chooser.candidates()[chosen].choice
    );
}

#[test]
fn a_category_change_returns_the_gallery_to_its_top() {
    let registry = ThemeRegistry::with_builtins();
    let style = style_for(registry.active());
    let mut chooser = Chooser::new(
        catalog_over(&[("Space", 24), ("Nature", 24)]),
        &settings_without_a_wallpaper(),
    );
    chooser.relayout(MIN_WIN_WIDTH, MIN_WIN_HEIGHT);
    let _ = shot(&mut chooser, style);

    for _ in 0..8 {
        let _ = chooser.on_pointer(
            &InputEvent::PointerScrolled { dx: 0, dy: 4 },
            style,
            &mut damage::sink(),
        );
    }
    assert!(chooser.scroll_offset() > 0);

    let layout = chooser.layout(style);
    if layout.categories().is_empty() {
        // Too narrow for a rail: the keyboard is the path that remains.
        while chooser.focus() != Focus::Categories {
            let _ = key(&mut chooser, NamedKey::Tab, style);
        }
        return;
    }
    let space = rail_rect(&chooser, 2, style);
    let _ = click(&mut chooser, centre(space), style);
    assert_eq!(chooser.scroll_offset(), 0);
}

#[test]
fn the_keyboard_reaches_the_rail_and_narrows_the_gallery() {
    let registry = ThemeRegistry::with_builtins();
    let style = style_for(registry.active());
    let mut chooser = categorised_chooser();

    while chooser.focus() != Focus::Categories {
        let _ = key(&mut chooser, NamedKey::Tab, style);
    }
    // The rail's cursor starts on the active entry; Down walks to the next
    // one and Enter narrows the gallery to it.
    let _ = shot(&mut chooser, style);
    assert_eq!(chooser.rail().current(), Some(0));
    let _ = key(&mut chooser, NamedKey::Down, style);
    assert_eq!(chooser.rail().current(), Some(1));
    let _ = key(&mut chooser, NamedKey::Enter, style);
    assert_eq!(chooser.active_category(), Some("Abstract"));
}

#[test]
fn the_rail_ignores_the_axis_it_does_not_stack_along() {
    let registry = ThemeRegistry::with_builtins();
    let style = style_for(registry.active());
    let mut chooser = categorised_chooser();

    while chooser.focus() != Focus::Categories {
        let _ = key(&mut chooser, NamedKey::Tab, style);
    }
    let _ = shot(&mut chooser, style);
    let before = chooser.rail().current();
    let _ = key(&mut chooser, NamedKey::Right, style);
    assert_eq!(chooser.rail().current(), before);
}

#[test]
fn every_tile_a_narrowed_gallery_draws_is_the_tile_a_click_there_selects() {
    let registry = ThemeRegistry::with_builtins();
    let style = style_for(registry.active());
    let mut chooser = categorised_chooser();

    let space = rail_rect(&chooser, 3, style);
    let _ = click(&mut chooser, centre(space), style);

    // The painter walks the visible window's positions and the hit-test
    // resolves a click through the same one, so a click on the tile drawn at
    // a position must select the candidate that position names.
    let shown = chooser.visible().to_vec();
    assert!(shown.len() > 1, "the narrowed gallery draws several tiles");
    for index in shown {
        let target = tile_rect(&chooser, index, style);
        let _ = click(&mut chooser, centre(target), style);
        assert_eq!(chooser.selected(), index);
    }
}

// ---- the preview panel's true-scale screen model -------------------------

#[test]
fn the_preview_model_box_matches_the_screens_aspect_stays_within_and_centred_in_the_panel() {
    let registry = ThemeRegistry::with_builtins();
    let theme = registry.active();
    for (width, height) in [
        (MIN_WIN_WIDTH, MIN_WIN_HEIGHT),
        (WIN_WIDTH, WIN_HEIGHT),
        (1600, 1200),
    ] {
        for screen in [(1920, 1080), (1080, 1920), (4, 3), (21, 9)] {
            let style = style_with_screen(theme, screen);
            let layout = Layout::compute(
                width,
                height,
                style.scale(),
                style.theme(),
                style.font(),
                style.screen(),
                TEST_RAIL,
            );
            let panel = layout.preview();
            let model = layout.preview_model();

            // A panel with no room models nothing; otherwise the model is
            // always a real, non-empty rectangle.
            if panel.is_empty() {
                assert!(model.is_empty(), "{width}x{height} screen {screen:?}");
                continue;
            }
            assert!(!model.is_empty(), "{width}x{height} screen {screen:?}");

            // Never exceeds the panel.
            assert!(model.left() >= panel.left(), "{width}x{height} {screen:?}");
            assert!(model.top() >= panel.top(), "{width}x{height} {screen:?}");
            assert!(
                model.right() <= panel.right(),
                "{width}x{height} {screen:?}"
            );
            assert!(
                model.bottom() <= panel.bottom(),
                "{width}x{height} {screen:?}"
            );

            // Centred: the shared placement geometry's own centring offset
            // is a floor division, so the two margins on an axis differ by
            // at most the one pixel an odd remainder leaves.
            let left_margin = model.left() - panel.left();
            let right_margin = panel.right() - model.right();
            assert!(
                (left_margin - right_margin).abs() <= 1,
                "{width}x{height} {screen:?}: left {left_margin}, right {right_margin}"
            );
            let top_margin = model.top() - panel.top();
            let bottom_margin = panel.bottom() - model.bottom();
            assert!(
                (top_margin - bottom_margin).abs() <= 1,
                "{width}x{height} {screen:?}: top {top_margin}, bottom {bottom_margin}"
            );

            // The screen's own aspect ratio: `WallpaperFit::Fit`'s contain
            // arithmetic fixes one dimension to the panel's exactly and
            // floors the other, so the two cross products can differ by
            // less than the larger screen dimension, never more or in the
            // wrong direction.
            let (screen_w, screen_h) = (u64::from(screen.0), u64::from(screen.1));
            let width_cross = u64::from(model.width) * screen_h;
            let height_cross = u64::from(model.height) * screen_w;
            let diff = width_cross.abs_diff(height_cross);
            assert!(
                diff < screen_w.max(screen_h),
                "{width}x{height} screen {screen:?}: model {model:?}"
            );
        }
    }
}

#[test]
fn the_preview_model_box_for_a_portrait_screen_is_taller_than_wide() {
    let registry = ThemeRegistry::with_builtins();
    let theme = registry.active();

    let landscape = style_with_screen(theme, (1920, 1080));
    let landscape_model = Layout::compute(
        WIN_WIDTH,
        WIN_HEIGHT,
        landscape.scale(),
        landscape.theme(),
        landscape.font(),
        landscape.screen(),
        TEST_RAIL,
    )
    .preview_model();
    assert!(
        landscape_model.width > landscape_model.height,
        "{landscape_model:?}"
    );

    let portrait = style_with_screen(theme, (1080, 1920));
    let portrait_model = Layout::compute(
        WIN_WIDTH,
        WIN_HEIGHT,
        portrait.scale(),
        portrait.theme(),
        portrait.font(),
        portrait.screen(),
        TEST_RAIL,
    )
    .preview_model();
    assert!(
        portrait_model.height > portrait_model.width,
        "{portrait_model:?}"
    );
}

#[test]
fn a_screen_extent_change_invalidates_the_cached_preview_and_asks_again() {
    let registry = ThemeRegistry::with_builtins();
    let theme = registry.active();
    let mut chooser = sample_chooser();

    let landscape = style_with_screen(theme, (1920, 1080));
    let first = chooser
        .next_preview(landscape)
        .expect("a preview to render");
    let pixels = Surface::new(first.width, first.height).expect("a test surface");
    chooser.set_preview(first.clone(), pixels, landscape, &mut damage::sink());
    assert!(chooser.next_preview(landscape).is_none());
    assert!(chooser.preview_surface(&first).is_some());

    // The desktop's screen changed (a monitor swap, say): same window, same
    // selection, same fit, but a different screen the preview must model.
    // The held pixels answered the old screen and are unrepresentable as an
    // answer to the new one.
    let portrait = style_with_screen(theme, (1080, 1920));
    let second = chooser
        .next_preview(portrait)
        .expect("the screen change asks again");
    assert_ne!(second, first);
    assert_eq!(second.path, first.path);
    assert_eq!(second.fit, first.fit);
    assert_ne!(second.screen, first.screen);
    assert!(chooser.preview_surface(&second).is_none());
}

// --- C.3: what a round presents covers what it redrew -------------------
//
// Two directions, because either alone passes trivially: every pixel a round
// changes must lie inside what it reported (or a stale frame reaches the
// screen), and a round must report only where it changed (or reporting buys
// nothing over presenting the whole window).

/// Every pixel of `before` that `after` changed, as window coordinates.
fn changed_pixels(before: &Surface, after: &Surface) -> alloc::vec::Vec<Point> {
    let mut changed = alloc::vec::Vec::new();
    for y in 0..before.height() {
        for x in 0..before.width() {
            if before.get(x, y) != after.get(x, y) {
                changed.push(Point::new(to_i32(x), to_i32(y)));
            }
        }
    }
    changed
}

/// Feed one pointer `event`, returning what it repainted and what it reported.
fn round(
    chooser: &mut Chooser,
    event: &InputEvent,
    style: Style<'_>,
) -> (alloc::vec::Vec<Point>, Region) {
    round_at(
        chooser,
        style,
        WIN_WIDTH,
        WIN_HEIGHT,
        |chooser, reported| {
            let _ = chooser.on_pointer(event, style, reported);
        },
    )
}

/// [`round`] for any input on a `width` × `height` window: the tests that
/// scroll need a window its tiles outrun, and the keyboard reaches the same
/// viewport the pointer does.
fn round_at(
    chooser: &mut Chooser,
    style: Style<'_>,
    width: u32,
    height: u32,
    feed: impl FnOnce(&mut Chooser, &mut Region),
) -> (alloc::vec::Vec<Point>, Region) {
    let before = shot_at(chooser, style, width, height);
    let mut reported = damage::sink();
    feed(chooser, &mut reported);
    let after = shot_at(chooser, style, width, height);
    (changed_pixels(&before, &after), reported)
}

#[test]
fn every_pixel_a_chooser_round_changes_lies_inside_what_it_reported() {
    let registry = ThemeRegistry::with_builtins();
    let style = style_for(registry.active());
    let mut chooser = sample_chooser();
    let layout = chooser.layout(style);

    // A walk over every interactive region the chooser owns: onto a tile,
    // press and release it (which moves the selection and re-models the
    // preview), onto another tile, onto the Apply button, press and release,
    // onto the category rail, and out over the preview panel.
    let first = centre(tile_rect(&chooser, 1, style));
    let second = centre(tile_rect(&chooser, 2, style));
    let walk = [
        InputEvent::PointerMoved { to: first },
        InputEvent::PointerPressed {
            button: PointerButton::Primary,
        },
        InputEvent::PointerReleased {
            button: PointerButton::Primary,
        },
        InputEvent::PointerMoved { to: second },
        InputEvent::PointerMoved {
            to: centre(layout.apply()),
        },
        InputEvent::PointerPressed {
            button: PointerButton::Primary,
        },
        InputEvent::PointerReleased {
            button: PointerButton::Primary,
        },
        InputEvent::PointerMoved {
            to: centre(layout.categories()),
        },
        InputEvent::PointerMoved {
            to: centre(layout.preview()),
        },
    ];
    let mut steps_that_changed = 0;
    for (step, event) in walk.iter().enumerate() {
        let (changed, reported) = round(&mut chooser, event, style);
        if !changed.is_empty() {
            steps_that_changed += 1;
        }
        for point in changed {
            assert!(
                reported.contains(point),
                "step {step} ({event:?}) changed {point:?}, which it did not report"
            );
        }
    }
    assert!(
        steps_that_changed >= 5,
        "the walk must actually repaint for this to prove anything, not {steps_that_changed} steps"
    );
}

#[test]
fn hovering_a_tile_reports_that_tile_and_a_second_sample_in_it_reports_nothing() {
    let registry = ThemeRegistry::with_builtins();
    let style = style_for(registry.active());
    let mut chooser = sample_chooser();
    let tile = tile_rect(&chooser, 2, style);

    let mut reported = damage::sink();
    let _ = chooser.on_pointer(
        &InputEvent::PointerMoved { to: centre(tile) },
        style,
        &mut reported,
    );
    assert_eq!(
        reported.rects(),
        &[tile],
        "entering a tile reports exactly that tile, not the gallery"
    );

    // A second sample inside the same tile changes nothing, so it reports
    // nothing and the round presents nothing at all.
    let mut still = damage::sink();
    let asked = chooser.on_pointer(
        &InputEvent::PointerMoved {
            to: Point::new(centre(tile).x + 1, centre(tile).y),
        },
        style,
        &mut still,
    );
    assert_eq!(
        asked,
        ChooserAction::None,
        "an idle sample asks for nothing"
    );
    assert!(
        still.is_empty(),
        "an idle sample reports nothing: {:?}",
        still.rects()
    );
}

#[test]
fn a_delivered_thumbnail_reports_only_its_own_tile() {
    let registry = ThemeRegistry::with_builtins();
    let theme = registry.active();
    let style = style_for(theme);
    let mut chooser = sample_chooser();
    // Resolve the preview first, so the request that follows is a thumbnail.
    if let Some(request) = chooser.next_preview(style) {
        let pixels = Surface::new(request.width, request.height).expect("a test surface");
        chooser.set_preview(request, pixels, style, &mut damage::sink());
    }
    let request = chooser
        .next_thumbnail(style)
        .expect("a thumbnail to render");
    let tile = tile_rect(&chooser, request.index, style);

    let before = shot(&mut chooser, style);
    let mut reported = damage::sink();
    let pixels = Surface::new(request.side, request.side).expect("a test surface");
    chooser.set_thumbnail(request.index, pixels, style, &mut reported);
    let after = shot(&mut chooser, style);

    assert_eq!(
        reported.rects(),
        &[tile],
        "a delivered thumbnail redraws one tile, not the gallery"
    );
    for point in changed_pixels(&before, &after) {
        assert!(
            reported.contains(point),
            "the thumbnail changed {point:?}, which it did not report"
        );
    }
}

#[test]
fn tab_reports_the_two_regions_the_focus_ring_moves_between() {
    let registry = ThemeRegistry::with_builtins();
    let style = style_for(registry.active());
    let mut chooser = sample_chooser();
    let layout = chooser.layout(style);

    // From the settings drop-downs the ring walks to Close, then to Apply:
    // two buttons whose rectangles the layout names outright.
    while chooser.focus() != Focus::Close {
        let _ = key(&mut chooser, NamedKey::Tab, style);
    }
    let before = shot(&mut chooser, style);
    let mut reported = damage::sink();
    let _ = chooser.on_key(
        Key::Named(NamedKey::Tab),
        Modifiers::default(),
        style,
        &mut reported,
    );
    let after = shot(&mut chooser, style);
    assert_eq!(chooser.focus(), Focus::Apply);
    assert_eq!(
        reported.rects().len(),
        2,
        "the ring leaves one and takes one"
    );
    assert!(reported.contains(centre(layout.close())));
    assert!(reported.contains(centre(layout.apply())));
    for point in changed_pixels(&before, &after) {
        assert!(
            reported.contains(point),
            "the focus move changed {point:?}, which it did not report"
        );
    }
}
