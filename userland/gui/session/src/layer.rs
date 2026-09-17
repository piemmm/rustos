//! Desktop layer surfaces: the session's policy for a surface placed on the
//! desktop outside any window of its own.
//!
//! The window engine has already checked the caller's kernel-attested
//! `CAP_DESKTOP_LAYER`, the geometry against the wire bound, and the
//! per-client and per-seat counts. What lives here is everything only the
//! session knows: the live UI scale the extent is re-checked against, the
//! work area the surface is clamped onto, which stacking anchor each depth
//! resolves to, and when both feeds must stop.
//!
//! # Why a layer surface is safe to hand out
//!
//! It is a user-interface spoofing primitive, so it is bounded structurally
//! rather than by trusting its holder, and each control below closes a
//! distinct attack:
//!
//! * **It cannot reproduce a surface the user is meant to trust.** Its sides
//!   are bounded below the narrowest surface the session draws for a trusted
//!   decision ([`LAYER_FITS_UNDER_TRUSTED_SURFACES`] proves that at compile
//!   time against each of them).
//! * **It cannot capture a keystroke.** It never enters the focus rotation,
//!   so even a pixel-perfect lookalike of a password field is typed past.
//! * **It cannot trap clicks.** It catches the pointer only where its own
//!   content is opaque, so its transparent margin is not an invisible
//!   screen-wide button.
//! * **It cannot cover the surfaces the user acts through.** Its highest
//!   depth sits below the icon bar, and a menu or tooltip raises over it.
//! * **It cannot watch a credential being entered.** Both feeds stop while a
//!   trusted surface is up, and the surface itself is hidden.

use alloc::vec::Vec;
use tairix_abi::window_ipc::{
    LayerDepth, TerrainPlate, DESKTOP_LAYER_MAX_PLATES, DESKTOP_LAYER_MAX_SIDE_LOGICAL,
};

use tairix_abi::Errno;
use tairix_geometry::Scale;
use tairix_log::EventId;
use tairix_wm::{Compositor, Point, PointerCatch, Rect, WindowId};

/// That a layer surface is too small to reproduce any surface the session
/// draws for a trusted decision.
///
/// The bound is the defence; this is the proof it still holds. Shrinking a
/// trusted prompt below the bound would let a holder render it at its own
/// size, so that change must fail the build rather than quietly open the
/// hole. The screen-filling surfaces — the lock screen, the greeter, the
/// trusted picker — are larger than any prompt and so are covered a
/// fortiori.
pub const LAYER_FITS_UNDER_TRUSTED_SURFACES: () = {
    assert!(DESKTOP_LAYER_MAX_SIDE_LOGICAL < crate::elevate::WIN_WIDTH);
    assert!(DESKTOP_LAYER_MAX_SIDE_LOGICAL < crate::confirm::WIN_WIDTH);
};

/// Event id of a desktop layer surface opening, in the desktop session's
/// reserved range.
///
/// Opening one is a security decision: the holder gains presence on the
/// desktop outside any window of its own **and** a feed of the pointer's
/// position across the whole screen. The record names that plainly, so an
/// audit of who could watch the pointer does not have to be inferred from
/// the capability grant alone.
pub const LAYER_OPENED: EventId = EventId(20_010);

/// The exact message [`LAYER_OPENED`] is emitted with.
pub const LAYER_OPENED_MESSAGE: &str =
    "desktop layer surface opened; holder receives pointer position";

/// Event id of a refused desktop layer surface.
pub const LAYER_REFUSED: EventId = EventId(20_011);

/// The exact message [`LAYER_REFUSED`] is emitted with.
pub const LAYER_REFUSED_MESSAGE: &str = "desktop layer surface refused";

/// Event id of a desktop layer surface being retired.
pub const LAYER_RETIRED: EventId = EventId(20_012);

/// The exact message [`LAYER_RETIRED`] is emitted with.
pub const LAYER_RETIRED_MESSAGE: &str = "desktop layer surface retired";

/// Event id of the layer feeds starting or stopping as a trusted surface
/// comes and goes.
pub const LAYER_FEEDS: EventId = EventId(20_013);

/// The exact message [`LAYER_FEEDS`] is emitted with when a trusted surface
/// stops the feeds.
pub const LAYER_FEEDS_STOPPED_MESSAGE: &str =
    "desktop layer surface hidden and its feeds stopped for a trusted surface";

/// The exact message [`LAYER_FEEDS`] is emitted with when they resume.
pub const LAYER_FEEDS_RESUMED_MESSAGE: &str = "desktop layer surface feeds resumed";

