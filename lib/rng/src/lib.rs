//! RustOS random number generation.
//!
//! This crate is the one place the rest of RustOS gets randomness from. It
//! provides three layers, separated by purpose:
//!
//! * [`CsRng`] — the **cryptographically secure** generator. A NIST SP
//!   800-90Ar1 HMAC-SHA256 DRBG ([`drbg::HmacDrbg`]) that reseeds from a
//!   pluggable [`EntropySource`] for forward secrecy. This is what `RustFS`
//!   keys, the encrypted-swap key, nonces, and the
//!   KASLR/ASLR seed must use. Its draws are fallible and fail closed — they never block, spin, or panic.
//! * [`FastRng`] — a **fast, non-cryptographic** generator (xoshiro256++) for
//!   bulk randomness with no security requirement (scheduler decisions,
//!   collection seeds, backoff jitter). Never use it for keys or nonces.
//! * [`hardware::PlatformFast`] — a fast generator that prefers a motherboard
//!   **hardware RNG** ([`hardware::HardwareRng`]) when one is present and
//!   falls back to [`FastRng`] when it is not (or transiently fails).
//!
//! # The cryptographic core is composed, not hand-rolled
//!
//! Per no cryptographic primitive is hand-rolled here.
//! The DRBG is the standard HMAC-DRBG construction layered over `lib/crypto`'s
//! audited HMAC-SHA256, exactly as `lib/crypto`'s `kdf` layers HKDF-Expand
//! over the same PRF. The only first-party algorithm is the *non-cryptographic*
//! xoshiro256++, which is an ordinary PRNG, not a security primitive.
//!
//! # Hardware RNG: extra entropy *and* a fast source
//!
//! A platform hardware source ([`hardware::HardwareRng`], supplied by
//! `kernel/arch/<target>` or a `drivers/*` crate — it is never probed here,
//! keeping the crate architecture-neutral) is used two ways:
//!
//! 1. As an *additional* entropy input: wrap it in
//!    [`hardware::HardwareEntropy`] and XOR-mix it with the other platform
//!    sources via [`CombinedSource`] before it ever feeds [`CsRng`]. It is one
//!    independent input among several, never the sole trusted source.
//! 2. As a fast source: [`hardware::PlatformFast`] draws from it directly,
//!    falling back to the software [`FastRng`] when absent or failing.
//!
//! # Worked wiring
//!
//! ```
//! use rustos_rng::{CombinedSource, CsRng, EntropyError, EntropySource, JitterSource};
//! use rustos_rng::hardware::{HardwareEntropy, HardwareRng, PlatformFast};
//! use rustos_rng::RandU64;
//!
//! # struct Rdrand;
//! # impl HardwareRng for Rdrand {
//! #     fn try_fill(&self, out: &mut [u8]) -> Result<(), EntropyError> {
//! #         for (i, b) in out.iter_mut().enumerate() { *b = i as u8 ^ 0xA5; }
//! #         Ok(())
//! #     }
//! # }
//! # fn wire(hw: Option<Rdrand>) -> Result<(), EntropyError> {
//! // Mix a hardware source (if present) with timing jitter into one pool…
//! let rdrand = Rdrand;
//! // `JitterSource` samples a high-resolution counter; supply the platform's
//! // (here a stand-in monotonic counter with varying deltas).
//! let (mut lcg, mut now) = (0x1234_5678u64, 0u64);
//! let mut jitter = JitterSource::new(move || {
//!     lcg = lcg.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
//!     now = now.wrapping_add((lcg >> 40) | 1);
//!     now
//! });
//! let mut hw_entropy = HardwareEntropy::new(&rdrand);
//! let mut sources: [&mut dyn EntropySource; 2] = [&mut hw_entropy, &mut jitter];
//! let pool = CombinedSource::new(&mut sources);
//!
//! // …and seed the cryptographic generator from the combined pool.
//! let mut csrng = CsRng::new(pool)?;
//! let mut key = [0u8; 32];
//! csrng.try_fill_bytes(&mut key)?;
//!
//! // A fast, hardware-preferring generator, seeded unpredictably.
//! let mut fast = PlatformFast::new(hw, csrng.try_next_u64()?);
//! let _coin = fast.next_u64() & 1;
//! # Ok(())
//! # }
//! ```

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

pub mod csprng;
pub mod drbg;
pub mod entropy;
pub mod fast;
pub mod hardware;
pub mod interrupt;
pub mod jitter;
pub mod rand;
pub mod reserve;

pub use csprng::{CsRng, DEFAULT_RESEED_INTERVAL};
pub use drbg::{DrbgError, HmacDrbg, DRBG_OUTLEN, RESEED_INTERVAL};
pub use entropy::{CombinedSource, EntropyError, EntropySource, MixedPair};
pub use fast::FastRng;
pub use hardware::{HardwareEntropy, HardwareRng, PlatformFast};
pub use interrupt::{InterruptEntropyPool, InterruptPoolSource};
pub use jitter::{JitterSource, TimeSource};
pub use rand::RandU64;
pub use reserve::{OutputReserve, ReserveError, DEFAULT_RESERVE_BYTES};
