//! Tests over the public glyph-outline API and the bounds it holds a hostile
//! face to.

use alloc::vec;
use alloc::vec::Vec;

use crate::engine::MAX_COMPONENTS;
use crate::tests::asset;
use crate::{AxisSetting, Contour, Face, OutlineSegment};
use tairix_abi::font_ipc::FONT_MAX_OUTLINE_POINTS;

/// Where a segment ends.
fn end_of(segment: &OutlineSegment) -> (f64, f64) {
    match *segment {
        OutlineSegment::Line { to } | OutlineSegment::Quadratic { to, .. } => to,
    }
}

/// A contour's points in order, curves flattened uniformly — enough to reason
/// about area and winding, which the control points only refine.
fn polygon(contour: &Contour) -> Vec<(f64, f64)> {
    let mut points = vec![contour.start];
    let mut from = contour.start;
    for segment in &contour.segments {
        match *segment {
            OutlineSegment::Line { to } => points.push(to),
            OutlineSegment::Quadratic { control, to } => {
                for step in 1..=8 {
                    let t = f64::from(step) / 8.0;
                    let u = 1.0 - t;
                    points.push((
                        u * u * from.0 + 2.0 * u * t * control.0 + t * t * to.0,
                        u * u * from.1 + 2.0 * u * t * control.1 + t * t * to.1,
                    ));
                }
            }
        }
        from = end_of(segment);
    }
    points
}

/// Twice the signed area of a contour: positive counter-clockwise in a y-up
/// space, and its sign is the direction the contour is wound.
fn signed_area(contour: &Contour) -> f64 {
    let points = polygon(contour);
    let mut total = 0.0;
    for i in 0..points.len() {
        let (x0, y0) = points[i];
        let (x1, y1) = points[(i + 1) % points.len()];
        total += x0 * y1 - x1 * y0;
    }
    total
}

/// The non-zero winding number of `at` with respect to every contour: how
/// many times the outline wraps the point, counting direction. Zero is
/// outside the fill.
fn winding(contours: &[Contour], at: (f64, f64)) -> i32 {
    let mut count = 0;
    for contour in contours {
        let points = polygon(contour);
        for i in 0..points.len() {
            let (x0, y0) = points[i];
            let (x1, y1) = points[(i + 1) % points.len()];
            // A ray cast in +x, counting an upward crossing as +1 and a
            // downward one as -1. The span is half-open so a shared vertex is
            // counted once.
            let crosses = if y0 <= at.1 { y1 > at.1 } else { y1 <= at.1 };
            if !crosses {
                continue;
            }
            let t = (at.1 - y0) / (y1 - y0);
            if x0 + t * (x1 - x0) > at.0 {
                count += if y1 > y0 { 1 } else { -1 };
            }
        }
    }
    count
}

/// The bounding box of a set of contours.
fn bounds(contours: &[Contour]) -> Option<((f64, f64), (f64, f64))> {
    let mut box_ = None;
    for contour in contours {
        for (x, y) in polygon(contour) {
            let ((lx, ly), (hx, hy)) = box_.unwrap_or(((x, y), (x, y)));
            box_ = Some(((lx.min(x), ly.min(y)), (hx.max(x), hy.max(y))));
        }
    }
    box_
}

/// The same contours shifted by `(dx, dy)`.
fn shifted(contours: &[Contour], dx: f64, dy: f64) -> Vec<Contour> {
    let move_ = |(x, y): (f64, f64)| (x + dx, y + dy);
    contours
        .iter()
        .map(|contour| Contour {
            start: move_(contour.start),
            segments: contour
                .segments
                .iter()
                .map(|segment| match *segment {
                    OutlineSegment::Line { to } => OutlineSegment::Line { to: move_(to) },
                    OutlineSegment::Quadratic { control, to } => OutlineSegment::Quadratic {
                        control: move_(control),
                        to: move_(to),
                    },
                })
                .collect(),
        })
        .collect()
}

fn mono() -> Vec<u8> {
    asset("mono/Inconsolata-EX.ttf")
}

fn outline_of(face: &Face<'_>, ch: char) -> Vec<Contour> {
    let glyph = face
        .glyph_for(u32::from(ch))
        .unwrap_or_else(|| panic!("face maps {ch:?}"));
    face.glyph_outline(glyph)
        .unwrap_or_else(|e| panic!("{ch:?} outlines: {e}"))
}

