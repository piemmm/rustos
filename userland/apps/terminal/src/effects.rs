//! The terminal's screen effects: the ordered post-processing pipeline a
//! rendered frame passes through before it is presented.
//!
//! # The pipeline is a description, not a pile of flags
//!
//! [`Effects`] carries one strength per effect, each a permille `0..=1000`
//! that a settings slider sets directly. [`Effects::passes`] turns that into
//! the ordered [`Pass`] list a frame actually goes through, and
//! [`apply`](Effects::apply) runs those passes over a
//! [`Surface`]. A pass whose strength is zero is not
//! in the list at all, so a terminal with the effects off pays nothing.
//!
//! Keeping the pipeline as a typed, ordered description is deliberate: a
//! display that can composite hardware layers can read the same list and
//! programme its own engine from it, with the software passes here staying
//! the conformance oracle for what the result must look like. That is why an
//! effect is a *description plus* an implementation rather than code inlined
//! into the renderer.
//!
//! [`EffectKey`] is the other half of that: one ordered list of which effects
//! exist, what each is called, and where its strength lives, so the settings
//! sheet reads a row back through the same list it built it from and cannot
//! map a slider onto the wrong field.
//!
//! # The effects
//!
//! * **Translucency** is not a pass. It is the alpha the default background
//!   is filled at ([`Effects::background_alpha`]), applied while the cells
//!   are painted, so the compositor's own premultiplied blend does the work
//!   and a glyph stays fully opaque over a see-through background.
//! * **Backdrop blur** is not a pass either: only the compositor can see what
//!   is behind a window, so the strength is converted to a logical radius
//!   ([`Effects::blur_radius_px`]) and handed to the window channel.
//! * **Scan lines** dim every other physical row, the flat part of a shadow
//!   mask's look.
//! * **Glow** spreads the light of brightly-driven pixels sideways into their
//!   neighbourhood, the halation of a tube's faceplate glass.
//! * **Fuzz** adds a per-pixel luminance jitter that moves each animation
//!   step, the analogue noise floor of a composite signal.
//! * **Phosphor** is a persistence trail: a decaying record of what was lit
//!   recently, added back to the frame so fast-scrolling text smears the way
//!   a long-persistence tube does.
//! * **Wobble** displaces each row horizontally along a slow travelling sine,
//!   the horizontal-oscillator instability of a tube that is not quite in
//!   time.
//!
//! Glow and phosphor are the two halves of a tube's light and are independent:
//! phosphor spreads light through *time*, glow spreads it through *space*.
//!
//! # Animation is a clock, not a spin
//!
//! Every animated pass is a pure function of a monotonically increasing
//! [`Phase`]. The program advances the phase when its wait deadline elapses,
//! so an animated terminal costs one timed wake per frame and an unanimated
//! one parks indefinitely — the emulator never polls.

use alloc::vec::Vec;

use tairix_raster::{box_blur, Pixel, Surface};

/// The full-scale strength of an effect, in permille — the value a slider at
/// its maximum sets.
pub const FULL: u16 = 1000;

/// The largest backdrop-blur radius the terminal will ask the compositor for,
/// in logical pixels.
///
/// A fixed bound on how much per-frame work one window may demand of the
/// compositor, not a capacity that grows with the machine: a wider blur costs
/// the desktop, not this app.
pub const MAX_BLUR_RADIUS_PX: u16 = 24;

/// The least opaque a background may be made, in permille.
///
/// A window faded past this stops being a terminal and starts being a way to
/// make text unreadable, so the slider cannot reach it. Text itself never
/// fades — only the background does.
pub const MIN_OPACITY: u16 = 300;

/// How far a wobble may displace a row, in physical pixels at the reference
/// density, at full strength.
const MAX_WOBBLE_PX: u32 = 6;

/// The strongest per-pixel luminance jitter fuzz applies, as an 8-bit
/// amplitude at full strength.
const MAX_FUZZ_AMPLITUDE: u32 = 56;

/// How much of a lit pixel survives into the next frame's afterglow at full
/// phosphor strength, in permille.
const MAX_PERSISTENCE: u32 = 820;

