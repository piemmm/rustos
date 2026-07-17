//! Interrupt-arrival-timing entropy source.
//!
//! A hardware RNG (RDRAND/RDSEED, `RNDR`, virtio-rng) and the CPU-timing
//! jitter source ([`crate::JitterSource`]) are both *synchronous* — the kernel
//! draws from them when it wants a seed. This source captures a different,
//! *asynchronous* physical process: the exact time at which external device
//! interrupts arrive. When a keystroke, disk completion, network packet, or
//! timer edge is delivered, an observer records the value of the platform's
//! high-resolution counter. The unpredictable low bits of the inter-arrival
//! timing — driven by device, bus, and human latency the CPU cannot predict —
//! are a classic, independent entropy input.
//!
//! Like the jitter source, this is **defense-in-depth**, mixed with the other
//! sources through [`crate::MixedPair`] and **never trusted alone**: XOR is
//! entropy-preserving for independent inputs, so it can only raise the seed's
//! quality, never lower it. Three rules keep the claim honest:
//!
//! * **Wait-free recording.** [`InterruptEntropyPool::record`] is a single
//!   atomic store into a ring, so the observer costs one counter read plus one
//!   store on the interrupt hot path — no lock, no allocation, no conditioning.
//! * **Freshness gate.** [`InterruptPoolSource::fill`] only contributes once
//!   the whole ring has been refilled with samples not yet drained; before
//!   then it fails closed with [`EntropyError::Unavailable`] (so at boot,
//!   before interrupts have flowed, the mix falls back to the other sources).
//! * **Health test fails closed.** A repetition-count test (NIST SP 800-90B
//!   §4.4.1) over the snapshot rejects a stuck/emulated counter that offers no
//!   timing variance, returning [`EntropyError::Unavailable`] rather than
//!   crediting predictable samples.
//!
//! Collected samples are conditioned with SHA-256 (via `lib/crypto`, never a
//! hand-rolled mixer) on the drain path; the running chain state is kept
//! separate from the emitted block and zeroised on the way out.

use core::sync::atomic::{AtomicU64, Ordering};

use zeroize::Zeroize;

use tairix_crypto::{sha256, SHA256_OUTPUT_LEN};

use crate::entropy::{EntropyError, EntropySource};

/// Number of samples retained in the ring. A power of two so the slot index is
/// a cheap mask, and wide enough that one drain conditions many independent
/// interrupt-timing samples.
const POOL_SLOTS: usize = 64;

/// Mask selecting a slot from the monotonic event counter.
const SLOT_MASK: u64 = POOL_SLOTS as u64 - 1;

/// Fresh samples required since the last drain before the source will
/// contribute. Requiring the whole ring to have turned over means a drain
/// never re-conditions samples it already emitted, and never contributes from
/// a ring that interrupts have barely touched.
const REQUIRED_FRESH_SAMPLES: u64 = POOL_SLOTS as u64;

/// Repetition-count cutoff (NIST SP 800-90B §4.4.1): if any single sample
/// value occupies this many or more of the [`POOL_SLOTS`] snapshot slots, the
/// timing source offers no usable variance (a lockstep/emulated counter) and
/// the source fails closed. A healthy high-resolution counter never repeats a
/// full sample value this often across a full ring.
const RCT_CUTOFF: usize = POOL_SLOTS / 2;

/// Domain-separation label folded into the conditioner before any sample, so
/// this source's output cannot collide with another SHA-256 use of the same
/// bytes.
const DOMAIN: &[u8; 8] = b"ROSIRQEP";

/// A shared, wait-free pool of recent interrupt-arrival timing samples.
///
/// The kernel installs one of these as a `'static` and feeds it from the
/// architecture-neutral interrupt-dispatch path (one counter read + one
/// [`Self::record`] per interrupt). An [`InterruptPoolSource`] drains and
/// conditions it on each CSPRNG (re)seed. All state is atomic so `record` is
/// safe to call from interrupt context on any CPU without a lock.
#[derive(Debug)]
pub struct InterruptEntropyPool {
    /// Most-recent sample per ring slot, overwritten as events arrive.
    slots: [AtomicU64; POOL_SLOTS],
    /// Total number of samples recorded since boot. Drives slot selection and
    /// the drain-side freshness gate; monotonic (wrapping).
    events: AtomicU64,
}

