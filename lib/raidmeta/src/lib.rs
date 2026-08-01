//! The RAID **array superblock**: the on-disk metadata every member of an
//! array carries, and the fail-closed logic that reassembles an array from a
//! set of discovered members (`plans/FIX-IO.md` IO6).
//!
//! A RAID array is not a hand-maintained list of devices: the assembling
//! serve process discovers block devices (`AGENTS.md` §18) and reads a
//! superblock from each. The superblock is what lets a member identify the
//! array it belongs to, its slot in that array, the array geometry, and — via
//! a monotonic **generation** counter — how current its copy is relative to
//! its siblings. From those the assembler reconstructs the array with no
//! external configuration, and knows which members hold a current copy and
//! which are behind and must be rebuilt (`plans/FIX-IO.md` IO3 recovery /
//! IO6 resync).
//!
//! # On-disk shape
//!
//! [`ArraySuperblock`] is a fixed-size, little-endian record ([`WIRE_LEN`]
//! bytes) protected by a trailing CRC-32C (`tairix_crc32c`) computed over
//! everything before it. Every wire field is bounds- and shape-checked on
//! decode and the record **fails closed** (`AGENTS.md` §5.4): a bad magic,
//! an unknown version, a checksum mismatch, an unknown RAID level, a
//! zero member count, a slot outside the array, or a degenerate geometry is
//! a typed [`SuperblockError`], never a silently-trusted record. The CRC is a
//! media/transport integrity check, not a security control: an array's
//! authenticity rests on the signed driver bundle and the members' own
//! capability-gated block endpoints, not on this value.
//!
//! The record is stored at a fixed offset within a reserved metadata block on
//! each member; only the leading [`WIRE_LEN`] bytes are the superblock, so a
//! caller may place it in a larger (block-sized) buffer and zero the rest.
//!
//! # Reassembly
//!
//! Discovery hands the assembler a heterogeneous set of members that need not
//! all belong to the same array. [`distinct_arrays`] enumerates the distinct
//! [`ArrayUuid`]s present among them — the "which arrays are on these disks"
//! step — so the assembler resolves each array in turn.
//!
//! [`ArrayIdentity::resolve`] establishes the authoritative array identity
//! from a set of [`Candidate`] members: the member reporting the **highest
//! generation** is freshest, so it fixes the array's level, member count,
//! geometry, and current generation. [`ArrayIdentity::fill_slots`] then places
//! each member into its slot, marking a member whose generation matches the
//! authoritative one as in sync and a member that is behind as **stale** (a
//! rebuild target), while [`ArrayIdentity::verdict_of`] classifies any member
//! the array cannot safely admit (a foreign array, a member disagreeing on the
//! array shape, an out-of-range slot, or a duplicate claim on a slot). Both
//! read the same [`ArrayIdentity::verdict_of`] decision, so the slot table and
//! the per-member verdict can never disagree (`AGENTS.md` §2.2). The whole
//! layer is pure and allocation-free — it borrows the caller's member slice
//! and fills a caller-owned slot buffer, so it imposes no fixed member ceiling
//! (`AGENTS.md` §24.1); the growable member tier lives in the assembling serve
//! process.
//!
//! # Metadata updates (membership changes)
//!
//! Reassembly reads the generation counter; the write side *advances* it.
//! When the array's membership changes — a member drops out on a fault, or a
//! rebuilt member rejoins — the serve process calls
//! [`ArrayIdentity::bump_generation`] and re-stamps every **current** member's
//! superblock with [`ArrayIdentity::member_superblock`] at the new generation.
//! A member that was absent for that bump keeps its lower generation, so on
//! return it resolves as a stale rebuild target rather than being trusted as
//! current: this is what closes the stale-read window (`AGENTS.md` §5.4,
//! §26.5 — a disk that missed writes is a disk that can lie). Promoting a
//! rebuilt member back to current is the same `member_superblock` write, so
//! the read and write halves share one notion of "current" and cannot diverge
//! (`AGENTS.md` §2.2).

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

use tairix_abi::driver::block::BlockGeometry;
use tairix_abi::time::Time64;

/// The largest data-member count a GF(2^8) parity array (RAID6 double parity
/// or RAID-TP triple parity) can have.
///
/// A GF(2^8) parity syndrome weights data member `k` by `gᵏ` (and the higher
/// syndrome by `g²ᵏ`), whose exponents `g⁰ … g²⁵⁴` are the 255 distinct
/// non-zero field elements (`g = {02}`, polynomial `0x11d` — the Linux-md
/// field). Beyond 255 data members those coefficients would repeat and the
/// erasures could no longer be solved, so a GF-parity array admits at most
/// this many data members. This is the single definition of that structural
/// ceiling: [`RaidLevel::max_members`] and the parity composition engines'
/// fields all derive from it (`AGENTS.md` §2.2).
pub const MAX_PARITY_DATA_MEMBERS: u16 = 255;

