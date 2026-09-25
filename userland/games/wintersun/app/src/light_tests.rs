//! The sun shades relative to the palette, saturates where the ground
//! stops being walkable, and never lights ground the client has not got.

use super::*;
use tairix_wintersun_world::biome::Material;
use tairix_wintersun_world::chunk::ChunkWindow;

fn sun() -> Sun {
    Sun::winter()
}

#[test]
fn a_slope_facing_neither_way_leaves_the_palette_alone() {
    let gain = sun().gain(sun().level(0, 0));
    assert_eq!(
        (gain.r, gain.g, gain.b),
        (UNSHADED, UNSHADED, UNSHADED),
        "flat ground was tinted"
    );
    let ground = Pixel {
        r: 104,
        g: 106,
        b: 110,
        a: 255,
    };
    let lit = apply(
        ground,
        Lit { gain, mist: 0 },
        Sky {
            mist: Color::rgb(0, 0, 0),
            mist_depth: 0,
        },
    );
    assert_eq!(
        lit, ground,
        "unshaded, unmisted ground is the material itself"
    );
}

#[test]
fn a_slope_toward_the_sun_brightens_and_one_away_darkens() {
    let sun = sun();
    let lit = sun.level(64, 64);
    let shade = sun.level(-64, -64);
    assert!(
        lit > 128 && shade < 128,
        "the sun lit {lit} and shaded {shade}"
    );
    let bright = sun.gain(lit);
    let dark = sun.gain(shade);
    assert!(bright.r > UNSHADED, "a lit face did not brighten");
    assert!(dark.r < UNSHADED, "a shaded face did not darken");
    // The low sun is warm and its shadow is cold: the lit face gains
    // more red than blue, and the shaded one the reverse.
    assert!(bright.r > bright.b);
    assert!(dark.b > dark.r);
}

#[test]
fn shading_saturates_where_the_ground_stops_being_walkable() {
    let sun = sun();
    let cliff = sun.level(SLOPE_FULL * 4, SLOPE_FULL * 4);
    let edge = sun.level(SLOPE_FULL, SLOPE_FULL);
    assert_eq!(
        cliff, edge,
        "a face four times as steep as a cliff shaded differently from a cliff"
    );
    let walkable = sun.level(SLOPE_FULL / 2, 0);
    assert!(
        walkable < edge,
        "walkable ground was already at the shading limit"
    );
}

#[test]
fn a_sun_directly_overhead_shades_nothing() {
    let overhead = Sun::new(0, 0, Color::rgb(255, 255, 255), Color::rgb(0, 0, 0), 200);
    for (gx, gy) in [(0, 0), (64, -64), (-1_000, 1_000)] {
        assert_eq!(overhead.level(gx, gy), 128, "a vertical sun shaded a slope");
    }
}

#[test]
fn mist_pools_in_hollows_and_thins_on_high_ground() {
    let low = shade_at_height(-400);
    let high = shade_at_height(MIST_CEILING * 2);
    assert!(
        low > high,
        "mist of {low} in a hollow and {high} on a summit"
    );
    assert_eq!(high, 0, "mist above the ceiling collects at all");
}

/// The mist a lattice of uniform height produces.
fn shade_at_height(ground: i32) -> u8 {
    let below = (MIST_CEILING - ground).clamp(0, MIST_CEILING);
    u8::try_from(i64::from(below) * 255 / i64::from(MIST_CEILING)).unwrap_or(255)
}

#[test]
fn ground_the_client_does_not_hold_is_not_lit() {
    let empty: [&tairix_wintersun_world::chunk::Chunk; 0] = [];
    let window = ChunkWindow::new(&empty).expect("an empty window is sorted");
    let mut grid = crate::terrain::TerrainGrid::new();
    grid.rebuild(
        &window,
        tairix_wintersun_art::decal::Bounds {
            min_x: 0,
            min_y: 0,
            max_x: 2_000,
            max_y: 2_000,
        },
        &[],
        &tairix_wintersun_art::decal::Fray::new(1),
    )
    .expect("the grid fits");
    let shading = Shading {
        sun: sun(),
        shift: 1,
        relief: Relief::Wide,
        step: 32,
        origin: WorldPoint { x: 0, y: 0 },
    };
    assert_eq!(
        shade_at(&grid, &shading, WorldPoint { x: 100, y: 100 }),
        Lit::NEUTRAL,
        "unmapped ground was lit as though it were a plain"
    );
}

#[test]
fn unmapped_ground_has_no_gradient_whatever_the_relief() {
    let mut grid = crate::terrain::TerrainGrid::new();
    let empty: [&tairix_wintersun_world::chunk::Chunk; 0] = [];
    let window = ChunkWindow::new(&empty).expect("sorted");
    grid.rebuild(
        &window,
        tairix_wintersun_art::decal::Bounds {
            min_x: 0,
            min_y: 0,
            max_x: 1_000,
            max_y: 1_000,
        },
        &[],
        &tairix_wintersun_art::decal::Fray::new(1),
    )
    .expect("the grid fits");
    for relief in [Relief::Wide, Relief::Narrow, Relief::Flat] {
        assert_eq!(
            gradient(&grid, relief, WorldPoint { x: 0, y: 0 }),
            None,
            "unmapped ground reported a gradient under {relief:?}"
        );
    }
}