#[test]
fn a_simple_glyph_is_closed_contours_in_font_units() {
    let bytes = mono();
    let face = Face::parse(&bytes).expect("parses");
    let contours = outline_of(&face, 'A');

    assert!(!contours.is_empty(), "'A' has no contours");
    for contour in &contours {
        assert!(
            !contour.segments.is_empty(),
            "a contour carries no segments"
        );
        let last = end_of(&contour.segments[contour.segments.len() - 1]);
        assert_eq!(
            last, contour.start,
            "the last segment must return to the start — a contour is closed"
        );
    }

    // Font units, not pixels and not an em fraction: a capital reaches a good
    // part of the 1024-unit em and sits on the baseline at y = 0.
    let ((_, low), (_, high)) = bounds(&contours).expect("'A' has ink");
    assert_eq!(face.units_per_em(), 1024, "the committed face's em changed");
    let em = f64::from(face.units_per_em());
    assert!(
        low.abs() < em * 0.02,
        "'A' should sit on the baseline, not at {low}"
    );
    assert!(
        high > em * 0.4 && high < em,
        "'A' should stand within the em, not to {high}"
    );
}

#[test]
fn a_counter_is_its_own_contour_wound_to_cut_a_hole() {
    let bytes = mono();
    let face = Face::parse(&bytes).expect("parses");
    let contours = outline_of(&face, 'o');

    assert_eq!(contours.len(), 2, "'o' is a bowl and its counter");
    let outer = signed_area(&contours[0]);
    let inner = signed_area(&contours[1]);
    assert!(
        outer * inner < 0.0,
        "the counter must be wound against the bowl ({outer} and {inner})"
    );
    assert!(
        outer.abs() > inner.abs(),
        "the bowl must enclose more than its counter"
    );

    // What the winding actually has to buy: the centre of the 'o' is outside
    // the fill, while the bowl's own ink is inside it.
    let ((low_x, low_y), (high_x, high_y)) = bounds(&contours).expect("'o' has ink");
    let centre = (f64::midpoint(low_x, high_x), f64::midpoint(low_y, high_y));
    assert_eq!(
        winding(&contours, centre),
        0,
        "the counter is not cut out of the bowl"
    );
    let ((counter_x, _), _) = bounds(&contours[1..]).expect("the counter has extent");
    let stem = (f64::midpoint(low_x, counter_x), centre.1);
    assert_ne!(
        winding(&contours, stem),
        0,
        "the bowl's own ink should be filled"
    );
}

#[test]
fn a_composite_places_each_component_where_its_record_says() {
    // Inconsolata's 'é' is an accent and an 'e', both at a zero offset: the
    // components carry their own position, so the composite is their contours
    // verbatim.
    let bytes = mono();
    let face = Face::parse(&bytes).expect("parses");
    let accent = face.glyph_outline(117).expect("accent outlines");
    let base = outline_of(&face, 'e');
    let composite = outline_of(&face, '\u{E9}');

    let mut expected = accent.clone();
    expected.extend(base.iter().cloned());
    assert_eq!(
        composite, expected,
        "'é' must be its two components' contours, unmoved"
    );

    // The accent has to end up above the letter, not on top of it.
    let (_, (_, base_top)) = bounds(&base).expect("'e' has ink");
    let ((_, accent_low), _) = bounds(&accent).expect("the accent has ink");
    assert!(
        accent_low > base_top,
        "the accent should clear the 'e' ({accent_low} vs {base_top})"
    );
}

#[test]
fn a_composite_component_is_moved_by_its_offset() {
    // Inter's 'é' places its accent at x + 356, so the composite must carry
    // that component shifted by exactly that much and nothing else.
    let bytes = asset("inter/Inter-Variable.ttf");
    let face = Face::parse(&bytes).expect("parses");
    let base = face.glyph_outline(614).expect("base outlines");
    let accent = face.glyph_outline(1770).expect("accent outlines");
    let composite = outline_of(&face, '\u{E9}');

    let mut expected = base.clone();
    expected.extend(shifted(&accent, 356.0, 0.0));
    assert_eq!(
        composite, expected,
        "'é' must be its base plus its accent moved by the record's offset"
    );
}