/// The deepest a scan line dims the row it darkens, in permille, at full
/// strength.
const MAX_SCANLINE_DEPTH: u32 = 620;

/// How far a glow reaches from the pixel emitting it, in physical pixels at
/// the reference density.
///
/// Halation is a fixed distance through the faceplate glass, so the reach is a
/// density-scaled constant rather than a second slider: the strength sets how
/// *bright* the halo is, not how wide.
const GLOW_REACH_PX: u32 = 5;

/// How hard a pixel must be driven before it glows at all, on the same 8-bit
/// scale as a colour channel.
///
/// Half scale: text of any colour clears it, an unlit or mid-grey background
/// does not. Without a knee the whole screen would glow into itself and the
/// effect would read as a flat wash rather than as light coming off the text.
const GLOW_KNEE: u8 = 128;

/// How much of the spread light is added back at full glow strength, in
/// permille.
///
/// Above unity, so at full strength a halo is brighter than the light it was
/// spread from — which is what makes the effect read as a tube's bloom rather
/// than a faint wash. Still capped, for the reason [`MIN_OPACITY`] is: a
/// slider must not be able to make text unreadable, and a light scheme's
/// background clears [`GLOW_KNEE`] on its own, so at full strength the whole
/// field glows.
const MAX_GLOW_INTENSITY: u32 = 1200;

// The knee is a floor to subtract from, so it has to leave something above it.
const _: () = assert!(GLOW_KNEE < u8::MAX);

/// A monotonic animation step.
///
/// Not a wall-clock time: the effects only need *a* value that advances once
/// per animated frame, and taking a step count rather than a timestamp keeps
/// every pass exactly reproducible in a test.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Default, PartialOrd, Ord)]
pub struct Phase(pub u32);

impl Phase {
    /// The next step. Wrapping is deliberate and harmless: every pass reads
    /// the phase through a periodic function.
    #[must_use]
    pub const fn advance(self) -> Self {
        Self(self.0.wrapping_add(1))
    }
}

/// One stage of the pipeline, in the order a frame passes through it.
///
/// The list is what an accelerated path would programme its engine from, so
/// each variant carries the *resolved* physical parameters rather than the
/// permille the user set.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Pass {
    /// Displace each row horizontally along a travelling sine of this
    /// amplitude in physical pixels.
    Wobble {
        /// Peak displacement, in physical pixels.
        amplitude_px: u32,
    },
    /// Add the decaying afterglow of recently-lit pixels back into the frame.
    Phosphor {
        /// How much of a lit pixel survives one frame, in permille.
        persistence: u32,
    },
    /// Dim alternate physical rows.
    ScanLines {
        /// How deeply the dimmed rows are darkened, in permille.
        depth: u32,
    },
    /// Spread the light of brightly-driven pixels into their neighbourhood.
    Glow {
        /// How far the halo reaches, in physical pixels, falling off as a
        /// tent across that reach.
        reach_px: u32,
        /// How much of the spread light is added back, in permille.
        intensity: u32,
    },
    /// Jitter each pixel's luminance.
    Fuzz {
        /// Peak jitter, as an 8-bit amplitude.
        amplitude: u32,
    },
}

impl Pass {
    /// Whether this pass looks different from one [`Phase`] to the next, and
    /// so needs a timed repaint to be seen moving.
    #[must_use]
    pub const fn is_animated(self) -> bool {
        match self {
            Self::Wobble { .. } | Self::Phosphor { .. } | Self::Fuzz { .. } => true,
            Self::ScanLines { .. } | Self::Glow { .. } => false,
        }
    }
}

/// The most passes the pipeline can hold — one per [`Pass`] variant, since a
/// pass appears at most once.
pub const MAX_PASSES: usize = 5;

/// The strengths every screen effect is set to, each in permille.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Effects {
    /// How opaque the default background is: [`FULL`] is solid, less lets the
    /// desktop through. Never below [`MIN_OPACITY`].
    pub opacity: u16,
    /// How strongly the compositor blurs what is behind the window.
    pub blur: u16,
    /// How deeply alternate rows are dimmed.
    pub scanlines: u16,
    /// How brightly lit pixels glow into their neighbourhood.
    pub glow: u16,
    /// How much per-pixel luminance jitter is added.
    pub fuzz: u16,
    /// How long a lit pixel persists as an afterglow.
    pub phosphor: u16,
    /// How far rows are displaced by the travelling wobble.
    pub wobble: u16,
}