#[test]
fn the_figures_are_lit_by_the_sun_the_harness_measures_them_under() {
    assert_eq!(
        Sun::winter().light(),
        Ok(tairix_wintersun_figure::reference::Reference::light().expect("the harness sun")),
        "the game draws its figures under a light the art harness never measured"
    );
    let overhead = Sun::new(0, 0, Color::rgb(0, 0, 0), Color::rgb(0, 0, 0), 0);
    assert!(
        overhead.light().is_ok(),
        "an overhead sun left the figures unlit"
    );
}

#[test]
fn the_mist_a_figure_is_veiled_by_is_the_texel_at_its_feet() {
    let view = crate::view::Viewport::new(32, 16, crate::quality::RenderScale::ONE)
        .expect("a real window");
    let mut buffer = LightBuffer::new();
    buffer.resize(&view, 2).expect("the buffer fits");
    let (width, _) = buffer.extent();
    let at = |x: u32, y: u32| (y * width + x) as usize;
    buffer.texels[at(2, 1)].mist = 255;
    buffer.texels[at(0, 0)].mist = 128;
    let sky = Sky::winter();
    let full = mist_of(
        Lit {
            gain: Lit::NEUTRAL.gain,
            mist: 255,
        },
        sky,
    );
    // A texel covers four pixels either way at this shift.
    assert_eq!(buffer.mist_at(8, 4, sky), full);
    assert_eq!(buffer.mist_at(11, 7, sky), full);
    assert_eq!(buffer.mist_at(12, 4, sky), 0);
    // Off the view, the nearest texel answers rather than none.
    assert_eq!(buffer.mist_at(-40, -1, sky), buffer.mist_at(0, 0, sky));
    assert!(buffer.mist_at(0, 0, sky) > 0);
    assert_eq!(buffer.mist_at(i32::MAX, i32::MAX, sky), 0);
    assert_eq!(
        LightBuffer::new().mist_at(3, 3, sky),
        0,
        "no buffer is no mist"
    );
}

#[test]
fn the_buffer_is_coarser_than_the_frame_and_has_a_texel_past_each_edge() {
    let view = crate::view::Viewport::new(128, 64, crate::quality::RenderScale::ONE)
        .expect("a real window");
    let mut buffer = LightBuffer::new();
    for shift in [1u32, 2, 3] {
        buffer.resize(&view, shift).expect("the buffer fits");
        let (w, h) = buffer.extent();
        assert_eq!(buffer.shift(), shift);
        assert_eq!((w, h), ((128 >> shift) + 2, (64 >> shift) + 2));
        assert_eq!(buffer.scratch_len(), w as usize);
    }
}

#[test]
fn compositing_over_a_neutral_buffer_changes_nothing() {
    let view =
        crate::view::Viewport::new(32, 8, crate::quality::RenderScale::ONE).expect("a real window");
    let mut buffer = LightBuffer::new();
    buffer.resize(&view, 1).expect("the buffer fits");
    let sky = Sky {
        mist: Color::rgb(0, 0, 0),
        mist_depth: 0,
    };
    let ground = Pixel {
        r: 90,
        g: 120,
        b: 60,
        a: 255,
    };
    let mut row = [ground; 32];
    let mut scratch = alloc::vec![Lit::NEUTRAL; buffer.scratch_len()];
    buffer.composite_row(&mut row, &mut scratch, sky, 0);
    assert!(
        row.iter().all(|p| *p == ground),
        "a neutral light moved the ground"
    );
}

#[test]
fn a_composite_with_no_buffer_leaves_the_row_alone() {
    let buffer = LightBuffer::new();
    let ground = Pixel {
        r: 10,
        g: 20,
        b: 30,
        a: 255,
    };
    let mut row = [ground; 4];
    let mut scratch = alloc::vec::Vec::new();
    buffer.composite_row(&mut row, &mut scratch, Sky::winter(), 0);
    assert!(row.iter().all(|p| *p == ground));
}

#[test]
fn a_material_under_full_mist_reads_as_the_mist_and_not_as_itself() {
    let sky = Sky {
        mist: Color::rgb(200, 210, 220),
        mist_depth: 255,
    };
    let rock = tairix_wintersun_art::material::params(Material::Rock).flat();
    let ground = Pixel {
        r: rock.r,
        g: rock.g,
        b: rock.b,
        a: 255,
    };
    let fogged = apply(
        ground,
        Lit {
            gain: Color::rgb(UNSHADED, UNSHADED, UNSHADED),
            mist: 255,
        },
        sky,
    );
    assert_eq!(
        (fogged.r, fogged.g, fogged.b),
        (sky.mist.r, sky.mist.g, sky.mist.b)
    );
    assert_eq!(fogged.a, 255, "the mist made the ground translucent");
}
