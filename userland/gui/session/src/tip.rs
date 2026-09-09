//! The seat's one tooltip: what an application declared, when it is shown,
//! where the plate goes, and every reason it comes down (`plans/TOOLTIPS.md`).
//!
//! An application says only two things — *this region of my window* and *this
//! one short line*. It is never told where its window sits on screen, so it
//! could not place a plate truthfully even if it owned one, and it has no
//! pointer position inside the seat to time a dwell against. Everything else
//! is therefore the desktop's:
//!
//! * the **dwell** before a tip appears, armed while the pointer rests inside
//!   a declared region and cleared the moment it leaves;
//! * the **placement**, through the one shared plate rule
//!   [`tairix_controls::plate_rect`] — the same arithmetic a menu plate and
//!   the icon-bar's picker go through, never a second copy;
//! * the **lifetime**: any press, key, or scroll takes it down, as does
//!   leaving the region, the owner withdrawing its declaration, the owner
//!   dying, and any change of scale, theme, or display mode.
//!
//! One tip at a time, for the same reason there is one menu at a time: it is
//! the *seat's* tip, and two would be two answers to one pointer.
//!
//! The dwell is a deadline, not a poll: [`park_deadline_ns`] shortens the
//! session's own park to the moment the tip is due and [`tick`] resolves it,
//! so a resting pointer wakes nothing until then.
//!
//! [`park_deadline_ns`]: SeatTooltip::park_deadline_ns
//! [`tick`]: SeatTooltip::tick

use alloc::collections::BTreeMap;
use alloc::string::String;

use tairix_abi::window_ipc::WindowRegion;
use tairix_controls::{plate_rect, PlatePlacement, PlateSide, Tooltip};
use tairix_geometry::{Point, Rect, Scale};
use tairix_raster::Surface;
use tairix_theme::Theme;

/// How long the pointer must rest inside a declared region before its tooltip
/// appears, in monotonic nanoseconds.
///
/// A deliberate, fixed interaction bound, not a hardware-scaled capacity: it
/// is the pause that separates *asking* what something is from merely
/// travelling across it, and reaching it fails nothing — a pointer that moves
/// on simply never sees a tip. Long enough that sweeping across a toolbar pops
/// nothing up, short enough that a deliberate rest does not feel stuck.
pub const TOOLTIP_DWELL_NS: u64 = 600_000_000;

/// The clearance a tooltip plate keeps from the region it explains, in
/// *logical* pixels at the reference density.
///
/// A plate flush against its region would read as part of it; this is the gap
/// that says the tip is *about* the thing rather than in it.
const TOOLTIP_GAP: u32 = 4;

/// One window's declaration: the region and the line.
#[derive(Clone, Debug, Eq, PartialEq)]
struct Declared {
    region: WindowRegion,
    text: String,
}

/// The dwell in flight, if any: the window whose region the pointer rests in
/// and when its tip is due.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
struct Dwell {
    window: u64,
    due_ns: u64,
}

/// The seat's tooltip: at most one declaration per window, at most one dwell,
/// and at most one tip on screen.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SeatTooltip {
    /// What each window has declared. A window holds at most one, so a second
    /// declaration replaces the first rather than joining it.
    declared: BTreeMap<u64, Declared>,
    dwell: Option<Dwell>,
    /// The window whose tip is on screen, if one is.
    shown: Option<u64>,
}

impl SeatTooltip {
    /// A seat with nothing declared and nothing shown.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record `window`'s declaration, replacing any it had made before.
    ///
    /// Empty `text` withdraws it — one operation, so there is no second
    /// "hide" to fall out of step with — and takes a tip already on screen
    /// down with it. Answers whether anything on screen changed.
    pub fn declare(&mut self, window: u64, region: WindowRegion, text: &str) -> bool {
        if text.is_empty() {
            return self.withdraw(window);
        }
        self.declared.insert(
            window,
            Declared {
                region,
                text: String::from(text),
            },
        );
        // A region that moved under a shown tip no longer explains what the
        // tip is beside, so the tip goes and the dwell starts again.
        self.clear_for(window)
    }

    /// Withdraw `window`'s declaration. Answers whether anything on screen
    /// changed.
    pub fn withdraw(&mut self, window: u64) -> bool {
        self.declared.remove(&window);
        self.clear_for(window)
    }

    /// Forget everything about `window`: its declaration, its dwell, and its
    /// tip. Answers whether anything on screen changed.
    ///
    /// The owner died or the window closed, so nothing it declared can still
    /// be true.
    pub fn forget(&mut self, window: u64) -> bool {
        self.withdraw(window)
    }

    /// Take the tip down and disarm the dwell, whichever window they belong
    /// to. Answers whether anything on screen changed.
    ///
    /// The one answer to every event that ends a tip outright: a press, a key,
    /// a scroll, a scale or theme or mode change. None of them is *about* the
    /// tip, and all of them mean the user has moved on from asking.
    pub fn dismiss(&mut self) -> bool {
        self.dwell = None;
        self.shown.take().is_some()
    }

