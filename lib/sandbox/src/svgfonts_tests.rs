//! Unit tests for the two-phase glyph supply: what a sandboxed decode
//! records, what crosses the pipe, and what it refuses.

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use tairix_svg::font::{
    FaceId, FaceMetrics, FaceRequest, FontProvider, FontStyle, GlyphOutline, OutlineContour,
    OutlineSegment,
};

use super::{
    style_of_wire, FaceWant, FontTable, FontWants, TableFonts, MAX_FACES, MAX_FAMILY_NAME,
    MAX_SCALARS,
};
use crate::wire::{Reader, Writer};

/// A face request naming `family` at the initial axes.
fn request(family: &str) -> FaceRequest<'_> {
    FaceRequest {
        family,
        weight: 400,
        style: FontStyle::Normal,
        stretch: 10_000,
    }
}

/// Usable metrics for a face on a 1000-unit em.
fn metrics(id: u32) -> FaceMetrics {
    FaceMetrics {
        id: FaceId::new(id),
        units_per_em: 1000.0,
        ascent: 800.0,
        descent: 200.0,
        line_gap: 50.0,
    }
}

/// A one-contour glyph.
fn glyph() -> GlyphOutline {
    GlyphOutline {
        units_per_em: 1000.0,
        advance: 500.0,
        synthetic_bold: 0.0416,
        synthetic_shear: 0.2493,
        contours: vec![OutlineContour {
            start: (10.0, 20.0),
            segments: vec![
                OutlineSegment::Line { to: (500.0, 20.0) },
                OutlineSegment::Quadratic {
                    control: (600.0, 400.0),
                    to: (500.0, 700.0),
                },
                OutlineSegment::Line { to: (10.0, 20.0) },
            ],
        }],
    }
}

/// The want a table entry is filed under.
fn want(family: &str, scalars: &[char]) -> FaceWant {
    FaceWant {
        family: String::from(family),
        weight: 400,
        style: 1,
        stretch: 10_000,
        scalars: scalars.to_vec(),
    }
}

#[test]
fn an_empty_table_records_every_face_and_scalar_it_is_asked_for() {
    let table = FontTable::new();
    let mut provider = TableFonts::new(&table);
    let face = provider.select(&request("Inter")).expect("a placeholder");
    let mut out = Vec::new();
    provider
        .outlines(face.id, &['b', 'a', 'b'], &mut out)
        .expect("placeholders");
    assert_eq!(out.len(), 3, "the walk must go on against placeholders");

    let wants = provider.into_wants();
    assert_eq!(wants.faces.len(), 1);
    assert_eq!(wants.faces[0].family, "Inter");
    assert_eq!(
        wants.faces[0].scalars,
        vec!['a', 'b'],
        "each scalar is wanted once, in order"
    );
}

#[test]
fn a_document_of_several_faces_records_them_all_in_one_pass() {
    let table = FontTable::new();
    let mut provider = TableFonts::new(&table);
    let first = provider.select(&request("Inter")).expect("a placeholder");
    let second = provider.select(&request("Serif")).expect("a placeholder");
    let mut out = Vec::new();
    provider.outlines(first.id, &['a'], &mut out).expect("ok");
    provider.outlines(second.id, &['z'], &mut out).expect("ok");
    let wants = provider.into_wants();
    assert_eq!(wants.faces.len(), 2);
    assert_eq!(wants.faces[0].scalars, vec!['a']);
    assert_eq!(wants.faces[1].scalars, vec!['z']);
}

#[test]
fn a_supplied_table_answers_without_recording_anything() {
    let mut table = FontTable::new();
    table
        .push(want("Inter", &['a']), metrics(0), vec![('a', glyph())])
        .expect("a usable face");
    let mut provider = TableFonts::new(&table);
    let face = provider.select(&request("Inter")).expect("the held face");
    assert!((face.units_per_em - 1000.0).abs() < f64::EPSILON);
    let mut out = Vec::new();
    provider.outlines(face.id, &['a'], &mut out).expect("held");
    assert!((out[0].advance - 500.0).abs() < f64::EPSILON);
    assert!(
        provider.into_wants().is_empty(),
        "a served decode must ask for nothing, so it costs one round"
    );
}

#[test]
fn a_table_missing_a_scalar_of_a_face_it_holds_is_refused() {
    let mut table = FontTable::new();
    table
        .push(want("Inter", &['a']), metrics(0), vec![('a', glyph())])
        .expect("a usable face");
    let mut provider = TableFonts::new(&table);
    let face = provider.select(&request("Inter")).expect("the held face");
    let mut out = Vec::new();
    assert!(
        provider.outlines(face.id, &['a', 'q'], &mut out).is_err(),
        "a third round is not a thing this exchange has, so it fails closed"
    );
}

#[test]
fn an_unissued_face_id_is_refused() {
    let table = FontTable::new();
    let mut provider = TableFonts::new(&table);
    let mut out = Vec::new();
    assert!(provider.outlines(FaceId::new(3), &['a'], &mut out).is_err());
}

#[test]
fn a_face_is_recorded_once_however_often_it_is_selected() {
    let table = FontTable::new();
    let mut provider = TableFonts::new(&table);
    for _ in 0..4 {
        provider.select(&request("Inter")).expect("a placeholder");
    }
    assert_eq!(provider.into_wants().faces.len(), 1);
}

