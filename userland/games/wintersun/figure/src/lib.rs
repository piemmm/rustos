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
//! # What is not here
//!
//! The procedural layers over a clip — gait phase from distance travelled,
//! look-at, recoil, cloth and hair sway, breathing, planting each foot on
//! its own ground height — are the next item, and so is everything that
//! moves the figure's *root*: a jump's lift, the pelvis drop of a crouch, a
//! dodge's displacement. Root motion cannot be decided without the ground
//! the feet are standing on, so it lives with the terrain solve rather than
//! split across both. A pose here is articulation only.
//!
//! [`Pose`]: pose::Pose
//! [`Body`]: frame::Body
//! [`JointId`]: joint::JointId
//! [`Posture::set`]: rig::Posture::set
//! [`Rig::new`]: rig::Rig::new
//! [`Posture::place`]: rig::Posture::place

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod blend;
pub mod clip;
pub mod error;
pub mod frame;
pub mod humanoid;
pub mod joint;
pub mod pose;
pub mod rig;
pub mod rigging;
pub mod socket;
pub mod transition;

pub use error::FigureError;
