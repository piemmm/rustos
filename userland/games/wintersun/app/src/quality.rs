//! The degradation ladder: what gives way when a frame will not fit, and
//! in what order.
//!
//! A renderer that sheds whatever is cheapest to shed degrades
//! unpredictably, and a reviewer cannot tell a deliberate trade from a
//! bug. So the order is fixed and total — particle density, then
//! light-buffer resolution, then detail-material octaves, then shadow
//! softness, then render scale — one notch at a time, and frame rate is
//! never what gives way.
//!
//! The ladder is a single step count. Each rung sheds through its own
//! notches before the next rung is touched at all, so two machines at the
//! same step are drawing the same picture, and the step is the one number
//! a diagnostic has to report.

use tairix_wintersun_art::material::{Quality as MaterialQuality, MAX_OCTAVES};

/// Which knob is currently giving way.
///
/// Reported for diagnosis: the step alone says how far the renderer has
/// fallen back, and this says what a viewer is actually seeing less of.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum Rung {
    /// Nothing has been shed.
    Full,
    /// Fewer particles.
    ParticleDensity,
    /// A coarser light buffer.
    LightResolution,
    /// Flatter detail on the materials.
    MaterialDetail,
    /// Harder, then absent, contact shadows.
    ShadowSoftness,
    /// A smaller render target, upscaled to the window.
    RenderScale,
}

/// How a contact shadow is drawn.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Shadow {
    /// Penumbra, as authored.
    Soft,
    /// A hard edge, which costs one test rather than a kernel.
    Hard,
    /// None at all.
    Off,
}

/// The size of the render target as a fraction of the window.
///
/// Held as a fraction rather than a percentage so the scaling arithmetic
/// is exact and a round trip through it cannot drift.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct RenderScale {
    numerator: u32,
    denominator: u32,
}

impl RenderScale {
    /// Render at the window's own resolution.
    pub const ONE: Self = Self {
        numerator: 1,
        denominator: 1,
    };

    /// The numerator of the fraction.
    #[must_use]
    pub const fn numerator(self) -> u32 {
        self.numerator
    }

    /// The denominator of the fraction.
    #[must_use]
    pub const fn denominator(self) -> u32 {
        self.denominator
    }

    /// Whether the render target is the window's own size, so the present
    /// needs no resample at all.
    #[must_use]
    pub const fn is_native(self) -> bool {
        self.numerator == self.denominator
    }

    /// `length` scaled down, never to zero.
    ///
    /// A render target of no pixels is not a degradation, it is a blank
    /// window, so the floor is one pixel.
    #[must_use]
    pub fn apply(self, length: u32) -> u32 {
        let scaled = u64::from(length) * u64::from(self.numerator) / u64::from(self.denominator);
        u32::try_from(scaled).unwrap_or(u32::MAX).max(1)
    }
}

/// One notch per octave the material synthesis can shed.
const MATERIAL_NOTCHES: u8 = {
    #[allow(
        clippy::cast_possible_truncation,
        reason = "the synthesis caps its octaves at a single-digit count"
    )]
    {
        MAX_OCTAVES as u8
    }
};

/// The notches of each rung, in the order they are shed.
///
/// Read once by [`Ladder`] to place a step on a rung, so the order lives
/// in exactly one place and the rung boundaries cannot drift from the
/// knob values below.
const NOTCHES: [(Rung, u8); 5] = [
    (Rung::ParticleDensity, 3),
    (Rung::LightResolution, 2),
    (Rung::MaterialDetail, MATERIAL_NOTCHES),
    (Rung::ShadowSoftness, 2),
    (Rung::RenderScale, 3),
];

/// The render-scale fractions, coarsest last.
const RENDER_SCALES: [RenderScale; 3] = [
    RenderScale {
        numerator: 3,
        denominator: 4,
    },
    RenderScale {
        numerator: 2,
        denominator: 3,
    },
    RenderScale {
        numerator: 1,
        denominator: 2,
    },
];

