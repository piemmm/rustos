//! The SVG decoder's font seam, backed by the OS font service.
//!
//! `lib/svg` holds no font data and no authority: it resolves a face and
//! fetches glyph geometry through an injected provider
//! (`tairix_svg::font::FontProvider`). This is that provider for a process
//! that already talks to `fontd` — the desktop chrome, and the host side of
//! the sandboxed document decode — so there is exactly one adapter between
//! the decoder's seam and the endpoint rather than one per consumer.
//!
//! # A face is resolved by asking for a glyph
//!
//! The seam needs the face's *font-unit* geometry before any layout, and the
//! outline reply's own header carries exactly that. Selecting a face is
//! therefore one outline request for the space, whose answer is both the
//! family's metrics and a cache entry the run is very likely to want anyway.
//! An unknown family is refused by the service, which is what lets the
//! decoder's ladder move on to the next name it was given.

use alloc::vec::Vec;

use tairix_abi::font_ipc::{FamilyKey, FontStretch, FontStyle, FontWeight};
use tairix_svg::font::{
    FaceId, FaceMetrics, FaceRequest, FontProvider, FontStyle as SvgStyle, FontUnavailable,
    GlyphOutline, OutlineContour, OutlineSegment,
};

use crate::client::{OutlineReply, OwnedContour, OwnedOutline, OwnedSegment};

/// The scalar a face is probed with, chosen because every face maps it and
/// a run of text almost always needs it.
const PROBE: char = ' ';

/// The most faces one provider will resolve.
///
/// A fixed containment bound: the decoder's own family ladder is bounded, so
/// a document that walks past this has been asking for faces rather than
/// drawing text.
const MAX_FACES: usize = 32;

/// One face this provider has resolved, and the request that named it.
#[derive(Clone, Copy, Debug)]
struct Resolved {
    family: FamilyKey,
    weight: FontWeight,
    style: FontStyle,
    stretch: FontStretch,
    metrics: FaceMetrics,
}

/// The SVG font seam over `FONT_ENDPOINT`.
///
/// Holds only the faces it has resolved; the glyphs themselves are memoised
/// in the process's own outline cache ([`crate::set_outline_cache`]), so two
/// documents drawn one after the other share what they fetched.
#[derive(Debug, Default)]
pub struct ServiceFonts {
    faces: Vec<Resolved>,
}

impl ServiceFonts {
    /// A provider that has resolved nothing yet.
    #[must_use]
    pub const fn new() -> Self {
        Self { faces: Vec::new() }
    }

    /// The face `id` names, if this provider issued it.
    fn face(&self, id: FaceId) -> Option<Resolved> {
        self.faces.get(id.get() as usize).copied()
    }
}

impl FontProvider for ServiceFonts {
    fn select(&mut self, req: &FaceRequest<'_>) -> Result<FaceMetrics, FontUnavailable> {
        let family = FamilyKey::new(req.family).map_err(|_| FontUnavailable)?;
        let weight = FontWeight::new(req.weight).map_err(|_| FontUnavailable)?;
        let style = wire_style(req.style);
        let stretch = FontStretch::new(req.stretch).map_err(|_| FontUnavailable)?;
        if let Some(held) = self.faces.iter().find(|held| {
            held.family == family
                && held.weight == weight
                && held.style == style
                && held.stretch == stretch
        }) {
            return Ok(held.metrics);
        }
        if self.faces.len() >= MAX_FACES {
            return Err(FontUnavailable);
        }
        // A face is not selectable without the geometry a run is laid out
        // in, and the outline reply's header is where the service states it.
        let reply =
            crate::outline_run(&[PROBE], family, weight, style, stretch).ok_or(FontUnavailable)?;
        let id = FaceId::new(u32::try_from(self.faces.len()).map_err(|_| FontUnavailable)?);
        let metrics = FaceMetrics {
            id,
            units_per_em: f64::from(reply.units_per_em),
            ascent: f64::from(reply.ascent),
            descent: f64::from(reply.descent),
            line_gap: f64::from(reply.line_gap),
        };
        if !metrics.is_usable() {
            return Err(FontUnavailable);
        }
        self.faces.push(Resolved {
            family,
            weight,
            style,
            stretch,
            metrics,
        });
        Ok(metrics)
    }

    fn outlines(
        &mut self,
        face: FaceId,
        run: &[char],
        out: &mut Vec<GlyphOutline>,
    ) -> Result<(), FontUnavailable> {
        let resolved = self.face(face).ok_or(FontUnavailable)?;
        // `outline_run` already serves the whole run, asking the service
        // again for whatever one reply's frame could not carry, so there is
        // no second prefix loop here.
        let reply = crate::outline_run(
            run,
            resolved.family,
            resolved.weight,
            resolved.style,
            resolved.stretch,
        )
        .ok_or(FontUnavailable)?;
        if reply.glyphs.len() != run.len() {
            return Err(FontUnavailable);
        }
        out.extend(reply.glyphs.into_iter().map(seam_outline));
        Ok(())
    }
}

