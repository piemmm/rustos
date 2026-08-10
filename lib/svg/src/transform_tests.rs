//! Unit tests for the transform grammar and viewport fitting.

use tairix_raster::Affine;

use crate::error::SvgError;

use super::{
    parse_aspect_ratio, parse_transform, parse_view_box, viewport_transform, Align, AspectRatio,
    ViewBox,
};

#[track_caller]
fn maps(transform: Affine, from: (f64, f64), to: (f64, f64)) {
    let got = transform.apply(from);
    assert!(
        (got.0 - to.0).abs() <= 1e-9 && (got.1 - to.1).abs() <= 1e-9,
        "expected {to:?}, got {got:?}"
    );
}

// --- the transform list ---------------------------------------------------

#[test]
fn each_transform_function_does_what_it_says() {
    maps(
        parse_transform("translate(10 20)").expect("a translation"),
        (1.0, 1.0),
        (11.0, 21.0),
    );
    maps(
        parse_transform("translate(10)").expect("a one-axis translation"),
        (1.0, 1.0),
        (11.0, 1.0),
    );
    maps(
        parse_transform("scale(2 3)").expect("a scale"),
        (2.0, 2.0),
        (4.0, 6.0),
    );
    maps(
        parse_transform("scale(2)").expect("a uniform scale"),
        (2.0, 2.0),
        (4.0, 4.0),
    );
    maps(
        parse_transform("rotate(90)").expect("a rotation"),
        (1.0, 0.0),
        (0.0, 1.0),
    );
    maps(
        parse_transform("rotate(90 1 0)").expect("a rotation about a point"),
        (1.0, 0.0),
        (1.0, 0.0),
    );
    maps(
        parse_transform("matrix(1 0 0 1 5 6)").expect("a matrix"),
        (1.0, 1.0),
        (6.0, 7.0),
    );
    maps(
        parse_transform("skewX(45)").expect("a horizontal skew"),
        (0.0, 1.0),
        (1.0, 1.0),
    );
    maps(
        parse_transform("skewY(45)").expect("a vertical skew"),
        (1.0, 0.0),
        (1.0, 1.0),
    );
}

/// A transform list applies right to left: the last function written is the
/// first one the point meets.
#[test]
fn a_transform_list_applies_right_to_left() {
    let transform = parse_transform("translate(10 0) scale(2)").expect("a list");
    maps(transform, (1.0, 0.0), (12.0, 0.0));
    let other = parse_transform("scale(2) translate(10 0)").expect("a list");
    maps(other, (1.0, 0.0), (22.0, 0.0));
}

#[test]
fn separators_between_functions_are_optional() {
    let transform = parse_transform("translate(1,2),scale(2)").expect("a list");
    maps(transform, (1.0, 1.0), (3.0, 4.0));
}

#[test]
fn a_malformed_transform_is_refused() {
    for bad in [
        "translate(",
        "translate)",
        "wobble(1)",
        "translate(1 2 3)",
        "rotate(1 2)",
        "matrix(1 2 3)",
        "scale(x)",
        "scale(1) junk",
    ] {
        assert_eq!(
            parse_transform(bad),
            Err(SvgError::InvalidNumber),
            "{bad:?} should be refused"
        );
    }
}

#[test]
fn an_empty_transform_is_the_identity() {
    assert_eq!(parse_transform("   "), Ok(Affine::IDENTITY));
}

// --- the view box ---------------------------------------------------------

#[test]
fn a_view_box_reads_its_origin_and_extent() {
    assert_eq!(
        parse_view_box("1 2 30 40"),
        Ok(ViewBox {
            min: (1.0, 2.0),
            size: (30.0, 40.0),
        })
    );
    assert_eq!(
        parse_view_box("0,0,10,10"),
        Ok(ViewBox {
            min: (0.0, 0.0),
            size: (10.0, 10.0),
        })
    );
}

