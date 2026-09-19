//! What the client shell refuses, and why.

use core::fmt;

/// A refusal from the client shell.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ClientError {
    /// A buffer the frame needs did not fit.
    OutOfMemory,
    /// A viewport of zero pixels, or one larger than a surface may be.
    Viewport,
    /// The world generator refused, or could not fit what it was asked
    /// for.
    World,
}

impl fmt::Display for ClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::OutOfMemory => "out of memory",
            Self::Viewport => "viewport out of range",
            Self::World => "the world could not be generated",
        })
    }
}
