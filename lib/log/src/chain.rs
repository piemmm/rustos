//! Hash-chained audit records (`AGENTS.md` §19.4).
//!
//! The append-only security log under `/System/Logs` must be
//! *tamper-evident*: an attacker who can write to the log store must not be
//! able to alter or delete an existing entry without the change being
//! detectable. This module provides the cryptographic backbone for that
//! guarantee — the persistence layer (the `/System/Logs` writer, Stage 5)
//! and the periodic signed anchors (§19.4, blocked on the Stage 2 signing
//! authority) are built on top of it.
//!
//! # Model
//!
//! Each CPU owns one [`LogChain`]. Appending a record produces a
//! [`ChainedEntry`] whose hash binds:
//!
//! * the hash of the *previous* entry on that CPU's chain (the genesis
//!   entry binds [`GENESIS_ANCHOR`]),
//! * a strictly monotonic per-CPU sequence number,
//! * the owning CPU id, and
//! * a digest of the record payload.
//!
//! Because each entry hash feeds into the next, editing or removing any
//! entry breaks every later link; truncation is detectable because the
//! sequence numbers are contiguous and the chain head is anchored
//! separately (§19.4). [`verify_chain`] re-derives the whole chain and
//! reports the first inconsistency.
//!
//! # No allocation
//!
//! The hot path ([`LogChain::append`]) hashes a single fixed-size stack
//! buffer and performs no allocation, matching the crate's no-alloc
//! contract. The payload is reduced to a fixed-size [`Sha256Digest`] by the
//! caller (which already holds the serialized record bytes), keeping this
//! module payload-format agnostic.

use rustos_crypto::{sha256, Sha256Digest, SHA256_OUTPUT_LEN};

/// The anchor that precedes the first entry of a chain.
///
/// A fresh [`LogChain`] starts with this value as its head hash, so the
/// genesis entry's `prev_hash` is all-zero. It is *not* a valid entry hash
/// (no payload hashes to it under SHA-256 with overwhelming probability),
/// which lets a verifier distinguish "start of chain" from "links to a real
/// predecessor".
pub const GENESIS_ANCHOR: Sha256Digest = [0u8; SHA256_OUTPUT_LEN];

/// Length of the byte string hashed to produce an entry hash:
/// `prev_hash(32) || seq(8) || cpu(4) || payload_digest(32)`.
const ENTRY_PREIMAGE_LEN: usize = SHA256_OUTPUT_LEN + 8 + 4 + SHA256_OUTPUT_LEN;

/// Compute the hash that links an entry into its chain.
///
/// The field order and little-endian encoding are part of the on-disk
/// audit-log contract and must not change without an audit-log format
/// version bump.
fn link_hash(
    prev_hash: &Sha256Digest,
    seq: u64,
    cpu: u32,
    payload_digest: &Sha256Digest,
) -> Sha256Digest {
    let mut preimage = [0u8; ENTRY_PREIMAGE_LEN];
    preimage[0..SHA256_OUTPUT_LEN].copy_from_slice(prev_hash);
    let mut cursor = SHA256_OUTPUT_LEN;
    preimage[cursor..cursor + 8].copy_from_slice(&seq.to_le_bytes());
    cursor += 8;
    preimage[cursor..cursor + 4].copy_from_slice(&cpu.to_le_bytes());
    cursor += 4;
    preimage[cursor..cursor + SHA256_OUTPUT_LEN].copy_from_slice(payload_digest);
    sha256(&preimage)
}

/// One tamper-evident record in a per-CPU audit chain.
///
/// A `ChainedEntry` is self-describing: [`Self::recompute_hash`] re-derives
/// [`Self::entry_hash`] from the other fields, so a verifier never has to
/// trust a stored hash it did not recompute.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct ChainedEntry {
    /// CPU that issued this entry. A chain is per-CPU; all entries in one
    /// chain share this value.
    pub cpu: u32,
    /// Strictly monotonic, contiguous sequence number within the CPU's
    /// chain. The genesis entry has sequence `0`.
    pub seq: u64,
    /// Hash of the previous entry on this CPU's chain, or [`GENESIS_ANCHOR`]
    /// for the first entry.
    pub prev_hash: Sha256Digest,
    /// Digest of the record payload supplied by the caller.
    pub payload_digest: Sha256Digest,
    /// Hash that links this entry into the chain. Equal to
    /// [`Self::recompute_hash`] for an unmodified entry.
    pub entry_hash: Sha256Digest,
}

