//! The screensaver: one surface over the whole screen once the desktop has
//! sat idle, gone at the first input.
//!
//! It covers every window, so nothing on screen is legible while it is up.
//! It is not a lock: the first input only takes it down, and reaches nothing
//! else. A lock the idle policy engaged beneath it is what the user meets
//! when they come back.

use tairix_wallpaper::ScreensaverKind;
use tairix_wm::{Color, Compositor, Surface, WindowId};

use crate::switchuser::park_within;

/// How long a slideshow shows one picture.
pub const SLIDE_INTERVAL_NS: u64 = 30_000_000_000;

/// How dark a dimmed screensaver lays black over the backdrop, out of 255:
/// enough that nothing reads as an invitation to click, not so much that the
/// picture is lost.
const DIM_ALPHA: u8 = 176;

/// One screensaver on the screen.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
struct Shown {
    wm: WindowId,
    kind: ScreensaverKind,
    /// The catalog position a slideshow shows next.
    next_slide: usize,
    /// Monotonic nanoseconds of the next slide, or `None` when there is none.
    slide_due_ns: Option<u64>,
}

/// The session's screensaver.
#[derive(Debug, Default)]
pub struct Screensaver {
    shown: Option<Shown>,
}

impl Screensaver {
    /// No screensaver up.
    #[must_use]
    pub const fn new() -> Self {
        Self { shown: None }
    }

    /// Whether the screensaver is up.
    ///
    /// While it is, the embedder drains the seat's next input into nothing
    /// and takes the screensaver down: the gesture that wakes the screen acts
    /// on nothing behind it.
    #[must_use]
    pub const fn is_shown(&self) -> bool {
        self.shown.is_some()
    }

    /// Cover the screen with `kind` at monotonic `now_ns`.
    ///
    /// `ground` is the desktop's own backdrop at the screen's size, which a
    /// dimmed screensaver darkens; without one it is black. `slides` is how
    /// many pictures a slideshow can ask for; with none it stays black.
    /// Answers whether the screen is covered: a surface the heap would not
    /// give covers nothing, and the desktop stays as it was.
    pub fn start(
        &mut self,
        kind: ScreensaverKind,
        ground: Option<Surface>,
        slides: usize,
        compositor: &mut Compositor,
        now_ns: u64,
    ) -> bool {
        if self.shown.is_some() {
            return true;
        }
        let screen = compositor.screen_rect();
        let frame = match (kind, ground) {
            (ScreensaverKind::Dim, Some(mut ground)) => {
                let (width, height) = (ground.width(), ground.height());
                // Composited over the ground: a plain fill would replace it.
                ground.fill_round_rect(0, 0, width, height, 0, Color::rgba(0, 0, 0, DIM_ALPHA));
                Some(ground)
            }
            _ => black(screen.width, screen.height),
        };
        let Some(frame) = frame else {
            return false;
        };
        let wm = compositor.add_window(screen.origin, frame);
        compositor.raise(wm);
        self.shown = Some(Shown {
            wm,
            kind,
            next_slide: 0,
            slide_due_ns: (kind == ScreensaverKind::Slideshow && slides > 0).then_some(now_ns),
        });
        true
    }

    /// Take the screensaver down, answering whether one was up.
    pub fn dismiss(&mut self, compositor: &mut Compositor) -> bool {
        let Some(shown) = self.shown.take() else {
            return false;
        };
        let _ = compositor.remove(shown.wm);
        true
    }

    /// The screensaver's window while it is up, which the lock keeps
    /// directly beneath.
    #[must_use]
    pub fn window(&self) -> Option<WindowId> {
        self.shown.map(|shown| shown.wm)
    }

    /// Raise the screensaver over everything, the lock included, so a window
    /// opened or raised behind it cannot surface over it.
    pub fn keep_topmost(&self, compositor: &mut Compositor) {
        if let Some(shown) = self.shown.as_ref() {
            let _ = compositor.raise(shown.wm);
        }
    }

    /// The catalog position a slideshow wants shown at `now_ns`, if its next
    /// picture is due; the one after it is due a slide interval later.
    pub fn due_slide(&mut self, now_ns: u64, slides: usize) -> Option<usize> {
        let shown = self.shown.as_mut()?;
        let due = shown.slide_due_ns?;
        if now_ns < due || slides == 0 {
            return None;
        }
        let index = shown.next_slide % slides;
        shown.next_slide = (index + 1) % slides;
        shown.slide_due_ns = Some(now_ns.saturating_add(SLIDE_INTERVAL_NS));
        Some(index)
    }

    /// Show a prepared slide, if a slideshow is still up to show it.
    pub fn show_slide(&mut self, frame: Surface, compositor: &mut Compositor) {
        if let Some(shown) = self
            .shown
            .as_ref()
            .filter(|shown| shown.kind == ScreensaverKind::Slideshow)
        {
            let _ = compositor.set_surface(shown.wm, frame);
        }
    }

