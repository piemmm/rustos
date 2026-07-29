//! The fonts a theme selects, one per *text role*.
//!
//! A theme names text by the job it does — a panel heading, a list item's
//! title, its secondary detail line, a column header, a metric readout — not
//! by the widget that draws it. Each role resolves to a [`FontSpec`]: the
//! family name of an installed face under `/System/Fonts` plus a size and a
//! weight. This crate stores the reference, it does not rasterise glyphs.
//!
//! Sizes are *logical* pixels at the reference density
//! (`tairix_geometry::REFERENCE_DPI`); the desktop's DPI / UI scale
//! (`tairix_geometry::Scale`) converts a size to physical pixels when a face
//! is rasterised, so text stays a comfortable physical size across panel
//! densities.
//!
//! # One ladder, derived from one base size
//!
//! The design boards (`plans/desktop1.png`, `plans/desktop2a.png`) carry their
//! hierarchy with a deliberately *tight* size ladder and a rising weight: a
//! secondary detail line is a little smaller than the item title above it, a
//! column header is smaller still but bold, and a panel heading is a step
//! larger. Every role therefore states its size as a percentage of the one
//! authored base size ([`Fonts::ladder`]) rather than as an independent
//! number, so the whole desktop's type scales together and no two roles can
//! silently drift apart.

use alloc::string::String;

/// The weight a text role is set in.
///
/// This is the font service's own weight type, re-exported rather than
/// restated: the weight a theme names is exactly the value a glyph request
/// carries, so there is one definition for both. The shipped faces are
/// Regular-only, so the heavier weights are synthesised by the service as a
/// bounded thickening of the same outline coverage, leaving the advance — and
/// therefore every layout — unchanged.
pub use tairix_abi::font_ipc::FontWeight;

/// The job a run of text does, which is what a theme sizes and weights.
///
/// The set is closed: a widget picks the role whose *job* matches, and the
/// theme decides how that job looks. Adding a treatment is retuning a role,
/// never a new size literal at a draw site.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum TextRole {
    /// A panel or dialog heading — the largest text in a surface.
    Heading,
    /// The primary line of a list item, task, or job: an app or file name.
    ItemTitle,
    /// A window or panel title in furniture (a title bar, a panel's
    /// wordmark).
    WindowTitle,
    /// Ordinary interface text: button labels, menu rows, fields, list rows.
    Body,
    /// A numeric readout beside a meter — a percentage, a byte count, a rate.
    Metric,
    /// The secondary line under an item title, a clock, or any de-emphasised
    /// annotation.
    Caption,
    /// A column or group header over a list, set in bold.
    SectionHeader,
    /// Fixed-width text: the terminal, a log or code view.
    Monospace,
}

impl TextRole {
    /// Every role, in descending nominal size — the order the ladder is
    /// authored and tested in.
    pub const ALL: [Self; 8] = [
        Self::Heading,
        Self::ItemTitle,
        Self::WindowTitle,
        Self::Body,
        Self::Metric,
        Self::Caption,
        Self::SectionHeader,
        Self::Monospace,
    ];

    /// This role's rung in the ladder, which is also its slot in a
    /// [`Fonts`] table.
    ///
    /// A direct index keeps a text draw's font lookup a constant-time array
    /// read rather than a search, and the mapping is total, so no lookup can
    /// miss.
    const fn index(self) -> usize {
        match self {
            Self::Heading => 0,
            Self::ItemTitle => 1,
            Self::WindowTitle => 2,
            Self::Body => 3,
            Self::Metric => 4,
            Self::Caption => 5,
            Self::SectionHeader => 6,
            Self::Monospace => 7,
        }
    }
}

/// A reference to one font face at one size and weight.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FontSpec {
    /// Family name of an installed face under `/System/Fonts`.
    pub family: String,
    /// Nominal size in logical pixels at the reference density (scaled to
    /// physical pixels by `tairix_geometry::Scale`).
    pub size_px: u16,
    /// Face weight.
    pub weight: FontWeight,
}

impl FontSpec {
    /// A font specification from its parts.
    #[must_use]
    pub fn new(family: impl Into<String>, size_px: u16, weight: FontWeight) -> Self {
        Self {
            family: family.into(),
            size_px,
            weight,
        }
    }
}

/// One rung of the ladder: a role's size as a percentage of the base size,
/// and the weight the boards set it in.
struct Rung {
    role: TextRole,
    /// Size as a percentage of [`Fonts::base_size_px`], read off the boards.
    percent: u32,
    weight: FontWeight,
}

