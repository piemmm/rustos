//! The figure digest: the cross-target claim, made over everything this
//! crate computes before a pixel is touched.
//!
//! The world generator and the simulation each carry a four-target vertical
//! because each does arithmetic whose identity across targets is a property
//! of the code rather than of the language. This crate is the same case and
//! more so: it is `f64` throughout — clip sampling and easing, overlay
//! summing, the orthonormal resolve, the projection and its foreshortening,
//! the two-bone planting solve, the root transform, and the shadow's
//! singular-value decomposition.
//!
//! All of it is IEEE-754 basic operations over `lib/util::mathf`, whose
//! transcendentals are first-party polynomial kernels rather than a
//! platform libm, so bit-identity *follows* from the language — which is
//! exactly why the fold below is over raw [`f64::to_bits`] with no
//! quantisation and no tolerance. A tolerance would hide the one hazard
//! that is real: a backend fusing a multiply and an add into a single
//! rounded operation. Rust does not enable that, and these verticals are
//! what says so rather than assuming it.
//!
//! # What is folded, and what is not
//!
//! Every figure of the grid, the generated ones included: its record's
//! bytes, the complete placed-strip stream in the depth order the painter
//! walks, the carried rings at full precision, the planting root and miss,
//! the height each clip holds the body at across its cycle, and the quality
//! numbers the art is gated on. Then a designer's preview played through
//! every change of clip, which is where the transition machine's cross-fade
//! of pose and root height enters the claim: the grid samples one clip at a
//! time.
//!
//! Not the pixels. Those are `lib/raster`'s shared scan converter, which is
//! separately tested and is the client frame digest's subject
//! (`tairix_wintersun_app::digest`). This is the fourth digest rather than
//! an extension of that one because a figure has to agree across targets
//! whether or not anything draws it.

use core::hash::Hasher;

use tairix_hash::FastHash;
use tairix_raster::shape::{Placed, Shape};
use tairix_wintersun_net::value::Facing;

use crate::clip::Clip;
use crate::error::FigureError;
use crate::frame::project;
use crate::gait::Gait;
use crate::humanoid;
use crate::mesh::Hoop;
use crate::motion::{self, Kind};
use crate::plant::{Legs, Planted};
use crate::pose::Param;
use crate::preview::{Frame, Preview, FADE};
use crate::quality::Measured;
use crate::reference::{self, Reference, SIDES};
use crate::rig::{Frames, Placement, Resolved, Strip};
use crate::rigging::Rigging;
use crate::shadow::Contact;
use crate::socket::Side;
use crate::species::Species;

/// The digest of the reference grid, on every Tier-1 target.
///
/// Changing the rig, a species' ranges, a feature's template, the record
/// format, a clip, a clip's root height, a joint limit, the projection, the
/// planting solve, the shadow, the transition machine or the plausible
/// generator changes this. That is the point: it is not a number to be
/// re-derived when a test fails, it is the record of what a figure does. A
/// change that moves it changes every figure anybody will ever see, and the
/// new value is written down deliberately rather than pasted out of a
/// failure.
pub const REFERENCE_DIGEST: u64 = 0x9A61_8EF9_E425_DD95;

/// The stream the reference grid is folded into.
pub const REFERENCE_SEED: u64 = 0x5749_4E54_4552_4647;

/// Figure-local units to surface pixels for the reference placements.
///
/// Not one: a scale of one would multiply the projection away rather than
/// fold it, and a sign or an axis lost in the conversion would not show.
const SCALE: f64 = 1.25;

/// Where the reference figures' ground contact sits on the surface.
///
/// Off both axes and off any round number, so a column and a row swapped for
/// one another show up.
const AT: (f64, f64) = (37.5, 92.25);

/// The slopes the planting solve is probed at, beyond the level ground the
/// grid stands on.
///
/// One inside the legs' own reach, one past it so the figure leans, and one
/// no leg could ever meet so the miss is reported rather than fudged.
const SLOPES: [[f64; 2]; 3] = [[2.5, -2.5], [18.0, -18.0], [400.0, 400.0]];

