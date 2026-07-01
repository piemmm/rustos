//! CPU timing-jitter entropy source.
//!
//! A hardware RNG (RDRAND/RDSEED, ARMv8.5 `RNDR`, virtio-rng) is a single
//! opaque source: if it is backdoored, stuck, or merely observable, a
//! generator seeded from it alone inherits that weakness. The charter forbids
//! trusting one source, so RustOS mixes an **independent** software source
//! into the seed — this one — via [`crate::MixedPair`] before it ever reaches
//! [`crate::CsRng`]. XOR-mixing is entropy-preserving for independent inputs,
//! so even a conservative amount of genuine timing jitter raises the bar
//! against a compromised hardware source, and can never lower it.
//!
//! # Where the entropy comes from
//!
//! The unpredictability is the *variation in execution time* of a fixed
//! workload, measured with a high-resolution monotonic counter
//! ([`TimeSource`], the platform cycle/counter register the kernel exposes
//! through the Arch HAL). Cache state, branch prediction, memory-bus
//! contention, interrupts, and DVFS make successive runs of the same code
//! take measurably different numbers of counter ticks. This is the
//! well-studied CPU-jitter mechanism (Müller's `jitterentropy`).
//!
//! # Honest, conservative accounting
//!
//! Timing jitter is treated as **defense-in-depth**, not the primary entropy
//! source — the hardware RNG remains that. Three rules keep the claim honest:
//!
//! * **Only non-stuck samples are credited.** Each raw timing delta is run
//!   through a "stuck" test (its first/second/third discrete derivatives): a
//!   sample that a deterministic clock could have produced contributes to the
//!   conditioner but is not *counted* toward the entropy budget.
//! * **Heavy oversampling.** Several credited samples are folded per output
//!   *bit* (`OVERSAMPLE_PER_BIT`, many per byte), so the conditioned output is at
//!   full entropy even if each sample carries well under one bit of
//!   min-entropy.
//! * **Health tests fail closed.** A repetition-count test (NIST SP 800-90B
//!   §4.4.1) and a bounded attempt budget mean a clock with no usable jitter —
//!   an emulator with a lockstep counter, a deterministic host test — returns
//!   [`EntropyError::Unavailable`] rather than manufacturing entropy or
//!   looping forever. In the mix that simply falls back to the hardware
//!   source, and if *both* are unavailable the seed fails closed (never weak).
//!
//! Collected timing samples are conditioned with SHA-256 (via `lib/crypto`,
//! never a hand-rolled mixer) into the requested output; the running chain
//! state is kept separate from the emitted block and zeroised on the way out.

use zeroize::Zeroize;

use rustos_crypto::{sha256, SHA256_OUTPUT_LEN};

use crate::entropy::{EntropyError, EntropySource};

/// A high-resolution monotonic counter the jitter source samples.
///
/// The platform supplies this — the x86 time-stamp counter (`RDTSC`), the
/// aarch64 physical counter (`CNTPCT_EL0`), the riscv64 `time`/`cycle` CSR —
/// so `lib/rng` stays architecture-neutral (the target-specific read lives in
/// `kernel/arch/<target>`). The counter must be monotonic and as
/// high-resolution as the hardware offers; a coarse or deterministic counter
/// simply yields "stuck" samples and the source fails closed (see the module
/// docs), which is correct rather than dangerous.
pub trait TimeSource {
    /// Read the current counter value.
    fn now(&mut self) -> u64;
}

impl<F: FnMut() -> u64> TimeSource for F {
    fn now(&mut self) -> u64 {
        self()
    }
}

/// Credited (non-stuck) timing samples folded per output **bit**.
///
/// Conservative oversampling: even if a single measurement carries well under
/// one bit of min-entropy, folding this many per bit keeps the SHA-256
/// conditioned output at full entropy. Chosen for a comfortable margin over
/// the ~1 bit/sample a memory-walk workload yields in practice, while keeping
/// a boot-time seed draw to a few milliseconds.
const OVERSAMPLE_PER_BIT: usize = 4;

/// Credited samples per output byte (`8 * OVERSAMPLE_PER_BIT`).
const SAMPLES_PER_BYTE: usize = 8 * OVERSAMPLE_PER_BIT;

