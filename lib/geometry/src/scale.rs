//! The desktop DPI / UI scale factor (`lib/geometry`).
//!
//! RustOS authors every desktop length — theme corner radii and border
//! thicknesses, font sizes, taskbar extents, window chrome — in *logical*
//! pixels at a fixed reference density ([`REFERENCE_DPI`]). A display panel's
//! real pixel density varies wildly (a low-DPI monitor versus a high-DPI
//! laptop), so the desktop holds one settable [`Scale`] and converts logical
//! pixels into the panel's *physical* pixels at layout and render time. The
//! result is a UI that stays a comfortable physical size on any panel while
//! the user picks the density that suits them.
//!
//! [`Scale`] is the single shared definition of that conversion, so the same
//! logical→physical arithmetic is never written twice across the window
//! manager, the taskbar, the cursors, and the apps (`AGENTS.md` §2.2). It
//! lives in `lib/geometry` because scaling a length is geometry, and the
//! geometry crate sits at the bottom of the §17.4 layering where every GUI
//! consumer can reach it.
//!
//! All arithmetic widens through `u64` and saturates, so a pathological scale
//! or length fails closed rather than wrapping (`AGENTS.md` §2.9), and an
//! out-of-range scale is rejected at construction rather than producing a
//! degenerate desktop.

/// The reference display density, in dots per inch, at which one logical
/// pixel is exactly one physical pixel — i.e. the density a [`Scale`] of
/// `100`% describes. Logical desktop lengths are authored against this
/// density.
pub const REFERENCE_DPI: u32 = 96;

/// A desktop UI scale factor: the ratio of physical pixels to logical
/// (design) pixels, expressed as a percentage.
///
/// `100` is 1:1 (the reference density); `200` renders every logical pixel as
/// a 2×2 physical block for a high-DPI panel; `150` is a comfortable middle
/// setting. The percentage is the canonical form the renderers consume; a
/// target density in DPI is converted to and from it with [`from_dpi`] and
/// [`dpi`].
///
/// A `Scale` is always valid: its percentage is constrained to
/// [`MIN_PERCENT`]..=[`MAX_PERCENT`] at construction, so it can never be zero
/// (which would erase the desktop) or so large that scaling overflows.
///
/// [`from_dpi`]: Scale::from_dpi
/// [`dpi`]: Scale::dpi
/// [`MIN_PERCENT`]: Scale::MIN_PERCENT
/// [`MAX_PERCENT`]: Scale::MAX_PERCENT
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub struct Scale {
    percent: u32,
}

impl Scale {
    /// The unscaled 1:1 factor (100%), where a logical pixel is a physical
    /// pixel.
    pub const ONE: Self = Self { percent: 100 };

    /// The smallest permitted scale (25%). A smaller factor would render the
    /// desktop unusably tiny and is rejected by [`from_percent`].
    ///
    /// [`from_percent`]: Self::from_percent
    pub const MIN_PERCENT: u32 = 25;

    /// The largest permitted scale (800%). A larger factor risks overflowing
    /// physical dimensions and is rejected by [`from_percent`].
    ///
    /// [`from_percent`]: Self::from_percent
    pub const MAX_PERCENT: u32 = 800;

    /// A scale from a percentage of the reference density, or `None` if
    /// `percent` is outside
    /// [`MIN_PERCENT`](Self::MIN_PERCENT)..=[`MAX_PERCENT`](Self::MAX_PERCENT).
    ///
    /// Rejecting an out-of-range value rather than clamping it keeps a
    /// caller's bad input visible instead of silently substituting a
    /// different desktop scale (`AGENTS.md` §5.4 / §2.9).
    #[must_use]
    pub const fn from_percent(percent: u32) -> Option<Self> {
        if percent < Self::MIN_PERCENT || percent > Self::MAX_PERCENT {
            return None;
        }
        Some(Self { percent })
    }

    /// A scale that renders the desktop at `dpi` dots per inch relative to
    /// [`REFERENCE_DPI`], or `None` if the implied percentage is out of range.
    ///
    /// This is how a "comfortable DPI" the user picks becomes a scale factor:
    /// `from_dpi(192)` on the 96-DPI reference yields a 200% scale.
    #[must_use]
    pub fn from_dpi(dpi: u32) -> Option<Self> {
        let percent = u64::from(dpi) * 100 / u64::from(REFERENCE_DPI);
        Self::from_percent(u32::try_from(percent).ok()?)
    }

    /// The scale as a percentage of the reference density.
    #[must_use]
    pub const fn percent(self) -> u32 {
        self.percent
    }

    /// The display density this scale describes, in dots per inch relative to
    /// [`REFERENCE_DPI`]. The inverse of [`from_dpi`](Self::from_dpi),
    /// rounded down to a whole DPI.
    #[must_use]
    pub fn dpi(self) -> u32 {
        let dpi = u64::from(REFERENCE_DPI) * u64::from(self.percent) / 100;
        u32::try_from(dpi).unwrap_or(u32::MAX)
    }

    /// Convert a `logical` pixel length into physical pixels at this scale.
    ///
    /// The product widens through `u64` and saturates at [`u32::MAX`], so an
    /// extreme length scales to a clamped maximum rather than wrapping
    /// (`AGENTS.md` §2.9).
    #[must_use]
    pub fn scale_length(self, logical: u32) -> u32 {
        let physical = u64::from(logical) * u64::from(self.percent) / 100;
        u32::try_from(physical).unwrap_or(u32::MAX)
    }
}

impl Default for Scale {
    /// The unscaled 1:1 desktop ([`Scale::ONE`]).
    fn default() -> Self {
        Self::ONE
    }
}
