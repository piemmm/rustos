//! The shared RAID array vocabulary: the composition an array uses
//! ([`RaidLevel`]), the disposition of one reassembled array slot
//! ([`SlotDisposition`]), the live health of a composed array
//! ([`ArrayHealth`]), and the membership state of one array slot
//! ([`MemberState`]).
//!
//! These types are the *single* definition every RAID layer names. The
//! on-disk metadata layer (`lib/raidmeta`) stamps [`RaidLevel`] into each
//! member's superblock and fills a [`SlotDisposition`] table when it
//! reassembles an array; the composition engines (`lib/raid`) turn that table
//! into live members and report [`ArrayHealth`] and per-slot [`MemberState`];
//! the storage-discovery probe (`lib/fsprobe`) reads a member's level; and the
//! System Information API's array-health reporting records and their generated
//! C view (`plans/FIX-IO.md` IO6) name the same vocabulary. Because that C
//! view is generated from this crate alone, and because the metadata and
//! composition crates sit *above* it, the vocabulary lives here so the on-disk
//! format, the composition engines, the control protocol, the reporting
//! records, and the C view all resolve to one definition.

use crate::error::Errno;
use crate::sysinfo::MountAvailability;

/// The largest data-member count a GF(2^8) parity array (RAID6 double parity
/// or RAID-TP triple parity) can have.
///
/// A GF(2^8) parity syndrome weights data member `k` by `gᵏ` (and the higher
/// syndrome by `g²ᵏ`), whose exponents `g⁰ … g²⁵⁴` are the 255 distinct
/// non-zero field elements (`g = {02}`, polynomial `0x11d` — the Linux-md
/// field). Beyond 255 data members those coefficients would repeat and the
/// erasures could no longer be solved, so a GF-parity array admits at most this
/// many data members. This is the single definition of that structural ceiling:
/// [`RaidLevel::max_members`] and the parity composition engines' fields all
/// derive from it.
pub const MAX_PARITY_DATA_MEMBERS: u16 = 255;

/// The composition a superblock describes. Encoded as one on-disk byte;
/// decoding an unrecognised value fails closed ([`from_u8`](Self::from_u8)
/// returns [`Errno::OutOfRange`]) so a member written by a future or foreign
/// format is never mistaken for a mirror.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum RaidLevel {
    /// RAID1 mirror (`MirrorArray`): every member is a full copy, so
    /// the array survives any subset of member faults while one copy remains.
    /// Carries no stripe unit (`ArraySuperblock::chunk_blocks` is `0`).
    Mirror = 1,
    /// RAID0 stripe (`StripeArray`): the logical block space is cut
    /// into fixed-size chunks round-robined across the members, so the array
    /// has the *sum* of the members' capacity and no redundancy — any one
    /// member fault loses the array. Carries a non-zero stripe unit
    /// (`ArraySuperblock::chunk_blocks`).
    Stripe = 2,
    /// RAID5 distributed parity (`ParityArray`): the logical block
    /// space is striped in fixed-size chunks across the members, and each
    /// stripe reserves one member's chunk for the parity (XOR) of the others,
    /// with the parity slot rotating across stripes so parity load is spread.
    /// The array has the capacity of `member_count - 1` members and survives
    /// any single member fault by reconstructing its data from the survivors.
    /// Carries a non-zero stripe unit (`ArraySuperblock::chunk_blocks`).
    Parity = 3,
    /// RAID6 double distributed parity (`DualParityArray`): the
    /// logical block space is striped in fixed-size chunks across the members
    /// like RAID5, but each stripe reserves *two* members' chunks — a P
    /// (bytewise XOR) syndrome and a Q (Reed-Solomon, GF(2^8)) syndrome — for
    /// the data of the others, both slots rotating across stripes. The array
    /// has the capacity of `member_count - 2` members and survives **any two**
    /// members being lost by solving the two syndromes for the two unknowns.
    /// Carries a non-zero stripe unit (`ArraySuperblock::chunk_blocks`).
    DualParity = 4,
    /// RAID-TP triple distributed parity (`TripleParityArray`): the logical
    /// block space is striped in fixed-size chunks across the members like
    /// RAID6, but each stripe reserves *three* members' chunks — a P (bytewise
    /// XOR) syndrome, a Q (Reed-Solomon `Σ gᵏ·Dₖ`) syndrome, and an R
    /// (`Σ g²ᵏ·Dₖ`) syndrome over GF(2^8) — for the data of the others, all
    /// three slots rotating across stripes. The array has the capacity of
    /// `member_count - 3` members and survives **any three** members being
    /// lost by solving the three syndromes for the three unknowns. Carries a
    /// non-zero stripe unit (`ArraySuperblock::chunk_blocks`).
    TripleParity = 5,
    /// RAID10 stripe of mirrors (`Raid10Array`): the members are paired into
    /// two-copy mirrors and the logical block space is striped in fixed-size
    /// chunks across the pairs (a stripe *of* mirrors), so the array has the
    /// capacity of half its members and survives any member fault — and
    /// several at once — as long as no mirror pair loses *both* copies. The
    /// member count is always even (two copies per pair) and at least four
    /// (two pairs), below which the layout is a plain mirror rather than a
    /// stripe of mirrors. Carries a non-zero stripe unit
    /// (`ArraySuperblock::chunk_blocks`).
    Raid10 = 6,
}

