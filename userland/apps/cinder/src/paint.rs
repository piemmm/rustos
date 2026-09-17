//! The two painters: the playpen window, and the companion surface.
//!
//! Both draw the same creature through the same projection and the same
//! shapes — the only difference is what is behind him. The pen has a floor, a
//! wall, and furniture; the companion has nothing at all, because the desktop
//! shows through everywhere the creature is not.

use tairix_geometry::{Point, Rect, Scale};
use tairix_raster::{Color, Surface};
use tairix_theme::Theme;

use crate::cinder::{palette, parts, Pose, BODY_RADIUS};
use crate::fur::{self, falloff_table, Blob, FALLOFF_STEPS};
use crate::layout::{PenLayout, COMPANION_FEET};
use crate::pen::{Pen, Whereabouts};
use crate::project::contact_shadow;
use crate::shape::{self, Scratch, Shape};

/// The shared falloff table and anything else a paint needs more than once.
///
/// Built once and held by the caller rather than rebuilt per frame: the table
/// is sixty-four bytes and costs a square root per entry, which is nothing
/// once and waste per frame. The outline buffers are here for the same reason
/// — a creature's worth of shapes costs no allocation at all once the painter
/// has drawn its first frame.
pub struct Painter {
    falloff: [u8; FALLOFF_STEPS],
    scratch: Scratch,
}

impl Default for Painter {
    fn default() -> Self {
        Self::new()
    }
}

impl Painter {
    /// A painter with its falloff table built.
    #[must_use]
    pub fn new() -> Self {
        Self {
            falloff: falloff_table(),
            scratch: Scratch::new(),
        }
    }

    /// Draw Cinder at `pose` onto `surface`, shadow first.
    ///
    /// The shadow goes down before the body so the creature stands on it
    /// rather than in front of it, and it is drawn from the same pose, so a
    /// jump lifts the body and fades the shadow together.
    pub fn draw_cinder(&mut self, surface: &mut Surface, pose: &Pose) {
        let (rx, ry, alpha) = contact_shadow(BODY_RADIUS, pose.lift);
        fur::shadow(surface, pose.at.x, pose.at.y, rx, ry, alpha);
        for part in parts(pose) {
            // Fur is the one shape with no outline: it is a soft splat, so it
            // takes the falloff path rather than the scan converter.
            if let Shape::Fur { radius } = part.shape {
                fur::splat(
                    surface,
                    &Blob {
                        x: part.x,
                        y: part.y,
                        radius,
                        color: part.color,
                        seed: part.seed,
                    },
                    &self.falloff,
                );
            } else {
                shape::fill(surface, &part, &mut self.scratch);
            }
        }
    }

    /// Draw the whole playpen: the wall, the floor, the furniture, and
    /// whoever is home.
    pub fn draw_pen(
        &mut self,
        surface: &mut Surface,
        layout: &PenLayout,
        pen: &Pen,
        pose: &Pose,
        theme: &Theme,
        scale: Scale,
    ) {
        let palette_ref = theme.palette();
        // The pen is a lit room: a wall a shade above the window body, a floor
        // a shade below it, so the two read as surfaces rather than as bands.
        fill(surface, layout.wall, Color::from(palette_ref.surface));
        fill(
            surface,
            layout.floor,
            Color::from(palette_ref.surface_raised),
        );
        Self::draw_furniture(surface, layout);
        if pen.whereabouts() == Whereabouts::Inside {
            self.draw_cinder(surface, pose);
        } else {
            Self::draw_empty_bed(surface, layout);
        }
        // The strip is chrome, so it is drawn over the room rather than in it.
        fill(surface, layout.strip, Color::from(palette_ref.surface));
        pen.button().render(surface, layout.button, scale, theme);
    }

    /// Draw the bed, the bowl, and the toy where it has rolled to.
    fn draw_furniture(surface: &mut Surface, layout: &PenLayout) {
        // A cushion with a darker rim and a lighter inside, rather than a flat
        // rounded rectangle: at this size a plain one reads as an unlabelled
        // button, which is the last thing the floor should look like.
        round(surface, layout.bed, palette::FUR_DEEP);
        round(surface, inset(layout.bed, 3), palette::FUR_MID);
        round(surface, layout.bowl, palette::SLATE_SHADE);
        round(surface, inset(layout.bowl, 3), palette::SLATE_DEEP);
        // The toy is a single bright blob rather than a rectangle: it is the
        // one thing in the pen that moves, and a disc reads as a ball.
        let toy = Rect::new(
            layout.toy.left(),
            layout.toy.top(),
            layout.toy.width,
            layout.toy.height,
        );
        round(surface, toy, palette::FUR_BRIGHT);
    }

