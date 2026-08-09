//! Unit tests over the committed faces.

use alloc::vec::Vec;
use std::fs;
use std::path::PathBuf;

use crate::{CellGeometry, Face, FontFamily, ATLAS_EM_PX};

/// The native cell height the atlas is authored at (ascent 13 + descent 3).
const NATIVE_HEIGHT: u32 = 16;

/// Read a committed face by its `<family>/<file>` path under
/// `lib/font/assets`.
pub(crate) fn asset(rel: &str) -> Vec<u8> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../font/assets")
        .join(rel);
    fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// The count of inked (non-zero coverage) bytes in a bitmap.
pub(crate) fn ink(bitmap: &[u8]) -> usize {
    bitmap.iter().filter(|&&c| c > 0).count()
}

fn primary_bytes() -> Vec<u8> {
    asset("mono/Inconsolata-EX.ttf")
}

fn family_bytes() -> [Vec<u8>; 4] {
    [
        asset("mono/Inconsolata-EX.ttf"),
        asset("mono/MPLUS1Code-Regular.ttf"),
        asset("mono/D2Coding-Regular.ttf"),
        asset("mono/NotoSansHebrew-ExtraCondensed.ttf"),
    ]
}

fn family_from(bytes: &[Vec<u8>; 4]) -> FontFamily<'_> {
    FontFamily::parse(&[
        bytes[0].as_slice(),
        bytes[1].as_slice(),
        bytes[2].as_slice(),
        bytes[3].as_slice(),
    ])
    .expect("family parses")
}

/// The pixels-per-em and cell geometry the runtime uses to render a glyph at
/// `height` physical pixels, scaled linearly from the native reference.
fn runtime_geometry(face: &Face<'_>, advance: u16, height: u32) -> (CellGeometry, f64) {
    let native = CellGeometry::derive(face, advance, ATLAS_EM_PX).expect("native geometry");
    let width = (native.width * height).div_ceil(NATIVE_HEIGHT).max(1);
    let baseline = native.baseline * height / NATIVE_HEIGHT;
    let px_per_em = f64::from(ATLAS_EM_PX) * f64::from(height) / f64::from(NATIVE_HEIGHT);
    (
        CellGeometry {
            width,
            height,
            baseline,
        },
        px_per_em,
    )
}

#[test]
fn native_geometry_matches_the_atlas_cell() {
    let bytes = primary_bytes();
    let face = Face::parse(&bytes).expect("primary parses");
    let advance = face.uniform_advance().expect("monospace");
    let geometry = CellGeometry::derive(&face, advance, ATLAS_EM_PX).expect("geometry");
    assert_eq!(geometry.width, 8);
    assert_eq!(geometry.height, NATIVE_HEIGHT);
    assert_eq!(geometry.baseline, 13);
}

#[test]
fn ascii_resolves_to_the_primary_face_and_rasterises_with_ink() {
    let bytes = family_bytes();
    let family = family_from(&bytes);
    let advance = family.primary().uniform_advance().expect("monospace");
    let (index, glyph) = family.resolve(u32::from('A')).expect("A is covered");
    assert_eq!(index, 0, "ASCII comes from the primary face");

    let (geometry, px_per_em) = runtime_geometry(family.primary(), advance, NATIVE_HEIGHT);
    let bitmap = family
        .rasterise(index, glyph, &geometry, px_per_em, geometry.width * 2)
        .expect("rasterises");
    assert_eq!(
        bitmap.len(),
        (geometry.width * 2 * geometry.height) as usize
    );
    assert!(ink(&bitmap) > 0, "the letter A drew no ink");
    assert!(bitmap.iter().all(|&c| c <= 15), "coverage exceeds 4 bits");
}

#[test]
fn a_space_glyph_is_blank() {
    let bytes = family_bytes();
    let family = family_from(&bytes);
    let advance = family.primary().uniform_advance().expect("monospace");
    let (index, glyph) = family.resolve(u32::from(' ')).expect("space is covered");
    let (geometry, px_per_em) = runtime_geometry(family.primary(), advance, NATIVE_HEIGHT);
    let bitmap = family
        .rasterise(index, glyph, &geometry, px_per_em, geometry.width * 2)
        .expect("rasterises");
    assert_eq!(ink(&bitmap), 0, "space is not blank");
}

#[test]
fn a_larger_cell_rasterises_a_taller_glyph_directly_from_the_outline() {
    // The whole point of the engine: a bigger request rasterises a bigger
    // glyph from the outline, not an upscaled bitmap. Ink scales up with the
    // cell, and the bitmap is exactly the requested dimensions.
    let bytes = family_bytes();
    let family = family_from(&bytes);
    let advance = family.primary().uniform_advance().expect("monospace");
    let (index, glyph) = family.resolve(u32::from('B')).expect("B is covered");

    let render = |height: u32| {
        let (geometry, px_per_em) = runtime_geometry(family.primary(), advance, height);
        let bitmap = family
            .rasterise(index, glyph, &geometry, px_per_em, geometry.width * 2)
            .expect("rasterises");
        assert_eq!(bitmap.len(), (geometry.width * 2 * height) as usize);
        ink(&bitmap)
    };

    let small = render(14);
    let native = render(NATIVE_HEIGHT);
    let large = render(200);
    assert!(small > 0 && native > 0 && large > 0);
    assert!(
        native > small,
        "native glyph should have more ink than 14px"
    );
    assert!(
        large > native,
        "200px glyph should have more ink than native"
    );
}

#[test]
fn merged_repertoire_is_sorted_and_deduplicated() {
    let bytes = family_bytes();
    let family = family_from(&bytes);
    let merged = family.merged();
    assert!(!merged.is_empty());
    // Strictly ascending codepoints: earliest-face-wins leaves exactly one
    // entry per codepoint.
    assert!(
        merged.windows(2).all(|w| w[0].0 < w[1].0),
        "merged repertoire is not strictly ascending / deduplicated"
    );
    // ASCII 'A' comes from the primary face; its merged entry agrees with
    // resolution (earliest face wins) for every codepoint.
    let a = merged.iter().find(|&&(code, ..)| code == u32::from('A'));
    assert_eq!(a.map(|&(_, face, _)| face), Some(0));
    for &(code, face, glyph) in merged.iter().step_by(97) {
        assert_eq!(
            family.resolve(code),
            Some((face, glyph)),
            "merged entry disagrees with resolve at U+{code:04X}"
        );
    }
}

#[test]
fn garbage_fails_closed_rather_than_panicking() {
    assert!(Face::parse(&[]).is_err());
    assert!(Face::parse(&[0u8; 32]).is_err());
    assert!(FontFamily::parse(&[]).is_err());
}