impl Default for Effects {
    /// A see-through, frosted window, and every effect that costs a frame off.
    ///
    /// Translucency is free: it is the alpha the background is filled at while
    /// the cells are painted, so the compositor's own blend does the work. The
    /// blur is what makes it read as glass rather than as a hole, and it is
    /// half strength because that is the depth at which text stays legible over
    /// a busy backdrop. The compositor stopped computing the frosts a stack of
    /// them buries (`plans/FIX-DESKTOP-SPEEDUP.md` D.13), which is what made a
    /// screenful of frosted terminals affordable.
    fn default() -> Self {
        Self {
            opacity: 800,
            blur: 500,
            scanlines: 0,
            glow: 0,
            fuzz: 0,
            phosphor: 0,
            wobble: 0,
        }
    }
}

impl Effects {
    /// The alpha the default background is filled at, `0..=255`.
    #[must_use]
    pub fn background_alpha(self) -> u8 {
        let clamped = self.opacity.clamp(MIN_OPACITY, FULL);
        u8::try_from(scaled(clamped, 255)).unwrap_or(u8::MAX)
    }

    /// The backdrop-blur radius to ask the compositor for, in logical pixels.
    ///
    /// A blur behind an opaque window would be invisible work, so a fully
    /// opaque terminal asks for none however far the slider is pushed.
    #[must_use]
    pub fn blur_radius_px(self) -> u16 {
        if self.opacity >= FULL {
            return 0;
        }
        u16::try_from(scaled(self.blur, u32::from(MAX_BLUR_RADIUS_PX)))
            .unwrap_or(MAX_BLUR_RADIUS_PX)
    }

    /// The ordered passes a frame goes through, at `scale_percent` of the
    /// reference density (so a wobble is the same *apparent* size on a dense
    /// screen as on a sparse one).
    ///
    /// Returns the filled prefix of the array; a zero-strength effect
    /// contributes no pass.
    #[must_use]
    pub fn passes(self, scale_percent: u32) -> ([Pass; MAX_PASSES], usize) {
        let mut passes = [Pass::ScanLines { depth: 0 }; MAX_PASSES];
        let mut len = 0;
        let mut push = |pass: Pass| {
            if let Some(slot) = passes.get_mut(len) {
                *slot = pass;
                len += 1;
            }
        };
        // Geometry first, so what a later pass jitters or dims is the frame
        // as it will actually be seen.
        if self.wobble > 0 {
            let reach = MAX_WOBBLE_PX.saturating_mul(scale_percent.max(1)) / 100;
            let amplitude_px = scaled(self.wobble, reach.max(1)).max(1);
            push(Pass::Wobble { amplitude_px });
        }
        if self.phosphor > 0 {
            push(Pass::Phosphor {
                persistence: scaled(self.phosphor, MAX_PERSISTENCE),
            });
        }
        if self.scanlines > 0 {
            push(Pass::ScanLines {
                depth: scaled(self.scanlines, MAX_SCANLINE_DEPTH),
            });
        }
        // The glow spreads the light the passes above decided on, so it runs
        // after them and its halo reaches into the scan lines' dark rows, as a
        // tube's does. Fuzz stays last so the noise floor reads as noise
        // rather than being smeared into the halo.
        if self.glow > 0 {
            let reach = GLOW_REACH_PX.saturating_mul(scale_percent.max(1)) / 100;
            push(Pass::Glow {
                reach_px: reach.max(1),
                intensity: scaled(self.glow, MAX_GLOW_INTENSITY),
            });
        }
        if self.fuzz > 0 {
            push(Pass::Fuzz {
                amplitude: scaled(self.fuzz, MAX_FUZZ_AMPLITUDE),
            });
        }
        (passes, len)
    }

