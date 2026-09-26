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
use tairix_raster::color::Pixel;
use tairix_raster::surface::Surface;
use tairix_wintersun_art::digest as art;
use tairix_wintersun_figure::motion::Set;

use crate::camera::Zoom;
use crate::error::ClientError;
use crate::frame::Renderer;
use crate::quality::Ladder;
use crate::reference::{self, Shot};
use crate::view::Viewport;

/// The digest of the reference frames, on every target.
///
/// Changing the projection, the lattice sampling, the light model, the
/// ladder's knobs, the figure pass, or anything in the art, the figure
/// engine or the world generator beneath them changes this. It is the
/// record of what the game looks like, not a number to be re-derived when a
/// test fails.
pub const REFERENCE_DIGEST: u64 = 0x3506_8284_0DFA_3CEA;

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

/// Draw the reference frames and return their digest.
///
/// # Errors
///
/// [`ClientError::World`] if the realm or a chunk could not be
/// generated, [`ClientError::Figure`] if a figure could not be, and
/// [`ClientError::OutOfMemory`] if a frame buffer does not fit or the cache
/// refused a tile a frame needed.
pub fn reference() -> Result<u64, ClientError> {
    let mut hasher = FastHash::with_seed(reference::SEED);
    draw_frames(|target, renderer| {
        fold_frame(&mut hasher, target.pixels(), renderer.grid().unmapped());
        hasher.write_u64(u64::try_from(renderer.figures()).unwrap_or(u64::MAX));
    })?;
    // The ground the frames are drawn from, so a change to the art moves
    // this number too — the coverage the art crate does not carry.
    hasher.write_u64(art::REFERENCE_DIGEST);
    Ok(hasher.finish())
}

/// Draw each of [`FRAMES`] in order, handing `each` the frame and the
/// renderer that drew it.
fn draw_frames(mut each: impl FnMut(&Surface, &Renderer)) -> Result<(), ClientError> {
    let mut world = reference::World::generate()?;
    let set = Set::new().map_err(|_| ClientError::Figure)?;
    let clips = set.clips().map_err(|_| ClientError::Figure)?;
    let mut cache = reference::cache(&reference::Unpressured);
    let mut renderer = Renderer::new();
    for (step, zoom) in FRAMES {
        let ladder = Ladder::new(step);
        let view = Viewport::new(FRAME_WIDTH, FRAME_HEIGHT, ladder.render_scale())?;
        let (width, height) = view.render();
        let mut target = Surface::new(width, height).ok_or(ClientError::OutOfMemory)?;
        let shot = Shot {
            view: &view,
            zoom,
            ladder,
        };
        world.draw(
            &clips,
            shot,
            &mut target,
            &mut renderer,
            &mut cache,
            &tairix_parallel::SERIAL,
        )?;
        each(&target, &renderer);
    }
    Ok(())
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
