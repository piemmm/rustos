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

/// A screen-sized picture that ramps smoothly from black at the top to white
/// at the bottom — the tonal shape a photographic sky has, and the one a
/// darkening wash can flatten into bands.
fn vertical_ramp() -> Surface {
    let mut image = Surface::new(SCREEN.width, SCREEN.height).expect("a 1000x600 picture");
    for y in 0..SCREEN.height {
        let level = u8::try_from(y * 255 / (SCREEN.height - 1)).unwrap_or(u8::MAX);
        image.fill_rect(0, y, SCREEN.width, 1, Color::rgb(level, level, level));
    }
    image
}

/// The longest run of consecutive rows of `frame` that carry the same tone in
/// the strip `[0, STRIP)` — a band — and how many distinct tones those rows
/// hold, measured over `rows`.
///
/// A row's tone is the sum of its green channels across the strip, so it
/// resolves the row to a fraction of a level rather than to the one level a
/// single pixel can hold. The strip is at the left edge, clear of the centred
/// column and below the chrome band, so it is backdrop and nothing else.
fn backdrop_bands(frame: &Surface, rows: core::ops::Range<u32>) -> (u32, u32) {
    const STRIP: u32 = 64;
    let tone = |y: u32| -> u32 {
        (0..STRIP)
            .filter_map(|x| frame.get(x, y))
            .map(|pixel| u32::from(pixel.g))
            .sum()
    };
    let mut previous = tone(rows.start);
    let (mut longest, mut run, mut tones) = (1, 1, 1);
    for y in rows.skip(1) {
        let here = tone(y);
        if here == previous {
            run += 1;
            longest = longest.max(run);
        } else {
            run = 1;
            tones += 1;
            previous = here;
        }
    }
    (longest, tones)
}

/// Darkening the wallpaper must not band it. This is the whole point of the
/// scrim being a wash rather than a curtain: the picture stays a picture.
///
/// Measured over the middle third, where the scrim is the only thing laid
/// over the picture, at the heaviest scrim the surface will ever ask for. The
/// ramp there crosses about 85 levels in 200 rows, and an undithered scrim of
/// 224 answered them with twelve tones and plateaus 22 rows deep — bands wide
/// enough to read across a room, which is exactly what a 1080-row screen
/// showed.
#[test]
fn the_heaviest_scrim_does_not_band_the_wallpaper_it_darkens() {
    let surface = AuthSurface::new("ann");
    let image = vertical_ramp();

    let frame = surface
        .render(
            SCREEN,
            Scale::ONE,
            &theme(),
            Backdrop::Wallpaper {
                image: &image,
                scrim: MAX_SCRIM,
            },
        )
        .expect("a wallpapered frame");

    let middle = SCREEN.height / 3..SCREEN.height - SCREEN.height / 3;
    let (band, tones) = backdrop_bands(&frame, middle);
    assert!(band <= 8, "the scrim flattened {band} rows into one tone");
    assert!(tones >= 32, "the scrim left only {tones} tones of the ramp");
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