/// A 128-bit array identifier, minted once when the array is created and
/// identical on every member. Two members belong to the same array iff their
/// identifiers are equal.
pub type ArrayUuid = [u8; 16];

/// The composition a superblock describes. Encoded as one on-disk byte;
/// decoding an unrecognised value fails closed
/// ([`SuperblockError::UnknownRaidLevel`]) so a member written by a future or
/// foreign format is never mistaken for a mirror.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum RaidLevel {
    /// RAID1 mirror (`MirrorArray`): every member is a full copy, so
    /// the array survives any subset of member faults while one copy remains.
    /// Carries no stripe unit ([`ArraySuperblock::chunk_blocks`] is `0`).
    Mirror = 1,
    /// RAID0 stripe (`StripeArray`): the logical block space is cut
    /// into fixed-size chunks round-robined across the members, so the array
    /// has the *sum* of the members' capacity and no redundancy — any one
    /// member fault loses the array. Carries a non-zero stripe unit
    /// ([`ArraySuperblock::chunk_blocks`]).
    Stripe = 2,
    /// RAID5 distributed parity (`ParityArray`): the logical block
    /// space is striped in fixed-size chunks across the members, and each
    /// stripe reserves one member's chunk for the parity (XOR) of the others,
    /// with the parity slot rotating across stripes so parity load is spread.
    /// The array has the capacity of `member_count - 1` members and survives
    /// any single member fault by reconstructing its data from the survivors.
    /// Carries a non-zero stripe unit ([`ArraySuperblock::chunk_blocks`]).
    Parity = 3,
    /// RAID6 double distributed parity (`DualParityArray`): the
    /// logical block space is striped in fixed-size chunks across the members
    /// like RAID5, but each stripe reserves *two* members' chunks — a P
    /// (bytewise XOR) syndrome and a Q (Reed-Solomon, GF(2^8)) syndrome — for
    /// the data of the others, both slots rotating across stripes. The array
    /// has the capacity of `member_count - 2` members and survives **any two**
    /// members being lost by solving the two syndromes for the two unknowns.
    /// Carries a non-zero stripe unit ([`ArraySuperblock::chunk_blocks`]).
    DualParity = 4,
    /// RAID-TP triple distributed parity (`TripleParityArray`): the logical
    /// block space is striped in fixed-size chunks across the members like
    /// RAID6, but each stripe reserves *three* members' chunks — a P (bytewise
    /// XOR) syndrome, a Q (Reed-Solomon `Σ gᵏ·Dₖ`) syndrome, and an R
    /// (`Σ g²ᵏ·Dₖ`) syndrome over GF(2^8) — for the data of the others, all
    /// three slots rotating across stripes. The array has the capacity of
    /// `member_count - 3` members and survives **any three** members being
    /// lost by solving the three syndromes for the three unknowns. Carries a
    /// non-zero stripe unit ([`ArraySuperblock::chunk_blocks`]).
    TripleParity = 5,
    /// RAID10 stripe of mirrors (`Raid10Array`): the members are paired into
    /// two-copy mirrors and the logical block space is striped in fixed-size
    /// chunks across the pairs (a stripe *of* mirrors), so the array has the
    /// capacity of half its members and survives any member fault — and
    /// several at once — as long as no mirror pair loses *both* copies. The
    /// member count is always even (two copies per pair) and at least four
    /// (two pairs), below which the layout is a plain mirror rather than a
    /// stripe of mirrors. Carries a non-zero stripe unit
    /// ([`ArraySuperblock::chunk_blocks`]).
    Raid10 = 6,
}

impl RaidLevel {
    /// The on-disk byte for this level.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// Whether this level round-robins a fixed stripe unit across its members
    /// (and so requires a non-zero [`ArraySuperblock::chunk_blocks`]); a level
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
    /// [`is_striped`](Self::is_striped) and [`data_members`](Self::data_members):
    /// the composed-device dispatch refuses redundancy-only operations on a
    /// non-redundant array with it, and the maintenance scheduler asks it
    /// before driving any self-healing at all, so the two cannot disagree
    /// about which arrays can heal themselves (`AGENTS.md` §2.2).
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
    /// by the on-disk [`decode`](ArraySuperblock::decode) boundary and by the
    /// composition engines' `assemble`, so the metadata layer and the
    /// composition layer cannot disagree on how small an array may be.
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
    /// engines (`AGENTS.md` §2.2).
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
    /// one place the array's capacity is sized from its geometry (`AGENTS.md`
    /// §2.2). Fails closed to [`None`] when the width is below the level's
    /// structural floor (via [`data_members`](Self::data_members)) or when the
    /// product overflows `u64`, so a composed device can never wrap to a
    /// smaller array that would truncate addresses (`AGENTS.md` §5.4).
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

