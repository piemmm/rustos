//! What this crate refuses, and why.

use core::fmt;

/// A refusal from the art pipeline.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ArtError {
    /// A tile, a cache or a particle store could not be allocated.
    OutOfMemory,
    /// A mip level beyond the chain the base tile side supports.
    NoSuchMip,
    /// A particle field with no room and nothing older to retire.
    ParticleBudgetFull,
}

impl fmt::Display for ArtError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            Self::OutOfMemory => "out of memory",
            Self::NoSuchMip => "mip level beyond the chain",
            Self::ParticleBudgetFull => "particle budget full",
        };
        f.write_str(text)
    }
}
