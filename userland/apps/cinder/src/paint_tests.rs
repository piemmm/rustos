//! Painter tests: what each surface ends up holding, and the one placement
//! relationship the companion depends on.

use super::{companion_feet, companion_origin, pen_client, Painter};
use crate::cinder::Pose;
use crate::layout::{pen as lay_out, COMPANION_SIDE, PEN_HEIGHT, PEN_WIDTH};
use crate::pen::Pen;
use crate::project::Ground;
use tairix_geometry::{Rect, Scale};
use tairix_raster::{Pixel, Surface};
use tairix_theme::{Theme, ThemeRegistry};

/// Lay the pen out at the reference density under the active theme.
fn lay(w: u32, h: u32) -> crate::layout::PenLayout {
    lay_out(Rect::new(0, 0, w, h), scale(), &theme())
}

fn scale() -> Scale {
    Scale::from_percent(100).expect("the reference density")
}

fn theme() -> Theme {
    ThemeRegistry::with_builtins().active().clone()
}

fn blank(w: u32, h: u32) -> Surface {
    Surface::filled(w, h, Pixel::TRANSPARENT).expect("a test surface allocates")
}

fn covered(surface: &Surface) -> usize {
    surface.pixels().iter().filter(|p| p.a > 0).count()
}

#[test]
fn the_companion_surface_is_transparent_everywhere_the_fur_is_not() {
    let mut painter = Painter::new();
    let mut surface = blank(COMPANION_SIDE, COMPANION_SIDE);
    painter.draw_companion(
        &mut surface,
        &Pose {
            at: Ground::new(0.0, 0.0),
            ..Pose::default()
        },
        COMPANION_SIDE,
    );
    assert!(covered(&surface) > 0, "the creature must actually be drawn");
    assert!(
        covered(&surface) < (COMPANION_SIDE * COMPANION_SIDE) as usize,
        "a companion that filled its surface would be a rectangle on the desktop"
    );
    // The very corner is margin: this is what the desktop's shaped hit test
    // relies on to let a click through.
    assert_eq!(surface.get(0, 0).map(|p| p.a), Some(0));
}

#[test]
fn the_companion_is_drawn_at_the_surfaces_own_feet_not_at_world_coordinates() {
    let mut painter = Painter::new();
    let mut surface = blank(COMPANION_SIDE, COMPANION_SIDE);
    // A pose far off the desktop must still draw inside its own surface.
    painter.draw_companion(
        &mut surface,
        &Pose {
            at: Ground::new(4_000.0, 3_000.0),
            ..Pose::default()
        },
        COMPANION_SIDE,
    );
    assert!(covered(&surface) > 0);
}

#[test]
fn drawing_twice_leaves_no_trace_of_the_first_pose() {
    let mut painter = Painter::new();
    let mut surface = blank(COMPANION_SIDE, COMPANION_SIDE);
    painter.draw_companion(&mut surface, &Pose::default(), COMPANION_SIDE);
    let first = covered(&surface);
    painter.draw_companion(&mut surface, &Pose::default(), COMPANION_SIDE);
    assert_eq!(
        covered(&surface),
        first,
        "the surface is cleared each frame, so a moving creature leaves no smear"
    );
}

#[test]
fn placing_the_surface_and_drawing_into_it_agree_about_where_his_feet_are() {
    // If these two disagreed he would drift from where the desktop thinks he
    // is, and the shaped hit test would stop matching the pixels.
    let at = Ground::new(640.0, 480.0);
    let origin = companion_origin(at, COMPANION_SIDE);
    let feet = companion_feet(COMPANION_SIDE);
    let drawn_at_x = f64::from(origin.x) + feet.x;
    let drawn_at_y = f64::from(origin.y) + feet.y;
    assert!((drawn_at_x - at.x).abs() <= 1.0);
    assert!((drawn_at_y - at.y).abs() <= 1.0);
}

#[test]
fn the_pen_fills_its_whole_client() {
    let mut painter = Painter::new();
    let (w, h) = pen_client(scale());
    let mut surface = blank(w, h);
    let layout = lay(w, h);
    let mut pen = Pen::new();
    pen.settle(&layout);
    painter.draw_pen(
        &mut surface,
        &layout,
        &pen,
        &pose_in(&layout),
        &theme(),
        scale(),
    );
    assert_eq!(
        covered(&surface),
        (w * h) as usize,
        "a window must not show through: the pen is a room, not an overlay"
    );
}

