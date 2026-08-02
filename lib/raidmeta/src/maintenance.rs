//! The RAID **array maintenance record**: the durable position of an array's
//! self-maintenance, so a verification pass or a rebuild survives a reboot
//! instead of starting over (`plans/FIX-IO.md` IO6).
//!
//! An array's maintenance is deliberately incremental: a scrub and a resync
//! each advance a cursor one bounded chunk at a time so a 100 TB+ array never
//! verifies or rebuilds in one sweep (`AGENTS.md` §26.6, §2.23). On such an
//! array a full pass is measured in **hours or days**, which is longer than
//! the interval between reboots on a real machine. Holding that cursor only in
//! memory would mean every restart silently discarded the work and began
//! again, so a large array could be rebooted often enough that it never
//! finishes a rebuild or is never verified at all — a latent, unbounded
//! data-integrity hole precisely where redundancy is supposed to protect the
//! most data (`AGENTS.md` §26.5, §26.6).
//!
//! [`MaintenanceRecord`] closes it, and carries the contract in full.

use tairix_abi::time::Time64;

use crate::{ArrayIdentity, ArrayUuid};

/// The member block holding the [`ArraySuperblock`](crate::ArraySuperblock):
/// the first block of every member, where discovery probes for it.
pub const SUPERBLOCK_BLOCK: u64 = 0;

/// The member block holding the [`MaintenanceRecord`]: the block immediately
/// after the superblock.
///
/// Every member of the array carries its own copy, written by the serving
/// process as the array's maintenance advances. A member that is not current
/// (absent during a checkpoint, or rebuilding) simply carries a staler copy;
/// the freshest one wins, which is what
/// [`is_fresher_than`](MaintenanceRecord::is_fresher_than) decides.
pub const MAINTENANCE_BLOCK: u64 = 1;

/// The number of member blocks reserved for array metadata: the superblock and
/// the maintenance record.
///
/// A member's share of the array's *data* begins at this block offset, so this
/// is the single definition of the member data offset every consumer derives
/// (`AGENTS.md` §2.2) — a second, hand-picked offset anywhere would silently
/// place the array's data over its own metadata.
pub const RESERVED_METADATA_BLOCKS: u64 = MAINTENANCE_BLOCK + 1;

/// A reason a [`MaintenanceRecord`] could not be decoded. Every variant is a
/// fail-closed refusal: a record that does not decode cleanly is discarded, so
/// the array verifies and rebuilds from the beginning rather than trusting a
/// cursor it cannot vouch for.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum MaintenanceRecordError {
    /// The input was shorter than [`MaintenanceRecord::WIRE_LEN`].
    TooSmall,
    /// The leading magic did not match [`MaintenanceRecord::MAGIC`]: not a
    /// TAIRiX RAID maintenance record (very likely array data, or a blank
    /// block).
    BadMagic,
    /// The version field named a format this build does not understand.
    UnsupportedVersion,
    /// The stored CRC-32C did not match the recomputed one: the record is
    /// corrupt or torn.
    BadChecksum,
    /// The flags byte carried a bit this build does not define. A record whose
    /// meaning is partly unknown is not partly trusted.
    UnknownFlags,
    /// The stored completion timestamp was not a canonical [`Time64`].
    BadTimestamp,
    /// A field the flags mark absent was not zero. The encoding is canonical —
    /// an unset cursor or completion stamp is stored as zero — so a record
    /// carrying data in a field it declares absent is malformed, not merely
    /// odd, and is refused rather than half-read.
    NonCanonicalField,
}

/// Bit 0: [`MaintenanceRecord::last_scrub_completed`] is present.
const FLAG_SCRUB_COMPLETED: u8 = 1 << 0;
/// Bit 1: the scrub cursor is present (a verification pass is in progress).
const FLAG_SCRUB_CURSOR: u8 = 1 << 1;
/// Bit 2: the resync cursor is present (a rebuild is in progress).
const FLAG_RESYNC_CURSOR: u8 = 1 << 2;
/// Every bit this build defines; any other bit set fails the decode closed.
const FLAG_KNOWN: u8 = FLAG_SCRUB_COMPLETED | FLAG_SCRUB_CURSOR | FLAG_RESYNC_CURSOR;

