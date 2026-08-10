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
use tairix_geometry::{Point, Rect};
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

/// `count` catalog entries under the shipped store, in listing order.
fn catalog(count: usize) -> Vec<Candidate> {
    let entries: Vec<CatalogEntry> = (0..count)
        .map(|index| CatalogEntry {
            name: alloc::format!("image-{index:02}.png"),
            bytes: 10,
        })
        .collect();
    candidates_from_catalog(&entries)
}

/// A chooser over three images, opened on the first of them.
fn sample_chooser() -> Chooser {
    Chooser::new(
        catalog(3),
        &settings_selecting("/System/Graphics/Wallpapers/image-00.png"),
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

/// Release the primary button where the pointer already is.
fn release(chooser: &mut Chooser, style: Style<'_>) -> ChooserAction {
    chooser.on_pointer(
        &InputEvent::PointerReleased {
            button: PointerButton::Primary,
        },
        style,
        &mut damage::sink(),
    )
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
    chooser.on_key(Key::Named(named), Modifiers::default(), style)
}

/// The rectangle of the gallery tile at `index`, which must be visible.
fn tile_rect(chooser: &Chooser, index: usize, style: Style<'_>) -> Rect {
    chooser
        .layout(style)
        .grid(chooser.candidates().len())
        .cell_rect(chooser.scroll_offset(), index)
        .expect("the tile is visible")
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
        &settings_selecting("/System/Graphics/Wallpapers/image-01.png"),
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
    chooser.mark_thumbnail_refused(first.index);
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
        chooser.mark_thumbnail_refused(request.index);
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
    chooser.set_thumbnail(request.index, pixels);
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
    chooser.set_thumbnail(index, wrong_side);

    let again = chooser
        .next_thumbnail(style)
        .expect("the wrong-sided thumbnail is asked for again");
    assert_eq!(again.index, index);
    assert_eq!(again.side, request.side);

    let fresh = Surface::new(request.side, request.side).expect("a test surface");
    chooser.set_thumbnail(index, fresh);
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
        chooser.mark_thumbnail_refused(request.index);
        resolved += 1;
        assert!(resolved <= chooser.candidates().len(), "no re-asking");
    }
    assert!(chooser
        .next_thumbnail(style_with_screen(theme, (800, 600)))
        .is_none());
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

    let resting = chooser.render(style).expect("a rendered window");
    let target = tile_rect(&chooser, 2, style);
    assert_eq!(
        move_to(&mut chooser, centre(target), style),
        ChooserAction::Changed
    );
    let hovered = chooser.render(style).expect("a rendered window");
    assert_ne!(resting.pixels(), hovered.pixels());

    // ...and moving away puts it back exactly as it was.
    let _ = move_to(&mut chooser, Point::new(0, 0), style);
    let left = chooser.render(style).expect("a rendered window");
    assert_eq!(resting.pixels(), left.pixels());
}

#[test]
fn pressing_a_tile_draws_it_pressed_before_the_release_decides() {
    let registry = ThemeRegistry::with_builtins();
    let style = style_for(registry.active());
    let mut chooser = sample_chooser();

    let target = tile_rect(&chooser, 2, style);
    let _ = move_to(&mut chooser, centre(target), style);
    let hovered = chooser.render(style).expect("a rendered window");
    let _ = press(&mut chooser, style);
    assert_eq!(chooser.armed(), Some(2));
    let pressed = chooser.render(style).expect("a rendered window");
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

    let resting = chooser.render(style).expect("a rendered window");
    assert_eq!(
        move_to(&mut chooser, centre(layout.apply()), style),
        ChooserAction::Changed
    );
    let hovered = chooser.render(style).expect("a rendered window");
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
fn dragging_the_scrollbar_thumb_scrolls_the_gallery() {
    let registry = ThemeRegistry::with_builtins();
    let style = style_for(registry.active());
    let mut chooser = Chooser::new(catalog(24), &settings_without_a_wallpaper());
    chooser.relayout(MIN_WIN_WIDTH, MIN_WIN_HEIGHT);
    // One paint brings the bar's model in line with the resized gallery,
    // which is what gives it a thumb to grab.
    let _ = chooser.render(style);
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
    chooser.set_preview(first.clone(), pixels);
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
        chooser.set_thumbnail(request.index, pixels);
    }
    let held = chooser.next_preview(style).expect("a preview to render");
    let pixels = Surface::new(held.width, held.height).expect("a test surface");
    chooser.set_preview(held.clone(), pixels);

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
    chooser.mark_preview_refused(request.clone());
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

    let mut seen = vec![chooser.focus()];
    for _ in 1..Focus::ORDER.len() {
        let _ = key(&mut chooser, NamedKey::Tab, style);
        seen.push(chooser.focus());
    }
    assert_eq!(seen, Focus::ORDER.to_vec());
    let _ = key(&mut chooser, NamedKey::Tab, style);
    assert_eq!(chooser.focus(), Focus::Gallery);
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

    let _ = chooser.on_key(Key::Named(NamedKey::Tab), back, style);
    assert_eq!(chooser.focus(), Focus::Apply);
    let _ = chooser.on_key(Key::Named(NamedKey::Tab), back, style);
    assert_eq!(chooser.focus(), Focus::Close);
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
    assert_eq!(chooser.focus(), Focus::Gallery);
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
    let parsed = tairix_wallpaper::settings::parse(&document).expect("a valid document");
    assert_eq!(parsed, chooser.to_settings());
    assert_eq!(
        parsed.wallpaper,
        chooser.candidates()[chooser.selected()].choice
    );
}

#[test]
fn an_apply_outcome_starts_absent_and_reports_exactly_what_was_set() {
    let mut chooser = sample_chooser();
    assert!(chooser.apply_outcome().is_none());
    chooser.set_apply_outcome(ApplyOutcome::Refused(String::from("denied")));
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

    let silent = chooser.render(style).expect("a rendered window");
    let mut rendered = vec![silent.pixels().to_vec()];
    for outcome in [
        ApplyOutcome::Applied,
        ApplyOutcome::Refused(String::from("the session said no")),
        ApplyOutcome::NoDesktop,
    ] {
        chooser.set_apply_outcome(outcome);
        let painted = chooser.render(style).expect("a rendered window");
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
        );
        let regions = [
            layout.preview(),
            layout.caption(),
            layout.heading(),
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
        assert!(layout.tiles().right() <= layout.scrollbar().left());
        assert!(layout.status().right() <= layout.close().left());
        assert!(layout.close().right() <= layout.apply().left());
        assert!(layout.heading().top() >= layout.preview().bottom());
        assert!(layout.tiles().top() >= layout.heading().bottom());
        assert!(layout.apply().top() >= layout.tiles().bottom());
    }
}

#[test]
fn the_chooser_renders_a_window_sized_surface_at_every_size() {
    let registry = ThemeRegistry::with_builtins();
    let style = style_for(registry.active());
    let mut chooser = sample_chooser();
    for (width, height) in [(MIN_WIN_WIDTH, MIN_WIN_HEIGHT), (WIN_WIDTH, WIN_HEIGHT)] {
        chooser.relayout(width, height);
        let painted = chooser.render(style).expect("a rendered window");
        assert_eq!(painted.width(), width);
        assert_eq!(painted.height(), height);
    }
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
    let _ = chooser.render(style);
    let grid = chooser.layout(style).grid(chooser.candidates().len());
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
    chooser.set_preview(first.clone(), pixels);
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