impl ChainedEntry {
    /// Re-derive the entry hash from the entry's other fields.
    ///
    /// Used by [`verify_chain`]; exposed so a caller can independently audit
    /// a single persisted entry.
    #[must_use]
    pub fn recompute_hash(&self) -> Sha256Digest {
        link_hash(&self.prev_hash, self.seq, self.cpu, &self.payload_digest)
    }

    /// Whether the stored [`Self::entry_hash`] matches a fresh recomputation
    /// of the entry's contents.
    ///
    /// A `false` result means the entry was altered after it was issued.
    #[must_use]
    pub fn is_self_consistent(&self) -> bool {
        self.recompute_hash() == self.entry_hash
    }
}

/// The growing head of one CPU's tamper-evident audit chain.
///
/// Hold one per CPU (`AGENTS.md` §19.4 "monotonic per-CPU sequence
/// number"). [`Self::append`] is the only mutating operation and never
/// allocates.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct LogChain {
    cpu: u32,
    next_seq: u64,
    head_hash: Sha256Digest,
}

impl LogChain {
    /// Start a fresh chain for `cpu`, anchored at [`GENESIS_ANCHOR`].
    #[must_use]
    pub fn new(cpu: u32) -> Self {
        Self {
            cpu,
            next_seq: 0,
            head_hash: GENESIS_ANCHOR,
        }
    }

    /// Resume an existing chain from a persisted head.
    ///
    /// `next_seq` is the sequence number the next appended entry will carry,
    /// and `head_hash` is the [`Self::head_hash`] recorded when the chain was
    /// last persisted. Used when reopening `/System/Logs` after a clean
    /// shutdown.
    #[must_use]
    pub fn resume(cpu: u32, next_seq: u64, head_hash: Sha256Digest) -> Self {
        Self {
            cpu,
            next_seq,
            head_hash,
        }
    }

    /// CPU this chain belongs to.
    #[must_use]
    pub fn cpu(&self) -> u32 {
        self.cpu
    }

    /// Sequence number the next [`Self::append`] will assign.
    #[must_use]
    pub fn next_seq(&self) -> u64 {
        self.next_seq
    }

    /// Current chain head hash — the root over every entry appended so far.
    ///
    /// This is the value the periodic anchor signs (§19.4). For a chain with
    /// no entries it equals [`GENESIS_ANCHOR`].
    #[must_use]
    pub fn head_hash(&self) -> Sha256Digest {
        self.head_hash
    }

    /// Append a record identified by `payload_digest`, returning its entry.
    ///
    /// Advances the chain head and the sequence counter. The caller computes
    /// `payload_digest` over the serialized record bytes it is about to
    /// persist (e.g. with [`rustos_crypto::sha256`]).
    pub fn append(&mut self, payload_digest: &Sha256Digest) -> ChainedEntry {
        let seq = self.next_seq;
        let prev_hash = self.head_hash;
        let entry_hash = link_hash(&prev_hash, seq, self.cpu, payload_digest);
        self.head_hash = entry_hash;
        self.next_seq = seq + 1;
        ChainedEntry {
            cpu: self.cpu,
            seq,
            prev_hash,
            payload_digest: *payload_digest,
            entry_hash,
        }
    }
}

/// Reason a chain failed [`verify_chain`].
///
/// `index` is the position, within the verified slice, of the offending
/// entry. A discontinuity is a security event in its own right (§19.4).
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum ChainError {
    /// The entry's stored hash does not match a recomputation of its
    /// contents: the entry was altered after issuance.
    HashMismatch {
        /// Position of the altered entry.
        index: usize,
    },
    /// The entry's `prev_hash` does not match the previous entry's hash (or
    /// the expected start hash for the first entry): an entry was inserted,
    /// removed, or reordered.
    BrokenLink {
        /// Position of the entry whose back-link is wrong.
        index: usize,
    },
    /// The entry's sequence number is not the expected contiguous successor:
    /// entries were dropped or duplicated.
    SequenceGap {
        /// Position of the entry with the unexpected sequence number.
        index: usize,
    },
    /// The entry belongs to a different CPU than the chain being verified.
    CpuMismatch {
        /// Position of the foreign entry.
        index: usize,
    },
}