/// The wire posture an SVG one names.
const fn wire_style(style: SvgStyle) -> FontStyle {
    match style {
        SvgStyle::Normal => FontStyle::Normal,
        SvgStyle::Italic => FontStyle::Italic,
        SvgStyle::Oblique => FontStyle::Oblique,
    }
}

/// Convert one fetched outline into the decoder seam's own form.
fn seam_outline(glyph: OwnedOutline) -> GlyphOutline {
    GlyphOutline {
        units_per_em: f64::from(glyph.units_per_em),
        advance: glyph.advance,
        synthetic_bold: glyph.synthetic_bold,
        synthetic_shear: glyph.synthetic_shear,
        contours: glyph.contours.into_iter().map(seam_contour).collect(),
    }
}

/// Convert one fetched contour into the decoder seam's own form.
fn seam_contour(contour: OwnedContour) -> OutlineContour {
    OutlineContour {
        start: contour.start,
        segments: contour
            .segments
            .into_iter()
            .map(|segment| match segment {
                OwnedSegment::Line { to } => OutlineSegment::Line { to },
                OwnedSegment::Quadratic { control, to } => {
                    OutlineSegment::Quadratic { control, to }
                }
            })
            .collect(),
    }
}

/// The reply a provider turns into seam outlines, exposed for the host-side
/// two-phase exchange that fetches on a sandboxed decoder's behalf.
#[must_use]
pub fn seam_outlines(reply: OutlineReply) -> Vec<GlyphOutline> {
    reply.glyphs.into_iter().map(seam_outline).collect()
}

#[cfg(test)]
mod tests {
    use super::{seam_outline, wire_style, ServiceFonts, MAX_FACES};
    use crate::client::{OwnedContour, OwnedOutline, OwnedSegment};
    use alloc::vec;
    use tairix_abi::font_ipc::FontStyle;
    use tairix_svg::font::{FaceId, FontProvider, FontStyle as SvgStyle, OutlineSegment};

    #[test]
    fn a_posture_crosses_the_seam_unchanged() {
        assert_eq!(wire_style(SvgStyle::Normal), FontStyle::Normal);
        assert_eq!(wire_style(SvgStyle::Italic), FontStyle::Italic);
        assert_eq!(wire_style(SvgStyle::Oblique), FontStyle::Oblique);
    }

    #[test]
    fn a_fetched_outline_keeps_its_quadratics_whole() {
        let converted = seam_outline(OwnedOutline {
            units_per_em: 2048,
            advance: 600.0,
            synthetic_bold: 0.04,
            synthetic_shear: 0.25,
            contours: vec![OwnedContour {
                start: (1.0, 2.0),
                segments: vec![
                    OwnedSegment::Line { to: (3.0, 4.0) },
                    OwnedSegment::Quadratic {
                        control: (5.0, 6.0),
                        to: (7.0, 8.0),
                    },
                ],
            }],
        });
        assert!((converted.units_per_em - 2048.0).abs() < f64::EPSILON);
        assert!((converted.synthetic_shear - 0.25).abs() < f64::EPSILON);
        assert_eq!(
            converted.contours[0].segments[1],
            OutlineSegment::Quadratic {
                control: (5.0, 6.0),
                to: (7.0, 8.0),
            }
        );
    }

    #[test]
    fn an_unissued_face_id_is_refused_rather_than_indexed() {
        let mut provider = ServiceFonts::new();
        let mut out = alloc::vec::Vec::new();
        assert!(provider.outlines(FaceId::new(0), &['a'], &mut out).is_err());
        assert!(provider
            .outlines(FaceId::new(u32::MAX), &['a'], &mut out)
            .is_err());
        assert!(out.is_empty());
    }

    #[test]
    fn the_face_bound_refuses_rather_than_growing() {
        // Nothing here reaches the service, so every `select` fails at the
        // fetch — what is under test is that the bound is checked *before*
        // the table grows, so a hostile document cannot make a provider
        // hold more faces than the bound names.
        let mut provider = ServiceFonts::new();
        for index in 0..=MAX_FACES {
            let name = alloc::format!("family{index}");
            let request = tairix_svg::font::FaceRequest {
                family: &name,
                weight: 400,
                style: SvgStyle::Normal,
                stretch: 10_000,
            };
            assert!(provider.select(&request).is_err());
        }
    }
}