impl RaidLevel {
    /// The on-disk byte for this level.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// Whether this level round-robins a fixed stripe unit across its members
    /// (and so requires a non-zero `ArraySuperblock::chunk_blocks`); a level
    /// that stores a full copy per member (the mirror) does not.
    #[must_use]
    pub const fn is_striped(self) -> bool {
        matches!(
            self,
            Self::Stripe | Self::Parity | Self::DualParity | Self::TripleParity | Self::Raid10
        )
    }

    /// Whether this level stores enough redundancy to reconstruct a member's
    /// data from the others — so the array can degrade rather than fail, heal
    /// a latent media error from a good copy, and rebuild a returning member.
    ///
    /// Every level but the RAID0 stripe does: a mirror holds a full copy per
    /// member, the parity levels hold one, two, or three syndromes per stripe,
    /// and RAID10 mirrors within each stripe column. A stripe holds nothing
    /// spare, so it has nothing to scrub from, rebuild from, or hot-swap.
    ///
    /// This is the *single* definition of the redundancy question, beside
    /// [`is_striped`](Self::is_striped) and
    /// [`data_members`](Self::data_members): the composed-device dispatch
    /// refuses redundancy-only operations on a non-redundant array with it, and
    /// the maintenance scheduler asks it before driving any self-healing at
    /// all, so the two cannot disagree about which arrays can heal themselves.
    #[must_use]
    pub const fn is_redundant(self) -> bool {
        !matches!(self, Self::Stripe)
    }

    /// The fewest member slots this level's structure can be composed from.
    /// Below it the record does not describe the level it claims to be: a
    /// mirror or a stripe needs at least one member, single parity needs three
    /// (two data plus the parity chunk), double parity needs four (two data
    /// plus the P and Q chunks), and triple parity needs five (two data plus
    /// the P, Q, and R chunks). A RAID5 record claiming two members, or a
    /// RAID6 record claiming three, describes an array that cannot exist and
    /// is as malformed as a zero member count.
    ///
    /// This is the *single* definition of each level's minimum, consumed both
    /// by the on-disk `decode` boundary and by the composition engines'
    /// `assemble`, so the metadata layer and the composition layer cannot
    /// disagree on how small an array may be.
    #[must_use]
    pub const fn min_members(self) -> u16 {
        match self {
            Self::Mirror | Self::Stripe => 1,
            Self::Parity => 3,
            // RAID6 reserves P and Q; a RAID10 needs two two-copy pairs
            // (below four it is a plain mirror, not a stripe of mirrors).
            Self::DualParity | Self::Raid10 => 4,
            Self::TripleParity => 5,
        }
    }

    /// The most member slots this level can be composed from.
    ///
    /// Only the GF(2^8) parity levels have a real ceiling: their syndromes'
    /// coefficients stay distinct for at most 255 data members, so a RAID6
    /// array holds at most those 255 data members plus its two syndrome chunks
    /// (257 slots) and a RAID-TP array at most 255 plus its three (258 slots).
    /// Every other level is bounded only by the on-disk `u16` member-count
    /// field. The 255 figure is the one [`MAX_PARITY_DATA_MEMBERS`] names, so
    /// the ceiling is defined once and shared with the parity composition
    /// engines.
    #[must_use]
    pub const fn max_members(self) -> u16 {
        match self {
            Self::DualParity => MAX_PARITY_DATA_MEMBERS + 2,
            Self::TripleParity => MAX_PARITY_DATA_MEMBERS + 3,
            _ => u16::MAX,
        }
    }