    /// Decode an on-disk level byte, failing closed on an unknown value.
    ///
    /// # Errors
    ///
    /// [`SuperblockError::UnknownRaidLevel`] if `raw` names no known level.
    pub const fn from_u8(raw: u8) -> Result<Self, SuperblockError> {
        match raw {
            1 => Ok(Self::Mirror),
            2 => Ok(Self::Stripe),
            3 => Ok(Self::Parity),
            4 => Ok(Self::DualParity),
            5 => Ok(Self::TripleParity),
            6 => Ok(Self::Raid10),
            _ => Err(SuperblockError::UnknownRaidLevel),
        }
    }
}

/// A reason an [`ArraySuperblock`] could not be decoded. Every variant is a
/// fail-closed refusal: a superblock that does not decode cleanly is never
/// trusted.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum SuperblockError {
    /// The input was shorter than [`WIRE_LEN`].
    TooSmall,
    /// The leading magic did not match [`MAGIC`]: not a TAIRiX RAID superblock.
    BadMagic,
    /// The version field named a format this build does not understand.
    UnsupportedVersion,
    /// The stored CRC-32C did not match the recomputed one: the record is
    /// corrupt.
    BadChecksum,
    /// The RAID level byte named no known composition.
    UnknownRaidLevel,
    /// The member count was zero; an array has at least one member.
    ZeroMembers,
    /// The member slot was not less than the member count.
    SlotOutOfRange,
    /// The array geometry was degenerate (a zero block size or zero block
    /// count): no usable array.
    ZeroGeometry,
    /// The stored timestamp was not a canonical [`Time64`].
    BadTimestamp,
    /// The stripe unit was inconsistent with the RAID level: a striped level
    /// (RAID0) recorded a zero stripe unit, or a non-striped level (the
    /// mirror) recorded a non-zero one. The two must agree, so a record whose
    /// level and stripe unit contradict each other is never trusted.
    BadStripeChunk,
    /// The member count was non-zero but outside the range its RAID level can
    /// be composed from ([`RaidLevel::min_members`]/[`RaidLevel::max_members`]):
    /// too few members for the level's structure (e.g. a RAID5 claiming two
    /// members or a RAID6 claiming three), or — for RAID6 — more data members
    /// than its Q syndrome can distinguish. Such a record describes an array
    /// that cannot exist and is refused rather than half-trusted.
    MemberCountOutOfRange,
}

/// The 8-byte magic that opens every array superblock (`"TXRAIDSB"`).
pub const MAGIC: [u8; 8] = *b"TXRAIDSB";

/// The only superblock format version this build reads or writes. The on-disk
/// format is unfrozen pre-release (`AGENTS.md` §2.13): it is changed in place,
/// never versioned alongside an old one.
pub const FORMAT_VERSION: u16 = 1;

// Field offsets within the fixed little-endian record. Laid out with no
// padding: every field is read from its explicit offset with `from_le_bytes`,
// never a struct cast, so alignment is irrelevant.
const OFF_MAGIC: usize = 0; // [u8; 8]
const OFF_VERSION: usize = 8; // u16
const OFF_LEVEL: usize = 10; // u8
const OFF_MEMBER_COUNT: usize = 11; // u16
const OFF_MEMBER_SLOT: usize = 13; // u16
const OFF_UUID: usize = 15; // [u8; 16]
const OFF_BLOCK_SIZE: usize = 31; // u32
const OFF_BLOCK_COUNT: usize = 35; // u64
const OFF_GENERATION: usize = 43; // u64
const OFF_UPDATED_AT: usize = 51; // Time64 (WIRE_LEN = 12)
const OFF_CHUNK_BLOCKS: usize = OFF_UPDATED_AT + Time64::WIRE_LEN; // u32
const OFF_CHECKSUM: usize = OFF_CHUNK_BLOCKS + 4; // u32

/// The encoded size of an [`ArraySuperblock`] in bytes. The CRC-32C covers the
/// first `WIRE_LEN - 4` bytes; the trailing four bytes are the checksum.
pub const WIRE_LEN: usize = OFF_CHECKSUM + 4;

