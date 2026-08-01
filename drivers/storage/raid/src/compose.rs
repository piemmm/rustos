//! The composer's **registration → assembly → publication** decision logic
//! (`plans/FIX-IO.md` `IO6d`): which discovered members belong to which array,
//! and when an array may be brought online.
//!
//! # What this decides, and why it is separate from doing it
//!
//! The composer's live half holds IPC, block transports, and published nodes;
//! none of that can be exercised without a kernel. The judgement it makes is
//! where the data-integrity risk lives, so it is pure logic here, driven by a
//! caller-supplied monotonic clock and proven host-side over member doubles.
//! The live half reads a device's superblock, hands it to [`MemberRegistry`],
//! and does what [`MemberRegistry::next_action`] says.
//!
//! # Bring an array online, but never a broken one
//!
//! Two failures are possible and both lose data, so the rules that avoid them
//! are stated once, here:
//!
//! - **Serving an array that cannot answer for itself.** A stripe missing a
//!   member, or a RAID5 missing two, has holes no redundancy can fill.
//!   Publishing it would hand a filesystem a device that silently cannot read
//!   parts of itself, so such an array is left unassembled until the missing
//!   members arrive — the shared level rule
//!   [`RaidLevel::can_serve`](tairix_raid::RaidLevel::can_serve) is the single
//!   definition of that question.
//! - **Starting degraded too eagerly.** Discovery is asynchronous: members
//!   appear one at a time, and a member that is merely slow — spinning up, or
//!   riding out a bus blip inside its own driver's recovery grace window — is
//!   not a missing member. Bringing the array up without it forces a needless
//!   rebuild of a disk that was never really absent, so an incomplete array
//!   waits a **settle window** before it starts degraded. A *complete* array
//!   never waits: there is nothing left to wait for.
//!
//! The settle window is the array's own hardware's recovery grace window,
//! taken through the shared [`RetryCadence::for_class`] and folded over the
//! members' declared classes with
//! [`BlkDeviceClass::most_patient`](tairix_abi::blkio::BlkDeviceClass::most_patient),
//! so a rotational array waits out a spin-up while a solid-state one does not
//! — a policy read from the discovered hardware rather than a number chosen on
//! a developer's machine. A mixed array is only as impatient as its slowest
//! member, so the window widens when one registers; but it always runs from
//! the instant the array's *first* member appeared, so a steady trickle of
//! arrivals cannot postpone assembly indefinitely. A member that turns up
//! after the array started degraded rejoins as the stale rebuild target its
//! generation counter says it is.
//!
//! A failed assembly attempt escalates the same [`RetryState`] rather than
//! being retried at once, so an array whose devices are unreachable is not
//! re-probed in a tight loop. Every wait this engine asks for is an absolute
//! deadline strictly in the future, so the caller parks on a one-shot timer
//! and never spins.
//!
//! # What is believed, and what is read
//!
//! Nothing an agent says about its device is trusted. A member's array, slot,
//! and generation come from the superblock read off the disk, and the
//! authoritative array shape comes from the freshest member of the set
//! ([`ArrayIdentity::resolve`]). A member disagreeing with that shape, or
//! losing a slot contest, is simply never placed — it stays registered and
//! unused, because a later, fresher member can legitimately redefine the array
//! and make it placeable. The engine therefore never refuses a device whose
//! metadata decoded: a refusal would give a corrupt or hostile disk the power
//! to evict a healthy one from consideration.

use alloc::vec::Vec;

use tairix_abi::blkio::BlkDeviceClass;
use tairix_abi::raid_ipc::MemberOffer;
use tairix_raid::{
    ArrayIdentity, ArraySuperblock, ArrayUuid, Candidate, CandidateVerdict, RetryCadence,
    RetryState, SlotDisposition,
};

/// Whether a registered member has been placed into a composed array yet.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum MemberStanding {
    /// Registered and available, but not part of a composed array: either its
    /// array has not been assembled yet, or the reassembly does not place it
    /// (a shape mismatch, or a slot a fresher copy holds).
    Held,
    /// Placed into the composed array its superblock names.
    Composed,
}