#[test]
fn more_faces_than_the_bound_are_refused_rather_than_recorded() {
    let table = FontTable::new();
    let mut provider = TableFonts::new(&table);
    let names: Vec<String> = (0..=MAX_FACES)
        .map(|index| alloc::format!("family{index}"))
        .collect();
    let mut refused = false;
    for name in &names {
        if provider.select(&request(name)).is_err() {
            refused = true;
        }
    }
    assert!(refused, "the face bound never bit");
}

#[test]
fn the_wants_round_trip_over_the_wire() {
    let wants = FontWants {
        faces: vec![
            want("Inter", &['a', 'b']),
            FaceWant {
                family: String::from("Noto Serif"),
                weight: 250,
                style: 3,
                stretch: 6250,
                scalars: vec!['\u{10FFFF}'],
            },
        ],
    };
    let mut w = Writer::new();
    wants.encode(&mut w);
    let encoded = w.finish();
    let mut r = Reader::new(&encoded);
    let decoded = FontWants::decode(&mut r).expect("a decode");
    assert!(r.is_exhausted());
    assert_eq!(decoded, wants);
}

#[test]
fn the_table_round_trips_over_the_wire() {
    let mut table = FontTable::new();
    table
        .push(
            want("Inter", &['a', 'b']),
            metrics(0),
            vec![('a', glyph()), ('b', glyph())],
        )
        .expect("a usable face");
    let mut w = Writer::new();
    table.encode(&mut w);
    let encoded = w.finish();
    let mut r = Reader::new(&encoded);
    let decoded = FontTable::decode(&mut r).expect("a decode");
    assert!(r.is_exhausted());

    // The geometry survives the fixed-point crossing to within one step of
    // 1/64 of a font unit.
    let mut provider = TableFonts::new(&decoded);
    let face = provider.select(&request("Inter")).expect("the held face");
    let mut out = Vec::new();
    provider.outlines(face.id, &['a'], &mut out).expect("held");
    let original = glyph();
    assert!((out[0].advance - original.advance).abs() <= 1.0 / 64.0);
    assert!((out[0].synthetic_shear - original.synthetic_shear).abs() <= 1.0 / 64.0);
    assert_eq!(out[0].contours[0].segments.len(), 3);
    match out[0].contours[0].segments[1] {
        OutlineSegment::Quadratic { control, to } => {
            assert!((control.0 - 600.0).abs() <= 1.0 / 64.0);
            assert!((to.1 - 700.0).abs() <= 1.0 / 64.0);
        }
        OutlineSegment::Line { .. } => panic!("the quadratic came back a line"),
    }
}

#[test]
fn a_face_with_no_usable_em_is_refused_by_the_table() {
    let mut table = FontTable::new();
    let broken = FaceMetrics {
        units_per_em: 0.0,
        ..metrics(0)
    };
    assert!(table
        .push(want("Inter", &['a']), broken, Vec::new())
        .is_err());
}

#[test]
fn an_undrawable_glyph_is_refused_by_the_table() {
    let mut table = FontTable::new();
    let broken = GlyphOutline {
        advance: f64::NAN,
        ..glyph()
    };
    assert!(table
        .push(want("Inter", &['a']), metrics(0), vec![('a', broken)])
        .is_err());
}

#[test]
fn a_wire_family_longer_than_the_bound_is_refused() {
    let mut w = Writer::new();
    w.u32(1);
    let long = "x".repeat(MAX_FAMILY_NAME + 1);
    w.str(&long);
    let encoded = w.finish();
    let mut r = Reader::new(&encoded);
    assert!(FontWants::decode(&mut r).is_err());
}

#[test]
fn a_wire_scalar_count_past_the_bound_is_refused() {
    let mut w = Writer::new();
    w.u32(1);
    w.str("Inter");
    w.u32(400);
    w.u8(1);
    w.u32(10_000);
    // Bounded before any scalar is read, so a hostile count drives no
    // allocation at all.
    w.u32(u32::try_from(MAX_SCALARS + 1).expect("the bound fits a u32"));
    let encoded = w.finish();
    let mut r = Reader::new(&encoded);
    assert!(FontWants::decode(&mut r).is_err());
}

#[test]
fn a_wire_scalar_that_is_not_a_scalar_value_is_refused() {
    let mut w = Writer::new();
    w.u32(1);
    w.str("Inter");
    w.u32(400);
    w.u8(1);
    w.u32(10_000);
    w.u32(1);
    // A lone surrogate is not a Unicode scalar value.
    w.u32(0xD800);
    let encoded = w.finish();
    let mut r = Reader::new(&encoded);
    assert!(FontWants::decode(&mut r).is_err());
}

#[test]
fn every_posture_crosses_the_wire_and_an_unknown_one_is_refused() {
    for style in [FontStyle::Normal, FontStyle::Italic, FontStyle::Oblique] {
        let wire = super::style_wire(style);
        assert_eq!(style_of_wire(wire), Some(style));
    }
    assert_eq!(style_of_wire(0), None);
    assert_eq!(style_of_wire(9), None);
}