    /// Draw the bed with nobody in it, which is what the pen shows while
    /// Cinder is out.
    fn draw_empty_bed(surface: &mut Surface, layout: &PenLayout) {
        // A dent in the bedding: the pen must not look broken while he is
        // away, it must look like he has just got up.
        let dent = Rect::new(
            layout.bed.left() + i32::try_from(layout.bed.width / 4).unwrap_or(0),
            layout.bed.top() + i32::try_from(layout.bed.height / 4).unwrap_or(0),
            layout.bed.width / 2,
            layout.bed.height / 2,
        );
        round(surface, dent, palette::SLATE_DEEP);
    }

    /// Draw Cinder onto a transparent companion surface of `side` pixels,
    /// clearing it first.
    ///
    /// The pose's ground point is replaced with the surface's own feet
    /// position, because a companion surface is *placed* at the creature's
    /// screen position — the pose's world coordinates would draw him off the
    /// edge of his own surface.
    pub fn draw_companion(&mut self, surface: &mut Surface, pose: &Pose, side: u32) {
        surface.fill(Color {
            r: 0,
            g: 0,
            b: 0,
            a: 0,
        });
        let local = Pose {
            at: companion_feet(side),
            ..*pose
        };
        self.draw_cinder(surface, &local);
    }
}

/// Where the creature's feet sit inside a companion surface of `side` pixels.
///
/// One definition, because the painter draws him there and the run loop
/// subtracts the same offset to place the surface — if the two disagreed he
/// would drift from where the desktop thinks he is.
#[must_use]
pub fn companion_feet(side: u32) -> crate::project::Ground {
    crate::project::Ground::new(
        f64::from(side) * COMPANION_FEET.0,
        f64::from(side) * COMPANION_FEET.1,
    )
}

/// The screen origin a companion surface of `side` must sit at for the
/// creature's feet to land on ground point `(x, y)`.
#[must_use]
pub fn companion_origin(at: crate::project::Ground, side: u32) -> Point {
    let feet = companion_feet(side);
    Point::new(
        tairix_util::mathf::round_i32(at.x - feet.x),
        tairix_util::mathf::round_i32(at.y - feet.y),
    )
}

/// The pen's client rectangle at `scale`, in physical pixels.
#[must_use]
pub fn pen_client(scale: Scale) -> (u32, u32) {
    (
        scale.scale_length(crate::layout::PEN_WIDTH),
        scale.scale_length(crate::layout::PEN_HEIGHT),
    )
}

/// Fill `rect` with `color`, clipped to the surface.
fn fill(surface: &mut Surface, rect: Rect, color: Color) {
    let (Ok(x), Ok(y)) = (u32::try_from(rect.left()), u32::try_from(rect.top())) else {
        return;
    };
    surface.fill_rect(x, y, rect.width, rect.height, color);
}

/// `rect` pulled in by `by` pixels on every side, or an empty rectangle when
/// there is not that much of it.
fn inset(rect: Rect, by: u32) -> Rect {
    let shrink = by.saturating_mul(2);
    if rect.width <= shrink || rect.height <= shrink {
        return Rect::new(rect.left(), rect.top(), 0, 0);
    }
    Rect::new(
        rect.left().saturating_add(i32::try_from(by).unwrap_or(0)),
        rect.top().saturating_add(i32::try_from(by).unwrap_or(0)),
        rect.width - shrink,
        rect.height - shrink,
    )
}

/// Fill `rect` as a fully rounded shape — an oval, near enough, at the sizes
/// the furniture is drawn at.
fn round(surface: &mut Surface, rect: Rect, color: Color) {
    if rect.width == 0 || rect.height == 0 {
        return;
    }
    let (Ok(x), Ok(y)) = (u32::try_from(rect.left()), u32::try_from(rect.top())) else {
        return;
    };
    let radius = rect.width.min(rect.height) / 2;
    surface.fill_round_rect(x, y, rect.width, rect.height, radius, color);
}

#[cfg(test)]
#[path = "paint_tests.rs"]
mod tests;