/// Log2 of the light buffer's divisor at full quality.
///
/// The light pass accumulates at half resolution and upsamples even when
/// nothing has been shed: the buffer is low-frequency by nature and the
/// upsample is invisible, so this is the authored quality rather than the
/// first degradation.
const LIGHT_SHIFT_FULL: u32 = 1;

/// How far the renderer has fallen back.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct Ladder {
    step: u8,
}

impl Default for Ladder {
    fn default() -> Self {
        Self::FULL
    }
}

impl Ladder {
    /// Nothing shed.
    pub const FULL: Self = Self { step: 0 };

    /// Every notch of every rung shed.
    pub const MAX_STEP: u8 = {
        let mut total = 0;
        let mut i = 0;
        while i < NOTCHES.len() {
            total += NOTCHES[i].1;
            i += 1;
        }
        total
    };

    /// The ladder at `step`, clamped to the bottom.
    #[must_use]
    pub const fn new(step: u8) -> Self {
        Self {
            step: if step > Self::MAX_STEP {
                Self::MAX_STEP
            } else {
                step
            },
        }
    }

    /// How many notches have been shed.
    #[must_use]
    pub const fn step(self) -> u8 {
        self.step
    }

    /// One notch further back, or `None` when there is nothing left to
    /// shed.
    ///
    /// `None` is not a failure: it is the renderer having spent its whole
    /// ladder, which the caller reports rather than reaching for the frame
    /// rate.
    #[must_use]
    pub const fn shed(self) -> Option<Self> {
        if self.step < Self::MAX_STEP {
            Some(Self {
                step: self.step + 1,
            })
        } else {
            None
        }
    }

    /// One notch back toward full, or `None` at full.
    #[must_use]
    pub const fn restore(self) -> Option<Self> {
        if self.step > 0 {
            Some(Self {
                step: self.step - 1,
            })
        } else {
            None
        }
    }

    /// Which rung the last shed notch came off.
    #[must_use]
    pub fn rung(self) -> Rung {
        if self.step == 0 {
            return Rung::Full;
        }
        let mut remaining = self.step;
        for (rung, notches) in NOTCHES {
            if remaining <= notches {
                return rung;
            }
            remaining -= notches;
        }
        Rung::RenderScale
    }

    /// How many notches of `rung` have been shed.
    fn shed_on(self, want: Rung) -> u8 {
        let mut remaining = self.step;
        for (rung, notches) in NOTCHES {
            let taken = remaining.min(notches);
            if rung == want {
                return taken;
            }
            remaining -= taken;
        }
        0
    }

    /// Log2 of the divisor applied to the derived particle budget, or
    /// `None` once particles are shed entirely.
    #[must_use]
    pub fn particle_shift(self) -> Option<u32> {
        match self.shed_on(Rung::ParticleDensity) {
            0 => Some(0),
            1 => Some(1),
            2 => Some(2),
            _ => None,
        }
    }

    /// Log2 of the light buffer's divisor.
    #[must_use]
    pub fn light_shift(self) -> u32 {
        LIGHT_SHIFT_FULL + u32::from(self.shed_on(Rung::LightResolution))
    }

    /// The octave count the materials are synthesised at.
    #[must_use]
    pub fn material_quality(self) -> MaterialQuality {
        MaterialQuality::new(
            MAX_OCTAVES.saturating_sub(u32::from(self.shed_on(Rung::MaterialDetail))),
        )
    }

    /// How contact shadows are drawn.
    #[must_use]
    pub fn shadow(self) -> Shadow {
        match self.shed_on(Rung::ShadowSoftness) {
            0 => Shadow::Soft,
            1 => Shadow::Hard,
            _ => Shadow::Off,
        }
    }

    /// The render target's size as a fraction of the window.
    #[must_use]
    pub fn render_scale(self) -> RenderScale {
        match self.shed_on(Rung::RenderScale) {
            0 => RenderScale::ONE,
            n => RENDER_SCALES[(n as usize - 1).min(RENDER_SCALES.len() - 1)],
        }
    }
}

#[cfg(test)]
#[path = "quality_tests.rs"]
mod tests;
