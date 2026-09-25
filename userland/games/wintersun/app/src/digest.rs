//! The client frame digest: the cross-target rendering claim, made over
//! a whole composited frame.
//!
//! The world generator and the simulation each carry their own
//! four-target vertical, because each does arithmetic whose identity
//! across targets is a property of the code. The ground art carries a
//! constant but no vertical: it contains no floating point at all, so
//! bit-identity follows from Rust's integer rules rather than from a
//! run.
//!
//! This is where those three meet. A frame is the generator's `f64`
//! terrain, read through the art's integer splat, lit by this crate's
//! integer shading, on four different compiler backends. That the parts
//! agree separately does not say the composition does, and the digest
//! below is the claim that it does.
//!
//! [`tairix_wintersun_art::digest::REFERENCE_DIGEST`] is folded in, so
//! this vertical is also the one that fails when the art moves — which
//! is the coverage the art crate deliberately does not carry itself.
//!
//! The frames are drawn with figures standing in them, because the figure
//! crate's own vertical proves a pose and its placement but not the client
//! drawing it: the band-by-band fill, the depth order, the veil and the
//! waterline are this crate's arithmetic.

use core::hash::Hasher;

use tairix_hash::FastHash;
use tairix_log::{Event, Sink};
use tairix_raster::color::Pixel;
use tairix_raster::surface::Surface;
use tairix_reclaim::{PressureBand, ReportedPressure};
use tairix_wintersun_art::cache::MaterialCache;
use tairix_wintersun_art::decal::Fray;
use tairix_wintersun_art::digest as art;
use tairix_wintersun_art::splat::Warp;
use tairix_wintersun_figure::actor::Actor;
use tairix_wintersun_figure::motion::{Clips, Kind, Set};
use tairix_wintersun_figure::reference as figures;
use tairix_wintersun_figure::species::Species;
use tairix_wintersun_net::value::{EntityId, Facing, WorldPoint};
use tairix_wintersun_world::chunk::{Chunk, ChunkBuild, ChunkWindow};
use tairix_wintersun_world::params::{RealmParams, RealmSpec};
use tairix_wintersun_world::realm::RealmField;

use crate::budget::FRAME_NS;
use crate::camera::{realm_bounds, Camera, Zoom};
use crate::error::ClientError;
use crate::figures::Cast;
use crate::frame::{Renderer, Scene, Stopped};
use crate::light::{Sky, Sun};
use crate::quality::Ladder;
use crate::terrain::{self, RoadDecals};
use crate::view::Viewport;

/// The digest of the reference frames, on every target.
///
/// Changing the projection, the lattice sampling, the light model, the
/// ladder's knobs, the figure pass, or anything in the art, the figure
/// engine or the world generator beneath them changes this. It is the
/// record of what the game looks like, not a number to be re-derived when a
/// test fails.
pub const REFERENCE_DIGEST: u64 = 0xD17F_8CA7_595A_FD82;

/// The realm the reference frames are drawn in.
pub const REFERENCE_SEED: u64 = 0x5749_4E54_4552_4652;

/// The window the reference frames are drawn at.
///
/// Small deliberately: the claim is about arithmetic, and a guest that
/// spends a minute generating a realm to prove it is a slower answer to
/// the same question. Wide enough, at the furthest zoom, to cover three
/// hundred cells and the several materials that come with them — a
/// frame of half a dozen cells would be a flat fill whatever the
/// renderer did.
pub const FRAME_WIDTH: u32 = 160;

/// The height of the reference frames.
pub const FRAME_HEIGHT: u32 = 120;

/// The frames the digest is folded over: a ladder step and the zoom to
/// draw it at.
///
/// Two, and deliberately at opposite corners of both knobs. The wide one
/// at full quality covers many cells through the coarse mips and every soft
/// shadow; the close one with every rung shed covers the fine mips and the
/// flat, hard-shadowed, half-scale end of the ladder. Between them every
/// knob moves the digest.
const FRAMES: [(u8, Zoom); 2] = [(0, Zoom::FURTHEST), (Ladder::MAX_STEP, Zoom::DEFAULT)];

/// One figure of the reference frames: who it is, where it starts from the
/// view's centre and how far it moves each frame, in world sub-units, which
/// way it faces, what it performs, and how deep the water it stands in is.
struct Extra {
    species: Species,
    from: (i32, i32),
    per_frame: (i32, i32),
    toward: Facing,
    performs: Option<Kind>,
    submerged: i32,
}

