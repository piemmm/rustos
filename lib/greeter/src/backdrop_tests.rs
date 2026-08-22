//! Unit tests for what lies behind the column: the picture reaching the frame
//! exactly as it was authored, and the shadow the text over it carries.

use tairix_geometry::Scale;
use tairix_raster::{Color, Surface};

use crate::layout::Prompt;
use crate::surface::{text_shadow, AuthSurface, Backdrop};
use crate::testkit::{darkest_in, picture, render, render_over, theme, SCREEN};

/// A column clear of everything the surface centres, so what shows there is
/// the backdrop and nothing else.
const EDGE: u32 = 4;

/// A screen-sized picture ramping from black at the top to white at the
/// bottom: the tonal shape any wash over it would show up in.
fn ramp() -> Surface {
    let mut image = Surface::new(SCREEN.width, SCREEN.height).expect("a 1000x600 picture");
    for y in 0..SCREEN.height {
        let level = u8::try_from(y * 255 / (SCREEN.height - 1)).unwrap_or(u8::MAX);
        image.fill_rect(0, y, SCREEN.width, 1, Color::rgb(level, level, level));
    }
    image
}

/// The wallpaper reaches the frame at all.
#[test]
fn the_backdrop_blits_the_wallpaper() {
    let surface = AuthSurface::new("ann");
    let image = picture(Color::rgb(255, 0, 0));

    assert_ne!(
        render(&surface),
        render_over(&surface, &image),
        "the wallpaper never reached the frame"
    );
}

/// The picture is drawn as authored, at the ends of the screen as much as in
/// the middle: darkening it for legibility would land in one or the other, and
/// neither may touch a pixel of it.
#[test]
fn the_picture_reaches_the_frame_verbatim() {
    let surface = AuthSurface::new("ann");
    let image = ramp();

    let frame = render_over(&surface, &image);

    for y in [1, SCREEN.height / 2, SCREEN.height - 2] {
        assert_eq!(
            frame.get(EDGE, y),
            image.get(EDGE, y),
            "row {y} of the picture was shaded"
        );
    }
}

/// Text over a picture inks ground the plain draw would have left showing,
/// which is what makes a pale ink readable on a pale photograph.
///
/// Measured on the whitest picture there is: in the account name's own band
/// nothing the surface draws — the picture, the ink, or the two blended — can
/// be darker than the ink itself, so a pixel that is proves the theme's dark
/// desktop colour landed behind the line.
#[test]
fn text_over_a_bright_picture_inks_ground_the_plain_draw_leaves_showing() {
    let surface = AuthSurface::new("Ann Example");
    let ink = theme().palette().on_surface;

    let frame = render_over(&surface, &picture(Color::rgb(255, 255, 255)));

    let darkest = darkest_in(&frame, Prompt::new(SCREEN, Scale::ONE).name);
    assert!(
        darkest < ink.g,
        "the name's band reached {darkest}, no darker than its own ink"
    );
}

/// The flat backdrop asks for no shadow at all, and a picture always asks for
/// one. Over the flat colour there is nothing a shadow could show against — it
/// is the shadow's own colour — so the screen lock pays for one glyph pass,
/// not two.
#[test]
fn only_a_picture_asks_for_a_shadow() {
    let image = picture(Color::rgb(255, 255, 255));

    assert!(text_shadow(&theme(), Scale::ONE, Backdrop::Desktop).is_none());
    assert!(
        text_shadow(&theme(), Scale::ONE, Backdrop::Wallpaper { image: &image }).is_some(),
        "a picture went unshadowed"
    );
}