#[test]
fn the_pen_shows_an_empty_bed_while_he_is_out() {
    let mut painter = Painter::new();
    let (w, h) = pen_client(scale());
    let layout = lay(w, h);
    let mut pen = Pen::new();
    pen.settle(&layout);

    let mut home = blank(w, h);
    painter.draw_pen(
        &mut home,
        &layout,
        &pen,
        &pose_in(&layout),
        &theme(),
        scale(),
    );

    assert!(pen.let_out(None));
    let mut away = blank(w, h);
    painter.draw_pen(
        &mut away,
        &layout,
        &pen,
        &pose_in(&layout),
        &theme(),
        scale(),
    );

    assert_ne!(
        home.pixels(),
        away.pixels(),
        "the pen must look different with nobody in it"
    );
}

#[test]
fn the_default_pen_client_matches_the_authored_size() {
    assert_eq!(pen_client(scale()), (PEN_WIDTH, PEN_HEIGHT));
}

fn pose_in(layout: &crate::layout::PenLayout) -> Pose {
    Pose {
        at: Ground::new(
            f64::from(layout.floor.left()) + f64::from(layout.floor.width) / 2.0,
            f64::from(layout.floor.bottom()) - 10.0,
        ),
        ..Pose::default()
    }
}

#[test]
fn the_buttons_label_actually_reaches_the_surface() {
    // The defect this closes, three times over: the plate drew, the glyphs did
    // not, and every gate stayed green because nothing asserted that a
    // control's *label* lands in pixels. The strip was shorter than a line of
    // the theme's body text, and the control silently dropped its content.
    let mut painter = Painter::new();
    let (w, h) = pen_client(scale());
    let mut surface = blank(w, h);
    let layout = lay(w, h);
    let mut pen = Pen::new();
    pen.settle(&layout);
    let theme = theme();
    painter.draw_pen(
        &mut surface,
        &layout,
        &pen,
        &pose_in(&layout),
        &theme,
        scale(),
    );

    let font =
        tairix_font::BitmapFont::for_role(theme.fonts(), tairix_theme::TextRole::Body, scale());
    let ink = font.text_width(crate::pen::LET_OUT_LABEL);
    assert!(ink > 0, "the label must measure to something");

    // The plate is one flat colour, so any pixel inside it that is *not* the
    // plate's dominant colour is content the label put there.
    let mut counts = alloc::collections::BTreeMap::new();
    for y in layout.button.top()..layout.button.bottom() {
        for x in layout.button.left()..layout.button.right() {
            let (Ok(ux), Ok(uy)) = (u32::try_from(x), u32::try_from(y)) else {
                continue;
            };
            if let Some(pixel) = surface.get(ux, uy) {
                *counts.entry((pixel.r, pixel.g, pixel.b)).or_insert(0_usize) += 1;
            }
        }
    }
    let total: usize = counts.values().sum();
    let plate = counts.values().copied().max().unwrap_or(0);
    let content = total - plate;
    // A line of type this wide covers far more than the plate's four rounded
    // corners, which is all the old behaviour left behind.
    let least = usize::try_from(ink).unwrap_or(0);
    assert!(
        content >= least,
        "the button's label put {content} pixels on a {total}-pixel plate; \
         a {ink}-pixel-wide line of type must put at least {least}"
    );
}

#[test]
fn the_label_survives_every_ui_scale() {
    // A strip sized from the control holds at every density; one sized by a
    // constant only held at the density it was picked at.
    for percent in [50, 100, 150, 200, 250] {
        let Some(scale) = Scale::from_percent(percent) else {
            continue;
        };
        let theme = theme();
        let (w, h) = (
            scale.scale_length(PEN_WIDTH),
            scale.scale_length(PEN_HEIGHT),
        );
        let layout = lay_out(Rect::new(0, 0, w, h), scale, &theme);
        let font =
            tairix_font::BitmapFont::for_role(theme.fonts(), tairix_theme::TextRole::Body, scale);
        assert!(
            layout.button.height >= font.glyph_height(),
            "at {percent}% the plate is {} tall and a line of type is {}",
            layout.button.height,
            font.glyph_height()
        );
    }
}
