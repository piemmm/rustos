//! `WinterSun`'s figures: how a character exists before anything animates it.
//!
//! A figure is parametric parts on a skeleton, not a sprite sheet. Eight
//! headings times every clip times every equipment combination is artwork
//! nobody can author or keep consistent, and a sheet is unreviewable by a
//! test. This crate is the structure that replaces it: joints, the parts
//! bound to them, the sockets equipment hangs on, and the projection that
//! turns all of it toward the camera.
//!
//! # Three ideas, and the rest follows
//!
//! **One body frame serves every heading.** A part carries a position in the
//! figure's own frame — [`Body`], along its heading, across it, and away from
//! the ground — and [`frame::project`] turns that frame by the heading and
//! foreshortens its depth axis. Parts then paint far-first by projected
//! depth, so facing away a face falls behind the skull and is covered, and
//! facing the camera it comes forward. There is no front/back/side artwork,
//! no per-direction branch, and no second path to keep in step.
//!
//! **A joint that bears a limb also carries its mass.** A part names a
//! [`JointId`], so the shoulder cap and the arm that swings from it are the
//! same joint's — they cannot drift apart and leave a limb growing out of
//! thin air. [`Rig::new`] refuses a rig where a joint bearing a child carries
//! no part of its own, or where a child's origin lies beyond everything its
//! parent draws.
//!
//! **A posture cannot be illegal.** Every joint documents a rotation limit
//! per axis, and [`Posture::set`] refuses a rotation outside it, so
//! [`Posture::place`] has no out-of-limit case to handle and an animation
//! that would bend an elbow backwards fails where it is authored.
//!
//! **An animation is authored in parameters, not in rotations.** A [`Pose`]
//! is named scalars — how far an elbow is folded, how far a hip has swung —
//! each a fraction of that joint's *own* documented travel. A clip keying
//! them therefore plays on any rig that declares the same parameters, and no
//! value of any parameter can leave a limit: the rotation it becomes is
//! scaled into the interval, so a bent-backwards elbow is not a pose that is
//! rejected, it is one that cannot be spelled.
//!
//! **The layers above a clip are deltas, and planting has the last word.**
//! Breathing, look-at and recoil each state their effect as a signed
//! [`Overlay`] rather than as a value, so they sum instead of overwriting
//! one another and the sum lands back inside each parameter's range — the
//! in-limit guarantee survives any number of them. [`Legs::plant`] then runs
//! *last*, because it must re-aim legs the layers above have finished with:
//! it consumes their pose and answers the final one together with the
//! figure's root transform. On flat ground that solve is an identity, so a
//! figure on the level is drawn exactly as its clip authored it.
//!
//! **The root is the placement's, not a joint's.** A jump's lift, the pelvis
//! drop of a crouch and a slope's lean are a rigid transform of the whole
//! body, carried on the [`Stance`] and seeded into the resolve as the frame
//! the parentless joints hang in. It pivots about the ground contact, which
//! is where a figure leaning into a hill must turn and is no joint's origin;
//! and a pelvis limit tight enough to keep a spine sane is nowhere near wide
//! enough for terrain.
//!
//! # The order it runs in
//!
//! 1. [`Animator`] picks the clips, [`Blend`] resolves a [`Pose`].
//! 2. [`Breath`], [`Look`] and [`Recoil`] add their overlays; the sum is
//!    applied.
//! 3. [`Legs::plant`] solves the feet onto the ground and answers the root.
//! 4. [`Rigging::posture`] and [`Posture::place`] draw it; [`Sway`] turns the
//!    gear, and [`Contact`] lays the shadow under it all.
//!
//! # What is not here
//!
//! The contact-sheet harness that makes art quality a measured property, the
//! species and build parameter space, and the designer are later items.
//!
//! [`Pose`]: pose::Pose
//! [`Overlay`]: pose::Overlay
//! [`Body`]: frame::Body
//! [`JointId`]: joint::JointId
//! [`Posture::set`]: rig::Posture::set
//! [`Rig::new`]: rig::Rig::new
//! [`Posture::place`]: rig::Posture::place
//! [`Stance`]: rig::Stance
//! [`Animator`]: transition::Animator
//! [`Blend`]: blend::Blend
//! [`Breath`]: breath::Breath
//! [`Look`]: look::Look
//! [`Recoil`]: recoil::Recoil
//! [`Legs::plant`]: plant::Legs::plant
//! [`Rigging::posture`]: rigging::Rigging::posture
//! [`Sway`]: sway::Sway
//! [`Contact`]: shadow::Contact

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

#[cfg(test)]
extern crate alloc;

pub mod blend;
pub mod breath;
pub mod clip;
pub mod digest;
pub mod error;
pub mod frame;
pub mod gait;
pub mod humanoid;
pub mod joint;
pub mod look;
pub mod mesh;
pub mod motion;
pub mod paint;
pub mod plant;
pub mod pose;
pub mod quality;
pub mod recoil;
pub mod reference;
pub mod rig;
pub mod rigging;
pub mod shadow;
pub mod socket;
pub mod spring;
pub mod sway;
pub mod transition;

pub use error::FigureError;