// Field offsets within the fixed little-endian record. Laid out with no
// padding: every field is read from its explicit offset with `from_le_bytes`,
// never a struct cast, so alignment is irrelevant.
const OFF_MAGIC: usize = 0; // [u8; 8]
const OFF_VERSION: usize = 8; // u16
const OFF_FLAGS: usize = 10; // u8
const OFF_UUID: usize = 11; // [u8; 16]
const OFF_GENERATION: usize = 27; // u64
const OFF_SEQUENCE: usize = 35; // u64
const OFF_SCRUB_CURSOR: usize = 43; // u64
const OFF_RESYNC_CURSOR: usize = 51; // u64
const OFF_LAST_SCRUB: usize = 59; // Time64 (WIRE_LEN = 12)
const OFF_CHECKSUM: usize = OFF_LAST_SCRUB + Time64::WIRE_LEN; // u32

/// The resumable position of an array's self-maintenance: how far a
/// verification pass and a rebuild have got.
///
/// This is the value the composition engines report and accept, and the value
/// a [`MaintenanceRecord`] carries across a reboot, so the in-memory and
/// on-disk notions of "how far have we got" are the same one (`AGENTS.md`
/// §2.2). [`None`] means "not running": no pass in progress, nothing to
/// resume.
///
/// The cursors are in the engine's own cursor domain — the same units its
/// `scrub_cursor()` reports — which for a striped level is per-member blocks
/// rather than array logical blocks. They are only ever produced and consumed
/// by the same array, and the receiving engine re-validates them against its
/// own bounds, so the domain never has to be guessed.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Default)]
pub struct ArrayProgress {
    /// The next block a verification (scrub) pass will read, or [`None`] if no
    /// pass is in progress.
    pub scrub_cursor: Option<u64>,
    /// The next block a rebuild (resync) will copy, or [`None`] if no member
    /// is rebuilding.
    pub resync_cursor: Option<u64>,
}

impl ArrayProgress {
    /// Nothing in progress: neither a verification pass nor a rebuild.
    pub const IDLE: Self = Self {
        scrub_cursor: None,
        resync_cursor: None,
    };

    /// Whether either a verification pass or a rebuild is in progress.
    #[must_use]
    pub const fn is_active(&self) -> bool {
        self.scrub_cursor.is_some() || self.resync_cursor.is_some()
    }

    /// Whether every cursor present names a block an array of `span` blocks
    /// actually has.
    ///
    /// A cursor is the *next* block a pass will process, so a pass that is in
    /// progress has strictly less than `span` behind it; `span` itself is the
    /// idle position, which is spelled [`None`] here. This is the one
    /// definition of that bound (`AGENTS.md` §2.2), applied by every
    /// composition engine before it adopts a restored position, because a
    /// cursor beyond the end of the array is not a harmless oddity: adopted as
    /// a rebuild position it would declare a member fully copied without ever
    /// having copied its tail, leaving a member trusted as a current read
    /// source while holding stale data (`AGENTS.md` §5.4, §26.5). A record
    /// that does not fit is refused, and the array starts its passes afresh.
    #[must_use]
    pub const fn fits_span(&self, span: u64) -> bool {
        let scrub_ok = match self.scrub_cursor {
            Some(cursor) => cursor < span,
            None => true,
        };
        let resync_ok = match self.resync_cursor {
            Some(cursor) => cursor < span,
            None => true,
        };
        scrub_ok && resync_ok
    }
}

