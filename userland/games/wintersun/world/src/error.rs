//! What generation can refuse, and why.

/// A refusal from the generator.
///
/// Generation is pure arithmetic over an **already validated** parameter
/// document — [`RealmParams`](crate::params::RealmParams) has one
/// constructor and it checks every field — so a bad document is refused
/// before anything here runs. That leaves exactly one way to fail, and it
/// is a value rather than an abort: an exhausted machine gets a typed
/// refusal, never a panic.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum WorldError {
    /// The realm field or a chunk did not fit in memory.
    OutOfMemory,
    /// A chunk window is not strictly sorted by coordinate, so it could
    /// not be searched.
    UnsortedWindow,
}