/// One member device the composer holds: the membership call it answers when
/// the membership ends, and the transport the member's agent delegated.
///
/// The device's *metadata* is deliberately not here — it lives once, in the
/// reassembly candidate at the same index
/// ([`MemberRegistry::candidates`]) — so no fact about a member is stored
/// twice.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct HeldMember {
    membership: u64,
    offer: MemberOffer,
    class: BlkDeviceClass,
    standing: MemberStanding,
}

impl HeldMember {
    /// The outstanding offer call the composer answers to end this membership.
    #[must_use]
    pub const fn membership(&self) -> u64 {
        self.membership
    }

    /// The transport the member's agent delegated.
    #[must_use]
    pub const fn offer(&self) -> MemberOffer {
        self.offer
    }

    /// The device class the member declared when the composer connected to it,
    /// which sizes the patience its array is assembled with.
    #[must_use]
    pub const fn class(&self) -> BlkDeviceClass {
        self.class
    }

    /// Whether this member has been placed into a composed array.
    #[must_use]
    pub const fn standing(&self) -> MemberStanding {
        self.standing
    }
}

/// The outcome of offering a device to the registry.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Admission {
    /// The device is registered at this index. Indices are stable only until
    /// the next [`MemberRegistry::admit`] or [`MemberRegistry::release`].
    Registered {
        /// Where the member landed in [`MemberRegistry::members`].
        index: usize,
    },
    /// A member with the same block-service endpoint is already registered, so
    /// this offer is a second membership for a device the composer already
    /// holds. Refused rather than registered twice: two memberships for one
    /// device would let it occupy a slot twice over.
    Duplicate,
    /// The registry could not grow to hold the member. Allocation failure is a
    /// value, never a panic, and the caller ends the membership rather than
    /// silently dropping the device.
    OutOfMemory,
}

/// What the composer should do next.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ComposerAction {
    /// Compose this array from its registered members and publish it. The
    /// caller resolves the shape with [`MemberRegistry::identity`], fills its
    /// own slot table, and reports the outcome through
    /// [`MemberRegistry::note_composed`] or
    /// [`MemberRegistry::note_assembly_failed`].
    Assemble {
        /// The array to bring online.
        array_uuid: ArrayUuid,
    },
    /// Place this already-registered member into the *already composed* array
    /// it belongs to — the disk that turned up late, or came back.
    Join {
        /// The live array to place it into.
        array_uuid: ArrayUuid,
        /// Its index in [`MemberRegistry::members`], valid until the next
        /// [`MemberRegistry::admit`] or [`MemberRegistry::release`].
        member: usize,
        /// The array slot it fills.
        slot: u16,
        /// Whether its generation is current (`true`) or behind, making it a
        /// rebuild target that must not serve reads until resynced.
        in_sync: bool,
    },
    /// Nothing to do. Park until a member registers, or until `deadline_ns`
    /// if one is given — an absolute monotonic instant that is always strictly
    /// in the future, so parking on it is a wait and never a spin.
    Wait {
        /// The soonest settle or retry deadline across the arrays, if any.
        deadline_ns: Option<u64>,
    },
}

/// One array the registry knows about, and its position in the settle window
/// and the retry cadence.
struct ArrayState {
    uuid: ArrayUuid,
    /// When the array's first member registered. The settle window runs from
    /// here, so later arrivals cannot push it back.
    first_seen_ns: u64,
    /// The escalating backoff after a refused assembly attempt. Unarmed while
    /// no attempt has been refused.
    attempt: RetryState,
    /// Whether the array is composed and published.
    published: bool,
}

/// How one array stands against the members registered so far.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum Assessment {
    /// Compose it now.
    Ready,
    /// Wait until this absolute monotonic deadline, which is strictly in the
    /// future.
    Waiting(u64),
    /// The members present cannot serve the array. Nothing but a further
    /// member changes that, so there is no deadline to wait for.
    Unservable,
}

