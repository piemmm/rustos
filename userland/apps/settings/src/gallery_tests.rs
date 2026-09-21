//! Unit tests for the Wallpaper pane's gallery.
//!
//! What the gallery exists to get right: the candidate list it builds from
//! a catalog it did not read, the one picture it asks for at a time, the
//! placeholder a picture that never arrived leaves, a refusal that is not
//! asked for again, and a press released away from its tile choosing
//! nothing.

use alloc::vec;

use tairix_geometry::Point;
use tairix_wallpaper::{Backdrop, Rgb, WallpaperFit};

use super::*;

use crate::test_support::{damage, theme};

/// The band a gallery is laid out in for these tests.
const BAND: Rect = Rect::new(0, 0, 400, 300);

fn catalog(files: &[&str]) -> Vec<CatalogItem> {
    files
        .iter()
        .map(|file| CatalogItem {
            category: String::from("TAIRiX"),
            file: String::from(*file),
        })
        .collect()
}

/// Settings with no picture in effect, so the candidate list is exactly
/// the `no picture` entry plus the catalog. The documented *default* is
/// the shipped master, which a test catalog that does not hold it would
/// otherwise have appended as a candidate of its own.
fn plain() -> DesktopSettings {
    DesktopSettings {
        wallpaper: WallpaperChoice::None,
        ..DesktopSettings::default()
    }
}

/// Press and release the primary button at `at`, answering the outcome of
/// the release.
fn click(gallery: &mut Gallery, at: Point, theme: &Theme) -> GalleryOutcome {
    let mut sink = damage();
    let mut last = GalleryOutcome::Idle;
    for event in [
        InputEvent::PointerMoved { to: at },
        InputEvent::PointerPressed {
            button: PointerButton::Primary,
        },
        InputEvent::PointerReleased {
            button: PointerButton::Primary,
        },
    ] {
        last = gallery.on_pointer(&event, BAND, 0, Scale::ONE, theme, &mut sink);
    }
    last
}

/// Where tile `index` is drawn.
fn tile_centre(gallery: &Gallery, index: usize, theme: &Theme) -> Point {
    let rect = gallery
        .grid(BAND, Scale::ONE, theme)
        .cell_rect(0, index)
        .expect("the tile is seated");
    Point::new(
        rect.left() + tairix_geometry::to_i32(rect.width / 2),
        rect.top() + tairix_geometry::to_i32(rect.height / 2),
    )
}

#[test]
fn the_no_picture_entry_always_leads_and_a_bare_backdrop_selects_it() {
    let gallery = Gallery::new(&catalog(&["a.png", "b.png"]), &plain());
    assert_eq!(gallery.len(), 3, "the catalog's two plus `no picture`");
    assert_eq!(gallery.selected(), 0);
}

#[test]
fn the_picture_in_effect_is_the_one_selected() {
    let settings = DesktopSettings {
        wallpaper: WallpaperChoice::Image(
            WallpaperPath::new("/System/Graphics/Wallpapers/TAIRiX/b.png").expect("a valid path"),
        ),
        ..DesktopSettings::default()
    };
    let gallery = Gallery::new(&catalog(&["a.png", "b.png"]), &settings);
    assert_eq!(gallery.selected(), 2, "the second shipped picture");
}

/// The gallery must never hide the choice that is actually in force, even
/// when the store no longer offers it.
#[test]
fn a_picture_the_catalog_does_not_hold_is_still_offered_and_never_asked_for() {
    let theme = theme();
    let settings = DesktopSettings {
        wallpaper: WallpaperChoice::Image(
            WallpaperPath::new("/Users/me/Documents/holiday.png").expect("a valid path"),
        ),
        ..DesktopSettings::default()
    };
    let gallery = Gallery::new(&catalog(&["a.png"]), &settings);
    assert_eq!(gallery.len(), 3);
    assert_eq!(gallery.selected(), 2);
    // Only the one catalog entry is ever asked for: a render names a
    // catalog position, and this picture has none.
    assert_eq!(
        gallery
            .next_wanted(BAND, Scale::ONE, &theme)
            .map(|wanted| wanted.index),
        Some(0)
    );
}