/// The on-disk record of an array's maintenance progress, carried by every
/// member alongside its [`ArraySuperblock`](crate::ArraySuperblock): the live
/// scrub and rebuild cursors, and the instant the last *complete* verification
/// pass finished.
///
/// # Why it is a separate record from the superblock
///
/// The superblock records the array's **identity and membership**: it changes
/// only when the array's shape does. This record records **progress**: it is
/// checkpointed as the array works. Keeping them in one record would put the
/// array's identity at risk on every routine checkpoint, since a torn write of
/// a progress update could damage the metadata assembly depends on. They live
/// in separate blocks ([`SUPERBLOCK_BLOCK`] / [`MAINTENANCE_BLOCK`]) so a lost
/// or corrupt maintenance record can never cost the array its identity, and so
/// the two can be written at completely different rates.
///
/// # Fail-safe in both directions
///
/// The record is bound to the array it describes, and every way of losing it
/// degrades toward *more* verification, never less (`AGENTS.md` §5.4, §26.5):
///
/// * A record that does not decode — absent, blank, torn, or corrupt — yields
///   nothing. The array then verifies from the beginning and rebuilds from the
///   beginning: slower, never unsound.
/// * A record whose array identity does not match is ignored entirely: a
///   foreign or recycled disk can never inject cursors into this array.
/// * The cursors are additionally bound to the array **generation** they were
///   taken at ([`progress_for`](Self::progress_for)). A membership change bumps
///   that generation ([`bump_generation`](crate::ArrayIdentity::bump_generation)),
///   which invalidates them — exactly right, because a member that joined or
///   left changes what still needs verifying or rebuilding, so resuming a stale
///   cursor could skip data that a new member never received.
/// * A completion stamp *ahead* of the current wall clock is not credible (a
///   clock step, an unset clock, or a forged record), so
///   [`since_last_scrub_ns`](Self::since_last_scrub_ns) reports "unknown" and
///   the array is verified now rather than trusted as recently clean.
///
/// Because the checksum, the identity binding, and the canonical-encoding
/// checks are all enforced on decode, a hostile or failing disk cannot use this
/// record to make an array *skip* work: the worst a bad record achieves is
/// being discarded.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct MaintenanceRecord {
    /// The array this record describes. A record whose identifier does not
    /// match the assembled array is ignored.
    pub array_uuid: ArrayUuid,
    /// The array generation the cursors were taken at. Cursors are honoured
    /// only while the array is still at this generation.
    pub generation: u64,
    /// A counter the writer advances on every checkpoint, so the freshest of
    /// several members' copies can be identified even when they were written
    /// at the same array generation
    /// ([`is_fresher_than`](Self::is_fresher_than)).
    pub sequence: u64,
    /// The maintenance position at this checkpoint.
    pub progress: ArrayProgress,
    /// When the last *complete* verification pass finished, or [`None`] if the
    /// array has never completed one. Stored as a full [`Time64`]
    /// (`AGENTS.md` §21), and deliberately independent of the cursors: it
    /// survives a membership change, because verifying the array is a property
    /// of the data, not of the member set.
    pub last_scrub_completed: Option<Time64>,
}

impl MaintenanceRecord {
    /// The 8-byte magic that opens every maintenance record (`"TXRAIDMT"`).
    pub const MAGIC: [u8; 8] = *b"TXRAIDMT";

    /// The only maintenance-record format version this build reads or writes.
    /// The on-disk format is unfrozen pre-release (`AGENTS.md` §2.13): it is
    /// changed in place, never versioned alongside an old one.
    pub const FORMAT_VERSION: u16 = 1;

    /// The encoded size of a record in bytes. The CRC-32C covers the first
    /// `WIRE_LEN - 4` bytes; the trailing four bytes are the checksum.
    pub const WIRE_LEN: usize = OFF_CHECKSUM + 4;

    /// Build the record to persist for `identity` at this checkpoint.
    ///
    /// `sequence` is the writer's checkpoint counter (advanced per write, so
    /// the freshest copy is identifiable), `progress` is the array's current
    /// position, and `last_scrub_completed` is the instant of the array's last
    /// finished verification pass — carried forward unchanged from the
    /// previous record until a pass completes.
    #[must_use]
    pub const fn checkpoint(
        identity: &ArrayIdentity,
        sequence: u64,
        progress: ArrayProgress,
        last_scrub_completed: Option<Time64>,
    ) -> Self {
        Self {
            array_uuid: identity.array_uuid,
            generation: identity.generation,
            sequence,
            progress,
            last_scrub_completed,
        }
    }

    /// Whether `self` is a later checkpoint of the same array than `other`.
    ///
    /// Every member carries its own copy and they need not agree: a member
    /// that was absent, faulted, or rebuilding during a checkpoint holds a
    /// staler one. The freshest is the one at the highest array generation,
    /// breaking a tie on the checkpoint sequence, so a copy from a superseded
    /// membership can never outrank a current one no matter how many times it
    /// was written.
    ///
    /// Records of *different* arrays are not comparable, and neither is
    /// fresher: this returns `false` in that case, so a foreign record can
    /// never win a comparison.
    #[must_use]
    pub fn is_fresher_than(&self, other: &Self) -> bool {
        if self.array_uuid != other.array_uuid {
            return false;
        }
        (self.generation, self.sequence) > (other.generation, other.sequence)
    }

    /// Whether this record describes `identity`'s array at all.
    ///
    /// Everything the record says — its cursors *and* when the array was last
    /// verified — is about the array it names and no other, so a consumer asks
    /// this before believing any of it. A recycled or hostile disk carrying
    /// another array's record must be unable to inject a position into this
    /// array, and equally unable to talk it out of verifying itself by
    /// claiming a completion that was never this array's.
    #[must_use]
    pub fn belongs_to(&self, identity: &ArrayIdentity) -> bool {
        self.array_uuid == identity.array_uuid
    }

