use super::{params, MaterialTile, Mip, Quality, Texel, MAX_OCTAVES, MIP_LEVELS, TILE_SIDE};
use tairix_wintersun_world::biome::Material;

#[test]
fn a_mip_beyond_the_chain_cannot_be_built() {
    assert!(Mip::new(0).is_some());
    assert!(Mip::new(MIP_LEVELS - 1).is_some());
    assert!(Mip::new(MIP_LEVELS).is_none());
    assert_eq!(Mip::coarsest().level(), MIP_LEVELS - 1);
}

#[test]
fn mip_sides_halve_down_the_chain() {
    let mut expected = TILE_SIDE;
    for level in 0..MIP_LEVELS {
        let mip = Mip::new(level).expect("level is in the chain");
        assert_eq!(mip.side(), expected);
        expected /= 2;
    }
    assert_eq!(Mip::coarsest().side(), 8);
}

#[test]
fn density_picks_a_finer_mip_the_closer_the_camera_is() {
    let shift = 5;
    let span = 1u32 << shift;
    assert_eq!(Mip::for_density(shift, 1).level(), 0);
    assert_eq!(Mip::for_density(shift, span).level(), 0);
    assert_eq!(Mip::for_density(shift, span * 2).level(), 1);
    assert_eq!(Mip::for_density(shift, span * 8).level(), 3);
    // Far beyond the chain still lands inside it.
    assert_eq!(Mip::for_density(shift, span * 4096).level(), MIP_LEVELS - 1);
}

#[test]
fn quality_caps_and_sheds() {
    assert_eq!(Quality::new(99).octaves(), MAX_OCTAVES);
    assert_eq!(Quality::FULL.octaves(), MAX_OCTAVES);
    let mut quality = Quality::FULL;
    let mut steps = 0;
    while let Some(next) = quality.shed() {
        assert!(next.octaves() < quality.octaves());
        quality = next;
        steps += 1;
    }
    assert_eq!(quality.octaves(), 0);
    assert_eq!(steps, MAX_OCTAVES);
}

#[test]
fn every_material_has_a_plausible_parameter_set() {
    for material in Material::ALL {
        let p = params(material);
        assert!(p.grain_shift >= 1 && p.grain_shift <= 8, "{material:?}");
        assert!(p.grain_cell_log2 >= 1, "{material:?}");
        // Relief may not swing the standing height off either end, or the
        // material would clip flat over part of its own surface.
        let half = i32::from(p.relief) / 2;
        assert!(i32::from(p.stand) - half >= 0, "{material:?} clips low");
        assert!(
            i32::from(p.stand) + half <= i32::from(u8::MAX),
            "{material:?} clips high"
        );
    }
}

#[test]
fn materials_stand_in_a_sensible_order() {
    // The height field is what decides which material wins a shared
    // pixel, so the standing order *is* the art direction: rock through
    // gravel through soil through water.
    let stand = |m| params(m).stand;
    assert!(stand(Material::Rock) > stand(Material::Gravel));
    assert!(stand(Material::Gravel) > stand(Material::Sand));
    assert!(stand(Material::Sand) > stand(Material::Water));
    assert!(stand(Material::Glacier) > stand(Material::Snowfield));
}

#[test]
fn a_flat_tier_exists_for_every_material() {
    for material in Material::ALL {
        let p = params(material);
        let flat = p.flat();
        assert_eq!(
            (flat.r, flat.g, flat.b),
            (p.ramp.mid.r, p.ramp.mid.g, p.ramp.mid.b)
        );
        assert_eq!(flat.height, p.stand);
    }
}

#[test]
fn a_tile_is_the_side_its_mip_says() {
    for level in 0..MIP_LEVELS {
        let mip = Mip::new(level).expect("level is in the chain");
        let tile = MaterialTile::synthesise(Material::Gravel, mip, Quality::FULL)
            .expect("a tile fits in test memory");
        assert_eq!(tile.side(), mip.side());
        let area = usize::try_from(mip.side() * mip.side()).expect("a tile fits a usize");
        assert_eq!(tile.texels().len(), area);
        assert_eq!(tile.mip(), mip);
        assert_eq!(tile.material(), Material::Gravel);
    }
}

#[test]
fn synthesis_is_reproducible() {
    let a = MaterialTile::synthesise(Material::Moor, Mip::BASE, Quality::FULL)
        .expect("a tile fits in test memory");
    let b = MaterialTile::synthesise(Material::Moor, Mip::BASE, Quality::FULL)
        .expect("a tile fits in test memory");
    assert_eq!(a, b);
}