/// Verify a slice of entries forms an unbroken chain and return its root.
///
/// `cpu`, `start_seq`, and `start_hash` describe the expected state *before*
/// `entries[0]`: for a chain verified from the beginning pass
/// [`verify_fresh_chain`] instead. On success the returned digest is the
/// chain head after the last entry (equal to `start_hash` when `entries` is
/// empty).
///
/// # Errors
///
/// Returns the first [`ChainError`] encountered, identifying the offending
/// entry by its index in `entries`.
pub fn verify_chain(
    entries: &[ChainedEntry],
    cpu: u32,
    start_seq: u64,
    start_hash: &Sha256Digest,
) -> Result<Sha256Digest, ChainError> {
    let mut expected_prev = *start_hash;
    let mut expected_seq = start_seq;
    for (index, entry) in entries.iter().enumerate() {
        if entry.cpu != cpu {
            return Err(ChainError::CpuMismatch { index });
        }
        if entry.seq != expected_seq {
            return Err(ChainError::SequenceGap { index });
        }
        if entry.prev_hash != expected_prev {
            return Err(ChainError::BrokenLink { index });
        }
        if !entry.is_self_consistent() {
            return Err(ChainError::HashMismatch { index });
        }
        expected_prev = entry.entry_hash;
        expected_seq = entry.seq + 1;
    }
    Ok(expected_prev)
}

/// Verify a chain from its genesis (`cpu`, sequence `0`, [`GENESIS_ANCHOR`]).
///
/// # Errors
///
/// Returns the first [`ChainError`] encountered.
pub fn verify_fresh_chain(entries: &[ChainedEntry], cpu: u32) -> Result<Sha256Digest, ChainError> {
    verify_chain(entries, cpu, 0, &GENESIS_ANCHOR)
}

#[cfg(test)]
mod tests {
    use super::{
        verify_chain, verify_fresh_chain, ChainError, ChainedEntry, LogChain, GENESIS_ANCHOR,
    };
    use rustos_crypto::sha256;

    fn build(cpu: u32, payloads: &[&[u8]]) -> (LogChain, Vec<ChainedEntry>) {
        let mut chain = LogChain::new(cpu);
        let mut entries = Vec::new();
        for payload in payloads {
            entries.push(chain.append(&sha256(payload)));
        }
        (chain, entries)
    }

    #[test]
    fn fresh_chain_starts_at_genesis() {
        let chain = LogChain::new(3);
        assert_eq!(chain.cpu(), 3);
        assert_eq!(chain.next_seq(), 0);
        assert_eq!(chain.head_hash(), GENESIS_ANCHOR);
    }