/// One live desktop layer surface.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct LayerSurface {
    /// The window-channel id the owning application names it by.
    pub ipc: u64,
    /// The compositor window showing it.
    pub wm: WindowId,
    /// Which stacking layer it currently sits in.
    pub depth: LayerDepth,
}

/// The session's layer-surface state: the one live surface, the terrain
/// generation, and the pointer sample not yet delivered.
#[derive(Default)]
pub struct LayerState {
    surface: Option<LayerSurface>,
    /// Bumped whenever the desktop's shape changes, so a holder can tell a
    /// stale pull from a fresh one.
    generation: u64,
    /// Whether the holder has been told about the current generation.
    generation_sent: bool,
    /// The last pointer position delivered, so an unmoved pointer costs
    /// nothing.
    delivered_pointer: Option<Point>,
    /// The pointer position seen since the last frame, awaiting delivery.
    pending_pointer: Option<Point>,
    /// Whether a trusted surface is up, which hides the surface and stops
    /// both feeds.
    suppressed: bool,
    /// The terrain last observed, so a change is detected by comparing the
    /// desktop's actual shape rather than by hooking every mutation that
    /// could alter it — one of which would eventually be missed.
    seen: TerrainSnapshot,
    /// Security decisions awaiting the session's log.
    ///
    /// Bounded, and deliberately: a client that hammers a refused request
    /// must not be able to grow this without limit. Beyond the bound the
    /// *count* still rises, so a flood is visible in the record as a flood
    /// rather than silently dropped.
    decisions: Vec<LayerDecision>,
    /// Decisions dropped because the queue was full, reported with the next
    /// drain so a flood is never invisible.
    decisions_lost: u32,
}

/// How many unlogged layer decisions the session holds before it starts
/// counting them instead.
///
/// A validation bound on a queue an untrusted client drives, not a capacity:
/// a refused request is cheap to issue, so the queue must not be a way to
/// make the session allocate.
const DECISION_QUEUE_MAX: usize = 16;

/// The terrain last observed, held so the next observation can be compared
/// against it exactly.
///
/// Bounded by the reply's own plate bound, so the snapshot costs what one
/// answer costs and never grows with the desktop.
struct TerrainSnapshot {
    plates: [TerrainPlate; DESKTOP_LAYER_MAX_PLATES as usize],
    len: usize,
}

impl Default for TerrainSnapshot {
    fn default() -> Self {
        Self {
            plates: [TerrainPlate {
                x: 0,
                y: 0,
                width_px: 1,
                height_px: 1,
            }; DESKTOP_LAYER_MAX_PLATES as usize],
            len: 0,
        }
    }
}

/// One security decision about a layer surface, awaiting the session's log.
///
/// Recorded here and drained by the session rather than written from the
/// policy, so this module stays host-testable and free of a log sink — the
/// same shape every other session surface's first-showing notice uses.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum LayerDecision {
    /// A surface was opened; its holder now receives pointer position.
    Opened,
    /// A request was refused, with the reason.
    Refused(Errno),
    /// A surface was retired.
    Retired,
}

/// One feed message the session owes the holder after a frame.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum LayerFeed {
    /// The desktop's shape changed; the holder pulls the answer when ready.
    Terrain {
        /// The surface to address.
        ipc: u64,
        /// The generation a pull will answer at or after.
        generation: u64,
    },
    /// Where the pointer is now, in physical screen pixels.
    Pointer {
        /// The surface to address.
        ipc: u64,
        /// Screen position.
        x: i32,
        /// Screen position.
        y: i32,
    },
}

impl LayerState {
    /// The live surface, if one is open.
    #[must_use]
    pub const fn surface(&self) -> Option<LayerSurface> {
        self.surface
    }

    /// The compositor window of the live surface, if one is open.
    #[must_use]
    pub fn wm(&self) -> Option<WindowId> {
        self.surface.map(|surface| surface.wm)
    }

    /// Adopt a newly opened surface.
    pub fn opened(&mut self, surface: LayerSurface) {
        self.surface = Some(surface);
        self.delivered_pointer = None;
        self.pending_pointer = None;
        // A fresh surface has seen no terrain, so it is owed the current
        // generation rather than told nothing until the next window moves.
        self.generation_sent = false;
    }

