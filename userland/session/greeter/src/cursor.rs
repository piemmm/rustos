//! Where the pointer is, and the artwork drawn at it.

use tairix_abi::driver::display::DisplayMode;
use tairix_cursor::{CursorImage, CursorRegistry};
use tairix_geometry::{Point, Scale};
use tairix_theme::CursorKind;

/// The pointer artwork for `scale`: the shared set's arrow, rasterised once.
///
/// The login screen runs before any on-disk cursor set is read, so the
/// built-in set is the whole choice. `None` when the arrow will not
/// rasterise at this scale, which costs a visible pointer and nothing else
/// — the position is still tracked and the screen still hit-tests.
#[must_use]
pub fn pointer_image(scale: Scale) -> Option<CursorImage> {
    CursorRegistry::with_builtin()
        .active_cursor(CursorKind::Arrow)
        .rasterise(scale.percent())
}

/// The pointer position the seat's relative motion accumulates into.
///
/// The seat reports movement and the surface hit-tests a position, so the
/// running total is kept here and held inside the screen: a stream of large
/// deltas can never place the pointer off the frame, and the arithmetic is
/// total for every screen shape, a degenerate one included.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Cursor {
    at: Point,
    width: i32,
    height: i32,
}

impl Cursor {
    /// A pointer in the middle of a screen of `mode`.
    ///
    /// An extent wider than the signed coordinate space is taken at its
    /// widest representable value rather than wrapping to a negative one.
    #[must_use]
    pub fn centred(mode: &DisplayMode) -> Self {
        let width = i32::try_from(mode.width_px).unwrap_or(i32::MAX);
        let height = i32::try_from(mode.height_px).unwrap_or(i32::MAX);
        Self {
            at: Point::new(width / 2, height / 2),
            width,
            height,
        }
    }

    /// Where the pointer is now.
    #[must_use]
    pub const fn at(&self) -> Point {
        self.at
    }

    /// Move by `(dx, dy)` and report where that landed.
    pub fn moved_by(&mut self, dx: i32, dy: i32) -> Point {
        self.at = Point::new(
            on_axis(self.at.x.saturating_add(dx), self.width),
            on_axis(self.at.y.saturating_add(dy), self.height),
        );
        self.at
    }
}

/// Hold `value` inside an axis `extent` pixels long.
///
/// Written so the upper bound can never fall below the lower one: a
/// zero-extent axis clamps to `0` instead of faulting on an empty range.
fn on_axis(value: i32, extent: i32) -> i32 {
    value.clamp(0, extent.saturating_sub(1).max(0))
}

#[cfg(test)]
mod tests {
    use super::{pointer_image, Cursor};
    use tairix_abi::driver::display::{DisplayFormat, DisplayMode};
    use tairix_geometry::{Point, Scale};

    fn mode(width_px: u32, height_px: u32) -> DisplayMode {
        DisplayMode {
            width_px,
            height_px,
            stride_bytes: width_px.saturating_mul(4),
            format: DisplayFormat::Rgba8888,
        }
    }

    #[test]
    fn a_new_pointer_starts_in_the_middle_of_the_screen() {
        let cursor = Cursor::centred(&mode(1920, 1080));
        assert_eq!(cursor.at().x, 960);
        assert_eq!(cursor.at().y, 540);
    }

    #[test]
    fn motion_accumulates() {
        let mut cursor = Cursor::centred(&mode(1920, 1080));
        cursor.moved_by(10, -20);
        let at = cursor.moved_by(5, 5);
        assert_eq!(at.x, 975);
        assert_eq!(at.y, 525);
        assert_eq!(cursor.at(), at);
    }

    #[test]
    fn the_pointer_stops_at_the_screen_edges() {
        let mut cursor = Cursor::centred(&mode(640, 480));
        assert_eq!(cursor.moved_by(-100_000, -100_000), Point::new(0, 0));
        let at = cursor.moved_by(100_000, 100_000);
        assert_eq!(at.x, 639);
        assert_eq!(at.y, 479);
    }

    #[test]
    fn an_extreme_delta_saturates_rather_than_wrapping() {
        let mut cursor = Cursor::centred(&mode(640, 480));
        cursor.moved_by(i32::MAX, i32::MAX);
        assert_eq!(cursor.at().x, 639);
        cursor.moved_by(i32::MIN, i32::MIN);
        assert_eq!(cursor.at().x, 0);
        assert_eq!(cursor.at().y, 0);
    }

    /// A screen of no extent leaves the clamp with an empty range, which a
    /// naive `clamp(0, width - 1)` faults on.
    #[test]
    fn a_screen_with_no_extent_does_not_fault() {
        let mut cursor = Cursor::centred(&mode(0, 0));
        assert_eq!(cursor.at(), Point::new(0, 0));
        assert_eq!(cursor.moved_by(7, 7), Point::new(0, 0));
        assert_eq!(cursor.moved_by(-7, -7), Point::new(0, 0));
    }

    #[test]
    fn a_single_pixel_screen_holds_the_one_position() {
        let mut cursor = Cursor::centred(&mode(1, 1));
        assert_eq!(cursor.moved_by(3, 3), Point::new(0, 0));
    }

    #[test]
    fn an_extent_wider_than_the_coordinate_space_is_taken_at_its_widest() {
        let cursor = Cursor::centred(&mode(u32::MAX, 4));
        assert_eq!(cursor.at().x, i32::MAX / 2);
        assert_eq!(cursor.at().y, 2);
    }

    #[test]
    fn the_arrow_rasterises_and_grows_with_the_scale() {
        let native = pointer_image(Scale::ONE).expect("the built-in arrow renders");
        assert!(native.width() > 0 && native.height() > 0);
        let doubled = pointer_image(Scale::from_percent(200).expect("a permitted scale"))
            .expect("the built-in arrow renders at 2x");
        assert_eq!(doubled.width(), native.width() * 2);
    }
}