    /// Whether any enabled pass changes from one [`Phase`] to the next, and
    /// so whether the program needs a timed repaint at all.
    #[must_use]
    pub fn is_animated(self, scale_percent: u32) -> bool {
        let (passes, len) = self.passes(scale_percent);
        passes.iter().take(len).any(|pass| pass.is_animated())
    }

    /// Run the pipeline over `surface` at animation step `phase`, at
    /// `scale_percent` of the reference density.
    ///
    /// `state` carries what the stateful passes remember between frames; a
    /// caller keeps one alive across frames and
    /// [`clear`](EffectState::clear)s it when the screen it describes stops
    /// existing. The surface is left the same size it arrived at, whatever
    /// the passes did.
    pub fn apply(
        self,
        surface: &mut Surface,
        state: &mut EffectState,
        phase: Phase,
        scale_percent: u32,
    ) {
        let (passes, len) = self.passes(scale_percent);
        for pass in passes.iter().take(len) {
            match *pass {
                Pass::Wobble { amplitude_px } => wobble(surface, amplitude_px, phase),
                Pass::Phosphor { persistence } => state.persistence.apply(surface, persistence),
                Pass::ScanLines { depth } => scan_lines(surface, depth),
                Pass::Glow {
                    reach_px,
                    intensity,
                } => state.halo.apply(surface, reach_px, intensity),
                Pass::Fuzz { amplitude } => fuzz(surface, amplitude, phase),
            }
        }
    }
}

/// Which screen effects exist, what each is called, and where its strength
/// lives in [`Effects`].
///
/// The settings sheet builds a slider row per key and reads a row back
/// through this same list, so a reordering cannot redirect a slider onto
/// another effect's field.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum EffectKey {
    /// [`Effects::opacity`].
    Opacity,
    /// [`Effects::blur`].
    Blur,
    /// [`Effects::scanlines`].
    ScanLines,
    /// [`Effects::glow`].
    Glow,
    /// [`Effects::fuzz`].
    Fuzz,
    /// [`Effects::phosphor`].
    Phosphor,
    /// [`Effects::wobble`].
    Wobble,
}

impl EffectKey {
    /// Every effect, in the order the settings sheet lists them.
    pub const ALL: [Self; 7] = [
        Self::Opacity,
        Self::Blur,
        Self::ScanLines,
        Self::Glow,
        Self::Fuzz,
        Self::Phosphor,
        Self::Wobble,
    ];

    /// How many effects there are.
    pub const COUNT: usize = Self::ALL.len();

    /// The human label the settings sheet shows.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Opacity => "Opacity",
            Self::Blur => "Backdrop blur",
            Self::ScanLines => "Scan lines",
            Self::Glow => "Glow",
            Self::Fuzz => "Fuzz",
            Self::Phosphor => "Phosphor",
            Self::Wobble => "Wobble",
        }
    }

    /// The closed range this effect's slider spans.
    ///
    /// Opacity starts at [`MIN_OPACITY`] rather than zero because a fully
    /// transparent screen is unreadable; mapping the slider onto that range
    /// keeps the whole travel live instead of leaving its first third
    /// snapping back.
    #[must_use]
    pub const fn bounds(self) -> (u16, u16) {
        match self {
            Self::Opacity => (MIN_OPACITY, FULL),
            Self::Blur
            | Self::ScanLines
            | Self::Glow
            | Self::Fuzz
            | Self::Phosphor
            | Self::Wobble => (0, FULL),
        }
    }

    /// This effect's strength in `effects`.
    #[must_use]
    pub const fn of(self, effects: Effects) -> u16 {
        match self {
            Self::Opacity => effects.opacity,
            Self::Blur => effects.blur,
            Self::ScanLines => effects.scanlines,
            Self::Glow => effects.glow,
            Self::Fuzz => effects.fuzz,
            Self::Phosphor => effects.phosphor,
            Self::Wobble => effects.wobble,
        }
    }

    /// Set this effect's strength in `effects`.
    pub fn set(self, effects: &mut Effects, permille: u16) {
        let field = match self {
            Self::Opacity => &mut effects.opacity,
            Self::Blur => &mut effects.blur,
            Self::ScanLines => &mut effects.scanlines,
            Self::Glow => &mut effects.glow,
            Self::Fuzz => &mut effects.fuzz,
            Self::Phosphor => &mut effects.phosphor,
            Self::Wobble => &mut effects.wobble,
        };
        *field = permille;
    }
}