/// Consecutive-identical-delta cutoff for the repetition-count health test
/// (NIST SP 800-90B §4.4.1). Reaching it means the counter is producing the
/// same value over and over — no jitter — so the source fails closed. The
/// cutoff is deliberately generous: real jitter never repeats a full delta
/// this many times in a row, so a healthy source never trips it, while a
/// stuck/deterministic counter trips it quickly.
const RCT_CUTOFF: u32 = 32;

/// Upper bound on measurement *attempts* per output byte before the source
/// gives up. A deterministic clock yields only stuck samples that never get
/// credited; rather than loop forever the source fails closed once it has
/// tried this many times without collecting its quota. Generous enough that a
/// healthy source (which credits almost every sample) never reaches it.
const ATTEMPT_BUDGET_PER_BYTE: usize = SAMPLES_PER_BYTE * 64;

/// Length of the scratch buffer whose data-dependent walk gives the timed
/// workload its cache/timing variance. A power of two so the index mask is a
/// cheap `& (LEN - 1)`.
const SCRATCH_LEN: usize = 128;

/// Iterations of the data-dependent memory walk per measurement. Enough to
/// expose cache/pipeline variance between the two counter reads, few enough
/// that a measurement is cheap.
const WORKLOAD_ITERS: usize = 16;

/// Domain-separation label folded into the conditioner before any sample, so
/// this source's output cannot collide with another SHA-256 use of the same
/// bytes.
const DOMAIN: &[u8; 8] = b"ROSJITTR";

/// A CPU-timing-jitter [`EntropySource`].
///
/// See the [module docs](self) for the mechanism and the honest, conservative
/// accounting. Construct it over a platform [`TimeSource`] and mix it with the
/// hardware RNG through [`crate::MixedPair`]; never rely on it as the sole
/// source.
pub struct JitterSource<T: TimeSource> {
    time: T,
    /// Scratch memory the timed workload walks (data-dependently) to create
    /// microarchitectural timing variance.
    scratch: [u8; SCRATCH_LEN],
    /// State of the workload's data-dependent walk, carried between
    /// measurements so the access pattern does not repeat.
    walk: u64,
    /// Previous raw delta and its first derivative, for the stuck test.
    last_delta: u64,
    last_delta2: u64,
    /// Repetition-count health-test state: the last raw delta and how many
    /// times in a row it has now occurred.
    rct_last: u64,
    rct_count: u32,
    /// Whether any sample has been taken yet (so the first sample seeds the
    /// health-test state rather than comparing against a zero it never saw).
    primed: bool,
}

impl<T: TimeSource> JitterSource<T> {
    /// Create a jitter source over `time`.
    pub fn new(time: T) -> Self {
        Self {
            time,
            scratch: [0u8; SCRATCH_LEN],
            walk: 0,
            last_delta: 0,
            last_delta2: 0,
            rct_last: 0,
            rct_count: 0,
            primed: false,
        }
    }

