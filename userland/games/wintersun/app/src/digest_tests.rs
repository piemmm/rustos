//! The reference frames are drawn, reproducible, and sensitive to
//! everything beneath them.

use super::*;

#[test]
fn the_reference_frames_agree_with_the_constant() {
    let produced = reference().expect("the reference realm generates");
    assert_eq!(
        produced, REFERENCE_DIGEST,
        "the client frame digest moved to {produced:#018x}; if that was \
         intended, write the new value down deliberately"
    );
}

#[test]
fn the_digest_is_the_same_every_time() {
    let first = reference().expect("the reference realm generates");
    let second = reference().expect("the reference realm generates");
    assert_eq!(first, second, "two runs of one binary disagreed");
}

#[test]
fn the_frames_are_not_blank() {
    // A digest over an all-transparent buffer would be perfectly stable
    // and perfectly worthless, which is the failure mode a renderer's
    // reproducibility test is most likely to have.
    let params = reference_params().expect("the spec is in range");
    let field = RealmField::generate(params).expect("the realm generates");
    let roads = RoadDecals::from_realm(&field).expect("the roads fit");
    let decals = roads.decals().expect("the decals fit");
    let warp = Warp::new(params.seed());
    let fray = Fray::new(params.seed());
    let camera = Camera::new(
        WorldPoint { x: 0, y: 0 },
        Zoom::FURTHEST,
        realm_bounds(params),
    );
    let view = Viewport::new(FRAME_WIDTH, FRAME_HEIGHT, crate::quality::RenderScale::ONE)
        .expect("a real window");
    let (width, height) = view.render();
    let held = generate(&field, camera.visible(width, height)).expect("the chunks generate");
    let borrowed = borrow(&held).expect("the window fits");
    let chunks = ChunkWindow::new(&borrowed).expect("generated in coordinate order");

    PRESSURE.report(PressureBand::Normal);
    let mut cache = MaterialCache::new(
        "wintersun-blank-test",
        CACHE_BACKING_BYTES,
        &PRESSURE,
        &SINK,
    );
    let mut renderer = Renderer::new();
    let mut target = alloc::vec![Pixel::TRANSPARENT; view.render_pixels()];
    renderer
        .render(
            &mut target,
            &view,
            &Scene {
                camera,
                chunks,
                decals: &decals,
                fray: &fray,
                warp: &warp,
                sun: Sun::winter(),
                sky: Sky::winter(),
                ladder: Ladder::FULL,
            },
            &mut cache,
            &tairix_parallel::SERIAL,
            &Stopped,
        )
        .expect("the frame draws");

    assert!(
        target.iter().all(|p| p.a == 255),
        "the ground pass left transparent pixels"
    );
    assert_eq!(
        renderer.grid().unmapped(),
        0,
        "every visible chunk was generated"
    );
    let distinct = target
        .iter()
        .map(|p| (p.r, p.g, p.b))
        .collect::<alloc::collections::BTreeSet<_>>();
    assert!(
        distinct.len() > 16,
        "a frame of {} distinct colours is a flat fill, not terrain",
        distinct.len()
    );
}

#[test]
fn the_digest_folds_in_the_art_constant() {
    // Not a claim about the value, a claim about the dependency: the
    // art's own digest is part of the input, so a change to the ground
    // moves this number rather than passing unnoticed.
    let without = FastHash::with_seed(REFERENCE_SEED);
    let mut with = FastHash::with_seed(REFERENCE_SEED);
    with.write_u64(art::REFERENCE_DIGEST);
    assert_ne!(without.finish(), with.finish());
}

#[test]
fn the_reference_frames_cover_both_ends_of_both_knobs() {
    let steps: alloc::vec::Vec<u8> = FRAMES.iter().map(|(step, _)| *step).collect();
    let zooms: alloc::vec::Vec<Zoom> = FRAMES.iter().map(|(_, zoom)| *zoom).collect();
    assert!(
        steps.contains(&0) && steps.contains(&Ladder::MAX_STEP),
        "a digest that never sheds proves nothing about the ladder"
    );
    assert!(
        zooms.contains(&Zoom::DEFAULT) && zooms.contains(&Zoom::FURTHEST),
        "a digest at one zoom exercises one mip band"
    );
}
