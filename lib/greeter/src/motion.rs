//! The surface's animations: one view giving way to another, a refused
//! attempt shaking, and the veil the screen arrives from and leaves through.
//!
//! Every one of them is a [`Timeline`] started from the theme's duration for
//! its interaction, so a reduced-motion theme collapses each to an immediate
//! state change with no branch here, and none of them keeps a timer armed
//! once it has settled. What a timeline *means* — how far a disc has
//! travelled, how far a column is displaced, how dark the screen is — is
//! recomputed only when the owner steps it, which is what keeps painting a
//! pure function of already-computed state that reads no clock.

use tairix_font::BitmapFont;
use tairix_geometry::{Rect, Scale};
use tairix_raster::{div255, Surface};
use tairix_theme::{Fade, Rgba, TextRole, Theme, Timeline};

/// Horizontal reach of the rejection shake, in logical pixels at the
/// reference density.
const SHAKE_REACH: u32 = 9;

/// Oscillations one rejection shake makes.
const SHAKE_CYCLES: u32 = 3;

/// Phase steps in one whole oscillation.
const PERIOD: u32 = 1 << 12;

/// Nanoseconds in one millisecond.
const NANOS_PER_MS: u64 = 1_000_000;

/// A quarter period of a sine, in 1/255 units, sampled every eighth of the
/// quarter and interpolated between samples.
///
/// There is no floating point in a `no_std` surface and no `sin` in the
/// rasteriser, and a square wave would read as a stutter rather than a shake;
/// nine samples with a linear step between them hold the curve to well under
/// a pixel at the reach drawn here.
const QUARTER_SINE: [i32; 9] = [0, 50, 98, 142, 180, 212, 236, 250, 255];

/// Sine of `phase`, in 1/255 units, over a [`PERIOD`]-step period.
fn sine(phase: u32) -> i32 {
    let half = PERIOD / 2;
    let quarter = PERIOD / 4;
    let span = quarter / 8;
    let phase = phase % PERIOD;
    let (folded, sign) = if phase < half {
        (phase, 1)
    } else {
        (phase - half, -1)
    };
    let mirrored = if folded < quarter {
        folded
    } else {
        half - folded
    };
    let step = usize::try_from(mirrored / span).unwrap_or(8).min(8);
    let from = QUARTER_SINE[step];
    let to = QUARTER_SINE[(step + 1).min(8)];
    let within = i32::try_from(mirrored % span).unwrap_or(0);
    let span = i32::try_from(span).unwrap_or(1).max(1);
    sign * (from + (to - from) * within / span)
}

/// `from` moved `factor/255` of the way to `to`.
fn between(from: u32, to: u32, factor: u8) -> u32 {
    let moved = i64::from(from)
        + (i64::from(to) - i64::from(from)) * i64::from(factor) / i64::from(u8::MAX);
    u32::try_from(moved.max(0)).unwrap_or(from)
}

/// `from` moved `factor/255` of the way to `to`, on the coordinate axis.
fn between_at(from: i32, to: i32, factor: u8) -> i32 {
    let moved = i64::from(from)
        + (i64::from(to) - i64::from(from)) * i64::from(factor) / i64::from(u8::MAX);
    i32::try_from(moved).unwrap_or(from)
}

/// `from` moved `factor/255` of the way to `to`, corner and extent alike, so
/// either end of a travel is that end's own rectangle exactly.
pub(crate) fn between_rects(from: Rect, to: Rect, factor: u8) -> Rect {
    Rect::new(
        between_at(from.origin.x, to.origin.x, factor),
        between_at(from.origin.y, to.origin.y, factor),
        between(from.width, to.width, factor),
        between(from.height, to.height, factor),
    )
}

/// `ink` at `strength` of its own opacity.
///
/// The one way an element is drawn part-way in or part-way out, which every
/// fill and every glyph already honours, so a stage giving way needs no
/// second render of the screen to fade between.
pub(crate) fn at_strength(ink: Rgba, strength: u8) -> Rgba {
    ink.with_alpha(div255(u32::from(ink.a) * u32::from(strength)))
}

