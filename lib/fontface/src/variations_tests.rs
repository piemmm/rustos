//! Tests for OpenType variable-font instancing and proportional rasterisation.

use alloc::vec::Vec;

use crate::tests::{asset, ink};
use crate::{AxisSetting, CellGeometry, Face, GlyphRaster, ATLAS_EM_PX};

/// A `wght`/`opsz`/… axis setting.
fn setting(tag: [u8; 4], value: f32) -> AxisSetting {
    AxisSetting { tag, value }
}

/// Parse `bytes` instanced at `settings`.
fn face_at<'a>(bytes: &'a [u8], settings: &[AxisSetting]) -> Face<'a> {
    Face::parse_instance(bytes, settings).expect("face parses")
}

/// A box tall enough that a 32-px glyph — Latin *or* full-em CJK — never
/// clips, so a coverage total reflects the whole glyph.
const PX: f64 = 32.0;
const HEIGHT: u32 = 96;
const BASELINE: u32 = 72;

/// The total inked coverage of `chars`, rendered proportionally at [`PX`] from
/// `bytes` instanced at `settings`.
fn ink_total(bytes: &[u8], settings: &[AxisSetting], chars: &[char]) -> usize {
    let face = face_at(bytes, settings);
    let mut total = 0;
    for &ch in chars {
        let glyph = face
            .glyph_for(u32::from(ch))
            .unwrap_or_else(|| panic!("no glyph for {ch:?}"));
        let raster = face
            .rasterise_proportional(glyph, PX, BASELINE, HEIGHT)
            .expect("rasterises");
        total += ink(&raster.coverage);
    }
    total
}

/// The leftmost inked column of a tight proportional bitmap, if any.
fn leftmost_inked_col(raster: &GlyphRaster) -> Option<u32> {
    let width = raster.width as usize;
    let mut leftmost: Option<u32> = None;
    for (i, &sample) in raster.coverage.iter().enumerate() {
        if sample > 0 {
            let col = u32::try_from(i % width).expect("column fits u32");
            leftmost = Some(leftmost.map_or(col, |m| m.min(col)));
        }
    }
    leftmost
}

/// An FNV-1a hash over coverage bytes, for the static-face golden.
fn fnv(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn inter() -> Vec<u8> {
    asset("inter/Inter-Variable.ttf")
}

#[test]
fn a_variable_face_reports_its_axes_and_a_static_one_reports_none() {
    let inter = inter();
    let face = Face::parse(&inter).expect("Inter parses");
    assert!(face.is_variable(), "Inter is a variable face");
    let axes = face.axes();

    let wght = axes
        .iter()
        .find(|axis| &axis.tag == b"wght")
        .expect("Inter declares wght");
    assert!((99.0..101.0).contains(&wght.min));
    assert!(
        (399.0..401.0).contains(&wght.default),
        "wght default is 400"
    );
    assert!((899.0..901.0).contains(&wght.max));

    let opsz = axes
        .iter()
        .find(|axis| &axis.tag == b"opsz")
        .expect("Inter declares opsz");
    assert!((13.0..15.0).contains(&opsz.min));
    assert!((13.0..15.0).contains(&opsz.default), "opsz default is 14");
    assert!((31.0..33.0).contains(&opsz.max));

    let inconsolata = asset("mono/Inconsolata-EX.ttf");
    let static_face = Face::parse(&inconsolata).expect("Inconsolata parses");
    assert!(!static_face.is_variable(), "Inconsolata is static");
    assert!(static_face.axes().is_empty());
}

#[test]
fn weight_changes_the_outline_ink() {
    let inter = inter();
    let letters = ['a', 'g', 'e', 'n', 'o', 's'];
    let light = ink_total(&inter, &[setting(*b"wght", 100.0)], &letters);
    let regular = ink_total(&inter, &[setting(*b"wght", 400.0)], &letters);
    let bold = ink_total(&inter, &[setting(*b"wght", 700.0)], &letters);
    assert!(regular > light, "wght=400 must be heavier than wght=100");
    assert!(bold > regular, "wght=700 must be heavier than wght=400");
}

#[test]
fn an_intermediate_weight_lies_between_its_masters() {
    let inter = inter();
    let letters = ['a', 'g', 'e', 'n', 'o', 's'];
    let regular = ink_total(&inter, &[setting(*b"wght", 400.0)], &letters);
    let mid = ink_total(&inter, &[setting(*b"wght", 550.0)], &letters);
    let bold = ink_total(&inter, &[setting(*b"wght", 700.0)], &letters);
    assert!(
        regular < mid && mid < bold,
        "wght=550 ink ({mid}) must lie strictly between 400 ({regular}) and 700 ({bold})"
    );
}

#[test]
fn a_thin_default_face_instances_to_a_heavier_weight() {
    // NotoSansSC's default instance is Thin (wght=100), so a wght=400 request
    // must genuinely instance the outline to something heavier — proving the
    // engine applies variation rather than serving the default master.
    let sc = asset("sans-fallback/NotoSansSC-Variable.ttf");
    let cjk = ['\u{4E00}'];
    let default = ink_total(&sc, &[], &cjk);
    let regular = ink_total(&sc, &[setting(*b"wght", 400.0)], &cjk);
    let bold = ink_total(&sc, &[setting(*b"wght", 700.0)], &cjk);
    assert!(
        regular > default,
        "the wght=400 instance ({regular}) must differ from the Thin default ({default})"
    );
    assert!(bold > regular, "wght=700 must be heavier than wght=400");
}

#[test]
fn advances_are_proportional_and_weight_sensitive() {
    let inter = inter();
    let regular = face_at(&inter, &[setting(*b"wght", 400.0)]);
    let advance = |face: &Face<'_>, ch: char| -> i32 {
        let glyph = face.glyph_for(u32::from(ch)).expect("glyph");
        face.advance(glyph).expect("advance")
    };
    assert!(
        advance(&regular, 'i') < advance(&regular, 'W'),
        "a proportional face advances 'i' less than 'W'"
    );

    let bold = face_at(&inter, &[setting(*b"wght", 700.0)]);
    let glyph = regular.glyph_for(u32::from('n')).expect("n");
    assert!(
        bold.advance(glyph).expect("bold advance") >= regular.advance(glyph).expect("advance"),
        "a heavier weight does not advance less"
    );
}

