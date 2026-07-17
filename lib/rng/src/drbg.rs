//! HMAC-SHA256 deterministic random bit generator (NIST SP 800-90Ar1 §10.1.2).
//!
//! This is the cryptographic engine underneath [`crate::CsRng`]. It is the
//! standard HMAC-DRBG construction — Instantiate, Update, Reseed, Generate —
//! built **entirely** over the audited HMAC-SHA256 primitive in `lib/crypto`,
//! exactly as `lib/crypto`'s `kdf` layers single-block HKDF-Expand over the
//! same PRF. Nothing here is a hand-rolled cryptographic primitive: HMAC *is* the conditioner, so the DRBG needs no
//! derivation function of its own.
//!
//! # Why HMAC-DRBG
//!
//! HMAC-DRBG is the most conservative of the SP 800-90A generators: its
//! security reduces to HMAC being a PRF (Bellare), it has no awkward
//! block-cipher key/counter edge cases, and it is the construction
//! [`rustos_crypto`] can already serve with zero new audit surface. It is the
//! right "best in class" core for `ARXFS` volume keys, the encrypted-swap key, and KASLR/ASLR seeds.
//!
//! # Backtracking and prediction resistance
//!
//! Every [`HmacDrbg::generate`] call ends with an Update over the working
//! state, so the `Key`/`V` that produced a block cannot be recovered from the
//! post-call state: outputs are **backtracking-resistant**. *Prediction*
//! resistance (recovery after a state compromise) requires fresh entropy and
//! is provided one layer up by [`crate::CsRng`] reseeding; this type only
//! reseeds when asked.

use rustos_crypto::{hmac_sha256, hmac_sha256_parts, MacKey, MacTag};
use zeroize::Zeroize;

/// Length, in bytes, of the DRBG's `Key` and `V` working-state words and of
/// each generated output block (one HMAC-SHA256 output).
pub const DRBG_OUTLEN: usize = 32;

/// Maximum number of [`HmacDrbg::generate`] calls between reseeds, per NIST
/// SP 800-90Ar1 Table 2 for HMAC-DRBG (`2^48`). Reaching it makes
/// [`HmacDrbg::generate`] fail closed with [`DrbgError::ReseedRequired`]
/// rather than silently weakening. At realistic call
/// rates this bound is never reached in a single boot; [`crate::CsRng`]
/// reseeds far more often.
pub const RESEED_INTERVAL: u64 = 1 << 48;

/// The reason a DRBG operation could not complete.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum DrbgError {
    /// The reseed interval ([`RESEED_INTERVAL`]) has been reached: the DRBG
    /// must be reseeded with fresh entropy before it will generate again.
    /// No output is produced and the caller's buffer is left untouched.
    ReseedRequired,
}

/// An HMAC-SHA256 DRBG instance (NIST SP 800-90Ar1 §10.1.2).
///
/// Holds the `Key`/`V` working state and the reseed counter. The state is
/// key material and is zeroed on drop.
pub struct HmacDrbg {
    key: MacKey,
    v: MacTag,
    reseed_counter: u64,
}

impl HmacDrbg {
    /// Instantiate a DRBG from `entropy`, a `nonce`, and an optional
    /// `personalization` string (SP 800-90Ar1 §10.1.2.3).
    ///
    /// `entropy` must carry at least the 256-bit security strength of the
    /// generator and `nonce` at least half that; supplying them is the
    /// caller's (and [`crate::CsRng`]'s) responsibility — the DRBG cannot
    /// manufacture entropy it was not given. `personalization` may be empty.
    #[must_use]
    pub fn new(entropy: &[u8], nonce: &[u8], personalization: &[u8]) -> Self {
        // SP 800-90Ar1 §10.1.2.3: Key = 0x00..., V = 0x01..., then Update
        // over the seed material entropy ‖ nonce ‖ personalization.
        let mut drbg = Self {
            key: [0x00; DRBG_OUTLEN],
            v: [0x01; DRBG_OUTLEN],
            reseed_counter: 0,
        };
        drbg.update(&[entropy, nonce, personalization]);
        drbg.reseed_counter = 1;
        drbg
    }

    /// Reseed the DRBG with fresh `entropy` and optional `additional` input
    /// (SP 800-90Ar1 §10.1.2.4), resetting the reseed counter.
    ///
    /// This is what restores prediction resistance after a (hypothetical)
    /// state compromise, so `entropy` must be fresh, full-strength entropy.
    pub fn reseed(&mut self, entropy: &[u8], additional: &[u8]) {
        self.update(&[entropy, additional]);
        self.reseed_counter = 1;
    }