/// The on-disk metadata one member of a RAID array carries: the array's
/// identity and shape plus this member's role and freshness within it.
///
/// See the [crate documentation](crate) for the on-disk layout, the
/// fail-closed decode contract, and how the generation counter drives
/// reassembly.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct ArraySuperblock {
    /// The array this member belongs to.
    pub array_uuid: ArrayUuid,
    /// The composition the array uses.
    pub raid_level: RaidLevel,
    /// The total number of member slots in the array.
    pub member_count: u16,
    /// This member's slot index, in `0..member_count`.
    pub member_slot: u16,
    /// The array's logical geometry, identical on every member.
    pub geometry: BlockGeometry,
    /// The array generation this member last participated in. A member that
    /// missed a membership change (it was faulted while the survivors advanced
    /// the array) carries a lower generation and is a rebuild target.
    pub generation: u64,
    /// Wall-clock instant this superblock was last written (`AGENTS.md` §21 —
    /// stored as a full [`Time64`], never a 32-bit second).
    pub updated_at: Time64,
    /// The stripe unit in logical blocks for a striped level: the number of
    /// consecutive logical blocks placed on one member before the stripe moves
    /// to the next. It is `0` for a level that stores a full copy per member
    /// (the mirror) and non-zero for a striped level (RAID0); [`decode`] fails
    /// closed if the level and this field disagree. It is array policy, not a
    /// fixed constant (`AGENTS.md` §24.1), so different arrays can be tuned to
    /// their workload.
    ///
    /// [`decode`]: ArraySuperblock::decode
    pub chunk_blocks: u32,
}

impl ArraySuperblock {
    /// Encode `self` into its fixed-size little-endian on-disk form, sealing it
    /// with a trailing CRC-32C over the preceding bytes.
    #[must_use]
    pub fn encode(&self) -> [u8; WIRE_LEN] {
        let mut out = [0u8; WIRE_LEN];
        out[OFF_MAGIC..OFF_MAGIC + 8].copy_from_slice(&MAGIC);
        out[OFF_VERSION..OFF_VERSION + 2].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
        out[OFF_LEVEL] = self.raid_level.as_u8();
        out[OFF_MEMBER_COUNT..OFF_MEMBER_COUNT + 2]
            .copy_from_slice(&self.member_count.to_le_bytes());
        out[OFF_MEMBER_SLOT..OFF_MEMBER_SLOT + 2].copy_from_slice(&self.member_slot.to_le_bytes());
        out[OFF_UUID..OFF_UUID + 16].copy_from_slice(&self.array_uuid);
        out[OFF_BLOCK_SIZE..OFF_BLOCK_SIZE + 4]
            .copy_from_slice(&self.geometry.block_size.to_le_bytes());
        out[OFF_BLOCK_COUNT..OFF_BLOCK_COUNT + 8]
            .copy_from_slice(&self.geometry.block_count.to_le_bytes());
        out[OFF_GENERATION..OFF_GENERATION + 8].copy_from_slice(&self.generation.to_le_bytes());
        out[OFF_UPDATED_AT..OFF_UPDATED_AT + Time64::WIRE_LEN]
            .copy_from_slice(&self.updated_at.to_le_bytes());
        out[OFF_CHUNK_BLOCKS..OFF_CHUNK_BLOCKS + 4]
            .copy_from_slice(&self.chunk_blocks.to_le_bytes());
        let crc = tairix_crc32c::checksum(&out[..OFF_CHECKSUM]);
        out[OFF_CHECKSUM..OFF_CHECKSUM + 4].copy_from_slice(&crc.to_le_bytes());
        out
    }

