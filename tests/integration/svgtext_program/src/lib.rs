//! The `svgtext` fixture's shared vocabulary: the command word it installs
//! under, the two drawings it renders, and the measurement record the
//! vertical's guest gate judges (`plans/SVG.md` S23/S24).
//!
//! Both sides read this module — the fixture emits the record, the guest
//! kernel's sink decodes and judges it, and `tools/xtask` types the command
//! word — so the three cannot drift into each other's silence.
//!
//! # What the measurement has to rule out
//!
//! A vertical that only witnessed "a render completed" would pass on a
//! decoder that drew no lettering at all. So the fixture renders two
//! drawings that are byte-identical but for the single character they
//! letter, and reports the ink each left. Real outlines make those two
//! numbers differ in the direction the characters do; a decoder drawing
//! nothing, a placeholder, or a fixed box makes them equal. The third
//! render repeats the first with no font seam at all and must be *refused*,
//! which is what pins the glyphs to the service rather than to anything the
//! decoder held already.

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

use tairix_log::{Event, EventId, Field, FieldValue, Level};

/// The command word the fixture bundle installs under, and the bare word the
/// vertical's script types at the shell.
pub const COMMAND: &str = "svgtext";

/// Record the fixture emits once it has measured every render.
pub const REPORT_EVENT: EventId = EventId(4513);

/// Record the fixture emits instead when a render it needed did not happen.
pub const REPORT_FAILED_EVENT: EventId = EventId(4514);

/// Message of the measurement record.
pub const REPORT_MESSAGE: &str = "svgtext sandboxed text render measured";

/// Message of the failure record. The guest gate fails the run on sight of
/// it rather than waiting out its budget for a measurement that will never
/// come.
pub const REPORT_FAILED_MESSAGE: &str = "svgtext could not measure a sandboxed text render";

/// Exit status of a run that could not measure.
pub const REPORT_FAILED_STATUS: i32 = 1;

/// The drawing whose glyph is wide.
///
/// It carries no `font-family`, so the decoder's ladder ends at the generic
/// every store answers and the *service* picks the family — the resolution
/// path a real document takes.
pub const WIDE_DOCUMENT: &str =
    r#"<svg viewBox="0 0 64 64"><text x="2" y="48" font-size="48">M</text></svg>"#;

/// The drawing whose glyph is narrow: [`WIDE_DOCUMENT`] with one character
/// changed and nothing else, so a difference between the two renders can
/// only have come from glyph geometry.
pub const NARROW_DOCUMENT: &str =
    r#"<svg viewBox="0 0 64 64"><text x="2" y="48" font-size="48">I</text></svg>"#;

/// Field keys of the measurement record, in field order.
pub mod field {
    /// [`super::Report::wide_ink`].
    pub const WIDE_INK: &str = "wide_ink";
    /// [`super::Report::narrow_ink`].
    pub const NARROW_INK: &str = "narrow_ink";
    /// [`super::Report::pixels`].
    pub const PIXELS: &str = "pixels";
    /// [`super::Report::refused_without_fonts`].
    pub const REFUSED_WITHOUT_FONTS: &str = "refused_without_fonts";
}

/// What one run of the fixture measured.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct Report {
    /// Destination pixels [`WIDE_DOCUMENT`]'s render left non-transparent.
    pub wide_ink: u64,
    /// Destination pixels [`NARROW_DOCUMENT`]'s render left non-transparent.
    pub narrow_ink: u64,
    /// Pixels in one render, so the gate judges a solid fill against the
    /// record's own total rather than a constant restated beside it.
    pub pixels: u64,
    /// Whether [`WIDE_DOCUMENT`] through a sandbox with no font seam was
    /// refused for want of glyphs.
    pub refused_without_fonts: bool,
}

impl Report {
    /// The record's fields, ready to log.
    #[must_use]
    pub fn fields(&self) -> [Field<'static>; 4] {
        [
            Field {
                key: field::WIDE_INK,
                value: FieldValue::UnsignedInt(self.wide_ink),
            },
            Field {
                key: field::NARROW_INK,
                value: FieldValue::UnsignedInt(self.narrow_ink),
            },
            Field {
                key: field::PIXELS,
                value: FieldValue::UnsignedInt(self.pixels),
            },
            Field {
                key: field::REFUSED_WITHOUT_FONTS,
                value: FieldValue::UnsignedInt(u64::from(self.refused_without_fonts)),
            },
        ]
    }