/// How high above its ground point the shadow is cast from, for the probe
/// that covers the fade and the slide a rising figure's shadow shows.
const LIFTS: [f64; 3] = [0.0, 6.0, 41.0];

/// How many phases the root-height curve is folded at.
///
/// Sixteenths, so both of the run's flight windows are crossed at their
/// middle rather than only at the boundaries the reference grid's eighths
/// land on.
const ROOT_PROBES: u32 = 16;

/// The distances the gait probe advances by.
///
/// Not a whole number of strides between them, and one longer than a cycle,
/// so the wrap and the lap count are both folded rather than only the easy
/// case.
const GAIT_STEPS: [f64; 4] = [7.5, 23.25, 140.0, -11.75];

/// The clips the preview probe plays in turn, and how many frames it holds
/// each for.
///
/// Every clip gives way to another, and each is held past the fade into it,
/// so frames inside a fade and after one are both folded.
const PREVIEW_SCRIPT: [(Kind, u32); 6] = [
    (Kind::Walk, 5),
    (Kind::Run, 5),
    (Kind::Dodge, 9),
    (Kind::Idle, 4),
    (Kind::Cast, 8),
    (Kind::Run, 4),
];

/// The preview probe's frame length, in seconds: no fraction of a fade or of
/// any clip's cycle, so no frame lands on a boundary by luck.
const PREVIEW_FRAME: f64 = 0.071;

const _: () = {
    let mut index = 0;
    while index < PREVIEW_SCRIPT.len() {
        let held = PREVIEW_SCRIPT[index].1 as f64 * PREVIEW_FRAME;
        assert!(held > FADE, "a preview probe clip ends inside its fade");
        index += 1;
    }
};

/// Draw the reference grid and return its digest.
///
/// # Errors
///
/// Whatever the rig, the motions, the planting solve or the placement
/// refuse — none of which is reachable for the shipped set, which is what
/// the crate's own tests say.
pub fn reference() -> Result<u64, FigureError> {
    let mut placement = Placement::new();
    let mut frames = Frames::new();
    let mut hasher = FastHash::with_seed(REFERENCE_SEED);

    // One figure at a time, so only one rig is ever held: the grid's whole
    // buffer set has to fit the boot stack the verticals run on.
    for entry in reference::grid() {
        let entry = entry?;
        let identity = entry.identity()?;
        hasher.write(&identity.encode());
        let figure = Reference::new(&identity)?;
        let rigging = humanoid::rigging(figure.rig())?;
        let legs = figure.legs();

        for kind in entry.kinds() {
            let clip = figure.clip(*kind)?;
            fold_quality(&mut hasher, &rigging, clip, *kind, &legs)?;
            fold_gait(&mut hasher, &rigging, clip, *kind, &legs)?;
        }

        for cell in entry.cells() {
            let planted = figure.place(cell, SCALE, AT, &mut placement)?;
            fold_planted(&mut hasher, &planted);
            // The surfaces themselves, before they are rounded onto the
            // converter's grid: a strip is stored at the resolution it is
            // drawn, and the cross-target claim is about the arithmetic
            // behind it rather than about the pixel it lands on.
            rigging
                .posture(&planted.pose())?
                .resolve(planted.root(), &mut frames);
            for part in figure.rig().parts() {
                for hoop in figure.surfaces(part, &frames)? {
                    fold_hoop(&mut hasher, cell.facing, hoop);
                }
            }
            fold_usize(&mut hasher, placement.len());
            for strip in placement.strips() {
                fold_strip(&mut hasher, &strip);
            }
        }

        for kind in entry.kinds() {
            fold_slopes(&mut hasher, &rigging, legs, figure.clip(*kind)?)?;
        }
    }
    fold_shadow(&mut hasher)?;
    fold_preview(&mut hasher, &mut placement)?;
    Ok(hasher.finish())
}