    /// Time one run of the data-dependent memory workload, returning the
    /// counter-tick delta.
    ///
    /// The workload reads and writes `scratch` at a data-dependent index so
    /// the compiler cannot hoist or elide it and the CPU's cache/pipeline
    /// state genuinely varies the timing between the two [`TimeSource::now`]
    /// reads.
    fn measure(&mut self) -> u64 {
        let start = self.time.now();
        let mut w = self.walk;
        for _ in 0..WORKLOAD_ITERS {
            // Index from a low byte of the walk state (byte extraction, not a
            // truncating cast) masked into range; `SCRATCH_LEN` is a power of
            // two so the mask is exact.
            let idx = usize::from(w.to_le_bytes()[0]) & (SCRATCH_LEN - 1);
            w = w
                .wrapping_add(u64::from(self.scratch[idx]))
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1);
            // Write back a high byte so the access is a genuine load+store the
            // optimiser must keep and the next index depends on memory state.
            self.scratch[idx] = w.to_le_bytes()[5];
        }
        self.walk = w;
        let end = self.time.now();
        end.wrapping_sub(start)
    }

    /// The "stuck" test: a sample whose value, first, second, or third
    /// discrete derivative is zero could have come from a deterministic
    /// counter, so it carries no *credited* entropy (it is still folded into
    /// the conditioner, harmlessly). Updates the derivative history.
    fn is_stuck(&mut self, delta: u64) -> bool {
        let delta2 = delta.wrapping_sub(self.last_delta);
        let delta3 = delta2.wrapping_sub(self.last_delta2);
        self.last_delta = delta;
        self.last_delta2 = delta2;
        if !self.primed {
            // No prior sample to derive against; treat the first as stuck so
            // it seeds the history without being credited.
            self.primed = true;
            return true;
        }
        delta == 0 || delta2 == 0 || delta3 == 0
    }

    /// Repetition-count health test (NIST SP 800-90B §4.4.1) over raw deltas.
    /// Returns `false` (fail closed) once the same delta has recurred
    /// [`RCT_CUTOFF`] times in a row — a sign the counter offers no jitter.
    fn repetition_ok(&mut self, delta: u64) -> bool {
        if self.rct_count != 0 && delta == self.rct_last {
            self.rct_count += 1;
            self.rct_count < RCT_CUTOFF
        } else {
            self.rct_last = delta;
            self.rct_count = 1;
            true
        }
    }

    /// Gather timing samples and condition them into `out` with SHA-256.
    ///
    /// Produces output in [`SHA256_OUTPUT_LEN`]-byte blocks; each block folds
    /// [`SAMPLES_PER_BYTE`] credited (non-stuck) samples per output byte into
    /// a running chain, then derives the emitted block from the chain and a
    /// block counter so the secret chain state never leaves the source.
    fn gather(&mut self, out: &mut [u8]) -> Result<(), EntropyError> {
        // The running conditioner chain, primed with the domain label.
        let mut chain = sha256(DOMAIN);
        // Scratch for the fold `sha256(chain || 8 bytes)`.
        let mut buf = [0u8; SHA256_OUTPUT_LEN + 8];

        let mut produced = 0usize;
        let mut block_index: u64 = 0;
        while produced < out.len() {
            let block_len = core::cmp::min(SHA256_OUTPUT_LEN, out.len() - produced);
            let needed = SAMPLES_PER_BYTE * block_len;
            let budget = ATTEMPT_BUDGET_PER_BYTE * block_len;
            let mut credited = 0usize;
            let mut attempts = 0usize;

            while credited < needed {
                if attempts >= budget {
                    // The counter offered too little jitter to credit a full
                    // block within the budget: fail closed rather than
                    // manufacture entropy or loop.
                    chain.zeroize();
                    buf.zeroize();
                    return Err(EntropyError::Unavailable);
                }
                attempts += 1;

                let delta = self.measure();
                if !self.repetition_ok(delta) {
                    chain.zeroize();
                    buf.zeroize();
                    return Err(EntropyError::Unavailable);
                }
                let stuck = self.is_stuck(delta);

                // Fold every sample into the chain (folding a stuck sample is
                // harmless); only credit non-stuck ones toward the quota.
                buf[..SHA256_OUTPUT_LEN].copy_from_slice(&chain);
                buf[SHA256_OUTPUT_LEN..].copy_from_slice(&delta.to_le_bytes());
                chain = sha256(&buf);
                if !stuck {
                    credited += 1;
                }
            }

            // Derive the emitted block from the chain and the block counter,
            // keeping the chain itself secret for the next block.
            buf[..SHA256_OUTPUT_LEN].copy_from_slice(&chain);
            buf[SHA256_OUTPUT_LEN..].copy_from_slice(&block_index.to_le_bytes());
            let block = sha256(&buf);
            out[produced..produced + block_len].copy_from_slice(&block[..block_len]);
            produced += block_len;
            block_index += 1;
        }

        chain.zeroize();
        buf.zeroize();
        Ok(())
    }
}