    /// `park_ns` shortened to a slideshow's next picture, or left as it is.
    #[must_use]
    pub fn park_deadline_ns(&self, now_ns: u64, park_ns: u64) -> u64 {
        park_within(
            park_ns,
            self.shown
                .and_then(|shown| shown.slide_due_ns)
                .map(|due| due.saturating_sub(now_ns)),
        )
    }
}

/// A black surface `width` by `height`, or `None` when the heap will not give
/// one.
fn black(width: u32, height: u32) -> Option<Surface> {
    let mut surface = Surface::new(width, height)?;
    surface.fill(Color::rgb(0, 0, 0));
    Some(surface)
}

#[cfg(test)]
mod tests {
    use tairix_wallpaper::ScreensaverKind;
    use tairix_wm::{Color, Point, Surface};

    use super::{Screensaver, SLIDE_INTERVAL_NS};
    use crate::tests::compositor;

    fn centre(compositor: &tairix_wm::Compositor) -> Point {
        let screen = compositor.screen_rect();
        Point::new(
            screen.left() + i32::try_from(screen.width / 2).expect("small"),
            screen.top() + i32::try_from(screen.height / 2).expect("small"),
        )
    }

    #[test]
    fn a_screensaver_covers_the_screen_until_it_is_dismissed() {
        let mut comp = compositor();
        let behind = comp.add_window(Point::new(0, 0), Surface::new(64, 64).expect("a surface"));
        let mut saver = Screensaver::new();
        assert!(saver.start(ScreensaverKind::Blank, None, 0, &mut comp, 0));
        assert!(saver.is_shown());
        let over = comp.window_at(centre(&comp)).expect("something is on top");
        assert_ne!(over, behind);
        assert_eq!(comp.window_at(Point::new(1, 1)), Some(over), "every pixel");
        assert!(saver.dismiss(&mut comp));
        assert!(!saver.is_shown());
        assert_eq!(comp.window_at(Point::new(1, 1)), Some(behind));
        assert!(!saver.dismiss(&mut comp), "nothing left to dismiss");
    }

    #[test]
    fn a_window_raised_behind_the_screensaver_goes_back_beneath_it() {
        let mut comp = compositor();
        let mut saver = Screensaver::new();
        assert!(saver.start(ScreensaverKind::Blank, None, 0, &mut comp, 0));
        let saver_window = comp.window_at(Point::new(1, 1));
        let late = comp.add_window(Point::new(0, 0), Surface::new(64, 64).expect("a surface"));
        comp.raise(late);
        saver.keep_topmost(&mut comp);
        assert_eq!(comp.window_at(Point::new(1, 1)), saver_window);
    }

    #[test]
    fn a_dimmed_screensaver_is_the_ground_it_was_given() {
        let mut comp = compositor();
        let screen = comp.screen_rect();
        let mut ground = Surface::new(screen.width, screen.height).expect("a ground");
        ground.fill(Color::rgb(200, 200, 200));
        let mut saver = Screensaver::new();
        assert!(saver.start(ScreensaverKind::Dim, Some(ground), 0, &mut comp, 0));
        assert!(saver.is_shown());
        assert_eq!(
            saver.park_deadline_ns(0, u64::MAX),
            u64::MAX,
            "nothing to advance"
        );
    }

    #[test]
    fn a_slideshow_asks_for_each_picture_in_turn_one_interval_apart() {
        let mut comp = compositor();
        let mut saver = Screensaver::new();
        assert!(saver.start(ScreensaverKind::Slideshow, None, 3, &mut comp, 100));
        assert_eq!(saver.park_deadline_ns(0, u64::MAX), 100);
        assert_eq!(
            saver.due_slide(100, 3),
            Some(0),
            "the first is asked for at once"
        );
        assert_eq!(saver.due_slide(100, 3), None);
        assert_eq!(saver.park_deadline_ns(100, u64::MAX), SLIDE_INTERVAL_NS);
        let later = 100 + SLIDE_INTERVAL_NS;
        assert_eq!(saver.due_slide(later, 3), Some(1));
        assert_eq!(saver.due_slide(later + SLIDE_INTERVAL_NS, 3), Some(2));
        assert_eq!(
            saver.due_slide(later + 2 * SLIDE_INTERVAL_NS, 3),
            Some(0),
            "and round"
        );
    }

    #[test]
    fn a_blank_screensaver_or_an_empty_catalog_asks_for_no_picture() {
        let mut comp = compositor();
        let mut blank = Screensaver::new();
        assert!(blank.start(ScreensaverKind::Blank, None, 3, &mut comp, 0));
        assert_eq!(blank.due_slide(u64::MAX, 3), None);
        let mut empty = Screensaver::new();
        assert!(empty.start(ScreensaverKind::Slideshow, None, 0, &mut comp, 0));
        assert_eq!(empty.due_slide(u64::MAX, 0), None);
        assert_eq!(empty.park_deadline_ns(0, u64::MAX), u64::MAX);
    }
}