impl InterruptEntropyPool {
    /// Construct an empty pool. `const` so the kernel can place one in a
    /// `static` reachable from both the interrupt observer and the reseeding
    /// reserve.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            slots: [const { AtomicU64::new(0) }; POOL_SLOTS],
            events: AtomicU64::new(0),
        }
    }

    /// Record one interrupt-arrival timing `sample` (the platform
    /// high-resolution counter read at interrupt dispatch).
    ///
    /// Wait-free: a single relaxed increment of the event counter selects a
    /// slot and a single relaxed store overwrites it. Ordering is `Relaxed`
    /// because the pool is a statistical accumulator, not a synchronisation
    /// channel — a drain that races a store simply sees the old-or-new sample,
    /// both of which are valid entropy. Safe to call from interrupt context.
    pub fn record(&self, sample: u64) {
        let n = self.events.fetch_add(1, Ordering::Relaxed);
        let idx = (n & SLOT_MASK) as usize;
        self.slots[idx].store(sample, Ordering::Relaxed);
    }

    /// Number of samples recorded since boot (wrapping). Used by
    /// [`InterruptPoolSource`] to gate on freshness.
    #[must_use]
    pub fn event_count(&self) -> u64 {
        self.events.load(Ordering::Acquire)
    }

    /// Snapshot the ring into `out`.
    fn snapshot(&self, out: &mut [u64; POOL_SLOTS]) {
        for (dst, slot) in out.iter_mut().zip(self.slots.iter()) {
            *dst = slot.load(Ordering::Relaxed);
        }
    }
}

impl Default for InterruptEntropyPool {
    fn default() -> Self {
        Self::new()
    }
}

/// A draining [`EntropySource`] view over a shared [`InterruptEntropyPool`].
///
/// Owned by the reseeding reserve (so it must hold a `'static` reference to
/// the shared pool in the kernel). It tracks the event count at its last
/// successful drain so it only contributes once a full ring of fresh samples
/// has arrived, and fails closed otherwise — the "no single source is trusted
/// alone" mix simply falls back to the other sources.
pub struct InterruptPoolSource<'a> {
    pool: &'a InterruptEntropyPool,
    /// Event count observed at the last successful drain, so freshness is
    /// measured relative to what this source has already consumed.
    last_events: u64,
}

impl<'a> InterruptPoolSource<'a> {
    /// Build a draining source over `pool`.
    #[must_use]
    pub fn new(pool: &'a InterruptEntropyPool) -> Self {
        Self {
            pool,
            last_events: pool.event_count(),
        }
    }

    /// Repetition-count health test: reject if any single value fills
    /// [`RCT_CUTOFF`] or more of the snapshot slots (a stuck counter).
    fn repetition_ok(snapshot: &[u64; POOL_SLOTS]) -> bool {
        for (i, &value) in snapshot.iter().enumerate() {
            let mut count = 1usize;
            for &other in &snapshot[i + 1..] {
                if other == value {
                    count += 1;
                    if count >= RCT_CUTOFF {
                        return false;
                    }
                }
            }
        }
        true
    }

    /// Condition `snapshot` into `out` with a SHA-256 chain, emitting
    /// [`SHA256_OUTPUT_LEN`]-byte blocks derived from the chain and a block
    /// counter so the secret chain state never leaves the source.
    fn condition(snapshot: &[u64; POOL_SLOTS], out: &mut [u8]) {
        let mut chain = sha256(DOMAIN);
        let mut buf = [0u8; SHA256_OUTPUT_LEN + 8];
        // Fold every sample into the running chain.
        for &sample in snapshot {
            buf[..SHA256_OUTPUT_LEN].copy_from_slice(&chain);
            buf[SHA256_OUTPUT_LEN..].copy_from_slice(&sample.to_le_bytes());
            chain = sha256(&buf);
        }
        let mut produced = 0usize;
        let mut block_index: u64 = 0;
        while produced < out.len() {
            let block_len = core::cmp::min(SHA256_OUTPUT_LEN, out.len() - produced);
            buf[..SHA256_OUTPUT_LEN].copy_from_slice(&chain);
            buf[SHA256_OUTPUT_LEN..].copy_from_slice(&block_index.to_le_bytes());
            let block = sha256(&buf);
            out[produced..produced + block_len].copy_from_slice(&block[..block_len]);
            produced += block_len;
            block_index += 1;
        }
        chain.zeroize();
        buf.zeroize();
    }
}