/// A designer's preview of the richest reference figure, played through
/// [`PREVIEW_SCRIPT`] and turned a little every frame: the blended pose and
/// root at full precision, and every view the preview draws, in both of its
/// framings.
fn fold_preview(hasher: &mut FastHash, placement: &mut Placement) -> Result<(), FigureError> {
    let motions = motion::Set::new()?;
    let clips = motions.clips()?;
    let mut preview = Preview::new(&reference::identity(Species::Dragonkin)?, &clips)?;
    let mut facing = Facing(0x0C00);
    for (kind, frames) in PREVIEW_SCRIPT {
        preview.select(kind)?;
        for _ in 0..frames {
            preview.advance(PREVIEW_FRAME)?;
            facing = Facing(facing.0.wrapping_add(0x0B00));
            preview.face(facing);
            let planted = preview.planted()?;
            fold_planted(hasher, &planted);
            for param in Param::ALL {
                fold_real(hasher, planted.pose().get(param));
            }
            for frame in [Frame::Shared, Frame::Measured] {
                for side in SIDES {
                    let shadow = preview.view(frame, side, placement)?;
                    fold_usize(hasher, placement.len());
                    for strip in placement.strips() {
                        fold_strip(hasher, &strip);
                    }
                    fold_placed(hasher, shadow);
                }
            }
        }
    }
    Ok(())
}

/// A gait driven by the ground it covers, which is how a figure is animated
/// rather than by a clock.
///
/// Folded as its own probe rather than through the grid, because the grid's
/// pose must depend on the phase alone — a sheet's four headings are one
/// figure seen four ways.
fn fold_gait(
    hasher: &mut FastHash,
    rigging: &Rigging<'_>,
    clip: Clip<'_>,
    kind: Kind,
    legs: &Legs,
) -> Result<(), FigureError> {
    if kind.stride().is_none() {
        return Ok(());
    }
    let mut gait = Gait::fitted(rigging, clip, legs, Side::Left)?;
    fold_real(hasher, gait.stride());
    for onward in GAIT_STEPS {
        let stepped = gait.travel(onward)?;
        fold_real(hasher, stepped.from);
        fold_real(hasher, stepped.to);
        hasher.write(&stepped.cycles.to_le_bytes());
    }
    Ok(())
}

/// The numbers the art is gated on, so a clip re-authored to a different
/// quality moves the digest as well as the ledger.
fn fold_quality(
    hasher: &mut FastHash,
    rigging: &Rigging<'_>,
    clip: Clip<'_>,
    kind: Kind,
    legs: &Legs,
) -> Result<(), FigureError> {
    hasher.write(kind.name().as_bytes());
    fold_real(hasher, clip.seconds());
    for (what, value, _) in Measured::of(kind, rigging, clip, legs)?.each() {
        hasher.write(what.as_bytes());
        fold_real(hasher, value);
    }
    fold_root(hasher, clip);
    if let Some(authored) = kind.stride() {
        fold_real(hasher, authored);
    }
    Ok(())
}

/// The height a clip holds the body at, across the cycle.
///
/// Folded on its own grid rather than through the reference cells, because
/// the grid's eight phases land on the run's flight *boundaries* — where the
/// arc meets the stance height and contributes nothing — so a cell-only fold
/// would leave the whole curve outside the cross-target claim. Sixteenths
/// cross the middle of both flight windows.
fn fold_root(hasher: &mut FastHash, clip: Clip<'_>) {
    for step in 0..ROOT_PROBES {
        let phase = f64::from(step) / f64::from(ROOT_PROBES);
        fold_real(hasher, clip.root_at(phase));
    }
}

/// The planting solve at each probe slope: where the root ended up, and what
/// each foot missed its ground by.
fn fold_slopes(
    hasher: &mut FastHash,
    rigging: &Rigging<'_>,
    legs: Legs,
    clip: Clip<'_>,
) -> Result<(), FigureError> {
    // Mid-cycle rather than at the head of it, so the probe runs against a
    // figure with one leg swinging rather than one standing to attention.
    const PROBE: f64 = 0.5;
    let posed = clip.sample(PROBE)?;
    let mut frames = Frames::new();
    rigging
        .posture(&posed)?
        .resolve(Resolved::REST, &mut frames);
    let root = clip.root_at(PROBE);
    for ground in SLOPES {
        fold_planted(hasher, &legs.plant(rigging, &posed, &frames, ground, root)?);
    }
    Ok(())
}