#[test]
fn an_instanced_face_outlines_at_the_axis_it_was_asked_for() {
    let bytes = asset("inter/Inter-Variable.ttf");
    let light = Face::parse_instance(
        &bytes,
        &[AxisSetting {
            tag: *b"wght",
            value: 100.0,
        }],
    )
    .expect("light instances");
    let heavy = Face::parse_instance(
        &bytes,
        &[AxisSetting {
            tag: *b"wght",
            value: 900.0,
        }],
    )
    .expect("heavy instances");

    let thin = outline_of(&light, 'o');
    let bold = outline_of(&heavy, 'o');
    assert_eq!(
        thin.len(),
        bold.len(),
        "an instance changes an outline's coordinates, never its topology"
    );
    assert_ne!(thin, bold, "the two weights must not outline identically");

    // A heavier weight is more ink: the counter shrinks as the bowl thickens.
    let counter = |contours: &[Contour]| signed_area(&contours[1]).abs();
    assert!(
        counter(&bold) < counter(&thin),
        "the bold counter ({}) should be tighter than the light one ({})",
        counter(&bold),
        counter(&thin)
    );
}

#[test]
fn an_empty_glyph_has_no_contours_and_notdef_does() {
    let bytes = mono();
    let face = Face::parse(&bytes).expect("parses");

    let space = face.glyph_for(u32::from(' ')).expect("maps a space");
    assert!(
        face.glyph_outline(space)
            .expect("a space outlines")
            .is_empty(),
        "a space draws nothing, so it has no contours"
    );

    // .notdef is glyph 0 by definition and draws the missing-glyph box.
    assert!(
        !face.glyph_outline(0).expect(".notdef outlines").is_empty(),
        ".notdef must draw its box"
    );
}

#[test]
fn a_glyph_id_past_the_face_is_refused() {
    let bytes = mono();
    let face = Face::parse(&bytes).expect("parses");
    assert!(
        face.glyph_outline(u16::MAX).is_err(),
        "a glyph id past the face's count must fail closed"
    );
}

#[test]
fn the_rasteriser_and_the_outline_api_read_the_same_glyph() {
    // One walk feeds both, so the ink the rasteriser fills must lie inside
    // the contours the outline API reports — a second decoder would drift.
    let bytes = mono();
    let face = Face::parse(&bytes).expect("parses");
    let em = f64::from(face.units_per_em());
    for ch in ['A', 'o', 'g', '\u{E9}'] {
        let contours = outline_of(&face, ch);
        let ((low_x, low_y), (high_x, high_y)) = bounds(&contours).expect("ink");

        let glyph = face.glyph_for(u32::from(ch)).expect("mapped");
        let px = 64.0;
        let baseline = 64u32;
        let raster = face
            .rasterise_proportional(glyph, px, baseline, 96)
            .expect("rasterises");
        assert!(raster.width > 0, "{ch:?} rasterised no ink");

        // The tight bitmap's inked columns must match the outline's own x
        // extent, to within the pixel the bitmap is rounded out to.
        let scale = px / em;
        let expected_left = low_x * scale;
        let expected_width = (high_x - low_x) * scale;
        assert!(
            (f64::from(raster.left) - expected_left).abs() <= 1.5,
            "{ch:?}: bitmap starts at {} but the outline starts at {expected_left}",
            raster.left
        );
        assert!(
            (f64::from(raster.width) - expected_width).abs() <= 2.5,
            "{ch:?}: bitmap is {} wide but the outline is {expected_width}",
            raster.width
        );
        assert!(high_y > low_y, "{ch:?} has no vertical extent");
    }
}

/// A byte-for-byte mutable copy of the committed face, with the offsets a
/// test needs to reach into its `glyf`.
struct Patched {
    bytes: Vec<u8>,
    glyf: usize,
    loca: usize,
    long_loca: bool,
}

impl Patched {
    fn new() -> Self {
        let bytes = mono();
        let count = usize::from(u16::from_be_bytes([bytes[4], bytes[5]]));
        let mut glyf = 0;
        let mut loca = 0;
        let mut head = 0;
        for i in 0..count {
            let entry = 12 + 16 * i;
            let offset = u32::from_be_bytes([
                bytes[entry + 8],
                bytes[entry + 9],
                bytes[entry + 10],
                bytes[entry + 11],
            ]) as usize;
            match &bytes[entry..entry + 4] {
                b"glyf" => glyf = offset,
                b"loca" => loca = offset,
                b"head" => head = offset,
                _ => {}
            }
        }
        let long_loca = i16::from_be_bytes([bytes[head + 50], bytes[head + 51]]) == 1;
        Self {
            bytes,
            glyf,
            loca,
            long_loca,
        }
    }

