//! Unit tests for the injected font seam: what a provider must answer, and
//! what the decoder refuses it.

use alloc::vec;
use alloc::vec::Vec;

use super::{
    FaceId, FaceMetrics, FaceRequest, FontProvider, FontStyle, FontUnavailable, GlyphOutline,
    NoFonts, OutlineContour, OutlineSegment, MAX_GLYPH_POINTS,
};

/// Metrics that can be laid text out with.
fn usable() -> FaceMetrics {
    FaceMetrics {
        id: FaceId::new(1),
        units_per_em: 1000.0,
        ascent: 800.0,
        descent: 200.0,
        line_gap: 0.0,
    }
}

/// An outline of one square contour.
fn square() -> GlyphOutline {
    GlyphOutline {
        units_per_em: 1000.0,
        advance: 500.0,
        synthetic_bold: 0.0,
        synthetic_shear: 0.0,
        contours: vec![OutlineContour {
            start: (0.0, 0.0),
            segments: vec![
                OutlineSegment::Line { to: (500.0, 0.0) },
                OutlineSegment::Line { to: (500.0, 700.0) },
                OutlineSegment::Line { to: (0.0, 700.0) },
                OutlineSegment::Line { to: (0.0, 0.0) },
            ],
        }],
    }
}

#[test]
fn a_face_id_is_the_providers_own_name_and_nothing_more() {
    assert_eq!(FaceId::new(7).get(), 7);
}

#[test]
fn metrics_with_no_em_cannot_be_laid_out_with() {
    assert!(usable().is_usable());
    for broken in [0.0, -1.0, f64::NAN, f64::INFINITY] {
        let face = FaceMetrics {
            units_per_em: broken,
            ..usable()
        };
        assert!(!face.is_usable(), "{broken} passed as an em");
    }
}

#[test]
fn a_non_finite_vertical_metric_is_not_usable() {
    for broken in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        assert!(
            !FaceMetrics {
                ascent: broken,
                ..usable()
            }
            .is_usable(),
            "{broken} passed as an ascent"
        );
        assert!(!FaceMetrics {
            descent: broken,
            ..usable()
        }
        .is_usable());
        assert!(!FaceMetrics {
            line_gap: broken,
            ..usable()
        }
        .is_usable());
    }
}

#[test]
fn a_glyph_counts_its_contours_and_its_segments() {
    // One contour start plus its four segments.
    assert_eq!(square().points(), 5);
    assert_eq!(GlyphOutline::default().points(), 0);
}

#[test]
fn a_drawable_glyph_is_finite_throughout_and_within_the_point_bound() {
    assert!(square().is_drawable());

    for broken in [f64::NAN, f64::INFINITY] {
        let mut glyph = square();
        glyph.contours[0].segments[1] = OutlineSegment::Line { to: (broken, 0.0) };
        assert!(!glyph.is_drawable(), "a {broken} end point was admitted");

        let mut control = square();
        control.contours[0].segments[0] = OutlineSegment::Quadratic {
            control: (0.0, broken),
            to: (1.0, 1.0),
        };
        assert!(!control.is_drawable(), "a {broken} control was admitted");

        let mut start = square();
        start.contours[0].start = (broken, 0.0);
        assert!(!start.is_drawable());

        assert!(!GlyphOutline {
            advance: broken,
            ..square()
        }
        .is_drawable());
        assert!(!GlyphOutline {
            units_per_em: broken,
            ..square()
        }
        .is_drawable());
        assert!(!GlyphOutline {
            synthetic_shear: broken,
            ..square()
        }
        .is_drawable());
    }
}

#[test]
fn a_negative_synthetic_bold_is_refused() {
    assert!(!GlyphOutline {
        synthetic_bold: -0.01,
        ..square()
    }
    .is_drawable());
}

#[test]
fn a_glyph_past_the_point_bound_is_refused() {
    let segments = vec![OutlineSegment::Line { to: (1.0, 1.0) }; MAX_GLYPH_POINTS];
    let glyph = GlyphOutline {
        contours: vec![OutlineContour {
            start: (0.0, 0.0),
            segments,
        }],
        ..square()
    };
    assert_eq!(glyph.points(), MAX_GLYPH_POINTS + 1);
    assert!(
        !glyph.is_drawable(),
        "a glyph past the point bound must be refused, not merely large"
    );

    let fitted = GlyphOutline {
        contours: vec![OutlineContour {
            start: (0.0, 0.0),
            segments: vec![OutlineSegment::Line { to: (1.0, 1.0) }; MAX_GLYPH_POINTS - 1],
        }],
        ..square()
    };
    assert!(fitted.is_drawable());
}

#[test]
fn a_glyph_with_no_ink_is_drawable_and_advances_the_pen() {
    let space = GlyphOutline {
        contours: Vec::new(),
        ..square()
    };
    assert!(space.is_drawable());
    assert_eq!(space.points(), 0);
}

#[test]
fn no_fonts_furnishes_nothing_rather_than_a_default_face() {
    let mut provider = NoFonts;
    let request = FaceRequest {
        family: "sans-serif",
        weight: 400,
        style: FontStyle::Normal,
        stretch: 10_000,
    };
    assert_eq!(provider.select(&request), Err(FontUnavailable));

    let mut out = Vec::new();
    assert_eq!(
        provider.outlines(FaceId::new(0), &['a'], &mut out),
        Err(FontUnavailable)
    );
    assert!(out.is_empty());
}