    #[test]
    fn first_entry_links_to_genesis_and_advances_head() {
        let (chain, entries) = build(0, &[b"boot"]);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].seq, 0);
        assert_eq!(entries[0].prev_hash, GENESIS_ANCHOR);
        assert!(entries[0].is_self_consistent());
        assert_eq!(chain.next_seq(), 1);
        assert_eq!(chain.head_hash(), entries[0].entry_hash);
        assert_ne!(chain.head_hash(), GENESIS_ANCHOR);
    }

    #[test]
    fn entries_link_back_to_back() {
        let (_, entries) = build(1, &[b"a", b"b", b"c"]);
        assert_eq!(entries[1].prev_hash, entries[0].entry_hash);
        assert_eq!(entries[2].prev_hash, entries[1].entry_hash);
        assert_eq!(entries[0].seq, 0);
        assert_eq!(entries[1].seq, 1);
        assert_eq!(entries[2].seq, 2);
    }

    #[test]
    fn verify_accepts_an_honest_chain_and_returns_head() {
        let (chain, entries) = build(2, &[b"x", b"y", b"z"]);
        let root = verify_fresh_chain(&entries, 2).expect("honest chain verifies");
        assert_eq!(root, chain.head_hash());
    }

    #[test]
    fn verify_accepts_empty_chain() {
        let root = verify_fresh_chain(&[], 0).expect("empty chain verifies");
        assert_eq!(root, GENESIS_ANCHOR);
    }

    #[test]
    fn tampering_with_a_payload_is_detected() {
        let (_, mut entries) = build(0, &[b"alpha", b"beta", b"gamma"]);
        entries[1].payload_digest = sha256(b"forged");
        assert_eq!(
            verify_fresh_chain(&entries, 0),
            Err(ChainError::HashMismatch { index: 1 })
        );
    }

    #[test]
    fn deleting_an_entry_breaks_the_link() {
        let (_, mut entries) = build(0, &[b"one", b"two", b"three"]);
        entries.remove(1);
        // Index 1 (formerly "three") now carries seq 2 where seq 1 is
        // expected, so the gap is caught before the link check.
        assert_eq!(
            verify_fresh_chain(&entries, 0),
            Err(ChainError::SequenceGap { index: 1 })
        );
    }

    #[test]
    fn reordering_entries_breaks_the_link() {
        let (_, mut entries) = build(0, &[b"one", b"two"]);
        entries.swap(0, 1);
        // The first slot now has seq 1 where seq 0 is expected.
        assert_eq!(
            verify_fresh_chain(&entries, 0),
            Err(ChainError::SequenceGap { index: 0 })
        );
    }

    #[test]
    fn forged_entry_hash_is_detected() {
        let (_, mut entries) = build(0, &[b"only"]);
        entries[0].entry_hash = sha256(b"not the real hash");
        assert_eq!(
            verify_fresh_chain(&entries, 0),
            Err(ChainError::HashMismatch { index: 0 })
        );
    }

    #[test]
    fn entry_from_another_cpu_is_rejected() {
        let (_, entries) = build(7, &[b"p", b"q"]);
        assert_eq!(
            verify_fresh_chain(&entries, 9),
            Err(ChainError::CpuMismatch { index: 0 })
        );
    }

    #[test]
    fn cpu_is_bound_into_the_hash() {
        let (_, a) = build(0, &[b"same"]);
        let (_, b) = build(1, &[b"same"]);
        assert_ne!(a[0].entry_hash, b[0].entry_hash);
    }

    #[test]
    fn resume_continues_an_existing_chain() {
        let (first, entries_a) = build(4, &[b"first", b"second"]);

        let mut resumed = LogChain::resume(4, first.next_seq(), first.head_hash());
        let cont = resumed.append(&sha256(b"third"));
        assert_eq!(cont.seq, 2);
        assert_eq!(cont.prev_hash, entries_a[1].entry_hash);

        let full = [entries_a[0], entries_a[1], cont];
        let root = verify_fresh_chain(&full, 4).expect("resumed chain verifies");
        assert_eq!(root, resumed.head_hash());
    }

    #[test]
    fn verify_chain_can_start_from_a_persisted_midpoint() {
        let (chain, entries) = build(5, &[b"a", b"b", b"c", b"d"]);
        // Verify only the tail, given the state captured after entry 1.
        let root = verify_chain(&entries[2..], 5, entries[2].seq, &entries[1].entry_hash)
            .expect("tail verifies against the captured midpoint");
        assert_eq!(root, chain.head_hash());
    }

    #[test]
    fn splicing_a_foreign_entry_with_a_matching_seq_breaks_the_link() {
        // SECURITY.md §3.4 / §19.4: an attacker who replaces an entry with
        // a *self-consistent* entry lifted from another chain (same CPU,
        // same sequence number, so the cheap seq/self-consistency checks
        // pass) is still caught — its `prev_hash` cannot match the real
        // predecessor, so verification reports the broken link and names
        // the offending index.
        let (_, honest) = build(0, &[b"alpha", b"beta", b"gamma"]);
        let (_, foreign) = build(0, &[b"x", b"y", b"z"]);
        let spliced = [honest[0], foreign[1], honest[2]];
        // The foreign entry is internally consistent and carries the
        // expected sequence number, so only the back-link betrays it.
        assert!(foreign[1].is_self_consistent());
        assert_eq!(foreign[1].seq, 1);
        assert_eq!(
            verify_fresh_chain(&spliced, 0),
            Err(ChainError::BrokenLink { index: 1 })
        );
    }

    #[test]
    fn truncating_the_tail_is_caught_against_the_signed_anchor() {
        // SECURITY.md §3.4 / §19.4: dropping the tail of the log leaves a
        // slice that still verifies *internally* — the attacker simply
        // forgot the later entries existed. Truncation is caught because
        // the chain head is anchored separately (the periodically signed
        // root, §19.4): the root recomputed over the truncated slice no
        // longer equals the anchored head.
        let (chain, entries) = build(2, &[b"one", b"two", b"three"]);
        let signed_anchor = chain.head_hash();

        let truncated = &entries[..2];
        let root = verify_fresh_chain(truncated, 2).expect("truncated slice is self-consistent");
        // Self-consistent in isolation, yet detectably short: the root
        // disagrees with the separately-signed anchor.
        assert_ne!(root, signed_anchor);
        assert_eq!(root, entries[1].entry_hash);
    }

    extern crate alloc;
    use alloc::vec::Vec;
}