    /// Forget the surface named by window-channel id `ipc`, answering
    /// whether it was the live one.
    ///
    /// Retiring a layer surface is an ordinary window close, so this is
    /// driven from the same teardown every window takes.
    pub fn closed(&mut self, ipc: u64) -> bool {
        if self.surface.is_some_and(|surface| surface.ipc == ipc) {
            let suppressed = self.suppressed;
            let decisions = core::mem::take(&mut self.decisions);
            let decisions_lost = self.decisions_lost;
            *self = Self {
                suppressed,
                decisions,
                decisions_lost,
                ..Self::default()
            };
            self.note(LayerDecision::Retired);
            return true;
        }
        false
    }

    /// Record the depth the compositor actually stacked the surface at.
    pub fn placed(&mut self, depth: LayerDepth) {
        if let Some(surface) = &mut self.surface {
            surface.depth = depth;
        }
    }

    /// Note that the desktop's shape changed.
    ///
    /// A burst of window movement bumps this many times and costs the holder
    /// one event, because the event carries a generation rather than the
    /// terrain itself.
    pub fn terrain_changed(&mut self) {
        self.generation = self.generation.saturating_add(1);
        self.generation_sent = false;
    }

    /// Compare the desktop's current shape against the last observed one,
    /// bumping the generation when it differs.
    ///
    /// Asked once a frame rather than hooked onto every move, resize, open,
    /// close, raise, and hide that could change it: an exact comparison
    /// against a bounded snapshot cannot miss a change, where a hook on each
    /// mutation site eventually would. It costs one pass over the visible
    /// windows, which is a handful.
    pub fn observe_terrain(&mut self, compositor: &Compositor, wm: WindowId) {
        let mut plates = [TerrainPlate {
            x: 0,
            y: 0,
            width_px: 1,
            height_px: 1,
        }; DESKTOP_LAYER_MAX_PLATES as usize];
        let len = terrain_into(compositor, wm, &mut plates);
        if len == self.seen.len && plates[..len] == self.seen.plates[..len] {
            return;
        }
        self.seen.plates = plates;
        self.seen.len = len;
        self.terrain_changed();
    }

    /// Note where the pointer is, without delivering anything yet.
    ///
    /// Samples arrive far faster than frames, so they are coalesced here and
    /// at most one survives to [`take_feeds`](Self::take_feeds).
    pub fn pointer_moved(&mut self, at: Point) {
        self.pending_pointer = Some(at);
    }

    /// Hide or show the surface and stop or restart both feeds, answering
    /// whether the state changed.
    ///
    /// Suppression is what keeps a holder from watching the pointer over a
    /// password field: while a trusted surface is up the surface is not
    /// composited and nothing at all is delivered. Nothing is *queued*
    /// either — a suppressed sample is dropped, not held back to be replayed
    /// the moment the prompt goes away.
    pub fn set_suppressed(&mut self, suppressed: bool, compositor: &mut Compositor) -> bool {
        if suppressed == self.suppressed {
            return false;
        }
        self.suppressed = suppressed;
        if suppressed {
            self.pending_pointer = None;
            self.delivered_pointer = None;
        }
        if let Some(wm) = self.wm() {
            compositor.set_visible(wm, !suppressed);
        }
        true
    }

    /// Whether a trusted surface is currently suppressing the feeds.
    #[must_use]
    pub const fn is_suppressed(&self) -> bool {
        self.suppressed
    }

    /// Record a security decision for the session to log.
    pub fn note(&mut self, decision: LayerDecision) {
        if self.decisions.len() >= DECISION_QUEUE_MAX {
            self.decisions_lost = self.decisions_lost.saturating_add(1);
            return;
        }
        self.decisions.push(decision);
    }

    /// Hand every recorded decision to `report`, and the number that were
    /// dropped since the last drain, clearing both.
    pub fn report_decisions(
        &mut self,
        mut report: impl FnMut(LayerDecision),
        lost: impl FnOnce(u32),
    ) {
        for decision in self.decisions.drain(..) {
            report(decision);
        }
        let dropped = core::mem::take(&mut self.decisions_lost);
        if dropped != 0 {
            lost(dropped);
        }
    }