/// A zero or negative extent disables rendering in SVG, which for an asset
/// the desktop must draw is a refusal.
#[test]
fn a_view_box_without_positive_area_is_refused() {
    for bad in [
        "0 0 0 10",
        "0 0 10 0",
        "0 0 -5 5",
        "0 0 10",
        "0 0 10 10 10",
        "a b c d",
    ] {
        assert_eq!(
            parse_view_box(bad),
            Err(SvgError::InvalidViewBox),
            "{bad:?} should be refused"
        );
    }
}

// --- preserveAspectRatio --------------------------------------------------

#[test]
fn the_aspect_ratio_keywords_all_parse() {
    assert_eq!(
        parse_aspect_ratio("none"),
        Ok(AspectRatio {
            align: Align::None,
            slice: false
        })
    );
    assert_eq!(
        parse_aspect_ratio("xMidYMid meet"),
        Ok(AspectRatio::default())
    );
    assert_eq!(
        parse_aspect_ratio("xMinYMin slice"),
        Ok(AspectRatio {
            align: Align::Min,
            slice: true
        })
    );
    assert!(parse_aspect_ratio("defer xMaxYMax").is_ok());
    assert!(parse_aspect_ratio("xMinYMax").is_ok());
    assert_eq!(
        parse_aspect_ratio("sideways"),
        Err(SvgError::InvalidViewBox)
    );
    assert_eq!(
        parse_aspect_ratio("xMidYMid maybe"),
        Err(SvgError::InvalidViewBox)
    );
}

/// A square view box fills a square viewport exactly, whatever the anchoring
/// says, because there is no spare space to anchor in.
#[test]
fn a_square_view_box_fills_a_square_viewport() {
    let view_box = ViewBox {
        min: (0.0, 0.0),
        size: (10.0, 10.0),
    };
    let transform = viewport_transform(view_box, (100.0, 100.0), AspectRatio::default());
    maps(transform, (0.0, 0.0), (0.0, 0.0));
    maps(transform, (10.0, 10.0), (100.0, 100.0));
    maps(transform, (5.0, 5.0), (50.0, 50.0));
}

/// The whole point of honouring the aspect ratio: a wide drawing is
/// letter-boxed into a square slot instead of being stretched.
#[test]
fn a_wide_view_box_is_letter_boxed_into_a_square_viewport() {
    let view_box = ViewBox {
        min: (0.0, 0.0),
        size: (20.0, 10.0),
    };
    let transform = viewport_transform(view_box, (100.0, 100.0), AspectRatio::default());
    // Scaled by five to fit the width, and centred vertically in what is
    // left over.
    maps(transform, (0.0, 0.0), (0.0, 25.0));
    maps(transform, (20.0, 10.0), (100.0, 75.0));
}

#[test]
fn the_none_alignment_stretches_instead_of_fitting() {
    let view_box = ViewBox {
        min: (0.0, 0.0),
        size: (20.0, 10.0),
    };
    let ratio = AspectRatio {
        align: Align::None,
        slice: false,
    };
    let transform = viewport_transform(view_box, (100.0, 100.0), ratio);
    maps(transform, (20.0, 10.0), (100.0, 100.0));
}

#[test]
fn slice_covers_the_viewport_and_overflows() {
    let view_box = ViewBox {
        min: (0.0, 0.0),
        size: (20.0, 10.0),
    };
    let ratio = AspectRatio {
        align: Align::Min,
        slice: true,
    };
    let transform = viewport_transform(view_box, (100.0, 100.0), ratio);
    // Scaled by ten to cover the height, so the width runs off the edge.
    maps(transform, (0.0, 0.0), (0.0, 0.0));
    maps(transform, (20.0, 10.0), (200.0, 100.0));
}

#[test]
fn a_view_box_origin_is_subtracted_before_scaling() {
    let view_box = ViewBox {
        min: (5.0, 5.0),
        size: (10.0, 10.0),
    };
    let transform = viewport_transform(view_box, (100.0, 100.0), AspectRatio::default());
    maps(transform, (5.0, 5.0), (0.0, 0.0));
    maps(transform, (15.0, 15.0), (100.0, 100.0));
}