    /// The number of member slots whose capacity the composed array *presents*
    /// — its "data members".
    ///
    /// A stripe concatenates every member (`member_count`); a mirror presents a
    /// single copy (`1`, independent of how many mirrored copies exist); single
    /// parity reserves one member's chunk for its parity and presents the rest
    /// (`member_count - 1`); double parity reserves two members' chunks for its
    /// P and Q syndromes and presents the rest (`member_count - 2`); triple
    /// parity reserves three members' chunks for its P, Q, and R syndromes and
    /// presents the rest (`member_count - 3`).
    ///
    /// This is the *single* definition of each level's usable width, so the
    /// composition engines' `assemble` and any capacity a serving process
    /// presents derive from one rule that lives beside
    /// [`min_members`](Self::min_members) / [`max_members`](Self::max_members)
    /// and cannot drift apart.
    ///
    /// Total and fail-closed: returns [`None`] when `member_count` is below the
    /// count at which the level has any data member at all (a parity level with
    /// too few members, or an empty stripe), so a caller that has not already
    /// validated the width via [`min_members`](Self::min_members) can never
    /// compute a nonsensical capacity or underflow.
    #[must_use]
    pub const fn data_members(self, member_count: u64) -> Option<u64> {
        match self {
            Self::Mirror => Some(1),
            Self::Stripe => {
                if member_count == 0 {
                    None
                } else {
                    Some(member_count)
                }
            }
            Self::Parity => {
                if member_count >= 2 {
                    Some(member_count - 1)
                } else {
                    None
                }
            }
            Self::DualParity => {
                if member_count >= 3 {
                    Some(member_count - 2)
                } else {
                    None
                }
            }
            Self::TripleParity => {
                if member_count >= 4 {
                    Some(member_count - 3)
                } else {
                    None
                }
            }
            // A stripe of two-copy mirrors presents half its members' worth
            // of capacity, and only an even member count pairs cleanly; an
            // odd count describes an array that cannot exist.
            Self::Raid10 => {
                if member_count >= 2 && member_count.is_multiple_of(2) {
                    Some(member_count / 2)
                } else {
                    None
                }
            }
        }
    }

    /// The logical block count the composed array presents, given each
    /// member's own block count and the array's member count.
    ///
    /// This is `per_member_blocks × `[`data_members`](Self::data_members), the
    /// one place the array's capacity is sized from its geometry. Fails closed
    /// to [`None`] when the width is below the level's structural floor (via
    /// [`data_members`](Self::data_members)) or when the product overflows
    /// `u64`, so a composed device can never wrap to a smaller array that would
    /// truncate addresses.
    #[must_use]
    pub const fn logical_block_count(
        self,
        per_member_blocks: u64,
        member_count: u64,
    ) -> Option<u64> {
        match self.data_members(member_count) {
            Some(data) => per_member_blocks.checked_mul(data),
            None => None,
        }
    }