    /// Where `glyph`'s outline begins in the file.
    fn entry(&self, glyph: u16) -> usize {
        let i = usize::from(glyph);
        let offset = if self.long_loca {
            u32::from_be_bytes([
                self.bytes[self.loca + 4 * i],
                self.bytes[self.loca + 4 * i + 1],
                self.bytes[self.loca + 4 * i + 2],
                self.bytes[self.loca + 4 * i + 3],
            ]) as usize
        } else {
            usize::from(u16::from_be_bytes([
                self.bytes[self.loca + 2 * i],
                self.bytes[self.loca + 2 * i + 1],
            ])) * 2
        };
        self.glyf + offset
    }

    fn write_u16(&mut self, at: usize, value: u16) {
        self.bytes[at..at + 2].copy_from_slice(&value.to_be_bytes());
    }
}

#[test]
fn a_glyf_entry_claiming_more_than_it_holds_fails_closed() {
    // Glyph 'A' declares far more contours than its `loca` range can hold, so
    // reading its points runs off the end. That must be an error, not a panic
    // and not a half-decoded glyph.
    let mut patched = Patched::new();
    let bytes = mono();
    let probe = Face::parse(&bytes).expect("parses");
    let glyph = probe.glyph_for(u32::from('A')).expect("maps 'A'");
    let entry = patched.entry(glyph);
    patched.write_u16(entry, 4000);

    let face = Face::parse(&patched.bytes).expect("the face still parses");
    assert!(
        face.glyph_outline(glyph).is_err(),
        "a glyf entry that overruns its range must fail closed"
    );
}

#[test]
fn a_cyclic_composite_is_refused_by_the_depth_bound() {
    // 'é' is a composite; point its first component at itself so the walk
    // would recurse for ever.
    let mut patched = Patched::new();
    let bytes = mono();
    let probe = Face::parse(&bytes).expect("parses");
    let glyph = probe.glyph_for(0xE9).expect("maps 'é'");
    assert!(
        i16::from_be_bytes([
            patched.bytes[patched.entry(glyph)],
            patched.bytes[patched.entry(glyph) + 1],
        ]) < 0,
        "'é' must be a composite for this test to mean anything"
    );
    let entry = patched.entry(glyph);
    // The first component record's glyphIndex sits after numberOfContours,
    // the four bounding-box fields, and the record's own flags word.
    patched.write_u16(entry + 10 + 2, glyph);

    let face = Face::parse(&patched.bytes).expect("the face still parses");
    let outlined = face.glyph_outline(glyph);
    assert!(
        outlined.is_err(),
        "a self-referential composite must fail closed"
    );
    assert!(
        face.rasterise_proportional(glyph, 32.0, 24, 32).is_err(),
        "the rasteriser shares the walk, so it must refuse it too"
    );
}

#[test]
fn every_committed_glyph_clears_the_outline_bounds() {
    // The bounds are sized for a hostile face, so no real one may meet them —
    // including the CJK faces, whose glyphs are the heaviest in the tree, and
    // the variable ones, whose outlines are instanced before they are
    // measured. Sweeping every repertoire also asserts the walk never panics.
    const FACES: [&str; 10] = [
        "inter/Inter-Variable.ttf",
        "mono/D2Coding-Regular.ttf",
        "mono/Inconsolata-EX.ttf",
        "mono/MPLUS1Code-Regular.ttf",
        "mono/NotoSansHebrew-ExtraCondensed.ttf",
        "noto-sans/NotoSans-Variable.ttf",
        "noto-serif/NotoSerif-Variable.ttf",
        "sans-fallback/NotoSansHebrew-Variable.ttf",
        "sans-fallback/NotoSansKR-Variable.ttf",
        "sans-fallback/NotoSansSC-Variable.ttf",
    ];

    let mut heaviest = 0;
    let mut most_contours = 0;
    for name in FACES {
        let bytes = asset(name);
        let face = Face::parse(&bytes).unwrap_or_else(|e| panic!("{name} parses: {e}"));
        for &(_, glyph) in face.mapped() {
            let contours = face
                .glyph_outline(glyph)
                .unwrap_or_else(|e| panic!("{name} glyph {glyph} outlines: {e}"));
            most_contours = most_contours.max(contours.len());
            heaviest = heaviest.max(
                contours
                    .iter()
                    .map(|contour| contour.segments.len())
                    .sum::<usize>(),
            );
        }
    }
    assert!(most_contours > 1, "the sweep found no multi-contour glyph");
    assert!(
        heaviest * 4 < FONT_MAX_OUTLINE_POINTS as usize,
        "the heaviest committed glyph ({heaviest}) leaves too little headroom \
         under the point bound ({FONT_MAX_OUTLINE_POINTS})"
    );
}