    /// The maintenance position this record can restore into `identity`, or
    /// [`ArrayProgress::IDLE`] if it cannot restore one.
    ///
    /// The cursors are honoured **only** when the record describes this array
    /// *at its current generation*. A record from another array is not this
    /// array's business, and a record from an earlier generation was taken
    /// under a different member set: a member has joined or left since, so a
    /// resumed cursor could skip data the new member never received or verify
    /// against copies that no longer exist. In both cases the array starts its
    /// passes afresh, which costs time and never correctness (`AGENTS.md`
    /// §5.4, §26.5).
    #[must_use]
    pub fn progress_for(&self, identity: &ArrayIdentity) -> ArrayProgress {
        if !self.belongs_to(identity) || self.generation != identity.generation {
            return ArrayProgress::IDLE;
        }
        self.progress
    }

    /// How long ago the array's last complete verification pass finished, as
    /// of the wall-clock instant `now`, in nanoseconds — the elapsed span the
    /// maintenance scheduler is seeded with.
    ///
    /// Returns [`u64::MAX`] ("effectively forever ago", so a pass is due at
    /// once) when the history is unknown or not credible:
    ///
    /// * the array has never completed a pass, so nothing has been verified;
    /// * the recorded completion is *ahead* of `now`, which a real completion
    ///   cannot be. That happens when the wall clock has not been set this
    ///   boot, was stepped backwards, or the record was forged; trusting it
    ///   would let a bogus future stamp suppress verification indefinitely.
    ///
    /// An array whose verification history is unknown is verified, never
    /// assumed clean (`AGENTS.md` §5.4, §26.5).
    #[must_use]
    pub fn since_last_scrub_ns(&self, now: Time64) -> u64 {
        let Some(completed) = self.last_scrub_completed else {
            return u64::MAX;
        };
        if completed > now {
            return u64::MAX;
        }
        now.saturating_duration_since(completed)
            .saturating_total_nanos()
    }

    /// The flags byte describing which optional fields this record carries.
    const fn flags(&self) -> u8 {
        let mut flags = 0u8;
        if self.last_scrub_completed.is_some() {
            flags |= FLAG_SCRUB_COMPLETED;
        }
        if self.progress.scrub_cursor.is_some() {
            flags |= FLAG_SCRUB_CURSOR;
        }
        if self.progress.resync_cursor.is_some() {
            flags |= FLAG_RESYNC_CURSOR;
        }
        flags
    }

    /// Encode `self` into its fixed-size little-endian on-disk form, sealing it
    /// with a trailing CRC-32C over the preceding bytes.
    ///
    /// The encoding is canonical: a field the flags mark absent is written as
    /// zero, which is exactly what [`decode`](Self::decode) requires, so a
    /// decoded record always re-encodes to the identical bytes.
    #[must_use]
    pub fn encode(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        out[OFF_MAGIC..OFF_MAGIC + 8].copy_from_slice(&Self::MAGIC);
        out[OFF_VERSION..OFF_VERSION + 2].copy_from_slice(&Self::FORMAT_VERSION.to_le_bytes());
        out[OFF_FLAGS] = self.flags();
        out[OFF_UUID..OFF_UUID + 16].copy_from_slice(&self.array_uuid);
        out[OFF_GENERATION..OFF_GENERATION + 8].copy_from_slice(&self.generation.to_le_bytes());
        out[OFF_SEQUENCE..OFF_SEQUENCE + 8].copy_from_slice(&self.sequence.to_le_bytes());
        out[OFF_SCRUB_CURSOR..OFF_SCRUB_CURSOR + 8]
            .copy_from_slice(&self.progress.scrub_cursor.unwrap_or(0).to_le_bytes());
        out[OFF_RESYNC_CURSOR..OFF_RESYNC_CURSOR + 8]
            .copy_from_slice(&self.progress.resync_cursor.unwrap_or(0).to_le_bytes());
        out[OFF_LAST_SCRUB..OFF_LAST_SCRUB + Time64::WIRE_LEN].copy_from_slice(
            &self
                .last_scrub_completed
                .unwrap_or(Time64::UNIX_EPOCH)
                .to_le_bytes(),
        );
        let crc = tairix_crc32c::checksum(&out[..OFF_CHECKSUM]);
        out[OFF_CHECKSUM..OFF_CHECKSUM + 4].copy_from_slice(&crc.to_le_bytes());
        out
    }

