//! The zone: every body in one region, and the step that advances them.
//!
//! # The step's order is the specification
//!
//! One step is total — every phase runs, every tick — and its order is
//! fixed. Two realms that resolved the same tick in a different order would
//! diverge, and nothing downstream recovers from that, so the order is
//! written down rather than left to whichever phase happened to be first:
//!
//! 1. **Clear** the previous step's notifications and refusals.
//! 2. **Bucket** every body in the broad phase, from the positions the
//!    previous step left final.
//! 3. **Statuses**: read each body's periodic effect and apply it, then age
//!    durations and the diminishing ledger. Before movement, so a root or a
//!    slow that lands this tick binds this tick.
//! 4. **Intents**: apply every admitted intent, in the order the clock
//!    placed them — earliest sample first, then by identity.
//! 5. **Movement**: advance every body, in identity order.
//! 6. **Separation**: gather every overlapping pair's correction, then
//!    apply them all at once, so no body's outcome depends on whether its
//!    neighbour was processed first.
//! 7. **Reap** the bodies at zero health, one death notification each.
//! 8. **Advance** the tick and refill each body's intent budget.
//!
//! Bodies are iterated in identity order everywhere, from an array kept
//! sorted by an identity the zone mints monotonically. "Whatever order the
//! map iterated" is a defect, not a detail.
//!
//! # What is admitted, and when it is adjudicated
//!
//! [`Zone::submit`] adjudicates what can be judged on arrival — the entity
//! exists, the sequence has not been applied, the tick's budget is not
//! spent, the sample time is not in the future — and queues the rest. What
//! depends on the state of the tick the intent will land in, like whether
//! the body is stunned, is judged in the step, because a stun landing in
//! phase three must bind an intent applied in phase four. A refusal from
//! either is recorded on [`Zone::refusals`] and never dropped silently.

use alloc::vec::Vec;

use tairix_wintersun_net::client::{Intent, IntentKind};
use tairix_wintersun_net::value::{
    EntityId, EntityState, GameEvent, PlayEvent, TickInstant, WorldPoint,
};

use crate::bounds::{
    MAX_BODY_RADIUS_SUB_UNITS, MAX_INTENTS_PER_TICK, MAX_SPEED_SUB_UNITS_PER_TICK,
};
use crate::clock::{self, TickRate};
use crate::damage::{self, Blow, Landed};
use crate::entity::{Entity, SpawnSpec};
use crate::error::{Refusal, RulesError, ZoneError};
use crate::motion;
use crate::space::BroadPhase;
use crate::status::{Application, Status};
use crate::terrain::{cell_at, Terrain};

/// An intent the realm took but has not yet applied.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
struct Pending {
    placed: TickInstant,
    entity: EntityId,
    sequence: u64,
    kind: IntentKind,
}

/// An intent the step could not honour.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Refused {
    /// Whose it was.
    pub entity: EntityId,
    /// Which of that client's intents.
    pub sequence: u64,
    /// Why.
    pub reason: Refusal,
}

/// Every body in one region of the world, and the clock they share.
#[derive(Clone, Debug)]
pub struct Zone {
    tick: u64,
    rate: TickRate,
    next_id: u64,
    entities: Vec<Entity>,
    pending: Vec<Pending>,
    corrections: Vec<(i64, i64)>,
    events: Vec<GameEvent>,
    refusals: Vec<Refused>,
    broad: BroadPhase,
}

impl Zone {
    /// An empty zone at tick zero.
    #[must_use]
    pub const fn new(rate: TickRate) -> Self {
        Self {
            tick: 0,
            rate,
            next_id: 1,
            entities: Vec::new(),
            pending: Vec::new(),
            corrections: Vec::new(),
            events: Vec::new(),
            refusals: Vec::new(),
            broad: BroadPhase::new(),
        }
    }

    /// The tick about to be simulated.
    #[must_use]
    pub const fn tick(&self) -> u64 {
        self.tick
    }