    /// Decode an array superblock from the leading [`WIRE_LEN`] bytes of
    /// `bytes`, validating every field and failing closed on the first fault.
    ///
    /// # Errors
    ///
    /// A [`SuperblockError`] for any of: a short input, a bad magic, an
    /// unknown version, a checksum mismatch, an unknown RAID level, a zero
    /// member count, a member count the level cannot be composed from, a slot
    /// outside the array, a degenerate geometry, a non-canonical timestamp, or
    /// a stripe unit inconsistent with the level.
    pub fn decode(bytes: &[u8]) -> Result<Self, SuperblockError> {
        if bytes.len() < WIRE_LEN {
            return Err(SuperblockError::TooSmall);
        }
        if bytes[OFF_MAGIC..OFF_MAGIC + 8] != MAGIC {
            return Err(SuperblockError::BadMagic);
        }
        let version = u16::from_le_bytes([bytes[OFF_VERSION], bytes[OFF_VERSION + 1]]);
        if version != FORMAT_VERSION {
            return Err(SuperblockError::UnsupportedVersion);
        }
        let stored_crc = u32::from_le_bytes([
            bytes[OFF_CHECKSUM],
            bytes[OFF_CHECKSUM + 1],
            bytes[OFF_CHECKSUM + 2],
            bytes[OFF_CHECKSUM + 3],
        ]);
        if tairix_crc32c::checksum(&bytes[..OFF_CHECKSUM]) != stored_crc {
            return Err(SuperblockError::BadChecksum);
        }
        let raid_level = RaidLevel::from_u8(bytes[OFF_LEVEL])?;
        let member_count =
            u16::from_le_bytes([bytes[OFF_MEMBER_COUNT], bytes[OFF_MEMBER_COUNT + 1]]);
        if member_count == 0 {
            return Err(SuperblockError::ZeroMembers);
        }
        // The member count must be one the claimed level can actually be
        // composed from: a RAID5 with two members or a RAID6 with three
        // describes an array that cannot exist, and a RAID6 with more data
        // members than its Q syndrome distinguishes is equally unbuildable.
        // Reject it at the boundary rather than let it reach an engine that
        // would fail closed later (fail closed on malformed metadata).
        if member_count < raid_level.min_members() || member_count > raid_level.max_members() {
            return Err(SuperblockError::MemberCountOutOfRange);
        }
        // A member count within the level's [min, max] range can still be
        // structurally impossible — a RAID10 needs an *even* count to pair its
        // copies. `data_members` is the single oracle of composability, so the
        // on-disk boundary rejects exactly what an engine's `assemble` would
        // (fail closed on malformed metadata; no drift, `AGENTS.md` §2.2).
        if raid_level.data_members(u64::from(member_count)).is_none() {
            return Err(SuperblockError::MemberCountOutOfRange);
        }
        let member_slot = u16::from_le_bytes([bytes[OFF_MEMBER_SLOT], bytes[OFF_MEMBER_SLOT + 1]]);
        if member_slot >= member_count {
            return Err(SuperblockError::SlotOutOfRange);
        }
        let mut array_uuid = [0u8; 16];
        array_uuid.copy_from_slice(&bytes[OFF_UUID..OFF_UUID + 16]);
        let block_size = u32::from_le_bytes([
            bytes[OFF_BLOCK_SIZE],
            bytes[OFF_BLOCK_SIZE + 1],
            bytes[OFF_BLOCK_SIZE + 2],
            bytes[OFF_BLOCK_SIZE + 3],
        ]);
        let block_count = u64::from_le_bytes([
            bytes[OFF_BLOCK_COUNT],
            bytes[OFF_BLOCK_COUNT + 1],
            bytes[OFF_BLOCK_COUNT + 2],
            bytes[OFF_BLOCK_COUNT + 3],
            bytes[OFF_BLOCK_COUNT + 4],
            bytes[OFF_BLOCK_COUNT + 5],
            bytes[OFF_BLOCK_COUNT + 6],
            bytes[OFF_BLOCK_COUNT + 7],
        ]);
        if block_size == 0 || block_count == 0 {
            return Err(SuperblockError::ZeroGeometry);
        }
        let generation = u64::from_le_bytes([
            bytes[OFF_GENERATION],
            bytes[OFF_GENERATION + 1],
            bytes[OFF_GENERATION + 2],
            bytes[OFF_GENERATION + 3],
            bytes[OFF_GENERATION + 4],
            bytes[OFF_GENERATION + 5],
            bytes[OFF_GENERATION + 6],
            bytes[OFF_GENERATION + 7],
        ]);
        let updated_at =
            Time64::from_bytes(&bytes[OFF_UPDATED_AT..OFF_UPDATED_AT + Time64::WIRE_LEN])
                .map_err(|_| SuperblockError::BadTimestamp)?;
        let chunk_blocks = u32::from_le_bytes([
            bytes[OFF_CHUNK_BLOCKS],
            bytes[OFF_CHUNK_BLOCKS + 1],
            bytes[OFF_CHUNK_BLOCKS + 2],
            bytes[OFF_CHUNK_BLOCKS + 3],
        ]);
        // The stripe unit and the level must agree: a striped level needs a
        // non-zero unit, a full-copy level must not carry one. A record whose
        // level and stripe unit contradict is corrupt or foreign — fail closed.
        if raid_level.is_striped() == (chunk_blocks == 0) {
            return Err(SuperblockError::BadStripeChunk);
        }
        Ok(Self {
            array_uuid,
            raid_level,
            member_count,
            member_slot,
            geometry: BlockGeometry {
                block_size,
                block_count,
            },
            generation,
            updated_at,
            chunk_blocks,
        })
    }
}

/// A discovered member offered to reassembly: the caller's opaque handle for
/// the device (typically an index into its own discovered-device list) and the
/// superblock decoded from it.
///
/// A device with no decodable superblock is simply not offered — the caller
/// skips it — so every `Candidate` carries a superblock that already passed
/// [`ArraySuperblock::decode`].
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Candidate {
    /// The caller's handle for this device, returned verbatim in a
    /// [`SlotDisposition::Present`] so the caller can map a slot back to its
    /// device.
    pub tag: usize,
    /// The superblock read from this device.
    pub superblock: ArraySuperblock,
}

