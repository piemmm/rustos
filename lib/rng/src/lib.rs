//! TAIRiX random number generation.
//!
//! This crate is the one place the rest of TAIRiX gets randomness from. It
//! provides three generators, named for the property that decides which one a
//! call site may use:
//!
//! * [`CsRng`] — **cryptographically secure**, for long-lived key material. A
//!   NIST SP 800-90Ar1 HMAC-SHA256 DRBG ([`drbg::HmacDrbg`]) reseeded from a
//!   pluggable [`EntropySource`], so a state compromise cannot predict output
//!   indefinitely. `ARXFS` volume keys, the encrypted-swap key, and the
//!   KASLR/ASLR seed come from here. Its draws are fallible and fail closed —
//!   they never block, spin, or panic.
//! * [`FastRng`] — **fast and unpredictable**, for everything that should not
//!   be guessable but is not long-lived key material: task ids, a network
//!   payload, the kernel's userland-facing output reserve. Buffered `ChaCha12`
//!   with fast key erasure over `lib/crypto`, roughly forty times cheaper than
//!   the DRBG per byte and backtracking-resistant.
//! * [`NonCryptoRng`] — **fast and predictable** (xoshiro256++). Statistically
//!   excellent and trivially invertible, so it is for decorrelation and
//!   reproducible fixtures only: spreading per-CPU work-stealing scans, seeded
//!   test streams. Never a key, a nonce, or an identifier an adversary
//!   benefits from enumerating.
//!
//! # The cryptographic core is composed, not hand-rolled
//!
//! No cryptographic primitive is written here. The DRBG is the standard
//! HMAC-DRBG construction over `lib/crypto`'s audited HMAC-SHA256, exactly as
//! `lib/crypto`'s `kdf` layers HKDF-Expand over the same PRF, and [`FastRng`]
//! is Bernstein's fast-key-erasure construction over `lib/crypto`'s audited
//! `ChaCha12`. The only first-party algorithm is xoshiro256++, which is an
//! ordinary PRNG rather than a security primitive.
//!
//! # Hardware RNG: entropy input, never output
//!
//! A platform hardware source ([`hardware::HardwareRng`], supplied by
//! `kernel/arch/<target>` or a `drivers/*` crate — never probed here, so the
//! crate stays architecture-neutral) is an *additional entropy input* and
//! nothing else: wrap it in [`hardware::HardwareEntropy`] and XOR-mix it with
//! the other platform sources through [`CombinedSource`] before it feeds
//! [`CsRng`]. It is one independent input among several, never the sole
//! trusted source, and its bytes never reach a caller unconditioned. A caller
//! wanting speed takes [`FastRng`], which is both conditioned and cheaper than
//! a hardware instruction.
//!
//! # Worked wiring
//!
//! ```
//! use tairix_rng::{CombinedSource, CsRng, EntropyError, EntropySource, JitterSource};
//! use tairix_rng::hardware::{HardwareEntropy, HardwareRng};
//! use tairix_rng::{FastRng, RandU64};
//!
//! # struct Rdrand;
//! # impl HardwareRng for Rdrand {
//! #     fn try_fill(&self, out: &mut [u8]) -> Result<(), EntropyError> {
//! #         for (i, b) in out.iter_mut().enumerate() { *b = i as u8 ^ 0xA5; }
//! #         Ok(())
//! #     }
//! # }
//! # fn wire() -> Result<(), EntropyError> {
//! // Mix a hardware source with timing jitter into one pool…
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
//! // …seed the cryptographic generator from the combined pool…
//! let mut csrng = CsRng::new(pool)?;
//! let mut volume_key = [0u8; 32];
//! csrng.try_fill_bytes(&mut volume_key)?;
//!
//! // …and key the fast generator from it for bulk unpredictable bytes.
//! let mut fast: FastRng = csrng.fork_fast()?;
//! let _coin = fast.next_u64() & 1;
//! # Ok(())
//! # }
//! ```

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

pub mod bootseed;
pub mod csprng;
pub mod drbg;
pub mod entropy;
pub mod fast;
pub mod hardware;
pub mod interrupt;
pub mod jitter;
pub mod noncrypto;
pub mod rand;
pub mod reserve;

pub use bootseed::{BootSeedSource, MAX_BOOT_SEED_LEN};
pub use csprng::{CsRng, DEFAULT_RESEED_INTERVAL};
pub use drbg::{DrbgError, HmacDrbg, DRBG_OUTLEN, RESEED_INTERVAL};
pub use entropy::{CombinedSource, EntropyError, EntropySource, MixedPair};
pub use fast::{FastRng, FAST_BUFFER_BYTES, FAST_REFILL_BYTES, PERTURB_INTERVAL_BYTES};
pub use hardware::{HardwareEntropy, HardwareRng};
pub use interrupt::{InterruptEntropyPool, InterruptPoolSource};
pub use jitter::{JitterSource, TimeSource};
pub use noncrypto::NonCryptoRng;
pub use rand::RandU64;
// The 256-bit key `FastRng` takes, re-exported from `lib/crypto` so a
// consumer of this crate names one crate rather than two. Not a second
// definition: the type is the cipher's own.
pub use reserve::{OutputReserve, ReserveError, DEFAULT_RESERVE_BYTES};
pub use tairix_crypto::{StreamKey, STREAM_KEY_LEN};