/// A minimal well-formed face carrying exactly the glyphs a bound test needs.
///
/// A bound sized for a hostile face cannot be reached by a legitimate one, so
/// the only way to see it refuse is to build the face that meets it. `glyphs`
/// are raw `glyf` entries, indexed by glyph id; an empty one is a glyph with
/// no outline. Glyph 1 is what `'A'` maps to.
fn synthetic_face(glyphs: &[Vec<u8>]) -> Vec<u8> {
    let glyph_count = u16::try_from(glyphs.len()).expect("a handful of glyphs");

    let mut glyf: Vec<u8> = Vec::new();
    let mut loca: Vec<u8> = Vec::new();
    for entry in glyphs {
        loca.extend_from_slice(&u32::try_from(glyf.len()).expect("small glyf").to_be_bytes());
        glyf.extend_from_slice(entry);
        while !glyf.len().is_multiple_of(4) {
            glyf.push(0);
        }
    }
    loca.extend_from_slice(&u32::try_from(glyf.len()).expect("small glyf").to_be_bytes());

    let mut head = vec![0u8; 54];
    head[18..20].copy_from_slice(&1000u16.to_be_bytes());
    head[50..52].copy_from_slice(&1i16.to_be_bytes());

    let mut hhea = vec![0u8; 36];
    hhea[4..6].copy_from_slice(&800i16.to_be_bytes());
    hhea[6..8].copy_from_slice(&(-200i16).to_be_bytes());
    hhea[34..36].copy_from_slice(&glyph_count.to_be_bytes());

    let mut maxp = vec![0u8; 6];
    maxp[0..4].copy_from_slice(&0x0001_0000u32.to_be_bytes());
    maxp[4..6].copy_from_slice(&glyph_count.to_be_bytes());

    let mut hmtx: Vec<u8> = Vec::new();
    for _ in 0..glyph_count {
        hmtx.extend_from_slice(&600u16.to_be_bytes());
        hmtx.extend_from_slice(&0i16.to_be_bytes());
    }

    // One format-4 subtable mapping 'A' to glyph 1, then the required
    // 0xFFFF terminating segment.
    let letter = u16::from(b'A');
    let mut cmap: Vec<u8> = vec![0, 0, 0, 1, 0, 3, 0, 1];
    cmap.extend_from_slice(&12u32.to_be_bytes());
    for word in [
        4,
        32,
        0,
        4,
        0,
        0,
        0,
        letter,
        0xFFFF,
        0,
        letter,
        0xFFFF,
        1u16.wrapping_sub(letter),
        1,
        0,
        0,
    ] {
        cmap.extend_from_slice(&word.to_be_bytes());
    }

    let tables: [(&[u8; 4], Vec<u8>); 7] = [
        (b"cmap", cmap),
        (b"glyf", glyf),
        (b"head", head),
        (b"hhea", hhea),
        (b"hmtx", hmtx),
        (b"loca", loca),
        (b"maxp", maxp),
    ];

    let count = u16::try_from(tables.len()).expect("seven tables");
    let mut font: Vec<u8> = Vec::new();
    font.extend_from_slice(&0x0001_0000u32.to_be_bytes());
    font.extend_from_slice(&count.to_be_bytes());
    font.extend_from_slice(&[0u8; 6]);
    let mut at = font.len() + 16 * tables.len();
    let mut body: Vec<u8> = Vec::new();
    for (tag, data) in &tables {
        font.extend_from_slice(*tag);
        font.extend_from_slice(&0u32.to_be_bytes());
        font.extend_from_slice(&u32::try_from(at).expect("small font").to_be_bytes());
        font.extend_from_slice(
            &u32::try_from(data.len())
                .expect("small table")
                .to_be_bytes(),
        );
        body.extend_from_slice(data);
        at += data.len();
        while !at.is_multiple_of(4) {
            body.push(0);
            at += 1;
        }
    }
    font.extend_from_slice(&body);
    font
}

/// A `glyf` entry for a square contour, so a synthetic face has one glyph
/// that genuinely draws.
fn square_glyph() -> Vec<u8> {
    let mut entry: Vec<u8> = Vec::new();
    for word in [1i16, 100, 0, 500, 700] {
        entry.extend_from_slice(&word.to_be_bytes());
    }
    entry.extend_from_slice(&3u16.to_be_bytes());
    entry.extend_from_slice(&0u16.to_be_bytes());
    entry.extend_from_slice(&[0x01; 4]);
    for delta in [100i16, 400, 0, -400, 0, 0, 700, 0] {
        entry.extend_from_slice(&delta.to_be_bytes());
    }
    entry
}