    /// Take the feed messages owed to the holder after this frame: at most
    /// one terrain notice and at most one pointer position.
    ///
    /// A pointer that has not moved since the last delivery produces nothing,
    /// so an idle desktop costs no messages at all.
    pub fn take_feeds(&mut self) -> impl Iterator<Item = LayerFeed> {
        let mut feeds = [None, None];
        if let Some(surface) = self.surface.filter(|_| !self.suppressed) {
            if !self.generation_sent {
                self.generation_sent = true;
                feeds[0] = Some(LayerFeed::Terrain {
                    ipc: surface.ipc,
                    generation: self.generation,
                });
            }
            if let Some(at) = self.pending_pointer.take() {
                if self.delivered_pointer != Some(at) {
                    self.delivered_pointer = Some(at);
                    feeds[1] = Some(LayerFeed::Pointer {
                        ipc: surface.ipc,
                        x: at.x,
                        y: at.y,
                    });
                }
            }
        } else {
            self.pending_pointer = None;
        }
        feeds.into_iter().flatten()
    }
}

/// Where a layer surface of `size` physical pixels asking for `at` actually
/// goes: clamped whole onto `work_area`.
///
/// An application never learns the screen's geometry from this — it asks for
/// a point and the session decides — so an off-screen ask is an ordinary
/// request, not an error.
#[must_use]
pub fn clamped_origin(at: (i32, i32), size: (u32, u32), work_area: Rect) -> Point {
    Rect::new(at.0, at.1, size.0, size.1)
        .clamped_onto(work_area)
        .origin
}

/// Whether a surface of `size` physical pixels is within the layer bound at
/// the live `scale`.
///
/// The wire bound is the ceiling at a UI scale of one; this is the same bound
/// in the physical pixels the desktop is actually drawn in, so a surface
/// cannot grow past it by asking on a high-density screen.
#[must_use]
pub fn fits_layer_bound(size: (u32, u32), scale: Scale) -> bool {
    let max = scale.scale_length(DESKTOP_LAYER_MAX_SIDE_LOGICAL);
    size.0 <= max && size.1 <= max
}

/// Stack, shape, and pin a freshly opened layer surface `wm`.
///
/// One place for the four properties that make a layer surface containable,
/// so an open and a later move cannot disagree about any of them.
pub fn apply_participation(
    compositor: &mut Compositor,
    wm: WindowId,
    anchor: Option<WindowId>,
    depth: LayerDepth,
) {
    // Only the pixels it drew catch the pointer, so a transparent margin is
    // not an invisible screen-wide button.
    compositor.set_pointer_catch(wm, PointerCatch::Shape);
    // Never the focused window: a lookalike that cannot be typed into
    // captures nothing.
    compositor.set_focusable(wm, false);
    stack_at_depth(compositor, wm, anchor, depth);
}

/// Put `wm` in the stacking position `depth` names.
///
/// `Above` goes under the icon bar rather than to the front, so a layer
/// surface can never cover the session's own chrome; with no bar on the seat
/// there is nothing to sit under and the front is the honest answer.
pub fn stack_at_depth(
    compositor: &mut Compositor,
    wm: WindowId,
    anchor: Option<WindowId>,
    depth: LayerDepth,
) {
    match depth {
        LayerDepth::Below => {
            compositor.lower(wm);
        }
        LayerDepth::Above => match anchor {
            Some(bar) => {
                compositor.stack_below(wm, bar);
            }
            None => {
                compositor.raise(wm);
            }
        },
    }
}

/// The terrain `wm` sees, written into `out` and answered as the count.
///
/// Rectangles and back-to-front order only: no identity, no title, no owner,
/// no pixels. A desktop with more visible windows than `out` holds is
/// reported truncated to the frontmost, which are the ones a surface can
/// actually meet.
pub fn terrain_into(compositor: &Compositor, wm: WindowId, out: &mut [TerrainPlate]) -> usize {
    if out.is_empty() {
        return 0;
    }
    // A rectangle with no area names nothing a surface could walk on, so it
    // is not terrain and is not counted against the reply either.
    let plates = || {
        compositor
            .terrain(wm)
            .filter(|rect| rect.width != 0 && rect.height != 0)
    };
    // Keep the frontmost when the desktop is deeper than the reply: those are
    // the windows a surface can actually meet.
    let drop_first = plates().count().saturating_sub(out.len());
    let mut written = 0usize;
    for rect in plates().skip(drop_first) {
        out[written] = TerrainPlate {
            x: rect.left(),
            y: rect.top(),
            width_px: rect.width,
            height_px: rect.height,
        };
        written += 1;
    }
    written
}

/// The refusal a layer request gets when the session cannot serve one.
///
/// Named so the open path and the place path answer the same way, and so the
/// reason a surface was refused is a value rather than a scattering of
/// literals.
#[must_use]
pub const fn no_display() -> Errno {
    Errno::NotFound
}

#[cfg(test)]
#[path = "layer_tests.rs"]
mod tests;