    /// Generate `out.len()` pseudorandom bytes into `out`, optionally mixing
    /// in `additional` input (SP 800-90Ar1 §10.1.2.5).
    ///
    /// Pass an empty slice for `additional` when there is none. On success
    /// `out` is fully overwritten.
    ///
    /// # Errors
    ///
    /// Returns [`DrbgError::ReseedRequired`] if [`RESEED_INTERVAL`] generate
    /// calls have elapsed since the last (re)seed. The DRBG fails closed:
    /// `out` is not modified and the caller must [`reseed`](Self::reseed)
    /// before retrying.
    pub fn generate(&mut self, out: &mut [u8], additional: &[u8]) -> Result<(), DrbgError> {
        if self.reseed_counter > RESEED_INTERVAL {
            return Err(DrbgError::ReseedRequired);
        }
        if !additional.is_empty() {
            self.update(&[additional]);
        }
        let mut filled = 0;
        while filled < out.len() {
            // V = HMAC(Key, V); the new V is the next output block.
            self.v = hmac_sha256(&self.key, &self.v);
            let take = core::cmp::min(DRBG_OUTLEN, out.len() - filled);
            out[filled..filled + take].copy_from_slice(&self.v[..take]);
            filled += take;
        }
        // Final Update folds `additional` (or null) back in, making the
        // post-call state independent of the bytes just returned.
        self.update(&[additional]);
        self.reseed_counter += 1;
        Ok(())
    }

    /// The number of [`generate`](Self::generate) calls since the last
    /// (re)seed. Starts at 1 after instantiation/reseed.
    #[must_use]
    pub fn reseed_counter(&self) -> u64 {
        self.reseed_counter
    }

    /// `true` once [`RESEED_INTERVAL`] is reached and [`generate`](Self::generate)
    /// will fail until [`reseed`](Self::reseed) is called.
    #[must_use]
    pub fn needs_reseed(&self) -> bool {
        self.reseed_counter > RESEED_INTERVAL
    }

    /// The HMAC-DRBG Update function (SP 800-90Ar1 §10.1.2.2).
    ///
    /// `provided` is the (possibly multi-part) provided-data string; an
    /// all-empty `provided` is the "null" case, which performs only the
    /// first `Key`/`V` refresh, per the standard.
    fn update(&mut self, provided: &[&[u8]]) {
        self.update_round(0x00, provided);
        if provided.iter().all(|p| p.is_empty()) {
            return;
        }
        self.update_round(0x01, provided);
    }

    /// One Update round: `Key = HMAC(Key, V ‖ [sep] ‖ provided)` then
    /// `V = HMAC(Key, V)`.
    fn update_round(&mut self, sep: u8, provided: &[&[u8]]) {
        // Internal invariant: every caller passes at most three provided
        // parts (entropy ‖ nonce ‖ personalization is the widest), so the
        // fixed scratch of `V`, the separator, and the parts never overflows.
        const MAX_PARTS: usize = 8;
        debug_assert!(provided.len() + 2 <= MAX_PARTS);
        let sep = [sep];
        let mut parts: [&[u8]; MAX_PARTS] = [&[]; MAX_PARTS];
        parts[0] = &self.v;
        parts[1] = &sep;
        for (slot, part) in parts[2..].iter_mut().zip(provided.iter()) {
            *slot = part;
        }
        let count = 2 + provided.len();
        self.key = hmac_sha256_parts(&self.key, &parts[..count]);
        self.v = hmac_sha256(&self.key, &self.v);
    }
}

impl Drop for HmacDrbg {
    fn drop(&mut self) {
        // SAFETY-INVARIANT: `Zeroize::zeroize` uses volatile writes the
        // compiler may not elide, so the working state — from which all
        // future output is derived — is gone once the DRBG is dropped.
        self.key.zeroize();
        self.v.zeroize();
    }
}

impl core::fmt::Debug for HmacDrbg {
    /// Never reveals the working state.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("HmacDrbg")
            .field("key", &"<redacted>")
            .field("v", &"<redacted>")
            .field("reseed_counter", &self.reseed_counter)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    extern crate alloc;
    use alloc::vec::Vec;

