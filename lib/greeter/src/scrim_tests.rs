//! Unit tests for the wallpaper scrim: how much of the theme's own desktop
//! colour a picture needs behind the panel, and that the backdrop lays it on.

use tairix_geometry::{Rect, Scale};
use tairix_raster::{Color, Surface};

use crate::scrim::{scrim_alpha, MAX_SCRIM, MIN_SCRIM, SAMPLES_PER_AXIS};
use crate::surface::{panel_rect, AuthSurface, Backdrop};
use crate::testkit::{render, theme, SCREEN};

/// A screen-sized picture in one flat colour.
fn flat(color: Color) -> Surface {
    let mut image = Surface::new(SCREEN.width, SCREEN.height).expect("a 1000x600 picture");
    image.fill(color);
    image
}

fn panel() -> Rect {
    panel_rect(SCREEN, Scale::ONE)
}

/// The two extremes must not be answered the same: a picture that matches
/// the theme needs almost nothing, one that fights it needs a great deal.
#[test]
fn a_black_and_a_white_wallpaper_get_different_bounded_scrims() {
    let dark = scrim_alpha(&flat(Color::rgb(0, 0, 0)), panel(), &theme());
    let light = scrim_alpha(&flat(Color::rgb(255, 255, 255)), panel(), &theme());

    assert_ne!(dark, light);
    assert!(light > dark, "a white wallpaper needs the heavier scrim");
    for alpha in [dark, light] {
        assert!(
            (MIN_SCRIM..=MAX_SCRIM).contains(&alpha),
            "{alpha} is outside the documented range"
        );
    }
}

/// Every answer stays inside the documented range, whatever the picture: an
/// opaque scrim would hide the wallpaper outright, a bare one would leave
/// text on raw detail.
#[test]
fn every_answer_is_bounded() {
    for level in [0, 32, 64, 96, 128, 160, 192, 224, 255] {
        let alpha = scrim_alpha(&flat(Color::rgb(level, level, level)), panel(), &theme());

        assert!(
            (MIN_SCRIM..=MAX_SCRIM).contains(&alpha),
            "grey {level} answered {alpha}"
        );
    }
}

/// The theme's own desktop colour needs no subduing at all.
#[test]
fn a_wallpaper_matching_the_theme_needs_only_the_resting_minimum() {
    let desktop = Color::from(theme().palette().desktop);

    assert_eq!(scrim_alpha(&flat(desktop), panel(), &theme()), MIN_SCRIM);
}

/// The scrim is sized for the worst patch under the panel, not the average:
/// a mostly-dark picture with a bright band across the panel still needs the
/// heavier scrim, because text sits over the bright band too.
#[test]
fn the_brightest_patch_under_the_panel_sizes_the_scrim() {
    let mut mostly_dark = flat(Color::rgb(0, 0, 0));
    let panel = panel();
    let (x, y) = (
        u32::try_from(panel.origin.x).expect("an on-screen panel"),
        u32::try_from(panel.origin.y).expect("an on-screen panel"),
    );
    mostly_dark.fill_rect(
        x,
        y,
        panel.width,
        panel.height / 4,
        Color::rgb(255, 255, 255),
    );

    let banded = scrim_alpha(&mostly_dark, panel, &theme());
    let dark = scrim_alpha(&flat(Color::rgb(0, 0, 0)), panel, &theme());

    assert!(banded > dark, "the bright band was averaged away");
}

/// Nothing of the picture under the panel means nothing to subdue: what
/// shows there is the desktop colour already.
#[test]
fn a_picture_that_does_not_reach_the_panel_needs_only_the_minimum() {
    let corner = flat(Color::rgb(255, 255, 255));
    let mut cropped = Surface::new(4, 4).expect("a tiny picture");
    cropped.fill(Color::rgb(255, 255, 255));

    assert_eq!(scrim_alpha(&cropped, panel(), &theme()), MIN_SCRIM);
    assert_ne!(scrim_alpha(&corner, panel(), &theme()), MIN_SCRIM);
}

/// The sample grid is fixed, so a huge picture costs no more to weigh than a
/// small one: the same flat colour answers the same at either size.
#[test]
fn the_sampling_is_bounded_and_size_independent() {
    let small = {
        let mut image = Surface::new(SAMPLES_PER_AXIS, SAMPLES_PER_AXIS).expect("a tiny picture");
        image.fill(Color::rgb(200, 200, 200));
        image
    };
    let large = flat(Color::rgb(200, 200, 200));
    let whole_small = Rect::new(0, 0, SAMPLES_PER_AXIS, SAMPLES_PER_AXIS);

    assert_eq!(
        scrim_alpha(&small, whole_small, &theme()),
        scrim_alpha(
            &large,
            Rect::new(0, 0, SCREEN.width, SCREEN.height),
            &theme()
        )
    );
}

/// A picture with no pixels is not a reason to fail: the panel simply sits
/// on the desktop colour.
#[test]
fn an_empty_picture_answers_the_minimum() {
    let empty = Surface::new(0, 0).expect("an empty picture");

    assert_eq!(scrim_alpha(&empty, panel(), &theme()), MIN_SCRIM);
}

/// The wallpaper reaches the frame, and the scrim over it is what decides
/// how much of it shows.
#[test]
fn the_backdrop_blits_the_wallpaper_under_the_scrim() {
    let surface = AuthSurface::new("ann");
    let image = flat(Color::rgb(255, 0, 0));

    let plain = render(&surface);
    let light = surface
        .render(
            SCREEN,
            Scale::ONE,
            &theme(),
            Backdrop::Wallpaper {
                image: &image,
                scrim: MIN_SCRIM,
            },
        )
        .expect("a wallpapered frame");
    let heavy = surface
        .render(
            SCREEN,
            Scale::ONE,
            &theme(),
            Backdrop::Wallpaper {
                image: &image,
                scrim: 255,
            },
        )
        .expect("a wallpapered frame");

    assert_ne!(plain, light, "the wallpaper never reached the frame");
    assert_eq!(
        plain, heavy,
        "a fully opaque scrim is not the flat desktop backdrop"
    );
}