/// Scale a permille `strength` onto `full`, rounding to nearest.
fn scaled(strength: u16, full: u32) -> u32 {
    let strength = u32::from(strength.min(FULL));
    let full_permille = u32::from(FULL);
    (strength.saturating_mul(full) + full_permille / 2) / full_permille
}

/// What the stateful passes remember between frames.
///
/// Two passes keep something — the phosphor's persistence trail and the
/// glow's working buffers — and a caller has no business knowing which, so
/// one object carries both and one [`clear`](Self::clear) forgets everything
/// held under a screen that has stopped existing.
#[derive(Clone, Debug, Default)]
pub struct EffectState {
    persistence: Persistence,
    halo: Halo,
}

impl EffectState {
    /// State remembering nothing.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Forget everything remembered and give the buffers back.
    ///
    /// Called when the screen's geometry or colours change underneath: a
    /// trail of a screen that no longer exists is a ghost of the wrong
    /// thing, and a terminal whose effects were switched off should not hold
    /// screenfuls of pixels.
    pub fn clear(&mut self) {
        self.persistence.clear();
        self.halo.clear();
    }
}

/// Size `buffer` to `count` items of `fill`, discarding what it held.
///
/// Reports `false` when the process cannot afford the buffer, so a caller
/// draws without its effect rather than aborting on an allocation.
fn refit<T: Copy>(buffer: &mut Vec<T>, count: usize, fill: T) -> bool {
    buffer.clear();
    if buffer.try_reserve_exact(count).is_err() {
        return false;
    }
    buffer.resize(count, fill);
    true
}

/// The persistence state a phosphor pass carries between frames: how brightly
/// each pixel was lit, decaying towards dark.
///
/// The buffer is grown to fit the surface it is asked to work on and then
/// reused, so an animated terminal allocates once rather than once a frame.
#[derive(Clone, Debug, Default)]
struct Persistence {
    /// Per-pixel remembered luminance, row-major, `width * height` long.
    trail: Vec<u8>,
    /// The surface width `trail` was sized for.
    width: u32,
    /// The surface height `trail` was sized for.
    height: u32,
}

impl Persistence {
    /// Forget everything remembered, so the next frame starts clean.
    fn clear(&mut self) {
        self.trail = Vec::new();
        self.width = 0;
        self.height = 0;
    }

    /// Add the remembered afterglow into `surface` and remember this frame,
    /// keeping `persistence` permille of each lit pixel.
    ///
    /// A surface the trail cannot be sized for (an allocation the process
    /// cannot afford) simply draws with no afterglow — never a panic, and
    /// never a wrong frame.
    fn apply(&mut self, surface: &mut Surface, persistence: u32) {
        let (width, height) = (surface.width(), surface.height());
        if width == 0 || height == 0 {
            return;
        }
        let Some(needed) = (width as usize).checked_mul(height as usize) else {
            return;
        };
        // A reshape that happens to preserve the area still describes a
        // different screen, so the dimensions decide this, not the length.
        if self.width != width || self.height != height || self.trail.len() != needed {
            if !refit(&mut self.trail, needed, 0) {
                self.clear();
                return;
            }
            self.width = width;
            self.height = height;
        }
        for y in 0..height {
            let Some((_, row)) = surface.row_span_mut(y, 0, width) else {
                continue;
            };
            let base = usize::try_from(y).unwrap_or(0) * usize::try_from(width).unwrap_or(0);
            for (x, pixel) in row.iter_mut().enumerate() {
                let Some(slot) = self.trail.get_mut(base.saturating_add(x)) else {
                    continue;
                };
                let lit = luminance(*pixel);
                // What the tube still holds from before is added back, then
                // this frame's own light is remembered in its place.
                let trail = *slot;
                if trail > 0 {
                    *pixel = add_trail(*pixel, trail);
                }
                let decayed = u32::from(trail) * persistence / u32::from(FULL);
                *slot = u8::try_from(decayed.max(u32::from(lit) * persistence / u32::from(FULL)))
                    .unwrap_or(u8::MAX);
            }
        }
    }
}