/// Take `surface` down to `strength` of its own opacity.
///
/// For artwork already composed of several colours — a tile, a monogram disc
/// — where scaling the colours it was drawn from would fade its parts against
/// each other instead of against the screen.
pub(crate) fn fade(surface: &mut Surface, strength: u8) {
    if strength == u8::MAX {
        return;
    }
    let width = surface.width();
    for row in 0..surface.height() {
        let Some((_, span)) = surface.row_span_mut(row, 0, width) else {
            continue;
        };
        for pixel in span {
            *pixel = pixel.scale_alpha(strength);
        }
    }
}

/// The font a travelling monogram is drawn at, `factor` of the way from the
/// tile's disc to the prompt's larger one.
///
/// The mark grows with the disc rather than stepping between two authored
/// sizes, and lands on the prompt's own role exactly, so a completed travel
/// draws the settled prompt.
pub(crate) fn travelling_font(theme: &Theme, scale: Scale, factor: u8) -> BitmapFont {
    let fonts = theme.fonts();
    let from = BitmapFont::for_role(fonts, TextRole::Heading, scale);
    let to = BitmapFont::for_role(fonts, TextRole::Display, scale);
    BitmapFont::new(
        to.family(),
        between(from.pixel_height(), to.pixel_height(), factor),
    )
    .with_weight(to.weight())
}

/// Which way a stage transition is running.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) enum Toward {
    /// The chooser is giving way to the chosen account's prompt.
    Prompt,
    /// The prompt is giving way back to the chooser.
    Chooser,
}

impl Toward {
    /// The other direction.
    const fn reversed(self) -> Self {
        match self {
            Self::Prompt => Self::Chooser,
            Self::Chooser => Self::Prompt,
        }
    }
}

/// One view giving way to another: the chooser stepping to a prompt, or a
/// prompt stepping back.
///
/// The chosen tile's disc travels between the two stages while the stage it
/// leaves fades out and the stage it arrives at fades in. Both ends are the
/// settled stage exactly, so a completed transition is indistinguishable from
/// never having animated.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) struct Stage {
    timeline: Timeline,
    /// The chooser slot whose disc travels.
    slot: usize,
    toward: Toward,
    /// Eased travel: `0` at the source stage, [`u8::MAX`] at the destination.
    travel: u8,
}

impl Stage {
    /// A transition of `duration_ms` toward `toward` carrying `slot`'s disc,
    /// or `None` when the theme animates it away to nothing.
    pub(crate) fn start(
        slot: usize,
        toward: Toward,
        now_ns: u64,
        duration_ms: u16,
    ) -> Option<Self> {
        let timeline = Timeline::start(now_ns, duration_ms);
        timeline.running().then(|| Self {
            timeline,
            slot,
            toward,
            travel: timeline.eased(now_ns),
        })
    }

    /// The transition that answers this one: the same disc going back the way
    /// it came, entered at the travel already made rather than at its own
    /// beginning, so an interrupted travel turns round instead of jumping.
    pub(crate) fn reverse(self, now_ns: u64, duration_ms: u16) -> Option<Self> {
        if duration_ms == 0 {
            return None;
        }
        // Smoothstep is symmetric about the half-way point, so entering the
        // return at the outgoing travel's remainder puts the disc back where
        // that travel left it, to within the one step the byte-wide curve can
        // be read at. Rounded up, so the entry lands on that remainder rather
        // than a step short of it.
        let remainder = u64::from(u8::MAX - self.timeline.progress(now_ns));
        let span = u64::from(duration_ms).saturating_mul(NANOS_PER_MS);
        let behind = remainder
            .saturating_mul(span)
            .saturating_add(u64::from(u8::MAX) - 1)
            / u64::from(u8::MAX);
        let timeline = Timeline::start(now_ns.saturating_sub(behind), duration_ms);
        Some(Self {
            timeline,
            slot: self.slot,
            toward: self.toward.reversed(),
            travel: timeline.eased(now_ns),
        })
    }

    /// The slot whose disc is travelling.
    pub(crate) const fn slot(self) -> usize {
        self.slot
    }

    /// Which way the transition runs.
    pub(crate) const fn toward(self) -> Toward {
        self.toward
    }