    /// Decode a maintenance record from the leading [`WIRE_LEN`] bytes of
    /// `bytes`, validating every field and failing closed on the first fault.
    ///
    /// [`WIRE_LEN`]: Self::WIRE_LEN
    ///
    /// # Errors
    ///
    /// A [`MaintenanceRecordError`] for any of: a short input, a bad magic, an
    /// unknown version, a checksum mismatch, an undefined flag bit, a
    /// non-canonical timestamp, or a field carrying data the flags declare
    /// absent.
    pub fn decode(bytes: &[u8]) -> Result<Self, MaintenanceRecordError> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(MaintenanceRecordError::TooSmall);
        }
        if bytes[OFF_MAGIC..OFF_MAGIC + 8] != Self::MAGIC {
            return Err(MaintenanceRecordError::BadMagic);
        }
        let version = u16::from_le_bytes([bytes[OFF_VERSION], bytes[OFF_VERSION + 1]]);
        if version != Self::FORMAT_VERSION {
            return Err(MaintenanceRecordError::UnsupportedVersion);
        }
        let stored_crc = read_u32(bytes, OFF_CHECKSUM);
        if tairix_crc32c::checksum(&bytes[..OFF_CHECKSUM]) != stored_crc {
            return Err(MaintenanceRecordError::BadChecksum);
        }
        let flags = bytes[OFF_FLAGS];
        if flags & !FLAG_KNOWN != 0 {
            return Err(MaintenanceRecordError::UnknownFlags);
        }
        let mut array_uuid = [0u8; 16];
        array_uuid.copy_from_slice(&bytes[OFF_UUID..OFF_UUID + 16]);
        let scrub_cursor = optional_u64(bytes, OFF_SCRUB_CURSOR, flags & FLAG_SCRUB_CURSOR != 0)?;
        let resync_cursor =
            optional_u64(bytes, OFF_RESYNC_CURSOR, flags & FLAG_RESYNC_CURSOR != 0)?;
        let stamp = Time64::from_bytes(&bytes[OFF_LAST_SCRUB..OFF_LAST_SCRUB + Time64::WIRE_LEN])
            .map_err(|_| MaintenanceRecordError::BadTimestamp)?;
        let last_scrub_completed = if flags & FLAG_SCRUB_COMPLETED != 0 {
            Some(stamp)
        } else {
            // An absent stamp is stored as the zero encoding; anything else
            // means the flags and the payload disagree.
            if stamp != Time64::UNIX_EPOCH {
                return Err(MaintenanceRecordError::NonCanonicalField);
            }
            None
        };
        Ok(Self {
            array_uuid,
            generation: read_u64(bytes, OFF_GENERATION),
            sequence: read_u64(bytes, OFF_SEQUENCE),
            progress: ArrayProgress {
                scrub_cursor,
                resync_cursor,
            },
            last_scrub_completed,
        })
    }
}

// The superblock and the maintenance record occupy distinct blocks, so a
// routine progress checkpoint can never tear the array's identity, and the
// array's data starts clear of both.
const _: () = assert!(SUPERBLOCK_BLOCK != MAINTENANCE_BLOCK);
const _: () = assert!(RESERVED_METADATA_BLOCKS > SUPERBLOCK_BLOCK);
const _: () = assert!(RESERVED_METADATA_BLOCKS > MAINTENANCE_BLOCK);
// The record must fit the smallest block a real device presents, or a member
// with 512-byte blocks could not hold it in its own block.
const _: () = assert!(MaintenanceRecord::WIRE_LEN <= 512);

/// Read the optional `u64` at `offset`: its value when `present`, or [`None`]
/// when absent — in which case the stored bytes must be zero, since the
/// encoding is canonical.
fn optional_u64(
    bytes: &[u8],
    offset: usize,
    present: bool,
) -> Result<Option<u64>, MaintenanceRecordError> {
    let raw = read_u64(bytes, offset);
    if present {
        Ok(Some(raw))
    } else if raw == 0 {
        Ok(None)
    } else {
        Err(MaintenanceRecordError::NonCanonicalField)
    }
}

/// Read the little-endian `u32` at `offset`. The caller has already checked
/// the length, so the slice indexing cannot be out of range.
fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    let mut buf = [0u8; 4];
    buf.copy_from_slice(&bytes[offset..offset + 4]);
    u32::from_le_bytes(buf)
}

/// Read the little-endian `u64` at `offset`. The caller has already checked
/// the length, so the slice indexing cannot be out of range.
fn read_u64(bytes: &[u8], offset: usize) -> u64 {
    let mut buf = [0u8; 8];
    buf.copy_from_slice(&bytes[offset..offset + 8]);
    u64::from_le_bytes(buf)
}

#[cfg(test)]
mod tests;