/// The shadow solve: the ellipse a raking light throws under a figure at
/// each probe height.
fn fold_shadow(hasher: &mut FastHash) -> Result<(), FigureError> {
    let (radius, tone) = reference::SHADOW;
    let contact = Contact::new(radius, tone)?;
    for lift in LIFTS {
        fold_placed(hasher, Reference::shadow(lift, SCALE, AT)?);
        for ring in contact.penumbra(Reference::light()?, lift, SCALE, AT)? {
            fold_placed(hasher, ring);
        }
    }
    Ok(())
}

fn fold_planted(hasher: &mut FastHash, planted: &Planted) {
    let root = planted.root();
    fold_real(hasher, root.at.forward);
    fold_real(hasher, root.at.side);
    fold_real(hasher, root.at.up);
    for axis in [root.basis.forward, root.basis.side, root.basis.up] {
        fold_real(hasher, axis.forward);
        fold_real(hasher, axis.side);
        fold_real(hasher, axis.up);
    }
    for side in Side::BOTH {
        fold_real(hasher, planted.miss(side));
    }
}

/// One carried ring, and where it projects to, at full precision.
fn fold_hoop(hasher: &mut FastHash, facing: Facing, hoop: Hoop) {
    for axis in [hoop.at, hoop.wide, hoop.deep] {
        fold_real(hasher, axis.forward);
        fold_real(hasher, axis.side);
        fold_real(hasher, axis.up);
    }
    let placed = project(facing, hoop.at);
    fold_real(hasher, placed.dx);
    fold_real(hasher, placed.dy);
    fold_real(hasher, placed.depth);
}

/// One shaded strip: every point of both its boundaries, and its tone.
fn fold_strip(hasher: &mut FastHash, strip: &Strip<'_>) {
    hasher.write(&strip.surface.to_le_bytes());
    fold_usize(hasher, strip.near.len());
    for side in [strip.near, strip.far] {
        for (x, y) in side {
            hasher.write(&x.to_le_bytes());
            hasher.write(&y.to_le_bytes());
        }
    }
    hasher.write(&[strip.color.r, strip.color.g, strip.color.b, strip.color.a]);
}

fn fold_placed(hasher: &mut FastHash, placed: Placed) {
    fold_real(hasher, placed.x);
    fold_real(hasher, placed.y);
    fold_real(hasher, placed.turn);
    fold_shape(hasher, placed.shape);
    hasher.write(&[
        placed.color.r,
        placed.color.g,
        placed.color.b,
        placed.color.a,
    ]);
    hasher.write(&placed.seed.to_le_bytes());
}

fn fold_shape(hasher: &mut FastHash, shape: Shape) {
    let (tag, a, b, c) = match shape {
        Shape::Splat { radius } => (0u8, radius, 0.0, 0.0),
        Shape::Superellipse { rx, ry, square } => (1, rx, ry, square),
        Shape::Taper { length, top, foot } => (2, length, top, foot),
        Shape::Wedge {
            half_width,
            height,
            lean,
        } => (3, half_width, height, lean),
        Shape::ScallopedPanel { rx, ry, folds } => (4, rx, ry, f64::from(folds)),
        Shape::BevelledPanel { rx, ry, bevel } => (5, rx, ry, bevel),
    };
    hasher.write(&[tag]);
    fold_real(hasher, a);
    fold_real(hasher, b);
    fold_real(hasher, c);
}

/// Fold a real by its bits, so a value that differs anywhere it can differ
/// moves the digest — no quantisation, and nothing rounded away.
fn fold_real(hasher: &mut FastHash, value: f64) {
    hasher.write_u64(value.to_bits());
}

/// Fold a count, widened first.
///
/// `usize` is four bytes on `wasm32` and eight everywhere else, so folding
/// one raw would make the digest depend on the target it is meant to prove
/// nothing depends on.
fn fold_usize(hasher: &mut FastHash, count: usize) {
    hasher.write_u64(u64::try_from(count).unwrap_or(u64::MAX));
}

#[cfg(test)]
#[path = "digest/tests.rs"]
mod tests;