    /// The record to log for this measurement.
    #[must_use]
    pub const fn event<'a>(fields: &'a [Field<'a>]) -> Event<'a> {
        Event {
            level: Level::Info,
            id: REPORT_EVENT,
            message: REPORT_MESSAGE,
            fields,
        }
    }

    /// Decode a measurement from an emitted record, or `None` when `event`
    /// is not one — a foreign id, a missing counter, or a flag that is
    /// neither `0` nor `1`.
    ///
    /// Fails closed on a partial record: defaulting an absent counter would
    /// turn a truncated record into a verdict.
    #[must_use]
    pub fn from_event(event: &Event<'_>) -> Option<Self> {
        if event.id != REPORT_EVENT {
            return None;
        }
        let read = |key: &str| {
            event.fields.iter().find_map(|f| match f.value {
                FieldValue::UnsignedInt(v) if f.key == key => Some(v),
                _ => None,
            })
        };
        let refused = match read(field::REFUSED_WITHOUT_FONTS)? {
            0 => false,
            1 => true,
            _ => return None,
        };
        Some(Self {
            wide_ink: read(field::WIDE_INK)?,
            narrow_ink: read(field::NARROW_INK)?,
            pixels: read(field::PIXELS)?,
            refused_without_fonts: refused,
        })
    }

    /// Judge this measurement.
    ///
    /// The one definition of the rule: the fixture reads it for its own exit
    /// status and stderr line, and the guest gate reads it to decide the run.
    #[must_use]
    pub const fn verdict(&self) -> Verdict {
        if self.pixels == 0 {
            return Verdict::NoDestination;
        }
        if !self.refused_without_fonts {
            return Verdict::GlyphsNotFromTheService;
        }
        if self.wide_ink == 0 || self.narrow_ink == 0 {
            return Verdict::NothingDrawn;
        }
        if self.wide_ink >= self.pixels {
            return Verdict::SolidFill;
        }
        if self.wide_ink <= self.narrow_ink {
            return Verdict::GlyphsNotDistinct;
        }
        Verdict::Accepted
    }
}

/// What judging a [`Report`] came to.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Verdict {
    /// Real, character-dependent outlines were drawn, and only the service
    /// could have supplied them.
    Accepted,
    /// The record describes a render with no pixels in it, so nothing it
    /// says about ink means anything.
    NoDestination,
    /// The same drawing decoded with no font seam produced a picture
    /// instead of a refusal, so its lettering need not have come from the
    /// font service at all.
    GlyphsNotFromTheService,
    /// A render left no ink: the document's only content is its text, so a
    /// blank render is a decoder that drew no lettering.
    NothingDrawn,
    /// The render is entirely ink — a fill, not lettering.
    SolidFill,
    /// The wide and narrow drawings inked the same, so whatever was drawn
    /// does not depend on the character: a placeholder, not an outline.
    GlyphsNotDistinct,
}

impl Verdict {
    /// Whether the measurement holds.
    #[must_use]
    pub const fn held(self) -> bool {
        matches!(self, Self::Accepted)
    }

    /// A stable word for the transcript.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::NoDestination => "no_destination",
            Self::GlyphsNotFromTheService => "glyphs_not_from_the_service",
            Self::NothingDrawn => "nothing_drawn",
            Self::SolidFill => "solid_fill",
            Self::GlyphsNotDistinct => "glyphs_not_distinct",
        }
    }
}

#[cfg(test)]
mod tests {
    extern crate alloc;

    use alloc::vec::Vec;

    use super::{
        Report, Verdict, NARROW_DOCUMENT, REPORT_EVENT, REPORT_FAILED_EVENT, WIDE_DOCUMENT,
    };
    use tairix_log::{Event, Field, FieldValue, Level};
    use tairix_svg::font::NoFonts;
    use tairix_svg::{SvgError, Viewport};

    /// A measurement of the shape a passing run produces.
    const fn measured() -> Report {
        Report {
            wide_ink: 520,
            narrow_ink: 170,
            pixels: 64 * 64,
            refused_without_fonts: true,
        }
    }

    /// The two drawings differ in exactly one byte — the character — so a
    /// difference between their renders can only be glyph geometry. A
    /// second difference creeping in would make the whole discriminator
    /// unsound, which is why it is pinned rather than eyeballed.
    #[test]
    fn the_two_drawings_differ_only_in_the_character_they_letter() {
        assert_eq!(WIDE_DOCUMENT.len(), NARROW_DOCUMENT.len());
        let differing = WIDE_DOCUMENT
            .bytes()
            .zip(NARROW_DOCUMENT.bytes())
            .filter(|(wide, narrow)| wide != narrow)
            .count();
        assert_eq!(differing, 1);
    }

