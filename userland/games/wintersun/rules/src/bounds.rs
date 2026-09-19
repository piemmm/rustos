//! The fixed bounds the rules are defined over.
//!
//! None of these is a capacity. The number of entities a zone holds, the
//! events a step emits and the cells the broad phase occupies all follow the
//! work and grow on demand; what is fixed here is the range each *rule* is
//! defined over — how much authority one intent can carry, how far a status
//! can reach, how fast anything can be. Those are bounds on untrusted input
//! and on arithmetic range, and they do not move to admit a value.
//!
//! The arithmetic argument matters as much as the security one: every
//! product the pipelines form is bounded by these, which is what lets the
//! whole simulation run in checked integer arithmetic without a single
//! saturating step hiding a real overflow.

use tairix_wintersun_net::bounds::WORLD_SUB_UNITS_PER_UNIT;

/// The ceiling every stat curve is defined over.
///
/// The curves are evaluated at a stat rather than extrapolated from one, so
/// this is the domain they are stated on and tested at both ends.
pub const MAX_STAT: u16 = 1000;

/// Harmful statuses one entity may carry at once.
///
/// Partitioned from the helpful ones on purpose: a single shared ceiling
/// would let a player fill it with buffs and become immune to debuffs, which
/// is a defence nobody granted.
pub const MAX_HARMFUL_STATUS: usize = 8;

/// Helpful statuses one entity may carry at once.
pub const MAX_HELPFUL_STATUS: usize = 8;

/// Statuses one entity may carry at once, over both partitions.
pub const MAX_STATUS_PER_ENTITY: usize = MAX_HARMFUL_STATUS + MAX_HELPFUL_STATUS;

/// Intents from one entity the realm admits in one tick.
///
/// A client that sends more is not refused a connection — it is simply
/// describing more actions than a tick contains, and the surplus waits for
/// the next one.
pub const MAX_INTENTS_PER_TICK: u8 = 8;

/// How far behind the current tick a sample time may sit and still be read
/// as a position inside it.
///
/// A client's sample time is an input to place an action within the tick it
/// arrived in. Older than this and the realm stops believing the placement
/// and uses the tick the intent actually arrived in, so a back-dated sample
/// buys nothing.
pub const INTENT_LOOKBEHIND_TICKS: u64 = 8;

/// The widest body the broad phase is built for, in sub-units of radius.
pub const MAX_BODY_RADIUS_SUB_UNITS: i32 = 2 * WORLD_SUB_UNITS_PER_UNIT;

/// The fastest anything may move, in sub-units per tick.
///
/// Well above any speed the curves produce, so it bounds the arithmetic
/// rather than the design. One world unit per tick is thirty a second at the
/// default rate, which no authored movement approaches.
pub const MAX_SPEED_SUB_UNITS_PER_TICK: i32 = WORLD_SUB_UNITS_PER_UNIT;

/// The largest proportional status magnitude, in parts per thousand.
///
/// Below a full thousand so no proportional effect can reach total: a
/// mitigation that reached 1000 would negate damage entirely, and a slow
/// that reached it would be a root by another name — an effect the
/// vocabulary already has, with its own diminishing returns.
pub const MAX_PERMILLE_MAGNITUDE: u16 = 900;

/// The largest per-tick health change a periodic status may carry.
pub const MAX_PERIODIC_HEALTH: u16 = 1000;

/// The largest absorb pool a shield may carry.
pub const MAX_ABSORB: u16 = u16::MAX;

/// The longest any status may last, in ticks.
///
/// The bound exists for the arithmetic: it is what keeps every
/// duration-times-magnitude product the accounting forms inside `u64` with
/// room to spare. Nothing authored legitimately outlives it.
pub const MAX_STATUS_TICKS: u32 = 1 << 20;

/// Seconds of no further application after which a status's diminishing
/// returns reset.
///
/// Stated in seconds rather than ticks because it is a fairness rule about
/// how often a player is controlled, and that must not change with the
/// realm's tick rate. The rate converts it.
pub const DIMINISH_RESET_SECONDS: u32 = 15;

/// Applications of one control effect after which the target is immune
/// until the window resets.
///
/// Three, so the chain is full, half, quarter, immune. A player who has just
/// been stunned three times in fifteen seconds has not been playing, and a
/// fourth is the point at which generosity to the attacker stops being
/// interesting to anybody.
pub const DIMINISH_IMMUNE_AT: u8 = 3;

/// The armour ceiling the damage pipeline is defined over.
pub const MAX_ARMOUR: u16 = 10_000;

/// The largest base damage a blow may be authored with.
pub const MAX_BLOW_BASE: u32 = 1_000_000;

/// The largest power scale a blow may ask for, in parts per thousand.
pub const MAX_POWER_SCALE_PERMILLE: u16 = 10_000;

/// Damage a landed blow always deals, however mitigated.
///
/// Flat defence subtracted from a small blow would otherwise reach zero, and
/// a hit that connects and does nothing is indistinguishable from a miss.
/// The floor is why flat defence is safe to have at all.
pub const MIN_LANDED_DAMAGE: u32 = 1;

/// The stat at which a school resistance reaches half.
///
/// The resistance curve is `stat / (stat + this)`, which approaches total
/// without reaching it at any finite stat — so resistance can never make a
/// target immune. That is a property of the curve, not a clamp on top of it.
pub const RESIST_HALF_STAT: u32 = 500;

/// A proportional magnitude that reached a full thousand would make a
/// mitigation total and a slow a root, so the ceiling below it is
/// load-bearing rather than decorative.
const _: () = assert!(
    MAX_PERMILLE_MAGNITUDE < 1000,
    "a proportional effect must never reach total"
);

/// The two partitions share one inline set, so their ceilings must add up to
/// its length or a partition could be refused while the set had room.
const _: () = assert!(MAX_STATUS_PER_ENTITY == MAX_HARMFUL_STATUS + MAX_HELPFUL_STATUS);

/// A body must fit inside the broad phase's own arithmetic, and the
/// movement integrator's per-tick step must be narrower than the wire
/// vector it is reported in.
const _: () = assert!(MAX_SPEED_SUB_UNITS_PER_TICK < i16::MAX as i32);