/// Why a candidate cannot be placed in the array it claims membership of.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum RejectReason {
    /// The candidate belongs to a different array (its UUID is not the target).
    WrongArray,
    /// The candidate belongs to the target array but disagrees with the
    /// authoritative member on the array shape (level, member count, or
    /// geometry): it is corrupt or was reshaped, and is not admitted.
    Mismatched,
    /// The candidate's slot is outside the authoritative member count.
    BadSlot,
    /// Another candidate holds the same slot with a fresher (or, on a tie, a
    /// lower-tagged) copy; this one is a stale or duplicate claimant.
    Duplicate,
}

/// The verdict on one candidate against a resolved [`ArrayIdentity`].
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum CandidateVerdict {
    /// The candidate is admitted into `slot`; `in_sync` is `true` when its
    /// generation matches the authoritative one and `false` when it is behind
    /// and must be rebuilt before it serves reads.
    Placed {
        /// The array slot this candidate fills.
        slot: u16,
        /// Whether the copy is current (`true`) or a stale rebuild target
        /// (`false`).
        in_sync: bool,
    },
    /// The candidate is refused; see [`RejectReason`].
    Rejected(RejectReason),
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

/// The authoritative identity and shape of an array, resolved from the freshest
/// member of a candidate set. Borrows nothing; the candidate slice is passed
/// back in to [`fill_slots`](Self::fill_slots) / [`verdict_of`](Self::verdict_of).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct ArrayIdentity {
    /// The array's identifier (the resolution target).
    pub array_uuid: ArrayUuid,
    /// The array composition.
    pub raid_level: RaidLevel,
    /// The number of member slots.
    pub member_count: u16,
    /// The logical geometry every member shares.
    pub geometry: BlockGeometry,
    /// The freshest generation present among the candidates: the authoritative
    /// "current" generation. A candidate below it is a stale rebuild target.
    pub generation: u64,
    /// The stripe unit in logical blocks (`0` for a full-copy level, non-zero
    /// for a striped one). Part of the array shape: a member disagreeing on it
    /// is refused, so every admitted member stripes identically.
    pub chunk_blocks: u32,
}

/// A reason an array could not be resolved from a candidate set.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum AssemblyError {
    /// No candidate claimed membership of the target array, so there is
    /// nothing to assemble. Fails closed rather than inventing an empty array.
    NoMembers,
}

impl ArrayIdentity {
    /// Resolve the authoritative identity of the array `target_uuid` from
    /// `candidates`.
    ///
    /// The freshest member — the candidate for `target_uuid` reporting the
    /// highest [`generation`](ArraySuperblock::generation), breaking a tie by
    /// the lowest slot then the lowest tag so resolution is deterministic —
    /// fixes the array's level, member count, geometry, and current
    /// generation. Members disagreeing with that shape are not authoritative;
    /// they are refused at placement time ([`verdict_of`](Self::verdict_of)),
    /// never allowed to redefine the array.
    ///
    /// # Errors
    ///
    /// [`AssemblyError::NoMembers`] if no candidate belongs to `target_uuid`.
    pub fn resolve(
        target_uuid: ArrayUuid,
        candidates: &[Candidate],
    ) -> Result<Self, AssemblyError> {
        let mut best: Option<&ArraySuperblock> = None;
        for candidate in candidates {
            let sb = &candidate.superblock;
            if sb.array_uuid != target_uuid {
                continue;
            }
            best = Some(match best {
                None => sb,
                Some(current) if is_fresher(sb, current) => sb,
                Some(current) => current,
            });
        }
        let Some(authoritative) = best else {
            return Err(AssemblyError::NoMembers);
        };
        Ok(Self {
            array_uuid: target_uuid,
            raid_level: authoritative.raid_level,
            member_count: authoritative.member_count,
            geometry: authoritative.geometry,
            generation: authoritative.generation,
            chunk_blocks: authoritative.chunk_blocks,
        })
    }

    /// Whether `sb` reports the same array shape (level, member count,
    /// geometry, and stripe unit) as this identity.
    fn shape_matches(&self, sb: &ArraySuperblock) -> bool {
        sb.raid_level == self.raid_level
            && sb.member_count == self.member_count
            && sb.geometry == self.geometry
            && sb.chunk_blocks == self.chunk_blocks
    }

