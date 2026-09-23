//! What the art harness measures, checked against pictures it makes itself.

use tairix_raster::surface::Surface;
use tairix_raster::Color;
use tairix_wintersun_figure::humanoid;
use tairix_wintersun_figure::mesh::{self, LEVELS};
use tairix_wintersun_figure::reference;
use tairix_wintersun_figure::species::Species;
use tairix_wintersun_figure::tint::Tint;

use super::{
    classify, contrast, coverage, declared, luminance, regions, shade, shades, shares, MIN_REGIONS,
    MIN_TONE_SHARE,
};

/// Four tones a picture can be painted in, far enough apart to classify.
const TONES: [Color; 4] = [
    Color::rgb(0xCE, 0x9E, 0x78),
    Color::rgb(0x4C, 0x5A, 0x6E),
    Color::rgb(0x44, 0x4C, 0x58),
    Color::rgb(0x2E, 0x5E, 0x38),
];

fn painted(fill: Color, side: u32, rows: u32) -> Surface {
    let mut surface = Surface::new(side, side).expect("a surface");
    surface.fill_rect(0, 0, side, rows, fill);
    surface
}

/// Coverage is the alpha-weighted fraction of the cell, so a known
/// rectangle measures its own area.
#[test]
fn coverage_is_the_fraction_of_the_cell_the_figure_marks() {
    let surface = painted(Color::rgb(0x40, 0x40, 0x40), 32, 8);
    let measured = coverage(&surface);
    assert!(
        (measured - 0.25).abs() < 1e-9,
        "a quarter-filled cell measured {measured}"
    );
    assert!(coverage(&Surface::new(32, 32).expect("a surface")) < 1e-12);
}

/// A figure is drawn in a closed set of tones, and a pixel is read as the
/// nearest of them — which is what survives the antialiasing between two
/// shades of one base.
#[test]
fn a_pixel_reads_as_the_nearest_declared_tone() {
    let every = shades(&TONES);
    assert_eq!(every.len(), TONES.len() * LEVELS as usize);
    for (slot, base) in TONES.iter().enumerate() {
        for level in 0..LEVELS {
            let lit = mesh::shaded(*base, level);
            let slot = u8::try_from(slot).expect("a small palette");
            assert_eq!(
                shade(lit.premultiply(), &every),
                Some(slot),
                "{lit:?} must read as the tone it is a shade of"
            );
        }
    }
    // Nothing like any of them, and nothing covered enough to judge.
    assert_eq!(
        shade(Color::rgb(0x00, 0xFF, 0x00).premultiply(), &every),
        None
    );
    assert_eq!(
        shade(
            Color::rgba(TONES[0].r, TONES[0].g, TONES[0].b, 8).premultiply(),
            &every
        ),
        None
    );
}

/// A blob is one region and a figure is several: the count is what says the
/// head and the limbs have not merged into the trunk.
#[test]
fn a_blob_resolves_into_one_region_and_a_figure_into_several() {
    let blob = painted(TONES[1], 32, 20);
    let count = regions(&blob, &classify(&blob, &shades(&TONES)));
    assert_eq!(count, 1, "one tone in one run is one region");
    assert!(count < MIN_REGIONS, "the bound must reject a blob");

    let mut several = Surface::new(32, 32).expect("a surface");
    for (index, tone) in TONES.iter().enumerate() {
        let y = u32::try_from(index).expect("a small palette") * 8;
        several.fill_rect(0, y, 32, 4, *tone);
    }
    assert_eq!(
        regions(&several, &classify(&several, &shades(&TONES))),
        4,
        "four separated tones are four regions"
    );

    // A run below the floor is not something a player can read.
    let mut speck = Surface::new(32, 32).expect("a surface");
    speck.fill_rect(0, 0, 1, 1, TONES[0]);
    assert_eq!(regions(&speck, &classify(&speck, &shades(&TONES))), 0);
}

/// Contrast is measured against the tone's lit end, and the shares are what
/// decide whether a tone counts at all.
#[test]
fn contrast_is_taken_over_the_tones_that_cover_the_figure() {
    let white = Color::rgb(0xFF, 0xFF, 0xFF);
    let black = Color::rgb(0x00, 0x00, 0x00);
    let ratio = contrast(white, black);
    assert!(
        (ratio - 21.0).abs() < 0.01,
        "black on white measured {ratio}"
    );
    assert!((contrast(white, white) - 1.0).abs() < 1e-9);
    assert!(luminance(white) > luminance(black));

    let surface = painted(TONES[1], 32, 16);
    let measured = shares(&classify(&surface, &shades(&TONES)), TONES.len());
    assert!(
        (measured[1] - 1.0).abs() < 1e-9,
        "one tone covering the figure is its whole share"
    );
    assert!(measured[1] >= MIN_TONE_SHARE);
}

/// A figure is held to exactly the colours its own record chose: every role
/// one of its surfaces is drawn in, once, and a role nothing is drawn in
/// claims no pixels.
#[test]
fn a_figure_declares_the_tones_its_surfaces_are_drawn_in() {
    for species in Species::ALL {
        let identity = reference::identity(species).expect("a real record");
        let rig = humanoid::rig(&identity).expect("it builds");
        let tones = declared(&rig);
        for part in rig.parts() {
            assert!(tones.contains(&rig.tints().get(part.tint())));
        }
        for (index, tone) in tones.iter().enumerate() {
            assert!(
                !tones[index + 1..].contains(tone),
                "{tone:?} is declared twice"
            );
        }
        let marked = rig.parts().iter().any(|part| part.tint() == Tint::Markings);
        assert_eq!(marked, !species.markings().is_empty());
    }
}

/// The bare form is what `ci` runs, so it has to be green on the committed
/// tree — and it is the whole gate: the ledger's drift and every bound.
#[test]
fn the_committed_ledger_matches_the_shipped_figures() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("the workspace root");
    super::check(&root).expect("the committed ledger is in step with the figures");
}