    /// Whether an array of this level can serve its logical blocks with
    /// exactly the members `slots` reports present.
    ///
    /// This is the *metadata-layer* precondition an assembling process asks
    /// before it composes anything: given the reassembled slot table
    /// (`ArrayIdentity::fill_slots`), is the surviving set structurally capable
    /// of reconstructing every logical block, or would the array be serving
    /// data it cannot vouch for? An array that fails it is left unassembled
    /// rather than brought online short — a partial stripe or a twice-punctured
    /// RAID5 has holes no redundancy can fill, and publishing it would hand a
    /// filesystem a device that silently cannot read parts of itself.
    ///
    /// Each level's answer follows from its redundancy, and this is the single
    /// definition of it, beside [`is_redundant`](Self::is_redundant) and
    /// [`data_members`](Self::data_members): a stripe holds nothing spare, so
    /// it needs every member; a mirror needs any one copy; single, double, and
    /// triple parity tolerate one, two, and three missing members
    /// respectively; and RAID10 tolerates any number of losses as long as no
    /// mirrored pair loses both of its copies.
    ///
    /// It answers about the *slot table*, not about live hardware: a slot it
    /// counts present may still turn out unreachable when the composition
    /// engine probes it, and the engine's own `assemble` remains the authority
    /// on what the live devices can do. The two questions compose — this one
    /// decides whether composing is worth attempting at all.
    ///
    /// Total and fail-closed: a slot table whose width the level cannot be
    /// composed from at all (below [`min_members`](Self::min_members), or an
    /// odd RAID10 width) is not servable, so a caller that has not already
    /// validated the width can never conclude a nonsensical array is usable.
    #[must_use]
    pub fn can_serve(self, slots: &[SlotDisposition]) -> bool {
        if slots.len() < usize::from(self.min_members())
            || self.data_members(slots.len() as u64).is_none()
        {
            return false;
        }
        let present = |slot: &SlotDisposition| matches!(slot, SlotDisposition::Present { .. });
        let missing = slots.iter().filter(|slot| !present(slot)).count();
        match self {
            Self::Stripe => missing == 0,
            Self::Mirror => missing < slots.len(),
            Self::Parity => missing <= 1,
            Self::DualParity => missing <= 2,
            Self::TripleParity => missing <= 3,
            // Losses are only survivable while they fall in *different*
            // pairs: a column that lost both of its copies has no source for
            // its stripes, however healthy the other columns are.
            Self::Raid10 => slots
                .as_chunks::<2>()
                .0
                .iter()
                .all(|pair| pair.iter().any(present)),
        }
    }

    /// Decode an on-disk level byte, failing closed on an unknown value.
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] if `raw` names no known level.
    pub const fn from_u8(raw: u8) -> Result<Self, Errno> {
        match raw {
            1 => Ok(Self::Mirror),
            2 => Ok(Self::Stripe),
            3 => Ok(Self::Parity),
            4 => Ok(Self::DualParity),
            5 => Ok(Self::TripleParity),
            6 => Ok(Self::Raid10),
            _ => Err(Errno::OutOfRange),
        }
    }
}

/// What occupies one array slot after reassembly.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum SlotDisposition {
    /// No admitted candidate claims this slot: a missing copy.
    Missing,
    /// An admitted candidate fills this slot.
    Present {
        /// The caller's handle for the device in this slot.
        tag: usize,
        /// Whether the copy is current (`true`) or a stale rebuild target
        /// (`false`).
        in_sync: bool,
    },
}

/// The health of a composed array, ordered best → worst.
#[repr(u8)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ArrayHealth {
    /// Every member is in sync: full redundancy.
    Optimal = 0,
    /// At least one copy is serving, but redundancy is reduced — a member is
    /// faulted or absent (missing) and none is currently rebuilding. Data is
    /// safe on the survivors; a tool shows the array as at-risk.
    Degraded,
    /// At least one copy is serving and a member is being rebuilt.
    Recovering,
    /// No in-sync copy remains: the array cannot serve and fails closed.
    Failed,
}

impl ArrayHealth {
    /// Raw discriminant, as carried in a reported array record.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// Decode a discriminant, failing closed on an unknown value.
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] for any value outside the closed set: an array
    /// whose health cannot be read is never presented as a healthy one.
    pub const fn from_u8(raw: u8) -> Result<Self, Errno> {
        match raw {
            0 => Ok(Self::Optimal),
            1 => Ok(Self::Degraded),
            2 => Ok(Self::Recovering),
            3 => Ok(Self::Failed),
            _ => Err(Errno::OutOfRange),
        }
    }

    /// Whether the array can still serve I/O (any state but
    /// [`Failed`](Self::Failed)).
    #[must_use]
    pub const fn is_serving(self) -> bool {
        !matches!(self, Self::Failed)
    }

    /// The volume-availability this array health maps to, so a serving process
    /// can surface array health through the same `sysinfo` mount surface a leaf
    /// volume uses (`plans/FIX-IO.md` IO2/IO5) rather than a second vocabulary.
    #[must_use]
    pub const fn to_mount_availability(self) -> MountAvailability {
        match self {
            Self::Optimal => MountAvailability::Available,
            Self::Degraded => MountAvailability::Degraded,
            Self::Recovering => MountAvailability::Recovering,
            Self::Failed => MountAvailability::UnavailableLost,
        }
    }
}