    /// The rate it runs at.
    #[must_use]
    pub const fn rate(&self) -> TickRate {
        self.rate
    }

    /// Every body, in identity order.
    #[must_use]
    pub fn entities(&self) -> &[Entity] {
        &self.entities
    }

    /// How many bodies are live.
    #[must_use]
    pub fn population(&self) -> usize {
        self.entities.len()
    }

    /// One body.
    #[must_use]
    pub fn entity(&self, id: EntityId) -> Option<&Entity> {
        self.index_of(id).and_then(|index| self.entities.get(index))
    }

    /// What every body looks like to a client.
    pub fn states(&self) -> impl Iterator<Item = EntityState> + '_ {
        self.entities.iter().map(Entity::state)
    }

    /// Notifications the last step produced, cleared at the start of the
    /// next.
    #[must_use]
    pub fn events(&self) -> &[GameEvent] {
        &self.events
    }

    /// Intents the last step could not honour, cleared at the start of the
    /// next.
    #[must_use]
    pub fn refusals(&self) -> &[Refused] {
        &self.refusals
    }

    /// Bring a body into the zone, at full health.
    ///
    /// Grows the submission queue to the whole population's worst case as
    /// each body arrives, so [`Zone::submit`] can adjudicate without also
    /// having to fail: an intent is refused for a reason a client is told,
    /// never because a vector could not grow.
    ///
    /// # Errors
    ///
    /// [`RulesError::OutOfMemory`] when the zone cannot be grown.
    pub fn spawn(&mut self, spec: SpawnSpec) -> Result<EntityId, RulesError> {
        let oom = |_| RulesError::OutOfMemory;
        self.entities.try_reserve(1).map_err(oom)?;
        self.corrections.try_reserve(1).map_err(oom)?;
        // `try_reserve` guarantees capacity for `len + additional`, so the
        // queue's whole worst case is asked for rather than one body's
        // share: every body may spend its budget in the same tick.
        let queue = self
            .entities
            .len()
            .saturating_add(1)
            .saturating_mul(usize::from(MAX_INTENTS_PER_TICK));
        self.pending
            .try_reserve(queue.saturating_sub(self.pending.len()))
            .map_err(oom)?;

        let id = EntityId(self.next_id);
        self.next_id = self.next_id.saturating_add(1);
        self.entities.push(Entity::spawn(id, spec));
        Ok(id)
    }

    /// Remove a body, answering whether it was there.
    pub fn despawn(&mut self, id: EntityId) -> bool {
        match self.index_of(id) {
            Some(index) => {
                self.entities.remove(index);
                self.pending.retain(|queued| queued.entity != id);
                true
            }
            None => false,
        }
    }

    /// Take an intent, or say why not.
    ///
    /// The returned instant is where the clock placed it, which is what the
    /// step sorts by.
    ///
    /// # Errors
    ///
    /// [`Refusal::SampledInTheFuture`] for a sample ahead of the realm,
    /// [`Refusal::NoSuchEntity`] for a body this zone does not hold,
    /// [`Refusal::Replayed`] for a sequence already adjudicated, and
    /// [`Refusal::TooManyThisTick`] once the tick's budget is spent.
    pub fn submit(&mut self, id: EntityId, intent: &Intent) -> Result<TickInstant, Refusal> {
        let placed = clock::place(self.tick, intent.sampled)?;
        let index = self.index_of(id).ok_or(Refusal::NoSuchEntity(id))?;
        let entity = self
            .entities
            .get_mut(index)
            .ok_or(Refusal::NoSuchEntity(id))?;
        if intent.sequence <= entity.acknowledged() {
            return Err(Refusal::Replayed);
        }
        if entity.admitted_this_tick() >= MAX_INTENTS_PER_TICK {
            return Err(Refusal::TooManyThisTick);
        }
        entity.acknowledge(intent.sequence);
        self.pending.push(Pending {
            placed,
            entity: id,
            sequence: intent.sequence,
            kind: intent.kind,
        });
        Ok(placed)
    }

    /// Land a blow on a body, running the whole damage pipeline.
    ///
    /// `attacker` is absent for a blow with nobody behind it — a trap, a
    /// fall, a hazard — which scales with no stats at all. Death is reaped
    /// by the next step, which is also what emits the notification for it.
    ///
    /// # Errors
    ///
    /// [`ZoneError::Refused`] with [`Refusal::NoSuchEntity`] for a body this
    /// zone does not hold, and [`ZoneError::OutOfMemory`] when the
    /// notification could not be recorded.
    pub fn apply_blow(
        &mut self,
        attacker: Option<EntityId>,
        target: EntityId,
        blow: &Blow,
    ) -> Result<Landed, ZoneError> {
        let attacker_stats = match attacker {
            Some(id) => self.entity(id).ok_or(Refusal::NoSuchEntity(id))?.stats(),
            None => crate::stat::Stats::default(),
        };
        let index = self.index_of(target).ok_or(Refusal::NoSuchEntity(target))?;
        self.events
            .try_reserve(1)
            .map_err(|_| ZoneError::OutOfMemory)?;

        let (landed, at) = {
            let body = self
                .entities
                .get_mut(index)
                .ok_or(Refusal::NoSuchEntity(target))?;
            let landed = damage::resolve(
                blow,
                attacker_stats,
                body.stats(),
                body.armour(),
                body.status(),
            );
            body.status_mut().consume_absorb(landed.absorbed);
            body.health_mut().drain(landed.to_health);
            (landed, body.at())
        };
        self.note(
            at,
            PlayEvent::Damage {
                target,
                source: attacker,
                amount: landed.total(),
            },
        )?;
        Ok(landed)
    }

    /// Heal a body, returning what it actually received.
    ///
    /// `healer` is absent for healing with nobody behind it — a shrine, a
    /// resting bonus — which scales with no stats.
    ///
    /// # Errors
    ///
    /// [`ZoneError::Refused`] with [`Refusal::NoSuchEntity`] for a body this
    /// zone does not hold.
    pub fn apply_heal(
        &mut self,
        healer: Option<EntityId>,
        target: EntityId,
        base: u32,
    ) -> Result<u32, ZoneError> {
        let healer_stats = match healer {
            Some(id) => self.entity(id).ok_or(Refusal::NoSuchEntity(id))?.stats(),
            None => crate::stat::Stats::default(),
        };
        let index = self.index_of(target).ok_or(Refusal::NoSuchEntity(target))?;
        let body = self
            .entities
            .get_mut(index)
            .ok_or(Refusal::NoSuchEntity(target))?;
        let amount = damage::healed(base, healer_stats, body.status());
        Ok(body.health_mut().restore(amount))
    }

    /// Put a status on a body.
    ///
    /// # Errors
    ///
    /// [`ZoneError::Refused`] with [`Refusal::NoSuchEntity`] for a body this
    /// zone does not hold.
    pub fn apply_status(
        &mut self,
        target: EntityId,
        status: Status,
    ) -> Result<Application, ZoneError> {
        let index = self.index_of(target).ok_or(Refusal::NoSuchEntity(target))?;
        let body = self
            .entities
            .get_mut(index)
            .ok_or(Refusal::NoSuchEntity(target))?;
        Ok(body.status_mut().apply(status))
    }

    /// Spend from a body's resource pool, answering whether it could pay.
    ///
    /// # Errors
    ///
    /// [`ZoneError::Refused`] with [`Refusal::NoSuchEntity`] for a body this
    /// zone does not hold.
    pub fn spend_resource(&mut self, id: EntityId, amount: u32) -> Result<bool, ZoneError> {
        let index = self.index_of(id).ok_or(Refusal::NoSuchEntity(id))?;
        let body = self
            .entities
            .get_mut(index)
            .ok_or(Refusal::NoSuchEntity(id))?;
        Ok(body.resource_mut().spend(amount))
    }

    /// Advance the simulation one tick.
    ///
    /// # Errors
    ///
    /// [`RulesError::OutOfMemory`] when the per-tick arrays could not be
    /// grown to the population.
    pub fn step(&mut self, terrain: &impl Terrain) -> Result<(), RulesError> {
        self.events.clear();
        self.refusals.clear();

        self.broad
            .rebuild(self.entities.iter().map(|body| (body.id(), body.at())))?;

        self.tick_statuses();
        self.apply_pending()?;
        self.advance_movement(terrain);
        self.resolve_separation(terrain)?;
        self.reap()?;

        self.tick = self.tick.saturating_add(1);
        for body in &mut self.entities {
            body.reset_tick_budget();
        }
        Ok(())
    }

    /// Phase three: periodic effects, then ageing.
    fn tick_statuses(&mut self) {
        let window = self.rate.diminish_reset_ticks();
        for body in &mut self.entities {
            let periodic = body.status().periodic_health();
            if periodic != 0 {
                body.health_mut().apply_delta(periodic);
            }
            body.status_mut().advance(window);
        }
    }

    /// Phase four: every admitted intent, in placement order.
    ///
    /// # Errors
    ///
    /// [`RulesError::OutOfMemory`] when the refusal list cannot be grown.
    fn apply_pending(&mut self) -> Result<(), RulesError> {
        self.refusals
            .try_reserve(self.pending.len())
            .map_err(|_| RulesError::OutOfMemory)?;
        self.pending
            .sort_unstable_by_key(|queued| (queued.placed, queued.entity, queued.sequence));
        // Taken out rather than iterated in place, so an intent adjudicated
        // here cannot be re-adjudicated next tick, and the allocation is
        // handed back for the next one.
        let mut queued = core::mem::take(&mut self.pending);
        for item in &queued {
            if let Err(reason) = self.apply_one(item) {
                self.refusals.push(Refused {
                    entity: item.entity,
                    sequence: item.sequence,
                    reason,
                });
            }
        }
        queued.clear();
        self.pending = queued;
        Ok(())
    }

    /// One intent, against the state of the tick it landed in.
    ///
    /// A movement is recorded whatever the body's statuses say — a root
    /// works through the speed, not by discarding the input — where a
    /// discrete action a stun or a silence forbids is refused with the
    /// reason.
    fn apply_one(&mut self, item: &Pending) -> Result<(), Refusal> {
        let index = self
            .index_of(item.entity)
            .ok_or(Refusal::NoSuchEntity(item.entity))?;
        let body = self
            .entities
            .get_mut(index)
            .ok_or(Refusal::NoSuchEntity(item.entity))?;
        match item.kind {
            IntentKind::Move(direction) => {
                body.hold(direction);
                Ok(())
            }
            _ if !body.status().may_act() => Err(Refusal::Stunned),
            IntentKind::Cast { .. } if !body.status().may_cast() => Err(Refusal::Silenced),
            // No action, spell, inventory or interaction table exists to
            // resolve these yet, so every identifier is an unknown one —
            // which is the same refusal the lookup will give once a table
            // does exist.
            IntentKind::Action { .. }
            | IntentKind::Cast { .. }
            | IntentKind::Interact { .. }
            | IntentKind::Item { .. } => Err(Refusal::Unresolvable),
        }
    }

    /// Phase five: integrate every body.
    fn advance_movement(&mut self, terrain: &impl Terrain) {
        for body in &mut self.entities {
            let step = motion::step(body, terrain);
            body.place(step.at, step.residue, step.moved);
        }
    }

    /// Phase six: gather every overlap's correction, then apply them at
    /// once.
    ///
    /// The broad phase holds pre-movement positions, so the query radius is
    /// inflated by one tick of the fastest travel: a body that ended up
    /// within touching distance was, before it moved, no further away than
    /// that.
    fn resolve_separation(&mut self, terrain: &impl Terrain) -> Result<(), RulesError> {
        self.corrections.clear();
        self.corrections
            .try_reserve(self.entities.len())
            .map_err(|_| RulesError::OutOfMemory)?;
        self.corrections.resize(self.entities.len(), (0, 0));

        let Self {
            entities,
            broad,
            corrections,
            ..
        } = self;
        let inflation =
            i64::from(MAX_BODY_RADIUS_SUB_UNITS) + i64::from(MAX_SPEED_SUB_UNITS_PER_TICK);
        for (index, body) in entities.iter().enumerate() {
            let reach = i64::from(body.radius()) + inflation;
            broad.for_each_near(body.at(), reach, |other| {
                // Each pair once, and with the lower identity first, which
                // is what makes two exactly-coincident bodies part the same
                // way on every machine.
                if other <= body.id() {
                    return;
                }
                let Ok(far_index) = entities.binary_search_by_key(&other, Entity::id) else {
                    return;
                };
                let Some(far) = entities.get(far_index) else {
                    return;
                };
                let Some((dx, dy)) =
                    motion::separation(body.at(), body.radius(), far.at(), far.radius())
                else {
                    return;
                };
                if let Some(slot) = corrections.get_mut(index) {
                    slot.0 = slot.0.saturating_add(dx);
                    slot.1 = slot.1.saturating_add(dy);
                }
                if let Some(slot) = corrections.get_mut(far_index) {
                    slot.0 = slot.0.saturating_sub(dx);
                    slot.1 = slot.1.saturating_sub(dy);
                }
            });
        }

        for (index, body) in entities.iter_mut().enumerate() {
            let Some(&(dx, dy)) = corrections.get(index) else {
                continue;
            };
            if dx == 0 && dy == 0 {
                continue;
            }
            let at = motion::offset(body.at(), dx, dy);
            // A push must not shove a body through a wall, so the
            // correction is taken only where the ground would have let it
            // walk there.
            if motion::footprint_clear(terrain, cell_at(body.at()), at, body.radius()) {
                // The reported motion is unchanged: a push is not travel to
                // extrapolate, and a client that extrapolated one would
                // slide a body that is standing still.
                body.place(at, body.residue(), body.motion());
            }
        }
        Ok(())
    }

    /// Phase seven: the dead leave, each with a notification.
    ///
    /// # Errors
    ///
    /// [`RulesError::OutOfMemory`] when the notifications cannot be
    /// recorded.
    fn reap(&mut self) -> Result<(), RulesError> {
        let fallen = self.entities.iter().filter(|body| !body.is_alive()).count();
        if fallen == 0 {
            return Ok(());
        }
        self.events
            .try_reserve(fallen)
            .map_err(|_| RulesError::OutOfMemory)?;
        let tick = self.tick;
        let Self {
            entities, events, ..
        } = self;
        for body in entities.iter().filter(|body| !body.is_alive()) {
            events.push(GameEvent {
                tick,
                at: body.at(),
                event: PlayEvent::Death { entity: body.id() },
            });
        }
        entities.retain(Entity::is_alive);
        Ok(())
    }

    /// Record a notification.
    ///
    /// # Errors
    ///
    /// [`RulesError::OutOfMemory`] when the list cannot be grown.
    fn note(&mut self, at: WorldPoint, event: PlayEvent) -> Result<(), RulesError> {
        self.events
            .try_reserve(1)
            .map_err(|_| RulesError::OutOfMemory)?;
        self.events.push(GameEvent {
            tick: self.tick,
            at,
            event,
        });
        Ok(())
    }

    /// The index of a body, by binary search over an array the monotonic
    /// identity keeps sorted.
    fn index_of(&self, id: EntityId) -> Option<usize> {
        self.entities.binary_search_by_key(&id, Entity::id).ok()
    }
}

#[cfg(test)]
mod tests;