    /// The verdict on `candidates[index]` against this identity: which slot it
    /// fills (and whether it is in sync), or why it is refused.
    ///
    /// This is the single decision the slot table
    /// ([`fill_slots`](Self::fill_slots)) is built from, so a per-candidate
    /// verdict and the slot table can never disagree (`AGENTS.md` §2.2).
    ///
    /// # Panics / bounds
    ///
    /// Returns [`CandidateVerdict::Rejected`]`(`[`RejectReason::WrongArray`]`)`
    /// for an out-of-range `index` (there is no such member to admit), so the
    /// call is total and never indexes out of bounds.
    #[must_use]
    pub fn verdict_of(&self, candidates: &[Candidate], index: usize) -> CandidateVerdict {
        let Some(candidate) = candidates.get(index) else {
            return CandidateVerdict::Rejected(RejectReason::WrongArray);
        };
        let sb = &candidate.superblock;
        if sb.array_uuid != self.array_uuid {
            return CandidateVerdict::Rejected(RejectReason::WrongArray);
        }
        if !self.shape_matches(sb) {
            return CandidateVerdict::Rejected(RejectReason::Mismatched);
        }
        if sb.member_slot >= self.member_count {
            return CandidateVerdict::Rejected(RejectReason::BadSlot);
        }
        // Lose the slot to any better claimant: a shape-matching candidate for
        // the same slot that is fresher, or equally fresh with a lower tag
        // (the deterministic tie-break). A candidate never loses to itself.
        for (other_index, other) in candidates.iter().enumerate() {
            if other_index == index {
                continue;
            }
            let osb = &other.superblock;
            if osb.array_uuid != self.array_uuid
                || !self.shape_matches(osb)
                || osb.member_slot != sb.member_slot
            {
                continue;
            }
            if is_fresher(osb, sb) || (equally_fresh(osb, sb) && other.tag < candidate.tag) {
                return CandidateVerdict::Rejected(RejectReason::Duplicate);
            }
        }
        CandidateVerdict::Placed {
            slot: sb.member_slot,
            in_sync: sb.generation == self.generation,
        }
    }

    /// Fill `slots` with the reassembled member table, one entry per array
    /// slot. `slots` must be exactly [`member_count`](Self::member_count)
    /// long.
    ///
    /// Every slot is initialised [`SlotDisposition::Missing`]; each candidate
    /// [`verdict_of`](Self::verdict_of) admits is written into its slot. Slot
    /// contention is already resolved by `verdict_of` (exactly one candidate is
    /// [`CandidateVerdict::Placed`] for any slot), so a single pass fills the
    /// table unambiguously.
    ///
    /// # Errors
    ///
    /// [`AssemblyError::NoMembers`] if `slots.len()` is not
    /// [`member_count`](Self::member_count) (the caller sized the buffer
    /// wrong); the table is left untouched.
    pub fn fill_slots(
        &self,
        candidates: &[Candidate],
        slots: &mut [SlotDisposition],
    ) -> Result<(), AssemblyError> {
        if slots.len() != usize::from(self.member_count) {
            return Err(AssemblyError::NoMembers);
        }
        for slot in slots.iter_mut() {
            *slot = SlotDisposition::Missing;
        }
        for (index, candidate) in candidates.iter().enumerate() {
            if let CandidateVerdict::Placed { slot, in_sync } = self.verdict_of(candidates, index) {
                slots[usize::from(slot)] = SlotDisposition::Present {
                    tag: candidate.tag,
                    in_sync,
                };
            }
        }
        Ok(())
    }

    /// Advance this identity to the next array generation, returning the
    /// identity the survivors of a membership change persist.
    ///
    /// The generation counter is the array's event count: every membership
    /// change (a member dropping out on a fault, a rebuilt member rejoining)
    /// bumps it, and the survivors re-stamp their superblocks
    /// ([`member_superblock`](Self::member_superblock)) at the new value. A
    /// member that was *absent* for the bump keeps its lower generation, so on
    /// return it resolves as a **stale** rebuild target
    /// ([`verdict_of`](Self::verdict_of) reports `in_sync == false`) rather
    /// than being trusted as current. This is the write-side counterpart of
    /// [`resolve`](Self::resolve) that closes the stale-read window: a disk
    /// that missed writes while it was gone can never come back masquerading
    /// as up to date (the charter's fail-closed rule; §26.5 "a disk that
    /// missed writes is a disk that can lie").
    ///
    /// The bump saturates at [`u64::MAX`]: an array that somehow reached
    /// `2^64 - 1` membership changes stops advancing rather than wrapping to a
    /// generation an already-written member could match. Saturation is the
    /// safe direction — every live member simply shares the ceiling and stays
    /// in sync — and is unreachable in practice.
    #[must_use]
    pub const fn bump_generation(self) -> Self {
        Self {
            generation: self.generation.saturating_add(1),
            ..self
        }
    }