/// The membership state of one mirror slot.
///
/// A member is only ever a read source while [`InSync`](Self::InSync). A
/// [`Faulted`](Self::Faulted) member has been dropped from the array (a
/// whole-device fault, or a failed write/repair) and no longer serves or
/// receives I/O until it is re-added, but it still *occupies its slot* (its
/// device is retained for a re-add). A [`Resyncing`](Self::Resyncing) member
/// is being rebuilt from an in-sync copy: it receives writes to its
/// already-synced region so it never falls behind, but is not yet a read
/// source. An [`Absent`](Self::Absent) slot holds no device at all.
///
/// The slot's device presence is exactly determined by this state: every
/// state but [`Absent`](Self::Absent) has a backing device, and
/// [`Absent`](Self::Absent) has none. The constructors and reconfiguration
/// operations are the only mutators and they preserve that invariant, so the
/// two can never drift.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum MemberState {
    /// A full, current copy. A read source and a write target.
    InSync,
    /// Dropped from the array after a whole-device fault or a failed write.
    /// Neither serves reads nor receives writes until re-added. Its device is
    /// retained in the slot so a re-add can re-probe it.
    Faulted,
    /// Being rebuilt from an in-sync copy. Receives writes to its
    /// already-synced region; becomes [`InSync`](Self::InSync) when the
    /// rebuild cursor reaches the end of the array.
    Resyncing,
    /// No device occupies this slot: a member the array is *defined* to have
    /// but which is currently missing — never inserted, or removed after a
    /// fault. Like a Linux md "removed" slot, an absent member reduces the
    /// array's redundancy, so the array reports [`ArrayHealth::Degraded`]
    /// while one is present. A spare can be installed into an absent slot,
    /// which then rebuilds it from a surviving copy.
    Absent,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raid_level_round_trips_and_fails_closed() {
        assert_eq!(
            RaidLevel::from_u8(RaidLevel::Mirror.as_u8()),
            Ok(RaidLevel::Mirror)
        );
        assert_eq!(
            RaidLevel::from_u8(RaidLevel::Stripe.as_u8()),
            Ok(RaidLevel::Stripe)
        );
        assert_eq!(
            RaidLevel::from_u8(RaidLevel::Parity.as_u8()),
            Ok(RaidLevel::Parity)
        );
        assert_eq!(
            RaidLevel::from_u8(RaidLevel::DualParity.as_u8()),
            Ok(RaidLevel::DualParity)
        );
        assert_eq!(
            RaidLevel::from_u8(RaidLevel::TripleParity.as_u8()),
            Ok(RaidLevel::TripleParity)
        );
        assert_eq!(
            RaidLevel::from_u8(RaidLevel::Raid10.as_u8()),
            Ok(RaidLevel::Raid10)
        );
        assert!(!RaidLevel::Mirror.is_striped());
        assert!(RaidLevel::Stripe.is_striped());
        assert!(RaidLevel::Parity.is_striped());
        assert!(RaidLevel::DualParity.is_striped());
        assert!(RaidLevel::TripleParity.is_striped());
        assert!(RaidLevel::Raid10.is_striped());
        for raw in [0u8, 7, 8, 255] {
            assert_eq!(RaidLevel::from_u8(raw), Err(Errno::OutOfRange));
        }
    }

    #[test]
    fn raid_level_member_bounds_are_the_shared_source() {
        assert_eq!(RaidLevel::Mirror.min_members(), 1);
        assert_eq!(RaidLevel::Stripe.min_members(), 1);
        assert_eq!(RaidLevel::Parity.min_members(), 3);
        assert_eq!(RaidLevel::DualParity.min_members(), 4);
        assert_eq!(RaidLevel::TripleParity.min_members(), 5);
        // RAID10 needs two two-copy pairs (four members) to be a stripe of
        // mirrors rather than a plain mirror.
        assert_eq!(RaidLevel::Raid10.min_members(), 4);
        // Only the GF(2^8) parity levels have a real ceiling: 255 data members
        // plus their syndrome chunks (RAID6 = 257 slots, RAID-TP = 258). Every
        // other level is bounded only by the on-disk `u16` member-count field.
        assert_eq!(RaidLevel::Mirror.max_members(), u16::MAX);
        assert_eq!(RaidLevel::Stripe.max_members(), u16::MAX);
        assert_eq!(RaidLevel::Parity.max_members(), u16::MAX);
        assert_eq!(RaidLevel::DualParity.max_members(), 257);
        assert_eq!(RaidLevel::TripleParity.max_members(), 258);
        assert_eq!(RaidLevel::Raid10.max_members(), u16::MAX);
    }

    #[test]
    fn is_redundant_is_the_shared_answer_for_every_level() {
        // Only the RAID0 stripe holds nothing spare, so only it has nothing to
        // scrub from, rebuild from, or hot-swap.
        assert!(!RaidLevel::Stripe.is_redundant());
        assert!(RaidLevel::Mirror.is_redundant());
        assert!(RaidLevel::Parity.is_redundant());
        assert!(RaidLevel::DualParity.is_redundant());
        assert!(RaidLevel::TripleParity.is_redundant());
        assert!(RaidLevel::Raid10.is_redundant());
    }

    #[test]
    fn data_members_is_the_shared_usable_width_per_level() {
        // A mirror presents one copy's worth regardless of how many copies exist.
        assert_eq!(RaidLevel::Mirror.data_members(1), Some(1));
        assert_eq!(RaidLevel::Mirror.data_members(4), Some(1));
        // A stripe concatenates every member.
        assert_eq!(RaidLevel::Stripe.data_members(1), Some(1));
        assert_eq!(RaidLevel::Stripe.data_members(6), Some(6));
        // Single parity reserves one member's chunk for parity.
        assert_eq!(RaidLevel::Parity.data_members(3), Some(2));
        assert_eq!(RaidLevel::Parity.data_members(8), Some(7));
        // Double parity reserves two (P and Q).
        assert_eq!(RaidLevel::DualParity.data_members(4), Some(2));
        assert_eq!(RaidLevel::DualParity.data_members(10), Some(8));
        // Triple parity reserves three (P, Q, and R).
        assert_eq!(RaidLevel::TripleParity.data_members(5), Some(2));
        assert_eq!(RaidLevel::TripleParity.data_members(10), Some(7));
        // A RAID10 stripe of two-copy mirrors presents half its members.
        assert_eq!(RaidLevel::Raid10.data_members(4), Some(2));
        assert_eq!(RaidLevel::Raid10.data_members(10), Some(5));
    }

    #[test]
    fn data_members_fails_closed_below_the_structural_floor() {
        // A width with no data member at all yields `None` rather than underflow:
        // an empty stripe, and parity levels below the count that leaves any data.
        assert_eq!(RaidLevel::Stripe.data_members(0), None);
        assert_eq!(RaidLevel::Parity.data_members(1), None);
        assert_eq!(RaidLevel::Parity.data_members(0), None);
        assert_eq!(RaidLevel::DualParity.data_members(2), None);
        assert_eq!(RaidLevel::DualParity.data_members(0), None);
        assert_eq!(RaidLevel::TripleParity.data_members(3), None);
        assert_eq!(RaidLevel::TripleParity.data_members(0), None);
        // A RAID10 with an odd member count cannot pair its copies.
        assert_eq!(RaidLevel::Raid10.data_members(5), None);
        assert_eq!(RaidLevel::Raid10.data_members(0), None);
        // The mirror is the identity case: always one copy's worth, even at zero
        // (an empty mirror is rejected earlier by `assemble`, not here).
        assert_eq!(RaidLevel::Mirror.data_members(0), Some(1));
    }

    #[test]
    fn logical_block_count_is_per_member_times_data_members() {
        // Capacity is each member's block count times the usable width.
        assert_eq!(RaidLevel::Mirror.logical_block_count(1000, 3), Some(1000));
        assert_eq!(RaidLevel::Stripe.logical_block_count(1000, 3), Some(3000));
        assert_eq!(RaidLevel::Parity.logical_block_count(1000, 4), Some(3000));
        assert_eq!(
            RaidLevel::DualParity.logical_block_count(1000, 5),
            Some(3000)
        );
    }

    #[test]
    fn logical_block_count_fails_closed_on_overflow_and_underwidth() {
        // A product that would overflow `u64` fails closed rather than wrapping to
        // a smaller array that would truncate addresses.
        assert_eq!(RaidLevel::Stripe.logical_block_count(u64::MAX, 2), None);
        assert_eq!(
            RaidLevel::Parity.logical_block_count(u64::MAX, 3),
            None,
            "u64::MAX * 2 overflows"
        );
        // Below the structural floor there is no data member to multiply by.
        assert_eq!(RaidLevel::DualParity.logical_block_count(1000, 2), None);
        assert_eq!(RaidLevel::Stripe.logical_block_count(1000, 0), None);
    }

    /// A slot table whose entries follow `present`: `true` fills the slot with a
    /// current member, `false` leaves it missing.
    fn slots_from<const N: usize>(present: [bool; N]) -> [SlotDisposition; N] {
        let mut slots = [SlotDisposition::Missing; N];
        for (tag, slot) in slots.iter_mut().enumerate() {
            if present[tag] {
                *slot = SlotDisposition::Present { tag, in_sync: true };
            }
        }
        slots
    }

    #[test]
    fn a_stripe_can_serve_only_with_every_member_present() {
        // No redundancy: a gap is a hole in the logical block space, so the array
        // is left unassembled rather than serving reads it cannot answer.
        assert!(RaidLevel::Stripe.can_serve(&slots_from([true, true, true])));
        assert!(!RaidLevel::Stripe.can_serve(&slots_from([true, false, true])));
    }

    #[test]
    fn a_mirror_can_serve_from_any_single_surviving_copy() {
        assert!(RaidLevel::Mirror.can_serve(&slots_from([false, false, true])));
        assert!(!RaidLevel::Mirror.can_serve(&slots_from([false, false, false])));
    }

    #[test]
    fn each_parity_level_tolerates_exactly_its_syndrome_count_of_losses() {
        // Single parity reconstructs one lost member and no more.
        assert!(RaidLevel::Parity.can_serve(&slots_from([true, false, true, true])));
        assert!(!RaidLevel::Parity.can_serve(&slots_from([true, false, false, true])));
        // Double parity survives two losses, not three.
        assert!(RaidLevel::DualParity.can_serve(&slots_from([false, true, true, false, true])));
        assert!(!RaidLevel::DualParity.can_serve(&slots_from([false, true, false, false, true])));
        // Triple parity survives three losses, not four.
        assert!(RaidLevel::TripleParity.can_serve(&slots_from([false, false, true, false, true])));
        assert!(!RaidLevel::TripleParity.can_serve(&slots_from([false, false, true, false, false])));
    }

    #[test]
    fn a_raid10_survives_losses_in_distinct_pairs_but_not_a_lost_pair() {
        // One copy gone from each of the three columns: every stripe still has a
        // source, so the array serves.
        assert!(RaidLevel::Raid10.can_serve(&slots_from([true, false, false, true, true, false])));
        // Both copies of the middle column gone: its stripes have no source,
        // however healthy the other columns are.
        assert!(!RaidLevel::Raid10.can_serve(&slots_from([true, true, false, false, true, true])));
    }

    #[test]
    fn can_serve_fails_closed_on_a_width_the_level_cannot_be_composed_from() {
        // An empty table describes no array at all.
        assert!(!RaidLevel::Mirror.can_serve(&[]));
        // Below the level's structural floor: RAID5 needs three members, RAID6
        // four, RAID-TP five.
        assert!(!RaidLevel::Parity.can_serve(&slots_from([true, true])));
        assert!(!RaidLevel::DualParity.can_serve(&slots_from([true, true, true])));
        assert!(!RaidLevel::TripleParity.can_serve(&slots_from([true, true, true, true])));
        // An odd RAID10 width pairs no column cleanly, so it is not an array this
        // level can serve however many members are present.
        assert!(!RaidLevel::Raid10.can_serve(&slots_from([true, true, true, true, true])));
    }

    #[test]
    fn array_health_serving_and_availability_mapping() {
        assert!(ArrayHealth::Optimal.is_serving());
        assert!(ArrayHealth::Degraded.is_serving());
        assert!(ArrayHealth::Recovering.is_serving());
        assert!(!ArrayHealth::Failed.is_serving());
        assert_eq!(
            ArrayHealth::Optimal.to_mount_availability(),
            MountAvailability::Available
        );
        assert_eq!(
            ArrayHealth::Degraded.to_mount_availability(),
            MountAvailability::Degraded
        );
        assert_eq!(
            ArrayHealth::Recovering.to_mount_availability(),
            MountAvailability::Recovering
        );
        assert_eq!(
            ArrayHealth::Failed.to_mount_availability(),
            MountAvailability::UnavailableLost
        );
    }
}
