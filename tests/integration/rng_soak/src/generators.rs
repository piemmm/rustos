//! The generators the battery runs over: the two real ones, and the two
//! known-bad controls that keep it honest.
//!
//! `NonCryptoRng` is deliberately absent. It is predictable by design and
//! passes this battery comfortably — xoshiro's `++` scrambler is nonlinear,
//! and its state is wider than the matrix-rank test's span — which is the
//! whole reason the two-tier split exists: statistical quality says nothing
//! about unpredictability, so a battery is the wrong instrument for judging
//! that type. Its structural properties are unit-tested where it lives.

use tairix_rng::{CsRng, EntropyError, EntropySource, FastRng, RandU64, STREAM_KEY_LEN};

/// A byte source the battery can draw a sequence from.
pub trait Stream {
    /// Fill `out` with the generator's next bytes.
    ///
    /// # Errors
    /// Returns a description if the generator cannot produce bytes, which for
    /// the DRBG means its entropy source failed.
    fn fill(&mut self, out: &mut [u8]) -> Result<(), String>;
}

/// Deterministic stand-in for a platform entropy source: a counter mixed
/// wide enough that each fill differs.
///
/// Not entropy, and never used as any. It is what makes a battery run
/// reproducible from its logged seed, which is the only way a failure over
/// millions of sequences can be investigated.
struct SeededEntropy {
    state: u64,
}

impl EntropySource for SeededEntropy {
    fn fill(&mut self, out: &mut [u8]) -> Result<(), EntropyError> {
        for byte in out.iter_mut() {
            self.state = self
                .state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1);
            *byte = self.state.to_le_bytes()[4];
        }
        Ok(())
    }
}

/// `tairix_rng::FastRng` — buffered `ChaCha12` with fast key erasure.
struct Fast(FastRng);

impl Stream for Fast {
    fn fill(&mut self, out: &mut [u8]) -> Result<(), String> {
        self.0.fill_bytes(out);
        Ok(())
    }
}

/// `tairix_rng::CsRng` — the HMAC-SHA256 DRBG.
struct Cs(CsRng<SeededEntropy>);

impl Stream for Cs {
    fn fill(&mut self, out: &mut [u8]) -> Result<(), String> {
        self.0
            .try_fill_bytes(out)
            .map_err(|e| format!("the DRBG could not produce bytes: {e:?}"))
    }
}

/// A maximal-length 31-bit Fibonacci LFSR over `x^31 + x^28 + 1`.
///
/// The negative control for [`crate::statistics::binary_matrix_rank`]. Its
/// output is statistically excellent — an m-sequence has ideal balance and an
/// ideal run-length distribution — and every bit of it is a linear function
/// of 31 state bits, so a 32-bit-wide matrix built from it can never reach
/// full rank. The degree is *deliberately* below the matrix span: a wider
/// register would hide the linearity from a 32x32 test and make the control
/// vacuous.
struct Lfsr {
    state: u32,
}

impl Lfsr {
    /// Seed the register, avoiding the all-zero state it cannot leave.
    fn new(seed: u64) -> Self {
        let state = u32::try_from(seed & 0x7fff_ffff).unwrap_or(1);
        Self {
            state: if state == 0 { 1 } else { state },
        }
    }

    fn next_bit(&mut self) -> u32 {
        let bit = ((self.state >> 30) ^ (self.state >> 27)) & 1;
        self.state = ((self.state << 1) | bit) & 0x7fff_ffff;
        bit
    }
}

impl Stream for Lfsr {
    fn fill(&mut self, out: &mut [u8]) -> Result<(), String> {
        for byte in out.iter_mut() {
            let mut packed = 0u8;
            for _ in 0..8 {
                packed = (packed << 1) | u8::try_from(self.next_bit()).unwrap_or(0);
            }
            *byte = packed;
        }
        Ok(())
    }
}

/// A bare incrementing counter, written little-endian.
///
/// The negative control for everything the LFSR passes, and a real bug rather
/// than a contrived one: reaching for a counter where randomness was wanted
/// is the classic mistake, and a battery that did not reject it would be
/// measuring nothing at all.
struct Counter {
    next: u64,
}