    /// Build the on-disk [`ArraySuperblock`] a **current** member in `slot`
    /// persists for this array: the identity's shape and its current
    /// generation, stamped `updated_at`.
    ///
    /// This is the record written to a member the array considers in sync — a
    /// survivor re-stamped after a [`bump_generation`](Self::bump_generation),
    /// a freshly-created member, or a rebuilt member being promoted to current
    /// on resync completion (writing the current generation is exactly what
    /// makes a formerly-stale copy resolve as in sync again). It is never
    /// written to a member that is still behind: such a member must keep its
    /// lower generation until its rebuild finishes, so that it stays a
    /// [`stale`](SlotDisposition) read-excluded rebuild target
    /// (`AGENTS.md` §5.4, §26.5).
    ///
    /// # Errors / bounds
    ///
    /// Returns [`None`] if `slot` is not less than
    /// [`member_count`](Self::member_count): there is no such member slot, so
    /// the call fails closed rather than minting a superblock for a slot the
    /// array cannot admit ([`decode`](ArraySuperblock::decode) would reject it
    /// anyway as [`SlotOutOfRange`](SuperblockError::SlotOutOfRange)).
    #[must_use]
    pub const fn member_superblock(
        &self,
        slot: u16,
        updated_at: Time64,
    ) -> Option<ArraySuperblock> {
        if slot >= self.member_count {
            return None;
        }
        Some(ArraySuperblock {
            array_uuid: self.array_uuid,
            raid_level: self.raid_level,
            member_count: self.member_count,
            member_slot: slot,
            geometry: self.geometry,
            generation: self.generation,
            updated_at,
            chunk_blocks: self.chunk_blocks,
        })
    }
}

/// A lazy iterator over the **distinct** array identifiers present in a
/// candidate set, in first-appearance order.
///
/// Built by [`distinct_arrays`]. See that function for the contract; this type
/// is its return value and holds only a borrow of the candidate slice and a
/// cursor, so it allocates nothing and imposes no ceiling on the number of
/// arrays it can enumerate (`AGENTS.md` §24.1).
#[derive(Clone, Debug)]
pub struct DistinctArrays<'a> {
    candidates: &'a [Candidate],
    next: usize,
}

impl Iterator for DistinctArrays<'_> {
    type Item = ArrayUuid;

    fn next(&mut self) -> Option<ArrayUuid> {
        while let Some(candidate) = self.candidates.get(self.next) {
            let index = self.next;
            self.next += 1;
            let uuid = candidate.superblock.array_uuid;
            // Yield a UUID only on its *first* appearance: a UUID already seen
            // among the candidates before this one has been yielded, so
            // skipping it here deduplicates without holding any state beyond
            // the cursor. The scan of the prefix is bounded by the candidate
            // count (the number of discovered devices, tiny), never unbounded.
            let seen_earlier = self.candidates[..index]
                .iter()
                .any(|earlier| earlier.superblock.array_uuid == uuid);
            if !seen_earlier {
                return Some(uuid);
            }
        }
        None
    }
}

/// Enumerate the distinct array identifiers present among `candidates`, so the
/// assembling serve process can discover *which* arrays exist before resolving
/// each one with [`ArrayIdentity::resolve`].
///
/// Device discovery (`AGENTS.md` §18) hands the serve process a heterogeneous
/// set of block devices whose superblocks decoded: some may be members of one
/// array, some of another, and any two need not belong to the same array. This
/// is the primitive that partitions that set by array identity — the "which
/// arrays are on these disks" step that precedes assembly, so an array is
/// *discovered*, never configured (`AGENTS.md` §18, §16.5).
///
/// The returned [`DistinctArrays`] yields each array's [`ArrayUuid`] exactly
/// once, in the order it first appears in `candidates`, so enumeration is
/// deterministic. It borrows `candidates` and allocates nothing, imposing no
/// ceiling on the number of arrays (`AGENTS.md` §24.1); the caller drives it to
/// build one [`ArrayIdentity`] per array:
///
/// ```ignore
/// for uuid in distinct_arrays(&candidates) {
///     let identity = ArrayIdentity::resolve(uuid, &candidates)?;
///     // …assemble this array from its members in `candidates`…
/// }
/// ```
///
/// A `Candidate` is only offered once its superblock passed
/// [`ArraySuperblock::decode`], so every enumerated array has at least one
/// member and [`ArrayIdentity::resolve`] cannot fail with
/// [`AssemblyError::NoMembers`] for a UUID this iterator yields.
#[must_use]
pub fn distinct_arrays(candidates: &[Candidate]) -> DistinctArrays<'_> {
    DistinctArrays {
        candidates,
        next: 0,
    }
}

/// Whether `a` is strictly fresher than `b`: a higher generation, or the same
/// generation with a lower slot (the deterministic tie-break).
fn is_fresher(a: &ArraySuperblock, b: &ArraySuperblock) -> bool {
    (a.generation, b.member_slot) > (b.generation, a.member_slot)
}

/// Whether `a` and `b` are equally fresh (same generation and slot).
fn equally_fresh(a: &ArraySuperblock, b: &ArraySuperblock) -> bool {
    a.generation == b.generation && a.member_slot == b.member_slot
}

#[cfg(test)]
mod tests;