/// A pixel's premultiplied luminance, `0..=255`.
fn luminance(pixel: Pixel) -> u8 {
    let sum = 299 * u32::from(pixel.r) + 587 * u32::from(pixel.g) + 114 * u32::from(pixel.b);
    u8::try_from(sum / 1000).unwrap_or(u8::MAX)
}

/// How much of a remembered trail is added back to each channel: a third,
/// so it reads as a dimming ghost rather than a second copy of the text.
const TRAIL_SHARE: u32 = 3;

/// Add `trail` back into `pixel`, saturating.
///
/// The trail keeps the pixel's own hue rather than washing towards white,
/// which is what a single-phosphor tube actually does. Alpha rises with it:
/// light the tube is emitting is not see-through, however translucent the
/// background behind it is.
fn add_trail(pixel: Pixel, trail: u8) -> Pixel {
    let lift = |channel: u8| -> u8 {
        u8::try_from(u32::from(channel) + u32::from(trail) / TRAIL_SHARE).unwrap_or(u8::MAX)
    };
    Pixel {
        r: lift(pixel.r),
        g: lift(pixel.g),
        b: lift(pixel.b),
        a: lift(pixel.a),
    }
}

/// The buffers a glow pass works in: the highlights it extracts from the
/// frame, and the intermediate the separable blur hands from its horizontal
/// pass to its vertical one.
///
/// Both are grown to the surface and reused, so an animated terminal
/// allocates once rather than once a frame. Neither carries meaning between
/// frames — the glow is a function of the frame it is spread from — so a
/// resize costs a resize and nothing else.
#[derive(Clone, Debug, Default)]
struct Halo {
    /// The extracted highlight field, row-major, blurred in place.
    highlights: Vec<Pixel>,
    /// The blur's pass-to-pass intermediate.
    aux: Vec<Pixel>,
}

impl Halo {
    /// Give the buffers back.
    fn clear(&mut self) {
        self.highlights = Vec::new();
        self.aux = Vec::new();
    }

    /// Size both buffers to `count` pixels, or report that the process cannot
    /// afford them and hold neither.
    fn fit(&mut self, count: usize) -> bool {
        if self.highlights.len() == count && self.aux.len() == count {
            return true;
        }
        if refit(&mut self.highlights, count, Pixel::TRANSPARENT)
            && refit(&mut self.aux, count, Pixel::TRANSPARENT)
        {
            return true;
        }
        self.clear();
        false
    }

    /// Spread the light of `surface`'s brightly-driven pixels `reach_px` into
    /// their neighbourhood and add `intensity` permille of it back.
    ///
    /// A surface the buffers cannot be sized for simply draws with no glow —
    /// never a panic, and never a partly-glowed frame.
    fn apply(&mut self, surface: &mut Surface, reach_px: u32, intensity: u32) {
        let (width, height) = (surface.width(), surface.height());
        let (Ok(cols), Ok(rows)) = (usize::try_from(width), usize::try_from(height)) else {
            return;
        };
        let Some(count) = cols.checked_mul(rows) else {
            return;
        };
        if count == 0 || reach_px == 0 || intensity == 0 || !self.fit(count) {
            return;
        }
        // A pass is a whole-frame post-process, and this one reads a
        // neighbourhood rather than a pixel. A surface offering less than its
        // whole extent would have it spreading light it may not read, so it
        // declines before touching a thing.
        for y in 0..height {
            let Some((first, row)) = surface.row_span(y, 0, width) else {
                return;
            };
            if first != 0 || row.len() != cols {
                return;
            }
            let Some(slots) = self.row_mut(y, cols) else {
                return;
            };
            for (slot, pixel) in slots.iter_mut().zip(row) {
                *slot = highlight(*pixel);
            }
        }
        // Two boxes rather than one: a single box average leaves a
        // rectangular halo with a visible edge, where two sum to a tent that
        // falls off smoothly over the same reach.
        let near = usize::try_from(reach_px / 2).unwrap_or(0);
        let far = usize::try_from(reach_px - reach_px / 2).unwrap_or(0);
        box_blur(&mut self.highlights, cols, rows, near, &mut self.aux);
        box_blur(&mut self.highlights, cols, rows, far, &mut self.aux);
        for y in 0..height {
            let Some(light) = self.row(y, cols) else {
                return;
            };
            let Some((_, row)) = surface.row_span_mut(y, 0, width) else {
                return;
            };
            for (pixel, light) in row.iter_mut().zip(light) {
                *pixel = add_light(*pixel, gain(*light, intensity));
            }
        }
    }