#[test]
fn a_static_monospace_face_keeps_one_advance() {
    let inconsolata = asset("mono/Inconsolata-EX.ttf");
    let face = Face::parse(&inconsolata).expect("parses");
    let uniform = i32::from(face.uniform_advance().expect("monospace"));
    for &(code, glyph) in face.mapped() {
        let advance = face.advance(glyph).expect("advance");
        assert!(
            advance == uniform || advance == 0,
            "U+{code:04X} advances {advance}, not the uniform {uniform}"
        );
    }
}

#[test]
fn computed_advance_agrees_with_rendered_ink_extent() {
    // A wrongly-scaled advance delta would leave the ink far outside a sane
    // window around the advance; assert the ink sits inside one.
    let inter = inter();
    let face = face_at(&inter, &[setting(*b"wght", 400.0)]);
    let units = f64::from(face.units_per_em());
    for ch in ['n', 'o', 'H', 'M', 'g'] {
        let glyph = face.glyph_for(u32::from(ch)).expect("glyph");
        let advance_px = f64::from(face.advance(glyph).expect("advance")) * PX / units;
        let raster = face
            .rasterise_proportional(glyph, PX, BASELINE, HEIGHT)
            .expect("rasterises");
        let right = f64::from(raster.left + i32::try_from(raster.width).expect("width fits i32"));
        assert!(
            f64::from(raster.left) >= -advance_px - 2.0,
            "{ch}: ink starts far left of the advance"
        );
        assert!(
            right <= advance_px * 2.0 + 2.0,
            "{ch}: ink extends far past the advance"
        );
    }
}

#[test]
fn proportional_rasterisation_is_tight_and_positioned() {
    let inter = inter();
    let face = face_at(&inter, &[setting(*b"wght", 400.0)]);
    let raster = |ch: char| {
        let glyph = face.glyph_for(u32::from(ch)).expect("glyph");
        face.rasterise_proportional(glyph, PX, BASELINE, HEIGHT)
            .expect("rasterises")
    };

    let space = raster(' ');
    assert_eq!(space.width, 0, "a space has no ink");
    assert_eq!(space.left, 0);
    assert!(space.coverage.is_empty());

    let narrow = raster('l');
    let wide = raster('M');
    assert!(narrow.width < wide.width, "'l' is narrower than 'M'");

    for glyph in [&narrow, &wide] {
        assert_eq!(
            glyph.coverage.len(),
            (glyph.width * glyph.height) as usize,
            "coverage is exactly the bitmap"
        );
        assert!(
            glyph.coverage.iter().all(|&c| c <= 15),
            "coverage stays 4-bit"
        );
        assert_eq!(
            leftmost_inked_col(glyph),
            Some(0),
            "ink starts at the reported left column (the bitmap is tight)"
        );
    }
}

#[test]
fn a_static_face_rasters_are_unchanged_by_the_refactor() {
    // A golden over the primary console face at its native geometry: the
    // generated atlas is checked against this rasteriser, so a static face
    // must be byte-identical to before variable-font support landed.
    let inconsolata = asset("mono/Inconsolata-EX.ttf");
    let face = Face::parse(&inconsolata).expect("parses");
    let advance = face.uniform_advance().expect("monospace");
    let geometry = CellGeometry::derive(&face, advance, ATLAS_EM_PX).expect("geometry");
    let mut total: u64 = 0;
    for ch in ['A', 'B', 'g', '@', 'W', 'i', 'l', 'M'] {
        let glyph = face.glyph_for(u32::from(ch)).expect("glyph");
        let coverage = face
            .rasterise_glyph(glyph, &geometry, f64::from(ATLAS_EM_PX), geometry.width)
            .expect("rasterises");
        total = total.wrapping_add(fnv(&coverage));
    }
    assert_eq!(
        total, 0xf927_aac1_0bad_1a3e,
        "a static face's rasterisation changed"
    );
}

#[test]
fn cjk_faces_map_and_rasterise_their_scripts() {
    let sc = asset("sans-fallback/NotoSansSC-Variable.ttf");
    let sc_face = Face::parse(&sc).expect("SC parses");
    for ch in ['\u{4E00}', '\u{3042}'] {
        let glyph = sc_face
            .glyph_for(u32::from(ch))
            .unwrap_or_else(|| panic!("SC maps {ch:?}"));
        let raster = sc_face
            .rasterise_proportional(glyph, PX, BASELINE, HEIGHT)
            .expect("rasterises");
        assert!(ink(&raster.coverage) > 0, "{ch:?} drew no ink");
    }

    let kr = asset("sans-fallback/NotoSansKR-Variable.ttf");
    let kr_face = Face::parse(&kr).expect("KR parses");
    let glyph = kr_face
        .glyph_for(u32::from('\u{AC00}'))
        .expect("KR maps U+AC00");
    let raster = kr_face
        .rasterise_proportional(glyph, PX, BASELINE, HEIGHT)
        .expect("rasterises");
    assert!(ink(&raster.coverage) > 0, "U+AC00 drew no ink");
}