/// Every species, and between them every part of the figure pass: a walk
/// and a run, an upper-body action over a stride, a whole-body action in the
/// air, and a figure wading.
const EXTRAS: [Extra; 5] = [
    Extra {
        species: Species::Human,
        from: (-1400, 200),
        per_frame: (20, 0),
        toward: Facing(0),
        performs: None,
        submerged: 0,
    },
    Extra {
        species: Species::Elf,
        from: (-500, -700),
        per_frame: (0, 20),
        toward: Facing(0x4000),
        performs: Some(Kind::Cast),
        submerged: 0,
    },
    Extra {
        species: Species::Dwarf,
        from: (500, 400),
        per_frame: (0, 0),
        toward: Facing(0xC000),
        performs: None,
        submerged: 48,
    },
    Extra {
        species: Species::Beastkin,
        from: (1300, -400),
        per_frame: (-61, 0),
        toward: Facing(0x8000),
        performs: None,
        submerged: 0,
    },
    Extra {
        species: Species::Dragonkin,
        from: (100, 1000),
        per_frame: (0, 0),
        toward: Facing(0x2000),
        performs: Some(Kind::Dodge),
        submerged: 0,
    },
];

/// How many frames the extras play before a reference frame is drawn: into
/// the middle of the dodge's flight and the cast's release.
const FRAMES_PLAYED: u32 = 12;

/// Memory the reference frames' material cache is sized from.
///
/// Stated rather than discovered, because the digest must not depend on
/// the machine that produced it: a cache that refused a tile would draw
/// that material flat and move the number. Sixteen mebibytes admits
/// every tile these two frames need several times over and still sits
/// well inside the boot heap of the guests that run this.
const CACHE_BACKING_BYTES: usize = 16 * 1024 * 1024;

/// A sink that drops what it is given.
///
/// The cache charges a refusal through an audit sink; a digest run has
/// no journal and wants none, and a refusal is already visible in the
/// picture as a flat material.
struct Discard;

impl Sink for Discard {
    fn write_event(&self, _: &Event<'_>) {}
}

static SINK: Discard = Discard;
static PRESSURE: ReportedPressure = ReportedPressure::unknown();

/// Draw the reference frames and return their digest.
///
/// # Errors
///
/// [`ClientError::World`] if the realm or a chunk could not be
/// generated, [`ClientError::Figure`] if a figure could not be, and
/// [`ClientError::OutOfMemory`] if a frame buffer does not fit.
pub fn reference() -> Result<u64, ClientError> {
    PRESSURE.report(PressureBand::Normal);
    let params = reference_params()?;
    let field = RealmField::generate(params).map_err(|_| ClientError::World)?;
    let roads = RoadDecals::from_realm(&field)?;
    let decals = roads.decals()?;
    let warp = Warp::new(params.seed());
    let fray = Fray::new(params.seed());

    let mut cache = MaterialCache::new(
        "wintersun-client-digest",
        CACHE_BACKING_BYTES,
        &PRESSURE,
        &SINK,
    );
    let set = Set::new().map_err(|_| ClientError::Figure)?;
    let clips = set.clips().map_err(|_| ClientError::Figure)?;
    let mut renderer = Renderer::new();
    let mut hasher = FastHash::with_seed(REFERENCE_SEED);

    for (step, zoom) in FRAMES {
        let ladder = Ladder::new(step);
        let camera = Camera::new(WorldPoint { x: 0, y: 0 }, zoom, realm_bounds(params));
        let view = Viewport::new(FRAME_WIDTH, FRAME_HEIGHT, ladder.render_scale())?;
        let held = generate(&field, camera.visible(&view))?;
        let borrowed = borrow(&held)?;
        let chunks = ChunkWindow::new(&borrowed).map_err(|_| ClientError::World)?;
        let cast = extras(&clips, camera.centre(&view))?;

        let (width, height) = view.render();
        let mut target = Surface::new(width, height).ok_or(ClientError::OutOfMemory)?;
        renderer.render(
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
                ladder,
                cast: &cast,
            },
            &mut cache,
            &tairix_parallel::SERIAL,
            &Stopped,
        )?;
        fold_frame(&mut hasher, target.pixels(), renderer.grid().unmapped());
        hasher.write_u64(u64::try_from(renderer.figures()).unwrap_or(u64::MAX));
    }
    // The ground the frames are drawn from, so a change to the art moves
    // this number too — the coverage the art crate does not carry.
    hasher.write_u64(art::REFERENCE_DIGEST);
    Ok(hasher.finish())
}