    /// Decode a hex string into bytes; test-only fixture helper.
    fn unhex(s: &str) -> Vec<u8> {
        assert!(s.len() % 2 == 0, "hex string must have even length");
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("valid hex"))
            .collect()
    }

    /// NIST SP 800-90Ar1 CAVP HMAC-DRBG, `[SHA-256]`, prediction resistance
    /// false, reseed enabled, 256-bit entropy, 128-bit nonce, no
    /// personalization, 256-bit additional input, 1024-bit returned bits,
    /// COUNT = 0. Exercises Instantiate, Reseed, and two Generate calls with
    /// additional input — every public method on one authoritative vector.
    #[test]
    fn nist_cavp_reseed_with_additional_input_vector() {
        let entropy = unhex("05ac9fc4c62a02e3f90840da5616218c6de5743d66b8e0fbf833759c5928b53d");
        let nonce = unhex("2b89a17904922ed8f017a63044848545");
        let entropy_reseed =
            unhex("2791126b8b52ee1fd9392a0a13e0083bed4186dc649b739607ac70ec8dcecf9b");
        let additional_reseed =
            unhex("43bac13bae715092cf7eb280a2e10a962faf7233c41412f69bc74a35a584e54c");
        let additional1 = unhex("3f2fed4b68d506ecefa21f3f5bb907beb0f17dbc30f6ffbba5e5861408c53a1e");
        let additional2 = unhex("529030df50f410985fde068df82b935ec23d839cb4b269414c0ede6cffea5b68");
        let expected = unhex(concat!(
            "02ddff5173da2fcffa10215b030d660d61179e61ecc22609b1151a75f1cbcbb4",
            "363c3a89299b4b63aca5e581e73c860491010aa35de3337cc6c09ebec8c91a62",
            "87586f3a74d9694b462d2720ea2e11bbd02af33adefb4a16e6b370fa0effd57d",
            "607547bdcfbb7831f54de7073ad2a7da987a0016a82fa958779a168674b56524",
        ));

        let mut drbg = HmacDrbg::new(&entropy, &nonce, &[]);
        drbg.reseed(&entropy_reseed, &additional_reseed);

        let mut out = [0u8; 128];
        drbg.generate(&mut out, &additional1)
            .expect("first generate");
        drbg.generate(&mut out, &additional2)
            .expect("second generate");
        assert_eq!(&out[..], &expected[..]);
    }

    #[test]
    fn same_seed_is_deterministic_and_different_seed_diverges() {
        let mut a = HmacDrbg::new(b"entropy-bytes-aaaaaaaaaaaaaaaaaaaa", b"nonce-aaaa", &[]);
        let mut b = HmacDrbg::new(b"entropy-bytes-aaaaaaaaaaaaaaaaaaaa", b"nonce-aaaa", &[]);
        let mut c = HmacDrbg::new(b"entropy-bytes-bbbbbbbbbbbbbbbbbbbb", b"nonce-aaaa", &[]);
        let (mut oa, mut ob, mut oc) = ([0u8; 80], [0u8; 80], [0u8; 80]);
        a.generate(&mut oa, &[]).unwrap();
        b.generate(&mut ob, &[]).unwrap();
        c.generate(&mut oc, &[]).unwrap();
        assert_eq!(oa, ob, "identical seeds must give identical streams");
        assert_ne!(oa, oc, "different seeds must diverge");
    }

    #[test]
    fn personalization_changes_the_stream() {
        let mut a = HmacDrbg::new(b"shared-entropy-xxxxxxxxxxxxxxxxxx", b"nonce", b"alpha");
        let mut b = HmacDrbg::new(b"shared-entropy-xxxxxxxxxxxxxxxxxx", b"nonce", b"beta");
        let (mut oa, mut ob) = ([0u8; 48], [0u8; 48]);
        a.generate(&mut oa, &[]).unwrap();
        b.generate(&mut ob, &[]).unwrap();
        assert_ne!(oa, ob);
    }

    #[test]
    fn reseed_changes_the_stream() {
        let mut a = HmacDrbg::new(b"entropy-aaaaaaaaaaaaaaaaaaaaaaaa", b"nonce", &[]);
        let mut b = HmacDrbg::new(b"entropy-aaaaaaaaaaaaaaaaaaaaaaaa", b"nonce", &[]);
        b.reseed(b"fresh-entropy-bbbbbbbbbbbbbbbbbb", &[]);
        let (mut oa, mut ob) = ([0u8; 48], [0u8; 48]);
        a.generate(&mut oa, &[]).unwrap();
        b.generate(&mut ob, &[]).unwrap();
        assert_ne!(oa, ob);
    }

    #[test]
    fn partial_block_lengths_are_a_prefix_of_full_blocks() {
        // Requesting fewer bytes returns the leftmost bytes of the same
        // stream a full request would (the DRBG truncates the last block).
        let mut a = HmacDrbg::new(b"entropy-prefixprefixprefixprefix", b"nonce", &[]);
        let mut b = HmacDrbg::new(b"entropy-prefixprefixprefixprefix", b"nonce", &[]);
        let mut full = [0u8; 32];
        let mut partial = [0u8; 20];
        a.generate(&mut full, &[]).unwrap();
        b.generate(&mut partial, &[]).unwrap();
        assert_eq!(&full[..20], &partial[..]);
    }

    #[test]
    fn reseed_required_fails_closed() {
        let mut drbg = HmacDrbg::new(b"entropy-reseed-required-xxxxxxxx", b"nonce", &[]);
        drbg.reseed_counter = RESEED_INTERVAL + 1;
        assert!(drbg.needs_reseed());
        let mut out = [0xAB; 16];
        assert_eq!(drbg.generate(&mut out, &[]), Err(DrbgError::ReseedRequired));
        assert_eq!(out, [0xAB; 16], "buffer must be untouched on failure");
        drbg.reseed(b"fresh-entropy-reseed-requiredxxx", &[]);
        assert!(!drbg.needs_reseed());
        drbg.generate(&mut out, &[]).expect("generate after reseed");
    }

    #[test]
    fn debug_does_not_leak_state() {
        extern crate alloc;
        use alloc::format;
        let drbg = HmacDrbg::new(b"secret-entropy-do-not-printxxxx", b"nonce", &[]);
        let s = format!("{drbg:?}");
        assert!(s.contains("redacted"));
        assert!(!s.contains("secret"));
    }
}