/// The composer's growable table of registered array members, and the
/// assembly decisions it draws from them.
///
/// The composer reads each offered device's own superblock, registers it with
/// [`admit`](Self::admit), and does what [`next_action`](Self::next_action)
/// says. Two rules keep a broken array off the bus:
///
/// - **An array its members cannot serve is never composed.** The shared
///   [`RaidLevel::can_serve`](tairix_raid::RaidLevel::can_serve) is the single
///   definition of that question: a punctured stripe or a twice-punctured
///   RAID5 has holes no redundancy can fill, so it is left unassembled until
///   the missing members arrive rather than handed to a filesystem as a device
///   that silently cannot read parts of itself.
/// - **A complete array is composed at once; an incomplete one settles
///   first.** A member that is merely spinning up, or riding out a blip inside
///   its own driver's recovery grace window, is not a missing member, and
///   starting without it forces a needless rebuild. The settle window is that
///   hardware's own grace window ([`RetryCadence::for_class`] over the members'
///   declared classes), widening to the slowest member but always running from
///   the array's first, so a trickle of arrivals cannot postpone assembly
///   indefinitely.
///
/// A member the authoritative shape does not place is held unused rather than
/// refused, so no single corrupt disk can evict a healthy one from
/// consideration; every wait is an absolute deadline strictly in the future,
/// so the caller parks on a one-shot timer and never spins; and the table
/// grows fallibly, so there is no member ceiling and exhaustion is a value
/// rather than a panic.
pub struct MemberRegistry {
    /// One entry per registered member, in registration order.
    members: Vec<HeldMember>,
    /// The reassembly view of the same members: `candidates[i]` is the
    /// metadata of `members[i]`, and its `tag` is `i`. Maintained only by
    /// [`Self::admit`] and [`Self::release`], which keep the two tables the
    /// same length and the tags in step.
    candidates: Vec<Candidate>,
    /// One entry per distinct array among the members.
    arrays: Vec<ArrayState>,
    /// The slot table reused while assessing an array, so a decision needs no
    /// allocation of the caller's.
    scratch: Vec<SlotDisposition>,
}