    /// Row `y` of the highlight field, `cols` wide.
    fn row(&self, y: u32, cols: usize) -> Option<&[Pixel]> {
        let base = usize::try_from(y).ok()?.checked_mul(cols)?;
        self.highlights.get(base..)?.get(..cols)
    }

    /// Row `y` of the highlight field, `cols` wide, to write.
    fn row_mut(&mut self, y: u32, cols: usize) -> Option<&mut [Pixel]> {
        let base = usize::try_from(y).ok()?.checked_mul(cols)?;
        self.highlights.get_mut(base..)?.get_mut(..cols)
    }
}

/// The light `pixel` spreads into its neighbourhood: how far its
/// hardest-driven channel clears [`GLOW_KNEE`], taken in the pixel's own
/// colour.
///
/// The drive is the peak channel rather than a luma weighting, because a
/// phosphor driven to full emits as much light whatever its colour — under
/// luma's weights, saturated red would glow at half of saturated green and
/// blue at a fifth of it.
fn highlight(pixel: Pixel) -> Pixel {
    let peak = pixel.r.max(pixel.g).max(pixel.b);
    let Some(over) = peak.checked_sub(GLOW_KNEE) else {
        return Pixel::TRANSPARENT;
    };
    let span = u32::from(u8::MAX - GLOW_KNEE);
    let share = u8::try_from(u32::from(over) * u32::from(u8::MAX) / span).unwrap_or(u8::MAX);
    pixel.scale_alpha(share)
}

/// `light` scaled by `permille`, saturating each channel.
///
/// A gain, not a fade: the glow adds back more light than it spread at any
/// intensity above unity, so the multiply is taken wide and clamped. Scaling
/// every channel by one factor and clamping them all at the same ceiling
/// keeps the pixel premultiplied.
fn gain(light: Pixel, permille: u32) -> Pixel {
    let channel = |value: u8| {
        u8::try_from(u32::from(value).saturating_mul(permille) / u32::from(FULL)).unwrap_or(u8::MAX)
    };
    Pixel {
        r: channel(light.r),
        g: channel(light.g),
        b: channel(light.b),
        a: channel(light.a),
    }
}

/// Add `light` into `pixel`, saturating.
///
/// Alpha rises with the colour, as the phosphor trail's does: light the tube
/// is emitting is not see-through, however translucent the background behind
/// it is. Both operands are premultiplied, so adding them channel-wise keeps
/// every channel inside the alpha it is premultiplied against.
fn add_light(pixel: Pixel, light: Pixel) -> Pixel {
    Pixel {
        r: pixel.r.saturating_add(light.r),
        g: pixel.g.saturating_add(light.g),
        b: pixel.b.saturating_add(light.b),
        a: pixel.a.saturating_add(light.a),
    }
}

/// Dim alternate physical rows by `depth` permille.
fn scan_lines(surface: &mut Surface, depth: u32) {
    let width = surface.width();
    let keep = u32::from(FULL).saturating_sub(depth.min(u32::from(FULL)));
    let factor = u8::try_from(keep * 255 / u32::from(FULL)).unwrap_or(u8::MAX);
    let mut y = 1;
    while y < surface.height() {
        if let Some((_, row)) = surface.row_span_mut(y, 0, width) {
            for pixel in row.iter_mut() {
                *pixel = pixel.scale_alpha(factor);
            }
        }
        y += 2;
    }
}