impl<T: TimeSource> EntropySource for JitterSource<T> {
    fn fill(&mut self, out: &mut [u8]) -> Result<(), EntropyError> {
        if out.is_empty() {
            return Ok(());
        }
        self.gather(out)
    }
    // `fill_blocking` uses the default (delegates to `fill`): jitter is a
    // synchronous CPU measurement with no pool to wait on — it either has
    // usable timing variance now or it does not.
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A time source whose deltas vary strongly (an LCG driving the
    /// increment), standing in for a healthy high-resolution counter. Two
    /// consecutive equal deltas — and therefore a stuck classification — are
    /// astronomically unlikely, so the health path is reliably exercised
    /// without flakiness.
    struct VaryingClock {
        now: u64,
        lcg: u64,
    }

    impl VaryingClock {
        fn new(seed: u64) -> Self {
            Self { now: 0, lcg: seed }
        }
    }

    impl TimeSource for VaryingClock {
        fn now(&mut self) -> u64 {
            // Advance by a pseudo-random, always-positive increment so each
            // measured delta (two `now` calls straddling the workload) is
            // effectively random.
            self.lcg = self
                .lcg
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1);
            self.now = self.now.wrapping_add((self.lcg >> 40) | 1);
            self.now
        }
    }

    /// A time source that advances by a fixed step: every measured delta is
    /// identical (a lockstep/deterministic counter), so the source must fail
    /// closed rather than credit anything.
    struct LockstepClock {
        now: u64,
    }

    impl TimeSource for LockstepClock {
        fn now(&mut self) -> u64 {
            self.now = self.now.wrapping_add(1);
            self.now
        }
    }

    #[test]
    fn healthy_clock_produces_non_zero_output() {
        let mut src = JitterSource::new(VaryingClock::new(0x1234_5678));
        let mut out = [0u8; 48];
        src.fill(&mut out).expect("a varying clock yields entropy");
        assert_ne!(out, [0u8; 48], "conditioned output must not be all-zero");
    }

    #[test]
    fn two_draws_differ() {
        let mut src = JitterSource::new(VaryingClock::new(0x9E37_79B9));
        let (mut a, mut b) = ([0u8; 32], [0u8; 32]);
        src.fill(&mut a).unwrap();
        src.fill(&mut b).unwrap();
        assert_ne!(a, b, "successive jitter draws must differ");
    }

    #[test]
    fn output_spans_multiple_sha256_blocks() {
        // A 70-byte request crosses three 32-byte conditioner blocks; prove
        // the whole buffer is filled (no block left zeroed) and each block
        // differs from the others.
        let mut src = JitterSource::new(VaryingClock::new(0xDEAD_BEEF));
        let mut out = [0u8; 70];
        src.fill(&mut out).unwrap();
        assert_ne!(&out[0..32], &[0u8; 32], "block 0 filled");
        assert_ne!(&out[32..64], &[0u8; 32], "block 1 filled");
        assert_ne!(&out[64..70], &[0u8; 6], "tail block filled");
        assert_ne!(&out[0..32], &out[32..64], "blocks must differ");
    }

    #[test]
    fn deterministic_clock_fails_closed() {
        // A lockstep counter offers no jitter: every delta is identical, so
        // the source must return `Unavailable` (never manufacture entropy,
        // never loop forever).
        let mut src = JitterSource::new(LockstepClock { now: 0 });
        let mut out = [0u8; 8];
        assert_eq!(src.fill(&mut out), Err(EntropyError::Unavailable));
    }

    #[test]
    fn empty_request_is_ok() {
        let mut src = JitterSource::new(VaryingClock::new(1));
        let mut out = [0u8; 0];
        assert_eq!(src.fill(&mut out), Ok(()));
    }

    #[test]
    fn repetition_count_test_trips_on_constant_delta() {
        // Directly exercise the RCT: feeding the same delta `RCT_CUTOFF`
        // times in a row must fail the test.
        let mut src = JitterSource::new(VaryingClock::new(1));
        // First observation seeds the state and is OK.
        assert!(src.repetition_ok(42));
        // It stays OK until the count reaches the cutoff.
        let mut tripped = false;
        for _ in 1..RCT_CUTOFF {
            if !src.repetition_ok(42) {
                tripped = true;
                break;
            }
        }
        assert!(
            tripped,
            "constant delta must trip the repetition-count test"
        );
    }

    #[test]
    fn repetition_count_resets_on_change() {
        let mut src = JitterSource::new(VaryingClock::new(1));
        for _ in 0..(RCT_CUTOFF - 1) {
            assert!(src.repetition_ok(7));
        }
        // A different delta resets the run, so the test does not trip.
        assert!(src.repetition_ok(8));
        assert!(src.repetition_ok(8));
    }
}