impl Default for MemberRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl MemberRegistry {
    /// An empty registry.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            members: Vec::new(),
            candidates: Vec::new(),
            arrays: Vec::new(),
            scratch: Vec::new(),
        }
    }

    /// The members registered so far, in registration order.
    #[must_use]
    pub fn members(&self) -> &[HeldMember] {
        &self.members
    }

    /// The reassembly view of the registered members: `candidates()[i]`
    /// describes `members()[i]`, and its tag is `i`, so a slot table resolved
    /// from these candidates maps straight back to the member that fills it.
    #[must_use]
    pub fn candidates(&self) -> &[Candidate] {
        &self.candidates
    }

    /// The authoritative shape of `array_uuid` as the registered members
    /// describe it, or [`None`] when none of them claims that array.
    #[must_use]
    pub fn identity(&self, array_uuid: ArrayUuid) -> Option<ArrayIdentity> {
        ArrayIdentity::resolve(array_uuid, &self.candidates).ok()
    }

    /// Register a member device: the membership call to answer when it leaves,
    /// the transport its agent delegated, the class the device declared, and
    /// the superblock read off the device itself.
    ///
    /// `superblock` must be one the caller decoded from the device
    /// ([`ArraySuperblock::decode`]); this engine never takes a member's array,
    /// slot, or generation from anything the agent said.
    pub fn admit(
        &mut self,
        membership: u64,
        offer: MemberOffer,
        class: BlkDeviceClass,
        superblock: ArraySuperblock,
        now_ns: u64,
    ) -> Admission {
        if self
            .members
            .iter()
            .any(|held| held.offer.endpoint == offer.endpoint)
        {
            return Admission::Duplicate;
        }
        // Reserve every table before touching any of them, so an allocation
        // failure leaves the registry exactly as it was rather than half
        // updated. A reservation a table already has room for costs nothing.
        if self.members.try_reserve(1).is_err()
            || self.candidates.try_reserve(1).is_err()
            || self.arrays.try_reserve(1).is_err()
        {
            return Admission::OutOfMemory;
        }
        let index = self.members.len();
        let array_uuid = superblock.array_uuid;
        self.members.push(HeldMember {
            membership,
            offer,
            class,
            standing: MemberStanding::Held,
        });
        self.candidates.push(Candidate {
            tag: index,
            superblock,
        });
        if self.array_index(array_uuid).is_none() {
            self.arrays.push(ArrayState {
                uuid: array_uuid,
                first_seen_ns: now_ns,
                attempt: RetryState::new(),
                published: false,
            });
        }
        Admission::Registered { index }
    }

    /// Remove the member at `index` — its membership ended, or its device went
    /// away — and hand it back so the caller can answer the outstanding call.
    ///
    /// Later members shift down, so any index or slot table the caller holds
    /// from before this call is stale and must be recomputed.
    pub fn release(&mut self, index: usize) -> Option<HeldMember> {
        if index >= self.members.len() {
            return None;
        }
        let member = self.members.remove(index);
        let candidate = self.candidates.remove(index);
        for (tag, entry) in self.candidates.iter_mut().enumerate() {
            entry.tag = tag;
        }
        let array_uuid = candidate.superblock.array_uuid;
        if !self.claims_array(array_uuid) {
            self.arrays.retain(|state| state.uuid != array_uuid);
        }
        Some(member)
    }

    /// What the composer should do next, given the monotonic clock reading
    /// `now_ns`.
    ///
    /// Arrays are considered in first-registration order, so the decision is
    /// deterministic; bringing an array online is preferred to growing one
    /// that is already serving.
    pub fn next_action(&mut self, now_ns: u64) -> ComposerAction {
        let mut soonest: Option<u64> = None;
        for index in 0..self.arrays.len() {
            if self.arrays[index].published {
                continue;
            }
            match self.assess(index, now_ns) {
                Assessment::Ready => {
                    return ComposerAction::Assemble {
                        array_uuid: self.arrays[index].uuid,
                    }
                }
                Assessment::Waiting(deadline_ns) => {
                    soonest = Some(soonest.map_or(deadline_ns, |held: u64| held.min(deadline_ns)));
                }
                Assessment::Unservable => {}
            }
        }
        self.pending_join().unwrap_or(ComposerAction::Wait {
            deadline_ns: soonest,
        })
    }

    /// Record that `array_uuid` was composed and published from `slots`, the
    /// slot table the caller assembled it with.
    ///
    /// Only slots holding a member of *this* array are marked, so a slot table
    /// resolved for a different array cannot mark the wrong devices composed.
    pub fn note_composed(&mut self, array_uuid: ArrayUuid, slots: &[SlotDisposition]) {
        for slot in slots {
            let SlotDisposition::Present { tag, .. } = *slot else {
                continue;
            };
            let belongs = self
                .candidates
                .get(tag)
                .is_some_and(|candidate| candidate.superblock.array_uuid == array_uuid);
            if !belongs {
                continue;
            }
            if let Some(member) = self.members.get_mut(tag) {
                member.standing = MemberStanding::Composed;
            }
        }
        if let Some(state) = self.array_state_mut(array_uuid) {
            state.published = true;
            state.attempt.disarm();
        }
    }

    /// Record that the member at `index` was placed into its live array, so it
    /// is not offered for placement again.
    pub fn note_joined(&mut self, index: usize) {
        if let Some(member) = self.members.get_mut(index) {
            member.standing = MemberStanding::Composed;
        }
    }

    /// Record that composing `array_uuid` failed — its devices could not be
    /// read, or the engine refused the member set — so the next attempt is
    /// delayed, doubling towards the cadence ceiling rather than retried at
    /// once.
    pub fn note_assembly_failed(&mut self, array_uuid: ArrayUuid, now_ns: u64) {
        let cadence = self.array_cadence(array_uuid);
        if let Some(state) = self.array_state_mut(array_uuid) {
            state.attempt.note_failure(cadence, now_ns);
        }
    }

    /// How the array at `index` in [`Self::arrays`] stands at `now_ns`.
    fn assess(&mut self, index: usize, now_ns: u64) -> Assessment {
        let array_uuid = self.arrays[index].uuid;
        let Some(identity) = self.identity(array_uuid) else {
            return Assessment::Unservable;
        };
        if !self.fit_scratch(usize::from(identity.member_count)) {
            return Assessment::Unservable;
        }
        if identity
            .fill_slots(&self.candidates, &mut self.scratch)
            .is_err()
        {
            return Assessment::Unservable;
        }
        if !identity.raid_level.can_serve(&self.scratch) {
            return Assessment::Unservable;
        }
        let complete = self
            .scratch
            .iter()
            .all(|slot| matches!(slot, SlotDisposition::Present { .. }));
        let settle_ns = self.array_cadence(array_uuid).base_ns();
        let state = &self.arrays[index];
        // A refused attempt is paced whatever the member set looks like, so
        // unreachable devices are not re-probed in a tight loop.
        if let Some(due_ns) = state.attempt.due_ns() {
            if due_ns > now_ns {
                return Assessment::Waiting(due_ns);
            }
        }
        // A complete array has nothing left to wait for; an incomplete one
        // serves out its settle window before it starts degraded.
        if complete {
            return Assessment::Ready;
        }
        let settle_deadline_ns = state.first_seen_ns.saturating_add(settle_ns);
        if settle_deadline_ns > now_ns {
            Assessment::Waiting(settle_deadline_ns)
        } else {
            Assessment::Ready
        }
    }

    /// The first held member that belongs in an array which is already
    /// composed, and whose slot no composed member occupies.
    fn pending_join(&self) -> Option<ComposerAction> {
        for (index, (candidate, member)) in self.candidates.iter().zip(&self.members).enumerate() {
            if member.standing != MemberStanding::Held {
                continue;
            }
            let array_uuid = candidate.superblock.array_uuid;
            if !self
                .array_state(array_uuid)
                .is_some_and(|state| state.published)
            {
                continue;
            }
            let Some(identity) = self.identity(array_uuid) else {
                continue;
            };
            let CandidateVerdict::Placed { slot, in_sync } =
                identity.verdict_of(&self.candidates, index)
            else {
                continue;
            };
            if self.slot_is_composed(array_uuid, slot) {
                continue;
            }
            return Some(ComposerAction::Join {
                array_uuid,
                member: index,
                slot,
                in_sync,
            });
        }
        None
    }

    /// Whether a composed member of `array_uuid` already occupies `slot`.
    fn slot_is_composed(&self, array_uuid: ArrayUuid, slot: u16) -> bool {
        self.candidates
            .iter()
            .zip(&self.members)
            .any(|(candidate, member)| {
                member.standing == MemberStanding::Composed
                    && candidate.superblock.array_uuid == array_uuid
                    && candidate.superblock.member_slot == slot
            })
    }

    /// Size the scratch slot table to `len` missing slots, or report that the
    /// registry could not grow to hold it.
    fn fit_scratch(&mut self, len: usize) -> bool {
        self.scratch.clear();
        if self.scratch.try_reserve(len).is_err() {
            return false;
        }
        self.scratch.resize(len, SlotDisposition::Missing);
        true
    }

    /// The settle/backoff cadence for `array_uuid`: the recovery grace window
    /// of the most patient class among its registered members, so an array of
    /// spinning disks is given the time one of them may legitimately take to
    /// spin up while an array of solid-state devices is not.
    ///
    /// Folded over the members present *now*, so a mixed array widens its
    /// window when its slowest member registers rather than being held to the
    /// impatience of whichever member happened to appear first. The window
    /// still runs from the array's first member, so a widening changes how
    /// long the array waits but never restarts the wait.
    fn array_cadence(&self, array_uuid: ArrayUuid) -> RetryCadence {
        let mut widest: Option<BlkDeviceClass> = None;
        for (candidate, member) in self.candidates.iter().zip(&self.members) {
            if candidate.superblock.array_uuid == array_uuid {
                widest = Some(match widest {
                    Some(held) => held.most_patient(member.class),
                    None => member.class,
                });
            }
        }
        // An array with no registered member has no hardware to read a policy
        // from, so it falls back to the unclassified default envelope.
        RetryCadence::for_class(widest.unwrap_or_default())
    }

    /// Whether any registered member claims membership of `array_uuid`.
    fn claims_array(&self, array_uuid: ArrayUuid) -> bool {
        self.candidates
            .iter()
            .any(|candidate| candidate.superblock.array_uuid == array_uuid)
    }

    /// Where `array_uuid` sits in [`Self::arrays`].
    fn array_index(&self, array_uuid: ArrayUuid) -> Option<usize> {
        self.arrays
            .iter()
            .position(|state| state.uuid == array_uuid)
    }

    /// The state of `array_uuid`, if the registry knows it.
    fn array_state(&self, array_uuid: ArrayUuid) -> Option<&ArrayState> {
        self.arrays.iter().find(|state| state.uuid == array_uuid)
    }

    /// The mutable state of `array_uuid`, if the registry knows it.
    fn array_state_mut(&mut self, array_uuid: ArrayUuid) -> Option<&mut ArrayState> {
        self.arrays
            .iter_mut()
            .find(|state| state.uuid == array_uuid)
    }
}

#[cfg(test)]
mod tests;