impl Stream for Counter {
    fn fill(&mut self, out: &mut [u8]) -> Result<(), String> {
        for chunk in out.chunks_mut(8) {
            let bytes = self.next.to_le_bytes();
            chunk.copy_from_slice(&bytes[..chunk.len()]);
            self.next = self.next.wrapping_add(1);
        }
        Ok(())
    }
}

/// The generators `cargo xtask rngsoak` can soak. The single source of truth
/// for `--list` and the `soak.sh` fan-out, so neither hard-codes the list.
pub const TARGETS: &[&str] = &["fast", "csprng"];

/// The known-bad generators every statistic is held against. Not soak
/// targets: soaking a generator that must be rejected proves nothing beyond
/// the first batch.
pub const CONTROLS: &[&str] = &["lfsr", "counter"];

/// Build a generator by name, from `seed`.
///
/// # Errors
/// Returns a description for an unknown name, or when the DRBG cannot be
/// instantiated.
pub fn build(name: &str, seed: u64) -> Result<Box<dyn Stream>, String> {
    match name {
        "fast" => {
            let mut key = [0u8; STREAM_KEY_LEN];
            SeededEntropy { state: seed }
                .fill(&mut key)
                .map_err(|e| format!("seeding the fast generator failed: {e:?}"))?;
            Ok(Box::new(Fast(FastRng::from_key(&key))))
        }
        "csprng" => {
            let rng = CsRng::new(SeededEntropy { state: seed })
                .map_err(|e| format!("instantiating the DRBG failed: {e:?}"))?;
            Ok(Box::new(Cs(rng)))
        }
        "lfsr" => Ok(Box::new(Lfsr::new(seed))),
        "counter" => Ok(Box::new(Counter { next: seed })),
        other => Err(format!(
            "rngsoak: unknown generator `{other}`; known: {}, controls: {}",
            TARGETS.join(", "),
            CONTROLS.join(", ")
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::{build, CONTROLS, TARGETS};

    #[test]
    fn every_name_builds_and_produces_bytes() {
        for name in TARGETS.iter().chain(CONTROLS) {
            let mut g = build(name, 1).expect("a registered generator builds");
            let mut out = [0u8; 64];
            g.fill(&mut out).expect("it produces bytes");
            assert!(out.iter().any(|&b| b != 0), "{name} produced only zeros");
        }
    }

    #[test]
    fn an_unknown_generator_fails_closed() {
        let err = build("mt19937", 1)
            .err()
            .expect("unknown names must not build");
        for known in TARGETS.iter().chain(CONTROLS) {
            assert!(err.contains(known), "the error should list {known}: {err}");
        }
    }

    #[test]
    fn each_generator_is_reproducible_from_its_seed_and_differs_across_seeds() {
        for name in TARGETS.iter().chain(CONTROLS) {
            let draw = |seed| {
                let mut g = build(name, seed).expect("builds");
                let mut out = [0u8; 256];
                g.fill(&mut out).expect("produces");
                out
            };
            assert_eq!(draw(7), draw(7), "{name} is not reproducible");
            assert_ne!(draw(7), draw(8), "{name} ignores its seed");
        }
    }

    /// A generator's own `fill` must be one continuous stream, not a restart
    /// per call, or every sequence the battery draws would be the same one.
    #[test]
    fn successive_fills_continue_the_stream() {
        for name in TARGETS.iter().chain(CONTROLS) {
            let mut g = build(name, 3).expect("builds");
            let (mut first, mut second) = ([0u8; 128], [0u8; 128]);
            g.fill(&mut first).expect("produces");
            g.fill(&mut second).expect("produces");
            assert_ne!(first, second, "{name} restarts its stream every call");
        }
    }

    /// The LFSR control is only a control if its register is narrower than
    /// the matrix-rank test's span; a 32-bit-or-wider one would pass.
    #[test]
    fn the_lfsr_control_is_narrower_than_the_rank_test_span() {
        let mut g = super::Lfsr::new(1);
        // The register is 31 bits, so its state never sets bit 31.
        for _ in 0..1_000 {
            g.next_bit();
            assert_eq!(g.state >> 31, 0);
        }
    }
}