/// The realm the reference frames are drawn in.
///
/// Small and coarse: the claim is the rendering, and a realm large
/// enough to be played in would spend the guest's whole budget being
/// generated.
fn reference_params() -> Result<RealmParams, ClientError> {
    RealmParams::new(RealmSpec {
        seed: REFERENCE_SEED,
        // Thirty-two chunks rather than the realm's own two hundred and
        // fifty-six: large enough that the ground around the origin has
        // the slopes and the half-dozen materials a real one does —
        // measured, not assumed — and small enough that a guest solves
        // it in a moment.
        extent_chunks: 32,
        coarse_samples: 32,
        plates: 8,
        ocean_permille: 380,
        relief_units: 1800,
        north_celsius: -22,
        south_celsius: 14,
        wind: Facing(0x0800),
    })
    .map_err(|_| ClientError::World)
}

/// The extras, standing around `centre` and played through
/// [`FRAMES_PLAYED`] frames.
fn extras<'a>(clips: &'a Clips<'a>, centre: WorldPoint) -> Result<Cast<'a>, ClientError> {
    let mut cast = Cast::new();
    for (index, extra) in (0u64..).zip(&EXTRAS) {
        let identity = figures::identity(extra.species).map_err(|_| ClientError::Figure)?;
        let mut actor =
            Actor::new(&identity, clips, extra.toward).map_err(|_| ClientError::Figure)?;
        if let Some(kind) = extra.performs {
            actor.perform(kind).map_err(|_| ClientError::Figure)?;
        }
        let id = EntityId(index);
        let mut at = WorldPoint {
            x: centre.x + extra.from.0,
            y: centre.y + extra.from.1,
        };
        cast.join(id, actor, at)?;
        let figure = cast.get_mut(id).ok_or(ClientError::Figure)?;
        for _ in 0..FRAMES_PLAYED {
            at.x += extra.per_frame.0;
            at.y += extra.per_frame.1;
            figure.step(FRAME_NS, at, extra.toward, extra.submerged)?;
        }
    }
    Ok(cast)
}

/// Generate every chunk the view needs, in coordinate order.
fn generate(
    field: &RealmField,
    visible: tairix_wintersun_art::decal::Bounds,
) -> Result<alloc::vec::Vec<Chunk>, ClientError> {
    let mut held = alloc::vec::Vec::new();
    for coord in terrain::visible_chunks(visible) {
        if !field.params().holds_chunk(coord.x, coord.y) {
            continue;
        }
        let build = ChunkBuild::new(coord).map_err(|_| ClientError::World)?;
        let chunk = build.finish(field).map_err(|_| ClientError::World)?;
        held.try_reserve(1).map_err(|_| ClientError::OutOfMemory)?;
        held.push(chunk);
    }
    Ok(held)
}

/// Borrow the generated chunks as the window wants them.
fn borrow(held: &[Chunk]) -> Result<alloc::vec::Vec<&Chunk>, ClientError> {
    let mut borrowed = alloc::vec::Vec::new();
    borrowed
        .try_reserve(held.len())
        .map_err(|_| ClientError::OutOfMemory)?;
    borrowed.extend(held.iter());
    Ok(borrowed)
}

/// Fold a whole frame's pixels, and how much of it was ground the client
/// did not hold.
fn fold_frame(hasher: &mut FastHash, pixels: &[Pixel], unmapped: usize) {
    // A fixed width, because `usize` is four bytes on wasm32 and eight
    // everywhere else and a length folded raw would differ by target for
    // that reason alone.
    hasher.write_u64(u64::try_from(pixels.len()).unwrap_or(u64::MAX));
    hasher.write_u64(u64::try_from(unmapped).unwrap_or(u64::MAX));
    for pixel in pixels {
        hasher.write(&[pixel.r, pixel.g, pixel.b, pixel.a]);
    }
}

#[cfg(test)]
#[path = "digest_tests.rs"]
mod tests;