#[test]
fn one_picture_is_asked_for_at_a_time_and_an_answer_moves_on() {
    let theme = theme();
    let mut gallery = Gallery::new(&catalog(&["a.png", "b.png"]), &plain());
    let first = gallery
        .next_wanted(BAND, Scale::ONE, &theme)
        .expect("a picture is wanted");
    assert_eq!(first.index, 0);
    assert!(first.side > 0);
    assert_eq!(
        gallery.next_wanted(BAND, Scale::ONE, &theme),
        Some(first),
        "the same picture is wanted until it is answered"
    );

    let side = usize::from(first.side);
    assert!(gallery.set_picture(first.index, first.side, &vec![0u8; side * side * 4]));
    assert_eq!(
        gallery
            .next_wanted(BAND, Scale::ONE, &theme)
            .map(|wanted| wanted.index),
        Some(1)
    );
}

/// A refusal is remembered: a picture the desktop could not render is not
/// asked for again, which is what stops the retry loop.
#[test]
fn a_refused_picture_is_never_asked_for_again() {
    let theme = theme();
    let mut gallery = Gallery::new(&catalog(&["a.png", "b.png"]), &plain());
    assert!(gallery.mark_refused(0));
    assert_eq!(
        gallery
            .next_wanted(BAND, Scale::ONE, &theme)
            .map(|wanted| wanted.index),
        Some(1)
    );
    assert!(gallery.mark_refused(1));
    assert_eq!(gallery.next_wanted(BAND, Scale::ONE, &theme), None);
    gallery.invalidate_pictures();
    assert_eq!(
        gallery.next_wanted(BAND, Scale::ONE, &theme),
        None,
        "a scale change must not retry a picture the desktop refused"
    );
}

#[test]
fn a_pixel_run_that_is_not_the_square_it_claims_is_refused_rather_than_drawn() {
    let theme = theme();
    let mut gallery = Gallery::new(&catalog(&["a.png"]), &plain());
    assert!(gallery.set_picture(0, 8, &[0u8; 7]));
    assert_eq!(
        gallery.next_wanted(BAND, Scale::ONE, &theme),
        None,
        "a malformed answer must not leave the tile asking for ever"
    );
}

#[test]
fn an_answer_for_a_position_the_gallery_does_not_hold_is_dropped() {
    let mut gallery = Gallery::new(&catalog(&["a.png"]), &plain());
    assert!(!gallery.set_picture(9, 8, &vec![0u8; 8 * 8 * 4]));
    assert!(!gallery.mark_refused(9));
}

/// A tile that never got its picture still draws: the built-in glyph and
/// the name, never a blank.
#[test]
fn a_picture_that_never_arrived_draws_its_placeholder() {
    let theme = theme();
    let gallery = Gallery::new(&catalog(&["a.png"]), &plain());
    let mut surface = Surface::filled(
        BAND.width,
        BAND.height,
        Color::rgba(0, 0, 0, 255).premultiply(),
    )
    .expect("a test surface");
    gallery.render(&mut surface, BAND, 0, Scale::ONE, &theme);
    let rect = gallery
        .grid(BAND, Scale::ONE, &theme)
        .cell_rect(0, 1)
        .expect("the tile is seated");
    assert!(
        has_ink(&surface, rect),
        "a pending tile drew nothing at all"
    );
}

/// Whether anything but the fill colour was drawn inside `rect`.
fn has_ink(surface: &Surface, rect: Rect) -> bool {
    let base = Color::rgba(0, 0, 0, 255).premultiply();
    (0..rect.height).any(|dy| {
        (0..rect.width).any(|dx| {
            let x = u32::try_from(rect.left()).unwrap_or(0) + dx;
            let y = u32::try_from(rect.top()).unwrap_or(0) + dy;
            surface.get(x, y).is_some_and(|pixel| pixel != base)
        })
    })
}

#[test]
fn choosing_a_tile_reports_the_settings_that_choice_means() {
    let theme = theme();
    let mut gallery = Gallery::new(&catalog(&["a.png", "b.png"]), &plain());
    let at = tile_centre(&gallery, 2, &theme);
    let GalleryOutcome::Chose(settings) = click(&mut gallery, at, &theme) else {
        panic!("choosing a tile reported no choice");
    };
    assert_eq!(
        settings.wallpaper,
        WallpaperChoice::Image(
            WallpaperPath::new("/System/Graphics/Wallpapers/TAIRiX/b.png").expect("a valid path")
        )
    );
    assert_eq!(gallery.selected(), 2);
    // The rest of the pinboard document is untouched: the gallery reports
    // the picture, never the fit or the backdrop.
    assert_eq!(settings.fit, DesktopSettings::default().fit);
}