    /// How far along the tile-to-prompt axis the travel has come, and equally
    /// how strongly the prompt stage is drawn: `0` at the tile, [`u8::MAX`] at
    /// the prompt, whichever way the transition runs.
    ///
    /// One number for both, so a travel interrupted half-way turns round from
    /// where it is rather than from its mirror image, and so the travelling
    /// disc lifts off the tile that is dissolving and settles as the prompt's
    /// own. The chooser takes the remainder.
    pub(crate) const fn prompt_strength(self) -> u8 {
        match self.toward {
            Toward::Prompt => self.travel,
            Toward::Chooser => u8::MAX - self.travel,
        }
    }

    /// Recompute the travel from `now_ns`, reporting whether it moved.
    pub(crate) fn advance(&mut self, now_ns: u64) -> bool {
        let travel = self.timeline.eased(now_ns);
        let moved = travel != self.travel;
        self.travel = travel;
        moved
    }

    /// Whether the travel has reached the destination stage.
    pub(crate) fn finished(self, now_ns: u64) -> bool {
        self.timeline.finished(now_ns)
    }

    /// Nanoseconds until the next frame of the travel.
    pub(crate) fn next_frame_in(self, now_ns: u64) -> Option<u64> {
        self.timeline.next_frame_in(now_ns)
    }
}

/// The damped horizontal oscillation that answers a rejected attempt.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) struct Shake {
    timeline: Timeline,
    /// Displacement in 1/255 of the reach.
    offset: i32,
}

impl Shake {
    /// A shake of `duration_ms`, or `None` when the theme animates it away to
    /// nothing — a reduced-motion refusal is reported by its notice alone.
    pub(crate) fn start(now_ns: u64, duration_ms: u16) -> Option<Self> {
        let timeline = Timeline::start(now_ns, duration_ms);
        timeline.running().then(|| Self {
            timeline,
            offset: Self::displacement(timeline.progress(now_ns)),
        })
    }

    /// The oscillation at `progress`: three cycles under a linear decay, so
    /// it crosses zero on the way and comes to rest at exactly zero.
    fn displacement(progress: u8) -> i32 {
        let phase = u32::from(progress)
            .saturating_mul(SHAKE_CYCLES)
            .saturating_mul(PERIOD)
            / u32::from(u8::MAX);
        let decay = i32::from(u8::MAX) - i32::from(progress);
        sine(phase) * decay / i32::from(u8::MAX)
    }

    /// Displacement in physical pixels at `scale`, kept inside `room` on
    /// either side so a prompt with nowhere to move does not move.
    pub(crate) fn offset(self, scale: Scale, room: (u32, u32)) -> i32 {
        let reach = i32::try_from(Self::reach(scale)).unwrap_or(0);
        let moved = self.offset.saturating_mul(reach) / i32::from(u8::MAX);
        moved.clamp(
            -i32::try_from(room.0).unwrap_or(0),
            i32::try_from(room.1).unwrap_or(0),
        )
    }

    /// How far either side of its resting place the shaken band can ever be,
    /// in physical pixels at `scale` — which is what its damage covers.
    pub(crate) fn reach(scale: Scale) -> u32 {
        scale.scale_length(SHAKE_REACH)
    }

    /// Recompute the displacement from `now_ns`, reporting whether it moved.
    pub(crate) fn advance(&mut self, now_ns: u64) -> bool {
        let offset = Self::displacement(self.timeline.progress(now_ns));
        let moved = offset != self.offset;
        self.offset = offset;
        moved
    }

    /// Whether the oscillation has decayed to its rest.
    pub(crate) fn finished(self, now_ns: u64) -> bool {
        self.timeline.finished(now_ns)
    }

    /// Nanoseconds until the next frame of the oscillation.
    pub(crate) fn next_frame_in(self, now_ns: u64) -> Option<u64> {
        self.timeline.next_frame_in(now_ns)
    }
}

/// The black the screen arrives from and leaves through.
///
/// Deliberately not a theme colour: the desktop taking the screen over
/// arrives out of the same black, and the two halves of that handover meet
/// only if both name the one absolute rather than each its own appearance.
pub(crate) const VEIL: Rgba = Rgba::rgb(0, 0, 0);