#[test]
fn two_materials_do_not_synthesise_the_same_tile() {
    let a = MaterialTile::synthesise(Material::Moor, Mip::BASE, Quality::FULL)
        .expect("a tile fits in test memory");
    let b = MaterialTile::synthesise(Material::Tundra, Mip::BASE, Quality::FULL)
        .expect("a tile fits in test memory");
    assert_ne!(a.texels(), b.texels());
}

#[test]
fn quality_changes_the_tile() {
    // If it did not, the cache's generation token would be meaningless
    // and shedding an octave would cost detail without saving work.
    let full = MaterialTile::synthesise(Material::Rock, Mip::BASE, Quality::FULL)
        .expect("a tile fits in test memory");
    let thin = MaterialTile::synthesise(Material::Rock, Mip::BASE, Quality::new(1))
        .expect("a tile fits in test memory");
    assert_ne!(full.texels(), thin.texels());
}

#[test]
fn a_tile_wraps_seamlessly_at_its_own_side() {
    // The property the whole synthesis exists for: drawn end to end, a
    // tile must not show where it restarts.
    for material in [Material::Gravel, Material::Rock, Material::Sand] {
        let tile = MaterialTile::synthesise(material, Mip::BASE, Quality::FULL)
            .expect("a tile fits in test memory");
        let side = tile.side();
        let mut worst = 0i32;
        for y in 0..side {
            let left = tile.texel(0, y);
            let wrapped = tile.texel(side, y);
            assert_eq!(left, wrapped, "the read does not wrap at all");

            // Across the join, neighbouring texels must be as close as
            // neighbours anywhere else in the tile.
            let across = i32::from(tile.texel(side - 1, y).r) - i32::from(left.r);
            worst = worst.max(across.abs());
        }
        let mut interior = 0i32;
        for y in 0..side {
            for x in 1..side {
                let step = i32::from(tile.texel(x, y).r) - i32::from(tile.texel(x - 1, y).r);
                interior = interior.max(step.abs());
            }
        }
        assert!(
            worst <= interior,
            "{material:?} steps {worst} across the join against {interior} inside it",
        );
    }
}

#[test]
fn texel_reads_wrap_rather_than_clamp() {
    let tile = MaterialTile::synthesise(Material::Sand, Mip::BASE, Quality::FULL)
        .expect("a tile fits in test memory");
    let side = tile.side();
    assert_eq!(tile.texel(3, 7), tile.texel(3 + side, 7 + side));
    assert_eq!(tile.texel(3, 7), tile.texel(3 + side * 5, 7));
}

#[test]
fn a_tile_carries_real_variation() {
    // A synthesis that produced one colour would pass every other test
    // here and draw a flat plane.
    let tile = MaterialTile::synthesise(Material::Tundra, Mip::BASE, Quality::FULL)
        .expect("a tile fits in test memory");
    let (mut low, mut high) = (u8::MAX, 0u8);
    let (mut low_h, mut high_h) = (u8::MAX, 0u8);
    for texel in tile.texels() {
        low = low.min(texel.r);
        high = high.max(texel.r);
        low_h = low_h.min(texel.height);
        high_h = high_h.max(texel.height);
    }
    assert!(high - low > 20, "colour spread is only {}", high - low);
    assert!(
        high_h - low_h > 10,
        "height spread is only {}",
        high_h - low_h
    );
}

#[test]
fn zero_octaves_gives_the_flat_tone() {
    let tile = MaterialTile::synthesise(Material::Moor, Mip::BASE, Quality::new(0))
        .expect("a tile fits in test memory");
    let flat = params(Material::Moor).flat();
    assert!(tile.texels().iter().all(|&t| t == flat));
}

#[test]
fn scrubbing_clears_the_texels() {
    let mut tile = MaterialTile::synthesise(Material::Ashland, Mip::coarsest(), Quality::FULL)
        .expect("a tile fits in test memory");
    tile.scrub();
    assert!(tile.texels().iter().all(|&t| t == Texel::VOID));
}

#[test]
fn payload_bytes_are_four_per_texel() {
    let tile = MaterialTile::synthesise(Material::Rock, Mip::coarsest(), Quality::FULL)
        .expect("a tile fits in test memory");
    assert_eq!(tile.payload_bytes(), tile.texels().len() * 4);
}

#[test]
fn an_absurd_grain_shift_picks_a_mip_rather_than_shifting_off() {
    // `for_density` is public and takes a caller's shift; `1 << 32` is
    // not a value a machine can produce.
    for shift in [31, 32, 64, u32::MAX] {
        assert_eq!(Mip::for_density(shift, u32::MAX).level(), 0);
    }
}