impl EntropySource for InterruptPoolSource<'_> {
    fn fill(&mut self, out: &mut [u8]) -> Result<(), EntropyError> {
        if out.is_empty() {
            return Ok(());
        }
        let events = self.pool.event_count();
        // Freshness gate: contribute only once the whole ring has turned over
        // with samples this source has not already drained. `wrapping_sub`
        // handles the monotonic counter wrapping.
        if events.wrapping_sub(self.last_events) < REQUIRED_FRESH_SAMPLES {
            return Err(EntropyError::Unavailable);
        }
        let mut snapshot = [0u64; POOL_SLOTS];
        self.pool.snapshot(&mut snapshot);
        if !Self::repetition_ok(&snapshot) {
            snapshot.zeroize();
            return Err(EntropyError::Unavailable);
        }
        Self::condition(&snapshot, out);
        snapshot.zeroize();
        self.last_events = events;
        Ok(())
    }
    // `fill_blocking` uses the default (delegates to `fill`): there is no pool
    // to park on — either enough fresh interrupts have arrived or they have
    // not, and in the mix a miss simply falls back to the other sources.
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feed `count` samples from an LCG (a stand-in for a healthy
    /// high-resolution counter whose successive readings vary).
    fn feed_varying(pool: &InterruptEntropyPool, seed: u64, count: usize) {
        let mut lcg = seed;
        for _ in 0..count {
            lcg = lcg.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            pool.record(lcg);
        }
    }

    #[test]
    fn record_advances_the_event_count() {
        let pool = InterruptEntropyPool::new();
        assert_eq!(pool.event_count(), 0);
        pool.record(42);
        pool.record(43);
        assert_eq!(pool.event_count(), 2);
    }

    #[test]
    fn fails_closed_before_the_ring_is_fresh() {
        // A source constructed on an empty pool must not contribute until a
        // full ring of fresh samples has arrived (at boot, before interrupts
        // flow, the mix falls back to the other sources).
        let pool = InterruptEntropyPool::new();
        let mut src = InterruptPoolSource::new(&pool);
        let mut out = [0u8; 16];
        assert_eq!(src.fill(&mut out), Err(EntropyError::Unavailable));
        // One short of the whole ring is still not enough.
        feed_varying(&pool, 0x1234, POOL_SLOTS - 1);
        assert_eq!(src.fill(&mut out), Err(EntropyError::Unavailable));
    }

    #[test]
    fn contributes_once_a_full_fresh_ring_has_arrived() {
        let pool = InterruptEntropyPool::new();
        let mut src = InterruptPoolSource::new(&pool);
        feed_varying(&pool, 0x9E37_79B9, POOL_SLOTS);
        let mut out = [0u8; 32];
        src.fill(&mut out).expect("a full fresh ring contributes");
        assert_ne!(out, [0u8; 32], "conditioned output must not be all-zero");
    }

    #[test]
    fn will_not_re_drain_without_new_samples() {
        // After a successful drain, draining again without any new interrupts
        // must fail closed — a drain never re-conditions stale samples.
        let pool = InterruptEntropyPool::new();
        let mut src = InterruptPoolSource::new(&pool);
        feed_varying(&pool, 0xDEAD_BEEF, POOL_SLOTS);
        let mut out = [0u8; 16];
        src.fill(&mut out).expect("first drain contributes");
        assert_eq!(
            src.fill(&mut out),
            Err(EntropyError::Unavailable),
            "no fresh samples ⇒ fail closed"
        );
        // Another full ring of fresh samples lets it contribute again, and the
        // output differs from the first drain.
        let mut again = [0u8; 16];
        feed_varying(&pool, 0xFEED_FACE, POOL_SLOTS);
        src.fill(&mut again).expect("second fresh ring contributes");
        assert_ne!(out, again, "successive drains must differ");
    }

    #[test]
    fn stuck_counter_fails_closed() {
        // A counter that yields the same value on every interrupt offers no
        // timing variance: the repetition-count test must reject it rather
        // than credit predictable samples.
        let pool = InterruptEntropyPool::new();
        let mut src = InterruptPoolSource::new(&pool);
        for _ in 0..POOL_SLOTS {
            pool.record(0xABCD_ABCD);
        }
        let mut out = [0u8; 16];
        assert_eq!(src.fill(&mut out), Err(EntropyError::Unavailable));
    }

    #[test]
    fn output_spans_multiple_sha256_blocks() {
        // A 70-byte request crosses three 32-byte conditioner blocks; prove
        // the whole buffer is filled and the blocks differ.
        let pool = InterruptEntropyPool::new();
        let mut src = InterruptPoolSource::new(&pool);
        feed_varying(&pool, 0xC0FF_EE11, POOL_SLOTS);
        let mut out = [0u8; 70];
        src.fill(&mut out).expect("fresh ring contributes");
        assert_ne!(&out[0..32], &[0u8; 32], "block 0 filled");
        assert_ne!(&out[32..64], &[0u8; 32], "block 1 filled");
        assert_ne!(&out[64..70], &[0u8; 6], "tail block filled");
        assert_ne!(&out[0..32], &out[32..64], "blocks must differ");
    }

    #[test]
    fn empty_request_is_ok() {
        let pool = InterruptEntropyPool::new();
        let mut src = InterruptPoolSource::new(&pool);
        let mut out = [0u8; 0];
        assert_eq!(src.fill(&mut out), Ok(()));
    }

    #[test]
    fn slot_wrap_keeps_recording() {
        // Recording more than a full ring must keep advancing the count and
        // overwrite oldest slots (no panic on wrap).
        let pool = InterruptEntropyPool::new();
        feed_varying(&pool, 1, POOL_SLOTS * 3 + 7);
        assert_eq!(pool.event_count(), (POOL_SLOTS * 3 + 7) as u64);
    }
}