#[test]
fn a_press_released_away_from_its_tile_chooses_nothing() {
    let theme = theme();
    let mut gallery = Gallery::new(&catalog(&["a.png", "b.png"]), &plain());
    let mut sink = damage();
    let on = tile_centre(&gallery, 2, &theme);
    gallery.on_pointer(
        &InputEvent::PointerMoved { to: on },
        BAND,
        0,
        Scale::ONE,
        &theme,
        &mut sink,
    );
    gallery.on_pointer(
        &InputEvent::PointerPressed {
            button: PointerButton::Primary,
        },
        BAND,
        0,
        Scale::ONE,
        &theme,
        &mut sink,
    );
    let elsewhere = tile_centre(&gallery, 0, &theme);
    gallery.on_pointer(
        &InputEvent::PointerMoved { to: elsewhere },
        BAND,
        0,
        Scale::ONE,
        &theme,
        &mut sink,
    );
    let acted = gallery.on_pointer(
        &InputEvent::PointerReleased {
            button: PointerButton::Primary,
        },
        BAND,
        0,
        Scale::ONE,
        &theme,
        &mut sink,
    );
    assert!(
        !matches!(acted, GalleryOutcome::Chose(_)),
        "a press released elsewhere chose a picture"
    );
    assert_eq!(gallery.selected(), 0, "the selection moved anyway");
}

/// The swatch is the colour the backdrop is actually set to, so the "no
/// picture" tile shows what turning the picture off would look like.
#[test]
fn the_no_picture_tile_shows_the_backdrop_colour_in_effect() {
    let theme = theme();
    let settings = DesktopSettings {
        backdrop: Backdrop::Colour(Rgb::new(0xff, 0x00, 0x00)),
        ..plain()
    };
    let gallery = Gallery::new(&[], &settings);
    let swatch = gallery
        .backdrop_swatch(BAND, Scale::ONE, &theme)
        .expect("a swatch");
    assert_eq!(
        swatch.get(0, 0),
        Some(Color::rgb(0xff, 0x00, 0x00).premultiply())
    );
}

/// An apply the session refused puts the selection back, so the pane never
/// shows a picture the next login would not restore.
#[test]
fn adopting_the_store_puts_the_selection_back() {
    let theme = theme();
    let mut gallery = Gallery::new(&catalog(&["a.png", "b.png"]), &plain());
    let at = tile_centre(&gallery, 2, &theme);
    assert!(matches!(
        click(&mut gallery, at, &theme),
        GalleryOutcome::Chose(_)
    ));
    assert_eq!(gallery.selected(), 2);

    let held = DesktopSettings {
        wallpaper: WallpaperChoice::Image(
            WallpaperPath::new("/System/Graphics/Wallpapers/TAIRiX/a.png").expect("a valid path"),
        ),
        ..DesktopSettings::default()
    };
    gallery.adopt(&held);
    assert_eq!(
        gallery.selected(),
        1,
        "the refused choice was left standing"
    );
}

/// The fit the desktop holds is not the gallery's to change: a chosen tile
/// reports the picture and nothing else.
#[test]
fn choosing_a_picture_leaves_every_other_pinboard_value_alone() {
    let theme = theme();
    let settings = DesktopSettings {
        fit: WallpaperFit::Tile,
        backdrop: Backdrop::Colour(Rgb::new(0x11, 0x22, 0x33)),
        ..plain()
    };
    let mut gallery = Gallery::new(&catalog(&["a.png"]), &settings);
    let at = tile_centre(&gallery, 0, &theme);
    let GalleryOutcome::Chose(chosen) = click(&mut gallery, at, &theme) else {
        panic!("choosing the `no picture` tile reported no choice");
    };
    assert_eq!(chosen.wallpaper, WallpaperChoice::None);
    assert_eq!(chosen.fit, WallpaperFit::Tile);
    assert_eq!(chosen.backdrop, settings.backdrop);
}