/// The boards' ladder: the size percentage and weight of every role relative
/// to the authored base (body) size.
///
/// The percentages are measured from the reference boards, where a button
/// label, an item title, and its detail line sit within one point of each
/// other and the weight — not the size — carries most of the hierarchy.
const LADDER: [Rung; 8] = [
    Rung {
        role: TextRole::Heading,
        percent: 133,
        weight: FontWeight::Medium,
    },
    Rung {
        role: TextRole::ItemTitle,
        percent: 113,
        weight: FontWeight::Medium,
    },
    Rung {
        role: TextRole::WindowTitle,
        percent: 100,
        weight: FontWeight::Medium,
    },
    Rung {
        role: TextRole::Body,
        percent: 100,
        weight: FontWeight::Regular,
    },
    Rung {
        role: TextRole::Metric,
        percent: 100,
        weight: FontWeight::Bold,
    },
    Rung {
        role: TextRole::Caption,
        percent: 87,
        weight: FontWeight::Regular,
    },
    Rung {
        role: TextRole::SectionHeader,
        percent: 80,
        weight: FontWeight::Bold,
    },
    Rung {
        role: TextRole::Monospace,
        percent: 100,
        weight: FontWeight::Regular,
    },
];

/// The fonts a theme provides, one [`FontSpec`] per [`TextRole`].
///
/// Build one with [`Fonts::ladder`]: it derives every role from a single base
/// size through the boards' one shared ladder, so a theme authors *one*
/// number and the whole scale follows.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Fonts {
    ui_family: String,
    monospace_family: String,
    base_size_px: u16,
    specs: [FontSpec; 8],
}

impl Fonts {
    /// The smallest base size a ladder may be authored at, in logical pixels.
    ///
    /// The smallest rung is a fraction of the base, and text below the
    /// rasteriser's floor loses the strokes that distinguish one glyph from
    /// another, so a ladder is never authored under this.
    pub const MIN_BASE_SIZE_PX: u16 = 12;

    /// The largest base size a ladder may be authored at, in logical pixels.
    ///
    /// The tallest rung is a third larger again; this bound keeps even that
    /// rung within the rasteriser's cell-height ceiling at a high DPI scale.
    pub const MAX_BASE_SIZE_PX: u16 = 96;

    /// The ladder for `base_size_px` logical pixels of body text, drawn in
    /// `ui_family` with `monospace_family` for the fixed-width role.
    ///
    /// The base size is clamped into
    /// [`MIN_BASE_SIZE_PX`](Self::MIN_BASE_SIZE_PX)..=[`MAX_BASE_SIZE_PX`](Self::MAX_BASE_SIZE_PX),
    /// so a theme cannot author text too small to read or too large to
    /// rasterise.
    #[must_use]
    pub fn ladder(
        ui_family: impl Into<String>,
        monospace_family: impl Into<String>,
        base_size_px: u16,
    ) -> Self {
        let ui_family = ui_family.into();
        let monospace_family = monospace_family.into();
        let base = base_size_px.clamp(Self::MIN_BASE_SIZE_PX, Self::MAX_BASE_SIZE_PX);
        let specs = core::array::from_fn(|i| {
            let rung = &LADDER[i];
            let family = if matches!(rung.role, TextRole::Monospace) {
                &monospace_family
            } else {
                &ui_family
            };
            FontSpec::new(family.clone(), rung_size(base, rung.percent), rung.weight)
        });
        Self {
            ui_family,
            monospace_family,
            base_size_px: base,
            specs,
        }
    }

    /// The specification for `role`.
    #[must_use]
    pub fn spec(&self, role: TextRole) -> &FontSpec {
        &self.specs[role.index()]
    }

    /// The family every non-monospace role is drawn in.
    #[must_use]
    pub fn ui_family(&self) -> &str {
        &self.ui_family
    }

    /// The family the [`TextRole::Monospace`] role is drawn in.
    #[must_use]
    pub fn monospace_family(&self) -> &str {
        &self.monospace_family
    }

    /// The authored base (body) size in logical pixels, from which every rung
    /// of the ladder derives.
    #[must_use]
    pub fn base_size_px(&self) -> u16 {
        self.base_size_px
    }
}

/// A rung's size in logical pixels: `percent` of `base`, rounded to the
/// nearest whole pixel and never below one.
fn rung_size(base: u16, percent: u32) -> u16 {
    let scaled = (u32::from(base) * percent + 50) / 100;
    u16::try_from(scaled.max(1)).unwrap_or(u16::MAX)
}
