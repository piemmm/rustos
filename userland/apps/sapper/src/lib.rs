//! Sapper: the mine-clearing game, as the host-tested model its `Run` binary
//! composes.
#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

extern crate alloc;

pub mod anim;
pub mod board;
pub mod game;
pub mod layout;
pub mod paint;
pub mod scores;