    /// Both drawings genuinely demand a face: decoded with no font seam
    /// they are refused, never drawn blank. A document that could be drawn
    /// without glyphs would let the vertical pass while measuring nothing.
    #[test]
    fn neither_drawing_can_be_decoded_without_a_font() {
        for document in [WIDE_DOCUMENT, NARROW_DOCUMENT] {
            assert_eq!(
                tairix_svg::decode(document.as_bytes(), Viewport::Natural, &mut NoFonts).err(),
                Some(SvgError::FontUnavailable),
                "{document} must demand a face",
            );
        }
    }

    #[test]
    fn a_measurement_round_trips_through_its_own_record() {
        let report = measured();
        let fields = report.fields();
        let event = Report::event(&fields);
        assert_eq!(event.id, REPORT_EVENT);
        assert_eq!(Report::from_event(&event), Some(report));
    }

    #[test]
    fn a_foreign_record_is_not_a_measurement() {
        let fields = measured().fields();
        let foreign = Event {
            level: Level::Info,
            id: REPORT_FAILED_EVENT,
            message: "something else",
            fields: &fields,
        };
        assert_eq!(Report::from_event(&foreign), None);
    }

    #[test]
    fn a_record_missing_a_counter_decodes_to_nothing() {
        let all = measured().fields();
        for drop in 0..all.len() {
            let mut short: Vec<Field<'_>> = all.to_vec();
            short.remove(drop);
            let event = Report::event(&short);
            assert_eq!(
                Report::from_event(&event),
                None,
                "field {} absent must refuse the whole record",
                all[drop].key
            );
        }
    }

    #[test]
    fn a_counter_of_the_wrong_shape_decodes_to_nothing() {
        let mut fields: Vec<Field<'_>> = measured().fields().to_vec();
        fields[0].value = FieldValue::Str("520");
        let event = Report::event(&fields);
        assert_eq!(Report::from_event(&event), None);
    }

    /// The flag is a flag: a value that is neither state is a record this
    /// decoder cannot believe, not a truthy one.
    #[test]
    fn a_flag_that_is_neither_state_decodes_to_nothing() {
        let mut fields: Vec<Field<'_>> = measured().fields().to_vec();
        fields[3].value = FieldValue::UnsignedInt(2);
        let event = Report::event(&fields);
        assert_eq!(Report::from_event(&event), None);
    }

    #[test]
    fn a_complete_measurement_is_accepted() {
        assert_eq!(measured().verdict(), Verdict::Accepted);
        assert!(measured().verdict().held());
    }

    #[test]
    fn each_way_the_measurement_can_fail_has_its_own_verdict() {
        let cases = [
            (
                Report {
                    pixels: 0,
                    ..measured()
                },
                Verdict::NoDestination,
            ),
            (
                Report {
                    refused_without_fonts: false,
                    ..measured()
                },
                Verdict::GlyphsNotFromTheService,
            ),
            (
                Report {
                    wide_ink: 0,
                    ..measured()
                },
                Verdict::NothingDrawn,
            ),
            (
                Report {
                    narrow_ink: 0,
                    ..measured()
                },
                Verdict::NothingDrawn,
            ),
            (
                Report {
                    wide_ink: 64 * 64,
                    ..measured()
                },
                Verdict::SolidFill,
            ),
            (
                Report {
                    narrow_ink: 520,
                    ..measured()
                },
                Verdict::GlyphsNotDistinct,
            ),
        ];
        for (report, expected) in cases {
            assert_eq!(report.verdict(), expected, "{report:?}");
            assert!(!report.verdict().held());
        }
    }

    /// Every verdict has its own word, so a failing transcript names which
    /// expectation it missed rather than one of two.
    #[test]
    fn every_verdict_is_spelled_distinctly() {
        let all = [
            Verdict::Accepted,
            Verdict::NoDestination,
            Verdict::GlyphsNotFromTheService,
            Verdict::NothingDrawn,
            Verdict::SolidFill,
            Verdict::GlyphsNotDistinct,
        ];
        for (index, verdict) in all.iter().enumerate() {
            assert!(!verdict.as_str().is_empty());
            assert!(
                !all[..index]
                    .iter()
                    .any(|held| held.as_str() == verdict.as_str()),
                "{} is spelled twice",
                verdict.as_str()
            );
        }
    }
}
