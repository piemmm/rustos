//! The reference scene draws a real picture, and the same one every time and
//! at every size a window gives it.

use alloc::collections::BTreeSet;
use alloc::vec::Vec;

use tairix_raster::color::Pixel;
use tairix_raster::surface::Surface;
use tairix_reclaim::{GrowthAllowance, PressureBand, PressureGauge};
use tairix_wintersun_art::cache::MaterialCache;
use tairix_wintersun_figure::motion::Set;

use super::{cache, Discard, Unpressured, World, CACHE_BACKING_BYTES, CAST_LEN};
use crate::error::ClientError;
use crate::frame::Renderer;

/// Window extents that take every path a window holding the scene can: the
/// smallest client the game declares, a desktop-sized one, and one past the
/// software path's cap, drawn reduced and resampled up.
const WINDOW_SIZES: [(u32, u32); 3] = [(320, 240), (1024, 768), (2600, 1500)];

/// Draw the scene into a window of `width`×`height` with `cache`, returning
/// its pixels and how many figures it drew.
fn window_frame(
    world: &mut World,
    width: u32,
    height: u32,
    cache: &mut MaterialCache,
) -> (Vec<Pixel>, usize) {
    let set = Set::new().expect("the shipped set");
    let clips = set.clips().expect("the shipped clips");
    let mut window = Surface::new(width, height).expect("a frame fits");
    let mut renderer = Renderer::new();
    world
        .draw_window(
            &clips,
            &mut window,
            &mut None,
            &mut renderer,
            cache,
            &tairix_parallel::SERIAL,
        )
        .expect("the scene draws");
    assert_eq!(
        renderer.grid().unmapped(),
        0,
        "the scene drew ground it had not generated"
    );
    (window.pixels().to_vec(), renderer.figures())
}

/// A gauge at critical pressure, which admits no tile at all.
struct Starved;

impl PressureGauge for Starved {
    fn sample(&self) -> PressureBand {
        PressureBand::Critical
    }

    fn growth_allowance(&self) -> GrowthAllowance {
        GrowthAllowance::refused()
    }
}

/// A drawing pressure cost a tile is refused rather than handed back in flat
/// tones as though it were the scene.
#[test]
fn a_drawing_the_cache_cost_a_tile_is_refused() {
    let mut world = World::generate().expect("the reference realm generates");
    let set = Set::new().expect("the shipped set");
    let clips = set.clips().expect("the shipped clips");
    let mut window = Surface::new(320, 240).expect("a frame fits");
    let mut renderer = Renderer::new();
    let drawn = world.draw_window(
        &clips,
        &mut window,
        &mut None,
        &mut renderer,
        &mut cache(&Starved),
        &tairix_parallel::SERIAL,
    );
    assert_eq!(drawn, Err(ClientError::OutOfMemory));
    assert_ne!(renderer.refused_tiles(), 0, "the cache refused the tiles");
}

/// A picture that is stable but blank would prove nothing, which is the
/// failure a reproducibility check is most likely to have.
#[test]
fn every_window_size_draws_real_terrain_with_the_whole_cast() {
    let mut world = World::generate().expect("the reference realm generates");
    for (width, height) in WINDOW_SIZES {
        let (pixels, figures) = window_frame(&mut world, width, height, &mut cache(&Unpressured));
        assert!(
            pixels.iter().all(|p| p.a == 255),
            "{width}x{height} left transparent pixels"
        );
        assert_eq!(figures, CAST_LEN, "{width}x{height} lost a figure");
        let colours = pixels
            .iter()
            .map(|p| (p.r, p.g, p.b))
            .collect::<BTreeSet<_>>();
        assert!(
            colours.len() > 16,
            "{width}x{height} is a flat fill of {} colours",
            colours.len()
        );
    }
}

/// The pinned cache never decides a pixel: a window drawn with it matches one
/// drawn with a cache sixteen times larger.
#[test]
fn the_pinned_cache_never_shapes_the_picture() {
    let mut world = World::generate().expect("the reference realm generates");
    for (width, height) in WINDOW_SIZES {
        let (pinned, _) = window_frame(&mut world, width, height, &mut cache(&Unpressured));
        let mut ample = MaterialCache::new(
            "wintersun-reference-ample",
            CACHE_BACKING_BYTES * 16,
            &Unpressured,
            &Discard,
        );
        let (unbounded, _) = window_frame(&mut world, width, height, &mut ample);
        assert!(
            pinned == unbounded,
            "{width}x{height}: the pinned cache shaped the picture"
        );
    }
}

#[test]
fn the_scene_is_the_same_every_time_it_is_drawn() {
    let mut world = World::generate().expect("the reference realm generates");
    let (first, _) = window_frame(&mut world, 1024, 768, &mut cache(&Unpressured));
    let mut again = World::generate().expect("the reference realm generates");
    let (second, _) = window_frame(&mut again, 1024, 768, &mut cache(&Unpressured));
    assert!(
        first == second,
        "two drawings of the reference scene differ"
    );
}
