//! What the rules can refuse, and why.
//!
//! Three kinds of refusal, deliberately distinct types, because a caller
//! does different things with each: an authored value that is out of range
//! is a content defect, an intent the realm will not honour is an answer to
//! a player, and an exhausted machine is neither.

use tairix_wintersun_net::value::EntityId;

/// The simulation could not grow.
///
/// The only way the step itself fails. Every other refusal is a value about
/// the input rather than a failure of the run, and allocation failure is a
/// typed result rather than an abort.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum RulesError {
    /// The entity table, the event list or the broad phase could not be
    /// grown to hold what this step produced.
    OutOfMemory,
}

/// An authored value outside the range its rule admits.
///
/// Every validating constructor in this crate answers with one of these, so
/// a refused document names the field rather than the type. The bounds are
/// fixed bounds on untrusted input — a realm hands a client its rules, and a
/// realm is no more trusted by a client than the reverse — so none of them
/// scales with the machine.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum RuleError {
    /// A stat exceeds the ceiling every curve is defined over.
    Stat,
    /// The tick rate is zero or above the protocol's ceiling.
    TickRate,
    /// A status magnitude is outside the range its kind admits — including
    /// a non-zero magnitude on a kind whose effect is binary.
    StatusMagnitude,
    /// A status duration is zero or longer than any effect may last.
    StatusDuration,
    /// A body radius is zero or wider than the broad phase is built for.
    BodyRadius,
    /// An armour value exceeds the ceiling the damage pipeline is defined
    /// over.
    Armour,
    /// A blow's authored base damage exceeds the pipeline's ceiling.
    BlowBase,
    /// A power scale is outside the range a blow may ask for.
    PowerScale,
    /// A terrain window is not sorted by chunk coordinate, so it could not
    /// be searched.
    TerrainWindow,
}

/// Why an intent was not honoured.
///
/// A refusal is an answer, not an error: the realm states why and the
/// session continues. Nothing here is a reason to drop a connection — the
/// protocol's own decode owns that — and nothing here is silent, because a
/// refused action that vanishes reads to a player as a bug.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Refusal {
    /// No such entity in this zone. A client may name only entities it has
    /// been told about, and it is not told about this one.
    NoSuchEntity(EntityId),
    /// The sequence number is one already applied, or one behind it. The
    /// realm applies each of a client's intents once.
    Replayed,
    /// The intent claims to have been sampled in a tick that has not
    /// happened. A client may act in the present or the recent past, never
    /// ahead of the realm.
    SampledInTheFuture,
    /// This entity has already had as many intents admitted this tick as
    /// the rate bound allows.
    TooManyThisTick,
    /// A status prevents the entity acting at all.
    Stunned,
    /// A status prevents the entity casting.
    Silenced,
    /// The zone holds no action, spell, inventory or interaction table that
    /// resolves this intent. Movement is the whole of what this item
    /// simulates; the rest is refused by the same lookup that will admit it
    /// once a table exists.
    Unresolvable,
}

/// Why one of the zone's verbs did not do what was asked.
///
/// Two quite different things, kept apart: the realm declining a request is
/// an answer a client is told, where an exhausted machine is not the
/// client's business at all.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ZoneError {
    /// The realm will not honour the request, and why.
    Refused(Refusal),
    /// The zone could not be grown to hold the result.
    OutOfMemory,
}

impl From<Refusal> for ZoneError {
    fn from(refusal: Refusal) -> Self {
        Self::Refused(refusal)
    }
}

impl From<RulesError> for ZoneError {
    fn from(error: RulesError) -> Self {
        match error {
            RulesError::OutOfMemory => Self::OutOfMemory,
        }
    }
}