/// A simple glyph declaring one point more than the point bound allows.
fn overlong_glyph() -> Vec<u8> {
    let mut entry: Vec<u8> = Vec::new();
    for word in [1i16, 0, 0, 0, 0] {
        entry.extend_from_slice(&word.to_be_bytes());
    }
    let last = u16::try_from(FONT_MAX_OUTLINE_POINTS).expect("the bound fits a point index");
    entry.extend_from_slice(&last.to_be_bytes());
    entry.extend_from_slice(&0u16.to_be_bytes());
    entry
}

/// A composite declaring one component record more than the bound allows,
/// each a legitimate xy-offset placement of glyph 1.
fn overwide_composite() -> Vec<u8> {
    let mut entry: Vec<u8> = Vec::new();
    for word in [-1i16, 0, 0, 0, 0] {
        entry.extend_from_slice(&word.to_be_bytes());
    }
    for i in 0..=MAX_COMPONENTS {
        let more = if i == MAX_COMPONENTS { 0 } else { 0x0020 };
        entry.extend_from_slice(&(0x0002u16 | more).to_be_bytes());
        entry.extend_from_slice(&1u16.to_be_bytes());
        entry.extend_from_slice(&[0, 0]);
    }
    entry
}

#[test]
fn a_synthetic_face_draws_its_square() {
    let bytes = synthetic_face(&[Vec::new(), square_glyph()]);
    let face = Face::parse(&bytes).expect("the synthetic face parses");
    let contours = outline_of(&face, 'A');
    assert_eq!(contours.len(), 1, "the square is one contour");
    assert_eq!(
        bounds(&contours),
        Some(((100.0, 0.0), (500.0, 700.0))),
        "the square's corners must come through in font units"
    );
}

#[test]
fn an_outline_past_the_point_bound_is_refused() {
    let bytes = synthetic_face(&[Vec::new(), square_glyph(), overlong_glyph()]);
    let face = Face::parse(&bytes).expect("parses");
    let refused = face
        .glyph_outline(2)
        .expect_err("the point bound must bite");
    assert_eq!(refused.message(), "glyph outline exceeds the point bound");
    assert!(
        face.rasterise_proportional(2, 32.0, 24, 32).is_err(),
        "the rasteriser shares the walk, so the bound must hold there too"
    );
}

#[test]
fn a_composite_past_the_component_bound_is_refused() {
    // Without a budget charged across the whole walk, a composite of N
    // components each naming another such composite expands as N^depth: a
    // malformed face could hang the font service outright.
    let bytes = synthetic_face(&[Vec::new(), square_glyph(), overwide_composite()]);
    let face = Face::parse(&bytes).expect("parses");
    let refused = face
        .glyph_outline(2)
        .expect_err("the component bound must bite");
    assert_eq!(
        refused.message(),
        "composite expands past the component bound"
    );
}

#[test]
fn nested_composites_share_one_component_budget() {
    // Each level is well under the bound on its own; together they pass it.
    // A per-record cap would let this through and multiply with depth.
    let wide = |count: u16, component: u16| {
        let mut entry: Vec<u8> = Vec::new();
        for word in [-1i16, 0, 0, 0, 0] {
            entry.extend_from_slice(&word.to_be_bytes());
        }
        for i in 0..count {
            let more = if i + 1 == count { 0 } else { 0x0020 };
            entry.extend_from_slice(&(0x0002u16 | more).to_be_bytes());
            entry.extend_from_slice(&component.to_be_bytes());
            entry.extend_from_slice(&[0, 0]);
        }
        entry
    };
    let half = u16::try_from(MAX_COMPONENTS / 2 + 1).expect("half the bound fits");
    let bytes = synthetic_face(&[Vec::new(), square_glyph(), wide(half, 1), wide(half, 2)]);
    let face = Face::parse(&bytes).expect("parses");
    assert!(
        face.glyph_outline(2).is_ok(),
        "one level of {half} components is within the bound"
    );
    let refused = face
        .glyph_outline(3)
        .expect_err("two such levels must exceed it");
    assert_eq!(
        refused.message(),
        "composite expands past the component bound"
    );
}