/// The black over the whole screen, running between two strengths: away as
/// the surface arrives, and up to opaque as it leaves once a secret is
/// accepted.
///
/// One veil for both directions, so the screen a person is shown and the
/// screen they leave are the same black at the same weight, and a fade
/// interrupted by the other cannot jump. The ramp is the shared [`Fade`],
/// which the desktop's own screen fade runs on too: the two halves of the
/// handover cannot drift apart if neither owns the arithmetic.
///
/// The strength is cached rather than read from the clock, because painting
/// reads none: the owner steps this, and every frame draws what that step
/// decided.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) struct Veil {
    fade: Fade,
    /// How much of the screen the black has taken.
    strength: u8,
}

impl Veil {
    /// A veil of `duration_ms` uncovering the screen from `now_ns`: opaque at
    /// the start, gone at the end. A theme that animates it away to nothing
    /// begins already clear and already finished.
    pub(crate) fn arriving(now_ns: u64, duration_ms: u16) -> Self {
        Self::between(u8::MAX, 0, now_ns, duration_ms)
    }

    /// A veil of `duration_ms` covering the screen from `now_ns`, entered at
    /// `from` so a screen still uncovering goes on to black from the strength
    /// it had reached rather than brightening first. A theme that animates it
    /// away to nothing begins already black and already finished.
    pub(crate) fn leaving(from: u8, now_ns: u64, duration_ms: u16) -> Self {
        Self::between(from, u8::MAX, now_ns, duration_ms)
    }

    fn between(from: u8, to: u8, now_ns: u64, duration_ms: u16) -> Self {
        let mut veil = Self {
            fade: Fade::start(now_ns, duration_ms, from, to),
            strength: from,
        };
        veil.advance(now_ns);
        veil
    }

    /// How opaque the black is now. A pure strength, so it takes the linear
    /// progress rather than the shaping a travelling element wants.
    pub(crate) const fn strength(self) -> u8 {
        self.strength
    }

    /// Whether this is the veil the screen leaves through rather than the one
    /// it arrives from.
    pub(crate) const fn is_leaving(self) -> bool {
        self.fade.target() == u8::MAX
    }

    /// Recompute the strength from `now_ns`, reporting whether it moved.
    ///
    /// The end strength is the end of the fade, and the owner holds a veil
    /// for as long as it holds the screen, so the fade settles here: a
    /// screen that has already gone asks for no further frame.
    pub(crate) fn advance(&mut self, now_ns: u64) -> bool {
        let strength = self.fade.strength(now_ns);
        let moved = strength != self.strength;
        self.strength = strength;
        if self.finished() {
            self.fade.settle();
        }
        moved
    }

    /// Whether the fade has reached the strength it runs to.
    pub(crate) const fn finished(self) -> bool {
        self.strength == self.fade.target()
    }

    /// Nanoseconds until the next frame of the fade.
    pub(crate) fn next_frame_in(self, now_ns: u64) -> Option<u64> {
        self.fade.next_frame_in(now_ns)
    }
}

/// What one round of animation changed.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) enum Changed {
    /// Nothing moved.
    Nothing,
    /// This rectangle owes a repaint.
    Region(Rect),
    /// The whole screen owes one.
    Whole,
}

impl Changed {
    /// One report covering both, so a round that stepped several animations
    /// answers with a single rectangle rather than one report each.
    pub(crate) fn merged(self, other: Self) -> Self {
        match (self, other) {
            (Self::Nothing, report) | (report, Self::Nothing) => report,
            (Self::Whole, _) | (_, Self::Whole) => Self::Whole,
            (Self::Region(mine), Self::Region(theirs)) => Self::Region(mine.union(&theirs)),
        }
    }

    /// Whether anything moved at all.
    pub(crate) const fn moved(self) -> bool {
        !matches!(self, Self::Nothing)
    }

    /// This report as a damage rectangle, `None` being the whole screen.
    pub(crate) const fn damage(self) -> Option<Rect> {
        match self {
            Self::Nothing | Self::Whole => None,
            Self::Region(rect) => Some(rect),
        }
    }
}

/// The nearer of two frame deadlines, or whichever one there is.
pub(crate) fn sooner(one: Option<u64>, other: Option<u64>) -> Option<u64> {
    match (one, other) {
        (Some(one), Some(other)) => Some(one.min(other)),
        (only @ Some(_), None) | (None, only @ Some(_)) => only,
        (None, None) => None,
    }
}