/// Jitter each pixel's luminance by up to `amplitude`, deterministically for
/// a given `phase` so the same frame always renders the same way.
fn fuzz(surface: &mut Surface, amplitude: u32, phase: Phase) {
    if amplitude == 0 {
        return;
    }
    let width = surface.width();
    let span = amplitude.saturating_mul(2).saturating_add(1);
    let amplitude = i32::try_from(amplitude).unwrap_or(i32::MAX);
    for y in 0..surface.height() {
        let Some((_, row)) = surface.row_span_mut(y, 0, width) else {
            continue;
        };
        let mut state = splitmix((u64::from(phase.0) << 32) | u64::from(y));
        for pixel in row.iter_mut() {
            state = splitmix(state);
            let draw = u32::try_from(state >> 40).unwrap_or(0) % span;
            let delta = i32::try_from(draw).unwrap_or(0) - amplitude;
            *pixel = jitter_pixel(*pixel, delta);
        }
    }
}

/// Shift a premultiplied pixel's colour channels by `delta`, clamped to the
/// alpha it is premultiplied against so the pixel stays valid.
fn jitter_pixel(pixel: Pixel, delta: i32) -> Pixel {
    let shift = |channel: u8| -> u8 {
        let value = i32::from(channel) + delta;
        u8::try_from(value.clamp(0, i32::from(pixel.a))).unwrap_or(0)
    };
    Pixel {
        r: shift(pixel.r),
        g: shift(pixel.g),
        b: shift(pixel.b),
        a: pixel.a,
    }
}

/// Displace each row horizontally along a travelling sine of `amplitude_px`.
fn wobble(surface: &mut Surface, amplitude_px: u32, phase: Phase) {
    if amplitude_px == 0 {
        return;
    }
    let width = surface.width();
    let height = surface.height();
    if width == 0 {
        return;
    }
    let amplitude = i32::try_from(amplitude_px).unwrap_or(i32::MAX);
    let mut scratch = Vec::new();
    if scratch.try_reserve_exact(width as usize).is_err() {
        return;
    }
    for y in 0..height {
        // A slow vertical travel plus a slow advance in time: the wave moves
        // down the screen rather than the whole screen shearing together.
        let angle = (y.wrapping_mul(6).wrapping_add(phase.0.wrapping_mul(3))) % SINE_PERIOD;
        let offset = sine(angle) * amplitude / SINE_SCALE;
        if offset == 0 {
            continue;
        }
        let Some((_, row)) = surface.row_span_mut(y, 0, width) else {
            continue;
        };
        scratch.clear();
        scratch.extend_from_slice(row);
        for (x, pixel) in row.iter_mut().enumerate() {
            let source = i32::try_from(x).unwrap_or(i32::MAX) - offset;
            *pixel = usize::try_from(source)
                .ok()
                .and_then(|index| scratch.get(index).copied())
                .unwrap_or(Pixel::TRANSPARENT);
        }
    }
}

/// The number of steps in one full turn of [`sine`].
const SINE_PERIOD: u32 = 64;

/// The amplitude [`sine`] returns at its peak.
const SINE_SCALE: i32 = 256;

/// One quarter turn of a sine, scaled to [`SINE_SCALE`].
///
/// A table rather than a float: the effect only needs a smooth periodic
/// displacement, and an integer table is exactly reproducible on every target.
const SINE_QUARTER: [i32; 17] = [
    0, 25, 50, 74, 98, 121, 142, 162, 181, 198, 213, 226, 237, 245, 251, 255, 256,
];

/// The sine of `step` sixty-fourths of a turn, scaled to [`SINE_SCALE`].
fn sine(step: u32) -> i32 {
    let step = step % SINE_PERIOD;
    let quarter = step / 16;
    let within = usize::try_from(step % 16).unwrap_or(0);
    let value = |index: usize| SINE_QUARTER.get(index).copied().unwrap_or(0);
    match quarter {
        0 => value(within),
        1 => value(16 - within),
        2 => -value(within),
        _ => -value(16 - within),
    }
}

/// One round of the `SplitMix64` mixing function.
///
/// The fuzz needs a cheap, well-distributed, *reproducible* jitter, not
/// unpredictability: nothing here is a secret, so this is deliberately not
/// the system's cryptographic generator.
const fn splitmix(state: u64) -> u64 {
    let mut z = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

#[cfg(test)]
#[path = "effects_tests.rs"]
mod tests;