    /// Note the pointer at screen position `at` at monotonic `now_ns`, with
    /// `client_origin` answering where each window's client area begins.
    ///
    /// Arms the dwell when the pointer is inside a declared region and no
    /// dwell for that window is already running — so the delay is a *rest*
    /// rather than a countdown restarted by every sample of a stationary
    /// hand — and clears both dwell and tip the moment the pointer leaves.
    /// Answers whether anything on screen changed.
    pub fn pointer_moved<F>(&mut self, at: Point, now_ns: u64, client_origin: F) -> bool
    where
        F: Fn(u64) -> Option<Point>,
    {
        let Some(window) = self.window_under(at, &client_origin) else {
            self.dwell = None;
            return self.shown.take().is_some();
        };
        if self.shown == Some(window) {
            // Travelling *within* the region the tip explains changes nothing:
            // the tip already answers this pointer.
            return false;
        }
        let changed = self.shown.take().is_some();
        if self.dwell.map(|dwell| dwell.window) != Some(window) {
            self.dwell = Some(Dwell {
                window,
                due_ns: now_ns.saturating_add(TOOLTIP_DWELL_NS),
            });
        }
        changed
    }

    /// The park this seat needs, shortened to the moment a pending tip is due.
    ///
    /// A seat with no dwell in flight asks for nothing: a resting pointer
    /// wakes no core to find out that it has not moved.
    #[must_use]
    pub fn park_deadline_ns(&self, now_ns: u64, park_ns: u64) -> u64 {
        match self.dwell {
            Some(dwell) => park_ns.min(dwell.due_ns.saturating_sub(now_ns)),
            None => park_ns,
        }
    }

    /// Resolve a dwell that has come due at `now_ns`, answering whether
    /// anything on screen changed.
    ///
    /// The only path that *shows* a tip, so it depends on elapsed time alone
    /// rather than on a hand that happens to jitter. A dwell whose window has
    /// withdrawn its declaration in the meantime shows nothing.
    pub fn tick(&mut self, now_ns: u64) -> bool {
        let Some(dwell) = self.dwell else {
            return false;
        };
        if now_ns < dwell.due_ns {
            return false;
        }
        self.dwell = None;
        if !self.declared.contains_key(&dwell.window) {
            return false;
        }
        self.shown = Some(dwell.window);
        true
    }

    /// The window whose tip is on screen, if one is.
    #[must_use]
    pub const fn shown(&self) -> Option<u64> {
        self.shown
    }

    /// Whether a dwell is in flight.
    #[must_use]
    pub const fn is_dwelling(&self) -> bool {
        self.dwell.is_some()
    }

    /// The text `window` declared, if it holds a declaration.
    #[must_use]
    pub fn text(&self, window: u64) -> Option<&str> {
        self.declared.get(&window).map(|d| d.text.as_str())
    }

    /// Where the shown tip's plate goes on `viewport`, and the tooltip to
    /// draw in it — or `None` when no tip is shown or its owner's client
    /// origin is unknown.
    ///
    /// Placed through the one shared plate rule, so a tip beside a control
    /// flips off a screen edge and slides along it exactly as a menu plate
    /// does. Nothing here re-derives that arithmetic.
    #[must_use]
    pub fn placed<F>(
        &self,
        viewport: Rect,
        scale: Scale,
        theme: &Theme,
        client_origin: F,
    ) -> Option<(Tooltip, Rect)>
    where
        F: Fn(u64) -> Option<Point>,
    {
        let window = self.shown?;
        let declared = self.declared.get(&window)?;
        let anchor = screen_region(declared.region, client_origin(window)?);
        let tip = Tooltip::new(declared.text.clone());
        let (w, h) = tip.preferred_size(scale, theme);
        let rect = plate_rect(
            w,
            h,
            PlatePlacement {
                anchor,
                // Below the region it explains, flipping above at the screen's
                // bottom edge: a tip under the pointer's own arrow is the one
                // place it does not cover what the user is looking at.
                side: PlateSide::Below,
                gap: scale.scale_length(TOOLTIP_GAP),
            },
            viewport,
        );
        Some((tip, rect))
    }

    /// Draw the shown tip into `surface`, which is the plate's own window.
    ///
    /// A no-op when nothing is shown, so a caller need not ask first.
    pub fn render(&self, surface: &mut Surface, bounds: Rect, scale: Scale, theme: &Theme) {
        let Some(window) = self.shown else {
            return;
        };
        let Some(declared) = self.declared.get(&window) else {
            return;
        };
        Tooltip::new(declared.text.clone()).render(surface, bounds, scale, theme);
    }

    /// The window whose declared region contains `at`, if any.
    ///
    /// Front-to-back order is the compositor's, not this map's, so a pointer
    /// inside two windows' regions at once resolves to the lowest window id —
    /// which is why the caller hands in only the origin of the window the seat
    /// says the pointer is actually over.
    fn window_under<F>(&self, at: Point, client_origin: &F) -> Option<u64>
    where
        F: Fn(u64) -> Option<Point>,
    {
        self.declared.iter().find_map(|(&window, declared)| {
            let origin = client_origin(window)?;
            screen_region(declared.region, origin)
                .contains(at)
                .then_some(window)
        })
    }

    /// Clear a dwell or tip belonging to `window`, answering whether anything
    /// on screen changed.
    fn clear_for(&mut self, window: u64) -> bool {
        if self.dwell.map(|dwell| dwell.window) == Some(window) {
            self.dwell = None;
        }
        if self.shown == Some(window) {
            self.shown = None;
            return true;
        }
        false
    }
}

/// A window-local region resolved to screen space against its client
/// `origin`.
///
/// The application never learns this: it states its own client pixels and the
/// seat, which is the only thing that knows where the window sits, resolves
/// them.
fn screen_region(region: WindowRegion, origin: Point) -> Rect {
    Rect::new(
        origin.x.saturating_add(region.x()),
        origin.y.saturating_add(region.y()),
        region.width_px(),
        region.height_px(),
    )
}

#[cfg(test)]
#[path = "tip_tests.rs"]
mod tests;
