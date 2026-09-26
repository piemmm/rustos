//! `WinterSun`'s client shell: the window a player looks through, and the
//! frame they see in it.
#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![deny(clippy::float_arithmetic)]

extern crate alloc;

pub mod budget;
pub mod camera;
pub mod cli;
pub mod digest;
pub mod error;
pub mod figures;
pub mod frame;
pub mod input;
pub mod light;
pub mod pacing;
pub mod presets;
pub mod quality;
pub mod reference;
pub mod shell;
pub mod terrain;
pub mod view;
