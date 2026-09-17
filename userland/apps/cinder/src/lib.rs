//! Cinder: the desktop companion, as the host-tested model its `Run` binary
//! composes.
//!
//! Cinder lives in a playpen window and can be let out onto the desktop, where
//! he wanders, chases the pointer, climbs onto windows, burrows under them, or
//! walks around them. Everything with behaviour lives here so it can be tested
//! without a screen: the elevated camera ([`project`]), the shapes a body part
//! is drawn as ([`shape`]), the soft fur splat ([`fur`]), the body
//! ([`cinder`]), locomotion ([`gait`]), what he decides to do ([`mind`]), the
//! desktop he walks ([`world`]), how he gets about it ([`roam`]), the pen
//! ([`pen`]), its geometry ([`layout`]), the two painters ([`paint`]), and
//! what survives a restart ([`state`]).
#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

extern crate alloc;

pub mod cinder;
pub mod fur;
pub mod gait;
pub mod layout;
pub mod mind;
pub mod paint;
pub mod pen;
pub mod project;
pub mod roam;
pub mod shape;
pub mod state;
pub mod world;
