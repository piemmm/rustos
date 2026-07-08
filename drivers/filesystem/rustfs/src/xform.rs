//! The host-injected transform-cache seam (`plans/SMARTRAM.md` SMART3).
//!
//! Reading any byte of a compressed cluster costs the full transform
//! pipeline — one device read, one AEAD decrypt, and one integrity check
//! per stored block, then a whole-frame decompression
//! (`cluster::read_data_cluster`). The serving read path
//! ([`read_file`](crate::RustFs)) repeats that pipeline on every call
//! that touches the cluster, so a caller streaming a compressed file in
//! small reads pays it once per read instead of once per cluster.
//!
//! [`ClusterCache`] lets the *host* retain the verified, decrypted,
//! decompressed cluster plaintext between reads. The driver stays
//! kernel-independent: it only names this trait, and the concrete
//! implementation — classification through the kernel's reclaimable-
//! memory admission gate, byte budgets, pressure-band shrinking, LRU
//! eviction, and zeroisation of every released buffer — lives with the
//! host that mounts the volume (`rustos-kernel`'s transform cache). A
//! volume opened without a cache serves every read through the full
//! pipeline, exactly as before.
//!
//! # Coherence contract
//!
//! The driver keys entries by the cluster's first stored physical block
//! and upholds precise invalidation:
//!
//! * every block free funnels through the driver's `free_block`, which
//!   calls [`invalidate`](ClusterCache::invalidate) for the freed block
//!   — a freed run can only be rewritten after passing through there,
//!   so no entry outlives the bytes it was derived from;
//! * a transaction rollback returns this transaction's allocations to
//!   the pool without individual frees, so the driver calls
//!   [`purge`](ClusterCache::purge) — fail closed, never a stale entry;
//! * the integrity passes (scrub, check, rescue) never consult the
//!   cache: they exist to verify the on-disk bytes and always read the
//!   device.
//!
//! Cached plaintext is decrypted user data: an implementation must zero
//! every buffer it releases (invalidation, eviction, purge, teardown).

use alloc::vec::Vec;

use zeroize::Zeroize;

/// Upper bound on one cluster's decompressed plaintext, in bytes: a
/// cluster is a fixed number of logical blocks
/// (`COMPRESS_CLUSTER_BLOCKS`) and a block's content capacity is strictly below the
/// largest supported block size. A [`ClusterCache`] implementation uses
/// this as its per-entry validation bound, so the one definition lives
/// beside the trait rather than being re-derived by each host.
// The cluster block count is a small fixed constant (16), so narrowing
// it to usize cannot truncate on any supported target.
#[allow(clippy::cast_possible_truncation)]
pub const MAX_CLUSTER_PLAINTEXT: usize =
    (crate::COMPRESS_CLUSTER_BLOCKS as usize) * crate::MAX_BLOCK_SIZE;

/// Host-provided retention of verified, decrypted, decompressed
/// cluster plaintext. See the module docs for the coherence contract.
pub trait ClusterCache: Send {
    /// The retained plaintext of the cluster whose stored run starts at
    /// `phys`, or `None` when the cluster is not retained. The returned
    /// slice is the whole cluster (`ext.len` logical blocks of content
    /// capacity), exactly as `read_data_cluster` produced it.
    fn get(&mut self, phys: u64) -> Option<&[u8]>;

    /// Offer the cluster plaintext for retention. `stored` is the run
    /// length in physical blocks (`[phys, phys + stored)`), so the
    /// implementation can drop the entry when any block of the run is
    /// freed. Best-effort: the implementation may decline (over budget,
    /// under pressure, allocation failure) and the driver keeps serving.
    fn put(&mut self, phys: u64, stored: u64, plaintext: &[u8]);

    /// The stored block at `phys` was freed: drop (and zero) any entry
    /// whose run covers it.
    fn invalidate(&mut self, phys: u64);

    /// Drop (and zero) every entry: the fail-closed response to a
    /// transaction rollback.
    fn purge(&mut self);
}

/// Copy the covered byte range of a cached or freshly decompressed
/// cluster into the caller's buffer.
///
/// `plain` is the whole cluster's plaintext, `cluster_off` the byte
/// offset within it, and the copy length is bounded by both the
/// remaining plaintext and the caller's remaining request. Returns the
/// bytes copied. Shared by the hit and miss arms of the serving read
/// path so the arithmetic exists once.
pub(crate) fn copy_from_cluster(
    plain: &[u8],
    cluster_off: usize,
    out: &mut [u8],
    want: usize,
) -> usize {
    let chunk = plain.len().saturating_sub(cluster_off).min(want);
    out[..chunk].copy_from_slice(&plain[cluster_off..cluster_off + chunk]);
    chunk
}

/// Zero a transient plaintext buffer before it is dropped: the volumes
/// are encrypted at rest, so decompressed cluster bytes must not linger
/// in reusable heap memory once the read they served is finished. The
/// wipe is volatile (`zeroize`), so the compiler cannot elide it on the
/// grounds that the buffer is about to be freed.
pub(crate) fn scrub(mut buf: Vec<u8>) {
    buf.as_mut_slice().zeroize();
}
